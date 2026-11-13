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
const ORG: &str = "roomler";
/// New app segment (post-rename, fresh installs).
const NEW_APP: &str = "roomler";
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
/// the tree hangs off the home the account database names ([`dirs_for`]), and
/// a `ProjectDirs` takes its home from `$HOME` with no way to be given another.
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
/// Everywhere else `ProjectDirs` decides, as it always has: `$HOME` and the
/// XDG variables on Linux, the known folders on Windows.
fn dirs_for(app: &str) -> Option<AppDirs> {
    #[cfg(target_os = "macos")]
    {
        static ACCOUNT_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
        macos_dirs(app, || ACCOUNT_HOME.get_or_init(own_home).clone())
    }
    #[cfg(not(target_os = "macos"))]
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
/// dir at all, or on macOS the account database names no home.
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
///   * both exist (split-brain from the self-heal era) → left in place
///     (the resolution keeps the tree that holds the config folder, the new
///     one when both do, and with no config folder the new data folder's;
///     config cleanup is the `ConfigCleanupStale` verb's job)
///   * legacy absent → nothing to do
///
/// Never deletes; a failed rename (locked file, ACL) is noted and the
/// read-both resolution keeps everything working until the next start.
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
/// first and by one rename, and stops at a pair whose move fails: the pairs
/// before it stay moved, the pairs after it stay in the OLD tree. The
/// machine pair's root is [`machine_program_data`].
pub fn migrate_legacy_trees(
    companion_running: bool,
    installer_step: bool,
    own_profile: Option<&Path>,
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
                    notes.extend(migrate_dirs_within(profile, &pairs, rename_on_disk));
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
            let (old, new) = (base.join(OLD_APP), base.join(NEW_APP));
            if installer_step {
                if old.exists() {
                    notes.push(in_an_installer_step(&old));
                }
            } else {
                notes.extend(migrate_dirs_within(&root, &[(old, new)], rename_on_disk));
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
    use std::os::unix::ffi::OsStrExt;
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
        if rc != 0 || found.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        // SAFETY: `pw_dir` points into `buf`, NUL-terminated by `getpwuid_r`,
        // and `buf` outlives this borrow.
        let dir = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
        let home = PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
        return home.is_absolute().then_some(home);
    }
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
/// rename of a pair's folder ([`rename_on_disk`] outside tests). Whether
/// they move is decided for every pair before any pair moves: none moves
/// when one lies outside `root` or is reached through a link below it
/// ([`kept_in_place`]). They then move in the order given, the config pair
/// first and by one rename, and a pair whose move fails ends the move: the
/// pairs before it stay moved, the pairs after it stay in the OLD tree.
/// The segment choice ([`segment_of`]) takes the tree that holds a config
/// folder. The move makes the NEW config folder only whole, by one rename,
/// and the NEW local data folder only when the config pair's move did not
/// fail: the daemon reads the config it had, from the NEW tree or from the
/// OLD one.
fn migrate_dirs_within(
    root: &Path,
    pairs: &[(PathBuf, PathBuf)],
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Vec<String> {
    if let Some(note) = kept_in_place(root, pairs) {
        return vec![note];
    }
    let mut notes = Vec::new();
    for (at, (old, new)) in pairs.iter().enumerate() {
        // The first pair holds `config.toml`: one rename moves its folder
        // whole, or the folder stays. The pairs after it may move child by
        // child.
        match migrate_dir(old, new, at > 0, &mut rename) {
            Ok(note) => notes.extend(note),
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
/// tests.
fn rename_on_disk(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// Apply the migration rules to one directory pair, with `rename` as the
/// rename of its folder: `Ok` with a note for anything noteworthy (moved, or
/// left beside a NEW folder that is already there), `Ok(None)` for the quiet
/// no-legacy case, `Err` with a note when the move failed and the legacy
/// folder is still where it was. A rename the system refuses
/// (`PermissionDenied`) moves the folder's children one by one when
/// `per_child` ([`migrate_children`]), and is an `Err` otherwise.
fn migrate_dir(
    old: &Path,
    new: &Path,
    per_child: bool,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<Option<String>, String> {
    if !old.exists() {
        return Ok(None);
    }
    if new.exists() {
        return Ok(Some(format!(
            "legacy tree left in place (new tree also present): {}",
            old.display()
        )));
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
        Err(e) if per_child && e.kind() == std::io::ErrorKind::PermissionDenied => {
            // Field finding (DEVBOX, 2026-07-27): renaming the TOP dir was
            // denied by an external ROOT-handle (an AV / indexer
            // directory-watch — no roomler process held anything, ACLs
            // were Full, and every CHILD renamed fine). Fall back to
            // moving the children individually; that dodges the
            // root-handle without needing the watcher gone.
            migrate_children(old, new, &e).map(Some)
        }
        Err(e) => Err(format!(
            "migration failed ({} -> {}): {e} — read-both fallback stays in charge",
            old.display(),
            new.display()
        )),
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

/// Per-child move of a data pair whose folder rename the system refused
/// (never the first pair, which holds `config.toml`). A failure midway
/// renames the moved entries back and removes the NEW folder this move
/// made; either can fail in turn, and the note's `rolled back` says only
/// whether every rename back succeeded.
fn migrate_children(old: &Path, new: &Path, root_err: &std::io::Error) -> Result<String, String> {
    let Ok(entries) = std::fs::read_dir(old) else {
        return Err(format!(
            "migration failed ({} -> {}): {root_err} (and the legacy dir is unreadable)",
            old.display(),
            new.display()
        ));
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    // config.toml last: the enrollment must be the final thing to leave
    // the old tree.
    children.sort_by_key(|p| p.file_name().is_some_and(|f| f == "config.toml"));
    let made_new = !new.exists();
    if let Err(e) = std::fs::create_dir_all(new) {
        return Err(format!("migration failed (create {}): {e}", new.display()));
    }
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for src in &children {
        let Some(name) = src.file_name() else {
            continue;
        };
        let dst = new.join(name);
        match std::fs::rename(src, &dst) {
            Ok(()) => moved.push((src.clone(), dst)),
            Err(e) => {
                // Roll back so the OLD tree stays complete (best-effort;
                // a rollback failure is noted and read-both still covers
                // the pieces that DID move — they're identical content).
                let mut rolled_back = true;
                for (orig, dstp) in moved.iter().rev() {
                    if std::fs::rename(dstp, orig).is_err() {
                        rolled_back = false;
                    }
                }
                // The NEW folder this move made goes too: the segment choice
                // keys on a new config folder, and on a new data folder where
                // neither tree holds a config folder. `remove_dir` leaves it
                // while anything is in it.
                if made_new {
                    let _ = std::fs::remove_dir(new);
                }
                return Err(format!(
                    "per-child migration of {} aborted at {} ({e}); rolled back: {rolled_back} \
                     (root rename was denied: {root_err})",
                    old.display(),
                    src.display()
                ));
            }
        }
    }
    let _ = std::fs::remove_dir(old);
    Ok(format!(
        "migrated legacy tree {} -> {} per-child ({} entries; the root dir itself was \
         locked by another process: {root_err})",
        old.display(),
        new.display(),
        moved.len()
    ))
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

        let notes = migrate_dirs_within(&profile, &[config.clone(), data.clone()], rename_on_disk);

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
        let notes = migrate_dirs_within(&plain, &pairs, rename_on_disk);
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
        let notes = migrate_dirs_within(profile, &[config.clone(), data.clone()], |from, to| {
            tried.push(from.to_path_buf());
            if from == config.0 {
                return Err(std::io::Error::other("another volume"));
            }
            std::fs::rename(from, to)
        });
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

        let notes = migrate_dirs_within(profile, &[config.clone(), data.clone()], |from, to| {
            if from == config.0 {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            std::fs::rename(from, to)
        });

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

        let note = migrate_dir(&old, &new, true, rename_on_disk)
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

    #[test]
    fn migrate_dir_leaves_split_brain_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("roomler-agent");
        let new = tmp.path().join("roomler");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("stale.txt"), b"old").unwrap();

        let note = migrate_dir(&old, &new, true, rename_on_disk)
            .unwrap()
            .expect("both-present produces a note");
        assert!(note.contains("left in place"), "{note}");
        assert!(old.join("stale.txt").exists(), "nothing may be deleted");
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
        let note = migrate_children(&old, &new, &err).unwrap();
        assert!(note.contains("per-child"), "{note}");
        assert!(!old.exists(), "emptied legacy root must be removed");
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).unwrap(),
            "agent_token = \"x\""
        );
        assert!(new.join("service-logs").join("a.log").exists());
    }

    #[test]
    fn migrate_children_rolls_back_on_midway_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("roomler-agent");
        let new = tmp.path().join("roomler");
        std::fs::create_dir_all(old.join("crashes")).unwrap();
        std::fs::write(old.join("config.toml"), b"tok").unwrap();
        // Pre-create a CONFLICTING non-empty destination for `crashes` so
        // its rename fails mid-way (rename onto a non-empty dir errors).
        std::fs::create_dir_all(new.join("crashes").join("blocker")).unwrap();

        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "root locked");
        let note = migrate_children(&old, &new, &err).unwrap_err();
        assert!(note.contains("aborted"), "{note}");
        // config.toml sorted LAST → it never left the old tree before the
        // failure, and anything that did move was rolled back.
        assert!(
            old.join("config.toml").exists(),
            "the enrollment must still be in the OLD tree: {note}"
        );
        assert!(old.join("crashes").exists());
    }

    /// #3702 — a per-child move that stops midway leaves no NEW folder it
    /// made: the segment choice keys on that folder being there. A file held
    /// open with no sharing cannot be renamed on Windows.
    #[cfg(windows)]
    #[test]
    fn a_per_child_move_that_stops_leaves_no_new_folder() {
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

        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "root locked");
        let note = migrate_children(&old, &new, &err).unwrap_err();
        drop(held);

        assert!(note.contains("aborted"), "{note}");
        assert!(!new.exists(), "{note}");
        for name in ["a.log", "b.log", "config.toml"] {
            assert!(old.join(name).is_file(), "{name}: {note}");
        }
    }

    #[test]
    fn migrate_dir_is_quiet_without_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            migrate_dir(
                &tmp.path().join("roomler-agent"),
                &tmp.path().join("roomler"),
                true,
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
    /// installed Mac reads moves. Everywhere else every answer is
    /// `ProjectDirs`'. The child only resolves paths: the `HOME` it is given
    /// stays empty, and nothing is made in the home of whoever runs this.
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
            #[cfg(target_os = "macos")]
            {
                let account = own_home().expect("the account database names this account's home");
                assert!(
                    theirs.starts_with(given.path()),
                    "{name}: ProjectDirs did not follow the given HOME: {theirs:?}"
                );
                assert!(
                    ours.starts_with(&account) && !ours.starts_with(given.path()),
                    "{name}: {ours:?} is not in {account:?}, the home of the account the child runs as"
                );
            }
            #[cfg(not(target_os = "macos"))]
            assert_eq!(ours, theirs, "{name}: the folders moved off ProjectDirs");
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
}

// RETIRED-NAME-ANCHOR-END
