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

/// Lowest agent release on the `0.3.0` rc line that handles the full
/// QUIC-over-TURN tunnel setup (`TunnelQuicSetup` carrying relay
/// `ice_servers`), shipped as `agent-v0.3.0-rc.104`. An agent older
/// than this silently ignores `TunnelQuicSetup`, so the server must NOT
/// negotiate `quic-v1` for it — the client would otherwise sit through
/// its full `QUIC_READY_TIMEOUT` (30 s) before falling back to
/// `webrtc-dc-v1`. See [`agent_supports_quic`].
pub const MIN_QUIC_AGENT_RC: u32 = 104;

/// Whether an agent reporting `version` (its `CARGO_PKG_VERSION`, e.g.
/// `"0.3.0-rc.104"`) supports the QUIC tunnel data plane — i.e. whether
/// the server may safely negotiate `quic-v1` for it rather than risking
/// a 30 s setup timeout against an agent that can't answer.
///
/// Conservative by design: an unrecognised string returns `false`, so
/// negotiation falls back to the proven `webrtc-dc-v1` instead of
/// gambling on a transport the agent may not speak.
///
/// * `0.3.0-rc.N` → `N >= MIN_QUIC_AGENT_RC`
/// * `0.3.0` final, or any version ordering after it (`0.3.1`, `0.4.x`,
///   `1.x`) → `true` (QUIC shipped well before `0.3.0` final)
/// * anything before `0.3.0` (`0.1.x` / `0.2.x`), or an unparseable
///   string → `false`
///
/// Reliable in practice because `agent_version` is refreshed on every
/// agent WS connect (`update_hello`) and a tunnel can only open to an
/// *online* agent — so the version checked here is always current.
pub fn agent_supports_quic(version: &str) -> bool {
    // Peel off the `-rc.<n>` pre-release tail, if present.
    let (core, rc) = match version.split_once("-rc.") {
        Some((core, n)) => (core, Some(n)),
        None => (version, None),
    };
    // Keep only the `MAJOR.MINOR.PATCH` triple — defensively strip any
    // other `-pre` / `+build` tail (we only ever ship `-rc.`).
    let core = core.split(['-', '+']).next().unwrap_or(core).trim();
    let mut parts = core.split('.').map(|p| p.trim().parse::<u32>().ok());
    let triple = match (parts.next(), parts.next(), parts.next()) {
        (Some(Some(maj)), Some(Some(min)), Some(Some(pat))) => (maj, min, pat),
        _ => return false, // unparseable core → conservative
    };
    use std::cmp::Ordering::{Equal, Greater, Less};
    match triple.cmp(&(0, 3, 0)) {
        Greater => true, // 0.3.1+, 0.4.x, 1.x — after QUIC shipped
        Less => false,   // 0.1.x / 0.2.x — before QUIC
        Equal => match rc {
            None => true, // 0.3.0 final sorts after every 0.3.0-rc.*
            Some(n) => n
                .trim()
                .parse::<u32>()
                .is_ok_and(|n| n >= MIN_QUIC_AGENT_RC),
        },
    }
}

/// Lowest agent release that runs the overlay WireGuard L3 data plane
/// (`wireguard-v1` + the `rc:overlay.*` netmap). An agent older than
/// this ignores overlay setup, so the server must NOT treat it as an
/// overlay node. Bumped to the first rc that ships the `overlay`
/// feature; placeholder until that tag is cut.
pub const MIN_OVERLAY_AGENT_RC: u32 = 130;

/// Whether an agent reporting `version` supports the overlay L3 data
/// plane — i.e. whether the server may enroll it as an overlay node and
/// negotiate `wireguard-v1` for it. Same parse-and-compare shape +
/// conservative-on-unparseable contract as [`agent_supports_quic`].
///
/// * `0.3.0-rc.N` → `N >= MIN_OVERLAY_AGENT_RC`
/// * `0.3.0` final / any later line (`0.3.1`, `0.4.x`, `1.x`) → `true`
/// * anything earlier, or an unparseable string → `false`
pub fn agent_supports_overlay(version: &str) -> bool {
    let (core, rc) = match version.split_once("-rc.") {
        Some((core, n)) => (core, Some(n)),
        None => (version, None),
    };
    let core = core.split(['-', '+']).next().unwrap_or(core).trim();
    let mut parts = core.split('.').map(|p| p.trim().parse::<u32>().ok());
    let triple = match (parts.next(), parts.next(), parts.next()) {
        (Some(Some(maj)), Some(Some(min)), Some(Some(pat))) => (maj, min, pat),
        _ => return false,
    };
    use std::cmp::Ordering::{Equal, Greater, Less};
    match triple.cmp(&(0, 3, 0)) {
        Greater => true,
        Less => false,
        Equal => match rc {
            None => true,
            Some(n) => n
                .trim()
                .parse::<u32>()
                .is_ok_and(|n| n >= MIN_OVERLAY_AGENT_RC),
        },
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_quic_gate_rc_threshold() {
        // The 0.3.0-rc line is gated exactly at MIN_QUIC_AGENT_RC.
        assert!(!agent_supports_quic("0.3.0-rc.35"));
        assert!(!agent_supports_quic("0.3.0-rc.103"));
        assert!(agent_supports_quic("0.3.0-rc.104"));
        assert!(agent_supports_quic("0.3.0-rc.116"));
        assert!(agent_supports_quic("0.3.0-rc.999"));
    }

    #[test]
    fn agent_quic_gate_release_lines() {
        // Pre-0.3.0 never spoke QUIC — even a high rc on an older line.
        assert!(!agent_supports_quic("0.1.0"));
        assert!(!agent_supports_quic("0.2.7"));
        assert!(!agent_supports_quic("0.2.99-rc.500"));
        // 0.3.0 final + every later line is QUIC-capable.
        assert!(agent_supports_quic("0.3.0"));
        assert!(agent_supports_quic("0.3.1"));
        assert!(agent_supports_quic("0.4.0"));
        assert!(agent_supports_quic("0.4.0-rc.1"));
        assert!(agent_supports_quic("1.0.0"));
    }

    #[test]
    fn agent_quic_gate_unparseable_is_conservative() {
        assert!(!agent_supports_quic(""));
        assert!(!agent_supports_quic("garbage"));
        assert!(!agent_supports_quic("0.3")); // not a full triple
        assert!(!agent_supports_quic("0.3.x"));
        assert!(!agent_supports_quic("0.3.0-rc.notanumber"));
    }

    #[test]
    fn agent_overlay_gate_rc_threshold() {
        assert!(!agent_supports_overlay(&format!(
            "0.3.0-rc.{}",
            MIN_OVERLAY_AGENT_RC - 1
        )));
        assert!(agent_supports_overlay(&format!(
            "0.3.0-rc.{MIN_OVERLAY_AGENT_RC}"
        )));
        assert!(agent_supports_overlay(&format!(
            "0.3.0-rc.{}",
            MIN_OVERLAY_AGENT_RC + 10
        )));
    }

    #[test]
    fn agent_overlay_gate_release_lines_and_unparseable() {
        // Earlier lines never speak overlay, even at a high rc.
        assert!(!agent_supports_overlay("0.2.99-rc.999"));
        assert!(!agent_supports_overlay("0.3.0-rc.1"));
        // 0.3.0 final + every later line is overlay-capable.
        assert!(agent_supports_overlay("0.3.0"));
        assert!(agent_supports_overlay("0.4.0"));
        assert!(agent_supports_overlay("1.0.0"));
        // Conservative on garbage.
        assert!(!agent_supports_overlay(""));
        assert!(!agent_supports_overlay("0.3.x"));
        assert!(!agent_supports_overlay("0.3.0-rc.notanumber"));
    }
}
