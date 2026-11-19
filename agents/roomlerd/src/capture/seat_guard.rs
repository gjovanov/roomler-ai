// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-95 P2 — may a capture that a system install switched on by itself read
//! the screen now?
//!
//! DRM reads the scanout below the compositor, so it sees whatever the seat
//! shows: the greeter's password field, the lock screen's unlock dialog
//! (FR-36 measured both). An operator who set `drm_capture` chose that. A
//! system install that captures by ITSELF must read neither, and must never
//! read a person who was not told they are being viewed. So such a capture is
//! **bound** to the person's session that was on screen when it opened, the
//! one told by FR-93 P6g's notice, and reads the scanout only while that
//! session is on screen, unlocked.
//!
//! The seat is watched over logind's D-Bus: a `PropertiesChanged` from logind
//! (a lock, a user switch, the greeter) is acted on as it arrives, and the seat
//! is read again every [`TICK`] all the same, so a missed signal costs a
//! second, not the guard.
//!
//! ⚠️ Fail CLOSED. A reading older than [`STALE`] (the watcher died, logind
//! went away) is `Away`, and so is a seat logind cannot vouch for.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::WaitFor;

/// How often the seat is read again, signal or not.
const TICK: Duration = Duration::from_secs(1);

/// A reading this old is no reading: the guard closes.
const STALE: Duration = Duration::from_secs(5);

/// What the guard says about one bound session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Guard {
    /// The bound session is on screen, unlocked: read it.
    Open,
    /// The bound session is on screen, locked: its unlock dialog is.
    Locked,
    /// The bound session is not on screen: the greeter, another person's
    /// session, a text console, or nothing logind vouches for.
    Away,
}

impl Guard {
    /// What the controller is told while the guard is closed: the per-user
    /// arms' `screen_unlock` and `session_away`, in a system install's words.
    pub(crate) fn wait(self) -> Option<WaitFor> {
        match self {
            Guard::Open => None,
            Guard::Locked => Some(WaitFor::GuardLocked),
            Guard::Away => Some(WaitFor::GuardAway),
        }
    }
}

/// The seat's active session, as far as the guard cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OnScreen {
    pub id: String,
    /// logind's `Class`: `user` for a person, `greeter`, `lock-screen`, …
    pub class: String,
    /// `LockedHint`, which GNOME and KDE set while their lock screen is up.
    pub locked: bool,
    /// logind's `State` is `closing`: the person logged out, and the session
    /// is still the seat's active one only while its processes end.
    pub closing: bool,
}

/// The guard as a pure function. `on_screen` is the seat's active session,
/// `None` when there is none, or none logind vouches for.
///
/// ⚠️ Matched on the session id, never the user: a second login of the same
/// person is a different session, told nothing.
///
/// ⚠️ A bound session that is `closing` is away. The person logged out, and
/// logind keeps the session as the seat's active one until every process in
/// it has ended, which can take until systemd's stop timeout. Measured on
/// GDM's Xorg autologin (FR-113 AC3, 2026-10-19): its session worker ignored
/// SIGTERM, so the seat showed no greeter for 90 s after the logout, while
/// the X server was already gone. Read as open, that scanout reached the
/// viewer as a black screen with nothing said.
pub(crate) fn guard(bound: &str, on_screen: Option<&OnScreen>) -> Guard {
    match on_screen {
        Some(s) if s.id == bound && s.class == "user" && !s.closing && s.locked => Guard::Locked,
        Some(s) if s.id == bound && s.class == "user" && !s.closing => Guard::Open,
        _ => Guard::Away,
    }
}

struct Seen {
    on_screen: Option<OnScreen>,
    at: Instant,
}

static SEEN: Mutex<Option<Seen>> = Mutex::new(None);

fn store(on_screen: Option<OnScreen>) {
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.as_ref().map(|s| &s.on_screen) != Some(&on_screen) {
        tracing::info!(
            session = on_screen.as_ref().map_or("-", |s| s.id.as_str()),
            class = on_screen.as_ref().map_or("-", |s| s.class.as_str()),
            locked = on_screen.as_ref().is_some_and(|s| s.locked),
            closing = on_screen.as_ref().is_some_and(|s| s.closing),
            "seat guard: on screen now (FR-95 P2)"
        );
    }
    *seen = Some(Seen {
        on_screen,
        at: Instant::now(),
    });
}

/// The seat's active session, from the watcher (started on first use, with
/// one reading taken here first so the answer is never "not yet looked").
pub(crate) fn on_screen() -> Option<OnScreen> {
    start();
    let seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    seen.as_ref()
        .filter(|s| s.at.elapsed() < STALE)
        .and_then(|s| s.on_screen.clone())
}

/// The guard for `bound`, now.
pub(crate) fn now(bound: &str) -> Guard {
    guard(bound, on_screen().as_ref())
}

/// Automatic captures alive, whatever their guard says. While one is, the
/// input a session opens is the automatic uinput, even during a lock: the
/// alternative there is enigo, whose XTest reaches Xwayland windows behind a
/// lock screen. Measured in the lab: a viewer's first move during a lock,
/// with only [`VIEWING`] to go by, opened enigo.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Automatic captures whose guard is open at this moment. Input that a system
/// install switched on by itself (uinput, FR-95 P2) is injected only while
/// one is: a controller who cannot see the screen must not type into it.
static VIEWING: AtomicUsize = AtomicUsize::new(0);

/// Whether an automatic capture is alive (its guard open or not).
pub(crate) fn live() -> bool {
    #[cfg(test)]
    if let Some(here) = LIVE_HERE.with(std::cell::Cell::get) {
        return here;
    }
    LIVE.load(Ordering::Relaxed) > 0
}

#[cfg(test)]
thread_local! {
    /// #3918 — `Some(live)` answers [`live`] on this thread: the
    /// input arbiter's tests read no other test's automatic capture.
    pub(crate) static LIVE_HERE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Whether an automatic capture is reading a person's unlocked session now.
pub(crate) fn viewing() -> bool {
    VIEWING.load(Ordering::Relaxed) > 0
}

/// One automatic capture's share of both counts: in [`LIVE`] from creation
/// to drop, in [`VIEWING`] while its guard is open, and in neither after it
/// is dropped.
#[derive(Debug)]
pub(crate) struct ViewingMark(bool);

impl Default for ViewingMark {
    fn default() -> Self {
        // #4069 — the live edge, told after the count, so the input arbiter's
        // next read of `live` sees it.
        if goes_live(LIVE.fetch_add(1, Ordering::Relaxed)) {
            tell_live(&ON_LIVE);
        }
        Self(false)
    }
}

impl ViewingMark {
    pub(crate) fn set(&mut self, open: bool) {
        if open != self.0 {
            self.0 = open;
            if open {
                VIEWING.fetch_add(1, Ordering::Relaxed);
            } else {
                VIEWING.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for ViewingMark {
    fn drop(&mut self) {
        self.set(false);
        LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// #4069 — what an automatic capture's live edge runs: the input arbiter sets
/// it as it starts, and its warm-up then opens the automatic uinput as the
/// capture goes live, ahead of the first event. The input side sets it, so
/// this module calls no input code. It runs on the capture's thread, inside
/// the open, which may have no tokio runtime, so it must never block or start
/// anything. With no arbiter in the process (a recorder, a smoke command)
/// nothing is set, and the edge tells nobody.
pub(crate) type LiveHook = Box<dyn Fn() + Send + Sync>;

static ON_LIVE: OnceLock<LiveHook> = OnceLock::new();

/// #4069 — set what the live edge runs, once for the process: the input
/// arbiter, as it starts.
pub(crate) fn on_live(hook: LiveHook) {
    let _ = ON_LIVE.set(hook);
}

/// #4069 — whether a capture that found `alive` automatic captures alive is
/// the live edge: the first one alive, and only that one. [`live`] is a bool,
/// which a second capture beside the first does not change.
fn goes_live(alive: usize) -> bool {
    alive == 0
}

/// #4069 — the live edge, told to the hook `on` holds, or to nobody.
fn tell_live(on: &OnceLock<LiveHook>) {
    if let Some(hook) = on.get() {
        hook();
    }
}

/// Start the watcher, once per process.
fn start() {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    STARTED.get_or_init(|| {
        store(read_loginctl());
        let spawned = std::thread::Builder::new()
            .name("seat-guard".into())
            .spawn(watch);
        if let Err(e) = spawned {
            // No watcher: every reading goes stale, and the guard stays shut.
            tracing::warn!(%e, "seat guard: no watcher thread; automatic capture stays closed (FR-95 P2)");
        }
    });
}

/// The seat's active session, read with `loginctl`: the first reading, and
/// the watcher's in a build without zbus.
fn read_loginctl() -> Option<OnScreen> {
    let out = std::process::Command::new("loginctl")
        .args(["show-seat", "seat0", "-p", "ActiveSession", "--value"])
        .output()
        .ok()?;
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || id.is_empty() {
        return None;
    }
    let s = crate::keep_busy::logind::show(&id)?;
    Some(OnScreen {
        id: s.id,
        class: s.class,
        locked: s.locked,
        closing: s.state == "closing",
    })
}

#[cfg(feature = "portal-capture")]
fn watch() {
    loop {
        if let Err(e) = dbus::watch_once() {
            tracing::warn!(%e, "seat guard: logind's bus unavailable; reading the seat with loginctl (FR-95 P2)");
        }
        // Until the bus is back: the same reading, the slow way.
        for _ in 0..60 {
            store(read_loginctl());
            std::thread::sleep(TICK);
        }
    }
}

#[cfg(not(feature = "portal-capture"))]
fn watch() {
    loop {
        store(read_loginctl());
        std::thread::sleep(TICK);
    }
}

#[cfg(feature = "portal-capture")]
mod dbus {
    use super::{OnScreen, TICK, store};
    use std::sync::mpsc::RecvTimeoutError;
    use zbus::blocking::Connection;
    use zbus::zvariant::{OwnedValue, Value};

    const LOGIND: &str = "org.freedesktop.login1";
    const SEAT0: &str = "/org/freedesktop/login1/seat/seat0";
    const SEAT: &str = "org.freedesktop.login1.Seat";
    const SESSION: &str = "org.freedesktop.login1.Session";
    const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

    pub(super) fn watch_once() -> anyhow::Result<()> {
        let conn = Connection::system()?;
        // zbus's blocking iterator has no timed `next`: the signals are read
        // on their own thread and handed over a channel the tick waits on.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        // By logind's object paths, not its bus name: a signal carries the
        // sender's unique name, which a local filter would have to resolve.
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .path_namespace("/org/freedesktop/login1")?
            .interface(PROPERTIES)?
            .member("PropertiesChanged")?
            .build();
        let mut signals = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, None)?;
        std::thread::Builder::new()
            .name("seat-guard-signals".into())
            .spawn(move || {
                for msg in &mut signals {
                    if msg.is_err() || tx.send(()).is_err() {
                        return;
                    }
                }
            })?;
        tracing::info!("seat guard: watching logind for locks and switches (FR-95 P2)");
        loop {
            store(read(&conn)?);
            match rx.recv_timeout(TICK) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("logind's PropertiesChanged stream ended")
                }
            }
        }
    }

    /// One `Properties.Get`, uncached: a proxy's property cache is updated by
    /// the same signal that wakes this, so it could still hold the old value.
    fn get(conn: &Connection, path: &str, iface: &str, prop: &str) -> anyhow::Result<OwnedValue> {
        let reply =
            conn.call_method(Some(LOGIND), path, Some(PROPERTIES), "Get", &(iface, prop))?;
        Ok(reply.body().deserialize::<OwnedValue>()?)
    }

    fn read(conn: &Connection) -> anyhow::Result<Option<OnScreen>> {
        // `(so)`: the session's id and its object path; `("", "/")` for none.
        let active = get(conn, SEAT0, SEAT, "ActiveSession")?;
        let (id, path) = match &*active {
            Value::Structure(s) => match s.fields() {
                [Value::Str(id), Value::ObjectPath(path)] => {
                    (id.as_str().to_string(), path.as_str().to_string())
                }
                other => anyhow::bail!("ActiveSession: unexpected fields {other:?}"),
            },
            other => anyhow::bail!("ActiveSession: not a structure: {other:?}"),
        };
        if id.is_empty() {
            return Ok(None);
        }
        let class = match &*get(conn, &path, SESSION, "Class")? {
            Value::Str(s) => s.as_str().to_string(),
            other => anyhow::bail!("Class: not a string: {other:?}"),
        };
        let locked = match &*get(conn, &path, SESSION, "LockedHint")? {
            Value::Bool(b) => *b,
            other => anyhow::bail!("LockedHint: not a boolean: {other:?}"),
        };
        // `online`, `active` or `closing`: only the last tells a logout that
        // is still the seat's active session (see `guard`).
        let closing = match &*get(conn, &path, SESSION, "State")? {
            Value::Str(s) => s.as_str() == "closing",
            other => anyhow::bail!("State: not a string: {other:?}"),
        };
        Ok(Some(OnScreen {
            id,
            class,
            locked,
            closing,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, class: &str, locked: bool) -> OnScreen {
        OnScreen {
            id: id.into(),
            class: class.into(),
            locked,
            closing: false,
        }
    }

    /// FR-95 P2 (AC4) — the screen is read only while the person who was told
    /// is on it, unlocked. Their lock screen, the greeter, someone else's
    /// session and a seat nobody vouches for are all closed.
    #[test]
    fn the_guard_opens_only_on_the_bound_person_unlocked() {
        assert_eq!(guard("3", Some(&s("3", "user", false))), Guard::Open);
        assert_eq!(guard("3", Some(&s("3", "user", true))), Guard::Locked);
        assert_eq!(guard("3", Some(&s("c1", "greeter", false))), Guard::Away);
        assert_eq!(guard("3", Some(&s("7", "user", false))), Guard::Away);
        assert_eq!(guard("3", Some(&s("3", "lock-screen", false))), Guard::Away);
        assert_eq!(guard("3", None), Guard::Away);
    }

    /// FR-113 P4 (AC3) — a logout is away from the moment logind marks the
    /// bound session `closing`, though the seat still names it as its active
    /// session, unlocked, while the session's processes end. Measured on GDM's
    /// Xorg autologin: 90 s of a bare scanout reached the viewer with nothing
    /// said.
    #[test]
    fn a_closing_session_is_away_even_while_still_on_the_seat() {
        let logging_out = OnScreen {
            closing: true,
            ..s("3", "user", false)
        };
        assert_eq!(guard("3", Some(&logging_out)), Guard::Away);
        let locked_and_logging_out = OnScreen {
            closing: true,
            ..s("3", "user", true)
        };
        assert_eq!(guard("3", Some(&locked_and_logging_out)), Guard::Away);
        // The same session before the logout: open, as ever.
        assert_eq!(guard("3", Some(&s("3", "user", false))), Guard::Open);
    }

    /// The controller is told why the picture stopped.
    #[test]
    fn a_closed_guard_names_its_wait() {
        assert_eq!(Guard::Open.wait(), None);
        assert_eq!(Guard::Locked.wait(), Some(WaitFor::GuardLocked));
        assert_eq!(Guard::Away.wait(), Some(WaitFor::GuardAway));
    }

    /// Input follows the counts: the automatic uinput is chosen while any
    /// automatic capture is alive, and injects while one is open. A dropped
    /// mark gives both shares back.
    #[test]
    fn a_viewing_mark_counts_while_alive_and_open_and_never_after_drop() {
        let (live0, view0) = (
            LIVE.load(Ordering::Relaxed),
            VIEWING.load(Ordering::Relaxed),
        );
        let mut a = ViewingMark::default();
        assert_eq!(
            LIVE.load(Ordering::Relaxed),
            live0 + 1,
            "alive from creation"
        );
        assert_eq!(
            VIEWING.load(Ordering::Relaxed),
            view0,
            "a closed guard views nothing"
        );
        a.set(true);
        a.set(true);
        assert_eq!(VIEWING.load(Ordering::Relaxed), view0 + 1);
        a.set(false);
        assert_eq!(VIEWING.load(Ordering::Relaxed), view0);
        assert_eq!(
            LIVE.load(Ordering::Relaxed),
            live0 + 1,
            "still alive while locked"
        );
        a.set(true);
        drop(a);
        assert_eq!(VIEWING.load(Ordering::Relaxed), view0);
        assert_eq!(LIVE.load(Ordering::Relaxed), live0);
    }

    /// #4069 — the live edge is the first automatic capture alive, and only
    /// that one: [`live`] reads a bool, which a second capture beside the
    /// first does not change. Asked as a pure function, because `LIVE` is the
    /// process's and other tests make marks.
    #[test]
    fn the_live_edge_is_the_first_automatic_capture_alive() {
        assert!(goes_live(0));
        assert!(!goes_live(1));
        assert!(!goes_live(4));
    }

    /// #4069 — with no input arbiter in the process nothing set the hook, and
    /// the live edge tells nobody: it returns, on a thread with no tokio
    /// runtime, as a capture's may be. Once a hook is set, each edge runs it
    /// once. And this module's code names no input code, so the edge can never
    /// be the call that starts the arbiter, whose start needs a runtime
    /// (`input::arbiter::global`).
    #[test]
    fn with_no_arbiter_the_live_edge_tells_nobody_and_needs_no_runtime() {
        let told = std::sync::Arc::new(AtomicUsize::new(0));
        let count = told.clone();
        let a_capture_thread = std::thread::spawn(move || {
            let no_runtime = tokio::runtime::Handle::try_current().is_err();
            tell_live(&OnceLock::new());
            let set = OnceLock::<LiveHook>::new();
            let _ = set.set(Box::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            }));
            tell_live(&set);
            no_runtime
        });
        assert!(
            a_capture_thread.join().expect("the live edge panicked"),
            "the thread had a runtime"
        );
        assert_eq!(told.load(Ordering::Relaxed), 1, "a set hook runs once");
        // A Windows checkout has CRLF line ends.
        let src = include_str!("seat_guard.rs").replace("\r\n", "\n");
        let code = &src[..src.find("#[cfg(test)]\nmod tests").expect("the tests")];
        for (i, line) in code.lines().enumerate() {
            let line = line.split("//").next().unwrap_or("");
            assert!(
                !line.contains("input::"),
                "seat_guard.rs:{}: input code on the capture's side",
                i + 1
            );
        }
    }
}
