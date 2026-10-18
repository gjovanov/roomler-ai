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
}

impl FakeVault {
    fn asked(&self) -> Vec<(String, ObjectId, u64, String)> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl VaultTools for FakeVault {
    #[cfg(feature = "vault")]
    async fn lease_exec(
        &self,
        session: ObjectId,
        fence: u64,
        handle: &super::super::vault_exec::Handle,
        program: &str,
        ttl_secs: u32,
    ) -> Result<String, String> {
        let key = if handle.field.is_empty() {
            handle.secret.clone()
        } else {
            format!("{}#{}", handle.secret, handle.field)
        };
        self.asked.lock().unwrap().push((
            "lease_exec".into(),
            session,
            fence,
            format!("{key} for {program} {ttl_secs}s"),
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
        Ok(id)
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
    // P3-4c-1 — and, in a build with the vault, its one use.
    #[cfg(feature = "vault")]
    assert_eq!(names[3..], ["vault_exec"]);
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
    assert!(
        !VAULT_TOOLS.contains(&"mcp__roomler__vault_exec"),
        "the settings never allow it: a person approves every call"
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
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        !dir.path().join("survived").exists(),
        "the group died with the command"
    );
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
