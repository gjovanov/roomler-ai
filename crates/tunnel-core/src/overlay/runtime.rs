//! Overlay node runtime (Phase 3b).
//!
//! Drives one node's membership in the overlay mesh: announces itself
//! (`rc:overlay.join`), applies the server's netmap (install / drop a
//! WireGuard peer per entry), brings up the TUN, and pumps packets
//! between the TUN and the [`WgDevice`](super::wg::WgDevice).
//!
//! The runtime **owns** the `WgDevice` and runs a single `select!` loop:
//! a TUN read (→ `send_ip_packet`) and a netmap event (→ `add_peer` /
//! `remove_peer`) never run concurrently, so the `&`/`&mut` borrows don't
//! collide and no interior mutability is needed. Only the inbound writer
//! (decrypted `tun_rx` → TUN) is a separate task — it never touches the
//! device.
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
use crate::localapi::{ConnectionType, ExitNodeStatus, OverlayView, PeerInfo};
use crate::transport::derp::DerpMux;
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer, NetmapPeer, OverlayNetworkInfo};

/// rc.131/132/143 — direct LAN carrier context: one UDP socket per LAN
/// interface (each bound to that interface IP — rc.143), this node's LAN IPs
/// across ALL interfaces (for the same-subnet test), and the `IP:port`
/// endpoints we advertise (one per interface socket) so a multi-homed peer can
/// reach us on whichever subnet it shares with us.
struct DirectCtx {
    /// One UDP socket bound to EACH usable LAN interface IP (rc.143 — NOT
    /// `0.0.0.0`). Binding to the specific address forces egress out that NIC,
    /// so a same-subnet peer is reached over the LAN even when a full-tunnel VPN
    /// has hijacked the default route (a `0.0.0.0` socket sent the reply out the
    /// VPN and the peer never got it). A peer is served by the socket whose
    /// interface IP shares its /24.
    socks: Vec<(Ipv4Addr, Arc<UdpSocket>)>,
    my_ips: Vec<Ipv4Addr>,
    endpoints: Vec<String>,
    /// Phase A (`public_direct_enabled`) — a single `0.0.0.0:0` socket used to
    /// DIAL a peer's public endpoint. Unbound to any interface so the OS routing
    /// table picks the egress NIC for each public destination (a per-interface
    /// socket bound to a private LAN IP would need us to know which NIC holds
    /// the default route on a multi-homed host). Its demux loop catches the
    /// exit's replies (keyed by the exit's public source). `None` when the
    /// public-direct tier is off. We do NOT advertise this socket's address (a
    /// peer reaches US on our per-interface PUBLIC socket, already advertised in
    /// `endpoints` since a public NIC IP passes `is_usable_lan_ipv4`).
    public_sock: Option<Arc<UdpSocket>>,
    /// Phase C — the interface socket that owns our FIRST advertised srflx
    /// candidate (`srflx_endpoints[0]`), paired with that candidate string. To
    /// hole-punch a NAT'd peer we must dial its srflx from THIS socket, so our
    /// outbound WG INITs ride the same NAT mapping we advertised (opening our
    /// filter toward the peer). Distinct from `public_sock` (the Phase A
    /// public-NIC dialer, an unbound `0.0.0.0` socket): a punch requires the
    /// mapping-owning socket, not an arbitrary egress one. `None` when the srflx
    /// tier is off or no public srflx was gathered. Set after the startup
    /// srflx gather (which returns each candidate with its socket).
    punch: Option<(String, Arc<UdpSocket>)>,
    /// Phase C — OUR probed NAT mapping type (`"cone"` / `"symmetric"`), or
    /// `None` when unknown. Set at the startup gather (probing the punch socket
    /// against two STUN targets). `install_peers` reads it to skip a srflx punch
    /// only when BOTH ends are symmetric.
    my_nat: Option<String>,
}

/// CC1 (NAT-traversal plan) — per-peer direct-carrier failure bookkeeping, kept
/// **split by tier** so a failing public-direct attempt can NEVER poison the
/// proven same-LAN direct path (or vice-versa). Each tier has its own retry
/// cooldown + consecutive-failure count; the escalation rule
/// ([`direct_retry_cooldown`]) is shared. The original code threaded two bare
/// maps; folding the second tier into a struct keeps the hot-path signatures
/// from growing another pair of args.
#[derive(Default)]
struct DirectCooldowns {
    /// Same-LAN direct tier (rc.136) — `until`-instant per peer.
    lan: HashMap<ObjectId, Instant>,
    /// Same-LAN direct consecutive-failure count (VPN-pool false /24 detector).
    lan_fails: HashMap<ObjectId, u32>,
    /// Public-direct tier (Phase A) — `until`-instant per peer. A firewalled /
    /// unreachable public endpoint escalates to the session-sticky deny exactly
    /// like the LAN tier, but on its OWN counter.
    public: HashMap<ObjectId, Instant>,
    /// Public-direct consecutive-failure count.
    public_fails: HashMap<ObjectId, u32>,
    /// Phase C — srflx hole-punch tier — `until`-instant per peer. Its OWN
    /// counter so a punch failure (routine for stricter NATs) never poisons the
    /// LAN or public-direct tiers (CC1). Escalates to a SHORTER deny than the
    /// LAN/public 24 h ([`SRFLX_DENY_COOLDOWN`], 15 min) — NAT conditions change
    /// when a host roams, so a permanent deny would wrongly outlive them.
    srflx: HashMap<ObjectId, Instant>,
    /// srflx consecutive-failure count.
    srflx_fails: HashMap<ObjectId, u32>,
}

impl DirectCooldowns {
    /// Is `nid` currently cooling down on the given tier?
    fn cooling(map: &HashMap<ObjectId, Instant>, nid: &ObjectId, now: Instant) -> bool {
        map.get(nid).is_some_and(|&until| until > now)
    }
}

/// Which carrier tier an installed peer is on. Direct tiers differ in cooldown
/// bookkeeping (CC1 — a failure on one tier must never poison another) and
/// each direct tier carries a WG-handshake completion deadline (a carrier that
/// never establishes is torn down to relay; relay itself is governed by its
/// own hard-dead/one-way signals).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DirectTier {
    /// Same-subnet LAN direct (rc.131-135) — on-link. rc.204: gets a TIGHT
    /// handshake deadline too — pre-handshake tx/rx stay flat, so without a
    /// deadline a false LAN match (stale endpoint, AP isolation, VPN-captured
    /// reply path) was a PERMANENT zombie with no relay fallback.
    Lan,
    /// Direct-to-public NIC (Phase A) — off-link; public cooldown + a loose
    /// handshake deadline (the accept side may lag).
    Public,
    /// srflx hole-punch (Phase C) — off-link; srflx cooldown + a tight
    /// handshake deadline (a cross-NAT punch works in a couple of INIT cycles
    /// or won't at all).
    Srflx,
    /// coturn relay carrier — not a direct tier; the tx/rx + hard-dead relay
    /// path governs it, so no handshake deadline here.
    Relay,
}

impl DirectTier {
    /// True for the direct tiers (everything but [`Relay`](Self::Relay)) — the
    /// carriers whose failure bookkeeping is keyed by tier.
    fn is_direct(self) -> bool {
        !matches!(self, DirectTier::Relay)
    }

    /// Phase C — the WG-handshake completion deadline past which a
    /// never-established carrier on this tier is torn down to relay.
    /// [`Duration::MAX`] for LAN/Relay (no deadline). Only consulted for the
    /// off-link `Public`/`Srflx` tiers.
    fn handshake_deadline(self) -> Duration {
        match self {
            DirectTier::Srflx => SRFLX_HANDSHAKE_DEADLINE,
            DirectTier::Public => PUBLIC_HANDSHAKE_DEADLINE,
            // rc.204 — LAN gets a deadline too (see the variant doc): on-link
            // handshakes complete in milliseconds or not at all.
            DirectTier::Lan => LAN_HANDSHAKE_DEADLINE,
            DirectTier::Relay => Duration::MAX,
        }
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
}

/// rc.208 — an in-flight make-before-break upgrade probe. The candidate direct
/// carrier lives as a shadow `Tunn` in [`WgDevice::probes`] (keyed by `pubkey`);
/// THIS is the runtime-side metadata the promote/expire sweep needs. While it is
/// present, `by_node[node]` still points at the peer's ACTIVE (relay) carrier —
/// routing is untouched — and [`OverlayRuntime::sweep_upgrade_probes`] either
/// promotes it (handshake latched → cut over to direct) or drops it (past the
/// tier's [`DirectTier::handshake_deadline`] → keep relay). See
/// [`super::direct::make_before_break_enabled`].
struct UpgradeProbe {
    pubkey: [u8; 32],
    overlay_ip: Ipv4Addr,
    /// The direct endpoint the probe dials (the promoted carrier's off-link
    /// exit-exemption dst for `Public`/`Srflx`).
    dst: std::net::SocketAddr,
    /// Which direct tier is being probed — drives the deadline + CC1 cooldown.
    tier: DirectTier,
    /// When the probe was started — for the tier handshake deadline.
    since: Instant,
}

/// Grace after install before the fallback can fire — lets the bilateral
/// handshake + first packets flow before we judge the carrier.
const DIRECT_GRACE: Duration = Duration::from_secs(8);
/// Consecutive bad sweeps (sent, received nothing) before falling back. At the
/// 5 s tick that's ~15 s of one-way traffic — long enough to ignore a blip,
/// short enough that a VPN/AP-isolation break doesn't stay dark for long.
const BAD_SWEEPS_TO_FALLBACK: u32 = 3;
/// rc.206 — the "silent zombie" backstop. An *established* carrier that stops
/// RECEIVING is dead even when it also stopped SENDING: a healthy peer emits a
/// WireGuard persistent-keepalive every ~25 s (`wg::KEEPALIVE_SECS`), so no
/// inbound packet for this long means the underlying path died AND boringtun
/// gave up re-handshaking (it stops emitting anything once a rekey attempt
/// expires ~90 s). With no tx either, the `tx>last_tx && rx==last_rx` heuristic
/// reads that as "just idle — no judgment" and never tears the carrier down —
/// observed in the field as an 8-hour "direct" carrier stuck at 100 % loss with
/// a frozen last-seen. This absolute rx-staleness deadline catches it regardless
/// of tx. 90 s = ~3–4 missed keepalives: comfortably past a transient blip, well
/// short of the multi-hour zombie. A false trip only forces a (cheap) rebuild,
/// which re-establishes if the path actually recovered.
const RX_STALE_DEADLINE: Duration = Duration::from_secs(90);
/// After a direct carrier fails, don't retry direct for this peer for this
/// long — it stays on relay, then re-attempts direct (auto-recovers when the
/// blocking condition clears, e.g. the VPN disconnects).
const DIRECT_COOLDOWN: Duration = Duration::from_secs(60);
/// After this many consecutive direct-carrier failures for a peer, stop
/// retrying direct for the rest of the session (escalate to
/// [`DIRECT_DENY_COOLDOWN`]). The "same /24" was a false LAN signal that never
/// actually reaches — the classic case is two hosts sharing a corp full-tunnel
/// VPN's client pool (e.g. Check Point hands both a `192.168.0.x`) where the
/// VPN isolates clients from each other. Without this, the 60 s
/// [`DIRECT_COOLDOWN`] lapses and the next netmap re-upgrades the WORKING relay
/// to a direct carrier that can never complete — an endless relay↔direct flap
/// (field-observed: clean 44 ms relay pings interleaved with multi-second
/// stalls + total route-drop windows). A genuine transient (real-LAN blip) still
/// gets `DIRECT_MAX_FAILURES` attempts; a direct carrier that later proves
/// healthy clears the strike count.
const DIRECT_MAX_FAILURES: u32 = 2;
/// The session-sticky "give up on direct for this peer" cooldown, applied once a
/// peer hits [`DIRECT_MAX_FAILURES`]. Long enough to outlive any agent session
/// (agents cycle well under a day), so the peer stays pinned to the working
/// relay; a restart — or the peer genuinely landing on a real LAN in a later
/// session — re-attempts direct.
const DIRECT_DENY_COOLDOWN: Duration = Duration::from_secs(24 * 3600);
/// Phase C — the WG-handshake completion deadline for a srflx punch carrier:
/// past it with no session, the punch failed → tear down to relay. Tight —
/// bilateral INIT retransmit is ~5 s, so ~2 cycles + jitter + RTT covers a
/// genuine cross-NAT punch; longer just delays the relay fallback for a pair
/// that can't punch (e.g. one side symmetric).
const SRFLX_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(12);
/// Phase C — the handshake deadline for a public-direct (Phase A) carrier.
/// Looser than srflx: the accept side (a NAT'd client dialling a public exit)
/// can lag, and public-NIC reachability rarely fails outright, so we don't rush
/// it to relay. Still finite so a truly dead public dst can't zombie forever
/// (closes the same latent Phase A gap the srflx work exposed).
const PUBLIC_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(30);
/// rc.204 — LAN handshake deadline. On-link, so a genuine same-subnet
/// handshake completes in milliseconds; one that hasn't completed by this
/// window is a false LAN match (stale/foreign endpoint, Wi-Fi AP isolation, a
/// VPN-captured reply path). Pre-rc.204 the LAN tier had NO deadline, and a
/// never-handshaken carrier's tx/rx stay flat, so the rx-flat heuristic never
/// fired either — the pair was a permanent zombie with no relay fallback
/// (field-observed 2026-07-21: every LAN pair wedged in
/// `HANDSHAKE(REKEY_TIMEOUT)` while boringtun gave up after ~90 s). As tight
/// as srflx: it either establishes near-instantly or never will.
const LAN_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(12);
/// Phase C — srflx consecutive failures before the session-sticky deny. One
/// more than the LAN/public [`DIRECT_MAX_FAILURES`] — a punch legitimately
/// misses more often (timing/NAT), so give it an extra try before pinning.
const SRFLX_MAX_FAILURES: u32 = 3;
/// Phase C — the srflx deny cooldown once [`SRFLX_MAX_FAILURES`] is hit. Much
/// SHORTER than the LAN/public 24 h: a punch failure reflects the CURRENT NAT
/// pair, which changes when a host roams networks, so a day-long deny would
/// wrongly outlive the condition. 15 min re-attempts within a session.
const SRFLX_DENY_COOLDOWN: Duration = Duration::from_secs(15 * 60);
/// Phase C (D8) — re-run the direct-upgrade evaluation every Nth fallback tick
/// (6 × [`FALLBACK_TICK`] ≈ 30 s). A lapsed cooldown otherwise only matters when
/// the next netmap happens to arrive, so a quiet mesh would never re-attempt
/// direct after a fallback; this drives that retry (and Phase C punch
/// convergence at large install skew) without a netmap.
const REUPGRADE_EVERY_N_TICKS: u32 = 6;

/// The cooldown to apply after a direct-carrier failure, keyed by tier (CC1).
/// Escalates to the tier's session-sticky deny once a peer has failed that tier
/// [`DIRECT_MAX_FAILURES`] / [`SRFLX_MAX_FAILURES`] times (a persistent false
/// match — a VPN client pool for LAN, an unpunchable NAT pair for srflx —
/// rather than a transient blip). `fails` is the running failure count INCLUDING
/// the current failure.
fn direct_retry_cooldown(tier: DirectTier, fails: u32) -> Duration {
    let (max, deny) = match tier {
        DirectTier::Srflx => (SRFLX_MAX_FAILURES, SRFLX_DENY_COOLDOWN),
        _ => (DIRECT_MAX_FAILURES, DIRECT_DENY_COOLDOWN),
    };
    if fails >= max { deny } else { DIRECT_COOLDOWN }
}

/// The consecutive-failure count at which a tier escalates to its session-sticky
/// deny (the `sticky` log/decision in the sweep). Mirrors [`direct_retry_cooldown`].
fn direct_max_failures(tier: DirectTier) -> u32 {
    match tier {
        DirectTier::Srflx => SRFLX_MAX_FAILURES,
        _ => DIRECT_MAX_FAILURES,
    }
}
/// rc.139 — a dead RELAY carrier (one-way, same `tx>rx` signal) is usually a
/// STALE coturn port: the peer re-allocated (restart/churn → new port) and we
/// kept dialing the old one. Refresh it (re-request → fresh allocation, re-dial
/// the peer's CURRENT address) — but not more than once per this window, so two
/// ends each refreshing don't ping-pong faster than they can converge.
const RELAY_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
/// How often the carrier-health sweep runs. Cheap (lock-free atomic reads), so
/// a tighter cadence is fine and makes detection quicker.
const FALLBACK_TICK: Duration = Duration::from_secs(5);
/// How often to re-assert per-peer `/32` routes on the overlay NIC (rc.146).
/// A full-tunnel VPN (Check Point) keeps re-installing a competing `/32` for
/// each overlay IP via its own NIC that swallows overlay traffic; the route
/// table flaps between it and ours. Re-asserting UNCONDITIONALLY on a tight
/// cadence — not gated on the carrier's traffic counters, because a captured
/// route means our packets never reach the WG device so `tx` stays flat and a
/// traffic-gated check would never fire — keeps the overlay winning the route
/// war. Cheap (a couple of route commands per peer) and 2 s bounds the capture
/// window to a couple of dropped pings.
const ROUTE_GUARD_TICK: Duration = Duration::from_secs(2);
/// rc.211 — loop-stall watchdog threshold. While ANY steady-state select! arm's
/// handler awaits, the outbound TUN-packet arm (rc.213: an mpsc fed by the
/// dedicated reader task) is NOT re-polled, so outbound packets queue for the
/// handler's full duration (the field-observed
/// 1–2 s RTT plateaus on a churny Windows host). Every arm and the expensive
/// sub-calls inside the fat ones are timed via [`warn_if_slow`]; anything over
/// this threshold is named in the log. Permanent telemetry: two `Instant` reads
/// per arm, and the only way a head-of-line regression gets caught in the field.
const LOOP_STALL_WARN_MS: u128 = 250;

/// rc.211 — log a named steady-loop stall (see [`LOOP_STALL_WARN_MS`]).
fn warn_if_slow(stage: &'static str, t0: Instant) {
    let ms = t0.elapsed().as_millis();
    if ms > LOOP_STALL_WARN_MS {
        warn!(
            stage,
            ms, "overlay: steady-loop handler stalled the data plane (outbound queued this long)"
        );
    }
}

/// rc.211 — a finished OFF-LOOP QUIC-over-TURN carrier build (see
/// [`RelayBuildQueue`]). `quic: None` = the QUIC handshake failed/timed out →
/// the commit installs the link's already-built raw relay carrier (today's
/// fallback semantics, unchanged — just no longer blocking the loop).
struct BuiltRelay {
    epoch: u64,
    link: ReadyLink,
    quic: Option<Arc<Carrier>>,
}

/// rc.211 — bookkeeping for OFF-LOOP relay carrier builds. The QUIC-over-TURN
/// rendezvous (`Carrier::quic_relay`, capped at [`QUIC_BUILD_TIMEOUT`] = 8 s)
/// used to run INLINE on the steady-state select! loop — the field-proven
/// head-of-line stall behind the 1–2 s overlay RTT plateaus (the S1 watchdog
/// named it five times at 8.06 s in one 150 s run). `install_ready` now spawns
/// the build and the completion is committed by a dedicated select! arm.
///
/// Guards (adversarial-review C2/C3):
/// * `in_flight` maps node → the epoch stamped at spawn; a completion commits
///   ONLY if its epoch is still current, so any invalidating event (peer
///   removed, direct carrier installed / `coord.forget`) simply removes the
///   entry and the stale build is dropped on arrival — immune to the
///   forget→re-request ABA a plain "is building" set would have.
/// * `install_peers`' relay-coordination branch checks `in_flight` so it never
///   spawns a DUPLICATE coordination for a peer whose carrier is mid-build
///   (post-`try_build` the coordinator no longer tracks the peer, so
///   `!is_tracking` alone would re-request during the 8 s window).
struct RelayBuildQueue {
    in_flight: HashMap<ObjectId, u64>,
    epoch: u64,
    tx: mpsc::Sender<BuiltRelay>,
}

impl RelayBuildQueue {
    /// Stamp a new build for `node` (invalidates any prior in-flight build).
    fn stamp(&mut self, node: ObjectId) -> u64 {
        self.epoch += 1;
        self.in_flight.insert(node, self.epoch);
        self.epoch
    }
    /// Invalidate any in-flight build for `node` (peer removed / went direct /
    /// coordinator forgotten) — its completion will be dropped on arrival.
    fn invalidate(&mut self, node: &ObjectId) {
        self.in_flight.remove(node);
    }
    /// `true` iff `built` is still the CURRENT build for its peer; clears the
    /// entry either way (the completion consumes the slot).
    fn take_if_current(&mut self, built: &BuiltRelay) -> bool {
        if self.in_flight.get(&built.link.node_id) == Some(&built.epoch) {
            self.in_flight.remove(&built.link.node_id);
            true
        } else {
            false
        }
    }
}
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
                rtt_ms: None,
                last_seen_ms,
                // P3b-3 — carry the backing agent id (hex) so the daemon can join
                // this peer to a tunnel flow and label it `Tunnel`.
                agent_id: np.agent_id.map(|a| a.to_hex()),
                // rc.187 — relay endpoints (relay carriers only) so `peers --json`
                // shows each end's coturn worker; same IP on both = same-worker.
                relay_local: inst.and_then(|i| i.relay_local).map(|a| a.to_string()),
                relay_dst: inst.and_then(|i| i.relay_dst).map(|a| a.to_string()),
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
        // Set by `publish_view` from the runtime's exit-routing state (S4).
        exit_node: None,
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

/// P5 exit-node — the two IPv4 split-default halves. Installing these (as OS
/// routes via the overlay NIC + as the exit peer's WG `allowed_ips`) beats the
/// host's `0.0.0.0/0` default by longest-prefix WITHOUT deleting it, so the OS
/// default self-heals the instant the overlay routes go away (a crash / kill
/// can't strand the host offline — see A2/D3 in the P5 plan). `pub(crate)` so the
/// crash-safety purge ([`super::tun::purge_split_default`]) removes EXACTLY what
/// the installer installs — one source of truth, symmetric by construction.
pub(crate) const SPLIT_DEFAULT_V4: [&str; 2] = ["0.0.0.0/1", "128.0.0.0/1"];
/// P5 exit-node — the two IPv6 halves, installed via the overlay NIC as a
/// FAIL-CLOSED measure: the crypto-router drops any non-derived-ULA v6
/// destination, so routing `::/1` + `8000::/1` into the overlay blackholes ALL
/// v6 internet egress. Without this a dual-stack host would send v4 through the
/// exit but leak v6 straight out its uplink (silent AAAA deanonymisation). Full
/// v6 exit egress is a follow-up (S3b); this is the minimum-safe stance (A5).
pub(crate) const SPLIT_DEFAULT_V6: [&str; 2] = ["::/1", "8000::/1"];

/// P5 — resolve the operator's chosen exit-node selector (a [`NetmapPeer`]'s
/// `name` or a node-id hex string) to a concrete node in the current netmap.
/// Pure, so the name-vs-hex match is unit-tested directly. `None` when no peer
/// matches (the chosen exit isn't in the mesh yet — reconcile defers rather than
/// blackholing egress waiting for it).
fn resolve_exit_peer(selector: &str, peers: &HashMap<ObjectId, NetmapPeer>) -> Option<ObjectId> {
    let selector = selector.trim();
    peers
        .values()
        .find(|np| np.name == selector || np.node_id.to_hex() == selector)
        .map(|np| np.node_id)
}

/// P5 — is `peer` an admin-APPROVED exit node? The client only routes its
/// default egress through a peer whose netmap `routes` carry a default route
/// (`0.0.0.0/0`); the server only ever puts one there via the dedicated
/// exit-node approval (A6), so this is the client-side half of the admin gate —
/// naming a peer that wasn't approved as an exit node stays inert. Pure.
fn peer_is_approved_exit(peer: &NetmapPeer) -> bool {
    peer.routes
        .iter()
        .filter_map(|r| super::router::Cidr::parse(r))
        .any(|c| c.is_default_route())
}

/// P5 — the carrier-critical endpoint IPs that MUST bypass the split-default
/// (pinned via the ORIGINAL gateway) for the mesh to survive exit routing: the
/// coordination server's resolved IPs, every live RELAY carrier's coturn worker
/// IPs (both our own allocation `relay_local` and the peer's `relay_dst`), AND
/// (Phase A) every PUBLIC-DIRECT carrier's peer address. A SAME-LAN direct
/// carrier is on-link (a connected route more specific than a `/1`), so it needs
/// no exemption — but a public-direct carrier crosses the internet via the
/// default route, so without pinning its dst the split-default would swallow the
/// path to the exit itself and self-wedge. Pure, so the set arithmetic is
/// unit-tested against synthetic carriers.
fn exit_exemption_set(
    server_ips: &[IpAddr],
    by_node: &HashMap<ObjectId, Installed>,
) -> HashSet<IpAddr> {
    let mut set: HashSet<IpAddr> = server_ips.iter().copied().collect();
    for inst in by_node.values() {
        if let Some(a) = inst.relay_local {
            set.insert(a.ip());
        }
        if let Some(a) = inst.relay_dst {
            set.insert(a.ip());
        }
        if let Some(a) = inst.public_direct_dst {
            set.insert(a.ip());
        }
    }
    set
}

/// P5 — the exit peer's WG `allowed_ips` while it carries this node's default
/// egress: its own real (non-default) advertised subnets UNIONed with the two
/// v4 split-default halves, so packets to any non-overlay v4 destination
/// encapsulate to it (the `/1`s) while its `/32` host route + any real subnets
/// keep winning by longest-prefix for their own ranges. A peer that advertised
/// only `0.0.0.0/0` yields exactly the two `/1`s. Pure.
fn exit_peer_allowed_ips(exit: &NetmapPeer) -> Vec<super::router::Cidr> {
    let mut cidrs: Vec<super::router::Cidr> = peer_config_from_netmap(exit)
        .map(|c| c.subnets)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| !c.is_default_route())
        .collect();
    // unwrap: both are valid /1 CIDR literals (const-correct, covered by tests).
    cidrs.push(super::router::Cidr::parse(SPLIT_DEFAULT_V4[0]).unwrap());
    cidrs.push(super::router::Cidr::parse(SPLIT_DEFAULT_V4[1]).unwrap());
    cidrs
}

/// P5 — is the chosen exit peer ready to carry this node's default egress?
/// `Ok((id, np, pubkey))` when it is present in the netmap, admin-APPROVED
/// (`peer_is_approved_exit`), AND has a live carrier; else `Err(reason)` — the
/// operator-facing split-tunnel reason surfaced in [`ExitNodeStatus`] (S4). Pure,
/// so the reason mapping is unit-tested directly.
fn exit_readiness(
    selector: &str,
    current_peers: &HashMap<ObjectId, NetmapPeer>,
    by_node: &HashMap<ObjectId, Installed>,
) -> Result<(ObjectId, NetmapPeer, [u8; 32]), &'static str> {
    let id = resolve_exit_peer(selector, current_peers)
        .ok_or("exit node not visible in the mesh yet")?;
    let np = current_peers
        .get(&id)
        .ok_or("exit node not visible in the mesh yet")?;
    if !peer_is_approved_exit(np) {
        return Err("not an admin-approved exit node (no 0.0.0.0/0 approved)");
    }
    let inst = by_node
        .get(&id)
        .ok_or("exit node has no live carrier yet")?;
    Ok((id, np.clone(), inst.pubkey))
}

/// P5 — the LocalAPI [`ExitNodeStatus`] for the daemon view (S4), or `None` when
/// this node isn't an exit-node client. `active` mirrors the installed
/// split-default; `withheld_reason` is surfaced only while inactive (a stale
/// reason left on the state is suppressed once routing is active). Pure.
fn exit_node_status(selector: Option<&str>, state: &ExitRoutingState) -> Option<ExitNodeStatus> {
    let selector = selector?.to_string();
    Some(ExitNodeStatus {
        selector,
        active: state.split_default_installed,
        withheld_reason: if state.split_default_installed {
            None
        } else {
            state.withheld_reason.clone()
        },
        // S3b — global IPv6 also routes through the exit only when v4 is active
        // AND v6 egress was enabled (v6 exemptions pinned). Otherwise v6 is
        // fail-closed even while v4 egress is active.
        v6_active: state.split_default_installed && state.v6_active == Some(true),
        // S4b — DNS steered through the exit only while v4 egress is active.
        dns_steered: state.split_default_installed && state.dns_steered,
    })
}

/// S4 — record the split-tunnel WITHHELD reason on the state and log it ONCE per
/// reason change (dedup on `state.withheld_reason`), so a persistently-withheld
/// exit config doesn't spam the log every reconcile while still surfacing each
/// distinct cause. The live reason is also exposed via [`ExitNodeStatus`] for
/// `roomler status`.
fn note_withheld(state: &mut ExitRoutingState, selector: &str, reason: &'static str) {
    if state.withheld_reason.as_deref() != Some(reason) {
        warn!(
            exit = %selector, reason,
            "overlay exit-node: default routing WITHHELD (split-tunnel safety) — egress stays on the local uplink"
        );
        state.withheld_reason = Some(reason.to_string());
    }
}

/// P5 exit-node — live state of default-route capture, owned by [`run`]'s loop.
#[derive(Default)]
struct ExitRoutingState {
    /// The exit peer currently carrying our egress, once chosen + reachable +
    /// carriered + approved. `None` when inactive.
    active_peer: Option<ObjectId>,
    /// `/32` (host) exemptions currently pinned via the original gateway — so we
    /// add only NEW ones per reconcile and revert exactly on teardown.
    exemptions: HashSet<IpAddr>,
    /// Whether the v4 split-default (`0.0.0.0/1`+`128.0.0.0/1`) is installed.
    split_default_installed: bool,
    /// S4 — why default routing is currently WITHHELD (the split-tunnel signal),
    /// surfaced in [`ExitNodeStatus`]. `None` when active or not configured. Also
    /// the dedup key for the withhold WARN (log only on a reason change).
    withheld_reason: Option<String>,
    /// S3b — global IPv6 egress state: `None` = undecided; `Some(true)` = v6
    /// routes through the exit; `Some(false)` = v6 fail-closed (no v6 uplink to
    /// exempt the coordination server, or a Windows exit). Reset to `None` on
    /// teardown. Independent of `split_default_installed` (v4) per BLOCKER-1 — v6
    /// never gates v4. Also the transition-log dedup key.
    v6_active: Option<bool>,
    /// S4b — exit-node DNS steering context + state. `dns_magic_domain`: `Some` =
    /// MagicDNS on (steer "." at the local resolver `dns_target` == self_v4, which
    /// forwards to the network upstream via the exit); `None` = MagicDNS off (steer
    /// "." at `dns_target` == the public upstream directly). `dns_bound`: the local
    /// resolver bound :53 (only gates the MagicDNS-on steer — steering at a dead
    /// resolver would blackhole ALL DNS). `dns_steered`: the "." catch-all steer is
    /// currently installed (⇒ `split_default_installed`, locked by a debug-assert).
    dns_magic_domain: Option<String>,
    dns_target: Option<Ipv4Addr>,
    dns_bound: bool,
    dns_steered: bool,
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
        }
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
            let mut view = build_overlay_view(self_ip, by_node, current_peers, now, epoch_now_ms);
            // S4 — the exit-node routing status the runtime holds (the view
            // builder is pure over peers, so this is grafted on after).
            view.exit_node = exit_status;
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
            supports_relay_single: crate::overlay::direct::relay_single_enabled(),
            // Phase D (DERP) — advertise the DERP capability (our OVERLAY_DERP
            // flag) so a both-UDP-blocked pair only picks DERP when BOTH ends
            // opted in. Default-OFF until field-proven.
            supports_derp: crate::overlay::direct::derp_enabled(),
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
        // rc.136 + CC1 — peers whose DIRECT carrier just failed: don't retry
        // that tier until its `until` Instant (they stay on relay). Split by
        // tier (LAN / public) so a public-direct failure never poisons the
        // proven same-LAN path; each tier auto-expires → direct retried, and
        // escalates to a session-sticky deny after DIRECT_MAX_FAILURES (a
        // VPN-pool false /24, or a persistently-firewalled public endpoint).
        let mut cooldowns = DirectCooldowns::default();
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

        let mut fallback = tokio::time::interval(FALLBACK_TICK);
        // rc.146 — re-assert per-peer /32 routes so a full-tunnel VPN can't keep
        // its competing capture routes installed. First tick fires immediately;
        // skip it (routes are freshly installed by `install_peers` below).
        let mut route_guard = tokio::time::interval(ROUTE_GUARD_TICK);
        route_guard.tick().await;
        // Phase D — LAZY `/derp`: open the WS (via the agent-provided factory)
        // ONLY for a relay-mode node that is itself UDP-blocked — i.e. its srflx
        // gather found nothing (`srflx_advertised.is_empty()`). A UDP-capable
        // node can never be in a both-UDP-blocked pair, so it never needs DERP
        // and shouldn't hold an idle `/derp` WS. The factory is `FnOnce`, so
        // `take()` it; a reconnect re-runs `run` and re-decides from the fresh
        // gather (a node that became UDP-blocked opens `/derp` then).
        let derp_mux = if matches!(self.mode, CarrierMode::Relay) && srflx_advertised.is_empty() {
            self.derp_mux_factory.take().map(|f| f())
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
        self.install_peers(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &first_peers,
            direct_ctx.as_ref(),
            &cooldowns,
            &mut upgrade_probes,
            &mut relay_bq,
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
            exit_node_status(self.exit_node.as_deref(), &exit_state),
        );

        // Phase C (D8) — re-upgrade tick counter (see `REUPGRADE_EVERY_N_TICKS`).
        let mut reupgrade_ticks: u32 = 0;

        // rc.206 — serializes the DETACHED route-guard re-assert (see the
        // `route_guard.tick()` arm): an owned `try_lock` drops any tick whose
        // predecessor batch is still running, so a slow Windows `netsh` sweep
        // never stacks concurrent delete-then-add mutations on the same prefix.
        let route_reassert_lock = Arc::new(tokio::sync::Mutex::new(()));

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
        let (tun_pkt_tx, mut tun_pkt_rx) = mpsc::channel::<Vec<u8>>(512);
        let reader_tun = tun.clone();
        let tun_reader = tokio::spawn(async move {
            loop {
                match reader_tun.read_packet().await {
                    Ok(pkt) => {
                        // Reader-side twin of `warn_if_slow`: a slow send here
                        // means the channel is FULL — the steady loop stopped
                        // consuming for long enough to queue 512 packets. The
                        // handler watchdog times executing handlers; this catches
                        // the complementary failure (loop wedged/starved between
                        // polls), which rc.211's telemetry was blind to.
                        let t0 = Instant::now();
                        if tun_pkt_tx.send(pkt).await.is_err() {
                            break; // runtime loop gone
                        }
                        let ms = t0.elapsed().as_millis();
                        if ms > LOOP_STALL_WARN_MS {
                            warn!(
                                ms,
                                "overlay: steady loop backpressured the TUN reader (outbound queue full)"
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

        // Phase 2 — steady state.
        loop {
            tokio::select! {
                read = tun_pkt_rx.recv() => match read {
                    Some(pkt) => {
                        let t0 = Instant::now();
                        let _ = wg.send_ip_packet(&pkt).await;
                        warn_if_slow("send_ip_packet", t0);
                    }
                    None => { debug!("overlay: TUN reader ended; runtime exiting"); break; }
                },
                // rc.211 — commit a finished OFF-LOOP relay carrier build (the
                // spawned QUIC-over-TURN rendezvous — see `RelayBuildQueue`).
                // The install half is µs. A STALE completion (peer removed /
                // went direct / superseded mid-build) is dropped; the next
                // netmap/sweep tick re-coordinates cleanly.
                built = built_rx.recv() => {
                    if let Some(built) = built {
                        let t_arm = Instant::now();
                        if relay_bq.take_if_current(&built) && current_peers.contains_key(&built.link.node_id) {
                            let BuiltRelay { link, quic, .. } = built;
                            self.install_built(&mut wg, &mut by_node, &tun, link, quic).await;
                            // Same tail as a synchronous relay install: a new
                            // coturn worker may need an exit exemption, and the
                            // LocalAPI view must reflect the new carrier.
                            self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                            self.publish_view(&self_ip, &by_node, &current_peers, exit_node_status(self.exit_node.as_deref(), &exit_state));
                        } else {
                            debug!(peer = %built.link.node_id, "overlay: dropping stale off-loop carrier build (peer removed/superseded mid-build)");
                        }
                        warn_if_slow("arm:relay_build_commit", t_arm);
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
                    let t0 = Instant::now();
                    self.sweep_carrier_health(
                        &mut wg, &mut by_node, &mut relay, &tun,
                        &mut cooldowns, &mut relay_refresh_cooldown, &current_peers,
                    ).await;
                    warn_if_slow("sweep_carrier_health", t0);
                    // rc.208 — make-before-break: promote any upgrade probe whose
                    // handshake latched (cut over to direct, drop the relay) and
                    // expire any that missed its deadline (keep the relay). Inert
                    // when the feature is off / no probes are in flight.
                    let t0 = Instant::now();
                    self.sweep_upgrade_probes(
                        &mut wg, &mut by_node, &mut relay, &tun,
                        &mut upgrade_probes, &mut cooldowns,
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
                            &peers, direct_ctx.as_ref(), &cooldowns, &mut upgrade_probes,
                            &mut relay_bq,
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
                        let ips: Vec<Ipv4Addr> =
                            by_node.values().map(|e| e.overlay_ip).collect();
                        tokio::spawn(async move {
                            let _guard = guard;
                            for ip in ips {
                                tun2.add_peer_route(ip).await.ok();
                            }
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
                            &current_peers, &mut cooldowns, &mut upgrade_probes,
                            &mut relay_bq, inb,
                        ).await;
                        self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                        self.publish_view(&self_ip, &by_node, &current_peers, exit_node_status(self.exit_node.as_deref(), &exit_state));
                        warn_if_slow("arm:direct_inbound", t_arm);
                    }
                },
                evt = events.recv() => match evt {
                    // Re-sync: install any newly-listed peers (deltas drive
                    // removals; a full diff/prune is a later refinement).
                    Some(OverlayEvent::Netmap { peers, .. }) => {
                        let t_arm = Instant::now();
                        current_peers = peers.iter().map(|p| (p.node_id, p.clone())).collect();
                        if let Some(names) = &dns_names { sync_name_map(names, &current_peers).await; }
                        let t0 = Instant::now();
                        self.install_peers(&mut wg, &mut by_node, &mut relay, &tun, &peers, direct_ctx.as_ref(), &cooldowns, &mut upgrade_probes, &mut relay_bq).await;
                        warn_if_slow("install_peers(netmap)", t0);
                        self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                        self.publish_view(&self_ip, &by_node, &current_peers, exit_node_status(self.exit_node.as_deref(), &exit_state));
                        warn_if_slow("arm:netmap", t_arm);
                    }
                    Some(OverlayEvent::NetmapDelta { upserts, removes }) => {
                        let t_arm = Instant::now();
                        for p in &upserts { current_peers.insert(p.node_id, p.clone()); }
                        let t0 = Instant::now();
                        self.install_peers(&mut wg, &mut by_node, &mut relay, &tun, &upserts, direct_ctx.as_ref(), &cooldowns, &mut upgrade_probes, &mut relay_bq).await;
                        warn_if_slow("install_peers(delta)", t0);
                        for node_id in removes {
                            current_peers.remove(&node_id);
                            if let Some(e) = by_node.remove(&node_id) {
                                wg.remove_peer(&e.pubkey).await;
                                tun.del_peer_route(e.overlay_ip).await;
                                info!(peer = %node_id, "overlay: peer removed");
                            }
                            if let Some(r) = relay.as_mut() {
                                r.forget(&node_id);
                            }
                            // rc.211 — drop any in-flight off-loop relay build
                            // for the removed peer (stale on arrival).
                            relay_bq.invalidate(&node_id);
                            // rc.208 — drop any in-flight make-before-break probe
                            // for a removed peer (its shadow carrier + demux reg).
                            if let Some(pr) = upgrade_probes.remove(&node_id) {
                                wg.drop_direct_probe(&pr.pubkey).await;
                            }
                        }
                        if let Some(names) = &dns_names { sync_name_map(names, &current_peers).await; }
                        self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                        self.publish_view(&self_ip, &by_node, &current_peers, exit_node_status(self.exit_node.as_deref(), &exit_state));
                        warn_if_slow("arm:netmap_delta", t_arm);
                    }
                    Some(OverlayEvent::RelayGrant { peer_node_id, ice_servers, pair_key }) => {
                        let t_arm = Instant::now();
                        if let Some(r) = relay.as_mut() {
                            let t0 = Instant::now();
                            let link = r.on_grant(peer_node_id, ice_servers, pair_key).await;
                            warn_if_slow("on_grant(dns+turn-allocate)", t0);
                            if let Some(link) = link {
                                let t0 = Instant::now();
                                self.install_ready(&mut wg, &mut by_node, &tun, link, &mut relay_bq).await;
                                warn_if_slow("install_ready(spawn-or-sync)", t0);
                                // A newly-installed relay carrier adds a coturn worker
                                // to exempt (and the exit peer may have just become
                                // reachable) — reconcile exit routing, then refresh
                                // the view so `roomler status` reflects it.
                                self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                                self.publish_view(&self_ip, &by_node, &current_peers, exit_node_status(self.exit_node.as_deref(), &exit_state));
                            }
                        }
                        warn_if_slow("arm:relay_grant", t_arm);
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
        // Phase C — stop the srflx keepalive task (if any) on runtime exit.
        if let Some(h) = srflx_keepalive {
            h.abort();
        }
    }

    /// rc.137/139 — find carriers that are one-way / dead and repair them.
    /// Health is LOCK-FREE: each sweep snapshots `(tx, rx)` (atomic reads — no
    /// `Tunn` lock, so it can't stall the packet path like the rc.136
    /// handshake-age check did); a carrier where **tx climbed but rx stayed
    /// flat** for [`BAD_SWEEPS_TO_FALLBACK`] consecutive sweeps is dead (we're
    /// sending, nothing comes back). The repair depends on the carrier kind:
    /// - **direct** → fall back to relay (the LAN path only LOOKED viable —
    ///   corp VPN route hijack, Wi-Fi AP/client isolation, asymmetric firewall);
    ///   [`DIRECT_COOLDOWN`] keeps the next netmap from re-upgrading it.
    /// - **relay** (rc.139) → refresh it: the peer almost certainly
    ///   re-allocated its coturn port (restart/churn) and we're dialing a stale
    ///   one. Re-request so we re-allocate + re-dial the peer's CURRENT address
    ///   ([`RELAY_REFRESH_COOLDOWN`] bounds two ends ping-ponging).
    #[allow(clippy::too_many_arguments)]
    async fn sweep_carrier_health(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        relay: &mut Option<RelayCoordinator>,
        tun: &Arc<dyn TunIo>,
        cooldowns: &mut DirectCooldowns,
        relay_refresh_cooldown: &mut HashMap<ObjectId, Instant>,
        current_peers: &HashMap<ObjectId, NetmapPeer>,
    ) {
        let now = Instant::now();
        // (node_id, tier, hard_dead, rx_stale)
        let mut dead: Vec<(ObjectId, DirectTier, bool, bool)> = Vec::new();
        for (nid, e) in by_node.iter_mut() {
            let Some((tx, rx)) = wg.peer_traffic(&e.pubkey) else {
                continue;
            };
            let (last_tx, last_rx) = e.last_traffic;
            e.last_traffic = (tx, rx);
            // P3b-3 / rc.206 — "last heard from this peer" advances on ANY
            // authenticated inbound packet, INCLUDING content-free WG keepalives.
            // The IP-data `rx` counter alone froze on a mostly-idle-but-alive
            // carrier (its only inbound is keepalives → `TunnResult::Done`, which
            // never touches `rx`), so the rx-staleness watchdog below would have
            // reaped a healthy idle link. `peer_take_rx_any` drains the
            // keepalive-inclusive liveness counter (single-consumer; the sweep is
            // the only reader). Advance BEFORE the warm-up `continue` so a freshly
            // installed peer's first inbound already registers.
            if wg.peer_take_rx_any(&e.pubkey) > 0 {
                e.last_rx_at = now;
            }
            // rc.181 — a relay carrier whose underlying send hard-errored (a
            // TURNS/TCP reset, or a lost QUIC-over-TURN connection) is dead
            // NOW. Skip BOTH the warm-up grace and the multi-sweep rx-flat
            // heuristic for it and re-allocate on this tick (still rate-limited
            // by `relay_refresh_cooldown` below). Always `false` for a direct
            // carrier, so this only ever fast-paths a relay.
            let hard_dead = wg.peer_carrier_dead(&e.pubkey).unwrap_or(false);
            // Phase C (+ rc.204) — direct-tier handshake deadline: a direct
            // carrier that never completed its WG handshake within the tier
            // deadline is a zombie. Its tx/rx stay flat pre-handshake
            // (handshake packets touch neither counter), so the rx-flat
            // heuristic below can't see it, and boringtun stops even
            // keepalives once the attempt expires (~90 s) — it would live
            // forever. Tear it down → cooldown → relay. Once the handshake
            // latches (`peer_handshake_done`) this can never fire again (the
            // tx/rx heuristic governs the established carrier thereafter).
            // rc.204 extends this to the LAN tier — a false same-subnet match
            // (stale endpoint / AP isolation / VPN-captured replies) was a
            // PERMANENT zombie before, with no fallback to relay.
            let punch_dead = matches!(
                e.tier,
                DirectTier::Public | DirectTier::Srflx | DirectTier::Lan
            ) && !wg.peer_handshake_done(&e.pubkey).unwrap_or(true)
                && e.since.elapsed() > e.tier.handshake_deadline();
            // rc.206 — silent-zombie backstop (see RX_STALE_DEADLINE). An
            // ESTABLISHED carrier (handshake latched — so `punch_dead`, which
            // only fires PRE-handshake, can never catch it) whose inbound packet
            // count has stayed frozen past the deadline is dead: a live peer's
            // persistent-keepalives would have kept advancing `last_rx_at`. This
            // is independent of tx, so it catches the no-tx-AND-no-rx zombie the
            // `tx>last_tx && rx==last_rx` heuristic below misreads as benign idle
            // (boringtun stops emitting once its rekey attempts expire → tx also
            // flatlines → the heuristic's strike counter even resets). Covers a
            // relay carrier too — a silently-dropped coturn allocation stops
            // delivering with no send-error for `hard_dead` to observe.
            let rx_stale = wg.peer_handshake_done(&e.pubkey).unwrap_or(false)
                && e.since.elapsed() >= DIRECT_GRACE
                && now.saturating_duration_since(e.last_rx_at) > RX_STALE_DEADLINE;
            // Warm-up grace: let the handshake + first packets flow. (A blown
            // punch deadline is > grace by construction, so it never lands in the
            // grace window; a hard-dead relay conclusively skips it.)
            if !hard_dead && e.since.elapsed() < DIRECT_GRACE {
                continue;
            }
            // Sent this interval but received nothing back ⇒ suspect. (If we
            // didn't send either, the link is just idle — no judgment.)
            if tx > last_tx && rx == last_rx {
                e.bad_sweeps += 1;
            } else {
                e.bad_sweeps = 0;
                // A direct carrier that's actually RECEIVING is genuinely
                // healthy → clear its strike count so old failures don't
                // accumulate across a long healthy period and prematurely pin a
                // real peer to relay. (rx advancing, not just idle.) Clear the
                // count on the carrier's OWN tier (CC1 — never cross-clear).
                if e.is_direct && rx > last_rx {
                    match e.tier {
                        DirectTier::Srflx => {
                            cooldowns.srflx_fails.remove(nid);
                        }
                        DirectTier::Public => {
                            cooldowns.public_fails.remove(nid);
                        }
                        _ => {
                            cooldowns.lan_fails.remove(nid);
                        }
                    }
                }
            }
            if e.bad_sweeps >= BAD_SWEEPS_TO_FALLBACK || hard_dead || punch_dead || rx_stale {
                // For a relay, hold off if we just refreshed it (anti-ping-pong).
                if !e.is_direct
                    && relay_refresh_cooldown
                        .get(nid)
                        .is_some_and(|&until| until > now)
                {
                    continue;
                }
                dead.push((*nid, e.tier, hard_dead, rx_stale));
            }
        }
        for (nid, tier, hard_dead, rx_stale) in dead {
            let Some(e) = by_node.remove(&nid) else {
                continue;
            };
            wg.remove_peer(&e.pubkey).await;
            tun.del_peer_route(e.overlay_ip).await;
            if tier.is_direct() {
                // Escalating cooldown on the carrier's OWN tier (CC1). LAN: the
                // "same /24" was a VPN client pool, not a reachable LAN. Public:
                // the peer's advertised public endpoint isn't actually reachable
                // (host firewall / not truly public). Srflx: the pair couldn't
                // punch (one side symmetric / hostile filter). Either way, after
                // that tier's max failures pin this peer to relay for the session
                // (srflx: only 15 min — NAT conditions change on roam) so a false
                // match can't flap the working relay.
                let (count_map, cooldown_map, tier_name) = match tier {
                    DirectTier::Srflx => {
                        (&mut cooldowns.srflx_fails, &mut cooldowns.srflx, "srflx")
                    }
                    DirectTier::Public => {
                        (&mut cooldowns.public_fails, &mut cooldowns.public, "public")
                    }
                    _ => (&mut cooldowns.lan_fails, &mut cooldowns.lan, "LAN"),
                };
                let fails = count_map.entry(nid).or_insert(0);
                *fails += 1;
                let sticky = *fails >= direct_max_failures(tier);
                cooldown_map.insert(nid, now + direct_retry_cooldown(tier, *fails));
                if sticky {
                    warn!(
                        peer = %nid, tier = tier_name, fails = *fails,
                        "overlay: direct carrier failed repeatedly — pinning this peer to relay for the session"
                    );
                } else if rx_stale {
                    // rc.206 — an ESTABLISHED direct carrier that went silent
                    // (peer roamed / NAT rebind / path died mid-session), not a
                    // never-punched one. Distinct message so field logs separate
                    // "died" from "never established". A re-upgrade re-punches
                    // once the cooldown clears; the fail count usually clears on
                    // the first receiving sweep after that, so a one-off death
                    // doesn't march toward the sticky pin.
                    warn!(
                        peer = %nid, tier = tier_name,
                        "overlay: established direct carrier went silent (no keepalive within the rx-stale deadline — peer roamed / NAT rebind / path died) — rebuilding via relay"
                    );
                } else {
                    warn!(
                        peer = %nid, tier = tier_name,
                        "overlay: direct carrier didn't establish (firewall / VPN / AP-isolation / unpunchable NAT?) — falling back to relay"
                    );
                }
            } else {
                relay_refresh_cooldown.insert(nid, now + RELAY_REFRESH_COOLDOWN);
                if hard_dead {
                    warn!(
                        peer = %nid,
                        "overlay: relay carrier send hard-errored (TURNS/TCP reset / QUIC-over-TURN lost) — re-allocating"
                    );
                } else if rx_stale {
                    // rc.206 — a relay carrier that stopped delivering with no
                    // send-error to trip `hard_dead` (silently-dropped coturn
                    // allocation / a dead worker the send path can't detect).
                    warn!(
                        peer = %nid,
                        "overlay: relay carrier went silent (no keepalive within the rx-stale deadline — coturn allocation dropped?) — re-allocating"
                    );
                } else {
                    warn!(
                        peer = %nid,
                        "overlay: relay carrier one-way (stale coturn port?) — re-allocating"
                    );
                }
            }
            // (Re)request the relay now (don't wait for the next netmap). For a
            // relay refresh we first forget the stale allocation so a fresh one
            // is made; a direct→relay fall has no prior allocation to forget.
            if let (Some(coord), Some(np)) = (relay.as_mut(), current_peers.get(&nid))
                && let Some(cfg) = peer_config_from_netmap(np)
            {
                if !tier.is_direct() {
                    coord.forget(&nid);
                }
                coord.request(nid, cfg).await;
            }
        }
    }

    /// rc.131 — bind the shared direct-carrier socket + discover our LAN
    /// endpoint. Only in Relay mode (Direct mode is the loopback test/helper
    /// path) and when `ROOMLER_AGENT_OVERLAY_DIRECT` isn't disabled. `None` if
    /// disabled, not relay mode, the bind fails, or there's no usable LAN IP
    /// (offline / CGNAT-only) — the node then stays relay-only as before.
    async fn setup_direct(&self) -> Option<DirectCtx> {
        if !matches!(self.mode, CarrierMode::Relay) || !direct::direct_enabled() {
            return None;
        }
        let ifaces = direct::gather_lan_interfaces();
        let my_ips: Vec<Ipv4Addr> = ifaces.iter().map(|(ip, _)| *ip).collect();
        if my_ips.is_empty() {
            info!("overlay: no usable LAN interface; direct path off (relay only)");
            return None;
        }
        // rc.143 — bind ONE socket per interface IP (to that IP, not 0.0.0.0);
        // rc.144 — ALSO pin egress to that NIC via IP_UNICAST_IF, because on
        // Windows a source-IP bind alone doesn't force the egress interface (the
        // route does), so a full-tunnel VPN's default route otherwise steals the
        // send and same-WiFi direct oscillates. Advertise each socket's own
        // `ip:port`; the peer dials whichever shares its subnet, and both sides
        // then send/receive over that interface's pinned socket.
        let mut socks: Vec<(Ipv4Addr, Arc<UdpSocket>)> = Vec::new();
        let mut endpoints: Vec<String> = Vec::new();
        for (ip, ifindex) in &ifaces {
            match UdpSocket::bind((*ip, 0)).await {
                Ok(s) => {
                    if let Some(idx) = ifindex {
                        direct::force_egress_interface(&s, *idx);
                    }
                    match s.local_addr() {
                        Ok(local) => {
                            endpoints.push(format!("{ip}:{}", local.port()));
                            socks.push((*ip, Arc::new(s)));
                        }
                        Err(e) => {
                            warn!(%ip, %e, "overlay: direct socket local_addr failed; skipping")
                        }
                    }
                }
                Err(e) => {
                    warn!(%ip, %e, "overlay: bind direct socket on interface failed; skipping")
                }
            }
        }
        if socks.is_empty() {
            info!("overlay: no bindable LAN interface; direct path off (relay only)");
            return None;
        }
        info!(
            endpoints = ?endpoints,
            "overlay: advertising direct LAN endpoints (per-interface sockets; same-subnet peers dial direct)"
        );
        // Phase A/B — a single unbound socket to DIAL peers' public endpoints
        // (the OS picks egress per-destination). Shared by the public-direct
        // tier (peer's public NIC) AND the srflx tier (peer's NAT mapping), so
        // it's bound when EITHER is on. Best-effort: a bind failure just leaves
        // both public-dial tiers off (relay still works).
        let public_sock = if direct::public_direct_enabled() || direct::srflx_enabled() {
            match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
                Ok(s) => {
                    info!(
                        public_direct = direct::public_direct_enabled(),
                        srflx = direct::srflx_enabled(),
                        "overlay: public-dial egress socket ON (NAT-traversal Phase A/B)"
                    );
                    Some(Arc::new(s))
                }
                Err(e) => {
                    warn!(%e, "overlay: public-dial egress socket bind failed; public/srflx tiers off");
                    None
                }
            }
        } else {
            None
        };
        Some(DirectCtx {
            socks,
            my_ips,
            endpoints,
            public_sock,
            // Set after the startup srflx gather (Phase C) once we know which
            // interface socket owns our first advertised srflx candidate + the
            // NAT-type probe on it.
            punch: None,
            my_nat: None,
        })
    }

    /// Reconcile the netmap into installed peers. NOT-yet-installed: Direct
    /// mode → build the loopback/test carrier; Relay mode → a DIRECT LAN
    /// carrier when the peer advertises a same-subnet endpoint (rc.131/134 — N
    /// peers share one socket via the device's source-address demux), else the
    /// coturn relay coordination. ALREADY-installed on RELAY but a same-subnet
    /// endpoint has since appeared → UPGRADE to direct (rc.134 re-evaluation:
    /// a peer first seen before its endpoint arrived would otherwise stay on
    /// relay forever). A peer in a [`DIRECT_COOLDOWN`] (its direct carrier just
    /// failed — rc.136) is kept on relay regardless of a same-subnet endpoint.
    #[allow(clippy::too_many_arguments)]
    async fn install_peers(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        relay: &mut Option<RelayCoordinator>,
        tun: &Arc<dyn TunIo>,
        peers: &[NetmapPeer],
        direct_ctx: Option<&DirectCtx>,
        cooldowns: &DirectCooldowns,
        upgrade_probes: &mut HashMap<ObjectId, UpgradeProbe>,
        relay_bq: &mut RelayBuildQueue,
    ) {
        let now = Instant::now();
        // rc.208 — make-before-break: probe a relay→direct upgrade instead of
        // tearing the relay down speculatively. Read once per call.
        let make_before_break = super::direct::make_before_break_enabled();
        for np in peers {
            let Some(cfg) = peer_config_from_netmap(np) else {
                continue;
            };
            // rc.136 + CC1 — suppress a direct TIER while this peer is cooling
            // down from a failure on THAT tier (treat as if no such endpoint →
            // fall through). Expired entries lapse, so the tier is retried. The
            // LAN / public / srflx cooldowns are all independent (a punch
            // failure never poisons the LAN or public-NIC tiers).
            let lan_cooling = DirectCooldowns::cooling(&cooldowns.lan, &np.node_id, now);
            let public_cooling = DirectCooldowns::cooling(&cooldowns.public, &np.node_id, now);
            let srflx_cooling = DirectCooldowns::cooling(&cooldowns.srflx, &np.node_id, now);
            // A same-subnet LAN endpoint for this peer (highest-priority tier).
            // rc.204 — scan the provenance-pure `lan_endpoints` bucket (the
            // peer's join-time NIC sockets), NOT the `endpoints` union: the
            // union also carries the peer's trickled coturn-RELAYED addresses,
            // and on this fleet the coturn workers ride the hosts' own public
            // IPs, so a fleet host same-/24-matched a peer's *relay allocation*
            // and "LAN"-dialed coturn forever (field-observed 2026-07-21: mars
            // dialing NEO16's relayed 94.130.141.74:* as a LAN endpoint).
            let direct_dst = if lan_cooling {
                None
            } else {
                direct_ctx.and_then(|ctx| {
                    direct::pick_same_subnet_endpoint(&ctx.my_ips, &cfg.lan_endpoints)
                })
            };
            // Phase A — the peer's PUBLIC NIC endpoint (its join-time bucket),
            // dialable WITHOUT STUN because the peer's NIC holds a public IP.
            // Dialed over the shared `public_sock` (arbitrary egress is fine —
            // the peer has no NAT filter). Gated by the flag + its cooldown +
            // the egress socket.
            let phase_a_dst = if public_cooling {
                None
            } else {
                direct_ctx.and_then(|ctx| {
                    (direct::public_direct_enabled() && ctx.public_sock.is_some())
                        .then(|| direct::pick_public_endpoint(&ctx.my_ips, &cfg.lan_endpoints))
                        .flatten()
                })
            };
            // Phase C — the peer's srflx (its STUN-learned public NAT mapping),
            // dialed over the PUNCH socket (the one that owns OUR advertised
            // srflx) so our INITs ride our advertised mapping and open our NAT's
            // filter toward the peer — the mutual hole-punch. Distinct socket
            // and cooldown from Phase A. Gated by the flag + its cooldown + a
            // gathered punch socket, AND skipped when BOTH ends are symmetric
            // (a punch can't work then — save the futile 12 s attempt + the
            // strike; any cone/unknown side still attempts). Lowest direct tier.
            let srflx_dst = if srflx_cooling {
                None
            } else {
                direct_ctx.and_then(|ctx| {
                    (direct::srflx_enabled()
                        && ctx.punch.is_some()
                        && direct::srflx_punch_worth_trying(
                            ctx.my_nat.as_deref(),
                            cfg.srflx_nat.as_deref(),
                        ))
                    .then(|| direct::pick_public_endpoint(&ctx.my_ips, &cfg.srflx_endpoints))
                    .flatten()
                })
            };

            // Copy-out the installed carrier's shape (all Copy), so the by_node
            // borrow ends before any mutation below.
            let installed = by_node
                .get(&np.node_id)
                .map(|e| (e.is_direct, e.pubkey, e.tier, e.public_direct_dst));
            match installed {
                Some((true, pk, tier, inst_dst)) => {
                    // D10 — a zombie srflx punch (installed but never handshook)
                    // whose advertised srflx has since CHANGED: re-dial the fresh
                    // mapping NOW, without booking a strike (the old dst is
                    // known-stale — not evidence the pair can't punch). Otherwise
                    // a srflx re-trickle sits ignored on an already-direct peer
                    // until the handshake deadline tears it down (~100 s later),
                    // and books a bogus strike doing so.
                    if tier == DirectTier::Srflx
                        && !wg.peer_handshake_done(&pk).unwrap_or(true)
                        && let (Some(ctx), Some(fresh)) = (direct_ctx, srflx_dst)
                        && inst_dst != Some(fresh)
                    {
                        info!(peer = %np.node_id, old = ?inst_dst, new = %fresh, "overlay: srflx changed under a pending punch — re-dialing fresh mapping");
                        wg.remove_peer(&pk).await;
                        self.install_srflx_direct(wg, by_node, tun, ctx, np.node_id, &cfg, fresh)
                            .await;
                    }
                    continue; // already direct (LAN / public / srflx)
                }
                Some((false, pk, _, _)) => {
                    // Installed on RELAY — upgrade to the best available direct
                    // tier now that an endpoint has appeared: LAN > public-NIC >
                    // srflx punch.
                    //
                    // rc.208 make-before-break: when enabled, install the
                    // candidate direct carrier as a SHADOW PROBE (keyed by the
                    // same pubkey; its own `Tunn` in `WgDevice::probes`) while the
                    // working relay keeps routing. `sweep_upgrade_probes` cuts
                    // over only once the probe's handshake latches (proof the path
                    // works both ways), and drops it — leaving the relay
                    // untouched — if it never does. This kills the ~15-38 s
                    // per-upgrade freeze the destructive path below caused on a
                    // peer that can only relay (same-NAT AP-isolation / no
                    // hairpin). Skip if a probe for this peer is already in flight.
                    if make_before_break {
                        // Resolve the best available direct tier's (socket, dst):
                        // LAN > public-NIC > srflx punch — same precedence as the
                        // destructive path. Skip if a probe is already in flight.
                        let probe_target = if wg.has_direct_probe(&pk) {
                            None
                        } else {
                            direct_ctx.and_then(|ctx| {
                                if let Some((local_ip, dst)) = direct_dst {
                                    ctx.socks
                                        .iter()
                                        .find(|(ip, _)| *ip == local_ip)
                                        .map(|(_, s)| (s.clone(), dst, DirectTier::Lan))
                                } else if let Some(dst) = phase_a_dst {
                                    ctx.public_sock
                                        .clone()
                                        .map(|s| (s, dst, DirectTier::Public))
                                } else if let Some(dst) = srflx_dst {
                                    ctx.punch.clone().map(|(_, s)| (s, dst, DirectTier::Srflx))
                                } else {
                                    None
                                }
                            })
                        };
                        if let Some((sock, dst, tier)) = probe_target {
                            self.start_upgrade_probe(
                                wg,
                                upgrade_probes,
                                np.node_id,
                                &cfg,
                                sock,
                                dst,
                                tier,
                                now,
                            )
                            .await;
                        }
                        continue;
                    }
                    // Pre-rc.208 destructive upgrade (break-before-make): tears the
                    // relay down first, then handshakes over the (unproven) direct
                    // path. Kept as the default until make-before-break is
                    // field-proven per-host.
                    if let (Some(ctx), Some((local_ip, dst))) = (direct_ctx, direct_dst) {
                        info!(peer = %np.node_id, %dst, "overlay: upgrading relay peer to direct LAN carrier");
                        wg.remove_peer(&pk).await;
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_direct(wg, by_node, tun, ctx, np.node_id, &cfg, local_ip, dst)
                            .await;
                    } else if let (Some(ctx), Some(dst)) = (direct_ctx, phase_a_dst) {
                        info!(peer = %np.node_id, %dst, "overlay: upgrading relay peer to direct-to-public carrier");
                        wg.remove_peer(&pk).await;
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_public_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
                            .await;
                    } else if let (Some(ctx), Some(dst)) = (direct_ctx, srflx_dst) {
                        info!(peer = %np.node_id, %dst, "overlay: upgrading relay peer to srflx hole-punch carrier");
                        wg.remove_peer(&pk).await;
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_srflx_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
                            .await;
                    }
                    continue;
                }
                None => {}
            }

            match &self.mode {
                CarrierMode::Direct(links) => {
                    let Some(carrier) = links.build_carrier(&cfg).await else {
                        debug!(peer = %np.node_id, "overlay: no carrier built; retry next netmap");
                        continue;
                    };
                    self.install_ready(
                        wg,
                        by_node,
                        tun,
                        ReadyLink {
                            node_id: np.node_id,
                            public_key: cfg.public_key,
                            overlay_ip: cfg.overlay_ip,
                            carrier,
                            relay_parts: None,
                            supports_quic: cfg.supports_quic,
                            single_relay: None,
                            relay_kind: RelayKind::Turn,
                            subnets: cfg.subnets.clone(),
                        },
                        relay_bq,
                    )
                    .await;
                }
                CarrierMode::Relay => {
                    if let (Some(ctx), Some((local_ip, dst))) = (direct_ctx, direct_dst) {
                        // Same-subnet → LAN direct, skip the relay. Forget any
                        // pending relay request so a late grant can't later
                        // clobber the direct carrier.
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_direct(wg, by_node, tun, ctx, np.node_id, &cfg, local_ip, dst)
                            .await;
                    } else if let (Some(ctx), Some(dst)) = (direct_ctx, phase_a_dst) {
                        // Phase A — peer's NIC is public: dial it directly, skip
                        // the relay. Same forget-the-pending-relay guard.
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_public_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
                            .await;
                    } else if let (Some(ctx), Some(dst)) = (direct_ctx, srflx_dst) {
                        // Phase C — both NAT'd: hole-punch the peer's srflx from
                        // the punch socket, skip the relay.
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_srflx_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
                            .await;
                    } else if let Some(coord) = relay.as_mut() {
                        // rc.211 — a carrier for this peer is mid-BUILD off-loop:
                        // post-`try_build` the coordinator no longer tracks it, so
                        // without this guard `!is_tracking` would re-`request` a
                        // DUPLICATE coordination during the 8 s QUIC window.
                        if relay_bq.in_flight.contains_key(&np.node_id) {
                            continue;
                        }
                        if let Some(link) = coord.maybe_complete(np.node_id, &cfg) {
                            let t0 = Instant::now();
                            self.install_ready(wg, by_node, tun, link, relay_bq).await;
                            warn_if_slow("install_ready(maybe_complete)", t0);
                        } else if !coord.is_tracking(&np.node_id) {
                            // Both ends pick the same coturn worker from the
                            // server's symmetric pair_key (in the grant), so no
                            // initiator/responder asymmetry is needed here — see
                            // relay_link.rs. The WG handshake still tie-breaks
                            // the dialer by pubkey in `install_ready`.
                            coord.request(np.node_id, cfg).await;
                        }
                    }
                }
            }
        }
    }

    /// rc.208 make-before-break — start a shadow direct-carrier PROBE for a peer
    /// currently on relay: register the demux + hand the candidate carrier to
    /// [`WgDevice::start_direct_probe`] (its own `Tunn`, NOT in the routing map),
    /// and record the [`UpgradeProbe`] metadata the promote/expire sweep reads.
    /// Does NOT touch `by_node` or the relay allocation — routing stays on relay
    /// until [`Self::sweep_upgrade_probes`] promotes this on a latched handshake.
    #[allow(clippy::too_many_arguments)]
    async fn start_upgrade_probe(
        &self,
        wg: &mut WgDevice,
        upgrade_probes: &mut HashMap<ObjectId, UpgradeProbe>,
        node_id: ObjectId,
        cfg: &PeerConfig,
        sock: Arc<UdpSocket>,
        dst: std::net::SocketAddr,
        tier: DirectTier,
        now: Instant,
    ) {
        wg.ensure_direct_demux(sock.clone());
        // Outbound upgrade: WE dial the peer, so initiate the handshake.
        wg.start_direct_probe(sock, cfg.public_key, cfg.overlay_ip, dst, true)
            .await;
        upgrade_probes.insert(
            node_id,
            UpgradeProbe {
                pubkey: cfg.public_key,
                overlay_ip: cfg.overlay_ip,
                dst,
                tier,
                since: now,
            },
        );
        info!(
            peer = %node_id, overlay_ip = %cfg.overlay_ip, %dst, ?tier,
            "overlay: make-before-break — probing direct upgrade (relay held; cuts over only if the probe handshakes)"
        );
    }

    /// rc.208 make-before-break — drive in-flight upgrade probes each fallback
    /// tick. For each probe: PROMOTE it (swap the direct carrier in, drop the
    /// relay, retag `by_node` as direct, clear the tier's strikes) the moment its
    /// handshake latches; or, past the tier's [`DirectTier::handshake_deadline`],
    /// DROP it (keep the relay, book a tier failure — CC1, like the health
    /// sweep's fallback). The active carrier never stalls either way. No-op when
    /// no probes are in flight.
    async fn sweep_upgrade_probes(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        relay: &mut Option<RelayCoordinator>,
        tun: &Arc<dyn TunIo>,
        upgrade_probes: &mut HashMap<ObjectId, UpgradeProbe>,
        cooldowns: &mut DirectCooldowns,
    ) {
        if upgrade_probes.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut settled: Vec<ObjectId> = Vec::new();
        for (nid, p) in upgrade_probes.iter() {
            if wg.probe_handshake_done(&p.pubkey) == Some(true) {
                // Bidirectional direct proven → cut over. `promote_direct_probe`
                // drops the old relay carrier; forget its coturn allocation and
                // retag `by_node` as the direct tier.
                if wg.promote_direct_probe(&p.pubkey) {
                    if let Some(r) = relay.as_mut() {
                        r.forget(nid);
                    }
                    let off_link = matches!(p.tier, DirectTier::Public | DirectTier::Srflx);
                    by_node.insert(
                        *nid,
                        Installed {
                            pubkey: p.pubkey,
                            overlay_ip: p.overlay_ip,
                            is_direct: true,
                            since: now,
                            last_traffic: (0, 0),
                            bad_sweeps: 0,
                            last_rx_at: now,
                            relay_local: None,
                            relay_dst: None,
                            public_direct_dst: off_link.then_some(p.dst),
                            tier: p.tier,
                        },
                    );
                    tun.add_peer_route(p.overlay_ip).await.ok();
                    // Success clears this tier's accumulated strikes (CC1).
                    match p.tier {
                        DirectTier::Srflx => {
                            cooldowns.srflx_fails.remove(nid);
                        }
                        DirectTier::Public => {
                            cooldowns.public_fails.remove(nid);
                        }
                        _ => {
                            cooldowns.lan_fails.remove(nid);
                        }
                    }
                    info!(
                        peer = %nid, overlay_ip = %p.overlay_ip, tier = ?p.tier,
                        "overlay: make-before-break — direct carrier promoted (relay held throughout; zero stall)"
                    );
                }
                settled.push(*nid);
            } else if now.duration_since(p.since) > p.tier.handshake_deadline() {
                // Probe never latched within the deadline → direct unreachable.
                // Drop it, KEEP the relay, book the failure on the tier (CC1 —
                // mirrors `sweep_carrier_health`'s direct→relay bookkeeping so a
                // repeatedly-failing tier still escalates to its sticky deny and
                // stops re-probing).
                wg.drop_direct_probe(&p.pubkey).await;
                let (count_map, cooldown_map, tier_name) = match p.tier {
                    DirectTier::Srflx => {
                        (&mut cooldowns.srflx_fails, &mut cooldowns.srflx, "srflx")
                    }
                    DirectTier::Public => {
                        (&mut cooldowns.public_fails, &mut cooldowns.public, "public")
                    }
                    _ => (&mut cooldowns.lan_fails, &mut cooldowns.lan, "LAN"),
                };
                let fails = count_map.entry(*nid).or_insert(0);
                *fails += 1;
                cooldown_map.insert(*nid, now + direct_retry_cooldown(p.tier, *fails));
                info!(
                    peer = %nid, tier = tier_name,
                    "overlay: make-before-break — direct probe did not handshake within deadline; kept relay (no stall)"
                );
                settled.push(*nid);
            }
        }
        for nid in settled {
            upgrade_probes.remove(&nid);
        }
    }

    /// rc.134 — install a peer over the SHARED direct-LAN socket (demuxed by
    /// source address, so any number of same-subnet peers coexist — no more
    /// "one direct peer" cap). Both ends initiate (bilateral hole-punch,
    /// rc.133). Adds the `/32` route + records it as `direct` in `by_node`.
    #[allow(clippy::too_many_arguments)]
    async fn install_direct(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        ctx: &DirectCtx,
        node_id: ObjectId,
        cfg: &PeerConfig,
        local_ip: Ipv4Addr,
        dst: std::net::SocketAddr,
    ) {
        // Use the socket bound to the interface that shares the peer's subnet
        // (rc.143) so send/receive stay on the right NIC past a full-tunnel VPN.
        let Some((_, sock)) = ctx.socks.iter().find(|(ip, _)| *ip == local_ip) else {
            warn!(peer = %node_id, %local_ip, "overlay: no socket bound for the matching LAN interface; skipping direct");
            return;
        };
        wg.ensure_direct_demux(sock.clone());
        wg.add_direct_peer(sock.clone(), cfg.public_key, cfg.overlay_ip, dst, true)
            .await;
        by_node.insert(
            node_id,
            Installed {
                pubkey: cfg.public_key,
                overlay_ip: cfg.overlay_ip,
                is_direct: true,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at: Instant::now(),
                relay_local: None,
                relay_dst: None,
                public_direct_dst: None,
                tier: DirectTier::Lan,
            },
        );
        if let Err(e) = tun.add_peer_route(cfg.overlay_ip).await {
            debug!(peer = %node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        // Phase 1 — if this peer is an approved subnet router, route its CIDRs
        // to it (router allowed_ips + OS route).
        self.install_subnets(wg, tun, node_id, cfg.public_key, &cfg.subnets)
            .await;
        info!(peer = %node_id, overlay_ip = %cfg.overlay_ip, %dst, "overlay: direct LAN carrier (same subnet) — skipping relay");
    }

    /// Phase A — install a peer over the **direct-to-public** carrier: dial its
    /// public NIC endpoint over the shared `public_sock` (a `0.0.0.0` socket, so
    /// the OS picks the egress NIC per-destination), demuxed by source like any
    /// direct peer. Bilateral init (a direct carrier initiates on both ends,
    /// `install_ready` semantics — the peer either dials us back symmetrically
    /// or, if NAT'd, accepts our dial and replies over the mapping our INIT
    /// opened). Records `public_direct_dst` so the health sweep tiers it and the
    /// exit-node exemption pins its IP (never self-wedge).
    #[allow(clippy::too_many_arguments)]
    async fn install_public_direct(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        ctx: &DirectCtx,
        node_id: ObjectId,
        cfg: &PeerConfig,
        dst: std::net::SocketAddr,
    ) {
        let Some(sock) = ctx.public_sock.clone() else {
            warn!(peer = %node_id, "overlay: public-direct requested but no egress socket; skipping");
            return;
        };
        wg.ensure_direct_demux(sock.clone());
        wg.add_direct_peer(sock, cfg.public_key, cfg.overlay_ip, dst, true)
            .await;
        by_node.insert(
            node_id,
            Installed {
                pubkey: cfg.public_key,
                overlay_ip: cfg.overlay_ip,
                is_direct: true,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at: Instant::now(),
                relay_local: None,
                relay_dst: None,
                public_direct_dst: Some(dst),
                tier: DirectTier::Public,
            },
        );
        if let Err(e) = tun.add_peer_route(cfg.overlay_ip).await {
            debug!(peer = %node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        self.install_subnets(wg, tun, node_id, cfg.public_key, &cfg.subnets)
            .await;
        info!(peer = %node_id, overlay_ip = %cfg.overlay_ip, %dst, "overlay: direct-to-public carrier (NAT-traversal Phase A) — skipping relay");
    }

    /// Phase C — install a peer over the **srflx hole-punch** carrier: dial its
    /// STUN-learned public mapping from the PUNCH socket (`ctx.punch`, the
    /// interface socket that owns our own first advertised srflx), so our
    /// outbound WG INITs ride the same NAT mapping we advertised — opening our
    /// NAT's filter toward the peer's srflx while the peer's bilateral INITs open
    /// theirs toward ours (the mutual hole-punch). This is the crux difference
    /// from [`install_public_direct`], which dials via the arbitrary-egress
    /// `public_sock`: a punch REQUIRES the mapping-owning socket. Records
    /// `public_direct_dst` (off-link ⇒ exit-node exemption) and `tier = Srflx`
    /// (its own cooldown + the tight handshake deadline). No punch socket (srflx
    /// off / none gathered) ⇒ skip, and the caller falls through to relay.
    #[allow(clippy::too_many_arguments)]
    async fn install_srflx_direct(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        ctx: &DirectCtx,
        node_id: ObjectId,
        cfg: &PeerConfig,
        dst: std::net::SocketAddr,
    ) {
        let Some((_, sock)) = ctx.punch.clone() else {
            warn!(peer = %node_id, "overlay: srflx punch requested but no punch socket; skipping");
            return;
        };
        wg.ensure_direct_demux(sock.clone());
        wg.add_direct_peer(sock, cfg.public_key, cfg.overlay_ip, dst, true)
            .await;
        by_node.insert(
            node_id,
            Installed {
                pubkey: cfg.public_key,
                overlay_ip: cfg.overlay_ip,
                is_direct: true,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at: Instant::now(),
                relay_local: None,
                relay_dst: None,
                public_direct_dst: Some(dst),
                tier: DirectTier::Srflx,
            },
        );
        if let Err(e) = tun.add_peer_route(cfg.overlay_ip).await {
            debug!(peer = %node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        self.install_subnets(wg, tun, node_id, cfg.public_key, &cfg.subnets)
            .await;
        info!(peer = %node_id, overlay_ip = %cfg.overlay_ip, %dst, "overlay: srflx hole-punch carrier (NAT-traversal Phase C) — skipping relay");
    }

    /// Phase A — act on an AUTHENTICATED inbound direct handshake initiation
    /// forwarded by a demux loop ([`crate::overlay::wg::DirectInbound`]): a NAT'd client
    /// dialing our advertised public endpoint (the exit-side accept — we can't
    /// know its NAT'd source ahead of time), or a known peer that restarted /
    /// roamed onto a new ephemeral port. Installs (or re-points) that peer onto
    /// a direct carrier bound to the arriving socket + source, then feeds the
    /// very init back in so the response goes out immediately (no ~5 s wait for
    /// the initiator's retransmit).
    ///
    /// Safety: `wg.authenticate_init` cryptographically proves the sender holds
    /// the claimed key's private half (a forger copying a public key fails), so
    /// this can't be used to hijack a healthy peer's route. Only a pubkey that
    /// maps to a CURRENT netmap peer (server-ACL-authorised) is acted on. A peer
    /// cooling down on the matching tier is left on relay (anti-thrash).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    async fn handle_direct_inbound(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        relay: &mut Option<RelayCoordinator>,
        tun: &Arc<dyn TunIo>,
        current_peers: &HashMap<ObjectId, NetmapPeer>,
        cooldowns: &mut DirectCooldowns,
        upgrade_probes: &mut HashMap<ObjectId, UpgradeProbe>,
        relay_bq: &mut RelayBuildQueue,
        inb: crate::overlay::wg::DirectInbound,
    ) {
        let Some(pubkey) = wg.authenticate_init(&inb.packet) else {
            return; // unparseable / forged — drop
        };
        // Map the authenticated key to a current, ACL-authorised netmap peer.
        let Some(np) = current_peers
            .values()
            .find(|p| super::decode_public(&p.wg_public_key).is_some_and(|k| k == pubkey))
        else {
            debug!(src = %inb.src, "overlay: authenticated inbound init from a non-netmap peer — dropping");
            return;
        };
        let Some(cfg) = peer_config_from_netmap(np) else {
            return;
        };
        let node_id = np.node_id;

        // Already direct on THIS exact source → nothing to change; just answer
        // the init (it may be a keepalive-driven rehandshake).
        if wg.direct_src_of(&pubkey) == Some(inb.src) {
            wg.feed_direct(inb.src, inb.sock.clone(), &inb.packet).await;
            return;
        }

        // Classify the arriving source into a tier. A public source that
        // matches this peer's advertised srflx is a hole-punch (Phase C); any
        // other public source is a direct-to-public dial (Phase A); a private
        // source is a same-LAN roam.
        let now = Instant::now();
        let make_before_break = super::direct::make_before_break_enabled();
        let is_public_src = matches!(inb.src, SocketAddr::V4(v4) if direct::is_public_v4(*v4.ip()));
        let src_str = inb.src.to_string();
        let is_srflx_src = is_public_src && np.srflx_endpoints.iter().any(|e| e.trim() == src_str);
        let tier = if is_srflx_src {
            DirectTier::Srflx
        } else if is_public_src {
            DirectTier::Public
        } else {
            DirectTier::Lan
        };

        // Anti-thrash: honour the matching tier's cooldown — EXCEPT for a srflx
        // punch (D9). An authenticated init that traversed BOTH NATs is proof the
        // pair CAN punch right now, so it overrides the srflx cooldown (which
        // exists only because punches routinely miss) and clears this peer's
        // stale srflx strikes. The LAN/public gates are unchanged.
        let cooling = match tier {
            DirectTier::Srflx => {
                cooldowns.srflx.remove(&node_id);
                cooldowns.srflx_fails.remove(&node_id);
                false
            }
            DirectTier::Public => DirectCooldowns::cooling(&cooldowns.public, &node_id, now),
            _ => DirectCooldowns::cooling(&cooldowns.lan, &node_id, now),
        };
        if cooling {
            return;
        }

        // rc.208 make-before-break (inbound): when enabled and the peer is
        // currently on RELAY, accept the peer's direct init as a SHADOW PROBE
        // (its own `Tunn` in `WgDevice::probes`) and answer the init on it via
        // `feed_direct`, WITHOUT tearing down the relay. `sweep_upgrade_probes`
        // cuts over only once the probe's handshake latches (proof our response
        // reached the peer AND its follow-up reached us — the reverse direction
        // works). If it never latches, the probe is dropped and the relay is
        // untouched — so a peer whose direct init reaches us over a path that
        // can't carry OUR reply (one-way) doesn't cost us the relay.
        if make_before_break {
            // A retransmitted init while we're already probing this src → just
            // answer it and let the in-flight probe keep converging.
            if wg.has_direct_probe(&pubkey) {
                wg.feed_direct(inb.src, inb.sock.clone(), &inb.packet).await;
                return;
            }
            // Only probe when there's a working relay to protect. A fresh peer
            // (nothing installed) or one already on direct-via-another-src falls
            // through to the destructive re-point — no relay is at risk there.
            if by_node.get(&node_id).is_some_and(|e| !e.is_direct) {
                wg.ensure_direct_demux(inb.sock.clone());
                // Inbound: DON'T initiate — the peer already sent the init; we
                // answer it on the probe via `feed_direct` below.
                wg.start_direct_probe(inb.sock.clone(), pubkey, cfg.overlay_ip, inb.src, false)
                    .await;
                wg.feed_direct(inb.src, inb.sock.clone(), &inb.packet).await;
                upgrade_probes.insert(
                    node_id,
                    UpgradeProbe {
                        pubkey,
                        overlay_ip: cfg.overlay_ip,
                        dst: inb.src,
                        tier,
                        since: now,
                    },
                );
                info!(
                    peer = %node_id, src = %inb.src, ?tier,
                    "overlay: make-before-break — accepted inbound direct handshake as a PROBE (relay held)"
                );
                return;
            }
        }

        // Re-point: drop any existing carrier (relay or direct-on-another-src)
        // and any pending relay request, then install direct on the arriving
        // socket keyed by the init's source. `initiate = false` — the peer
        // already initiated; we only need to respond.
        if let Some(old) = by_node.remove(&node_id) {
            wg.remove_peer(&old.pubkey).await;
        }
        if let Some(r) = relay.as_mut() {
            r.forget(&node_id);
        }
        // rc.211 — the direct carrier installed below supersedes any relay
        // build still in flight for this peer; drop it on arrival.
        relay_bq.invalidate(&node_id);
        // rc.208 — if a stale probe lingers for this peer (feature toggled off
        // mid-session, or a direct-on-another-src re-point), discard it so it
        // can't later promote over the carrier we install here.
        if upgrade_probes.remove(&node_id).is_some() {
            wg.drop_direct_probe(&pubkey).await;
        }
        wg.ensure_direct_demux(inb.sock.clone());
        wg.add_direct_peer(inb.sock.clone(), pubkey, cfg.overlay_ip, inb.src, false)
            .await;
        by_node.insert(
            node_id,
            Installed {
                pubkey,
                overlay_ip: cfg.overlay_ip,
                is_direct: true,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at: Instant::now(),
                relay_local: None,
                relay_dst: None,
                // Any OFF-LINK public inbound source is an exit-exemption; a
                // private source is an on-link LAN roam (no exemption). The tier
                // (Srflx punch vs Public dial vs Lan roam) drives cooldown +
                // deadline.
                public_direct_dst: is_public_src.then_some(inb.src),
                tier,
            },
        );
        if let Err(e) = tun.add_peer_route(cfg.overlay_ip).await {
            debug!(peer = %node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        self.install_subnets(wg, tun, node_id, pubkey, &cfg.subnets)
            .await;
        // Answer the init that triggered this, immediately.
        wg.feed_direct(inb.src, inb.sock.clone(), &inb.packet).await;
        info!(peer = %node_id, src = %inb.src, ?tier, "overlay: accepted authenticated inbound direct handshake");
    }

    /// Install a ready carrier as a WG peer, add its `/32` route, and record
    /// it (pubkey + IP) for later removal.
    async fn install_ready(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        link: ReadyLink,
        relay_bq: &mut RelayBuildQueue,
    ) {
        // Handshake direction. RELAY carriers use a deterministic single
        // initiator (the lexicographically smaller pubkey dials; both ends
        // compute it identically) — fine because the relay forwards both ways.
        //
        // rc.133 — DIRECT carriers need BOTH ends to initiate (bilateral
        // hole-punch). A direct WG init is an UNSOLICITED inbound UDP on the
        // responder's PHYSICAL interface, which default Windows Firewall drops
        // (field: two same-LAN hosts, direct carrier built but
        // HANDSHAKE(REKEY_TIMEOUT) forever). When both ends initiate, each
        // side's outbound init opens a stateful firewall hole for the other's
        // inbound, so the handshake completes. The relay path never hit this
        // because its ciphertext rides the agent's OWN outbound TURN
        // connection (already a stateful hole).
        // Optional QUIC-over-TURN upgrade of a relay carrier (opt-in, default
        // OFF via `overlay_quic_enabled`). QUIC's congestion control smooths the
        // relay's buffer-bloat latency spikes and its keepalive holds the TURN
        // permission fresh. On ANY handshake failure/timeout we fall back to the
        // already-built raw relay carrier, so the upgrade can only improve —
        // never break — the link.
        //
        // rc.199 — mutual coturn permission bootstrap for EVERY relay carrier
        // (raw, QUIC, or QUIC-fallback-to-raw). coturn only relays a peer's
        // datagrams to this allocation once it holds a *permission* for that
        // peer's relayed address, and a permission is opened by SENDING to it
        // (the webrtc-rs `turn` client lazily CreatePermission's the dst on the
        // first `send_to`). The relay carrier uses a single WG initiator (the
        // lexicographically-smaller pubkey), so without this the RESPONDER never
        // sends first → its allocation never opens a permission for the initiator
        // → coturn silently drops the WG handshake INIT → HANDSHAKE(REKEY_TIMEOUT)
        // forever. This is exactly why the cross-NAT relay never completed in the
        // field (P5 exit-node bring-up 2026-07-19: relay LINK ready + peer
        // installed, yet the handshake timed out for every peer); the DIRECT path
        // always worked precisely because it needs no coturn permission. Both
        // ends build a carrier and send this stray `\x00`, so BOTH permissions are
        // open before the handshake. `quic_relay` already does its own `\x00`
        // internally (wg.rs); this covers the raw + QUIC-fallback paths, which
        // previously shipped WITHOUT it (the wg.rs relay tests only passed because
        // they do the bootstrap by hand). The 1-byte datagram is below WG's
        // minimum message size, so boringtun ignores it.
        if let Some((conn, dst)) = &link.relay_parts {
            let _ = conn.send_to(b"\x00", *dst).await;
        }
        // Phase D — a single-relay link FORCES the QUIC carrier, ignoring the
        // `OVERLAY_QUIC` opt-in: a raw `Carrier::Relay` discards the recv source
        // (wg.rs recv), so the anchor would reply to the dialer's ADVERTISED
        // srflx port — wrong under a symmetric NAT (per-destination mapping) —
        // and the handshake dies. Only quinn's server consumes the observed
        // path. Symmetric on both ends: any build that advertises
        // `supports_relay_single` carries this rule, so the pair can't split
        // QUIC/raw (see `ReadyLink::single_relay`).
        //
        // A DERP link (`relay_kind == Derp`) is explicitly EXCLUDED from QUIC:
        // it's raw WG over the pubkey-addressed WS relay, and the pubkey pinning
        // makes the raw recv-source discard correct. QUIC-over-DERP would be
        // QUIC-over-TCP (double-reliable, HOL-on-HOL) and is untested — v1 stays
        // raw (A2). The gate below is belt-and-suspenders: a DERP link already
        // sets `single_relay: None` + `supports_quic: false`, but the explicit
        // `Turn` check keeps a future field-add from silently upgrading it.
        let want_quic = link.relay_parts.is_some()
            && link.relay_kind == RelayKind::Turn
            && (link.single_relay.is_some() || (overlay_quic_enabled() && link.supports_quic));
        if want_quic {
            // rc.211 — the QUIC-over-TURN rendezvous (up to QUIC_BUILD_TIMEOUT
            // = 8 s) runs OFF-LOOP: awaiting it here head-of-line-blocked the
            // `tun.read_packet()` arm for its full duration — the field-proven
            // 1–2 s overlay RTT plateaus (S1 watchdog: five 8.06 s stalls named
            // `install_ready(quic-build)` in one 150 s run on a churny host).
            // The spawned build sends its result to the `built_rx` select! arm,
            // which commits via `install_built` (µs). `quic_relay` sends the
            // `\x00` permission bootstrap itself; on failure the builder sends
            // one for the raw fallback (mirrors the pre-split inline probe).
            //
            // QUIC role. For a SINGLE-RELAY link the ANCHOR must serve — its
            // allocation is the rendezvous, and only the server-on-the-
            // allocation replies to coturn's observed sources. With UDP-aware
            // anchor selection the anchor may hold the LARGER pubkey, so the
            // pubkey rule would invert the roles and deadlock (the anchor
            // would QUIC-connect toward the dialer's srflx, which that
            // socket's NAT filter drops). Both-allocate keeps the pubkey rule
            // (deterministic, both ends agree; either allocation can serve).
            let (conn, dst) = link.relay_parts.clone().unwrap();
            let am_server = match link.single_relay {
                Some(anchor) => anchor,
                None => self.keypair.public.to_bytes() < link.public_key,
            };
            let min_datagram = self.mtu as usize + WG_OVERHEAD;
            let epoch = relay_bq.stamp(link.node_id);
            let tx = relay_bq.tx.clone();
            tokio::spawn(async move {
                let quic = match Carrier::quic_relay(
                    conn.clone(),
                    dst,
                    am_server,
                    min_datagram,
                    QUIC_BUILD_TIMEOUT,
                )
                .await
                {
                    Ok(q) => {
                        info!(peer = %link.node_id, %dst, am_server, "overlay: QUIC-over-TURN carrier up");
                        Some(q)
                    }
                    Err(e) => {
                        // For a single-relay link the raw fallback only carries
                        // for cone-ish dialers (port-preserving mapping); a
                        // symmetric dialer stays dark until the health sweep
                        // re-coordinates.
                        warn!(peer = %link.node_id, %e, single_relay = ?link.single_relay,
                              "overlay: QUIC carrier build failed; using raw relay");
                        // Permission bootstrap for the raw fallback (the QUIC
                        // attempt sent its own, but re-assert — it's 1 byte).
                        let _ = conn.send_to(b"\x00", dst).await;
                        None
                    }
                };
                // Receiver dropped ⇒ runtime exited; the build is moot.
                let _ = tx.send(BuiltRelay { epoch, link, quic }).await;
            });
            return;
        }
        self.install_built(wg, by_node, tun, link, None).await;
    }

    /// rc.211 — commit an already-BUILT relay/test carrier as a WG peer: the
    /// µs-fast install half of the old `install_ready` (`wg.add_peer` + `/32`
    /// route + subnets + bookkeeping). `quic: Some` = the off-loop QUIC build
    /// succeeded; `None` = raw carrier (no-QUIC link, or QUIC fallback).
    async fn install_built(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        link: ReadyLink,
        quic: Option<Arc<Carrier>>,
    ) {
        let (relay_local, relay_dst) = match &link.relay_parts {
            Some((conn, dst)) => (conn.local_addr().ok(), Some(*dst)),
            None => (None, None),
        };
        let carrier = quic.unwrap_or_else(|| link.carrier.clone());
        let initiate = carrier.is_direct() || self.keypair.public.to_bytes() < link.public_key;
        let is_direct = carrier.is_direct();
        wg.add_peer(link.public_key, link.overlay_ip, carrier, initiate);
        by_node.insert(
            link.node_id,
            Installed {
                pubkey: link.public_key,
                overlay_ip: link.overlay_ip,
                is_direct,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at: Instant::now(),
                relay_local,
                relay_dst,
                public_direct_dst: None,
                // A relay carrier, or the loopback carrier used in Direct/test
                // mode. Test loopback carriers are direct → Lan (no off-link
                // handshake deadline); coturn carriers → Relay.
                tier: if is_direct {
                    DirectTier::Lan
                } else {
                    DirectTier::Relay
                },
            },
        );
        // Host `/32` so overlay traffic to this peer beats any colliding
        // less-specific route on the uplink (e.g. a carrier CGNAT /10).
        // Best-effort — clean hosts route fine via the connected /10.
        if let Err(e) = tun.add_peer_route(link.overlay_ip).await {
            debug!(peer = %link.node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        // Phase 1 — subnet-router peer: route its approved CIDRs to it.
        self.install_subnets(wg, tun, link.node_id, link.public_key, &link.subnets)
            .await;
        info!(peer = %link.node_id, overlay_ip = %link.overlay_ip, initiate, "overlay: peer installed");
    }

    /// Phase 1 — register a peer's approved subnet routes in the crypto-router
    /// (so packets to those CIDRs encapsulate to it) and install the matching OS
    /// routes via the overlay NIC. No-op when the peer advertised none.
    async fn install_subnets(
        &self,
        wg: &mut WgDevice,
        tun: &Arc<dyn TunIo>,
        node_id: ObjectId,
        pubkey: [u8; 32],
        subnets: &[super::router::Cidr],
    ) {
        // P5/A1 — the generic subnet-install path NEVER installs a default route.
        // Approving `0.0.0.0/0` on an exit node fans it into every peer's netmap
        // `routes`; without this filter each client would install it here
        // unconditionally — into the crypto-router's allowed_ips AND an OS default
        // route — hijacking the whole fleet's egress with zero opt-in. Default
        // routing toward a CHOSEN exit node is a separate, opt-in path
        // (split-default `/1`s + carrier-endpoint exemptions) that never flows
        // through this generic installer.
        let filtered: Vec<super::router::Cidr> = subnets
            .iter()
            .copied()
            .filter(|c| !c.is_default_route())
            .collect();
        if filtered.len() != subnets.len() {
            warn!(
                peer = %node_id,
                "overlay: dropped advertised default route(s) from a peer's subnets \
                 (exit-node routing is opt-in — a /0 is never auto-installed)"
            );
        }
        wg.set_peer_subnets(pubkey, &filtered);
        for c in &filtered {
            let cidr = c.to_string();
            if let Err(e) = tun.add_cidr_route(&cidr).await {
                debug!(peer = %node_id, %cidr, %e, "overlay: subnet route not installed");
            } else {
                info!(peer = %node_id, %cidr, "overlay: subnet route installed (router peer)");
            }
        }
    }

    /// P5 exit-node — reconcile default-route capture toward the chosen exit
    /// peer. No-op unless `exit_node` is configured. Idempotent + safe to call
    /// after every carrier change: it pins any newly-needed carrier exemptions
    /// FIRST, and only once EVERY required endpoint is exempted does it install
    /// the split-default — so a missing exemption can never sever the very tunnel
    /// that carries the mesh + coordination path (the load-bearing bootstrap
    /// safety, R1/D3). Withdraws the capture (egress reverts to the never-deleted
    /// OS default) if the chosen peer leaves, loses its carrier/approval, or an
    /// exemption can't be pinned.
    async fn reconcile_exit_routing(
        &self,
        wg: &mut WgDevice,
        tun: &Arc<dyn TunIo>,
        by_node: &HashMap<ObjectId, Installed>,
        current_peers: &HashMap<ObjectId, NetmapPeer>,
        state: &mut ExitRoutingState,
    ) {
        let Some(selector) = self.exit_node.as_deref() else {
            return; // not an exit-node client — inert
        };

        // The chosen exit must be present, reachable-with-a-live-carrier, AND an
        // admin-approved exit node. Any miss → withdraw the capture, record the
        // (distinct) split-tunnel reason for `roomler status`, and wait.
        let (exit_id, exit_np, exit_pubkey) = match exit_readiness(selector, current_peers, by_node)
        {
            Ok(v) => v,
            Err(reason) => {
                if state.split_default_installed {
                    self.teardown_exit_routing(wg, tun, by_node, current_peers, state)
                        .await;
                }
                note_withheld(state, selector, reason);
                return;
            }
        };

        // Pin any exemptions we don't yet hold, BEFORE (re)installing the /1s —
        // the coturn set grows as relay carriers appear, so re-run on churn.
        let want = exit_exemption_set(&self.exit_server_ips, by_node);
        for ip in &want {
            if state.exemptions.contains(ip) {
                continue;
            }
            match tun.add_host_exemption(*ip).await {
                Ok(()) => {
                    state.exemptions.insert(*ip);
                    info!(%ip, "overlay exit-node: pinned carrier-endpoint exemption via the original uplink");
                }
                Err(e) => {
                    warn!(%ip, %e, "overlay exit-node: FAILED to pin a carrier exemption — withholding default routing to avoid a self-wedge");
                }
            }
        }

        // BLOCKER-1 safety gate — the v4 split-default gates ONLY on the v4
        // exemptions (server A-records + coturn workers). A v6-exemption failure
        // (e.g. roomler.ai has an AAAA but this host has no v6 default route, so
        // its `/128` can't pin) must NEVER withhold v4 exit — v6 is handled
        // separately below and simply stays fail-closed. Without this split, a
        // pure-v6 feature would regress shipped v4 the moment roomler.ai gained
        // an AAAA.
        let v4_ok = want
            .iter()
            .filter(|ip| ip.is_ipv4())
            .all(|ip| state.exemptions.contains(ip));
        if !v4_ok {
            if state.split_default_installed {
                self.teardown_exit_routing(wg, tun, by_node, current_peers, state)
                    .await;
            }
            note_withheld(
                state,
                selector,
                "carrier-endpoint exemption unavailable (no original default route?)",
            );
            return;
        }

        // Install (or move) the split-default toward the exit peer (idempotent):
        // the two v4 /1 halves into its WG allowed_ips + as OS routes, plus the
        // two v6 /1 halves as OS routes into the overlay NIC. The v6 halves either
        // FORWARD (v6 egress) or blackhole (fail-closed) depending on `v6_exit`,
        // set below — the routes themselves are identical either way.
        if !(state.split_default_installed && state.active_peer == Some(exit_id)) {
            let allowed = exit_peer_allowed_ips(&exit_np);
            wg.set_peer_subnets(exit_pubkey, &allowed);
            for cidr in SPLIT_DEFAULT_V4.iter().chain(SPLIT_DEFAULT_V6.iter()) {
                if let Err(e) = tun.add_cidr_route(cidr).await {
                    warn!(%cidr, %e, "overlay exit-node: split-default route not installed");
                }
            }
            state.split_default_installed = true;
            state.active_peer = Some(exit_id);
            state.withheld_reason = None;
            info!(peer = %exit_id, exit = %selector, "overlay exit-node: v4 default egress now routes through the exit peer");
        }

        // S3b — global IPv6 egress, INDEPENDENT of v4 and re-asserted every
        // reconcile so a `remove_peer`-clear during a relay↔direct carrier
        // reinstall self-repairs (MAJOR-3). Enable only when EVERY v6 exemption
        // (the coordination server's AAAA) is pinned, so the WS-over-v6 control
        // channel stays direct (MAJOR-1). Otherwise `v6_exit=None` keeps the `::/1`
        // routes as a fail-closed blackhole — v6 never leaks, and v4 is unaffected.
        let v6_ok = want
            .iter()
            .filter(|ip| ip.is_ipv6())
            .all(|ip| state.exemptions.contains(ip));
        if v6_ok {
            wg.set_v6_exit(Some(exit_pubkey));
            if state.v6_active != Some(true) {
                state.v6_active = Some(true);
                info!(peer = %exit_id, "overlay exit-node: global IPv6 egress now routes through the exit peer");
            }
        } else {
            wg.set_v6_exit(None);
            if state.v6_active != Some(false) {
                state.v6_active = Some(false);
                warn!(exit = %selector, "overlay exit-node: IPv6 egress WITHHELD (no v6 uplink to exempt the coordination server) — v6 stays fail-closed while v4 routes through the exit");
            }
        }

        // S4b — exit-node DNS steering, coupled to the v4 split-default so DNS can
        // never route to the exit while egress doesn't. Idempotent (`!dns_steered`).
        // When MagicDNS is on, gated on a live local resolver (`dns_bound`, known
        // before the first reconcile) — steering "." at a dead :53 would blackhole
        // ALL DNS. A not-bound resolver is left unsteered (working local DNS beats a
        // blackhole) and surfaced via `dns_steered=false` in `roomler status`.
        if state.split_default_installed
            && !state.dns_steered
            && (state.dns_magic_domain.is_none() || state.dns_bound)
            && let Some(target) = state.dns_target
        {
            if dns::steer_default_dns(target, state.dns_magic_domain.as_deref()).await {
                state.dns_steered = true;
                info!(exit = %selector, "overlay exit-node: DNS now steers all queries through the exit (no DNS leak)");
            } else {
                debug!(exit = %selector, "overlay exit-node: DNS steer command failed (resolvectl/NRPT unavailable?) — DNS NOT steered");
            }
        }
        debug_assert!(
            !state.dns_steered
                || (state.split_default_installed
                    && (state.dns_magic_domain.is_none() || state.dns_bound)),
            "exit-node DNS steered without an active split-default + a live local resolver"
        );
    }

    /// P5 exit-node — revert everything [`reconcile_exit_routing`] installed:
    /// drop the split-default OS routes, reset the (former) exit peer's WG
    /// `allowed_ips` back to its real subnets (so it keeps working as a normal /
    /// subnet-router peer), and remove the carrier exemptions. Idempotent. NB:
    /// `process::exit` paths (watchdog stall, self-update) bypass this — a
    /// synchronous pre-exit cleanup + a boot-time stale-route reconciler are the
    /// A2 follow-up (S3.5); the split-default self-heals regardless (the OS
    /// default was never deleted).
    async fn teardown_exit_routing(
        &self,
        wg: &mut WgDevice,
        tun: &Arc<dyn TunIo>,
        by_node: &HashMap<ObjectId, Installed>,
        current_peers: &HashMap<ObjectId, NetmapPeer>,
        state: &mut ExitRoutingState,
    ) {
        if !state.split_default_installed
            && state.exemptions.is_empty()
            && state.active_peer.is_none()
            && !state.dns_steered
        {
            return;
        }
        for cidr in SPLIT_DEFAULT_V4.iter().chain(SPLIT_DEFAULT_V6.iter()) {
            tun.del_cidr_route(cidr).await;
        }
        // Reset the former exit peer's allowed_ips to its real subnets, if it's
        // still installed + in the netmap (else its Tunn is already gone).
        if let Some(id) = state.active_peer
            && let (Some(inst), Some(np)) = (by_node.get(&id), current_peers.get(&id))
        {
            let real: Vec<super::router::Cidr> = peer_config_from_netmap(np)
                .map(|c| c.subnets)
                .unwrap_or_default()
                .into_iter()
                .filter(|c| !c.is_default_route())
                .collect();
            wg.set_peer_subnets(inst.pubkey, &real);
        }
        for ip in state.exemptions.drain() {
            tun.del_host_exemption(ip).await;
        }
        // S3b — stop routing global v6 to the (now former) exit; global v6 reverts
        // to the physical uplink once the `::/1` routes above are removed.
        wg.set_v6_exit(None);
        // S4b — revert DNS steering (drop the "." catch-all). With MagicDNS on the
        // P2 suffix rule stays, so overlay names keep resolving; otherwise the
        // physical resolver is restored.
        if state.dns_steered {
            dns::unsteer_default_dns(state.dns_magic_domain.as_deref()).await;
            state.dns_steered = false;
        }
        state.split_default_installed = false;
        state.active_peer = None;
        state.v6_active = None;
        info!("overlay exit-node: default routing torn down; egress reverted to the local uplink");
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

    /// rc.211 — the off-loop build queue's staleness guards. (a) A completion
    /// commits only while its epoch is current; (b) `invalidate` (peer removed /
    /// went direct) drops the in-flight build on arrival; (c) re-`stamp` for the
    /// same peer supersedes the old build — the ABA case a plain "is building"
    /// set would get wrong (old completion must NOT commit, new one must).
    #[tokio::test]
    async fn relay_build_queue_epoch_guards() {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let dst: SocketAddr = sock.local_addr().unwrap();
        let mk = |node, epoch| BuiltRelay {
            epoch,
            link: ReadyLink {
                node_id: node,
                public_key: [0u8; 32],
                overlay_ip: std::net::Ipv4Addr::new(100, 64, 0, 9),
                carrier: Arc::new(Carrier::Direct {
                    sock: sock.clone(),
                    dst,
                }),
                relay_parts: None,
                supports_quic: false,
                single_relay: None,
                relay_kind: RelayKind::Turn,
                subnets: vec![],
            },
            quic: None,
        };
        let mut bq = test_relay_bq();
        let n = ObjectId::from_bytes([9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        // (a) current epoch commits, and the slot is consumed.
        let e1 = bq.stamp(n);
        assert!(bq.in_flight.contains_key(&n));
        assert!(bq.take_if_current(&mk(n, e1)));
        assert!(!bq.in_flight.contains_key(&n), "commit consumes the slot");
        assert!(
            !bq.take_if_current(&mk(n, e1)),
            "a second arrival of the same build is stale"
        );

        // (b) invalidate → the completion is dropped on arrival.
        let e2 = bq.stamp(n);
        bq.invalidate(&n);
        assert!(!bq.take_if_current(&mk(n, e2)));

        // (c) ABA: re-stamp supersedes — the OLD build must not commit, the
        // NEW one must.
        let e3 = bq.stamp(n);
        let e4 = bq.stamp(n);
        assert!(!bq.take_if_current(&mk(n, e3)), "superseded build is stale");
        assert!(bq.take_if_current(&mk(n, e4)), "current build commits");
    }

    #[test]
    fn direct_cooldown_escalates_to_sticky_after_repeated_failures() {
        // The VPN-pool relay↔direct anti-flap fix: the 1st direct failure gets
        // the normal 60 s retry, but once a peer hits DIRECT_MAX_FAILURES the
        // cooldown becomes session-sticky so it stops re-upgrading the working
        // relay to a direct carrier that can never complete.
        // `direct_retry_cooldown(_, 1) == DIRECT_COOLDOWN` only holds when
        // DIRECT_MAX_FAILURES >= 2, so this also guards that invariant (at least
        // one plain retry before the sticky pin).
        assert_eq!(
            direct_retry_cooldown(DirectTier::Public, 1),
            DIRECT_COOLDOWN
        );
        assert_eq!(
            direct_retry_cooldown(DirectTier::Public, DIRECT_MAX_FAILURES),
            DIRECT_DENY_COOLDOWN
        );
        assert_eq!(
            direct_retry_cooldown(DirectTier::Lan, DIRECT_MAX_FAILURES + 3),
            DIRECT_DENY_COOLDOWN
        );
        // Phase C — the srflx tier has its OWN thresholds: one MORE plain retry
        // (SRFLX_MAX_FAILURES = 3 > 2), and a SHORTER, non-24 h deny (NAT
        // conditions change on roam).
        assert_eq!(direct_retry_cooldown(DirectTier::Srflx, 1), DIRECT_COOLDOWN);
        assert_eq!(
            direct_retry_cooldown(DirectTier::Srflx, SRFLX_MAX_FAILURES - 1),
            DIRECT_COOLDOWN,
            "srflx gets an extra plain retry vs LAN/public"
        );
        assert_eq!(
            direct_retry_cooldown(DirectTier::Srflx, SRFLX_MAX_FAILURES),
            SRFLX_DENY_COOLDOWN
        );
        assert!(
            SRFLX_DENY_COOLDOWN < DIRECT_DENY_COOLDOWN,
            "srflx deny must be shorter than the LAN/public session-sticky deny"
        );
        // The deadlines are ordered srflx/LAN (tight) < public (loose); relay
        // has none (governed by its own hard-dead/one-way signals). rc.204 —
        // LAN gained a deadline: a false same-subnet match must demote, not
        // zombie forever.
        assert!(SRFLX_HANDSHAKE_DEADLINE < PUBLIC_HANDSHAKE_DEADLINE);
        assert!(LAN_HANDSHAKE_DEADLINE < PUBLIC_HANDSHAKE_DEADLINE);
        assert!(
            LAN_HANDSHAKE_DEADLINE > DIRECT_GRACE,
            "a blown LAN deadline must land past the warm-up grace"
        );
        assert_eq!(DirectTier::Lan.handshake_deadline(), LAN_HANDSHAKE_DEADLINE);
        assert_eq!(DirectTier::Relay.handshake_deadline(), Duration::MAX);
        assert_eq!(direct_max_failures(DirectTier::Srflx), SRFLX_MAX_FAILURES);
        assert_eq!(direct_max_failures(DirectTier::Public), DIRECT_MAX_FAILURES);
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
                pubkey: peer_kp.public.to_bytes(),
                overlay_ip,
                is_direct: true,
                // Installed past the srflx handshake deadline (and the grace).
                since: Instant::now()
                    .checked_sub(Duration::from_secs(SRFLX_HANDSHAKE_DEADLINE.as_secs() + 3))
                    .unwrap(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at: Instant::now(),
                relay_local: None,
                relay_dst: None,
                public_direct_dst: Some(dead),
                tier: DirectTier::Srflx,
            },
        );

        let tun: Arc<dyn TunIo> = tun_mock;
        let mut cooldowns = DirectCooldowns::default();
        let mut relay_refresh: HashMap<ObjectId, Instant> = HashMap::new();
        let mut relay: Option<RelayCoordinator> = None;
        let current_peers: HashMap<ObjectId, NetmapPeer> = HashMap::new();

        rt.sweep_carrier_health(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut cooldowns,
            &mut relay_refresh,
            &current_peers,
        )
        .await;

        assert!(
            !by_node.contains_key(&nid),
            "the zombie srflx carrier is torn down"
        );
        assert!(
            cooldowns.srflx.contains_key(&nid),
            "the srflx cooldown is set"
        );
        assert_eq!(
            cooldowns.srflx_fails.get(&nid),
            Some(&1),
            "one srflx strike"
        );
        assert!(
            !cooldowns.lan.contains_key(&nid) && !cooldowns.lan_fails.contains_key(&nid),
            "CC1: the LAN tier is NOT poisoned"
        );
        assert!(
            !cooldowns.public.contains_key(&nid) && !cooldowns.public_fails.contains_key(&nid),
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
                pubkey: peer_kp.public.to_bytes(),
                overlay_ip,
                is_direct: true,
                // Installed past the LAN handshake deadline (and the grace).
                since: Instant::now()
                    .checked_sub(Duration::from_secs(LAN_HANDSHAKE_DEADLINE.as_secs() + 3))
                    .unwrap(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at: Instant::now(),
                relay_local: None,
                relay_dst: None,
                public_direct_dst: None,
                tier: DirectTier::Lan,
            },
        );

        let tun: Arc<dyn TunIo> = tun_mock;
        let mut cooldowns = DirectCooldowns::default();
        let mut relay_refresh: HashMap<ObjectId, Instant> = HashMap::new();
        let mut relay: Option<RelayCoordinator> = None;
        let current_peers: HashMap<ObjectId, NetmapPeer> = HashMap::new();

        rt.sweep_carrier_health(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut cooldowns,
            &mut relay_refresh,
            &current_peers,
        )
        .await;

        assert!(
            !by_node.contains_key(&nid),
            "the zombie LAN carrier is torn down"
        );
        assert!(cooldowns.lan.contains_key(&nid), "the LAN cooldown is set");
        assert_eq!(cooldowns.lan_fails.get(&nid), Some(&1), "one LAN strike");
        assert!(
            !cooldowns.srflx.contains_key(&nid) && !cooldowns.srflx_fails.contains_key(&nid),
            "CC1: the srflx tier is NOT poisoned"
        );
        assert!(
            !cooldowns.public.contains_key(&nid) && !cooldowns.public_fails.contains_key(&nid),
            "CC1: the public-direct tier is NOT poisoned"
        );
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
            Installed {
                pubkey: pk,
                overlay_ip,
                is_direct: false,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at: Instant::now(),
                relay_local: None,
                relay_dst: None,
                public_direct_dst: None,
                tier: DirectTier::Relay,
            },
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

        let mut cooldowns = DirectCooldowns::default();
        cooldowns.srflx_fails.insert(nid, 2); // stale strikes, should clear on success
        let mut relay: Option<RelayCoordinator> = None;

        rt.sweep_upgrade_probes(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut upgrade_probes,
            &mut cooldowns,
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
        assert!(
            !cooldowns.srflx_fails.contains_key(&nid),
            "success clears the tier's strikes"
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

        let mut cooldowns = DirectCooldowns::default();
        let mut relay: Option<RelayCoordinator> = None;

        rt.sweep_upgrade_probes(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut upgrade_probes,
            &mut cooldowns,
        )
        .await;

        assert!(upgrade_probes.is_empty(), "the probe settled");
        assert_eq!(wg.probe_count(), 0, "the failed probe was dropped");
        let inst = by_node.get(&nid).expect("relay carrier kept");
        assert!(!inst.is_direct, "the RELAY carrier is untouched (no stall)");
        assert_eq!(inst.tier, DirectTier::Relay);
        assert_eq!(
            cooldowns.srflx_fails.get(&nid),
            Some(&1),
            "one srflx strike booked"
        );
        assert!(
            cooldowns.srflx.contains_key(&nid),
            "the srflx cooldown is set"
        );
    }

    /// Make-before-break — while a probe is in flight (not yet latched, within
    /// the deadline) the sweep is a no-op: the probe stays, and the peer keeps
    /// routing over its relay carrier.
    #[tokio::test(flavor = "multi_thread")]
    async fn mbb_holds_relay_while_probe_in_flight() {
        let (rt, mut wg, tun, mut by_node, mut upgrade_probes, nid, _pk) =
            mbb_fixture(DirectTier::Srflx, Instant::now()).await;
        let mut cooldowns = DirectCooldowns::default();
        let mut relay: Option<RelayCoordinator> = None;

        rt.sweep_upgrade_probes(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut upgrade_probes,
            &mut cooldowns,
        )
        .await;

        assert_eq!(upgrade_probes.len(), 1, "still probing");
        assert_eq!(wg.probe_count(), 1, "the probe is still in flight");
        assert!(
            !by_node.get(&nid).unwrap().is_direct,
            "the relay is still the routing carrier"
        );
        assert!(
            cooldowns.srflx_fails.is_empty(),
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
        let relay_installed = || Installed {
            pubkey: peer_kp.public.to_bytes(),
            overlay_ip: Ipv4Addr::new(100, 64, 0, 7),
            is_direct: false,
            since: Instant::now(),
            last_traffic: (0, 0),
            bad_sweeps: 0,
            last_rx_at: Instant::now(),
            relay_local: None,
            relay_dst: None,
            public_direct_dst: None,
            tier: DirectTier::Relay,
        };
        let mut cooldowns = DirectCooldowns::default();
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
            &mut cooldowns,
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
            &mut cooldowns,
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
                pubkey: peer_kp.public.to_bytes(),
                overlay_ip,
                is_direct: true,
                since: stale,
                last_traffic: snap,
                bad_sweeps: 0,
                last_rx_at: stale,
                relay_local: None,
                relay_dst: None,
                public_direct_dst: Some(dst),
                tier: DirectTier::Srflx,
            },
        );

        let tun: Arc<dyn TunIo> = tun_mock;
        let mut cooldowns = DirectCooldowns::default();
        let mut relay_refresh: HashMap<ObjectId, Instant> = HashMap::new();
        let mut relay: Option<RelayCoordinator> = None;
        let current_peers: HashMap<ObjectId, NetmapPeer> = HashMap::new();

        rt.sweep_carrier_health(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut cooldowns,
            &mut relay_refresh,
            &current_peers,
        )
        .await;

        assert!(
            !by_node.contains_key(&nid),
            "the silent established carrier is torn down via rx-staleness"
        );
        assert!(
            cooldowns.srflx.contains_key(&nid),
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
                pubkey: peer_kp.public.to_bytes(),
                overlay_ip,
                is_direct: true,
                since: old,
                last_traffic: snap,
                bad_sweeps: 0,
                last_rx_at: old,
                relay_local: None,
                relay_dst: None,
                public_direct_dst: Some(dst),
                tier: DirectTier::Srflx,
            },
        );

        let tun: Arc<dyn TunIo> = tun_mock;
        let mut cooldowns = DirectCooldowns::default();
        let mut relay_refresh: HashMap<ObjectId, Instant> = HashMap::new();
        let mut relay: Option<RelayCoordinator> = None;
        let current_peers: HashMap<ObjectId, NetmapPeer> = HashMap::new();

        rt.sweep_carrier_health(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &mut cooldowns,
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
        assert!(
            cooldowns.srflx.is_empty() && cooldowns.srflx_fails.is_empty(),
            "no failure is booked for a healthy carrier"
        );
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
        let cooldowns = DirectCooldowns::default();
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
            &cooldowns,
            &mut HashMap::new(),
            &mut test_relay_bq(),
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
            &cooldowns,
            &mut HashMap::new(),
            &mut test_relay_bq(),
        )
        .await;
        let inst = by_node
            .get(&lan_peer.node_id)
            .expect("the pure-bucket LAN candidate installs a LAN carrier");
        assert_eq!(inst.tier, DirectTier::Lan);
        assert!(inst.is_direct);
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
                pubkey: [0u8; 32],
                overlay_ip: ip,
                is_direct,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at,
                relay_local: relay.map(|(l, _)| l),
                relay_dst: relay.map(|(_, d)| d),
                public_direct_dst: None,
                tier: if is_direct {
                    DirectTier::Lan
                } else {
                    DirectTier::Relay
                },
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

        let view = build_overlay_view("100.64.0.9", &by_node, &current, now, epoch_now_ms);
        assert_eq!(view.self_ip.as_deref(), Some("100.64.0.9"));
        assert_eq!(view.peers.len(), 4);
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
                pubkey: [0u8; 32],
                overlay_ip: Ipv4Addr::new(100, 64, 0, 1),
                is_direct,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at: Instant::now(),
                relay_local: relay.map(|(l, _)| l),
                relay_dst: relay.map(|(_, d)| d),
                public_direct_dst: None,
                tier: if is_direct {
                    DirectTier::Lan
                } else {
                    DirectTier::Relay
                },
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
                pubkey: [1u8; 32],
                overlay_ip: Ipv4Addr::new(100, 64, 0, 9),
                is_direct: true,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at: Instant::now(),
                relay_local: None,
                relay_dst: None,
                public_direct_dst: Some(pd),
                tier: DirectTier::Public,
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
            Installed {
                pubkey: [7u8; 32],
                overlay_ip: Ipv4Addr::new(100, 64, 0, 1),
                is_direct: true,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
                last_rx_at: Instant::now(),
                relay_local: None,
                relay_dst: None,
                public_direct_dst: None,
                tier: DirectTier::Lan,
            },
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
