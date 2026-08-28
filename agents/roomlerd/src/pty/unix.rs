//! The Unix half of [`crate::pty`] — `openpty`, a login shell, and two pumps.
//!
//! # Why `openpty` and not `posix_openpt`
//!
//! The `posix_openpt` + `grantpt` + `unlockpt` + `ptsname_r` sequence needs a
//! platform split: `ptsname_r` does not exist on macOS. `openpty` is one call,
//! is declared for both Linux and Apple in `libc`, and on glibc ≥ 2.34 lives in
//! `libc.so.6` itself so no `-lutil` link flag is needed (our Linux targets are
//! ubuntu-22.04 / glibc 2.35 and newer). Fewer moving parts across a platform
//! boundary that CI compiles only at release-tag time — and one of the two
//! remaining `libc` calls here still managed to differ (see `open_pty`).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use tokio::io::unix::AsyncFd;
use tracing::{debug, warn};

use super::WinSize;

/// A pseudo-terminal with a process attached to its slave side.
///
/// Dropping this kills the process group — see [`PtyProcess::kill_group`] for
/// why that is the group and not just the child.
#[derive(Debug)]
pub struct PtyProcess {
    /// The master side, non-blocking, shared by the read and write pumps.
    /// `AsyncFd` permits concurrent readable/writable interest on one fd,
    /// which is exactly the two-pump shape an interactive session needs.
    master: Arc<AsyncFd<OwnedFd>>,
    child: tokio::process::Child,
    /// The child is a session leader (it called `setsid`), so its pid IS the
    /// process-group id. Kept separately because `child.id()` returns `None`
    /// once the child has been reaped.
    pgid: libc::pid_t,
}

/// Open a pty pair. Returns `(master, slave)` as owned fds.
fn open_pty(size: WinSize) -> io::Result<(OwnedFd, OwnedFd)> {
    let size = size.sane();
    // ⚠️ `mut`, and `*mut` pointers below, because the two platforms declare
    // this differently: Linux takes `termp`/`winp` as `*const`, macOS as
    // `*mut`. A `*mut` weakens to `*const` implicitly, so passing `*mut`
    // satisfies BOTH; passing `*const` compiles on Linux and fails on macOS —
    // which CI would never catch, since macOS only builds at release-tag time.
    // (It didn't: this shape broke the rc.422 release and nothing before it.)
    let mut ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // SAFETY: both out-params are valid for writes; the name pointer is null
    // (we never need the slave's path) and the termios pointer is null
    // (kernel defaults are what a login shell expects).
    //
    // ⚠️ The `allow` is load-bearing, not laziness. On LINUX these really are
    // `*const`, so clippy is right that `&mut` is unnecessary — and taking its
    // advice reintroduces the exact bug that broke the rc.422 and rc.423
    // releases, because on macOS they are `*mut`. Passing `*mut` satisfies both
    // (it weakens implicitly); passing `*const` compiles here and fails there,
    // where CI never looks.
    #[allow(clippy::unnecessary_mut_passed)]
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded, so both are fresh fds this process owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

/// Which shell to run when the client asked for a login shell.
///
/// `$SHELL` is deliberately NOT consulted: it describes the DAEMON's
/// environment (root's, or SYSTEM's), not the session's, and honouring it would
/// mean a session's shell depends on how the service happened to be started.
fn default_shell() -> String {
    for candidate in ["/bin/bash", "/bin/sh"] {
        if std::path::Path::new(candidate).exists() {
            return candidate.to_string();
        }
    }
    "/bin/sh".to_string()
}

impl PtyProcess {
    /// Spawn a login shell (or `shell -c <command>`) attached to a new pty.
    ///
    /// `run_as` goes through the same [`crate::exec::apply_run_as`] as every
    /// other execution path, so the identity rules — and every refusal, such as
    /// `console_user` being a Windows-only concept — are identical here. There
    /// is deliberately no separate privilege story for interactive sessions.
    pub fn spawn(
        command: Option<&str>,
        term: &str,
        size: WinSize,
        run_as: &crate::exec::RunAs,
    ) -> Result<Self, String> {
        let (master, slave) = open_pty(size).map_err(|e| format!("could not open a pty: {e}"))?;

        let shell = default_shell();
        let mut cmd = tokio::process::Command::new(&shell);
        match command {
            // `-c` matches what sshd does for `ssh -t <node> <cmd>`.
            Some(c) => {
                cmd.arg("-c").arg(c);
            }
            // A leading `-` in argv[0] is how a shell is told it is a LOGIN
            // shell, which is what makes it read the profile files an operator
            // expects to have run.
            None => {
                let base = std::path::Path::new(&shell)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "sh".into());
                cmd.arg0(format!("-{base}"));
            }
        }

        // The three standard fds all point at the slave; that is what makes it
        // a terminal rather than three pipes. `Stdio::from` dup2s them into
        // place, so the pre_exec closure below does not have to.
        let dup = |what: &str| -> Result<std::process::Stdio, String> {
            slave
                .try_clone()
                .map(std::process::Stdio::from)
                .map_err(|e| format!("could not duplicate the pty slave for {what}: {e}"))
        };
        cmd.stdin(dup("stdin")?)
            .stdout(dup("stdout")?)
            .stderr(dup("stderr")?);

        // TERM is the one environment variable an interactive session cannot
        // do without — without it, anything curses-based refuses to draw.
        cmd.env(
            "TERM",
            if term.is_empty() {
                "xterm-256color"
            } else {
                term
            },
        );

        // Registered BEFORE `apply_run_as`'s own pre_exec so the terminal is
        // claimed while still privileged. Not strictly required (TIOCSCTTY
        // needs session leadership, not root) but it is the conventional order
        // and it keeps the drop as the last thing that happens before exec.
        //
        // SAFETY: both calls are async-signal-safe syscalls with no allocation
        // and no locks, which is the whole contract for `pre_exec`.
        unsafe {
            cmd.pre_exec(|| {
                // A controlling terminal can only be claimed by a session
                // leader, and the child inherits the daemon's session
                // otherwise — in which case Ctrl-C would signal the DAEMON's
                // process group rather than the session's.
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                // fd 0 is the slave (dup2'd above). `as _` because the request
                // argument is c_ulong on Linux and c_ulong-but-differently on
                // macOS — the same platform-width care the privilege drop
                // needed, and the same reason: macOS compiles at tag time only.
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        crate::exec::apply_run_as(&mut cmd, run_as)?;

        let child = cmd
            .spawn()
            .map_err(|e| format!("could not start {shell}: {e}"))?;

        // The PARENT must drop the slave. While any process holds it open,
        // reads on the master never report end-of-file — so a session whose
        // shell had exited would hang forever instead of closing. This is the
        // single easiest thing to get wrong in a pty implementation.
        drop(slave);

        let pgid = child.id().unwrap_or(0) as libc::pid_t;

        // Non-blocking is what makes `AsyncFd` legal: readiness tells us a read
        // will not block, and a blocking fd would stall a runtime worker if
        // that turned out to be wrong (spurious wakeups are permitted).
        set_nonblocking(master.as_raw_fd())
            .map_err(|e| format!("could not make the pty master non-blocking: {e}"))?;
        let master = Arc::new(
            AsyncFd::new(master).map_err(|e| format!("could not register the pty master: {e}"))?,
        );

        debug!(%shell, pgid, cols = size.cols, rows = size.rows, "ssh: pty session started");
        Ok(Self {
            master,
            child,
            pgid,
        })
    }

    /// A handle for writing client keystrokes into the terminal and resizing
    /// it, usable from a different task than the output pump.
    pub fn handle(&self) -> PtyHandle {
        PtyHandle {
            master: self.master.clone(),
        }
    }

    /// Read one chunk of terminal output. `Ok(0)` means the session ended.
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.master.readable().await?;
            match guard.try_io(|inner| raw_read(inner.as_raw_fd(), buf)) {
                Ok(Ok(n)) => return Ok(n),
                // ⚠️ Linux reports EIO — not EOF — on a master whose slave has
                // been closed by the last process holding it. Treating that as
                // an error would surface a normal shell exit as a transport
                // failure on every single session.
                Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    /// Wait for the attached process and return its exit status in the form
    /// SSH carries (unsigned). A signal death maps to the shell convention
    /// 128+n rather than to 0, which is what `exit_status` would otherwise
    /// report for a killed process.
    pub async fn wait(&mut self) -> u32 {
        match self.child.wait().await {
            Ok(status) => {
                if let Some(code) = status.code() {
                    code.clamp(0, 255) as u32
                } else {
                    use std::os::unix::process::ExitStatusExt;
                    status.signal().map(|s| 128 + s as u32).unwrap_or(1)
                }
            }
            Err(e) => {
                warn!(%e, "ssh: could not reap the pty session's process");
                1
            }
        }
    }

    /// Signal the whole process group, not just the shell.
    ///
    /// The shell is a session leader with children of its own; killing only it
    /// would orphan whatever it was running — a `tail -f` or an editor that
    /// keeps the pty alive and the session's resources with it. SIGHUP is the
    /// signal a terminal hangup actually delivers, so a shell handles it the
    /// way it was designed to.
    pub fn kill_group(&self) {
        if self.pgid > 0 {
            // SAFETY: a plain kill(2) on a pid we started. A stale pgid can
            // only fail with ESRCH, never reach an unrelated process — the pid
            // is not reaped until `wait`, so it cannot have been recycled.
            unsafe {
                libc::killpg(self.pgid, libc::SIGHUP);
            }
        }
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        // A dropped session must not leave a shell running against a master
        // nobody reads — that is a process leak reachable from the network.
        self.kill_group();
    }
}

/// Write and resize half of a [`PtyProcess`], separable so the input pump and
/// the output pump can run as independent tasks.
#[derive(Clone)]
pub struct PtyHandle {
    master: Arc<AsyncFd<OwnedFd>>,
}

impl PtyHandle {
    /// Write client input into the terminal, looping until all of it lands —
    /// a short write on a pty is normal when the line discipline's buffer is
    /// full, and dropping the remainder would silently eat keystrokes.
    pub async fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let mut guard = self.master.writable().await?;
            match guard.try_io(|inner| raw_write(inner.as_raw_fd(), buf)) {
                Ok(Ok(0)) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(Ok(n)) => buf = &buf[n..],
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// Tell the terminal its new geometry.
    ///
    /// This is what makes a resized client window reflow rather than keep
    /// drawing at the old size; the kernel also sends SIGWINCH to the
    /// foreground group, which is how applications learn to redraw.
    pub fn resize(&self, size: WinSize) -> io::Result<()> {
        let size = size.sane();
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: a well-formed winsize for an fd this struct owns.
        let rc = unsafe {
            libc::ioctl(
                self.master.as_raw_fd(),
                libc::TIOCSWINSZ as _,
                std::ptr::addr_of!(ws),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl on an fd we own.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn raw_read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for `buf.len()` writes.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn raw_write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for `buf.len()` reads.
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_pty_hands_back_two_usable_fds() {
        let (master, slave) = open_pty(WinSize { cols: 80, rows: 24 }).expect("a pty pair");
        assert!(master.as_raw_fd() >= 0);
        assert!(slave.as_raw_fd() >= 0);
        assert_ne!(master.as_raw_fd(), slave.as_raw_fd());
    }

    /// The geometry asked for is the geometry the terminal reports — this is
    /// what `stty size` inside the session reads.
    #[tokio::test]
    async fn a_session_sees_the_geometry_it_was_given() {
        let mut pty = PtyProcess::spawn(
            Some("stty size"),
            "xterm",
            WinSize {
                cols: 132,
                rows: 43,
            },
            &crate::exec::RunAs::Daemon,
        )
        .expect("a pty session");

        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match pty.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
        let _ = pty.wait().await;
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("43 132"),
            "the session should see 43 rows by 132 cols, got {text:?}"
        );
    }

    /// The whole point of the module: output arrives as it is produced, and the
    /// session ends cleanly when the shell exits.
    #[tokio::test]
    async fn a_session_streams_output_and_reports_its_exit_status() {
        let mut pty = PtyProcess::spawn(
            Some("echo hello-from-the-tty; exit 7"),
            "xterm",
            WinSize { cols: 80, rows: 24 },
            &crate::exec::RunAs::Daemon,
        )
        .expect("a pty session");

        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match pty.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
        assert!(
            String::from_utf8_lossy(&out).contains("hello-from-the-tty"),
            "expected the command's output, got {:?}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(pty.wait().await, 7, "the shell's exit code must survive");
    }

    /// Input written to the handle reaches the shell's stdin. Proves the two
    /// halves can be driven from separate tasks, which is how a real session
    /// runs.
    #[tokio::test]
    async fn keystrokes_written_to_the_handle_reach_the_shell() {
        let mut pty = PtyProcess::spawn(
            None,
            "xterm",
            WinSize { cols: 80, rows: 24 },
            &crate::exec::RunAs::Daemon,
        )
        .expect("a pty session");
        let handle = pty.handle();

        handle
            .write_all(b"echo marker-9271\nexit\n")
            .await
            .expect("writing to the terminal");

        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match pty.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
        let _ = pty.wait().await;
        assert!(
            String::from_utf8_lossy(&out).contains("marker-9271"),
            "the shell should have run what we typed, got {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    /// Resizing a live terminal is accepted; a session that outlives a client
    /// window change must not fail.
    #[tokio::test]
    async fn a_live_terminal_can_be_resized() {
        let mut pty = PtyProcess::spawn(
            None,
            "xterm",
            WinSize { cols: 80, rows: 24 },
            &crate::exec::RunAs::Daemon,
        )
        .expect("a pty session");
        pty.handle()
            .resize(WinSize {
                cols: 200,
                rows: 60,
            })
            .expect("resizing a live terminal");
        pty.kill_group();
        let _ = pty.wait().await;
    }

    /// `console_user` is a Windows concept, and the PTY path must refuse it for
    /// exactly the same reason and with the same words as every other execution
    /// path — one identity model, not two.
    #[test]
    fn the_identity_rules_are_the_same_ones_exec_enforces() {
        let err = PtyProcess::spawn(
            None,
            "xterm",
            WinSize { cols: 80, rows: 24 },
            &crate::exec::RunAs::ConsoleUser,
        )
        .expect_err("console_user cannot be honoured on unix");
        assert!(
            err.contains("console_user"),
            "the refusal must name the mode, got {err:?}"
        );
    }
}
