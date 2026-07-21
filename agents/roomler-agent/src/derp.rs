//! Agent-side `/derp` WebSocket owner (NAT-traversal Phase D, DERP).
//!
//! When DERP is enabled (`ROOMLER_NODE_OVERLAY_DERP=1`) the agent opens ONE
//! persistent WSS to the server's `/derp` relay, registers its WG pubkey, and
//! bridges it to the node's [`DerpMux`]: outbound carrier frames drain to the
//! socket, inbound frames are demuxed by source pubkey to the right peer's
//! [`DerpConn`](tunnel_core::transport::derp::DerpConn). This is the transport
//! the [`RelayCoordinator`](tunnel_core::overlay::relay_link) uses to carry WG
//! between two BOTH-UDP-blocked peers — both dial OUT over TCP/TLS-443, so no
//! UDP is needed anywhere.
//!
//! The mux + this task are created per overlay connection (in
//! [`crate::overlay::maybe_start`]); the task holds only a `Weak<DerpMux>` and
//! owns the outbound receiver, so when the runtime tears down (its strong
//! `Arc<DerpMux>` + all `DerpConn`s drop) the outbound channel closes and this
//! task exits — no leak across the agent's WS reconnects.

use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use tunnel_core::transport::derp::DerpMux;

/// Hard upper bound on a single `/derp` connect attempt (mirrors the control
/// WS's `WS_CONNECT_TIMEOUT`): a hung TLS handshake must not stall the backoff.
const DERP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Reconnect backoff ceiling.
const DERP_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Derive the `/derp` WSS URL from the control `ws_url` (`wss://host/ws`).
fn derp_url_from_ws(ws_url: &str) -> String {
    match ws_url.strip_suffix("/ws") {
        Some(base) => format!("{base}/derp"),
        // An operator override that isn't `/ws`-suffixed: swap the last segment.
        None => match ws_url.rsplit_once('/') {
            Some((base, _)) => format!("{base}/derp"),
            None => format!("{ws_url}/derp"),
        },
    }
}

/// Spawn the persistent `/derp` WS owner for this overlay connection. `ws_url`
/// is the control WS URL (`cfg.ws_url()`); `agent_token` authenticates exactly
/// like the control WS (`role=agent`). Holds a `Weak` to `mux` and owns
/// `outbound_rx`, so it exits when the runtime tears the mux down.
pub fn spawn(
    ws_url: &str,
    agent_token: &str,
    mux: &Arc<DerpMux>,
    outbound_rx: mpsc::Receiver<Vec<u8>>,
) {
    let full_url = format!(
        "{}?token={}&role=agent",
        derp_url_from_ws(ws_url),
        crate::signaling::urlencode(agent_token)
    );
    let reg_frame = mux.registration_frame();
    let weak = Arc::downgrade(mux);
    tokio::spawn(run(full_url, reg_frame, weak, outbound_rx));
}

async fn run(
    url: String,
    reg_frame: Vec<u8>,
    mux: Weak<DerpMux>,
    mut outbound_rx: mpsc::Receiver<Vec<u8>>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        // The mux is gone (runtime torn down) ⇒ stop reconnecting.
        if mux.upgrade().is_none() {
            debug!("overlay derp: mux dropped; stopping /derp WS owner");
            return;
        }
        match tokio::time::timeout(DERP_CONNECT_TIMEOUT, connect_async(&url)).await {
            Ok(Ok((ws, _))) => {
                backoff = Duration::from_secs(1);
                let (mut tx, mut rx) = ws.split();
                // Register our pubkey as the first frame.
                if tx
                    .send(Message::Binary(reg_frame.clone().into()))
                    .await
                    .is_err()
                {
                    warn!("overlay derp: registration send failed; reconnecting");
                    if let Some(m) = mux.upgrade() {
                        m.mark_down();
                    }
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                if let Some(m) = mux.upgrade() {
                    m.mark_up();
                } else {
                    return;
                }
                info!("overlay derp: /derp WS connected + registered");

                // Pump until either half dies (or the mux is torn down).
                loop {
                    tokio::select! {
                        out = outbound_rx.recv() => match out {
                            // A carrier frame to relay: [peer_pubkey||payload].
                            Some(frame) => {
                                if tx.send(Message::Binary(frame.into())).await.is_err() {
                                    break;
                                }
                            }
                            // All senders gone ⇒ the mux + every DerpConn dropped
                            // (runtime torn down): we're done for good.
                            None => {
                                debug!("overlay derp: outbound closed; /derp WS owner exiting");
                                return;
                            }
                        },
                        inb = rx.next() => match inb {
                            Some(Ok(Message::Binary(data))) => match mux.upgrade() {
                                Some(m) => m.deliver(&data[..]),
                                None => return,
                            },
                            // Keep the corp middlebox happy; ignore text.
                            Some(Ok(Message::Ping(p))) => {
                                let _ = tx.send(Message::Pong(p)).await;
                            }
                            Some(Ok(_)) => {}
                            Some(Err(_)) | None => break,
                        },
                    }
                }
                if let Some(m) = mux.upgrade() {
                    m.mark_down();
                }
                warn!("overlay derp: /derp WS closed; reconnecting");
            }
            Ok(Err(e)) => warn!(%e, "overlay derp: /derp connect failed"),
            Err(_) => warn!("overlay derp: /derp connect timed out"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(DERP_BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derp_url_swaps_ws_for_derp() {
        assert_eq!(
            derp_url_from_ws("wss://roomler.ai/ws"),
            "wss://roomler.ai/derp"
        );
        assert_eq!(
            derp_url_from_ws("ws://localhost:3000/ws"),
            "ws://localhost:3000/derp"
        );
        // Non-/ws override: swap the last path segment.
        assert_eq!(derp_url_from_ws("wss://host/control"), "wss://host/derp");
    }
}
