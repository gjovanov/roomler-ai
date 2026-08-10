//! Multi-org v2 — the process-wide shared direct-carrier plane.
//!
//! Today every org runtime binds its own direct sockets off the SAME
//! process-global stable port ([`direct::direct_port`]), deconflicted only by
//! the blind band walk — so which org holds the base is a spawn-order race
//! that re-runs on every restart and rebuild, churning the NAT mappings of
//! every org that loses it (field: CORPLAP-1's second org re-punching after
//! each restart). The plane replaces that with ONE socket set for the whole
//! process: every attached engine (one per org) sends and receives on the
//! same stable `ip:port`, and inbound datagrams are demultiplexed by the
//! WireGuard **receiver index** instead of by source address.
//!
//! Why the index and not the source: with N orgs sharing one socket pair on
//! BOTH hosts, two independent sessions (org A's and org B's) arrive from the
//! SAME remote `ip:port` — a source-keyed table cannot tell them apart (the
//! `wg.rs` `DemuxRoutes` insert is last-write-wins). The receiver index can:
//! boringtun derives each session's on-wire index from the `index` passed to
//! `Tunn::new` (`local_index = index << 8`; rekeys walk only the low 8 bits),
//! so `receiver_idx >> 8` is a stable per-session key — provided indices are
//! unique across ALL engines, which is exactly what [`PlaneHandle::alloc_index`]
//! provides.
//!
//! Handshake INITIATIONS carry no receiver index (the peer doesn't know our
//! session yet), so they route the way WireGuard itself routes them — by
//! static key: the plane tries [`wg::authenticate_init_with`] against each
//! attached engine (N ≤ a handful, and only on handshakes). An init from a
//! source the authenticated engine already has a session with is processed on
//! that session's `Tunn` (the rekey path); anything else is forwarded to that
//! engine's [`DirectInbound`] channel exactly as the per-device demux did —
//! same rate limit, same runtime-side accept path.
//!
//! One plane per process, owned by the embedder (the agent holds it in a
//! static beside the TUN cache). Engines attach with [`CarrierPlane::attach`];
//! the returned [`PlaneHandle`] is the engine's capability to allocate
//! indices and register routes, and detaches on drop. Gated by
//! `OVERLAY_SHARED_CARRIER` at the embedder — nothing in this module reads
//! the flag.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use boringtun::noise::{Packet, Tunn};
use boringtun::x25519::{PublicKey, StaticSecret};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::direct;
use super::wg::{
    Carrier, DirectInbound, Ingress, UNKNOWN_INIT_MAX_SOURCES, UNKNOWN_INIT_MIN_INTERVAL,
    authenticate_init_with, is_wg_shaped, process_inbound,
};
use crate::transport::stun::StunInbound;

/// Receiver-index space: boringtun shifts the `Tunn::new` index left by 8, so
/// the index must fit 24 bits for the on-wire receiver index to round-trip.
const INDEX_SPACE: u32 = 1 << 24;

/// One engine (org) attached to the plane. The secret/public pair is what
/// handshake-initiation trial-authentication runs against; `direct_events`
/// receives the engine's unknown-source inits; `tun_tx` is where its peers'
/// decrypted inbound lands.
pub struct EngineHooks {
    pub secret: StaticSecret,
    pub public: PublicKey,
    pub direct_events: mpsc::Sender<DirectInbound>,
    pub tun_tx: mpsc::Sender<Vec<u8>>,
}

struct EngineEntry {
    id: u64,
    secret: StaticSecret,
    public: PublicKey,
    direct_events: mpsc::Sender<DirectInbound>,
    tun_tx: mpsc::Sender<Vec<u8>>,
}

/// One registered session route: the demux target for `receiver_idx >> 8`.
struct PlaneRoute {
    engine: u64,
    tunn: Arc<tokio::sync::Mutex<Tunn>>,
    ingress: Ingress,
    tun_tx: mpsc::Sender<Vec<u8>>,
    /// The peer endpoint this session sends to. Inbound for the session is
    /// accepted from this source ONLY — the same no-roam stance as the
    /// source-keyed demux, so behavior under the plane is identical.
    expected_src: SocketAddr,
}

/// The plane's bound socket set (the process-wide twin of one runtime's
/// direct-socket half of `DirectCtx`).
pub struct PlaneView {
    /// One socket per usable LAN interface, bound to `(iface_ip, stable_port)`.
    pub socks: Vec<(Ipv4Addr, Arc<UdpSocket>)>,
    /// The wildcard public/srflx dialer at `stable_port + 32`, when either
    /// public-direct or srflx is enabled.
    pub public_sock: Option<Arc<UdpSocket>>,
    /// The advertised `ip:port` per LAN socket.
    pub endpoints: Vec<String>,
    pub my_ips: Vec<Ipv4Addr>,
}

struct PlaneBinds {
    socks: Vec<(Ipv4Addr, Arc<UdpSocket>)>,
    public_sock: Option<Arc<UdpSocket>>,
    endpoints: Vec<String>,
    my_ips: Vec<Ipv4Addr>,
    tasks: Vec<JoinHandle<()>>,
    /// PR-B1 — per-socket receive liveness, in lockstep with the recv loops
    /// (`socks` order first, then the public dialer). Snapshotted into
    /// `NodeStatus.direct_socks` so a reader-less socket is visible.
    stats: Vec<Arc<direct::SockStat>>,
}

impl Drop for PlaneBinds {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

struct PlaneState {
    engines: Vec<EngineEntry>,
    routes: HashMap<u32, PlaneRoute>,
    /// Reverse view for handshake-initiation routing: an init from a source
    /// an engine already has a session with is the REKEY path and processes
    /// on that session's `Tunn`. Keyed by `(engine, src)` — the same remote
    /// `ip:port` legitimately appears once per org.
    by_src: HashMap<(u64, SocketAddr), u32>,
    binds: Option<PlaneBinds>,
    next_engine_id: u64,
    /// Demux-routed STUN Binding responses (the plane twin of the device's
    /// channel — the srflx keepalive's query rides plane sockets whose
    /// `recv_from` the plane owns).
    stun_tx: mpsc::Sender<StunInbound>,
    stun_rx: Option<mpsc::Receiver<StunInbound>>,
    /// The shared srflx state every attached runtime mirrors onto its own
    /// control WS (see [`SrflxShared`]).
    srflx_watch: tokio::sync::watch::Sender<SrflxShared>,
    /// The plane keepalive task, once armed (aborted + re-armed by the
    /// rebuild — the plane itself is process-lifetime).
    keepalive: Option<JoinHandle<()>>,
    /// P1-d — the runtimes subscribed to rebuild steps ([`PlaneEvent`]).
    subscribers: Vec<mpsc::Sender<PlaneEvent>>,
    /// P1-d — rebuild-in-flight latch + cooldown stamp (the plane twin of
    /// the per-runtime `pending_rebuild`/`last_rebuild` pair).
    rebuilding: bool,
    last_rebuild: Option<Instant>,
}

/// The shared direct-carrier plane. See the module docs.
pub struct CarrierPlane {
    state: Mutex<PlaneState>,
    /// Session-index allocator, unique across every attached engine for the
    /// process lifetime (monotonic, wrapped into 24 bits, 0 skipped). At one
    /// install per second it takes ~194 days to wrap, and a wrapped index
    /// collides only with a session that has been dead for exactly that long.
    index_alloc: AtomicU32,
    /// Serializes [`ensure_srflx`](Self::ensure_srflx): the first caller
    /// gathers while later callers wait, then read the cached result.
    srflx_gate: tokio::sync::Mutex<bool>,
    /// PR-B1 — serializes [`ensure_bound`](Self::ensure_bound) ACROSS its
    /// awaits. The bind loop can take seconds (3×300 ms base retries per
    /// socket), and two orgs attaching concurrently both used to pass the
    /// empty-view fast-path, both bind, and the second `st.binds` assignment
    /// dropped the first set — whose sockets the first runtime's `DirectCtx`
    /// already held: bound, reader-less, still advertised. Field 2026-08-10:
    /// mars/jupiter relay-locked, the advertised socket's Recv-Q pegged at
    /// rmem, the winner walked the band to :43649. First caller binds; every
    /// later caller waits here and reuses the same view.
    bind_gate: tokio::sync::Mutex<()>,
    /// Times the bind section actually ran (must stay ≤1 between rebuilds —
    /// locked by `ensure_bound_binds_once_under_concurrent_attach`).
    binds_performed: AtomicU32,
}

impl CarrierPlane {
    pub fn new() -> Arc<Self> {
        let (stun_tx, stun_rx) = mpsc::channel(16);
        let (srflx_watch, _) = tokio::sync::watch::channel(SrflxShared::default());
        Arc::new(Self {
            state: Mutex::new(PlaneState {
                engines: Vec::new(),
                routes: HashMap::new(),
                by_src: HashMap::new(),
                binds: None,
                next_engine_id: 1,
                stun_tx,
                stun_rx: Some(stun_rx),
                srflx_watch,
                keepalive: None,
                subscribers: Vec::new(),
                rebuilding: false,
                last_rebuild: None,
            }),
            index_alloc: AtomicU32::new(0),
            srflx_gate: tokio::sync::Mutex::new(false),
            bind_gate: tokio::sync::Mutex::new(()),
            binds_performed: AtomicU32::new(0),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PlaneState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Attach an engine. The handle is the engine's capability to allocate
    /// indices and register routes; dropping it detaches the engine and
    /// purges every route it registered.
    pub fn attach(self: &Arc<Self>, hooks: EngineHooks) -> PlaneHandle {
        let mut st = self.lock();
        let id = st.next_engine_id;
        st.next_engine_id += 1;
        st.engines.push(EngineEntry {
            id,
            secret: hooks.secret,
            public: hooks.public,
            direct_events: hooks.direct_events,
            tun_tx: hooks.tun_tx,
        });
        info!(
            engine = id,
            engines = st.engines.len(),
            "carrier plane: engine attached"
        );
        PlaneHandle {
            plane: self.clone(),
            id,
        }
    }

    /// Bind the plane's socket set once (idempotent): one socket per usable
    /// LAN interface at the stable direct port, plus the wildcard
    /// public/srflx dialer on its offset band — the exact binds one runtime's
    /// `setup_direct` performs today, moved to process scope so the band is
    /// walked once instead of raced per org. `None` when no LAN interface is
    /// usable (the caller stays relay-only, as today).
    pub async fn ensure_bound(self: &Arc<Self>) -> Option<PlaneView> {
        // PR-B1 — hold the gate across the WHOLE bind (see `bind_gate`): the
        // first caller binds, every later caller parks here and reuses its
        // view. Without this, concurrent org attaches raced the empty-view
        // check and the loser's sockets leaked bound-but-reader-less.
        let _gate = self.bind_gate.lock().await;
        if let Some(v) = self.view() {
            return Some(v);
        }
        let ifaces = direct::gather_lan_interfaces();
        let my_ips: Vec<Ipv4Addr> = ifaces.iter().map(|(ip, _)| *ip).collect();
        if my_ips.is_empty() {
            info!("carrier plane: no usable LAN interface; direct path off (relay only)");
            return None;
        }
        let stable_port = direct::direct_port();
        let mut socks: Vec<(Ipv4Addr, Arc<UdpSocket>)> = Vec::new();
        let mut endpoints: Vec<String> = Vec::new();
        for (ip, ifindex) in &ifaces {
            let Some(s) = direct::bind_direct_socket(*ip, stable_port, "lan").await else {
                continue;
            };
            if let Some(idx) = ifindex {
                direct::force_egress_interface(&s, *idx);
            }
            match s.local_addr() {
                Ok(local) => {
                    endpoints.push(format!("{ip}:{}", local.port()));
                    socks.push((*ip, Arc::new(s)));
                }
                Err(e) => warn!(%ip, %e, "carrier plane: socket local_addr failed; skipping"),
            }
        }
        if socks.is_empty() {
            info!("carrier plane: no bindable LAN interface; direct path off (relay only)");
            return None;
        }
        let public_sock = if direct::public_direct_enabled() || direct::srflx_enabled() {
            let public_port = if stable_port != 0 {
                stable_port + direct::PUBLIC_DIAL_PORT_OFFSET
            } else {
                0
            };
            match direct::bind_direct_socket(Ipv4Addr::UNSPECIFIED, public_port, "public-dial")
                .await
            {
                Some(s) => {
                    if let Some(ix) = direct::vpn_bypass_ifindex() {
                        direct::force_egress_interface(&s, ix);
                    }
                    Some(Arc::new(s))
                }
                None => {
                    warn!("carrier plane: public-dial socket bind failed; public/srflx tiers off");
                    None
                }
            }
        } else {
            None
        };
        info!(
            endpoints = ?endpoints,
            public_dial = public_sock.is_some(),
            "carrier plane: direct sockets bound ONCE for every attached engine"
        );
        self.binds_performed.fetch_add(1, Ordering::Relaxed);
        let mut tasks = Vec::new();
        let mut stats: Vec<Arc<direct::SockStat>> = Vec::new();
        for ((_, s), ep) in socks.iter().zip(&endpoints) {
            let stat = direct::SockStat::new(ep.clone());
            tasks.push(self.adopt_socket_with(s.clone(), Some(stat.clone())));
            stats.push(stat);
        }
        if let Some(p) = &public_sock {
            let local = p
                .local_addr()
                .map(|a| format!("{a} (public-dial)"))
                .unwrap_or_else(|_| "public-dial".into());
            let stat = direct::SockStat::new(local);
            tasks.push(self.adopt_socket_with(p.clone(), Some(stat.clone())));
            stats.push(stat);
        }
        let mut st = self.lock();
        if st.binds.is_some() {
            // Tripwire — structurally unreachable under `bind_gate`; if it
            // ever fires the race guard regressed. Keep the EXISTING set (its
            // sockets are already advertised/held) and discard ours cleanly:
            // aborting our tasks drops the loops, and our socket Arcs die
            // with the locals below, closing the fds.
            warn!(
                "carrier plane: bind completed against an already-bound plane — keeping the \
                 existing set (tripwire: ensure_bound race guard failed?)"
            );
            for t in &tasks {
                t.abort();
            }
            drop(st);
            return self.view();
        }
        st.binds = Some(PlaneBinds {
            socks,
            public_sock,
            endpoints,
            my_ips,
            tasks,
            stats,
        });
        drop(st);
        self.view()
    }

    /// PR-B1 — per-socket receive liveness for `NodeStatus.direct_socks`:
    /// one row per bound plane socket. A row whose `rx_pkts` is frozen while
    /// its endpoint is advertised is the wedge signature.
    pub fn socket_stats(&self) -> Vec<crate::localapi::DirectSockStatus> {
        let st = self.lock();
        st.binds
            .as_ref()
            .map(|b| b.stats.iter().map(|s| s.status()).collect())
            .unwrap_or_default()
    }

    /// The current bound view, if [`ensure_bound`](Self::ensure_bound) ran.
    pub fn view(&self) -> Option<PlaneView> {
        let st = self.lock();
        st.binds.as_ref().map(|b| PlaneView {
            socks: b.socks.clone(),
            public_sock: b.public_sock.clone(),
            endpoints: b.endpoints.clone(),
            my_ips: b.my_ips.clone(),
        })
    }

    /// Test seam: spawn the plane's recv loop on a caller-provided (loopback)
    /// socket, no liveness stat. Production sockets go through `ensure_bound`,
    /// which wires a [`direct::SockStat`] per socket.
    #[cfg(test)]
    pub(crate) fn adopt_socket(self: &Arc<Self>, sock: Arc<UdpSocket>) -> JoinHandle<()> {
        self.adopt_socket_with(sock, None)
    }

    /// The plane recv loop, with a PR-B1 liveness stat the loop bumps per
    /// read datagram (`None` for test loops).
    fn adopt_socket_with(
        self: &Arc<Self>,
        sock: Arc<UdpSocket>,
        stat: Option<Arc<direct::SockStat>>,
    ) -> JoinHandle<()> {
        let plane = self.clone();
        tokio::spawn(async move {
            let local = sock.local_addr().map(|a| a.to_string()).unwrap_or_default();
            let mut buf = vec![0u8; super::wg::WG_BUF];
            // Per-source rate limit for forwarded unknown-source initiations —
            // loop-local, exactly like the per-device demux's.
            let mut recent_unknown: HashMap<SocketAddr, Instant> = HashMap::new();
            loop {
                let (n, src) = match sock.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(e) => {
                        // PR-B1 tripwire — a dead recv loop leaves the socket
                        // bound + advertised but reader-less; that's a WARN,
                        // not a DEBUG nobody sees.
                        warn!(%e, %local, "carrier plane: recv ended; loop exiting");
                        break;
                    }
                };
                if let Some(s) = &stat {
                    s.bump();
                }
                let stun_tx = { plane.lock().stun_tx.clone() };
                if crate::transport::stun::has_stun_cookie(&buf[..n]) && !is_wg_shaped(&buf[..n]) {
                    let _ = stun_tx.try_send(StunInbound {
                        src,
                        packet: buf[..n].to_vec(),
                    });
                    continue;
                }
                plane
                    .handle_datagram(&sock, src, &mut buf, n, Some(&mut recent_unknown))
                    .await;
            }
        })
    }

    /// Take the receiver for plane-routed STUN Binding responses. `None`
    /// after the first take (one srflx keepalive per plane).
    pub fn take_stun_events(&self) -> Option<mpsc::Receiver<StunInbound>> {
        self.lock().stun_rx.take()
    }

    /// Route + process ONE WG datagram. Shared by the recv loops and
    /// [`PlaneHandle::feed`] (the accept path re-feeding the very init that
    /// triggered a [`DirectInbound`], which carries no rate-limit state).
    async fn handle_datagram(
        self: &Arc<Self>,
        sock: &Arc<UdpSocket>,
        src: SocketAddr,
        buf: &mut [u8],
        n: usize,
        mut recent_unknown: Option<&mut HashMap<SocketAddr, Instant>>,
    ) {
        let routed = match Tunn::parse_incoming_packet(&buf[..n]) {
            // Sessionful message types carry OUR receiver index — the exact
            // demux key, unique across engines by construction.
            Ok(Packet::HandshakeResponse(p)) => self.route_by_index(p.receiver_idx >> 8, src),
            Ok(Packet::PacketCookieReply(p)) => self.route_by_index(p.receiver_idx >> 8, src),
            Ok(Packet::PacketData(p)) => self.route_by_index(p.receiver_idx >> 8, src),
            // An initiation carries no receiver index; route it by static key
            // the way WireGuard does. Fast path first: if exactly one engine
            // already has a session with this source, it's that session's
            // rekey — no crypto needed, same trust as the source-keyed demux.
            Ok(Packet::HandshakeInit(_)) => {
                let candidates: Vec<u64> = {
                    let st = self.lock();
                    st.engines
                        .iter()
                        .map(|e| e.id)
                        .filter(|id| st.by_src.contains_key(&(*id, src)))
                        .collect()
                };
                match candidates.as_slice() {
                    [only] => self.route_session_of(*only, src),
                    // Zero OR several engines know this source: authenticate
                    // to find the owner (the init is sealed to exactly one
                    // engine's static). Zero-known sources are rate-limited
                    // FIRST so a junk flood never burns a DH per engine —
                    // the same CPU profile as the per-device demux.
                    _ => {
                        if candidates.is_empty()
                            && let Some(recent) = recent_unknown.take()
                            && !unknown_init_fresh(recent, src)
                        {
                            Routed::Drop
                        } else {
                            match self.authenticate_against_engines(&buf[..n]) {
                                Some(engine) => match self.route_session_of(engine, src) {
                                    Routed::Drop => {
                                        let ev = {
                                            let st = self.lock();
                                            st.engines
                                                .iter()
                                                .find(|e| e.id == engine)
                                                .map(|e| e.direct_events.clone())
                                        };
                                        match ev {
                                            Some(tx) => Routed::ForwardInit(tx),
                                            None => Routed::Drop,
                                        }
                                    }
                                    hit => hit,
                                },
                                None => Routed::Drop,
                            }
                        }
                    }
                }
            }
            _ => Routed::Drop,
        };

        match routed {
            Routed::Session {
                tunn,
                ingress,
                tun_tx,
            } => {
                let reply = Carrier::direct(sock.clone(), src);
                let mut t = tunn.lock().await;
                process_inbound(&mut t, n, buf, &reply, &tun_tx, &ingress).await;
            }
            Routed::ForwardInit(tx) => {
                let _ = tx.try_send(DirectInbound {
                    src,
                    sock: sock.clone(),
                    packet: buf[..n].to_vec(),
                });
            }
            Routed::Drop => {}
        }

        // Local helper wants a name — defined after use for readability.
        fn unknown_init_fresh(recent: &mut HashMap<SocketAddr, Instant>, src: SocketAddr) -> bool {
            if recent.len() >= UNKNOWN_INIT_MAX_SOURCES {
                recent.retain(|_, t| t.elapsed() < UNKNOWN_INIT_MIN_INTERVAL);
            }
            let fresh = recent
                .get(&src)
                .is_none_or(|t| t.elapsed() >= UNKNOWN_INIT_MIN_INTERVAL);
            if fresh && recent.len() < UNKNOWN_INIT_MAX_SOURCES {
                recent.insert(src, Instant::now());
                true
            } else {
                false
            }
        }
    }

    fn route_by_index(&self, idx: u32, src: SocketAddr) -> Routed {
        let st = self.lock();
        match st.routes.get(&idx) {
            Some(r) if r.expected_src == src => Routed::Session {
                tunn: r.tunn.clone(),
                ingress: r.ingress.clone(),
                tun_tx: r.tun_tx.clone(),
            },
            Some(r) => {
                // Same conservative stance as the source-keyed demux: a known
                // session from an unexpected source is dropped, not roamed.
                debug!(
                    idx,
                    %src,
                    expected = %r.expected_src,
                    "carrier plane: session datagram from unexpected source; dropped"
                );
                Routed::Drop
            }
            None => Routed::Drop,
        }
    }

    fn route_session_of(&self, engine: u64, src: SocketAddr) -> Routed {
        let st = self.lock();
        match st
            .by_src
            .get(&(engine, src))
            .and_then(|idx| st.routes.get(idx))
        {
            Some(r) => Routed::Session {
                tunn: r.tunn.clone(),
                ingress: r.ingress.clone(),
                tun_tx: r.tun_tx.clone(),
            },
            None => Routed::Drop,
        }
    }

    /// Try each attached engine's static against a handshake initiation.
    /// Engines are snapshotted out of the lock — the DH runs unlocked.
    fn authenticate_against_engines(&self, init: &[u8]) -> Option<u64> {
        let engines: Vec<(u64, StaticSecret, PublicKey)> = {
            let st = self.lock();
            st.engines
                .iter()
                .map(|e| (e.id, e.secret.clone(), e.public))
                .collect()
        };
        engines
            .into_iter()
            .find(|(_, secret, public)| authenticate_init_with(secret, public, init).is_some())
            .map(|(id, _, _)| id)
    }
}

/// One routed datagram's disposition. `Session` = process on the session's
/// `Tunn`; `ForwardInit` = hand an authenticated unknown-source initiation
/// to the owning engine's accept path; `Drop` = not ours / not permitted.
enum Routed {
    Session {
        tunn: Arc<tokio::sync::Mutex<Tunn>>,
        ingress: Ingress,
        tun_tx: mpsc::Sender<Vec<u8>>,
    },
    ForwardInit(mpsc::Sender<DirectInbound>),
    Drop,
}

/// An attached engine's capability: allocate plane-unique session indices and
/// register/unregister the session routes they key. Dropping the handle
/// detaches the engine and purges its routes — the engine (a `WgDevice`)
/// owns exactly one.
pub struct PlaneHandle {
    plane: Arc<CarrierPlane>,
    id: u64,
}

impl PlaneHandle {
    /// A process-unique 24-bit session index for `Tunn::new`. Monotonic;
    /// never handed out twice within a wrap of the 24-bit space.
    pub(crate) fn alloc_index(&self) -> u32 {
        1 + self.plane.index_alloc.fetch_add(1, Ordering::Relaxed) % (INDEX_SPACE - 1)
    }

    /// Register the session route for an index handed out by
    /// [`alloc_index`](Self::alloc_index). Replaces nothing by construction —
    /// an occupied slot is a bug and is logged loudly instead of silently
    /// clobbered (the source-keyed table's failure mode). Decrypted inbound
    /// for the session lands on the engine's `tun_tx` from its attach hooks.
    pub(crate) fn register_route(
        &self,
        idx: u32,
        tunn: Arc<tokio::sync::Mutex<Tunn>>,
        ingress: Ingress,
        expected_src: SocketAddr,
    ) {
        let mut st = self.plane.lock();
        let Some(tun_tx) = st
            .engines
            .iter()
            .find(|e| e.id == self.id)
            .map(|e| e.tun_tx.clone())
        else {
            warn!(
                idx,
                engine = self.id,
                "carrier plane: register on a detached engine (bug)"
            );
            return;
        };
        st.by_src.insert((self.id, expected_src), idx);
        if st
            .routes
            .insert(
                idx,
                PlaneRoute {
                    engine: self.id,
                    tunn,
                    ingress,
                    tun_tx,
                    expected_src,
                },
            )
            .is_some()
        {
            warn!(
                idx,
                engine = self.id,
                "carrier plane: index slot was already occupied (bug)"
            );
        }
    }

    /// Drop a session route. The `(engine, src)` reverse entry is removed
    /// only if it still points at THIS index — a replacement session for the
    /// same source (re-install to the same peer) registered after us must
    /// keep its own reverse entry.
    pub(crate) fn unregister_route(&self, idx: u32) {
        let mut st = self.plane.lock();
        if let Some(r) = st.routes.remove(&idx) {
            let key = (self.id, r.expected_src);
            if st.by_src.get(&key) == Some(&idx) {
                st.by_src.remove(&key);
            }
        }
    }

    /// Process ONE datagram as if it had arrived on a plane socket — the
    /// plane twin of `WgDevice::feed_direct` (the accept path answers the
    /// very initiation that triggered its [`DirectInbound`] immediately
    /// instead of waiting for the ~5 s retransmit).
    pub(crate) async fn feed(&self, src: SocketAddr, sock: Arc<UdpSocket>, packet: &[u8]) {
        let mut buf = packet.to_vec();
        let n = buf.len();
        self.plane
            .handle_datagram(&sock, src, &mut buf, n, None)
            .await;
    }
}

impl Drop for PlaneHandle {
    fn drop(&mut self) {
        let mut st = self.plane.lock();
        st.engines.retain(|e| e.id != self.id);
        let dead: Vec<u32> = st
            .routes
            .iter()
            .filter(|(_, r)| r.engine == self.id)
            .map(|(idx, _)| *idx)
            .collect();
        for idx in &dead {
            if let Some(r) = st.routes.remove(idx) {
                let key = (self.id, r.expected_src);
                if st.by_src.get(&key) == Some(idx) {
                    st.by_src.remove(&key);
                }
            }
        }
        if !dead.is_empty() || !st.engines.is_empty() {
            debug!(
                engine = self.id,
                purged = dead.len(),
                remaining_engines = st.engines.len(),
                "carrier plane: engine detached"
            );
        }
    }
}

/// Multi-org v2 — the plane's shared srflx state: ONE gather + NAT probe +
/// keepalive for the whole process, broadcast to every attached runtime via
/// [`CarrierPlane::subscribe_srflx`] (each runtime advertises on its OWN
/// control WS, after its OWN join — the server-side join-clears-srflx
/// ordering holds per org). `generation` bumps on every change so a
/// forwarder can tell a fresh value from the initial empty state.
#[derive(Clone, Default)]
pub struct SrflxShared {
    pub stun_server: Option<SocketAddr>,
    /// Advertised candidates; `[0]` is the punch candidate.
    pub candidates: Vec<String>,
    /// The punch pair — the advertised candidate and the plane socket that
    /// owns its NAT mapping (peers' srflx are dialed FROM this socket).
    pub punch: Option<(String, Arc<UdpSocket>)>,
    pub my_nat: Option<String>,
    /// What the LocalAPI shows when the tier is off/empty this run.
    pub error: Option<String>,
    pub generation: u64,
}

impl CarrierPlane {
    /// Subscribe to the shared srflx state (see [`SrflxShared`]).
    pub fn subscribe_srflx(&self) -> tokio::sync::watch::Receiver<SrflxShared> {
        self.lock().srflx_watch.subscribe()
    }

    /// ONE srflx gather + NAT probe for the whole process. The first caller
    /// performs it — via the plane's STUN sink, because the plane's recv
    /// loops own the sockets and raw-socket STUN would be stolen by them —
    /// and every later caller gets the cached result. Also arms the plane
    /// keepalive, which holds the mapping open and re-publishes on change.
    /// P1-d re-gathers per rebuild epoch; until then the result is
    /// process-lifetime, like the plane's binds.
    pub async fn ensure_srflx(self: &Arc<Self>, stun_urls: &[String]) -> SrflxShared {
        let mut done = self.srflx_gate.lock().await;
        if *done {
            return self.lock().srflx_watch.borrow().clone();
        }
        let mut sink = self.lock().stun_rx.take();
        let shared = match sink.as_mut() {
            Some(rx) => self.gather_via_sink(stun_urls, rx).await,
            None => SrflxShared {
                generation: 1,
                error: Some("plane STUN sink already taken".into()),
                ..Default::default()
            },
        };
        // send_replace, NOT send: the plane keeps no persistent receiver
        // (the channel's initial one is dropped at construction), and
        // `send` DROPS the value when there are zero receivers — which is
        // exactly the case here, because ensure_srflx runs BEFORE any
        // runtime's forwarder subscribes. That silently left the watch empty,
        // so every forwarder read `[]` and never advertised, and the losing
        // org of the gather-gate race went relay-locked (field 2026-08-10:
        // random org per host stuck at srflx=[]). send_replace stores the
        // value unconditionally, so late subscribers see the candidate.
        self.lock().srflx_watch.send_replace(shared.clone());
        if let Some(rx) = sink {
            self.arm_keepalive(rx, stun_urls.to_vec(), &shared);
        }
        *done = true;
        shared
    }

    /// The gather pass — the plane twin of the runtime's
    /// `gather_and_advertise_srflx` core, minus the advertising (each
    /// runtime does its own) and driven through the STUN sink.
    async fn gather_via_sink(
        &self,
        stun_urls: &[String],
        rx: &mut mpsc::Receiver<StunInbound>,
    ) -> SrflxShared {
        let mut out = SrflxShared {
            generation: 1,
            ..Default::default()
        };
        if !direct::srflx_gather_active() || stun_urls.is_empty() {
            return out;
        }
        let Some(v) = self.view() else {
            return out;
        };
        if v.socks.is_empty() {
            return out;
        }
        let Some(server) = direct::resolve_stun_server(stun_urls, &v.my_ips).await else {
            warn!(
                urls = ?stun_urls,
                "carrier plane: no resolvable STUN server — srflx tier OFF for every org this run"
            );
            out.error = Some(format!("no resolvable STUN server among {stun_urls:?}"));
            return out;
        };
        out.stun_server = Some(server);
        let pairs = tokio::time::timeout(super::runtime::SRFLX_GATHER_BUDGET, async {
            let mut pairs: Vec<(String, Arc<UdpSocket>)> = Vec::new();
            for (_ip, sock) in &v.socks {
                match crate::transport::stun::srflx_query_via_sink(
                    sock,
                    rx,
                    server,
                    super::runtime::SRFLX_ATTEMPT_TIMEOUT,
                )
                .await
                {
                    Ok(SocketAddr::V4(s)) if direct::is_public_v4(*s.ip()) => {
                        let ep = SocketAddr::V4(s).to_string();
                        if !pairs.iter().any(|(e, _)| e == &ep) {
                            pairs.push((ep, sock.clone()));
                        }
                    }
                    Ok(other) => {
                        debug!(%other, "carrier plane: srflx candidate not public — skipping")
                    }
                    Err(e) => {
                        debug!(%e, "carrier plane: srflx query failed on a socket — skipping")
                    }
                }
            }
            pairs
        })
        .await
        .unwrap_or_default();
        if pairs.is_empty() {
            // WARN for the same reason the per-runtime pass warns: an empty
            // srflx tier once died fleet-wide at debug! visibility.
            warn!(
                %server,
                sockets = v.socks.len(),
                "carrier plane: srflx gather yielded NO public candidate — every org's peers \
                 will read this node as UDP-blocked (pairs fall to the relay/DERP tier)"
            );
            out.error = Some(format!(
                "STUN yielded no public candidate from {server} ({} socket(s) probed)",
                v.socks.len()
            ));
            return out;
        }
        let targets = direct::resolve_stun_targets(stun_urls, &v.my_ips).await;
        let my_nat = if targets.len() >= 2 {
            let punch_sock = pairs[0].1.clone();
            let a = crate::transport::stun::srflx_query_via_sink(
                &punch_sock,
                rx,
                targets[0],
                super::runtime::SRFLX_ATTEMPT_TIMEOUT,
            )
            .await
            .ok();
            let b = crate::transport::stun::srflx_query_via_sink(
                &punch_sock,
                rx,
                targets[1],
                super::runtime::SRFLX_ATTEMPT_TIMEOUT,
            )
            .await
            .ok();
            match (a, b) {
                (Some(a), Some(b)) => Some(if a == b { "cone" } else { "symmetric" }.to_string()),
                _ => None,
            }
        } else {
            None
        };
        out.candidates = pairs.iter().map(|(e, _)| e.clone()).collect();
        out.punch = pairs.into_iter().next();
        out.my_nat = my_nat;
        info!(
            candidates = ?out.candidates,
            my_nat = ?out.my_nat,
            %server,
            "carrier plane: srflx gathered ONCE for every attached org"
        );
        out
    }

    /// The plane keepalive — the process-wide twin of the per-runtime srflx
    /// keepalive: re-query the pinned server on the punch socket each
    /// interval, publish a changed mapping on the watch (every runtime's
    /// forwarder then re-advertises on its own WS), re-resolve the server
    /// after repeated failures. Owns the plane's STUN sink from here on.
    fn arm_keepalive(
        self: &Arc<Self>,
        rx: mpsc::Receiver<StunInbound>,
        stun_urls: Vec<String>,
        seed: &SrflxShared,
    ) {
        let secs = direct::srflx_keepalive_secs();
        let (Some(server0), Some((_, punch))) = (seed.stun_server, seed.punch.clone()) else {
            self.lock().stun_rx = Some(rx);
            return;
        };
        if !direct::srflx_enabled() || secs == 0 || seed.candidates.is_empty() {
            self.lock().stun_rx = Some(rx);
            return;
        }
        let plane = self.clone();
        let my_ips: Vec<Ipv4Addr> = self.view().map(|v| v.my_ips).unwrap_or_default();
        let mut state = seed.clone();
        let handle = tokio::spawn(async move {
            let mut rx = rx;
            let mut server = server0;
            let mut failures = 0u32;
            // PR-B1 tripwire — one WARN per outage (not per cycle): repeated
            // keepalive failure is the "advertised mapping may be dead"
            // signal, most notably a reader-less punch socket queueing the
            // reply forever (the 2026-08-10 wedge). DEBUG-only left it
            // invisible.
            let mut warned = false;
            const RERESOLVE_AFTER: u32 = 3;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // The interval's immediate first tick — the gather just ran.
            tick.tick().await;
            loop {
                tick.tick().await;
                match crate::transport::stun::srflx_query_via_sink(
                    &punch,
                    &mut rx,
                    server,
                    super::runtime::SRFLX_ATTEMPT_TIMEOUT,
                )
                .await
                {
                    Ok(cur) => {
                        failures = 0;
                        let mut publish = false;
                        if warned {
                            warned = false;
                            state.error = None;
                            publish = true;
                            info!("carrier plane: srflx keepalive recovered");
                        }
                        let ep = cur.to_string();
                        if state.candidates.first() != Some(&ep) {
                            if state.candidates.is_empty() {
                                state.candidates.push(ep.clone());
                            } else {
                                state.candidates[0] = ep.clone();
                            }
                            publish = true;
                            info!(
                                new_srflx = %ep,
                                "carrier plane: srflx mapping changed — every org re-advertises"
                            );
                        }
                        if publish {
                            state.generation += 1;
                            plane.lock().srflx_watch.send_replace(state.clone());
                        }
                    }
                    Err(e) => {
                        failures += 1;
                        if failures >= RERESOLVE_AFTER && !warned {
                            warned = true;
                            warn!(
                                %e, failures,
                                "carrier plane: srflx keepalive failing repeatedly — the \
                                 advertised mapping may be DEAD (reader-less socket / filtered \
                                 path?); peers punching it will fail"
                            );
                            state.error =
                                Some(format!("srflx keepalive failing ({failures} consecutive)"));
                            state.generation += 1;
                            plane.lock().srflx_watch.send_replace(state.clone());
                        } else {
                            debug!(
                                %e, failures,
                                "carrier plane: srflx keepalive query failed — retaining last advert"
                            );
                        }
                        if failures >= RERESOLVE_AFTER {
                            if let Some(fresh) =
                                direct::resolve_stun_server(&stun_urls, &my_ips).await
                            {
                                server = fresh;
                            }
                            failures = 0;
                        }
                    }
                }
            }
        });
        self.lock().keepalive = Some(handle);
    }
}

/// Multi-org v2 P1-d — one step of the plane-wide direct-socket rebuild,
/// delivered to every subscribed runtime.
pub enum PlaneEvent {
    /// Release every socket-pinning Arc (direct carriers, probes, the
    /// runtime's `DirectCtx` view), then ack — the plane re-binds only when
    /// every engine has released (or the 3 s straggler timeout fires; the
    /// band walk absorbs a lingering port).
    Teardown { ack: mpsc::Sender<()> },
    /// The plane re-bound its socket set: re-join from the new endpoints,
    /// re-gather (the plane's srflx cache was reset), re-install.
    Ready,
}

impl CarrierPlane {
    /// P1-d — subscribe a runtime to the plane's rebuild steps. One
    /// subscription per attached runtime; a dropped receiver is pruned on
    /// the next rebuild.
    pub fn subscribe_events(&self) -> mpsc::Receiver<PlaneEvent> {
        let (tx, rx) = mpsc::channel(4);
        self.lock().subscribers.push(tx);
        rx
    }

    /// P1-d — request a plane-wide rebuild. Debounced: an in-flight rebuild
    /// swallows the request, and a non-authoritative one (net-change storms)
    /// also respects [`REBUILD_COOLDOWN`](super::runtime::REBUILD_COOLDOWN);
    /// authoritative triggers (resume, the embedder's fingerprint push)
    /// bypass the cooldown, matching the per-runtime R3 semantics. Sync —
    /// the executor runs on its own task, so any select arm can call this.
    pub fn request_rebuild(self: &Arc<Self>, reason: &'static str, authoritative: bool) {
        {
            let mut st = self.lock();
            if st.rebuilding {
                return;
            }
            if !authoritative
                && let Some(t) = st.last_rebuild
                && t.elapsed() < super::runtime::REBUILD_COOLDOWN
            {
                return;
            }
            st.rebuilding = true;
        }
        let plane = self.clone();
        tokio::spawn(async move { plane.run_rebuild(reason).await });
    }

    /// The rebuild executor: Teardown to every runtime → await acks (3 s
    /// straggler timeout — a wedged org must not block the plane) → drop the
    /// binds (recv loops + socket Arcs) → fresh STUN sink + srflx cache
    /// reset → re-bind → Ready to every runtime. Ordering is load-bearing
    /// exactly as in the per-runtime R3 it replaces: the ports only free
    /// once every carrier/probe/view Arc is gone.
    async fn run_rebuild(self: Arc<Self>, reason: &'static str) {
        warn!(
            reason,
            "carrier plane: rebuilding the shared socket set for every org"
        );
        let subs: Vec<mpsc::Sender<PlaneEvent>> = {
            let mut st = self.lock();
            st.subscribers.retain(|s| !s.is_closed());
            st.subscribers.clone()
        };
        let (ack_tx, mut ack_rx) = mpsc::channel::<()>(subs.len().max(1));
        let mut expected = 0usize;
        for s in &subs {
            if s.try_send(PlaneEvent::Teardown {
                ack: ack_tx.clone(),
            })
            .is_ok()
            {
                expected += 1;
            }
        }
        drop(ack_tx);
        let acked = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut n = 0usize;
            while n < expected && ack_rx.recv().await.is_some() {
                n += 1;
            }
            n
        })
        .await
        .unwrap_or(0);
        if acked < expected {
            warn!(
                acked,
                expected,
                "carrier plane: teardown ack timeout — re-binding anyway (band walk absorbs \
                 a straggler's lingering port)"
            );
        }
        let had_binds = {
            let mut st = self.lock();
            if let Some(h) = st.keepalive.take() {
                h.abort();
            }
            // Fresh STUN sink: retired recv loops keep the dead sender; the
            // loops adopted below clone the new one — the same discipline as
            // the device-side `replace_stun_events`.
            let (tx, rx) = mpsc::channel(16);
            st.stun_tx = tx;
            st.stun_rx = Some(rx);
            // Dropping the binds aborts the recv loops and releases the
            // plane's own socket Arcs.
            st.binds.take().is_some()
        };
        // The srflx result was measured through sockets that no longer
        // exist — the next `ensure_srflx` re-gathers on the new binds.
        *self.srflx_gate.lock().await = false;
        if had_binds && self.ensure_bound().await.is_none() {
            warn!("carrier plane: re-bind found no usable LAN interface — direct path off");
        }
        {
            let mut st = self.lock();
            st.rebuilding = false;
            st.last_rebuild = Some(Instant::now());
            for s in &st.subscribers {
                let _ = s.try_send(PlaneEvent::Ready);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Multi-org v2 proof: N engines share ONE socket pair and the plane
    //! demuxes their sessions correctly even when both hosts' orgs collapse
    //! onto the SAME remote `ip:port` — the exact collision the source-keyed
    //! demux cannot express (its insert is last-write-wins).

    use super::*;
    use crate::overlay::WgKeypair;
    use crate::overlay::wg::{DirectInbound, WgDevice, test_genuine_init};
    use std::time::Duration;
    use tokio::sync::mpsc::Receiver;
    use tokio::time::timeout;

    /// Minimal well-formed IPv4 packet (version nibble + total length + dst),
    /// the same shape the wg two-device tests use.
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

    /// One plane-attached engine: device + its decrypted-inbound receiver.
    async fn engine(plane: &Arc<CarrierPlane>, kp: &WgKeypair) -> (WgDevice, Receiver<Vec<u8>>) {
        let (mut dev, tun_rx) = WgDevice::new(kp.secret.clone());
        let handle = plane.attach(dev.plane_hooks());
        dev.set_plane(handle);
        (dev, tun_rx)
    }

    /// Send `payload` from `dev` until it lands on `rx` (the handshake races
    /// the first sends — boringtun queues + initiates), then return it.
    async fn pump_until_delivered(
        dev: &WgDevice,
        rx: &mut Receiver<Vec<u8>>,
        pkt: &[u8],
    ) -> Vec<u8> {
        for _ in 0..100 {
            dev.send_ip_packet(pkt).await;
            if let Ok(Some(got)) = timeout(Duration::from_millis(50), rx.recv()).await {
                return got;
            }
        }
        panic!("packet never delivered through the plane");
    }

    /// THE core proof: two orgs per host, ONE socket per host, all four
    /// sessions ride the same `ip:port` pair. Receiver-index demux keeps
    /// both directions of both org pairs apart; the source-keyed table
    /// structurally could not (same key for both).
    #[tokio::test(flavor = "multi_thread")]
    async fn two_orgs_share_one_socket_pair_and_demux_by_index() {
        let (a1_kp, a2_kp) = (WgKeypair::generate(), WgKeypair::generate());
        let (b1_kp, b2_kp) = (WgKeypair::generate(), WgKeypair::generate());

        let plane_a = CarrierPlane::new();
        let plane_b = CarrierPlane::new();
        let sock_a = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let sock_b = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let (addr_a, addr_b) = (sock_a.local_addr().unwrap(), sock_b.local_addr().unwrap());
        plane_a.adopt_socket(sock_a.clone());
        plane_b.adopt_socket(sock_b.clone());

        let (mut a1, _a1_rx) = engine(&plane_a, &a1_kp).await;
        let (mut a2, _a2_rx) = engine(&plane_a, &a2_kp).await;
        let (mut b1, mut b1_rx) = engine(&plane_b, &b1_kp).await;
        let (mut b2, mut b2_rx) = engine(&plane_b, &b2_kp).await;

        let (ip_a1, ip_b1) = (Ipv4Addr::new(100, 65, 4, 1), Ipv4Addr::new(100, 65, 4, 2));
        let (ip_a2, ip_b2) = (Ipv4Addr::new(100, 65, 8, 1), Ipv4Addr::new(100, 65, 8, 2));

        // Org 1 pair and org 2 pair — SAME socket pair, four sessions. The
        // RESPONDER side registers first: an `initiate` install fires its
        // handshake immediately, and boringtun retransmits a lost init only
        // after its ~5 s rekey timeout (in production the accept path
        // re-feeds it; this test has no runtime).
        b1.add_direct_peer(
            sock_b.clone(),
            a1_kp.public.to_bytes(),
            ip_a1,
            addr_a,
            false,
        )
        .await;
        b2.add_direct_peer(
            sock_b.clone(),
            a2_kp.public.to_bytes(),
            ip_a2,
            addr_a,
            false,
        )
        .await;
        a1.add_direct_peer(sock_a.clone(), b1_kp.public.to_bytes(), ip_b1, addr_b, true)
            .await;
        a2.add_direct_peer(sock_a.clone(), b2_kp.public.to_bytes(), ip_b2, addr_b, true)
            .await;

        let got1 =
            pump_until_delivered(&a1, &mut b1_rx, &synthetic_ipv4(ip_a1, ip_b1, b"org-one")).await;
        assert_eq!(&got1[20..], b"org-one");
        let got2 =
            pump_until_delivered(&a2, &mut b2_rx, &synthetic_ipv4(ip_a2, ip_b2, b"org-two")).await;
        assert_eq!(&got2[20..], b"org-two");

        // Cross-isolation: org 1's channel never saw org 2's packet.
        assert!(
            timeout(Duration::from_millis(100), b1_rx.recv())
                .await
                .is_err(),
            "org 1's engine received a packet that belongs to org 2"
        );
    }

    /// An initiation from an unknown source is trial-authenticated against
    /// each engine's static and forwarded ONLY to the owner; a forged init
    /// (sealed to nobody here) reaches no engine at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_init_forwards_to_the_owning_engine_only() {
        let (a1_kp, a2_kp) = (WgKeypair::generate(), WgKeypair::generate());
        let plane = CarrierPlane::new();
        let sock = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = sock.local_addr().unwrap();
        plane.adopt_socket(sock);

        let (mut a1, _rx1) = engine(&plane, &a1_kp).await;
        let (mut a2, _rx2) = engine(&plane, &a2_kp).await;
        let mut a1_events = a1.take_direct_events().unwrap();
        let mut a2_events = a2.take_direct_events().unwrap();

        let dialer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stranger = WgKeypair::generate();

        // Sealed to a2's static: lands on a2's accept channel, not a1's.
        let init = test_genuine_init(&stranger.secret, a2_kp.public.to_bytes());
        dialer.send_to(&init, addr).await.unwrap();
        let ev: DirectInbound = timeout(Duration::from_secs(2), a2_events.recv())
            .await
            .expect("owner engine never got the init")
            .unwrap();
        assert_eq!(ev.packet, init);
        assert!(
            timeout(Duration::from_millis(150), a1_events.recv())
                .await
                .is_err(),
            "non-owner engine received a foreign init"
        );

        // Sealed to an unrelated third key: nobody accepts it.
        let nobody = WgKeypair::generate();
        let forged = test_genuine_init(&stranger.secret, nobody.public.to_bytes());
        dialer.send_to(&forged, addr).await.unwrap();
        assert!(
            timeout(Duration::from_millis(200), a2_events.recv())
                .await
                .is_err(),
            "an init sealed to no attached engine was forwarded"
        );
    }

    /// P1-a lock: indices are unique across handles for the process life —
    /// they key the cross-engine demux, so a duplicate would alias sessions.
    #[tokio::test]
    async fn alloc_index_is_unique_across_engines() {
        let plane = CarrierPlane::new();
        let (tx_e, _rx_e) = mpsc::channel(1);
        let (tx_t, _rx_t) = mpsc::channel(1);
        let kp1 = WgKeypair::generate();
        let h1 = plane.attach(EngineHooks {
            secret: kp1.secret.clone(),
            public: kp1.public,
            direct_events: tx_e.clone(),
            tun_tx: tx_t.clone(),
        });
        let kp2 = WgKeypair::generate();
        let h2 = plane.attach(EngineHooks {
            secret: kp2.secret.clone(),
            public: kp2.public,
            direct_events: tx_e,
            tun_tx: tx_t,
        });
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            for h in [&h1, &h2] {
                let idx = h.alloc_index();
                assert!(idx > 0 && idx < INDEX_SPACE);
                assert!(seen.insert(idx), "index handed out twice: {idx}");
            }
        }
    }

    /// The srflx-watch send must persist the value even with ZERO receivers —
    /// the real ordering is: ensure_srflx gathers and publishes BEFORE any
    /// runtime's forwarder subscribes. Plain `watch::Sender::send` DROPS the
    /// value when there are no receivers (the plane keeps none), which left
    /// every forwarder reading `[]` and relay-locked the gather-gate loser
    /// (field 2026-08-10). `send_replace` stores unconditionally.
    #[tokio::test]
    async fn srflx_watch_persists_value_sent_before_any_subscriber() {
        let plane = CarrierPlane::new();
        let shared = SrflxShared {
            candidates: vec!["1.2.3.4:43648".to_string()],
            my_nat: Some("cone".to_string()),
            generation: 1,
            ..Default::default()
        };
        // Publish BEFORE any forwarder subscribes (the exact production order).
        plane.lock().srflx_watch.send_replace(shared);
        // A late subscriber (the forwarder) MUST see the candidate, not `[]`.
        let rx = plane.subscribe_srflx();
        assert_eq!(rx.borrow().candidates, vec!["1.2.3.4:43648".to_string()]);
        assert_eq!(rx.borrow().my_nat.as_deref(), Some("cone"));
    }

    /// P1-d protocol lock: a rebuild delivers Teardown to every subscriber,
    /// waits for their acks, then delivers Ready; a second request while one
    /// is in flight is swallowed, a non-authoritative one inside the
    /// cooldown is swallowed, and an authoritative one is not. The no-ack
    /// straggler path re-binds after the timeout (paused time). Runs on an
    /// UNBOUND plane — protocol only; the bind half is the runtime's own
    /// rebuild coverage.
    #[tokio::test(start_paused = true)]
    async fn rebuild_delivers_teardown_then_ready_to_every_subscriber() {
        let plane = CarrierPlane::new();
        let mut r1 = plane.subscribe_events();
        let mut r2 = plane.subscribe_events();

        plane.request_rebuild("test", true);
        plane.request_rebuild("test-dup", true); // swallowed: in flight

        let a1 = match r1.recv().await {
            Some(PlaneEvent::Teardown { ack }) => ack,
            _ => panic!("subscriber 1 expected Teardown"),
        };
        let a2 = match r2.recv().await {
            Some(PlaneEvent::Teardown { ack }) => ack,
            _ => panic!("subscriber 2 expected Teardown"),
        };
        a1.send(()).await.unwrap();
        a2.send(()).await.unwrap();
        assert!(matches!(r1.recv().await, Some(PlaneEvent::Ready)));
        assert!(matches!(r2.recv().await, Some(PlaneEvent::Ready)));

        // Cooldown (real-time clock): a net-change right after is swallowed…
        plane.request_rebuild("net-change", false);
        // …an authoritative resume is not. Nobody acks this round — the 3 s
        // straggler timeout (paused tokio time auto-advances) re-binds and
        // Ready still arrives.
        plane.request_rebuild("resume", true);
        assert!(matches!(r1.recv().await, Some(PlaneEvent::Teardown { .. })));
        assert!(matches!(r2.recv().await, Some(PlaneEvent::Teardown { .. })));
        assert!(matches!(r1.recv().await, Some(PlaneEvent::Ready)));
        assert!(matches!(r2.recv().await, Some(PlaneEvent::Ready)));
    }

    /// PR-B1 — the ensure_bound race lock: two orgs attaching concurrently
    /// must produce exactly ONE bind pass and the SAME socket set. Field
    /// 2026-08-10: without the gate, both passed the empty-view check, both
    /// bound, and the loser's sockets stayed bound + advertised with no
    /// reader (Recv-Q pegged at rmem) — mars/jupiter relay-locked fleet-wide.
    /// Binds REAL host ports (band-walking if a local daemon holds the base);
    /// on a host with no usable LAN interface both callers get `None`, and
    /// the ≤1 assertion still locks the invariant.
    #[tokio::test]
    async fn ensure_bound_binds_once_under_concurrent_attach() {
        let plane = CarrierPlane::new();
        let (a, b) = tokio::join!(plane.ensure_bound(), plane.ensure_bound());
        assert!(
            plane.binds_performed.load(Ordering::Relaxed) <= 1,
            "concurrent ensure_bound calls ran the bind section more than once"
        );
        match (a, b) {
            (Some(a), Some(b)) => {
                assert_eq!(a.endpoints, b.endpoints);
                assert!(
                    Arc::ptr_eq(&a.socks[0].1, &b.socks[0].1),
                    "both callers must share the SAME bound sockets, not parallel sets"
                );
            }
            (None, None) => {} // no usable LAN interface (CI container)
            _ => panic!("one caller bound while the other didn't"),
        }
    }
}
