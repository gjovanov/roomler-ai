//! P4 route-event feed — since netstate (2026-08-16), a COMPAT SHIM.
//!
//! The OS subscriptions (NotifyRouteChange2 / NotifyUnicastIpAddressChange /
//! NotifyIpInterfaceChange on Windows; `ip -o monitor route` elsewhere) now
//! live in [`super::netstate`], registered ONCE per process instead of once
//! per org runtime. This module keeps the legacy consumer contract the
//! runtime's route-event select arm was built on — `spawn_route_watch()` →
//! [`RouteWatch::recv`] yielding class-prefixed strings ("addr …" /
//! "iface …" / route lines) — by subscribing to netstate's delta broadcast
//! and rendering one line per (already-debounced) delta. PR-2 replaces the
//! runtime arm with typed [`super::netstate::NetDelta`] consumption and
//! deletes this shim.
//!
//! Behaviour deltas vs the raw feed, both deliberate:
//! * Events arrive one debounce window (~750 ms) after the first OS signal
//!   instead of milliseconds — still far inside the route-guard's demoted
//!   30 s heartbeat, and the netstate monitor publishes IMMATERIAL bursts
//!   too (an erased peer `/32` is snapshot-invisible), so the P4 "re-assert
//!   on erase" contract holds.
//! * A `Lagged` subscriber (only possible if the runtime stalls) receives a
//!   conservative "addr"-classed line so every accelerated reaction fires on
//!   catch-up.
//!
//! Kill-switches compose: `ROOMLER_NODE_OVERLAY_ROUTE_EVENTS=0` disables
//! this consumer (tick-only route guard, the pre-P4 behaviour);
//! `ROOMLER_NODE_OVERLAY_NETMON=0` disables the whole subsystem, which
//! implies the same.

use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tracing::debug;

/// Minimum spacing between event-driven re-assert waves (consumed by the
/// runtime's route-event arm; unchanged).
pub(crate) const ROUTE_WAVE_MIN_INTERVAL: Duration = Duration::from_secs(3);

/// `ROOMLER_NODE_OVERLAY_ROUTE_EVENTS` — default ON; `0`/`false`/`off`
/// disables this legacy consumer entirely (tick-only route guard).
pub(crate) fn route_events_enabled() -> bool {
    !matches!(
        crate::env::node_env("OVERLAY_ROUTE_EVENTS")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// A live network-change feed: `recv().await` yields one short line per
/// debounced change burst. Dropping it detaches from netstate (the OS
/// subscription itself is process-wide and stays).
pub(crate) struct RouteWatch {
    rx: mpsc::UnboundedReceiver<String>,
    _guard: WatchGuard,
}

struct WatchGuard(tokio::task::JoinHandle<()>);

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl RouteWatch {
    /// Next event; `None` when the feed died (netstate monitor stopped) —
    /// the runtime then falls back to tick-only.
    pub(crate) async fn recv(&mut self) -> Option<String> {
        self.rx.recv().await
    }

    /// Drain everything queued behind the first event; returns the count.
    pub(crate) fn drain(&mut self) -> usize {
        let mut n = 0;
        while self.rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }
}

/// Attach a legacy string feed to the process-wide netstate monitor.
/// `None` = disabled via either kill-switch, or netstate's OS backend
/// failed to register — the caller keeps the tick fallback either way.
pub(crate) fn spawn_route_watch() -> Option<RouteWatch> {
    if !route_events_enabled() {
        debug!(
            "overlay: route-event consumer disabled by env (ROOMLER_NODE_OVERLAY_ROUTE_EVENTS=0)"
        );
        return None;
    }
    let handle = super::netstate::handle()?;
    let mut deltas = handle.subscribe();
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            match deltas.recv().await {
                Ok(d) => {
                    // Class prefixes preserve the runtime arm's
                    // `starts_with("addr"|"iface")` accelerated branches.
                    let line = if d.saw_addr_signal {
                        format!("addr {}", d.summary)
                    } else if d.saw_iface_signal {
                        format!("iface {}", d.summary)
                    } else {
                        d.summary
                    };
                    if tx.send(line).is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // Missed deltas may have included address changes —
                    // conservative class so every accelerated path fires.
                    if tx.send(format!("addr lagged={n}")).is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Some(RouteWatch {
        rx,
        _guard: WatchGuard(task),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kill-switch parse (default ON; explicit 0/false/off only).
    #[test]
    fn env_kill_switch_parses() {
        for (v, want) in [
            (None, true),
            (Some("1"), true),
            (Some("shadow"), true),
            (Some("0"), false),
            (Some("false"), false),
            (Some("off"), false),
            (Some("OFF"), false),
        ] {
            let normalized = v.map(|s: &str| s.trim().to_ascii_lowercase());
            let enabled = !matches!(
                normalized.as_deref(),
                Some("0") | Some("false") | Some("off")
            );
            assert_eq!(enabled, want, "value {v:?}");
        }
    }

    /// The shim attaches to the process-wide monitor and tears down cleanly
    /// (no events required — generating one needs privileges CI doesn't
    /// have). On hosts without the platform facility spawn returns None,
    /// also a valid, non-crashing outcome.
    #[tokio::test]
    async fn route_watch_spawns_and_drops_cleanly() {
        let watch = spawn_route_watch();
        if let Some(mut w) = watch {
            assert_eq!(w.drain(), 0, "no synthetic events expected");
            drop(w);
        }
    }
}
