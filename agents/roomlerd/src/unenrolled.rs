// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-43 P3a — the control plane a GUI worker with no enrollment talks to.
//!
//! A one-row Mac's GUI worker (`run --supervised --unenrolled`) holds no agent
//! token and dials no server: the root daemon holds the enrollment and hands it
//! each session over the delegation channel. But the worker serves those
//! sessions from inside [`crate::signaling`]'s `connect_once`, in the
//! `next_delegated` arm, and that loop runs on a control WS. So this is one: a
//! small WS server on loopback, in the worker's own process, that answers pings
//! and discards everything else. The loop dials it like any server, which leaves
//! the session path every platform shares as it is.
//!
//! ## Who can reach it, and what it can reach
//!
//! The handshake must carry a secret minted for this process, so another local
//! process that dials the port gets a 401 and no socket. Nothing here ever sends
//! a frame of its own.
//!
//! The secret proves the worker to the stand-in, and nothing proves the stand-in
//! to the worker. If the listener ended while the worker lived, its port would
//! be free, any local process could bind it, and the loop's reconnect would dial
//! that process, hand it the secret and act on what it sent: a control plane for
//! a process holding the person's Screen Recording and Accessibility grants. So
//! the two share a lifetime. When the listener's task ends, for any reason, the
//! process exits at once with [`STAND_IN_LOST_EXIT_CODE`], and the supervisor
//! starts a new worker on a new port with a new secret. There is no path back to
//! an old port.
//!
//! A second gate does not depend on who holds the port: with
//! [`crate::signaling::OrgCtx::unenrolled`] the loop drops every text frame off
//! this socket, so a session reaches this process through the root's delegation
//! channel and nothing else.

use std::future::Future;
use std::time::Duration;

use anyhow::Context;
use futures::StreamExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;

/// The worker's exit when the stand-in's listener has ended: never a clean one,
/// because that task ending is a defect, and the supervisor records it as a crash
/// on the root's enrollment (`macos_supervisor::crash_summary`). Taken by nothing
/// else in [`crate::watchdog`]'s list.
pub const STAND_IN_LOST_EXIT_CODE: i32 = 10;

/// Bytes of entropy in the process's secret, hex-encoded on the wire.
const SECRET_BYTES: usize = 32;

/// How long a dialler gets to finish the handshake. Past this it holds nothing.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(5);

/// How long the accept loop waits after a failed accept. The usual cause is the
/// process running out of descriptors, which passes, and the listener still holds
/// the port meanwhile.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);

/// A running stand-in: where it listens, and the secret the loop dials it with.
pub struct StandIn {
    port: u16,
    secret: String,
}

impl StandIn {
    /// Bind and start serving, for as long as this process lives.
    pub async fn start() -> anyhow::Result<Self> {
        Self::start_with(std::future::pending(), exit_for_lost_listener).await
    }

    /// [`Self::start`] with what ends the listener and what follows it given, so
    /// a test can end one and watch the rest. `die` is pending for good in
    /// production, where nothing asks the listener to end.
    async fn start_with(
        die: impl Future<Output = ()> + Send + 'static,
        on_lost: fn(),
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("binding the stand-in control plane on loopback")?;
        let port = listener
            .local_addr()
            .context("reading the stand-in's port")?
            .port();
        let secret = mint_secret();
        let listening = tokio::spawn(accept_loop(listener, secret.clone(), die));
        tokio::spawn(async move {
            let end = listening.await;
            // Only a runtime going away cancels this task, and the process goes
            // with it. Nothing here aborts it.
            if matches!(&end, Err(e) if e.is_cancelled()) {
                return;
            }
            tracing::error!(
                ?end,
                "the stand-in control plane's listener ended; exiting so that no reconnect \
                 can reach whoever binds its port next (FR-43 P3a)"
            );
            on_lost();
        });
        Ok(Self { port, secret })
    }

    /// The URL the worker's view of its config names as its server.
    pub fn server_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The URL its signalling loop dials.
    pub fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/ws", self.port)
    }

    /// What its signalling loop dials with, in the place of an agent token.
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

fn exit_for_lost_listener() {
    std::process::exit(STAND_IN_LOST_EXIT_CODE);
}

fn mint_secret() -> String {
    use rand::RngCore;
    let mut raw = [0u8; SECRET_BYTES];
    rand::rng().fill_bytes(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

async fn accept_loop(
    listener: TcpListener,
    secret: String,
    die: impl Future<Output = ()> + Send + 'static,
) {
    tokio::pin!(die);
    loop {
        tokio::select! {
            () = &mut die => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(serve(stream, secret.clone()));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "stand-in control plane: accept failed; retrying");
                    tokio::time::sleep(ACCEPT_RETRY).await;
                }
            },
        }
    }
}

/// One connection: the handshake, then read until it ends. tungstenite answers a
/// ping as it reads, and the hello, the heartbeats and the rest have nowhere to
/// go: the root's row says all of that.
async fn serve(stream: TcpStream, secret: String) {
    // tungstenite's handshake `Callback` fixes the closure's error type.
    #[allow(clippy::result_large_err)]
    let check = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        if offers_secret(req.uri().query(), &secret) {
            Ok(resp)
        } else {
            let mut refused = ErrorResponse::new(None);
            *refused.status_mut() = http::StatusCode::UNAUTHORIZED;
            Err(refused)
        }
    };
    let handshake = tokio_tungstenite::accept_hdr_async(stream, check);
    let Ok(Ok(mut ws)) = tokio::time::timeout(HANDSHAKE_DEADLINE, handshake).await else {
        return;
    };
    while let Some(Ok(_)) = ws.next().await {}
}

/// Does this query string carry the secret as its `token`, as `connect_once`
/// sends it? The secret is hex, so the URL encoding leaves it unchanged.
fn offers_secret(query: Option<&str>, secret: &str) -> bool {
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|pair| pair.strip_prefix("token="))
        .any(|token| crate::delegate::secret_eq(secret, token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegate::{SessionParams, WorkerInbound};
    use crate::signaling::{self, OrgCtx};
    use futures::SinkExt;
    use roomler_ai_remote_control::permissions::Permissions;
    use roomler_ai_remote_control::signaling::{ClientMsg, ServerMsg};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio_tungstenite::tungstenite::{self, Message};

    /// How long a test waits for something that should happen.
    const PATIENCE: Duration = Duration::from_secs(30);

    fn dial_url(stand_in: &StandIn, token: &str) -> String {
        format!("{}?token={token}&role=agent&tid=", stand_in.ws_url())
    }

    #[test]
    fn only_the_token_field_with_the_secret_passes() {
        assert!(offers_secret(Some("token=s3&role=agent&tid="), "s3"));
        assert!(offers_secret(Some("role=agent&token=s3"), "s3"));
        assert!(!offers_secret(Some("token=s4&role=agent"), "s3"));
        assert!(!offers_secret(Some("xtoken=s3"), "s3"));
        assert!(!offers_secret(Some("token="), "s3"));
        assert!(!offers_secret(Some(""), "s3"));
        assert!(!offers_secret(None, "s3"));
    }

    /// Dialled as `connect_once` dials: it upgrades, answers the loop's keepalive
    /// with the same payload, and says nothing about what it is sent.
    #[tokio::test]
    async fn it_takes_its_secret_answers_pings_and_discards_the_rest() {
        let stand_in = StandIn::start().await.unwrap();
        let (mut ws, _) = crate::ws_connect::connect(&dial_url(&stand_in, stand_in.secret()))
            .await
            .expect("the secret opens it");
        ws.send(Message::Text("{\"t\":\"rc:agent.hello\"}".into()))
            .await
            .unwrap();
        ws.send(Message::Ping(b"keepalive".to_vec().into()))
            .await
            .unwrap();
        let next = tokio::time::timeout(PATIENCE, ws.next())
            .await
            .expect("a pong in time")
            .expect("the socket is open")
            .expect("a good frame");
        assert_eq!(next, Message::Pong(b"keepalive".to_vec().into()));
    }

    /// Another local process that finds the port gets a 401 and no socket.
    #[tokio::test]
    async fn anyone_without_the_secret_gets_a_401() {
        let stand_in = StandIn::start().await.unwrap();
        for url in [dial_url(&stand_in, "not-the-secret"), stand_in.ws_url()] {
            match crate::ws_connect::connect(&url).await {
                Err(tungstenite::Error::Http(resp)) => {
                    assert_eq!(resp.status(), http::StatusCode::UNAUTHORIZED, "{url}")
                }
                Err(e) => panic!("{url}: refused some other way: {e}"),
                Ok(_) => panic!("{url}: upgraded with no secret"),
            }
        }
    }

    /// The listener's end reaches `on_lost`, whether its task returned or
    /// panicked, and a runtime going away does not count as either.
    #[tokio::test]
    async fn the_listeners_end_is_noticed() {
        static LOST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        fn count() {
            LOST.fetch_add(1, Ordering::SeqCst);
        }
        for die in [true, false] {
            let before = LOST.load(Ordering::SeqCst);
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let _stand_in = StandIn::start_with(
                async move {
                    let _ = rx.await;
                    assert!(!die, "the listener's task panics here");
                },
                count,
            )
            .await
            .unwrap();
            tokio::task::yield_now().await;
            assert_eq!(LOST.load(Ordering::SeqCst), before, "still listening");
            tx.send(()).unwrap();
            tokio::time::timeout(PATIENCE, async {
                while LOST.load(Ordering::SeqCst) == before {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the end is noticed");
        }
    }

    /// Set in the child [`the_worker_ends_with_the_listener`] starts.
    const CHILD_ENV: &str = "ROOMLERD_TEST_STAND_IN_CHILD";
    /// What the child prints if a dial reaches whoever took the port.
    const DIALLED_THE_PORT: &str = "A-DIAL-REACHED-THE-FREED-PORT";

    /// The whole of it, in a real process: a worker's signalling loop up on the
    /// stand-in, then the listener gone. The process must end with the code the
    /// supervisor reads, and before any reconnect reaches a port someone else
    /// could hold by then.
    #[test]
    fn the_worker_ends_with_the_listener() {
        if std::env::var_os(CHILD_ENV).is_some() {
            child_loses_its_listener();
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unenrolled::tests::the_worker_ends_with_the_listener",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .expect("the test binary runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(STAND_IN_LOST_EXIT_CODE),
            "stdout: {stdout}\nstderr: {stderr}"
        );
        assert!(!stdout.contains(DIALLED_THE_PORT), "{stdout}");
    }

    fn child_loses_its_listener() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (kill, killed) = tokio::sync::oneshot::channel::<()>();
            let stand_in = StandIn::start_with(
                async move {
                    let _ = killed.await;
                    panic!("the listener dies here");
                },
                exit_for_lost_listener,
            )
            .await
            .unwrap();
            let port = stand_in.port;
            let worker = spawn_worker(&stand_in, true).await;
            wait_connected(&worker).await;
            kill.send(()).unwrap();
            // A local process taking the freed port, as fast as it can.
            loop {
                if let Ok(rogue) = TcpListener::bind(("127.0.0.1", port)).await {
                    if tokio::time::timeout(Duration::from_secs(10), rogue.accept())
                        .await
                        .is_ok()
                    {
                        println!("{DIALLED_THE_PORT}");
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            // Reached only if the process outlived its listener by that long,
            // which the parent reads from this exit code.
            std::process::exit(0);
        });
    }

    /// A worker's signalling loop, as `run_cmd` builds it on the view of a
    /// person's file, and the two ends of its delegation channel.
    struct Worker {
        connected: Arc<AtomicBool>,
        to_worker: tokio::sync::mpsc::Sender<WorkerInbound>,
        from_worker: tokio::sync::mpsc::Receiver<ClientMsg>,
        stop: tokio::sync::watch::Sender<bool>,
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
        _dir: tempfile::TempDir,
    }

    async fn spawn_worker(stand_in: &StandIn, unenrolled: bool) -> Worker {
        spawn_worker_on(
            &stand_in.server_url(),
            &stand_in.ws_url(),
            stand_in.secret(),
            unenrolled,
        )
        .await
    }

    async fn spawn_worker_on(server: &str, ws: &str, secret: &str, unenrolled: bool) -> Worker {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = crate::config::test_fixture().for_unenrolled_worker(server, ws, secret);
        let (to_worker, inbound) = tokio::sync::mpsc::channel(64);
        let (outbound, from_worker) = tokio::sync::mpsc::channel(64);
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let connected = Arc::new(AtomicBool::new(false));
        let (view_tx, _view_rx) =
            tokio::sync::watch::channel(tunnel_core::localapi::OverlayView::default());
        let broker = crate::consent::ConsentBroker::new(
            crate::consent::Mode::AutoGrant,
            dir.path().join("consent"),
        )
        .unwrap();
        let ctx = OrgCtx::primary(crate::token_slot::TokenSlot::new(secret.to_string()));
        let ctx = if unenrolled { ctx.unenrolled() } else { ctx };
        let remote_cfg = crate::remote_config::RemoteConfigServices::new(
            path,
            Arc::new(tokio::sync::Mutex::new(())),
            false,
            false,
        );
        let task = tokio::spawn(signaling::run(
            ctx,
            crate::delegate::Delegation::Worker(crate::delegate::WorkerLink { inbound, outbound }),
            cfg,
            crate::encode::EncoderPreference::Software,
            stop_rx,
            connected.clone(),
            view_tx,
            Default::default(),
            broker,
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            remote_cfg,
            crate::rc_sessions::RcSessionRegistry::new(),
        ));
        Worker {
            connected,
            to_worker,
            from_worker,
            stop,
            task,
            _dir: dir,
        }
    }

    async fn wait_connected(worker: &Worker) {
        tokio::time::timeout(PATIENCE, async {
            while !worker.connected.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the loop comes up on the stand-in");
    }

    /// An offer as a browser makes one, and the peer behind it. The caller closes
    /// the peer: dropping one frees none of its sockets.
    async fn browser_offer() -> (Arc<webrtc::peer_connection::RTCPeerConnection>, String) {
        let api = webrtc::api::APIBuilder::new().build();
        let pc = Arc::new(
            api.new_peer_connection(Default::default())
                .await
                .expect("a peer"),
        );
        pc.create_data_channel("control", None)
            .await
            .expect("a channel");
        let offer = pc.create_offer(None).await.expect("an offer");
        pc.set_local_description(offer.clone())
            .await
            .expect("the local description");
        (pc, offer.sdp)
    }

    fn sdp_offer(session_id: bson::oid::ObjectId, sdp: String) -> ServerMsg {
        ServerMsg::SdpOffer {
            session_id,
            sdp,
            ice_servers: Vec::new(),
            extauth_mac: None,
            peer_auth: None,
        }
    }

    /// P3a's point: with no enrollment, on the stand-in alone, a session the root
    /// hands over is answered on the delegation channel.
    #[tokio::test]
    async fn a_worker_with_no_enrollment_answers_a_delegated_offer() {
        let stand_in = StandIn::start().await.unwrap();
        let mut worker = spawn_worker(&stand_in, true).await;
        wait_connected(&worker).await;

        let session_id = bson::oid::ObjectId::new();
        worker
            .to_worker
            .send(WorkerInbound::Params(Box::new(SessionParams {
                session_id: session_id.to_hex(),
                codec: "h264".into(),
                transport: None,
                chroma: None,
                chunk_framing: false,
                audio: false,
                permissions: Permissions::default(),
                controller_name: "A viewer".into(),
                input_mode: None,
                asking_org: Some("Another org".into()),
                record: None,
                rc_overlay: None,
                idle_secs: None,
                lock_on_end: None,
                media_legacy: None,
            })))
            .await
            .unwrap();
        let (browser, sdp) = browser_offer().await;
        worker
            .to_worker
            .send(WorkerInbound::Msg(Box::new(sdp_offer(session_id, sdp))))
            .await
            .unwrap();

        let answered = tokio::time::timeout(PATIENCE, async {
            loop {
                match worker.from_worker.recv().await {
                    Some(ClientMsg::SdpAnswer {
                        session_id: sid, ..
                    }) => return sid,
                    Some(_) => {}
                    None => panic!("the worker's channel closed"),
                }
            }
        })
        .await
        .expect("an answer in time");
        assert_eq!(answered, session_id);

        worker
            .to_worker
            .send(WorkerInbound::Msg(Box::new(ServerMsg::Terminate {
                session_id,
                reason: roomler_ai_remote_control::models::EndReason::ControllerHangup,
                limit: None,
            })))
            .await
            .unwrap();
        let _ = browser.close().await;
        let _ = worker.stop.send(true);
        let _ = tokio::time::timeout(PATIENCE, worker.task).await;
    }

    /// A control plane that sends an offer the moment it is dialled: what a
    /// process that took the port would do. Returns what the worker answers.
    async fn offering_server(sdp: String) -> (String, tokio::sync::mpsc::Receiver<String>, u16) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (said_tx, said_rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let offer = sdp_offer(bson::oid::ObjectId::new(), sdp);
            ws.send(Message::Text(serde_json::to_string(&offer).unwrap().into()))
                .await
                .unwrap();
            while let Some(Ok(frame)) = ws.next().await {
                if let Message::Text(text) = frame {
                    let _ = said_tx.send(text.to_string()).await;
                }
            }
        });
        (format!("ws://127.0.0.1:{port}/ws"), said_rx, port)
    }

    /// Reads what the worker says until an answer or `within`.
    async fn answers_within(
        said: &mut tokio::sync::mpsc::Receiver<String>,
        within: Duration,
    ) -> bool {
        tokio::time::timeout(within, async {
            while let Some(line) = said.recv().await {
                if line.contains("rc:sdp.answer") {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    }

    /// The second gate: whatever holds the socket, an unenrolled worker takes no
    /// session off it. The control is the same server and the same offer with
    /// the switch off, which is answered, so the silence is the gate's.
    #[tokio::test]
    async fn an_offer_off_the_socket_is_dropped() {
        let (browser, sdp) = browser_offer().await;

        let (url, mut said, port) = offering_server(sdp.clone()).await;
        let server = format!("http://127.0.0.1:{port}");
        let worker = spawn_worker_on(&server, &url, "s3", false).await;
        assert!(
            answers_within(&mut said, PATIENCE).await,
            "control: an enrolled worker answers its own control plane"
        );
        let _ = worker.stop.send(true);
        let _ = tokio::time::timeout(PATIENCE, worker.task).await;

        let (url, mut said, port) = offering_server(sdp).await;
        let server = format!("http://127.0.0.1:{port}");
        let worker = spawn_worker_on(&server, &url, "s3", true).await;
        wait_connected(&worker).await;
        assert!(
            !answers_within(&mut said, Duration::from_secs(5)).await,
            "an unenrolled worker took a session off its socket"
        );
        let _ = worker.stop.send(true);
        let _ = tokio::time::timeout(PATIENCE, worker.task).await;
        let _ = browser.close().await;
    }
}
