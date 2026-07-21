//! Loopback-TURN corp-relay (Phase 2b) — the agent side.
//!
//! When a corp host runs this agent AND its browser is the controller, the
//! browser can't punch a direct WebRTC path (chrome.exe UDP is firewalled /
//! the corp edge drops UDP) so sessions fall back to the far coturn relay and
//! get the relay caps (blurry text). The *agent*, however, traverses the corp
//! edge fine over the overlay. So: the local agent hosts a tiny UDP TURN on its
//! overlay IP, the co-located browser relays through it over **loopback** (never
//! firewalled), and the remote agent reaches the allocated relays over the
//! **overlay** (WFP-permitted). Design: `~/.claude/plans/roomler-loopback-turn-
//! corp-relay.md`; the TURN server itself is [`LocalTurnHost`] (Phase 1).
//!
//! This module is the discovery + lifecycle half:
//!   * a loopback HTTP probe on `127.0.0.1:47989` the browser fetches to learn
//!     its local agent's TURN (`turn_port` + `overlay_ip` + freshly-minted
//!     creds) — answering the Chrome Private-Network-Access (PNA) CORS
//!     preflight so an HTTPS page may read a loopback resource;
//!   * a [`LocalTurnHost`] kept in sync with the overlay's assigned self-IP.
//!
//! Default-OFF: gated on `ROOMLER_AGENT_LOCAL_TURN` (loopback-TURN Phase 4
//! posture — inert on the fleet until a field test opts a host in; the browser
//! side is independently opt-in). Compiles in the default build; without an
//! overlay IP the probe simply answers 503 and no TURN is hosted.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use roomler_ai_remote_control::signaling::LocalRelayDescriptor;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{info, warn};
use tunnel_core::localapi::OverlayView;
use tunnel_core::transport::turn_host::LocalTurnHost;

/// Fixed loopback port the browser probes. MUST match `LOCAL_RELAY_PROBE_PORT`
/// in `ui/src/composables/useRemoteControl.ts`.
pub const PROBE_PORT: u16 = 47989;

/// TTL of a minted relay credential (handed to the browser + remote agent).
const CRED_TTL: Duration = Duration::from_secs(3600);
/// Max time to read a request head before giving up on a connection.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Cap on the request head we buffer (a browser probe head is ~a few hundred B).
const MAX_HEAD: usize = 16 * 1024;

/// Browser origins allowed to read the descriptor across the public→loopback
/// (Private-Network-Access) boundary. An origin outside this set gets no CORS
/// headers → the browser blocks the read → the probe is a graceful no-op.
const ALLOWED_ORIGINS: &[&str] = &[
    "https://roomler.ai",
    "http://localhost:5000",
    "http://localhost:5173",
];

/// `true` when the operator opted this host in via `ROOMLER_AGENT_LOCAL_TURN`.
/// Default-OFF.
pub fn enabled() -> bool {
    matches!(
        std::env::var("ROOMLER_AGENT_LOCAL_TURN").ok().as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Spawn the loopback-TURN probe server + host supervisor when [`enabled`].
/// A cheap no-op otherwise (the common case). `agent_id` is used as the minted
/// credential's user-id (opaque; the host both mints and validates statelessly).
pub fn spawn(
    overlay_view: watch::Receiver<OverlayView>,
    agent_id: String,
    shutdown: watch::Receiver<bool>,
) {
    if !enabled() {
        return;
    }
    tokio::spawn(serve(overlay_view, agent_id, shutdown));
}

/// The overlay's currently-assigned self-IP, parsed. `None` until the node has
/// joined a mesh (or in a non-overlay build).
fn overlay_self_ip(view: &watch::Receiver<OverlayView>) -> Option<IpAddr> {
    view.borrow()
        .self_ip
        .as_deref()
        .and_then(|s| s.parse().ok())
}

/// (Re)build `host` to match `want_ip`. Idempotent: unchanged IP ⇒ no-op;
/// a new IP tears down the old host and starts a fresh one bound to `0.0.0.0:0`
/// (so it receives on BOTH loopback and the overlay adapter) with the overlay
/// IP as its relay-candidate address; `None` tears the host down.
async fn reconcile_host(host: &mut Option<LocalTurnHost>, want_ip: Option<IpAddr>, secret: &[u8]) {
    let have = host.as_ref().map(|h| h.relay_ip());
    if want_ip == have {
        return;
    }
    if let Some(old) = host.take() {
        let _ = old.stop().await;
    }
    let Some(ip) = want_ip else {
        info!("loopback-TURN: overlay IP cleared; local TURN host stopped");
        return;
    };
    match LocalTurnHost::start((Ipv4Addr::UNSPECIFIED, 0).into(), ip, secret.to_vec()).await {
        Ok(h) => {
            info!(
                port = h.local_addr().port(),
                relay = %ip,
                "loopback-TURN: local TURN host up on overlay IP"
            );
            *host = Some(h);
        }
        Err(e) => warn!(error = %e, "loopback-TURN: failed to start local TURN host"),
    }
}

/// Main supervisor: serve the loopback probe + keep the TURN host synced to the
/// overlay IP, until shutdown.
async fn serve(
    overlay_view: watch::Receiver<OverlayView>,
    agent_id: String,
    mut shutdown: watch::Receiver<bool>,
) {
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, PROBE_PORT)).await {
        Ok(l) => l,
        Err(e) => {
            warn!(port = PROBE_PORT, error = %e, "loopback-TURN: cannot bind probe port; disabled");
            return;
        }
    };
    info!(
        port = PROBE_PORT,
        "loopback-TURN: probe on 127.0.0.1 (ROOMLER_AGENT_LOCAL_TURN)"
    );

    // Per-run TURN secret (32 random bytes). The host both mints AND validates
    // with it, so it never leaves this process except as an opaque HMAC in the
    // credential handed to the two relay endpoints.
    let secret: Vec<u8> = rand::random::<[u8; 32]>().to_vec();

    let mut overlay_view = overlay_view;
    let mut host: Option<LocalTurnHost> = None;
    reconcile_host(&mut host, overlay_self_ip(&overlay_view), &secret).await;

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            changed = overlay_view.changed() => {
                if changed.is_err() {
                    break; // sender dropped — agent is going away
                }
                reconcile_host(&mut host, overlay_self_ip(&overlay_view), &secret).await;
            }
            accept = listener.accept() => {
                let Ok((stream, _peer)) = accept else { continue };
                // Mint fresh creds off the live host and hand the connection an
                // owned descriptor (or None if no overlay yet → 503). No host
                // sharing across tasks.
                let descriptor = host.as_ref().map(|h| {
                    let (username, credential) = h.mint_credentials_now(&agent_id, CRED_TTL);
                    LocalRelayDescriptor {
                        turn_port: h.local_addr().port(),
                        overlay_ip: h.relay_ip().to_string(),
                        username,
                        credential,
                    }
                });
                tokio::spawn(handle_conn(stream, descriptor));
            }
        }
    }

    if let Some(h) = host.take() {
        let _ = h.stop().await;
    }
    info!("loopback-TURN: probe stopped");
}

/// Handle one probe connection: `GET /rc-local-turn` → the descriptor JSON (or
/// 503 with no overlay), `OPTIONS` → the PNA CORS preflight, else 404. Generic
/// over the stream so it's unit-testable over an in-memory duplex.
async fn handle_conn<S>(mut stream: S, descriptor: Option<LocalRelayDescriptor>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(head) = read_head(&mut stream).await else {
        return;
    };
    let (verb, path, origin) = parse_request_head(&head);
    let allow = allowed_origin(origin.as_deref());

    let response = match verb.as_str() {
        "OPTIONS" => preflight_response(allow),
        "GET" if path.starts_with("/rc-local-turn") => match &descriptor {
            Some(d) => match serde_json::to_string(d) {
                Ok(body) => json_response(allow, &body),
                Err(_) => status_response(500, "Internal Server Error", allow),
            },
            None => status_response(503, "Service Unavailable", allow),
        },
        _ => status_response(404, "Not Found", allow),
    };

    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Read the request head (up to the blank line) with a timeout + size cap.
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        let n = match tokio::time::timeout(READ_TIMEOUT, stream.read(&mut tmp)).await {
            Ok(Ok(0)) => break,        // EOF
            Ok(Ok(n)) => n,            // got bytes
            Ok(Err(_)) => return None, // read error
            Err(_) => return None,     // timeout
        };
        buf.extend_from_slice(&tmp[..n]);
        if buf.ends_with(b"\r\n\r\n") || buf.len() > MAX_HEAD {
            break;
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Extract `(verb, path, origin)` from a raw request head. Tolerant of garbage
/// (empty verb/path ⇒ the caller answers 404).
fn parse_request_head(head: &str) -> (String, String, Option<String>) {
    let mut lines = head.lines();
    let mut request = lines.next().unwrap_or("").split_whitespace();
    let verb = request.next().unwrap_or("").to_string();
    let path = request.next().unwrap_or("").to_string();
    let origin = head.lines().find_map(|l| {
        let (name, val) = l.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("origin")
            .then(|| val.trim().to_string())
    });
    (verb, path, origin)
}

/// The origin to reflect in `Access-Control-Allow-Origin`, or `None` to emit no
/// CORS headers (an origin outside the allowlist).
fn allowed_origin(origin: Option<&str>) -> Option<&str> {
    origin.filter(|o| ALLOWED_ORIGINS.contains(o))
}

/// CORS + Private-Network-Access headers, emitted only for an allowed origin.
fn cors_headers(allow: Option<&str>) -> String {
    match allow {
        Some(origin) => format!(
            "Access-Control-Allow-Origin: {origin}\r\n\
             Access-Control-Allow-Private-Network: true\r\n\
             Vary: Origin\r\n"
        ),
        None => String::new(),
    }
}

/// Response to the Chrome PNA preflight (`OPTIONS`).
fn preflight_response(allow: Option<&str>) -> String {
    format!(
        "HTTP/1.1 204 No Content\r\n\
         {cors}\
         Access-Control-Allow-Methods: GET, OPTIONS\r\n\
         Access-Control-Allow-Headers: *\r\n\
         Access-Control-Max-Age: 600\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n",
        cors = cors_headers(allow),
    )
}

/// 200 with the descriptor JSON body.
fn json_response(allow: Option<&str>, body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
         {cors}\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        cors = cors_headers(allow),
        len = body.len(),
    )
}

/// Empty-body status response (404 / 503 / 500).
fn status_response(code: u16, reason: &str, allow: Option<&str>) -> String {
    format!(
        "HTTP/1.1 {code} {reason}\r\n\
         {cors}\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n",
        cors = cors_headers(allow),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc() -> LocalRelayDescriptor {
        LocalRelayDescriptor {
            turn_port: 47989,
            overlay_ip: "100.64.0.7".into(),
            username: "1700000600:507f1f77bcf86cd799439012".into(),
            credential: "abcd1234".into(),
        }
    }

    #[test]
    fn allowlist_only_passes_known_origins() {
        assert_eq!(
            allowed_origin(Some("https://roomler.ai")),
            Some("https://roomler.ai")
        );
        assert_eq!(
            allowed_origin(Some("http://localhost:5000")),
            Some("http://localhost:5000")
        );
        assert_eq!(allowed_origin(Some("https://evil.example")), None);
        assert_eq!(allowed_origin(None), None);
    }

    #[test]
    fn parses_verb_path_and_origin() {
        let head = "GET /rc-local-turn HTTP/1.1\r\nHost: 127.0.0.1:47989\r\nOrigin: https://roomler.ai\r\n\r\n";
        let (verb, path, origin) = parse_request_head(head);
        assert_eq!(verb, "GET");
        assert_eq!(path, "/rc-local-turn");
        assert_eq!(origin.as_deref(), Some("https://roomler.ai"));
    }

    #[test]
    fn preflight_carries_pna_header_for_allowed_origin() {
        let r = preflight_response(Some("https://roomler.ai"));
        assert!(r.starts_with("HTTP/1.1 204"));
        assert!(r.contains("Access-Control-Allow-Origin: https://roomler.ai\r\n"));
        assert!(r.contains("Access-Control-Allow-Private-Network: true\r\n"));
        assert!(r.contains("Access-Control-Allow-Methods: GET, OPTIONS\r\n"));
    }

    #[test]
    fn preflight_omits_cors_for_unknown_origin() {
        let r = preflight_response(None);
        assert!(r.starts_with("HTTP/1.1 204"));
        assert!(!r.contains("Access-Control-Allow-Origin"));
        assert!(!r.contains("Access-Control-Allow-Private-Network"));
    }

    #[test]
    fn json_response_has_body_and_content_length() {
        let body = serde_json::to_string(&desc()).unwrap();
        let r = json_response(Some("https://roomler.ai"), &body);
        assert!(r.starts_with("HTTP/1.1 200 OK"));
        assert!(r.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(r.contains("Content-Type: application/json"));
        assert!(r.ends_with(&body));
        assert!(r.contains("\"overlay_ip\":\"100.64.0.7\""));
    }

    #[tokio::test]
    async fn handle_conn_get_returns_descriptor_json() {
        let (mut client, server) = tokio::io::duplex(8192);
        let task = tokio::spawn(handle_conn(server, Some(desc())));
        client
            .write_all(b"GET /rc-local-turn HTTP/1.1\r\nOrigin: https://roomler.ai\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.unwrap();
        task.await.unwrap();
        let resp = String::from_utf8_lossy(&out);
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "resp: {resp}");
        assert!(resp.contains("Access-Control-Allow-Private-Network: true"));
        assert!(resp.contains("\"turn_port\":47989"));
        assert!(resp.contains("\"overlay_ip\":\"100.64.0.7\""));
    }

    #[tokio::test]
    async fn handle_conn_get_without_host_is_503() {
        let (mut client, server) = tokio::io::duplex(8192);
        let task = tokio::spawn(handle_conn(server, None));
        client
            .write_all(b"GET /rc-local-turn HTTP/1.1\r\nOrigin: https://roomler.ai\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.unwrap();
        task.await.unwrap();
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 503"));
    }

    #[tokio::test]
    async fn handle_conn_options_is_pna_preflight() {
        let (mut client, server) = tokio::io::duplex(8192);
        let task = tokio::spawn(handle_conn(server, None));
        client
            .write_all(
                b"OPTIONS /rc-local-turn HTTP/1.1\r\nOrigin: https://roomler.ai\r\n\
                  Access-Control-Request-Private-Network: true\r\n\r\n",
            )
            .await
            .unwrap();
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.unwrap();
        task.await.unwrap();
        let resp = String::from_utf8_lossy(&out);
        assert!(resp.starts_with("HTTP/1.1 204"));
        assert!(resp.contains("Access-Control-Allow-Private-Network: true"));
    }

    #[tokio::test]
    async fn reconcile_starts_and_stops_host_on_overlay_ip() {
        let secret = vec![7u8; 32];
        let mut host: Option<LocalTurnHost> = None;

        // No overlay IP → no host.
        reconcile_host(&mut host, None, &secret).await;
        assert!(host.is_none());

        // Overlay IP assigned → host up on that IP with an ephemeral port.
        let ip: IpAddr = "100.64.0.7".parse().unwrap();
        reconcile_host(&mut host, Some(ip), &secret).await;
        {
            let h = host.as_ref().expect("host started");
            assert_eq!(h.relay_ip(), ip);
            assert_ne!(h.local_addr().port(), 0);
        }

        // Same IP → idempotent (still up, same host).
        let port_before = host.as_ref().unwrap().local_addr().port();
        reconcile_host(&mut host, Some(ip), &secret).await;
        assert_eq!(host.as_ref().unwrap().local_addr().port(), port_before);

        // Overlay IP cleared → host torn down.
        reconcile_host(&mut host, None, &secret).await;
        assert!(host.is_none());
    }
}
