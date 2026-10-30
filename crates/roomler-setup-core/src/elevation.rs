// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-129 C3 — the wizard asks for administrator rights itself.
//!
//! On Windows one install needs an elevated wizard: a reinstall over the
//! pre-logon service (SystemContext), whose settings sit in a folder only
//! SYSTEM and Administrators may open (#3073, #3116). Before C3 the person
//! had to close the wizard and choose "Run as administrator" from its context
//! menu. Now the wizard starts its own image again through the shell's
//! `runas` verb — one UAC prompt — and the limited copy exits.
//!
//! The mechanics are here, beside the MSI runner's, because this crate owns
//! every Win32 call the wizard makes (its Cargo.toml says why):
//!
//! - [`run_elevated`]: `ShellExecuteExW` with `runas`, the declined prompt
//!   (`ERROR_CANCELLED`) told apart from every other failure, because the two
//!   mean different things to the person — "you said no" leaves the wizard as
//!   it was, anything else is a fault to show.
//! - [`wait_for_exit`]: the elevated copy waits, bounded, for the limited one
//!   to be gone before it takes the single-instance lock — otherwise it would
//!   find the dying copy's lock, hand its arguments to that copy and quit.
//! - [`command_line`]: one parameter string with the C runtime's quoting
//!   rules, so what the elevated copy reads back is exactly what was sent.

use std::path::Path;
use std::time::Duration;

/// Why an elevated start did not happen.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ElevationError {
    /// The person declined the UAC prompt (`ERROR_CANCELLED`, 1223).
    #[error("the administrator prompt was cancelled")]
    Cancelled,
    /// Anything else, as text to show.
    #[error("{0}")]
    Failed(String),
}

/// What [`wait_for_exit`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The process exited (or was gone already) within the bound.
    Exited,
    /// It was still running when the bound ran out.
    TimedOut,
    /// Neither a handle nor the process list could be had: nothing to wait
    /// on, so the caller goes ahead.
    Unknown,
}

/// The Win32 code of a declined UAC prompt.
pub const ERROR_CANCELLED: u32 = 1223;

/// Join `args` into one command-line string that the Microsoft C runtime's
/// parser (`CommandLineToArgvW`, Rust's `std::env::args`) splits back into
/// exactly `args`: an argument with a space, a tab, a quote or nothing in it
/// is quoted, a quote inside is escaped, and backslashes are doubled only
/// where they precede a quote.
pub fn command_line(args: &[String]) -> String {
    args.iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Every backslash before a quote is doubled, and the quote
                // itself escaped.
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // Backslashes before the closing quote are doubled too.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Start `file` elevated with `args`, through the shell's `runas` verb: one
/// UAC prompt. Returns the new process's id once it has started.
///
/// `SEE_MASK_NOASYNC` because the caller exits right after a success, which
/// the shell asks of a caller that does; `SEE_MASK_NOZONECHECKS` because the
/// file is the wizard's own running image — its zone and SmartScreen check
/// ran when the person opened it, and a second warning on top of the UAC
/// prompt (which names the publisher) would be noise, not a check. Not for
/// any other file: callers pass `std::env::current_exe()`.
#[cfg(target_os = "windows")]
pub fn run_elevated(file: &Path, args: &[String]) -> Result<u32, ElevationError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::GetProcessId;
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SEE_MASK_NOZONECHECKS, SHELLEXECUTEINFOW,
        ShellExecuteExW,
    };

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }
    let verb = wide(std::ffi::OsStr::new("runas"));
    let file_w = wide(file.as_os_str());
    let params = wide(std::ffi::OsStr::new(&command_line(args)));

    // SAFETY: SHELLEXECUTEINFOW is plain data, valid zeroed; cbSize is its
    // size; every pointer is a local that outlives the call.
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_NOZONECHECKS;
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = file_w.as_ptr();
    sei.lpParameters = params.as_ptr();
    sei.nShow = 1; // SW_SHOWNORMAL

    // SAFETY: the struct is fully initialised above.
    let ok = unsafe { ShellExecuteExW(&mut sei) };
    if ok == 0 {
        // SAFETY: read straight after the failing call on this thread.
        let err = unsafe { GetLastError() };
        if err == ERROR_CANCELLED {
            return Err(ElevationError::Cancelled);
        }
        return Err(ElevationError::Failed(format!(
            "Windows could not start Roomler Setup as administrator (error {err})."
        )));
    }
    if sei.hProcess.is_null() {
        return Err(ElevationError::Failed(
            "Windows reported the administrator start, but no process came of it.".to_string(),
        ));
    }
    // SAFETY: hProcess is a valid handle from SEE_MASK_NOCLOSEPROCESS; it is
    // ours to close.
    let pid = unsafe { GetProcessId(sei.hProcess) };
    unsafe { CloseHandle(sei.hProcess) };
    Ok(pid)
}

/// Off Windows there is no UAC; the wizard's halves ask through `pkexec` or
/// the macOS administrator prompt instead, and nothing calls this.
#[cfg(not(target_os = "windows"))]
pub fn run_elevated(_file: &Path, _args: &[String]) -> Result<u32, ElevationError> {
    Err(ElevationError::Failed(
        "Restarting as administrator is a Windows step.".to_string(),
    ))
}

/// Wait until process `pid`, running the image named `image` (a file name:
/// the wizard passes its own, which may be a release's long name), has
/// exited, at most `timeout`.
///
/// A handle is the precise way: `OpenProcess(SYNCHRONIZE)` and a wait. Under
/// over-the-shoulder credentials (a standard user types an administrator's
/// password) the elevated copy runs as ANOTHER account, and the limited copy's
/// process may refuse it that handle; then the process list is polled
/// instead ([`poll_until_gone`]), which needs no access to the process.
#[cfg(target_os = "windows")]
pub fn wait_for_exit(pid: u32, image: &str, timeout: Duration) -> WaitOutcome {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    // The standard right, as msi_runner.rs spells it (windows-sys 0.59 has it
    // only under the file-system access rights).
    const SYNCHRONIZE: u32 = 0x0010_0000;

    // SAFETY: a plain OpenProcess; the handle is null-checked and closed.
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return poll_until_gone(pid, image, timeout);
    }
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: a valid handle, waited on once.
    let r = unsafe { WaitForSingleObject(handle, ms) };
    unsafe { CloseHandle(handle) };
    if r == WAIT_OBJECT_0 {
        WaitOutcome::Exited
    } else {
        WaitOutcome::TimedOut
    }
}

/// [`wait_for_exit`]'s fallback: poll the process list every 100 ms until no
/// process has id `pid` AND image `image` — a reused id belongs to another
/// image, so it is never waited on — or until `timeout`.
#[cfg(target_os = "windows")]
pub fn poll_until_gone(pid: u32, image: &str, timeout: Duration) -> WaitOutcome {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listed(pid, image) {
            None => return WaitOutcome::Unknown,
            Some(false) => return WaitOutcome::Exited,
            Some(true) if std::time::Instant::now() >= deadline => return WaitOutcome::TimedOut,
            Some(true) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Is a process with id `pid` and image `image` in the process list? `None`
/// when the list could not be read.
#[cfg(target_os = "windows")]
fn listed(pid: u32, image: &str) -> Option<bool> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: a snapshot handle, checked and closed below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: plain data, valid zeroed; dwSize set as the API requires.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut found = false;
    // SAFETY: a valid snapshot and a correctly sized entry, each call.
    let mut more = unsafe { Process32FirstW(snap, &mut entry) } != 0;
    while more {
        if entry.th32ProcessID == pid {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            found = String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case(image);
            break;
        }
        more = unsafe { Process32NextW(snap, &mut entry) } != 0;
    }
    // SAFETY: the snapshot handle from above.
    unsafe { CloseHandle(snap) };
    Some(found)
}

/// Nothing hands a process id across an elevation off Windows.
#[cfg(not(target_os = "windows"))]
pub fn wait_for_exit(_pid: u32, _image: &str, _timeout: Duration) -> WaitOutcome {
    WaitOutcome::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|a| a.to_string()).collect()
    }

    /// The wizard's own arguments carry no space or quote, and pass as they
    /// are.
    #[test]
    fn plain_arguments_are_joined_as_they_are() {
        assert_eq!(
            command_line(&s(&[
                "--role",
                "daemon-system",
                "--server",
                "https://roomler.example:8443/x?y=1",
                "--after-elevation",
                "4242"
            ])),
            "--role daemon-system --server https://roomler.example:8443/x?y=1 --after-elevation 4242"
        );
    }

    /// The C runtime's rules, case by case (the examples in Microsoft's
    /// "Parsing C command-line arguments", read backwards).
    #[test]
    fn quoting_follows_the_c_runtime_rules() {
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg("a\tb"), "\"a\tb\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg(r"C:\dir\"), r"C:\dir\");
        assert_eq!(quote_arg(r"C:\my dir\"), r#""C:\my dir\\""#);
        assert_eq!(quote_arg(r#"a\"b c"#), r#""a\\\"b c""#);
        assert_eq!(quote_arg(r"a\\b"), r"a\\b");
    }

    /// The elevated copy reads back exactly what was sent, through the same
    /// parser `std::env::args` uses on Windows.
    #[cfg(target_os = "windows")]
    #[test]
    fn what_is_sent_is_what_the_elevated_copy_reads() {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::UI::Shell::CommandLineToArgvW;
        let args = s(&[
            "--role",
            "daemon-system",
            "",
            "two words",
            r#"a "quoted" word"#,
            r"C:\trailing slash\",
            r"back\\slashes",
        ]);
        let line = format!("roomler-setup.exe {}", command_line(&args));
        let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
        let mut argc = 0i32;
        // SAFETY: a NUL-terminated buffer; the returned block is freed below.
        let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut argc) };
        assert!(!argv.is_null());
        let mut back = Vec::new();
        for i in 1..argc as usize {
            // SAFETY: argc entries, each a NUL-terminated wide string.
            let p = unsafe { *argv.add(i) };
            let len = (0..).take_while(|&j| unsafe { *p.add(j) } != 0).count();
            back.push(String::from_utf16_lossy(unsafe {
                std::slice::from_raw_parts(p, len)
            }));
        }
        unsafe { LocalFree(argv as _) };
        assert_eq!(back, args);
    }

    /// A long-ish child to wait on: `cmd` running a one-second ping.
    #[cfg(target_os = "windows")]
    fn a_second_long_child() -> std::process::Child {
        std::process::Command::new("cmd")
            .args(["/C", "ping -n 2 127.0.0.1 >NUL"])
            .spawn()
            .unwrap()
    }

    /// A process that has already exited is not waited on, and a running
    /// one is waited on until it ends.
    #[cfg(target_os = "windows")]
    #[test]
    fn wait_for_exit_follows_a_real_process() {
        let mut child = a_second_long_child();
        let pid = child.id();
        assert_eq!(
            wait_for_exit(pid, "cmd.exe", Duration::from_millis(50)),
            WaitOutcome::TimedOut
        );
        assert_eq!(
            wait_for_exit(pid, "cmd.exe", Duration::from_secs(30)),
            WaitOutcome::Exited
        );
        child.wait().unwrap();
        // A pid nobody runs any more: no wait at all.
        let started = std::time::Instant::now();
        let gone = wait_for_exit(pid, "cmd.exe", Duration::from_secs(30));
        assert_eq!(gone, WaitOutcome::Exited);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// The over-the-shoulder fallback, driven directly: the process list
    /// keeps the wait going while the process runs and ends it when the
    /// process is gone; a pid whose image is another program's (a reused id)
    /// is not waited on at all, and the image name compares without case.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_process_list_fallback_follows_a_real_process() {
        let mut child = a_second_long_child();
        let pid = child.id();
        assert_eq!(
            poll_until_gone(pid, "CMD.EXE", Duration::from_millis(250)),
            WaitOutcome::TimedOut
        );
        let started = std::time::Instant::now();
        assert_eq!(
            poll_until_gone(
                pid,
                "roomler-setup-0.4.190-x86_64-pc-windows-msvc.exe",
                Duration::from_secs(30)
            ),
            WaitOutcome::Exited,
            "another image under that id is not the wizard"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            poll_until_gone(pid, "cmd.exe", Duration::from_secs(30)),
            WaitOutcome::Exited
        );
        child.wait().unwrap();
    }
}
