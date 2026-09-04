// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-19 peer relays — the ADMIN surface (`docs/fr/FR-19-peer-relays.md`):
//! the org switch (gate 1), per-device approval (gate 3) and the audit reader.
//! The mint itself — gate 2 and the session push — lives in `ws::overlay`
//! (P3c) and writes into the same `peer_relay_audit`.
//!
//! ## Permissions — and why there is no `RELAY_DEVICE` bit
//!
//! The spec assumed a dedicated bit. There is none to take: the UI mirror
//! checks masks with JavaScript's signed 32-bit bitwise ops, and bit 30 is the
//! ceiling by design (`role.rs`, #888). Approval therefore rides
//! `MANAGE_AGENTS` **and** `EXEC_DEVICE`, which is coherent rather than a
//! stand-in: an `EXEC_DEVICE` holder can already run
//! `roomler config set relay_server_enabled true` on any exec-enabled device
//! as root, so the coupling grants nothing new. What it cannot express —
//! relay approvers who may NOT run root commands — waits on the BigInt
//! migration. The audit reads behind `VIEW_EXEC_AUDIT` for the same reason.
//!
//! The org switch is `MANAGE_TENANT`, like `exec-settings`: it decides whether
//! the org's devices carry each other's traffic at all — an org-owner
//! decision, not a fleet-admin one. Reading the settings is `MANAGE_AGENTS`,
//! like every other device-fleet view.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::models::{
    Agent, AgentStatus, PeerRelayAuditAction, PeerRelayAuditEvent, PeerRelayDenyReason,
    PeerRelayMode, PeerRelayPolicy, RpcCap,
};
use roomler_ai_services::dao::base::PaginationParams;
use serde::{Deserialize, Serialize};
use tracing::warn;

use roomler_core::guards::require_permission;
use roomler_core::{ApiError, extractors::auth::AuthUser};

use crate::NetworkState;

/// The whole approval policy as one pure function, so it can be tested without
/// a database and every refusal has exactly one place to come from — the shape
/// of `remote_config::decide`.
///
/// Clearing an approval (`serve: false`) is a plain `MANAGE_AGENTS` act: it is
/// not a grant, and the admin who can approve must not be the only one who can
/// revoke. Re-stating an approval that already stands is not a grant either —
/// #600's `check_grant` gates only bits being ADDED, and the same rule applies
/// to this one-bit policy.
pub fn decide_approval(
    caller_permissions: u64,
    current: &PeerRelayPolicy,
    requested: &PeerRelayPolicy,
) -> Result<(), PeerRelayDenyReason> {
    if !permissions::has(caller_permissions, permissions::MANAGE_AGENTS) {
        return Err(PeerRelayDenyReason::NotDeviceAdmin);
    }
    if requested.serve
        && !current.serve
        && !permissions::has(caller_permissions, permissions::EXEC_DEVICE)
    {
        return Err(PeerRelayDenyReason::CannotGrantRelay);
    }
    Ok(())
}

fn tenant_of(tenant_id: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PeerRelayModeBody {
    pub mode: PeerRelayMode,
}

/// One approved relay device, as the settings view lists it.
#[derive(Debug, Serialize)]
pub struct RelayDeviceView {
    pub id: String,
    pub name: String,
    pub status: AgentStatus,
    /// Gate 4 as the device last advertised it: `true` only when its hello
    /// carried the `relay-server` capability, i.e. the device itself opted in
    /// with `relay_server_enabled`. An approved device that is not serving is
    /// the most common "why is nothing relayed?" answer, so it is on screen.
    pub serving: bool,
    pub static_endpoints: Vec<String>,
}

impl From<Agent> for RelayDeviceView {
    fn from(a: Agent) -> Self {
        Self {
            id: a.id.map(|i| i.to_hex()).unwrap_or_default(),
            name: a.name,
            status: a.status,
            serving: a.capabilities.has_rpc(RpcCap::RelayServer),
            static_endpoints: a.peer_relay_policy.static_endpoints,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PeerRelaySettings {
    pub mode: PeerRelayMode,
    /// Devices approved under gate 3, whether or not they are serving.
    pub relays: Vec<RelayDeviceView>,
}

/// `GET /api/tenant/{tenant_id}/peer-relay`
pub async fn get_settings(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<PeerRelaySettings>, ApiError> {
    let tid = tenant_of(&tenant_id)?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;
    let mode = state
        .overlay_networks
        .get_or_create(tid)
        .await?
        .peer_relay_mode;
    let relays = state
        .fleet
        .agents
        .list_relay_approved(tid)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(PeerRelaySettings { mode, relays }))
}

/// `PUT /api/tenant/{tenant_id}/peer-relay` — gate 1.
pub async fn set_mode(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<PeerRelayModeBody>,
) -> Result<Json<PeerRelayModeBody>, ApiError> {
    let tid = tenant_of(&tenant_id)?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_TENANT,
        "MANAGE_TENANT",
    )
    .await?;
    state
        .overlay_networks
        .set_peer_relay_mode(tid, body.mode)
        .await?;
    state.org_relay.invalidate_mode(tid);
    if body.mode == PeerRelayMode::Off {
        // §7 trigger 1 — off means off NOW, not at the next expiry.
        let n = crate::org_relay::revoke_tenant(&state, tid, "mode_off").await;
        if n > 0 {
            warn!(tenant = %tenant_id, revoked = n, "peer-relay: live sessions revoked by the org switch");
        }
    }
    warn!(
        tenant = %tenant_id, admin = %auth.user_id, mode = ?body.mode,
        "peer-relay: org switch changed"
    );
    Ok(Json(PeerRelayModeBody { mode: body.mode }))
}

/// `PUT /api/tenant/{tenant_id}/agent/{agent_id}/peer-relay-policy` — gate 3.
///
/// Audited on BOTH arms from one call site, so a new refusal cannot forget to
/// audit itself; a refused caller gets a 403 that names the gate.
pub async fn set_policy(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Json(requested): Json<PeerRelayPolicy>,
) -> Result<Json<PeerRelayPolicy>, ApiError> {
    let tid = tenant_of(&tenant_id)?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    // Membership first: `get_member_permissions` is the membership check too,
    // so a non-member never reaches the policy below.
    let perms = state
        .tenants
        .get_member_permissions(tid, auth.user_id)
        .await?;

    // Tenant-scoped, so an agent id from another org is a 404 rather than a
    // cross-tenant read.
    let agent = state
        .fleet
        .agents
        .base
        .find_by_id_in_tenant(tid, aid)
        .await?;
    // Static endpoints are server-pushed probe targets that every device in
    // the tenant will dial as SYSTEM/root: public `ip:port` literals only
    // (spec §5, SSRF). Checked before the decision so a refused body never
    // leaves a granted-looking audit row behind.
    if let Some(bad) = requested
        .static_endpoints
        .iter()
        .find(|e| !crate::org_relay::valid_static_endpoint(e))
    {
        return Err(ApiError::BadRequest(format!(
            "static endpoint {bad} is not a public ip:port"
        )));
    }

    let verdict = decide_approval(perms, &agent.peer_relay_policy, &requested);
    let event = PeerRelayAuditEvent {
        id: None,
        tenant_id: tid,
        action: PeerRelayAuditAction::Approve,
        agent_id: Some(aid),
        user_id: Some(auth.user_id),
        requester_node_id: None,
        peer_node_id: None,
        relay_node_id: None,
        serve: Some(requested.serve),
        vni: None,
        warn_only: false,
        at: DateTime::now(),
        denied: verdict.err(),
        reason: None,
    };
    if let Err(e) = state.peer_relay_audit.record(event).await {
        // Best-effort, like the other decision logs: an audit insert must
        // never be what stops a legitimate change.
        warn!(%e, "peer-relay audit write failed");
    }
    if let Err(reason) = verdict {
        return Err(ApiError::Forbidden(reason.message().to_string()));
    }

    state
        .fleet
        .agents
        .update_peer_relay_policy(tid, aid, &requested)
        .await?;
    if !requested.serve {
        // §7 trigger 3 — clearing the approval tears down what it carried.
        let n = crate::org_relay::revoke_relay_agent(&state, tid, aid, "policy_revoked").await;
        if n > 0 {
            warn!(tenant = %tenant_id, agent = %agent_id, revoked = n,
                "peer-relay: live sessions revoked with the approval");
        }
    }
    warn!(
        tenant = %tenant_id, agent = %agent_id, admin = %auth.user_id,
        serve = requested.serve, "peer-relay: device approval changed"
    );
    Ok(Json(requested))
}

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default = "default_audit_page")]
    pub page: u64,
    #[serde(default = "default_audit_per_page")]
    pub per_page: u64,
    #[serde(default)]
    pub before: Option<String>,
}

fn default_audit_page() -> u64 {
    1
}

fn default_audit_per_page() -> u64 {
    25
}

/// The DTO the audit route returns. Deliberately NOT [`PeerRelayAuditEvent`]
/// straight off the wire: bson's `ObjectId` / `DateTime` serialise to extended
/// JSON that no client here parses — the `[object Object]` trap `ExecAuditRow`
/// exists to avoid.
#[derive(Debug, Serialize)]
pub struct PeerRelayAuditRow {
    pub id: String,
    pub tenant_id: String,
    pub action: PeerRelayAuditAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester_node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serve: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vni: Option<u32>,
    pub warn_only: bool,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied: Option<PeerRelayDenyReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl From<PeerRelayAuditEvent> for PeerRelayAuditRow {
    fn from(e: PeerRelayAuditEvent) -> Self {
        let hex = |o: Option<ObjectId>| o.map(|i| i.to_hex());
        Self {
            id: e.id.map(|i| i.to_hex()).unwrap_or_default(),
            tenant_id: e.tenant_id.to_hex(),
            action: e.action,
            agent_id: hex(e.agent_id),
            user_id: hex(e.user_id),
            requester_node_id: hex(e.requester_node_id),
            peer_node_id: hex(e.peer_node_id),
            relay_node_id: hex(e.relay_node_id),
            serve: e.serve,
            vni: e.vni,
            warn_only: e.warn_only,
            at: e.at.try_to_rfc3339_string().unwrap_or_default(),
            denied: e.denied,
            reason: e.reason,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PeerRelayAuditResponse {
    pub items: Vec<PeerRelayAuditRow>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
}

/// `GET /api/tenant/{tenant_id}/peer-relay-audit`
pub async fn audit(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<PeerRelayAuditResponse>, ApiError> {
    let tid = tenant_of(&tenant_id)?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::VIEW_EXEC_AUDIT,
        "VIEW_EXEC_AUDIT",
    )
    .await?;
    let pg = PaginationParams {
        page: q.page,
        per_page: q.per_page,
        before: q.before.clone(),
    };
    let page = match &q.agent_id {
        Some(a) => {
            let aid = ObjectId::parse_str(a)
                .map_err(|_| ApiError::BadRequest("Invalid agent_id".into()))?;
            state.peer_relay_audit.list_for_agent(tid, aid, &pg).await?
        }
        None => state.peer_relay_audit.list_for_tenant(tid, &pg).await?,
    };
    Ok(Json(PeerRelayAuditResponse {
        items: page.items.into_iter().map(Into::into).collect(),
        total: page.total,
        page: page.page,
        per_page: page.per_page,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use permissions::*;

    fn p(serve: bool) -> PeerRelayPolicy {
        PeerRelayPolicy {
            serve,
            static_endpoints: Vec::new(),
        }
    }

    /// The whole point: `DEFAULT_ADMIN` carries MANAGE_AGENTS but not
    /// EXEC_DEVICE, and that must not be enough to nominate a chokepoint.
    #[test]
    fn an_admin_without_exec_device_cannot_approve() {
        assert_eq!(
            decide_approval(DEFAULT_ADMIN, &p(false), &p(true)),
            Err(PeerRelayDenyReason::CannotGrantRelay)
        );
    }

    #[test]
    fn an_admin_with_exec_device_can_approve() {
        assert_eq!(
            decide_approval(DEFAULT_ADMIN | EXEC_DEVICE, &p(false), &p(true)),
            Ok(())
        );
    }

    /// Revocation is not a grant: the admin who can approve must not be the
    /// only one who can take it back.
    #[test]
    fn clearing_an_approval_needs_only_manage_agents() {
        assert_eq!(decide_approval(DEFAULT_ADMIN, &p(true), &p(false)), Ok(()));
    }

    /// Re-stating an approval that already stands adds no bit, so it is not
    /// a grant either — the `check_grant` rule (#600) on a one-bit policy.
    #[test]
    fn restating_an_approval_is_not_a_grant() {
        assert_eq!(decide_approval(DEFAULT_ADMIN, &p(true), &p(true)), Ok(()));
    }

    #[test]
    fn a_member_is_not_a_device_admin_even_to_clear() {
        assert_eq!(
            decide_approval(DEFAULT_MEMBER, &p(true), &p(false)),
            Err(PeerRelayDenyReason::NotDeviceAdmin)
        );
    }

    /// EXEC_DEVICE without MANAGE_AGENTS is "may run commands", not "may
    /// manage the fleet" — the first gate still applies.
    #[test]
    fn exec_device_alone_is_not_a_device_admin() {
        assert_eq!(
            decide_approval(EXEC_DEVICE, &p(false), &p(true)),
            Err(PeerRelayDenyReason::NotDeviceAdmin)
        );
    }

    #[test]
    fn administrator_bypasses_as_everywhere_else() {
        assert_eq!(decide_approval(ADMINISTRATOR, &p(false), &p(true)), Ok(()));
    }
}
