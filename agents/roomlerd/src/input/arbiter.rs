// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! P6 (multi-org program) — the InputArbiter: ONE injection worker for all
//! concurrent remote-control sessions.
//!
//! Pre-P6 every session ran its own `InputInjector` on its own tokio task:
//! two typing users interleaved raw `SendInput` streams on the host's ONE
//! modifier plane (A holding Shift turned B's clicks into Shift-clicks),
//! and nothing released a session's held keys when it died mid-chord. The
//! server compensated with the P3 single-INPUT-holder rule.
//!
//! The arbiter replaces that with TeamViewer-style **free-for-all +
//! fencing** (the program's decided default):
//!
//!   * every event flows through one worker thread that owns the single
//!     OS injector — per-session streams are serialized, never interleaved;
//!   * per-session held state (keys + mouse buttons) is tracked from the
//!     event stream; **switching injecting sessions fences modifiers**: the
//!     previous sessions' held modifier keys (HID `0xe0..=0xe7`) are
//!     released before the new session's event injects, so one user's held
//!     Ctrl can never contaminate another's click (the cross-session
//!     generalisation of the #306 text-vs-modifier transition planner);
//!   * **release-all on teardown**: a session's every held key/button is
//!     released when it closes (browser crash, tab kill, watchdog) — the
//!     precise superset of the 2026-08-04 blanket modifier release;
//!   * an **exclusive** mode (single INPUT holder with explicit floor
//!     control) selectable per device via `AccessPolicy.input_mode` and
//!     in-session via the control DC. Floor requests auto-transfer when
//!     the current holder has been input-idle ≥ [`IDLE_TAKEOVER`].
//!
//! The worker also owns the multi-user UX fan-out:
//!   * `rc:control.state` on every session's control DC whenever
//!     membership / mode / holder changes (the viewer's participants rail
//!     + request-control UX), and
//!   * **ghost cursors**: each session's pointer position is rebroadcast
//!     (throttled to ~[`GHOST_MIN_INTERVAL`]) to every OTHER session's
//!     cursor DC as `cursor:peer`, name-tagged.
//!
//! Split for testability: [`ArbiterState`] is a pure state machine
//! (returns *plans* — what to inject / whether to broadcast) with zero
//! DC / OS / tokio types, unit-tested on the default build; the worker
//! layer executes plans against the real injector and the DC stashes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use webrtc::data_channel::RTCDataChannel;

use super::{Button, InputMsg};

/// HID usage codes for the 8 modifier keys (L/R Ctrl, Shift, Alt, Meta) —
/// the set the cross-session fence releases.
pub const MODIFIER_CODES: std::ops::RangeInclusive<u32> = 0xe0..=0xe7;

/// #3046 — the bits of `InputMsg::Key.mods` with the two keys that answer each
/// (left, right), in the order a chord presses them.
const MOD_BITS: [(u8, u32, u32); 4] = [
    (0x01, 0xe0, 0xe4), // Ctrl
    (0x04, 0xe2, 0xe6), // Alt
    (0x02, 0xe1, 0xe5), // Shift
    (0x08, 0xe3, 0xe7), // Meta
];

/// #3046 — the modifiers `mods` names for a key press that this session does
/// not hold: the left key of each, in press order. Empty for nearly every
/// frame, because a physical keyboard's modifiers arrive as frames of their own
/// and stay held. `mods` alone carries the soft keyboard's sticky Ctrl, a
/// controller's one-key chord, and a Ctrl+V flushed after its Ctrl came up.
///
/// `ctrl_may_be_cmd`: a viewer turns Ctrl into Cmd for a Mac host in the key's
/// code and not in `mods` (FR-13), so there a held Cmd answers the Ctrl bit.
/// Without it every Ctrl+C from a viewer would press a second, literal Ctrl.
fn chord_tap(code: u32, mods: u8, held: &HashSet<Held>, ctrl_may_be_cmd: bool) -> Vec<u32> {
    if MODIFIER_CODES.contains(&code) {
        return Vec::new();
    }
    let holds = |key: u32| held.contains(&Held::Key(key));
    MOD_BITS
        .iter()
        .filter(|(bit, left, right)| {
            mods & bit != 0
                && !holds(*left)
                && !holds(*right)
                && !(ctrl_may_be_cmd && *bit == 0x01 && (holds(0xe3) || holds(0xe7)))
        })
        .map(|(_, left, _)| *left)
        .collect()
}

/// A floor request against a holder who has produced no input for this
/// long is auto-granted (walk-away takeover); an ACTIVE holder keeps the
/// floor and the requester is told who has it.
pub const IDLE_TAKEOVER: Duration = Duration::from_secs(2);

/// Per-source ghost-cursor rebroadcast floor (~30 Hz).
pub const GHOST_MIN_INTERVAL: Duration = Duration::from_millis(33);

/// Field diagnostic — ghost-cursor payloads handed to the DC layer since
/// process start (see the log in `ghost_broadcast`).
static GHOSTS_SENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Input arbitration mode. `Free` (the program's decided default) lets
/// every INPUT-granted session inject, serialized + modifier-fenced;
/// `Exclusive` funnels injection through one floor holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Free,
    Exclusive,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "free" => Some(Mode::Free),
            "exclusive" => Some(Mode::Exclusive),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Free => "free",
            Mode::Exclusive => "exclusive",
        }
    }
}

/// One held thing — a key (HID code) or a mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Held {
    Key(u32),
    Btn(Button),
}

impl Held {
    fn release_msg(&self) -> InputMsg {
        match *self {
            Held::Key(code) => InputMsg::Key {
                code,
                down: false,
                mods: 0,
            },
            // #1945 — never a `MouseButton` up: that carries a position, and
            // every backend moves there first. The (0,0) this used to send
            // was GNOME's Activities hot corner, and the end of the drag.
            Held::Btn(btn) => InputMsg::MouseRelease { btn },
        }
    }
    fn is_modifier(&self) -> bool {
        matches!(*self, Held::Key(code) if MODIFIER_CODES.contains(&code))
    }
}

struct SessCore {
    name: String,
    /// The session's INPUT grant (rail display + a defense-in-depth inject
    /// gate behind the per-DC enforcement).
    can_input: bool,
    held: HashSet<Held>,
    last_input: Instant,
    /// #3918 — marked as having no picture of this device's virtual desktop,
    /// at a command where its input would reach the desktop's X server
    /// ([`ArbiterState::leave_behind`]): a session without input while that
    /// lasts, whatever its grant. The rail still shows the grant.
    left_behind: bool,
}

impl SessCore {
    /// Whether this session may act on the host: its INPUT grant, unless it
    /// was left behind by the desktop's X server (#3918).
    fn may_input(&self) -> bool {
        self.can_input && !self.left_behind
    }
}

/// FR-92 — why a session may not switch keep-busy on or off. (An org's
/// deny is not here: it is device-wide — the strictest of every enrolled
/// org — and the engine itself refuses, with `org_denied`, whoever asks.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepBusyRefusal {
    /// Not a registered session (already closed).
    Unknown,
    /// The session holds no INPUT grant — keep-busy is input.
    NoInput,
    /// Exclusive mode, and another session holds the floor.
    NotFloorHolder,
}

impl KeepBusyRefusal {
    pub fn wire(self) -> &'static str {
        match self {
            KeepBusyRefusal::Unknown => "unknown_session",
            KeepBusyRefusal::NoInput => "no_input_permission",
            KeepBusyRefusal::NotFloorHolder => "not_floor_holder",
        }
    }
}

/// What the worker should do with one event.
#[derive(Debug, PartialEq)]
pub enum EventPlan {
    /// Floor-denied (exclusive mode, not the holder) or no INPUT grant.
    Deny,
    /// Inject `pre` (fence releases, then a chord's modifiers), the event
    /// itself, then `post` (the release of a chord tapped whole, #3046).
    Inject {
        pre: Vec<InputMsg>,
        post: Vec<InputMsg>,
    },
}

/// One event as the worker acts on it: its [`EventPlan`], and whose input
/// each of the fence's releases is.
#[derive(Debug, PartialEq)]
pub struct Planned {
    pub plan: EventPlan,
    /// #3918 — whose held modifier each of the fence's releases
    /// lets go: the releases come first in `pre`, one owner each, in order.
    /// The worker offers each under its owner, never under the session whose
    /// event it is.
    pub fenced: Vec<ObjectId>,
    /// #3918 — the event moved the floor: in exclusive mode a free
    /// floor goes to the first session to act. The worker tells the viewers
    /// in this command, as every other command that moves the floor does.
    pub floor_moved: bool,
}

/// Pure decision core. All methods take `now` so tests inject time.
pub struct ArbiterState {
    mode: Mode,
    /// Exclusive-mode floor holder.
    holder: Option<ObjectId>,
    /// Which session injected last — the fence trigger edge.
    last_injector: Option<ObjectId>,
    /// Whether the device-policy mode hint was already applied (first
    /// session only; later joins must not stomp an in-session toggle).
    ///
    /// FR-27 — CLEARED when the last session leaves. Before that it was set
    /// once for the life of the daemon, so an in-session toggle outlived every
    /// session that could have justified it: set a device to `exclusive`, let
    /// one viewer flip it to `free`, let everyone disconnect, and the next
    /// session still came up `free` — the device policy silently stopped
    /// applying until the daemon restarted. "Later joins must not stomp a
    /// toggle" is about a LIVE conversation between concurrent viewers; with
    /// nobody left there is no conversation to preserve.
    mode_seeded: bool,
    /// FR-27 — an outstanding exclusive-mode floor request, when the holder
    /// was active and the request could not be auto-granted.
    ///
    /// `request_floor` used to just return `false` and drop it: the holder
    /// never learned anyone had asked, and the requester saw nothing at all,
    /// so "Request control" looked broken unless you happened to click it
    /// during the holder's idle window. Carried in the snapshot so both ends
    /// can render it.
    pending_request: Option<ObjectId>,
    sessions: HashMap<ObjectId, SessCore>,
}

/// Broadcast-ready snapshot of the arbiter state.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub mode: Mode,
    pub holder: Option<ObjectId>,
    pub participants: Vec<Participant>,
    /// FR-27 — who is waiting for the floor, if anyone. `None` in free mode
    /// and whenever nothing is outstanding.
    pub pending_request: Option<Participant>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Participant {
    pub session: ObjectId,
    pub name: String,
    pub input: bool,
}

impl Default for ArbiterState {
    fn default() -> Self {
        Self {
            mode: Mode::Free,
            holder: None,
            last_injector: None,
            mode_seeded: false,
            pending_request: None,
            sessions: HashMap::new(),
        }
    }
}

impl ArbiterState {
    /// Register a session. `mode_hint` = the device's `AccessPolicy.input_mode`
    /// (server-resolved) — applied only while no session has toggled the mode.
    /// In exclusive mode the first INPUT-capable session becomes the holder.
    pub fn open(
        &mut self,
        session: ObjectId,
        name: String,
        can_input: bool,
        mode_hint: Option<Mode>,
        now: Instant,
    ) {
        if !self.mode_seeded {
            if let Some(m) = mode_hint {
                self.mode = m;
            }
            self.mode_seeded = true;
        }
        self.sessions.insert(
            session,
            SessCore {
                name,
                can_input,
                held: HashSet::new(),
                last_input: now,
                left_behind: false,
            },
        );
        if self.mode == Mode::Exclusive && self.holder.is_none() && can_input {
            self.holder = Some(session);
        }
    }

    /// FR-93 P4c — whether `session` is registered with an input grant.
    pub fn can_input(&self, session: ObjectId) -> bool {
        self.sessions.get(&session).is_some_and(SessCore::may_input)
    }

    /// #4069 — whether any registered session may input, as [`Self::can_input`]
    /// asks of one: an automatic capture's live edge warms for those only.
    pub fn any_can_input(&self) -> bool {
        self.sessions.values().any(SessCore::may_input)
    }

    /// Deregister a session. Returns the key/button releases to inject
    /// (its whole held set). Reassigns the exclusive holder if it left.
    pub fn close(&mut self, session: ObjectId) -> Vec<InputMsg> {
        let Some(core) = self.sessions.remove(&session) else {
            return Vec::new();
        };
        if self.last_injector == Some(session) {
            self.last_injector = None;
        }
        if self.pending_request == Some(session) {
            self.pending_request = None;
        }
        if self.holder == Some(session) {
            // Hand the floor to a remaining INPUT-capable session — the
            // LOWEST session id, not whatever `HashMap` iteration happened to
            // yield first. Two viewers watching the same handover should see
            // the same outcome, and a nondeterministic one is untestable.
            self.holder = self
                .sessions
                .iter()
                .filter(|(_, c)| c.may_input())
                .map(|(id, _)| *id)
                .min();
        }
        // FR-27 — with the last session gone there is no in-session decision
        // left to preserve, so let the device policy seed the next one. See
        // the field on `mode_seeded`.
        if self.sessions.is_empty() {
            self.mode_seeded = false;
            self.pending_request = None;
        }
        core.held.iter().map(Held::release_msg).collect()
    }

    /// Release everything one session holds WITHOUT deregistering it — the
    /// input DC died (browser hiccup) but the session may live on.
    pub fn release_held(&mut self, session: ObjectId) -> Vec<InputMsg> {
        self.sessions
            .get_mut(&session)
            .map(|c| {
                let r: Vec<InputMsg> = c.held.iter().map(Held::release_msg).collect();
                c.held.clear();
                r
            })
            .unwrap_or_default()
    }

    /// #3918 — `session` is marked as having no picture of this device's
    /// virtual desktop, and its input would reach the desktop's X server
    /// (`x_follow::Sessions`). What it holds is forgotten, releasing nothing:
    /// its presses went with the X server they were made on. Until
    /// `take_back` it is a session without input: its events, its floor
    /// requests, grants to it, its mode, keep busy and layout switches are
    /// refused as for a session without an INPUT grant. The floor it holds,
    /// and a request it made, are given up and not given back, so the person
    /// who reconnects does not wait out an idle takeover from their own dead
    /// session: in exclusive mode the next session to act takes the floor.
    /// `true` when the floor or the request moved.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    pub fn leave_behind(&mut self, session: ObjectId) -> bool {
        let Some(c) = self.sessions.get_mut(&session) else {
            return false;
        };
        c.held.clear();
        c.left_behind = true;
        let mut moved = false;
        if self.holder == Some(session) {
            self.holder = None;
            moved = true;
        }
        if self.pending_request == Some(session) {
            self.pending_request = None;
            moved = true;
        }
        moved
    }

    /// #3918 — whether [`Self::leave_behind`] left `session` behind.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    pub fn is_left_behind(&self, session: ObjectId) -> bool {
        self.sessions.get(&session).is_some_and(|c| c.left_behind)
    }

    /// #3918 — `session`, marked, is left behind no longer: its input goes
    /// elsewhere than the desktop's X server now (a portal or mutter route
    /// takes it, or another injector), so it acts as before. What it gave up
    /// when it was left behind (what it held, the floor, a request) stays
    /// given up.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    pub fn take_back(&mut self, session: ObjectId) {
        if let Some(c) = self.sessions.get_mut(&session) {
            c.left_behind = false;
        }
    }

    /// Decide one event. Tracks held state, applies the exclusive floor,
    /// and plans cross-session modifier fencing on injector switch.
    pub fn plan(&mut self, session: ObjectId, msg: &InputMsg, now: Instant) -> EventPlan {
        self.plan_event(session, msg, now).plan
    }

    /// [`Self::plan`], with whose input each fence release is.
    pub fn plan_event(&mut self, session: ObjectId, msg: &InputMsg, now: Instant) -> Planned {
        let holder = self.holder;
        let mut fenced = Vec::new();
        let plan = self.plan_fenced(session, msg, now, &mut fenced);
        Planned {
            plan,
            fenced,
            floor_moved: self.holder != holder,
        }
    }

    fn plan_fenced(
        &mut self,
        session: ObjectId,
        msg: &InputMsg,
        now: Instant,
        fenced: &mut Vec<ObjectId>,
    ) -> EventPlan {
        // Heartbeats carry no input; pass them through un-fenced so their
        // timing diagnostics stay honest.
        if matches!(msg, InputMsg::Heartbeat { .. }) {
            return EventPlan::Inject {
                pre: Vec::new(),
                post: Vec::new(),
            };
        }
        let can_input = self.sessions.get(&session).is_some_and(SessCore::may_input);
        if !can_input {
            return EventPlan::Deny;
        }
        if self.mode == Mode::Exclusive {
            if self.holder.is_none() {
                // No holder (e.g. a holder left behind gave it up, #3918) —
                // first event wins, and the worker tells the viewers.
                self.holder = Some(session);
            }
            if self.holder != Some(session) {
                return EventPlan::Deny;
            }
        }

        // Fence: the injecting session changed — release every OTHER
        // session's held MODIFIERS so their chords can't contaminate this
        // event (their own eventual key-ups become OS-level no-ops).
        let mut pre = Vec::new();
        if self.last_injector != Some(session) {
            for (id, core) in self.sessions.iter_mut() {
                if *id == session {
                    continue;
                }
                let mods: Vec<Held> = core
                    .held
                    .iter()
                    .filter(|h| h.is_modifier())
                    .copied()
                    .collect();
                for m in mods {
                    core.held.remove(&m);
                    pre.push(m.release_msg());
                    fenced.push(*id);
                }
            }
            self.last_injector = Some(session);
        }

        // Held-state tracking for THIS session.
        let mut post = Vec::new();
        if let Some(core) = self.sessions.get_mut(&session) {
            core.last_input = now;
            match msg {
                InputMsg::Key { code, down, mods } => {
                    let chord = if *down {
                        chord_tap(*code, *mods, &core.held, cfg!(target_os = "macos"))
                    } else {
                        Vec::new()
                    };
                    if !chord.is_empty() {
                        // #3046 — a chord with no modifier frame of its own is
                        // one press and one release, on this frame. The input
                        // channel is unreliable: a later frame that has to let
                        // go of the modifier could be lost, and a stuck Ctrl on
                        // the host is worse than a bare letter. Nothing here is
                        // left down or tracked, so the viewer's up frame finds
                        // nothing to release.
                        core.held.remove(&Held::Key(*code));
                        pre.extend(chord.iter().map(|&code| InputMsg::Key {
                            code,
                            down: true,
                            mods: 0,
                        }));
                        post.push(Held::Key(*code).release_msg());
                        post.extend(chord.iter().rev().map(|&m| Held::Key(m).release_msg()));
                    } else if *down {
                        core.held.insert(Held::Key(*code));
                    } else {
                        core.held.remove(&Held::Key(*code));
                    }
                }
                InputMsg::MouseButton { btn, down, .. } => {
                    if *down {
                        core.held.insert(Held::Btn(*btn));
                    } else {
                        core.held.remove(&Held::Btn(*btn));
                    }
                }
                InputMsg::MouseRelease { btn } => {
                    core.held.remove(&Held::Btn(*btn));
                }
                // FR-109 — relative mode presses where the pointer is. It is
                // held like any press, so a session that ends mid-drag still
                // has its button released.
                InputMsg::MousePress { btn } => {
                    core.held.insert(Held::Btn(*btn));
                }
                _ => {}
            }
        }
        EventPlan::Inject { pre, post }
    }

    /// Floor request. Returns `(granted, releases)` — on a takeover the
    /// previous holder's held set is released (their chord is over).
    pub fn request_floor(&mut self, session: ObjectId, now: Instant) -> (bool, Vec<InputMsg>) {
        if self.mode == Mode::Free {
            // Nothing to hold in free mode — trivially granted.
            return (true, Vec::new());
        }
        if !self.sessions.get(&session).is_some_and(SessCore::may_input) {
            return (false, Vec::new());
        }
        match self.holder {
            None => {
                self.holder = Some(session);
                self.pending_request = None;
                (true, Vec::new())
            }
            Some(h) if h == session => {
                self.pending_request = None;
                (true, Vec::new())
            }
            Some(h) => {
                let idle = self
                    .sessions
                    .get(&h)
                    .map(|c| now.duration_since(c.last_input) >= IDLE_TAKEOVER)
                    .unwrap_or(true);
                if idle {
                    let releases = self
                        .sessions
                        .get_mut(&h)
                        .map(|c| {
                            let r: Vec<InputMsg> = c.held.iter().map(Held::release_msg).collect();
                            c.held.clear();
                            r
                        })
                        .unwrap_or_default();
                    self.holder = Some(session);
                    self.pending_request = None;
                    (true, releases)
                } else {
                    // FR-27 — REMEMBER the refusal. Dropping it on the floor
                    // made "Request control" indistinguishable from a dead
                    // button: the holder was never told anyone wanted the
                    // floor, and the requester got no acknowledgement, so the
                    // only way to succeed was to happen to click during the
                    // holder's idle window. The snapshot broadcast that
                    // follows tells both ends.
                    self.pending_request = Some(session);
                    (false, Vec::new())
                }
            }
        }
    }

    /// FR-27 — the holder hands the floor over on request, without waiting for
    /// the idle timer. The courteous half of [`Self::request_floor`]: once you
    /// can SEE that someone is waiting, you need a way to say yes.
    ///
    /// Only the current holder may grant, and only to the session that
    /// actually asked — a stale click after the requester left, or after the
    /// floor moved, must not hand control to whoever asked last.
    pub fn grant_floor(&mut self, holder: ObjectId, to: ObjectId) -> (bool, Vec<InputMsg>) {
        if self.mode != Mode::Exclusive
            || self.holder != Some(holder)
            || self.pending_request != Some(to)
        {
            return (false, Vec::new());
        }
        if !self.sessions.get(&to).is_some_and(SessCore::may_input) {
            self.pending_request = None;
            return (false, Vec::new());
        }
        let releases = self
            .sessions
            .get_mut(&holder)
            .map(|c| {
                let r: Vec<InputMsg> = c.held.iter().map(Held::release_msg).collect();
                c.held.clear();
                r
            })
            .unwrap_or_default();
        self.holder = Some(to);
        self.pending_request = None;
        (true, releases)
    }

    /// FR-27 — the holder declines, or the requester withdraws. Clearing the
    /// request is what stops a refused chip from sitting on the toolbar
    /// forever; the requester may always ask again.
    pub fn clear_floor_request(&mut self, by: ObjectId) -> bool {
        let mine = self.pending_request == Some(by);
        let im_the_holder = self.holder == Some(by);
        if self.pending_request.is_some() && (mine || im_the_holder) {
            self.pending_request = None;
            return true;
        }
        false
    }

    /// In-session mode toggle (INPUT-granted sessions only). Returns whether
    /// the mode actually changed. Flipping to exclusive makes the toggling
    /// session the holder; flipping to free clears the floor.
    pub fn set_mode(&mut self, session: ObjectId, mode: Mode) -> bool {
        if !self.sessions.get(&session).is_some_and(SessCore::may_input) {
            return false;
        }
        self.mode_seeded = true;
        if self.mode == mode {
            return false;
        }
        self.mode = mode;
        self.holder = match mode {
            Mode::Exclusive => Some(session),
            Mode::Free => None,
        };
        // Either direction settles the floor, so nothing can still be waiting
        // for it: exclusive just handed it to the toggler, free abolished it.
        self.pending_request = None;
        true
    }

    /// FR-92 — is any session holding a mouse button? A keep-busy move
    /// spliced into a controller's drag would drag whatever they hold.
    pub fn any_button_held(&self) -> bool {
        self.sessions
            .values()
            .any(|c| c.held.iter().any(|h| matches!(h, Held::Btn(_))))
    }

    /// FR-92 — may `session` switch keep-busy on or off? The same bar as
    /// injecting: an INPUT grant, and the floor in exclusive mode. Returns
    /// the controller's display name for the `set_by` the host and the
    /// other viewers see.
    pub fn keep_busy_may_set(&self, session: ObjectId) -> Result<String, KeepBusyRefusal> {
        let core = self
            .sessions
            .get(&session)
            .ok_or(KeepBusyRefusal::Unknown)?;
        if !core.may_input() {
            return Err(KeepBusyRefusal::NoInput);
        }
        if self.mode == Mode::Exclusive && self.holder.is_some_and(|h| h != session) {
            return Err(KeepBusyRefusal::NotFloorHolder);
        }
        Ok(core.name.clone())
    }

    pub fn snapshot(&self) -> Snapshot {
        let participant = |id: &ObjectId| -> Option<Participant> {
            self.sessions.get(id).map(|c| Participant {
                session: *id,
                name: c.name.clone(),
                input: c.can_input,
            })
        };
        let mut participants: Vec<Participant> =
            self.sessions.keys().filter_map(participant).collect();
        participants.sort_by_key(|p| p.session);
        Snapshot {
            mode: self.mode,
            holder: self.holder,
            participants,
            // Free mode has no floor, so a leftover request there would render
            // a "waiting for control" chip nobody can act on.
            pending_request: (self.mode == Mode::Exclusive)
                .then(|| self.pending_request.as_ref().and_then(participant))
                .flatten(),
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

// ─── Worker layer ───────────────────────────────────────────────────────

type DcStash = Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>;

enum Cmd {
    Open {
        session: ObjectId,
        name: String,
        can_input: bool,
        mode_hint: Option<Mode>,
        control: DcStash,
        cursor: DcStash,
    },
    Close {
        session: ObjectId,
    },
    ReleaseHeld {
        session: ObjectId,
    },
    /// P6 field fix — a session's control DC just OPENED. Registration
    /// happens at `AgentPeer::new`, before any DC exists, so the join-time
    /// `rc:control.state` broadcast found an empty stash and was dropped:
    /// followers never saw the participants rail (and in exclusive mode
    /// could not render the Request-control button). Re-broadcast now.
    ControlReady {
        session: ObjectId,
    },
    Event {
        session: ObjectId,
        msg: InputMsg,
    },
    RequestFloor {
        session: ObjectId,
    },
    /// FR-27 — the holder hands the floor to whoever is waiting.
    GrantFloor {
        holder: ObjectId,
        to: ObjectId,
    },
    /// FR-27 — the holder declines, or the requester withdraws.
    ClearFloorRequest {
        session: ObjectId,
    },
    SetMode {
        session: ObjectId,
        mode: Mode,
    },
    /// FR-92 — keep-busy's engine asks the ONE injector to read or move.
    KeepBusy(KbReq),
    /// FR-92 — a controller asked to switch keep-busy on or off; the
    /// arbiter holds the grant and the floor, so it decides.
    KeepBusySet {
        session: ObjectId,
        request: crate::keep_busy::SetRequest,
    },
    /// FR-97 P3e — a controller's `rc:layout.set`. Switching the host's
    /// layout changes what the person at it types, so it takes keep busy's
    /// rule: a session that may type, and in exclusive mode the floor holder.
    /// Before P3e the control arm switched for any session, a view-only one's
    /// included.
    #[cfg(all(
        any(target_os = "windows", target_os = "linux", target_os = "macos"),
        feature = "enigo-input"
    ))]
    LayoutSet {
        session: ObjectId,
        token: String,
    },
    /// #4069 — an automatic system capture went live (`seat_guard`'s live
    /// edge, [`live_edge`]): the warm-up runs again for the sessions that may
    /// input, so the automatic uinput opens ahead of the first event whichever
    /// came first, the capture or a session's control DC.
    #[cfg(all(target_os = "linux", feature = "drm-capture"))]
    CaptureLive,
}

/// FR-92 — a keep-busy request, answered on `reply` from the arbiter thread
/// (where the one OS injector lives).
pub struct KbReq {
    pub op: KbOp,
    pub reply: std::sync::mpsc::SyncSender<KbReply>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KbOp {
    /// The cursor, in desktop pixels.
    Locate,
    /// A real-input absolute move to desktop pixel `(x, y)`.
    Move { x: i32, y: i32 },
    /// FR-92 P9 — a real-input absolute move to `(x, y) / NORM_ONE` of the
    /// whole desktop, for a backend with no pixel space of its own (uinput).
    MoveNorm { x: u32, y: u32 },
    /// Is a physical mouse button held?
    Buttons,
}

/// [`KbOp::MoveNorm`]'s unit: `NORM_ONE` is the far edge of the desktop.
/// 2¹⁶ steps is finer than a pixel on any screen uinput's own 0..=32767
/// axis can address.
pub const NORM_ONE: u32 = 1 << 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KbReply {
    Located(i32, i32),
    Moved,
    Buttons(bool),
    /// Refused: a controller is holding a mouse button.
    RemoteButtonHeld,
    /// This backend has no pixel-space read/move.
    Unsupported,
    /// The injector exists but the OS has not granted it (macOS
    /// Accessibility).
    NoPermission,
    Failed(String),
}

/// How long the keep-busy engine waits for the arbiter: the injector
/// thread's own bound plus room for a queue of controller events ahead.
pub const KB_REPLY_TIMEOUT: Duration = Duration::from_millis(500);

struct Sinks {
    control: DcStash,
    cursor: DcStash,
    last_ghost: Instant,
}

/// Process-global handle. Cheap to clone; all methods are non-blocking
/// sends into the worker (a full queue drops the event — input is a
/// realtime stream, stale events are worse than lost ones).
#[derive(Clone)]
pub struct Arbiter {
    tx: std::sync::mpsc::SyncSender<Cmd>,
}

/// The global arbiter. First call must happen inside a tokio runtime (the
/// worker captures the handle for DC sends) — peer.rs session setup is.
pub fn global() -> &'static Arbiter {
    static ARBITER: OnceLock<Arbiter> = OnceLock::new();
    ARBITER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Cmd>(512);
        let handle = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name("input-arbiter".into())
            .spawn(move || worker(rx, handle))
            .expect("spawn input-arbiter worker");
        // #4069 — an automatic system capture's live edge warms again. Set
        // as the arbiter starts, so a capture in a process without one tells
        // nobody, and the edge never starts it.
        #[cfg(all(target_os = "linux", feature = "drm-capture"))]
        crate::capture::seat_guard::on_live(live_edge(tx.clone()));
        Arbiter { tx }
    })
}

/// #4069 — what `seat_guard`'s live edge runs, on the capture's thread: one
/// [`Cmd::CaptureLive`], sent after the capture was counted alive, so the
/// arbiter's read of `seat_guard::live` behind it sees the capture. It holds
/// only the arbiter's sender, so it starts nothing and needs no runtime, and a
/// full queue drops it rather than block the capture: the first event then
/// opens, as before #4069.
#[cfg(all(target_os = "linux", feature = "drm-capture"))]
fn live_edge(tx: std::sync::mpsc::SyncSender<Cmd>) -> crate::capture::seat_guard::LiveHook {
    Box::new(move || {
        if tx.try_send(Cmd::CaptureLive).is_err() {
            tracing::debug!("input arbiter queue full — the capture's live edge dropped (#4069)");
        }
    })
}

impl Arbiter {
    pub fn session_open(
        &self,
        session: ObjectId,
        name: String,
        can_input: bool,
        mode_hint: Option<Mode>,
        control: DcStash,
        cursor: DcStash,
    ) {
        let _ = self.tx.try_send(Cmd::Open {
            session,
            name,
            can_input,
            mode_hint,
            control,
            cursor,
        });
    }
    pub fn session_closed(&self, session: ObjectId) {
        let _ = self.tx.try_send(Cmd::Close { session });
    }
    /// Input DC died mid-session — release this session's held keys/buttons
    /// without removing it from the participants rail.
    pub fn release_held(&self, session: ObjectId) {
        let _ = self.tx.try_send(Cmd::ReleaseHeld { session });
    }
    /// A session's control DC opened — (re)deliver `rc:control.state`.
    pub fn control_ready(&self, session: ObjectId) {
        let _ = self.tx.try_send(Cmd::ControlReady { session });
    }
    pub fn event(&self, session: ObjectId, msg: InputMsg) {
        if self.tx.try_send(Cmd::Event { session, msg }).is_err() {
            tracing::debug!(%session, "input arbiter queue full — event dropped");
        }
    }
    pub fn request_floor(&self, session: ObjectId) {
        let _ = self.tx.try_send(Cmd::RequestFloor { session });
    }
    /// FR-27 — the holder grants the floor to the waiting session.
    pub fn grant_floor(&self, holder: ObjectId, to: ObjectId) {
        let _ = self.tx.try_send(Cmd::GrantFloor { holder, to });
    }
    /// FR-27 — the holder declines, or the requester withdraws.
    pub fn clear_floor_request(&self, session: ObjectId) {
        let _ = self.tx.try_send(Cmd::ClearFloorRequest { session });
    }
    pub fn set_mode(&self, session: ObjectId, mode: Mode) {
        let _ = self.tx.try_send(Cmd::SetMode { session, mode });
    }

    /// FR-92 — run one keep-busy request on the injector and wait (bounded)
    /// for the answer. Called from the keep-busy engine thread, never from
    /// a tokio task.
    pub fn keep_busy(&self, op: KbOp) -> KbReply {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        if self
            .tx
            .try_send(Cmd::KeepBusy(KbReq { op, reply }))
            .is_err()
        {
            return KbReply::Failed("input arbiter queue full".into());
        }
        rx.recv_timeout(KB_REPLY_TIMEOUT)
            .unwrap_or_else(|_| KbReply::Failed("input arbiter did not answer".into()))
    }

    /// FR-92 — a controller's `rc:keep-busy.set`.
    pub fn keep_busy_set(&self, session: ObjectId, request: crate::keep_busy::SetRequest) {
        let _ = self.tx.try_send(Cmd::KeepBusySet { session, request });
    }

    /// FR-97 P3e — a controller's `rc:layout.set` ([`Cmd::LayoutSet`]).
    #[cfg(all(
        any(target_os = "windows", target_os = "linux", target_os = "macos"),
        feature = "enigo-input"
    ))]
    pub fn layout_set(&self, session: ObjectId, token: String) {
        let _ = self.tx.try_send(Cmd::LayoutSet { session, token });
    }
}

/// FR-97 P3e — [`Cmd::LayoutSet`]'s decision, apart from the worker so a test
/// holds it: `switch` runs only for a session keep busy's rule lets act. A
/// refusal sends nothing: the picker is bound to the reported layout, which did
/// not change, so it snaps back.
#[cfg(any(
    test,
    all(
        any(target_os = "windows", target_os = "linux", target_os = "macos"),
        feature = "enigo-input"
    )
))]
fn layout_set_if_allowed(
    state: &ArbiterState,
    session: ObjectId,
    token: String,
    switch: impl FnOnce(String),
) {
    match state.keep_busy_may_set(session) {
        Ok(_) => switch(token),
        Err(refusal) => tracing::info!(
            %session,
            refusal = refusal.wire(),
            "layout: a session's switch refused"
        ),
    }
}

/// #3918 — which sessions control the virtual desktop: none that is marked as
/// having no picture of it while its input would reach the desktop's X
/// server (`input::x_follow::Sessions`).
#[cfg(all(target_os = "linux", feature = "enigo-input"))]
use super::x_follow::Sessions as Follow;

/// #3918 — off Linux, or without the X11 injector, no session is left behind
/// by an X server.
#[cfg(not(all(target_os = "linux", feature = "enigo-input")))]
#[derive(Default)]
struct Follow {}

#[cfg(not(all(target_os = "linux", feature = "enigo-input")))]
impl Follow {
    fn closed(&mut self, _: ObjectId) {}
    fn drops(&mut self, _: &ArbiterState, _: ObjectId) -> bool {
        false
    }
    fn forget_left_behind(
        &mut self,
        _: &mut ArbiterState,
        _: Option<&(dyn super::InputInjector + Send)>,
    ) -> bool {
        false
    }
}

fn worker(rx: std::sync::mpsc::Receiver<Cmd>, handle: tokio::runtime::Handle) {
    let mut state = ArbiterState::default();
    let mut sinks: HashMap<ObjectId, Sinks> = HashMap::new();
    // ONE OS injector for the whole process — created lazily so the
    // SystemContext worker-role probe runs at first real use, matching the
    // pre-P6 per-session open timing.
    let mut injector: Option<Box<dyn super::InputInjector + Send>> = None;
    // #3164 — when an injector opened without the OS's grant is opened again.
    let mut regrant = Regrant::default();
    // #3918 — which sessions control the virtual desktop: none that is
    // marked, while its input would reach the desktop's X server.
    let mut follow = Follow::default();

    while let Ok(cmd) = rx.recv() {
        let parts = Parts {
            state: &mut state,
            sinks: &mut sinks,
            injector: &mut injector,
            regrant: &mut regrant,
            follow: &mut follow,
        };
        run_command(parts, cmd, &handle, |state, sinks| {
            broadcast_state(state, sinks, &handle);
        });
    }
    tracing::debug!("input arbiter worker exiting (channel closed)");
}

/// What one command acts on: the worker's own state, one per process, on
/// the worker's thread.
struct Parts<'a> {
    state: &'a mut ArbiterState,
    sinks: &'a mut HashMap<ObjectId, Sinks>,
    injector: &'a mut Option<Box<dyn super::InputInjector + Send>>,
    regrant: &'a mut Regrant,
    follow: &'a mut Follow,
}

/// One command, as the worker runs it: a test runs this function, not a
/// copy of an arm (#3918). `tell` sends the viewers the floor
/// state: [`broadcast_state`] in the daemon, a recorder in a test. A
/// command tells it at most once ([`Viewers`]).
fn run_command(
    parts: Parts<'_>,
    cmd: Cmd,
    handle: &tokio::runtime::Handle,
    tell: impl FnMut(&ArbiterState, &HashMap<ObjectId, Sinks>),
) {
    let Parts {
        state,
        sinks,
        injector,
        regrant,
        follow,
    } = parts;
    // #3918 — before anything is decided, every session marked as having
    // no picture of this device's virtual desktop whose input would reach
    // the desktop's X server is left behind: a session without input while
    // that lasts, whose held keys and buttons are forgotten, releasing
    // nothing, so neither its close, the modifier fence, a floor takeover
    // nor keep busy acts on what it held on the old X server. The floor it
    // held is given up, and the viewers are told, once ([`Viewers`]). One
    // read when no session is marked.
    let mut viewers = Viewers {
        owed: follow.forget_left_behind(state, injector.as_deref()),
        tell,
    };
    match cmd {
        Cmd::Open {
            session,
            name,
            can_input,
            mode_hint,
            control,
            cursor,
        } => {
            // #3164 — a new session's first event asks the OS afresh.
            regrant.session_opened();
            // #3787 — and its first missing display is said at its level.
            super::no_display::forget();
            state.open(session, name, can_input, mode_hint, Instant::now());
            sinks.insert(
                session,
                Sinks {
                    control,
                    cursor,
                    last_ghost: crate::clock::instant_before(GHOST_MIN_INTERVAL),
                },
            );
            tracing::info!(
                %session, can_input, mode = state.snapshot().mode.as_str(),
                sessions = state.session_count(),
                "input arbiter: session registered"
            );
            viewers.tell_if(true, state, sinks);
        }
        Cmd::Close { session } => {
            let releases = state.close(session);
            if !releases.is_empty() {
                tracing::info!(
                    %session,
                    released = releases.len(),
                    "input arbiter: release-all on session close"
                );
                inject_all_of(injector, regrant, &releases, Some(session));
                // FR-92 — a released button moves the pointer first
                // (#1945): keep-busy must re-anchor, not fight it.
                crate::keep_busy::note_remote_input();
            }
            // #3918 — its mark goes with it, after what it released was
            // offered under it.
            follow.closed(session);
            sinks.remove(&session);
            viewers.tell_if(state.session_count() > 0, state, sinks);
        }
        Cmd::ControlReady { session } => {
            tracing::debug!(%session, "input arbiter: control DC ready — re-broadcasting state");
            viewers.tell_if(true, state, sinks);
            if state.can_input(session) {
                warm(injector, "control_ready");
            }
        }
        #[cfg(all(target_os = "linux", feature = "drm-capture"))]
        Cmd::CaptureLive => {
            tracing::debug!("input arbiter: an automatic system capture went live (#4069)");
            if state.any_can_input() {
                warm(injector, "capture_live");
            }
        }
        Cmd::ReleaseHeld { session } => {
            let releases = state.release_held(session);
            if !releases.is_empty() {
                tracing::info!(
                    %session,
                    released = releases.len(),
                    "input arbiter: input DC closed — released held keys"
                );
                inject_all_of(injector, regrant, &releases, Some(session));
                crate::keep_busy::note_remote_input();
            }
        }
        Cmd::Event { session, msg } => 'event: {
            // #3918 — a session left behind (marked as having no picture
            // of the virtual desktop, its input bound for the desktop's X
            // server) controls nothing: dropped here, ahead of the plan and
            // of the gate that would open an injector on the new server. The
            // command still tells what its start changed, at its end.
            if follow.drops(state, session) {
                break 'event;
            }
            // Ghost cursors ride on intent, not on the floor: even a
            // denied (non-holder) session's pointer shows to others.
            if let InputMsg::MouseMove { x, y, mon } = msg
                && sinks.len() > 1
            {
                ghost_broadcast(state, sinks, handle, session, x, y, mon);
            }
            let Planned {
                plan,
                fenced,
                floor_moved,
            } = state.plan_event(session, &msg, Instant::now());
            // #3918 — what the viewers show of the floor is who
            // holds it after every command, this one included.
            viewers.tell_if(floor_moved, state, sinks);
            match plan {
                EventPlan::Deny => {}
                EventPlan::Inject { pre, post } => {
                    if !pre.is_empty() {
                        tracing::debug!(
                            %session,
                            before = pre.len(),
                            after = post.len(),
                            "input arbiter: modifier fence or chord"
                        );
                    }
                    inject_planned(injector, regrant, &pre, &fenced, &msg, &post, session);
                    // FR-92 — a controller's input reached the OS: a
                    // remote person is active. A heartbeat is not input.
                    if !matches!(msg, InputMsg::Heartbeat { .. }) {
                        crate::keep_busy::note_remote_input();
                    }
                }
            }
        }
        Cmd::KeepBusy(req) => {
            let reply = keep_busy_op(state, injector, regrant, req.op);
            let _ = req.reply.try_send(reply);
        }
        Cmd::KeepBusySet { session, request } => {
            // Accepted requests are answered by the engine's broadcast;
            // a refusal goes to the asking viewer alone, with its reason.
            let refused = match state.keep_busy_may_set(session) {
                Ok(name) => (!crate::keep_busy::submit_from_session(session, name, request))
                    .then_some("unsupported"),
                Err(refusal) => {
                    tracing::info!(
                        %session,
                        refusal = refusal.wire(),
                        "keep-busy: a session's request refused"
                    );
                    Some(refusal.wire())
                }
            };
            if let Some(why) = refused
                && let Some(s) = sinks.get(&session)
            {
                let stash = s.control.clone();
                let payload = crate::keep_busy::state_json(Some(why));
                handle.spawn(async move {
                    if let Some(dc) = stash.lock().await.clone() {
                        let _ = dc.send_text(payload).await;
                    }
                });
            }
        }
        #[cfg(all(
            any(target_os = "windows", target_os = "linux", target_os = "macos"),
            feature = "enigo-input"
        ))]
        Cmd::LayoutSet { session, token } => layout_set_if_allowed(
            state,
            session,
            token,
            crate::input::layout::request_set_layout,
        ),
        Cmd::RequestFloor { session } => {
            // What a takeover releases is what the holder held.
            let was = state.holder;
            let (granted, releases) = state.request_floor(session, Instant::now());
            tracing::info!(%session, granted, "input arbiter: floor request");
            if !releases.is_empty() {
                inject_all_of(injector, regrant, &releases, was);
                crate::keep_busy::note_remote_input();
            }
            viewers.tell_if(true, state, sinks);
        }
        Cmd::GrantFloor { holder, to } => {
            let (granted, releases) = state.grant_floor(holder, to);
            tracing::info!(%holder, %to, granted, "input arbiter: floor grant");
            if !releases.is_empty() {
                inject_all_of(injector, regrant, &releases, Some(holder));
                crate::keep_busy::note_remote_input();
            }
            // Broadcast even on a refusal: the requester may have left, or
            // the floor may have moved, and both ends need the truth.
            viewers.tell_if(true, state, sinks);
        }
        Cmd::ClearFloorRequest { session } => {
            let cleared = state.clear_floor_request(session);
            if cleared {
                tracing::info!(%session, "input arbiter: floor request cleared");
            }
            viewers.tell_if(cleared, state, sinks);
        }
        Cmd::SetMode { session, mode } => {
            if state.set_mode(session, mode) {
                tracing::info!(%session, mode = mode.as_str(), "input arbiter: mode changed");
            }
            viewers.tell_if(true, state, sinks);
        }
    }
    // What the command's start changed, when no arm told it.
    viewers.tell_if(false, state, sinks);
}

/// #3918 — what one command tells the viewers of the floor: at most one
/// state. [`broadcast_state`] spawns one task per sink, and two states one
/// command spawned can reach a viewer in either order, leaving it on the
/// older one. What the command's start changed (a session left behind) is
/// owed. An arm with a tell point pays it there, with or without a reason of
/// its own, after its last change: an event that moves no floor after its
/// plan and before its injection, a ClearFloorRequest that clears nothing and
/// the last Close at theirs. Only an arm without a tell point (ReleaseHeld,
/// KeepBusy, KeepBusySet, LayoutSet, CaptureLive) and a dropped event pay it
/// at the command's end. With nothing owed every arm tells as before. Two
/// commands still tell twice, in tasks of their own.
struct Viewers<F> {
    owed: bool,
    tell: F,
}

impl<F: FnMut(&ArbiterState, &HashMap<ObjectId, Sinks>)> Viewers<F> {
    /// The state as it is now, when the arm has a `reason` or the command's
    /// start owes one.
    fn tell_if(&mut self, reason: bool, state: &ArbiterState, sinks: &HashMap<ObjectId, Sinks>) {
        if reason || self.owed {
            self.owed = false;
            (self.tell)(state, sinks);
        }
    }
}

/// FR-93 P4c — open the ONE injector (or replace a stale one) as soon as a
/// session that may input is ready, instead of at its first event.
///
/// ⚠️ A uinput device is not usable the moment it exists: libinput adds it
/// asynchronously, and under load the first move written after the backend's
/// settle still reached nobody (measured on KDE, real hardware: 2 of 8 fresh
/// devices). Opened here, the device has the second or more a viewer takes to
/// send anything. Linux only — the race is libinput's — and an injector
/// without the OS's permission is not cached, by `keep_busy_op`'s rule: one
/// cached before a display exists would break every later controller's input.
/// `ROOMLERD_INPUT_WARM=0` keeps the first event as the opener ([`warm_on`]).
///
/// #4069 — it runs when a session's control DC is ready
/// ([`Cmd::ControlReady`]) and again when an automatic system capture goes
/// live (`Cmd::CaptureLive`), for the sessions that may input; `on` names
/// which. While that capture is planned and not live yet
/// (`input::awaits_automatic_capture`), it opens nothing: `open_default`
/// picks the automatic uinput only while the capture is live, and before that
/// it would open enigo, which a root daemon has no display for (#3787's ERROR,
/// with a person at the screen). On 0.4.227 a faster connect let the control
/// DC come before the capture, and the uinput opened at the first event,
/// without its warm-up.
fn warm(injector: &mut Option<Box<dyn super::InputInjector + Send>>, on: &'static str) {
    if !cfg!(target_os = "linux") || !warm_on() {
        return;
    }
    if injector.as_deref().is_some_and(|i| super::stale(i)) {
        *injector = None;
    }
    // #3918 — nothing opens while this device's virtual desktop is down, and
    // an injector left on an older X server of it is let go first. After the
    // drop above, so whatever was dropped there opens nothing then.
    if !follow_the_desktop(injector) {
        return;
    }
    if injector.is_some() {
        return;
    }
    // #4069 — the automatic system capture's live edge opens it. An event
    // before then opens what `open_default` picks then, as before.
    if super::awaits_automatic_capture() {
        tracing::info!(
            on,
            "input arbiter: the warm-up waits for the planned automatic system capture to go live \
             (#4069)"
        );
        return;
    }
    let fresh = open_injector();
    if fresh.has_permission() {
        tracing::info!(
            on,
            "input arbiter: injector opened ahead of the first event (FR-93 P4c)"
        );
        *injector = Some(fresh);
    }
}

/// FR-93 P4c — `ROOMLERD_INPUT_WARM` (default on), read at each warm-up: `0`
/// keeps the first event as the opener, the live edge's warm-up (#4069)
/// included.
fn warm_on() -> bool {
    #[cfg(test)]
    if let Some(on) = tests::WARM_HERE.with(std::cell::Cell::get) {
        return on;
    }
    tunnel_core::env::flag("INPUT_WARM", true)
}

/// #3164 — an injector opened before the OS granted input is not final.
///
/// macOS: enigo refuses to construct without Accessibility, so a session's
/// first event before the grant got `open_default`'s Noop, which was then
/// cached for the life of the process. The grant itself is live —
/// `AXIsProcessTrusted` sees it at once, and so does each new session's reach
/// check, which then stops warning — while the cached Noop kept dropping every
/// event. Field, agent 0.4.188: after the grant the viewer's banner was gone
/// and its typed line still never landed. So an injector without the
/// permission is dropped and opened again once the OS grants what it lacked,
/// asked at most once per [`Regrant::EVERY`] while events arrive (a stubborn
/// backend then costs one open a second, not one per event), and at once for
/// the first event of each new session.
///
/// #2066 — off macOS there is no grant to ask about. A Noop there means no
/// backend opened (a Linux daemon before its display or `/dev/uinput` exists),
/// and only trying again can tell that one would now. Each failed open warns,
/// and a root daemon with no `DISPLAY` never gets one, so the wait doubles with
/// every open, up to [`Regrant::AT_MOST`], and starts over with each new session.
#[derive(Debug, Default)]
struct Regrant {
    asked_at: Option<Instant>,
    /// Opens tried since this session began or an injector last had permission.
    opens: u32,
}

impl Regrant {
    const EVERY: Duration = Duration::from_secs(1);
    const AT_MOST: Duration = Duration::from_secs(60);

    /// How long after the last question the next one may come: [`Self::EVERY`]
    /// until the first open and right after it, then doubling.
    fn wait(&self) -> Duration {
        Self::EVERY
            .saturating_mul(1 << self.opens.saturating_sub(1).min(6))
            .min(Self::AT_MOST)
    }

    /// Whether to drop the cached injector and open another: it lacks the OS's
    /// permission, the OS was not asked within [`Self::wait`], and asked now
    /// (`os_grants`, only then) it grants it.
    fn reopen(
        &mut self,
        has_permission: bool,
        now: Instant,
        os_grants: impl FnOnce() -> bool,
    ) -> bool {
        if has_permission {
            self.opens = 0;
            return false;
        }
        if self
            .asked_at
            .is_some_and(|at| now.saturating_duration_since(at) < self.wait())
        {
            return false;
        }
        self.asked_at = Some(now);
        let granted = os_grants();
        if granted {
            self.opens = self.opens.saturating_add(1);
        }
        granted
    }

    /// A session opened: its first event asks the OS again.
    fn session_opened(&mut self) {
        self.asked_at = None;
        self.opens = 0;
    }
}

/// The cached injector, unless it no longer fits: stale (FR-95 P2,
/// `input::stale`), or opened without a permission it may have now (#3164,
/// #2066).
fn refresh(injector: &mut Option<Box<dyn super::InputInjector + Send>>, regrant: &mut Regrant) {
    refresh_with(injector, regrant, super::os_grants_input);
}

/// [`refresh`], with the OS's answer given (a test has no Accessibility to
/// grant).
fn refresh_with(
    injector: &mut Option<Box<dyn super::InputInjector + Send>>,
    regrant: &mut Regrant,
    os_grants: impl FnOnce() -> bool,
) {
    let Some(inj) = injector.as_deref() else {
        return;
    };
    if super::stale(inj) {
        *injector = None;
    } else if regrant.reopen(inj.has_permission(), Instant::now(), os_grants) {
        tracing::info!(
            "input: this injector has no permission — opening it again, no restart needed (#3164, \
             #2066). A failed open says why right after this line, except a missing display this \
             session already reported (#3787); otherwise no line means it worked."
        );
        *injector = None;
    }
}

/// #3918 — `false` for an event or an open while this device's virtual
/// desktop is down: the event is dropped and nothing opens (said once a down
/// period). An injector left on an older X server of the desktop is let go
/// here, as a stale one is, so the lazy open after this connects to the new
/// server. Only on Linux with the X11 injector (`input::x_follow`): everywhere
/// else this is `true` and reads nothing.
fn follow_the_desktop(injector: &mut Option<Box<dyn super::InputInjector + Send>>) -> bool {
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    match super::x_follow::gate(injector.as_deref()) {
        super::x_follow::Gate::Down => return false,
        super::x_follow::Gate::LeftBehind { was, now } => {
            super::x_follow::let_go(injector, was, now)
        }
        super::x_follow::Gate::Pass => {}
    }
    #[cfg(not(all(target_os = "linux", feature = "enigo-input")))]
    let _ = injector;
    true
}

/// The arbiter's lazy open of its ONE injector: [`super::open_default`]. A
/// test opens its stand-in instead, on the test's own thread.
fn open_injector() -> Box<dyn super::InputInjector + Send> {
    #[cfg(test)]
    if let Some(stand_in) =
        tests::STAND_IN_OPEN.with(|o| o.borrow_mut().as_mut().map(|open| open()))
    {
        return stand_in;
    }
    super::open_default()
}

/// [`inject_all_of`] for messages that are no session's own. Its callers are
/// the X11 injector's tests.
#[cfg(test)]
#[cfg_attr(
    not(all(target_os = "linux", feature = "enigo-input")),
    allow(dead_code)
)]
fn inject_all(
    injector: &mut Option<Box<dyn super::InputInjector + Send>>,
    regrant: &mut Regrant,
    msgs: &[InputMsg],
) {
    inject_all_of(injector, regrant, msgs, None);
}

/// Inject `msgs`, the input of session `by` (`None`: no session's).
fn inject_all_of(
    injector: &mut Option<Box<dyn super::InputInjector + Send>>,
    regrant: &mut Regrant,
    msgs: &[InputMsg],
    by: Option<ObjectId>,
) {
    for m in msgs {
        // FR-45 P4 — while a portal capture with granted input is live, the
        // portal session owns injection: on the hosts that backend serves,
        // the OS injector's events have no reader at all (which is why the
        // portal is in use). Checked PER EVENT, not at injector creation —
        // the injector is created lazily at the first event while the portal
        // capture opens concurrently from another task, so a one-time choice
        // would race startup and freeze the loser in for the process life.
        // #3918 — first, ahead of the mark check below: a marked session's
        // event that a route carries is delivered.
        if route_takes(m) {
            continue;
        }
        // FR-95 P2 — the injector follows the automatic system capture: its
        // uinput while one reads a person's session, the configured cascade
        // otherwise (`input::stale`). #3164 — and a grant given since it
        // opened.
        refresh(injector, regrant);
        // #3918 — and this device's virtual desktop to its new X server; an
        // event for it while it is down is dropped here. Last before the
        // lazy open, so an injector the refresh just dropped is not opened
        // again onto a display with no server.
        if !follow_the_desktop(injector) {
            continue;
        }
        let inj = injector.get_or_insert_with(open_injector);
        // #3918 — an event of a session marked as having no picture of the
        // virtual desktop never reaches an X client of the desktop, whatever
        // the start of the command decided (`x_follow::Sessions`): inside
        // one command the regrant can swap a cached injector for one on the
        // new server, and a route can go between the two reads. Here, on the
        // OS injector's branch, after the open: what a route takes is not
        // concerned, and neither is any other injector. The injector's kind
        // first: an injector that is no X client takes no lock here.
        if inj.x_generation().is_some() && by.is_some_and(super::has_no_picture) {
            continue;
        }
        if let Err(e) = inj.inject(m.clone()) {
            tracing::debug!(%e, "input arbiter: inject failed");
        }
    }
}

/// FR-45 P4 — whether a live route (the portal's or mutter's) took `m`
/// ahead of the OS injector. One call site in every build (#3918), so a
/// test pins where production asks: a Linux test build with `enigo-input`
/// asks its own thread's route (`x_follow::test_seam::route_takes`), because
/// the real routes are process-wide and another test's would take this
/// test's events; a non-test Linux build with `portal-capture` asks the real
/// routes, as before; every other build answers `false`, as a non-test build
/// did before, and a test build without `enigo-input` reads no route.
fn route_takes(m: &InputMsg) -> bool {
    #[cfg(all(test, target_os = "linux", feature = "enigo-input"))]
    {
        super::x_follow::test_seam::route_takes(m)
    }
    #[cfg(all(not(test), target_os = "linux", feature = "portal-capture"))]
    {
        crate::capture::portal::input_route::try_route(m)
    }
    #[cfg(not(any(
        all(test, target_os = "linux", feature = "enigo-input"),
        all(not(test), target_os = "linux", feature = "portal-capture")
    )))]
    {
        let _ = m;
        false
    }
}

/// One event of session `by` as [`ArbiterState::plan_event`] planned it.
/// ⚠️ `post` runs whatever `msg` did: it is the release of what `pre`
/// pressed.
///
/// #3918 — the fence's releases, first in `pre`, are the input of
/// the sessions whose modifiers they let go (`fenced`, one each), and are
/// offered under each one's own mark: a marked `by` would drop a live
/// session's release at the desktop's X client while the arbiter has already
/// forgotten the key, which stays down on the desktop. A marked owner's
/// release reaches a route as before and no X client of the desktop, which
/// its press never reached either. The chord's presses, the event and `post`
/// are `by`'s.
fn inject_planned(
    injector: &mut Option<Box<dyn super::InputInjector + Send>>,
    regrant: &mut Regrant,
    pre: &[InputMsg],
    fenced: &[ObjectId],
    msg: &InputMsg,
    post: &[InputMsg],
    by: ObjectId,
) {
    let (fence, chord) = pre.split_at(fenced.len().min(pre.len()));
    for (release, owner) in fence.iter().zip(fenced) {
        inject_all_of(
            injector,
            regrant,
            std::slice::from_ref(release),
            Some(*owner),
        );
    }
    inject_all_of(injector, regrant, chord, Some(by));
    inject_all_of(injector, regrant, std::slice::from_ref(msg), Some(by));
    inject_all_of(injector, regrant, post, Some(by));
}

/// FR-92 — run one keep-busy request against the ONE injector.
///
/// ⚠️ The injector is created lazily and then cached, a `NoopInjector` included
/// until [`Regrant`] opens it again. Remote control's first event used to be
/// the only creator, and a session implies a display. Keep-busy may be first (a
/// boot-time restore), so an injector without the OS's permission is answered
/// for and NOT cached: a Noop cached before a display exists would cost a later
/// controller its first input. (The engine also sends nothing before its host
/// confirms an interactive session.)
fn keep_busy_op(
    state: &ArbiterState,
    injector: &mut Option<Box<dyn super::InputInjector + Send>>,
    regrant: &mut Regrant,
    op: KbOp,
) -> KbReply {
    if matches!(op, KbOp::Move { .. } | KbOp::MoveNorm { .. }) && state.any_button_held() {
        return KbReply::RemoteButtonHeld;
    }
    // FR-95 P2 — the same swap the controller's path makes (`input::stale`),
    // and #3164's regrant.
    refresh(injector, regrant);
    // #3918 — as for a controller's event, and after the refresh as there:
    // nothing while this device's virtual desktop is down, a transient
    // failure keep busy retries.
    if !follow_the_desktop(injector) {
        return KbReply::Failed("this device's virtual desktop is down".into());
    }
    if injector.is_none() {
        let fresh = open_injector();
        if !fresh.has_permission() {
            return classify_unpermitted(fresh);
        }
        *injector = Some(fresh);
    }
    let Some(inj) = injector.as_mut() else {
        return KbReply::Unsupported;
    };
    if !inj.has_permission() {
        return KbReply::NoPermission;
    }
    let result = match op {
        KbOp::Locate => inj.cursor_px().map(|(x, y)| KbReply::Located(x, y)),
        KbOp::Move { x, y } => inj.move_px(x, y).map(|()| KbReply::Moved),
        KbOp::MoveNorm { x, y } => {
            let f = |v: u32| f64::from(v.min(NORM_ONE)) / f64::from(NORM_ONE);
            inj.move_norm(f(x), f(y)).map(|()| KbReply::Moved)
        }
        KbOp::Buttons => inj.buttons_held().map(KbReply::Buttons),
    };
    result.unwrap_or_else(|e| {
        if e.is::<super::PxUnsupported>() {
            KbReply::Unsupported
        } else {
            KbReply::Failed(e.to_string())
        }
    })
}

/// An injector the OS has not granted: a Noop (no backend at all) has no
/// pixel read either; a real backend without permission (macOS
/// Accessibility) does.
///
/// ⚠️ macOS asks the OS first. enigo now REFUSES to construct without
/// Accessibility ("the application does not have the permission to simulate
/// input"), so `open_default` hands back a Noop there too, and a Noop alone
/// reads as `unsupported`: "Unavailable on this computer." Field, macOS 27,
/// 0.4.126, with the grant switched off: exactly that, where the truth was
/// one switch the owner can flip. `no_permission` is the reason that says so.
fn classify_unpermitted(mut inj: Box<dyn super::InputInjector + Send>) -> KbReply {
    #[cfg(target_os = "macos")]
    if !crate::tcc::accessibility_trusted() {
        return KbReply::NoPermission;
    }
    match inj.cursor_px() {
        Err(e) if e.is::<super::PxUnsupported>() => KbReply::Unsupported,
        _ => KbReply::NoPermission,
    }
}

/// `rc:control.state` fan-out to every session's control DC.
fn broadcast_state(
    state: &ArbiterState,
    sinks: &HashMap<ObjectId, Sinks>,
    handle: &tokio::runtime::Handle,
) {
    let snap = state.snapshot();
    let payload = serde_json::json!({
        "t": "rc:control.state",
        "mode": snap.mode.as_str(),
        "holder": snap.holder.map(|h| h.to_hex()),
        "participants": snap
            .participants
            .iter()
            .map(|p| serde_json::json!({
                "session": p.session.to_hex(),
                "name": p.name,
                "input": p.input,
            }))
            .collect::<Vec<_>>(),
        // FR-27 — who is waiting for the floor. Omitted when nothing is, so a
        // viewer that ignores the key behaves exactly as before.
        "pending_request": snap.pending_request.as_ref().map(|p| serde_json::json!({
            "session": p.session.to_hex(),
            "name": p.name,
        })),
    })
    .to_string();
    for s in sinks.values() {
        let stash = s.control.clone();
        let payload = payload.clone();
        handle.spawn(async move {
            if let Some(dc) = stash.lock().await.clone() {
                let _ = dc.send_text(payload).await;
            }
        });
    }
}

/// Throttled `cursor:peer` rebroadcast of one session's pointer to every
/// OTHER session's cursor DC.
fn ghost_broadcast(
    state: &ArbiterState,
    sinks: &mut HashMap<ObjectId, Sinks>,
    handle: &tokio::runtime::Handle,
    from: ObjectId,
    x: f32,
    y: f32,
    mon: u8,
) {
    let now = Instant::now();
    let Some(own) = sinks.get_mut(&from) else {
        return;
    };
    if now.duration_since(own.last_ghost) < GHOST_MIN_INTERVAL {
        return;
    }
    own.last_ghost = now;
    // Field diagnostic (2026-08-05): ghost cursors were not observed in the
    // first two-viewer field test. The viewer coalesces `mouse_move` through
    // `requestAnimationFrame`, which Chrome SUSPENDS in a background tab — so
    // a test driving two tabs (only one foreground at a time) may simply
    // never send the moves. Log the first send and then every 300th so the
    // field can tell "agent never sent" from "viewer never rendered" without
    // a debug build.
    let n = GHOSTS_SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if n == 1 || n.is_multiple_of(300) {
        tracing::info!(
            source = %from,
            targets = sinks.len().saturating_sub(1),
            total = n,
            "input arbiter: ghost cursor broadcast"
        );
    }
    let name = state
        .snapshot()
        .participants
        .iter()
        .find(|p| p.session == from)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let payload = serde_json::json!({
        "t": "cursor:peer",
        "sid": from.to_hex(),
        "name": name,
        "x": x,
        "y": y,
        "mon": mon,
    })
    .to_string();
    for (id, s) in sinks.iter() {
        if *id == from {
            continue;
        }
        let stash = s.cursor.clone();
        let payload = payload.clone();
        handle.spawn(async move {
            if let Some(dc) = stash.lock().await.clone() {
                let _ = dc.send_text(payload).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid() -> ObjectId {
        ObjectId::new()
    }
    fn key(code: u32, down: bool) -> InputMsg {
        InputMsg::Key {
            code,
            down,
            mods: 0,
        }
    }
    fn click(down: bool) -> InputMsg {
        InputMsg::MouseButton {
            btn: Button::Left,
            down,
            x: 0.5,
            y: 0.5,
            mon: 0,
        }
    }

    #[test]
    fn free_mode_switch_fences_other_sessions_held_modifiers() {
        let mut st = ArbiterState::default();
        let (a, b) = (sid(), sid());
        let now = Instant::now();
        st.open(a, "A".into(), true, None, now);
        st.open(b, "B".into(), true, None, now);

        // A holds Left-Ctrl (0xe0).
        assert_eq!(
            st.plan(a, &key(0xe0, true), now),
            EventPlan::Inject {
                pre: vec![],
                post: vec![]
            }
        );
        // B clicks — A's held Ctrl must be released FIRST.
        match st.plan(b, &click(true), now) {
            EventPlan::Inject { pre, .. } => {
                assert_eq!(pre.len(), 1);
                assert!(matches!(
                    pre[0],
                    InputMsg::Key {
                        code: 0xe0,
                        down: false,
                        ..
                    }
                ));
            }
            p => panic!("expected inject, got {p:?}"),
        }
        // A's Ctrl was force-released — its own later key-up releases nothing
        // new, and switching back to A fences B's held button? No: the fence
        // covers MODIFIERS only; B's held left button is untouched.
        match st.plan(a, &key(0xe0, false), now) {
            EventPlan::Inject { pre, .. } => assert!(pre.is_empty(), "no modifiers left to fence"),
            p => panic!("expected inject, got {p:?}"),
        }
    }

    #[test]
    fn non_modifier_keys_are_not_fenced_on_switch() {
        let mut st = ArbiterState::default();
        let (a, b) = (sid(), sid());
        let now = Instant::now();
        st.open(a, "A".into(), true, None, now);
        st.open(b, "B".into(), true, None, now);
        // A holds the letter W (0x1a) — a game-style hold, not a modifier.
        st.plan(a, &key(0x1a, true), now);
        match st.plan(b, &click(true), now) {
            EventPlan::Inject { pre, .. } => assert!(pre.is_empty(), "letters are not fenced"),
            p => panic!("expected inject, got {p:?}"),
        }
    }

    #[test]
    fn close_releases_everything_the_session_held() {
        let mut st = ArbiterState::default();
        let a = sid();
        let now = Instant::now();
        st.open(a, "A".into(), true, None, now);
        st.plan(a, &key(0xe1, true), now); // L-Shift
        st.plan(a, &key(0x04, true), now); // letter A
        st.plan(a, &click(true), now); // left button
        let releases = st.close(a);
        assert_eq!(releases.len(), 3, "shift + letter + button all released");
        assert!(releases.iter().all(|m| matches!(
            m,
            InputMsg::Key { down: false, .. } | InputMsg::MouseRelease { .. }
        )));
        // Idempotent.
        assert!(st.close(a).is_empty());
    }

    /// FR-109 — relative mode presses with `MousePress`, and that press is
    /// held like any other: a session that ends mid-drag still has its button
    /// released, and released where the pointer is. A watcher without input
    /// moves nothing, relatively or not.
    #[test]
    fn a_relative_press_is_held_and_released_at_the_end() {
        let now = Instant::now();
        let mut st = ArbiterState::default();
        let (a, w) = (sid(), sid());
        st.open(a, "A".into(), true, None, now);
        st.open(w, "W".into(), false, None, now);
        assert_eq!(
            st.plan(a, &InputMsg::MousePress { btn: Button::Left }, now),
            EventPlan::Inject {
                pre: vec![],
                post: vec![]
            }
        );
        assert_eq!(
            st.plan(a, &InputMsg::MouseRel { dx: 30, dy: 0 }, now),
            EventPlan::Inject {
                pre: vec![],
                post: vec![]
            }
        );
        assert_eq!(
            st.plan(w, &InputMsg::MouseRel { dx: 30, dy: 0 }, now),
            EventPlan::Deny
        );
        assert_eq!(
            st.close(a),
            vec![InputMsg::MouseRelease { btn: Button::Left }]
        );

        // The control: a release lets go of the relative press, so nothing is
        // left to release when the session ends.
        let mut st = ArbiterState::default();
        st.open(a, "A".into(), true, None, now);
        st.plan(a, &InputMsg::MousePress { btn: Button::Left }, now);
        st.plan(a, &InputMsg::MouseRelease { btn: Button::Left }, now);
        assert!(st.close(a).is_empty());
    }

    /// #1945 — every path that lets go of a session's held button lets go
    /// WHERE THE POINTER IS. A `MouseButton` up carries a position every
    /// backend moves to first; the (0,0) the release-all used to send was
    /// GNOME's Activities hot corner (a macOS hot corner may lock the screen),
    /// and the end of the drag.
    #[test]
    fn a_held_button_is_released_without_moving_the_pointer() {
        let moves = |ms: &[InputMsg]| {
            ms.iter()
                .filter(|m| matches!(m, InputMsg::MouseButton { .. } | InputMsg::MouseMove { .. }))
                .count()
        };
        let released = |ms: &[InputMsg]| ms.contains(&InputMsg::MouseRelease { btn: Button::Left });
        let now = Instant::now();

        // The session closes mid-drag (tab closed, browser crashed, watchdog).
        let mut st = ArbiterState::default();
        let a = sid();
        st.open(a, "A".into(), true, None, now);
        st.plan(a, &click(true), now);
        let r = st.close(a);
        assert!(released(&r) && moves(&r) == 0, "close: {r:?}");

        // Its input channel dies but the session lives on.
        let mut st = ArbiterState::default();
        st.open(a, "A".into(), true, None, now);
        st.plan(a, &click(true), now);
        let r = st.release_held(a);
        assert!(released(&r) && moves(&r) == 0, "release_held: {r:?}");

        // Exclusive mode: an idle holder's floor is taken over, and the holder
        // hands it over on request.
        let b = sid();
        let mut st = ArbiterState::default();
        st.open(a, "A".into(), true, Some(Mode::Exclusive), now);
        st.open(b, "B".into(), true, None, now);
        st.plan(a, &click(true), now);
        let (granted, r) = st.request_floor(b, now + IDLE_TAKEOVER);
        assert!(granted);
        assert!(released(&r) && moves(&r) == 0, "takeover: {r:?}");

        let mut st = ArbiterState::default();
        st.open(a, "A".into(), true, Some(Mode::Exclusive), now);
        st.open(b, "B".into(), true, None, now);
        st.plan(a, &click(true), now);
        assert!(
            !st.request_floor(b, now).0,
            "an active holder keeps the floor"
        );
        let (granted, r) = st.grant_floor(a, b);
        assert!(granted);
        assert!(released(&r) && moves(&r) == 0, "grant: {r:?}");
    }

    /// A viewer may send the no-move release itself; it ends the hold, so
    /// nothing is released twice when the session closes.
    #[test]
    fn a_viewers_own_release_ends_the_hold() {
        let mut st = ArbiterState::default();
        let a = sid();
        let now = Instant::now();
        st.open(a, "A".into(), true, None, now);
        st.plan(a, &click(true), now);
        st.plan(a, &InputMsg::MouseRelease { btn: Button::Left }, now);
        assert!(st.close(a).is_empty());
    }

    /// FR-27 P10 — the host's Disconnect closes the session HERE at the click
    /// (`AgentPeer::stop_serving`) while its transport lives on for the grace,
    /// so whatever the input channel still delivers must go nowhere.
    #[test]
    fn a_closed_sessions_later_events_are_denied() {
        let mut st = ArbiterState::default();
        let a = sid();
        let now = Instant::now();
        st.open(a, "A".into(), true, None, now);
        assert!(matches!(
            st.plan(a, &click(true), now),
            EventPlan::Inject { .. }
        ));
        st.close(a);
        assert_eq!(st.plan(a, &click(false), now), EventPlan::Deny);
        assert_eq!(st.plan(a, &key(0x04, true), now), EventPlan::Deny);
        // A denied event is not tracked as held: nothing is left to release.
        assert!(st.close(a).is_empty());
    }

    #[test]
    fn exclusive_mode_drops_non_holder_events() {
        let mut st = ArbiterState::default();
        let (a, b) = (sid(), sid());
        let now = Instant::now();
        st.open(a, "A".into(), true, Some(Mode::Exclusive), now);
        st.open(b, "B".into(), true, None, now);
        assert_eq!(st.snapshot().holder, Some(a), "first INPUT session holds");
        assert!(matches!(
            st.plan(a, &click(true), now),
            EventPlan::Inject { .. }
        ));
        assert_eq!(st.plan(b, &click(true), now), EventPlan::Deny);
        // Heartbeats always pass.
        assert!(matches!(
            st.plan(b, &InputMsg::Heartbeat { seq: 1, ts_ms: 0 }, now),
            EventPlan::Inject { .. }
        ));
    }

    #[test]
    fn floor_request_denied_while_holder_active_granted_when_idle() {
        let mut st = ArbiterState::default();
        let (a, b) = (sid(), sid());
        let now = Instant::now();
        st.open(a, "A".into(), true, Some(Mode::Exclusive), now);
        st.open(b, "B".into(), true, None, now);
        // Holder is actively typing (and holding Shift).
        st.plan(a, &key(0xe1, true), now);
        let (granted, _) = st.request_floor(b, now + Duration::from_millis(500));
        assert!(!granted, "active holder keeps the floor");
        // Holder goes idle past the takeover window → transfer + release-all.
        let later = now + IDLE_TAKEOVER + Duration::from_millis(1);
        let (granted, releases) = st.request_floor(b, later);
        assert!(granted);
        assert_eq!(releases.len(), 1, "previous holder's Shift released");
        assert_eq!(st.snapshot().holder, Some(b));
        // B now injects; A is denied.
        assert!(matches!(
            st.plan(b, &click(true), later),
            EventPlan::Inject { .. }
        ));
        assert_eq!(st.plan(a, &click(true), later), EventPlan::Deny);
    }

    #[test]
    fn mode_toggle_and_holder_lifecycle() {
        let mut st = ArbiterState::default();
        let (a, b) = (sid(), sid());
        let now = Instant::now();
        st.open(a, "A".into(), true, None, now);
        st.open(b, "B".into(), true, None, now);
        assert_eq!(st.snapshot().mode, Mode::Free);
        // B flips to exclusive → B holds.
        assert!(st.set_mode(b, Mode::Exclusive));
        assert_eq!(st.snapshot().holder, Some(b));
        assert_eq!(st.plan(a, &click(true), now), EventPlan::Deny);
        // Holder leaves → floor passes to a remaining INPUT session.
        st.close(b);
        assert_eq!(st.snapshot().holder, Some(a));
        assert!(matches!(
            st.plan(a, &click(true), now),
            EventPlan::Inject { .. }
        ));
        // Back to free clears the floor.
        assert!(st.set_mode(a, Mode::Free));
        assert_eq!(st.snapshot().holder, None);
    }

    #[test]
    fn policy_mode_hint_applies_once_and_never_stomps_a_toggle() {
        let mut st = ArbiterState::default();
        let (a, b) = (sid(), sid());
        let now = Instant::now();
        st.open(a, "A".into(), true, Some(Mode::Exclusive), now);
        assert_eq!(st.snapshot().mode, Mode::Exclusive);
        st.set_mode(a, Mode::Free);
        // A later join re-sending the policy hint must not re-apply it.
        st.open(b, "B".into(), true, Some(Mode::Exclusive), now);
        assert_eq!(st.snapshot().mode, Mode::Free, "in-session toggle wins");
    }

    #[test]
    fn view_only_sessions_appear_in_snapshot_but_never_inject_or_hold() {
        let mut st = ArbiterState::default();
        let (a, w) = (sid(), sid());
        let now = Instant::now();
        st.open(w, "Watcher".into(), false, Some(Mode::Exclusive), now);
        assert_eq!(st.snapshot().holder, None, "view-only never holds");
        st.open(a, "A".into(), true, None, now);
        assert_eq!(st.plan(w, &click(true), now), EventPlan::Deny);
        let (granted, _) = st.request_floor(w, now);
        assert!(!granted);
        assert!(!st.set_mode(w, Mode::Free), "view-only cannot toggle mode");
        let snap = st.snapshot();
        assert_eq!(snap.participants.len(), 2);
        assert!(
            snap.participants
                .iter()
                .any(|p| p.name == "Watcher" && !p.input)
        );
    }

    /// Field regression (2026-08-05): arbiter entries LEAKED because
    /// deregistration hung off the control DC's `on_close`, which does not
    /// fire on PeerConnection teardown — a fresh session then registered as
    /// `sessions=3` while the server reported zero open sessions. The state
    /// machine itself is correct (close removes the entry); the fix is the
    /// CALLER (`Drop for AgentPeer`). This locks the invariant the leak
    /// violated: registering N sessions and closing them all leaves zero,
    /// and a later session sees a count of exactly 1.
    #[test]
    fn close_leaves_no_residue_for_the_next_session() {
        let mut st = ArbiterState::default();
        let now = Instant::now();
        let (a, b) = (sid(), sid());
        st.open(a, "A".into(), true, None, now);
        st.open(b, "B".into(), true, None, now);
        assert_eq!(st.session_count(), 2);
        st.close(a);
        st.close(b);
        assert_eq!(st.session_count(), 0, "both entries released");
        let c = sid();
        st.open(c, "C".into(), true, None, now);
        assert_eq!(
            st.session_count(),
            1,
            "a later session must see a clean arbiter, not stale peers"
        );
        // And the reused-close path stays idempotent.
        assert!(st.close(a).is_empty());
        assert_eq!(st.session_count(), 1);
    }

    #[test]
    fn mode_parse_round_trips() {
        assert_eq!(Mode::parse("free"), Some(Mode::Free));
        assert_eq!(Mode::parse(" Exclusive "), Some(Mode::Exclusive));
        assert_eq!(Mode::parse("nope"), None);
        assert_eq!(Mode::Free.as_str(), "free");
        assert_eq!(Mode::Exclusive.as_str(), "exclusive");
    }

    // ─── FR-27 ──────────────────────────────────────────────────────────

    /// The device policy must apply to EVERY fresh occupancy, not just the
    /// first one since the daemon booted. `mode_seeded` used to latch for the
    /// process lifetime, so one viewer's in-session toggle silently disabled
    /// the policy forever.
    #[test]
    fn the_device_policy_re_seeds_once_everyone_has_left() {
        let (a, b) = (sid(), sid());
        let mut st = ArbiterState::default();
        let now = Instant::now();

        st.open(a, "A".into(), true, Some(Mode::Exclusive), now);
        assert_eq!(st.snapshot().mode, Mode::Exclusive);

        // A viewer overrides it mid-session — legitimate, and it must stick
        // while anyone is still connected.
        assert!(st.set_mode(a, Mode::Free));
        st.open(b, "B".into(), true, Some(Mode::Exclusive), now);
        assert_eq!(
            st.snapshot().mode,
            Mode::Free,
            "a later JOIN must not stomp a live in-session toggle"
        );

        // Everyone leaves. There is no conversation left to preserve.
        st.close(a);
        st.close(b);
        st.open(sid(), "C".into(), true, Some(Mode::Exclusive), now);
        assert_eq!(
            st.snapshot().mode,
            Mode::Exclusive,
            "the device policy must apply again once the device is idle"
        );
    }

    /// A refused floor request has to be VISIBLE. Dropping it made "Request
    /// control" indistinguishable from a dead button.
    #[test]
    fn a_refused_floor_request_is_remembered_and_broadcast() {
        let (holder, asker) = (sid(), sid());
        let mut st = ArbiterState::default();
        let t0 = Instant::now();
        st.open(holder, "Holder".into(), true, Some(Mode::Exclusive), t0);
        st.open(asker, "Asker".into(), true, None, t0);
        assert_eq!(st.snapshot().holder, Some(holder));

        // The holder is ACTIVE, so no auto-takeover…
        st.plan(holder, &key(0x04, true), t0);
        let (granted, _) = st.request_floor(asker, t0);
        assert!(!granted);

        // …but both ends can now see who is waiting.
        let snap = st.snapshot();
        assert_eq!(
            snap.pending_request.as_ref().map(|p| p.session),
            Some(asker)
        );
        assert_eq!(snap.pending_request.unwrap().name, "Asker");
    }

    /// The holder can hand over without waiting out the idle timer — and
    /// handing over releases whatever they were holding down, exactly as an
    /// idle takeover does.
    #[test]
    fn the_holder_can_grant_the_floor_and_their_keys_are_released() {
        let (holder, asker) = (sid(), sid());
        let mut st = ArbiterState::default();
        let t0 = Instant::now();
        st.open(holder, "Holder".into(), true, Some(Mode::Exclusive), t0);
        st.open(asker, "Asker".into(), true, None, t0);

        st.plan(holder, &key(0xe0, true), t0); // holding Ctrl
        st.plan(holder, &click(true), t0); // and the left button
        assert!(!st.request_floor(asker, t0).0);

        let (granted, releases) = st.grant_floor(holder, asker);
        assert!(granted);
        assert_eq!(st.snapshot().holder, Some(asker));
        assert_eq!(
            releases.len(),
            2,
            "the outgoing holder's chord must not be left down"
        );
        assert!(st.snapshot().pending_request.is_none());
        // The new holder can actually inject now.
        assert!(matches!(
            st.plan(asker, &key(0x04, true), t0),
            EventPlan::Inject { .. }
        ));
    }

    /// Only the CURRENT holder may grant, and only to the session that asked.
    /// A stale click must not hand control to whoever asked last.
    #[test]
    fn granting_is_refused_from_a_non_holder_or_to_a_non_requester() {
        let (holder, asker, third) = (sid(), sid(), sid());
        let mut st = ArbiterState::default();
        let t0 = Instant::now();
        st.open(holder, "Holder".into(), true, Some(Mode::Exclusive), t0);
        st.open(asker, "Asker".into(), true, None, t0);
        st.open(third, "Third".into(), true, None, t0);
        st.plan(holder, &key(0x04, true), t0);
        assert!(!st.request_floor(asker, t0).0);

        assert!(!st.grant_floor(third, asker).0, "a non-holder cannot grant");
        assert!(
            !st.grant_floor(holder, third).0,
            "the holder cannot grant to someone who never asked"
        );
        assert_eq!(st.snapshot().holder, Some(holder));
        assert_eq!(
            st.snapshot().pending_request.map(|p| p.session),
            Some(asker),
            "a refused grant must leave the real request standing"
        );
    }

    /// A request must not outlive the thing it was about: the requester
    /// leaving, the mode being abolished, or a decline.
    #[test]
    fn a_pending_request_is_cleared_by_departure_mode_change_or_decline() {
        let (holder, asker) = (sid(), sid());
        let t0 = Instant::now();

        // …the requester disconnects.
        let mut st = ArbiterState::default();
        st.open(holder, "H".into(), true, Some(Mode::Exclusive), t0);
        st.open(asker, "A".into(), true, None, t0);
        st.plan(holder, &key(0x04, true), t0);
        st.request_floor(asker, t0);
        st.close(asker);
        assert!(st.snapshot().pending_request.is_none());

        // …the holder declines.
        let mut st = ArbiterState::default();
        st.open(holder, "H".into(), true, Some(Mode::Exclusive), t0);
        st.open(asker, "A".into(), true, None, t0);
        st.plan(holder, &key(0x04, true), t0);
        st.request_floor(asker, t0);
        assert!(st.clear_floor_request(holder));
        assert!(st.snapshot().pending_request.is_none());

        // …the mode goes free, so there is no floor left to want.
        let mut st = ArbiterState::default();
        st.open(holder, "H".into(), true, Some(Mode::Exclusive), t0);
        st.open(asker, "A".into(), true, None, t0);
        st.plan(holder, &key(0x04, true), t0);
        st.request_floor(asker, t0);
        assert!(st.set_mode(holder, Mode::Free));
        assert!(st.snapshot().pending_request.is_none());
    }

    /// Handover on close must be deterministic — two viewers watching the same
    /// disconnect have to agree on who got the floor.
    #[test]
    fn floor_handover_on_close_is_deterministic() {
        let mut ids = [sid(), sid(), sid()];
        ids.sort();
        let (low, high, holder) = (ids[0], ids[1], ids[2]);
        let t0 = Instant::now();

        // Run it repeatedly: HashMap iteration order varies per process AND
        // per insertion history, so a single pass could pass by luck.
        for _ in 0..8 {
            let mut st = ArbiterState::default();
            st.open(holder, "H".into(), true, Some(Mode::Exclusive), t0);
            st.open(high, "High".into(), true, None, t0);
            st.open(low, "Low".into(), true, None, t0);
            assert_eq!(st.snapshot().holder, Some(holder));
            st.close(holder);
            assert_eq!(
                st.snapshot().holder,
                Some(low),
                "the lowest surviving INPUT session must take the floor"
            );
        }
    }

    /// FR-92 — keep-busy is input: it needs the INPUT grant, the floor in
    /// exclusive mode, and an org that allows it.
    #[test]
    fn keep_busy_needs_the_input_grant_the_floor_and_the_org() {
        let mut st = ArbiterState::default();
        let (a, b, viewer) = (sid(), sid(), sid());
        let now = Instant::now();
        st.open(a, "A".into(), true, Some(Mode::Exclusive), now);
        st.open(b, "B".into(), true, None, now);
        st.open(viewer, "V".into(), false, None, now);
        assert_eq!(st.keep_busy_may_set(a), Ok("A".to_string()));
        assert_eq!(
            st.keep_busy_may_set(b),
            Err(KeepBusyRefusal::NotFloorHolder)
        );
        assert_eq!(st.keep_busy_may_set(viewer), Err(KeepBusyRefusal::NoInput));
        assert_eq!(st.keep_busy_may_set(sid()), Err(KeepBusyRefusal::Unknown));
        assert!(st.set_mode(a, Mode::Free));
        assert_eq!(st.keep_busy_may_set(b), Ok("B".to_string()));
    }

    /// FR-97 P3e — switching the host's keyboard layout changes what the person
    /// at it types, so it takes keep busy's rule. Before P3e every session's
    /// `rc:layout.set` reached the switch, a view-only one's included.
    #[test]
    fn a_layout_switch_needs_the_input_grant_and_the_floor() {
        let mut st = ArbiterState::default();
        let (a, b, viewer) = (sid(), sid(), sid());
        let now = Instant::now();
        st.open(a, "A".into(), true, Some(Mode::Exclusive), now);
        st.open(b, "B".into(), true, None, now);
        st.open(viewer, "V".into(), false, None, now);
        let switched = |st: &ArbiterState, s| {
            let mut got = None;
            layout_set_if_allowed(st, s, "the-token".into(), |t| got = Some(t));
            got
        };
        assert_eq!(switched(&st, a).as_deref(), Some("the-token"));
        assert_eq!(switched(&st, b), None, "not the floor holder");
        assert_eq!(switched(&st, viewer), None, "view-only");
        assert_eq!(switched(&st, sid()), None, "an unknown session");
        assert!(st.set_mode(a, Mode::Free));
        assert_eq!(switched(&st, b).as_deref(), Some("the-token"));
        assert_eq!(switched(&st, viewer), None, "view-only, in free mode too");
    }

    /// FR-92 — a keep-busy move spliced into a controller's drag would drag
    /// whatever they hold, so a held BUTTON blocks it. A held key does not.
    #[test]
    fn only_a_held_mouse_button_blocks_keep_busy_moves() {
        let mut st = ArbiterState::default();
        let a = sid();
        let now = Instant::now();
        st.open(a, "A".into(), true, None, now);
        assert!(!st.any_button_held());
        st.plan(a, &key(0x04, true), now);
        assert!(!st.any_button_held(), "a held key is not a drag");
        st.plan(a, &click(true), now);
        assert!(st.any_button_held());
        st.plan(a, &click(false), now);
        assert!(!st.any_button_held());
        st.plan(a, &click(true), now);
        let _ = st.close(a);
        assert!(!st.any_button_held(), "a closed session holds nothing");
    }

    /// FR-92 — why keep busy cannot run, from an injector the OS has not
    /// granted: no backend at all (a Noop) is `unsupported`; a backend that
    /// can read the pointer but may not move it is `no_permission`. (A Mac
    /// asks the OS first, because its enigo refuses to construct at all
    /// without Accessibility; that branch is field-verified, not reachable
    /// from here.)
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn an_unpermitted_injector_is_unsupported_only_when_there_is_no_backend() {
        struct Unpermitted;
        impl crate::input::InputInjector for Unpermitted {
            fn inject(&mut self, _event: crate::input::InputMsg) -> anyhow::Result<()> {
                Ok(())
            }
            fn has_permission(&self) -> bool {
                false
            }
            fn cursor_px(&mut self) -> anyhow::Result<(i32, i32)> {
                Ok((10, 10))
            }
        }
        assert!(matches!(
            classify_unpermitted(Box::new(crate::input::NoopInjector)),
            KbReply::Unsupported
        ));
        assert!(matches!(
            classify_unpermitted(Box::new(Unpermitted)),
            KbReply::NoPermission
        ));
    }

    /// An injector that records keep busy's normalised moves — uinput's
    /// shape: no pixel read, no pixel move.
    struct NormOnly(std::sync::Arc<std::sync::Mutex<Vec<(f64, f64)>>>);

    impl crate::input::InputInjector for NormOnly {
        fn inject(&mut self, _event: crate::input::InputMsg) -> anyhow::Result<()> {
            Ok(())
        }
        fn has_permission(&self) -> bool {
            true
        }
        fn move_norm(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
            self.0.lock().unwrap().push((x, y));
            Ok(())
        }
    }

    /// FR-92 P9 — the normalised move takes the same held-button rule as a
    /// pixel move (a GNOME/Wayland drag is still a drag), reaches the
    /// injector as a fraction of the desktop, clamps a value past the far
    /// edge, and a backend without it says so instead of failing.
    #[test]
    fn a_normalised_move_is_blocked_by_a_held_button_and_reaches_the_injector() {
        let moves = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut injector: Option<Box<dyn crate::input::InputInjector + Send>> =
            Some(Box::new(NormOnly(moves.clone())));
        let mut regrant = Regrant::default();
        let mut st = ArbiterState::default();
        let a = sid();
        let now = Instant::now();
        st.open(a, "A".into(), true, None, now);
        let half = KbOp::MoveNorm {
            x: NORM_ONE / 2,
            y: NORM_ONE / 4,
        };
        st.plan(a, &click(true), now);
        assert_eq!(
            keep_busy_op(&st, &mut injector, &mut regrant, half),
            KbReply::RemoteButtonHeld
        );
        assert!(moves.lock().unwrap().is_empty(), "nothing reached the OS");
        st.plan(a, &click(false), now);
        assert_eq!(
            keep_busy_op(&st, &mut injector, &mut regrant, half),
            KbReply::Moved
        );
        let past = KbOp::MoveNorm {
            x: NORM_ONE * 3,
            y: 0,
        };
        assert_eq!(
            keep_busy_op(&st, &mut injector, &mut regrant, past),
            KbReply::Moved
        );
        assert_eq!(*moves.lock().unwrap(), vec![(0.5, 0.25), (1.0, 0.0)]);
        // The pixel ops are not this backend's: unsupported, never "failed".
        assert_eq!(
            keep_busy_op(&st, &mut injector, &mut regrant, KbOp::Locate),
            KbReply::Unsupported
        );
        assert_eq!(
            keep_busy_op(&st, &mut injector, &mut regrant, KbOp::Move { x: 1, y: 1 }),
            KbReply::Unsupported
        );
    }

    /// #3164 — the Noop a session cached before the Accessibility grant is
    /// dropped once the OS grants it, so the next event opens a real injector.
    /// Master kept it for the life of the process (`get_or_insert_with` after
    /// a staleness check only): the viewer stopped warning while every typed
    /// key was still dropped.
    #[test]
    fn a_noop_cached_before_the_grant_is_opened_again_after_it() {
        let mut injector: Option<Box<dyn crate::input::InputInjector + Send>> =
            Some(Box::new(crate::input::NoopInjector));
        let mut regrant = Regrant::default();
        // Not granted yet: kept (and the OS was asked).
        let mut asked = false;
        refresh_with(&mut injector, &mut regrant, || {
            asked = true;
            false
        });
        assert!(asked);
        assert!(injector.is_some(), "no grant, nothing to reopen");
        // The person grants it; a new session opens, and its first event asks
        // again at once rather than waiting out the window.
        regrant.session_opened();
        refresh_with(&mut injector, &mut regrant, || true);
        assert!(
            injector.is_none(),
            "granted: dropped, so the next event opens the real injector"
        );
    }

    /// The OS is asked at most once a second while events stream in, and a
    /// permitted injector is never asked about or dropped.
    #[test]
    fn the_os_is_asked_about_a_grant_at_most_once_a_window() {
        let t0 = Instant::now();
        let mut r = Regrant::default();
        assert!(!r.reopen(false, t0, || false));
        assert!(
            !r.reopen(false, t0 + Duration::from_millis(500), || panic!(
                "asked inside the window"
            )),
            "inside the window: not asked"
        );
        assert!(
            r.reopen(false, t0 + Regrant::EVERY, || true),
            "a window later: asked"
        );
        let mut fresh = Regrant::default();
        assert!(!fresh.reopen(true, t0, || panic!(
            "a permitted injector is never re-asked"
        )));
        assert!(fresh.asked_at.is_none());
    }

    fn chord(code: u32, down: bool, mods: u8) -> InputMsg {
        InputMsg::Key { code, down, mods }
    }

    fn one_session() -> (ArbiterState, ObjectId, Instant) {
        let mut st = ArbiterState::default();
        let a = sid();
        let now = Instant::now();
        st.open(a, "A".into(), true, None, now);
        (st, a, now)
    }

    fn plain() -> EventPlan {
        EventPlan::Inject {
            pre: vec![],
            post: vec![],
        }
    }

    /// #3046 — a chord sent as one key plus `mods`, with no modifier frame:
    /// the soft keyboard's sticky Ctrl, a controller's one-key chord. The whole
    /// press goes out on the `down`, and nothing is left for a lost frame to
    /// strand: a session that dies right after it has nothing to release.
    #[test]
    fn a_chord_in_mods_is_tapped_whole_and_leaves_nothing_down() {
        let (mut st, a, now) = one_session();
        assert_eq!(
            st.plan(a, &chord(0x06, true, 0x01), now),
            EventPlan::Inject {
                pre: vec![key(0xe0, true)],
                post: vec![key(0x06, false), key(0xe0, false)],
            }
        );
        assert!(st.release_held(a).is_empty());
        assert!(st.close(a).is_empty());
    }

    /// The viewer's up frame for a tapped key presses nothing and releases no
    /// modifier: the tap already let go of both.
    #[test]
    fn the_up_frame_after_a_tap_is_only_itself() {
        let (mut st, a, now) = one_session();
        st.plan(a, &chord(0x06, true, 0x01), now);
        assert_eq!(st.plan(a, &chord(0x06, false, 0x01), now), plain());
    }

    /// A physical Ctrl is already down: pressing another would be a double
    /// press, and the tap's release would lift the one the person still holds.
    #[test]
    fn a_held_modifier_is_not_pressed_again() {
        let (mut st, a, now) = one_session();
        st.plan(a, &chord(0xe0, true, 0x01), now);
        assert_eq!(st.plan(a, &chord(0x06, true, 0x01), now), plain());
        assert_eq!(st.plan(a, &chord(0x06, false, 0x01), now), plain());
        assert_eq!(
            st.close(a),
            vec![key(0xe0, false)],
            "the person's Ctrl stays theirs"
        );
    }

    #[test]
    fn the_right_hand_key_answers_the_bit_too() {
        for (right, bit) in [(0xe4, 0x01), (0xe5, 0x02), (0xe6, 0x04), (0xe7, 0x08)] {
            let (mut st, a, now) = one_session();
            st.plan(a, &chord(right, true, bit), now);
            assert_eq!(st.plan(a, &chord(0x06, true, bit), now), plain());
        }
    }

    /// A modifier's own frame carries its own bit (`ev.ctrlKey` is true on
    /// Ctrl's keydown), and a second modifier carries the first's. Those are
    /// held like any key, never tapped.
    #[test]
    fn a_modifiers_own_frame_is_never_a_chord() {
        let (mut st, a, now) = one_session();
        assert_eq!(st.plan(a, &chord(0xe0, true, 0x01), now), plain());
        let (mut st, a, now) = one_session();
        assert_eq!(st.plan(a, &chord(0xe1, true, 0x03), now), plain());
        assert_eq!(st.close(a), vec![key(0xe1, false)]);
    }

    #[test]
    fn an_up_frame_never_taps() {
        let (mut st, a, now) = one_session();
        assert_eq!(st.plan(a, &chord(0x06, false, 0x0f), now), plain());
    }

    #[test]
    fn a_chord_of_several_presses_in_order_and_releases_in_reverse() {
        let (mut st, a, now) = one_session();
        assert_eq!(
            st.plan(a, &chord(0x06, true, 0x0f), now),
            EventPlan::Inject {
                pre: vec![
                    key(0xe0, true),
                    key(0xe2, true),
                    key(0xe1, true),
                    key(0xe3, true),
                ],
                post: vec![
                    key(0x06, false),
                    key(0xe3, false),
                    key(0xe1, false),
                    key(0xe2, false),
                    key(0xe0, false),
                ],
            }
        );
    }

    /// One bit held, one missing: only the missing one is pressed and let go,
    /// and the one the person holds stays down.
    #[test]
    fn only_the_missing_bit_is_pressed() {
        let (mut st, a, now) = one_session();
        st.plan(a, &chord(0xe0, true, 0x01), now);
        assert_eq!(
            st.plan(a, &chord(0x06, true, 0x03), now),
            EventPlan::Inject {
                pre: vec![key(0xe1, true)],
                post: vec![key(0x06, false), key(0xe1, false)],
            }
        );
        assert_eq!(st.close(a), vec![key(0xe0, false)]);
    }

    /// FR-13: a viewer rewrites Ctrl to Cmd for a Mac host in the code and not
    /// in `mods`, so there Cmd answers the Ctrl bit. Anywhere else it must not:
    /// Win held is not Ctrl held.
    #[test]
    fn a_held_cmd_answers_the_ctrl_bit_only_on_a_mac_host() {
        for cmd in [0xe3, 0xe7] {
            let held = HashSet::from([Held::Key(cmd)]);
            assert!(chord_tap(0x06, 0x01, &held, true).is_empty());
            assert_eq!(chord_tap(0x06, 0x01, &held, false), vec![0xe0]);
        }
        // Held Ctrl never answers the Meta bit, on a Mac host or anywhere.
        let held = HashSet::from([Held::Key(0xe0)]);
        assert_eq!(chord_tap(0x06, 0x08, &held, true), vec![0xe3]);
        assert_eq!(chord_tap(0x06, 0x08, &held, false), vec![0xe3]);
    }

    /// A key repeat from a chord with no modifier frame is one more whole tap.
    #[test]
    fn a_repeat_is_another_tap() {
        let (mut st, a, now) = one_session();
        let tap = EventPlan::Inject {
            pre: vec![key(0xe0, true)],
            post: vec![key(0x06, false), key(0xe0, false)],
        };
        assert_eq!(st.plan(a, &chord(0x06, true, 0x01), now), tap);
        assert_eq!(st.plan(a, &chord(0x06, true, 0x01), now), tap);
    }

    /// A key the session was holding, then seen again with a chord the session
    /// does not hold, ends released, so it can't be released a second time at
    /// the session's end.
    #[test]
    fn a_held_key_a_chord_taps_is_no_longer_held() {
        let (mut st, a, now) = one_session();
        st.plan(a, &key(0x06, true), now);
        st.plan(a, &chord(0x06, true, 0x01), now);
        assert!(st.close(a).is_empty());
    }

    #[test]
    fn a_denied_session_taps_nothing() {
        let now = Instant::now();
        let mut st = ArbiterState::default();
        let w = sid();
        st.open(w, "W".into(), false, None, now);
        assert_eq!(st.plan(w, &chord(0x06, true, 0x01), now), EventPlan::Deny);
    }

    /// The other session's modifier is let go before this one's is pressed.
    #[test]
    fn the_fence_comes_before_the_chords_press() {
        let (mut st, a, now) = one_session();
        let b = sid();
        st.open(b, "B".into(), true, None, now);
        st.plan(a, &chord(0xe0, true, 0x01), now);
        assert_eq!(
            st.plan(b, &chord(0x06, true, 0x01), now),
            EventPlan::Inject {
                pre: vec![key(0xe0, false), key(0xe0, true)],
                post: vec![key(0x06, false), key(0xe0, false)],
            }
        );
    }

    /// A viewer whose Ctrl was fenced away by another session still means
    /// Ctrl+C when it presses C: the bit is answered by what it holds now.
    #[test]
    fn a_modifier_fenced_away_is_tapped_back() {
        let (mut st, a, now) = one_session();
        let b = sid();
        st.open(b, "B".into(), true, None, now);
        st.plan(a, &chord(0xe0, true, 0x01), now);
        st.plan(b, &click(true), now);
        assert_eq!(
            st.plan(a, &chord(0x06, true, 0x01), now),
            EventPlan::Inject {
                pre: vec![key(0xe0, true)],
                post: vec![key(0x06, false), key(0xe0, false)],
            }
        );
    }

    #[derive(Clone, Default)]
    struct FailsOnLetters(Arc<std::sync::Mutex<Vec<InputMsg>>>);

    impl crate::input::InputInjector for FailsOnLetters {
        fn inject(&mut self, event: InputMsg) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(event.clone());
            match event {
                InputMsg::Key { code, .. } if !MODIFIER_CODES.contains(&code) => {
                    anyhow::bail!("no such key")
                }
                _ => Ok(()),
            }
        }
        fn has_permission(&self) -> bool {
            true
        }
    }

    /// ⚠️ What `pre` pressed is let go even when the key itself failed: the OS
    /// refusing one letter must not cost the host a stuck Ctrl.
    #[test]
    fn a_failed_key_still_lets_go_of_its_chord() {
        let sent = FailsOnLetters::default();
        let mut injector: Option<Box<dyn crate::input::InputInjector + Send>> =
            Some(Box::new(sent.clone()));
        inject_planned(
            &mut injector,
            &mut Regrant::default(),
            &[key(0xe0, true)],
            &[],
            &chord(0x06, true, 0x01),
            &[key(0x06, false), key(0xe0, false)],
            ObjectId::new(),
        );
        assert_eq!(
            *sent.0.lock().unwrap(),
            vec![
                key(0xe0, true),
                chord(0x06, true, 0x01),
                key(0x06, false),
                key(0xe0, false),
            ]
        );
    }

    /// #2066 — off macOS no grant is waited on, so a Noop a first event cached
    /// before a display or `/dev/uinput` existed is dropped, and the next event
    /// opens whatever can open now. Master kept it until a restart.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn off_macos_a_noop_is_opened_again() {
        assert!(crate::input::os_grants_input());
        let mut injector: Option<Box<dyn crate::input::InputInjector + Send>> =
            Some(Box::new(crate::input::NoopInjector));
        refresh(&mut injector, &mut Regrant::default());
        assert!(injector.is_none());
    }

    /// #2066 — a host that can never open one (a root daemon with no `DISPLAY`)
    /// pays one open, and one warning, a minute while events arrive, not one a
    /// second. A new session asks at once, from the shortest wait again.
    #[test]
    fn a_failing_open_backs_off_to_a_minute_and_a_session_starts_it_over() {
        let mut at = Instant::now();
        let mut r = Regrant::default();
        for secs in [1, 2, 4, 8, 16, 32, 60, 60, 60] {
            assert!(r.reopen(false, at, || true), "due after {at:?}");
            let next = at + Duration::from_secs(secs);
            assert!(
                !r.reopen(false, next - Duration::from_millis(1), || panic!(
                    "asked inside the wait of {secs} s"
                )),
                "inside the wait of {secs} s"
            );
            at = next;
        }
        r.session_opened();
        assert!(r.reopen(false, at, || true), "a new session asks at once");
        assert!(!r.reopen(false, at + Duration::from_millis(999), || panic!(
            "asked inside the shortest wait"
        )));
        assert!(r.reopen(false, at + Regrant::EVERY, || true));
    }

    /// An open that worked ends the back-off: an injector that later loses its
    /// permission starts from the shortest wait, not the longest.
    #[test]
    fn a_permitted_injector_ends_the_back_off() {
        let mut at = Instant::now();
        let mut r = Regrant::default();
        for _ in 0..8 {
            r.reopen(false, at, || true);
            at += Regrant::AT_MOST;
        }
        assert_eq!(r.wait(), Regrant::AT_MOST);
        assert!(!r.reopen(true, at, || panic!(
            "a permitted injector is never asked about"
        )));
        assert_eq!(r.wait(), Regrant::EVERY);
    }

    /// macOS's 1 s poll of a grant nobody has given yet must not slow down: the
    /// wait grows only with an open, never with a "no" from the OS.
    #[test]
    fn a_grant_not_given_keeps_the_shortest_wait() {
        let mut at = Instant::now();
        let mut r = Regrant::default();
        for _ in 0..4 {
            assert!(!r.reopen(false, at, || false));
            assert_eq!(r.wait(), Regrant::EVERY);
            at += Regrant::EVERY;
        }
    }

    /// #3918 — what a test sets as the arbiter's lazy open: it makes the
    /// stand-in the arbiter opens.
    pub(super) type StandInOpen = Box<dyn FnMut() -> Box<dyn crate::input::InputInjector + Send>>;

    thread_local! {
        /// #3918 — the arbiter's lazy open ([`open_injector`]) on this thread,
        /// when a test set one: it opens the test's stand-in.
        pub(super) static STAND_IN_OPEN: std::cell::RefCell<Option<StandInOpen>> =
            const { std::cell::RefCell::new(None) };
        /// #4069 — `Some(on)` answers [`warm_on`] on this thread, as
        /// `ROOMLERD_INPUT_WARM` would: a test process cannot set the switch
        /// off for itself alone.
        pub(super) static WARM_HERE: std::cell::Cell<Option<bool>> =
            const { std::cell::Cell::new(None) };
    }

    /// #3918 — what reached one stand-in X11 injector, and whether it was told
    /// its X server is gone.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[derive(Clone, Default)]
    struct Seen {
        generation: Option<u64>,
        events: std::sync::Arc<std::sync::Mutex<Vec<InputMsg>>>,
        moves: std::sync::Arc<std::sync::Mutex<Vec<(f64, f64)>>>,
        gone: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    impl Seen {
        fn events(&self) -> Vec<InputMsg> {
            self.events.lock().unwrap().clone()
        }
        fn moves(&self) -> Vec<(f64, f64)> {
            self.moves.lock().unwrap().clone()
        }
        fn gone(&self) -> bool {
            self.gone.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// #3918 — the X11 injector as the arbiter sees it: it records the
    /// desktop's generation as it opens, as `EnigoInjector::new` does.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    struct OnX(Seen);

    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    impl crate::input::InputInjector for OnX {
        fn inject(&mut self, event: InputMsg) -> anyhow::Result<()> {
            self.0.events.lock().unwrap().push(event);
            Ok(())
        }
        fn has_permission(&self) -> bool {
            true
        }
        fn move_norm(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
            self.0.moves.lock().unwrap().push((x, y));
            Ok(())
        }
        fn x_generation(&self) -> Option<u64> {
            self.0.generation
        }
        fn server_gone(&mut self) {
            self.0.gone.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// #3918 — every stand-in the arbiter opened on this thread, in order.
    /// `x11`: whether they are X11 injectors of the desktop (they record its
    /// generation) or any other kind (they record none).
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    fn stand_ins(x11: bool) -> std::sync::Arc<std::sync::Mutex<Vec<Seen>>> {
        // No automatic capture of another test is alive here
        // (`x11_would_open`, `stale`), and none is planned (#4069's wait).
        #[cfg(feature = "drm-capture")]
        crate::capture::seat_guard::LIVE_HERE.with(|l| l.set(Some(false)));
        #[cfg(all(feature = "uinput-input", feature = "drm-capture"))]
        crate::input::PLANNED_HERE.with(|p| p.set(Some(false)));
        let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Seen>::new()));
        let list = opened.clone();
        STAND_IN_OPEN.with(|o| {
            *o.borrow_mut() = Some(Box::new(move || {
                let seen = Seen {
                    generation: if x11 {
                        crate::input::x_follow::opened_under()
                    } else {
                        None
                    },
                    ..Seen::default()
                };
                list.lock().unwrap().push(seen.clone());
                Box::new(OnX(seen))
            }))
        });
        opened
    }

    /// #3918 — the stand-ins and the stand-in desktop off this thread.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    fn no_stand_ins() {
        STAND_IN_OPEN.with(|o| *o.borrow_mut() = None);
        crate::input::x_follow::test_seam::clear();
        #[cfg(feature = "drm-capture")]
        crate::capture::seat_guard::LIVE_HERE.with(|l| l.set(None));
        #[cfg(all(feature = "uinput-input", feature = "drm-capture"))]
        crate::input::PLANNED_HERE.with(|p| p.set(None));
        WARM_HERE.with(|w| w.set(None));
    }

    /// A runtime handle for [`run_command`]. Nothing a test runs spawns on
    /// it: a command starts with no sinks (an Open adds its own and spawns
    /// nothing), and its tells go to a recorder.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    fn idle_handle() -> tokio::runtime::Handle {
        static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RT.get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("a test runtime")
        })
        .handle()
        .clone()
    }

    /// One command through the worker's own code ([`run_command`]), with no
    /// sinks at its start: the states it tells the viewers, in order. A test
    /// that injects sets its stand-ins first (`stand_ins`): the arbiter's
    /// lazy open would open this machine's injector otherwise.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    fn command(
        st: &mut ArbiterState,
        follow: &mut Follow,
        injector: &mut Option<Box<dyn crate::input::InputInjector + Send>>,
        regrant: &mut Regrant,
        cmd: Cmd,
    ) -> Vec<Snapshot> {
        command_probed(st, follow, injector, regrant, cmd, || ())
            .into_iter()
            .map(|(state, _, ())| state)
            .collect()
    }

    /// [`command`], with `probe` read at each tell: what the stand-ins had
    /// opened and been sent then says where in its work the command told.
    /// Each tell comes with the sessions it reaches, sorted: those whose
    /// sinks are in the map then. A command starts with no
    /// sinks, and an Open adds its own.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    fn command_probed<T>(
        st: &mut ArbiterState,
        follow: &mut Follow,
        injector: &mut Option<Box<dyn crate::input::InputInjector + Send>>,
        regrant: &mut Regrant,
        cmd: Cmd,
        mut probe: impl FnMut() -> T,
    ) -> Vec<(Snapshot, Vec<ObjectId>, T)> {
        let mut sinks = HashMap::new();
        let mut told = Vec::new();
        let parts = Parts {
            state: st,
            sinks: &mut sinks,
            injector,
            regrant,
            follow,
        };
        run_command(parts, cmd, &idle_handle(), |state, sinks| {
            let mut to: Vec<ObjectId> = sinks.keys().copied().collect();
            to.sort();
            told.push((state.snapshot(), to, probe()));
        });
        told
    }

    /// #3918 — a session opens as `Cmd::Open` registers it.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    fn open_session(st: &mut ArbiterState, session: ObjectId) {
        st.open(session, "viewer".into(), true, None, Instant::now());
    }

    /// #3918 — what the worker does before every command, run through the
    /// worker's own code in a command that does nothing by itself (a release
    /// of nobody's keys): every marked session whose input would reach the
    /// desktop's X server is left behind. `true` when the viewers were told.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    fn before_each(
        st: &mut ArbiterState,
        follow: &mut Follow,
        injector: &mut Option<Box<dyn crate::input::InputInjector + Send>>,
        regrant: &mut Regrant,
    ) -> bool {
        let nobody = Cmd::ReleaseHeld { session: sid() };
        !command(st, follow, injector, regrant, nobody).is_empty()
    }

    /// #3918 — the supervisor's tell, or a session's refused open, marks
    /// `session` as having no picture of the desktop: what the daemon does
    /// with the reason both of them carry.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    fn told_the_desktop_is_down(session: ObjectId) {
        let reason = crate::capture::CaptureUnavailable::virtual_desktop_down(
            "this device's virtual desktop is down: its X server (Xvfb) exited",
        );
        crate::input::told_no_picture(session, &reason);
    }

    /// #3918 — one controller event, through the worker's own `Cmd::Event`:
    /// the states told the viewers, in order.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    fn session_event(
        st: &mut ArbiterState,
        follow: &mut Follow,
        session: ObjectId,
        msg: InputMsg,
        injector: &mut Option<Box<dyn crate::input::InputInjector + Send>>,
        regrant: &mut Regrant,
    ) -> Vec<Snapshot> {
        command(st, follow, injector, regrant, Cmd::Event { session, msg })
    }

    /// #3918 — a session's end, through the worker's own `Cmd::Close`: what
    /// it holds is released, under its mark, and the mark goes.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    fn close_session(
        st: &mut ArbiterState,
        follow: &mut Follow,
        session: ObjectId,
        injector: &mut Option<Box<dyn crate::input::InputInjector + Send>>,
        regrant: &mut Regrant,
    ) -> Vec<Snapshot> {
        command(st, follow, injector, regrant, Cmd::Close { session })
    }

    /// #3918 — a cached injector that is no X client of the desktop
    /// and has no permission, as a `NoopInjector` is: the regrant opens the
    /// injector again at the next event (#3164).
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    struct NoGrant;

    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    impl crate::input::InputInjector for NoGrant {
        fn inject(&mut self, _: InputMsg) -> anyhow::Result<()> {
            Ok(())
        }
        fn has_permission(&self) -> bool {
            false
        }
    }

    /// #3918 — a mark changes nothing for input that a portal
    /// or mutter route carries while the route stays live. A, marked at the X
    /// server exit while a route takes every event, is not left behind: its
    /// events reach the route, and it keeps the floor, what it holds, and its
    /// keep busy and layout switches. (A command in a moment with no route
    /// leaves it behind, and the floor it gives up is not given back.)
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_marked_sessions_input_through_a_route_reaches_it_and_keeps_its_floor() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        let a = sid();
        desk::up(0);
        desk::route(true);
        st.open(a, "A".into(), true, Some(Mode::Exclusive), Instant::now());
        session_event(
            &mut st,
            &mut follow,
            a,
            key(0x04, true),
            &mut injector,
            &mut regrant,
        );
        desk::down(0, 1);
        told_the_desktop_is_down(a);
        desk::up(1);
        session_event(
            &mut st,
            &mut follow,
            a,
            key(0x05, true),
            &mut injector,
            &mut regrant,
        );
        let left = st.is_left_behind(a);
        let holder = st.snapshot().holder;
        let mut switched = false;
        layout_set_if_allowed(&st, a, "de".into(), |_| switched = true);
        let keep_busy = st.keep_busy_may_set(a);
        let routed = desk::routed_events();
        let held = st.release_held(a);
        follow.closed(a);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert!(!left, "A was left behind while a route carried its input");
        assert_eq!(routed, vec![key(0x04, true), key(0x05, true)]);
        assert!(opened.is_empty(), "an X client of the desktop was opened");
        assert_eq!(holder, Some(a), "A gave up the floor");
        assert!(switched, "A's layout switch was refused");
        assert_eq!(keep_busy, Ok("A".to_string()));
        assert_eq!(held.len(), 2, "A's held keys were forgotten: {held:?}");
    }

    /// #3918 — the same, over time: the route goes, then comes back. A,
    /// marked, is left behind only while no route takes its input: then its
    /// key would reach the desktop's X server, and reaches nothing, while an
    /// unmarked B's does. Once a route takes the input again, A is taken back
    /// and its key reaches the route, as on master.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_marked_session_is_left_behind_only_while_no_route_takes_its_input() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        let (a, b) = (sid(), sid());
        desk::up(0);
        desk::route(true);
        st.open(a, "A".into(), true, None, Instant::now());
        st.open(b, "B".into(), true, None, Instant::now());
        desk::down(0, 1);
        told_the_desktop_is_down(a);
        desk::up(1);
        session_event(
            &mut st,
            &mut follow,
            a,
            key(0x04, true),
            &mut injector,
            &mut regrant,
        );
        let first = desk::routed_events();
        // The route goes: its capture reopens, or its owner left.
        desk::route(false);
        session_event(
            &mut st,
            &mut follow,
            a,
            key(0x05, true),
            &mut injector,
            &mut regrant,
        );
        let gone = st.is_left_behind(a);
        session_event(
            &mut st,
            &mut follow,
            b,
            key(0x06, true),
            &mut injector,
            &mut regrant,
        );
        // It comes back.
        desk::route(true);
        session_event(
            &mut st,
            &mut follow,
            a,
            key(0x07, true),
            &mut injector,
            &mut regrant,
        );
        let back = (st.is_left_behind(a), desk::routed_events());
        follow.closed(a);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert_eq!(first, vec![key(0x04, true)]);
        assert!(gone, "A was not left behind with no route");
        assert_eq!(opened.len(), 1);
        assert_eq!(
            opened[0].events(),
            vec![key(0x06, true)],
            "A's key reached the X client"
        );
        assert_eq!(
            back,
            (false, vec![key(0x07, true)]),
            "A did not reach the route again"
        );
    }

    /// #3918 — a marked session does not hold the floor against a
    /// live one. A took the floor while a route carried its input; the route
    /// goes, and the next command, B's floor request, leaves A behind first:
    /// the floor is free, and B's key reaches the desktop.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_marked_session_holds_no_floor_against_a_live_one_once_its_input_would_reach_x() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        let (a, b) = (sid(), sid());
        desk::up(0);
        desk::route(true);
        st.open(a, "A".into(), true, Some(Mode::Exclusive), Instant::now());
        st.open(b, "B".into(), true, None, Instant::now());
        desk::down(0, 1);
        told_the_desktop_is_down(a);
        desk::up(1);
        session_event(
            &mut st,
            &mut follow,
            a,
            key(0x04, true),
            &mut injector,
            &mut regrant,
        );
        let a_held = st.snapshot().holder;
        desk::route(false);
        let moved = before_each(&mut st, &mut follow, &mut injector, &mut regrant);
        let (granted, releases) = st.request_floor(b, Instant::now());
        session_event(
            &mut st,
            &mut follow,
            b,
            key(0x05, true),
            &mut injector,
            &mut regrant,
        );
        follow.closed(a);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert_eq!(a_held, Some(a));
        assert!(moved, "the viewers were not told that the floor moved");
        assert_eq!(
            (granted, releases),
            (true, Vec::new()),
            "A's floor or keys stood"
        );
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].events(), vec![key(0x05, true)]);
    }

    /// #3918 — the start of a command decides from the
    /// injector cached then, and the same command can swap it: a cached
    /// injector without permission is opened again (#3164), here as an X
    /// client of the new server. A marked session's key reaches no X client
    /// all the same, and the next command leaves the session behind.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_marked_sessions_key_reaches_no_x_client_its_own_command_opened() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut regrant = Regrant::default();
        let (a, b) = (sid(), sid());
        desk::up(0);
        st.open(a, "A".into(), true, None, Instant::now());
        st.open(b, "B".into(), true, None, Instant::now());
        desk::down(0, 1);
        told_the_desktop_is_down(a);
        desk::up(1);
        let mut injector: Option<Box<dyn crate::input::InputInjector + Send>> =
            Some(Box::new(NoGrant));
        session_event(
            &mut st,
            &mut follow,
            a,
            key(0x04, true),
            &mut injector,
            &mut regrant,
        );
        let a_left_at_its_command = st.is_left_behind(a);
        session_event(
            &mut st,
            &mut follow,
            b,
            key(0x05, true),
            &mut injector,
            &mut regrant,
        );
        let a_left_after = st.is_left_behind(a);
        follow.closed(a);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert!(
            !a_left_at_its_command,
            "the cached injector was read as the desktop's"
        );
        assert_eq!(opened.len(), 1, "A's command did not open the X client");
        assert_eq!(
            opened[0].events(),
            vec![key(0x05, true)],
            "A's key reached the X client"
        );
        assert!(a_left_after, "the next command did not leave A behind");
    }

    /// #3918 — at the injection point only the
    /// injector in hand decides. A marked session's key reaches no X client
    /// of the desktop, wherever the start of the command thought the input
    /// would go (a configured uinput that does not open lands on the X11
    /// injector), and reaches any other injector, as on master.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn at_the_injection_point_only_an_x_client_of_the_desktop_refuses_a_marked_key() {
        use crate::input::x_follow::test_seam as desk;
        let a = sid();
        desk::up(1);
        told_the_desktop_is_down(a);
        let x = stand_ins(true);
        inject_all_of(
            &mut None,
            &mut Regrant::default(),
            &[key(0x04, true)],
            Some(a),
        );
        let x = x.lock().unwrap().clone();
        let other = stand_ins(false);
        inject_all_of(
            &mut None,
            &mut Regrant::default(),
            &[key(0x05, true)],
            Some(a),
        );
        let other = other.lock().unwrap().clone();
        Follow::default().closed(a);
        no_stand_ins();
        assert_eq!(x.len(), 1, "no X client was opened");
        assert!(
            x[0].events().is_empty(),
            "the marked key reached the X client"
        );
        assert_eq!(other.len(), 1);
        assert_eq!(
            other[0].events(),
            vec![key(0x05, true)],
            "the other injector got nothing"
        );
    }

    /// #3918 — a marked session acts in the one-command
    /// window (the cached injector lost its grant, so the start of the
    /// command read no X client, and the regrant opens one on the new
    /// server), and the fence lets go of the other sessions' held modifiers.
    /// The release is B's input: B's Ctrl, which the arbiter no longer holds
    /// for B, is released on the desktop instead of staying down under every
    /// later controller's keys. A's own key reaches no X client.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_fence_release_is_its_owners_input_not_the_marked_actors() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut regrant = Regrant::default();
        let (a, b) = (sid(), sid());
        desk::up(0);
        open_session(&mut st, a);
        open_session(&mut st, b);
        desk::down(0, 1);
        told_the_desktop_is_down(a);
        desk::up(1);
        // B holds Left-Ctrl, as the arbiter planned its press.
        assert!(matches!(
            st.plan(b, &key(0xe0, true), Instant::now()),
            EventPlan::Inject { .. }
        ));
        let mut injector: Option<Box<dyn crate::input::InputInjector + Send>> =
            Some(Box::new(NoGrant));
        session_event(
            &mut st,
            &mut follow,
            a,
            key(0x04, true),
            &mut injector,
            &mut regrant,
        );
        let a_left_at_its_command = st.is_left_behind(a);
        follow.closed(a);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert!(
            !a_left_at_its_command,
            "the cached injector was read as the desktop's"
        );
        assert_eq!(opened.len(), 1, "A's command did not open the X client");
        assert_eq!(
            opened[0].events(),
            vec![key(0xe0, false)],
            "B's Ctrl stayed down on the desktop, or A's key reached it"
        );
    }

    /// #3918 — a fence release of a marked session's modifier
    /// is offered under that session's mark: no X client of the desktop gets
    /// it (its press reached none), while the unmarked actor's own key does.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_marked_owners_fence_release_reaches_no_x_client() {
        use crate::input::x_follow::test_seam as desk;
        let (owner, actor) = (sid(), sid());
        desk::up(1);
        told_the_desktop_is_down(owner);
        let opened = stand_ins(true);
        inject_planned(
            &mut None,
            &mut Regrant::default(),
            &[key(0xe1, false)],
            &[owner],
            &key(0x05, true),
            &[],
            actor,
        );
        Follow::default().closed(owner);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert_eq!(opened.len(), 1);
        assert_eq!(
            opened[0].events(),
            vec![key(0x05, true)],
            "the marked owner's release reached the X client, or the actor's key did not"
        );
    }

    /// #3918 — the viewers are told, once, in the command
    /// whose event takes a free floor. Exclusive mode: A holds the floor, the
    /// X server exits, A is marked. With an X client of the new server
    /// cached, B's key leaves A behind first (the floor given up), then takes
    /// the free floor ("first event wins"): one state is told, B holding the
    /// floor, before B's key is injected. Without a tell in the event's arm every
    /// viewer went on showing nobody holding the floor while B held it, until
    /// another command broadcast; with a tell per change the command told two
    /// states, which could reach a viewer in either order.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn the_viewers_are_told_when_an_event_takes_a_free_floor() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut regrant = Regrant::default();
        let (a, b) = (sid(), sid());
        desk::up(0);
        st.open(a, "A".into(), true, Some(Mode::Exclusive), Instant::now());
        st.open(b, "B".into(), true, None, Instant::now());
        let a_held = st.snapshot().holder;
        desk::down(0, 1);
        told_the_desktop_is_down(a);
        desk::up(1);
        let mut injector = Some(open_injector());
        let sent = || opened.lock().unwrap()[0].events().len();
        let told = command_probed(
            &mut st,
            &mut follow,
            &mut injector,
            &mut regrant,
            Cmd::Event {
                session: b,
                msg: key(0x05, true),
            },
            &sent,
        );
        let b_holds = st.snapshot().holder;
        follow.closed(a);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert_eq!(a_held, Some(a));
        assert_eq!(b_holds, Some(b), "B's key did not take the free floor");
        let told: Vec<_> = told.iter().map(|(s, _, sent)| (s.holder, *sent)).collect();
        assert_eq!(
            told,
            vec![(Some(b), 0)],
            "the command did not tell once, B holding the floor, before B's key was injected"
        );
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].events(), vec![key(0x05, true)]);
    }

    /// #3918 — a command whose event is dropped still tells what
    /// its start changed. The set-up of
    /// `the_viewers_are_told_when_an_event_takes_a_free_floor`; A's own key
    /// leaves A behind (the floor given up) and is dropped: one state is told,
    /// nobody holding the floor, and nothing reaches the X client.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_dropped_event_still_tells_what_its_command_changed() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut regrant = Regrant::default();
        let (a, b) = (sid(), sid());
        desk::up(0);
        st.open(a, "A".into(), true, Some(Mode::Exclusive), Instant::now());
        st.open(b, "B".into(), true, None, Instant::now());
        desk::down(0, 1);
        told_the_desktop_is_down(a);
        desk::up(1);
        let mut injector = Some(open_injector());
        let told = session_event(
            &mut st,
            &mut follow,
            a,
            key(0x05, true),
            &mut injector,
            &mut regrant,
        );
        follow.closed(a);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        let holders: Vec<_> = told.iter().map(|s| s.holder).collect();
        assert_eq!(
            holders,
            vec![None],
            "the dropped event's command did not tell, once, that A gave the floor up"
        );
        assert_eq!(opened.len(), 1);
        assert_eq!(
            opened[0].events(),
            Vec::new(),
            "A's key reached the X client"
        );
    }

    /// #3918 — a command whose arm has no reason of its own to
    /// tell still tells what its start changed, once: at its arm's tell point
    /// when it has one (an event that moves no floor, a view-only session's,
    /// after its plan and before its injection; a withdrawal of no request,
    /// where a withdrawal tells), else at the command's end (a release of
    /// held input, keep busy's read, a refused keep-busy or layout switch, an
    /// automatic capture's live edge).
    /// Each in the set-up of
    /// `a_dropped_event_still_tells_what_its_command_changed`, with a
    /// view-only V for B; the refusals are A's, which is left behind. A holds
    /// a key it pressed on the X server that exited: no
    /// command releases it on either server's X client, and the leave-behind
    /// forgets it.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_command_that_tells_nothing_still_tells_what_its_start_changed() {
        use crate::input::x_follow::test_seam as desk;
        // A command of A's or V's.
        type MakeCmd = fn(ObjectId, ObjectId) -> Cmd;
        let commands: [(&str, MakeCmd); 6] = [
            ("ReleaseHeld", |a, _| Cmd::ReleaseHeld { session: a }),
            ("KeepBusy", |_, _| {
                Cmd::KeepBusy(KbReq {
                    op: KbOp::Locate,
                    reply: std::sync::mpsc::sync_channel(1).0,
                })
            }),
            ("KeepBusySet", |a, _| Cmd::KeepBusySet {
                session: a,
                request: crate::keep_busy::SetRequest {
                    on: false,
                    settings: Default::default(),
                },
            }),
            ("LayoutSet", |a, _| Cmd::LayoutSet {
                session: a,
                token: "us".into(),
            }),
            ("an event that moves no floor", |_, v| Cmd::Event {
                session: v,
                msg: key(0x05, true),
            }),
            ("ClearFloorRequest", |_, v| Cmd::ClearFloorRequest {
                session: v,
            }),
        ];
        // #4069 — the live edge warms for a session that may input, and
        // neither A, left behind, nor V may.
        #[cfg(feature = "drm-capture")]
        let commands = {
            let mut all = commands.to_vec();
            all.push(("CaptureLive", |_, _| Cmd::CaptureLive));
            all
        };
        for (name, cmd) in commands {
            let opened = stand_ins(true);
            let mut st = ArbiterState::default();
            let mut follow = Follow::default();
            let mut regrant = Regrant::default();
            let (a, v) = (sid(), sid());
            desk::up(0);
            st.open(a, "A".into(), true, Some(Mode::Exclusive), Instant::now());
            st.open(v, "V".into(), false, None, Instant::now());
            // A presses a key through an X client of the server that exits.
            let mut injector = Some(open_injector());
            let press = Cmd::Event {
                session: a,
                msg: key(0x04, true),
            };
            command(&mut st, &mut follow, &mut injector, &mut regrant, press);
            desk::down(0, 1);
            told_the_desktop_is_down(a);
            desk::up(1);
            let mut injector = Some(open_injector());
            let told = command(&mut st, &mut follow, &mut injector, &mut regrant, cmd(a, v));
            let a_holds = st.sessions.get(&a).is_some_and(|c| !c.held.is_empty());
            follow.closed(a);
            let sent: Vec<_> = opened.lock().unwrap().iter().map(|s| s.events()).collect();
            no_stand_ins();
            let holders: Vec<_> = told.iter().map(|s| s.holder).collect();
            assert_eq!(
                holders,
                vec![None],
                "{name}: the command did not tell, once, that A gave the floor up"
            );
            // The old server's X client has A's press alone, the new one's
            // nothing: A's key is released on neither.
            assert_eq!(
                sent,
                vec![vec![key(0x04, true)], Vec::new()],
                "{name}: something reached an X client"
            );
            assert!(!a_holds, "{name}: A's held key was not forgotten");
        }
    }

    /// #3918 — with no session marked, every command tells the viewers at the
    /// same place in its work as when each arm told for itself:
    /// Open, once its session's sink is in the map, so the new session is
    /// told too; ControlReady before its warm open; RequestFloor and
    /// GrantFloor after what they release; a ClearFloorRequest that clears;
    /// SetMode; and a Close that leaves a session, after what it releases (B
    /// holds a key when it closes). An event that moves no floor, a refused
    /// one, ReleaseHeld, KeepBusy, a refused KeepBusySet or LayoutSet, a
    /// ClearFloorRequest that clears nothing, a live edge (#4069) and the last
    /// Close tell nothing.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn with_no_mark_every_command_tells_as_before() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(false);
        desk::up(0);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        let (a, b, v) = (sid(), sid(), sid());
        // How many stand-ins were open, and what they had been sent in all,
        // when a state was told.
        let at = || {
            let o = opened.lock().unwrap();
            (o.len(), o.iter().map(|s| s.events().len()).sum::<usize>())
        };
        let open = |session, can_input, mode_hint| Cmd::Open {
            session,
            name: "viewer".into(),
            can_input,
            mode_hint,
            control: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            cursor: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        };
        let fresh = |st: &mut ArbiterState, s: ObjectId| {
            st.sessions.get_mut(&s).expect("open").last_input = Instant::now();
        };
        macro_rules! run {
            ($cmd:expr) => {
                command_probed(&mut st, &mut follow, &mut injector, &mut regrant, $cmd, &at)
                    .into_iter()
                    .map(|(s, _, at)| (s.holder, at))
                    .collect::<Vec<_>>()
            };
        }
        // `run!`, with the sessions each tell reaches.
        macro_rules! run_to {
            ($cmd:expr) => {
                command_probed(&mut st, &mut follow, &mut injector, &mut regrant, $cmd, &at)
                    .into_iter()
                    .map(|(s, to, at)| (s.holder, to, at))
                    .collect::<Vec<_>>()
            };
        }
        let none: Vec<(Option<ObjectId>, (usize, usize))> = Vec::new();
        assert_eq!(
            run_to!(open(a, true, Some(Mode::Exclusive))),
            vec![(Some(a), vec![a], (0, 0))],
            "Open, its own sink in the map"
        );
        assert_eq!(
            run_to!(open(b, true, None)),
            vec![(Some(a), vec![b], (0, 0))],
            "Open, its own sink in the map"
        );
        assert_eq!(
            run_to!(open(v, false, None)),
            vec![(Some(a), vec![v], (0, 0))],
            "Open, its own sink in the map"
        );
        assert_eq!(
            run!(Cmd::ControlReady { session: a }),
            vec![(Some(a), (0, 0))],
            "ControlReady"
        );
        assert_eq!(at(), (1, 0), "ControlReady's warm open");
        #[cfg(feature = "drm-capture")]
        {
            assert_eq!(run!(Cmd::CaptureLive), none, "a live edge");
            assert_eq!(at(), (1, 0), "a live edge with an injector that fits");
        }
        let a_key = Cmd::Event {
            session: a,
            msg: key(0x04, true),
        };
        assert_eq!(run!(a_key), none, "an event that moves no floor");
        assert_eq!(at(), (1, 1));
        // B asks while A is idle: A's key is released, then the state told.
        st.sessions.get_mut(&a).expect("open").last_input =
            crate::clock::instant_before(IDLE_TAKEOVER + Duration::from_secs(1));
        assert_eq!(
            run!(Cmd::RequestFloor { session: b }),
            vec![(Some(b), (1, 2))],
            "RequestFloor, a takeover"
        );
        fresh(&mut st, b);
        assert_eq!(
            run!(Cmd::RequestFloor { session: a }),
            vec![(Some(b), (1, 2))],
            "RequestFloor, which waits"
        );
        assert_eq!(
            run!(Cmd::ClearFloorRequest { session: v }),
            none,
            "a ClearFloorRequest that clears nothing"
        );
        assert_eq!(
            run!(Cmd::ClearFloorRequest { session: a }),
            vec![(Some(b), (1, 2))],
            "a ClearFloorRequest that clears"
        );
        fresh(&mut st, b);
        assert_eq!(
            run!(Cmd::RequestFloor { session: a }).len(),
            1,
            "RequestFloor"
        );
        let b_key = Cmd::Event {
            session: b,
            msg: key(0x06, true),
        };
        assert_eq!(run!(b_key), none, "the holder's event");
        assert_eq!(at(), (1, 3));
        assert_eq!(
            run!(Cmd::GrantFloor { holder: b, to: a }),
            vec![(Some(a), (1, 4))],
            "GrantFloor: B's key released, then told"
        );
        let v_key = Cmd::Event {
            session: v,
            msg: key(0x07, true),
        };
        assert_eq!(run!(v_key), none, "a refused event");
        assert_eq!(run!(Cmd::ReleaseHeld { session: a }), none, "ReleaseHeld");
        let locate = Cmd::KeepBusy(KbReq {
            op: KbOp::Locate,
            reply: std::sync::mpsc::sync_channel(1).0,
        });
        assert_eq!(run!(locate), none, "KeepBusy");
        let keep_busy = Cmd::KeepBusySet {
            session: v,
            request: crate::keep_busy::SetRequest {
                on: false,
                settings: Default::default(),
            },
        };
        assert_eq!(run!(keep_busy), none, "a refused KeepBusySet");
        let layout = Cmd::LayoutSet {
            session: v,
            token: "us".into(),
        };
        assert_eq!(run!(layout), none, "a refused LayoutSet");
        assert_eq!(at(), (1, 4), "something reached the stand-in");
        let mode = run!(Cmd::SetMode {
            session: a,
            mode: Mode::Free,
        });
        assert_eq!(mode, vec![(st.snapshot().holder, (1, 4))], "SetMode");
        // In free mode B's key is injected and held, and moves no floor.
        let b_holds = Cmd::Event {
            session: b,
            msg: key(0x08, true),
        };
        assert_eq!(run!(b_holds), none, "an event in free mode");
        assert_eq!(at(), (1, 5));
        let close_v = run!(Cmd::Close { session: v });
        assert_eq!(
            close_v,
            vec![(st.snapshot().holder, (1, 5))],
            "a Close that leaves a session"
        );
        let close_b = run!(Cmd::Close { session: b });
        assert_eq!(
            close_b,
            vec![(st.snapshot().holder, (1, 6))],
            "a Close that leaves a session: B's key released, then told"
        );
        assert_eq!(run!(Cmd::Close { session: a }), none, "the last Close");
        no_stand_ins();
    }

    /// #3918, the rule — a session marked as having no picture of the
    /// desktop has no input of it while its input would reach the desktop's
    /// X server (here, with the X11 injector, for as long as it is open: the
    /// mark stays until it closes). A, open across a
    /// restart, pressed a key and a button on the first server, and the tell
    /// named it. After the restart its keys and clicks reach nothing, what it
    /// held is forgotten (keep busy is not held off by its button, and its
    /// close releases nothing), and that is said once, at INFO; its close
    /// forgets its mark. B, opened after the restart and never told, types
    /// into the new server through an injector opened on it. Before the rule,
    /// A's input followed the injector to a desktop A cannot see; before
    /// #3918 every event went to the dead server.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_session_the_daemon_told_reaches_nothing_and_one_opened_after_types() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        let (a, b) = (sid(), sid());
        desk::up(0);
        open_session(&mut st, a);
        let (mut button_held, mut held_at_close) = (true, usize::MAX);
        let lines = desk::logged(|| {
            let (st, follow, inj, re) = (&mut st, &mut follow, &mut injector, &mut regrant);
            session_event(st, follow, a, key(0x04, true), inj, re);
            session_event(st, follow, a, click(true), inj, re);
            // The X server exits, the tell names A, and the supervisor brings
            // a new server up.
            desk::down(0, 1);
            told_the_desktop_is_down(a);
            desk::up(1);
            // Keep busy reads the remote buttons before it moves (`Cmd::KeepBusy`).
            before_each(st, follow, inj, re);
            button_held = st.any_button_held();
            session_event(st, follow, a, key(0x04, false), inj, re);
            session_event(st, follow, a, click(false), inj, re);
            session_event(st, follow, a, key(0x05, true), inj, re);
            session_event(st, follow, a, key(0x05, false), inj, re);
            open_session(st, b);
            session_event(st, follow, b, key(0x06, true), inj, re);
            session_event(st, follow, b, key(0x06, false), inj, re);
            // What A's close releases is what A holds then: A was left
            // behind above, which forgot what it held on the old server.
            held_at_close = st.sessions.get(&a).map_or(0, |c| c.held.len());
            close_session(st, follow, a, inj, re);
        });
        let still_marked = crate::input::has_no_picture(a);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert!(!still_marked, "A's close left its mark");
        assert_eq!(opened.len(), 2, "one injector a server: {lines:#?}");
        assert_eq!(opened[0].generation, Some(0));
        assert_eq!(opened[1].generation, Some(1));
        assert_eq!(opened[0].events(), vec![key(0x04, true), click(true)]);
        assert!(
            opened[0].gone(),
            "the first injector was told its server is gone"
        );
        assert!(
            !button_held,
            "A's button from the old server held keep busy off"
        );
        assert_eq!(
            held_at_close, 0,
            "A's close released what A held on the old server onto the new one"
        );
        assert_eq!(
            opened[1].events(),
            vec![key(0x06, true), key(0x06, false)],
            "only B reached the new server"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("WARN") || l.contains("ERROR")),
            "{lines:#?}"
        );
        let said = |what: &str| lines.iter().filter(|l| l.contains(what)).count();
        assert_eq!(
            said("is dropped while it would reach the desktop's X server"),
            1,
            "{lines:#?}"
        );
        assert_eq!(said("is back on a new X server"), 1, "{lines:#?}");
    }

    /// #3918, the same rule — C, opened while the desktop is down, had its
    /// open refused with the down sentence (#3671), which its viewer keeps up
    /// until the person reconnects: the notice marked it, so it controls
    /// nothing then, and nothing once the desktop is back either. Its keys and
    /// clicks reach no injector and open none, and that is said once. On
    /// every device: the down check refuses the open
    /// whatever the plan, and the arbiter reads none, so a session refused on
    /// a device whose explicit arm fell through is left behind too.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_session_whose_open_was_refused_while_down_controls_nothing_once_it_is_back() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        let c = sid();
        desk::down(0, 1);
        open_session(&mut st, c);
        told_the_desktop_is_down(c);
        let lines = desk::logged(|| {
            let (st, follow, inj, re) = (&mut st, &mut follow, &mut injector, &mut regrant);
            session_event(st, follow, c, key(0x04, true), inj, re);
            desk::up(1);
            session_event(st, follow, c, key(0x04, false), inj, re);
            session_event(st, follow, c, click(true), inj, re);
            session_event(st, follow, c, click(false), inj, re);
        });
        follow.closed(c);
        let n = opened.lock().unwrap().len();
        no_stand_ins();
        assert_eq!(n, 0, "an injector opened for C: {lines:#?}");
        assert!(injector.is_none());
        assert!(!st.any_button_held());
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains("is dropped while it would reach the desktop's X server"))
                .count(),
            1,
            "{lines:#?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("WARN") || l.contains("ERROR")),
            "{lines:#?}"
        );
    }

    /// #3918 — the arbiter reads no device plan: a
    /// session the supervisor marked at an X server exit is left behind
    /// whatever the plan, told or not. A, open across the exit with a key
    /// down on the first server, is marked in silence, as on a device whose
    /// explicit `drm_capture` or `portal_capture` arm fell through to the
    /// desktop's X server: nobody tells it, and its last picture is frozen.
    /// After the restart its keys reach nothing, and that is said once. B,
    /// the person's reconnect, types into the new server. Before, where the
    /// plan was another arm, A's keys followed the injector to a desktop A
    /// cannot see.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_session_marked_in_silence_reaches_nothing_and_its_reconnect_types() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        let (a, b) = (sid(), sid());
        desk::up(0);
        open_session(&mut st, a);
        let lines = desk::logged(|| {
            let (st, follow, inj, re) = (&mut st, &mut follow, &mut injector, &mut regrant);
            session_event(st, follow, a, key(0x04, true), inj, re);
            // The X server exits, and the supervisor marks A before it reads
            // the plan, which is another arm here: no notice.
            desk::down(0, 1);
            crate::input::mark_no_picture(a);
            desk::up(1);
            session_event(st, follow, a, key(0x04, false), inj, re);
            session_event(st, follow, a, key(0x05, true), inj, re);
            open_session(st, b);
            session_event(st, follow, b, key(0x06, true), inj, re);
            session_event(st, follow, b, key(0x06, false), inj, re);
        });
        let a_left = st.is_left_behind(a);
        follow.closed(a);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert!(a_left, "a session marked in silence kept its input");
        assert_eq!(opened.len(), 2, "one injector a server: {lines:#?}");
        assert_eq!(opened[0].events(), vec![key(0x04, true)]);
        assert_eq!(
            opened[1].events(),
            vec![key(0x06, true), key(0x06, false)],
            "only B reached the new server: {lines:#?}"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains("is dropped while it would reach the desktop's X server"))
                .count(),
            1,
            "{lines:#?}"
        );
    }

    /// #3918 — D, opened while the desktop is down,
    /// whose capture opens only once the desktop is back (the data-channel
    /// pump first waits for its connection's path), was never refused and
    /// never told: it sees the new desktop live, and its keys reach the new
    /// server. Under the rule taken in the peer's constructor, D recorded
    /// "down" and was left behind for its life, with a live picture and no
    /// notice.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_session_opened_while_down_and_never_told_types_once_the_desktop_is_back() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        let d = sid();
        desk::down(0, 1);
        open_session(&mut st, d);
        let lines = desk::logged(|| {
            let (st, follow, inj, re) = (&mut st, &mut follow, &mut injector, &mut regrant);
            desk::up(1);
            session_event(st, follow, d, key(0x07, true), inj, re);
            session_event(st, follow, d, key(0x07, false), inj, re);
        });
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert_eq!(opened.len(), 1, "{lines:#?}");
        assert_eq!(opened[0].generation, Some(1));
        assert_eq!(
            opened[0].events(),
            vec![key(0x07, true), key(0x07, false)],
            "a session never told was left behind: {lines:#?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("is dropped while it would reach the desktop's X server")),
            "{lines:#?}"
        );
    }

    /// #3918 — a session told that it has no
    /// picture is one without input for whatever it can still ask: a floor
    /// request, a grant, a mode switch, keep busy and a layout switch are
    /// refused as for a session without an INPUT grant. The floor it held in
    /// exclusive mode is given up, and so is a request it made, so the person
    /// who reconnects does not wait out an idle takeover from their own dead
    /// session: B's first key takes the floor and reaches the new server.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_told_session_is_refused_as_one_without_input_and_gives_up_the_floor() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        let (a, b, c) = (sid(), sid(), sid());
        let now = Instant::now();
        desk::up(0);
        st.open(a, "A".into(), true, Some(Mode::Exclusive), now);
        st.open(b, "B".into(), true, None, now);
        st.open(c, "C".into(), true, None, now);
        assert_eq!(st.snapshot().holder, Some(a));
        // C asks while A, the holder, is active: the request waits.
        assert_eq!(st.request_floor(c, now), (false, Vec::new()));
        assert_eq!(st.snapshot().pending_request.map(|p| p.session), Some(c));
        desk::down(0, 1);
        told_the_desktop_is_down(a);
        told_the_desktop_is_down(c);
        desk::up(1);
        let moved = before_each(&mut st, &mut follow, &mut injector, &mut regrant);
        let after = st.snapshot();
        let mut switched = false;
        layout_set_if_allowed(&st, a, "de".into(), |_| switched = true);
        let refused = (
            st.request_floor(a, Instant::now()),
            st.set_mode(a, Mode::Free),
            st.keep_busy_may_set(a),
            st.grant_floor(a, b),
        );
        session_event(
            &mut st,
            &mut follow,
            b,
            key(0x08, true),
            &mut injector,
            &mut regrant,
        );
        let holder = st.snapshot().holder;
        follow.closed(a);
        follow.closed(c);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert!(moved, "the viewers were not told that the floor moved");
        assert_eq!(after.holder, None, "A kept the floor");
        assert_eq!(after.pending_request, None, "C's request was kept");
        assert!(!switched, "A switched the new server's layout");
        assert_eq!(
            refused,
            (
                (false, Vec::new()),
                false,
                Err(KeepBusyRefusal::NoInput),
                (false, Vec::new())
            )
        );
        assert_eq!(after.mode, Mode::Exclusive);
        assert_eq!(holder, Some(b), "B's first key did not take the floor");
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].events(), vec![key(0x08, true)]);
    }

    /// #3918 — what a told session held on the old
    /// X server is forgotten before any other session acts, so no release of
    /// it reaches the new server: not through the modifier fence when B
    /// types, and not through a floor takeover. Before, A's held Ctrl was
    /// released onto the new server by B's first key.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn another_sessions_event_or_takeover_releases_nothing_of_a_told_session() {
        use crate::input::x_follow::test_seam as desk;
        const CTRL: u32 = 0xe0;
        let opened = stand_ins(true);
        let mut st = ArbiterState::default();
        let mut follow = Follow::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        let (a, b) = (sid(), sid());
        desk::up(0);
        open_session(&mut st, a);
        open_session(&mut st, b);
        session_event(
            &mut st,
            &mut follow,
            a,
            key(CTRL, true),
            &mut injector,
            &mut regrant,
        );
        desk::down(0, 1);
        told_the_desktop_is_down(a);
        desk::up(1);
        session_event(
            &mut st,
            &mut follow,
            b,
            key(0x09, true),
            &mut injector,
            &mut regrant,
        );
        // Exclusive mode: the told session holds the floor and Ctrl, and has
        // been idle long enough for a takeover, which releases what the
        // holder holds.
        let mut ex = ArbiterState::default();
        let idle = crate::clock::instant_before(IDLE_TAKEOVER + Duration::from_secs(1));
        let (h, r) = (sid(), sid());
        ex.open(h, "H".into(), true, Some(Mode::Exclusive), idle);
        ex.open(r, "R".into(), true, None, idle);
        assert!(matches!(
            ex.plan(h, &key(CTRL, true), idle),
            EventPlan::Inject { .. }
        ));
        told_the_desktop_is_down(h);
        before_each(&mut ex, &mut follow, &mut injector, &mut regrant);
        let takeover = ex.request_floor(r, Instant::now());
        follow.closed(a);
        follow.closed(h);
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert_eq!(opened.len(), 2);
        assert_eq!(
            opened[1].events(),
            vec![key(0x09, true)],
            "a release of A's Ctrl reached the new server"
        );
        assert_eq!(
            takeover,
            (true, Vec::new()),
            "the takeover released H's Ctrl"
        );
    }

    /// #3918 — an injector that never opened (a Noop) is dropped by #3164's
    /// regrant while the desktop is down: the gate comes after the refresh,
    /// so nothing opens onto the display with no server, where the open would
    /// fail and warn at every regrant. With the gate first, the refresh
    /// dropped the Noop after the gate had passed it, and the lazy open ran.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn a_noop_the_regrant_drops_while_the_desktop_is_down_opens_nothing() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let mut injector: Option<Box<dyn crate::input::InputInjector + Send>> =
            Some(Box::new(crate::input::NoopInjector));
        let mut regrant = Regrant::default();
        desk::down(0, 1);
        inject_all(&mut injector, &mut regrant, &[key(0x04, true)]);
        let n = opened.lock().unwrap().len();
        no_stand_ins();
        assert!(injector.is_none(), "the regrant dropped the Noop");
        assert_eq!(n, 0, "and nothing opened while the desktop is down");
    }

    /// #3918 — while the desktop is down, a controller's events, keep busy's
    /// moves and a new session's early open reach no injector and open none,
    /// and that is said once for the whole period, at INFO. Once it is back,
    /// a new session's early open lets the old injector go and opens on the
    /// new server, and keep busy moves through it. Before #3918 every event
    /// went to the injector of the dead server.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn input_while_the_desktop_is_down_reaches_nothing_and_is_said_once() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(true);
        let st = ArbiterState::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        let half = KbOp::MoveNorm {
            x: NORM_ONE / 2,
            y: NORM_ONE / 2,
        };
        desk::up(0);
        inject_all(
            &mut injector,
            &mut regrant,
            &[key(0x04, true), key(0x04, false)],
        );
        let lines = desk::logged(|| {
            // A period no other test of this process uses: the line is said
            // once a period, process-wide.
            desk::down(0, 7);
            for _ in 0..50 {
                let mv = InputMsg::MouseMove {
                    x: 0.5,
                    y: 0.5,
                    mon: 0,
                };
                inject_all(&mut injector, &mut regrant, &[mv]);
            }
            assert!(matches!(
                keep_busy_op(&st, &mut injector, &mut regrant, half),
                KbReply::Failed(_)
            ));
            warm(&mut injector, "control_ready");
            // With nothing open, nothing opens either.
            let mut none: Option<Box<dyn crate::input::InputInjector + Send>> = None;
            inject_all(&mut none, &mut regrant, &[key(0x04, true)]);
            warm(&mut none, "control_ready");
            assert!(none.is_none(), "nothing opens while the desktop is down");
            desk::up(1);
            warm(&mut injector, "control_ready");
            assert_eq!(
                keep_busy_op(&st, &mut injector, &mut regrant, half),
                KbReply::Moved
            );
        });
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert_eq!(opened.len(), 2, "{lines:#?}");
        assert_eq!(
            opened[0].events(),
            vec![key(0x04, true), key(0x04, false)],
            "nothing reached the first injector after its server went"
        );
        assert!(opened[0].moves().is_empty());
        assert!(opened[0].gone());
        assert_eq!(opened[1].moves(), vec![(0.5, 0.5)]);
        let said: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("virtual desktop is down"))
            .collect();
        assert_eq!(said.len(), 1, "once for the period: {lines:#?}");
        assert!(said[0].contains("INFO"), "{said:#?}");
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("WARN") || l.contains("ERROR")),
            "{lines:#?}"
        );
    }

    /// #3918 — an injector that is not the desktop's X11 one (uinput, an X11
    /// seat that is not the virtual desktop) records no generation and sees
    /// no change: its events reach it while the desktop is down, and a new X
    /// server of the desktop does not drop it.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[test]
    fn an_injector_that_is_not_the_desktops_never_follows_it() {
        use crate::input::x_follow::test_seam as desk;
        let opened = stand_ins(false);
        let mut injector = None;
        let mut regrant = Regrant::default();
        desk::up(0);
        inject_all(&mut injector, &mut regrant, &[key(0x04, true)]);
        desk::down(0, 1);
        inject_all(&mut injector, &mut regrant, &[key(0x04, false)]);
        desk::up(1);
        inject_all(&mut injector, &mut regrant, &[key(0x05, true)]);
        warm(&mut injector, "control_ready");
        let opened = opened.lock().unwrap().clone();
        no_stand_ins();
        assert_eq!(opened.len(), 1, "never let go, never opened again");
        assert_eq!(opened[0].generation, None);
        assert!(!opened[0].gone());
        assert_eq!(
            opened[0].events(),
            vec![key(0x04, true), key(0x04, false), key(0x05, true)]
        );
    }

    /// #4069 — the warm-up waits for a planned automatic system capture, and
    /// runs again on its live edge. Each case runs the worker's own code
    /// ([`command`]) on a system install's seat on this thread
    /// ([`on_a_seat`]): `seat_guard::LIVE_HERE` says whether an automatic
    /// capture is live, `input::PLANNED_HERE` whether one is planned. The
    /// process's real count is never read: other tests make marks.
    #[cfg(all(
        target_os = "linux",
        feature = "enigo-input",
        feature = "uinput-input",
        feature = "drm-capture"
    ))]
    mod warm_up {
        use super::*;
        use crate::input::x_follow::test_seam as desk;
        use std::sync::{Arc, Mutex};

        /// What one stand-in open made, as `open_default` picks with `uinput`
        /// unset: the automatic uinput while an automatic capture is live,
        /// another injector (enigo) otherwise; and what reached it.
        #[derive(Clone, Default)]
        struct Opened {
            automatic: bool,
            events: Arc<Mutex<Vec<InputMsg>>>,
        }

        impl Opened {
            fn events(&self) -> Vec<InputMsg> {
                self.events.lock().unwrap().clone()
            }
        }

        struct OnSeat(Opened);

        impl crate::input::InputInjector for OnSeat {
            fn inject(&mut self, event: InputMsg) -> anyhow::Result<()> {
                self.0.events.lock().unwrap().push(event);
                Ok(())
            }
            fn has_permission(&self) -> bool {
                true
            }
            fn automatic(&self) -> bool {
                self.0.automatic
            }
        }

        /// A system install's seat on this thread: the arbiter's lazy open
        /// makes an [`OnSeat`], automatic while `seat_guard::live` says so;
        /// the automatic capture is `planned` or not, and none is live yet;
        /// the warm-up is on; no virtual desktop runs. Every open, in order.
        /// `no_stand_ins` undoes it.
        fn on_a_seat(planned: bool) -> Arc<Mutex<Vec<Opened>>> {
            crate::capture::seat_guard::LIVE_HERE.with(|l| l.set(Some(false)));
            crate::input::PLANNED_HERE.with(|p| p.set(Some(planned)));
            WARM_HERE.with(|w| w.set(Some(true)));
            desk::none();
            let opened = Arc::new(Mutex::new(Vec::<Opened>::new()));
            let list = opened.clone();
            STAND_IN_OPEN.with(|o| {
                *o.borrow_mut() = Some(Box::new(move || {
                    let made = Opened {
                        automatic: crate::capture::seat_guard::live(),
                        ..Opened::default()
                    };
                    list.lock().unwrap().push(made.clone());
                    Box::new(OnSeat(made))
                }))
            });
            opened
        }

        /// The automatic capture goes live on this thread, as its mark's
        /// count does before the edge is sent.
        fn capture_goes_live() {
            crate::capture::seat_guard::LIVE_HERE.with(|l| l.set(Some(true)));
        }

        /// The worker's state on this thread, and each command through the
        /// worker's own code.
        #[derive(Default)]
        struct Worker {
            st: ArbiterState,
            follow: Follow,
            injector: Option<Box<dyn crate::input::InputInjector + Send>>,
            regrant: Regrant,
        }

        impl Worker {
            fn run(&mut self, cmd: Cmd) {
                command(
                    &mut self.st,
                    &mut self.follow,
                    &mut self.injector,
                    &mut self.regrant,
                    cmd,
                );
            }
            fn open(&mut self, session: ObjectId, can_input: bool) {
                self.run(Cmd::Open {
                    session,
                    name: "viewer".into(),
                    can_input,
                    mode_hint: None,
                    control: Arc::new(tokio::sync::Mutex::new(None)),
                    cursor: Arc::new(tokio::sync::Mutex::new(None)),
                });
            }
            /// Whether the cached injector is the automatic uinput.
            fn cached(&self) -> Option<bool> {
                self.injector.as_deref().map(|i| i.automatic())
            }
        }

        /// 0.4.227's order: the control DC is ready before the automatic
        /// capture exists. The warm-up opens nothing (no enigo, which a root
        /// daemon has no display for: #3787's ERROR) and says it waits. The
        /// live edge then opens the automatic uinput, once, and keeps it,
        /// ahead of the first event, which reaches it. Before #4069 the control
        /// DC's warm-up fell through to enigo here, and the uinput opened at
        /// the first event.
        #[test]
        fn the_control_dc_waits_and_the_live_edge_opens_the_uinput() {
            let opened = on_a_seat(true);
            let mut w = Worker::default();
            let a = sid();
            let mut at_control_dc = usize::MAX;
            let lines = desk::logged(|| {
                w.open(a, true);
                w.run(Cmd::ControlReady { session: a });
                at_control_dc = opened.lock().unwrap().len();
                capture_goes_live();
                w.run(Cmd::CaptureLive);
            });
            w.run(Cmd::Event {
                session: a,
                msg: key(0x04, true),
            });
            let cached = w.cached();
            let opened = opened.lock().unwrap().clone();
            no_stand_ins();
            assert_eq!(at_control_dc, 0, "the control DC opened: {lines:#?}");
            assert_eq!(opened.len(), 1, "{lines:#?}");
            assert!(opened[0].automatic, "the live edge opened enigo");
            assert_eq!(cached, Some(true), "the uinput was not kept");
            assert_eq!(opened[0].events(), vec![key(0x04, true)]);
            let said = |what: &str, on: &str| {
                lines
                    .iter()
                    .any(|l| l.contains("INFO") && l.contains(what) && l.contains(on))
            };
            assert!(
                said("waits for the planned automatic", "control_ready"),
                "{lines:#?}"
            );
            assert!(
                said("opened ahead of the first event", "capture_live"),
                "{lines:#?}"
            );
        }

        /// 0.4.226's order: the automatic capture is live before the control
        /// DC. Its live edge opens the uinput as soon as a session that may
        /// input is registered, and the control DC's warm-up opens nothing
        /// more: one open in all.
        #[test]
        fn a_capture_live_before_the_control_dc_opens_the_uinput_once() {
            let opened = on_a_seat(true);
            let mut w = Worker::default();
            let a = sid();
            w.open(a, true);
            capture_goes_live();
            w.run(Cmd::CaptureLive);
            let at_the_edge = opened.lock().unwrap().len();
            w.run(Cmd::ControlReady { session: a });
            let cached = w.cached();
            let opened = opened.lock().unwrap().clone();
            no_stand_ins();
            assert_eq!(at_the_edge, 1, "the live edge opened nothing");
            assert_eq!(opened.len(), 1, "the control DC opened again");
            assert!(opened[0].automatic);
            assert_eq!(cached, Some(true));
        }

        /// No session may input: the live edge with no session, with a
        /// view-only one, and after the only one that could has closed, opens
        /// nothing. A session that may input, registered after the edge,
        /// opens at its control DC, the capture live by then.
        #[test]
        fn the_live_edge_opens_nothing_with_no_session_that_may_input() {
            let opened = on_a_seat(true);
            let mut w = Worker::default();
            let (v, gone, a) = (sid(), sid(), sid());
            capture_goes_live();
            w.run(Cmd::CaptureLive);
            w.open(v, false);
            w.run(Cmd::CaptureLive);
            w.open(gone, true);
            w.run(Cmd::Close { session: gone });
            w.run(Cmd::CaptureLive);
            let at_the_edges = opened.lock().unwrap().len();
            w.open(a, true);
            w.run(Cmd::ControlReady { session: a });
            let opened = opened.lock().unwrap().clone();
            no_stand_ins();
            assert_eq!(at_the_edges, 0, "an edge opened for nobody");
            assert_eq!(opened.len(), 1, "the control DC did not open");
            assert!(opened[0].automatic);
        }

        /// `ROOMLERD_INPUT_WARM=0` turns every warm-up off, the live edge's
        /// too: neither the control DC (nothing planned, so it would open
        /// otherwise) nor the edge opens anything, and the first event is the
        /// opener.
        #[test]
        fn input_warm_off_opens_nothing_at_the_control_dc_or_the_live_edge() {
            let opened = on_a_seat(false);
            WARM_HERE.with(|w| w.set(Some(false)));
            let mut w = Worker::default();
            let a = sid();
            w.open(a, true);
            w.run(Cmd::ControlReady { session: a });
            capture_goes_live();
            w.run(Cmd::CaptureLive);
            let warmed = opened.lock().unwrap().len();
            w.run(Cmd::Event {
                session: a,
                msg: key(0x04, true),
            });
            let opened = opened.lock().unwrap().clone();
            no_stand_ins();
            assert_eq!(warmed, 0, "a warm-up opened with the switch off");
            assert_eq!(opened.len(), 1, "the first event did not open");
            assert!(opened[0].automatic);
            assert_eq!(opened[0].events(), vec![key(0x04, true)]);
        }

        /// Nothing planned and nothing live (an X11 desktop, `uinput` set, the
        /// automatic capture switched off: `input`'s
        /// `the_automatic_uinput_is_planned_only_for_the_drm_arm_with_uinput_unset`):
        /// the control DC's warm-up opens at once, as before #4069, what
        /// `open_default` picks then, and keeps it.
        #[test]
        fn with_nothing_planned_the_control_dc_opens_as_before() {
            let opened = on_a_seat(false);
            let mut w = Worker::default();
            let a = sid();
            w.open(a, true);
            w.run(Cmd::ControlReady { session: a });
            let cached = w.cached();
            let opened = opened.lock().unwrap().clone();
            no_stand_ins();
            assert_eq!(opened.len(), 1, "the control DC did not open");
            assert!(!opened[0].automatic);
            assert_eq!(cached, Some(false));
        }

        /// The prediction missed (no brief yet, or one a minute old), and the
        /// capture comes: the control DC's warm-up opened enigo, and the live
        /// edge replaces it through `stale`, so the uinput is open ahead of the
        /// first event, and no event during a lock can reach enigo, whose
        /// XTest reaches Xwayland windows behind the lock screen.
        #[test]
        fn the_live_edge_replaces_what_the_control_dc_opened_before_it() {
            let opened = on_a_seat(false);
            let mut w = Worker::default();
            let a = sid();
            w.open(a, true);
            w.run(Cmd::ControlReady { session: a });
            capture_goes_live();
            w.run(Cmd::CaptureLive);
            let at_the_edge = opened.lock().unwrap().len();
            w.run(Cmd::Event {
                session: a,
                msg: key(0x04, true),
            });
            let cached = w.cached();
            let opened = opened.lock().unwrap().clone();
            no_stand_ins();
            assert_eq!(at_the_edge, 2, "the live edge did not replace enigo");
            assert_eq!(opened.len(), 2);
            assert!(!opened[0].automatic);
            assert!(opened[1].automatic);
            assert_eq!(cached, Some(true));
            assert_eq!(
                opened[0].events(),
                Vec::<InputMsg>::new(),
                "enigo got the event"
            );
            assert_eq!(opened[1].events(), vec![key(0x04, true)]);
        }

        /// The prediction missed the other way: the capture is planned and
        /// never goes live (its DRM open failed, and the cascade fell
        /// through). The waiting warm-up costs the warm-up and no event: the
        /// first event opens what `open_default` picks then, as before, and
        /// every event reaches it.
        #[test]
        fn a_planned_capture_that_never_goes_live_leaves_the_first_event_the_opener() {
            let opened = on_a_seat(true);
            let mut w = Worker::default();
            let a = sid();
            w.open(a, true);
            w.run(Cmd::ControlReady { session: a });
            let warmed = opened.lock().unwrap().len();
            for msg in [key(0x04, true), key(0x04, false)] {
                w.run(Cmd::Event { session: a, msg });
            }
            let opened = opened.lock().unwrap().clone();
            no_stand_ins();
            assert_eq!(warmed, 0);
            assert_eq!(opened.len(), 1, "the first event did not open");
            assert!(!opened[0].automatic);
            assert_eq!(opened[0].events(), vec![key(0x04, true), key(0x04, false)]);
        }

        /// The edge's signal as the arbiter sets it ([`live_edge`]), on a
        /// thread with no tokio runtime, as a capture's may be: one
        /// `CaptureLive`, then, on a full queue, nothing, at once. It holds
        /// only the arbiter's sender, so it can start nothing. With no
        /// arbiter nothing set it, and the edge tells nobody: `seat_guard`'s
        /// `with_no_arbiter_the_live_edge_tells_nobody_and_needs_no_runtime`.
        #[test]
        fn the_live_edges_signal_never_blocks_and_needs_no_runtime() {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Cmd>(1);
            let edge = live_edge(tx);
            let (done, finished) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let no_runtime = tokio::runtime::Handle::try_current().is_err();
                edge();
                edge();
                let _ = done.send(no_runtime);
            });
            assert_eq!(
                finished.recv_timeout(Duration::from_secs(10)),
                Ok(true),
                "the edge waited on a full queue, or ran inside a runtime"
            );
            assert!(matches!(rx.try_recv(), Ok(Cmd::CaptureLive)));
            assert!(rx.try_recv().is_err(), "the second edge was not dropped");
        }
    }
}

/// #3918 on a real X server: CI's X11 step (`ROOMLERD_TEST_X11=1`, an Xvfb,
/// one test at a time), in a process of its own, because what is asserted is
/// the process's log: enigo logs from its own thread, through `log`.
#[cfg(all(test, target_os = "linux", feature = "enigo-input"))]
mod x11_follow_tests {
    use super::*;
    use crate::input::x_follow::test_seam as desk;
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::ConnectionExt as _;

    /// Everything this process logs, `log`'s records included, as the
    /// daemon's own logging bridges them. Set once for the process.
    #[derive(Clone, Default)]
    struct ProcessLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for ProcessLog {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl ProcessLog {
        fn get() -> &'static ProcessLog {
            static LOG: std::sync::OnceLock<ProcessLog> = std::sync::OnceLock::new();
            LOG.get_or_init(|| {
                let log = ProcessLog::default();
                let writer = log.clone();
                let subscriber = tracing_subscriber::fmt()
                    .with_target(false)
                    .with_ansi(false)
                    .without_time()
                    .with_thread_names(true)
                    .with_max_level(tracing::Level::INFO)
                    .with_writer(move || writer.clone())
                    .finish();
                // With the bridge from `log`. Refused when another test set
                // one first: the test then hears nothing, and says so.
                let _ = tracing_subscriber::util::SubscriberInitExt::try_init(subscriber);
                log
            })
        }

        fn len(&self) -> usize {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
        }

        fn since(&self, from: usize) -> Vec<String> {
            let all = self.0.lock().unwrap_or_else(|e| e.into_inner());
            String::from_utf8_lossy(&all[from.min(all.len())..])
                .lines()
                .map(String::from)
                .collect()
        }
    }

    /// What the X server on `display` shows: the pointer, whether the left
    /// button is down, whether any key is down.
    type Seen = ((i16, i16), bool, bool);

    fn seen_on(display: &str) -> Option<Seen> {
        let (conn, screen) = x11rb::connect(Some(display)).ok()?;
        let root = conn.setup().roots[screen].root;
        let p = conn.query_pointer(root).ok()?.reply().ok()?;
        let keys = conn.query_keymap().ok()?.reply().ok()?;
        let left = u16::from(p.mask) & u16::from(x11rb::protocol::xproto::KeyButMask::BUTTON1) != 0;
        Some((
            (p.root_x, p.root_y),
            left,
            keys.keys.iter().any(|b| *b != 0),
        ))
    }

    /// The server's state once it shows `want`, or as it is after 3 s.
    fn until(display: &str, want: impl Fn(Seen) -> bool) -> Option<Seen> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let seen = seen_on(display);
            if seen.is_some_and(&want) || Instant::now() >= deadline {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn to(x: f32, y: f32) -> InputMsg {
        InputMsg::MouseMove { x, y, mon: 0 }
    }

    fn key(code: u32, down: bool) -> InputMsg {
        InputMsg::Key {
            code,
            down,
            mods: 0,
        }
    }

    fn left(down: bool) -> InputMsg {
        InputMsg::MouseButton {
            btn: Button::Left,
            down,
            x: 0.25,
            y: 0.25,
            mon: 0,
        }
    }

    /// #3918 — the input injector of a desktop whose X server is killed and
    /// started again on the same display. A key and the left button are
    /// down on the first server when it dies. While the desktop is down,
    /// moves and keep busy reach nothing. Once a new server answers, the
    /// releases go to it and release nothing there, a move lands, and the
    /// next key and click are down and up on the new server, as is keep
    /// busy's move. The old injector goes without a line of its own: no
    /// ERROR for the keys it held, which enigo releases as it drops and
    /// cannot on a dead connection. Before #3918 every event after the kill
    /// went to the dead connection, with enigo's ERROR lines.
    #[test]
    fn the_input_injector_follows_a_new_x_server_and_its_old_one_goes_quietly() {
        if !crate::virtual_desktop::TestX::opted_in() {
            eprintln!("ROOMLERD_TEST_X11 unset — skipping (the Xvfb lane sets it)");
            return;
        }
        let log = ProcessLog::get();
        let x = crate::virtual_desktop::TestX::start()
            .expect("ROOMLERD_TEST_X11=1 but no Xvfb came up");
        let display = x.display().to_string();
        let _pointed = x.point_display();
        let st = ArbiterState::default();
        let mut injector = None;
        let mut regrant = Regrant::default();
        desk::up(0);
        let from = log.len();
        inject_all(
            &mut injector,
            &mut regrant,
            &[to(0.5, 0.5), key(0x04, true), left(true)],
        );
        let first = until(&display, |(_, l, k)| l && k);
        assert!(
            first.is_some_and(|(_, l, k)| l && k),
            "the key and the button reached the first server: {first:?}"
        );
        assert_eq!(
            injector.as_deref().and_then(|i| i.x_generation()),
            Some(0),
            "the injector recorded the desktop's generation as it opened"
        );

        x.kill();
        desk::down(0, 1);
        for _ in 0..20 {
            inject_all(&mut injector, &mut regrant, &[to(0.6, 0.6)]);
        }
        assert!(matches!(
            keep_busy_op(
                &st,
                &mut injector,
                &mut regrant,
                KbOp::Move { x: 10, y: 10 }
            ),
            KbReply::Failed(_)
        ));
        assert!(x.restart(), "a new X server on the same display");
        desk::up(1);

        inject_all(
            &mut injector,
            &mut regrant,
            &[key(0x04, false), left(false), to(0.75, 0.75)],
        );
        let near = |a: i16, b: i16| (a - b).abs() <= 2;
        let back = until(&display, |((px, py), l, k)| {
            !l && !k && near(px, 480) && near(py, 360)
        });
        assert!(
            back.is_some_and(|((px, py), l, k)| !l && !k && near(px, 480) && near(py, 360)),
            "the releases released nothing and the move landed on the new server: {back:?}"
        );
        inject_all(&mut injector, &mut regrant, &[key(0x05, true), left(true)]);
        let down = until(&display, |(_, l, k)| l && k);
        assert!(
            down.is_some_and(|(_, l, k)| l && k),
            "the next key and click reached the new server: {down:?}"
        );
        inject_all(
            &mut injector,
            &mut regrant,
            &[key(0x05, false), left(false)],
        );
        let up = until(&display, |(_, l, k)| !l && !k);
        assert!(
            up.is_some_and(|(_, l, k)| !l && !k),
            "and their releases: {up:?}"
        );
        // Keep busy's move is answered by the injector's thread within the
        // daemon's bound (`PX_REPLY_TIMEOUT`), and that thread opened moments
        // ago on a fresh server: on a loaded runner its first answer can come
        // later. A late answer is not a fault (keep busy asks again at its
        // next tick), so the test asks again, for up to 5 s.
        let deadline = Instant::now() + Duration::from_secs(5);
        let moved = loop {
            let reply = keep_busy_op(
                &st,
                &mut injector,
                &mut regrant,
                KbOp::Move { x: 100, y: 120 },
            );
            if reply == KbReply::Moved || Instant::now() >= deadline {
                break reply;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(moved, KbReply::Moved);
        let kept = until(&display, |(p, _, _)| p == (100, 120));
        assert!(
            kept.is_some_and(|(p, _, _)| p == (100, 120)),
            "keep busy moved through the new injector: {kept:?}"
        );
        // The old injector's thread ends on its own.
        std::thread::sleep(Duration::from_millis(300));
        desk::clear();

        let lines = log.since(from);
        assert!(
            lines.iter().any(|l| l.contains("back on a new X server")),
            "the process log was not heard: {lines:#?}"
        );
        let count = |s: &str| lines.iter().filter(|l| l.contains(s)).count();
        // The input path: the injector thread (enigo logs there, through `log`)
        // and the arbiter's own lines. The test's X server logs its own.
        let input = |s: &str| {
            lines
                .iter()
                .filter(|l| l.contains(s) && (l.contains("roomlerd-input") || l.contains("input:")))
                .count()
        };
        let (errors, warnings) = (input("ERROR"), input("WARN"));
        let (said_down, said_back) = (
            count("virtual desktop is down"),
            count("back on a new X server"),
        );
        assert_eq!((errors, warnings), (0, 0), "{lines:#?}");
        assert_eq!((said_down, said_back), (1, 1), "{lines:#?}");
        println!(
            "x11-real: the input injector followed a new X server: the key and button released and pressed again there, keep busy moved, {errors} errors, {warnings} warnings, {said_down} line for the down period and {said_back} for the reopen"
        );
    }
}
