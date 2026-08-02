//! Overlay node runtime (Phase 3b).
//!
//! Drives one node's membership in the overlay mesh: announces itself
//! (`rc:overlay.join`), applies the server's netmap (install / drop a
//! WireGuard peer per entry), brings up the TUN, and pumps packets
//! between the TUN and the [`WgDevice`](super::wg::WgDevice).
//!
//! The runtime **owns** the `WgDevice` and runs a single `select!` loop of
//! pure CONTROL work (netmap installs, carrier health sweeps, relay
//! coordination, route guard). The DATA PLANE is fully off-loop (P1/S6):
//! outbound rides the persistent TUN reader task → mpsc → the dedicated
//! outbound pump (a [`WgSender`](super::wg::WgSender) clone — the shared
//! send half of the device, single-writer/multi-reader); inbound rides the
//! per-peer recv tasks / shared demux → mpsc → the inbound writer task.
//! No control handler can therefore ever delay a packet; `warn_if_slow`
//! remains as the tripwire against re-coupling.
//!
//! Carrier construction (direct UDP vs coturn relay) is delegated to a
//! [`LinkFactory`] so this orchestration is testable with loopback
//! carriers + a mock TUN, and so the corp-NAT relay path can be added
//! without reworking the runtime.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use bson::oid::ObjectId;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use super::WgKeypair;
use super::direct;
use super::dns;
use super::netmap::{PeerConfig, peer_config_from_netmap};
use super::relay_link::{ReadyLink, RelayCoordinator, RelayKind};
use super::tun::TunIo;
use super::wg::{Carrier, QUIC_BUILD_TIMEOUT, WG_OVERHEAD, WgDevice, overlay_quic_enabled};
use crate::localapi::{
    ConnectionType, DnsStatus, ExitNodeStatus, OverlayView, PeerCarrierDebug, PeerInfo,
};
use crate::transport::derp::DerpMux;
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer, NetmapPeer, OverlayNetworkInfo};

// rc.284 — the god-file split. The establishment half, the inbound-adopt
// handler, and the route-ownership/exit-routing half live in CHILD modules of
// `runtime` (not siblings), so the moved code keeps private-field access to
// [`OverlayRuntime`]/[`Installed`] and its `use super::*` inherits this import
// block unchanged — a pure move, no behavior change.
mod establish;
mod inbound;
mod route_guard;

use establish::*;
// Path-compat re-export: `tun.rs` purge names these as
// `crate::overlay::runtime::SPLIT_DEFAULT_V4`/`_V6`.
use route_guard::*;
pub(crate) use route_guard::{SPLIT_DEFAULT_V4, SPLIT_DEFAULT_V6};

// P2 — `DirectTier`, the lifecycle deadlines/constants, and the carrier/probe
// transition fns live in the sibling `lifecycle` module (one place for every
// rule that can kill a carrier). Glob-imported so this module — and its test
// module's `use super::*` — reads them unchanged.
use super::lifecycle::*;
// P3 PR-A — the PathMonitor (measured path selection) runs in SHADOW next to
// the legacy cooldown machinery below: fed the same evidence, asked the same
// questions, compared, never obeyed. See `PathShadow`.
use super::path;

/// P3 PR-A — the shadow harness around [`path::PathMonitor`]: the monitor +
/// the divergence bookkeeping the 48 h soak gate reads. Owned by
/// [`OverlayRuntime`] behind a `std::sync::Mutex` so the sweep/install
/// surfaces (whose signatures are frozen by the timer-parity tests) can feed
/// it through `&self`; every access goes through [`OverlayRuntime::shadow`],
/// which locks, runs a SYNC closure, and drops the guard — the guard can
/// never be held across an await by construction.
struct PathShadow {
    mode: path::PathMonMode,
    mon: path::PathMonitor,
    stats: path::ShadowStats,
    /// Per-peer divergence-warn rate limit (1/min/peer).
    last_div_log: HashMap<ObjectId, Instant>,
    /// Last 10-minute summary emission.
    last_summary: Instant,
    /// Harmful-class ledger: (peer, tier) the monitor refused while legacy
    /// proceeded. If legacy then PROVES the tier (probe latch / healthy rx)
    /// within [`SHADOW_HARM_WINDOW`], the refusal was harmful — the
    /// acceptance gate requires zero of these.
    refused: HashMap<(ObjectId, DirectTier), Instant>,
}

/// A monitor refusal contradicted by a legacy establishment within this
/// window counts as the HARMFUL divergence class.
const SHADOW_HARM_WINDOW: Duration = Duration::from_secs(60);
/// Shadow summary cadence.
const SHADOW_SUMMARY_EVERY: Duration = Duration::from_secs(600);

impl PathShadow {
    fn new() -> Self {
        let mode = path::PathMonMode::parse(crate::env::node_env("OVERLAY_PATHMON").as_deref());
        if mode != path::PathMonMode::On {
            // PR-E — the legacy selection machinery is deleted; there is no
            // shadow/off SELECTION to revert to any more. One startup warn so
            // an operator holding the old env understands what it now does.
            warn!(
                mode = mode.as_str(),
                "overlay pathmon: legacy selection removed (PR-E) — this mode now affects telemetry only; selection is always the monitor (rollback = deploy a pre-PR-E release)"
            );
        }
        Self {
            mode,
            mon: path::PathMonitor::default(),
            stats: path::ShadowStats::default(),
            last_div_log: HashMap::new(),
            last_summary: Instant::now(),
            refused: HashMap::new(),
        }
    }

    /// The tier a comparable action commits to, if any.
    fn action_tier(a: path::PathAction) -> Option<DirectTier> {
        match a {
            path::PathAction::Install(t) | path::PathAction::Probe(t) => Some(t),
            path::PathAction::Keep | path::PathAction::Relay => None,
        }
    }

    /// Record one legacy-vs-monitor comparison. `None` = "refuse / no
    /// decision" (the inbound gate's shape). Divergences are counted (per
    /// trigger class too — P3 PR-B: a reupgrade-tick divergence is a
    /// SCHEDULING disagreement, a netmap/delta one a selection disagreement,
    /// so the soak reads them separately), fed to the harmful ledger, and
    /// warn-logged at most once per minute per peer.
    fn compare(
        &mut self,
        surface: &'static str,
        trigger: &'static str,
        peer: &ObjectId,
        legacy: Option<path::PathAction>,
        monitor: Option<path::PathAction>,
        now: Instant,
    ) {
        self.stats.decisions += 1;
        let class = self.stats.by_class.entry(trigger).or_insert((0, 0));
        class.0 += 1;
        let diverged = match (legacy, monitor) {
            (Some(l), Some(m)) => path::classify(l, m) == path::DivergenceClass::Diverged,
            (l, m) => l != m,
        };
        if !diverged {
            return;
        }
        self.stats.diverged += 1;
        class.1 += 1;
        // Harmful ledger: legacy commits to a tier the monitor currently
        // holds ineligible (whatever the monitor proposed instead).
        if let Some(t) = legacy.and_then(Self::action_tier)
            && monitor.and_then(Self::action_tier) != Some(t)
            && !self.mon.eligible(peer, t, now)
        {
            self.refused.insert((*peer, t), now);
        }
        if self
            .last_div_log
            .get(peer)
            .is_none_or(|&t| now.duration_since(t) >= Duration::from_secs(60))
        {
            self.last_div_log.insert(*peer, now);
            info!(
                %surface, peer = %peer, ?legacy, ?monitor,
                "overlay pathmon[shadow]: decision divergence (legacy authoritative; rate-limited 1/min/peer)"
            );
        }
    }

    /// Legacy PROVED a tier for this peer (probe latched / direct carrier
    /// genuinely receiving) — grade any recent refusal as harmful.
    fn establishment(&mut self, peer: &ObjectId, tier: DirectTier, now: Instant) {
        if let Some(at) = self.refused.remove(&(*peer, tier))
            && now.duration_since(at) <= SHADOW_HARM_WINDOW
        {
            self.stats.harmful += 1;
            warn!(
                peer = %peer, ?tier,
                "overlay pathmon[shadow]: HARMFUL divergence — monitor refused a tier legacy then proved within 60 s"
            );
        }
    }

    /// Model-bug tripwire: right after the monitor booked a failure on a
    /// tier, that tier must be ineligible (the penalty math guarantees it).
    fn assert_ineligible(&mut self, peer: &ObjectId, tier: DirectTier, now: Instant) {
        if self.mon.eligible(peer, tier, now) {
            self.stats.post_death_eligible += 1;
            warn!(
                peer = %peer, ?tier,
                "overlay pathmon[shadow]: MODEL BUG — tier still eligible immediately after its failure booked"
            );
        }
    }

    /// The 10-minute rolling summary (driven from the 5 s health sweep).
    fn maybe_summary(&mut self, now: Instant) {
        // PR-E — `off` silences telemetry only (the monitor still selects).
        if self.mode == path::PathMonMode::Off {
            return;
        }
        if now.duration_since(self.last_summary) < SHADOW_SUMMARY_EVERY {
            return;
        }
        self.last_summary = now;
        // Expire stale refusals so the ledger can't grow unbounded.
        self.refused
            .retain(|_, &mut at| now.duration_since(at) <= SHADOW_HARM_WINDOW);
        info!(
            mode = self.mode.as_str(),
            decisions = self.stats.decisions,
            diverged = self.stats.diverged,
            harmful = self.stats.harmful,
            post_death_eligible = self.stats.post_death_eligible,
            d10_redials = self.stats.d10_redials,
            classes = %self.stats.classes_line(),
            "overlay pathmon: 10-min summary (soak gate: steady <0.1% diverged, harmful = 0)"
        );
    }
}

/// An installed peer carrier + the bookkeeping the direct→relay fallback
/// (rc.136/137) needs.
struct Installed {
    pubkey: [u8; 32],
    overlay_ip: Ipv4Addr,
    /// `true` if reached over the direct LAN socket, `false` over the relay.
    is_direct: bool,
    /// When this carrier was installed — for the warm-up grace period.
    since: Instant,
    /// Last `(tx, rx)` snapshot from the previous sweep (rc.137 lock-free
    /// health). Only meaningful for direct carriers.
    last_traffic: (u64, u64),
    /// Consecutive sweeps where we sent but received nothing (tx grew, rx
    /// flat). A few in a row ⇒ the direct carrier is one-way / dead.
    bad_sweeps: u32,
    /// rc.275 honesty — the health sweep's verdict that this carrier is
    /// SILENTLY ONE-WAY: installed past the warm-up grace with either no
    /// completed WG handshake ever (the pre-handshake zombie whose tx/rx
    /// counters stay flat — pc50045 behind its corp VPN: every tier
    /// "installed", zero handshakes, `roomler peers` said `direct`/`relay`
    /// while 100% of pings died) or the rc.137 one-way strike counter
    /// accumulating. Surfaced through the LocalAPI peer view so the CLI can
    /// render `stalled` instead of a healthy-looking tier label. Verdict
    /// only — every kill/refresh decision stays in `lifecycle::carrier_tick`.
    stalled: bool,
    /// Monotonic instant we last HEARD from this peer — a real "last seen"
    /// (P3b-3). Seeded to `since` at install; advanced by `sweep_carrier_health`
    /// whenever the keepalive-inclusive `rx_any` liveness counter climbed since
    /// the previous sweep (rc.206 — NOT the IP-data `rx`, which stays flat on an
    /// idle-but-alive link whose only inbound is keepalives). Converted to an
    /// absolute epoch-ms `last_seen_ms` in `build_overlay_view`. Sweep cadence
    /// (`FALLBACK_TICK`, ~5 s) sets the granularity — fine for a human
    /// "Ns/Nm ago" column, and passive keepalives now keep it fresh for live
    /// peers (which is also what the rx-staleness watchdog relies on).
    last_rx_at: Instant,
    /// rc.187 — for a RELAY carrier: our own coturn-relayed address (the worker
    /// we allocated on) and the peer's relayed address we dial. `None` for a
    /// direct carrier. Surfaced in the LocalAPI `peers` view so an operator can
    /// see — without a debug-log hunt — which coturn worker each end pinned and
    /// whether a relay pair is same-worker (IPs equal) or cross-worker.
    relay_local: Option<std::net::SocketAddr>,
    relay_dst: Option<std::net::SocketAddr>,
    /// Phase A/C — for an OFF-LINK direct carrier (public-NIC dial OR srflx
    /// punch), the peer's public `ip:port` we dial (or accepted an inbound dial
    /// from). `None` for a same-LAN direct carrier or a relay carrier. It is a
    /// MANDATORY exit-node exemption — an off-link public dst is a real internet
    /// address reached via the default route, NOT on-link like a same-LAN peer,
    /// so the split-default `/1`s would capture the very path to the exit and
    /// self-wedge unless its IP is pinned via the original gateway (see
    /// [`exit_exemption_set`]). Which tier (`Public` vs `Srflx`) it is comes
    /// from [`tier`](Self::tier), not this field (both set it).
    public_direct_dst: Option<std::net::SocketAddr>,
    /// Phase C — which carrier tier this is. Drives the health sweep's tier-split
    /// cooldown (CC1) and the off-link handshake deadline. `Relay` for a coturn
    /// carrier, `Lan`/`Public`/`Srflx` for the three direct tiers.
    tier: DirectTier,
    /// rc.276 diagnostics — did WE initiate this carrier's flow (outbound dial /
    /// our own allocation), or was it adopted from an authenticated INBOUND
    /// dial (accept re-point / accepted-probe promote)? The pc50045 corp-VPN
    /// case turns on exactly this split: outbound-initiated flows are policy-
    /// dropped while inbound-accepted ones pass — and pre-rc.276 the
    /// `initiate` bit was discarded at install, so the peers view couldn't
    /// distinguish them.
    initiated: bool,
    /// rc.276 diagnostics — the sweep's latest `peer_handshake_done` read
    /// (set-once WG session latch, either role). Stamped each tick next to
    /// `stalled`; `false` until the first sweep after install.
    hs_done: bool,
    /// rc.276 diagnostics — the carrier socket's LOCAL address (for a relay
    /// carrier: the allocation's relayed address). Which socket each
    /// direction rides is the exact question the pc50045 field captures need
    /// answered.
    carrier_local: Option<std::net::SocketAddr>,
    /// rc.276 diagnostics — the carrier's send destination (direct: the dial
    /// dst or the accepted peer's observed src; relay: the peer's relayed
    /// address).
    carrier_dst: Option<std::net::SocketAddr>,
    /// rc.276 diagnostics — relay flavor label for the peers view (`"turn"` /
    /// `"derp"`). `None` for direct carriers.
    relay_kind_dbg: Option<&'static str>,
}

impl Installed {
    /// rc.279 — every field's inert default in ONE place. Install sites
    /// override only what they actually decide (`..Installed::base(…)`
    /// struct-update), so a new diagnostics field lands HERE instead of in
    /// every literal (adding `stalled`, then `initiated`, each swept ~20
    /// literals in this file and broke E0063 twice in a single session).
    /// `is_direct` is derived (`tier != Relay`) — every site agreed on that
    /// mapping, including the two that computed `tier` FROM `is_direct`.
    fn base(pubkey: [u8; 32], overlay_ip: Ipv4Addr, tier: DirectTier, now: Instant) -> Self {
        Installed {
            pubkey,
            overlay_ip,
            is_direct: !matches!(tier, DirectTier::Relay),
            since: now,
            last_traffic: (0, 0),
            bad_sweeps: 0,
            stalled: false,
            initiated: false,
            hs_done: false,
            carrier_local: None,
            carrier_dst: None,
            relay_kind_dbg: None,
            last_rx_at: now,
            relay_local: None,
            relay_dst: None,
            public_direct_dst: None,
            tier,
        }
    }
}

/// Phase C (D8) — re-run the direct-upgrade evaluation every Nth fallback tick
/// (6 × [`FALLBACK_TICK`] ≈ 30 s). A lapsed suppression penalty otherwise only
/// matters when the next netmap happens to arrive, so a quiet mesh would never
/// re-attempt direct after a fallback; this drives that retry (and Phase C
/// punch convergence at large install skew) without a netmap.
const REUPGRADE_EVERY_N_TICKS: u32 = 6;
/// rc.139 — a dead RELAY carrier (one-way, same `tx>rx` signal) is usually a
/// STALE coturn port: the peer re-allocated (restart/churn → new port) and we
/// kept dialing the old one. Refresh it (re-request → fresh allocation, re-dial
/// the peer's CURRENT address) — but not more than once per this window, so two
/// ends each refreshing don't ping-pong faster than they can converge.
const RELAY_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
/// How often the carrier-health sweep runs. Cheap (lock-free atomic reads), so
/// a tighter cadence is fine and makes detection quicker.
const FALLBACK_TICK: Duration = Duration::from_secs(5);
/// P8 — resume-from-suspend detection threshold: across one sweep interval,
/// wall-clock advancing this much MORE than the monotonic clock means the host
/// slept in between (monotonic clocks exclude suspend on Windows and Linux —
/// QPC and CLOCK_MONOTONIC both stop). Well above any NTP step correction.
const RESUME_SKEW_THRESHOLD: Duration = Duration::from_secs(120);

/// P8 — did the host suspend between two sweep samples? Pure (tested with
/// synthetic deltas). On resume every installed carrier is dead (NAT
/// mappings/firewall pinholes expired, peers tore their ends down while we
/// slept) and every cooldown is stale evidence — the caller drops both and
/// re-coordinates from scratch, exactly like a fresh session but without
/// losing the WS or the TUN.
fn resumed_from_suspend(mono_elapsed: Duration, wall_elapsed: Duration) -> bool {
    wall_elapsed > mono_elapsed + RESUME_SKEW_THRESHOLD
}
/// P1 (S6) — outbound TUN→pump queue depth. Sized for a full burst of
/// in-flight packets between the reader and the pump; the reader-side
/// backpressure warn fires if the pump ever lets it fill.
const OUTBOUND_QUEUE_PKTS: usize = 512;

/// rc.211 (re-scoped by P1/S6) — slow-handler watchdog threshold. Since P1 the
/// data plane runs entirely off-loop (reader → outbound pump → carriers), so a
/// slow select! arm can no longer delay packets — a trip here now means
/// CONTROL-PLANE convergence is late (carrier repair, netmap install,
/// inbound-init answering — the last still latency-relevant at the ~5 s WG
/// init retransmit). The same threshold guards the pump's per-packet send
/// (`pump:send_ip_packet`) and the reader-side backpressure twin, where a trip
/// IS a data-plane incident (a wedged carrier send). Permanent telemetry: this
/// is the tripwire that catches anyone re-coupling work onto the pump or
/// fattening a control arm.
const LOOP_STALL_WARN_MS: u128 = 250;

/// rc.211 — log a named slow handler / slow send (see [`LOOP_STALL_WARN_MS`]).
fn warn_if_slow(stage: &'static str, t0: Instant) {
    let ms = t0.elapsed().as_millis();
    if ms > LOOP_STALL_WARN_MS {
        warn!(
            stage,
            ms,
            "overlay: handler ran long (control-plane latency; data plane unaffected unless stage is pump:*)"
        );
    }
}

/// P1 (S6) test hook — when non-zero, the Netmap arm sleeps this long at its
/// top (async — the arm is genuinely busy on the loop for the duration).
/// Lets `control_stall_does_not_delay_outbound` prove a fat control arm can
/// no longer delay outbound packets. Zero (inert) outside tests.
#[cfg(test)]
static TEST_NETMAP_STALL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// rc.211 — a finished OFF-LOOP QUIC-over-TURN carrier build (see
/// [`RelayBuildQueue`]). `quic: None` = the QUIC handshake failed/timed out →
/// the commit installs the link's already-built raw relay carrier (today's
/// fallback semantics, unchanged — just no longer blocking the loop).
struct BuiltRelay {
    epoch: u64,
    link: ReadyLink,
    quic: Option<Arc<Carrier>>,
}

/// P2 (rc.211 + rc.218 merged) — epoch-token ABA bookkeeping for OFF-LOOP work,
/// generic over the completion payload. One instance per off-loop pipeline:
///
/// * `in_flight` maps node → the epoch stamped at spawn; a completion commits
///   ONLY if its epoch is still current, so any invalidating event (peer
///   removed, direct carrier installed / `coord.forget`) simply removes the
///   entry and the stale completion is dropped on arrival — immune to the
///   forget→re-request ABA a plain "is in flight" set would have.
/// * Call sites that would start duplicate work consult `in_flight` first
///   (see the per-alias docs below for each pipeline's specifics).
struct EpochQueue<T> {
    in_flight: HashMap<ObjectId, u64>,
    epoch: u64,
    tx: mpsc::Sender<T>,
}

impl<T> EpochQueue<T> {
    /// Stamp new in-flight work for `node` (supersedes any prior stamp).
    fn stamp(&mut self, node: ObjectId) -> u64 {
        self.epoch += 1;
        self.in_flight.insert(node, self.epoch);
        self.epoch
    }
    /// Invalidate any in-flight work for `node` — its completion will be
    /// dropped on arrival.
    fn invalidate(&mut self, node: &ObjectId) {
        self.in_flight.remove(node);
    }
    /// `true` iff `(node, epoch)` is still the CURRENT work for its peer;
    /// clears the entry either way (the completion consumes the slot).
    fn take_if_current(&mut self, node: &ObjectId, epoch: u64) -> bool {
        if self.in_flight.get(node) == Some(&epoch) {
            self.in_flight.remove(node);
            true
        } else {
            false
        }
    }
}

/// rc.211 — OFF-LOOP relay carrier builds. The QUIC-over-TURN rendezvous
/// (`Carrier::quic_relay`, capped at [`QUIC_BUILD_TIMEOUT`] = 8 s) used to run
/// INLINE on the steady-state select! loop — the field-proven head-of-line
/// stall behind the 1–2 s overlay RTT plateaus (the S1 watchdog named it five
/// times at 8.06 s in one 150 s run). `install_ready` spawns the build and the
/// completion is committed by a dedicated select! arm. `install_peers`' relay-
/// coordination branch checks `in_flight` so it never spawns a DUPLICATE
/// coordination for a peer whose carrier is mid-build (post-`try_build` the
/// coordinator no longer tracks the peer, so `!is_tracking` alone would
/// re-request during the 8 s window).
type RelayBuildQueue = EpochQueue<BuiltRelay>;

/// rc.218 — a finished OFF-LOOP relay ALLOCATE (see [`RelayAllocQueue`]).
/// `conn: None` = every TURN candidate failed/timed out → the commit arm
/// `forget`s the peer so the next netmap/sweep tick re-requests cleanly
/// (replacing the old park-in-`pending`-forever, which left a peer whose one
/// allocate failed PERMANENTLY blocked — nothing ever cleared `pending`).
struct AllocDone {
    epoch: u64,
    node_id: ObjectId,
    conn: Option<Arc<dyn crate::transport::relay::RelayConn>>,
}

/// rc.218 — OFF-LOOP relay allocates. rc.211 moved the QUIC-over-TURN build
/// off-loop and flagged `on_grant`'s inline DNS + TURN allocate (UDP 5 s →
/// TURNS/TCP 6 s caps per candidate) as "next in line if it ever fires in the
/// field" — it did: pc50045's rc.213–216 logs still show `stalled the data
/// plane` from exactly this await on its hostile corp path. The `RelayGrant`
/// arm stashes the creds (`grant_accept`, sync), spawns
/// [`RelayCoordinator::allocate_for_pair`], and a dedicated select! arm
/// commits the result (`commit_alloc`, µs). Same epoch-token ABA guard, with
/// NO per-site invalidation plumbing except the P7 force-DERP conversion
/// (`invalidate` — a stale `AllocDone` landing after the pair re-cycled into
/// `pending` would otherwise commit a TURN link inside the forced window; the
/// orphan coturn allocation idles out at the server's TTL): LAST GRANT WINS
/// (every grant re-stamps, superseding any in-flight allocate), a stale
/// completion drops on epoch mismatch, and `commit_alloc` requiring the peer
/// still in `pending` drops the forgotten-not-re-requested case. While an
/// allocate is in flight the peer stays in `pending`, so `is_tracking` keeps
/// deduping re-requests exactly as before.
type RelayAllocQueue = EpochQueue<AllocDone>;
/// Phase B — per-socket STUN attempt timeout when gathering srflx candidates at
/// startup. `srflx_query` retries a few times, so worst-case per socket is a
/// small multiple of this; the whole gather is additionally bounded by
/// [`SRFLX_GATHER_BUDGET`] so an unreachable STUN server can't stall the join.
const SRFLX_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(700);
/// Phase B — overall wall-clock cap on the startup srflx gather across all
/// sockets. The common case (coturn reachable) resolves on the first attempt
/// per socket in tens of ms; this only bounds the pathological all-unreachable
/// case so the runtime never blocks the netmap→install path for long.
const SRFLX_GATHER_BUDGET: Duration = Duration::from_secs(4);

/// Overlay control events the runtime consumes, fed in from the node's
/// signaling loop (the `ServerMsg::Overlay*` handlers forward these).
#[derive(Debug, Clone)]
pub enum OverlayEvent {
    /// Full snapshot — carries the node's own `self_ip`, so the first one
    /// triggers TUN bring-up.
    Netmap {
        self_ip: String,
        network: OverlayNetworkInfo,
        peers: Vec<NetmapPeer>,
    },
    /// Incremental update.
    NetmapDelta {
        upserts: Vec<NetmapPeer>,
        removes: Vec<ObjectId>,
    },
    /// Coturn creds for a relay leg to `peer_node_id` (relay mode only).
    /// `pair_key` is the server's symmetric `sorted(a,b)` key — both ends
    /// receive an identical value and use it to pick the same coturn worker.
    RelayGrant {
        peer_node_id: ObjectId,
        ice_servers: Vec<IceServer>,
        pair_key: String,
    },
    /// P7 — server-pushed per-pair DERP escalation: the server observed
    /// sustained TURN churn for this pair and tells BOTH ends to pin it onto
    /// the DERP carrier for `ttl_ms`. Pushed (never grant-borne) because the
    /// single-relay DIALER never sends a relay_request and so never sees a
    /// grant.
    ForceDerp { peer_node_id: ObjectId, ttl_ms: u64 },
}

/// Builds the WG carrier for a peer. Production wires a direct UDP socket
/// or a coturn relay; tests inject pre-wired loopback carriers. Returning
/// `None` skips the peer (it is retried on the next netmap that lists it).
#[async_trait]
pub trait LinkFactory: Send + Sync {
    async fn build_carrier(&self, peer: &PeerConfig) -> Option<Arc<Carrier>>;
}

/// Creates the TUN once the node's overlay IP is known. Production
/// returns `SystemTun`; tests return a mock. Boxed so the runtime stays
/// device-agnostic. Args: `(self_ip, netmask, mtu)`.
pub type TunFactory =
    Box<dyn Fn(Ipv4Addr, Ipv4Addr, u16) -> std::io::Result<Arc<dyn TunIo>> + Send + Sync>;

/// IPv4 netmask for a CIDR prefix length (e.g. `10` → `255.192.0.0`).
fn netmask_for_prefix(prefix: u8) -> Ipv4Addr {
    if prefix == 0 {
        return Ipv4Addr::UNSPECIFIED;
    }
    Ipv4Addr::from(!0u32 << (32 - u32::from(prefix.min(32))))
}

/// Prefix length out of a `"a.b.c.d/n"` CIDR string.
fn prefix_of_cidr(cidr: &str) -> Option<u8> {
    cidr.split_once('/')
        .and_then(|(_, p)| p.trim().parse().ok())
}

/// Phase 2 MagicDNS — rebuild the resolver's `name → overlay-IP` map from the
/// current netmap peers (named peers only). Called after each netmap change.
async fn sync_name_map(names: &dns::NameMap, peers: &HashMap<ObjectId, NetmapPeer>) {
    let mut map = names.write().await;
    map.clear();
    for p in peers.values() {
        if p.name.is_empty() {
            continue;
        }
        if let Ok(ip) = p.overlay_ip.parse::<Ipv4Addr>() {
            map.insert(p.name.clone(), ip);
        }
    }
}

/// Phase C (D5) — the srflx keepalive / re-gather task. Every `interval`
/// (jittered) it re-runs a STUN Binding on the PUNCH socket (through the demux
/// STUN sink) to (a) hold the NAT mapping open on an idle link — WG keepalives
/// only cover ACTIVE sessions — and (b) detect a CHANGED public mapping and
/// re-advertise it, so a peer that joins later dials the live srflx, not a dead
/// one.
///
/// The STUN target is PINNED (A4): re-resolved only after several consecutive
/// failures, so a multi-worker DNS rotation can't masquerade as a mapping change
/// and fan a network-wide re-trickle every tick. On failure the last-known
/// advert is RETAINED (a transient STUN outage must not strip a working srflx).
/// Re-trickles ONLY when the punch mapping (`[0]`) actually changes. Ends when
/// the control channel closes (runtime gone).
#[allow(clippy::too_many_arguments)]
async fn run_srflx_keepalive(
    punch_sock: Arc<UdpSocket>,
    mut stun_rx: mpsc::Receiver<crate::transport::stun::StunInbound>,
    mut stun_server: SocketAddr,
    stun_urls: Vec<String>,
    own_ips: Vec<Ipv4Addr>,
    mut advertised: Vec<String>,
    nat: Option<String>,
    outbound: mpsc::Sender<ClientMsg>,
    interval: Duration,
) {
    const RERESOLVE_AFTER: u32 = 3;
    let mut failures: u32 = 0;
    loop {
        // Small jitter (≤25% of the interval) so a fleet doesn't STUN in
        // lockstep; scaled to the interval so short test intervals stay quick.
        let jitter =
            Duration::from_millis(rand::random::<u64>() % (interval.as_millis() as u64 / 4 + 1));
        tokio::time::sleep(interval + jitter).await;
        match crate::transport::stun::srflx_query_via_sink(
            &punch_sock,
            &mut stun_rx,
            stun_server,
            SRFLX_ATTEMPT_TIMEOUT,
        )
        .await
        {
            Ok(mapped) => {
                failures = 0;
                let ep = mapped.to_string();
                if advertised.first().map(String::as_str) != Some(ep.as_str()) {
                    // Mapping changed → update the punch candidate `[0]` and
                    // re-advertise (keeping any other multi-homed candidates).
                    if advertised.is_empty() {
                        advertised.push(ep.clone());
                    } else {
                        advertised[0] = ep.clone();
                    }
                    info!(new_srflx = %ep, "overlay: srflx mapping changed — re-advertising (Phase C keepalive)");
                    if outbound
                        .send(ClientMsg::OverlaySrflx {
                            candidates: advertised.clone(),
                            // Re-send our NAT type — the mapping changed, not the
                            // NAT class — so the server never clears it.
                            nat: nat.clone(),
                        })
                        .await
                        .is_err()
                    {
                        break; // control channel closed → runtime gone
                    }
                }
            }
            Err(e) => {
                failures += 1;
                debug!(%e, failures, "overlay: srflx keepalive query failed — retaining last advert");
                if failures >= RERESOLVE_AFTER {
                    if let Some(fresh) = direct::resolve_stun_server(&stun_urls, &own_ips).await {
                        stun_server = fresh;
                    }
                    failures = 0;
                }
            }
        }
    }
}

/// How the runtime obtains a carrier for each peer.
enum CarrierMode {
    /// Direct/test: a stateless [`LinkFactory`] builds the carrier
    /// immediately (loopback in tests).
    Direct(Arc<dyn LinkFactory>),
    /// Production: coturn relay coordination ([`RelayCoordinator`]) —
    /// field-pending.
    Relay,
}

/// One node's overlay runtime. Construct with [`OverlayRuntime::new`] (or
/// [`new_relay`](OverlayRuntime::new_relay)), then
/// `tokio::spawn(rt.run(events, endpoints))`.
pub struct OverlayRuntime {
    keypair: WgKeypair,
    outbound: mpsc::Sender<ClientMsg>,
    mode: CarrierMode,
    tun_factory: TunFactory,
    mtu: u16,
    /// Phase 1 — subnet CIDRs this node advertises as a router (from config).
    /// Sent in the join; the server gates them behind admin approval.
    advertised_routes: Vec<String>,
    /// Unification P1 — where to publish this node's live overlay view (self
    /// IP + peers with connection type) for the daemon's LocalAPI. `None` in
    /// test / direct mode (nothing reads it there).
    peer_view: Option<watch::Sender<OverlayView>>,
    /// P5 exit-node CLIENT opt-in — the mesh peer (its [`NetmapPeer::name`] or
    /// node-id hex) this node routes ALL its internet egress through. `None` =
    /// today's behaviour (no default routing). Only takes effect once the named
    /// peer is present, reachable, has a live carrier, AND is an admin-approved
    /// exit node (its netmap `routes` carry `0.0.0.0/0`). See
    /// [`OverlayRuntime::reconcile_exit_routing`].
    exit_node: Option<String>,
    /// P5 exit-node — carrier-critical endpoint IPs that MUST stay on the
    /// physical uplink (exempted from the split-default) for the mesh to survive
    /// exit routing: the coordination server's resolved A-records, provided by
    /// the agent (which knows `server_url`) BEFORE any `/1` is installed, so DNS
    /// still worked when they were resolved. Coturn worker IPs are added
    /// dynamically from live relay carriers. Empty unless `exit_node` is set.
    exit_server_ips: Vec<IpAddr>,
    /// Phase D (DERP) — a factory that OPENS this node's `/derp` WS + returns
    /// its demux, called LAZILY by [`run`](Self::run) only when the node is
    /// itself UDP-blocked (its srflx gather found nothing). A UDP-capable node
    /// can never be in a both-UDP-blocked pair, so it never needs DERP — this
    /// way it doesn't hold an idle `/derp` WS. `None` (no factory / not called)
    /// ⇒ no DERP; the coordinator falls through to both-allocate. Set via
    /// [`with_derp_mux_factory`](Self::with_derp_mux_factory).
    derp_mux_factory: Option<DerpMuxFactory>,
    /// P3 PR-A — the shadow [`path::PathMonitor`] + divergence bookkeeping.
    /// Behind a Mutex (not a field of the loop) because the feed sites are
    /// `&self` methods whose signatures are frozen by the timer-parity tests;
    /// accessed ONLY through [`Self::shadow`] (sync closure — the guard
    /// cannot cross an await). Uncontended: the overlay loop is the only
    /// caller.
    path_shadow: std::sync::Mutex<PathShadow>,
    /// S2 — the MagicDNS bring-up outcome, set once by [`run`](Self::run)
    /// after the resolver/OS-steer attempt and grafted onto every
    /// published [`OverlayView`]. `None` when MagicDNS is off.
    dns_status: Option<DnsStatus>,
}

/// Opens the node's `/derp` WS (the agent owns `server_url` + the token +
/// `tokio_tungstenite`) and returns the connected [`DerpMux`]. Boxed +
/// agent-provided so `tunnel-core` stays WebSocket-free; [`OverlayRuntime::run`]
/// calls it AT MOST ONCE, and only for a UDP-blocked node (lazy `/derp`).
///
/// `Send + Sync`: the `run` future keeps `&self` alive across awaits and is
/// spawned onto the multi-thread runtime, so `OverlayRuntime` (and thus this
/// factory) must be `Sync`. The agent's closure captures only `Sync` values
/// (`String` server-url/token + the 32-byte pubkey), so it satisfies both.
pub type DerpMuxFactory = Box<dyn FnOnce() -> Arc<DerpMux> + Send + Sync>;

/// Map the runtime's live carrier bookkeeping into the LocalAPI [`OverlayView`]
/// — the daemon-internal shape the `roomler status` / `peers` verbs read. Pure
/// (no I/O / no `self`) so the [`ConnectionType`] classification is unit-tested
/// directly. `current_peers` (the netmap) is authoritative for membership;
/// `by_node` tells us HOW we currently reach each one:
/// - installed **direct** carrier → [`ConnectionType::Direct`]
/// - installed **relay** carrier → [`ConnectionType::Relay`]
/// - known + server-reachable but no carrier yet (relay pending, cooling down)
///   → [`ConnectionType::Blocked`]
/// - not server-reachable → [`ConnectionType::Offline`]
///
/// `Tunnel` is never produced here — that's the userspace-tunnel fallback the
/// daemon labels once the tunnel-client folds in (P3). `rtt_ms` isn't tracked by
/// the runtime (the daemon fills it from an ICMP prober); `last_seen_ms` is the
/// absolute epoch-ms of the peer's last inbound packet (P3b-3), `None` for a peer
/// with no installed carrier.
///
/// `now` + `epoch_now_ms` are the monotonic + wall-clock references captured by
/// the caller ([`publish_view`]); passed in (not read here) so this stays a pure
/// function the tests can drive with a fixed clock.
fn build_overlay_view(
    self_ip: &str,
    by_node: &HashMap<ObjectId, Installed>,
    current_peers: &HashMap<ObjectId, NetmapPeer>,
    probing: &HashMap<ObjectId, UpgradeProbe>,
    now: Instant,
    epoch_now_ms: u64,
) -> OverlayView {
    let mut peers: Vec<PeerInfo> = current_peers
        .values()
        .map(|np| {
            let inst = by_node.get(&np.node_id);
            let connection = match inst {
                Some(inst) if inst.is_direct => ConnectionType::Direct,
                Some(_) => ConnectionType::Relay,
                None if np.reachable => ConnectionType::Blocked,
                None => ConnectionType::Offline,
            };
            // Absolute epoch-ms of the last inbound packet (what the CLI's
            // `fmt_last_seen` expects). Only a peer with an installed carrier
            // has an `last_rx_at`; Blocked/Offline stay `None`.
            let last_seen_ms = inst.map(|inst| {
                let age_ms = now.saturating_duration_since(inst.last_rx_at).as_millis() as u64;
                epoch_now_ms.saturating_sub(age_ms)
            });
            PeerInfo {
                node_id: np.node_id.to_hex(),
                name: np.name.clone(),
                overlay_ip: (!np.overlay_ip.is_empty()).then(|| np.overlay_ip.clone()),
                overlay_ip6: derived_v6_of(&np.overlay_ip),
                online: np.reachable,
                connection,
                // P8-cosmetics — a relay-carried peer with an MBB probe in
                // flight renders as `upgrading` in the CLI, so a snapshot
                // taken mid-transition reads as what it is.
                upgrading: connection == ConnectionType::Relay && probing.contains_key(&np.node_id),
                // rc.275 honesty — the sweep's silently-one-way verdict.
                stalled: inst.is_some_and(|i| i.stalled),
                rtt_ms: None,
                last_seen_ms,
                // P3b-3 — carry the backing agent id (hex) so the daemon can join
                // this peer to a tunnel flow and label it `Tunnel`.
                agent_id: np.agent_id.map(|a| a.to_hex()),
                // rc.187 — relay endpoints (relay carriers only) so `peers --json`
                // shows each end's coturn worker; same IP on both = same-worker.
                relay_local: inst.and_then(|i| i.relay_local).map(|a| a.to_string()),
                relay_dst: inst.and_then(|i| i.relay_dst).map(|a| a.to_string()),
                // rc.276 diagnostics — the carrier forensic snapshot (JSON-only).
                debug: inst.map(|i| PeerCarrierDebug {
                    tier: match i.tier {
                        DirectTier::Lan => "lan",
                        DirectTier::Public => "public",
                        DirectTier::Srflx => "srflx",
                        DirectTier::Relay => "relay",
                    }
                    .to_string(),
                    initiated: i.initiated,
                    hs_done: i.hs_done,
                    local: i.carrier_local.map(|a| a.to_string()),
                    dst: i.carrier_dst.map(|a| a.to_string()),
                    tx: i.last_traffic.0,
                    rx: i.last_traffic.1,
                    last_rx_age_s: now.saturating_duration_since(i.last_rx_at).as_secs(),
                    relay_kind: i.relay_kind_dbg.map(str::to_string),
                }),
            }
        })
        .collect();
    // Stable order so a LocalAPI reader doesn't see the list jitter between
    // otherwise-identical reads (HashMap iteration order is nondeterministic).
    peers.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    OverlayView {
        self_ip: (!self_ip.is_empty()).then(|| self_ip.to_string()),
        self_ip6: derived_v6_of(self_ip),
        peers,
        // Set by `publish_view` from the runtime's exit-routing state (S4)
        // and DNS bring-up state (S2).
        exit_node: None,
        dns: None,
    }
}

/// The *derived* overlay IPv6 for an overlay-v4 string ([`derive_overlay_v6`]
/// as display text), or `None` for an empty/unparseable one. The runtime is the
/// single place the daemon-facing view learns v6 addresses — the daemon and its
/// clients (CLI / tray) render them without needing the `overlay` feature.
fn derived_v6_of(overlay_ip: &str) -> Option<String> {
    overlay_ip
        .parse::<Ipv4Addr>()
        .ok()
        .map(|v4| super::router::derive_overlay_v6(v4).to_string())
}

impl OverlayRuntime {
    /// Direct/test runtime: carriers come from `links`.
    pub fn new(
        keypair: WgKeypair,
        outbound: mpsc::Sender<ClientMsg>,
        links: Arc<dyn LinkFactory>,
        tun_factory: TunFactory,
        mtu: u16,
    ) -> Self {
        Self {
            keypair,
            outbound,
            mode: CarrierMode::Direct(links),
            tun_factory,
            mtu,
            advertised_routes: Vec::new(),
            peer_view: None,
            exit_node: None,
            exit_server_ips: Vec::new(),
            derp_mux_factory: None,
            path_shadow: std::sync::Mutex::new(PathShadow::new()),
            dns_status: None,
        }
    }

    /// Production runtime: carriers come from the coturn relay
    /// coordination (field-pending).
    pub fn new_relay(
        keypair: WgKeypair,
        outbound: mpsc::Sender<ClientMsg>,
        tun_factory: TunFactory,
        mtu: u16,
    ) -> Self {
        Self {
            keypair,
            outbound,
            mode: CarrierMode::Relay,
            tun_factory,
            mtu,
            advertised_routes: Vec::new(),
            peer_view: None,
            exit_node: None,
            exit_server_ips: Vec::new(),
            derp_mux_factory: None,
            path_shadow: std::sync::Mutex::new(PathShadow::new()),
            dns_status: None,
        }
    }

    /// P3 PR-A (re-scoped by PR-E) — run a SYNC closure against the path
    /// monitor. Always runs: the monitor IS the selector now, so `off` can no
    /// longer skip the feed (it only silences telemetry inside PathShadow's
    /// own methods). The lock/drop pair lives entirely inside this call, so
    /// no caller can hold the guard across an await; the closure being
    /// `FnOnce(&mut PathShadow) -> R` (not async) makes that structural.
    /// Returns `Option` purely for call-site compatibility — always `Some`.
    fn shadow<R>(&self, f: impl FnOnce(&mut PathShadow) -> R) -> Option<R> {
        let mut s = self.path_shadow.lock().unwrap_or_else(|p| p.into_inner());
        Some(f(&mut s))
    }

    /// Phase 1 — set the subnet routes this node advertises as a router.
    pub fn with_advertised_routes(mut self, routes: Vec<String>) -> Self {
        self.advertised_routes = routes;
        self
    }

    /// Phase D (DERP) — attach a factory that opens the node's `/derp` WS. The
    /// runtime calls it LAZILY, only when this node is itself UDP-blocked, so a
    /// UDP-capable node never opens an idle `/derp` WS. The factory (agent-side,
    /// owning `server_url`/token/`tokio_tungstenite`) creates the [`DerpMux`],
    /// opens the WS, and returns the mux for the relay coordinator to vend
    /// `DerpConn` carriers. `None` (the default) leaves DERP inert.
    pub fn with_derp_mux_factory(mut self, factory: Option<DerpMuxFactory>) -> Self {
        self.derp_mux_factory = factory;
        self
    }

    /// P5 exit-node CLIENT opt-in — route this node's default internet egress
    /// through `exit_node` (a peer's [`NetmapPeer::name`] or node-id hex).
    /// `server_ips` are the coordination server's already-resolved IPs (the agent
    /// resolves `server_url` before the mesh forms — while the uplink is still
    /// clean — so they can be exempted from the split-default). Both `None` /
    /// empty (test / non-exit nodes) leaves exit routing entirely inert.
    pub fn with_exit_node(mut self, exit_node: Option<String>, server_ips: Vec<IpAddr>) -> Self {
        self.exit_node = exit_node;
        self.exit_server_ips = server_ips;
        self
    }

    /// Unification P1 — publish this node's live overlay view (self IP + peers
    /// with connection type) on `tx` so the daemon's LocalAPI can answer
    /// `roomler status` / `peers`. The runtime republishes on join and after
    /// every netmap / carrier-state change. Unset (test / direct mode) → the
    /// runtime publishes nothing.
    pub fn with_peer_view(mut self, tx: watch::Sender<OverlayView>) -> Self {
        self.peer_view = Some(tx);
        self
    }

    /// Rebuild + publish the [`OverlayView`] if a LocalAPI receiver is wired.
    /// Cheap (a few-element Vec + a `watch` replace); called at each point the
    /// netmap or a carrier changes. The `watch` keeps only the latest value, so
    /// coalescing bursts is automatic.
    fn publish_view(
        &self,
        self_ip: &str,
        by_node: &HashMap<ObjectId, Installed>,
        current_peers: &HashMap<ObjectId, NetmapPeer>,
        probing: &HashMap<ObjectId, UpgradeProbe>,
        exit_status: Option<ExitNodeStatus>,
    ) {
        if let Some(tx) = &self.peer_view {
            // Capture both clocks together so `last_seen_ms` (absolute epoch-ms)
            // is derived from the same instant the monotonic ages are measured
            // against. `UNIX_EPOCH` is monotonic-safe here (a backwards wall
            // clock only makes a last_seen look slightly newer).
            let now = Instant::now();
            let epoch_now_ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let mut view =
                build_overlay_view(self_ip, by_node, current_peers, probing, now, epoch_now_ms);
            // S4 — the exit-node routing status the runtime holds (the view
            // builder is pure over peers, so this is grafted on after).
            view.exit_node = exit_status;
            // S2 — ditto for the DNS bring-up outcome.
            view.dns = self.dns_status.clone();
            // send_replace never fails (unlike send) even if the receiver is
            // transiently absent, and keeps the value for the next borrow.
            tx.send_replace(view);
        }
    }

    /// Run until the event channel closes (WS disconnect). Sends
    /// `OverlayJoin`, waits for the first full netmap (which yields the
    /// node's overlay IP), brings up the TUN + inbound writer, then
    /// steady-state pumps TUN traffic and applies netmap deltas.
    pub async fn run(mut self, mut events: mpsc::Receiver<OverlayEvent>, endpoints: Vec<String>) {
        // rc.131 — direct LAN path: bind a shared UDP socket + discover our
        // LAN endpoint so a same-subnet peer dials us directly and skips the
        // relay. Off in Direct mode (the test/helper path) and when disabled.
        // `mut` — the srflx gather (below, after the first netmap) records the
        // punch socket into it (Phase C).
        let mut direct_ctx = self.setup_direct().await;
        let mut advertised = endpoints;
        if let Some(ctx) = &direct_ctx {
            advertised.extend(ctx.endpoints.iter().cloned());
        }

        let join = ClientMsg::OverlayJoin {
            network_hint: None,
            wg_public_key: self.keypair.public_base64(),
            key_epoch: 0,
            supported: vec!["wireguard-v1".to_string()],
            mtu: self.mtu,
            endpoints: advertised,
            // rc.142 — advertise the QUIC-over-TURN capability so the server
            // only tells a peer to attempt QUIC when BOTH ends support it.
            supports_quic: overlay_quic_enabled(),
            // Phase D — advertise the single-relay capability (our OVERLAY_RELAY_SINGLE
            // flag) so the server only lets a peer pick single-relay when BOTH ends
            // opted in; a mixed pair stays on the both-allocate relay.
            // rc.276 — forced-TLS vetoes single-relay: the raw-UDP DIALER
            // role is the exact flow shape the affected hosts can't send,
            // and advertising the veto keeps both ends' strategy symmetric.
            supports_relay_single: crate::overlay::direct::relay_single_enabled()
                && !crate::overlay::direct::relay_tls_forced(),
            // Phase D (DERP) — advertise the DERP capability (our OVERLAY_DERP
            // flag) so a both-UDP-blocked pair only picks DERP when BOTH ends
            // opted in. Default-OFF until field-proven.
            supports_derp: crate::overlay::direct::derp_enabled(),
            // P7 — this build honors the server's per-pair `OverlayForceDerp`
            // escalation push. Same local flag as `supports_derp` (a node with
            // DERP disabled can't be force-pinned onto it).
            supports_forced_derp: crate::overlay::direct::derp_enabled(),
            // Phase 1 — subnet routes we offer (admin must approve server-side).
            advertised_routes: self.advertised_routes.clone(),
        };
        if self.outbound.send(join).await.is_err() {
            warn!("overlay: control channel closed before join");
            return;
        }
        info!("overlay: rc:overlay.join sent");

        // Phase 1 — wait for the first full netmap (it carries self_ip).
        let (self_ip, network, first_peers) = loop {
            match events.recv().await {
                Some(OverlayEvent::Netmap {
                    self_ip,
                    network,
                    peers,
                }) => break (self_ip, network, peers),
                Some(OverlayEvent::NetmapDelta { .. }) => continue, // pre-netmap; ignore
                Some(OverlayEvent::RelayGrant { .. }) => continue,  // pre-netmap; ignore
                Some(OverlayEvent::ForceDerp { .. }) => continue,   // pre-netmap; ignore
                None => return,
            }
        };

        let Ok(self_v4) = self_ip.parse::<Ipv4Addr>() else {
            warn!(%self_ip, "overlay: server sent a non-IPv4 self_ip; aborting runtime");
            return;
        };
        let netmask = netmask_for_prefix(prefix_of_cidr(&network.cidr).unwrap_or(10));

        let (mut wg, tun_rx) = WgDevice::new(self.keypair.secret.clone());
        let tun: Arc<dyn TunIo> = match (self.tun_factory)(self_v4, netmask, self.mtu) {
            Ok(t) => t,
            Err(e) => {
                warn!(%e, %self_v4, "overlay: TUN bring-up failed; aborting runtime");
                return;
            }
        };
        info!(%self_v4, mtu = self.mtu, "overlay: TUN up");

        // Phase 1 — if this node advertises subnet routes, turn on IP forwarding
        // + NAT so overlay peers can reach the LANs it fronts. Held for the
        // runtime's lifetime; its `Drop` reverts on WS disconnect / shutdown.
        let _subnet_router = super::nat::enable(&network.cidr, &self.advertised_routes).await;

        // Inbound writer: decrypted packets → TUN. Independent of the
        // device, so it's a plain spawned task.
        let writer_tun = tun.clone();
        let inbound = tokio::spawn(async move {
            let mut rx = tun_rx;
            while let Some(pkt) = rx.recv().await {
                if let Err(e) = writer_tun.write_packet(&pkt).await {
                    debug!(%e, "overlay: TUN write failed; inbound writer exiting");
                    break;
                }
            }
        });

        // node_id → installed carrier (pubkey/IP/kind/install-time).
        let mut by_node: HashMap<ObjectId, Installed> = HashMap::new();
        // rc.139 — peers whose stale relay was just refreshed (anti-ping-pong).
        let mut relay_refresh_cooldown: HashMap<ObjectId, Instant> = HashMap::new();
        // rc.208 — in-flight make-before-break upgrade probes (node → metadata).
        // The shadow carriers live in `WgDevice::probes`; this tracks tier +
        // deadline for `sweep_upgrade_probes`. Empty unless the feature is on.
        let mut upgrade_probes: HashMap<ObjectId, UpgradeProbe> = HashMap::new();

        // NAT-traversal Phase B/C — gather our server-reflexive (srflx)
        // candidates and advertise them, so a peer behind a DIFFERENT NAT can
        // dial us at the public mapping our own STUN query opens, AND record the
        // PUNCH SOCKET (the interface socket that owns our first candidate) so we
        // dial a peer's srflx from it (Phase C hole-punch). This MUST run BEFORE
        // the eager demux below starts reading these sockets: the STUN reply
        // rides the same socket the overlay traffic will use (that's the point —
        // the NAT mapping has to match), so a demux recv loop would otherwise
        // steal the response. Best-effort + time-bounded, so a slow/unreachable
        // STUN server just leaves srflx unset this run. WG keepalives hold the
        // mapping for active sessions; Phase C's in-band keepalive (demux-routed
        // STUN, chunk 2) refreshes an idle mapping + re-trickles on change.
        // Captured for the Phase C keepalive task (chunk 2): the pinned STUN
        // server it re-queries, the candidates it started from (so it only
        // re-trickles on a CHANGE), and our probed NAT type (re-sent on each
        // re-trickle so the server never clears it). Empty/None ⇒ no keepalive.
        let mut srflx_stun_server: Option<SocketAddr> = None;
        let mut srflx_advertised: Vec<String> = Vec::new();
        let mut srflx_my_nat: Option<String> = None;
        // Phase D — also gather+advertise our srflx when single-relay is on (even
        // with srflx-direct off): a single-relay DIALER advertises no relay, so
        // the ANCHOR permits its inbound by the IP it learns from our srflx.
        if direct::srflx_gather_active() {
            let socks = direct_ctx
                .as_ref()
                .map(|c| c.socks.clone())
                .unwrap_or_default();
            // Our own interface IPs — excluded as STUN targets so a fleet host
            // co-located with a coturn worker doesn't STUN itself (→ hairpin →
            // false UDP-blocked). See `direct::resolve_stun_server`.
            let own_ips: Vec<Ipv4Addr> = socks.iter().map(|(ip, _)| *ip).collect();
            if !socks.is_empty() && !network.stun_urls.is_empty() {
                match direct::resolve_stun_server(&network.stun_urls, &own_ips).await {
                    Some(stun_server) => {
                        srflx_stun_server = Some(stun_server);
                        let pairs = tokio::time::timeout(
                            SRFLX_GATHER_BUDGET,
                            direct::gather_srflx(&socks, stun_server, SRFLX_ATTEMPT_TIMEOUT),
                        )
                        .await
                        .unwrap_or_default();
                        if pairs.is_empty() {
                            debug!(%stun_server, "overlay: srflx gather yielded no public candidate");
                        } else {
                            // The FIRST pair is the punch socket: its candidate
                            // is advertised at index 0, which the peer's dial-side
                            // (`pick_public_endpoint`) picks first — so both ends
                            // agree on the mapping to punch.
                            let punch = pairs.first().cloned();
                            // Phase C — probe OUR NAT type on the punch socket
                            // (two distinct STUN targets), BEFORE its demux loop
                            // starts (same socket-read race as the gather). A
                            // peer skips the punch only when BOTH ends are
                            // symmetric; `None` (unknown) stays optimistic.
                            let my_nat = if let Some((_, ps)) = &punch {
                                let targets =
                                    direct::resolve_stun_targets(&network.stun_urls, &own_ips)
                                        .await;
                                direct::probe_nat_type(ps, &targets, SRFLX_ATTEMPT_TIMEOUT)
                                    .await
                                    .map(str::to_string)
                            } else {
                                None
                            };
                            srflx_my_nat = my_nat.clone();
                            if let (Some(ctx), Some(first)) = (direct_ctx.as_mut(), punch) {
                                ctx.punch = Some(first);
                                ctx.my_nat = my_nat.clone();
                            }
                            let candidates: Vec<String> =
                                pairs.into_iter().map(|(c, _)| c).collect();
                            srflx_advertised = candidates.clone();
                            info!(?candidates, ?my_nat, %stun_server, "overlay: advertising srflx candidates (NAT-traversal Phase B/C)");
                            let _ = self
                                .outbound
                                .send(ClientMsg::OverlaySrflx {
                                    candidates,
                                    nat: my_nat,
                                })
                                .await;
                        }
                    }
                    None => {
                        debug!(urls = ?network.stun_urls, "overlay: no resolvable STUN server; srflx off this run");
                    }
                }
            }
        }

        // Phase A/B — receiver for AUTHENTICATED inbound direct handshakes (a
        // NAT'd client dialing our public endpoint, or a known peer that roamed
        // to a new ephemeral port — the field-observed stale-port race). Wired
        // when EITHER public-dial tier is on (public-direct or srflx; CC8
        // flag-gate); the demux loops for our own sockets are started EAGERLY
        // here so an inbound INIT is read even before any peer is installed (an
        // exit with no other direct peers would otherwise never spawn a recv
        // loop for its public socket).
        let mut direct_events = if direct_ctx.is_some()
            && (direct::public_direct_enabled() || direct::srflx_enabled())
        {
            if let Some(ctx) = &direct_ctx {
                for (_ip, s) in &ctx.socks {
                    wg.ensure_direct_demux(s.clone());
                }
                if let Some(ps) = &ctx.public_sock {
                    wg.ensure_direct_demux(ps.clone());
                }
            }
            wg.take_direct_events()
        } else {
            None
        };

        // Phase C (D5) — spawn the srflx keepalive/re-gather task. It re-queries
        // the PINNED STUN server on the punch socket every interval (via the
        // demux STUN sink wired just above) to hold an idle NAT mapping open and
        // re-advertise a changed one. Only when: srflx tier on, a punch socket +
        // STUN server resolved, an advert exists, and the interval isn't 0 (off).
        let srflx_keepalive = {
            let secs = direct::srflx_keepalive_secs();
            match (
                direct_ctx.as_ref().and_then(|c| c.punch.clone()),
                srflx_stun_server,
                wg.take_stun_events(),
            ) {
                (Some((_, punch_sock)), Some(stun_server), Some(stun_rx))
                    if direct::srflx_enabled() && secs > 0 && !srflx_advertised.is_empty() =>
                {
                    Some(tokio::spawn(run_srflx_keepalive(
                        punch_sock,
                        stun_rx,
                        stun_server,
                        network.stun_urls.clone(),
                        direct_ctx
                            .as_ref()
                            .map(|c| c.socks.iter().map(|(ip, _)| *ip).collect())
                            .unwrap_or_default(),
                        srflx_advertised.clone(),
                        srflx_my_nat.clone(),
                        self.outbound.clone(),
                        Duration::from_secs(secs),
                    )))
                }
                _ => None,
            }
        };

        // Latest netmap view (node_id → peer), so the fallback sweep can drive
        // the relay path for a downgraded peer without waiting for a netmap.
        let mut current_peers: HashMap<ObjectId, NetmapPeer> =
            first_peers.iter().map(|p| (p.node_id, p.clone())).collect();

        // Phase 2 MagicDNS — if the tenant set a domain, run a local split-DNS
        // resolver bound to our overlay IP:53, point the OS at it for that
        // domain, and keep the resolver's name→IP map synced with the netmap.
        // `None` when MagicDNS is off. `_dns_os_guard` reverts the OS DNS config
        // on runtime exit (WS disconnect / shutdown).
        // P5/Phase2 DNS. Compute the upstream once — the resolver's forward target
        // AND (when MagicDNS is off) the exit-DNS catch-all target. `dns_magic` is
        // the normalised suffix, `None` when MagicDNS is off.
        let dns_upstream = network
            .nameservers
            .iter()
            .find_map(|s| dns::parse_upstream(s))
            .unwrap_or_else(|| SocketAddr::from(([1, 1, 1, 1], 53)));
        let dns_magic: Option<String> = network
            .magic_domain
            .as_deref()
            .map(|d| d.trim_end_matches('.').to_ascii_lowercase())
            .filter(|d| !d.is_empty());
        let mut _dns_os_guard: Option<dns::DnsOsGuard> = None;
        // P5 S4b — did the local resolver actually bind :53? Only meaningful when
        // MagicDNS is on (else exit-DNS steers the public upstream directly). Gates
        // the "." steer so we never point the OS at a dead resolver (→ a total DNS
        // blackhole). Known before the first reconcile (awaited here), so there's
        // no late-bind race to chase.
        let mut dns_bound = false;
        let dns_names: Option<dns::NameMap> = if let Some(magic_domain) = dns_magic.clone() {
            let names: dns::NameMap = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            sync_name_map(&names, &current_peers).await;
            let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(dns::run(
                dns::DnsConfig {
                    bind: SocketAddr::new(self_v4.into(), 53),
                    magic_domain: magic_domain.clone(),
                    upstream: dns_upstream,
                    names: names.clone(),
                    // AAAA (derived overlay v6) default-on; ROOMLER_AGENT_DNS_AAAA=0
                    // reverts to A-only without a rebuild — the mixed-fleet escape
                    // hatch (an old peer's OS doesn't own its derived v6, so v6 to
                    // it blackholes; happy-eyeballs apps fall back, sequential apps
                    // may hang on it).
                    answer_aaaa: crate::env::node_env("DNS_AAAA").as_deref() != Some("0"),
                },
                bound_tx,
            ));
            // The bind is a local UDP bind — microseconds; bound the wait so a hung
            // reactor can't stall the join. Timeout / send-error → not-bound.
            dns_bound = tokio::time::timeout(Duration::from_secs(2), bound_rx)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or(false);
            // Point the OS resolver at us for `<magic_domain>` (reverted on Drop).
            _dns_os_guard = Some(dns::configure_os(self_v4, &magic_domain).await);
            Some(names)
        } else {
            None
        };
        // S2 — record the DNS bring-up outcome for the LocalAPI view
        // (`roomler status` / the desktop's DNS section). `answer_aaaa`
        // recomputes the same expression the resolver was configured with.
        self.dns_status = dns_magic.clone().map(|magic_domain| DnsStatus {
            magic_domain,
            resolver_bound: dns_bound,
            os_steer_active: _dns_os_guard.as_ref().is_some_and(|g| g.active()),
            upstream: dns_upstream.to_string(),
            answer_aaaa: crate::env::node_env("DNS_AAAA").as_deref() != Some("0"),
        });

        let mut fallback = tokio::time::interval(FALLBACK_TICK);
        // P4 — event-driven route guard: subscribe to OS route-table changes
        // (NotifyRouteChange2 / `ip monitor route`) so an erased route is
        // re-asserted within milliseconds instead of at the next blind tick.
        // `None` (env-disabled / platform-unavailable) = pre-P4 behaviour
        // exactly. Waves are rate-limited: our own re-asserts feed the
        // subscription back (see `route_events` module doc).
        let mut route_watch = super::route_events::spawn_route_watch();
        let mut last_route_wave: Option<Instant> = None;
        // rc.146 — re-assert per-peer /32 routes so a full-tunnel VPN can't
        // keep its competing capture routes installed. First tick fires
        // immediately; skip it (routes are freshly installed by
        // `install_peers` below). P4 demotion: with a live subscription the
        // tick is a 30 s belt-and-braces heartbeat (events + their
        // trailing-edge pull do the real catching); without one — or if the
        // watch dies mid-session (the event arm's `None` leg) — it is the
        // 2 s war cadence.
        let route_cadence = route_guard_cadence(
            route_watch.is_some(),
            crate::env::node_env("OVERLAY_ROUTE_TICK_SECS"),
        );
        let mut route_guard = tokio::time::interval(route_cadence);
        route_guard.tick().await;
        info!(
            events = route_watch.is_some(),
            tick_secs = route_cadence.as_secs(),
            "overlay: route guard armed"
        );
        // Phase D — LAZY `/derp`: open the WS (via the agent-provided factory)
        // ONLY for a relay-mode node that is itself UDP-blocked — i.e. its srflx
        // gather found nothing (`srflx_advertised.is_empty()`). A UDP-capable
        // node can never be in a both-UDP-blocked pair, so it doesn't hold an
        // idle `/derp` WS. The factory is `FnOnce`, so `take()` it; a reconnect
        // re-runs `run` and re-decides from the fresh gather.
        //
        // P7 — the factory is RETAINED (moved into a local, not consumed at
        // startup unless needed): a UDP-capable node can now be force-pinned
        // onto DERP mid-run by the server's `OverlayForceDerp` push, and the
        // handler invokes the factory at-most-once THEN — see the ForceDerp
        // arm.
        let mut derp_factory = self.derp_mux_factory.take();
        let derp_mux = if matches!(self.mode, CarrierMode::Relay) && srflx_advertised.is_empty() {
            derp_factory.take().map(|f| f())
        } else {
            None
        };
        let mut relay = match self.mode {
            // Pass our LAN endpoints so the relay-endpoint trickle re-includes
            // them (the server replaces, so they'd otherwise be clobbered —
            // rc.135). Empty when the direct path is off.
            CarrierMode::Relay => Some(RelayCoordinator::new(
                self.outbound.clone(),
                self.keypair.public.to_bytes(),
                // Phase D — we can be the raw-UDP single-relay DIALER only if our
                // own srflx gather succeeded (proof raw UDP to coturn works). A
                // UDP-blocked host gathered none ⇒ it can only be the ANCHOR
                // (TURNS/TCP allocation). The peer's equivalent is read off the
                // netmap's `srflx_endpoints`, so the role choice is symmetric.
                !srflx_advertised.is_empty(),
                direct_ctx
                    .as_ref()
                    .map(|c| c.endpoints.clone())
                    .unwrap_or_default(),
                derp_mux,
            )),
            CarrierMode::Direct(_) => None,
        };
        // rc.211 — off-loop relay carrier builds (see `RelayBuildQueue`).
        // Created before the FIRST install so the startup batch can spawn too.
        let (built_tx, mut built_rx) = mpsc::channel::<BuiltRelay>(16);
        let mut relay_bq = RelayBuildQueue {
            in_flight: HashMap::new(),
            epoch: 0,
            tx: built_tx,
        };
        // rc.218 — off-loop relay ALLOCATES (see `RelayAllocQueue`).
        let (alloc_tx, mut alloc_rx) = mpsc::channel::<AllocDone>(16);
        let mut alloc_q = RelayAllocQueue {
            in_flight: HashMap::new(),
            epoch: 0,
            tx: alloc_tx,
        };
        self.install_peers(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &first_peers,
            direct_ctx.as_ref(),
            &mut upgrade_probes,
            &mut relay_bq,
            "initial",
        )
        .await;
        // P5 exit-node — default-route capture state, reconciled after every
        // carrier change. Inert unless this node has `exit_node` configured.
        // P5 S4b — DNS-steering context for this run (immutable): whether MagicDNS
        // is on (→ steer "." at the LOCAL resolver `self_v4`, which forwards to the
        // network upstream via the exit; else steer "." at the public upstream
        // DIRECTLY), the catch-all target, and whether the local resolver bound.
        let dns_target = if dns_magic.is_some() {
            self_v4
        } else {
            match dns_upstream.ip() {
                IpAddr::V4(v4) => v4,
                IpAddr::V6(_) => Ipv4Addr::new(1, 1, 1, 1),
            }
        };
        let mut exit_state = ExitRoutingState {
            dns_magic_domain: dns_magic,
            dns_target: Some(dns_target),
            dns_bound,
            ..Default::default()
        };
        self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state)
            .await;

        // Unification P1 — first LocalAPI view, so `roomler status` right after
        // join isn't empty until the first sweep (carries the exit-node status).
        self.publish_view(
            &self_ip,
            &by_node,
            &current_peers,
            &upgrade_probes,
            exit_node_status(self.exit_node.as_deref(), &exit_state),
        );

        // Phase C (D8) — re-upgrade tick counter (see `REUPGRADE_EVERY_N_TICKS`).
        let mut reupgrade_ticks: u32 = 0;

        // rc.206 — serializes the DETACHED route-guard re-assert (see the
        // `route_guard.tick()` arm): an owned `try_lock` drops any tick whose
        // predecessor batch is still running, so a slow Windows `netsh` sweep
        // never stacks concurrent delete-then-add mutations on the same prefix.
        let route_reassert_lock = Arc::new(tokio::sync::Mutex::new(()));

        // P8 — resume-from-suspend detector state (see `resumed_from_suspend`):
        // one (monotonic, wall) sample per sweep tick.
        let mut resume_probe = (Instant::now(), SystemTime::now());

        // rc.213 — dedicated outbound TUN reader (the Windows 1–2 s batching
        // fix). `tokio::select!` DROPS every losing arm's future each
        // iteration; a dropped `tun.read_packet()` future on Windows leaves its
        // blocking-pool thread parked in `WaitForMultipleObjects(INFINITE)` on
        // wintun's read event as a ZOMBIE waiter. The event releases ONE waiter
        // per edge, so an accumulated zombie usually swallows it and the live
        // read future starves — outbound packets then only left the ring when a
        // periodic arm (route guard 2 s / fallback 5 s) woke the loop and a
        // FRESH read future's `try_receive` drained the backlog. Field-proven
        // on neo16: mars-side tcpdump showed every overlay packet arriving in
        // bursts on exactly the union of those two timer grids (sub-ms aligned;
        // RTT sequence {2,2,1,1,2,2} s ⇒ the measured ~1.65 s averages) while
        // the raw wire RTT was 43 ms — and the rc.211 handler watchdog stayed
        // silent throughout, because the delay was future-PENDING time, which
        // no handler timer sees. A PERSISTENT reader task is never cancelled,
        // so exactly one event waiter exists and every edge lands on it; the
        // loop consumes via an mpsc arm, whose `recv()` is cancel-safe by
        // contract. Linux never suffered (level-triggered epoll on the tun fd,
        // no blocking-pool waiters) and shares the structure harmlessly.
        let (tun_pkt_tx, tun_pkt_rx) = mpsc::channel::<Vec<u8>>(OUTBOUND_QUEUE_PKTS);
        let reader_tun = tun.clone();
        let tun_reader = tokio::spawn(async move {
            loop {
                match reader_tun.read_packet().await {
                    Ok(pkt) => {
                        // Reader-side twin of `warn_if_slow`: a slow send here
                        // means the channel is FULL — since P1 the consumer is
                        // the dedicated outbound PUMP, so a full queue indicts
                        // the send path itself (a wedged carrier send), not
                        // the control loop. Complements the pump's per-packet
                        // timer, which can't see future-PENDING time.
                        let t0 = Instant::now();
                        if tun_pkt_tx.send(pkt).await.is_err() {
                            break; // outbound pump gone
                        }
                        let ms = t0.elapsed().as_millis();
                        if ms > LOOP_STALL_WARN_MS {
                            warn!(
                                ms,
                                "overlay: outbound pump backpressured the TUN reader (send queue full — carrier send path wedged?)"
                            );
                        }
                    }
                    Err(e) => {
                        debug!(%e, "overlay: TUN read ended; reader exiting");
                        break;
                    }
                }
            }
        });

        // P1 (S6) — the dedicated OUTBOUND PUMP: TUN packets → encapsulate →
        // carrier, in its own task via the shared [`WgSender`], so NO control
        // work on the select! loop can ever delay an outbound packet again
        // (the structural close of the rc.206→rc.218 inline-await arc; the
        // loop below is pure control plane). Pump death is FATAL, never
        // respawned: it only exits when the TUN reader died (`recv()` →
        // `None` — the same condition the pre-P1 loop arm broke on) or on a
        // panic in the send path (respawning would just re-panic on the next
        // packet); either way the loop breaks and the agent's WS lifecycle
        // rebuilds the whole runtime.
        let sender = wg.sender();
        let mut outbound_pump = tokio::spawn(async move {
            let mut rx = tun_pkt_rx;
            while let Some(pkt) = rx.recv().await {
                let t0 = Instant::now();
                let _ = sender.send_ip_packet(&pkt).await;
                // >250 ms here is now PURE send-path latency (a wedged
                // carrier send), un-polluted by loop scheduling.
                warn_if_slow("pump:send_ip_packet", t0);
            }
            debug!("overlay: TUN reader ended; outbound pump exiting");
        });

        // Phase 2 — steady state. Control plane ONLY (P1): the data plane
        // runs reader → pump → carriers / demux → inbound writer, entirely
        // off-loop.
        loop {
            tokio::select! {
                // P1 (S6) — the pump ending is the session-fatal signal the
                // TUN-reader `None` used to be.
                _ = &mut outbound_pump => {
                    debug!("overlay: outbound pump ended; runtime exiting");
                    break;
                },
                // rc.211 — commit a finished OFF-LOOP relay carrier build (the
                // spawned QUIC-over-TURN rendezvous — see `RelayBuildQueue`).
                // The install half is µs. A STALE completion (peer removed /
                // went direct / superseded mid-build) is dropped; the next
                // netmap/sweep tick re-coordinates cleanly.
                built = built_rx.recv() => {
                    if let Some(built) = built {
                        let t_arm = Instant::now();
                        if relay_bq.take_if_current(&built.link.node_id, built.epoch) && current_peers.contains_key(&built.link.node_id) {
                            let BuiltRelay { link, quic, .. } = built;
                            self.install_built(&mut wg, &mut by_node, &tun, link, quic).await;
                            // Same tail as a synchronous relay install: a new
                            // coturn worker may need an exit exemption, and the
                            // LocalAPI view must reflect the new carrier.
                            self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                            self.publish_view(&self_ip, &by_node, &current_peers, &upgrade_probes, exit_node_status(self.exit_node.as_deref(), &exit_state));
                        } else {
                            debug!(peer = %built.link.node_id, "overlay: dropping stale off-loop carrier build (peer removed/superseded mid-build)");
                        }
                        warn_if_slow("arm:relay_build_commit", t_arm);
                    }
                },
                // rc.218 — commit a finished OFF-LOOP relay ALLOCATE (the
                // spawned DNS + TURN allocate — see `RelayAllocQueue`). Success
                // advertises + tries to build (µs, on-loop by design: the
                // `OverlayEndpoints` trickle reads coordinator state); failure
                // FORGETS the peer so the next netmap/sweep tick re-requests —
                // the old inline path parked a failed peer in `pending`
                // forever, with nothing to ever retry it.
                done = alloc_rx.recv() => {
                    if let Some(done) = done {
                        let t_arm = Instant::now();
                        if alloc_q.take_if_current(&done.node_id, done.epoch) {
                            if let Some(r) = relay.as_mut() {
                                match done.conn {
                                    Some(conn) => {
                                        let link = r.commit_alloc(done.node_id, conn).await;
                                        if let Some(link) = link {
                                            let t0 = Instant::now();
                                            self.install_ready(&mut wg, &mut by_node, &tun, link, &mut relay_bq).await;
                                            warn_if_slow("install_ready(spawn-or-sync)", t0);
                                            // Same tail as the old inline grant path: a
                                            // newly-installed relay carrier adds a coturn
                                            // worker to exempt, and the LocalAPI view must
                                            // reflect the new carrier.
                                            self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                                            self.publish_view(&self_ip, &by_node, &current_peers, &upgrade_probes, exit_node_status(self.exit_node.as_deref(), &exit_state));
                                        }
                                    }
                                    None => {
                                        warn!(peer = %done.node_id, "overlay relay: off-loop allocate failed — dropping coordination (re-requested next tick)");
                                        r.forget(&done.node_id);
                                    }
                                }
                            }
                        } else {
                            debug!(peer = %done.node_id, "overlay: dropping stale off-loop allocate (peer superseded mid-allocate)");
                        }
                        warn_if_slow("arm:relay_alloc_commit", t_arm);
                    }
                },
                // rc.136 — direct→relay fallback sweep. A DIRECT carrier whose
                // handshake never completes (or dies mid-session) means the LAN
                // path only LOOKED viable (same subnet) but isn't actually
                // reachable — a corp full-tunnel VPN that hijacks routing, Wi-Fi
                // AP/client isolation, an asymmetric firewall. Tear it down and
                // switch the peer to relay (with a cooldown so the next netmap
                // doesn't immediately re-upgrade it to direct).
                _ = fallback.tick() => {
                    let t_arm = Instant::now();
                    // P8 — resume-from-suspend: wall-vs-monotonic skew across
                    // the tick means the host slept. Every installed carrier is
                    // dead (NAT/pinhole state expired; peers tore their ends
                    // down while we slept) and every cooldown is stale
                    // evidence — drop both and re-coordinate NOW instead of
                    // letting the sweeps discover the corpses one deadline at
                    // a time (field 2026-07-25: a hibernate wake-up left the
                    // mesh relay-wedged for hours).
                    let (prev_mono, prev_wall) = resume_probe;
                    let wall_elapsed = SystemTime::now()
                        .duration_since(prev_wall)
                        .unwrap_or_default();
                    let mono_elapsed = prev_mono.elapsed();
                    resume_probe = (Instant::now(), SystemTime::now());
                    if resumed_from_suspend(mono_elapsed, wall_elapsed) {
                        warn!(
                            slept_s = wall_elapsed.as_secs(),
                            "overlay: resume from suspend — dropping all carriers + path penalties for fresh re-coordination"
                        );
                        let nids: Vec<ObjectId> = by_node.keys().copied().collect();
                        for nid in nids {
                            // Through the shared teardown: this used to leave
                            // `upgrade_probes` populated, so a later
                            // `promote_direct_probe` could re-upsert a STALE
                            // probe.overlay_ip and hijack a recycled address.
                            self.remove_peer_state(
                                nid, &mut wg, &mut by_node, &tun, &mut relay,
                                &mut relay_bq, Some(&mut alloc_q), &mut upgrade_probes,
                                PeerRoute::Drop,
                            ).await;
                        }
                        relay_refresh_cooldown.clear();
                        // P3 PR-A — same clean slate in the monitor (probe
                        // bookkeeping survives, like `upgrade_probes` does).
                        self.shadow(|s| s.mon.on_resume());
                        let peers_now: Vec<NetmapPeer> = current_peers.values().cloned().collect();
                        self.install_peers(
                            &mut wg, &mut by_node, &mut relay, &tun, &peers_now,
                            direct_ctx.as_ref(), &mut upgrade_probes, &mut relay_bq,
                            "resume",
                        ).await;
                        self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                        self.publish_view(&self_ip, &by_node, &current_peers, &upgrade_probes, exit_node_status(self.exit_node.as_deref(), &exit_state));
                    }
                    let t0 = Instant::now();
                    self.sweep_carrier_health(
                        &mut wg, &mut by_node, &mut relay, &tun,
                        &mut relay_refresh_cooldown, &current_peers,
                    ).await;
                    warn_if_slow("sweep_carrier_health", t0);
                    // rc.208 — make-before-break: promote any upgrade probe whose
                    // handshake latched (cut over to direct, drop the relay) and
                    // expire any that missed its deadline (keep the relay). Inert
                    // when the feature is off / no probes are in flight.
                    let t0 = Instant::now();
                    self.sweep_upgrade_probes(
                        &mut wg, &mut by_node, &mut relay, &tun,
                        &mut upgrade_probes, &mut relay_bq,
                    ).await;
                    warn_if_slow("sweep_upgrade_probes", t0);
                    // D8 — periodic direct re-upgrade (~every 6th tick ≈ 30 s).
                    // A lapsed cooldown only takes effect on the next netmap
                    // otherwise; a quiet mesh would never re-attempt direct after
                    // a fallback. Re-run the tier evaluation over the current
                    // netmap — install_peers no-ops on already-direct peers and
                    // won't re-request a relay it's already tracking, so this only
                    // (a) retries a direct tier whose cooldown lapsed and (b)
                    // drives Phase C punch convergence at large install skew.
                    reupgrade_ticks = reupgrade_ticks.wrapping_add(1);
                    if reupgrade_ticks.is_multiple_of(REUPGRADE_EVERY_N_TICKS) {
                        let peers: Vec<NetmapPeer> = current_peers.values().cloned().collect();
                        let t0 = Instant::now();
                        self.install_peers(
                            &mut wg, &mut by_node, &mut relay, &tun,
                            &peers, direct_ctx.as_ref(), &mut upgrade_probes,
                            &mut relay_bq, "reupgrade",
                        ).await;
                        warn_if_slow("install_peers(reupgrade)", t0);
                    }
                    // A carrier flip may have changed the coturn worker set or
                    // the exit peer's reachability — re-reconcile exit routing
                    // FIRST, so the refreshed view carries the new exit status.
                    let t0 = Instant::now();
                    self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state)
                        .await;
                    warn_if_slow("reconcile_exit_routing(sweep)", t0);
                    // A direct→relay fallback (or relay refresh) changed how we
                    // reach a peer (and maybe the exit status) — refresh the view.
                    self.publish_view(
                        &self_ip,
                        &by_node,
                        &current_peers,
                        &upgrade_probes,
                        exit_node_status(self.exit_node.as_deref(), &exit_state),
                    );
                    warn_if_slow("arm:fallback_sweep", t_arm);
                },
                // rc.146 — re-assert every installed peer's /32 on the overlay
                // NIC (evict any competing route a full-tunnel VPN re-added, then
                // re-add ours at low metric). Unconditional: a captured route
                // keeps our packets off the WG device, so the carrier's traffic
                // counters can't detect it — only a periodic re-assert can.
                _ = route_guard.tick() => {
                    // A tick wave counts as a wave for the event arm's rate
                    // limiter / trailing edge (P4 demotion — both arms share
                    // one wave clock).
                    last_route_wave = Some(Instant::now());
                    // rc.206 — DETACH the per-peer /32 re-assert (the head-of-line
                    // bulk on Windows: N peers × `route`/`netsh` delete-then-add,
                    // ~0.3–2 s each) off the select! loop. Awaiting it INLINE
                    // stalled the outbound TUN-packet arm above (select! doesn't
                    // re-poll a sibling arm while the chosen handler awaits), so
                    // outbound packets piled unread in the wintun ring → ~1.8 s
                    // Windows RTT (lossless, just delayed) vs Linux's ~40 ms (one
                    // fast `ip route replace`). The owned `try_lock` drops a tick
                    // whose predecessor is still running (a slow batch must never
                    // stack concurrent delete-then-add on the same prefix) and
                    // releases on task end/panic. Worst case a since-removed peer
                    // leaves a harmless dangling /32 to a dead overlay IP (traffic
                    // there drops anyway; `store=active` clears on reboot).
                    if let Ok(guard) = route_reassert_lock.clone().try_lock_owned() {
                        let tun2 = tun.clone();
                        // rc.285 — the wave runs the DECLARATIVE defended set
                        // (every peer /32, then our own /32 + v6 twins —
                        // composed in one place, `route_guard::defended_routes`).
                        let set = defended_routes(&by_node, self_v4);
                        tokio::spawn(async move {
                            let _guard = guard;
                            run_defense_wave(tun2, set).await;
                        });
                    }
                    // P5 — the exit split-default /1 re-assert stays INLINE (NOT
                    // detached): a background task with a stale `split` snapshot
                    // could re-install a /1 that `teardown_exit_routing` (running
                    // on THIS loop) had just purged, black-holing the host's whole
                    // egress with no exit carrier to forward it — and the
                    // edge-triggered teardown would never heal it (self-wedge).
                    // Inline keeps it mutually exclusive with teardown. It's ≤4
                    // route calls and fires only on exit-node clients
                    // (`split_default_installed` is false everywhere else →
                    // skipped), so it isn't the latency bulk. Mirrors the per-peer
                    // /32 war (A7): a competing full-tunnel VPN default can't
                    // reclaim egress.
                    if exit_state.split_default_installed {
                        let t0 = Instant::now();
                        for cidr in SPLIT_DEFAULT_V4.iter().chain(SPLIT_DEFAULT_V6.iter()) {
                            tun.add_cidr_route(cidr).await.ok();
                        }
                        warn_if_slow("arm:route_guard(exit /1)", t0);
                    }
                },
                // P4 — OS route-table change → re-assert NOW (same body as the
                // tick above: detached /32 wave + inline exit /1). Rate-
                // limited to one wave per ROUTE_WAVE_MIN_INTERVAL because our
                // own re-asserts feed the subscription back; an event inside
                // the quiet window pulls the next tick to the due boundary
                // (trailing edge), so a quiet-window erase is repaired within
                // ROUTE_WAVE_MIN_INTERVAL — the demoted 30 s heartbeat never
                // carries it. Inert (`pending`) when the subscription is
                // off/unavailable.
                maybe_route_evt = async {
                    match route_watch.as_mut() {
                        Some(w) => w.recv().await,
                        None => std::future::pending::<Option<String>>().await,
                    }
                } => {
                    match maybe_route_evt {
                        Some(first) => {
                            let burst = route_watch.as_mut().map(|w| w.drain()).unwrap_or(0);
                            let due = last_route_wave
                                .is_none_or(|t| t.elapsed() >= super::route_events::ROUTE_WAVE_MIN_INTERVAL);
                            if due {
                                last_route_wave = Some(Instant::now());
                                let first_short: String = first.chars().take(120).collect();
                                info!(
                                    events = burst + 1, first = %first_short,
                                    "overlay: route-table change — re-asserting peer routes now (P4 event-driven; heartbeat tick is the backstop)"
                                );
                                if let Ok(guard) = route_reassert_lock.clone().try_lock_owned() {
                                    let tun2 = tun.clone();
                                    // rc.285 — the same declarative set as the
                                    // tick arm. A VPN connect is exactly the
                                    // route storm that re-adds the hijacking
                                    // /32 for our own address; evict it NOW
                                    // rather than waiting for the 2 s tick.
                                    let set = defended_routes(&by_node, self_v4);
                                    tokio::spawn(async move {
                                        let _guard = guard;
                                        run_defense_wave(tun2, set).await;
                                    });
                                }
                                // P5 exit /1 — INLINE for the same teardown-
                                // mutual-exclusion reason as the tick arm.
                                if exit_state.split_default_installed {
                                    let t0 = Instant::now();
                                    for cidr in SPLIT_DEFAULT_V4.iter().chain(SPLIT_DEFAULT_V6.iter()) {
                                        tun.add_cidr_route(cidr).await.ok();
                                    }
                                    warn_if_slow("arm:route_events(exit /1)", t0);
                                }
                                // The heartbeat counts from the last wave.
                                route_guard.reset();
                            } else {
                                // Trailing edge: pull the next tick to the due
                                // boundary so an erase inside the quiet window
                                // waits ≤ ROUTE_WAVE_MIN_INTERVAL, not a full
                                // heartbeat. Idempotent re-adds don't notify
                                // (field: waves run ~6-40/h, not at echo
                                // cadence), so a no-op wave settles silent.
                                let elapsed = last_route_wave
                                    .map(|t| t.elapsed())
                                    .unwrap_or_default();
                                route_guard.reset_after(
                                    super::route_events::ROUTE_WAVE_MIN_INTERVAL
                                        .saturating_sub(elapsed),
                                );
                            }
                        }
                        None => {
                            // Post-demotion this is operational, not cosmetic:
                            // the 30 s heartbeat must not keep running as the
                            // only guard. Restore the 2 s war cadence; the
                            // fresh interval fires immediately → one prompt
                            // catch-up wave.
                            warn!(
                                "overlay: route-event subscription ended — restoring the 2 s route-guard tick"
                            );
                            route_watch = None;
                            route_guard = tokio::time::interval(ROUTE_GUARD_TICK);
                        }
                    }
                },
                // Phase A — an authenticated inbound direct handshake initiation
                // forwarded by a demux loop (a NAT'd client dialing our public
                // endpoint, or a peer roaming to a new port). `pending()` when
                // the public-direct tier is off, so this branch is inert on the
                // fleet default.
                maybe_init = async {
                    match direct_events.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<crate::overlay::wg::DirectInbound>>().await,
                    }
                } => {
                    if let Some(inb) = maybe_init {
                        let t_arm = Instant::now();
                        self.handle_direct_inbound(
                            &mut wg, &mut by_node, &mut relay, &tun,
                            &current_peers, &mut upgrade_probes,
                            &mut relay_bq, inb,
                        ).await;
                        self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                        self.publish_view(&self_ip, &by_node, &current_peers, &upgrade_probes, exit_node_status(self.exit_node.as_deref(), &exit_state));
                        warn_if_slow("arm:direct_inbound", t_arm);
                    }
                },
                evt = events.recv() => match evt {
                    // Re-sync: install any newly-listed peers (deltas drive
                    // removals; a full diff/prune is a later refinement).
                    Some(OverlayEvent::Netmap { peers, .. }) => {
                        let t_arm = Instant::now();
                        #[cfg(test)]
                        {
                            let stall = TEST_NETMAP_STALL_MS.load(std::sync::atomic::Ordering::Relaxed);
                            if stall > 0 {
                                tokio::time::sleep(Duration::from_millis(stall)).await;
                            }
                        }
                        let old_peers = std::mem::take(&mut current_peers);
                        current_peers = peers.iter().map(|p| (p.node_id, p.clone())).collect();
                        // rc.225 — a peer whose direct endpoints changed gets a
                        // clean slate: its old strike counts were measured
                        // against dial conditions that no longer exist.
                        for p in &peers {
                            if let Some(old) = old_peers.get(&p.node_id)
                                && direct_endpoints_changed(old, p)
                            {
                                // P3 PR-A — same clean slate in the monitor
                                // (penalties, strikes AND quality — F1).
                                self.shadow(|s| s.mon.on_endpoint_change(&p.node_id));
                                debug!(peer = %p.node_id, "overlay: peer's direct endpoints changed — cleared its path-monitor evidence");
                            }
                        }
                        // The full netmap is AUTHORITATIVE: anything we still
                        // hold that it does not list is gone — released, or
                        // ACL'd out.
                        //
                        // DEFENSIVE ONLY — do not read this as fixing a live
                        // leak. `run()` sends ONE `OverlayJoin` and consumes the
                        // first full netmap before this loop starts, and the
                        // server only emits a full `OverlayNetmap` in reply to a
                        // join, so today a second full netmap never reaches a
                        // running loop. The runtime is scoped to ONE WS session
                        // (`overlay::maybe_start` runs per connection and the
                        // runtime ends when `overlay_evt_tx` drops), so a
                        // disconnect tears down `by_node`, the WgDevice and the
                        // TUN wholesale and the reconnect rebuilds from empty —
                        // which already covers "a peer vanished while we were
                        // away", more thoroughly than a diff could. Field-checked
                        // 2026-08-02: cutting an agent's WS produced a second
                        // `node runtime started` / `rc:overlay.join sent` /
                        // `TUN up` and a full reinstall of every peer.
                        //
                        // It stays because it is cheap and it is the correct
                        // behaviour the day anything does re-push a full netmap
                        // into a live loop (a server-side re-fan, or a re-join
                        // that keeps the connection). The removal paths that
                        // actually run today are the delta `removes` arm and
                        // `sweep_carrier_health` — those are what make
                        // `Router::remove_by_pubkey` and
                        // `del_peer_route_if_unowned` load-bearing under
                        // address recycling, NOT this arm.
                        //
                        // MEMBERSHIP, not presence: P9 ghost rows are still
                        // LISTED (with `reachable = false`), so they are not
                        // pruned — an offline peer keeps its carrier and comes
                        // back without a rebuild. Only a peer the server dropped
                        // from the netmap entirely — released, or ACL'd out — is
                        // torn down here.
                        //
                        // Collect first: the diff holds borrows of `by_node`,
                        // `upgrade_probes` and `current_peers` that must end
                        // before the loop takes them mutably.
                        let mut vanished: Vec<ObjectId> = by_node
                            .keys()
                            .chain(upgrade_probes.keys())
                            .filter(|id| !current_peers.contains_key(id))
                            .copied()
                            .collect();
                        vanished.sort_unstable();
                        vanished.dedup();
                        if !vanished.is_empty() {
                            info!(pruned = vanished.len(), listed = current_peers.len(),
                                "overlay: full netmap prune");
                        }
                        // Prune BEFORE install_peers so a stale peer and the new
                        // owner of its address are never both installed.
                        for nid in vanished {
                            self.evict_peer(
                                nid, &mut wg, &mut by_node, &tun, &mut relay, &mut relay_bq,
                                &mut alloc_q, &mut upgrade_probes, &mut current_peers,
                                &mut relay_refresh_cooldown,
                            ).await;
                        }
                        if let Some(names) = &dns_names { sync_name_map(names, &current_peers).await; }
                        let t0 = Instant::now();
                        self.install_peers(&mut wg, &mut by_node, &mut relay, &tun, &peers, direct_ctx.as_ref(), &mut upgrade_probes, &mut relay_bq, "netmap").await;
                        warn_if_slow("install_peers(netmap)", t0);
                        self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                        self.publish_view(&self_ip, &by_node, &current_peers, &upgrade_probes, exit_node_status(self.exit_node.as_deref(), &exit_state));
                        warn_if_slow("arm:netmap", t_arm);
                    }
                    Some(OverlayEvent::NetmapDelta { upserts, removes }) => {
                        let t_arm = Instant::now();
                        for p in &upserts {
                            // rc.225 — endpoint change ⇒ clean cooldown slate
                            // (see the Netmap arm / `direct_endpoints_changed`).
                            if let Some(old) = current_peers.insert(p.node_id, p.clone())
                                && direct_endpoints_changed(&old, p)
                            {
                                // P3 PR-A — same clean slate in the monitor
                                // (penalties, strikes AND quality — F1).
                                self.shadow(|s| s.mon.on_endpoint_change(&p.node_id));
                                debug!(peer = %p.node_id, "overlay: peer's direct endpoints changed — cleared its path-monitor evidence");
                            }
                        }
                        let t0 = Instant::now();
                        self.install_peers(&mut wg, &mut by_node, &mut relay, &tun, &upserts, direct_ctx.as_ref(), &mut upgrade_probes, &mut relay_bq, "delta").await;
                        warn_if_slow("install_peers(delta)", t0);
                        for node_id in removes {
                            self.evict_peer(
                                node_id, &mut wg, &mut by_node, &tun, &mut relay, &mut relay_bq,
                                &mut alloc_q, &mut upgrade_probes, &mut current_peers,
                                &mut relay_refresh_cooldown,
                            ).await;
                        }
                        if let Some(names) = &dns_names { sync_name_map(names, &current_peers).await; }
                        self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                        self.publish_view(&self_ip, &by_node, &current_peers, &upgrade_probes, exit_node_status(self.exit_node.as_deref(), &exit_state));
                        warn_if_slow("arm:netmap_delta", t_arm);
                    }
                    Some(OverlayEvent::RelayGrant { peer_node_id, ice_servers, pair_key }) => {
                        let t_arm = Instant::now();
                        if let Some(r) = relay.as_mut() {
                            // rc.218 — the DNS + TURN allocate no longer runs
                            // inline (it stalled the data plane for seconds on a
                            // hostile corp path — see `RelayAllocQueue`). Stash
                            // the creds sync, spawn the allocate, and let the
                            // `alloc_rx` arm commit the result. LAST GRANT WINS:
                            // re-stamping supersedes any in-flight allocate for
                            // this peer (its completion drops on epoch mismatch),
                            // and `grant_accept` requiring a `pending` slot drops
                            // grants for peers already torn down.
                            if let Some((ice, pair_key)) = r.grant_accept(peer_node_id, ice_servers, pair_key) {
                                let epoch = alloc_q.stamp(peer_node_id);
                                let tx = alloc_q.tx.clone();
                                tokio::spawn(async move {
                                    let conn = RelayCoordinator::allocate_for_pair(&ice, &pair_key).await;
                                    let _ = tx.send(AllocDone { epoch, node_id: peer_node_id, conn }).await;
                                });
                            }
                        }
                        warn_if_slow("arm:relay_grant", t_arm);
                    }
                    Some(OverlayEvent::ForceDerp { peer_node_id, ttl_ms }) => {
                        let t_arm = Instant::now();
                        // P3 PR-A — annotate the pinned window in the monitor
                        // (relay-Q resets; direct decisions unaffected — the
                        // pin stays a server-side OVERRIDE, never scored).
                        self.shadow(|s| s.mon.on_forced_derp(&peer_node_id, Duration::from_millis(ttl_ms), Instant::now()));
                        if let Some(r) = relay.as_mut() {
                            // P7 — a UDP-capable node skipped the startup
                            // `/derp` open; lazily invoke the retained factory
                            // (at-most-once) before pinning.
                            if !r.has_derp_mux()
                                && let Some(f) = derp_factory.take()
                            {
                                r.set_derp_mux(f());
                            }
                            // Supersede any in-flight off-loop ALLOCATE — a
                            // stale AllocDone must not resurrect a TURN link
                            // inside the forced window.
                            alloc_q.invalidate(&peer_node_id);
                            let mut link = r.force_derp(peer_node_id, Duration::from_millis(ttl_ms));
                            // rc.222 — stamp-only + an INSTALLED TURN relay for
                            // the pair: tear it down NOW and rebuild under the
                            // pin. The server just certified the pair's TURN
                            // broken, and a half-dead (one-way) relay keeps rx
                            // alive, so the liveness sweep would never cycle it
                            // (the field-observed ZOMBIE wedge). Direct
                            // carriers are left alone — the escalation is about
                            // the TURN tier.
                            if link.is_none()
                                && by_node.get(&peer_node_id).is_some_and(|e| !e.is_direct)
                                && let Some(e) = by_node.remove(&peer_node_id)
                            {
                                wg.remove_peer(&e.pubkey).await;
                                del_peer_route_if_unowned(&tun, &by_node, e.overlay_ip).await;
                                r.forget(&peer_node_id);
                                if let Some(cfg) = current_peers
                                    .get(&peer_node_id)
                                    .and_then(peer_config_from_netmap)
                                {
                                    r.request(peer_node_id, cfg.clone()).await;
                                    link = r.maybe_complete(peer_node_id, &cfg);
                                }
                            }
                            if let Some(link) = link {
                                let t0 = Instant::now();
                                self.install_ready(&mut wg, &mut by_node, &tun, link, &mut relay_bq).await;
                                warn_if_slow("install_ready(spawn-or-sync)", t0);
                                self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                                self.publish_view(&self_ip, &by_node, &current_peers, &upgrade_probes, exit_node_status(self.exit_node.as_deref(), &exit_state));
                            }
                        }
                        warn_if_slow("arm:force_derp", t_arm);
                    }
                    None => break,
                },
            }
        }

        // P5 — revert exit-node default routing on a clean exit (WS disconnect /
        // shutdown). An UNCLEAN exit self-heals too (the OS default was never
        // deleted); S3.5 adds the process::exit + boot-reconciler paths (A2).
        self.teardown_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state)
            .await;

        inbound.abort();
        // rc.213 — stop the dedicated outbound TUN reader; aborting drops its
        // in-flight `read_packet()` future, and the TUN `Arc` it holds drops
        // with the task, so session teardown isn't kept alive by the reader.
        tun_reader.abort();
        // P1 (S6) — and the outbound pump (it would end on its own once the
        // reader's channel closes, but an abort makes teardown prompt).
        outbound_pump.abort();
        // Phase C — stop the srflx keepalive task (if any) on runtime exit.
        if let Some(h) = srflx_keepalive {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    //! Phase 3b proof: two `OverlayRuntime`s, driven only by injected
    //! `rc:overlay.netmap` events + a loopback `LinkFactory`, bring up
    //! their WG peers and round-trip an IP packet between their mock
    //! TUNs — exercising join → netmap → add_peer → bridge end to end
    //! with no real device and no server.

    use super::*;
    use std::io;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::net::UdpSocket;
    use tokio::sync::Mutex;

    /// rc.211 — a fresh off-loop build queue for tests. The receiver is
    /// dropped: these tests exercise direct/LAN paths that never spawn a
    /// QUIC build, and a send into a closed channel is simply ignored.
    fn test_relay_bq() -> RelayBuildQueue {
        let (tx, _rx) = mpsc::channel(4);
        RelayBuildQueue {
            in_flight: HashMap::new(),
            epoch: 0,
            tx,
        }
    }

    /// rc.211 (P2: generic `EpochQueue`) — the off-loop queue's staleness
    /// guards. (a) A completion commits only while its epoch is current;
    /// (b) `invalidate` (peer removed / went direct) drops the in-flight work
    /// on arrival; (c) re-`stamp` for the same peer supersedes — the ABA case
    /// a plain "is in flight" set would get wrong (old completion must NOT
    /// commit, new one must). Exercised through the `RelayBuildQueue` alias;
    /// `RelayAllocQueue` is the same generic (its extra semantics — last
    /// grant wins, no per-site invalidation — are call-site policy, not
    /// queue mechanics).
    #[tokio::test]
    async fn relay_build_queue_epoch_guards() {
        let mut bq = test_relay_bq();
        let n = ObjectId::from_bytes([9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        // (a) current epoch commits, and the slot is consumed.
        let e1 = bq.stamp(n);
        assert!(bq.in_flight.contains_key(&n));
        assert!(bq.take_if_current(&n, e1));
        assert!(!bq.in_flight.contains_key(&n), "commit consumes the slot");
        assert!(
            !bq.take_if_current(&n, e1),
            "a second arrival of the same build is stale"
        );

        // (b) invalidate → the completion is dropped on arrival.
        let e2 = bq.stamp(n);
        bq.invalidate(&n);
        assert!(!bq.take_if_current(&n, e2));

        // (c) ABA: re-stamp supersedes — the OLD build must not commit, the
        // NEW one must.
        let e3 = bq.stamp(n);
        let e4 = bq.stamp(n);
        assert!(!bq.take_if_current(&n, e3), "superseded build is stale");
        assert!(bq.take_if_current(&n, e4), "current build commits");
    }

    /// rc.218 — the ALLOCATE queue rides the same generic: last grant wins
    /// (re-stamp supersedes), commits consume the slot, stale completions
    /// drop.
    #[tokio::test]
    async fn relay_alloc_queue_epoch_guards() {
        let (tx, _rx) = mpsc::channel::<AllocDone>(4);
        let mut q = RelayAllocQueue {
            in_flight: HashMap::new(),
            epoch: 0,
            tx,
        };
        let n = ObjectId::from_bytes([7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        // (a) current epoch commits, and the slot is consumed.
        let e1 = q.stamp(n);
        assert!(q.take_if_current(&n, e1));
        assert!(!q.in_flight.contains_key(&n), "commit consumes the slot");
        assert!(
            !q.take_if_current(&n, e1),
            "a second arrival of the same allocate is stale"
        );

        // (b) last grant wins: the superseded allocate must not commit, the
        // newest must.
        let e2 = q.stamp(n);
        let e3 = q.stamp(n);
        assert!(!q.take_if_current(&n, e2), "superseded allocate stale");
        assert!(q.take_if_current(&n, e3), "current allocate commits");
    }

    /// P3 PR-E (was PR-C's ON-leg; the legacy control leg is gone with the
    /// legacy) — a FRESH peer with a probe already in flight rides relay
    /// instead of racing a destructive direct install over the probe (the
    /// soak-#1 Class-A fix, now the only behaviour).
    #[tokio::test(flavor = "multi_thread")]
    async fn fresh_peer_with_probe_in_flight_rides_relay_not_installing() {
        let kp = WgKeypair::generate();
        let peer_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let (tun_mock, _inj, _del) = MockTun::new();
        let tf: TunFactory = {
            let m = tun_mock.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);
        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let tun: Arc<dyn TunIo> = tun_mock;
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let my_ip: Ipv4Addr = "10.1.2.9".parse().unwrap();
        let ctx = DirectCtx {
            socks: vec![(my_ip, sock)],
            my_ips: vec![my_ip],
            endpoints: vec![],
            public_sock: None,
            punch: None,
            my_nat: None,
        };
        let mut relay: Option<RelayCoordinator> = None;
        let mut by_node = HashMap::new();
        let mut upgrade_probes = HashMap::new();
        let mut lan_peer = peer(&peer_kp, "100.64.0.7");
        lan_peer.lan_endpoints = vec!["10.1.2.3:1000".into()];

        // The Class-A shape: a probe survives its peer's carrier death.
        let probe_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        wg.start_direct_probe(
            probe_sock,
            peer_kp.public.to_bytes(),
            "100.64.0.7".parse().unwrap(),
            "127.0.0.1:9".parse().unwrap(),
            false,
        )
        .await;
        rt.path_shadow.lock().unwrap().mon.on_probe_started(
            &lan_peer.node_id,
            DirectTier::Srflx,
            Instant::now(),
        );

        rt.install_peers(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            std::slice::from_ref(&lan_peer),
            Some(&ctx),
            &mut upgrade_probes,
            &mut test_relay_bq(),
            "test",
        )
        .await;
        assert!(
            !by_node.contains_key(&lan_peer.node_id),
            "no fresh install while a probe is in flight (rides relay)"
        );
    }

    /// P3 PR-E — a monitor-penalized tier is not probed by the upgrade arm
    /// (the eligibility plane is the ONE suppression system now).
    #[tokio::test(flavor = "multi_thread")]
    async fn upgrade_arm_respects_monitor_penalty() {
        let kp = WgKeypair::generate();
        let peer_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let (tun_mock, _inj, _del) = MockTun::new();
        let tf: TunFactory = {
            let m = tun_mock.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);
        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let tun: Arc<dyn TunIo> = tun_mock;
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let my_ip: Ipv4Addr = "10.1.2.9".parse().unwrap();
        let ctx = DirectCtx {
            socks: vec![(my_ip, sock)],
            my_ips: vec![my_ip],
            endpoints: vec![],
            public_sock: None,
            punch: None,
            my_nat: None,
        };
        let mut relay: Option<RelayCoordinator> = None;
        let mut by_node = HashMap::new();
        let mut upgrade_probes = HashMap::new();
        let mut lan_peer = peer(&peer_kp, "100.64.0.7");
        lan_peer.lan_endpoints = vec!["10.1.2.3:1000".into()];
        by_node.insert(
            lan_peer.node_id,
            Installed {
                is_direct: false,
                ..Installed::base(
                    peer_kp.public.to_bytes(),
                    "100.64.0.7".parse().unwrap(),
                    DirectTier::Relay,
                    Instant::now(),
                )
            },
        );
        rt.path_shadow.lock().unwrap().mon.on_death(
            &lan_peer.node_id,
            DirectTier::Lan,
            DeathReason::HandshakeDeadline,
            true,
            Instant::now(),
        );

        rt.install_peers(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            std::slice::from_ref(&lan_peer),
            Some(&ctx),
            &mut upgrade_probes,
            &mut test_relay_bq(),
            "test",
        )
        .await;
        assert!(
            !upgrade_probes.contains_key(&lan_peer.node_id),
            "no upgrade probe against a monitor-penalized tier"
        );
        // The suppression lapses (H_ORDINARY) — locked by path.rs's parity
        // curves; here we just confirm the monitor still holds the strike.
        assert_eq!(
            rt.path_shadow
                .lock()
                .unwrap()
                .mon
                .strikes(&lan_peer.node_id, DirectTier::Lan),
            1
        );
    }

    /// P8 — the resume detector: fires only when wall-clock outran the
    /// monotonic clock by more than the threshold (suspend), not on NTP-step
    /// noise or ordinary ticks.
    #[test]
    fn resume_detector_fires_on_suspend_skew_only() {
        let tick = Duration::from_secs(5);
        assert!(!resumed_from_suspend(tick, tick), "ordinary tick");
        assert!(
            !resumed_from_suspend(tick, tick + Duration::from_secs(60)),
            "NTP step below threshold"
        );
        assert!(
            resumed_from_suspend(tick, tick + Duration::from_secs(3600)),
            "an hour of sleep must fire"
        );
        assert!(
            !resumed_from_suspend(Duration::from_secs(3600), Duration::from_secs(3600)),
            "long but consistent gap (loop stalled, no sleep) must not fire"
        );
    }

    /// rc.225 (re-scoped by PR-E) — the endpoint-change PREDICATE reacts to
    /// the LAN/srflx buckets but NOT the churny relay-advert bucket (the
    /// wipe itself is `PathMonitor::on_endpoint_change`, locked by path.rs's
    /// `endpoint_change_resets_penalties_strikes_and_q`).
    #[test]
    fn endpoint_change_predicate_tracks_direct_buckets_only() {
        let a = ObjectId::from_bytes([1; 12]);
        let mk = |lan: &[&str], srflx: &[&str], relay: &[&str]| NetmapPeer {
            node_id: a,
            overlay_ip: "100.64.0.9".into(),
            name: "t".into(),
            wg_public_key: String::new(),
            endpoints: relay.iter().map(|s| s.to_string()).collect(),
            lan_endpoints: lan.iter().map(|s| s.to_string()).collect(),
            srflx_endpoints: srflx.iter().map(|s| s.to_string()).collect(),
            srflx_nat: None,
            relay_home: None,
            reachable: true,
            supports_quic: false,
            supports_relay_single: false,
            supports_derp: false,
            routes: vec![],
            agent_id: None,
        };
        let base = mk(
            &["192.168.68.126:51573"],
            &["37.63.112.129:58770"],
            &["94.130.141.74:1"],
        );
        assert!(
            !direct_endpoints_changed(&base, &base.clone()),
            "identical ⇒ no reset"
        );
        assert!(
            direct_endpoints_changed(
                &base,
                &mk(
                    &["192.168.68.126:60001"],
                    &["37.63.112.129:58770"],
                    &["94.130.141.74:1"]
                )
            ),
            "LAN port change (daemon restart) ⇒ reset"
        );
        assert!(
            direct_endpoints_changed(
                &base,
                &mk(
                    &["192.168.68.126:51573"],
                    &["37.63.112.129:60002"],
                    &["94.130.141.74:1"]
                )
            ),
            "srflx change (roam / NAT rebind) ⇒ reset"
        );
        assert!(
            !direct_endpoints_changed(
                &base,
                &mk(
                    &["192.168.68.126:51573"],
                    &["37.63.112.129:58770"],
                    &["5.9.157.226:9"]
                )
            ),
            "relay-advert churn must NOT reset (it changes on every re-allocation)"
        );
        // The deadlines are ordered srflx/LAN (tight) < public (loose) <
        // relay (loosest — needs BOTH ends' allocations, and the peer's grant
        // cycle can lag). rc.204 — LAN gained a deadline: a false same-subnet
        // match must demote, not zombie forever. rc.223 — RELAY gained one
        // too: a never-handshaked relay was invisible to every other detector
        // (pre-handshake traffic advances neither tx nor rx) and wedged the
        // pair for good — while also starving the P7 churn counter.
        assert!(SRFLX_HANDSHAKE_DEADLINE < PUBLIC_HANDSHAKE_DEADLINE);
        assert!(LAN_HANDSHAKE_DEADLINE < PUBLIC_HANDSHAKE_DEADLINE);
        assert!(PUBLIC_HANDSHAKE_DEADLINE < RELAY_HANDSHAKE_DEADLINE);
        assert!(
            LAN_HANDSHAKE_DEADLINE > DIRECT_GRACE,
            "a blown LAN deadline must land past the warm-up grace"
        );
        assert!(
            RELAY_HANDSHAKE_DEADLINE > DIRECT_GRACE,
            "a blown relay deadline must land past the warm-up grace"
        );
        assert_eq!(DirectTier::Lan.handshake_deadline(), LAN_HANDSHAKE_DEADLINE);
        assert_eq!(
            DirectTier::Relay.handshake_deadline(),
            RELAY_HANDSHAKE_DEADLINE
        );
        // PR-E — the escalation thresholds live in the monitor now; keep the
        // cross-tier relationships the deleted legacy tests guarded (const
        // blocks: these are compile-time invariants, which is the point).
        const {
            assert!(path::MAX_FAILURES_SRFLX > path::MAX_FAILURES_LAN_PUBLIC);
        }
        assert!(path::H_ESCALATED.as_secs() > path::H_ORDINARY.as_secs() * 5);
    }

    /// Phase C (D7 + CC1) — the health sweep tears down a zombie srflx punch (a
    /// Srflx-tier carrier that never completed its WG handshake) once past the
    /// srflx deadline, and books the failure ONLY on the srflx cooldown tier —
    /// never poisoning the proven LAN or public-direct tiers.
    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_tears_down_zombie_srflx_and_cools_only_srflx_tier() {
        let kp = WgKeypair::generate();
        let peer_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let (tun_mock, _inj, _del) = MockTun::new();
        let tf: TunFactory = {
            let m = tun_mock.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);

        // A direct peer dialing a DEAD destination → the handshake never
        // completes → `peer_handshake_done` stays false (the zombie condition).
        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        wg.ensure_direct_demux(sock.clone());
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let overlay_ip = Ipv4Addr::new(100, 64, 0, 2);
        wg.add_direct_peer(
            sock.clone(),
            peer_kp.public.to_bytes(),
            overlay_ip,
            dead,
            true,
        )
        .await;
        assert_eq!(
            wg.peer_handshake_done(&peer_kp.public.to_bytes()),
            Some(false),
            "precondition: the punch never handshook"
        );

        let nid = ObjectId::from_bytes([5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut by_node = HashMap::new();
        by_node.insert(
            nid,
            Installed {
                // Installed past the srflx handshake deadline (and the grace).
                since: Instant::now()
                    .checked_sub(Duration::from_secs(SRFLX_HANDSHAKE_DEADLINE.as_secs() + 3))
                    .unwrap(),
                public_direct_dst: Some(dead),
                ..Installed::base(
                    peer_kp.public.to_bytes(),
                    overlay_ip,
                    DirectTier::Srflx,
                    Instant::now(),
                )
            },
        );

        let tun: Arc<dyn TunIo> = tun_mock;
        let mut relay_refresh: HashMap<ObjectId, Instant> = HashMap::new();
        let mut relay: Option<RelayCoordinator> = None;
        let current_peers: HashMap<ObjectId, NetmapPeer> = HashMap::new();

        rt.sweep_carrier_health(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut relay_refresh,
            &current_peers,
        )
        .await;

        assert!(
            !by_node.contains_key(&nid),
            "the zombie srflx carrier is torn down"
        );
        // PR-E — the failure bookkeeping is the monitor's suppression plane.
        let now = Instant::now();
        let s = rt.path_shadow.lock().unwrap();
        assert!(
            !s.mon.eligible(&nid, DirectTier::Srflx, now),
            "the srflx suppression penalty is booked"
        );
        assert_eq!(
            s.mon.strikes(&nid, DirectTier::Srflx),
            1,
            "one srflx strike"
        );
        assert!(
            s.mon.eligible(&nid, DirectTier::Lan, now) && s.mon.strikes(&nid, DirectTier::Lan) == 0,
            "CC1: the LAN tier is NOT poisoned"
        );
        assert!(
            s.mon.eligible(&nid, DirectTier::Public, now)
                && s.mon.strikes(&nid, DirectTier::Public) == 0,
            "CC1: the public-direct tier is NOT poisoned"
        );
    }

    /// rc.204 — the health sweep tears down a zombie LAN carrier (a Lan-tier
    /// carrier that never completed its WG handshake) once past the LAN
    /// deadline, and books the failure ONLY on the LAN cooldown tier. Before
    /// rc.204 the LAN tier had no handshake deadline: pre-handshake tx/rx stay
    /// flat, so the rx-flat heuristic never fired and a false same-subnet match
    /// was a PERMANENT zombie with no relay fallback (field-observed
    /// 2026-07-21: every LAN pair wedged in `HANDSHAKE(REKEY_TIMEOUT)`).
    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_tears_down_zombie_lan_and_cools_only_lan_tier() {
        let kp = WgKeypair::generate();
        let peer_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let (tun_mock, _inj, _del) = MockTun::new();
        let tf: TunFactory = {
            let m = tun_mock.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);

        // A LAN carrier dialing a DEAD destination → the handshake never
        // completes → `peer_handshake_done` stays false (the zombie condition).
        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        wg.ensure_direct_demux(sock.clone());
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let overlay_ip = Ipv4Addr::new(100, 64, 0, 3);
        wg.add_direct_peer(
            sock.clone(),
            peer_kp.public.to_bytes(),
            overlay_ip,
            dead,
            true,
        )
        .await;
        assert_eq!(
            wg.peer_handshake_done(&peer_kp.public.to_bytes()),
            Some(false),
            "precondition: the LAN carrier never handshook"
        );

        let nid = ObjectId::from_bytes([6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut by_node = HashMap::new();
        by_node.insert(
            nid,
            Installed {
                // Installed past the LAN handshake deadline (and the grace).
                since: Instant::now()
                    .checked_sub(Duration::from_secs(LAN_HANDSHAKE_DEADLINE.as_secs() + 3))
                    .unwrap(),
                ..Installed::base(
                    peer_kp.public.to_bytes(),
                    overlay_ip,
                    DirectTier::Lan,
                    Instant::now(),
                )
            },
        );

        let tun: Arc<dyn TunIo> = tun_mock;
        let mut relay_refresh: HashMap<ObjectId, Instant> = HashMap::new();
        let mut relay: Option<RelayCoordinator> = None;
        let current_peers: HashMap<ObjectId, NetmapPeer> = HashMap::new();

        rt.sweep_carrier_health(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut relay_refresh,
            &current_peers,
        )
        .await;

        assert!(
            !by_node.contains_key(&nid),
            "the zombie LAN carrier is torn down"
        );
        // PR-E — the failure bookkeeping is the monitor's suppression plane.
        let now = Instant::now();
        let s = rt.path_shadow.lock().unwrap();
        assert!(
            !s.mon.eligible(&nid, DirectTier::Lan, now),
            "the LAN suppression penalty is booked"
        );
        assert_eq!(s.mon.strikes(&nid, DirectTier::Lan), 1, "one LAN strike");
        assert!(
            s.mon.eligible(&nid, DirectTier::Srflx, now)
                && s.mon.strikes(&nid, DirectTier::Srflx) == 0,
            "CC1: the srflx tier is NOT poisoned"
        );
        assert!(
            s.mon.eligible(&nid, DirectTier::Public, now)
                && s.mon.strikes(&nid, DirectTier::Public) == 0,
            "CC1: the public-direct tier is NOT poisoned"
        );
    }

    /// A `TunIo` that records every peer-route add/remove, so the tests can
    /// assert what actually reached the OS routing table. `MockTun` takes the
    /// no-op default `add_peer_route`/`del_peer_route`, which makes route calls
    /// unobservable.
    struct RouteRecordingTun {
        routes: std::sync::Mutex<Vec<(&'static str, Ipv4Addr)>>,
    }
    impl RouteRecordingTun {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                routes: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn calls(&self) -> Vec<(&'static str, Ipv4Addr)> {
            self.routes.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl TunIo for RouteRecordingTun {
        async fn read_packet(&self) -> io::Result<Vec<u8>> {
            // Never yields — the prune tests drive the helpers directly.
            std::future::pending().await
        }
        async fn write_packet(&self, _packet: &[u8]) -> io::Result<()> {
            Ok(())
        }
        async fn add_peer_route(&self, ip: Ipv4Addr) -> io::Result<()> {
            self.routes.lock().unwrap().push(("add", ip));
            Ok(())
        }
        async fn del_peer_route(&self, ip: Ipv4Addr) {
            self.routes.lock().unwrap().push(("del", ip));
        }
        async fn defend_self_route(&self, ip: Ipv4Addr) {
            self.routes.lock().unwrap().push(("defend", ip));
        }
    }

    /// rc.278 — the route-guard re-assert must cover our OWN overlay `/32`, not
    /// just peers'. A full-tunnel VPN (Check Point) installs a metric-1 `/32`
    /// for our own address that out-ranks Windows' metric-256 on-link route, so
    /// every packet addressed to us — including the REPLY to everything we
    /// initiate — is forwarded into the corp tunnel instead of delivered
    /// locally (field: pc50045, 100 % IPv4 loss both ways while its WireGuard
    /// carriers were healthy and IPv6 worked). rc.285 — the test now drives the
    /// REAL wave (`defended_routes` → `run_defense_wave`, the exact code both
    /// select-loop arms spawn) instead of a hand-mirrored copy of the arms.
    #[tokio::test(flavor = "multi_thread")]
    async fn route_reassert_covers_self_and_peers() {
        let tun = RouteRecordingTun::new();
        let self_v4 = Ipv4Addr::new(100, 64, 0, 28);
        let peers = [Ipv4Addr::new(100, 64, 0, 2), Ipv4Addr::new(100, 64, 0, 4)];
        let mut by_node = HashMap::new();
        for (i, ip) in peers.iter().enumerate() {
            by_node.insert(ObjectId::new(), installed_at([i as u8 + 1; 32], *ip));
        }

        let t: Arc<dyn TunIo> = tun.clone();
        run_defense_wave(t, defended_routes(&by_node, self_v4)).await;

        let got = tun.calls();
        assert_eq!(got.len(), peers.len() + 1);
        assert_eq!(
            got.last(),
            Some(&("defend", self_v4)),
            "our own address defended LAST, after every peer re-assert"
        );
        let added: HashSet<Ipv4Addr> = got
            .iter()
            .filter(|(op, _)| *op == "add")
            .map(|(_, ip)| *ip)
            .collect();
        assert_eq!(
            added,
            peers.iter().copied().collect::<HashSet<_>>(),
            "every installed peer re-asserted exactly once"
        );
        assert!(
            !got.iter().any(|(op, ip)| *op == "add" && *ip == self_v4),
            "our own address must be EVICTION-only — never re-added via the TUN \
             (the on-link route already serves local delivery; adding a /32 to \
             ourselves risks a forwarding loop)"
        );
    }

    /// rc.285 — the defended-set composition is declarative: every installed
    /// peer exactly once, self exactly once and LAST, and nothing else. (The
    /// P5 exit `/1`s are deliberately ABSENT — their re-assert is inline-only
    /// for the teardown mutual-exclusion; see [`Defend`].)
    #[test]
    fn defended_set_lists_every_peer_once_and_self_last() {
        let self_v4 = Ipv4Addr::new(100, 64, 0, 28);
        let ips = [
            Ipv4Addr::new(100, 64, 0, 2),
            Ipv4Addr::new(100, 64, 0, 4),
            Ipv4Addr::new(100, 64, 0, 9),
        ];
        let mut by_node = HashMap::new();
        for (i, ip) in ips.iter().enumerate() {
            by_node.insert(ObjectId::new(), installed_at([i as u8 + 1; 32], *ip));
        }

        let set = defended_routes(&by_node, self_v4);
        assert_eq!(set.len(), ips.len() + 1);
        assert_eq!(set.last(), Some(&Defend::EvictSelf(self_v4)));
        let asserted: HashSet<Ipv4Addr> = set
            .iter()
            .filter_map(|d| match d {
                Defend::AssertPeer(ip) => Some(*ip),
                Defend::EvictSelf(_) => None,
            })
            .collect();
        assert_eq!(asserted, ips.iter().copied().collect::<HashSet<_>>());

        // An empty mesh still defends our own address.
        assert_eq!(
            defended_routes(&HashMap::new(), self_v4),
            vec![Defend::EvictSelf(self_v4)]
        );
    }

    /// The off-loop ALLOCATE queue's test twin (see `test_relay_bq`).
    fn test_alloc_q() -> RelayAllocQueue {
        let (tx, _rx) = mpsc::channel(4);
        RelayAllocQueue {
            in_flight: HashMap::new(),
            epoch: 0,
            tx,
        }
    }

    /// Build an `Installed` for a peer whose carrier details don't matter.
    fn installed_at(pubkey: [u8; 32], overlay_ip: Ipv4Addr) -> Installed {
        Installed::base(pubkey, overlay_ip, DirectTier::Lan, Instant::now())
    }

    /// The full netmap is authoritative: a peer it no longer lists loses its WG
    /// identity, its crypto-route and its OS `/32`. Before the prune, only a
    /// delta `removes` could evict — so a peer that vanished while this client
    /// was disconnected stayed installed (and kept accepting inbound) forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn full_netmap_prune_tears_down_a_vanished_peer() {
        let kp = WgKeypair::generate();
        let gone_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let tun_rec = RouteRecordingTun::new();
        let tf: TunFactory = {
            let m = tun_rec.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);

        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        wg.ensure_direct_demux(sock.clone());
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let gone_pk = gone_kp.public.to_bytes();
        wg.add_direct_peer(sock.clone(), gone_pk, IP_A, dead, true)
            .await;

        let nid = ObjectId::from_bytes([7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut by_node = HashMap::from([(nid, installed_at(gone_pk, IP_A))]);
        let tun: Arc<dyn TunIo> = tun_rec.clone();
        let mut relay: Option<RelayCoordinator> = None;
        let mut relay_bq = test_relay_bq();
        let mut alloc_q = test_alloc_q();
        let mut upgrade_probes: HashMap<ObjectId, UpgradeProbe> = HashMap::new();
        // The peer is NOT in the incoming netmap — i.e. it vanished.
        let mut current_peers: HashMap<ObjectId, NetmapPeer> = HashMap::new();
        let mut relay_refresh: HashMap<ObjectId, Instant> = HashMap::new();

        rt.evict_peer(
            nid,
            &mut wg,
            &mut by_node,
            &tun,
            &mut relay,
            &mut relay_bq,
            &mut alloc_q,
            &mut upgrade_probes,
            &mut current_peers,
            &mut relay_refresh,
        )
        .await;

        assert!(!by_node.contains_key(&nid), "the carrier is gone");
        assert!(
            wg.peer_handshake_done(&gone_pk).is_none(),
            "the WG peer (and its inbound acceptance) is gone"
        );
        assert_eq!(
            tun_rec.calls(),
            vec![("del", IP_A)],
            "the OS /32 is dropped — nobody else claims it"
        );
    }

    /// …but a vanished peer whose address was RECYCLED to a live node must not
    /// take that node's OS route with it. This is the blackhole the release
    /// feature would otherwise introduce.
    #[tokio::test(flavor = "multi_thread")]
    async fn prune_keeps_the_os_route_of_a_recycled_overlay_ip() {
        let kp = WgKeypair::generate();
        let stale_kp = WgKeypair::generate();
        let live_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let tun_rec = RouteRecordingTun::new();
        let tf: TunFactory = {
            let m = tun_rec.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);

        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        wg.ensure_direct_demux(sock.clone());
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let stale_pk = stale_kp.public.to_bytes();
        let live_pk = live_kp.public.to_bytes();
        // Both hold IP_A: the server released it from `stale` and handed it to
        // `live`, and only now do we get around to reaping `stale`.
        wg.add_direct_peer(sock.clone(), stale_pk, IP_A, dead, true)
            .await;
        wg.add_direct_peer(sock.clone(), live_pk, IP_A, dead, true)
            .await;

        let stale_nid = ObjectId::from_bytes([8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let live_nid = ObjectId::from_bytes([9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut by_node = HashMap::from([
            (stale_nid, installed_at(stale_pk, IP_A)),
            (live_nid, installed_at(live_pk, IP_A)),
        ]);
        let tun: Arc<dyn TunIo> = tun_rec.clone();
        let mut relay: Option<RelayCoordinator> = None;
        let mut relay_bq = test_relay_bq();
        let mut alloc_q = test_alloc_q();
        let mut upgrade_probes: HashMap<ObjectId, UpgradeProbe> = HashMap::new();
        let mut current_peers: HashMap<ObjectId, NetmapPeer> = HashMap::new();
        let mut relay_refresh: HashMap<ObjectId, Instant> = HashMap::new();

        rt.evict_peer(
            stale_nid,
            &mut wg,
            &mut by_node,
            &tun,
            &mut relay,
            &mut relay_bq,
            &mut alloc_q,
            &mut upgrade_probes,
            &mut current_peers,
            &mut relay_refresh,
        )
        .await;

        assert!(by_node.contains_key(&live_nid), "the live peer survives");
        assert!(
            tun_rec.calls().is_empty(),
            "the OS /32 is KEPT — the address now belongs to the live peer, got {:?}",
            tun_rec.calls()
        );
        assert!(
            wg.peer_handshake_done(&live_pk).is_some(),
            "the live peer's WG identity is untouched"
        );
        assert!(
            wg.peer_handshake_done(&stale_pk).is_none(),
            "…and the stale one is gone"
        );
        // The crypto-route still resolves IP_A, and to the LIVE peer.
        assert_eq!(wg.test_route(&IP_A), Some(live_pk));
    }

    /// rc.208 make-before-break test scaffold: a peer currently on RELAY with a
    /// shadow direct PROBE in flight for `dst`. Returns the runtime, the wg
    /// device (probe already started), the `by_node` (relay), and the
    /// `upgrade_probes` metadata — the caller drives `sweep_upgrade_probes`.
    async fn mbb_fixture(
        tier: DirectTier,
        probe_since: Instant,
    ) -> (
        OverlayRuntime,
        WgDevice,
        Arc<dyn TunIo>,
        HashMap<ObjectId, Installed>,
        HashMap<ObjectId, UpgradeProbe>,
        ObjectId,
        [u8; 32],
    ) {
        let kp = WgKeypair::generate();
        let peer_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let (tun_mock, _inj, _del) = MockTun::new();
        let tf: TunFactory = {
            let m = tun_mock.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);

        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        wg.ensure_direct_demux(sock.clone());
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let overlay_ip = Ipv4Addr::new(100, 64, 0, 7);
        let pk = peer_kp.public.to_bytes();
        wg.start_direct_probe(sock, pk, overlay_ip, dead, true)
            .await;

        let nid = ObjectId::from_bytes([9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut by_node = HashMap::new();
        // The peer routes over RELAY while the probe runs (make-before-break).
        by_node.insert(
            nid,
            Installed::base(pk, overlay_ip, DirectTier::Relay, Instant::now()),
        );
        let mut upgrade_probes = HashMap::new();
        upgrade_probes.insert(
            nid,
            UpgradeProbe {
                pubkey: pk,
                overlay_ip,
                dst: dead,
                tier,
                since: probe_since,
                initiated: true,
                local: None,
            },
        );
        (rt, wg, tun_mock, by_node, upgrade_probes, nid, pk)
    }

    /// Make-before-break — a probe whose handshake LATCHES (direct proven both
    /// ways) is promoted: `by_node` retags to the direct tier, the shadow probe
    /// leaves the probe map, and the tier's accumulated strikes clear. The relay
    /// was held the entire time (no stall).
    #[tokio::test(flavor = "multi_thread")]
    async fn mbb_promotes_probe_on_handshake_latch() {
        let (rt, mut wg, tun, mut by_node, mut upgrade_probes, nid, pk) =
            mbb_fixture(DirectTier::Srflx, Instant::now()).await;
        assert_eq!(wg.probe_count(), 1);
        // The direct handshake completed (peer's response reached us).
        wg.test_latch_probe_handshake_done(&pk);

        // Stale strikes in the monitor — the latch must clear them (CC1).
        rt.path_shadow.lock().unwrap().mon.on_death(
            &nid,
            DirectTier::Srflx,
            DeathReason::HandshakeDeadline,
            true,
            Instant::now() - Duration::from_secs(3600),
        );
        let mut relay: Option<RelayCoordinator> = None;

        rt.sweep_upgrade_probes(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut upgrade_probes,
            &mut test_relay_bq(),
        )
        .await;

        assert!(upgrade_probes.is_empty(), "the probe settled");
        assert_eq!(wg.probe_count(), 0, "promoted out of the shadow map");
        let inst = by_node.get(&nid).expect("still tracked");
        assert!(inst.is_direct, "cut over to a DIRECT carrier");
        assert_eq!(inst.tier, DirectTier::Srflx);
        assert_eq!(
            inst.public_direct_dst.map(|d| d.to_string()),
            Some("127.0.0.1:9".into()),
            "off-link tier records its exit-exemption dst"
        );
        assert_eq!(
            rt.path_shadow
                .lock()
                .unwrap()
                .mon
                .strikes(&nid, DirectTier::Srflx),
            0,
            "success clears the tier's strikes"
        );
    }

    /// P3 PR-B — the probe's REAL latch latency is measured from the PROBE
    /// START (the runtime's `since`), not the process epoch, and reads `None`
    /// while unlatched. Locks the `handshake_at` plumbing the PathMonitor's
    /// latch Q credit consumes (pre-PR-B the sweep's 5 s tick quantization
    /// made any number worse than none).
    #[tokio::test(flavor = "multi_thread")]
    async fn probe_latency_measures_from_probe_start() {
        let (_rt, wg, _tun, _by_node, _probes, _nid, pk) =
            mbb_fixture(DirectTier::Srflx, Instant::now()).await;
        let since = Instant::now() - Duration::from_millis(200);
        assert_eq!(
            wg.probe_handshake_latency_ms(&pk, since),
            None,
            "unlatched probe has no latency"
        );
        wg.test_latch_probe_handshake_done(&pk);
        let ms = wg.probe_handshake_latency_ms(&pk, since).expect("latched");
        // PR-C — floor at 170, not 200: Windows quantizes Instant deltas to
        // the ~15.6 ms timer resolution, so the 200 ms back-date can measure
        // as low as ~185 (field-flaked on pristine master 2026-07-28).
        assert!(
            (170..10_000).contains(&ms),
            "latency runs from probe start (the back-dated since sets a ~200 ms floor, \
             minus Windows timer quantization), got {ms}"
        );
    }

    /// Make-before-break — a probe that never latches within the tier deadline is
    /// dropped and the RELAY is left untouched (the whole point: no stall on a
    /// peer that can only relay). The failure books ONE strike on the probed
    /// tier (CC1), so a persistently-unreachable tier still escalates.
    #[tokio::test(flavor = "multi_thread")]
    async fn mbb_expires_probe_and_keeps_relay_past_deadline() {
        let stale = Instant::now()
            .checked_sub(Duration::from_secs(SRFLX_HANDSHAKE_DEADLINE.as_secs() + 3))
            .unwrap();
        let (rt, mut wg, tun, mut by_node, mut upgrade_probes, nid, _pk) =
            mbb_fixture(DirectTier::Srflx, stale).await;
        assert_eq!(wg.probe_count(), 1);
        // NOT latched — the direct path never handshook.

        let mut relay: Option<RelayCoordinator> = None;

        rt.sweep_upgrade_probes(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut upgrade_probes,
            &mut test_relay_bq(),
        )
        .await;

        assert!(upgrade_probes.is_empty(), "the probe settled");
        assert_eq!(wg.probe_count(), 0, "the failed probe was dropped");
        let inst = by_node.get(&nid).expect("relay carrier kept");
        assert!(!inst.is_direct, "the RELAY carrier is untouched (no stall)");
        assert_eq!(inst.tier, DirectTier::Relay);
        let s = rt.path_shadow.lock().unwrap();
        assert_eq!(
            s.mon.strikes(&nid, DirectTier::Srflx),
            1,
            "one srflx strike booked"
        );
        assert!(
            !s.mon.eligible(&nid, DirectTier::Srflx, Instant::now()),
            "the srflx suppression penalty is set"
        );
    }

    /// Make-before-break — while a probe is in flight (not yet latched, within
    /// the deadline) the sweep is a no-op: the probe stays, and the peer keeps
    /// routing over its relay carrier.
    #[tokio::test(flavor = "multi_thread")]
    async fn mbb_holds_relay_while_probe_in_flight() {
        let (rt, mut wg, tun, mut by_node, mut upgrade_probes, nid, _pk) =
            mbb_fixture(DirectTier::Srflx, Instant::now()).await;
        let mut relay: Option<RelayCoordinator> = None;

        rt.sweep_upgrade_probes(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut upgrade_probes,
            &mut test_relay_bq(),
        )
        .await;

        assert_eq!(upgrade_probes.len(), 1, "still probing");
        assert_eq!(wg.probe_count(), 1, "the probe is still in flight");
        assert!(
            !by_node.get(&nid).unwrap().is_direct,
            "the relay is still the routing carrier"
        );
        assert_eq!(
            rt.path_shadow
                .lock()
                .unwrap()
                .mon
                .strikes(&nid, DirectTier::Srflx),
            0,
            "no strike while the probe is still pending"
        );
    }

    /// rc.208 make-before-break INBOUND — an authenticated direct init arriving
    /// while the peer is on RELAY is accepted as a SHADOW PROBE (relay held), not
    /// a destructive re-point. With the feature OFF (the default) the same init
    /// tears the relay down and installs direct immediately.
    #[tokio::test(flavor = "multi_thread")]
    async fn mbb_inbound_accepts_init_as_probe_and_holds_relay() {
        let our = WgKeypair::generate();
        let peer_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let (tun_mock, _inj, _del) = MockTun::new();
        let tf: TunFactory = {
            let m = tun_mock.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(our.clone(), out_tx, tf, 1280);
        let tun: Arc<dyn TunIo> = tun_mock;

        // A private (LAN) source → tier Lan, no cooldown gating.
        let src: SocketAddr = "192.168.50.9:41000".parse().unwrap();
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let np = peer(&peer_kp, "100.64.0.7");
        let nid = np.node_id;
        let mut current_peers = HashMap::new();
        current_peers.insert(nid, np);
        let relay_installed = || {
            Installed::base(
                peer_kp.public.to_bytes(),
                Ipv4Addr::new(100, 64, 0, 7),
                DirectTier::Relay,
                Instant::now(),
            )
        };
        let mut relay: Option<RelayCoordinator> = None;

        // Serialize env mutation (the CI overlay-l3 suite runs --test-threads=1).
        let key = "ROOMLER_NODE_OVERLAY_MBB";
        let restore = std::env::var(key).ok();

        // ── MBB ON: accept as a probe, hold the relay ──
        unsafe { std::env::set_var(key, "1") };
        let (mut wg, _rx) = WgDevice::new(our.secret.clone());
        let mut by_node = HashMap::from([(nid, relay_installed())]);
        let mut probes = HashMap::new();
        let inb = crate::overlay::wg::DirectInbound {
            src,
            sock: sock.clone(),
            packet: crate::overlay::wg::test_genuine_init(&peer_kp.secret, our.public.to_bytes()),
        };
        rt.handle_direct_inbound(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &current_peers,
            &mut probes,
            &mut test_relay_bq(),
            inb,
        )
        .await;
        assert_eq!(
            wg.probe_count(),
            1,
            "inbound init accepted as a shadow probe"
        );
        assert!(probes.contains_key(&nid), "the probe is recorded");
        let inst = by_node.get(&nid).expect("still tracked");
        assert!(!inst.is_direct, "the RELAY carrier is HELD, not destroyed");
        assert_eq!(inst.tier, DirectTier::Relay);

        // ── MBB OFF: the same init destructively re-points to direct ──
        unsafe { std::env::set_var(key, "0") };
        let (mut wg2, _rx2) = WgDevice::new(our.secret.clone());
        let mut by_node2 = HashMap::from([(nid, relay_installed())]);
        let mut probes2 = HashMap::new();
        let inb2 = crate::overlay::wg::DirectInbound {
            src,
            sock: sock.clone(),
            packet: crate::overlay::wg::test_genuine_init(&peer_kp.secret, our.public.to_bytes()),
        };
        rt.handle_direct_inbound(
            &mut wg2,
            &mut by_node2,
            &mut relay,
            &tun,
            &current_peers,
            &mut probes2,
            &mut test_relay_bq(),
            inb2,
        )
        .await;
        assert_eq!(wg2.probe_count(), 0, "MBB off → no probe");
        assert!(probes2.is_empty());
        assert!(
            by_node2.get(&nid).expect("tracked").is_direct,
            "MBB off → destructive re-point to a DIRECT carrier (pre-rc.208)"
        );

        match restore {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        };
    }

    /// rc.206 — the silent-zombie backstop. An ESTABLISHED direct carrier whose
    /// inbound packets stop (peer roamed / NAT rebind / path died mid-session)
    /// goes tx-flat AND rx-flat once boringtun gives up re-handshaking, so the
    /// `tx>last_tx && rx==last_rx` heuristic reads it as benign idle and
    /// `punch_dead` can't fire (the handshake already latched). Pre-rc.206 it
    /// lived forever — field-observed as an 8-hour "direct" carrier at 100 %
    /// loss with a frozen last-seen. The absolute `last_rx_at` staleness deadline
    /// tears it down and re-requests via relay.
    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_tears_down_established_carrier_gone_silent() {
        let kp = WgKeypair::generate();
        let peer_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let (tun_mock, _inj, _del) = MockTun::new();
        let tf: TunFactory = {
            let m = tun_mock.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);

        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        wg.ensure_direct_demux(sock.clone());
        let dst: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let overlay_ip = Ipv4Addr::new(100, 64, 0, 2);
        wg.add_direct_peer(
            sock.clone(),
            peer_kp.public.to_bytes(),
            overlay_ip,
            dst,
            true,
        )
        .await;
        // Latch the handshake so this is an ESTABLISHED carrier: `punch_dead`
        // (which fires only PRE-handshake) can't be the reason it's reaped —
        // isolating the rx-staleness trigger.
        wg.test_latch_handshake_done(&peer_kp.public.to_bytes());
        assert_eq!(
            wg.peer_handshake_done(&peer_kp.public.to_bytes()),
            Some(true),
            "precondition: the carrier is established"
        );
        // Pin `last_traffic` to the current snapshot so the tx/rx-delta heuristic
        // takes its else-branch (no strike accrues) — only rx-staleness can be
        // the trigger for this teardown.
        let snap = wg.peer_traffic(&peer_kp.public.to_bytes()).unwrap();

        let nid = ObjectId::from_bytes([6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        // Installed (and last received) well past the rx-stale deadline — hence
        // also past DIRECT_GRACE.
        let stale = Instant::now()
            .checked_sub(RX_STALE_DEADLINE + Duration::from_secs(5))
            .unwrap();
        let mut by_node = HashMap::new();
        by_node.insert(
            nid,
            Installed {
                last_traffic: snap,
                public_direct_dst: Some(dst),
                ..Installed::base(
                    peer_kp.public.to_bytes(),
                    overlay_ip,
                    DirectTier::Srflx,
                    stale,
                )
            },
        );

        let tun: Arc<dyn TunIo> = tun_mock;
        let mut relay_refresh: HashMap<ObjectId, Instant> = HashMap::new();
        let mut relay: Option<RelayCoordinator> = None;
        let current_peers: HashMap<ObjectId, NetmapPeer> = HashMap::new();

        rt.sweep_carrier_health(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut relay_refresh,
            &current_peers,
        )
        .await;

        assert!(
            !by_node.contains_key(&nid),
            "the silent established carrier is torn down via rx-staleness"
        );
        assert!(
            !rt.path_shadow
                .lock()
                .unwrap()
                .mon
                .eligible(&nid, DirectTier::Srflx, Instant::now()),
            "the failure books on the carrier's own tier → relay fallback"
        );
    }

    /// rc.206 — the rx-staleness backstop must NOT reap a HEALTHY but IDLE
    /// carrier. A live peer's only inbound on a quiet link is WG persistent-
    /// keepalives, which advance the keepalive-inclusive `rx_any` counter but
    /// NOT the IP-data `rx`. This locks that the sweep refreshes a stale
    /// `last_rx_at` from a keepalive (drained via `peer_take_rx_any`) so the
    /// carrier survives — the false premise the reviewer caught, now a real test
    /// (the earlier version injected a fresh `last_rx_at` keepalives never move).
    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_keeps_established_idle_carrier_heard_via_keepalive() {
        let kp = WgKeypair::generate();
        let peer_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let (tun_mock, _inj, _del) = MockTun::new();
        let tf: TunFactory = {
            let m = tun_mock.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);

        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        wg.ensure_direct_demux(sock.clone());
        let dst: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let overlay_ip = Ipv4Addr::new(100, 64, 0, 2);
        wg.add_direct_peer(
            sock.clone(),
            peer_kp.public.to_bytes(),
            overlay_ip,
            dst,
            true,
        )
        .await;
        wg.test_latch_handshake_done(&peer_kp.public.to_bytes());
        // Simulate a persistent-keepalive landing THIS interval: `rx_any` bumps
        // but the IP-data `rx` does NOT (a keepalive decapsulates to Done). The
        // sweep must read that as "heard" and refresh an otherwise-stale
        // `last_rx_at` — the exact case the pre-rc.206 `rx`-only signal missed.
        wg.test_bump_rx_any(&peer_kp.public.to_bytes());
        let snap = wg.peer_traffic(&peer_kp.public.to_bytes()).unwrap();

        let nid = ObjectId::from_bytes([7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        // `last_rx_at` last advanced > 90 s ago (looks silent) — but the keepalive
        // above proves the carrier is alive, so the sweep must NOT reap it.
        let old = Instant::now()
            .checked_sub(RX_STALE_DEADLINE + Duration::from_secs(5))
            .unwrap();
        let mut by_node = HashMap::new();
        by_node.insert(
            nid,
            Installed {
                last_traffic: snap,
                public_direct_dst: Some(dst),
                ..Installed::base(
                    peer_kp.public.to_bytes(),
                    overlay_ip,
                    DirectTier::Srflx,
                    old,
                )
            },
        );

        let tun: Arc<dyn TunIo> = tun_mock;
        let mut relay_refresh: HashMap<ObjectId, Instant> = HashMap::new();
        let mut relay: Option<RelayCoordinator> = None;
        let current_peers: HashMap<ObjectId, NetmapPeer> = HashMap::new();

        rt.sweep_carrier_health(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut relay_refresh,
            &current_peers,
        )
        .await;

        assert!(
            by_node.contains_key(&nid),
            "an idle carrier heard from via keepalive must survive the sweep"
        );
        assert!(
            by_node.get(&nid).unwrap().last_rx_at > old,
            "the sweep refreshed last_rx_at from the keepalive (rx_any), not rx"
        );
        {
            let s = rt.path_shadow.lock().unwrap();
            assert!(
                s.mon.eligible(&nid, DirectTier::Srflx, Instant::now())
                    && s.mon.strikes(&nid, DirectTier::Srflx) == 0,
                "no failure is booked for a healthy carrier"
            );
        }
    }

    /// bind-to-interface-by-route (Phase 1): with the gate OFF, `lan_egress_socket`
    /// returns the same-subnet socket unchanged (byte-identical to pre-change).
    /// With the gate ON and a loopback ctx, the connect()-trick resolves the
    /// loopback source, `classify_egress` says `Use`, and the matching socket is
    /// returned — exercising the OS-routed selection path end to end.
    #[tokio::test(flavor = "multi_thread")]
    async fn lan_egress_socket_gate_off_then_on_selects_socket() {
        let n = "ROOMLER_NODE_OVERLAY_BIND_BY_ROUTE";
        let a = "ROOMLER_AGENT_OVERLAY_BIND_BY_ROUTE";
        let (rn, ra) = (std::env::var(n).ok(), std::env::var(a).ok());
        let lo = Ipv4Addr::LOCALHOST;
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let want = sock.local_addr().unwrap();
        let ctx = DirectCtx {
            socks: vec![(lo, sock)],
            my_ips: vec![lo],
            endpoints: vec![],
            public_sock: None,
            punch: None,
            my_nat: None,
        };
        let dst: SocketAddr = "127.0.0.1:9".parse().unwrap();

        // Gate OFF → the same-subnet socket, no route query.
        unsafe {
            std::env::remove_var(n);
            std::env::remove_var(a);
        }
        let off = lan_egress_socket(&ctx, lo, dst).await.expect("same-subnet");
        assert_eq!(
            off.local_addr().unwrap(),
            want,
            "gate off → same-subnet pick"
        );

        // Gate ON → connect-trick to a loopback dst sources from 127.0.0.1,
        // which is in our socket set → Use(127.0.0.1) → the same socket.
        unsafe { std::env::set_var(n, "1") };
        let on = lan_egress_socket(&ctx, lo, dst)
            .await
            .expect("os-routed socket");
        assert_eq!(on.local_addr().unwrap(), want, "gate on → OS-routed socket");

        unsafe {
            match rn {
                Some(v) => std::env::set_var(n, v),
                None => std::env::remove_var(n),
            }
            match ra {
                Some(v) => std::env::set_var(a, v),
                None => std::env::remove_var(a),
            }
        }
    }

    /// rc.204 — the same-subnet LAN tier must scan ONLY the provenance-pure
    /// `lan_endpoints` bucket. The `endpoints` union also carries the peer's
    /// trickled coturn-RELAYED addresses, and on this fleet the coturn workers
    /// ride the hosts' own public IPs — pre-rc.204 a fleet host same-/24
    /// matched a peer's *relay allocation* and "LAN"-dialed coturn forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn lan_tier_scans_only_the_pure_lan_endpoint_bucket() {
        let kp = WgKeypair::generate();
        let peer_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let (tun_mock, _inj, _del) = MockTun::new();
        let tf: TunFactory = {
            let m = tun_mock.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);
        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let tun: Arc<dyn TunIo> = tun_mock;

        // Our side: one "interface" at 10.1.2.9 (the socket itself is bound to
        // loopback — nothing needs to actually flow in this test).
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let my_ip: Ipv4Addr = "10.1.2.9".parse().unwrap();
        let ctx = DirectCtx {
            socks: vec![(my_ip, sock)],
            my_ips: vec![my_ip],
            endpoints: vec!["10.1.2.9:41000".into()],
            public_sock: None,
            punch: None,
            my_nat: None,
        };
        let mut relay: Option<RelayCoordinator> = None;
        let mut by_node = HashMap::new();

        // A same-/24 address present ONLY in the `endpoints` union (the shape
        // of a trickled relay allocation) must NOT produce a LAN carrier.
        let mut tainted = peer(&peer_kp, "100.64.0.7");
        tainted.endpoints = vec!["10.1.2.3:1000".into()];
        rt.install_peers(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            std::slice::from_ref(&tainted),
            Some(&ctx),
            &mut HashMap::new(),
            &mut test_relay_bq(),
            "test",
        )
        .await;
        assert!(
            by_node.is_empty(),
            "an endpoints-union (relay-tainted) address must not become a LAN carrier"
        );

        // The SAME address in the pure `lan_endpoints` bucket → LAN carrier.
        let mut lan_peer = peer(&peer_kp, "100.64.0.7");
        lan_peer.node_id = tainted.node_id;
        lan_peer.lan_endpoints = vec!["10.1.2.3:1000".into()];
        rt.install_peers(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            std::slice::from_ref(&lan_peer),
            Some(&ctx),
            &mut HashMap::new(),
            &mut test_relay_bq(),
            "test",
        )
        .await;
        let inst = by_node
            .get(&lan_peer.node_id)
            .expect("the pure-bucket LAN candidate installs a LAN carrier");
        assert_eq!(inst.tier, DirectTier::Lan);
        assert!(inst.is_direct);
    }

    /// P9 — a FRESH peer with a same-/24 candidate must NOT install the
    /// unproven LAN carrier destructively when a fallback tier exists: a
    /// false-subnet match (vendor-default /24s collide across sites) otherwise
    /// burns the whole LAN handshake deadline with NO carrier at all, on every
    /// retry (field 2026-07-28). The LAN candidate becomes a shadow PROBE and
    /// the working carrier comes from the fallback walk (here: srflx) in the
    /// SAME pass. The airgap case (nothing else dialable, `relay = None`) in
    /// `lan_tier_scans_only_the_pure_lan_endpoint_bucket` keeps the
    /// destructive install.
    #[tokio::test(flavor = "multi_thread")]
    async fn fresh_lan_probes_first_when_fallback_exists() {
        let kp = WgKeypair::generate();
        let peer_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let (tun_mock, _inj, _del) = MockTun::new();
        let tf: TunFactory = {
            let m = tun_mock.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);
        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let tun: Arc<dyn TunIo> = tun_mock;

        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let punch_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let my_ip: Ipv4Addr = "10.1.2.9".parse().unwrap();
        let ctx = DirectCtx {
            socks: vec![(my_ip, sock)],
            my_ips: vec![my_ip],
            endpoints: vec!["10.1.2.9:41000".into()],
            public_sock: None,
            punch: Some(("93.184.216.90:5555".into(), punch_sock)),
            my_nat: None,
        };
        let mut relay: Option<RelayCoordinator> = None;
        let mut by_node = HashMap::new();
        let mut upgrade_probes = HashMap::new();

        // Same-/24 LAN candidate (possibly a false match) + a srflx fallback.
        let mut p = peer(&peer_kp, "100.64.0.7");
        p.lan_endpoints = vec!["10.1.2.3:1000".into()];
        p.srflx_endpoints = vec!["93.184.216.34:4444".into()];
        rt.install_peers(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            std::slice::from_ref(&p),
            Some(&ctx),
            &mut upgrade_probes,
            &mut test_relay_bq(),
            "test",
        )
        .await;

        let probe = upgrade_probes
            .get(&p.node_id)
            .expect("the LAN candidate is a shadow probe, not the carrier");
        assert_eq!(probe.tier, DirectTier::Lan);
        assert_eq!(probe.dst.to_string(), "10.1.2.3:1000");
        assert_eq!(wg.probe_count(), 1, "one shadow probe in flight");
        let inst = by_node
            .get(&p.node_id)
            .expect("the working carrier comes from the fallback walk");
        assert_eq!(
            inst.tier,
            DirectTier::Srflx,
            "srflx fallback carries while the LAN probe runs"
        );
        assert!(inst.is_direct);

        // A second pass with the probe in flight must not duplicate it.
        rt.install_peers(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            std::slice::from_ref(&p),
            Some(&ctx),
            &mut upgrade_probes,
            &mut test_relay_bq(),
            "test",
        )
        .await;
        assert_eq!(wg.probe_count(), 1, "no duplicate probe on re-entry");
    }

    /// P9 — a peer the server marked `reachable = false` (ghost enrollment /
    /// stale heartbeat / clean leave) is never dialed: no carrier install, no
    /// shadow probe, no relay request — even with perfectly dialable
    /// candidates on the row.
    #[tokio::test(flavor = "multi_thread")]
    async fn unreachable_peer_is_never_dialed() {
        let kp = WgKeypair::generate();
        let peer_kp = WgKeypair::generate();
        let (out_tx, _out_rx) = mpsc::channel::<ClientMsg>(16);
        let (tun_mock, _inj, _del) = MockTun::new();
        let tf: TunFactory = {
            let m = tun_mock.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt = OverlayRuntime::new_relay(kp.clone(), out_tx, tf, 1280);
        let (mut wg, _tun_rx) = WgDevice::new(kp.secret.clone());
        let tun: Arc<dyn TunIo> = tun_mock;

        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let my_ip: Ipv4Addr = "10.1.2.9".parse().unwrap();
        let ctx = DirectCtx {
            socks: vec![(my_ip, sock)],
            my_ips: vec![my_ip],
            endpoints: vec!["10.1.2.9:41000".into()],
            public_sock: None,
            punch: None,
            my_nat: None,
        };
        let mut relay: Option<RelayCoordinator> = None;
        let mut by_node = HashMap::new();
        let mut upgrade_probes = HashMap::new();

        let mut p = peer(&peer_kp, "100.64.0.9");
        p.lan_endpoints = vec!["10.1.2.3:1000".into()];
        p.reachable = false;
        rt.install_peers(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            std::slice::from_ref(&p),
            Some(&ctx),
            &mut upgrade_probes,
            &mut test_relay_bq(),
            "test",
        )
        .await;

        assert!(by_node.is_empty(), "no carrier for an unreachable peer");
        assert!(upgrade_probes.is_empty(), "no shadow probe either");
        assert_eq!(wg.probe_count(), 0);
    }

    /// Minimal STUN Binding Success carrying an XOR-MAPPED-ADDRESS (IPv4), so a
    /// keepalive test needs no real STUN server (RFC 5389 §15.2).
    fn stun_success(txn: [u8; 12], ip: [u8; 4], port: u16) -> Vec<u8> {
        const COOKIE: u32 = 0x2112_A442;
        let cookie = COOKIE.to_be_bytes();
        let xport = port ^ ((COOKIE >> 16) as u16);
        let mut r = Vec::new();
        r.extend_from_slice(&0x0101u16.to_be_bytes()); // Binding Success
        r.extend_from_slice(&12u16.to_be_bytes()); // one 12-byte attribute
        r.extend_from_slice(&cookie);
        r.extend_from_slice(&txn);
        r.extend_from_slice(&0x0020u16.to_be_bytes()); // XOR-MAPPED-ADDRESS
        r.extend_from_slice(&8u16.to_be_bytes());
        r.push(0);
        r.push(0x01); // family IPv4
        r.extend_from_slice(&xport.to_be_bytes());
        r.extend_from_slice(&[
            ip[0] ^ cookie[0],
            ip[1] ^ cookie[1],
            ip[2] ^ cookie[2],
            ip[3] ^ cookie[3],
        ]);
        r
    }

    /// Phase C (D5) — the srflx keepalive re-advertises EXACTLY when the punch
    /// mapping changes, and never on a query returning the same mapping. A demux
    /// emulator feeds STUN replies into the sink as the real demux loop would.
    #[tokio::test(flavor = "multi_thread")]
    async fn srflx_keepalive_retrickles_only_on_mapping_change() {
        let punch = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let server = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let server_addr = server.local_addr().unwrap();
        let (sink_tx, sink_rx) = mpsc::channel::<crate::transport::stun::StunInbound>(16);

        // Reply to the FIRST query with the initial advert (no change), and to
        // every later query with a CHANGED mapping.
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let mut seen = 0u32;
            loop {
                let Ok((_n, _from)) = server.recv_from(&mut buf).await else {
                    break;
                };
                let txn: [u8; 12] = buf[8..20].try_into().unwrap();
                let port = if seen == 0 { 1111 } else { 2222 };
                seen += 1;
                let _ = sink_tx
                    .send(crate::transport::stun::StunInbound {
                        src: server_addr,
                        packet: stun_success(txn, [203, 0, 113, 7], port),
                    })
                    .await;
            }
        });

        let (out_tx, mut out_rx) = mpsc::channel::<ClientMsg>(16);
        let advertised = vec!["203.0.113.7:1111".to_string()];
        let task = tokio::spawn(run_srflx_keepalive(
            punch,
            sink_rx,
            server_addr,
            vec![],
            vec![], // own_ips (co-located-worker exclusion — none in this test)
            advertised,
            Some("cone".into()),
            out_tx,
            Duration::from_millis(60),
        ));

        // First tick: mapping == advert → NO trickle. Second tick: changed to
        // :2222 → exactly one trickle with the new punch candidate at [0].
        let msg = tokio::time::timeout(Duration::from_secs(3), out_rx.recv())
            .await
            .expect("expected a re-trickle")
            .expect("channel closed");
        match msg {
            ClientMsg::OverlaySrflx { candidates, nat } => {
                assert_eq!(candidates, vec!["203.0.113.7:2222".to_string()]);
                // The NAT type rides every re-trickle (mapping changed, class
                // didn't) so the server never clears it.
                assert_eq!(nat.as_deref(), Some("cone"));
            }
            other => panic!("expected OverlaySrflx, got {other:?}"),
        }
        // No further trickle while the mapping stays :2222.
        assert!(
            tokio::time::timeout(Duration::from_millis(400), out_rx.recv())
                .await
                .is_err(),
            "must not re-trickle when the mapping is unchanged"
        );
        task.abort();
    }

    /// Phase C (D5) — a STUN outage must NOT strip a working advert: with no
    /// reply arriving, the keepalive retains the last-known srflx (no trickle).
    #[tokio::test(flavor = "multi_thread")]
    async fn srflx_keepalive_retains_advert_on_outage() {
        let punch = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        // Hold the sender so the channel stays open; never feed it (outage).
        let (_sink_tx, sink_rx) = mpsc::channel::<crate::transport::stun::StunInbound>(1);
        let (out_tx, mut out_rx) = mpsc::channel::<ClientMsg>(4);
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let task = tokio::spawn(run_srflx_keepalive(
            punch,
            sink_rx,
            dead,
            vec![],
            vec![], // own_ips
            vec!["203.0.113.7:1111".to_string()],
            Some("cone".into()),
            out_tx,
            Duration::from_millis(30),
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(500), out_rx.recv())
                .await
                .is_err(),
            "a STUN outage must not produce a re-trickle"
        );
        task.abort();
    }

    #[test]
    fn overlay_view_classifies_connection_types_and_sorts() {
        // Locks the LocalAPI connection-type mapping (the Tailscale-style
        // per-device "how am I reaching it" column): installed-direct → Direct,
        // installed-relay → Relay, reachable-but-no-carrier → Blocked,
        // not-reachable → Offline. And the peer list is node_id-sorted so a
        // LocalAPI reader doesn't see it jitter.
        fn oid(b: u8) -> ObjectId {
            ObjectId::from_bytes([b, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        }
        fn np(id: ObjectId, name: &str, ip: &str, reachable: bool) -> NetmapPeer {
            NetmapPeer {
                node_id: id,
                overlay_ip: ip.into(),
                name: name.into(),
                wg_public_key: String::new(),
                endpoints: vec![],
                lan_endpoints: vec![],
                srflx_endpoints: vec![],
                srflx_nat: None,
                relay_home: None,
                reachable,
                supports_quic: false,
                supports_relay_single: false,
                supports_derp: false,
                routes: vec![],
                agent_id: None,
            }
        }
        fn installed(
            is_direct: bool,
            ip: Ipv4Addr,
            last_rx_at: Instant,
            relay: Option<(std::net::SocketAddr, std::net::SocketAddr)>,
        ) -> Installed {
            Installed {
                last_rx_at,
                relay_local: relay.map(|(l, _)| l),
                relay_dst: relay.map(|(_, d)| d),
                ..Installed::base(
                    [0u8; 32],
                    ip,
                    if is_direct {
                        DirectTier::Lan
                    } else {
                        DirectTier::Relay
                    },
                    Instant::now(),
                )
            }
        }

        // Fixed clock basis so the epoch-ms conversion is deterministic. Both
        // the peers' `last_rx_at` and the view's `now` derive from this `now`.
        let now = Instant::now();
        let epoch_now_ms: u64 = 1_000_000_000_000;
        let (d, r, b, o) = (oid(0x01), oid(0x02), oid(0x03), oid(0x04));
        let mut by_node = HashMap::new();
        // Direct peer last received a packet 10 s ago; relay peer just now.
        by_node.insert(
            d,
            installed(
                true,
                Ipv4Addr::new(100, 64, 0, 1),
                now.checked_sub(std::time::Duration::from_secs(10)).unwrap(),
                None,
            ),
        );
        by_node.insert(
            r,
            installed(
                false,
                Ipv4Addr::new(100, 64, 0, 2),
                now,
                Some((
                    "94.130.141.74:10850".parse().unwrap(),
                    "5.9.157.226:12728".parse().unwrap(),
                )),
            ),
        );

        let mut current = HashMap::new();
        current.insert(d, np(d, "direct-peer", "100.64.0.1", true));
        current.insert(r, np(r, "relay-peer", "100.64.0.2", true));
        current.insert(b, np(b, "pending-peer", "100.64.0.3", true)); // no carrier
        current.insert(o, np(o, "offline-peer", "100.64.0.4", false));

        // P8-cosmetics — an in-flight MBB probe marks the RELAY peer (and only
        // it) as `upgrading`; a probe entry for the DIRECT peer is ignored
        // (the marker is a relay-tier transition signal).
        let mut probes: HashMap<ObjectId, UpgradeProbe> = HashMap::new();
        let dummy_probe = || UpgradeProbe {
            pubkey: [0u8; 32],
            overlay_ip: Ipv4Addr::new(100, 64, 0, 2),
            dst: "192.168.68.1:51820".parse().unwrap(),
            tier: DirectTier::Lan,
            since: now,
            initiated: true,
            local: None,
        };
        probes.insert(r, dummy_probe());
        probes.insert(d, dummy_probe());

        let view = build_overlay_view("100.64.0.9", &by_node, &current, &probes, now, epoch_now_ms);
        assert_eq!(view.self_ip.as_deref(), Some("100.64.0.9"));
        assert_eq!(view.peers.len(), 4);
        assert!(
            view.peers[1].upgrading,
            "relay peer with a probe ⇒ upgrading"
        );
        assert!(
            !view.peers[0].upgrading,
            "direct peer never shows upgrading"
        );
        assert!(!view.peers[2].upgrading && !view.peers[3].upgrading);
        // Sorted by node_id hex → 01,02,03,04.
        assert_eq!(view.peers[0].connection, ConnectionType::Direct);
        assert_eq!(view.peers[0].name, "direct-peer");
        assert_eq!(view.peers[0].overlay_ip.as_deref(), Some("100.64.0.1"));
        assert!(view.peers[0].online);
        assert_eq!(view.peers[1].connection, ConnectionType::Relay);
        assert_eq!(view.peers[2].connection, ConnectionType::Blocked);
        assert!(
            view.peers[2].online,
            "blocked peer is still server-reachable"
        );
        assert_eq!(view.peers[3].connection, ConnectionType::Offline);
        assert!(!view.peers[3].online);
        // RTT isn't tracked by the runtime (the daemon's prober fills it).
        assert!(view.peers[0].rtt_ms.is_none());
        // last_seen_ms is absolute epoch-ms of the last inbound packet: the
        // direct peer 10 s ago, the relay peer ~now; carrier-less peers None.
        assert_eq!(view.peers[0].last_seen_ms, Some(epoch_now_ms - 10_000));
        assert_eq!(view.peers[1].last_seen_ms, Some(epoch_now_ms));
        assert!(view.peers[2].last_seen_ms.is_none());
        assert!(view.peers[3].last_seen_ms.is_none());
        // rc.187 — relay endpoints surface only for the relay peer (local=mars,
        // dst=zeus ⇒ the cross-worker signal an operator reads from `peers`);
        // direct + carrier-less peers carry none.
        assert_eq!(
            view.peers[1].relay_local.as_deref(),
            Some("94.130.141.74:10850")
        );
        assert_eq!(
            view.peers[1].relay_dst.as_deref(),
            Some("5.9.157.226:12728")
        );
        assert!(view.peers[0].relay_local.is_none() && view.peers[0].relay_dst.is_none());
        assert!(view.peers[2].relay_dst.is_none());
        assert!(view.peers[3].relay_dst.is_none());
        // rc.276 — the carrier debug snapshot: present for installed peers
        // (tier + rx-age mapped through), absent for carrier-less ones.
        let d0 = view.peers[0].debug.as_ref().expect("direct peer has debug");
        assert_eq!(d0.tier, "lan");
        assert_eq!(d0.last_rx_age_s, 10);
        assert!(!d0.initiated && !d0.hs_done, "fixture defaults map through");
        assert!(d0.relay_kind.is_none());
        let d1 = view.peers[1].debug.as_ref().expect("relay peer has debug");
        assert_eq!(d1.tier, "relay");
        assert_eq!(d1.last_rx_age_s, 0);
        assert!(view.peers[2].debug.is_none() && view.peers[3].debug.is_none());
    }

    struct MockTun {
        inject: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
        delivered: mpsc::UnboundedSender<Vec<u8>>,
    }
    impl MockTun {
        fn new() -> (
            Arc<Self>,
            mpsc::UnboundedSender<Vec<u8>>,
            mpsc::UnboundedReceiver<Vec<u8>>,
        ) {
            let (i_tx, i_rx) = mpsc::unbounded_channel();
            let (d_tx, d_rx) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    inject: Mutex::new(i_rx),
                    delivered: d_tx,
                }),
                i_tx,
                d_rx,
            )
        }
    }
    #[async_trait]
    impl TunIo for MockTun {
        async fn read_packet(&self) -> io::Result<Vec<u8>> {
            self.inject
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| io::Error::other("mock inject closed"))
        }
        async fn write_packet(&self, packet: &[u8]) -> io::Result<()> {
            self.delivered
                .send(packet.to_vec())
                .map_err(|_| io::Error::other("mock delivered closed"))
        }
    }

    /// A factory that always hands back a fixed loopback carrier (one
    /// peer per node in the test).
    struct LoopbackLinks {
        sock: Arc<UdpSocket>,
        dst: SocketAddr,
    }
    #[async_trait]
    impl LinkFactory for LoopbackLinks {
        async fn build_carrier(&self, _peer: &PeerConfig) -> Option<Arc<Carrier>> {
            Some(Carrier::direct(self.sock.clone(), self.dst))
        }
    }

    fn synthetic_ipv4(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
        let total = 20 + payload.len();
        let mut p = vec![0u8; total];
        p[0] = 0x45;
        p[2] = (total >> 8) as u8;
        p[3] = (total & 0xff) as u8;
        p[8] = 64;
        p[9] = 17;
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        p[20..].copy_from_slice(payload);
        p
    }

    fn net() -> OverlayNetworkInfo {
        OverlayNetworkInfo {
            cidr: "100.64.0.0/10".into(),
            mtu: 1280,
            magic_domain: None,
            nameservers: vec![],
            stun_urls: vec![],
        }
    }
    fn peer(kp: &WgKeypair, ip: &str) -> NetmapPeer {
        NetmapPeer {
            node_id: ObjectId::new(),
            overlay_ip: ip.into(),
            name: String::new(),
            wg_public_key: kp.public_base64(),
            endpoints: vec![],
            lan_endpoints: vec![],
            srflx_endpoints: vec![],
            srflx_nat: None,
            relay_home: None,
            reachable: true,
            supports_quic: false,
            supports_relay_single: false,
            supports_derp: false,
            routes: vec![],
            agent_id: None,
        }
    }

    const IP_A: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 1);
    const IP_B: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 2);

    #[tokio::test(flavor = "multi_thread")]
    async fn runtime_installs_peer_from_netmap_and_round_trips() {
        let a = WgKeypair::generate();
        let b = WgKeypair::generate();

        let sock_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let sock_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = sock_a.local_addr().unwrap();
        let addr_b = sock_b.local_addr().unwrap();

        let (out_a, mut out_a_rx) = mpsc::channel::<ClientMsg>(16);
        let (out_b, mut out_b_rx) = mpsc::channel::<ClientMsg>(16);
        let (evt_a, evt_a_rx) = mpsc::channel::<OverlayEvent>(16);
        let (evt_b, evt_b_rx) = mpsc::channel::<OverlayEvent>(16);

        let (mock_a, inject_a, _del_a) = MockTun::new();
        let (mock_b, _inj_b, mut del_b) = MockTun::new();
        let tf_a: TunFactory = {
            let m = mock_a.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let tf_b: TunFactory = {
            let m = mock_b.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };

        let rt_a = OverlayRuntime::new(
            a.clone(),
            out_a,
            Arc::new(LoopbackLinks {
                sock: sock_a,
                dst: addr_b,
            }),
            tf_a,
            1280,
        );
        let rt_b = OverlayRuntime::new(
            b.clone(),
            out_b,
            Arc::new(LoopbackLinks {
                sock: sock_b,
                dst: addr_a,
            }),
            tf_b,
            1280,
        );
        tokio::spawn(rt_a.run(evt_a_rx, vec![]));
        tokio::spawn(rt_b.run(evt_b_rx, vec![]));

        // Both runtimes announce themselves first.
        assert!(matches!(
            out_a_rx.recv().await,
            Some(ClientMsg::OverlayJoin { .. })
        ));
        assert!(matches!(
            out_b_rx.recv().await,
            Some(ClientMsg::OverlayJoin { .. })
        ));

        // Server pushes each its netmap (the other node as the one peer).
        evt_a
            .send(OverlayEvent::Netmap {
                self_ip: "100.64.0.1".into(),
                network: net(),
                peers: vec![peer(&b, "100.64.0.2")],
            })
            .await
            .unwrap();
        evt_b
            .send(OverlayEvent::Netmap {
                self_ip: "100.64.0.2".into(),
                network: net(),
                peers: vec![peer(&a, "100.64.0.1")],
            })
            .await
            .unwrap();

        // App on A sends to B's overlay IP; assert it arrives on B's TUN.
        // Re-inject (best-effort send drops until the WG session is up).
        let pkt = synthetic_ipv4(IP_A, IP_B, b"runtime-loopback");
        for _ in 0..100 {
            let _ = inject_a.send(pkt.clone());
            if let Ok(Some(got)) =
                tokio::time::timeout(Duration::from_millis(150), del_b.recv()).await
            {
                assert_eq!(got, pkt, "packet must traverse the overlay runtime intact");
                return;
            }
        }
        panic!("packet did not traverse the runtime in time");
    }

    /// P1 (S6) — the permanent RE-COUPLING TRIPWIRE. A fat control arm (a
    /// 2 s Netmap handler, via [`TEST_NETMAP_STALL_MS`]) must not delay
    /// outbound packets: the data plane runs on the dedicated pump, off the
    /// select! loop. Pre-P1 this fails by construction — the outbound arm
    /// shared the loop, so every packet injected during the stall queued for
    /// its full duration (one >2 s inter-arrival gap at the receiver);
    /// post-P1 the MAX GAP stays bounded. (A delivered-count assertion can't
    /// discriminate: the mpsc buffers the burst either way — the gap is the
    /// signal.)
    #[tokio::test(flavor = "multi_thread")]
    async fn control_stall_does_not_delay_outbound() {
        let a = WgKeypair::generate();
        let b = WgKeypair::generate();

        let sock_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let sock_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = sock_a.local_addr().unwrap();
        let addr_b = sock_b.local_addr().unwrap();

        let (out_a, mut out_a_rx) = mpsc::channel::<ClientMsg>(16);
        let (out_b, mut out_b_rx) = mpsc::channel::<ClientMsg>(16);
        let (evt_a, evt_a_rx) = mpsc::channel::<OverlayEvent>(16);
        let (evt_b, evt_b_rx) = mpsc::channel::<OverlayEvent>(16);

        let (mock_a, inject_a, _del_a) = MockTun::new();
        let (mock_b, _inj_b, mut del_b) = MockTun::new();
        let tf_a: TunFactory = {
            let m = mock_a.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let tf_b: TunFactory = {
            let m = mock_b.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };

        let rt_a = OverlayRuntime::new(
            a.clone(),
            out_a,
            Arc::new(LoopbackLinks {
                sock: sock_a,
                dst: addr_b,
            }),
            tf_a,
            1280,
        );
        let rt_b = OverlayRuntime::new(
            b.clone(),
            out_b,
            Arc::new(LoopbackLinks {
                sock: sock_b,
                dst: addr_a,
            }),
            tf_b,
            1280,
        );
        tokio::spawn(rt_a.run(evt_a_rx, vec![]));
        tokio::spawn(rt_b.run(evt_b_rx, vec![]));
        assert!(matches!(
            out_a_rx.recv().await,
            Some(ClientMsg::OverlayJoin { .. })
        ));
        assert!(matches!(
            out_b_rx.recv().await,
            Some(ClientMsg::OverlayJoin { .. })
        ));

        let netmap_a = OverlayEvent::Netmap {
            self_ip: "100.64.0.1".into(),
            network: net(),
            peers: vec![peer(&b, "100.64.0.2")],
        };
        evt_a.send(netmap_a).await.unwrap();
        evt_b
            .send(OverlayEvent::Netmap {
                self_ip: "100.64.0.2".into(),
                network: net(),
                peers: vec![peer(&a, "100.64.0.1")],
            })
            .await
            .unwrap();

        // Establish the session (first round-trip), then drain any handshake-
        // era straggler deliveries so the measurement starts clean.
        let pkt = synthetic_ipv4(IP_A, IP_B, b"stall-probe");
        let mut established = false;
        for _ in 0..100 {
            let _ = inject_a.send(pkt.clone());
            if tokio::time::timeout(Duration::from_millis(150), del_b.recv())
                .await
                .ok()
                .flatten()
                .is_some()
            {
                established = true;
                break;
            }
        }
        assert!(established, "session did not establish in time");
        while tokio::time::timeout(Duration::from_millis(300), del_b.recv())
            .await
            .ok()
            .flatten()
            .is_some()
        {}

        // Arm the 2 s stall and hand A a DUPLICATE netmap (same peer ⇒ the
        // already-installed carrier is kept — the arm just sits busy).
        TEST_NETMAP_STALL_MS.store(2000, std::sync::atomic::Ordering::Relaxed);
        evt_a
            .send(OverlayEvent::Netmap {
                self_ip: "100.64.0.1".into(),
                network: net(),
                peers: vec![peer(&b, "100.64.0.2")],
            })
            .await
            .unwrap();
        // Let the loop pick the netmap arm up before measuring.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Collect delivery instants at B while injecting 40 packets at 50 ms
        // across the stall window.
        let collector = tokio::spawn(async move {
            let mut arrivals: Vec<Instant> = Vec::new();
            while arrivals.len() < 40 {
                match tokio::time::timeout(Duration::from_secs(1), del_b.recv()).await {
                    Ok(Some(_)) => arrivals.push(Instant::now()),
                    _ => break,
                }
            }
            arrivals
        });
        for _ in 0..40 {
            let _ = inject_a.send(pkt.clone());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let arrivals = collector.await.unwrap();
        TEST_NETMAP_STALL_MS.store(0, std::sync::atomic::Ordering::Relaxed);

        assert!(
            arrivals.len() >= 35,
            "expected ≥35/40 deliveries over an established loopback session, got {}",
            arrivals.len()
        );
        let max_gap = arrivals
            .windows(2)
            .map(|w| w[1].duration_since(w[0]))
            .max()
            .unwrap_or_default();
        assert!(
            max_gap < Duration::from_millis(500),
            "outbound delayed by a control-arm stall: max inter-arrival gap {max_gap:?} \
             (pre-P1 the 2 s Netmap stall produces a >2 s gap here)"
        );
    }

    /// P1 (S6) — pump death is session-fatal, never respawned: when the TUN
    /// dies (inject sender dropped ⇒ `read_packet` errors ⇒ reader exits ⇒
    /// the pump's channel closes ⇒ the pump ends), the runtime loop must
    /// exit — same disposition the pre-P1 outbound arm had on reader death.
    #[tokio::test(flavor = "multi_thread")]
    async fn runtime_exits_when_outbound_pump_dies() {
        let a = WgKeypair::generate();
        let sock_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let dst = sock_a.local_addr().unwrap();

        let (out_a, mut out_a_rx) = mpsc::channel::<ClientMsg>(16);
        let (evt_a, evt_a_rx) = mpsc::channel::<OverlayEvent>(16);
        let (mock_a, inject_a, _del_a) = MockTun::new();
        let tf_a: TunFactory = {
            let m = mock_a.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let rt_a = OverlayRuntime::new(
            a.clone(),
            out_a,
            Arc::new(LoopbackLinks { sock: sock_a, dst }),
            tf_a,
            1280,
        );
        let run = tokio::spawn(rt_a.run(evt_a_rx, vec![]));
        assert!(matches!(
            out_a_rx.recv().await,
            Some(ClientMsg::OverlayJoin { .. })
        ));
        // Reach steady state (TUN up, reader + pump spawned).
        evt_a
            .send(OverlayEvent::Netmap {
                self_ip: "100.64.0.1".into(),
                network: net(),
                peers: vec![],
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Kill the TUN: reader errors out → pump ends → loop breaks.
        drop(inject_a);
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("runtime must exit once the outbound pump dies")
            .unwrap();
    }

    // ---- P5 exit-node pure helpers ----

    fn exit_oid(b: u8) -> ObjectId {
        ObjectId::from_bytes([b, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
    }

    fn exit_np(id: ObjectId, name: &str, routes: Vec<String>) -> NetmapPeer {
        NetmapPeer {
            node_id: id,
            overlay_ip: "100.64.0.1".into(),
            name: name.into(),
            wg_public_key: String::new(),
            endpoints: vec![],
            lan_endpoints: vec![],
            srflx_endpoints: vec![],
            srflx_nat: None,
            relay_home: None,
            reachable: true,
            supports_quic: false,
            supports_relay_single: false,
            supports_derp: false,
            routes,
            agent_id: None,
        }
    }

    #[test]
    fn resolve_exit_peer_matches_name_or_hex() {
        let a = exit_oid(0x0a);
        let b = exit_oid(0x0b);
        let mut peers = HashMap::new();
        peers.insert(a, exit_np(a, "jupiter", vec![]));
        peers.insert(b, exit_np(b, "zeus", vec![]));
        // By name.
        assert_eq!(resolve_exit_peer("jupiter", &peers), Some(a));
        assert_eq!(resolve_exit_peer("zeus", &peers), Some(b));
        // By node-id hex.
        assert_eq!(resolve_exit_peer(&a.to_hex(), &peers), Some(a));
        // Surrounding whitespace is tolerated.
        assert_eq!(resolve_exit_peer("  jupiter  ", &peers), Some(a));
        // Unknown selector → None (reconcile defers rather than blackholing).
        assert_eq!(resolve_exit_peer("mars", &peers), None);
    }

    /// Names are recycled between machines now, so a NAME selector that starts
    /// resolving elsewhere must not silently move this host's whole egress. A
    /// hex selector can't drift and a first resolution isn't drift.
    #[test]
    fn exit_selector_drifted_only_fires_for_a_name_that_moved() {
        let a = exit_oid(0x31);
        let b = exit_oid(0x32);
        assert!(
            !exit_selector_drifted(None, "jupiter", a),
            "first resolution is not drift"
        );
        assert!(
            !exit_selector_drifted(Some(a), "jupiter", a),
            "same machine is not drift"
        );
        assert!(
            exit_selector_drifted(Some(a), "jupiter", b),
            "the name moved to another machine"
        );
        assert!(
            !exit_selector_drifted(Some(a), &b.to_hex(), b),
            "a hex selector is unambiguous — never drift"
        );
    }

    #[test]
    fn peer_is_approved_exit_detects_default_route() {
        // An admin-approved exit node carries 0.0.0.0/0 in its netmap routes.
        assert!(peer_is_approved_exit(&exit_np(
            exit_oid(1),
            "x",
            vec!["0.0.0.0/0".into()]
        )));
        // Exit node that is ALSO a subnet router.
        assert!(peer_is_approved_exit(&exit_np(
            exit_oid(1),
            "x",
            vec!["192.168.1.0/24".into(), "0.0.0.0/0".into()]
        )));
        // A plain subnet router is NOT an exit node.
        assert!(!peer_is_approved_exit(&exit_np(
            exit_oid(1),
            "x",
            vec!["192.168.1.0/24".into()]
        )));
        assert!(!peer_is_approved_exit(&exit_np(exit_oid(1), "x", vec![])));
    }

    #[test]
    fn exit_exemption_set_unions_server_and_relay_workers() {
        fn inst(is_direct: bool, relay: Option<(SocketAddr, SocketAddr)>) -> Installed {
            Installed {
                relay_local: relay.map(|(l, _)| l),
                relay_dst: relay.map(|(_, d)| d),
                ..Installed::base(
                    [0u8; 32],
                    Ipv4Addr::new(100, 64, 0, 1),
                    if is_direct {
                        DirectTier::Lan
                    } else {
                        DirectTier::Relay
                    },
                    Instant::now(),
                )
            }
        }
        // Server A + AAAA (S3b — the v6 AAAA rides the set too; reconcile
        // partitions by family, and the v6 exemption keeps the WS-over-v6 direct).
        let server: Vec<IpAddr> = vec![
            "94.130.141.98".parse().unwrap(),
            "94.130.141.99".parse().unwrap(),
            "2a01:4f8:c17:b8f::2".parse().unwrap(),
        ];
        let mut by_node = HashMap::new();
        // A relay carrier → BOTH its coturn worker IPs are exempted.
        by_node.insert(
            exit_oid(1),
            inst(
                false,
                Some((
                    "94.130.141.74:10850".parse().unwrap(),
                    "5.9.157.226:12728".parse().unwrap(),
                )),
            ),
        );
        // A direct carrier → contributes NO exemption (same-subnet / on-link).
        by_node.insert(exit_oid(2), inst(true, None));

        let set = exit_exemption_set(&server, &by_node);
        assert!(set.contains(&"94.130.141.98".parse::<IpAddr>().unwrap()));
        assert!(set.contains(&"94.130.141.99".parse::<IpAddr>().unwrap()));
        assert!(set.contains(&"2a01:4f8:c17:b8f::2".parse::<IpAddr>().unwrap())); // AAAA
        assert!(set.contains(&"94.130.141.74".parse::<IpAddr>().unwrap())); // relay_local
        assert!(set.contains(&"5.9.157.226".parse::<IpAddr>().unwrap())); // relay_dst
        // Exactly the 3 server (2×A + 1×AAAA) + 2 relay IPs; direct added nothing.
        assert_eq!(set.len(), 5);
    }

    /// Phase A never-self-wedge: a PUBLIC-DIRECT carrier's peer IP MUST be
    /// exempted (it's a real internet dst reached via the default route, unlike
    /// an on-link LAN peer), or the split-default `/1`s would swallow the path
    /// to the very exit that carries egress.
    #[test]
    fn exit_exemption_set_includes_public_direct_dst() {
        let pd: std::net::SocketAddr = "5.9.157.226:41234".parse().unwrap();
        let mut by_node = HashMap::new();
        by_node.insert(
            exit_oid(9),
            Installed {
                public_direct_dst: Some(pd),
                ..Installed::base(
                    [1u8; 32],
                    Ipv4Addr::new(100, 64, 0, 9),
                    DirectTier::Public,
                    Instant::now(),
                )
            },
        );
        let set = exit_exemption_set(&[], &by_node);
        assert!(
            set.contains(&pd.ip()),
            "a public-direct peer IP must be exempted from the split-default"
        );
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn exit_peer_allowed_ips_preserves_real_subnets_and_appends_split_default() {
        let kp = WgKeypair::generate();
        let exit = NetmapPeer {
            node_id: exit_oid(7),
            overlay_ip: "100.64.0.7".into(),
            name: "jupiter".into(),
            wg_public_key: kp.public_base64(),
            endpoints: vec![],
            lan_endpoints: vec![],
            srflx_endpoints: vec![],
            srflx_nat: None,
            relay_home: None,
            reachable: true,
            supports_quic: false,
            supports_relay_single: false,
            supports_derp: false,
            routes: vec!["192.168.5.0/24".into(), "0.0.0.0/0".into()],
            agent_id: None,
        };
        let strs: Vec<String> = exit_peer_allowed_ips(&exit)
            .iter()
            .map(|c| c.to_string())
            .collect();
        // Real subnet preserved; both /1 halves appended; the bare /0 dropped.
        assert!(strs.contains(&"192.168.5.0/24".to_string()));
        assert!(strs.contains(&"0.0.0.0/1".to_string()));
        assert!(strs.contains(&"128.0.0.0/1".to_string()));
        assert!(!strs.contains(&"0.0.0.0/0".to_string()));
        assert_eq!(strs.len(), 3);
    }

    #[test]
    fn exit_readiness_reports_distinct_split_tunnel_reasons() {
        let id = exit_oid(0x21);
        let no_carriers: HashMap<ObjectId, Installed> = HashMap::new();

        // Not in the mesh at all.
        let empty: HashMap<ObjectId, NetmapPeer> = HashMap::new();
        assert_eq!(
            exit_readiness("jupiter", &empty, &no_carriers).unwrap_err(),
            "exit node not visible in the mesh yet"
        );

        // Present, but not an admin-approved exit node (no /0 in its routes).
        let mut subnet_only = HashMap::new();
        subnet_only.insert(id, exit_np(id, "jupiter", vec!["192.168.1.0/24".into()]));
        assert_eq!(
            exit_readiness("jupiter", &subnet_only, &no_carriers).unwrap_err(),
            "not an admin-approved exit node (no 0.0.0.0/0 approved)"
        );

        // Approved, but no live carrier yet.
        let mut approved = HashMap::new();
        approved.insert(id, exit_np(id, "jupiter", vec!["0.0.0.0/0".into()]));
        assert_eq!(
            exit_readiness("jupiter", &approved, &no_carriers).unwrap_err(),
            "exit node has no live carrier yet"
        );

        // Approved + carriered → ready, yields the peer's pubkey.
        let mut carriered = HashMap::new();
        carriered.insert(
            id,
            Installed::base(
                [7u8; 32],
                Ipv4Addr::new(100, 64, 0, 1),
                DirectTier::Lan,
                Instant::now(),
            ),
        );
        let (rid, _np, pk) = exit_readiness("jupiter", &approved, &carriered).unwrap();
        assert_eq!(rid, id);
        assert_eq!(pk, [7u8; 32]);
    }

    #[test]
    fn exit_node_status_reflects_active_and_withheld() {
        let mut st = ExitRoutingState::default();
        // Not configured (no selector) → no status at all.
        assert!(exit_node_status(None, &st).is_none());
        // Configured + withheld surfaces the reason.
        st.withheld_reason = Some("exit node has no live carrier yet".into());
        let w = exit_node_status(Some("jupiter"), &st).unwrap();
        assert_eq!(w.selector, "jupiter");
        assert!(!w.active);
        assert_eq!(
            w.withheld_reason.as_deref(),
            Some("exit node has no live carrier yet")
        );
        // Withheld → v6 is never "on".
        assert!(!w.v6_active);
        // Active but v6 undecided/fail-closed → active, v6 off.
        st.split_default_installed = true;
        let a = exit_node_status(Some("jupiter"), &st).unwrap();
        assert!(a.active);
        assert!(a.withheld_reason.is_none());
        assert!(!a.v6_active);
        // Active AND v6 enabled → both on.
        st.v6_active = Some(true);
        let a6 = exit_node_status(Some("jupiter"), &st).unwrap();
        assert!(a6.active && a6.v6_active);
        // Active but v6 fail-closed → v4 on, v6 off.
        st.v6_active = Some(false);
        assert!(!exit_node_status(Some("jupiter"), &st).unwrap().v6_active);
        // S4b — DNS steered surfaces only while active.
        st.dns_steered = true;
        let d = exit_node_status(Some("jupiter"), &st).unwrap();
        assert!(d.active && d.dns_steered);
        // Not active → dns_steered is never reported true (masked like v6).
        st.split_default_installed = false;
        assert!(!exit_node_status(Some("jupiter"), &st).unwrap().dns_steered);
    }
}
