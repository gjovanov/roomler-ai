//! `Hub` — process-global registry of online agents and live sessions.
//!
//! The Hub is `Clone`-able (it's a thin Arc handle) and is shared across all
//! Axum WS handlers. It owns:
//!
//! - `agents`:   `agent_id` → connected agent's tx + metadata
//! - `sessions`: `session_id` → live session state
//!
//! All concurrent access goes through `DashMap` so we don't take a global
//! lock on every signaling message. The session inner state is wrapped in
//! `parking_lot::Mutex` because it's read+modified together (state machine).

use bson::oid::ObjectId;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

use crate::audit::AuditSink;
use crate::consent::{ConsentOutcome, DEFAULT_CONSENT_TIMEOUT};
use crate::error::{Error, Result};
use crate::models::{AuditKind, ConsentMode, EndReason, OsKind, SessionPhase};
use crate::permissions::Permissions;
use crate::session::{ClientTx, LiveSession};
use crate::signaling::{AgentCloseReason, ClientMsg, Role, ServerMsg};
use crate::turn_creds::{TurnConfig, ice_servers_for};

const SERVER_TX_CAPACITY: usize = 64;

/// Consent window for owner-side channels (Email/Push) — the owner has to read a
/// mail / tap a push, so it's far longer than the 30 s on-host prompt
/// ([`DEFAULT_CONSENT_TIMEOUT`]). Also bounds the `ConsentRequest` link TTL.
const ASYNC_CONSENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

// ────────────────────────────────────────────────────────────────────────────

pub struct ConnectedAgent {
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub owner_user_id: ObjectId,
    pub os: OsKind,
    pub tx: ClientTx,
    pub active_sessions: u8,
    pub max_sessions: u8,
    /// rc.53: signalled by [`Hub::register_agent`] when a newer
    /// connection displaces this one. The WS handler's read-loop
    /// `select!`s on `socket_rx.next()` AND `cancel.notified()`, so
    /// the displaced socket exits cleanly within milliseconds of the
    /// displacement Goodbye landing — instead of lingering up to one
    /// 25 s keepalive interval waiting for a ping-send to fail.
    pub cancel: Arc<Notify>,
}

pub struct ConnectedController {
    pub user_id: ObjectId,
    pub tx: ClientTx,
}

/// Emitted by [`Hub::create_session`] to have the API layer notify the device
/// OWNER. Two flavours, distinguished by `override_reason`:
/// * `None` — Email/Push consent modes: the owner must APPROVE (the API persists
///   a `ConsentRequest` + sends the approve-link; the owner's approve/deny later
///   calls [`Hub::deliver_consent`], resolving the SAME slot the waiter awaits).
/// * `Some(reason)` — an admin break-glass already happened: an INFORMATIONAL
///   "your device was accessed" notice (no approval needed, no token).
///
/// The Hub stays DB-agnostic — it just publishes; the API consumer resolves the
/// owner from `agent_id`.
#[derive(Debug, Clone)]
pub struct ConsentEvent {
    pub session_id: ObjectId,
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub controller_user_id: ObjectId,
    pub controller_name: String,
    pub mode: ConsentMode,
    /// Same window the waiter uses — the consumer sizes the `ConsentRequest`
    /// TTL to match so a stale link can't resolve a long-gone session.
    pub timeout_secs: u32,
    /// `Some` ⇒ this is a break-glass NOTICE (already-granted), carrying the
    /// admin's reason; `None` ⇒ an Email/Push APPROVAL request.
    pub override_reason: Option<String>,
}

pub struct HubInner {
    /// Online agents, keyed by agent_id.
    agents: DashMap<ObjectId, ConnectedAgent>,

    /// Live sessions (anything not yet `Closed`).
    sessions: DashMap<ObjectId, Arc<Mutex<LiveSession>>>,

    /// Per-controller-user connections (one user may have multiple browser tabs).
    controllers: DashMap<ObjectId, Vec<ClientTx>>,

    /// TURN issuance.
    turn: Option<TurnConfig>,

    /// Audit sink.
    audit: AuditSink,

    /// Phase 4 — async-consent event sink (Email/Push). `None` (tests / no email
    /// configured) drops the events and those sessions fall back to the waiter
    /// timeout.
    consent_tx: Option<mpsc::Sender<ConsentEvent>>,
}

#[derive(Clone)]
pub struct Hub {
    inner: Arc<HubInner>,
}

impl Hub {
    pub fn new(audit: AuditSink, turn: Option<TurnConfig>) -> Self {
        Self::new_with_consent(audit, turn, None)
    }

    /// Like [`Hub::new`] but wires the Phase-4 async-consent event sender.
    pub fn new_with_consent(
        audit: AuditSink,
        turn: Option<TurnConfig>,
        consent_tx: Option<mpsc::Sender<ConsentEvent>>,
    ) -> Self {
        Self {
            inner: Arc::new(HubInner {
                agents: DashMap::new(),
                sessions: DashMap::new(),
                controllers: DashMap::new(),
                turn,
                audit,
                consent_tx,
            }),
        }
    }

    // ─── connection registration ──────────────────────────────────────

    /// Called by the WS handler when an agent finishes auth+hello.
    /// Returns `(tx, cancel, rx)`:
    ///
    ///   * `tx` — clone of the channel registered in `ConnectedAgent.tx`.
    ///     The WS handler captures this so its later `unregister_agent`
    ///     call can identify "still my entry?" via `ClientTx::same_channel`
    ///     and avoid evicting a NEWER displacing connection's entry
    ///     (rc.53 race fix; the pre-rc.53 unregister unconditionally
    ///     removed by `agent_id`).
    ///   * `cancel` — `Arc<Notify>` the WS handler `select!`s on so a
    ///     displacement triggers an immediate read-loop exit, NOT a 25 s
    ///     wait for the agent's own keepalive ping to fail.
    ///   * `rx` — pump source for `pump_server_messages`.
    pub fn register_agent(
        &self,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        owner_user_id: ObjectId,
        os: OsKind,
        max_sessions: u8,
    ) -> (ClientTx, Arc<Notify>, mpsc::Receiver<ServerMsg>) {
        let (tx, rx) = mpsc::channel(SERVER_TX_CAPACITY);
        let cancel = Arc::new(Notify::new());
        let entry = ConnectedAgent {
            agent_id,
            tenant_id,
            owner_user_id,
            os,
            tx: tx.clone(),
            active_sessions: 0,
            max_sessions,
            cancel: cancel.clone(),
        };
        if let Some(prev) = self.inner.agents.insert(agent_id, entry) {
            // rc.53: don't just `drop(prev)` — that leaves the old WS
            // read-loop polling `socket_rx.next()` for up to one 25 s
            // keepalive interval before the agent's own ping fails and
            // surfaces as a transient. Instead:
            //   1. Push a structured `ServerMsg::Goodbye {
            //      reason: ReplacedByNewerConnection }` via try_send.
            //   2. `notify_waiters()` on the cancel so the read-loop
            //      exits within milliseconds.
            //   3. drop(prev) — closes the channel; pump task exits
            //      cleanly after forwarding the Goodbye to the socket.
            warn!(
                "agent {} reconnected; notifying previous connection with ReplacedByNewerConnection and dropping",
                agent_id
            );
            let goodbye = ServerMsg::Goodbye {
                reason: AgentCloseReason::ReplacedByNewerConnection,
                message: format!(
                    "Another agent is connecting with the same agent_id ({agent_id}); \
                     this connection is being closed. Check for a duplicate install \
                     (another physical host with a copy of this config.toml, the tray \
                     companion, etc.) or re-enrol to mint a fresh agent_id."
                ),
            };
            match prev.tx.try_send(goodbye) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // SERVER_TX_CAPACITY is 64; under contention the pump
                    // may not have drained yet. Operator sees this and
                    // knows the displaced agent likely missed the message
                    // and will reconnect via the raw-close path.
                    warn!(
                        "agent {agent_id} displacement goodbye dropped (channel full); \
                         displaced agent will see raw close only"
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    // Displaced agent's pump task already exited; the
                    // socket-close path will fire on its next read.
                }
            }
            // `notify_one` (NOT notify_waiters) — the former LATCHES the
            // notification when no waiter is parked, the latter just
            // broadcasts to currently-parked waiters. Auto-fail #5 in
            // the v2 plan: if the displacement fires before the
            // displaced handler entered its select-loop (extreme race),
            // notify_waiters would lose the signal and the old socket
            // would linger waiting on its 25 s keepalive. notify_one's
            // permit storage closes that hole.
            prev.cancel.notify_one();
            drop(prev);
        }
        info!("agent {} online", agent_id);
        (tx, cancel, rx)
    }

    /// Unregister a connected agent. The optional `tx` is the channel
    /// the caller captured at registration time; when provided, the
    /// removal happens ONLY if the currently-registered tx is still
    /// the same channel. This protects against a pre-existing race
    /// (rc.53 critique #4) where a displaced `handle_agent_socket`'s
    /// late unregister evicted the NEWER displacing connection's
    /// registry entry, killing its in-flight sessions.
    ///
    /// Pass `None` for admin-driven kicks (e.g. the kick-agent route)
    /// that don't carry the agent's tx identity — these always
    /// remove unconditionally.
    pub fn unregister_agent(&self, agent_id: ObjectId, tx: Option<&ClientTx>) {
        if let Some(expected_tx) = tx {
            // Identity-gated removal. Read the current entry's tx
            // under the DashMap lock and compare; if it doesn't match
            // ours, a newer connection has already taken over this
            // slot — leave it alone.
            let still_ours = self
                .inner
                .agents
                .get(&agent_id)
                .map(|a| ptr_eq(&a.tx, expected_tx))
                .unwrap_or(false);
            if !still_ours {
                info!(
                    "agent {} unregister skipped (tx no longer matches; newer connection holds the slot)",
                    agent_id
                );
                return;
            }
        }
        if self.inner.agents.remove(&agent_id).is_some() {
            info!("agent {} offline", agent_id);
            // Force-close any sessions tied to this agent.
            let dead: Vec<ObjectId> = self
                .inner
                .sessions
                .iter()
                .filter_map(|s| {
                    let live = s.value().lock();
                    (live.agent_id == agent_id).then_some(*s.key())
                })
                .collect();
            for sid in dead {
                let _ = self.terminate(sid, EndReason::AgentDisconnect);
            }
        }
    }

    /// Called by the WS handler when a controller browser tab connects.
    pub fn register_controller(&self, user_id: ObjectId) -> (ClientTx, mpsc::Receiver<ServerMsg>) {
        let (tx, rx) = mpsc::channel(SERVER_TX_CAPACITY);
        self.inner
            .controllers
            .entry(user_id)
            .or_default()
            .push(tx.clone());
        (tx, rx)
    }

    pub fn unregister_controller(&self, user_id: ObjectId, tx: &ClientTx) {
        if let Some(mut list) = self.inner.controllers.get_mut(&user_id) {
            list.retain(|t| !ptr_eq(t, tx));
        }

        // Terminate any sessions this controller still owns. Without this
        // the agent's active_sessions counter never drops when a browser
        // tab closes mid-session, and subsequent Connect attempts fail
        // with AgentBusy until the agent itself disconnects.
        let orphaned: Vec<ObjectId> = self
            .inner
            .sessions
            .iter()
            .filter(|e| e.value().lock().controller_user_id == user_id)
            .map(|e| *e.key())
            .collect();
        for session_id in orphaned {
            let _ = self.terminate(session_id, EndReason::ControllerHangup);
        }
    }

    // ─── session lifecycle ────────────────────────────────────────────

    /// Controller asked to start a session against `agent_id`.
    /// Creates the session, notifies the agent, returns the new session id.
    /// The caller (WS dispatcher) is expected to follow up by awaiting consent
    /// in a spawned task — see [`Self::run_consent_flow`].
    // Each new field on `rc:session.request` lands as another arg
    // here. Clippy's 7-arg threshold is conservative for an internal
    // helper whose call sites are exhaustive; allowing it is cheaper
    // than building a builder we'd never use elsewhere.
    #[allow(clippy::too_many_arguments)]
    pub fn create_session(
        &self,
        agent_id: ObjectId,
        controller_user_id: ObjectId,
        controller_name: String,
        controller_tx: ClientTx,
        permissions: Permissions,
        browser_caps: Vec<String>,
        preferred_transport: Option<String>,
        chroma_pref: Option<String>,
        audio_enabled: bool,
        consent_mode: ConsentMode,
        override_reason: Option<String>,
    ) -> Result<ObjectId> {
        let agent_org = {
            let mut agent = self
                .inner
                .agents
                .get_mut(&agent_id)
                .ok_or_else(|| Error::AgentOffline(agent_id.to_hex()))?;
            if agent.active_sessions >= agent.max_sessions {
                return Err(Error::AgentBusy);
            }
            agent.active_sessions += 1;
            agent.tenant_id
        };

        let session_id = ObjectId::new();
        let (live, waiter) = LiveSession::new(
            session_id,
            agent_id,
            agent_org,
            controller_user_id,
            permissions,
            controller_tx.clone(),
        );
        self.inner
            .sessions
            .insert(session_id, Arc::new(Mutex::new(live)));

        // Tell the controller the session id.
        let _ = controller_tx.try_send(ServerMsg::SessionCreated {
            session_id,
            agent_id,
        });

        // Per-mode consent window: modes with an owner-side (email/push) leg get
        // a much longer window than the pure on-host prompt — `PromptThenEmail`
        // included, since its host prompt runs alongside the emailed link and the
        // owner needs time to act.
        let timeout = match consent_mode {
            ConsentMode::Email | ConsentMode::Push | ConsentMode::PromptThenEmail => {
                ASYNC_CONSENT_TIMEOUT
            }
            _ => DEFAULT_CONSENT_TIMEOUT,
        };

        // Move to AwaitingConsent and tell the agent. The agent always gets the
        // Request (it needs the codec / transport context) + the mode directive;
        // for Email/Push it will NOT respond — the owner resolves the slot.
        self.with_session(session_id, |s| s.transition(SessionPhase::AwaitingConsent))?;
        let agent_tx = self.agent_tx(agent_id)?;
        let _ = agent_tx.try_send(ServerMsg::Request {
            session_id,
            controller_user_id,
            controller_name: controller_name.clone(),
            permissions,
            consent_timeout_secs: timeout.as_secs() as u32,
            browser_caps,
            preferred_transport,
            chroma_pref,
            audio_enabled,
            // Server-authoritative directive: the agent obeys this rather than
            // its local `auto_grant_session`. `Auto` → immediate grant;
            // `Prompt` → on-host prompt; `Email`/`Push` → wait (owner resolves).
            consent_mode: Some(consent_mode),
        });

        self.audit(session_id, agent_id, agent_org, AuditKind::SessionRequested);
        self.audit(session_id, agent_id, agent_org, AuditKind::ConsentPrompted);

        // Phase 5 — record the break-glass BEFORE the session proceeds. The API
        // gate only sets `override_reason` for a validated `ADMINISTRATOR` force;
        // `consent_mode` was resolved to `Auto` alongside it, so consent is
        // skipped and this audit is the accountability trail for it.
        let override_for_notify = override_reason.clone();
        if let Some(reason) = override_reason {
            self.audit(
                session_id,
                agent_id,
                agent_org,
                AuditKind::AdminOverride { reason },
            );
        }

        // Phase 4/5 — owner-side notification: an Email/Push APPROVAL request, or
        // (when a break-glass just happened) an informational "your device was
        // accessed" NOTICE. Best-effort; the API consumer resolves the owner from
        // `agent_id`. With no consumer wired the session relies on the waiter.
        if (matches!(
            consent_mode,
            ConsentMode::Email | ConsentMode::Push | ConsentMode::PromptThenEmail
        ) || override_for_notify.is_some())
            && let Some(tx) = &self.inner.consent_tx
        {
            let _ = tx.try_send(ConsentEvent {
                session_id,
                agent_id,
                tenant_id: agent_org,
                controller_user_id,
                controller_name,
                mode: consent_mode,
                timeout_secs: timeout.as_secs() as u32,
                override_reason: override_for_notify,
            });
        }

        // Spawn the consent watcher.
        let hub = self.clone();
        tokio::spawn(async move {
            let outcome = waiter.wait(timeout).await;
            hub.handle_consent_outcome(session_id, outcome);
        });

        Ok(session_id)
    }

    fn handle_consent_outcome(&self, session_id: ObjectId, outcome: ConsentOutcome) {
        let (agent_id, tenant_id, controller_tx) = {
            let Some(arc) = self.inner.sessions.get(&session_id) else {
                return;
            };
            let s = arc.value().lock();
            (s.agent_id, s.tenant_id, s.controller_tx.clone())
        };

        match outcome {
            ConsentOutcome::Granted => {
                self.audit(session_id, agent_id, tenant_id, AuditKind::ConsentGranted);
                if let Err(e) =
                    self.with_session(session_id, |s| s.transition(SessionPhase::Negotiating))
                {
                    warn!("post-consent transition failed: {e}");
                    let _ = self.terminate(session_id, EndReason::Error);
                    return;
                }
                // Tell the controller it can send its offer.
                if let Some(tx) = controller_tx {
                    let user_id = self.controller_for(session_id).unwrap_or_default();
                    let ice = ice_servers_for(&user_id.to_hex(), self.inner.turn.as_ref());
                    let _ = tx.try_send(ServerMsg::Ready {
                        session_id,
                        ice_servers: ice,
                    });
                }
            }
            ConsentOutcome::Denied => {
                self.audit(session_id, agent_id, tenant_id, AuditKind::ConsentDenied);
                let _ = self.terminate(session_id, EndReason::UserDenied);
            }
            ConsentOutcome::Timeout => {
                self.audit(session_id, agent_id, tenant_id, AuditKind::ConsentTimedOut);
                let _ = self.terminate(session_id, EndReason::ConsentTimeout);
            }
        }
    }

    /// Caller: WS dispatcher when it sees `rc:consent` from agent.
    pub fn deliver_consent(&self, session_id: ObjectId, granted: bool) -> Result<()> {
        let arc = self
            .inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_hex()))?;
        let slot = {
            let mut s = arc.value().lock();
            s.consent_slot.take()
        };
        slot.ok_or_else(|| Error::BadMessage("consent already delivered"))?
            .resolve(granted)
    }

    // ─── SDP / ICE forwarding ────────────────────────────────────────

    /// Forward controller's SDP offer to the agent.
    pub fn forward_offer(&self, session_id: ObjectId, sdp: String) -> Result<()> {
        let agent_id = self.with_session(session_id, |s| Ok(s.agent_id))?;
        let user_id = self.controller_for(session_id).unwrap_or_default();
        let ice = ice_servers_for(&user_id.to_hex(), self.inner.turn.as_ref());
        let agent_tx = self.agent_tx(agent_id)?;
        agent_tx
            .try_send(ServerMsg::SdpOffer {
                session_id,
                sdp,
                ice_servers: ice,
            })
            .map_err(|_| Error::SendFailed)
    }

    /// Forward agent's SDP answer to the controller.
    pub fn forward_answer(&self, session_id: ObjectId, sdp: String) -> Result<()> {
        let (controller_tx, user_id) = {
            let arc = self
                .inner
                .sessions
                .get(&session_id)
                .ok_or_else(|| Error::SessionNotFound(session_id.to_hex()))?;
            let s = arc.value().lock();
            (s.controller_tx.clone(), s.controller_user_id)
        };
        let tx = controller_tx.ok_or(Error::SendFailed)?;
        let ice = ice_servers_for(&user_id.to_hex(), self.inner.turn.as_ref());
        tx.try_send(ServerMsg::SdpAnswer {
            session_id,
            sdp,
            ice_servers: ice,
        })
        .map_err(|_| Error::SendFailed)?;

        // Once the answer is in flight, mark the session active.
        // (The peers may still be doing ICE, but signaling is done from our POV.)
        self.with_session(session_id, |s| s.transition(SessionPhase::Active))?;
        let (sid, aid, oid) =
            self.with_session(session_id, |s| Ok((s.id, s.agent_id, s.tenant_id)))?;
        self.audit(sid, aid, oid, AuditKind::SessionStarted);
        Ok(())
    }

    /// Forward an ICE candidate to whichever side didn't send it.
    pub fn forward_ice(
        &self,
        role: Role,
        session_id: ObjectId,
        candidate: serde_json::Value,
    ) -> Result<()> {
        let (agent_id, controller_tx) = {
            let arc = self
                .inner
                .sessions
                .get(&session_id)
                .ok_or_else(|| Error::SessionNotFound(session_id.to_hex()))?;
            let s = arc.value().lock();
            (s.agent_id, s.controller_tx.clone())
        };
        let dest_tx = match role {
            Role::Controller => self.agent_tx(agent_id)?, // controller → agent
            Role::Agent => controller_tx.ok_or(Error::SendFailed)?,
        };
        dest_tx
            .try_send(ServerMsg::Ice {
                session_id,
                candidate,
            })
            .map_err(|_| Error::SendFailed)
    }

    // ─── termination ─────────────────────────────────────────────────

    pub fn terminate(&self, session_id: ObjectId, reason: EndReason) -> Result<()> {
        let Some((_, arc)) = self.inner.sessions.remove(&session_id) else {
            return Ok(()); // already gone, idempotent
        };

        let (agent_id, tenant_id, controller_tx) = {
            let mut s = arc.lock();
            // Best-effort transition; ignore if already closed.
            let _ = s.transition(SessionPhase::Closed);
            (s.agent_id, s.tenant_id, s.controller_tx.clone())
        };

        // Decrement agent session counter.
        if let Some(mut a) = self.inner.agents.get_mut(&agent_id) {
            a.active_sessions = a.active_sessions.saturating_sub(1);
        }

        // Notify both sides (best-effort; either may be gone).
        let msg = ServerMsg::Terminate { session_id, reason };
        if let Some(tx) = controller_tx {
            let _ = tx.try_send(msg.clone());
        }
        if let Ok(agent_tx) = self.agent_tx(agent_id) {
            let _ = agent_tx.try_send(msg);
        }

        self.audit(
            session_id,
            agent_id,
            tenant_id,
            AuditKind::SessionEnded { reason },
        );
        Ok(())
    }

    // ─── helpers ─────────────────────────────────────────────────────

    fn agent_tx(&self, agent_id: ObjectId) -> Result<ClientTx> {
        self.inner
            .agents
            .get(&agent_id)
            .map(|a| a.tx.clone())
            .ok_or_else(|| Error::AgentOffline(agent_id.to_hex()))
    }

    /// Push a `ServerMsg` straight to a connected agent. Returns
    /// `AgentOffline` if the agent isn't currently registered,
    /// `SendFailed` if the channel is full (rare — the agent rx pump
    /// reads as fast as it can serialise + write to the socket).
    ///
    /// Used by the `roomler-tunnel` relay path in
    /// `crates/api/src/ws/tunnel.rs` to forward
    /// `ServerMsg::TcpForwardForward` / `TcpHalfClose` /
    /// `TcpClosed` / `TunnelTerminate` to the agent on behalf of a
    /// connected tunnel-client. Distinct from the remote-control
    /// session flow which goes through `dispatch`.
    pub fn send_to_agent(&self, agent_id: ObjectId, msg: ServerMsg) -> Result<()> {
        let tx = self.agent_tx(agent_id)?;
        tx.try_send(msg).map_err(|e| match e {
            tokio::sync::mpsc::error::TrySendError::Full(_) => Error::SendFailed,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                Error::AgentOffline(agent_id.to_hex())
            }
        })
    }

    fn controller_for(&self, session_id: ObjectId) -> Option<ObjectId> {
        self.inner
            .sessions
            .get(&session_id)
            .map(|s| s.value().lock().controller_user_id)
    }

    fn with_session<F, R>(&self, session_id: ObjectId, f: F) -> Result<R>
    where
        F: FnOnce(&mut LiveSession) -> Result<R>,
    {
        let arc = self
            .inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_hex()))?;
        let mut s = arc.value().lock();
        f(&mut s)
    }

    fn audit(&self, sid: ObjectId, aid: ObjectId, oid: ObjectId, k: AuditKind) {
        self.inner.audit.record(sid, aid, oid, k);
    }

    // ─── introspection (for /api/sessions and admin) ─────────────────

    pub fn online_agents(&self) -> Vec<ObjectId> {
        self.inner.agents.iter().map(|e| *e.key()).collect()
    }

    pub fn is_agent_online(&self, agent_id: ObjectId) -> bool {
        self.inner.agents.contains_key(&agent_id)
    }

    pub fn live_sessions(&self) -> usize {
        self.inner.sessions.len()
    }
}

fn ptr_eq(a: &ClientTx, b: &ClientTx) -> bool {
    // mpsc::Sender doesn't expose pointer identity directly; use same_channel.
    a.same_channel(b)
}

// ────────────────────────────────────────────────────────────────────────────
// High-level dispatch — the WS handler funnels every parsed ClientMsg here.
// ────────────────────────────────────────────────────────────────────────────

pub struct DispatchCtx {
    pub role: Role,
    pub user_id: Option<ObjectId>,  // Some for Controller
    pub agent_id: Option<ObjectId>, // Some for Agent
    pub controller_name: Option<String>,
    pub controller_tx: Option<ClientTx>,
    /// Phase 2 — the server-resolved consent mode for a controller's
    /// `SessionRequest` (self-control → `Auto`; else the device's effective
    /// mode). The API WS layer computes it (it has the DB access the Hub lacks)
    /// and sets it here; `create_session` forwards it to the agent. Ignored for
    /// non-request messages and agent-role dispatch (defaults to `Prompt`).
    pub consent_mode: ConsentMode,
    /// Phase 5 — a VALIDATED admin break-glass reason (the API gate confirmed the
    /// controller is an `ADMINISTRATOR` forcing a device they don't own). `Some`
    /// ⇒ consent was skipped; `create_session` records an `AdminOverride` audit.
    /// The `SessionRequest` wire field is NOT trusted directly — only this.
    pub override_reason: Option<String>,
}

impl Hub {
    pub fn dispatch(&self, ctx: &DispatchCtx, msg: ClientMsg) -> Result<()> {
        match (ctx.role, msg) {
            (
                Role::Controller,
                ClientMsg::SessionRequest {
                    agent_id,
                    permissions,
                    browser_caps,
                    preferred_transport,
                    chroma_pref,
                    audio_enabled,
                    // Ignored here — the Hub can't validate admin; the API gate
                    // validates the wire field and re-supplies it via `ctx`.
                    override_reason: _,
                },
            ) => {
                // Forward browser codec caps verbatim to the agent in
                // the ServerMsg::Request envelope. The agent picks the
                // best intersection with its own AgentCaps and uses it
                // to choose the encoder + advertise the codec in SDP.
                // `preferred_transport` (Phase Y.3) is forwarded the
                // same way; the agent matches it against its own
                // AgentCaps.transports and decides whether to honour
                // or fall back to the WebRTC video track.
                let user_id = ctx.user_id.ok_or(Error::PermissionDenied("no user"))?;
                let name = ctx.controller_name.clone().unwrap_or_default();
                let tx = ctx.controller_tx.clone().ok_or(Error::SendFailed)?;
                self.create_session(
                    agent_id,
                    user_id,
                    name,
                    tx,
                    permissions,
                    browser_caps,
                    preferred_transport,
                    chroma_pref,
                    audio_enabled,
                    ctx.consent_mode,
                    ctx.override_reason.clone(),
                )?;
                Ok(())
            }
            (Role::Controller, ClientMsg::SdpOffer { session_id, sdp }) => {
                self.forward_offer(session_id, sdp)
            }
            (Role::Agent, ClientMsg::SdpAnswer { session_id, sdp }) => {
                self.forward_answer(session_id, sdp)
            }
            (
                Role::Agent,
                ClientMsg::Consent {
                    session_id,
                    granted,
                },
            ) => self.deliver_consent(session_id, granted),
            (
                role,
                ClientMsg::Ice {
                    session_id,
                    candidate,
                },
            ) => self.forward_ice(role, session_id, candidate),
            (_, ClientMsg::Terminate { session_id, reason }) => self.terminate(session_id, reason),
            (_, ClientMsg::Ping { id: _ }) => Ok(()), // pong handled by WS layer
            (_, ClientMsg::AgentHello { .. } | ClientMsg::AgentHeartbeat { .. }) => {
                // Hello is handled at registration time; heartbeat is logged by WS layer.
                Ok(())
            }
            (role, msg) => {
                warn!("unexpected msg for role {role:?}: {:?}", msg);
                Err(Error::BadMessage("wrong role for message"))
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditSink;
    use mongodb::Client;
    use std::time::Duration;

    async fn test_hub() -> Hub {
        // Use an in-memory-ish setup: Mongo isn't actually contacted unless we
        // record audit; the channel buffers it and we never let the task tick.
        // For a real CI test that exercises Mongo, see the integration tests.
        let client = Client::with_uri_str("mongodb://localhost:27017")
            .await
            .expect("mongo for tests");
        let db = client.database("rc_test");
        let (audit, _h) = AuditSink::spawn(db);
        Hub::new(audit, None)
    }

    #[tokio::test]
    async fn rejects_session_for_offline_agent() {
        let hub = test_hub().await;
        let (tx, _rx) = mpsc::channel(8);
        let res = hub.create_session(
            ObjectId::new(),
            ObjectId::new(),
            "Goran".into(),
            tx,
            Permissions::default(),
            Vec::new(),
            None,
            None,  // chroma_pref
            false, // audio_enabled
            ConsentMode::Prompt,
            None, // override_reason
        );
        assert!(matches!(res, Err(Error::AgentOffline(_))));
    }

    #[tokio::test]
    async fn end_to_end_consent_grant() {
        let hub = test_hub().await;
        let agent_id = ObjectId::new();
        let (_agent_tx, _cancel, _agent_rx) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let sid = hub
            .create_session(
                agent_id,
                ObjectId::new(),
                "Goran".into(),
                ctl_tx,
                Permissions::default(),
                Vec::new(),
                None,
                None,  // chroma_pref
                false, // audio_enabled
                ConsentMode::Prompt,
                None, // override_reason
            )
            .unwrap();

        // Controller should immediately receive SessionCreated.
        let m = ctl_rx.try_recv().unwrap();
        assert!(matches!(m, ServerMsg::SessionCreated { .. }));

        // Deliver consent.
        hub.deliver_consent(sid, true).unwrap();

        // Give the consent task a tick to fire.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Controller should now have a Ready message.
        let m = ctl_rx.try_recv().unwrap();
        assert!(matches!(m, ServerMsg::Ready { .. }));
    }

    // ─── rc.53 Phase 2b: Hub displacement notify-then-close ──────────

    #[tokio::test]
    async fn displacement_sends_goodbye_then_notifies_cancel() {
        // The headline rc.53 fix: when a second connection arrives
        // for the same agent_id, the FIRST connection's pump rx must
        // receive a `ServerMsg::Goodbye{ReplacedByNewerConnection}`
        // AND its cancel notify must fire so the read-loop exits
        // within milliseconds (not 25 s waiting on keepalive).
        let hub = test_hub().await;
        let agent_id = ObjectId::new();

        let (_tx1, cancel1, mut rx1) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);
        let (_tx2, _cancel2, _rx2) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);

        // First connection's pump rx should immediately have the
        // Goodbye queued; the channel hasn't been polled yet so
        // try_recv finds the message.
        let msg = rx1
            .try_recv()
            .expect("displaced connection should receive Goodbye");
        match msg {
            ServerMsg::Goodbye { reason, message } => {
                assert_eq!(reason, AgentCloseReason::ReplacedByNewerConnection);
                assert!(
                    message.contains(&agent_id.to_hex()),
                    "Goodbye message should mention the agent_id: {message}"
                );
            }
            other => panic!("expected Goodbye on displaced rx, got {other:?}"),
        }

        // cancel1 must have a stored notify-permit waiting. We use
        // `notify_one` (not `notify_waiters`) in register_agent
        // precisely so that the displacement signal latches even if
        // the displaced handler hasn't entered its select-loop yet.
        // `.notified()` here consumes that permit and resolves
        // immediately. A 50 ms timeout is generous for any CI
        // scheduler jitter — production wakes in microseconds.
        tokio::time::timeout(Duration::from_millis(50), cancel1.notified())
            .await
            .expect(
                "displacement must store a notify permit so the displaced read-loop \
                 exits within milliseconds (rc.53 auto-fail #5 mitigation)",
            );
    }

    #[tokio::test]
    async fn unregister_agent_with_stale_tx_is_noop() {
        // rc.53 race fix: the displaced `handle_agent_socket`
        // eventually exits its read-loop and calls `unregister_agent`.
        // By then the registry entry is the NEW connection's. The
        // pre-rc.53 code unconditionally removed by agent_id, which
        // evicted the NEW entry and killed its sessions. Phase 2b's
        // `Option<&ClientTx>` gates the removal on identity match.
        let hub = test_hub().await;
        let agent_id = ObjectId::new();

        let (tx1, _cancel1, _rx1) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);
        let (_tx2, _cancel2, _rx2) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);

        // tx1 is now stale (tx2 holds the slot). Stale-tx unregister
        // must NOT evict tx2's entry.
        hub.unregister_agent(agent_id, Some(&tx1));
        assert!(
            hub.is_agent_online(agent_id),
            "stale-tx unregister evicted the NEW connection — rc.53 race regression"
        );
    }

    #[tokio::test]
    async fn unregister_agent_with_none_tx_always_removes() {
        // Admin-kick path: `routes/remote_control.rs::kick_agent`
        // calls `unregister_agent(aid, None)` — no tx identity is
        // available there, so the call must always remove
        // unconditionally (otherwise admins lose their kick power).
        let hub = test_hub().await;
        let agent_id = ObjectId::new();
        let (_tx, _cancel, _rx) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);
        assert!(hub.is_agent_online(agent_id));

        hub.unregister_agent(agent_id, None);
        assert!(
            !hub.is_agent_online(agent_id),
            "None-tx unregister must always remove (admin-kick path)"
        );
    }

    #[tokio::test]
    async fn unregister_agent_with_matching_tx_removes() {
        // Sanity: when the tx still matches, removal proceeds normally.
        let hub = test_hub().await;
        let agent_id = ObjectId::new();
        let (tx, _cancel, _rx) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);
        assert!(hub.is_agent_online(agent_id));

        hub.unregister_agent(agent_id, Some(&tx));
        assert!(
            !hub.is_agent_online(agent_id),
            "matching-tx unregister must remove the entry"
        );
    }
}
