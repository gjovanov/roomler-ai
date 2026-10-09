// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P1i — agent sessions on Windows: who a session runs as, which harness
//! it runs, and the session's preparation.
//!
//! A Windows device runs a session as the user signed in at the console, and as
//! nobody else (design §0.1, D2). Never as SYSTEM, which the daemon is. Never as
//! another account either, which would take that account's password: there is
//! no S4U and no `LogonUser` here, and the daemon asks for no credential. So
//! `hive_accounts` must map the starter to the console user, and with nobody
//! signed in a start is refused `no_console_user`.
//!
//! These are the Windows counterparts of what the Unix launcher does with
//! `setuid` and a `/bin/sh` wrapper (`hive/supervisor.rs`):
//!
//! | Unix | Windows |
//! |---|---|
//! | `apply_run_as` to the mapped account | [`console_user`]'s token, after [`check_mapping`] |
//! | `resolve_harness` | [`resolve_harness`], then [`harness_command_line`] |
//! | the wrapper, run as the account | `roomlerd hive-prep` ([`prep`]), which [`run_prep`] runs as the console user |
//! | the harness's process group | its Job Object ([`JobObject`], [`spawn_into_job`]) |
//!
//! P1i-2 wires them into the supervisor. Until then nothing calls them, and the
//! Windows build does not advertise `hive`.
//!
//! [`JobObject`]: crate::win_service::supervisor::JobObject
//! [`spawn_into_job`]: crate::win_service::supervisor::spawn_into_job

#![cfg(windows)]

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use roomler_ai_remote_control::hive::HiveRefusal;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::{
    GetTokenInformation, LookupAccountSidW, SID_NAME_USE, TOKEN_USER, TokenUser,
};

use crate::win_service::supervisor::{
    self, CapturedChild, EnvBlock, JobObject, OwnedHandle, SpawnAs,
};

/// The hidden subcommand the daemon runs as the console user before a harness
/// starts: `roomlerd hive-prep <config-dir> <folder> <memory-dir>
/// <auto-memory-dir>`, `<memory-dir>` empty for none.
pub const PREP_SUBCOMMAND: &str = "hive-prep";

/// How long a preparation may take: two directories and two small copies.
pub const PREP_TIMEOUT: Duration = Duration::from_secs(30);

/// What a preparation's output is read up to: it says one line at most.
const PREP_OUTPUT: u64 = 8 * 1024;

/// The user signed in at the console: the one account a session here runs as.
pub struct ConsoleUser {
    /// The console's session id.
    pub session: u32,
    /// `LookupAccountSidW`'s domain: the computer's name for a local account,
    /// `AzureAD` for an Entra ID one, the NetBIOS domain for a domain one.
    pub domain: String,
    pub name: String,
    /// The profile Windows records for the user, from the token. Never built
    /// from the name: `C:\Users\alice` and `C:\Users\alice.CORP` are two people.
    pub profile: PathBuf,
    /// `%APPDATA%` in the user's own environment, where npm puts `claude.cmd`.
    pub appdata: Option<PathBuf>,
    token: OwnedHandle,
}

impl ConsoleUser {
    /// What a session's processes are started as. The token lives as long as
    /// `self`, so `self` must outlive the spawn.
    pub fn spawn_as(&self) -> SpawnAs {
        SpawnAs::User(self.token.raw())
    }

    /// `DOMAIN\name`.
    pub fn qualified(&self) -> String {
        format!("{}\\{}", self.domain, self.name)
    }
}

/// Who is signed in at the console, or why no session can run as them.
///
/// `no_console_user` when nobody is: no console session, or one showing the
/// sign-in screen. A daemon that may not ask (it is not SYSTEM, so
/// `WTSQueryUserToken` is refused) gets `launch_failed`, saying so.
pub fn console_user() -> Result<ConsoleUser, (HiveRefusal, String)> {
    let Some(session) = supervisor::active_console_session_id() else {
        return Err((
            HiveRefusal::NoConsoleUser,
            "nobody is signed in at the console, and on Windows a session runs only as the \
             console user"
                .into(),
        ));
    };
    let token = match supervisor::query_user_token(session) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Err((
                HiveRefusal::NoConsoleUser,
                format!(
                    "nobody is signed in at the console (session {session} has no user), and on \
                     Windows a session runs only as the console user"
                ),
            ));
        }
        Err(e) => {
            return Err((
                HiveRefusal::LaunchFailed,
                format!(
                    "cannot obtain the console user's token ({e}); this needs the daemon to run \
                     as SYSTEM, a perMachine service install"
                ),
            ));
        }
    };
    let (domain, name) = account_of_token(token.raw()).map_err(|e| {
        (
            HiveRefusal::LaunchFailed,
            format!("reading the console user's account: {e}"),
        )
    })?;
    let profile = crate::win_identity::profile_dir_of_token(token.raw())
        .map(PathBuf::from)
        .ok_or_else(|| {
            (
                HiveRefusal::LaunchFailed,
                format!("{domain}\\{name} has no profile directory"),
            )
        })?;
    let appdata = EnvBlock::for_token(token.raw())
        .ok()
        .and_then(|block| env_value(&block.entries(), "APPDATA"))
        .map(PathBuf::from);
    Ok(ConsoleUser {
        session,
        domain,
        name,
        profile,
        appdata,
        token,
    })
}

/// The account a token is for, as `(domain, name)`. `token` must be live,
/// with `TOKEN_QUERY`.
fn account_of_token(token: HANDLE) -> Result<(String, String), String> {
    let mut len: u32 = 0;
    // SAFETY: the documented size query: no buffer, the size written to `len`.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        return Err(format!(
            "GetTokenInformation(TokenUser): {}",
            std::io::Error::last_os_error()
        ));
    }
    // `u64`s, so the TOKEN_USER, which holds a pointer, is aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes and outlives the call.
    if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0
    {
        return Err(format!(
            "GetTokenInformation(TokenUser): {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: a TOKEN_USER, whose SID points into `buf`, alive below.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    let (mut name_len, mut domain_len) = (0u32, 0u32);
    let mut kind: SID_NAME_USE = 0;
    // SAFETY: the documented size query: null buffers, the sizes written out.
    unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid,
            std::ptr::null_mut(),
            &mut name_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut kind,
        )
    };
    if name_len == 0 {
        return Err(format!(
            "LookupAccountSidW: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut name = vec![0u16; name_len as usize];
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    domain_len = domain.len() as u32;
    // SAFETY: both buffers hold the sizes passed and outlive the call.
    let ok = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    if ok == 0 {
        return Err(format!(
            "LookupAccountSidW: {}",
            std::io::Error::last_os_error()
        ));
    }
    // On success the lengths exclude the terminators.
    let name = &name[..(name_len as usize).min(name.len())];
    let domain = &domain[..(domain_len as usize).min(domain.len())];
    Ok((
        String::from_utf16_lossy(domain),
        String::from_utf16_lossy(name),
    ))
}

/// Whether `mapped`, what `hive_accounts` maps the starter to, names the
/// console user `domain\name`. It may say `name`, `DOMAIN\name`, or `.\name`
/// for an account local to this computer (`computer`), compared
/// case-insensitively, as Windows compares account names. An empty name never
/// matches, nor does `.` on a computer whose name is unknown.
pub fn names_console_user(mapped: &str, domain: &str, name: &str, computer: &str) -> bool {
    let same = |a: &str, b: &str| !a.is_empty() && a.to_uppercase() == b.to_uppercase();
    let mapped = mapped.trim();
    match mapped.split_once('\\') {
        None => same(mapped, name),
        Some((".", n)) => same(computer, domain) && same(n, name),
        Some((d, n)) => same(d, domain) && same(n, name),
    }
}

/// A start's account gate on Windows: what `hive_accounts` maps the starter to
/// must name the console user. A refusal says who is at the console, which is
/// what the device's owner needs to fix the map.
pub fn check_mapping(mapped: &str, console: &ConsoleUser) -> Result<(), (HiveRefusal, String)> {
    let computer = std::env::var("COMPUTERNAME").unwrap_or_default();
    check_mapping_for(mapped, &console.domain, &console.name, &computer)
}

fn check_mapping_for(
    mapped: &str,
    domain: &str,
    name: &str,
    computer: &str,
) -> Result<(), (HiveRefusal, String)> {
    if names_console_user(mapped, domain, name, computer) {
        return Ok(());
    }
    Err((
        HiveRefusal::NoAccount,
        format!(
            "this user maps to {mapped:?}, but {domain}\\{name} is signed in at the console, and \
             on Windows a session runs only as the console user (map them to `name` or \
             `DOMAIN\\name`)"
        ),
    ))
}

/// Where Claude Code is looked for, in order: `hive_harness` alone when it is
/// set; else the native installer's `%USERPROFILE%\.local\bin\claude.exe`, then
/// npm's `%APPDATA%\npm\claude.cmd`.
pub fn harness_candidates(
    configured: Option<&Path>,
    profile: &Path,
    appdata: Option<&Path>,
) -> Vec<PathBuf> {
    if let Some(h) = configured {
        return vec![h.to_path_buf()];
    }
    let mut c = vec![profile.join(".local").join("bin").join("claude.exe")];
    if let Some(a) = appdata {
        c.push(a.join("npm").join("claude.cmd"));
    }
    c
}

/// The first candidate that is a file, or `None`: `harness_missing`.
pub fn resolve_harness(
    configured: Option<&Path>,
    profile: &Path,
    appdata: Option<&Path>,
) -> Option<PathBuf> {
    harness_candidates(configured, profile, appdata)
        .into_iter()
        .find(|p| std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false))
}

/// One argument as `CommandLineToArgvW` and the C runtime read it back:
/// as it is when nothing in it needs quoting; otherwise in quotes, each `"`
/// escaped, and each run of backslashes before a `"`, or before the closing
/// quote, doubled.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut slashes = 0usize;
    for c in arg.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        out.push(c);
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

/// A program's path as the first token of a command line: in quotes, which a
/// Windows path cannot contain, so no escaping is needed and none is possible.
fn program(path: &Path) -> Result<String, String> {
    let p = utf8(path.as_os_str(), "the program's path")?;
    if p.contains('"') {
        return Err(format!("{p} holds a quote"));
    }
    Ok(format!("\"{p}\""))
}

fn utf8<'a>(s: &'a std::ffi::OsStr, what: &str) -> Result<&'a str, String> {
    s.to_str()
        .ok_or_else(|| format!("{what} is not valid Unicode: {}", s.to_string_lossy()))
}

/// What `cmd.exe` reads as its own, inside quotes or out: expansion (`%`, and
/// `!` should delayed expansion be on), escapes, and the quote itself.
const CMD_UNSAFE_ANYWHERE: &[char] = &['%', '!', '^', '"'];
/// What it reads as its own outside quotes, where an argument the shim passes
/// on with `%*` lands: npm's `claude.cmd` expands `%*` inside an `IF (…)` block,
/// so a `)` there ends the block early.
const CMD_UNSAFE_UNQUOTED: &[char] = &['&', '|', '<', '>', '(', ')'];

/// The command line that starts `harness` with `args`.
///
/// An `.exe` runs directly, each argument quoted for the C runtime. A `.cmd`
/// or `.bat` (npm's shim) runs through `cmd.exe`, which reads the line again
/// by its own rules, and the shim passes it on once more with `%*`. No quoting
/// is right for both readers, so there an argument, or the path, holding one of
/// cmd's metacharacters is refused, never escaped. Anything else is refused,
/// because it would not start.
pub fn harness_command_line(harness: &Path, args: &[OsString]) -> Result<String, String> {
    let ext = harness
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let args: Vec<&str> = args
        .iter()
        .map(|a| utf8(a, "an argument"))
        .collect::<Result<_, _>>()?;
    match ext.as_deref() {
        Some("exe") => {
            let mut line = program(harness)?;
            for a in args {
                line.push(' ');
                line.push_str(&quote_arg(a));
            }
            Ok(line)
        }
        Some("cmd" | "bat") => {
            let path = utf8(harness.as_os_str(), "the harness's path")?;
            if path.contains(CMD_UNSAFE_ANYWHERE) || path.contains(char::is_control) {
                return Err(format!(
                    "{path} holds a character cmd.exe would read as its own; install Claude Code \
                     where its path has none, or set hive_harness to its .exe"
                ));
            }
            let mut inner = format!("\"{path}\"");
            for a in args {
                if a.contains(CMD_UNSAFE_ANYWHERE)
                    || a.contains(CMD_UNSAFE_UNQUOTED)
                    || a.contains(char::is_control)
                {
                    return Err(format!(
                        "the argument {a:?} holds a character cmd.exe would read as its own, so \
                         {path} cannot be given it safely; set hive_harness to Claude Code's .exe"
                    ));
                }
                inner.push(' ');
                inner.push_str(&quote_arg(a));
            }
            // `/s`: cmd strips the first and the last quote and runs the rest
            // as it stands. `/d`: no AutoRun from the registry runs first.
            // `/v:off`: no delayed expansion, whatever the registry says.
            Ok(format!(
                "{} /d /v:off /s /c \"{inner}\"",
                program(&system_cmd())?
            ))
        }
        _ => Err(format!(
            "{} is neither an .exe nor a .cmd, so Windows cannot start it",
            harness.display()
        )),
    }
}

/// `%SystemRoot%\System32\cmd.exe`, by its full path: a bare `cmd.exe` is
/// looked for in the daemon's own directory first.
fn system_cmd() -> PathBuf {
    std::env::var_os("SystemRoot")
        .filter(|r| !r.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join("cmd.exe")
}

/// A variable's value among an environment block's entries, the name compared
/// case-insensitively.
fn env_value(entries: &[Vec<u16>], name: &str) -> Option<OsString> {
    let name: Vec<u16> = name.encode_utf16().collect();
    entries.iter().find_map(|e| {
        let (n, rest) = e.split_at_checked(name.len())?;
        let (eq, value) = rest.split_first()?;
        (*eq == u16::from(b'=')
            && String::from_utf16_lossy(n).to_uppercase()
                == String::from_utf16_lossy(&name).to_uppercase())
        .then(|| OsString::from_wide(value))
    })
}

/// `roomlerd hive-prep`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepArgs {
    /// `CLAUDE_CONFIG_DIR`: made, as the user.
    pub config_dir: PathBuf,
    /// The session's folder: it must open as the user.
    pub folder: PathBuf,
    /// The daemon's copy of the session's core memory, when the start came
    /// with one and the device shows it.
    pub memory_dir: Option<PathBuf>,
    /// Where `MEMORY.md` goes.
    pub auto_memory_dir: PathBuf,
}

impl PrepArgs {
    /// This process's own, when it was started as `roomlerd hive-prep`:
    /// `None` when it was not, and an error when it was, wrongly.
    pub fn from_env() -> Option<Result<Self, String>> {
        Self::parse(std::env::args_os().skip(1))
    }

    fn parse(mut args: impl Iterator<Item = OsString>) -> Option<Result<Self, String>> {
        if args.next()? != PREP_SUBCOMMAND {
            return None;
        }
        let rest: Vec<OsString> = args.collect();
        let given = rest.len();
        let Ok([config_dir, folder, memory, auto_memory_dir]) = <[OsString; 4]>::try_from(rest)
        else {
            return Some(Err(format!(
                "{PREP_SUBCOMMAND} takes four arguments (<config-dir> <folder> <memory-dir> \
                 <auto-memory-dir>), got {given}"
            )));
        };
        Some(Ok(Self {
            config_dir: config_dir.into(),
            folder: folder.into(),
            memory_dir: (!memory.is_empty()).then(|| memory.into()),
            auto_memory_dir: auto_memory_dir.into(),
        }))
    }

    /// The command line that runs it: `"<exe>" hive-prep …`.
    pub fn command_line(&self, exe: &Path) -> Result<String, String> {
        let mut line = program(exe)?;
        line.push(' ');
        line.push_str(PREP_SUBCOMMAND);
        let empty = PathBuf::new();
        for p in [
            &self.config_dir,
            &self.folder,
            self.memory_dir.as_ref().unwrap_or(&empty),
            &self.auto_memory_dir,
        ] {
            line.push(' ');
            line.push_str(&quote_arg(utf8(p.as_os_str(), "a path")?));
        }
        Ok(line)
    }
}

/// Prepare a session, as the user it runs as: what the Unix wrapper does as
/// the account before it becomes the harness.
///
/// Makes the config directory. When the start came with core memory, copies
/// its `CLAUDE.md` into the config directory and its `MEMORY.md` into the
/// auto-memory directory, each only when nothing is there yet, so a resume
/// keeps what the session has, its own edits included; a copy that fails is
/// skipped, because memory never stops a session. Last, the folder must open
/// as this user, as the wrapper's `cd` requires.
///
/// The directories are made under the user's profile and inherit its
/// permissions, which are the user's own: the counterpart of the wrapper's
/// `umask 077`.
pub fn prep(args: &PrepArgs) -> Result<(), String> {
    let paths = [
        ("the config directory", Some(&args.config_dir)),
        ("the folder", Some(&args.folder)),
        ("the memory directory", args.memory_dir.as_ref()),
        ("the auto-memory directory", Some(&args.auto_memory_dir)),
    ];
    for (what, path) in paths {
        if let Some(p) = path
            && !p.is_absolute()
        {
            return Err(format!(
                "{what} must be an absolute path, got {}",
                p.display()
            ));
        }
    }
    std::fs::create_dir_all(&args.config_dir)
        .map_err(|e| format!("making {}: {e}", args.config_dir.display()))?;
    if let Some(memory) = &args.memory_dir {
        copy_if_absent(&memory.join("CLAUDE.md"), &args.config_dir, "CLAUDE.md");
        copy_if_absent(
            &memory.join("MEMORY.md"),
            &args.auto_memory_dir,
            "MEMORY.md",
        );
    }
    std::fs::read_dir(&args.folder)
        .map(drop)
        .map_err(|e| format!("{} does not open as this user: {e}", args.folder.display()))
}

/// `src` into `dir\name` when `src` is a file and nothing is at the
/// destination, not even a link; `dir` is made when it must be. Every failure
/// is skipped.
fn copy_if_absent(src: &Path, dir: &Path, name: &str) {
    let dst = dir.join(name);
    let is_file = std::fs::metadata(src).map(|m| m.is_file()).unwrap_or(false);
    if !is_file || std::fs::symlink_metadata(&dst).is_ok() {
        return;
    }
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::copy(src, &dst);
    }
}

/// Run `cmdline` (a [`PrepArgs::command_line`]) as `who`, and wait for it, at
/// most `timeout`: `Ok` when it exited 0, else what it said, for the refusal.
/// It runs in a job of its own, so a preparation that hangs is ended whole.
///
/// # Safety
/// As [`spawn_into_job`](supervisor::spawn_into_job): `SpawnAs::User`'s token
/// must stay alive across the call.
pub unsafe fn run_prep(who: SpawnAs, cmdline: &str, timeout: Duration) -> Result<(), String> {
    let job = JobObject::kill_on_close().map_err(|e| format!("{e:#}"))?;
    // SAFETY: forwarded from the caller.
    let child = unsafe { supervisor::spawn_into_job(who, cmdline, None, &[], false, &job) }
        .map_err(|e| format!("{e:#}"))?;
    let CapturedChild {
        process,
        stdout,
        stderr,
        ..
    } = child;
    // Both pipes at once (one read to the end first deadlocks on the other),
    // and bounded.
    let budget = Arc::new(AtomicU64::new(PREP_OUTPUT));
    let out_budget = Arc::clone(&budget);
    let out = std::thread::spawn(move || supervisor::read_pipe_to_end(&stdout, &out_budget));
    let err = std::thread::spawn(move || supervisor::read_pipe_to_end(&stderr, &budget));
    let finished = process.wait_for_exit(timeout);
    if !finished {
        let _ = job.terminate(1);
        let _ = process.wait_for_exit(Duration::from_secs(5));
    }
    let _ = out.join();
    let said = err
        .join()
        .map(|(bytes, _)| String::from_utf8_lossy(&bytes).trim().to_string())
        .unwrap_or_default();
    if !finished {
        return Err(format!(
            "preparing the session did not finish within {} s",
            timeout.as_secs()
        ));
    }
    match process.try_wait() {
        Ok(Some(0)) => Ok(()),
        Ok(Some(code)) if said.is_empty() => {
            Err(format!("preparing the session failed (exit {code})"))
        }
        Ok(Some(_)) => Err(said),
        Ok(None) => Err("preparing the session had not ended".into()),
        Err(e) => Err(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    /// What a command line reads back as, by the rules a C-runtime program
    /// (Claude Code's `.exe`, `roomlerd`) parses it with.
    fn argv(line: &str) -> Vec<String> {
        let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
        let mut n = 0i32;
        // SAFETY: a NUL-terminated line; the array is freed below.
        let list = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut n) };
        assert!(!list.is_null(), "CommandLineToArgvW failed on {line:?}");
        let out = (0..n as usize)
            .map(|i| {
                // SAFETY: `n` NUL-terminated strings.
                let p = unsafe { *list.add(i) };
                let len = (0..).take_while(|&k| unsafe { *p.add(k) } != 0).count();
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
            })
            .collect();
        // SAFETY: the one block CommandLineToArgvW allocated.
        unsafe { LocalFree(list as _) };
        out
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_map_must_name_the_console_user_and_nobody_else() {
        // (mapped, the console's domain, its name, this computer) → names it?
        let table = [
            ("dev", "WS-1", "dev", "WS-1", true),
            ("DEV", "WS-1", "dev", "WS-1", true),
            (" dev ", "WS-1", "dev", "WS-1", true),
            (r"WS-1\dev", "WS-1", "dev", "WS-1", true),
            (r"ws-1\DEV", "WS-1", "dev", "WS-1", true),
            (r".\dev", "WS-1", "dev", "WS-1", true),
            (r"AzureAD\JaneDoe", "AzureAD", "JaneDoe", "WS-1", true),
            // Another account, or this one in another domain.
            ("ops", "WS-1", "dev", "WS-1", false),
            (r"CORP\dev", "WS-1", "dev", "WS-1", false),
            // `.` is THIS computer: a domain account is not local.
            (r".\dev", "CORP", "dev", "WS-1", false),
            // A computer whose name is unknown matches no `.`.
            (r".\dev", "WS-1", "dev", "", false),
            // Nothing matches nothing.
            ("", "WS-1", "dev", "WS-1", false),
            (r"WS-1\", "WS-1", "dev", "WS-1", false),
            (r"\dev", "WS-1", "dev", "WS-1", false),
            // A UPN is not one of the forms.
            ("dev@example.com", "WS-1", "dev", "WS-1", false),
        ];
        for (mapped, domain, name, computer, names) in table {
            assert_eq!(
                names_console_user(mapped, domain, name, computer),
                names,
                "{mapped:?} vs {domain}\\{name} on {computer:?}"
            );
        }
    }

    /// The refusal is `no_account`, and says who is at the console: what an
    /// owner whose map says `jane` needs to see when Windows calls her
    /// `AzureAD\JaneDoe`.
    #[test]
    fn a_map_naming_someone_else_is_refused_saying_who_is_at_the_console() {
        let (refusal, detail) =
            check_mapping_for("jane", "AzureAD", "JaneDoe", "WS-1").unwrap_err();
        assert_eq!(refusal, HiveRefusal::NoAccount);
        assert!(detail.contains("\"jane\""), "{detail}");
        assert!(detail.contains(r"AzureAD\JaneDoe"), "{detail}");
        assert!(check_mapping_for(r"AzureAD\JaneDoe", "AzureAD", "JaneDoe", "WS-1").is_ok());
    }

    #[test]
    fn the_harness_is_hive_harness_alone_else_the_native_install_then_npm() {
        let profile = Path::new(r"C:\Users\dev");
        let appdata = Path::new(r"C:\Users\dev\AppData\Roaming");
        assert_eq!(
            harness_candidates(None, profile, Some(appdata)),
            [
                PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe"),
                PathBuf::from(r"C:\Users\dev\AppData\Roaming\npm\claude.cmd"),
            ]
        );
        assert_eq!(
            harness_candidates(None, profile, None),
            [PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe")]
        );
        assert_eq!(
            harness_candidates(
                Some(Path::new(r"D:\tools\claude.exe")),
                profile,
                Some(appdata)
            ),
            [PathBuf::from(r"D:\tools\claude.exe")],
            "a configured harness is the only candidate"
        );
    }

    #[test]
    fn the_first_candidate_that_is_a_file_is_the_harness() {
        let home = tempfile::tempdir().unwrap();
        let appdata = home.path().join("AppData").join("Roaming");
        assert_eq!(resolve_harness(None, home.path(), Some(&appdata)), None);
        std::fs::create_dir_all(appdata.join("npm")).unwrap();
        std::fs::write(appdata.join("npm").join("claude.cmd"), "@echo off").unwrap();
        assert_eq!(
            resolve_harness(None, home.path(), Some(&appdata)),
            Some(appdata.join("npm").join("claude.cmd"))
        );
        let native = home.path().join(".local").join("bin");
        std::fs::create_dir_all(&native).unwrap();
        // A directory by that name is not the harness.
        std::fs::create_dir(native.join("claude.exe")).unwrap();
        assert_eq!(
            resolve_harness(None, home.path(), Some(&appdata)),
            Some(appdata.join("npm").join("claude.cmd"))
        );
        std::fs::remove_dir(native.join("claude.exe")).unwrap();
        std::fs::write(native.join("claude.exe"), b"MZ").unwrap();
        assert_eq!(
            resolve_harness(None, home.path(), Some(&appdata)),
            Some(native.join("claude.exe"))
        );
        assert_eq!(
            resolve_harness(
                Some(&home.path().join("missing.exe")),
                home.path(),
                Some(&appdata)
            ),
            None,
            "a configured harness that is not there is missing, with no fallback"
        );
    }

    /// Every argument reads back exactly as it was given, however it is made.
    #[test]
    fn an_exe_harness_gets_each_argument_back_exactly() {
        let args = [
            "-p",
            "",
            "two words",
            r"C:\ProgramData\Roomler\hive\run\abc\settings.json",
            r"C:\dir with space\",
            r#"say "hi""#,
            r#"a\"b"#,
            r"trailing\\",
            "tab\there",
            "mcp__roomler__approve",
        ];
        let line =
            harness_command_line(Path::new(r"C:\Users\dev\.local\bin\claude.exe"), &os(&args))
                .unwrap();
        let back = argv(&line);
        assert_eq!(back[0], r"C:\Users\dev\.local\bin\claude.exe");
        assert_eq!(&back[1..], &args);
    }

    #[test]
    fn a_cmd_harness_runs_through_cmd_and_refuses_what_cmd_would_read() {
        let shim = Path::new(r"C:\Users\dev\AppData\Roaming\npm\claude.cmd");
        let line = harness_command_line(shim, &os(&["-p", "--session-id", "x y"])).unwrap();
        assert!(
            line.ends_with(r#" /d /v:off /s /c ""C:\Users\dev\AppData\Roaming\npm\claude.cmd" -p --session-id "x y"""#),
            "{line}"
        );
        assert!(
            line.to_ascii_lowercase()
                .starts_with(&format!("\"{}", system_cmd().display()).to_ascii_lowercase()),
            "cmd.exe by its full path: {line}"
        );
        for bad in [
            "100%", "a&b", "a|b", "a>b", "a<b", "a)b", "a(b", "a^b", "a!b", r#"a"b"#, "a\nb",
        ] {
            assert!(
                harness_command_line(shim, &os(&[bad])).is_err(),
                "{bad:?} must be refused for a .cmd harness"
            );
        }
        assert!(
            harness_command_line(Path::new(r"C:\Users\100%\npm\claude.cmd"), &[]).is_err(),
            "a path cmd would expand is refused"
        );
        assert!(harness_command_line(Path::new(r"C:\tools\claude.ps1"), &[]).is_err());
        assert!(harness_command_line(Path::new(r"C:\tools\claude"), &[]).is_err());
    }

    #[test]
    fn prep_args_read_back_from_their_command_line() {
        let a = PrepArgs {
            config_dir: PathBuf::from(r"C:\Users\dev\.roomler\hive\s1\claude"),
            folder: PathBuf::from(r"C:\work\my app"),
            memory_dir: None,
            auto_memory_dir: PathBuf::from(
                r"C:\Users\dev\.roomler\hive\s1\claude\projects\hive-s1\memory",
            ),
        };
        let line = a
            .command_line(Path::new(r"C:\Program Files\Roomler\roomlerd.exe"))
            .unwrap();
        let back = argv(&line);
        assert_eq!(back[0], r"C:\Program Files\Roomler\roomlerd.exe");
        assert_eq!(back[1], PREP_SUBCOMMAND);
        assert_eq!(back[4], "", "no memory is an empty argument");
        let parsed = PrepArgs::parse(back.into_iter().skip(1).map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(parsed, a);

        let with_memory = PrepArgs {
            memory_dir: Some(PathBuf::from(r"C:\ProgramData\Roomler\hive\run\s1\memory")),
            ..a
        };
        let back = argv(
            &with_memory
                .command_line(Path::new(r"C:\r\roomlerd.exe"))
                .unwrap(),
        );
        let parsed = PrepArgs::parse(back.into_iter().skip(1).map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(parsed, with_memory);
    }

    #[test]
    fn only_hive_prep_with_four_arguments_is_a_preparation() {
        let p = |a: &[&str]| PrepArgs::parse(os(a).into_iter());
        assert!(p(&[]).is_none());
        assert!(p(&["run"]).is_none());
        assert!(p(&["hive-mcp", "x"]).is_none());
        assert!(p(&["hive-prep", "a", "b", "c"]).unwrap().is_err());
        assert!(p(&["hive-prep", "a", "b", "c", "d", "e"]).unwrap().is_err());
        assert!(p(&["hive-prep", "a", "b", "c", "d"]).unwrap().is_ok());
    }

    #[test]
    fn prep_makes_the_config_dir_and_copies_memory_only_where_nothing_is() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("state").join("claude");
        let auto = config.join("projects").join("hive-s1").join("memory");
        let folder = root.path().join("work");
        let memory = root.path().join("memory");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("CLAUDE.md"), "org rules").unwrap();
        std::fs::write(memory.join("MEMORY.md"), "org memory").unwrap();
        let args = PrepArgs {
            config_dir: config.clone(),
            folder: folder.clone(),
            memory_dir: Some(memory.clone()),
            auto_memory_dir: auto.clone(),
        };
        prep(&args).unwrap();
        assert_eq!(
            std::fs::read_to_string(config.join("CLAUDE.md")).unwrap(),
            "org rules"
        );
        assert_eq!(
            std::fs::read_to_string(auto.join("MEMORY.md")).unwrap(),
            "org memory"
        );

        // A resume keeps what the session has, its own edits included.
        std::fs::write(config.join("CLAUDE.md"), "edited").unwrap();
        std::fs::remove_file(auto.join("MEMORY.md")).unwrap();
        std::fs::create_dir(auto.join("MEMORY.md")).unwrap();
        std::fs::write(memory.join("CLAUDE.md"), "newer org rules").unwrap();
        prep(&args).unwrap();
        assert_eq!(
            std::fs::read_to_string(config.join("CLAUDE.md")).unwrap(),
            "edited"
        );
        assert!(
            auto.join("MEMORY.md").is_dir(),
            "something there is never replaced"
        );

        // Without memory, nothing is copied.
        let bare = PrepArgs {
            config_dir: root.path().join("other").join("claude"),
            memory_dir: None,
            ..args.clone()
        };
        prep(&bare).unwrap();
        assert!(bare.config_dir.is_dir());
        assert!(!bare.config_dir.join("CLAUDE.md").exists());
    }

    #[test]
    fn prep_refuses_a_folder_that_does_not_open_and_a_relative_path() {
        let root = tempfile::tempdir().unwrap();
        let args = PrepArgs {
            config_dir: root.path().join("claude"),
            folder: root.path().join("missing"),
            memory_dir: None,
            auto_memory_dir: root.path().join("claude").join("memory"),
        };
        let e = prep(&args).unwrap_err();
        assert!(e.contains("does not open"), "{e}");
        let relative = PrepArgs {
            folder: PathBuf::from("work"),
            ..args
        };
        assert!(prep(&relative).unwrap_err().contains("absolute"));
    }

    fn cmd(script: &str) -> String {
        format!("{} /d /c \"{script}\"", program(&system_cmd()).unwrap())
    }

    #[test]
    fn run_prep_is_ok_on_exit_0_and_says_what_a_failure_said() {
        // SAFETY: `Daemon` carries no token.
        unsafe {
            assert_eq!(
                run_prep(SpawnAs::Daemon, &cmd("exit 0"), PREP_TIMEOUT),
                Ok(())
            );
            let e = run_prep(
                SpawnAs::Daemon,
                &cmd("echo roomlerd hive-prep: making X: denied 1>&2 & exit 1"),
                PREP_TIMEOUT,
            )
            .unwrap_err();
            assert_eq!(e, "roomlerd hive-prep: making X: denied");
            let e = run_prep(SpawnAs::Daemon, &cmd("exit 3"), PREP_TIMEOUT).unwrap_err();
            assert!(e.contains("exit 3"), "{e}");
        }
    }

    #[test]
    fn a_preparation_that_hangs_is_ended_at_its_deadline() {
        let started = Instant::now();
        // SAFETY: `Daemon` carries no token.
        let e = unsafe {
            run_prep(
                SpawnAs::Daemon,
                &cmd("ping -n 60 127.0.0.1 >NUL"),
                Duration::from_secs(1),
            )
        }
        .unwrap_err();
        assert!(e.contains("did not finish"), "{e}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "ended, not waited out"
        );
    }

    /// The account read from a token is the one Windows says this process
    /// runs as.
    #[test]
    fn a_tokens_account_is_read_back() {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: our own process's token, closed below.
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
            0
        );
        let got = account_of_token(token);
        // SAFETY: the handle OpenProcessToken gave us, closed once.
        unsafe { CloseHandle(token) };
        let (domain, name) = got.unwrap();
        assert!(!domain.is_empty() && !name.is_empty());
        if let Ok(user) = std::env::var("USERNAME") {
            assert!(
                name.eq_ignore_ascii_case(&user),
                "{name} vs USERNAME={user}"
            );
        }
    }

    /// A test process is not SYSTEM, so it cannot obtain the console user's
    /// token. It must refuse, naming why, and never come back as someone.
    #[test]
    fn without_system_there_is_no_console_user_to_run_as() {
        if crate::win_identity::process_is_local_system() {
            return;
        }
        match console_user() {
            Ok(u) => panic!("a non-SYSTEM process obtained {}", u.qualified()),
            Err((HiveRefusal::NoConsoleUser, _)) => {}
            Err((HiveRefusal::LaunchFailed, detail)) => {
                assert!(detail.contains("SYSTEM"), "{detail}")
            }
            Err((other, detail)) => panic!("unexpected refusal {other:?}: {detail}"),
        }
    }

    #[test]
    fn a_variable_is_found_whatever_its_case() {
        let entries: Vec<Vec<u16>> = [
            r"=C:=C:\x",
            r"AppData=C:\Users\dev\AppData\Roaming",
            "APPDATAX=no",
        ]
        .iter()
        .map(|e| e.encode_utf16().collect())
        .collect();
        assert_eq!(
            env_value(&entries, "APPDATA"),
            Some(OsString::from(r"C:\Users\dev\AppData\Roaming"))
        );
        assert_eq!(env_value(&entries, "TEMP"), None);
    }
}
