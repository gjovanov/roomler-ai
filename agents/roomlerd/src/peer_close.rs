// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3716 — a session's peer close runs to its end, whoever stops waiting.
//!
//! A close site waits for a peer's close at most
//! [`PEER_CLOSE_BUDGET`](crate::signaling::PEER_CLOSE_BUDGET)
//! ([`close_within_budget`](crate::signaling::close_within_budget)), and
//! before #3716 a close that overran was dropped where it stood. Since
//! `RTCPeerConnection::close` marks the peer closed at its first step, no
//! later close could finish the job: the ICE agent, every socket and the SCTP
//! association it had not reached yet lived until the daemon exited (17 UDP
//! sockets a session in the field). The causes found are fixed where they live
//! (the vendored webrtc, webrtc-sctp and webrtc-ice); this is the net under
//! the ones not found yet.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use webrtc::peer_connection::RTCPeerConnection;

use crate::signaling::PEER_CLOSE_BUDGET;

/// How long a peer's close goes on in the background after its caller stopped
/// waiting at [`PEER_CLOSE_BUDGET`].
pub(crate) const PEER_CLOSE_CEILING: Duration = Duration::from_secs(60);

/// `pc.close()` on a task of its own, bounded by [`PEER_CLOSE_CEILING`]. A
/// caller that stops waiting no longer cuts the close short: it runs to the
/// end, and says so when that end came past the budget, in a panic, or never
/// came. Its own result goes back to a caller still waiting for it.
pub(crate) async fn close(
    pc: Arc<RTCPeerConnection>,
    session: bson::oid::ObjectId,
) -> Result<(), webrtc::Error> {
    let started = Instant::now();
    let close = tokio::spawn(async move {
        // A panic is caught here, not where the task is joined: a caller past
        // its budget has dropped the handle, and the panic reached stderr only.
        let closing = std::panic::AssertUnwindSafe(pc.close());
        let closing = futures::FutureExt::catch_unwind(closing);
        let result = tokio::time::timeout(PEER_CLOSE_CEILING, closing).await;
        let took_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(Ok(result)) => {
                if took_ms > PEER_CLOSE_BUDGET.as_millis() as u64 {
                    info!(
                        session = %session,
                        took_ms,
                        "peer close finished in the background, past its budget (#3716)"
                    );
                }
                result
            }
            // What the close had not reached yet stays alive, and a service
            // has no stderr to read the panic from.
            Ok(Err(payload)) => {
                let panic = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                warn!(
                    session = %session,
                    took_ms,
                    %panic,
                    "peer close panicked — what it did not reach stays alive (#3716)"
                );
                Err(webrtc::Error::new(format!(
                    "the peer's close panicked: {panic}"
                )))
            }
            Err(_) => {
                warn!(
                    session = %session,
                    ceiling_ms = PEER_CLOSE_CEILING.as_millis() as u64,
                    "peer close did not finish within its ceiling — what it did not reach stays alive (#3716)"
                );
                Ok(())
            }
        }
    });
    // A JoinError is the runtime going down, which takes the rest with it.
    close.await.unwrap_or(Ok(()))
}
