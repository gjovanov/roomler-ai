//! WebSocket signaling loop against `/ws?role=agent&token=...`.
//!
//! This is deliberately media-free for v1: we speak the `rc:*` protocol from
//! `roomler-ai-remote-control::signaling`, send `rc:agent.hello` on connect,
//! auto-grant consent (per AccessPolicy default for self-controlled hosts),
//! and reply to an `rc:sdp.offer` by terminating the session with
//! `EndReason::Error` until the WebRTC PeerConnection is wired in a later
//! commit.
//!
//! Reconnect strategy: exponential backoff capped at 60 s. The tokio-level
//! loop never panics on network errors — it logs and retries.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use roomler_ai_remote_control::{
    models::{AgentCaps, DisplayInfo, EndReason, OsKind},
    signaling::{ClientMsg, ServerMsg},
};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::config::AgentConfig;

/// Drive the signaling loop forever. Returns only on fatal error (e.g.
/// auth rejection) or shutdown signal.
pub async fn run(cfg: AgentConfig, shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            info!("shutdown signalled; exiting signaling loop");
            return Ok(());
        }

        match connect_once(&cfg, shutdown.clone()).await {
            Ok(()) => {
                info!("signaling connection closed cleanly, reconnecting");
                backoff = Duration::from_secs(1);
            }
            Err(ConnectError::AuthRejected) => {
                // Server refused our agent token. Nothing we can do here — the
                // user needs to re-enroll.
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
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), ConnectError> {
    let url = format!(
        "{}?token={}&role=agent",
        cfg.ws_url(),
        urlencode(&cfg.agent_token)
    );
    info!(%url, "connecting to signaling server");

    let (mut ws, response) = connect_async(&url).await.map_err(|e| {
        // tungstenite returns a specific Http error when the server rejects
        // the upgrade with a 4xx (our path returns 401 for a bad token).
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

    // Main loop: read inbound messages + react. Shutdown is checked every
    // tick so Ctrl-C exits promptly.
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("shutdown signalled; closing ws");
                    let _ = ws.send(Message::Close(None)).await;
                    return Ok(());
                }
            }
            maybe_msg = ws.next() => match maybe_msg {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ServerMsg>(&text) {
                        Ok(parsed) => handle_server_msg(&mut ws, parsed).await?,
                        Err(e) => debug!(%e, text = %text.as_str(), "ignoring non-rc:* frame"),
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    let _ = ws.send(Message::Pong(data)).await;
                }
                Some(Ok(Message::Close(_))) | None => {
                    info!("ws closed by peer");
                    return Ok(());
                }
                Some(Err(e)) => {
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
) -> Result<(), ConnectError> {
    match msg {
        ServerMsg::Request {
            session_id,
            controller_user_id,
            controller_name,
            permissions,
            consent_timeout_secs,
        } => {
            info!(
                %session_id, %controller_user_id, %controller_name,
                ?permissions, consent_timeout_secs,
                "incoming session request — auto-granting for MVP"
            );
            // TODO: run consent UI. For now, auto-grant (matches the doc's
            // "self-controlling-self" default at §11.2).
            send_msg(ws, &ClientMsg::Consent { session_id, granted: true })
                .await
                .map_err(|e| ConnectError::Transient(e.context("sending consent")))?;
        }
        ServerMsg::SdpOffer { session_id, sdp, ice_servers: _ } => {
            // Media isn't wired yet. Cleanly decline with a terminate +
            // Error reason so the controller sees a clear failure instead
            // of hanging on the negotiating phase.
            warn!(
                %session_id, sdp_len = sdp.len(),
                "rc:sdp.offer received but WebRTC peer is not yet implemented in this agent build"
            );
            send_msg(
                ws,
                &ClientMsg::Terminate {
                    session_id,
                    reason: EndReason::Error,
                },
            )
            .await
            .map_err(|e| ConnectError::Transient(e.context("sending terminate")))?;
        }
        ServerMsg::Ice { session_id, candidate } => {
            // We'd feed this to the PeerConnection once it exists. For now,
            // log it at debug so the reader of this log knows signaling is
            // flowing end to end.
            debug!(%session_id, ?candidate, "rc:ice received (no-op until peer is wired)");
        }
        ServerMsg::Terminate { session_id, reason } => {
            info!(%session_id, ?reason, "session terminated by server");
        }
        ServerMsg::Error { session_id, code, message } => {
            warn!(?session_id, %code, %message, "server-side rc error");
        }
        ServerMsg::Ready { session_id, ice_servers: _ }
        | ServerMsg::SessionCreated { session_id, .. } => {
            // These are controller-oriented messages and shouldn't normally
            // reach an agent. Log at debug in case the server routing is
            // ever extended.
            debug!(%session_id, ?msg, "unexpected controller-side msg on agent socket");
        }
        ServerMsg::SdpAnswer { .. } => {
            debug!("unexpected rc:sdp.answer on agent socket");
        }
        ServerMsg::Pong { .. } => {}
    }
    Ok(())
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
    // Real implementation will query the capture backend once it exists.
    vec![DisplayInfo {
        index: 0,
        name: "primary".into(),
        width_px: 1920,
        height_px: 1080,
        scale: 1.0,
        primary: true,
    }]
}

fn stub_caps() -> AgentCaps {
    AgentCaps {
        hw_encoders: vec![],
        codecs: vec!["h264".into()],
        has_input_permission: false, // stub: we don't actually inject yet
        supports_clipboard: false,
        supports_file_transfer: false,
        max_simultaneous_sessions: 1,
    }
}

fn urlencode(s: &str) -> String {
    // JWTs may include `+`, `/`, `=` — those three must be percent-escaped
    // in a query string. No need to pull in a url crate for this.
    s.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
}
