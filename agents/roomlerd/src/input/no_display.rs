// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3787 — how loudly the X11 injector says it has no display to open.
//!
//! On Linux, enigo's only backend in this build is x11rb. With no `DISPLAY`,
//! x11rb answers `DisplayNotSet` before any I/O, and enigo then logs that
//! error and "no successful connection" at ERROR. A system install with nobody
//! signed in at the screen has no display by design, and its session already
//! tells the viewer why, so every input open there printed an ERROR pair that
//! was noise. `enigo_backend` no longer tries an open that cannot succeed. It
//! returns [`NoDisplayToOpen`], and `input::open_default` says it through
//! [`say`].
//!
//! Who is at the screen is read when a session's first line is said. The
//! desktop brief alone can be a minute old (`desktop_brief::REFRESH`): the
//! first session after a sign-out still saw the person there and said ERROR
//! (the 0.4.219 release lab: ERROR 35 s after a sign-out, INFO 88 s after).
//! The open still fails at once on the input arbiter's thread, which serves
//! every injector and must never wait for logind (#3830). The session's first
//! try leaves its line to a thread of its own (`no-display-line`), which asks
//! the seat through one look in flight (`crate::one_in_flight`, on a
//! `seat-look` thread), 1.5 s at most, and says the line at the level of what
//! it saw.
//! With no answer in time the brief's kind stands, which is the level this
//! said before, and the line says the seat was not known. A later try in the
//! session is a DEBUG repeat, unless a read of the seat newer than the line's
//! shows another kind.
//!
//! Pure but for the lines' memory and threads, so every test lane compiles
//! it, whatever the features.
#![cfg_attr(
    not(all(target_os = "linux", feature = "enigo-input")),
    allow(dead_code)
)]

#[cfg(any(target_os = "linux", test))]
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", test))]
use crate::one_in_flight::{Asked, OneInFlight};

/// Why there is no X display for this process to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoDisplay {
    /// The seat is known to show nobody at a graphical session: the login
    /// screen, nobody at a seat, or a text console. No display is expected.
    NobodySignedIn,
    /// Anything else: a person signed in on X11, on Wayland or in a session of
    /// any other type, or a seat not known yet. A display should exist, so its
    /// absence is a fault.
    ShouldExist,
}

impl NoDisplay {
    /// Which kind, from whether this process has a `DISPLAY` and from the
    /// seat's session class and type (`None` before the seat is first read).
    /// `None` when `DISPLAY` is set: then the open runs.
    pub fn of(display_set: bool, seat: Option<(&str, &str)>) -> Option<NoDisplay> {
        if display_set {
            return None;
        }
        Some(match seat {
            Some((session, kind)) if nobody_graphical(session, kind) => NoDisplay::NobodySignedIn,
            _ => NoDisplay::ShouldExist,
        })
    }

    /// The level the missing display is said at: INFO where none is
    /// expected, ERROR where one should exist.
    pub fn level(self) -> tracing::Level {
        match self {
            NoDisplay::NobodySignedIn => tracing::Level::INFO,
            NoDisplay::ShouldExist => tracing::Level::ERROR,
        }
    }
}

impl std::fmt::Display for NoDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NoDisplay::NobodySignedIn => {
                "no X display to open: this process has no DISPLAY, and nobody is signed in at a \
                 graphical session on this screen"
            }
            NoDisplay::ShouldExist => {
                "no X display to open: this process has no DISPLAY, though a person is signed in \
                 at this screen or the seat is not known yet"
            }
        })
    }
}

/// A seat known to show nobody at a graphical session: logind's class
/// `greeter` (the login screen) or `none` (nobody at a seat), or the type
/// `tty` (a text console). Anything else counts as a person who should have a
/// display, a session type this does not know included. Equality only.
fn nobody_graphical(session: &str, kind: &str) -> bool {
    matches!(session, "greeter" | "none") || kind == "tty"
}

/// The level to say `kind` at, given the kind last said in this session: a
/// repeat is DEBUG. Every input open is one try: the arbiter's warm, a first
/// event, each regrant (1 s, doubling to 60 s), and each keep-busy op, which
/// does not cache a Noop. Said every time, one session repeated it many times
/// a minute.
pub fn level_after(kind: NoDisplay, last: Option<NoDisplay>) -> tracing::Level {
    if last == Some(kind) {
        tracing::Level::DEBUG
    } else {
        kind.level()
    }
}

/// What `EnigoInjector::new` returns instead of trying an open that cannot
/// succeed: the kind the desktop brief shows, and when that brief began its
/// read of logind (`None` before the first brief). [`say`] reads the seat
/// afresh and falls back on this kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoDisplayToOpen {
    pub kind: NoDisplay,
    pub read: Option<Instant>,
}

impl std::fmt::Display for NoDisplayToOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.kind, f)
    }
}

impl std::error::Error for NoDisplayToOpen {}

/// What an open failed for, read from the error's type, never its text.
/// `None` for any other failure, which keeps its own line.
pub fn kind_of(e: &anyhow::Error) -> Option<NoDisplayToOpen> {
    e.downcast_ref::<NoDisplayToOpen>().copied()
}

/// Say that there is no display to open, once a session at its level, and
/// never wait: the input arbiter's thread calls this, and it serves every
/// injector. A session's first try leaves its line to a thread of its own,
/// which looks at the seat (the module's docs).
pub fn say(missing: NoDisplayToOpen) {
    #[cfg(target_os = "linux")]
    LINES.say(
        Seen {
            kind: missing.kind,
            at: missing.read,
        },
        seat_now,
        crate::desktop_brief::refresh_soon,
        log,
    );
    // Only Linux skips an open for a missing display (`EnigoInjector::new`).
    #[cfg(not(target_os = "linux"))]
    let _ = missing;
}

/// A session opened, or an injector did: the next missing display is said at
/// its own level again, after a look of its own.
pub fn forget() {
    #[cfg(target_os = "linux")]
    LINES.forget();
}

/// Wait for the line a missing display left to its own thread, if one is
/// still coming: for `roomlerd input-smoke`, a one-shot command that exits
/// right after its open. The daemon never waits for it.
pub fn wait_for_the_line() {
    #[cfg(target_os = "linux")]
    LINES.wait_for_the_line();
}

/// #3787 — how long a session's first line waits for its look at the seat:
/// as long as the seat0 lookups wait (`files::SEAT_LOOKUP_WAIT`).
const SEAT_LOOK_WAIT: Duration = Duration::from_millis(1500);

/// What the thread that says a session's first line is called
/// (`/proc/<pid>/task/*/comm` shows 15 bytes of it).
const LINE_THREAD: &str = "no-display-line";

/// A read of the seat: the kind it showed, and when its read of logind
/// began. `None` for a brief not read yet, which is older than any read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    kind: NoDisplay,
    at: Option<Instant>,
}

/// What a look at the seat answers: when it began, and the kind it read;
/// `None` when logind could not tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Look {
    at: Instant,
    kind: Option<NoDisplay>,
}

/// One line about a missing display.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    /// At `kind`'s level.
    Said { kind: NoDisplay, chosen: Chosen },
    /// At DEBUG: this session said `kind` already.
    Repeat(NoDisplay),
    /// At DEBUG: this session's first line still waits for its look.
    Waiting,
}

impl Line {
    fn level(&self) -> tracing::Level {
        match self {
            Line::Said { kind, .. } => kind.level(),
            Line::Repeat(_) | Line::Waiting => tracing::Level::DEBUG,
        }
    }
}

/// What chose a line's kind.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Chosen {
    /// A look at the seat, taken for this session's first line.
    Look,
    /// A desktop brief read after this session's last line.
    Brief,
    /// The desktop brief, which can be a minute old: the look gave no answer.
    NoAnswer(Unanswered),
}

/// Why a look at the seat gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unanswered {
    /// None within the wait: logind is slow, stopped or hung.
    NotInTime,
    /// logind told nothing: no `loginctl`, or it failed.
    CannotTell,
    /// The look failed: it panicked, or no thread could run it.
    Failed(String),
}

impl std::fmt::Display for Unanswered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unanswered::NotInTime => write!(f, "the seat was not known within {SEAT_LOOK_WAIT:?}"),
            Unanswered::CannotTell => f.write_str("logind could not tell who is at the seat"),
            Unanswered::Failed(why) => write!(f, "the look at the seat failed ({why})"),
        }
    }
}

/// A session's first line, from the brief at its first try and the look.
/// A look that answered decides, and the line stands on its read. A look that
/// ran out of time, could not tell or failed is no reading at all: the
/// brief's kind stands, the level this said before #3787's look, and the line
/// stands on the brief's own read, so the look never wins over the brief.
#[cfg(any(target_os = "linux", test))]
fn first_line(brief: Seen, look: Asked<Look>) -> (Line, Seen) {
    let (seen, chosen) = match look {
        Asked::Answer(Look {
            at,
            kind: Some(kind),
        }) => (Seen { kind, at: Some(at) }, Chosen::Look),
        Asked::Answer(Look { kind: None, .. }) => (brief, Chosen::NoAnswer(Unanswered::CannotTell)),
        Asked::Failed(why) => (brief, Chosen::NoAnswer(Unanswered::Failed(why))),
        Asked::OutOfTime => (brief, Chosen::NoAnswer(Unanswered::NotInTime)),
    };
    (
        Line::Said {
            kind: seen.kind,
            chosen,
        },
        seen,
    )
}

/// A later try in the same session, given the read its last line stood on
/// and the brief now: `None` is a repeat. The newer read decides. A brief
/// read before the line's (the person who signed out, still in a brief read
/// before the look) changes nothing; one read after it shows what changed
/// since (a person who signed in). A read with no instant is older than any.
fn next_line(said: Seen, brief: Seen) -> Option<(Line, Seen)> {
    let newer = if brief.at > said.at { brief } else { said };
    (level_after(newer.kind, Some(said.kind)) != tracing::Level::DEBUG).then_some((
        Line::Said {
            kind: newer.kind,
            chosen: Chosen::Brief,
        },
        newer,
    ))
}

/// The lines of one session at a time: what it said, the looks at the seat
/// its first line takes (one in flight however many sessions ask), and how
/// long a first line waits for one. The daemon's is `LINES`; a test makes
/// its own.
#[cfg(any(target_os = "linux", test))]
struct Lines {
    memory: Mutex<Memory>,
    looks: OneInFlight<Look>,
    wait: Duration,
    /// The thread saying the latest first line, for `input-smoke` to wait for.
    pending: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// What the session said so far.
#[derive(Debug)]
struct Memory {
    /// Moved on by every forget: a first line whose session has gone leaves
    /// the next session's memory alone.
    epoch: u64,
    state: State,
}

#[derive(Debug, Clone, Copy)]
enum State {
    /// Nothing said yet in this session.
    Quiet,
    /// The first line waits for its look: a try meanwhile is a repeat.
    Looking,
    /// A line said, standing on this read of the seat.
    Said(Seen),
}

#[cfg(any(target_os = "linux", test))]
impl Lines {
    const fn new(thread_name: &'static str, wait: Duration) -> Self {
        Self {
            memory: Mutex::new(Memory {
                epoch: 0,
                state: State::Quiet,
            }),
            looks: OneInFlight::new(thread_name),
            wait,
            pending: Mutex::new(None),
        }
    }

    /// One try's line, given the brief at that try. Never waits: a session's
    /// first try marks the memory and leaves its line to a thread of its own
    /// ([`Self::first_line_on_its_own_thread`]). A try while that line waits,
    /// or one that finds its kind said already, is a DEBUG repeat.
    fn say(
        &'static self,
        brief: Seen,
        lookup: impl FnOnce() -> Option<NoDisplay> + Send + 'static,
        stale_brief: impl FnOnce() + Send + 'static,
        emit: impl Fn(Line) + Clone + Send + 'static,
    ) {
        let mut memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        let state = memory.state;
        let line = match state {
            State::Quiet => {
                memory.state = State::Looking;
                let epoch = memory.epoch;
                drop(memory);
                self.first_line_on_its_own_thread(epoch, brief, lookup, stale_brief, emit);
                return;
            }
            State::Looking => Line::Waiting,
            State::Said(said) => match next_line(said, brief) {
                Some((line, newer)) => {
                    memory.state = State::Said(newer);
                    line
                }
                None => Line::Repeat(said.kind),
            },
        };
        drop(memory);
        emit(line);
    }

    /// The session's first line, said from a thread named `LINE_THREAD`:
    /// the look's kind within the wait, else the brief's and why. When the
    /// look saw another kind than the brief, the brief is asked to read again
    /// (`stale_brief`). Both run on that thread only, never on the caller's,
    /// which is the input arbiter's.
    fn first_line_on_its_own_thread(
        &'static self,
        epoch: u64,
        brief: Seen,
        lookup: impl FnOnce() -> Option<NoDisplay> + Send + 'static,
        stale_brief: impl FnOnce() + Send + 'static,
        emit: impl Fn(Line) + Clone + Send + 'static,
    ) {
        let emit_here = emit.clone();
        let spawned = std::thread::Builder::new()
            .name(LINE_THREAD.into())
            .spawn(move || {
                let look = self.looks.ask_blocking(self.wait, move || Look {
                    at: Instant::now(),
                    kind: lookup(),
                });
                let (line, seen) = first_line(brief, look);
                self.said(epoch, seen);
                let disagrees = matches!(
                    &line,
                    Line::Said { kind, chosen: Chosen::Look } if *kind != brief.kind
                );
                if disagrees {
                    stale_brief();
                }
                emit(line);
            });
        match spawned {
            Ok(thread) => *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread),
            Err(e) => {
                let why = format!("no thread to wait for it: {e}");
                let (line, seen) = first_line(brief, Asked::Failed(why));
                self.said(epoch, seen);
                emit_here(line);
            }
        }
    }

    /// The first line of `epoch` stands on `seen`, unless its session has
    /// gone since.
    fn said(&self, epoch: u64, seen: Seen) {
        let mut memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        if memory.epoch == epoch && matches!(memory.state, State::Looking) {
            memory.state = State::Said(seen);
        }
    }

    /// A session opened: its first try looks again.
    fn forget(&self) {
        let mut memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        memory.epoch = memory.epoch.wrapping_add(1);
        memory.state = State::Quiet;
    }

    /// Wait for the thread saying the latest first line, if there is one.
    fn wait_for_the_line(&self) {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(line) = pending {
            let _ = line.join();
        }
    }
}

/// The daemon's lines: their looks run on a thread named `seat-look`.
#[cfg(target_os = "linux")]
static LINES: Lines = Lines::new("seat-look", SEAT_LOOK_WAIT);

/// The look itself, on the look's own thread: what is at the screen now,
/// read from logind as the desktop brief reads it. `None` when logind cannot
/// tell.
#[cfg(target_os = "linux")]
fn seat_now() -> Option<NoDisplay> {
    let (session, kind) = crate::desktop_brief::seen_now()?;
    NoDisplay::of(false, Some((session.as_str(), kind.as_str())))
}

/// The daemon's sink for `LINES`: each line in the journal at its level.
#[cfg(target_os = "linux")]
fn log(line: Line) {
    let level = line.level();
    match line {
        Line::Said { kind, chosen } => {
            let unread = match chosen {
                Chosen::NoAnswer(why) => format!(
                    "; {why}, so the desktop brief chose this level, and it can be a minute old"
                ),
                Chosen::Look | Chosen::Brief => String::new(),
            };
            if level == tracing::Level::ERROR {
                tracing::error!("input: {kind} — input injection disabled (#3787){unread}");
            } else {
                tracing::info!("input: {kind} — no input until there is one (#3787){unread}");
            }
        }
        Line::Repeat(kind) => tracing::debug!("input: still {kind}"),
        Line::Waiting => tracing::debug!(
            "input: still no X display to open; this session's first line waits for its look at \
             the seat (#3787)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    /// #3787 — the 0.4.217 lab: a system install, a greeter at seat0, nobody
    /// signed in. No display is expected there, so the line is INFO.
    #[test]
    fn with_no_display_and_nobody_signed_in_the_open_is_said_at_info() {
        for seat in [("greeter", "wayland"), ("greeter", "x11"), ("none", "none")] {
            let kind = NoDisplay::of(false, Some(seat));
            assert_eq!(kind, Some(NoDisplay::NobodySignedIn), "{seat:?}");
            assert_eq!(kind.map(NoDisplay::level), Some(Level::INFO), "{seat:?}");
        }
        // A text console is a person, but not a display.
        let tty = NoDisplay::of(false, Some(("user", "tty")));
        assert_eq!(tty.map(NoDisplay::level), Some(Level::INFO));
    }

    /// #3787 — warm, the first event and each regrant are one try each, and
    /// keep busy can try on every op: within one session the kind is said
    /// once, and a repeat of it is DEBUG.
    #[test]
    fn a_repeat_within_one_session_is_said_at_debug() {
        let kind = NoDisplay::NobodySignedIn;
        assert_eq!(level_after(kind, None), Level::INFO);
        assert_eq!(level_after(kind, Some(kind)), Level::DEBUG);
    }

    /// The control: where a display should exist, its absence is still an
    /// ERROR, as on master, where enigo's own lines said it. Only a seat known
    /// to show nobody graphical is INFO: a session type this does not know
    /// is a person.
    #[test]
    fn a_display_that_should_exist_still_fails_at_error() {
        for seat in [
            Some(("user", "x11")),
            Some(("user", "wayland")),
            Some(("lock-screen", "x11")),
            Some(("user-incomplete", "wayland")),
            Some(("user", "unspecified")),
            Some(("lock-screen", "unspecified")),
            Some(("user", "mir")),
            None,
        ] {
            let kind = NoDisplay::of(false, seat);
            assert_eq!(kind, Some(NoDisplay::ShouldExist), "{seat:?}");
            assert_eq!(kind.map(NoDisplay::level), Some(Level::ERROR), "{seat:?}");
        }
        // A kind this session has not said yet is not a repeat: its own level.
        assert_eq!(
            level_after(NoDisplay::ShouldExist, Some(NoDisplay::NobodySignedIn)),
            Level::ERROR
        );
        assert_eq!(level_after(NoDisplay::ShouldExist, None), Level::ERROR);
    }

    /// With `DISPLAY` set nothing is decided here: the open runs as on
    /// master, and a failed connect logs enigo's own ERROR lines.
    #[test]
    fn only_an_unset_display_skips_the_open() {
        for seat in [Some(("greeter", "wayland")), Some(("user", "x11")), None] {
            assert_eq!(NoDisplay::of(true, seat), None, "{seat:?}");
        }
    }

    /// `open_default` picks the line by the error's type. Any other failure,
    /// even one worded the same way, keeps master's line. The brief's read
    /// instant travels with the type.
    #[test]
    fn the_kind_is_read_from_the_error_type_never_its_text() {
        let missing = NoDisplayToOpen {
            kind: NoDisplay::NobodySignedIn,
            read: Some(Instant::now()),
        };
        let typed = anyhow::Error::new(missing);
        assert_eq!(kind_of(&typed), Some(missing));
        let worded = anyhow::anyhow!("enigo init: {missing}");
        assert_eq!(kind_of(&worded), None);
        let enigo = anyhow::anyhow!(
            "enigo init: no connection could be established: (no successful connection)"
        );
        assert_eq!(kind_of(&enigo), None);
    }

    // #3787 — the first line of a session is chosen by a look at the seat.

    use std::sync::{Arc, Condvar, mpsc};
    use std::time::{Duration, Instant};

    /// Long enough that only a held look runs out of it.
    const LONG: Duration = Duration::from_secs(10);

    /// The release lab's gap: a session 35 s after a sign-out, whose brief
    /// was read before it.
    const AFTER_THE_SIGN_OUT: Duration = Duration::from_secs(35);

    /// A [`Lines`] for one test: `'static`, as the daemon's is.
    fn lines(wait: Duration) -> &'static Lines {
        Box::leak(Box::new(Lines::new("test-seat-look", wait)))
    }

    /// A sink that hands each line to the test, stamped when it was said.
    fn sink(tx: &mpsc::Sender<(Instant, Line)>) -> impl Fn(Line) + Clone + Send + 'static {
        let tx = tx.clone();
        move |line| {
            let _ = tx.send((Instant::now(), line));
        }
    }

    /// Looks held until the test lets them answer.
    #[derive(Clone, Default)]
    struct Held(Arc<(Mutex<bool>, Condvar)>);

    impl Held {
        /// A look that waits to be let go, then answers `kind`.
        fn look(
            &self,
            kind: Option<NoDisplay>,
        ) -> impl FnOnce() -> Option<NoDisplay> + Send + 'static {
            let held = self.clone();
            move || {
                let (go, gone) = &*held.0;
                let go = go.lock().unwrap_or_else(|e| e.into_inner());
                drop(
                    gone.wait_while(go, |go| !*go)
                        .unwrap_or_else(|e| e.into_inner()),
                );
                kind
            }
        }

        fn let_go(&self) {
            *self.0.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
            self.0.1.notify_all();
        }
    }

    /// Lets the held looks go when a test ends, whatever it asserted.
    struct LetGoAtTheEnd(Held);

    impl Drop for LetGoAtTheEnd {
        fn drop(&mut self) {
            self.0.let_go();
        }
    }

    /// #3787 — the release lab, 35 s after a sign-out: the brief, read
    /// before it, still shows the person, and the look sees the login
    /// screen. The line is INFO. On master the brief chose, and it was ERROR.
    #[test]
    fn a_stale_brief_with_a_person_and_a_look_at_nobody_say_info() {
        let read = Instant::now();
        let looked = read + AFTER_THE_SIGN_OUT;
        for brief in [
            Seen {
                kind: NoDisplay::ShouldExist,
                at: Some(read),
            },
            // No brief yet, which master also said at ERROR.
            Seen {
                kind: NoDisplay::ShouldExist,
                at: None,
            },
        ] {
            let look = Asked::Answer(Look {
                at: looked,
                kind: Some(NoDisplay::NobodySignedIn),
            });
            let (line, seen) = first_line(brief, look);
            assert_eq!(
                line,
                Line::Said {
                    kind: NoDisplay::NobodySignedIn,
                    chosen: Chosen::Look
                },
                "{brief:?}"
            );
            assert_eq!(line.level(), Level::INFO, "{brief:?}");
            assert_eq!(
                seen,
                Seen {
                    kind: NoDisplay::NobodySignedIn,
                    at: Some(looked)
                },
                "the line stands on the look's read"
            );
        }
    }

    /// The control: a look that sees a person signed in at the screen says
    /// ERROR, as the brief did.
    #[test]
    fn a_look_at_a_person_says_error() {
        let read = Instant::now();
        let look = || {
            Asked::Answer(Look {
                at: read + AFTER_THE_SIGN_OUT,
                kind: Some(NoDisplay::ShouldExist),
            })
        };
        for brief in [
            Seen {
                kind: NoDisplay::ShouldExist,
                at: Some(read),
            },
            Seen {
                kind: NoDisplay::ShouldExist,
                at: None,
            },
        ] {
            let (line, _) = first_line(brief, look());
            assert_eq!(line.level(), Level::ERROR, "{brief:?}");
        }
    }

    /// #3787 — a person signed in since the brief was read: the look sees
    /// them, so the missing display is an ERROR, where the brief, still
    /// showing the login screen, chose INFO.
    #[test]
    fn a_person_the_look_sees_is_an_error_though_the_brief_shows_the_login_screen() {
        let read = Instant::now();
        let brief = Seen {
            kind: NoDisplay::NobodySignedIn,
            at: Some(read),
        };
        let look = Asked::Answer(Look {
            at: read + AFTER_THE_SIGN_OUT,
            kind: Some(NoDisplay::ShouldExist),
        });
        let (line, _) = first_line(brief, look);
        assert_eq!(
            line,
            Line::Said {
                kind: NoDisplay::ShouldExist,
                chosen: Chosen::Look
            }
        );
        assert_eq!(line.level(), Level::ERROR);
    }

    /// #3787 — no answer from the look: the brief's kind stands, at the level
    /// master said it, and the line says why the seat was not known. A look
    /// that ran out of time, could not tell or failed is no reading, so the
    /// line stands on the brief's own.
    #[test]
    fn with_no_answer_the_brief_keeps_todays_level_and_the_line_says_why() {
        let read = Instant::now();
        let cases = [
            (NoDisplay::ShouldExist, Some(read), Asked::OutOfTime),
            (NoDisplay::NobodySignedIn, Some(read), Asked::OutOfTime),
            (NoDisplay::ShouldExist, None, Asked::OutOfTime),
            (
                NoDisplay::NobodySignedIn,
                Some(read),
                Asked::Answer(Look {
                    at: read + AFTER_THE_SIGN_OUT,
                    kind: None,
                }),
            ),
            (
                NoDisplay::ShouldExist,
                Some(read),
                Asked::Failed("it panicked: a look that fails".into()),
            ),
        ];
        for (kind, at, look) in cases {
            let brief = Seen { kind, at };
            let why = match &look {
                Asked::OutOfTime => Unanswered::NotInTime,
                Asked::Answer(_) => Unanswered::CannotTell,
                Asked::Failed(why) => Unanswered::Failed(why.clone()),
            };
            let (line, seen) = first_line(brief, look);
            assert_eq!(
                line,
                Line::Said {
                    kind,
                    chosen: Chosen::NoAnswer(why)
                },
                "{brief:?}"
            );
            assert_eq!(line.level(), kind.level(), "{brief:?}");
            assert_eq!(seen, brief, "no reading of its own");
        }
    }

    /// #3787 — after the look's INFO, a later try in the same session meets
    /// a brief read before the look: the older read, so it changes nothing.
    /// A brief never read is older than any look.
    #[test]
    fn a_brief_older_than_the_look_changes_nothing() {
        let read = Instant::now();
        let said = Seen {
            kind: NoDisplay::NobodySignedIn,
            at: Some(read + AFTER_THE_SIGN_OUT),
        };
        for stale in [Some(read), None] {
            let brief = Seen {
                kind: NoDisplay::ShouldExist,
                at: stale,
            };
            assert_eq!(next_line(said, brief), None, "{brief:?}");
        }
    }

    /// A brief read after the session's last line is the newer read: a kind
    /// that differs is said at its level, the same kind is a repeat. A line
    /// that stood on no read is older than any brief since.
    #[test]
    fn a_brief_newer_than_the_line_says_a_change_at_its_level() {
        let read = Instant::now();
        let later = Some(read + Duration::from_secs(60));
        let said = Seen {
            kind: NoDisplay::NobodySignedIn,
            at: Some(read),
        };
        let signed_in = Seen {
            kind: NoDisplay::ShouldExist,
            at: later,
        };
        let line = next_line(said, signed_in);
        assert_eq!(
            line,
            Some((
                Line::Said {
                    kind: NoDisplay::ShouldExist,
                    chosen: Chosen::Brief
                },
                signed_in
            ))
        );
        assert_eq!(line.map(|(line, _)| line.level()), Some(Level::ERROR));
        let same = Seen {
            kind: NoDisplay::NobodySignedIn,
            at: later,
        };
        assert_eq!(next_line(said, same), None);
        let unread = Seen {
            kind: NoDisplay::ShouldExist,
            at: None,
        };
        let nobody = Seen {
            kind: NoDisplay::NobodySignedIn,
            at: Some(read),
        };
        assert_eq!(
            next_line(unread, nobody).map(|(line, _)| line.level()),
            Some(Level::INFO)
        );
    }

    /// #3787, #3830 — a look that never answers (a hung logind): the call on
    /// the input arbiter's thread returns at once, and the session's one line
    /// comes when the wait ends, at today's level, the brief's, saying the
    /// seat was not known in time.
    #[test]
    fn a_look_that_never_answers_holds_up_nothing_and_the_line_comes_at_the_bound() {
        let wait = Duration::from_millis(400);
        let lines = lines(wait);
        let held = Held::default();
        let _let_go = LetGoAtTheEnd(held.clone());
        let (tx, rx) = mpsc::channel();
        let brief = Seen {
            kind: NoDisplay::ShouldExist,
            at: Some(Instant::now()),
        };
        let asked = Instant::now();
        lines.say(
            brief,
            held.look(Some(NoDisplay::NobodySignedIn)),
            || {},
            sink(&tx),
        );
        let returned = asked.elapsed();
        assert!(
            returned < Duration::from_millis(50),
            "the arbiter's call took {returned:?}"
        );
        let (at, line) = rx.recv_timeout(LONG).expect("the line");
        let said_after = at.duration_since(asked);
        assert!(
            said_after >= wait,
            "said after {said_after:?}, inside the wait"
        );
        assert_eq!(
            line,
            Line::Said {
                kind: NoDisplay::ShouldExist,
                chosen: Chosen::NoAnswer(Unanswered::NotInTime)
            }
        );
        assert_eq!(line.level(), Level::ERROR);
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "one line"
        );
    }

    /// #3787 — every try while the first line waits for its look is a DEBUG
    /// repeat: the session says one line however often it tries (a regrant,
    /// keep busy's ops), and looks once.
    #[test]
    fn a_try_while_the_look_runs_is_a_repeat_and_looks_no_more() {
        let lines = lines(LONG);
        let held = Held::default();
        let _let_go = LetGoAtTheEnd(held.clone());
        let (tx, rx) = mpsc::channel();
        let brief = Seen {
            kind: NoDisplay::ShouldExist,
            at: None,
        };
        lines.say(
            brief,
            held.look(Some(NoDisplay::NobodySignedIn)),
            || {},
            sink(&tx),
        );
        for _ in 0..3 {
            lines.say(
                brief,
                || unreachable!("one look a session"),
                || {},
                sink(&tx),
            );
            let line = rx.recv_timeout(LONG).map(|(_, line)| line);
            assert_eq!(line, Ok(Line::Waiting));
        }
        held.let_go();
        let line = rx.recv_timeout(LONG).map(|(_, line)| line);
        assert_eq!(
            line,
            Ok(Line::Said {
                kind: NoDisplay::NobodySignedIn,
                chosen: Chosen::Look
            })
        );
    }

    /// #3787 — a look at nobody, then later tries in the same session against
    /// the stale brief that still shows the person who signed out: no ERROR
    /// and no change of kind, only repeats.
    #[test]
    fn after_a_look_at_nobody_a_stale_brief_brings_no_error_and_no_change() {
        let lines = lines(LONG);
        let (tx, rx) = mpsc::channel();
        let stale = Seen {
            kind: NoDisplay::ShouldExist,
            at: Some(Instant::now()),
        };
        lines.say(stale, || Some(NoDisplay::NobodySignedIn), || {}, sink(&tx));
        let line = rx.recv_timeout(LONG).map(|(_, line)| line);
        assert_eq!(
            line,
            Ok(Line::Said {
                kind: NoDisplay::NobodySignedIn,
                chosen: Chosen::Look
            })
        );
        // A regrant, a keep-busy op, a first event.
        for _ in 0..3 {
            lines.say(
                stale,
                || unreachable!("one look a session"),
                || unreachable!("the brief is asked again only by a look"),
                sink(&tx),
            );
            let line = rx.recv_timeout(LONG).map(|(_, line)| line);
            assert_eq!(line, Ok(Line::Repeat(NoDisplay::NobodySignedIn)));
        }
    }

    /// #3787 — a look that sees another kind than the brief asks the brief to
    /// read again, from the line's own thread, never from the caller's (the
    /// input arbiter's). A new session looks again, and a look that agrees
    /// with the brief asks nothing.
    #[test]
    fn a_look_that_disagrees_asks_the_brief_again_from_the_lines_thread() {
        let lines = lines(LONG);
        let (tx, rx) = mpsc::channel();
        let (asked_tx, asked_rx) = mpsc::channel();
        let stale = Seen {
            kind: NoDisplay::ShouldExist,
            at: Some(Instant::now()),
        };
        let asking = asked_tx.clone();
        lines.say(
            stale,
            || Some(NoDisplay::NobodySignedIn),
            move || {
                let _ = asking.send(std::thread::current().name().map(str::to_string));
            },
            sink(&tx),
        );
        let line = rx.recv_timeout(LONG).map(|(_, line)| line);
        assert_eq!(line.map(|line| line.level()), Ok(Level::INFO));
        assert_eq!(asked_rx.try_recv(), Ok(Some(LINE_THREAD.to_string())));

        lines.forget();
        let asking = asked_tx.clone();
        lines.say(
            stale,
            || Some(NoDisplay::ShouldExist),
            move || {
                let _ = asking.send(None);
            },
            sink(&tx),
        );
        let line = rx.recv_timeout(LONG).map(|(_, line)| line);
        assert_eq!(
            line,
            Ok(Line::Said {
                kind: NoDisplay::ShouldExist,
                chosen: Chosen::Look
            }),
            "a new session looks again"
        );
        assert_eq!(asked_rx.try_recv(), Err(mpsc::TryRecvError::Empty));
    }
}
