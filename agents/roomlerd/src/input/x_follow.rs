// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3918 — the X11 injector follows this daemon's virtual desktop to its next
//! X server.
//!
//! The arbiter keeps ONE injector for the process (`arbiter::worker`). Where
//! the desktop is the daemon's own virtual desktop, that injector is enigo on
//! an X connection to the desktop's display, and the connection belongs to
//! the X server it was opened on. The daemon starts the server again when it
//! exits (#3643), on the same display: `DISPLAY` still names it, and the old
//! connection reaches nobody. Before #3918 every key, click and move then
//! failed until the daemon restarted, in the open session and in every
//! session opened afterwards, with enigo's ERROR lines for each event.
//!
//! - An injector records the desktop's generation before it connects
//!   ([`opened_under`]). One left on an older generation is let go
//!   ([`let_go`]) through the arbiter's own drop, so the arbiter's lazy open
//!   connects to the new server: one INFO line.
//! - While the desktop is down, an event for it is dropped before it reaches
//!   any injector, and nothing opens ([`Gate::Down`]): one INFO line a down
//!   period. The supervisor has already warned of the exit itself, once;
//!   this is its consequence for input, not a second fault.
//! - A session that has no picture of the desktop, told so or open at an X
//!   server exit, has no input of it while its input would reach the
//!   desktop's X server ([`Sessions`], [`no_picture`]): one INFO line a
//!   session.
//!
//! Only the X11 injector, and only when its X server is the virtual
//! desktop's. A uinput injector, an X11 seat that is not the virtual desktop
//! and a daemon without one record no generation, and nothing here reads the
//! desktop for them. `ROOMLERD_INPUT_X11_FOLLOW=0` restores the behaviour
//! before #3918, for input and for a shared pipeline's joins ([`enabled`]).

use super::InputInjector;
use super::arbiter::ArbiterState;
use crate::virtual_desktop::XServerNow;
use bson::oid::ObjectId;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

/// What the arbiter does with one event, or one open, as far as the virtual
/// desktop is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// As before #3918: not for the virtual desktop, or for the X server that
    /// answers now.
    Pass,
    /// The desktop is down: the event is dropped, and nothing opens.
    Down,
    /// The cached injector is a client of an older X server of the desktop:
    /// [`let_go`] it, and the lazy open makes one on the new server.
    LeftBehind { was: u64, now: u64 },
}

/// `ROOMLERD_INPUT_X11_FOLLOW` (default on), read once for the daemon's life:
/// `0` keeps the injector on its first X server, as before #3918, and turns
/// off both readers of the no-picture marks ([`no_picture`]): the arbiter
/// leaves nobody behind ([`Sessions`]) and every shared pipeline takes
/// joiners (`input::has_no_picture`), as before #3918. The marks are still
/// written, and nothing reads them. The tell has a switch of its own,
/// `ROOMLERD_VIRTUAL_DESKTOP_TELL_SESSIONS=0`, which turns off the notice
/// only.
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = test_seam::FOLLOW.with(std::cell::Cell::get) {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| tunnel_core::env::flag("INPUT_X11_FOLLOW", true))
}

/// The desktop as input sees it ([`crate::virtual_desktop::x_server_now`]),
/// or a test's stand-in on the test's own thread.
fn now() -> Option<XServerNow> {
    #[cfg(test)]
    if let Some(stand_in) = test_seam::DESKTOP.with(std::cell::Cell::get) {
        return stand_in;
    }
    crate::virtual_desktop::x_server_now()
}

/// What an X11 injector records as it opens, BEFORE it connects: the
/// generation of this daemon's virtual desktop, when one runs and input
/// follows it. `None` everywhere else, and then nothing ever follows.
pub(crate) fn opened_under() -> Option<u64> {
    if !enabled() {
        return None;
    }
    now().map(|n| n.generation)
}

/// The gate for one event or open: `inj` is the cached injector, `None`
/// before the first open (or after one without permission, which keep busy
/// does not keep).
pub(crate) fn gate(inj: Option<&(dyn InputInjector + Send)>) -> Gate {
    #[cfg(not(test))]
    {
        static SAID: AtomicU64 = AtomicU64::new(u64::MAX);
        gate_in(inj, enabled(), &SAID)
    }
    // A test's own slot, on its own thread (`test_seam::SAID`).
    #[cfg(test)]
    {
        test_seam::SAID.with(|said| gate_in(inj, enabled(), said))
    }
}

/// [`gate`] with the kill switch and the down period last said given.
fn gate_in(inj: Option<&(dyn InputInjector + Send)>, on: bool, said: &AtomicU64) -> Gate {
    if !on {
        return Gate::Pass;
    }
    match inj {
        // Not an X client of the desktop: the desktop is not even read.
        Some(i) => match i.x_generation() {
            None => Gate::Pass,
            Some(was) => gate_at(Some(was), now(), said),
        },
        // Nothing open: the open that would follow concerns the desktop only
        // when one runs here and the open would be the X11 one.
        None => match now() {
            Some(n) if super::x11_would_open() => gate_at(None, Some(n), said),
            _ => Gate::Pass,
        },
    }
}

/// [`gate`] for an X client of the desktop: `opened` is the generation it was
/// opened under (`None` before the first open), `now` the desktop.
fn gate_at(opened: Option<u64>, now: Option<XServerNow>, said: &AtomicU64) -> Gate {
    // No virtual desktop in this process.
    let Some(now) = now else {
        return Gate::Pass;
    };
    if let Some(period) = now.down {
        if said.swap(period, Ordering::Relaxed) != period {
            tracing::info!(
                down_period = period,
                "input: this device's virtual desktop is down — remote input for it is dropped \
                 until its X server is back (#3918; said once until then)"
            );
        }
        return Gate::Down;
    }
    match opened {
        Some(was) if was != now.generation => Gate::LeftBehind {
            was,
            now: now.generation,
        },
        _ => Gate::Pass,
    }
}

/// Let the cached injector go: its X server is gone. It is told first, so its
/// teardown asks nothing of the dead server, and then dropped exactly as the
/// arbiter drops a stale one, so the arbiter's lazy open, next, connects to
/// the desktop's new server.
///
/// What it held went with its server. The arbiter's own record of what each
/// session holds is not touched: a release that arrives for a press made on
/// the old server goes to the new one, where it releases nothing (#3918).
pub(crate) fn let_go(injector: &mut Option<Box<dyn InputInjector + Send>>, was: u64, now: u64) {
    if let Some(mut old) = injector.take() {
        old.server_gone();
        drop(old);
        tracing::info!(
            was,
            now,
            "input: this device's virtual desktop is back on a new X server — the injector opens \
             on it, no restart needed (#3918)"
        );
    }
}

/// #3918 — the sessions that have no picture of this daemon's virtual
/// desktop: those told so by FR-80's notice with the desktop's own reason (a
/// session's open refused while the desktop is down, a shared pipeline's
/// owner telling its followers, a join's replay; `super::told_no_picture`),
/// and every session open at an X server exit, which the supervisor marks
/// before it reads the device's plan or the tell's kill switch
/// (`super::mark_no_picture`). Marked from any thread, read by the arbiter's
/// worker ([`Sessions`]) and by a shared pipeline's join
/// (`media_share::try_join`), each through [`read`] with
/// `ROOMLERD_INPUT_X11_FOLLOW` ([`super::enabled`]). A leaf lock, held across
/// nothing and used as it is when poisoned. A mark goes at the session's
/// close ([`Sessions::closed`]) and nowhere else: a session that gets frames
/// again (a follower promoted, a transport that reopens its capture) stays
/// marked, as its viewer keeps the notice up until the person reconnects.
pub(crate) mod no_picture {
    use bson::oid::ObjectId;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard, PoisonError};

    /// The marked sessions: as many as were open at an exit, so a list.
    static MARKED: Mutex<Vec<ObjectId>> = Mutex::new(Vec::new());
    /// How many are marked, written under the lock: with none marked, a
    /// reader reads only this.
    static COUNT: AtomicUsize = AtomicUsize::new(0);

    fn marked() -> MutexGuard<'static, Vec<ObjectId>> {
        MARKED.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn mark(session: ObjectId) {
        let mut marked = marked();
        if !marked.contains(&session) {
            marked.push(session);
            COUNT.store(marked.len(), Ordering::Release);
        }
    }

    pub(crate) fn is_marked(session: ObjectId) -> bool {
        COUNT.load(Ordering::Acquire) != 0 && marked().contains(&session)
    }

    /// What a reader of the marks reads (`input::has_no_picture`): whether
    /// `session` is marked, with `ROOMLERD_INPUT_X11_FOLLOW` given (`on`).
    /// With the switch off no mark is read: `false`, as before #3918.
    pub(crate) fn read(session: ObjectId, on: bool) -> bool {
        on && is_marked(session)
    }

    pub(crate) fn forget(session: ObjectId) {
        if COUNT.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut marked = marked();
        marked.retain(|s| *s != session);
        COUNT.store(marked.len(), Ordering::Release);
    }

    /// Every marked session; none in one read.
    pub(crate) fn all() -> Vec<ObjectId> {
        if COUNT.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        marked().clone()
    }
}

/// #3918 — which sessions control the desktop. The rule: a session marked as
/// having no picture of this desktop has no input of it either, while its
/// input would reach the desktop's X server. The mark is one fact, set
/// where it is known ([`no_picture`]): FR-80's notice with the desktop's own
/// reason, and the supervisor at an X server exit, which marks every session
/// open at the exit before it reads the device's plan or the tell's kill
/// switch (`ROOMLERD_VIRTUAL_DESKTOP_TELL_SESSIONS=0`). Where the plan is the
/// desktop's, those sessions are also told. Elsewhere (an explicit
/// `drm_capture` or `portal_capture` arm, whose open may have fallen through
/// to the desktop's X server) and with the switch, the marks are made in
/// silence. A session opened while the desktop was down whose capture opens
/// only once the desktop is back was told nothing and sees the new desktop
/// live: its input reaches it.
///
/// A mark is a fact about the session, and what it costs is decided where the
/// session's input goes. A marked session is left behind
/// ([`ArbiterState::leave_behind`]) only while its input would reach an X
/// client of the desktop: no portal or mutter route takes it ([`routed`]),
/// and the injector is, or would open as, the desktop's X11 one. That is
/// decided again before every command, and a marked session whose input goes
/// elsewhere (a live route, uinput, any other injector) is taken back
/// ([`ArbiterState::take_back`]) and acts as on master. The injection point
/// holds the rule for each event as well (`arbiter::inject_all_of`): an event of
/// a marked session never reaches an X client of the desktop, whatever the
/// start of its command decided.
///
/// A left-behind session is one without input: its events are dropped before
/// they reach any injector, and so before the gate above lets the injector go
/// and opens one on the new server, and that is said once for the session, at
/// INFO, until it closes. Its floor requests and grants, its mode, keep busy
/// and layout switches are refused as for a session without an INPUT grant.
/// What it holds is forgotten, releasing nothing: its presses went with their
/// X server, so no release of them reaches the new one, through its close,
/// the modifier fence or a floor takeover, and keep busy is not held off by a
/// button it had down. The floor it holds, and a request it made, are given
/// up, so the person who reconnects does not wait out an idle takeover from
/// their own dead session, and the viewers are told. Taken back, it does not
/// get them back.
///
/// So where the desktop's X11 injector carries the input, a session open at an
/// exit has no input of the desktop for as long as it is open, told or not
/// (on master its input stayed on the X server that is gone). Where a route
/// or another injector carries it, a mark changes nothing, with one exception:
/// a command processed in a moment with no route (a
/// capture's reopen, the route owner's close) leaves the session behind for
/// that moment, and in exclusive mode the floor and a pending request it
/// gives up then are not given back.
/// `ROOMLERD_INPUT_X11_FOLLOW=0` leaves nobody behind, as before #3918
/// ([`enabled`]). Keep busy is not a session and is not concerned.
#[derive(Default)]
pub(crate) struct Sessions {
    /// The left-behind sessions already said.
    said: HashSet<ObjectId>,
}

impl Sessions {
    /// `session` closed: its mark goes, here and nowhere else.
    pub(crate) fn closed(&mut self, session: ObjectId) {
        no_picture::forget(session);
        self.said.remove(&session);
    }

    /// Before the arbiter decides anything: every marked session whose input
    /// would reach the desktop's X server is left behind
    /// ([`ArbiterState::leave_behind`]), every other marked session taken
    /// back. With the switch off, or no session marked, one read and nothing
    /// else: the routes are read only after both. `true` when that moved the
    /// floor or a request for it, which the viewers are then told.
    pub(crate) fn forget_left_behind(
        &mut self,
        state: &mut ArbiterState,
        inj: Option<&(dyn InputInjector + Send)>,
    ) -> bool {
        self.forget_left_behind_in(state, inj, enabled(), routed)
    }

    /// [`Sessions::forget_left_behind`] with the kill switch and the route
    /// given.
    fn forget_left_behind_in(
        &mut self,
        state: &mut ArbiterState,
        inj: Option<&(dyn InputInjector + Send)>,
        on: bool,
        routed: impl FnOnce() -> bool,
    ) -> bool {
        if !on {
            return false;
        }
        let marked = no_picture::all();
        if marked.is_empty() {
            return false;
        }
        // Where this command's events go: to the desktop's X server only
        // while no portal or mutter route takes them, and the injector is,
        // or would open as, an X client of the desktop, which runs in this
        // process. Decided again for every command: a route comes and goes
        // (a capture's reopen, its owner's close), and so can the injector.
        let ours = !routed()
            && match inj {
                Some(i) => i.x_generation().is_some(),
                None => super::x11_would_open(),
            };
        let x_bound = ours && now().is_some();
        let mut moved = false;
        for session in marked {
            if x_bound {
                moved |= state.leave_behind(session);
            } else {
                state.take_back(session);
            }
        }
        moved
    }

    /// One event of `session`, after [`Sessions::forget_left_behind`]. `true`
    /// when the session is left behind: the event is dropped, and that is
    /// said once for the session.
    pub(crate) fn drops(&mut self, state: &ArbiterState, session: ObjectId) -> bool {
        if !state.is_left_behind(session) {
            return false;
        }
        if self.said.insert(session) {
            tracing::info!(
                %session,
                "input: this session is marked as having no picture of this device's virtual \
                 desktop — its input is dropped while it would reach the desktop's X server \
                 (#3918; said once for this session)"
            );
        }
        true
    }
}

/// FR-45 P4 — a portal or mutter input route is live. It takes every event
/// ahead of the OS injector (`arbiter::inject_all_of`), so while one is, no
/// session's input goes to the desktop's X server. The routes are
/// process-wide, so a unit test never reads them: it sets a route of its own
/// on its thread ([`test_seam::route`]), and this one line is left to the
/// field.
fn routed() -> bool {
    #[cfg(test)]
    {
        test_seam::ROUTE.with(|r| r.borrow().is_some())
    }
    #[cfg(all(not(test), feature = "portal-capture"))]
    {
        crate::capture::portal::input_route::active().is_some()
    }
    #[cfg(all(not(test), not(feature = "portal-capture")))]
    {
        false
    }
}

/// A test's stand-in for the desktop, on the test's own thread.
#[cfg(test)]
pub(crate) mod test_seam {
    use crate::virtual_desktop::XServerNow;

    thread_local! {
        /// `Some(desktop)` replaces [`crate::virtual_desktop::x_server_now`]
        /// on this thread; `Some(None)` is a process with no desktop.
        pub(crate) static DESKTOP: std::cell::Cell<Option<Option<XServerNow>>> =
            const { std::cell::Cell::new(None) };
        /// [`super::gate`]'s down period last said, on this thread: tests
        /// run in parallel, and one process-wide slot let one test's period
        /// make another's line twice.
        pub(crate) static SAID: std::sync::atomic::AtomicU64 =
            const { std::sync::atomic::AtomicU64::new(u64::MAX) };
        /// A portal or mutter route on this thread: `Some` takes every event
        /// `arbiter::inject_all_of` offers it (`arbiter::route_takes`), ahead
        /// of the OS injector, as a live route does, and [`super::routed`]
        /// reads it. The real routes are process-wide: a test never
        /// registers one.
        pub(crate) static ROUTE: std::cell::RefCell<Option<Vec<crate::input::InputMsg>>> =
            const { std::cell::RefCell::new(None) };
        /// `Some(on)` answers [`super::enabled`] on this thread, as
        /// `ROOMLERD_INPUT_X11_FOLLOW` would: a test process cannot set the
        /// switch off for itself alone. Set only through [`follow`].
        pub(crate) static FOLLOW: std::cell::Cell<Option<bool>> =
            const { std::cell::Cell::new(None) };
    }

    /// `ROOMLERD_INPUT_X11_FOLLOW` as `on` on this thread until the guard
    /// drops, which restores what was there.
    #[must_use]
    pub(crate) fn follow(on: bool) -> FollowGuard {
        FollowGuard(FOLLOW.with(|f| f.replace(Some(on))))
    }

    /// Restores this thread's switch as it was before [`follow`].
    pub(crate) struct FollowGuard(Option<bool>);

    impl Drop for FollowGuard {
        fn drop(&mut self) {
            FOLLOW.with(|f| f.set(self.0));
        }
    }

    /// A route on this thread goes live (`true`, having taken nothing yet),
    /// or goes.
    pub(crate) fn route(live: bool) {
        ROUTE.with(|r| *r.borrow_mut() = live.then(Vec::new));
    }

    /// What the route on this thread took.
    pub(crate) fn routed_events() -> Vec<crate::input::InputMsg> {
        ROUTE.with(|r| r.borrow().clone().unwrap_or_default())
    }

    /// A test build's `arbiter::route_takes`, first in
    /// `arbiter::inject_all_of`: `msg` offered to a route on this thread,
    /// `true` when one took it.
    pub(crate) fn route_takes(msg: &crate::input::InputMsg) -> bool {
        ROUTE.with(|r| {
            r.borrow_mut()
                .as_mut()
                .map(|taken| taken.push(msg.clone()))
                .is_some()
        })
    }

    /// The desktop up on its X server of `generation`.
    pub(crate) fn up(generation: u64) {
        DESKTOP.with(|d| {
            d.set(Some(Some(XServerNow {
                generation,
                down: None,
            })))
        });
    }

    /// The desktop down in its down period `period`, its last server of
    /// `generation` gone.
    pub(crate) fn down(generation: u64, period: u64) {
        DESKTOP.with(|d| {
            d.set(Some(Some(XServerNow {
                generation,
                down: Some(period),
            })))
        });
    }

    /// No desktop in this process.
    pub(crate) fn none() {
        DESKTOP.with(|d| d.set(Some(None)));
    }

    /// What `f` logs on this thread, one string a line.
    pub(crate) fn logged(f: impl FnOnce()) -> Vec<String> {
        let log = roomler_log_capture::Captured::default();
        let subscriber = roomler_log_capture::fmt(log.clone())
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .finish();
        roomler_log_capture::with_default(subscriber, f);
        log.text().lines().map(String::from).collect()
    }

    /// The real desktop again, and no route.
    pub(crate) fn clear() {
        DESKTOP.with(|d| d.set(None));
        ROUTE.with(|r| *r.borrow_mut() = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::InputMsg;

    /// An injector of the given kind: an X client of the desktop opened
    /// under `Some(generation)`, or anything else (`None`).
    struct Kind(Option<u64>);

    impl InputInjector for Kind {
        fn inject(&mut self, _: InputMsg) -> anyhow::Result<()> {
            Ok(())
        }
        fn has_permission(&self) -> bool {
            true
        }
        fn x_generation(&self) -> Option<u64> {
            self.0
        }
    }

    fn at(generation: u64, down: Option<u64>) -> Option<XServerNow> {
        Some(XServerNow { generation, down })
    }

    /// An injector of the X server answering now is used as it is; one of an
    /// older server is let go; while the desktop is down nothing reaches an
    /// injector, opened or not.
    #[test]
    fn an_x_client_of_the_desktop_follows_its_generation() {
        let said = AtomicU64::new(u64::MAX);
        assert_eq!(gate_at(Some(3), at(3, None), &said), Gate::Pass);
        assert_eq!(
            gate_at(Some(3), at(4, None), &said),
            Gate::LeftBehind { was: 3, now: 4 }
        );
        assert_eq!(gate_at(Some(3), at(3, Some(2)), &said), Gate::Down);
        assert_eq!(gate_at(None, at(3, Some(2)), &said), Gate::Down);
        // Nothing open and the desktop up: the lazy open makes one.
        assert_eq!(gate_at(None, at(3, None), &said), Gate::Pass);
        // No desktop in this process.
        assert_eq!(gate_at(Some(3), None, &said), Gate::Pass);
        assert_eq!(gate_at(None, None, &said), Gate::Pass);
    }

    /// One line a down period, not one an event: the second and later events
    /// of a period are dropped in silence, and the next period says it again.
    #[test]
    fn a_down_period_is_said_once() {
        let said = AtomicU64::new(u64::MAX);
        let lines = test_seam::logged(|| {
            for _ in 0..50 {
                assert_eq!(gate_at(Some(0), at(0, Some(1)), &said), Gate::Down);
            }
            assert_eq!(
                gate_at(Some(0), at(1, None), &said),
                Gate::LeftBehind { was: 0, now: 1 }
            );
            for _ in 0..50 {
                assert_eq!(gate_at(Some(1), at(1, Some(2)), &said), Gate::Down);
            }
        });
        let down: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("virtual desktop is down"))
            .collect();
        assert_eq!(down.len(), 2, "one line a down period: {lines:#?}");
        assert!(down.iter().all(|l| l.contains("INFO")), "{down:#?}");
    }

    /// A uinput injector, an X11 seat that is not the virtual desktop, and a
    /// daemon without one: nothing follows, whatever the desktop says, and
    /// the kill switch restores the behaviour before #3918 for the desktop's
    /// own injector too.
    #[test]
    fn nothing_but_the_desktops_x11_injector_follows_it() {
        let said = AtomicU64::new(u64::MAX);
        let other: Box<dyn InputInjector + Send> = Box::new(Kind(None));
        let ours: Box<dyn InputInjector + Send> = Box::new(Kind(Some(0)));
        test_seam::down(0, 1);
        assert_eq!(gate_in(Some(other.as_ref()), true, &said), Gate::Pass);
        assert_eq!(gate_in(Some(ours.as_ref()), true, &said), Gate::Down);
        assert_eq!(gate_in(Some(ours.as_ref()), false, &said), Gate::Pass);
        test_seam::up(1);
        assert_eq!(gate_in(Some(other.as_ref()), true, &said), Gate::Pass);
        assert_eq!(
            gate_in(Some(ours.as_ref()), true, &said),
            Gate::LeftBehind { was: 0, now: 1 }
        );
        assert_eq!(gate_in(Some(ours.as_ref()), false, &said), Gate::Pass);
        // No desktop: an injector opened here records no generation.
        test_seam::none();
        assert_eq!(gate_in(Some(ours.as_ref()), true, &said), Gate::Pass);
        test_seam::clear();
    }

    /// #3918 — a session told that it has no picture of the desktop is marked
    /// for as long as it is open: marked twice is marked once, a later look
    /// finds it, and its close, and only its close, forgets it. A session
    /// never told is never marked.
    #[test]
    fn a_session_told_it_has_no_picture_stays_marked_until_its_close() {
        let (told, other) = (ObjectId::new(), ObjectId::new());
        crate::input::mark_no_picture(told);
        crate::input::mark_no_picture(told);
        let all = no_picture::all();
        assert_eq!(all.iter().filter(|s| **s == told).count(), 1, "{all:?}");
        assert!(crate::input::has_no_picture(told));
        assert!(!crate::input::has_no_picture(other));
        Sessions::default().closed(told);
        assert!(!crate::input::has_no_picture(told), "its close forgot it");
    }

    /// #3918 — FR-80's notice, the daemon's own function, marks its session
    /// when, and only when, the reason is this daemon's virtual desktop: the
    /// down sentence made by the tell and by a refused open. The same words
    /// under any other reason mark nothing, as before #3918, and the notice's
    /// payload is the same either way.
    #[test]
    fn the_notice_marks_its_session_only_for_the_virtual_desktops_own_reason() {
        use crate::capture::{CaptureUnavailable, CaptureUnavailableCode};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the notices");
        let (down, other) = (ObjectId::new(), ObjectId::new());
        let detail = "this device's virtual desktop is down: its X server (Xvfb) exited";
        let its = CaptureUnavailable::virtual_desktop_down(detail);
        let same_words = CaptureUnavailable::new(CaptureUnavailableCode::BackendError, detail);
        rt.block_on(async {
            for (session, reason) in [(down, its), (other, same_words)] {
                let stash = std::sync::Arc::new(tokio::sync::Mutex::new(None));
                crate::peer::spawn_capture_unavailable_notice(session, stash, reason);
            }
        });
        let (marked, not) = (
            crate::input::has_no_picture(down),
            crate::input::has_no_picture(other),
        );
        Sessions::default().closed(down);
        assert!(marked, "the desktop's own reason marked nothing");
        assert!(
            !not,
            "another reason with the same words marked its session"
        );
    }

    /// #3918 — `ROOMLERD_INPUT_X11_FOLLOW=0` turns off both readers of the
    /// marks, as before #3918: a marked session is left behind by nothing, so
    /// its input goes where the injector goes, and a shared pipeline it owns
    /// takes joiners (`media_share::try_join` reads `input::has_no_picture`,
    /// which is [`no_picture::read`] with the switch). With the switch on,
    /// the same session is left behind, and a pipeline it owns with nothing
    /// published of its capture (unknown, which fails closed) takes no joiner.
    /// The off position reaches `has_no_picture` through [`enabled`]
    /// (`try_join` itself: `media_share`'s
    /// `with_the_switch_off_a_marked_owners_pipeline_takes_a_joiner`).
    #[test]
    fn with_the_kill_switch_nothing_reads_a_mark() {
        let session = ObjectId::new();
        let ours: Box<dyn InputInjector + Send> = Box::new(Kind(Some(0)));
        let mut state = ArbiterState::default();
        state.open(session, "A".into(), true, None, std::time::Instant::now());
        crate::input::mark_no_picture(session);
        test_seam::up(1);
        let mut follow = Sessions::default();
        // With the switch off the routes are not read.
        let mut routes_read_off = false;
        follow.forget_left_behind_in(&mut state, Some(ours.as_ref()), false, || {
            routes_read_off = true;
            false
        });
        let off = state.is_left_behind(session);
        let refuses_joins_off = no_picture::read(session, false);
        follow.forget_left_behind_in(&mut state, Some(ours.as_ref()), true, || false);
        let on = state.is_left_behind(session);
        let refuses_joins_on = no_picture::read(session, true);
        // What `try_join` and the injection point read: the mark with the
        // switch, off and on, as the environment would set it.
        let wired = {
            let _off = test_seam::follow(false);
            let off = crate::input::has_no_picture(session);
            let _on = test_seam::follow(true);
            (off, crate::input::has_no_picture(session))
        };
        follow.closed(session);
        test_seam::clear();
        assert_eq!(
            wired,
            (false, true),
            "has_no_picture does not read the switch"
        );
        assert!(!off, "left behind with ROOMLERD_INPUT_X11_FOLLOW=0");
        assert!(
            !routes_read_off,
            "the routes were read with ROOMLERD_INPUT_X11_FOLLOW=0"
        );
        assert!(
            !refuses_joins_off,
            "a pipeline it owns refuses joiners with ROOMLERD_INPUT_X11_FOLLOW=0"
        );
        assert!(on, "a marked session was not left behind");
        assert!(refuses_joins_on, "a pipeline it owns takes joiners");
    }
}
