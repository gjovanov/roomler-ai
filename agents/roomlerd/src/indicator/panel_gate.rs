// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #4053 — whether a Mac's consent panel can show a question now.
//!
//! On macOS the root daemon draws its own consent panel ([`super::mac`],
//! FR-27). It can only while the window server serves it, and the 0.4.226
//! lab measured two ways it stops being served, for the rest of the process's
//! life:
//!
//! | When | What the window server does | The panel until #4053 |
//! |---|---|---|
//! | launchd starts the daemon at boot, before anyone signs in | refuses it: "This program is not permitted to connect to or launch Window Server before login.", then "Conn 0x0 is not a valid connection ID." | took each free question, logged it as shown (`native=true`), and nothing came up |
//! | a log-out, with the daemon running on | detaches it ("Detaching application from window server"); after the next login it is still on connection 0 ("invalid display identifier") | the same |
//! | the daemon (re)started while the person is signed in | serves it: the panel is on screen | shows the question |
//!
//! So the panel asks this gate at each question. It reads two things
//! ([`Screen`]) and answers `false` when either says the panel cannot reach
//! the person at the screen now:
//!
//! - **the connection**: whether this process can put a window on the window
//!   server now, read once at the pump's start and again at each question.
//!   Refused at the start: `false` from the first question on. Gone later:
//!   `false` from then on. No answer in time: `false` for that question.
//! - **the console session**: another console session started since the
//!   pump's start (a log-out and a login, a user switch, a restart of
//!   loginwindow) means the person the panel could reach is gone: `false` from
//!   then on.
//!
//! ⚠️ This `false` is not a refusal. It is the answer a panel already showing
//! another question gives, and it takes the same road: the caller marks the
//! question for the person's app (`PromptSurface::Companion`) and, on a
//! one-row Mac, the app gets it over the root daemon's relay (#3864, #3967).
//! Never turn it into a deny or an accept.
//!
//! The operator's ruling on #4053 keeps it there: after a reboot or a log-out
//! every question goes to the app until the root daemon's next start, and
//! nothing here restarts anything (a restart at each login would end every
//! live session on the Mac). Drawing the panel from a process in the person's
//! own session is FR-149 (#4054).
//!
//! Each change is logged once, never a line per question, and every line
//! carries `window_number` and `console_session`. The readers are macOS's
//! ([`super::mac`], [`super::console_watch`]); here they are a trait, so these
//! cells run on every lane.

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
    /// since. `None` when it cannot be read.
    fn console_session(&self) -> Option<u64>;
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
    /// A read had no answer in time, or could not be made.
    NotKnown,
}

impl Verdict {
    /// Whether the panel takes the question.
    pub(crate) fn shows(self) -> bool {
        self == Self::Shows
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
    /// Whether [`Self::announce`] has said its line: once a process, however
    /// many organizations' loops make a panel.
    announced: AtomicBool,
    state: Mutex<State>,
}

struct State {
    /// A verdict that holds for the rest of the process's life once it is
    /// seen: the connection gone, or another console session started. No
    /// read is made after it.
    ended: Option<Verdict>,
    /// What the last question was told, so that each change is logged once.
    told: Verdict,
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
            announced: AtomicBool::new(false),
            state: Mutex::new(State { ended: None, told }),
        }
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

    /// What the gate answers a question, read now. The reads are made with
    /// no lock held: the connection's waits for the main thread. Each change
    /// of answer is logged once: a WARN when the panel stops showing
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
        let connection = screen.connection();
        let verdict = match (connection, self.session_now(session)) {
            (Connection::Refused(_), _) => Verdict::Lost,
            (_, Session::Another) => Verdict::AnotherSession,
            (Connection::NotKnown, _) | (_, Session::NotKnown) => Verdict::NotKnown,
            (Connection::Works(_), Session::Same) => Verdict::Shows,
        };
        let mut state = self.state();
        if let Some(ended) = state.ended {
            // A question asked meanwhile saw it end, and said so.
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
    /// reader could not read at the start leaves the connection to decide.
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
enum Session {
    Same,
    Another,
    NotKnown,
}

/// The one line for a change of answer.
fn say(verdict: Verdict, connection: Connection, session: Option<u64>, at_start: Option<u64>) {
    let window_number = OrNone(connection.number());
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::files_log_tests::Logged;
    use std::cell::Cell;

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
    /// session is one WARN too, the loss's, which names both sessions.
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
        assert!(text.contains("window_number=42"), "{text}");
        assert!(text.contains("console_session=1"), "{text}");
        assert!(text.contains("console_session_at_start=0"), "{text}");
        drop(logged);

        let logged = Logged::start(tracing::Level::INFO);
        let both = StoodIn::new(REFUSED, Some(2));
        let gate = PanelGate::new(WORKS, SESSION);
        assert_eq!(gate.verdict(&both), Verdict::Lost);
        assert_eq!(gate.verdict(&both), Verdict::Lost);
        let text = logged.text();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("can no longer put a window"), "{text}");
        assert!(text.contains("console_session=2"), "{text}");
        assert!(text.contains("console_session_at_start=0"), "{text}");
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

    /// Cell 5 — THE TRAP: a question the panel does not take because it
    /// cannot reach the screen takes the road of one it does not take
    /// because another question is up. Each caller marks it for the person's
    /// app (`surface: if native { Native } else { Companion }`: remote
    /// control at `signaling.rs:4355`, exec at `:5880`, SSH at `ssh.rs:2482`,
    /// a recording at `recording/remote.rs:1559`), and on a one-row Mac the
    /// app reads the root daemon's questions over the root daemon's relay
    /// (#3864). Here the root daemon's own LocalAPI answers the relay, so a
    /// `false` from each of the gate's verdicts is followed, beside a taken
    /// panel's, from the marker to the person's list and back: it is listed
    /// with a surface the app shows (every one but `native`,
    /// `agents/roomler-desktop/src/main.rs:410`), and the person's answer
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
        let no_answer = StoodIn::new(Connection::NotKnown, SESSION);
        // `Inner::prompt`'s own answer for a panel already showing a
        // question (`mac.rs`): the road every one below must take.
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
}
