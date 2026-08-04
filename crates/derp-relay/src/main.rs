//! `derp-relay` — the standalone regional DERP relay (one per relay PoP).
//!
//! A pubkey-addressed WebSocket forwarder for the overlay's both-UDP-blocked
//! carrier tier, functionally the API pods' `/derp` (see
//! `crates/api/src/ws/derp.rs`) minus everything a $5 PoP must not hold:
//!
//! - **No JWT secret.** Admission is an Ed25519 TICKET minted by the API
//!   (`remote_control::derp_ticket`); this binary holds only the PUBLIC key
//!   (`DERP_TICKET_PUBLIC_KEY`). A compromised PoP can forge nothing.
//! - **No Mongo.** The ticket carries the two facts the central relay reads
//!   from the DB: the node's overlay `network` (forwarding scope) and its
//!   `wg pubkey` (the only key it may register — checked against the first
//!   frame, so a stolen ticket can't intercept another node's traffic).
//! - **No cluster machinery.** One process per PoP; both ends of a pair are
//!   pushed the same regional URL by the API, so rendezvous is structural.
//!
//! The wire protocol is IDENTICAL to the central `/derp` (registration frame,
//! `[dst_pk(32)‖payload]` ≤ 2048 B, src-rewrite on delivery, drop-on-overflow)
//! — `DerpConn`/`DerpMux` on the agent work unchanged.
//!
//! Deployment: plain HTTP/WS on `DERP_BIND` (default `127.0.0.1:8443`); the
//! PoP's nginx terminates TLS for `derp-{region}.roomler.ai:443` and proxies
//! here (`scripts/relay-pop/`). `/healthz` serves the health cron.
//!
//! v1 scope note: the central relay additionally consults the overlay-ACL
//! DERP table (`ws/derp_acl.rs`) for ENFORCING tenants. Tickets don't carry
//! per-peer allow-lists yet, so a PoP enforces network scoping only; ACL
//! claims are the designed extension (the netmap already withholds denied
//! peers' keys from honest clients).

use axum::{
    Router,
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use roomler_ai_remote_control::derp_ticket;
// Shared through remote_control so mint and verify can never version-drift.
use roomler_ai_remote_control::derp_ticket::jsonwebtoken::DecodingKey;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// 32-byte WireGuard public key — the addressing unit.
type DerpPubKey = [u8; 32];
/// Registry key: a pubkey is reachable only WITHIN its overlay network.
type DerpKey = (String, DerpPubKey);
type Registry = Arc<DashMap<DerpKey, mpsc::Sender<Vec<u8>>>>;

/// Same limits as the central relay (`api/src/ws/derp.rs`).
const DERP_SEND_QUEUE: usize = 256;
const DERP_MAX_FRAME: usize = 2048;

#[derive(Clone)]
struct RelayState {
    registry: Registry,
    ticket_key: Arc<DecodingKey>,
}

#[derive(Deserialize)]
struct DerpParams {
    ticket: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let public_b64 = std::env::var("DERP_TICKET_PUBLIC_KEY")
        .expect("DERP_TICKET_PUBLIC_KEY (base64 raw 32-byte Ed25519 public key) is required");
    let ticket_key = derp_ticket::decoding_key_from_public_b64(&public_b64)
        .expect("DERP_TICKET_PUBLIC_KEY is not a valid Ed25519 public key");
    let bind = std::env::var("DERP_BIND").unwrap_or_else(|_| "127.0.0.1:8443".into());

    let state = RelayState {
        registry: Arc::new(DashMap::new()),
        ticket_key: Arc::new(ticket_key),
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|e| panic!("bind {bind}: {e}"));
    info!(%bind, "derp-relay listening");
    axum::serve(listener, app).await.expect("serve");
}

fn router(state: RelayState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/derp", get(derp_upgrade))
        .with_state(state)
}

/// `GET /derp?ticket=<eddsa-jwt>` — verify the ticket, then upgrade.
async fn derp_upgrade(
    State(state): State<RelayState>,
    Query(params): Query<DerpParams>,
    ws: WebSocketUpgrade,
) -> Response {
    let claims = match derp_ticket::verify(&params.ticket, &state.ticket_key) {
        Ok(c) => c,
        Err(e) => {
            debug!(%e, "derp-relay: rejected ticket");
            return Response::builder()
                .status(401)
                .body("Unauthorized (derp ticket)".into())
                .unwrap();
        }
    };
    ws.on_upgrade(move |socket| handle_socket(state, socket, claims.net, claims.pk))
}

/// Drive one connection: validate the registration frame against the ticket's
/// pubkey, register, pump frames until close. Mirrors the central relay's
/// loop (last-writer-wins re-registration, identity-gated deregistration).
async fn handle_socket(state: RelayState, socket: WebSocket, network: String, ticket_pk: String) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // First frame MUST be the 32-byte registration pubkey and MUST equal the
    // ticket's `pk` — a node can only register the key its ticket names.
    let self_pubkey: DerpPubKey = match ws_rx.next().await {
        Some(Ok(Message::Binary(b))) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b[..]);
            k
        }
        _ => {
            debug!("derp-relay: bad or absent registration frame; closing");
            return;
        }
    };
    if BASE64.encode(self_pubkey) != ticket_pk {
        warn!("derp-relay: registration pubkey != ticket pk; refusing");
        return;
    }

    let key: DerpKey = (network.clone(), self_pubkey);
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(DERP_SEND_QUEUE);
    // Last-writer-wins on re-registration (a reconnect replaces the stale
    // sender; the displaced socket keeps working as a SENDER until it closes).
    state.registry.insert(key.clone(), out_tx.clone());
    info!(%network, "derp-relay: node registered");

    let mut write = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(frame.into())).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    loop {
        tokio::select! {
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Binary(frame))) => {
                    forward_frame(&state.registry, &network, &self_pubkey, &frame[..]);
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {} // text/ping/pong — axum auto-pongs
            },
            _ = &mut write => break,
        }
    }

    // Deregister only if we're still the registered sender (a newer
    // reconnect may have replaced us; we must not evict it).
    state
        .registry
        .remove_if(&key, |_, tx| tx.same_channel(&out_tx));
    write.abort();
    info!(%network, "derp-relay: node disconnected");
}

/// `[dst_pk(32)‖payload]` from `src` → `[src_pk(32)‖payload]` to dst, only
/// within `network`. Drops silently on short/oversized frames, unknown dst,
/// or a full destination queue (the carrier is loss-tolerant).
fn forward_frame(registry: &Registry, network: &str, src_pubkey: &DerpPubKey, frame: &[u8]) {
    if frame.len() < 32 || frame.len() > DERP_MAX_FRAME {
        return;
    }
    let mut dst = [0u8; 32];
    dst.copy_from_slice(&frame[..32]);
    let payload = &frame[32..];
    let sender = match registry.get(&(network.to_string(), dst)) {
        Some(r) => r.clone(),
        None => return,
    };
    let mut out = Vec::with_capacity(32 + payload.len());
    out.extend_from_slice(src_pubkey);
    out.extend_from_slice(payload);
    let _ = sender.try_send(out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use roomler_ai_remote_control::derp_ticket::DerpTicketSigner;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    fn pk(byte: u8) -> DerpPubKey {
        [byte; 32]
    }

    fn test_signer() -> (DerpTicketSigner, DecodingKey) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let signer = DerpTicketSigner::from_pkcs8_b64(&BASE64.encode(pkcs8.as_ref())).unwrap();
        let key = derp_ticket::decoding_key_from_public_b64(signer.public_key_b64()).unwrap();
        (signer, key)
    }

    async fn serve(key: DecodingKey) -> (std::net::SocketAddr, Registry) {
        let state = RelayState {
            registry: Arc::new(DashMap::new()),
            ticket_key: Arc::new(key),
        };
        let registry = state.registry.clone();
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, registry)
    }

    async fn connect(
        addr: std::net::SocketAddr,
        ticket: &str,
        reg_pk: DerpPubKey,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let url = format!("ws://{addr}/derp?ticket={ticket}");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws.send(WsMsg::Binary(reg_pk.to_vec().into()))
            .await
            .unwrap();
        ws
    }

    #[tokio::test]
    async fn forwards_within_network_and_rewrites_src() {
        let (signer, key) = test_signer();
        let (addr, _) = serve(key).await;
        let (a, b) = (pk(0xAA), pk(0xBB));
        let (ta, _) = signer.mint("net1", &BASE64.encode(a)).unwrap();
        let (tb, _) = signer.mint("net1", &BASE64.encode(b)).unwrap();
        let mut ws_a = connect(addr, &ta, a).await;
        let mut ws_b = connect(addr, &tb, b).await;
        // Registration is processed on the relay's read loop; give B's
        // registry insert a beat before A sends.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut frame = b.to_vec();
        frame.extend_from_slice(&[1, 2, 3]);
        ws_a.send(WsMsg::Binary(frame.into())).await.unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), ws_b.next())
            .await
            .expect("B receives within 5s")
            .unwrap()
            .unwrap();
        let data = got.into_data();
        assert_eq!(&data[..32], &a, "src prefix rewritten to the sender");
        assert_eq!(&data[32..], &[1, 2, 3]);
    }

    #[tokio::test]
    async fn never_crosses_networks_and_rejects_bad_tickets() {
        let (signer, key) = test_signer();
        let (addr, _) = serve(key).await;
        let (a, b) = (pk(0x01), pk(0x02));
        let (ta, _) = signer.mint("net1", &BASE64.encode(a)).unwrap();
        // Same pubkey B, registered in ANOTHER network.
        let (tb, _) = signer.mint("net2", &BASE64.encode(b)).unwrap();
        let mut ws_a = connect(addr, &ta, a).await;
        let mut ws_b = connect(addr, &tb, b).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut frame = b.to_vec();
        frame.extend_from_slice(&[9]);
        ws_a.send(WsMsg::Binary(frame.into())).await.unwrap();
        // Cross-network: B must receive nothing.
        let r = tokio::time::timeout(std::time::Duration::from_millis(500), ws_b.next()).await;
        assert!(r.is_err(), "cross-network frame must not be delivered");

        // A ticket signed by a FOREIGN key is refused at upgrade.
        let (foreign_signer, _) = test_signer();
        let (bad, _) = foreign_signer.mint("net1", &BASE64.encode(a)).unwrap();
        let url = format!("ws://{addr}/derp?ticket={bad}");
        assert!(
            tokio_tungstenite::connect_async(&url).await.is_err(),
            "foreign-signed ticket must 401"
        );
    }

    #[tokio::test]
    async fn registration_pubkey_must_match_ticket() {
        let (signer, key) = test_signer();
        let (addr, registry) = serve(key).await;
        let (a, victim) = (pk(0x0A), pk(0x0B));
        // Ticket names A, but the socket tries to register VICTIM's key.
        let (ta, _) = signer.mint("net1", &BASE64.encode(a)).unwrap();
        let _ws = connect(addr, &ta, victim).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            registry.is_empty(),
            "mismatched registration must never enter the registry"
        );
    }
}
