// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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

/// Receive-liveness deadline for the ESTABLISHED control WS. The server
/// auto-pongs our 25 s keepalive Pings, so a healthy link never goes this
/// long (3 keepalive periods + margin) without SOME inbound frame. Send
/// success alone proves nothing: a TLS-inspecting corp middlebox terminates
/// TCP locally and keeps ACKing our Pings after its upstream leg has died —
/// field case WINHOST-A 2026-08-02, where the agent sat "connected" for 45+
/// minutes after a server pod roll while registered on no pod (heartbeats,
/// log uploads and overlay all fine; rc/tunnel dead). Reconnect must key on
/// frames RECEIVED, not frames sent.
const WS_RX_DEADLINE: Duration = Duration::from_secs(80);

/// Hard bound on a single WS frame WRITE on the established connection.
/// Every select arm awaits its sends inline, so one wedged `ws.send`
/// freezes the whole loop: no arm runs, no `watchdog::tick`, and at 90 s
/// the process watchdog kills the worker. Field case WINHOST-A 2026-08-15
/// 13:42:47Z — a corp-VPN connect captured the default route mid-flight,
/// the kernel kept retransmitting the WS TCP stream into the void, the
/// loop froze on a send and `stalled=signaling=92s` turned a routing
/// blip into a process death: full overlay teardown, warm relay
/// allocations lost, >2 min blackout. A send that can't complete in 15 s
/// IS a dead connection — surface it as `ConnectError::Transient` and
/// let the reconnect ladder cycle the WS (the persistent overlay runtime
/// rides through WS reconnects untouched; the `SignalingPumpGuard`
/// disarms the stall budget during backoff). 15 s tolerates a slow-but-
/// alive link (3 missed RTOs) and leaves 75 s of watchdog headroom.
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(15);

/// Hard bound on one session's `get_stats()` read in the heartbeat arm.
/// webrtc-rs stats await internal ICE locks; on a peer whose network was
/// just captured (corp-VPN connect mid-RC-session) those locks are held by
/// tasks retransmitting into the void, and the await blocks for MINUTES —
/// field WINHOST-A 2026-08-15, FOUR `stalled=signaling=9xs` process deaths
/// in one day (09:48, 13:42, 18:22, 19:13), the last two AFTER the WS
/// writes were bounded: the freeze was never only the socket. Telemetry is
/// decorative — a wedged peer forfeits its stats round, the loop moves on.
const SESSION_STATS_BUDGET: Duration = Duration::from_secs(3);

/// Hard bound on one peer's `pc.close()` during connection teardown — the
/// same webrtc-internals hang class as [`SESSION_STATS_BUDGET`], on the
/// paths that run BEFORE the reconnect can start (`close_all_peers` &c.).
/// Bounding it is safe: the P6 session-scoped teardown (arbiter dereg,
/// display-match release) runs in `AgentPeer`'s Drop on every path, so an
/// expired close forfeits only the polite goodbye to a peer that is
/// unreachable anyway.
const PEER_CLOSE_BUDGET: Duration = Duration::from_secs(5);

/// FR-27 — how long to wait for `companion::ensure_running` before deciding
/// this host has no on-screen prompt surface.
///
/// Bounded because the probe shells out (`launchctl`, `loginctl`,
/// `systemd-run`, `tasklist`) and the attended prompt is already counting down
/// against a 30 s window the controller shares: spending a large slice of it
/// waiting on a wedged helper would turn a slow answer into no answer. A
/// timeout is treated as "no surface", which is the same thing the operator
/// experiences.
pub(crate) const COMPANION_START_BUDGET: Duration = Duration::from_secs(3);

/// Pong-RTT degradation bound for the control WS. Field winhost-a 2026-08-15
/// ~20:00Z: a WS that SURVIVED a corp-VPN route capture kept "working"
/// with application round-trips over 60 s — every send completed into the
/// local TCP
/// buffer ([`WS_SEND_TIMEOUT`] never fired) and frames DID eventually
/// arrive (the RX deadline never fired), but overlay relay grants and
/// fleet-RPC rode a molasses path: the primary org's pairs sat
/// carrier-less while the SECONDARY org, whose WS had reconnected fresh
/// after the capture, relayed fine. Liveness checks cannot see this; only
/// latency can. The keepalive ping carries its send time and the echoed
/// pong measures the true round trip.
const WS_PONG_RTT_DEGRADED: Duration = Duration::from_secs(20);

/// Consecutive degraded pongs before the WS is cycled — one slow pong can
/// be a scheduler/GC blip; two spaced a keepalive interval apart is a path
/// verdict. A ping whose pong hasn't arrived within
/// [`WS_PONG_RTT_DEGRADED`] by the next keepalive check counts as a strike
/// too — waiting for the late pong itself made each verdict cost a full
/// zombie round-trip (field 2026-08-15: 41 s RTT ⇒ conviction at ~90 s;
/// with missing-pong strikes + [`WS_PING_RETRY_ACCEL`] it lands ≈ 50-65 s,
/// and the relay rebuild escapes the zombie one round-trip sooner).
const WS_PONG_RTT_STRIKES: u32 = 2;

/// After the FIRST strike the keepalive re-arms at this interval instead of
/// the normal 25 s, so the confirming verdict lands quickly. Only the
/// cadence accelerates — the [`WS_PONG_RTT_DEGRADED`] deadline per ping is
/// unchanged, so a healthy-but-jittery link still gets its full window to
/// answer before the second strike.
const WS_PING_RETRY_ACCEL: Duration = Duration::from_secs(10);

/// Slowness-cycle treadmill cap: consecutive YOUNG connections (see
/// [`WS_TREADMILL_FRESH_WINDOW`]) convicted for RTT degradation before the
/// loop stops cycling and HOLDS the alive-but-slow WS instead.
///
/// The degraded-cycle verdict exists for the regime where a fresh
/// connection escapes a zombie (field 2026-08-15: the secondary org
/// reconnected fresh after a capture and relayed fine while the primary's
/// grandfathered WS crawled). But the OPPOSITE regime is just as real
/// (field 2026-08-17, winhost-a in-VPN morning): Check Point throttled EVERY
/// connection to ~41 s RTTs, so each cycle bought a fresh WS that
/// re-convicted within 2-3 min — a treadmill whose collateral was worse
/// than the slowness itself (each cycle flaps server presence
/// offline→online, so every peer's netmap churns and half-rebuilt pairs
/// park "blocked"; grants/netmap/exec stall in every down-window). Two
/// consecutive fresh connections that ALSO went slow prove cycling is not
/// helping — from then on slowness only detects (logged, strikes reset),
/// never selects the cycle. True deadness still convicts: the
/// [`WS_RX_DEADLINE`] path and send failures are untouched.
const WS_SLOWNESS_TREADMILL_CAP: u32 = 2;

/// A connection that stayed healthy at least this long before its first
/// degradation verdict starts a FRESH slowness episode (treadmill counter
/// rearms at 1): the path evidently worked, so cycling deserves another
/// chance. Connections convicted younger than this are treadmill evidence.
const WS_TREADMILL_FRESH_WINDOW: Duration = Duration::from_secs(600);

/// `true` = HOLD the degraded-but-alive WS (cycling has been shown not to
/// help); `false` = cycle as before. Books this conviction on the
/// cross-connection counter either way.
fn slowness_treadmill_holds(cycles: &mut u32, connection_age: Duration) -> bool {
    if connection_age >= WS_TREADMILL_FRESH_WINDOW {
        *cycles = 1;
        return false;
    }
    *cycles = cycles.saturating_add(1);
    *cycles > WS_SLOWNESS_TREADMILL_CAP
}

/// netstate PR-2 — a MAJOR network change (default route moved / addresses
/// vanished) probes the control WS immediately: a ping with this deadline,
/// [`WS_NETCHANGE_PROBE_STRIKES`] chances. The first cut (5 s, single
/// strike) cycled WORKING sockets all over an in-VPN host: under Check
/// Point throttle a healthy WS legitimately answers in 6-15 s, and the
/// route-flap storm re-fired the probe every couple of minutes — reattach
/// loops, netmap/grant starvation, every pair "blocked" (field 2026-08-16
/// 19:0xZ). 2×10 s convicts a genuinely dead socket in ≤20 s — still 2-4×
/// faster than the organic path — while a slow-but-alive one survives.
const WS_NETCHANGE_PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// Missed probe windows before the WS is cycled (see above).
const WS_NETCHANGE_PROBE_STRIKES: u8 = 2;

/// netstate PR-2 — the delta receiver, feature-shaped: the overlay builds
/// subscribe to the process-wide monitor; a no-overlay build has no
/// subsystem and the arm pends forever.
#[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
type NetDeltaRx = tokio::sync::broadcast::Receiver<tunnel_core::overlay::netstate::NetDelta>;
#[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
type NetDeltaRx = std::convert::Infallible;

/// Yield the summary of the next MATERIAL+MAJOR network delta; pend forever
/// with no subscription (or after the monitor closes). Minor/immaterial
/// deltas and `Lagged` are absorbed here — the probe is strictly for "the
/// network moved" moments. Cancel-safe (broadcast `recv` is cancel-safe).
async fn next_major_netchange(rx: &mut Option<NetDeltaRx>) -> String {
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    {
        use tokio::sync::broadcast::error::RecvError;
        use tunnel_core::overlay::netstate::Severity;
        while let Some(r) = rx.as_mut() {
            match r.recv().await {
                Ok(d) if d.material && d.severity == Severity::Major => return d.summary,
                Ok(_) | Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => {
                    *rx = None;
                    break;
                }
            }
        }
    }
    #[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
    let _ = rx;
    std::future::pending().await
}

/// A5 — minimum wall-vs-monotonic skew that reads as suspend/resume at a
/// keepalive tick (mirrors the overlay runtime's `RESUME_SKEW_THRESHOLD`).
/// Below this it's scheduler jitter; above it the host slept and the WS is
/// cycled proactively instead of trusting a possibly-dead socket for up to
/// another RX-deadline window.
const RESUME_SKEW_MIN: Duration = Duration::from_secs(120);

/// A5 — a control-WS outage this long (or longer) triggers an immediate
/// update check on reconnect: the standing fleet-heal path for an agent
/// that missed a server-pushed `rc:agent.update` while its socket was
/// wedged/split (the push rides this very WS). One hour matches "a broken
/// agent should pick up the fixed build within minutes of recovering, not
/// at the next periodic check".
const UPDATE_RECHECK_AFTER_DOWN: Duration = Duration::from_secs(3600);

/// Multi-org P1 — per-enrollment context for one supervised signaling loop.
/// `run_cmd` builds one per enabled enrollment (the config's scalar primary
/// + each `[[orgs]]` entry) and spawns one [`run`] per context.
#[derive(Clone)]
pub struct OrgCtx {
    /// `primary` or the `[[orgs]]` label. Log/CLI-facing.
    pub label: String,
    /// Only the primary enrollment drives PROCESS-WIDE effects: the
    /// self-updater (`rc:agent.update` + the long-outage recheck),
    /// attention sentinels (the notify subsystem has no per-org files),
    /// exit-route purges, and the `process::exit` escalations — a secondary
    /// org's terminal conditions stop ITS loop, never the daemon.
    pub is_primary: bool,
    /// Watchdog pump name, registered by `run_cmd` before spawn:
    /// `signaling` for the primary (kept verbatim for log/tooling
    /// continuity) and `signaling:<label>` for secondaries — per-org pumps
    /// so one org's healthy ticks can't mask another org's stalled loop.
    /// `&'static` because the watchdog registry is keyed on static strs;
    /// secondary names are INTERNED once per [`OrgCtx::secondary`] call
    /// (bounded by the org count for the process lifetime — `run_cmd`
    /// builds each org's ctx exactly once and clones it thereafter).
    pub pump: &'static str,
    /// A5 — epoch-ms of the moment THIS org's control WS first went down in
    /// the current outage; 0 = up (or never connected). Was a process-global
    /// static pre-multi-org: one org's reconnect would have erased another
    /// org's outage stamp (and fired the update recheck off the wrong org).
    down_since_ms: Arc<std::sync::atomic::AtomicI64>,
    /// `rc:agent.update` pushes ignored on this org's WS because it is not
    /// the primary. Surfaced in the LocalAPI `OrgStatus`.
    pub updates_ignored: Arc<std::sync::atomic::AtomicU32>,
}

impl OrgCtx {
    pub fn primary() -> Self {
        Self {
            label: crate::config::PRIMARY_ORG_LABEL.to_string(),
            is_primary: true,
            pump: "signaling",
            down_since_ms: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            updates_ignored: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }

    pub fn secondary(label: &str) -> Self {
        Self {
            label: label.to_string(),
            is_primary: false,
            // Interned (see the `pump` field doc): the watchdog registry
            // wants `&'static str`; one bounded leak per org per process.
            pump: Box::leak(format!("signaling:{label}").into_boxed_str()),
            down_since_ms: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            updates_ignored: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }

    fn note_ws_down(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        // Only the FIRST failure of an outage stamps (CAS from 0).
        let _ = self.down_since_ms.compare_exchange(
            0,
            now,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn note_ws_up_and_maybe_recheck_update(&self) {
        let since = self
            .down_since_ms
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        if since == 0 {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let down_ms = now.saturating_sub(since);
        if down_ms < UPDATE_RECHECK_AFTER_DOWN.as_millis() as i64 {
            return;
        }
        if self.is_primary {
            let fired = crate::updater::request_update_now(None);
            info!(
                down_mins = down_ms / 60_000,
                update_check_fired = fired,
                "control WS recovered after a long outage — requesting an immediate update check"
            );
        } else {
            // The machine-wide updater is the primary's to drive; a
            // secondary's outage recovering says nothing about the
            // primary's ability to receive pushes.
            info!(
                org = %self.label,
                down_mins = down_ms / 60_000,
                "org control WS recovered after a long outage (update recheck is primary-only)"
            );
        }
    }
}

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
struct SignalingPumpGuard {
    pump: &'static str,
}

impl SignalingPumpGuard {
    /// Activate the watchdog pump for THIS org's loop and reset its
    /// `last_tick` (the `gate(false → true)` transition resets the
    /// timer; see `watchdog.rs::Watchdog::gate`). Use right after
    /// `connect_async` returns Ok.
    fn activate(pump: &'static str) -> Self {
        watchdog::gate(pump, true);
        Self { pump }
    }
}

impl Drop for SignalingPumpGuard {
    fn drop(&mut self) {
        watchdog::gate(self.pump, false);
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
    fn mark(flag: Arc<AtomicBool>, clear_attention: bool) -> Self {
        flag.store(true, Ordering::Relaxed);
        // S1b — a healthy authenticated connect resolves (almost) every
        // attention reason by definition: auth works, no goodbye, no live
        // duel. Clear stale sentinels in BOTH locations so the desktop's
        // "Attention required" banner can't outlive the condition (the old
        // code cleared only after a same-process auth-failure streak, so
        // e.g. test-artifact sentinels stuck forever).
        //
        // FR-53: `rollback_failed` used to be spared unconditionally, which
        // left a recovered device claiming a crash loop in a build it no
        // longer runs — measured on a host asserting "0.4.34 has crashed 3
        // times, reinstall manually" while running 0.4.41. It is now spared
        // only while the accused build is the one connecting, which is the
        // case the exemption was actually written for. The version passed
        // here is this binary's own, so it is a fact and not a claim.
        //
        // Multi-org P1: PRIMARY-only. The notify subsystem has no per-org
        // sentinel files, so a healthy SECONDARY connect must not clear a
        // sentinel the (possibly still-broken) primary raised.
        if clear_attention {
            crate::notify::clear_attention_on_healthy_connect_from(env!("CARGO_PKG_VERSION"));
        }
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
/// B1 — the CURRENT connection's RTT-sample sink. Each overlay start
/// installs a hook wrapping a WEAK handle to its runtime's event
/// channel (weak by design — the runtime tears down when its
/// connection's `evt_tx` drops, and a strong clone held by the
/// process-lifetime prober would defeat that; a dead weak sender just
/// drops the sample). Type-erased so this compiles without the overlay
/// features (`tunnel_core::overlay` is feature-gated): the slot simply
/// stays `None` and the prober's hook is inert.
pub type RttSampleSlot = Arc<std::sync::RwLock<Option<crate::localapi_state::RttSampleHook>>>;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    // Multi-org P1 — which enrollment this loop serves ([`OrgCtx::primary`]
    // for the config's scalar identity; `run_cmd` synthesizes a per-org
    // `AgentConfig` via `AgentConfig::for_org` for secondaries).
    ctx: OrgCtx,
    // FR-43 P2b — this process's part in delegation. Owned so the reconnect
    // loop can lend it per attempt. Only the PRIMARY org's loop ever acts on it
    // (`handle_server_msg` gates on `ctx.is_primary`), but it is passed to all
    // of them so the gate lives in one place rather than at three call sites
    // that could each forget it.
    mut delegation: crate::delegate::Delegation,
    // `mut` for exactly one reason: FR-40 key rotation swaps the overlay
    // secret in this snapshot between two connections (see
    // `ConnectError::KeyRotated`). Nothing else writes it after start.
    mut cfg: AgentConfig,
    encoder_preference: crate::encode::EncoderPreference,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    // Unification P1 — LocalAPI live handles (stable across reconnects, owned by
    // `run_cmd`): a flag flipped while connected, and the channel the overlay
    // runtime publishes its mesh view on.
    connected: Arc<AtomicBool>,
    overlay_view_tx: tokio::sync::watch::Sender<OverlayView>,
    // B1 — where each connection publishes its (downgraded) overlay event
    // sender for the RTT prober bridge.
    rtt_sample_slot: RttSampleSlot,
    // P2b — the operator-consent broker, created in `run_cmd` and SHARED with the
    // LocalAPI's DaemonState so its live `pending` set gates LocalAPI decisions.
    consent_broker: crate::consent::ConsentBroker,
    // P3b-2 PR-C — the tunnel-client hub, created in `run_cmd` and SHARED with the
    // LocalAPI's DaemonState (create/kill/flows verbs). The signaling loop
    // publishes the live agent-WS egress into it + demuxes client-bound replies.
    tunnel_hub: crate::tunnel::client_mgr::TunnelClientHub,
    // Remote config (docs/remote-config.md) — the config path, the daemon-wide
    // write lock and the LIVE `exec_enabled` as one value. Machine-wide, so the
    // SAME instance is shared by every org loop; only the primary ever applies.
    remote_cfg: crate::remote_config::RemoteConfigServices,
    // FR-27 — the daemon-wide live-session registry, so a thin client can see
    // "Being viewed by …" and end a session. Each loop registers its OWN kill
    // channel per session, which is what makes a Disconnect on a multi-org
    // daemon reach the loop that actually owns that session.
    rc_sessions: crate::rc_sessions::RcSessionRegistry,
) -> Result<()> {
    // Overlay "Disconnect" control → this channel → the connect_once
    // loop, which tears the session down (peer close + ClientMsg::Terminate).
    // Created once; the receiver is polled inside connect_once across
    // reconnects. A `kill_tx` clone is retained below for the whole loop
    // so the channel never fully closes — a closed receiver would busy-
    // spin the select! — even when the overlay is disabled.
    let (kill_tx, mut kill_rx) = mpsc::channel::<bson::oid::ObjectId>(4);
    // rc.307 (B) — multi-region DERP admission-ticket cache, hoisted from
    // connect_once scope: the PERSISTENT overlay runtime's regional-DERP
    // factory captures this slot ONCE, so a per-connection slot went stale
    // after the first reconnect (nothing refilled the captured copy →
    // tickets expired → regional relays permanently degraded to central).
    // Process-scope + threaded into every connection, like rtt_sample_slot.
    let derp_ticket_slot: crate::relay_probe::DerpTicketSlot = Default::default();
    // One overlay handle, reused across reconnects. Failing to bring up
    // the indicator is non-fatal — the session still works, the user
    // just doesn't get the visual "you're being watched" cue.
    // FR-27 — where a NATIVE consent panel's Approve/Deny comes back. The
    // backend never resolves consent itself: the answer lands here, the loop
    // below feeds it to the broker, and the broker applies the gate it already
    // has. One decision point for the native panel, the companion and the CLI
    // — three would be three chances to disagree about whether a session was
    // approved. A retained sender keeps the receiver from closing (same reason
    // as `kill_tx`), so the `select!` cannot busy-spin where no panel exists.
    let (consent_tx, mut consent_rx) = mpsc::channel::<(String, bool)>(4);
    let indicator =
        match ViewerIndicator::new(kill_tx.clone(), rc_sessions.clone(), consent_tx.clone()) {
            Ok(v) => v,
            Err(e) => {
                warn!(%e, "viewer-indicator init failed; continuing without overlay");
                // FR-27 — an overlay that could not start must NOT also cost the
                // device its session registry: `disabled()` carries a private one,
                // so rebuild the real handle around it. This is the ordinary case
                // on macOS and Linux, where there is no native overlay at all and
                // the companion's banner is the only one.
                ViewerIndicator::disabled().with_registry(kill_tx.clone(), rc_sessions.clone())
            }
        };
    let _consent_keepalive = consent_tx;
    // Keep the sender alive for the loop's lifetime (see above).
    let _kill_keepalive = kill_tx;
    // Operator-consent broker: created in `run_cmd` and passed in (P2b), so the
    // LocalAPI shares the SAME instance and its live `pending` set. Lives across
    // reconnects, so a sentinel dropped while the WS was down is still honoured.
    let mut backoff = Duration::from_secs(1);
    // netstate PR-2b — a MAJOR network change during a reconnect backoff
    // cuts the wait and resets the ladder: the transition that killed this
    // WS has ended (or moved again), and an exponential grown to 60 s while
    // the corp path blackholed fresh TLS must not ALSO delay the recovery
    // attempt. Field 2026-08-16, org{label=jovanov}: the WS spent most of a
    // 5-minute captive window parked in backoff — no WS ⇒ no grants, no
    // netmap, no honored pins ⇒ the org's entire overlay sat dark until the
    // ladder happened to line up with a reconnectable path.
    let mut ladder_net_rx: Option<NetDeltaRx> = {
        #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
        {
            tunnel_core::overlay::netstate::handle().map(|h| h.subscribe())
        }
        #[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
        {
            None
        }
    };
    let mut auth_failures: u32 = 0;
    // rc.53: rolling window of recent `ReplacedByNewerConnection`
    // events. Three within 5 min escalates from "back off 60 s and
    // hope the duel settles" to "operator action required +
    // process::exit(AGENT_DELETED_EXIT_CODE)" — at that point the
    // duel is real, neither instance can win, and the operator needs
    // to find + stop the duplicate (or re-enrol THIS host with a
    // fresh enrollment JWT to mint a new agent_id).
    let mut recent_replacements: Vec<std::time::Instant> = Vec::new();
    // Slowness-cycle treadmill (see [`WS_SLOWNESS_TREADMILL_CAP`]): spans
    // reconnects so "the fresh connection ALSO went slow" is countable.
    let mut slowness_cycles: u32 = 0;
    loop {
        if *shutdown.borrow() {
            info!("shutdown signalled; exiting signaling loop");
            return Ok(());
        }

        match connect_once(
            &ctx,
            &cfg,
            &remote_cfg,
            encoder_preference,
            shutdown.clone(),
            indicator.clone(),
            consent_broker.clone(),
            connected.clone(),
            overlay_view_tx.clone(),
            rtt_sample_slot.clone(),
            derp_ticket_slot.clone(),
            tunnel_hub.clone(),
            &mut slowness_cycles,
            &mut delegation,
            &mut kill_rx,
            &mut consent_rx,
        )
        .await
        {
            Ok(()) => {
                info!("signaling connection closed cleanly, reconnecting");
                backoff = Duration::from_secs(1);
                slowness_cycles = 0;
                if auth_failures > 0 {
                    info!(
                        prior_auth_failures = auth_failures,
                        "auth recovered; clearing attention sentinel"
                    );
                    // Multi-org P1: sentinels are primary-owned (no per-org
                    // files) — a secondary's recovery must not clear one.
                    if ctx.is_primary {
                        notify::clear_attention();
                    }
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
                // operator genuinely needs to act. Multi-org P1: the
                // sentinel is primary-only; a secondary org's auth death
                // is surfaced via the LocalAPI `OrgStatus` + logs.
                if auth_failures == 3 && ctx.is_primary {
                    let msg = "Roomler agent: re-enrollment required.\n\n\
                              The server is rejecting this agent's token. \
                              Either the token expired (default 1 year) or an \
                              admin revoked it. Run:\n\n\
                              \troomlerd re-enroll --token <new-jwt>\n\n\
                              with a fresh enrollment JWT from the admin UI \
                              to restore service.";
                    match notify::raise_attention_with_reason(notify::REASON_AUTH, msg) {
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
                // Multi-org P1: a SECONDARY org's goodbye (row deleted /
                // policy refused in THAT org) terminates only its own loop.
                // No sentinel (primary-owned), no exit-route purge (the
                // primary's overlay owns those routes), no process exit —
                // the supervisor in `run_cmd` records the terminal error
                // for the LocalAPI `OrgStatus`.
                if !ctx.is_primary {
                    warn!(
                        org = %ctx.label,
                        ?reason,
                        %message,
                        "server-side close for this org — stopping its loop (other orgs unaffected)"
                    );
                    return Err(anyhow::anyhow!(
                        "server goodbye ({reason:?}): {message} — re-enroll this org \
                         with `roomlerd enroll --server <url> --token <new-jwt>`"
                    ));
                }
                // rc.53: server told us to stop reconnecting. The
                // teardown of in-flight peers already ran in the
                // `handle_server_msg::ServerMsg::Goodbye` arm
                // (close_all_peers + close_all_tunnel_peers) — this
                // arm only writes the operator sentinel + exits.
                let body = format!(
                    "Roomler agent: server-side close — {reason:?}.\n\n{message}\n\n\
                     The agent will not reconnect. Re-enrol with a fresh enrollment \
                     JWT from the admin UI:\n\n\
                     \troomlerd re-enroll --token <new-jwt>\n\n\
                     then restart the service (or wait for the supervisor to relaunch)."
                );
                match notify::raise_attention_machine_aware_with_reason(
                    notify::REASON_GOODBYE,
                    &body,
                ) {
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
                // P5/A2 — a fatal goodbye is a PERMANENT stop (the row is gone);
                // no restart will run the boot reconciler, so drop any exit-node
                // split-default now or the host stays blackholed until reboot.
                crate::purge_exit_routes();
                std::process::exit(watchdog::AGENT_DELETED_EXIT_CODE);
            }
            Err(ConnectError::KeyRotated {
                secret_base64,
                key_epoch,
            }) => {
                // FR-40 — the handler already persisted the new key (persist
                // FIRST, or a restart would bring the retired key back). Adopt
                // it here, in the one snapshot `connect_once` reads, and go
                // straight back: this is one device re-joining, not a fleet
                // event, so no stagger. `overlay::maybe_start` sees the
                // fingerprint change (it includes the public key) and rebuilds
                // the runtime; the join carries the new key + epoch.
                cfg.overlay_wg_secret_key = Some(secret_base64);
                cfg.overlay_wg_key_epoch = key_epoch;
                info!(
                    org = %ctx.label,
                    key_epoch,
                    "overlay key rotated — reconnecting under the new identity"
                );
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return Ok(()); }
                    },
                }
            }
            Err(ConnectError::ReplacedByNewer { message }) => {
                let now = std::time::Instant::now();
                // Drop events older than the rolling window BEFORE pushing
                // the new one (so escalation depends only on what's actually
                // within the window). W4(d): 30 min, not 5 — the backoff
                // ladder below reaches 15 min, and a 5 min window would have
                // emptied mid-sleep and reset a live duel back to full speed.
                recent_replacements
                    .retain(|t| now.duration_since(*t) < Duration::from_secs(30 * 60));
                recent_replacements.push(now);
                let displacements = recent_replacements.len();
                warn!(
                    %message,
                    count = displacements,
                    "server signalled this connection was replaced; staggering reconnect to break the duel"
                );

                // Legacy escalation (`ws_replaced_exit = true`): sentinel +
                // process exit at the 3rd displacement, exactly as before
                // W4(d). Off by default — see below.
                if displacements >= 3 && cfg.ws_replaced_exit.unwrap_or(false) {
                    // Multi-org P1: a secondary org's duel terminates only
                    // its own loop (see the FatalGoodbye arm's rationale).
                    if !ctx.is_primary {
                        warn!(
                            org = %ctx.label,
                            displacements,
                            "duplicate-instance duel on this org — stopping its loop \
                             (other orgs unaffected)"
                        );
                        return Err(anyhow::anyhow!(
                            "duplicate-instance duel: displaced {displacements}× in the \
                             window — another process is using this org's agent_id ({message})",
                        ));
                    }
                    write_replaced_sentinel(&message, displacements);
                    // P5/A2 — bypasses RAII; drop any exit-node split-default so
                    // a duel-losing instance doesn't leave the host blackholed.
                    crate::purge_exit_routes();
                    std::process::exit(watchdog::AGENT_DELETED_EXIT_CODE);
                }

                // W4(d) — NO process exit by default. Field (winhost-a, every
                // VPN transition): displacement storms on TLS-inspected paths
                // are ZOMBIE half-open WSes — self-limiting once the server's
                // receive-liveness reaps them — and each exit tore down the
                // WHOLE overlay (carriers, TUN, routes) for an event that
                // needed none of it. A TRUE duplicate-instance duel (copied
                // config.toml on another machine) isn't fixed by exiting
                // either: the supervisor respawns straight back into the
                // duel. Instead the operator sentinel is raised once per
                // storm and the ladder parks the loser at 15 min; the
                // overlay keeps running on its last netmap throughout.
                if displacements == 3 && ctx.is_primary {
                    write_replaced_sentinel(&message, displacements);
                }

                // 60 s minimum — long enough that two duelling instances
                // stagger out of phase and one wins; sustained duels climb
                // the ladder instead of escalating to an exit.
                tokio::select! {
                    _ = tokio::time::sleep(replaced_backoff_for(displacements)) => {},
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
                // A5 — first failure of this outage stamps the clock the
                // reconnect-recovery update check keys off.
                ctx.note_ws_down();
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
                    _ = tokio::time::sleep(backoff) => {
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                    },
                    // netstate PR-2b — the network moved: retry NOW with a
                    // reset ladder instead of waiting out a backoff sized
                    // for the network that no longer exists.
                    _ = next_major_netchange(&mut ladder_net_rx) => {
                        info!("network changed during reconnect backoff — retrying immediately");
                        backoff = Duration::from_secs(1);
                        // New network = new throttle regime: the slowness
                        // treadmill re-earns its verdict from scratch.
                        slowness_cycles = 0;
                    },
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return Ok(()); }
                    },
                }
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

/// W4(d) — ReplacedByNewer backoff ladder (displacements counted over a
/// 30 min rolling window). Zombie half-open WSes self-limit once the
/// server reaps them, so early rungs stay quick; a sustained TRUE duel
/// parks the loser at the 15 min cap with the operator sentinel raised —
/// it never exits the process (the overlay keeps carrying traffic on its
/// last netmap; `ws_replaced_exit = true` restores the legacy exit).
///
/// 1st/2nd → 60 s (stagger two live dialers out of phase)
/// 3rd → 2 min, 4th → 4 min, 5th → 8 min, 6th+ → 15 min
pub(crate) fn replaced_backoff_for(displacements: usize) -> Duration {
    match displacements {
        0..=2 => Duration::from_secs(60),
        3 => Duration::from_secs(120),
        4 => Duration::from_secs(240),
        5 => Duration::from_secs(480),
        _ => Duration::from_secs(900),
    }
}

/// The ReplacedByNewer operator sentinel — written once per storm (at the
/// 3rd displacement in the window), whether or not the legacy exit is on.
fn write_replaced_sentinel(message: &str, displacements: usize) {
    let body = format!(
        "Roomler agent: duplicate-instance duel detected.\n\n{message}\n\n\
         This connection has been displaced {displacements} times in the current \
         window — another process (different physical host with a copy of this \
         config.toml, or a stale half-open connection through a TLS-inspecting \
         middlebox) is using the same agent_id. The agent stays up and backs \
         off; if this persists, stop the duplicate or re-enrol THIS host with \
         a fresh enrollment JWT to mint a new agent_id."
    );
    match notify::raise_attention_machine_aware_with_reason(notify::REASON_DUPLICATE, &body) {
        Ok(path) => warn!(
            path = %path.display(),
            displacements,
            "wrote needs-attention sentinel for ReplacedByNewer escalation"
        ),
        Err(e) => warn!(
            error = %e,
            "failed to write needs-attention sentinel for ReplacedByNewer escalation"
        ),
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
    /// FR-40 — the `rc:agent.key_rotate` handler minted + PERSISTED a new
    /// overlay key for this org. The outer `run()` loop adopts it in its
    /// config snapshot (the only copy `connect_once` ever reads) and
    /// reconnects at once: the overlay runtime's fingerprint includes the
    /// public key, so the reconnect rebuilds it and joins under the new
    /// identity. Carries the secret in-process only — it never crosses a
    /// socket.
    #[error("overlay key rotated (epoch {key_epoch}); reconnecting under the new identity")]
    KeyRotated {
        secret_base64: String,
        key_epoch: u32,
    },
    #[error(transparent)]
    Transient(#[from] anyhow::Error),
}

#[allow(clippy::too_many_arguments)]
async fn connect_once(
    ctx: &OrgCtx,
    cfg: &AgentConfig,
    // Remote config — the live `exec_enabled` and the persist path.
    remote_cfg: &crate::remote_config::RemoteConfigServices,
    encoder_preference: crate::encode::EncoderPreference,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    indicator: ViewerIndicator,
    consent_broker: crate::consent::ConsentBroker,
    connected: Arc<AtomicBool>,
    overlay_view_tx: tokio::sync::watch::Sender<OverlayView>,
    // B1 — this connection publishes its downgraded overlay event sender
    // here (the RTT prober bridge reads it).
    rtt_sample_slot: RttSampleSlot,
    // rc.307 (B) — process-scope DERP admission-ticket cache (see run()).
    derp_ticket_slot: crate::relay_probe::DerpTicketSlot,
    tunnel_hub: crate::tunnel::client_mgr::TunnelClientHub,
    // Slowness-cycle treadmill counter (see [`WS_SLOWNESS_TREADMILL_CAP`]) —
    // lives in `run`'s loop so it spans reconnects; reset on a clean close
    // and on a netstate Major (new network = new regime).
    slowness_cycles: &mut u32,
    // FR-43 P2b — this process's part in delegation: the daemon's channel, the
    // worker's two ends, or neither.
    delegation: &mut crate::delegate::Delegation,
    // Overlay "Disconnect" → session ObjectId to tear down. Borrowed
    // (not owned) so the same receiver survives across reconnects.
    kill_rx: &mut mpsc::Receiver<bson::oid::ObjectId>,
    // FR-27 — Approve/Deny from a NATIVE consent panel.
    consent_rx: &mut mpsc::Receiver<(String, bool)>,
) -> Result<(), ConnectError> {
    // S6 — `tid` is the tenant-affinity key the server-front LB hashes
    // on, co-locating this agent's WS with its tenant's controllers on
    // one pod (the rc-hub is pod-local). Server validates it against
    // the JWT's tenant claim; pre-S6 servers just ignore the param.
    let url = format!(
        "{}?token={}&role=agent&tid={}",
        cfg.ws_url(),
        urlencode(&cfg.agent_token),
        urlencode(&cfg.tenant_id)
    );
    // Log the endpoint WITHOUT the token: the URL carries the long-lived agent
    // JWT (a 1-year credential), and this line lands in the rolling log a user
    // might paste into a chat / issue (field 2026-07-25 — a controller shared a
    // log with the full token in it). The token itself never adds diagnostic
    // value here; `{server}/ws (role=agent)` is enough to see WHERE we dial.
    info!(server = %cfg.ws_url(), "connecting to signaling server (role=agent)");

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
    let _pump_guard = SignalingPumpGuard::activate(ctx.pump);
    // Unification P1 — mark the daemon connected for the LocalAPI; the guard
    // clears it on every return path (like `_pump_guard`). Sentinel clearing
    // is primary-only (see `ConnectedGuard::mark`).
    let _connected_guard = ConnectedGuard::mark(connected, ctx.is_primary);

    // Fleet RPC — teach the redactor THIS org's agent token before any exec
    // can run. Registered per-org (the registry is process-wide) so a command
    // asked for by org A can never leak org B's token in its output.
    crate::exec::register_secret(&cfg.agent_token);

    // Say hello.
    let hello = ClientMsg::AgentHello {
        machine_name: cfg.machine_name.clone(),
        os: detect_os(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        displays: stub_displays(),
        caps: Box::new(stub_caps(cfg.overlay_multi_org)),
        // Tunnel mesh subnet-router: advertise the CIDRs this host offers to
        // route — explicit `advertise_routes` config unioned with auto-detected
        // local subnets. Admin-gated server-side (untrusted until an admin
        // approves them into this agent's `routes`).
        advertised_routes: crate::subnet_detect::local_advertised_routes(cfg),
        // Multi-region relay PoPs: advertise the probe capability so the
        // server may push `rc:relay.regions` (it never sends the variant to
        // an agent that didn't flag it — our deserializer would error).
        supports_relay_regions: crate::relay_probe::probing_enabled(),
        // P6 — publish the host key's PUBLIC half so a caller can verify what
        // it dialled. Empty here is meaningful ("this device cannot prove
        // itself"), never "trust anything".
        ssh_host_pubkey: ssh_host_pubkey_for(cfg),
    };
    send_msg(&mut ws, &hello).await.context("sending hello")?;
    // rc.58: explicit tick on hello — the 25 s keepalive timer hasn't
    // fired yet, and a slow first server response (no inbound frame
    // for 30+ s) would otherwise leave the pump's `last_tick` at the
    // gate-activation instant. Belt-and-suspenders: the gate already
    // reset the timer, so this only matters when the server stalls
    // immediately after upgrade.
    watchdog::tick(ctx.pump);
    info!("rc:agent.hello sent");
    // A5 — if this connect ended a ≥1 h outage, the fleet may have shipped a
    // fix (or a forced update push) we never saw; check now, not in ≤4 h.
    ctx.note_ws_up_and_maybe_recheck_update();

    // Outbound channel shared by all per-session peers. Peers push their
    // locally-gathered ICE candidates and state-change terminates here;
    // the main loop flushes them to the WS.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<ClientMsg>(PEER_OUTBOUND_CAP);
    // Multi-region relay probing: the server's last region push (per
    // connection) + the periodic re-prober (exits with the connection —
    // its sends fail once `outbound_rx` drops).
    let relay_regions_slot: crate::relay_probe::RegionSlot = Default::default();
    if crate::relay_probe::probing_enabled() {
        tokio::spawn(crate::relay_probe::periodic_reprobe(
            relay_regions_slot.clone(),
            outbound_tx.clone(),
        ));
    }
    // P3b-2 PR-C: publish this connection's egress so tunnel-client flow
    // supervisors can open sessions over it; the guard clears it to `None` on
    // every exit path (like `_connected_guard`), so a supervisor holding the
    // dead egress re-waits for the next connection's sink.
    // FR-40 P1c — if the PREVIOUS session of this org ended in a key
    // rotation, say so again on this healthy socket: the copy sent on the
    // dying session can be lost (it was, in the second field run).
    drain_pending_rotation_report(&cfg.tenant_id, &outbound_tx);
    let _sink_guard = tunnel_hub.publish_sink(outbound_tx.clone());
    // FR-43 P2b — point the delegation channel at THIS connection's outbound
    // queue, so a worker's `SdpAnswer` / `Ice` reach the server.
    //
    // Re-set on every reconnect, deliberately: the sender belongs to the
    // connection, not the daemon. A worker reply that arrives while no WS is
    // up is dropped rather than queued, because a session without a control
    // plane is a session the server has already forgotten — queueing it would
    // deliver an answer to a question nobody is still asking.
    // Cloned FIRST so its borrow of `delegation` ends here — the worker's
    // receiver below is borrowed for the whole connection, and a process is
    // one role or the other, so only one of these is ever `Some`.
    let delegate = delegation.host().cloned();
    if let Some(delegate) = delegate.as_ref()
        && ctx.is_primary
    {
        delegate.set_outbound(outbound_tx.clone());
    }
    // FR-43 P2b-2 — the WORKER's ends. The receiver is borrowed for this
    // connection; the sender is cloned so a delegated session can hold it.
    // FR-43 P2c — what we last told the server about our permissions.
    //
    // Per CONNECTION, not per process: a reconnect sends a fresh `rc:agent.hello`
    // carrying our own caps, so the server's view resets and our memory of it
    // must reset too — otherwise a worker that attached before a reconnect would
    // never be re-announced, and the row would sit wrong until the worker
    // happened to change.
    let mut last_announced_permissions: Option<(Vec<String>, bool)> = None;
    let (mut delegated_in, delegated_out) = match delegation.worker_mut() {
        Some((rx, tx)) => (Some(rx), Some(tx.clone())),
        None => (None, None),
    };
    // Phase 3b: if overlay is enabled, start the node runtime (relay mode)
    // and capture the channel its `rc:overlay.*` events flow into. The
    // runtime sends its `ClientMsg`s back through `outbound_tx`, like any
    // peer, and tears down when this connection's `overlay_evt_tx` drops.
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    let mut overlay_evt_tx = crate::overlay::maybe_start(
        cfg,
        outbound_tx.clone(),
        overlay_view_tx.clone(),
        derp_ticket_slot.clone(),
        crate::ssh::SessionServices {
            power_activity: Some(crate::power::shared_activity().clone()),
            consent: consent_broker.clone(),
            indicator: indicator.clone(),
            activity: crate::ssh::ActivitySink::new(cfg, outbound_tx.clone()),
        },
    )
    .await;
    // B1 — install THIS connection's RTT-sample sink: a hook wrapping a
    // WEAK handle to the runtime's event channel. `None` when overlay is
    // off; a stale hook is harmless (weak upgrade fails once the runtime
    // is gone).
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    {
        *rtt_sample_slot.write().unwrap_or_else(|e| e.into_inner()) =
            overlay_evt_tx.as_ref().map(|t| {
                let weak = t.downgrade();
                let hook: crate::localapi_state::RttSampleHook =
                    Arc::new(move |node_hex: &str, rtt_ms: u32| {
                        let Ok(node_id) = bson::oid::ObjectId::parse_str(node_hex) else {
                            return;
                        };
                        if let Some(tx) = weak.upgrade() {
                            let _ = tx.try_send(
                                tunnel_core::overlay::runtime::OverlayEvent::RttSample {
                                    node_id,
                                    rtt_ms,
                                },
                            );
                        }
                    });
                hook
            });
    }
    // Without an overlay surface nothing publishes the view; keep the params
    // used so the LocalAPI wiring stays feature-agnostic in `run_cmd`.
    #[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
    let _ = (&overlay_view_tx, &rtt_sample_slot);
    let mut peers: HashMap<bson::oid::ObjectId, AgentPeer> = HashMap::new();
    // 2026-07-27 — this connect iteration starts with a fresh session map;
    // release any GPU-clock pin a previous iteration left engaged (its
    // sessions died with the old map).
    crate::gpu_clock::on_sessions_changed(0);
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
    // agent's `ROOMLERD_VP9_CHROMA` env-var default.
    let mut pending_chroma: HashMap<bson::oid::ObjectId, Option<String>> = HashMap::new();
    // FR-17 — whether THIS controller can parse the framed DataChannel wire
    // format. Same lifecycle as `pending_chroma`: stashed by the Request
    // handler, consumed when the peer is built, cleared on WS teardown.
    let mut pending_chunk_framing: HashMap<bson::oid::ObjectId, bool> = HashMap::new();
    // Same lifecycle as `pending_transports`/`pending_chroma` but for the
    // opt-in system-audio flag forwarded from the controller's
    // `rc:session.request.audio_enabled`. `false`/missing → no audio
    // track. Inserted when `rc:request` arrives, consumed at
    // `rc:sdp.offer` time where the AgentPeer + (optional) audio pump
    // are built.
    let mut pending_audio: HashMap<bson::oid::ObjectId, bool> = HashMap::new();
    // Clipboard-v2 hardening: session permission bitfield from
    // `rc:request`, consumed at `rc:sdp.offer` time so the AgentPeer's
    // DC handlers can enforce it (today: the clipboard handler).
    // Missing entry (test harness skipped rc:request) → the
    // `Permissions` default (VIEW | INPUT | CLIPBOARD), which matches
    // pre-v2 behavior.
    let mut pending_permissions: HashMap<
        bson::oid::ObjectId,
        roomler_ai_remote_control::permissions::Permissions,
    > = HashMap::new();
    // P6 — (controller display name, device-policy input mode) from
    // `rc:request`, consumed at `rc:sdp.offer` so AgentPeer can register
    // the session with the InputArbiter (participants rail + mode seed).
    let mut pending_session_meta: HashMap<
        bson::oid::ObjectId,
        (
            String,
            Option<roomler_ai_remote_control::models::InputMode>,
            // FR-27 follow-up — the asking org, carried so the "Being viewed
            // by" banner can be raised at peer-build time (not request time).
            Option<String>,
        ),
    > = HashMap::new();
    // T2.10d: one `AgentTunnelPeer` per active `roomler`
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
    // R3 — reclaim any QUIC peers stashed by a prior session's TRANSIENT exit
    // (empty unless `tunnel_peers_survive_reattach` is on ⇒ pre-R3 identical).
    let mut tunnel_quic_peers = reclaim_survived_quic_peers(&cfg.tenant_id);

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
    // server side. active_sessions comes straight from the peer map.
    // Stats PR-5: the promised sysinfo follow-up — every heartbeat now
    // carries an `AgentSysStats` block (process rss/cpu, host net
    // counters, overlay carrier tallies + median peer RTT from the
    // published view). The legacy top-level rss/cpu fields are filled
    // too, but the server reads only the block.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await; // Swallow the immediate first tick.
    let mut sys_sampler = crate::telemetry::SysSampler::new();

    // Receive-liveness stamp: refreshed on EVERY inbound frame (the Pongs the
    // server auto-sends for our keepalive Pings count), checked against
    // `WS_RX_DEADLINE` on the keepalive tick. See the const's doc for why
    // send-success is not a liveness signal.
    let mut last_rx = std::time::Instant::now();
    // Pong-RTT detector state: pings stamp `ws_epoch`-relative millis into
    // their payload; the echoed pong yields an exact application-level RTT.
    let ws_epoch = std::time::Instant::now();
    let mut slow_pongs: u32 = 0;
    // Send time of the newest keepalive ping still awaiting its pong.
    // `None` once any stamped pong arrives (the RTT verdict then comes from
    // the stamp itself). Checked at each keepalive tick: still outstanding
    // past the degradation bound = a strike WITHOUT waiting for the late
    // pong to crawl back.
    let mut outstanding_ping: Option<std::time::Instant> = None;

    // A5 — resume detection. Every timer here runs on the MONOTONIC clock,
    // which excludes suspend on Windows/Linux — so after a sleep the RX
    // deadline picks up where it left off and a socket that died during
    // the nap stays trusted for up to ~80 more seconds. Wall-vs-monotonic
    // skew at the keepalive tick catches the nap (the overlay runtime and
    // the watchdog use the same trick) and cycles the WS immediately: a
    // fresh connect is cheap, a post-resume zombie socket is not.
    let mut skew_wall = std::time::SystemTime::now();
    let mut skew_mono = std::time::Instant::now();

    // netstate PR-2 — probe-then-cycle on Major network changes. `Some` =
    // a probe ping is out: (window start, missed windows so far); verdict
    // at the next keepalive tick (pulled forward to the deadline).
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    let mut net_rx: Option<NetDeltaRx> =
        tunnel_core::overlay::netstate::handle().map(|h| h.subscribe());
    #[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
    let mut net_rx: Option<NetDeltaRx> = None;
    let mut netchange_probe: Option<(std::time::Instant, u8)> = None;

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("shutdown signalled; closing ws");
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    // Terminal: a daemon shutdown must tear peers down, never
                    // stash them (the process is going away).
                    close_all_tunnel_quic_peers(&mut tunnel_quic_peers).await;
                    let _ = send_frame(&mut ws, Message::Close(None)).await;
                    return Ok(());
                }
            }
            _ = keepalive.tick() => {
                // W4(a) — TUN-death self-heal. The overlay runtime EXITS when
                // its TUN dies (a contract the runtime tests pin), and before
                // this the only respawner was the NEXT control-WS reconnect
                // discovering the closed slot — a healthy WS + a dead TUN
                // meant silently no mesh, indefinitely. A closed event sender
                // IS the death signal and `maybe_start` IS the respawner
                // (fingerprint/slot logic included); the 25 s keepalive
                // cadence is the natural retry backoff. (The B1 RTT hook
                // stays bound to the old runtime until the next reconnect —
                // harmless: its weak upgrade fails and samples drop.)
                #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
                if overlay_evt_tx.as_ref().is_some_and(|t| t.is_closed()) {
                    warn!(
                        "overlay runtime exited mid-session (TUN death?) — respawning now \
                         instead of waiting for the next reconnect"
                    );
                    overlay_evt_tx = crate::overlay::maybe_start(
                        cfg,
                        outbound_tx.clone(),
                        overlay_view_tx.clone(),
                        derp_ticket_slot.clone(),
                        crate::ssh::SessionServices {
                            power_activity: Some(crate::power::shared_activity().clone()),
            consent: consent_broker.clone(),
                            indicator: indicator.clone(),
                            activity: crate::ssh::ActivitySink::new(cfg, outbound_tx.clone()),
                        },
                    )
                    .await;
                }
                let wall_gap = skew_wall.elapsed().unwrap_or_default();
                let mono_gap = skew_mono.elapsed();
                skew_wall = std::time::SystemTime::now();
                skew_mono = std::time::Instant::now();
                if wall_gap > mono_gap + RESUME_SKEW_MIN {
                    warn!(
                        napped_s = (wall_gap - mono_gap).as_secs(),
                        "suspend/resume detected at keepalive — cycling the control WS"
                    );
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                    return Err(ConnectError::Transient(anyhow::anyhow!(
                        "resume-from-suspend (napped ~{} s)",
                        (wall_gap - mono_gap).as_secs()
                    )));
                }
                if last_rx.elapsed() > WS_RX_DEADLINE {
                    warn!(
                        silent_s = last_rx.elapsed().as_secs(),
                        "no inbound WS frames within the RX deadline — half-open socket \
                         (middlebox still ACKs our pings); reconnecting"
                    );
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                    return Err(ConnectError::Transient(anyhow::anyhow!(
                        "ws rx deadline: no inbound frames for {} s",
                        last_rx.elapsed().as_secs()
                    )));
                }
                // netstate PR-2 — the net-change probe verdict. Answered =
                // its pong cleared `outstanding_ping`, or ANY frame arrived
                // after the probe went out (both prove the socket survived
                // the transition). Unanswered past the tight deadline =
                // cycle NOW, single strike — the network moved under the
                // socket and every second of blind trust stalls the whole
                // grants/netmap plane behind a dead WS.
                if let Some((probed, misses)) = netchange_probe {
                    if outstanding_ping.is_none() || last_rx.elapsed() < probed.elapsed() {
                        netchange_probe = None;
                    } else if probed.elapsed() >= WS_NETCHANGE_PROBE_DEADLINE {
                        let misses = misses + 1;
                        if misses >= WS_NETCHANGE_PROBE_STRIKES {
                            warn!(
                                unanswered_s = probed.elapsed().as_secs(),
                                misses,
                                "control WS unresponsive after a MAJOR network change — cycling now"
                            );
                            close_all_peers(&mut peers, &indicator).await;
                            close_all_tunnel_peers(&mut tunnel_peers).await;
                            park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                            return Err(ConnectError::Transient(anyhow::anyhow!(
                                "net-change probe: {} windows of {} s unanswered after a \
                                 Major network change",
                                misses,
                                WS_NETCHANGE_PROBE_DEADLINE.as_secs()
                            )));
                        }
                        // One more chance: a throttled-but-alive corp path
                        // legitimately answers slower than one window.
                        warn!(
                            unanswered_s = probed.elapsed().as_secs(),
                            "net-change probe window missed once — re-probing (2-strike rule)"
                        );
                        if let Err(e) =
                            send_frame(&mut ws, Message::Ping(ping_payload(ws_epoch).into())).await
                        {
                            warn!(%e, "net-change re-probe ping failed — reconnecting");
                            close_all_peers(&mut peers, &indicator).await;
                            close_all_tunnel_peers(&mut tunnel_peers).await;
                            park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                            return Err(ConnectError::Transient(e.context("net-change re-probe")));
                        }
                        netchange_probe = Some((std::time::Instant::now(), misses));
                        keepalive.reset_after(WS_NETCHANGE_PROBE_DEADLINE);
                    }
                }
                // Missing-pong strike: a ping still unanswered past the
                // degradation bound is the SAME verdict as a late pong,
                // available one zombie round-trip sooner (field: 41 s RTT
                // priced conviction at ~90 s when every verdict waited for
                // its pong to crawl back). A ping still WITHIN its window
                // rides — sending a replacement would reset its clock and
                // the deadline could never accrue under the accelerated
                // cadence.
                let send_fresh_ping = match outstanding_ping {
                    Some(sent) if sent.elapsed() >= WS_PONG_RTT_DEGRADED => {
                        slow_pongs += 1;
                        warn!(
                            unanswered_s = sent.elapsed().as_secs(),
                            strikes = slow_pongs,
                            "control WS keepalive ping unanswered past the degradation bound"
                        );
                        if slow_pongs >= WS_PONG_RTT_STRIKES {
                            if slowness_treadmill_holds(slowness_cycles, ws_epoch.elapsed()) {
                                warn!(
                                    unanswered_s = sent.elapsed().as_secs(),
                                    treadmill_cycles = *slowness_cycles,
                                    "control WS degraded but HELD — fresh connections re-throttled repeatedly, cycling is not improving the path; keeping the alive-but-slow WS (deadness still convicts via the rx deadline)"
                                );
                                slow_pongs = 0;
                            } else {
                                close_all_peers(&mut peers, &indicator).await;
                                close_all_tunnel_peers(&mut tunnel_peers).await;
                                park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                                return Err(ConnectError::Transient(anyhow::anyhow!(
                                    "control WS degraded: keepalive ping unanswered for {} s \
                                     ({} strikes) — cycling to a fresh connection",
                                    sent.elapsed().as_secs(),
                                    slow_pongs
                                )));
                            }
                        }
                        true
                    }
                    Some(_) => false, // in flight and inside its window
                    None => true,
                };
                if send_fresh_ping {
                    if let Err(e) =
                        send_frame(&mut ws, Message::Ping(ping_payload(ws_epoch).into())).await
                    {
                        warn!(%e, "keepalive ping failed — will reconnect");
                        close_all_peers(&mut peers, &indicator).await;
                        close_all_tunnel_peers(&mut tunnel_peers).await;
                        park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                        return Err(ConnectError::Transient(e.context("ws ping")));
                    }
                    outstanding_ping = Some(std::time::Instant::now());
                }
                if slow_pongs > 0 {
                    // One strike in: check again quickly instead of waiting
                    // out the full keepalive interval (only the CADENCE
                    // accelerates; each ping keeps its full window).
                    keepalive.reset_after(WS_PING_RETRY_ACCEL);
                } else if netchange_probe.is_some() {
                    // A probe is out: pull the next tick to its deadline.
                    keepalive.reset_after(WS_NETCHANGE_PROBE_DEADLINE);
                }
                // Liveness: a successful keepalive proves the WS pump
                // is healthy even during long quiet periods between
                // sessions. Without this tick the watchdog would flag
                // a stall after 90 s of no inbound traffic.
                watchdog::tick(ctx.pump);
            }
            summary = next_major_netchange(&mut net_rx) => {
                // New network = new throttle regime — the slowness treadmill
                // re-earns its hold verdict from scratch.
                *slowness_cycles = 0;
                // netstate PR-2 — the network materially moved. Probe the
                // WS immediately (one ping, 5 s verdict at the pulled-
                // forward keepalive tick) instead of trusting a socket the
                // transition may have killed for another 45-90 s. A probe
                // already in flight rides — its verdict covers this change.
                if netchange_probe.is_none() {
                    info!(change = %summary, "MAJOR network change — probing the control WS now");
                    if let Err(e) =
                        send_frame(&mut ws, Message::Ping(ping_payload(ws_epoch).into())).await
                    {
                        warn!(%e, "net-change probe ping failed — reconnecting");
                        close_all_peers(&mut peers, &indicator).await;
                        close_all_tunnel_peers(&mut tunnel_peers).await;
                        park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                        return Err(ConnectError::Transient(e.context("net-change probe ping")));
                    }
                    if outstanding_ping.is_none() {
                        outstanding_ping = Some(std::time::Instant::now());
                    }
                    netchange_probe = Some((std::time::Instant::now(), 0));
                    keepalive.reset_after(WS_NETCHANGE_PROBE_DEADLINE);
                }
            }
            _ = heartbeat.tick() => {
                // Wave 3 — tunnel volume the server cannot see for itself
                // (the payload rides the P2P data channel). Cumulative for
                // each forward's life, so the server treats it like the
                // other byte counters: store per bucket, difference on read.
                let tunnel_bytes = tunnel_hub.flows_snapshot().iter().fold(
                    (0u64, 0u64),
                    |(rx, tx), f| {
                        (rx.saturating_add(f.bytes_in), tx.saturating_add(f.bytes_out))
                    },
                );
                // The borrow guards are dropped before the send await.
                let sys = sys_sampler.sample(&overlay_view_tx.borrow(), tunnel_bytes);
                // NAT-traversal health. `None` only while the overlay runtime
                // hasn't published a gather yet — once it has, a measured 0 is
                // reported as 0, which is the value the server actually needs
                // (it means this node can't hole-punch at all).
                let srflx_count = overlay_view_tx
                    .borrow()
                    .srflx
                    .as_ref()
                    .map(|s| s.candidates.len().min(u8::MAX as usize) as u8);
                // C4 stage 2 — advertise the live warm allocation's relayed
                // address so the server holds it pair-less: a peer whose
                // pair to this node dies can dial it WITHOUT a coordination
                // round-trip through this (possibly captured) WS.
                let warm_relay = overlay_view_tx
                    .borrow()
                    .warm_relay
                    .as_ref()
                    .filter(|w| w.state == "live")
                    .and_then(|w| w.relayed.clone());
                // FR-43 P2c — announce capabilities only when they have
                // CHANGED. Caps travel once in `rc:agent.hello`, which is too
                // early for a macOS daemon: it must say hello immediately, and
                // the GUI worker whose permissions it reports attaches later
                // (or never, when nobody is logged in).
                //
                // ⚠️ Sending them every beat would be ~200 bytes of nothing on
                // a frequent message. `None` means "no news", not "no caps".
                let caps_now = delegate.as_ref().and_then(|d| d.effective_permissions());
                let caps = if caps_now == last_announced_permissions {
                    None
                } else {
                    last_announced_permissions = caps_now.clone();
                    // Our own caps, with the worker's permissions substituted:
                    // codecs and encoders stay OURS, because we are the half
                    // that answers `rc:session.request` (the P2b-3 lesson).
                    let mut c = crate::encode::caps::detect();
                    if let Some((perms, has_input)) = caps_now {
                        c.permissions = Some(perms);
                        c.has_input_permission = has_input;
                    }
                    info!(
                        permissions = ?c.permissions,
                        "announcing changed capabilities on the heartbeat (FR-43 P2c)"
                    );
                    Some(Box::new(c))
                };
                let hb = ClientMsg::AgentHeartbeat {
                    rss_mb: sys.rss_mb,
                    cpu_pct: sys.cpu_pct,
                    active_sessions: peers.len().min(u8::MAX as usize) as u8,
                    sys: Some(sys),
                    srflx_count,
                    warm_relay,
                    // FR-27 — cached for 10 min inside, so this is a map lookup
                    // on all but one heartbeat in twenty.
                    companion_version: crate::companion::installed_version(),
                    caps,
                };
                if let Err(e) = send_msg(&mut ws, &hb).await {
                    warn!(%e, "heartbeat send failed — will reconnect");
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                    return Err(ConnectError::Transient(e.context("heartbeat send")));
                }
                // Wave 2 — one `rc:session.stats` per LIVE session, on the
                // heartbeat's own 30 s tick (no extra timer, and it stops
                // by construction when the session map empties). The
                // server folds these into `remote_sessions.stats`, which
                // recorded zeros for every session before this.
                for (session_id, peer) in peers.iter() {
                    let t = match tokio::time::timeout(SESSION_STATS_BUDGET, peer.telemetry())
                        .await
                    {
                        Ok(t) => t,
                        Err(_elapsed) => {
                            warn!(
                                %session_id,
                                "session telemetry timed out — skipping its stats this \
                                 round (wedged ICE after a network transition?)"
                            );
                            watchdog::tick(ctx.pump);
                            continue;
                        }
                    };
                    let msg = ClientMsg::SessionStats {
                        session_id: session_id.to_hex(),
                        bytes_sent: t.bytes_sent,
                        bytes_recv: t.bytes_recv,
                        fps: t.fps,
                        rtt_ms: t.rtt_ms,
                        keyframe_requests: t.keyframe_requests,
                        input_events: t.input_events,
                        shared_seconds: t.shared_seconds,
                        mixed_dial_seconds: t.mixed_dial_seconds,
                    };
                    // Best-effort: telemetry must never tear down a
                    // working session, so a failed send is only logged —
                    // the heartbeat above is what proves liveness.
                    if let Err(e) = send_msg(&mut ws, &msg).await {
                        debug!(%session_id, %e, "session stats send failed");
                        break;
                    }
                }
                watchdog::tick(ctx.pump);
            }
            Some(outbound_msg) = outbound_rx.recv() => {
                if let Err(e) = send_msg(&mut ws, &outbound_msg).await {
                    // A failed control-WS write means the connection is
                    // done — warn-and-continue only deferred the cycle to
                    // the RX deadline (~80 s of dead air), and a WEDGED
                    // send never surfaces on the read side at all. Same
                    // teardown as every other fatal arm.
                    warn!(%e, "failed to flush peer-originated message — will reconnect");
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                    return Err(ConnectError::Transient(e.context("outbound flush")));
                }
                watchdog::tick(ctx.pump);
            }
            Some((session_hex, allow)) = consent_rx.recv() => {
                // FR-27 — an Approve/Deny from the NATIVE panel. Routed
                // through the broker, not applied here, so it passes the same
                // live-prompt gate as a LocalAPI or CLI decision: an answer
                // only counts for a question the broker is actively asking,
                // which is what makes pre-approval unrepresentable.
                let recorded = consent_broker.record_decision(&session_hex, allow);
                info!(
                    session = %session_hex, allow, recorded,
                    "native consent panel decision"
                );
                // Take the panel down either way. If the broker refused it,
                // the prompt it belonged to is already over, and a panel left
                // on screen for a resolved session is its own bug.
                indicator.hide_prompt(&session_hex);
                watchdog::tick(ctx.pump);
            }
            Some(sid) = kill_rx.recv() => {
                // Viewee clicked "Disconnect" in the on-screen overlay.
                // Close the local peer and tell the server, which notifies
                // the browser and echoes ServerMsg::Terminate back; that
                // handler is idempotent, so the echo re-running is safe.
                info!(session_id = %sid, "viewee requested disconnect via overlay badge");
                if let Some(peer) = peers.remove(&sid) {
                    let _ = tokio::time::timeout(PEER_CLOSE_BUDGET, peer.close()).await;
                }
                pending_codecs.remove(&sid);
                pending_transports.remove(&sid);
                pending_audio.remove(&sid);
                pending_permissions.remove(&sid);
                pending_session_meta.remove(&sid);
                let _ = send_msg(
                    &mut ws,
                    &ClientMsg::Terminate {
                        session_id: sid,
                        reason: EndReason::AgentHangup,
                    },
                )
                .await;
                indicator.hide_session(sid.to_hex());
                watchdog::tick(ctx.pump);
            }
            // FR-43 P2b-2 — an rc message the root daemon delegated to us.
            //
            // It runs through the SAME `handle_server_msg` with the SAME
            // session state as our own sessions: the session ids come from the
            // daemon's device row and cannot collide with ours, and everything
            // downstream differs only in the sender the peer is built with.
            //
            // The argument list is duplicated from the `ws.next()` arm below
            // rather than hoisted, because hoisting it would mean restructuring
            // that arm's nested parse/intercept chain — a bigger change than
            // this feature. Divergence is a compile error, so the duplication
            // cannot rot silently.
            Some(inbound) = next_delegated(&mut delegated_in) => {
                watchdog::tick(ctx.pump);
                let delegated_msg = match inbound {
                    // The parameters the daemon resolved, arriving ahead of the
                    // offer that consumes them. Populating the SAME maps the
                    // `Request` arm would have means the `SdpOffer` handler
                    // needs no delegation-specific branch at all.
                    crate::delegate::WorkerInbound::Params(p) => {
                        match bson::oid::ObjectId::parse_str(&p.session_id) {
                            Ok(sid) => {
                                pending_codecs.insert(sid, p.codec);
                                pending_transports.insert(sid, p.transport);
                                pending_chroma.insert(sid, p.chroma);
                                pending_chunk_framing.insert(sid, p.chunk_framing);
                                pending_audio.insert(sid, p.audio);
                                pending_permissions.insert(sid, p.permissions);
                                pending_session_meta.insert(
                                    sid,
                                    (p.controller_name, p.input_mode, p.asking_org),
                                );
                                info!(session_id = %sid, "delegation: session params received");
                            }
                            Err(e) => {
                                warn!(%e, sid = %p.session_id, "delegation: unparseable session id");
                            }
                        }
                        continue;
                    }
                    crate::delegate::WorkerInbound::Msg(m) => *m,
                };
                info!(
                    kind = crate::delegate::server_msg_kind(&delegated_msg),
                    "delegation: serving a delegated rc message"
                );
                handle_server_msg(
                    ctx,
                    &cfg.tenant_id,
                    &mut ws,
                    delegated_msg,
                    &mut peers,
                    &mut pending_codecs,
                    &mut pending_transports,
                    &mut pending_chroma,
                    &mut pending_chunk_framing,
                    &mut pending_audio,
                    &mut pending_permissions,
                    &mut pending_session_meta,
                    &mut tunnel_peers,
                    &mut tunnel_quic_peers,
                    &outbound_tx,
                    encoder_preference,
                    &indicator,
                    &consent_broker,
                    &cfg.forward_acl,
                    &relay_regions_slot,
                    &derp_ticket_slot,
                    cfg,
                    remote_cfg,
                    delegate.as_ref(),
                    delegated_out.as_ref(),
                )
                .await?;
            }
            maybe_msg = ws.next() => match maybe_msg {
                Some(Ok(Message::Text(text))) => {
                    last_rx = std::time::Instant::now();
                    watchdog::tick(ctx.pump);
                    match serde_json::from_str::<ServerMsg>(&text) {
                        Ok(parsed) => {
                            // Phase 3b: route `rc:overlay.*` to the node
                            // runtime; everything else falls through to the
                            // normal dispatch below.
                            #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
                            let parsed = match &overlay_evt_tx {
                                Some(tx) => match crate::overlay::intercept(tx, parsed, ctx.is_primary) {
                                    Some(p) => p,
                                    None => continue,
                                },
                                None => parsed,
                            };
                            // P3b-2 PR-C: route client-bound tunnel replies to
                            // this daemon's originated flows; everything else
                            // (incl. the target-side tunnel messages) falls
                            // through to `handle_server_msg` unchanged.
                            let parsed = match crate::tunnel::client_mgr::intercept_server_msg(
                                &tunnel_hub,
                                parsed,
                            ) {
                                Some(p) => p,
                                None => continue,
                            };
                            handle_server_msg(
                                ctx,
                                &cfg.tenant_id,
                                &mut ws,
                                parsed,
                                &mut peers,
                                &mut pending_codecs,
                                &mut pending_transports,
                                &mut pending_chroma,
                                &mut pending_chunk_framing,
                                &mut pending_audio,
                                &mut pending_permissions,
                                &mut pending_session_meta,
                                &mut tunnel_peers,
                                &mut tunnel_quic_peers,
                                &outbound_tx,
                                encoder_preference,
                                &indicator,
                                &consent_broker,
                                &cfg.forward_acl,
                                &relay_regions_slot,
                                &derp_ticket_slot,
                                cfg,
                                remote_cfg,
                                delegate.as_ref(),
                                // A message off our OWN control WS is never a
                                // delegated one — its replies belong here.
                                None,
                            )
                            .await?;
                        }
                        // A parse failure has TWO causes that this arm used to
                        // report as one, and the difference is the difference
                        // between noise and an outage.
                        //
                        // Something that is not ours at all (a proxy's keepalive,
                        // a future frame family) is genuinely ignorable — debug.
                        //
                        // But a frame whose `t` starts with `rc:` IS ours and
                        // failed to decode, which means the server sent
                        // something this build cannot read and the frame is
                        // being dropped whole. For an `rc:overlay.netmap` that
                        // is the entire mesh: no address, no peers, and — at
                        // debug — no sign of it. That is not hypothetical: a
                        // REQUIRED field the client never reads (`epoch`) is
                        // enough to trigger it, and the message would have
                        // claimed the frame was "non-rc:*" while naming an
                        // `rc:` tag in its own payload.
                        //
                        // Deliberately not fatal: dropping one frame is still
                        // better than exiting, and an old agent meeting a new
                        // frame family must keep running. It just has to SAY so.
                        Err(e) => {
                            let tag = serde_json::from_str::<serde_json::Value>(&text)
                                .ok()
                                .and_then(|v| v.get("t")?.as_str().map(str::to_string));
                            match tag {
                                Some(t) if t.starts_with("rc:") => warn!(
                                    %e, %t,
                                    "DROPPED a roomler frame this build cannot decode — the \
                                     server sent a shape we do not understand; anything it \
                                     carried (a netmap, a session) did not arrive"
                                ),
                                _ => debug!(%e, text = %text.as_str(), "ignoring non-rc:* frame"),
                            }
                        }
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    last_rx = std::time::Instant::now();
                    if let Err(e) = send_frame(&mut ws, Message::Pong(data)).await {
                        warn!(%e, "pong reply failed — will reconnect");
                        close_all_peers(&mut peers, &indicator).await;
                        close_all_tunnel_peers(&mut tunnel_peers).await;
                        park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                        return Err(ConnectError::Transient(e.context("ws pong")));
                    }
                    watchdog::tick(ctx.pump);
                }
                Some(Ok(Message::Close(_))) | None => {
                    info!("ws closed by peer");
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                    return Ok(());
                }
                Some(Err(e)) => {
                    close_all_peers(&mut peers, &indicator).await;
                    close_all_tunnel_peers(&mut tunnel_peers).await;
                    park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                    return Err(ConnectError::Transient(anyhow::Error::new(e).context("ws read")));
                }
                Some(Ok(Message::Pong(payload))) => {
                    last_rx = std::time::Instant::now();
                    outstanding_ping = None;
                    // Zombie-slow WS detector (see [`WS_PONG_RTT_DEGRADED`]):
                    // a pong echoing one of OUR stamped pings measures the
                    // true application round trip. Two consecutive verdicts
                    // over the bound mean the path is unusable for the
                    // request/reply traffic that rides it (relay grants,
                    // exec) — cycle to a fresh connection; the persistent
                    // overlay runtime rides through WS reconnects untouched.
                    if let Some(rtt) = pong_rtt(&payload, ws_epoch) {
                        if rtt >= WS_PONG_RTT_DEGRADED {
                            slow_pongs += 1;
                            warn!(
                                rtt_s = rtt.as_secs(),
                                strikes = slow_pongs,
                                "control WS pong RTT degraded"
                            );
                            if slow_pongs >= WS_PONG_RTT_STRIKES {
                                if slowness_treadmill_holds(slowness_cycles, ws_epoch.elapsed()) {
                                    warn!(
                                        rtt_s = rtt.as_secs(),
                                        treadmill_cycles = *slowness_cycles,
                                        "control WS degraded but HELD — fresh connections re-throttled repeatedly, cycling is not improving the path; keeping the alive-but-slow WS (deadness still convicts via the rx deadline)"
                                    );
                                    slow_pongs = 0;
                                } else {
                                    close_all_peers(&mut peers, &indicator).await;
                                    close_all_tunnel_peers(&mut tunnel_peers).await;
                                    park_survived_quic_peers(&mut tunnel_quic_peers, &cfg.tenant_id).await;
                                    return Err(ConnectError::Transient(anyhow::anyhow!(
                                        "control WS degraded: pong rtt {} s ({} consecutive over \
                                         the {} s bound) — cycling to a fresh connection",
                                        rtt.as_secs(),
                                        slow_pongs,
                                        WS_PONG_RTT_DEGRADED.as_secs()
                                    )));
                                }
                            }
                        } else {
                            slow_pongs = 0;
                        }
                    }
                    watchdog::tick(ctx.pump);
                }
                // Binary / raw frames — any inbound frame proves the link
                // is alive.
                Some(Ok(_)) => {
                    last_rx = std::time::Instant::now();
                }
            }
        }
    }
}

/// Tell the server what this device did with a pushed desired-config
/// (`docs/remote-config.md`).
///
/// Fire-and-forget, mirroring `ssh::ActivitySink::report` and for the same
/// reason: a full outbound queue means the connection is already struggling,
/// and losing a status line must never be allowed to stall the WS receive path
/// that produced it. The daemon log always has the same information.
///
/// `detail` is redacted and capped BEFORE it leaves the host — it is an error
/// string we did not compose, so it gets the same treatment as any other text
/// this daemon ships off the machine.
fn report_config_status(
    outbound_tx: &mpsc::Sender<ClientMsg>,
    revision: u64,
    outcome: roomler_ai_remote_control::models::ConfigOutcome,
    detail: Option<&str>,
) {
    use roomler_ai_remote_control::models::ConfigReport;

    let detail = detail.map(|d| {
        let mut s = crate::exec::redactor().apply(d);
        if s.chars().count() > ConfigReport::MAX_DETAIL {
            s = s.chars().take(ConfigReport::MAX_DETAIL).collect();
            s.push('…');
        }
        s
    });
    let msg = ClientMsg::ConfigStatus {
        revision,
        outcome,
        live: Vec::new(),
        needs_restart: Vec::new(),
        detail,
    };
    if outbound_tx.try_send(msg).is_err() {
        debug!("rc:agent.config_status dropped (outbound queue full or closed)");
    }
}

/// FR-40 — the device's answer to `rc:agent.key_rotate`, on every outcome.
/// Same fire-and-forget rule as [`report_config_status`]; `detail` is
/// redacted and capped before it leaves the host. Only PUBLIC keys are ever
/// passed here — the frame has no field for anything else, by construction.
fn report_key_rotated(
    outbound_tx: &mpsc::Sender<ClientMsg>,
    request_id: &str,
    outcome: roomler_ai_remote_control::models::KeyRotationOutcome,
    old_public_key: Option<&str>,
    new_public_key: Option<&str>,
    key_epoch: u32,
    detail: Option<&str>,
) {
    use roomler_ai_remote_control::models::KeyRotationReport;

    let detail = detail.map(|d| {
        let mut s = crate::exec::redactor().apply(d);
        if s.chars().count() > KeyRotationReport::MAX_DETAIL {
            s = s.chars().take(KeyRotationReport::MAX_DETAIL).collect();
            s.push('…');
        }
        s
    });
    let msg = ClientMsg::KeyRotated {
        request_id: request_id.to_string(),
        outcome,
        old_public_key: old_public_key.map(str::to_string),
        new_public_key: new_public_key.map(str::to_string),
        key_epoch,
        detail,
    };
    if outbound_tx.try_send(msg).is_err() {
        debug!("rc:agent.key_rotated dropped (outbound queue full or closed)");
    }
}

/// FR-40 P1c — a `rotated` report to re-send on the NEXT session of the org
/// that rotated. The first copy rides the session that is about to end and
/// can be lost — in the second field run the server never received it and
/// resolved `delivered` for a rotation it had itself verified at the join.
/// The re-send rides a healthy socket; the server's conditional write makes
/// the duplicate harmless.
struct PendingRotationReport {
    request_id: String,
    old_public_key: Option<String>,
    new_public_key: String,
    key_epoch: u32,
}

/// Keyed per org (tenant id): every org loop lives in this one daemon and
/// must only re-send its OWN rotation.
fn pending_rotation_reports() -> &'static std::sync::Mutex<HashMap<String, PendingRotationReport>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<HashMap<String, PendingRotationReport>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn drain_pending_rotation_report(tenant_id: &str, outbound_tx: &mpsc::Sender<ClientMsg>) {
    let pending = pending_rotation_reports()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(tenant_id);
    if let Some(p) = pending {
        info!(
            request_id = %p.request_id,
            key_epoch = p.key_epoch,
            "rc:agent.key_rotated — re-sending the rotation report on the new session"
        );
        report_key_rotated(
            outbound_tx,
            &p.request_id,
            roomler_ai_remote_control::models::KeyRotationOutcome::Rotated,
            p.old_public_key.as_deref(),
            Some(&p.new_public_key),
            p.key_epoch,
            None,
        );
    }
}

/// FR-40 — the device's own floor between two rotations of ONE org's key.
/// The server enforces the same ceiling; this one survives a server that
/// does not.
const KEY_ROTATION_MIN_INTERVAL: Duration = Duration::from_secs(60);

/// `true` (and stamps now) when no rotation for `tenant_id` ran inside
/// [`KEY_ROTATION_MIN_INTERVAL`]. Process-wide, keyed per org, since every
/// org loop lives in this one daemon.
fn key_rotation_ceiling_ok(tenant_id: &str) -> bool {
    static LAST: std::sync::OnceLock<std::sync::Mutex<HashMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    let m = LAST.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut last = m.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    if let Some(prev) = last.get(tenant_id)
        && now.duration_since(*prev) < KEY_ROTATION_MIN_INTERVAL
    {
        return false;
    }
    last.insert(tenant_id.to_string(), now);
    true
}

#[allow(clippy::too_many_arguments)]
async fn handle_server_msg(
    ctx: &OrgCtx,
    // R4 — this org's tenant id (OrgCtx carries only the label); the
    // quic-derp-v1 setup arm resolves its DERP mux by tenant.
    tenant_id: &str,
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    msg: ServerMsg,
    peers: &mut HashMap<bson::oid::ObjectId, AgentPeer>,
    pending_codecs: &mut HashMap<bson::oid::ObjectId, String>,
    pending_transports: &mut HashMap<bson::oid::ObjectId, Option<String>>,
    pending_chroma: &mut HashMap<bson::oid::ObjectId, Option<String>>,
    pending_chunk_framing: &mut HashMap<bson::oid::ObjectId, bool>,
    pending_audio: &mut HashMap<bson::oid::ObjectId, bool>,
    pending_permissions: &mut HashMap<
        bson::oid::ObjectId,
        roomler_ai_remote_control::permissions::Permissions,
    >,
    pending_session_meta: &mut HashMap<
        bson::oid::ObjectId,
        (
            String,
            Option<roomler_ai_remote_control::models::InputMode>,
            // FR-27 follow-up — the asking org, carried so the "Being viewed
            // by" banner can be raised at peer-build time (not request time).
            Option<String>,
        ),
    >,
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
    relay_regions_slot: &crate::relay_probe::RegionSlot,
    derp_ticket_slot: &crate::relay_probe::DerpTicketSlot,
    // Multi-org: this loop's OWN enrollment — the server_url + machine
    // identity a pushed `rc:agent.join_org` enrolls with. Never a value
    // taken off the wire.
    agent_cfg: &AgentConfig,
    remote_cfg: &crate::remote_config::RemoteConfigServices,
    // FR-43 P2b — the macOS GUI-worker channel, when one is attached. `None`
    // on every other platform and whenever no worker has attached, and the
    // delegation branch below is then unreachable.
    delegate: Option<&crate::delegate::DelegateHost>,
    // FR-43 P2b-2 — set when THIS message was delegated to us by the root
    // daemon, in which case the session's replies belong on the delegation
    // channel and not on our own control WS. `None` is every ordinary session.
    delegated: Option<&mpsc::Sender<ClientMsg>>,
) -> Result<(), ConnectError> {
    // FR-43 P2b — hand a remote-desktop session to the GUI worker, if there is
    // one. This sits ahead of the whole dispatch rather than inside each of
    // the five handlers: one place to read, one place to audit, and no way for
    // a handler to be added later that quietly forgets to check.
    //
    // ⚠️ `send_to_worker` returning false is NOT an error — it means no worker
    // is attached (or its queue is full), and the message then falls through
    // to the local handlers exactly as before. On a root macOS daemon that
    // local path cannot serve pixels, which is the whole reason P2 exists; but
    // failing SOFT is still right, because the alternative is a session that
    // vanishes rather than one that fails the way it always has.
    //
    // ⚠️ PRIMARY ORG ONLY. The GUI worker is host-global — one screen, one
    // keyboard — while the server models policy per org, so a secondary org's
    // admin must not be able to drive it. Same rule, and the same reason, as
    // `rc:agent.update`, exec and the FR-19 relay: a host-global resource
    // belongs to the enrollment that owns the host.
    if let Some(delegate) = delegate
        && ctx.is_primary
        && crate::delegate::delegable_inbound(&msg)
        && delegate.send_to_worker(&msg)
    {
        tracing::debug!(
            kind = crate::delegate::server_msg_kind(&msg),
            "delegated an rc message to the GUI worker"
        );
        return Ok(());
    }

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
            chunk_framing,
            audio_enabled,
            consent_mode,
            host_prompt_timeout_secs,
            input_mode,
            tenant_name,
        } => {
            // Multi-org — WHICH organization is asking. The server's display
            // name when it sent one; otherwise this loop's own org label,
            // which at least distinguishes secondaries.
            //
            // Shown ONLY on a daemon that actually serves more than one
            // enrollment. The line exists to disambiguate, and on a
            // single-org device there is nothing to disambiguate — every
            // request necessarily comes from the one org, so an "On behalf
            // of …" row would be pure chrome on the majority of installs.
            // A secondary loop is multi-org by definition; the primary
            // checks whether any `[[orgs]]` entry rides alongside it (its
            // config keeps them — only the SYNTHESIZED per-org config has
            // `orgs` cleared).
            let multi_org = !ctx.is_primary || !agent_cfg.orgs.is_empty();
            let asking_org: Option<String> = multi_org
                .then(|| {
                    tenant_name
                        .clone()
                        .or_else(|| (!ctx.is_primary).then(|| ctx.label.clone()))
                })
                .flatten();
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
            pending_chunk_framing.insert(session_id, chunk_framing.unwrap_or(false));
            // Opt-in system audio. Only honour the controller's request
            // when the agent actually advertises an audio codec
            // (`AgentCaps.audio` non-empty ⇒ the `audio` feature is
            // compiled in and cpal/opus are available). Otherwise store
            // `false` so the SdpOffer handler never tries to add a track
            // this build can't feed — matches the transport intersection
            // above.
            let audio_negotiated = audio_enabled && !our_caps.audio.is_empty();
            pending_audio.insert(session_id, audio_negotiated);
            // Clipboard-v2 hardening — stash the session's permission
            // bitfield so the SdpOffer handler can hand it to the
            // AgentPeer's DC handlers for enforcement.
            pending_permissions.insert(session_id, permissions);
            // P6 — controller name (participants rail / ghost labels) +
            // the device policy's input arbitration mode.
            pending_session_meta.insert(
                session_id,
                (controller_name.clone(), input_mode, asking_org.clone()),
            );
            // FR-43 P2b-3 — hand the SAME resolved values to the GUI worker, if
            // one is attached. `Request` is not delegated (consent and the
            // upstream reply belong to the daemon, which is the enrolled
            // identity), but everything it resolves here is consumed by the
            // `SdpOffer` handler — which IS delegated. Without this the worker
            // defaults all seven, and the one that matters silently is
            // `transport`: `None` means the legacy RTP track, so a browser that
            // negotiated `data-channel-h264` gets a black screen while the
            // agent happily encodes into a pipe nobody reads.
            if let Some(delegate) = delegate
                && ctx.is_primary
            {
                let sent = delegate.send_params(crate::delegate::DelegateFrame::SessionParams(
                    Box::new(crate::delegate::SessionParams {
                        session_id: session_id.to_hex(),
                        codec: chosen.clone(),
                        transport: negotiated_transport.clone(),
                        chroma: chroma_pref.clone(),
                        chunk_framing: chunk_framing.unwrap_or(false),
                        audio: audio_negotiated,
                        permissions,
                        controller_name: controller_name.clone(),
                        input_mode,
                        asking_org: asking_org.clone(),
                    }),
                ));
                if !sent {
                    // No worker, or its queue is full. Not fatal — the daemon
                    // still serves the session itself, which on macOS means a
                    // blank screen but a working, terminable session.
                    tracing::debug!(%session_id, "no worker to hand session params to");
                }
            }
            info!(
                %session_id, %controller_user_id, %controller_name,
                ?permissions, consent_timeout_secs,
                browser_caps = ?browser_caps,
                chosen_codec = %chosen,
                requested_transport = ?preferred_transport,
                negotiated_transport = ?negotiated_transport,
                chroma_pref = ?chroma_pref,
                audio_requested = audio_enabled,
                audio_negotiated,
                consent_mode = ?consent_broker.mode(),
                org = ?asking_org,
                "incoming session request — running consent broker"
            );
            // FR-27 follow-up (field, 2026-08-30) — the "Being viewed by …"
            // banner + LocalAPI session registry publish are NO LONGER raised
            // here, at request time. They used to go up alongside the consent
            // prompt as defence-in-depth, but "Being viewed by X" is FALSE
            // while consent is still pending — nobody is viewing yet — and next
            // to a "Remote control request" it reads as a contradiction. The
            // `show_session_full` call now fires at PEER-BUILD (`rc:sdp.offer`
            // handler, below), so the banner + registry entry appear only once
            // the session is actually established and pixels flow. That covers
            // every mode uniformly: auto-grant reaches peer-build within ~1 ms
            // (the banner still appears at once, its only defence-in-depth
            // case), while prompt / email / push reach it only after approval.
            // `asking_org` is stashed in `pending_session_meta` for that call.
            // Spawn a task to run the broker decision in the
            // background; auto-grant resolves <1ms, prompt mode can
            // take up to 30s — we MUST NOT block the WS read loop.
            // Decision flows back via outbound_tx as a ClientMsg::Consent.
            // Phase 2 — obey the server's per-session consent directive when
            // present; fall back to the broker's startup mode (local
            // `auto_grant_session`) for an older server that sends none.
            //
            // FR-27 — the ON-HOST window is `host_prompt_timeout_secs`, not
            // `consent_timeout_secs`. They coincide for a plain attended
            // prompt and diverge for `prompt_then_email`, whose session waits
            // minutes for the owner's emailed link while the modal on this
            // screen has no business standing there that long. An older server
            // sends no split, and then the two are the same thing — the
            // pre-FR-27 behaviour, byte for byte.
            let host_window = std::time::Duration::from_secs(
                host_prompt_timeout_secs.unwrap_or(consent_timeout_secs) as u64,
            );
            let directed_mode: Option<crate::consent::Mode> = consent_mode.map(|m| match m {
                roomler_ai_remote_control::models::ConsentMode::Auto => {
                    crate::consent::Mode::AutoGrant
                }
                // Prompt + the async owner-side channels (Email / Push /
                // PromptThenEmail) all resolve to an on-host prompt at the
                // agent: the server drives the owner channels itself (Phase 4)
                // and asks the agent to prompt as the on-console path/fallback.
                _ => crate::consent::Mode::Prompt {
                    timeout: host_window,
                },
            });
            // FR-27 — resolve directive AGAINST the local setting, taking the
            // stricter. This used to be `directed_mode.unwrap_or(local)`, i.e.
            // the directive simply won — so a device that had set
            // `auto_grant_session = false` was silently overridden by a server
            // `Auto`, inverting the gate-4 property exec and SSH are built on.
            let effective_mode =
                crate::consent::strictest_of(directed_mode, consent_broker.mode(), host_window);
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
            //
            // FR-27 — whether the marker landed is REMEMBERED. It is the only
            // prompt surface this daemon has besides the CLI, so a failure to
            // write it means no human can ever be asked, and that has to reach
            // the controller as "nobody could be asked" rather than as a deny.
            let will_prompt = !owner_side_consent
                && matches!(effective_mode, crate::consent::Mode::Prompt { .. });
            let mut have_surface = true;
            if will_prompt {
                // FR-27 — NATIVE surface first, companion as the fallback.
                //
                // The native panel is drawn by the daemon itself, so it needs
                // no second process running, no login-session plumbing and no
                // IPC. It exists on Windows (capture-excluded), on X11, and on
                // macOS; it does NOT exist on a headless host, under GNOME/KDE
                // Wayland (neither exposes `wlr-layer-shell` to arbitrary
                // clients), or in a build without the per-OS feature — and
                // `show_prompt` says which by returning false.
                let native = indicator.show_prompt(crate::indicator::PromptView {
                    session_hex: session_hex.clone(),
                    title: "Remote control request".into(),
                    lead: format!("{controller_name} is requesting to control this device."),
                    detail: String::new(),
                    permissions: permissions.wire_names(),
                    org: asking_org.clone().unwrap_or_default(),
                    expires_at: std::time::Instant::now() + host_window,
                });
                let prompt = crate::consent::PendingPrompt {
                    kind: crate::consent::PromptKind::RemoteControl,
                    asked_by: &controller_name,
                    permissions: permissions.wire_names(),
                    detail: String::new(),
                    // The marker is written EITHER WAY — it is the
                    // machine-readable record that a decision is outstanding,
                    // and `roomlerd consent --list` must show a
                    // natively-prompted session too. This field is what stops
                    // the companion from ALSO popping a panel and asking the
                    // same question twice.
                    surface: if native {
                        crate::consent::PromptSurface::Native
                    } else {
                        crate::consent::PromptSurface::Companion
                    },
                    // Multi-org — the desktop modal renders this so the
                    // operator can tell WHICH organization is asking. On a
                    // device enrolled in two orgs, "Alice wants to control
                    // this machine" is not enough to decide on. Absent for a
                    // single-org device (nothing useful to add).
                    org: asking_org.clone().unwrap_or_default(),
                    timeout: host_window,
                };

                if let Err(e) = consent_broker.write_prompt(&session_hex, &prompt) {
                    // Only fatal to the SURFACE when the companion was the
                    // plan: with a native panel already up, a human can still
                    // answer — the marker's loss costs the CLI listing, not
                    // the prompt.
                    if !native {
                        have_surface = false;
                    }
                    tracing::warn!(session = %session_hex, %e, native, "could not write the .pending consent marker");
                }

                if !native {
                    // FR-27 — the marker only helps if something is READING it.
                    // Nothing used to start the companion, so a device set to
                    // "Prompt on host" whose operator had quit the menu-bar app
                    // showed nothing, waited out its window, and reported a deny.
                    //
                    // AWAITED, not spawned: its verdict is what separates
                    // "nobody answered" from "there is nobody to ask, and there
                    // never was" — two outcomes with different fixes, and the
                    // caller cannot tell them apart afterwards. Bounded so a
                    // wedged `launchctl` / `systemd-run` cannot eat the prompt
                    // window; a timeout means we could not confirm a surface,
                    // which is the same answer as not having one.
                    let companion_up = tokio::time::timeout(
                        COMPANION_START_BUDGET,
                        crate::companion::ensure_running(),
                    )
                    .await
                    .unwrap_or(false);
                    if !companion_up {
                        have_surface = false;
                    }
                }
                info!(
                    session = %session_hex, native, have_surface,
                    "consent prompt surface"
                );
                // FR-34 — a LOCKED host has the panel on the (currently
                // invisible) secure desktop, so the operator must UNLOCK the
                // machine before they can see and approve it. That is the
                // sound flow (unlock proves presence, then approve; 5-minute
                // window since P4) — but the controller has no way to know it
                // is locked, so its "awaiting consent" wait looks like a hang.
                // Tell it. Advisory + best-effort; `probe_lock_state()` is
                // Unlocked on every non-Windows host, so this only fires where
                // a lock screen exists.
                // FR-34 P3b — detect the lock from the SERVICE context.
                // This consent code runs on the SCM service window
                // station (the daemon only switches to WinSta0
                // per-session, when input is wired, AFTER consent), so
                // the `OpenInputDesktop`-based `probe_lock_state()` reads
                // the service station's own `Default` and ALWAYS reports
                // Unlocked here — the emit could never fire on a
                // perMachine/SYSTEM host (field-confirmed on CORPLAP-1,
                // 0.4.22). `probe_lock_state_service()` asks WTS about the
                // console session directly, from any station; fall back to
                // the desktop probe (correct for a perUser/attended daemon
                // already on WinSta0) when WTS is unavailable / UNKNOWN.
                let host_locked = crate::lock_state::probe_lock_state_service()
                    .unwrap_or_else(crate::lock_state::probe_lock_state)
                    == crate::lock_state::LockState::Locked;
                if host_locked {
                    info!(
                        session = %session_hex,
                        "consent prompt on a LOCKED host — signalling the controller to unlock + approve"
                    );
                    let _ = outbound_tx.try_send(ClientMsg::ConsentPending {
                        session_id,
                        host_locked: true,
                    });
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
                let ind = indicator.clone();
                tokio::spawn(async move {
                    let decision = broker.request_with_mode(&session_hex, effective_mode).await;
                    // FR-27 — take the native panel down however the question
                    // was answered: the click here, the CLI, the companion, or
                    // the window simply expiring. This is the ONE place every
                    // one of those paths converges, so it is the only place
                    // that cannot miss one — a panel still on screen for a
                    // resolved session is its own bug, and a stale Approve
                    // button is a dangerous one.
                    ind.hide_prompt(&session_hex);
                    let granted = decision.granted();
                    // FR-27 — say WHY, when the answer is no. A bare `false`
                    // reaches the controller as "the user denied your request",
                    // which is a lie whenever the truth is that the prompt
                    // stood unanswered, or that nothing could raise one.
                    let reason = match decision {
                        crate::consent::Decision::Granted => None,
                        crate::consent::Decision::Denied => None,
                        crate::consent::Decision::Timeout if !have_surface => Some(
                            roomler_ai_remote_control::consent::ConsentDenyReason::NoPromptSurface,
                        ),
                        crate::consent::Decision::Timeout => {
                            Some(roomler_ai_remote_control::consent::ConsentDenyReason::HostTimeout)
                        }
                    };
                    tracing::info!(
                        session = %session_hex,
                        ?decision,
                        ?effective_mode,
                        granted,
                        reason = reason.map(|r| r.wire()),
                        "consent decision → sending rc:consent"
                    );
                    if let Err(e) = outbound
                        .send(ClientMsg::Consent {
                            session_id,
                            granted,
                            reason: reason.map(|r| r.wire().to_string()),
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
                let _ = tokio::time::timeout(PEER_CLOSE_BUDGET, old.close()).await;
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
            // `ROOMLERD_VP9_CHROMA` env-var default.
            let chroma_pref = pending_chroma.remove(&session_id).unwrap_or(None);
            // FR-17 — absent (older controller, or a test harness that skipped
            // rc:request) means the legacy unframed format, which is the only
            // safe default: framing bytes a peer cannot parse is unrecoverable.
            let chunk_framing = pending_chunk_framing.remove(&session_id).unwrap_or(false);
            // Pull the opt-in audio flag stashed by the Request handler.
            // Missing (session skipped rc:request in a test harness) →
            // no audio track, the safe default.
            let audio_enabled = pending_audio.remove(&session_id).unwrap_or(false);
            // Pull the session permissions stashed by the Request
            // handler. Missing (harness skipped rc:request) → the
            // Permissions default (VIEW | INPUT | CLIPBOARD) so those
            // flows behave exactly as pre-v2.
            let permissions = pending_permissions.remove(&session_id).unwrap_or_default();
            // P6 — controller name + policy input mode for the arbiter
            // registration. Missing (harness skipped rc:request) → an
            // anonymous label + the agent-default (free) mode.
            let (controller_name, input_mode, asking_org) = pending_session_meta
                .remove(&session_id)
                .unwrap_or_else(|| ("Controller".to_string(), None, None));
            // FR-27 follow-up — capture what the "Being viewed by" banner needs
            // BEFORE `controller_name` is moved into `AgentPeer::new` below.
            // The banner is raised once the peer is established (after
            // `handle_offer`), not at request time. `permissions` is Copy.
            let banner_name = controller_name.clone();
            let banner_org = asking_org.unwrap_or_default();

            let peer = match AgentPeer::new(
                session_id,
                &ice_servers,
                // FR-43 P2b-2 — THE seam. Everything this peer later emits
                // (ICE, `SessionStats`, its own `Terminate`) leaves through
                // the sender it is constructed with, so a delegated session
                // needs exactly one decision, taken once, here. Nothing
                // downstream has to know it is delegated.
                delegated.unwrap_or(outbound_tx).clone(),
                encoder_preference,
                chosen_codec,
                negotiated_transport,
                chroma_pref,
                chunk_framing,
                audio_enabled,
                permissions,
                controller_name,
                input_mode.map(|m| {
                    match m {
                        roomler_ai_remote_control::models::InputMode::Free => "free",
                        roomler_ai_remote_control::models::InputMode::Exclusive => "exclusive",
                    }
                    .to_string()
                }),
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    warn!(%session_id, %e, "AgentPeer::new failed; terminating");
                    let _ = reply_for_session(
                        ws,
                        delegated,
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
                    let _ = tokio::time::timeout(PEER_CLOSE_BUDGET, peer.close()).await;
                    let _ = reply_for_session(
                        ws,
                        delegated,
                        &ClientMsg::Terminate {
                            session_id,
                            reason: EndReason::Error,
                        },
                    )
                    .await;
                    return Ok(());
                }
            };

            // FR-27 follow-up — the session is now established (peer built,
            // answer ready): raise the "Being viewed by …" banner and publish
            // to the LocalAPI session registry. Deferred to here from request
            // time so it never shows while consent is still pending — "Being
            // viewed by X" is only true once pixels actually flow. Harmless
            // no-op on non-Windows / when the indicator feature is disabled.
            indicator.show_session_full(
                session_id,
                banner_name,
                permissions.wire_names(),
                banner_org,
            );

            reply_for_session(
                ws,
                delegated,
                &ClientMsg::SdpAnswer {
                    session_id,
                    sdp: answer_sdp,
                },
            )
            .await
            .map_err(|e| ConnectError::Transient(e.context("sending answer")))?;
            peers.insert(session_id, peer);
            // 2026-07-27 — first live session engages the GPU-clock pin
            // (opt-in, `ROOMLERD_GPU_CLOCK_PIN`; no-op otherwise).
            crate::gpu_clock::on_sessions_changed(peers.len());
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

        // ⚠️ `WARN [] failed to handle_inbound: ErrChunk` immediately BEFORE
        // this line is EXPECTED and benign — it is the teardown, not a fault.
        // The controller hangs up, DTLS goes away, and a final in-flight SCTP
        // packet fails to parse on the way out. Measured on a macOS host
        // 2026-08-24: 14/14 occurrences were followed by this `Terminate`
        // (`reason=ControllerHangup`) within ~25 ms, and NO session ended
        // without one. It is emitted by `webrtc_sctp::association` with an
        // EMPTY name (`[]`), so it carries no `session_id` and reads as
        // unattributable — which is exactly why it invites a wrong story.
        // (It cost one: "sessions die at ~7 s" — they did not; those sessions
        // were disconnected by hand while a healthy one in the same log ran to
        // 8160 frames / 75 MB.)
        //
        // ⚠️ Do NOT "fix" it by filtering `webrtc_sctp::association` — a
        // grep for ErrChunk otherwise lands only on the CHUNKING story
        // (`clipboard.rs` rc.44, `useRemoteControl.ts` rc.23: a single SCTP
        // message ≥ the 65536 `max_message_size` default), and that same warn
        // was the ONLY signal for both of those real bugs, which dropped data
        // silently otherwise. Mid-session it means something; here it does not.
        // Distinguish by what follows: a teardown line, or nothing.
        ServerMsg::Terminate { session_id, reason } => {
            info!(%session_id, ?reason, "session terminated by server");
            if let Some(peer) = peers.remove(&session_id) {
                let _ = tokio::time::timeout(PEER_CLOSE_BUDGET, peer.close()).await;
            }
            // 2026-07-27 — last session gone → release the GPU-clock pin
            // (Drop resets the locked clocks).
            crate::gpu_clock::on_sessions_changed(peers.len());
            // Drop any orphaned pending-codec / transport entry for
            // this session so the maps don't accumulate under long-
            // running agents (e.g. sessions cancelled before SDP is
            // exchanged).
            pending_codecs.remove(&session_id);
            pending_transports.remove(&session_id);
            pending_audio.remove(&session_id);
            pending_permissions.remove(&session_id);
            pending_session_meta.remove(&session_id);
            indicator.hide_session(session_id.to_hex());
        }

        ServerMsg::Error {
            session_id,
            code,
            message,
            open_nonce: _,
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
            pending_chroma.clear();
            pending_chunk_framing.clear();
            pending_audio.clear();
            pending_permissions.clear();
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

        // Multi-region relay PoPs: stash the pushed region list; probe now
        // when the set actually changed (rev), else let the periodic
        // re-prober use the stored copy.
        ServerMsg::RelayRegions { regions, rev } => {
            // Multi-region DERP: any region carrying a derp_url ⇒ fetch an
            // admission ticket (once; the reply arm schedules refreshes).
            // Only when this build can actually dial regional relays.
            #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
            if regions.iter().any(|r| r.derp_url.is_some())
                && tunnel_core::overlay::direct::derp_enabled()
                && derp_ticket_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
            {
                let _ = outbound_tx.try_send(ClientMsg::DerpTicketRequest {});
            }
            if crate::relay_probe::probing_enabled() {
                let changed = {
                    let mut slot = relay_regions_slot.lock().unwrap_or_else(|e| e.into_inner());
                    let changed = slot.as_ref().map(|(_, r)| *r != rev).unwrap_or(true);
                    *slot = Some((regions.clone(), rev));
                    changed
                };
                if changed {
                    debug!(count = regions.len(), rev, "relay regions pushed; probing");
                    tokio::spawn(crate::relay_probe::probe_and_report(
                        regions,
                        outbound_tx.clone(),
                    ));
                }
            }
        }

        // Multi-region DERP: cache the admission ticket + schedule a refresh
        // at ~90 % of its remaining validity (the refresher dies with the
        // connection — its send fails once the outbound channel closes).
        ServerMsg::DerpTicket { ticket, exp } => {
            *derp_ticket_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((ticket, exp));
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let refresh_in =
                std::time::Duration::from_secs(exp.saturating_sub(now).saturating_mul(9) / 10)
                    .max(std::time::Duration::from_secs(60));
            let tx = outbound_tx.clone();
            debug!(
                exp,
                refresh_in_s = refresh_in.as_secs(),
                "derp ticket cached"
            );
            tokio::spawn(async move {
                tokio::time::sleep(refresh_in).await;
                let _ = tx.send(ClientMsg::DerpTicketRequest {}).await;
            });
        }

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
                    reason: roomler_ai_remote_control::signaling::REJECT_REASON_SESSION_GONE.into(),
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
                    reason: roomler_ai_remote_control::signaling::REJECT_REASON_SESSION_GONE.into(),
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
            transport,
            client_derp_pubkey,
        } => {
            // R4 — quic-derp-v1: the quinn server rides this node's
            // ESTABLISHED `/derp` WS toward the client's pubkey. No TURN
            // allocation, no permission dance. Any missing precondition
            // logs + sends no ready — the client times out and soft-falls
            // back to webrtc-dc, exactly the TURN-alloc-failure shape.
            if transport.as_deref() == Some(tunnel_core::transport::TRANSPORT_QUIC_DERP_V1) {
                let Some(handle) = crate::tunnel::netwatch::derp_tunnel_handle(tenant_id) else {
                    warn!(%session_id, "tunnel quic-derp: no live derp mux for this tenant — no ready");
                    return Ok(());
                };
                let Some(client_pk) = client_derp_pubkey
                    .as_deref()
                    .and_then(tunnel_core::transport::derp::parse_pubkey_hex)
                else {
                    warn!(%session_id, "tunnel quic-derp: missing/unparseable client_derp_pubkey — no ready");
                    return Ok(());
                };
                let conn = handle.mux.tunnel_conn_for(client_pk);
                let relay_conn: Arc<dyn relay::RelayConn> = Arc::new(conn);
                match crate::tunnel::quic_peer::AgentQuicPeer::setup_over_derp(
                    session_id,
                    quic_auth_token,
                    relay_conn,
                    handle.self_pubkey_hex.clone(),
                ) {
                    Ok(peer) => {
                        let ready = ClientMsg::TunnelQuicReady {
                            session_id,
                            cert_fingerprint: peer.cert_fingerprint().to_string(),
                            addrs: peer.addrs(),
                            derp_pubkey: peer.derp_pubkey_hex().map(str::to_string),
                        };
                        tunnel_quic_peers.insert(session_id, Arc::new(peer));
                        let _ = outbound_tx.send(ready).await;
                        info!(%session_id, "agent QUIC-over-DERP peer ready; rc:tunnel.quic.ready sent");
                    }
                    Err(e) => {
                        warn!(%session_id, %e, "tunnel quic-derp: AgentQuicPeer setup failed");
                    }
                }
                return Ok(());
            }
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
                        derp_pubkey: None,
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
                let _ = tokio::time::timeout(PEER_CLOSE_BUDGET, peer.close()).await;
            }
            // The session may instead be on the QUIC data plane.
            // `AgentQuicPeer::close` is synchronous (aborts the accept
            // task; the endpoint drops with the last Arc).
            if let Some(quic_peer) = tunnel_quic_peers.remove(&session_id) {
                quic_peer.close();
            }
        }

        // S1a — operator-triggered forced self-update from the web UI.
        // Hand the request to the updater loop's trigger channel; the
        // actual check/download/install runs there (same gates as the
        // periodic path: 5-min install-storm cooldown; transfer-defer
        // is bypassed with a warn because the operator asked NOW).
        ServerMsg::UpdateNow { pin } => {
            // Multi-org P1: the self-updater is machine-wide, so only the
            // PRIMARY enrollment may drive it — a secondary org's admin
            // must not force-update a binary shared with every other org.
            // The ignore is surfaced (log + LocalAPI OrgStatus counter)
            // rather than silently swallowed.
            if !ctx.is_primary {
                let total = ctx
                    .updates_ignored
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                warn!(
                    org = %ctx.label,
                    ?pin,
                    ignored_total = total,
                    "rc:agent.update ignored — only the primary enrollment drives the \
                     machine-wide self-updater"
                );
                return Ok(());
            }
            info!(?pin, "rc:agent.update — operator-triggered self-update");
            if !crate::updater::request_update_now(pin) {
                warn!(
                    "update trigger dropped — auto-updater not running \
                     (ROOMLERD_AUTO_UPDATE=0?) or a trigger is already queued"
                );
            }
        }

        // Remote config (docs/remote-config.md) — reconcile against a pushed
        // desired state, then SAY what happened.
        //
        // Every branch below reports, including both refusals. That is the
        // point of the report rather than a completeness reflex: an operator
        // watching the dashboard cannot otherwise tell a device that declined
        // from one that never received the frame, and the two refusals here
        // are the ones they will actually hit — each with a concrete next
        // action they can only take if somebody tells them.
        ServerMsg::ConfigPush { revision, desired } => {
            use roomler_ai_remote_control::models::ConfigOutcome;

            // Machine-wide keys, so only the PRIMARY enrollment may drive
            // them — the identical rule `UpdateNow` applies to the
            // machine-wide self-updater. `AgentConfig::for_org` scopes none
            // of exec_enabled/ssh_*, so a secondary org's push would change
            // what EVERY org on this host can reach.
            if !ctx.is_primary {
                warn!(
                    org = %ctx.label,
                    revision,
                    "rc:agent.config ignored — only the primary enrollment may \
                     change machine-wide config"
                );
                report_config_status(outbound_tx, revision, ConfigOutcome::NotPrimary, None);
                return Ok(());
            }
            // Gate 4, and the reason this feature does not erode it: the
            // device decides whether it accepts pushed config at all, and the
            // server can never set this key.
            //
            // Read LIVE, not from the startup snapshot: this is how the person
            // holding the machine REVOKES the delegation, and a revocation
            // that waited for a service restart — while the control plane's
            // assertions took effect immediately — would be the slower half of
            // a rule that exists to be the last word.
            if !remote_cfg.remote_config_enabled() {
                info!(
                    revision,
                    "rc:agent.config ignored — this device has not opted in \
                     (set remote_config_enabled=true locally to accept pushed config)"
                );
                // Reporting a refusal is NOT a leak of gate 4's protection:
                // the server already knows it pushed and got nothing. What it
                // does not know is WHY, and that difference is the whole gap
                // between "update this device" and "go set one key on it".
                report_config_status(outbound_tx, revision, ConfigOutcome::NotOptedIn, None);
                return Ok(());
            }
            // Opted in and primary: reconcile against the file on DISK.
            // Idempotent — an already-matching desired state writes nothing,
            // which matters because this runs on every single reconnect.
            match remote_cfg.apply(&desired).await {
                Ok(applied) if applied.is_noop() => {
                    debug!(revision, "rc:agent.config — already matches; nothing to do");
                    // Still reported. A converged device and a device that has
                    // not answered look identical otherwise, and the steady
                    // state of this feature is "everything already matches" —
                    // so the common case would be the one with no evidence.
                    report_config_status(outbound_tx, revision, ConfigOutcome::Noop, None);
                }
                Ok(applied) => {
                    // Two lists, deliberately never one: `exec_enabled` is in
                    // force NOW, while the ssh_* keys are only WRITTEN until
                    // the daemon restarts (the SSH server is spliced into the
                    // packet path at overlay-runtime build). Reporting them
                    // together would tell an operator SSH is on when it isn't.
                    info!(
                        revision,
                        live = ?applied.live,
                        needs_restart = ?applied.needs_restart,
                        "rc:agent.config applied"
                    );
                    if !applied.needs_restart.is_empty() {
                        warn!(
                            keys = ?applied.needs_restart,
                            "rc:agent.config — saved, but these need a daemon restart \
                             to take effect"
                        );
                    }
                    let msg = ClientMsg::ConfigStatus {
                        revision,
                        outcome: ConfigOutcome::Applied,
                        live: applied.live.iter().map(|k| k.to_string()).collect(),
                        needs_restart: applied
                            .needs_restart
                            .iter()
                            .map(|k| k.to_string())
                            .collect(),
                        detail: None,
                    };
                    if outbound_tx.try_send(msg).is_err() {
                        debug!("rc:agent.config_status dropped (outbound queue full or closed)");
                    }
                }
                Err(e) => {
                    warn!(revision, %e, "rc:agent.config apply failed");
                    report_config_status(
                        outbound_tx,
                        revision,
                        ConfigOutcome::Failed,
                        Some(e.as_str()),
                    );
                }
            }
        }

        // Fleet RPC — run one bounded command and answer with rc:rpc.result.
        //
        // The server has already cleared gates 1–3 (org kill-switch, caller
        // permission, the device's ExecPolicy). Gate 4 is ours: `exec_enabled`
        // belongs to whoever holds this box and is the only refusal that
        // survives a compromised control plane.
        //
        // Everything after the gate runs on a SPAWNED task: a command may take
        // up to 300 s and this handler is on the WS receive path — blocking
        // here would stall every other message on this org's socket, including
        // the heartbeat that keeps the device marked online.
        ServerMsg::RpcExec {
            request_id,
            shell,
            command,
            timeout_ms,
            max_output_bytes,
            cwd,
            caller,
            consent_mode,
        } => {
            // Gate 4, read LIVE (docs/remote-config.md): a pushed change takes
            // effect on the next command, with no restart — which is why
            // this is the atomic and not the startup snapshot.
            if !remote_cfg.exec_enabled() {
                warn!(
                    %request_id, %caller,
                    "rc:rpc.exec refused — exec_enabled is off on this device"
                );
                // Always answer. A caller is blocked on this; silence would
                // read as a hang rather than a refusal.
                let _ = outbound_tx
                    .send(ClientMsg::RpcResult {
                        request_id,
                        exit_code: None,
                        stdout: String::new(),
                        stderr: String::new(),
                        truncated: false,
                        duration_ms: 0,
                        error: Some(
                            "remote execution is disabled on this device \
                             (`roomler config set exec_enabled true` to allow it)"
                                .to_string(),
                        ),
                    })
                    .await;
                return Ok(());
            }

            // Absent directive ⇒ prompt. Fail-safe for a gate that grants root.
            let auto = matches!(
                consent_mode,
                Some(roomler_ai_remote_control::models::ConsentMode::Auto)
            );
            let directive = if auto {
                crate::consent::Mode::AutoGrant
            } else {
                crate::consent::Mode::Prompt {
                    timeout: crate::consent::DEFAULT_PROMPT_TIMEOUT,
                }
            };
            // FR-27 — same floor as the RC path: a device that set
            // `auto_grant_session = false` is not overridden into auto-granting
            // a command that runs as SYSTEM/root.
            let consent_mode = crate::consent::strictest_of(
                Some(directive),
                consent_broker.mode(),
                crate::consent::DEFAULT_PROMPT_TIMEOUT,
            );

            // FR-27 — exec has ALWAYS prompted through this broker and NEVER
            // written a marker, so its prompt reached no UI: the only way to
            // answer one was to find the request id in the daemon log and run
            // the CLI inside 30 s. In practice that made a non-`auto`
            // `exec_policy` unusable rather than strict.
            //
            // ⚠️ The command is redacted BEFORE it goes anywhere near a marker
            // file that a GUI will render — same redactor that scrubs exec
            // output leaving the host, for the same reason.
            if matches!(consent_mode, crate::consent::Mode::Prompt { .. }) {
                let shown = crate::exec::redactor().apply(&command);
                let detail: String = shown.chars().take(512).collect();
                // Same multi-org rule as the RC prompt: name the asking org
                // only on a device that serves more than one, since on a
                // single-org device there is nothing to disambiguate.
                // `rc:rpc.exec` carries no tenant display name, so this
                // loop's own label is the best available.
                let org = if !ctx.is_primary || !agent_cfg.orgs.is_empty() {
                    ctx.label.clone()
                } else {
                    String::new()
                };
                // Native panel first, exactly as for a screen-control request
                // — and with the command in front of whoever is deciding,
                // because "approve SSH-ish access" and "approve `rm -rf`
                // running as SYSTEM" are not the same question.
                let native = indicator.show_prompt(crate::indicator::PromptView {
                    session_hex: request_id.clone(),
                    title: "Command execution request".into(),
                    lead: format!("{caller} wants to run a command on this device."),
                    detail: detail.clone(),
                    permissions: String::new(),
                    org: org.clone(),
                    expires_at: std::time::Instant::now() + crate::consent::DEFAULT_PROMPT_TIMEOUT,
                });
                let prompt = crate::consent::PendingPrompt {
                    kind: crate::consent::PromptKind::Exec,
                    asked_by: &caller,
                    permissions: String::new(),
                    detail,
                    org,
                    timeout: crate::consent::DEFAULT_PROMPT_TIMEOUT,
                    surface: if native {
                        crate::consent::PromptSurface::Native
                    } else {
                        crate::consent::PromptSurface::Companion
                    },
                };
                if let Err(e) = consent_broker.write_prompt(&request_id, &prompt) {
                    tracing::warn!(%request_id, %e, native, "could not write the .pending consent marker for this command");
                }
                if !native {
                    tokio::spawn(crate::companion::ensure_running());
                }
            }

            let broker = consent_broker.clone();
            let outbound = outbound_tx.clone();
            let req = crate::exec::ExecRequest {
                request_id: request_id.clone(),
                shell,
                command,
                timeout_ms,
                max_output_bytes,
                cwd,
                caller: caller.clone(),
                // Fleet RPC is privileged diagnostics by design — the whole
                // point is `netsh`, route tables and service state. It has
                // always run as the daemon and continues to.
                run_as: crate::exec::RunAs::Daemon,
            };
            let ind = indicator.clone();
            tokio::spawn(async move {
                let decision = broker.request_with_mode(&request_id, consent_mode).await;
                // FR-27 — same convergence point as the RC path: take the
                // native panel down however the question was answered.
                ind.hide_prompt(&request_id);
                let outcome = if decision.granted() {
                    crate::exec::shared()
                        .run(req, &crate::exec::redactor())
                        .await
                } else {
                    warn!(%request_id, %caller, ?decision, "rc:rpc.exec denied at the device");
                    crate::exec::ExecOutcome {
                        error: Some(format!(
                            "the operator at this device did not approve the command ({decision:?})"
                        )),
                        ..Default::default()
                    }
                };
                if let Err(e) = outbound
                    .send(ClientMsg::RpcResult {
                        request_id: request_id.clone(),
                        exit_code: outcome.exit_code,
                        stdout: outcome.stdout,
                        stderr: outcome.stderr,
                        truncated: outcome.truncated,
                        duration_ms: outcome.duration_ms,
                        error: outcome.error,
                    })
                    .await
                {
                    warn!(%request_id, %e, "rc:rpc.result send failed (channel closed)");
                }
            });
        }

        // Fleet RPC — kill an in-flight command. The run itself still answers
        // with an rc:rpc.result carrying `error`, so the caller isn't left
        // waiting on a request nobody will finish.
        ServerMsg::RpcCancel { request_id } => {
            tokio::spawn(async move {
                let found = crate::exec::shared().cancel(&request_id).await;
                info!(%request_id, found, "rc:rpc.cancel");
            });
        }

        // Roomler SSH — the server authorized ONE inbound session against this
        // device. Recording it is all that happens here; the grant is redeemed
        // (and consumed) when a connection authenticates with the named key.
        //
        // Gate 4 is re-checked locally: a device with `ssh_enabled` off must
        // not accumulate grants it would never honour, and refusing loudly
        // beats a caller timing out against a port that answers nothing.
        ServerMsg::SshGrant {
            grant_id,
            public_key,
            caller,
            account_mode,
            account,
            expires_at_ms,
            session_secs,
            consent_mode,
        } => {
            #[cfg(feature = "ssh-server")]
            {
                if !agent_cfg.ssh_enabled {
                    warn!(
                        %grant_id, %caller,
                        "rc:ssh.grant refused — ssh_enabled is off on this device"
                    );
                } else if let Err(e) = crate::ssh::record_grant(
                    grant_id.clone(),
                    public_key,
                    caller.clone(),
                    account_mode,
                    account,
                    consent_mode,
                    expires_at_ms,
                    session_secs,
                ) {
                    warn!(%grant_id, %caller, %e, "rc:ssh.grant rejected");
                }
            }
            #[cfg(not(feature = "ssh-server"))]
            {
                let _ = (
                    &public_key,
                    &account_mode,
                    &account,
                    consent_mode,
                    expires_at_ms,
                    session_secs,
                );
                warn!(
                    %grant_id, %caller,
                    "rc:ssh.grant ignored — this build lacks the `ssh-server` feature"
                );
            }
        }

        // Roomler SSH — the answer to a session THIS device asked for
        // (`roomler ssh`). Handed to whichever LocalAPI call is parked on it;
        // an unknown id means that caller already gave up.
        ServerMsg::SshResponse {
            request_id,
            address,
            port,
            grant_id,
            host_pubkey,
            expires_at_ms,
            error,
        } => {
            // The key itself is never logged — only whether one arrived. It is
            // public, but a log line is the wrong place to start treating key
            // material as printable, and "did the server have one for this
            // device" is the whole diagnostic value anyway.
            info!(
                %request_id,
                address = ?address, port = ?port, grant_id = ?grant_id,
                verifiable = host_pubkey.is_some(), error = ?error,
                "rc:ssh.response"
            );
            crate::ssh_origin::deliver(
                &request_id,
                crate::ssh_origin::SshGrantAnswer {
                    address,
                    port,
                    host_pubkey,
                    grant_id,
                    expires_at_ms,
                    error,
                },
            );
        }

        // Fleet RPC — the answer to a command THIS device asked another device
        // to run (`roomler exec`). Handed to whichever LocalAPI call is parked
        // on it; an unknown id means that caller already gave up.
        ServerMsg::RpcExecResponse {
            request_id,
            exit_code,
            stdout,
            stderr,
            truncated,
            duration_ms,
            error,
        } => {
            let delivered = crate::exec::deliver_response(
                &request_id,
                crate::exec::ExecOutcome {
                    exit_code,
                    stdout,
                    stderr,
                    truncated,
                    duration_ms,
                    error,
                },
            );
            if !delivered {
                debug!(%request_id, "rc:rpc.response for an unknown request — caller gave up");
            }
        }

        // FR-40 — retire THIS org's overlay key: mint → persist → report →
        // reconnect under the new identity
        // (`docs/fr/FR-40-overlay-key-rotation.md`).
        //
        // Deliberately NOT primary-only: the key is per enrollment
        // (`AgentConfig::for_org` scopes it), so org B's admin rotating org B's
        // key on a shared host touches nothing of org A's — the opposite of
        // `rc:agent.update` / `rc:agent.config`, which drive machine-wide
        // state. Every branch reports, refusals included: the operator is
        // looking at a security action "in flight" and each refusal has a
        // different fix.
        ServerMsg::KeyRotate { request_id } => {
            use roomler_ai_remote_control::models::KeyRotationOutcome;

            if agent_cfg.overlay_key_rotation == Some(false) {
                warn!(
                    %request_id,
                    "rc:agent.key_rotate refused — overlay_key_rotation=false on this device"
                );
                report_key_rotated(
                    outbound_tx,
                    &request_id,
                    KeyRotationOutcome::Disabled,
                    None,
                    None,
                    agent_cfg.overlay_wg_key_epoch,
                    Some("overlay_key_rotation=false on the device"),
                );
                return Ok(());
            }
            // The device's OWN ceiling. The server has one too, but a bound
            // that exists only on the ordering side is not a bound.
            if !key_rotation_ceiling_ok(tenant_id) {
                warn!(
                    %request_id,
                    "rc:agent.key_rotate refused — a rotation ran less than a minute ago"
                );
                report_key_rotated(
                    outbound_tx,
                    &request_id,
                    KeyRotationOutcome::RateLimited,
                    None,
                    None,
                    agent_cfg.overlay_wg_key_epoch,
                    Some("rotated less than 60 s ago"),
                );
                return Ok(());
            }
            // A build with no overlay surface has nothing to rotate. The mint
            // lives in `crate::key_rotation`, which carries the feature split,
            // so this arm — and `ConnectError::KeyRotated` — compiles the same
            // in every feature set (see that module's doc for why that is not
            // merely tidy).
            let Some((new_secret, new_public)) = crate::key_rotation::mint_wg_identity() else {
                warn!(
                    %request_id,
                    "rc:agent.key_rotate refused — this build has no overlay surface"
                );
                report_key_rotated(
                    outbound_tx,
                    &request_id,
                    KeyRotationOutcome::Unsupported,
                    None,
                    None,
                    0,
                    Some("no overlay surface in this build"),
                );
                return Ok(());
            };
            {
                let old_public = agent_cfg
                    .overlay_wg_secret_key
                    .as_deref()
                    .and_then(crate::key_rotation::wg_public_of);
                // Persist FIRST. If this fails the identity stays and the
                // device says so: a key that is not written down would be
                // lost at the next restart, and the device would come back as
                // the key it just retired.
                let key_epoch = match remote_cfg
                    .rotate_overlay_key(ctx.is_primary, tenant_id, new_secret.clone())
                    .await
                {
                    Ok(epoch) => epoch,
                    Err(e) => {
                        warn!(
                            %request_id,
                            error = %e,
                            "rc:agent.key_rotate failed — identity unchanged"
                        );
                        report_key_rotated(
                            outbound_tx,
                            &request_id,
                            KeyRotationOutcome::Failed,
                            old_public.as_deref(),
                            None,
                            agent_cfg.overlay_wg_key_epoch,
                            Some(&e),
                        );
                        return Ok(());
                    }
                };
                info!(
                    org = %ctx.label,
                    %request_id,
                    old_public_key = old_public.as_deref().unwrap_or("-"),
                    new_public_key = %new_public,
                    key_epoch,
                    "rc:agent.key_rotate — new overlay key persisted; reconnecting under it"
                );
                // P1c — queue the same report for the NEXT session first
                // (the copy below rides a socket that is about to close).
                pending_rotation_reports()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        tenant_id.to_string(),
                        PendingRotationReport {
                            request_id: request_id.clone(),
                            old_public_key: old_public.clone(),
                            new_public_key: new_public.clone(),
                            key_epoch,
                        },
                    );
                report_key_rotated(
                    outbound_tx,
                    &request_id,
                    KeyRotationOutcome::Rotated,
                    old_public.as_deref(),
                    Some(&new_public),
                    key_epoch,
                    None,
                );
                // Let the report leave on this session before it ends — the
                // pump drains the outbound queue concurrently. Even if it is
                // lost, the join under the new key is what the server verifies.
                tokio::time::sleep(Duration::from_millis(300)).await;
                // Same teardown as `Goodbye`: the `?`-propagated Err below
                // would otherwise skip it and leave peers to a 10–30 s
                // silence-detect instead of a clean close.
                close_all_peers(peers, indicator).await;
                close_all_tunnel_peers(tunnel_peers).await;
                close_all_tunnel_quic_peers(tunnel_quic_peers).await;
                pending_codecs.clear();
                pending_transports.clear();
                pending_chroma.clear();
                pending_chunk_framing.clear();
                pending_audio.clear();
                pending_permissions.clear();
                return Err(ConnectError::KeyRotated {
                    secret_base64: new_secret,
                    key_epoch,
                });
            }
        }

        // Multi-org — "Add to another organization" from the admin UI: enroll
        // this machine into a SECOND org on the same server, from a pushed
        // single-use token, and bring that org's loop up without a restart.
        ServerMsg::JoinOrg {
            enrollment_token,
            label,
            overlay_mode,
        } => {
            // Same escalation guard as `rc:agent.update`: only the PRIMARY
            // enrollment may add orgs. A secondary org's admin borrows this
            // device — it must not be able to hand it to further orgs.
            if !ctx.is_primary {
                warn!(
                    org = %ctx.label,
                    "rc:agent.join_org ignored — only the primary enrollment may \
                     add organizations"
                );
                return Ok(());
            }
            // Off-loop: the join makes an HTTP round-trip and takes the
            // config write lock. The message loop must keep pumping (a
            // blocked loop trips the watchdog + the receive-liveness
            // deadline).
            let cfg = agent_cfg.clone();
            tokio::spawn(async move {
                match crate::org_join::join_from_push(
                    &cfg,
                    &enrollment_token,
                    label.as_deref(),
                    overlay_mode.as_deref(),
                )
                .await
                {
                    Ok(outcome) => info!(?outcome, "rc:agent.join_org applied"),
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), "rc:agent.join_org failed")
                    }
                }
            });
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
        // Bounded ([`PEER_CLOSE_BUDGET`]): `pc.close()` hangs on webrtc
        // internals when the network was captured mid-session — the
        // stalled=signaling suicide class. Dropping `peer` right after
        // runs the P6 Drop teardown either way.
        if tokio::time::timeout(PEER_CLOSE_BUDGET, peer.close())
            .await
            .is_err()
        {
            warn!(
                session = %session_id,
                "peer close timed out — dropping it (P6 Drop teardown still runs)"
            );
        }
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
        // Same bound as `close_all_peers` — a webrtc close on a captured
        // network must not stall the signaling loop into the watchdog.
        let _ = tokio::time::timeout(PEER_CLOSE_BUDGET, peer.close()).await;
    }
    info!(count, "torn down agent tunnel peers on ws disconnect");
}

/// Phase 1d (quic-v1): tear down every QUIC tunnel peer on WS
/// disconnect. `AgentQuicPeer::close` is synchronous (aborts the
/// accept task; the quinn endpoint drops with the last `Arc`), so
/// unlike [`close_all_tunnel_peers`] there's no per-peer `.await`.
/// Cheap no-op when the map is empty (normal for non-QUIC agents).
async fn close_all_tunnel_quic_peers(tunnel_quic_peers: &mut TunnelQuicPeers) {
    if tunnel_quic_peers.is_empty() {
        return;
    }
    let count = tunnel_quic_peers.len();
    for (_, peer) in tunnel_quic_peers.drain() {
        peer.close();
    }
    info!(count, "torn down agent QUIC tunnel peers on ws disconnect");
}

/// R3 — per-tenant stash of QUIC tunnel peers that SURVIVE a transient
/// control-WS reattach. QUIC flows self-signal over their own streams (no
/// welded control-WS sender — see `tunnel_core::forward::run_flow_quic`), so
/// an established QUIC/derp data plane keeps flowing while the control WS
/// re-establishes; the server-side grace (`ROOMLER__RC__TUNNEL_GRACE_SECS`)
/// keeps the session from being terminated meanwhile. Gated on
/// `tunnel_peers_survive_reattach` (default off ⇒ pre-R3 byte-identical).
/// Size-capped so a long agent outage (client re-opened elsewhere ⇒ orphaned
/// peers) can't leak sockets without bound; a dropped peer self-closes via
/// its #602 Drop.
/// A control-WS session's live QUIC tunnel peers, keyed by tunnel session id.
type TunnelQuicPeers = HashMap<bson::oid::ObjectId, Arc<crate::tunnel::quic_peer::AgentQuicPeer>>;

static TUNNEL_QUIC_SURVIVAL: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<String, TunnelQuicPeers>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));

/// Max surviving QUIC sessions stashed per tenant (orphan-leak bound).
const TUNNEL_QUIC_SURVIVAL_CAP: usize = 32;

/// The R3 agent gate (`ROOMLERD_TUNNEL_PEERS_SURVIVE_REATTACH`, default
/// off). Read per exit so a live `roomler config set` + restart flips it.
fn tunnel_peers_survive_enabled() -> bool {
    tunnel_core::env::flag("TUNNEL_PEERS_SURVIVE_REATTACH", false)
}

/// Reclaim the tenant's stashed QUIC peers at the start of a control-WS
/// session. Empty when the flag is off — pre-R3 behaviour byte-identical.
fn reclaim_survived_quic_peers(tenant: &str) -> TunnelQuicPeers {
    if !tunnel_peers_survive_enabled() {
        return HashMap::new();
    }
    let map = TUNNEL_QUIC_SURVIVAL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(tenant)
        .unwrap_or_default();
    if !map.is_empty() {
        info!(
            count = map.len(),
            "R3: reclaimed surviving QUIC tunnel peers across control-WS reattach"
        );
    }
    map
}

/// On a TRANSIENT control-WS exit (RX deadline, resume-skew, netstate probe,
/// pong-RTT, read/write/pong error): STASH the QUIC peers so they survive to
/// the next session (flag on), or close them as today (flag off). TERMINAL
/// exits — shutdown and `Goodbye` — call [`close_all_tunnel_quic_peers`]
/// directly instead (a re-registration or a delete must not keep peers).
async fn park_survived_quic_peers(tunnel_quic_peers: &mut TunnelQuicPeers, tenant: &str) {
    if !tunnel_peers_survive_enabled() {
        close_all_tunnel_quic_peers(tunnel_quic_peers).await;
        return;
    }
    if tunnel_quic_peers.is_empty() {
        return;
    }
    let count = tunnel_quic_peers.len();
    let mut stash = TUNNEL_QUIC_SURVIVAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let slot = stash.entry(tenant.to_string()).or_default();
    for (sid, peer) in tunnel_quic_peers.drain() {
        slot.insert(sid, peer);
    }
    while slot.len() > TUNNEL_QUIC_SURVIVAL_CAP {
        let Some(k) = slot.keys().next().cloned() else {
            break;
        };
        slot.remove(&k); // dropped peer self-closes (#602 Drop)
    }
    let stashed = slot.len();
    drop(stash);
    info!(
        count,
        stashed, "R3: parked QUIC tunnel peers to survive the control-WS reattach"
    );
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Single choke point for every frame written to the control WS, bounded
/// by `WS_SEND_TIMEOUT` (see its doc for the field failure this exists
/// for). A timeout is reported as an ordinary send error so every caller
/// takes its existing failed-send path — which for all of them means
/// cycling the connection. Cancelling a tungstenite send mid-flush is
/// safe here because no caller reuses the stream after an error.
async fn send_frame(ws: &mut Ws, frame: Message) -> Result<()> {
    match tokio::time::timeout(WS_SEND_TIMEOUT, ws.send(frame)).await {
        Ok(res) => res.context("ws send"),
        Err(_elapsed) => Err(anyhow::anyhow!(
            "ws send wedged for {} s (route captured mid-flight?) — cycling the connection",
            WS_SEND_TIMEOUT.as_secs()
        )),
    }
}

/// The next rc message the daemon delegated to us, or never if this process is
/// not a supervised worker.
///
/// `pending()` rather than an `Option` arm with a guard: a `select!` branch
/// that is simply never ready is the honest encoding of "this process is not a
/// worker", and it keeps the arm's body identical in both cases.
async fn next_delegated(
    rx: &mut Option<&mut mpsc::Receiver<crate::delegate::WorkerInbound>>,
) -> Option<crate::delegate::WorkerInbound> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Send one reply for a session, to wherever that session's replies go.
///
/// The WS path keeps its DIRECT write. That is not incidental: the outbound
/// queue is drained by an arm of the same `select!` that calls
/// `handle_server_msg`, so while that function runs — holding `&mut ws` — no
/// queued message can be written, and the SDP answer has therefore always
/// preceded the peer's first ICE candidate. A queued answer would lose that.
///
/// A DELEGATED session has one ordered queue for everything, so the answer may
/// follow the first candidates. Safe, and verified rather than assumed: the
/// controller buffers early ICE in `pendingRemoteIce` (`useRemoteControl.ts`)
/// exactly because `addIceCandidate` throws before `setRemoteDescription`, and
/// flushes it when the answer lands.
async fn reply_for_session(
    ws: &mut Ws,
    delegated: Option<&mpsc::Sender<ClientMsg>>,
    msg: &ClientMsg,
) -> Result<()> {
    match delegated {
        Some(tx) => tx
            .send(msg.clone())
            .await
            .map_err(|_| anyhow::anyhow!("delegation queue closed")),
        None => send_msg(ws, msg).await,
    }
}

async fn send_msg(ws: &mut Ws, msg: &ClientMsg) -> Result<()> {
    let json = serde_json::to_string(msg).context("serialising ClientMsg")?;
    send_frame(ws, Message::text(json)).await
}

/// Keepalive ping payload: `ws_epoch`-relative send time in millis, 8 bytes
/// BE. RFC 6455 §5.5.3 makes the peer echo it verbatim in the pong, turning
/// every keepalive into an application-level RTT probe for free.
fn ping_payload(epoch: std::time::Instant) -> Vec<u8> {
    (epoch.elapsed().as_millis() as u64).to_be_bytes().to_vec()
}

/// Decode a pong payload stamped by [`ping_payload`] into the measured
/// round trip. `None` for foreign shapes (an unsolicited server pong, or a
/// pre-upgrade peer that strips payloads) — those simply don't vote.
fn pong_rtt(payload: &[u8], epoch: std::time::Instant) -> Option<Duration> {
    let ms = u64::from_be_bytes(payload.try_into().ok()?);
    let now = epoch.elapsed().as_millis() as u64;
    Some(Duration::from_millis(now.saturating_sub(ms)))
}

/// The device's SSH host public key for the hello, or empty.
///
/// Two cfg'd items rather than one function with an inner `#[cfg]` expression:
/// the `ssh` module is gated on the OVERLAY features (no overlay ⇒ no address
/// to serve SSH on ⇒ the module does not exist), so the call itself has to
/// disappear, not just its result. `ssh.rs` then applies the SECOND gate —
/// `ssh-server` — on top.
#[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
fn ssh_host_pubkey_for(cfg: &crate::config::AgentConfig) -> String {
    crate::ssh::host_public_key(cfg)
}

#[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
fn ssh_host_pubkey_for(_cfg: &crate::config::AgentConfig) -> String {
    String::new()
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

fn stub_caps(multi_org_tun: bool) -> AgentCaps {
    // Real probe via encode::caps; replaces the empty-vec stub. The
    // resulting AgentCaps populates the rc:agent.hello payload, which
    // the server persists into the agents collection and surfaces in
    // the admin UI (2A.2).
    let mut caps = crate::encode::caps::detect();
    // `detect()` is memoized and config-blind, so the one capability that
    // depends on the host's config gets appended here: `tun` says "this
    // daemon's TUN is already muxed — a second org can join the mesh
    // live". The flag is read from the config the PROCESS started with
    // (the reconnect loop lives inside `run`, which owns `cfg`), which is
    // precisely the honest answer: flipping `overlay_multi_org` at runtime
    // does not mux a TUN the primary loop already opened un-muxed, so the
    // server keeps being told "no" until a real daemon restart. That's
    // what lets `join-org` answer `restart_required` instead of promising
    // a mesh that would fail its bring-up (field 2026-08-07, WINHOST-A).
    if multi_org_tun {
        caps.multi_org.push("tun".into());
    }
    caps
}

pub(crate) fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slowness-cycle treadmill (field winhost-a 2026-08-17): cycling on RTT
    /// degradation is allowed twice; a THIRD young-connection conviction
    /// holds instead — but a connection that ran healthy past the fresh
    /// window re-arms the counter (the 2026-08-15 escape-the-zombie regime
    /// must keep working).
    #[test]
    fn slowness_treadmill_caps_young_reconvictions_and_rearms_on_aged_connections() {
        let young = Duration::from_secs(120);
        let aged = WS_TREADMILL_FRESH_WINDOW + Duration::from_secs(1);
        let mut cycles = 0u32;

        // First two young convictions cycle (the proven-useful regime).
        assert!(!slowness_treadmill_holds(&mut cycles, young));
        assert_eq!(cycles, 1);
        assert!(!slowness_treadmill_holds(&mut cycles, young));
        assert_eq!(cycles, 2);
        // Third young conviction: the fresh connections ALSO went slow —
        // cycling is a treadmill; hold.
        assert!(slowness_treadmill_holds(&mut cycles, young));
        // Further convictions on the same held connection keep holding.
        assert!(slowness_treadmill_holds(&mut cycles, young));

        // A connection that stayed healthy past the window before
        // degrading is a NEW episode: counter re-arms at 1 and the cycle
        // fires again.
        assert!(!slowness_treadmill_holds(&mut cycles, aged));
        assert_eq!(cycles, 1);
        // And the treadmill can re-latch from there.
        assert!(!slowness_treadmill_holds(&mut cycles, young));
        assert!(slowness_treadmill_holds(&mut cycles, young));

        // The outer loop's resets (clean close / netstate Major) restore
        // full cycling credit.
        cycles = 0;
        assert!(!slowness_treadmill_holds(&mut cycles, young));
    }

    /// The keepalive ping payload must round-trip through [`pong_rtt`] as a
    /// near-zero RTT, and foreign pong shapes must abstain rather than
    /// vote — the zombie-slow-WS detector (field winhost-a 2026-08-15) only
    /// ever acts on pongs that echo OUR stamp.
    #[test]
    fn pong_rtt_round_trips_and_rejects_foreign_payloads() {
        let epoch = std::time::Instant::now();
        let p = ping_payload(epoch);
        assert_eq!(p.len(), 8);
        let rtt = pong_rtt(&p, epoch).expect("own stamp decodes");
        assert!(
            rtt < WS_PONG_RTT_DEGRADED,
            "an immediate echo must read healthy, got {rtt:?}"
        );
        assert_eq!(
            pong_rtt(&[], epoch),
            None,
            "empty (unsolicited) pong abstains"
        );
        assert_eq!(
            pong_rtt(&[1, 2, 3], epoch),
            None,
            "short foreign payload abstains"
        );
        // A stamp from 25 s ago reads as a degraded round trip. Use an
        // epoch shifted into the past so "now" is ~30 s along it — a fresh
        // epoch's elapsed() is ~0 and the stale stamp would saturate to 0.
        let old_epoch = std::time::Instant::now()
            .checked_sub(Duration::from_secs(30))
            .expect("30 s before now is representable");
        let stale = ((old_epoch.elapsed().as_millis() as u64) - 25_000)
            .to_be_bytes()
            .to_vec();
        let slow = pong_rtt(&stale, old_epoch).expect("stamped payload decodes");
        assert!(
            slow >= WS_PONG_RTT_DEGRADED,
            "a 25 s echo must read degraded, got {slow:?}"
        );
    }

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

    /// W4(d) — the displacement ladder pins each rung, stays monotonic and
    /// PARKS at 15 min: a sustained duel must never re-accelerate, and the
    /// default path never escalates to a process exit at any count.
    #[test]
    fn replaced_ladder_parks_at_fifteen_minutes() {
        assert_eq!(replaced_backoff_for(1), Duration::from_secs(60));
        assert_eq!(replaced_backoff_for(2), Duration::from_secs(60));
        assert_eq!(replaced_backoff_for(3), Duration::from_secs(120));
        assert_eq!(replaced_backoff_for(4), Duration::from_secs(240));
        assert_eq!(replaced_backoff_for(5), Duration::from_secs(480));
        assert_eq!(replaced_backoff_for(6), Duration::from_secs(900));
        assert_eq!(replaced_backoff_for(500), Duration::from_secs(900));
        let mut last = Duration::ZERO;
        for n in 1..=20usize {
            let d = replaced_backoff_for(n);
            assert!(d >= last, "ladder must be monotonic; failed at n={n}");
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
    fn ws_send_timeout_fits_inside_the_watchdog_budget() {
        // The signaling pump's stall threshold is 90 s (main.rs
        // registration). The worst un-ticked dwell in one select arm is
        // TWO bounded sends back-to-back (heartbeat + first session-stats
        // frame — the stats loop breaks on its first error), so
        // 2×WS_SEND_TIMEOUT must leave the watchdog real headroom, or
        // the WINHOST-A 2026-08-15 failure comes back: wedged send →
        // stalled=signaling → exit(2) → full overlay teardown on a mere
        // VPN route capture. The lower bound keeps a slow-but-alive
        // link (a few missed RTOs) from being cycled spuriously.
        assert!(
            WS_SEND_TIMEOUT >= Duration::from_secs(5),
            "WS_SEND_TIMEOUT too aggressive — would cycle slow-but-alive \
             links; current={WS_SEND_TIMEOUT:?}"
        );
        assert!(
            2 * WS_SEND_TIMEOUT <= Duration::from_secs(45),
            "2×WS_SEND_TIMEOUT must stay well inside the 90 s signaling \
             stall budget; current={WS_SEND_TIMEOUT:?}"
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
