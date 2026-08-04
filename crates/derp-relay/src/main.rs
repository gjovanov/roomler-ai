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
        .route("/stats", get(stats))
        .route("/derp", get(derp_upgrade))
        .with_state(state)
}

/// `GET /stats` — the PoP's load snapshot for the API's load-aware region
/// routing: system load/memory/uptime + per-interface traffic counters from
/// `/proc` (host-visible: the container runs host-network and /proc's
/// loadavg/meminfo/net are not namespaced away), live DERP registrations from
/// the in-process registry, and coturn's allocation gauges scraped from its
/// localhost prometheus listener (absent → `null`, never an error — a PoP
/// without the coturn exporter still reports its system half).
async fn stats(State(state): State<RelayState>) -> axum::Json<serde_json::Value> {
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let mut load_it = load.split_whitespace();
    let load1: f64 = load_it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let load5: f64 = load_it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mem_kb = |key: &str| -> u64 {
        meminfo
            .lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };

    // Aggregate rx/tx across physical interfaces (skip lo). Monotonic
    // counters — the poller derives rates from successive samples.
    let netdev = std::fs::read_to_string("/proc/net/dev").unwrap_or_default();
    let (mut rx, mut tx) = (0u64, 0u64);
    for line in netdev.lines().skip(2) {
        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        if iface.trim() == "lo" {
            continue;
        }
        let f: Vec<u64> = rest
            .split_whitespace()
            .map(|v| v.parse().unwrap_or(0))
            .collect();
        if f.len() >= 9 {
            rx = rx.saturating_add(f[0]);
            tx = tx.saturating_add(f[8]);
        }
    }

    let uptime_s: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0);

    axum::Json(serde_json::json!({
        "region": std::env::var("REGION").unwrap_or_default(),
        "cpus": cpus,
        "load1": load1,
        "load5": load5,
        "mem_total_kb": mem_kb("MemTotal:"),
        "mem_available_kb": mem_kb("MemAvailable:"),
        "net_rx_bytes": rx,
        "net_tx_bytes": tx,
        "uptime_s": uptime_s,
        "derp_registrations": state.registry.len(),
        "coturn": coturn_prometheus().await,
    }))
}

/// Scrape coturn's localhost prometheus listener (`prometheus` directive,
/// :9641 — firewalled to loopback by provision.sh) for the gauges the router
/// cares about. Minimal HTTP/1.0 over a raw socket — not worth an HTTP-client
/// dependency for one localhost endpoint. `null` on any failure.
async fn coturn_prometheus() -> serde_json::Value {
    let fetch = async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", 9641))
            .await
            .ok()?;
        s.write_all(b"GET /metrics HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .ok()?;
        let mut body = String::new();
        s.read_to_string(&mut body).await.ok()?;
        let gauge = |name: &str| -> f64 {
            body.lines()
                .filter(|l| l.starts_with(name) && !l.starts_with('#'))
                .filter_map(|l| l.split_whitespace().last()?.parse::<f64>().ok())
                .sum()
        };
        Some(serde_json::json!({
            // Live allocations = the direct "how busy is this TURN" signal.
            "allocations": gauge("turn_total_allocations"),
            "sessions": gauge("turn_total_sessions"),
        }))
    };
    match tokio::time::timeout(std::time::Duration::from_secs(1), fetch).await {
        Ok(Some(v)) => v,
        _ => serde_json::Value::Null,
    }
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
    async fn stats_serves_system_snapshot() {
        let (_, key) = test_signer();
        let (addr, _) = serve(key).await;
        let body = reqwest_lite(addr, "/stats").await;
        let v: serde_json::Value = serde_json::from_str(&body).expect("stats is JSON");
        // /proc-backed fields exist on Linux; on other CI hosts they parse to
        // zero — the shape is the contract either way.
        assert!(v["cpus"].as_u64().unwrap() >= 1);
        assert!(v["load1"].is_number());
        assert!(v["derp_registrations"].is_u64());
        // No coturn exporter in the test env → explicit null, not an error.
        assert!(v["coturn"].is_null());
    }

    /// Tiny HTTP/1.0 GET so the test needs no client dependency.
    async fn reqwest_lite(addr: std::net::SocketAddr, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.0\r\nHost: t\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        resp.split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default()
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
