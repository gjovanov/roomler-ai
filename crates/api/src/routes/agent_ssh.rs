//! Roomler SSH — the server side.
//!
//! This module is the POLICY DECISION POINT, the twin of [`agent_exec`]. The
//! agent decides what happens on the box (its own `ssh_enabled` key, the grant
//! bounds, what a session may do); everything about *who may ask* is decided
//! here, in one place.
//!
//! ## The four gates
//!
//! A session is authorized only if all four pass, each owned by a different
//! party so no single compromise is sufficient:
//!
//! 1. `TenantSettings.remote_ssh_enabled` — the org kill-switch, default off,
//!    and deliberately NOT the same switch as `remote_exec_enabled`.
//! 2. `permissions::SSH_DEVICE` on the caller's role. Not implied by
//!    `MANAGE_AGENTS` and not part of `DEFAULT_ADMIN`.
//! 3. The device's `SshPolicy` — mode, allowed users/roles, and (for the
//!    device-originated leg) `can_originate` on the ORIGINATING device.
//! 4. The agent's own `ssh_enabled` config key, enforced agent-side.
//!
//! Gates 1-3 are evaluated by [`authorize`]. Gate 4 cannot be reported back
//! here at all, which is the structural difference from exec: the session runs
//! over a path the server is not on.
//!
//! ## Why this hands out a key instead of a command
//!
//! Exec pushes a command and waits for the output — the server sees
//! everything. SSH cannot work that way and should not: the whole point is a
//! session the server never observes. So the server's role ends at
//! authorization. It mints a **grant** — the caller's ephemeral public key,
//! the principal's name, the account, an expiry — pushes it to the target, and
//! tells the caller where to dial. What passes over that connection is between
//! the two devices.
//!
//! That also means a refusal here is the LAST word the server gets. There is
//! no equivalent of exec's `error` coming back from the device, so every
//! reason a request can fail is enumerated in [`SshDenyReason`] and answered
//! synchronously.
//!
//! ## Privilege
//!
//! A session runs as what the policy actually asked for, never more:
//! `SshPolicy.account_mode` crosses to the device in the grant and the agent's
//! `RunAs` honours it or refuses (P5a/P5b). `daemon` still means SYSTEM under a
//! perMachine Windows install and root under systemd — so a policy of `daemon`
//! is granting root on that device, deliberately and in writing.
//!
//! ## Consent
//!
//! `SshPolicy.consent_mode` decides whether a human at the device approves
//! before anything runs. Only `auto` skips the prompt. This route refuses to
//! STORE a non-auto value for a device whose agent predates P5d
//! ([`consent_mode_would_be_ignored`]) — such an agent accepts the field and
//! ignores it, and a policy that reads as enforced while doing nothing is worse
//! than no policy, because it gets relied upon.
//!
//! [`agent_exec`]: super::agent_exec

use axum::{
    Json,
    extract::{Path, State},
};
use bson::oid::ObjectId;
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::{
    models::{
        Agent, ConsentMode, RpcCap, SshAccountMode, SshDenyReason, SshGates, SshGrantSpec, SshMode,
        SshPolicy, ssh_limits,
    },
    signaling::ServerMsg,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{
    error::ApiError, extractors::auth::AuthUser, routes::remote_control::require_permission,
    state::AppState,
};

// ────────────────────────────────────────────────────────────────────────────
// Wire shapes
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SshRequestBody {
    /// OpenSSH public key of the caller's EPHEMERAL session keypair
    /// (`ssh-ed25519 AAAA… comment`). The private half never leaves the
    /// caller; this is the only thing the server or the target ever sees.
    pub public_key: String,
    /// Requested session lifetime in seconds. Clamped server-side; 0 or absent
    /// means the ceiling.
    #[serde(default)]
    pub session_secs: u64,
}

#[derive(Debug, Serialize)]
pub struct SshResponseBody {
    /// Where to dial. Absent on refusal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The device's MagicDNS name, for display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    /// Unix ms after which the grant is dead — dial before this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// Set when the request was refused, naming which gate said no.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SshResponseBody {
    fn denied(reason: SshDenyReason) -> Self {
        Self {
            address: None,
            port: None,
            name: None,
            grant_id: None,
            expires_at_ms: None,
            error: Some(reason.message().to_string()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SshPolicyBody {
    #[serde(default)]
    pub mode: SshMode,
    #[serde(default)]
    pub can_originate: bool,
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub allowed_role_ids: Vec<String>,
    #[serde(default)]
    pub account_mode: SshAccountMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent_mode: Option<ConsentMode>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OrgSshSettings {
    pub remote_ssh_enabled: bool,
}

/// The acting principal, and how it reached us.
pub struct Caller {
    pub user_id: ObjectId,
    pub display: String,
    /// Set on the device-originated leg — the device whose LocalAPI asked.
    pub origin_agent_id: Option<ObjectId>,
}

// ────────────────────────────────────────────────────────────────────────────
// Authorization
// ────────────────────────────────────────────────────────────────────────────

/// Evaluate gates 1-3. `Ok(())` means a grant may be minted.
///
/// Split out from [`dispatch`] so the whole decision is testable on its own
/// and so no path can skip a gate by taking a different route into the mint.
///
/// Takes the TARGET's [`SshGates`] — the authorize-time half of its policy,
/// produced by [`SshPolicy::split`] — rather than the policy itself, so this
/// function cannot quietly ignore a policy field: the split routes every
/// field, and the exhaustive destructure below has to bind every gate.
pub async fn authorize(
    state: &AppState,
    tenant_id: ObjectId,
    gates: &SshGates,
    caller: &Caller,
) -> Result<(), SshDenyReason> {
    // EXHAUSTIVE — a gate added to `SshGates` must be bound (and, under
    // `-D warnings`, used) here before this compiles again.
    let SshGates {
        mode,
        can_originate,
        allowed_user_ids,
        allowed_role_ids,
    } = gates;
    // Two fields are consulted elsewhere, in writing: the TARGET's
    // `can_originate` is not an inbound gate (it is read off the ORIGIN
    // device's row below), and the user allowlist is consulted through
    // `allows_caller`. Consumed here so the destructure stays exhaustive and
    // a NEW gate cannot borrow this line as precedent for being skipped.
    let _ = (can_originate, allowed_user_ids);

    // Gate 1 — the org kill-switch. First because it is the cheapest check and
    // the most likely reason a whole fleet says no.
    let tenant = match state.tenants.base.find_by_id(tenant_id).await {
        Ok(t) => t,
        Err(_) => return Err(SshDenyReason::OrgDisabled),
    };
    if !tenant.settings.remote_ssh_enabled {
        return Err(SshDenyReason::OrgDisabled);
    }

    // Gate 2 — the caller's role.
    let perms = state
        .tenants
        .get_member_permissions(tenant_id, caller.user_id)
        .await
        .unwrap_or(0);
    if !permissions::has(perms, permissions::SSH_DEVICE) {
        return Err(SshDenyReason::NoPermission);
    }

    // Gate 3 — the target device's own policy.
    if *mode != SshMode::On {
        return Err(SshDenyReason::DeviceDisabled);
    }
    // Role ids are only fetched when the policy actually restricts by role —
    // an extra query on every SSH request would be paid by the common case to
    // serve the rare one.
    let role_ids = if allowed_role_ids.is_empty() {
        Vec::new()
    } else {
        state
            .tenants
            .member_role_ids(tenant_id, caller.user_id)
            .await
            .unwrap_or_default()
    };
    if !gates.allows_caller(&caller.user_id, &role_ids) {
        return Err(SshDenyReason::CallerNotAllowed);
    }

    // The device-originated leg only: the ORIGINATING device must be blessed.
    // Without this, compromising any enrolled laptop would inherit its owner's
    // SSH rights across the whole fleet.
    if let Some(origin) = caller.origin_agent_id {
        match state.agents.find_in_tenant(tenant_id, origin).await {
            Ok(a) if a.ssh_policy.can_originate => {}
            _ => return Err(SshDenyReason::OriginNotAllowed),
        }
    }

    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Dispatch
// ────────────────────────────────────────────────────────────────────────────

/// Gate, mint, push, answer. The ONE path a session request takes, whether it
/// came from the browser, the API, or a device's LocalAPI.
pub async fn dispatch(
    state: &AppState,
    tenant_id: ObjectId,
    agent: &Agent,
    caller: &Caller,
    public_key: &str,
    session_secs: u64,
) -> SshResponseBody {
    let agent_id = agent.id.unwrap_or_default();

    // Validate the key BEFORE consulting policy: a caller who sent garbage
    // should be told that, not told their permissions are wrong.
    if !is_supported_public_key(public_key) {
        return deny(state, agent_id, caller, SshDenyReason::BadPublicKey).await;
    }

    // The ONE place the target's policy is consumed: `split` routes every
    // field into either the authorize gates or the grant spec, so a new
    // policy field cannot be silently ignored by either half.
    let (gates, spec) = agent.ssh_policy.clone().split();

    if let Err(reason) = authorize(state, tenant_id, &gates, caller).await {
        return deny(state, agent_id, caller, reason).await;
    }

    // Where to send them. A device can pass every gate and still be
    // unreachable — that is a different failure and says so.
    let node = match state
        .overlay_nodes
        .find_live_by_agent(tenant_id, agent_id)
        .await
    {
        Ok(Some(n)) if !n.overlay_ip.is_empty() => n,
        _ => return deny(state, agent_id, caller, SshDenyReason::NoOverlayAddress).await,
    };

    let grant_id = ObjectId::new().to_hex();
    let session_secs = ssh_limits::clamp_session_secs(session_secs);
    let expires_at_ms = now_ms() + ssh_limits::GRANT_TTL_SECS * 1000;

    // EXHAUSTIVE — the spec's fields map 1:1 onto the wire, and binding them
    // all here is what keeps a field routed into the spec from being dropped
    // on its way into the grant.
    let SshGrantSpec {
        account_mode,
        account,
        consent_mode,
    } = spec;
    let msg = ServerMsg::SshGrant {
        grant_id: grant_id.clone(),
        public_key: public_key.to_string(),
        caller: caller.display.clone(),
        account_mode: account_mode_wire(account_mode).to_string(),
        account,
        expires_at_ms,
        session_secs,
        consent_mode: Some(effective_wire_consent(consent_mode)),
    };

    if let Err(e) = state.rc_hub.push_ssh_grant(agent_id, tenant_id, msg) {
        // The hub distinguishes "not connected" from "connected but cannot
        // honour it", and the caller needs that difference: one is wait and
        // retry, the other is upgrade the agent.
        let reason = match e {
            roomler_ai_remote_control::error::Error::ExecUnsupported(_) => {
                SshDenyReason::Unsupported
            }
            _ => SshDenyReason::Offline,
        };
        return deny(state, agent_id, caller, reason).await;
    }

    info!(
        agent = %agent_id, caller = %caller.display, %grant_id,
        account_mode = ?account_mode, session_secs,
        "ssh: grant issued"
    );

    SshResponseBody {
        address: Some(node.overlay_ip.clone()),
        // The port the device intercepts is agent-side config, and the server
        // does not carry it. The built-in default is what an unconfigured
        // device serves; a device that moved its port tells its own users.
        port: Some(DEFAULT_SSH_PORT),
        name: (!node.name.is_empty()).then(|| node.name.clone()),
        grant_id: Some(grant_id),
        expires_at_ms: Some(expires_at_ms),
        error: None,
    }
}

/// Built-in intercepted SSH port, mirroring the agent's
/// `agent_core::config::DEFAULT_SSH_PORT`. Duplicated rather than imported
/// because the API crate must not depend on the agent's crate — the same
/// reason `agent_exec` duplicates the consent timeout.
const DEFAULT_SSH_PORT: u16 = 2222;

/// Log the refusal and shape the answer.
///
/// Every denial is logged at WARN with the device and the principal. Auditing
/// to a collection the way exec does is the next slice; a refusal that leaves
/// no trace at all is how someone probes which devices will let them in.
async fn deny(
    _state: &AppState,
    agent_id: ObjectId,
    caller: &Caller,
    reason: SshDenyReason,
) -> SshResponseBody {
    warn!(
        agent = %agent_id, caller = %caller.display, ?reason,
        "ssh: request denied"
    );
    SshResponseBody::denied(reason)
}

/// Is this an OpenSSH public key we will hand to a device?
///
/// ed25519 only, matching what the agent's host key and the whole design use.
/// Checked here as well as agent-side so a typo is a clear 200-with-error from
/// the API rather than a session that mysteriously never authenticates.
fn is_supported_public_key(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(algo) = parts.next() else {
        return false;
    };
    let Some(blob) = parts.next() else {
        return false;
    };
    algo == "ssh-ed25519" && blob.len() >= 32 && blob.chars().all(|c| c.is_ascii_graphic())
}

/// What the grant tells the device about consent. Only an explicit `Auto`
/// crosses as `Auto`; everything else — the session-shaped Email/Push and an
/// UNSET policy alike — collapses to `Prompt`, the attended default (the same
/// collapse `ExecPolicy::effective_consent_mode` applies). The device treats
/// the wire value as authoritative, so an accidental `Auto` here would skip a
/// human the admin believed was in the loop.
fn effective_wire_consent(mode: Option<ConsentMode>) -> ConsentMode {
    match mode {
        Some(ConsentMode::Auto) => ConsentMode::Auto,
        _ => ConsentMode::Prompt,
    }
}

/// Wire spelling of an account mode — snake_case, matching the enum's serde.
fn account_mode_wire(mode: SshAccountMode) -> &'static str {
    match mode {
        SshAccountMode::Daemon => "daemon",
        SshAccountMode::ConsoleUser => "console_user",
        SshAccountMode::Named => "named",
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers shared with the handlers
// ────────────────────────────────────────────────────────────────────────────

async fn tenant_of(tenant_id: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(tenant_id).map_err(|_| ApiError::BadRequest("Invalid tenant id".into()))
}

async fn load_agent(
    state: &AppState,
    tenant_id: ObjectId,
    agent_id: &str,
) -> Result<Agent, ApiError> {
    let aid = ObjectId::parse_str(agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent id".into()))?;
    state
        .agents
        .find_in_tenant(tenant_id, aid)
        .await
        .map_err(|_| ApiError::NotFound("Agent not found".into()))
}

async fn http_caller(state: &AppState, auth: &AuthUser) -> Caller {
    let display = state
        .users
        .base
        .find_by_id(auth.user_id)
        .await
        .map(|u| u.display_name)
        .unwrap_or_else(|_| auth.user_id.to_hex());
    Caller {
        user_id: auth.user_id,
        display,
        origin_agent_id: None,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Handlers
// ────────────────────────────────────────────────────────────────────────────

/// `POST /api/tenant/{tenant_id}/agent/{agent_id}/ssh` — ask for a session.
///
/// Answers 200 with either where to dial or why not: a refusal is a policy
/// outcome, not a transport failure, and the caller needs to read the reason.
pub async fn request_session(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Json(body): Json<SshRequestBody>,
) -> Result<Json<SshResponseBody>, ApiError> {
    let tid = tenant_of(&tenant_id).await?;
    let agent = load_agent(&state, tid, &agent_id).await?;
    let caller = http_caller(&state, &auth).await;
    Ok(Json(
        dispatch(
            &state,
            tid,
            &agent,
            &caller,
            &body.public_key,
            body.session_secs,
        )
        .await,
    ))
}

/// `PUT /api/tenant/{tenant_id}/agent/{agent_id}/ssh-policy` — gate 3.
///
/// `MANAGE_AGENTS`, not `SSH_DEVICE`: deciding a device may be SSHed into is a
/// management act, distinct from being allowed to do it. Same split exec uses.
pub async fn set_policy(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Json(body): Json<SshPolicyBody>,
) -> Result<Json<SshPolicyBody>, ApiError> {
    let tid = tenant_of(&tenant_id).await?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;
    let agent = load_agent(&state, tid, &agent_id).await?;

    let parse_ids = |v: &[String], what: &str| -> Result<Vec<ObjectId>, ApiError> {
        v.iter()
            .map(|s| {
                ObjectId::parse_str(s)
                    .map_err(|_| ApiError::BadRequest(format!("Invalid {what}: {s}")))
            })
            .collect()
    };

    // A named account with no name is a policy that cannot be satisfied;
    // refuse it here rather than let a device fail every session later.
    if body.account_mode == SshAccountMode::Named
        && body.account.as_deref().unwrap_or("").trim().is_empty()
    {
        return Err(ApiError::BadRequest(
            "account_mode `named` requires an account".into(),
        ));
    }

    // Same rule, applied to consent: an agent older than P5d accepts a
    // `consent_mode` and ignores it. Storing one would hand the admin a policy
    // that reads as "a human must approve" while that device keeps letting
    // every session straight through — which is the precise defect P5d exists
    // to remove, so the server must not be able to recreate it per-device.
    if consent_mode_would_be_ignored(&agent.capabilities, body.consent_mode) {
        return Err(ApiError::BadRequest(format!(
            "this device's agent does not honour consent_mode yet (needs the P5d agent); \
             it would accept `{:?}` and ignore it. Update the device, or set `auto`.",
            body.consent_mode.unwrap_or(ConsentMode::Auto)
        )));
    }

    let policy = SshPolicy {
        mode: body.mode,
        can_originate: body.can_originate,
        allowed_user_ids: parse_ids(&body.allowed_user_ids, "user id")?,
        allowed_role_ids: parse_ids(&body.allowed_role_ids, "role id")?,
        account_mode: body.account_mode,
        account: body.account.clone(),
        consent_mode: body.consent_mode,
    };
    state
        .agents
        .update_ssh_policy(tid, agent.id.unwrap_or_default(), &policy)
        .await?;
    warn!(
        agent = %agent_id, admin = %auth.user_id, mode = ?policy.mode,
        account_mode = ?policy.account_mode,
        "ssh: device policy updated"
    );
    Ok(Json(body))
}

/// `GET /api/tenant/{tenant_id}/ssh-settings` — gate 1's current state, so the
/// device console can explain why every device is refusing before anyone
/// starts editing per-device policy.
pub async fn get_org_settings(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<OrgSshSettings>, ApiError> {
    let tid = tenant_of(&tenant_id).await?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;
    let tenant = state
        .tenants
        .base
        .find_by_id(tid)
        .await
        .map_err(|_| ApiError::NotFound("Tenant not found".into()))?;
    Ok(Json(OrgSshSettings {
        remote_ssh_enabled: tenant.settings.remote_ssh_enabled,
    }))
}

/// `PUT /api/tenant/{tenant_id}/ssh-settings` — flip gate 1.
///
/// `MANAGE_TENANT`, the highest bar of the three, because this one switch
/// governs the whole org.
pub async fn set_org_settings(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<OrgSshSettings>,
) -> Result<Json<OrgSshSettings>, ApiError> {
    let tid = tenant_of(&tenant_id).await?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_TENANT,
        "MANAGE_TENANT",
    )
    .await?;
    let tenant = state
        .tenants
        .set_remote_ssh_enabled(tid, body.remote_ssh_enabled)
        .await?;
    warn!(
        tenant = %tenant_id, admin = %auth.user_id,
        enabled = body.remote_ssh_enabled,
        "ssh: org kill-switch changed"
    );
    Ok(Json(OrgSshSettings {
        remote_ssh_enabled: tenant.settings.remote_ssh_enabled,
    }))
}

/// Would storing this `consent_mode` produce a policy the target device
/// silently ignores?
///
/// Agents rc.419 and earlier advertise `ssh` while destructuring
/// `SshPolicy.consent_mode` away, so persisting a non-auto value for one would
/// show an admin a rule reading "a human must approve" while that device let
/// every session straight through. P5d fixed the agent; this keeps the server
/// from recreating the same lie on any device that hasn't updated yet.
///
/// `auto` and an absent value are always storable: neither promises a prompt.
fn consent_mode_would_be_ignored(
    caps: &roomler_ai_remote_control::models::AgentCaps,
    mode: Option<ConsentMode>,
) -> bool {
    !matches!(mode, None | Some(ConsentMode::Auto)) && !caps.has_rpc(RpcCap::SshConsent)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy that promises operator approval may only be stored for a device
    /// that will actually ask. Otherwise the admin is handed a lie — the exact
    /// defect P5d removed from the agent.
    #[test]
    fn a_consent_policy_is_refused_for_a_device_that_would_ignore_it() {
        use roomler_ai_remote_control::models::AgentCaps;

        let caps = |verbs: &[&str]| AgentCaps {
            rpc: verbs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let p5d = caps(&["exec", "originate", "ssh", "ssh-consent"]);
        // rc.419 and earlier: advertises `ssh`, ignores consent_mode.
        let older = caps(&["exec", "originate", "ssh"]);

        for m in [
            Some(ConsentMode::Prompt),
            Some(ConsentMode::Email),
            Some(ConsentMode::Push),
        ] {
            assert!(
                consent_mode_would_be_ignored(&older, m),
                "{m:?} must be refused for an agent that would ignore it"
            );
            assert!(
                !consent_mode_would_be_ignored(&p5d, m),
                "{m:?} must be storable once the device honours it"
            );
        }

        // Neither of these promises a prompt, so both are always storable —
        // including for an agent too old to advertise anything at all.
        for m in [None, Some(ConsentMode::Auto)] {
            assert!(!consent_mode_would_be_ignored(&older, m));
            assert!(!consent_mode_would_be_ignored(&caps(&[]), m));
        }
    }

    /// The wire may carry `Auto` only when the policy said `Auto` in so many
    /// words. Everything else — including a policy that said nothing — must
    /// cross as `Prompt`.
    #[test]
    fn only_an_explicit_auto_crosses_the_wire_as_auto() {
        assert_eq!(
            effective_wire_consent(Some(ConsentMode::Auto)),
            ConsentMode::Auto
        );
        for m in [
            None,
            Some(ConsentMode::Prompt),
            Some(ConsentMode::Email),
            Some(ConsentMode::Push),
            Some(ConsentMode::PromptThenEmail),
        ] {
            assert_eq!(effective_wire_consent(m), ConsentMode::Prompt, "{m:?}");
        }
    }

    #[test]
    fn only_ed25519_keys_are_accepted() {
        assert!(is_supported_public_key(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyMaterialHere00 goran@neo16"
        ));
        // No comment is fine — `ssh-keygen -y` emits exactly this.
        assert!(is_supported_public_key(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyMaterialHere00"
        ));

        // RSA is refused on purpose: the `rsa` feature is off in the agent, so
        // accepting one here would mint a grant no device could ever redeem.
        assert!(!is_supported_public_key(
            "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQExampleKeyMaterial00 goran@neo16"
        ));
        assert!(!is_supported_public_key("ssh-ed25519"));
        assert!(!is_supported_public_key(""));
        assert!(!is_supported_public_key("ssh-ed25519 short"));
        // Whitespace or control characters in the blob would end up in the
        // agent's config-shaped parsing; reject rather than pass through.
        assert!(!is_supported_public_key(
            "ssh-ed25519 AAAA\u{7f}BBBB0000000000000000000000000000"
        ));
    }

    #[test]
    fn account_modes_use_the_enum_wire_spelling() {
        // These strings cross to the agent and are matched there; if the enum
        // gains a variant, this is what fails rather than a device silently
        // treating an unknown mode as the default.
        assert_eq!(account_mode_wire(SshAccountMode::Daemon), "daemon");
        assert_eq!(
            account_mode_wire(SshAccountMode::ConsoleUser),
            "console_user"
        );
        assert_eq!(account_mode_wire(SshAccountMode::Named), "named");
        for m in [
            SshAccountMode::Daemon,
            SshAccountMode::ConsoleUser,
            SshAccountMode::Named,
        ] {
            let json = serde_json::to_string(&m).unwrap();
            assert_eq!(json.trim_matches('"'), account_mode_wire(m));
        }
    }

    #[test]
    fn every_deny_reason_says_which_gate_refused() {
        for r in [
            SshDenyReason::OrgDisabled,
            SshDenyReason::NoPermission,
            SshDenyReason::DeviceDisabled,
            SshDenyReason::CallerNotAllowed,
            SshDenyReason::OriginNotAllowed,
            SshDenyReason::Unsupported,
            SshDenyReason::Offline,
            SshDenyReason::NoOverlayAddress,
            SshDenyReason::RateLimited,
            SshDenyReason::BadPublicKey,
        ] {
            let m = r.message();
            assert!(!m.is_empty(), "{r:?} has no message");
            assert!(
                m.len() > 20,
                "{r:?}'s message is too terse to act on: {m:?}"
            );
        }
    }
}
