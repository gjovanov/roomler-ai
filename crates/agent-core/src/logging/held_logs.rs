// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-145 P2 — the `service-logs` of a process with a [`super::ServiceLogRole`],
//! on Windows: the daily log and the other writers of that folder.
//!
//! The seam: each act below is still by path, as today's writers act, and
//! nothing outside the tests calls it yet. The change to handles follows it.
#![allow(dead_code)]

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
mod tests;

/// The folder this process holds, set once its start held it and opened the
/// day's log there.
static HELD: OnceLock<HeldLogDir> = OnceLock::new();

/// A role process in the machine tree found the switch at `0` (FR-145's
/// `ROOMLERD_MACHINE_TREE_HELD`).
static SWITCHED_OFF: AtomicBool = AtomicBool::new(false);

/// This process's `service-logs`, as its start held it.
pub struct HeldLogDir {
    dir: PathBuf,
}

impl HeldLogDir {
    fn hold(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    /// The folder, as this process names it ([`super::log_dir`]).
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// `bytes` as the file `name`, whole.
    pub fn write_file(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(self.dir.join(name), bytes)
    }

    /// The file `name`, opened for appending.
    pub fn append_file(&self, name: &str) -> io::Result<File> {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(name))
    }

    /// The file `name`, at most `max_len` bytes of it. `Ok(None)`: it is not
    /// there.
    pub fn read_file(&self, name: &str, max_len: u64) -> io::Result<Option<Vec<u8>>> {
        use std::io::Read as _;
        match File::open(self.dir.join(name)) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(max_len).read_to_end(&mut bytes)?;
                Ok(Some(bytes))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The file `from` renamed to `to`, a file there replaced.
    pub fn rename_file(&self, from: &str, to: &str) -> io::Result<()> {
        std::fs::rename(self.dir.join(from), self.dir.join(to))
    }

    /// The prune ([`super::prune_old_logs_at`]'s rule).
    pub(super) fn prune(&self, cutoff: SystemTime) {
        super::prune_old_logs_at(&self.dir, cutoff);
    }

    /// The length of the file `name`; `None` when it is not there.
    pub(super) fn len_of(&self, name: &str) -> Option<u64> {
        std::fs::metadata(self.dir.join(name)).ok().map(|m| m.len())
    }
}

/// The folder this process holds, when it is `dir`.
pub fn held_dir(dir: &Path) -> Option<&'static HeldLogDir> {
    HELD.get().filter(|held| held.dir == dir)
}

/// The held folder and the name of `path`, when `path` is a file directly in
/// the folder this process holds.
pub fn held_file(path: &Path) -> Option<(&'static HeldLogDir, &str)> {
    let name = path.file_name()?.to_str()?;
    Some((held_dir(path.parent()?)?, name))
}

/// `dir` is below the machine's `ProgramData`.
fn in_machine_tree(dir: &Path) -> bool {
    crate::appdirs::names_below(dir, &crate::appdirs::machine_program_data())
        .is_some_and(|names| !names.is_empty())
}

/// The day's log of a role process in the machine tree, `dir` held for the
/// life of the process. `Ok(None)`: any other process, or the switch at `0`,
/// which writes as before.
pub(super) fn open_writer(
    dir: &Path,
    basename: &'static str,
) -> io::Result<Option<HeldRollingWriter>> {
    if super::SERVICE_ROLE.get().is_none() || !in_machine_tree(dir) {
        return Ok(None);
    }
    if !crate::machine_tree::Switches::from_env().held {
        SWITCHED_OFF.store(true, Ordering::Relaxed);
        return Ok(None);
    }
    let logs = HeldLogDir::hold(dir)?;
    let date = date_of(SystemTime::now());
    let file = logs.append_file(&day_name(basename, &date))?;
    let _ = HELD.set(logs);
    let logs = HELD.get().expect("set above");
    Ok(Some(HeldRollingWriter {
        logs,
        basename,
        now: SystemTime::now,
        date,
        file,
    }))
}

/// The WARN a role process logs once its subscriber is up, when it found the
/// switch at `0`.
pub(super) fn switch_note() -> Option<&'static str> {
    SWITCHED_OFF.load(Ordering::Relaxed).then_some(
        "logging: ROOMLERD_MACHINE_TREE_HELD=0: the service log and the other service-logs \
         files are written by path, not by handle",
    )
}

/// `<basename>.<YYYY-MM-DD>`: tracing-appender's daily name.
fn day_name(basename: &str, date: &str) -> String {
    format!("{basename}.{date}")
}

fn date_of(at: SystemTime) -> String {
    super::format_utc_date(at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
}

/// The daily log of [`open_writer`]: each day's file under tracing-appender's
/// name, in UTC.
pub struct HeldRollingWriter {
    logs: &'static HeldLogDir,
    basename: &'static str,
    now: fn() -> SystemTime,
    date: String,
    file: File,
}

impl HeldRollingWriter {
    /// The writer of `basename`'s days in `logs`, today's file opened, with
    /// `now` as its clock.
    pub(super) fn open(
        logs: &'static HeldLogDir,
        basename: &'static str,
        now: fn() -> SystemTime,
    ) -> io::Result<Self> {
        let date = date_of(now());
        let file = logs.append_file(&day_name(basename, &date))?;
        Ok(Self {
            logs,
            basename,
            now,
            date,
            file,
        })
    }
}

impl Write for HeldRollingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let date = date_of((self.now)());
        if date != self.date {
            let name = day_name(self.basename, &date);
            match self.logs.append_file(&name) {
                Ok(file) => {
                    let _ = self.file.flush();
                    self.file = file;
                }
                Err(e) => super::say_on_stderr(format_args!(
                    "logging: could not open {name}: {e}; the day before's file goes on"
                )),
            }
            self.date = date;
        }
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// For the tests here and the daemon's: a process holding its `service-logs`
/// below a stand-in `%ProgramData%`, as a role process's start holds it.
#[cfg(any(test, feature = "test-fixtures"))]
pub mod test_fixture {
    use std::path::{Path, PathBuf};

    /// The root a parent hands its child.
    pub const ROOT: &str = "ROOMLER_NODE_TEST_SERVICE_LOGS_ROOT";
    /// What a child prints once its case is checked.
    pub const CHECKED: &str = "service-logs case: checked";

    /// A child's folders.
    pub struct Case {
        /// The test's own folder, outside the stand-in.
        pub root: PathBuf,
        /// `service-logs`, as a role process names it.
        pub dir: PathBuf,
        /// Where a junction in the tests leads: outside the machine tree.
        pub elsewhere: PathBuf,
    }

    /// The parent's half of the case `case` of `module` (its
    /// `module_path!()`), in a temporary folder `root`: [`run_in_child`]
    /// with an elevated token, which writes below a sealed folder as SYSTEM
    /// does. Without one it says so and returns, except in CI's elevated
    /// lanes, where it fails.
    pub fn parent(module: &str, case: &str, root: &Path) {
        if crate::machine_tree::test_support::elevated_or_skip(case) {
            run_in_child(root, &test_path(module, case), &[]);
        }
    }

    /// `module::case` as this test binary names it: without the crate's name.
    pub fn test_path(module: &str, case: &str) -> String {
        let module = module.split_once("::").map_or(module, |(_, rest)| rest);
        format!("{module}::{case}")
    }

    /// Below `root`, a stand-in `%ProgramData%` with an open machine folder
    /// and a folder `elsewhere`; then the test `test` (its path in this test
    /// binary) alone in a child with `env`, which must succeed and say it was
    /// checked.
    pub fn run_in_child(root: &Path, test: &str, env: &[(&str, &str)]) {
        let pd = crate::machine_tree::test_support::program_data(root);
        std::fs::create_dir_all(pd.join("roomler").join("roomler")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(ROOT, root)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "{said}");
        assert!(said.contains(CHECKED), "the child ran the case: {said}");
    }

    /// The child's half: the stand-in, before anything names the machine
    /// tree; with `hold`, `service-logs` held as a role process's start
    /// holds it, and named as the log folder. `None` in the parent.
    pub fn child(hold: bool) -> Option<Case> {
        let root = PathBuf::from(std::env::var_os(ROOT)?);
        crate::appdirs::test_fixture::program_data_is(root.join("ProgramData"));
        let dir = crate::appdirs::machine_global_dir().join("service-logs");
        if hold {
            let held = super::HeldLogDir::hold(&dir).unwrap_or_else(|e| panic!("held: {e}"));
            assert!(super::HELD.set(held).is_ok(), "one hold per process");
            let _ = super::super::LOG_DIR.set(Some(dir.clone()));
        }
        Some(Case {
            elsewhere: root.join("elsewhere"),
            root,
            dir,
        })
    }

    /// `dir` swapped for a junction to `elsewhere`, as something else would
    /// swap it under a process that holds it: every file in it and then the
    /// folder deleted with POSIX semantics (a handle open on either stays
    /// open, and leads nowhere), and the junction made at its name.
    pub fn swap_for_junction(dir: &Path, elsewhere: &Path) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            posix_delete(&entry.path());
        }
        posix_delete(dir);
        crate::machine_tree::test_support::junction(dir, elsewhere);
    }

    /// Delete `path` (a file, or an empty folder) with POSIX semantics,
    /// through a handle opened as the entry itself.
    fn posix_delete(path: &Path) {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
            FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfoEx,
            SetFileInformationByHandle,
        };
        const DELETE: u32 = 0x1_0000;
        let file = std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let info = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        };
        // SAFETY: a handle this function owns; `info` is the class's struct.
        let ok = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfoEx,
                (&raw const info).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
            )
        };
        assert_ne!(
            ok,
            0,
            "{}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
}
