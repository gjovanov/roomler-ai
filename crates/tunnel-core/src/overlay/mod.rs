// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Overlay L3 data plane — userspace WireGuard mesh (Phase 2+).
//!
//! Feature-gated behind `overlay`. Pulls in `boringtun` for the Noise
//! crypto state machine; everything else (carrier selection, routing,
//! netmap application) is Roomler code.
//!
//! Layout:
//! * [`wg`] — the [`wg::WgDevice`]: a static keypair + a per-peer
//!   `boringtun::noise::Tunn`, each bridged to a [`wg::Carrier`] (a
//!   direct UDP socket or a coturn [`RelayConn`](crate::transport::relay::RelayConn)).
//! * [`router`] — the `overlay_ip → wg_public_key` crypto-routing table
//!   (boringtun's `Tunn` is single-peer, so the `allowed_ips` map lives
//!   here).
//! * [`netmap`] — decode a signed `rc:overlay.netmap` peer into a
//!   routable [`netmap::PeerConfig`].
//! * [`tun`] — the [`tun::TunIo`] OS-NIC seam (`SystemTun` behind
//!   `overlay-l3`; an in-memory mock in tests).
//! * [`bridge`] — the TUN↔`WgDevice` packet pump ([`bridge::run_bridge`]).
//! * [`runtime`] — the node runtime ([`runtime::OverlayRuntime`]): join →
//!   netmap → install WG peers + bring up the TUN + pump packets.
//!
//! Identity: each node owns a stable Curve25519 keypair. The private
//! key never leaves the node; the base64 public key is registered with
//! the coordination server and distributed in the netmap.

pub mod bridge;
/// Multi-org v2 — the process-wide shared direct-carrier plane: one stable
/// socket set for EVERY org engine, inbound demultiplexed by WireGuard
/// receiver index (initiations by per-engine static-key trial).
pub mod carrier_plane;
pub mod dialer;
pub mod direct;
pub mod disco;
pub mod dns;
/// #1363 — the OS path, measured: one lookup a minute of a name only an
/// overlay resolver answers, shared by FR-72 P6's ladder and the status.
pub mod dns_probe;
/// FR-97 P0b-2b — host state the daemon changes that must not outlive it
/// (IPv4 forwarding a Windows router turned on), refcounted in process and
/// persisted for the boot purge.
pub mod host_ledger;
/// FR-97 P3i — what the host's own firewall (firewalld, ufw) lets a mesh peer
/// reach on the overlay adapter; reported, never changed.
pub mod hostfw;
pub mod hosts;
pub mod ingress;
/// A LAN interface's own prefix route made unusable underneath it (a next
/// hop that is not on its LAN, a VPN's stale gateway): detected and repaired
/// with the prefix's two on-link halves. Never routes around a capture.
pub mod lan_repair;
pub(crate) mod lifecycle;
pub mod nat;
pub mod netcheck;
pub mod netmap;
/// Userspace TCP/IP stack (smoltcp) presented as a [`tun::TunIo`] — the
/// OS-free twin of [`tun::SystemTun`]. Feature `overlay-netstack`.
#[cfg(feature = "overlay-netstack")]
pub mod netstack;
/// SOCKS5-CONNECT front for the [`netstack`] — the app-facing half of the
/// OS-free path. Feature `overlay-netstack`.
#[cfg(feature = "overlay-netstack")]
pub mod netstack_socks;
/// netstate — the process-wide network-state monitor (ONE OS change
/// subscription, typed snapshots/deltas, non-blocking fan-out). Public: the
/// agent's signaling loop subscribes for its probe-then-cycle arm.
pub mod netstate;
/// FR-19 P1 — org-relay wire framing (Geneve `Opt Len 0`, pinned protocol,
/// 24-bit VNI). Framing and shape rules only: nothing here forwards, binds or
/// holds session state.
pub mod orgrelay;
pub(crate) mod path;
/// FR-97 P1b — a Mac serves as an exit or subnet router through pf.
pub(crate) mod pf;
pub mod relay_link;
pub mod router;
pub mod runtime;
/// FR-110 — the buffers a carrier socket asks for. At the OS default the
/// kernel dropped bursts before the daemon read them, and TCP inside the tunnel
/// read every drop as congestion.
pub(crate) mod sockbuf;
/// Port-intercept shim: terminates one TCP port of this node's overlay address
/// in the in-process [`netstack`] instead of handing it to the OS, so an
/// in-daemon service can own e.g. `:22` on a host where `sshd` already holds
/// `0.0.0.0:22` — and without binding a socket a firewall or EDR agent could
/// block. Feature `overlay-netstack`.
#[cfg(feature = "overlay-netstack")]
pub mod split_tun;
pub mod tun;
/// #2213 / #2220 — reading a UDP socket past the errors Windows reports about
/// an EARLIER datagram (an ICMP port-unreachable, as 10054), for every loop
/// that serves a real OS socket.
pub(crate) mod udp_recv;
/// C4 — the warm TURN allocation's loop-side state and pure helpers
/// (`docs/overlay-warm-relay.md`).
pub(crate) mod warm_relay;
pub mod wg;

/// WFP firewall override (Windows + `overlay-l3`): hard-permit the
/// `roomler` adapter so the overlay survives a GPO-locked Defender Firewall.
#[cfg(all(feature = "overlay-l3", windows))]
pub mod wfp;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use boringtun::x25519::{PublicKey, StaticSecret};

/// #4380 — the target of the route guard's two INFO lines: the off switch's
/// (`ROOMLERD_OVERLAY_SUBNET_GUARD=0`, once per process) and the one that names
/// another route holding a subnet's prefix (once per hold). The daemon's log
/// filter appends `<this>=info` under any `RUST_LOG`
/// (`roomler_node_core::logging::filter_for`), as it does for FR-110's
/// [`sockbuf`]: the packaged Linux units set `RUST_LOG=roomlerd=info,warn`,
/// which drops every other `tunnel_core` INFO, and dropped these two as well.
///
/// No module has this path, so no other line lands on it by default; a module
/// placed at this path would put all of its INFO lines in the packaged log.
/// Only INFO lines use it, because the appended `=info` caps a DEBUG line on
/// it under every spec, `RUST_LOG=debug` included.
pub(crate) const ROUTE_GUARD_LOG: &str = "tunnel_core::overlay::route_guard";

/// A node's WireGuard static identity. The secret stays in the node's
/// secure storage; only [`WgKeypair::public_base64`] is published.
#[derive(Clone)]
pub struct WgKeypair {
    pub secret: StaticSecret,
    pub public: PublicKey,
}

impl WgKeypair {
    /// Generate a fresh Curve25519 keypair from the OS CSPRNG.
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Reconstruct from a base64-encoded 32-byte secret (e.g. read back
    /// from config on restart).
    pub fn from_secret_base64(s: &str) -> Option<Self> {
        let raw = B64.decode(s.trim()).ok()?;
        let bytes: [u8; 32] = raw.try_into().ok()?;
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret);
        Some(Self { secret, public })
    }

    /// base64 of the 32-byte secret — for persisting to secure storage.
    pub fn secret_base64(&self) -> String {
        B64.encode(self.secret.to_bytes())
    }

    /// base64 of the 32-byte public key — what the netmap distributes.
    pub fn public_base64(&self) -> String {
        encode_public(&self.public)
    }
}

/// base64-encode a WireGuard public key (the netmap wire form).
pub fn encode_public(public: &PublicKey) -> String {
    B64.encode(public.to_bytes())
}

/// Decode a base64 WireGuard public key into raw bytes. `None` on bad
/// base64 or wrong length.
pub fn decode_public(s: &str) -> Option<[u8; 32]> {
    let raw = B64.decode(s.trim()).ok()?;
    raw.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_roundtrips_through_base64() {
        let kp = WgKeypair::generate();
        let restored = WgKeypair::from_secret_base64(&kp.secret_base64()).unwrap();
        assert_eq!(kp.public.to_bytes(), restored.public.to_bytes());
        assert_eq!(kp.public_base64(), restored.public_base64());
    }

    #[test]
    fn public_key_codec_roundtrips() {
        let kp = WgKeypair::generate();
        let b64 = kp.public_base64();
        assert_eq!(decode_public(&b64), Some(kp.public.to_bytes()));
        assert!(decode_public("not-base64!!").is_none());
        assert!(decode_public("c2hvcnQ=").is_none()); // valid b64, wrong len
    }
}

/// #4380 — what a cell's own thread says while a [`said::Lines`] guard lives,
/// each event with its target and level. The daemon's log filter keeps or
/// drops a line by its target, so a cell reads the target a line is said on.
#[cfg(test)]
pub(crate) mod said {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt as _;

    /// One event: its target, its level, and its message with its fields.
    #[derive(Debug, Clone)]
    pub(crate) struct Said {
        pub target: String,
        pub level: tracing::Level,
        pub text: String,
    }

    /// Every event this thread says while the guard from [`Lines::start`]
    /// lives, at every level. Thread-scoped, so a parallel cell's events
    /// never land in it.
    #[derive(Clone, Default)]
    pub(crate) struct Lines(Arc<Mutex<Vec<Said>>>);

    impl Lines {
        pub(crate) fn start() -> (Self, tracing::subscriber::DefaultGuard) {
            let lines = Self::default();
            let subscriber = tracing_subscriber::registry().with(Record(lines.clone()));
            (lines, tracing::subscriber::set_default(subscriber))
        }

        /// The events whose text carries `words`.
        pub(crate) fn with(&self, words: &str) -> Vec<Said> {
            let Lines(all) = self;
            all.lock()
                .unwrap()
                .iter()
                .filter(|s| s.text.contains(words))
                .cloned()
                .collect()
        }
    }

    struct Record(Lines);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Record {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut text = Text(String::new());
            event.record(&mut text);
            let meta = event.metadata();
            let Record(Lines(all)) = self;
            all.lock().unwrap().push(Said {
                target: meta.target().to_string(),
                level: *meta.level(),
                text: text.0,
            });
        }
    }

    /// An event's message, then each field as `name=value`.
    struct Text(String);

    impl tracing::field::Visit for Text {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            if field.name() == "message" {
                let _ = write!(self.0, "{value:?}");
            } else {
                let _ = write!(self.0, "{}={value:?}", field.name());
            }
        }
    }
}
