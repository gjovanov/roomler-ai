//! Multi-org P2c — ONE OS TUN carrying N per-org overlay runtimes.
//!
//! A multi-org daemon runs one [`OverlayRuntime`](super::runtime::OverlayRuntime)
//! per org, and each runtime expects to own a [`TunIo`]. But the host can only
//! have one `roomler` adapter (the rc.279 sweep deletes siblings, two on-link
//! CGNAT blocks are ambiguous, the route-guard would purge a twin's routes —
//! the full list is in `docs/multi-org.md` §2). So the runtimes must SHARE the
//! device without knowing it: this module hands each org a [`MuxPort`] facade
//! that looks exactly like its own TUN, and demuxes the real device's read
//! stream by destination.
//!
//! ## Why dst-based demux is decidable at all
//!
//! P2b gives every migrated tenant a DISJOINT address block out of
//! `100.64.0.0/10`, and the legacy `/10` tenants all live below the carve
//! floor (`100.65.0.0`). A packet's destination therefore names its org —
//! longest prefix wins, so one legacy `/10` org (typically the primary) can
//! coexist with any number of carved-block orgs nested inside the same `/10`.
//! What CANNOT be decided is two orgs on the *same* range — two un-migrated
//! `/10` tenants — and [`TunMux::register`] refuses the second one loudly
//! (that org withholds; the fix is renumbering the tenant, `multi-org.md`
//! §4.3). The source address is deliberately NOT a key: reply traffic and
//! forwarded subnet-router traffic carry foreign sources.
//!
//! ## Where the table comes from
//!
//! Nobody feeds the mux a netmap. Each org's runtime already tells its TUN
//! everything the demux needs, through the very calls that install OS routes:
//!
//! * [`TunIo::add_peer_route`] — a peer `/32` (the bulk of the table),
//! * [`TunIo::add_cidr_route`] — approved subnet routes and the exit-node
//!   split-defaults (`0.0.0.0/1` + `128.0.0.0/1`, v6 twins),
//! * registration itself — the org's own block (its on-link prefix).
//!
//! The facade records each entry against its org and passes the call through
//! to the real device, so the OS table and the demux table can never drift.
//!
//! ## Reads
//!
//! One persistent reader pump (spawned by [`TunMux::new`]) owns the real
//! `read_packet` loop — preserving the rc.213 single-waiter invariant on
//! Windows (a second concurrent reader would re-create the zombie-waiter
//! starvation this codebase already paid for once) — and forwards each packet
//! into the owning org's bounded channel. A port that has died (runtime gone,
//! receiver dropped) just loses its packets until its org reconnects and
//! re-registers, exactly like a single-org TUN whose runtime is between
//! sessions.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::mux_nat::{self, FlowMap};
use super::router::{Router, embedded_v4_of_overlay_v6};
use super::tun::TunIo;

/// Per-port inbound queue depth. Matches the runtime's own outbound queue
/// order of magnitude — the consumer is the org's TUN-reader pump, which
/// drains at packet rate; a full queue means that org's runtime is wedged and
/// dropping is the correct backpressure (this is IP).
const PORT_QUEUE_PKTS: usize = 512;

/// One v4 route entry: `(base, prefix)` — `base` is already masked.
type V4Entry = (u32, u8);
/// One v6 route entry: `(base, prefix)` — for the non-ULA v6 the runtime
/// installs (exit split-defaults). Derived-ULA destinations never consult
/// this: they unmap to v4 first.
type V6Entry = (u128, u8);

struct PortEntry {
    /// The org key — the tenant id hex (stable, unique per org).
    org: String,
    /// The org's own block, from registration. Also a demux entry.
    block: V4Entry,
    /// The org's own address on the shared device — the exact thing
    /// `add_address_sync` assigned, so teardown can hand back the SAME
    /// value. Distinct from [`Self::block`], which is masked to the block
    /// base and is a routing key, not an address: `ip addr del 100.66.0.0/22`
    /// deletes nothing when the interface holds `100.66.0.7/22`, and the
    /// failure is silent (the CI kernel test caught exactly that).
    self_ip: Ipv4Addr,
    /// Live v4 routes this org's runtime installed (peer `/32`s, subnet
    /// routes, the v4 split-default halves).
    v4: Vec<V4Entry>,
    /// Live non-ULA v6 routes (exit split-default halves).
    v6: Vec<V6Entry>,
    /// Inbound (device → org) queue. `try_send`: a wedged org must not stall
    /// the shared reader.
    tx: mpsc::Sender<Vec<u8>>,
}

struct MuxState {
    ports: Vec<PortEntry>,
    /// Normalized cross-org egress flows (the mux NAT) — see [`mux_nat`].
    flows: FlowMap,
    /// `OVERLAY_MUX_NAT` gate, resolved once at construction (the same
    /// process-lifetime stance as `OVERLAY_RPF` in [`super::wg`]).
    nat_enabled: bool,
}

/// The shared-device multiplexer. One per process (the agent holds it in a
/// static beside the TUN cache); orgs come and go via [`register`].
///
/// [`register`]: TunMux::register
pub struct TunMux {
    real: Arc<dyn TunIo>,
    state: Arc<Mutex<MuxState>>,
    /// Set by the reader pump on device death. A dead mux must be REPLACED
    /// (new device, new mux) — registering on it would strand the org on a
    /// reader that will never run again.
    dead: Arc<std::sync::atomic::AtomicBool>,
}

impl TunMux {
    /// Wrap `real` and spawn the single reader pump. The pump exits when the
    /// device dies (`read_packet` errors), after which every port's reads
    /// return `Err` — each org's runtime then tears down its session exactly
    /// as it would for its own dead TUN.
    pub fn new(real: Arc<dyn TunIo>) -> Arc<Self> {
        let nat_enabled = crate::env::flag("OVERLAY_MUX_NAT", true);
        if !nat_enabled {
            info!(
                "tun-mux: cross-org egress source normalization DISABLED \
                 (OVERLAY_MUX_NAT / config overlay_mux_nat)"
            );
        }
        let mux = Arc::new(Self {
            real: real.clone(),
            state: Arc::new(Mutex::new(MuxState {
                ports: Vec::new(),
                flows: FlowMap::default(),
                nat_enabled,
            })),
            dead: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        let state = mux.state.clone();
        let dead = mux.dead.clone();
        tokio::spawn(async move {
            loop {
                match real.read_packet().await {
                    Ok(pkt) => route_inbound(&state, pkt),
                    Err(e) => {
                        debug!(%e, "tun-mux: device read ended; reader exiting");
                        dead.store(true, std::sync::atomic::Ordering::SeqCst);
                        // Dropping the senders EOFs every port; the flow map
                        // dies with the device it described.
                        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                        st.ports.clear();
                        st.flows.clear();
                        break;
                    }
                }
            }
        });
        mux
    }

    /// Register (or re-register, on reconnect) an org and get its facade.
    ///
    /// `self_ip`/`netmask` are the org's own address + block mask from its
    /// netmap — the same pair the runtime hands any TUN factory. A repeat
    /// registration for the same org REPLACES its port (fresh queue, fresh
    /// route set — the runtime reinstalls peers from its netmap anyway);
    /// the org's demux entries start from just its block.
    ///
    /// Refused (`AddrInUse`) when another LIVE org holds the SAME block —
    /// the two-un-migrated-`/10`s case, which no destination can decide.
    /// Nested blocks (legacy `/10` + carved `/22`s) are fine: longest
    /// prefix disambiguates every address that is actually in use, because
    /// carved blocks start above the legacy reserve.
    pub fn register(
        self: &Arc<Self>,
        org: &str,
        self_ip: Ipv4Addr,
        netmask: Ipv4Addr,
    ) -> std::io::Result<Arc<MuxPort>> {
        let prefix = mask_prefix(netmask).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("non-contiguous netmask {netmask}"),
            )
        })?;
        let block = (mask_base(u32::from(self_ip), prefix), prefix);

        let (tx, rx) = mpsc::channel::<Vec<u8>>(PORT_QUEUE_PKTS);
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(holder) = st
                .ports
                .iter()
                .find(|p| p.block == block && p.org != org && !p.tx.is_closed())
            {
                warn!(
                    org,
                    holder = %holder.org,
                    block = %format_v4(block),
                    "tun-mux: block already claimed by another org — two un-migrated \
                     tenants cannot share one TUN; renumber one onto its own block \
                     (docs/multi-org.md §4.3). This org's overlay is withheld."
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "overlay block already claimed by another org on this host",
                ));
            }
            // Replace a previous incarnation of this org (reconnect), and
            // reap any dead ports while we hold the lock.
            st.ports.retain(|p| p.org != org && !p.tx.is_closed());
            st.ports.push(PortEntry {
                org: org.to_string(),
                block,
                self_ip,
                v4: Vec::new(),
                v6: Vec::new(),
                tx,
            });
            info!(
                org,
                block = %format_v4(block),
                ports = st.ports.len(),
                "tun-mux: org registered on the shared TUN"
            );
        }

        Ok(Arc::new(MuxPort {
            org: org.to_string(),
            self_ip,
            block: (Ipv4Addr::from(block.0), block.1),
            real: self.real.clone(),
            state: self.state.clone(),
            rx: tokio::sync::Mutex::new(rx),
        }))
    }

    /// Test seam: flip the mux-NAT gate without touching process env (the
    /// production value is resolved once in [`TunMux::new`]).
    #[cfg(test)]
    fn set_nat_enabled(&self, on: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .nat_enabled = on;
    }

    /// Is the underlying device still alive as far as the mux knows? False
    /// once the reader pump has exited on a device error.
    pub fn is_alive(&self) -> bool {
        !self.dead.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Drop an org's port (its runtime is gone / never started). Returns the
    /// port's block so the caller can take the OS address back down.
    ///
    /// Field 2026-08-05: the factory used to assign the org's address to the
    /// adapter BEFORE `register` could refuse it, so a REFUSED org (the
    /// two-un-migrated-tenants case) left a live address behind on every
    /// host — an address nothing answered on, which is exactly the sort of
    /// litter that makes a later diagnosis lie. Registration now happens
    /// first and the address only follows a successful claim.
    pub fn deregister(&self, org: &str) -> Option<(Ipv4Addr, u8)> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let idx = st.ports.iter().position(|p| p.org == org)?;
        let port = st.ports.remove(idx);
        // The org's OS address is going away with it: flows restoring TO it
        // or recorded FROM it are pointless (and would misfire if the block
        // is ever re-assigned).
        st.flows.purge_addr(port.self_ip);
        // The org's own ADDRESS, not its block base — the caller hands this
        // straight to `del_address_sync`, and deleting the base is a silent
        // no-op that leaves the real address on the adapter forever.
        Some((port.self_ip, port.block.1))
    }
}

/// Longest-prefix demux of one inbound packet, then a non-blocking handoff to
/// the winning org. Free function (not a method) so the reader task doesn't
/// hold an `Arc<TunMux>` cycle.
fn route_inbound(state: &Mutex<MuxState>, mut pkt: Vec<u8>) {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    let MuxState {
        ports,
        flows,
        nat_enabled,
    } = &mut *guard;
    let ports: &[PortEntry] = ports;
    if ports.is_empty() {
        return;
    }

    let winner: Option<&PortEntry> = match Router::dst_of_ip_packet(&pkt) {
        // v4, or derived-ULA v6 unmapped to its embedded v4.
        Some(dst) => best_v4(ports, u32::from(dst)),
        // Non-ULA v6 (an exit client's global-unicast egress) — match the
        // orgs' installed v6 routes. Anything else v6 (link-local chatter,
        // multicast) or unparseable: fall through to None.
        None => pkt
            .first()
            .filter(|b| (**b >> 4) == 6)
            .and_then(|_| pkt.get(24..40))
            .and_then(|dst| <[u8; 16]>::try_from(dst).ok())
            .and_then(|dst| best_v6(ports, u128::from_be_bytes(dst))),
    };

    // No org claims the destination. The OS only routes packets here along
    // routes SOME org installed, so a miss is transient (a route removed
    // mid-flight) or OS noise (NDP/SSDP probes on the interface): drop.
    let Some(port) = winner else {
        return;
    };
    if *nat_enabled {
        normalize_egress_src(ports, port.self_ip, flows, &mut pkt);
    }
    match port.tx.try_send(pkt) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            // The org's runtime is wedged; dropping is IP-correct.
            debug!(org = %port.org, "tun-mux: org queue full; packet dropped");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            // Runtime between sessions — same as a single-org TUN with no
            // reader attached. The dead port is reaped on next register.
        }
    }
}

/// Millisecond clock for the NAT warn throttle (process-relative).
fn nat_mono_ms() -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// One log line per minute per slot; CAS so concurrent callers emit one line.
/// The FIRST event always logs (`last == 0`).
fn nat_throttled(slot: &AtomicU64) -> bool {
    let now = nat_mono_ms().max(1);
    let last = slot.load(Ordering::Relaxed);
    (last == 0 || now.saturating_sub(last) >= 60_000)
        && slot
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

static NAT_PASSTHROUGH_WARNED_AT: AtomicU64 = AtomicU64::new(0);
static NAT_FIRST_REWRITE: AtomicBool = AtomicBool::new(false);
static NAT_V6_TRIPWIRE: AtomicBool = AtomicBool::new(false);

/// Hook A of the mux NAT (host-egress). A locally-originated v4 packet whose
/// source is ANOTHER org's own address is rewritten to the winning org's
/// address so single-org receivers can route their replies, and the reverse
/// mapping is recorded for [`MuxPort::restore_inbound_dst`]. The OS picks
/// such a wrong source freely on a multi-org host — nested org blocks defeat
/// its source selection (field 2026-08-09, pc50045; `docs/multi-org.md`).
/// Forwarded traffic (subnet-router, exit returns) can never trigger this:
/// its sources are never our own addresses.
fn normalize_egress_src(
    ports: &[PortEntry],
    winner_self: Ipv4Addr,
    flows: &mut FlowMap,
    pkt: &mut [u8],
) {
    let Some(v) = mux_nat::v4_view(pkt) else {
        // v6 tripwire: exactly one overlay v6 exists per host today, so a
        // derived-ULA SOURCE embedding another org's v4 is impossible. If
        // multi-org v6 ever lands without extending the NAT, say so once
        // instead of failing silently like v4 did.
        if pkt.first().map(|b| *b >> 4) == Some(6)
            && let Some(src) = pkt.get(8..24)
            && let Ok(src) = <[u8; 16]>::try_from(src)
            && let Some(src4) = embedded_v4_of_overlay_v6(src.into())
            && src4 != winner_self
            && ports.iter().any(|p| p.self_ip == src4)
            && !NAT_V6_TRIPWIRE.swap(true, Ordering::Relaxed)
        {
            warn!(
                src = %src4,
                "tun-mux: a derived-ULA v6 packet carries ANOTHER org's \
                 source — the mux NAT only normalizes v4, so this flow's \
                 replies will be unroutable at single-org receivers"
            );
        }
        return;
    };
    if v.src == winner_self || !ports.iter().any(|p| p.self_ip == v.src) {
        return;
    }
    let now = Instant::now();
    let tracked = match v.proto {
        // ICMPv4's checksum has no pseudo-header and the OS matches echo
        // replies by identifier — safe to rewrite even untracked (errors,
        // non-echo types). Echo requests also record the reverse mapping.
        mux_nat::PROTO_ICMP => {
            if v.first_fragment
                && let Some(key) = mux_nat::egress_key(pkt, &v)
            {
                let _ = flows.note_egress(key, v.src, winner_self, now);
            }
            true
        }
        mux_nat::PROTO_TCP | mux_nat::PROTO_UDP => {
            if v.first_fragment {
                // Rewrite ONLY when the reverse mapping is recorded: a
                // rewritten-but-unmapped flow fails with zero diagnostics
                // (returns go to an address no socket is anchored to), while
                // an unrewritten one keeps today's behavior plus the
                // receiver-side RPF warn as the breadcrumb.
                matches!(mux_nat::egress_key(pkt, &v),
                    Some(key) if flows.note_egress(key, v.src, winner_self, now))
            } else {
                // Non-first fragment: no L4 header here — the offset-0
                // fragment recorded the flow, and the rewrite decision is
                // IP-header-only, so every fragment of the datagram gets the
                // same source.
                true
            }
        }
        // Anything else on the overlay is unexpected; do not guess.
        _ => return,
    };
    if !tracked {
        if nat_throttled(&NAT_PASSTHROUGH_WARNED_AT) {
            warn!(
                src = %v.src, dst = %v.dst, proto = v.proto,
                "tun-mux: cross-org flow could not be tracked (flow table \
                 full or unparseable L4) — passing through with the \
                 OS-chosen source; the flow will likely fail at a single-org \
                 receiver"
            );
        }
        return;
    }
    mux_nat::rewrite_src(pkt, &v, winner_self);
    if !NAT_FIRST_REWRITE.swap(true, Ordering::Relaxed) {
        info!(
            orig = %v.src, wire = %winner_self,
            "tun-mux: normalizing cross-org egress source — the OS picked a \
             foreign org's overlay address for this destination org \
             (docs/multi-org.md)"
        );
    }
    debug!(
        orig = %v.src, wire = %winner_self, dst = %v.dst, proto = v.proto,
        "tun-mux: egress source normalized"
    );
}

/// The port with the LONGEST matching v4 entry (own block + installed
/// routes) for `dst`. First-registered wins a same-length tie (can only
/// happen transiently, e.g. two orgs both holding a defensive `/32` during a
/// renumber's overlap window).
fn best_v4(ports: &[PortEntry], dst: u32) -> Option<&PortEntry> {
    let mut best: Option<(&PortEntry, u8)> = None;
    for p in ports {
        let candidate = std::iter::once(&p.block)
            .chain(p.v4.iter())
            .filter(|(base, prefix)| mask_base(dst, *prefix) == *base)
            .map(|(_, prefix)| *prefix)
            .max();
        if let Some(len) = candidate
            && best.map(|(_, b)| len > b).unwrap_or(true)
        {
            best = Some((p, len));
        }
    }
    best.map(|(p, _)| p)
}

fn best_v6(ports: &[PortEntry], dst: u128) -> Option<&PortEntry> {
    let mut best: Option<(&PortEntry, u8)> = None;
    for p in ports {
        let candidate =
            p.v6.iter()
                .filter(|(base, prefix)| mask_base6(dst, *prefix) == *base)
                .map(|(_, prefix)| *prefix)
                .max();
        if let Some(len) = candidate
            && best.map(|(_, b)| len > b).unwrap_or(true)
        {
            best = Some((p, len));
        }
    }
    best.map(|(p, _)| p)
}

/// One org's view of the shared device. Implements [`TunIo`] so the
/// runtime cannot tell it from a private TUN.
pub struct MuxPort {
    org: String,
    /// This org's own address on the shared device — the mux NAT's ingress
    /// gate ([`Self::restore_inbound_dst`]).
    self_ip: Ipv4Addr,
    /// This org's block `(net, plen)` — what THIS port's
    /// [`TunIo::defend_block_floor`] floors. The shared device's own
    /// connected block is the creator org's only.
    block: (Ipv4Addr, u8),
    real: Arc<dyn TunIo>,
    state: Arc<Mutex<MuxState>>,
    /// The inbound queue's receiving half. A `tokio::sync::Mutex` because
    /// `read_packet` takes `&self` and awaits; in practice exactly one
    /// reader (the runtime's TUN pump) ever holds it.
    rx: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl MuxPort {
    /// Record a v4 entry for this org (idempotent).
    fn note_v4(&self, base: u32, prefix: u8) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = st.ports.iter_mut().find(|p| p.org == self.org) {
            let e = (mask_base(base, prefix), prefix);
            if !p.v4.contains(&e) {
                p.v4.push(e);
            }
        }
    }

    fn forget_v4(&self, base: u32, prefix: u8) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = st.ports.iter_mut().find(|p| p.org == self.org) {
            let e = (mask_base(base, prefix), prefix);
            p.v4.retain(|x| *x != e);
        }
    }

    fn note_v6(&self, base: u128, prefix: u8) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = st.ports.iter_mut().find(|p| p.org == self.org) {
            let e = (mask_base6(base, prefix), prefix);
            if !p.v6.contains(&e) {
                p.v6.push(e);
            }
        }
    }

    fn forget_v6(&self, base: u128, prefix: u8) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = st.ports.iter_mut().find(|p| p.org == self.org) {
            let e = (mask_base6(base, prefix), prefix);
            p.v6.retain(|x| *x != e);
        }
    }

    /// Hook B of the mux NAT (host-ingress): `Some(rewritten)` iff this
    /// decrypted inbound v4 packet is addressed to this org's own address AND
    /// matches a flow [`normalize_egress_src`] recorded — the destination is
    /// then restored to the address the OS originally chose, so the anchored
    /// socket receives it. Fragments are never rewritten: only the offset-0
    /// fragment carries the ports, and a half-rewritten train fails
    /// reassembly — strictly worse than untouched.
    fn restore_inbound_dst(&self, packet: &[u8]) -> Option<Vec<u8>> {
        let v = mux_nat::v4_view(packet)?;
        if v.dst != self.self_ip || v.fragment {
            return None;
        }
        let key = mux_nat::ingress_key(packet, &v)?;
        let orig = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !st.nat_enabled {
                return None;
            }
            st.flows.restore_dst(&key, self.self_ip, Instant::now())?
        };
        let mut pkt = packet.to_vec();
        mux_nat::rewrite_dst(&mut pkt, &v, orig);
        debug!(
            wire = %v.dst, orig = %orig, src = %v.src,
            "tun-mux: ingress destination restored"
        );
        Some(pkt)
    }

    /// Record whichever family `cidr` parses as. Unparseable strings are
    /// ignored here — the real device rejects them and its error is what the
    /// caller sees.
    fn note_cidr(&self, cidr: &str, add: bool) {
        if let Some((ip, prefix)) = split_cidr(cidr) {
            match ip {
                std::net::IpAddr::V4(v4) => {
                    if add {
                        self.note_v4(u32::from(v4), prefix);
                    } else {
                        self.forget_v4(u32::from(v4), prefix);
                    }
                }
                std::net::IpAddr::V6(v6) => {
                    if add {
                        self.note_v6(u128::from(v6), prefix);
                    } else {
                        self.forget_v6(u128::from(v6), prefix);
                    }
                }
            }
        }
    }
}

#[async_trait]
impl TunIo for MuxPort {
    async fn read_packet(&self) -> std::io::Result<Vec<u8>> {
        self.rx.lock().await.recv().await.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "shared TUN gone or this org was re-registered",
            )
        })
    }

    async fn write_packet(&self, packet: &[u8]) -> std::io::Result<()> {
        // Writes need no arbitration: the device serialises internally and
        // the packets carry their own addressing. The one exception is the
        // mux NAT's reverse leg — a reply to a flow whose egress source was
        // normalized must get its destination restored, or the OS delivers
        // it to an address no local socket is anchored to.
        if let Some(rewritten) = self.restore_inbound_dst(packet) {
            return self.real.write_packet(&rewritten).await;
        }
        self.real.write_packet(packet).await
    }

    /// Multi-org v2 — a port is a facade over the shared device: per-adapter
    /// consumers (subnet-router NAT) must scope to the REAL adapter's name.
    fn os_name(&self) -> Option<String> {
        self.real.os_name()
    }

    async fn add_peer_route(&self, peer: Ipv4Addr) -> std::io::Result<()> {
        // Record BEFORE the (best-effort, fallible) OS install: the demux
        // must know the peer even when the OS route already exists or the
        // install is racing a VPN's competing entry. The `_from` twin pins
        // this org's address as the route's source hint (Linux), so kernel
        // source selection can never pick a sibling org's address here.
        self.note_v4(u32::from(peer), 32);
        self.real.add_peer_route_from(peer, self.self_ip).await
    }

    async fn del_peer_route(&self, peer: Ipv4Addr) {
        self.forget_v4(u32::from(peer), 32);
        // The OS `/32` is shared truth — another org can't hold the same
        // address (disjoint blocks), so deleting is safe.
        self.real.del_peer_route(peer).await
    }

    async fn defend_self_route(&self, self_ip: Ipv4Addr) {
        self.real.defend_self_route(self_ip).await
    }

    async fn add_cidr_route(&self, cidr: &str) -> std::io::Result<()> {
        self.note_cidr(cidr, true);
        // Source-hinted like the peer `/32`s — a subnet behind this org's
        // router must never be reached with a sibling org's source either.
        self.real.add_cidr_route_from(cidr, self.self_ip).await
    }

    async fn del_cidr_route(&self, cidr: &str) {
        self.note_cidr(cidr, false);
        self.real.del_cidr_route(cidr).await
    }

    async fn defend_block_floor(&self) {
        // Forward THIS org's block through the device's `_of` twin. The
        // device's plain method floors its own connected block — the CREATOR
        // org's — and the trait default is a no-op, so without this arm the
        // #391 corp-VPN leak guard is silently dead for every org on a
        // shared device. Floors bypass `note_cidr`: they are drop-guards,
        // and the org's block registration already covers them in the demux.
        let (net, plen) = self.block;
        self.real.defend_block_floor_of(net, plen).await
    }

    async fn add_host_exemption(&self, ip: std::net::IpAddr) -> std::io::Result<()> {
        // Exemptions route AWAY from the TUN (via the original uplink), so
        // they never appear in the demux table.
        self.real.add_host_exemption(ip).await
    }

    async fn del_host_exemption(&self, ip: std::net::IpAddr) {
        self.real.del_host_exemption(ip).await
    }
}

// ---------------------------------------------------------------------------
// Small pure helpers
// ---------------------------------------------------------------------------

fn mask_prefix(mask: Ipv4Addr) -> Option<u8> {
    let m = u32::from(mask);
    let ones = m.count_ones();
    (m == if ones == 0 { 0 } else { !0u32 << (32 - ones) }).then_some(ones as u8)
}

fn mask_base(addr: u32, prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        addr & (!0u32 << (32 - u32::from(prefix.min(32))))
    }
}

fn mask_base6(addr: u128, prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        addr & (!0u128 << (128 - u32::from(prefix.min(128))))
    }
}

fn split_cidr(s: &str) -> Option<(std::net::IpAddr, u8)> {
    let (ip, prefix) = s.split_once('/')?;
    Some((ip.trim().parse().ok()?, prefix.trim().parse().ok()?))
}

fn format_v4((base, prefix): V4Entry) -> String {
    format!("{}/{prefix}", Ipv4Addr::from(base))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock device: scripted inbound packets + captured writes.
    struct MockTun {
        inbound: tokio::sync::Mutex<mpsc::Receiver<std::io::Result<Vec<u8>>>>,
        writes: Mutex<Vec<Vec<u8>>>,
        peer_routes: AtomicUsize,
        floors: Mutex<Vec<(Ipv4Addr, u8)>>,
    }

    fn mock() -> (mpsc::Sender<std::io::Result<Vec<u8>>>, Arc<MockTun>) {
        let (tx, rx) = mpsc::channel(64);
        (
            tx,
            Arc::new(MockTun {
                inbound: tokio::sync::Mutex::new(rx),
                writes: Mutex::new(Vec::new()),
                peer_routes: AtomicUsize::new(0),
                floors: Mutex::new(Vec::new()),
            }),
        )
    }

    #[async_trait]
    impl TunIo for MockTun {
        async fn read_packet(&self) -> std::io::Result<Vec<u8>> {
            self.inbound
                .lock()
                .await
                .recv()
                .await
                .unwrap_or_else(|| Err(std::io::Error::other("mock closed")))
        }
        async fn write_packet(&self, packet: &[u8]) -> std::io::Result<()> {
            self.writes.lock().unwrap().push(packet.to_vec());
            Ok(())
        }
        async fn add_peer_route(&self, _peer: Ipv4Addr) -> std::io::Result<()> {
            self.peer_routes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn defend_block_floor_of(&self, net: Ipv4Addr, plen: u8) {
            self.floors.lock().unwrap().push((net, plen));
        }
    }

    fn v4_pkt(dst: [u8; 4]) -> Vec<u8> {
        let mut pkt = vec![0u8; 20];
        pkt[0] = 0x45;
        pkt[16..20].copy_from_slice(&dst);
        pkt
    }

    fn ula_pkt(v4: [u8; 4]) -> Vec<u8> {
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x60;
        // fd72:6f6f:6d6c:: with the v4 embedded in the last 4 bytes.
        pkt[24] = 0xfd;
        pkt[25] = 0x72;
        pkt[26] = 0x6f;
        pkt[27] = 0x6f;
        pkt[28] = 0x6d;
        pkt[29] = 0x6c;
        pkt[36..40].copy_from_slice(&v4);
        pkt
    }

    fn v6_pkt(dst: std::net::Ipv6Addr) -> Vec<u8> {
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x60;
        pkt[24..40].copy_from_slice(&dst.octets());
        pkt
    }

    async fn recv_on(port: &MuxPort) -> Vec<u8> {
        tokio::time::timeout(std::time::Duration::from_secs(2), port.read_packet())
            .await
            .expect("timed out")
            .expect("port alive")
    }

    /// The core promise: a legacy `/10` primary and a carved `/22` secondary
    /// share one device, and every destination lands on ITS org — the nested
    /// case longest-prefix exists for.
    #[tokio::test]
    async fn nested_legacy_and_carved_blocks_demux_by_longest_prefix() {
        let (feed, dev) = mock();
        let mux = TunMux::new(dev);
        let legacy = mux
            .register(
                "orgA",
                "100.64.0.7".parse().unwrap(),
                "255.192.0.0".parse().unwrap(),
            )
            .unwrap();
        let carved = mux
            .register(
                "orgB",
                "100.65.0.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();

        // A dst inside the carved /22 — nested INSIDE the legacy /10.
        feed.send(Ok(v4_pkt([100, 65, 0, 9]))).await.unwrap();
        assert_eq!(recv_on(&carved).await[16..20], [100, 65, 0, 9]);

        // A dst in the legacy reserve — only the /10 covers it.
        feed.send(Ok(v4_pkt([100, 64, 0, 12]))).await.unwrap();
        assert_eq!(recv_on(&legacy).await[16..20], [100, 64, 0, 12]);

        // Derived-ULA v6 unmaps to its embedded v4 and follows it.
        feed.send(Ok(ula_pkt([100, 65, 1, 4]))).await.unwrap();
        assert_eq!(recv_on(&carved).await[0] >> 4, 6);
    }

    /// #391's corp-VPN leak guard must survive the mux: each org's port
    /// forwards its OWN block to the device's `_of` twin. The device's plain
    /// `defend_block_floor` only knows the creator's connected block and the
    /// trait default is a no-op — the exact combination that left the guard
    /// silently dead on every multi-org host.
    #[tokio::test]
    async fn block_floor_forwards_each_orgs_own_block() {
        let (_feed, dev) = mock();
        let mux = TunMux::new(dev.clone());
        let legacy = mux
            .register(
                "orgA",
                "100.64.0.7".parse().unwrap(),
                "255.192.0.0".parse().unwrap(),
            )
            .unwrap();
        let carved = mux
            .register(
                "orgB",
                "100.65.4.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();

        legacy.defend_block_floor().await;
        carved.defend_block_floor().await;

        let floors = dev.floors.lock().unwrap().clone();
        assert_eq!(
            floors,
            vec![
                ("100.64.0.0".parse::<Ipv4Addr>().unwrap(), 10),
                ("100.65.4.0".parse::<Ipv4Addr>().unwrap(), 22),
            ]
        );
    }

    /// A peer `/32` recorded via the facade OUTRANKS another org's block:
    /// the runtime's own route installs drive the demux.
    #[tokio::test]
    async fn peer_routes_sharpen_the_demux() {
        let (feed, dev) = mock();
        let mux = TunMux::new(dev.clone());
        let a = mux
            .register(
                "orgA",
                "100.64.0.7".parse().unwrap(),
                "255.192.0.0".parse().unwrap(),
            )
            .unwrap();
        let b = mux
            .register(
                "orgB",
                "100.65.0.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();

        // orgA (the WIDE block) installs a peer /32 for an address that sits
        // inside orgB's block-adjacent space… actually inside A's own /10 but
        // outside B's /22 — then B installs a /32 INSIDE A's /10. B's /32
        // must win over A's /10.
        b.add_peer_route("100.64.9.9".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(dev.peer_routes.load(Ordering::SeqCst), 1, "passed through");

        feed.send(Ok(v4_pkt([100, 64, 9, 9]))).await.unwrap();
        assert_eq!(recv_on(&b).await[16..20], [100, 64, 9, 9]);

        // …and deleting it hands the dst back to the /10.
        b.del_peer_route("100.64.9.9".parse().unwrap()).await;
        feed.send(Ok(v4_pkt([100, 64, 9, 9]))).await.unwrap();
        assert_eq!(recv_on(&a).await[16..20], [100, 64, 9, 9]);
    }

    /// Two orgs on the SAME block — two un-migrated `/10` tenants — is the
    /// undecidable case and must be refused, not raced.
    #[tokio::test]
    async fn a_second_unmigrated_tenant_is_refused() {
        let (_feed, dev) = mock();
        let mux = TunMux::new(dev);
        let _a = mux
            .register(
                "orgA",
                "100.64.0.7".parse().unwrap(),
                "255.192.0.0".parse().unwrap(),
            )
            .unwrap();
        let err = match mux.register(
            "orgB",
            "100.64.0.9".parse().unwrap(),
            "255.192.0.0".parse().unwrap(),
        ) {
            Err(e) => e,
            Ok(_) => panic!("second /10 must be refused"),
        };
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

        // Same org re-registering (reconnect) is NOT a conflict.
        let again = mux.register(
            "orgA",
            "100.64.0.7".parse().unwrap(),
            "255.192.0.0".parse().unwrap(),
        );
        assert!(again.is_ok());
    }

    /// Deregistering hands back the org's OWN ADDRESS, not its block base.
    ///
    /// The caller feeds this straight to `del_address_sync`, and the two are
    /// not interchangeable: `ip addr del 100.66.0.0/22` deletes nothing when
    /// the interface holds `100.66.0.7/22`, and the failure is silent — the
    /// address stays up forever, answering for an org that has left. The CI
    /// kernel test caught it against a real device; this pins it without one.
    #[tokio::test]
    async fn deregister_returns_the_address_that_was_assigned() {
        let (_feed, dev) = mock();
        let mux = TunMux::new(dev);
        let _p = mux
            .register(
                "orgA",
                "100.66.0.7".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();

        let (addr, prefix) = mux.deregister("orgA").expect("registered org");
        assert_eq!(
            addr,
            "100.66.0.7".parse::<Ipv4Addr>().unwrap(),
            "the org's address, NOT its block base 100.66.0.0"
        );
        assert_eq!(prefix, 22);
        assert!(mux.deregister("orgA").is_none(), "gone after the first");
    }

    /// Re-registration EOFs the old port (its runtime is stale) and the new
    /// port gets the traffic.
    #[tokio::test]
    async fn reconnect_replaces_the_port() {
        let (feed, dev) = mock();
        let mux = TunMux::new(dev);
        let old = mux
            .register(
                "orgA",
                "100.65.0.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();
        let new = mux
            .register(
                "orgA",
                "100.65.0.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();

        feed.send(Ok(v4_pkt([100, 65, 0, 1]))).await.unwrap();
        assert_eq!(recv_on(&new).await[16..20], [100, 65, 0, 1]);
        // The stale facade reads EOF — exactly a dead TUN to its runtime.
        assert!(old.read_packet().await.is_err());
    }

    /// Exit split-defaults recorded via `add_cidr_route` steer non-mesh
    /// destinations (an exit client's whole egress) to the exit-holding org,
    /// v4 and v6 both.
    #[tokio::test]
    async fn exit_split_defaults_capture_internet_destinations() {
        let (feed, dev) = mock();
        let mux = TunMux::new(dev);
        let plain = mux
            .register(
                "orgA",
                "100.65.0.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();
        let exit = mux
            .register(
                "orgB",
                "100.65.4.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();
        exit.add_cidr_route("0.0.0.0/1").await.unwrap();
        exit.add_cidr_route("128.0.0.0/1").await.unwrap();
        exit.add_cidr_route("2000::/3").await.unwrap();

        feed.send(Ok(v4_pkt([1, 1, 1, 1]))).await.unwrap();
        assert_eq!(recv_on(&exit).await[16..20], [1, 1, 1, 1]);
        feed.send(Ok(v4_pkt([142, 250, 0, 1]))).await.unwrap();
        assert_eq!(recv_on(&exit).await[16..20], [142, 250, 0, 1]);
        // Global-unicast v6 → the v6 route table.
        feed.send(Ok(v6_pkt("2606:4700::1111".parse().unwrap())))
            .await
            .unwrap();
        assert_eq!(recv_on(&exit).await[0] >> 4, 6);

        // Mesh traffic still lands on its own org.
        feed.send(Ok(v4_pkt([100, 65, 0, 2]))).await.unwrap();
        assert_eq!(recv_on(&plain).await[16..20], [100, 65, 0, 2]);
    }

    /// A destination NO org claims is dropped (OS noise / a route removed
    /// mid-flight) — never delivered to an arbitrary org.
    #[tokio::test]
    async fn unclaimed_destinations_are_dropped() {
        let (feed, dev) = mock();
        let mux = TunMux::new(dev);
        let a = mux
            .register(
                "orgA",
                "100.65.0.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();
        feed.send(Ok(v4_pkt([9, 9, 9, 9]))).await.unwrap();
        feed.send(Ok(v4_pkt([100, 65, 0, 1]))).await.unwrap();
        // Only the in-block packet arrives; the unclaimed one vanished.
        assert_eq!(recv_on(&a).await[16..20], [100, 65, 0, 1]);
    }

    /// Writes pass straight through — both orgs share the device's send side.
    #[tokio::test]
    async fn writes_pass_through() {
        let (_feed, dev) = mock();
        let mux = TunMux::new(dev.clone());
        let a = mux
            .register(
                "orgA",
                "100.65.0.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();
        let b = mux
            .register(
                "orgB",
                "100.65.4.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();
        a.write_packet(&v4_pkt([100, 65, 0, 1])).await.unwrap();
        b.write_packet(&v4_pkt([100, 65, 4, 1])).await.unwrap();
        assert_eq!(dev.writes.lock().unwrap().len(), 2);
    }

    /// Device death EOFs every port, so each runtime tears down like it
    /// owned the TUN.
    #[tokio::test]
    async fn device_death_reaches_every_port() {
        let (feed, dev) = mock();
        let mux = TunMux::new(dev);
        let a = mux
            .register(
                "orgA",
                "100.65.0.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();
        let b = mux
            .register(
                "orgB",
                "100.65.4.3".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            )
            .unwrap();
        feed.send(Err(std::io::Error::other("device gone")))
            .await
            .unwrap();
        assert!(a.read_packet().await.is_err());
        assert!(b.read_packet().await.is_err());
    }

    // -----------------------------------------------------------------------
    // Mux NAT (cross-org egress source normalization + reverse restore)
    // -----------------------------------------------------------------------

    use crate::overlay::mux_nat::{self, FLOW_CAP, FlowKey, reference as refpkt};

    fn addr(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    /// Legacy `/10` org A (self 100.64.0.7) + carved `/22` org B (self
    /// 100.65.0.3), with an A-side peer `/32` at 100.64.9.9 — the exact
    /// nested-block shape the field bug lived in. NAT forced ON so tests are
    /// independent of ambient env.
    async fn nat_fixture() -> (
        mpsc::Sender<std::io::Result<Vec<u8>>>,
        Arc<MockTun>,
        Arc<TunMux>,
        Arc<MuxPort>,
        Arc<MuxPort>,
    ) {
        let (feed, dev) = mock();
        let mux = TunMux::new(dev.clone());
        mux.set_nat_enabled(true);
        let a = mux
            .register("orgA", addr("100.64.0.7"), addr("255.192.0.0"))
            .unwrap();
        let b = mux
            .register("orgB", addr("100.65.0.3"), addr("255.255.252.0"))
            .unwrap();
        a.add_peer_route(addr("100.64.9.9")).await.unwrap();
        (feed, dev, mux, a, b)
    }

    fn last_write(dev: &MockTun) -> Vec<u8> {
        dev.writes.lock().unwrap().last().cloned().expect("a write")
    }

    /// The field bug, reproduced end-to-end: a packet the OS sourced from the
    /// WRONG org's address (100.65.0.3, org B) toward an org-A destination is
    /// delivered to org A with its source normalized to A's own address and
    /// checksums intact.
    #[tokio::test]
    async fn cross_org_egress_source_is_normalized() {
        let (feed, _dev, _mux, a, _b) = nat_fixture().await;
        let pkt = refpkt::mk_udp(addr("100.65.0.3"), addr("100.64.9.9"), 40000, 53);
        feed.send(Ok(pkt)).await.unwrap();
        let got = recv_on(&a).await;
        let v = mux_nat::v4_view(&got).unwrap();
        assert_eq!(v.src, addr("100.64.0.7"));
        assert_eq!(v.dst, addr("100.64.9.9"));
        refpkt::assert_checksums_valid(&got);
    }

    /// The trigger is exact-match on OUR self addresses: forwarded traffic
    /// (subnet-router LAN sources, exit-node internet returns) passes
    /// byte-identical.
    #[tokio::test]
    async fn foreign_sources_are_never_rewritten() {
        let (feed, _dev, _mux, a, _b) = nat_fixture().await;
        for src in ["10.66.51.147", "1.1.1.1"] {
            let pkt = refpkt::mk_udp(addr(src), addr("100.64.9.9"), 4433, 53);
            feed.send(Ok(pkt.clone())).await.unwrap();
            assert_eq!(recv_on(&a).await, pkt, "src {src} must pass untouched");
        }
    }

    /// The reverse leg: the peer's reply (addressed to the WIRE source) has
    /// its destination restored to the address the local socket is anchored
    /// to; an unrelated reply passes byte-identical.
    #[tokio::test]
    async fn reply_dst_is_restored_on_ingress() {
        let (feed, dev, _mux, a, _b) = nat_fixture().await;
        let out = refpkt::mk_udp(addr("100.65.0.3"), addr("100.64.9.9"), 40000, 53);
        feed.send(Ok(out)).await.unwrap();
        let _ = recv_on(&a).await;

        let reply = refpkt::mk_udp(addr("100.64.9.9"), addr("100.64.0.7"), 53, 40000);
        a.write_packet(&reply).await.unwrap();
        let got = last_write(&dev);
        let v = mux_nat::v4_view(&got).unwrap();
        assert_eq!(v.dst, addr("100.65.0.3"), "restored to the anchored addr");
        refpkt::assert_checksums_valid(&got);

        let unrelated = refpkt::mk_udp(addr("100.64.9.9"), addr("100.64.0.7"), 53, 41000);
        a.write_packet(&unrelated).await.unwrap();
        assert_eq!(last_write(&dev), unrelated, "no flow ⇒ untouched");
    }

    /// ICMP echo round-trips by identifier — and an INBOUND echo request from
    /// the peer never matches the entry our own outbound request created.
    #[tokio::test]
    async fn icmp_echo_round_trips_by_id() {
        let (feed, dev, _mux, a, _b) = nat_fixture().await;
        let req = refpkt::mk_icmp(addr("100.65.0.3"), addr("100.64.9.9"), 8, 77);
        feed.send(Ok(req)).await.unwrap();
        let got = recv_on(&a).await;
        assert_eq!(mux_nat::v4_view(&got).unwrap().src, addr("100.64.0.7"));
        refpkt::assert_checksums_valid(&got);

        let reply = refpkt::mk_icmp(addr("100.64.9.9"), addr("100.64.0.7"), 0, 77);
        a.write_packet(&reply).await.unwrap();
        let got = last_write(&dev);
        assert_eq!(mux_nat::v4_view(&got).unwrap().dst, addr("100.65.0.3"));
        refpkt::assert_checksums_valid(&got);

        // The peer pinging OUR wire address with the same id is not a reply
        // and must pass untouched (type-8 never keys ingress).
        let inbound_req = refpkt::mk_icmp(addr("100.64.9.9"), addr("100.64.0.7"), 8, 77);
        a.write_packet(&inbound_req).await.unwrap();
        assert_eq!(last_write(&dev), inbound_req);
    }

    /// Fragment policy: egress non-first fragments get the SAME source
    /// rewrite (IP-header-only decision, L4 bytes untouched); ingress
    /// fragments are NEVER rewritten, even when a flow matches.
    #[tokio::test]
    async fn fragments_follow_the_policy() {
        let (feed, dev, _mux, a, _b) = nat_fixture().await;
        // Record the flow with the offset-0 packet first.
        let out = refpkt::mk_udp(addr("100.65.0.3"), addr("100.64.9.9"), 40000, 53);
        feed.send(Ok(out)).await.unwrap();
        let _ = recv_on(&a).await;

        // A non-first fragment of the same datagram (offset 5, no L4 header).
        let mut frag = refpkt::mk_udp(addr("100.65.0.3"), addr("100.64.9.9"), 40000, 53);
        frag[6] = 0x00;
        frag[7] = 0x05;
        let ck = refpkt::ipv4_header_cksum(&frag);
        frag[10..12].copy_from_slice(&ck.to_be_bytes());
        let l4_before = frag[20..].to_vec();
        feed.send(Ok(frag)).await.unwrap();
        let got = recv_on(&a).await;
        let v = mux_nat::v4_view(&got).unwrap();
        assert_eq!(v.src, addr("100.64.0.7"), "fragment source rewritten");
        assert_eq!(
            u16::from_be_bytes([got[10], got[11]]),
            refpkt::ipv4_header_cksum(&got),
            "ip cksum patched"
        );
        assert_eq!(got[20..].to_vec(), l4_before, "L4 bytes untouched");

        // An MF-flagged reply matching the recorded flow: untouched.
        let mut mf_reply = refpkt::mk_udp(addr("100.64.9.9"), addr("100.64.0.7"), 53, 40000);
        mf_reply[6] = 0x20;
        let ck = refpkt::ipv4_header_cksum(&mf_reply);
        mf_reply[10..12].copy_from_slice(&ck.to_be_bytes());
        a.write_packet(&mf_reply).await.unwrap();
        assert_eq!(
            last_write(&dev),
            mf_reply,
            "ingress fragments never rewritten"
        );
    }

    /// The v6 twin does not exist today (one overlay v6 per host) — a
    /// derived-ULA packet with a cross-org embedded SOURCE trips the log-once
    /// warn but passes byte-identical.
    #[tokio::test]
    async fn ula_v6_with_cross_org_embedded_src_is_untouched() {
        let (feed, _dev, _mux, a, _b) = nat_fixture().await;
        let mut pkt = ula_pkt([100, 64, 9, 9]);
        // Source = derived ULA embedding org B's 100.65.0.3.
        pkt[8] = 0xfd;
        pkt[9] = 0x72;
        pkt[10] = 0x6f;
        pkt[11] = 0x6f;
        pkt[12] = 0x6d;
        pkt[13] = 0x6c;
        pkt[20..24].copy_from_slice(&[100, 65, 0, 3]);
        feed.send(Ok(pkt.clone())).await.unwrap();
        assert_eq!(recv_on(&a).await, pkt, "v6 is observed, never rewritten");
    }

    /// A full flow table degrades NEW TCP/UDP flows to today's unrewritten
    /// behavior (with a breadcrumb) — but stateless-safe ICMP still rewrites.
    #[tokio::test]
    async fn full_map_passes_tcp_through_and_still_rewrites_icmp() {
        let (feed, _dev, mux, a, _b) = nat_fixture().await;
        {
            let mut st = mux.state.lock().unwrap();
            let now = Instant::now();
            for i in 0..FLOW_CAP as u32 {
                let key = FlowKey {
                    proto: mux_nat::PROTO_UDP,
                    remote: Ipv4Addr::from(0x0A000001 + (i >> 16)),
                    remote_port: 9,
                    local_port: (i & 0xFFFF) as u16,
                };
                assert!(
                    st.flows
                        .note_egress(key, addr("100.65.0.3"), addr("100.64.0.7"), now)
                );
            }
        }
        let tcp = refpkt::mk_tcp(addr("100.65.0.3"), addr("100.64.9.9"), 50000, 22);
        feed.send(Ok(tcp.clone())).await.unwrap();
        assert_eq!(recv_on(&a).await, tcp, "untracked TCP passes unrewritten");

        let icmp = refpkt::mk_icmp(addr("100.65.0.3"), addr("100.64.9.9"), 8, 5);
        feed.send(Ok(icmp)).await.unwrap();
        let got = recv_on(&a).await;
        assert_eq!(mux_nat::v4_view(&got).unwrap().src, addr("100.64.0.7"));
    }

    /// Deregistering an org purges its flows; re-registering an org (same
    /// address) keeps restoring — entries are keyed by addresses, never by
    /// port identity.
    #[tokio::test]
    async fn deregister_purges_and_reregistration_keeps_restoring() {
        let (feed, dev, mux, a, _b) = nat_fixture().await;
        let out = refpkt::mk_udp(addr("100.65.0.3"), addr("100.64.9.9"), 40000, 53);
        feed.send(Ok(out.clone())).await.unwrap();
        let _ = recv_on(&a).await;

        // Org B (the ORIG side of the mapping) leaves: the reply is no
        // longer restored.
        mux.deregister("orgB").expect("orgB registered");
        let reply = refpkt::mk_udp(addr("100.64.9.9"), addr("100.64.0.7"), 53, 40000);
        a.write_packet(&reply).await.unwrap();
        assert_eq!(last_write(&dev), reply, "purged with its org");

        // Fresh mapping, then org A (the WIRE side) re-registers: the entry
        // still matches through the NEW port.
        let _b2 = mux
            .register("orgB", addr("100.65.0.3"), addr("255.255.252.0"))
            .unwrap();
        feed.send(Ok(out)).await.unwrap();
        let _ = recv_on(&a).await;
        let a2 = mux
            .register("orgA", addr("100.64.0.7"), addr("255.192.0.0"))
            .unwrap();
        a2.write_packet(&reply).await.unwrap();
        let got = last_write(&dev);
        assert_eq!(mux_nat::v4_view(&got).unwrap().dst, addr("100.65.0.3"));
    }

    /// The kill switch restores byte-identical pre-fix behavior on both hooks.
    #[tokio::test]
    async fn kill_switch_disables_both_hooks() {
        let (feed, dev, mux, a, _b) = nat_fixture().await;
        // Record a flow while ON, then flip OFF: neither hook may act.
        let out = refpkt::mk_udp(addr("100.65.0.3"), addr("100.64.9.9"), 40000, 53);
        feed.send(Ok(out.clone())).await.unwrap();
        let _ = recv_on(&a).await;
        mux.set_nat_enabled(false);

        feed.send(Ok(out.clone())).await.unwrap();
        assert_eq!(recv_on(&a).await, out, "egress hook off");

        let reply = refpkt::mk_udp(addr("100.64.9.9"), addr("100.64.0.7"), 53, 40000);
        a.write_packet(&reply).await.unwrap();
        assert_eq!(last_write(&dev), reply, "ingress hook off");
    }
}
