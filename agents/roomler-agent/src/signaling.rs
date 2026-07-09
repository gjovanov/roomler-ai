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
    signaling::{AgentCloseReason, ClientMsg, ServerMsg},
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::config::AgentConfig;
use crate::indicator::ViewerIndicator;
use crate::notify;
use crate::peer::AgentPeer;
use crate::watchdog;
use tunnel_core::localapi::OverlayView;
use tunnel_core::transport::relay;

/// Capacity of the outbound channel peers use to push `ClientMsg` back into
/// the signaling loop (ICE trickles, terminate signals). 64 is generous for
/// one session's ICE gather phase.
const PEER_OUTBOUND_CAP: usize = 64;

/// Per-flow `connect()` budget for `rc:tunnel.tcp.forward` requests.
/// Matches the dialer's default — see `tunnel::dialer::DEFAULT_TIMEOUT`.
const TUNNEL_DIAL_TIMEOUT: Duration = Duration::from_secs(5);

/// rc.58: hard upper bound on a single `connect_async` attempt. Without
/// a wrapper, a hung TLS handshake (e.g. server-side renegotiation
/// race against a rustls client that refuses re-negotiation) sits
/// inside `connect_async` indefinitely and the outer backoff ladder
/// never fires. 30 s is much longer than a healthy WSS handshake
/// (<1 s typical) and short enough that the operator notices in field
/// logs. Timeouts are routed through `ConnectError::Transient` so the
/// backoff loop handles them like any other connection failure.
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// rc.58: format an error chain by walking `source()` so the top-level
/// `Display` (which `tokio_tungstenite::Error` keeps deliberately
/// terse) doesn't hide the root cause — TLS handshake error,
/// ECONNREFUSED, EAI_NONAME, etc. Field repro 2026-05-24: a flaky
/// network turned every cold start into `error=ws connect` with no
/// further detail, making it impossible to tell a DNS failure from a
/// TLS failure without packet capture. The `preflight` module ships
/// the same helper; duplicated here to avoid forcing every consumer
/// crate to depend on `preflight`.
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut src = err.source();
    while let Some(cause) = src {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        src = cause.source();
    }
    out
}

/// rc.58: RAII guard that flips the watchdog's `signaling` pump on
/// for the lifetime of a single live WebSocket connection. On drop
/// (every return path from `connect_once`, including `?` early-exit)
/// the pump goes back to gated-off so the next reconnect-backoff loop
/// doesn't count its silence against the 90 s stall threshold.
///
/// Before rc.58 the pump was registered with `active=true` from
/// process start, so the watchdog's 90 s timer ran during initial
/// exponential backoff against an unreachable server — every cold
/// start with a flaky network got force-exited at 90 s and the
/// supervisor crash-looped forever. See `main.rs` register call for
/// the symmetric flip there.
struct SignalingPumpGuard;

impl SignalingPumpGuard {
    /// Activate the watchdog's `signaling` pump and reset its
    /// `last_tick` (the `gate(false → true)` transition resets the
    /// timer; see `watchdog.rs::Watchdog::gate`). Use right after
    /// `connect_async` returns Ok.
    fn activate() -> Self {
        watchdog::gate("signaling", true);
        Self
    }
}

impl Drop for SignalingPumpGuard {
    fn drop(&mut self) {
        watchdog::gate("signaling", false);
    }
}

/// Unification P1 — RAII flag that marks the daemon "connected to the
/// coordination server" for the LocalAPI (`roomler status`) while a WS
/// connection is live, and clears it on EVERY exit path from `connect_once`
/// (Ok, `?`-propagated Err, explicit return) — same discipline as
/// [`SignalingPumpGuard`]. The `DaemonState` reads this flag; while it's false
/// `peers()` reports none (the overlay carriers are torn down on disconnect).
struct ConnectedGuard(Arc<AtomicBool>);

impl ConnectedGuard {
    fn mark(flag: Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::Relaxed);
        Self(flag)
    }
}

impl Drop for ConnectedGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Drive the signaling loop forever. Returns only on fatal error (e.g.
/// auth rejection) or shutdown signal.
pub async fn run(
    cfg: AgentConfig,
    encoder_preference: crate::encode::EncoderPreference,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    // Unification P1 — LocalAPI live handles (stable across reconnects, owned by
    // `run_cmd`): a flag flipped while connected, and the channel the overlay
    // runtime publishes its mesh view on.
    connected: Arc<AtomicBool>,
    overlay_view_tx: tokio::sync::watch::Sender<OverlayView>,
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
    // Operator-consent broker lives across reconnects so a sentinel
    // file dropped while the WS was down is still honoured when the
    // next session.request arrives. Mode is locked at startup from
    // the config (back-compat default = AutoGrant; org fleets flip
    // `auto_grant_session=false` to require operator approval).
    let consent_mode = crate::consent::Mode::from_config(cfg.auto_grant_session);
    let consent_dir = match crate::consent::ConsentBroker::default_sentinel_dir() {
        Ok(p) => p,
        Err(e) => {
            warn!(%e, "could not resolve consent sentinel dir; falling back to temp dir");
            std::env::temp_dir().join("roomler-agent-consent")
        }
    };
    let consent_broker = match crate::consent::ConsentBroker::new(consent_mode, consent_dir.clone())
    {
        Ok(b) => b,
        Err(e) => {
            warn!(%e, "consent broker init failed; defaulting to AutoGrant for this run");
            // Fail open in the broker-init failure path so a directory-
            // permission glitch doesn't stop the agent from servicing
            // self-host (auto-grant) deployments. Org-controlled
            // fleets relying on Prompt mode still get audit visibility
            // via the failure log line above.
            crate::consent::ConsentBroker::new(
                crate::consent::Mode::AutoGrant,
                std::env::temp_dir(),
            )
            .expect("AutoGrant broker init cannot fail with temp_dir")
        }
    };
    info!(
        mode = ?consent_broker.mode(),
        sentinel_dir = %consent_broker.sentinel_dir().display(),
        "operator-consent broker ready"
    );
    let mut backoff = Duration::from_secs(1);
    let mut auth_failures: u32 = 0;
    // rc.53: rolling window of recent `ReplacedByNewerConnection`
    // events. Three within 5 min escalates from "back off 60 s and
    // hope the duel settles" to "operator action required +
    // process::exit(AGENT_DELETED_EXIT_CODE)" — at that point the
    // duel is real, neither instance can win, and the operator needs
    // to find + stop the duplicate (or re-enrol THIS host with a
    // fresh enrollment JWT to mint a new agent_id).
    let mut recent_replacements: Vec<std::time::Instant> = Vec::new();
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
            consent_broker.clone(),
            connected.clone(),
            overlay_view_tx.clone(),
        )
        .await
        {
            Ok(()) => {
                info!("signaling connection closed cleanly, reconnecting");
                backoff = Duration::from_secs(1);
                if auth_failures > 0 {
                    info!(
                        prior_auth_failures = auth_failures,
                        "auth recovered; clearing attention sentinel"
                    );
                    notify::clear_attention();
                    auth_failures = 0;
                }
            }
            Err(ConnectError::AuthRejected) => {
                auth_failures = auth_failures.saturating_add(1);
                let auth_backoff = auth_backoff_for(auth_failures);
                warn!(
                    consecutive = auth_failures,
                    retry_in_secs = auth_backoff.as_secs(),
                    "agent token rejected; will retry — re-enrollment may be required"
                );
                // Raise the attention sentinel after the third
                // consecutive 401 — by then a transient server-side
                // JWT-cache miss has had time to recover and the
                // operator genuinely needs to act.
                if auth_failures == 3 {
                    let msg = "Roomler agent: re-enrollment required.\n\n\
                              The server is rejecting this agent's token. \
                              Either the token expired (default 1 year) or an \
                              admin revoked it. Run:\n\n\
                              \troomler-agent re-enroll --token <new-jwt>\n\n\
                              with a fresh enrollment JWT from the admin UI \
                              to restore service.";
                    match notify::raise_attention(msg) {
                        Ok(path) => warn!(
                            path = %path.display(),
                            "wrote needs-attention sentinel"
                        ),
                        Err(e) => warn!(error = %e, "failed to write needs-attention sentinel"),
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(auth_backoff) => {},
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return Ok(()); }
                    },
                }
            }
            Err(ConnectError::FatalGoodbye { reason, message }) => {
                // rc.53: server told us to stop reconnecting. The
                // teardown of in-flight peers already ran in the
                // `handle_server_msg::ServerMsg::Goodbye` arm
                // (close_all_peers + close_all_tunnel_peers) — this
                // arm only writes the operator sentinel + exits.
                let body = format!(
                    "Roomler agent: server-side close — {reason:?}.\n\n{message}\n\n\
                     The agent will not reconnect. Re-enrol with a fresh enrollment \
                     JWT from the admin UI:\n\n\
                     \troomler-agent re-enroll --token <new-jwt>\n\n\
                     then restart the service (or wait for the supervisor to relaunch)."
                );
                match notify::raise_attention_machine_aware(&body) {
                    Ok(path) => warn!(
                        path = %path.display(),
                        ?reason,
                        "wrote needs-attention sentinel for FatalGoodbye"
                    ),
                    Err(e) => warn!(
                        error = %e,
                        ?reason,
                        "failed to write needs-attention sentinel for FatalGoodbye"
                    ),
                }
                // Exit with the documented code so the SCM
                // supervisor's rc.53 code-7 fast-alarm fires on this
                // FIRST exit (not after 8). Operator sees the
                // structured error within <1 minute.
                std::process::exit(watchdog::AGENT_DELETED_EXIT_CODE);
            }
            Err(ConnectError::ReplacedByNewer { message }) => {
                let now = std::time::Instant::now();
                // Drop events older than the 5 min rolling window
                // BEFORE pushing the new one (so escalation depends
                // only on what's actually within the window).
                recent_replacements
                    .retain(|t| now.duration_since(*t) < Duration::from_secs(5 * 60));
                recent_replacements.push(now);
                warn!(
                    %message,
                    count = recent_replacements.len(),
                    "server signalled this connection was replaced; staggering reconnect to break the duel"
                );

                if recent_replacements.len() >= 3 {
                    let body = format!(
                        "Roomler agent: duplicate-instance duel detected.\n\n{message}\n\n\
                         This connection has been displaced {} times in the last 5 minutes — \
                         another process (different physical host with a copy of this \
                         config.toml, or a tray companion, etc.) is using the same agent_id. \
                         Stop the duplicate or re-enrol THIS host with a fresh enrollment \
                         JWT to mint a new agent_id.",
                        recent_replacements.len()
                    );
                    match notify::raise_attention_machine_aware(&body) {
                        Ok(path) => warn!(
                            path = %path.display(),
                            displacements = recent_replacements.len(),
                            "wrote needs-attention sentinel for ReplacedByNewer escalation"
                        ),
                        Err(e) => warn!(
                            error = %e,
                            "failed to write needs-attention sentinel for ReplacedByNewer escalation"
                        ),
                    }
                    std::process::exit(watchdog::AGENT_DELETED_EXIT_CODE);
                }

                // Back off 60 s minimum — long enough that two
                // duelling instances stagger out of phase and one
                // wins. Shorter would put both in sync and burn
                // attempts before the escalation gate.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {},
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return Ok(()); }
                    },
                }
                // Reset the transient backoff — the next reconnect
                // attempt is paced by the 60 s above, not by the
                // exponential ladder.
                backoff = Duration::from_secs(1);
            }
            Err(ConnectError::Transient(e)) => {
                // rc.58: log the full `source()` chain alongside the
                // top-level Display — `tokio_tungstenite::Error`'s top
                // message is just "ws connect" / "ws read" without the
                // underlying TLS / DNS / ECONNREFUSED detail. Field
                // repro 2026-05-24: a flaky WSS handshake produced
                // identical-looking `error=ws connect` lines for
                // every failure mode, blocking root-cause analysis.
                let cause = error_chain(e.as_ref());
                warn!(error = %e, %cause, "signaling connect failed; backing off");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {},
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return Ok(()); }
                    },
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

/// Auth-rejection backoff ladder. Tuned for "transient server JWT
/// cache miss recovers fast; persistent revocation gets surfaced to
/// the operator without burning CPU on retry storms."
///
/// 1st failure → 30 s (server might just be deploying)
/// 2nd → 60 s
/// 3rd → 5 min (sentinel raises here too)
/// 4th and beyond → 1 hour (stable steady-state)
pub(crate) fn auth_backoff_for(consecutive_failures: u32) -> Duration {
    match consecutive_failures {
        0 | 1 => Duration::from_secs(30),
        2 => Duration::from_secs(60),
        3 => Duration::from_secs(5 * 60),
        _ => Duration::from_secs(60 * 60),
    }
}

#[derive(Debug, thiserror::Error)]
enum ConnectError {
    #[error("auth rejected")]
    AuthRejected,
    /// rc.53: server explicitly told us to stop reconnecting (row
    /// deleted, policy refused, or any unknown future-variant reason
    /// which `AgentCloseReason::Deserialize` rounds to
    /// `PolicyRejected`). The outer `run()` loop responds with a
    /// needs-attention sentinel + `process::exit(AGENT_DELETED_EXIT_CODE)`
    /// so the SCM supervisor's code-7 fast-alarm fires immediately.
    #[error("fatal goodbye: {reason:?}: {message}")]
    FatalGoodbye {
        reason: AgentCloseReason,
        message: String,
    },
    /// rc.53: server told us a newer WS connection displaced us
    /// (duplicate-instance duel). The outer loop backs off ≥60 s on
    /// the first 1-2 events (so two duelling instances stagger out
    /// of phase and one wins); escalates to fatal +
    /// process::exit(AGENT_DELETED_EXIT_CODE) on the 3rd event
    /// within a 5 min rolling window.
    #[error("replaced by newer connection: {message}")]
    ReplacedByNewer { message: String },
    #[error(transparent)]
    Transient(#[from] anyhow::Error),
}

#[allow(clippy::too_many_arguments)]
async fn connect_once(
    cfg: &AgentConfig,
    encoder_preference: crate::encode::EncoderPreference,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    indicator: ViewerIndicator,
    consent_broker: crate::consent::ConsentBroker,
    connected: Arc<AtomicBool>,
    overlay_view_tx: tokio::sync::watch::Sender<OverlayView>,
) -> Result<(), ConnectError> {
    let url = format!(
        "{}?token={}&role=agent",
        cfg.ws_url(),
        urlencode(&cfg.agent_token)
    );
    info!(%url, "connecting to signaling server");

    // rc.58: wrap `connect_async` in a hard timeout. A hung TLS
    // handshake (rustls refusing renegotiation against an LB that
    // requests it mid-stream is one observed mode) would otherwise
    // sit here indefinitely, never giving the outer backoff loop a
    // chance to fire. The timeout becomes another `Transient` so the
    // backoff handles it like any other connection failure.
    let (mut ws, response) =
        match tokio::time::timeout(WS_CONNECT_TIMEOUT, connect_async(&url)).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                if let tokio_tungstenite::tungstenite::Error::Http(ref resp) = e
                    && resp.status().as_u16() == 401
                {
                    return Err(ConnectError::AuthRejected);
                }
                return Err(ConnectError::Transient(
                    anyhow::Error::new(e).context("ws connect"),
                ));
            }
            Err(_elapsed) => {
                return Err(ConnectError::Transient(anyhow::anyhow!(
                    "ws connect timed out after {}s",
                    WS_CONNECT_TIMEOUT.as_secs()
                )));
            }
        };
    debug!(status = ?response.status(), "ws upgrade complete");

    // rc.58: now that the WS handshake is done, flip the watchdog's
    // `signaling` pump on for the lifetime of this connection. The
    // RAII guard ensures EVERY return path (Ok, ?-propagated Err,
    // explicit `return Err(...)`) also flips it back off, so the next
    // backoff-reconnect cycle isn't counted against the 90 s stall
    // threshold. See the type-level comment on `SignalingPumpGuard`.
    let _pump_guard = SignalingPumpGuard::activate();
    // Unification P1 — mark the daemon connected for the LocalAPI; the guard
    // clears it on every return path (like `_pump_guard`).
    let _connected_guard = ConnectedGuard::mark(connected);

    // Say hello.
    let hello = ClientMsg::AgentHello {
        machine_name: cfg.machine_name.clone(),
        os: detect_os(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        displays: stub_displays(),
        caps: stub_caps(),
    };
    send_msg(&mut ws, &hello).await.context("sending hello")?;
    // rc.58: explicit tick on hello — the 25 s keepalive timer hasn't
    // fired yet, and a slow first server response (no inbound frame
    // for 30+ s) would otherwise leave the pump's `last_tick` at the
    // gate-activation instant. Belt-and-suspenders: the gate already
    // reset the timer, so this only matters when the server stalls
    // immediately after upgrade.
    watchdog::tick("signaling");
    info!("rc:agent.hello sent");

    // Outbound channel shared by all per-session peers. Peers push their
    // locally-gathered ICE candidates and state-change terminates here;
    // the main loop flushes them to the WS.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<ClientMsg>(PEER_OUTBOUND_CAP);
    // Phase 3b: if overlay is enabled, start the node runtime (relay mode)
    // and capture the channel its `rc:overlay.*` events flow into. The
    // runtime sends its `ClientMsg`s back through `outbound_tx`, like any
    // peer, and tears down when this connection's `overlay_evt_tx` drops.
    #[cfg(feature = "overlay-l3")]
    let overlay_evt_tx =
        crate::overlay::maybe_start(cfg, outbound_tx.clone(), overlay_view_tx.clone());
    // Without the overlay feature nothing publishes the view; keep the param
    // used so the LocalAPI wiring stays feature-agnostic in `run_cmd`.
    #[cfg(not(feature = "overlay-l3"))]
    let _ = &overlay_view_tx;
    let mut peers: HashMap<bson::oid::ObjectId, AgentPeer> = HashMap::new();
    // Codec selected for each pending session (computed from the
    // browser∩agent intersection when `rc:session.request` arrives, read
    // at `rc:sdp.offer` time to drive the track + encoder). Entries are
    // removed when the peer is built; orphaned entries (session
    // cancelled before SDP) get cleaned when the session is terminated.
    let mut pending_codecs: HashMap<bson::oid::ObjectId, String> = HashMap::new();
    // Y.3: same lifecycle as `pending_codecs` but for the negotiated
    // video transport. Inserted when `rc:session.request` arrives,
    // consumed when `rc:sdp.offer` builds the AgentPeer + media pump.
    // `Some("data-channel-vp9-444")` flips the pump into DC mode;
    // None is the legacy WebRTC track.
    let mut pending_transports: HashMap<bson::oid::ObjectId, Option<String>> = HashMap::new();
    // rc.62: same lifecycle as `pending_transports` but for the
    // per-session VP9 chroma override forwarded from the controller's
    // `rc:session.request.chroma_pref`. `None` → fall back to the
    // agent's `ROOMLER_AGENT_VP9_CHROMA` env-var default.
    let mut pending_chroma: HashMap<bson::oid::ObjectId, Option<String>> = HashMap::new();
    // T2.10d: one `AgentTunnelPeer` per active `roomler-tunnel`
    // session. Distinct map from `peers` (remote-control sessions)
    // because the namespaces don't overlap and the lifecycles
    // differ — tunnel peers live until `TunnelTerminate` /
    // disconnect; rc peers live until session-end.
    let mut tunnel_peers: HashMap<bson::oid::ObjectId, Arc<crate::tunnel::peer::AgentTunnelPeer>> =
        HashMap::new();
    // Phase 1d (quic-v1): one `AgentQuicPeer` per active tunnel session
    // negotiated onto the QUIC transport. Separate map from
    // `tunnel_peers` (WebRTC DC) because a session uses exactly one
    // data plane — `TcpForwardForward` dispatch checks this map first
    // and falls back to the WebRTC `tunnel_peers` map. Same lifecycle:
    // live until `TunnelTerminate` / WS disconnect.
    let mut tunnel_quic_peers: HashMap<
        bson::oid::ObjectId,
        Arc<crate::tunnel::quic_peer::AgentQuicPeer>,
    > = HashMap::new();

    // Keepalive. nginx + K8s ingress commonly idle-close WSes at 60-120s of
    // silence; send an application-level Ping every 25s so the connection
    // survives quiet periods between sessions.
    let mut keepalive = tokio::time::interval(Duration::from_secs(25));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await; // Swallow the immediate first tick.

    // Phase 7: heartbeat telemetry. The server uses this to refresh
    // `agents.last_seen_at` so a quiet but connected agent doesn't
    // appear "online forever" if its WS dies silently. 30 s cadence
    // pairs with a "online if last_seen_at > now - 90 s" rule on the
    // server side. rss_mb / cpu_pct are 0 for v1 — populating them
    // needs a process-self metrics crate (sysinfo) that we'd rather
    // ship in a follow-up. active_sessions comes straight from the
    // peer map.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await; // Swallow the immediate first tick.

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("shutdown signalled; closing ws");
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    close_all_tunnel_quic_peers(&mut tunnel_quic_peers).await;
                    let _ = ws.send(Message::Close(None)).await;
                    return Ok(());
                }
            }
            _ = keepalive.tick() => {
                if let Err(e) = ws.send(Message::Ping(Vec::new().into())).await {
                    warn!(%e, "keepalive ping failed — will reconnect");
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    close_all_tunnel_quic_peers(&mut tunnel_quic_peers).await;
                    return Err(ConnectError::Transient(anyhow::Error::new(e).context("ws ping")));
                }
                // Liveness: a successful keepalive proves the WS pump
                // is healthy even during long quiet periods between
                // sessions. Without this tick the watchdog would flag
                // a stall after 90 s of no inbound traffic.
                watchdog::tick("signaling");
            }
            _ = heartbeat.tick() => {
                let hb = ClientMsg::AgentHeartbeat {
                    rss_mb: 0,
                    cpu_pct: 0.0,
                    active_sessions: peers.len().min(u8::MAX as usize) as u8,
                };
                if let Err(e) = send_msg(&mut ws, &hb).await {
                    warn!(%e, "heartbeat send failed — will reconnect");
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    close_all_tunnel_quic_peers(&mut tunnel_quic_peers).await;
                    return Err(ConnectError::Transient(e.context("heartbeat send")));
                }
                watchdog::tick("signaling");
            }
            Some(outbound_msg) = outbound_rx.recv() => {
                if let Err(e) = send_msg(&mut ws, &outbound_msg).await {
                    warn!(%e, "failed to flush peer-originated message");
                }
                watchdog::tick("signaling");
            }
            maybe_msg = ws.next() => match maybe_msg {
                Some(Ok(Message::Text(text))) => {
                    watchdog::tick("signaling");
                    match serde_json::from_str::<ServerMsg>(&text) {
                        Ok(parsed) => {
                            // Phase 3b: route `rc:overlay.*` to the node
                            // runtime; everything else falls through to the
                            // normal dispatch below.
                            #[cfg(feature = "overlay-l3")]
                            let parsed = match &overlay_evt_tx {
                                Some(tx) => match crate::overlay::intercept(tx, parsed) {
                                    Some(p) => p,
                                    None => continue,
                                },
                                None => parsed,
                            };
                            handle_server_msg(
                                &mut ws,
                                parsed,
                                &mut peers,
                                &mut pending_codecs,
                                &mut pending_transports,
                                &mut pending_chroma,
                                &mut tunnel_peers,
                                &mut tunnel_quic_peers,
                                &outbound_tx,
                                encoder_preference,
                                &indicator,
                                &consent_broker,
                                &cfg.forward_acl,
                            )
                            .await?;
                        }
                        Err(e) => debug!(%e, text = %text.as_str(), "ignoring non-rc:* frame"),
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    let _ = ws.send(Message::Pong(data)).await;
                    watchdog::tick("signaling");
                }
                Some(Ok(Message::Close(_))) | None => {
                    info!("ws closed by peer");
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    close_all_tunnel_quic_peers(&mut tunnel_quic_peers).await;
                    return Ok(());
                }
                Some(Err(e)) => {
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    close_all_tunnel_quic_peers(&mut tunnel_quic_peers).await;
                    return Err(ConnectError::Transient(anyhow::Error::new(e).context("ws read")));
                }
                _ => {}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_server_msg(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    msg: ServerMsg,
    peers: &mut HashMap<bson::oid::ObjectId, AgentPeer>,
    pending_codecs: &mut HashMap<bson::oid::ObjectId, String>,
    pending_transports: &mut HashMap<bson::oid::ObjectId, Option<String>>,
    pending_chroma: &mut HashMap<bson::oid::ObjectId, Option<String>>,
    tunnel_peers: &mut HashMap<bson::oid::ObjectId, Arc<crate::tunnel::peer::AgentTunnelPeer>>,
    tunnel_quic_peers: &mut HashMap<
        bson::oid::ObjectId,
        Arc<crate::tunnel::quic_peer::AgentQuicPeer>,
    >,
    outbound_tx: &mpsc::Sender<ClientMsg>,
    encoder_preference: crate::encode::EncoderPreference,
    indicator: &ViewerIndicator,
    consent_broker: &crate::consent::ConsentBroker,
    forward_acl: &crate::tunnel::acl::AgentForwardAcl,
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
            chroma_pref,
            consent_mode,
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
            // Stash for the upcoming SdpOffer handler — that's where
            // AgentPeer::new is called and the media pump is built.
            // Without this stash the negotiation result was logged but
            // not actually applied (the bug Y.3's media-pump branch
            // surfaces).
            pending_transports.insert(session_id, negotiated_transport.clone());
            // rc.62 — stash per-session chroma override so the
            // SdpOffer handler can pass it into `AgentPeer::new` and
            // ultimately into the VP9-444 media pump. Only meaningful
            // when negotiated_transport == data-channel-vp9-444;
            // ignored otherwise.
            pending_chroma.insert(session_id, chroma_pref.clone());
            info!(
                %session_id, %controller_user_id, %controller_name,
                ?permissions, consent_timeout_secs,
                browser_caps = ?browser_caps,
                chosen_codec = %chosen,
                requested_transport = ?preferred_transport,
                negotiated_transport = ?negotiated_transport,
                chroma_pref = ?chroma_pref,
                consent_mode = ?consent_broker.mode(),
                "incoming session request — running consent broker"
            );
            // Show the "someone is watching" overlay on the controlled
            // host. Harmless no-op on non-Windows or when the feature
            // is disabled. Indicator goes up at request time (not after
            // grant) so an in-prompt operator sees the visual cue
            // alongside the prompt — defence-in-depth against a sneaky
            // unattended grant.
            indicator.show_session(session_id.to_hex(), controller_name.clone());
            // Spawn a task to run the broker decision in the
            // background; auto-grant resolves <1ms, prompt mode can
            // take up to 30s — we MUST NOT block the WS read loop.
            // Decision flows back via outbound_tx as a ClientMsg::Consent.
            // Phase 2 — obey the server's per-session consent directive when
            // present; fall back to the broker's startup mode (local
            // `auto_grant_session`) for an older server that sends none.
            let directed_mode: Option<crate::consent::Mode> = consent_mode.map(|m| match m {
                roomler_ai_remote_control::models::ConsentMode::Auto => {
                    crate::consent::Mode::AutoGrant
                }
                // Prompt + the async owner-side channels (Email / Push /
                // PromptThenEmail) all resolve to an on-host prompt at the
                // agent: the server drives the owner channels itself (Phase 4)
                // and asks the agent to prompt as the on-console path/fallback.
                // Bound the wait by the server-sent timeout.
                _ => crate::consent::Mode::Prompt {
                    timeout: std::time::Duration::from_secs(consent_timeout_secs as u64),
                },
            });
            // Resolve to a concrete mode (directive, else the broker's startup
            // mode) so the .pending write and the request use the same decision.
            let effective_mode = directed_mode.unwrap_or_else(|| consent_broker.mode());
            let session_hex = session_id.to_hex();
            // Phase 4 — Email/Push are OWNER-side modes: the SERVER obtains
            // consent from the device owner (email link / push), so the agent
            // must NOT decide — no prompt, no `.pending`, no `rc:consent`. It
            // just waits; when the owner approves, the server sends `rc:ready`,
            // the controller offers, and the agent builds the peer from the
            // media context stashed above.
            let owner_side_consent = matches!(
                consent_mode,
                Some(roomler_ai_remote_control::models::ConsentMode::Email)
                    | Some(roomler_ai_remote_control::models::ConsentMode::Push)
            );
            // Phase 3 — when this session will PROMPT on the host, drop a
            // `.pending` marker so the tray can pop a rich Approve/Deny modal
            // (the agent→tray signal). Auto grants + owner-side modes write
            // nothing. The broker's poll loop removes it when the decision
            // resolves. Best-effort; a failure falls back to the CLI path.
            if !owner_side_consent && matches!(effective_mode, crate::consent::Mode::Prompt { .. })
            {
                let body = serde_json::json!({
                    "session_id": session_hex,
                    "controller_name": controller_name,
                    "permissions": permissions,
                    "timeout_secs": consent_timeout_secs,
                })
                .to_string();
                if let Err(e) = consent_broker.write_pending(&session_hex, &body) {
                    tracing::warn!(session = %session_hex, %e, "could not write .pending consent marker for tray");
                }
            }
            if owner_side_consent {
                info!(
                    %session_id, ?consent_mode,
                    "owner-side consent (email/push) — agent waits for the server to resolve"
                );
            } else {
                let broker = consent_broker.clone();
                let outbound = outbound_tx.clone();
                tokio::spawn(async move {
                    let decision = broker.request_with_mode(&session_hex, effective_mode).await;
                    let granted = decision.granted();
                    tracing::info!(
                        session = %session_hex,
                        ?decision,
                        ?effective_mode,
                        granted,
                        "consent decision → sending rc:consent"
                    );
                    if let Err(e) = outbound
                        .send(ClientMsg::Consent {
                            session_id,
                            granted,
                        })
                        .await
                    {
                        tracing::warn!(session = %session_hex, %e, "outbound consent send failed (channel closed)");
                    }
                });
            }
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
            // Y.3: pull the transport stashed in the request handler.
            // `None` (legacy WebRTC track) is the silent default for
            // older controllers / sessions that arrived without
            // preferred_transport.
            let negotiated_transport = pending_transports.remove(&session_id).unwrap_or(None);
            // rc.62 — pull the per-session chroma override stashed by
            // the Request handler. `None` → AgentPeer falls back to
            // `ROOMLER_AGENT_VP9_CHROMA` env-var default.
            let chroma_pref = pending_chroma.remove(&session_id).unwrap_or(None);

            let peer = match AgentPeer::new(
                session_id,
                &ice_servers,
                outbound_tx.clone(),
                encoder_preference,
                chosen_codec,
                negotiated_transport,
                chroma_pref,
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
            // Drop any orphaned pending-codec / transport entry for
            // this session so the maps don't accumulate under long-
            // running agents (e.g. sessions cancelled before SDP is
            // exchanged).
            pending_codecs.remove(&session_id);
            pending_transports.remove(&session_id);
            indicator.hide_session(session_id.to_hex());
        }

        ServerMsg::Error {
            session_id,
            code,
            message,
        } => {
            warn!(?session_id, %code, %message, "server-side rc error");
        }

        // rc.53: server has decided this WS is over. Tear down every
        // peer cleanly (so the controller side gets clean ICE-restart
        // hints rather than a 10-30 s silence-detect) and surface the
        // reason via a typed ConnectError so the outer `run()` loop
        // can decide between fatal exit (`AgentDeleted` /
        // `PolicyRejected`) and back-off-with-escalation
        // (`ReplacedByNewerConnection`).
        //
        // The teardown invariant lives HERE — not in `run()` — because
        // the existing `connect_once` exit paths only run cleanup at
        // explicit `return` sites and the `?` propagation of this
        // arm's Err would otherwise SKIP `close_all_peers`. SM-1b
        // (delete-agent-with-active-session) checks this invariant
        // explicitly.
        ServerMsg::Goodbye { reason, message } => {
            tracing::error!(
                ?reason,
                %message,
                "server-side rc:goodbye received — stopping current session loop"
            );
            close_all_peers(peers, indicator).await;
            close_all_tunnel_peers(tunnel_peers).await;
            close_all_tunnel_quic_peers(tunnel_quic_peers).await;
            // Drop pending codec / transport entries too; they're tied
            // to in-flight session_ids that no longer have peers.
            pending_codecs.clear();
            pending_transports.clear();
            return match reason {
                AgentCloseReason::AgentDeleted | AgentCloseReason::PolicyRejected => {
                    Err(ConnectError::FatalGoodbye { reason, message })
                }
                AgentCloseReason::ReplacedByNewerConnection => {
                    Err(ConnectError::ReplacedByNewer { message })
                }
            };
        }

        // Controller-oriented messages shouldn't reach us.
        ServerMsg::Ready { session_id, .. }
        | ServerMsg::SessionCreated { session_id, .. }
        | ServerMsg::SdpAnswer { session_id, .. } => {
            debug!(%session_id, "unexpected controller-side msg on agent socket");
        }
        ServerMsg::Pong { .. } => {}

        // rc:tunnel.tcp.forward — server has gated the request, asks
        // the agent to dial dst + reply with Accept/Reject via the
        // outbound channel. The acceptor handles ACL + dial in an
        // async task so the WS read loop is never blocked. Owner is
        // recorded in the audit log but not consulted here (server
        // is authoritative for policy).
        ServerMsg::TcpForwardForward {
            session_id,
            flow_id,
            dst_host,
            dst_port,
            owner_user_id: _,
        } => {
            // Dispatch on the session's negotiated data plane: a QUIC
            // session has an entry in `tunnel_quic_peers`, otherwise
            // it's a WebRTC-DC session in `tunnel_peers`. The server
            // negotiates exactly one transport per session, so at most
            // one map matches; QUIC is checked first.
            if let Some(quic_peer) = tunnel_quic_peers.get(&session_id).cloned() {
                let outbound = outbound_tx.clone();
                let acl = forward_acl.clone();
                tokio::spawn(async move {
                    crate::tunnel::acceptor::handle_forward_request_quic(
                        session_id,
                        flow_id,
                        &dst_host,
                        dst_port,
                        &acl,
                        TUNNEL_DIAL_TIMEOUT,
                        &quic_peer,
                        outbound,
                    )
                    .await;
                });
                return Ok(());
            }
            // Look up the WebRTC tunnel peer for this session — must
            // exist before the server is allowed to relay a forward
            // request for it. If absent (race / bad server), synthesise
            // an AgentError reject so the client doesn't hang.
            let Some(tunnel_peer) = tunnel_peers.get(&session_id).cloned() else {
                warn!(%session_id, %flow_id, "TcpForwardForward for unknown tunnel session — rejecting");
                let reply = ClientMsg::TcpForwardReject {
                    session_id,
                    flow_id,
                    kind: roomler_ai_remote_control::signaling::RejectKind::AgentError,
                    reason: "tunnel session not open on agent".into(),
                };
                let _ = outbound_tx.send(reply).await;
                return Ok(());
            };
            let outbound = outbound_tx.clone();
            let acl = forward_acl.clone();
            tokio::spawn(async move {
                crate::tunnel::acceptor::handle_forward_request(
                    session_id,
                    flow_id,
                    &dst_host,
                    dst_port,
                    &acl,
                    TUNNEL_DIAL_TIMEOUT,
                    &tunnel_peer,
                    outbound,
                )
                .await;
            });
        }

        // rc:tunnel.udp.forward — UDP ASSOCIATE analogue of
        // TcpForwardForward. Same dispatch: a QUIC session dials over
        // its quinn peer, a WebRTC-DC session over its DC pool. The
        // acceptor binds a target UDP socket, replies Accept/Reject,
        // and pumps datagrams.
        ServerMsg::UdpForwardForward {
            session_id,
            flow_id,
            dst_host,
            dst_port,
            owner_user_id: _,
        } => {
            if let Some(quic_peer) = tunnel_quic_peers.get(&session_id).cloned() {
                let outbound = outbound_tx.clone();
                let acl = forward_acl.clone();
                tokio::spawn(async move {
                    crate::tunnel::acceptor::handle_udp_forward_request_quic(
                        session_id,
                        flow_id,
                        &dst_host,
                        dst_port,
                        &acl,
                        TUNNEL_DIAL_TIMEOUT,
                        &quic_peer,
                        outbound,
                    )
                    .await;
                });
                return Ok(());
            }
            let Some(tunnel_peer) = tunnel_peers.get(&session_id).cloned() else {
                warn!(%session_id, %flow_id, "UdpForwardForward for unknown tunnel session — rejecting");
                let reply = ClientMsg::UdpForwardReject {
                    session_id,
                    flow_id,
                    kind: roomler_ai_remote_control::signaling::RejectKind::AgentError,
                    reason: "tunnel session not open on agent".into(),
                };
                let _ = outbound_tx.send(reply).await;
                return Ok(());
            };
            let outbound = outbound_tx.clone();
            let acl = forward_acl.clone();
            tokio::spawn(async move {
                crate::tunnel::acceptor::handle_udp_forward_request(
                    session_id,
                    flow_id,
                    &dst_host,
                    dst_port,
                    &acl,
                    TUNNEL_DIAL_TIMEOUT,
                    &tunnel_peer,
                    outbound,
                )
                .await;
            });
        }

        // rc:tunnel.sdp.offer — controller's offer for the WebRTC
        // peer. Build an AgentTunnelPeer, accept the offer, ship the
        // answer back as `rc:tunnel.sdp.answer`. The peer takes care
        // of its own ICE trickle via the outbound channel.
        ServerMsg::TunnelSdpOffer { session_id, sdp } => {
            match crate::tunnel::peer::AgentTunnelPeer::accept_offer(
                session_id,
                &sdp,
                Vec::new(),
                outbound_tx.clone(),
            )
            .await
            {
                Ok((peer, answer_sdp)) => {
                    tunnel_peers.insert(session_id, Arc::new(peer));
                    let _ = outbound_tx
                        .send(ClientMsg::TunnelSdpAnswer {
                            session_id,
                            sdp: answer_sdp,
                        })
                        .await;
                    info!(%session_id, "agent tunnel peer constructed; SDP answer sent");
                }
                Err(e) => {
                    warn!(%session_id, %e, "tunnel accept_offer failed");
                }
            }
        }

        // rc:tunnel.quic.setup — QUIC analogue of TunnelSdpOffer. The
        // server's trigger to stand up a quinn server endpoint for this
        // session and authorize the client bearing `quic_auth_token`.
        // We mint an ephemeral cert + bind the endpoint, then reply
        // `rc:tunnel.quic.ready` with the cert fingerprint (for the
        // client to pin — there's no CA) + dialable addrs.
        ServerMsg::TunnelQuicSetup {
            session_id,
            quic_auth_token,
            ice_servers,
        } => {
            // Phase 3d: if the server minted coturn creds, ride QUIC over
            // a TURN relay (QUIC-over-TURN) so symmetric-NAT /
            // UDP-restricted hosts are reachable; the relay peer
            // advertises its coturn relayed address. Otherwise bind a
            // direct 0.0.0.0:0 UDP endpoint (same-LAN / directly-
            // reachable; Phase 2a host candidates). A relay allocation
            // failure is non-fatal — we simply don't reply
            // `rc:tunnel.quic.ready`, and the client soft-falls back to
            // webrtc-dc-v1.
            let turn_creds = ice_servers
                .iter()
                .find_map(|s| match (&s.username, &s.credential) {
                    (Some(u), Some(c)) if relay::turn_udp_server(&s.urls).is_some() => {
                        Some((s.urls.clone(), u.clone(), c.clone()))
                    }
                    _ => None,
                });

            let peer_result = if let Some((urls, user, cred)) = turn_creds {
                match relay::allocate_relay_from_ice(&urls, &user, &cred).await {
                    Ok(turn_relay) => {
                        let relay_conn: Arc<dyn relay::RelayConn> = Arc::new(turn_relay);
                        crate::tunnel::quic_peer::AgentQuicPeer::setup_over_relay(
                            session_id,
                            quic_auth_token,
                            relay_conn,
                        )
                    }
                    Err(e) => {
                        warn!(%session_id, %e, "tunnel quic: TURN allocate failed — no QUIC relay this session");
                        return Ok(());
                    }
                }
            } else {
                let bind = match "0.0.0.0:0".parse() {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(%session_id, %e, "tunnel quic: bad bind addr — skipping setup");
                        return Ok(());
                    }
                };
                crate::tunnel::quic_peer::AgentQuicPeer::setup(session_id, quic_auth_token, bind)
            };

            match peer_result {
                Ok(peer) => {
                    let ready = ClientMsg::TunnelQuicReady {
                        session_id,
                        cert_fingerprint: peer.cert_fingerprint().to_string(),
                        addrs: peer.addrs(),
                    };
                    tunnel_quic_peers.insert(session_id, Arc::new(peer));
                    let _ = outbound_tx.send(ready).await;
                    info!(%session_id, "agent QUIC peer ready; rc:tunnel.quic.ready sent");
                }
                Err(e) => {
                    warn!(%session_id, %e, "tunnel quic: AgentQuicPeer setup failed");
                }
            }
        }

        // rc:tunnel.quic.candidate — the tunnel-client's relay
        // address(es), relayed by the server. The agent is the QUIC
        // *server* and never sends first, so for each candidate we
        // install a TURN permission (one bootstrap datagram through our
        // own allocation) — without it coturn drops the client's opening
        // QUIC Initials. No-op for a direct (non-relay) peer. Phase 3d.
        ServerMsg::TunnelQuicCandidate { session_id, addrs } => {
            if let Some(peer) = tunnel_quic_peers.get(&session_id) {
                for a in &addrs {
                    match a.parse::<std::net::SocketAddr>() {
                        Ok(sa) => {
                            if let Err(e) = peer.permit(sa).await {
                                debug!(%session_id, addr = %a, %e, "tunnel quic: permit failed");
                            }
                        }
                        Err(e) => {
                            debug!(%session_id, addr = %a, %e, "tunnel quic: unparseable candidate addr")
                        }
                    }
                }
            } else {
                debug!(%session_id, "tunnel quic candidate for unknown session — dropping");
            }
        }

        // rc:tunnel.ice — trickle one ICE candidate into the agent's
        // tunnel peer. Drop silently if the peer is gone (e.g. peer
        // already torn down by a `TunnelTerminate`).
        ServerMsg::TunnelIce {
            session_id,
            candidate,
        } => {
            if let Some(peer) = tunnel_peers.get(&session_id) {
                if let Err(e) = peer.add_remote_ice(candidate).await {
                    debug!(%session_id, %e, "tunnel add_remote_ice failed");
                }
            } else {
                debug!(%session_id, "tunnel ICE for unknown session — dropping");
            }
        }

        // rc:tunnel.terminate from the server (relayed from the
        // client or admin-side teardown). Tear down our peer state.
        ServerMsg::TunnelTerminate { session_id, reason } => {
            info!(%session_id, ?reason, "rc:tunnel.terminate — closing peer");
            if let Some(peer) = tunnel_peers.remove(&session_id) {
                peer.close().await;
            }
            // The session may instead be on the QUIC data plane.
            // `AgentQuicPeer::close` is synchronous (aborts the accept
            // task; the endpoint drops with the last Arc).
            if let Some(quic_peer) = tunnel_quic_peers.remove(&session_id) {
                quic_peer.close();
            }
        }

        // Remaining tunnel-flow `ServerMsg` variants
        // (TunnelOpened / TcpForwardAccept / TcpForwardReject /
        // TcpHalfClose / TcpClosed / TunnelRevoked) target the
        // browser-side tunnel-client, not the agent. Catch-all +
        // debug log so a misrouted message is visible but doesn't
        // trip a "non-exhaustive match" build error if the variants
        // change shape later.
        //
        // `#[allow(unreachable_patterns)]` because in a checkout where
        // the tunnel `ServerMsg` variants haven't landed yet (e.g.
        // master before the T2 wire types merge), the explicit arms
        // above already cover every variant and clippy flags this
        // arm as dead. The allow makes the same source compile both
        // before and after the variants land. See CLAUDE.md
        // "Defensive enum catch-alls" rule.
        #[allow(unreachable_patterns)]
        other => {
            debug!(
                ?other,
                "tunnel-side ServerMsg routed to agent signaling — ignoring"
            );
        }
    }
    Ok(())
}

async fn close_all_peers(
    peers: &mut HashMap<bson::oid::ObjectId, AgentPeer>,
    indicator: &ViewerIndicator,
) {
    // rc.24 — also hide the viewer-indicator overlay for every
    // session being torn down. Previously the indicator only
    // hid on receipt of `rc:terminate`, which never fires when
    // the WS itself drops (e.g. server pod recreate, network
    // blip). Field repro 2026-05-13 on the field-test host: after a roomler.ai
    // web deploy, the red "Being viewed by gjovanov" frame stayed
    // painted on the host indefinitely + the operator couldn't
    // reconnect ("agent capacity exceeded") until the agent
    // service was restarted manually. By hiding the overlay here
    // the next session can reconnect with a clean slate.
    if peers.is_empty() {
        return;
    }
    let count = peers.len();
    for (session_id, peer) in peers.drain() {
        indicator.hide_session(session_id.to_hex());
        peer.close().await;
    }
    info!(
        count,
        "torn down peers + hid indicator overlays on ws disconnect"
    );
}

/// T2.10d: tear down every tunnel peer on WS disconnect. Cheap
/// no-op when the map is empty (normal for agents that never serve
/// a tunnel).
async fn close_all_tunnel_peers(
    tunnel_peers: &mut HashMap<bson::oid::ObjectId, Arc<crate::tunnel::peer::AgentTunnelPeer>>,
) {
    if tunnel_peers.is_empty() {
        return;
    }
    let count = tunnel_peers.len();
    for (_, peer) in tunnel_peers.drain() {
        peer.close().await;
    }
    info!(count, "torn down agent tunnel peers on ws disconnect");
}

/// Phase 1d (quic-v1): tear down every QUIC tunnel peer on WS
/// disconnect. `AgentQuicPeer::close` is synchronous (aborts the
/// accept task; the quinn endpoint drops with the last `Arc`), so
/// unlike [`close_all_tunnel_peers`] there's no per-peer `.await`.
/// Cheap no-op when the map is empty (normal for non-QUIC agents).
async fn close_all_tunnel_quic_peers(
    tunnel_quic_peers: &mut HashMap<
        bson::oid::ObjectId,
        Arc<crate::tunnel::quic_peer::AgentQuicPeer>,
    >,
) {
    if tunnel_quic_peers.is_empty() {
        return;
    }
    let count = tunnel_quic_peers.len();
    for (_, peer) in tunnel_quic_peers.drain() {
        peer.close();
    }
    info!(count, "torn down agent QUIC tunnel peers on ws disconnect");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_backoff_ladder_pins_each_step() {
        // Step 1 covers both 0 and 1 because the counter is bumped
        // *before* the lookup; first failure passes 1 in.
        assert_eq!(auth_backoff_for(0), Duration::from_secs(30));
        assert_eq!(auth_backoff_for(1), Duration::from_secs(30));
        assert_eq!(auth_backoff_for(2), Duration::from_secs(60));
        assert_eq!(auth_backoff_for(3), Duration::from_secs(5 * 60));
        assert_eq!(auth_backoff_for(4), Duration::from_secs(60 * 60));
        assert_eq!(auth_backoff_for(99), Duration::from_secs(60 * 60));
    }

    #[test]
    fn auth_backoff_is_monotonic_non_decreasing() {
        // Fleet stability: a regression that swapped two ladder
        // entries (e.g. 5min + 1h) would silently flap agents.
        let mut last = Duration::ZERO;
        for n in 1..=10u32 {
            let d = auth_backoff_for(n);
            assert!(
                d >= last,
                "ladder must be monotonic non-decreasing; failed at n={n}"
            );
            last = d;
        }
    }

    // ─── rc.58 regression tests ──────────────────────────────────────────────

    #[test]
    fn ws_connect_timeout_is_long_enough_for_healthy_handshake() {
        // A healthy WSS handshake is <1 s typical; the bound exists
        // to catch hangs, not to clip latency. 30 s gives 30× headroom
        // and matches the field-tested value from the rc.58 fix.
        // A regression dropping this below ~5 s would start clipping
        // legitimate slow handshakes (e.g. cold-cache TLS to a far-
        // geo LB) and produce a fresh round of false-positive ws-
        // connect-timeout warnings.
        assert!(
            WS_CONNECT_TIMEOUT >= Duration::from_secs(10),
            "WS_CONNECT_TIMEOUT must give legitimate handshakes room; \
             current={WS_CONNECT_TIMEOUT:?}"
        );
    }

    #[test]
    fn error_chain_walks_anyhow_context_layers() {
        // The whole point of this helper is to surface root causes
        // that `Display` hides. Pin the format so a future refactor
        // (e.g. swap colon-join for newline-join) doesn't silently
        // change field log shape that operators grep.
        let inner = std::io::Error::other("ECONNREFUSED");
        let middle = anyhow::Error::new(inner).context("tls handshake");
        let outer = middle.context("ws connect");
        let chain = error_chain(outer.as_ref());
        // Each layer present and ordered outer→inner.
        assert!(
            chain.starts_with("ws connect"),
            "outer must lead the chain; got: {chain}"
        );
        assert!(
            chain.contains("tls handshake"),
            "middle layer missing; got: {chain}"
        );
        assert!(
            chain.contains("ECONNREFUSED"),
            "root cause missing; got: {chain}"
        );
        assert!(
            chain.matches(": ").count() >= 2,
            "expected at least two layer separators; got: {chain}"
        );
    }

    #[test]
    fn error_chain_handles_single_layer_error() {
        // A bare error with no `.source()` chain must round-trip its
        // own message — the helper shouldn't panic or emit empty.
        let bare = std::io::Error::other("simple");
        let chain = error_chain(&bare);
        assert_eq!(chain, "simple");
    }
}
