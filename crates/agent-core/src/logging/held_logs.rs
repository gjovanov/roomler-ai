// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-145 P2 — the `service-logs` of a process with a [`super::ServiceLogRole`],
//! on Windows, written by handle.
//!
//! A role process holds its `service-logs` for its whole life: its start
//! reaches the folder with [`HeldPath::machine`] and [`Seal::Readable`] (the
//! tray reads the logs), and opens the day's log below that handle
//! ([`open_writer`]). Every later act in the folder is below the handle too,
//! the file names stay tracing-appender's daily ones, and an act refused
//! there walks again once, as a start would: the folder may have been
//! deleted or swapped since.
//!
//! | Writer | By handle |
//! |---|---|
//! | the day's log ([`HeldRollingWriter`]) | `append_file("<basename>.<YYYY-MM-DD>")`, a new name each UTC day by the writer's own clock |
//! | the prune (`prune_old_logs`) | `entries()`, the same name and age rule, `remove_file` |
//! | the panic file | `write_file("panic-<pid>-<ts>.log")` |
//! | stderr's file (#3292, #3438) | `append_file` of the day's log, twice; its length from the file itself |
//! | the daemon's encoder caps cache, install record, wedged MSI log and update-attempt marker | `write_file`, `read_file` and `rename_file`, through [`held_file`] |
//!
//! Every handle a writer keeps open appends and reads attributes, nothing
//! more ([`for_appending`]), as tracing-appender's did: a reader that does not
//! share deletion still opens the day's log. What the hold logs at the start
//! (the leaf policy's one WARN) is logged once the subscriber is up.
//!
//! A start whose hold is refused takes #3073's fallback, the reason logged
//! first: nothing is held then, and every writer goes by path in the fallback
//! folder. With `ROOMLERD_MACHINE_TREE_HELD=0` nothing is held either:
//! tracing-appender's writer and every other writer by path, as before, with
//! one WARN that names the switch. A process with no role holds nothing.
//!
//! The crash recorder's records and its verdicts ledger keep their own walk
//! (FR-145 P1): already by handle, for each write.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::machine_tree::held::Refusal;
use crate::machine_tree::{HeldPath, Seal};

#[cfg(test)]
mod tests;

/// The folder this process holds, set once its start held it and opened the
/// day's log there.
static HELD: OnceLock<HeldLogDir> = OnceLock::new();

/// A role process in the machine tree found the switch at `0` (FR-145's
/// `ROOMLERD_MACHINE_TREE_HELD`).
static SWITCHED_OFF: AtomicBool = AtomicBool::new(false);

/// The names the writers of `service-logs` use besides the logs and the
/// panic files ([`super::is_pruned`]): the daemon's, and the crash
/// recorder's ledger. After a seal in place, each one is sealed when it is a
/// plain file of SYSTEM's or Administrators', and goes when it is not.
const OTHER_NAMES: [&str; 6] = [
    "caps-cache.json",
    "last-install.json",
    "update-attempt",
    "msi-wedged.log",
    "msi-wedged.prev.log",
    crate::crash_recorder::VERDICTS_FILE,
];

/// This process's `service-logs`, as its start held it.
pub struct HeldLogDir {
    dir: PathBuf,
    held: Mutex<Arc<HeldPath>>,
}

impl HeldLogDir {
    fn hold(dir: &Path) -> Result<Self, Refusal> {
        Ok(Self {
            dir: dir.to_path_buf(),
            held: Mutex::new(Arc::new(walk(dir)?)),
        })
    }

    /// The folder, as this process names it ([`super::log_dir`]).
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// `bytes` as the file `name`, whole: a fresh file below the folder,
    /// renamed onto the name ([`HeldPath::write_file`]).
    pub fn write_file(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.act(|held| held.write_file(name, bytes, Seal::Readable))
    }

    /// The file `name`, opened for appending below the folder: a link or a
    /// file of more than one name there is deleted as itself and the file
    /// made again ([`HeldPath::append_file`]). The handle returned appends
    /// and reads attributes, nothing more ([`for_appending`]).
    pub fn append_file(&self, name: &str) -> io::Result<File> {
        self.act(|held| held.append_file(name, Seal::Readable))
            .map(for_appending)
    }

    /// The file `name`, at most `max_len` bytes of it, never through a link.
    /// `Ok(None)`: it is not there.
    pub fn read_file(&self, name: &str, max_len: u64) -> io::Result<Option<Vec<u8>>> {
        self.act(|held| held.read_file(name, max_len))
    }

    /// The file `from` renamed to `to` by its own handle, a file there
    /// replaced.
    pub fn rename_file(&self, from: &str, to: &str) -> io::Result<()> {
        self.act(|held| held.rename_file(from, held, to, true))
    }

    /// The prune: [`super::is_pruned`]'s names older than `cutoff`, read
    /// from the folder's listing and deleted by their own handles. A folder
    /// or a link is never one.
    pub(super) fn prune(&self, cutoff: SystemTime) {
        let cutoff = cutoff
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let Ok(entries) = self.act(HeldPath::entries) else {
            return;
        };
        for entry in entries {
            if !entry.is_dir
                && !entry.is_link
                && super::is_pruned(&entry.name)
                && entry.modified_unix < cutoff
            {
                let _ = self.act(|held| held.remove_file(&entry.name));
            }
        }
    }

    /// The length of the day's file `name`, from the file itself, opened as
    /// its writers open it ([`HeldLogDir::append_file`]). Never from the
    /// folder's listing: NTFS updates a listed length lazily, and a day's log
    /// is open for writing all day.
    pub(super) fn len_of(&self, name: &str) -> io::Result<u64> {
        Ok(self.append_file(name)?.metadata()?.len())
    }

    /// `act` below the folder held; refused, once more below the folder
    /// walked again, which then stays held.
    fn act<T>(&self, act: impl Fn(&HeldPath) -> Result<T, Refusal>) -> io::Result<T> {
        let held = Arc::clone(&self.lock());
        let refused = match act(&held) {
            Ok(done) => return Ok(done),
            Err(refused) => refused,
        };
        let Ok(again) = walk(&self.dir) else {
            return Err(refused.into());
        };
        let again = Arc::new(again);
        *self.lock() = Arc::clone(&again);
        act(&again).map_err(Into::into)
    }

    fn lock(&self) -> MutexGuard<'_, Arc<HeldPath>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `dir`, reached by handle and sealed with the leaf policy (D2). An earlier
/// build's folder sealed in place hands nothing down: each name a writer of
/// the folder uses is then sealed by its own handle when it is a plain file
/// of SYSTEM's or Administrators', and goes when it is not. Other names are
/// left as they are.
fn walk(dir: &Path) -> Result<HeldPath, Refusal> {
    let held = HeldPath::machine(dir, Seal::Readable)?;
    if held.resealed() {
        let gone = held.keep_only_plain(
            |name| super::is_pruned(name) || OTHER_NAMES.contains(&name),
            Seal::Readable,
        );
        if !gone.is_empty() {
            let gone: Vec<String> = gone.iter().map(ToString::to_string).collect();
            tracing::warn!(
                ?gone,
                "logging: service-logs sealed; what was not a plain file of its writers removed, \
                 or left"
            );
        }
    }
    Ok(held)
}

/// `file`, opened again by its own handle (`ReOpenFile`: the same file, no
/// name looked up) to append and read attributes alone, and the handle
/// [`HeldPath::append_file`] gave closed. That one holds `DELETE`, for what
/// it finds at the name and deletes; while a handle with `DELETE` is open, a
/// reader that does not share deletion (PowerShell's `Get-Content`, `type`)
/// cannot open the file, and a day's log is open all day. tracing-appender's
/// handle never held it. Should the reopen fail (the open before it passed
/// the same checks), the handle `append_file` gave goes on: the log stays
/// where it is, as P1's ledger keeps its own.
fn for_appending(file: File) -> File {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_APPEND_DATA, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, ReOpenFile,
    };
    const SYNCHRONIZE: u32 = 0x10_0000;
    // SAFETY: `file`'s handle stays open for the call; the handle returned is
    // a new one, owned below.
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle(),
            FILE_APPEND_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return file;
    }
    // SAFETY: a handle `ReOpenFile` just opened, which nothing else owns.
    unsafe { File::from_raw_handle(handle) }
}

/// What the start's hold logged (the leaf policy's WARN, the scrub's), kept
/// until the subscriber is up: the hold runs before it exists.
static EARLY: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `start`, with what it logs kept in [`EARLY`].
fn keeping_its_lines<T>(start: impl FnOnce() -> T) -> T {
    let text = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = Arc::clone(&text);
    let keeper = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_level(false)
        .with_target(false)
        .with_writer(move || Lines(Arc::clone(&sink)))
        .finish();
    let done = tracing::subscriber::with_default(keeper, start);
    let text = text.lock().unwrap_or_else(PoisonError::into_inner);
    EARLY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .extend(String::from_utf8_lossy(&text).lines().map(str::to_owned));
    done
}

/// The lines [`keeping_its_lines`] kept, for `init` to log once its
/// subscriber is up; each once.
pub(super) fn take_early_lines() -> Vec<String> {
    std::mem::take(&mut *EARLY.lock().unwrap_or_else(PoisonError::into_inner))
}

/// The writer of [`keeping_its_lines`]'s subscriber.
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Write for Lines {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut text = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        text.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The folder this process holds, when it is `dir`.
pub fn held_dir(dir: &Path) -> Option<&'static HeldLogDir> {
    HELD.get().filter(|held| held.dir == dir)
}

/// The held folder and the name of `path`, when `path` is a file directly in
/// the folder this process holds. `None` for any other path, and in a process
/// that holds none (no role, the switch at `0`, a refused hold): the caller
/// writes by path, as before.
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
/// which writes as before. An error (the hold refused, or the day's file):
/// nothing is held, and the start takes its fallback (#3073).
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
    let now = unix_secs(SystemTime::now());
    let (logs, file) = keeping_its_lines(|| -> io::Result<_> {
        let logs = HeldLogDir::hold(dir)?;
        let file = logs.append_file(&day_name(basename, &date_of(now)))?;
        Ok((logs, file))
    })?;
    if HELD.set(logs).is_err() {
        // One hold a process: `init` opens its log once.
        return Ok(None);
    }
    let logs = HELD.get().expect("set above");
    Ok(Some(HeldRollingWriter {
        logs,
        basename,
        now: SystemTime::now,
        next_day: next_day(now),
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

fn unix_secs(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn date_of(unix: u64) -> String {
    super::format_utc_date(unix)
}

/// The start of the UTC day after the one `unix` is in.
fn next_day(unix: u64) -> u64 {
    (unix / 86_400 + 1) * 86_400
}

/// The daily log of [`open_writer`] (D3): each UTC day's file under
/// tracing-appender's name, opened below the held folder, and as
/// tracing-appender's roll, only forward: a clock set back keeps the day's
/// file. A day whose file cannot be opened keeps writing to the day before's,
/// as tracing-appender's does, and says so on stderr.
pub struct HeldRollingWriter {
    logs: &'static HeldLogDir,
    basename: &'static str,
    now: fn() -> SystemTime,
    /// When the file in use stops being the day's.
    next_day: u64,
    file: File,
}

impl HeldRollingWriter {
    /// The writer of `basename`'s days in `logs`, today's file opened, with
    /// `now` as its clock.
    #[cfg(test)]
    pub(super) fn open(
        logs: &'static HeldLogDir,
        basename: &'static str,
        now: fn() -> SystemTime,
    ) -> io::Result<Self> {
        let at = unix_secs(now());
        let file = logs.append_file(&day_name(basename, &date_of(at)))?;
        Ok(Self {
            logs,
            basename,
            now,
            next_day: next_day(at),
            file,
        })
    }
}

impl Write for HeldRollingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let at = unix_secs((self.now)());
        if at >= self.next_day {
            let name = day_name(self.basename, &date_of(at));
            match self.logs.append_file(&name) {
                Ok(file) => {
                    let _ = self.file.flush();
                    self.file = file;
                }
                Err(e) => super::say_on_stderr(format_args!(
                    "logging: could not open {name}: {e}; the day before's file goes on"
                )),
            }
            self.next_day = next_day(at);
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
    /// The stand-in the parent made below the root, handed over whole: the
    /// child names the folder its parent made, never one of its own.
    const STAND_IN: &str = "ROOMLER_NODE_TEST_SERVICE_LOGS_STAND_IN";
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
            .env(STAND_IN, &pd)
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
        let stand_in = std::env::var_os(STAND_IN).expect("the parent names its stand-in");
        crate::appdirs::test_fixture::program_data_is(PathBuf::from(stand_in));
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
