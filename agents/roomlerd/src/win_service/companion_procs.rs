// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #1686 — find a running companion by the file it was STARTED from, never by
//! its image name.
//!
//! On the reporting host an old companion kept running through an update from
//! an image that had been unlinked into `$Extend\$Deleted`. Windows then lists
//! the process under that entry's name — a file reference plus four random
//! bytes, `00240000000AA4A74B077E87` — and `tasklist /FI "IMAGENAME eq
//! roomler-desktop.exe"` matches nothing, so every name-based check
//! (`tasklist`, `taskkill /IM`) was blind to it: the refresh logged
//! `respawned=false`, the old companion kept the single-instance lock, and
//! every launch of the new one exited 0. How the running image got unlinked
//! is not reproduced — for an ordinary or elevated caller Windows refuses a
//! delete, a replace-rename and a raw POSIX-disposition delete of a mapped
//! image alike — but the two refreshers racing on that host (the SCM host and
//! the elevated worker, each renaming and deleting the other's files) are in
//! the logs, and a name check has no business deciding either way.
//!
//! The loader's record of the main module does not follow the file: the first
//! entry of the process's module list keeps answering the path the process
//! was started from (`C:\Program Files\Roomler\roomler-desktop.exe`) — as WMI's
//! `ExecutablePath` (the PEB's `ImagePathName`) did for that very process.
//! That is the identity used here (see [`started_from`] for the one API trap).
//! The image name only pre-filters which processes are worth opening: the
//! companion's own name, a renamed-aside `…exe.old`, or an extension-less
//! `$Deleted` entry name.
//!
//! #3885 — the per-user clean-up (`install_cleanup.rs`) stops the daemon of the
//! account it cleans through the same walk: [`read_named`] lists the processes
//! under a set of image names with what this process can read of each (its
//! account, the file it was started from, its image now), and
//! [`terminate_checked`] ends a pid only when the process that pid names now
//! still checks out, read through the handle that ends it. [`find`] and
//! [`terminate_verified`] are the companion's questions over the same calls.

#![cfg(target_os = "windows")]

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, FALSE, GetLastError, HANDLE, HMODULE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::{K32EnumProcessModules, K32GetModuleFileNameExW};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, PROCESS_VM_READ, QueryFullProcessImageNameW,
    TerminateProcess, WaitForSingleObject,
};

/// A running process started from the companion's path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompanionProcess {
    pub pid: u32,
    /// Terminal Services session; `None` when the lookup was refused.
    pub session: Option<u32>,
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: we own this handle and close it exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Every running process whose main module was loaded from `exe`
/// (case-insensitive, `\\?\` prefix ignored). The caller's own process is
/// never included. An unreadable process (access denied, protected) is
/// skipped: this answers "which companions can we see", and a companion is an
/// ordinary user-session process every caller of this can open.
pub fn find(exe: &Path) -> Vec<CompanionProcess> {
    let want = normalize(exe);
    let own = std::process::id();
    let file_name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mut out = Vec::new();
    for entry in snapshot().unwrap_or_default() {
        let pid = entry.pid;
        if pid == own || pid == 0 || !worth_opening(&entry.name, &file_name) {
            continue;
        }
        if let Ok(h) = open(pid, 0)
            && started_from(h.0).is_some_and(|path| normalize(&path) == want)
        {
            out.push(CompanionProcess {
                pid,
                session: session_of(pid),
            });
        }
    }
    out
}

/// Stop each pid that is STILL a process started from `exe`, and wait (up to
/// `timeout` in total) for it to be gone. Returns the pids confirmed exited.
///
/// The check and the kill go through ONE handle: a pid is only a number, and
/// between [`find`] and here (a `spawn_blocking` hop at least) the process can
/// exit and the pid be reused. Run as SYSTEM, `PROCESS_TERMINATE` opens almost
/// anything, so a kill by a bare pid could take an unrelated process down.
/// Holding the handle pins the process object; re-verifying its started-from
/// path on that same handle makes "the process we verified" and "the process
/// we kill" the same object. A pid that no longer verifies is left alone and
/// not counted. [`terminate_checked`] does it, with the started-from path as
/// the check.
pub fn terminate_verified(exe: &Path, pids: &[u32], timeout: Duration) -> Vec<u32> {
    let want = normalize(exe);
    let deadline = std::time::Instant::now() + timeout;
    let is_it = |seen: &Seen| {
        seen.started_from
            .as_deref()
            .is_some_and(|path| normalize(path) == want)
    };
    let mut gone = Vec::new();
    for &pid in pids {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if terminate_checked(pid, left, &is_it) == Ended::Stopped {
            gone.push(pid);
        }
    }
    gone
}

/// Image names worth opening: the companion's own, a renamed-aside copy
/// (`roomler-desktop.exe.old`), and an extension-less name — what Windows
/// reports once the file behind a running image was POSIX-deleted into
/// `$Extend\$Deleted` (a bare file id such as `00240000000AA4A74B077E87`).
pub(crate) fn worth_opening(image: &str, companion_file_name: &str) -> bool {
    let image = image.to_ascii_lowercase();
    image == companion_file_name
        || image.starts_with(companion_file_name)
        || (!image.contains('.') && !image.is_empty())
}

/// #3885 — one process of a snapshot: its pid, its parent's, and the name it
/// is listed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) pid: u32,
    pub(crate) parent: u32,
    pub(crate) name: String,
}

/// Every running process, from a ToolHelp snapshot. `None` when Windows
/// refuses the snapshot.
fn snapshot() -> Option<Vec<Entry>> {
    // SAFETY: TH32CS_SNAPPROCESS + pid 0 is the documented call form.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return None;
    }
    let _guard = OwnedHandle(snap);
    // SAFETY: zeroed POD, then the documented dwSize init.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut out = Vec::new();
    // SAFETY: `snap` is valid; `entry` lives across the calls.
    if unsafe { Process32FirstW(snap, &mut entry) } == 0 {
        return Some(out);
    }
    loop {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        out.push(Entry {
            pid: entry.th32ProcessID,
            parent: entry.th32ParentProcessID,
            name: String::from_utf16_lossy(&entry.szExeFile[..len]),
        });
        // SAFETY: as above.
        if unsafe { Process32NextW(snap, &mut entry) } == 0 {
            break;
        }
    }
    Some(out)
}

/// #3885 — the pid of the process that started `pid`, as a snapshot lists it.
/// `None` when the snapshot is refused or does not list `pid`.
pub(crate) fn parent_of(pid: u32) -> Option<u32> {
    snapshot()?
        .into_iter()
        .find(|entry| entry.pid == pid)
        .map(|entry| entry.parent)
}

/// Open `pid` with `extra` access, and with what reads its module list
/// (`PROCESS_QUERY_INFORMATION | PROCESS_VM_READ`) where Windows grants that,
/// else with the limited query it grants more often, which reads its account
/// and image but not its module list. `Err`: the Win32 error of the refusal,
/// `ERROR_INVALID_PARAMETER` when no process has that pid.
fn open(pid: u32, extra: u32) -> Result<OwnedHandle, u32> {
    for read in [
        PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
        PROCESS_QUERY_LIMITED_INFORMATION,
    ] {
        // SAFETY: plain OpenProcess; the handle is owned below.
        let h = unsafe { OpenProcess(read | extra, FALSE, pid) };
        if !h.is_null() {
            return Ok(OwnedHandle(h));
        }
    }
    // SAFETY: a thread-local read, right after the refused call.
    Err(unsafe { GetLastError() })
}

/// Has the process behind `h` (opened with `PROCESS_SYNCHRONIZE`) exited?
fn exited(h: &OwnedHandle) -> bool {
    // SAFETY: `h` is a live handle with SYNCHRONIZE; a zero wait only polls.
    unsafe { WaitForSingleObject(h.0, 0) == WAIT_OBJECT_0 }
}

/// What this process can read of `pid` through `h`.
fn read(pid: u32, name: &str, h: &OwnedHandle) -> Seen {
    Seen {
        pid,
        name: name.to_string(),
        user: crate::win_token::user_sid_of(h.0),
        started_from: started_from(h.0),
        image: image_path(h.0),
    }
}

/// The path the process's main module was loaded from — the loader's
/// module-list entry, which a later rename of the file does not change — on a
/// handle with at least `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ`.
///
/// ⚠️ With an EXPLICIT module handle, not `NULL`. Measured after renaming a
/// running image `X.exe` → `X.exe.old`: `K32GetModuleFileNameExW(h, NULL)` and
/// `QueryFullProcessImageNameW` both answer `X.exe.old` (they follow the file),
/// while the first entry of the module list — and WMI's `ExecutablePath`, the
/// PEB's `ImagePathName` — still answer `X.exe`.
fn started_from(h: HANDLE) -> Option<PathBuf> {
    // The FIRST module of the list is the executable itself.
    let mut first: HMODULE = std::ptr::null_mut();
    let mut needed = 0u32;
    // SAFETY: `h` has QUERY_INFORMATION|VM_READ (caller's contract); the
    // out-buffer holds one HMODULE and its size is passed; `needed` is a
    // valid out-pointer.
    let ok = unsafe {
        K32EnumProcessModules(
            h,
            &mut first,
            std::mem::size_of::<HMODULE>() as u32,
            &mut needed,
        )
    };
    if ok == 0 || first.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 32_768];
    // SAFETY: `h` as above; `first` is a module of that process; `buf` is
    // sized as passed.
    let n = unsafe { K32GetModuleFileNameExW(h, first, buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buf[..n as usize])))
}

/// #3885 — the file the process's image is now, as Windows names it, on a
/// handle with at least `PROCESS_QUERY_LIMITED_INFORMATION`. It follows a
/// rename of the file ([`started_from`] does not), and it reads where the
/// module list cannot: a process this one may only query in the limited way,
/// or one whose loader has not run yet.
fn image_path(h: HANDLE) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    // SAFETY: `h` has at least QUERY_LIMITED_INFORMATION (caller's contract);
    // `len` holds the buffer's size in characters and gets the path's.
    let ok =
        unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) };
    (ok != 0 && len > 0).then(|| PathBuf::from(OsString::from_wide(&buf[..len as usize])))
}

fn session_of(pid: u32) -> Option<u32> {
    let mut sid = 0u32;
    // SAFETY: out-pointer valid for the call.
    (unsafe { ProcessIdToSessionId(pid, &mut sid) } != 0).then_some(sid)
}

/// Compare paths the way Windows does: case-insensitively, without the
/// `\\?\` verbatim prefix `canonicalize` adds.
pub(crate) fn normalize(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.to_lowercase()
}

/// #3885 — a running process, as this process could read it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) pid: u32,
    /// The name it is listed under: its image's file name, or a bare file id
    /// once the file behind its image is gone (#1686).
    pub(crate) name: String,
    /// The SID of the account it runs as, from its token. `None` when its
    /// token cannot be read.
    pub(crate) user: Option<String>,
    /// The file it was started from, the first entry of its module list
    /// ([`started_from`]), which a later rename of the file does not change.
    /// `None` when the list cannot be read.
    pub(crate) started_from: Option<PathBuf>,
    /// The file its image is now, as Windows names it
    /// (`QueryFullProcessImageNameW`), which follows a rename. `None` when it
    /// cannot be read.
    pub(crate) image: Option<PathBuf>,
}

impl Seen {
    /// The file it runs: the one it was started from, else its image now.
    pub(crate) fn path(&self) -> Option<&Path> {
        self.started_from.as_deref().or(self.image.as_deref())
    }
}

/// #3885 — what became of a process [`terminate_checked`] was asked to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    /// It had exited, or no process has its pid now.
    Gone,
    /// The process its pid names now does not check out: left alone.
    NotIt,
    /// It checked out, was terminated, and exited within the wait.
    Stopped,
    /// It checks out, and Windows refused this process the right to end it:
    /// the Win32 error.
    Refused(u32),
    /// It checked out and was terminated, and still ran when the wait ended.
    StillRunning,
}

/// #3885 — every running process listed under one of `names` (file names such
/// as `roomlerd.exe`, compared without case), with what this process can read
/// of it. A copy renamed aside (`roomlerd.exe.old`) is listed too, and so is a
/// process listed under a bare file id once the file behind its image is gone
/// (#1686), when the file it runs has one of `names`. A process listed under
/// one of `names` that cannot be opened is listed with nothing read, for the
/// caller to say so. This process is listed too. `None` when Windows refuses
/// the snapshot.
pub(crate) fn read_named(names: &[&str]) -> Option<Vec<Seen>> {
    let names: Vec<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    for entry in snapshot()? {
        let listed = entry.name.to_ascii_lowercase();
        if entry.pid == 0 || !names.iter().any(|n| worth_opening(&listed, n)) {
            continue;
        }
        let by_name = names.iter().any(|n| listed.starts_with(n.as_str()));
        let seen = match open(entry.pid, 0) {
            Ok(h) => read(entry.pid, &entry.name, &h),
            Err(_) => Seen {
                pid: entry.pid,
                name: entry.name.clone(),
                ..Seen::default()
            },
        };
        let runs_one = seen
            .path()
            .and_then(Path::file_name)
            .is_some_and(|file| names.contains(&file.to_string_lossy().to_ascii_lowercase()));
        if by_name || runs_one {
            out.push(seen);
        }
    }
    Some(out)
}

/// #3885 — end `pid` when the process it names now still checks out, and wait
/// up to `wait` for it to go.
///
/// The check and the end go through ONE handle, as in [`terminate_verified`]:
/// `is_it` judges what is read through the handle that ends the process, so a
/// pid another process took after it was listed is never ended on the strength
/// of the listing. When Windows refuses the right to end it, the process is
/// read again without that right, and only one that checks out is `Refused`:
/// whoever is told to end that pid by hand is told of a process that checks
/// out. The exit code (1) marks a kill.
pub(crate) fn terminate_checked(pid: u32, wait: Duration, is_it: &dyn Fn(&Seen) -> bool) -> Ended {
    let h = match open(pid, PROCESS_TERMINATE | PROCESS_SYNCHRONIZE) {
        Ok(h) => h,
        Err(ERROR_INVALID_PARAMETER) => return Ended::Gone,
        Err(refused) => {
            return match open(pid, PROCESS_SYNCHRONIZE) {
                Ok(h) if exited(&h) => Ended::Gone,
                Ok(h) if is_it(&read(pid, "", &h)) => Ended::Refused(refused),
                Ok(_) => Ended::NotIt,
                Err(ERROR_INVALID_PARAMETER) => Ended::Gone,
                // Read a moment ago, unreadable now: another process has the
                // pid.
                Err(_) => Ended::NotIt,
            };
        }
    };
    if exited(&h) {
        return Ended::Gone;
    }
    if !is_it(&read(pid, "", &h)) {
        return Ended::NotIt;
    }
    // SAFETY: `h` is a live process handle with PROCESS_TERMINATE, just
    // checked through this same handle.
    unsafe {
        TerminateProcess(h.0, 1);
    }
    let ms = wait.as_millis().min(u128::from(u32::MAX)) as u32;
    // SAFETY: `h` has SYNCHRONIZE; the wait is bounded.
    if unsafe { WaitForSingleObject(h.0, ms) } == WAIT_OBJECT_0 {
        Ended::Stopped
    } else {
        Ended::StillRunning
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `find(exe)` once it names `pid`, or its last answer after 10 s.
    ///
    /// #3005: a process `find` can name has a loader's module list, and a
    /// just-spawned child has none until its first thread has run. On a busy
    /// runner that has not happened yet when `spawn` returns.
    fn once_found(exe: &Path, pid: u32) -> Vec<CompanionProcess> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let found = find(exe);
            if found.iter().any(|p| p.pid == pid) || std::time::Instant::now() >= deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn only_the_companion_its_renamed_copy_or_a_file_id_is_worth_opening() {
        let name = "roomler-desktop.exe";
        assert!(worth_opening("roomler-desktop.exe", name));
        assert!(worth_opening("ROOMLER-DESKTOP.EXE", name));
        assert!(worth_opening("roomler-desktop.exe.old", name));
        // A running image whose file was POSIX-deleted (#1686, measured).
        assert!(worth_opening("00240000000AA4A74B077E87", name));
        assert!(!worth_opening("explorer.exe", name));
        assert!(!worth_opening("roomlerd.exe", name));
        assert!(!worth_opening("", name));
    }

    #[test]
    fn paths_compare_like_windows_does() {
        assert_eq!(
            normalize(Path::new(
                r"\\?\C:\Program Files\Roomler\roomler-desktop.exe"
            )),
            normalize(Path::new(r"c:\program files\roomler\ROOMLER-DESKTOP.EXE"))
        );
        assert_ne!(
            normalize(Path::new(r"C:\Program Files\Roomler\roomler-desktop.exe")),
            normalize(Path::new(
                r"C:\Users\x\AppData\Local\Programs\Roomler\roomler-desktop.exe"
            ))
        );
    }

    /// #1686: identity by the path a program was STARTED from, and stop by pid.
    /// Start a program from a path, then do to its file what the refresh does
    /// to the companion's — rename it aside — and check `find` still names it,
    /// with its session, and `terminate_verified` stops it.
    ///
    /// What this cannot do is recreate the field state itself. On the reporting
    /// host the old companion ran from an image POSIX-unlinked into
    /// `$Extend\$Deleted` (it was listed as `00240000000AA4A74B077E87`, and
    /// `tasklist /FI "IMAGENAME eq roomler-desktop.exe"` matched nothing),
    /// while WMI's `ExecutablePath` — the loader's record `find` reads —
    /// still said `C:\Program Files\Roomler\roomler-desktop.exe`. For an
    /// ordinary and an elevated caller Windows refuses every way of unlinking
    /// a running image's file (delete, replace-rename, a raw
    /// `FileDispositionInfoEx` POSIX delete — all measured, err 5), so the
    /// refusals are asserted below as the premise they are: a change in any of
    /// them is news worth failing on.
    #[test]
    fn a_running_program_is_found_by_its_started_from_path_and_stopped_by_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A unique name: a real `roomler-desktop.exe` may be running on the
        // machine that runs this test, and must neither match nor be touched.
        let name = format!("fr1686-probe-{}.exe", std::process::id());
        let exe = dir.path().join(&name);
        let system32 = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        std::fs::copy(Path::new(&system32).join(r"System32\cmd.exe"), &exe).expect("copy cmd.exe");
        let mut child = std::process::Command::new(&exe)
            .args(["/c", "ping -n 30 127.0.0.1 > nul"])
            .spawn()
            .expect("spawn the probe");

        let found = once_found(&exe, child.id());
        let me = found.iter().find(|p| p.pid == child.id()).copied();
        let own_session = session_of(std::process::id());

        // The refresh's first move on the companion's file: rename it aside.
        // A just-started image can still be open elsewhere for a moment (os
        // error 32 under load, #3005), so a sharing violation is retried. The
        // companion the refresh renames has been running far longer.
        let aside = dir.path().join(format!("{name}.old"));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let renamed = loop {
            match std::fs::rename(&exe, &aside) {
                Err(e) if e.raw_os_error() == Some(32) && std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                other => break other,
            }
        };
        renamed.expect("rename a running image aside");
        let found_after_rename = find(&exe).iter().any(|p| p.pid == child.id());
        // A running image's file refuses deletion, and a replace onto it.
        let delete_refused = std::fs::remove_file(&aside).is_err();
        let newer = dir.path().join("newer.bin");
        std::fs::write(&newer, b"the next swap's copy").expect("write the next copy");
        let replace_refused = std::fs::rename(&newer, &aside).is_err();

        let stopped = terminate_verified(&exe, &[child.id()], Duration::from_secs(10));
        let _ = child.kill();
        let _ = child.wait();

        let me = me.expect("found by the path it was started from");
        assert_eq!(
            me.session, own_session,
            "reported in the session it runs in"
        );
        assert!(
            found_after_rename,
            "still found by its started-from path after a rename"
        );
        assert!(
            delete_refused,
            "premise: a running image's file refuses a plain delete"
        );
        assert!(replace_refused, "premise: … and a replace-rename onto it");
        assert_eq!(
            stopped,
            vec![child.id()],
            "stopped by pid, confirmed exited"
        );
        assert!(
            find(&exe).iter().all(|p| p.pid != child.id()),
            "gone once stopped"
        );
    }

    /// The pid-reuse guard: a pid handed to `terminate_verified` that is NOT
    /// (or no longer) a process started from `exe` is left running — the
    /// check and the kill are one handle, so a reused pid cannot be killed on
    /// the strength of what `find` saw earlier.
    #[test]
    fn a_pid_that_does_not_verify_against_the_path_is_left_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let system32 = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let ours = dir
            .path()
            .join(format!("fr1686-ours-{}.exe", std::process::id()));
        let other = dir
            .path()
            .join(format!("fr1686-other-{}.exe", std::process::id()));
        for p in [&ours, &other] {
            std::fs::copy(Path::new(&system32).join(r"System32\cmd.exe"), p).expect("copy cmd.exe");
        }
        let mut bystander = std::process::Command::new(&other)
            .args(["/c", "ping -n 30 127.0.0.1 > nul"])
            .spawn()
            .expect("spawn the bystander");
        // The control: the bystander verifies against its OWN path, so a
        // refusal below is the path check, not a process the loader has not
        // set up yet (which verifies against nothing).
        let readable = once_found(&other, bystander.id())
            .iter()
            .any(|p| p.pid == bystander.id());

        // The bystander's pid, claimed to be a process started from `ours`.
        let stopped = terminate_verified(&ours, &[bystander.id()], Duration::from_secs(2));
        let still_running = bystander.try_wait().ok().flatten().is_none();
        let _ = bystander.kill();
        let _ = bystander.wait();

        assert!(readable, "the control: found by its own started-from path");
        assert!(
            stopped.is_empty(),
            "nothing it could not verify is counted as stopped"
        );
        assert!(
            still_running,
            "a process started from another path is never killed"
        );
    }
}
