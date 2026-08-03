//! Admin CRUD for the overlay L3 ACL.
//!
//! Mirrors the tunnel-policy admin surface (`routes/tunnel.rs`) but drives a
//! different enforcement point: these rows shape the NETMAP each node receives
//! — which peers it sees and which of their approved routes it may install —
//! rather than gating individual tunnel flows.
//!
//! The tenant-wide posture lives on `OverlayNetwork.acl_mode`
//! (`off` → `warn` → `enforce`) and is edited through
//! `PUT /overlay-acl/mode`. It defaults to `off`, so adding policies is
//! inert until an admin deliberately turns enforcement on — the feature can
//! never black-hole a live mesh by merely being deployed.
//!
//! Everything here requires `MANAGE_AGENTS`: overlay traffic bypasses the
//! tunnel ACL entirely, so an overlay grant is at least as powerful as an
//! agent-management action.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use bson::oid::ObjectId;
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::models::{
    OverlayAclMode, OverlayPolicy, OverlayRule, OverlaySelector, OverlayTarget,
};
use roomler_ai_services::dao::base::PaginationParams;
use serde::{Deserialize, Serialize};

use crate::{
    error::ApiError, extractors::auth::AuthUser, routes::remote_control::require_permission,
    state::AppState,
};

#[derive(Debug, Serialize)]
pub struct OverlayPolicyResponse {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub enabled: bool,
    pub sources: Vec<OverlaySelector>,
    pub via: Vec<OverlayTarget>,
    pub destinations: Vec<OverlayRule>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<OverlayPolicy> for OverlayPolicyResponse {
    fn from(p: OverlayPolicy) -> Self {
        Self {
            id: p.id.map(|i| i.to_hex()).unwrap_or_default(),
            tenant_id: p.tenant_id.to_hex(),
            name: p.name,
            enabled: p.enabled,
            sources: p.sources,
            via: p.via,
            destinations: p.destinations,
            created_at: p.created_at.try_to_rfc3339_string().unwrap_or_default(),
            updated_at: p.updated_at.try_to_rfc3339_string().unwrap_or_default(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct OverlayPolicyListResponse {
    pub items: Vec<OverlayPolicyResponse>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    /// The tenant's current posture, so the UI can render the mode selector
    /// and the "rules are not being enforced yet" banner from one round-trip.
    pub mode: OverlayAclMode,
}

#[derive(Debug, Deserialize)]
pub struct UpsertOverlayPolicyRequest {
    pub name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub sources: Vec<OverlaySelector>,
    pub via: Vec<OverlayTarget>,
    pub destinations: Vec<OverlayRule>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct SetAclModeRequest {
    pub mode: OverlayAclMode,
}

#[derive(Debug, Serialize)]
pub struct AclModeResponse {
    pub mode: OverlayAclMode,
}

/// Reject rules that can never match, so an admin never authors a silent
/// no-op. Every CIDR must parse, and the port range must be sane.
fn validate(body: &UpsertOverlayPolicyRequest) -> Result<(), ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name is required".into()));
    }
    if body.sources.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one source is required".into(),
        ));
    }
    if body.via.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one via node is required".into(),
        ));
    }
    if body.destinations.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one destination is required".into(),
        ));
    }
    for d in &body.destinations {
        if d.cidr.trim().parse::<ipnet::IpNet>().is_err() {
            return Err(ApiError::BadRequest(format!(
                "destination '{}' is not a valid CIDR (a bare address needs a prefix, e.g. 10.0.0.5/32)",
                d.cidr
            )));
        }
        if d.port_range.low == 0 || d.port_range.high < d.port_range.low {
            return Err(ApiError::BadRequest(format!(
                "destination '{}' has an invalid port range",
                d.cidr
            )));
        }
    }
    Ok(())
}

async fn gate(state: &AppState, tenant_id: &str, auth: &AuthUser) -> Result<ObjectId, ApiError> {
    let tid = ObjectId::parse_str(tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    require_permission(
        state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;
    Ok(tid)
}

pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<OverlayPolicyListResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    let page = state.overlay_policies.list_for_tenant(tid, &params).await?;
    let mode = state.overlay_networks.get_or_create(tid).await?.acl_mode;
    Ok(Json(OverlayPolicyListResponse {
        items: page.items.into_iter().map(Into::into).collect(),
        total: page.total,
        page: page.page,
        per_page: page.per_page,
        mode,
    }))
}

pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<UpsertOverlayPolicyRequest>,
) -> Result<Json<OverlayPolicyResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    validate(&body)?;
    let created = state
        .overlay_policies
        .create(
            tid,
            body.name.trim().to_string(),
            body.enabled,
            body.sources,
            body.via,
            body.destinations,
        )
        .await?;
    refan_tenant(&state, tid).await;
    Ok(Json(created.into()))
}

pub async fn get(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, policy_id)): Path<(String, String)>,
) -> Result<Json<OverlayPolicyResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    let pid = ObjectId::parse_str(&policy_id)
        .map_err(|_| ApiError::BadRequest("Invalid policy_id".to_string()))?;
    Ok(Json(
        state
            .overlay_policies
            .find_in_tenant(tid, pid)
            .await?
            .into(),
    ))
}

pub async fn update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, policy_id)): Path<(String, String)>,
    Json(body): Json<UpsertOverlayPolicyRequest>,
) -> Result<Json<OverlayPolicyResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    let pid = ObjectId::parse_str(&policy_id)
        .map_err(|_| ApiError::BadRequest("Invalid policy_id".to_string()))?;
    validate(&body)?;
    state
        .overlay_policies
        .update(
            tid,
            pid,
            Some(body.name.trim().to_string()),
            Some(body.enabled),
            Some(body.sources),
            Some(body.via),
            Some(body.destinations),
        )
        .await?;
    refan_tenant(&state, tid).await;
    Ok(Json(
        state
            .overlay_policies
            .find_in_tenant(tid, pid)
            .await?
            .into(),
    ))
}

pub async fn delete(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, policy_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    let pid = ObjectId::parse_str(&policy_id)
        .map_err(|_| ApiError::BadRequest("Invalid policy_id".to_string()))?;
    let removed = state.overlay_policies.soft_delete(tid, pid).await?;
    if removed {
        refan_tenant(&state, tid).await;
    }
    Ok(Json(serde_json::json!({ "deleted": removed })))
}

pub async fn get_mode(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<AclModeResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    let mode = state.overlay_networks.get_or_create(tid).await?.acl_mode;
    Ok(Json(AclModeResponse { mode }))
}

pub async fn set_mode(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<SetAclModeRequest>,
) -> Result<Json<AclModeResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    state.overlay_networks.set_acl_mode(tid, body.mode).await?;
    refan_tenant(&state, tid).await;
    Ok(Json(AclModeResponse { mode: body.mode }))
}

/// Re-fan every node in the tenant so a policy edit takes effect immediately,
/// and refresh the DERP relay allow table with it.
///
/// `refan_node` is single-node; a policy change can alter what EVERY node
/// sees of every other, so the whole network has to be re-evaluated. Nodes are
/// re-fanned one at a time (each fan-out is itself per-recipient), which is
/// fine at fleet scale and keeps the write path simple — netmap edits are rare.
///
/// The DERP rebuild is NOT optional here: reshaping netmaps alone would leave a
/// denied pair still able to relay by pubkey, which is the bypass the gate
/// exists to close. It runs AFTER the fan so the two never disagree in the
/// permissive direction — a peer loses its netmap entry before it loses relay.
async fn refan_tenant(state: &AppState, tenant_id: ObjectId) {
    let Ok(network) = state.overlay_networks.get_or_create(tenant_id).await else {
        return;
    };
    let Some(network_id) = network.id else { return };
    let Ok(nodes) = state
        .overlay_nodes
        .list_active_in_network(tenant_id, network_id)
        .await
    else {
        return;
    };
    for n in &nodes {
        crate::ws::overlay::refan_node(state, n).await;
    }
    crate::ws::derp_acl::rebuild(state, tenant_id, network_id).await;
}
