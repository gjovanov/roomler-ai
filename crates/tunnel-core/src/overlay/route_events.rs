//! P4 — event-driven route-guard input (consolidation invariant I4).
//!
//! The 2 s blind route re-assert (rc.146) exists because a corp full-tunnel
//! VPN (Check Point) keeps re-installing competing routes and a captured
//! route makes our `tx` flatline — so nothing traffic-gated can detect it.
//! The loop compensates for a missing SIGNAL; this module supplies the
//! signal: an OS route-table change subscription feeding a channel the
//! runtime selects on, so the re-assert fires within milliseconds of an
//! erase instead of up to 2 s later.
//!
//! P4 lands STAGED: the subscription + event-driven re-assert ship first
//! while the 2 s tick keeps running as the backstop; the tick demotes to a
//! 30 s heartbeat only after a fleet soak proves the subscription catches
//! every erase first (grep `route-table change` waves against route-loss
//! symptoms on the corp-VPN hosts). Kill-switch:
//! `ROOMLER_NODE_OVERLAY_ROUTE_EVENTS=0` → no subscription → exactly the
//! pre-P4 behaviour.
//!
//! ## Feedback loop, by construction
//!
//! Our OWN re-asserts change the route table, so every wave re-triggers the
//! subscription (on both OSes). The runtime therefore rate-limits waves
//! (≥ [`ROUTE_WAVE_MIN_INTERVAL`] apart, draining the burst in between) —
//! an erase landing inside the quiet window is caught by the still-running
//! 2 s tick, which is exactly why the tick stays until the demotion gate.
//!
//! ## Platform mechanics
//!
//! * **Windows**: `NotifyRouteChange2` (IP Helper — the same API family the
//!   rc.209 route ops use). The callback runs on a SYSTEM WORKER THREAD:
//!   it must never block or panic, so it formats a tiny description and
//!   pushes it through an unbounded (non-blocking) channel. Teardown via
//!   `CancelMibChangeNotify2` on drop — it blocks until in-flight callbacks
//!   finish, after which freeing the context box is safe. The registration
//!   MUST be cancelled per overlay-runtime rebuild (the agent's WS
//!   lifecycle rebuilds the runtime) or callbacks would accumulate.
//! * **Linux**: a spawned `ip -o monitor route` child (dependency-free,
//!   matches the existing shelled-`ip` route-management style; netlink
//!   route watching is unprivileged), one line = one event,
//!   `kill_on_drop` for teardown. On platforms without `ip` (macOS) the
//!   spawn fails gracefully → `None` → tick-only, today's behaviour.

use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Minimum spacing between event-driven re-assert waves. Absorbs both the
/// self-triggered feedback burst (our own delete+add per peer) and a VPN
/// connect's route storm (hundreds of installs in one gulp). Erases inside
/// the quiet window are the 2 s tick's job until the demotion gate.
pub(crate) const ROUTE_WAVE_MIN_INTERVAL: Duration = Duration::from_secs(3);

/// `ROOMLER_NODE_OVERLAY_ROUTE_EVENTS` — default ON; `0`/`false`/`off`
/// disables the subscription entirely (pre-P4 behaviour: 2 s tick only).
pub(crate) fn route_events_enabled() -> bool {
    !matches!(
        crate::env::node_env("OVERLAY_ROUTE_EVENTS")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// A live route-table subscription: `recv().await` yields one short
/// human-readable event description per OS notification. Dropping it tears
/// the platform subscription down.
pub(crate) struct RouteWatch {
    rx: mpsc::UnboundedReceiver<String>,
    _guard: imp::WatchGuard,
}

impl RouteWatch {
    /// Next event; `None` when the platform side died (the runtime then
    /// falls back to tick-only).
    pub(crate) async fn recv(&mut self) -> Option<String> {
        self.rx.recv().await
    }

    /// Drain everything queued (the burst behind the first event); returns
    /// how many were dropped on the floor.
    pub(crate) fn drain(&mut self) -> usize {
        let mut n = 0;
        while self.rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }
}

/// Start the platform route-table watch. `None` = disabled via env, or the
/// platform registration/spawn failed (logged) — the caller keeps the 2 s
/// tick either way.
pub(crate) fn spawn_route_watch() -> Option<RouteWatch> {
    if !route_events_enabled() {
        debug!(
            "overlay: route-event subscription disabled by env (ROOMLER_NODE_OVERLAY_ROUTE_EVENTS=0)"
        );
        return None;
    }
    let (tx, rx) = mpsc::unbounded_channel();
    match imp::spawn(tx) {
        Some(guard) => Some(RouteWatch { rx, _guard: guard }),
        None => {
            warn!(
                "overlay: route-event subscription unavailable — route guard stays tick-only (2 s)"
            );
            None
        }
    }
}

#[cfg(windows)]
mod imp {
    use core::ffi::c_void;

    use tokio::sync::mpsc::UnboundedSender;
    use windows_sys::Win32::Foundation::{HANDLE, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        CancelMibChangeNotify2, MIB_IPFORWARD_ROW2, MIB_NOTIFICATION_TYPE, NotifyRouteChange2,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC};

    struct Ctx {
        tx: UnboundedSender<String>,
    }

    pub(super) struct WatchGuard {
        handle: HANDLE,
        ctx: *mut Ctx,
    }
    // The raw pointers are only touched in Drop (after CancelMibChangeNotify2
    // returns, no callback can be running); the guard itself just travels
    // with the runtime task.
    unsafe impl Send for WatchGuard {}

    /// SYSTEM-worker-thread callback: format a minimal description and
    /// hand off through the unbounded (non-blocking) channel. Never blocks,
    /// never panics (all reads are guarded).
    unsafe extern "system" fn on_route_change(
        caller_context: *const c_void,
        row: *const MIB_IPFORWARD_ROW2,
        notification_type: MIB_NOTIFICATION_TYPE,
    ) {
        if caller_context.is_null() {
            return;
        }
        let ctx = unsafe { &*(caller_context as *const Ctx) };
        let desc = if row.is_null() {
            format!("type={notification_type}")
        } else {
            let row = unsafe { &*row };
            let prefix = &row.DestinationPrefix;
            let family = unsafe { prefix.Prefix.si_family };
            if family == AF_INET {
                let o = unsafe { prefix.Prefix.Ipv4.sin_addr.S_un.S_addr }.to_ne_bytes();
                format!(
                    "type={notification_type} dst={}.{}.{}.{}/{}",
                    o[0], o[1], o[2], o[3], prefix.PrefixLength
                )
            } else if family == AF_INET6 {
                format!("type={notification_type} dst=v6/{}", prefix.PrefixLength)
            } else {
                format!("type={notification_type} family={family}")
            }
        };
        let _ = ctx.tx.send(desc);
    }

    pub(super) fn spawn(tx: UnboundedSender<String>) -> Option<WatchGuard> {
        let ctx = Box::into_raw(Box::new(Ctx { tx }));
        let mut handle: HANDLE = core::ptr::null_mut();
        // AF_UNSPEC: both v4 (peer /32s) and v6 (exit /1s) tables;
        // initial_notification = false (we only want CHANGES).
        let rc = unsafe {
            NotifyRouteChange2(
                AF_UNSPEC,
                Some(on_route_change),
                ctx as *const c_void,
                false,
                &mut handle,
            )
        };
        if rc != NO_ERROR {
            tracing::warn!(rc, "overlay: NotifyRouteChange2 registration failed");
            drop(unsafe { Box::from_raw(ctx) });
            return None;
        }
        Some(WatchGuard { handle, ctx })
    }

    impl Drop for WatchGuard {
        fn drop(&mut self) {
            unsafe {
                // Blocks until in-flight callbacks complete — after this the
                // context box is provably unreferenced.
                CancelMibChangeNotify2(self.handle);
                drop(Box::from_raw(self.ctx));
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::process::Stdio;

    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::sync::mpsc::UnboundedSender;

    pub(super) struct WatchGuard {
        // Held for kill_on_drop; the reader task ends when the child dies.
        _child: tokio::process::Child,
        reader: tokio::task::JoinHandle<()>,
    }

    pub(super) fn spawn(tx: UnboundedSender<String>) -> Option<WatchGuard> {
        let mut child = tokio::process::Command::new("ip")
            .args(["-o", "monitor", "route"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).is_err() {
                    break; // runtime side gone
                }
            }
            // Child exited / stdout closed: the sender drops here, so the
            // runtime's recv() yields None and it falls back to tick-only.
        });
        Some(WatchGuard {
            _child: child,
            reader,
        })
    }

    impl Drop for WatchGuard {
        fn drop(&mut self) {
            self.reader.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kill-switch parse (default ON; explicit 0/false/off only).
    #[test]
    fn env_kill_switch_parses() {
        // Can't set the env here (parallel tests + the shared parser reads
        // the live environment) — exercise the match shape directly.
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

    /// The subscription spawns and tears down cleanly (no events required —
    /// generating one needs privileges CI doesn't have). On hosts without
    /// the platform facility (`ip` missing) spawn returns None, which is
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
