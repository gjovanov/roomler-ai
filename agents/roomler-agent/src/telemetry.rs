//! Stats PR-5 — heartbeat telemetry sampler.
//!
//! Fills the [`AgentSysStats`] block on `rc:agent.heartbeat`: agent-process
//! rss/cpu (the historically hardcoded-zero fields), host-total cumulative
//! network counters (the server derives rates from successive samples), and
//! the overlay carrier tallies + median peer RTT read from the runtime's
//! published [`OverlayView`] — the richest telemetry on the host, which
//! previously never left the LocalAPI socket.
//!
//! Sampling is cheap and synchronous; the 30 s heartbeat cadence satisfies
//! sysinfo's minimum CPU-measurement interval (the first tick reports 0%).

use roomler_ai_remote_control::signaling::{AgentSysStats, PeerLink};
use tunnel_core::localapi::{ConnectionType, OverlayView, PeerInfo};

/// How this node currently reaches a peer, as ONE label.
///
/// `ConnectionType` has no `Derp` variant — DERP is a relay flavour only
/// the carrier forensics block distinguishes — so the split lives here,
/// used by both the aggregate counters and the per-edge report.
fn carrier_label(p: &PeerInfo) -> &'static str {
    match p.connection {
        ConnectionType::Direct => "direct",
        ConnectionType::Relay => {
            let is_derp = p
                .debug
                .as_ref()
                .map(|d| d.relay_kind.as_deref() == Some("derp"))
                .unwrap_or(false);
            if is_derp { "derp" } else { "relay" }
        }
        ConnectionType::Tunnel => "tunnel",
        ConnectionType::Blocked => "blocked",
        ConnectionType::Offline => "offline",
    }
}

pub struct SysSampler {
    sys: sysinfo::System,
    networks: sysinfo::Networks,
    pid: Option<sysinfo::Pid>,
}

impl Default for SysSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl SysSampler {
    pub fn new() -> Self {
        Self {
            sys: sysinfo::System::new(),
            networks: sysinfo::Networks::new_with_refreshed_list(),
            pid: sysinfo::get_current_pid().ok(),
        }
    }

    pub fn sample(&mut self, view: &OverlayView) -> AgentSysStats {
        let (rss_mb, cpu_pct) = match self.pid {
            Some(pid) => {
                self.sys
                    .refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
                match self.sys.process(pid) {
                    Some(p) => (
                        (p.memory() / (1024 * 1024)).min(u64::from(u32::MAX)) as u32,
                        p.cpu_usage(),
                    ),
                    None => (0, 0.0),
                }
            }
            None => (0, 0.0),
        };

        self.networks.refresh(true);
        let mut net_rx_bytes = 0u64;
        let mut net_tx_bytes = 0u64;
        for (_name, data) in self.networks.iter() {
            net_rx_bytes = net_rx_bytes.saturating_add(data.total_received());
            net_tx_bytes = net_tx_bytes.saturating_add(data.total_transmitted());
        }

        let (mut direct, mut relay, mut derp) = (0u32, 0u32, 0u32);
        let mut rtts: Vec<u32> = Vec::new();
        // Wave 2: the same walk also emits the per-peer EDGES the org
        // dashboard graphs. Offline/blocked peers are reported too (as
        // their own carrier kinds) — a mesh drawing needs to show the
        // peer we cannot reach, which is exactly the interesting case.
        let mut links: Vec<PeerLink> = Vec::with_capacity(view.peers.len());
        for p in &view.peers {
            let carrier = carrier_label(p);
            links.push(PeerLink {
                node: p.node_id.clone(),
                carrier: carrier.to_string(),
                rtt_ms: p.rtt_ms,
                stalled: p.stalled,
            });
            if !p.online {
                continue;
            }
            match carrier {
                "direct" => direct += 1,
                "derp" => derp += 1,
                "relay" => relay += 1,
                _ => {}
            }
            if let Some(r) = p.rtt_ms {
                rtts.push(r);
            }
        }
        rtts.sort_unstable();
        let peer_rtt_ms = (!rtts.is_empty()).then(|| rtts[rtts.len() / 2]);

        AgentSysStats {
            rss_mb,
            cpu_pct,
            net_rx_bytes,
            net_tx_bytes,
            direct,
            relay,
            derp,
            peer_rtt_ms,
            links,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tunnel_core::localapi::{PeerCarrierDebug, PeerInfo};

    fn peer(online: bool, conn: ConnectionType, rtt: Option<u32>, kind: Option<&str>) -> PeerInfo {
        PeerInfo {
            node_id: "n".into(),
            name: "p".into(),
            overlay_ip: Some("100.64.0.9".into()),
            overlay_ip6: None,
            online,
            connection: conn,
            upgrading: false,
            stalled: false,
            rtt_ms: rtt,
            last_seen_ms: None,
            agent_id: None,
            relay_local: None,
            relay_dst: None,
            debug: kind.map(|k| PeerCarrierDebug {
                tier: "relay".into(),
                initiated: true,
                hs_done: true,
                local: None,
                dst: None,
                tx: 0,
                rx: 0,
                last_rx_age_s: 0,
                relay_kind: Some(k.into()),
                rx_denied: 0,
            }),
        }
    }

    #[test]
    fn carrier_tallies_and_median_rtt() {
        let mut s = SysSampler::new();
        let view = OverlayView {
            self_ip: Some("100.64.0.7".into()),
            self_ip6: None,
            peers: vec![
                peer(true, ConnectionType::Direct, Some(10), None),
                peer(true, ConnectionType::Relay, Some(50), Some("turn")),
                peer(true, ConnectionType::Relay, Some(90), Some("derp")),
                peer(true, ConnectionType::Relay, None, None), // no debug → TURN
                peer(false, ConnectionType::Direct, Some(999), None), // offline: ignored
            ],
            exit_node: None,
            dns: None,
            srflx: None,
        };
        let out = s.sample(&view);
        assert_eq!(out.direct, 1);
        assert_eq!(out.relay, 2);
        assert_eq!(out.derp, 1);
        assert_eq!(out.peer_rtt_ms, Some(50)); // median of [10, 50, 90]
        // Wave 2: the same walk emits one EDGE per peer — including the
        // offline one, which a mesh drawing must show rather than omit.
        assert_eq!(out.links.len(), 5);
        let carriers: Vec<&str> = out.links.iter().map(|l| l.carrier.as_str()).collect();
        assert_eq!(
            carriers,
            vec!["direct", "relay", "derp", "relay", "direct"],
            "carrier labels split DERP out of the Relay variant"
        );
        assert_eq!(out.links[0].rtt_ms, Some(10));
    }

    /// A peer we cannot reach is still an edge — with its own carrier
    /// label — but never counts as a live carrier.
    #[test]
    fn unreachable_peers_are_edges_not_carriers() {
        let mut s = SysSampler::new();
        let view = OverlayView {
            self_ip: Some("100.64.0.7".into()),
            self_ip6: None,
            peers: vec![
                peer(true, ConnectionType::Direct, Some(10), None),
                peer(false, ConnectionType::Offline, None, None),
                peer(false, ConnectionType::Blocked, None, None),
            ],
            exit_node: None,
            dns: None,
            srflx: None,
        };
        let out = s.sample(&view);
        assert_eq!(out.links.len(), 3);
        assert_eq!(out.links[1].carrier, "offline");
        assert_eq!(out.links[2].carrier, "blocked");
        assert_eq!(out.direct, 1);
        assert_eq!(out.relay + out.derp, 0);
    }
}
