// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! `roomlerdw.exe` — what the per-user Scheduled Task runs, so the daemon has
//! no window (#3292). The `w` is the one in `pythonw.exe` and `javaw.exe`.
//!
//! # Why
//!
//! `roomlerd.exe` is a console program and has to stay one: `enroll`, `service`
//! and every diagnostic answer on the terminal they were run from. But Task
//! Scheduler gives a console program started with an interactive token a new
//! console, so a per-user daemon kept a terminal of raw logs on the person's
//! desktop, and closing that window killed the daemon. This windows-subsystem
//! launcher gets no console, and starts the daemon the way the service's
//! supervisor starts its worker (`win_service::supervisor`).
//!
//! The alternatives, measured on a Win11 guest:
//! * `FreeConsole` first thing in the daemon leaves the terminal opening anyway:
//!   the console already exists when `main` runs.
//! * `conhost.exe --headless` as the task's command is undocumented, and public
//!   EDR rules look for exactly that command line.
//!
//! # Behaviour notes
//!
//! * `CREATE_NO_WINDOW`, not `DETACHED_PROCESS`: the daemon keeps a console
//!   nobody can see, and the console programs it spawns share it. With no console
//!   at all, every spawn without its own `CREATE_NO_WINDOW` would open one.
//! * It stays until the daemon exits and leaves with the daemon's code, because
//!   Task Scheduler watches THIS process: its `Running` state, `IgnoreNew` and
//!   `LastTaskResult` must keep meaning what they meant for the daemon.
//! * The daemon sits in a kill-on-close job, because `schtasks /End` ends this
//!   process and nothing below it, measured: without the job the task reads
//!   `Ready`, `/End` reports success, and the daemon runs on. Task Scheduler's own
//!   job, which holds this process too, is not what `/End` closes. The job lets the
//!   daemon's children out, with or without their asking: `msiexec` and its watcher
//!   must outlive the daemon on an update, and so must the companion and a session's
//!   shell.
//! * At logoff it is shut down after the daemon, which gets its own console
//!   control event first; otherwise this process going would kill the daemon
//!   through the job before that event arrived.
//! * The daemon is the sibling file, never looked up on PATH, so another Roomler
//!   install elsewhere can never be started in its place.
//! * The daemon's stdio is `NUL`. Its stdout carries only what its rolling log
//!   carries too, and a task-supervised daemon points its own stderr at that log
//!   (`logging::send_stderr_to_the_log`).

#![cfg_attr(windows, windows_subsystem = "windows")]

/// Exit code for "we could not even start the daemon". 127 is the shell's
/// command-not-found, as in `roomler-cli-shim`.
#[cfg(windows)]
const EXIT_NO_DAEMON: i32 = 127;

#[cfg(windows)]
fn main() -> ! {
    std::process::exit(run())
}

#[cfg(not(windows))]
fn main() {
    // The workspace builds on every OS; only the Windows per-user MSI ships this.
    eprintln!("roomlerdw is the Windows Scheduled Task's launcher: nothing to do here");
    std::process::exit(2);
}

#[cfg(windows)]
fn run() -> i32 {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let Ok(me) = std::env::current_exe() else {
        return EXIT_NO_DAEMON;
    };
    shut_down_after_the_daemon();
    let job = kill_on_close_job();
    let spawned = Command::new(me.with_file_name("roomlerd.exe"))
        .args(std::env::args_os().skip(1))
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut daemon) = spawned else {
        return EXIT_NO_DAEMON;
    };
    if let Some(job) = job {
        // A daemon that is already gone refuses this, and `wait` answers for it.
        // SAFETY: both handles are valid and owned here for the whole call.
        unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                job,
                daemon.as_raw_handle(),
            );
        }
    }
    match daemon.wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    }
}

/// A job that kills its member when the last handle to it closes, which is this
/// process going, and whose member's children stay out of it. `None` when Windows
/// refuses: the daemon then runs outside it, and only a hard stop is weaker. The
/// handle stays open for the life of the process, on purpose.
#[cfg(windows)]
fn kill_on_close_job() -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    // SAFETY: null attributes and name ask for an unnamed job with default
    // security; `info` is a valid, fully zeroed struct of the size passed.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        // BREAKAWAY_OK as well: `companion_spawn` asks by name, and a job with
        // only the silent flag is not one it has been measured against.
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK
            | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        (set != 0).then_some(job)
    }
}

/// The lowest shutdown level an application may take, below the daemon's
/// default, so a logoff reaches the daemon first.
#[cfg(windows)]
fn shut_down_after_the_daemon() {
    use windows_sys::Win32::System::Threading::SetProcessShutdownParameters;
    // Not bound by windows-sys on this line.
    const SHUTDOWN_NORETRY: u32 = 1;
    // SAFETY: plain value parameters; a refusal only leaves the default order.
    unsafe {
        SetProcessShutdownParameters(0x100, SHUTDOWN_NORETRY);
    }
}
