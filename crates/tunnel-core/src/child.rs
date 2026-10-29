// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3080 — child processes that cannot be held up by another process's copy
//! of their pipe, and long-lived children that cannot hold anyone else's.
//!
//! macOS has no `pipe2`. Rust's std makes every pipe there with `pipe()` and
//! marks each end close-on-exec afterwards, in two more calls. A child spawned
//! on another thread inside that window inherits both ends, at the same
//! numbers, without the flag. A short-lived inheritor closes them when it
//! exits and nothing shows. A long-lived one keeps the write end open for as
//! long as it lives, and whoever reads the pipe waits for an EOF that does not
//! come. Linux is not exposed: its `pipe2(O_CLOEXEC)` is atomic.
//!
//! Field, 2026-10-29, on 0.4.184: netstate's `route -n monitor`, which lives
//! as long as the daemon, was spawned inside the window of the GUI-worker
//! supervisor's `/bin/ps`. `ps` exited, but the supervisor's
//! `Command::output()` read on, for good, on a runtime worker, and the Mac
//! never got its GUI worker: no capture and no input until a restart.
//!
//! One answer for each side of the race:
//! - [`output_bounded`] reads a short command's output until the command
//!   EXITS rather than until EOF, within a deadline, and always reaps it.
//! - [`inherit_only_stdio`] (and [`inherit_only_stdio_async`]) has a
//!   long-lived child mark every descriptor above stderr close-on-exec just
//!   before it execs, so it keeps no pipe but its own stdio.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long a read whose pipes have gone quiet waits before it checks again
/// whether the child has exited.
const EXIT_POLL: Duration = Duration::from_millis(20);

/// The highest descriptor [`inherit_only_stdio`] sweeps, whatever
/// `RLIMIT_NOFILE` says. An unlimited soft limit must not cost a child
/// billions of `fcntl` calls before it can exec.
const SWEEP_CAP: libc::c_int = 65_536;

/// The lowest ceiling of that sweep. A soft limit lowered after descriptors
/// above it were opened must not leave them out.
const SWEEP_FLOOR: libc::c_int = 1_024;

/// Run `cmd` to completion and collect its output, like
/// [`Command::output`], except in the two ways that matter when a pipe may
/// have leaked into some other process:
///
/// - Reading stops when the child EXITS, not at EOF. By then everything the
///   child wrote is in the pipe. An EOF that has not come is being held off by
///   another process with a copy of the write end, and may never come. Output
///   written after the child exits, by a background process it started for
///   instance, is not collected.
/// - A child still running at `deadline` is killed, and the error is
///   [`io::ErrorKind::TimedOut`].
///
/// Either way the child is reaped before this returns, so no call leaves a
/// zombie behind. stdin is `/dev/null`, as with `output()`; stdout and stderr
/// are captured. It blocks the calling thread: call it from a blocking
/// context, never on an async runtime's worker.
pub fn output_bounded(cmd: &mut Command, deadline: Duration) -> io::Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let collected = collect(&mut child, deadline);
    if collected.is_err() {
        // Ignored: the child may have exited by itself in the meantime.
        let _ = child.kill();
    }
    // A child `try_wait` saw exit is already reaped, and `wait` returns the
    // same status at once. A killed one is reaped here.
    let status = child.wait()?;
    let (stdout, stderr) = collected?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn collect(child: &mut Child, deadline: Duration) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut out = Stream::new(child.stdout.take());
    let mut err = Stream::new(child.stderr.take());
    let started = Instant::now();
    loop {
        // The exit first, then the read: a read that follows an observed exit
        // finds everything the child ever wrote.
        let exited = child.try_wait()?.is_some();
        out.drain()?;
        err.drain()?;
        if exited {
            return Ok((out.buf, err.buf));
        }
        let left = deadline.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the child did not exit within {deadline:?}"),
            ));
        }
        let open: Vec<RawFd> = [out.fd(), err.fd()].into_iter().flatten().collect();
        poll_readable(&open, left.min(EXIT_POLL))?;
    }
}

/// One captured stream: its pipe while it is open, and what came out of it.
struct Stream<R> {
    pipe: Option<R>,
    buf: Vec<u8>,
}

impl<R: Read + AsRawFd> Stream<R> {
    fn new(pipe: Option<R>) -> Self {
        Self {
            pipe,
            buf: Vec::new(),
        }
    }

    fn fd(&self) -> Option<RawFd> {
        self.pipe.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Read whatever the pipe holds now, without waiting for more. The pipe is
    /// dropped at EOF.
    fn drain(&mut self) -> io::Result<()> {
        let mut chunk = [0u8; 16 * 1024];
        while let Some(pipe) = self.pipe.as_mut() {
            // A pipe that polls readable has data or is at EOF, so this read
            // returns at once.
            if poll_readable(&[pipe.as_raw_fd()], Duration::ZERO)? == 0 {
                return Ok(());
            }
            match pipe.read(&mut chunk) {
                Ok(0) => self.pipe = None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// `poll(2)` for readability on `fds`, for at most `timeout`. Returns how many
/// are readable (a pipe at EOF counts). With no descriptors it only sleeps.
fn poll_readable(fds: &[RawFd], timeout: Duration) -> io::Result<usize> {
    if fds.is_empty() {
        std::thread::sleep(timeout);
        return Ok(0);
    }
    let mut pfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: `pfds` is a live array of `pfds.len()` initialised `pollfd`s.
    let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, ms) };
    if rc >= 0 {
        return Ok(rc as usize);
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::Interrupted {
        // A signal, not an answer: the caller looks again.
        Ok(0)
    } else {
        Err(e)
    }
}

/// Have the child mark every descriptor above stderr close-on-exec after the
/// fork and just before the exec, so the program keeps only the stdio this
/// `Command` gave it.
///
/// For a child that may outlive the moment it was spawned in: one that
/// inherits a pipe another thread had just made holds that pipe's write end
/// for its whole life (see the module docs). Sweeping in the child closes the
/// window from the child's side, whatever the parent's threads were doing at
/// the time.
///
/// ⚠️ It costs a `fork`, where std would otherwise `posix_spawn` (std falls
/// back to `fork` whenever a `pre_exec` hook is set), and up to [`SWEEP_CAP`]
/// `fcntl` calls in the child. So it is for the few children that live long,
/// not for every short command.
pub fn inherit_only_stdio(cmd: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    let ceiling = sweep_ceiling();
    // SAFETY: the hook runs in the child between `fork` and `exec`, where only
    // async-signal-safe calls are allowed. It makes `fcntl` calls and nothing
    // else: no allocation, no lock, nothing shared with the parent.
    unsafe {
        cmd.pre_exec(move || {
            mark_cloexec_above_stderr(ceiling);
            Ok(())
        })
    }
}

/// [`inherit_only_stdio`] for a `tokio::process::Command`.
pub fn inherit_only_stdio_async(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    let ceiling = sweep_ceiling();
    // SAFETY: as in `inherit_only_stdio`: `fcntl` calls only.
    unsafe {
        cmd.pre_exec(move || {
            mark_cloexec_above_stderr(ceiling);
            Ok(())
        })
    }
}

/// The sweep's upper bound, read in the parent: the soft `RLIMIT_NOFILE`,
/// kept within [`SWEEP_FLOOR`]..=[`SWEEP_CAP`].
fn sweep_ceiling() -> libc::c_int {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rl` is a valid out-pointer for one `rlimit`.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
        rl.rlim_cur
    } else {
        0
    };
    // `RLIM_INFINITY` is the largest value, so it lands on the cap like any
    // other limit too big to sweep.
    libc::c_int::try_from(soft)
        .unwrap_or(SWEEP_CAP)
        .clamp(SWEEP_FLOOR, SWEEP_CAP)
}

/// Async-signal-safe: `fcntl` only.
fn mark_cloexec_above_stderr(ceiling: libc::c_int) {
    for fd in 3..ceiling {
        // SAFETY: `F_GETFD` on a number that is not open answers EBADF, the
        // common case, which is skipped.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
            // SAFETY: `fd` is open: `F_GETFD` just answered for it.
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    /// A file the shell below writes its background holder's pid to.
    fn pid_file(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        std::env::temp_dir().join(format!(
            "roomler-child-{tag}-{}-{nanos}.pid",
            std::process::id()
        ))
    }

    /// #3080's end state: a shell starts a long-lived `sleep` that inherits
    /// its stdout, prints `done` and exits. The direct child is gone while
    /// another process still holds the write end of its stdout pipe.
    fn pinned(pid_file: &Path) -> Command {
        let mut c = Command::new("/bin/sh");
        c.arg("-c")
            .arg("sleep 30 & echo $! > \"$PIDFILE\"; echo done")
            .env("PIDFILE", pid_file);
        c
    }

    /// End the holder [`pinned`] started.
    fn end_holder(pid_file: &Path) {
        for _ in 0..250 {
            if let Some(pid) = std::fs::read_to_string(pid_file)
                .ok()
                .and_then(|s| s.trim().parse::<libc::pid_t>().ok())
            {
                // SAFETY: one signal to the `sleep` this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                let _ = std::fs::remove_file(pid_file);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the holder never wrote its pid to {}", pid_file.display());
    }

    /// The positive control: `Command::output()`, which the supervisor's
    /// `find_processes` called before #3080, waits for the holder.
    #[test]
    fn std_output_waits_as_long_as_another_process_holds_the_pipe() {
        let pid_file = pid_file("std");
        let mut cmd = pinned(&pid_file);
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _ = tx.send(cmd.output());
        });
        let early = rx.recv_timeout(Duration::from_millis(1500));
        end_holder(&pid_file);
        assert!(
            early.is_err(),
            "output() returned while the holder lived: {early:?}"
        );
        let late = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("output() returns once the holder is gone")
            .expect("ran");
        assert_eq!(late.stdout, b"done\n");
        reader.join().unwrap();
    }

    #[test]
    fn output_bounded_returns_when_the_child_exits_though_its_pipe_is_held() {
        let pid_file = pid_file("bounded");
        let started = Instant::now();
        let out = output_bounded(&mut pinned(&pid_file), Duration::from_secs(10));
        let took = started.elapsed();
        end_holder(&pid_file);
        let out = out.expect("ran");
        assert!(out.status.success());
        assert_eq!(out.stdout, b"done\n");
        assert!(
            took < Duration::from_secs(5),
            "waited {took:?} on the holder"
        );
    }

    #[test]
    fn output_bounded_keeps_what_output_returns_for_an_ordinary_command() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("echo out; echo err >&2; exit 3");
        let out = output_bounded(&mut cmd, Duration::from_secs(10)).expect("ran");
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
        assert_eq!(out.status.code(), Some(3));
    }

    /// More than a pipe holds (64 KiB on both systems), so the child would
    /// stall on a full pipe unless it is read while it runs.
    #[test]
    fn output_bounded_reads_more_than_a_pipe_holds() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("head -c 300000 /dev/zero");
        let out = output_bounded(&mut cmd, Duration::from_secs(10)).expect("ran");
        assert_eq!(out.stdout.len(), 300_000);
        assert!(out.status.success());
    }

    #[test]
    fn output_bounded_kills_a_child_past_its_deadline() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30");
        let started = Instant::now();
        let e = output_bounded(&mut cmd, Duration::from_millis(300)).expect_err("timed out");
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// Whether the program a child execs still has descriptor 9 open, when the
    /// child got 9 the way the race hands one over: a copy made after the fork,
    /// open and without close-on-exec. The sweep, when asked for, is set after
    /// that copy, so it finds 9 open, as it would find an inherited pipe.
    fn fd9_after_exec(sweep: bool) -> String {
        let mut cmd = Command::new("/bin/sh");
        // `true`, not `:`: a redirection error on a special builtin would end
        // the shell before it could answer.
        cmd.arg("-c")
            .arg("if true 2>/dev/null >&9; then echo open; else echo closed; fi");
        // SAFETY: `dup2` only, which is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                if libc::dup2(2, 9) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        if sweep {
            inherit_only_stdio(&mut cmd);
        }
        let out = output_bounded(&mut cmd, Duration::from_secs(10)).expect("ran");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// The positive control: without the sweep, the descriptor survives the
    /// exec.
    #[test]
    fn a_child_keeps_a_descriptor_it_inherits_without_close_on_exec() {
        assert_eq!(fd9_after_exec(false), "open");
    }

    #[test]
    fn inherit_only_stdio_leaves_the_program_only_its_stdio() {
        assert_eq!(fd9_after_exec(true), "closed");
    }

    #[tokio::test]
    async fn inherit_only_stdio_async_sweeps_the_same_way() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("if true 2>/dev/null >&9; then echo open; else echo closed; fi");
        // SAFETY: `dup2` only, which is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                if libc::dup2(2, 9) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        inherit_only_stdio_async(&mut cmd);
        let out = cmd.output().await.expect("ran");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "closed");
    }

    #[test]
    fn the_sweep_ceiling_stays_within_its_bounds() {
        let c = sweep_ceiling();
        assert!((SWEEP_FLOOR..=SWEEP_CAP).contains(&c), "{c}");
    }
}
