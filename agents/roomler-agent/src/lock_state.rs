//! Lock-screen detection for the user-context worker (M3 Z-path).
//!
//! Background. The M5 verification on PC50045 + e069019l confirmed
//! the field gap: when the user presses Win+L (or Windows otherwise
//! switches the input desktop to `winsta0\Winlogon`), the user-
//! context agent worker stays alive — `WTSGetActiveConsoleSessionId`
//! doesn't change, the SCM supervisor's `decide_spawn` returns
//! `KeepCurrent`, the WS connection stays connected, the WebRTC
//! peer stays connected — but capture frames go black/stale because
//! the worker's desktop attachment (`winsta0\Default`) is no longer
//! visible, and input injection is silently dropped because
//! `SendInput` targets the wrong desktop.
//!
//! M3's Z-path closes this in the simplest possible way: detect the
//! lock transition from the user-context worker, paint a static
//! "Host is locked" overlay frame to the encoder until unlock, and
//! suppress input injection. No SYSTEM-context capture+input thread,
//! no IPC, no remote-unlock — just a dignified "we're paused"
//! signal so the operator doesn't see a frozen desktop and assume
//! the agent crashed.
//!
//! Detection mechanism. We poll `OpenInputDesktop` every 500 ms from
//! the user-context worker. Because the worker runs in the user's
//! security context — *not* SYSTEM — the call returns:
//!   - `Ok(Some(_))` with desktop name `"Default"` when the user is
//!     on their normal interactive desktop
//!   - `Ok(None)` (`ERROR_ACCESS_DENIED`) when the input desktop has
//!     transitioned to `winsta0\Winlogon` (the lock screen, UAC
//!     consent, or a service-launched secure prompt)
//!   - `Ok(Some(_))` with a different desktop name in unusual cases
//!     (Citrix / RDP custom desktops); we treat anything that isn't
//!     `Default` as "not visible to me" → locked from our POV.
//!
//! 500 ms is a calm cadence. The actual desktop transition takes
//! ~250 ms on Win11, so the worst case the user sees is one half-
//! second of "frozen" frames before the overlay kicks in. Could be
//! tightened to 250 ms if field reports show that's user-visible,
//! but a full second of poll-loop CPU work × N agents × forever is
//! not free.
//!
//! Why not `WTSRegisterSessionNotification`? It fires on
//! `WTS_SESSION_LOCK` / `WTS_SESSION_UNLOCK` exactly when we want,
//! but requires a top-level window owned by the calling process to
//! receive the WM_WTSSESSION_CHANGE message — the agent worker is
//! a console app with no message pump, so plumbing that in adds
//! more code than the polling loop saves. Polling is also more
//! robust to the "user opened a UAC prompt" case which doesn't
//! fire WTS_SESSION_LOCK but DOES switch the input desktop.

use std::time::Duration;

/// Observable state of the user's interactive desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    /// Input desktop is `winsta0\Default` and we have access to it.
    /// Capture works, input injection works, normal operation.
    Unlocked,
    /// Input desktop is `winsta0\Winlogon` (or otherwise inaccessible
    /// to the user-context worker). Capture frames will be black or
    /// stale; input injection silently fails. The encoder should
    /// paint the "Host is locked" overlay until this flips back.
    Locked,
}

/// How often the lock-state poll loop wakes up. Tuned for "one half-
/// second of stale frames at worst is acceptable" against "we don't
/// burn a CPU core polling forever." Locked here so the encoder
/// pump and tests use the same value.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Pure: classify the result of an `OpenInputDesktop`-equivalent
/// probe into a `LockState`. Splitting this out from the polling
/// loop keeps the FFI surface a thin wrapper and the decision logic
/// (which has all the gotchas around desktop names) trivially
/// testable.
///
/// Inputs:
///   - `access_ok`: true when the OS handed us back a desktop
///     handle, false when the call returned ACCESS_DENIED or any
///     other failure. Behaviour treats *any* failure as Locked
///     because the most common cause of failure on a healthy host
///     is the desktop transition; spurious failures (resource
///     exhaustion etc.) are rare and falsely-locked is a softer
///     failure than falsely-unlocked.
///   - `desktop_name`: when access succeeded, the name returned
///     (e.g. `"Default"`). Empty string when access failed.
pub fn classify(access_ok: bool, desktop_name: &str) -> LockState {
    if !access_ok {
        return LockState::Locked;
    }
    // Desktop name comparison is case-sensitive per Win32 docs.
    // `winsta0\Default` is the canonical interactive desktop name
    // every user session has at logon. Anything else (Winlogon,
    // Citrix__1, etc.) is treated as "not visible from here" =
    // Locked, because the user-context capture/input plumbing only
    // works against Default.
    if desktop_name == "Default" {
        LockState::Unlocked
    } else {
        LockState::Locked
    }
}

#[cfg(target_os = "windows")]
mod win {
    use super::{LockState, classify};
    use crate::win_service::desktop;

    /// Probe the lock state from the user-context worker. Returns
    /// `Locked` when `OpenInputDesktop` denies access (the input
    /// desktop has transitioned to `winsta0\Winlogon`) OR when the
    /// returned desktop name isn't `"Default"`.
    pub fn probe_lock_state() -> LockState {
        match desktop::open_input_desktop() {
            Ok(Some(_d)) => {
                // We have access. Read the *current thread's*
                // desktop name (not the input desktop's) — the
                // worker thread is attached to Default and that's
                // what matters for capture/input. Realistically
                // the input desktop and our thread's desktop are
                // the same when access succeeds; the
                // current_thread_desktop_name read just re-confirms.
                match desktop::current_thread_desktop_name() {
                    Ok(name) => classify(true, &name),
                    Err(_) => classify(true, "Default"), // optimistic
                }
            }
            Ok(None) => classify(false, ""),
            Err(e) => {
                // Unexpected — log once at trace level so the field
                // can spot it, but treat as Locked to be safe.
                tracing::trace!(error = %e, "lock_state: OpenInputDesktop probe failed unexpectedly");
                classify(false, "")
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod nowin {
    use super::LockState;
    /// Non-Windows hosts don't have the desktop-switch problem.
    /// Always report Unlocked so the encoder pump runs normally.
    pub fn probe_lock_state() -> LockState {
        LockState::Unlocked
    }
}

#[cfg(not(target_os = "windows"))]
pub use nowin::probe_lock_state;
#[cfg(target_os = "windows")]
pub use win::probe_lock_state;

/// Spawn a tokio task that polls `probe_lock_state` every
/// `POLL_INTERVAL` and emits transitions on the returned
/// `tokio::sync::watch::Receiver<LockState>`. The watch channel
/// is the right primitive here: late subscribers can read the
/// current value, and the pump only wakes when the value changes
/// (no busy loop on consumers).
///
/// Drop the returned `JoinHandle` to abort the task; it has no
/// internal shutdown channel because it's cheap to abort and
/// shutdown of the agent ends the runtime anyway.
pub fn spawn_monitor() -> (
    tokio::sync::watch::Receiver<LockState>,
    tokio::task::JoinHandle<()>,
) {
    let initial = probe_lock_state();
    let (tx, rx) = tokio::sync::watch::channel(initial);
    let handle = tokio::spawn(async move {
        let mut last = initial;
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            // Receiver-gone-shutdown: when every receiver has been
            // dropped (the owning media pump exited), the watch
            // sender's `is_closed()` flips. Without this check the
            // monitor task can outlive its consumers indefinitely
            // because `tx.send()` only fires on state *change* —
            // a steady-Unlocked session never tries to send, never
            // notices the receivers are gone, and leaks the task
            // until runtime shutdown.
            if tx.is_closed() {
                return;
            }
            let current = probe_lock_state();
            if current != last {
                tracing::info!(
                    from = ?last,
                    to = ?current,
                    "lock_state: transition observed"
                );
                // We just confirmed the channel is open one tick
                // ago; if a race made it close between then and
                // now, the next tick's `is_closed` catches it.
                let _ = tx.send(current);
                last = current;
            }
        }
    });
    (rx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_default_with_access_is_unlocked() {
        assert_eq!(classify(true, "Default"), LockState::Unlocked);
    }

    #[test]
    fn classify_no_access_is_locked() {
        // The most common cause: input desktop transitioned to
        // Winlogon and the user-context probe got ACCESS_DENIED.
        assert_eq!(classify(false, ""), LockState::Locked);
        assert_eq!(classify(false, "Default"), LockState::Locked);
    }

    #[test]
    fn classify_other_desktop_name_is_locked() {
        // Citrix / RDP / custom desktops aren't accessible to our
        // user-context capture either; treat as Locked.
        assert_eq!(classify(true, "Winlogon"), LockState::Locked);
        assert_eq!(classify(true, "Disconnect"), LockState::Locked);
        assert_eq!(classify(true, "Citrix__1"), LockState::Locked);
        assert_eq!(classify(true, ""), LockState::Locked);
    }

    #[test]
    fn classify_is_case_sensitive_on_default() {
        // Win32 documents desktop name compares as case-sensitive.
        // "default" lower-case is NOT the same desktop as "Default";
        // treat as Locked rather than risk a false-unlocked that
        // sends bad capture frames.
        assert_eq!(classify(true, "default"), LockState::Locked);
        assert_eq!(classify(true, "DEFAULT"), LockState::Locked);
    }

    #[test]
    fn poll_interval_is_500ms() {
        // Lock the cadence: too-fast burns CPU on every host with
        // an installed agent (forever); too-slow leaves a visible
        // freeze on lock that confuses operators.
        assert_eq!(POLL_INTERVAL, Duration::from_millis(500));
    }

    #[test]
    fn lock_state_round_trip() {
        // The PartialEq derive lets us compare LockState values in
        // the watch-channel send-only-on-change path. Pin the
        // contract: equal variants must compare equal.
        assert_eq!(LockState::Locked, LockState::Locked);
        assert_eq!(LockState::Unlocked, LockState::Unlocked);
        assert_ne!(LockState::Locked, LockState::Unlocked);
    }
}
