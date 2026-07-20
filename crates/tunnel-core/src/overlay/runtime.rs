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
use super::relay_link::{ReadyLink, RelayCoordinator};
use super::tun::TunIo;
use super::wg::{Carrier, QUIC_BUILD_TIMEOUT, WG_OVERHEAD, WgDevice, overlay_quic_enabled};
use crate::localapi::{ConnectionType, ExitNodeStatus, OverlayView, PeerInfo};
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
}

impl DirectCooldowns {
    /// Is `nid` currently cooling down on the given tier?
    fn cooling(map: &HashMap<ObjectId, Instant>, nid: &ObjectId, now: Instant) -> bool {
        map.get(nid).is_some_and(|&until| until > now)
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
    /// Monotonic instant the peer's rx IP-packet count last advanced — a real
    /// "last seen from this peer" (P3b-3). Seeded to `since` at install;
    /// advanced by `sweep_carrier_health` whenever rx climbs. Converted to an
    /// absolute epoch-ms `last_seen_ms` in `build_overlay_view`. Sweep cadence
    /// (`FALLBACK_TICK`, ~5 s) sets the granularity — fine for a human
    /// "Ns/Nm ago" column, and passive keepalives keep it fresh for live peers.
    last_rx_at: Instant,
    /// rc.187 — for a RELAY carrier: our own coturn-relayed address (the worker
    /// we allocated on) and the peer's relayed address we dial. `None` for a
    /// direct carrier. Surfaced in the LocalAPI `peers` view so an operator can
    /// see — without a debug-log hunt — which coturn worker each end pinned and
    /// whether a relay pair is same-worker (IPs equal) or cross-worker.
    relay_local: Option<std::net::SocketAddr>,
    relay_dst: Option<std::net::SocketAddr>,
    /// Phase A — for a PUBLIC-DIRECT carrier, the peer's public `ip:port` we
    /// dial (or that we accepted an inbound dial from). `None` for a same-LAN
    /// direct carrier or a relay carrier. Two loads ride on this: (1) it marks
    /// the carrier as the public-direct tier for the health sweep's tier-split
    /// fallback (CC1), and (2) it is a MANDATORY exit-node exemption — a
    /// public-direct dst is a real internet address reached via the default
    /// route, NOT on-link like a same-LAN peer, so the split-default `/1`s would
    /// capture the very path to the exit and self-wedge unless its IP is pinned
    /// via the original gateway (see [`exit_exemption_set`]).
    public_direct_dst: Option<std::net::SocketAddr>,
}

/// Grace after install before the fallback can fire — lets the bilateral
/// handshake + first packets flow before we judge the carrier.
const DIRECT_GRACE: Duration = Duration::from_secs(8);
/// Consecutive bad sweeps (sent, received nothing) before falling back. At the
/// 5 s tick that's ~15 s of one-way traffic — long enough to ignore a blip,
/// short enough that a VPN/AP-isolation break doesn't stay dark for long.
const BAD_SWEEPS_TO_FALLBACK: u32 = 3;
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

/// The cooldown to apply after a direct-carrier failure. Escalates to the
/// session-sticky [`DIRECT_DENY_COOLDOWN`] once a peer has failed direct
/// [`DIRECT_MAX_FAILURES`] times (a persistent false /24 match — a VPN client
/// pool — rather than a transient blip). `fails` is the running failure count
/// INCLUDING the current failure.
fn direct_retry_cooldown(fails: u32) -> Duration {
    if fails >= DIRECT_MAX_FAILURES {
        DIRECT_DENY_COOLDOWN
    } else {
        DIRECT_COOLDOWN
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
}

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
        }
    }

    /// Phase 1 — set the subnet routes this node advertises as a router.
    pub fn with_advertised_routes(mut self, routes: Vec<String>) -> Self {
        self.advertised_routes = routes;
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
    pub async fn run(self, mut events: mpsc::Receiver<OverlayEvent>, endpoints: Vec<String>) {
        // rc.131 — direct LAN path: bind a shared UDP socket + discover our
        // LAN endpoint so a same-subnet peer dials us directly and skips the
        // relay. Off in Direct mode (the test/helper path) and when disabled.
        let direct_ctx = self.setup_direct().await;
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
        // Phase A — receiver for AUTHENTICATED inbound direct handshakes (a
        // NAT'd client dialing our public endpoint, or a known peer that roamed
        // to a new ephemeral port — the field-observed stale-port race). Only
        // wired when the public-direct tier is on (CC8 flag-gate); the demux
        // loops for our own sockets are started EAGERLY here so an inbound INIT
        // is read even before any peer is installed (an exit with no other
        // direct peers would otherwise never spawn a recv loop for its public
        // socket).
        let mut direct_events = if direct_ctx.is_some() && direct::public_direct_enabled() {
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
        let mut relay = match self.mode {
            // Pass our LAN endpoints so the relay-endpoint trickle re-includes
            // them (the server replaces, so they'd otherwise be clobbered —
            // rc.135). Empty when the direct path is off.
            CarrierMode::Relay => Some(RelayCoordinator::new(
                self.outbound.clone(),
                direct_ctx
                    .as_ref()
                    .map(|c| c.endpoints.clone())
                    .unwrap_or_default(),
            )),
            CarrierMode::Direct(_) => None,
        };
        self.install_peers(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &first_peers,
            direct_ctx.as_ref(),
            &cooldowns,
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

        // Phase 2 — steady state.
        loop {
            tokio::select! {
                read = tun.read_packet() => match read {
                    Ok(pkt) => { let _ = wg.send_ip_packet(&pkt).await; }
                    Err(e) => { debug!(%e, "overlay: TUN read ended; runtime exiting"); break; }
                },
                // rc.136 — direct→relay fallback sweep. A DIRECT carrier whose
                // handshake never completes (or dies mid-session) means the LAN
                // path only LOOKED viable (same subnet) but isn't actually
                // reachable — a corp full-tunnel VPN that hijacks routing, Wi-Fi
                // AP/client isolation, an asymmetric firewall. Tear it down and
                // switch the peer to relay (with a cooldown so the next netmap
                // doesn't immediately re-upgrade it to direct).
                _ = fallback.tick() => {
                    self.sweep_carrier_health(
                        &mut wg, &mut by_node, &mut relay, &tun,
                        &mut cooldowns, &mut relay_refresh_cooldown, &current_peers,
                    ).await;
                    // A carrier flip may have changed the coturn worker set or
                    // the exit peer's reachability — re-reconcile exit routing
                    // FIRST, so the refreshed view carries the new exit status.
                    self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state)
                        .await;
                    // A direct→relay fallback (or relay refresh) changed how we
                    // reach a peer (and maybe the exit status) — refresh the view.
                    self.publish_view(
                        &self_ip,
                        &by_node,
                        &current_peers,
                        exit_node_status(self.exit_node.as_deref(), &exit_state),
                    );
                },
                // rc.146 — re-assert every installed peer's /32 on the overlay
                // NIC (evict any competing route a full-tunnel VPN re-added, then
                // re-add ours at low metric). Unconditional: a captured route
                // keeps our packets off the WG device, so the carrier's traffic
                // counters can't detect it — only a periodic re-assert can.
                _ = route_guard.tick() => {
                    for e in by_node.values() {
                        tun.add_peer_route(e.overlay_ip).await.ok();
                    }
                    // P5 — re-assert the exit split-default on the same tight
                    // cadence so a competing full-tunnel VPN default can't
                    // reclaim egress (mirrors the per-peer /32 route war, A7).
                    if exit_state.split_default_installed {
                        for cidr in SPLIT_DEFAULT_V4.iter().chain(SPLIT_DEFAULT_V6.iter()) {
                            tun.add_cidr_route(cidr).await.ok();
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
                        None => std::future::pending::<Option<super::wg::DirectInbound>>().await,
                    }
                } => {
                    if let Some(inb) = maybe_init {
                        self.handle_direct_inbound(
                            &mut wg, &mut by_node, &mut relay, &tun,
                            &current_peers, &cooldowns, inb,
                        ).await;
                        self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                        self.publish_view(&self_ip, &by_node, &current_peers, exit_node_status(self.exit_node.as_deref(), &exit_state));
                    }
                },
                evt = events.recv() => match evt {
                    // Re-sync: install any newly-listed peers (deltas drive
                    // removals; a full diff/prune is a later refinement).
                    Some(OverlayEvent::Netmap { peers, .. }) => {
                        current_peers = peers.iter().map(|p| (p.node_id, p.clone())).collect();
                        if let Some(names) = &dns_names { sync_name_map(names, &current_peers).await; }
                        self.install_peers(&mut wg, &mut by_node, &mut relay, &tun, &peers, direct_ctx.as_ref(), &cooldowns).await;
                        self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                        self.publish_view(&self_ip, &by_node, &current_peers, exit_node_status(self.exit_node.as_deref(), &exit_state));
                    }
                    Some(OverlayEvent::NetmapDelta { upserts, removes }) => {
                        for p in &upserts { current_peers.insert(p.node_id, p.clone()); }
                        self.install_peers(&mut wg, &mut by_node, &mut relay, &tun, &upserts, direct_ctx.as_ref(), &cooldowns).await;
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
                        }
                        if let Some(names) = &dns_names { sync_name_map(names, &current_peers).await; }
                        self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                        self.publish_view(&self_ip, &by_node, &current_peers, exit_node_status(self.exit_node.as_deref(), &exit_state));
                    }
                    Some(OverlayEvent::RelayGrant { peer_node_id, ice_servers, pair_key }) => {
                        if let Some(r) = relay.as_mut()
                            && let Some(link) = r.on_grant(peer_node_id, ice_servers, pair_key).await
                        {
                            self.install_ready(&mut wg, &mut by_node, &tun, link).await;
                            // A newly-installed relay carrier adds a coturn worker
                            // to exempt (and the exit peer may have just become
                            // reachable) — reconcile exit routing, then refresh
                            // the view so `roomler status` reflects it.
                            self.reconcile_exit_routing(&mut wg, &tun, &by_node, &current_peers, &mut exit_state).await;
                            self.publish_view(&self_ip, &by_node, &current_peers, exit_node_status(self.exit_node.as_deref(), &exit_state));
                        }
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
        // (node_id, was_direct, was_public_direct, hard_dead)
        let mut dead: Vec<(ObjectId, bool, bool, bool)> = Vec::new();
        for (nid, e) in by_node.iter_mut() {
            let Some((tx, rx)) = wg.peer_traffic(&e.pubkey) else {
                continue;
            };
            let (last_tx, last_rx) = e.last_traffic;
            e.last_traffic = (tx, rx);
            // P3b-3: rx advancing = a packet arrived FROM this peer → a real
            // "last seen". Advance BEFORE the warm-up `continue` so a freshly
            // installed peer's first inbound packets already register. Reuses
            // the same lock-free `(tx,rx)` snapshot the health check reads.
            if rx > last_rx {
                e.last_rx_at = now;
            }
            // rc.181 — a relay carrier whose underlying send hard-errored (a
            // TURNS/TCP reset, or a lost QUIC-over-TURN connection) is dead
            // NOW. Skip BOTH the warm-up grace and the multi-sweep rx-flat
            // heuristic for it and re-allocate on this tick (still rate-limited
            // by `relay_refresh_cooldown` below). Always `false` for a direct
            // carrier, so this only ever fast-paths a relay.
            let hard_dead = wg.peer_carrier_dead(&e.pubkey).unwrap_or(false);
            // Warm-up grace: let the handshake + first packets flow.
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
                    if e.public_direct_dst.is_some() {
                        cooldowns.public_fails.remove(nid);
                    } else {
                        cooldowns.lan_fails.remove(nid);
                    }
                }
            }
            if e.bad_sweeps >= BAD_SWEEPS_TO_FALLBACK || hard_dead {
                // For a relay, hold off if we just refreshed it (anti-ping-pong).
                if !e.is_direct
                    && relay_refresh_cooldown
                        .get(nid)
                        .is_some_and(|&until| until > now)
                {
                    continue;
                }
                dead.push((*nid, e.is_direct, e.public_direct_dst.is_some(), hard_dead));
            }
        }
        for (nid, was_direct, was_public_direct, hard_dead) in dead {
            let Some(e) = by_node.remove(&nid) else {
                continue;
            };
            wg.remove_peer(&e.pubkey).await;
            tun.del_peer_route(e.overlay_ip).await;
            if was_direct {
                // Escalating cooldown on the carrier's OWN tier (CC1). LAN: the
                // "same /24" was a VPN client pool, not a reachable LAN. Public:
                // the peer's advertised public endpoint isn't actually reachable
                // (host firewall / not truly public). Either way, after
                // DIRECT_MAX_FAILURES consecutive failures pin this peer to relay
                // for the session so a false match can't flap the working relay.
                let (count_map, cooldown_map, tier) = if was_public_direct {
                    (&mut cooldowns.public_fails, &mut cooldowns.public, "public")
                } else {
                    (&mut cooldowns.lan_fails, &mut cooldowns.lan, "LAN")
                };
                let fails = count_map.entry(nid).or_insert(0);
                *fails += 1;
                let sticky = *fails >= DIRECT_MAX_FAILURES;
                cooldown_map.insert(nid, now + direct_retry_cooldown(*fails));
                if sticky {
                    warn!(
                        peer = %nid, tier, fails = *fails,
                        "overlay: direct carrier failed repeatedly — pinning this peer to relay for the session"
                    );
                } else {
                    warn!(
                        peer = %nid, tier,
                        "overlay: direct carrier didn't establish (firewall / VPN / AP-isolation?) — falling back to relay"
                    );
                }
            } else {
                relay_refresh_cooldown.insert(nid, now + RELAY_REFRESH_COOLDOWN);
                if hard_dead {
                    warn!(
                        peer = %nid,
                        "overlay: relay carrier send hard-errored (TURNS/TCP reset / QUIC-over-TURN lost) — re-allocating"
                    );
                } else {
                    warn!(
                        peer = %nid,
                        "overlay: relay carrier one-way (stale coturn port?) — re-allocating"
                    );
                }
            }
            // (Re)request the relay now (don't wait for the next netmap). For a
            // refresh we first forget the stale allocation so a fresh one is made.
            if let (Some(coord), Some(np)) = (relay.as_mut(), current_peers.get(&nid))
                && let Some(cfg) = peer_config_from_netmap(np)
            {
                if !was_direct {
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
        // Phase A — a single unbound socket to DIAL peers' public endpoints
        // (the OS picks egress per-destination). Best-effort: a bind failure
        // just leaves the public-direct tier off (relay still works).
        let public_sock = if direct::public_direct_enabled() {
            match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
                Ok(s) => {
                    info!(
                        "overlay: public-direct tier ON — dialing peers' public endpoints (NAT-traversal Phase A)"
                    );
                    Some(Arc::new(s))
                }
                Err(e) => {
                    warn!(%e, "overlay: public-direct egress socket bind failed; tier off");
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
    ) {
        let now = Instant::now();
        for np in peers {
            let Some(cfg) = peer_config_from_netmap(np) else {
                continue;
            };
            // rc.136 + CC1 — suppress a direct TIER while this peer is cooling
            // down from a failure on THAT tier (treat as if no such endpoint →
            // fall through). Expired entries lapse, so the tier is retried. LAN
            // and public cooldowns are independent.
            let lan_cooling = DirectCooldowns::cooling(&cooldowns.lan, &np.node_id, now);
            let public_cooling = DirectCooldowns::cooling(&cooldowns.public, &np.node_id, now);
            // A same-subnet LAN endpoint for this peer (highest-priority tier).
            let direct_dst = if lan_cooling {
                None
            } else {
                direct_ctx
                    .and_then(|ctx| direct::pick_same_subnet_endpoint(&ctx.my_ips, &cfg.endpoints))
            };
            // Phase A — a PUBLIC endpoint from the peer's join-time NIC bucket,
            // dialable directly (no STUN) when the peer's NIC holds a public IP.
            // Requires the public-egress socket (public-direct tier on) and no
            // public-tier cooldown. Second-priority, after LAN direct.
            let public_dst = if public_cooling {
                None
            } else {
                direct_ctx.and_then(|ctx| {
                    ctx.public_sock
                        .as_ref()
                        .and_then(|_| direct::pick_public_endpoint(&ctx.my_ips, &cfg.lan_endpoints))
                })
            };

            match by_node.get(&np.node_id).map(|e| (e.is_direct, e.pubkey)) {
                Some((true, _)) => continue, // already direct (LAN or public)
                Some((false, pk)) => {
                    // Installed on RELAY — upgrade to a direct tier now that an
                    // endpoint has appeared. LAN wins over public.
                    if let (Some(ctx), Some((local_ip, dst))) = (direct_ctx, direct_dst) {
                        info!(peer = %np.node_id, %dst, "overlay: upgrading relay peer to direct LAN carrier");
                        wg.remove_peer(&pk).await;
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        self.install_direct(wg, by_node, tun, ctx, np.node_id, &cfg, local_ip, dst)
                            .await;
                    } else if let (Some(ctx), Some(dst)) = (direct_ctx, public_dst) {
                        info!(peer = %np.node_id, %dst, "overlay: upgrading relay peer to direct-to-public carrier");
                        wg.remove_peer(&pk).await;
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        self.install_public_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
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
                            subnets: cfg.subnets.clone(),
                        },
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
                        self.install_direct(wg, by_node, tun, ctx, np.node_id, &cfg, local_ip, dst)
                            .await;
                    } else if let (Some(ctx), Some(dst)) = (direct_ctx, public_dst) {
                        // Phase A — peer's NIC is public: dial it directly, skip
                        // the relay. Same forget-the-pending-relay guard.
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        self.install_public_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
                            .await;
                    } else if let Some(coord) = relay.as_mut() {
                        if let Some(link) = coord.maybe_complete(np.node_id, &cfg) {
                            self.install_ready(wg, by_node, tun, link).await;
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
            },
        );
        if let Err(e) = tun.add_peer_route(cfg.overlay_ip).await {
            debug!(peer = %node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        self.install_subnets(wg, tun, node_id, cfg.public_key, &cfg.subnets)
            .await;
        info!(peer = %node_id, overlay_ip = %cfg.overlay_ip, %dst, "overlay: direct-to-public carrier (NAT-traversal Phase A) — skipping relay");
    }

    /// Phase A — act on an AUTHENTICATED inbound direct handshake initiation
    /// forwarded by a demux loop ([`super::wg::DirectInbound`]): a NAT'd client
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
    async fn handle_direct_inbound(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        relay: &mut Option<RelayCoordinator>,
        tun: &Arc<dyn TunIo>,
        current_peers: &HashMap<ObjectId, NetmapPeer>,
        cooldowns: &DirectCooldowns,
        inb: super::wg::DirectInbound,
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

        // Anti-thrash: honour the matching tier's cooldown. A public source is
        // the public-direct tier; a private source is a LAN roam.
        let now = Instant::now();
        let is_public_src = matches!(inb.src, SocketAddr::V4(v4) if direct::is_public_v4(*v4.ip()));
        let cooling = if is_public_src {
            DirectCooldowns::cooling(&cooldowns.public, &node_id, now)
        } else {
            DirectCooldowns::cooling(&cooldowns.lan, &node_id, now)
        };
        if cooling {
            return;
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
                // Only a PUBLIC inbound source is an exit-exemption / public
                // tier; a private source is an on-link LAN roam (no exemption).
                public_direct_dst: is_public_src.then_some(inb.src),
            },
        );
        if let Err(e) = tun.add_peer_route(cfg.overlay_ip).await {
            debug!(peer = %node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        self.install_subnets(wg, tun, node_id, pubkey, &cfg.subnets)
            .await;
        // Answer the init that triggered this, immediately.
        wg.feed_direct(inb.src, inb.sock.clone(), &inb.packet).await;
        info!(peer = %node_id, src = %inb.src, public = is_public_src, "overlay: accepted authenticated inbound direct handshake (Phase A)");
    }

    /// Install a ready carrier as a WG peer, add its `/32` route, and record
    /// it (pubkey + IP) for later removal.
    async fn install_ready(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        link: ReadyLink,
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
        // rc.187 — capture the relay endpoints for `peers` visibility BEFORE the
        // carrier is (maybe) upgraded to QUIC. `relay_parts` is `Some` for any
        // relay carrier (raw or QUIC-over-TURN), `None` for a direct link.
        let (relay_local, relay_dst) = match &link.relay_parts {
            Some((conn, dst)) => (conn.local_addr().ok(), Some(*dst)),
            None => (None, None),
        };
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
        let carrier = if overlay_quic_enabled() && link.supports_quic && link.relay_parts.is_some()
        {
            let (conn, dst) = link.relay_parts.clone().unwrap();
            // Deterministic role: the lexicographically-smaller pubkey serves
            // (same rule as the WG relay initiator, so both ends agree on who
            // dials vs accepts).
            let am_server = self.keypair.public.to_bytes() < link.public_key;
            match Carrier::quic_relay(
                conn,
                dst,
                am_server,
                self.mtu as usize + WG_OVERHEAD,
                QUIC_BUILD_TIMEOUT,
            )
            .await
            {
                Ok(q) => {
                    info!(peer = %link.node_id, %dst, "overlay: QUIC-over-TURN carrier up");
                    q
                }
                Err(e) => {
                    warn!(peer = %link.node_id, %e, "overlay: QUIC carrier build failed; using raw relay");
                    link.carrier
                }
            }
        } else {
            link.carrier
        };

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

    #[test]
    fn direct_cooldown_escalates_to_sticky_after_repeated_failures() {
        // The VPN-pool relay↔direct anti-flap fix: the 1st direct failure gets
        // the normal 60 s retry, but once a peer hits DIRECT_MAX_FAILURES the
        // cooldown becomes session-sticky so it stops re-upgrading the working
        // relay to a direct carrier that can never complete.
        // `direct_retry_cooldown(1) == DIRECT_COOLDOWN` only holds when
        // DIRECT_MAX_FAILURES >= 2, so this also guards that invariant (at least
        // one plain retry before the sticky pin).
        assert_eq!(direct_retry_cooldown(1), DIRECT_COOLDOWN);
        assert_eq!(
            direct_retry_cooldown(DIRECT_MAX_FAILURES),
            DIRECT_DENY_COOLDOWN
        );
        assert_eq!(
            direct_retry_cooldown(DIRECT_MAX_FAILURES + 3),
            DIRECT_DENY_COOLDOWN
        );
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
                relay_home: None,
                reachable,
                supports_quic: false,
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
            relay_home: None,
            reachable: true,
            supports_quic: false,
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
            relay_home: None,
            reachable: true,
            supports_quic: false,
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
            relay_home: None,
            reachable: true,
            supports_quic: false,
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
