// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3787 — how loudly the X11 injector says it has no display to open.
//!
//! On Linux, enigo's only backend in this build is x11rb. With no `DISPLAY`,
//! x11rb answers `DisplayNotSet` before any I/O, and enigo then logs that
//! error and "no successful connection" at ERROR. A system install with nobody
//! signed in at the screen has no display by design, and its session already
//! tells the viewer why, so every input open there printed an ERROR pair that
//! was noise.
//!
//! Pure, so every test lane compiles it, whatever the features.
#![cfg_attr(
    not(all(target_os = "linux", feature = "enigo-input")),
    allow(dead_code)
)]

/// Why there is no X display for this process to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoDisplay {
    /// Nobody is signed in at a graphical session on this host's screen: a
    /// login screen, a tty, or nobody at a seat. No display is expected.
    NobodySignedIn,
    /// A person is signed in at a graphical session, or the seat is not known
    /// yet. A display should exist, so its absence is a fault.
    ShouldExist,
}

impl NoDisplay {
    /// Which kind, from whether this process has a `DISPLAY` and from the
    /// desktop brief's session class and type (`None` before the brief is
    /// first read). `None` when `DISPLAY` is set: then the open runs.
    pub fn of(display_set: bool, seat: Option<(&str, &str)>) -> Option<NoDisplay> {
        if display_set {
            return None;
        }
        Some(match seat {
            Some((session, kind)) if !signed_in_graphically(session, kind) => {
                NoDisplay::NobodySignedIn
            }
            _ => NoDisplay::ShouldExist,
        })
    }

    /// The level the missing display is said at.
    pub fn level(self) -> tracing::Level {
        tracing::Level::ERROR
    }
}

/// A graphical session (`x11`, `wayland`) with a person in it: any logind
/// class but the login screen's (`greeter`) and `none`. Equality only.
fn signed_in_graphically(session: &str, kind: &str) -> bool {
    matches!(kind, "x11" | "wayland") && !matches!(session, "greeter" | "none")
}

/// The level to say `kind` at, given the kind last said in this session.
pub fn level_after(kind: NoDisplay, _last: Option<NoDisplay>) -> tracing::Level {
    kind.level()
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
}
