// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-92 P3 — the logind session behind the X display keep busy drives.
//!
//! Two questions, both logind's to answer: who is signed in at this display
//! (whose keep-busy store applies), and whether that session is locked
//! (`LockedHint`, which GNOME, KDE and light-locker set).
//!
//! Read with `loginctl`, key=value only — the same choice, for the same
//! reason, as `companion::graphical_session_matching`: the column layout of
//! `list-sessions` differs between systemd releases, and only its first
//! column (the id) is stable.
//!
//! ⚠️ A locker that never sets `LockedHint` (xscreensaver, i3lock) is not
//! seen. Keep busy then keeps moving the pointer behind it — harmless, since
//! those lockers have no hot corners and a move unlocks nothing — but the
//! display stays awake while it is on.

use std::process::Command;

/// The properties [`show`] asks for — and nothing else, so a systemd that
/// adds one changes nothing here.
const PROPS: [&str; 7] = [
    "Id",
    "User",
    "Class",
    "Active",
    "LockedHint",
    "Display",
    "Type",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub uid: u32,
    /// `user`, `greeter`, `lock-screen`, …
    pub class: String,
    pub active: bool,
    pub locked: bool,
    /// The X display (`:0`) — empty for a Wayland or tty session.
    pub display: Option<String>,
    /// `x11`, `wayland`, `tty`, …
    pub kind: String,
}

impl Session {
    /// A person's session — never the display manager's greeter.
    pub fn is_user(&self) -> bool {
        self.class == "user"
    }
}

/// One `loginctl show-session` block. `None` without an id or a numeric
/// user: a session this cannot attribute to anyone is not one to act for.
pub fn parse_show(text: &str) -> Option<Session> {
    let field = |k: &str| -> Option<&str> {
        text.lines()
            .find_map(|l| l.trim_end().strip_prefix(k)?.strip_prefix('='))
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    Some(Session {
        id: field("Id")?.to_string(),
        uid: field("User")?.parse().ok()?,
        class: field("Class").unwrap_or_default().to_string(),
        active: field("Active") == Some("yes"),
        locked: field("LockedHint") == Some("yes"),
        display: field("Display").map(str::to_string),
        kind: field("Type").unwrap_or_default().to_string(),
    })
}

/// `":0.0"` and `":0"` name the same display; logind reports the latter.
pub fn same_display(a: &str, b: &str) -> bool {
    fn base(d: &str) -> &str {
        match d.rfind(':') {
            Some(colon) => match d[colon..].find('.') {
                Some(dot) => &d[..colon + dot],
                None => d,
            },
            None => d,
        }
    }
    base(a.trim()) == base(b.trim())
}

/// Of `sessions`, the one that owns `display`; else, with no display to go
/// by, the active graphical one. A greeter is returned like any other — the
/// caller decides that a greeter means "nobody signed in".
pub fn pick<'a>(sessions: &'a [Session], display: Option<&str>) -> Option<&'a Session> {
    if let Some(d) = display {
        let on_display: Vec<&Session> = sessions
            .iter()
            .filter(|s| s.display.as_deref().is_some_and(|sd| same_display(sd, d)))
            .collect();
        // Several sessions can name one display over time; the live one wins.
        return on_display
            .iter()
            .find(|s| s.active)
            .or(on_display.first())
            .copied();
    }
    sessions
        .iter()
        .find(|s| s.active && (s.kind == "x11" || s.kind == "wayland"))
}

/// One session's properties, or `None` when it is gone (or `loginctl` is).
pub fn show(id: &str) -> Option<Session> {
    let mut args = vec!["show-session", id, "--no-pager"];
    for p in PROPS {
        args.push("-p");
        args.push(p);
    }
    let out = Command::new("loginctl").args(&args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_show(&String::from_utf8_lossy(&out.stdout))
}

/// Every session logind knows. `None` when there is no `loginctl` at all
/// (no systemd): the caller then has no session facts, which is not the same
/// as "nobody is signed in".
pub fn sessions() -> Option<Vec<Session>> {
    let out = Command::new("loginctl")
        .args(["list-sessions", "--no-legend", "--no-pager"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(
        text.lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(show)
            .collect(),
    )
}

/// The session on `display` (the process's `DISPLAY` when `None`).
pub fn session_for_display(display: Option<&str>) -> Option<Session> {
    let env = std::env::var("DISPLAY").ok();
    let display = display.or(env.as_deref());
    pick(&sessions()?, display).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GNOME_X11: &str =
        "Id=2\nUser=1000\nClass=user\nActive=yes\nLockedHint=no\nDisplay=:0\nType=x11\n";

    #[test]
    fn a_show_block_parses_and_a_nameless_one_does_not() {
        let s = parse_show(GNOME_X11).unwrap();
        assert_eq!(
            s,
            Session {
                id: "2".into(),
                uid: 1000,
                class: "user".into(),
                active: true,
                locked: false,
                display: Some(":0".into()),
                kind: "x11".into(),
            }
        );
        assert!(s.is_user());
        let locked = parse_show(&GNOME_X11.replace("LockedHint=no", "LockedHint=yes")).unwrap();
        assert!(locked.locked);
        // An empty Display is no display at all.
        let wl = parse_show(&GNOME_X11.replace("Display=:0", "Display=")).unwrap();
        assert_eq!(wl.display, None);
        assert_eq!(parse_show("User=1000\nClass=user\n"), None, "no id");
        assert_eq!(parse_show("Id=c1\nUser=gdm\n"), None, "no numeric user");
    }

    /// `LockedHint=yesterday` is not `yes`, and a key that merely starts
    /// with another (`IdleHint` vs `Id`) is not it.
    #[test]
    fn keys_and_values_match_exactly() {
        let s = parse_show("IdleHint=yes\nId=7\nUser=1001\nLockedHint=yesterday\n").unwrap();
        assert_eq!(s.id, "7");
        assert!(!s.locked);
    }

    #[test]
    fn a_display_and_its_screen_are_the_same_display() {
        assert!(same_display(":0", ":0.0"));
        assert!(same_display(":1.0", ":1"));
        assert!(!same_display(":0", ":1"));
        assert!(!same_display(":10", ":1"), "no prefix match");
        assert!(same_display("localhost:10.0", "localhost:10"));
    }

    #[test]
    fn the_session_on_our_display_wins_and_a_greeter_is_still_reported() {
        let user = parse_show(GNOME_X11).unwrap();
        let greeter = Session {
            id: "c1".into(),
            uid: 120,
            class: "greeter".into(),
            active: true,
            locked: false,
            display: Some(":1".into()),
            kind: "x11".into(),
        };
        let all = vec![greeter.clone(), user.clone()];
        assert_eq!(pick(&all, Some(":0.0")), Some(&user));
        assert_eq!(pick(&all, Some(":1")), Some(&greeter));
        assert_eq!(pick(&all, Some(":9")), None, "no session owns :9");
        assert_eq!(pick(&all, None), Some(&greeter), "first active graphical");
    }

    /// One display, two sessions over time (a logout and a login): the live
    /// one is the answer.
    #[test]
    fn of_two_sessions_on_one_display_the_active_one_wins() {
        let gone = Session {
            active: false,
            id: "3".into(),
            ..parse_show(GNOME_X11).unwrap()
        };
        let live = Session {
            uid: 1001,
            id: "4".into(),
            ..parse_show(GNOME_X11).unwrap()
        };
        let all = vec![gone, live.clone()];
        assert_eq!(pick(&all, Some(":0")), Some(&live));
    }
}
