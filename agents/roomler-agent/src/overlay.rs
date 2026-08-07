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
use tunnel_core::overlay::runtime::{
    DerpMuxFactory, OverlayEvent, OverlayRuntime, RegionalDerpFactory, TunFactory,
};
#[cfg(feature = "overlay-l3")]
use tunnel_core::overlay::tun::SystemTun;
use tunnel_core::overlay::tun::TunIo;

use crate::config::AgentConfig;

/// Overlay MTU. 1280 (the IPv6 minimum) is safe under WireGuard + coturn
/// overhead on any path.
const OVERLAY_MTU: u16 = 1280;

/// rc.307 (B) — everything a spawned runtime's behavior was derived from.
/// A reconnect with an EQUAL fingerprint re-attaches the persistent runtime
/// (carriers survive); any difference spawns a fresh one (the old dies when
/// its last event-sender clone drops). Live-read env flags (MBB, DERP, tier
/// gates, …) are deliberately NOT here — `make_join` re-reads them on every
/// re-join, so they don't need a rebuild.
#[derive(Clone, PartialEq)]
struct RuntimeFingerprint {
    wg_public_key: String,
    netstack_port: Option<u16>,
    advertised_routes: Vec<String>,
    exit_node: Option<String>,
    tenant_id: String,
    server_url: String,
    /// The local LAN interface IP set (sorted). The config-derived fields
    /// above are process-immutable (config changes restart the daemon), so
    /// this is the ONE input that actually changes at runtime: after a
    /// network move the persistent runtime's per-interface sockets are bound
    /// to dead addresses and its punch socket STUNs from a corpse — a
    /// changed set forces the full rebuild a move needs. IP set only (not
    /// ifindexes): Windows renumbers ifindexes on adapter events without the
    /// addresses changing, and the sockets are bound to addresses.
    lan_ips: Vec<String>,
}

struct RuntimeSlot {
    fingerprint: RuntimeFingerprint,
    evt_tx: mpsc::Sender<OverlayEvent>,
}

/// rc.307 (B) — the process-lifetime runtime slots. The overlay runtime used
/// to be scoped to ONE control-WS session (every server deploy / pod roll /
/// receive-liveness cycle tore down all carriers and rebuilt from empty —
/// which relay-locks a corp-VPN'd host whose firewall only grandfathers
/// established UDP flows). Now the first session spawns the runtime and
/// every later session RE-ATTACHES it (`OverlayEvent::Reattach`): the
/// runtime swaps its outbound sender, re-joins, and reconciles the reply
/// netmap against live state. The data plane (TUN, WG peers, carriers)
/// keeps flowing through the whole control-plane outage.
///
/// Multi-org P2c — keyed by TENANT: each org supervisor owns its own
/// persistent runtime, so org B's fresh spawn can never evict org A's
/// re-attach entry (a single slot would ping-pong and both orgs would
/// rebuild-from-empty on every reconnect — exactly the churn rc.307 killed).
static RUNTIME_SLOTS: std::sync::Mutex<std::collections::BTreeMap<String, RuntimeSlot>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// If overlay is enabled, spawn the node runtime (relay mode) and return
/// the channel its control events arrive on. `None` when overlay is
/// disabled or the node has no persisted WG key (generated at startup in
/// `main`, so a missing one here means a misconfiguration).
pub async fn maybe_start(
    cfg: &AgentConfig,
    outbound: mpsc::Sender<ClientMsg>,
    peer_view: watch::Sender<OverlayView>,
    derp_ticket_slot: crate::relay_probe::DerpTicketSlot,
) -> Option<mpsc::Sender<OverlayEvent>> {
    if !cfg.overlay_enabled {
        // Drop THIS org's persistent-runtime slot entry: once the sessions'
        // event-sender clones are gone too, its channel closes and it tears
        // down cleanly. Other orgs' runtimes are untouched.
        RUNTIME_SLOTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&cfg.tenant_id);
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

    // rc.307 (B) — re-attach the persistent runtime when nothing that shaped
    // it changed. Take the sender clone OUTSIDE the lock before awaiting.
    let fingerprint = RuntimeFingerprint {
        wg_public_key: keypair.public_base64(),
        netstack_port: netstack_socks_port(),
        advertised_routes: cfg.effective_overlay_advertised_routes(),
        exit_node: cfg.overlay_exit_node.clone(),
        tenant_id: cfg.tenant_id.clone(),
        server_url: cfg.server_url.clone(),
        lan_ips: {
            let mut ips: Vec<String> = tunnel_core::overlay::direct::gather_lan_interfaces()
                .into_iter()
                .map(|(ip, _)| ip.to_string())
                .collect();
            ips.sort();
            ips
        },
    };
    let existing = {
        let slots = RUNTIME_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .get(&cfg.tenant_id)
            .filter(|s| s.fingerprint == fingerprint)
            .map(|s| s.evt_tx.clone())
    };
    if let Some(evt_tx) = existing {
        match evt_tx
            .send(OverlayEvent::Reattach {
                outbound: outbound.clone(),
            })
            .await
        {
            Ok(()) => {
                info!("overlay: re-attached the persistent node runtime (carriers intact)");
                return Some(evt_tx);
            }
            Err(_) => {
                // The runtime exited (self_ip change, TUN failure). Fall
                // through to a fresh spawn.
                info!("overlay: previous node runtime exited; starting fresh");
            }
        }
    }

    let (evt_tx, evt_rx) = mpsc::channel::<OverlayEvent>(64);

    // Pick the overlay surface: the userspace netstack (+ loopback SOCKS front)
    // when `ROOMLER_AGENT_OVERLAY_NETSTACK_SOCKS` names a port, else the OS TUN.
    // Either surface can be absent at build time; the helper warns + `None`s,
    // and `?` aborts the (mis)configured start.
    //
    // Multi-org P2c: with `overlay_multi_org` on, EVERY org (primary
    // included) goes through the process-wide shared-TUN mux, keyed by its
    // tenant id — and netstack mode is overridden (its SOCKS front and
    // handle channel are process-global singletons; N netstack orgs would
    // fight over them).
    let tun_factory: TunFactory = if cfg.overlay_multi_org {
        if netstack_socks_port().is_some() {
            warn!(
                "overlay: OVERLAY_NETSTACK_SOCKS is set but overlay_multi_org is on — \
                 netstack mode is single-org; using the shared OS TUN instead"
            );
        }
        mux_systun_factory(cfg.tenant_id.clone())?
    } else {
        match netstack_socks_port() {
            // Give the netstack SOCKS front a live mesh view so it can resolve
            // DOMAIN targets (peer name / MagicDNS FQDN → overlay IP). Same channel
            // the runtime publishes to below, so it's stable across reconnects.
            Some(port) => netstack_tun_factory(port, peer_view.subscribe(), &cfg.tenant_id)?,
            None => systun_tun_factory()?,
        }
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
        let tenant = cfg.tenant_id.clone();
        let pubkey = keypair.public.to_bytes();
        Some(Box::new(move || {
            let (mux, outbound_rx) = tunnel_core::transport::derp::DerpMux::new(pubkey);
            crate::derp::spawn(&ws_url, &token, &tenant, &mux, outbound_rx);
            info!("overlay derp: /derp carrier opened (node UDP-blocked; both-UDP-blocked tier)");
            mux
        }))
    } else {
        None
    };

    // Multi-region DERP — the per-URL regional relay opener the force-DERP
    // handler consults when a push carries a `derp_url`. Reads the admission
    // ticket cached by the signaling loop; no ticket yet ⇒ `None` (the
    // coordinator degrades to the central mux; the WS owner spawned on a
    // later successful open reads the slot per attempt anyway).
    let regional_derp_factory: Option<RegionalDerpFactory> =
        if tunnel_core::overlay::direct::derp_enabled() {
            let tenant = cfg.tenant_id.clone();
            let pubkey = keypair.public.to_bytes();
            let slot = derp_ticket_slot.clone();
            Some(Box::new(move |url: &str| {
                let has_ticket = slot.lock().unwrap_or_else(|e| e.into_inner()).is_some();
                if !has_ticket {
                    warn!(%url, "overlay derp: regional relay pushed but no admission ticket yet");
                    return None;
                }
                let (mux, outbound_rx) = tunnel_core::transport::derp::DerpMux::new(pubkey);
                crate::derp::spawn_regional(url, slot.clone(), &tenant, &mux, outbound_rx);
                info!(%url, "overlay derp: regional /derp carrier opened");
                Some(mux)
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
        // Multi-region DERP — per-URL regional relay opener (ticket-gated).
        .with_regional_derp_factory(regional_derp_factory)
        // Unification P1 — publish the live mesh view for the LocalAPI so
        // `roomler status` / `peers` see per-device connection types.
        .with_peer_view(peer_view);
    // FIELD: endpoints are advertised lazily — the relay coordinator
    // trickles each relayed address post-allocation — so join carries none.
    tokio::spawn(rt.run(evt_rx, Vec::new()));
    info!("overlay: node runtime started (relay mode)");
    // rc.307 (B) — remember it for the next session's re-attach, under THIS
    // org's key. Replacing a fingerprint-mismatched entry drops the old
    // runtime's last stored sender clone; it tears down once the old
    // session's clones are gone too.
    RUNTIME_SLOTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            cfg.tenant_id.clone(),
            RuntimeSlot {
                fingerprint,
                evt_tx: evt_tx.clone(),
            },
        );
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

/// rc.280 — process-lifetime TUN cache. The runtime builds its device per WS
/// SESSION (first netmap) and drops it when `run()` returns, which used to
/// REMOVE the Wintun adapter on every reconnect (wintun's close-of-created
/// removes) and re-create it seconds later — the ifIndex/NLA churn multiplier
/// on top of the per-create random GUID (fixed in rc.279), and the source of
/// the rc.209 "device installation mutex" race between a dying session's
/// release and the next session's create. Caching the `Arc` here keeps the
/// device alive across sessions: a reconnect with the same `(ip, netmask,
/// mtu)` reuses it (metric pin, derived v6, WFP guard, Private profile all
/// still in place); a changed tuple (re-IP / re-enroll) drops the old device
/// first, then builds fresh. The static itself is never dropped, so even a
/// clean process exit leaves the adapter installed — the next process opens
/// it by name or re-forms the same identity via the rc.279 stable GUID.
#[cfg(feature = "overlay-l3")]
#[allow(clippy::type_complexity)]
static SYSTUN_CACHE: std::sync::Mutex<
    Option<(
        (std::net::Ipv4Addr, std::net::Ipv4Addr, u16),
        Arc<SystemTun>,
    )>,
> = std::sync::Mutex::new(None);

/// OS-TUN factory (`overlay-l3`). The agent is privileged, so the device +
/// routes come up directly in `SystemTun::up`. Kill-switch for the cache:
/// `overlay_tun_persist` / `ROOMLER_NODE_OVERLAY_TUN_PERSIST`, default ON —
/// `0` restores the per-session create/remove cycle.
#[cfg(feature = "overlay-l3")]
fn systun_tun_factory() -> Option<TunFactory> {
    Some(Box::new(|ip, nm, mtu| {
        if !tunnel_core::env::flag("OVERLAY_TUN_PERSIST", true) {
            return SystemTun::up(ip, nm, mtu).map(|t| Arc::new(t) as Arc<dyn TunIo>);
        }
        let mut cache = SYSTUN_CACHE.lock().unwrap();
        if let Some((params, dev)) = cache.as_ref()
            && *params == (ip, nm, mtu)
            && dev.is_alive()
        {
            info!("overlay: reusing the process-lifetime TUN device (reconnect)");
            return Ok(dev.clone() as Arc<dyn TunIo>);
        }
        // Param change (re-IP) or dead device: release the old one BEFORE the
        // new create so the adapter's device-install lock frees up (the
        // rc.209 mutex retry inside `up` covers the release latency).
        *cache = None;
        let dev = Arc::new(SystemTun::up(ip, nm, mtu)?);
        *cache = Some(((ip, nm, mtu), dev.clone()));
        Ok(dev as Arc<dyn TunIo>)
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

/// Multi-org P2c — the process-wide shared device + its [`TunMux`], for the
/// mux factory below. The FIRST org to establish creates the device with its
/// own `(ip, netmask, mtu)` (exactly what `SystemTun::up` always took); every
/// later org rides the same device with its own address added. Rebuilt from
/// scratch when the device dies (`is_alive` false) — the stale mux's reader
/// has EOF'd every old port by then, so each org's runtime rebuilds its
/// session and re-registers on the fresh one.
#[cfg(feature = "overlay-l3")]
#[allow(clippy::type_complexity)]
static SHARED_MUX: std::sync::Mutex<
    Option<(Arc<SystemTun>, Arc<tunnel_core::overlay::tun_mux::TunMux>)>,
> = std::sync::Mutex::new(None);

/// Multi-org — release everything process-wide an org held: its shared-TUN
/// port + address, and the netstack surface if it owned that.
///
/// Called when an org's supervised loop ENDS for good (disabled, removed,
/// terminal error) — until now the address stayed up until the daemon
/// restarted (docs/multi-org.md §12). Harmless but untidy, and on a
/// long-lived multi-org host the litter accumulates.
///
/// No-op for anything the org never claimed (single-org daemons never
/// register), so it is safe to call unconditionally on teardown.
pub fn release_org(org_key: &str) {
    release_shared_tun(org_key);
    release_netstack(org_key);
}

#[cfg(feature = "overlay-l3")]
fn release_shared_tun(org_key: &str) {
    let shared = SHARED_MUX.lock().unwrap_or_else(|e| e.into_inner());
    let Some((dev, mux)) = shared.as_ref() else {
        return;
    };
    if let Some((addr, prefix)) = mux.deregister(org_key) {
        dev.del_address_sync(addr, prefix);
        info!(org = %org_key, %addr, "overlay: released the org's shared-TUN claim");
    }
}
#[cfg(not(feature = "overlay-l3"))]
fn release_shared_tun(_org_key: &str) {}

/// Hand the netstack surface back when its owner leaves, so a remaining org
/// can take it on its next reconnect instead of withholding forever.
#[cfg(feature = "overlay-netstack")]
fn release_netstack(org_key: &str) {
    let mut owner = NETSTACK_OWNER.lock().unwrap_or_else(|e| e.into_inner());
    if owner.as_deref() == Some(org_key) {
        *owner = None;
        info!(org = %org_key, "overlay netstack: released the host's netstack surface");
    }
}
#[cfg(not(feature = "overlay-netstack"))]
fn release_netstack(_org_key: &str) {}

/// Multi-org P2c — per-org TUN factory over the ONE shared device.
///
/// Differences from [`systun_tun_factory`], all deliberate:
/// * the device is ALWAYS process-lifetime (the mux and its reader pump are
///   shared state; the `overlay_tun_persist` kill-switch does not apply);
/// * a registration can be REFUSED (`AddrInUse`) when another org already
///   claims the same block — the two-un-migrated-tenants case. The runtime
///   treats that like any TUN bring-up failure: this org's session aborts
///   and retries on its next reconnect, while the OTHER orgs' meshes stay
///   up. The fix is renumbering one tenant (docs/multi-org.md §4.3).
#[cfg(feature = "overlay-l3")]
fn mux_systun_factory(org_key: String) -> Option<TunFactory> {
    use tunnel_core::overlay::tun_mux::TunMux;
    Some(Box::new(move |ip, nm, mtu| {
        let mut shared = SHARED_MUX.lock().unwrap();
        let (dev, mux, fresh) = match shared.as_ref() {
            Some((dev, mux)) if dev.is_alive() && mux.is_alive() => {
                (dev.clone(), mux.clone(), false)
            }
            _ => {
                // Dead or first use: release the old device BEFORE the new
                // create (frees the adapter's device-install lock; the
                // retry inside `up` covers the release latency).
                *shared = None;
                let dev = Arc::new(SystemTun::up(ip, nm, mtu)?);
                let mux = TunMux::new(dev.clone() as Arc<dyn TunIo>);
                *shared = Some((dev.clone(), mux.clone()));
                info!("overlay: shared multi-org TUN device created");
                (dev, mux, true)
            }
        };
        // Claim the block FIRST. A REFUSED registration (another org already
        // holds this block) must not leave an address on the adapter — field
        // 2026-08-05 left three of them behind across the fleet, addresses
        // nothing answered on, which is exactly the litter that makes a
        // later diagnosis lie.
        let port = mux.register(&org_key, ip, nm)?;
        // Then the org's own address: its block's connected route rides the
        // assignment. The creator's address came up with the device;
        // add_address_sync is idempotent, so skipping the create params only
        // avoids a pointless netsh round-trip.
        if !fresh {
            let prefix = u32::from(nm).count_ones() as u8;
            if let Err(e) = dev.add_address_sync(ip, prefix) {
                // Roll the claim back — a block held by an org with no
                // address to receive on would refuse every later joiner.
                if let Some((addr, plen)) = mux.deregister(&org_key) {
                    dev.del_address_sync(addr, plen);
                }
                return Err(e);
            }
        }
        Ok(port as Arc<dyn TunIo>)
    }))
}
#[cfg(not(feature = "overlay-l3"))]
fn mux_systun_factory(_org_key: String) -> Option<TunFactory> {
    warn!(
        "overlay: overlay_multi_org requires an `overlay-l3` build (the shared \
         OS TUN); not joining"
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

/// Multi-org — which org owns the process-wide netstack surface.
///
/// The netstack surface is a set of singletons: ONE handle channel, ONE
/// loopback SOCKS port, ONE `roomler ping` backend. Nothing about them is
/// org-scoped, so a second org publishing to [`ns_handle_tx`] does not join
/// the mesh twice — it REPLACES the first org's stack underneath the SOCKS
/// front and the pinger, which keep serving the same port. A user dialing
/// through SOCKS believing they reach org A would silently be routed by org
/// B's stack.
///
/// So the surface is CLAIMED. First org in owns it; a second org withholds
/// its overlay (loud warning, retried on its next reconnect) exactly like a
/// refused shared-TUN registration — the org that cannot have the surface
/// goes without a mesh rather than taking someone else's. Re-claiming by the
/// owner is a no-op, which is what every reconnect does.
///
/// Untangling the statics into per-org instances is the real fix and is
/// deliberately not done here (docs/multi-org.md §12): it is significant work
/// for a case nobody has hit, whereas silent cross-org misrouting is a bug
/// whatever the demand.
#[cfg(feature = "overlay-netstack")]
static NETSTACK_OWNER: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Claim the netstack surface for `org_key`. `false` ⇒ another org holds it.
#[cfg(feature = "overlay-netstack")]
fn claim_netstack(org_key: &str) -> bool {
    let mut owner = NETSTACK_OWNER.lock().unwrap_or_else(|e| e.into_inner());
    match owner.as_deref() {
        Some(cur) => cur == org_key,
        None => {
            *owner = Some(org_key.to_string());
            true
        }
    }
}

/// Netstack factory (`overlay-netstack`): each (re)connect builds a fresh
/// userspace stack and publishes its handle to the process-wide loopback SOCKS
/// front (bound once), so the front outlives reconnects without rebinding.
///
/// `None` when another org already owns the surface — see [`NETSTACK_OWNER`].
#[cfg(feature = "overlay-netstack")]
fn netstack_tun_factory(
    socks_port: u16,
    view_rx: watch::Receiver<OverlayView>,
    org_key: &str,
) -> Option<TunFactory> {
    use std::net::Ipv4Addr;
    use tunnel_core::overlay::netstack::Netstack;
    use tunnel_core::overlay::netstack_socks::serve_socks5;

    if !claim_netstack(org_key) {
        warn!(
            org = %org_key,
            "overlay netstack: another organization already owns this host's netstack \
             surface (one userspace stack, one SOCKS port, one ping backend). This org \
             joins no mesh — enable `overlay_multi_org` to put both orgs on the shared \
             OS TUN instead."
        );
        return None;
    }

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
    _org_key: &str,
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
        // P7 — server-pushed per-pair DERP escalation (corp TURN churn).
        ServerMsg::OverlayForceDerp {
            peer_node_id,
            ttl_ms,
            derp_url,
        } => OverlayEvent::ForceDerp {
            peer_node_id,
            ttl_ms,
            derp_url,
        },
        other => return Some(other),
    };
    if evt_tx.try_send(evt).is_err() {
        warn!("overlay: event channel full/closed; dropping a netmap update");
    }
    None
}

#[cfg(all(test, feature = "overlay-netstack"))]
mod netstack_claim_tests {
    use super::{claim_netstack, release_netstack};

    /// The netstack surface is one stack, one SOCKS port, one ping backend.
    /// Whoever holds it holds it exclusively — a second org must be refused
    /// rather than silently replacing the first org's stack under a SOCKS
    /// front that keeps answering on the same port.
    #[test]
    fn the_netstack_surface_has_exactly_one_owner() {
        // Deliberately serialized in ONE test: the claim is process-global,
        // so separate #[test] fns would race each other.
        assert!(claim_netstack("org-a"), "first org in takes the surface");
        assert!(
            claim_netstack("org-a"),
            "re-claim by the owner is a no-op — every reconnect does this"
        );
        assert!(
            !claim_netstack("org-b"),
            "a second org is refused, not served"
        );

        // A non-owner leaving must not hand away someone else's surface.
        release_netstack("org-b");
        assert!(!claim_netstack("org-b"), "still org-a's");

        // The owner leaving frees it for the org that was withholding.
        release_netstack("org-a");
        assert!(claim_netstack("org-b"), "the waiting org can take over");
        release_netstack("org-b");
    }
}

/// Multi-org P2c — the shared-TUN factory against REAL kernel networking.
///
/// [`tunnel_core::overlay::tun_mux`]'s own tests cover the bookkeeping with a
/// fake device. What they cannot cover is the half that touches the OS: that
/// a REFUSED registration leaves no address behind on the adapter, that a
/// disjoint block really does get its address, and that releasing an org
/// really does take it off again. That half was unit-locked but never run
/// against a kernel (docs/multi-org.md §12) because reproducing it in the
/// field needs a third organization — and there is no way to retire a
/// throwaway org afterwards.
///
/// A real `tun` device costs nothing to create, so the OS half runs here
/// instead: same factory, same `SystemTun`, same `ip addr` calls, real
/// kernel. It needs CAP_NET_ADMIN, so it is opt-in via
/// `ROOMLER_TUN_KERNEL_TEST=1` (set in the CI job that runs as root) and
/// skips silently otherwise — including on every developer machine.
#[cfg(all(test, feature = "overlay-l3", target_os = "linux"))]
mod tun_kernel_tests {
    use super::{mux_systun_factory, release_org};
    use std::net::Ipv4Addr;

    const MASK_22: Ipv4Addr = Ipv4Addr::new(255, 255, 252, 0);

    /// Addresses the kernel currently has on the overlay device.
    fn addrs() -> String {
        let out = std::process::Command::new("ip")
            .args(["-4", "addr", "show", "dev", "roomler0"])
            .output()
            .expect("ip addr show");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[tokio::test]
    async fn refused_registration_leaves_no_address_on_the_adapter() {
        if std::env::var("ROOMLER_TUN_KERNEL_TEST").as_deref() != Ok("1") {
            eprintln!("skipping: set ROOMLER_TUN_KERNEL_TEST=1 (needs CAP_NET_ADMIN)");
            return;
        }

        // Org A creates the device and takes 100.65.0.0/22.
        let a = mux_systun_factory("kernel-org-a".into()).expect("factory");
        let _port_a = a(Ipv4Addr::new(100, 65, 0, 5), MASK_22, 1280).expect("org A joins");
        assert!(
            addrs().contains("100.65.0.5"),
            "org A's address must be on the device:\n{}",
            addrs()
        );

        // Org B asks for an address inside THE SAME block — the
        // two-un-migrated-tenants case. The mux refuses, and the refusal has
        // to be clean: an address left behind here is one nothing answers
        // on, which is exactly what makes a later diagnosis lie.
        let b = mux_systun_factory("kernel-org-b".into()).expect("factory");
        let err = b(Ipv4Addr::new(100, 65, 1, 9), MASK_22, 1280)
            .expect_err("an overlapping block must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
        let after = addrs();
        assert!(
            !after.contains("100.65.1.9"),
            "a refused org must leave NO address behind:\n{after}"
        );
        assert!(
            after.contains("100.65.0.5"),
            "org A keeps its mesh:\n{after}"
        );

        // A disjoint block is accepted and really does land on the adapter.
        let c = mux_systun_factory("kernel-org-c".into()).expect("factory");
        let _port_c = c(Ipv4Addr::new(100, 66, 0, 7), MASK_22, 1280).expect("org C joins");
        assert!(
            addrs().contains("100.66.0.7"),
            "org C's address must be on the device:\n{}",
            addrs()
        );

        // …and releasing that org takes it back off (the litter fix).
        release_org("kernel-org-c");
        let after = addrs();
        assert!(
            !after.contains("100.66.0.7"),
            "a released org's address must be gone:\n{after}"
        );
        assert!(
            after.contains("100.65.0.5"),
            "and only that org's — A is untouched:\n{after}"
        );

        release_org("kernel-org-a");
    }
}
