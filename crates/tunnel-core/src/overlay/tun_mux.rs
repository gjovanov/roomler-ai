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

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::router::Router;
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
        let mux = Arc::new(Self {
            real: real.clone(),
            state: Arc::new(Mutex::new(MuxState { ports: Vec::new() })),
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
                        // Dropping the senders EOFs every port.
                        state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .ports
                            .clear();
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
            real: self.real.clone(),
            state: self.state.clone(),
            rx: tokio::sync::Mutex::new(rx),
        }))
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
        Some((Ipv4Addr::from(port.block.0), port.block.1))
    }
}

/// Longest-prefix demux of one inbound packet, then a non-blocking handoff to
/// the winning org. Free function (not a method) so the reader task doesn't
/// hold an `Arc<TunMux>` cycle.
fn route_inbound(state: &Mutex<MuxState>, pkt: Vec<u8>) {
    let st = state.lock().unwrap_or_else(|e| e.into_inner());
    if st.ports.is_empty() {
        return;
    }

    let winner: Option<&PortEntry> = match Router::dst_of_ip_packet(&pkt) {
        // v4, or derived-ULA v6 unmapped to its embedded v4.
        Some(dst) => best_v4(&st.ports, u32::from(dst)),
        // Non-ULA v6 (an exit client's global-unicast egress) — match the
        // orgs' installed v6 routes. Anything else v6 (link-local chatter,
        // multicast) or unparseable: fall through to None.
        None => pkt
            .first()
            .filter(|b| (**b >> 4) == 6)
            .and_then(|_| pkt.get(24..40))
            .and_then(|dst| <[u8; 16]>::try_from(dst).ok())
            .and_then(|dst| best_v6(&st.ports, u128::from_be_bytes(dst))),
    };

    // No org claims the destination. The OS only routes packets here along
    // routes SOME org installed, so a miss is transient (a route removed
    // mid-flight) or OS noise (NDP/SSDP probes on the interface): drop.
    let Some(port) = winner else {
        return;
    };
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
        // the packets carry their own addressing.
        self.real.write_packet(packet).await
    }

    async fn add_peer_route(&self, peer: Ipv4Addr) -> std::io::Result<()> {
        // Record BEFORE the (best-effort, fallible) OS install: the demux
        // must know the peer even when the OS route already exists or the
        // install is racing a VPN's competing entry.
        self.note_v4(u32::from(peer), 32);
        self.real.add_peer_route(peer).await
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
        self.real.add_cidr_route(cidr).await
    }

    async fn del_cidr_route(&self, cidr: &str) {
        self.note_cidr(cidr, false);
        self.real.del_cidr_route(cidr).await
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
    }

    fn mock() -> (mpsc::Sender<std::io::Result<Vec<u8>>>, Arc<MockTun>) {
        let (tx, rx) = mpsc::channel(64);
        (
            tx,
            Arc::new(MockTun {
                inbound: tokio::sync::Mutex::new(rx),
                writes: Mutex::new(Vec::new()),
                peer_routes: AtomicUsize::new(0),
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
}
