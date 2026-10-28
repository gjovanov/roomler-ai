// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-129 B1 — the daemon install on Linux and macOS: the mechanics under
//! [`crate::orchestrator_unix`]. The wizard is a GUI front end to the
//! **embedded** `install.sh` halves (operator decisions 5 and 6), never a
//! Rust re-implementation of them: every row of the spec's "what the
//! terminal path does" table was paid for in the field, and two
//! implementations drift until a field report says so.
//!
//! What lives here:
//!
//! * the script, `include_str!`'d from `scripts/install.sh` — the same bytes
//!   the server embeds and serves (`setup_release.rs`), cut with every agent
//!   release (FR-84 S1);
//! * the **argv** of each half — every argument its own element, never a
//!   shell string. On Linux the privileged half is `pkexec /bin/sh <script>
//!   …`; on macOS `osascript` runs a constant AppleScript that takes the
//!   same argv as *its* arguments and quotes each with `quoted form of`, so a
//!   device name like `a'b"c$(x)` reaches the script byte-exact and nothing
//!   is ever interpolated into AppleScript source; the terminal fallback is
//!   the same argv, shell-quoted, behind `sudo`;
//! * **when the half has started** — when something runs as root — and what
//!   an **exit code** means on each side of that line. Before it, the exit is
//!   the prompt tool's own: `pkexec` exits 126 when the person dismissed its
//!   dialog and 127 when it could not authenticate at all (no polkit agent,
//!   `pkexec` absent, the password refused); `osascript` reports a dismissed
//!   prompt as `User canceled. (-128)`. After it, every exit is the script's:
//!   `pkexec` passes the script's status through, so a `set -e` script whose
//!   `dpkg` is missing exits 127 too, and that is a failed step, never "no
//!   prompt possible". A dismissed dialog reads as *cancelled*, never as a
//!   failure that blames the package, and *cancelled* means nothing ran;
//! * the **scratch** directory, 0700, the person's own, holding the script,
//!   the privileged half's output files and — for a system scope — the
//!   config the wizard enrolled in-process (0600; the agent JWT and the
//!   WireGuard key). The config is shredded the moment the privileged half
//!   has placed it; a placement that failed keeps it, so a Retry reuses the
//!   enrollment instead of a burnt single-use token (the B0 property), and
//!   the wizard's exit shreds whatever is left. The script file is the
//!   person's own: a user-level attacker who could swap it (or anything else
//!   in the directory) could as well call `pkexec` themselves, and gains
//!   nothing from the swap — so the directory is 0700 and not guarded
//!   further;
//! * the **runner**. The privileged half writes to FILES in the scratch
//!   directory, never to pipes the wizard holds: a root `dpkg -i` must not be
//!   killed by SIGPIPE halfway through because the person closed the window,
//!   and `osascript`'s `do shell script` returns nothing until the command
//!   has finished, so a file is also the only way its lines stream at all.
//!   The runner tails the files into the progress channel and watches the
//!   cancel flag. **Before the half has started** a cancel kills the prompt
//!   client (`pkexec`, `osascript`, still the person's processes) and the
//!   dialog goes away. **After it** nothing the person's process sends can
//!   stop a root process, and killing `osascript` would only stop the wizard
//!   from seeing the end of one that keeps running (`installer -pkg` would
//!   carry on while the wizard said "nothing was installed", and a Retry
//!   could start a second, concurrent install) — so the cancel is reported
//!   and the half waited out. The started signal is the half's own output:
//!   for `pkexec`, its first stdout line (the script prints a banner before
//!   anything else; `pkexec` itself writes only stderr) or a kill refused
//!   with EPERM (`pkexec` already switched to root); for `osascript`, the
//!   output file the root shell creates as its first act.

use std::ffi::{OsStr, OsString};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::ipc::Channel;
use wizard_shared::progress::{ProgressEvent, replay_log};

use crate::role::{HostOs, Scope};

/// `scripts/install.sh`, verbatim. The server embeds the same file
/// (`crates/modules/fleet/src/setup_release.rs`), and both are cut with the
/// agent release, so the wizard runs exactly the script a terminal would.
pub const INSTALL_SH: &str = include_str!("../../../scripts/install.sh");

/// The script's name in the scratch directory — the terminal fallback shows
/// it, so it reads as what it is.
pub const SCRIPT_NAME: &str = "install.sh";

/// The system-scope config's name in the scratch directory.
pub const CONFIG_FROM_NAME: &str = "config.toml";

/// The root daemon's config on Linux (the packaged `roomlerd.service`'s
/// `--config`) and the macOS root half's (`com.roomler.daemon.plist`).
pub const SYSTEM_CONFIG: &str = "/etc/roomler/config.toml";

/// How long the privileged half may take, the prompt included: a person has
/// to answer it, and the companion's package pull takes minutes on a slow
/// line.
pub const PRIVILEGED_BUDGET: Duration = Duration::from_secs(30 * 60);

/// How long the user half may take.
pub const USER_BUDGET: Duration = Duration::from_secs(5 * 60);

/// How long the wizard waits for the person to run the terminal command.
pub const TERMINAL_BUDGET: Duration = Duration::from_secs(30 * 60);

/// How often the runner reads the half's output and the cancel flag.
pub const POLL: Duration = Duration::from_millis(200);

/// macOS: after a cancel killed `osascript` before the half was seen to
/// start, how long to watch for a root shell it had started anyway — the
/// password accepted in the same instant as the cancel. The shell's first act
/// is to create its output file, milliseconds after the prompt closes.
pub const LATE_START_GRACE: Duration = Duration::from_secs(5);

/// Which half of the script runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Half {
    Privileged,
    User,
}

impl Half {
    /// The SPA's spelling (`ProgressEvent::ScriptHalfStarted`).
    pub fn wire(self) -> &'static str {
        match self {
            Half::Privileged => "privileged",
            Half::User => "user",
        }
    }
}

/// What runs a half.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// Linux: polkit's prompt, through the session's authentication agent.
    Pkexec,
    /// macOS: `do shell script … with administrator privileges`.
    Osascript,
    /// The person, in a terminal, with `sudo`.
    Terminal,
    /// The user half, as the person.
    Sh,
}

impl Tool {
    pub fn wire(self) -> &'static str {
        match self {
            Tool::Pkexec => "pkexec",
            Tool::Osascript => "osascript",
            Tool::Terminal => "terminal",
            Tool::Sh => "sh",
        }
    }

    /// The program the runner spawns.
    pub fn program(self) -> &'static str {
        match self {
            Tool::Pkexec => "pkexec",
            Tool::Osascript => "osascript",
            Tool::Terminal => "sudo",
            Tool::Sh => "/bin/sh",
        }
    }
}

/// The desktop's own administrator prompt on this OS; `None` where the
/// daemon roles do not go through this module (Windows: the MSI's UAC).
pub fn elevation_tool(os: HostOs) -> Option<Tool> {
    match os {
        HostOs::Linux => Some(Tool::Pkexec),
        HostOs::Macos => Some(Tool::Osascript),
        HostOs::Windows | HostOs::Other => None,
    }
}

// ─── paths ──────────────────────────────────────────────────────────────────

/// Where the package puts the daemon: the `.deb` / tarball's `/usr/bin`, the
/// `.pkg`'s frozen bundle (its name keys the macOS TCC grants — FR-21 D5).
pub fn daemon_binary(os: HostOs) -> PathBuf {
    match os {
        HostOs::Macos => PathBuf::from("/Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd"),
        _ => PathBuf::from("/usr/bin/roomlerd"),
    }
}

/// The `roomler` CLI a daemon package carries (role → action composition):
/// the Linux packages put it beside the daemon; the macOS `.pkg` keeps it
/// inside the bundle and on no PATH, so nothing is promised there.
pub fn cli_binary(os: HostOs) -> Option<PathBuf> {
    match os {
        HostOs::Linux => Some(PathBuf::from("/usr/bin/roomler")),
        _ => None,
    }
}

/// Is the desktop companion where its package places it?
pub fn companion_present(os: HostOs) -> bool {
    match os {
        HostOs::Linux => Path::new("/usr/bin/roomler-desktop").is_file(),
        HostOs::Macos => Path::new("/Applications/Roomler.app").is_dir(),
        _ => false,
    }
}

pub fn system_config_path() -> PathBuf {
    PathBuf::from(SYSTEM_CONFIG)
}

/// The per-user daemon's config: the user unit's `roomlerd run` and the
/// LaunchAgent's `roomlerd run` both resolve it from the person's profile.
pub fn per_user_config_path() -> Result<PathBuf, String> {
    roomlerd::config::default_config_path().map_err(|e| format!("resolve config path: {e}"))
}

/// `roomlerd --version` → `0.4.181`, or nothing.
pub fn installed_version(binary: &Path) -> Option<String> {
    let out = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    let version = line.rsplit(' ').next()?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

/// What this host has of a daemon install, for the Welcome page's summary
/// and the preflight. Existence only: the system config is root's and
/// unreadable to the person, which is the whole reason the privileged half
/// merges it (#2338).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnixDetect {
    pub daemon_installed: bool,
    pub daemon_version: Option<String>,
    /// The person's own config exists: a per-user daemon was enrolled here.
    pub per_user_config: bool,
    /// `/etc/roomler/config.toml` exists: a system (root) daemon was enrolled.
    pub system_config: bool,
}

pub fn detect(os: HostOs) -> UnixDetect {
    let binary = daemon_binary(os);
    let daemon_installed = binary.is_file();
    UnixDetect {
        daemon_installed,
        daemon_version: daemon_installed
            .then(|| installed_version(&binary))
            .flatten(),
        per_user_config: per_user_config_path().map(|p| p.is_file()).unwrap_or(false),
        system_config: system_config_path().is_file(),
    }
}

// ─── argv ───────────────────────────────────────────────────────────────────

/// The privileged half's inputs.
pub struct PrivilegedSpec<'a> {
    pub script: &'a Path,
    pub server: &'a str,
    pub device_name: &'a str,
    pub scope: Scope,
    /// The system-scope config the wizard enrolled in-process.
    pub config_from: Option<&'a Path>,
    /// The config keys the person set by hand, for `--config-take`.
    pub take: &'a [String],
}

/// `/bin/sh <script> --privileged-steps --role daemon --server … --name …
/// --desktop [--system --config-from … [--config-take …]]`, one element per
/// argument. `--desktop` always: `pkexec` strips `DISPLAY`, so the half
/// cannot see the session this wizard is running in, and the companion is
/// the device's consent prompt surface.
pub fn privileged_args(spec: &PrivilegedSpec<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        OsString::from("/bin/sh"),
        spec.script.as_os_str().to_os_string(),
        OsString::from("--privileged-steps"),
        OsString::from("--role"),
        OsString::from("daemon"),
        OsString::from("--server"),
        OsString::from(spec.server),
        OsString::from("--name"),
        OsString::from(spec.device_name),
        OsString::from("--desktop"),
    ];
    if spec.scope == Scope::System {
        args.push(OsString::from("--system"));
    }
    if let Some(from) = spec.config_from {
        args.push(OsString::from("--config-from"));
        args.push(from.as_os_str().to_os_string());
        if !spec.take.is_empty() {
            args.push(OsString::from("--config-take"));
            args.push(OsString::from(spec.take.join(",")));
        }
    }
    args
}

/// `/bin/sh <script> --user-steps --role daemon [--system]` — as the person.
pub fn user_args(script: &Path, scope: Scope) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        OsString::from("/bin/sh"),
        script.as_os_str().to_os_string(),
        OsString::from("--user-steps"),
        OsString::from("--role"),
        OsString::from("daemon"),
    ];
    if scope == Scope::System {
        args.push(OsString::from("--system"));
    }
    args
}

/// Where the privileged half's output goes: three files in the scratch
/// directory. `out` and `err` are the script's stdout and stderr, tailed into
/// the progress channel; `status` is its exit status, which the macOS half
/// writes itself (a root shell outlives an `osascript` that was killed in the
/// instant the password was accepted, and only the file then says how it
/// ended).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HalfFiles {
    pub out: PathBuf,
    pub err: PathBuf,
    pub status: PathBuf,
}

impl HalfFiles {
    pub fn in_dir(dir: &Path) -> HalfFiles {
        HalfFiles {
            out: dir.join("privileged.out"),
            err: dir.join("privileged.err"),
            status: dir.join("privileged.status"),
        }
    }

    /// Remove what an earlier attempt left in the same directory (a Retry
    /// reuses a kept scratch): a stale output file would read as "started".
    pub fn clear(&self) {
        for p in [&self.out, &self.err, &self.status] {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The AppleScript the macOS half runs — a constant. It takes three paths
/// and the half's argv as *its* arguments and quotes each with `quoted form
/// of`, so the device name and the server URL are never part of the source:
/// `osascript` receives them as plain argv, after `--`. The root shell sends
/// the half's stdout and stderr to the two files (`umask 022`: the person's
/// wizard reads them, inside its own 0700 directory) and writes the exit
/// status to the third, then exits with it, so `osascript` reports `(N)` too.
pub const ADMIN_APPLESCRIPT: &[&str] = &[
    "on run argv",
    "set outPath to item 1 of argv",
    "set errPath to item 2 of argv",
    "set statusPath to item 3 of argv",
    "set cmd to \"\"",
    "repeat with i from 4 to (count of argv)",
    "set cmd to cmd & \" \" & quoted form of ((item i of argv) as text)",
    "end repeat",
    "do shell script \"umask 022; (\" & cmd & \") >\" & quoted form of outPath & \" 2>\" & quoted form of errPath & \"; rc=$?; echo $rc >\" & quoted form of statusPath & \"; exit $rc\" with administrator privileges",
    "end run",
];

/// `osascript -e <line>… -- <out> <err> <status> <args…>`.
pub fn osascript_args(files: &HalfFiles, args: &[OsString]) -> Vec<OsString> {
    let mut out: Vec<OsString> = Vec::with_capacity(ADMIN_APPLESCRIPT.len() * 2 + 4 + args.len());
    for line in ADMIN_APPLESCRIPT {
        out.push(OsString::from("-e"));
        out.push(OsString::from(line));
    }
    out.push(OsString::from("--"));
    out.push(files.out.as_os_str().to_os_string());
    out.push(files.err.as_os_str().to_os_string());
    out.push(files.status.as_os_str().to_os_string());
    out.extend(args.iter().cloned());
    out
}

/// One word, safe inside a POSIX shell: single-quoted, with each `'` as
/// `'\''`. A word that needs no quoting is left bare for readability.
pub fn sh_quote(word: &OsStr) -> String {
    let s = word.to_string_lossy();
    let plain = !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':' | b'=' | b',')
        });
    if plain {
        return s.into_owned();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The privileged half as a terminal command — for a host with no prompt
/// to show. It carries no secret: a half never takes a token, and the config
/// file it names is readable by the person who runs this.
pub fn terminal_command(args: &[OsString]) -> String {
    let words: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
    format!("sudo {}", words.join(" "))
}

/// How the runner knows the half's script has started: that something runs,
/// or ran, as root (F1, F2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartSignal {
    /// The child's stdout is the script's own: `pkexec` execs the script in
    /// place and writes nothing to stdout itself, and the script prints its
    /// banner before anything else. Its first output is the start.
    FirstStdout,
    /// The root shell creates the output file as its first act (the macOS
    /// half's redirection); the file appearing is the start. `osascript`'s
    /// own stdout and stderr are its result and its error, nothing more.
    OutputFile,
}

/// A prompt tool's command line, as the runner spawns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub signal: StartSignal,
}

/// The privileged half under the prompt tool: `pkexec <half argv>`, or
/// `osascript <the constant script> -- <files> <half argv>`.
pub fn prompt_command(tool: Tool, files: &HalfFiles, half_args: &[OsString]) -> PromptCommand {
    match tool {
        Tool::Osascript => PromptCommand {
            program: OsString::from(Tool::Osascript.program()),
            args: osascript_args(files, half_args),
            signal: StartSignal::OutputFile,
        },
        _ => PromptCommand {
            program: OsString::from(tool.program()),
            args: half_args.to_vec(),
            signal: StartSignal::FirstStdout,
        },
    }
}

// ─── exit codes ─────────────────────────────────────────────────────────────

/// What a half's exit meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HalfOutcome {
    /// The script ran and exited with this status (`0` = done).
    Exited(i32),
    /// The person dismissed the administrator prompt, or the wizard's cancel
    /// closed it before the half started: NOTHING ran as root.
    Cancelled,
    /// No prompt could be shown, or it refused: the reason, for the person.
    Unavailable(String),
}

/// What a half's process did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HalfRun {
    /// The exit status: the script's once it has started (for the macOS half
    /// read from its status file), else the prompt tool's.
    pub status: Option<i32>,
    /// The last lines of stderr, for the exit's meaning; the prompt tool's
    /// own last line is the last one.
    pub stderr_tail: String,
    /// The wizard's cancel closed the prompt before the half started.
    pub killed: bool,
    /// The half's script started: something runs, or ran, as root.
    pub started: bool,
    /// A cancel came after the start: the half was waited out, not stopped.
    pub cancelled_late: bool,
}

/// The exit's meaning. Once the half has started every status is the
/// script's own — `pkexec` passes it through, 126 and 127 included (F2) — and
/// a kill can only have come before the start, so *cancelled* means nothing
/// ran (F1). Before the start: `pkexec`'s 126 is the person's No and 127 is
/// "no authentication possible"; `osascript`'s `(-128)` is the person's
/// Cancel.
pub fn classify(tool: Tool, run: &HalfRun) -> HalfOutcome {
    if run.started {
        return HalfOutcome::Exited(run.status.unwrap_or(-1));
    }
    if run.killed {
        return HalfOutcome::Cancelled;
    }
    match tool {
        Tool::Pkexec => match run.status {
            Some(0) => HalfOutcome::Exited(0),
            Some(126) => HalfOutcome::Cancelled,
            Some(127) => HalfOutcome::Unavailable(pkexec_reason(&run.stderr_tail)),
            Some(n) => HalfOutcome::Exited(n),
            None => HalfOutcome::Exited(-1),
        },
        Tool::Osascript => match run.status {
            Some(0) => HalfOutcome::Exited(0),
            Some(_) | None => match applescript_error_code(&run.stderr_tail) {
                Some(-128) => HalfOutcome::Cancelled,
                Some(n) if n > 0 => HalfOutcome::Exited(n),
                _ => HalfOutcome::Unavailable(format!(
                    "the administrator prompt could not run the install: {}",
                    last_line(&run.stderr_tail)
                )),
            },
        },
        Tool::Terminal | Tool::Sh => HalfOutcome::Exited(run.status.unwrap_or(-1)),
    }
}

/// Why `pkexec` exited 127, from what it printed.
fn pkexec_reason(stderr_tail: &str) -> String {
    let lower = stderr_tail.to_ascii_lowercase();
    if lower.contains("not authorized")
        || lower.contains("authentication") && lower.contains("fail")
    {
        "the password was not accepted, or this account may not administer the computer".to_string()
    } else if lower.contains("no session") || lower.contains("agent") || lower.contains("authority")
    {
        "no password prompt could be shown here (no polkit authentication agent in this session)"
            .to_string()
    } else {
        format!("pkexec could not authenticate: {}", last_line(stderr_tail))
    }
}

/// The `(N)` AppleScript puts at the end of an `execution error:` line.
fn applescript_error_code(stderr_tail: &str) -> Option<i32> {
    let line = last_line(stderr_tail);
    let trimmed = line.trim_end();
    let open = trimmed.rfind('(')?;
    let inner = trimmed[open + 1..].strip_suffix(')')?;
    inner.trim().parse::<i32>().ok()
}

fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)")
}

/// A spawn that did not happen, as a reason for the person.
pub fn spawn_failure(tool: Tool, err: &std::io::Error) -> String {
    if err.kind() == std::io::ErrorKind::NotFound {
        format!("{} is not installed on this computer", tool.program())
    } else {
        format!("{} could not be started: {err}", tool.program())
    }
}

// ─── the scratch directory ──────────────────────────────────────────────────

/// A 0700 directory of the person's own, holding the script, the privileged
/// half's output files and — for a system scope — the enrolled config the
/// privileged half places. Dropping it shreds the config and removes the
/// directory.
#[derive(Debug)]
pub struct Scratch {
    dir: tempfile::TempDir,
    pub script: PathBuf,
}

impl Scratch {
    /// Create the directory and write the script into it.
    pub fn create() -> Result<Scratch, String> {
        let dir = tempfile::Builder::new()
            .prefix("roomler-setup-")
            .tempdir()
            .map_err(|e| format!("could not create a private temporary directory: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(dir.path())
                .map_err(|e| format!("stat {}: {e}", dir.path().display()))?
                .permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(dir.path(), perms)
                .map_err(|e| format!("chmod {}: {e}", dir.path().display()))?;
        }
        let script = dir.path().join(SCRIPT_NAME);
        write_private(&script, INSTALL_SH.as_bytes())
            .map_err(|e| format!("could not write {}: {e}", script.display()))?;
        Ok(Scratch { dir, script })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Where the system-scope config goes.
    pub fn config_from(&self) -> PathBuf {
        self.dir.path().join(CONFIG_FROM_NAME)
    }

    /// Where the privileged half's output goes.
    pub fn half_files(&self) -> HalfFiles {
        HalfFiles::in_dir(self.dir.path())
    }

    /// Overwrite the config with zeros and remove it. Best-effort, and a
    /// no-op when there is none.
    pub fn shred_config(&self) {
        shred(&self.config_from());
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.shred_config();
    }
}

/// Create a file 0600 (Unix) with these bytes, refusing to follow an
/// existing path.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut f = create_private(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Create an empty file 0600 (Unix), refusing an existing path.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Overwrite a file's bytes with zeros, flush, remove.
fn shred(path: &Path) {
    use std::io::Write as _;
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.is_file()
        && let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(path)
    {
        let zeros = vec![0u8; meta.len().min(1 << 20) as usize];
        let mut left = meta.len();
        while left > 0 {
            let n = left.min(zeros.len() as u64) as usize;
            if f.write_all(&zeros[..n]).is_err() {
                break;
            }
            left -= n as u64;
        }
        let _ = f.sync_all();
    }
    let _ = std::fs::remove_file(path);
}

// ─── the enrollment kept from an attempt that stopped ───────────────────────

/// What an attempt that did not reach the end leaves for the next one with
/// the same server, name and scope, so a Retry never posts a single-use token
/// that is already spent (the B0 property, "a package placement that fails
/// leaves an enrolled config to retry with") and never asks for the
/// administrator password again for a half that already ran.
#[derive(Debug)]
pub struct KeptEnrollment {
    pub scope: Scope,
    pub server: String,
    pub device_name: String,
    /// The device the server now holds for this machine, `(agent, tenant)`,
    /// when the attempt enrolled.
    pub ids: Option<(String, String)>,
    pub config_path: PathBuf,
    /// System scope, the privileged half NOT run to its end: the enrolled
    /// config, still in the scratch directory, for the half to place.
    pub scratch: Option<Scratch>,
    /// The privileged half ran to its end: the package is installed, and for
    /// a system scope the enrolled config is placed and its unit running.
    pub privileged_done: bool,
}

static KEPT: Mutex<Option<KeptEnrollment>> = Mutex::new(None);

/// Keep an enrollment for the next attempt. Replaces whatever was kept.
pub fn keep(kept: KeptEnrollment) {
    let mut g = KEPT.lock().unwrap_or_else(|p| p.into_inner());
    *g = Some(kept);
}

/// Take the kept enrollment, if it is for this scope, server and name —
/// anything else is dropped (and its config shredded), because a different
/// server or name needs a new enrollment.
pub fn take_kept(scope: Scope, server: &str, device_name: &str) -> Option<KeptEnrollment> {
    let mut g = KEPT.lock().unwrap_or_else(|p| p.into_inner());
    let kept = g.take()?;
    if kept.scope == scope && kept.server == server && kept.device_name == device_name {
        Some(kept)
    } else {
        tracing::info!("a kept enrollment is for another server, name or scope; dropping it");
        None
    }
}

/// Drop whatever is kept: the wizard's exit, a success.
pub fn clear_kept() {
    let mut g = KEPT.lock().unwrap_or_else(|p| p.into_inner());
    *g = None;
}

// ─── while the privileged half runs ─────────────────────────────────────────

static PRIVILEGED_RUNNING: AtomicBool = AtomicBool::new(false);

/// The privileged half (or the terminal step that stands in for it) is
/// running. The window refuses to close meanwhile: the wizard's exit would
/// remove the scratch directory — the script a terminal step has yet to run,
/// the config a root half has yet to place — from under it.
pub fn privileged_running() -> bool {
    PRIVILEGED_RUNNING.load(Ordering::SeqCst)
}

/// Marks [`privileged_running`] for as long as it lives.
pub struct PrivilegedRunning(());

impl PrivilegedRunning {
    pub fn begin() -> PrivilegedRunning {
        PRIVILEGED_RUNNING.store(true, Ordering::SeqCst);
        PrivilegedRunning(())
    }
}

impl Drop for PrivilegedRunning {
    fn drop(&mut self) {
        PRIVILEGED_RUNNING.store(false, Ordering::SeqCst);
    }
}

// ─── the terminal fallback's "I ran it" ─────────────────────────────────────

static CONFIRM: Mutex<Option<tokio::sync::oneshot::Sender<()>>> = Mutex::new(None);

/// Arm the wait: the receiver completes when the SPA says the command ran.
pub fn confirm_arm() -> tokio::sync::oneshot::Receiver<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut g = CONFIRM.lock().unwrap_or_else(|p| p.into_inner());
    *g = Some(tx);
    rx
}

/// The SPA's "I've run it": wakes the waiting pipeline. Refused when
/// nothing waits.
pub fn confirm_fire() -> Result<(), String> {
    let mut g = CONFIRM.lock().unwrap_or_else(|p| p.into_inner());
    match g.take() {
        Some(tx) => {
            let _ = tx.send(());
            Ok(())
        }
        None => Err("no install is waiting for the terminal step".to_string()),
    }
}

// ─── the runners ────────────────────────────────────────────────────────────

/// Lines from a file another process appends to.
struct Tail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl Tail {
    fn new(path: &Path) -> Tail {
        Tail {
            path: path.to_path_buf(),
            offset: 0,
            partial: Vec::new(),
        }
    }

    /// The complete lines written since the last call; a file that does not
    /// exist yet reads as nothing.
    fn poll(&mut self) -> Vec<String> {
        let Ok(mut f) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(self.offset)).is_err() || f.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        self.offset += buf.len() as u64;
        self.partial.extend_from_slice(&buf);
        let mut lines = Vec::new();
        while let Some(pos) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=pos).collect();
            lines.push(String::from_utf8_lossy(&line).trim_end().to_string());
        }
        lines
    }

    /// The rest, the last unterminated line included: the writer is done.
    fn finish(&mut self) -> Vec<String> {
        let mut lines = self.poll();
        if !self.partial.is_empty() {
            lines.push(
                String::from_utf8_lossy(&self.partial)
                    .trim_end()
                    .to_string(),
            );
            self.partial.clear();
        }
        lines
    }

    /// Has anything been written at all?
    fn has_output(&self) -> bool {
        self.offset > 0
    }
}

/// The half's lines so far into the progress channel; stderr's into the tail
/// that gives the exit its meaning too.
fn drain(
    out: &mut Tail,
    err: &mut Tail,
    err_tail: &mut Vec<String>,
    channel: &Channel<ProgressEvent>,
    half: Half,
    finish: bool,
) {
    let (o, e) = if finish {
        (out.finish(), err.finish())
    } else {
        (out.poll(), err.poll())
    };
    for line in o {
        emit_line(channel, half, line);
    }
    for line in e {
        push_tail(err_tail, line.clone());
        emit_line(channel, half, line);
    }
}

fn push_tail(tail: &mut Vec<String>, line: String) {
    if tail.len() >= 40 {
        tail.remove(0);
    }
    tail.push(line);
}

/// The macOS half's own record of how it ended.
fn read_status(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Run the privileged half under its prompt tool (F1, F2). The half writes to
/// the scratch directory's files, which are tailed into the progress channel
/// as `ScriptLine`; `cancel` is watched. Before the half has started a cancel
/// kills the prompt client and the dialog goes away; after it the cancel is
/// reported and the half waited out (`cancelled_late`), because a root
/// process cannot be stopped from here and pretending it was is how a Retry
/// starts a second install beside the first. Over budget, the same: the run
/// is reported, never abandoned.
pub async fn run_privileged_half(
    cmd: &PromptCommand,
    files: &HalfFiles,
    on_event: &Channel<ProgressEvent>,
    budget: Duration,
    cancel: &AtomicBool,
) -> std::io::Result<HalfRun> {
    use tokio::io::AsyncReadExt as _;
    let half = Half::Privileged;
    files.clear();
    let mut command = tokio::process::Command::new(&cmd.program);
    command.args(&cmd.args).stdin(std::process::Stdio::null());
    match cmd.signal {
        StartSignal::FirstStdout => {
            command.stdout(std::process::Stdio::from(create_private(&files.out)?));
            command.stderr(std::process::Stdio::from(create_private(&files.err)?));
        }
        StartSignal::OutputFile => {
            command
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
        }
    }
    let mut child = command.spawn()?;
    // `osascript`'s own words — its result (nothing: the half's output goes to
    // the files) and its execution error — read whole, off the poll loop.
    let own_out = child.stdout.take().map(|mut o| {
        tokio::spawn(async move {
            let mut b = Vec::new();
            let _ = o.read_to_end(&mut b).await;
            b
        })
    });
    let own_err = child.stderr.take().map(|mut e| {
        tokio::spawn(async move {
            let mut b = Vec::new();
            let _ = e.read_to_end(&mut b).await;
            b
        })
    });

    let mut out = Tail::new(&files.out);
    let mut err = Tail::new(&files.err);
    let mut err_tail: Vec<String> = Vec::new();
    let mut run = HalfRun::default();
    let begun = Instant::now();
    let mut cancel_seen = false;
    let mut over_budget = false;
    let started_now = |out: &Tail| match cmd.signal {
        StartSignal::FirstStdout => out.has_output(),
        StartSignal::OutputFile => files.out.exists(),
    };

    let exit = loop {
        drain(&mut out, &mut err, &mut err_tail, on_event, half, false);
        if !run.started && started_now(&out) {
            run.started = true;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if !cancel_seen && cancel.load(Ordering::SeqCst) {
            cancel_seen = true;
            // Look once more right before deciding: a first line written
            // since the read above is a start, and a started half is never
            // killed.
            drain(&mut out, &mut err, &mut err_tail, on_event, half, false);
            if !run.started && started_now(&out) {
                run.started = true;
            }
            if run.started {
                run.cancelled_late = true;
                emit_line(
                    on_event,
                    half,
                    "cancel: the install steps already run as administrator and cannot be stopped from here; waiting for them to finish, then the install stops".to_string(),
                );
            } else {
                match child.start_kill() {
                    Ok(()) => {
                        run.killed = true;
                        emit_line(
                            on_event,
                            half,
                            "cancel: closing the administrator prompt".to_string(),
                        );
                    }
                    // `pkexec` switched to root the moment the password was
                    // accepted: the script runs, whatever it has printed yet.
                    Err(e) => {
                        run.started = true;
                        run.cancelled_late = true;
                        emit_line(
                            on_event,
                            half,
                            format!(
                                "cancel: the install steps already run as administrator and cannot be stopped from here ({e}); waiting for them to finish, then the install stops"
                            ),
                        );
                    }
                }
            }
        }
        if !over_budget && begun.elapsed() > budget {
            over_budget = true;
            emit_line(
                on_event,
                half,
                format!(
                    "this step has taken more than {} minutes; still waiting for it",
                    budget.as_secs() / 60
                ),
            );
        }
        tokio::time::sleep(POLL).await;
    };

    // The macOS late start: the kill closed `osascript`, but a root shell it
    // had already started — the password accepted in the same instant — runs
    // on. Its output file appearing says so; its status file says how it
    // ended. Until then the cancel is not a cancel.
    if run.killed && cmd.signal == StartSignal::OutputFile {
        let grace = Instant::now() + LATE_START_GRACE;
        while Instant::now() < grace {
            if files.out.exists() {
                run.started = true;
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        if run.started {
            run.killed = false;
            run.cancelled_late = true;
            emit_line(
                on_event,
                half,
                "the password had already been accepted: the install steps run as administrator and cannot be stopped from here; waiting for them to finish, then the install stops".to_string(),
            );
            let until = Instant::now() + budget;
            while !files.status.exists() && Instant::now() < until {
                drain(&mut out, &mut err, &mut err_tail, on_event, half, false);
                tokio::time::sleep(POLL).await;
            }
        }
    }

    drain(&mut out, &mut err, &mut err_tail, on_event, half, true);
    // A first stdout line read only now still means the script ran: never
    // call that a cancel.
    if cmd.signal == StartSignal::FirstStdout && out.has_output() && !run.started {
        run.started = true;
        if run.killed {
            run.killed = false;
            run.cancelled_late = true;
        }
    }

    // Bounded, like the user half's readers: whatever holds the pipes past
    // the client's exit is not worth waiting on.
    let own_err_text = match own_err {
        Some(task) => match tokio::time::timeout(Duration::from_secs(5), task).await {
            Ok(Ok(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
            _ => String::new(),
        },
        None => String::new(),
    };
    if let Some(task) = own_out {
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    for line in own_err_text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
    {
        push_tail(&mut err_tail, line.to_string());
        emit_line(on_event, half, line.to_string());
    }

    run.status = match cmd.signal {
        StartSignal::FirstStdout => exit.code(),
        StartSignal::OutputFile if run.started => {
            read_status(&files.status).or(match exit.code() {
                Some(0) => Some(0),
                _ => applescript_error_code(&own_err_text).filter(|n| *n > 0),
            })
        }
        StartSignal::OutputFile => exit.code(),
    };
    run.stderr_tail = err_tail.join("\n");
    Ok(run)
}

/// Spawn the user half (`program args…`, as the person), stream every stdout
/// and stderr line into the progress channel as `ScriptLine`, watch `cancel`,
/// and wait for the exit within `budget`. It is the person's own process, so
/// a cancel stops it outright.
pub async fn run_half(
    program: &str,
    args: &[OsString],
    half: Half,
    on_event: &Channel<ProgressEvent>,
    budget: Duration,
    cancel: &AtomicBool,
) -> std::io::Result<HalfRun> {
    use tokio::io::AsyncBufReadExt as _;

    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tail: std::sync::Arc<Mutex<Vec<String>>> = Default::default();

    let mut readers = Vec::new();
    if let Some(out) = stdout {
        let channel = on_event.clone();
        readers.push(tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(out).split(b'\n');
            while let Ok(Some(raw)) = lines.next_segment().await {
                let line = String::from_utf8_lossy(&raw).trim_end().to_string();
                emit_line(&channel, half, line);
            }
        }));
    }
    if let Some(err) = stderr {
        let channel = on_event.clone();
        let tail = tail.clone();
        readers.push(tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(err).split(b'\n');
            while let Ok(Some(raw)) = lines.next_segment().await {
                let line = String::from_utf8_lossy(&raw).trim_end().to_string();
                {
                    let mut t = tail.lock().unwrap_or_else(|p| p.into_inner());
                    push_tail(&mut t, line.clone());
                }
                emit_line(&channel, half, line);
            }
        }));
    }

    let begun = Instant::now();
    let mut run = HalfRun::default();
    let mut cancel_seen = false;
    let mut over_budget = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if !cancel_seen && cancel.load(Ordering::SeqCst) {
            cancel_seen = true;
            if child.start_kill().is_ok() {
                run.killed = true;
                emit_line(on_event, half, "cancel: stopping the step".to_string());
            }
        }
        if !over_budget && begun.elapsed() > budget {
            over_budget = true;
            emit_line(
                on_event,
                half,
                format!(
                    "this step has taken more than {} minutes; still waiting for it",
                    budget.as_secs() / 60
                ),
            );
        }
        tokio::time::sleep(POLL).await;
    };
    // Bounded: a process the step left behind could hold the pipes open.
    for r in readers {
        let _ = tokio::time::timeout(Duration::from_secs(5), r).await;
    }
    run.status = status.code();
    run.stderr_tail = tail.lock().unwrap_or_else(|p| p.into_inner()).join("\n");
    Ok(run)
}

fn emit_line(channel: &Channel<ProgressEvent>, half: Half, line: String) {
    if line.is_empty() {
        return;
    }
    let event = ProgressEvent::ScriptLine {
        half: half.wire().to_string(),
        line,
    };
    replay_log().push(event.clone());
    let _ = channel.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTILE: &str = "a'b\"c$(x) `d` ;e|f & \\g\n";

    fn spec<'a>(
        script: &'a Path,
        scope: Scope,
        config_from: Option<&'a Path>,
        take: &'a [String],
    ) -> PrivilegedSpec<'a> {
        PrivilegedSpec {
            script,
            server: "https://example.invalid",
            device_name: HOSTILE,
            scope,
            config_from,
            take,
        }
    }

    /// The embedded script is the real one: the shebang, the halves, and the
    /// server needle the server rewrites (which the wizard never relies on —
    /// it passes `--server`).
    #[test]
    fn the_embedded_script_is_install_sh() {
        assert!(INSTALL_SH.starts_with("#!/bin/sh"));
        for flag in [
            "--privileged-steps",
            "--user-steps",
            "--config-from",
            "--config-take",
            "--desktop",
        ] {
            assert!(INSTALL_SH.contains(flag), "{flag}");
        }
        assert!(INSTALL_SH.contains("adopt-enrollment"), "the #2338 merge");
        // F1 — the started signal: the main block prints the banner, on
        // stdout, before it dispatches to any step (the functions that need
        // root are defined above it, and run only from that dispatch).
        let banner = INSTALL_SH
            .find("say \"roomler install.sh — role=$ROLE")
            .expect("the banner line");
        for dispatch in [
            "daemon/Linux)  install_daemon_linux",
            "daemon/Darwin) install_daemon_macos",
            "Linux)  user_steps_linux",
            "Darwin) user_steps_macos",
        ] {
            let at = INSTALL_SH.find(dispatch).expect(dispatch);
            assert!(banner < at, "the banner precedes {dispatch}");
            assert_eq!(
                INSTALL_SH.matches(dispatch).count(),
                1,
                "one call site: {dispatch}"
            );
        }
    }

    /// Every argument is its own element, the hostile name among them,
    /// byte-exact; the system scope adds `--system --config-from` and the
    /// taken keys; the per-user scope adds nothing.
    #[test]
    fn privileged_argv_is_one_element_per_argument() {
        let script = Path::new("/tmp/roomler-setup-x/install.sh");
        let take = vec![
            "overlay_enabled".to_string(),
            "auto_grant_session".to_string(),
        ];
        let args = privileged_args(&spec(script, Scope::PerUser, None, &[]));
        let as_str: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            as_str,
            vec![
                "/bin/sh",
                "/tmp/roomler-setup-x/install.sh",
                "--privileged-steps",
                "--role",
                "daemon",
                "--server",
                "https://example.invalid",
                "--name",
                HOSTILE,
                "--desktop",
            ]
        );

        let from = Path::new("/tmp/roomler-setup-x/config.toml");
        let args = privileged_args(&spec(script, Scope::System, Some(from), &take));
        let as_str: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            &as_str[10..],
            &[
                "--system",
                "--config-from",
                "/tmp/roomler-setup-x/config.toml",
                "--config-take",
                "overlay_enabled,auto_grant_session",
            ]
        );
        assert_eq!(as_str[8], HOSTILE, "the name is one element, untouched");

        // A system scope with nothing taken names no --config-take.
        let args = privileged_args(&spec(script, Scope::System, Some(from), &[]));
        assert!(!args.iter().any(|a| a == "--config-take"));

        let user = user_args(script, Scope::System);
        let as_str: Vec<&str> = user.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            as_str,
            vec![
                "/bin/sh",
                "/tmp/roomler-setup-x/install.sh",
                "--user-steps",
                "--role",
                "daemon",
                "--system"
            ]
        );
        let user = user_args(script, Scope::PerUser);
        assert!(!user.iter().any(|a| a == "--system"));
    }

    /// The macOS prompt: a constant AppleScript, then `--`, the three output
    /// files, then the half's argv verbatim. Nothing from the argv or the
    /// paths appears in a `-e` line.
    #[test]
    fn osascript_takes_the_argv_as_arguments_never_as_source() {
        let script = Path::new("/tmp/roomler-setup-x/install.sh");
        let files = HalfFiles::in_dir(Path::new("/tmp/roomler-setup-x"));
        let half = privileged_args(&spec(script, Scope::PerUser, None, &[]));
        let cmd = prompt_command(Tool::Osascript, &files, &half);
        assert_eq!(cmd.program, "osascript");
        assert_eq!(cmd.signal, StartSignal::OutputFile);
        let as_str: Vec<&str> = cmd.args.iter().map(|a| a.to_str().unwrap()).collect();
        let dashdash = as_str
            .iter()
            .position(|a| *a == "--")
            .expect("a -- separator");
        // Before `--`: -e/line pairs, the constant script.
        let source: Vec<&str> = as_str[..dashdash]
            .chunks(2)
            .map(|pair| {
                assert_eq!(pair[0], "-e");
                pair[1]
            })
            .collect();
        assert_eq!(source, ADMIN_APPLESCRIPT);
        assert!(
            source.iter().all(|l| !l.contains("example.invalid")
                && !l.contains("a'b")
                && !l.contains("roomler-setup-x")),
            "no argument is interpolated into the AppleScript"
        );
        assert!(
            source
                .iter()
                .any(|l| l.contains("quoted form of") && l.contains("item i of argv")),
            "each argument is quoted with `quoted form of`"
        );
        let shell = source
            .iter()
            .find(|l| l.contains("with administrator privileges"))
            .expect("the prompt is the administrator one");
        for (part, why) in [
            ("quoted form of outPath", "stdout to the out file"),
            ("quoted form of errPath", "stderr to the err file"),
            ("quoted form of statusPath", "the status to its file"),
            ("umask 022", "the person can read what root wrote"),
            ("exit $rc", "osascript reports the script's own status"),
        ] {
            assert!(shell.contains(part), "{why}: {shell}");
        }
        // After `--`: the three files, then the half's argv, byte-exact.
        assert_eq!(
            &as_str[dashdash + 1..dashdash + 4],
            &[
                files.out.to_str().unwrap(),
                files.err.to_str().unwrap(),
                files.status.to_str().unwrap(),
            ]
        );
        assert!(
            files.out.ends_with("privileged.out") && files.status.ends_with("privileged.status")
        );
        let half_str: Vec<&str> = half.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(&as_str[dashdash + 4..], &half_str[..]);
        assert_eq!(as_str[dashdash + 4 + 8], HOSTILE);

        // pkexec takes the half's argv as it is.
        let cmd = prompt_command(Tool::Pkexec, &files, &half);
        assert_eq!(cmd.program, "pkexec");
        assert_eq!(cmd.args, half);
        assert_eq!(cmd.signal, StartSignal::FirstStdout);
    }

    /// The terminal command quotes every word for a POSIX shell.
    #[test]
    fn terminal_command_quotes_for_sh() {
        assert_eq!(
            sh_quote(OsStr::new("--privileged-steps")),
            "--privileged-steps"
        );
        assert_eq!(
            sh_quote(OsStr::new("/tmp/roomler-setup-x/install.sh")),
            "/tmp/roomler-setup-x/install.sh"
        );
        assert_eq!(sh_quote(OsStr::new("a b")), "'a b'");
        assert_eq!(sh_quote(OsStr::new("it's")), "'it'\\''s'");
        assert_eq!(sh_quote(OsStr::new("")), "''");
        let script = Path::new("/tmp/roomler-setup-x/install.sh");
        let cmd = terminal_command(&privileged_args(&spec(script, Scope::PerUser, None, &[])));
        assert!(cmd.starts_with("sudo /bin/sh /tmp/roomler-setup-x/install.sh --privileged-steps --role daemon --server https://example.invalid --name '"), "{cmd}");
        assert!(cmd.ends_with("' --desktop"), "{cmd}");
        assert!(!cmd.contains("token"), "no secret on a command line");
    }

    /// On a real shell, the quoted hostile name comes back byte-exact.
    #[cfg(unix)]
    #[test]
    fn sh_quote_round_trips_through_sh() {
        let quoted = sh_quote(OsStr::new(HOSTILE));
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf %s {quoted}"))
            .output()
            .expect("sh");
        assert_eq!(String::from_utf8_lossy(&out.stdout), HOSTILE);
    }

    fn ran(status: Option<i32>, stderr: &str, killed: bool, started: bool) -> HalfRun {
        HalfRun {
            status,
            stderr_tail: stderr.to_string(),
            killed,
            started,
            cancelled_late: false,
        }
    }

    /// `pkexec`, before the script started: 126 is the person's No, 127 is
    /// "no prompt possible" with a reason read from what it printed, anything
    /// else is an exit; the wizard's kill is a cancel.
    #[test]
    fn pkexec_exits_are_classified() {
        let c =
            |status, stderr, killed| classify(Tool::Pkexec, &ran(status, stderr, killed, false));
        assert_eq!(c(Some(0), "", false), HalfOutcome::Exited(0));
        assert_eq!(c(Some(126), "", false), HalfOutcome::Cancelled);
        assert_eq!(c(Some(1), "error: x", false), HalfOutcome::Exited(1));
        assert_eq!(c(None, "", true), HalfOutcome::Cancelled);
        match c(
            Some(127),
            "Error executing command as another user: Not authorized",
            false,
        ) {
            HalfOutcome::Unavailable(reason) => assert!(reason.contains("password"), "{reason}"),
            other => panic!("{other:?}"),
        }
        match c(
            Some(127),
            "polkit-agent-helper-1: error response to PolicyKit daemon: GDBus.Error:org.freedesktop.PolicyKit1.Error.Failed: No session for cookie",
            false,
        ) {
            HalfOutcome::Unavailable(reason) => assert!(reason.contains("agent"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    /// F2 — once the script has started, pkexec's 126 and 127 are the
    /// script's own exits (a `set -e` step whose command is missing exits
    /// 127), never "cancelled" and never "no polkit agent" — which would show
    /// the terminal fallback for a package step that failed.
    #[test]
    fn after_the_start_126_and_127_are_the_scripts_own() {
        let c = |status, stderr| classify(Tool::Pkexec, &ran(status, stderr, false, true));
        assert_eq!(
            c(Some(127), "/tmp/x/install.sh: 1: dpkg: not found"),
            HalfOutcome::Exited(127)
        );
        assert_eq!(c(Some(126), "permission denied"), HalfOutcome::Exited(126));
        assert_eq!(c(Some(0), ""), HalfOutcome::Exited(0));
        assert_eq!(c(None, ""), HalfOutcome::Exited(-1));
    }

    /// `osascript`: `(-128)` is the person's Cancel and `(N)` the script's
    /// exit before the start; after it, the status is the script's and a kill
    /// can no longer read as a cancel (F1).
    #[test]
    fn osascript_exits_are_classified() {
        let c = |status, stderr, killed, started| {
            classify(Tool::Osascript, &ran(status, stderr, killed, started))
        };
        assert_eq!(c(Some(0), "", false, false), HalfOutcome::Exited(0));
        assert_eq!(
            c(
                Some(1),
                "execution error: User canceled. (-128)",
                false,
                false
            ),
            HalfOutcome::Cancelled
        );
        assert_eq!(
            c(
                Some(1),
                "execution error: The command exited with a non-zero status. (1)",
                false,
                false
            ),
            HalfOutcome::Exited(1)
        );
        match c(
            Some(1),
            "execution error: something else (-1700)",
            false,
            false,
        ) {
            HalfOutcome::Unavailable(_) => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(c(None, "", true, false), HalfOutcome::Cancelled);
        // F1 — the root shell had started: a kill of osascript is no cancel.
        assert_eq!(c(Some(0), "", true, true), HalfOutcome::Exited(0));
        assert_eq!(c(Some(3), "", false, true), HalfOutcome::Exited(3));
        assert_eq!(
            classify(Tool::Sh, &ran(Some(3), "", false, false)),
            HalfOutcome::Exited(3)
        );
        assert_eq!(
            classify(Tool::Sh, &ran(None, "", false, false)),
            HalfOutcome::Exited(-1)
        );
    }

    /// A spawn that found no program names it.
    #[test]
    fn a_missing_program_is_named() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        assert_eq!(
            spawn_failure(Tool::Pkexec, &e),
            "pkexec is not installed on this computer"
        );
    }

    /// The scratch directory holds the script, private; the config is
    /// shredded on drop and the directory removed.
    #[test]
    fn scratch_is_private_and_shreds_its_config() {
        let scratch = Scratch::create().unwrap();
        let dir = scratch.path().to_path_buf();
        assert!(scratch.script.is_file());
        assert_eq!(
            std::fs::read_to_string(&scratch.script).unwrap(),
            INSTALL_SH
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&scratch.script)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let files = scratch.half_files();
        assert!(files.out.starts_with(&dir) && files.status.starts_with(&dir));
        let cfg = scratch.config_from();
        std::fs::write(&cfg, "agent_token = \"secret\"").unwrap();
        drop(scratch);
        assert!(!cfg.exists());
        assert!(!dir.exists());
    }

    /// A kept enrollment is handed back only to the attempt it was made for.
    #[test]
    fn a_kept_enrollment_matches_scope_server_and_name() {
        clear_kept();
        let kept = || KeptEnrollment {
            scope: Scope::System,
            server: "https://example.invalid".into(),
            device_name: "lab".into(),
            ids: Some(("aid".into(), "tid".into())),
            config_path: PathBuf::from("/etc/roomler/config.toml"),
            scratch: None,
            privileged_done: true,
        };
        keep(kept());
        assert!(take_kept(Scope::PerUser, "https://example.invalid", "lab").is_none());
        keep(kept());
        assert!(take_kept(Scope::System, "https://other.invalid", "lab").is_none());
        keep(kept());
        assert!(take_kept(Scope::System, "https://example.invalid", "lab-2").is_none());
        keep(kept());
        let got = take_kept(Scope::System, "https://example.invalid", "lab").unwrap();
        assert_eq!(got.ids, Some(("aid".into(), "tid".into())));
        assert!(got.privileged_done);
        // Taken once.
        assert!(take_kept(Scope::System, "https://example.invalid", "lab").is_none());
    }

    /// The terminal confirm wakes exactly one waiter, and refuses when none
    /// waits.
    #[tokio::test]
    async fn terminal_confirm_wakes_the_waiter() {
        assert!(confirm_fire().is_err());
        let rx = confirm_arm();
        confirm_fire().unwrap();
        rx.await.unwrap();
        assert!(confirm_fire().is_err());
    }

    /// The tool per OS.
    #[test]
    fn the_prompt_tool_follows_the_os() {
        assert_eq!(elevation_tool(HostOs::Linux), Some(Tool::Pkexec));
        assert_eq!(elevation_tool(HostOs::Macos), Some(Tool::Osascript));
        assert_eq!(elevation_tool(HostOs::Windows), None);
        assert_eq!(Tool::Pkexec.program(), "pkexec");
        assert_eq!(Tool::Osascript.program(), "osascript");
        assert_eq!(Tool::Sh.program(), "/bin/sh");
    }

    /// The marker the window's close guard reads lives as long as its guard.
    #[test]
    fn privileged_running_follows_its_guard() {
        assert!(!privileged_running());
        {
            let _g = PrivilegedRunning::begin();
            assert!(privileged_running());
        }
        assert!(!privileged_running());
    }

    /// A file another process appends to comes out as whole lines, the last
    /// unterminated one at the end.
    #[test]
    fn a_tail_reads_whole_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let mut tail = Tail::new(&path);
        assert!(tail.poll().is_empty(), "no file yet");
        std::fs::write(&path, "one\ntw").unwrap();
        assert_eq!(tail.poll(), vec!["one".to_string()]);
        assert!(tail.has_output());
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"o\r\nthree").unwrap();
        }
        assert_eq!(tail.poll(), vec!["two".to_string()]);
        assert_eq!(tail.finish(), vec!["three".to_string()]);
    }

    // ── the runner, against real processes standing in for the prompt tools
    //    (Unix: they are `/bin/sh` scripts) ──

    #[cfg(unix)]
    fn collector() -> (Channel<ProgressEvent>, std::sync::Arc<Mutex<Vec<String>>>) {
        let seen: std::sync::Arc<Mutex<Vec<String>>> = Default::default();
        let sink = seen.clone();
        let channel = Channel::new(move |body| {
            if let tauri::ipc::InvokeResponseBody::Json(json) = body {
                sink.lock().unwrap().push(json);
            }
            Ok(())
        });
        (channel, seen)
    }

    /// A stand-in for `pkexec`: a shell whose stdout is the half's, as
    /// `pkexec`'s is once it has exec'd the script.
    #[cfg(unix)]
    fn pkexec_like(script: &str) -> PromptCommand {
        PromptCommand {
            program: OsString::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(script)],
            signal: StartSignal::FirstStdout,
        }
    }

    /// Set `flag` once `path` has content, from a thread — the person's click.
    #[cfg(unix)]
    fn cancel_when(path: PathBuf, flag: std::sync::Arc<AtomicBool>) {
        std::thread::spawn(move || {
            for _ in 0..200 {
                if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
                    flag.store(true, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
    }

    /// F1 — a cancel AFTER the half printed its first line (it runs as root
    /// now) does not kill it and does not report "cancelled": the half is
    /// waited out to its own exit, and the run says the cancel came late.
    /// Before the fix the runner killed the client and the outcome was
    /// `Cancelled` — "nothing was installed" — while the root half ran on.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancel_after_the_start_waits_the_half_out() {
        let dir = tempfile::tempdir().unwrap();
        let files = HalfFiles::in_dir(dir.path());
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        cancel_when(files.out.clone(), cancel.clone());
        let (channel, seen) = collector();
        let cmd = pkexec_like(
            "echo '==> roomler install.sh — banner'; sleep 1; echo '==> placed'; exit 0",
        );
        let run = run_privileged_half(&cmd, &files, &channel, Duration::from_secs(60), &cancel)
            .await
            .unwrap();
        assert!(run.started, "{run:?}");
        assert!(!run.killed, "a started half is never killed: {run:?}");
        assert!(run.cancelled_late, "{run:?}");
        assert_eq!(classify(Tool::Pkexec, &run), HalfOutcome::Exited(0));
        let seen = seen.lock().unwrap().join("\n");
        assert!(
            seen.contains("==> placed"),
            "the half ran to its end: {seen}"
        );
        assert!(
            seen.contains("cannot be stopped"),
            "the person is told: {seen}"
        );
    }

    /// F1 — the control: a cancel while the prompt is up (nothing printed
    /// yet) kills the prompt client, and that IS a cancel.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancel_before_the_start_closes_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let files = HalfFiles::in_dir(dir.path());
        let cancel = AtomicBool::new(true);
        let (channel, _seen) = collector();
        let cmd = pkexec_like("sleep 5; echo '==> too late'");
        let begun = Instant::now();
        let run = run_privileged_half(&cmd, &files, &channel, Duration::from_secs(60), &cancel)
            .await
            .unwrap();
        assert!(
            begun.elapsed() < Duration::from_secs(4),
            "killed, not waited"
        );
        assert!(run.killed && !run.started, "{run:?}");
        assert_eq!(classify(Tool::Pkexec, &run), HalfOutcome::Cancelled);
    }

    /// F2 — a script that started and then exits 127 (a `set -e` step whose
    /// command is missing) is a failed step, not "no polkit agent".
    #[cfg(unix)]
    #[tokio::test]
    async fn a_127_after_the_start_is_the_scripts_exit() {
        let dir = tempfile::tempdir().unwrap();
        let files = HalfFiles::in_dir(dir.path());
        let cancel = AtomicBool::new(false);
        let (channel, seen) = collector();
        let cmd = pkexec_like(
            "echo '==> roomler install.sh'; echo 'sh: 1: dpkg: not found' >&2; exit 127",
        );
        let run = run_privileged_half(&cmd, &files, &channel, Duration::from_secs(60), &cancel)
            .await
            .unwrap();
        assert!(run.started, "{run:?}");
        assert_eq!(run.status, Some(127));
        assert_eq!(classify(Tool::Pkexec, &run), HalfOutcome::Exited(127));
        assert!(run.stderr_tail.contains("dpkg: not found"), "{run:?}");
        assert!(seen.lock().unwrap().join("\n").contains("dpkg: not found"));
        // The same exit with nothing on stdout is pkexec's own: no prompt.
        let cmd = pkexec_like(
            "echo 'Error executing command as another user: Not authorized' >&2; exit 127",
        );
        let run = run_privileged_half(&cmd, &files, &channel, Duration::from_secs(60), &cancel)
            .await
            .unwrap();
        assert!(!run.started, "{run:?}");
        assert!(matches!(
            classify(Tool::Pkexec, &run),
            HalfOutcome::Unavailable(_)
        ));
    }

    /// F1, the macOS race — the cancel kills `osascript` before the half was
    /// seen to start, but the password had been accepted in that instant and
    /// the root shell runs on (here a background shell that outlives its
    /// parent, as the root shell outlives `osascript`). Its output file then
    /// appears: the run is NOT a cancel, and it is waited out to its status
    /// file. Before the fix this read as "cancelled, nothing was installed"
    /// while `installer -pkg` carried on.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_root_shell_that_outlives_a_killed_osascript_is_waited_out() {
        let dir = tempfile::tempdir().unwrap();
        let files = HalfFiles::in_dir(dir.path());
        // The person's click lands 300 ms in: the stand-in "root shell" exists
        // by then, and creates its output file only later — the race.
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                cancel.store(true, Ordering::SeqCst);
            });
        }
        let (channel, seen) = collector();
        // `$1 $2 $3` = the out, err and status files, as osascript's argv.
        let late_root_shell = "( sleep 1.2; echo '==> root shell up' > \"$1\"; echo 'warning' > \"$2\"; sleep 0.6; echo '==> done' >> \"$1\"; echo 0 > \"$3\" ) >/dev/null 2>&1 </dev/null & sleep 30";
        let cmd = PromptCommand {
            program: OsString::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from(late_root_shell),
                OsString::from("sh"),
                files.out.clone().into_os_string(),
                files.err.clone().into_os_string(),
                files.status.clone().into_os_string(),
            ],
            signal: StartSignal::OutputFile,
        };
        let run = run_privileged_half(&cmd, &files, &channel, Duration::from_secs(60), &cancel)
            .await
            .unwrap();
        assert!(run.started, "{run:?}");
        assert!(!run.killed, "{run:?}");
        assert!(run.cancelled_late, "{run:?}");
        assert_eq!(run.status, Some(0), "read from the status file");
        assert_eq!(classify(Tool::Osascript, &run), HalfOutcome::Exited(0));
        let seen = seen.lock().unwrap().join("\n");
        assert!(seen.contains("==> done"), "waited to the end: {seen}");
        assert!(
            seen.contains("password had already been accepted"),
            "{seen}"
        );
    }

    /// The macOS control: a killed `osascript` whose shell never started is a
    /// cancel, after the grace.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_killed_osascript_with_no_root_shell_is_a_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let files = HalfFiles::in_dir(dir.path());
        let cancel = AtomicBool::new(true);
        let (channel, _seen) = collector();
        let cmd = PromptCommand {
            program: OsString::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from("sleep 30")],
            signal: StartSignal::OutputFile,
        };
        let run = run_privileged_half(&cmd, &files, &channel, Duration::from_secs(60), &cancel)
            .await
            .unwrap();
        assert!(run.killed && !run.started, "{run:?}");
        assert_eq!(classify(Tool::Osascript, &run), HalfOutcome::Cancelled);
    }
}
