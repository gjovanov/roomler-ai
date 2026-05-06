//! M3 A1 SYSTEM-context input injection backend.
//!
//! Same shape as [`super::enigo_backend::EnigoInjector`] (dedicated
//! OS thread + `std::mpsc` command channel + enigo `SendInput`), but
//! the worker thread:
//!
//! 1. Calls [`super::super::system_context::desktop_rebind::attach_to_winsta0`]
//!    once at startup. The SCM service container's window-station is
//!    `Service-0x0-3e7$` by default — `OpenDesktopW("Default" |
//!    "Winlogon")` fails with `ERROR_NOACCESS` from there. Attaching
//!    to `WinSta0` is the bootstrap that unblocks every subsequent
//!    desktop call.
//! 2. Runs an initial [`super::super::system_context::desktop_rebind::try_change_desktop`]
//!    so the thread starts bound to whichever desktop currently
//!    receives input (`Default` if a user is logged in, `Winlogon` if
//!    locked, `Screen-saver.Default` for an active screensaver).
//! 3. Constructs `Enigo` *after* the desktop binding is in place so
//!    enigo's lazy keyboard-layout cache (HKL) is loaded against the
//!    right desktop's window-station ACL.
//! 4. Re-runs `try_change_desktop` before every inject. The cost is
//!    one `OpenInputDesktop` syscall (microseconds); cheap enough at
//!    1000+ events/s during fast typing. `try_change_desktop` is
//!    self-deduping — only fires `SetThreadDesktop` when the input
//!    desktop actually differs from the thread's current binding.
//!
//! ## Why a dedicated thread
//!
//! `SetThreadDesktop` is per-thread. Tokio's work-stealing executor
//! distributes tasks across worker threads non-deterministically, so
//! a tokio-task injection that did `SetThreadDesktop` on one tokio
//! worker could find itself dispatched on a different worker on the
//! next poll, with the wrong desktop binding. Pinning to a single
//! `std::thread` owned by this module sidesteps that entirely.
//!
//! ## Why we don't reconstruct `Enigo` on rebind
//!
//! Enigo's internal state (cached HKL, modifier shadow state) is per-
//! thread, not per-desktop. Once the calling thread is `SetThreadDesktop`-
//! switched, subsequent `SendInput` calls land on the new desktop —
//! the per-thread state moves with the thread, not against it.
//! Rebuilding Enigo each rebind would lose the modifier shadow state
//! (Ctrl-down across a lock would never be released) without buying
//! anything.
//!
//! ## Wire-format passthrough
//!
//! `dispatch` is the same one [`super::enigo_backend`] uses (we re-
//! export it via `pub(super) fn dispatch`). The HID-to-VK table in
//! `hid_to_key` is shared too — this backend is purely a
//! desktop-rebind preamble in front of the same SendInput pipeline,
//! not a different keyboard semantics.
//!
//! ## Failure modes
//!
//! * `attach_to_winsta0` failing at startup → constructor returns
//!   `Err(...)`. Caller falls back to the standard EnigoInjector
//!   which still works in user context (the perUser MSI never gets
//!   here because `worker_role::probe_self()` returns
//!   `WorkerRole::User`).
//! * `try_change_desktop` failing per-event → log at warn and
//!   dispatch against the last-known binding. Better than dropping
//!   the event silently — the operator sees their click land on the
//!   previous desktop, which is wrong but recoverable on the next
//!   event after the rebind succeeds.
//! * Enigo `SendInput` failing per-event → log at debug; events drop
//!   silently (matches the user-context backend's behaviour).

#![cfg(all(
    feature = "system-context",
    target_os = "windows",
    feature = "enigo-input"
))]

use anyhow::{Context, Result, anyhow};
use enigo::{Enigo, Settings};
use std::sync::mpsc as std_mpsc;
use std::thread;

use super::enigo_backend;
use super::{InputInjector, InputMsg};
use crate::system_context::desktop_rebind;

/// Async-side handle. Constructor blocks until the worker thread has
/// successfully attached to `WinSta0` AND constructed `Enigo`; then
/// returns. After that all `inject` calls just push onto the mpsc.
pub struct SystemContextInjector {
    tx: std_mpsc::Sender<InputMsg>,
    has_perm: bool,
}

impl SystemContextInjector {
    pub fn new() -> Result<Self> {
        let (tx, rx) = std_mpsc::channel::<InputMsg>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();

        thread::Builder::new()
            .name("roomler-agent-system-input".into())
            .spawn(move || {
                if let Err(e) = desktop_rebind::attach_to_winsta0() {
                    let _ = ready_tx.send(Err(anyhow!("attach_to_winsta0: {e}")));
                    return;
                }
                match desktop_rebind::try_change_desktop() {
                    Ok(desktop_rebind::DesktopChange::Unchanged) => {
                        tracing::info!(
                            "system-context input: thread already bound to input desktop at startup"
                        );
                    }
                    Ok(desktop_rebind::DesktopChange::Switched(name)) => {
                        tracing::info!(
                            %name,
                            "system-context input: bound to input desktop at startup"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            %e,
                            "initial try_change_desktop in system-context input thread — non-fatal, will retry per-event"
                        );
                    }
                }
                let settings = Settings::default();
                let enigo = match Enigo::new(&settings) {
                    Ok(e) => {
                        let _ = ready_tx.send(Ok(()));
                        e
                    }
                    Err(e) => {
                        let _ =
                            ready_tx.send(Err(anyhow!("enigo init in SYSTEM context: {e}")));
                        return;
                    }
                };
                run_worker(enigo, rx);
            })
            .context("spawn system-context input thread")?;

        ready_rx
            .recv()
            .context("system-context input thread never responded")??;
        Ok(Self { tx, has_perm: true })
    }
}

/// Worker loop. Receives `InputMsg` commands; rebinds the thread's
/// desktop on each one (cheap; self-deduping); dispatches via the
/// shared enigo dispatcher.
fn run_worker(mut enigo: Enigo, rx: std_mpsc::Receiver<InputMsg>) {
    let mut events_since_log: u64 = 0;
    let mut consec_dispatch_errors: u64 = 0;
    while let Ok(msg) = rx.recv() {
        events_since_log = events_since_log.wrapping_add(1);
        match desktop_rebind::try_change_desktop() {
            Ok(desktop_rebind::DesktopChange::Unchanged) => {
                // Most common branch by ~1000:1. Stay quiet.
            }
            Ok(desktop_rebind::DesktopChange::Switched(name)) => {
                tracing::info!(
                    %name,
                    events_since_last_switch = events_since_log,
                    "system-context input: rebound desktop before dispatch"
                );
                events_since_log = 0;
                // Reset Enigo's per-thread state. After SetThreadDesktop
                // crosses a desktop boundary the previous Enigo's
                // cached HKL / modifier shadow state is stale; held
                // modifiers can't be released because their down-
                // events landed on the OLD desktop. Rebuilding Enigo
                // is cheap (~ms) and resets cached state cleanly.
                if let Ok(fresh) = Enigo::new(&Settings::default()) {
                    enigo = fresh;
                }
            }
            Err(e) => {
                tracing::warn!(
                    %e,
                    "try_change_desktop before input dispatch failed; dispatching against last-known binding"
                );
            }
        }
        match enigo_backend::dispatch_for_external(&mut enigo, msg.clone()) {
            Ok(_) => {
                consec_dispatch_errors = 0;
            }
            Err(e) => {
                consec_dispatch_errors = consec_dispatch_errors.saturating_add(1);
                // First error in a streak: attempt explicit rebind +
                // single retry. SendInput / SetCursorPos commonly
                // fail ACCESS_DENIED when the desktop's handle was
                // recycled mid-session (lock/unlock); a fresh
                // try_change_desktop + retry recovers without
                // session restart.
                if consec_dispatch_errors == 1 {
                    tracing::warn!(
                        %e,
                        "system-context input dispatch error; forcing rebind + retry"
                    );
                    let _ = desktop_rebind::try_change_desktop();
                    if let Ok(fresh) = Enigo::new(&Settings::default()) {
                        enigo = fresh;
                    }
                    if let Err(e2) = enigo_backend::dispatch_for_external(&mut enigo, msg) {
                        tracing::warn!(
                            error = %e2,
                            "system-context input retry-after-rebind also failed"
                        );
                    }
                } else if consec_dispatch_errors % 32 == 0 {
                    // Streak of failures; rate-limit logging to once
                    // per 32 consecutive errors so we don't spam.
                    tracing::warn!(
                        consec_errors = consec_dispatch_errors,
                        %e,
                        "system-context input dispatch failing in a streak"
                    );
                }
            }
        }
    }
    tracing::info!("system-context input worker thread exiting (cmd channel closed)");
}

impl InputInjector for SystemContextInjector {
    fn inject(&mut self, event: InputMsg) -> Result<()> {
        self.tx
            .send(event)
            .map_err(|_| anyhow!("system-context input worker exited"))
    }

    fn has_permission(&self) -> bool {
        self.has_perm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_is_send() {
        // ScreenCapture / InputInjector both require Send. The cmd_tx
        // is std::mpsc::Sender<InputMsg> which is Send when InputMsg
        // is Send (verified separately). Lock at compile time.
        fn assert_send<T: Send>() {}
        assert_send::<SystemContextInjector>();
    }

    #[test]
    fn impls_input_injector_trait() {
        fn assert_trait<T: InputInjector>() {}
        assert_trait::<SystemContextInjector>();
    }

    #[test]
    fn constructor_does_not_panic() {
        // Under the user-context test runner: attach_to_winsta0 is
        // idempotent (already on WinSta0); try_change_desktop returns
        // Unchanged; Enigo::new() succeeds. CI without a desktop:
        // Enigo::new() may fail; we accept Err. Lock against panic.
        let res = SystemContextInjector::new();
        drop(res);
    }
}
