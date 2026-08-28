//! Viewer-indicator overlay.
//!
//! When a remote-control session is active, the controlled host should
//! have a clear local signal that *someone is watching*. Parsec and
//! Moonlight both draw a thin colored border plus a caption listing the
//! active viewers; we copy that idea.
//!
//! The overlay is a topmost, transparent, click-through, always-on-top
//! window drawn on the agent's primary monitor. Two critical Windows
//! properties keep the overlay useful:
//!
//! - `WDA_EXCLUDEFROMCAPTURE` via `SetWindowDisplayAffinity` — DWM
//!   composites the overlay on the local screen but omits it from any
//!   capture API (DXGI desktop duplication, Windows.Graphics.Capture,
//!   BitBlt from the virtual screen). That means the overlay is visible
//!   to the person in front of the controlled PC but is invisible in
//!   the RTP video going back to the controller, so the two parties
//!   don't end up staring at a recursive red picture-frame.
//!
//! - `WS_EX_TRANSPARENT | WS_EX_NOACTIVATE` — mouse input falls through
//!   to whatever window is underneath, and activation focus isn't
//!   stolen from e.g. the game or terminal the user was working in.
//!
//! Non-Windows builds stub the whole module to a no-op so the call
//! sites in signalling don't need `#[cfg]`. A future PR can add an
//! X11 / Wayland / Cocoa implementation.

use anyhow::Result;

/// Channel the overlay's "Disconnect" control uses to ask the signaling
/// loop to tear down a session, identified by its `ObjectId`. Created
/// once in `run()` and handed to [`ViewerIndicator::new`]; the receiver
/// is polled by `connect_once`'s `select!`. `run()` retains its own
/// clone so the channel never fully closes (which would busy-spin the
/// select!) even when the overlay is disabled.
pub type KillSender = tokio::sync::mpsc::Sender<bson::oid::ObjectId>;

/// A handle to the viewer-indicator worker. Cheap to clone; multiple
/// sessions sharing one handle is the common case (one worker, many
/// concurrent sessions → one combined label).
#[derive(Clone)]
pub struct ViewerIndicator {
    inner: Inner,
}

impl ViewerIndicator {
    /// Spin up the worker. On Windows with the `viewer-indicator`
    /// feature this creates a background thread that owns a layered,
    /// click-through overlay window (a thin always-on border) plus a
    /// reveal-on-hover badge carrying the viewer's initials + a
    /// "Disconnect" control that fires through `kill_tx`. Everywhere
    /// else this is a no-op constructor — the returned handle accepts
    /// `show_session` / `hide_session` calls and drops them.
    pub fn new(kill_tx: KillSender) -> Result<Self> {
        Ok(Self {
            inner: Inner::new(kill_tx)?,
        })
    }

    /// Explicitly-disabled handle. Callers that can't bring the overlay
    /// up (init failed, headless CI, etc.) can use this so the rest of
    /// the code stays oblivious. Equivalent to `new()` on non-Windows.
    pub fn disabled() -> Self {
        Self {
            inner: Inner::disabled(),
        }
    }

    /// Announce that a session has started. The overlay redraws to
    /// include `controller_name` in its caption. Safe to call multiple
    /// times with the same `session_id` (idempotent — the name is
    /// replaced rather than appended).
    pub fn show_session(&self, session_id: String, controller_name: String) {
        self.inner.show(session_id, controller_name);
    }

    /// Announce that a session has ended. When the last session drops,
    /// the overlay is hidden.
    pub fn hide_session(&self, session_id: String) {
        self.inner.hide(session_id);
    }
}

// ---------------------------------------------------------------------------
// Platform-specific inner. The stub is used on non-Windows and when the
// `viewer-indicator` feature is disabled; the real impl is in
// `indicator::win`.

#[cfg(all(target_os = "windows", feature = "viewer-indicator"))]
mod win;

#[cfg(all(target_os = "windows", feature = "viewer-indicator"))]
use win::Inner;

#[cfg(not(all(target_os = "windows", feature = "viewer-indicator")))]
#[derive(Clone, Default)]
struct Inner;

#[cfg(not(all(target_os = "windows", feature = "viewer-indicator")))]
impl Inner {
    fn new(_kill_tx: KillSender) -> Result<Self> {
        // Drop the sender: `run()` retains its own clone to keep the
        // channel open, so the signaling `select!` never busy-spins on a
        // fully-closed receiver even without a real overlay here.
        Ok(Self)
    }
    fn disabled() -> Self {
        Self
    }
    fn show(&self, _session_id: String, _controller_name: String) {}
    fn hide(&self, _session_id: String) {}
}

/// Compute a 1–2 character initials label from a controller's display
/// name for the overlay badge. "Goran Jovanov" → "GJ", "gjovanov" →
/// "GJ", "alice" → "AL", "" → "?". Splits on whitespace + common
/// separators, takes the first letter of the first two tokens (or the
/// first two letters of a single token), uppercased. Pure + platform-
/// agnostic so it's unit-tested in the default CI.
#[cfg_attr(
    not(all(target_os = "windows", feature = "viewer-indicator")),
    allow(dead_code)
)]
pub(crate) fn initials_of(name: &str) -> String {
    let tokens: Vec<&str> = name
        .split(|c: char| c.is_whitespace() || matches!(c, '.' | '_' | '-' | ','))
        .filter(|t| !t.is_empty())
        .collect();
    let mut out = String::new();
    match tokens.as_slice() {
        [] => {}
        [single] => {
            for c in single.chars().take(2) {
                out.extend(c.to_uppercase());
            }
        }
        [first, second, ..] => {
            if let Some(c) = first.chars().next() {
                out.extend(c.to_uppercase());
            }
            if let Some(c) = second.chars().next() {
                out.extend(c.to_uppercase());
            }
        }
    }
    if out.is_empty() {
        out.push('?');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::initials_of;

    #[test]
    fn initials_two_tokens() {
        assert_eq!(initials_of("Goran Jovanov"), "GJ");
        assert_eq!(initials_of("goran jovanov"), "GJ");
    }

    #[test]
    fn initials_separators() {
        assert_eq!(initials_of("goran.jovanov"), "GJ");
        assert_eq!(initials_of("goran_jovanov"), "GJ");
        assert_eq!(initials_of("goran-jovanov"), "GJ");
    }

    #[test]
    fn initials_single_token() {
        assert_eq!(initials_of("gjovanov"), "GJ");
        assert_eq!(initials_of("alice"), "AL");
        assert_eq!(initials_of("x"), "X");
    }

    #[test]
    fn initials_three_tokens_uses_first_two() {
        assert_eq!(initials_of("Jean Luc Picard"), "JL");
    }

    #[test]
    fn initials_empty_is_placeholder() {
        assert_eq!(initials_of(""), "?");
        assert_eq!(initials_of("   "), "?");
    }
}
