use axum::{Json, extract::State};
use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

#[derive(Debug, Deserialize)]
pub struct CreateTenantRequest {
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Serialize)]
pub struct TenantResponse {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub owner_id: String,
    pub plan: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListTenantsQuery {
    /// Include archived organizations. Off by default — an archived org is
    /// meant to be out of the way, and the switcher is the main caller.
    #[serde(default)]
    pub include_archived: bool,
}

pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    axum::extract::Query(q): axum::extract::Query<ListTenantsQuery>,
) -> Result<Json<Vec<TenantResponse>>, ApiError> {
    let tenants = state.tenants.find_user_tenants(auth.user_id).await?;

    let response: Vec<TenantResponse> = tenants
        .into_iter()
        .filter(|t| q.include_archived || !t.is_archived)
        .map(|t| TenantResponse {
            id: t.id.unwrap().to_hex(),
            name: t.name,
            slug: t.slug,
            owner_id: t.owner_id.to_hex(),
            // Plan enum is `#[serde(rename_all = "snake_case")]` so it
            // serializes as "free"/"pro"/"business"/"enterprise" via
            // serde. The frontend's plan cards (and Stripe /plans
            // response) use lowercase ids — Debug-formatting gives
            // "Free"/"Pro" which doesn't match, breaking the
            // currentPlan comparison.
            plan: format!("{:?}", t.plan).to_lowercase(),
        })
        .collect();

    Ok(Json(response))
}

pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateTenantRequest>,
) -> Result<Json<TenantResponse>, ApiError> {
    let tenant = state
        .tenants
        .create(body.name, body.slug, auth.user_id)
        .await?;

    Ok(Json(TenantResponse {
        id: tenant.id.unwrap().to_hex(),
        name: tenant.name,
        slug: tenant.slug,
        owner_id: tenant.owner_id.to_hex(),
        plan: format!("{:?}", tenant.plan).to_lowercase(),
    }))
}

pub async fn get(
    State(state): State<AppState>,
    auth: AuthUser,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
) -> Result<Json<TenantResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    // Verify membership
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let tenant = state.tenants.base.find_by_id(tid).await?;

    Ok(Json(TenantResponse {
        id: tenant.id.unwrap().to_hex(),
        name: tenant.name,
        slug: tenant.slug,
        owner_id: tenant.owner_id.to_hex(),
        plan: format!("{:?}", tenant.plan).to_lowercase(),
    }))
}

/// What archiving an organization actually did.
#[derive(Debug, Serialize)]
pub struct ArchiveResponse {
    pub id: String,
    pub name: String,
    pub is_archived: bool,
    /// Devices whose enrollment was revoked (archive only).
    pub devices_revoked: u64,
    /// Overlay nodes released back to the address pool (archive only).
    pub nodes_released: u64,
    /// The org's address block, quarantined so its range is never handed to
    /// a different tenant while a missed device still believes it holds one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_quarantined: Option<String>,
}

/// POST /api/tenant/{tenant_id}/archive — retire an organization.
///
/// **Archiving means the org stops acting and keeps everything it knows.**
/// Five effects, and nothing else:
///
/// 1. hidden from the org switcher (`GET /api/tenant`);
/// 2. no new device enrollments;
/// 3. no new remote-control sessions;
/// 4. its devices leave its mesh — every agent's enrollment is revoked,
///    every overlay node is released back to the pool, and its address
///    block is quarantined;
/// 5. everything else — rooms, messages, files, members, roles, audit —
///    is retained untouched, so `unarchive` restores a working org.
///
/// Effect 4 is the one that makes a throwaway org genuinely retirable, and
/// it is why this is owner-only: revoking every device's token is not
/// something a MANAGE_TENANT delegate should be able to do by surprise.
///
/// Restoring does NOT restore the address block (quarantined blocks are
/// never re-issued — a device that missed the change may still believe it
/// holds an address there) or the device enrollments; the org gets a fresh
/// block on its next carve, and devices re-enroll. That asymmetry is
/// deliberate: archive is cheap to undo for people, and honest about the
/// things that cannot be safely un-revoked.
pub async fn archive(
    State(state): State<AppState>,
    auth: AuthUser,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
) -> Result<Json<ArchiveResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let tenant = state.tenants.base.find_by_id(tid).await?;
    if tenant.owner_id != auth.user_id {
        return Err(ApiError::Forbidden(
            "Only the organization's owner can archive it".to_string(),
        ));
    }
    if tenant.is_archived {
        return Err(ApiError::BadRequest(
            "This organization is already archived".to_string(),
        ));
    }

    // Flip the flag FIRST: from here on nothing new can enroll or start a
    // session, so the teardown below is not racing new arrivals.
    let tenant = state.tenants.set_archived(tid, true).await?;

    // Devices: revoke the enrollment. The agent's long-lived JWT stays
    // cryptographically valid for its full year, so the row is the only
    // thing that can withdraw it — `soft_delete` is what every auth path
    // already checks.
    let agents = state
        .agents
        .list_all_active_for_tenant(tid)
        .await
        .unwrap_or_default();
    let mut devices_revoked = 0u64;
    for a in &agents {
        if let Some(id) = a.id
            && state.agents.soft_delete(tid, id).await.unwrap_or(false)
        {
            devices_revoked += 1;
        }
    }

    // Mesh: release every node, which pools its address and tells the peers.
    let network = state
        .overlay_networks
        .find_for_tenant(tid)
        .await
        .ok()
        .flatten();
    let mut nodes_released = 0u64;
    if let Some(net) = &network
        && let Some(net_id) = net.id
    {
        let nodes = state
            .overlay_nodes
            .list_active_in_network(tid, net_id)
            .await
            .unwrap_or_default();
        for n in &nodes {
            if crate::ws::overlay::release_overlay_node(&state, n, "tenant archived")
                .await
                .is_some()
            {
                nodes_released += 1;
            }
        }
    }

    // Block: quarantine, never re-issue. A device that never saw the
    // archive still thinks it owns an address in that range.
    let mut block_quarantined = None;
    if let Some(net_id) = network.as_ref().and_then(|n| n.id)
        && let Ok(Some(block)) = state
            .overlay_networks
            .blocks()
            .find_assigned_for_network(net_id)
            .await
        && let Some(bid) = block.id
        && state
            .overlay_networks
            .blocks()
            .quarantine(bid, "tenant archived")
            .await
            .unwrap_or(false)
    {
        block_quarantined = Some(block.cidr);
    }

    tracing::info!(
        owner = %auth.user_id, tenant = %tid, name = %tenant.name,
        devices_revoked, nodes_released, block = ?block_quarantined,
        "organization archived"
    );

    Ok(Json(ArchiveResponse {
        id: tid.to_hex(),
        name: tenant.name,
        is_archived: true,
        devices_revoked,
        nodes_released,
        block_quarantined,
    }))
}

/// POST /api/tenant/{tenant_id}/unarchive — bring a retired organization
/// back. Restores visibility, enrollment and sessions; does NOT resurrect
/// revoked device enrollments or the quarantined block (see [`archive`]).
pub async fn unarchive(
    State(state): State<AppState>,
    auth: AuthUser,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
) -> Result<Json<ArchiveResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let tenant = state.tenants.base.find_by_id(tid).await?;
    if tenant.owner_id != auth.user_id {
        return Err(ApiError::Forbidden(
            "Only the organization's owner can restore it".to_string(),
        ));
    }
    let tenant = state.tenants.set_archived(tid, false).await?;
    tracing::info!(owner = %auth.user_id, tenant = %tid, "organization restored");
    Ok(Json(ArchiveResponse {
        id: tid.to_hex(),
        name: tenant.name,
        is_archived: false,
        devices_revoked: 0,
        nodes_released: 0,
        block_quarantined: None,
    }))
}
