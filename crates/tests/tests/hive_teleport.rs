// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2f-1 — a session teleported for real, end to end: a real server,
//! the primary in-process, and the target a process of its own (this test
//! binary again, asked to be a device by [`device_process`], which an
//! ordinary run passes at once). The target holds no copy of the session, and
//! its owner never let it hold copies at all (`hive_replica` off): what lets
//! it take the session is the teleport's prepare. Nothing is seeded on the
//! record; the route starts the teleport, and each step after it is a
//! device's own frame.
//!
//! | step | the devices | what the test reads |
//! |---|---|---|
//! | a session on A, alone; two prompts | A runs it, and checkpoints nothing: it has no member | B is no member |
//! | `POST …/teleport {device_id: B, folder}` | B prepares with every gate but the copy, and joins in the `teleport` role; A is told it has a member, checkpoints at once and says where its copy ends; B's copy follows the stream there | the answer `teleporting` |
//! | the drain, the sync and the move | as a promotion's | the record at fence 2 on B, in B's folder; the room's notes |
//! | the moved start | B takes the session over from the copy it took, the history and the folder put in place, then `--resume` | the folder's file; B's resume note; a prompt answered by B's harness, which remembers A's two prompts |
//! | the old primary | A is demoted, its harness stopped; it holds copies, so it stays a member | A's harness gone; A's row a member's |
//!
//! ⚠️ The stream rides `direct` on loopback: the tests build carries no
//! overlay, as in the canary.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use roomler_ai_tests::fixtures::seed::SeededTenant;
use roomler_ai_tests::fixtures::test_app::TestApp;
use serde_json::{Value, json};

mod hive_support;
use hive_support::*;

/// A stand-in for Claude Code, as far as a teleport asks. It keeps the
/// session's history where Claude Code does (the path the launch names), and
/// refuses as it does: `--session-id` for an id whose history exists,
/// `--resume` for one without. `recall` answers with how many prompts that
/// history holds, so a test sees what a resumed process remembers. A
/// `control_request` is answered, never taken as a prompt. Each prompt goes to
/// `$HOME/harness.stdin`, and the process's pid to `$HOME/harness.pid`: the
/// device's home, never the session's folder, which moves with the session.
const HARNESS: &str = r#"#!/bin/sh
echo $$ > "$HOME/harness.pid"
mode=""; sid=""; prev=""
for a in "$@"; do
  case "$prev" in --session-id|--resume) mode="$prev"; sid="$a" ;; esac
  prev="$a"
done
hist="$CLAUDE_CONFIG_DIR/projects/$CLAUDE_CODE_PROJECT_DIR_NAME/$sid.jsonl"
if [ "$mode" = "--session-id" ] && [ -e "$hist" ]; then
  echo "Error: Session ID $sid is already in use." >&2; exit 1
fi
if [ "$mode" = "--resume" ] && [ ! -e "$hist" ]; then
  echo "No conversation found with session ID: $sid" >&2; exit 1
fi
first=1
while IFS= read -r line; do
  case "$line" in
    *'"type":"control_request"'*)
      echo '{"type":"control_response","response":{"subtype":"success"}}'
      continue ;;
  esac
  printf '%s\n' "$line" >> "$HOME/harness.stdin"
  mkdir -p "${hist%/*}" && printf '%s\n' "$line" >> "$hist"
  if [ "$first" = 1 ]; then
    echo '{"type":"system","subtype":"init","session_id":"fake","model":"m","cwd":"'"$PWD"'","tools":[]}'
    first=0
  fi
  case "$line" in
    *recall*)
      n=$(wc -l < "$hist" | tr -d ' ')
      echo '{"type":"assistant","message":{"content":[{"type":"text","text":"history: '"$n"'"}]}}'
      ;;
    *)
      echo '{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}'
      ;;
  esac
  echo '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"duration_ms":5,"total_cost_usd":0.0}'
done
"#;

/// What the folder holds before the session's first turn: the work a
/// teleport must carry to the other device.
const WORK: &str = "the session's work, to be teleported\n";

/// The bound on the teleport, its joining step included.
const LONG: Duration = Duration::from_secs(90);

/// Set to a device's directory when the scenario starts this binary as that
/// device.
const DEVICE_ENV: &str = "HIVE_TELEPORT_DEVICE";

/// A device's process: this binary again, from the config the scenario
/// enrolled and saved in its directory, until its stdin closes. An ordinary
/// run passes it at once.
#[test]
fn device_process() {
    let Ok(dir) = std::env::var(DEVICE_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let joined = std::thread::Builder::new()
        .name("hive-teleport-device".into())
        .stack_size(16 << 20)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let cfg =
                    roomlerd::config::load(&dir.join("config.toml")).expect("the device's config");
                roomlerd::hive::init_as_daemon(
                    &cfg,
                    &dir.join("hive.db"),
                    &dir.join("run"),
                    &dir.join("home"),
                )
                .expect("the device's launcher");
                let (stop, stop_rx) = tokio::sync::watch::channel(false);
                let device = spawn_device(cfg, stop_rx);
                // Until the scenario closes our stdin.
                let _ = tokio::task::spawn_blocking(|| {
                    let mut sink = Vec::new();
                    let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut sink);
                })
                .await;
                let _ = stop.send(true);
                let _ = tokio::time::timeout(Duration::from_secs(10), device).await;
            });
        })
        .unwrap()
        .join();
    if let Err(panic) = joined {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn a_session_teleports_to_a_device_that_held_no_copy_and_runs_there_from_it() {
    // Its own thread with a deep stack, and a current-thread runtime, as the
    // canary: several servers' worth of tasks live in one body.
    let joined = std::thread::Builder::new()
        .name("hive-teleport".into())
        .stack_size(16 << 20)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(scenario())
        })
        .unwrap()
        .join();
    if let Err(panic) = joined {
        std::panic::resume_unwind(panic);
    }
}

/// A device's directory: a folder root with `work` in it, a home, the
/// harness. Its `work`.
fn device_dir(dir: &Path) -> PathBuf {
    let work = dir.join("roots").join("work");
    for d in [&work, &dir.join("home")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let harness = dir.join("claude");
    std::fs::write(&harness, HARNESS).unwrap();
    std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o755)).unwrap();
    work
}

/// A device that runs the owner's sessions; one that `holds` copies too.
fn runs(cfg: &mut roomlerd::config::AgentConfig, seeded: &SeededTenant, dir: &Path, holds: bool) {
    cfg.hive_enabled = true;
    cfg.hive_replica = holds;
    cfg.hive_accounts
        .insert(seeded.admin.id.clone(), "teleporter".into());
    cfg.hive_roots = vec![dir.join("roots").display().to_string()];
    cfg.hive_harness = Some(dir.join("claude").display().to_string());
}

/// The device `cfg` describes, as a process of its own, online.
async fn device(
    app: &TestApp,
    seeded: &SeededTenant,
    dir: &Path,
    cfg: &roomlerd::config::AgentConfig,
) -> tokio::process::Child {
    roomlerd::config::save(&dir.join("config.toml"), cfg).expect("the device's config");
    let child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "device_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(DEVICE_ENV, dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("the device's process");
    wait_online(app, seeded, &cfg.agent_id).await;
    child
}

/// Close the device's stdin and wait for its process to end.
async fn stop(mut child: tokio::process::Child) {
    drop(child.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
}

/// `device`'s row in the session's replicaset, as the server keeps it.
fn member<'a>(s: &'a Value, device: &str) -> Option<&'a Value> {
    s["replicaset"]["members"]
        .as_array()?
        .iter()
        .find(|m| m["device_id"] == device)
}

/// Poll the session until `pred` holds, within `bound`; the record either way.
async fn until_session(
    app: &TestApp,
    seeded: &SeededTenant,
    sid: &str,
    bound: Duration,
    what: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + bound;
    loop {
        let s = session(app, seeded, sid).await;
        if pred(&s) {
            return s;
        }
        assert!(Instant::now() < deadline, "{what}: {s}");
        tokio::time::sleep(POLL).await;
    }
}

/// `device`'s own row once it answered its join as the primary. The device
/// takes that join and the start behind it up in tasks of their own
/// (`handle_replica_join`, `handle_start`), so its answer may reach the
/// server after the start's: a session read idle on its new primary says
/// nothing yet of the primary's row. Once it moved on from `pending`, that
/// is the device's answer.
async fn answered_as_primary(
    app: &TestApp,
    seeded: &SeededTenant,
    sid: &str,
    device: &str,
) -> Value {
    let s = until_session(
        app,
        seeded,
        sid,
        LONG,
        &format!("{device} never answered its join as the primary"),
        |s| member(s, device).is_some_and(|m| m["role"] == "primary" && m["state"] != "pending"),
    )
    .await;
    member(&s, device).cloned().unwrap_or_default()
}

/// One prompt over `b`, and its turn's end.
async fn prompt(b: &mut Browser, id: &str, text: &str) -> Vec<Value> {
    b.send(json!({"op": "prompt", "id": id, "text": text}))
        .await;
    let ack = b.recv_op("prompt").await;
    assert_eq!(ack["ok"], true, "{id}: {ack}");
    b.events_until(|e| e["event"]["kind"] == "turn").await
}

/// The resume note `dir`'s device left for `sid`: what Claude Code would be
/// told as it resumes there. Empty: none left.
fn resume_note_of(dir: &Path, sid: &str) -> String {
    std::fs::read_to_string(
        dir.join("run")
            .join(sid)
            .join(roomlerd::hive::RESUME_NOTE_FILE),
    )
    .unwrap_or_default()
}

/// Whether the process `pid` names is still there.
fn alive(pid: &str) -> bool {
    Path::new("/proc").join(pid.trim()).exists()
}

async fn scenario() {
    let app = TestApp::spawn_with_settings(|s| {
        s.modules.hive = true;
        s.hive.replicaset = true;
        s.app.rate_limit_per_sec = 1_000;
        s.app.rate_limit_burst = 10_000;
    })
    .await;
    let seeded = app.seed_tenant("hiveteleport").await;
    let tid = seeded.tenant_id.clone();
    let owner = seeded.admin.access_token.clone();
    let dir = tempfile::tempdir().unwrap();

    // ── A, the primary, in this process; B, the target, in its own ─────────
    let a_dir = dir.path().join("a");
    let a_work = device_dir(&a_dir);
    std::fs::write(a_work.join("notes.md"), WORK).unwrap();
    let mut a_cfg = enrol(&app, &seeded, "hive-teleport-a").await;
    runs(&mut a_cfg, &seeded, &a_dir, true);
    roomlerd::hive::init_as_daemon(
        &a_cfg,
        &a_dir.join("hive.db"),
        &a_dir.join("run"),
        &a_dir.join("home"),
    )
    .expect("the test launcher");
    let a = a_cfg.agent_id.clone();
    let (stop_a, stop_rx) = tokio::sync::watch::channel(false);
    let a_task = spawn_device(a_cfg, stop_rx);
    wait_online(&app, &seeded, &a).await;

    // B runs the owner's sessions, and holds no copies of any.
    let b_dir = dir.path().join("b");
    let b_work = device_dir(&b_dir);
    let mut b_cfg = enrol(&app, &seeded, "hive-teleport-b").await;
    runs(&mut b_cfg, &seeded, &b_dir, false);
    let b = b_cfg.agent_id.clone();
    let b_proc = device(&app, &seeded, &b_dir, &b_cfg).await;

    // ── The session on A, alone, and two prompts ───────────────────────────
    let s = start(&app, &seeded, &a, &a_work).await;
    let sid = s["id"].as_str().unwrap().to_string();
    let room = s["room_id"].as_str().unwrap().to_string();
    assert!(
        reached(&app, &seeded, &sid, "idle").await,
        "the session never came up"
    );
    let alone = session(&app, &seeded, &sid).await;
    assert!(
        member(&alone, &b).is_none(),
        "B holds no copies: placement chose nobody: {alone}"
    );
    let mut ws = UserWs::open(&app, &owner).await;
    let mut on_a = view(&mut ws, &sid).await;
    on_a.send(json!({"op": "hello"})).await;
    let hello = on_a.recv_op("hello").await;
    assert_eq!(hello["may_prompt"], true, "{hello}");
    let tip = hello["tip"].as_u64().unwrap_or(0);
    on_a.send(json!({"op": "follow", "after": tip})).await;
    for (id, text) in [("p1", "the first prompt"), ("p2", "the second prompt")] {
        let turn = prompt(&mut on_a, id, text).await;
        assert!(
            turn.iter().any(|e| e["event"]["kind"] == "turn"),
            "{id}: the turn never ended: {turn:?}"
        );
    }
    on_a.close(&mut ws).await;
    let a_pid =
        std::fs::read_to_string(a_dir.join("home").join("harness.pid")).expect("A's harness ran");
    assert!(alive(&a_pid), "A's harness runs the session");

    // ── The teleport: prepared, joined, caught up, drained, synced, moved ──
    let began = Instant::now();
    let resp = app
        .auth_post(
            &format!("/api/tenant/{tid}/hive/session/{sid}/teleport"),
            &owner,
        )
        .json(&json!({ "device_id": b, "folder": b_work }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(answer["outcome"], "teleporting", "B is ready: {answer}");
    let moved = until_session(
        &app,
        &seeded,
        &sid,
        LONG,
        "the session never moved to B",
        |s| {
            s["device_id"] == b.as_str()
                && s["fence"] == 2
                && s["promotion"].is_null()
                && s["status"] == "idle"
        },
    )
    .await;
    eprintln!(
        "hive_teleport: teleported in {} ms",
        began.elapsed().as_millis()
    );
    assert_eq!(moved["folder"], b_work.display().to_string(), "{moved}");
    let row = answered_as_primary(&app, &seeded, &sid, &b).await;
    assert_eq!(
        (row["role"].as_str(), row["state"].as_str()),
        (Some("primary"), Some("joined")),
        "B's own row, as the primary: {row}"
    );
    for want in ["which holds no copy of it yet", "🔀 Moved from"] {
        let noted = until_room_message(&app, &seeded, &room, |m| {
            m["content"].as_str().is_some_and(|c| c.contains(want))
        })
        .await;
        assert!(noted, "the room says: {want}");
    }

    // ── B runs it, from the copy it took: the folder and the conversation ──
    assert_eq!(
        std::fs::read_to_string(b_work.join("notes.md"))
            .ok()
            .as_deref(),
        Some(WORK),
        "the folder came with the session"
    );
    let note = resume_note_of(&b_dir, &sid);
    assert!(
        note.contains("moved to this device by")
            && note.contains("It ran on hive-teleport-a, in ")
            && note.contains("Paths in earlier turns under "),
        "B's resume note: {note:?}"
    );
    let mut on_b = view(&mut ws, &sid).await;
    on_b.send(json!({"op": "hello"})).await;
    let hello = on_b.recv_op("hello").await;
    assert_eq!(hello["may_prompt"], true, "B serves it now: {hello}");
    let tip = hello["tip"].as_u64().unwrap_or(0);
    on_b.send(json!({"op": "follow", "after": tip})).await;
    let turn = prompt(&mut on_b, "p3", "recall").await;
    assert!(
        turn.iter().any(|e| text_of(e).contains("history: 3")),
        "B's harness resumed A's conversation, its two prompts and this one: {turn:?}"
    );
    on_b.close(&mut ws).await;

    // ── A, demoted: its harness stopped; it holds copies, so a member ──────
    let deadline = Instant::now() + WAIT;
    while alive(&a_pid) {
        assert!(Instant::now() < deadline, "A's harness still runs");
        tokio::time::sleep(POLL).await;
    }
    let a_read =
        std::fs::read_to_string(a_dir.join("home").join("harness.stdin")).unwrap_or_default();
    assert!(
        !a_read.contains("recall"),
        "nothing after the move reached A: {a_read}"
    );
    until_session(
        &app,
        &seeded,
        &sid,
        WAIT,
        "A never took its member's copy",
        |s| member(s, &a).is_some_and(|m| m["state"] == "joined" && m["role"] == "owner"),
    )
    .await;

    stop(b_proc).await;
    let _ = stop_a.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(10), a_task).await;
}
