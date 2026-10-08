// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P0f — AC2, the canary: what a session says reaches its device, and
//! nothing the server keeps.
//!
//! A real server (`TestApp`, module `hive` on) and a real device in-process:
//! `roomlerd`'s signalling loop, its Hive supervisor, its replica store and
//! its viewer peer — sessions launched as this test's own account through
//! `hive::init_as_daemon` (feature `hive-test-launcher`). This file is its own
//! test BINARY because that supervisor is process-global: in the lib's test
//! binary, every other in-process agent would re-point it at its socket.
//!
//! Three canaries, three channels into a session:
//!
//! | canary | how it enters | must be in | must NOT be in |
//! |---|---|---|---|
//! | `prompt` | a prompt typed over the viewer peer | the device's store, the viewer | Mongo · object store · server logs · any frame the server sent the browser |
//! | `tool` | a tool's output (a file in the session's folder) | the same | the same |
//! | `stderr` | what a crashing harness writes to stderr | the device's store, as a note | the same |
//!
//! Each absence is checked next to a PRESENCE that proves the check can see:
//! the session's title (metadata, the server's by design) is found in Mongo;
//! a probe file uploaded through the files API is found in the object store;
//! the session's id is found in a server log line and in a frame. An absence
//! that cannot fail proves nothing.
//!
//! "Shown failing first" (AC2): on the code before this test, the `stderr`
//! canary reached the server — a crashing harness's stderr tail rode the
//! `ended` detail into the session record and the room's ended note. The
//! negative controls are in the FR-90 step log.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bson::{Document, doc};
use bytes::Bytes;
use futures::{SinkExt, StreamExt, TryStreamExt};
use roomler_ai_tests::fixtures::seed::SeededTenant;
use roomler_ai_tests::fixtures::test_app::TestApp;
use roomlerd::config::AgentConfig;
use roomlerd::encode::EncoderPreference;
use roomlerd::hive::framing::{self, Bounds, Reassembler};
use roomlerd::{enrollment, signaling};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

const WAIT: Duration = Duration::from_secs(20);
/// Between two reads of a state the server applies after the frame.
const POLL: Duration = Duration::from_millis(250);

/// A stand-in for Claude Code. Each prompt: one tool call that reads
/// `canary-tool.txt` in the session's folder, and its result. A prompt with
/// `crash` in it writes `canary-stderr.txt` to stderr and exits 3.
const HARNESS: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *crash*) cat canary-stderr.txt >&2; exit 3 ;;
  esac
  out=$(cat canary-tool.txt)
  echo '{"type":"system","subtype":"init","session_id":"fake","model":"m","cwd":"'"$PWD"'","tools":["Bash"]}'
  echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cat canary-tool.txt"}}]}}'
  echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"'"$out"'","is_error":false}]}}'
  echo '{"type":"assistant","message":{"content":[{"type":"text","text":"read it"}]}}'
  echo '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"duration_ms":5,"total_cost_usd":0.0}'
done
"#;

fn random(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

// ─── The log watch ──────────────────────────────────────────────────────────

/// Every event and span field of this thread's runtime that carries one of
/// the watched strings: (target, string). Installed as the thread's default
/// subscriber around a current-thread runtime, so the server's tasks and the
/// device's all log through it.
#[derive(Clone, Default)]
struct Watch {
    strings: Arc<Mutex<Vec<String>>>,
    hits: Arc<Mutex<Vec<(String, String)>>>,
}

impl Watch {
    fn watch(&self, s: &str) {
        self.strings.lock().unwrap().push(s.to_string());
    }

    fn check(&self, target: &str, fields: &str) {
        for s in self.strings.lock().unwrap().iter() {
            if fields.contains(s.as_str()) {
                self.hits
                    .lock()
                    .unwrap()
                    .push((target.to_string(), s.clone()));
            }
        }
    }

    /// The SERVER's lines that carried `s`: every crate of the server is
    /// `roomler_ai_*` or `roomler_core`; the device's are `roomlerd` and
    /// `roomler_node_core`, and its store `roomler_hive_node`.
    fn server_hits(&self, s: &str) -> Vec<String> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, w)| {
                w == s && (t.starts_with("roomler_ai") || t.starts_with("roomler_core"))
            })
            .map(|(t, _)| t.clone())
            .collect()
    }
}

struct Fields(String);

impl Visit for Fields {
    fn record_str(&mut self, _: &Field, v: &str) {
        self.0.push_str(v);
        self.0.push(' ');
    }
    fn record_debug(&mut self, _: &Field, v: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{v:?} "));
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Watch {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut f = Fields(String::new());
        event.record(&mut f);
        self.check(event.metadata().target(), &f.0);
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, _: &Id, _: Context<'_, S>) {
        let mut f = Fields(String::new());
        attrs.record(&mut f);
        self.check(attrs.metadata().target(), &f.0);
    }

    fn on_record(&self, span: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let mut f = Fields(String::new());
        values.record(&mut f);
        let target = ctx
            .span(span)
            .map(|s| s.metadata().target().to_string())
            .unwrap_or_default();
        self.check(&target, &f.0);
    }
}

// ─── The browser's user socket ──────────────────────────────────────────────

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The browser's `/ws`. Every frame the server sends is kept in `seen`,
/// whether or not the test reads it.
struct UserWs {
    sink: futures::stream::SplitSink<Ws, Message>,
    rx: mpsc::UnboundedReceiver<Value>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl UserWs {
    async fn open(app: &TestApp, token: &str) -> Self {
        let url = format!(
            "ws://{}/ws?token={}",
            app.addr,
            token
                .replace('+', "%2B")
                .replace('/', "%2F")
                .replace('=', "%3D")
        );
        let (ws, _) = connect_async(&url).await.expect("user ws connect");
        let (sink, mut stream) = ws.split();
        let (tx, rx) = mpsc::unbounded_channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let keep = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                if let Message::Text(text) = msg {
                    keep.lock().unwrap().push(text.to_string());
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        let _ = tx.send(v);
                    }
                }
            }
        });
        Self { sink, rx, seen }
    }

    async fn send(&mut self, kind: &str, data: Value) {
        self.sink
            .send(Message::Text(
                json!({ "type": kind, "data": data }).to_string().into(),
            ))
            .await
            .unwrap();
    }

    /// The `data` of the next frame of `kind`; a refusal on the way fails.
    async fn read(&mut self, kind: &str) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("no {kind} in time"));
            let f = tokio::time::timeout(left, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("no {kind} in time"))
                .expect("the user socket is open");
            assert_ne!(f["type"], "hive:view.refused", "refused: {f}");
            if f["type"] == kind {
                return f["data"].clone();
            }
        }
    }
}

// ─── The browser's viewer peer ──────────────────────────────────────────────

struct Browser {
    grant: String,
    pc: Arc<RTCPeerConnection>,
    inbox: mpsc::Receiver<Value>,
    dc: Arc<RTCDataChannel>,
    next_id: u32,
}

impl Browser {
    async fn send(&mut self, v: Value) {
        let bytes = serde_json::to_vec(&v).unwrap();
        for frame in framing::encode(self.next_id, &bytes) {
            self.dc.send(&Bytes::from(frame)).await.unwrap();
        }
        self.next_id = self.next_id.wrapping_add(1);
    }

    async fn recv_op(&mut self, op: &str) -> Value {
        loop {
            let v = tokio::time::timeout(WAIT, self.inbox.recv())
                .await
                .unwrap_or_else(|_| panic!("no {op:?} from the device in time"))
                .expect("the channel is open");
            if v["op"] == op {
                return v;
            }
        }
    }

    /// Transcript events as they arrive, until one matches `until` or the
    /// patience runs out — what arrived, either way.
    async fn events_until(&mut self, until: impl Fn(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + WAIT;
        let mut got = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match tokio::time::timeout(left, self.inbox.recv()).await {
                Ok(Some(v)) if v["op"] == "events" => {
                    for e in v["events"].as_array().cloned().unwrap_or_default() {
                        let done = until(&e);
                        got.push(e);
                        if done {
                            return got;
                        }
                    }
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        got
    }

    async fn close(self, ws: &mut UserWs) {
        ws.send("hive:view.close", json!({ "grant_id": self.grant }))
            .await;
        let _ = self.pc.close().await;
    }
}

/// Open `session`'s viewer as the SPA does: `hive:view.open`, a peer dialled
/// only after `ready`, our offer before our candidates, theirs buffered until
/// the answer.
async fn view(ws: &mut UserWs, session: &str) -> Browser {
    ws.send(
        "hive:view.open",
        json!({ "session_id": session, "ref": session }),
    )
    .await;
    let ready = ws.read("hive:view.ready").await;
    let grant = ready["grant_id"].as_str().unwrap().to_string();

    let api = APIBuilder::new().build();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );
    let dc = pc.create_data_channel("hive", None).await.unwrap();
    let (inbox_tx, inbox) = mpsc::channel(1024);
    {
        let reasm = Arc::new(Mutex::new(Reassembler::new(Bounds {
            max_message_bytes: 8 * 1024 * 1024,
            max_in_flight: 4,
        })));
        dc.on_message(Box::new(move |msg| {
            let inbox = inbox_tx.clone();
            let reasm = Arc::clone(&reasm);
            Box::pin(async move {
                let done = reasm.lock().unwrap().push(&msg.data);
                if let Ok(Some(m)) = done
                    && let Ok(v) = serde_json::from_slice::<Value>(&m)
                {
                    let _ = inbox.send(v).await;
                }
            })
        }));
    }
    let (cand_tx, mut cand_rx) = mpsc::unbounded_channel::<Value>();
    pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
        let tx = cand_tx.clone();
        Box::pin(async move {
            if let Some(c) = c
                && let Ok(init) = c.to_json()
            {
                let _ = tx.send(serde_json::to_value(init).unwrap());
            }
        })
    }));
    let offer = pc.create_offer(None).await.unwrap();
    pc.set_local_description(offer.clone()).await.unwrap();
    ws.send(
        "hive:view.offer",
        json!({ "grant_id": grant, "sdp": offer.sdp }),
    )
    .await;

    let deadline = Instant::now() + WAIT;
    let mut answered = false;
    let mut theirs: Vec<RTCIceCandidateInit> = Vec::new();
    while dc.ready_state() != RTCDataChannelState::Open {
        assert!(Instant::now() < deadline, "the viewer channel never opened");
        while let Ok(c) = cand_rx.try_recv() {
            ws.send(
                "hive:view.ice",
                json!({ "grant_id": grant, "candidate": c }),
            )
            .await;
        }
        let Ok(Some(f)) = tokio::time::timeout(Duration::from_millis(50), ws.rx.recv()).await
        else {
            continue;
        };
        if f["data"]["grant_id"] != grant.as_str() {
            continue;
        }
        match f["type"].as_str() {
            Some("hive:view.answer") => {
                let sdp = f["data"]["sdp"].as_str().unwrap().to_string();
                pc.set_remote_description(RTCSessionDescription::answer(sdp).unwrap())
                    .await
                    .unwrap();
                answered = true;
                for c in theirs.drain(..) {
                    let _ = pc.add_ice_candidate(c).await;
                }
            }
            Some("hive:view.ice") => {
                if let Ok(init) =
                    serde_json::from_value::<RTCIceCandidateInit>(f["data"]["candidate"].clone())
                {
                    if answered {
                        let _ = pc.add_ice_candidate(init).await;
                    } else {
                        theirs.push(init);
                    }
                }
            }
            Some("hive:view.closed") => panic!("the device closed the viewer: {f}"),
            _ => {}
        }
    }
    Browser {
        grant,
        pc,
        inbox,
        dc,
        next_id: 0,
    }
}

// ─── The server's side, read past the API ───────────────────────────────────

/// Every document in the test's database whose raw BSON holds `needle` —
/// bytes, so a string inside a binary field is found too.
async fn in_mongo(db: &mongodb::Database, needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for coll in db.list_collection_names().await.unwrap() {
        let mut cur = db
            .collection::<Document>(&coll)
            .find(doc! {})
            .await
            .unwrap();
        while let Some(d) = cur.try_next().await.unwrap() {
            let raw = bson::to_vec(&d).unwrap();
            if contains(&raw, needle.as_bytes()) {
                hits.push(format!(
                    "{coll} {}",
                    d.get("_id").map(|i| i.to_string()).unwrap_or_default()
                ));
            }
        }
    }
    hits
}

/// Every file under the object store's root whose name or bytes hold `needle`.
fn in_storage(root: &Path, needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.to_string_lossy().contains(needle) {
                hits.push(format!("name {}", p.display()));
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => dirs.push(p),
                Ok(t) if t.is_file() => {
                    let small = e.metadata().map(|m| m.len() <= 64 << 20).unwrap_or(false);
                    if small
                        && let Ok(bytes) = std::fs::read(&p)
                        && contains(&bytes, needle.as_bytes())
                    {
                        hits.push(format!("bytes {}", p.display()));
                    }
                }
                _ => {}
            }
        }
    }
    hits
}

// ─── Scaffolding ────────────────────────────────────────────────────────────

async fn enrol(app: &TestApp, seeded: &SeededTenant, machine: &str) -> AgentConfig {
    let et: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    enrollment::enroll(enrollment::EnrollInputs {
        server_url: &app.base_url,
        enrollment_token: et["enrollment_token"].as_str().unwrap(),
        machine_id: machine,
        machine_name: machine,
    })
    .await
    .expect("agent enrollment")
}

/// `roomlerd run`'s signalling loop, with the handles `main.rs` gives it.
fn spawn_device(
    cfg: AgentConfig,
    stop: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (view_tx, _view_rx) = tokio::sync::watch::channel(Default::default());
        let broker = roomlerd::consent::ConsentBroker::new(
            roomlerd::consent::Mode::AutoGrant,
            std::env::temp_dir().join(format!("roomler-test-consent-{}", cfg.agent_id)),
        )
        .expect("consent broker init");
        let exec_enabled = cfg.exec_enabled;
        let remote_config_enabled = cfg.remote_config_enabled;
        let _ = signaling::run(
            signaling::OrgCtx::primary(),
            roomlerd::delegate::Delegation::Off,
            cfg,
            EncoderPreference::Software,
            stop,
            connected,
            view_tx,
            Default::default(),
            broker,
            roomlerd::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            roomlerd::remote_config::RemoteConfigServices::new(
                PathBuf::from("unused-in-tests.toml"),
                Arc::new(tokio::sync::Mutex::new(())),
                exec_enabled,
                remote_config_enabled,
            ),
            roomlerd::rc_sessions::RcSessionRegistry::new(),
        )
        .await;
    })
}

async fn wait_online(app: &TestApp, seeded: &SeededTenant, agent_id: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let row: Value = app
            .auth_get(
                &format!("/api/tenant/{}/agent/{agent_id}", seeded.tenant_id),
                &seeded.admin.access_token,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap_or(Value::Null);
        if row["is_online"] == true {
            return;
        }
        assert!(Instant::now() < deadline, "the device never came online");
        tokio::time::sleep(POLL).await;
    }
}

/// `POST …/hive/session`, retrying only the refusals that race the device's
/// registration (the row reads online a moment before the Hub holds its
/// capabilities); both come before any session exists.
async fn start(app: &TestApp, s: &SeededTenant, device: &str, folder: &Path) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let body: Value = app
            .auth_post(
                &format!("/api/tenant/{}/hive/session", s.tenant_id),
                &s.admin.access_token,
            )
            .json(&json!({ "device_id": device, "folder": folder }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let racing = body["outcome"] == "refused"
            && body["session"].is_null()
            && matches!(
                body["reason"].as_str(),
                Some("device_offline" | "device_unsupported")
            );
        if !racing {
            assert_eq!(body["outcome"], "accepted", "{body}");
            return body["session"].clone();
        }
        assert!(Instant::now() < deadline, "the start kept racing: {body}");
        tokio::time::sleep(POLL).await;
    }
}

/// The session's record — `Null` when the answer is not one (the reads
/// before the server's side is searched must not fail the test first).
async fn session(app: &TestApp, s: &SeededTenant, sid: &str) -> Value {
    app.auth_get(
        &format!("/api/tenant/{}/hive/session/{sid}", s.tenant_id),
        &s.admin.access_token,
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap_or(Value::Null)
}

/// Poll until the session's status is `want`; `false` when it never was.
async fn reached(app: &TestApp, s: &SeededTenant, sid: &str, want: &str) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if session(app, s, sid).await["status"] == want {
            return true;
        }
        tokio::time::sleep(POLL).await;
    }
    false
}

/// The session room's messages, as its owner reads them.
async fn room_messages(app: &TestApp, s: &SeededTenant, room: &str) -> Vec<Value> {
    let v: Value = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{room}/message?per_page=100",
                s.tenant_id
            ),
            &s.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap_or(Value::Null);
    v["items"].as_array().cloned().unwrap_or_default()
}

/// A probe file through the files API, into the org's first room — what the
/// object-store walk must be able to find.
async fn upload_probe(app: &TestApp, s: &SeededTenant, content: &str) {
    let room = &s.rooms[0].id;
    app.auth_post(
        &format!("/api/tenant/{}/room/{room}/join", s.tenant_id),
        &s.admin.access_token,
    )
    .send()
    .await
    .unwrap();
    let part = reqwest::multipart::Part::bytes(content.as_bytes().to_vec())
        .file_name("probe.txt")
        .mime_str("text/plain")
        .unwrap();
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("room_id", room.clone());
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/file/upload", s.tenant_id),
            &s.admin.access_token,
        )
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the probe upload");
}

/// FR-90 P1a-2 — the harness's side of a session's toolbelt: what Claude
/// Code sends through its relay, played by the test.
struct Harness {
    rd: tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    wr: tokio::net::unix::OwnedWriteHalf,
}

impl Harness {
    async fn connect(socket: &Path) -> Self {
        use tokio::io::AsyncBufReadExt;
        let s = tokio::net::UnixStream::connect(socket)
            .await
            .unwrap_or_else(|e| panic!("the session's toolbelt at {}: {e}", socket.display()));
        let (rd, wr) = s.into_split();
        Self {
            rd: tokio::io::BufReader::new(rd).lines(),
            wr,
        }
    }

    async fn send(&mut self, v: Value) {
        use tokio::io::AsyncWriteExt;
        let mut line = v.to_string();
        line.push('\n');
        self.wr.write_all(line.as_bytes()).await.unwrap();
    }

    /// The answer to request `id`, skipping progress.
    async fn answer(&mut self, id: u64) -> Value {
        loop {
            let line = tokio::time::timeout(WAIT, self.rd.next_line())
                .await
                .expect("the toolbelt answered in time")
                .unwrap()
                .expect("the toolbelt is open");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                return v;
            }
        }
    }
}

/// Whether a message in `room` matches `pred` within the test's patience.
async fn until_room_message(
    app: &TestApp,
    s: &SeededTenant,
    room: &str,
    pred: impl Fn(&Value) -> bool,
) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if room_messages(app, s, room).await.iter().any(&pred) {
            return true;
        }
        tokio::time::sleep(POLL).await;
    }
    false
}

fn text_of(e: &Value) -> String {
    e["event"].to_string()
}

// ─── The test ───────────────────────────────────────────────────────────────

#[test]
fn what_a_session_says_reaches_its_device_and_nothing_the_server_keeps() {
    let watch = Watch::default();
    let w = watch.clone();
    // Its own thread: a deep stack (the suite wants 8 MiB), and a
    // current-thread runtime under a thread-default subscriber, so every
    // task of the server and the device logs through the watch.
    let joined = std::thread::Builder::new()
        .name("hive-canary".into())
        .stack_size(16 << 20)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let subscriber = tracing_subscriber::registry().with(w.clone());
            tracing::subscriber::with_default(subscriber, || rt.block_on(scenario(w)));
        })
        .unwrap()
        .join();
    if let Err(panic) = joined {
        std::panic::resume_unwind(panic);
    }
}

async fn scenario(watch: Watch) {
    let prompt_canary = random("canaryprompt");
    let tool_canary = random("canarytool");
    let stderr_canary = random("canarystderr");
    // P1a-2 — what an approval would run, and what a driver told the model.
    let approval_canary = random("canaryapproval");
    let deny_canary = random("canarydeny");
    let title_probe = random("probetitle");
    let file_probe = random("probefile");
    for c in [
        &prompt_canary,
        &tool_canary,
        &stderr_canary,
        &approval_canary,
        &deny_canary,
    ] {
        watch.watch(c);
    }

    // The API's per-IP limiter is not what this test is about, and its 429
    // would end a run before the server's side is searched.
    let app = TestApp::spawn_with_settings(|s| {
        s.modules.hive = true;
        s.app.rate_limit_per_sec = 1_000;
        s.app.rate_limit_burst = 10_000;
    })
    .await;
    let seeded = app.seed_tenant("hivecanary").await;
    upload_probe(&app, &seeded, &file_probe).await;

    // The device: a folder root holding the two sessions' folders, a home,
    // the harness, the store.
    let dir = tempfile::tempdir().unwrap();
    let roots = dir.path().join("roots");
    let work = roots.join(&title_probe);
    let crash = roots.join("crash");
    let home = dir.path().join("home");
    for d in [&work, &crash, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(work.join("canary-tool.txt"), &tool_canary).unwrap();
    std::fs::write(crash.join("canary-stderr.txt"), &stderr_canary).unwrap();
    std::fs::write(crash.join("canary-tool.txt"), "unused").unwrap();
    let harness = dir.path().join("claude");
    std::fs::write(&harness, HARNESS).unwrap();
    std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut cfg = enrol(&app, &seeded, "hive-canary").await;
    cfg.hive_enabled = true;
    cfg.hive_accounts
        .insert(seeded.admin.id.clone(), "canary".into());
    cfg.hive_roots = vec![roots.display().to_string()];
    cfg.hive_harness = Some(harness.display().to_string());
    cfg.hive_max_sessions = Some(2);
    roomlerd::hive::init_as_daemon(
        &cfg,
        &dir.path().join("hive.db"),
        &dir.path().join("run"),
        &home,
    )
    .expect("the test launcher");
    let device = cfg.agent_id.clone();
    let (stop_device, stop_rx) = tokio::sync::watch::channel(false);
    let device_task = spawn_device(cfg, stop_rx);
    wait_online(&app, &seeded, &device).await;
    let mut ws = UserWs::open(&app, &seeded.admin.access_token).await;

    // ── Session 1: a prompt over the peer, and a tool's output ──────────────
    let s1 = start(&app, &seeded, &device, &work).await;
    let sid1 = s1["id"].as_str().unwrap().to_string();
    let room1 = s1["room_id"].as_str().unwrap().to_string();
    watch.watch(&sid1);
    assert!(
        reached(&app, &seeded, &sid1, "idle").await,
        "session 1 never came up"
    );
    let mut b = view(&mut ws, &sid1).await;
    b.send(json!({"op": "hello"})).await;
    let hello = b.recv_op("hello").await;
    assert_eq!(hello["may_prompt"], true, "{hello}");
    let tip = hello["tip"].as_u64().unwrap_or(0);
    b.send(json!({"op": "follow", "after": tip})).await;
    b.send(json!({"op": "prompt", "id": "p1", "text": format!("read the file, {prompt_canary}")}))
        .await;
    let ack = b.recv_op("prompt").await;
    assert_eq!(ack["ok"], true, "the device took the prompt: {ack}");
    // The turn, as far as it gets — everything after this point is checked
    // only once the server's side has been searched, so a leak is reported
    // as a leak rather than as whatever it broke on the way.
    let live1 = b.events_until(|e| e["event"]["kind"] == "turn").await;
    let turn_ref = format!("{sid1}#1");
    let deadline = Instant::now() + WAIT;
    let mut stub_done = false;
    while !stub_done && Instant::now() < deadline {
        stub_done = room_messages(&app, &seeded, &room1).await.iter().any(|m| {
            m["binding"]["ref"] == turn_ref.as_str()
                && m["content"].as_str().is_some_and(|c| c.contains("done"))
        });
        if !stub_done {
            tokio::time::sleep(POLL).await;
        }
    }
    // ── An approval in session 1 (P1a-2): the test plays the harness ────────
    // asking, through the session's toolbelt as Claude Code would, and the
    // browser is the driver who answers.
    let socket = dir.path().join("run").join(&sid1).join("toolbelt.sock");
    let mut harness = Harness::connect(&socket).await;
    harness
        .send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25"}}))
        .await;
    let _ = harness.answer(0).await;
    harness
        .send(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "approve",
                "arguments": {"tool_name": "Bash", "tool_use_id": "toolu_canary",
                              "input": {"command": format!("echo {approval_canary}")}},
            }}),
        )
        .await;
    let open = b.recv_op("approvals").await;
    let approval = open["pending"][0].as_str().unwrap_or_default().to_string();
    let approval_ref = format!("{sid1}!{approval}");
    let stub_opened = until_room_message(&app, &seeded, &room1, |m| {
        m["binding"]["ref"] == approval_ref.as_str()
            && m["content"]
                .as_str()
                .is_some_and(|c| c.contains("Approval needed"))
    })
    .await;
    b.send(
        json!({"op": "answer", "id": "a1", "approval": approval, "decision": "deny",
        "message": format!("not now, {deny_canary}")}),
    )
    .await;
    let answered = b.recv_op("answer").await;
    let verdict = harness.answer(1).await;
    let stub_denied = until_room_message(&app, &seeded, &room1, |m| {
        m["binding"]["ref"] == approval_ref.as_str()
            && m["content"]
                .as_str()
                .is_some_and(|c| c.contains("denied by"))
    })
    .await;

    b.send(json!({"op": "page", "after": 0, "limit": 500}))
        .await;
    let stored1 = b.recv_op("page").await;
    b.close(&mut ws).await;
    let _ = app
        .auth_post(
            &format!("/api/tenant/{}/hive/session/{sid1}/stop", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await;
    let stopped1 = reached(&app, &seeded, &sid1, "ended").await;

    // ── Session 2: a harness that dies talking ───────────────────────────────
    let s2 = start(&app, &seeded, &device, &crash).await;
    let sid2 = s2["id"].as_str().unwrap().to_string();
    let up2 = reached(&app, &seeded, &sid2, "idle").await;
    let mut b2 = view(&mut ws, &sid2).await;
    b2.send(json!({"op": "hello"})).await;
    let tip2 = b2.recv_op("hello").await["tip"].as_u64().unwrap_or(0);
    b2.send(json!({"op": "follow", "after": tip2})).await;
    b2.send(json!({"op": "prompt", "id": "p2", "text": "crash"}))
        .await;
    let live2 = b2
        .events_until(|e| {
            e["event"]["kind"] == "note"
                && e["event"]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains(stderr_canary.as_str()))
        })
        .await;
    let ended2 = reached(&app, &seeded, &sid2, "ended").await;
    b2.close(&mut ws).await;
    let _ = stop_device.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(10), device_task).await;

    // ── The server's side ───────────────────────────────────────────────────
    let uploads = roomler_core::storage::local_upload_dir();
    let frames = ws.seen.lock().unwrap().clone();
    let in_frames = |s: &str| frames.iter().filter(|f| f.contains(s)).count();

    // The checks can see: each channel holds what it is supposed to.
    assert!(
        !in_mongo(&app.db, &title_probe).await.is_empty(),
        "the Mongo scan must find the session's title, which the server holds"
    );
    assert!(
        !in_storage(&uploads, &file_probe).is_empty(),
        "the object-store walk must find the probe file under {}",
        uploads.display()
    );
    assert!(
        !watch.server_hits(&sid1).is_empty(),
        "the log watch must see the server log the session's id"
    );
    assert!(
        in_frames(&sid1) > 0,
        "the frame capture must see the session's id"
    );

    // And none of them holds what the session said.
    let mut leaks = Vec::new();
    for (what, canary) in [
        ("the prompt", &prompt_canary),
        ("the tool's output", &tool_canary),
        ("the harness's stderr", &stderr_canary),
        ("what an approval would run", &approval_canary),
        ("what a driver told the model", &deny_canary),
    ] {
        for hit in in_mongo(&app.db, canary).await {
            leaks.push(format!("{what} is in Mongo: {hit}"));
        }
        for hit in in_storage(&uploads, canary) {
            leaks.push(format!("{what} is in the object store: {hit}"));
        }
        for target in watch.server_hits(canary) {
            leaks.push(format!("{what} is in a server log line ({target})"));
        }
        if in_frames(canary) > 0 {
            leaks.push(format!("{what} is in a frame the server sent the browser"));
        }
    }
    assert!(
        leaks.is_empty(),
        "what a session says reached the server:\n  {}",
        leaks.join("\n  ")
    );

    // ── The device's side: everything is there ──────────────────────────────
    let live1: Vec<String> = live1.iter().map(text_of).collect();
    assert!(
        live1
            .iter()
            .any(|e| e.contains("user_message") && e.contains(prompt_canary.as_str())),
        "the viewer saw the prompt: {live1:#?}"
    );
    assert!(
        live1
            .iter()
            .any(|e| e.contains("tool_result") && e.contains(tool_canary.as_str())),
        "the viewer saw the tool's output: {live1:#?}"
    );
    assert!(
        live1.iter().any(|e| e.contains("\"turn\"")),
        "the turn completed: {live1:#?}"
    );
    let stored1 = stored1.to_string();
    assert!(
        stored1.contains(prompt_canary.as_str()) && stored1.contains(tool_canary.as_str()),
        "the device's store holds both: {stored1}"
    );
    assert!(stub_done, "the server stubbed the turn as done");

    // P1a-2 — the approval: the driver's answer reached the harness, with
    // the driver's words; the server knew only THAT it waited, and how it
    // ended, and who answered.
    assert!(
        !approval.is_empty(),
        "the viewer was told an approval is open: {open}"
    );
    assert_eq!(
        answered["ok"], true,
        "the device took the driver's answer: {answered}"
    );
    let said: Value = serde_json::from_str(
        verdict["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("{}"),
    )
    .unwrap_or_default();
    assert_eq!(said["behavior"], "deny", "{verdict}");
    assert!(
        said["message"]
            .as_str()
            .is_some_and(|m| m.contains(deny_canary.as_str())),
        "the harness read the driver's reason: {said}"
    );
    assert!(
        stub_opened,
        "the room's stub said the session needs approval"
    );
    assert!(stub_denied, "and then how it ended");
    let record = app
        .db
        .collection::<Document>("agent_approvals")
        .find_one(doc! { "approval_id": &approval })
        .await
        .unwrap()
        .expect("the approval is on the record");
    assert_eq!(record.get_str("status").unwrap(), "denied", "{record}");
    assert_eq!(
        record.get_object_id("answered_by").unwrap().to_hex(),
        seeded.admin.id,
        "who answered: {record}"
    );
    let notified = app
        .db
        .collection::<Document>("notifications")
        .find_one(doc! { "notification_type": "approval_request" })
        .await
        .unwrap()
        .expect("the driver was notified");
    assert!(
        notified
            .get_str("link")
            .is_ok_and(|l| l.ends_with(&format!("/room/{room1}"))),
        "the notification leads to the session's room: {notified}"
    );
    assert!(
        stored1.contains(approval_canary.as_str()) && stored1.contains(deny_canary.as_str()),
        "the device's store holds what was asked and what the driver said: {stored1}"
    );
    assert!(stopped1, "session 1 ended on its stop");
    assert!(up2, "session 2 came up");
    assert!(
        live2
            .iter()
            .any(|e| text_of(e).contains(stderr_canary.as_str())),
        "the harness's last words are in the transcript: {live2:#?}"
    );
    assert!(ended2, "session 2 ended when its harness died");
    let s2 = session(&app, &seeded, &sid2).await;
    assert!(
        s2["detail"].as_str().is_some_and(|d| d.contains("code 3")),
        "the server knows HOW it ended: {s2}"
    );
}
