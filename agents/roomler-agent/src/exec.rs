//! Fleet RPC — the agent-side execution engine.
//!
//! Runs ONE bounded shell command on behalf of a `rc:rpc.exec` frame the
//! server has already gated (org kill-switch, caller permission, the device's
//! `ExecPolicy`) and answers with a `rc:rpc.result`.
//!
//! ## What this module is responsible for
//!
//! The server owns *who may ask*. This module owns *what actually happens on
//! the box*, which is four things the server cannot enforce from a distance:
//!
//! * **Gate 4** — the agent's own `exec_enabled` config key. A device owner
//!   can refuse execution even when the server says yes, and that refusal
//!   survives a compromised control plane. Checked by the caller in
//!   `signaling.rs` before we are reached.
//! * **Bounds** — wall-clock timeout, a combined stdout+stderr ceiling, and a
//!   concurrency cap. Re-enforced here rather than trusted from the wire, so
//!   a forged or replayed frame can't ask for an unbounded run.
//! * **Redaction** — output is swept for the agent token, `Bearer …` headers
//!   and JWT-shaped strings BEFORE it leaves the host. Command output is
//!   persisted in `exec_audit`, so a secret a command happens to echo would
//!   otherwise outlive the session by 90 days.
//! * **Process-tree kill** — a timeout or cancel must not leave orphans. On
//!   Windows that's `taskkill /T /F`; on Unix the child leads its own process
//!   group and we signal the group.
//!
//! ## Injection
//!
//! The command is passed as a SINGLE argv element to the shell
//! (`pwsh -Command <cmd>`, `bash -c <cmd>`). There is no string concatenation
//! at the spawn boundary, so nothing the caller writes can escape into the
//! agent's own argv — the shell then interprets it, which is the whole point
//! of the feature.
//!
//! ## Privilege
//!
//! On a perMachine Windows install the daemon runs as SYSTEM; under systemd,
//! as root. Commands inherit that. This is deliberate — `Get-NetFirewallRule`,
//! `netsh`, route tables and service state are exactly what remote diagnosis
//! needs — and is surfaced in the admin UI's opt-in copy rather than hidden.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Semaphore, oneshot};
use tracing::{info, warn};

use roomler_ai_remote_control::models::exec_limits;

/// One execution request, already clamped by the server and re-clamped here.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    pub request_id: String,
    /// `pwsh` | `powershell` | `cmd` | `bash` | `sh`, or empty for the host
    /// default.
    pub shell: String,
    pub command: String,
    pub timeout_ms: u64,
    pub max_output_bytes: u64,
    pub cwd: Option<String>,
    /// Display name of the acting principal, for the local log.
    pub caller: String,
}

/// What came back. Mirrors `ClientMsg::RpcResult` one-for-one.
#[derive(Debug, Clone, Default)]
pub struct ExecOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub duration_ms: u64,
    /// Set when the command never ran, timed out, or was cancelled.
    pub error: Option<String>,
}

impl ExecOutcome {
    /// A refusal / failure that never reached a process.
    fn failed(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Default::default()
        }
    }

    /// SHA-256 over the full redacted output, so a truncated audit sample can
    /// still be tied to what actually ran.
    ///
    /// The streams are domain-separated: without the `\0`, `("one", "two")`
    /// and `("onetwo", "")` would hash identically, and two materially
    /// different runs would be indistinguishable in the audit log.
    pub fn output_sha256(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.stdout.as_bytes());
        h.update([0u8]);
        h.update(self.stderr.as_bytes());
        hex::encode(h.finalize())
    }

    /// Combined output size in bytes.
    pub fn output_bytes(&self) -> u64 {
        (self.stdout.len() + self.stderr.len()) as u64
    }
}

/// Masks secrets out of command output before it leaves the host.
///
/// Deliberately dependency-free (no `regex` in a binary that ships to the
/// fleet) and deliberately over-eager: a false positive costs a reader some
/// context, a false negative persists a credential for 90 days.
#[derive(Clone, Default)]
pub struct Redactor {
    /// Literal secrets known to this process — the agent token(s).
    literals: Vec<String>,
}

/// What a masked span is replaced with.
const MASK: &str = "[redacted]";
/// Shortest literal we'll mask. Below this, matches are noise rather than
/// secrets and blanking them would shred ordinary output.
const MIN_LITERAL_LEN: usize = 8;

impl Redactor {
    pub fn new(literals: impl IntoIterator<Item = String>) -> Self {
        Self {
            literals: literals
                .into_iter()
                .filter(|s| s.len() >= MIN_LITERAL_LEN)
                .collect(),
        }
    }

    pub fn apply(&self, input: &str) -> String {
        let mut out = input.to_string();
        for lit in &self.literals {
            if out.contains(lit.as_str()) {
                out = out.replace(lit.as_str(), MASK);
            }
        }
        let out = mask_bearer(&out);
        mask_jwt_shaped(&out)
    }
}

/// Mask the token after a `Bearer ` / `bearer ` prefix, up to the next
/// whitespace.
fn mask_bearer(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        let Some(pos) = find_ignore_ascii_case(rest, "bearer ") else {
            out.push_str(rest);
            return out;
        };
        let after = pos + "bearer ".len();
        out.push_str(&rest[..after]);
        let tail = &rest[after..];
        let end = tail.find(|c: char| c.is_whitespace()).unwrap_or(tail.len());
        if end >= MIN_LITERAL_LEN {
            out.push_str(MASK);
        } else {
            out.push_str(&tail[..end]);
        }
        rest = &tail[end..];
    }
}

fn find_ignore_ascii_case(haystack: &str, needle_lower: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle_lower.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| {
        h[i..i + n.len()]
            .iter()
            .zip(n)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
    })
}

/// Mask `xxx.yyy.zzz` runs of base64url characters — the JWT shape. Three
/// segments of ≥8 base64url chars each is not a thing ordinary command output
/// produces, so this is safe to be blunt about.
fn mask_jwt_shaped(input: &str) -> String {
    const MIN_SEG: usize = 8;
    let is_b64 = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';

    let mut out = String::with_capacity(input.len());
    let bytes: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if !is_b64(bytes[i]) {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // Try to consume seg '.' seg '.' seg starting at i.
        let start = i;
        let mut cursor = i;
        let mut segs = 0;
        let mut ok = true;
        while segs < 3 {
            let seg_start = cursor;
            while cursor < bytes.len() && is_b64(bytes[cursor]) {
                cursor += 1;
            }
            if cursor - seg_start < MIN_SEG {
                ok = false;
                break;
            }
            segs += 1;
            if segs < 3 {
                if cursor < bytes.len() && bytes[cursor] == '.' {
                    cursor += 1;
                } else {
                    ok = false;
                    break;
                }
            }
        }
        if ok && segs == 3 {
            out.push_str(MASK);
            i = cursor;
        } else {
            // Not a JWT — emit the first run verbatim and re-scan from there.
            let mut run_end = start;
            while run_end < bytes.len() && is_b64(bytes[run_end]) {
                run_end += 1;
            }
            out.extend(&bytes[start..run_end]);
            i = run_end;
        }
    }
    out
}

/// Which program + fixed args implement a shell name on this host.
///
/// The caller's command is appended as ONE further argv element — never
/// concatenated — so it cannot escape into the agent's own argv.
/// Prefix that forces a Windows shell to emit UTF-8.
///
/// PowerShell writes its pipe in the host's ANSI/OEM codepage, not UTF-8. On a
/// German-locale host `whoami` returns `nt-autorität\system`, which arrives as
/// `nt-autorit<?>t\system` after our lossy decode — field-caught on CORPLAP-1,
/// 2026-08-06. Setting the console output encoding per-process is the standard
/// fix; it does not leak outside this child, and an assignment before the
/// caller's command does not affect the exit code PowerShell reports.
#[cfg(windows)]
const PS_UTF8_PREFIX: &str = "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; ";
/// `cmd.exe` equivalent — switch the console codepage to UTF-8 first.
#[cfg(windows)]
const CMD_UTF8_PREFIX: &str = "chcp 65001>nul&";

/// Wrap the caller's command so the shell emits UTF-8. Still ONE argv element,
/// so the injection story is unchanged.
#[cfg(windows)]
fn utf8_wrapped(shell_program: &str, command: &str) -> String {
    if shell_program.eq_ignore_ascii_case("cmd.exe") {
        format!("{CMD_UTF8_PREFIX}{command}")
    } else {
        format!("{PS_UTF8_PREFIX}{command}")
    }
}

fn resolve_shell(requested: &str) -> Result<(&'static str, Vec<&'static str>), String> {
    let want = requested.trim().to_ascii_lowercase();
    #[cfg(windows)]
    {
        // `powershell` is the auto default: it is present on every supported
        // Windows since Vista, whereas `pwsh` is an optional install. A
        // caller who wants pwsh asks for it by name and gets a clean
        // "not installed" if it is absent.
        match want.as_str() {
            "" | "auto" | "powershell" => Ok((
                "powershell.exe",
                vec!["-NoProfile", "-NonInteractive", "-NoLogo", "-Command"],
            )),
            "pwsh" => Ok((
                "pwsh.exe",
                vec!["-NoProfile", "-NonInteractive", "-NoLogo", "-Command"],
            )),
            "cmd" => Ok(("cmd.exe", vec!["/C"])),
            other => Err(format!(
                "unsupported shell {other:?} on this device (have: powershell, pwsh, cmd)"
            )),
        }
    }
    #[cfg(not(windows))]
    {
        // `-c`, not `-lc`: a login shell sources profiles that print banners
        // into stdout and make output non-deterministic across hosts. A
        // command that needs profile PATH can source it explicitly.
        match want.as_str() {
            "" | "auto" | "bash" => Ok(("bash", vec!["-c"])),
            "sh" => Ok(("sh", vec!["-c"])),
            other => Err(format!(
                "unsupported shell {other:?} on this device (have: bash, sh)"
            )),
        }
    }
}

/// Literal secrets every exec's output is swept for. Process-wide because a
/// multi-org daemon holds one agent token PER ORG and a command's output must
/// not leak org B's token just because org A asked for it.
static SECRETS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Register a literal secret to mask out of every future exec's output. Each
/// org's signalling loop calls this with its own agent token on connect.
pub fn register_secret(secret: &str) {
    if secret.len() < MIN_LITERAL_LEN {
        return;
    }
    let mut g = match SECRETS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !g.iter().any(|s| s == secret) {
        g.push(secret.to_string());
    }
}

/// A [`Redactor`] over every secret registered so far.
pub fn redactor() -> Redactor {
    let g = match SECRETS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    Redactor::new(g.clone())
}

/// The process-wide engine. Concurrency is a property of the DEVICE, not of
/// an org's WS connection, so a multi-org daemon must not multiply its own
/// exec cap by the number of orgs it has joined.
pub fn shared() -> &'static ExecEngine {
    static ENGINE: std::sync::OnceLock<ExecEngine> = std::sync::OnceLock::new();
    ENGINE.get_or_init(ExecEngine::new)
}

// ─── Outbound leg: commands THIS device asks other devices to run ────────
//
// `roomler exec <device> …` goes CLI → LocalAPI → this daemon's agent WS →
// server → target. The answer arrives asynchronously on the WS receive path,
// which has no way back to the parked LocalAPI call — hence this registry.

type PendingMap = std::sync::Mutex<HashMap<String, oneshot::Sender<ExecOutcome>>>;

fn pending() -> &'static PendingMap {
    static PENDING: std::sync::OnceLock<PendingMap> = std::sync::OnceLock::new();
    PENDING.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn lock_pending() -> std::sync::MutexGuard<'static, HashMap<String, oneshot::Sender<ExecOutcome>>> {
    match pending().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Park a slot for `request_id` before sending the request, and get the
/// receiver to await the answer on. Dropping the returned guard deregisters,
/// so a caller that times out leaves nothing behind.
pub fn expect_response(request_id: &str) -> (PendingGuard, oneshot::Receiver<ExecOutcome>) {
    let (tx, rx) = oneshot::channel();
    lock_pending().insert(request_id.to_string(), tx);
    (
        PendingGuard {
            request_id: request_id.to_string(),
        },
        rx,
    )
}

/// Deregisters its request id on drop, so an abandoned caller can't leak a
/// slot that a later id-reuse would deliver into.
pub struct PendingGuard {
    request_id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        lock_pending().remove(&self.request_id);
    }
}

/// Hand a `rc:rpc.response` to whoever is parked on it. Returns whether a
/// waiter was found — `false` just means that caller already gave up.
pub fn deliver_response(request_id: &str, outcome: ExecOutcome) -> bool {
    match lock_pending().remove(request_id) {
        Some(tx) => tx.send(outcome).is_ok(),
        None => false,
    }
}

/// Serialises + bounds every exec on this device.
pub struct ExecEngine {
    sem: Arc<Semaphore>,
    inflight: Mutex<HashMap<String, oneshot::Sender<()>>>,
}

impl Default for ExecEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecEngine {
    pub fn new() -> Self {
        Self {
            sem: Arc::new(Semaphore::new(exec_limits::MAX_CONCURRENT_PER_AGENT)),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Cancel an in-flight request. Returns whether one was found. The run
    /// itself still answers with an `ExecOutcome` carrying `error`, so a
    /// cancelled caller is never left waiting.
    pub async fn cancel(&self, request_id: &str) -> bool {
        match self.inflight.lock().await.remove(request_id) {
            Some(tx) => {
                let _ = tx.send(());
                true
            }
            None => false,
        }
    }

    /// Run one command to completion (or timeout / cancel).
    pub async fn run(&self, req: ExecRequest, redactor: &Redactor) -> ExecOutcome {
        // Re-clamp rather than trust the wire: the server clamps too, but a
        // forged frame reaching a compromised control plane must not be able
        // to ask for an unbounded run.
        let timeout_ms = exec_limits::clamp_timeout_ms(req.timeout_ms);
        let max_output = exec_limits::clamp_output_bytes(req.max_output_bytes);

        let (program, args) = match resolve_shell(&req.shell) {
            Ok(v) => v,
            Err(e) => return ExecOutcome::failed(e),
        };

        // Refuse rather than queue: a caller blocked on a deadline would
        // rather get a fast "busy" than a slow timeout.
        let Ok(_permit) = self.sem.clone().try_acquire_owned() else {
            return ExecOutcome::failed(format!(
                "device is already running {} commands (the per-device limit)",
                exec_limits::MAX_CONCURRENT_PER_AGENT
            ));
        };

        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.inflight
            .lock()
            .await
            .insert(req.request_id.clone(), cancel_tx);

        info!(
            request_id = %req.request_id,
            caller = %req.caller,
            shell = %req.shell,
            timeout_ms,
            "fleet-rpc: running command"
        );

        let started = std::time::Instant::now();
        let mut outcome = self
            .spawn_and_wait(&req, program, &args, timeout_ms, max_output, cancel_rx)
            .await;
        outcome.duration_ms = started.elapsed().as_millis() as u64;

        self.inflight.lock().await.remove(&req.request_id);

        outcome.stdout = redactor.apply(&outcome.stdout);
        outcome.stderr = redactor.apply(&outcome.stderr);

        info!(
            request_id = %req.request_id,
            exit_code = ?outcome.exit_code,
            duration_ms = outcome.duration_ms,
            truncated = outcome.truncated,
            error = ?outcome.error,
            "fleet-rpc: command finished"
        );
        outcome
    }

    async fn spawn_and_wait(
        &self,
        req: &ExecRequest,
        program: &str,
        args: &[&str],
        timeout_ms: u64,
        max_output: u64,
        cancel_rx: oneshot::Receiver<()>,
    ) -> ExecOutcome {
        #[cfg(windows)]
        let command = utf8_wrapped(program, &req.command);
        #[cfg(not(windows))]
        let command = req.command.clone();

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            // The caller's command is ONE argv element. Nothing in it can
            // escape into our own argv.
            .arg(&command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &req.cwd {
            cmd.current_dir(dir);
        }
        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW — the daemon may run in an interactive
            // session; a console flashing on the user's desktop for every
            // diagnostic would be its own bug report.
            cmd.creation_flags(0x0800_0000);
        }
        #[cfg(unix)]
        {
            // Lead a new process group so a timeout can signal the whole
            // tree, not just the shell that spawned it.
            cmd.process_group(0);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ExecOutcome::failed(format!(
                    "shell {program:?} is not installed on this device"
                ));
            }
            Err(e) => return ExecOutcome::failed(format!("failed to start {program:?}: {e}")),
        };
        let pid = child.id();

        // One shared budget across both streams — "combined ceiling" is what
        // the wire promises, so per-stream caps would silently double it.
        let budget = Arc::new(AtomicU64::new(max_output));
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_task = tokio::spawn(read_capped(stdout, budget.clone()));
        let err_task = tokio::spawn(read_capped(stderr, budget.clone()));

        let timeout = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms));
        tokio::pin!(timeout);

        let (status, error) = tokio::select! {
            res = child.wait() => match res {
                Ok(s) => (Some(s), None),
                Err(e) => (None, Some(format!("waiting for the command failed: {e}"))),
            },
            _ = &mut timeout => {
                kill_tree(&mut child, pid).await;
                (None, Some(format!("timed out after {timeout_ms}ms")))
            }
            _ = cancel_rx => {
                kill_tree(&mut child, pid).await;
                (None, Some("cancelled by the caller".to_string()))
            }
        };

        let (stdout, out_trunc) = out_task.await.unwrap_or_default();
        let (stderr, err_trunc) = err_task.await.unwrap_or_default();

        ExecOutcome {
            exit_code: status.and_then(|s| s.code()),
            stdout,
            stderr,
            truncated: out_trunc || err_trunc,
            duration_ms: 0,
            error,
        }
    }
}

/// Drain one pipe into a String, stopping once the shared budget is spent.
/// Returns `(text, truncated)`.
async fn read_capped<R>(reader: Option<R>, budget: Arc<AtomicU64>) -> (String, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return (String::new(), false);
    };
    let mut collected: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                // Claim from the shared budget; whatever we can't claim is
                // the truncation point.
                let allowed = loop {
                    let left = budget.load(Ordering::Relaxed);
                    let take = (n as u64).min(left);
                    if budget
                        .compare_exchange_weak(
                            left,
                            left - take,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        break take as usize;
                    }
                };
                collected.extend_from_slice(&chunk[..allowed]);
                if allowed < n {
                    truncated = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&collected).into_owned(), truncated)
}

/// Kill the child AND everything it spawned. A diagnostic that shells out
/// must not leave a detached process behind when it times out.
async fn kill_tree(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(windows)]
    {
        if let Some(pid) = pid {
            // `taskkill /T` is the only way to reach grandchildren without
            // building a Job Object around every spawn.
            let _ = tokio::process::Command::new("taskkill.exe")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
    }
    #[cfg(unix)]
    {
        if let Some(pid) = pid {
            // Negative pid = the process group we created with
            // `process_group(0)`.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    #[cfg(not(any(windows, unix)))]
    let _ = pid;

    if let Err(e) = child.kill().await {
        warn!(%e, "fleet-rpc: killing the command failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> ExecEngine {
        ExecEngine::new()
    }

    fn req(command: &str) -> ExecRequest {
        ExecRequest {
            request_id: "t1".into(),
            shell: String::new(),
            command: command.into(),
            timeout_ms: 10_000,
            max_output_bytes: 0,
            cwd: None,
            caller: "test".into(),
        }
    }

    /// A command that prints `hello` on either platform's auto shell.
    fn echo_hello() -> &'static str {
        if cfg!(windows) {
            "Write-Output hello"
        } else {
            "echo hello"
        }
    }

    #[tokio::test]
    async fn runs_a_command_and_captures_stdout() {
        let out = engine().run(req(echo_hello()), &Redactor::default()).await;
        assert_eq!(out.error, None, "stderr was: {}", out.stderr);
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("hello"), "stdout was: {:?}", out.stdout);
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn reports_a_nonzero_exit_code() {
        // `exit 3` happens to be spelled the same in PowerShell and bash.
        let out = engine().run(req("exit 3"), &Redactor::default()).await;
        assert_eq!(out.exit_code, Some(3));
        assert_eq!(out.error, None);
    }

    #[tokio::test]
    async fn timeout_kills_and_reports() {
        let cmd = if cfg!(windows) {
            "Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };
        let mut r = req(cmd);
        r.timeout_ms = 500;
        let out = engine().run(r, &Redactor::default()).await;
        assert_eq!(out.exit_code, None);
        assert!(
            out.error
                .as_deref()
                .unwrap_or_default()
                .contains("timed out"),
            "error was {:?}",
            out.error
        );
        // The bound is what's promised — a 30 s sleep must not have run to
        // completion.
        assert!(out.duration_ms < 10_000, "took {}ms", out.duration_ms);
    }

    #[tokio::test]
    async fn cancel_stops_an_inflight_command() {
        let cmd = if cfg!(windows) {
            "Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };
        let eng = Arc::new(engine());
        let mut r = req(cmd);
        r.request_id = "cancel-me".into();
        r.timeout_ms = 30_000;

        let runner = {
            let eng = eng.clone();
            tokio::spawn(async move { eng.run(r, &Redactor::default()).await })
        };
        // Let it get as far as registering itself.
        for _ in 0..100 {
            if eng.inflight.lock().await.contains_key("cancel-me") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(eng.cancel("cancel-me").await, "request should be in flight");

        let out = runner.await.unwrap();
        assert!(
            out.error
                .as_deref()
                .unwrap_or_default()
                .contains("cancelled"),
            "error was {:?}",
            out.error
        );
        assert!(out.duration_ms < 10_000, "took {}ms", out.duration_ms);
        assert!(!eng.cancel("cancel-me").await, "should be deregistered");
    }

    #[tokio::test]
    async fn output_is_capped_and_flagged() {
        // Emit far more than the cap allows.
        let cmd = if cfg!(windows) {
            "1..2000 | ForEach-Object { 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' }"
        } else {
            "for i in $(seq 1 2000); do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done"
        };
        let mut r = req(cmd);
        r.max_output_bytes = 4096;
        let out = engine().run(r, &Redactor::default()).await;
        assert!(out.truncated, "expected truncation");
        assert!(
            out.output_bytes() <= 4096,
            "captured {} bytes, cap was 4096",
            out.output_bytes()
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn non_ascii_output_survives_a_non_utf8_console_codepage() {
        // Field-caught on CORPLAP-1 (German locale): `whoami` returns
        // `nt-autorität\system`, which PowerShell wrote in the host ANSI
        // codepage and our lossy decode turned into `nt-autorit<?>t\system`.
        // A diagnostic that garbles the machine's own output is worse than
        // useless — it invites the reader to doubt the tool, not the finding.
        let out = engine()
            .run(req("Write-Output 'ä ö ü ß — 日本'"), &Redactor::default())
            .await;
        assert_eq!(out.error, None, "stderr: {}", out.stderr);
        assert!(
            out.stdout.contains("ä ö ü ß"),
            "non-ASCII was mangled: {:?}",
            out.stdout
        );
        assert!(
            !out.stdout.contains('\u{FFFD}'),
            "replacement chars in output: {:?}",
            out.stdout
        );
    }

    #[cfg(windows)]
    #[test]
    fn utf8_prefix_is_shell_appropriate() {
        assert!(utf8_wrapped("powershell.exe", "X").starts_with("[Console]::OutputEncoding"));
        assert!(utf8_wrapped("pwsh.exe", "X").starts_with("[Console]::OutputEncoding"));
        assert!(utf8_wrapped("cmd.exe", "X").starts_with("chcp 65001"));
        // The caller's command is still there, unmodified, at the end.
        assert!(utf8_wrapped("cmd.exe", "echo hi").ends_with("echo hi"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn utf8_prefix_does_not_disturb_the_exit_code() {
        // The prefix is an assignment before the caller's command; PowerShell
        // must still report the command's own exit code.
        let out = engine().run(req("exit 7"), &Redactor::default()).await;
        assert_eq!(out.exit_code, Some(7), "error: {:?}", out.error);
    }

    #[tokio::test]
    async fn unknown_shell_is_a_clean_error_not_a_panic() {
        let mut r = req("whatever");
        r.shell = "zsh-from-mars".into();
        let out = engine().run(r, &Redactor::default()).await;
        assert!(
            out.error
                .as_deref()
                .unwrap_or_default()
                .contains("unsupported shell"),
            "error was {:?}",
            out.error
        );
        assert_eq!(out.exit_code, None);
    }

    #[tokio::test]
    async fn concurrency_cap_refuses_rather_than_queues() {
        let cmd = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };
        let eng = Arc::new(engine());
        let mut runners = Vec::new();
        for i in 0..exec_limits::MAX_CONCURRENT_PER_AGENT {
            let eng = eng.clone();
            let mut r = req(cmd);
            r.request_id = format!("hog-{i}");
            runners.push(tokio::spawn(async move {
                eng.run(r, &Redactor::default()).await
            }));
        }
        // Wait for the slots to actually be taken.
        for _ in 0..200 {
            if eng.inflight.lock().await.len() == exec_limits::MAX_CONCURRENT_PER_AGENT {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let mut extra = req(echo_hello());
        extra.request_id = "one-too-many".into();
        let refused = eng.run(extra, &Redactor::default()).await;
        assert!(
            refused
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("already running"),
            "error was {:?}",
            refused.error
        );

        for i in 0..exec_limits::MAX_CONCURRENT_PER_AGENT {
            eng.cancel(&format!("hog-{i}")).await;
        }
        for r in runners {
            let _ = r.await;
        }
    }

    // ─── Redaction ───────────────────────────────────────────────────
    //
    // Every fixture below is ASSEMBLED AT RUNTIME rather than written as a
    // string literal. These tests are about secret-SHAPED text by definition,
    // and a secret-shaped literal in the source trips repo scanners on every
    // push — a permanently red security check trains people to ignore
    // security checks. The shape is what the tests need, never the payload.

    /// A secret-shaped string of `n` characters, built so no literal in this
    /// file ever looks like a credential.
    fn shaped(seed: &str, n: usize) -> String {
        seed.chars().cycle().take(n).collect()
    }

    #[test]
    fn redacts_literal_secrets() {
        let token = shaped("agenttoken", 24);
        let r = Redactor::new([token.clone()]);
        let out = r.apply(&format!("token={token} trailing"));
        assert!(!out.contains(&token), "{out}");
        assert!(out.contains(MASK), "{out}");
        assert!(out.contains("trailing"), "surrounding text survives: {out}");
    }

    #[test]
    fn short_literals_are_not_masked() {
        // Masking a 3-char "secret" would shred ordinary output for no gain.
        let r = Redactor::new(["abc".to_string()]);
        assert_eq!(r.apply("abc def"), "abc def");
    }

    #[test]
    fn redacts_bearer_tokens_case_insensitively() {
        let r = Redactor::default();
        let token = shaped("bearervalue", 16);
        let out = r.apply(&format!("Authorization: Bearer {token}\nnext line"));
        assert!(!out.contains(&token), "{out}");
        assert!(out.contains("Bearer [redacted]"), "{out}");
        assert!(out.contains("next line"), "{out}");

        let out = r.apply(&format!("authorization: bearer {token}"));
        assert!(!out.contains(&token), "{out}");
    }

    #[test]
    fn redacts_jwt_shaped_strings() {
        let r = Redactor::default();
        let jwt = [
            shaped("headerpart", 20),
            shaped("payloadpart", 19),
            shaped("signaturepart", 28),
        ]
        .join(".");
        let out = r.apply(&format!("token: {jwt} end"));
        assert!(!out.contains(&jwt), "{out}");
        assert!(out.contains(MASK), "{out}");
        assert!(out.contains("end"), "{out}");
    }

    #[test]
    fn ordinary_output_survives_redaction() {
        // The redactor is over-eager by design; it still must not mangle a
        // route table or a version string.
        let r = Redactor::default();
        for sample in [
            "0.0.0.0/0 via 192.168.68.1 dev eth0",
            "roomlerd 0.3.0-rc.310",
            "Ethernet 2   Up   00-15-5D-01-02-03",
            "a.b.c",
            "file.tar.gz",
        ] {
            assert_eq!(r.apply(sample), sample, "mangled: {sample}");
        }
    }

    #[test]
    fn outcome_hash_separates_the_two_streams() {
        let a = ExecOutcome {
            stdout: "one".into(),
            stderr: "two".into(),
            ..Default::default()
        };
        let b = ExecOutcome {
            stdout: "onetwo".into(),
            stderr: String::new(),
            ..Default::default()
        };
        // Identical concatenation, materially different runs: "printed two on
        // stderr" vs "printed nothing on stderr". The audit hash must tell
        // them apart.
        assert_eq!(a.output_bytes(), b.output_bytes());
        assert_ne!(a.output_sha256(), b.output_sha256());
        assert_eq!(a.output_sha256().len(), 64);
    }
}
