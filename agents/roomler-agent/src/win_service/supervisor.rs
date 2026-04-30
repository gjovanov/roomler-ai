//! Worker-process supervisor (Effort 2 M2).
//!
//! When the SCM-launched service receives a `Running` state and
//! a console session is active, this module spawns the agent's own
//! `roomler-agent.exe run` as the active user via
//! `WTSQueryUserToken` + `CreateProcessAsUserW`, then watches it.
//! The worker exiting non-zero triggers a respawn with exponential
//! backoff (parity with the Scheduled Task `RestartOnFailure` PT1M
//! × 10 we ship for the user-mode model).
//!
//! Session change notifications from the SCM (LOGON / LOGOFF /
//! CONSOLE_CONNECT / etc.) flow into the supervisor via a
//! `mpsc::Sender<Event>` so the active-session resolution stays in
//! one place: the supervisor reacts by tearing down the old worker
//! and trying to spawn one in the new active session.
//!
//! M3 will add a SYSTEM-context capture path here for the case
//! where no console session has a logged-in user yet (pre-logon
//! lock-screen scenario). Today, no-active-session is reported and
//! the supervisor idles until the first LOGON event.
//!
//! Safety: a non-trivial chunk of `unsafe` for raw Win32 FFI. Each
//! unsafe block has a localised SAFETY comment explaining the
//! invariants. We never expose raw HANDLEs across function
//! boundaries — `OwnedHandle` wraps the lifetime so leaks / double-
//! frees become compile errors.

#![cfg(target_os = "windows")]

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_TOKEN, FALSE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetExitCodeProcess,
    PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

/// Sentinel for "no console session is currently attached" — what
/// `WTSGetActiveConsoleSessionId` returns when the host is at the
/// boot screen, between log-outs, or sitting at the Windows lock
/// screen with no user ever logged in this boot.
const NO_ACTIVE_SESSION: u32 = 0xFFFF_FFFF;

/// Backoff cap for worker respawn. Mirrors the Scheduled Task's
/// `RestartOnFailure` PT1M cap so an admin-installed service feels
/// the same as the user-mode auto-start under repeated failures.
const RESPAWN_BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Initial respawn delay. Short enough that a one-shot crash is
/// barely noticeable; doubles up to the cap on repeated crashes.
const RESPAWN_BACKOFF_INITIAL: Duration = Duration::from_secs(2);

/// How long to wait between supervisor wake-ups when no work is
/// pending. Used for the "is the worker still alive?" poll loop.
/// Short enough that a worker crash is detected promptly; long
/// enough that we don't burn CPU on the idle service.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// External signal types the SCM control handler sends in.
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    /// SCM Stop / Preshutdown received. Tear down the worker and exit.
    Shutdown,
    /// SCM SessionChange received. Resolve a fresh active-session
    /// id and respawn the worker if it changed.
    SessionChanged,
}

/// Wrapper that closes the handle on drop. The Win32 `HANDLE` type
/// is just a pointer-sized integer with no Drop semantics; without
/// this every error path leaks an OS handle. `pub` because
/// `query_user_token` returns it across the module boundary; safe to
/// expose because the only operations available are `raw()` (read-only
/// borrow of the inner HANDLE) and Drop.
pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(h: HANDLE) -> Option<Self> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            None
        } else {
            Some(Self(h))
        }
    }
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: We own this handle and have not handed it out as
        // raw to any other long-lived owner. CloseHandle on a valid
        // open handle is the canonical cleanup.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// `WTSGetActiveConsoleSessionId` wrapper. Returns the session id of
/// the user currently attached to the physical console, or `None`
/// for "no active session" (the host's at the boot screen, locked
/// without anyone logged in, or between users).
pub fn active_console_session_id() -> Option<u32> {
    // SAFETY: WTSGetActiveConsoleSessionId is a thread-safe Win32
    // call with no preconditions. Returns 0xFFFFFFFF when no session
    // is active.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    if id == NO_ACTIVE_SESSION {
        None
    } else {
        Some(id)
    }
}

/// `WTSQueryUserToken` wrapper. Must be called from a process running
/// as `LocalSystem` (which the SCM-launched service is) — under any
/// other principal it returns `ERROR_PRIVILEGE_NOT_HELD`. Returns
/// `None` if no user is logged into the session (e.g. a console
/// session showing the lock screen with no user ever logged in
/// this boot — the supervisor's M3 SYSTEM-context fallback handles
/// that case).
pub fn query_user_token(session_id: u32) -> Result<Option<OwnedHandle>> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `token` is an out-pointer the API writes a HANDLE to.
    // We check the return code and wrap the result in OwnedHandle so
    // CloseHandle is guaranteed.
    let ok = unsafe { WTSQueryUserToken(session_id, &mut token) };
    if ok == 0 {
        // SAFETY: GetLastError is a thread-local read.
        let err = unsafe { GetLastError() };
        if err == ERROR_NO_TOKEN {
            return Ok(None);
        }
        bail!("WTSQueryUserToken({session_id}) failed (err {err})");
    }
    Ok(OwnedHandle::new(token))
}

/// `CreateEnvironmentBlock` wrapper. Returns the user-context env
/// block (USERPROFILE, APPDATA, PATH, …) suitable for passing to
/// CreateProcessAsUserW. The pointer must be passed to
/// `DestroyEnvironmentBlock` when no longer needed; we package it
/// in a tiny RAII guard.
struct EnvBlock {
    raw: *mut std::ffi::c_void,
}

impl EnvBlock {
    fn for_token(token: HANDLE) -> Result<Self> {
        let mut env: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `&mut env` is a valid out-pointer; second arg is
        // the user token. We pass FALSE for inherit so the system
        // env doesn't leak into the worker.
        let ok = unsafe { CreateEnvironmentBlock(&mut env, token, FALSE) };
        if ok == 0 {
            // SAFETY: GetLastError is a thread-local read.
            let err = unsafe { GetLastError() };
            bail!("CreateEnvironmentBlock failed (err {err})");
        }
        Ok(Self { raw: env })
    }
}

impl Drop for EnvBlock {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: `raw` is the same pointer the OS gave us via
            // CreateEnvironmentBlock; pairing it with Destroy is the
            // documented free path.
            unsafe {
                DestroyEnvironmentBlock(self.raw);
            }
        }
    }
}

/// Encode a Rust string into a NUL-terminated UTF-16 buffer that
/// CreateProcessAsUserW expects.
fn encode_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Spawn `<exe> <args>` in the user session whose `token` we hold.
/// Caller is responsible for closing the returned PROCESS_INFORMATION
/// handles (we do this via [`OwnedProcess`]).
///
/// # Safety
/// `token` must be a valid Win32 user token (typically from
/// [`query_user_token`]); the function dereferences the raw HANDLE
/// when handing it to `CreateProcessAsUserW`. Marked unsafe so the
/// caller's contract about `token`'s liveness is explicit.
pub unsafe fn spawn_in_session(token: HANDLE, exe: &Path, args: &[&str]) -> Result<OwnedProcess> {
    let env = EnvBlock::for_token(token).context("CreateEnvironmentBlock")?;

    // CreateProcessAsUserW takes the command line in a single mutable
    // wide-string buffer. Quote the exe (handles paths with spaces)
    // and join the arguments with single spaces. We don't try to
    // shell-escape arguments — the only callers are inside this
    // crate and pass simple strings (e.g. "run").
    let mut cmdline = String::with_capacity(exe.as_os_str().len() + 32);
    cmdline.push('"');
    cmdline.push_str(&exe.to_string_lossy());
    cmdline.push('"');
    for a in args {
        cmdline.push(' ');
        cmdline.push_str(a);
    }
    let mut cmdline_w = encode_wide(OsStr::new(&cmdline));

    let exe_w = encode_wide(exe.as_os_str());

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    // Attach to winsta0\default so the worker's GUI subsystem gets
    // a usable interactive desktop. Without this CreateProcessAsUserW
    // attaches to a noninteractive desktop and any GUI APIs
    // (including MessageBox) silently no-op.
    let mut desktop_w = encode_wide(OsStr::new("winsta0\\default"));
    si.lpDesktop = desktop_w.as_mut_ptr();

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // SAFETY: All buffers are alive for the duration of the call;
    // out-params are valid mutable pointers; flags are documented.
    let ok = unsafe {
        CreateProcessAsUserW(
            token,
            exe_w.as_ptr(),
            cmdline_w.as_mut_ptr(),
            std::ptr::null_mut(), // process security attributes
            std::ptr::null_mut(), // thread security attributes
            FALSE,                // inherit handles
            CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_CONSOLE,
            env.raw,
            std::ptr::null(), // current directory — let the user's profile decide
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        // SAFETY: GetLastError is a thread-local read.
        let err = unsafe { GetLastError() };
        bail!("CreateProcessAsUserW({}) failed (err {err})", exe.display());
    }
    drop(env);

    Ok(OwnedProcess {
        process: OwnedHandle(pi.hProcess),
        thread: OwnedHandle(pi.hThread),
        pid: pi.dwProcessId,
    })
}

/// Owned process + thread handles from a successful CreateProcess.
/// Close on drop, expose a non-blocking exit-code probe.
pub struct OwnedProcess {
    process: OwnedHandle,
    #[allow(dead_code)]
    thread: OwnedHandle,
    pub pid: u32,
}

impl OwnedProcess {
    /// Non-blocking exit-code probe. Returns:
    ///   - `Ok(None)` if the process is still running
    ///   - `Ok(Some(code))` once it has exited
    pub fn try_wait(&self) -> Result<Option<u32>> {
        // SAFETY: process handle is valid for SYNCHRONIZE. 0 ms
        // timeout makes WaitForSingleObject non-blocking.
        let r = unsafe { WaitForSingleObject(self.process.raw(), 0) };
        match r {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code: u32 = 0;
                // SAFETY: handle valid; `code` is an owned out-param.
                let ok = unsafe { GetExitCodeProcess(self.process.raw(), &mut code) };
                if ok == 0 {
                    // SAFETY: thread-local error.
                    let err = unsafe { GetLastError() };
                    bail!("GetExitCodeProcess failed (err {err})");
                }
                if code as i32 == STILL_ACTIVE {
                    // Edge: WaitForSingleObject signalled but the
                    // exit code is still STILL_ACTIVE — should not
                    // happen, but treat as still running.
                    return Ok(None);
                }
                Ok(Some(code))
            }
            other => bail!("WaitForSingleObject returned 0x{other:x}"),
        }
    }

    /// Best-effort terminate. Used when the SCM is shutting us down
    /// or when a session change makes the current worker stale.
    pub fn terminate(&self) {
        // SAFETY: process handle valid for TERMINATE access.
        // Exit code 1 — distinct from a clean shutdown and from
        // STILL_ACTIVE, so logs can tell the cases apart.
        unsafe {
            TerminateProcess(self.process.raw(), 1);
        }
    }
}

/// Decide whether to (re)spawn the worker, and what session it
/// should attach to. Pure: no side effects, no FFI, easy to unit-
/// test the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnDecision {
    /// Spawn a worker in this session id.
    SpawnIn(u32),
    /// Worker is already running in the right session — leave it.
    KeepCurrent,
    /// No active session; idle. (M3 will replace this with a
    /// SystemContextCapture variant.)
    Idle,
}

pub fn decide_spawn(
    active_session: Option<u32>,
    current_worker_session: Option<u32>,
) -> SpawnDecision {
    match (active_session, current_worker_session) {
        (None, _) => SpawnDecision::Idle,
        (Some(active), Some(current)) if active == current => SpawnDecision::KeepCurrent,
        (Some(active), _) => SpawnDecision::SpawnIn(active),
    }
}

/// Compute the next backoff duration given how many consecutive
/// failures we've seen. Doubles from 2 s to 60 s cap.
pub fn next_backoff(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    // 2 s * 2^(n-1) capped at 60 s. Saturating shift so a runaway
    // counter doesn't overflow.
    let factor = 1u64
        .checked_shl(consecutive_failures.saturating_sub(1))
        .unwrap_or(u64::MAX);
    let secs = RESPAWN_BACKOFF_INITIAL.as_secs().saturating_mul(factor);
    let backoff = Duration::from_secs(secs);
    if backoff > RESPAWN_BACKOFF_CAP {
        RESPAWN_BACKOFF_CAP
    } else {
        backoff
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Supervisor main loop. Called from `service_main_inner`.
// ────────────────────────────────────────────────────────────────────────────

/// Run the supervisor until [`SupervisorEvent::Shutdown`] arrives.
/// `worker_exe` is the path to the agent binary — typically
/// `std::env::current_exe()` resolved by the SCM-installed service
/// — which gets relaunched with `worker_args` (e.g. `["run"]`) in
/// the active console session.
pub fn run(
    worker_exe: PathBuf,
    worker_args: Vec<String>,
    rx: mpsc::Receiver<SupervisorEvent>,
) -> Result<()> {
    let args_borrow: Vec<&str> = worker_args.iter().map(String::as_str).collect();

    let mut current_worker: Option<OwnedProcess> = None;
    let mut current_session: Option<u32> = None;
    let mut consecutive_failures: u32 = 0;
    let mut respawn_at: Option<Instant> = None;

    loop {
        // Drain pending events without blocking, so a flurry of
        // SessionChange notifications gets coalesced into one
        // resolution pass.
        let mut shutdown = false;
        loop {
            match rx.try_recv() {
                Ok(SupervisorEvent::Shutdown) => {
                    shutdown = true;
                    break;
                }
                Ok(SupervisorEvent::SessionChanged) => {
                    tracing::info!("supervisor: SessionChange received; will resolve");
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    tracing::warn!("supervisor: control channel closed; treating as shutdown");
                    shutdown = true;
                    break;
                }
            }
        }
        if shutdown {
            if let Some(w) = current_worker.take() {
                tracing::info!(pid = w.pid, "supervisor: terminating worker on shutdown");
                w.terminate();
            }
            return Ok(());
        }

        // Reap a finished worker.
        if let Some(w) = current_worker.as_ref() {
            match w.try_wait() {
                Ok(Some(code)) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let backoff = next_backoff(consecutive_failures);
                    tracing::warn!(
                        pid = w.pid,
                        code,
                        consecutive_failures,
                        backoff_secs = backoff.as_secs(),
                        "supervisor: worker exited; backing off before respawn"
                    );
                    respawn_at = Some(Instant::now() + backoff);
                    current_worker = None;
                    current_session = None;
                }
                Ok(None) => { /* still running */ }
                Err(e) => {
                    tracing::warn!(error = %e, "supervisor: try_wait failed; assuming worker is gone");
                    current_worker = None;
                    current_session = None;
                }
            }
        }

        // Decide whether to spawn.
        let active = active_console_session_id();
        let decision = decide_spawn(active, current_session);
        let due_for_respawn = respawn_at.is_none_or(|t| Instant::now() >= t);

        match (decision, due_for_respawn) {
            (SpawnDecision::SpawnIn(sid), true) if current_worker.is_none() => {
                match query_user_token(sid) {
                    Ok(Some(token)) => {
                        // SAFETY: `token` is a fresh, owned Win32 user
                        // token from WTSQueryUserToken; valid until the
                        // OwnedHandle drops at end of this scope. The
                        // CreateProcessAsUserW call inside duplicates
                        // any handles it needs.
                        match unsafe { spawn_in_session(token.raw(), &worker_exe, &args_borrow) } {
                            Ok(p) => {
                                tracing::info!(
                                    pid = p.pid,
                                    session_id = sid,
                                    "supervisor: spawned worker"
                                );
                                current_worker = Some(p);
                                current_session = Some(sid);
                                consecutive_failures = 0;
                                respawn_at = None;
                            }
                            Err(e) => {
                                consecutive_failures = consecutive_failures.saturating_add(1);
                                let bo = next_backoff(consecutive_failures);
                                tracing::warn!(
                                    error = %e,
                                    consecutive_failures,
                                    backoff_secs = bo.as_secs(),
                                    "supervisor: spawn failed; backing off"
                                );
                                respawn_at = Some(Instant::now() + bo);
                            }
                        }
                    }
                    Ok(None) => {
                        // No user logged into this session — M3
                        // territory. Idle for now.
                        tracing::debug!(
                            session_id = sid,
                            "supervisor: WTSQueryUserToken returned no token; idling for M3 fallback"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, session_id = sid, "supervisor: WTSQueryUserToken failed");
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        respawn_at = Some(Instant::now() + next_backoff(consecutive_failures));
                    }
                }
            }
            (SpawnDecision::SpawnIn(sid), _)
                if current_worker.is_some() && current_session != Some(sid) =>
            {
                // Active session changed under us — kill the old
                // worker, the next loop iteration will spawn a new
                // one for `sid`.
                if let Some(old) = current_worker.take() {
                    tracing::info!(
                        pid = old.pid,
                        old_session = ?current_session,
                        new_session = sid,
                        "supervisor: active session changed; terminating old worker"
                    );
                    old.terminate();
                }
                current_session = None;
            }
            _ => {}
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_spawn_idles_when_no_active_session() {
        assert_eq!(decide_spawn(None, None), SpawnDecision::Idle);
        assert_eq!(decide_spawn(None, Some(2)), SpawnDecision::Idle);
    }

    #[test]
    fn decide_spawn_keeps_worker_when_session_unchanged() {
        assert_eq!(decide_spawn(Some(2), Some(2)), SpawnDecision::KeepCurrent);
    }

    #[test]
    fn decide_spawn_targets_active_session_when_no_worker() {
        assert_eq!(decide_spawn(Some(2), None), SpawnDecision::SpawnIn(2));
    }

    #[test]
    fn decide_spawn_targets_new_session_when_active_changed() {
        // Worker is for session 2 but the active console moved to 5
        // (the previous user logged out, a new one logged in).
        assert_eq!(decide_spawn(Some(5), Some(2)), SpawnDecision::SpawnIn(5));
    }

    #[test]
    fn next_backoff_zero_on_zero_failures() {
        assert_eq!(next_backoff(0), Duration::ZERO);
    }

    #[test]
    fn next_backoff_doubles_then_caps() {
        assert_eq!(next_backoff(1), Duration::from_secs(2));
        assert_eq!(next_backoff(2), Duration::from_secs(4));
        assert_eq!(next_backoff(3), Duration::from_secs(8));
        assert_eq!(next_backoff(4), Duration::from_secs(16));
        assert_eq!(next_backoff(5), Duration::from_secs(32));
        // Cap kicks in at 60 s.
        assert_eq!(next_backoff(6), RESPAWN_BACKOFF_CAP);
        assert_eq!(next_backoff(10), RESPAWN_BACKOFF_CAP);
        // And a runaway counter doesn't panic.
        assert_eq!(next_backoff(u32::MAX), RESPAWN_BACKOFF_CAP);
    }

    #[test]
    fn active_console_session_id_is_callable() {
        // Smoke test only — we can't assert a specific value because
        // CI runners and dev machines have wildly different session
        // layouts. Just confirm the call doesn't panic and produces
        // either Some(u32) or None.
        let _ = active_console_session_id();
    }
}
