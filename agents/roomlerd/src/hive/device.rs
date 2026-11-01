// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2d-0 — the device's own parts, and the seams they reach the
//! daemon through (the FR-90 spec, §3i "P8-2d-0, as built").
//!
//! Four parts of Hive serve the DEVICE, not one session: the replica store,
//! the replica stream with its follow (`supervisor::stream`,
//! `supervisor::follow`), the viewer's peer (`view`), and the knowhow
//! prober's measure (`knowhow_probe`). P8-2d moves them into one worker for
//! the device (decision 63), in the order decision 70 sets: the prober's
//! measure, the viewer's peer, the stream, the store. This module is the
//! boundary they move across, built first, with nothing moved yet: every part
//! still runs in the daemon, behind it.
//!
//! | Seam | Direction | Carries | The daemon's, today |
//! |---|---|---|---|
//! | [`ControlLink`] | the parts → the daemon | the frames the viewer's peer and the stream speak themselves, and no other ([`speaks`], decision 69) | the control connection's queue |
//! | [`DeviceSeams`] | the parts → the daemon | what they ask of what the daemon holds or decides: run state, a driver's prompt and answer, a teleport's place, a pack made as the session's account, the DERP mux | the supervisor's own methods |
//! | [`Measure`] | the daemon → the parts | a knowhow probe's measure, a method and never a frame (decision 69) | [`InDaemon`]: the probe, in this process |
//! | [`Device`]'s methods | the daemon → the parts | the frames the daemon takes for them, once its own gates passed | the same code as before, moved off the supervisor |
//!
//! The split is every earlier P8-2 step's: the parts take what parses input
//! from someone else (another device's QUIC, a browser's DTLS and SCTP, a
//! probed host's TLS, the store's SQLite); the daemon keeps what holds or
//! decides (the grant gates, run state, the control connection, config and
//! its lock, the DERP mux, every spawn as a session's account).
//!
//! ⚠️ **Nothing a seam carries is a closure, and one thing is a handle.** A
//! [`Conn`] is its number to a worker; in process it also keeps its
//! connection open, as the frame's own sender did (its note). The handle is
//! [`DeviceSeams::derp_handle`]'s: the DERP mux itself, which the stream rides
//! until P8-2d-3 carries its datagrams across the channel instead, tagged by
//! grant (decision 75).
//!
//! ⚠️ **The daemon makes a [`Device`] for each frame it hands over**
//! ([`super::supervisor::Supervisor::device`]), its seams the supervisor
//! itself: a part's task then holds the supervisor as it did when the code
//! ran on it, and nothing holds a cycle. A worker will make one for its life.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bson::oid::ObjectId;
use roomler_ai_remote_control::hive::HiveRunState;
use roomler_ai_remote_control::models::DestinationRule;
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_hive_node::checkpoint::WorkspaceSnap;
#[cfg(worker_calls)]
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, broadcast, mpsc};
use tunnel_core::transport::derp::DerpTunnelHandle;

use super::knowhow_probe::{Asked, Probed};
use super::store::StoreHandle;
use super::supervisor::{Author, StartOrder, follow, stream};
use super::toolbelt::Decision;
use super::view::Viewers;

/// A primary connection, as the device's parts name the one a frame came on:
/// the number the daemon gave it, which is all of it a worker will hold
/// (P8-2d-3). The daemon alone holds the connection, and answers on it by
/// that number. In process, the connection's queue rides along besides, so
/// whoever holds a `Conn` holds the connection open until it answers or ends,
/// as a part held the frame's own sender before P8-2d-0; across a worker's
/// channel, the daemon's signalling loop is what holds it open.
#[derive(Debug, Clone)]
pub(crate) struct Conn {
    n: u64,
    _held: Option<mpsc::Sender<ClientMsg>>,
}

impl Conn {
    /// The daemon's: connection `n`, its queue `held` open by whoever holds
    /// this.
    pub(crate) fn held(n: u64, held: mpsc::Sender<ClientMsg>) -> Self {
        Self {
            n,
            _held: Some(held),
        }
    }

    /// As a worker will name a connection: its number alone.
    #[cfg(test)]
    pub(crate) fn numbered(n: u64) -> Self {
        Self { n, _held: None }
    }

    /// Its number.
    pub(crate) fn number(&self) -> u64 {
        self.n
    }
}

/// Why a frame the device's parts speak did not go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unsent {
    /// The connection is gone: the one named closed, or none is up.
    Gone,
    /// Its queue is full, or it is closing: [`ControlLink::send`] never
    /// waits.
    Full,
    /// Not a frame the device's parts speak (decision 69): never sent.
    NotTheirs,
}

/// FR-90 P8-2d-0 — whether `msg` is one the device's parts speak themselves:
/// what [`ControlLink`] carries, and all it carries (decision 69). The
/// viewer's peer answers a browser's offer, trickles its candidates, and
/// says why it closed; the stream answers a grant, sends its relayed
/// address, asks for a stream, says where its copy ends, and ends one. Every
/// other frame is the daemon's to speak: a narrow link is one a worker cannot
/// speak the daemon's words on.
pub(crate) fn speaks(msg: &ClientMsg) -> bool {
    matches!(
        msg,
        ClientMsg::HiveViewAnswer { .. }
            | ClientMsg::HiveViewIce { .. }
            | ClientMsg::HiveViewClosed { .. }
            | ClientMsg::HiveReplicaReady { .. }
            | ClientMsg::HiveReplicaCandidate { .. }
            | ClientMsg::HiveReplicaClose { .. }
            | ClientMsg::HiveReplicaWant { .. }
            | ClientMsg::HiveReplicaTip { .. }
    )
}

/// FR-90 P8-2d-0 — the frames the device's parts speak themselves, on the
/// daemon's control connection: narrow (decision 69), [`speaks`] alone. The
/// knowhow prober speaks none here: it answers as a method ([`Measure`]).
#[async_trait::async_trait]
pub(crate) trait ControlLink: Send + Sync {
    /// `msg` on the connection `on`, the one that delivered the frame it
    /// answers, waiting while its queue is full. [`Unsent::Gone`] once that
    /// connection has closed.
    async fn reply(&self, on: Conn, msg: ClientMsg) -> Result<(), Unsent>;
    /// `msg` on the primary connection that is up now, waiting while its
    /// queue is full. [`Unsent::Gone`] when none is.
    async fn deliver(&self, msg: ClientMsg) -> Result<(), Unsent>;
    /// `msg` on the primary connection that is up now, never waiting:
    /// dropped when its queue is full or none is up, and never replayed.
    fn send(&self, msg: ClientMsg) -> Result<(), Unsent>;
}

/// FR-90 P8-2d-0 — what the device's parts ask of what the daemon holds or
/// decides, each a method. Data in, data out; the one handle is the DERP
/// mux's (this module's note).
#[async_trait::async_trait]
pub(crate) trait DeviceSeams: Send + Sync {
    // ─── What runs here: the viewer's peer, and a source's hello ───────────

    /// Whether this device runs `session` now.
    fn holds_live(&self, session: ObjectId) -> bool;
    /// P2d-3b — whether `session` is not over here: it runs here, its hosted
    /// record will resume it, or (P2e-5c) it moves here.
    fn session_live(&self, session: ObjectId) -> bool;
    /// The latest state reported for `session`, while it is remembered.
    fn run_state(&self, session: ObjectId) -> Option<HiveRunState>;
    /// The approvals open in `session` now.
    fn pending_approvals(&self, session: ObjectId) -> Vec<String>;

    // ─── What a driver does over the viewer's peer ──────────────────────────

    /// A driver's prompt to a session that runs here.
    fn prompt(&self, session: ObjectId, author: Option<Author>, text: String)
    -> Result<(), String>;
    /// A driver's answer to one of `session`'s open approvals.
    fn answer_approval(
        &self,
        session: ObjectId,
        approval: &str,
        decision: Decision,
        by: Author,
    ) -> Result<(), String>;

    // ─── What a member asks: a teleport's place and its repository ──────────

    /// P2f-1a — whether a teleport's prepare passed here for `session`,
    /// naming the start after `fence`.
    fn teleport_admitted(&self, session: ObjectId, fence: u64) -> bool;
    /// P2f-2b — where a teleport this device was prepared for runs `session`,
    /// when the copy's workspace may come against its folder's repository.
    fn have_want_place(&self, session: ObjectId) -> Option<(StartOrder, String, PathBuf)>;
    /// P2f-2b — what that folder's repository holds that a pack can be made
    /// against, asked of the account's own git, as the account.
    async fn haves_for(
        &self,
        order: &StartOrder,
        account: &str,
        folder: &Path,
        ws: &WorkspaceSnap,
    ) -> Option<Vec<String>>;

    // ─── What a source asks: a pack, made as the session's account ─────────

    /// P2f-2b — the pack that brings `commit` into a repository holding
    /// `haves`, made from where `session` runs here (or, P2f-2c, was last
    /// worked on), as its account: what it is against, and its bytes. The
    /// `no_pack` word when it cannot be made.
    async fn pack_for_member(
        &self,
        session: ObjectId,
        sid: &str,
        commit: &str,
        haves: &[String],
    ) -> Result<(Vec<String>, Vec<u8>), &'static str>;

    // ─── The DERP mux ───────────────────────────────────────────────────────

    /// This device's DERP mux, the primary organization's, while the overlay
    /// is up. ⚠️ The one handle a seam carries: decision 75 replaces it at
    /// P8-2d-3 with datagrams that cross the channel tagged by grant.
    fn derp_handle(&self) -> Option<DerpTunnelHandle>;
}

/// FR-90 P8-2d-0 — a knowhow probe's measure, as the daemon's prober asks it
/// of the device's parts (decision 69: a method, never a frame). The daemon
/// keeps the prober itself: the probe asked, the connection that hears its
/// answer, the answers it gave, and the list it read under the config's lock.
/// The parts resolve, connect and shake hands within that list, and return
/// what they saw. P8-2d-1 measures in the device's worker.
#[async_trait::async_trait]
pub(crate) trait Measure: Send + Sync {
    /// `asked`, within `rules`: what the probe saw.
    async fn measure(&self, asked: Asked, rules: Vec<DestinationRule>) -> Probed;
}

/// The device's parts, measuring in the daemon's own process: every probe,
/// until P8-2d-1 measures in the device's worker where the owner turned
/// workers on.
pub(crate) struct InDaemon;

#[async_trait::async_trait]
impl Measure for InDaemon {
    async fn measure(&self, asked: Asked, rules: Vec<DestinationRule>) -> Probed {
        super::knowhow_probe::probe(&asked, &rules).await
    }
}

/// FR-90 P8-2d-0 — what the device's parts hold: one for the daemon, the
/// supervisor's. Each part's moves into the device's worker as the part does
/// (decision 70's order).
pub(crate) struct Parts {
    /// The replica store. The daemon holds a handle to the same one for its
    /// own calls until the store moves, last (P8-2d-4).
    pub(super) store: Result<StoreHandle, String>,
    /// P2d — the replica streams this device is a side of, by grant: the
    /// ones it serves as a source and the ones it follows as a member.
    pub(super) streams: Mutex<HashMap<ObjectId, stream::Stream>>,
    /// P2d-3 — what the streams this device follows hold in memory, across
    /// all of them: event bytes, and blob bytes, each taken before it is
    /// read ([`stream::EVENT_BUDGET`], [`stream::BLOB_BUDGET`]).
    pub(super) replica_events: Semaphore,
    pub(super) replica_blobs: Semaphore,
    /// P2d-3b — what this device does about each session it holds a copy of
    /// as a member: the ask in flight, the stream followed, the wait.
    pub(super) follows: Mutex<follow::Follows>,
    /// The viewer peers this device serves (P0d-2).
    pub(super) viewers: Viewers,
    /// Every state as it is reported, for the viewers of that session: the
    /// daemon's word, pushed here ([`Self::run_state_changed`]).
    states: broadcast::Sender<(ObjectId, HiveRunState)>,
    /// P1a — a session's open approvals each time they change, for its
    /// viewers: a driver's card shows its buttons only while it is open.
    approvals: broadcast::Sender<(ObjectId, Vec<String>)>,
}

impl Parts {
    pub(crate) fn new(store: Result<StoreHandle, String>) -> Self {
        Self {
            store,
            streams: Mutex::new(HashMap::new()),
            replica_events: Semaphore::new(stream::EVENT_BUDGET),
            replica_blobs: Semaphore::new(stream::BLOB_BUDGET),
            follows: Mutex::new(follow::Follows::default()),
            viewers: Viewers::default(),
            states: broadcast::channel(64).0,
            approvals: broadcast::channel(64).0,
        }
    }

    /// The daemon: `session`'s run reported `state`. Its viewers hear it;
    /// nobody watching is fine.
    pub(crate) fn run_state_changed(&self, session: ObjectId, state: HiveRunState) {
        let _ = self.states.send((session, state));
    }

    /// The daemon: `session`'s open approvals are `open` now. Its viewers
    /// hear it; nobody watching is fine.
    pub(crate) fn approvals_changed(&self, session: ObjectId, open: Vec<String>) {
        let _ = self.approvals.send((session, open));
    }

    /// Every state reported from now on, of every session.
    pub(crate) fn subscribe_states(&self) -> broadcast::Receiver<(ObjectId, HiveRunState)> {
        self.states.subscribe()
    }

    /// P1a — every change to any session's open approvals, from now on.
    pub(crate) fn subscribe_approvals(&self) -> broadcast::Receiver<(ObjectId, Vec<String>)> {
        self.approvals.subscribe()
    }
}

/// FR-90 P8-2d-0 — the device's parts as their code runs on them: what they
/// hold, and the only ways they reach the daemon, [`ControlLink`] and
/// [`DeviceSeams`].
pub(crate) struct Device {
    parts: Arc<Parts>,
    pub(super) link: Arc<dyn ControlLink>,
    pub(super) seams: Arc<dyn DeviceSeams>,
}

impl Device {
    pub(crate) fn new(
        parts: Arc<Parts>,
        link: Arc<dyn ControlLink>,
        seams: Arc<dyn DeviceSeams>,
    ) -> Self {
        Self { parts, link, seams }
    }
}

/// What the parts hold, read as the device's own: their code is the code it
/// was on the supervisor, and only what crosses to the daemon changed.
impl std::ops::Deref for Device {
    type Target = Parts;

    fn deref(&self) -> &Parts {
        &self.parts
    }
}

/// FR-90 P8-2d-0 — what the daemon asks the device's worker, which P8-2d-1
/// starts. A type of its own, not a session worker's
/// [`Call`](super::worker::Call): a link carries one role's calls
/// ([`super::worker::link::DeviceLink`]), so neither is handed to the
/// other's worker.
#[cfg(worker_calls)]
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "d", rename_all = "snake_case")]
pub(crate) enum DeviceCall {
    /// A knowhow probe's measure ([`Measure`]), within the rules the daemon
    /// read under its config's lock.
    Measure {
        asked: Asked,
        rules: Vec<DestinationRule>,
    },
}

/// The answer to a [`DeviceCall`].
#[cfg(worker_calls)]
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub(crate) enum DeviceAnswer {
    /// What the probe saw. The daemon's prober speaks the frame
    /// (`rc:knowhow.probed`), its detail held to the wire's bound there.
    Probed { probed: Probed },
    /// Not done, and why.
    Refused { why: String },
}

#[cfg(test)]
mod tests;
