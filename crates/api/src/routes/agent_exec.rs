//! Fleet RPC — the server side of remote command execution.
//!
//! This module is the POLICY DECISION POINT. The agent decides what happens on
//! the box (bounds, redaction, its own `exec_enabled` gate); everything about
//! *who may ask* is decided here, in one place, and written to `exec_audit`
//! whichever way it goes.
//!
//! ## The four gates
//!
//! A request runs only if all four pass. Each is owned by a different party,
//! so no single compromise is sufficient:
//!
//! 1. `TenantSettings.remote_exec_enabled` — the org kill-switch, default off.
//! 2. `permissions::EXEC_DEVICE` on the caller's role. Deliberately not
//!    implied by `MANAGE_AGENTS`, and not part of `DEFAULT_ADMIN`.
//! 3. The device's `ExecPolicy` — mode, allowed users/roles, allowed shells,
//!    and (for the CLI leg) `can_originate` on the ORIGINATING device.
//! 4. The agent's own `exec_enabled` config key, enforced agent-side.
//!
//! Gates 1–3 are evaluated by [`authorize`]; gate 4 comes back as an
//! `ExecOutcome::error`, which is why a refusal is still a 200 with an error
//! body rather than a transport failure.
//!
//! ## Why refusals are audited
//!
//! Every path through [`dispatch`] writes a row. A denied exec is the
//! interesting one — without the row, someone probing which devices will run
//! things for them leaves no trace at all.
//!
//! ## Privilege
//!
//! Commands inherit the daemon's identity: SYSTEM under a perMachine Windows
//! install, root under systemd. Enabling gate 3 on a device is granting root
//! on it, and the admin UI says so in those words.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::{
    error::Error as HubError,
    models::{
        Agent, ExecAuditEvent, ExecDenyReason, ExecMode, ExecOutcome, ExecPolicy, exec_limits,
    },
    signaling::ServerMsg,
};
use roomler_ai_services::dao::base::PaginationParams;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::{
    error::ApiError, extractors::auth::AuthUser, routes::remote_control::require_permission,
    state::AppState,
};

/// How long the server waits for a device's answer beyond the command's own
/// budget. Generous on purpose: the agent answers its OWN timeout, and
/// "timed out after 30000ms" from the device is strictly more useful than the
/// server giving up first and reporting nothing about what happened.
const RESULT_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// Extra patience for a device whose policy PROMPTS: the agent waits for a
/// human before the command's own budget even starts.
///
/// Mirrors `roomler_agent::consent::DEFAULT_PROMPT_TIMEOUT` (30 s), duplicated
/// rather than imported because the API crate must not depend on the agent
/// binary's crate. Without it, an attended device would routinely report a
/// false "no answer within Ns" while the command was in fact running — the
/// worst kind of diagnostic, one that lies about the thing being diagnosed.
const CONSENT_GRACE: std::time::Duration = std::time::Duration::from_secs(35);

/// Cap on a bulk fan-out, so one request can't tie up every worker.
const MAX_BULK_TARGETS: usize = 64;
/// How many devices a bulk request talks to at once.
const BULK_CONCURRENCY: usize = 8;

// ────────────────────────────────────────────────────────────────────────────
// Wire types
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ExecRequestBody {
    /// `pwsh` | `powershell` | `cmd` | `bash` | `sh`. Empty ⇒ the host default.
    #[serde(default)]
    pub shell: String,
    pub command: String,
    /// 0 ⇒ the built-in default; clamped to [`exec_limits::MAX_TIMEOUT_MS`].
    #[serde(default)]
    pub timeout_ms: u64,
    /// 0 ⇒ the built-in default; clamped to [`exec_limits::MAX_OUTPUT_BYTES`].
    #[serde(default)]
    pub max_output_bytes: u64,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExecResponse {
    /// Echoed so the caller can address a cancel.
    pub request_id: String,
    pub agent_id: String,
    pub agent_name: String,
    #[serde(flatten)]
    pub outcome: ExecOutcome,
}

#[derive(Debug, Deserialize)]
pub struct BulkExecRequestBody {
    /// Devices to run on, by hex id. Empty ⇒ every device in the org whose
    /// `ExecPolicy` is on — "run this everywhere it's allowed", which is the
    /// fleet-sweep the feature exists for.
    #[serde(default)]
    pub agent_ids: Vec<String>,
    #[serde(flatten)]
    pub exec: ExecRequestBody,
}

#[derive(Debug, Serialize)]
pub struct BulkExecResponse {
    pub results: Vec<ExecResponse>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExecPolicyBody {
    #[serde(default)]
    pub mode: ExecMode,
    #[serde(default)]
    pub can_originate: bool,
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub allowed_role_ids: Vec<String>,
    #[serde(default)]
    pub consent_mode: Option<roomler_ai_remote_control::models::ConsentMode>,
    #[serde(default)]
    pub shells: Vec<String>,
}

/// One audit row as the API renders it.
///
/// Deliberately NOT `ExecAuditEvent` straight off the wire: bson's `ObjectId`
/// and `DateTime` serialise to extended JSON (`{"$oid": …}` / `{"$date": …}`),
/// which no client here parses. Every other route in this codebase projects
/// through a DTO with hex ids + RFC3339 timestamps, and this one has to as
/// well or the audit table renders `[object Object]`.
#[derive(Debug, Serialize)]
pub struct ExecAuditRow {
    pub id: String,
    pub tenant_id: String,
    pub agent_id: String,
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_agent_id: Option<String>,
    pub request_id: String,
    pub source: String,
    pub shell: String,
    pub command: String,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied: Option<ExecDenyReason>,
    pub output_sample: String,
    pub output_sha256: String,
    pub output_bytes: u64,
    pub truncated: bool,
}

impl From<ExecAuditEvent> for ExecAuditRow {
    fn from(e: ExecAuditEvent) -> Self {
        Self {
            id: e.id.map(|i| i.to_hex()).unwrap_or_default(),
            tenant_id: e.tenant_id.to_hex(),
            agent_id: e.agent_id.to_hex(),
            user_id: e.user_id.to_hex(),
            origin_agent_id: e.origin_agent_id.map(|i| i.to_hex()),
            request_id: e.request_id,
            source: e.source,
            shell: e.shell,
            command: e.command,
            at: e.at.try_to_rfc3339_string().unwrap_or_default(),
            exit_code: e.exit_code,
            duration_ms: e.duration_ms,
            denied: e.denied,
            output_sample: e.output_sample,
            output_sha256: e.output_sha256,
            output_bytes: e.output_bytes,
            truncated: e.truncated,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ExecAuditResponse {
    pub items: Vec<ExecAuditRow>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
}

// ────────────────────────────────────────────────────────────────────────────
// Authorization
// ────────────────────────────────────────────────────────────────────────────

/// The acting principal, and how it reached us.
pub struct Caller {
    pub user_id: ObjectId,
    pub display: String,
    /// Set on the CLI leg — the device whose LocalAPI originated the request.
    pub origin_agent_id: Option<ObjectId>,
    /// `cli` | `ui` | `api`.
    pub source: &'static str,
}

/// Evaluate gates 1–3. `Ok(())` means the command may be pushed; `Err` carries
/// the reason, which the caller must audit before returning it.
///
/// Split out from [`dispatch`] so the whole decision is testable as a table
/// and so no path can accidentally skip a gate by taking a different route
/// into the push.
pub async fn authorize(
    state: &AppState,
    tenant_id: ObjectId,
    agent: &Agent,
    caller: &Caller,
    shell: &str,
) -> Result<(), ExecDenyReason> {
    // Gate 1 — the org kill-switch. Checked first because it is the cheapest
    // and the most likely reason a fleet says no.
    let tenant = match state.tenants.base.find_by_id(tenant_id).await {
        Ok(t) => t,
        Err(_) => return Err(ExecDenyReason::OrgDisabled),
    };
    if !tenant.settings.remote_exec_enabled {
        return Err(ExecDenyReason::OrgDisabled);
    }

    // Gate 2 — the caller's role.
    let perms = state
        .tenants
        .get_member_permissions(tenant_id, caller.user_id)
        .await
        .unwrap_or(0);
    if !permissions::has(perms, permissions::EXEC_DEVICE) {
        return Err(ExecDenyReason::NoPermission);
    }

    // Gate 3 — the target device's own policy.
    let policy = &agent.exec_policy;
    if policy.mode != ExecMode::On {
        return Err(ExecDenyReason::DeviceDisabled);
    }
    if !policy.allowed_user_ids.is_empty() && !policy.allowed_user_ids.contains(&caller.user_id) {
        return Err(ExecDenyReason::CallerNotAllowed);
    }
    if !policy.allowed_role_ids.is_empty() {
        let role_ids = state
            .tenants
            .member_role_ids(tenant_id, caller.user_id)
            .await
            .unwrap_or_default();
        if !role_ids.iter().any(|r| policy.allowed_role_ids.contains(r)) {
            return Err(ExecDenyReason::CallerNotAllowed);
        }
    }
    if !policy.allows_shell(shell) {
        return Err(ExecDenyReason::ShellNotAllowed);
    }

    // The CLI leg only: the ORIGINATING device must be blessed. Without this,
    // compromising any enrolled laptop would inherit its owner's exec rights
    // across the whole fleet.
    if let Some(origin) = caller.origin_agent_id {
        match state.agents.find_in_tenant(tenant_id, origin).await {
            Ok(a) if a.exec_policy.can_originate => {}
            _ => return Err(ExecDenyReason::OriginNotAllowed),
        }
    }

    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Dispatch
// ────────────────────────────────────────────────────────────────────────────

/// Gate, push, await, audit. The ONE path a command takes, whether it came
/// from the browser, the CLI, or a bulk fan-out.
pub async fn dispatch(
    state: &AppState,
    tenant_id: ObjectId,
    agent: &Agent,
    caller: &Caller,
    body: &ExecRequestBody,
) -> ExecResponse {
    let agent_id = agent.id.unwrap_or_default();
    let request_id = ObjectId::new().to_hex();
    // Resolve BEFORE the allowlist check, and send the resolved name on the
    // wire, so the policy decision and the execution can never disagree about
    // which shell ran. An empty request means "the host default", which the
    // allowlist must be compared against by NAME — not as a literal "".
    let shell = ExecPolicy::resolve_shell(&body.shell, agent.os);
    let timeout_ms = exec_limits::clamp_timeout_ms(body.timeout_ms);
    let max_output_bytes = exec_limits::clamp_output_bytes(body.max_output_bytes);

    let mut audit = ExecAuditEvent {
        id: None,
        tenant_id,
        agent_id,
        user_id: caller.user_id,
        origin_agent_id: caller.origin_agent_id,
        request_id: request_id.clone(),
        source: caller.source.to_string(),
        shell: shell.clone(),
        command: body.command.clone(),
        at: DateTime::now(),
        exit_code: None,
        duration_ms: None,
        denied: None,
        output_sample: String::new(),
        output_sha256: String::new(),
        output_bytes: 0,
        truncated: false,
    };

    let deny = |mut audit: ExecAuditEvent, reason: ExecDenyReason, message: String| {
        audit.denied = Some(reason);
        (
            audit,
            ExecOutcome {
                error: Some(message),
                ..Default::default()
            },
        )
    };

    let (audit, outcome) = match authorize(state, tenant_id, agent, caller, &shell).await {
        Err(reason) => {
            let message = deny_message(reason);
            warn!(
                %agent_id, user = %caller.user_id, ?reason,
                "fleet-rpc: refused"
            );
            deny(audit, reason, message)
        }
        Ok(()) => {
            let consent_mode = agent.exec_policy.effective_consent_mode();
            let msg = ServerMsg::RpcExec {
                request_id: request_id.clone(),
                shell: shell.clone(),
                command: body.command.clone(),
                timeout_ms,
                max_output_bytes,
                cwd: body.cwd.clone(),
                caller: caller.display.clone(),
                consent_mode: Some(consent_mode),
            };
            // An attended device spends up to the prompt window waiting for a
            // human BEFORE the command's own budget starts, so the two are
            // additive — not overlapping.
            let mut deadline = std::time::Duration::from_millis(timeout_ms) + RESULT_GRACE;
            if consent_mode != roomler_ai_remote_control::models::ConsentMode::Auto {
                deadline += CONSENT_GRACE;
            }
            match state
                .rc_hub
                .exec_on_agent(agent_id, tenant_id, request_id.clone(), msg, deadline)
                .await
            {
                Ok(outcome) => {
                    audit.exit_code = outcome.exit_code;
                    audit.duration_ms = Some(outcome.duration_ms);
                    audit.truncated = outcome.truncated;
                    let combined = format!("{}{}", outcome.stdout, outcome.stderr);
                    audit.output_bytes = combined.len() as u64;
                    audit.output_sha256 = hex::encode(Sha256::digest(combined.as_bytes()));
                    audit.output_sample = truncate_sample(&combined);
                    info!(
                        %agent_id, user = %caller.user_id,
                        exit_code = ?outcome.exit_code,
                        duration_ms = outcome.duration_ms,
                        "fleet-rpc: ran"
                    );
                    (audit, outcome)
                }
                // The device is online but too old to understand the frame.
                // A distinct reason, because "upgrade the agent" is a
                // completely different action from "turn the feature on".
                Err(HubError::ExecUnsupported(_)) => deny(
                    audit,
                    ExecDenyReason::Unsupported,
                    format!(
                        "device runs agent {} which predates Fleet RPC — update it to use \
                         the device console",
                        agent.agent_version
                    ),
                ),
                Err(HubError::ExecTimeout(_)) => deny(
                    audit,
                    ExecDenyReason::Offline,
                    format!("no answer from the device within {}s", deadline.as_secs()),
                ),
                Err(HubError::AgentOffline(_)) => deny(
                    audit,
                    ExecDenyReason::Offline,
                    "device is offline".to_string(),
                ),
                Err(e) => deny(audit, ExecDenyReason::Offline, e.to_string()),
            }
        }
    };

    // Best-effort: the audit write must never gate the command's result. A
    // dropped row is logged loudly because it is the record someone will look
    // for later.
    if let Err(e) = state.exec_audit.record(audit).await {
        warn!(%agent_id, %e, "fleet-rpc: audit write failed");
    }

    ExecResponse {
        request_id,
        agent_id: agent_id.to_hex(),
        agent_name: agent.name.clone(),
        outcome,
    }
}

fn deny_message(reason: ExecDenyReason) -> String {
    match reason {
        ExecDenyReason::OrgDisabled => {
            "remote execution is disabled for this organization".to_string()
        }
        ExecDenyReason::NoPermission => {
            "you do not have the EXEC_DEVICE permission in this organization".to_string()
        }
        ExecDenyReason::DeviceDisabled => {
            "remote execution is not enabled on this device".to_string()
        }
        ExecDenyReason::CallerNotAllowed => {
            "this device does not allow remote execution by you".to_string()
        }
        ExecDenyReason::ShellNotAllowed => "this device does not allow that shell".to_string(),
        ExecDenyReason::OriginNotAllowed => {
            "this device is not permitted to originate remote execution".to_string()
        }
        ExecDenyReason::Unsupported => "device does not support Fleet RPC".to_string(),
        ExecDenyReason::Offline => "device is offline".to_string(),
        ExecDenyReason::ConsentDenied => {
            "the operator at the device did not approve the command".to_string()
        }
        ExecDenyReason::RateLimited => "too many commands for this device".to_string(),
        ExecDenyReason::AgentDisabled => {
            "remote execution is disabled on the device itself".to_string()
        }
    }
}

/// Trim the persisted output sample on a CHARACTER boundary — a byte-slice of
/// UTF-8 output would panic the moment a command prints anything non-ASCII,
/// which on a German-locale Windows host is immediately.
fn truncate_sample(s: &str) -> String {
    if s.len() <= ExecAuditEvent::SAMPLE_BYTES {
        return s.to_string();
    }
    let mut end = ExecAuditEvent::SAMPLE_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ────────────────────────────────────────────────────────────────────────────
// HTTP handlers
// ────────────────────────────────────────────────────────────────────────────

async fn tenant_of(tenant_id: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(tenant_id).map_err(|_| ApiError::BadRequest("Invalid tenant_id".into()))
}

/// Parse the tenant AND require the caller to be a member of it.
///
/// Gate 2 (`EXEC_DEVICE`) is evaluated inside [`authorize`] so a refusal is
/// AUDITED rather than turned into a bare 403 — but that means the handler
/// would otherwise reach the device lookup on behalf of a total stranger, and
/// the response body names devices (`agent_name`, and on a bulk sweep the
/// whole roster). Membership is the cheap check that stops a non-member
/// enumerating a fleet by asking it to run something.
async fn member_tenant(
    state: &AppState,
    tenant_id: &str,
    auth: &AuthUser,
) -> Result<ObjectId, ApiError> {
    let tid = tenant_of(tenant_id).await?;
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".into()));
    }
    Ok(tid)
}

async fn load_agent(
    state: &AppState,
    tenant_id: ObjectId,
    agent_id: &str,
) -> Result<Agent, ApiError> {
    let aid = ObjectId::parse_str(agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".into()))?;
    Ok(state.agents.find_in_tenant(tenant_id, aid).await?)
}

/// Resolve the acting principal for a browser/API caller.
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
        source: "ui",
    }
}

/// `POST /api/tenant/{tenant_id}/agent/{agent_id}/exec`
///
/// Always 200 when the request itself was well-formed: a policy refusal is a
/// result (`outcome.error`), not a transport error, so the caller renders one
/// consistent shape and the audit row is the source of truth either way.
pub async fn exec(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Json(body): Json<ExecRequestBody>,
) -> Result<Json<ExecResponse>, ApiError> {
    let tid = member_tenant(&state, &tenant_id, &auth).await?;
    if body.command.trim().is_empty() {
        return Err(ApiError::BadRequest("command must not be empty".into()));
    }
    let agent = load_agent(&state, tid, &agent_id).await?;
    let caller = http_caller(&state, &auth).await;
    Ok(Json(dispatch(&state, tid, &agent, &caller, &body).await))
}

/// `POST /api/tenant/{tenant_id}/agent/exec` — the fleet sweep.
///
/// An empty `agent_ids` means "every device whose policy allows it", which is
/// the whole point: `roomler exec --all -- <probe>` across a heterogeneous
/// fleet in one round-trip.
pub async fn exec_bulk(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<BulkExecRequestBody>,
) -> Result<Json<BulkExecResponse>, ApiError> {
    let tid = member_tenant(&state, &tenant_id, &auth).await?;
    if body.exec.command.trim().is_empty() {
        return Err(ApiError::BadRequest("command must not be empty".into()));
    }

    let agents: Vec<Agent> = if body.agent_ids.is_empty() {
        // One page wide enough to cover any fleet we'd sweep in a single
        // request — the MAX_BULK_TARGETS check below is the real bound, and
        // silently paginating here would make "--all" quietly mean "the first
        // 20 devices", which is exactly the kind of unannounced cap that
        // makes a sweep look complete when it isn't.
        let page = state
            .agents
            .list_for_tenant(
                tid,
                &PaginationParams {
                    page: 1,
                    per_page: (MAX_BULK_TARGETS + 1) as u64,
                    before: None,
                },
            )
            .await?;
        page.items
            .into_iter()
            .filter(|a| a.exec_policy.mode == ExecMode::On)
            .collect()
    } else {
        let mut out = Vec::new();
        for id in &body.agent_ids {
            out.push(load_agent(&state, tid, id).await?);
        }
        out
    };

    if agents.len() > MAX_BULK_TARGETS {
        return Err(ApiError::BadRequest(format!(
            "too many devices ({}); the per-request limit is {MAX_BULK_TARGETS}",
            agents.len()
        )));
    }

    let caller = http_caller(&state, &auth).await;
    let exec_body = &body.exec;
    // Bounded concurrency: a fleet sweep must not open one task per device and
    // starve the rest of the pod.
    let mut results = Vec::with_capacity(agents.len());
    for chunk in agents.chunks(BULK_CONCURRENCY) {
        let runs = chunk
            .iter()
            .map(|a| dispatch(&state, tid, a, &caller, exec_body));
        results.extend(futures::future::join_all(runs).await);
    }
    Ok(Json(BulkExecResponse { results }))
}

/// `POST /api/tenant/{tenant_id}/agent/{agent_id}/exec/{request_id}/cancel`
pub async fn cancel(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id, request_id)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = tenant_of(&tenant_id).await?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::EXEC_DEVICE,
        "EXEC_DEVICE",
    )
    .await?;
    let agent = load_agent(&state, tid, &agent_id).await?;
    let aid = agent.id.unwrap_or_default();
    match state.rc_hub.cancel_exec(aid, tid, request_id) {
        Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(e) => Err(ApiError::BadRequest(e.to_string())),
    }
}

/// `PUT /api/tenant/{tenant_id}/agent/{agent_id}/exec-policy` — gate 3.
///
/// `MANAGE_AGENTS`, like every other device-policy edit. Note this does NOT
/// require `EXEC_DEVICE`: deciding a device may be exec'd is a management act,
/// performing the exec is a separate power.
pub async fn set_policy(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Json(body): Json<ExecPolicyBody>,
) -> Result<Json<ExecPolicyBody>, ApiError> {
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

    let policy = ExecPolicy {
        mode: body.mode,
        can_originate: body.can_originate,
        allowed_user_ids: parse_ids(&body.allowed_user_ids, "user id")?,
        allowed_role_ids: parse_ids(&body.allowed_role_ids, "role id")?,
        consent_mode: body.consent_mode,
        shells: body.shells.clone(),
    };
    state
        .agents
        .update_exec_policy(tid, agent.id.unwrap_or_default(), &policy)
        .await?;
    info!(
        agent = %agent_id, admin = %auth.user_id, mode = ?policy.mode,
        "fleet-rpc: exec policy updated"
    );
    Ok(Json(body))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OrgExecSettings {
    pub remote_exec_enabled: bool,
}

/// `GET /api/tenant/{tenant_id}/exec-settings` — gate 1's current state.
/// Readable by any `MANAGE_AGENTS` admin so the device console can explain
/// *why* every device is refusing before anyone starts editing per-device
/// policy.
pub async fn get_org_settings(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<OrgExecSettings>, ApiError> {
    let tid = tenant_of(&tenant_id).await?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;
    let tenant = state.tenants.base.find_by_id(tid).await?;
    Ok(Json(OrgExecSettings {
        remote_exec_enabled: tenant.settings.remote_exec_enabled,
    }))
}

/// `PUT /api/tenant/{tenant_id}/exec-settings` — flip gate 1.
///
/// `MANAGE_TENANT`, not `MANAGE_AGENTS`: this one switch decides whether the
/// org's devices can be shelled into at all, which is an org-owner decision
/// rather than a fleet-admin one.
pub async fn set_org_settings(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<OrgExecSettings>,
) -> Result<Json<OrgExecSettings>, ApiError> {
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
        .set_remote_exec_enabled(tid, body.remote_exec_enabled)
        .await?;
    warn!(
        tenant = %tenant_id, admin = %auth.user_id,
        enabled = body.remote_exec_enabled,
        "fleet-rpc: org kill-switch changed"
    );
    Ok(Json(OrgExecSettings {
        remote_exec_enabled: tenant.settings.remote_exec_enabled,
    }))
}

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    #[serde(flatten)]
    pub page: PaginationParams,
    /// Narrow to one device.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Narrow to one person — where an incident review starts.
    #[serde(default)]
    pub user_id: Option<String>,
}

/// `GET /api/tenant/{tenant_id}/exec-audit`
pub async fn audit(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<ExecAuditResponse>, ApiError> {
    let tid = tenant_of(&tenant_id).await?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::VIEW_EXEC_AUDIT,
        "VIEW_EXEC_AUDIT",
    )
    .await?;

    let page = match (&q.agent_id, &q.user_id) {
        (Some(a), _) => {
            let aid = ObjectId::parse_str(a)
                .map_err(|_| ApiError::BadRequest("Invalid agent_id".into()))?;
            state.exec_audit.list_for_agent(tid, aid, &q.page).await?
        }
        (None, Some(u)) => {
            let uid = ObjectId::parse_str(u)
                .map_err(|_| ApiError::BadRequest("Invalid user_id".into()))?;
            state.exec_audit.list_for_user(tid, uid, &q.page).await?
        }
        (None, None) => state.exec_audit.list_for_tenant(tid, &q.page).await?,
    };
    Ok(Json(ExecAuditResponse {
        items: page.items.into_iter().map(Into::into).collect(),
        total: page.total,
        page: page.page,
        per_page: page.per_page,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_truncation_never_splits_a_char() {
        // A German-locale Windows host prints non-ASCII in the first line of
        // half its diagnostics; a byte-slice here would panic on the first
        // real command.
        let s = "ä".repeat(ExecAuditEvent::SAMPLE_BYTES);
        let out = truncate_sample(&s);
        assert!(out.len() <= ExecAuditEvent::SAMPLE_BYTES);
        assert!(out.chars().all(|c| c == 'ä'));
        // Round-trips as valid UTF-8 by construction — the point of the test.
        assert_eq!(out, String::from_utf8(out.clone().into_bytes()).unwrap());
    }

    #[test]
    fn short_output_is_kept_verbatim() {
        assert_eq!(truncate_sample("hello"), "hello");
        assert_eq!(truncate_sample(""), "");
    }

    #[test]
    fn every_deny_reason_has_an_operator_message() {
        for r in [
            ExecDenyReason::OrgDisabled,
            ExecDenyReason::NoPermission,
            ExecDenyReason::DeviceDisabled,
            ExecDenyReason::CallerNotAllowed,
            ExecDenyReason::ShellNotAllowed,
            ExecDenyReason::OriginNotAllowed,
            ExecDenyReason::Unsupported,
            ExecDenyReason::Offline,
            ExecDenyReason::ConsentDenied,
            ExecDenyReason::RateLimited,
            ExecDenyReason::AgentDisabled,
        ] {
            let m = deny_message(r);
            assert!(!m.is_empty(), "{r:?} has no message");
            // "Forbidden" tells an operator nothing about which of four gates
            // said no, which is the entire diagnostic value here.
            assert!(m.len() > 15, "{r:?} message is too terse: {m}");
        }
    }
}
