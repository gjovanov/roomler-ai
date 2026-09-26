// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #1683 — the graceful-stop channel between the Windows SCM service host and
//! its worker.
//!
//! The service host stops its worker with `TerminateProcess`
//! ([`super::supervisor::OwnedProcess::terminate`]). A hard kill delivers no
//! console control event and never trips the worker's `terminate_signal()`, so
//! the worker never reaches its `os_initiated_stop` arm — which means an
//! **ephemeral** device installed as the SCM service never self-unenrolls on a
//! clean stop (only the server-side reaper collects it, after the TTL), and a
//! deliberate stop is miscounted toward `crash_count` on the next start (the
//! #1040 wall, on Windows). FR-51 promised a clean stop self-unenrolls in
//! seconds; that worked on Linux (SIGTERM) only.
//!
//! The fix is a named Win32 **manual-reset** event that the host
//! creates BEFORE it spawns each worker and signals on SCM stop / preshutdown:
//! the host asks the worker to leave gracefully, waits a bounded time
//! ([`super::supervisor::WORKER_GRACEFUL_STOP_BUDGET`]), and only then falls
//! back to `TerminateProcess`. The worker's Windows `terminate_signal()` waits
//! on the same event and, when it fires, takes the exact `os_initiated_stop`
//! arm a Unix SIGTERM takes.
//!
//! ## Security — why the DACL is load-bearing
//!
//! The worker may run as **SYSTEM** (SystemContext) or as the **signed-in user**
//! (the attended flavour). If any local user could *signal* this event, any
//! local user could stop a SYSTEM worker — a denial-of-service, and for an
//! ephemeral device an unenroll-by-anyone. So the event carries an explicit,
//! protected DACL ([`STOP_EVENT_SDDL`]):
//!
//! * **SYSTEM** (the host) gets `EVENT_ALL_ACCESS` — it created the event and is
//!   the only principal that may `SetEvent` it.
//! * **Interactive users** get `SYNCHRONIZE` only — enough to *wait*
//!   ([`wait_for_stop_event`]), never enough to signal. `SYNCHRONIZE` (`0x100000`)
//!   does not include `EVENT_MODIFY_STATE` (`0x2`), so a second interactive user
//!   (fast-user-switch), or the worker itself, can wait but cannot stop anyone.
//!
//! The name is unique per spawn (host PID + a monotonic counter), so a restart
//! never opens a stale, possibly-already-signaled handle. The host cannot use
//! the worker PID in the name — it must create the event before the worker
//! exists — so per-spawn uniqueness stands in for "per worker": each spawn gets
//! a fresh event, fresh name, fresh handle, torn down with the worker.
//!
//! Everything degrades safely: if the event cannot be created the worker spawns
//! anyway (the shutdown path just hard-terminates, exactly as before this
//! change), and if the worker cannot open/​wait the event it falls through to
//! `std::future::pending()` (today's behaviour), so the host's bounded wait
//! simply times out into the `TerminateProcess` fallback. The server-side
//! reaper remains the backstop for every exit that never reaches self-unenroll.

#![cfg(target_os = "windows")]

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};

/// The security descriptor, in SDDL, applied to the stop event. **This is a
/// security decision — read the module docs before changing it.**
///
/// * `D:P` — a protected DACL: nothing is inherited, only what is written here.
/// * `(A;;0x1f0003;;;SY)` — `EVENT_ALL_ACCESS` for **SYSTEM** (the host). Full
///   access includes `EVENT_MODIFY_STATE`, so only SYSTEM may `SetEvent`.
/// * `(A;;0x100000;;;IU)` — `SYNCHRONIZE` for **Interactive Users** (the
///   attended worker, which runs as the console user). Wait-only: `0x100000`
///   excludes `EVENT_MODIFY_STATE` (`0x2`), so no unprivileged local user can
///   stop a SYSTEM worker through this event.
///
/// The SystemContext worker runs as SYSTEM and is covered by the `SY` ACE; the
/// attended worker is an interactive logon and is covered by `IU`.
const STOP_EVENT_SDDL: &str = "D:P(A;;0x1f0003;;;SY)(A;;0x100000;;;IU)";

/// `SDDL_REVISION_1` — the only defined SDDL revision.
const SDDL_REVISION_1: u32 = 1;

/// `SYNCHRONIZE` — the sole access the worker asks for when it opens the event
/// to wait. Deliberately NOT `EVENT_MODIFY_STATE`: the worker waits, it never
/// signals.
const SYNCHRONIZE: u32 = 0x0010_0000;

/// The kernel-object name prefix. `Global\` so the worker (which may be in a
/// user session, i.e. session ≥ 1) can open an event the host created in
/// session 0. The host is SYSTEM and holds `SeCreateGlobalPrivilege`; opening a
/// `Global\` object needs no privilege.
const STOP_EVENT_NAME_PREFIX: &str = "Global\\roomler-worker-stop-";

/// Monotonic per-process counter that makes each spawn's event name unique.
static SPAWN_SEQ: AtomicU64 = AtomicU64::new(0);

/// A NUL-terminated UTF-16 buffer for a Win32 wide-string argument.
fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The unique name for the next spawn's stop event: `Global\roomler-worker-stop-
/// <host-pid>-<seq>`. Host PID keeps names from colliding across concurrent
/// hosts (there is only ever one, but a stale PID-reused name would still not
/// collide with a live one); the sequence keeps them unique within a host's
/// lifetime.
fn next_event_name() -> String {
    let seq = SPAWN_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}{}-{}", STOP_EVENT_NAME_PREFIX, std::process::id(), seq)
}

/// Owns a Win32 `HANDLE`, closing it on drop. Local to this module (the
/// supervisor has its own `OwnedHandle`); a stop-event handle never leaves it.
struct EventHandle(HANDLE);

// SAFETY: a Win32 `HANDLE` is a process-wide reference to a kernel object with
// no thread affinity; `SetEvent` / `WaitForSingleObject` / `CloseHandle` are
// all thread-safe. Send so a `HostStopEvent` may live on the supervisor's
// `ActiveWorker` and be handed between the supervisor's own frames.
unsafe impl Send for EventHandle {}

impl Drop for EventHandle {
    fn drop(&mut self) {
        // SAFETY: we own this handle and hand its raw value to no other owner.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// The host side of the stop event: created by the SCM service host before it
/// spawns a worker, signaled once on SCM stop / preshutdown, and dropped (which
/// closes the handle) when the worker it belongs to is reaped or swapped.
pub struct HostStopEvent {
    handle: EventHandle,
    name: String,
}

impl HostStopEvent {
    /// Create a fresh, uniquely-named manual-reset event with [`STOP_EVENT_SDDL`],
    /// initially unsignaled. The caller passes [`name`](Self::name) to the
    /// worker as `--stop-event <name>`.
    pub fn create() -> io::Result<Self> {
        let name = next_event_name();

        // Build the security descriptor from SDDL. `psd` is LocalAlloc'd and
        // must be LocalFree'd; the OS copies it into the event at create time,
        // so we free our copy immediately after CreateEventW.
        let sddl_w = to_wide(STOP_EVENT_SDDL);
        let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `sddl_w` is a NUL-terminated UTF-16 buffer; `psd` is a valid
        // out-pointer; the size-out argument is null (documented optional).
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_w.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd,
            bInheritHandle: 0,
        };
        let name_w = to_wide(&name);
        // SAFETY: `&sa` outlives the call; `sa.lpSecurityDescriptor` is the SD
        // we just built; `name_w` is NUL-terminated. Manual-reset (1) so a
        // set-before-wait is never lost; initial state unsignaled (0).
        let handle = unsafe { CreateEventW(&sa, 1, 0, name_w.as_ptr()) };
        // Capture the create error BEFORE freeing the SD (LocalFree could reset
        // the thread-local error).
        let create_err = unsafe { GetLastError() };
        // SAFETY: `psd` came from ConvertStringSecurityDescriptor…W (LocalAlloc);
        // the event has copied it, so LocalFree of our copy is the documented
        // release and safe on all paths below.
        let _ = unsafe { LocalFree(psd) };

        if handle.is_null() {
            return Err(io::Error::from_raw_os_error(create_err as i32));
        }
        if create_err == ERROR_ALREADY_EXISTS {
            // A name collision (effectively impossible with host-PID + counter)
            // would hand us a pre-existing, possibly-already-signaled event.
            // Refuse it: a fresh handle here would be worse than falling back to
            // the hard-terminate path, which the caller does when create fails.
            // SAFETY: `handle` is the valid handle CreateEventW just returned.
            unsafe {
                CloseHandle(handle);
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("stop event {name} already existed"),
            ));
        }
        Ok(Self {
            handle: EventHandle(handle),
            name,
        })
    }

    /// The kernel-object name — pass it to the worker as `--stop-event <name>`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Signal the event: the SCM host calls this on stop / preshutdown to ask
    /// the worker to leave gracefully. Best-effort — a failure is logged and the
    /// caller's bounded wait then falls back to `TerminateProcess`.
    pub fn signal(&self) {
        // SAFETY: the creator's handle carries EVENT_ALL_ACCESS (incl.
        // EVENT_MODIFY_STATE) regardless of the DACL, so SetEvent is permitted.
        let ok = unsafe { SetEvent(self.handle.0) };
        if ok == 0 {
            // SAFETY: GetLastError is a thread-local read.
            let err = unsafe { GetLastError() };
            tracing::warn!(name = %self.name, err, "stop event: SetEvent failed");
        }
    }
}

/// Open the named stop event and wait, with a bounded timeout in ms. Returns
/// `Ok(true)` if it was signaled, `Ok(false)` on timeout, `Err` if it could not
/// be opened (e.g. the host never created it). Opens with `SYNCHRONIZE` only.
fn wait_impl(name: &str, timeout_ms: u32) -> io::Result<bool> {
    let name_w = to_wide(name);
    // SAFETY: `name_w` is NUL-terminated; we ask for SYNCHRONIZE (wait) only,
    // not inheritable. A null return means the open failed (GetLastError).
    let handle = unsafe { OpenEventW(SYNCHRONIZE, 0, name_w.as_ptr()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let guard = EventHandle(handle);
    // SAFETY: `guard.0` is a valid handle opened for SYNCHRONIZE.
    let r = unsafe { WaitForSingleObject(guard.0, timeout_ms) };
    drop(guard);
    match r {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        other => Err(io::Error::other(format!(
            "WaitForSingleObject on the stop event returned 0x{other:x}"
        ))),
    }
}

/// Worker side: block until the SCM host signals the stop event named `name`.
/// Called from the worker's `terminate_signal()` (on a blocking task, since the
/// wait is blocking). Returns `Ok(())` when the host asked for a graceful stop;
/// `Err` if the event could not be opened/​waited — in which case the caller
/// must NOT treat it as a stop request (it awaits `pending()` instead), so a
/// missing or inaccessible event never fabricates a shutdown.
pub fn wait_for_stop_event(name: &str) -> io::Result<()> {
    match wait_impl(name, INFINITE)? {
        true => Ok(()),
        // INFINITE cannot time out; a non-signalled return is anomalous and must
        // not read as "stop".
        false => Err(io::Error::other(
            "stop-event wait returned without a signal",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sddl_is_the_locked_security_contract() {
        // Only SYSTEM may SIGNAL (EVENT_MODIFY_STATE ⊂ 0x1f0003); an interactive
        // user gets 0x100000 = SYNCHRONIZE = WAIT ONLY. That is the whole
        // security property (#1683): no unprivileged local user can stop a
        // SYSTEM worker. Changing this string is changing that decision.
        assert_eq!(STOP_EVENT_SDDL, "D:P(A;;0x1f0003;;;SY)(A;;0x100000;;;IU)");
        // …and it must actually convert + create, or every host would fail
        // closed (no stop event ⇒ hard-terminate ⇒ the bug is back).
        let ev = HostStopEvent::create();
        assert!(
            ev.is_ok(),
            "the locked SDDL must build an event: {:?}",
            ev.err()
        );
    }

    #[test]
    fn stop_event_names_are_unique_and_global() {
        let a = HostStopEvent::create().expect("create a");
        let b = HostStopEvent::create().expect("create b");
        assert_ne!(a.name(), b.name(), "each spawn needs its own event name");
        assert!(
            a.name().starts_with(STOP_EVENT_NAME_PREFIX),
            "name must be in the Global namespace: {}",
            a.name()
        );
    }

    #[test]
    fn host_signal_is_observed_by_the_worker_wait() {
        // The end-to-end mechanism, in one process: the host creates + signals,
        // a "worker" opens the same name and waits. Before this module, the
        // worker's Windows terminate_signal() was pending() forever, so this
        // round-trip did not exist.
        let ev = HostStopEvent::create().expect("create");
        let name = ev.name().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            // Generous bound so a slow CI box never flakes; the signal arrives
            // in milliseconds in practice.
            let _ = tx.send(wait_impl(&name, 5_000));
        });
        // Let the waiter reach WaitForSingleObject, then signal.
        std::thread::sleep(std::time::Duration::from_millis(200));
        ev.signal();
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the waiter must return a result");
        assert!(
            matches!(got, Ok(true)),
            "the worker's wait must observe the host's signal: {got:?}"
        );
        let _ = waiter.join();
    }

    #[test]
    fn an_unsignaled_event_times_out_rather_than_resolving() {
        // Proves the wait actually blocks on the event — it must not resolve
        // early (which, in terminate_signal, would fabricate a stop and, for an
        // ephemeral device, an erroneous self-unenroll).
        let ev = HostStopEvent::create().expect("create");
        let r = wait_impl(ev.name(), 200);
        assert!(
            matches!(r, Ok(false)),
            "an unsignaled event must time out, not resolve: {r:?}"
        );
    }

    #[test]
    fn waiting_on_a_missing_event_errs() {
        // The safe-degradation path: no event ⇒ Err ⇒ the worker awaits
        // pending() instead of treating it as a stop.
        let r = wait_impl("Global\\roomler-worker-stop-nonexistent-1683", 100);
        assert!(
            r.is_err(),
            "opening an absent event must error so the worker degrades to terminate"
        );
    }
}
