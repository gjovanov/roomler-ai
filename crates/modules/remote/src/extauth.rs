// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-52 P3c — the server's half of an external-access login: a BLIND relay.
//!
//! An outsider's browser and a device run an OPAQUE login through this pod. The
//! server resolves the connect code, applies the two gates it owns (the org's
//! switch, the per-device approval), forwards the three login messages, and
//! learns nothing it could use: not the password, nothing to attack offline, no
//! proof it could replay. The device verifies; the server only carries.
//!
//! # Two rules that shape every function here
//!
//! 1. **One answer for every refusal the outsider must not tell apart** —
//!    [`ExtauthRefusal::Unavailable`] for no-such-code, gate 1 off, gate 2
//!    closed or expired, offline, too old. The org's audit log is where they
//!    are told apart (`login_detail`); the outsider never sees the difference.
//!    And the same LATENCY as far as that is cheap to buy: the reply goes out
//!    BEFORE the audit write (a write only resolvable codes trigger), and an
//!    unresolvable code still pays the tenant read a resolvable one does.
//! 2. **Park before the push.** The device can answer faster than this
//!    function gets back from `send_to_agent`; the waiter is registered first,
//!    so an early answer finds somewhere to land (FR-83's rule).
//!
//! # What is not here yet
//!
//! The cross-pod path. An outsider has no tenant, so tenant affinity cannot put
//! their socket on the pod that holds the device, and roughly half of them will
//! land elsewhere. Until the PR-2 relay carries these frames (P3c-4), such a
//! device answers `unavailable` with `login_detail` saying why. Nothing sends
//! `rc:extauth.start` in production before P4's connect page exists.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use dashmap::DashMap;
use roomler_ai_remote_control::connect_code;
use roomler_ai_remote_control::models::{
    Agent, ExtauthRefusal, ExternalRcAuditAction, ExternalRcAuditEvent, RpcCap,
};
use roomler_ai_remote_control::session::ClientTx;
use roomler_ai_remote_control::signaling::ServerMsg;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::RemoteState;

/// How long the server waits for the device to answer one login step. The
/// device's own work is an OPRF evaluation and a 3DH; this covers a slow
/// relayed path to it, not computation.
pub const DEVICE_ANSWER_BOUND: Duration = Duration::from_secs(10);
/// How long an attempt may sit between the challenge and the controller's
/// KE3. Longer than the device's own pending TTL (60 s), so the device — the
/// party that counts guesses — is always the one that expires it first.
pub const ATTEMPT_TTL: Duration = Duration::from_secs(90);
/// The server's own ceiling on starts per principal per minute: the SECOND
/// limit (§4b). The device's budget is the one that protects the password;
/// this one bounds relay traffic and audit rows per account.
pub const STARTS_PER_MINUTE: u32 = 10;

/// What the device said about one login step.
#[derive(Debug)]
pub enum DeviceAnswer {
    Ke2(String),
    Outcome {
        refused: Option<ExtauthRefusal>,
        retry_after_secs: Option<u32>,
    },
}

struct Waiter {
    agent_id: ObjectId,
    tx: oneshot::Sender<DeviceAnswer>,
}

struct Attempt {
    agent_id: ObjectId,
    principal: ObjectId,
    started: Instant,
}

/// Attempts in flight and the waiters for the device's answers. Pod-local.
#[derive(Default)]
pub struct ExtauthRelay {
    waiters: DashMap<String, Waiter>,
    attempts: DashMap<String, Attempt>,
    /// principal → (window start, starts in it).
    starts: std::sync::Mutex<HashMap<ObjectId, (Instant, u32)>>,
}

impl ExtauthRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the waiter for `attempt_id`'s next answer — BEFORE the push.
    fn expect(self: &Arc<Self>, attempt_id: &str, agent_id: ObjectId) -> PendingAnswer {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .insert(attempt_id.to_string(), Waiter { agent_id, tx });
        PendingAnswer {
            relay: Arc::clone(self),
            attempt_id: attempt_id.to_string(),
            rx,
        }
    }

    /// Hand the device's answer to whoever is waiting for it.
    ///
    /// ⚠️ Honoured ONLY from the agent the step was sent to. Attempt ids are
    /// relayed to both parties and are not secret; `from_agent` comes from the
    /// authenticated socket, never from the frame.
    pub fn deliver(&self, attempt_id: &str, from_agent: ObjectId, answer: DeviceAnswer) -> bool {
        match self
            .waiters
            .remove_if(attempt_id, |_, w| w.agent_id == from_agent)
        {
            Some((_, w)) => w.tx.send(answer).is_ok(),
            None => false,
        }
    }

    /// The server's second limit. Fixed one-minute windows per principal.
    fn allow_start(&self, principal: ObjectId, now: Instant) -> bool {
        let mut starts = self
            .starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Bounded: a window older than a minute is dead weight.
        starts.retain(|_, (since, _)| {
            now.saturating_duration_since(*since) < Duration::from_secs(60)
        });
        let entry = starts.entry(principal).or_insert((now, 0));
        if entry.1 >= STARTS_PER_MINUTE {
            return false;
        }
        entry.1 += 1;
        true
    }

    /// Drop attempts nobody finished. Ages via `saturating_duration_since`,
    /// never `now - d`.
    fn prune(&self, now: Instant) {
        self.attempts
            .retain(|_, a| now.saturating_duration_since(a.started) <= ATTEMPT_TTL);
    }

    /// Attempts currently in flight. For tests and gauges.
    pub fn attempts_in_flight(&self) -> usize {
        self.attempts.len()
    }
}

/// A registered waiter. Dropping it deregisters, so an early return anywhere
/// cannot leave a slot a late answer would land in.
struct PendingAnswer {
    relay: Arc<ExtauthRelay>,
    attempt_id: String,
    rx: oneshot::Receiver<DeviceAnswer>,
}

impl PendingAnswer {
    async fn wait(mut self, bound: Duration) -> Option<DeviceAnswer> {
        tokio::time::timeout(bound, &mut self.rx).await.ok()?.ok()
    }
}

impl Drop for PendingAnswer {
    fn drop(&mut self) {
        self.relay.waiters.remove(&self.attempt_id);
    }
}

fn result(
    attempt_id: Option<String>,
    refused: Option<ExtauthRefusal>,
    retry_after_secs: Option<u32>,
) -> ServerMsg {
    ServerMsg::ExtauthResult {
        attempt_id,
        refused,
        retry_after_secs,
    }
}

fn unavailable(attempt_id: Option<String>) -> ServerMsg {
    result(attempt_id, Some(ExtauthRefusal::Unavailable), None)
}

/// Gates 1 and 2, plus "can this device do it at all". `Err` carries what the
/// org's audit log records; the outsider sees only `unavailable`.
async fn gates(state: &RemoteState, agent: &Agent) -> Result<(), &'static str> {
    let org_on = state
        .fleet
        .tenants
        .base
        .find_by_id(agent.tenant_id)
        .await
        .map(|t| t.settings.external_rc_enabled)
        .unwrap_or(false);
    if !org_on {
        return Err("gate 1: the organization has external access switched off");
    }
    let (gates, _) = agent.external_access_policy.clone().split();
    if !gates.is_open_at(bson::DateTime::now()) {
        return Err(
            "gate 2: the device is not approved for external access, or the approval expired",
        );
    }
    // The same source gate 2's APPROVAL read (P1): the capabilities the agent
    // reported on its last hello. A device downgraded since then still claims
    // the verb here; it then never answers, and the bound below turns that into
    // `unavailable` — slower, never wrong.
    if !agent.capabilities.has_rpc(RpcCap::ExternalAccess) {
        return Err("the device's agent is too old to be sent an external login");
    }
    Ok(())
}

/// Resolve a connect code to its live device.
///
/// ⚠️ An unresolvable code still pays one tenant read, so it costs roughly
/// what a resolvable-but-gated code costs before the reply goes out. Cheap to
/// buy, and it keeps "no such code" from being the one fast answer.
async fn resolve(state: &RemoteState, typed: &str) -> Option<Agent> {
    let found = match connect_code::normalize(typed) {
        Some(code) => state
            .fleet
            .agents
            .find_by_connect_code(&code)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    if found.is_none() {
        let _ = state
            .fleet
            .tenants
            .base
            .find_by_id(ObjectId::from_bytes([0; 12]))
            .await;
    }
    found
}

/// Written AFTER the reply, so its latency is not part of the answer (§5).
#[allow(clippy::too_many_arguments)]
fn audit(
    state: &RemoteState,
    agent: &Agent,
    agent_id: ObjectId,
    principal: ObjectId,
    actor: &str,
    attempt_id: Option<String>,
    refused: Option<ExtauthRefusal>,
    detail: Option<&str>,
) {
    let event = ExternalRcAuditEvent {
        id: None,
        tenant_id: agent.tenant_id,
        action: ExternalRcAuditAction::Login,
        agent_id,
        user_id: principal,
        actor: actor.to_string(),
        approved: None,
        max_permissions: None,
        expires_at: None,
        at: bson::DateTime::now(),
        denied: None,
        attempt_id,
        login_refused: refused,
        login_detail: detail.map(str::to_string),
    };
    let dao = Arc::clone(&state.fleet.external_rc_audit);
    tokio::spawn(async move {
        if let Err(e) = dao.record(event).await {
            warn!(%e, "extauth: the login audit row could not be written");
        }
    });
}

/// `rc:extauth.start` — resolve, gate, relay KE1, relay the device's answer.
pub async fn handle_start(
    state: &RemoteState,
    principal: ObjectId,
    actor: &str,
    tx: &ClientTx,
    connect_code: &str,
    ke1: String,
) {
    let relay = &state.extauth;
    let now = Instant::now();
    relay.prune(now);
    if !relay.allow_start(principal, now) {
        let _ = tx.try_send(result(None, Some(ExtauthRefusal::RateLimited), None));
        return;
    }
    let Some(agent) = resolve(state, connect_code).await else {
        let _ = tx.try_send(unavailable(None));
        return;
    };
    let Some(agent_id) = agent.id else {
        let _ = tx.try_send(unavailable(None));
        return;
    };
    if let Err(detail) = gates(state, &agent).await {
        let _ = tx.try_send(unavailable(None));
        info!(%principal, agent = %agent_id, detail, "extauth: start refused");
        audit(
            state,
            &agent,
            agent_id,
            principal,
            actor,
            None,
            Some(ExtauthRefusal::Unavailable),
            Some(detail),
        );
        return;
    }
    if !state.fleet.rc_hub.is_agent_online(agent_id) {
        let detail = "the device is not connected to this pod (offline, or homed on another pod — the cross-pod path is P3c-4)";
        let _ = tx.try_send(unavailable(None));
        audit(
            state,
            &agent,
            agent_id,
            principal,
            actor,
            None,
            Some(ExtauthRefusal::Unavailable),
            Some(detail),
        );
        return;
    }

    let attempt_id = ObjectId::new().to_hex();
    relay.attempts.insert(
        attempt_id.clone(),
        Attempt {
            agent_id,
            principal,
            started: now,
        },
    );
    // Park BEFORE the push (module docs, rule 2).
    let pending = relay.expect(&attempt_id, agent_id);
    let push = ServerMsg::ExtauthKe1 {
        attempt_id: attempt_id.clone(),
        principal: principal.to_hex(),
        ke1,
    };
    if state
        .fleet
        .rc_hub
        .send_to_agent_in_tenant(agent_id, agent.tenant_id, push)
        .is_err()
    {
        relay.attempts.remove(&attempt_id);
        let _ = tx.try_send(unavailable(None));
        audit(
            state,
            &agent,
            agent_id,
            principal,
            actor,
            Some(attempt_id),
            Some(ExtauthRefusal::Unavailable),
            Some("the device went away before KE1 could be sent"),
        );
        return;
    }

    match pending.wait(DEVICE_ANSWER_BOUND).await {
        Some(DeviceAnswer::Ke2(ke2)) => {
            // The attempt stays for the controller's KE3.
            let _ = tx.try_send(ServerMsg::ExtauthChallenge { attempt_id, ke2 });
        }
        Some(DeviceAnswer::Outcome {
            refused,
            retry_after_secs,
        }) => {
            relay.attempts.remove(&attempt_id);
            // ⚠️ "Verified" in answer to a KE1 is protocol nonsense — nothing
            // has been proven yet. Relayed as a refusal, never as a login.
            let refused = Some(refused.unwrap_or(ExtauthRefusal::Other));
            let _ = tx.try_send(result(Some(attempt_id.clone()), refused, retry_after_secs));
            audit(
                state,
                &agent,
                agent_id,
                principal,
                actor,
                Some(attempt_id),
                refused,
                None,
            );
        }
        None => {
            relay.attempts.remove(&attempt_id);
            let _ = tx.try_send(unavailable(Some(attempt_id.clone())));
            audit(
                state,
                &agent,
                agent_id,
                principal,
                actor,
                Some(attempt_id),
                Some(ExtauthRefusal::Unavailable),
                Some("the device did not answer KE1 in time"),
            );
        }
    }
}

/// `rc:extauth.finish` — relay KE3, relay the verdict.
pub async fn handle_finish(
    state: &RemoteState,
    principal: ObjectId,
    actor: &str,
    tx: &ClientTx,
    connect_code: &str,
    attempt_id: String,
    ke3: String,
) {
    let relay = &state.extauth;
    relay.prune(Instant::now());
    // Taken, not read: whatever happens next, this attempt ends here. Only its
    // own principal can take it — anyone else leaves it in place and is told
    // it does not exist.
    let Some((_, attempt)) = relay
        .attempts
        .remove_if(&attempt_id, |_, a| a.principal == principal)
    else {
        let _ = tx.try_send(result(
            Some(attempt_id),
            Some(ExtauthRefusal::UnknownAttempt),
            None,
        ));
        return;
    };
    // The code must still name the attempt's device, and the gates must still
    // be open: an admin who revokes mid-login must not see it complete.
    let agent = match resolve(state, connect_code).await {
        Some(a) if a.id == Some(attempt.agent_id) => a,
        _ => {
            let _ = tx.try_send(result(
                Some(attempt_id),
                Some(ExtauthRefusal::UnknownAttempt),
                None,
            ));
            return;
        }
    };
    let agent_id = attempt.agent_id;
    if let Err(detail) = gates(state, &agent).await {
        let _ = tx.try_send(unavailable(Some(attempt_id.clone())));
        audit(
            state,
            &agent,
            agent_id,
            principal,
            actor,
            Some(attempt_id),
            Some(ExtauthRefusal::Unavailable),
            Some(detail),
        );
        return;
    }

    let pending = relay.expect(&attempt_id, agent_id);
    let push = ServerMsg::ExtauthKe3 {
        attempt_id: attempt_id.clone(),
        principal: principal.to_hex(),
        ke3,
    };
    if state
        .fleet
        .rc_hub
        .send_to_agent_in_tenant(agent_id, agent.tenant_id, push)
        .is_err()
    {
        let _ = tx.try_send(unavailable(Some(attempt_id.clone())));
        audit(
            state,
            &agent,
            agent_id,
            principal,
            actor,
            Some(attempt_id),
            Some(ExtauthRefusal::Unavailable),
            Some("the device went away before KE3 could be sent"),
        );
        return;
    }

    match pending.wait(DEVICE_ANSWER_BOUND).await {
        Some(DeviceAnswer::Outcome {
            refused,
            retry_after_secs,
        }) => {
            let _ = tx.try_send(result(Some(attempt_id.clone()), refused, retry_after_secs));
            if refused.is_none() {
                info!(%principal, agent = %agent_id, %attempt_id, "extauth: login VERIFIED by the device");
            }
            audit(
                state,
                &agent,
                agent_id,
                principal,
                actor,
                Some(attempt_id),
                refused,
                None,
            );
        }
        // A KE2 in answer to a KE3 is protocol nonsense: a refusal.
        Some(DeviceAnswer::Ke2(_)) => {
            let refused = Some(ExtauthRefusal::Other);
            let _ = tx.try_send(result(Some(attempt_id.clone()), refused, None));
            audit(
                state,
                &agent,
                agent_id,
                principal,
                actor,
                Some(attempt_id),
                refused,
                Some("the device answered KE3 with a KE2"),
            );
        }
        None => {
            let _ = tx.try_send(unavailable(Some(attempt_id.clone())));
            audit(
                state,
                &agent,
                agent_id,
                principal,
                actor,
                Some(attempt_id),
                Some(ExtauthRefusal::Unavailable),
                Some("the device did not answer KE3 in time"),
            );
        }
    }
}

/// Route a controller frame here if it is an `rc:extauth.*` frame. `None` =
/// not ours; `Some(true)` = handled (or deliberately swallowed).
///
/// Runs BEFORE the session authz gate: an outsider has no membership for that
/// gate to check — their gates are the org's switch, the device's approval and
/// the password. And each step is SPAWNED: it waits on the device for up to
/// [`DEVICE_ANSWER_BOUND`], and the rest of this socket's frames must not
/// queue behind it.
pub fn intercept(
    state: &RemoteState,
    frame: &crate::controller::ControllerFrame<'_>,
) -> Option<bool> {
    // Cheap pre-filter: most rc:* traffic is SDP and ICE.
    if !frame.text.contains("rc:extauth.") {
        return None;
    }
    let msg =
        serde_json::from_str::<roomler_ai_remote_control::signaling::ClientMsg>(frame.text).ok()?;
    use roomler_ai_remote_control::signaling::ClientMsg;
    let (state, principal, actor, tx) = (
        state.clone(),
        frame.user_id,
        frame.controller_name.to_string(),
        frame.controller_tx.clone(),
    );
    match msg {
        ClientMsg::ExtauthStart { connect_code, ke1 } => {
            tokio::spawn(async move {
                handle_start(&state, principal, &actor, &tx, &connect_code, ke1).await;
            });
            Some(true)
        }
        ClientMsg::ExtauthFinish {
            connect_code,
            attempt_id,
            ke3,
        } => {
            tokio::spawn(async move {
                handle_finish(
                    &state,
                    principal,
                    &actor,
                    &tx,
                    &connect_code,
                    attempt_id,
                    ke3,
                )
                .await;
            });
            Some(true)
        }
        // A DEVICE's frames arriving on a USER socket: a browser trying to
        // speak for a device. Swallowed — never delivered, never dispatched.
        ClientMsg::ExtauthKe2 { .. } | ClientMsg::ExtauthOutcome { .. } => {
            warn!(%principal, "extauth: a device frame arrived on a user socket — dropped");
            Some(true)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_millis(200);

    #[tokio::test]
    async fn the_devices_answer_resolves_the_wait() {
        let relay = Arc::new(ExtauthRelay::new());
        let device = ObjectId::new();
        let pending = relay.expect("a1", device);
        assert!(relay.deliver("a1", device, DeviceAnswer::Ke2("k2".into())));
        assert!(matches!(pending.wait(SHORT).await, Some(DeviceAnswer::Ke2(k)) if k == "k2"));
    }

    /// Attempt ids are not secret: an answer from any OTHER agent confirms
    /// nothing and leaves the slot for the right one.
    #[tokio::test]
    async fn an_answer_from_another_agent_lands_nowhere() {
        let relay = Arc::new(ExtauthRelay::new());
        let (device, stranger) = (ObjectId::new(), ObjectId::new());
        let pending = relay.expect("a1", device);
        let verified = || DeviceAnswer::Outcome {
            refused: None,
            retry_after_secs: None,
        };
        assert!(
            !relay.deliver("a1", stranger, verified()),
            "a stranger's verdict"
        );
        assert!(relay.deliver("a1", device, verified()));
        assert!(matches!(
            pending.wait(SHORT).await,
            Some(DeviceAnswer::Outcome { refused: None, .. })
        ));
    }

    #[tokio::test]
    async fn silence_is_no_answer_and_the_slot_is_released() {
        let relay = Arc::new(ExtauthRelay::new());
        let pending = relay.expect("a1", ObjectId::new());
        assert!(pending.wait(SHORT).await.is_none());
        assert!(relay.waiters.is_empty());
    }

    #[test]
    fn the_server_limit_counts_per_principal_and_resets_each_minute() {
        let relay = ExtauthRelay::new();
        let (a, b) = (ObjectId::new(), ObjectId::new());
        let t0 = Instant::now();
        for _ in 0..STARTS_PER_MINUTE {
            assert!(relay.allow_start(a, t0));
        }
        assert!(!relay.allow_start(a, t0), "the eleventh start in a minute");
        assert!(
            relay.allow_start(b, t0),
            "another principal has its own window"
        );
        assert!(
            relay.allow_start(a, t0 + Duration::from_secs(61)),
            "a new window"
        );
    }
}
