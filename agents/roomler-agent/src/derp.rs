// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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

use crate::relay_probe::DerpTicketSlot;
use tunnel_core::transport::derp::DerpMux;

/// Hard upper bound on a single `/derp` connect attempt (mirrors the control
/// WS's `WS_CONNECT_TIMEOUT`): a hung TLS handshake must not stall the backoff.
const DERP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// FR-18 — ceiling for the `/derp` TCP socket's send buffer. Sized well above
/// the bandwidth-delay product of the paths this carrier actually serves
/// (3 Mbps × 200 ms ≈ 75 KB) so throughput is untouched, and far below the
/// megabytes autotuning would otherwise allow to accumulate as latency.
const DERP_SNDBUF_BYTES: usize = 256 * 1024;

/// Reconnect backoff ceiling. Deliberately LOW: the `/derp` WS is the relay
/// FLOOR — with a force-DERP pin active the pinned pair has no other tier, and
/// (post-#497) its build is withheld for exactly as long as this mux is down.
/// At the old 60 s ceiling a VPN capture pushed attempts to ~90 s spacing
/// (30 s connect timeout + 60 s backoff), so the floor could lag the network's
/// recovery by minutes (field 2026-08-16: multi-minute dark windows on pinned
/// pairs through a Check Point cycle). One TLS dial per 10 s while the path is
/// actually broken is noise; a fast floor restoration is the whole point.
const DERP_BACKOFF_MAX: Duration = Duration::from_secs(10);
/// Keepalive Ping cadence on the established `/derp` WS. An idle DERP socket
/// is legitimately silent (frames only flow while a both-UDP-blocked pair is
/// relaying), so WITHOUT our own pings the RX deadline below would false-fire
/// on healthy idle links. The server auto-pongs, giving a healthy link an
/// inbound frame at least this often.
const DERP_KEEPALIVE: Duration = Duration::from_secs(25);
/// Receive-liveness deadline (3 keepalive periods + margin). Same rationale
/// as `signaling::WS_RX_DEADLINE`: a TLS-inspecting corp middlebox keeps
/// ACKing our pings after its upstream leg died, so send-success proves
/// nothing — reconnect must key on frames RECEIVED.
const DERP_RX_DEADLINE: Duration = Duration::from_secs(80);

/// FR-18 — request the [`DERP_SNDBUF_BYTES`] ceiling on the socket under the
/// (possibly TLS-wrapped) WebSocket. Best-effort by design: an OS may clamp or
/// ignore the request, and a carrier that works with a big buffer must not fail
/// to start because we could not shrink it.
fn cap_send_buffer(
    ws: &tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    relay: &str,
) {
    use socket2::SockRef;
    use tokio_tungstenite::MaybeTlsStream;
    let tcp = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => s.get_ref().0,
        // `MaybeTlsStream` is `#[non_exhaustive]`; an unknown wrapper just
        // keeps the OS default rather than blocking the connection.
        _ => {
            debug!(%relay, "overlay derp: unrecognised TLS wrapper — send buffer left at the OS default");
            return;
        }
    };
    if let Err(e) = SockRef::from(tcp).set_send_buffer_size(DERP_SNDBUF_BYTES) {
        debug!(%relay, %e, "overlay derp: set_send_buffer_size failed — OS default retained");
    }
}

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
    tenant_id: &str,
    mux: &Arc<DerpMux>,
    outbound_rx: mpsc::Receiver<tunnel_core::transport::derp::OutboundFrame>,
) {
    // S6 — `tid` = tenant-affinity key for the front LB. DERP relays are
    // pod-local, so both mesh peers must land on the SAME pod; hashing
    // every /derp socket by tenant guarantees it. Pre-S6 servers ignore
    // the param.
    let full_url = format!(
        "{}?token={}&role=agent&tid={}",
        derp_url_from_ws(ws_url),
        crate::signaling::urlencode(agent_token),
        crate::signaling::urlencode(tenant_id)
    );
    let reg_frame = mux.registration_frame();
    let weak = Arc::downgrade(mux);
    tokio::spawn(run(
        "central".to_string(),
        Box::new(move || Some(full_url.clone())),
        reg_frame,
        weak,
        outbound_rx,
    ));
}

/// Multi-region DERP: like [`spawn`] but for a REGIONAL relay authenticated by
/// an admission ticket. The URL is rebuilt per connect attempt from the ticket
/// slot, so a refreshed ticket takes effect on the next reconnect; while the
/// slot is empty the owner just backs off (ticket still in flight).
pub fn spawn_regional(
    derp_url: &str,
    ticket_slot: DerpTicketSlot,
    tenant_id: &str,
    mux: &Arc<DerpMux>,
    outbound_rx: mpsc::Receiver<tunnel_core::transport::derp::OutboundFrame>,
) {
    let base = derp_url.to_string();
    // Host part only — the ticket query string must never reach a log line.
    let label = derp_url
        .trim_start_matches("wss://")
        .trim_start_matches("ws://")
        .split(['/', '?'])
        .next()
        .unwrap_or(derp_url)
        .to_string();
    let tid = crate::signaling::urlencode(tenant_id);
    let reg_frame = mux.registration_frame();
    let weak = Arc::downgrade(mux);
    tokio::spawn(run(
        label,
        Box::new(move || {
            let ticket = ticket_slot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|(t, _)| t.clone())?;
            let sep = if base.contains('?') { '&' } else { '?' };
            Some(format!(
                "{base}{sep}ticket={}&tid={tid}",
                crate::signaling::urlencode(&ticket)
            ))
        }),
        reg_frame,
        weak,
        outbound_rx,
    ));
}

async fn run(
    relay: String,
    url_for_attempt: Box<dyn Fn() -> Option<String> + Send>,
    reg_frame: Vec<u8>,
    mux: Weak<DerpMux>,
    mut outbound_rx: mpsc::Receiver<tunnel_core::transport::derp::OutboundFrame>,
) {
    // FR-18 — resolved once: the queue-age bound is a deployment knob, not a
    // per-frame decision, and re-reading the env per frame would be the hot
    // path of the whole carrier.
    let max_queue_age = tunnel_core::transport::derp::queue_max_age();
    // FR-18 — last value reported, so the keepalive log is change-gated
    // rather than a line every 25 s on a carrier that shed nothing.
    let mut last_reported_stale: u64 = 0;
    let mut backoff = Duration::from_secs(1);
    loop {
        // The mux is gone (runtime torn down) ⇒ stop reconnecting.
        if mux.upgrade().is_none() {
            debug!(%relay, "overlay derp: mux dropped; stopping /derp WS owner");
            return;
        }
        // Regional relays: no admission ticket cached yet — back off and
        // retry (the signaling loop fills the slot within seconds).
        let Some(url) = url_for_attempt() else {
            debug!(%relay, "overlay derp: no admission ticket yet; delaying connect");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(DERP_BACKOFF_MAX);
            continue;
        };
        match tokio::time::timeout(DERP_CONNECT_TIMEOUT, connect_async(&url)).await {
            Ok(Ok((ws, _))) => {
                backoff = Duration::from_secs(1);
                // FR-18 — bound the KERNEL's send buffer too. Shedding stale
                // frames above is pointless if a frame we did choose to send
                // then sits for seconds in a socket buffer the age check can no
                // longer reach: Windows autotuning happily grows this into the
                // megabytes, which at a 3 Mbps relay is multiple seconds of
                // committed latency. The BDP of these paths is tens of KB
                // (3 Mbps × 200 ms ≈ 75 KB), so this cap costs no throughput.
                cap_send_buffer(&ws, &relay);
                let (mut tx, mut rx) = ws.split();
                // Register our pubkey as the first frame.
                if tx
                    .send(Message::Binary(reg_frame.clone().into()))
                    .await
                    .is_err()
                {
                    warn!(%relay, "overlay derp: registration send failed; reconnecting");
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
                info!(%relay, "overlay derp: /derp WS connected + registered");

                let mut keepalive = tokio::time::interval(DERP_KEEPALIVE);
                keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                keepalive.tick().await; // swallow the immediate first tick
                let mut last_rx = std::time::Instant::now();
                // A5 — resume detection (see signaling.rs RESUME_SKEW_MIN):
                // monotonic timers exclude suspend, so a nap leaves a dead
                // socket trusted for up to another RX-deadline window.
                let mut skew_wall = std::time::SystemTime::now();
                let mut skew_mono = std::time::Instant::now();

                // Pump until either half dies (or the mux is torn down).
                loop {
                    tokio::select! {
                        _ = keepalive.tick() => {
                            let wall_gap = skew_wall.elapsed().unwrap_or_default();
                            let mono_gap = skew_mono.elapsed();
                            skew_wall = std::time::SystemTime::now();
                            skew_mono = std::time::Instant::now();
                            if wall_gap > mono_gap + std::time::Duration::from_secs(120) {
                                warn!(
                                    %relay,
                                    napped_s = (wall_gap - mono_gap).as_secs(),
                                    "overlay derp: suspend/resume detected — cycling the /derp WS"
                                );
                                break;
                            }
                            if last_rx.elapsed() > DERP_RX_DEADLINE {
                                warn!(
                                    %relay,
                                    silent_s = last_rx.elapsed().as_secs(),
                                    "overlay derp: no inbound frames within the RX deadline — half-open /derp WS; reconnecting"
                                );
                                break;
                            }
                            // FR-18 — report the shed count on the keepalive
                            // tick, change-gated so a healthy carrier stays
                            // silent. Without a reader the counter proves
                            // nothing in the field, and "the carrier shed
                            // stale load" has to be distinguishable from "the
                            // carrier stalled" — they look identical from the
                            // pump's side and want opposite responses.
                            if let Some(m) = mux.upgrade() {
                                let shed = m.stale_drops();
                                if shed != last_reported_stale {
                                    info!(
                                        %relay,
                                        dropped_stale = shed,
                                        since_last = shed - last_reported_stale,
                                        "overlay derp: shed stale carrier frames (queue age bound)"
                                    );
                                    last_reported_stale = shed;
                                }
                            }
                            if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                                warn!(%relay, "overlay derp: keepalive ping failed; reconnecting");
                                break;
                            }
                        }
                        out = outbound_rx.recv() => match out {
                            // A carrier frame to relay: [peer_pubkey||payload].
                            Some((queued_at, frame)) => {
                                // FR-18 — shed stale load. A frame that has
                                // already waited longer than its useful life is
                                // worse than nothing: it occupies the wire the
                                // CURRENT frame needs, and by the time it lands
                                // the receiver has moved on. Dropping here is
                                // also what turns the bounded-but-deep queue
                                // into drop-OLDEST semantics — under overload
                                // the writer discards the stale head quickly and
                                // reaches fresh frames, instead of faithfully
                                // delivering 1.8 s of history.
                                //
                                // WG is loss-tolerant by construction, so this
                                // costs a retransmit at worst; the alternative
                                // costs seconds of latency (measured: a single
                                // frame blocked 10.2 s inside the DC send).
                                if let Some(max_age) = max_queue_age
                                    && queued_at.elapsed() > max_age
                                {
                                    if let Some(m) = mux.upgrade() {
                                        m.note_stale_drop();
                                    }
                                    continue;
                                }
                                if tx.send(Message::Binary(frame.into())).await.is_err() {
                                    break;
                                }
                            }
                            // All senders gone ⇒ the mux + every DerpConn dropped
                            // (runtime torn down): we're done for good.
                            None => {
                                debug!(%relay, "overlay derp: outbound closed; /derp WS owner exiting");
                                return;
                            }
                        },
                        inb = rx.next() => match inb {
                            Some(Ok(Message::Binary(data))) => {
                                last_rx = std::time::Instant::now();
                                match mux.upgrade() {
                                    Some(m) => m.deliver(&data[..]),
                                    None => return,
                                }
                            }
                            // Keep the corp middlebox happy; ignore text.
                            Some(Ok(Message::Ping(p))) => {
                                last_rx = std::time::Instant::now();
                                let _ = tx.send(Message::Pong(p)).await;
                            }
                            // Pong (auto-reply to our keepalive) et al. — any
                            // inbound frame proves the link is alive.
                            Some(Ok(_)) => {
                                last_rx = std::time::Instant::now();
                            }
                            Some(Err(_)) | None => break,
                        },
                    }
                }
                if let Some(m) = mux.upgrade() {
                    m.mark_down();
                }
                warn!(%relay, "overlay derp: /derp WS closed; reconnecting");
            }
            Ok(Err(e)) => warn!(%relay, %e, "overlay derp: /derp connect failed"),
            Err(_) => warn!(%relay, "overlay derp: /derp connect timed out"),
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
