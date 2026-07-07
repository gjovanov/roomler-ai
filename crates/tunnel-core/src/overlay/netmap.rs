//! Netmap → routable peer config.
//!
//! Converts a wire [`NetmapPeer`] (base64 pubkey + dotted overlay IP)
//! into a [`PeerConfig`] the [`WgDevice`](super::wg::WgDevice) can
//! install. Carrier selection (direct vs relay) is the node runtime's
//! job (Phase 3) — this module only decodes identity + address.

use std::net::Ipv4Addr;

use roomler_ai_remote_control::signaling::NetmapPeer;

use super::decode_public;

/// A peer reduced to what the WG device needs: its static public key and
/// its single-host overlay address (`allowed_ips = overlay_ip/32`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerConfig {
    pub public_key: [u8; 32],
    pub overlay_ip: Ipv4Addr,
    /// Human-facing node name (Phase 0), for MagicDNS name→IP. Empty when the
    /// server predates Phase 0.
    pub name: String,
    /// Dialable endpoints (host/srflx/relay), priority order — the
    /// runtime picks one to build the carrier.
    pub endpoints: Vec<String>,
    /// rc.142 — the peer advertised it can carry WG over QUIC-over-TURN. The
    /// runtime only attempts the QUIC relay carrier when this is set (both
    /// ends must agree, else the pair falls back to raw relay).
    pub supports_quic: bool,
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
        endpoints: peer.endpoints.clone(),
        supports_quic: peer.supports_quic,
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
            relay_home: None,
            reachable,
            supports_quic: false,
            routes: vec![],
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
