// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Worker-process supervisor (Effort 2 M2).
//!
//! When the SCM-launched service receives a `Running` state and
//! a console session is active, this module spawns the agent's own
//! `roomlerd.exe run` as the active user via
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

use anyhow::{Context, Result, anyhow, bail};
use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_TOKEN, FALSE, GENERIC_READ, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, STILL_ACTIVE, SetHandleInformation, TRUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SECURITY_ATTRIBUTES, SecurityImpersonation,
    TOKEN_ALL_ACCESS, TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN, TokenElevationType,
    TokenElevationTypeLimited, TokenLinkedToken, TokenPrimary,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetExitCodeProcess,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
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

// SAFETY: a Win32 `HANDLE` is a process-wide reference to a kernel object, not
// a thread-affine resource — any thread in the process may use it, and the
// operations performed on these (WaitForSingleObject, GetExitCodeProcess,
// TerminateProcess, ReadFile, CloseHandle) are all thread-safe. The
// thread-affine exceptions in Win32 are GUI objects — window handles bound to a
// message queue, some GDI DCs — and none are ever wrapped here: this holds
// tokens, pipes and process handles.
//
// `Sync` too, because the console-user exec path shares the child process
// handle between a blocking waiter and the async select that may terminate it.
// `Arc` guarantees the single `CloseHandle` on drop runs after every reference
// is gone, so the one non-reentrant operation cannot race.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

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
///
/// `pub(crate)` because roomler SSH's ConPTY path builds its own
/// `CreateProcessAsUserW` call (it must attach a pseudoconsole through an
/// attribute list, which no helper here takes) but still needs the SAME user
/// environment every other console-user spawn gets — a shell started without
/// it has no `PATH`, no `USERPROFILE`, and looks broken to the operator.
pub(crate) struct EnvBlock {
    pub(crate) raw: *mut std::ffi::c_void,
}

impl EnvBlock {
    pub(crate) fn for_token(token: HANDLE) -> Result<Self> {
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
            // CREATE_NO_WINDOW (not CREATE_NEW_CONSOLE): the worker is a
            // background service child — its logs go to the rolling file, not
            // a console. CREATE_NEW_CONSOLE popped a visible console window on
            // the user's desktop on every (re)start / update (field 2026-07-25:
            // "why do I see an Admin PowerShell with logs?"). The interactive
            // desktop attach for GUI (`winsta0\default` in STARTUPINFO above,
            // for the consent tray / MessageBox) is independent of console
            // allocation, so hiding the console keeps those working.
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
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

/// Parse the `ROOMLERD_ELEVATE_WORKER` value. **Default ON**
/// (field-proven on an interactive-admin box, 2026-07-23): the
/// user-context worker is spawned with the interactive admin's ELEVATED
/// linked token so the overlay's Wintun adapter (a privileged device
/// install) can be created. Only an explicit disable value — `0` /
/// `false` / `no` / `off` (case-insensitive) — is the kill-switch;
/// unset / truthy / empty / anything unrecognised keeps the default ON.
///
/// "Default ON" is safe fleet-wide because the elevation is a no-op for
/// anyone who can't use it: [`elevated_primary_token`] only elevates a
/// `TokenElevationTypeLimited` (a UAC-split administrator) and returns
/// `None` for standard users / already-`Full` tokens → the caller spawns
/// with the original token, exactly as before. So flipping the default
/// changes behaviour ONLY on an interactive split-token-admin box whose
/// worker isn't already SYSTEM (i.e. not running SystemContext) — exactly
/// the hosts that need it — and there it also closes the UIPI gap
/// (high-IL input can reach elevated foreground apps). The kill-switch
/// reverts any box to the pre-existing filtered-token behaviour without
/// a rebuild. Pure over its input so it's unit-testable.
fn parse_elevate_flag(val: Option<&str>) -> bool {
    match val {
        Some(v) => {
            let t = v.trim();
            // Explicit kill-switch only; everything else (truthy, empty,
            // or an unrecognised value) keeps the default ON.
            !(t.eq_ignore_ascii_case("0")
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("no")
                || t.eq_ignore_ascii_case("off"))
        }
        None => true,
    }
}

/// Should the supervisor spawn the user-context worker with the
/// interactive admin's ELEVATED linked token?
///
/// **Why this exists:** the overlay's L3 TUN is a Wintun adapter, and
/// `WintunCreateAdapter` is a privileged device install. The default
/// user-context worker runs with the interactive user's UAC-**filtered**
/// (medium-IL) token — even for an administrator — so on a workstation
/// with someone logged in, overlay bring-up fails with
/// `WintunCreateAdapter … "device installation mutex: Access is denied"`
/// and the node never joins the mesh. (Headless / lock-screen hosts are
/// unaffected: their worker already runs as SYSTEM.) The heavyweight
/// SystemContext SYSTEM-swap works around it, but it runs the worker
/// under the LocalSystem profile — wrong `%APPDATA%`/`%USERPROFILE%` for
/// every user-profile lookup. Spawning with the user's *linked elevated*
/// token instead keeps the worker as the **same user** (correct profile)
/// while granting the integrity level Wintun needs — and, as a bonus,
/// lets input injection reach elevated foreground apps (the UIPI gap).
///
/// **Default ON** (field-proven 2026-07-23); disable per-host with
/// `ROOMLERD_ELEVATE_WORKER=0`. The elevation is inert unless the
/// interactive user is a UAC-split administrator, so the default only
/// engages on the hosts that need it — see [`parse_elevate_flag`].
fn worker_elevation_requested() -> bool {
    use tunnel_core::env::node_env;
    parse_elevate_flag(node_env("ELEVATE_WORKER").as_deref())
}

/// `GetTokenInformation(TokenElevationType)` → one of
/// `TokenElevationType{Default,Full,Limited}`. `Default` = UAC off or a
/// standard user (no split token); `Full` = already elevated (built-in
/// Administrator, UAC off + admin, or an already-high-IL caller);
/// `Limited` = a UAC-split administrator running with the filtered token
/// (the only case with a distinct elevated linked token to fetch).
///
/// # Safety
/// `token` must be a valid, live Win32 token handle with `TOKEN_QUERY`
/// access (the WTSQueryUserToken result qualifies).
unsafe fn token_elevation_type(token: HANDLE) -> Result<TOKEN_ELEVATION_TYPE> {
    let mut ty: TOKEN_ELEVATION_TYPE = 0;
    let mut ret_len: u32 = 0;
    // SAFETY: `&mut ty` is a valid out-buffer of exactly the declared
    // size; `&mut ret_len` receives the bytes written. Caller guarantees
    // `token` is a live queryable handle.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevationType,
            &mut ty as *mut TOKEN_ELEVATION_TYPE as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut ret_len,
        )
    };
    if ok == 0 {
        // SAFETY: GetLastError is a thread-local read.
        let err = unsafe { GetLastError() };
        bail!("GetTokenInformation(TokenElevationType) failed: win32 error {err}");
    }
    Ok(ty)
}

/// Resolve the interactive user's ELEVATED primary token, when there is
/// one to resolve. Returns:
/// * `Ok(Some(primary))` — the caller is a UAC-split administrator; the
///   returned owned handle is a **primary** token duplicated from the
///   full linked token, ready for `CreateProcessAsUserW`.
/// * `Ok(None)` — nothing to elevate to (standard user, or the token is
///   already `Full`). The caller spawns with the original token
///   unchanged; no regression.
///
/// The linked token belongs to the SAME logon (hence the same Terminal
/// Services session) as `user_token`, so — unlike the winlogon SYSTEM
/// path — no `SetTokenInformation(TokenSessionId)` re-bind is needed.
///
/// # Safety
/// `user_token` must be a valid, live Win32 user token (typically the
/// `WTSQueryUserToken` result). Handles obtained here are wrapped in
/// [`OwnedHandle`] so every path closes them.
unsafe fn elevated_primary_token(user_token: HANDLE) -> Result<Option<OwnedHandle>> {
    // SAFETY: forwarded contract on `user_token`.
    let etype = unsafe { token_elevation_type(user_token) }?;
    if etype != TokenElevationTypeLimited {
        // Standard user (Default) → nothing to elevate to. Already Full →
        // the caller's token can create Wintun as-is. Either way, spawn
        // with the original token.
        return Ok(None);
    }

    // Fetch the FULL (elevated) linked token of this split-token admin.
    let mut linked = TOKEN_LINKED_TOKEN {
        LinkedToken: std::ptr::null_mut(),
    };
    let mut ret_len: u32 = 0;
    // SAFETY: `&mut linked` is a valid out-buffer of the declared size;
    // the API writes an owned HANDLE into `linked.LinkedToken`.
    let ok = unsafe {
        GetTokenInformation(
            user_token,
            TokenLinkedToken,
            &mut linked as *mut TOKEN_LINKED_TOKEN as *mut c_void,
            std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut ret_len,
        )
    };
    if ok == 0 {
        // SAFETY: GetLastError is a thread-local read.
        let err = unsafe { GetLastError() };
        bail!("GetTokenInformation(TokenLinkedToken) failed: win32 error {err}");
    }
    // Own the linked handle so it's closed even if the dup below fails.
    let linked_owned = OwnedHandle::new(linked.LinkedToken)
        .ok_or_else(|| anyhow!("TokenLinkedToken returned a null/invalid handle"))?;

    // The linked token is an impersonation token; CreateProcessAsUserW
    // needs a PRIMARY token — duplicate it. (Mirrors the winlogon path's
    // DuplicateTokenEx(TokenPrimary).)
    let mut dup: HANDLE = std::ptr::null_mut();
    // SAFETY: `linked_owned.raw()` is a valid token; null attributes are
    // documented-OK; the out-handle is checked + wrapped below.
    let ok = unsafe {
        DuplicateTokenEx(
            linked_owned.raw(),
            TOKEN_ALL_ACCESS,
            std::ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut dup as *mut HANDLE,
        )
    };
    if ok == 0 {
        // SAFETY: GetLastError is a thread-local read.
        let err = unsafe { GetLastError() };
        bail!("DuplicateTokenEx(linked elevated token) failed: win32 error {err}");
    }
    let primary =
        OwnedHandle::new(dup).ok_or_else(|| anyhow!("DuplicateTokenEx returned a null handle"))?;
    Ok(Some(primary))
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
    /// Was `system-context`-gated when the M3 A1 spawn arm was its only
    /// caller. Roomler SSH's console-user path is a second caller in a
    /// different feature, so the gate would have to become an `any(...)`
    /// growing with each new one — and the `dead_code` warning it avoided
    /// cannot fire now: `spawn_in_session_captured` in this module calls it
    /// unconditionally.
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
    /// spawned. M5 finding #8 (the field-test host 2026-05-02): a 145 ms gap
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
/// 145 ms gap on the field-test host and well under any human-perceptible
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
    /// Worker refused to run because it is REDUNDANT — another process
    /// already holds this config's single-instance lock. Stop supervising
    /// and let the service exit cleanly.
    ///
    /// This is the only reaction that does not eventually respawn, and it
    /// needs its own variant precisely because neither existing one is
    /// survivable here:
    ///
    ///   * `Respawn` is what the bug was. The worker loses the lock check
    ///     in ~milliseconds, so "respawn immediately, no backoff" is a hot
    ///     loop with no upper bound.
    ///   * `Backoff` would spin the ladder to its 60 s cap forever and,
    ///     worse, bank a `SupervisorDetected` crash on every iteration —
    ///     fleet crash metrics filling up with a *correct refusal*.
    ///
    /// Retrying cannot help: the other instance is not going away because
    /// we asked. Whoever installed two daemon flavours has to remove one,
    /// and a service that quietly stopped is the signal that says so.
    Stop,
}

/// Cross-feature wrapper for [`crate::system_context::peer_presence::is_signaled`].
/// Always returns `false` on builds without the `system-context`
/// feature — the marker file is never written in that case anyway,
/// but this helper keeps the supervisor's call site free of
/// `#[cfg]` arms.
///
/// Also gated on the `ROOMLERD_ENABLE_SYSTEM_SWAP` env var. The
/// auto-swap from user-context → SystemContext on every controller
/// connection is more aggressive than the original M3 A1 plan
/// intended (the plan called for swap on lock screen, not on every
/// connection) and the SystemContext spawn path has not yet been
/// field-verified end-to-end. Until both gaps close, the swap is
/// opt-in. Without the env var, the supervisor behaves like 0.2.7:
/// user-context worker always, Z-path overlay covers the lock screen,
/// no SystemContext spawn ever fires.
///
/// Set `ROOMLERD_ENABLE_SYSTEM_SWAP=1` (or `true`/`yes`/`on`) in
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
/// Field repro the field-test host 2026-05-06 showed that the swap window
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

/// Read the `ROOMLERD_ENABLE_SYSTEM_SWAP` env var.
/// Truthy values: `1` / `true` / `yes` / `on` (case-insensitive).
/// Anything else (including unset) → false.
#[cfg(all(feature = "system-context", target_os = "windows"))]
fn system_swap_enabled() -> bool {
    use tunnel_core::env::node_env;
    match node_env("ENABLE_SYSTEM_SWAP") {
        Some(v) => {
            let t = v.trim();
            t.eq_ignore_ascii_case("1")
                || t.eq_ignore_ascii_case("true")
                || t.eq_ignore_ascii_case("yes")
                || t.eq_ignore_ascii_case("on")
        }
        None => false,
    }
}

/// W4 — the exit code Windows session teardown delivers to processes in a
/// dying session (`DBG_TERMINATE_PROCESS`, 0x40010004). Field 2026-08-14
/// (winhost-a): logon-screen transitions and Check-Point-VPN session flaps
/// killed the worker with this code, and each death escalated the backoff
/// ladder (a flapping session keeps uptime under the 30 s healthy
/// threshold) — a multi-minute mesh outage manufactured from a
/// seconds-long OS event. Session teardown is the OS's action, not a
/// worker crash; it must neither escalate the ladder nor pollute fleet
/// crash metrics. NOTE: the supervisor's OWN terminate() exits the worker
/// with code 1 and reaps it via `current.take()` BEFORE the ladder ever
/// sees it — 0x40010004 exits observed at the reap site are always
/// external.
pub const SESSION_TEARDOWN_EXIT_CODE: u32 = 0x4001_0004;

/// Decide how to react to a worker exit. Returns the reaction plus
/// the new value for `consecutive_failures`.
pub fn decide_exit_reaction(code: u32, consecutive_failures: u32) -> (ExitReaction, u32) {
    if code == 0 {
        (ExitReaction::Respawn, 0)
    } else if code == crate::watchdog::ALREADY_RUNNING_EXIT_CODE as u32 {
        // The worker is redundant, not broken. Checked BEFORE the generic
        // non-zero arm below, which would otherwise put a correct refusal
        // on the crash ladder. Counter reset because nothing failed.
        (ExitReaction::Stop, 0)
    } else if code == SESSION_TEARDOWN_EXIT_CODE {
        // Counter reset (not a crash) but a 2 s floor, never zero-delay: an
        // external tool spamming this exit code must not create a
        // spawn-kill hot loop.
        (ExitReaction::Backoff(RESPAWN_BACKOFF_INITIAL), 0)
    } else {
        let next = consecutive_failures.saturating_add(1);
        (ExitReaction::Backoff(next_backoff(next)), next)
    }
}

/// Should the supervisor write a crash sidecar for this worker exit?
/// Pure predicate so the call site logic is unit-testable. The
/// supervisor records `SupervisorDetected` crashes for every non-
/// zero exit code EXCEPT:
///
///   * `STALL_EXIT_CODE` — the watchdog already recorded that case
///     before forcing exit; double-recording would inflate fleet
///     crash metrics (B2 from the Task 9 plan critique).
///   * `AGENT_DELETED_EXIT_CODE` (rc.53) — the agent's
///     `handle_server_msg` `ServerMsg::Goodbye` arm already raised a
///     `needs-attention.txt` sentinel; the supervisor's fleet-crash
///     dashboard would treat code-7 exits as silent crashes and
///     mask the operator-action signal.
///   * `SESSION_TEARDOWN_EXIT_CODE` (W4) — Windows killed the session,
///     not the worker; recording every logon/VPN session flap as a
///     `SupervisorDetected` crash buried real crashes in the fleet
///     dashboard (winhost-a logged several per day).
///   * `ALREADY_RUNNING_EXIT_CODE` — the worker declined to start because
///     another instance already holds the lock. That is the single-instance
///     guard working, not a crash; banking it would mean a host with two
///     daemon flavours installed reporting a crash on every supervisor
///     cycle forever.
pub fn should_record_supervisor_crash(code: u32) -> bool {
    code != 0
        && code != crate::watchdog::STALL_EXIT_CODE as u32
        && code != crate::watchdog::AGENT_DELETED_EXIT_CODE as u32
        && code != crate::watchdog::ALREADY_RUNNING_EXIT_CODE as u32
        && code != SESSION_TEARDOWN_EXIT_CODE
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

/// rc.51: a worker must run at least this long before its exit is
/// treated as "it was healthy" — i.e. before a non-zero exit resets
/// the `consecutive_failures` backoff ladder. A doomed worker that
/// crash-loops dies in ~400 ms, far under this; a worker that ran 30 s+
/// and then crashed is a genuine isolated failure that should start a
/// fresh ladder rather than inherit a stale escalated backoff.
///
/// 30 s is comfortably above the observed ~400 ms crash-exit and below
/// `next_backoff`'s 60 s cap, so it gates *backoff escalation* — a
/// faster concern than version-rollback's longer clean-run threshold.
pub const HEALTHY_UPTIME_THRESHOLD: Duration = Duration::from_secs(30);

/// rc.51: once `consecutive_failures` reaches this, the supervisor
/// emits a throttled `error!` so a crash-looping host surfaces in log
/// aggregation. ~8 failures is roughly where `next_backoff` saturates
/// at the 60 s cap. The supervisor never *gives up* (infinite respawn
/// is the M3/M5 resilience design — a give-up would convert a
/// recoverable loop into a permanently-dark host); this is purely an
/// observability signal.
pub const RESPAWN_ALARM_THRESHOLD: u32 = 8;

/// rc.51: should a worker exit reset the `consecutive_failures`
/// ladder? True only when the worker ran long enough to be considered
/// healthy. A successful *spawn* is NOT a healthy run — the doomed
/// crash-loop worker spawns fine then dies in 400 ms — so the reset
/// must gate on uptime, not on spawn success (the pre-rc.51 bug:
/// `consecutive_failures = 0` on every spawn pinned the counter at 1).
pub fn reap_resets_counter(worker_uptime: Duration) -> bool {
    worker_uptime >= HEALTHY_UPTIME_THRESHOLD
}

/// rc.51: the supervisor's live worker handle plus the metadata that
/// must stay in lockstep with it. Pre-rc.51 these were four parallel
/// locals (`current_worker` / `current_session` /
/// `current_is_system_context` / a would-be `spawned_at`) cleared at
/// ~5 separate sites — a desync waiting to happen. Bundling them into
/// one `Option<ActiveWorker>` makes "worker present" and "its
/// metadata" structurally inseparable: a single `.take()` / `= None`
/// clears everything atomically.
struct ActiveWorker {
    /// The spawned worker process handle (RAII — drops the OS handle).
    process: OwnedProcess,
    /// Console session the worker was spawned into.
    session: u32,
    /// `true` iff spawned via the SYSTEM-context winlogon-token arm.
    /// The worker looks identical from outside (same binary, same PID)
    /// — only the spawn site knows which token was used.
    is_system_context: bool,
    /// When the worker was spawned. Drives [`reap_resets_counter`].
    spawned_at: Instant,
}

// ────────────────────────────────────────────────────────────────────────────
// Track A stage 1 — the SECOND supervisor child: `roomlerd netd`.
// ────────────────────────────────────────────────────────────────────────────

/// Truthiness for the `ROOMLERD_OVERLAY_NETD` env lever (pure; tested).
fn netd_flag_truthy(v: &str) -> bool {
    let t = v.trim();
    t.eq_ignore_ascii_case("1")
        || t.eq_ignore_ascii_case("true")
        || t.eq_ignore_ascii_case("yes")
        || t.eq_ignore_ascii_case("on")
}

/// Track A stage 1 — is the netd scaffold enabled? Env lever first
/// (`ROOMLERD_OVERLAY_NETD`), else the MACHINE-GLOBAL config's
/// `overlay_netd` (the supervisor is LocalSystem; per-user enrollments
/// keep today's single-child topology until a later stage). Read ONCE at
/// service start — changing it needs a service restart, by design: the
/// two-child topology must never flip mid-run.
fn netd_enabled() -> bool {
    if let Some(v) = tunnel_core::env::node_env("OVERLAY_NETD") {
        return netd_flag_truthy(&v);
    }
    // FR-66: `read_if_present`, NOT `load`. This path is legitimately absent on
    // every user-context install (the worker runs in session 1 and keeps its
    // config under `%APPDATA%`), and `load` announces a missing config as
    // `the host must be re-enrolled` at ERROR — so probing an optional flag
    // through it told every healthy host to re-enroll on every service start.
    // Behaviour here is unchanged: absent still means the flag is off.
    let p = crate::config::machine_global_config_path();
    crate::config::read_if_present(&p)
        .and_then(|c| c.overlay_netd)
        .unwrap_or(false)
}

/// The netd child: session 0 (a plain std spawn inherits the supervisor's
/// own LocalSystem session), with its own respawn ladder — a netd crash
/// must never touch the session worker's ladder, and vice versa.
///
/// SCAFFOLD ONLY: netd hosts nothing yet. This exists so the two-child
/// supervisor machinery (spawn / ladder / healthy-uptime reset / shutdown
/// ordering) soaks in the field before the network plane moves into it
/// (`docs/overlay-session-proof.md` §6 stage 1).
struct NetdChild {
    child: Option<std::process::Child>,
    spawned_at: Option<Instant>,
    consecutive_failures: u32,
    respawn_at: Option<Instant>,
}

impl NetdChild {
    fn new() -> Self {
        Self {
            child: None,
            spawned_at: None,
            consecutive_failures: 0,
            respawn_at: None,
        }
    }

    /// One poll-loop tick: reap a dead netd (ladder, with the same
    /// healthy-uptime reset rule as the worker), (re)spawn when due.
    fn tick(&mut self, exe: &Path) {
        if let Some(c) = self.child.as_mut() {
            match c.try_wait() {
                Ok(None) => return, // healthy
                Ok(Some(status)) => {
                    let uptime = self
                        .spawned_at
                        .map(|t| t.elapsed())
                        .unwrap_or(Duration::ZERO);
                    self.consecutive_failures = if uptime >= HEALTHY_UPTIME_THRESHOLD {
                        1
                    } else {
                        self.consecutive_failures.saturating_add(1)
                    };
                    tracing::warn!(
                        code = status.code().unwrap_or(-1),
                        uptime_s = uptime.as_secs(),
                        failures = self.consecutive_failures,
                        "supervisor: netd exited; ladder respawn"
                    );
                    self.respawn_at =
                        Some(Instant::now() + next_backoff(self.consecutive_failures));
                    self.child = None;
                    self.spawned_at = None;
                }
                Err(e) => {
                    tracing::warn!(%e, "supervisor: netd try_wait failed");
                    return;
                }
            }
        }
        if let Some(at) = self.respawn_at
            && Instant::now() < at
        {
            return;
        }
        self.respawn_at = None;
        match std::process::Command::new(exe).arg("netd").spawn() {
            Ok(c) => {
                tracing::info!(
                    pid = c.id(),
                    "supervisor: netd child spawned (Track A scaffold)"
                );
                self.child = Some(c);
                self.spawned_at = Some(Instant::now());
            }
            Err(e) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.respawn_at = Some(Instant::now() + next_backoff(self.consecutive_failures));
                tracing::warn!(%e, failures = self.consecutive_failures,
                    "supervisor: netd spawn failed; backing off");
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(mut c) = self.child.take() {
            tracing::info!(pid = c.id(), "supervisor: terminating netd on shutdown");
            let _ = c.kill();
            let _ = c.wait();
        }
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
            env_var = "ROOMLERD_ENABLE_SYSTEM_SWAP",
            "supervisor: M3 A1 auto-swap (user-context -> SystemContext) is {}",
            if enabled {
                "ENABLED"
            } else {
                "DISABLED (default)"
            }
        );
    }
    // rc.51: the worker handle + its session/context/spawn-time, one
    // Option so they can never desync (see `ActiveWorker`).
    let mut current: Option<ActiveWorker> = None;
    // Track A stage 1 — the OPTIONAL second child (`roomlerd netd`),
    // gated on `overlay_netd` at service start. `None` = today's
    // single-child topology, bit-for-bit.
    let mut netd: Option<NetdChild> = netd_enabled().then(NetdChild::new);
    if netd.is_some() {
        tracing::info!(
            "supervisor: Track A netd scaffold ENABLED — running the two-child topology"
        );
    }
    // Last observed keep_stream_alive value. Used to log only on
    // transitions instead of every poll iteration — the supervisor
    // checks the marker every POLL_INTERVAL (500 ms), which would
    // flood the log with identical "keep_stream_alive=true" lines.
    let mut last_logged_keep_stream_alive: Option<bool> = None;
    let mut consecutive_failures: u32 = 0;
    let mut respawn_at: Option<Instant> = None;
    // rc.51: throttle for the crash-loop alarm `error!` — emit at most
    // once per minute so a looping host surfaces in log aggregation
    // without flooding it.
    let mut last_alarm_at: Option<Instant> = None;
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
                    // rc.51: a session change (logon / unlock /
                    // fast-user-switch) is exactly the signal that a
                    // previously-doomed pre-logon spawn may now
                    // succeed — clear any pending backoff so the next
                    // loop iteration spawns immediately instead of
                    // waiting out a stale (up to 60 s) `respawn_at`.
                    respawn_at = None;
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
            if let Some(w) = current.take() {
                tracing::info!(
                    pid = w.process.pid,
                    "supervisor: terminating worker on shutdown"
                );
                w.process.terminate();
                // Wait for the OS to actually reap it before
                // returning — see TERMINATE_WAIT for rationale.
                if !w.process.wait_for_exit(TERMINATE_WAIT) {
                    tracing::warn!(
                        pid = w.process.pid,
                        "supervisor: worker did not exit within {}ms after terminate",
                        TERMINATE_WAIT.as_millis()
                    );
                }
            }
            // Track A — netd goes down LAST (once it hosts the network
            // plane, the worker must lose its transport only after it has
            // been told to stop; the scaffold already exercises the order).
            if let Some(n) = netd.as_mut() {
                n.shutdown();
            }
            return Ok(());
        }

        // Track A stage 1 — the netd child's own reap/ladder/respawn,
        // fully independent of the session worker's.
        if let Some(n) = netd.as_mut() {
            n.tick(&worker_exe);
        }

        // Reap a finished worker.
        if let Some(w) = current.as_ref() {
            match w.process.try_wait() {
                Ok(Some(code)) => {
                    // rc.51: gate the counter reset on worker UPTIME,
                    // not spawn success. A worker that ran ≥30 s then
                    // crashed is an isolated failure → fresh ladder
                    // (pass prior=0); a worker that died in <30 s is a
                    // crash-loop iteration → keep escalating (pass the
                    // live counter). `decide_exit_reaction(0, _)`
                    // always resets, so a clean exit is unaffected.
                    let uptime = w.spawned_at.elapsed();
                    let prior = if reap_resets_counter(uptime) {
                        0
                    } else {
                        consecutive_failures
                    };
                    let (reaction, next_failures) = decide_exit_reaction(code, prior);
                    consecutive_failures = next_failures;
                    match reaction {
                        ExitReaction::Respawn => {
                            tracing::info!(
                                pid = w.process.pid,
                                "supervisor: worker exited cleanly (code=0); respawning without backoff"
                            );
                            respawn_at = None;
                        }
                        ExitReaction::Stop => {
                            // At ERROR, not WARN: this needs an operator, and
                            // it is the only thing that will resolve it. The
                            // service stops cleanly rather than idling —
                            // "Running" while supervising nothing is the
                            // green-but-dead reading that hides the problem.
                            //
                            // A clean stop does NOT trigger SCM recovery
                            // actions; those fire on unexpected termination.
                            tracing::error!(
                                pid = w.process.pid,
                                code,
                                "supervisor: another roomler daemon already holds this config's \
                                 single-instance lock — this one is redundant. Stopping instead of \
                                 respawning. Uninstall the duplicate daemon flavour (perMachine \
                                 service vs perUser task) to clear this."
                            );
                            // No `terminate()` here, unlike the shutdown path
                            // above: we are in the REAP arm, so the worker has
                            // already exited. netd is a separate child and is
                            // still running, so it does need stopping.
                            if let Some(n) = netd.as_mut() {
                                n.shutdown();
                            }
                            return Ok(());
                        }
                        ExitReaction::Backoff(backoff) => {
                            tracing::warn!(
                                pid = w.process.pid,
                                code,
                                consecutive_failures,
                                backoff_secs = backoff.as_secs(),
                                "supervisor: worker exited with non-zero code; backing off before respawn"
                            );
                            respawn_at = Some(Instant::now() + backoff);

                            // rc.51 + rc.53: crash-loop / operator-action alarm.
                            //
                            // Two alarm gates feed the same throttled `error!`:
                            //
                            //   * rc.53 fast-path: code == AGENT_DELETED_EXIT_CODE
                            //     fires on the FIRST failure (not after 8) so the
                            //     "server-side row deleted → re-enrol required"
                            //     signal is visible in <1 minute. Without this
                            //     fast-path the alarm waits ~4 min (8 × ~30 s avg
                            //     backoff) while the operator is staring at the
                            //     wedge wondering why the host won't connect.
                            //
                            //   * rc.51 generic crash-loop: consecutive_failures
                            //     ≥ RESPAWN_ALARM_THRESHOLD (~ where backoff
                            //     saturates at the 60 s cap). Any non-zero exit
                            //     code; covers the boot-loop case where there is
                            //     no Goodbye on the wire.
                            //
                            // The supervisor NEVER gives up — infinite respawn is
                            // intentional (a re-enrol or auto-update can still
                            // recover the host); these alarms are purely
                            // observability. 60 s throttle keeps log aggregation
                            // sane during a sustained loop.
                            let is_agent_deleted_exit =
                                code == crate::watchdog::AGENT_DELETED_EXIT_CODE as u32;
                            let alarm_due = is_agent_deleted_exit
                                || consecutive_failures >= RESPAWN_ALARM_THRESHOLD;
                            if alarm_due {
                                let now = Instant::now();
                                let throttle_passed = last_alarm_at.is_none_or(|t| {
                                    now.duration_since(t) >= Duration::from_secs(60)
                                });
                                if throttle_passed {
                                    last_alarm_at = Some(now);
                                    if is_agent_deleted_exit {
                                        tracing::error!(
                                            last_exit_code = code,
                                            "supervisor: worker exited with AGENT_DELETED_EXIT_CODE — server-side row was deleted or policy-rejected; operator action required (re-enrol with fresh token). Supervisor will keep respawning; expect successive code-{} exits until re-enrollment.",
                                            crate::watchdog::AGENT_DELETED_EXIT_CODE
                                        );
                                    } else {
                                        tracing::error!(
                                            consecutive_failures,
                                            last_exit_code = code,
                                            "supervisor: worker has failed {} times in a row — host likely needs operator attention (still respawning)",
                                            consecutive_failures
                                        );
                                    }
                                }
                            }

                            // Phase 1B (Task 9): record a crash
                            // sidecar so the next worker uploads it
                            // to roomler.ai. The predicate explicitly
                            // excludes STALL_EXIT_CODE — the watchdog
                            // already recorded that case before
                            // forcing exit; double-recording would
                            // inflate fleet crash metrics (B2 from
                            // the plan critique). Locked by
                            // `should_record_supervisor_crash_*`
                            // tests in this module.
                            if should_record_supervisor_crash(code) {
                                crate::crash_recorder::record(
                                    crate::crash_recorder::Reason::SupervisorDetected,
                                    &format!("worker exit code {code}"),
                                    crate::crash_recorder::WriterContext::Supervisor,
                                );
                            }
                        }
                    }
                    current = None;
                }
                Ok(None) => { /* still running */ }
                Err(e) => {
                    tracing::warn!(error = %e, "supervisor: try_wait failed; assuming worker is gone");
                    current = None;
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
            current.as_ref().map(|w| w.session),
            current.as_ref().is_some_and(|w| w.is_system_context),
            keep_stream_alive,
            last_active_session,
        );
        let due_for_respawn = respawn_at.is_none_or(|t| Instant::now() >= t);

        match (decision, due_for_respawn) {
            (SpawnDecision::SpawnIn(sid), true) if current.is_none() => {
                match query_user_token(sid) {
                    Ok(Some(token)) => {
                        // rc.206 — spawn with the interactive admin's
                        // ELEVATED linked token so the overlay's Wintun
                        // adapter (a privileged device install) succeeds
                        // without the heavier SystemContext SYSTEM-swap.
                        // Default-on; kill-switch ROOMLERD_ELEVATE_
                        // WORKER=0. Falls back to the filtered `token` for
                        // standard users / already-elevated / any failure
                        // (no regression). `elevated` (when Some) owns the
                        // duplicated primary token and MUST outlive the
                        // spawn call below — hence the outer binding.
                        let elevated = if worker_elevation_requested() {
                            // SAFETY: `token.raw()` is a live user token
                            // from WTSQueryUserToken, valid for this scope.
                            match unsafe { elevated_primary_token(token.raw()) } {
                                Ok(t) => t,
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        session_id = sid,
                                        "supervisor: elevated-token resolution failed; \
                                         spawning with the standard user token"
                                    );
                                    None
                                }
                            }
                        } else {
                            None
                        };
                        // Prefer the elevated token when we resolved one.
                        let (spawn_handle, is_elevated) = match &elevated {
                            Some(t) => (t.raw(), true),
                            None => (token.raw(), false),
                        };
                        // SAFETY: `spawn_handle` is a live primary/user
                        // token — either the WTSQueryUserToken result or
                        // its duplicated elevated linked token; both
                        // OwnedHandles outlive this call. CreateProcessAsUserW
                        // duplicates any handles it needs.
                        match unsafe { spawn_in_session(spawn_handle, &worker_exe, &args_borrow) } {
                            Ok(p) => {
                                tracing::info!(
                                    pid = p.pid,
                                    session_id = sid,
                                    elevated = is_elevated,
                                    "supervisor: spawned worker"
                                );
                                current = Some(ActiveWorker {
                                    process: p,
                                    session: sid,
                                    is_system_context: false,
                                    spawned_at: Instant::now(),
                                });
                                // rc.51: do NOT reset consecutive_failures
                                // here — a successful SPAWN is not a healthy
                                // RUN. The reset is now uptime-gated in the
                                // reap path (see reap_resets_counter). The
                                // pre-rc.51 reset-on-spawn pinned the counter
                                // at 1 and made backoff never escalate.
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
                if current
                    .as_ref()
                    .is_some_and(|w| w.session != sid || w.is_system_context) =>
            {
                // Active session changed under us — kill the old
                // worker, the next loop iteration will spawn a new
                // one for `sid`.
                if let Some(old) = current.take() {
                    tracing::info!(
                        pid = old.process.pid,
                        old_session = old.session,
                        was_system_context = old.is_system_context,
                        new_session = sid,
                        "supervisor: spawn target changed (session/context); terminating old worker"
                    );
                    old.process.terminate();
                    // Wait for reap so the next-iteration spawn doesn't
                    // race the instance lock — same rationale as the
                    // shutdown-path wait above.
                    let _ = old.process.wait_for_exit(TERMINATE_WAIT);
                }
            }
            (SpawnDecision::Idle, _) if current.is_some() => {
                // Active console session disappeared (user logged out;
                // host returned to the welcome / lock screen with no
                // logged-in user). The worker is still running but in
                // a now-dead session: every input event it tries to
                // inject returns ERROR_ACCESS_DENIED, every capture
                // call returns a stale frame. Field reproducer at
                // 2026-05-01 (the field-test host): logout → flood of "Zugriff
                // verweigert (os error 5)" with the worker still
                // visible to the controller. Terminate eagerly so the
                // controller sees the agent go offline cleanly; M3
                // will fold in SYSTEM-context capture+input here so
                // the lock screen itself becomes controllable.
                if let Some(old) = current.take() {
                    tracing::info!(
                        pid = old.process.pid,
                        old_session = old.session,
                        "supervisor: console session went idle (logout / lock screen); terminating worker"
                    );
                    old.process.terminate();
                    let _ = old.process.wait_for_exit(TERMINATE_WAIT);
                }
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
            // RETIRED-NAME-ANCHOR(3): names the PRE-RENAME appdirs segment a host
            // installed before P4b still has; appdirs::app_segment resolves it, so it
            // is an input.
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
            (SpawnDecision::SpawnSystemInSession(sid), true) if current.is_none() => {
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
                        current = Some(ActiveWorker {
                            process: p,
                            session: sid,
                            is_system_context: true,
                            spawned_at: Instant::now(),
                        });
                        // rc.51: no consecutive_failures reset here —
                        // the reset is uptime-gated in the reap path.
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
                if current
                    .as_ref()
                    .is_some_and(|w| w.session != sid || !w.is_system_context) =>
            {
                if let Some(old) = current.take() {
                    tracing::info!(
                        pid = old.process.pid,
                        old_session = old.session,
                        was_system_context = old.is_system_context,
                        new_session = sid,
                        "supervisor: swap to SYSTEM-context worker; terminating old worker"
                    );
                    old.process.terminate();
                    let _ = old.process.wait_for_exit(TERMINATE_WAIT);
                }
            }
            _ => {}
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Output-capturing spawn (roomler SSH P5b)
// ───────────────────────────────────────────────────────────────────────────

/// A child spawned into a user session with its output piped back.
///
/// The read ends are plain blocking pipe handles; drain them with
/// [`read_pipe_to_end`], one thread each. Both must be drained or a child that
/// fills the other pipe's 4 KiB buffer blocks forever — that is the classic
/// two-pipe deadlock, and it is why the caller cannot simply read stdout first.
pub struct CapturedChild {
    pub process: OwnedProcess,
    pub stdout: OwnedHandle,
    pub stderr: OwnedHandle,
}

/// Output-capturing twin of [`spawn_in_session`], for running a SHELL COMMAND
/// LINE as the signed-in console user and reading back what it printed.
///
/// [`spawn_in_session`] launches our own EXE and ignores its output; roomler
/// SSH needs the opposite, so this takes a full command line (no
/// `lpApplicationName`) and wires three pipes.
///
/// # Why the handle flags are the way they are
///
/// Two mistakes here produce a hang rather than an error, so both are spelled
/// out:
///
/// * The pipe WRITE ends must be inheritable and the READ ends must not. If a
///   read end leaks into the child, the pipe never reaches EOF after the child
///   exits — because the child still holds a writer's counterpart open — and
///   the reader blocks forever.
/// * The parent must close ITS copies of the write ends immediately after the
///   spawn. Same failure: as long as this process holds a writer, `ReadFile`
///   on the read end never returns 0.
///
/// # Safety
///
/// `token` must be a live primary token for the target session (from
/// [`query_user_token`] + [`duplicate_primary`]). The caller keeps it alive
/// across the call.
pub unsafe fn spawn_in_session_captured(
    token: HANDLE,
    cmdline: &str,
    cwd: Option<&Path>,
) -> Result<CapturedChild> {
    let env = EnvBlock::for_token(token).context("CreateEnvironmentBlock")?;

    // Inheritable by default — each read end is explicitly de-inherited below.
    let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
    sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
    sa.bInheritHandle = TRUE;
    sa.lpSecurityDescriptor = std::ptr::null_mut();

    let (out_r, out_w) = unsafe { make_pipe(&mut sa) }.context("CreatePipe (stdout)")?;
    let (err_r, err_w) = unsafe { make_pipe(&mut sa) }.context("CreatePipe (stderr)")?;

    // stdin from NUL rather than a null handle: with STARTF_USESTDHANDLES the
    // child gets exactly these three, and a shell handed an invalid stdin can
    // fail in ways that look like the command misbehaving.
    let nul_w = encode_wide(OsStr::new("NUL"));
    // SAFETY: documented CreateFileW call; the wide string outlives it.
    let nul = unsafe {
        CreateFileW(
            nul_w.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &sa,
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    let nul = OwnedHandle::new(nul).ok_or_else(|| anyhow!("opening NUL for stdin failed"))?;

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = nul.raw();
    si.hStdOutput = out_w.raw();
    si.hStdError = err_w.raw();
    // Same interactive-desktop attachment as `spawn_in_session`: without it the
    // child lands on a noninteractive desktop, which matters for anything the
    // user's shell profile might touch.
    let mut desktop_w = encode_wide(OsStr::new("winsta0\\default"));
    si.lpDesktop = desktop_w.as_mut_ptr();

    let mut cmdline_w = encode_wide(OsStr::new(cmdline));
    let cwd_w = cwd.map(|p| encode_wide(p.as_os_str()));
    let cwd_ptr = cwd_w
        .as_ref()
        .map(|w| w.as_ptr())
        .unwrap_or(std::ptr::null());

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // SAFETY: every buffer outlives the call; out-params are valid; the flags
    // and the TRUE inherit-handles argument are exactly what the pipe wiring
    // above requires.
    let ok = unsafe {
        CreateProcessAsUserW(
            token,
            std::ptr::null(), // no lpApplicationName — cmdline carries the shell
            cmdline_w.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            TRUE, // inherit handles — required for the pipes to reach the child
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            env.raw,
            cwd_ptr,
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        // SAFETY: thread-local error read.
        let err = unsafe { GetLastError() };
        bail!("CreateProcessAsUserW failed (err {err})");
    }

    // Drop OUR write ends and the NUL handle NOW. While this process holds a
    // writer, the corresponding read end never sees EOF and the drain threads
    // below would block until the timeout regardless of the child exiting.
    drop(out_w);
    drop(err_w);
    drop(nul);

    Ok(CapturedChild {
        process: OwnedProcess::from_raw_parts(pi.hProcess, pi.hThread, pi.dwProcessId),
        stdout: out_r,
        stderr: err_r,
    })
}

/// One anonymous pipe: `(read, write)`, with the read end de-inherited.
///
/// # Safety
/// `sa` must be a valid, initialised `SECURITY_ATTRIBUTES`.
unsafe fn make_pipe(sa: &mut SECURITY_ATTRIBUTES) -> Result<(OwnedHandle, OwnedHandle)> {
    let mut r: HANDLE = std::ptr::null_mut();
    let mut w: HANDLE = std::ptr::null_mut();
    // SAFETY: out-params are valid; `sa` is caller-validated.
    let ok = unsafe { CreatePipe(&mut r, &mut w, sa, 0) };
    if ok == 0 {
        // SAFETY: thread-local error read.
        let err = unsafe { GetLastError() };
        bail!("CreatePipe failed (err {err})");
    }
    let r = OwnedHandle::new(r).ok_or_else(|| anyhow!("CreatePipe returned a null read handle"))?;
    let w =
        OwnedHandle::new(w).ok_or_else(|| anyhow!("CreatePipe returned a null write handle"))?;
    // The child must NOT inherit the read end — see the deadlock note on
    // `spawn_in_session_captured`.
    // SAFETY: `r` is a live handle we own.
    let ok = unsafe { SetHandleInformation(r.raw(), HANDLE_FLAG_INHERIT, 0) };
    if ok == 0 {
        // SAFETY: thread-local error read.
        let err = unsafe { GetLastError() };
        bail!("SetHandleInformation on the pipe read end failed (err {err})");
    }
    Ok((r, w))
}

/// Drain a pipe to EOF, stopping early once `budget` is exhausted.
///
/// Blocking by design — the caller runs one of these per stream on a blocking
/// thread. Returns the bytes read and whether the budget cut it short, so the
/// caller can report truncation the same way the ordinary exec path does.
///
/// `budget` is shared across both streams: the wire promises a COMBINED
/// ceiling, so two independent caps would silently double it.
pub fn read_pipe_to_end(
    pipe: &OwnedHandle,
    budget: &std::sync::atomic::AtomicU64,
) -> (Vec<u8>, bool) {
    use std::sync::atomic::Ordering;

    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        let mut read: u32 = 0;
        // SAFETY: `pipe` is a live read handle we own; `buf`/`read` are valid
        // out-params for the length passed.
        let ok = unsafe {
            ReadFile(
                pipe.raw(),
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            // ERROR_BROKEN_PIPE is the normal end: the last writer closed.
            break;
        }
        let n = read as usize;
        let allowed = budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
            Some(b.saturating_sub(n as u64))
        });
        let remaining = allowed.unwrap_or(0);
        if remaining == 0 {
            truncated = true;
            break;
        }
        let take = n.min(remaining as usize);
        out.extend_from_slice(&buf[..take]);
        if take < n {
            truncated = true;
            break;
        }
    }
    (out, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Track A stage 1 — the env lever's truthiness (the config path is
    /// exercised in the field; this pins the emergency override parse).
    #[test]
    fn netd_env_lever_truthiness() {
        for v in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(netd_flag_truthy(v), "{v:?} must enable");
        }
        for v in ["0", "false", "off", "", "banana"] {
            assert!(!netd_flag_truthy(v), "{v:?} must disable");
        }
    }

    #[test]
    fn elevate_flag_defaults_on_with_explicit_kill_switch() {
        // Default ON: unset, truthy, empty, or any unrecognised value all
        // keep elevation enabled (the elevation itself is still inert for
        // non-split-token users — see elevated_primary_token).
        for v in [
            None,
            Some("1"),
            Some("true"),
            Some("on"),
            Some("Yes"),
            Some(""),
            Some("  "),
            Some("2"),
            Some("enabled"),
        ] {
            assert!(parse_elevate_flag(v), "{v:?} should keep elevation ON");
        }
        // Only an explicit disable value (case-insensitive, whitespace-
        // tolerant) is the kill-switch → OFF.
        for v in ["0", "false", "FALSE", "No", "off", " off ", "\tfalse\n"] {
            assert!(
                !parse_elevate_flag(Some(v)),
                "{v:?} must disable elevation (kill-switch)"
            );
        }
    }

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
        // Field bug the field-test host 2026-05-01: user logs out; active session
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

    /// The bug this fixes: a redundant worker loses the instance-lock check
    /// in milliseconds, so mapping its exit to `Respawn` (no backoff) is an
    /// unbounded hot loop, and mapping it to `Backoff` would climb the crash
    /// ladder forever for a correct refusal. It must STOP.
    #[test]
    fn already_running_stops_the_supervisor() {
        let code = crate::watchdog::ALREADY_RUNNING_EXIT_CODE as u32;
        assert_eq!(
            decide_exit_reaction(code, 0),
            (ExitReaction::Stop, 0),
            "a redundant worker must stop the supervisor, never respawn"
        );
        // Mid-ladder too: arriving with failures banked must not turn this
        // into a Backoff, and must reset the counter — nothing failed.
        assert_eq!(
            decide_exit_reaction(code, 7),
            (ExitReaction::Stop, 0),
            "the reaction must not depend on the ladder state"
        );
    }

    /// A correct refusal is not a crash. Without this, a host with two
    /// daemon flavours installed would bank a `SupervisorDetected` crash on
    /// every supervisor cycle and bury real crashes in the fleet dashboard —
    /// the same failure `SESSION_TEARDOWN_EXIT_CODE` was excluded for.
    #[test]
    fn already_running_is_not_recorded_as_a_crash() {
        assert!(!should_record_supervisor_crash(
            crate::watchdog::ALREADY_RUNNING_EXIT_CODE as u32
        ));
        // Guard the guard: an ordinary non-zero code still records, so this
        // exclusion cannot silently widen.
        assert!(should_record_supervisor_crash(1));
    }

    /// The sentinel is shared with the Linux units, which list it in
    /// `RestartPreventExitStatus` (asserted in `watchdog`'s tests). Nothing
    /// couples the three sites, so pin the value here as well — renumbering
    /// it restores the respawn loop on BOTH platforms, silently.
    #[test]
    fn the_already_running_sentinel_is_pinned() {
        assert_eq!(crate::watchdog::ALREADY_RUNNING_EXIT_CODE, 8);
        // It must not collide with any other code the supervisor special-cases,
        // or one meaning would shadow the other.
        for (other, name) in [
            (crate::watchdog::STALL_EXIT_CODE, "STALL"),
            (crate::watchdog::AGENT_DELETED_EXIT_CODE, "AGENT_DELETED"),
        ] {
            assert_ne!(
                crate::watchdog::ALREADY_RUNNING_EXIT_CODE,
                other,
                "ALREADY_RUNNING collides with {name}"
            );
        }
        assert_ne!(
            crate::watchdog::ALREADY_RUNNING_EXIT_CODE as u32,
            SESSION_TEARDOWN_EXIT_CODE
        );
    }

    #[test]
    fn decide_exit_reaction_saturates_counter() {
        // Runaway counter must not panic on overflow.
        let (_, next) = decide_exit_reaction(1, u32::MAX);
        assert_eq!(next, u32::MAX);
    }

    /// W4 — Windows session teardown (0x40010004) is the OS's action, not a
    /// crash: counter RESETS (a logon/VPN flap storm must not climb toward
    /// the 60 s cap — the winhost-a multi-minute-outage class) but the respawn
    /// keeps a 2 s floor so a spoofed exit code can't create a hot loop.
    /// And it never pollutes fleet crash metrics.
    #[test]
    fn session_teardown_resets_the_ladder_with_a_floor_and_records_no_crash() {
        assert_eq!(
            decide_exit_reaction(SESSION_TEARDOWN_EXIT_CODE, 6),
            (ExitReaction::Backoff(RESPAWN_BACKOFF_INITIAL), 0),
            "mid-ladder session teardown must reset the counter"
        );
        assert_eq!(
            decide_exit_reaction(SESSION_TEARDOWN_EXIT_CODE, 0),
            (ExitReaction::Backoff(RESPAWN_BACKOFF_INITIAL), 0)
        );
        assert!(!should_record_supervisor_crash(SESSION_TEARDOWN_EXIT_CODE));
        // A real crash right AFTER a teardown starts a FRESH ladder (2 s).
        assert_eq!(
            decide_exit_reaction(1, 0),
            (ExitReaction::Backoff(Duration::from_secs(2)), 1)
        );
    }

    // ─── B2 regression: supervisor crash-record exclusion ────────────

    #[test]
    fn should_record_supervisor_crash_excludes_stall_exit_code() {
        // The watchdog records WatchdogStall before forcing exit with
        // this code; the supervisor must NOT double-record. Locks
        // B2 from the Task 9 plan critique.
        assert!(!should_record_supervisor_crash(
            crate::watchdog::STALL_EXIT_CODE as u32
        ));
    }

    #[test]
    fn should_record_supervisor_crash_excludes_clean_exit() {
        // Code 0 = clean shutdown; the supervisor's Respawn branch
        // (not Backoff) handles it, but the predicate must still
        // return false defensively.
        assert!(!should_record_supervisor_crash(0));
    }

    #[test]
    fn should_record_supervisor_crash_records_other_non_zero_exits() {
        // Real crash codes — segfault on Windows (0xC0000005),
        // SIGABRT from a panic propagated through tokio runtime (134),
        // generic 1. All should fire the recorder.
        assert!(should_record_supervisor_crash(0xC0000005));
        assert!(should_record_supervisor_crash(134));
        assert!(should_record_supervisor_crash(1));
        assert!(should_record_supervisor_crash(255));
    }

    #[test]
    fn should_record_supervisor_crash_excludes_agent_deleted_exit_code() {
        // rc.53: the agent's `handle_server_msg` `ServerMsg::Goodbye`
        // arm already raised a `needs-attention.txt` sentinel before
        // `process::exit(AGENT_DELETED_EXIT_CODE)`; the supervisor
        // recording a crash sidecar on top would inflate fleet-crash
        // metrics and mask the operator-action signal. Mirror
        // exclusion of STALL_EXIT_CODE.
        assert!(!should_record_supervisor_crash(
            crate::watchdog::AGENT_DELETED_EXIT_CODE as u32
        ));
        // Defence: the constant must NOT collide with the stall code,
        // otherwise either arm could mask the other (constants set in
        // watchdog.rs — locked here so a future renumbering trips the
        // test before it ships).
        assert_ne!(
            crate::watchdog::AGENT_DELETED_EXIT_CODE,
            crate::watchdog::STALL_EXIT_CODE,
            "exit-code sentinels must be distinct"
        );
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

    // ─── rc.51: uptime-gated counter reset ─────────────────────────────────

    #[test]
    fn reap_resets_counter_boundary() {
        // Below the 30 s threshold → not healthy → counter keeps
        // climbing (a crash-loop iteration).
        assert!(!reap_resets_counter(Duration::ZERO));
        assert!(!reap_resets_counter(Duration::from_millis(400)));
        assert!(!reap_resets_counter(Duration::from_secs(29)));
        // At/above the threshold → healthy → fresh ladder.
        assert!(reap_resets_counter(Duration::from_secs(30)));
        assert!(reap_resets_counter(Duration::from_secs(31)));
        assert!(reap_resets_counter(Duration::from_secs(3600)));
    }

    #[test]
    fn crash_loop_escalates_backoff_when_uptime_short() {
        // Simulates the F1 field bug: a worker that spawns then dies
        // in ~400 ms every iteration. The reap path passes the LIVE
        // counter (uptime < 30 s) so `decide_exit_reaction` climbs the
        // ladder — 2→4→8→16→32→60 s — instead of being pinned at 2 s.
        let mut consecutive_failures: u32 = 0;
        let short_uptime = Duration::from_millis(400);
        let mut backoffs = Vec::new();
        for _ in 0..6 {
            let prior = if reap_resets_counter(short_uptime) {
                0
            } else {
                consecutive_failures
            };
            let (reaction, next) = decide_exit_reaction(1, prior);
            consecutive_failures = next;
            if let ExitReaction::Backoff(d) = reaction {
                backoffs.push(d.as_secs());
            }
        }
        assert_eq!(backoffs, vec![2, 4, 8, 16, 32, 60]);
        assert_eq!(consecutive_failures, 6);
    }

    #[test]
    fn healthy_run_then_crash_starts_fresh_ladder() {
        // A worker that ran well past the threshold then exited
        // non-zero is an ISOLATED failure, not a loop — the reap path
        // passes prior=0 so the next backoff is the 2 s floor, not an
        // inherited escalated value.
        let consecutive_failures: u32 = 5; // a prior loop had escalated
        let long_uptime = Duration::from_secs(600);
        let prior = if reap_resets_counter(long_uptime) {
            0
        } else {
            consecutive_failures
        };
        let (reaction, next) = decide_exit_reaction(1, prior);
        assert_eq!(next, 1, "healthy-then-crash restarts the ladder at 1");
        assert!(matches!(reaction, ExitReaction::Backoff(d) if d == Duration::from_secs(2)));
    }

    #[test]
    fn clean_exit_resets_regardless_of_uptime() {
        // code 0 always resets via decide_exit_reaction's own arm —
        // the uptime gate never suppresses a clean-exit reset.
        for uptime in [Duration::from_millis(50), Duration::from_secs(999)] {
            let prior = if reap_resets_counter(uptime) { 0 } else { 9 };
            let (reaction, next) = decide_exit_reaction(0, prior);
            assert_eq!(next, 0);
            assert!(matches!(reaction, ExitReaction::Respawn));
        }
    }

    // ─── Phase 4: observability constants pinned ─────────────────────────────

    #[test]
    // Compile-time contract lock — the constant-ness IS the point.
    #[allow(clippy::assertions_on_constants)]
    fn respawn_alarm_threshold_aligns_with_backoff_cap() {
        // RESPAWN_ALARM_THRESHOLD is the observability signal — past
        // this many consecutive failures the supervisor escalates to
        // `error!` (vs the default per-exit `warn!`) so a crash-loop
        // surfaces in log aggregation. Pinned at 8: roughly where
        // `next_backoff` saturates at the cap (2s, 4s, 8s, 16s, 32s,
        // 60s, 60s, 60s — by failure #8 we've been climbing for
        // ~3 min). A lower value would false-alarm on isolated
        // crashes; a higher value buries crash-loops under hours of
        // muted backoff.
        assert!(
            RESPAWN_ALARM_THRESHOLD >= 4,
            "ALARM threshold must be high enough to avoid false-alarming \
             on isolated crashes; got {RESPAWN_ALARM_THRESHOLD}"
        );
        assert!(
            RESPAWN_ALARM_THRESHOLD <= 15,
            "ALARM threshold must be low enough that crash-loops surface \
             within minutes, not hours; got {RESPAWN_ALARM_THRESHOLD}"
        );
        // The threshold should align roughly with the failure count
        // where next_backoff hits its cap — going beyond the cap adds
        // no extra backoff growth, just delayed visibility.
        let cap_failures = (0..32u32)
            .find(|&n| next_backoff(n) >= RESPAWN_BACKOFF_CAP)
            .expect("next_backoff must reach cap at some finite failure count");
        // Phase 4: alarm should fire within ~5 failures of the backoff
        // cap kicking in. The two values are tuned independently but
        // logically belong together — a drift in either should
        // trigger a paired review.
        assert!(
            RESPAWN_ALARM_THRESHOLD.abs_diff(cap_failures) <= 5,
            "RESPAWN_ALARM_THRESHOLD ({RESPAWN_ALARM_THRESHOLD}) should align \
             within ~5 of where backoff saturates ({cap_failures})"
        );
    }

    #[test]
    fn healthy_uptime_threshold_above_crash_loop_timing() {
        // Field repro on a third field-test host (2026-05-17): doomed
        // crash-loop worker died in ~400 ms each iteration. The reap
        // path must NOT treat that as "healthy". 30 s is comfortable
        // headroom — well above the observed crash-loop dwell, well
        // below `next_backoff`'s 60 s cap (so a brief stutter doesn't
        // get suppressed alongside a real loop).
        assert!(
            HEALTHY_UPTIME_THRESHOLD >= Duration::from_secs(5),
            "HEALTHY_UPTIME_THRESHOLD must exceed observed crash-loop dwell"
        );
        assert!(
            HEALTHY_UPTIME_THRESHOLD <= RESPAWN_BACKOFF_CAP,
            "HEALTHY_UPTIME_THRESHOLD must be below backoff cap so the \
             reset path can fire before a healthy worker is even possible"
        );
    }
}
