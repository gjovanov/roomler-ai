// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P1a — a session's toolbelt: the `roomler` MCP server, one per
//! session, and in P1 its one tool, `approve` — Claude Code's
//! `--permission-prompt-tool`, which it calls and waits on before every tool
//! call nothing else allowed (`docs/roomler-hive-design.md` §7.1).
//!
//! # How the harness reaches it
//!
//! Claude Code starts its MCP servers itself, as the session's account, and
//! talks to them over stdio. So the server it starts is a relay — `roomlerd
//! hive-mcp <socket>` ([`relay`]) — that pipes its stdin and stdout to the
//! session's socket, and the daemon speaks MCP on the other end. The socket
//! is `<runtime>/<sid>/toolbelt.sock`: owned by the session's account, `0600`,
//! in a directory only the daemon writes, so the kernel admits that account
//! and root; the daemon checks the peer's uid again on every connection. It
//! is deliberately not the LocalAPI socket: on a root daemon that one is
//! root-only, and most of its verbs trust the socket alone.
//!
//! On Windows (P1i-2) it is a named pipe, `\\.\pipe\roomler-hive-<session>-<nonce>`,
//! whose DACL admits SYSTEM and Administrators (the daemon is one or the
//! other) and the session's account by SID, to read and write only, and whose
//! clients' accounts are checked again by SID.
//!
//! # An approval
//!
//! `tools/call approve {tool_name, input, tool_use_id}` opens an approval:
//! the session records it, reports `awaiting_approval`, and tells its
//! viewers; a DRIVER answers over the viewer peer; the call returns
//! `{"behavior":"allow","updatedInput":…}` or `{"behavior":"deny","message":…}`
//! as one text block — the contract pinned against Claude Code 2.1.293 (FR-90
//! spec §8). Nobody answering within [`APPROVAL_TIMEOUT`] is a denial that
//! says so; the harness letting go (`notifications/cancelled`, a closed relay)
//! or the session ending withdraws it.
//!
//! ⚠️ What fails, fails CLOSED: a relay that cannot reach the socket leaves
//! Claude Code without its permission tool, and every call that needs one
//! then errors and the harness exits (measured: "MCP tool
//! mcp__roomler__approve … not found", exit 1). Nothing runs unapproved.
//!
//! ⚠️ Claude Code's permission prompts are UX, not the boundary: the device's
//! own gates — the mapped account, `hive_roots` — are. A process running as
//! the session's account can connect here too; all it can do is ASK, and the
//! answer goes back to whoever asked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use bson::oid::ObjectId;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::lines::LineReader;
use super::supervisor::Author;

/// How long an approval waits for a person before it is a denial. Under the
/// tool-call timeout the session's MCP config sets ([`TOOL_TIMEOUT_MS`]), so
/// what the model reads is our answer, never a transport error.
pub(crate) const APPROVAL_TIMEOUT: Duration = Duration::from_secs(25 * 60);
/// The toolbelt's tool-call timeout in the session's MCP config.
pub(crate) const TOOL_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// Approvals one session holds open at once. Claude Code asks one at a time
/// (measured: two parallel writes were two calls, the second after the
/// first was answered), so more than this is not Claude Code.
const MAX_PENDING: usize = 4;
/// The largest MCP message taken from the harness: a `Write` approval
/// carries the whole file.
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
/// Progress while an approval waits, so no idle timer on the harness's side
/// takes a person thinking for a server that died.
const PROGRESS_EVERY: Duration = Duration::from_secs(60);
/// The relay's subcommand: `roomlerd hive-mcp <socket>`.
pub const RELAY_SUBCOMMAND: &str = "hive-mcp";
#[cfg(unix)]
const SOCKET: &str = "toolbelt.sock";
/// The MCP versions this server answers in. It uses `initialize`,
/// `tools/list`, `tools/call`, `ping` and two notifications, which read the
/// same in each.
const PROTOCOL_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
/// Longest tool name or tool-use id kept from a call.
const MAX_NAME: usize = 256;

/// What a driver decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    /// `message` goes to the model with the denial.
    Deny {
        message: Option<String>,
    },
}

/// A driver's answer to one approval.
#[derive(Debug, Clone)]
pub(crate) struct Answer {
    pub decision: Decision,
    pub by: Author,
}

/// How an approval ended.
#[derive(Debug, Clone)]
pub(crate) enum Ended {
    Answered(Answer),
    /// Nobody answered within the timeout.
    Expired,
    /// The harness let go, or the session ended, first.
    Withdrawn,
}

/// What the toolbelt tells its session's task — in order, on one channel,
/// so the transcript and the run state stay the task's alone to write.
#[derive(Debug)]
pub(crate) enum ApprovalEvent {
    Opened {
        id: String,
        tool_name: String,
        tool_use_id: Option<String>,
        input: Value,
    },
    Closed {
        id: String,
        ended: Ended,
    },
}

/// A session's open approvals: the toolbelt opens them; a driver's answer,
/// the timeout, or the session's end closes them.
#[derive(Default)]
pub(crate) struct Pending {
    map: Mutex<HashMap<String, oneshot::Sender<Answer>>>,
}

impl Pending {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<Answer>>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Open `id`; `None` when [`MAX_PENDING`] are open already.
    fn open(&self, id: &str) -> Option<oneshot::Receiver<Answer>> {
        let mut map = self.lock();
        if map.len() >= MAX_PENDING {
            return None;
        }
        let (tx, rx) = oneshot::channel();
        map.insert(id.to_string(), tx);
        Some(rx)
    }

    /// Answer `id`. `false` when it is not open — answered already, expired,
    /// withdrawn, or never this session's.
    pub(crate) fn answer(&self, id: &str, answer: Answer) -> bool {
        match self.lock().remove(id) {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }

    /// The ids open now.
    pub(crate) fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().keys().cloned().collect();
        ids.sort();
        ids
    }

    fn forget(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Withdraw every open approval: their calls see the answer channel close.
    fn withdraw_all(&self) {
        self.lock().clear();
    }
}

/// How long an approval waits, and how often it says it still is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    pub timeout: Duration,
    pub progress_every: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            timeout: APPROVAL_TIMEOUT,
            progress_every: PROGRESS_EVERY,
        }
    }
}

/// The one account a connection is taken from, the session's: its uid on
/// Unix, its SID string on Windows (P1i-2).
#[cfg(unix)]
pub(crate) type PeerId = u32;
#[cfg(windows)]
pub(crate) type PeerId = String;

/// What every connection of one session's toolbelt shares.
struct Ctx {
    session: ObjectId,
    /// The only account a connection is taken from: the session's.
    peer: PeerId,
    pending: Arc<Pending>,
    events: mpsc::Sender<ApprovalEvent>,
    timing: Timing,
}

/// One session's toolbelt: its socket, its open approvals, and the switch
/// that ends both. Dropping it is the shutdown — the session task owns it,
/// so the toolbelt ends exactly when the session does.
pub(crate) struct Toolbelt {
    /// The socket's path; on Windows the pipe's name (`\\.\pipe\…`).
    socket: PathBuf,
    pending: Arc<Pending>,
    stop: CancellationToken,
}

impl Toolbelt {
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    pub(crate) fn pending(&self) -> Arc<Pending> {
        Arc::clone(&self.pending)
    }
}

impl Drop for Toolbelt {
    fn drop(&mut self) {
        self.stop.cancel();
        self.pending.withdraw_all();
        // A pipe is gone with its last instance; a socket stays a file.
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Bind `dir/toolbelt.sock` for one session and start serving it.
///
/// `owner` is the account the socket is handed to (`None` keeps the
/// daemon's, for a session that runs as the daemon — tests only); `uid` is
/// the one peer a connection is taken from.
#[cfg(unix)]
pub(crate) fn open(
    dir: &Path,
    owner: Option<(u32, u32)>,
    uid: u32,
    session: ObjectId,
    events: mpsc::Sender<ApprovalEvent>,
    timing: Timing,
) -> Result<Toolbelt, String> {
    let socket = dir.join(SOCKET);
    // A socket left by an earlier run of the same session is removed; anything
    // else in its place — a link, a file — is refused, never followed.
    match std::fs::symlink_metadata(&socket) {
        Ok(m) => {
            use std::os::unix::fs::FileTypeExt;
            if !m.file_type().is_socket() {
                return Err(format!("{} is not a socket", socket.display()));
            }
            std::fs::remove_file(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", socket.display())),
    }
    let listener = UnixListener::bind(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    // Bound under the daemon's umask: until these two calls, only the daemon
    // can connect. Then the session's account, and nobody else.
    let handed = owner
        .map(|(u, g)| std::os::unix::fs::chown(&socket, Some(u), Some(g)))
        .unwrap_or(Ok(()))
        .and_then(|()| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        });
    if let Err(e) = handed {
        let _ = std::fs::remove_file(&socket);
        return Err(format!("{}: {e}", socket.display()));
    }
    let pending = Arc::new(Pending::default());
    let stop = CancellationToken::new();
    let ctx = Arc::new(Ctx {
        session,
        peer: uid,
        pending: Arc::clone(&pending),
        events,
        timing,
    });
    tokio::spawn(serve(listener, ctx, stop.clone()));
    Ok(Toolbelt {
        socket,
        pending,
        stop,
    })
}

#[cfg(unix)]
async fn serve(listener: UnixListener, ctx: Arc<Ctx>, stop: CancellationToken) {
    loop {
        let got = tokio::select! {
            _ = stop.cancelled() => return,
            got = listener.accept() => got,
        };
        match got {
            Ok((stream, _)) => match stream.peer_cred() {
                Ok(c) if c.uid() == ctx.peer => {
                    let (rd, wr) = stream.into_split();
                    tokio::spawn(connection(rd, wr, Arc::clone(&ctx), stop.child_token()));
                }
                Ok(c) => warn!(
                    session = %ctx.session, uid = c.uid(),
                    "hive: a toolbelt connection from another account — refused"
                ),
                Err(e) => {
                    warn!(session = %ctx.session, %e, "hive: a toolbelt peer could not be identified — refused")
                }
            },
            Err(e) => {
                // Out of descriptors and the like: back off rather than spin.
                warn!(session = %ctx.session, %e, "hive: the toolbelt could not accept");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// P1i-2 — one session's toolbelt on Windows: a named pipe,
/// `\\.\pipe\roomler-hive-<session>-<nonce>`, made with
/// `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a name someone already holds is
/// refused, never joined. Its DACL ([`crate::hive_win::toolbelt_pipe_sddl`])
/// admits SYSTEM and Administrators (the daemon is one or the other), and the
/// session's account (`peer`, a SID) to read and write, and no remote client.
/// Each client's account is checked again, as Unix checks a uid, so an
/// administrator connecting is taken only when it is the session's account.
#[cfg(windows)]
pub(crate) fn open(
    peer: PeerId,
    session: ObjectId,
    events: mpsc::Sender<ApprovalEvent>,
    timing: Timing,
) -> Result<Toolbelt, String> {
    let name = format!(
        r"\\.\pipe\roomler-hive-{}-{}",
        session.to_hex(),
        hex::encode(rand::random::<[u8; 16]>())
    );
    let sddl = crate::hive_win::toolbelt_pipe_sddl(&peer);
    let first = pipe_instance(&name, &sddl, true).map_err(|e| format!("{name}: {e}"))?;
    let pending = Arc::new(Pending::default());
    let stop = CancellationToken::new();
    let ctx = Arc::new(Ctx {
        session,
        peer,
        pending: Arc::clone(&pending),
        events,
        timing,
    });
    tokio::spawn(serve_pipe(first, name.clone(), sddl, ctx, stop.clone()));
    Ok(Toolbelt {
        socket: PathBuf::from(name),
        pending,
        stop,
    })
}

/// One listening instance of a toolbelt's pipe.
#[cfg(windows)]
fn pipe_instance(name: &str, sddl: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    let mut sd = crate::hive_win::Sddl::new(sddl)?;
    // SAFETY: the attributes live in `sd` across the call, and the system
    // copies the descriptor into the instance.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, sd.attributes().cast())
    }
}

#[cfg(windows)]
async fn serve_pipe(
    mut server: NamedPipeServer,
    name: String,
    sddl: String,
    ctx: Arc<Ctx>,
    stop: CancellationToken,
) {
    use std::os::windows::io::AsRawHandle;
    loop {
        let got = tokio::select! {
            _ = stop.cancelled() => return,
            got = server.connect() => got,
        };
        // The next instance before this client is looked at, so a second
        // connection always finds one listening.
        let next = match pipe_instance(&name, &sddl, false) {
            Ok(next) => next,
            Err(e) => {
                warn!(session = %ctx.session, %e, "hive: the toolbelt pipe takes no more connections");
                return;
            }
        };
        let client = std::mem::replace(&mut server, next);
        if let Err(e) = got {
            warn!(session = %ctx.session, %e, "hive: the toolbelt could not accept");
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        match crate::hive_win::pipe_client_sid(client.as_raw_handle() as _) {
            Ok(sid) if sid == ctx.peer => {
                let (rd, wr) = tokio::io::split(client);
                tokio::spawn(connection(rd, wr, Arc::clone(&ctx), stop.child_token()));
            }
            Ok(sid) => warn!(
                session = %ctx.session, %sid,
                "hive: a toolbelt connection from another account — refused"
            ),
            Err(e) => {
                warn!(session = %ctx.session, %e, "hive: a toolbelt peer could not be identified — refused")
            }
        }
    }
}

fn reply(id: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn error(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

/// The one tool, as `tools/list` describes it. Claude Code keeps the
/// permission-prompt tool out of the model's own tool list (measured), so
/// this is read by the harness, not the model.
fn approve_tool() -> Value {
    json!({
        "name": "approve",
        "description": "Asks a driver of this Roomler session whether a tool call may run. \
                        Claude Code calls it before any tool call that needs approval.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tool_name": {"type": "string"},
                "input": {"type": "object"},
                "tool_use_id": {"type": "string"},
            },
            "required": ["tool_name", "input"],
        },
    })
}

/// The `initialize` result: the client's version when this server knows it,
/// else the newest it does.
fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[PROTOCOL_VERSIONS.len() - 1]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "roomler", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// In-flight calls on one connection, by request id, so a cancellation can
/// name one.
type Calls = Arc<Mutex<HashMap<String, CancellationToken>>>;

async fn connection(
    rd: impl AsyncRead + Unpin + Send + 'static,
    mut wr: impl AsyncWrite + Unpin + Send + 'static,
    ctx: Arc<Ctx>,
    stop: CancellationToken,
) {
    let (out, mut outbox) = mpsc::channel::<String>(64);
    let writer = tokio::spawn(async move {
        while let Some(line) = outbox.recv().await {
            if wr.write_all(line.as_bytes()).await.is_err() || wr.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });
    let calls: Calls = Default::default();
    let mut lines = LineReader::new(BufReader::new(rd), MAX_MESSAGE);
    loop {
        let line = tokio::select! {
            _ = stop.cancelled() => break,
            line = lines.next_line() => line,
        };
        let line = match line {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => break,
        };
        if line.is_empty() {
            // Blank, or longer than any message this server takes.
            continue;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
            let _ = out.send(error(&Value::Null, -32700, "not JSON")).await;
            continue;
        };
        on_message(&ctx, &out, &calls, &stop, msg).await;
    }
    // The harness let go: whatever it was still waiting for is withdrawn.
    let waiting: Vec<CancellationToken> = calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .map(|(_, t)| t)
        .collect();
    for t in waiting {
        t.cancel();
    }
    drop(out);
    let _ = writer.await;
}

async fn on_message(
    ctx: &Arc<Ctx>,
    out: &mpsc::Sender<String>,
    calls: &Calls,
    stop: &CancellationToken,
    msg: Value,
) {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").filter(|i| !i.is_null()).cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let (Some(method), Some(id)) = (method, id) else {
        // A notification, or a response to a request this server never
        // makes.
        if method == Some("notifications/cancelled") {
            let named = params.get("requestId").map(Value::to_string);
            if let Some(key) = named
                && let Some(t) = calls.lock().unwrap_or_else(|e| e.into_inner()).remove(&key)
            {
                t.cancel();
            }
        }
        return;
    };
    let answer = match method {
        "initialize" => reply(&id, initialize(&params)),
        "ping" => reply(&id, json!({})),
        "tools/list" => reply(&id, json!({"tools": [approve_tool()]})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name != "approve" {
                error(&id, -32602, "unknown tool")
            } else {
                let cancel = stop.child_token();
                calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.to_string(), cancel.clone());
                let progress = params
                    .get("_meta")
                    .and_then(|m| m.get("progressToken"))
                    .cloned();
                tokio::spawn(approve(
                    Arc::clone(ctx),
                    id,
                    params.get("arguments").cloned().unwrap_or(Value::Null),
                    progress,
                    out.clone(),
                    Arc::clone(calls),
                    cancel,
                ));
                return;
            }
        }
        // `server/discover` among them: Claude Code probes with it and, told
        // no, initializes (measured). Silence here would stall its start.
        _ => error(&id, -32601, "method not found"),
    };
    let _ = out.send(answer).await;
}

fn new_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

fn clamp(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_NAME)
        .collect()
}

/// What the model is told for each way an approval ends but "allowed".
fn denial(ended: &Ended, timeout: Duration) -> String {
    match ended {
        Ended::Answered(Answer {
            decision: Decision::Deny { message: Some(m) },
            by,
        }) => format!("{} denied this: {m}", by.name),
        Ended::Answered(Answer { by, .. }) => format!("{} denied this tool call.", by.name),
        Ended::Expired => {
            let secs = timeout.as_secs();
            let waited = if secs >= 60 {
                format!("{} minutes", secs / 60)
            } else {
                format!("{secs} seconds")
            };
            format!(
                "Nobody answered this approval within {waited}, so it did not run. If it is \
                 still needed, say so in your reply."
            )
        }
        Ended::Withdrawn => "This approval was withdrawn before anyone answered.".to_string(),
    }
}

/// One `approve` call, from asking to answering.
#[allow(clippy::too_many_arguments)]
async fn approve(
    ctx: Arc<Ctx>,
    id: Value,
    args: Value,
    progress: Option<Value>,
    out: mpsc::Sender<String>,
    calls: Calls,
    cancel: CancellationToken,
) {
    let key = id.to_string();
    let verdict = run_approval(&ctx, &args, progress.as_ref(), &out, &cancel).await;
    calls.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
    // A cancelled request gets no answer (MCP); a closed one cannot.
    if !cancel.is_cancelled() {
        let text = verdict.to_string();
        let _ = out
            .send(reply(
                &id,
                json!({"content": [{"type": "text", "text": text}]}),
            ))
            .await;
    }
}

/// Open the approval, wait for its end, record it, and say what the harness
/// is to do.
async fn run_approval(
    ctx: &Ctx,
    args: &Value,
    progress: Option<&Value>,
    out: &mpsc::Sender<String>,
    cancel: &CancellationToken,
) -> Value {
    let deny = |message: String| json!({"behavior": "deny", "message": message});
    let tool_name = clamp(args.get("tool_name").and_then(Value::as_str).unwrap_or(""));
    if tool_name.is_empty() {
        return deny("The approval request named no tool.".into());
    }
    // Taken out of the model's tools at launch; if it comes anyway, the
    // answer it needs is the person's choices, which no card here asks for.
    if tool_name == "AskUserQuestion" {
        return deny(
            "This session cannot show structured questions. Ask in plain text in your reply \
             instead."
                .into(),
        );
    }
    let input = args.get("input").cloned().unwrap_or_else(|| json!({}));
    let tool_use_id = args
        .get("tool_use_id")
        .and_then(Value::as_str)
        .map(clamp)
        .filter(|s| !s.is_empty());
    let approval = new_id();
    let Some(mut answered) = ctx.pending.open(&approval) else {
        warn!(session = %ctx.session, "hive: too many approvals open — one refused");
        return deny("Too many approvals are already waiting in this session.".into());
    };
    let opened = ApprovalEvent::Opened {
        id: approval.clone(),
        tool_name: tool_name.clone(),
        tool_use_id,
        input: input.clone(),
    };
    if ctx.events.send(opened).await.is_err() {
        // The session task is gone: the session is ending.
        ctx.pending.forget(&approval);
        return deny(denial(&Ended::Withdrawn, ctx.timing.timeout));
    }
    info!(session = %ctx.session, %approval, tool = %tool_name, "hive: approval asked");

    let deadline = tokio::time::sleep(ctx.timing.timeout);
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval_at(
        tokio::time::Instant::now() + ctx.timing.progress_every,
        ctx.timing.progress_every,
    );
    let mut step: u64 = 0;
    let mut ended = loop {
        tokio::select! {
            got = &mut answered => break match got {
                Ok(a) => Ended::Answered(a),
                Err(_) => Ended::Withdrawn,
            },
            _ = &mut deadline => break Ended::Expired,
            _ = cancel.cancelled() => break Ended::Withdrawn,
            _ = tick.tick(), if progress.is_some() => {
                step += 1;
                let note = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": progress,
                        "progress": step,
                        "message": "waiting for a person to answer",
                    },
                });
                let _ = out.send(note.to_string()).await;
            }
        }
    };
    ctx.pending.forget(&approval);
    // An answer that landed as the clock ran out still counts: the person
    // answered in time.
    if matches!(ended, Ended::Expired)
        && let Ok(a) = answered.try_recv()
    {
        ended = Ended::Answered(a);
    }
    let verdict = match &ended {
        Ended::Answered(Answer {
            decision: Decision::Allow,
            ..
        }) => json!({"behavior": "allow", "updatedInput": input}),
        other => deny(denial(other, ctx.timing.timeout)),
    };
    match &ended {
        Ended::Answered(a) => info!(
            session = %ctx.session, %approval, allowed = (a.decision == Decision::Allow),
            by = %a.by.user_id, "hive: approval answered"
        ),
        Ended::Expired => info!(session = %ctx.session, %approval, "hive: approval expired"),
        Ended::Withdrawn => debug!(session = %ctx.session, %approval, "hive: approval withdrawn"),
    }
    let _ = ctx
        .events
        .send(ApprovalEvent::Closed {
            id: approval,
            ended,
        })
        .await;
    verdict
}

// ─── The relay: what Claude Code starts as the session's MCP server ─────────

/// `roomlerd hive-mcp <socket>` → the socket, or `None` for every other
/// invocation. Read from raw argv, like the embedded CLI, so the relay runs
/// none of the daemon's start-up: it is started by Claude Code, as the
/// session's account, once per session.
pub fn relay_args() -> Option<PathBuf> {
    let mut it = std::env::args_os();
    let _exe = it.next()?;
    if it.next()? != RELAY_SUBCOMMAND {
        return None;
    }
    let socket = it.next()?;
    if it.next().is_some() {
        return None;
    }
    Some(PathBuf::from(socket))
}

/// Pipe this process's stdin and stdout to the session's toolbelt until
/// either side ends. The relay is bytes only: every message is read, bounded
/// and answered by the daemon.
pub async fn relay(socket: &Path) -> anyhow::Result<()> {
    relay_io(socket, tokio::io::stdin(), tokio::io::stdout()).await
}

/// The toolbelt's two halves for a client (the relay, a test): the socket.
#[cfg(unix)]
pub(crate) async fn connect(
    socket: &Path,
) -> std::io::Result<(
    tokio::net::unix::OwnedReadHalf,
    tokio::net::unix::OwnedWriteHalf,
)> {
    Ok(UnixStream::connect(socket).await?.into_split())
}

/// On Windows (P1i-2): the pipe, opened with exactly the access its DACL
/// grants. The instant every instance is taken (the server makes the next one
/// as it takes a client) is waited out, briefly.
#[cfg(windows)]
pub(crate) async fn connect(
    name: &Path,
) -> std::io::Result<(
    tokio::io::ReadHalf<NamedPipeClient>,
    tokio::io::WriteHalf<NamedPipeClient>,
)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match crate::hive_win::open_toolbelt_pipe(name) {
            Ok(c) => return Ok(tokio::io::split(c)),
            Err(e)
                if e.raw_os_error()
                    == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn relay_io(
    socket: &Path,
    mut input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    let (mut rd, mut wr) = connect(socket).await.with_context(|| {
        format!(
            "the session's toolbelt at {} is unreachable",
            socket.display()
        )
    })?;
    let up = async {
        let _ = tokio::io::copy(&mut input, &mut wr).await;
        let _ = wr.shutdown().await;
    };
    let down = async {
        let _ = tokio::io::copy(&mut rd, &mut output).await;
        let _ = output.flush().await;
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
