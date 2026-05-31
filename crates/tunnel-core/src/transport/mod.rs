//! Pluggable data-plane transport.
//!
//! v1 ships [`webrtc_dc::TunnelPeer`] (SCTP DataChannels over the
//! existing Roomler WebRTC plane). v0.5 will add `WireGuardTransport`
//! (userspace WG via boringtun + optional TUN). The [`Transport`]
//! trait is the contract both implementations satisfy; capability
//! negotiation in `rc:tunnel.hello` picks the strongest mutually-
//! supported one at peer setup time.
//!
//! See plan §3 (workspace layout) and §4 (wire protocol).

use async_trait::async_trait;

pub mod quic;
pub mod relay;
pub mod stun;
pub mod webrtc_dc;
pub mod wireguard;

/// Identifies a data-plane implementation in the `supported_transports`
/// list exchanged in `rc:tunnel.hello`. Strings (not an enum) so a
/// forward-rolled client and an older agent can still find a common
/// transport.
pub const TRANSPORT_WEBRTC_DC_V1: &str = "webrtc-dc-v1";
pub const TRANSPORT_WIREGUARD_V1: &str = "wireguard-v1";
/// Opportunistic QUIC P2P transport (quinn). Tried before
/// `webrtc-dc-v1` when both peers advertise it; falls back to WebRTC if
/// QUIC connection setup fails. See [`quic`].
pub const TRANSPORT_QUIC_V1: &str = "quic-v1";

/// Capabilities a transport advertises.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// One transport can carry many concurrent TCP forwards.
    pub multi_stream: bool,
    /// True for WireGuard, false for v1 DC transport (UDP DCs in v2).
    pub supports_udp: bool,
    /// True for WireGuard (full L3); false for v1 (per-port forwarding).
    pub l3: bool,
}

/// The contract every tunnel transport implements.
#[async_trait]
pub trait Transport: Send + Sync {
    fn label(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
}
