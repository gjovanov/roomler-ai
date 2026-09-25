// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-84 D6 — start the desktop companion from a USER-CONTEXT daemon without
//! handing it a single handle of ours.
//!
//! #1035: tunnel listener ports stayed bound for hours after the daemon
//! process that bound them was gone, and killing `roomler-desktop` freed them
//! within seconds. The companion's parent was that dead daemon. The
//! user-session respawn used `std::process::Command`, which calls
//! `CreateProcessW` with `bInheritHandles = TRUE`: every handle the daemon
//! had marked inheritable at that instant — and there is a window in which
//! some are (`make_pipe` below, the SSH console-user capture, third-party
//! code) — was copied into a GUI app that lives for months. The SYSTEM path
//! (`supervisor::spawn_in_session`) already passes `FALSE`; this is the same
//! guarantee for the other path.
//!
//! What this passes, and why:
//! * `bInheritHandles = FALSE` — the whole point; no `STARTF_USESTDHANDLES`
//!   (which would require inheritance), so the child gets no std handles.
//! * `DETACHED_PROCESS` — a console-subsystem build (debug) gets no console
//!   of ours; a GUI build ignores it.
//! * `CREATE_NEW_PROCESS_GROUP` — a Ctrl+C aimed at an attended `roomlerd
//!   run` does not reach the tray.
//! * `CREATE_BREAKAWAY_FROM_JOB`, when the job allows it — the per-user
//!   Scheduled Task runs the daemon inside a job, and a companion left in it
//!   dies with the task (an update, `schtasks /End`). Retried without it
//!   when the job forbids breakaway (`ERROR_ACCESS_DENIED`).

#![cfg(target_os = "windows")]

use anyhow::Result;
use std::path::Path;

/// Quote one argument so `CommandLineToArgvW` (and the MSVC CRT) read it
/// back unchanged.
pub fn quote_arg(arg: &str) -> String {
    // STUB (RED stage).
    arg.to_string()
}

/// The full command line: the quoted EXE, then each argument quoted as
/// needed.
pub fn command_line(exe: &Path, args: &[&str]) -> String {
    // STUB (RED stage): the naive join `spawn_in_session` does.
    let mut s = format!("\"{}\"", exe.display());
    for a in args {
        s.push(' ');
        s.push_str(a);
    }
    s
}

/// Start `exe args…` detached, inheriting NO handle. Returns the child's pid.
pub fn spawn_detached_no_inherit(exe: &Path, args: &[&str]) -> Result<u32> {
    // STUB (RED stage): exactly what the user-session respawn does today.
    let child = std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_survive_the_windows_parser() {
        let table: &[(&str, &str)] = &[
            ("--first-run", "--first-run"),
            ("a b", r#""a b""#),
            ("", r#""""#),
            (r#"he said "hi""#, r#""he said \"hi\"""#),
            // A trailing backslash in a QUOTED argument must be doubled, or it
            // escapes the closing quote and swallows the next argument.
            (r"C:\dir with space\", r#""C:\dir with space\\""#),
            // Backslashes not followed by a quote are literal.
            (r"C:\plain\path", r"C:\plain\path"),
            ("tab\there", "\"tab\there\""),
        ];
        for (arg, want) in table {
            assert_eq!(quote_arg(arg), *want, "quoting {arg:?}");
        }
    }

    #[test]
    fn command_line_quotes_the_exe_and_each_argument() {
        let exe = Path::new(r"C:\Program Files\Roomler\roomler-desktop.exe");
        assert_eq!(
            command_line(exe, &["--first-run"]),
            r#""C:\Program Files\Roomler\roomler-desktop.exe" --first-run"#
        );
        assert_eq!(
            command_line(exe, &["--view=welcome", "two words"]),
            r#""C:\Program Files\Roomler\roomler-desktop.exe" --view=welcome "two words""#
        );
        assert_eq!(
            command_line(exe, &[]),
            r#""C:\Program Files\Roomler\roomler-desktop.exe""#
        );
    }

    /// #1035 — the regression test. An INHERITABLE pipe is open in this
    /// process when the child is started; the parent then closes its write
    /// end and reads. With no inheritance the read sees end-of-file at once;
    /// if the child got a copy of the write handle, the read blocks until the
    /// child exits (~3 s here). `std::process::Command` — the pre-D6 spawn —
    /// fails this.
    #[test]
    fn the_spawned_child_holds_no_handle_of_ours() {
        use std::sync::mpsc;
        use std::time::Duration;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, TRUE};
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        use windows_sys::Win32::System::Pipes::CreatePipe;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };

        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: TRUE,
        };
        let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: out-pointers are valid; `sa` outlives the call.
        assert_ne!(unsafe { CreatePipe(&mut read, &mut write, &mut sa, 0) }, 0);

        // A child that lives ~3 s and never touches the pipe.
        let ping = std::path::PathBuf::from(
            std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
        )
        .join("System32")
        .join("PING.EXE");
        let pid = spawn_detached_no_inherit(&ping, &["-n", "4", "127.0.0.1"])
            .expect("spawning the probe child");

        // SAFETY: we own `write`; closing it leaves only copies a child took.
        unsafe { CloseHandle(write) };
        let read_raw = read as usize;
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 16];
            let mut n = 0u32;
            // SAFETY: `read_raw` is our live read handle; buffer is valid.
            let ok = unsafe {
                ReadFile(
                    read_raw as HANDLE,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            let _ = tx.send((ok, n));
        });
        let eof_at_once = rx.recv_timeout(Duration::from_millis(1500));

        // Clean up the child either way, then the reader.
        // SAFETY: plain process-handle open/terminate/close on our own child.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 0);
                CloseHandle(h);
            }
        }
        let _ = reader.join();
        // SAFETY: we own `read`.
        unsafe { CloseHandle(read) };

        match eof_at_once {
            Ok((ok, n)) => assert!(
                ok == 0 && n == 0,
                "the read must end in end-of-file (broken pipe), got ok={ok} n={n}"
            ),
            Err(_) => panic!(
                "the pipe stayed open after the parent closed its end: the child \
                 inherited our write handle (#1035)"
            ),
        }
    }
}
