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
use crate::signaling::{
    AgentCloseReason, ClientMsg, IceServer, LocalRelayDescriptor, Role, ServerMsg,
};
use crate::turn_creds::{
    RelayLoadMap, TurnConfig, TurnMap, ice_servers_for_session, pick_region_with_load,
};

const SERVER_TX_CAPACITY: usize = 64;

/// Consent window for owner-side channels (Email/Push) — the owner has to read a
/// mail / tap a push, so it's far longer than the 30 s on-host prompt
/// ([`DEFAULT_CONSENT_TIMEOUT`]). Also bounds the `ConsentRequest` link TTL.
const ASYNC_CONSENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Loopback-TURN corp-relay (Phase 2) guard. The overlay assigns node IPs from
/// the RFC 6598 CGNAT range 100.64.0.0/10 (v4) or a ULA `fc00::/7` (v6, derived
/// from the v4). The Hub only appends a controller-supplied `local_relay` TURN
/// entry to the REMOTE agent's ICE servers when its address is inside that
/// range — so a malicious controller can't coerce the agent's TURN client into
/// probing an arbitrary host. (Media is DTLS-E2E regardless, but keeping the
/// agent pointed only at overlay IPs bounds the blast radius to its own mesh.)
fn is_overlay_relay_ip(ip: &str) -> bool {
    match ip.parse::<std::net::IpAddr>() {
        // 100.64.0.0/10 == 100.64.0.0 – 100.127.255.255
        Ok(std::net::IpAddr::V4(v4)) => {
            let o = v4.octets();
            o[0] == 100 && (64..=127).contains(&o[1])
        }
        // fc00::/7 — the overlay's ULA space (today the agent hands out its v4
        // overlay IP; this keeps a future v6 relay from being rejected).
        Ok(std::net::IpAddr::V6(v6)) => (v6.segments()[0] & 0xfe00) == 0xfc00,
        Err(_) => false,
    }
}

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
    /// The agent's nearest relay region (`relay_regions` id), used to pick the
    /// per-session TURN config. Seeded from the DB row at registration and
    /// refreshed live when a probe report lands ([`Hub::set_agent_relay_home`]);
    /// `None` = the legacy default region.
    pub relay_home: Option<String>,
    /// The agent's regions ordered by measured RTT (nearest first) — the
    /// load-aware fallback ladder when [`relay_home`](Self::relay_home) is
    /// busy. Empty until a probe report lands.
    pub region_prefs: Vec<String>,
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

    /// TURN issuance — region-keyed; a disabled/regionless map behaves exactly
    /// like the old `Option<TurnConfig>` (its `default`).
    turn: TurnMap,

    /// Live per-region load written by the API's `/stats` poller; consulted
    /// when FREEZING a session's region at create time. Empty (tests / poller
    /// off) = nothing is ever busy.
    relay_load: RelayLoadMap,

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
        Self::new_with_consent(audit, TurnMap::single(turn), None, Default::default())
    }

    /// Like [`Hub::new`] but wires the Phase-4 async-consent event sender,
    /// the full region-keyed [`TurnMap`], and the live region-load table.
    pub fn new_with_consent(
        audit: AuditSink,
        turn: TurnMap,
        consent_tx: Option<mpsc::Sender<ConsentEvent>>,
        relay_load: RelayLoadMap,
    ) -> Self {
        Self {
            inner: Arc::new(HubInner {
                agents: DashMap::new(),
                sessions: DashMap::new(),
                controllers: DashMap::new(),
                turn,
                relay_load,
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
            relay_home: None,
            region_prefs: Vec::new(),
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
    ///
    /// Returns whether the removal was OURS (Phase A-1): callers gate
    /// the Mongo `mark_status(Offline)` + Redis presence delete on it,
    /// because a displaced handler's late teardown writing Offline over
    /// the displacing connection's Online was a live status-clobber
    /// race (the same race rc.53 fixed for the registry entry itself).
    pub fn unregister_agent(&self, agent_id: ObjectId, tx: Option<&ClientTx>) -> bool {
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
                return false;
            }
        }
        let removed = self.inner.agents.remove(&agent_id).is_some();
        if removed {
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
        removed
    }

    /// C-2 — the idle-agent nudge: close an agent's WS (via its
    /// displacement-cancel notify — the read loop exits within
    /// milliseconds and runs the normal teardown) so its reconnect
    /// re-hashes through the LB onto the pod the current map points at.
    /// STRICTLY idle-only: an active rc session or (per `extra_busy`,
    /// the caller's tunnel-session check) a live tunnel flow means the
    /// truthful answer to the requester is "busy", not a forced move.
    /// Returns whether the nudge fired.
    pub fn nudge_agent_if_idle(&self, agent_id: ObjectId, extra_busy: bool) -> bool {
        if extra_busy {
            return false;
        }
        let Some(agent) = self.inner.agents.get(&agent_id) else {
            return false;
        };
        if agent.active_sessions > 0 {
            return false;
        }
        info!(
            "agent {} nudged (idle) — cycling its WS for LB re-hash",
            agent_id
        );
        agent.cancel.notify_waiters();
        true
    }

    /// Phase A-1 graceful shutdown: fire every registered agent's
    /// displacement-cancel notify so each read loop exits within
    /// milliseconds and runs its OWN teardown (unregister, gated
    /// mark-offline, presence release) — the per-socket teardown is the
    /// single source of cleanup truth, so shutdown reuses it instead of
    /// duplicating it. Returns the notified agent ids for the caller's
    /// belt-and-braces bulk cleanup.
    pub fn cancel_all_agents(&self) -> Vec<ObjectId> {
        let ids: Vec<ObjectId> = self.inner.agents.iter().map(|a| *a.key()).collect();
        for entry in self.inner.agents.iter() {
            entry.value().cancel.notify_waiters();
        }
        ids
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

        // Terminate any sessions THIS CONNECTION still owns. Without this
        // the agent's active_sessions counter never drops when a browser
        // tab closes mid-session, and subsequent Connect attempts fail
        // with AgentBusy until the agent itself disconnects.
        //
        // P3 — keyed by the departing TX, not the user_id: this was
        // user-keyed and killed EVERY session of the user on ANY one tab
        // close — tab A's org-switch redial (#307 `setTenantAffinity`
        // closes the WS) tore down tab B's live session, and with
        // N-session support one closing watcher tab would have killed the
        // user's other sessions. A session mid-setup (tx not yet
        // registered) keeps its live channel, so it is never a false
        // positive; a session whose channel is already CLOSED is reaped
        // regardless of which tab it belonged to (the rc.185 orphan sweep
        // in `create_session` remains the belt for races).
        let orphaned: Vec<ObjectId> = self
            .inner
            .sessions
            .iter()
            .filter(|e| {
                let s = e.value().lock();
                s.controller_user_id == user_id
                    && s.controller_tx
                        .as_ref()
                        .map(|t| ptr_eq(t, tx) || t.is_closed())
                        .unwrap_or(false)
            })
            .map(|e| *e.key())
            .collect();
        for session_id in orphaned {
            let _ = self.terminate(session_id, EndReason::ControllerHangup);
        }
    }

    /// P3 — a live session's `(tenant_id, controller_user_id)`, for
    /// route-side authz + cross-pod ctrl tenant checks. `None` when this
    /// pod's hub doesn't hold the session (pod-local under S6).
    pub fn session_snapshot(&self, session_id: ObjectId) -> Option<(ObjectId, ObjectId)> {
        let arc = self.inner.sessions.get(&session_id)?;
        let s = arc.value().lock();
        Some((s.tenant_id, s.controller_user_id))
    }

    /// Refresh the cached relay home (+ RTT-ordered fallback ladder) of a
    /// connected agent — called by the WS layer at hello (from the DB row)
    /// and whenever a probe report changes the agent's picture. A
    /// disconnected agent is a no-op.
    pub fn set_agent_relay_home(
        &self,
        agent_id: ObjectId,
        home: Option<String>,
        prefs: Vec<String>,
    ) {
        if let Some(mut a) = self.inner.agents.get_mut(&agent_id) {
            a.relay_home = home;
            a.region_prefs = prefs;
        }
    }

    /// The connected agent's relay home, if any. Pub for the WS layer's
    /// probe-report hysteresis (compare-before-move).
    pub fn agent_relay_home(&self, agent_id: ObjectId) -> Option<String> {
        self.inner
            .agents
            .get(&agent_id)
            .and_then(|a| a.relay_home.clone())
    }

    /// The region frozen on a live session at create time (`None` = default
    /// region, or the session is gone).
    fn session_relay_region(&self, session_id: ObjectId) -> Option<String> {
        self.inner
            .sessions
            .get(&session_id)
            .and_then(|arc| arc.value().lock().relay_region.clone())
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
        local_relay: Option<LocalRelayDescriptor>,
    ) -> Result<ObjectId> {
        // rc.185 — self-heal the fast connect→disconnect race. A controller
        // that connects then drops before teardown completes can leave an
        // ORPHANED session pinned to this agent: `active_sessions` stays
        // elevated (the next reconnect hits `AgentBusy`) AND the agent's
        // "being viewed" red-frame indicator never clears (the
        // controller-disconnect cleanup missed the not-yet-registered
        // session, or the best-effort Terminate raced the socket close, or
        // the peer sits ~30 s in ICE-`Disconnected` before it reaches
        // `Failed` and self-terminates). Before the capacity gate, reap any
        // of THIS agent's sessions whose controller channel is already
        // closed. `terminate()` frees the counter AND re-sends
        // `ServerMsg::Terminate` to the agent, which clears the stale
        // indicator + peer. Mirrors the iterate-and-lock pattern in
        // `unregister_controller`; a mid-setup session keeps a live
        // controller_tx so it is never a false positive.
        let orphaned: Vec<ObjectId> = self
            .inner
            .sessions
            .iter()
            .filter_map(|e| {
                let dead_controller = {
                    let s = e.value().lock();
                    s.agent_id == agent_id
                        && s.controller_tx
                            .as_ref()
                            .map(|tx| tx.is_closed())
                            .unwrap_or(true)
                };
                dead_controller.then(|| *e.key())
            })
            .collect();
        for sid in orphaned {
            let _ = self.terminate(sid, EndReason::ControllerHangup);
        }

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

        // Multi-user P3 back-compat — the FILES bit becomes agent-ENFORCED in
        // this release, but every pre-P3 viewer requests the legacy triple
        // (VIEW|INPUT|CLIPBOARD, also `Permissions::default()`) and file
        // transfer has ALWAYS worked under it. Grandfather exactly that grant
        // to include FILES so a cached pre-P3 tab doesn't lose its file
        // toolbar against a P3 agent; new viewers request FILES explicitly,
        // and any OTHER combination (a genuine view-only watcher, a narrowed
        // grant) is taken literally. Remove once pre-P3 viewers age out.
        let permissions = if permissions == Permissions::default() {
            permissions | Permissions::FILES
        } else {
            permissions
        };

        // Multi-user P3 — ONE INPUT holder per agent. With N concurrent
        // sessions now reachable (rc_max_sessions ≥ 2) but no input
        // arbitration yet, two typing users would merge on the host's single
        // modifier plane (the agent injects real HID presses and the OS
        // combines them: A holding Shift turns B's clicks into chords) and
        // fight over the process-global keyboard layout. Until the P6
        // arbitration modes (free-for-all + per-session modifier fencing /
        // explicit handoff) land, a session requested while another LIVE
        // session on this agent already holds INPUT is downgraded to
        // view-effective (INPUT stripped; VIEW/CLIPBOARD/FILES kept). The
        // effective grant travels in `SessionCreated.permissions` (viewer)
        // and `Request.permissions` (agent, which enforces it on the input
        // DC), and INPUT is RE-offered naturally when the holder's session
        // ends and a new session is created.
        let permissions = if permissions.contains(Permissions::INPUT)
            && self.inner.sessions.iter().any(|e| {
                let s = e.value().lock();
                s.agent_id == agent_id && s.permissions.contains(Permissions::INPUT)
            }) {
            info!(
                "agent {} already has an INPUT-holding session — new session is view-effective \
                 (P3 single-holder rule; arbitration modes land with P6)",
                agent_id.to_hex()
            );
            permissions - Permissions::INPUT
        } else {
            permissions
        };

        let session_id = ObjectId::new();
        let (mut live, waiter) = LiveSession::new(
            session_id,
            agent_id,
            agent_org,
            controller_user_id,
            permissions,
            controller_tx.clone(),
        );
        // Loopback-TURN corp-relay (Phase 2): remember the controller's local
        // agent TURN descriptor so `forward_offer` can hand the REMOTE agent a
        // relay it reaches over the overlay. Validated at use, not here.
        live.local_relay = local_relay;
        // Multi-region relay: freeze the session's region NOW — a load-aware
        // pick over the agent's home + its RTT-ordered prefs — so Ready/
        // Offer/Answer all issue from the same PoP regardless of mid-setup
        // home or busy-flag churn.
        live.relay_region = self
            .inner
            .agents
            .get(&agent_id)
            .map(|a| (a.relay_home.clone(), a.region_prefs.clone()))
            .and_then(|(home, prefs)| {
                pick_region_with_load(
                    &self.inner.turn,
                    &self.inner.relay_load,
                    home.as_deref(),
                    &prefs,
                )
            });
        self.inner
            .sessions
            .insert(session_id, Arc::new(Mutex::new(live)));

        // Tell the controller the session id + its EFFECTIVE grant (which the
        // single-INPUT-holder rule above may have narrowed).
        let _ = controller_tx.try_send(ServerMsg::SessionCreated {
            session_id,
            agent_id,
            permissions: Some(permissions),
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

        self.audit(
            session_id,
            agent_id,
            agent_org,
            AuditKind::SessionRequested {
                controller_user_id,
                controller_name: controller_name.clone(),
                permissions,
            },
        );
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
                    // Region policy: the session's region was FROZEN (load-
                    // aware, agent-nearest) at create — all three issuance
                    // pushes read it so both ICE ends land on one PoP.
                    let region = self.session_relay_region(session_id);
                    let ice = ice_servers_for_session(
                        &user_id.to_hex(),
                        &session_id.to_hex(),
                        self.inner.turn.cfg_for(region.as_deref()),
                    );
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
        let (agent_id, local_relay, region) = self.with_session(session_id, |s| {
            Ok((s.agent_id, s.local_relay.clone(), s.relay_region.clone()))
        })?;
        let user_id = self.controller_for(session_id).unwrap_or_default();
        let mut ice = ice_servers_for_session(
            &user_id.to_hex(),
            &session_id.to_hex(),
            self.inner.turn.cfg_for(region.as_deref()),
        );
        // Loopback-TURN corp-relay (Phase 2): if the controller forwarded its
        // local agent's TURN descriptor, hand the REMOTE agent a relay it can
        // reach over the overlay (WFP-permitted, corp-traversal proven). This is
        // the SdpOffer→agent push — the one place the remote agent learns its
        // ICE servers — so it's where the overlay relay must be added. The IP is
        // validated inside the CGNAT overlay range so a controller can't coerce
        // the agent's TURN client into probing an arbitrary host.
        if let Some(lr) = local_relay {
            if is_overlay_relay_ip(&lr.overlay_ip) && lr.turn_port != 0 {
                info!(
                    session = %session_id.to_hex(),
                    relay = %format!("turn:{}:{}", lr.overlay_ip, lr.turn_port),
                    "loopback-TURN: adding local-agent overlay relay to agent ICE"
                );
                ice.push(IceServer {
                    urls: vec![format!("turn:{}:{}", lr.overlay_ip, lr.turn_port)],
                    username: Some(lr.username),
                    credential: Some(lr.credential),
                });
            } else {
                warn!(
                    session = %session_id.to_hex(),
                    overlay_ip = %lr.overlay_ip,
                    turn_port = lr.turn_port,
                    "loopback-TURN: rejecting local_relay (not an overlay address)"
                );
            }
        }
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
        let (controller_tx, user_id, region) = {
            let arc = self
                .inner
                .sessions
                .get(&session_id)
                .ok_or_else(|| Error::SessionNotFound(session_id.to_hex()))?;
            let s = arc.value().lock();
            (
                s.controller_tx.clone(),
                s.controller_user_id,
                s.relay_region.clone(),
            )
        };
        let tx = controller_tx.ok_or(Error::SendFailed)?;
        let ice = ice_servers_for_session(
            &user_id.to_hex(),
            &session_id.to_hex(),
            self.inner.turn.cfg_for(region.as_deref()),
        );
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
    /// Multi-user P3 security — is the dispatching CONNECTION a party to the
    /// session (the controller that owns it, or the agent it targets)?
    ///
    /// Session ids are not secrets: every tenant member sees them in the
    /// sessions listing/audit, and with N-session support watchers hold them
    /// legitimately. Pre-P3, `dispatch` matched `Terminate` / `SdpOffer` /
    /// `Ice` on session_id alone — any authenticated user who learned an id
    /// could kill the session or inject SDP/ICE into its signalling.
    /// Server-side paths with their own authz (the HTTP terminate route,
    /// orphan reaps, teardown sweeps) call the underlying methods directly
    /// and are unaffected.
    fn check_session_party(&self, ctx: &DispatchCtx, session_id: ObjectId) -> Result<()> {
        let arc = self
            .inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_hex()))?;
        let s = arc.value().lock();
        let is_party = match ctx.role {
            Role::Controller => ctx.user_id == Some(s.controller_user_id),
            Role::Agent => ctx.agent_id == Some(s.agent_id),
        };
        drop(s);
        if is_party {
            Ok(())
        } else {
            warn!(
                "rc dispatch refused: {:?} connection is not a party to session {}",
                ctx.role,
                session_id.to_hex()
            );
            Err(Error::PermissionDenied("not a party to this session"))
        }
    }

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
                    // Loopback-TURN corp-relay (Phase 2): the descriptor is a
                    // self-minted local-agent TURN; safe to pass through (the
                    // media is DTLS-E2E), overlay-IP-validated in forward_offer.
                    local_relay,
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
                    local_relay,
                )?;
                Ok(())
            }
            (Role::Controller, ClientMsg::SdpOffer { session_id, sdp }) => {
                self.check_session_party(ctx, session_id)?;
                self.forward_offer(session_id, sdp)
            }
            (Role::Agent, ClientMsg::SdpAnswer { session_id, sdp }) => {
                self.check_session_party(ctx, session_id)?;
                self.forward_answer(session_id, sdp)
            }
            (
                Role::Agent,
                ClientMsg::Consent {
                    session_id,
                    granted,
                },
            ) => {
                self.check_session_party(ctx, session_id)?;
                self.deliver_consent(session_id, granted)
            }
            (
                role,
                ClientMsg::Ice {
                    session_id,
                    candidate,
                },
            ) => {
                self.check_session_party(ctx, session_id)?;
                self.forward_ice(role, session_id, candidate)
            }
            (_, ClientMsg::Terminate { session_id, reason }) => {
                // Terminate stays IDEMPOTENT on an already-gone session (the
                // agent's overlay-badge disconnect and teardown races rely on
                // it) — only a live session enforces the party check.
                match self.check_session_party(ctx, session_id) {
                    Ok(()) => self.terminate(session_id, reason),
                    Err(Error::SessionNotFound(_)) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            (_, ClientMsg::Ping { id: _ }) => Ok(()), // pong handled by WS layer
            (
                _,
                ClientMsg::AgentHello { .. }
                | ClientMsg::AgentHeartbeat { .. }
                | ClientMsg::RelayProbeReport { .. }
                | ClientMsg::DerpTicketRequest {},
            ) => {
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
            None, // local_relay
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
                None, // local_relay
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

    /// P3 security — `dispatch` refuses session verbs from a connection
    /// that is not a party to the session (any authenticated user who
    /// learned a session id could previously Terminate it or inject
    /// SDP/ICE), while the owning controller and the session's agent pass,
    /// and Terminate stays idempotent for an already-gone session.
    #[tokio::test]
    async fn dispatch_enforces_session_party() {
        let hub = test_hub().await;
        let agent_id = ObjectId::new();
        let (_agent_tx, _cancel, _agent_rx) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);
        let owner = ObjectId::new();
        let (ctl_tx, _ctl_rx) = mpsc::channel(8);
        let sid = hub
            .create_session(
                agent_id,
                owner,
                "Owner".into(),
                ctl_tx.clone(),
                Permissions::default(),
                Vec::new(),
                None,
                None,
                false,
                ConsentMode::Prompt,
                None,
                None,
            )
            .unwrap();

        let ctx_for = |role: Role, user: Option<ObjectId>, agent: Option<ObjectId>| DispatchCtx {
            role,
            user_id: user,
            agent_id: agent,
            controller_name: None,
            controller_tx: Some(ctl_tx.clone()),
            consent_mode: ConsentMode::Prompt,
            override_reason: None,
        };
        let stranger = ctx_for(Role::Controller, Some(ObjectId::new()), None);
        let owner_ctx = ctx_for(Role::Controller, Some(owner), None);
        let session_agent = ctx_for(Role::Agent, None, Some(agent_id));
        let other_agent = ctx_for(Role::Agent, None, Some(ObjectId::new()));

        // A stranger cannot terminate, offer, or trickle ICE.
        for msg in [
            ClientMsg::Terminate {
                session_id: sid,
                reason: EndReason::ControllerHangup,
            },
            ClientMsg::SdpOffer {
                session_id: sid,
                sdp: "v=0".into(),
            },
            ClientMsg::Ice {
                session_id: sid,
                candidate: serde_json::json!({}),
            },
        ] {
            let res = hub.dispatch(&stranger, msg);
            assert!(
                matches!(res, Err(Error::PermissionDenied(_))),
                "stranger must be refused, got {res:?}"
            );
        }
        // A DIFFERENT agent cannot answer/consent/ICE into the session.
        for msg in [
            ClientMsg::SdpAnswer {
                session_id: sid,
                sdp: "v=0".into(),
            },
            ClientMsg::Consent {
                session_id: sid,
                granted: true,
            },
            ClientMsg::Ice {
                session_id: sid,
                candidate: serde_json::json!({}),
            },
        ] {
            let res = hub.dispatch(&other_agent, msg);
            assert!(
                matches!(res, Err(Error::PermissionDenied(_))),
                "foreign agent must be refused, got {res:?}"
            );
        }
        assert!(
            hub.session_snapshot(sid).is_some(),
            "session must survive every refused attempt"
        );

        // The session's OWN agent passes the check (consent resolves).
        hub.dispatch(
            &session_agent,
            ClientMsg::Consent {
                session_id: sid,
                granted: true,
            },
        )
        .unwrap();

        // The owning controller may terminate; a repeat (session gone) is
        // idempotent Ok, and a stranger's attempt on a MISSING session is
        // also Ok (nothing to protect, matches pre-P3 idempotency).
        hub.dispatch(
            &owner_ctx,
            ClientMsg::Terminate {
                session_id: sid,
                reason: EndReason::ControllerHangup,
            },
        )
        .unwrap();
        hub.dispatch(
            &owner_ctx,
            ClientMsg::Terminate {
                session_id: sid,
                reason: EndReason::ControllerHangup,
            },
        )
        .unwrap();
        hub.dispatch(
            &stranger,
            ClientMsg::Terminate {
                session_id: sid,
                reason: EndReason::ControllerHangup,
            },
        )
        .unwrap();
    }

    /// Multi-user P3 — ONE INPUT holder per agent: a second concurrent
    /// session is created view-effective (INPUT stripped, everything else
    /// kept), the effective grant rides `SessionCreated.permissions`, and
    /// INPUT becomes grantable again once the holder's session ends.
    #[tokio::test]
    async fn second_concurrent_session_is_input_stripped_until_holder_ends() {
        let hub = test_hub().await;
        let agent_id = ObjectId::new();
        let (_agent_tx, _cancel, _agent_rx) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);

        let mk = |hub: &Hub, name: &str, tx: ClientTx| {
            hub.create_session(
                agent_id,
                ObjectId::new(),
                name.into(),
                tx,
                Permissions::default(), // VIEW | INPUT | CLIPBOARD
                Vec::new(),
                None,
                None,
                false,
                ConsentMode::Prompt,
                None,
                None,
            )
        };
        let effective = |rx: &mut mpsc::Receiver<ServerMsg>| match rx.try_recv().unwrap() {
            ServerMsg::SessionCreated { permissions, .. } => {
                permissions.expect("P3 server always reports the effective grant")
            }
            other => panic!("expected SessionCreated, got {other:?}"),
        };

        let (tx_a, mut rx_a) = mpsc::channel(8);
        let sid_a = mk(&hub, "holder", tx_a).unwrap();
        let perms_a = effective(&mut rx_a);
        assert!(
            perms_a.contains(Permissions::INPUT),
            "first session holds INPUT"
        );

        let (tx_b, mut rx_b) = mpsc::channel(8);
        let sid_b = mk(&hub, "watcher", tx_b).unwrap();
        let perms_b = effective(&mut rx_b);
        assert!(
            !perms_b.contains(Permissions::INPUT),
            "second concurrent session must be view-effective, got {perms_b:?}"
        );
        assert!(perms_b.contains(Permissions::VIEW), "everything else kept");
        assert!(perms_b.contains(Permissions::CLIPBOARD));
        assert_ne!(sid_a, sid_b);

        // Holder ends → the next session gets INPUT again (B keeps its
        // narrowed grant — re-offer is by NEW session, not retro-upgrade).
        hub.terminate(sid_a, EndReason::ControllerHangup).unwrap();
        let (tx_c, mut rx_c) = mpsc::channel(8);
        let _sid_c = mk(&hub, "next", tx_c).unwrap();
        let perms_c = effective(&mut rx_c);
        assert!(
            perms_c.contains(Permissions::INPUT),
            "INPUT re-offered once the holder ended, got {perms_c:?}"
        );
    }

    /// P3 — `unregister_controller` is per-CONNECTION: closing tab A must
    /// not tear down the same user's live session on tab B (pre-P3 it was
    /// user-keyed and killed every session — #307's org-switch redial in
    /// one tab killed the other tab's live session).
    #[tokio::test]
    async fn unregister_controller_kills_only_that_tabs_sessions() {
        let hub = test_hub().await;
        let agent_id = ObjectId::new();
        let (_agent_tx, _cancel, _agent_rx) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);
        let user = ObjectId::new();

        let (tx_a, _rx_a) = hub.register_controller(user);
        let (tx_b, _rx_b) = hub.register_controller(user);
        let sid_a = hub
            .create_session(
                agent_id,
                user,
                "tab A".into(),
                tx_a.clone(),
                Permissions::default(),
                Vec::new(),
                None,
                None,
                false,
                ConsentMode::Prompt,
                None,
                None,
            )
            .unwrap();
        let sid_b = hub
            .create_session(
                agent_id,
                user,
                "tab B".into(),
                tx_b.clone(),
                Permissions::default(),
                Vec::new(),
                None,
                None,
                false,
                ConsentMode::Prompt,
                None,
                None,
            )
            .unwrap();

        // Tab A closes: only A's session goes.
        hub.unregister_controller(user, &tx_a);
        assert!(
            hub.session_snapshot(sid_a).is_none(),
            "tab A's session must be reaped"
        );
        assert!(
            hub.session_snapshot(sid_b).is_some(),
            "tab B's live session must SURVIVE tab A's close"
        );

        // Tab B closes: its session goes too.
        hub.unregister_controller(user, &tx_b);
        assert!(hub.session_snapshot(sid_b).is_none());
    }

    // rc.185 — the fast connect→disconnect reconnect fix. An orphaned
    // session (controller channel closed) must NOT permanently pin
    // `active_sessions`; the next create_session reaps it and succeeds.
    #[tokio::test]
    async fn create_session_reaps_orphan_from_dead_controller() {
        let hub = test_hub().await;
        let agent_id = ObjectId::new();
        // max_sessions = 1 so a single orphan blocks reconnect pre-fix.
        let (_agent_tx, _cancel, mut agent_rx) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 1);

        // Controller A connects, then "disconnects" (drop its receiver) —
        // the fast connect→disconnect that used to leave a stuck session.
        let (ctl_tx_a, ctl_rx_a) = mpsc::channel(8);
        let sid_a = hub
            .create_session(
                agent_id,
                ObjectId::new(),
                "A".into(),
                ctl_tx_a,
                Permissions::default(),
                Vec::new(),
                None,
                None,
                false,
                ConsentMode::Prompt,
                None,
                None, // local_relay
            )
            .unwrap();
        drop(ctl_rx_a); // controller gone → the session's controller_tx.is_closed()

        // Controller B reconnects. Pre-fix this returned AgentBusy because
        // A's orphan still held the only slot. The reap must free it.
        let (ctl_tx_b, _ctl_rx_b) = mpsc::channel(8);
        let res_b = hub.create_session(
            agent_id,
            ObjectId::new(),
            "B".into(),
            ctl_tx_b,
            Permissions::default(),
            Vec::new(),
            None,
            None,
            false,
            ConsentMode::Prompt,
            None,
            None, // local_relay
        );
        assert!(
            res_b.is_ok(),
            "reconnect must reap the orphaned session and succeed, got {res_b:?}"
        );
        assert_ne!(res_b.as_ref().unwrap(), &sid_a, "should be a fresh session");

        // The reap must have re-sent a Terminate to the agent for A (so the
        // agent clears its stale indicator + peer). The agent's queue holds
        // A's Request, then B's — plus a Terminate for A somewhere between.
        let mut saw_terminate_for_a = false;
        while let Ok(msg) = agent_rx.try_recv() {
            if let ServerMsg::Terminate { session_id, .. } = msg
                && session_id == sid_a
            {
                saw_terminate_for_a = true;
            }
        }
        assert!(
            saw_terminate_for_a,
            "agent must be told to terminate the orphaned session (clears the indicator)"
        );

        // Counter integrity: with B live and max_sessions=1, a THIRD live
        // controller must still be rejected — the reap freed exactly one slot,
        // it didn't leak the counter to 0.
        let (ctl_tx_c, _ctl_rx_c) = mpsc::channel(8);
        let res_c = hub.create_session(
            agent_id,
            ObjectId::new(),
            "C".into(),
            ctl_tx_c,
            Permissions::default(),
            Vec::new(),
            None,
            None,
            false,
            ConsentMode::Prompt,
            None,
            None, // local_relay
        );
        assert!(
            matches!(res_c, Err(Error::AgentBusy)),
            "a live session must still occupy the slot, got {res_c:?}"
        );
    }

    /// Loopback-TURN corp-relay (Phase 2): a session that forwarded a local
    /// agent's overlay TURN descriptor must have `forward_offer` append
    /// `turn:{overlay_ip}:{port}` to the REMOTE agent's ICE servers (the
    /// SdpOffer→agent push) — this is what lets the corp-Chrome viewer relay
    /// through the local agent's overlay instead of the capped far coturn.
    #[tokio::test]
    async fn forward_offer_appends_overlay_local_relay_to_agent_ice() {
        let hub = test_hub().await;
        let agent_id = ObjectId::new();
        let (_agent_tx, _cancel, mut agent_rx) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);
        let (ctl_tx, _ctl_rx) = mpsc::channel(8);
        let sid = hub
            .create_session(
                agent_id,
                ObjectId::new(),
                "Goran".into(),
                ctl_tx,
                Permissions::default(),
                Vec::new(),
                None,
                None,
                false,
                ConsentMode::Prompt,
                None,
                Some(LocalRelayDescriptor {
                    turn_port: 47989,
                    overlay_ip: "100.64.0.9".into(),
                    username: "1700000600:uid".into(),
                    credential: "abcd1234".into(),
                }),
            )
            .unwrap();

        hub.forward_offer(sid, "v=0".into()).unwrap();

        // Drain the agent queue; the SdpOffer must carry the overlay relay.
        let mut turn_urls: Vec<String> = Vec::new();
        while let Ok(msg) = agent_rx.try_recv() {
            if let ServerMsg::SdpOffer { ice_servers, .. } = msg {
                turn_urls = ice_servers
                    .iter()
                    .flat_map(|s| s.urls.iter().cloned())
                    .filter(|u| u.starts_with("turn:"))
                    .collect();
            }
        }
        assert_eq!(
            turn_urls,
            vec!["turn:100.64.0.9:47989".to_string()],
            "forward_offer must append the local-agent overlay relay to the agent's ICE"
        );
    }

    /// The overlay-range guard: a non-overlay `local_relay` (e.g. a public IP a
    /// malicious controller injected) must NOT be forwarded to the agent's TURN
    /// client — the agent should never be pointed at an arbitrary host.
    #[tokio::test]
    async fn forward_offer_rejects_non_overlay_local_relay() {
        let hub = test_hub().await;
        let agent_id = ObjectId::new();
        let (_agent_tx, _cancel, mut agent_rx) =
            hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);
        let (ctl_tx, _ctl_rx) = mpsc::channel(8);
        let sid = hub
            .create_session(
                agent_id,
                ObjectId::new(),
                "Goran".into(),
                ctl_tx,
                Permissions::default(),
                Vec::new(),
                None,
                None,
                false,
                ConsentMode::Prompt,
                None,
                Some(LocalRelayDescriptor {
                    turn_port: 47989,
                    overlay_ip: "8.8.8.8".into(), // public — must be rejected
                    username: "u".into(),
                    credential: "c".into(),
                }),
            )
            .unwrap();

        hub.forward_offer(sid, "v=0".into()).unwrap();

        let mut saw_turn = false;
        while let Ok(msg) = agent_rx.try_recv() {
            if let ServerMsg::SdpOffer { ice_servers, .. } = msg {
                saw_turn = ice_servers
                    .iter()
                    .flat_map(|s| s.urls.iter())
                    .any(|u| u.starts_with("turn:"));
            }
        }
        assert!(
            !saw_turn,
            "a non-overlay relay must be rejected, not forwarded to the agent's TURN client"
        );
    }

    #[test]
    fn overlay_relay_ip_predicate_covers_cgnat_and_ula() {
        // 100.64.0.0/10 — the overlay's CGNAT v4 space.
        assert!(is_overlay_relay_ip("100.64.0.1"));
        assert!(is_overlay_relay_ip("100.100.5.5"));
        assert!(is_overlay_relay_ip("100.127.255.255"));
        // Just outside the /10.
        assert!(!is_overlay_relay_ip("100.63.255.255"));
        assert!(!is_overlay_relay_ip("100.128.0.0"));
        // Other private / public / loopback / garbage.
        assert!(!is_overlay_relay_ip("192.168.1.1"));
        assert!(!is_overlay_relay_ip("8.8.8.8"));
        assert!(!is_overlay_relay_ip("127.0.0.1"));
        assert!(!is_overlay_relay_ip("not-an-ip"));
        // fc00::/7 ULA (future overlay v6) accepted; public v6 rejected.
        assert!(is_overlay_relay_ip("fd00::1"));
        assert!(!is_overlay_relay_ip("2001:4860:4860::8888"));
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
