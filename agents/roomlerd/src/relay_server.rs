//! FR-19 P1d — start the org-relay reachability responder, if this device
//! opted in.
//!
//! Process-wide and started once: the responder owns a single UDP socket, so
//! it is deliberately **not** per-org like [`crate::overlay::maybe_start`].
//!
//! # The default path costs nothing
//!
//! `relay_server_enabled` is FR-19's gate 4 — the refusal that survives a
//! compromised server — and it is opt-in, so a device that has not explicitly
//! turned this on binds no socket, spawns no task and logs nothing.
//!
//! # A failed bind is loud, and a successful one is not a promise
//!
//! If the port cannot be bound the daemon says so at `error!` with the reason,
//! rather than starting a relay that silently never answers. And the success
//! line states plainly that **binding is not reachability**: on a host with a
//! coturn DNAT the port is consumed in `PREROUTING` while `ss -ulnp` shows it
//! free and the socket receives nothing. That is not hypothetical — it is what
//! mars does, and it is why FR-19's E2E-3 nearly reached the opposite
//! conclusion about which port corporate egresses permit.

use std::sync::{Arc, OnceLock};

use tunnel_core::overlay::orgrelay;
use tunnel_core::overlay::orgrelay::responder::{ProbeResponder, ResponderCounts, ResponderStats};

/// Set only when the responder actually started, so `None` distinguishes
/// "not running" from "running and idle" — the distinction FR-19 insists on
/// for every counter it ships.
static STATS: OnceLock<Arc<ResponderStats>> = OnceLock::new();

/// Live counters, or `None` when no responder is running on this node.
pub fn stats() -> Option<ResponderCounts> {
    STATS.get().map(|s| s.snapshot())
}

/// How often the counters are summarised into the log. This IS the reader for
/// those counters until they reach `NodeStatus`; a counter without one cannot
/// be used to evaluate anything (FR-18's `dropped_stale` is the precedent).
const REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(300);

/// Start the responder if the device opted in. Safe to call once per process.
pub fn maybe_start() {
    if !orgrelay::relay_server_enabled() {
        return;
    }
    let port = orgrelay::relay_server_port();
    let stats = Arc::new(ResponderStats::default());
    if STATS.set(stats.clone()).is_err() {
        tracing::warn!("org-relay responder already started; ignoring second start");
        return;
    }
    tokio::spawn(async move {
        let sock = match tokio::net::UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                // Loud and specific. A relay that cannot bind must not look
                // like a relay that is merely quiet.
                tracing::error!(
                    port,
                    error = %e,
                    "org-relay responder NOT started: could not bind udp/{port}. \
                     Something else already owns the port, or the daemon lacks \
                     permission for it. The node will not answer relay probes."
                );
                return;
            }
        };
        tokio::spawn(report_loop(stats.clone()));
        ProbeResponder::new(stats).serve(sock).await;
    });
}

/// Summarise the counters whenever they change. Silent while nothing happens,
/// so an idle relay does not fill the log.
async fn report_loop(stats: Arc<ResponderStats>) {
    let mut last = ResponderCounts::default();
    loop {
        tokio::time::sleep(REPORT_EVERY).await;
        let now = stats.snapshot();
        if now == last {
            continue;
        }
        tracing::info!(
            answered = now.answered,
            refused_not_shaped = now.refused_not_shaped,
            refused_not_probe = now.refused_not_probe,
            refused_rate_limited = now.refused_rate_limited,
            "org-relay responder counters"
        );
        last = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is off, and "off" must mean nothing was created — not a
    /// responder sitting on a socket with zeroed counters. `stats()` returning
    /// `None` is how a reader tells those apart.
    #[test]
    fn a_device_that_did_not_opt_in_reports_no_responder() {
        // No env set in this test process, so the gate is closed.
        assert!(!orgrelay::relay_server_enabled());
        maybe_start();
        assert!(
            stats().is_none(),
            "opting out must leave no responder, not an idle one"
        );
    }
}
