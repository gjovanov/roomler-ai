// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The toolbelt over a real socket (on Windows a named pipe, P1i-2), with the
//! test playing Claude Code: the messages it sends are the ones FR-90 P1a's
//! contract probe recorded from Claude Code 2.1.293 (spec §8).

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::*;

const WAIT: Duration = Duration::from_secs(10);

#[cfg(unix)]
fn me() -> PeerId {
    // SAFETY: getuid reads our own credentials.
    unsafe { libc::getuid() }
}

#[cfg(windows)]
fn me() -> PeerId {
    crate::hive_win::own_sid().unwrap()
}

struct Belt {
    _dir: tempfile::TempDir,
    toolbelt: Toolbelt,
    events: mpsc::Receiver<ApprovalEvent>,
}

fn belt(timing: Timing) -> Belt {
    belt_for(me(), timing)
}

fn belt_for(peer: PeerId, timing: Timing) -> Belt {
    belt_running(peer, run(), timing)
}

/// P3-4b — a toolbelt serving `run`.
fn belt_running(peer: PeerId, run: Run, timing: Timing) -> Belt {
    let dir = tempfile::tempdir().unwrap();
    let (tx, events) = mpsc::channel(64);
    #[cfg(unix)]
    let toolbelt = open(dir.path(), None, peer, run, tx, timing).unwrap();
    #[cfg(windows)]
    let toolbelt = open(peer, run, tx, timing).unwrap();
    Belt {
        _dir: dir,
        toolbelt,
        events,
    }
}

/// A run on a device that takes no leases: no vault tools.
fn run() -> Run {
    Run {
        session: ObjectId::new(),
        fence: 1,
        vault: None,
        #[cfg(feature = "vault")]
        exec: None,
    }
}

fn quick() -> Timing {
    Timing {
        timeout: Duration::from_secs(30),
        progress_every: Duration::from_secs(30),
    }
}

/// Claude Code's side of one connection — the harness, through its relay.
pub(crate) struct Client {
    rd: tokio::io::Lines<BufReader<Box<dyn AsyncRead + Unpin + Send>>>,
    wr: Box<dyn AsyncWrite + Unpin + Send>,
}

impl Client {
    async fn connect(b: &Belt) -> Self {
        Self::at(b.toolbelt.socket()).await
    }

    /// Connect to the toolbelt at `path` (a socket; on Windows a pipe name),
    /// as the relay does.
    pub(crate) async fn at(path: &Path) -> Self {
        let (rd, wr) = connect(path).await.unwrap();
        let rd: Box<dyn AsyncRead + Unpin + Send> = Box::new(rd);
        Self {
            rd: BufReader::new(rd).lines(),
            wr: Box::new(wr),
        }
    }

    pub(crate) async fn send(&mut self, v: Value) {
        let mut line = v.to_string();
        line.push('\n');
        self.wr.write_all(line.as_bytes()).await.unwrap();
    }

    pub(crate) async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(WAIT, self.rd.next_line())
            .await
            .expect("an answer in time")
            .unwrap()
            .expect("the connection is open");
        serde_json::from_str(&line).unwrap()
    }

    /// The `approve` verdict for request `id`, skipping progress.
    pub(crate) async fn verdict(&mut self, id: u64) -> Value {
        loop {
            let m = self.recv().await;
            if m.get("method") == Some(&json!("notifications/progress")) {
                continue;
            }
            assert_eq!(m["id"], id, "{m}");
            let text = m["result"]["content"][0]["text"].as_str().unwrap();
            return serde_json::from_str(text).unwrap();
        }
    }
}

/// Claude Code's `tools/call` for one permission prompt, as recorded.
pub(crate) fn call(id: u64, tool: &str, input: Value) -> Value {
    json!({
        "method": "tools/call",
        "params": {
            "name": "approve",
            "arguments": {"tool_name": tool, "input": input, "tool_use_id": "toolu_01FAKE"},
            "_meta": {"claudecode/toolUseId": "toolu_01FAKE", "progressToken": id},
        },
        "jsonrpc": "2.0",
        "id": id,
    })
}

async fn next_event(b: &mut Belt) -> ApprovalEvent {
    tokio::time::timeout(WAIT, b.events.recv())
        .await
        .expect("an approval event in time")
        .expect("the channel is open")
}

async fn opened(b: &mut Belt) -> (String, String, Value) {
    match next_event(b).await {
        ApprovalEvent::Opened {
            id,
            tool_name,
            input,
            ..
        } => (id, tool_name, input),
        other => panic!("expected an opened approval, got {other:?}"),
    }
}

async fn closed(b: &mut Belt) -> (String, Ended) {
    match next_event(b).await {
        ApprovalEvent::Closed { id, ended } => (id, ended),
        other => panic!("expected a closed approval, got {other:?}"),
    }
}

fn dev() -> Author {
    Author {
        user_id: ObjectId::new(),
        name: "Dev".into(),
    }
}

/// Claude Code's start-up, as recorded: a `server/discover` probe that must
/// be told no (it then initializes), `initialize`, `notifications/initialized`,
/// `tools/list`.
#[tokio::test]
async fn the_server_answers_the_handshake_claude_code_sends() {
    let b = belt(quick());
    let mut c = Client::connect(&b).await;
    c.send(json!({"jsonrpc": "2.0", "id": "server-discover-probe-1", "method": "server/discover", "params": {}}))
        .await;
    let probe = c.recv().await;
    assert_eq!(probe["id"], "server-discover-probe-1");
    assert_eq!(probe["error"]["code"], -32601, "{probe}");

    c.send(json!({"method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "2.1.293"}}, "jsonrpc": "2.0", "id": 0}))
        .await;
    let init = c.recv().await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(init["result"]["serverInfo"]["name"], "roomler");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    c.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    c.send(json!({"method": "tools/list", "jsonrpc": "2.0", "id": 1}))
        .await;
    let list = c.recv().await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "{list}");
    assert_eq!(tools[0]["name"], "approve");
    assert_eq!(
        tools[0]["inputSchema"]["required"],
        json!(["tool_name", "input"])
    );

    // A version from the future is answered in the newest this server
    // speaks; ping, an unknown tool and garbage each get their own error.
    c.send(json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {"protocolVersion": "2099-01-01"}}))
        .await;
    assert_eq!(c.recv().await["result"]["protocolVersion"], "2025-11-25");
    c.send(json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}))
        .await;
    assert_eq!(c.recv().await["result"], json!({}));
    c.send(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "exec", "arguments": {}}}))
        .await;
    assert_eq!(c.recv().await["error"]["code"], -32602);
    c.wr.write_all(b"{not json\n").await.unwrap();
    assert_eq!(c.recv().await["error"]["code"], -32700);
}

/// Allowed: the call returns the input it asked about, unchanged. Denied:
/// the model reads who said no, and what they said.
#[tokio::test]
async fn an_allowed_call_runs_as_asked_and_a_denied_one_is_told_why() {
    let mut b = belt(quick());
    let mut c = Client::connect(&b).await;
    let input = json!({"file_path": "/tmp/x.txt", "content": "hello\n"});
    c.send(call(2, "Write", input.clone())).await;
    let (id, tool, asked) = opened(&mut b).await;
    assert_eq!((tool.as_str(), &asked), ("Write", &input));
    assert_eq!(b.toolbelt.pending().ids(), std::slice::from_ref(&id));
    assert!(b.toolbelt.pending().answer(
        &id,
        Answer {
            decision: Decision::Allow,
            by: dev(),
        }
    ));
    assert_eq!(
        c.verdict(2).await,
        json!({"behavior": "allow", "updatedInput": input})
    );
    let (closed_id, ended) = closed(&mut b).await;
    assert_eq!(closed_id, id);
    assert!(matches!(
        ended,
        Ended::Answered(Answer {
            decision: Decision::Allow,
            ..
        })
    ));
    assert!(b.toolbelt.pending().ids().is_empty());
    assert!(
        !b.toolbelt.pending().answer(
            &id,
            Answer {
                decision: Decision::Allow,
                by: dev()
            }
        ),
        "an answered approval takes no second answer"
    );

    c.send(call(3, "Bash", json!({"command": "rm -rf build"})))
        .await;
    let (id, _, _) = opened(&mut b).await;
    b.toolbelt.pending().answer(
        &id,
        Answer {
            decision: Decision::Deny {
                message: Some("not now".into()),
            },
            by: dev(),
        },
    );
    assert_eq!(
        c.verdict(3).await,
        json!({"behavior": "deny", "message": "Dev denied this: not now"})
    );
    assert!(matches!(closed(&mut b).await.1, Ended::Answered(_)));
}

/// Nobody answering is a denial that says so, and the waiting call says it is
/// still waiting meanwhile.
#[tokio::test]
async fn nobody_answering_is_a_denial_that_says_so() {
    let mut b = belt(Timing {
        timeout: Duration::from_millis(400),
        progress_every: Duration::from_millis(100),
    });
    let mut c = Client::connect(&b).await;
    c.send(call(7, "Bash", json!({"command": "make"}))).await;
    let (id, _, _) = opened(&mut b).await;
    let first = c.recv().await;
    assert_eq!(first["method"], "notifications/progress", "{first}");
    assert_eq!(first["params"]["progressToken"], 7);
    let v = c.verdict(7).await;
    assert_eq!(v["behavior"], "deny");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .starts_with("Nobody answered"),
        "{v}"
    );
    let (closed_id, ended) = closed(&mut b).await;
    assert_eq!(closed_id, id);
    assert!(matches!(ended, Ended::Expired));
}

/// The harness letting go withdraws what it asked: a cancellation gets no
/// answer (MCP), and a closed relay takes every open call with it.
#[tokio::test]
async fn a_cancelled_or_abandoned_call_is_withdrawn() {
    let mut b = belt(quick());
    let mut c = Client::connect(&b).await;
    c.send(call(5, "Bash", json!({"command": "sleep 1"}))).await;
    opened(&mut b).await;
    c.send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 5, "reason": "interrupted"}}))
        .await;
    assert!(matches!(closed(&mut b).await.1, Ended::Withdrawn));
    c.send(json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}))
        .await;
    assert_eq!(
        c.recv().await["id"],
        6,
        "the cancelled call was not answered; the next request was"
    );

    c.send(call(8, "Bash", json!({"command": "sleep 2"}))).await;
    opened(&mut b).await;
    drop(c);
    assert!(matches!(closed(&mut b).await.1, Ended::Withdrawn));
    assert!(b.toolbelt.pending().ids().is_empty());
}

/// Ending the toolbelt — the session ending — withdraws what is open and
/// takes the socket away.
#[tokio::test]
async fn the_toolbelt_ends_with_its_session() {
    let mut b = belt(quick());
    let mut c = Client::connect(&b).await;
    c.send(call(2, "Bash", json!({"command": "ls"}))).await;
    opened(&mut b).await;
    let socket = b.toolbelt.socket().to_path_buf();
    let Belt {
        _dir,
        toolbelt,
        mut events,
    } = b;
    drop(toolbelt);
    let ended = tokio::time::timeout(WAIT, events.recv()).await.unwrap();
    assert!(matches!(
        ended,
        Some(ApprovalEvent::Closed {
            ended: Ended::Withdrawn,
            ..
        })
    ));
    #[cfg(unix)]
    assert!(!socket.exists(), "the socket went with the session");
    // A pipe is gone once its last instance is: the listening one, and the
    // connected one whose task the stop ended.
    #[cfg(windows)]
    {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            match connect(&socket).await {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                other => {
                    drop(other);
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "the pipe went with the session"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
}

/// P1i-2 — a toolbelt is its session account's alone on Windows too. One made
/// for another account (SYSTEM's, here) drops this one's connection unanswered:
/// its DACL admits this test process as an administrator (CI's runner is
/// elevated), and the daemon checks every client's account again. And its name
/// cannot be taken twice.
#[cfg(windows)]
#[tokio::test]
async fn the_pipe_is_the_sessions_alone() {
    let other = belt_for("S-1-5-18".into(), quick());
    let mut c = Client::connect(&other).await;
    // A write may already find the pipe closing: that is the refusal too.
    let _ =
        c.wr.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
            .await;
    let got = tokio::time::timeout(WAIT, c.rd.next_line()).await.unwrap();
    assert!(
        matches!(got, Ok(None) | Err(_)),
        "another account's connection is dropped: {got:?}"
    );

    let b = belt(quick());
    let name = b.toolbelt.socket().to_string_lossy().into_owned();
    let sddl = crate::hive_win::toolbelt_pipe_sddl(&me());
    assert!(
        pipe_instance(&name, &sddl, true).is_err(),
        "a name in use is never joined as the first"
    );
}

/// The socket is the session account's alone: `0600`; a connection from any
/// other uid is dropped unanswered; and what sits where the socket goes is
/// removed only if it is a socket.
#[cfg(unix)]
#[tokio::test]
async fn the_socket_is_the_sessions_alone() {
    let b = belt(quick());
    let mode = std::fs::metadata(b.toolbelt.socket())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    let other = belt_for(me().wrapping_add(1), quick());
    let mut c = Client::connect(&other).await;
    c.send(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .await;
    let got = tokio::time::timeout(WAIT, c.rd.next_line()).await.unwrap();
    assert!(
        matches!(got, Ok(None) | Err(_)),
        "another account's connection is dropped: {got:?}"
    );

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("toolbelt.sock"), b"not a socket").unwrap();
    let (tx, _rx) = mpsc::channel(4);
    let refused = open(dir.path(), None, me(), run(), tx.clone(), quick());
    assert!(refused.is_err(), "a file in the socket's place is refused");
    std::fs::remove_file(dir.path().join("toolbelt.sock")).unwrap();
    let first = open(dir.path(), None, me(), run(), tx.clone(), quick()).unwrap();
    std::mem::forget(first); // leaves a stale socket, as a crash would
    let again = open(dir.path(), None, me(), run(), tx, quick());
    assert!(again.is_ok(), "a stale socket is replaced");
}

/// P3-4b — a vault that answers as told and records what it was asked; with
/// `hold`, each answer waits until the test releases it. P3-4c-1: it leases
/// in `exec` mode the `values` it holds by `secret` or `secret#field`, and
/// remembers each lease it holds and each it was told to let go of.
#[derive(Default)]
struct FakeVault {
    asked: std::sync::Mutex<Vec<(String, ObjectId, u64, String)>>,
    hold: Option<tokio::sync::Notify>,
    #[cfg(feature = "vault")]
    values: HashMap<String, Vec<u8>>,
    #[cfg(feature = "vault")]
    held: std::sync::Mutex<HashMap<String, Vec<u8>>>,
    #[cfg(feature = "vault")]
    released: std::sync::Mutex<Vec<String>>,
    #[cfg(feature = "vault")]
    issued: std::sync::atomic::AtomicUsize,
    /// P3-4c-2 — each file a tool attached to a lease.
    #[cfg(feature = "vault")]
    attached: std::sync::Mutex<Vec<(String, std::path::PathBuf)>>,
    /// P3-4c-3 — each lease's time left (the TTL it was asked for, unless a
    /// test shortens it), and the leases asked for ahead: (id, handle, mode,
    /// audience).
    #[cfg(feature = "vault")]
    lefts: std::sync::Mutex<HashMap<String, Duration>>,
    #[cfg(feature = "vault")]
    ahead: std::sync::Mutex<Vec<(String, String, String, String)>>,
    /// P3-4d-2 — each use reported.
    #[cfg(feature = "vault")]
    reports: std::sync::Mutex<Vec<crate::vault::Report>>,
    /// P3-5b-2 — what each lease stops when it is let go.
    #[cfg(feature = "vault")]
    stops: std::sync::Mutex<Vec<(String, CancellationToken)>>,
}

impl FakeVault {
    fn asked(&self) -> Vec<(String, ObjectId, u64, String)> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl VaultTools for FakeVault {
    #[cfg(feature = "vault")]
    async fn lease(
        &self,
        session: ObjectId,
        fence: u64,
        handle: &super::super::vault_exec::Handle,
        mode: &str,
        audience: &str,
        ttl_secs: u32,
    ) -> Result<String, String> {
        let key = handle.to_string();
        self.asked.lock().unwrap().push((
            format!("lease_{mode}"),
            session,
            fence,
            format!("{key} for {audience} {ttl_secs}s"),
        ));
        let value = self
            .values
            .get(&key)
            .ok_or_else(|| "the vault refused: no_secret".to_string())?;
        let n = self
            .issued
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("L{n}");
        self.held.lock().unwrap().insert(id.clone(), value.clone());
        self.lefts
            .lock()
            .unwrap()
            .insert(id.clone(), Duration::from_secs(u64::from(ttl_secs)));
        Ok(id)
    }

    #[cfg(feature = "vault")]
    async fn lease_ahead(
        &self,
        session: ObjectId,
        fence: u64,
        handle: &super::super::vault_exec::Handle,
        mode: &str,
        audience: &str,
        ttl_secs: u32,
    ) -> Result<String, String> {
        let leased = self
            .lease(session, fence, handle, mode, audience, ttl_secs)
            .await;
        if let Some(last) = self.asked.lock().unwrap().last_mut() {
            last.0 = format!("ahead_{mode}");
        }
        let id = leased?;
        self.ahead.lock().unwrap().push((
            id.clone(),
            handle.to_string(),
            mode.into(),
            audience.into(),
        ));
        Ok(id)
    }

    #[cfg(feature = "vault")]
    fn ahead(
        &self,
        _session: ObjectId,
        _fence: u64,
        handle: &super::super::vault_exec::Handle,
        mode: &str,
        audience: &str,
        at_least: Duration,
    ) -> Option<String> {
        let key = handle.to_string();
        self.ahead
            .lock()
            .unwrap()
            .iter()
            .find(|(id, h, m, a)| {
                *h == key
                    && m == mode
                    && a == audience
                    && self.left(id).is_some_and(|l| l >= at_least)
            })
            .map(|(id, ..)| id.clone())
    }

    #[cfg(feature = "vault")]
    fn left(&self, lease_id: &str) -> Option<Duration> {
        if !self.held.lock().unwrap().contains_key(lease_id) {
            return None;
        }
        self.lefts.lock().unwrap().get(lease_id).copied()
    }

    #[cfg(feature = "vault")]
    fn report(&self, r: crate::vault::Report) {
        self.reports.lock().unwrap().push(r);
    }

    #[cfg(feature = "vault")]
    fn attach(&self, lease_id: &str, path: &Path) -> bool {
        if !self.held.lock().unwrap().contains_key(lease_id) {
            return false;
        }
        self.attached
            .lock()
            .unwrap()
            .push((lease_id.to_string(), path.to_path_buf()));
        true
    }

    #[cfg(feature = "vault")]
    fn stop_with(&self, lease_id: &str, stop: CancellationToken) -> bool {
        if !self.held.lock().unwrap().contains_key(lease_id) {
            return false;
        }
        self.stops
            .lock()
            .unwrap()
            .push((lease_id.to_string(), stop));
        true
    }

    #[cfg(feature = "vault")]
    fn value(&self, lease_id: &str) -> Option<zeroize::Zeroizing<Vec<u8>>> {
        self.held
            .lock()
            .unwrap()
            .get(lease_id)
            .map(|v| zeroize::Zeroizing::new(v.clone()))
    }

    #[cfg(feature = "vault")]
    fn release(&self, lease_ids: &[String]) {
        let mut held = self.held.lock().unwrap();
        for id in lease_ids {
            held.remove(id);
            self.released.lock().unwrap().push(id.clone());
            // What it served stops, as the daemon's client stops it.
            for (lease, stop) in self.stops.lock().unwrap().iter() {
                if lease == id {
                    stop.cancel();
                }
            }
        }
    }

    async fn list(&self, session: ObjectId, fence: u64, prefix: &str) -> Result<Value, String> {
        self.asked
            .lock()
            .unwrap()
            .push(("list".into(), session, fence, prefix.into()));
        if let Some(hold) = &self.hold {
            hold.notified().await;
        }
        if prefix == "refused/" {
            return Err("the vault refused: no_role".into());
        }
        Ok(json!({
            "secrets": [{"rrn": "rrn:grox:secret:db/main", "path": "db/main", "kind": "static"}],
            "more": false,
        }))
    }

    async fn describe(&self, session: ObjectId, fence: u64, secret: &str) -> Result<Value, String> {
        self.asked
            .lock()
            .unwrap()
            .push(("describe".into(), session, fence, secret.into()));
        Ok(json!({"rrn": format!("rrn:grox:secret:{secret}"), "path": secret, "kind": "static"}))
    }
}

/// A `tools/call` of one of the toolbelt's own tools, as Claude Code sends it.
fn tool_call(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments, "_meta": {"claudecode/toolUseId": "toolu_01FAKE"}},
        "jsonrpc": "2.0",
        "id": id,
    })
}

/// P3-4b — the vault tools are offered where the device takes leases, and
/// nowhere else: elsewhere they are unknown tools. Their settings names are
/// the names offered, under the `roomler` server.
#[tokio::test]
async fn the_vault_tools_are_offered_only_where_the_device_takes_leases() {
    let without = belt(quick());
    let mut c = Client::connect(&without).await;
    c.send(json!({"method": "tools/list", "jsonrpc": "2.0", "id": 1}))
        .await;
    let names: Vec<Value> = c.recv().await["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert_eq!(names, [json!("approve")]);
    c.send(tool_call(2, "vault_list", json!({}))).await;
    assert_eq!(c.recv().await["error"]["code"], -32602, "not offered here");

    let vault = Arc::new(FakeVault::default());
    let with = belt_running(
        me(),
        Run {
            session: ObjectId::new(),
            fence: 1,
            vault: Some(vault.clone()),
            #[cfg(feature = "vault")]
            exec: None,
        },
        quick(),
    );
    let mut c = Client::connect(&with).await;
    c.send(json!({"method": "tools/list", "jsonrpc": "2.0", "id": 1}))
        .await;
    let list = c.recv().await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names[..3], ["approve", "vault_list", "vault_describe"]);
    // P3-4c — and, in a build with the vault, its uses, (P3-5b-2) an
    // endpoint, and (P3-4c-3) a lease asked for ahead.
    #[cfg(feature = "vault")]
    assert_eq!(
        names[3..],
        [
            "vault_exec",
            "vault_render",
            "vault_connect",
            "vault_request"
        ]
    );
    #[cfg(not(feature = "vault"))]
    assert_eq!(names.len(), 3);
    for t in &tools[1..3] {
        assert_eq!(t["annotations"]["readOnlyHint"], true, "{t}");
    }
    assert_eq!(
        tools[2]["inputSchema"]["required"],
        json!(["secret"]),
        "{list}"
    );
    let named: Vec<String> = VAULT_TOOL_NAMES
        .iter()
        .map(|n| format!("mcp__roomler__{n}"))
        .collect();
    assert_eq!(
        named, VAULT_TOOLS,
        "what the settings allow is what is offered"
    );
    assert!(
        vault.asked().is_empty(),
        "listing the tools asks the vault nothing"
    );
}

/// P3-4b — a vault tool answers what the vault says for the session's run
/// (its session and its fence), as text, or why not, as an error; nothing
/// asks a person, and a call without its argument asks the vault nothing.
#[tokio::test]
async fn a_vault_tool_answers_what_the_vault_says_for_the_sessions_run() {
    let vault = Arc::new(FakeVault::default());
    let sid = ObjectId::new();
    let mut b = belt_running(
        me(),
        Run {
            session: sid,
            fence: 7,
            vault: Some(vault.clone()),
            #[cfg(feature = "vault")]
            exec: None,
        },
        quick(),
    );
    let mut c = Client::connect(&b).await;
    c.send(tool_call(1, "vault_list", json!({"prefix": "db/"})))
        .await;
    let listed = c.recv().await;
    assert_eq!(listed["id"], 1);
    assert!(listed["result"].get("isError").is_none(), "{listed}");
    let text: Value =
        serde_json::from_str(listed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["secrets"][0]["path"], "db/main");
    assert_eq!(text["more"], false);

    c.send(tool_call(2, "vault_describe", json!({"secret": "db/main"})))
        .await;
    let described = c.recv().await;
    let text: Value =
        serde_json::from_str(described["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["rrn"], "rrn:grox:secret:db/main");

    c.send(tool_call(3, "vault_describe", json!({}))).await;
    let missing = c.recv().await;
    assert_eq!(missing["result"]["isError"], true, "{missing}");
    assert!(
        missing["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("secret is required")
    );

    c.send(tool_call(4, "vault_list", json!({"prefix": "refused/"})))
        .await;
    let refused = c.recv().await;
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert_eq!(
        refused["result"]["content"][0]["text"],
        "the vault refused: no_role"
    );

    assert_eq!(
        vault.asked(),
        [
            ("list".to_string(), sid, 7, "db/".to_string()),
            ("describe".to_string(), sid, 7, "db/main".to_string()),
            ("list".to_string(), sid, 7, "refused/".to_string()),
        ]
    );
    assert!(
        b.events.try_recv().is_err(),
        "a vault tool opens no approval"
    );
}

/// P3-4b — a vault call the harness cancels gets no answer, as MCP asks,
/// and the next one is answered as usual.
#[tokio::test]
async fn a_cancelled_vault_call_gets_no_answer() {
    let vault = Arc::new(FakeVault {
        hold: Some(tokio::sync::Notify::new()),
        ..FakeVault::default()
    });
    let b = belt_running(
        me(),
        Run {
            session: ObjectId::new(),
            fence: 1,
            vault: Some(vault.clone()),
            #[cfg(feature = "vault")]
            exec: None,
        },
        quick(),
    );
    let mut c = Client::connect(&b).await;
    c.send(tool_call(5, "vault_list", json!({}))).await;
    tokio::time::timeout(WAIT, async {
        while vault.asked().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the call reached the vault");
    c.send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 5, "reason": "interrupted"}}))
        .await;
    c.send(json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}))
        .await;
    assert_eq!(
        c.recv().await["id"],
        6,
        "nothing answers the cancelled call"
    );
    // Released now: had the call outlived its cancellation, it would answer
    // here, and the next call would wait forever for a release.
    vault.hold.as_ref().unwrap().notify_one();
    c.send(tool_call(7, "vault_list", json!({}))).await;
    let next = c.recv().await;
    assert_eq!(next["id"], 7, "{next}");
    assert_eq!(vault.asked().len(), 2);
}

/// P3-4c-1 — a toolbelt whose run's commands run as this daemon, from `dir`,
/// with `values` in its vault.
#[cfg(all(feature = "vault", unix))]
fn exec_belt(dir: &Path, values: &[(&str, &[u8])]) -> (Belt, Arc<FakeVault>, ObjectId) {
    let vault = Arc::new(FakeVault {
        values: values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_vec()))
            .collect(),
        ..FakeVault::default()
    });
    let sid = ObjectId::new();
    // The run's own directory, as the supervisor makes it before a launch.
    std::fs::create_dir_all(dir.join("run")).unwrap();
    let belt = belt_running(
        me(),
        Run {
            session: sid,
            fence: 5,
            vault: Some(vault.clone()),
            exec: Some(super::super::vault_exec::ExecTarget {
                account: None,
                home: dir.to_path_buf(),
                folder: dir.to_path_buf(),
                session_dir: dir.join("run"),
                owner: None,
            }),
        },
        quick(),
    );
    (belt, vault, sid)
}

/// The answer to a `vault_exec` call: its JSON, or its error text.
#[cfg(all(feature = "vault", unix))]
async fn exec_answer(c: &mut Client, id: u64, args: Value) -> Result<Value, String> {
    c.send(tool_call(id, "vault_exec", args)).await;
    let m = c.recv().await;
    assert_eq!(m["id"], id, "{m}");
    let text = m["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    if m["result"]["isError"] == true {
        Err(text)
    } else {
        Ok(serde_json::from_str(&text).unwrap())
    }
}

/// P3-4c-1 — `vault_exec` is offered with the reads, and never allowed in
/// the settings: each call asks a person first.
#[cfg(feature = "vault")]
#[tokio::test]
async fn vault_exec_is_offered_and_never_allowed_unasked() {
    let vault = Arc::new(FakeVault::default());
    let b = belt_running(
        me(),
        Run {
            session: ObjectId::new(),
            fence: 1,
            vault: Some(vault.clone()),
            exec: None,
        },
        quick(),
    );
    let mut c = Client::connect(&b).await;
    c.send(json!({"method": "tools/list", "jsonrpc": "2.0", "id": 1}))
        .await;
    let list = c.recv().await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let exec = tools
        .iter()
        .find(|t| t["name"] == "vault_exec")
        .expect("offered");
    assert_eq!(exec["annotations"]["readOnlyHint"], false, "{exec}");
    assert_eq!(exec["inputSchema"]["required"], json!(["argv", "env"]));
    // P3-4c-2 — and `vault_render`, the same way.
    let render = tools
        .iter()
        .find(|t| t["name"] == "vault_render")
        .expect("offered");
    assert_eq!(render["annotations"]["readOnlyHint"], false, "{render}");
    assert_eq!(
        render["inputSchema"]["required"],
        json!(["template", "dest"])
    );
    // P3-5b-2 — and `vault_connect`: no value handed on, but it acts as the
    // secret's user.
    let connect = tools
        .iter()
        .find(|t| t["name"] == "vault_connect")
        .expect("offered");
    assert_eq!(connect["annotations"]["readOnlyHint"], false, "{connect}");
    assert_eq!(
        connect["inputSchema"]["required"],
        json!(["handle", "engine", "upstream"])
    );
    for tool in [
        "mcp__roomler__vault_exec",
        "mcp__roomler__vault_render",
        "mcp__roomler__vault_connect",
    ] {
        assert!(
            !VAULT_TOOLS.contains(&tool),
            "the settings never allow {tool}: a person approves every call"
        );
    }
    // And the settings ASK for each use offered, so no other settings file
    // can allow one past the person.
    let uses: Vec<String> = VAULT_USE_NAMES
        .iter()
        .map(|n| format!("mcp__roomler__{n}"))
        .collect();
    assert_eq!(
        uses, VAULT_USE_TOOLS,
        "what the settings ask for is every use offered"
    );
}

/// P3-4c-1 — a command runs as the session's run would, with the value in
/// its environment, from the run's folder; the value comes back scrubbed,
/// the lease is asked for that program and let go after.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn vault_exec_runs_with_the_value_and_answers_it_scrubbed() {
    let dir = tempfile::tempdir().unwrap();
    let value: &[u8] = b"s3cr3t-pw-81";
    let (b, vault, sid) = exec_belt(dir.path(), &[("db/main#password", value)]);
    let mut c = Client::connect(&b).await;
    // `CARGO_MANIFEST_DIR` is in this test's own environment: the child, run
    // with a clean one, must not see it.
    assert!(
        std::env::var_os("CARGO_MANIFEST_DIR").is_some(),
        "the daemon-side variable the child must not inherit"
    );
    let answer = exec_answer(
        &mut c,
        1,
        json!({
            "argv": ["sh", "-c", "printf '%s|%s|%s' \"$PW\" \"$(pwd)\" \"$CARGO_MANIFEST_DIR\""],
            "env": {"PW": "db/main#password"},
        }),
    )
    .await
    .unwrap();
    let here = dir.path().canonicalize().unwrap();
    assert_eq!(
        answer["stdout"],
        format!("[vault:PW]|{}|", here.display()),
        "{answer}"
    );
    assert_eq!(answer["exit_code"], 0);
    assert_eq!(answer["timed_out"], false);
    assert!(!answer.to_string().contains("s3cr3t"), "{answer}");
    assert_eq!(
        vault.asked(),
        [(
            "lease_exec".to_string(),
            sid,
            5,
            "db/main#password for sh 90s".to_string()
        )]
    );
    assert_eq!(*vault.released.lock().unwrap(), ["L0"]);
    assert!(
        vault.held.lock().unwrap().is_empty(),
        "let go after the run"
    );
    // P3-4d-2 — and the use is reported: the lease, the program, how it
    // ended; never a value.
    assert_eq!(
        *vault.reports.lock().unwrap(),
        [crate::vault::Report {
            session_id: sid,
            fence: 5,
            kind: roomler_ai_remote_control::vault::VaultActivityKind::Exec,
            lease_ids: vec!["L0".into()],
            target: "sh".into(),
            exit_code: Some(0),
            timed_out: false,
        }]
    );
}

/// P3-4c-1 — one refused lease runs nothing, and the leases already held are
/// let go; a path, a program nowhere to be found, or a reserved name is
/// refused before anything is asked.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn a_refusal_runs_nothing_and_a_bad_call_asks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (b, vault, _) = exec_belt(dir.path(), &[("db/main", b"first-value-1")]);
    let mut c = Client::connect(&b).await;
    let refused = exec_answer(
        &mut c,
        1,
        json!({"argv": ["sh", "-c", "touch ran"], "env": {"A": "db/main", "B": "db/missing"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(refused, "B: the vault refused: no_secret");
    assert!(!dir.path().join("ran").exists(), "nothing ran");
    assert_eq!(*vault.released.lock().unwrap(), ["L0"]);
    assert!(vault.held.lock().unwrap().is_empty());
    let asked = vault.asked().len();
    for (args, says) in [
        (
            json!({"argv": ["/bin/sh", "-c", "true"], "env": {"A": "db/main"}}),
            "bare name",
        ),
        (
            json!({"argv": ["no-such-program-p34c"], "env": {"A": "db/main"}}),
            "was not found",
        ),
        (
            json!({"argv": ["sh"], "env": {"LD_PRELOAD": "db/main"}}),
            "cannot be handed",
        ),
        (
            json!({"argv": ["sh"], "env": {"PATH": "db/main"}}),
            "cannot be handed",
        ),
        (json!({"argv": ["sh"], "env": {}}), "env must map"),
        (
            json!({"argv": ["sh"], "env": {"A": "db/main"}, "timeout_s": 0}),
            "timeout_s",
        ),
    ] {
        let why = exec_answer(&mut c, 2, args.clone()).await.unwrap_err();
        assert!(why.contains(says), "{args}: {why}");
    }
    assert_eq!(
        vault.asked().len(),
        asked,
        "a bad call asks the vault nothing"
    );
    assert!(
        vault.reports.lock().unwrap().is_empty(),
        "nothing ran, nothing is reported"
    );
}

/// P3-4c-1 — the timeout kills the command and everything it started, and
/// the lease is let go.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn a_command_past_its_timeout_is_killed_with_its_group() {
    let dir = tempfile::tempdir().unwrap();
    let (b, vault, _) = exec_belt(dir.path(), &[("db/main", b"slow-value-22")]);
    let mut c = Client::connect(&b).await;
    let started = std::time::Instant::now();
    // A child that would touch the file two seconds in, had the group been
    // left alive past the one-second timeout.
    let answer = exec_answer(
        &mut c,
        1,
        json!({
            "argv": ["sh", "-c", "(sleep 2; touch survived) & sleep 30"],
            "env": {"A": "db/main"},
            "timeout_s": 1,
        }),
    )
    .await
    .unwrap();
    assert_eq!(answer["timed_out"], true, "{answer}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(vault.held.lock().unwrap().is_empty());
    // P3-4d-2 — reported as killed: no exit code, timed out.
    let reports = vault.reports.lock().unwrap().clone();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!((reports[0].exit_code, reports[0].timed_out), (None, true));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        !dir.path().join("survived").exists(),
        "the group died with the command"
    );
}

/// P3-4c-2 — a template is rendered into the run's secrets directory with
/// its values; the file is private, attached to each of its leases, and the
/// leases are kept, since the file lives with them. The answer is a path,
/// never a value.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn vault_render_writes_the_file_and_keeps_its_leases() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (b, vault, sid) = exec_belt(
        dir.path(),
        &[
            ("db/main#user", b"app-user-01"),
            ("db/main#password", b"pw-render-77"),
        ],
    );
    let mut c = Client::connect(&b).await;
    c.send(tool_call(
        1,
        "vault_render",
        json!({
            "template": "user={{ db/main#user }}\npass={{db/main#password}}\nuser again={{ db/main#user }}\n",
            "dest": "db.env",
            "ttl_s": 60,
        }),
    ))
    .await;
    let m = c.recv().await;
    assert!(m["result"].get("isError").is_none(), "{m}");
    assert!(
        !m.to_string().contains("pw-render"),
        "the answer carries no value"
    );
    let answer: Value =
        serde_json::from_str(m["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let path = PathBuf::from(answer["path"].as_str().unwrap());
    let secrets = dir.path().join("run").join("secrets");
    assert_eq!(path, secrets.join("db.env"));
    assert_eq!(answer["ttl_s"], 60);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"user=app-user-01\npass=pw-render-77\nuser again=app-user-01\n"
    );
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&secrets), 0o711);
    assert_eq!(
        vault.asked(),
        [
            (
                "lease_file".to_string(),
                sid,
                5,
                "db/main#user for db.env 60s".to_string()
            ),
            (
                "lease_file".to_string(),
                sid,
                5,
                "db/main#password for db.env 60s".to_string()
            ),
        ]
    );
    assert_eq!(
        *vault.attached.lock().unwrap(),
        [("L0".to_string(), path.clone()), ("L1".to_string(), path)]
    );
    assert!(
        vault.released.lock().unwrap().is_empty(),
        "kept: the file lives with its leases"
    );
    // P3-4d-2 — and the use is reported: both leases, the file's name.
    let reports = vault.reports.lock().unwrap().clone();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(
        reports[0].kind,
        roomler_ai_remote_control::vault::VaultActivityKind::Render
    );
    assert_eq!(reports[0].lease_ids, ["L0", "L1"]);
    assert_eq!(reports[0].target, "db.env");
    assert_eq!(reports[0].exit_code, None);
}

/// P3-4c-2 — one refused lease writes nothing and lets the others go.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn a_refused_render_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (b, vault, _) = exec_belt(dir.path(), &[("db/main", b"render-first-1")]);
    let mut c = Client::connect(&b).await;
    c.send(tool_call(
        1,
        "vault_render",
        json!({"template": "{{ db/main }} {{ db/missing }}", "dest": "out"}),
    ))
    .await;
    let m = c.recv().await;
    assert_eq!(m["result"]["isError"], true, "{m}");
    assert_eq!(
        m["result"]["content"][0]["text"],
        "db/missing: the vault refused: no_secret"
    );
    assert!(!dir.path().join("run").join("secrets").join("out").exists());
    assert_eq!(*vault.released.lock().unwrap(), ["L0"]);
    assert!(vault.attached.lock().unwrap().is_empty());
    assert!(
        vault.reports.lock().unwrap().is_empty(),
        "nothing written, nothing reported"
    );
}

/// P3-4c-3 — `vault_request` is offered with the reads and allowed like
/// them; it asks a lease ahead and answers the lease, never a value; a
/// refusal answers why.
#[cfg(feature = "vault")]
#[tokio::test]
async fn vault_request_asks_a_lease_ahead_and_answers_no_value() {
    // Made at runtime: a literal beside `password` reads as one to a scanner.
    let value = format!("ahead-{}", 11);
    let vault = Arc::new(FakeVault {
        values: [("db/main#password".to_string(), value.clone().into_bytes())].into(),
        ..FakeVault::default()
    });
    let sid = ObjectId::new();
    let b = belt_running(
        me(),
        Run {
            session: sid,
            fence: 1,
            vault: Some(vault.clone()),
            exec: None,
        },
        quick(),
    );
    let mut c = Client::connect(&b).await;
    c.send(json!({"method": "tools/list", "jsonrpc": "2.0", "id": 1}))
        .await;
    let list = c.recv().await;
    let request = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "vault_request")
        .expect("offered")
        .clone();
    assert_eq!(
        request["inputSchema"]["required"],
        json!(["handle", "mode", "audience"])
    );
    let allowed: Vec<String> = VAULT_AHEAD_NAMES
        .iter()
        .map(|n| format!("mcp__roomler__{n}"))
        .collect();
    assert_eq!(allowed, VAULT_AHEAD_TOOLS, "allowed is what is offered");
    assert!(!VAULT_USE_TOOLS.contains(&VAULT_AHEAD_TOOLS[0]));
    c.send(tool_call(
        2,
        "vault_request",
        json!({"handle": "db/main#password", "mode": "exec", "audience": "psql", "ttl_s": 600}),
    ))
    .await;
    let m = c.recv().await;
    assert!(m["result"].get("isError").is_none(), "{m}");
    assert!(!m.to_string().contains(&value), "no value: {m}");
    let answer: Value =
        serde_json::from_str(m["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        answer,
        json!({"lease_id": "L0", "handle": "db/main#password", "mode": "exec",
               "audience": "psql", "ttl_s": 600})
    );
    assert_eq!(
        vault.asked(),
        [(
            "ahead_exec".to_string(),
            sid,
            1,
            "db/main#password for psql 600s".to_string()
        )]
    );
    assert!(vault.released.lock().unwrap().is_empty(), "kept");
    c.send(tool_call(
        3,
        "vault_request",
        json!({"handle": "db/missing", "mode": "file", "audience": "db.env"}),
    ))
    .await;
    let m = c.recv().await;
    assert_eq!(m["result"]["isError"], true, "{m}");
    assert_eq!(
        m["result"]["content"][0]["text"],
        "db/missing: the vault refused: no_secret"
    );
}

/// P3-4c-3 — `vault_exec` takes a lease asked for ahead for its program
/// instead of asking, and keeps it; one that would end during the command
/// is not taken, and the call then asks and lets go of its own.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn vault_exec_takes_a_lease_asked_for_ahead_and_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    // Made at runtime: a literal beside `password` reads as one to a scanner.
    let value = format!("ahead-{}", 42);
    let (b, vault, sid) = exec_belt(dir.path(), &[("db/main#password", value.as_bytes())]);
    let mut c = Client::connect(&b).await;
    c.send(tool_call(
        1,
        "vault_request",
        json!({"handle": "db/main#password", "mode": "exec", "audience": "sh", "ttl_s": 600}),
    ))
    .await;
    assert!(c.recv().await["result"].get("isError").is_none());
    let run = json!({
        "argv": ["sh", "-c", "printf '%s' \"$PW\""],
        "env": {"PW": "db/main#password"},
    });
    let answer = exec_answer(&mut c, 2, run.clone()).await.unwrap();
    assert_eq!(answer["stdout"], "[vault:PW]", "{answer}");
    assert_eq!(
        vault.asked(),
        [(
            "ahead_exec".to_string(),
            sid,
            5,
            "db/main#password for sh 600s".to_string()
        )],
        "taken: nothing asked again"
    );
    assert!(vault.released.lock().unwrap().is_empty(), "kept");
    // Ending within the command's 60 s: not taken.
    vault
        .lefts
        .lock()
        .unwrap()
        .insert("L0".into(), Duration::from_secs(5));
    let answer = exec_answer(&mut c, 3, run).await.unwrap();
    assert_eq!(answer["stdout"], "[vault:PW]", "{answer}");
    assert_eq!(vault.asked()[1].0, "lease_exec");
    assert_eq!(
        *vault.released.lock().unwrap(),
        ["L1"],
        "its own let go, the one asked for ahead kept"
    );
}

/// P3-4c-3 — `vault_render` takes a lease asked for ahead toward its file,
/// asks for the rest, and its file lives until the first of them ends; a
/// lease asked for ahead toward another file is not taken.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn vault_render_takes_a_lease_asked_for_ahead_toward_its_file() {
    let dir = tempfile::tempdir().unwrap();
    // Made at runtime: a literal beside `password` reads as one to a scanner.
    let pass = format!("render-{}", 78);
    let (b, vault, _) = exec_belt(
        dir.path(),
        &[
            ("db/main#user", b"app-user-02"),
            ("db/main#password", pass.as_bytes()),
        ],
    );
    let mut c = Client::connect(&b).await;
    c.send(tool_call(
        1,
        "vault_request",
        json!({"handle": "db/main#user", "mode": "file", "audience": "db.env", "ttl_s": 600}),
    ))
    .await;
    assert!(c.recv().await["result"].get("isError").is_none());
    c.send(tool_call(
        2,
        "vault_render",
        json!({
            "template": "user={{ db/main#user }}\npass={{ db/main#password }}\n",
            "dest": "db.env",
            "ttl_s": 60,
        }),
    ))
    .await;
    let m = c.recv().await;
    assert!(m["result"].get("isError").is_none(), "{m}");
    let answer: Value =
        serde_json::from_str(m["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(answer["ttl_s"], 60, "until the first of its leases ends");
    let path = PathBuf::from(answer["path"].as_str().unwrap());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        format!("user=app-user-02\npass={pass}\n").into_bytes()
    );
    let kinds: Vec<String> = vault.asked().into_iter().map(|a| a.3).collect();
    assert_eq!(
        kinds,
        [
            "db/main#user for db.env 600s",
            "db/main#password for db.env 60s"
        ],
        "the user's taken, only the password asked for"
    );
    assert_eq!(
        *vault.attached.lock().unwrap(),
        [("L0".to_string(), path.clone()), ("L1".to_string(), path)]
    );
    assert!(vault.released.lock().unwrap().is_empty(), "both kept");
    // A file from the lease asked for ahead alone lives as long as it.
    let render = |id: u64, dest: &str| {
        tool_call(
            id,
            "vault_render",
            json!({"template": "{{ db/main#user }}", "dest": dest, "ttl_s": 60}),
        )
    };
    c.send(render(3, "db.env")).await;
    let m = c.recv().await;
    let answer: Value =
        serde_json::from_str(m["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(answer["ttl_s"], 600, "as long as the lease it took: {m}");
    assert_eq!(vault.asked().len(), 2, "taken: nothing asked");
    // One that would end before the file is asked to: not taken.
    vault
        .lefts
        .lock()
        .unwrap()
        .insert("L0".into(), Duration::from_secs(30));
    c.send(render(4, "db.env")).await;
    assert!(c.recv().await["result"].get("isError").is_none());
    assert_eq!(vault.asked()[2].3, "db/main#user for db.env 60s");
    // Nor one asked for ahead toward another file.
    c.send(render(5, "other.env")).await;
    assert!(c.recv().await["result"].get("isError").is_none());
    assert_eq!(
        vault.asked()[3].3,
        "db/main#user for other.env 60s",
        "asked for afresh toward another file"
    );
}

/// P3-5b-2 — the answer to a `vault_connect` call: its JSON, or its error
/// text.
#[cfg(feature = "vault")]
async fn connect_answer(c: &mut Client, id: u64, args: Value) -> Result<Value, String> {
    c.send(tool_call(id, "vault_connect", args)).await;
    let m = c.recv().await;
    assert_eq!(m["id"], id, "{m}");
    let text = m["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    if m["result"]["isError"] == true {
        Err(text)
    } else {
        Ok(serde_json::from_str(&text).unwrap())
    }
}

/// P3-5b-2 — `vault_connect` leases the secret's user and password, and its
/// CA, toward the upstream it names, and serves an endpoint only the
/// session's account reaches. A client that sends no password gets a session
/// there as the secret's user, and the answer holds no value. When a lease
/// goes, the endpoint goes with every connection it held, and the call's
/// other leases are let go rather than held to their expiry.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn vault_connect_serves_an_endpoint_that_signs_in_as_the_secrets_user() {
    use crate::vault_proxy::testing::{Saw, Upstreams, fake, made, message, next, startup};
    let pw = made();
    let (ca, port, mut saw) = fake(Upstreams::Scram(pw.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let (b, vault, sid) = exec_belt(
        dir.path(),
        &[
            ("db/main#user", b"app"),
            ("db/main#password", pw.as_bytes()),
            ("pki/db-ca", ca.as_bytes()),
        ],
    );
    let upstream = format!("localhost:{port}");
    let mut c = Client::connect(&b).await;
    let answer = connect_answer(
        &mut c,
        1,
        json!({
            "handle": "db/main",
            "engine": "postgres",
            "upstream": upstream,
            "database": "orders",
            "ca": "pki/db-ca",
            "ttl_s": 60,
        }),
    )
    .await
    .unwrap();
    assert!(
        !answer.to_string().contains(&pw),
        "the answer holds no value"
    );
    let host = PathBuf::from(answer["host"].as_str().unwrap());
    let proxies = dir.path().join("run").join("proxies");
    assert_eq!(host.parent(), Some(proxies.as_path()));
    assert_eq!(answer["port"], 5432);
    assert_eq!(answer["database"], "orders");
    assert_eq!(answer["ttl_s"], 60);
    assert_eq!(answer["env"]["PGHOST"], json!(host.to_string_lossy()));
    assert_eq!(answer["env"]["PGDATABASE"], "orders");
    assert!(
        answer["uri"]
            .as_str()
            .unwrap()
            .starts_with("postgresql:///orders?host=/"),
        "{answer}"
    );
    let socket = host.join(".s.PGSQL.5432");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&proxies), 0o711);
    assert_eq!(mode(&host), 0o711);
    assert_eq!(mode(&socket), 0o600);
    let lease = |h: &str| {
        (
            "lease_proxy".to_string(),
            sid,
            5,
            format!("{h} for {upstream} 60s"),
        )
    };
    assert_eq!(
        vault.asked(),
        [
            lease("db/main#user"),
            lease("db/main#password"),
            lease("pki/db-ca")
        ]
    );

    // A client with no password: a session, as the secret's user.
    let mut pg = tokio::net::UnixStream::connect(&socket).await.unwrap();
    pg.write_all(&startup(&[("user", "whoever"), ("database", "orders")]))
        .await
        .unwrap();
    assert_eq!(next(&mut pg).await, (b'R', vec![0, 0, 0, 0]));
    assert_eq!(next(&mut pg).await.0, b'S');
    assert_eq!(next(&mut pg).await.0, b'K');
    assert_eq!(next(&mut pg).await.0, b'Z');
    assert_eq!(
        saw.recv().await.unwrap(),
        Saw::Startup(vec![
            ("user".into(), "app".into()),
            ("database".into(), "orders".into()),
        ])
    );
    pg.write_all(&message(b'Q', b"select 1\0")).await.unwrap();
    assert_eq!(next(&mut pg).await.0, b'C');
    assert_eq!(next(&mut pg).await.0, b'Z');
    assert_eq!(saw.recv().await.unwrap(), Saw::Query("select 1".into()));
    assert!(
        vault.released.lock().unwrap().is_empty(),
        "kept while the endpoint serves"
    );

    // The password's lease goes: the endpoint closes, its connection with it,
    // and the call's other leases are let go.
    vault.release(&["L1".to_string()]);
    let mut rest = Vec::new();
    let ended = tokio::time::timeout(
        WAIT,
        tokio::io::AsyncReadExt::read_to_end(&mut pg, &mut rest),
    )
    .await;
    assert!(ended.is_ok(), "the connection ended with the lease");
    let gone = async {
        while host.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(WAIT, gone)
        .await
        .expect("the endpoint's directory goes");
    let released = || {
        let mut r = vault.released.lock().unwrap().clone();
        r.sort();
        r.dedup();
        r
    };
    let lets_go = async {
        while released().len() < 3 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(WAIT, lets_go)
        .await
        .expect("the call's leases are let go");
    assert_eq!(released(), ["L0", "L1", "L2"]);
    assert!(
        tokio::net::UnixStream::connect(&socket).await.is_err(),
        "nothing left to dial"
    );
}

/// P3-5b-2 — a refused lease opens no endpoint and lets the call's others
/// go; a call the device refuses asks the vault nothing.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn a_refused_connect_opens_no_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let (b, vault, _) = exec_belt(dir.path(), &[("db/main#user", b"app")]);
    let mut c = Client::connect(&b).await;
    let refused = connect_answer(
        &mut c,
        1,
        json!({"handle": "db/main", "engine": "postgres", "upstream": "db.internal:5432"}),
    )
    .await;
    assert_eq!(
        refused,
        Err("db/main#password: the vault refused: no_secret".into())
    );
    assert_eq!(*vault.released.lock().unwrap(), ["L0"]);
    assert!(vault.stops.lock().unwrap().is_empty());
    assert!(
        !dir.path().join("run").join("proxies").exists(),
        "no endpoint"
    );
    let asked = vault.asked().len();
    for bad in [
        json!({"handle": "db/main#password", "engine": "postgres", "upstream": "db.internal:5432"}),
        json!({"handle": "db/main", "engine": "mysql", "upstream": "db.internal:5432"}),
        json!({"handle": "db/main", "engine": "postgres", "upstream": "db.internal"}),
    ] {
        assert!(connect_answer(&mut c, 2, bad).await.is_err());
    }
    assert_eq!(vault.asked().len(), asked, "nothing asked of the vault");
}

/// P3-5b-2 — a run holds a bounded number of endpoints: one more is refused
/// before the vault is asked, and an endpoint that closes gives its place
/// back.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn a_run_holds_a_bounded_number_of_endpoints() {
    use super::super::vault_connect::MAX_ENDPOINTS;
    use crate::vault_proxy::testing::{Upstreams, fake, made};
    // An upstream to name, never dialled: its CA is what the endpoint trusts.
    let (ca, port, _saw) = fake(Upstreams::Trusting).await;
    let pw = made();
    let dir = tempfile::tempdir().unwrap();
    let (b, vault, _) = exec_belt(
        dir.path(),
        &[
            ("db/main#user", b"app"),
            ("db/main#password", pw.as_bytes()),
            ("pki/db-ca", ca.as_bytes()),
        ],
    );
    let call = json!({
        "handle": "db/main",
        "engine": "postgres",
        "upstream": format!("localhost:{port}"),
        "ca": "pki/db-ca",
    });
    let mut c = Client::connect(&b).await;
    let mut first = None;
    for id in 0..MAX_ENDPOINTS as u64 {
        let answer = connect_answer(&mut c, id, call.clone()).await.unwrap();
        first.get_or_insert(PathBuf::from(answer["host"].as_str().unwrap()));
    }
    let asked = vault.asked().len();
    let refused = connect_answer(&mut c, 100, call.clone()).await.unwrap_err();
    assert!(refused.contains("endpoints already"), "{refused}");
    assert_eq!(vault.asked().len(), asked, "nothing asked of the vault");
    // The first endpoint's CA lease goes, and with it the endpoint.
    vault.release(&["L2".to_string()]);
    let first = first.unwrap();
    let gone = async {
        while first.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(WAIT, gone).await.unwrap();
    let mut again = Err(String::new());
    for id in 200..250 {
        again = connect_answer(&mut c, id, call.clone()).await;
        if again.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(again.is_ok(), "its place is given back: {again:?}");
}

/// P3-5b-2 — a connection from any account but the session's is refused,
/// root's included: the socket is handed to the session's account, and each
/// connection's uid is checked again. Handing a socket to another account
/// needs root, as the daemon has; anywhere else this test says so and stops.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn a_proxy_connection_from_another_account_is_refused() {
    use crate::vault_proxy::testing::{Upstreams, fake, made, startup};
    // SAFETY: geteuid reads our own credentials; it cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipped: handing a socket to another account needs root");
        return;
    }
    let pw = made();
    let (ca, port, mut saw) = fake(Upstreams::Scram(pw.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let vault = Arc::new(FakeVault {
        values: [
            ("db/main#user", b"app".to_vec()),
            ("db/main#password", pw.as_bytes().to_vec()),
            ("pki/db-ca", ca.as_bytes().to_vec()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
        ..FakeVault::default()
    });
    std::fs::create_dir_all(dir.path().join("run")).unwrap();
    // The session runs as an account that is not this test's.
    let other = (4242, 4242);
    let b = belt_running(
        me(),
        Run {
            session: ObjectId::new(),
            fence: 5,
            vault: Some(vault.clone()),
            exec: Some(super::super::vault_exec::ExecTarget {
                account: None,
                home: dir.path().to_path_buf(),
                folder: dir.path().to_path_buf(),
                session_dir: dir.path().join("run"),
                owner: Some(other),
            }),
        },
        quick(),
    );
    let mut c = Client::connect(&b).await;
    let answer = connect_answer(
        &mut c,
        1,
        json!({
            "handle": "db/main",
            "engine": "postgres",
            "upstream": format!("localhost:{port}"),
            "ca": "pki/db-ca",
        }),
    )
    .await
    .unwrap();
    let socket = PathBuf::from(answer["host"].as_str().unwrap()).join(".s.PGSQL.5432");
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(&socket).unwrap();
        assert_eq!((m.uid(), m.gid()), other, "handed to the session's account");
    }
    // Root passes the socket's own permissions; the endpoint's check does not.
    let mut pg = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let _ = pg.write_all(&startup(&[("database", "d")])).await;
    let mut rest = Vec::new();
    let ended = tokio::time::timeout(
        WAIT,
        tokio::io::AsyncReadExt::read_to_end(&mut pg, &mut rest),
    )
    .await;
    assert!(ended.is_ok(), "the connection was closed");
    assert!(rest.is_empty(), "and answered nothing: {rest:?}");
    assert!(saw.try_recv().is_err(), "no upstream was dialled");
}

/// P3-5b-4 — a vault holding a secret's user and password, and the CA its
/// upstream's certificate is made under.
#[cfg(all(feature = "vault", windows))]
fn proxy_vault(password: &str, ca: &str) -> Arc<FakeVault> {
    Arc::new(FakeVault {
        values: [
            ("db/main#user", b"app".to_vec()),
            ("db/main#password", password.as_bytes().to_vec()),
            ("pki/db-ca", ca.as_bytes().to_vec()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
        ..FakeVault::default()
    })
}

/// P3-5b-4 — on Windows the endpoint is a loopback port. The answer names it,
/// and says the loopback leg is plain. A client of this account gets a
/// session as the secret's user, and when a lease goes, the connection goes.
#[cfg(all(feature = "vault", windows))]
#[tokio::test]
async fn vault_connect_serves_a_loopback_endpoint() {
    use crate::vault_proxy::testing::{Saw, Upstreams, fake, made, message, next, startup};
    let pw = made();
    let (ca, port, mut saw) = fake(Upstreams::Scram(pw.clone())).await;
    let vault = proxy_vault(&pw, &ca);
    let b = belt_running(
        me(),
        Run {
            session: ObjectId::new(),
            fence: 5,
            vault: Some(vault.clone()),
            exec: None,
        },
        quick(),
    );
    let mut c = Client::connect(&b).await;
    let answer = connect_answer(
        &mut c,
        1,
        json!({
            "handle": "db/main",
            "engine": "postgres",
            "upstream": format!("localhost:{port}"),
            "database": "orders",
            "ca": "pki/db-ca",
        }),
    )
    .await
    .unwrap();
    assert!(
        !answer.to_string().contains(&pw),
        "the answer holds no value"
    );
    assert_eq!(answer["host"], "127.0.0.1");
    assert_eq!(answer["env"]["PGSSLMODE"], "disable");
    let dial = u16::try_from(answer["port"].as_u64().unwrap()).unwrap();
    assert_eq!(
        answer["uri"],
        format!("postgresql://127.0.0.1:{dial}/orders?sslmode=disable")
    );
    let mut pg = tokio::net::TcpStream::connect(("127.0.0.1", dial))
        .await
        .unwrap();
    pg.write_all(&startup(&[("user", "whoever"), ("database", "orders")]))
        .await
        .unwrap();
    assert_eq!(next(&mut pg).await, (b'R', vec![0, 0, 0, 0]));
    assert_eq!(next(&mut pg).await.0, b'S');
    assert_eq!(next(&mut pg).await.0, b'K');
    assert_eq!(next(&mut pg).await.0, b'Z');
    assert_eq!(
        saw.recv().await.unwrap(),
        Saw::Startup(vec![
            ("user".into(), "app".into()),
            ("database".into(), "orders".into()),
        ])
    );
    pg.write_all(&message(b'Q', b"select 1\0")).await.unwrap();
    assert_eq!(next(&mut pg).await.0, b'C');
    assert_eq!(next(&mut pg).await.0, b'Z');
    // The password's lease goes, and with it the connection.
    vault.release(&["L1".to_string()]);
    let mut rest = Vec::new();
    let ended = tokio::time::timeout(
        WAIT,
        tokio::io::AsyncReadExt::read_to_end(&mut pg, &mut rest),
    )
    .await;
    assert!(ended.is_ok(), "the connection ended with the lease");
}

/// P3-5b-4 — a loopback connection from a process that does not run as the
/// session's account is refused. Here the session's account is another's,
/// so this test's own connection is the one refused.
#[cfg(all(feature = "vault", windows))]
#[tokio::test]
async fn a_loopback_connection_from_another_account_is_refused() {
    use crate::vault_proxy::testing::{Upstreams, fake, made, startup};
    let pw = made();
    let (ca, port, mut saw) = fake(Upstreams::Scram(pw.clone())).await;
    let vault = proxy_vault(&pw, &ca);
    // SYSTEM, unless this test is SYSTEM: then LOCAL SERVICE.
    let other = match crate::hive_win::own_sid().unwrap().as_str() {
        "S-1-5-18" => "S-1-5-19",
        _ => "S-1-5-18",
    }
    .to_string();
    let answer = super::super::vault_connect::call(
        vault,
        ObjectId::new(),
        5,
        None,
        &other,
        &json!({
            "handle": "db/main",
            "engine": "postgres",
            "upstream": format!("localhost:{port}"),
            "ca": "pki/db-ca",
        }),
    )
    .await
    .unwrap();
    let dial = u16::try_from(answer["port"].as_u64().unwrap()).unwrap();
    let mut pg = tokio::net::TcpStream::connect(("127.0.0.1", dial))
        .await
        .unwrap();
    let _ = pg.write_all(&startup(&[("database", "d")])).await;
    let mut rest = Vec::new();
    let ended = tokio::time::timeout(
        WAIT,
        tokio::io::AsyncReadExt::read_to_end(&mut pg, &mut rest),
    )
    .await;
    assert!(ended.is_ok(), "the connection was closed");
    assert!(rest.is_empty(), "and answered nothing: {rest:?}");
    assert!(saw.try_recv().is_err(), "no upstream was dialled");
}

/// P3-5c-2 — `vault_connect` serves the MongoDB engine on Unix. The socket
/// is one a driver dials by its path, and the answer's `uri` names it. A
/// handshake from a client with no credentials is answered by a connection
/// signed in as the secret's user, and the connection ends with its lease.
#[cfg(all(feature = "vault", unix))]
#[tokio::test]
async fn vault_connect_serves_mongodb_on_a_socket_a_driver_dials_by_its_path() {
    use crate::vault_proxy::testing::{
        MongoSaw, MongoUpstreams, fake_mongo, made, mongo_command, mongo_reply, op_query,
    };
    let pw = made();
    let (ca, port, mut saw) = fake_mongo(MongoUpstreams::Scram(pw.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let (b, vault, sid) = exec_belt(
        dir.path(),
        &[
            ("db/docs#user", b"app"),
            ("db/docs#password", pw.as_bytes()),
            ("pki/db-ca", ca.as_bytes()),
        ],
    );
    let upstream = format!("localhost:{port}");
    let mut c = Client::connect(&b).await;
    let answer = connect_answer(
        &mut c,
        1,
        json!({
            "handle": "db/docs",
            "engine": "mongodb",
            "upstream": upstream,
            "database": "orders",
            "ca": "pki/db-ca",
            "auth_source": "users",
            "ttl_s": 60,
        }),
    )
    .await
    .unwrap();
    assert!(
        !answer.to_string().contains(&pw),
        "the answer holds no value"
    );
    let socket = PathBuf::from(answer["host"].as_str().unwrap());
    assert!(socket.ends_with("mongodb-27017.sock"), "{answer}");
    assert_eq!(answer["port"], Value::Null);
    let uri = answer["uri"].as_str().unwrap();
    assert!(
        uri.starts_with("mongodb://%2F")
            && uri.ends_with("mongodb-27017.sock/orders?directConnection=true"),
        "{uri}"
    );
    assert_eq!(answer["env"]["MONGODB_URI"], uri);
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let mut m = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let handshake = bson::doc! { "isMaster": 1, "client": { "driver": { "name": "a-test" } } };
    m.write_all(&op_query(7, "admin.$cmd", &handshake))
        .await
        .unwrap();
    let (op, to, reply) = mongo_reply(&mut m).await;
    assert_eq!((op, to), (1, 7), "an OP_REPLY to the handshake");
    assert_eq!(reply.get_bool("ismaster"), Ok(true));
    let command = |name: &str, db: &str| MongoSaw::Command(name.into(), db.into());
    let seen: Vec<MongoSaw> = std::iter::from_fn(|| saw.try_recv().ok()).collect();
    assert_eq!(
        seen,
        [
            command("saslStart", "users"),
            MongoSaw::User("app".into()),
            command("saslContinue", "users"),
            MongoSaw::Query("admin.$cmd".into(), "isMaster".into()),
            MongoSaw::Metadata(true),
        ],
        "signed in in the call's auth database"
    );
    m.write_all(&mongo_command(
        8,
        &bson::doc! { "ping": 1, "$db": "orders" },
    ))
    .await
    .unwrap();
    let (op, to, reply) = mongo_reply(&mut m).await;
    assert_eq!((op, to), (2013, 8));
    assert_eq!(reply.get_f64("ok"), Ok(1.0));
    let lease = |h: &str| {
        (
            "lease_proxy".to_string(),
            sid,
            5,
            format!("{h} for {upstream} 60s"),
        )
    };
    assert_eq!(
        vault.asked(),
        [
            lease("db/docs#user"),
            lease("db/docs#password"),
            lease("pki/db-ca")
        ]
    );
    vault.release(&["L1".to_string()]);
    let mut rest = Vec::new();
    let ended = tokio::time::timeout(
        WAIT,
        tokio::io::AsyncReadExt::read_to_end(&mut m, &mut rest),
    )
    .await;
    assert!(ended.is_ok(), "the connection ended with the lease");
}

/// P3-5c-2 — and on Windows: a loopback port, `directConnection=true`, a
/// handshake answered by a connection signed in as the secret's user.
#[cfg(all(feature = "vault", windows))]
#[tokio::test]
async fn vault_connect_serves_mongodb_on_a_loopback_port() {
    use crate::vault_proxy::testing::{
        MongoUpstreams, fake_mongo, made, mongo_command, mongo_reply, op_query,
    };
    let pw = made();
    let (ca, port, _saw) = fake_mongo(MongoUpstreams::Scram(pw.clone())).await;
    let vault = proxy_vault(&pw, &ca);
    let b = belt_running(
        me(),
        Run {
            session: ObjectId::new(),
            fence: 5,
            vault: Some(vault.clone()),
            exec: None,
        },
        quick(),
    );
    let mut c = Client::connect(&b).await;
    let answer = connect_answer(
        &mut c,
        1,
        json!({
            "handle": "db/main",
            "engine": "mongodb",
            "upstream": format!("localhost:{port}"),
            "database": "orders",
            "ca": "pki/db-ca",
        }),
    )
    .await
    .unwrap();
    assert_eq!(answer["host"], "127.0.0.1");
    let dial = u16::try_from(answer["port"].as_u64().unwrap()).unwrap();
    assert_eq!(
        answer["uri"],
        format!("mongodb://127.0.0.1:{dial}/orders?directConnection=true")
    );
    let mut m = tokio::net::TcpStream::connect(("127.0.0.1", dial))
        .await
        .unwrap();
    let handshake = bson::doc! { "isMaster": 1, "client": { "driver": { "name": "a-test" } } };
    m.write_all(&op_query(7, "admin.$cmd", &handshake))
        .await
        .unwrap();
    let (_, to, reply) = mongo_reply(&mut m).await;
    assert_eq!(to, 7);
    assert_eq!(reply.get_bool("ismaster"), Ok(true));
    m.write_all(&mongo_command(
        8,
        &bson::doc! { "ping": 1, "$db": "orders" },
    ))
    .await
    .unwrap();
    let (_, to, reply) = mongo_reply(&mut m).await;
    assert_eq!((to, reply.get_f64("ok")), (8, Ok(1.0)));
    vault.release(&["L1".to_string()]);
    let mut rest = Vec::new();
    let ended = tokio::time::timeout(
        WAIT,
        tokio::io::AsyncReadExt::read_to_end(&mut m, &mut rest),
    )
    .await;
    assert!(ended.is_ok(), "the connection ended with the lease");
}

/// More open approvals than Claude Code ever asks for, and a question the
/// surface cannot show, are refused at once — nothing is opened for them.
#[tokio::test]
async fn too_many_approvals_and_structured_questions_are_refused_at_once() {
    let mut b = belt(quick());
    let mut c = Client::connect(&b).await;
    for id in 0..MAX_PENDING as u64 {
        c.send(call(10 + id, "Bash", json!({"command": "true"})))
            .await;
        opened(&mut b).await;
    }
    c.send(call(99, "Bash", json!({"command": "true"}))).await;
    let v = c.verdict(99).await;
    assert_eq!(v["behavior"], "deny");
    assert!(v["message"].as_str().unwrap().contains("Too many"), "{v}");

    c.send(call(
        100,
        "AskUserQuestion",
        json!({"questions": [{"question": "Which?"}]}),
    ))
    .await;
    let v = c.verdict(100).await;
    assert_eq!(v["behavior"], "deny");
    assert!(v["message"].as_str().unwrap().contains("plain text"), "{v}");
    assert_eq!(b.toolbelt.pending().ids().len(), MAX_PENDING);
}

/// The relay is a pipe: a message in, the daemon's answer out, and it ends
/// when the harness closes its side.
#[tokio::test]
async fn the_relay_pipes_both_ways_and_ends_with_the_harness() {
    let b = belt(quick());
    let (mut harness_in, relay_in) = tokio::io::duplex(1024);
    let (relay_out, harness_out) = tokio::io::duplex(1024);
    let socket = b.toolbelt.socket().to_path_buf();
    let relay = tokio::spawn(async move { relay_io(&socket, relay_in, relay_out).await });
    harness_in
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
        .await
        .unwrap();
    let mut lines = BufReader::new(harness_out).lines();
    let answer = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&answer).unwrap()["id"], 1);
    drop(harness_in);
    tokio::time::timeout(WAIT, relay)
        .await
        .expect("the relay ends with the harness")
        .unwrap()
        .unwrap();

    // Nothing to relay to is an error the harness sees, never a silent pipe.
    #[cfg(unix)]
    let nowhere = Path::new("/nonexistent/toolbelt.sock");
    #[cfg(windows)]
    let nowhere = Path::new(r"\\.\pipe\roomler-hive-nowhere");
    let gone = relay_io(nowhere, tokio::io::empty(), tokio::io::sink()).await;
    assert!(gone.is_err());
}

/// FR-90 P3-4e-2 — what the model is told when a lease needed a person: a
/// card nobody answered in time names its approval and says to ask again
/// once someone can answer it; a `denied` says a driver may have said no.
#[cfg(feature = "vault")]
#[test]
fn a_tool_says_plainly_what_an_approval_came_to() {
    use crate::vault::LeaseError;
    let waited = vault_why(LeaseError::AwaitingApproval("A1".into()));
    assert!(waited.contains("approval A1"), "{waited}");
    assert!(waited.contains("nobody answered"), "{waited}");
    let no = vault_why(LeaseError::Refused("denied".into()));
    assert!(no.contains("said no"), "{no}");
    assert_eq!(
        vault_why(LeaseError::Refused("no_role".into())),
        "the vault refused: no_role"
    );
    assert_eq!(APPROVAL_WINDOW, Duration::from_secs(600));
}
