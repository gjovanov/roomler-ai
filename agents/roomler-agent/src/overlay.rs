//! Agent-side overlay-network glue (Phase 3b, feature `overlay-l3`).
//!
//! Bridges the agent's WS signaling loop to the shared
//! [`OverlayRuntime`](tunnel_core::overlay::runtime::OverlayRuntime): on
//! connect it spawns the runtime (relay mode) and returns the channel its
//! `ServerMsg::Overlay*` events flow into; the WS read loop forwards those
//! via [`intercept`].
//!
//! The agent runs as a privileged service, so the real `SystemTun` device
//! comes up here directly (no helper — that's the unprivileged
//! tunnel-client's story). Default-OFF: only an `overlay_enabled` config
//! plus a build with `--features overlay-l3` joins the mesh.

use std::sync::Arc;

use roomler_ai_remote_control::signaling::{ClientMsg, ServerMsg};
use tokio::sync::mpsc;
use tracing::{info, warn};

use tunnel_core::overlay::WgKeypair;
use tunnel_core::overlay::runtime::{OverlayEvent, OverlayRuntime, TunFactory};
use tunnel_core::overlay::tun::{SystemTun, TunIo};

use crate::config::AgentConfig;

/// Overlay MTU. 1280 (the IPv6 minimum) is safe under WireGuard + coturn
/// overhead on any path.
const OVERLAY_MTU: u16 = 1280;

/// If overlay is enabled, spawn the node runtime (relay mode) and return
/// the channel its control events arrive on. `None` when overlay is
/// disabled or the node has no persisted WG key (generated at startup in
/// `main`, so a missing one here means a misconfiguration).
pub fn maybe_start(
    cfg: &AgentConfig,
    outbound: mpsc::Sender<ClientMsg>,
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
    let tun_factory: TunFactory =
        Box::new(|ip, nm, mtu| SystemTun::up(ip, nm, mtu).map(|t| Arc::new(t) as Arc<dyn TunIo>));
    let rt = OverlayRuntime::new_relay(keypair, outbound, tun_factory, OVERLAY_MTU);
    // FIELD: endpoints are advertised lazily — the relay coordinator
    // trickles each relayed address post-allocation — so join carries none.
    tokio::spawn(rt.run(evt_rx, Vec::new()));
    info!("overlay: node runtime started (relay mode)");
    Some(evt_tx)
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
