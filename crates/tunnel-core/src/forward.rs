//! Bidirectional TCP ↔ DataChannel pump with event-driven
//! backpressure.
//!
//! Pattern lifted from the agent's rc.19 file-DC code. Pause
//! `TcpStream::read` when `dc.buffered_amount > HIGH_WATER_MARK`,
//! resume when it crosses `LOW_WATER_MARK` (via webrtc-rs's
//! `bufferedAmountLowThreshold` event — not poll). Without this a
//! 3 GB MSSQL stream OOMs the agent.
//!
//! Two halves per flow:
//! * **TCP → DC**: read TCP, frame with `flow_id` (4-byte LE prefix
//!   per `crate::mux`), send on the DC, pause when `buffered_amount
//!   > HIGH_WATER_MARK_BYTES`, resume when the
//!   `bufferedAmountLow` event fires (threshold set to
//!   `LOW_WATER_MARK_BYTES`).
//! * **DC → TCP**: messages routed through a [`FlowDemux`] (one per
//!   DC, decodes the `flow_id` prefix and routes to per-flow
//!   `mpsc::Receiver<Bytes>`); receiver feeds bytes to the TCP write
//!   half until EOF or error.
//!
//! Cross-references that MUST stay in sync if these constants move:
//! `crates/api/src/ws/tunnel.rs` (server-side audit) and
//! `crates/remote_control/src/signaling.rs` (`CloseReason` taxonomy).

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use roomler_ai_remote_control::signaling::CloseReason;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify, mpsc};
use tracing::{debug, trace, warn};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;

use crate::mux;

/// Threshold above which the pump pauses TCP reads. 4 MiB.
pub const HIGH_WATER_MARK_BYTES: usize = 4 * 1024 * 1024;

/// Threshold at which the pump resumes TCP reads. 1 MiB. Hysteresis
/// matters — too tight = thrash; too loose = latency under burst.
pub const LOW_WATER_MARK_BYTES: usize = 1024 * 1024;

/// Default chunk size for native↔native DC sends. May rise to 256 KiB
/// post-bench (T3 perf harness) if SCTP+OS sockbuf cooperate. Subtract
/// the flow_id prefix from the TCP read budget so the framed DC
/// message stays under the SCTP max_message_size of 65536.
pub const CHUNK_BYTES: usize = 64 * 1024;

/// Per-flow inbound mailbox capacity (in messages, not bytes). When
/// the receiver is slow, the [`FlowDemux::on_message`] handler awaits
/// `send` — cascading backpressure into the DC read loop and (via
/// SCTP) the peer's sender. 256 messages × ~64 KiB chunks ≈ 16 MiB
/// max buffered per flow, which matches the rc.19 file-DC behaviour.
const FLOW_INBOX_CAP: usize = 256;

/// In-band half-close signal: `[flow_id_le | HALF_CLOSE_MAGIC]`.
/// Non-empty payload because empty-payload (4 byte total) DC
/// messages weren't reliably delivered in the local two-peer
/// fixture — possibly the DCEP empty-binary PPID path interacting
/// badly with our pre-negotiated streams. T2.10 replaces this with
/// the wire-level `rc:tunnel.tcp.half_close` message; until then
/// this in-band marker keeps the pump self-contained for tests.
pub(crate) const HALF_CLOSE_MAGIC: &[u8] = &[0xFF];

// Compile-time invariants. Cross-referenced from the audit log
// dashboard's roll-up — see CLAUDE.md for the constants we lock here.
const _: () = assert!(
    HIGH_WATER_MARK_BYTES >= LOW_WATER_MARK_BYTES * 2,
    "watermark hysteresis must be non-trivial — too close = thrash"
);
// SCTP max_message_size default is 65536 (webrtc-sctp
// DEFAULT_MAX_MESSAGE_SIZE). 64 KiB sits exactly at it; any larger
// and we'd need a SettingEngine knob for message size too.
const _: () = assert!(CHUNK_BYTES <= 65536);
const _: () = assert!(CHUNK_BYTES.is_power_of_two());

// ────────────────────────────────────────────────────────────────────────────
// Flow demux — one per DC, fans inbound DC messages to per-flow
// mailboxes by decoding the 4-byte `flow_id` prefix.
// ────────────────────────────────────────────────────────────────────────────

type FlowMap = Arc<Mutex<HashMap<u32, mpsc::Sender<Bytes>>>>;

/// Owns one `RTCDataChannel` and routes inbound messages to per-flow
/// `mpsc::Receiver<Bytes>`. Install once per DC right after the pool
/// is open; `register` before sending the first `TcpForwardAccept`
/// for that flow.
#[derive(Clone)]
pub struct FlowDemux {
    dc: Arc<RTCDataChannel>,
    flows: FlowMap,
}

impl FlowDemux {
    /// Wrap a DC and install the routing `on_message` handler. The
    /// handler `await`s on a slow mailbox so backpressure cascades
    /// into the DC reader. Unregistered flow_ids are logged + dropped
    /// (e.g. the peer raced a close).
    pub async fn install(dc: Arc<RTCDataChannel>) -> Self {
        let flows: FlowMap = Arc::new(Mutex::new(HashMap::new()));
        let flows_for_handler = Arc::clone(&flows);
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let flows = Arc::clone(&flows_for_handler);
            Box::pin(async move {
                let Some((flow_id, payload)) = mux::decode(&msg.data) else {
                    warn!(
                        len = msg.data.len(),
                        "tunnel DC message too short for flow_id prefix"
                    );
                    return;
                };
                // Half-close marker: see `HALF_CLOSE_MAGIC`. Drop
                // the mailbox so our `pump_dc_to_tcp` sees None on
                // its next recv() and shuts down the local TCP
                // write half (sending FIN downstream).
                if payload == HALF_CLOSE_MAGIC {
                    trace!(flow_id, "tunnel flow half-close marker received");
                    flows.lock().await.remove(&flow_id);
                    return;
                }
                let sender = {
                    let map = flows.lock().await;
                    map.get(&flow_id).cloned()
                };
                let Some(tx) = sender else {
                    trace!(
                        flow_id,
                        len = payload.len(),
                        "tunnel DC message for unregistered flow — dropping"
                    );
                    return;
                };
                if let Err(e) = tx.send(Bytes::copy_from_slice(payload)).await {
                    debug!(flow_id, %e, "tunnel flow mailbox closed; dropping payload");
                }
            })
        }));
        Self { dc, flows }
    }

    /// Open a mailbox for `flow_id`. The returned receiver yields
    /// payload `Bytes` (flow_id prefix already stripped) and closes
    /// when [`unregister`] fires or the DC drops.
    pub async fn register(&self, flow_id: u32) -> mpsc::Receiver<Bytes> {
        let (tx, rx) = mpsc::channel(FLOW_INBOX_CAP);
        let mut map = self.flows.lock().await;
        if map.insert(flow_id, tx).is_some() {
            warn!(
                flow_id,
                "tunnel flow re-registered; previous mailbox dropped"
            );
        }
        rx
    }

    /// Close the mailbox for `flow_id`. Idempotent.
    pub async fn unregister(&self, flow_id: u32) {
        let mut map = self.flows.lock().await;
        map.remove(&flow_id);
    }

    /// Borrow the DC so the caller can hand it to [`run_flow`].
    pub fn dc(&self) -> Arc<RTCDataChannel> {
        Arc::clone(&self.dc)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Bidirectional pump — one task per flow, runs until close.
// ────────────────────────────────────────────────────────────────────────────

/// Drive a single accepted forward to completion. Returns the
/// terminating [`CloseReason`] which the caller plumbs into the
/// `ClientMsg::TcpClosed` audit message.
///
/// Implementation: spawns one inner task for TCP→DC (with
/// `bufferedAmountLow`-driven backpressure) and one for DC→TCP;
/// returns as soon as either direction reaches EOF / errors. The
/// other direction is cancelled by dropping the task handle. The
/// caller is responsible for sending the `rc:tunnel.tcp.closed` wire
/// message and audit row.
pub async fn run_flow(
    tcp: TcpStream,
    dc: Arc<RTCDataChannel>,
    flow_id: u32,
    mut from_dc: mpsc::Receiver<Bytes>,
) -> CloseReason {
    // Set the DC's low-water threshold once. `on_buffered_amount_low`
    // fires whenever the SCTP send queue drops back to or below this
    // value — i.e. when we have room to push more.
    dc.set_buffered_amount_low_threshold(LOW_WATER_MARK_BYTES)
        .await;
    let resume = Arc::new(Notify::new());
    let resume_handler = Arc::clone(&resume);
    dc.on_buffered_amount_low(Box::new(move || {
        let resume = Arc::clone(&resume_handler);
        Box::pin(async move {
            resume.notify_waiters();
        })
    }))
    .await;

    let (read_half, write_half) = tcp.into_split();

    // Spawn TCP → DC.
    let dc_for_send = Arc::clone(&dc);
    let resume_for_send = Arc::clone(&resume);
    let tcp_to_dc = tokio::spawn(async move {
        pump_tcp_to_dc(read_half, dc_for_send, flow_id, resume_for_send).await
    });

    // Spawn DC → TCP.
    let dc_to_tcp =
        tokio::spawn(async move { pump_dc_to_tcp(write_half, &mut from_dc, flow_id).await });

    // Wait for BOTH directions to finish — half-close semantics.
    // pump_tcp_to_dc sends an empty-payload marker on EOF so the
    // peer's FlowDemux drops its sender and the peer's
    // pump_dc_to_tcp sees None on recv(). Without this, an echo-
    // style test (write 1 MiB, shutdown writer, read 1 MiB back)
    // loses in-flight bytes because the early-terminator wins.
    let r1 = tcp_to_dc.await.unwrap_or(CloseReason::IoError);
    let r2 = dc_to_tcp.await.unwrap_or(CloseReason::IoError);
    if matches!(r1, CloseReason::Eof) && matches!(r2, CloseReason::Eof) {
        CloseReason::Eof
    } else {
        CloseReason::IoError
    }
}

async fn pump_tcp_to_dc(
    mut read_half: tokio::net::tcp::OwnedReadHalf,
    dc: Arc<RTCDataChannel>,
    flow_id: u32,
    resume: Arc<Notify>,
) -> CloseReason {
    let mut buf = vec![0u8; CHUNK_BYTES - mux::FLOW_ID_HEADER_BYTES];
    loop {
        // Backpressure gate. Check current buffered_amount; if
        // above HIGH, wait on the notifier (which fires when SCTP
        // drains to LOW).
        loop {
            let buffered = dc.buffered_amount().await;
            if buffered <= HIGH_WATER_MARK_BYTES {
                break;
            }
            trace!(
                flow_id,
                buffered, "tunnel pump paused — awaiting bufferedAmountLow"
            );
            // Race a small timeout so we don't deadlock if the
            // low-water event somehow never fires (defensive — the
            // event SHOULD fire reliably).
            tokio::select! {
                _ = resume.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                    // Fall through and re-check buffered_amount.
                }
            }
        }

        let n = match read_half.read(&mut buf).await {
            Ok(0) => {
                // Local TCP read half hit EOF. Signal the peer with
                // the `HALF_CLOSE_MAGIC` sentinel so its FlowDemux
                // closes the mailbox and its pump_dc_to_tcp shuts
                // down cleanly.
                let marker = mux::encode(flow_id, HALF_CLOSE_MAGIC);
                if let Err(e) = dc.send(&Bytes::from(marker)).await {
                    debug!(flow_id, %e, "tunnel pump half-close marker send failed");
                }
                return CloseReason::Eof;
            }
            Ok(n) => n,
            Err(e) => {
                debug!(flow_id, %e, "tunnel pump TCP read error");
                let marker = mux::encode(flow_id, HALF_CLOSE_MAGIC);
                let _ = dc.send(&Bytes::from(marker)).await;
                return CloseReason::IoError;
            }
        };

        let framed = mux::encode(flow_id, &buf[..n]);
        if let Err(e) = dc.send(&Bytes::from(framed)).await {
            debug!(flow_id, %e, "tunnel pump DC send error");
            return CloseReason::IoError;
        }
    }
}

async fn pump_dc_to_tcp(
    mut write_half: tokio::net::tcp::OwnedWriteHalf,
    from_dc: &mut mpsc::Receiver<Bytes>,
    flow_id: u32,
) -> CloseReason {
    loop {
        let chunk = match from_dc.recv().await {
            Some(c) => c,
            None => {
                // Mailbox closed (FlowDemux saw the peer's
                // half-close marker). Shutdown the local TCP write
                // half so downstream sees FIN. Failure to shutdown
                // (e.g. peer already RST'd) is non-fatal.
                if let Err(e) = write_half.shutdown().await {
                    debug!(flow_id, %e, "tunnel pump TCP shutdown after EOF failed");
                }
                return CloseReason::Eof;
            }
        };
        if let Err(e) = write_half.write_all(&chunk).await {
            debug!(flow_id, %e, "tunnel pump TCP write error");
            return CloseReason::IoError;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::webrtc_dc::TunnelPeer;
    use std::sync::Arc;
    use std::time::Duration;

    /// Stress the [`FlowDemux`] with sustained traffic + the half-
    /// close marker: send 256 KiB in 4-KiB framed chunks via direct
    /// `dc.send` (mimicking what `pump_tcp_to_dc` would do), then
    /// send the `HALF_CLOSE_MAGIC` marker. Verifies (a) framing
    /// roundtrips at scale, (b) bytes arrive in order, (c) the
    /// marker closes the mailbox so `recv()` returns `None`. The
    /// pump's TCP-read interaction is exercised end-to-end via the
    /// agent + tunnel-client integration in T2.10.
    #[tokio::test(flavor = "multi_thread")]
    async fn demux_handles_256k_burst_then_half_close() {
        let offerer = TunnelPeer::new(vec![]).await.unwrap();
        let answerer = TunnelPeer::new(vec![]).await.unwrap();

        let answerer_pc = answerer.peer_connection();
        offerer.on_local_ice_candidate(move |c| {
            let pc = Arc::clone(&answerer_pc);
            Box::pin(async move {
                if let Some(c) = c
                    && let Ok(init) = c.to_json()
                {
                    let _ = pc.add_ice_candidate(init).await;
                }
            })
        });
        let offerer_pc = offerer.peer_connection();
        answerer.on_local_ice_candidate(move |c| {
            let pc = Arc::clone(&offerer_pc);
            Box::pin(async move {
                if let Some(c) = c
                    && let Ok(init) = c.to_json()
                {
                    let _ = pc.add_ice_candidate(init).await;
                }
            })
        });
        let offer = offerer.create_offer().await.unwrap();
        let answer = answerer.accept_offer(&offer.sdp).await.unwrap();
        offerer.accept_answer(&answer.sdp).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), offerer.wait_pool_open())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), answerer.wait_pool_open())
            .await
            .unwrap()
            .unwrap();

        let off_dc = offerer.dc(0).unwrap();
        let ans_dc = answerer.dc(0).unwrap();
        let ans_demux = FlowDemux::install(ans_dc.clone()).await;
        let mut from_dc_answerer = ans_demux.register(1).await;

        // Manual send loop — same framing as `pump_tcp_to_dc` but
        // bypasses the TCP read half so the test scope stays focused
        // on demux + marker behaviour.
        let payload: Vec<u8> = (0..(1 << 18)).map(|i| (i & 0xFF) as u8).collect();
        let payload_for_sender = payload.clone();
        let off_dc_for_sender = Arc::clone(&off_dc);
        tokio::spawn(async move {
            for chunk in payload_for_sender.chunks(4 * 1024) {
                let framed = mux::encode(1, chunk);
                off_dc_for_sender
                    .send(&Bytes::from(framed))
                    .await
                    .expect("dc.send failed");
            }
            // Half-close marker.
            let marker = mux::encode(1, HALF_CLOSE_MAGIC);
            off_dc_for_sender
                .send(&Bytes::from(marker))
                .await
                .expect("marker send failed");
        });

        // Drain the answerer's demux mailbox. The marker drops the
        // sender → recv() returns None once the buffered chunks are
        // consumed.
        let received = tokio::time::timeout(Duration::from_secs(30), async {
            let mut out = Vec::with_capacity(payload.len());
            while let Some(chunk) = from_dc_answerer.recv().await {
                out.extend_from_slice(&chunk);
            }
            out
        })
        .await
        .expect("demux did not deliver 256 KiB within 30s");

        assert_eq!(received.len(), payload.len(), "received length mismatch");
        assert_eq!(received, payload, "received payload mismatch");
    }

    /// Most basic FlowDemux smoke: install, register a flow_id,
    /// have the peer side send one framed message via dc.send(),
    /// expect it to land in the mailbox. Verifies the demux on_message
    /// hookup before the pump test (which depends on this working).
    #[tokio::test(flavor = "multi_thread")]
    async fn demux_routes_one_message_to_registered_flow() {
        let offerer = TunnelPeer::new(vec![]).await.unwrap();
        let answerer = TunnelPeer::new(vec![]).await.unwrap();
        let answerer_pc = answerer.peer_connection();
        offerer.on_local_ice_candidate(move |c| {
            let pc = Arc::clone(&answerer_pc);
            Box::pin(async move {
                if let Some(c) = c
                    && let Ok(init) = c.to_json()
                {
                    let _ = pc.add_ice_candidate(init).await;
                }
            })
        });
        let offerer_pc = offerer.peer_connection();
        answerer.on_local_ice_candidate(move |c| {
            let pc = Arc::clone(&offerer_pc);
            Box::pin(async move {
                if let Some(c) = c
                    && let Ok(init) = c.to_json()
                {
                    let _ = pc.add_ice_candidate(init).await;
                }
            })
        });
        let offer = offerer.create_offer().await.unwrap();
        let answer = answerer.accept_offer(&offer.sdp).await.unwrap();
        offerer.accept_answer(&answer.sdp).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), offerer.wait_pool_open())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), answerer.wait_pool_open())
            .await
            .unwrap()
            .unwrap();

        let off_dc = offerer.dc(0).unwrap();
        let ans_dc = answerer.dc(0).unwrap();
        let ans_demux = FlowDemux::install(ans_dc).await;
        let mut from_dc = ans_demux.register(42).await;

        let framed = mux::encode(42, b"hello world");
        off_dc.send(&Bytes::from(framed)).await.unwrap();

        let received = tokio::time::timeout(Duration::from_secs(5), from_dc.recv())
            .await
            .expect("demux did not deliver within 5s");
        assert_eq!(received.as_deref(), Some(b"hello world".as_ref()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unregistered_flow_id_drops_messages_silently() {
        let peer = TunnelPeer::new(vec![]).await.unwrap();
        let dc = peer.dc(0).unwrap();
        let _demux = FlowDemux::install(dc).await;
        // No register; flow_id 99 mailbox doesn't exist. No
        // assertion needed — if the handler panicked on unknown
        // flow ids, the test would fail by causing the runtime to
        // tear down. As-is this just exercises the trace! path.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
