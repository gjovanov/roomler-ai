//! `/derp` — a pubkey-addressed WebSocket relay for the both-UDP-blocked
//! overlay carrier tier (NAT-traversal Phase D, DERP).
//!
//! # Why this exists
//!
//! The overlay carrier cascade (LAN-direct → public-direct → srflx-punch →
//! single-relay) covers every peer pair EXCEPT one: two nodes that are BOTH on
//! all-UDP-blocked networks (a strict corp firewall that permits only
//! TCP/TLS-443). Single-relay provably can't serve them — exactly one side must
//! be the raw-UDP dialer, and neither has UDP. DERP breaks the deadlock because
//! **both peers dial OUT** over TCP/TLS-443 to this rendezvous relay: no UDP, no
//! inbound-reachable allocation, no TURN permission model. It's Tailscale's
//! DERP, scoped to a single overlay network.
//!
//! # What the relay does
//!
//! It is a dumb, opaque, pubkey-keyed forwarder. A node opens ONE `/derp` WSS,
//! sends its 32-byte WireGuard public key as the first (registration) frame,
//! then exchanges binary data frames of the form `[dst_pubkey(32) || payload]`.
//! The relay rewrites the prefix to the SENDER's pubkey and delivers
//! `[src_pubkey(32) || payload]` to the destination — but ONLY to a peer
//! registered in the **same overlay network** (hard tenant/network isolation,
//! the same scope the netmap fan-out enforces). The payload is opaque WG
//! ciphertext; the relay never inspects or decrypts it — WireGuard is
//! end-to-end between the two nodes.
//!
//! # Security
//!
//! - **Auth**: the agent JWT, same audience as `/ws?role=agent`
//!   (`verify_agent_token`). The DB is authoritative for the agent's tenant, so
//!   a forged tenant claim can't widen scope.
//! - **Registration authz**: a node may only register a pubkey that matches its
//!   OWN `overlay_nodes.wg_public_key` — it can't claim a peer's key to
//!   intercept that peer's frames.
//! - **Network scoping**: a frame is delivered only to a pubkey registered in
//!   the sender's own network. The `(network_id, pubkey)` registry key makes a
//!   cross-network delivery structurally impossible.
//!
//! Placement (v1): on the `roomler2` API pods (hostNetwork, :443), behind the
//! dedicated `/derp` nginx `location`. The registry is in-memory on a
//! single-replica Recreate deployment, so a web deploy severs all DERP links
//! (they rebuild via the carrier `dead` latch) and it can't scale past one
//! replica — fine for the handful of corp pairs this tier serves.

use axum::{
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::oid::ObjectId;
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::handler::WsParams;
use super::overlay::{NodeIdentity, current_node};
use crate::state::AppState;

/// 32-byte WireGuard public key — the DERP addressing unit.
pub type DerpPubKey = [u8; 32];

/// Registry key. A pubkey is only reachable WITHIN its overlay network, so the
/// network id is part of the key — a forward lookup can never cross a network
/// boundary (the same hard isolation the netmap enforces).
pub type DerpKey = (ObjectId, DerpPubKey);

/// `(network_id, dst_pubkey)` → a bounded sender feeding that peer's live WS
/// write task. Shared across every `/derp` connection (lives in `AppState`).
pub type DerpRegistry = Arc<DashMap<DerpKey, mpsc::Sender<Vec<u8>>>>;

/// Per-connection outbound queue depth. Bounded so a slow or hostile consumer
/// can't grow the relay's memory without bound; on overflow we DROP the frame
/// (WG/QUIC are loss-tolerant — a dropped carrier datagram just retransmits).
const DERP_SEND_QUEUE: usize = 256;

/// Max DERP frame = `[pubkey(32) || WG-carrier datagram]`. The carrier datagram
/// stays ≤ the overlay MTU (~1280–1420) + WG overhead; 2 KiB matches the relay
/// carrier's `MAX_DATAGRAM` with headroom for the 32-byte pubkey prefix and is
/// comfortably ≥ `mtu + WG_OVERHEAD + 32`.
const DERP_MAX_FRAME: usize = 2048;

/// `GET /derp?token=<agent-jwt>` — upgrade to the DERP relay WS. Agent-only,
/// same audience as `/ws?role=agent`.
pub async fn derp_upgrade(
    State(state): State<AppState>,
    Query(params): Query<WsParams>,
    ws: WebSocketUpgrade,
) -> Response {
    let claims = match state.auth.verify_agent_token(&params.token) {
        Ok(c) => c,
        Err(_) => {
            return Response::builder()
                .status(401)
                .body("Unauthorized (derp)".into())
                .unwrap();
        }
    };
    let agent_id = match ObjectId::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return Response::builder()
                .status(400)
                .body("Invalid agent ID".into())
                .unwrap();
        }
    };
    ws.on_upgrade(move |socket| handle_derp_socket(state, socket, agent_id))
}

/// Drive one DERP connection: resolve the agent's node, validate its
/// registration pubkey, add it to the registry, then pump frames until the
/// socket closes.
async fn handle_derp_socket(state: AppState, socket: WebSocket, agent_id: ObjectId) {
    // Resolve this agent's overlay node → its network + its stored pubkey.
    let node = match current_node(&state, NodeIdentity::Agent(agent_id)).await {
        Some(n) => n,
        None => {
            debug!(%agent_id, "derp: no overlay node for agent; closing");
            return;
        }
    };
    let network_id = node.network_id;

    let (mut ws_tx, mut ws_rx) = socket.split();

    // First frame MUST be the 32-byte registration pubkey, and it MUST equal
    // this node's own `wg_public_key` — a node can only register ITS OWN key,
    // never a peer's (which would let it intercept that peer's frames).
    let self_pubkey: DerpPubKey = match ws_rx.next().await {
        Some(Ok(Message::Binary(b))) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b[..]);
            k
        }
        _ => {
            debug!(%agent_id, "derp: bad or absent registration frame; closing");
            return;
        }
    };
    if BASE64.encode(self_pubkey) != node.wg_public_key {
        warn!(%agent_id, "derp: registration pubkey != node's wg_public_key; refusing");
        return;
    }

    let key: DerpKey = (network_id, self_pubkey);
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(DERP_SEND_QUEUE);
    // Last-writer-wins on re-registration: a reconnect for the same pubkey
    // replaces the stale sender (corp middleboxes leave half-open TCP, so the
    // old entry would otherwise black-hole inbound frames). The old socket's
    // read loop keeps working as a SENDER until it notices its own close; only
    // inbound routing moves to the new connection.
    state.derp_registry.insert(key, out_tx.clone());
    info!(%agent_id, %network_id, "derp: node registered");

    // Write task: drain outbound frames → WS binary.
    let mut write = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(frame.into())).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    // Read loop: forward each data frame to its dst within THIS network.
    loop {
        tokio::select! {
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Binary(frame))) => {
                    forward_frame(&state.derp_registry, network_id, &self_pubkey, &frame[..]);
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                // Ignore text / ping / pong (axum auto-pongs).
                Some(Ok(_)) => {}
            },
            // If the write task ends (peer's socket died on the write side),
            // stop reading too.
            _ = &mut write => break,
        }
    }

    // Deregister — but ONLY if we're still the registered sender. A newer
    // reconnect (last-writer-wins) may have replaced us; we must not evict it.
    state
        .derp_registry
        .remove_if(&key, |_, tx| tx.same_channel(&out_tx));
    write.abort();
    info!(%agent_id, %network_id, "derp: node disconnected");
}

/// Parse `[dst_pubkey(32) || payload]` sent by `src_pubkey`, and forward
/// `[src_pubkey(32) || payload]` to the destination — but ONLY to a peer
/// registered in the SAME `network_id` (hard scope). Silently drops on: a short
/// or oversized frame, an unknown dst (peer offline / not in this network), or a
/// full destination queue (the carrier is loss-tolerant).
fn forward_frame(
    registry: &DerpRegistry,
    network_id: ObjectId,
    src_pubkey: &DerpPubKey,
    frame: &[u8],
) {
    if frame.len() < 32 || frame.len() > DERP_MAX_FRAME {
        return;
    }
    let mut dst = [0u8; 32];
    dst.copy_from_slice(&frame[..32]);
    let payload = &frame[32..];

    // Clone the sender out of the shard guard so we don't hold the DashMap lock
    // across the (non-blocking) try_send.
    let sender = match registry.get(&(network_id, dst)) {
        Some(r) => r.clone(),
        None => return, // dst offline or not in this network
    };

    let mut out = Vec::with_capacity(32 + payload.len());
    out.extend_from_slice(src_pubkey);
    out.extend_from_slice(payload);
    // Bounded, non-blocking: drop on overflow (loss-tolerant carrier).
    let _ = sender.try_send(out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(byte: u8) -> DerpPubKey {
        [byte; 32]
    }

    fn frame(dst: &DerpPubKey, payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(32 + payload.len());
        f.extend_from_slice(dst);
        f.extend_from_slice(payload);
        f
    }

    #[test]
    fn forwards_within_network_and_rewrites_src() {
        let reg: DerpRegistry = Arc::new(DashMap::new());
        let net = ObjectId::new();
        let (a, b) = (pk(0xAA), pk(0xBB));
        let (b_tx, mut b_rx) = mpsc::channel::<Vec<u8>>(8);
        reg.insert((net, b), b_tx);

        // A → B with payload [1,2,3]; B should receive [A-pubkey || 1,2,3].
        forward_frame(&reg, net, &a, &frame(&b, &[1, 2, 3]));

        let got = b_rx.try_recv().expect("B should receive the frame");
        assert_eq!(&got[..32], &a, "src prefix must be rewritten to the sender");
        assert_eq!(&got[32..], &[1, 2, 3]);
    }

    #[test]
    fn never_crosses_a_network_boundary() {
        let reg: DerpRegistry = Arc::new(DashMap::new());
        let (net_a, net_b) = (ObjectId::new(), ObjectId::new());
        let (a, b) = (pk(0xAA), pk(0xBB));
        // Same pubkey B registered in BOTH networks with distinct channels.
        let (b_in_a_tx, mut b_in_a_rx) = mpsc::channel::<Vec<u8>>(8);
        let (b_in_b_tx, mut b_in_b_rx) = mpsc::channel::<Vec<u8>>(8);
        reg.insert((net_a, b), b_in_a_tx);
        reg.insert((net_b, b), b_in_b_tx);

        // A sends from net_a → only net_a's B receives; net_b's B never does.
        forward_frame(&reg, net_a, &a, &frame(&b, &[9]));

        assert!(b_in_a_rx.try_recv().is_ok(), "same-network dst delivered");
        assert!(
            b_in_b_rx.try_recv().is_err(),
            "cross-network dst must NOT be delivered"
        );
    }

    #[test]
    fn unknown_dst_is_dropped_silently() {
        let reg: DerpRegistry = Arc::new(DashMap::new());
        let net = ObjectId::new();
        // No registrations at all — forwarding must not panic.
        forward_frame(&reg, net, &pk(0xAA), &frame(&pk(0xCC), &[1]));
    }

    #[test]
    fn short_frame_without_full_pubkey_is_dropped() {
        let reg: DerpRegistry = Arc::new(DashMap::new());
        let net = ObjectId::new();
        let (dst_tx, mut dst_rx) = mpsc::channel::<Vec<u8>>(8);
        reg.insert((net, pk(0xBB)), dst_tx);
        // 10 bytes < 32 → no dst pubkey → dropped, nothing delivered.
        forward_frame(&reg, net, &pk(0xAA), &[0u8; 10]);
        assert!(dst_rx.try_recv().is_err());
    }
}
