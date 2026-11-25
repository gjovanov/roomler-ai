// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
// RETIRED-NAME-ANCHOR-BEGIN
// This whole module IS the legacy-segment fallback. Every occurrence of a
// retired name in it is the OLD half of a dual-read — the module doc explaining
// the new-then-old resolution, `OLD_APP`, `migrate_legacy_trees`, and the tests
// that build a legacy tree in order to migrate it.
// INVARIANT: if you add a retired name here that is NOT the old half of a
// dual-read, that is a bug, not a new exemption. docs/fr/FR-21
//! Roomler node application directories with a legacy-segment fallback.
//!
//! The controlled-host daemon is being renamed `roomler-agent` -> `roomlerd`
//! (the unified device/node model — see the unification plan). Its per-user and
//! machine-global data trees historically live under an app segment
//! `roomler-agent` (`%APPDATA%\roomler\roomler-agent`,
//! `%PROGRAMDATA%\roomler\roomler-agent`, `~/.config/roomler-agent`, ...).
//!
//! Renaming that segment to `roomler` must **never** orphan a host's enrolled
//! `config.toml` (its bearer token) — that's the same class of silent
//! fleet-drop-off as the MajorUpgrade-drops-env-vars bug. So resolution reads
//! BOTH: it uses the tree that holds the config folder, the NEW `roomler` one
//! first, else the OLD `roomler-agent` one; only when neither holds one does a
//! local data folder decide, the same way; and only a genuinely fresh install
//! lands on the new segment (#3747). The decision is made once per process
//! (cached) and applied to every directory, so config / logs / crashes on a host
//! never split across two trees.
//!
//! S1b adds the follow-up: [`migrate_legacy_trees`] performs a one-shot
//! startup RENAME of the legacy trees onto the `roomler` segment (per-user
//! config/data roots + the Windows machine-global root). It runs before any
//! consumer caches a segment decision, never deletes, and on any failure
//! simply leaves the read-both resolution in charge (retry next start) — so
//! enrollment still cannot be lost.

use directories::ProjectDirs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Reverse-domain qualifier for the agent's per-user dirs (macOS uses it;
/// Windows/Linux ignore it). Historically "live" — preserved so existing
/// macOS dirs aren't orphaned.
const QUALIFIER: &str = "live";
pub(crate) const ORG: &str = "roomler";
/// New app segment (post-rename, fresh installs).
pub(crate) const NEW_APP: &str = "roomler";
/// Legacy app segment (pre-rename installs already in the field).
// RETIRED-NAME-ANCHOR: a pre-rename host resolves its ENROLLED config through this
// segment. Renaming it orphans that config -- the device drops off the mesh and
// re-enrolls as a stranger. See docs/fr/FR-21.
pub(crate) const OLD_APP: &str = "roomler-agent";

/// The tree an account's processes use, chosen from that account's folders:
/// `config_dir` and `data_local_dir` name a tree's config folder and local
/// data folder by its app segment (`None` where the platform names none), and
/// `exists` is the filesystem. #3747 — the tree that holds a config folder,
/// the NEW one first: `config.toml`, the enrollment, is there. Only when
/// neither tree holds one, the tree that holds a local data folder, the NEW
/// one first; else the NEW one (a fresh install). Pure, so the rule tests on
/// every platform.
fn segment_of(
    config_dir: impl Fn(&str) -> Option<PathBuf>,
    data_local_dir: impl Fn(&str) -> Option<PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> &'static str {
    let found = |dir: Option<PathBuf>| dir.is_some_and(|dir| exists(&dir));
    if found(config_dir(NEW_APP)) {
        NEW_APP
    } else if found(config_dir(OLD_APP))
        || (!found(data_local_dir(NEW_APP)) && found(data_local_dir(OLD_APP)))
    {
        OLD_APP
    } else {
        NEW_APP
    }
}

/// #3802 — the agent's folders in one account's tree, as `ProjectDirs` names
/// them: the config folder, the data folder and the local data folder.
/// [`project_dirs`] returns this rather than a `ProjectDirs` because on macOS
/// the tree hangs off the home the account database names, and for root on
/// Linux off `/root` (#3866) ([`dirs_for`]), and a `ProjectDirs` takes its
/// home from `$HOME` with no way to be given another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    config: PathBuf,
    data: PathBuf,
    data_local: PathBuf,
}

impl AppDirs {
    pub fn config_dir(&self) -> &Path {
        &self.config
    }

    pub fn data_dir(&self) -> &Path {
        &self.data
    }

    pub fn data_local_dir(&self) -> &Path {
        &self.data_local
    }
}

impl From<&ProjectDirs> for AppDirs {
    fn from(dirs: &ProjectDirs) -> Self {
        Self {
            config: dirs.config_dir().to_path_buf(),
            data: dirs.data_dir().to_path_buf(),
            data_local: dirs.data_local_dir().to_path_buf(),
        }
    }
}

/// The folder `ProjectDirs` names on macOS for segment `app` of the account
/// whose home is `home`: `<home>/Library/Application Support/live.roomler.<app>`.
fn macos_app_support(home: &Path, app: &str) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join(format!("{QUALIFIER}.{ORG}.{app}"))
}

/// #3802 — the macOS rule, pure: the agent's folders for segment `app` hang
/// off `account_home`, the home the account database names for the account
/// this process runs as, and nothing reads `$HOME`. All three are the one
/// [`macos_app_support`] folder, as in `ProjectDirs`' macOS layout. `None`
/// when the account database names no home for this account.
#[cfg(any(target_os = "macos", test))]
fn macos_dirs(app: &str, account_home: impl FnOnce() -> Option<PathBuf>) -> Option<AppDirs> {
    let dir = macos_app_support(&account_home()?, app);
    Some(AppDirs {
        config: dir.clone(),
        data: dir.clone(),
        data_local: dir,
    })
}

/// #3866 — an account's own folders on Linux, as the XDG Base Directory
/// specification names them: its home, its config, data, cache and state
/// folders, and its runtime folder where it has one.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnFolders {
    pub home: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub state: PathBuf,
    pub runtime: Option<PathBuf>,
}

/// Root's home on Linux: `/root`, the home systemd gives a service that runs
/// with `User=root`.
#[cfg(any(target_os = "linux", test))]
const ROOT_HOME: &str = "/root";

/// #3866 — the Linux rule, pure: the folders of the account `euid`, with
/// `var` as the process's environment and `account_home` as the home the
/// account database names for that account.
///
/// - Root (`euid` 0) reads no variable and asks the account database
///   nothing. Its home is `/root`, as systemd gives it to a service with
///   `User=root`, and each folder is the XDG default beneath it, with no
///   runtime folder. So a root process uses root's own folders beneath
///   `/root`, whatever `HOME` or `XDG_*` it was started with, and a daemon
///   that systemd started as root keeps the folders it has always had.
/// - Any other account resolves as `directories` resolves it: its home is
///   `HOME` where that is not empty, else the account database's; each
///   `XDG_*_HOME` and `XDG_RUNTIME_DIR` counts where it names an absolute
///   folder, else the default beneath the home. `None` with neither home.
///
/// A folder counts as absolute by `Path::has_root`, which is
/// `Path::is_absolute` on Linux; written so, the rule tests on every
/// platform.
#[cfg(any(target_os = "linux", test))]
fn linux_folders(
    euid: u32,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    account_home: impl FnOnce() -> Option<PathBuf>,
) -> Option<OwnFolders> {
    let root = euid == 0;
    let home = if root {
        PathBuf::from(ROOT_HOME)
    } else {
        var("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(account_home)?
    };
    let given = |name: &str| {
        if root {
            return None;
        }
        var(name)
            .map(PathBuf::from)
            .filter(|folder| folder.has_root())
    };
    Some(OwnFolders {
        config: given("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
        data: given("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")),
        cache: given("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache")),
        state: given("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state")),
        runtime: given("XDG_RUNTIME_DIR"),
        home,
    })
}

/// #3866 — this process's own folders on Linux ([`linux_folders`]): its
/// effective uid, its environment as it is at the call, and, for a process
/// that is not root and has no `HOME`, the home the account database names
/// for its account, read once per process.
#[cfg(target_os = "linux")]
pub fn own_folders() -> Option<OwnFolders> {
    static ACCOUNT_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    // SAFETY: `geteuid` is a thread-safe read of the caller's own credentials.
    let euid = unsafe { libc::geteuid() };
    linux_folders(
        euid,
        |name| std::env::var_os(name),
        || ACCOUNT_HOME.get_or_init(own_home).clone(),
    )
}

/// #3866 — the daemon's folders for segment `app` in an account's own
/// folders on Linux (`own`), laid out as `ProjectDirs` lays them out there:
/// the config folder, and the data folder, which is the local data folder
/// too.
#[cfg(any(target_os = "linux", test))]
fn linux_dirs(app: &str, own: Option<OwnFolders>) -> Option<AppDirs> {
    let own = own?;
    Some(AppDirs {
        config: own.config.join(app),
        data: own.data.join(app),
        data_local: own.data.join(app),
    })
}

/// #3866 — an account's Videos folder on Linux: the one `user-dirs.dirs` in
/// its own config folder names (`XDG_VIDEOS_DIR`), read as `directories`'
/// `UserDirs` reads it ([`user_dir_in`]). `None` where the file names none,
/// or there is no file. `read` stands in for the file system.
#[cfg(any(target_os = "linux", test))]
pub fn videos_dir(
    own: &OwnFolders,
    read: impl FnOnce(&Path) -> Option<Vec<u8>>,
) -> Option<PathBuf> {
    let text = read(&own.config.join("user-dirs.dirs"))?;
    user_dir_in(&own.home, &text, "VIDEOS")
}

/// The folder a `user-dirs.dirs` text names for `name` (its
/// `XDG_<name>_DIR` line), as `dirs-sys` parses the file: blanks around the
/// key and the value are trimmed, and the value is a double-quoted
/// `"$HOME/…"`, a folder beneath `home`, or a double-quoted absolute folder,
/// with each backslash taking the byte after it as it is. `"$HOME/"` alone
/// and any other value name no folder, and of the lines that name one, the
/// last counts.
#[cfg(any(target_os = "linux", test))]
fn user_dir_in(home: &Path, text: &[u8], name: &str) -> Option<PathBuf> {
    fn trimmed(bytes: &[u8]) -> &[u8] {
        let blank = |b: &&u8| **b == b' ' || **b == b'\t';
        let start = bytes.iter().take_while(blank).count();
        let bytes = &bytes[start..];
        &bytes[..bytes.len() - bytes.iter().rev().take_while(blank).count()]
    }
    let mut found = None;
    for line in text.split(|b| *b == b'\n') {
        let Some(at) = line.iter().position(|b| *b == b'=') else {
            continue;
        };
        let key = trimmed(&line[..at]);
        if key
            .strip_prefix(b"XDG_")
            .and_then(|key| key.strip_suffix(b"_DIR"))
            != Some(name.as_bytes())
        {
            continue;
        }
        let Some(value) = trimmed(&line[at + 1..])
            .strip_prefix(b"\"")
            .and_then(|value| value.strip_suffix(b"\""))
        else {
            continue;
        };
        let (beneath_home, value) = if value == b"$HOME/" {
            continue;
        } else if let Some(rest) = value.strip_prefix(b"$HOME/") {
            (true, rest)
        } else if value.starts_with(b"/") {
            (false, value)
        } else {
            continue;
        };
        let mut unescaped = Vec::with_capacity(value.len());
        let mut bytes = value.iter();
        while let Some(&byte) = bytes.next() {
            if byte == b'\\' {
                unescaped.extend(bytes.next());
            } else {
                unescaped.push(byte);
            }
        }
        let folder = path_of_bytes(unescaped);
        found = Some(if beneath_home {
            home.join(folder)
        } else {
            folder
        });
    }
    found
}

/// A path from the bytes a file names it by.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn path_of_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

/// A path from the bytes a file names it by; outside Unix only the tests
/// read one, and their paths are UTF-8.
#[cfg(all(test, not(unix)))]
fn path_of_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

/// #3802 — the agent's folders for segment `app` in the tree of the account
/// this process runs as.
///
/// On macOS the tree hangs off the home the account database names for that
/// account ([`own_home`], read once per process), whatever `HOME` the process
/// was given. macOS sudo keeps the caller's `HOME` (`env_keep += "HOME"`), so
/// a root step that read it, the one-row enrollment or a hand-typed
/// `sudo roomlerd …`, made and owned the person's `live.roomler.roomler`, and
/// the screen worker the root daemon starts as that person could not write
/// it. A process given no `HOME`, which `ProjectDirs` answers from the same
/// account database (the root daemon under launchd), or its own home in it
/// (the LaunchAgent, the worker `sudo -u` starts, the tray), resolves the same
/// folders as before.
///
/// #3866 — on Linux the tree is in the account's own folders
/// ([`own_folders`]): a root process uses root's own folders beneath
/// `/root`, whatever `HOME` or `XDG_*` it was started with, and any other
/// process honours `HOME` and the XDG variables as `ProjectDirs` does.
///
/// On Windows `ProjectDirs` decides, as it always has: the known folders.
fn dirs_for(app: &str) -> Option<AppDirs> {
    #[cfg(target_os = "macos")]
    {
        static ACCOUNT_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
        macos_dirs(app, || ACCOUNT_HOME.get_or_init(own_home).clone())
    }
    #[cfg(target_os = "linux")]
    {
        linux_dirs(app, own_folders())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        ProjectDirs::from(QUALIFIER, ORG, app).map(|dirs| AppDirs::from(&dirs))
    }
}

/// Whether to use the OLD segment for the per-user tree: [`segment_of`] over
/// this account's own folders ([`dirs_for`]). Cached: the filesystem answer is
/// stable within a process, and caching guarantees every consumer in one run
/// agrees (no split trees).
fn use_old_segment() -> bool {
    static DECISION: OnceLock<bool> = OnceLock::new();
    *DECISION.get_or_init(|| {
        segment_of(
            |app| dirs_for(app).map(|d| d.config_dir().to_path_buf()),
            |app| dirs_for(app).map(|d| d.data_local_dir().to_path_buf()),
            Path::exists,
        ) == OLD_APP
    })
}

/// The resolved per-user app segment: "roomler-agent" for a pre-rename install
/// whose config folder is in the old tree and not in the new one, or, with no
/// config folder in either tree, whose local data folder is only in the old
/// tree; "roomler" otherwise (fresh and migrated hosts).
fn app_segment() -> &'static str {
    if use_old_segment() { OLD_APP } else { NEW_APP }
}

/// The agent's folders ([`dirs_for`]), resolved to the NEW segment unless a
/// pre-rename install is detected (then the OLD segment, so its enrolled
/// config is never orphaned). `None` only if the platform exposes no config
/// dir at all, or on macOS the account database names no home, or on Linux a
/// process that is not root has no `HOME` and no home in the account
/// database.
pub fn project_dirs() -> Option<AppDirs> {
    dirs_for(app_segment())
}

/// FR-43 P3c — the folder [`project_dirs`]`().config_dir()` is on macOS for
/// the person whose home is `home`, chosen as that person's own process
/// chooses it ([`use_old_segment`]): the current segment where it exists,
/// else the pre-rename one where only that exists. A root process reaches a
/// person's files through it, since its own `project_dirs` is root's. Pure,
/// with `exists` as the filesystem, so it tests on every platform.
pub fn macos_config_dir_in_home(home: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let current = macos_app_support(home, NEW_APP);
    let old = macos_app_support(home, OLD_APP);
    if !exists(&current) && exists(&old) {
        old
    } else {
        current
    }
}

/// Machine-global data root `%PROGRAMDATA%\roomler\<segment>` (Windows only).
/// `C:\Windows\System32`, as the kernel knows it: never from `%SystemRoot%` or
/// `%windir%`, which a limited process can override for its own user in
/// `HKCU\Environment`.
#[cfg(target_os = "windows")]
pub fn system_directory() -> PathBuf {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    system_folder(GetSystemDirectoryW).unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32"))
}

/// `C:\Windows`, read the way [`system_directory`] is.
#[cfg(target_os = "windows")]
pub fn windows_directory() -> PathBuf {
    use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
    system_folder(GetSystemWindowsDirectoryW).unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

#[cfg(target_os = "windows")]
fn system_folder(read: unsafe extern "system" fn(*mut u16, u32) -> u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut buf = [0u16; 512];
    // SAFETY: the buffer and its length in characters, for one call.
    let len = unsafe { read(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (len > 0 && len < buf.len()).then(|| std::ffi::OsString::from_wide(&buf[..len]).into())
}

/// A program that ships with Windows, by its full path. FR-129 C: this, never
/// a bare name, on any path a person's wizard runs. `Command` looks for a bare
/// name in the folder of the running EXE BEFORE System32, the shell's `runas`
/// in the current directory, and the wizard sits in the person's Downloads and
/// goes on to run elevated: a bare `icacls` there is any `icacls.exe` someone
/// left beside it, with the administrator's token and no prompt. CI's gate is
/// `agents/roomler-setup/src/spawn_gate.rs`.
#[cfg(target_os = "windows")]
pub fn system32_tool(file: &str) -> PathBuf {
    system_directory().join(file)
}

/// The variables that name a machine's own folders, with what they really are:
/// from the kernel and the machine's registry, whatever the environment says.
/// A value in `HKCU\Environment` overrides each of them for every process of
/// that user, an elevated one included, so an elevated process that reads one
/// is reading what a limited one wrote. FR-129 C: the wizard sets these first
/// thing, for the code it links that reads them
/// (`agents/roomler-setup/src/launch.rs`, `start_clean`).
#[cfg(target_os = "windows")]
pub fn machine_folder_variables() -> Vec<(&'static str, PathBuf)> {
    let windows = windows_directory();
    let drive: PathBuf = windows.components().take(1).collect();
    let drive_text = drive.to_string_lossy().into_owned();
    let program_files = registry_program_files(&drive_text);
    let program_data = registry_program_data(&drive_text);
    vec![
        ("SystemRoot", windows.clone()),
        ("windir", windows),
        ("SystemDrive", drive),
        ("ComSpec", system32_tool("cmd.exe")),
        ("ProgramFiles", program_files.clone()),
        ("ProgramW6432", program_files),
        ("ProgramData", program_data.clone()),
        ("ALLUSERSPROFILE", program_data),
    ]
}

/// The drive Windows is on (`C:`), from the kernel's answer.
#[cfg(target_os = "windows")]
fn system_drive_text() -> String {
    let drive: PathBuf = windows_directory().components().take(1).collect();
    drive.to_string_lossy().into_owned()
}

/// `C:\Program Files`, as the machine's registry names it.
#[cfg(target_os = "windows")]
fn registry_program_files(drive: &str) -> PathBuf {
    PathBuf::from(
        hklm_text(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion",
            "ProgramFilesDir",
        )
        .unwrap_or_else(|| format!(r"{drive}\Program Files")),
    )
}

/// `C:\ProgramData`, as the machine's registry names it.
#[cfg(target_os = "windows")]
fn registry_program_data(drive: &str) -> PathBuf {
    // `%SystemDrive%\ProgramData` as it is stored: left unexpanded, and that
    // one variable filled in from the kernel's answer.
    PathBuf::from(
        hklm_text(
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList",
            "ProgramData",
        )
        .map(|v| expand_system_drive(&v, drive))
        .unwrap_or_else(|| format!(r"{drive}\ProgramData")),
    )
}

/// #3702 — the machine's `ProgramData` folder (`C:\ProgramData`), from the
/// kernel and the machine's registry, never from a variable. Every machine
/// folder a daemon or one of the package's steps names is rooted here
/// ([`machine_global_dir`] and the daemon's own files beside it), so the folder
/// is the machine's whichever account's environment the process was given.
/// Read each time it is asked for: one registry value.
#[cfg(target_os = "windows")]
pub fn machine_program_data() -> PathBuf {
    #[cfg(any(test, feature = "test-fixtures"))]
    if let Some(folder) = test_fixture::PROGRAM_DATA.get() {
        return folder.clone();
    }
    registry_program_data(&system_drive_text())
}

/// #3702 — the machine's `Program Files` folder, read the way
/// [`machine_program_data`] is: where the per-Machine MSI installs.
#[cfg(target_os = "windows")]
pub fn machine_program_files() -> PathBuf {
    registry_program_files(&system_drive_text())
}

/// #3702 — the profile folder of the account `sid`, as the machine's registry
/// names it (`ProfileList\<sid>\ProfileImagePath`), with `%SystemRoot%` and
/// `%SystemDrive%` filled in from the kernel's answers. `None` when the account
/// has no profile there, or its path names any other variable, or is not
/// absolute: no variable is read from the environment.
#[cfg(target_os = "windows")]
pub fn profile_of_account(sid: &str) -> Option<PathBuf> {
    if sid.is_empty() || sid.contains('\\') {
        return None;
    }
    let stored = hklm_text(
        &format!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\{sid}"),
        "ProfileImagePath",
    )?;
    let windows = windows_directory();
    let filled = fill_in_system_folders(&stored, &windows.to_string_lossy(), &system_drive_text())?;
    let path = PathBuf::from(filled);
    path.is_absolute().then_some(path)
}

/// `%SystemRoot%` and `%SystemDrive%`, in any case, filled in with `windows`
/// and `drive`. `None` when anything else in `stored` looks like a variable.
#[cfg(any(target_os = "windows", test))]
fn fill_in_system_folders(stored: &str, windows: &str, drive: &str) -> Option<String> {
    // ASCII lowercasing keeps every byte where it is, so an index found in
    // `lower` is the same place in `stored`.
    let lower = stored.to_ascii_lowercase();
    let mut out = String::with_capacity(stored.len());
    let mut from = 0;
    while let Some(found) = lower[from..].find('%') {
        let at = from + found;
        out.push_str(&stored[from..at]);
        let (name, value) = [("%systemroot%", windows), ("%systemdrive%", drive)]
            .into_iter()
            .find(|(name, _)| lower[at..].starts_with(name))?;
        out.push_str(value);
        from = at + name.len();
    }
    out.push_str(&stored[from..]);
    Some(out)
}

/// #3702 — the service account's own profile
/// (`C:\Windows\System32\config\systemprofile`), from
/// [`profile_of_account`], else where Windows keeps it below
/// [`system_directory`].
#[cfg(target_os = "windows")]
pub fn system_profile() -> PathBuf {
    profile_of_account(crate::machine_tree::SYSTEM)
        .unwrap_or_else(|| system_directory().join("config").join("systemprofile"))
}

/// #3702 — what a process on the service account names its folders by:
/// [`machine_folder_variables`], and `USERPROFILE`, `APPDATA` and
/// `LOCALAPPDATA` of [`system_profile`].
#[cfg(target_os = "windows")]
pub fn system_folder_variables() -> Vec<(&'static str, PathBuf)> {
    let profile = system_profile();
    let mut vars = machine_folder_variables();
    vars.push(("APPDATA", profile.join("AppData").join("Roaming")));
    vars.push(("LOCALAPPDATA", profile.join("AppData").join("Local")));
    vars.push(("USERPROFILE", profile));
    vars
}

/// #3702 — is `own_sid`, the SID of the account a process runs as, the
/// service account's (LocalSystem's)? What [`pin_system_folders`] asks first.
#[cfg(target_os = "windows")]
pub fn is_service_account(own_sid: Option<&str>) -> bool {
    own_sid == Some(crate::machine_tree::SYSTEM)
}

/// #3702 — the first thing a daemon's `main` does on Windows, before the
/// process has a second thread. On the service account (`own_sid` is
/// LocalSystem's) it sets the process's folder variables to
/// [`system_folder_variables`], so what reads them, in this binary and in
/// what it links, names the machine's folders and the service account's own
/// profile, and every process it starts without a block of its own inherits
/// them. For any other account it does nothing: a person's process keeps the
/// environment the system gave it. Returns whether it set them.
///
/// A service `Environment` entry that names one of these variables does not
/// reach a process on the service account: the value here replaces it.
///
/// This is the second guard. The machine's folders come from
/// [`machine_program_data`] and [`machine_program_files`], which read the
/// system and never these variables. A value set here is what every reader
/// of the process's environment through Windows or Rust sees; C code that
/// reads its runtime's own copy (`getenv`) keeps the copy that runtime took
/// when the process started.
#[cfg(target_os = "windows")]
pub fn pin_system_folders(own_sid: Option<&str>) -> bool {
    if !is_service_account(own_sid) {
        return false;
    }
    for (name, value) in system_folder_variables() {
        // SAFETY: `main` calls this before it starts any thread. Windows
        // keeps the environment block behind its own lock, and std takes its
        // own around every read and write of it as well.
        unsafe { std::env::set_var(name, value) };
    }
    true
}

/// #3702 — the account's own `LocalAppData` known folder, where the per-user
/// MSI installs (`Programs\<folder>`), as the shell names it for the account
/// the process runs as. `None` when the shell names none.
#[cfg(target_os = "windows")]
pub fn local_app_data() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.data_local_dir().to_path_buf())
}

/// Tests only: the machine's folders under a stand-in the test names itself.
/// A release build has no such switch, and nothing here reads a variable.
#[cfg(all(target_os = "windows", any(test, feature = "test-fixtures")))]
#[doc(hidden)]
pub mod test_fixture {
    use std::path::PathBuf;
    use std::sync::OnceLock;

    pub(super) static PROGRAM_DATA: OnceLock<PathBuf> = OnceLock::new();

    /// The machine's `ProgramData` is `folder` for the rest of this test
    /// process: [`super::machine_program_data`] and every machine folder rooted
    /// at it name the stand-in. Call it first: once the machine tree has been
    /// named it panics, so a test never goes on with this computer's own tree.
    pub fn program_data_is(folder: PathBuf) {
        assert!(
            super::MACHINE_GLOBAL.get().is_none(),
            "the machine tree was named before the stand-in was set"
        );
        if let Err(folder) = PROGRAM_DATA.set(folder) {
            assert_eq!(
                PROGRAM_DATA.get(),
                Some(&folder),
                "one stand-in per process"
            );
        }
    }
}

/// #3702 — is `path` the folder `folder` or below it, named the way Windows
/// names folders: in any case, `\` and `/` alike? A `..` anywhere is never
/// below anything. Text only, so it answers the same on every platform.
pub fn in_windows_folder(path: &Path, folder: &Path) -> bool {
    fn parts(path: &Path) -> Option<Vec<String>> {
        let mut parts = Vec::new();
        for part in path.to_str()?.split(['\\', '/']) {
            match part {
                "" | "." => {}
                ".." => return None,
                name => parts.push(name.to_lowercase()),
            }
        }
        Some(parts)
    }
    match (parts(path), parts(folder)) {
        (Some(path), Some(folder)) => !folder.is_empty() && path.starts_with(&folder),
        _ => false,
    }
}

/// #3702 — is `path` the folder `folder` or below it? On Windows as
/// [`in_windows_folder`] answers; elsewhere part by part, exactly.
pub fn in_folder(path: &Path, folder: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        in_windows_folder(path, folder)
    }
    #[cfg(not(target_os = "windows"))]
    {
        names_below(path, folder).is_some()
    }
}

/// The names from `folder` down to `path`, when `path` is in `folder`
/// ([`in_folder`]'s rule, a part on Windows in any case). `None` when it is
/// not, or when a part below `folder` is anything but a name.
pub(crate) fn names_below<'a>(path: &'a Path, folder: &Path) -> Option<Vec<&'a std::ffi::OsStr>> {
    use std::path::Component;
    let same = |a: &std::ffi::OsStr, b: &std::ffi::OsStr| {
        if cfg!(target_os = "windows") {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        } else {
            a == b
        }
    };
    let mut parts = path.components();
    let mut matched = 0;
    for want in folder.components() {
        if !same(parts.next()?.as_os_str(), want.as_os_str()) {
            return None;
        }
        matched += 1;
    }
    if matched == 0 {
        return None;
    }
    parts
        .map(|part| match part {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect()
}

/// #3702 — the first folder link (a symbolic link, or on Windows a junction)
/// on the way from `folder` down to `path`, `folder` itself not counted;
/// `path` itself when it is not in `folder`. `None` when the way down holds no
/// link. A part that is not there ends the walk: nothing below it can be a
/// link. A part that cannot be read is answered as a link: what cannot be
/// read is not followed.
pub fn link_below(folder: &Path, path: &Path) -> Option<PathBuf> {
    let Some(names) = names_below(path, folder) else {
        return Some(path.to_path_buf());
    };
    let mut at = folder.to_path_buf();
    for name in names {
        at.push(name);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => return Some(at),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => return Some(at),
        }
    }
    None
}

/// Fill in `%SystemDrive%`, in any case, and nothing else. Anything else that
/// looks like a variable stays as it is: no variable is read here.
#[cfg(any(target_os = "windows", test))]
fn expand_system_drive(stored: &str, drive: &str) -> String {
    const NAME: &str = "%systemdrive%";
    match stored.to_ascii_lowercase().find(NAME) {
        Some(at) => format!("{}{drive}{}", &stored[..at], &stored[at + NAME.len()..]),
        None => stored.to_string(),
    }
}

/// A text value under `HKEY_LOCAL_MACHINE`, not expanded. `None` when it is not
/// there or is not text.
#[cfg(target_os = "windows")]
fn hklm_text(key: &str, value: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
    };
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let (key, value) = (wide(key), wide(value));
    let mut buf = [0u16; 1024];
    let mut bytes = std::mem::size_of_val(&buf) as u32;
    // SAFETY: NUL-terminated names, and a buffer with its size in bytes, for
    // one call; no type out-parameter is asked for.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != 0 {
        return None;
    }
    let chars = buf.get(..(bytes as usize) / 2)?;
    let end = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
    let text = String::from_utf16_lossy(&chars[..end]);
    (!text.is_empty()).then_some(text)
}

/// The machine-global tree (a perMachine/SystemContext host's enrolled config
/// lives here and must not be orphaned): the new folder if it exists, else the
/// old one if it exists, else the new one. This choice keys on the folders
/// themselves and is taken independently of [`project_dirs`]'s, which keys on
/// the config folders. Consumers `.join(...)` their subdir
/// (`config.toml`, `service-logs`, `crashes`, `staging`, ...).
#[cfg(target_os = "windows")]
pub fn machine_global_dir() -> PathBuf {
    MACHINE_GLOBAL
        .get_or_init(|| resolve_machine_global(&machine_program_data().join(ORG)))
        .clone()
}

/// [`machine_global_dir`]'s answer, once named.
#[cfg(target_os = "windows")]
static MACHINE_GLOBAL: OnceLock<PathBuf> = OnceLock::new();

/// The machine-global root under `base` (= `%PROGRAMDATA%\roomler`), resolved
/// new-then-old.
///
/// Split out of [`machine_global_dir`] so the decision can be TESTED. The public
/// function caches in a `OnceLock` against the real `%PROGRAMDATA%`, so a process
/// can only ever observe one answer — which left the pre-rename branch, the one
/// that keeps an upgraded host's enrolled config and its `staging\` dir
/// reachable, provable only by finding a host that still has such a tree. Every
/// host in the fleet is post-rename, so that proof was not available; this makes
/// it a test instead.
#[cfg(target_os = "windows")]
pub(crate) fn resolve_machine_global(base: &Path) -> PathBuf {
    let new = base.join(NEW_APP);
    let old = base.join(OLD_APP);
    if !new.exists() && old.exists() {
        old // pre-rename install still present -> keep it
    } else {
        new // fresh install, or the new tree is already present
    }
}

/// FR-92 — `<profile>\AppData\Local\<org>\<segment>\data`: what
/// [`project_dirs`]`().data_local_dir()` resolves to for the OWNER of
/// `profile`, computed from the profile path so a SYSTEM worker can reach the
/// signed-in person's per-user data (its own `project_dirs` is SYSTEM's).
/// #3747 — the tree is the one the owner's own processes pick
/// ([`segment_in_profile`]); the owner's Roaming AppData folder is taken at
/// its place below `profile`. Windows only, like the workers that need it.
#[cfg(target_os = "windows")]
pub fn data_local_dir_in_profile(profile: &Path) -> PathBuf {
    let app_data = profile.join("AppData");
    data_local_dir_in(
        &app_data.join("Roaming"),
        &app_data.join("Local"),
        Path::exists,
    )
}

/// [`data_local_dir_in_profile`] for the account whose Roaming and Local
/// AppData folders are `roaming` and `local`, with `exists` as the filesystem.
#[cfg(any(target_os = "windows", test))]
fn data_local_dir_in(roaming: &Path, local: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    local
        .join(ORG)
        .join(segment_in_profile(roaming, Some(local), exists))
        .join("data")
}

/// #3747 — the config folder of the account whose Roaming AppData folder is
/// `roaming` and whose Local AppData folder is `local` (`None` when not
/// known), in the tree that account's own processes pick
/// ([`segment_in_profile`]). The service host reads the console user's
/// enrollment here. `local` changes no config read: the local data folders
/// decide only where neither tree holds a config folder, and then neither
/// holds a `config.toml`.
#[cfg(any(target_os = "windows", test))]
pub fn config_dir_in_profile(
    roaming: &Path,
    local: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    roaming
        .join(ORG)
        .join(segment_in_profile(roaming, local, exists))
        .join("config")
}

/// #3747 — the tree the processes of the account whose Roaming and Local
/// AppData folders are `roaming` and `local` pick for themselves:
/// [`segment_of`] over that account's folders, laid out as `ProjectDirs` lays
/// them out on Windows. Every path into another account's profile is named
/// through this. `local` `None` takes that account's local data folders as
/// absent.
#[cfg(any(target_os = "windows", test))]
fn segment_in_profile(
    roaming: &Path,
    local: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> &'static str {
    segment_of(
        |app| Some(roaming.join(ORG).join(app).join("config")),
        |app| local.map(|local| local.join(ORG).join(app).join("data")),
        exists,
    )
}

/// The SCM-service log directory (`%PROGRAMDATA%\roomler\<segment>\service-logs`).
///
/// P3e lever E: canonical home for the path both sides need — the daemon's
/// `win_service::default_log_dir` delegates here, and the desktop companion
/// resolves the same directory to show/open SCM logs without linking the
/// daemon. Windows-only like [`machine_global_dir`].
#[cfg(target_os = "windows")]
pub fn service_log_dir() -> PathBuf {
    machine_global_dir().join("service-logs")
}

// ─── S1b: one-shot legacy-tree migration ───────────────────────────────────

/// Notes from the last [`migrate_legacy_trees`] run — logged by the caller
/// AFTER logging::init (migration must run before it, so it can't trace
/// directly). Empty = nothing noteworthy happened.
static MIGRATION_NOTES: OnceLock<Vec<String>> = OnceLock::new();

pub fn migration_notes() -> &'static [String] {
    MIGRATION_NOTES.get().map(|v| v.as_slice()).unwrap_or(&[])
}

/// One-shot startup migration of the legacy `roomler-agent` trees onto the
/// `roomler` segment. MUST run before `logging::init` and before ANY other
/// appdirs consumer (the segment decision is `OnceLock`-cached per process,
/// and log files open inside the tree).
///
/// Rules, per directory (per-user config/data roots + the Windows
/// machine-global root, each independent):
///   * legacy exists, new doesn't → atomic same-volume `fs::rename`
///   * both exist → a data pair merges ([`migrate_children`], #3746); the
///     config and machine pairs stay (the resolution keeps the tree that
///     holds the config folder, the new one when both do, and with no config
///     folder the new data folder's; `ConfigCleanupStale` cleans config up)
///   * legacy absent → nothing to do
///
/// Never deletes or overwrites; what cannot move (locked file, ACL) is noted,
/// stays, and moves at a later start. [`DataMerge`]: who runs the merge.
///
/// `companion_running` (the roomler-desktop tray): skip entirely — the tray
/// may hold open handles inside the tree, and a pre-S1a tray would keep
/// writing the old tree after the move, re-manufacturing the split.
///
/// #3702 — the move runs only for the account that owns the folders
/// ([`may_migrate`]): never in an installer step (`installer_step`), and the
/// per-user pairs only when each lies in `own_profile`, the profile the
/// system names for the account this process runs as. The pairs move by
/// [`migrate_dirs_within`], which decides for all of them before any pair
/// moves (none moves through a link below its root), moves the config pair
/// first and by one rename, and leaves the rest in the OLD tree when that
/// fails. The machine pair's root is [`machine_program_data`].
pub fn migrate_legacy_trees(
    companion_running: bool,
    installer_step: bool,
    own_profile: Option<&Path>,
    merge: DataMerge,
) {
    let mut notes = Vec::new();
    if companion_running {
        notes.push(
            "migration skipped: desktop companion is running (will retry next start)".to_string(),
        );
    } else {
        if let (Some(old), Some(new)) = (dirs_for(OLD_APP), dirs_for(NEW_APP)) {
            let mut pairs: Vec<(PathBuf, PathBuf)> = vec![
                (
                    old.config_dir().to_path_buf(),
                    new.config_dir().to_path_buf(),
                ),
                (old.data_dir().to_path_buf(), new.data_dir().to_path_buf()),
                (
                    old.data_local_dir().to_path_buf(),
                    new.data_local_dir().to_path_buf(),
                ),
            ];
            // data_dir == data_local_dir on Linux; config may coincide too on
            // some platforms. Adjacent duplicates only.
            pairs.dedup();
            let folders: Vec<PathBuf> = pairs
                .iter()
                .flat_map(|(o, n)| [o.clone(), n.clone()])
                .collect();
            match own_profile.filter(|_| may_migrate(installer_step, own_profile, &folders)) {
                Some(profile) => {
                    // The config pair is first in `pairs`: the order
                    // `migrate_dirs_within` keeps.
                    notes.extend(migrate_dirs_within(profile, &pairs, merge, rename_on_disk));
                    // Best-effort: drop the now-empty legacy shells (e.g.
                    // Windows `%APPDATA%\roomler\roomler-agent` after its
                    // `config`/`data` children moved out). `remove_dir`
                    // refuses non-empty dirs, so anything left behind
                    // survives; a shell reached through a link stays.
                    for parent in pairs
                        .iter()
                        .filter_map(|(o, _)| o.parent().map(Path::to_path_buf))
                    {
                        if parent.file_name().is_some_and(|f| f == OLD_APP)
                            && link_below(profile, &parent).is_none()
                        {
                            let _ = std::fs::remove_dir(&parent);
                        }
                    }
                }
                None => {
                    if let Some((old, _)) = pairs.iter().find(|(o, _)| o.exists()) {
                        notes.push(match own_profile {
                            _ if installer_step => in_an_installer_step(old),
                            Some(profile) => format!(
                                "legacy tree left in place: {} is not in this account's own profile {}",
                                folders
                                    .iter()
                                    .find(|f| !in_folder(f, profile))
                                    .unwrap_or(old)
                                    .display(),
                                profile.display()
                            ),
                            None => format!(
                                "legacy tree left in place: {} (the system names no profile for this account)",
                                old.display()
                            ),
                        });
                    }
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            let root = machine_program_data();
            let base = root.join(ORG);
            let pair = (base.join(OLD_APP), base.join(NEW_APP));
            if installer_step {
                if pair.0.exists() {
                    notes.push(in_an_installer_step(&pair.0));
                }
            } else {
                notes.extend(migrate_dirs_within(&root, &[pair], merge, rename_on_disk));
            }
        }
    }
    let _ = MIGRATION_NOTES.set(notes);
}

/// The note for a legacy tree an installer step leaves to the daemon.
fn in_an_installer_step(old: &Path) -> String {
    format!(
        "legacy tree left in place: {} moves at the daemon's own start, not in an installer step",
        old.display()
    )
}

/// #3702 — the home folder the system names for the account this process
/// runs as: its own entry in the account database (`getpwuid_r` of the
/// effective uid), never the environment. `None` when there is no entry or
/// it names no absolute folder.
#[cfg(unix)]
pub fn own_home() -> Option<PathBuf> {
    // SAFETY: `geteuid` is a thread-safe read of the caller's own credentials.
    account_home(unsafe { libc::geteuid() })
}

/// #3802 — the home folder the account database names for `uid`
/// (`getpwuid_r`), never the environment. `None` when there is no entry or it
/// names no absolute folder. The macOS supervisor reads the console person's
/// folder through it ([`macos_config_dir_in_home`]).
#[cfg(unix)]
pub fn account_home(uid: u32) -> Option<PathBuf> {
    with_passwd(uid, |entry| {
        // SAFETY: the entry's strings point into the buffer `with_passwd`
        // holds for this call.
        let home = PathBuf::from(unsafe { passwd_text(entry.pw_dir) }?);
        home.is_absolute().then_some(home)
    })
}

/// FR-147 — an account as the account database names it (`getpwuid_r`),
/// never the environment: what `sudo roomlerd one-row` starts a person's own
/// process with ([`account_entry`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountEntry {
    pub uid: u32,
    /// `pw_name`, as the database spells it.
    pub name: std::ffi::OsString,
    /// `pw_gid`: the account's primary group.
    pub gid: u32,
    /// `pw_dir`, as the database names it; whether it is an absolute folder
    /// is the reader's to check.
    pub home: PathBuf,
}

/// FR-147 — `uid`'s entry in the account database: its name, its primary
/// group and its home. `None` when there is no entry, or it has no name or no
/// home.
#[cfg(unix)]
pub fn account_entry(uid: u32) -> Option<AccountEntry> {
    with_passwd(uid, |entry| {
        // SAFETY: as in `account_home`.
        let (name, home) = unsafe { (passwd_text(entry.pw_name)?, passwd_text(entry.pw_dir)?) };
        Some(AccountEntry {
            uid,
            name,
            gid: entry.pw_gid,
            home: PathBuf::from(home),
        })
    })
}

/// `getpwuid_r` of `uid`, and `read` of the entry while the buffer its
/// strings point into is alive. `None` when the database has no entry.
#[cfg(unix)]
fn with_passwd<T>(uid: u32, read: impl FnOnce(&libc::passwd) -> Option<T>) -> Option<T> {
    let mut buf: Vec<libc::c_char> = vec![0; 1024];
    loop {
        // SAFETY: an all-zero `passwd` is a valid value of a plain C struct;
        // `getpwuid_r` fills it in.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer names memory this frame owns for the whole
        // call, and `buf.len()` is the length of `buf`, where the entry's
        // strings are written.
        let rc =
            unsafe { libc::getpwuid_r(uid, &mut entry, buf.as_mut_ptr(), buf.len(), &mut found) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || found.is_null() {
            return None;
        }
        return read(&entry);
    }
}

/// One of a `passwd` entry's strings, as bytes; `None` for a null pointer.
///
/// # Safety
/// `text` is null, or points to a NUL-terminated string that outlives the
/// call (`with_passwd`'s buffer).
#[cfg(unix)]
unsafe fn passwd_text(text: *const libc::c_char) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStrExt;
    if text.is_null() {
        return None;
    }
    // SAFETY: the caller's promise.
    let text = unsafe { std::ffi::CStr::from_ptr(text) };
    Some(std::ffi::OsStr::from_bytes(text.to_bytes()).to_os_string())
}

/// #3702 — may the one-time folder move run in this process over `folders`?
/// Never in an installer step, and only when every folder lies in
/// `own_profile`, the profile (or on Unix the home) the system names for the
/// account this process runs as: a folder anywhere else is another
/// account's, moved by that account's own process.
pub fn may_migrate(installer_step: bool, own_profile: Option<&Path>, folders: &[PathBuf]) -> bool {
    !installer_step && own_profile.is_some_and(|own| folders.iter().all(|f| in_folder(f, own)))
}

/// #3702 — why every pair in `root` (a profile, or the machine's data
/// folder) stays where it is, decided for all of them before any pair moves:
/// the first folder of a pair with a legacy folder to move that lies outside
/// `root`, or is reached through a folder link below it ([`link_below`]),
/// named in one note. `None` when every pair may move.
fn kept_in_place(root: &Path, pairs: &[(PathBuf, PathBuf)]) -> Option<String> {
    for (old, new) in pairs.iter().filter(|(old, _)| old.exists()) {
        for path in [old, new] {
            if !in_folder(path, root) {
                return Some(format!(
                    "legacy tree left in place: {} is not in {}",
                    path.display(),
                    root.display()
                ));
            }
            if let Some(link) = link_below(root, path) {
                return Some(format!(
                    "legacy tree left in place: {} is a link",
                    link.display()
                ));
            }
        }
    }
    None
}

/// #3702 — [`migrate_dir`] for the pairs in `root`, with `rename` as the
/// rename of a pair's folder or of an entry a merge moves ([`rename_on_disk`]
/// outside tests). Whether they move is decided for every pair before any
/// pair moves: none moves when one lies outside `root` or is reached through
/// a link below it ([`kept_in_place`]). They then move in the order given,
/// the config pair first and by one rename; its failure ends the move, and
/// the pairs after it stay in the OLD tree. A data pair converges on its own
/// (#3746, [`DataMerge`]). The segment choice ([`segment_of`]) takes the tree
/// that holds a config folder. The move makes the NEW config folder only
/// whole, by one rename, and the NEW local data folder only when the config
/// pair's move did not fail: the daemon reads the config it had, from the NEW
/// tree or from the OLD one.
fn migrate_dirs_within(
    root: &Path,
    pairs: &[(PathBuf, PathBuf)],
    data_merge: DataMerge,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Vec<String> {
    if let Some(note) = kept_in_place(root, pairs) {
        return vec![note];
    }
    let mut notes = Vec::new();
    for (at, (old, new)) in pairs.iter().enumerate() {
        // The first pair holds `config.toml`: one rename moves its folder
        // whole, or the folder stays. A data pair after it merges, and one
        // that leaves an entry behind holds back no pair after it.
        match migrate_dir(old, new, (at > 0).then_some(data_merge), &mut rename) {
            Ok(note) => notes.extend(note),
            Err(note) if at > 0 => notes.push(note),
            Err(note) => {
                notes.push(note);
                notes.extend(pairs[at + 1..].iter().filter(|(old, _)| old.exists()).map(
                    |(old, _)| {
                        format!(
                            "legacy tree left in place: {} (a move before it failed)",
                            old.display()
                        )
                    },
                ));
                break;
            }
        }
    }
    notes
}

/// The system's own rename: the one [`migrate_dirs_within`] takes outside
/// tests. A real folder renames as the system renames one; anything else (a
/// file, or a link, which is never followed) only to a free name
/// ([`rename_no_replace`]), so a merge never takes a name that is taken.
fn rename_on_disk(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(from)?.is_dir() {
        return std::fs::rename(from, to);
    }
    rename_no_replace(from, to)
}

/// Apply the migration rules to one directory pair, with `rename` as the
/// rename of its folder: `Ok` with a note for anything noteworthy (moved, or
/// left beside a NEW folder that is already there), `Ok(None)` for the quiet
/// no-legacy case, `Err` with a note when the move failed and the legacy
/// folder is still where it was. A data pair (`merge` is `Some`) merges into
/// a NEW folder that is there, or when its folder's rename fails, entry by
/// entry ([`migrate_children`]); the config and machine pairs never do.
fn migrate_dir(
    old: &Path,
    new: &Path,
    merge: Option<DataMerge>,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<Option<String>, String> {
    if !old.exists() {
        return Ok(None);
    }
    if new.exists() {
        return match merge {
            Some(merge) => migrate_children(old, new, merge, None, &mut rename).map(Some),
            None => Ok(Some(format!(
                "legacy tree left in place (new tree also present): {}",
                old.display()
            ))),
        };
    }
    if let Some(parent) = new.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Err(format!(
            "migration failed (create {}): {e}",
            parent.display()
        ));
    }
    match rename(old, new) {
        Ok(()) => Ok(Some(format!(
            "migrated legacy tree {} -> {}",
            old.display(),
            new.display()
        ))),
        // Field finding (DEVBOX, 2026-07-27): renaming the TOP dir was
        // denied by an external ROOT-handle (an AV / indexer
        // directory-watch — no roomler process held anything, ACLs
        // were Full, and every CHILD renamed fine). A data pair merges
        // its children instead; that dodges the root-handle without
        // needing the watcher gone, and a held child waits for a later start.
        Err(e) => match merge {
            Some(merge) => migrate_children(old, new, merge, Some(&e), &mut rename).map(Some),
            None => Err(format!(
                "migration failed ({} -> {}): {e} — read-both fallback stays in charge",
                old.display(),
                new.display()
            )),
        },
    }
}

// ─── Linux: the system config lives in /etc/roomler ────────────────────────

/// Canonical config for a Linux SYSTEM install (the `roomlerd.service` unit).
///
/// The packaged unit has run `roomlerd --config /etc/roomler/config.toml`
/// since rc.426, but hosts enrolled before that convention keep their config
/// in root's profile (`/root/.config/roomler/config.toml`) — so the FIRST
/// systemd-mediated start after the packaging change died `no config found`
/// and `Restart=always` turned it into a crash-loop. Field-hit on three nodes
/// (buildhost, fleet-host-2, and the WSL sibling, the last one from a plain `dpkg -i`, with
/// no operator restart involved at all).
///
/// One canonical location fixes that for good: [`migrate_system_config`] moves
/// a legacy config here at startup, and [`crate::config::default_config_path`]
/// resolves here for root, so an explicit `--config` and a bare `roomlerd run`
/// can no longer disagree about where this host's identity lives.
#[cfg(target_os = "linux")]
pub fn system_config_path() -> PathBuf {
    PathBuf::from("/etc/roomler/config.toml")
}

/// Is this process root? Only root may own `/etc/roomler`; a per-user Linux
/// install keeps its profile config and must not be migrated anywhere.
#[cfg(target_os = "linux")]
pub fn running_as_root() -> bool {
    // SAFETY: `geteuid` is a thread-safe read of the caller's own credentials.
    unsafe { libc::geteuid() == 0 }
}

/// One-shot migration of a legacy root-profile config into `/etc/roomler`.
///
/// Same rules as [`migrate_legacy_trees`], for the same reasons:
///   * `/etc` copy exists → leave the legacy one alone (NEW wins)
///   * legacy absent → nothing to do
///   * legacy present, `/etc` absent → `rename` (same filesystem in practice)
///
/// **Never deletes.** A failed move is noted and the read-both resolution in
/// `default_config_path` keeps the host running on its legacy config until the
/// next start retries — a daemon that cannot find its identity is exactly the
/// outcome this exists to prevent.
///
/// The `.prev` sibling (config::save's rollback copy) moves too: leaving it
/// behind would strand a second, credential-bearing copy of the config at a
/// path something might later be pointed back at.
///
/// MUST run before `logging::init`, like the tree migration — hence the same
/// note-collecting instead of tracing.
///
/// #3702 — like the tree migration, it moves only root's own config: one in
/// `own_home`, the home the system names for this account ([`own_home`]),
/// reached by no link below it ([`system_config_home`]).
#[cfg(target_os = "linux")]
pub fn migrate_system_config(own_home: Option<&Path>) {
    let mut notes = Vec::new();
    if !running_as_root() {
        let _ = SYSTEM_CONFIG_NOTES.set(notes);
        return;
    }
    let new = system_config_path();
    let legacy = ProjectDirs::from(QUALIFIER, ORG, NEW_APP)
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_default();

    if new.exists() || !legacy.exists() {
        let _ = SYSTEM_CONFIG_NOTES.set(notes);
        return;
    }
    let home = match system_config_home(own_home, &legacy) {
        Ok(home) => home,
        Err(note) => {
            notes.push(note);
            let _ = SYSTEM_CONFIG_NOTES.set(notes);
            return;
        }
    };
    if let Some(parent) = new.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        notes.push(format!(
            "system-config migration failed (create {}): {e}",
            parent.display()
        ));
        let _ = SYSTEM_CONFIG_NOTES.set(notes);
        return;
    }
    // 0700: the config carries the agent token and the WG secret key.
    #[cfg(unix)]
    if let Some(parent) = new.parent() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }

    match std::fs::rename(&legacy, &new) {
        Ok(()) => {
            notes.push(format!(
                "migrated system config {} -> {}",
                legacy.display(),
                new.display()
            ));
            // Best-effort; a left-behind `.prev` is untidy, not dangerous,
            // and must never turn a successful migration into a failure.
            let (lp, np) = (with_prev(&legacy), with_prev(&new));
            if lp.exists() && !np.exists() && link_below(home, &lp).is_none() {
                let _ = std::fs::rename(&lp, &np);
            }
        }
        Err(e) => notes.push(format!(
            "system-config migration failed ({} -> {}): {e} — staying on the legacy path",
            legacy.display(),
            new.display()
        )),
    }
    let _ = SYSTEM_CONFIG_NOTES.set(notes);
}

/// #3702 — the home a legacy root-profile config (`legacy`) may move out of:
/// `own_home`, when `legacy` lies in it and the way down to it holds no link.
/// `Err` is the note that says why it stays.
#[cfg(target_os = "linux")]
fn system_config_home<'a>(own_home: Option<&'a Path>, legacy: &Path) -> Result<&'a Path, String> {
    match own_home {
        Some(home) if may_migrate(false, Some(home), &[legacy.to_path_buf()]) => {
            match link_below(home, legacy) {
                None => Ok(home),
                Some(link) => Err(format!(
                    "system config left in place: {} is a link",
                    link.display()
                )),
            }
        }
        Some(home) => Err(format!(
            "system config left in place: {} is not in this account's own home {}",
            legacy.display(),
            home.display()
        )),
        None => Err(format!(
            "system config left in place: {} (the system names no home for this account)",
            legacy.display()
        )),
    }
}

#[cfg(target_os = "linux")]
fn with_prev(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(".prev");
    PathBuf::from(s)
}

#[cfg(target_os = "linux")]
static SYSTEM_CONFIG_NOTES: OnceLock<Vec<String>> = OnceLock::new();

/// Notes from [`migrate_system_config`], logged by the caller after
/// `logging::init` (the migration runs before it).
#[cfg(target_os = "linux")]
pub fn system_config_notes() -> &'static [String] {
    SYSTEM_CONFIG_NOTES
        .get()
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

/// #3746 — the merge of a data pair's OLD folder into its NEW folder, entry
/// by entry ([`merge_folder`]): when the NEW folder is already there (`why`
/// is `None`, as once that start's `logging::init` made `logs` in it), or
/// when the folder's own rename failed (`why`). A folder both hold merges
/// the same way inside it; any other name both hold keeps the copy modified
/// last, a tie the NEW folder's, and the other copy goes beside it
/// ([`KEPT_BESIDE`]). Nothing is overwritten or deleted, and an entry gone
/// meanwhile (another runner moved it) is done. `Ok` once the OLD folder is
/// gone, `Err` while an entry is left in it for the next start; the note
/// names what was kept beside and what was left.
fn migrate_children(
    old: &Path,
    new: &Path,
    merge: DataMerge,
    why: Option<&std::io::Error>,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<String, String> {
    let how = match why {
        None => "the new folder was already there".to_string(),
        Some(e) => format!("its folder rename failed: {e}"),
    };
    if gone(old) {
        return Ok(format!(
            "legacy tree {} already moved by another start",
            old.display()
        ));
    }
    if let Some(because) = merge.left_because() {
        return Err(format!(
            "legacy tree left in place ({how}): {} — {because}",
            old.display()
        ));
    }
    if !is_real_folder(old) {
        return Err(format!(
            "legacy tree left in place: {} is not a folder",
            old.display()
        ));
    }
    let made_new = match std::fs::create_dir(new) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(format!("migration failed (create {}): {e}", new.display())),
    };
    if !is_real_folder(new) {
        return Err(format!(
            "legacy tree left in place: {} is not a folder",
            new.display()
        ));
    }
    let mut merged = Merged::default();
    merge_folder(old, new, "", rename, &mut merged);
    let old_gone = match std::fs::remove_dir(old) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            if merged.left.is_empty() {
                merged.leave("", &e);
            }
            false
        }
    };
    // A NEW folder this merge made goes again while it is empty: the segment
    // choice keys on a new data folder where neither tree holds a config
    // folder. `remove_dir` leaves it while anything is in it.
    if made_new && !old_gone {
        let _ = std::fs::remove_dir(new);
    }
    let note = merged.note(old, new, &how, old_gone);
    if old_gone { Ok(note) } else { Err(note) }
}

/// #3746 — whether this process runs the data pairs' merge
/// ([`migrate_children`]). A process running elevated from a split token
/// does not: the account's own unelevated daemon runs it at its next start,
/// so the merge never runs above the integrity level of the folder owner's
/// own processes. A pair's one folder rename is the same in every process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataMerge {
    /// The merge runs.
    Runs,
    /// Left to the account's own daemon: this process runs elevated from a
    /// split token.
    Elevated,
    /// Left to the account's own daemon: this process could not read its
    /// own token.
    TokenUnread,
}

impl DataMerge {
    /// From whether this process runs elevated from a split token:
    /// `Some(true)` it does, `Some(false)` it does not, `None` it could not
    /// tell.
    pub fn from_split_elevation(elevated: Option<bool>) -> Self {
        match elevated {
            Some(false) => Self::Runs,
            Some(true) => Self::Elevated,
            None => Self::TokenUnread,
        }
    }

    /// Why the merge is left, for the note; `None` when it runs.
    fn left_because(self) -> Option<&'static str> {
        match self {
            Self::Runs => None,
            Self::Elevated => Some(
                "this process runs elevated; the account's own daemon merges it at its next start",
            ),
            Self::TokenUnread => Some(
                "this process could not read its own token; the account's own daemon merges it \
                 at its next start",
            ),
        }
    }
}

/// #3746 — the suffix of a copy the merge keeps beside a name both folders
/// hold: `<name>.older`, then `<name>.older-2`, `<name>.older-3`, … when the
/// name before it is taken.
pub const KEPT_BESIDE: &str = ".older";

/// How many names beside one name [`put_beside`] tries.
const BESIDE_TRIES: u32 = 100;

/// #3746 — whether `name` is a copy the merge kept beside another
/// (`<name>.older` or `<name>.older-<n>`), which no reader takes for the
/// file of that name.
pub fn is_kept_beside(name: &str) -> bool {
    let Some(at) = name.rfind(KEPT_BESIDE) else {
        return false;
    };
    let rest = &name[at + KEPT_BESIDE.len()..];
    at > 0
        && (rest.is_empty()
            || rest
                .strip_prefix('-')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
}

/// The `n`th name beside `name`: `<name>.older`, `<name>.older-2`, …
fn beside_name(name: &Path, n: u32) -> PathBuf {
    let mut at = name.as_os_str().to_os_string();
    at.push(beside_suffix(n));
    PathBuf::from(at)
}

fn beside_suffix(n: u32) -> String {
    match n {
        1 => KEPT_BESIDE.to_string(),
        n => format!("{KEPT_BESIDE}-{n}"),
    }
}

/// Nothing is at `path`, read without following a link.
fn gone(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// A real folder is at `path`, not a link to one.
fn is_real_folder(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// What a merge did, for its note.
#[derive(Default)]
struct Merged {
    moved: usize,
    /// Each copy kept beside a name, by its path from the pair's folder.
    beside: Vec<String>,
    /// Each entry left in the OLD folder, with why.
    left: Vec<String>,
}

impl Merged {
    fn kept_beside(&mut self, rel: &str, n: u32, whose: &str) {
        let suffix = beside_suffix(n);
        self.beside
            .push(format!("{rel}{suffix} (the {whose} folder's copy)"));
    }

    fn leave(&mut self, rel: &str, e: &std::io::Error) {
        let rel = if rel.is_empty() {
            "the folder itself"
        } else {
            rel
        };
        self.left.push(format!("{rel} ({e})"));
    }

    fn note(&self, old: &Path, new: &Path, how: &str, old_gone: bool) -> String {
        let mut note = format!(
            "{} {} -> {} entry by entry ({how}): {} moved",
            if old_gone {
                "migrated legacy tree"
            } else {
                "legacy tree partly migrated"
            },
            old.display(),
            new.display(),
            self.moved
        );
        if !self.beside.is_empty() {
            note.push_str("; kept beside: ");
            note.push_str(&listed(&self.beside));
        }
        if !old_gone {
            note.push_str("; left in the old folder for the next start: ");
            note.push_str(&listed(&self.left));
        }
        note
    }
}

/// `items` joined for a note, the first 32 of them.
fn listed(items: &[String]) -> String {
    const SHOWN: usize = 32;
    let mut text = items
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > SHOWN {
        text.push_str(&format!(" and {} more", items.len() - SHOWN));
    }
    text
}

/// A name's path from the pair's folder, for the note: `rel/name`.
fn joined(rel: &str, name: &std::ffi::OsStr) -> String {
    let name = name.to_string_lossy();
    if rel.is_empty() {
        name.into_owned()
    } else {
        format!("{rel}/{name}")
    }
}

/// Every entry of `src_dir`, an OLD folder, into `dst_dir`, the same place
/// in the NEW folder, by [`merge_entry`]. `rel` is `src_dir`'s path from the
/// pair's folder. `config.toml` goes last.
fn merge_folder(
    src_dir: &Path,
    dst_dir: &Path,
    rel: &str,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
    merged: &mut Merged,
) {
    let entries = match std::fs::read_dir(src_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => return merged.leave(rel, &e),
    };
    let mut names = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => names.push(entry.file_name()),
            Err(e) => {
                merged.leave(rel, &e);
                break;
            }
        }
    }
    // config.toml last: the enrollment must be the final thing to leave
    // the old tree.
    names.sort_by(|a, b| (a == "config.toml", a).cmp(&(b == "config.toml", b)));
    for name in names {
        let (src, dst) = (src_dir.join(&name), dst_dir.join(&name));
        merge_entry(&src, &dst, &joined(rel, &name), rename, merged);
    }
}

/// One entry of an OLD folder (`src`) to the same place in the NEW folder
/// (`dst`), each read as it is now and never followed. An entry gone
/// meanwhile was moved by another runner, and is done. To a free name it
/// moves by one rename; a real folder whose rename fails merges entry by
/// entry; a name the NEW folder holds too is settled by [`settle`].
fn merge_entry(
    src: &Path,
    dst: &Path,
    rel: &str,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
    merged: &mut Merged,
) {
    if gone(src) {
        return;
    }
    if gone(dst) {
        let e = match rename(src, dst) {
            Ok(()) => {
                merged.moved += 1;
                return;
            }
            Err(e) => e,
        };
        if gone(src) {
            return;
        }
        if gone(dst) {
            if !is_real_folder(src) {
                return merged.leave(rel, &e);
            }
            // A folder whose own rename failed (a handle held on it, or on
            // something in it): its entries, one by one.
            match std::fs::create_dir(dst) {
                Ok(()) => return merge_subfolder(src, dst, rel, rename, merged, true),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return merged.leave(rel, &e),
            }
        }
    }
    settle(src, dst, rel, rename, merged);
}

/// A folder both folders hold (`made`: one this merge just made in the NEW
/// folder): its entries merge, and the OLD folder goes once it is empty. A
/// folder this merge made goes again while the OLD one stays and it is
/// empty.
fn merge_subfolder(
    src: &Path,
    dst: &Path,
    rel: &str,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
    merged: &mut Merged,
    made: bool,
) {
    let noted = merged.left.len();
    merge_folder(src, dst, rel, rename, merged);
    match std::fs::remove_dir(src) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            if merged.left.len() == noted {
                merged.leave(rel, &e);
            }
            if made {
                let _ = std::fs::remove_dir(dst);
            }
        }
    }
}

/// A name both folders hold. Two real folders merge ([`merge_subfolder`]);
/// anything else is settled by the rule: the copy modified last keeps the
/// name, a tie the NEW folder's, and the other copy goes beside it
/// ([`put_beside`]). Nothing is overwritten or deleted.
fn settle(
    src: &Path,
    dst: &Path,
    rel: &str,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
    merged: &mut Merged,
) {
    let old = match std::fs::symlink_metadata(src) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => return merged.leave(rel, &e),
    };
    let new = match std::fs::symlink_metadata(dst) {
        Ok(meta) => meta,
        // The NEW folder's copy went meanwhile: the name is free again.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return take_name(src, dst, rel, rename, merged);
        }
        Err(e) => return merged.leave(rel, &e),
    };
    if old.is_dir() && new.is_dir() {
        return merge_subfolder(src, dst, rel, rename, merged, false);
    }
    let modified =
        |meta: &std::fs::Metadata| meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    if modified(&old) <= modified(&new) {
        // The NEW folder's copy keeps the name; the OLD one goes beside it.
        match put_beside(src, dst, rename) {
            Beside::Put(n) => merged.kept_beside(rel, n, "old"),
            Beside::Gone => {}
            Beside::Failed(e) => merged.leave(rel, &e),
        }
        return;
    }
    // The OLD folder's copy is newer: the NEW one goes beside, and the OLD
    // one takes the name.
    match put_beside(dst, dst, rename) {
        Beside::Put(n) => match rename(src, dst) {
            Ok(()) => {
                merged.moved += 1;
                merged.kept_beside(rel, n, "new");
            }
            Err(e) => {
                // The NEW folder's copy goes back under its name while that
                // is free, and stays beside it otherwise.
                if rename(&beside_name(dst, n), dst).is_err() {
                    merged.kept_beside(rel, n, "new");
                }
                if !gone(src) {
                    merged.leave(rel, &e);
                }
            }
        },
        Beside::Gone => take_name(src, dst, rel, rename, merged),
        Beside::Failed(e) => merged.leave(rel, &e),
    }
}

/// `src` to `dst`, a name that was free when last read: another runner may
/// take it first, and the next start settles it then.
fn take_name(
    src: &Path,
    dst: &Path,
    rel: &str,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
    merged: &mut Merged,
) {
    match rename(src, dst) {
        Ok(()) => merged.moved += 1,
        Err(e) if !gone(src) => merged.leave(rel, &e),
        Err(_) => {}
    }
}

/// What [`put_beside`] did.
enum Beside {
    /// Renamed to the `n`th name beside ([`beside_name`]).
    Put(u32),
    /// The entry went meanwhile: another runner moved it.
    Gone,
    Failed(std::io::Error),
}

/// `from` renamed to the first free name beside `name`: `<name>.older`, then
/// `<name>.older-2`, … A name taken meanwhile is skipped.
fn put_beside(
    from: &Path,
    name: &Path,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Beside {
    let mut last = None;
    for n in 1..=BESIDE_TRIES {
        let at = beside_name(name, n);
        if !gone(&at) {
            continue;
        }
        match rename(from, &at) {
            Ok(()) => return Beside::Put(n),
            Err(_) if gone(from) => return Beside::Gone,
            Err(e) if !gone(&at) => last = Some(e),
            Err(e) => return Beside::Failed(e),
        }
    }
    Beside::Failed(last.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "every name beside it is taken",
        )
    }))
}

/// #3746 — `from` renamed to `to` only while `to` is free: `AlreadyExists`
/// when anything is there. `from` itself is renamed, never what a link at it
/// leads to.
#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let (from, to) = (wide_path(from)?, wide_path(to)?);
    // SAFETY: both are NUL-terminated wide strings that outlive the call.
    // Flags 0: no replace, and no copy to another volume.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `path` as a NUL-terminated wide string; a NUL inside it is refused.
#[cfg(windows)]
fn wide_path(path: &Path) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a NUL in the path",
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// #3746 — as on Windows, by `renameat2` with `RENAME_NOREPLACE`, made as a
/// raw system call: libc's own wrapper needs glibc 2.28 at load time. A
/// kernel or filesystem without the flag takes [`link_then_unlink`].
#[cfg(target_os = "linux")]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let (from_c, to_c) = (c_path(from)?, c_path(to)?);
    // SAFETY: both are NUL-terminated strings that outlive the call, and
    // `renameat2` takes (dirfd, path, dirfd, path, flags).
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD as libc::c_long,
            from_c.as_ptr(),
            libc::AT_FDCWD as libc::c_long,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE as libc::c_long,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOSYS) | Some(libc::EINVAL) => link_then_unlink(from, to),
        _ => Err(e),
    }
}

/// #3746 — as on Windows, by `renamex_np` with `RENAME_EXCL`. A filesystem
/// without the flag takes [`link_then_unlink`].
#[cfg(target_vendor = "apple")]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let (from_c, to_c) = (c_path(from)?, c_path(to)?);
    // SAFETY: both are NUL-terminated strings that outlive the call.
    if unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) } == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOTSUP) | Some(libc::EINVAL) => link_then_unlink(from, to),
        _ => Err(e),
    }
}

/// #3746 — elsewhere, [`link_then_unlink`].
#[cfg(all(unix, not(target_os = "linux"), not(target_vendor = "apple")))]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    link_then_unlink(from, to)
}

/// `path` as a C string; a NUL inside it is refused.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn c_path(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a NUL in the path"))
}

/// A rename that never takes a taken name, where the system has none: a
/// second link to `from` at `to` (refused when `to` is taken), then `from`'s
/// own link removed. When that removal fails, the link at `to` goes again,
/// and only while it is still `from`'s.
#[cfg(unix)]
fn link_then_unlink(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    std::fs::hard_link(from, to)?;
    if let Err(e) = std::fs::remove_file(from) {
        if let (Ok(a), Ok(b)) = (
            std::fs::symlink_metadata(from),
            std::fs::symlink_metadata(to),
        ) && (a.dev(), a.ino()) == (b.dev(), b.ino())
        {
            let _ = std::fs::remove_file(to);
        }
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-43 P3c — a person's folder, as their own process would pick it.
    #[test]
    fn a_persons_folder_is_the_one_their_own_process_picks() {
        let home = Path::new("/Users/someone");
        let support = home.join("Library").join("Application Support");
        let current = support.join("live.roomler.roomler");
        let old = support.join(format!("{QUALIFIER}.{ORG}.{OLD_APP}"));
        // Neither is there yet: the current one, which a first start creates.
        assert_eq!(macos_config_dir_in_home(home, |_| false), current);
        // Only the pre-rename one: that host's enrollment lives there.
        assert_eq!(macos_config_dir_in_home(home, |p| p == old), old);
        // Both: the current one, as for the person.
        assert_eq!(macos_config_dir_in_home(home, |_| true), current);
    }

    /// #3747 — a person's Roaming and Local AppData folders.
    fn persons_app_data() -> (PathBuf, PathBuf) {
        let app_data = Path::new(r"C:\Users\person").join("AppData");
        (app_data.join("Roaming"), app_data.join("Local"))
    }

    /// One of the four folders the tree choice keys on in that person's
    /// profile, laid out as `ProjectDirs` lays them out on Windows: `OC` and
    /// `NC` the old and new config folder, `OD` and `ND` the old and new
    /// local data folder.
    fn folder(name: &str) -> PathBuf {
        let (roaming, local) = persons_app_data();
        let (base, app, leaf) = match name {
            "OC" => (roaming, OLD_APP, "config"),
            "NC" => (roaming, NEW_APP, "config"),
            "OD" => (local, OLD_APP, "data"),
            "ND" => (local, NEW_APP, "data"),
            other => panic!("no folder {other}"),
        };
        base.join(ORG).join(app).join(leaf)
    }

    /// `exists` for that profile when it holds exactly the folders `held`
    /// names (`"OC ND"`).
    fn holding(held: &str) -> impl Fn(&Path) -> bool {
        let folders: Vec<PathBuf> = held.split_whitespace().map(folder).collect();
        move |path| folders.iter().any(|f| f == path)
    }

    /// The tree the person's own process picks ([`segment_of`] over its
    /// `ProjectDirs`) when the profile holds `held`.
    fn own_pick(held: &str) -> &'static str {
        let (roaming, local) = persons_app_data();
        segment_of(
            |app| Some(roaming.join(ORG).join(app).join("config")),
            |app| Some(local.join(ORG).join(app).join("data")),
            holding(held),
        )
    }

    /// #3747 — the local data folder a process names in another account's
    /// profile (the SystemContext worker's keep-busy store) is the one that
    /// account's own processes pick, whatever the naming process picked for
    /// itself.
    #[test]
    fn a_profiles_local_data_folder_is_its_owners_tree() {
        let (roaming, local) = persons_app_data();
        for held in ["OC", "OC OD"] {
            assert_eq!(
                data_local_dir_in(&roaming, &local, holding(held)),
                folder("OD"),
                "only the old tree: {held}"
            );
        }
        for held in ["NC", "NC ND"] {
            assert_eq!(
                data_local_dir_in(&roaming, &local, holding(held)),
                folder("ND"),
                "the new tree: {held}"
            );
        }
    }

    /// #3747 — the service host reads the console user's config in the tree
    /// that holds it: the old one when only that exists, the new one when it
    /// exists. Whether it knows the person's local folder changes nothing.
    #[test]
    fn the_service_host_reads_the_console_users_config_in_their_tree() {
        let (roaming, local) = persons_app_data();
        for local in [None, Some(local.as_path())] {
            for held in ["OC", "OC OD"] {
                assert_eq!(
                    config_dir_in_profile(&roaming, local, holding(held)),
                    folder("OC"),
                    "only the old tree: {held}, local folder {local:?}"
                );
            }
            for held in ["NC", "NC ND"] {
                assert_eq!(
                    config_dir_in_profile(&roaming, local, holding(held)),
                    folder("NC"),
                    "the new tree: {held}, local folder {local:?}"
                );
            }
        }
    }

    /// #3747 — an old config folder beside a new local data folder, with no
    /// new config folder, is the old tree: for the account's own process and
    /// for a process that names that account's folders. The enrollment is in
    /// the old config folder.
    #[test]
    fn an_old_config_folder_beats_a_new_data_folder() {
        let (roaming, local) = persons_app_data();
        for held in ["OC ND", "OC OD ND"] {
            assert_eq!(own_pick(held), OLD_APP, "its own process: {held}");
            assert_eq!(
                config_dir_in_profile(&roaming, Some(&local), holding(held)),
                folder("OC"),
                "its config, named by another account: {held}"
            );
            assert_eq!(
                data_local_dir_in(&roaming, &local, holding(held)),
                folder("OD"),
                "its local data, named by another account: {held}"
            );
        }
    }

    /// #3747 — every other row of the rule stays as it was.
    #[test]
    fn the_rule_keeps_its_unchanged_rows() {
        for (held, want) in [
            // Nothing: a fresh install.
            ("", NEW_APP),
            // Only the new tree.
            ("NC", NEW_APP),
            ("NC ND", NEW_APP),
            // Both config folders.
            ("OC NC", NEW_APP),
            ("OC NC OD", NEW_APP),
            ("OC NC ND", NEW_APP),
            ("OC NC OD ND", NEW_APP),
            // No config folder in either tree: the data folders decide.
            ("OD", OLD_APP),
            ("ND", NEW_APP),
            ("OD ND", NEW_APP),
            // The other rows that do not change.
            ("NC OD", NEW_APP),
            ("NC OD ND", NEW_APP),
            ("OC", OLD_APP),
            ("OC OD", OLD_APP),
        ] {
            assert_eq!(own_pick(held), want, "{held:?}");
        }
    }

    /// FR-129 C — what the wizard's Windows path names by full path is there.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_tools_are_where_the_helper_says() {
        for tool in ["icacls.exe", "msiexec.exe", "schtasks.exe", "cmd.exe"] {
            let path = system32_tool(tool);
            assert!(path.is_absolute(), "{}", path.display());
            assert!(path.is_file(), "{}", path.display());
        }
        assert!(windows_directory().join("explorer.exe").is_file());
    }

    /// FR-129 C — the machine's folders do not move with the environment: the
    /// same test in a child process whose variables say otherwise, as a value
    /// in `HKCU\Environment` would, reads the same answer.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_machine_folders_ignore_what_the_environment_says() {
        const ANSWER: &str = "ROOMLER_TEST_FOLDERS_ANSWER=";
        let answer = format!(
            "{ANSWER}{:?} {:?} {:?} {:?}",
            machine_folder_variables(),
            system_folder_variables(),
            machine_program_data(),
            machine_program_files(),
        );
        if std::env::var_os("ROOMLER_TEST_FOLDERS_CHILD").is_some() {
            println!("{answer}");
            return;
        }
        assert!(!answer.contains("poison"), "{answer}");
        for (name, value) in machine_folder_variables() {
            assert!(value.is_absolute() || name == "SystemDrive", "{name}");
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "appdirs::tests::the_machine_folders_ignore_what_the_environment_says",
                "--nocapture",
            ])
            .env("ROOMLER_TEST_FOLDERS_CHILD", "1")
            .env("ProgramFiles", r"C:\poison")
            .env("ProgramW6432", r"C:\poison")
            .env("ProgramData", r"C:\poison")
            .env("ALLUSERSPROFILE", r"C:\poison")
            .env("SystemDrive", "P:")
            .env("windir", r"C:\poison")
            .env("ComSpec", r"C:\poison\cmd.exe")
            .env("USERPROFILE", r"C:\poison")
            .env("APPDATA", r"C:\poison")
            .env("LOCALAPPDATA", r"C:\poison")
            .output()
            .unwrap();
        let printed = String::from_utf8_lossy(&out.stdout);
        assert!(printed.contains(&answer), "{printed}");
    }

    /// #3702 — the service account's own profile is where the system keeps
    /// it, and a person's is in the registry's list too.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_service_accounts_profile_is_the_one_the_system_names() {
        let profile = system_profile();
        assert!(
            in_windows_folder(&profile, &system_directory()),
            "{}",
            profile.display()
        );
        let vars = system_folder_variables();
        let named = |n: &str| vars.iter().find(|(name, _)| *name == n).map(|(_, v)| v);
        assert_eq!(named("USERPROFILE"), Some(&profile));
        assert_eq!(
            named("LOCALAPPDATA"),
            Some(&profile.join("AppData").join("Local"))
        );
        assert_eq!(profile_of_account(r"S-1-5-18\..\x"), None);
        assert_eq!(profile_of_account("S-1-5-0-not-there"), None);
    }

    #[test]
    fn only_the_system_folders_are_filled_in_a_profile_path() {
        let (windows, drive) = (r"D:\Windows", "D:");
        for (stored, want) in [
            (
                r"%SystemRoot%\System32\config\systemprofile",
                Some(r"D:\Windows\System32\config\systemprofile"),
            ),
            (r"%systemdrive%\Users\u", Some(r"D:\Users\u")),
            (r"C:\Users\u", Some(r"C:\Users\u")),
            (r"%USERPROFILE%\x", None),
            (r"C:\Users\%x", None),
        ] {
            assert_eq!(
                fill_in_system_folders(stored, windows, drive).as_deref(),
                want,
                "{stored}"
            );
        }
    }

    /// #3702 — a folder and what is below it, by name and in any case on
    /// Windows; a link on the way down is found.
    #[test]
    fn a_path_is_in_a_folder_by_its_parts() {
        let folder = Path::new(r"C:\Windows\System32\config\systemprofile");
        assert!(in_windows_folder(
            Path::new(r"c:/windows/SYSTEM32/config/systemprofile/AppData"),
            folder
        ));
        assert!(in_windows_folder(folder, folder));
        assert!(!in_windows_folder(
            Path::new(r"C:\Windows\System32\config\systemprofile2"),
            folder
        ));
        assert!(!in_windows_folder(
            Path::new(r"C:\Windows\System32\config\systemprofile\..\x"),
            folder
        ));
        assert!(!in_windows_folder(Path::new(r"C:\Users\u"), folder));
        assert!(!in_windows_folder(folder, Path::new("")));

        let root = tempfile::tempdir().unwrap();
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("below")).unwrap();
        let real = root.path().join("real");
        std::fs::create_dir_all(real.join("a")).unwrap();
        assert_eq!(link_below(root.path(), &real.join("a").join("b")), None);
        folder_link(&real.join("link"), &elsewhere);
        let through = real.join("link").join("below");
        assert_eq!(link_below(root.path(), &through), Some(real.join("link")));
        let outside = Path::new(if cfg!(windows) { r"C:\x" } else { "/x" });
        assert_eq!(
            link_below(root.path(), outside),
            Some(outside.to_path_buf())
        );
    }

    /// #3702 — the one-time folder move runs only for the account that owns
    /// the folder: never in an installer step, and only over folders in the
    /// profile of the account the process runs as.
    #[test]
    fn the_folder_move_runs_only_for_the_account_that_owns_the_folder() {
        let (own, other) = if cfg!(windows) {
            (
                Path::new(r"C:\Windows\System32\config\systemprofile"),
                Path::new(r"C:\Users\u"),
            )
        } else {
            (Path::new("/root"), Path::new("/home/u"))
        };
        let config = |profile: &Path| profile.join("AppData").join(ORG).join(OLD_APP);
        let own_folders = [config(own), own.join("Local").join(ORG).join(OLD_APP)];
        assert!(may_migrate(false, Some(own), &own_folders));
        assert!(
            !may_migrate(true, Some(own), &own_folders),
            "an installer step"
        );
        assert!(
            !may_migrate(false, Some(own), &[config(own), config(other)]),
            "a folder in another account's profile"
        );
        assert!(
            !may_migrate(false, None, &own_folders),
            "no profile the system names"
        );
        let sibling = PathBuf::from(format!("{}2", own.display())).join(ORG);
        assert!(
            !may_migrate(false, Some(own), &[sibling]),
            "a folder whose path only starts with the same letters"
        );
        #[cfg(windows)]
        assert!(
            may_migrate(
                false,
                Some(own),
                &[PathBuf::from(
                    r"C:\WINDOWS\system32\CONFIG\systemprofile\AppData\roomler"
                )]
            ),
            "Windows names a folder in any case"
        );
    }

    /// A folder link at `link` that leads to `target`: a junction on Windows,
    /// which needs no privilege, a symbolic link elsewhere.
    fn folder_link(link: &Path, target: &Path) {
        #[cfg(windows)]
        {
            let status = std::process::Command::new(system32_tool("cmd.exe"))
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "mklink /J {}", link.display());
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    /// #3702 — the move follows no link below the profile: a pair reached
    /// through one is left where it is, and so is what the link leads to.
    #[test]
    fn the_folder_move_follows_no_link_below_the_profile() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join(OLD_APP)).unwrap();
        std::fs::write(elsewhere.join(OLD_APP).join("config.toml"), "x").unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        let link = profile.join("AppData");
        folder_link(&link, &elsewhere);
        let notes = migrate_dirs_within(
            &profile,
            &[(link.join(OLD_APP), link.join(NEW_APP))],
            DataMerge::Runs,
            rename_on_disk,
        );
        assert!(
            elsewhere.join(OLD_APP).join("config.toml").is_file(),
            "{notes:?}"
        );
        assert!(!elsewhere.join(NEW_APP).exists(), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("link")), "{notes:?}");
        // The same pair with no link moves.
        let real = root.path().join("real");
        std::fs::create_dir_all(real.join(OLD_APP)).unwrap();
        let notes = migrate_dirs_within(
            &real,
            &[(real.join(OLD_APP), real.join(NEW_APP))],
            DataMerge::Runs,
            rename_on_disk,
        );
        assert!(real.join(NEW_APP).is_dir(), "{notes:?}");
    }

    /// The pair `<home>/roomler-agent` -> `<home>/roomler`.
    fn pair_in(home: &Path) -> (PathBuf, PathBuf) {
        (home.join(OLD_APP), home.join(NEW_APP))
    }

    /// Whether either folder the segment choice ([`segment_of`]) keys on
    /// exists in the tree whose config folder is `config` and whose local
    /// data folder is `data`.
    fn tree_present(config: &Path, data: &Path) -> bool {
        config.exists() || data.exists()
    }

    /// #3702 — the one-time move decides for every pair before any pair
    /// moves: a link on the way of the config pair (a junction on Windows, a
    /// symbolic link elsewhere) leaves the data pair where it is too, with one
    /// note that names the link. No NEW folder exists afterwards, so the
    /// segment choice keeps the OLD tree, and the config is where it was.
    #[test]
    fn a_link_on_one_pairs_way_leaves_every_pair_in_place() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join(OLD_APP)).unwrap();
        std::fs::write(elsewhere.join(OLD_APP).join("config.toml"), "x").unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        let config_home = profile.join("config");
        folder_link(&config_home, &elsewhere);
        let data_home = profile.join("data");
        std::fs::create_dir_all(data_home.join(OLD_APP)).unwrap();
        std::fs::write(data_home.join(OLD_APP).join("logs.txt"), "x").unwrap();
        let (config, data) = (pair_in(&config_home), pair_in(&data_home));

        let notes = migrate_dirs_within(
            &profile,
            &[config.clone(), data.clone()],
            DataMerge::Runs,
            rename_on_disk,
        );

        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains(&config_home.display().to_string()) && notes[0].contains("link"),
            "{notes:?}"
        );
        assert!(data.0.join("logs.txt").is_file(), "{notes:?}");
        assert!(!tree_present(&config.1, &data.1), "{notes:?}");
        assert!(tree_present(&config.0, &data.0), "{notes:?}");
        assert!(config.0.join("config.toml").is_file(), "{notes:?}");

        // The same pairs with no link on their way both move.
        let plain = root.path().join("plain");
        let pairs = [pair_in(&plain.join("config")), pair_in(&plain.join("data"))];
        for (old, _) in &pairs {
            std::fs::create_dir_all(old).unwrap();
        }
        let notes = migrate_dirs_within(&plain, &pairs, DataMerge::Runs, rename_on_disk);
        assert!(
            pairs.iter().all(|(old, new)| !old.exists() && new.is_dir()),
            "{notes:?}"
        );
    }

    /// #3702 — a pair whose move fails leaves the pairs after it where they
    /// are. The config pair moves first, so no NEW folder the segment choice
    /// keys on exists while the config is still in the OLD tree.
    #[test]
    fn a_failed_move_leaves_the_pairs_after_it_in_place() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path();
        let (config, data) = (
            pair_in(&profile.join("config")),
            pair_in(&profile.join("data")),
        );
        for (old, _) in [&config, &data] {
            std::fs::create_dir_all(old).unwrap();
        }
        std::fs::write(config.0.join("config.toml"), "x").unwrap();
        let mut tried = Vec::new();
        let notes = migrate_dirs_within(
            profile,
            &[config.clone(), data.clone()],
            DataMerge::Runs,
            |from, to| {
                tried.push(from.to_path_buf());
                if from == config.0 {
                    return Err(std::io::Error::other("another volume"));
                }
                std::fs::rename(from, to)
            },
        );
        assert_eq!(tried, std::slice::from_ref(&config.0), "{notes:?}");
        assert!(!tree_present(&config.1, &data.1), "{notes:?}");
        assert!(config.0.join("config.toml").is_file(), "{notes:?}");
        assert!(
            notes
                .iter()
                .any(|n| n.contains(&data.0.display().to_string())),
            "{notes:?}"
        );
    }

    /// #3702 — the config folder moves by one rename or not at all. A rename
    /// of it that the system refuses (an indexer's handle on the folder)
    /// leaves no NEW config folder and moves no pair after it: the segment
    /// choice keeps the OLD tree, which holds the config and everything else
    /// the folder held.
    #[test]
    fn the_config_folder_moves_by_one_rename_or_not_at_all() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path();
        let (config, data) = (
            pair_in(&profile.join("config")),
            pair_in(&profile.join("data")),
        );
        for (old, _) in [&config, &data] {
            std::fs::create_dir_all(old).unwrap();
            std::fs::write(old.join("a.log"), "x").unwrap();
        }
        std::fs::write(config.0.join("config.toml"), "x").unwrap();

        let notes = migrate_dirs_within(
            profile,
            &[config.clone(), data.clone()],
            DataMerge::Runs,
            |from, to| {
                if from == config.0 {
                    return Err(std::io::ErrorKind::PermissionDenied.into());
                }
                std::fs::rename(from, to)
            },
        );

        assert!(!tree_present(&config.1, &data.1), "{notes:?}");
        assert!(tree_present(&config.0, &data.0), "{notes:?}");
        for file in [
            config.0.join("config.toml"),
            config.0.join("a.log"),
            data.0.join("a.log"),
        ] {
            assert!(file.is_file(), "{}: {notes:?}", file.display());
        }
    }

    #[test]
    fn only_the_system_drive_variable_is_filled_in() {
        for (stored, want) in [
            (r"%SystemDrive%\ProgramData", r"D:\ProgramData"),
            (r"%SYSTEMDRIVE%\ProgramData", r"D:\ProgramData"),
            (r"E:\ProgramData", r"E:\ProgramData"),
            (r"%Other%\ProgramData", r"%Other%\ProgramData"),
        ] {
            assert_eq!(expand_system_drive(stored, "D:"), want, "{stored}");
        }
    }

    /// FR-92 — the profile-relative form must name the SAME directory the
    /// person's own `project_dirs()` does, or a SystemContext worker and a
    /// user-context worker would keep two different keep-busy stores.
    #[cfg(target_os = "windows")]
    #[test]
    fn data_local_dir_in_profile_matches_the_owners_project_dirs() {
        let Some(profile) = std::env::var_os("USERPROFILE") else {
            return;
        };
        let Some(dirs) = project_dirs() else {
            return;
        };
        // Only meaningful when the Local and Roaming AppData folders sit in
        // their default places, where the profile-relative form looks.
        let app_data = Path::new(&profile).join("AppData");
        if !dirs.data_local_dir().starts_with(app_data.join("Local"))
            || !dirs.config_dir().starts_with(app_data.join("Roaming"))
        {
            return;
        }
        assert_eq!(
            data_local_dir_in_profile(Path::new(&profile)),
            dirs.data_local_dir()
        );
    }

    // The order the Linux root config ladder in `config::default_config_path`
    // takes: the first path if it exists, else the second if it exists, else
    // the first. The predicate is injected because the real `/etc` depends on
    // the box and on root. The tree choice has its own tests over
    // [`segment_of`] above.
    fn pick<'a>(first: &'a str, second: &'a str, exists: impl Fn(&str) -> bool) -> &'a str {
        if exists(first) {
            first
        } else if exists(second) {
            second
        } else {
            first
        }
    }

    /// The invariant that retires the crash-loop class: for a Linux root
    /// process, `/etc/roomler/config.toml` and the profile config resolve in a
    /// fixed order, so the packaged unit's `--config` and a bare `roomlerd run`
    /// can never disagree about where this host's identity lives.
    ///
    /// The predicate is injected (`pick` above): asserting on the real `/etc`
    /// would depend on the box and on root.
    #[test]
    fn linux_system_config_precedence_is_etc_then_profile_then_etc() {
        // `/etc` present -> `/etc`, even when the legacy profile copy survives
        // (the migration never deletes, so both CAN exist).
        assert_eq!(
            pick("/etc", "profile", |s| s == "/etc" || s == "profile"),
            "/etc"
        );
        // Only the profile copy -> profile. A host whose migration has not run
        // yet, or could not, keeps its identity instead of losing it.
        assert_eq!(pick("/etc", "profile", |s| s == "profile"), "profile");
        // Neither -> `/etc`: a fresh system install, where `enroll` writes to
        // the path the unit already passes.
        assert_eq!(pick("/etc", "profile", |_| false), "/etc");
    }

    /// The rollback copy carries the agent token and the WG secret key, so it
    /// has to travel with the config — leaving it behind strands a second,
    /// credential-bearing copy at a path something may later be pointed at
    /// (exactly how a cleared SSH key came back on fleet-host-1).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_prev_sibling_is_the_config_path_plus_prev() {
        assert_eq!(
            with_prev(Path::new("/etc/roomler/config.toml")),
            PathBuf::from("/etc/roomler/config.toml.prev")
        );
    }

    /// #3702 — root's profile config moves into `/etc/roomler` only from
    /// the home the system names for root, and by no link below it.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_system_config_moves_only_from_the_accounts_own_home() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let config = |home: &Path| home.join(".config").join(ORG).join("config.toml");
        std::fs::create_dir_all(config(&home).parent().unwrap()).unwrap();
        std::fs::write(config(&home), "x").unwrap();
        assert_eq!(
            system_config_home(Some(&home), &config(&home)),
            Ok(home.as_path())
        );
        let other = root.path().join("other");
        let note = system_config_home(Some(&other), &config(&home)).unwrap_err();
        assert!(note.contains("not in this account's own home"), "{note}");
        let note = system_config_home(None, &config(&home)).unwrap_err();
        assert!(note.contains("names no home"), "{note}");
        // The same file reached through a link below the home stays.
        let linked = root.path().join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        folder_link(&linked.join(".config"), &home.join(".config"));
        let note = system_config_home(Some(&linked), &config(&linked)).unwrap_err();
        assert!(note.contains("is a link"), "{note}");
    }

    #[test]
    fn migrate_dir_moves_legacy_into_place() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("roomler-agent").join("config");
        let new = tmp.path().join("roomler").join("config");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("config.toml"), b"agent_token = \"x\"").unwrap();

        let note = migrate_dir(&old, &new, Some(DataMerge::Runs), rename_on_disk)
            .unwrap()
            .expect("a move produces a note");
        assert!(note.starts_with("migrated"), "{note}");
        assert!(!old.exists(), "legacy dir must be gone after the move");
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).unwrap(),
            "agent_token = \"x\"",
            "contents ride along byte-identically"
        );
    }

    /// #3746 — both folders present: the config pair stays as it is, and a
    /// data pair merges into the NEW folder. Before #3746 this test pinned
    /// "left in place" for every pair; it now pins both rules.
    #[test]
    fn a_split_brain_config_pair_stays_and_a_data_pair_merges() {
        let tmp = tempfile::tempdir().unwrap();
        for per_child in [false, true] {
            let old = tmp.path().join(per_child.to_string()).join("roomler-agent");
            let new = tmp.path().join(per_child.to_string()).join("roomler");
            std::fs::create_dir_all(&old).unwrap();
            std::fs::create_dir_all(&new).unwrap();
            std::fs::write(old.join("stale.txt"), b"old").unwrap();
            let merge = per_child.then_some(DataMerge::Runs);
            let note = migrate_dir(&old, &new, merge, rename_on_disk)
                .unwrap()
                .expect("both-present produces a note");
            if per_child {
                assert!(!old.exists(), "{note}");
                assert_eq!(std::fs::read(new.join("stale.txt")).unwrap(), b"old");
            } else {
                assert!(note.contains("left in place"), "{note}");
                assert!(old.join("stale.txt").exists(), "nothing may be deleted");
            }
        }
    }

    #[test]
    fn migrate_children_moves_everything_and_drops_old_root() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("roomler-agent");
        let new = tmp.path().join("roomler");
        std::fs::create_dir_all(old.join("service-logs")).unwrap();
        std::fs::write(old.join("config.toml"), b"agent_token = \"x\"").unwrap();
        std::fs::write(old.join("service-logs").join("a.log"), b"l").unwrap();

        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "root locked");
        let note =
            migrate_children(&old, &new, DataMerge::Runs, Some(&err), &mut rename_on_disk).unwrap();
        assert!(note.contains("entry by entry"), "{note}");
        assert!(!old.exists(), "emptied legacy root must be removed");
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).unwrap(),
            "agent_token = \"x\""
        );
        assert!(new.join("service-logs").join("a.log").exists());
    }

    /// #3746 — a data folder whose rename is refused while another runner
    /// fills the NEW folder merges into it entry by entry, with no roll
    /// back: a folder both hold merges, and what the other runner put there
    /// stays. Before #3746 this test pinned the per-child move's roll back.
    #[test]
    fn a_refused_data_folder_merges_into_what_another_runner_made() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("roomler-agent");
        let new = tmp.path().join("roomler");
        std::fs::create_dir_all(old.join("crashes")).unwrap();
        std::fs::write(old.join("crashes").join("c1.dmp"), b"dump").unwrap();
        std::fs::write(old.join("config.toml"), b"tok").unwrap();

        let note = migrate_dir(
            &old,
            &new,
            Some(DataMerge::Runs),
            |from: &Path, to: &Path| {
                if from == old {
                    // The other runner, first: a `crashes` folder with something in it.
                    std::fs::create_dir_all(new.join("crashes").join("blocker")).unwrap();
                    return Err(std::io::ErrorKind::PermissionDenied.into());
                }
                rename_on_disk(from, to)
            },
        )
        .unwrap()
        .expect("a merge produces a note");

        assert!(!old.exists(), "{note}");
        assert_eq!(std::fs::read(new.join("config.toml")).unwrap(), b"tok");
        let dump = new.join("crashes").join("c1.dmp");
        assert_eq!(std::fs::read(dump).unwrap(), b"dump", "{note}");
        assert!(new.join("crashes").join("blocker").is_dir(), "{note}");
    }

    /// #3746 — a file held open with no sharing, which cannot be renamed on
    /// Windows, stays in the OLD folder while every other entry moves, and
    /// the next start moves it. Before #3746 this test pinned the per-child
    /// move's roll back, which left the whole folder in the OLD tree.
    #[cfg(windows)]
    #[test]
    fn a_held_file_stays_in_the_old_folder_and_moves_at_the_next_start() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join(OLD_APP);
        let new = tmp.path().join(NEW_APP);
        std::fs::create_dir_all(&old).unwrap();
        for name in ["a.log", "b.log", "config.toml"] {
            std::fs::write(old.join(name), name).unwrap();
        }
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(old.join("b.log"))
            .unwrap();
        // An indexer's handle on the folder refuses the folder's own rename.
        let refused = |from: &Path, to: &Path| {
            if from == old {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            rename_on_disk(from, to)
        };

        let note = migrate_dir(&old, &new, Some(DataMerge::Runs), refused).unwrap_err();
        assert!(note.contains("b.log"), "{note}");
        for name in ["a.log", "config.toml"] {
            assert!(new.join(name).is_file(), "{name}: {note}");
        }
        assert!(old.join("b.log").is_file(), "{note}");

        drop(held);
        let note = migrate_dir(&old, &new, Some(DataMerge::Runs), rename_on_disk)
            .unwrap()
            .expect("a merge produces a note");
        assert!(!old.exists(), "{note}");
        for name in ["a.log", "b.log", "config.toml"] {
            assert_eq!(std::fs::read_to_string(new.join(name)).unwrap(), name);
        }
    }

    #[test]
    fn migrate_dir_is_quiet_without_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            migrate_dir(
                &tmp.path().join("roomler-agent"),
                &tmp.path().join("roomler"),
                Some(DataMerge::Runs),
                rename_on_disk
            ),
            Ok(None)
        );
    }

    // ── FR-21: the machine-global segment decision ──────────────────────
    //
    // Covers the acceptance criterion that had no host to run on: "the §5
    // staging path resolves correctly on BOTH a fresh perMachine host and a
    // pre-rename host". `files.rs` derives the staging dir as
    // `machine_global_dir().join("staging")` and `machine_global_config_path()`
    // derives config.toml the same way, so this decision is what it turns on.

    #[cfg(target_os = "windows")]
    #[test]
    fn machine_global_keeps_a_pre_rename_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(ORG);
        std::fs::create_dir_all(base.join(OLD_APP)).unwrap();
        // Legacy tree present, new one absent: an upgraded host. Resolving to
        // the new segment here orphans its enrolled config — the device drops
        // off the mesh and re-enrols as a stranger.
        assert_eq!(resolve_machine_global(&base), base.join(OLD_APP));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn machine_global_uses_the_new_tree_on_a_fresh_host() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(ORG);
        std::fs::create_dir_all(base.join(NEW_APP)).unwrap();
        assert_eq!(resolve_machine_global(&base), base.join(NEW_APP));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn machine_global_prefers_new_when_both_trees_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(ORG);
        std::fs::create_dir_all(base.join(NEW_APP)).unwrap();
        std::fs::create_dir_all(base.join(OLD_APP)).unwrap();
        // A migrated host keeps BOTH for a while. New wins, or a host would
        // fall back to a tree it has already moved off.
        assert_eq!(resolve_machine_global(&base), base.join(NEW_APP));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn machine_global_defaults_to_new_when_nothing_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(ORG);
        // Nothing on disk yet: a first install must create the NEW tree,
        // never resurrect the retired name.
        assert_eq!(resolve_machine_global(&base), base.join(NEW_APP));
    }

    /// #3802 — the macOS rule reads no `HOME`: root's folders hang off
    /// `/var/root`, the home the account database names for it, a person's
    /// off theirs, and a person's is the folder a root process reaches their
    /// files through ([`macos_config_dir_in_home`]).
    #[test]
    fn a_macs_folders_hang_off_the_home_the_account_database_names() {
        let root = macos_dirs(NEW_APP, || Some(PathBuf::from("/var/root"))).unwrap();
        let want = Path::new("/var/root/Library/Application Support/live.roomler.roomler");
        assert_eq!(root.config_dir(), want);
        assert_eq!(root.data_dir(), want);
        assert_eq!(root.data_local_dir(), want);
        let person = Path::new("/Users/p");
        let legacy = macos_dirs(OLD_APP, || Some(person.to_path_buf())).unwrap();
        assert_eq!(
            legacy.config_dir(),
            Path::new("/Users/p/Library/Application Support/live.roomler.roomler-agent")
        );
        assert_eq!(
            macos_dirs(NEW_APP, || Some(person.to_path_buf()))
                .unwrap()
                .config_dir(),
            macos_config_dir_in_home(person, |_| false)
        );
        assert_eq!(macos_dirs(NEW_APP, || None), None);
    }

    // ── #3866: a Linux process's own folders ──────────────────────────────

    /// `pairs` as a process's environment, the stand-in [`linux_folders`]
    /// reads.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into())
        }
    }

    /// The XDG defaults beneath `home`, with no runtime folder.
    fn defaults_beneath(home: impl Into<PathBuf>) -> OwnFolders {
        let home = home.into();
        OwnFolders {
            config: home.join(".config"),
            data: home.join(".local/share"),
            cache: home.join(".cache"),
            state: home.join(".local/state"),
            runtime: None,
            home,
        }
    }

    /// The account database's home, as a stand-in that names `home`.
    fn database(home: &str) -> impl FnOnce() -> Option<PathBuf> {
        let home = PathBuf::from(home);
        move || Some(home)
    }

    /// An account database that must not be asked.
    fn not_asked() -> Option<PathBuf> {
        panic!("the account database was asked")
    }

    /// The folder the single-instance lock is taken in: the runtime folder,
    /// else the cache folder.
    fn lock_folder(own: &OwnFolders) -> &Path {
        own.runtime.as_deref().unwrap_or(own.cache.as_path())
    }

    /// A `HOME` and XDG folders of another account.
    const ANOTHER_ACCOUNTS: &[(&str, &str)] = &[
        ("HOME", "/home/p"),
        ("XDG_CONFIG_HOME", "/home/p/.config"),
        ("XDG_DATA_HOME", "/home/p/.local/share"),
        ("XDG_CACHE_HOME", "/home/p/.cache"),
        ("XDG_STATE_HOME", "/home/p/.local/state"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
    ];

    /// #3866 AC1 — a root command uses root's own folders, whatever `HOME`
    /// and `XDG_*` it was given: each folder is the XDG default beneath
    /// `/root`, it has no runtime folder, so its lock is in root's own cache
    /// folder, and its Videos folder is the one root's own `user-dirs.dirs`
    /// names. It asks the account database nothing.
    #[test]
    fn a_root_command_uses_roots_own_folders_whatever_home_it_was_given() {
        let own = linux_folders(0, env_of(ANOTHER_ACCOUNTS), not_asked).unwrap();
        assert_eq!(own, defaults_beneath("/root"));
        assert_eq!(lock_folder(&own), Path::new("/root/.cache"));
        for app in [NEW_APP, OLD_APP] {
            let dirs = linux_dirs(app, Some(own.clone())).unwrap();
            assert_eq!(dirs.config_dir(), Path::new("/root/.config").join(app));
            assert_eq!(dirs.data_dir(), Path::new("/root/.local/share").join(app));
            assert_eq!(dirs.data_local_dir(), dirs.data_dir());
        }
        let videos = videos_dir(&own, |file| {
            assert_eq!(file, Path::new("/root/.config/user-dirs.dirs"));
            Some(b"XDG_VIDEOS_DIR=\"$HOME/Videos\"\n".to_vec())
        });
        assert_eq!(videos.as_deref(), Some(Path::new("/root/Videos")));
        // Values that are empty or relative change nothing either.
        let other: &[(&str, &str)] = &[
            ("HOME", ""),
            ("XDG_CONFIG_HOME", ""),
            ("XDG_DATA_HOME", "share"),
            ("XDG_RUNTIME_DIR", "run"),
        ];
        assert_eq!(
            linux_folders(0, env_of(other), not_asked),
            Some(defaults_beneath("/root"))
        );
    }

    /// #3866 — root's folders are beneath `/root` whatever the account
    /// database names for root: `/home/root`, another home, or no entry at
    /// all (as in some container images).
    #[test]
    fn root_uses_slash_root_whatever_the_account_database_names() {
        let none: &[(&str, &str)] = &[];
        for given in [ANOTHER_ACCOUNTS, none] {
            for names in [Some("/home/root"), Some("/srv/admin"), None] {
                assert_eq!(
                    linux_folders(0, env_of(given), move || names.map(PathBuf::from)),
                    Some(defaults_beneath("/root")),
                    "{given:?} {names:?}"
                );
            }
        }
    }

    /// #3866 AC2 — a daemon that systemd started keeps every folder. The
    /// environment systemd gives the shipped units: `roomlerd.service` runs
    /// with `User=root`, so `HOME=/root`, `USER`, `LOGNAME` and `SHELL`, and
    /// sets no `XDG_*`; `roomler-update.service` has no `User=`, so no
    /// `HOME`. Root's folders are the ones `directories` gave that
    /// environment (for the update helper, on a host whose account database
    /// names `/root`): the XDG defaults beneath `/root`, with no runtime
    /// folder, so the lock stays `/root/.cache/roomler-agent-<id>.lock`. A
    /// runtime folder in the environment, `/run/user/0`, does not move it
    /// either, and nothing here looks at whether a folder exists.
    #[test]
    fn a_daemon_systemd_started_keeps_every_folder() {
        const PATH: (&str, &str) = (
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
        let daemon: &[(&str, &str)] = &[
            PATH,
            ("HOME", "/root"),
            ("USER", "root"),
            ("LOGNAME", "root"),
            ("SHELL", "/bin/sh"),
            ("ROOMLERD_CONFIG", "/etc/roomler/config.toml"),
            ("RUST_LOG", "roomlerd=info,warn"),
        ];
        let update_helper: &[(&str, &str)] = &[PATH, ("RUST_LOG", "info")];
        let with_runtime: Vec<(&str, &str)> = daemon
            .iter()
            .copied()
            .chain([("XDG_RUNTIME_DIR", "/run/user/0")])
            .collect();
        for unit in [daemon, with_runtime.as_slice(), update_helper] {
            let own = linux_folders(0, env_of(unit), not_asked).unwrap();
            assert_eq!(own.home, Path::new("/root"), "{unit:?}");
            assert_eq!(own.config, Path::new("/root/.config"), "{unit:?}");
            assert_eq!(own.data, Path::new("/root/.local/share"), "{unit:?}");
            assert_eq!(own.cache, Path::new("/root/.cache"), "{unit:?}");
            assert_eq!(own.state, Path::new("/root/.local/state"), "{unit:?}");
            assert_eq!(own.runtime, None, "{unit:?}");
            assert_eq!(lock_folder(&own), Path::new("/root/.cache"), "{unit:?}");
            let dirs = linux_dirs(NEW_APP, Some(own)).unwrap();
            assert_eq!(dirs.config_dir(), Path::new("/root/.config/roomler"));
            assert_eq!(dirs.data_dir(), Path::new("/root/.local/share/roomler"));
        }
    }

    /// #3866 AC3 — a person's command honours `HOME` and the XDG variables
    /// as `directories` does: each `XDG_*` folder that is absolute as given,
    /// an empty or relative one the default beneath `HOME`, an empty or
    /// missing `HOME` the account database's home, and no folders with
    /// neither. The account database is not read while `HOME` names a home.
    #[test]
    fn a_persons_command_honours_xdg() {
        let person: &[(&str, &str)] = &[
            ("HOME", "/home/p"),
            ("XDG_CONFIG_HOME", "/data/p/config"),
            ("XDG_DATA_HOME", "/data/p/share"),
            ("XDG_CACHE_HOME", "/data/p/cache"),
            ("XDG_STATE_HOME", "/data/p/state"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ];
        let own = linux_folders(1000, env_of(person), not_asked).unwrap();
        assert_eq!(
            own,
            OwnFolders {
                home: PathBuf::from("/home/p"),
                config: PathBuf::from("/data/p/config"),
                data: PathBuf::from("/data/p/share"),
                cache: PathBuf::from("/data/p/cache"),
                state: PathBuf::from("/data/p/state"),
                runtime: Some(PathBuf::from("/run/user/1000")),
            }
        );
        assert_eq!(lock_folder(&own), Path::new("/run/user/1000"));
        let videos = videos_dir(&own, |file| {
            assert_eq!(file, Path::new("/data/p/config/user-dirs.dirs"));
            Some(b"XDG_VIDEOS_DIR=\"$HOME/Videos\"\n".to_vec())
        });
        assert_eq!(videos.as_deref(), Some(Path::new("/home/p/Videos")));
        let relative: &[(&str, &str)] = &[
            ("HOME", "/home/p"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", ""),
            ("XDG_RUNTIME_DIR", "run"),
        ];
        assert_eq!(
            linux_folders(1000, env_of(relative), || None),
            Some(defaults_beneath("/home/p"))
        );
        let empty: &[(&str, &str)] = &[("HOME", "")];
        let none: &[(&str, &str)] = &[];
        for no_home in [empty, none] {
            assert_eq!(
                linux_folders(1000, env_of(no_home), database("/home/q")),
                Some(defaults_beneath("/home/q")),
                "{no_home:?}"
            );
            assert_eq!(linux_folders(1000, env_of(no_home), || None), None);
        }
    }

    /// #3866 — the Videos folder is read from `user-dirs.dirs` as
    /// `directories`' `UserDirs` reads it.
    #[test]
    fn the_videos_folder_is_read_as_user_dirs_reads_it() {
        let own = defaults_beneath("/home/p");
        let named = |text: &str| {
            videos_dir(&own, |file| {
                assert_eq!(file, Path::new("/home/p/.config/user-dirs.dirs"));
                Some(text.as_bytes().to_vec())
            })
        };
        for (text, want) in [
            ("XDG_VIDEOS_DIR=\"$HOME/Videos\"\n", "/home/p/Videos"),
            ("XDG_VIDEOS_DIR=\"/srv/videos\"", "/srv/videos"),
            (
                " \tXDG_VIDEOS_DIR = \"$HOME/My\\ Videos\"\t ",
                "/home/p/My Videos",
            ),
            (
                "XDG_VIDEOS_DIR=\"$HOME/a\"\nXDG_VIDEOS_DIR=\"$HOME/b\"\nXDG_VIDEOS_DIR=\"$HOME/\"\n",
                "/home/p/b",
            ),
            (
                "# Written by xdg-user-dirs-update\nXDG_MUSIC_DIR=\"$HOME/Music\"\nXDG_VIDEOS_DIR=\"$HOME/Clips\"\n",
                "/home/p/Clips",
            ),
        ] {
            assert_eq!(named(text).as_deref(), Some(Path::new(want)), "{text:?}");
        }
        for text in [
            "",
            "# XDG_VIDEOS_DIR=\"$HOME/Videos\"",
            "XDG_VIDEOS_DIR=$HOME/Videos",
            "XDG_VIDEOS_DIR=\"Videos\"",
            "XDG_VIDEOS_DIR=\"$HOME/\"",
            "XDG_VIDEOS_DIR=\"$HOME\"",
            "XDG_VIDEOS_DIR=\"\"",
            "XDG_VIDEOS_DIR=\"",
            "XDG_DIR=\"/srv\"",
            "XDG_MUSIC_DIR=\"$HOME/Music\"",
            "XDG_VIDEOS_DIR=\"$HOME/Videos\"\r",
        ] {
            assert_eq!(named(text), None, "{text:?}");
        }
        assert_eq!(videos_dir(&own, |_| None), None);
    }

    /// #3866 — this process's own folders, from its real environment, which
    /// is read and never changed: where it is not root, exactly what
    /// `directories` resolves (its `BaseDirs` and its `UserDirs`' Videos
    /// folder); where it is root, the XDG defaults beneath `/root`.
    #[cfg(target_os = "linux")]
    #[test]
    fn this_processs_folders_are_what_directories_gives_unless_it_is_root() {
        let own = own_folders();
        // SAFETY: a read of this process's own credentials.
        if unsafe { libc::geteuid() } == 0 {
            assert_eq!(own, Some(defaults_beneath(ROOT_HOME)));
            return;
        }
        let base = directories::BaseDirs::new();
        assert_eq!(own.is_some(), base.is_some());
        let (Some(own), Some(base)) = (own, base) else {
            return;
        };
        assert_eq!(own.home, base.home_dir());
        assert_eq!(own.config, base.config_dir());
        assert_eq!(own.data, base.data_dir());
        assert_eq!(own.data, base.data_local_dir());
        assert_eq!(own.cache, base.cache_dir());
        assert_eq!(Some(own.state.as_path()), base.state_dir());
        assert_eq!(own.runtime.as_deref(), base.runtime_dir());
        let theirs =
            directories::UserDirs::new().and_then(|u| u.video_dir().map(Path::to_path_buf));
        assert_eq!(videos_dir(&own, |file| std::fs::read(file).ok()), theirs);
    }

    /// The home this process's folders hang off where its account decides
    /// them, not `HOME`: on macOS the one the account database names, and on
    /// Linux `/root` for root (#3866); `None` where `ProjectDirs` decides.
    fn account_home_decides() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            Some(own_home().expect("the account database names this account's home"))
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: a read of this process's own credentials.
            let root = unsafe { libc::geteuid() } == 0;
            root.then(|| PathBuf::from(ROOT_HOME))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            None
        }
    }

    /// Set in the child [`a_child_given_another_home_keeps_its_accounts_folders`]
    /// starts: the child prints what it resolves and writes nothing.
    const CHILD_3802: &str = "ROOMLER_TEST_3802_CHILD";

    /// Runs this test binary again as a child with `home` as its `HOME`
    /// (`None`: no `HOME` at all, as launchd starts the root daemon) and
    /// returns each folder the child resolved: its name, the agent's folder
    /// ([`project_dirs`]) and `ProjectDirs`' own answer in that same child.
    fn folders_of_a_child(home: Option<&Path>) -> Vec<(String, PathBuf, PathBuf)> {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "appdirs::tests::a_child_given_another_home_keeps_its_accounts_folders",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_3802, "1");
        match home {
            Some(home) => child.env("HOME", home),
            None => child.env_remove("HOME"),
        };
        // The child's folders hang off the home it is given, not off XDG
        // folders of whoever runs this.
        for name in [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
        ] {
            child.env_remove(name);
        }
        let out = child.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "the child failed: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rows: Vec<_> = stdout
            .lines()
            .filter_map(|line| {
                // libtest prints the test's name on the same line as its first row.
                let mut cells = line.split_once("3802|")?.1.split('|');
                Some((
                    cells.next()?.to_string(),
                    PathBuf::from(cells.next()?),
                    PathBuf::from(cells.next()?),
                ))
            })
            .collect();
        assert_eq!(rows.len(), 3, "the child printed: {stdout}");
        rows
    }

    /// #3802 — a roomlerd process given another account's `HOME` (macOS sudo
    /// keeps the caller's) takes its folders from the account it runs as: on
    /// macOS from the home the account database names for it, while
    /// `ProjectDirs` in that same child follows the `HOME` it was given, which
    /// shows the child really had another one. With its own `HOME`, or none,
    /// a child resolves exactly what `ProjectDirs` resolves, so nothing an
    /// installed Mac reads moves. #3866: a root child on Linux takes its
    /// folders from its own account the same way. Everywhere else every
    /// answer is `ProjectDirs`'. The child only resolves paths: the `HOME` it
    /// is given stays empty, and nothing is made in the home of whoever runs
    /// this.
    #[test]
    fn a_child_given_another_home_keeps_its_accounts_folders() {
        if std::env::var_os(CHILD_3802).is_some() {
            let ours = project_dirs().expect("the agent's folders");
            let theirs = ProjectDirs::from(QUALIFIER, ORG, app_segment()).expect("ProjectDirs");
            for (name, ours, theirs) in [
                ("config", ours.config_dir(), theirs.config_dir()),
                ("data", ours.data_dir(), theirs.data_dir()),
                ("data_local", ours.data_local_dir(), theirs.data_local_dir()),
            ] {
                println!("3802|{name}|{}|{}", ours.display(), theirs.display());
            }
            return;
        }
        let given = tempfile::tempdir().unwrap();
        let another = folders_of_a_child(Some(given.path()));
        assert!(
            std::fs::read_dir(given.path()).unwrap().next().is_none(),
            "the child wrote into the HOME it was given"
        );
        for (name, ours, theirs) in &another {
            match account_home_decides() {
                Some(account) => {
                    assert!(
                        theirs.starts_with(given.path()),
                        "{name}: ProjectDirs did not follow the given HOME: {theirs:?}"
                    );
                    assert!(
                        ours.starts_with(&account) && !ours.starts_with(given.path()),
                        "{name}: {ours:?} is not in {account:?}, the home of the account the child runs as"
                    );
                }
                None => assert_eq!(ours, theirs, "{name}: the folders moved off ProjectDirs"),
            }
        }
        #[cfg(unix)]
        let own = own_home();
        #[cfg(not(unix))]
        let own: Option<PathBuf> = None;
        for home in [own.as_deref(), None] {
            for (name, ours, theirs) in folders_of_a_child(home) {
                assert_eq!(ours, theirs, "{name} with HOME {home:?}: a folder moved");
            }
        }
    }

    /// FR-147 — `account_entry` reads the entry `account_home` reads: the
    /// same home where that one names one, and nothing for a uid the database
    /// does not have. (A runner whose uid has no entry has neither.)
    #[cfg(unix)]
    #[test]
    fn account_entry_names_the_home_account_home_names() {
        // SAFETY: a read of this process's own credentials.
        let uid = unsafe { libc::getuid() };
        let entry = account_entry(uid);
        assert_eq!(
            account_home(uid),
            entry
                .as_ref()
                .map(|e| e.home.clone())
                .filter(|h| h.is_absolute())
        );
        if let Some(entry) = entry {
            assert_eq!(entry.uid, uid);
            assert!(!entry.name.is_empty());
        }
        assert_eq!(account_entry(4_000_000_123), None);
    }

    // ── #3746: a data folder's move converges ─────────────────────────────

    const DAY: std::time::Duration = std::time::Duration::from_secs(86_400);
    const HOUR: std::time::Duration = std::time::Duration::from_secs(3_600);

    /// The file at `path` (its folders made), holding `body`, last modified
    /// at `when`.
    fn write_at(path: &Path, body: &str, when: std::time::SystemTime) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    /// The file at `path`, holding `body`, last modified `age` ago.
    fn write_aged(path: &Path, body: &str, age: std::time::Duration) {
        write_at(path, body, std::time::SystemTime::now() - age);
    }

    /// Every entry below `dir` that is not a folder, by its path from `dir`
    /// (parts joined by `/`), with a file's content, or `<link>` for a link,
    /// which is never followed. In name order.
    fn files_below(dir: &Path) -> Vec<(String, String)> {
        let mut found = Vec::new();
        let mut folders = vec![(dir.to_path_buf(), String::new())];
        while let Some((at, rel)) = folders.pop() {
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let rel = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                let kind = std::fs::symlink_metadata(entry.path()).unwrap().file_type();
                if kind.is_symlink() {
                    found.push((rel, "<link>".to_string()));
                } else if kind.is_dir() {
                    folders.push((entry.path(), rel));
                } else {
                    found.push((rel, std::fs::read_to_string(entry.path()).unwrap()));
                }
            }
        }
        found.sort();
        found
    }

    /// `(path, content)` pairs, owned and in name order, as [`files_below`]
    /// lists them.
    fn listing(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut listed: Vec<(String, String)> = entries
            .iter()
            .map(|(rel, body)| (rel.to_string(), body.to_string()))
            .collect();
        listed.sort();
        listed
    }

    /// The pairs of one profile: its config pair first, then its data pair.
    fn config_and_data(profile: &Path) -> [(PathBuf, PathBuf); 2] {
        [
            pair_in(&profile.join("config")),
            pair_in(&profile.join("data")),
        ]
    }

    /// #3746 AC1 — a data folder whose move failed moves at a later start,
    /// although that start's `logging::init` has made a `logs` folder in the
    /// NEW data folder meanwhile: no OLD data folder is left, and every file
    /// it held is in the NEW one.
    #[test]
    fn a_data_folder_whose_move_failed_moves_at_a_later_start() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path();
        let pairs = config_and_data(profile);
        let (data_old, data_new) = pairs[1].clone();
        write_aged(&pairs[0].0.join("config.toml"), "token", DAY);
        let held = [
            ("host-mute.json", "muted"),
            ("privacy-curtains.json", "curtains"),
            ("keep-busy.json", "busy"),
            ("logs/update-attempt", "attempt"),
            ("logs/last-install.json", "install"),
            ("logs/roomlerd.log.2026-11-01", "an earlier day"),
        ];
        for (rel, body) in held {
            write_aged(&data_old.join(rel), body, DAY);
        }

        // The first start: the config folder moves, and a handle held on the
        // data folder refuses every rename below it.
        let notes = migrate_dirs_within(profile, &pairs, DataMerge::Runs, |from, to| {
            if from.starts_with(&data_old) {
                return Err(std::io::Error::other("held"));
            }
            rename_on_disk(from, to)
        });
        assert!(pairs[0].1.join("config.toml").is_file(), "{notes:?}");
        assert!(data_old.join("host-mute.json").is_file(), "{notes:?}");
        // That start runs on the NEW tree: `logging::init` makes its folder.
        let today = data_new.join("logs").join("roomlerd.log.2026-11-22");
        write_aged(&today, "today", std::time::Duration::ZERO);

        // A later start, with nothing held.
        let notes = migrate_dirs_within(profile, &pairs, DataMerge::Runs, rename_on_disk);

        assert!(!data_old.exists(), "{notes:?}");
        let mut want = held.to_vec();
        want.push(("logs/roomlerd.log.2026-11-22", "today"));
        assert_eq!(files_below(&data_new), listing(&want), "{notes:?}");
    }

    /// #3746 AC2 — a name both folders hold. Folders merge, entry by entry;
    /// any other entry is kept under the name by the copy modified last (a
    /// tie by the NEW folder's), and the other copy is kept beside it as
    /// `<name>.older`, or `.older-2`, … when that is taken. Nothing is
    /// overwritten or deleted, and the run's note names what it kept beside.
    #[test]
    fn a_name_both_folders_hold_is_kept_by_the_copy_modified_last() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path();
        let pairs = config_and_data(profile);
        let (old, new) = pairs[1].clone();
        // NEW's copy is the newer one; OLD's goes beside it, past a name
        // an earlier merge took.
        write_aged(&old.join("host-mute.json"), "old mute", DAY);
        write_aged(&new.join("host-mute.json"), "new mute", HOUR);
        write_aged(&new.join("host-mute.json.older"), "an earlier merge", DAY);
        // OLD's copy is the newer one: it takes the name.
        write_aged(&old.join("keep-busy.json"), "old busy", HOUR);
        write_aged(&new.join("keep-busy.json"), "new busy", DAY);
        // The same time: the NEW folder's copy keeps the name.
        let then = std::time::SystemTime::now() - DAY;
        write_at(&old.join("privacy-curtains.json"), "old curtains", then);
        write_at(&new.join("privacy-curtains.json"), "new curtains", then);
        // A folder both hold merges, by the same rule inside it.
        let day_log = "logs/roomlerd.log.2026-11-01";
        write_aged(&old.join(day_log), "old day", DAY);
        write_aged(&new.join(day_log), "new day", HOUR);
        write_aged(&old.join("logs/update-attempt"), "attempt", DAY);

        let notes = migrate_dirs_within(profile, &pairs, DataMerge::Runs, rename_on_disk);

        assert!(!old.exists(), "{notes:?}");
        let want = listing(&[
            ("host-mute.json", "new mute"),
            ("host-mute.json.older", "an earlier merge"),
            ("host-mute.json.older-2", "old mute"),
            ("keep-busy.json", "old busy"),
            ("keep-busy.json.older", "new busy"),
            ("logs/roomlerd.log.2026-11-01", "new day"),
            ("logs/roomlerd.log.2026-11-01.older", "old day"),
            ("logs/update-attempt", "attempt"),
            ("privacy-curtains.json", "new curtains"),
            ("privacy-curtains.json.older", "old curtains"),
        ]);
        assert_eq!(files_below(&new), want, "{notes:?}");
        let note = notes.join("\n");
        for beside in [
            "host-mute.json.older-2",
            "keep-busy.json.older",
            "privacy-curtains.json.older",
            "roomlerd.log.2026-11-01.older",
        ] {
            assert!(note.contains(beside), "{beside}: {note}");
        }
    }

    /// #3746 — two processes can run the move at the same moment (a service
    /// and a process it starts, or the person's daemon and a command typed
    /// by hand). Each takes an entry the other moved as moved and a folder
    /// the other made as one to merge into, and no rename takes a name that
    /// is taken: no file is lost or doubled, and the next start finishes
    /// whatever either one left.
    #[test]
    fn two_runners_at_once_lose_nothing_and_converge() {
        for round in 0..12 {
            let root = tempfile::tempdir().unwrap();
            let profile = root.path();
            let pairs = config_and_data(profile);
            let (old, new) = pairs[1].clone();
            let mut want = Vec::new();
            for i in 0..40 {
                let rel = match i % 4 {
                    0 => format!("state-{i}.json"),
                    1 => format!("logs/roomlerd.log.{i}"),
                    2 => format!("logs/crashes/c{i}.dmp"),
                    _ => format!("recordings/r{i}.webm"),
                };
                write_aged(&old.join(&rel), &format!("old {i}"), DAY);
                want.push(format!("old {i}"));
                if i % 5 == 0 {
                    write_aged(&new.join(&rel), &format!("new {i}"), HOUR);
                    want.push(format!("new {i}"));
                }
            }
            std::fs::create_dir_all(new.join("logs")).unwrap();
            want.sort();
            let contents = || {
                let mut all: Vec<String> = [&old, &new]
                    .iter()
                    .flat_map(|dir| files_below(dir).into_iter().map(|(_, body)| body))
                    .collect();
                all.sort();
                all
            };

            let start = std::sync::Barrier::new(2);
            let notes: Vec<Vec<String>> = std::thread::scope(|s| {
                let runners: Vec<_> = (0..2)
                    .map(|_| {
                        s.spawn(|| {
                            start.wait();
                            migrate_dirs_within(profile, &pairs, DataMerge::Runs, rename_on_disk)
                        })
                    })
                    .collect();
                runners.into_iter().map(|r| r.join().unwrap()).collect()
            });
            assert_eq!(contents(), want, "round {round}: {notes:?}");

            let next = migrate_dirs_within(profile, &pairs, DataMerge::Runs, rename_on_disk);
            assert!(!old.exists(), "round {round}: {notes:?} then {next:?}");
            assert_eq!(contents(), want, "round {round}: {notes:?} then {next:?}");
        }
    }

    /// #3746 — another runner made the NEW data folder, with a file under a
    /// name the OLD folder holds too, just before this run's rename of the
    /// folder was refused: no collision, the folders merge by the rule, and
    /// both copies stay.
    #[test]
    fn a_folder_another_runner_just_made_is_merged_into() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path();
        let pairs = config_and_data(profile);
        let (old, new) = pairs[1].clone();
        write_aged(&old.join("host-mute.json"), "ours", DAY);
        write_aged(&old.join("a.log"), "a", DAY);

        let notes = migrate_dirs_within(profile, &pairs, DataMerge::Runs, |from, to| {
            if from == old {
                // The other runner, first: the NEW folder, a file in it.
                write_aged(&new.join("host-mute.json"), "theirs", HOUR);
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            rename_on_disk(from, to)
        });

        assert!(!old.exists(), "{notes:?}");
        let want = listing(&[
            ("a.log", "a"),
            ("host-mute.json", "theirs"),
            ("host-mute.json.older", "ours"),
        ]);
        assert_eq!(files_below(&new), want, "{notes:?}");
    }

    /// #3746 — an entry another runner moved between this run's look at the
    /// OLD folder and its rename is moved, not left behind.
    #[test]
    fn an_entry_another_runner_moved_is_done() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path();
        let pairs = config_and_data(profile);
        let (old, new) = pairs[1].clone();
        write_aged(&old.join("a.log"), "a", DAY);
        write_aged(&old.join("b.log"), "b", DAY);
        std::fs::create_dir_all(new.join("logs")).unwrap();

        let notes = migrate_dirs_within(profile, &pairs, DataMerge::Runs, |from, to| {
            if from == old.join("a.log") {
                // The other runner, first.
                std::fs::rename(from, to).unwrap();
            }
            rename_on_disk(from, to)
        });

        assert!(!old.exists(), "{notes:?}");
        assert_eq!(
            files_below(&new),
            listing(&[("a.log", "a"), ("b.log", "b")]),
            "{notes:?}"
        );
        assert!(!notes.join("\n").contains("a.log"), "{notes:?}");
    }

    /// #3746 — a data pair that leaves an entry in the OLD folder holds back
    /// no pair after it: each data pair converges on its own, and the next
    /// start moves the entry that was left.
    #[test]
    fn a_data_pair_that_leaves_an_entry_behind_holds_back_no_pair_after_it() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path();
        let pairs = [
            pair_in(&profile.join("config")),
            pair_in(&profile.join("roaming")),
            pair_in(&profile.join("local")),
        ];
        write_aged(&pairs[0].0.join("config.toml"), "token", DAY);
        write_aged(&pairs[1].0.join("held.log"), "held", DAY);
        write_aged(&pairs[1].0.join("free.log"), "free", DAY);
        write_aged(&pairs[2].0.join("logs/x.log"), "x", DAY);
        let held = pairs[1].0.join("held.log");

        let notes = migrate_dirs_within(profile, &pairs, DataMerge::Runs, |from, to| {
            if from == pairs[1].0 || from == held {
                return Err(std::io::Error::other("held"));
            }
            rename_on_disk(from, to)
        });

        assert!(pairs[0].1.join("config.toml").is_file(), "{notes:?}");
        assert!(pairs[1].1.join("free.log").is_file(), "{notes:?}");
        assert!(held.is_file(), "{notes:?}");
        assert!(notes.join("\n").contains("held.log"), "{notes:?}");
        assert!(!pairs[2].0.exists(), "{notes:?}");
        assert!(pairs[2].1.join("logs").join("x.log").is_file(), "{notes:?}");

        let notes = migrate_dirs_within(profile, &pairs, DataMerge::Runs, rename_on_disk);
        assert!(!pairs[1].0.exists(), "{notes:?}");
        assert!(pairs[1].1.join("held.log").is_file(), "{notes:?}");
    }

    /// #3746 — the rename the move makes on disk never takes a name that is
    /// taken: not a file's, not a folder's that holds something, and not by
    /// a link. A free name it takes.
    #[test]
    fn the_rename_on_disk_never_takes_a_name_that_is_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let at = |name: &str| tmp.path().join(name);
        std::fs::write(at("a"), "a").unwrap();
        std::fs::write(at("b"), "b").unwrap();
        let err = rename_on_disk(&at("a"), &at("b")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(std::fs::read_to_string(at("a")).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(at("b")).unwrap(), "b");

        std::fs::create_dir_all(at("d").join("x")).unwrap();
        std::fs::create_dir_all(at("e").join("y")).unwrap();
        assert!(rename_on_disk(&at("d"), &at("e")).is_err());
        assert!(at("d").join("x").is_dir() && at("e").join("y").is_dir());

        folder_link(&at("l"), &at("d"));
        assert!(rename_on_disk(&at("l"), &at("b")).is_err());
        assert_eq!(std::fs::read_to_string(at("b")).unwrap(), "b");
        assert!(at("d").join("x").is_dir());

        rename_on_disk(&at("a"), &at("c")).unwrap();
        assert!(!at("a").exists());
        assert_eq!(std::fs::read_to_string(at("c")).unwrap(), "a");
    }

    /// #3746 — a link in the OLD data folder (a junction on Windows, a
    /// symbolic link elsewhere) is settled by the rule as a link and never
    /// followed: what it leads to stays where it is, and the NEW folder of
    /// the same name keeps everything it holds.
    #[test]
    fn a_link_in_the_old_folder_moves_as_a_link_and_is_never_followed() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let elsewhere = root.path().join("elsewhere");
        write_aged(&elsewhere.join("secret.txt"), "secret", DAY);
        let pairs = config_and_data(&profile);
        let (old, new) = pairs[1].clone();
        write_aged(&old.join("other.json"), "other", DAY);
        folder_link(&old.join("logs"), &elsewhere);
        write_aged(&new.join("logs").join("today.log"), "today", HOUR);

        let notes = migrate_dirs_within(&profile, &pairs, DataMerge::Runs, rename_on_disk);

        assert!(!old.exists(), "{notes:?}");
        let secret = listing(&[("secret.txt", "secret")]);
        assert_eq!(files_below(&elsewhere), secret, "{notes:?}");
        let link_beside = listing(&[
            ("logs.older", "<link>"),
            ("logs/today.log", "today"),
            ("other.json", "other"),
        ]);
        let link_named = listing(&[
            ("logs", "<link>"),
            ("logs.older/today.log", "today"),
            ("other.json", "other"),
        ]);
        let found = files_below(&new);
        assert!(
            found == link_beside || found == link_named,
            "{found:?}: {notes:?}"
        );
    }

    /// #3746 — a process running elevated from a split token, or one that
    /// could not read its own token, leaves a data pair's merge to the
    /// account's own daemon, with a note that says why. A pair it can move
    /// by one rename still moves, and the account's daemon merges the rest
    /// at its next start.
    #[test]
    fn an_elevated_process_leaves_the_data_merge_to_the_accounts_own_daemon() {
        for merge in [DataMerge::Elevated, DataMerge::TokenUnread] {
            let root = tempfile::tempdir().unwrap();
            let profile = root.path();
            let pairs = [
                pair_in(&profile.join("config")),
                pair_in(&profile.join("roaming")),
                pair_in(&profile.join("local")),
            ];
            write_aged(&pairs[1].0.join("host-mute.json"), "muted", DAY);
            write_aged(&pairs[1].1.join("logs/today.log"), "today", HOUR);
            write_aged(&pairs[2].0.join("logs/x.log"), "x", DAY);

            let notes = migrate_dirs_within(profile, &pairs, merge, rename_on_disk);

            let because = merge.left_because().unwrap();
            let old = pairs[1].0.display().to_string();
            assert!(
                notes
                    .iter()
                    .any(|n| n.contains(because) && n.contains(&old)),
                "{merge:?}: {notes:?}"
            );
            let muted = listing(&[("host-mute.json", "muted")]);
            assert_eq!(files_below(&pairs[1].0), muted, "{notes:?}");
            let today = listing(&[("logs/today.log", "today")]);
            assert_eq!(files_below(&pairs[1].1), today, "{notes:?}");
            assert!(!pairs[2].0.exists(), "{notes:?}");
            assert!(pairs[2].1.join("logs").join("x.log").is_file(), "{notes:?}");

            // The account's own daemon, at its next start.
            let notes = migrate_dirs_within(profile, &pairs, DataMerge::Runs, rename_on_disk);
            assert!(!pairs[1].0.exists(), "{notes:?}");
            let both = listing(&[("host-mute.json", "muted"), ("logs/today.log", "today")]);
            assert_eq!(files_below(&pairs[1].1), both, "{notes:?}");
        }
    }

    /// #3746 — only a process running elevated from a split token, or one
    /// that could not tell, leaves the merge.
    #[test]
    fn the_merge_runs_unless_the_process_runs_elevated_from_a_split_token() {
        assert_eq!(
            DataMerge::from_split_elevation(Some(false)),
            DataMerge::Runs
        );
        assert_eq!(
            DataMerge::from_split_elevation(Some(true)),
            DataMerge::Elevated
        );
        assert_eq!(
            DataMerge::from_split_elevation(None),
            DataMerge::TokenUnread
        );
        assert_eq!(DataMerge::Runs.left_because(), None);
    }

    /// #3746 — a copy kept beside a name is known by its suffix alone.
    #[test]
    fn a_copy_kept_beside_is_known_by_its_suffix() {
        for name in [
            "a.older",
            "a.older-2",
            "a.older.older",
            "roomlerd.log.2026-11-22.older-17",
        ] {
            assert!(is_kept_beside(name), "{name}");
        }
        for name in [
            "a.older-",
            "a.olderx",
            ".older",
            "a.older-2x",
            "a.log",
            "older",
            "roomlerd.log.2026-11-22",
        ] {
            assert!(!is_kept_beside(name), "{name}");
        }
    }
}

// RETIRED-NAME-ANCHOR-END
