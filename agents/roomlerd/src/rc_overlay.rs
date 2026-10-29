// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #1882 — may a remote-desktop session use the WireGuard overlay, and the
//! filters that keep it off when it may not.
//!
//! The DEVICE decides: `AccessPolicy.rc_overlay`, default off, delivered per
//! session in `rc:request` (an older server never sends it, which reads off).
//! Off means no ICE pair of the session touches an overlay address. That
//! takes three filters, because the overlay reaches a session's ICE three
//! ways, and the interface-name rule RC always had covers only part of the
//! first:
//!
//! | how the overlay gets in | filter |
//! |---|---|
//! | a HOST candidate on the overlay interface — on macOS a kernel-named `utunN` the name rule cannot see | [`RcOverlay::keeps_local`], the `SettingEngine` IP filter |
//! | a REMOTE candidate on a mesh address, when the controller's browser runs on an overlay node; the agent's srflx and relay sockets are bound to `0.0.0.0`, so a check to it routes into the TUN no matter which candidates the agent offered | [`RcOverlay::keeps_remote_candidate`] on every trickled candidate, [`RcOverlay::strip_sdp`] on the offer |
//! | an ICE SERVER on a mesh address — the loopback-TURN relay the Hub appends for a corp controller | [`RcOverlay::ice_servers`] (the Hub also withholds it) |
//!
//! "Overlay address" is exact: the blocks this daemon's overlay routes
//! ([`tunnel_core::overlay_footprint`]) and the derived-v6 ULA — never the
//! whole CGNAT `/10`, which LTE modems, cloud VPCs and other meshes use too.
//!
//! On, nothing is filtered beyond the name rule: the behaviour from before
//! the switch existed.

use std::net::{IpAddr, Ipv4Addr};

use roomler_ai_remote_control::signaling::{IceServer, RcOverlayAutoWire};
use serde::{Deserialize, Serialize};
use tunnel_core::overlay_footprint as footprint;

/// The overlay decision for ONE session, resolved when it is requested.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct RcOverlay {
    /// The device's `AccessPolicy.rc_overlay`. `false`, the default, keeps the
    /// session off the overlay.
    pub allow: bool,
    /// FR-128 — the device's policy is `auto`. The session decides at its
    /// offer, in the process that builds the peer ([`Self::decide`]), and
    /// `allow` then holds the outcome; until then it is `false`. Absent, from
    /// an older daemon on the delegation channel, reads `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    /// FR-128 — the viewer's own device on the overlay, as the server named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer: Option<IpAddr>,
    /// FR-128 — whether both ends' last netcheck reached the far TURN over
    /// UDP. `None` = not measured, read as "it works".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub far_turn: Option<bool>,
    /// The overlay's v4 blocks, snapshotted by the DAEMON when the session is
    /// requested. Carried rather than re-read so a delegated session's GUI
    /// worker, which runs no overlay of its own, filters the same addresses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub v4_nets: Vec<(Ipv4Addr, u8)>,
}

/// FR-128 — `ROOMLERD_RC_OVERLAY_AUTO=0` makes `auto` behave as `off`.
pub fn auto_enabled() -> bool {
    tunnel_core::env::flag("RC_OVERLAY_AUTO", true)
}

impl RcOverlay {
    /// The decision for a session THIS process resolves: the device's policy,
    /// plus this process's overlay blocks when they are needed.
    pub fn resolve(allow: bool) -> Self {
        Self::resolve_mode(allow, None)
    }

    /// [`Self::resolve`] with FR-128's third value, `auto` (the server's
    /// hints present). It stands only while `allow` is off and
    /// [`auto_enabled`] holds; it starts as off, and the offer decides.
    pub fn resolve_mode(allow: bool, auto: Option<&RcOverlayAutoWire>) -> Self {
        let auto = auto.filter(|_| !allow && auto_enabled());
        Self {
            allow,
            auto: auto.is_some(),
            viewer: auto
                .and_then(|a| a.viewer.as_deref())
                .and_then(|v| v.parse().ok()),
            far_turn: auto.and_then(|a| a.far_turn),
            v4_nets: if allow {
                Vec::new()
            } else {
                footprint::v4_nets()
            },
        }
    }

    /// FR-128 — make an `auto` session's decision, at its offer: the live
    /// carrier to the viewer's device ([`mesh_carrier_of`]) and the server's
    /// far-TURN hint, read by [`auto_allows`]. Anything that is not `auto`
    /// comes back as it went in. One line in the log names the inputs.
    pub async fn decide(self, session: impl std::fmt::Display) -> Self {
        self.decide_with(session, mesh_carrier_of).await
    }

    /// [`Self::decide`] with the carrier lookup passed in, for tests.
    pub async fn decide_with<F, Fut>(self, session: impl std::fmt::Display, lookup: F) -> Self
    where
        F: FnOnce(IpAddr) -> Fut,
        Fut: std::future::Future<Output = MeshCarrier>,
    {
        if !self.auto {
            return self;
        }
        let carrier = match self.viewer {
            Some(viewer) => lookup(viewer).await,
            None => MeshCarrier::Unknown,
        };
        let allow = auto_allows(carrier, self.far_turn.unwrap_or(true));
        tracing::info!(
            %session,
            viewer = ?self.viewer,
            ?carrier,
            far_turn = ?self.far_turn,
            allow,
            "rc ICE: overlay auto (FR-128) — {}",
            if allow {
                "the session may ride the mesh"
            } else {
                "the session stays off the mesh"
            }
        );
        Self {
            allow,
            v4_nets: if allow { Vec::new() } else { self.v4_nets },
            ..self
        }
    }

    fn blocks(&self, ip: IpAddr) -> bool {
        !self.allow && footprint::is_overlay_addr(&self.v4_nets, ip)
    }

    /// The `SettingEngine` IP filter: may ICE gather a host candidate on `ip`?
    pub fn keeps_local(&self, ip: IpAddr) -> bool {
        !self.blocks(ip)
    }

    /// May this remote candidate (`candidate:… <addr> <port> typ …`) be added?
    /// A candidate whose address is not an IP literal — an mDNS `.local` name
    /// — passes, and the caller asks again once it has resolved it.
    pub fn keeps_remote_candidate(&self, candidate: &str) -> bool {
        candidate_ip(candidate).is_none_or(|ip| !self.blocks(ip))
    }

    /// The session's ICE servers minus every URL whose host is an overlay
    /// address, and minus any server left with no URL.
    pub fn ice_servers(&self, servers: &[IceServer]) -> Vec<IceServer> {
        if self.allow {
            return servers.to_vec();
        }
        servers
            .iter()
            .filter_map(|s| {
                let urls: Vec<String> = s
                    .urls
                    .iter()
                    .filter(|u| url_host_ip(u).is_none_or(|ip| !self.blocks(ip)))
                    .cloned()
                    .collect();
                (!urls.is_empty()).then(|| IceServer {
                    urls,
                    username: s.username.clone(),
                    credential: s.credential.clone(),
                })
            })
            .collect()
    }

    /// `sdp` without its overlay-addressed `a=candidate:` lines. A browser
    /// trickles its candidates, so an offer normally carries none; one that
    /// gathered first carries them all, and they would be added with no
    /// trickle to filter.
    pub fn strip_sdp(&self, sdp: &str) -> String {
        if self.allow {
            return sdp.to_string();
        }
        sdp.split_inclusive('\n')
            .filter(|line| {
                let l = line.trim_start();
                !l.starts_with("a=candidate:") || self.keeps_remote_candidate(l)
            })
            .collect()
    }

    /// The decision in words, for the one line a session logs, so "why did
    /// it not take the overlay pair" has an answer in the device's log.
    pub fn describe(&self) -> &'static str {
        match (self.allow, self.auto) {
            (true, true) => "allowed (device policy rc_overlay=auto, decided for this session)",
            (false, true) => "excluded (device policy rc_overlay=auto, decided for this session)",
            (true, false) => "allowed (device policy rc_overlay=on)",
            (false, false) => "excluded (device policy rc_overlay=off, the default)",
        }
    }
}

/// FR-128 — what carries the mesh link to the viewer's device, as `auto`
/// reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshCarrier {
    /// LAN, public or hole-punched: the overlay pair is a direct path.
    Direct,
    /// DERP, a peer relay or a tunnel: the overlay would carry the video over
    /// somebody else's TCP.
    Relayed,
    /// No answer: an older daemon, a peer that is unknown, blocked or offline,
    /// or LocalAPI slower than 400 ms.
    Unknown,
}

impl MeshCarrier {
    /// A LocalAPI `peers` answer, read for `auto`. `None`: the peer is not
    /// in the answer.
    pub fn of(connection: Option<tunnel_core::localapi::ConnectionType>) -> Self {
        use tunnel_core::localapi::ConnectionType as C;
        match connection {
            Some(C::Direct) => Self::Direct,
            Some(C::Relay | C::Tunnel) => Self::Relayed,
            Some(C::Blocked | C::Offline) | None => Self::Unknown,
        }
    }
}

/// FR-128 §3.1 — may an `auto` session use the overlay?
pub fn auto_allows(carrier: MeshCarrier, far_turn: bool) -> bool {
    match carrier {
        // The case a Mac's Chrome loses today, where macOS denies its network
        // helper the LAN: take the mesh, unconstrained.
        MeshCarrier::Direct => true,
        // A relayed mesh would put the video on TCP (§2.3), so take the far
        // TURN over UDP while there is one. With none, the overlay is the
        // only path (§2.4), and the relay-tier guard clamps the session.
        MeshCarrier::Relayed => !far_turn,
        // Fail safe.
        MeshCarrier::Unknown => false,
    }
}

/// FR-128 — the mesh carrier to the peer that owns `addr`: LocalAPI `peers`
/// (the `roomler peers` verb) within 400 ms, the bound the relay-tier guard
/// uses. Ungated, unlike that guard, because any session can be `auto`.
pub async fn mesh_carrier_of(addr: IpAddr) -> MeshCarrier {
    let addr = addr.to_string();
    let query = async {
        let mut client = tunnel_core::localapi::connect().await.ok()?;
        let peers = client.peers().await.ok()?;
        Some(
            peers
                .into_iter()
                .find(|p| {
                    p.overlay_ip.as_deref() == Some(addr.as_str())
                        || p.overlay_ip6.as_deref() == Some(addr.as_str())
                })
                .map(|p| p.connection),
        )
    };
    match tokio::time::timeout(std::time::Duration::from_millis(400), query).await {
        Ok(Some(connection)) => MeshCarrier::of(connection),
        _ => MeshCarrier::Unknown,
    }
}

/// The connection address of an ICE candidate line, if it is an IP literal.
/// The address is the fifth token in both shapes — a trickled
/// `candidate:<foundation> <component> <transport> <priority> <addr> …` and
/// an SDP `a=candidate:…` line.
pub fn candidate_ip(candidate: &str) -> Option<IpAddr> {
    candidate.split_whitespace().nth(4)?.parse().ok()
}

/// The host of a `stun:`/`turn:`/`turns:` URL, if it is an IP literal:
/// `turn:100.64.0.9:47989`, `turns:[fd72:6f6f:6d6c::1]:443?transport=tcp`.
fn url_host_ip(url: &str) -> Option<IpAddr> {
    let (_, rest) = url.split_once(':')?;
    let hostport = rest.split('?').next()?.trim_start_matches("//");
    let host = if let Some(v6) = hostport.strip_prefix('[') {
        v6.split(']').next()?
    } else {
        hostport.rsplit_once(':').map_or(hostport, |(h, _)| h)
    };
    host.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn off() -> RcOverlay {
        RcOverlay {
            allow: false,
            v4_nets: vec![(Ipv4Addr::new(100, 65, 4, 0), 22)],
            ..Default::default()
        }
    }

    fn on() -> RcOverlay {
        RcOverlay {
            allow: true,
            v4_nets: vec![(Ipv4Addr::new(100, 65, 4, 0), 22)],
            ..Default::default()
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The macOS case #1882 was filed for: the `utunN` carries the overlay v4
    /// and the derived v6, and neither may become a host candidate. A LAN
    /// address, a CGNAT address OUTSIDE our blocks (an LTE modem) and a home
    /// router's ULA stay.
    #[test]
    fn local_filter_drops_only_overlay_addresses() {
        let p = off();
        assert!(!p.keeps_local(ip("100.65.4.34")));
        assert!(!p.keeps_local(ip("fd72:6f6f:6d6c::6441:422")));
        assert!(p.keeps_local(ip("192.168.1.20")));
        assert!(p.keeps_local(ip("100.80.1.2")));
        assert!(p.keeps_local(ip("fd00::1")));
        assert!(on().keeps_local(ip("100.65.4.34")), "on = no filter");
    }

    #[test]
    fn remote_candidates_on_the_mesh_are_dropped_and_names_pass() {
        let p = off();
        let host = "candidate:1 1 udp 2122260223 100.65.4.33 54321 typ host generation 0";
        let relay =
            "candidate:2 1 udp 41885439 100.65.4.33 61000 typ relay raddr 127.0.0.1 rport 5000";
        let lan = "candidate:3 1 udp 2122260223 192.168.1.30 54322 typ host";
        let srflx = "candidate:4 1 udp 1686052607 37.63.112.129 54323 typ srflx raddr 192.168.1.30 rport 54322";
        let mdns = "candidate:5 1 udp 2122260223 4a5b6c7d-0000-1111-2222-333344445555.local 54324 typ host";
        let v6 = "candidate:6 1 udp 2122262783 fd72:6f6f:6d6c::6441:421 54325 typ host";
        assert!(!p.keeps_remote_candidate(host));
        assert!(!p.keeps_remote_candidate(relay), "the loopback-TURN relay");
        assert!(!p.keeps_remote_candidate(v6));
        assert!(p.keeps_remote_candidate(lan));
        assert!(p.keeps_remote_candidate(srflx));
        assert!(
            p.keeps_remote_candidate(mdns),
            "a name passes; it is re-checked after resolution"
        );
        assert!(p.keeps_remote_candidate("garbage"));
        assert!(on().keeps_remote_candidate(host));
    }

    #[test]
    fn the_overlay_turn_server_is_dropped_and_coturn_is_kept() {
        let servers = vec![
            IceServer {
                urls: vec![
                    "stun:turn.roomler.ai:3478".into(),
                    "turn:turn.roomler.ai:3478?transport=udp".into(),
                    "turns:turn.roomler.ai:443?transport=tcp".into(),
                ],
                username: Some("u".into()),
                credential: Some("c".into()),
            },
            IceServer {
                urls: vec!["turn:100.65.4.33:47989".into()],
                username: Some("lu".into()),
                credential: Some("lc".into()),
            },
            IceServer {
                urls: vec![
                    "turn:[fd72:6f6f:6d6c::6441:421]:47989".into(),
                    "turn:203.0.113.7:3478".into(),
                ],
                username: None,
                credential: None,
            },
        ];
        let kept = off().ice_servers(&servers);
        let urls: Vec<&str> = kept
            .iter()
            .flat_map(|s| s.urls.iter().map(String::as_str))
            .collect();
        assert_eq!(
            urls,
            [
                "stun:turn.roomler.ai:3478",
                "turn:turn.roomler.ai:3478?transport=udp",
                "turns:turn.roomler.ai:443?transport=tcp",
                "turn:203.0.113.7:3478",
            ]
        );
        assert_eq!(kept.len(), 2, "a server left with no URL is dropped");
        assert_eq!(kept[0].username.as_deref(), Some("u"), "creds survive");
        assert_eq!(on().ice_servers(&servers).len(), 3, "on = unchanged");
    }

    #[test]
    fn url_hosts_parse_in_every_shape() {
        assert_eq!(url_host_ip("turn:100.64.0.9:47989"), Some(ip("100.64.0.9")));
        assert_eq!(
            url_host_ip("turns:[fd72:6f6f:6d6c::1]:443?transport=tcp"),
            Some(ip("fd72:6f6f:6d6c::1"))
        );
        assert_eq!(url_host_ip("stun:100.64.0.9"), Some(ip("100.64.0.9")));
        assert_eq!(url_host_ip("stun:stun.l.google.com:19302"), None);
        assert_eq!(url_host_ip("nonsense"), None);
    }

    #[test]
    fn sdp_loses_only_its_overlay_candidate_lines() {
        let sdp = "v=0\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=candidate:1 1 udp 2122260223 100.65.4.33 54321 typ host\r\n\
                   a=candidate:3 1 udp 2122260223 192.168.1.30 54322 typ host\r\n\
                   a=end-of-candidates\r\n";
        let out = off().strip_sdp(sdp);
        assert!(!out.contains("100.65.4.33"), "{out}");
        assert!(out.contains("a=candidate:3 1 udp 2122260223 192.168.1.30 54322 typ host\r\n"));
        assert!(out.starts_with("v=0\r\n") && out.ends_with("a=end-of-candidates\r\n"));
        assert_eq!(on().strip_sdp(sdp), sdp);
    }

    /// The delegation channel carries the decision to a macOS GUI worker. An
    /// older daemon sends no field at all, and that must read OFF.
    #[test]
    fn serde_round_trips_and_absent_reads_off() {
        let p = off();
        let back: RcOverlay = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        let empty: RcOverlay = serde_json::from_str(r#"{"allow":false}"#).unwrap();
        assert_eq!(empty, RcOverlay::default());
        assert!(!RcOverlay::default().allow);
        // FR-128 — an older daemon's frame, which has no `auto`, reads
        // not-auto; and a plain off costs no bytes for it.
        assert!(!empty.auto && empty.viewer.is_none() && empty.far_turn.is_none());
        assert!(!serde_json::to_string(&off()).unwrap().contains("auto"));
    }

    /// An `auto` session before its offer: off, with the server's hints.
    fn auto(viewer: Option<&str>, far_turn: Option<bool>) -> RcOverlay {
        RcOverlay {
            auto: true,
            viewer: viewer.map(|v| v.parse().unwrap()),
            far_turn,
            ..off()
        }
    }

    /// The hints cross to a delegated GUI worker, which makes the decision.
    #[test]
    fn auto_and_its_hints_cross_the_delegation_channel() {
        let a = auto(Some("100.65.4.33"), Some(false));
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains(r#""auto":true"#), "{json}");
        assert!(json.contains(r#""viewer":"100.65.4.33""#), "{json}");
        assert_eq!(serde_json::from_str::<RcOverlay>(&json).unwrap(), a);
    }

    /// `auto` only while the policy is not `on`; it starts off; a viewer that
    /// is not an address is no viewer.
    #[test]
    fn resolve_mode_keeps_auto_only_for_an_off_policy() {
        let hints = RcOverlayAutoWire {
            viewer: Some("100.65.4.33".into()),
            far_turn: Some(true),
        };
        let a = RcOverlay::resolve_mode(false, Some(&hints));
        assert!(a.auto && !a.allow);
        assert_eq!(a.viewer, Some(ip("100.65.4.33")));
        assert_eq!(a.far_turn, Some(true));
        let on = RcOverlay::resolve_mode(true, Some(&hints));
        assert!(on.allow && !on.auto && on.viewer.is_none(), "on wins");
        assert!(!RcOverlay::resolve_mode(false, None).auto);
        let junk = RcOverlayAutoWire {
            viewer: Some("not-an-address".into()),
            far_turn: None,
        };
        let a = RcOverlay::resolve_mode(false, Some(&junk));
        assert!(a.auto && a.viewer.is_none());
    }

    /// FR-128 §3.1, the whole table.
    #[test]
    fn auto_allows_the_mesh_only_when_it_is_direct_or_the_only_path() {
        assert!(auto_allows(MeshCarrier::Direct, true));
        assert!(auto_allows(MeshCarrier::Direct, false));
        assert!(
            !auto_allows(MeshCarrier::Relayed, true),
            "the far TURN over UDP wins"
        );
        assert!(
            auto_allows(MeshCarrier::Relayed, false),
            "the only path (§2.4)"
        );
        assert!(!auto_allows(MeshCarrier::Unknown, true));
        assert!(!auto_allows(MeshCarrier::Unknown, false));
    }

    #[test]
    fn a_peers_answer_reads_as_direct_relayed_or_unknown() {
        use tunnel_core::localapi::ConnectionType as C;
        assert_eq!(MeshCarrier::of(Some(C::Direct)), MeshCarrier::Direct);
        assert_eq!(MeshCarrier::of(Some(C::Relay)), MeshCarrier::Relayed);
        assert_eq!(MeshCarrier::of(Some(C::Tunnel)), MeshCarrier::Relayed);
        assert_eq!(MeshCarrier::of(Some(C::Blocked)), MeshCarrier::Unknown);
        assert_eq!(MeshCarrier::of(Some(C::Offline)), MeshCarrier::Unknown);
        assert_eq!(MeshCarrier::of(None), MeshCarrier::Unknown);
    }

    /// The decision at the offer, with the lookup stubbed: what each carrier
    /// and hint makes of the session, and that the outcome is an ordinary
    /// on/off the filters already know.
    #[tokio::test]
    async fn the_offer_decides_from_the_carrier_and_the_far_turn_hint() {
        let decided = |p: RcOverlay, c: MeshCarrier| async move {
            p.decide_with("s", move |_| async move { c }).await
        };
        // The same-network Mac Chrome case: a direct mesh carries it.
        let d = decided(auto(Some("100.65.4.33"), None), MeshCarrier::Direct).await;
        assert!(d.allow && d.auto && d.v4_nets.is_empty(), "{d:?}");
        assert!(d.keeps_local(ip("100.65.4.34")), "on: no filter");
        assert_eq!(
            d.describe(),
            "allowed (device policy rc_overlay=auto, decided for this session)"
        );
        // A relayed mesh gives way to a far TURN, measured or assumed.
        for hint in [Some(true), None] {
            let d = decided(auto(Some("100.65.4.33"), hint), MeshCarrier::Relayed).await;
            assert!(!d.allow && !d.v4_nets.is_empty(), "{hint:?}");
            assert!(!d.keeps_local(ip("100.65.4.34")), "off: filtered");
        }
        // ... unless the far TURN is measured dead: then the mesh is the path.
        let d = decided(auto(Some("100.65.4.33"), Some(false)), MeshCarrier::Relayed).await;
        assert!(d.allow);
        // No answer: off.
        let d = decided(auto(Some("100.65.4.33"), Some(false)), MeshCarrier::Unknown).await;
        assert!(!d.allow);
        assert_eq!(
            d.describe(),
            "excluded (device policy rc_overlay=auto, decided for this session)"
        );
    }

    /// No viewer named: unknown, off, and the lookup is never asked.
    #[tokio::test]
    async fn no_viewer_is_off_without_a_lookup() {
        let d = auto(None, Some(false))
            .decide_with("s", |_| async { panic!("no viewer, no lookup") })
            .await;
        assert!(!d.allow && d.auto);
    }

    /// Not `auto`: the offer leaves the decision exactly as it was.
    #[tokio::test]
    async fn a_session_that_is_not_auto_is_left_as_it_was() {
        for p in [off(), on()] {
            let d = p
                .clone()
                .decide_with("s", |_| async { panic!("not auto, no lookup") })
                .await;
            assert_eq!(d, p);
        }
    }
}
