// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! `roomler forward` — open one TCP forward through an enrolled
//! agent. The TeamViewer-shaped flow:
//!
//! 1. WS-connect to `wss://<server>/ws?role=tunnel-client&tid=<tenant>`, the
//!    JWT in `Authorization: Bearer` (#3656; [`dial`] has the one-time
//!    query-form fallback for a server older than the header).
//! 2. `rc:tunnel.hello { role: TunnelClient, version, supported_transports }`.
//! 3. `rc:tunnel.open { agent_id, transport: "webrtc-dc-v1" }`.
//!    Wait for `rc:tunnel.opened` → carries `session_id` + `ice_servers`.
//! 4. Build a `TunnelPeer` with those ICE servers. Generate SDP
//!    offer; ship over `rc:sdp.offer { session_id, sdp }`.
//!    Trickle ICE via `rc:ice { session_id, candidate }`.
//! 5. Receive `rc:sdp.answer { session_id, sdp }`; remote-describe.
//!    Wait for the DC pool to fully open.
//! 6. Install `FlowDemux` on each DC.
//! 7. Bind a local TCP listener on `--local`. Per accepted conn:
//!    request a forward, and on Accept pump it over the negotiated
//!    transport.
//!
//! As of P3b-1b the session orchestration lives in
//! [`tunnel_core::driver::run_tunnel_session`], behind the
//! [`tunnel_core::signaling_link`] seam. This module is now the thin CLI:
//! it owns the WebSocket transport (connect, the single outbound-writer
//! task + keepalive, the WS-frame → `ServerMsg` parse), wraps it in
//! [`WsSink`] + [`WsSource`], and drives the reusable driver — plus the
//! auto-reconnect loop + the `--transport auto` WebRTC fallback.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt};
use roomler_ai_remote_control::signaling::{ClientMsg, ServerMsg};
use roomler_ai_remote_control::upgrade_auth::{self, Carrier};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::error::Elapsed;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::{Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, header};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite, tungstenite::Message,
};
use tracing::{debug, info, warn};
use tunnel_core::driver::{SessionOutcome, SessionParams, Target, run_tunnel_session};
use tunnel_core::signaling_link::{TunnelSignalingSink, TunnelSignalingSource};
use tunnel_core::transport::TRANSPORT_WEBRTC_DC_V1;

use crate::config::{TunnelConfig, derive_ws_url};

/// Re-export so `forward::TransportPref` still resolves for callers
/// (`main.rs`'s clap shim converts into it; `mesh.rs` takes it directly).
/// The type itself now lives in the driver so it's clap-free.
pub use tunnel_core::driver::TransportPref;

/// Buffer depth for the outbound WS channel. Generous — most flow
/// activity is ICE candidate trickle + per-flow Accept replies, both
/// modest. Sized to absorb a burst at session-open without blocking.
const WS_OUT_CHANNEL_DEPTH: usize = 256;

/// How often to send a WebSocket keepalive Ping on the control channel so an
/// idle middlebox (WS proxy / corp full-tunnel VPN) doesn't reap it after
/// ~5 min. Well under any real idle-reap window.
const WS_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(30);

/// Receive-liveness deadline (3 keepalive periods) for the established WS.
/// The server auto-pongs our keepalive Pings, so a healthy link delivers an
/// inbound frame at least every `WS_KEEPALIVE`. Send-success alone proves
/// nothing: a TLS-inspecting corp middlebox (Check Point et al.) terminates
/// TCP locally and keeps ACKing our Pings after its upstream leg has died —
/// field case WINHOST-A 2026-08-02 (agent twin of this loop sat half-open for
/// 45+ min after a server pod roll). `WsSource::recv` treats the deadline as
/// link-gone (`None`), which drives the normal reconnect path.
const WS_RX_DEADLINE: std::time::Duration = std::time::Duration::from_secs(90);

/// Auto-reconnect backoff bounds. Start short (a session that ran then dropped
/// usually reconnects instantly) and cap so a persistently-offline agent isn't
/// hammered.
const RECONNECT_BACKOFF_MIN: std::time::Duration = std::time::Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// Read half of the signaling WebSocket. Aliased so [`WsSource`] can name the
/// concrete `SplitStream` it wraps.
type WsReadHalf = futures::stream::SplitStream<Ws>;

/// The CLI's [`TunnelSignalingSink`]: every outbound `ClientMsg` the driver
/// emits is pushed onto the single outbound mpsc a lone writer task drains
/// onto the WS sink (which also emits the keepalive Ping). Funnelling through
/// ONE channel + ONE writer preserves the pre-seam FIFO send ordering — never
/// a shared locked SplitSink.
struct WsSink {
    tx: mpsc::Sender<ClientMsg>,
}

#[async_trait]
impl TunnelSignalingSink for WsSink {
    async fn send(&self, msg: ClientMsg) -> anyhow::Result<()> {
        self.tx
            .send(msg)
            .await
            .map_err(|e| anyhow::anyhow!("outbound WS channel closed: {e}"))
    }
}

/// The CLI's [`TunnelSignalingSource`]: the ONLY place the WS-frame layer
/// lives now. Absorbs Ping/Close/parse so the driver sees a typed `ServerMsg`
/// (lifted from the old `recv_server_msg` + `dispatch_loop`'s frame handling).
/// `None` = the link is gone (Close or a read error).
struct WsSource {
    inner: WsReadHalf,
}

#[async_trait]
impl TunnelSignalingSource for WsSource {
    async fn recv(&mut self) -> Option<ServerMsg> {
        // The timeout IS the receive-liveness check (see `WS_RX_DEADLINE`):
        // a healthy link delivers at least the auto-pongs to our keepalive
        // Pings, so `next()` never legitimately blocks this long. On expiry
        // report link-gone and let the driver reconnect.
        while let Ok(Some(item)) = tokio::time::timeout(WS_RX_DEADLINE, self.inner.next())
            .await
            .map_err(|_| {
                warn!(
                    deadline_s = WS_RX_DEADLINE.as_secs(),
                    "no inbound WS frames within the RX deadline — half-open socket; ending source"
                );
            })
        {
            match item {
                Ok(Message::Text(t)) => match serde_json::from_str::<ServerMsg>(t.as_str()) {
                    Ok(m) => return Some(m),
                    Err(e) => {
                        debug!(%e, text = %t.as_str(), "ignoring non-rc:* / unparseable WS frame");
                        continue;
                    }
                },
                Ok(Message::Close(c)) => {
                    info!(?c, "server closed WS");
                    return None;
                }
                Ok(Message::Ping(d)) => {
                    // tokio-tungstenite auto-pongs; log for diagnostics.
                    debug!(len = d.len(), "ws ping");
                    continue;
                }
                Ok(_) => continue,
                Err(e) => {
                    warn!(%e, "ws read error; ending source");
                    return None;
                }
            }
        }
        None
    }
}

/// `roomler forward` — one static local→remote TCP forward.
pub async fn run(
    cfg: TunnelConfig,
    agent_hex: &str,
    local: u16,
    remote: &str,
    transport: TransportPref,
) -> Result<()> {
    let (host, port) = parse_remote(remote)?;
    run_forward(
        cfg,
        agent_hex,
        local,
        Target::Static { host, port },
        transport,
    )
    .await
}

/// `roomler socks5` — the userspace-mode SOCKS5 proxy. Same transport +
/// server policy + agent allowlist as a static forward; the destination is taken
/// from each connection's SOCKS5 CONNECT instead of a fixed `--remote`.
pub async fn run_socks5(
    cfg: TunnelConfig,
    agent_hex: &str,
    local: u16,
    transport: TransportPref,
) -> Result<()> {
    run_forward(cfg, agent_hex, local, Target::Socks5, transport).await
}

/// Shared driver for `forward` (static target) and `socks5` (per-connection
/// target). Runs sessions in an **auto-reconnect loop**: each session serves
/// local TCP connections until its WS control channel drops (idle-reap by a WS
/// proxy / corp VPN, a network blip, a VPN reconnect), then re-establishes with
/// backoff. Ctrl-C kills the process, which ends the loop.
async fn run_forward(
    cfg: TunnelConfig,
    agent_hex: &str,
    local: u16,
    target: Target,
    transport: TransportPref,
) -> Result<()> {
    let agent_id = ObjectId::parse_str(agent_hex)
        .with_context(|| format!("--agent must be a 24-hex ObjectId, got {agent_hex}"))?;

    info!(
        server = %cfg.server_url,
        agent = %agent_id,
        local,
        ?target,
        ?transport,
        "roomler forward starting"
    );

    let mut backoff = RECONNECT_BACKOFF_MIN;
    loop {
        match run_one_session(&cfg, agent_id, local, &target, transport).await {
            // A session that established then dropped resets the backoff so the
            // reconnect is near-instant; a repeated setup failure grows it.
            Ok(()) => {
                info!("tunnel session ended; reconnecting");
                backoff = RECONNECT_BACKOFF_MIN;
            }
            Err(e) => {
                warn!(%e, backoff_s = backoff.as_secs(), "tunnel session failed; retrying");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
}

/// One session attempt: request the preferred transport (Auto → QUIC →
/// WebRTC-DC fallback) and serve local TCP connections until the control
/// channel drops. Returns `Ok(())` when a session ran and ended (→ reconnect),
/// `Err` on a setup failure (→ backoff + retry).
async fn run_one_session(
    cfg: &TunnelConfig,
    agent_id: ObjectId,
    local: u16,
    target: &Target,
    transport: TransportPref,
) -> Result<()> {
    let outcome = run_session(
        cfg,
        agent_id,
        local,
        target,
        transport.supported_transports(),
        transport.request_transport(),
    )
    .await?;
    if matches!(outcome, SessionOutcome::QuicSetupFailed) {
        if transport == TransportPref::Auto {
            warn!("QUIC transport setup failed; re-opening session forcing webrtc-dc-v1");
            let fallback = run_session(
                cfg,
                agent_id,
                local,
                target,
                vec![TRANSPORT_WEBRTC_DC_V1.to_string()],
                TRANSPORT_WEBRTC_DC_V1,
            )
            .await?;
            if matches!(fallback, SessionOutcome::QuicSetupFailed) {
                bail!("webrtc-dc-v1 fallback unexpectedly reported QUIC-setup-failed");
            }
        } else {
            bail!("QUIC transport setup failed and --transport={transport:?} forbids fallback");
        }
    }
    Ok(())
}

/// Connect a fresh WS, build the [`TunnelSignalingSink`] + [`TunnelSignalingSource`]
/// seam over it, and drive [`run_tunnel_session`]. This is the CLI's transport
/// glue: the driver owns the protocol; this owns the WebSocket.
async fn run_session(
    cfg: &TunnelConfig,
    agent_id: ObjectId,
    local: u16,
    target: &Target,
    supported_transports: Vec<String>,
    request_transport: &str,
) -> Result<SessionOutcome> {
    // ────────────── WS connect ─────────────────────────────────────
    let ws_base = derive_ws_url(&cfg.server_url)?;
    // S6 — `tid` = tenant-affinity hint for the front LB (co-locates
    // this tunnel client with its tenant's agents on one pod; the
    // tunnel-hub is pod-local). Extracted from our own JWT; the server
    // re-validates against the verified claims, and pre-S6 servers
    // ignore the param.
    // #3656 — the token is not in the URL: `dial` presents it in a header.
    let tid_param = tenant_id_from_jwt(&cfg.tunnel_client_token)
        .map(|t| format!("&tid={t}"))
        .unwrap_or_default();
    let ws_url = format!("{ws_base}?role=tunnel-client{tid_param}");
    info!(%ws_base, "connecting websocket");
    let (ws_stream, _resp) = match dial(&ws_url, &cfg.tunnel_client_token).await {
        Ok(answer) => answer.with_context(|| format!("WS connect to {ws_base}"))?,
        Err(_) => bail!(
            "WS connect to {ws_base} timed out after {}s",
            WS_DIAL_TIMEOUT.as_secs()
        ),
    };
    info!("websocket connected");
    let (mut ws_sink, ws_source) = ws_stream.split();

    // Outbound channel — any task pushes ClientMsg here; a single
    // task drains it onto the WS sink.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<ClientMsg>(WS_OUT_CHANNEL_DEPTH);

    let _sender_task = tokio::spawn(async move {
        // Keepalive so an idle middlebox (our nginx/HAProxy WS proxy, or a corp
        // full-tunnel VPN like Check Point) doesn't reap the control channel
        // after ~5 min of silence. Post-setup the data plane rides QUIC/DC and
        // the WS goes quiet, so without this the next SOCKS/forward connection —
        // which needs the WS to carry TcpForwardRequest — fails against a dead
        // socket. A protocol-level Ping needs no server change (axum auto-pongs).
        let mut keepalive = tokio::time::interval(WS_KEEPALIVE);
        keepalive.tick().await; // consume the immediate first tick
        loop {
            tokio::select! {
                maybe = outbound_rx.recv() => {
                    let Some(msg) = maybe else { break };
                    let json = match serde_json::to_string(&msg) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!(%e, "outbound serialise failed");
                            continue;
                        }
                    };
                    if let Err(e) = ws_sink.send(Message::text(json)).await {
                        warn!(%e, "outbound WS send failed; dropping");
                        break;
                    }
                }
                _ = keepalive.tick() => {
                    if let Err(e) = ws_sink.send(Message::Ping(Vec::new().into())).await {
                        warn!(%e, "WS keepalive ping failed; sender exiting");
                        break;
                    }
                }
            }
        }
        debug!("outbound WS task exiting");
    });

    let sink: Arc<dyn TunnelSignalingSink> = Arc::new(WsSink { tx: outbound_tx });
    let source: Box<dyn TunnelSignalingSource> = Box::new(WsSource { inner: ws_source });

    run_tunnel_session(
        sink,
        source,
        local,
        SessionParams {
            agent_id,
            target: target.clone(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            // R4: the standalone CLI is not an overlay node — no DERP
            // identity, no derp tunnel flavor; the classic ladder stands.
            derp: None,
            // #1685: one forward per process and no liveness state to
            // publish — the daemon's route reconciler is the consumer.
            on_listening: None,
        },
        supported_transports,
        request_transport,
        // The standalone CLI has one forward per process and doesn't surface a
        // `flows` table — a throwaway aggregate (P3b-3); the daemon owns the
        // real one.
        Arc::new(tunnel_core::forward::SessionThroughput::default()),
    )
    .await
}

/// Parse a `host:port` string. Robust to bracketed IPv6 (`[::1]:80`).
pub(crate) fn parse_remote(s: &str) -> Result<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        // IPv6: `[addr]:port`
        let close = rest
            .find(']')
            .with_context(|| format!("--remote with `[` must close with `]:port`: {s}"))?;
        let host = &rest[..close];
        let port_str = rest[close + 1..]
            .strip_prefix(':')
            .with_context(|| format!("missing `:port` after `]`: {s}"))?;
        let port = port_str
            .parse()
            .with_context(|| format!("invalid port {port_str}"))?;
        return Ok((host.to_string(), port));
    }
    let (host, port_str) = s
        .rsplit_once(':')
        .with_context(|| format!("--remote must be host:port, got {s}"))?;
    if host.is_empty() {
        bail!("--remote host must not be empty");
    }
    let port = port_str
        .parse()
        .with_context(|| format!("invalid port {port_str}"))?;
    Ok((host.to_string(), port))
}

/// One upgrade attempt's limit, the daemon's (`WS_CONNECT_TIMEOUT`): a TLS
/// handshake that hangs behind a middlebox must not hold the session forever.
const WS_DIAL_TIMEOUT: Duration = Duration::from_secs(30);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

type Answer = Result<Result<(Ws, Response), tungstenite::Error>, Elapsed>;

/// #3656 — the upgrade request for the tunnel-client socket at `url` with
/// `token` in `carrier`: an `Authorization: Bearer` header whose value is
/// marked sensitive, or the query form a server older than the header reads.
/// Never both.
fn ws_request(url: &str, token: &str, carrier: Carrier) -> Result<Request, tungstenite::Error> {
    match carrier {
        Carrier::Header => {
            let mut request = url.into_client_request()?;
            let mut bearer = HeaderValue::from_str(&format!("Bearer {token}"))?;
            bearer.set_sensitive(true);
            request.headers_mut().insert(header::AUTHORIZATION, bearer);
            Ok(request)
        }
        Carrier::Query => {
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}token={}", urlencoding_lite(token)).into_client_request()
        }
    }
}

/// #3656 — dial with the token in a header, each attempt bounded by
/// [`WS_DIAL_TIMEOUT`], and hand back the last answer. The daemon's rule
/// (`roomlerd::ws_connect::dial`): ONE redial in the query form after a
/// refusal with no verdict on the credential (a server older than the header,
/// or a header dropped on the way), and an endpoint where that redial
/// upgraded keeps the query form for [`upgrade_auth::QUERY_FORM_FOR`]. A
/// refusal that names an error never puts the token into a URL.
async fn dial(url: &str, token: &str) -> Answer {
    let notes = upgrade_auth::query_form_notes();
    let endpoint = upgrade_auth::endpoint_of(url);
    let first = notes.carrier_for(endpoint);
    let answer = dial_with(url, token, first).await;
    if first == Carrier::Query || !asks_for_query_form(&answer) {
        return answer;
    }
    debug!(
        %endpoint,
        "upgrade refused with no verdict on the credential (a server older than the header, \
         or a header dropped on the way); dialling once more with it in the query"
    );
    let again = dial_with(url, token, Carrier::Query).await;
    if matches!(again, Ok(Ok(_))) {
        notes.note_query_form(endpoint);
        info!(
            %endpoint,
            "this endpoint took the token only in the query: its dials carry it there \
             for the next 6 h (#3656)"
        );
    }
    again
}

async fn dial_with(url: &str, token: &str, carrier: Carrier) -> Answer {
    match ws_request(url, token, carrier) {
        Ok(request) => tokio::time::timeout(WS_DIAL_TIMEOUT, connect_async(request)).await,
        Err(e) => Ok(Err(e)),
    }
}

/// Whether a dial's answer is a refusal [`upgrade_auth::asks_for_query_form`]
/// reads as a request for the query form.
fn asks_for_query_form(answer: &Answer) -> bool {
    let Ok(Err(tungstenite::Error::Http(refused))) = answer else {
        return false;
    };
    let challenges = refused.headers().get_all(header::WWW_AUTHENTICATE);
    upgrade_auth::asks_for_query_form(
        refused.status().as_u16(),
        challenges.iter().filter_map(|v| v.to_str().ok()),
    )
}

/// S6 — best-effort extraction of the `tenant_id` claim from OUR OWN
/// JWT, used only as the LB affinity hint (`tid=`). No signature check:
/// the server re-validates the hint against the VERIFIED claims and
/// rejects a mismatch, so a garbled value can only mis-route, never
/// escalate. Returns `None` on any parse hiccup (param simply omitted;
/// the LB then pins the connection to the legacy pod).
fn tenant_id_from_jwt(token: &str) -> Option<String> {
    use base64::Engine as _;
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    // Only forward a plausible ObjectId hex — anything else would just
    // 403 at the server.
    let t = v.get("tenant_id")?.as_str()?;
    if t.len() == 24 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(t.to_string())
    } else {
        None
    }
}

/// Tiny URL-encoder for the JWT in the query string. We only need
/// to escape characters that appear in JWTs (`.`, `-`, `_` are safe
/// per JWT spec; we just guard against future drift). Avoids pulling
/// the `url` crate just for this.
fn urlencoding_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_simple() {
        let (h, p) = parse_remote("db.intranet:5432").unwrap();
        assert_eq!(h, "db.intranet");
        assert_eq!(p, 5432);
    }

    #[test]
    fn parse_remote_ipv4() {
        let (h, p) = parse_remote("10.0.0.5:1521").unwrap();
        assert_eq!(h, "10.0.0.5");
        assert_eq!(p, 1521);
    }

    #[test]
    fn parse_remote_ipv6_bracketed() {
        let (h, p) = parse_remote("[::1]:5432").unwrap();
        assert_eq!(h, "::1");
        assert_eq!(p, 5432);
    }

    #[test]
    fn parse_remote_ipv6_with_zone_id() {
        let (h, p) = parse_remote("[fe80::1%eth0]:22").unwrap();
        assert_eq!(h, "fe80::1%eth0");
        assert_eq!(p, 22);
    }

    #[test]
    fn parse_remote_rejects_missing_port() {
        let err = parse_remote("db.intranet").unwrap_err();
        assert!(err.to_string().contains("host:port"));
    }

    #[test]
    fn parse_remote_rejects_empty_host() {
        let err = parse_remote(":5432").unwrap_err();
        assert!(err.to_string().contains("host must not be empty"));
    }

    #[test]
    fn parse_remote_rejects_invalid_port() {
        assert!(parse_remote("db.intranet:notaport").is_err());
        assert!(parse_remote("db.intranet:99999").is_err()); // u16 overflow
    }

    #[test]
    fn urlencoding_lite_preserves_jwt_chars() {
        // JWT chars are A-Z a-z 0-9 . - _ — none should be encoded.
        let jwt = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.payload.sig-with_under";
        assert_eq!(urlencoding_lite(jwt), jwt);
    }

    #[test]
    fn urlencoding_lite_encodes_space() {
        assert_eq!(urlencoding_lite("a b"), "a%20b");
    }

    fn fake_jwt(payload_json: &str) -> String {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            b64.encode(r#"{"alg":"HS256"}"#),
            b64.encode(payload_json)
        )
    }

    #[test]
    fn tenant_id_from_jwt_extracts_hex_oid() {
        let jwt = fake_jwt(r#"{"sub":"x","tenant_id":"64b1f0aa12cd34ef56ab78cd"}"#);
        assert_eq!(
            tenant_id_from_jwt(&jwt).as_deref(),
            Some("64b1f0aa12cd34ef56ab78cd")
        );
    }

    /// What one dial presented to [`front`]: the request target and the
    /// `Authorization` header.
    type Seen = (String, Option<String>);

    /// #3656 — a loopback server that records what each upgrade presented and
    /// answers it with `answer`: `None` upgrades, `Some((status, challenge))`
    /// refuses.
    async fn front(
        answer: impl Fn(&Seen) -> Option<(u16, Option<&'static str>)> + Send + Sync + 'static,
    ) -> (String, Arc<std::sync::Mutex<Vec<Seen>>>) {
        use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
        use tokio_tungstenite::tungstenite::http;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (seen, answer) = (log.clone(), Arc::new(answer));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (seen, answer) = (seen.clone(), answer.clone());
                tokio::spawn(async move {
                    // tungstenite's handshake `Callback` fixes the closure's error type.
                    #[allow(clippy::result_large_err)]
                    let cb = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                        let this: Seen = (
                            req.uri().to_string(),
                            req.headers()
                                .get(http::header::AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string),
                        );
                        let verdict = answer(&this);
                        seen.lock().unwrap().push(this);
                        let Some((status, challenge)) = verdict else {
                            return Ok(resp);
                        };
                        let mut refused = ErrorResponse::new(None);
                        *refused.status_mut() = http::StatusCode::from_u16(status).unwrap();
                        if let Some(c) = challenge {
                            refused.headers_mut().insert(
                                http::header::WWW_AUTHENTICATE,
                                http::HeaderValue::from_static(c),
                            );
                        }
                        Err(refused)
                    };
                    if let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await {
                        while let Some(Ok(_)) = ws.next().await {}
                    }
                });
            }
        });
        (server, log)
    }

    fn tunnel_cfg(server_url: &str, token: &str) -> TunnelConfig {
        TunnelConfig {
            server_url: server_url.to_string(),
            tunnel_client_token: token.to_string(),
            machine_name: "box".to_string(),
            allow_plaintext_server: true,
        }
    }

    /// #3656 — the tunnel-client socket's dial, as a server sees it: the token
    /// in `Authorization: Bearer`, and a URL that names the role and the tenant
    /// only. The front refuses with a verdict, so nothing dials again.
    #[tokio::test]
    async fn the_tunnel_client_socket_presents_its_token_in_a_header() {
        let (server, log) = front(|_| {
            Some((
                401,
                Some(roomler_ai_remote_control::upgrade_auth::CHALLENGE_REFUSED),
            ))
        })
        .await;
        let token = fake_jwt(r#"{"sub":"x","tenant_id":"64b1f0aa12cd34ef56ab78cd"}"#);
        let cfg = tunnel_cfg(&server, &token);
        let target = Target::Static {
            host: "127.0.0.1".into(),
            port: 9,
        };
        let refused = run_session(
            &cfg,
            ObjectId::new(),
            0,
            &target,
            vec![TRANSPORT_WEBRTC_DC_V1.to_string()],
            TRANSPORT_WEBRTC_DC_V1,
        )
        .await;
        assert!(refused.is_err(), "a refused upgrade ends the session");
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "a verdict on the token is final: {seen:?}");
        let (uri, authorization) = &seen[0];
        assert!(uri.starts_with("/ws?"), "{uri}");
        assert!(!uri.contains("token="), "{uri}");
        assert!(!uri.contains(&token), "{uri}");
        assert!(uri.contains("role=tunnel-client"), "{uri}");
        assert!(uri.contains("tid=64b1f0aa12cd34ef56ab78cd"), "{uri}");
        assert_eq!(
            authorization.as_deref(),
            Some(format!("Bearer {token}").as_str())
        );
    }

    /// #3656 — each carrier puts the token in one place only: a sensitive
    /// header with the URL unchanged, or the URL alone.
    #[test]
    fn each_carrier_puts_the_token_in_one_place_only() {
        let url = "wss://roomler.ai/ws?role=tunnel-client&tid=t1";
        let header_form = ws_request(url, "a.b c", Carrier::Header).unwrap();
        assert_eq!(header_form.uri().to_string(), url);
        let bearer = header_form.headers().get(header::AUTHORIZATION).unwrap();
        assert_eq!(bearer, "Bearer a.b c");
        assert!(bearer.is_sensitive());
        assert!(
            !format!("{header_form:?}").contains("a.b c"),
            "{header_form:?}"
        );
        let query_form = ws_request(url, "a.b c", Carrier::Query).unwrap();
        assert_eq!(query_form.uri().to_string(), format!("{url}&token=a.b%20c"));
        assert!(query_form.headers().get(header::AUTHORIZATION).is_none());
    }

    /// #3656 — a server older than the header answers a bare 401: ONE redial
    /// in the query form with no header, and the endpoint's next dial goes
    /// straight to the query form. A redial refused too would note nothing.
    #[tokio::test]
    async fn a_server_older_than_the_header_hears_the_token_in_the_query() {
        let (server, log) =
            front(|(uri, _)| (!uri.contains("token=")).then_some((401, None))).await;
        let url = format!(
            "{}/ws?role=tunnel-client",
            server.replace("http://", "ws://")
        );
        dial(&url, "tok")
            .await
            .unwrap()
            .expect("the query form upgrades");
        dial(&url, "tok")
            .await
            .unwrap()
            .expect("the noted endpoint upgrades");
        let seen = log.lock().unwrap().clone();
        assert_eq!(
            seen,
            [
                (
                    "/ws?role=tunnel-client".to_string(),
                    Some("Bearer tok".to_string())
                ),
                ("/ws?role=tunnel-client&token=tok".to_string(), None),
                ("/ws?role=tunnel-client&token=tok".to_string(), None),
            ]
        );
    }

    #[test]
    fn tenant_id_from_jwt_rejects_non_oid_and_garbage() {
        // Non-hex / wrong length values are dropped (would 403 server-side).
        let jwt = fake_jwt(r#"{"tenant_id":"not-an-object-id"}"#);
        assert_eq!(tenant_id_from_jwt(&jwt), None);
        // Missing claim.
        let jwt2 = fake_jwt(r#"{"sub":"x"}"#);
        assert_eq!(tenant_id_from_jwt(&jwt2), None);
        // Not a JWT at all.
        assert_eq!(tenant_id_from_jwt("hello"), None);
    }
}
