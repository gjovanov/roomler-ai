// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-93 P2b — the hello's `desktop`: the desktop a controller would see on
//! this Linux device now, and what would capture it.
//!
//! The server counts these (how many hosts are Wayland, and which arm serves
//! each), which an exec sweep cannot: a sweep reaches only what is online.
//! logind is read with `loginctl`, which must never stall the signalling loop,
//! so a background thread refreshes a cache every [`REFRESH`] and the hello and
//! the heartbeat only ever read it. The heartbeat re-announces the caps when
//! the brief changes (a login, a logout, a greeter).
//!
//! `None` until the first refresh lands, and always off Linux.

use std::sync::Mutex;
use std::time::Instant;

use roomler_ai_remote_control::models::DesktopBrief;

/// How often the cache is refreshed.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const REFRESH: std::time::Duration = std::time::Duration::from_secs(60);

/// The latest brief, and when the read of logind behind it began.
static LATEST: Mutex<Option<(Instant, DesktopBrief)>> = Mutex::new(None);

/// Set by [`refresh_soon`]; the refresher waits on it between two reads.
#[cfg(target_os = "linux")]
static SOON: (Mutex<bool>, std::sync::Condvar) = (Mutex::new(false), std::sync::Condvar::new());

/// The latest brief. Never blocks on logind.
pub fn current() -> Option<DesktopBrief> {
    LATEST
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|(_, brief)| brief.clone())
}

/// #3787 — [`current`], and when the read of logind behind it began: a read
/// of the seat that began later is the newer one. Never blocks on logind.
#[cfg(target_os = "linux")]
pub(crate) fn current_read() -> Option<(Instant, DesktopBrief)> {
    LATEST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// #3643 — read the brief again now, not at the next [`REFRESH`]: the daemon's
/// own virtual desktop went down or came back, and the device should stop or
/// start offering its X11 arm within a heartbeat, not within a minute. Never
/// blocks, and a call made while a read is running is not lost: the refresher
/// reads once more as soon as that one ends.
#[cfg(target_os = "linux")]
pub fn refresh_soon() {
    *SOON.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    SOON.1.notify_all();
}

/// Sleep until the next refresh is due: [`REFRESH`] from now, or at once when
/// [`refresh_soon`] has asked.
#[cfg(target_os = "linux")]
fn wait_for_the_next_refresh(every: std::time::Duration) {
    let due = std::time::Instant::now() + every;
    let mut soon = SOON.0.lock().unwrap_or_else(|e| e.into_inner());
    while !*soon {
        let left = due.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        soon = SOON
            .1
            .wait_timeout(soon, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    *soon = false;
}

/// FR-93 P3d — whether the desktop a controller would see is a Wayland one:
/// the cached brief's session (at most [`REFRESH`] old), else logind asked
/// now. A greeter counts, a tty or nobody signed in does not.
#[cfg(target_os = "linux")]
pub fn seen_is_wayland() -> bool {
    if let Some(brief) = current() {
        return brief.kind == "wayland";
    }
    // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
    let uid = unsafe { libc::geteuid() };
    crate::keep_busy::logind::sessions()
        .as_deref()
        .and_then(|all| pick::seen_session(all, uid == 0, uid))
        .is_some_and(|s| s.kind == "wayland")
}

/// FR-113 P1 — whether the desktop a controller would see is an X11 one, read
/// as [`seen_is_wayland`] reads it. A greeter counts here too: the seat guard,
/// not this, keeps an automatic capture off the login screen.
#[cfg(target_os = "linux")]
pub fn seen_is_x11() -> bool {
    if let Some(brief) = current() {
        return brief.kind == "x11";
    }
    // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
    let uid = unsafe { libc::geteuid() };
    crate::keep_busy::logind::sessions()
        .as_deref()
        .and_then(|all| pick::seen_session(all, uid == 0, uid))
        .is_some_and(|s| s.kind == "x11")
}

/// #3787 — who is at the screen now, as the brief would carry it: logind's
/// session class and type of the session a controller would see, `none` for
/// both when there is none. Read from logind at once, not up to [`REFRESH`]
/// ago, and nothing else of the brief (its input reach can take an X probe).
/// `None` when logind cannot tell. ⚠️ Blocks on `loginctl`, which can hang:
/// only a thread of its own may call it, never an async worker or the input
/// arbiter (`input::no_display`).
#[cfg(target_os = "linux")]
pub(crate) fn seen_now() -> Option<(String, String)> {
    // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
    let uid = unsafe { libc::geteuid() };
    let all = crate::keep_busy::logind::sessions()?;
    Some(pick::session_and_kind(pick::seen_session(
        &all,
        uid == 0,
        uid,
    )))
}

/// Start the refresher (once per process; a second call does nothing).
pub fn start() {
    #[cfg(target_os = "linux")]
    {
        static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        STARTED.get_or_init(|| {
            let spawned = std::thread::Builder::new()
                .name("desktop-brief".into())
                .spawn(|| {
                    loop {
                        let prev = current();
                        // #3787 — when this read of logind began: a look at
                        // the seat that began later is newer than this brief.
                        let began = Instant::now();
                        let brief = linux::gather(prev.as_ref());
                        let mut latest = LATEST.lock().unwrap_or_else(|e| e.into_inner());
                        // Said once per change, so the journal shows what the
                        // hello and the heartbeat carry.
                        if latest.as_ref().map(|(_, last)| last) != Some(&brief) {
                            tracing::info!(
                                session = %brief.session,
                                kind = %brief.kind,
                                desktop = brief.desktop.as_deref().unwrap_or("-"),
                                capture_arm = %brief.capture_arm,
                                input_reach = %brief.input_reach,
                                "desktop brief (FR-93 P2b)"
                            );
                        }
                        *latest = Some((began, brief));
                        drop(latest);
                        wait_for_the_next_refresh(REFRESH);
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(%e, "desktop brief: no refresher thread; the hello carries none");
            }
        });
    }
}

#[cfg(target_os = "linux")]
mod pick {
    use crate::keep_busy::logind::Session;

    /// A desktop session, which a greeter is too. A tty or an SSH login is not
    /// one a controller would see.
    fn graphical(s: &Session) -> bool {
        s.active && (s.kind == "wayland" || s.kind == "x11")
    }

    /// The session a controller would see. A root daemon sees the seat: a
    /// person's session first, else the greeter or the lock screen. A per-user
    /// daemon sees only its own user's sessions.
    pub(super) fn seen_session(all: &[Session], root: bool, uid: u32) -> Option<&Session> {
        if root {
            all.iter()
                .filter(|s| graphical(s))
                .find(|s| s.is_user())
                .or_else(|| all.iter().find(|s| graphical(s)))
        } else {
            all.iter().find(|s| s.uid == uid && graphical(s))
        }
    }

    /// The brief's `session` (logind's class) and `kind` (its type) for the
    /// session a controller would see: `none` for both with no such session.
    /// #3787 — the brief and `seen_now` both map it here, so the two agree.
    pub(super) fn session_and_kind(seen: Option<&Session>) -> (String, String) {
        (
            seen.map_or("none", |s| s.class.as_str()).to_string(),
            seen.map_or("none", |s| s.kind.as_str()).to_string(),
        )
    }
}

/// Is `next` the same desktop as `prev`, all but the input reach?
///
/// The reach is the one field that costs something to read: it talks to the X
/// server (and, on a Wayland desktop, only to an Xwayland already running:
/// GNOME starts one on demand for any client that connects, see
/// `reach::predicted`). So it is read again only when the rest of the brief
/// changed (a login, a logout, the greeter, a different arm), which also
/// spares the server a re-announce every time an on-demand Xwayland comes or
/// goes. A reach that changes on its own is seen at the next of those. The
/// per-session `rc:input-reach` is read fresh and stays the authority.
#[cfg(target_os = "linux")]
fn same_desktop(prev: &DesktopBrief, next: &DesktopBrief) -> bool {
    prev.session == next.session
        && prev.kind == next.kind
        && prev.desktop == next.desktop
        && prev.capture_arm == next.capture_arm
}

#[cfg(target_os = "linux")]
mod linux {
    use super::pick::{seen_session, session_and_kind};
    use crate::capture::{CaptureArm, arm_facts, planned_arm, planned_input_route};
    use roomler_ai_remote_control::models::DesktopBrief;

    /// The brief now. `prev` (the last one) lends its input reach when nothing
    /// else changed: see [`super::same_desktop`].
    pub(super) fn gather(prev: Option<&DesktopBrief>) -> DesktopBrief {
        // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
        let uid = unsafe { libc::geteuid() };
        let root = uid == 0;
        let sessions = crate::keep_busy::logind::sessions();
        let seen = sessions
            .as_deref()
            .and_then(|all| seen_session(all, root, uid));
        let (session, kind) = session_and_kind(seen);
        let facts = arm_facts(sessions.as_deref(), root, uid, kind == "x11");
        let arm = planned_arm(&facts);
        // FR-93 P4 — the RemoteDesktop route that arm would open, if any.
        let route = planned_input_route(&facts);
        let mut next = DesktopBrief {
            session,
            kind,
            desktop: seen.and_then(|s| s.desktop.clone()),
            capture_arm: arm.map_or("none", CaptureArm::wire).to_string(),
            input_reach: String::new(),
        };
        next.input_reach = match prev {
            Some(p) if super::same_desktop(p, &next) => p.input_reach.clone(),
            _ => crate::input::reach::predicted(route, next.kind == "wayland", facts.auto_system)
                .reach
                .wire()
                .to_string(),
        };
        next
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::pick::seen_session;
    use super::same_desktop;
    use crate::keep_busy::logind::Session;
    use roomler_ai_remote_control::models::DesktopBrief;

    fn brief(session: &str, arm: &str, reach: &str) -> DesktopBrief {
        DesktopBrief {
            session: session.into(),
            kind: "wayland".into(),
            desktop: Some("gnome".into()),
            capture_arm: arm.into(),
            input_reach: reach.into(),
        }
    }

    /// The X probe behind the reach runs only when the desktop changed: an
    /// unchanged desktop keeps its reach, whatever the new read would say.
    #[test]
    fn the_reach_is_reread_only_when_the_desktop_changed() {
        let prev = brief("user", "mutter", "xwayland_only");
        assert!(same_desktop(&prev, &brief("user", "mutter", "")));
        assert!(
            !same_desktop(&prev, &brief("greeter", "mutter", "")),
            "a logout"
        );
        assert!(
            !same_desktop(&prev, &brief("user", "none", "")),
            "another arm"
        );
        let other_desktop = DesktopBrief {
            desktop: Some("KDE".into()),
            ..brief("user", "mutter", "")
        };
        assert!(!same_desktop(&prev, &other_desktop));
    }

    fn s(id: &str, uid: u32, class: &str, active: bool, kind: &str) -> Session {
        Session {
            id: id.into(),
            uid,
            class: class.into(),
            active,
            locked: false,
            display: None,
            kind: kind.into(),
            desktop: None,
            state: String::new(),
        }
    }

    #[test]
    fn a_root_daemon_sees_the_person_before_the_greeter() {
        let all = vec![
            s("c1", 120, "greeter", true, "wayland"),
            s("2", 1000, "user", true, "wayland"),
        ];
        assert_eq!(
            seen_session(&all, true, 0).map(|x| x.id.as_str()),
            Some("2")
        );
        // Nobody signed in: the greeter is what a controller would see.
        let greeter_only = vec![s("c1", 120, "greeter", true, "wayland")];
        assert_eq!(
            seen_session(&greeter_only, true, 0).map(|x| x.class.as_str()),
            Some("greeter")
        );
    }

    /// #3787 — the brief and the look at the seat (`seen_now`) map the seen
    /// session alike: nobody at a desktop is `none` for both, and a greeter is
    /// its class and type, which `input::no_display` reads as nobody signed in.
    #[test]
    fn the_brief_and_the_look_map_the_seen_session_alike() {
        use super::pick::session_and_kind;
        let none = ("none".to_string(), "none".to_string());
        assert_eq!(session_and_kind(None), none);
        let greeter = s("c1", 120, "greeter", true, "wayland");
        assert_eq!(
            session_and_kind(Some(&greeter)),
            ("greeter".to_string(), "wayland".to_string())
        );
        // A text console is no desktop, so it is seen as nobody, as in the brief.
        let all = vec![s("5", 1000, "user", true, "tty")];
        assert_eq!(session_and_kind(seen_session(&all, true, 0)), none);
    }

    #[test]
    fn a_tty_or_ssh_login_or_an_idle_session_is_no_desktop() {
        let all = vec![
            s("5", 1000, "user", true, "tty"),
            s("6", 1000, "user", false, "wayland"),
        ];
        assert!(seen_session(&all, true, 0).is_none());
        assert!(seen_session(&all, false, 1000).is_none());
    }

    #[test]
    fn a_per_user_daemon_sees_only_its_own_user() {
        let all = vec![
            s("2", 1000, "user", true, "wayland"),
            s("3", 1001, "user", true, "x11"),
        ];
        assert_eq!(
            seen_session(&all, false, 1001).map(|x| x.kind.as_str()),
            Some("x11")
        );
        assert!(seen_session(&all, false, 1002).is_none());
    }
}
