//! WebSocket signaling loop against `/ws?role=agent&token=...`.
//!
//! Handles the full rc:* handshake and owns a map of per-session
//! [`AgentPeer`] values that back each live WebRTC PeerConnection.
//!
//! Reconnect strategy: exponential backoff capped at 60 s. Fatal auth errors
//! (HTTP 401 on upgrade) exit the loop so the user can re-enroll.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use roomler_ai_remote_control::{
    models::{AgentCaps, DisplayInfo, EndReason, OsKind},
    signaling::{ClientMsg, ServerMsg},
};
use std::{collections::HashMap, time::Duration};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::config::AgentConfig;
use crate::indicator::ViewerIndicator;
use crate::peer::AgentPeer;

/// Capacity of the outbound channel peers use to push `ClientMsg` back into
/// the signaling loop (ICE trickles, terminate signals). 64 is generous for
/// one session's ICE gather phase.
const PEER_OUTBOUND_CAP: usize = 64;

/// Drive the signaling loop forever. Returns only on fatal error (e.g.
/// auth rejection) or shutdown signal.
pub async fn run(
    cfg: AgentConfig,
    encoder_preference: crate::encode::EncoderPreference,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    // One overlay handle, reused across reconnects. Failing to bring up
    // the indicator is non-fatal — the session still works, the user
    // just doesn't get the visual "you're being watched" cue.
    let indicator = match ViewerIndicator::new() {
        Ok(v) => v,
        Err(e) => {
            warn!(%e, "viewer-indicator init failed; continuing without overlay");
            ViewerIndicator::disabled()
        }
    };
    let mut backoff = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            info!("shutdown signalled; exiting signaling loop");
            return Ok(());
        }

        match connect_once(
            &cfg,
            encoder_preference,
            shutdown.clone(),
            indicator.clone(),
        )
        .await
        {
            Ok(()) => {
                info!("signaling connection closed cleanly, reconnecting");
                backoff = Duration::from_secs(1);
            }
            Err(ConnectError::AuthRejected) => {
                error!("agent token rejected; re-enrollment required");
                return Err(anyhow::anyhow!("agent token rejected by server"));
            }
            Err(ConnectError::Transient(e)) => {
                warn!(error = %e, "signaling connect failed; backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ConnectError {
    #[error("auth rejected")]
    AuthRejected,
    #[error(transparent)]
    Transient(#[from] anyhow::Error),
}

async fn connect_once(
    cfg: &AgentConfig,
    encoder_preference: crate::encode::EncoderPreference,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    indicator: ViewerIndicator,
) -> Result<(), ConnectError> {
    let url = format!(
        "{}?token={}&role=agent",
        cfg.ws_url(),
        urlencode(&cfg.agent_token)
    );
    info!(%url, "connecting to signaling server");

    let (mut ws, response) = connect_async(&url).await.map_err(|e| {
        if let tokio_tungstenite::tungstenite::Error::Http(ref resp) = e
            && resp.status().as_u16() == 401
        {
            return ConnectError::AuthRejected;
        }
        ConnectError::Transient(anyhow::Error::new(e).context("ws connect"))
    })?;
    debug!(status = ?response.status(), "ws upgrade complete");

    // Say hello.
    let hello = ClientMsg::AgentHello {
        machine_name: cfg.machine_name.clone(),
        os: detect_os(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        displays: stub_displays(),
        caps: stub_caps(),
    };
    send_msg(&mut ws, &hello).await.context("sending hello")?;
    info!("rc:agent.hello sent");

    // Outbound channel shared by all per-session peers. Peers push their
    // locally-gathered ICE candidates and state-change terminates here;
    // the main loop flushes them to the WS.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<ClientMsg>(PEER_OUTBOUND_CAP);
    let mut peers: HashMap<bson::oid::ObjectId, AgentPeer> = HashMap::new();
    // Codec selected for each pending session (computed from the
    // browser∩agent intersection when `rc:session.request` arrives, read
    // at `rc:sdp.offer` time to drive the track + encoder). Entries are
    // removed when the peer is built; orphaned entries (session
    // cancelled before SDP) get cleaned when the session is terminated.
    let mut pending_codecs: HashMap<bson::oid::ObjectId, String> = HashMap::new();

    // Keepalive. nginx + K8s ingress commonly idle-close WSes at 60-120s of
    // silence; send an application-level Ping every 25s so the connection
    // survives quiet periods between sessions.
    let mut keepalive = tokio::time::interval(Duration::from_secs(25));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await; // Swallow the immediate first tick.

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("shutdown signalled; closing ws");
                    close_all_peers(&mut peers).await;
                    let _ = ws.send(Message::Close(None)).await;
                    return Ok(());
                }
            }
            _ = keepalive.tick() => {
                if let Err(e) = ws.send(Message::Ping(Vec::new().into())).await {
                    warn!(%e, "keepalive ping failed — will reconnect");
                    close_all_peers(&mut peers).await;
                    return Err(ConnectError::Transient(anyhow::Error::new(e).context("ws ping")));
                }
            }
            Some(outbound_msg) = outbound_rx.recv() => {
                if let Err(e) = send_msg(&mut ws, &outbound_msg).await {
                    warn!(%e, "failed to flush peer-originated message");
                }
            }
            maybe_msg = ws.next() => match maybe_msg {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ServerMsg>(&text) {
                        Ok(parsed) => {
                            handle_server_msg(
                                &mut ws,
                                parsed,
                                &mut peers,
                                &mut pending_codecs,
                                &outbound_tx,
                                encoder_preference,
                                &indicator,
                            )
                            .await?;
                        }
                        Err(e) => debug!(%e, text = %text.as_str(), "ignoring non-rc:* frame"),
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    let _ = ws.send(Message::Pong(data)).await;
                }
                Some(Ok(Message::Close(_))) | None => {
                    info!("ws closed by peer");
                    close_all_peers(&mut peers).await;
                    return Ok(());
                }
                Some(Err(e)) => {
                    close_all_peers(&mut peers).await;
                    return Err(ConnectError::Transient(anyhow::Error::new(e).context("ws read")));
                }
                _ => {}
            }
        }
    }
}

async fn handle_server_msg(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    msg: ServerMsg,
    peers: &mut HashMap<bson::oid::ObjectId, AgentPeer>,
    pending_codecs: &mut HashMap<bson::oid::ObjectId, String>,
    outbound_tx: &mpsc::Sender<ClientMsg>,
    encoder_preference: crate::encode::EncoderPreference,
    indicator: &ViewerIndicator,
) -> Result<(), ConnectError> {
    match msg {
        ServerMsg::Request {
            session_id,
            controller_user_id,
            controller_name,
            permissions,
            consent_timeout_secs,
            browser_caps,
            preferred_transport,
        } => {
            // Pick the best codec for this session from the
            // intersection of (browser-advertised, agent-supported).
            // Stashed per session_id so the rc:sdp.offer handler can
            // read it back when building the peer: that's where the
            // track codec + encoder backend are actually bound.
            let our_caps = crate::encode::caps::detect();
            let chosen = crate::encode::caps::pick_best_codec(&browser_caps, &our_caps.codecs);
            pending_codecs.insert(session_id, chosen.clone());

            // Phase Y.3: figure out which video transport this
            // session will use. Honour `preferred_transport` only if
            // the agent's own AgentCaps.transports advertises it
            // (browser × agent intersection). Otherwise fall back to
            // the WebRTC video track silently — older agents had no
            // transports field at all.
            let negotiated_transport = preferred_transport.as_deref().and_then(|t| {
                if our_caps.transports.iter().any(|s| s == t) {
                    Some(t.to_string())
                } else {
                    None
                }
            });
            info!(
                %session_id, %controller_user_id, %controller_name,
                ?permissions, consent_timeout_secs,
                browser_caps = ?browser_caps,
                chosen_codec = %chosen,
                requested_transport = ?preferred_transport,
                negotiated_transport = ?negotiated_transport,
                "incoming session request — auto-granting (see docs §11.2)"
            );
            // Show the "someone is watching" overlay on the controlled
            // host. Harmless no-op on non-Windows or when the feature
            // is disabled.
            indicator.show_session(session_id.to_hex(), controller_name.clone());
            // TODO: real consent UI. Self-host default is "no prompt".
            send_msg(
                ws,
                &ClientMsg::Consent {
                    session_id,
                    granted: true,
                },
            )
            .await
            .map_err(|e| ConnectError::Transient(e.context("sending consent")))?;
        }

        ServerMsg::SdpOffer {
            session_id,
            sdp,
            ice_servers,
        } => {
            info!(%session_id, sdp_len = sdp.len(), "rc:sdp.offer — creating peer");

            // Build a fresh peer for this session. If an old one somehow
            // exists (controller retry?), close it first so the browser sees
            // a clean answer.
            if let Some(old) = peers.remove(&session_id) {
                old.close().await;
            }

            // Read back the codec picked by `rc:session.request`. If
            // the session skipped request (some test harnesses do) or
            // the message order is broken, default to "h264" so the
            // peer still works — that's the universal fallback the
            // browser understands.
            let chosen_codec = pending_codecs
                .remove(&session_id)
                .unwrap_or_else(|| "h264".to_string());

            let peer = match AgentPeer::new(
                session_id,
                &ice_servers,
                outbound_tx.clone(),
                encoder_preference,
                chosen_codec,
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    warn!(%session_id, %e, "AgentPeer::new failed; terminating");
                    let _ = send_msg(
                        ws,
                        &ClientMsg::Terminate {
                            session_id,
                            reason: EndReason::Error,
                        },
                    )
                    .await;
                    return Ok(());
                }
            };

            let answer_sdp = match peer.handle_offer(sdp).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(%session_id, chain = ?e, "handle_offer failed; terminating");
                    peer.close().await;
                    let _ = send_msg(
                        ws,
                        &ClientMsg::Terminate {
                            session_id,
                            reason: EndReason::Error,
                        },
                    )
                    .await;
                    return Ok(());
                }
            };

            send_msg(
                ws,
                &ClientMsg::SdpAnswer {
                    session_id,
                    sdp: answer_sdp,
                },
            )
            .await
            .map_err(|e| ConnectError::Transient(e.context("sending answer")))?;
            peers.insert(session_id, peer);
            info!(%session_id, "rc:sdp.answer sent; peer is live");
        }

        ServerMsg::Ice {
            session_id,
            candidate,
        } => {
            if let Some(peer) = peers.get(&session_id) {
                if let Err(e) = peer.add_remote_candidate(candidate).await {
                    debug!(%session_id, %e, "add_remote_candidate failed");
                }
            } else {
                debug!(%session_id, "ICE for unknown session; buffering not yet supported");
            }
        }

        ServerMsg::Terminate { session_id, reason } => {
            info!(%session_id, ?reason, "session terminated by server");
            if let Some(peer) = peers.remove(&session_id) {
                peer.close().await;
            }
            // Drop any orphaned pending-codec entry for this session so
            // the map doesn't accumulate under long-running agents
            // (e.g. sessions cancelled before SDP is exchanged).
            pending_codecs.remove(&session_id);
            indicator.hide_session(session_id.to_hex());
        }

        ServerMsg::Error {
            session_id,
            code,
            message,
        } => {
            warn!(?session_id, %code, %message, "server-side rc error");
        }

        // Controller-oriented messages shouldn't reach us.
        ServerMsg::Ready { session_id, .. }
        | ServerMsg::SessionCreated { session_id, .. }
        | ServerMsg::SdpAnswer { session_id, .. } => {
            debug!(%session_id, "unexpected controller-side msg on agent socket");
        }
        ServerMsg::Pong { .. } => {}
    }
    Ok(())
}

async fn close_all_peers(peers: &mut HashMap<bson::oid::ObjectId, AgentPeer>) {
    for (_, peer) in peers.drain() {
        peer.close().await;
    }
}

async fn send_msg(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    msg: &ClientMsg,
) -> Result<()> {
    let json = serde_json::to_string(msg).context("serialising ClientMsg")?;
    ws.send(Message::text(json)).await.context("ws send")?;
    Ok(())
}

fn detect_os() -> OsKind {
    match std::env::consts::OS {
        "linux" => OsKind::Linux,
        "macos" => OsKind::Macos,
        "windows" => OsKind::Windows,
        _ => OsKind::Linux,
    }
}

fn stub_displays() -> Vec<DisplayInfo> {
    // Real enumeration via `crate::displays::enumerate` (scrap-backed on
    // Windows / Linux / macOS). Falls back to a single 1920×1080 entry
    // on builds without `scrap-capture` or hosts where enumeration
    // fails. Kept named `stub_displays` for continuity with the
    // pre-0.1.31 call site; can be renamed once the rest of the
    // hello-preamble stubs are audited.
    crate::displays::enumerate()
}

fn stub_caps() -> AgentCaps {
    // Real probe via encode::caps; replaces the empty-vec stub. The
    // resulting AgentCaps populates the rc:agent.hello payload, which
    // the server persists into the agents collection and surfaces in
    // the admin UI (2A.2).
    crate::encode::caps::detect()
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}
