// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2b-1 — a session's worker on Windows (the FR-90 spec's P8-0,
//! points 3 and 4): its channel a named pipe of its own, whose client end is
//! the one handle it inherits; started into a job of its own, as the
//! daemon's identity, its memory bounded there.
//!
//! | Piece | As built |
//! |---|---|
//! | the channel | `\\.\pipe\roomler-hive-worker-<pid>-<n>`, one instance, local clients only, its DACL protected and naming the daemon's own account alone (SYSTEM, on a device); its client end opened by the daemon, overlapped and inheritable, before any other process could take the one instance |
//! | what it inherits | that client end and `NUL` for its standard handles, named in `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, and nothing else the daemon holds (tokio's sockets among them, #2836) |
//! | how it finds the channel | the handle's value in [`CHANNEL_ENV`], beside `ROOMLERD_HIVE_WORKER` |
//! | its job | `KILL_ON_JOB_CLOSE`, so the daemon's end is the worker's, and a per-process memory limit of [`MAX_MEMORY`]: an allocation past it fails in the worker, and the worker alone ends |
//! | the start | suspended, assigned to the job, then resumed, so it runs no instruction outside the job; one that cannot join is ended, never resumed |
//! | its end | the job ended, and the process waited for; a peak at the bound is logged as what ended it |
//!
//! ⚠️ No endpoint is served here yet: the daemon serves them on Windows until
//! a listening socket can cross as `WSADuplicateSocketW`'s protocol info
//! (P8-2b-2). ⚠️ Before then, too, a worker's crash dump must carry no heap,
//! as P8-2a-3 keeps a Unix worker's memory out of a core.

use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use tracing::warn;
use windows_sys::Win32::Foundation::{
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, TRUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    TerminateProcess, WaitForSingleObject,
};

use super::Running;

/// Where a worker finds its channel: the inherited handle's value.
pub(crate) const CHANNEL_ENV: &str = "ROOMLERD_HIVE_WORKER_CHANNEL";
/// The most memory one worker holds: its job's per-process limit.
pub(crate) const MAX_MEMORY: usize = 512 << 20;
/// How long a worker's end is waited for, once its job is ended.
const END_WAIT: Duration = Duration::from_secs(5);

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
}

#[async_trait::async_trait]
impl Running for InJob {
    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    async fn kill(&mut self) {
        let peak = self.peak();
        if let Some(job) = self.job.as_ref() {
            // SAFETY: a live job handle.
            unsafe { TerminateJobObject(job.as_raw_handle() as HANDLE, 1) };
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
        // The job's last handle: KILL_ON_JOB_CLOSE ends anything left in it.
        self.job = None;
        if peak.is_some_and(|p| p + (1 << 20) >= MAX_MEMORY) {
            warn!(
                pid = self.pid,
                max_mib = MAX_MEMORY >> 20,
                "hive: a session's worker reached its memory bound: what it allocated past it failed"
            );
        }
    }
}

#[cfg(test)]
mod tests;
