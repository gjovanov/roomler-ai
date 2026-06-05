//! `wireguard-v1` transport — the L3 overlay data plane.
//!
//! This file carries the always-compiled [`Transport`] descriptor so
//! capability negotiation (`supported_transports`) works in every build,
//! overlay feature on or off. The actual userspace WireGuard device
//! (boringtun + carriers + routing) lives in [`crate::overlay`], behind
//! the `overlay` feature — a node that advertises `wireguard-v1` is one
//! that was built with that feature and runs an [`crate::overlay::wg::WgDevice`].

use crate::transport::{Capabilities, TRANSPORT_WIREGUARD_V1, Transport};

/// The `wireguard-v1` transport descriptor: full L3, UDP-capable,
/// multi-stream (one WG link carries every overlay flow). The data
/// plane it advertises is driven by [`crate::overlay`].
#[derive(Debug, Default, Clone, Copy)]
pub struct WireGuardTransport;

impl Transport for WireGuardTransport {
    fn label(&self) -> &'static str {
        TRANSPORT_WIREGUARD_V1
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            multi_stream: true,
            supports_udp: true,
            l3: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_full_l3() {
        let t = WireGuardTransport;
        assert_eq!(t.label(), TRANSPORT_WIREGUARD_V1);
        let caps = t.capabilities();
        assert!(caps.l3, "wireguard-v1 is a full L3 transport");
        assert!(caps.supports_udp);
        assert!(caps.multi_stream);
    }
}
