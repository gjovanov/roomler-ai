//! Admin CRUD for overlay subnet-route approval (Phase 1 subnet router).
//!
//! A node advertises subnet CIDRs on join (`OverlayNode.advertised_routes`);
//! NOTHING is distributed to peers until an admin approves a subset here
//! (`approved_routes`). Mirrors the tunnel-policy admin surface. On change we
//! re-fan the node's netmap entry so peers pick up the routes immediately
//! instead of waiting for the next join.

use axum::{
    Json,
    extract::{Path, State},
};
use bson::oid::ObjectId;
use roomler_ai_remote_control::models::{AgentStatus, NodeRef, OverlayNode};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

#[derive(Debug, Serialize)]
pub struct OverlayNodeResponse {
    pub id: String,
    pub name: String,
    pub overlay_ip: String,
    /// `"agent"` | `"tunnel_client"`.
    pub kind: &'static str,
    /// CIDRs the node claims it can route (from its config).
    pub advertised_routes: Vec<String>,
    /// Admin-approved subset actually distributed to peers.
    pub approved_routes: Vec<String>,
    pub online: bool,
    pub last_seen_at: String,
}

impl From<OverlayNode> for OverlayNodeResponse {
    fn from(n: OverlayNode) -> Self {
        Self {
            id: n.id.map(|i| i.to_hex()).unwrap_or_default(),
            name: n.name,
            overlay_ip: n.overlay_ip,
            kind: match n.node_ref {
                NodeRef::Agent { .. } => "agent",
                NodeRef::TunnelClient { .. } => "tunnel_client",
            },
            online: matches!(n.status, AgentStatus::Online),
            advertised_routes: n.advertised_routes,
            approved_routes: n.approved_routes,
            last_seen_at: n.last_seen_at.try_to_rfc3339_string().unwrap_or_default(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SetApprovedRoutesRequest {
    pub approved_routes: Vec<String>,
}

/// GET /api/tenant/{tenant_id}/overlay-node — list the tenant's overlay nodes
/// with their advertised + approved subnet routes (the subnet-router admin view).
pub async fn list_overlay_nodes(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }
    let network = state.overlay_networks.get_or_create(tid).await?;
    let Some(network_id) = network.id else {
        return Ok(Json(serde_json::json!({ "items": [] })));
    };
    let nodes = state
        .overlay_nodes
        .list_active_in_network(tid, network_id)
        .await?;
    let items: Vec<OverlayNodeResponse> = nodes.into_iter().map(Into::into).collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

/// PUT /api/tenant/{tenant_id}/overlay-node/{node_id}/approved-routes — set the
/// admin-approved subset of a node's advertised routes. Only routes the node
/// actually advertised may be approved; the change is re-fanned to peers.
pub async fn set_approved_routes(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, node_id)): Path<(String, String)>,
    Json(body): Json<SetApprovedRoutesRequest>,
) -> Result<Json<OverlayNodeResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let nid = ObjectId::parse_str(&node_id)
        .map_err(|_| ApiError::BadRequest("Invalid node_id".to_string()))?;
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }
    let network = state.overlay_networks.get_or_create(tid).await?;
    let network_id = network
        .id
        .ok_or_else(|| ApiError::Internal("overlay network missing _id".into()))?;

    // Scope the lookup to the tenant's network — tenant-safe by construction.
    let nodes = state
        .overlay_nodes
        .list_active_in_network(tid, network_id)
        .await?;
    let node = nodes
        .into_iter()
        .find(|n| n.id == Some(nid))
        .ok_or_else(|| ApiError::NotFound("overlay node not found".into()))?;

    // Only routes the node advertised may be approved (dedup, preserve order).
    let mut approved: Vec<String> = Vec::new();
    for r in &body.approved_routes {
        if !node.advertised_routes.contains(r) {
            return Err(ApiError::BadRequest(format!(
                "route {r} is not advertised by this node"
            )));
        }
        if !approved.contains(r) {
            approved.push(r.clone());
        }
    }

    let updated = state
        .overlay_nodes
        .set_approved_routes(nid, &approved)
        .await?;
    // Push the change to peers now, not on their next join.
    crate::ws::overlay::refan_node(&state, &updated).await;
    Ok(Json(updated.into()))
}
