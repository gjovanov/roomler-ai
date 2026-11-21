// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! `roomlerdw.exe` — what the per-user Scheduled Task runs, so the daemon has
//! no window (#3292), and since FR-150 P2 (#3295) its supervisor. The `w` is the
//! one in `pythonw.exe` and `javaw.exe`.
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
//! * FR-150 P2: for the daemon's own run, exactly `run --supervisor task`, it
//!   starts the daemon again after a crash or a start that failed, with the
//!   service supervisor's backoff, and leaves with the daemon's code after a
//!   deliberate end or while the session ends ([`ends`]). Task Scheduler's own
//!   `<RestartOnFailure>` restarts no exit at all (#3292). Every other verb it
//!   runs (`--version` for the launcher check, `service start` for the start
//!   task) and the daemon's run with `ROOMLERDW_SUPERVISE=0` are waited for
//!   once, as before P2.
//! * It stays while the daemon runs and leaves with the daemon's code, because
//!   Task Scheduler watches THIS process: its `Running` state, `IgnoreNew` and
//!   `LastTaskResult` must keep meaning what they meant for the daemon.
//! * The daemon sits in a kill-on-close job, because `schtasks /End` ends this
//!   process and nothing below it, measured: without the job the task reads
//!   `Ready`, `/End` reports success, and the daemon runs on. Task Scheduler's own
//!   job, which holds this process too, is not what `/End` closes. The job lets the
//!   daemon's children out, with or without their asking: `msiexec` and its watcher
//!   must outlive the daemon on an update, and so must the companion and a session's
//!   shell. `/End` stays a hard stop: nothing restarts the daemon once this process
//!   has gone.
//! * At logoff it is shut down after the daemon, which gets its own console
//!   control event first; otherwise this process going would kill the daemon
//!   through the job before that event arrived.
//! * The daemon is the sibling file, never looked up on PATH, so another Roomler
//!   install elsewhere can never be started in its place.
//! * The daemon's stdio is `NUL`. Its stdout carries only what its rolling log
//!   carries too, and a task-supervised daemon points its own stderr at that log
//!   (`logging::send_stderr_to_the_log`). The launcher writes nothing itself: it
//!   must not touch the data tree, so the daemon's own log shows each start.

#![cfg_attr(windows, windows_subsystem = "windows")]

// FR-150 P2 — pure, and compiled on every OS so its tests run everywhere; only
// the Windows launcher runs it.
#[cfg_attr(not(windows), allow(dead_code))]
mod ends;

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
    let Ok(me) = std::env::current_exe() else {
        return ends::NO_DAEMON;
    };
    shut_down_after_the_daemon();
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mut launcher = Launcher {
        daemon: me.with_file_name("roomlerd.exe"),
        args: args.clone(),
        job: kill_on_close_job(),
        clock: std::time::Instant::now(),
    };
    let switch = std::env::var_os(ends::SUPERVISE_ENV);
    ends::launch(&args, switch.as_deref(), &mut launcher)
}

/// The Windows side of [`ends::Seam`]: the daemon beside this file, started the
/// way the service's supervisor starts its worker, in the kill-on-close job.
#[cfg(windows)]
struct Launcher {
    /// The sibling `roomlerd.exe`.
    daemon: std::path::PathBuf,
    /// This process's own arguments, handed to every daemon it starts.
    args: Vec<std::ffi::OsString>,
    /// [`kill_on_close_job`]'s job, which every daemon joins.
    job: Option<windows_sys::Win32::Foundation::HANDLE>,
    /// Where [`ends::Seam::now`] counts from.
    clock: std::time::Instant,
}

#[cfg(windows)]
impl ends::Seam for Launcher {
    type Daemon = std::process::Child;

    fn spawn(&mut self) -> Option<std::process::Child> {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let daemon = Command::new(&self.daemon)
            .args(&self.args)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        if let Some(job) = self.job {
            // A daemon that is already gone refuses this, and `wait` answers for it.
            // SAFETY: both handles are valid and owned here for the whole call.
            unsafe {
                windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                    job,
                    daemon.as_raw_handle(),
                );
            }
        }
        Some(daemon)
    }

    fn wait(&mut self, mut daemon: std::process::Child) -> i32 {
        match daemon.wait() {
            Ok(status) => status.code().unwrap_or(1),
            Err(_) => 1,
        }
    }

    fn session_ending(&self) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
        // SAFETY: a plain value parameter; the call has no preconditions.
        unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
    }

    fn sleep(&mut self, wait: std::time::Duration) {
        std::thread::sleep(wait);
    }

    fn now(&self) -> std::time::Duration {
        self.clock.elapsed()
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

#[cfg(test)]
mod tests {
    //! FR-150 P2 — the launcher's loop over a scripted host: the ends, the
    //! backoff and its reset, a failed start, an ending session, and which runs
    //! are supervised at all.

    use super::ends::{self, End, Seam};
    use std::ffi::OsStr;
    use std::time::Duration;

    const SEC: Duration = Duration::from_secs(1);

    /// A scripted host. Each start takes the next entry of `starts`: a daemon
    /// that runs for the given time and ends with the given code, or `None`, a
    /// start that fails. The session is ending once `ending_from` starts have
    /// been tried, or once the launcher has slept `ending_after_sleeps` times.
    struct Script {
        starts: Vec<Option<(i32, Duration)>>,
        tried: usize,
        ending_from: Option<usize>,
        ending_after_sleeps: Option<usize>,
        clock: Duration,
        slept: Vec<Duration>,
    }

    impl Script {
        fn new(starts: Vec<Option<(i32, Duration)>>) -> Self {
            Self {
                starts,
                tried: 0,
                ending_from: None,
                ending_after_sleeps: None,
                clock: Duration::ZERO,
                slept: Vec::new(),
            }
        }

        fn ending_from(mut self, tried: usize) -> Self {
            self.ending_from = Some(tried);
            self
        }

        fn ending_after_sleeps(mut self, sleeps: usize) -> Self {
            self.ending_after_sleeps = Some(sleeps);
            self
        }

        /// Daemons that ran, failed starts not counted.
        fn ran(&self) -> usize {
            self.starts[..self.tried].iter().flatten().count()
        }
    }

    impl Seam for Script {
        type Daemon = (i32, Duration);

        fn spawn(&mut self) -> Option<(i32, Duration)> {
            let start = *self
                .starts
                .get(self.tried)
                .unwrap_or_else(|| panic!("start {} was not in the script", self.tried + 1));
            self.tried += 1;
            start
        }

        fn wait(&mut self, (code, ran): (i32, Duration)) -> i32 {
            self.clock += ran;
            code
        }

        fn session_ending(&self) -> bool {
            self.ending_from.is_some_and(|n| self.tried >= n)
                || self
                    .ending_after_sleeps
                    .is_some_and(|n| self.slept.len() >= n)
        }

        fn sleep(&mut self, wait: Duration) {
            self.clock += wait;
            self.slept.push(wait);
        }

        fn now(&self) -> Duration {
            self.clock
        }
    }

    fn crash() -> Option<(i32, Duration)> {
        Some((1, SEC))
    }

    fn stop() -> Option<(i32, Duration)> {
        Some((ends::CLEAN_EXIT, SEC))
    }

    #[test]
    fn every_deliberate_end_leaves_with_its_code() {
        for (code, word) in [
            (0, "clean-exit"),
            (7, "agent-deleted"),
            (8, "already-running"),
            (9, "restart-requested"),
            (78, "no-enrollment"),
            (75, "install-in-flight"),
        ] {
            assert_eq!(ends::end_of(code, false), End::Leave(word));
            let mut host = Script::new(vec![Some((code, SEC))]);
            assert_eq!(ends::supervise(&mut host), code);
            assert_eq!(host.ran(), 1, "{word}");
            assert!(host.slept.is_empty(), "{word}: {:?}", host.slept);
        }
    }

    #[test]
    fn a_stall_a_panic_and_an_exception_are_crashes() {
        // 1: an error; 2: the watchdog's stall; 101: a panic; an access
        // violation; a console close; the session teardown 0x40010004 while the
        // session is not ending.
        for code in [
            1,
            2,
            101,
            0xC000_0005_u32 as i32,
            0xC000_013A_u32 as i32,
            0x4001_0004,
        ] {
            assert_eq!(ends::end_of(code, false), End::Restart, "{code:#x}");
        }
    }

    #[test]
    fn a_crash_is_started_again_after_the_service_supervisors_backoff() {
        let mut starts = vec![crash(); 7];
        starts.push(stop());
        let mut host = Script::new(starts);
        assert_eq!(ends::supervise(&mut host), 0);
        assert_eq!(host.ran(), 8);
        assert_eq!(host.slept, [2, 4, 8, 16, 32, 60, 60].map(|s| s * SEC));
    }

    #[test]
    fn the_backoff_is_the_service_supervisors() {
        assert_eq!(ends::backoff(0), Duration::ZERO);
        let waits: Vec<u64> = (1..=8).map(|n| ends::backoff(n).as_secs()).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(ends::backoff(u32::MAX), ends::BACKOFF_CAP);
        assert_eq!(ends::BACKOFF_INITIAL, 2 * SEC);
        assert_eq!(ends::BACKOFF_CAP, 60 * SEC);
    }

    #[test]
    fn a_run_of_five_minutes_starts_the_backoff_again() {
        // Three quick crashes climb to 8 s; a run of exactly five minutes that
        // then crashes starts again at 2 s.
        let mut host = Script::new(vec![
            crash(),
            crash(),
            crash(),
            Some((1, ends::HEALTHY_RUN)),
            stop(),
        ]);
        assert_eq!(ends::supervise(&mut host), 0);
        assert_eq!(host.slept, [2, 4, 8, 2].map(|s| s * SEC));

        // A second short of it does not.
        let mut host = Script::new(vec![
            crash(),
            crash(),
            crash(),
            Some((1, ends::HEALTHY_RUN - SEC)),
            stop(),
        ]);
        assert_eq!(ends::supervise(&mut host), 0);
        assert_eq!(host.slept, [2, 4, 8, 16].map(|s| s * SEC));
    }

    #[test]
    fn a_start_that_fails_is_tried_again() {
        // The file held at sign-in (an EDR's scan): two failed starts, then the
        // daemon runs and is stopped.
        let mut host = Script::new(vec![None, None, stop()]);
        assert_eq!(ends::supervise(&mut host), 0);
        assert_eq!(host.ran(), 1);
        assert_eq!(host.slept, [2, 4].map(|s| s * SEC));
    }

    #[test]
    fn an_ending_session_leaves() {
        // A daemon that ends while the session ends: whatever its code, the
        // launcher leaves with it and starts nothing.
        let mut host = Script::new(vec![crash()]).ending_from(1);
        assert_eq!(ends::supervise(&mut host), 1);
        assert_eq!(host.tried, 1);
        assert!(host.slept.is_empty());
        assert_eq!(ends::end_of(1, true), End::Leave("session-ending"));
        assert_eq!(ends::end_of(0, true), End::Leave("session-ending"));

        // A session that ends while the launcher waits to start again after a
        // stall: no start into it, and the last daemon's code.
        let mut host = Script::new(vec![Some((2, SEC)), crash()]).ending_after_sleeps(1);
        assert_eq!(ends::supervise(&mut host), 2);
        assert_eq!(host.tried, 1);
        assert_eq!(host.slept, [2 * SEC]);

        // The same after a failed start: the launcher's own code for it.
        let mut host = Script::new(vec![crash(), None, crash()]).ending_after_sleeps(2);
        assert_eq!(ends::supervise(&mut host), ends::NO_DAEMON);
        assert_eq!(host.tried, 2);

        // Ending before the first start: nothing is started.
        let mut host = Script::new(vec![]).ending_from(0);
        assert_eq!(ends::supervise(&mut host), ends::NO_DAEMON);
        assert_eq!(host.tried, 0);
    }

    /// The launcher supervises exactly `run --supervisor task`, compared by
    /// equality: the launcher check's `--version`, the start task's `service
    /// start`, and every near miss are waited for once, as before P2.
    #[test]
    fn only_the_daemons_run_is_supervised() {
        let script = || Script::new(vec![crash(), stop()]);

        let mut host = script();
        assert_eq!(
            ends::launch(&["run", "--supervisor", "task"], None, &mut host),
            0
        );
        assert_eq!(host.ran(), 2);

        let once: [&[&str]; 9] = [
            &["--version"],
            &["service", "start"],
            &["run"],
            &["run", "--supervisor"],
            &["run", "--supervisor", "tasks"],
            &["run", "--supervisor", "scm"],
            &["run", "--supervisor", "task", "--encoder", "software"],
            &["RUN", "--supervisor", "task"],
            &[],
        ];
        for args in once {
            let mut host = script();
            assert_eq!(ends::launch(args, None, &mut host), 1, "{args:?}");
            assert_eq!(host.ran(), 1, "{args:?}");
            assert!(host.slept.is_empty(), "{args:?}");
        }

        // A verb whose start fails leaves at once with the launcher's code.
        let mut host = Script::new(vec![None]);
        assert_eq!(
            ends::launch(&["--version"], None, &mut host),
            ends::NO_DAEMON
        );
        assert_eq!(host.tried, 1);
    }

    /// FR-150 D4 — `ROOMLERDW_SUPERVISE=0` waits once; any other value, and
    /// none, keeps the loop.
    #[test]
    fn the_switch_waits_once() {
        let run = ["run", "--supervisor", "task"];
        let mut host = Script::new(vec![crash(), stop()]);
        assert_eq!(ends::launch(&run, Some(OsStr::new("0")), &mut host), 1);
        assert_eq!(host.ran(), 1);

        for value in [None, Some("1"), Some(""), Some("false"), Some("00")] {
            let mut host = Script::new(vec![crash(), stop()]);
            assert_eq!(
                ends::launch(&run, value.map(OsStr::new), &mut host),
                0,
                "{value:?}"
            );
            assert_eq!(host.ran(), 2, "{value:?}");
        }
        assert_eq!(ends::SUPERVISE_ENV, "ROOMLERDW_SUPERVISE");
    }
}
