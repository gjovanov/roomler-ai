//! Netmap → routable peer config.
//!
//! Converts a wire [`NetmapPeer`] (base64 pubkey + dotted overlay IP)
//! into a [`PeerConfig`] the [`WgDevice`](super::wg::WgDevice) can
//! install. Carrier selection (direct vs relay) is the node runtime's
//! job (Phase 3) — this module only decodes identity + address.

use std::net::Ipv4Addr;

use roomler_ai_remote_control::signaling::NetmapPeer;

use super::decode_public;
use super::router::Cidr;

/// A peer reduced to what the WG device needs: its static public key and
/// its single-host overlay address (`allowed_ips = overlay_ip/32`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerConfig {
    pub public_key: [u8; 32],
    pub overlay_ip: Ipv4Addr,
    /// Human-facing node name (Phase 0), for MagicDNS name→IP. Empty when the
    /// server predates Phase 0.
    pub name: String,
    /// Phase 1 — approved subnet routes this peer is a router for. Installed into
    /// the local [`Router`](super::router::Router) (allowed_ips) + OS route table
    /// so LAN behind the peer is reachable over the overlay. Empty for a normal
    /// peer.
    pub subnets: Vec<Cidr>,
    /// Dialable endpoints (host/srflx/relay), priority order — the
    /// runtime picks one to build the carrier.
    pub endpoints: Vec<String>,
    /// Phase A — the peer's join-time NIC-derived endpoints (server
    /// `lan_endpoints` bucket, no relay addresses mixed in). A public IP here
    /// is a direct-to-public dial candidate. Empty from a pre-Phase-A server.
    pub lan_endpoints: Vec<String>,
    /// Phase B — the peer's server-reflexive (srflx) endpoints (server
    /// `srflx_endpoints` bucket): its public NAT mapping learned via STUN. A
    /// public IP here is a srflx dial candidate (a 1:1/cone-NAT'd peer whose NIC
    /// is private). Empty from a pre-Phase-B server or a peer with no srflx.
    pub srflx_endpoints: Vec<String>,
    /// Phase C — the peer's probed NAT mapping type (`"cone"` / `"symmetric"`),
    /// or `None` when unknown. The runtime skips the srflx punch only when BOTH
    /// this peer and we are `"symmetric"`; any other combination attempts.
    pub srflx_nat: Option<String>,
    /// rc.142 — the peer advertised it can carry WG over QUIC-over-TURN. The
    /// runtime only attempts the QUIC relay carrier when this is set (both
    /// ends must agree, else the pair falls back to raw relay).
    pub supports_quic: bool,
    /// Phase D (v1 single-relay) — the peer advertised it can run the
    /// single-relay carrier: ONE anchor allocation + a raw dialer, instead of
    /// today's both-allocate hairpin. The runtime picks single-relay only when
    /// BOTH ends advertise this AND our `OVERLAY_RELAY_SINGLE` flag is on; else
    /// the pair uses both-allocate. Wired to the `NetmapPeer` field in D1c —
    /// `false` until then keeps a mixed fleet on the proven path.
    pub supports_relay_single: bool,
}

/// Decode a netmap peer. `None` if the pubkey isn't valid base64/length
/// or the overlay IP isn't a v4 address. Unreachable peers are dropped
/// by the server before they reach the netmap, but a defensive
/// `reachable == false` is also skipped here.
pub fn peer_config_from_netmap(peer: &NetmapPeer) -> Option<PeerConfig> {
    if !peer.reachable {
        return None;
    }
    let public_key = decode_public(&peer.wg_public_key)?;
    let overlay_ip: Ipv4Addr = peer.overlay_ip.parse().ok()?;
    Some(PeerConfig {
        public_key,
        overlay_ip,
        name: peer.name.clone(),
        subnets: peer.routes.iter().filter_map(|r| Cidr::parse(r)).collect(),
        endpoints: peer.endpoints.clone(),
        lan_endpoints: peer.lan_endpoints.clone(),
        srflx_endpoints: peer.srflx_endpoints.clone(),
        srflx_nat: peer.srflx_nat.clone(),
        supports_quic: peer.supports_quic,
        // D1c wires this to `peer.supports_relay_single` once the NetmapPeer
        // field + server population land; `false` until then keeps single-relay
        // inert (both ends must advertise, so a mixed fleet stays both-allocate).
        supports_relay_single: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::WgKeypair;
    use bson::oid::ObjectId;

    fn netmap_peer(pubkey_b64: &str, ip: &str, reachable: bool) -> NetmapPeer {
        NetmapPeer {
            node_id: ObjectId::new(),
            overlay_ip: ip.into(),
            name: String::new(),
            wg_public_key: pubkey_b64.into(),
            endpoints: vec!["203.0.113.5:51820".into()],
            lan_endpoints: vec![],
            srflx_endpoints: vec![],
            srflx_nat: None,
            relay_home: None,
            reachable,
            supports_quic: false,
            routes: vec![],
            agent_id: None,
        }
    }

    #[test]
    fn decodes_valid_peer() {
        let kp = WgKeypair::generate();
        let p = netmap_peer(&kp.public_base64(), "100.64.0.5", true);
        let cfg = peer_config_from_netmap(&p).unwrap();
        assert_eq!(cfg.public_key, kp.public.to_bytes());
        assert_eq!(cfg.overlay_ip, Ipv4Addr::new(100, 64, 0, 5));
        assert_eq!(cfg.endpoints.len(), 1);
    }

    #[test]
    fn rejects_unreachable_or_malformed() {
        let kp = WgKeypair::generate();
        assert!(
            peer_config_from_netmap(&netmap_peer(&kp.public_base64(), "100.64.0.5", false))
                .is_none()
        );
        assert!(peer_config_from_netmap(&netmap_peer("bad!!", "100.64.0.5", true)).is_none());
        assert!(
            peer_config_from_netmap(&netmap_peer(&kp.public_base64(), "not-an-ip", true)).is_none()
        );
    }
}
