//! Agent-side overlay-network glue (Phase 3b).
//!
//! Bridges the agent's WS signaling loop to the shared
//! [`OverlayRuntime`](tunnel_core::overlay::runtime::OverlayRuntime): on
//! connect it spawns the runtime (relay mode) and returns the channel its
//! `ServerMsg::Overlay*` events flow into; the WS read loop forwards those
//! via [`intercept`].
//!
//! Two overlay surfaces, picked at runtime:
//! * **`overlay-l3`** — a real OS TUN (`SystemTun`). The agent runs privileged
//!   (service), so the device + routes come up directly. The default when no
//!   netstack port is set.
//! * **`overlay-netstack`** — a userspace smoltcp stack + a loopback SOCKS5
//!   front, the OS-free twin: on a locked-down host (full-tunnel VPN) the mesh
//!   is reachable with NO OS routing. Opt in with the env var
//!   `ROOMLER_AGENT_OVERLAY_NETSTACK_SOCKS=<port>`.
//!
//! Default-OFF regardless: `overlay_enabled` config **and** a build carrying
//! the relevant feature are both required to join the mesh.

use std::sync::Arc;

use roomler_ai_remote_control::signaling::{ClientMsg, ServerMsg};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use tunnel_core::env::node_env;
use tunnel_core::localapi::OverlayView;
use tunnel_core::overlay::WgKeypair;
use tunnel_core::overlay::runtime::{DerpMuxFactory, OverlayEvent, OverlayRuntime, TunFactory};
#[cfg(feature = "overlay-l3")]
use tunnel_core::overlay::tun::SystemTun;
use tunnel_core::overlay::tun::TunIo;

use crate::config::AgentConfig;

/// Overlay MTU. 1280 (the IPv6 minimum) is safe under WireGuard + coturn
/// overhead on any path.
const OVERLAY_MTU: u16 = 1280;

/// If overlay is enabled, spawn the node runtime (relay mode) and return
/// the channel its control events arrive on. `None` when overlay is
/// disabled or the node has no persisted WG key (generated at startup in
/// `main`, so a missing one here means a misconfiguration).
pub async fn maybe_start(
    cfg: &AgentConfig,
    outbound: mpsc::Sender<ClientMsg>,
    peer_view: watch::Sender<OverlayView>,
) -> Option<mpsc::Sender<OverlayEvent>> {
    if !cfg.overlay_enabled {
        return None;
    }
    let Some(keypair) = cfg
        .overlay_wg_secret_key
        .as_deref()
        .and_then(WgKeypair::from_secret_base64)
    else {
        warn!("overlay enabled but no/invalid WG key persisted; not joining the mesh");
        return None;
    };

    let (evt_tx, evt_rx) = mpsc::channel::<OverlayEvent>(64);

    // Pick the overlay surface: the userspace netstack (+ loopback SOCKS front)
    // when `ROOMLER_AGENT_OVERLAY_NETSTACK_SOCKS` names a port, else the OS TUN.
    // Either surface can be absent at build time; the helper warns + `None`s,
    // and `?` aborts the (mis)configured start.
    let tun_factory: TunFactory = match netstack_socks_port() {
        // Give the netstack SOCKS front a live mesh view so it can resolve
        // DOMAIN targets (peer name / MagicDNS FQDN → overlay IP). Same channel
        // the runtime publishes to below, so it's stable across reconnects.
        Some(port) => netstack_tun_factory(port, peer_view.subscribe())?,
        None => systun_tun_factory()?,
    };
    // P5 exit-node client — resolve the coordination server's IPs NOW, while the
    // uplink is still clean (before any split-default is installed), so exit
    // routing can exempt them. Only when this node opts into an exit node.
    let exit_server_ips = if cfg.overlay_exit_node.is_some() {
        resolve_server_ips(&cfg.server_url).await
    } else {
        Vec::new()
    };

    // Phase D (DERP) — when enabled, provide a factory that opens the persistent
    // `/derp` WS. The runtime calls it LAZILY — only if THIS node is UDP-blocked
    // (its srflx gather found nothing) — so a UDP-capable node (which can never
    // be in a both-UDP-blocked pair) never holds an idle `/derp` WS. When
    // called, it builds the demux, opens the WS (both peers dial OUT over
    // TCP/TLS-443), and returns the mux. Default-ON since rc.203.
    let derp_factory: Option<DerpMuxFactory> = if tunnel_core::overlay::direct::derp_enabled() {
        let ws_url = cfg.ws_url();
        let token = cfg.agent_token.clone();
        let pubkey = keypair.public.to_bytes();
        Some(Box::new(move || {
            let (mux, outbound_rx) = tunnel_core::transport::derp::DerpMux::new(pubkey);
            crate::derp::spawn(&ws_url, &token, &mux, outbound_rx);
            info!("overlay derp: /derp carrier opened (node UDP-blocked; both-UDP-blocked tier)");
            mux
        }))
    } else {
        None
    };

    let rt = OverlayRuntime::new_relay(keypair, outbound, tun_factory, OVERLAY_MTU)
        // Phase 1 — advertise this node's subnet routes (admin-gated server-side).
        // P5 — plus `0.0.0.0/0` when this node is configured as an exit node.
        .with_advertised_routes(cfg.effective_overlay_advertised_routes())
        // P5 — route THIS node's default egress through a chosen exit peer (with
        // carrier-endpoint exemptions), when `overlay_exit_node` is set.
        .with_exit_node(cfg.overlay_exit_node.clone(), exit_server_ips)
        // Phase D — LAZY `/derp`: the runtime opens the WS via this factory only
        // if the node is itself UDP-blocked (else no idle WS).
        .with_derp_mux_factory(derp_factory)
        // Unification P1 — publish the live mesh view for the LocalAPI so
        // `roomler status` / `peers` see per-device connection types.
        .with_peer_view(peer_view);
    // FIELD: endpoints are advertised lazily — the relay coordinator
    // trickles each relayed address post-allocation — so join carries none.
    tokio::spawn(rt.run(evt_rx, Vec::new()));
    info!("overlay: node runtime started (relay mode)");
    Some(evt_tx)
}

/// Resolve the coordination server's host (from `server_url`) to its current
/// IPs. Exit-node routing exempts these from the split-default, and they MUST be
/// resolved BEFORE any `0.0.0.0/1` is installed — once the default is captured,
/// DNS to a remote resolver may itself be swallowed. Best-effort + timeout-bound:
/// an empty result just means the runtime's exemption gate withholds default
/// routing (fail-safe — never a wedge). roomler.ai sits behind nginx/HAProxy and
/// may be multi-A, so every returned address is exempted.
async fn resolve_server_ips(server_url: &str) -> Vec<std::net::IpAddr> {
    use std::collections::HashSet;
    use std::net::IpAddr;
    use std::time::Duration;

    // Host out of `scheme://host[:port][/path]` (server_url is never a v6 literal).
    let authority = server_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(server_url)
        .split('/')
        .next()
        .unwrap_or("");
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority)
        .trim();
    if host.is_empty() {
        return Vec::new();
    }
    match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, 443u16)),
    )
    .await
    {
        Ok(Ok(addrs)) => {
            let ips: Vec<IpAddr> = addrs
                .map(|s| s.ip())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            info!(
                %host,
                count = ips.len(),
                "overlay exit-node: resolved coordination-server IPs for carrier exemption"
            );
            ips
        }
        Ok(Err(e)) => {
            warn!(%host, %e, "overlay exit-node: coordination-server DNS resolve failed; exit routing withholds until exemptions are known");
            Vec::new()
        }
        Err(_) => {
            warn!(%host, "overlay exit-node: coordination-server DNS resolve timed out; exit routing withholds");
            Vec::new()
        }
    }
}

/// The loopback SOCKS5 port for **netstack mode**, from
/// `ROOMLER_AGENT_OVERLAY_NETSTACK_SOCKS`. `None` (the default) selects OS-TUN
/// mode; a zero / unparseable value is treated as unset.
fn netstack_socks_port() -> Option<u16> {
    node_env("OVERLAY_NETSTACK_SOCKS")
        .and_then(|v| v.trim().parse::<u16>().ok())
        .filter(|p| *p != 0)
}

/// OS-TUN factory (`overlay-l3`). The agent is privileged, so the device +
/// routes come up directly in `SystemTun::up`.
#[cfg(feature = "overlay-l3")]
fn systun_tun_factory() -> Option<TunFactory> {
    Some(Box::new(|ip, nm, mtu| {
        SystemTun::up(ip, nm, mtu).map(|t| Arc::new(t) as Arc<dyn TunIo>)
    }))
}
#[cfg(not(feature = "overlay-l3"))]
fn systun_tun_factory() -> Option<TunFactory> {
    warn!(
        "overlay: OS-TUN mode requested but this build lacks `overlay-l3` \
         (set ROOMLER_AGENT_OVERLAY_NETSTACK_SOCKS for netstack mode); not joining"
    );
    None
}

/// Process-wide netstack handle channel — the live [`NetstackHandle`], (re)
/// published on each overlay connect. A `OnceLock` so the SOCKS front (below)
/// and the [`netstack_pinger`] `ping` backend share ONE channel regardless of
/// which touches it first.
#[cfg(feature = "overlay-netstack")]
fn ns_handle_tx() -> &'static watch::Sender<Option<tunnel_core::overlay::netstack::NetstackHandle>>
{
    static NS_HANDLE: std::sync::OnceLock<
        watch::Sender<Option<tunnel_core::overlay::netstack::NetstackHandle>>,
    > = std::sync::OnceLock::new();
    NS_HANDLE.get_or_init(|| watch::channel(None).0)
}

/// Netstack factory (`overlay-netstack`): each (re)connect builds a fresh
/// userspace stack and publishes its handle to the process-wide loopback SOCKS
/// front (bound once), so the front outlives reconnects without rebinding.
#[cfg(feature = "overlay-netstack")]
fn netstack_tun_factory(
    socks_port: u16,
    view_rx: watch::Receiver<OverlayView>,
) -> Option<TunFactory> {
    use std::net::Ipv4Addr;
    use tunnel_core::overlay::netstack::Netstack;
    use tunnel_core::overlay::netstack_socks::serve_socks5;

    // Bind the loopback SOCKS front exactly once, subscribing to the shared
    // handle channel so it always serves whatever stack is currently live.
    static SOCKS_BOUND: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    SOCKS_BOUND.get_or_init(move || {
        let handle_rx = ns_handle_tx().subscribe();
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, socks_port)).await {
                Ok(l) => {
                    info!(
                        port = socks_port,
                        "overlay netstack: SOCKS5 front on 127.0.0.1"
                    );
                    serve_socks5(handle_rx, view_rx, l).await;
                }
                Err(e) => {
                    warn!(port = socks_port, error = %e, "overlay netstack: SOCKS bind failed")
                }
            }
        });
    });

    Some(Box::new(move |ip, nm, mtu| {
        let ns = Netstack::start(ip, netmask_to_prefix(nm), mtu);
        let _ = ns_handle_tx().send(Some(ns.handle.clone()));
        info!(%ip, socks_port, "overlay netstack: userspace stack up (OS-free)");
        Ok(ns.tun as Arc<dyn TunIo>)
    }))
}

/// The netstack ICMP backend for the `roomler ping` LocalAPI verb, watching the
/// shared handle channel. `None` unless this node is in netstack mode
/// (`ROOMLER_AGENT_OVERLAY_NETSTACK_SOCKS` set) — an OS-TUN node has no OS-free
/// ICMP path (the OS `ping` works there).
#[cfg(feature = "overlay-netstack")]
pub fn netstack_pinger() -> Option<Arc<dyn crate::localapi_state::NetstackPinger>> {
    use std::net::IpAddr;
    use std::time::Duration;
    use tunnel_core::overlay::netstack::NetstackHandle;

    // Only meaningful in netstack mode; `?` short-circuits to `None` otherwise.
    netstack_socks_port()?;

    struct NsPinger {
        handle: watch::Receiver<Option<NetstackHandle>>,
    }
    #[async_trait::async_trait]
    impl crate::localapi_state::NetstackPinger for NsPinger {
        async fn ping(&self, dst: IpAddr, timeout: Duration) -> Result<Duration, String> {
            let handle = self
                .handle
                .borrow()
                .clone()
                .ok_or_else(|| "netstack not up yet (mesh not joined)".to_string())?;
            handle.ping(dst, timeout).await.map_err(|e| e.to_string())
        }
    }

    Some(Arc::new(NsPinger {
        handle: ns_handle_tx().subscribe(),
    }))
}
#[cfg(not(feature = "overlay-netstack"))]
fn netstack_tun_factory(
    _socks_port: u16,
    _view_rx: watch::Receiver<OverlayView>,
) -> Option<TunFactory> {
    warn!(
        "overlay: netstack mode requested (ROOMLER_AGENT_OVERLAY_NETSTACK_SOCKS set) \
         but this build lacks `overlay-netstack`; not joining"
    );
    None
}

/// IPv4 netmask → prefix length (count of leading one-bits).
#[cfg(feature = "overlay-netstack")]
fn netmask_to_prefix(nm: std::net::Ipv4Addr) -> u8 {
    u32::from(nm).count_ones() as u8
}

/// Forward an `rc:overlay.*` `ServerMsg` to the runtime. Returns the
/// message untouched if it isn't an overlay message, so the caller's
/// normal dispatch handles everything else.
pub fn intercept(evt_tx: &mpsc::Sender<OverlayEvent>, msg: ServerMsg) -> Option<ServerMsg> {
    let evt = match msg {
        ServerMsg::OverlayNetmap {
            self_ip,
            network,
            peers,
            ..
        } => OverlayEvent::Netmap {
            self_ip,
            network,
            peers,
        },
        ServerMsg::OverlayNetmapDelta {
            upserts, removes, ..
        } => OverlayEvent::NetmapDelta { upserts, removes },
        ServerMsg::OverlayRelayGrant {
            ice_servers,
            peer_node_id,
            pair_key,
        } => OverlayEvent::RelayGrant {
            peer_node_id,
            ice_servers,
            pair_key,
        },
        other => return Some(other),
    };
    if evt_tx.try_send(evt).is_err() {
        warn!("overlay: event channel full/closed; dropping a netmap update");
    }
    None
}
