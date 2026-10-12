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

use roomler_ai_remote_control::models::DesktopBrief;

/// How often the cache is refreshed.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const REFRESH: std::time::Duration = std::time::Duration::from_secs(60);

static LATEST: Mutex<Option<DesktopBrief>> = Mutex::new(None);

/// The latest brief. Never blocks on logind.
pub fn current() -> Option<DesktopBrief> {
    LATEST.lock().unwrap_or_else(|e| e.into_inner()).clone()
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
                        let brief = linux::gather();
                        *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(brief);
                        std::thread::sleep(REFRESH);
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
}

#[cfg(target_os = "linux")]
mod linux {
    use super::pick::seen_session;
    use crate::capture::{CaptureArm, arm_facts, planned_arm};
    use roomler_ai_remote_control::models::DesktopBrief;

    pub(super) fn gather() -> DesktopBrief {
        // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
        let uid = unsafe { libc::geteuid() };
        let root = uid == 0;
        let sessions = crate::keep_busy::logind::sessions();
        let seen = sessions
            .as_deref()
            .and_then(|all| seen_session(all, root, uid));
        let kind = seen.map_or("none", |s| s.kind.as_str()).to_string();
        let arm = planned_arm(&arm_facts(sessions.as_deref(), root, uid, kind == "x11"));
        let portal_input = matches!(arm, Some(CaptureArm::Portal | CaptureArm::Mutter))
            && tunnel_core::env::flag("PORTAL_INPUT", false);
        DesktopBrief {
            session: seen.map_or("none", |s| s.class.as_str()).to_string(),
            kind,
            desktop: seen.and_then(|s| s.desktop.clone()),
            capture_arm: arm.map_or("none", CaptureArm::wire).to_string(),
            input_reach: crate::input::reach::predicted(portal_input)
                .reach
                .wire()
                .to_string(),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::pick::seen_session;
    use crate::keep_busy::logind::Session;

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
