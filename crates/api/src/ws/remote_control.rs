// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! WebSocket glue for the remote-control subsystem.
//!
//! The `roomler-ai-remote-control` crate owns the state machine and the
//! registry of agents/controllers ([`Hub`]). This module is the thin bridge
//! between an Axum [`WebSocket`] and the Hub: it pumps [`ServerMsg`] values
//! from a per-connection [`mpsc::Receiver`] out to the socket, parses inbound
//! [`ClientMsg`] values and forwards them to [`Hub::dispatch`].

use axum::extract::ws::WebSocket;
use bson::oid::ObjectId;
use roomler_ai_mod_fleet::hub::DispatchCtx;
use roomler_ai_remote_control::{
    models::ConsentMode,
    signaling::{ClientMsg, RelayRegionRtt, Role, ServerMsg},
};
use tracing::{debug, info, warn};

use crate::state::AppState;

// FR-69 P5a: the rc control pair (publish + apply) is the fleet module s now;
// re-exported so every path in this crate reads as before.
pub use roomler_ai_mod_fleet::ctrl::{apply_rc_ctrl, publish_rc_ctrl};
// FR-69 P5c — the agent socket and its pump moved to the fleet module too;
// the tunnel socket and the controller socket share the pump, and the relay
// probe report (network's, still here) reads the RTT ladder the hello uses.
pub use roomler_ai_mod_fleet::socket::{prefs_from_rtt, pump_server_messages};

/// Persist one device-reported SSH activity row (P8).
///
/// Everything trustworthy in the row — which tenant, which device — is taken
/// from the authenticated connection by the caller and passed in here;
/// everything else is the device's claim. Best-effort by design: a log line
/// must never be able to disturb a live session, so a failed insert is warned
/// and dropped.
///
/// `detail` is re-clamped server-side. The device already caps it, but a
/// length bound that only exists on the reporting side is not a bound.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_ssh_activity(
    state: &AppState,
    tenant_id: ObjectId,
    agent_id: ObjectId,
    grant_id: Option<String>,
    caller: String,
    kind: roomler_ai_remote_control::models::SshActivityKind,
    detail: Option<String>,
    exit_code: Option<i32>,
    allowed: bool,
) {
    use roomler_ai_remote_control::models::SshActivityEvent;

    let detail = detail.map(|mut d| {
        if d.chars().count() > SshActivityEvent::MAX_DETAIL {
            d = d.chars().take(SshActivityEvent::MAX_DETAIL).collect();
            d.push('…');
        }
        d
    });
    let event = SshActivityEvent {
        id: None,
        tenant_id,
        agent_id,
        grant_id,
        caller,
        kind,
        detail,
        exit_code,
        allowed,
        at: bson::DateTime::now(),
    };
    if let Err(e) = state.ssh_activity.record(event).await {
        warn!(%agent_id, %e, "ssh_activity insert failed");
    }
}

/// FR-40 — persist a device's [`ClientMsg::KeyRotated`] onto its agent row.
/// Same rules as [`record_config_report`]: last report wins, `reported_at`
/// is stamped here, `detail` is re-clamped on receipt. Public keys only —
/// the frame has no field for anything else, by construction.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_key_rotation_report(
    state: &AppState,
    tenant_id: ObjectId,
    agent_id: ObjectId,
    request_id: String,
    outcome: roomler_ai_remote_control::models::KeyRotationOutcome,
    old_public_key: Option<String>,
    new_public_key: Option<String>,
    key_epoch: u32,
    detail: Option<String>,
) {
    use roomler_ai_remote_control::models::KeyRotationReport;

    let clamp = |s: String, max: usize| -> String {
        if s.chars().count() > max {
            let mut t: String = s.chars().take(max).collect();
            t.push('…');
            t
        } else {
            s
        }
    };
    // A WireGuard public key is 44 base64 chars; anything longer is not one.
    const MAX_KEY: usize = 64;
    let report = KeyRotationReport {
        request_id: clamp(request_id, 64),
        outcome,
        old_public_key: old_public_key.map(|k| clamp(k, MAX_KEY)),
        new_public_key: new_public_key.map(|k| clamp(k, MAX_KEY)),
        key_epoch,
        detail: detail.map(|d| clamp(d, KeyRotationReport::MAX_DETAIL)),
        reported_at: bson::DateTime::now(),
    };
    info!(
        %agent_id, request_id = %report.request_id, outcome = ?report.outcome,
        key_epoch, "overlay-key rotation reported by the device"
    );
    if let Err(e) = state
        .agents
        .record_key_rotation_report(tenant_id, agent_id, &report)
        .await
    {
        warn!(%agent_id, %e, "key rotation report write failed");
    }
}

/// The device-originated SSH leg (`roomler ssh <device>`).
///
/// Mirrors [`handle_agent_exec_request`] and goes through the SAME
/// [`agent_ssh::dispatch`] the HTTP route uses, so there is exactly one place
/// where the gates are evaluated regardless of how the request arrived.
///
/// [`agent_ssh::dispatch`]: crate::routes::agent_ssh::dispatch
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_agent_ssh_request(
    state: &AppState,
    tenant_id: bson::oid::ObjectId,
    origin_agent_id: bson::oid::ObjectId,
    request_id: String,
    target: String,
    public_key: String,
    session_secs: u64,
    reply_tx: roomler_ai_remote_control::session::ClientTx,
) {
    // A refusal carries no address, so it carries no host key either — the two
    // are only ever meaningful together.
    let fail = |msg: String| ServerMsg::SshResponse {
        request_id: request_id.clone(),
        address: None,
        port: None,
        grant_id: None,
        host_pubkey: None,
        expires_at_ms: None,
        error: Some(msg),
    };

    // The origin's owner is the person whose permissions this runs under. A
    // device whose row vanished mid-flight has no principal, so it gets
    // nothing.
    let origin = match state
        .agents
        .find_in_tenant(tenant_id, origin_agent_id)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            let _ = reply_tx.try_send(fail(format!("origin device unknown: {e}")));
            return;
        }
    };

    let agent = match resolve_exec_target(state, tenant_id, &target).await {
        Some(a) => a,
        None => {
            let _ = reply_tx.try_send(fail(format!("no device named {target:?} in this org")));
            return;
        }
    };

    // "<person> (via <device>)" — the target's log and consent prompt name
    // both the human accountable and the box the request came from.
    let who = state
        .users
        .base
        .find_by_id(origin.owner_user_id)
        .await
        .map(|u| u.display_name)
        .unwrap_or_else(|_| origin.owner_user_id.to_hex());
    let caller = crate::routes::agent_ssh::Caller {
        user_id: origin.owner_user_id,
        display: format!("{who} (via {})", origin.name),
        origin_agent_id: Some(origin_agent_id),
    };

    let res = crate::routes::agent_ssh::dispatch(
        state,
        tenant_id,
        &agent,
        &caller,
        &public_key,
        session_secs,
    )
    .await;

    // EXHAUSTIVE — this hand-written mapping is how `host_pubkey` went missing
    // on this leg in the first place: the HTTP response gained the field and a
    // field-by-field literal silently kept sending the old shape, which
    // compiles perfectly. Binding every field means the next addition has to be
    // decided about rather than forgotten.
    let crate::routes::agent_ssh::SshResponseBody {
        address,
        port,
        // Dropped on purpose: this leg answers a device that named its own
        // target, so echoing the MagicDNS name back tells it nothing it did
        // not just say. The HTTP leg keeps it for display.
        name: _,
        grant_id,
        host_pubkey,
        expires_at_ms,
        error,
    } = res;
    let msg = ServerMsg::SshResponse {
        request_id: request_id.clone(),
        address,
        port,
        grant_id,
        host_pubkey,
        expires_at_ms,
        error,
    };
    if reply_tx.try_send(msg).is_err() {
        warn!(%origin_agent_id, %request_id, "rc:ssh.response undeliverable — origin WS gone");
    }
}

/// Multi-region DERP: answer a ticket request. The ticket binds the agent's
/// overlay `(network_id, wg_public_key)` — exactly the invariants the central
/// `/derp` enforces from Mongo, so a PoP relay can enforce them with the
/// PUBLIC key alone.
pub(crate) async fn handle_derp_ticket_request(
    state: &AppState,
    agent_id: ObjectId,
    tx: &tokio::sync::mpsc::Sender<ServerMsg>,
) {
    let Some(signer) = &state.derp_ticket else {
        debug!(%agent_id, "derp ticket requested but no signer configured");
        return;
    };
    let Some(node) =
        crate::ws::overlay::current_node(state, crate::ws::overlay::NodeIdentity::Agent(agent_id))
            .await
    else {
        debug!(%agent_id, "derp ticket requested but agent has no overlay node");
        return;
    };
    match signer.mint(&node.network_id.to_hex(), &node.wg_public_key) {
        Ok((ticket, exp)) => {
            let _ = tx.try_send(ServerMsg::DerpTicket { ticket, exp });
        }
        Err(e) => warn!(%agent_id, %e, "derp ticket mint failed"),
    }
}

/// Multi-region relay PoPs: derive the agent's `relay_home` from a probe
/// report and fan it out (Hub live copy always; Mongo rate-limited).
///
/// Hysteresis: the home only MOVES when the best region improves on the
/// current home's measured RTT by >20%, or the current home stopped being
/// measurable (dropped from the region set, or all its samples timed out).
/// This keeps a border-line agent from flapping between two near-equal PoPs —
/// the sticky pair cache protects live pairs, this protects everything else.
pub(crate) async fn handle_relay_probe_report(
    state: &AppState,
    agent_id: ObjectId,
    results: &[RelayRegionRtt],
    last_persist: &mut Option<std::time::Instant>,
) {
    let map = &state.turn_map;
    if !map.enabled || map.regions.is_empty() {
        return;
    }
    let known: Vec<&RelayRegionRtt> = results
        .iter()
        .filter(|r| map.regions.contains_key(&r.region))
        .collect();
    let best = known
        .iter()
        .filter_map(|r| r.rtt_ms.map(|ms| (ms, r.region.as_str())))
        .min();
    let current = state.rc_hub.agent_relay_home(agent_id);
    let new_home: Option<String> = match (best, current.as_deref()) {
        // Nothing measurable (full-UDP-block / dead PoPs) → default region.
        (None, _) => None,
        (Some((_, b)), None) => Some(b.to_string()),
        (Some((best_ms, b)), Some(cur)) => {
            let cur_ms = known
                .iter()
                .find(|r| r.region == cur)
                .and_then(|r| r.rtt_ms);
            match cur_ms {
                None => Some(b.to_string()),
                Some(c) if f64::from(best_ms) < f64::from(c) * 0.8 => Some(b.to_string()),
                Some(_) => Some(cur.to_string()),
            }
        }
    };
    state
        .rc_hub
        .set_agent_relay_home(agent_id, new_home.clone(), prefs_from_rtt(results));
    let due = last_persist
        .map(|t| t.elapsed() >= PROBE_PERSIST_MIN_INTERVAL)
        .unwrap_or(true);
    if !due {
        return;
    }
    *last_persist = Some(std::time::Instant::now());
    if let Err(e) = state
        .agents
        .set_relay_home(agent_id, new_home.as_deref(), results)
        .await
    {
        warn!(%agent_id, %e, "set_relay_home (agents) failed");
    }
    if let Err(e) = state
        .overlay_nodes
        .set_relay_home_for_agent(agent_id, new_home.as_deref())
        .await
    {
        warn!(%agent_id, %e, "set_relay_home (overlay_nodes) failed");
    }
    debug!(%agent_id, home = ?new_home, "relay probe report processed");
}

/// Route a parsed `rc:*` message coming from a controller browser tab.
/// Returns `true` if the message was handled, `false` if it wasn't rc:*.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_controller_rc(
    state: &AppState,
    user_id: ObjectId,
    controller_name: &str,
    controller_tx: &roomler_ai_remote_control::session::ClientTx,
    text: &str,
    consent_mode: ConsentMode,
    override_reason: Option<String>,
    // P6 — the device's `AccessPolicy.input_mode` (resolved by the authz
    // gate alongside `consent_mode`); forwarded to the agent's arbiter.
    input_mode: Option<roomler_ai_remote_control::models::InputMode>,
    // Multi-org — the org name the host's consent prompt should name
    // (resolved by the same gate).
    tenant_name: Option<String>,
    // PR-1 rehome direction inputs: the affinity key this conn DIALED
    // with (None = key-less legacy/racy dial) and when it established.
    dialed_tid: Option<&str>,
    conn_established_ms: i64,
    // PR-2 relay: this browser socket's connection id - the address the
    // owner pod's proxy pump routes replies back to.
    connection_id: &str,
) -> bool {
    let hub = &state.rc_hub;
    let Ok(parsed) = serde_json::from_str::<ClientMsg>(text) else {
        return false;
    };
    let is_session_request = matches!(&parsed, ClientMsg::SessionRequest { .. });
    let ctx = DispatchCtx {
        role: Role::Controller,
        user_id: Some(user_id),
        agent_id: None,
        controller_name: Some(controller_name.to_string()),
        controller_tx: Some(controller_tx.clone()),
        consent_mode,
        // PR-2: the relay needs its own copy after ctx takes this one.
        override_reason: override_reason.clone(),
        input_mode,
        tenant_name: tenant_name.clone(),
    };
    // …and so does the cross-pod relay, for the same reason.
    let relay_tenant_name = tenant_name;
    if let Err(e) = hub.dispatch(&ctx, parsed) {
        warn!(%user_id, %e, "rc:* dispatch failed (controller)");
        // C-2 rehome: a SessionRequest that missed the LOCAL hub while a
        // FOREIGN pod holds a fresh presence record is a cross-pod split,
        // not a real offline. Tell the controller which state it's in
        // (`agent_on_other_pod` → the UI force-redials its WS and retries
        // once) and nudge the owning pod to cycle the agent's WS if idle —
        // both ends then re-land at the current LB hash. Probe budget
        // 250 ms; on any probe failure fall through to the honest
        // `agent_offline`.
        if is_session_request
            && let roomler_ai_remote_control::error::Error::AgentOffline(agent_hex) = &e
            && let Some(redis) = &state.redis_pubsub
            && let Ok(Ok(Some(owner))) = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                redis.agent_presence_foreign(agent_hex),
            )
            .await
        {
            note_agent_offline_evidence(state, agent_hex.clone(), "controller_session");
            crate::cluster::metrics::bump(&crate::cluster::metrics::RC_REHOME_TOTAL);
            let record = crate::cluster::directory::OwnerRecord::parse(&owner);
            let owner_pod = record
                .as_ref()
                .map(|r| r.pod_id.clone())
                .unwrap_or_default();
            let agent_since_ms = record.map(|r| r.since_ms).unwrap_or(0);
            // PR-2 relay FIRST: make the cross-pod session WORK now;
            // convergence (controller re-key, or the agent's own next
            // natural reconnect) proceeds without user-visible errors.
            // No nudge on the relay path - an idle-nudge racing the
            // relayed create would tear the session it just built.
            if let Ok(frame_val) = serde_json::from_str::<serde_json::Value>(text) {
                match crate::ws::rc_relay::relay_rc_frame(
                    state,
                    &owner_pod,
                    connection_id,
                    user_id,
                    controller_name,
                    consent_mode,
                    &override_reason,
                    input_mode,
                    &relay_tenant_name,
                    &frame_val,
                )
                .await
                {
                    Ok(None) => {
                        info!(
                            %user_id,
                            agent = %agent_hex,
                            %owner_pod,
                            "cross-pod rc miss: relayed to owner pod"
                        );
                        return true;
                    }
                    Ok(Some((code, message))) => {
                        // The owner's Hub answered authoritatively
                        // (agent_busy, permission_denied, ...) - same
                        // surface as a local dispatch failure.
                        let _ = controller_tx.try_send(ServerMsg::Error {
                            session_id: None,
                            code,
                            message,
                            open_nonce: None,
                        });
                        return true;
                    }
                    Err(()) => { /* relay unavailable - PR-1 path below */ }
                }
            }
            // PR-1 direction rule: only a correctly-keyed conn that is
            // provably NEWER than the agent's registration justifies
            // nudging the agent; every other shape means the CONTROLLER
            // is the mis-placed party (the 2026-08-04 incident) and a
            // nudge would bounce a correctly-homed, possibly busy agent.
            // One agent lookup for its tenant; on failure the helper
            // falls back to controller-moves (never nudge on doubt).
            let agent_tenant_hex = match ObjectId::parse_str(agent_hex) {
                Ok(aid) => state
                    .agents
                    .base
                    .find_by_id(aid)
                    .await
                    .map(|a| a.tenant_id.to_hex())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };
            let direction = crate::ws::rc_cluster::rehome_direction(
                dialed_tid,
                &agent_tenant_hex,
                conn_established_ms,
                agent_since_ms,
                state.settings.rc.rehome_direction_guard_ms,
            );
            match direction {
                crate::ws::rc_cluster::RehomeDirection::ControllerMove { reason } => {
                    crate::cluster::metrics::bump(
                        &crate::cluster::metrics::RC_REHOME_CONTROLLER_TOTAL,
                    );
                    info!(
                        %user_id,
                        agent = %agent_hex,
                        %owner_pod,
                        ?dialed_tid,
                        reason,
                        "cross-pod rc miss: controller re-dials (nudge suppressed)"
                    );
                }
                crate::ws::rc_cluster::RehomeDirection::NudgeAgent => {
                    info!(
                        %user_id,
                        agent = %agent_hex,
                        %owner_pod,
                        "cross-pod rc miss: agent judged parked; nudging owner pod"
                    );
                    spawn_agent_nudge(state, owner_pod, agent_hex.clone());
                }
            }
            // Pod identity stays in server logs; the wire carries only
            // the actionable fact (the UI re-keys + redials on it).
            let _ = controller_tx.try_send(ServerMsg::Error {
                session_id: None,
                code: "agent_on_other_pod".to_string(),
                message: "agent is homed on another pod; re-dial and retry".to_string(),
                open_nonce: None,
            });
            return true;
        }
        // PR-2: a session-scoped frame (answer / ice / terminate) whose
        // session lives on another pod - this conn relayed its create
        // there. Forward it: the owner holding the session dispatches;
        // an owner that lost it answers session_not_found, which is the
        // honest surface either way.
        if matches!(
            &e,
            roomler_ai_remote_control::error::Error::SessionNotFound(_)
        ) {
            let owners: Vec<String> = state
                .remote_rc_conns
                .get(connection_id)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            if !owners.is_empty()
                && let Ok(frame_val) = serde_json::from_str::<serde_json::Value>(text)
            {
                let mut refusal: Option<(String, String)> = None;
                for owner in owners {
                    match crate::ws::rc_relay::relay_rc_frame(
                        state,
                        &owner,
                        connection_id,
                        user_id,
                        controller_name,
                        consent_mode,
                        &override_reason,
                        input_mode,
                        &relay_tenant_name,
                        &frame_val,
                    )
                    .await
                    {
                        Ok(None) => return true,
                        Ok(Some(cm)) => refusal = Some(cm),
                        Err(()) => {}
                    }
                }
                if let Some((code, message)) = refusal {
                    let _ = controller_tx.try_send(ServerMsg::Error {
                        session_id: error_session_id(&e),
                        code,
                        message,
                        open_nonce: None,
                    });
                    return true;
                }
            }
        }
        // Phase A-1 split-evidence probe for the remaining offline paths
        // (non-session-request messages, probe timeouts).
        if let roomler_ai_remote_control::error::Error::AgentOffline(agent_hex) = &e {
            note_agent_offline_evidence(state, agent_hex.clone(), "controller_session");
        }
        // Surface the failure to the controller so the UI can exit its
        // "Requesting session…" spinner instead of hanging. Best-effort —
        // the controller may already be closing.
        let _ = controller_tx.try_send(ServerMsg::Error {
            session_id: error_session_id(&e),
            code: error_code(&e).to_string(),
            message: e.to_string(),
            open_nonce: None,
        });
    }
    true
}

/// Result of the session authz gate: the effective consent mode plus a VALIDATED
/// admin break-glass reason (Phase 5). `override_reason` is `Some` only when an
/// `ADMINISTRATOR` force-started a device they don't own with a non-empty reason
/// — in which case `mode` is `Auto` (consent skipped) and the Hub records an
/// `AdminOverride` audit.
pub struct SessionAuthz {
    pub mode: ConsentMode,
    pub override_reason: Option<String>,
    /// P6 — the device's `AccessPolicy.input_mode` (None = free default).
    pub input_mode: Option<roomler_ai_remote_control::models::InputMode>,
    /// Multi-org — display name of the organization this session happens in,
    /// so the host's consent prompt can say WHICH org is asking. Resolved
    /// from the agent row the gate already loads. `None` on the early-out
    /// paths (non-session-request, unknown agent) where no prompt follows.
    pub tenant_name: Option<String>,
}

impl SessionAuthz {
    fn allow(mode: ConsentMode) -> Self {
        Self {
            mode,
            override_reason: None,
            input_mode: None,
            tenant_name: None,
        }
    }
    fn allow_with_input(
        mode: ConsentMode,
        input_mode: Option<roomler_ai_remote_control::models::InputMode>,
        tenant_name: Option<String>,
    ) -> Self {
        Self {
            mode,
            override_reason: None,
            input_mode,
            tenant_name,
        }
    }
}

/// Authorization + consent-mode gate for `rc:session.request` — the Hub can't
/// do this because the `remote_control` crate sits below `services` in the dep
/// graph and has no access to tenant roles. Returns `Ok(SessionAuthz)` for an
/// allowed request, or `Err(reason)` to DENY. A non-`SessionRequest` rc:* message
/// returns `Ok(allow(Prompt))` — those are intra-session and the mode is unused.
///
/// Layers (coarse→fine authz): quarantine → self-control → tenant capability
/// (`ADMINISTRATOR` / `REMOTE_CONTROL`) → per-agent allowlist (empty = no
/// per-device restriction). Consent mode: self-control → `Auto`; else the
/// device's `effective_consent_mode()`. **Break-glass (Phase 5):** an
/// `ADMINISTRATOR` who sends a non-empty `override_reason` for a device they
/// don't own gets `Auto` (consent skipped) + the reason carried through for the
/// `AdminOverride` audit.
pub async fn resolve_session_authz(
    state: &AppState,
    controller_user_id: ObjectId,
    text: &str,
) -> Result<SessionAuthz, String> {
    use roomler_ai_db::models::role::permissions;
    use roomler_ai_remote_control::models::AgentStatus;

    let (agent_id, override_reason) = match serde_json::from_str::<ClientMsg>(text) {
        Ok(ClientMsg::SessionRequest {
            agent_id,
            override_reason,
            ..
        }) => (agent_id, override_reason),
        // Not a session request → the mode is unused; allow through.
        _ => return Ok(SessionAuthz::allow(ConsentMode::Prompt)),
    };

    // Unknown / soft-deleted agent → let the Hub answer with a clean
    // AgentNotFound rather than surfacing a permission error (the mode is moot —
    // create_session will fail on the agent lookup).
    let agent = match state.agents.base.find_by_id(agent_id).await {
        Ok(a) if a.deleted_at.is_none() => a,
        _ => return Ok(SessionAuthz::allow(ConsentMode::Prompt)),
    };

    if agent.status == AgentStatus::Quarantined {
        return Err("device is quarantined; new sessions are blocked".to_string());
    }

    // P6 — the device's input arbitration mode rides every allowed outcome.
    let input_mode = agent.access_policy.input_mode;

    // Multi-org — the organization name the host's consent prompt will show.
    // One extra read, and only on a real session request (the early-outs above
    // never reach here). A failed lookup degrades the prompt to the agent's own
    // org label rather than failing the session.
    // The SAME read answers "is this org archived?", which must refuse the
    // session: an archived org stops acting (`routes::tenant::archive`).
    // A failed lookup degrades the prompt to the agent's own org label
    // rather than failing the session — it must not, however, be read as
    // "not archived" on a row we could not see, so only a successful read
    // can refuse.
    let tenant = state.tenants.base.find_by_id(agent.tenant_id).await.ok();
    if tenant.as_ref().is_some_and(|t| t.is_archived) {
        return Err("this organization is archived; new sessions are blocked".to_string());
    }
    let tenant_name = tenant.map(|t| t.name);

    // Controlling your OWN device is always allowed. Whether it also
    // auto-consents is now the DEVICE's call (FR-27): `prompt_owner` unset —
    // every pre-FR-27 row, and the default — keeps the historical shortcut,
    // because unattended access to your own headless boxes is the common case
    // and prompting them asks a machine nobody is sitting at.
    //
    // ⚠️ This shortcut is why the consent picker looked broken. It short-circuits
    // BEFORE `effective_consent_mode()` is ever read, so on a fleet where one
    // person owns every device the setting had no observable effect at all —
    // and nothing on screen said so. The UI now labels it, and this flag is how
    // an owner opts into being asked (a shared workstation, or field-testing the
    // attended modes without a second account).
    if agent.owner_user_id == controller_user_id {
        return Ok(SessionAuthz::allow_with_input(
            agent.access_policy.owner_consent_mode(),
            input_mode,
            tenant_name,
        ));
    }

    // The effective mode for an allowed non-owner controller (attended default).
    let mode = agent.access_policy.effective_consent_mode();

    let perms = state
        .tenants
        .get_member_permissions(agent.tenant_id, controller_user_id)
        .await
        .unwrap_or(0);
    if permissions::has(perms, permissions::ADMINISTRATOR) {
        // Phase 5 break-glass: an ADMINISTRATOR may SKIP consent, but only with a
        // non-empty reason. A blank/absent reason ⇒ the admin gets the device's
        // normal consent mode (no forced override).
        if let Some(reason) = override_reason.filter(|r| !r.trim().is_empty()) {
            return Ok(SessionAuthz {
                mode: ConsentMode::Auto,
                override_reason: Some(reason),
                input_mode,
                tenant_name,
            });
        }
        return Ok(SessionAuthz::allow_with_input(
            mode,
            input_mode,
            tenant_name.clone(),
        ));
    }
    if !permissions::has(perms, permissions::REMOTE_CONTROL) {
        return Err("you don't have permission to control others' devices".to_string());
    }

    // Per-agent allowlist. Empty ⇒ no per-device restriction (any operator may
    // request; consent is the real gate). Non-empty ⇒ user or a role must match.
    let policy = &agent.access_policy;
    if policy.allowed_user_ids.is_empty() && policy.allowed_role_ids.is_empty() {
        return Ok(SessionAuthz::allow_with_input(
            mode,
            input_mode,
            tenant_name.clone(),
        ));
    }
    if policy.allowed_user_ids.contains(&controller_user_id) {
        return Ok(SessionAuthz::allow_with_input(
            mode,
            input_mode,
            tenant_name.clone(),
        ));
    }
    let role_ids = state
        .tenants
        .member_role_ids(agent.tenant_id, controller_user_id)
        .await
        .unwrap_or_default();
    if policy.allowed_role_ids.iter().any(|r| role_ids.contains(r)) {
        return Ok(SessionAuthz::allow_with_input(
            mode,
            input_mode,
            tenant_name.clone(),
        ));
    }
    Err("you're not on this device's control allowlist".to_string())
}

/// C-2/C-3 — fire-and-forget idle-agent nudge at the pod owning an
/// agent's WS (read from its directory record): the owner cycles the
/// socket iff the agent is fully idle, so its reconnect re-hashes onto
/// the current LB map. Failure is harmless (the requester's own retry
/// path still works; the agent converges on its next natural reconnect).
pub(crate) fn spawn_agent_nudge(state: &AppState, owner_pod: String, agent_hex: String) {
    let Some(bus) = state.cluster_bus.clone() else {
        return;
    };
    if owner_pod.is_empty() {
        return;
    }
    // PR-1 requester-side throttle: a controller click storm + retry
    // ladder sent 11 nudge RPCs in 15 s at one refusing owner in the
    // 2026-08-04 incident. The owner has its own cooldown; this just
    // keeps the bus quiet.
    if let Ok(aid) = ObjectId::parse_str(&agent_hex)
        && !crate::ws::rc_cluster::nudge_request_allowed(
            &state.agent_nudge_throttle,
            aid,
            std::time::Duration::from_millis(state.settings.rc.nudge_requester_throttle_ms),
        )
    {
        debug!(agent = %agent_hex, "agent nudge RPC suppressed (requester throttle)");
        return;
    }
    tokio::spawn(async move {
        match bus
            .request(
                &owner_pod,
                "rc.agent_nudge",
                serde_json::json!({ "agent_id": agent_hex }),
            )
            .await
        {
            Ok(rep) => {
                let nudged = rep.get("nudged").and_then(|v| v.as_bool()).unwrap_or(false);
                // `reason` is absent from pre-PR-1 peers (mixed-version
                // roll) — tolerate.
                let reason = rep.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                if nudged {
                    info!(agent = %agent_hex, %owner_pod, "agent rehome nudge fired on owner pod");
                } else {
                    info!(
                        agent = %agent_hex,
                        %owner_pod,
                        reason,
                        "agent rehome nudge refused by owner pod"
                    );
                }
            }
            Err(e) => debug!(agent = %agent_hex, %e, "agent rehome nudge failed"),
        }
    });
}

/// Phase A-1 split-evidence probe (A2b): fired on a LOCAL hub miss; if
/// another pod holds a FRESH presence record for the agent, that miss was
/// a cross-pod split, not a real offline. One warn + a process counter —
/// the permanent field instrument that gates the Phase A-2 rehome work
/// (steady-state nonzero = stable split; spikes only around rolls =
/// churn). Fire-and-forget: never blocks the caller.
pub(crate) fn note_agent_offline_evidence(
    state: &AppState,
    agent_hex: String,
    caller: &'static str,
) {
    // Probe throttle: the tunnel-ICE path can miss at candidate rate
    // (>10/s in the 2026-08-02 incident); one Redis GET per 5 s is
    // plenty for an existence instrument.
    static LAST_PROBE_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let now_ms = bson::DateTime::now().timestamp_millis();
    let last = LAST_PROBE_MS.load(std::sync::atomic::Ordering::Relaxed);
    if now_ms - last < 5_000
        || LAST_PROBE_MS
            .compare_exchange(
                last,
                now_ms,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
    {
        return;
    }
    let Some(redis) = state.redis_pubsub.clone() else {
        return;
    };
    tokio::spawn(async move {
        match redis.agent_presence_foreign(&agent_hex).await {
            Ok(Some(owner)) => {
                let total = crate::cluster::metrics::SPLIT_EVIDENCE_TOTAL
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                warn!(
                    agent = %agent_hex, caller, owner = %owner, total,
                    "SPLIT EVIDENCE: local hub miss but another pod holds a fresh presence record"
                );
            }
            Ok(None) => {}
            Err(e) => debug!(%agent_hex, %e, "split-evidence probe failed"),
        }
    });
}

/// Stable short code for the wire. Exhaustive match so a new
/// `remote_control::Error` variant triggers a compile error here rather
/// than silently being reported as "internal".
pub(crate) fn error_code(e: &roomler_ai_remote_control::Error) -> &'static str {
    use roomler_ai_remote_control::Error::*;
    match e {
        AgentOffline(_) => "agent_offline",
        AgentNotFound(_) => "agent_not_found",
        AgentBusy => "agent_busy",
        SessionNotFound(_) => "session_not_found",
        BadPhase(_, _) => "bad_phase",
        ConsentDenied => "consent_denied",
        ConsentTimeout => "consent_timeout",
        PermissionDenied(_) => "permission_denied",
        BadMessage(_) => "bad_message",
        SendFailed => "send_failed",
        // Fleet RPC. Distinct codes because the operator actions differ:
        // "update the agent" vs "the device stopped answering".
        ExecUnsupported(_) => "exec_unsupported",
        ExecTimeout(_) => "exec_timeout",
        Mongo(_) => "internal",
        Bson(_) => "internal",
        Json(_) => "internal",
    }
}

/// If the underlying error references a specific session, extract its id
/// so the controller UI can route the error to the right spinner instead
/// of assuming it's about the most recently attempted session.
fn error_session_id(e: &roomler_ai_remote_control::Error) -> Option<bson::oid::ObjectId> {
    use roomler_ai_remote_control::Error::*;
    match e {
        SessionNotFound(hex) => bson::oid::ObjectId::parse_str(hex).ok(),
        BadPhase(hex, _) => bson::oid::ObjectId::parse_str(hex).ok(),
        _ => None,
    }
}

/// Intercept tunnel-flow `ClientMsg` variants from the agent and route
/// the corresponding `ServerMsg` to the registered tunnel-client (if
/// any) keyed by `session_id`. Non-tunnel variants are returned
/// unchanged so the caller can pass them to the Hub.
///
/// Returns `None` if the message was consumed by the tunnel relay
/// (don't dispatch to the Hub afterwards), or `Some(parsed)` if the
/// caller should continue with Hub dispatch.
pub(crate) async fn relay_tunnel_msg_from_agent(
    state: &AppState,
    parsed: ClientMsg,
) -> Option<ClientMsg> {
    match parsed {
        ClientMsg::TcpForwardAccept {
            session_id,
            flow_id,
            dc_index,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpForwardAccept {
                    session_id,
                    flow_id,
                    dc_index,
                },
            )
            .await;
            None
        }
        ClientMsg::TcpForwardReject {
            session_id,
            flow_id,
            kind,
            reason,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpForwardReject {
                    session_id,
                    flow_id,
                    kind,
                    reason,
                },
            )
            .await;
            None
        }
        ClientMsg::TcpHalfClose {
            session_id,
            flow_id,
            direction,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpHalfClose {
                    session_id,
                    flow_id,
                    direction,
                },
            )
            .await;
            None
        }
        // The AGENT end of a flow closing. Byte counts are deliberately
        // ignored here: the audit row is written once, from the
        // ORIGINATOR's close (see `ws::tunnel::audit_tcp_close`), and
        // booking both ends would double every flow's volume.
        ClientMsg::TcpClosed {
            session_id,
            flow_id,
            reason,
            ..
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpClosed {
                    session_id,
                    flow_id,
                    reason,
                },
            )
            .await;
            None
        }
        // UDP ASSOCIATE relays — mirror the Tcp* variants above. The
        // agent bound a UDP socket (Accept) / rejected / closed a UDP
        // flow; relay each to the tunnel-client by session_id.
        ClientMsg::UdpForwardAccept {
            session_id,
            flow_id,
            dc_index,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::UdpForwardAccept {
                    session_id,
                    flow_id,
                    dc_index,
                },
            )
            .await;
            None
        }
        ClientMsg::UdpForwardReject {
            session_id,
            flow_id,
            kind,
            reason,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::UdpForwardReject {
                    session_id,
                    flow_id,
                    kind,
                    reason,
                },
            )
            .await;
            None
        }
        ClientMsg::UdpClosed {
            session_id,
            flow_id,
            reason,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::UdpClosed {
                    session_id,
                    flow_id,
                    reason,
                },
            )
            .await;
            None
        }
        ClientMsg::TunnelTerminate { session_id, reason } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelTerminate { session_id, reason },
            )
            .await;
            None
        }
        ClientMsg::TunnelSdpAnswer { session_id, sdp } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelSdpAnswer { session_id, sdp },
            )
            .await;
            None
        }
        ClientMsg::TunnelIce {
            session_id,
            candidate,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelIce {
                    session_id,
                    candidate,
                },
            )
            .await;
            None
        }
        // Phase 1c: the agent's QUIC endpoint is up — relay its cert
        // fingerprint (for the client to pin) + dialable addrs to the
        // tunnel-client so it can connect the direct P2P QUIC link.
        ClientMsg::TunnelQuicReady {
            session_id,
            cert_fingerprint,
            addrs,
            derp_pubkey,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelQuicReady {
                    session_id,
                    cert_fingerprint,
                    addrs,
                    // R4 — relayed verbatim; the client needs the agent's
                    // DERP identity to dial the quic-derp-v1 leg.
                    derp_pubkey,
                },
            )
            .await;
            None
        }
        // `TunnelHello` / `TunnelOpen` / `TcpForwardRequest` /
        // `TunnelSdpOffer` are tunnel-client → server messages;
        // agents shouldn't emit them. Pass through to the Hub so a
        // misbehaving agent gets a `bad_message` rather than being
        // silently dropped.
        other => Some(other),
    }
}

/// Push a `ServerMsg` to the tunnel-client registered for
/// `session_id`. No-op when the client has gone away (peer torn
/// down between agent emit + relay).
async fn relay_to_client(state: &AppState, session_id: bson::oid::ObjectId, msg: ServerMsg) {
    let Some(tx) = state
        .tunnel_clients_by_session
        .get(&session_id)
        .map(|entry| entry.value().clone())
    else {
        debug!(%session_id, "agent → client relay: no registered tunnel-client; dropping");
        // C-3 split evidence: if ANOTHER pod owns this session's record,
        // the drop was a cross-pod split (the agent's WS re-homed away
        // from the client's pod), not a torn-down client. Counted like
        // the A2b agent probe; throttled to 1/5 s.
        if let Some(dir) = state.cluster_directory.clone() {
            static LAST_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

            let now = bson::DateTime::now().timestamp_millis();
            let last = LAST_MS.load(std::sync::atomic::Ordering::Relaxed);
            if now - last >= 5_000
                && LAST_MS
                    .compare_exchange(
                        last,
                        now,
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
            {
                tokio::spawn(async move {
                    let key = crate::cluster::directory::tunnel_key(&session_id.to_hex());
                    if let Ok(Some(owner)) = dir.get(&key).await
                        && dir.is_foreign(&owner)
                    {
                        let total = crate::cluster::metrics::SPLIT_EVIDENCE_TOTAL
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        warn!(
                            session = %session_id, owner = %owner, total,
                            "SPLIT EVIDENCE: tunnel relay dropped but another pod owns the session"
                        );
                    }
                });
            }
        }
        return;
    };
    if let Err(e) = tx.send(msg).await {
        debug!(%session_id, %e, "agent → client relay: channel closed");
    }
}
