// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2b-1 and P8-2b-2 — a session's worker on Windows (the FR-90
//! spec's P8-0, points 3 and 4): its channel a named pipe of its own, whose
//! client end is the one handle it inherits; started into a job of its own, as
//! the daemon's identity, its memory bounded there — and kept out of every
//! crash dump, since Windows Error Reporting would otherwise write the lease
//! values it holds to disk (P8-2b-2).
//!
//! | Piece | As built |
//! |---|---|
//! | the channel | `\\.\pipe\roomler-hive-worker-<pid>-<n>`, one instance, local clients only, its DACL protected and naming the daemon's own account alone (SYSTEM, on a device); its client end opened by the daemon, overlapped and inheritable, before any other process could take the one instance |
//! | what it inherits | that client end and `NUL` for its standard handles, named in `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, and nothing else the daemon holds (tokio's sockets among them, #2836) |
//! | how it finds the channel | the handle's value in [`CHANNEL_ENV`], beside `ROOMLERD_HIVE_WORKER` |
//! | its job | `KILL_ON_JOB_CLOSE`, so the daemon's end is the worker's, and a per-process memory limit of [`MAX_MEMORY`]: an allocation past it fails in the worker, and the worker alone ends |
//! | the start | suspended, assigned to the job, then resumed, so it runs no instruction outside the job; one that cannot join is ended, never resumed |
//! | its crash | nothing of its memory kept ([`no_crash_dump`], P8-2b-2): set before it takes its channel, `SEM_NOGPFAULTERRORBOX` makes WER write no dump, no event and no upload for the worker, on every fault path and thread. A worker that cannot set it does not run, as one that cannot set its Unix limits does not (P8-2a-3) |
//! | its end | the job ended with a code of its own, the process waited for, then its exit code read and classified ([`InJob::ended`]): with crash reporting suppressed, nothing else on the box records how a worker ended, so the daemon logs it — a fault named, a peak at the bound read as out-of-memory |
//!
//! ⚠️ No endpoint is served here yet: the daemon serves them on Windows until
//! a listening socket can cross as `WSADuplicateSocketW`'s protocol info
//! (P8-2b-3). ⚠️ The crash-privacy mechanism was the one that held: measured
//! as SYSTEM with a full `LocalDumps` config, `WerSetFlags(WER_FAULT_REPORTING_FLAG_NOHEAP)`
//! did not keep heap out of the dump, and a top-level exception filter and the
//! job's `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION` were each skipped by a
//! fail-fast (Rust's `abort`, so every OOM at the bound). Only the process's
//! own `SEM_NOGPFAULTERRORBOX` suppressed WER on every fault and thread (§3i
//! "P8-2b-2, as built").

use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{
    AsRawHandle, FromRawHandle, FromRawSocket, OwnedHandle, OwnedSocket, RawHandle,
};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use tracing::warn;
use windows_sys::Win32::Foundation::{
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, TRUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Networking::WinSock::{
    FROM_PROTOCOL_INFO, INVALID_SOCKET, WSA_FLAG_OVERLAPPED, WSADATA, WSADuplicateSocketW,
    WSAPROTOCOL_INFOW, WSASocketW, WSAStartup,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    GetErrorMode, SEM_NOGPFAULTERRORBOX, SetErrorMode,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, TerminateProcess, WaitForSingleObject,
};

use super::Running;

/// Where a worker finds its channel: the inherited handle's value.
pub(crate) const CHANNEL_ENV: &str = "ROOMLERD_HIVE_WORKER_CHANNEL";
/// The most memory one worker holds: its job's per-process limit.
pub(crate) const MAX_MEMORY: usize = 512 << 20;
/// How long a worker's end is waited for, once its job is ended.
const END_WAIT: Duration = Duration::from_secs(5);

/// The exit code the daemon ends a worker's job with, so an ordinary end is
/// never read as a crash: the worker itself returns only 0–3, and a native
/// fault is a `0xC…` NTSTATUS, so none of those collide with it.
const ENDED_BY_DAEMON: u32 = 1;

/// What `GetExitCodeProcess` reports for a process that has not exited.
const STILL_ACTIVE: u32 = 259;

// The native fault codes the daemon names in its log. With crash reporting
// suppressed ([`no_crash_dump`]) nothing else on the box records how a worker
// ended, so its exit code is the only account of it.
const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
const STATUS_STACK_OVERFLOW: u32 = 0xC000_00FD;
/// A fail-fast (`__fastfail`), which is what Rust's `abort` raises: a panic
/// past its handler, or an allocation that failed at the memory bound
/// (`handle_alloc_error`). The two are told apart by the job's peak.
const STATUS_FATAL_APP_EXIT: u32 = 0xC000_0409;
const STATUS_NO_MEMORY: u32 = 0xC000_0017;

static NEXT: AtomicU64 = AtomicU64::new(1);

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn last_error(what: &str) -> std::io::Error {
    let e = std::io::Error::last_os_error();
    std::io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// A worker's channel: a named pipe of its own, its server end the daemon's;
/// and its client end, opened here, overlapped and inheritable, for the
/// worker to be handed.
pub(crate) fn channel() -> std::io::Result<(NamedPipeServer, OwnedHandle)> {
    channel_named(&format!(
        r"\\.\pipe\roomler-hive-worker-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// [`channel`], at `name`.
fn channel_named(name: &str) -> std::io::Result<(NamedPipeServer, OwnedHandle)> {
    let own = crate::hive_win::own_sid().map_err(std::io::Error::other)?;
    // Protected, and the daemon's own account alone: no other account opens
    // the pipe, and the one instance is taken below before anyone could.
    let mut sddl = crate::hive_win::Sddl::new(&format!("D:P(A;;GA;;;{own})"))?;
    // SAFETY: the attributes live in `sddl` across the call, and the system
    // copies the descriptor into the instance.
    let server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .max_instances(1)
            .reject_remote_clients(true)
            .access_inbound(true)
            .access_outbound(true)
            .create_with_security_attributes_raw(name, sddl.attributes().cast())?
    };
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: TRUE,
    };
    let path = wide(std::ffi::OsStr::new(name));
    // SAFETY: a NUL-terminated name and valid attributes; the handle is owned
    // below.
    let client = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            &sa,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if client == INVALID_HANDLE_VALUE {
        return Err(last_error("opening the worker's end of its pipe"));
    }
    // SAFETY: a handle CreateFileW just made, owned nowhere else.
    Ok((server, unsafe {
        OwnedHandle::from_raw_handle(client as RawHandle)
    }))
}

/// A worker's channel, as the worker takes it: the handle its daemon named
/// in [`CHANNEL_ENV`]. `None` where there is none, or it is no pipe's.
pub(crate) fn inherited_channel() -> Option<NamedPipeClient> {
    let raw = std::env::var(CHANNEL_ENV).ok()?.parse::<usize>().ok()?;
    // SAFETY: the daemon handed this process this handle, overlapped, and
    // nothing else owns it: this is the one place it is taken.
    unsafe { NamedPipeClient::from_raw_handle(raw as RawHandle) }.ok()
}

/// FR-90 P8-2b-2 — a worker keeps nothing of its memory in a crash dump. It
/// sets `SEM_NOGPFAULTERRORBOX`, so when it faults Windows Error Reporting
/// writes no dump, logs no Application Error event and uploads nothing — on
/// every fault path (an access violation, a stack overflow, a fail-fast — the
/// `abort` every OOM at the bound ends in) and every thread, since the mode is
/// per process and covers one that already runs. A worker's memory holds the
/// opened values of the leases it serves, as a Unix worker's does (P8-2a-3's
/// one-byte core limit); a `LocalDumps` config — an administrator's, or a
/// vendor driver's — would otherwise write all of it to disk in a full dump,
/// and the default triage upload would carry a Microsoft bucket of it.
///
/// Measured as SYSTEM with a full `LocalDumps`: this was the one mechanism that
/// held. `WerSetFlags(NOHEAP)` did not keep heap out of a `LocalDumps` dump,
/// and a top-level exception filter and the job's
/// `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION` were each skipped by a
/// fail-fast (§3i "P8-2b-2, as built").
///
/// Set before the worker takes its channel, and read back: a worker that
/// cannot set it does not run, as one that cannot set its Unix limits does not
/// (P8-2a-3). The bit is OR'd in, so a `SEM_FAILCRITICALERRORS` or
/// `SEM_NOOPENFILEERRORBOX` the worker's launcher left on stays. The worker
/// spawns no child; one that ever did would carry this mode into it unless
/// started with `CREATE_DEFAULT_ERROR_MODE`.
pub(crate) fn no_crash_dump() -> std::io::Result<()> {
    // SAFETY: GetErrorMode and SetErrorMode read and set this process's own
    // error mode; neither takes a pointer. GetErrorMode (not SetErrorMode(0))
    // reads the current bits, so no fault in another thread meets a cleared
    // mode in the window between the read and the set.
    let armed = unsafe {
        let current = GetErrorMode();
        SetErrorMode(current | SEM_NOGPFAULTERRORBOX);
        GetErrorMode()
    };
    if armed & SEM_NOGPFAULTERRORBOX == 0 {
        return Err(std::io::Error::other(
            "SEM_NOGPFAULTERRORBOX did not take: a crash could dump the worker's memory",
        ));
    }
    Ok(())
}

/// A job of a worker's own: what it starts dies with it, and with the
/// daemon's handle, and each of its processes holds at most [`MAX_MEMORY`].
fn job() -> std::io::Result<OwnedHandle> {
    // SAFETY: an unnamed job with default security; owned below.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(last_error("CreateJobObjectW"));
    }
    // SAFETY: a handle CreateJobObjectW just made.
    let job = unsafe { OwnedHandle::from_raw_handle(job as RawHandle) };
    // SAFETY: all-zero is a valid limit information: no limit set.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    info.ProcessMemoryLimit = MAX_MEMORY;
    // SAFETY: `info` matches the class set, and outlives the call.
    let ok = unsafe {
        SetInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error("SetInformationJobObject"));
    }
    Ok(job)
}

/// `NUL`, inheritable: a worker's standard handles.
fn nul() -> std::io::Result<OwnedHandle> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: TRUE,
    };
    let path = wide(std::ffi::OsStr::new("NUL"));
    // SAFETY: a NUL-terminated name and valid attributes; owned below.
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &sa,
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(last_error("opening NUL"));
    }
    // SAFETY: a handle CreateFileW just made.
    Ok(unsafe { OwnedHandle::from_raw_handle(h as RawHandle) })
}

/// The daemon's environment with `set` laid over it by name, as Windows
/// matches names (case-insensitively), sorted, as a Unicode block.
fn environment(set: &[(OsString, OsString)]) -> Vec<u16> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| {
            !set.iter().any(|(s, _)| {
                s.to_string_lossy()
                    .eq_ignore_ascii_case(&k.to_string_lossy())
            })
        })
        .collect();
    vars.extend(set.iter().cloned());
    vars.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_wide());
        block.push(u16::from(b'='));
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// `exe` with `args` as one command line, `arg0` for what the process is
/// called: each quoted for the C runtime.
fn command_line(exe: &Path, arg0: Option<&std::ffi::OsStr>, args: &[String]) -> Vec<u16> {
    let quote = |s: &std::ffi::OsStr| {
        let mut out = vec![u16::from(b'"')];
        out.extend(s.encode_wide().filter(|c| *c != u16::from(b'"')));
        out.push(u16::from(b'"'));
        out
    };
    let mut line = quote(arg0.unwrap_or(exe.as_os_str()));
    for arg in args {
        line.push(u16::from(b' '));
        line.extend(quote(std::ffi::OsStr::new(arg)));
    }
    line.push(0);
    line
}

/// Start `exe` as a worker, its channel's client end `pipe`: as the daemon's
/// own identity, into a job of its own, suspended until it is in it.
pub(crate) fn spawn(
    exe: &Path,
    arg0: Option<&std::ffi::OsStr>,
    args: &[String],
    pipe: OwnedHandle,
) -> std::io::Result<InJob> {
    let job = job()?;
    let (stdin, stdout, stderr) = (nul()?, nul()?, nul()?);
    let handles = vec![
        pipe.as_raw_handle() as HANDLE,
        stdin.as_raw_handle() as HANDLE,
        stdout.as_raw_handle() as HANDLE,
        stderr.as_raw_handle() as HANDLE,
    ];
    let mut startup =
        crate::win_service::supervisor::InheritOnly::new(handles).map_err(std::io::Error::other)?;
    let si = &mut startup.si.StartupInfo;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = stdin.as_raw_handle() as HANDLE;
    si.hStdOutput = stdout.as_raw_handle() as HANDLE;
    si.hStdError = stderr.as_raw_handle() as HANDLE;
    let mut set: Vec<(OsString, OsString)> = tunnel_core::env::config_fallbacks_for_child()
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
    set.push((super::CHILD_ENV.into(), "1".into()));
    set.push((
        CHANNEL_ENV.into(),
        (pipe.as_raw_handle() as usize).to_string().into(),
    ));
    let env = environment(&set);
    let exe_w = wide(exe.as_os_str());
    let mut line = command_line(exe, arg0, args);
    // SAFETY: all-zero is a valid out-buffer.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated strings and a double-NUL environment block that
    // outlive the call; `startup` holds the handle list, live across it, and
    // inheritance is on only so that list applies.
    let ok = unsafe {
        CreateProcessW(
            exe_w.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            TRUE,
            CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | EXTENDED_STARTUPINFO_PRESENT
                | CREATE_UNICODE_ENVIRONMENT,
            env.as_ptr().cast(),
            std::ptr::null(),
            &raw const startup.si.StartupInfo,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(last_error("CreateProcessW"));
    }
    // SAFETY: handles CreateProcessW just made, owned from here.
    let (process, thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(pi.hProcess as RawHandle),
            OwnedHandle::from_raw_handle(pi.hThread as RawHandle),
        )
    };
    // SAFETY: both handles are live.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle() as HANDLE, pi.hProcess) } == 0 {
        let e = last_error("AssignProcessToJobObject");
        // SAFETY: the suspended process, ended before it ran a step.
        unsafe { TerminateProcess(pi.hProcess, 1) };
        return Err(e);
    }
    // SAFETY: the process's one thread, suspended at its start.
    if unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) } == u32::MAX {
        let e = last_error("ResumeThread");
        // SAFETY: a live job handle.
        unsafe { TerminateJobObject(job.as_raw_handle() as HANDLE, 1) };
        return Err(e);
    }
    drop((thread, pipe, stdin, stdout, stderr));
    Ok(InJob {
        process,
        pid: pi.dwProcessId,
        job: Some(job),
    })
}

/// A worker's process, in its job.
pub(crate) struct InJob {
    process: OwnedHandle,
    pid: u32,
    job: Option<OwnedHandle>,
}

impl InJob {
    /// The most memory a process of its job has held.
    fn peak(&self) -> Option<usize> {
        let job = self.job.as_ref()?;
        // SAFETY: all-zero is a valid out-buffer for this class.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is exactly the size passed, and outlives the call.
        let ok = unsafe {
            QueryInformationJobObject(
                job.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                (&raw mut info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(info.PeakProcessMemoryUsed)
    }

    /// FR-90 P8-2b-2 — log how the worker ended, from its exit code. With
    /// crash reporting suppressed ([`no_crash_dump`]) nothing else on the box
    /// records a worker's crash, so the daemon names it. `peak` is the job's
    /// high-water mark, which tells a fail-fast at the memory bound (an OOM)
    /// from one past a panic. The daemon ends the job with [`ENDED_BY_DAEMON`],
    /// which `TerminateJobObject` cannot write over a process that already
    /// faulted, so a real fault is read as itself and an ordinary end is not.
    fn ended(&self, peak: Option<usize>) {
        let mut code = 0u32;
        // SAFETY: a live process handle; `code` is a valid out-param.
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle() as HANDLE, &mut code) } == 0 {
            warn!(
                pid = self.pid,
                "hive: a session's worker ended; its exit code could not be read"
            );
            return;
        }
        if code == STILL_ACTIVE {
            // It had not ended; the wait above already warned.
            return;
        }
        let at_bound = peak.is_some_and(|p| p + (1 << 20) >= MAX_MEMORY);
        let shown = format!("{code:#010x}");
        match code {
            0 | ENDED_BY_DAEMON => {
                // A clean return, or the daemon's own job-end: not a crash.
                tracing::debug!(pid = self.pid, code = %shown, "hive: a session's worker ended");
            }
            2 | 3 => warn!(
                pid = self.pid,
                code = %shown,
                "hive: a session's worker refused to run (0x2 = no channel; 0x3 = could not keep its memory out of a crash dump)"
            ),
            STATUS_ACCESS_VIOLATION => warn!(
                pid = self.pid,
                code = %shown,
                "hive: a session's worker crashed: an access violation"
            ),
            STATUS_STACK_OVERFLOW => warn!(
                pid = self.pid,
                code = %shown,
                "hive: a session's worker crashed: a stack overflow"
            ),
            STATUS_FATAL_APP_EXIT if at_bound => warn!(
                pid = self.pid,
                max_mib = MAX_MEMORY >> 20,
                code = %shown,
                "hive: a session's worker ended: out of memory at its bound, an allocation past it aborted"
            ),
            STATUS_FATAL_APP_EXIT => warn!(
                pid = self.pid,
                code = %shown,
                "hive: a session's worker crashed: a fail-fast — a panic past its handler, or an aborted allocation"
            ),
            STATUS_NO_MEMORY => warn!(
                pid = self.pid,
                code = %shown,
                "hive: a session's worker crashed: out of memory"
            ),
            _ if at_bound => warn!(
                pid = self.pid,
                max_mib = MAX_MEMORY >> 20,
                code = %shown,
                "hive: a session's worker ended at its memory bound"
            ),
            _ => warn!(
                pid = self.pid,
                code = %shown,
                "hive: a session's worker ended with an unexpected exit code"
            ),
        }
    }
}

#[async_trait::async_trait]
impl Running for InJob {
    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    async fn kill(&mut self) {
        let peak = self.peak();
        if let Some(job) = self.job.as_ref() {
            // SAFETY: a live job handle. ENDED_BY_DAEMON marks an ordinary end;
            // it cannot write over the code of a process that already faulted,
            // so `ended` never reads the daemon's own kill as a crash.
            unsafe { TerminateJobObject(job.as_raw_handle() as HANDLE, ENDED_BY_DAEMON) };
        }
        let process = self.process.as_raw_handle() as usize;
        let ms = END_WAIT.as_millis() as u32;
        // SAFETY: the process handle lives in `self` across the wait, which is
        // bounded.
        let waited = tokio::task::spawn_blocking(move || unsafe {
            WaitForSingleObject(process as HANDLE, ms)
        })
        .await;
        if !matches!(waited, Ok(WAIT_OBJECT_0)) {
            warn!(
                pid = self.pid,
                "hive: a session's worker did not end with its job in time"
            );
        }
        // Read before the job's last handle drops: the process handle is still
        // this struct's, and the worker has ended (or the wait warned).
        self.ended(peak);
        // The job's last handle: KILL_ON_JOB_CLOSE ends anything left in it.
        self.job = None;
    }
}

// ─── FR-90 P8-2b-3: a listener crosses to the worker as protocol info ──────
//
// There is no fd-4 channel here (P8-2b-1: the client end of the pipe is the
// one handle a worker inherits), so a loopback listener crosses inside the
// `Serve` call's own frame: its `WSAPROTOCOL_INFOW`, 628 bytes, which the
// worker turns back into a real listener with `WSASocketW`. Measured
// (`C:/dev/_tools/...`): the mechanism round-trips across a real process
// boundary; a `rehand` (a new worker, a new pid) duplicates the same source
// listener again, which survives every duplication untouched.

/// One listener's protocol info, exactly: what [`duplicate_for`] produces and
/// [`socket_from_info`] takes back.
pub(crate) const PROTOCOL_INFO_LEN: usize = std::mem::size_of::<WSAPROTOCOL_INFOW>();

/// `WSAStartup`, before relying on it: idempotent (its count only grows until
/// `WSACleanup`, never called here), so every caller invokes it rather than
/// assume some other code already has. Measured: a process that goes straight
/// to `WSASocketW` without an ordinary socket call of its own first gets
/// `WSANOTINITIALISED`, since std's own sockets call it lazily and nothing
/// guarantees a path through this process took one first.
fn wsa_startup() -> std::io::Result<()> {
    // SAFETY: a valid out-buffer of the size the call documents.
    let rc = unsafe {
        let mut data: WSADATA = std::mem::zeroed();
        WSAStartup(0x0202, &mut data)
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(rc))
    }
}

/// `socket`'s protocol info, for `pid` to reconstruct with [`socket_from_info`]:
/// fresh every time, so a hand-over and a later `rehand` (a new worker, a new
/// pid) each duplicate the daemon's own copy again. `socket` is untouched —
/// cloneable this way as many times as workers come and go.
pub(crate) fn duplicate_for(
    pid: u32,
    socket: windows_sys::Win32::Networking::WinSock::SOCKET,
) -> std::io::Result<[u8; PROTOCOL_INFO_LEN]> {
    // SAFETY: `socket` is a live socket handle the caller owns across this
    // call; `info` is a valid out-buffer of the exact size the call expects.
    unsafe {
        let mut info: WSAPROTOCOL_INFOW = std::mem::zeroed();
        if WSADuplicateSocketW(socket, pid, &mut info) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut bytes = [0u8; PROTOCOL_INFO_LEN];
        let p = (&raw const info).cast::<u8>();
        bytes.copy_from_slice(std::slice::from_raw_parts(p, PROTOCOL_INFO_LEN));
        Ok(bytes)
    }
}

/// A listener, reconstructed from `bytes` ([`duplicate_for`]'s): refused where
/// this is not the process `pid` named, or where `bytes` is not one whole
/// protocol info. Calls [`wsa_startup`] first, every time.
pub(crate) fn socket_from_info(bytes: &[u8]) -> std::io::Result<std::net::TcpListener> {
    if bytes.len() != PROTOCOL_INFO_LEN {
        return Err(std::io::Error::other(format!(
            "a listener's protocol info is {} bytes, not {PROTOCOL_INFO_LEN}",
            bytes.len()
        )));
    }
    wsa_startup()?;
    // SAFETY: `bytes` is exactly one WSAPROTOCOL_INFOW, read back as the same
    // POD type it was written as.
    let info: WSAPROTOCOL_INFOW = unsafe { std::ptr::read(bytes.as_ptr().cast()) };
    // SAFETY: `info` lives across the call; the handle WSASocketW returns is
    // owned by nobody else, taken into an `OwnedSocket` below.
    let raw = unsafe {
        WSASocketW(
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            (&raw const info).cast_mut(),
            0,
            WSA_FLAG_OVERLAPPED,
        )
    };
    if raw == INVALID_SOCKET {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a handle WSASocketW just made, owned nowhere else.
    let socket = unsafe { OwnedSocket::from_raw_socket(raw as std::os::windows::io::RawSocket) };
    Ok(std::net::TcpListener::from(socket))
}

#[cfg(test)]
mod tests;
