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

/// FR-27 — where an operator's Approve/Deny click on a NATIVE prompt comes
/// back: `(session hex, allowed)`.
///
/// The backend does not resolve consent itself. It hands the answer here, the
/// signalling loop feeds it to `ConsentBroker::record_decision`, and the broker
/// applies the gate it already has — a decision counts only as an answer to a
/// question it is actively asking. One resolution point for the native panel,
/// the companion and the CLI alike; three would be three chances to disagree
/// about whether a session was approved.
pub type ConsentSender = tokio::sync::mpsc::Sender<(String, bool)>;

/// FR-27 — one consent question, as a native surface needs to draw it.
///
/// Deliberately pre-rendered strings rather than the wire types: a window
/// procedure running on a Win32 pump thread (or an X11 event loop, or the
/// AppKit main thread) should be laying out text, not reasoning about
/// `Permissions` bitflags or which subsystem asked.
#[derive(Clone, Debug)]
pub struct PromptView {
    pub session_hex: String,
    /// "Remote control request" / "Command execution request" / …
    pub title: String,
    /// "Alice is requesting to control this device."
    pub lead: String,
    /// The redacted command for `exec`, the activity for `ssh`; empty for rc.
    pub detail: String,
    /// Pipe-separated permission names; empty for exec/ssh.
    pub permissions: String,
    /// Asking organization on a multi-org device; empty otherwise.
    pub org: String,
    /// When the prompt stops mattering — so the panel can count down against
    /// the real deadline rather than one it started itself.
    pub expires_at: std::time::Instant,
}

/// A handle to the viewer-indicator worker. Cheap to clone; multiple
/// sessions sharing one handle is the common case (one worker, many
/// concurrent sessions → one combined label).
#[derive(Clone)]
pub struct ViewerIndicator {
    inner: Inner,
    /// FR-27 — the LocalAPI-visible mirror of what this overlay shows.
    ///
    /// Maintained HERE rather than at the signalling call sites because
    /// `show_session` / `hide_session` already are the session lifecycle, and
    /// there are four of them: keeping the registry in step by hand would be
    /// four chances to drift, and a banner that outlives its session is worse
    /// than no banner.
    registry: crate::rc_sessions::RcSessionRegistry,
    /// The channel a registry-driven Disconnect fires through — the same one
    /// the native overlay's own button uses, so both take one teardown path.
    kill_tx: Option<KillSender>,
}

impl ViewerIndicator {
    /// Spin up the worker. On Windows with the `viewer-indicator`
    /// feature this creates a background thread that owns a layered,
    /// click-through overlay window (a thin always-on border) plus a
    /// reveal-on-hover badge carrying the viewer's initials + a
    /// "Disconnect" control that fires through `kill_tx`. Everywhere
    /// else this is a no-op constructor — the returned handle accepts
    /// `show_session` / `hide_session` calls and drops them.
    pub fn new(
        kill_tx: KillSender,
        registry: crate::rc_sessions::RcSessionRegistry,
        consent_tx: ConsentSender,
    ) -> Result<Self> {
        Ok(Self {
            inner: Inner::new(kill_tx.clone(), consent_tx)?,
            registry,
            kill_tx: Some(kill_tx),
        })
    }

    /// Explicitly-disabled handle. Callers that can't bring the overlay
    /// up (init failed, headless CI, etc.) can use this so the rest of
    /// the code stays oblivious. Equivalent to `new()` on non-Windows.
    pub fn disabled() -> Self {
        Self {
            inner: Inner::disabled(),
            // A disabled OVERLAY is not a disabled session list. Everything
            // outside Windows lands here today, and those are exactly the
            // hosts whose only banner is the companion's — reading an empty
            // registry there would be the bug, not the safe default.
            registry: crate::rc_sessions::RcSessionRegistry::new(),
            kill_tx: None,
        }
    }

    /// FR-27 — attach the shared registry + kill channel to a handle built by
    /// [`Self::disabled`].
    ///
    /// The two capabilities are independent and were conflated: "no native
    /// overlay on this platform" is the NORMAL state on macOS and Linux, and
    /// it must not also mean "this device cannot report who is viewing it".
    pub fn with_registry(
        mut self,
        kill_tx: KillSender,
        registry: crate::rc_sessions::RcSessionRegistry,
    ) -> Self {
        self.registry = registry;
        self.kill_tx = Some(kill_tx);
        self
    }

    /// Announce that a session has started. The overlay redraws to
    /// include `controller_name` in its caption. Safe to call multiple
    /// times with the same `session_id` (idempotent — the name is
    /// replaced rather than appended).
    pub fn show_session(&self, session_id: String, controller_name: String) {
        self.inner.show(session_id.clone(), controller_name);
    }

    /// FR-27 — the registry-aware form. Same lifecycle point as
    /// [`Self::show_session`], plus the fields a remote banner needs (the
    /// grant, the asking org) and the kill channel its Disconnect fires
    /// through. Callers with that context should prefer it.
    pub fn show_session_full(
        &self,
        session_id: bson::oid::ObjectId,
        controller_name: String,
        permissions: String,
        org: String,
    ) {
        self.inner
            .show(session_id.to_hex(), controller_name.clone());
        if let Some(kill) = &self.kill_tx {
            self.registry
                .insert(session_id, controller_name, permissions, org, kill.clone());
        }
    }

    /// FR-27 — put a consent question on this device's screen, natively.
    ///
    /// Returns `false` when this build/platform/session has no native surface
    /// — which is the ordinary answer on a headless host, on Wayland under
    /// GNOME or KDE (neither exposes `wlr-layer-shell` to arbitrary clients),
    /// and in a build without the per-OS feature. The caller then falls back
    /// to the desktop companion, and only reports `no_prompt_surface` when
    /// that fails too.
    ///
    /// Answers arrive on the `ConsentSender` given at construction, never by
    /// resolving consent here: the broker stays the single decision point.
    pub fn show_prompt(&self, view: PromptView) -> bool {
        self.inner.prompt(view)
    }

    /// Take a consent prompt down — it was answered, timed out, or resolved
    /// somewhere else (the CLI, the companion, an emailed link). Idempotent.
    pub fn hide_prompt(&self, session_hex: &str) {
        self.inner.dismiss(session_hex);
    }

    /// Announce that a session has ended. When the last session drops,
    /// the overlay is hidden.
    pub fn hide_session(&self, session_id: String) {
        if let Ok(oid) = bson::oid::ObjectId::parse_str(&session_id) {
            self.registry.remove(&oid);
        }
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
    fn new(_kill_tx: KillSender, _consent_tx: ConsentSender) -> Result<Self> {
        // Drop the senders: `run()` retains its own clones to keep the
        // channels open, so the signaling `select!` never busy-spins on a
        // fully-closed receiver even without a real overlay here.
        Ok(Self)
    }
    fn disabled() -> Self {
        Self
    }
    fn show(&self, _session_id: String, _controller_name: String) {}
    fn hide(&self, _session_id: String) {}
    /// No native surface here — say so, rather than swallowing the prompt.
    /// `false` is what routes the caller to the companion.
    fn prompt(&self, _view: PromptView) -> bool {
        false
    }
    fn dismiss(&self, _session_hex: &str) {}
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
