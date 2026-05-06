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
    /// Wrap raw process+thread HANDLEs that came from another
    /// spawn pipeline (e.g. the M3 A1 winlogon-token path's
    /// `ChildHandle::into_raw_parts`) into an `OwnedProcess` so
    /// the existing supervisor poll/terminate logic can drive
    /// either kind of worker uniformly.
    ///
    /// Caller transfers ownership: after this call, dropping the
    /// returned `OwnedProcess` will `CloseHandle` both. Both
    /// handles must be valid (non-null, non-INVALID_HANDLE_VALUE)
    /// — Drop assumes ownership unconditionally and the new
    /// constructor doesn't validate further.
    ///
    /// # Safety
    ///
    /// The HANDLEs must be:
    /// 1. Valid Win32 process / thread handles the caller produced
    ///    via a CreateProcess-family call.
    /// 2. NOT shared with any other live owner that would also
    ///    `CloseHandle` them — double-close is undefined behaviour.
    ///
    /// In practice the only caller is the M3 A1 spawn arm in
    /// [`run`], which receives the handles from
    /// `winlogon_token::ChildHandle::into_raw_parts` (which
    /// `mem::forget`'s the original wrapper, transferring sole
    /// ownership to the tuple).
    ///
    /// Gated behind `feature = "system-context"` because the only
    /// caller is the M3 A1 spawn arm; without the feature this
    /// would surface as a `dead_code` warning under `-D warnings`.
    #[cfg(feature = "system-context")]
    pub(crate) fn from_raw_parts(process: HANDLE, thread: HANDLE, pid: u32) -> Self {
        Self {
            process: OwnedHandle(process),
            thread: OwnedHandle(thread),
            pid,
        }
    }

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

    /// Block (with timeout) until the OS has actually reaped the
    /// process. `terminate()` only queues a kill — without a
    /// follow-up wait, the process can outlive its caller by tens
    /// to hundreds of milliseconds, which is enough for the named
    /// instance lock to still be held when the next worker is
    /// spawned. M5 finding #8 (PC50045 2026-05-02): a 145 ms gap
    /// between SCM Stop+Start was short enough that the new
    /// supervisor's first spawn lost the lock race and exited
    /// with code=0. Returns true if the process exited within the
    /// timeout, false otherwise.
    pub fn wait_for_exit(&self, timeout: Duration) -> bool {
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        // SAFETY: process handle is valid for SYNCHRONIZE; timeout
        // is a documented argument; return values are checked.
        let r = unsafe { WaitForSingleObject(self.process.raw(), timeout_ms) };
        r == WAIT_OBJECT_0
    }
}

/// How long to wait after `terminate()` for the OS to actually
/// reap the worker. 1.5 s is comfortably more than the observed
/// 145 ms gap on PC50045 and well under any human-perceptible
/// service-stop delay (services have 30 s before SCM force-kills).
const TERMINATE_WAIT: Duration = Duration::from_millis(1500);

/// Decide whether to (re)spawn the worker, and what session it
/// should attach to. Pure: no side effects, no FFI, easy to unit-
/// test the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnDecision {
    /// Spawn a worker in this session id.
    SpawnIn(u32),
    /// Worker is already running in the right session — leave it.
    KeepCurrent,
    /// No active console session AND no need to keep the stream
    /// alive (no peer connection on this host). Tear down the
    /// worker fully and idle until LOGON.
    Idle,
    /// Spawn a SYSTEM-context worker via the M3 A1 winlogon-token
    /// pipeline targeting the carried session id. The browser's
    /// WebRTC peer stays connected throughout the swap — only the
    /// encoder source + token-identity change underneath.
    ///
    /// The carried `u32` is the target interactive session for
    /// `winlogon.exe` lookup. Today this fires when the active
    /// session disappears mid-stream (full sign-out / fast-user-
    /// switch with controller still connected); the carried id is
    /// the LAST observed active session, which remains the right
    /// target while the supervisor waits for a new logon. Once the
    /// worker reports its lock-state via the heartbeat pipe, this
    /// variant will also fire when the active session is on
    /// `winsta0\Winlogon` (operator on the lock screen), which is
    /// the M3 A1 primary use case.
    SpawnSystemInSession(u32),
}

/// Decide what the supervisor should do given the current world
/// state. Parameters:
///
/// * `active_session` — `WTSGetActiveConsoleSessionId`'s current
///   answer. `None` means no interactive session at all (host on
///   the welcome screen, no users logged in).
/// * `current_worker_session` — session id our existing worker is
///   running in, if any.
/// * `current_is_system_context` — whether the existing worker (if
///   any) was spawned via the SYSTEM-context arm
///   (`SpawnSystemInSession`). False for a normal user-context
///   worker. Used so the swap-on-controller-connect arm doesn't
///   keep flapping back and forth: once we're SystemContext, stay
///   SystemContext while the controller is around.
/// * `keep_stream_alive` — true iff a controller is currently
///   connected to *some* worker on this host (signal comes from
///   `system_context::peer_presence::is_signaled` in the supervisor
///   loop).
/// * `last_active_session` — most recent `Some(active)` the
///   supervisor observed. Used only by the `(None, _) if
///   keep_stream_alive` branch as the target session id for the
///   SYSTEM-context spawn (no current session, but we should keep
///   painting frames anyway because a controller is connected).
///
/// Decision matrix:
///
/// | active | worker | is_sys | alive | -> | reason |
/// |---|---|---|---|---|---|
/// | None | _ | _ | true | SpawnSystemInSession(last) | hold for controller |
/// | None | _ | _ | false | Idle | nobody home |
/// | Some(s) | Some(s) | false | true | SpawnSystemInSession(s) | swap up to SYSTEM |
/// | Some(s) | Some(s) | true | false | SpawnIn(s) | swap back to user |
/// | Some(s) | Some(s) | _ | _ | KeepCurrent | steady state |
/// | Some(s) | _ | _ | true | SpawnSystemInSession(s) | bypass user-mode |
/// | Some(s) | _ | _ | false | SpawnIn(s) | normal cold-start |
///
/// Pure: easy to unit-test.
pub fn decide_spawn(
    active_session: Option<u32>,
    current_worker_session: Option<u32>,
    current_is_system_context: bool,
    keep_stream_alive: bool,
    last_active_session: Option<u32>,
) -> SpawnDecision {
    match (active_session, current_worker_session) {
        (None, _) if keep_stream_alive => match last_active_session {
            Some(s) => SpawnDecision::SpawnSystemInSession(s),
            None => SpawnDecision::Idle,
        },
        (None, _) => SpawnDecision::Idle,
        (Some(active), Some(current)) if active == current => {
            // Same session — decide on swap. If a controller is
            // connected and we're still user-context, swap up. If
            // controller disconnected and we're still SystemContext,
            // swap back down. Otherwise hold.
            if keep_stream_alive && !current_is_system_context {
                SpawnDecision::SpawnSystemInSession(active)
            } else if !keep_stream_alive && current_is_system_context {
                SpawnDecision::SpawnIn(active)
            } else {
                SpawnDecision::KeepCurrent
            }
        }
        (Some(active), _) if keep_stream_alive => {
            // No current worker (or wrong session) AND a controller
            // is connected → bypass the user-context spawn and go
            // straight to SystemContext.
            SpawnDecision::SpawnSystemInSession(active)
        }
        (Some(active), _) => SpawnDecision::SpawnIn(active),
    }
}

/// What the supervisor should do after observing a worker exit.
/// Pure function — no FFI, no logging — so the contract is easy to
/// pin with unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReaction {
    /// Worker exited cleanly (code=0). Respawn immediately and reset
    /// the consecutive-failure counter. Sources of clean exits we
    /// honour: auto-update self-shutdown (M5 #6), instance-lock race
    /// after an SCM restart (M5 #8). Differentiating by exit code
    /// keeps real crashes (non-zero) on the existing backoff ladder.
    Respawn,
    /// Worker exited with a non-zero code. Increment the counter and
    /// wait `Duration` before respawning (exponential backoff).
    Backoff(Duration),
}

/// Cross-feature wrapper for [`crate::system_context::peer_presence::is_signaled`].
/// Always returns `false` on builds without the `system-context`
/// feature — the marker file is never written in that case anyway,
/// but this helper keeps the supervisor's call site free of
/// `#[cfg]` arms.
///
/// Also gated on the `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP` env var. The
/// auto-swap from user-context → SystemContext on every controller
/// connection is more aggressive than the original M3 A1 plan
/// intended (the plan called for swap on lock screen, not on every
/// connection) and the SystemContext spawn path has not yet been
/// field-verified end-to-end. Until both gaps close, the swap is
/// opt-in. Without the env var, the supervisor behaves like 0.2.7:
/// user-context worker always, Z-path overlay covers the lock screen,
/// no SystemContext spawn ever fires.
///
/// Set `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP=1` (or `true`/`yes`/`on`) in
/// the SCM service environment to re-enable.
///
/// **0.3.0-rc.7 semantic change**: when the env var is on, this
/// function unconditionally returns `true` — i.e. the supervisor
/// treats every cycle as if a controller is connected, so
/// `decide_spawn` always picks SystemContext over user-context. The
/// marker file is now an observability tool only (visible via
/// `peer-presence-status`), not a swap gate.
///
/// Why: rc.4 → rc.6 used the marker as a swap gate so the supervisor
/// would swap user→SystemContext only when a controller connected.
/// Field repro PC50045 2026-05-06 showed that the swap window
/// (terminate user-context → spawn SystemContext → caps probe →
/// agent.hello, ~13 s) is LONGER than the browser's auto-reconnect
/// ladder (16 s budget across 6 attempts), so the browser consistently
/// gave up before the SystemContext worker was ready. Net result:
/// every controller-connect attempt killed itself.
///
/// The new "always SystemContext when env var on" semantic eliminates
/// the swap-mid-session race. SystemContext starts at supervisor cold-
/// start (when no worker exists yet), the browser connects to the
/// already-warm SystemContext WS, no swap, no session tear. Tradeoff:
/// the user-context-only data channels (clipboard, file-DC, cursor-DC)
/// don't work in this mode — that's the explicit cost of opting in
/// for admin/lock-screen control.
fn peer_presence_is_signaled() -> bool {
    #[cfg(all(feature = "system-context", target_os = "windows"))]
    {
        // env var on → always treat as connected. No marker check.
        system_swap_enabled()
    }
    #[cfg(not(all(feature = "system-context", target_os = "windows")))]
    {
        false
    }
}

/// Read the `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP` env var.
/// Truthy values: `1` / `true` / `yes` / `on` (case-insensitive).
/// Anything else (including unset) → false.
#[cfg(all(feature = "system-context", target_os = "windows"))]
fn system_swap_enabled() -> bool {
    match std::env::var("ROOMLER_AGENT_ENABLE_SYSTEM_SWAP") {
        Ok(v) => {
            let t = v.trim();
            t.eq_ignore_ascii_case("1")
                || t.eq_ignore_ascii_case("true")
                || t.eq_ignore_ascii_case("yes")
                || t.eq_ignore_ascii_case("on")
        }
        Err(_) => false,
    }
}

/// Decide how to react to a worker exit. Returns the reaction plus
/// the new value for `consecutive_failures`.
pub fn decide_exit_reaction(code: u32, consecutive_failures: u32) -> (ExitReaction, u32) {
    if code == 0 {
        (ExitReaction::Respawn, 0)
    } else {
        let next = consecutive_failures.saturating_add(1);
        (ExitReaction::Backoff(next_backoff(next)), next)
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

    // Log the auto-swap kill-switch state at startup so a "no
    // SystemContext worker ever spawns" investigation lands on the
    // env var first. The swap defaults OFF as of 0.3.0-rc.4 — see
    // `peer_presence_is_signaled` for rationale.
    #[cfg(all(feature = "system-context", target_os = "windows"))]
    {
        let enabled = system_swap_enabled();
        tracing::info!(
            enabled,
            env_var = "ROOMLER_AGENT_ENABLE_SYSTEM_SWAP",
            "supervisor: M3 A1 auto-swap (user-context -> SystemContext) is {}",
            if enabled {
                "ENABLED"
            } else {
                "DISABLED (default)"
            }
        );
    }
    let mut current_worker: Option<OwnedProcess> = None;
    let mut current_session: Option<u32> = None;
    // Last observed keep_stream_alive value. Used to log only on
    // transitions instead of every poll iteration — the supervisor
    // checks the marker every POLL_INTERVAL (500 ms), which would
    // flood the log with identical "keep_stream_alive=true" lines.
    let mut last_logged_keep_stream_alive: Option<bool> = None;
    // Whether the current worker (if any) was spawned via the
    // SYSTEM-context arm (`SpawnSystemInSession`). False on cold
    // start and after every user-context spawn. Drives the
    // `decide_spawn` swap arms — the supervisor needs this state
    // because the worker process itself looks identical from the
    // outside (same binary, same PID handle); only the spawn site
    // knows which token was used.
    let mut current_is_system_context: bool = false;
    let mut consecutive_failures: u32 = 0;
    let mut respawn_at: Option<Instant> = None;
    // Most recent active console session id observed. Used by
    // `decide_spawn`'s SpawnSystemInSession arm when the active
    // session disappears mid-stream (sign-out / fast-user-switch
    // with controller still connected). The supervisor's only
    // session-id memory; current_session can also serve but it's
    // cleared aggressively on worker termination, whereas
    // `last_active_session` is a longer memory of "who was here
    // last".
    let mut last_active_session: Option<u32> = None;

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
                // Wait for the OS to actually reap it before
                // returning — see TERMINATE_WAIT for rationale.
                if !w.wait_for_exit(TERMINATE_WAIT) {
                    tracing::warn!(
                        pid = w.pid,
                        "supervisor: worker did not exit within {}ms after terminate",
                        TERMINATE_WAIT.as_millis()
                    );
                }
            }
            return Ok(());
        }

        // Reap a finished worker.
        if let Some(w) = current_worker.as_ref() {
            match w.try_wait() {
                Ok(Some(code)) => {
                    let (reaction, next_failures) =
                        decide_exit_reaction(code, consecutive_failures);
                    consecutive_failures = next_failures;
                    match reaction {
                        ExitReaction::Respawn => {
                            tracing::info!(
                                pid = w.pid,
                                "supervisor: worker exited cleanly (code=0); respawning without backoff"
                            );
                            respawn_at = None;
                        }
                        ExitReaction::Backoff(backoff) => {
                            tracing::warn!(
                                pid = w.pid,
                                code,
                                consecutive_failures,
                                backoff_secs = backoff.as_secs(),
                                "supervisor: worker exited with non-zero code; backing off before respawn"
                            );
                            respawn_at = Some(Instant::now() + backoff);
                        }
                    }
                    current_worker = None;
                    current_session = None;
                    current_is_system_context = false;
                }
                Ok(None) => { /* still running */ }
                Err(e) => {
                    tracing::warn!(error = %e, "supervisor: try_wait failed; assuming worker is gone");
                    current_worker = None;
                    current_session = None;
                    current_is_system_context = false;
                }
            }
        }

        // Decide whether to spawn. The `keep_stream_alive` argument
        // comes from the `system_context::peer_presence` marker file
        // — true iff the worker has reported a `Connected` peer in
        // the last `PRESENCE_MAX_AGE` (15 s). This is the M3 A1
        // signal that drives the user-context → SYSTEM-context
        // worker swap.
        //
        // Under builds without the `system-context` feature the
        // marker file is never written (the worker's signal hooks
        // are gated to the same feature) so `is_signaled` returns
        // false and `decide_spawn` collapses to its pre-M3
        // behaviour.
        let active = active_console_session_id();
        if let Some(s) = active {
            last_active_session = Some(s);
        }
        let keep_stream_alive = peer_presence_is_signaled();
        if last_logged_keep_stream_alive != Some(keep_stream_alive) {
            // Log on transitions only — at info level so it shows up
            // in production logs without raising the bar to debug.
            // Carry the marker snapshot's metadata so a "no swap
            // happening" investigation lands on a log line that names
            // exactly why the supervisor thinks the controller is
            // (not) there.
            #[cfg(all(feature = "system-context", target_os = "windows"))]
            {
                let snap = crate::system_context::peer_presence::snapshot();
                tracing::info!(
                    keep_stream_alive,
                    marker_path = %snap.path.display(),
                    marker_exists = snap.exists,
                    marker_age_secs = ?snap.age.map(|d| d.as_secs()),
                    marker_error = ?snap.error,
                    "supervisor: peer-presence transition"
                );
            }
            #[cfg(not(all(feature = "system-context", target_os = "windows")))]
            {
                tracing::info!(
                    keep_stream_alive,
                    "supervisor: peer-presence transition (system-context feature off — always false)"
                );
            }
            last_logged_keep_stream_alive = Some(keep_stream_alive);
        }
        let decision = decide_spawn(
            active,
            current_session,
            current_is_system_context,
            keep_stream_alive,
            last_active_session,
        );
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
                                current_is_system_context = false;
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
            // SpawnIn fired but a current worker exists and is in a
            // different session OR is the wrong context (SystemContext
            // when we want user-context now that the controller has
            // disconnected). Either way, kill the existing worker;
            // the next loop iteration spawns the right one.
            (SpawnDecision::SpawnIn(sid), _)
                if current_worker.is_some()
                    && (current_session != Some(sid) || current_is_system_context) =>
            {
                // Active session changed under us — kill the old
                // worker, the next loop iteration will spawn a new
                // one for `sid`.
                if let Some(old) = current_worker.take() {
                    tracing::info!(
                        pid = old.pid,
                        old_session = ?current_session,
                        was_system_context = current_is_system_context,
                        new_session = sid,
                        "supervisor: spawn target changed (session/context); terminating old worker"
                    );
                    old.terminate();
                    // Wait for reap so the next-iteration spawn doesn't
                    // race the instance lock — same rationale as the
                    // shutdown-path wait above.
                    let _ = old.wait_for_exit(TERMINATE_WAIT);
                }
                current_session = None;
                current_is_system_context = false;
            }
            (SpawnDecision::Idle, _) if current_worker.is_some() => {
                // Active console session disappeared (user logged out;
                // host returned to the welcome / lock screen with no
                // logged-in user). The worker is still running but in
                // a now-dead session: every input event it tries to
                // inject returns ERROR_ACCESS_DENIED, every capture
                // call returns a stale frame. Field reproducer at
                // 2026-05-01 (PC50045): logout → flood of "Zugriff
                // verweigert (os error 5)" with the worker still
                // visible to the controller. Terminate eagerly so the
                // controller sees the agent go offline cleanly; M3
                // will fold in SYSTEM-context capture+input here so
                // the lock screen itself becomes controllable.
                if let Some(old) = current_worker.take() {
                    tracing::info!(
                        pid = old.pid,
                        old_session = ?current_session,
                        "supervisor: console session went idle (logout / lock screen); terminating worker"
                    );
                    old.terminate();
                    let _ = old.wait_for_exit(TERMINATE_WAIT);
                }
                current_session = None;
                current_is_system_context = false;
            }
            // M3 A1 SYSTEM-context spawn arm. Fires when decide_spawn
            // returns SpawnSystemInSession(sid) AND we don't already
            // have a worker. Uses the winlogon-token pipeline:
            // find winlogon.exe in `sid`, dup its primary token,
            // CreateProcessAsUserW the agent EXE with that token.
            // The spawned worker probes its own SID at startup
            // (worker_role::probe_self) and selects SystemContext-
            // mode plumbing.
            //
            // Runtime trigger as of 0.3.0: `keep_stream_alive` comes
            // from `peer_presence_is_signaled()` which reads the
            // `%PROGRAMDATA%\roomler-agent\peer-connected.lock`
            // marker file. The user-context worker writes to that
            // file every 5 s while its WebRTC peer is in `Connected`
            // state and removes it on disconnect. So this arm fires
            // when the supervisor sees the marker and there's no
            // current worker (cold start while controller waiting)
            // or the next iteration after the swap-arm killed a
            // user-context worker.
            //
            // Gated behind `system-context` because the winlogon_
            // token module + its windows-sys requirements only
            // compile under that feature. perUser MSI builds (no
            // system-context feature) keep the existing _ => {}
            // catch-all behaviour.
            #[cfg(feature = "system-context")]
            (SpawnDecision::SpawnSystemInSession(sid), true) if current_worker.is_none() => {
                use crate::system_context::winlogon_token;
                let cmdline = winlogon_token::build_cmdline(&worker_exe, &args_borrow);
                let res = (|| -> anyhow::Result<Option<OwnedProcess>> {
                    let Some(pid) = winlogon_token::find_winlogon_pid_in_session(sid)
                        .context("find_winlogon_pid_in_session")?
                    else {
                        return Ok(None);
                    };
                    let token = winlogon_token::open_winlogon_primary_token(pid, sid)
                        .context("open_winlogon_primary_token")?;
                    let child = winlogon_token::spawn_system_in_session(&token, &cmdline)
                        .context("spawn_system_in_session")?;
                    let (process, thread, child_pid, _child_sid) = child.into_raw_parts();
                    Ok(Some(OwnedProcess::from_raw_parts(
                        process, thread, child_pid,
                    )))
                })();
                match res {
                    Ok(Some(p)) => {
                        tracing::info!(
                            pid = p.pid,
                            session_id = sid,
                            "supervisor: spawned SYSTEM-context worker via winlogon-token"
                        );
                        current_worker = Some(p);
                        current_session = Some(sid);
                        current_is_system_context = true;
                        consecutive_failures = 0;
                        respawn_at = None;
                    }
                    Ok(None) => {
                        // No winlogon.exe found in `sid` — rare
                        // logon-transition race or Windows Sandbox /
                        // Hyper-V container. Idle for this iteration;
                        // the next session-change event will retry.
                        tracing::debug!(
                            session_id = sid,
                            "supervisor: no winlogon.exe in session; idling for SYSTEM-context retry"
                        );
                    }
                    Err(e) => {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        let bo = next_backoff(consecutive_failures);
                        tracing::warn!(
                            error = %e,
                            session_id = sid,
                            consecutive_failures,
                            backoff_secs = bo.as_secs(),
                            "supervisor: SYSTEM-context spawn failed; backing off"
                        );
                        respawn_at = Some(Instant::now() + bo);
                    }
                }
            }
            // SystemContext spawn fired but a worker exists in the
            // wrong session OR is the wrong context. Same swap-out
            // logic as the user-context SpawnIn case — kill it, the
            // next iteration spawns the right one.
            #[cfg(feature = "system-context")]
            (SpawnDecision::SpawnSystemInSession(sid), _)
                if current_worker.is_some()
                    && (current_session != Some(sid) || !current_is_system_context) =>
            {
                if let Some(old) = current_worker.take() {
                    tracing::info!(
                        pid = old.pid,
                        old_session = ?current_session,
                        was_system_context = current_is_system_context,
                        new_session = sid,
                        "supervisor: swap to SYSTEM-context worker; terminating old worker"
                    );
                    old.terminate();
                    let _ = old.wait_for_exit(TERMINATE_WAIT);
                }
                current_session = None;
                current_is_system_context = false;
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
    fn decide_spawn_idles_when_no_active_session_and_no_stream() {
        // last_active_session doesn't matter when keep_stream_alive=false;
        // either Some or None must give Idle. current_is_system_context
        // also doesn't matter here.
        assert_eq!(
            decide_spawn(None, None, false, false, None),
            SpawnDecision::Idle
        );
        assert_eq!(
            decide_spawn(None, Some(2), false, false, None),
            SpawnDecision::Idle
        );
        assert_eq!(
            decide_spawn(None, Some(2), false, false, Some(2)),
            SpawnDecision::Idle
        );
    }

    #[test]
    fn decide_spawn_keeps_worker_when_session_unchanged_and_no_swap_needed() {
        // No controller AND user-context worker → KeepCurrent (steady
        // state).
        assert_eq!(
            decide_spawn(Some(2), Some(2), false, false, Some(2)),
            SpawnDecision::KeepCurrent
        );
        // Controller connected AND already SystemContext → KeepCurrent
        // (steady state during active session).
        assert_eq!(
            decide_spawn(Some(2), Some(2), true, true, Some(2)),
            SpawnDecision::KeepCurrent
        );
    }

    #[test]
    fn decide_spawn_swaps_user_to_system_when_controller_arrives() {
        // M3 A1 swap-up: user-context worker is running; controller
        // connects (peer_presence marker fresh). Supervisor should
        // hand control to a SystemContext worker so DXGI capture
        // and SetThreadDesktop input continue working through any
        // upcoming lock-screen transition.
        assert_eq!(
            decide_spawn(Some(2), Some(2), false, true, Some(2)),
            SpawnDecision::SpawnSystemInSession(2)
        );
    }

    #[test]
    fn decide_spawn_swaps_system_to_user_when_controller_leaves() {
        // M3 A1 swap-down: SystemContext worker is running; controller
        // disconnects (marker stale). Supervisor should swap back to
        // user-context so clipboard / file-transfer / cursor data-
        // channels work for the NEXT controller without forcing them
        // to wait for a session change.
        assert_eq!(
            decide_spawn(Some(2), Some(2), true, false, Some(2)),
            SpawnDecision::SpawnIn(2)
        );
    }

    #[test]
    fn decide_spawn_targets_active_session_when_no_worker() {
        assert_eq!(
            decide_spawn(Some(2), None, false, false, None),
            SpawnDecision::SpawnIn(2)
        );
    }

    #[test]
    fn decide_spawn_bypasses_user_mode_when_controller_already_connected() {
        // Cold start with controller already waiting (e.g. supervisor
        // restarted while a session was in flight). Skip the user-
        // context spawn and go straight to SystemContext so the
        // browser's auto-reconnect ladder lands on a usable worker.
        assert_eq!(
            decide_spawn(Some(2), None, false, true, None),
            SpawnDecision::SpawnSystemInSession(2)
        );
    }

    #[test]
    fn decide_spawn_targets_new_session_when_active_changed() {
        // Worker is for session 2 but the active console moved to 5
        // (the previous user logged out, a new one logged in).
        assert_eq!(
            decide_spawn(Some(5), Some(2), false, false, Some(2)),
            SpawnDecision::SpawnIn(5)
        );
    }

    #[test]
    fn decide_spawn_idles_when_session_disappears_without_active_peer() {
        // Field bug PC50045 2026-05-01: user logs out; active session
        // becomes None; the worker is still alive but in a dead
        // session and floods Access Denied. With no active peer
        // connection (keep_stream_alive=false), decide_spawn returns
        // Idle so the supervisor's "idle && current_worker.is_some()"
        // arm tears the worker down.
        assert_eq!(
            decide_spawn(None, Some(2), false, false, Some(2)),
            SpawnDecision::Idle
        );
    }

    #[test]
    fn decide_spawn_returns_system_context_when_session_disappears_with_active_peer() {
        // M3 A1: user logs out / locks while a browser controller is
        // mid-session. Previous behaviour was `Idle`, which tore the
        // peer connection down. New behaviour: hand off to the
        // SYSTEM-context capture+input thread so the stream stays
        // alive. The carried session id is the LAST observed active
        // session (sign-out leaves session-2's winlogon.exe alive
        // through the welcome screen, so spawning into 2 is the
        // right target).
        assert_eq!(
            decide_spawn(None, Some(2), false, true, Some(2)),
            SpawnDecision::SpawnSystemInSession(2)
        );
        // No prior session memory + active disappeared + stream alive
        // → Idle (we have no spawn target). Supervisor in this corner
        // is between SCM start and the first session observation,
        // which is rare enough that Idle is acceptable.
        assert_eq!(
            decide_spawn(None, None, false, true, None),
            SpawnDecision::Idle
        );
        // Same active=None + stream alive, but the supervisor remembers
        // what session it just left → spawn into that.
        assert_eq!(
            decide_spawn(None, None, false, true, Some(7)),
            SpawnDecision::SpawnSystemInSession(7)
        );
    }

    #[test]
    fn decide_exit_reaction_zero_code_resets_counter_and_respawns_now() {
        // Auto-update self-exit (M5 finding #6) and SCM-restart
        // instance-lock race (M5 finding #8) both surface as code=0.
        // Neither is a real failure; the counter must reset to 0 so a
        // legitimate later crash starts the backoff ladder fresh.
        assert_eq!(decide_exit_reaction(0, 0), (ExitReaction::Respawn, 0));
        assert_eq!(
            decide_exit_reaction(0, 5),
            (ExitReaction::Respawn, 0),
            "code=0 must reset the counter even mid-ladder"
        );
    }

    #[test]
    fn decide_exit_reaction_non_zero_code_increments_and_backs_off() {
        // Real crash on a fresh ladder: counter=0 → 1, backoff=2 s.
        assert_eq!(
            decide_exit_reaction(1, 0),
            (ExitReaction::Backoff(Duration::from_secs(2)), 1)
        );
        // Mid-ladder: counter=3 → 4, backoff=16 s.
        assert_eq!(
            decide_exit_reaction(0xC0000005, 3),
            (ExitReaction::Backoff(Duration::from_secs(16)), 4),
            "ACCESS_VIOLATION should keep climbing the ladder"
        );
        // Cap holds: counter=10 → 11, but backoff caps at RESPAWN_BACKOFF_CAP.
        assert_eq!(
            decide_exit_reaction(1, 10),
            (ExitReaction::Backoff(RESPAWN_BACKOFF_CAP), 11)
        );
    }

    #[test]
    fn decide_exit_reaction_saturates_counter() {
        // Runaway counter must not panic on overflow.
        let (_, next) = decide_exit_reaction(1, u32::MAX);
        assert_eq!(next, u32::MAX);
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
