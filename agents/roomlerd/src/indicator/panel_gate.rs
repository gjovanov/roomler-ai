// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #4053 — whether a Mac's consent panel can show a question now, and
//! (#4119) in which order that is read.
//!
//! On macOS the root daemon draws its own consent panel ([`super::mac`],
//! FR-27). It can only while the window server serves it, and the release
//! labs measured three ways it stops being served, for the rest of the
//! process's life:
//!
//! | When | What the window server does | The panel before the gate's fix |
//! |---|---|---|
//! | launchd starts the daemon at boot, before anyone signs in (0.4.226) | refuses it: "This program is not permitted to connect to or launch Window Server before login.", then "Conn 0x0 is not a valid connection ID." | took each free question, logged it as shown (`native=true`), and nothing came up (#4053) |
//! | a log-out that detaches it, with the daemon running on (a one-row Mac, 0.4.226 and 0.4.228) | "Detaching application from window server"; after the next login it is still on connection 0 ("invalid display identifier"), and a window made on it gets no window device (number 0) | the same (#4053) |
//! | a log-out that KILLS its port, with the daemon running on (a two-row Mac's root half, 0.4.228) | "HIToolbox: received notification of WindowServer event port death"; a window made on it fails (`_CGSCreateWindow returned 268435459`, `NewWindowWithOpaqueShapeAndContext null window`), and AppKit ends the process with exit 0 while it makes that window | #4053's probe was such a window: the first question after the log-in ended the root half, with no line of ours, and launchd, which relaunches only a non-zero exit, left it down until the next boot (#4119) |
//! | the daemon (re)started while the person is signed in | serves it: the panel is on screen | shows the question |
//!
//! Which log-outs kill the port is not known: a one-row Mac's left it
//! detached, a two-row Mac's root half's killed it, in two runs of two.
//!
//! So the panel asks this gate at each question, and the gate answers
//! `false` when the panel cannot reach the person at the screen now. Three
//! things decide, in this order, and the first two make no window:
//!
//! 1. **the seat**, in a root process only (#4119, [`PanelGate::seat_looked`]):
//!    the person socket's look at who is at the seat, every 5 s, told to the
//!    gate between questions. A seat that empties or changes hands since the
//!    gate's first look (a log-out, a login as someone else, a user switch)
//!    ends the panel: `false` from then on, with no read at all. A look that
//!    could not tell changes nothing, and a seat that goes from nobody to a
//!    person (a boot, then a login) ends nothing. The seat is HELD from the
//!    change on, never read at a question: after a log-out and a login it
//!    finds someone at it again.
//! 2. **the console session**, in a root process only
//!    ([`reads_console_session`]): another console session started since the
//!    pump's start (a log-out and a login, a user switch, a restart of
//!    loginwindow) means the person the panel could reach is gone: `false`
//!    from then on, answered BEFORE the connection is read (#4119), so that
//!    question makes no window. One the reader could not read leaves the
//!    connection to be read, as before: there the seat covers a log-out.
//! 3. **the connection**: whether this process can put a window on the
//!    window server now, read once at the pump's start and again at each
//!    question the first two let through, with a 1×1 probe window. Refused at
//!    the start: `false` from the first question on. Gone later: `false` from
//!    then on. No answer in time: `false` for that question.
//!
//! Why that order (#4119): the connection's read is a window, and on a dead
//! port the window itself ends the process (the table's third row). In the
//! lab run that watched them, both console-session names posted at that
//! log-out (`GUISessionUserChanged` at the login too), so the session read
//! alone would have made no window; the seat is the second guard, for
//! a Mac where the names do not post. Under both, an exit that AppKit begins
//! inside one of our window calls anyway, and that our code did not choose,
//! leaves with a code of its own, which launchd relaunches
//! ([`super::exit_net`]). A per-user process follows neither the seat nor
//! the console session: its connection belongs to a GUI session of its own,
//! which a fast user switch away and back leaves alive, so the connection
//! alone decides for it.
//!
//! ⚠️ This `false` is not a refusal. It takes the road of a panel already
//! showing another question: the caller marks the question for the person's
//! app (`PromptSurface::Companion`) and, on a one-row Mac, the app gets it
//! over the root daemon's relay (#3864, #3967). Never turn it into a deny or
//! an accept. #4164 — it is no panel here, though, never a busy one
//! ([`reaches_the_screen`]): if nothing else shows the question, the caller's
//! words say the panel was busy only for a panel that showed another.
//!
//! The operator's ruling on #4053 keeps it there: after a reboot or a log-out
//! every question goes to the app until the root daemon's next start, and
//! nothing here restarts anything (a restart at each login would end every
//! live session on the Mac). Drawing the panel from a process in the person's
//! own session is FR-149 (#4054).
//!
//! Each change is logged once, never a line per question. Every line carries
//! `window_number` and `console_session` (`none` where that was not read),
//! but the seat's, which names the seat instead (`seat_was`, `seat_now`).
//! The readers are macOS's ([`super::mac`], [`super::console_watch`], the
//! person socket's look); here they are a trait and a call, so these cells
//! run on every lane.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// What the connection read answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Connection {
    /// This process can put a window on the window server: a probe window's
    /// number (above 0), which AppKit gives only a window with a window device.
    Works(isize),
    /// It cannot: a probe window got no window device, so its number is 0 or
    /// less. Refused before login, or detached at a log-out.
    Refused(isize),
    /// The read had no answer in time: the main thread did not run it.
    NotKnown,
}

impl Connection {
    /// The probe window's number, `None` when the read had no answer.
    pub(crate) fn number(self) -> Option<isize> {
        match self {
            Self::Works(n) | Self::Refused(n) => Some(n),
            Self::NotKnown => None,
        }
    }
}

/// The two reads the gate makes at each question. macOS's are in
/// [`super::mac`]; a test stands them in.
pub(crate) trait Screen {
    /// Whether this process can put a window on the window server now.
    fn connection(&self) -> Connection;
    /// The console session now, as a value that stays the one read at the
    /// pump's start for as long as no other console session has started
    /// since. `None` when it cannot be read, and in a process that reads
    /// none ([`reads_console_session`]).
    fn console_session(&self) -> Option<u64>;
}

/// Whether a process with this effective uid reads the console session: a
/// root process only (a one-row Mac's daemon, a two-row Mac's root half).
///
/// A root daemon's window-server connection belongs to the console session
/// it started in, so another one since leaves its panel reaching nobody. A
/// per-user process (a two-row Mac's per-user half) belongs to a GUI session
/// of its own, which a fast user switch away and back leaves alive, and the
/// window server posts the console-session names for that switch: a read
/// there would end its panel at the first switch, and its row's questions
/// would go to the app until it restarted. So it builds its gate with no
/// console session, which the gate takes as the same one at every question,
/// and the connection alone decides for it.
pub(crate) fn reads_console_session(euid: u32) -> bool {
    euid == 0
}

/// What the gate answers a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The panel can show it.
    Shows,
    /// The window server refused this process when the pump started.
    RefusedAtStart,
    /// The connection worked at the pump's start and is gone now.
    Lost,
    /// Another console session has started since the pump's start.
    AnotherSession,
    /// #4119 — the person at the seat left it, or another took it, since the
    /// gate's first look at the seat ([`PanelGate::seat_looked`]), in a root
    /// process.
    SeatChanged,
    /// A read had no answer in time, or could not be made.
    NotKnown,
}

impl Verdict {
    /// Whether the panel takes the question.
    pub(crate) fn shows(self) -> bool {
        self == Self::Shows
    }
}

/// #4119 — who a look at the seat found: the person socket's look on a Mac
/// (`/dev/console`'s owner, `person_socket::follow_the_seat`), less a look
/// that could not tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seat {
    /// A person, by uid. Never root.
    Person(u32),
    /// Nobody: the login window, or root.
    Nobody,
}

impl Seat {
    /// The seat a look found, as the person socket's own step reads it
    /// (`person_socket::step`): root at the seat is nobody. `None` for a look
    /// that could not tell, which changes nothing.
    pub(crate) fn of(look: &crate::person_socket::Look) -> Option<Self> {
        use crate::person_socket::Look;
        match look {
            Look::Person(uid) if *uid != 0 => Some(Self::Person(*uid)),
            Look::Person(_) | Look::Nobody => Some(Self::Nobody),
            Look::NotKnown(_) => None,
        }
    }
}

impl std::fmt::Display for Seat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Person(uid) => uid.fmt(f),
            Self::Nobody => f.write_str("nobody"),
        }
    }
}

/// A value for a log line, `none` where there is none.
struct OrNone<T>(Option<T>);

impl<T: std::fmt::Display> std::fmt::Display for OrNone<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(v) => v.fmt(f),
            None => f.write_str("none"),
        }
    }
}

/// The gate: what the two reads said at the pump's start, and what they have
/// said since.
pub(crate) struct PanelGate {
    start_connection: Connection,
    start_session: Option<u64>,
    /// #4119 — whether the seat can end this panel ([`Self::seat_looked`]):
    /// in a root process only, the one that reads the console session
    /// ([`reads_console_session`]).
    follows_the_seat: bool,
    /// Whether [`Self::announce`] has said its line: once a process, however
    /// many organizations' loops make a panel.
    announced: AtomicBool,
    state: Mutex<State>,
}

struct State {
    /// A verdict that holds for the rest of the process's life once it is
    /// seen: the connection gone, another console session started, or the
    /// seat emptied or changed hands. No read is made after it.
    ended: Option<Verdict>,
    /// What the last question was told, so that each change is logged once.
    told: Verdict,
    /// #4119 — the last look at the seat that told who is there: a person,
    /// or nobody. A look that could not tell never lands here.
    seat: Option<Seat>,
}

impl PanelGate {
    /// The gate, from what the two reads said at the pump's start. A
    /// connection that did not work then never shows a question: the window
    /// server does not take back a process it refused.
    pub(crate) fn new(connection: Connection, session: Option<u64>) -> Self {
        let told = if matches!(connection, Connection::Works(_)) {
            Verdict::Shows
        } else {
            Verdict::RefusedAtStart
        };
        Self {
            start_connection: connection,
            start_session: session,
            follows_the_seat: false,
            announced: AtomicBool::new(false),
            state: Mutex::new(State {
                ended: None,
                told,
                seat: None,
            }),
        }
    }

    /// #4119 — the same gate, ended by the seat as well when `follows` (a
    /// root process: [`reads_console_session`]). A per-user process's panel
    /// belongs to a GUI session of its own, which a fast user switch away and
    /// back leaves alive, so the seat never ends it.
    pub(crate) fn following_the_seat(mut self, follows: bool) -> Self {
        self.follows_the_seat = follows;
        self
    }

    /// #4119 — a look at the seat, from the person socket's poll in a root
    /// process (`indicator::mac::seat_looked`, every 5 s). A seat that the
    /// last look found a person at, and that this one finds empty or someone
    /// else's, ends the panel for the process's life: one WARN that names
    /// the seat, and no read at a question after it, so no window either.
    /// The gate's first look only sets where the seat stands, and a seat
    /// that goes from nobody to a person (a boot before anyone signed in,
    /// then a login) ends nothing. It is held, never read at a question:
    /// after a log-out and a login the seat finds someone at it again. A
    /// gate that does not follow the seat (a per-user process), or whose
    /// connection was refused at the start, takes no look.
    pub(crate) fn seat_looked(&self, seat: Seat) {
        if !self.follows_the_seat || !matches!(self.start_connection, Connection::Works(_)) {
            return;
        }
        let mut state = self.state();
        let Some(Seat::Person(was)) = state.seat.replace(seat) else {
            return;
        };
        if seat == Seat::Person(was) || state.ended.is_some() {
            return;
        }
        state.ended = Some(Verdict::SeatChanged);
        state.told = Verdict::SeatChanged;
        say_the_seat(was, seat);
    }

    /// #4119 — whether the panel has ended for this process's life, which a
    /// probe asks again on the main thread, just before it makes its window.
    pub(crate) fn has_ended(&self) -> bool {
        self.state().ended.is_some()
    }

    /// The panel's line when it comes up (`Inner::new`): available, or in its
    /// place one WARN that says why it shows no question. Said once.
    pub(crate) fn announce(&self) {
        if self.announced.swap(true, Ordering::SeqCst) {
            return;
        }
        let window_number = OrNone(self.start_connection.number());
        let console_session = OrNone(self.start_session);
        if matches!(self.start_connection, Connection::Works(_)) {
            tracing::info!(
                %window_number,
                %console_session,
                "macOS consent panel available"
            );
        } else {
            tracing::warn!(
                %window_number,
                %console_session,
                "macOS consent panel: this process could not put a window on the screen when it \
                 started (a root daemon that launchd started before anyone signed in), so the \
                 panel shows no consent question, and each goes to the person's app until \
                 roomlerd starts again with someone signed in (#4053)"
            );
        }
    }

    /// What the gate answers a question, read now: nothing once the panel
    /// has ended (the seat included), then the console session, then, unless
    /// that says another session (#4119), the connection. The reads are made
    /// with no lock held: the connection's waits for the main thread. Each
    /// change of answer is logged once: a WARN when the panel stops showing
    /// questions, an INFO when it shows them again (only after a read that
    /// had no answer).
    pub(crate) fn verdict(&self, screen: &dyn Screen) -> Verdict {
        if !matches!(self.start_connection, Connection::Works(_)) {
            // `announce` said it; nothing to read.
            return Verdict::RefusedAtStart;
        }
        if let Some(ended) = self.state().ended {
            return ended;
        }
        let session = screen.console_session();
        let now = self.session_now(session);
        // #4119 — another console session answers before the connection is
        // read. That read is a window, and after a log-out that kills the
        // window-server port AppKit ends the process while it makes one. A
        // session read with no answer still reads the connection, as before:
        // there the seat is what covers a log-out.
        let connection = match now {
            Session::Another => None,
            Session::Same | Session::NotKnown => Some(screen.connection()),
        };
        let verdict = match (now, connection) {
            (Session::Another, _) => Verdict::AnotherSession,
            (_, Some(Connection::Refused(_))) => Verdict::Lost,
            (Session::NotKnown, _) | (_, Some(Connection::NotKnown) | None) => Verdict::NotKnown,
            (Session::Same, Some(Connection::Works(_))) => Verdict::Shows,
        };
        let mut state = self.state();
        if let Some(ended) = state.ended {
            // A question asked meanwhile saw it end, and said so, or the seat
            // ended it (#4119).
            return ended;
        }
        if matches!(verdict, Verdict::Lost | Verdict::AnotherSession) {
            state.ended = Some(verdict);
        }
        if verdict != state.told {
            state.told = verdict;
            say(verdict, connection, session, self.start_session);
        }
        verdict
    }

    /// The console session read now, against the one at the start. One the
    /// reader could not read at the start, or a process that reads none
    /// ([`reads_console_session`]), leaves the connection to decide.
    fn session_now(&self, now: Option<u64>) -> Session {
        match (self.start_session, now) {
            (None, _) => Session::Same,
            (Some(_), None) => Session::NotKnown,
            (Some(start), Some(now)) if now == start => Session::Same,
            (Some(_), Some(_)) => Session::Another,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The console session now, against the one at the pump's start.
#[derive(Clone, Copy)]
enum Session {
    Same,
    Another,
    NotKnown,
}

/// The one line for a change of answer. `connection` is `None` when the
/// connection was not read (#4119: the console session answered first).
fn say(
    verdict: Verdict,
    connection: Option<Connection>,
    session: Option<u64>,
    at_start: Option<u64>,
) {
    let window_number = OrNone(connection.and_then(Connection::number));
    let console_session = OrNone(session);
    let console_session_at_start = OrNone(at_start);
    match verdict {
        Verdict::Shows => tracing::info!(
            %window_number,
            %console_session,
            "macOS consent panel: both reads answer again, and the panel shows consent questions \
             again (#4053)"
        ),
        Verdict::Lost => tracing::warn!(
            %window_number,
            %console_session,
            %console_session_at_start,
            "macOS consent panel: this process can no longer put a window on the screen (its \
             window-server connection is gone, as after a log-out), so the panel shows no consent \
             question, and each goes to the person's app until roomlerd starts again with someone \
             signed in (#4053)"
        ),
        Verdict::AnotherSession => tracing::warn!(
            %window_number,
            %console_session,
            %console_session_at_start,
            "macOS consent panel: another console session has started since the panel came up (a \
             log-out and a login, or a user switch), so the panel shows no consent question, and \
             each goes to the person's app until roomlerd starts again with someone signed in \
             (#4053)"
        ),
        Verdict::NotKnown => tracing::warn!(
            %window_number,
            %console_session,
            "macOS consent panel: a read of the screen had no answer in time (`none` above), so \
             consent questions go to the person's app until both reads answer again (#4053)"
        ),
        // `announce` says it, once.
        Verdict::RefusedAtStart => {}
        // `PanelGate::seat_looked` says it, once, when the seat changes.
        Verdict::SeatChanged => {}
    }
}

/// #4119 — the one line for a seat that ended the panel: who was at it, and
/// who is now (a uid, or `nobody`).
fn say_the_seat(was: u32, now: Seat) {
    tracing::warn!(
        seat_was = was,
        seat_now = %now,
        "macOS consent panel: the person at the seat has left it, or someone else has taken it, \
         since the panel came up (a log-out, a login, a user switch), so the panel shows no \
         consent question, and each goes to the person's app until roomlerd starts again with \
         someone signed in (#4119)"
    );
}

/// FR-149 P1 (C2) — where the AppKit main loop stands, for the panel
/// ([`super::mac`]): `main` promised it (`mac::promise_main_loop`, before it
/// starts the daemon's thread and hands its own to AppKit), and it pumps
/// (`run_main_loop`, once its first reads are made).
///
/// P0 read the gap on 0.4.228: a panel was built only once the loop pumped,
/// and the daemon's signalling loop, on its own thread, could build it first.
/// That refusal was kept for the process's life, so a screen worker whose
/// loop started late showed no question at all. Now the promise builds the
/// panel, and the loop's first pass opens it: a question before that goes to
/// the person's app, and none after it does for this reason. Nothing waits
/// for the loop, and nothing polls it: each question reads the flags once.
#[derive(Debug, Default)]
pub(crate) struct MainLoop {
    promised: AtomicBool,
    pumping: AtomicBool,
}

impl MainLoop {
    pub(crate) const fn new() -> Self {
        Self {
            promised: AtomicBool::new(false),
            pumping: AtomicBool::new(false),
        }
    }

    /// `main` will hand the main thread to AppKit.
    pub(crate) fn promise(&self) {
        self.promised.store(true, Ordering::SeqCst);
    }

    /// The loop pumps: its first reads are made.
    pub(crate) fn pumps(&self) {
        self.pumping.store(true, Ordering::SeqCst);
    }

    /// Whether a panel can be built now: the loop pumps, or `main` promised
    /// it. Neither in a process that never hands its main thread over (a CLI
    /// call, a test), which keeps the refusal it always had.
    pub(crate) fn builds_a_panel(&self) -> bool {
        self.promised.load(Ordering::SeqCst) || self.pumping.load(Ordering::SeqCst)
    }

    /// Whether the panel takes a question now: only while the loop pumps,
    /// since a question parked before then would wait for a pass that has
    /// not come. Read at each question, never kept.
    pub(crate) fn takes_a_question(&self) -> bool {
        self.pumping.load(Ordering::SeqCst)
    }
}

/// #4164 — whether the macOS panel reaches the screen for a question now:
/// the main loop takes one ([`MainLoop::takes_a_question`]), and then the
/// gate lets it through (`shows`, [`PanelGate::verdict`], read only then: its
/// connection read waits for a main thread that a loop not pumping never
/// runs). What `mac`'s panel asks first, with its slot unlocked
/// (`indicator::panel_takes`): `false` is no panel here, never a busy one.
pub(crate) fn reaches_the_screen(main_loop: &MainLoop, shows: impl FnOnce() -> bool) -> bool {
    main_loop.takes_a_question() && shows()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::files_log_tests::Logged;
    use std::cell::Cell;

    /// FR-149 P1 (C2) — a panel built after `main`'s promise and before the
    /// loop pumps takes no question until the loop's first pass, and every
    /// question from then on: the refusal does not outlive the loop's start.
    /// Without the promise (a CLI call, a test) no panel is built, as before.
    #[test]
    fn a_panel_built_before_the_main_loop_pumps_takes_questions_once_it_does() {
        let early = MainLoop::new();
        assert!(!early.builds_a_panel(), "no promise, no loop: no panel");
        early.promise();
        assert!(early.builds_a_panel(), "the promise builds the panel");
        assert!(
            !early.takes_a_question(),
            "before the loop pumps, a question goes to the app"
        );
        early.pumps();
        for question in 0..3 {
            assert!(
                early.takes_a_question(),
                "question {question} after the loop's start"
            );
        }
        let unpromised = MainLoop::new();
        unpromised.pumps();
        assert!(unpromised.builds_a_panel() && unpromised.takes_a_question());
    }

    /// The two reads, stood in: what each answers now, and how often each
    /// was made.
    struct StoodIn {
        connection: Cell<Connection>,
        session: Cell<Option<u64>>,
        connection_reads: Cell<usize>,
        session_reads: Cell<usize>,
    }

    impl StoodIn {
        fn new(connection: Connection, session: Option<u64>) -> Self {
            Self {
                connection: Cell::new(connection),
                session: Cell::new(session),
                connection_reads: Cell::new(0),
                session_reads: Cell::new(0),
            }
        }

        fn reads(&self) -> usize {
            self.connection_reads.get() + self.session_reads.get()
        }
    }

    impl Screen for StoodIn {
        fn connection(&self) -> Connection {
            self.connection_reads.set(self.connection_reads.get() + 1);
            self.connection.get()
        }
        fn console_session(&self) -> Option<u64> {
            self.session_reads.set(self.session_reads.get() + 1);
            self.session.get()
        }
    }

    const WORKS: Connection = Connection::Works(42);
    const REFUSED: Connection = Connection::Refused(0);
    const SESSION: Option<u64> = Some(0);

    /// The line `Inner::new` logs for a panel that can show questions.
    const AVAILABLE: &str = "macOS consent panel available";

    /// How many lines at `level` the log holds.
    fn lines_at(logged: &Logged, level: &str) -> usize {
        logged
            .text()
            .lines()
            .filter(|l| l.trim_start().starts_with(level))
            .count()
    }

    /// Cell 1 — a connection the window server refused when the pump
    /// started (a root daemon launchd started before anyone signed in): the
    /// panel takes no question, from the first one on, and the log has ONE
    /// WARN in place of "panel available", however many organizations' loops
    /// make a panel. No read is made at a question: the window server does
    /// not take back a process it refused.
    ///
    /// Red before #4053: the panel took every free question and logged it as
    /// shown, and nothing came up.
    #[test]
    fn a_connection_refused_at_the_start_takes_no_question_with_one_warn() {
        let logged = Logged::start(tracing::Level::INFO);
        let screen = StoodIn::new(WORKS, SESSION);
        let gate = PanelGate::new(REFUSED, SESSION);
        gate.announce();
        gate.announce();
        for _ in 0..5 {
            assert_eq!(gate.verdict(&screen), Verdict::RefusedAtStart);
            assert!(!gate.verdict(&screen).shows(), "a refused panel took it");
        }
        assert_eq!(screen.reads(), 0, "a question read the screen");
        let text = logged.text();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert_eq!(lines_at(&logged, "WARN"), 1, "{text}");
        assert!(
            text.contains("could not put a window on the screen when it started"),
            "{text}"
        );
        assert!(text.contains("window_number=0"), "{text}");
        assert!(text.contains("console_session=0"), "{text}");
        assert!(!text.contains(AVAILABLE), "{text}");
    }

    /// Cell 2 — a connection that worked at the start and is gone later (a
    /// log-out): the panel shows questions until then and takes none from the
    /// first question that finds it gone, with ONE WARN, when it is first
    /// seen gone. No read is made after that, so a read that would say it
    /// works again is never taken: the window server does not give it back.
    #[test]
    fn a_connection_lost_later_takes_no_question_from_then_on_with_one_warn() {
        let logged = Logged::start(tracing::Level::INFO);
        let screen = StoodIn::new(WORKS, SESSION);
        let gate = PanelGate::new(WORKS, SESSION);
        gate.announce();
        assert!(gate.verdict(&screen).shows());
        assert!(gate.verdict(&screen).shows());

        screen.connection.set(REFUSED);
        assert_eq!(gate.verdict(&screen), Verdict::Lost);
        let reads = screen.reads();
        screen.connection.set(WORKS);
        for _ in 0..5 {
            assert_eq!(gate.verdict(&screen), Verdict::Lost);
        }
        assert_eq!(screen.reads(), reads, "a read after the loss");
        let text = logged.text();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert_eq!(lines_at(&logged, "WARN"), 1, "{text}");
        assert!(
            text.contains("can no longer put a window on the screen"),
            "{text}"
        );
        assert!(text.contains("window_number=0"), "{text}");
        assert!(text.contains(AVAILABLE), "{text}");
        assert!(text.contains("window_number=42"), "{text}");
    }

    /// Cell 3 — a connection that works, in the console session of the
    /// pump's start: the panel takes the question, as it always did. Both
    /// reads are made at each question, and nothing is logged past "panel
    /// available".
    #[test]
    fn a_working_connection_in_the_same_session_takes_the_question_as_today() {
        let logged = Logged::start(tracing::Level::INFO);
        let screen = StoodIn::new(WORKS, SESSION);
        let gate = PanelGate::new(WORKS, SESSION);
        gate.announce();
        for asked in 1..=3 {
            assert_eq!(gate.verdict(&screen), Verdict::Shows);
            assert_eq!(screen.connection_reads.get(), asked);
            assert_eq!(screen.session_reads.get(), asked);
        }
        let text = logged.text();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains(AVAILABLE), "{text}");
        assert!(text.contains("window_number=42"), "{text}");
        assert!(text.contains("console_session=0"), "{text}");

        // A session the reader could not read at the start leaves the
        // connection to decide.
        let unread = StoodIn::new(WORKS, None);
        let gate = PanelGate::new(WORKS, None);
        assert!(gate.verdict(&unread).shows());
        unread.connection.set(REFUSED);
        assert_eq!(gate.verdict(&unread), Verdict::Lost);
    }

    /// Cell 4 — a console session other than the one at the pump's start (a
    /// log-out and a login, a user switch): the panel takes no question from
    /// then on, with ONE WARN at the change, even while the connection still
    /// works and even if the first session comes back: another one HAS
    /// started since. A log-out that loses the connection AND starts another
    /// session is one WARN too, the session's, which names both sessions:
    /// the session read answers before the connection is read, so no probe
    /// window is made (#4119), and its WARN says `window_number=none`. Until
    /// #4119 that case probed, and answered `Lost`.
    #[test]
    fn another_console_session_takes_no_question_from_then_on_with_one_warn() {
        let logged = Logged::start(tracing::Level::INFO);
        let screen = StoodIn::new(WORKS, SESSION);
        let gate = PanelGate::new(WORKS, SESSION);
        gate.announce();
        assert!(gate.verdict(&screen).shows());

        screen.session.set(Some(1));
        assert_eq!(gate.verdict(&screen), Verdict::AnotherSession);
        let reads = screen.reads();
        screen.session.set(SESSION);
        for _ in 0..5 {
            assert_eq!(gate.verdict(&screen), Verdict::AnotherSession);
        }
        assert_eq!(screen.reads(), reads, "a read after the change");
        let text = logged.text();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert_eq!(lines_at(&logged, "WARN"), 1, "{text}");
        assert!(
            text.contains("another console session has started since the panel came up"),
            "{text}"
        );
        // "available" said 42; the WARN read no connection.
        assert!(text.contains("window_number=42"), "{text}");
        assert!(text.contains("window_number=none"), "{text}");
        assert!(text.contains("console_session=1"), "{text}");
        assert!(text.contains("console_session_at_start=0"), "{text}");
        drop(logged);

        let logged = Logged::start(tracing::Level::INFO);
        let both = StoodIn::new(REFUSED, Some(2));
        let gate = PanelGate::new(WORKS, SESSION);
        assert_eq!(gate.verdict(&both), Verdict::AnotherSession);
        assert_eq!(gate.verdict(&both), Verdict::AnotherSession);
        assert_eq!(both.connection_reads.get(), 0, "a probe window was made");
        let text = logged.text();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert_eq!(lines_at(&logged, "WARN"), 1, "{text}");
        assert!(
            text.contains("another console session has started since the panel came up"),
            "{text}"
        );
        assert!(text.contains("window_number=none"), "{text}");
        assert!(text.contains("console_session=2"), "{text}");
        assert!(text.contains("console_session_at_start=0"), "{text}");
    }

    /// #4119 (a) — another console session answers before the connection is
    /// read. The connection's read is a probe window, and after a log-out
    /// that kills the window-server port, AppKit ends the process while it
    /// makes one (exit 0, which launchd does not relaunch). So the session
    /// read decides first, and the question that finds another session
    /// makes no window, nor does any after it.
    ///
    /// Red before #4119: that question read the connection once.
    #[test]
    fn another_console_session_answers_without_a_probe_window() {
        let logged = Logged::start(tracing::Level::INFO);
        let gate = PanelGate::new(WORKS, SESSION);
        let screen = StoodIn::new(WORKS, Some(1));
        assert_eq!(gate.verdict(&screen), Verdict::AnotherSession);
        assert_eq!(screen.connection_reads.get(), 0, "a probe window was made");
        assert_eq!(screen.session_reads.get(), 1);
        for _ in 0..3 {
            assert_eq!(gate.verdict(&screen), Verdict::AnotherSession);
        }
        assert_eq!(screen.reads(), 1, "a read after the change");
        let text = logged.text();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert_eq!(lines_at(&logged, "WARN"), 1, "{text}");
        assert!(text.contains("window_number=none"), "{text}");
        assert!(text.contains("console_session=1"), "{text}");
    }

    /// #4119 (b) — the seat ends a root process's panel. Once the person
    /// socket's look goes from a person to nobody (a log-out), or from one
    /// person to another (a login as someone else, a user switch), the gate
    /// answers `SeatChanged` for the rest of the process's life, with ONE
    /// WARN that names the seat, and reads nothing at a question: no probe
    /// window is made. The same person look after look changes nothing, and
    /// the first person coming back does not undo it. A root process whose
    /// console-session reader could not register (no session at the start)
    /// is ended by the seat all the same: the latch keys on the process, not
    /// on that reader.
    ///
    /// Red before #4119: the panel took the question after the seat emptied,
    /// and probed the connection for it.
    #[test]
    fn a_seat_that_empties_or_changes_hands_ends_a_root_panel_with_no_read() {
        let cases = [
            ("emptied", SESSION, Seat::Nobody),
            ("changed hands", SESSION, Seat::Person(502)),
            ("emptied, no console reader", None, Seat::Nobody),
        ];
        for (what, start, then) in cases {
            let logged = Logged::start(tracing::Level::INFO);
            let gate = PanelGate::new(WORKS, start).following_the_seat(true);
            let screen = StoodIn::new(WORKS, start);
            gate.announce();
            for _ in 0..3 {
                gate.seat_looked(Seat::Person(501));
                assert!(gate.verdict(&screen).shows(), "{what}");
            }
            assert!(!gate.has_ended(), "{what}");
            gate.seat_looked(then);
            // What a probe waiting for the main thread asks before its window.
            assert!(gate.has_ended(), "{what}");
            let reads = screen.reads();
            for _ in 0..5 {
                assert_eq!(gate.verdict(&screen), Verdict::SeatChanged, "{what}");
            }
            gate.seat_looked(Seat::Person(501));
            assert_eq!(gate.verdict(&screen), Verdict::SeatChanged, "{what}");
            assert_eq!(
                screen.reads(),
                reads,
                "{what}: a read after the seat changed"
            );
            let text = logged.text();
            assert_eq!(text.lines().count(), 2, "{what}: {text}");
            assert_eq!(lines_at(&logged, "WARN"), 1, "{what}: {text}");
            assert!(text.contains("the person at the seat"), "{what}: {text}");
            assert!(text.contains("seat_was=501"), "{what}: {text}");
            assert!(text.contains(&format!("seat_now={then}")), "{what}: {text}");
        }
    }

    /// #4119 — a seat that goes from nobody to a person (a boot before
    /// anyone signed in, then a login) latches nothing, and neither does a
    /// first look. A look that could not tell is no look at all: `Seat::of`
    /// gives none for it, so nothing reaches the gate. Root at the seat is
    /// nobody, as the person socket's own step reads it.
    #[test]
    fn a_login_after_nobody_and_a_look_that_cannot_tell_latch_nothing() {
        use crate::person_socket::Look;
        let logged = Logged::start(tracing::Level::INFO);
        let gate = PanelGate::new(WORKS, SESSION).following_the_seat(true);
        let screen = StoodIn::new(WORKS, SESSION);
        gate.seat_looked(Seat::Nobody);
        gate.seat_looked(Seat::Nobody);
        gate.seat_looked(Seat::Person(501));
        assert!(gate.verdict(&screen).shows());
        assert_eq!(logged.text(), "", "a line for a login");

        assert_eq!(Seat::of(&Look::Person(501)), Some(Seat::Person(501)));
        assert_eq!(Seat::of(&Look::Person(0)), Some(Seat::Nobody), "root");
        assert_eq!(Seat::of(&Look::Nobody), Some(Seat::Nobody));
        assert_eq!(Seat::of(&Look::NotKnown("a test".into())), None);
    }

    /// #4119 (b2) — a per-user process's panel is not ended by the seat: its
    /// gate reads no console session and follows no seat, because a fast
    /// user switch away and back leaves its own GUI session alive (cell 4c).
    /// The connection still decides for it.
    #[test]
    fn a_per_user_panel_is_not_ended_by_the_seat() {
        let logged = Logged::start(tracing::Level::INFO);
        let start = session_at_start(501);
        let per_user = PanelGate::new(WORKS, start).following_the_seat(reads_console_session(501));
        let screen = StoodIn::new(WORKS, None);
        per_user.announce();
        for seat in [Seat::Person(501), Seat::Nobody, Seat::Person(502)] {
            per_user.seat_looked(seat);
            assert!(per_user.verdict(&screen).shows(), "{seat}");
        }
        let text = logged.text();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains(AVAILABLE), "{text}");

        screen.connection.set(REFUSED);
        assert_eq!(per_user.verdict(&screen), Verdict::Lost);
    }

    /// A read with no answer in time (the main thread did not run the
    /// connection's, or the console session could not be read): that
    /// question goes to the app, with ONE WARN however many questions find
    /// it so, and the panel takes questions again once both reads answer,
    /// with one INFO. It is not a loss: the next question reads again.
    #[test]
    fn a_read_with_no_answer_sends_its_questions_to_the_app_until_both_answer() {
        let logged = Logged::start(tracing::Level::INFO);
        let screen = StoodIn::new(Connection::NotKnown, SESSION);
        let gate = PanelGate::new(WORKS, SESSION);
        for _ in 0..3 {
            assert_eq!(gate.verdict(&screen), Verdict::NotKnown);
        }
        screen.connection.set(WORKS);
        screen.session.set(None);
        assert_eq!(gate.verdict(&screen), Verdict::NotKnown);
        screen.session.set(SESSION);
        assert!(gate.verdict(&screen).shows());
        assert!(gate.verdict(&screen).shows());
        let text = logged.text();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert_eq!(lines_at(&logged, "WARN"), 1, "{text}");
        assert!(text.contains("window_number=none"), "{text}");
        assert_eq!(lines_at(&logged, "INFO"), 1, "{text}");
        assert!(text.contains("both reads answer again"), "{text}");
    }

    /// Cell 4c — only a root process reads the console session: a one-row
    /// Mac's daemon and a two-row Mac's root half run as root (0), a two-row
    /// Mac's per-user half as its person (501, the first account a Mac
    /// makes).
    #[test]
    fn only_a_root_process_reads_the_console_session() {
        assert!(reads_console_session(0), "a root process");
        assert!(!reads_console_session(501), "a per-user process");
    }

    /// The console session `PanelScreen::at_start` gives the gate in a
    /// process with this effective uid: its reader's first read in a root
    /// process, and none in any other, which has no reader.
    fn session_at_start(euid: u32) -> Option<u64> {
        if reads_console_session(euid) {
            SESSION
        } else {
            None
        }
    }

    /// Cell 4c — a fast user switch to a second account and back, as each
    /// process's gate sees it. The window server posts the console-session
    /// names for it. A root process's reader counts them: another console
    /// session, from then on, with ONE WARN (lab cell L10). A per-user
    /// process has no reader: its "available" line says
    /// `console_session=none`, and its panel takes the questions after the
    /// switch back with no line past that one (lab cell L9). The connection
    /// still decides for it: a lost one takes no question, with ONE WARN.
    ///
    /// Red with a reader in every process: the per-user half took no question
    /// after the first switch, for the rest of its life.
    #[test]
    fn a_user_switch_and_back_ends_the_panel_of_a_root_process_only() {
        let logged = Logged::start(tracing::Level::INFO);
        let start = session_at_start(0);
        let root = PanelGate::new(WORKS, start);
        let screen = StoodIn::new(WORKS, start);
        root.announce();
        assert!(root.verdict(&screen).shows());
        // The switch away and back: the reader counted the posts.
        screen.session.set(Some(1));
        for _ in 0..3 {
            assert_eq!(root.verdict(&screen), Verdict::AnotherSession);
        }
        let text = logged.text();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert_eq!(lines_at(&logged, "WARN"), 1, "{text}");
        assert!(
            text.contains("another console session has started since the panel came up"),
            "{text}"
        );
        assert!(text.contains("console_session_at_start=0"), "{text}");
        drop(logged);

        let logged = Logged::start(tracing::Level::INFO);
        let start = session_at_start(501);
        assert_eq!(start, None, "a per-user process read the console session");
        let per_user = PanelGate::new(WORKS, start);
        // No reader, so no session at a question either.
        let screen = StoodIn::new(WORKS, None);
        per_user.announce();
        // Before the switch, after the switch away, and after the switch back.
        for _ in 0..3 {
            assert!(per_user.verdict(&screen).shows());
        }
        let text = logged.text();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains(AVAILABLE), "{text}");
        assert!(text.contains("console_session=none"), "{text}");

        screen.connection.set(REFUSED);
        assert_eq!(per_user.verdict(&screen), Verdict::Lost);
        assert_eq!(per_user.verdict(&screen), Verdict::Lost);
        let text = logged.text();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert_eq!(lines_at(&logged, "WARN"), 1, "{text}");
        assert!(
            text.contains("can no longer put a window on the screen"),
            "{text}"
        );
    }

    /// Cell 5 — THE TRAP: a question the panel does not take because it
    /// cannot reach the screen takes the road of one it does not take
    /// because another question is up. Each caller marks it for the person's
    /// app (`surface: if native { Native } else { Companion }`: remote
    /// control at `signaling.rs:4408`, exec at `:5943`, SSH at `ssh.rs:2668`,
    /// a recording at `recording/remote.rs:1562`), and on a one-row Mac the
    /// app reads the root daemon's questions over the root daemon's relay
    /// (#3864). Here the root daemon's own LocalAPI answers the relay, so a
    /// `false` from each of the gate's verdicts is followed, beside a taken
    /// panel's, from the marker to the person's list and back: it is listed
    /// with a surface the app shows (every one but `native`,
    /// `agents/roomler-desktop/src/main.rs:414`), and the person's answer
    /// there decides it, an approval and a denial alike. Nothing denies it
    /// and nothing grants it on its own. The control: a question the panel
    /// takes is listed `native`, which the app leaves to the panel.
    ///
    /// The remote-control verdict for `native: false` on a one-row Mac, the
    /// app counted while the road is open, is
    /// `companion::tray_start_tests::a_one_row_tray_is_the_surface_only_while_its_road_is_open`.
    #[tokio::test]
    async fn a_question_the_panel_does_not_take_reaches_the_app_as_a_taken_panels_does() {
        use crate::consent::{
            ConsentBroker, Decision, Mode, PendingPrompt, PromptKind, PromptSurface,
        };
        use crate::consent_relay::{Relay, RootHalf, RootReply};
        use std::sync::Arc;
        use std::time::Duration;
        use tunnel_core::localapi::{Request, Response};

        /// The root daemon's LocalAPI, as its person socket serves it to the
        /// one-row Mac's screen worker.
        struct RootDaemon(crate::localapi_state::DaemonState);
        #[async_trait::async_trait]
        impl RootHalf for RootDaemon {
            async fn ask(&self, req: &Request) -> RootReply {
                RootReply::Answer(Box::new(tunnel_core::localapi::handle(req, &self.0)))
            }
        }

        let window = Duration::from_secs(30);
        let mode = Mode::Prompt { timeout: window };
        let broker =
            ConsentBroker::on_own_folder(mode, Err(anyhow::anyhow!("no data folder in this test")));
        let (_view, overlay) = tokio::sync::watch::channel(Default::default());
        let root = crate::localapi_state::DaemonState::new(
            "aid".into(),
            "host".into(),
            tunnel_core::localapi::DaemonMode::Service,
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            overlay,
            broker.clone(),
            None,
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            Default::default(),
        );
        let relay = Relay::new(Arc::new(RootDaemon(root)));

        let works = StoodIn::new(WORKS, SESSION);
        let free = PanelGate::new(WORKS, SESSION);
        let refused = PanelGate::new(REFUSED, SESSION);
        let lost = PanelGate::new(WORKS, SESSION);
        lost.verdict(&StoodIn::new(REFUSED, SESSION));
        let moved = PanelGate::new(WORKS, SESSION);
        moved.verdict(&StoodIn::new(WORKS, Some(1)));
        let left = PanelGate::new(WORKS, SESSION).following_the_seat(true);
        left.seat_looked(Seat::Person(501));
        left.seat_looked(Seat::Nobody);
        let no_answer = StoodIn::new(Connection::NotKnown, SESSION);
        // `native` for `Inner::prompt`'s answer to a panel already showing a
        // question (`mac.rs`, #4164's busy): the road every one below takes.
        let taken = false;
        let cases = [
            (
                "the panel shows another question",
                taken,
                PromptKind::RemoteControl,
                true,
            ),
            (
                "refused at the start",
                refused.verdict(&works).shows(),
                PromptKind::RemoteControl,
                false,
            ),
            (
                "lost at a log-out",
                lost.verdict(&works).shows(),
                PromptKind::Exec,
                true,
            ),
            (
                "another console session",
                moved.verdict(&works).shows(),
                PromptKind::Ssh,
                false,
            ),
            (
                "a read with no answer",
                free.verdict(&no_answer).shows(),
                PromptKind::Record,
                true,
            ),
            (
                "the seat emptied",
                left.verdict(&works).shows(),
                PromptKind::Exec,
                false,
            ),
        ];
        let surface = |native: bool| {
            // As each of the four callers writes it.
            if native {
                PromptSurface::Native
            } else {
                PromptSurface::Companion
            }
        };
        for (n, (what, native, kind, allow)) in cases.into_iter().enumerate() {
            assert!(!native, "{what}: the panel took the question");
            let session = format!("405300000000000000000a{n:02x}");
            let asking = broker.request_with_mode(&session, mode);
            let marker = PendingPrompt {
                kind,
                asked_by: "Alice",
                permissions: String::new(),
                detail: String::new(),
                org: String::new(),
                timeout: window,
                surface: surface(native),
            };
            broker
                .write_prompt(&session, &marker)
                .expect("the question is marked");

            // What the person's app lists on a one-row Mac.
            let listed = relay.listed(Vec::new()).await;
            let question = listed
                .iter()
                .find(|q| q.session_id == session)
                .unwrap_or_else(|| panic!("{what}: the relay did not list the question"));
            assert_eq!(question.surface, "companion", "{what}");
            assert_eq!(question.kind, kind.wire(), "{what}");

            // The person answers it there.
            let answer = relay.decided(&session, allow).await;
            assert!(
                matches!(answer, Response::ConsentDecided { ok: true }),
                "{what}: {answer:?}"
            );
            let decided = tokio::time::timeout(Duration::from_secs(5), asking)
                .await
                .unwrap_or_else(|_| panic!("{what}: the answer did not decide the question"));
            let expected = if allow {
                Decision::Granted
            } else {
                Decision::Denied
            };
            assert_eq!(decided, expected, "{what}");
        }

        // The control: a question the panel takes is listed for the panel.
        let native = free.verdict(&works).shows();
        assert!(native, "a free panel that reaches the screen");
        let session = "405300000000000000000aff";
        let _asking = broker.request_with_mode(session, mode);
        let marker = PendingPrompt {
            kind: PromptKind::RemoteControl,
            asked_by: "Alice",
            permissions: String::new(),
            detail: String::new(),
            org: String::new(),
            timeout: window,
            surface: surface(native),
        };
        broker
            .write_prompt(session, &marker)
            .expect("the question is marked");
        let listed = relay.listed(Vec::new()).await;
        let question = listed
            .iter()
            .find(|q| q.session_id == session)
            .expect("listed");
        assert_eq!(question.surface, "native");
    }

    /// #4164 — what the macOS panel's refusal says, read as `mac.rs` reads it
    /// ([`reaches_the_screen`] through `indicator::panel_takes`, with the
    /// panel's slot stood in): a second question while one is up is busy,
    /// and the first stays; no loop pumping, a connection refused at the
    /// start and one lost later are no panel, whatever the slot holds; and
    /// the screen is read with the slot unlocked, and not at all while no
    /// loop pumps.
    ///
    /// Red before #4164: every refusal was `false`, and the caller read a
    /// busy panel as no panel at all ("no one at its screen").
    #[test]
    fn the_macos_panel_says_busy_only_for_a_question_it_shows() {
        use crate::indicator::{NotShown, PromptView, panel_takes};
        use std::time::{Duration, Instant};
        let question = |id: &str| PromptView {
            session_hex: id.into(),
            title: "Command execution request".into(),
            lead: "Alice wants to run a command on this device.".into(),
            detail: "uptime".into(),
            permissions: String::new(),
            org: String::new(),
            expires_at: Instant::now() + Duration::from_secs(30),
        };
        let slot: Mutex<Option<PromptView>> = Mutex::new(None);
        let take = |main_loop: &MainLoop, gate: &PanelGate, screen: &StoodIn, id: &str| {
            panel_takes(
                || {
                    assert!(
                        slot.try_lock().is_ok(),
                        "the screen read under the slot's lock"
                    );
                    reaches_the_screen(main_loop, || gate.verdict(screen).shows())
                },
                &slot,
                question(id),
            )
        };
        let held = || slot.lock().unwrap().as_ref().map(|v| v.session_hex.clone());

        let main_loop = MainLoop::new();
        main_loop.promise();
        let screen = StoodIn::new(WORKS, SESSION);
        let gate = PanelGate::new(WORKS, SESSION);
        assert_eq!(
            take(&main_loop, &gate, &screen, "a"),
            Err(NotShown::NoPanel),
            "no loop pumps yet"
        );
        assert_eq!(
            screen.reads(),
            0,
            "the screen read with no loop to answer it"
        );
        assert_eq!(held(), None);

        main_loop.pumps();
        assert_eq!(take(&main_loop, &gate, &screen, "a"), Ok(()));
        assert_eq!(
            take(&main_loop, &gate, &screen, "b"),
            Err(NotShown::Busy),
            "a second question while one is up"
        );
        assert_eq!(held().as_deref(), Some("a"), "the first question stays");

        screen.connection.set(REFUSED);
        assert_eq!(
            take(&main_loop, &gate, &screen, "c"),
            Err(NotShown::NoPanel),
            "lost at a log-out, holding a question"
        );
        let refused = PanelGate::new(REFUSED, SESSION);
        let works = StoodIn::new(WORKS, SESSION);
        assert_eq!(
            take(&main_loop, &refused, &works, "d"),
            Err(NotShown::NoPanel),
            "refused at the start, holding a question"
        );
        *slot.lock().unwrap() = None;
        assert_eq!(
            take(&main_loop, &refused, &works, "e"),
            Err(NotShown::NoPanel),
            "refused at the start, nothing held"
        );
        assert_eq!(held(), None, "no panel takes nothing");
        assert_eq!(
            take(&main_loop, &PanelGate::new(WORKS, SESSION), &works, "f"),
            Ok(()),
            "a free panel that reaches the screen"
        );
    }
}
