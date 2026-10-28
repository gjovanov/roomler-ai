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
//! * what an **exit code** means. `pkexec` exits 126 when the person
//!   dismissed its dialog and 127 when it could not authenticate at all (no
//!   polkit agent, `pkexec` absent, the password refused); `osascript`
//!   reports a dismissed prompt as `User canceled. (-128)` and a failed shell
//!   command as `(N)` with the command's own exit status. A dismissed dialog
//!   reads as *cancelled*, never as a failure that blames the package;
//! * the **scratch** directory, 0700, the person's own, holding the script
//!   and — for a system scope — the config the wizard enrolled in-process
//!   (0600; the agent JWT and the WireGuard key). The config is shredded the
//!   moment the privileged half has placed it; a placement that failed keeps
//!   it, so a Retry reuses the enrollment instead of a burnt single-use
//!   token (the B0 property), and the wizard's exit shreds whatever is left.
//!   The script file is the person's own: a user-level attacker who could
//!   swap it could as well call `pkexec` themselves, and gains nothing from
//!   the swap — so it is written 0600 in a 0700 directory and not guarded
//!   further;
//! * the **runner**, which streams a half's stdout and stderr into the
//!   progress channel line by line and polls the cancel flag. A cancel while
//!   the prompt is up kills the client (`pkexec` / `osascript`, still the
//!   person's process) and the dialog goes away; after the password the half
//!   runs as root and nothing the person's process sends can stop it, so the
//!   cancel is reported and the half is waited out.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
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

/// The AppleScript the macOS half runs — a constant. It takes the half's
/// argv as *its* arguments and quotes each with `quoted form of`, so the
/// device name and the server URL are never part of the source: `osascript`
/// receives them as plain argv, after `--`.
pub const ADMIN_APPLESCRIPT: &[&str] = &[
    "on run argv",
    "set cmd to \"\"",
    "repeat with a in argv",
    "set cmd to cmd & \" \" & quoted form of (a as text)",
    "end repeat",
    "do shell script cmd with administrator privileges",
    "end run",
];

/// `osascript -e <line>… -- <args…>`.
pub fn osascript_args(args: &[OsString]) -> Vec<OsString> {
    let mut out: Vec<OsString> = Vec::with_capacity(ADMIN_APPLESCRIPT.len() * 2 + 1 + args.len());
    for line in ADMIN_APPLESCRIPT {
        out.push(OsString::from("-e"));
        out.push(OsString::from(line));
    }
    out.push(OsString::from("--"));
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

// ─── exit codes ─────────────────────────────────────────────────────────────

/// What a half's exit meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HalfOutcome {
    /// The script ran and exited with this status (`0` = done).
    Exited(i32),
    /// The person dismissed the administrator prompt, or the wizard's cancel
    /// closed it: nothing ran as root.
    Cancelled,
    /// No prompt could be shown, or it refused: the reason, for the person.
    Unavailable(String),
}

/// `pkexec`'s own exits (its manual: 126 dismissed, 127 not authorized or no
/// authentication possible) and the script's (1); `osascript`'s `(-128)` for
/// a dismissed prompt and `(N)` for a shell command that exited `N`.
/// `killed` says the wizard's cancel killed the client.
pub fn classify(tool: Tool, status: Option<i32>, stderr_tail: &str, killed: bool) -> HalfOutcome {
    if killed {
        return HalfOutcome::Cancelled;
    }
    match tool {
        Tool::Pkexec => match status {
            Some(0) => HalfOutcome::Exited(0),
            Some(126) => HalfOutcome::Cancelled,
            Some(127) => HalfOutcome::Unavailable(pkexec_reason(stderr_tail)),
            Some(n) => HalfOutcome::Exited(n),
            None => HalfOutcome::Exited(-1),
        },
        Tool::Osascript => match status {
            Some(0) => HalfOutcome::Exited(0),
            Some(_) | None => match applescript_error_code(stderr_tail) {
                Some(-128) => HalfOutcome::Cancelled,
                Some(n) if n > 0 => HalfOutcome::Exited(n),
                _ => HalfOutcome::Unavailable(format!(
                    "the administrator prompt could not run the install: {}",
                    last_line(stderr_tail)
                )),
            },
        },
        Tool::Terminal | Tool::Sh => match status {
            Some(n) => HalfOutcome::Exited(n),
            None => HalfOutcome::Exited(-1),
        },
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

/// A 0700 directory of the person's own, holding the script and — for a
/// system scope — the enrolled config the privileged half places. Dropping
/// it shreds the config and removes the directory.
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
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
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

// ─── the enrollment kept from a failed attempt ──────────────────────────────

/// An enrollment that happened in an attempt whose later step failed: the
/// identity the server already holds for this device, so the next attempt
/// with the same server and name reuses it instead of posting a token that
/// is already spent (the B0 property, "a package placement that fails leaves
/// an enrolled config to retry with"). For a system scope the scratch
/// directory still holds the enrolled file; for a per-user scope the config
/// is already at its path.
#[derive(Debug)]
pub struct KeptEnrollment {
    pub scope: Scope,
    pub server: String,
    pub device_name: String,
    pub agent_id: String,
    pub tenant_id: String,
    pub config_path: PathBuf,
    pub scratch: Option<Scratch>,
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

// ─── the runner ─────────────────────────────────────────────────────────────

/// What a half's process did.
#[derive(Debug)]
pub struct HalfRun {
    pub status: Option<i32>,
    /// The last lines of stderr, for the exit's meaning.
    pub stderr_tail: String,
    /// The wizard's cancel killed the process.
    pub killed: bool,
}

/// Spawn `program args…`, stream every stdout and stderr line into the
/// progress channel as `ScriptLine`, poll the cancel flag, and wait for the
/// exit within `budget`. On cancel the process is killed when it can be (the
/// prompt client, still the person's); when it cannot (the half already runs
/// as root) the cancel is reported and the half waited out. Over budget, the
/// same: a root process cannot be stopped from here, so the run is marked
/// and reported rather than abandoned.
pub async fn run_half(
    program: &str,
    args: &[OsString],
    half: Half,
    on_event: &Channel<ProgressEvent>,
    budget: Duration,
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
                    if t.len() >= 40 {
                        t.remove(0);
                    }
                    t.push(line.clone());
                }
                emit_line(&channel, half, line);
            }
        }));
    }

    let started = Instant::now();
    let mut killed = false;
    let mut cancel_seen = false;
    let mut over_budget = false;
    let status = loop {
        tokio::select! {
            res = child.wait() => break res?,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                if !cancel_seen && crate::CANCEL_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
                    cancel_seen = true;
                    match child.start_kill() {
                        Ok(()) => {
                            killed = true;
                            emit_line(on_event, half, "cancel: stopping the step".to_string());
                        }
                        Err(e) => emit_line(
                            on_event,
                            half,
                            format!(
                                "cancel: this step already runs as root and cannot be interrupted from here ({e}); waiting for it to finish"
                            ),
                        ),
                    }
                }
                if !over_budget && started.elapsed() > budget {
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
            }
        }
    };
    for r in readers {
        let _ = r.await;
    }
    let stderr_tail = tail.lock().unwrap_or_else(|p| p.into_inner()).join("\n");
    Ok(HalfRun {
        status: status.code(),
        stderr_tail,
        killed,
    })
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

    /// The macOS prompt: a constant AppleScript, then `--`, then the half's
    /// argv verbatim. Nothing from the argv appears in a `-e` line.
    #[test]
    fn osascript_takes_the_argv_as_arguments_never_as_source() {
        let script = Path::new("/tmp/roomler-setup-x/install.sh");
        let half = privileged_args(&spec(script, Scope::PerUser, None, &[]));
        let args = osascript_args(&half);
        let as_str: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
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
            source
                .iter()
                .all(|l| !l.contains("example.invalid") && !l.contains("a'b")),
            "no argument is interpolated into the AppleScript"
        );
        assert!(
            source
                .iter()
                .any(|l| l.contains("quoted form of") && l.contains("argv")
                    || l.contains("repeat with a in argv")),
            "each argument is quoted with `quoted form of`"
        );
        assert!(
            source
                .iter()
                .any(|l| l.contains("with administrator privileges")),
            "the prompt is the administrator one"
        );
        // After `--`: the half's argv, byte-exact.
        let half_str: Vec<&str> = half.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(&as_str[dashdash + 1..], &half_str[..]);
        assert_eq!(as_str[dashdash + 1 + 8], HOSTILE);
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

    /// `pkexec`: 126 is the person's No, 127 is "no prompt possible" with a
    /// reason read from what it printed, anything else is the script's own
    /// exit; a kill by the wizard's cancel is a cancel whatever the status.
    #[test]
    fn pkexec_exits_are_classified() {
        assert_eq!(
            classify(Tool::Pkexec, Some(0), "", false),
            HalfOutcome::Exited(0)
        );
        assert_eq!(
            classify(Tool::Pkexec, Some(126), "", false),
            HalfOutcome::Cancelled
        );
        assert_eq!(
            classify(Tool::Pkexec, Some(1), "error: x", false),
            HalfOutcome::Exited(1)
        );
        assert_eq!(
            classify(Tool::Pkexec, None, "", true),
            HalfOutcome::Cancelled
        );
        assert_eq!(
            classify(Tool::Pkexec, Some(0), "", true),
            HalfOutcome::Cancelled
        );
        match classify(
            Tool::Pkexec,
            Some(127),
            "Error executing command as another user: Not authorized",
            false,
        ) {
            HalfOutcome::Unavailable(reason) => assert!(reason.contains("password"), "{reason}"),
            other => panic!("{other:?}"),
        }
        match classify(
            Tool::Pkexec,
            Some(127),
            "polkit-agent-helper-1: error response to PolicyKit daemon: GDBus.Error:org.freedesktop.PolicyKit1.Error.Failed: No session for cookie",
            false,
        ) {
            HalfOutcome::Unavailable(reason) => assert!(reason.contains("agent"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    /// `osascript`: `(-128)` is the person's Cancel; `(N)` the script's exit.
    #[test]
    fn osascript_exits_are_classified() {
        assert_eq!(
            classify(Tool::Osascript, Some(0), "", false),
            HalfOutcome::Exited(0)
        );
        assert_eq!(
            classify(
                Tool::Osascript,
                Some(1),
                "execution error: User canceled. (-128)",
                false
            ),
            HalfOutcome::Cancelled
        );
        assert_eq!(
            classify(
                Tool::Osascript,
                Some(1),
                "execution error: error: no sha256 digest published for roomlerd.pkg — an elevated install places nothing it cannot verify (1)",
                false
            ),
            HalfOutcome::Exited(1)
        );
        match classify(
            Tool::Osascript,
            Some(1),
            "execution error: something else (-1700)",
            false,
        ) {
            HalfOutcome::Unavailable(_) => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(
            classify(Tool::Osascript, None, "", true),
            HalfOutcome::Cancelled
        );
        assert_eq!(
            classify(Tool::Sh, Some(3), "", false),
            HalfOutcome::Exited(3)
        );
        assert_eq!(classify(Tool::Sh, None, "", false), HalfOutcome::Exited(-1));
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
            agent_id: "aid".into(),
            tenant_id: "tid".into(),
            config_path: PathBuf::from("/etc/roomler/config.toml"),
            scratch: None,
        };
        keep(kept());
        assert!(take_kept(Scope::PerUser, "https://example.invalid", "lab").is_none());
        keep(kept());
        assert!(take_kept(Scope::System, "https://other.invalid", "lab").is_none());
        keep(kept());
        assert!(take_kept(Scope::System, "https://example.invalid", "lab-2").is_none());
        keep(kept());
        let got = take_kept(Scope::System, "https://example.invalid", "lab").unwrap();
        assert_eq!(got.agent_id, "aid");
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
}
