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

/// A floor request against a holder who has produced no input for this
/// long is auto-granted (walk-away takeover); an ACTIVE holder keeps the
/// floor and the requester is told who has it.
pub const IDLE_TAKEOVER: Duration = Duration::from_secs(2);

/// Per-source ghost-cursor rebroadcast floor (~30 Hz).
pub const GHOST_MIN_INTERVAL: Duration = Duration::from_millis(33);

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
            // Coordinates are ignored for an up of an already-known button
            // position; (0,0) on the primary monitor is the safe no-move up.
            Held::Btn(btn) => InputMsg::MouseButton {
                btn,
                down: false,
                x: 0.0,
                y: 0.0,
                mon: 0,
            },
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
}

/// What the worker should do with one event.
#[derive(Debug, PartialEq)]
pub enum EventPlan {
    /// Floor-denied (exclusive mode, not the holder) or no INPUT grant.
    Deny,
    /// Inject `pre` (fence releases) then the event itself.
    Inject { pre: Vec<InputMsg> },
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
    mode_seeded: bool,
    sessions: HashMap<ObjectId, SessCore>,
}

/// Broadcast-ready snapshot of the arbiter state.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub mode: Mode,
    pub holder: Option<ObjectId>,
    pub participants: Vec<Participant>,
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
            },
        );
        if self.mode == Mode::Exclusive && self.holder.is_none() && can_input {
            self.holder = Some(session);
        }
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
        if self.holder == Some(session) {
            // Hand the floor to any remaining INPUT-capable session.
            self.holder = self
                .sessions
                .iter()
                .find(|(_, c)| c.can_input)
                .map(|(id, _)| *id);
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

    /// Decide one event. Tracks held state, applies the exclusive floor,
    /// and plans cross-session modifier fencing on injector switch.
    pub fn plan(&mut self, session: ObjectId, msg: &InputMsg, now: Instant) -> EventPlan {
        // Heartbeats carry no input; pass them through un-fenced so their
        // timing diagnostics stay honest.
        if matches!(msg, InputMsg::Heartbeat { .. }) {
            return EventPlan::Inject { pre: Vec::new() };
        }
        let can_input = self
            .sessions
            .get(&session)
            .map(|c| c.can_input)
            .unwrap_or(false);
        if !can_input {
            return EventPlan::Deny;
        }
        if self.mode == Mode::Exclusive {
            if self.holder.is_none() {
                // No holder yet (e.g. mode toggled with none) — first event wins.
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
                }
            }
            self.last_injector = Some(session);
        }

        // Held-state tracking for THIS session.
        if let Some(core) = self.sessions.get_mut(&session) {
            core.last_input = now;
            match msg {
                InputMsg::Key { code, down, .. } => {
                    if *down {
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
                _ => {}
            }
        }
        EventPlan::Inject { pre }
    }

    /// Floor request. Returns `(granted, releases)` — on a takeover the
    /// previous holder's held set is released (their chord is over).
    pub fn request_floor(&mut self, session: ObjectId, now: Instant) -> (bool, Vec<InputMsg>) {
        if self.mode == Mode::Free {
            // Nothing to hold in free mode — trivially granted.
            return (true, Vec::new());
        }
        if !self
            .sessions
            .get(&session)
            .map(|c| c.can_input)
            .unwrap_or(false)
        {
            return (false, Vec::new());
        }
        match self.holder {
            None => {
                self.holder = Some(session);
                (true, Vec::new())
            }
            Some(h) if h == session => (true, Vec::new()),
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
                    (true, releases)
                } else {
                    (false, Vec::new())
                }
            }
        }
    }

    /// In-session mode toggle (INPUT-granted sessions only). Returns whether
    /// the mode actually changed. Flipping to exclusive makes the toggling
    /// session the holder; flipping to free clears the floor.
    pub fn set_mode(&mut self, session: ObjectId, mode: Mode) -> bool {
        if !self
            .sessions
            .get(&session)
            .map(|c| c.can_input)
            .unwrap_or(false)
        {
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
        true
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut participants: Vec<Participant> = self
            .sessions
            .iter()
            .map(|(id, c)| Participant {
                session: *id,
                name: c.name.clone(),
                input: c.can_input,
            })
            .collect();
        participants.sort_by_key(|p| p.session);
        Snapshot {
            mode: self.mode,
            holder: self.holder,
            participants,
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
    Event {
        session: ObjectId,
        msg: InputMsg,
    },
    RequestFloor {
        session: ObjectId,
    },
    SetMode {
        session: ObjectId,
        mode: Mode,
    },
}

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
        Arbiter { tx }
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
    pub fn event(&self, session: ObjectId, msg: InputMsg) {
        if self.tx.try_send(Cmd::Event { session, msg }).is_err() {
            tracing::debug!(%session, "input arbiter queue full — event dropped");
        }
    }
    pub fn request_floor(&self, session: ObjectId) {
        let _ = self.tx.try_send(Cmd::RequestFloor { session });
    }
    pub fn set_mode(&self, session: ObjectId, mode: Mode) {
        let _ = self.tx.try_send(Cmd::SetMode { session, mode });
    }
}

fn worker(rx: std::sync::mpsc::Receiver<Cmd>, handle: tokio::runtime::Handle) {
    let mut state = ArbiterState::default();
    let mut sinks: HashMap<ObjectId, Sinks> = HashMap::new();
    // ONE OS injector for the whole process — created lazily so the
    // SystemContext worker-role probe runs at first real use, matching the
    // pre-P6 per-session open timing.
    let mut injector: Option<Box<dyn super::InputInjector + Send>> = None;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Open {
                session,
                name,
                can_input,
                mode_hint,
                control,
                cursor,
            } => {
                state.open(session, name, can_input, mode_hint, Instant::now());
                sinks.insert(
                    session,
                    Sinks {
                        control,
                        cursor,
                        last_ghost: Instant::now() - GHOST_MIN_INTERVAL,
                    },
                );
                tracing::info!(
                    %session, can_input, mode = state.snapshot().mode.as_str(),
                    sessions = state.session_count(),
                    "input arbiter: session registered"
                );
                broadcast_state(&state, &sinks, &handle);
            }
            Cmd::Close { session } => {
                let releases = state.close(session);
                if !releases.is_empty() {
                    tracing::info!(
                        %session,
                        released = releases.len(),
                        "input arbiter: release-all on session close"
                    );
                    inject_all(&mut injector, &releases);
                }
                sinks.remove(&session);
                if state.session_count() > 0 {
                    broadcast_state(&state, &sinks, &handle);
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
                    inject_all(&mut injector, &releases);
                }
            }
            Cmd::Event { session, msg } => {
                // Ghost cursors ride on intent, not on the floor: even a
                // denied (non-holder) session's pointer shows to others.
                if let InputMsg::MouseMove { x, y, mon } = msg
                    && sinks.len() > 1
                {
                    ghost_broadcast(&state, &mut sinks, &handle, session, x, y, mon);
                }
                match state.plan(session, &msg, Instant::now()) {
                    EventPlan::Deny => {}
                    EventPlan::Inject { pre } => {
                        if !pre.is_empty() {
                            tracing::debug!(
                                %session,
                                fenced = pre.len(),
                                "input arbiter: cross-session modifier fence"
                            );
                            inject_all(&mut injector, &pre);
                        }
                        inject_all(&mut injector, std::slice::from_ref(&msg));
                    }
                }
            }
            Cmd::RequestFloor { session } => {
                let (granted, releases) = state.request_floor(session, Instant::now());
                tracing::info!(%session, granted, "input arbiter: floor request");
                if !releases.is_empty() {
                    inject_all(&mut injector, &releases);
                }
                broadcast_state(&state, &sinks, &handle);
            }
            Cmd::SetMode { session, mode } => {
                if state.set_mode(session, mode) {
                    tracing::info!(%session, mode = mode.as_str(), "input arbiter: mode changed");
                }
                broadcast_state(&state, &sinks, &handle);
            }
        }
    }
    tracing::debug!("input arbiter worker exiting (channel closed)");
}

fn inject_all(injector: &mut Option<Box<dyn super::InputInjector + Send>>, msgs: &[InputMsg]) {
    let inj = injector.get_or_insert_with(super::open_default);
    for m in msgs {
        if let Err(e) = inj.inject(m.clone()) {
            tracing::debug!(%e, "input arbiter: inject failed");
        }
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
            EventPlan::Inject { pre: vec![] }
        );
        // B clicks — A's held Ctrl must be released FIRST.
        match st.plan(b, &click(true), now) {
            EventPlan::Inject { pre } => {
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
            EventPlan::Inject { pre } => assert!(pre.is_empty(), "no modifiers left to fence"),
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
            EventPlan::Inject { pre } => assert!(pre.is_empty(), "letters are not fenced"),
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
            InputMsg::Key { down: false, .. } | InputMsg::MouseButton { down: false, .. }
        )));
        // Idempotent.
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

    #[test]
    fn mode_parse_round_trips() {
        assert_eq!(Mode::parse("free"), Some(Mode::Free));
        assert_eq!(Mode::parse(" Exclusive "), Some(Mode::Exclusive));
        assert_eq!(Mode::parse("nope"), None);
        assert_eq!(Mode::Free.as_str(), "free");
        assert_eq!(Mode::Exclusive.as_str(), "exclusive");
    }
}
