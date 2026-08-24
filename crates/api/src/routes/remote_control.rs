//! REST surface for the remote-control subsystem.
//!
//! Per `docs/remote-control.md` §9.1. Signaling (SDP/ICE) happens over the
//! WebSocket; this module is everything else: agent enrollment, CRUD, session
//! introspection, TURN credential issuance.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::{
    models::{AccessPolicy, AgentStatus, NodeRef, OsKind, RemoteAuditEvent, RemoteSession},
    permissions::Permissions,
    signaling::IceServer,
    turn_creds::ice_servers_for,
};
use roomler_ai_services::dao::base::PaginationParams;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

const ENROLLMENT_TTL_SECS: u64 = 600; // 10 minutes per §11.1

/// Require a `role::permissions` bit for `user_id` in `tenant_id`. Doubles as
/// the membership check — `get_member_permissions` returns `Forbidden` for a
/// non-member. `owner` always passes via the `ADMINISTRATOR` bypass in `has`.
///
/// `pub(crate)` so the sibling device-management surfaces in the same subsystem
/// (`routes::overlay_route`, `routes::tunnel`) gate their destructive endpoints
/// on the same `MANAGE_AGENTS` bit rather than re-deriving the check.
pub(crate) async fn require_permission(
    state: &AppState,
    tenant_id: ObjectId,
    user_id: ObjectId,
    flag: u64,
    label: &str,
) -> Result<(), ApiError> {
    let perms = state
        .tenants
        .get_member_permissions(tenant_id, user_id)
        .await?;
    if !permissions::has(perms, flag) {
        return Err(ApiError::Forbidden(format!("Missing {label} permission")));
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Agent enrollment
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct EnrollmentTokenResponse {
    pub enrollment_token: String,
    pub expires_in: u64,
    pub jti: String,
}

/// POST /api/tenant/{tenant_id}/agent/enroll-token — admin issues an enrollment
/// token that a new agent binary exchanges (once, within 10 min) for a
/// long-lived agent token.
pub async fn issue_enrollment_token(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<EnrollmentTokenResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    let (token, jti) = state
        .auth
        .issue_enrollment_token(auth.user_id, tid, ENROLLMENT_TTL_SECS)?;

    Ok(Json(EnrollmentTokenResponse {
        enrollment_token: token,
        expires_in: ENROLLMENT_TTL_SECS,
        jti,
    }))
}

#[derive(Debug, Deserialize)]
pub struct EnrollRequest {
    pub enrollment_token: String,
    pub machine_id: String,
    pub machine_name: String,
    pub os: OsKind,
    pub agent_version: String,
}

#[derive(Debug, Serialize)]
pub struct EnrollResponse {
    pub agent_id: String,
    pub tenant_id: String,
    pub agent_token: String,
}

/// POST /api/agent/enroll — public (no user JWT); authenticates via the
/// enrollment token instead. Creates or rehydrates the Agent row and returns
/// a long-lived agent JWT.
pub async fn enroll_agent(
    State(state): State<AppState>,
    Json(body): Json<EnrollRequest>,
) -> Result<Json<EnrollResponse>, ApiError> {
    let claims = state.auth.verify_enrollment_token(&body.enrollment_token)?;
    let tid = ObjectId::parse_str(&claims.tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id claim".to_string()))?;
    let admin_uid = ObjectId::parse_str(&claims.sub)
        .map_err(|_| ApiError::BadRequest("Invalid admin user id claim".to_string()))?;

    // An archived org accepts no devices — including back into an existing
    // row, which is what makes archiving a throwaway org actually final
    // (`routes::tenant::archive`). Checked before the single-use claim so a
    // refused enrollment does not also burn the token.
    if state
        .tenants
        .base
        .find_by_id(tid)
        .await
        .map(|t| t.is_archived)
        .unwrap_or(false)
    {
        return Err(ApiError::Forbidden(
            "This organization is archived and accepts no device enrollments".to_string(),
        ));
    }

    // If a row already exists for (tenant_id, machine_id), rehydrate it —
    // refresh name / os / agent_version, clear `deleted_at` if soft-deleted,
    // and re-issue a token against the same _id. The unique index on
    // (tenant_id, machine_id) covers soft-deleted rows too, so we must look
    // them up *including* tombstones and revive in place rather than create.
    let existing = state
        .agents
        .find_by_tenant_and_machine(tid, &body.machine_id)
        .await?;
    // S5 — plan device cap. Only a NEW row consumes a slot; a known machine
    // rehydrates below regardless of the cap (re-enrolling your existing
    // fleet must never brick). Checked BEFORE the single-use claim on
    // purpose: a token burnt by a rejection the operator can only fix
    // elsewhere (upgrade the plan, remove a device) would fail their retry a
    // second time, for a new and confusing reason.
    if existing.is_none() {
        let tenant = state.tenants.base.find_by_id(tid).await?;
        let max = tenant.plan.limits().max_devices as u64;
        let used = state.agents.count_active_for_tenant(tid).await?;
        if used >= max {
            return Err(ApiError::Forbidden(format!(
                "Device limit reached for the {:?} plan ({used} of {max} devices used). \
                 Upgrade the plan or remove a device first.",
                tenant.plan
            )));
        }
    }

    // Enrollment tokens are single-use BY DESIGN and were never enforced —
    // field 2026-08-05 replayed one after a cap rejection. Claimed on BOTH
    // branches: a replay against a KNOWN machine still mints a fresh agent
    // JWT, which is the credential worth protecting.
    state
        .used_tokens
        .claim(&claims.jti, "agent-enroll")
        .await
        .map_err(|_| {
            ApiError::Unauthorized("This enrollment token has already been used".into())
        })?;

    let agent = match existing {
        Some(a) => {
            let id =
                a.id.ok_or_else(|| ApiError::Internal("agent missing _id".to_string()))?;
            state
                .agents
                .rehydrate(id, &body.machine_name, body.os, &body.agent_version)
                .await?
        }
        None => {
            state
                .agents
                .create(
                    tid,
                    admin_uid,
                    body.machine_name,
                    body.machine_id,
                    body.os,
                    body.agent_version,
                    String::new(), // agent_token_hash unused in JWT-only scheme
                )
                .await?
        }
    };

    let agent_id = agent
        .id
        .ok_or_else(|| ApiError::Internal("agent missing _id".to_string()))?;
    let agent_token = state.auth.issue_agent_token(agent_id, tid, None)?;

    Ok(Json(EnrollResponse {
        agent_id: agent_id.to_hex(),
        tenant_id: tid.to_hex(),
        agent_token,
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// Agent CRUD
// ────────────────────────────────────────────────────────────────────────────

/// Phase A-1 three-state presence (see `to_agent_response`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentPresence {
    /// An rc socket is registered on some pod — Connect will work.
    Online,
    /// Heartbeat trail fresh but no pod claims the socket — amber.
    Stale,
    Offline,
}

#[derive(Debug, Serialize)]
pub struct AgentResponse {
    pub id: String,
    pub tenant_id: String,
    pub owner_user_id: String,
    pub name: String,
    pub machine_id: String,
    pub os: OsKind,
    pub agent_version: String,
    pub status: AgentStatus,
    /// Three-state truth: `online` | `stale` | `offline` (Phase A-1).
    pub presence: AgentPresence,
    /// Back-compat bool = `presence == online` (a socket is actually
    /// registered; pre-A-1 this also counted heartbeat-only agents,
    /// which is what showed green on dead sockets).
    pub is_online: bool,
    pub last_seen_at: String,
    pub access_policy: AccessPolicy,
    /// Subnet-router CIDRs this agent advertises for the mesh (Phase 2). The
    /// `roomler-tunnel socks5` mesh longest-prefix-matches a LAN target IP
    /// against these to pick the covering agent, which then dials the real
    /// IP. Admin-managed here; still gated by the tenant's `tunnel_policies`.
    pub routes: Vec<String>,
    /// Subnet CIDRs the agent itself ADVERTISES it can route (from its
    /// `advertise_routes` config, sent on `rc:agent.hello`). Untrusted
    /// suggestions the admin approves into `routes`. Empty for pre-feature agents.
    pub advertised_routes: Vec<String>,
    /// Codec + HW backend availability advertised by the agent in its
    /// most recent rc:agent.hello. Default empty for pre-2A.1 agents
    /// that haven't reconnected since the schema change.
    pub capabilities: roomler_ai_remote_control::models::AgentCaps,
    /// Multi-region relay PoPs: the agent's nearest relay region (derived
    /// from its STUN probe reports). `None` = never probed / all timed out —
    /// the default region serves it.
    pub relay_home: Option<String>,
    /// Fleet-RPC gate 3 as stored (`docs/fleet-rpc.md`), or `None` when this
    /// device's policy is indistinguishable from the untouched default.
    ///
    /// ⚠️ Returning this at all is not cosmetic. The policy dialogs PUT the
    /// WHOLE shape, so a dialog that cannot read the current policy opens on
    /// its closed default and the next save REPLACES the real one — silently
    /// dropping an `allowed_user_ids` restriction, which widens the device
    /// from "these three people" to "anyone holding the bit".
    ///
    /// ⚠️ And `Option`, not "always `Some`", for the mirror-image reason: the
    /// stored model cannot tell a device nobody configured from one explicitly
    /// saved as all-defaults, so `None` is the honest answer for that shape and
    /// leaves the dialog on its own closed default. See [`Self::ssh_policy`],
    /// where the difference has teeth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec_policy: Option<crate::routes::agent_exec::ExecPolicyBody>,
    /// Roomler-SSH gate 3 as stored, with the same replace-on-save warning as
    /// [`Self::exec_policy`] — and this is the one that makes `Option`
    /// load-bearing rather than tidy.
    ///
    /// The MODEL's default `account_mode` is `daemon` (SYSTEM / root); the
    /// DIALOG's is `console_user`, deliberately, so that an admin who turns SSH
    /// on without reading the selector does not get a root shell. Sending the
    /// model default for every unconfigured device would pre-select `daemon` in
    /// that dialog and undo exactly that protection — so a policy equal to the
    /// default is reported as absent instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_policy: Option<crate::routes::agent_ssh::SshPolicyBody>,
    /// Remote config — what an operator has ASKED this device to run, and what
    /// the device SAID it did (`docs/remote-config.md`).
    ///
    /// Absent when nothing has ever been requested. The two halves have to
    /// travel together: a report alone cannot be read (it may be about an
    /// older revision), and a request alone cannot be read either (a pending
    /// change and a refused one look identical without the answer).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_config: Option<RemoteConfigView>,
}

/// The remote-config state of one device, as the dashboard needs to read it.
///
/// Deliberately assembled here rather than serialising the two model fields
/// straight out. Beyond the usual `{"$oid": …}` problem, the honest answer to
/// "did this land?" is a comparison — desired revision vs reported revision vs
/// what the device is even capable of saying — and doing that comparison in
/// one place beats doing it in every client that asks.
#[derive(Debug, Serialize)]
pub struct RemoteConfigView {
    /// The keys under management, as requested.
    pub desired: DesiredConfigView,
    /// The device's last word, if it has said one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<ConfigReportView>,
    /// Where this stands, resolved server-side — see [`RemoteConfigState`].
    pub state: RemoteConfigState,
}

/// Where a device's remote config stands. One enum rather than three booleans
/// on the client, because these are mutually exclusive and a client that
/// derived them separately would eventually render two at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteConfigState {
    /// The device confirmed this exact revision, and everything asked for is
    /// in force.
    Applied,
    /// Confirmed, but some keys only take effect after a daemon restart. A
    /// SEPARATE state from `applied` on purpose: reporting them as one would
    /// tell an operator SSH is open while the device still refuses every
    /// session.
    NeedsRestart,
    /// The device refused — see `report.outcome` for which refusal. Both of
    /// them have a concrete next action, which is why they are surfaced rather
    /// than folded into "not applied".
    Refused,
    /// The device tried and failed; `report.detail` says why.
    Failed,
    /// Requested, and we are waiting for the device to say something about
    /// THIS revision. Includes "it has not reconnected yet" — reconcile
    /// happens on connect.
    Pending,
    /// The device's agent understands a pushed config but predates
    /// [`RpcCap::ConfigReport`], so it may well have applied this perfectly
    /// and will never say so. Distinct from `pending` because waiting is
    /// futile here and the fix is to update the device.
    ///
    /// [`RpcCap::ConfigReport`]: roomler_ai_remote_control::models::RpcCap::ConfigReport
    ReportsUnsupported,
    /// The device's agent predates `rc:agent.config` entirely — the push is
    /// not even sent. Nothing will happen until it is updated.
    PushUnsupported,
}

/// [`DesiredConfig`] with hex ids and an RFC3339 timestamp.
///
/// [`DesiredConfig`]: roomler_ai_remote_control::models::DesiredConfig
#[derive(Debug, Serialize)]
pub struct DesiredConfigView {
    pub revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_authorized_keys: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_account_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// [`ConfigReport`] with an RFC3339 timestamp.
///
/// [`ConfigReport`]: roomler_ai_remote_control::models::ConfigReport
#[derive(Debug, Serialize)]
pub struct ConfigReportView {
    pub revision: u64,
    pub outcome: roomler_ai_remote_control::models::ConfigOutcome,
    pub live: Vec<String>,
    pub needs_restart: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub reported_at: String,
}

/// Resolve the desired config + the device's report + its capabilities into
/// the one thing a reader wants: where this stands.
///
/// ⚠️ The revision comparison is the load-bearing part. A report for an OLDER
/// revision is a device that has not caught up — which looks exactly like a
/// refusal if you only read `outcome`, and exactly like success if you only
/// read "there is a report". Both misreadings put a wrong answer on a screen
/// an operator is using to decide whether a device is safe.
fn remote_config_view(
    desired: roomler_ai_remote_control::models::DesiredConfig,
    report: Option<roomler_ai_remote_control::models::ConfigReport>,
    caps: &roomler_ai_remote_control::models::AgentCaps,
) -> Option<RemoteConfigView> {
    use roomler_ai_remote_control::models::{ConfigOutcome, RpcCap};

    if desired.is_empty() {
        return None;
    }
    let current = report.as_ref().filter(|r| r.revision == desired.revision);
    let state = match current.map(|r| r.outcome) {
        Some(ConfigOutcome::Applied | ConfigOutcome::Noop) => {
            // `needs_restart` beats `applied`: the half that is merely written
            // down is the half an operator will otherwise act on wrongly.
            if current.is_some_and(|r| !r.needs_restart.is_empty()) {
                RemoteConfigState::NeedsRestart
            } else {
                RemoteConfigState::Applied
            }
        }
        Some(ConfigOutcome::NotOptedIn | ConfigOutcome::NotPrimary) => RemoteConfigState::Refused,
        Some(ConfigOutcome::Failed) => RemoteConfigState::Failed,
        // No report for THIS revision. Which of the three "no answer" states
        // it is depends on what the device can do, not on how long we waited.
        None if !caps.has_rpc(RpcCap::Config) => RemoteConfigState::PushUnsupported,
        None if !caps.has_rpc(RpcCap::ConfigReport) => RemoteConfigState::ReportsUnsupported,
        None => RemoteConfigState::Pending,
    };
    Some(RemoteConfigView {
        desired: DesiredConfigView {
            revision: desired.revision,
            exec_enabled: desired.exec_enabled,
            ssh_enabled: desired.ssh_enabled,
            ssh_authorized_keys: desired.ssh_authorized_keys,
            ssh_account_mode: desired.ssh_account_mode,
            ssh_port: desired.ssh_port,
            updated_by: desired.updated_by.map(|i| i.to_hex()),
            updated_at: desired.updated_at.map(fmt_dt),
        },
        report: report.map(|r| ConfigReportView {
            revision: r.revision,
            outcome: r.outcome,
            live: r.live,
            needs_restart: r.needs_restart,
            detail: r.detail,
            reported_at: fmt_dt(r.reported_at),
        }),
        state,
    })
}

/// `None` for a policy that is byte-for-byte the untouched default — see
/// [`AgentResponse::ssh_policy`] for why that distinction has to survive to
/// the client rather than being flattened here.
fn configured_only<T: Default + PartialEq, B: From<T>>(policy: T) -> Option<B> {
    (policy != T::default()).then(|| policy.into())
}

pub async fn list_agents(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let page = state.agents.list_for_tenant(tid, &params).await?;
    let redis_fresh = agent_presence_batch(&state, &page.items).await;
    let items: Vec<AgentResponse> = page
        .items
        .into_iter()
        .map(|a| {
            let fresh = a.id.map(|i| redis_fresh.contains(&i)).unwrap_or(false);
            to_agent_response(&state, a, fresh)
        })
        .collect();

    Ok(Json(serde_json::json!({
        "items": items,
        "total": page.total,
        "page": page.page,
        "per_page": page.per_page,
        "total_pages": page.total_pages,
    })))
}

pub async fn get_agent(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
) -> Result<Json<AgentResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let agent = state.agents.find_in_tenant(tid, aid).await?;
    let redis_fresh = agent_presence_batch(&state, std::slice::from_ref(&agent)).await;
    let fresh = agent.id.map(|i| redis_fresh.contains(&i)).unwrap_or(false);
    Ok(Json(to_agent_response(&state, agent, fresh)))
}

#[derive(Debug, Deserialize)]
pub struct UpdateAgentRequest {
    pub name: Option<String>,
    pub access_policy: Option<AccessPolicy>,
    /// Reassign the device owner (hex user id). `MANAGE_AGENTS` only.
    pub owner_user_id: Option<String>,
    /// Replace the advertised subnet-router CIDRs (mesh Phase 2). Each entry
    /// is validated + canonicalized by `normalize_routes`; an invalid CIDR
    /// fails the whole request with 400. `MANAGE_AGENTS` only.
    pub routes: Option<Vec<String>>,
}

pub async fn update_agent(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Json(body): Json<UpdateAgentRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    if let Some(owner) = body.owner_user_id {
        let owner_id = ObjectId::parse_str(&owner)
            .map_err(|_| ApiError::BadRequest("Invalid owner_user_id".to_string()))?;
        state.agents.update_owner(tid, aid, owner_id).await?;
    }
    if let Some(name) = body.name {
        state.agents.rename(tid, aid, &name).await?;
    }
    if let Some(policy) = body.access_policy {
        state.agents.update_access_policy(tid, aid, &policy).await?;
    }
    if let Some(routes) = body.routes {
        let normalized = normalize_routes(routes)?;
        state.agents.update_routes(tid, aid, &normalized).await?;
    }

    Ok(Json(serde_json::json!({ "updated": true })))
}

/// DELETE /api/tenant/{tid}/agent/{agent_id} — remove a device from the fleet.
///
/// Cascades to the overlay: the agent's mesh node is evicted (peers get a
/// `removes` delta) and its overlay IP goes back to the tenant's free pool for
/// reuse. The agent binary stays installed on the host and may be enrolled
/// again — but it comes back as a NEW mesh node with a fresh address.
pub async fn delete_agent(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    // Read first: gives a real 404 for a bogus agent id (`soft_delete` returns
    // a bool the handler used to discard, so deleting a nonexistent agent
    // reported `{"deleted": true}`), and yields the machine_id the overlay node
    // is keyed by.
    let agent = state.agents.find_in_tenant(tid, aid).await?;

    // Release the overlay lease BEFORE the soft-delete and BEFORE the kick. The
    // kick tears down the WS, whose teardown calls `handle_overlay_leave` — with
    // the node already tombstoned that is a clean no-op instead of a second
    // `removes` fan racing the release's compare-and-swap.
    let released = crate::ws::overlay::release_overlay_node_for(
        &state,
        tid,
        &agent.machine_id,
        &NodeRef::Agent { agent_id: aid },
        "agent_delete",
    )
    .await;

    state.agents.soft_delete(tid, aid).await?;
    // rc.53: admin-driven kick — no tx identity to thread through;
    // pass None to force unconditional removal (the identity gate is
    // only for the displaced-handler unregister race, not for
    // operator kicks).
    state.rc_hub.unregister_agent(aid, None); // kick any live WS
    // C-2 — the agent's WS may be homed on another pod; broadcast the
    // kick so every hub drops it (idempotent no-op where absent).
    crate::ws::remote_control::publish_rc_ctrl(
        &state,
        "kick",
        serde_json::json!({ "agent_id": aid.to_hex() }),
    )
    .await;
    Ok(Json(serde_json::json!({
        "deleted": true,
        "overlay_released": released.is_some(),
        "overlay_ip": released.as_ref().map(|r| r.overlay_ip.clone()),
    })))
}

// ────────────────────────────────────────────────────────────────────────────
// Forced self-update (S1a)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct TriggerUpdateRequest {
    /// Optional release tag to pin (e.g. `agent-v0.3.0-rc.260`); omitted =
    /// latest. Forwarded verbatim to the agent.
    #[serde(default)]
    pub pin: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TriggerUpdateResult {
    pub agent_id: String,
    /// Whether the message reached a live agent WS. `false` = offline (or a
    /// full outbound queue); offline agents pick the release up on their own
    /// periodic check anyway.
    pub delivered: bool,
}

/// POST /api/tenant/{tid}/agent/{agent_id}/update — push `rc:agent.update`
/// to one agent. MANAGE_AGENTS. Pre-S1a agents ignore the unknown message
/// (decode-and-drop, same contract as `rc:goodbye`).
pub async fn trigger_agent_update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    // Body is required (send `{}` for defaults) — plain `Json` keeps us off
    // axum 0.8's `OptionalFromRequest` surface, which the codebase doesn't
    // use anywhere else.
    Json(body): Json<TriggerUpdateRequest>,
) -> Result<Json<TriggerUpdateResult>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    // Tenant-scope the target (404 for a foreign agent id).
    let _ = state.agents.find_in_tenant(tid, aid).await?;

    let pin = body.pin;
    let delivered = state
        .rc_hub
        .send_to_agent(
            aid,
            roomler_ai_remote_control::signaling::ServerMsg::UpdateNow { pin: pin.clone() },
        )
        .is_ok();
    tracing::info!(
        admin = %auth.user_id, agent = %aid, ?pin, delivered,
        "operator-triggered agent self-update"
    );
    Ok(Json(TriggerUpdateResult {
        agent_id: aid.to_hex(),
        delivered,
    }))
}

#[derive(Debug, Deserialize, Default)]
pub struct BulkTriggerUpdateRequest {
    /// Explicit targets (hex agent ids). Omitted/empty = every agent in the
    /// tenant.
    #[serde(default)]
    pub agent_ids: Option<Vec<String>>,
    #[serde(default)]
    pub pin: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BulkTriggerUpdateResponse {
    pub results: Vec<TriggerUpdateResult>,
    pub requested: usize,
    pub delivered: usize,
}

/// POST /api/tenant/{tid}/agent/update — push `rc:agent.update` to selected
/// (or all) agents in the tenant. MANAGE_AGENTS. Fleet-side stampede control
/// is the agents' own 5-min install-storm cooldown + the release proxy cache.
pub async fn trigger_agents_update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<BulkTriggerUpdateRequest>,
) -> Result<Json<BulkTriggerUpdateResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    let target_ids: Vec<ObjectId> = match body.agent_ids {
        Some(ids) if !ids.is_empty() => {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                let aid = ObjectId::parse_str(&id)
                    .map_err(|_| ApiError::BadRequest(format!("Invalid agent_id: {id}")))?;
                // Tenant-scope every explicit target.
                let _ = state.agents.find_in_tenant(tid, aid).await?;
                out.push(aid);
            }
            out
        }
        _ => {
            let page = state
                .agents
                .list_for_tenant(
                    tid,
                    &PaginationParams {
                        page: 1,
                        per_page: 1000,
                        before: None,
                    },
                )
                .await?;
            page.items.into_iter().filter_map(|a| a.id).collect()
        }
    };

    let mut results = Vec::with_capacity(target_ids.len());
    let mut delivered = 0usize;
    for aid in &target_ids {
        let ok = state
            .rc_hub
            .send_to_agent(
                *aid,
                roomler_ai_remote_control::signaling::ServerMsg::UpdateNow {
                    pin: body.pin.clone(),
                },
            )
            .is_ok();
        if ok {
            delivered += 1;
        }
        results.push(TriggerUpdateResult {
            agent_id: aid.to_hex(),
            delivered: ok,
        });
    }
    tracing::info!(
        admin = %auth.user_id, tenant = %tid,
        requested = target_ids.len(), delivered, pin = ?body.pin,
        "operator-triggered bulk agent self-update"
    );
    Ok(Json(BulkTriggerUpdateResponse {
        requested: results.len(),
        delivered,
        results,
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// Sessions
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub id: String,
    pub agent_id: String,
    pub tenant_id: String,
    pub controller_user_id: String,
    pub permissions: Permissions,
    pub phase: roomler_ai_remote_control::models::SessionPhase,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

pub async fn get_session(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
) -> Result<Json<SessionResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let sid = ObjectId::parse_str(&session_id)
        .map_err(|_| ApiError::BadRequest("Invalid session_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let session = state.remote_sessions.find_in_tenant(tid, sid).await?;
    Ok(Json(to_session_response(session)))
}

pub async fn terminate_session(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let sid = ObjectId::parse_str(&session_id)
        .map_err(|_| ApiError::BadRequest("Invalid session_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    // P3 security — bare tenant membership no longer suffices: only the
    // session's OWN controller or a member holding REMOTE_CONTROL may
    // force-close it. Party/tenant facts come from the LIVE session when this
    // pod holds it; a session homed on ANOTHER pod (rc sessions are pod-local
    // under S6) can't be inspected here, so non-controllers fall back to the
    // permission check against the ROUTE tenant — the ctrl event below only
    // acts on a hub that actually holds the session, and its tenant is
    // re-checked there.
    let live = state.rc_hub.session_snapshot(sid);
    if let Some((live_tid, _)) = live
        && live_tid != tid
    {
        // The path tenant must own the session it names.
        return Err(ApiError::NotFound("Session not found".to_string()));
    }
    let is_controller = matches!(live, Some((_, controller)) if controller == auth.user_id);
    if !is_controller {
        require_permission(
            &state,
            tid,
            auth.user_id,
            permissions::REMOTE_CONTROL,
            "remote-control",
        )
        .await?;
    }

    // Force-close via Hub. The Hub pushes a Terminate to both peers and audits.
    let terminated_here = state
        .rc_hub
        .terminate(
            sid,
            roomler_ai_remote_control::models::EndReason::AdminTerminated,
        )
        .is_ok();
    // P3 — rc sessions are pod-local (S6): when the session is homed on a
    // different pod the local terminate is a no-op that previously STILL
    // returned `{"terminated": true}`. Broadcast an idempotent ctrl event so
    // the owning pod applies it; `terminated` now reports only what THIS
    // request could verify locally.
    crate::ws::remote_control::publish_rc_ctrl(
        &state,
        "terminate",
        serde_json::json!({
            "session_id": sid.to_hex(),
            "tenant_id": tid.to_hex(),
        }),
    )
    .await;
    Ok(Json(serde_json::json!({
        "terminated": terminated_here && live.is_some(),
        "broadcast": true,
    })))
}

#[derive(Debug, Serialize)]
pub struct AuditListResponse {
    pub items: Vec<RemoteAuditEvent>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

pub async fn session_audit(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<AuditListResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let sid = ObjectId::parse_str(&session_id)
        .map_err(|_| ApiError::BadRequest("Invalid session_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::VIEW_REMOTE_AUDIT,
        "VIEW_REMOTE_AUDIT",
    )
    .await?;

    // Ensure the session actually belongs to this tenant.
    let _ = state.remote_sessions.find_in_tenant(tid, sid).await?;

    let page = state.remote_audit.list_for_session(sid, &params).await?;
    Ok(Json(AuditListResponse {
        items: page.items,
        total: page.total,
        page: page.page,
        per_page: page.per_page,
        total_pages: page.total_pages,
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// TURN credentials
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TurnCredentialsResponse {
    pub ice_servers: Vec<IceServer>,
}

#[derive(Debug, Serialize)]
pub struct RelayRegionsResponse {
    pub regions_enabled: bool,
    pub regions: Vec<RelayRegionSummary>,
}

#[derive(Debug, Serialize)]
pub struct RelayRegionSummary {
    pub id: String,
    pub turn_url: String,
    pub derp_url: Option<String>,
    pub enabled: bool,
    /// P6b — the poller's latest load snapshot (`None` = never sampled).
    pub load: Option<roomler_ai_remote_control::turn_creds::RegionLoad>,
    /// P6b — currently steered around by the load-aware pick (fresh-busy).
    pub busy: bool,
}

/// GET /api/relay/regions — the configured relay PoP topology (region ids +
/// endpoints; never secrets). Authed users only; the same hostnames every
/// TURN grant already exposes to clients.
pub async fn relay_regions(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Result<Json<RelayRegionsResponse>, ApiError> {
    let regions = state
        .turn_map
        .specs
        .iter()
        .map(|s| RelayRegionSummary {
            load: state.relay_load.get(&s.id).map(|l| l.value().clone()),
            busy: roomler_ai_remote_control::turn_creds::region_busy(&state.relay_load, &s.id),
            id: s.id.clone(),
            turn_url: s.turn_url.clone(),
            derp_url: s.derp_url.clone(),
            enabled: s.enabled,
        })
        .collect();
    Ok(Json(RelayRegionsResponse {
        regions_enabled: state.turn_map.enabled,
        regions,
    }))
}

/// GET /api/turn/credentials — user-scoped, returns short-lived (10 min) TURN
/// creds plus a STUN fallback. Used by the browser controller and by the
/// native agent when it needs to trickle ICE.
pub async fn turn_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<TurnCredentialsResponse>, ApiError> {
    // This route is session-less (a pre-fetch), so it issues the default
    // region's generic URL list; the per-session same-worker affinity and the
    // region pick happen on the Hub's issuance paths.
    let ice_servers = ice_servers_for(&auth.user_id.to_hex(), state.turn_map.cfg_for(None));
    Ok(Json(TurnCredentialsResponse { ice_servers }))
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

/// Batched cross-pod presence read for the listing (Phase A-1): one MGET
/// under a 200 ms budget. Returns the set of agent ids with a FRESH
/// (TTL-enforced) directory record on ANY pod. Redis down / timeout ⇒
/// empty set — `to_agent_response` then degrades to the pre-A-1
/// heartbeat disjunction, never to hard-offline.
async fn agent_presence_batch(
    state: &AppState,
    agents: &[roomler_ai_remote_control::models::Agent],
) -> std::collections::HashSet<ObjectId> {
    let Some(redis) = &state.redis_pubsub else {
        return Default::default();
    };
    let ids: Vec<ObjectId> = agents.iter().filter_map(|a| a.id).collect();
    let hexes: Vec<String> = ids.iter().map(|i| i.to_hex()).collect();
    match tokio::time::timeout(
        std::time::Duration::from_millis(200),
        redis.agent_presence_get_many(&hexes),
    )
    .await
    {
        Ok(Ok(vals)) => ids
            .into_iter()
            .zip(vals)
            .filter_map(|(id, v)| v.map(|_| id))
            .collect(),
        Ok(Err(e)) => {
            tracing::debug!(%e, "agent presence MGET failed; degrading to heartbeat disjunction");
            Default::default()
        }
        Err(_) => {
            tracing::debug!("agent presence MGET timed out; degrading to heartbeat disjunction");
            Default::default()
        }
    }
}

fn to_agent_response(
    state: &AppState,
    a: roomler_ai_remote_control::models::Agent,
    redis_fresh: bool,
) -> AgentResponse {
    let id = a.id.map(|i| i.to_hex()).unwrap_or_default();
    // Phase A-1 three-state presence. "online" = an rc socket is
    // REGISTERED somewhere (this pod's hub, or any pod's fresh Redis
    // directory record) — the state in which Connect will actually work.
    // "stale" = the Mongo heartbeat trail is fresh but no pod claims the
    // socket (half-open middlebox leg, directory outage, or a pod that
    // died without cleanup) — visible, amber, Connect disabled. "offline"
    // = neither. With Redis down `redis_fresh` is always false, so the
    // presence degrades to the pre-A-1 heartbeat disjunction (a cross-pod
    // agent shows "stale" instead of "online" — degraded, never lying
    // green on a dead socket ONLY, and never hard-offline).
    let hub_online =
        a.id.map(|i| state.rc_hub.is_agent_online(i))
            .unwrap_or(false);
    let recently_seen = matches!(
        a.status,
        roomler_ai_remote_control::models::AgentStatus::Online
    ) && {
        let age_ms = bson::DateTime::now().timestamp_millis() - a.last_seen_at.timestamp_millis();
        age_ms < 90_000
    };
    let presence = if hub_online || redis_fresh {
        AgentPresence::Online
    } else if recently_seen {
        AgentPresence::Stale
    } else {
        AgentPresence::Offline
    };
    // Back-compat: `is_online` = the reachable state only. (Pre-A-1 it
    // was `hub_online || recently_seen`, which is what lied green.)
    let is_online = matches!(presence, AgentPresence::Online);
    // Resolved before the struct literal moves `capabilities`: the
    // remote-config state depends on which verbs this device advertises.
    let remote_config = remote_config_view(a.desired_config, a.config_report, &a.capabilities);
    AgentResponse {
        presence,
        id,
        tenant_id: a.tenant_id.to_hex(),
        owner_user_id: a.owner_user_id.to_hex(),
        name: a.name,
        machine_id: a.machine_id,
        os: a.os,
        agent_version: a.agent_version,
        status: a.status,
        is_online,
        last_seen_at: fmt_dt(a.last_seen_at),
        access_policy: a.access_policy,
        routes: a.routes,
        advertised_routes: a.advertised_routes,
        capabilities: a.capabilities,
        relay_home: a.relay_home,
        exec_policy: configured_only(a.exec_policy),
        ssh_policy: configured_only(a.ssh_policy),
        remote_config,
    }
}

fn to_session_response(s: RemoteSession) -> SessionResponse {
    SessionResponse {
        id: s.id.map(|i| i.to_hex()).unwrap_or_default(),
        agent_id: s.agent_id.to_hex(),
        tenant_id: s.tenant_id.to_hex(),
        controller_user_id: s.controller_user_id.to_hex(),
        permissions: s.permissions,
        phase: s.phase,
        created_at: fmt_dt(s.created_at),
        started_at: s.started_at.map(fmt_dt),
        ended_at: s.ended_at.map(fmt_dt),
    }
}

fn fmt_dt(dt: DateTime) -> String {
    dt.try_to_rfc3339_string()
        .unwrap_or_else(|_| dt.timestamp_millis().to_string())
}

/// Validate + canonicalize the subnet-route CIDRs an admin assigns to an agent
/// for the mesh subnet-router (Phase 2). Every entry must be valid CIDR
/// notation — IPv4 or IPv6, e.g. `10.66.24.0/24` or a single host
/// `10.66.24.53/32`. Host bits are masked to the network address, blank entries
/// are dropped, and duplicates removed. A bare IP or any unparseable entry
/// fails the whole request with 400 so the admin UI shows a clear error instead
/// of silently storing a route the mesh client would skip. Mirrors the
/// client-side parse in `roomler-tunnel`'s `mesh.rs` (both use `ipnet::IpNet`).
fn normalize_routes(raw: Vec<String>) -> Result<Vec<String>, ApiError> {
    use std::str::FromStr;
    const MAX_ROUTES: usize = 64;
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let net = ipnet::IpNet::from_str(trimmed).map_err(|_| {
            ApiError::BadRequest(format!(
                "Invalid route CIDR '{trimmed}' — use CIDR notation, \
                 e.g. 10.66.24.0/24 or a single host 10.66.24.53/32"
            ))
        })?;
        let canonical = net.trunc().to_string();
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    if out.len() > MAX_ROUTES {
        return Err(ApiError::BadRequest(format!(
            "Too many routes ({}); max {MAX_ROUTES}",
            out.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod remote_config_state_tests {
    use super::{RemoteConfigState, remote_config_view};
    use roomler_ai_remote_control::models::{
        AgentCaps, ConfigOutcome, ConfigReport, DesiredConfig, RpcCap,
    };

    fn caps(verbs: &[RpcCap]) -> AgentCaps {
        AgentCaps {
            rpc: verbs.iter().map(|v| v.wire().to_string()).collect(),
            ..Default::default()
        }
    }
    fn modern() -> AgentCaps {
        caps(&[RpcCap::Config, RpcCap::ConfigReport])
    }
    fn desired(revision: u64) -> DesiredConfig {
        DesiredConfig {
            exec_enabled: Some(true),
            revision,
            ..Default::default()
        }
    }
    fn report(revision: u64, outcome: ConfigOutcome, needs_restart: &[&str]) -> ConfigReport {
        ConfigReport {
            revision,
            outcome,
            live: vec![],
            needs_restart: needs_restart.iter().map(|s| s.to_string()).collect(),
            detail: None,
            reported_at: bson::DateTime::now(),
        }
    }
    fn state(
        d: DesiredConfig,
        r: Option<ConfigReport>,
        c: &AgentCaps,
    ) -> Option<RemoteConfigState> {
        remote_config_view(d, r, c).map(|v| v.state)
    }

    #[test]
    fn nothing_requested_is_not_a_state_at_all() {
        assert!(remote_config_view(DesiredConfig::default(), None, &modern()).is_none());
    }

    /// The comparison this whole view exists for. A device that answered about
    /// revision 3 has said NOTHING about revision 4 — reading its old
    /// `outcome` would show a stale "applied" over a change that has not
    /// landed, which is the most dangerous wrong answer available here.
    #[test]
    fn a_report_about_an_older_revision_is_not_an_answer() {
        assert_eq!(
            state(
                desired(4),
                Some(report(3, ConfigOutcome::Applied, &[])),
                &modern()
            ),
            Some(RemoteConfigState::Pending)
        );
        // …and the stale report is still RETURNED, so a UI can say what the
        // device last confirmed. It just doesn't decide the state.
        let view = remote_config_view(
            desired(4),
            Some(report(3, ConfigOutcome::Applied, &[])),
            &modern(),
        )
        .unwrap();
        assert_eq!(view.report.map(|r| r.revision), Some(3));
    }

    #[test]
    fn applied_and_noop_both_mean_converged() {
        for outcome in [ConfigOutcome::Applied, ConfigOutcome::Noop] {
            assert_eq!(
                state(desired(1), Some(report(1, outcome, &[])), &modern()),
                Some(RemoteConfigState::Applied),
                "{outcome:?}"
            );
        }
    }

    /// `needs_restart` must WIN over `applied`. The keys in that list are
    /// written to disk and not in force; calling the device "applied" would
    /// tell an operator SSH is open while it refuses every session.
    #[test]
    fn a_pending_restart_is_never_reported_as_applied() {
        assert_eq!(
            state(
                desired(1),
                Some(report(1, ConfigOutcome::Applied, &["ssh_enabled"])),
                &modern()
            ),
            Some(RemoteConfigState::NeedsRestart)
        );
    }

    #[test]
    fn both_refusals_are_refusals_and_a_failure_is_not() {
        for outcome in [ConfigOutcome::NotOptedIn, ConfigOutcome::NotPrimary] {
            assert_eq!(
                state(desired(1), Some(report(1, outcome, &[])), &modern()),
                Some(RemoteConfigState::Refused),
                "{outcome:?}"
            );
        }
        assert_eq!(
            state(
                desired(1),
                Some(report(1, ConfigOutcome::Failed, &[])),
                &modern()
            ),
            Some(RemoteConfigState::Failed)
        );
    }

    /// Silence means three different things, and only one of them is worth
    /// waiting through. Collapsing them into "pending" would leave an operator
    /// watching a spinner for an answer that is never coming.
    #[test]
    fn silence_is_disambiguated_by_what_the_device_can_do() {
        // Modern agent, no answer yet — genuinely pending.
        assert_eq!(
            state(desired(1), None, &modern()),
            Some(RemoteConfigState::Pending)
        );
        // rc.457/rc.458-era: applies pushed config, never reports. Waiting is
        // futile; the fix is an update.
        assert_eq!(
            state(desired(1), None, &caps(&[RpcCap::Config])),
            Some(RemoteConfigState::ReportsUnsupported)
        );
        // Predates the push entirely — the server does not even send it.
        assert_eq!(
            state(desired(1), None, &caps(&[RpcCap::Exec])),
            Some(RemoteConfigState::PushUnsupported)
        );
    }

    /// `config` is a PREFIX of `config-report`. A device advertising only
    /// `config` must not be read as report-capable, or every rc.458 device in
    /// the fleet shows "pending" forever.
    #[test]
    fn the_config_prefix_does_not_imply_reporting() {
        let old = caps(&[RpcCap::Config]);
        assert!(old.has_rpc(RpcCap::Config));
        assert!(!old.has_rpc(RpcCap::ConfigReport));
        assert_eq!(
            state(desired(1), None, &old),
            Some(RemoteConfigState::ReportsUnsupported)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_routes;

    #[test]
    fn normalize_routes_canonicalizes_and_dedups() {
        // host bits masked to the network; duplicate collapsed; blanks dropped
        let out = normalize_routes(vec![
            "10.66.24.53/24".to_string(),
            "  ".to_string(),
            "10.66.24.0/24".to_string(),
            "192.168.1.0/24".to_string(),
        ])
        .unwrap();
        assert_eq!(out, vec!["10.66.24.0/24", "192.168.1.0/24"]);
    }

    #[test]
    fn normalize_routes_accepts_host_route_and_ipv6() {
        let out = normalize_routes(vec![
            "10.66.24.53/32".to_string(),
            "2001:db8::/32".to_string(),
        ])
        .unwrap();
        assert_eq!(out, vec!["10.66.24.53/32", "2001:db8::/32"]);
    }

    #[test]
    fn normalize_routes_rejects_bare_ip_and_garbage() {
        // a bare IP (no prefix) is rejected — CIDR notation is required so the
        // stored value always parses on the mesh client side.
        assert!(normalize_routes(vec!["10.66.24.53".to_string()]).is_err());
        assert!(normalize_routes(vec!["not-a-cidr".to_string()]).is_err());
        assert!(normalize_routes(vec!["10.66.24.0/33".to_string()]).is_err());
    }

    #[test]
    fn normalize_routes_empty_is_ok() {
        assert_eq!(normalize_routes(vec![]).unwrap(), Vec::<String>::new());
        assert_eq!(
            normalize_routes(vec!["".to_string(), "   ".to_string()]).unwrap(),
            Vec::<String>::new()
        );
    }
}
