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
    let dir = tempfile::tempdir().unwrap();
    let (tx, events) = mpsc::channel(64);
    #[cfg(unix)]
    let toolbelt = open(dir.path(), None, peer, ObjectId::new(), tx, timing).unwrap();
    #[cfg(windows)]
    let toolbelt = open(peer, ObjectId::new(), tx, timing).unwrap();
    Belt {
        _dir: dir,
        toolbelt,
        events,
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
    let refused = open(dir.path(), None, me(), ObjectId::new(), tx.clone(), quick());
    assert!(refused.is_err(), "a file in the socket's place is refused");
    std::fs::remove_file(dir.path().join("toolbelt.sock")).unwrap();
    let first = open(dir.path(), None, me(), ObjectId::new(), tx.clone(), quick()).unwrap();
    std::mem::forget(first); // leaves a stale socket, as a crash would
    let again = open(dir.path(), None, me(), ObjectId::new(), tx, quick());
    assert!(again.is_ok(), "a stale socket is replaced");
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
