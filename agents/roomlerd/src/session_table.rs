// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-138 P2a — the org loop's remote-control sessions, kept through the loss
//! of the control socket.
//!
//! Until this, each connection owned its session map and every way it could
//! end closed every peer in it (`close_all_peers`), so a server roll ended
//! every live session although its pixels never crossed the server. The map now
//! lives in the org loop that redials ([`crate::signaling::run`]) and is lent to
//! each connection. A session in it is either **attached** to the current
//! connection or **kept** (detached) with a hard deadline, waiting for the next
//! connection's claim (`kept_sessions` in the hello), which the server answers
//! with `rc:session.resumed` or `rc:terminate`.
//!
//! Which way a connection ends decides ([`ConnExit::kind`], spec §2.2): the
//! twelve exits that lost only the path keep what can be kept, and the five that
//! ended the process, the machine or the device's standing close every peer,
//! kept ones included. A refused token closes them too (`run`'s `AuthRejected`
//! arm).
//!
//! ⚠️ **Nothing leaves the table except through [`close_within_budget`].**
//! `AgentPeer`'s `Drop` does not close its `RTCPeerConnection`, and a WebRTC
//! peer that is dropped frees nothing: its UDP sockets belong to tasks its ICE
//! agent spawned. A kept peer that is dropped is exactly the leak that once ate
//! a host's ephemeral port range. So every path out of `kept` closes, the table
//! closes what it holds even when it is dropped, and a peer is kept only when
//! three things hold: the device's switch is on ([`survive_enabled`]), the
//! server this connection spoke to announced a window above 0, and the session
//! was live.
//!
//! Off is today: with the switch off, or no window announced, every exit closes
//! every peer as `close_all_peers` did, the same order, label and line.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bson::oid::ObjectId;
use roomler_ai_remote_control::{models::RpcCap, signaling::ClientMsg};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::peer::AgentPeer;
use crate::signaling::close_within_budget;

/// The device's own ceiling on the server's window: a wrong setting cannot keep
/// a peer for an hour. With [`crate::signaling`]'s close budget it bounds a kept
/// peer's life at 125 s from its detach (spec §4.9).
pub(crate) const WINDOW_CAP: Duration = Duration::from_secs(120);

/// How often a peer kept after a net change is looked at: it closes once its
/// connection leaves `Connected` (spec §4.2).
const NETCHANGE_LOOK_EVERY: Duration = Duration::from_millis(250);

/// A session's own queue, between the senders it holds and the connection's.
/// The connection's size (`PEER_OUTBOUND_CAP`).
const LINK_CAP: usize = 64;

/// A kept session's own end, reported by its link. One a session at most.
const ENDED_CAP: usize = 32;

/// Ends said at the device during a gap, waiting for the next connection. No
/// more than the device's capacity can be live, so this is generous.
const PENDING_ENDS_CAP: usize = 16;

/// How long `rc:ended` may take on the control channel before the close goes
/// ahead without it: the idle warning's bound.
const SAY_BUDGET: Duration = Duration::from_secs(2);

/// The device's switch: `rc_sessions_survive_reattach` in the config, or
/// `ROOMLERD_RC_SESSIONS_SURVIVE_REATTACH`. Default off. Device-side and not
/// pushable: `DesiredConfig` has no such field. Read once per connection.
pub(crate) fn survive_enabled() -> bool {
    tunnel_core::env::flag("RC_SESSIONS_SURVIVE_REATTACH", false)
}

/// `rc-resume` in the hello's `AgentCaps.rpc`, and in every heartbeat that
/// re-announces the caps, exactly while the switch is on. Matched by equality on
/// both sides ([`RpcCap`]).
pub(crate) fn apply_resume_word(rpc: &mut Vec<String>, on: bool) {
    let word = RpcCap::RcResume.wire();
    if !on {
        rpc.retain(|w| w != word);
    } else if !rpc.iter().any(|w| w == word) {
        rpc.push(word.to_string());
    }
}

/// The seventeen ways a connection ends, one per arm of `connect_once` and
/// `handle_server_msg` that used to call `close_all_peers` (spec §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ConnExit {
    /// A stop, an update, a restart.
    Shutdown,
    /// The computer is going to sleep (FR-55 P4).
    Sleep,
    /// A resume from suspend, at least `RESUME_SKEW_MIN` napped.
    ResumeFromSuspend,
    /// No inbound frame within the RX deadline.
    RxDeadline,
    /// The net-change probe, two strikes.
    NetChangeProbe,
    /// The net-change re-probe ping failed.
    NetChangeReprobe,
    /// The keepalive ping unanswered, two strikes.
    KeepaliveUnanswered,
    /// The keepalive ping could not be sent.
    KeepaliveSendFailed,
    /// The net-change probe ping could not be sent.
    NetChangePingFailed,
    /// The heartbeat could not be sent.
    HeartbeatSendFailed,
    /// A peer's message could not be flushed.
    OutboundFlushFailed,
    /// The pong reply failed.
    PongReplyFailed,
    /// The server closed the socket.
    ServerClosed,
    /// A read error. A roll takes this arm.
    ReadError,
    /// The pong RTT degraded, two strikes.
    PongRttDegraded,
    /// `rc:goodbye`: deleted, refused, displaced.
    Goodbye,
    /// FR-40's key rotation.
    KeyRotated,
}

/// What an exit means for the sessions the connection held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitKind {
    /// The process, the machine or the device's standing ended: every peer
    /// closes, kept ones included.
    Terminal,
    /// Only the path to the server ended: a session is kept for its claim.
    Transient,
    /// The path ended after the host's addresses moved: kept, but closed once
    /// its connection leaves `Connected`, since the RC path has no ICE restart.
    NetChange,
}

impl ConnExit {
    #[cfg(test)]
    pub(crate) const ALL: [ConnExit; 17] = [
        ConnExit::Shutdown,
        ConnExit::Sleep,
        ConnExit::ResumeFromSuspend,
        ConnExit::RxDeadline,
        ConnExit::NetChangeProbe,
        ConnExit::NetChangeReprobe,
        ConnExit::KeepaliveUnanswered,
        ConnExit::KeepaliveSendFailed,
        ConnExit::NetChangePingFailed,
        ConnExit::HeartbeatSendFailed,
        ConnExit::OutboundFlushFailed,
        ConnExit::PongReplyFailed,
        ConnExit::ServerClosed,
        ConnExit::ReadError,
        ConnExit::PongRttDegraded,
        ConnExit::Goodbye,
        ConnExit::KeyRotated,
    ];

    /// RED — the pin's map: every arm called `close_all_peers`, so every exit
    /// closes every peer. The next commit gives the spec's (§2.2).
    pub(crate) fn kind(self) -> ExitKind {
        ExitKind::Terminal
    }

    /// The exit's name in the log lines, and the close's reason when it closes.
    pub(crate) fn label(self) -> &'static str {
        match self {
            ConnExit::Shutdown => "shutdown",
            ConnExit::Sleep => "sleep",
            ConnExit::ResumeFromSuspend => "resume_from_suspend",
            ConnExit::RxDeadline => "rx_deadline",
            ConnExit::NetChangeProbe => "netchange_probe",
            ConnExit::NetChangeReprobe => "netchange_reprobe",
            ConnExit::KeepaliveUnanswered => "keepalive_unanswered",
            ConnExit::KeepaliveSendFailed => "keepalive_send_failed",
            ConnExit::NetChangePingFailed => "netchange_ping_failed",
            ConnExit::HeartbeatSendFailed => "heartbeat_send_failed",
            ConnExit::OutboundFlushFailed => "outbound_flush_failed",
            ConnExit::PongReplyFailed => "pong_reply_failed",
            ConnExit::ServerClosed => "server_closed",
            ConnExit::ReadError => "read_error",
            ConnExit::PongRttDegraded => "pong_rtt_degraded",
            ConnExit::Goodbye => "goodbye",
            ConnExit::KeyRotated => "key_rotated",
        }
    }
}

/// What the table needs of a peer: [`AgentPeer`] in the daemon, a counting
/// stand-in in the tests, so that every path into Closed can be counted.
pub(crate) trait TablePeer: Send + Sync + 'static {
    /// Close the peer, and every socket its ICE agent owns with it. Only ever
    /// awaited through [`close_within_budget`].
    fn close(&self) -> impl Future<Output = ()> + Send + '_;
    /// Stop serving the session without closing its transport (FR-27 P10).
    fn stop_serving(&self);
    /// Whether the peer's connection is `Connected` now.
    fn is_connected(&self) -> bool;
    /// Say `text` on the session's control channel: best effort and bounded.
    fn say_on_control(&self, text: String) -> impl Future<Output = ()> + Send + '_;
}

impl TablePeer for AgentPeer {
    fn close(&self) -> impl Future<Output = ()> + Send + '_ {
        AgentPeer::close(self)
    }

    fn stop_serving(&self) {
        AgentPeer::stop_serving(self)
    }

    fn is_connected(&self) -> bool {
        AgentPeer::is_connected(self)
    }

    fn say_on_control(&self, text: String) -> impl Future<Output = ()> + Send + '_ {
        // The stash the idle warning rides: the control channel, once the
        // viewer opened it.
        let (_, control) = self.idle_handles();
        async move {
            let dc = control.lock().await.clone();
            if let Some(dc) = dc {
                let _ = tokio::time::timeout(SAY_BUDGET, dc.send_text(text)).await;
            }
        }
    }
}

/// Which end a session met, for what follows it beyond the close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RealEnd {
    /// The server's end, a deadline, a terminal exit: a recording waits for
    /// its controller (FR-85's grace).
    Ended,
    /// The person at the device ended it (Disconnect, the idle limit): a
    /// recording stops now.
    HostEnded,
}

/// What a session's REAL end does beyond closing its peer: the recording's
/// end, and the banner, whose last end may lock the screen (FR-99). A detach
/// calls none of it: the session is live and its pixels still flow.
pub(crate) type EndHook = Arc<dyn Fn(ObjectId, RealEnd) + Send + Sync>;

/// The daemon's [`EndHook`], what `close_all_peers` did for every session.
pub(crate) fn real_end_hook(indicator: crate::indicator::ViewerIndicator) -> EndHook {
    Arc::new(move |session_id, end| {
        #[cfg(feature = "recording")]
        match end {
            RealEnd::Ended => crate::recording::remote::session_ended(session_id),
            RealEnd::HostEnded => crate::recording::remote::host_ended(session_id),
        }
        #[cfg(not(feature = "recording"))]
        let _ = end;
        indicator.hide_session(session_id.to_hex());
    })
}

/// FR-138 — one session's sender, re-pointable.
///
/// Every sender the session holds is a clone of [`Self::sender`]: the
/// `AgentPeer` (its ICE-candidate handler, the state handler that sends
/// `Terminate` on `Failed`, the watchdog), its `record` context and its
/// `privacy` reporter. A task forwards what they send to the connection the
/// table points the link at, and the table points it at each new connection.
/// While the session is kept the link drops what it is given and never waits,
/// except the session's own `Terminate`, which it hands to the table: the
/// table closes the peer and queues the end for the next connection.
pub(crate) struct SessionLink {
    tx: mpsc::Sender<ClientMsg>,
    route: watch::Sender<Route>,
}

#[derive(Clone)]
enum Route {
    /// This connection's outbound queue.
    To(mpsc::Sender<ClientMsg>),
    /// No connection: nobody reads what the session says.
    Kept,
}

impl SessionLink {
    fn new(
        session_id: ObjectId,
        to: mpsc::Sender<ClientMsg>,
        ended: mpsc::Sender<(ObjectId, ClientMsg)>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(LINK_CAP);
        let (route, route_rx) = watch::channel(Route::To(to));
        tokio::spawn(forward(session_id, rx, route_rx, ended));
        Self { tx, route }
    }

    /// The sender every holder of the session clones.
    pub(crate) fn sender(&self) -> mpsc::Sender<ClientMsg> {
        self.tx.clone()
    }

    fn point_at(&self, to: mpsc::Sender<ClientMsg>) {
        self.route.send_replace(Route::To(to));
    }

    fn keep(&self) {
        self.route.send_replace(Route::Kept);
    }
}

/// `msg` is the session's own end: its state handler's `Terminate` on `Failed`,
/// or its watchdog's.
fn is_own_end(msg: &ClientMsg, session_id: ObjectId) -> bool {
    matches!(msg, ClientMsg::Terminate { session_id: s, .. } if *s == session_id)
}

/// The link's task. While attached it waits for room on the connection's queue
/// as a direct send did, and moves the moment the route does, so a send that was
/// waiting on a connection that died is never stuck. While kept it drops
/// everything at once.
async fn forward(
    session_id: ObjectId,
    mut rx: mpsc::Receiver<ClientMsg>,
    mut route: watch::Receiver<Route>,
    ended: mpsc::Sender<(ObjectId, ClientMsg)>,
) {
    // `false` once the table let the link go (its session closed): the route
    // stays where it last was, as a direct send's would.
    let mut movable = true;
    while let Some(msg) = rx.recv().await {
        let mut next = Some(msg);
        while let Some(msg) = next.take() {
            let to = route.borrow_and_update().clone();
            match to {
                Route::Kept => {
                    if is_own_end(&msg, session_id) {
                        // Never waits: a full queue costs the report, and the
                        // deadline still closes the peer.
                        let _ = ended.try_send((session_id, msg));
                    }
                }
                Route::To(tx) if movable => tokio::select! {
                    biased;
                    moved = route.changed() => {
                        if moved.is_err() {
                            movable = false;
                        }
                        next = Some(msg);
                    }
                    permit = tx.reserve() => match permit {
                        Ok(permit) => permit.send(msg),
                        // The connection is gone. A route that moved meanwhile
                        // takes the message; otherwise it is dropped, as a send
                        // into a closed queue was.
                        Err(_) => {
                            if route.has_changed().unwrap_or(false) {
                                next = Some(msg);
                            }
                        }
                    },
                },
                Route::To(tx) => {
                    let _ = tx.send(msg).await;
                }
            }
        }
    }
}

/// `rc:ended` on the control channel: the session's end, said peer to peer,
/// because the viewer's signalling cannot reach it during the gap (spec §4.3).
/// The same `reason` and `limit` the `rc:terminate` carries.
fn ended_text(terminate: &ClientMsg) -> Option<String> {
    let ClientMsg::Terminate { reason, limit, .. } = terminate else {
        return None;
    };
    let mut said = serde_json::json!({ "t": "rc:ended", "reason": reason });
    if let Some(limit) = limit {
        said["limit"] = serde_json::Value::from(limit.as_str());
    }
    Some(said.to_string())
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// A session attached to the current connection.
struct Live<P> {
    peer: P,
    /// `None`: never kept. A delegated session (FR-43, P2b's), an external
    /// one (FR-52, D3), or one built while the switch was off.
    link: Option<SessionLink>,
}

/// A session kept through the loss of its connection.
struct Kept<P> {
    peer: P,
    link: SessionLink,
    since: Instant,
    /// Set once, at the detach. No reconnect moves it: a claim the server has
    /// not answered by then is a claim refused.
    deadline: Instant,
    /// Detached by a net-change exit: closed once it leaves `Connected`.
    netchange: bool,
    exit: ConnExit,
}

/// What the table must act on next ([`SessionTable::next_event`]).
#[derive(Debug)]
pub(crate) enum TableEvent {
    /// A kept session's deadline passed.
    Deadline,
    /// A kept session's peer said its own end (it failed).
    Ended(ObjectId, ClientMsg),
    /// Time to look at the peers kept after a net change.
    Look,
}

/// FR-138 — the org loop's remote-control sessions. See the module doc.
pub(crate) struct SessionTable<P: TablePeer = AgentPeer> {
    attached: HashMap<ObjectId, Live<P>>,
    kept: HashMap<ObjectId, Kept<P>>,
    /// Ends the device said while no connection was up, for the next one.
    pending_ends: Vec<ClientMsg>,
    /// This connection's switch.
    survive: bool,
    /// The window the server this connection speaks to announced, capped;
    /// `None` = none, or withdrawn.
    window: Option<Duration>,
    /// This connection's outbound queue, while one is up.
    current: Option<mpsc::Sender<ClientMsg>>,
    /// `rc_max_sessions`: kept sessions count against it.
    capacity: usize,
    on_end: EndHook,
    ended_tx: mpsc::Sender<(ObjectId, ClientMsg)>,
    ended_rx: mpsc::Receiver<(ObjectId, ClientMsg)>,
    /// The next look at the peers kept after a net change. An instant, not a
    /// period: a loop that comes round often must not push it back.
    next_look: Option<Instant>,
}

impl<P: TablePeer> SessionTable<P> {
    pub(crate) fn new(capacity: usize, on_end: EndHook) -> Self {
        let (ended_tx, ended_rx) = mpsc::channel(ENDED_CAP);
        Self {
            attached: HashMap::new(),
            kept: HashMap::new(),
            pending_ends: Vec::new(),
            survive: false,
            window: None,
            current: None,
            capacity: capacity.max(1),
            on_end,
            ended_tx,
            ended_rx,
            next_look: None,
        }
    }

    /// A connection is up: its switch, read once. The window starts unannounced
    /// and the deadlines that already passed close here. With the switch off
    /// nothing kept can be claimed, so it closes. Returns the claim, the hello's
    /// `kept_sessions`.
    pub(crate) async fn begin_connection(&mut self, survive: bool) -> Vec<ObjectId> {
        self.survive = survive;
        self.window = None;
        self.current = None;
        self.close_due().await;
        if !survive {
            let kept: Vec<ObjectId> = self.kept.keys().copied().collect();
            for session_id in kept {
                self.close_kept(session_id, "switch_off").await;
            }
        }
        self.kept_ids()
    }

    /// The hello went out on a connection whose queue is `outbound`: what the
    /// device ended during the gap follows it, the way FR-40 says its rotation
    /// again.
    pub(crate) fn connected(&mut self, outbound: &mpsc::Sender<ClientMsg>) {
        self.current = Some(outbound.clone());
        for msg in std::mem::take(&mut self.pending_ends) {
            self.queue_end(msg);
        }
    }

    /// Whether this connection builds sessions it may keep: its switch.
    pub(crate) fn keeps_sessions(&self) -> bool {
        self.survive
    }

    /// `rc:agent.resume_window`: the window, capped at [`WINDOW_CAP`], or `None`
    /// when withdrawn (0). Ignored with the switch off.
    pub(crate) fn set_window(&mut self, secs: u32) -> Option<Duration> {
        if !self.survive {
            debug!(
                secs,
                "rc: a resume window for a device whose switch is off; ignored (FR-138)"
            );
            return None;
        }
        self.window = (secs > 0).then(|| Duration::from_secs(u64::from(secs)).min(WINDOW_CAP));
        info!(
            secs,
            kept_s = self.window.map(|w| w.as_secs()),
            "rc: the server announced a resume window (FR-138)"
        );
        self.window
    }

    /// A link for a session this connection builds, pointed at `outbound`.
    pub(crate) fn link(
        &self,
        session_id: ObjectId,
        outbound: &mpsc::Sender<ClientMsg>,
    ) -> SessionLink {
        SessionLink::new(session_id, outbound.clone(), self.ended_tx.clone())
    }

    /// A live session, attached. `link` is `None` for a session never kept.
    /// One the table already held under that id is closed first: a peer it
    /// lets go of is a peer it closes.
    pub(crate) async fn insert(
        &mut self,
        session_id: ObjectId,
        peer: P,
        link: Option<SessionLink>,
    ) {
        if let Some(old) = self.take(&session_id, "displaced") {
            warn!(session_id = %session_id, "rc: a session the table held was displaced; closing it");
            close_within_budget(old.close(), session_id, "displaced").await;
        }
        self.attached.insert(session_id, Live { peer, link });
    }

    /// An attached session's peer.
    pub(crate) fn get(&self, session_id: &ObjectId) -> Option<&P> {
        self.attached.get(session_id).map(|l| &l.peer)
    }

    /// The attached sessions, the ones this connection serves.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&ObjectId, &P)> {
        self.attached.iter().map(|(s, l)| (s, &l.peer))
    }

    /// Every live session, kept ones included: what counts against capacity.
    pub(crate) fn len(&self) -> usize {
        self.attached.len() + self.kept.len()
    }

    #[cfg(test)]
    pub(crate) fn attached_len(&self) -> usize {
        self.attached.len()
    }

    pub(crate) fn holds(&self, session_id: &ObjectId) -> bool {
        self.attached.contains_key(session_id) || self.kept.contains_key(session_id)
    }

    pub(crate) fn has_kept(&self) -> bool {
        !self.kept.is_empty()
    }

    /// The kept sessions, in a stable order: the hello's claim.
    pub(crate) fn kept_ids(&self) -> Vec<ObjectId> {
        let mut ids: Vec<ObjectId> = self.kept.keys().copied().collect();
        ids.sort();
        ids
    }

    /// A new request would put a session beside a kept peer past the device's
    /// capacity: it is refused as busy.
    pub(crate) fn full_beside_kept(&self) -> bool {
        !self.kept.is_empty() && self.len() >= self.capacity
    }

    /// Take a session out to end it: the server's terminate (a refused claim
    /// for a kept one), a re-offer, the device's end of an attached one. The
    /// caller closes it within the budget, as each of those arms always has.
    pub(crate) fn take(&mut self, session_id: &ObjectId, why: &'static str) -> Option<P> {
        if let Some(live) = self.attached.remove(session_id) {
            return Some(live.peer);
        }
        let kept = self.kept.remove(session_id)?;
        info!(
            session_id = %session_id,
            why,
            kept_ms = millis(kept.since.elapsed()),
            "rc session closed: a kept session ended (FR-138)"
        );
        Some(kept.peer)
    }

    /// The server accepted the claim: the session goes on, its senders pointed
    /// at this connection. `false` when the table no longer holds it, its
    /// deadline having come first: the caller tells the server.
    pub(crate) async fn resume(
        &mut self,
        session_id: ObjectId,
        outbound: &mpsc::Sender<ClientMsg>,
    ) -> bool {
        if self.attached.contains_key(&session_id) {
            debug!(session_id = %session_id, "rc:session.resumed for a session already attached");
            return true;
        }
        let Some(kept) = self.kept.remove(&session_id) else {
            info!(
                session_id = %session_id,
                "rc:session.resumed for a session this device no longer holds (FR-138)"
            );
            return false;
        };
        if Instant::now() >= kept.deadline {
            self.kept.insert(session_id, kept);
            self.close_kept(session_id, "claim_late").await;
            return false;
        }
        kept.link.point_at(outbound.clone());
        info!(
            session_id = %session_id,
            exit = kept.exit.label(),
            kept_ms = millis(kept.since.elapsed()),
            "rc session resumed: re-attached to the new connection (FR-138)"
        );
        self.attached.insert(
            session_id,
            Live {
                peer: kept.peer,
                link: Some(kept.link),
            },
        );
        true
    }

    /// The connection ended by `exit`. A terminal exit, the switch off or no
    /// window closes every peer, kept ones included. Otherwise every attached
    /// session that can be kept is kept until its deadline, and the rest close.
    pub(crate) async fn end_connection(&mut self, exit: ConnExit) {
        self.current = None;
        let kind = exit.kind();
        let window = match self.window {
            Some(window) if self.survive && kind != ExitKind::Terminal => window,
            _ => return self.close_all(exit.label()).await,
        };
        let now = Instant::now();
        let netchange = kind == ExitKind::NetChange;
        let mut closing: Vec<(ObjectId, P, &'static str)> = Vec::new();
        let attached: Vec<(ObjectId, Live<P>)> = self.attached.drain().collect();
        for (session_id, live) in attached {
            let Some(link) = live.link else {
                closing.push((session_id, live.peer, "ws_disconnect"));
                continue;
            };
            if self.kept.len() >= self.capacity {
                closing.push((session_id, live.peer, "over_capacity"));
                continue;
            }
            // The host's addresses moved and the peer is already off its path:
            // it cannot come back without an ICE restart.
            if netchange && !live.peer.is_connected() {
                closing.push((session_id, live.peer, "netchange_not_connected"));
                continue;
            }
            link.keep();
            info!(
                session_id = %session_id,
                exit = exit.label(),
                window_s = window.as_secs(),
                netchange,
                "rc session detached: its peer is kept until the server answers the claim, \
                 for the window at most (FR-138)"
            );
            self.kept.insert(
                session_id,
                Kept {
                    peer: live.peer,
                    link,
                    since: now,
                    deadline: now + window,
                    netchange,
                    exit,
                },
            );
        }
        if netchange && self.next_look.is_none() {
            self.next_look = Some(now + NETCHANGE_LOOK_EVERY);
        }
        for (session_id, peer, why) in closing {
            (self.on_end)(session_id, RealEnd::Ended);
            info!(
                session_id = %session_id,
                why,
                exit = exit.label(),
                "rc session closed at the control socket's loss: it could not be kept (FR-138)"
            );
            close_within_budget(peer.close(), session_id, why).await;
        }
    }

    /// Close every peer, attached and kept: a terminal exit, a refused token,
    /// the loop's end. The attached half is `close_all_peers` as it was, the same
    /// order, label and line.
    ///
    /// rc.24 — each real end takes its banner down: before, the indicator hid
    /// only on `rc:terminate`, which never comes when the socket itself drops,
    /// and after a web deploy the "Being viewed by" frame stayed painted while
    /// the controller could not reconnect ("agent capacity exceeded"). A kept
    /// session's banner stays up, because its session is still live.
    pub(crate) async fn close_all(&mut self, why: &'static str) {
        self.current = None;
        if !self.attached.is_empty() {
            let count = self.attached.len();
            let attached: Vec<(ObjectId, Live<P>)> = self.attached.drain().collect();
            for (session_id, live) in attached {
                (self.on_end)(session_id, RealEnd::Ended);
                close_within_budget(live.peer.close(), session_id, "ws_disconnect").await;
            }
            info!(
                count,
                "torn down peers + hid indicator overlays on ws disconnect"
            );
        }
        let kept: Vec<ObjectId> = self.kept.keys().copied().collect();
        for session_id in kept {
            self.close_kept(session_id, why).await;
        }
    }

    /// A connection ended without its exit arm (a `?`, such as a failed send
    /// of an answer): what it left attached closes. Kept sessions stay.
    pub(crate) async fn close_stranded(&mut self) {
        self.current = None;
        if self.attached.is_empty() {
            return;
        }
        warn!(
            count = self.attached.len(),
            "a connection ended without its exit arm; closing the sessions it left attached"
        );
        let attached: Vec<(ObjectId, Live<P>)> = self.attached.drain().collect();
        for (session_id, live) in attached {
            (self.on_end)(session_id, RealEnd::Ended);
            close_within_budget(live.peer.close(), session_id, "connection_ended").await;
        }
    }

    /// The person at the device ended a kept session (Disconnect, the idle
    /// limit): it stops serving at once, says `rc:ended` on its control channel,
    /// closes after `grace`, and its `terminate` goes to the server on the
    /// current connection, or the next. `false` when the session is not kept.
    pub(crate) fn end_kept_by_device(
        &mut self,
        session_id: ObjectId,
        terminate: ClientMsg,
        why: &'static str,
        grace: Duration,
    ) -> bool {
        let Some(kept) = self.kept.remove(&session_id) else {
            return false;
        };
        (self.on_end)(session_id, RealEnd::HostEnded);
        kept.peer.stop_serving();
        info!(
            session_id = %session_id,
            why,
            kept_ms = millis(kept.since.elapsed()),
            grace_ms = millis(grace),
            "rc session closed: ended at the device while kept; said on its control channel, \
             and its end goes to the server (FR-138)"
        );
        let said = ended_text(&terminate);
        let Kept { peer, link, .. } = kept;
        tokio::spawn(async move {
            if let Some(said) = said {
                peer.say_on_control(said).await;
            }
            tokio::time::sleep(grace).await;
            close_within_budget(peer.close(), session_id, why).await;
            drop(link);
        });
        self.queue_end(terminate);
        true
    }

    /// An end for the server: on the current connection, or the next one.
    pub(crate) fn queue_end(&mut self, msg: ClientMsg) {
        let msg = match &self.current {
            Some(tx) => match tx.try_send(msg) {
                Ok(()) => return,
                Err(e) => e.into_inner(),
            },
            None => msg,
        };
        if self.pending_ends.len() >= PENDING_ENDS_CAP {
            let dropped = self.pending_ends.remove(0);
            warn!(
                ?dropped,
                "rc: too many ends queued for the next connection; dropping the oldest"
            );
        }
        self.pending_ends.push(msg);
    }

    /// The next thing the table must act on: a deadline, a kept peer's own
    /// end, a look at the peers kept after a net change. Pends while nothing is
    /// kept. Cancel-safe, for a `select!` arm.
    pub(crate) async fn next_event(&mut self) -> TableEvent {
        let deadline = self.kept.values().map(|k| k.deadline).min();
        let look = self
            .next_look
            .filter(|_| self.kept.values().any(|k| k.netchange));
        tokio::select! {
            biased;
            Some((session_id, msg)) = self.ended_rx.recv() => TableEvent::Ended(session_id, msg),
            () = sleep_until(deadline) => TableEvent::Deadline,
            () = sleep_until(look) => TableEvent::Look,
        }
    }

    /// Act on `event`. Every close goes through [`close_within_budget`].
    pub(crate) async fn serve(&mut self, event: TableEvent) {
        match event {
            TableEvent::Deadline => self.close_due().await,
            TableEvent::Ended(session_id, msg) => {
                if self.kept.contains_key(&session_id) {
                    self.close_kept(session_id, "failed").await;
                    self.queue_end(msg);
                } else if self.attached.contains_key(&session_id) {
                    // Resumed before this was read: say it on this connection,
                    // as an attached peer's own end always went.
                    self.queue_end(msg);
                }
            }
            TableEvent::Look => {
                let left: Vec<ObjectId> = self
                    .kept
                    .iter()
                    .filter(|(_, k)| k.netchange && !k.peer.is_connected())
                    .map(|(s, _)| *s)
                    .collect();
                for session_id in left {
                    self.close_kept(session_id, "netchange_left_connected")
                        .await;
                }
                self.next_look = self
                    .kept
                    .values()
                    .any(|k| k.netchange)
                    .then(|| Instant::now() + NETCHANGE_LOOK_EVERY);
            }
        }
    }

    async fn close_due(&mut self) {
        let now = Instant::now();
        let due: Vec<ObjectId> = self
            .kept
            .iter()
            .filter(|(_, k)| k.deadline <= now)
            .map(|(s, _)| *s)
            .collect();
        for session_id in due {
            self.close_kept(session_id, "deadline").await;
        }
    }

    async fn close_kept(&mut self, session_id: ObjectId, why: &'static str) {
        let Some(kept) = self.kept.remove(&session_id) else {
            return;
        };
        (self.on_end)(session_id, RealEnd::Ended);
        info!(
            session_id = %session_id,
            why,
            kept_ms = millis(kept.since.elapsed()),
            "rc session closed: a kept session ended (FR-138)"
        );
        close_within_budget(kept.peer.close(), session_id, why).await;
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// The backstop: a table dropped while it holds a peer still closes it, within
/// the budget, on a task of its own. Every return of the org loop closes the
/// table first, so this reads as a WARN when it ever runs.
impl<P: TablePeer> Drop for SessionTable<P> {
    fn drop(&mut self) {
        let mut peers: Vec<(ObjectId, P)> =
            self.attached.drain().map(|(s, l)| (s, l.peer)).collect();
        peers.extend(self.kept.drain().map(|(s, k)| (s, k.peer)));
        if peers.is_empty() {
            return;
        }
        for (session_id, _) in &peers {
            (self.on_end)(*session_id, RealEnd::Ended);
        }
        warn!(
            count = peers.len(),
            "rc: the session table was dropped holding live peers; closing them"
        );
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    for (session_id, peer) in peers {
                        close_within_budget(peer.close(), session_id, "table_dropped").await;
                    }
                });
            }
            Err(_) => tracing::error!(
                "rc: no runtime to close the dropped table's peers; their sockets stay \
                 open until the process exits"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_remote_control::models::EndReason;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A peer that counts its closes: every path into Closed is counted.
    #[derive(Clone, Default)]
    struct Fake(Arc<FakeState>);

    #[derive(Default)]
    struct FakeState {
        closes: AtomicUsize,
        stopped: AtomicBool,
        connected: AtomicBool,
        said: Mutex<Vec<String>>,
    }

    impl Fake {
        fn connected() -> Self {
            let f = Self::default();
            f.set_connected(true);
            f
        }
        fn set_connected(&self, on: bool) {
            self.0.connected.store(on, Ordering::SeqCst);
        }
        fn closes(&self) -> usize {
            self.0.closes.load(Ordering::SeqCst)
        }
        fn stopped(&self) -> bool {
            self.0.stopped.load(Ordering::SeqCst)
        }
        fn said(&self) -> Vec<serde_json::Value> {
            self.0
                .said
                .lock()
                .unwrap()
                .iter()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect()
        }
    }

    impl TablePeer for Fake {
        fn close(&self) -> impl Future<Output = ()> + Send + '_ {
            async move {
                self.0.closes.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn stop_serving(&self) {
            self.0.stopped.store(true, Ordering::SeqCst);
        }
        fn is_connected(&self) -> bool {
            self.0.connected.load(Ordering::SeqCst)
        }
        fn say_on_control(&self, text: String) -> impl Future<Output = ()> + Send + '_ {
            async move {
                self.0.said.lock().unwrap().push(text);
            }
        }
    }

    /// The real ends the hook saw: the banner, the lock and the recording.
    #[derive(Clone, Default)]
    struct Ends(Arc<Mutex<Vec<(ObjectId, RealEnd)>>>);

    impl Ends {
        fn hook(&self) -> EndHook {
            let me = self.clone();
            Arc::new(move |s, e| me.0.lock().unwrap().push((s, e)))
        }
        fn all(&self) -> Vec<(ObjectId, RealEnd)> {
            self.0.lock().unwrap().clone()
        }
    }

    struct Session {
        id: ObjectId,
        peer: Fake,
        /// What the peer's handlers hold: the link's sender, or the
        /// connection's when the session is never kept.
        held: mpsc::Sender<ClientMsg>,
    }

    struct Rig {
        table: SessionTable<Fake>,
        ends: Ends,
        conn: mpsc::Receiver<ClientMsg>,
        tx: mpsc::Sender<ClientMsg>,
        sessions: Vec<Session>,
    }

    impl Rig {
        fn closes(&self) -> usize {
            self.sessions.iter().map(|s| s.peer.closes()).sum()
        }
    }

    /// A table on a connection whose switch is `survive` and whose server
    /// announced `window`, holding `n` live sessions built the way the offer arm
    /// builds them.
    async fn rig(survive: bool, window: Option<u32>, n: usize) -> Rig {
        rig_with(8, survive, window, n).await
    }

    async fn rig_with(capacity: usize, survive: bool, window: Option<u32>, n: usize) -> Rig {
        let ends = Ends::default();
        let mut table = SessionTable::new(capacity, ends.hook());
        let (tx, conn) = mpsc::channel(64);
        assert!(table.begin_connection(survive).await.is_empty());
        table.connected(&tx);
        if let Some(w) = window {
            table.set_window(w);
        }
        let mut sessions = Vec::new();
        for _ in 0..n {
            sessions.push(attach(&mut table, &tx).await);
        }
        Rig {
            table,
            ends,
            conn,
            tx,
            sessions,
        }
    }

    async fn attach(table: &mut SessionTable<Fake>, tx: &mpsc::Sender<ClientMsg>) -> Session {
        let id = ObjectId::new();
        let peer = Fake::connected();
        let link = table.keeps_sessions().then(|| table.link(id, tx));
        let held = link
            .as_ref()
            .map_or_else(|| tx.clone(), SessionLink::sender);
        table.insert(id, peer.clone(), link).await;
        Session { id, peer, held }
    }

    /// The next connection: its claim, and its queue.
    async fn reconnect(
        table: &mut SessionTable<Fake>,
    ) -> (
        Vec<ObjectId>,
        mpsc::Sender<ClientMsg>,
        mpsc::Receiver<ClientMsg>,
    ) {
        let (tx, rx) = mpsc::channel(64);
        let claim = table.begin_connection(true).await;
        table.connected(&tx);
        (claim, tx, rx)
    }

    fn terminate(session_id: ObjectId, reason: EndReason, limit: Option<&str>) -> ClientMsg {
        ClientMsg::Terminate {
            session_id,
            reason,
            limit: limit.map(str::to_string),
        }
    }

    /// The table's next event: a test never waits for ever (on a paused clock
    /// the ten minutes pass at once).
    async fn next(table: &mut SessionTable<Fake>) -> TableEvent {
        tokio::time::timeout(Duration::from_secs(600), table.next_event())
            .await
            .expect("the table has something to act on")
    }

    /// Until the link's task has taken everything sent on `held` (one thread:
    /// once it has, it has also routed it).
    async fn settled(held: &mpsc::Sender<ClientMsg>) {
        while held.capacity() < held.max_capacity() {
            tokio::task::yield_now().await;
        }
    }

    fn ice(session_id: ObjectId) -> ClientMsg {
        ClientMsg::Ice {
            session_id,
            candidate: serde_json::json!("candidate:1 1 udp 1 10.0.0.1 9 typ host"),
        }
    }

    // ─── The seventeen arms (spec §2.2) ─────────────────────────────────────
    //
    // Each runs the arm with the switch on and a 60 s window announced, two
    // live sessions in the table. Written first: at the pin every arm closes
    // every peer, so the twelve transient arms fail there.

    async fn the_arm_keeps_every_session(exit: ConnExit) {
        let mut r = rig(true, Some(60), 2).await;
        r.table.end_connection(exit).await;
        assert_eq!(r.closes(), 0, "{exit:?} closed a peer it should keep");
        assert_eq!(r.table.kept_ids().len(), 2, "{exit:?} keeps both sessions");
        assert_eq!(
            r.table.attached_len(),
            0,
            "{exit:?}: nothing stays attached"
        );
        assert!(
            r.ends.all().is_empty(),
            "{exit:?}: a detach takes no banner down, locks nothing, ends no recording"
        );
        r.table.close_all("test_end").await;
        assert_eq!(r.closes(), 2);
    }

    async fn the_arm_closes_every_peer(exit: ConnExit) {
        let mut r = rig(true, Some(60), 2).await;
        r.table.end_connection(exit).await;
        for s in &r.sessions {
            assert_eq!(s.peer.closes(), 1, "{exit:?} must close {}", s.id);
        }
        assert_eq!(r.table.len(), 0, "{exit:?} leaves nothing in the table");
        assert_eq!(
            r.ends.all().len(),
            2,
            "{exit:?}: each real end takes its banner down"
        );
    }

    macro_rules! per_arm {
        ($($name:ident: $exit:ident => $how:ident,)*) => {$(
            #[tokio::test]
            async fn $name() {
                $how(ConnExit::$exit).await
            }
        )*};
    }

    per_arm! {
        arm_shutdown_closes_every_peer: Shutdown => the_arm_closes_every_peer,
        arm_sleep_closes_every_peer: Sleep => the_arm_closes_every_peer,
        arm_resume_from_suspend_closes_every_peer: ResumeFromSuspend => the_arm_closes_every_peer,
        arm_rx_deadline_keeps_its_sessions: RxDeadline => the_arm_keeps_every_session,
        arm_netchange_probe_keeps_its_sessions: NetChangeProbe => the_arm_keeps_every_session,
        arm_netchange_reprobe_keeps_its_sessions: NetChangeReprobe => the_arm_keeps_every_session,
        arm_keepalive_unanswered_keeps_its_sessions: KeepaliveUnanswered => the_arm_keeps_every_session,
        arm_keepalive_send_failed_keeps_its_sessions: KeepaliveSendFailed => the_arm_keeps_every_session,
        arm_netchange_ping_failed_keeps_its_sessions: NetChangePingFailed => the_arm_keeps_every_session,
        arm_heartbeat_send_failed_keeps_its_sessions: HeartbeatSendFailed => the_arm_keeps_every_session,
        arm_outbound_flush_failed_keeps_its_sessions: OutboundFlushFailed => the_arm_keeps_every_session,
        arm_pong_reply_failed_keeps_its_sessions: PongReplyFailed => the_arm_keeps_every_session,
        arm_server_closed_keeps_its_sessions: ServerClosed => the_arm_keeps_every_session,
        arm_read_error_keeps_its_sessions: ReadError => the_arm_keeps_every_session,
        arm_pong_rtt_degraded_keeps_its_sessions: PongRttDegraded => the_arm_keeps_every_session,
        arm_goodbye_closes_every_peer: Goodbye => the_arm_closes_every_peer,
        arm_key_rotated_closes_every_peer: KeyRotated => the_arm_closes_every_peer,
    }

    #[test]
    fn twelve_arms_keep_and_five_close() {
        let terminal: Vec<ConnExit> = ConnExit::ALL
            .into_iter()
            .filter(|e| e.kind() == ExitKind::Terminal)
            .collect();
        assert_eq!(
            terminal,
            [
                ConnExit::Shutdown,
                ConnExit::Sleep,
                ConnExit::ResumeFromSuspend,
                ConnExit::Goodbye,
                ConnExit::KeyRotated
            ]
        );
        let netchange: Vec<ConnExit> = ConnExit::ALL
            .into_iter()
            .filter(|e| e.kind() == ExitKind::NetChange)
            .collect();
        assert_eq!(
            netchange,
            [
                ConnExit::NetChangeProbe,
                ConnExit::NetChangeReprobe,
                ConnExit::NetChangePingFailed
            ]
        );
        let labels: std::collections::HashSet<&str> =
            ConnExit::ALL.iter().map(|e| e.label()).collect();
        assert_eq!(labels.len(), 17, "every exit has its own label");
    }

    /// Every arm of the connection ends through the table, under its own name,
    /// and nothing else closes the connection's peers: the per-arm tests above
    /// are the arms the loop runs.
    #[test]
    fn every_arm_of_the_connection_ends_through_the_table() {
        let src = include_str!("signaling.rs");
        let code = src.split("\nmod tests {").next().unwrap();
        for exit in ConnExit::ALL {
            let call = format!("end_connection(ConnExit::{exit:?})");
            assert_eq!(
                code.matches(&call).count(),
                1,
                "{call} must be the arm's one call"
            );
        }
        assert!(
            !code.contains("close_all_peers("),
            "a connection's peers close only through the session table"
        );
        assert_eq!(
            code.matches("close_all(\"token_refused\")").count(),
            1,
            "a refused token closes every peer, kept ones included"
        );
    }

    // ─── Off is today ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn with_the_switch_off_every_arm_closes_every_peer() {
        for exit in ConnExit::ALL {
            // A window sent to a device whose switch is off changes nothing.
            let mut r = rig(false, Some(60), 2).await;
            assert!(!r.table.keeps_sessions());
            r.table.end_connection(exit).await;
            assert_eq!(
                r.closes(),
                2,
                "{exit:?} with the switch off closes every peer"
            );
            assert_eq!(r.table.len(), 0, "{exit:?}");
        }
    }

    #[tokio::test]
    async fn with_no_window_every_arm_closes_every_peer() {
        for window in [None, Some(0)] {
            for exit in ConnExit::ALL {
                let mut r = rig(true, window, 2).await;
                r.table.end_connection(exit).await;
                assert_eq!(
                    r.closes(),
                    2,
                    "{exit:?}, window {window:?}: every peer closes"
                );
                assert_eq!(r.table.len(), 0, "{exit:?}");
            }
        }
    }

    #[tokio::test]
    async fn a_window_withdrawn_keeps_nothing_at_the_next_exit() {
        let mut r = rig(true, Some(60), 1).await;
        assert_eq!(r.table.set_window(0), None);
        r.table.end_connection(ConnExit::ReadError).await;
        assert_eq!(r.closes(), 1);
    }

    /// A supervised Mac's delegated session (P2b's) and an external one (D3)
    /// are built with no link, and end as today: closed at every exit.
    #[tokio::test]
    async fn a_delegated_or_external_session_closes_at_a_transient_exit() {
        let mut r = rig(true, Some(60), 1).await;
        let id = ObjectId::new();
        let never = Fake::connected();
        r.table.insert(id, never.clone(), None).await;
        r.table.end_connection(ConnExit::ReadError).await;
        assert_eq!(
            never.closes(),
            1,
            "a session with no link is closed as before"
        );
        assert_eq!(r.table.kept_ids(), vec![r.sessions[0].id]);
        r.table.close_all("test_end").await;
    }

    /// A connection that ends through a `?` (a failed send of an answer) has no
    /// exit arm: `run` closes what it left attached, and keeps what was kept.
    #[tokio::test]
    async fn a_connection_that_ends_without_its_arm_strands_nothing() {
        let mut r = rig(true, Some(60), 1).await;
        r.table.end_connection(ConnExit::ReadError).await;
        let (_, tx2, _conn2) = reconnect(&mut r.table).await;
        let fresh = attach(&mut r.table, &tx2).await;
        r.table.close_stranded().await;
        assert_eq!(fresh.peer.closes(), 1, "the attached one closes");
        assert_eq!(r.closes(), 0, "the kept one waits for its claim");
        assert_eq!(r.table.kept_ids(), vec![r.sessions[0].id]);
        r.table.close_all("test_end").await;
    }

    // ─── The paths into Closed (spec §4.2's diagram) ─────────────────────────

    #[tokio::test(start_paused = true)]
    async fn the_deadline_closes_a_kept_session() {
        let mut r = rig(true, Some(60), 1).await;
        r.table.end_connection(ConnExit::ReadError).await;
        let early = tokio::time::timeout(Duration::from_secs(59), r.table.next_event()).await;
        assert!(early.is_err(), "nothing is due inside the window");
        let event = next(&mut r.table).await;
        assert!(matches!(event, TableEvent::Deadline), "{event:?}");
        r.table.serve(event).await;
        assert_eq!(r.closes(), 1);
        assert_eq!(r.table.len(), 0);
        assert_eq!(r.ends.all(), vec![(r.sessions[0].id, RealEnd::Ended)]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_device_caps_the_window_at_two_minutes() {
        let mut r = rig(true, None, 1).await;
        assert_eq!(r.table.set_window(3600), Some(WINDOW_CAP));
        r.table.end_connection(ConnExit::ReadError).await;
        let early = tokio::time::timeout(Duration::from_secs(119), r.table.next_event()).await;
        assert!(early.is_err(), "kept for the cap, not the server's hour");
        let event = next(&mut r.table).await;
        r.table.serve(event).await;
        assert_eq!(r.closes(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reconnect_does_not_move_the_deadline() {
        let mut r = rig(true, Some(60), 1).await;
        r.table.end_connection(ConnExit::ReadError).await;
        tokio::time::sleep(Duration::from_secs(30)).await;
        let (claim, _tx2, _conn2) = reconnect(&mut r.table).await;
        assert_eq!(claim, vec![r.sessions[0].id], "the hello claims it");
        r.table.set_window(60);
        let early = tokio::time::timeout(Duration::from_secs(29), r.table.next_event()).await;
        assert!(early.is_err());
        let event = next(&mut r.table).await;
        r.table.serve(event).await;
        assert_eq!(r.closes(), 1, "closed 60 s after the detach, unanswered");
    }

    #[tokio::test(start_paused = true)]
    async fn a_claim_answered_after_its_deadline_is_refused() {
        let mut r = rig(true, Some(60), 1).await;
        let id = r.sessions[0].id;
        r.table.end_connection(ConnExit::ReadError).await;
        let (claim, tx2, _conn2) = reconnect(&mut r.table).await;
        assert_eq!(claim, vec![id]);
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert!(!r.table.resume(id, &tx2).await, "too late: refused");
        assert_eq!(r.closes(), 1);
        assert_eq!(r.table.len(), 0);
    }

    #[tokio::test]
    async fn a_resumed_session_is_attached_and_sends_on_the_new_connection() {
        let mut r = rig(true, Some(60), 1).await;
        let id = r.sessions[0].id;
        let held = r.sessions[0].held.clone();
        r.table.end_connection(ConnExit::ReadError).await;
        // Kept: what the peer says goes nowhere, and never waits.
        held.send(ice(id)).await.unwrap();
        settled(&held).await;
        let (claim, tx2, mut conn2) = reconnect(&mut r.table).await;
        assert_eq!(claim, vec![id]);
        assert!(r.table.resume(id, &tx2).await);
        assert_eq!(r.table.attached_len(), 1);
        assert!(!r.table.has_kept());
        held.send(ice(id)).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), conn2.recv())
            .await
            .expect("the resumed session's sender reaches the new connection");
        assert!(matches!(got, Some(ClientMsg::Ice { .. })), "{got:?}");
        assert!(
            conn2.try_recv().is_err(),
            "what it said while kept was dropped"
        );
        assert!(
            r.conn.try_recv().is_err(),
            "nothing reached the dead connection"
        );
        r.table.close_all("test_end").await;
        assert_eq!(r.closes(), 1);
    }

    /// The bug a single re-pointed `AgentPeer::outbound` would have: the state
    /// handler's clone, taken at creation, would still be the dead connection's.
    #[tokio::test]
    async fn a_resumed_peer_that_fails_sends_its_terminate_on_the_new_connection() {
        let mut r = rig(true, Some(60), 1).await;
        let id = r.sessions[0].id;
        let state_handler = r.sessions[0].held.clone();
        r.table.end_connection(ConnExit::ReadError).await;
        drop(r.conn);
        let (_, tx2, mut conn2) = reconnect(&mut r.table).await;
        assert!(r.table.resume(id, &tx2).await);
        state_handler
            .send(terminate(id, EndReason::Error, None))
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), conn2.recv())
            .await
            .expect("the Terminate reaches the new connection");
        match got {
            Some(ClientMsg::Terminate {
                session_id,
                reason: EndReason::Error,
                ..
            }) => assert_eq!(session_id, id),
            other => panic!("not the peer's Terminate: {other:?}"),
        }
        r.table.close_all("test_end").await;
    }

    #[tokio::test]
    async fn a_kept_peer_that_fails_is_closed_and_its_end_queued() {
        let mut r = rig(true, Some(60), 1).await;
        let id = r.sessions[0].id;
        r.table.end_connection(ConnExit::ReadError).await;
        r.sessions[0]
            .held
            .send(terminate(id, EndReason::Error, None))
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), r.table.next_event())
            .await
            .expect("the link hands the session's own end to the table");
        assert!(
            matches!(event, TableEvent::Ended(s, _) if s == id),
            "{event:?}"
        );
        r.table.serve(event).await;
        assert_eq!(r.closes(), 1, "the table closes it");
        let (claim, _tx2, mut conn2) = reconnect(&mut r.table).await;
        assert!(claim.is_empty(), "a closed session is not claimed");
        match conn2.try_recv() {
            Ok(ClientMsg::Terminate {
                session_id,
                reason: EndReason::Error,
                ..
            }) => assert_eq!(session_id, id),
            other => panic!("the next connection says its end: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_kept_peers_sends_never_wait_on_the_bounded_queue() {
        let ends = Ends::default();
        let mut table: SessionTable<Fake> = SessionTable::new(8, ends.hook());
        // A connection's queue with one slot, full, that nobody reads.
        let (tx, _conn) = mpsc::channel(1);
        tx.try_send(ice(ObjectId::new())).unwrap();
        table.begin_connection(true).await;
        table.connected(&tx);
        table.set_window(60);
        let s = attach(&mut table, &tx).await;
        let id = s.id;
        let held = s.held.clone();
        let sender = tokio::spawn(async move {
            for _ in 0..200 {
                held.send(ice(id)).await.unwrap();
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !sender.is_finished(),
            "attached, a full connection queue holds the session's sends back, as before"
        );
        table.end_connection(ConnExit::ReadError).await;
        tokio::time::timeout(Duration::from_secs(2), sender)
            .await
            .expect("kept, the sends that waited go through at once")
            .unwrap();
        let held = s.held.clone();
        tokio::time::timeout(Duration::from_secs(2), async move {
            for _ in 0..1000 {
                held.send(ice(id)).await.unwrap();
            }
        })
        .await
        .expect("and every later send never waits");
        table.close_all("test_end").await;
        assert_eq!(s.peer.closes(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn after_a_net_change_a_kept_peer_that_leaves_connected_closes_at_once() {
        let mut r = rig(true, Some(60), 2).await;
        // One peer already off its path when the arm runs.
        r.sessions[1].peer.set_connected(false);
        r.table.end_connection(ConnExit::NetChangePingFailed).await;
        assert_eq!(
            r.sessions[1].peer.closes(),
            1,
            "not Connected at the detach: closed at once"
        );
        assert_eq!(r.table.kept_ids(), vec![r.sessions[0].id]);
        // Still Connected: kept.
        let early = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = r.table.next_event().await;
                r.table.serve(event).await;
            }
        })
        .await;
        assert!(early.is_err());
        assert_eq!(r.sessions[0].peer.closes(), 0);
        r.sessions[0].peer.set_connected(false);
        let event = next(&mut r.table).await;
        assert!(matches!(event, TableEvent::Look), "{event:?}");
        r.table.serve(event).await;
        assert_eq!(
            r.sessions[0].peer.closes(),
            1,
            "left Connected: closed at once"
        );
        assert_eq!(r.table.len(), 0);
    }

    /// The connection's loop comes round on its other arms far more often than
    /// the look's period: the look still comes at its time.
    #[tokio::test(start_paused = true)]
    async fn a_busy_loop_does_not_push_the_look_back() {
        let mut r = rig(true, Some(60), 1).await;
        r.table.end_connection(ConnExit::NetChangeProbe).await;
        r.sessions[0].peer.set_connected(false);
        let mut looked = false;
        for _ in 0..5 {
            let turn = tokio::time::timeout(Duration::from_millis(100), r.table.next_event()).await;
            if let Ok(event) = turn {
                r.table.serve(event).await;
                looked = true;
                break;
            }
        }
        assert!(
            looked,
            "the look comes at its time, however often the loop turns"
        );
        assert_eq!(r.closes(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn after_another_arm_a_kept_peer_that_leaves_connected_waits_for_its_watchdog() {
        let mut r = rig(true, Some(60), 1).await;
        r.table.end_connection(ConnExit::ReadError).await;
        r.sessions[0].peer.set_connected(false);
        let early = tokio::time::timeout(Duration::from_secs(30), r.table.next_event()).await;
        assert!(
            early.is_err(),
            "a path the roll did not touch may come back"
        );
        assert_eq!(r.closes(), 0);
        r.table.close_all("test_end").await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_disconnect_in_the_gap_closes_and_queues_its_end() {
        let mut r = rig(true, Some(60), 1).await;
        let id = r.sessions[0].id;
        let peer = r.sessions[0].peer.clone();
        r.table.end_connection(ConnExit::ReadError).await;
        let said = terminate(id, EndReason::HostDisconnect, None);
        assert!(
            r.table
                .end_kept_by_device(id, said, "host_disconnect", Duration::from_secs(1))
        );
        assert!(peer.stopped(), "stops serving at once");
        assert_eq!(
            r.ends.all(),
            vec![(id, RealEnd::HostEnded)],
            "the recording stops, the banner comes down"
        );
        assert_eq!(peer.closes(), 0, "the close waits for the grace");
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(peer.closes(), 1, "closed after the grace");
        assert_eq!(
            peer.said(),
            vec![serde_json::json!({"t": "rc:ended", "reason": "host_disconnect"})],
            "the viewer hears it peer to peer"
        );
        assert_eq!(r.table.len(), 0);
        let (claim, _tx2, mut conn2) = reconnect(&mut r.table).await;
        assert!(claim.is_empty(), "an ended session is not claimed");
        match conn2.try_recv() {
            Ok(ClientMsg::Terminate {
                session_id,
                reason: EndReason::HostDisconnect,
                limit: None,
            }) => assert_eq!(session_id, id),
            other => panic!("the next connection says host_disconnect: {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_idle_limit_in_the_gap_ends_as_idle_timeout() {
        let mut r = rig(true, Some(60), 1).await;
        let id = r.sessions[0].id;
        let peer = r.sessions[0].peer.clone();
        r.table.end_connection(ConnExit::ReadError).await;
        let said = terminate(id, EndReason::IdleTimeout, Some("idle"));
        assert!(
            r.table
                .end_kept_by_device(id, said, "idle_timeout", Duration::ZERO)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(peer.closes(), 1);
        assert_eq!(
            peer.said(),
            vec![serde_json::json!({"t": "rc:ended", "reason": "idle_timeout", "limit": "idle"})]
        );
        let (_, _tx2, mut conn2) = reconnect(&mut r.table).await;
        assert!(matches!(
            conn2.try_recv(),
            Ok(ClientMsg::Terminate {
                reason: EndReason::IdleTimeout,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn an_end_at_the_device_for_a_session_not_kept_is_not_the_tables() {
        let mut r = rig(true, Some(60), 1).await;
        let said = terminate(r.sessions[0].id, EndReason::HostDisconnect, None);
        assert!(
            !r.table
                .end_kept_by_device(r.sessions[0].id, said, "host_disconnect", Duration::ZERO),
            "an attached session ends through the connection's own arm"
        );
        assert_eq!(r.closes(), 0);
        r.table.close_all("test_end").await;
    }

    #[tokio::test]
    async fn a_kept_session_ended_on_a_live_connection_says_so_at_once() {
        let mut r = rig(true, Some(60), 1).await;
        let id = r.sessions[0].id;
        r.table.end_connection(ConnExit::ReadError).await;
        let (_, _tx2, mut conn2) = reconnect(&mut r.table).await;
        let said = terminate(id, EndReason::HostDisconnect, None);
        assert!(
            r.table
                .end_kept_by_device(id, said, "host_disconnect", Duration::ZERO)
        );
        assert!(matches!(
            conn2.try_recv(),
            Ok(ClientMsg::Terminate {
                reason: EndReason::HostDisconnect,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_refused_token_closes_every_peer_kept_ones_included() {
        let mut r = rig(true, Some(60), 2).await;
        r.table.end_connection(ConnExit::ReadError).await;
        assert_eq!(r.table.kept_ids().len(), 2);
        // `run`'s AuthRejected arm.
        r.table.close_all("token_refused").await;
        assert_eq!(r.closes(), 2);
        assert_eq!(r.table.len(), 0);
        assert_eq!(r.ends.all().len(), 2);
    }

    #[tokio::test]
    async fn a_terminal_exit_closes_kept_sessions_too() {
        for exit in ConnExit::ALL
            .into_iter()
            .filter(|e| e.kind() == ExitKind::Terminal)
        {
            let mut r = rig(true, Some(60), 1).await;
            r.table.end_connection(ConnExit::ReadError).await;
            let (claim, tx2, _conn2) = reconnect(&mut r.table).await;
            assert_eq!(claim.len(), 1);
            r.table.set_window(60);
            let fresh = attach(&mut r.table, &tx2).await;
            r.table.end_connection(exit).await;
            assert_eq!(r.closes(), 1, "{exit:?} closes the kept session");
            assert_eq!(fresh.peer.closes(), 1, "{exit:?} closes the attached one");
            assert_eq!(r.table.len(), 0);
        }
    }

    #[tokio::test]
    async fn the_switch_off_at_the_next_connection_closes_what_was_kept() {
        let mut r = rig(true, Some(60), 1).await;
        r.table.end_connection(ConnExit::ReadError).await;
        let claim = r.table.begin_connection(false).await;
        assert!(claim.is_empty(), "nothing is claimed without the verb");
        assert_eq!(r.closes(), 1);
    }

    #[tokio::test]
    async fn kept_sessions_count_against_the_capacity() {
        let mut r = rig_with(2, true, Some(60), 2).await;
        r.table.end_connection(ConnExit::ReadError).await;
        let (claim, tx2, _conn2) = reconnect(&mut r.table).await;
        assert_eq!(claim.len(), 2);
        assert!(r.table.full_beside_kept(), "a third is refused as busy");
        assert!(r.table.resume(claim[0], &tx2).await);
        assert!(
            r.table.full_beside_kept(),
            "one kept beside one resumed: still full"
        );
        assert!(r.table.resume(claim[1], &tx2).await);
        assert!(
            !r.table.full_beside_kept(),
            "nothing kept: the server's gate decides, as before"
        );
        r.table.close_all("test_end").await;
    }

    #[tokio::test]
    async fn the_table_never_keeps_more_than_the_capacity() {
        let mut r = rig_with(1, true, Some(60), 2).await;
        r.table.end_connection(ConnExit::ReadError).await;
        assert_eq!(r.table.kept_ids().len(), 1);
        assert_eq!(r.closes(), 1, "the one over the capacity closes");
        r.table.close_all("test_end").await;
        assert_eq!(r.closes(), 2);
    }

    #[tokio::test]
    async fn a_terminate_for_a_kept_session_takes_it_out() {
        let mut r = rig(true, Some(60), 1).await;
        let id = r.sessions[0].id;
        r.table.end_connection(ConnExit::ReadError).await;
        // The `Terminate` arm, for a claim refused: takes it out and closes it
        // within the budget, as it always closed an attached one.
        let peer = r.table.take(&id, "terminate").expect("kept");
        close_within_budget(peer.close(), id, "terminate").await;
        assert_eq!(r.closes(), 1);
        assert!(!r.table.holds(&id));
    }

    #[tokio::test]
    async fn a_table_dropped_holding_peers_still_closes_them() {
        let mut r = rig(true, Some(60), 2).await;
        r.table.end_connection(ConnExit::ReadError).await;
        let tx = r.tx.clone();
        let fresh = attach(&mut r.table, &tx).await;
        let Rig {
            table, sessions, ..
        } = r;
        drop(table);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(sessions.iter().map(|s| s.peer.closes()).sum::<usize>(), 2);
        assert_eq!(fresh.peer.closes(), 1);
    }

    #[test]
    fn the_resume_word_follows_the_switch_by_equality() {
        let mut rpc = vec!["rc-overlay-auto".to_string()];
        apply_resume_word(&mut rpc, true);
        apply_resume_word(&mut rpc, true);
        assert_eq!(
            rpc,
            ["rc-overlay-auto", "rc-resume"],
            "once, beside the others"
        );
        apply_resume_word(&mut rpc, false);
        assert_eq!(rpc, ["rc-overlay-auto"], "off says nothing");
    }

    #[test]
    fn rc_ended_carries_the_terminates_reason_and_limit() {
        let id = ObjectId::new();
        let text = ended_text(&terminate(id, EndReason::IdleTimeout, Some("idle"))).unwrap();
        let said: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            said,
            serde_json::json!({"t": "rc:ended", "reason": "idle_timeout", "limit": "idle"})
        );
        assert_eq!(ended_text(&ice(id)), None);
    }
}
