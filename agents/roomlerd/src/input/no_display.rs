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
//! Pure but for [`say`]'s one-slot memory, so every test lane compiles it,
//! whatever the features.
#![cfg_attr(
    not(all(target_os = "linux", feature = "enigo-input")),
    allow(dead_code)
)]

use std::sync::Mutex;

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

    /// The level the missing display is said at: INFO where none is
    /// expected, ERROR where one should exist.
    pub fn level(self) -> tracing::Level {
        match self {
            NoDisplay::NobodySignedIn => tracing::Level::INFO,
            NoDisplay::ShouldExist => tracing::Level::ERROR,
        }
    }
}

/// A graphical session (`x11`, `wayland`) with a person in it: any logind
/// class but the login screen's (`greeter`) and `none`. Equality only.
fn signed_in_graphically(session: &str, kind: &str) -> bool {
    matches!(kind, "x11" | "wayland") && !matches!(session, "greeter" | "none")
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
/// succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoDisplayToOpen(pub NoDisplay);

impl std::fmt::Display for NoDisplayToOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            NoDisplay::NobodySignedIn => {
                "no X display to open: this process has no DISPLAY, and nobody is signed in at a \
                 graphical session on this screen"
            }
            NoDisplay::ShouldExist => {
                "no X display to open: this process has no DISPLAY, though a graphical session is \
                 signed in at this screen or the seat is not known yet"
            }
        })
    }
}

impl std::error::Error for NoDisplayToOpen {}

/// The kind an open failed for, read from the error's type, never its text.
/// `None` for any other failure, which keeps its own line.
pub fn kind_of(e: &anyhow::Error) -> Option<NoDisplay> {
    e.downcast_ref::<NoDisplayToOpen>().map(|n| n.0)
}

/// The kind last said in this session.
static LAST_SAID: Mutex<Option<NoDisplay>> = Mutex::new(None);

/// Say `kind` at its level, or at DEBUG when this session already said it.
pub fn say(kind: NoDisplay) {
    let level = {
        let mut last = LAST_SAID.lock().unwrap_or_else(|e| e.into_inner());
        let level = level_after(kind, *last);
        *last = Some(kind);
        level
    };
    let why = NoDisplayToOpen(kind);
    if level == tracing::Level::ERROR {
        tracing::error!("input: {why} — input injection disabled (#3787)");
    } else if level == tracing::Level::INFO {
        tracing::info!("input: {why} — no input until there is one (#3787)");
    } else {
        tracing::debug!("input: still {why}");
    }
}

/// A session opened, or an injector did: the next missing display is said at
/// its own level again.
pub fn forget() {
    *LAST_SAID.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
    /// ERROR, as on master, where enigo's own lines said it.
    #[test]
    fn a_display_that_should_exist_still_fails_at_error() {
        for seat in [
            Some(("user", "x11")),
            Some(("user", "wayland")),
            Some(("lock-screen", "x11")),
            Some(("user-incomplete", "wayland")),
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
    /// even one worded the same way, keeps master's line.
    #[test]
    fn the_kind_is_read_from_the_error_type_never_its_text() {
        let typed = anyhow::Error::new(NoDisplayToOpen(NoDisplay::NobodySignedIn));
        assert_eq!(kind_of(&typed), Some(NoDisplay::NobodySignedIn));
        let worded = anyhow::anyhow!("enigo init: {}", NoDisplayToOpen(NoDisplay::NobodySignedIn));
        assert_eq!(kind_of(&worded), None);
        let enigo = anyhow::anyhow!(
            "enigo init: no connection could be established: (no successful connection)"
        );
        assert_eq!(kind_of(&enigo), None);
    }
}
