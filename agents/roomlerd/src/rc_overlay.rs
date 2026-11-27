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
//! "Overlay address" is exact: the blocks this daemon's overlay routes as they stand when a
//! filter judges the address (#3359, [`tunnel_core::overlay_footprint`]), and the derived-v6
//! ULA — never the whole CGNAT `/10`, which LTE modems, cloud VPCs and other meshes use too.
//!
//! On, nothing is filtered beyond the name rule: the behaviour from before
//! the switch existed.
//!
//! FR-148 P4 adds the carrier rank, behind its own switch ([`RankMode`],
//! `ROOMLERD_RC_CARRIER_RANK`, default `0`). With it on, a session the mesh
//! may carry reads the mesh link to the viewer's device at its offer
//! ([`RcOverlay::rank_with`]); unless that link is direct, every candidate only
//! the mesh carries ranks below every relay ([`RcOverlay::ice_rank`]), so the
//! session lands on TURN whenever TURN connects. The session's half of it (the
//! candidate lines, the opening's wait) is in [`crate::carrier_rank`].

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use roomler_ai_remote_control::signaling::{IceServer, RcOverlayAutoWire};
use serde::{Deserialize, Serialize};
use tunnel_core::localapi::{ConnectionType, PeerInfo};
use tunnel_core::overlay_footprint as footprint;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::ice::agent::agent_config::{CarrierRank, MeshAddrFn, ROUTE_HOLD, RouteRank};

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
    /// FR-128 — the viewer's own devices on the overlay, as the server named
    /// them: one per relay it hands this session (FR-121 P3b).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub viewers: Vec<IpAddr>,
    /// FR-128 — whether both ends' last netcheck reached the far TURN over
    /// UDP. `None` = not measured, read as "it works".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub far_turn: Option<bool>,
    /// FR-128 — the viewer devices an `auto` session that rides the mesh
    /// still keeps off it: their mesh link is worse than the best viewer's,
    /// so a relay behind it would carry the video on that worse link.
    /// Decided at the offer, in the process that builds the peer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<IpAddr>,
    /// The overlay's v4 blocks, snapshotted by the DAEMON when the session is
    /// requested. Carried so a delegated session's GUI worker filters by them
    /// too; the filters read them unioned with the footprint ([`Self::blocks`]).
    /// In rank mode they are carried when the mesh is allowed too: the rank
    /// needs them to know a mesh address.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub v4_nets: Vec<(Ipv4Addr, u8)>,
    /// FR-148 P4 — the carrier rank's switch, read by the DAEMON when the
    /// session is requested and carried, as the blocks are: a delegated GUI
    /// worker has neither the daemon's environment nor its config. Absent
    /// (switch `0`, or an older daemon) reads `0`, and switch `0` costs no
    /// byte on the delegation channel.
    #[serde(default, skip_serializing_if = "RankMode::is_off")]
    pub carrier_rank: RankMode,
    /// FR-148 P4 — decided at the offer in rank mode: the mesh link to the
    /// viewer's device is not direct, so every candidate only the mesh carries
    /// ranks last ([`Self::ice_rank`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mesh_last: bool,
}

/// FR-128 — `ROOMLERD_RC_OVERLAY_AUTO=0` makes `auto` behave as `off`.
pub fn auto_enabled() -> bool {
    tunnel_core::env::flag("RC_OVERLAY_AUTO", true)
}

/// FR-148 P4 — `ROOMLERD_RC_CARRIER_RANK` (config key `rc_carrier_rank`):
/// may a session whose mesh link to the viewer is not direct rank its mesh
/// candidates below every relay?
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RankMode {
    /// `0`, the default: no rank, and nothing a session does or sends changes.
    #[default]
    #[serde(rename = "0")]
    Off,
    /// `prio`: the rank, by priority alone.
    #[serde(rename = "prio")]
    Prio,
    /// `1`: the rank, and each ranked-last candidate's line also ends with
    /// ` network-cost 999`, Chrome's own lever.
    #[serde(rename = "1")]
    Cost,
    /// `route` (FR-148 P4 step 2): `1`, and the pair's route is the rank's
    /// unit (`CarrierRank::route`): no pair from a local candidate off the mesh
    /// to a remote on it, and the agent holds its mesh-route pairs while a
    /// relay off the mesh can still come, [`ROUTE_HOLD`] at most.
    #[serde(rename = "route")]
    Route,
}

impl RankMode {
    /// The switch as this process reads it: the environment, then the config
    /// key. Read once per session, where the session is resolved.
    pub fn read() -> Self {
        Self::parse(tunnel_core::env::node_env("RC_CARRIER_RANK").as_deref())
    }

    /// `0`, `prio`, `1` or `route`; anything else, unset included, reads `0`.
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(v) if v.eq_ignore_ascii_case("prio") => Self::Prio,
            Some("1") => Self::Cost,
            Some(v) if v.eq_ignore_ascii_case("route") => Self::Route,
            _ => Self::Off,
        }
    }

    /// Does a ranked-last candidate's line carry ` network-cost 999`?
    pub fn signals_cost(self) -> bool {
        matches!(self, Self::Cost | Self::Route)
    }

    pub fn is_off(&self) -> bool {
        *self == Self::Off
    }

    /// The value as it is set.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "0",
            Self::Prio => "prio",
            Self::Cost => "1",
            Self::Route => "route",
        }
    }
}

impl fmt::Display for RankMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl RcOverlay {
    /// The decision for a session THIS process resolves: the device's policy,
    /// plus this process's overlay blocks when they are needed.
    pub fn resolve(allow: bool) -> Self {
        Self::resolve_mode(allow, None)
    }

    /// [`Self::resolve`] with FR-128's third value, `auto` (the server's
    /// hints present). It stands only while `allow` is off and
    /// [`auto_enabled`] holds; it starts as off, and the offer decides. The
    /// carrier rank's switch is read here, once per session.
    pub fn resolve_mode(allow: bool, auto: Option<&RcOverlayAutoWire>) -> Self {
        Self::resolve_ranked(allow, auto, RankMode::read(), footprint::v4_nets())
    }

    /// [`Self::resolve_mode`] with the carrier rank's switch and this
    /// process's overlay blocks passed in.
    pub fn resolve_ranked(
        allow: bool,
        auto: Option<&RcOverlayAutoWire>,
        carrier_rank: RankMode,
        blocks: Vec<(Ipv4Addr, u8)>,
    ) -> Self {
        let auto = auto.filter(|_| !allow && auto_enabled());
        let mut viewers: Vec<IpAddr> = Vec::new();
        for v in auto.map(|a| a.viewers.as_slice()).unwrap_or_default() {
            if let Ok(ip) = v.parse()
                && !viewers.contains(&ip)
            {
                viewers.push(ip);
            }
        }
        Self {
            allow,
            auto: auto.is_some(),
            viewers,
            far_turn: auto.and_then(|a| a.far_turn),
            exclude: Vec::new(),
            v4_nets: if allow && carrier_rank.is_off() {
                Vec::new()
            } else {
                blocks
            },
            carrier_rank,
            mesh_last: false,
        }
    }

    /// FR-128 — make an `auto` session's decision, at its offer: the live
    /// carrier to each of the viewer's devices ([`mesh_carriers_of`]), the
    /// server's far-TURN hint, and this machine's own reach (D8,
    /// [`own_far_turn`]). The best carrier decides ([`auto_allows`]); a
    /// session that rides the mesh then leaves off every viewer device whose
    /// carrier is worse (`exclude`), so ICE cannot pick a relay behind it.
    /// Anything that is not `auto` comes back as it went in. One line in the
    /// log names the inputs.
    ///
    /// FR-148 P4 — in rank mode the carrier rank decides instead
    /// ([`Self::rank_with`]), from the session's ICE `servers` too, and a
    /// second line about 10 s on reads the same link again.
    pub async fn decide(self, session: impl fmt::Display, servers: &[IceServer]) -> Self {
        if self.carrier_rank.is_off() {
            return self
                .decide_with(session, mesh_carriers_of, own_far_turn)
                .await;
        }
        let session = session.to_string();
        let (decided, viewers, link) = self.rank_with(&session, servers, mesh_links_of).await;
        if !viewers.is_empty() {
            tokio::spawn(link_again_later(session, viewers, link));
        }
        decided
    }

    /// FR-148 P4 — the decision at the offer in rank mode, with the link
    /// lookup passed in for tests. `auto` behaves as `on`, and the far-TURN
    /// hint is not read: the mesh is allowed unless the policy is off, and the
    /// rank does the rest. The viewer's devices are the ones the server named,
    /// else the TURN servers of the offer that sit on the mesh: each viewer
    /// relay is a TURN server on its device's mesh address, and under `on` the
    /// server names none. The best of their links decides: only a direct one
    /// keeps today's rank; relayed, a tunnel, upgrading, stalled, unknown, no
    /// answer and no device known all rank the mesh last. One line in the log
    /// names it all. Also returns the devices read and the link that decided.
    pub async fn rank_with<F, Fut>(
        self,
        session: &str,
        servers: &[IceServer],
        lookup: F,
    ) -> (Self, Vec<IpAddr>, Link)
    where
        F: FnOnce(Vec<IpAddr>) -> Fut,
        Fut: std::future::Future<Output = Option<Vec<MeshLink>>>,
    {
        let allow = self.allow || self.auto;
        let (viewers, from) = if allow {
            self.viewer_devices(servers)
        } else {
            (Vec::new(), ViewersFrom::None)
        };
        let started = Instant::now();
        let link = if !allow {
            Link::Off
        } else if viewers.is_empty() {
            Link::NoViewer
        } else {
            Link::best(lookup(viewers.clone()).await)
        };
        let read = started.elapsed();
        let decided = Self {
            allow,
            exclude: Vec::new(),
            mesh_last: allow && !link.is_direct(),
            ..self
        };
        decided.log_rank(session, link, (viewers.len(), from), read);
        (decided, viewers, link)
    }

    /// The viewer's devices, and where they came from (see [`Self::rank_with`]).
    fn viewer_devices(&self, servers: &[IceServer]) -> (Vec<IpAddr>, ViewersFrom) {
        if !self.viewers.is_empty() {
            return (self.viewers.clone(), ViewersFrom::Named);
        }
        let mut viewers: Vec<IpAddr> = Vec::new();
        for url in servers.iter().flat_map(|s| &s.urls) {
            if (url.starts_with("turn:") || url.starts_with("turns:"))
                && let Some(ip) = url_host_ip(url)
                && footprint::is_overlay_addr(&self.v4_nets, ip)
                && !viewers.contains(&ip)
            {
                viewers.push(ip);
            }
        }
        let from = if viewers.is_empty() {
            ViewersFrom::None
        } else {
            ViewersFrom::RelayUrls
        };
        (viewers, from)
    }

    /// FR-148 P4 — the carrier rank this session's ICE agent gets: `None`
    /// under switch `0`. In rank mode a relay allocated over TCP or TLS ranks
    /// below one over UDP, and the agent keeps checking the pairs it would
    /// follow; with `mesh_last`, every local candidate on the mesh's own
    /// blocks or the derived-v6 ULA ([`footprint::is_overlay_addr`], never the
    /// whole CGNAT `/10`) ranks last.
    ///
    /// FR-148 P4 step 2 (D49): under `route` alone, the route rank too, with
    /// the same addresses whatever the link ([`RouteRank::mesh`]): its one
    /// predicate is what the follow gate, the checks above the selection, the
    /// opening and the carrier record read. It refuses and holds pairs only
    /// with `mesh_last` set as well.
    pub fn ice_rank(&self) -> Option<CarrierRank> {
        if self.carrier_rank.is_off() {
            return None;
        }
        let mesh = || {
            let nets = self.v4_nets.clone();
            Box::new(move |ip: IpAddr| footprint::is_overlay_addr(&nets, ip)) as MeshAddrFn
        };
        let mesh_last = self.mesh_last.then(mesh);
        let route = (self.carrier_rank == RankMode::Route).then(|| RouteRank {
            hold: ROUTE_HOLD,
            mesh: mesh(),
        });
        Some(CarrierRank { mesh_last, route })
    }

    /// [`Self::ice_rank`], set on the session's `SettingEngine`.
    pub fn rank_ice(&self, setting: &mut SettingEngine) {
        if let Some(rank) = self.ice_rank() {
            setting.set_ice_carrier_rank(rank);
        }
    }

    /// The offer's line in rank mode.
    fn log_rank(&self, session: &str, link: Link, viewers: (usize, ViewersFrom), read: Duration) {
        let ranked_last = if self.mesh_last {
            "host,srflx,relay"
        } else {
            "none"
        };
        tracing::info!(
            session = %session,
            carrier_rank = %self.carrier_rank,
            link = %link,
            upgrading = link.upgrading(),
            stalled = link.stalled(),
            viewers = viewers.0,
            viewers_from = %viewers.1,
            read_ms = read.as_millis() as u64,
            %ranked_last,
            mesh_blocks = %MeshBlocks(&self.v4_nets),
            "rc ICE: carrier rank (FR-148 P4)"
        );
    }

    /// [`Self::decide`] with both lookups passed in, for tests. The carrier
    /// lookup answers one carrier per address, in order; `own` answers this
    /// machine's own far-TURN reach. They run side by side.
    pub async fn decide_with<F, Fut, G, GFut>(
        self,
        session: impl std::fmt::Display,
        lookup: F,
        own: G,
    ) -> Self
    where
        F: FnOnce(Vec<IpAddr>) -> Fut,
        Fut: std::future::Future<Output = Vec<MeshCarrier>>,
        G: FnOnce() -> GFut,
        GFut: std::future::Future<Output = Option<bool>>,
    {
        if !self.auto {
            return self;
        }
        let (carriers, own_far_turn) = if self.viewers.is_empty() {
            (Vec::new(), None)
        } else {
            tokio::join!(lookup(self.viewers.clone()), own())
        };
        let best = carriers
            .iter()
            .copied()
            .max_by_key(|c| c.rank())
            .unwrap_or(MeshCarrier::Unknown);
        let far_turn = with_own_reach(self.far_turn, own_far_turn);
        let allow = auto_allows(best, far_turn.unwrap_or(true));
        let exclude: Vec<IpAddr> = if allow {
            self.viewers
                .iter()
                .zip(&carriers)
                .filter(|(_, c)| **c != best)
                .map(|(ip, _)| *ip)
                .collect()
        } else {
            Vec::new()
        };
        tracing::info!(
            %session,
            viewers = ?self.viewers,
            ?carriers,
            far_turn = ?self.far_turn,
            ?own_far_turn,
            allow,
            ?exclude,
            "rc ICE: overlay auto (FR-128) — {}",
            if allow {
                "the session may ride the mesh"
            } else {
                "the session stays off the mesh"
            }
        );
        Self {
            allow,
            exclude,
            v4_nets: if allow { Vec::new() } else { self.v4_nets },
            ..self
        }
    }

    fn blocks(&self, ip: IpAddr) -> bool {
        (!self.allow && self.is_overlay_now(ip)) || self.exclude.contains(&ip)
    }

    /// Nothing to filter at all: `on`, or an `auto` session that rides the
    /// mesh through every viewer device it was named.
    fn filters_nothing(&self) -> bool {
        self.allow && self.exclude.is_empty()
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
        if self.filters_nothing() {
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
        if self.filters_nothing() {
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

    /// Which of several viewer devices' links is best: direct, then relayed,
    /// then unknown.
    fn rank(self) -> u8 {
        match self {
            Self::Direct => 2,
            Self::Relayed => 1,
            Self::Unknown => 0,
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

/// FR-128 — the mesh carrier to the peer that owns each of `addrs`, in order:
/// ONE LocalAPI `peers` call (the `roomler peers` verb) within 400 ms, the
/// bound the relay-tier guard uses. Ungated, unlike that guard, because any
/// session can be `auto`. No answer in time reads every address as unknown.
pub async fn mesh_carriers_of(addrs: Vec<IpAddr>) -> Vec<MeshCarrier> {
    let peers = peers_answer().await;
    addrs
        .iter()
        .map(|addr| match &peers {
            Some(peers) => MeshCarrier::of(peer_at(peers, addr).map(|p| p.connection)),
            None => MeshCarrier::Unknown,
        })
        .collect()
}

/// ONE LocalAPI `peers` call within 400 ms. `None`: no answer in time.
async fn peers_answer() -> Option<Vec<PeerInfo>> {
    let query = async {
        let mut client = tunnel_core::localapi::connect().await.ok()?;
        client.peers().await.ok()
    };
    tokio::time::timeout(Duration::from_millis(400), query)
        .await
        .ok()
        .flatten()
}

/// The peer whose overlay address, v4 or v6, is `addr`.
fn peer_at<'a>(peers: &'a [PeerInfo], addr: &IpAddr) -> Option<&'a PeerInfo> {
    let addr = addr.to_string();
    peers.iter().find(|p| {
        p.overlay_ip.as_deref() == Some(addr.as_str())
            || p.overlay_ip6.as_deref() == Some(addr.as_str())
    })
}

/// FR-148 P4 — the mesh link to the peer that owns each of `addrs`, in
/// order, from the same ONE `peers` call within 400 ms as
/// [`mesh_carriers_of`]. `None`: no answer in time.
pub async fn mesh_links_of(addrs: Vec<IpAddr>) -> Option<Vec<MeshLink>> {
    let peers = peers_answer().await?;
    Some(
        addrs
            .iter()
            .map(|addr| MeshLink::of(peer_at(&peers, addr)))
            .collect(),
    )
}

/// FR-148 P4 — one viewer device's mesh link, as LocalAPI `peers` reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshLink {
    /// `None`: the device is not in the answer.
    pub connection: Option<ConnectionType>,
    /// A direct upgrade is being tried: the link is still relayed.
    pub upgrading: bool,
    /// The carrier is installed but silently one-way.
    pub stalled: bool,
}

impl MeshLink {
    fn of(peer: Option<&PeerInfo>) -> Self {
        Self {
            connection: peer.map(|p| p.connection),
            upgrading: peer.is_some_and(|p| p.upgrading),
            stalled: peer.is_some_and(|p| p.stalled),
        }
    }

    /// Direct, and neither upgrading nor stalled: the one link that keeps
    /// today's rank.
    pub fn is_direct(&self) -> bool {
        self.connection == Some(ConnectionType::Direct) && !self.upgrading && !self.stalled
    }

    /// Which of several devices' links is best.
    fn rank(&self) -> u8 {
        match self.connection {
            _ if self.is_direct() => 5,
            Some(ConnectionType::Direct) => 4,
            Some(ConnectionType::Relay) => 3,
            Some(ConnectionType::Tunnel) => 2,
            Some(ConnectionType::Blocked | ConnectionType::Offline) => 1,
            None => 0,
        }
    }
}

/// FR-148 P4 — the link that decided a session's rank, as its lines name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// The policy is off: no mesh, nothing read.
    Off,
    /// No viewer device known: nothing to read.
    NoViewer,
    /// The daemon did not answer within 400 ms.
    NoAnswer,
    /// The best viewer device's link.
    Read(MeshLink),
}

impl Link {
    /// The best of a `peers` answer's links; `None` is no answer.
    pub fn best(links: Option<Vec<MeshLink>>) -> Self {
        match links {
            None => Self::NoAnswer,
            Some(links) => links
                .into_iter()
                .max_by_key(MeshLink::rank)
                .map_or(Self::NoViewer, Self::Read),
        }
    }

    pub fn is_direct(&self) -> bool {
        matches!(self, Self::Read(l) if l.is_direct())
    }

    pub fn upgrading(&self) -> bool {
        matches!(self, Self::Read(l) if l.upgrading)
    }

    pub fn stalled(&self) -> bool {
        matches!(self, Self::Read(l) if l.stalled)
    }
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::NoViewer => "no-viewer",
            Self::NoAnswer => "no-answer",
            Self::Read(l) => match l.connection {
                Some(ConnectionType::Direct) => "direct",
                Some(ConnectionType::Relay) => "relay",
                Some(ConnectionType::Tunnel) => "tunnel",
                Some(ConnectionType::Blocked) => "blocked",
                Some(ConnectionType::Offline) => "offline",
                None => "unknown",
            },
        })
    }
}

/// FR-148 P4 — where a session's viewer devices came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewersFrom {
    /// The server named them (`auto`).
    Named,
    /// The offer's TURN servers on a mesh address.
    RelayUrls,
    /// Neither.
    None,
}

impl fmt::Display for ViewersFrom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Named => "named",
            Self::RelayUrls => "relay-urls",
            Self::None => "none",
        })
    }
}

/// The blocks a mesh address is in, as the offer's line names them: the
/// carried v4 blocks and the derived-v6 ULA.
struct MeshBlocks<'a>(&'a [(Ipv4Addr, u8)]);

impl fmt::Display for MeshBlocks<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (net, len) in self.0 {
            write!(f, "{net}/{len},")?;
        }
        f.write_str("fd72:6f6f:6d6c::/96")
    }
}

/// FR-148 P4 — how long after the offer the link is read again.
const LINK_AGAIN_AFTER: Duration = Duration::from_secs(10);

/// FR-148 P4 — the line about 10 s after the open: the same viewer devices'
/// link then, beside the one that decided at the offer. How often a link that
/// read relayed at the offer is direct a moment later decides the switch's
/// default.
async fn link_again_later(session: String, viewers: Vec<IpAddr>, at_offer: Link) {
    tokio::time::sleep(LINK_AGAIN_AFTER).await;
    let started = Instant::now();
    let now = Link::best(mesh_links_of(viewers).await);
    log_link_again(&session, at_offer, now, started.elapsed());
}

fn log_link_again(session: &str, at_offer: Link, now: Link, read: Duration) {
    tracing::info!(
        session = %session,
        link_at_offer = %at_offer,
        link = %now,
        upgrading = now.upgrading(),
        stalled = now.stalled(),
        read_ms = read.as_millis() as u64,
        "rc ICE: carrier rank, the mesh link 10 s on (FR-148 P4)"
    );
}

/// FR-128 D8 — how old this machine's netcheck may be and still count: the
/// gate every consumer uses (tunnel-core's `FRESHNESS_MAX_AGE`, the server's
/// `fresh_caps`), three times its 20-minute cadence.
const NETCHECK_FRESH_S: u64 = 60 * 60;

/// FR-128 D8 — this machine's own far-TURN reach, by its daemon's last
/// netcheck: `stun_udp` alone (P2b), behind the freshness gate. ONE LocalAPI
/// `status` call within the 400 ms the carrier lookup has, run beside it. A
/// Mac's GUI worker runs no overlay, and its row has no node the server's
/// hint could measure, so it reads its daemon's vector here (the per-user
/// half lends it, `person_socket::with_machine_network`). No answer in time,
/// or no fresh vector, is `None`: not measured.
pub async fn own_far_turn() -> Option<bool> {
    let query = async {
        let mut client = tunnel_core::localapi::connect().await.ok()?;
        client.status().await.ok()
    };
    let status = tokio::time::timeout(std::time::Duration::from_millis(400), query)
        .await
        .ok()
        .flatten()?;
    status
        .netcheck
        .filter(|n| n.age_s < NETCHECK_FRESH_S)
        .map(|n| n.stun_udp)
}

/// FR-128 D8 — the server's hint with this machine's own reach folded in: a
/// target whose netcheck found no UDP out reaches no far TURN, whatever the
/// server could see. Anything else leaves the hint as the server sent it,
/// which already holds this machine's vector wherever the server found one.
fn with_own_reach(hint: Option<bool>, own: Option<bool>) -> Option<bool> {
    if own == Some(false) {
        Some(false)
    } else {
        hint
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

/// #3359 — the footprint as it stands, for the four filters.
impl RcOverlay {
    /// Is `ip` an overlay address for this session's filters
    /// ([`Self::blocks`])? By the footprint AS IT STANDS when the filter
    /// judges the address ([`footprint::is_overlay_addr_now`]), unioned with
    /// the copy the session carried (`v4_nets`, all a delegated worker held
    /// before the daemon's first push). A session requested before the first
    /// netmap carries an empty copy for its whole life, and still filters by
    /// every block recorded after it. The footprint only grows, so a later
    /// judgement can only keep more out, and neither side is ever the CGNAT
    /// `/10`.
    ///
    /// ⚠️ Called from the ICE agent's gather (the `SettingEngine` IP filter):
    /// the registry's lock is a std one, never held across an `.await`.
    fn is_overlay_now(&self, ip: IpAddr) -> bool {
        footprint::is_overlay_addr(&self.v4_nets, ip) || footprint::is_overlay_addr_now(ip)
    }

    /// The v4 blocks this session's filters judge by, counted for its
    /// `rc ICE` line (`overlay_blocks`): kept off the mesh, the footprint as
    /// it stands now unioned with the session's copy; allowed, the copy, as
    /// before (empty, or the carrier rank's blocks). The footprint only
    /// grows, so the gather that follows can only hold more.
    pub fn overlay_blocks(&self) -> usize {
        if self.allow {
            return self.v4_nets.len();
        }
        let mut held = footprint::v4_nets();
        for net in &self.v4_nets {
            if !held.contains(net) {
                held.push(*net);
            }
        }
        held.len()
    }
}

/// #3359 — what each of a session's four filters does with `addr`, in order:
/// a host candidate on it ([`RcOverlay::keeps_local`]), a trickled remote
/// candidate ([`RcOverlay::keeps_remote_candidate`]), a candidate in the offer
/// ([`RcOverlay::strip_sdp`]) and a TURN server on it
/// ([`RcOverlay::ice_servers`]). `true` where the filter lets it through.
#[cfg(test)]
pub(crate) fn four_filters(p: &RcOverlay, addr: &str) -> [bool; 4] {
    let ip: IpAddr = addr.parse().expect("an address");
    let candidate = format!("candidate:1 1 udp 2122260223 {addr} 54321 typ host");
    let sdp = format!("v=0\r\na={candidate}\r\na=end-of-candidates\r\n");
    let turn = if ip.is_ipv6() {
        format!("turn:[{addr}]:47989")
    } else {
        format!("turn:{addr}:47989")
    };
    let servers = [IceServer {
        urls: vec![turn],
        username: Some("u".into()),
        credential: Some("c".into()),
    }];
    [
        p.keeps_local(ip),
        p.keeps_remote_candidate(&candidate),
        p.strip_sdp(&sdp).contains(addr),
        !p.ice_servers(&servers).is_empty(),
    ]
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
        assert!(!empty.auto && empty.viewers.is_empty() && empty.far_turn.is_none());
        assert!(empty.exclude.is_empty());
        assert!(!serde_json::to_string(&off()).unwrap().contains("auto"));
    }

    /// An `auto` session before its offer: off, with the server's hints.
    fn auto(viewers: &[&str], far_turn: Option<bool>) -> RcOverlay {
        RcOverlay {
            auto: true,
            viewers: viewers.iter().map(|v| v.parse().unwrap()).collect(),
            far_turn,
            ..off()
        }
    }

    /// The hints cross to a delegated GUI worker, which makes the decision.
    #[test]
    fn auto_and_its_hints_cross_the_delegation_channel() {
        let a = auto(&["100.65.4.33"], Some(false));
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains(r#""auto":true"#), "{json}");
        assert!(json.contains(r#""viewers":["100.65.4.33"]"#), "{json}");
        assert_eq!(serde_json::from_str::<RcOverlay>(&json).unwrap(), a);
    }

    /// `auto` only while the policy is not `on`; it starts off; a viewer that
    /// is not an address is no viewer, and one named twice counts once.
    #[test]
    fn resolve_mode_keeps_auto_only_for_an_off_policy() {
        let hints = RcOverlayAutoWire {
            viewers: vec![
                "100.65.4.33".into(),
                "100.65.4.33".into(),
                "100.65.4.40".into(),
            ],
            far_turn: Some(true),
        };
        let a = RcOverlay::resolve_mode(false, Some(&hints));
        assert!(a.auto && !a.allow);
        assert_eq!(a.viewers, vec![ip("100.65.4.33"), ip("100.65.4.40")]);
        assert_eq!(a.far_turn, Some(true));
        let on = RcOverlay::resolve_mode(true, Some(&hints));
        assert!(on.allow && !on.auto && on.viewers.is_empty(), "on wins");
        assert!(!RcOverlay::resolve_mode(false, None).auto);
        let junk = RcOverlayAutoWire {
            viewers: vec!["not-an-address".into()],
            far_turn: None,
        };
        let a = RcOverlay::resolve_mode(false, Some(&junk));
        assert!(a.auto && a.viewers.is_empty());
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
            p.decide_with(
                "s",
                move |addrs: Vec<IpAddr>| async move { vec![c; addrs.len()] },
                || async { None },
            )
            .await
        };
        // The same-network Mac Chrome case: a direct mesh carries it.
        let d = decided(auto(&["100.65.4.33"], None), MeshCarrier::Direct).await;
        assert!(d.allow && d.auto && d.v4_nets.is_empty(), "{d:?}");
        assert!(d.exclude.is_empty(), "{d:?}");
        assert!(d.keeps_local(ip("100.65.4.34")), "on: no filter");
        assert_eq!(
            d.describe(),
            "allowed (device policy rc_overlay=auto, decided for this session)"
        );
        // A relayed mesh gives way to a far TURN, measured or assumed.
        for hint in [Some(true), None] {
            let d = decided(auto(&["100.65.4.33"], hint), MeshCarrier::Relayed).await;
            assert!(!d.allow && !d.v4_nets.is_empty(), "{hint:?}");
            assert!(!d.keeps_local(ip("100.65.4.34")), "off: filtered");
        }
        // ... unless the far TURN is measured dead: then the mesh is the path.
        let d = decided(auto(&["100.65.4.33"], Some(false)), MeshCarrier::Relayed).await;
        assert!(d.allow);
        // No answer: off.
        let d = decided(auto(&["100.65.4.33"], Some(false)), MeshCarrier::Unknown).await;
        assert!(!d.allow);
        assert_eq!(
            d.describe(),
            "excluded (device policy rc_overlay=auto, decided for this session)"
        );
    }

    /// No viewer named: unknown, off, and neither lookup is ever asked.
    #[tokio::test]
    async fn no_viewer_is_off_without_a_lookup() {
        let d = auto(&[], Some(false))
            .decide_with(
                "s",
                |_| async { panic!("no viewer, no lookup") },
                || async { panic!("no viewer, no netcheck") },
            )
            .await;
        assert!(!d.allow && d.auto);
    }

    /// FR-128 D8 — the target's own netcheck, read at the offer. One that
    /// found no UDP out makes a relayed mesh the only path, whatever the
    /// server could see: for a Mac's GUI worker, whose row has no node, the
    /// server's hint reads `None`. Anything else leaves the hint as sent.
    #[tokio::test]
    async fn the_targets_own_netcheck_folds_into_the_hint() {
        let decided = |c: MeshCarrier, hint: Option<bool>, own: Option<bool>| async move {
            auto(&["100.65.4.33"], hint)
                .decide_with(
                    "s",
                    move |addrs: Vec<IpAddr>| async move { vec![c; addrs.len()] },
                    move || async move { own },
                )
                .await
                .allow
        };
        use MeshCarrier::*;
        assert!(
            decided(Relayed, None, Some(false)).await,
            "a Mac target with no UDP out: the mesh is the only path"
        );
        assert!(
            decided(Relayed, Some(true), Some(false)).await,
            "its own read wins toward dead"
        );
        assert!(!decided(Relayed, None, Some(true)).await);
        assert!(
            !decided(Relayed, None, None).await,
            "nothing measured: the far TURN is assumed"
        );
        assert!(
            decided(Relayed, Some(false), Some(true)).await,
            "the browser's end measured dead stands"
        );
        assert!(decided(Direct, Some(true), Some(true)).await);
        assert!(!decided(Unknown, None, Some(false)).await, "fail safe");
        assert_eq!(with_own_reach(Some(true), None), Some(true));
        assert_eq!(with_own_reach(None, Some(true)), None);
    }

    /// FR-121 P3b — one computer, two agents, two relays: Windows direct to
    /// the target, WSL on DERP. The best link decides, and the relay behind
    /// the worse one is left off every filter, so ICE cannot pick it.
    #[tokio::test]
    async fn two_viewer_devices_ride_only_the_best_link() {
        use MeshCarrier::*;
        let (win, wsl) = ("100.65.4.33", "100.65.4.40");
        let decided = |p: RcOverlay, cs: Vec<MeshCarrier>| async move {
            p.decide_with(
                "s",
                move |addrs: Vec<IpAddr>| async move {
                    assert_eq!(addrs.len(), cs.len(), "one carrier per viewer");
                    cs
                },
                || async { None },
            )
            .await
        };
        let d = decided(auto(&[win, wsl], None), vec![Direct, Relayed]).await;
        assert!(d.allow, "{d:?}");
        assert_eq!(d.exclude, vec![ip(wsl)]);
        assert!(d.keeps_local(ip("100.65.4.34")), "our own overlay is fine");
        let via = |addr: &str| format!("candidate:2 1 udp 41885439 {addr} 61000 typ relay");
        assert!(d.keeps_remote_candidate(&via(win)));
        assert!(!d.keeps_remote_candidate(&via(wsl)));
        let servers = vec![
            IceServer {
                urls: vec![format!("turn:{win}:47989")],
                username: Some("w".into()),
                credential: Some("w".into()),
            },
            IceServer {
                urls: vec![format!("turn:{wsl}:47990")],
                username: Some("l".into()),
                credential: Some("l".into()),
            },
            IceServer {
                urls: vec!["turn:turn.roomler.ai:3478?transport=udp".into()],
                username: Some("u".into()),
                credential: Some("c".into()),
            },
        ];
        let urls: Vec<String> = d
            .ice_servers(&servers)
            .into_iter()
            .flat_map(|s| s.urls)
            .collect();
        assert_eq!(
            urls,
            [
                format!("turn:{win}:47989"),
                "turn:turn.roomler.ai:3478?transport=udp".to_string()
            ]
        );
        let sdp = format!(
            "v=0\r\na=candidate:{}\r\na=candidate:{}\r\n",
            &via(win)[10..],
            &via(wsl)[10..]
        );
        let out = d.strip_sdp(&sdp);
        assert!(out.contains(win) && !out.contains(wsl), "{out}");

        // The order does not matter, and two direct links leave nothing out.
        let d = decided(auto(&[wsl, win], None), vec![Relayed, Direct]).await;
        assert!(d.allow && d.exclude == vec![ip(wsl)], "{d:?}");
        let d = decided(auto(&[win, wsl], None), vec![Direct, Direct]).await;
        assert!(d.allow && d.exclude.is_empty(), "{d:?}");
        assert_eq!(d.ice_servers(&servers).len(), 3, "nothing to filter");
        // With the far TURN dead, a relayed link is the path, and a device with
        // no link at all is still left off.
        let d = decided(auto(&[win, wsl], Some(false)), vec![Relayed, Unknown]).await;
        assert!(d.allow && d.exclude == vec![ip(wsl)], "{d:?}");
        // Both relayed, the far TURN alive: off, and nothing to exclude.
        let d = decided(auto(&[win, wsl], None), vec![Relayed, Relayed]).await;
        assert!(!d.allow && d.exclude.is_empty(), "{d:?}");
    }

    /// Not `auto`: the offer leaves the decision exactly as it was.
    #[tokio::test]
    async fn a_session_that_is_not_auto_is_left_as_it_was() {
        for p in [off(), on()] {
            let d = p
                .clone()
                .decide_with(
                    "s",
                    |_| async { panic!("not auto, no lookup") },
                    || async { panic!("not auto, no netcheck") },
                )
                .await;
            assert_eq!(d, p);
        }
    }

    // ---- #3359: the footprint as it stands, not as it was requested ----

    /// #3359 — a session requested while the daemon starts is resolved before
    /// any block is recorded, so its own copy is empty for its whole life. The
    /// device's block is then recorded before the host candidates are
    /// gathered, and from that moment every filter keeps the overlay address
    /// out. A 100.64/10 address in no recorded block (an LTE modem, another
    /// mesh) still passes all four: the rule never widens to the CGNAT `/10`.
    #[test]
    fn a_session_resolved_before_its_blocks_keeps_the_overlay_out_once_they_are_recorded() {
        // Process-global: asserts only on a block this test owns.
        let session = RcOverlay::resolve_ranked(false, None, RankMode::Off, Vec::new());
        assert!(session.v4_nets.is_empty(), "resolved before any block");
        footprint::note_v4_blocks(&[(Ipv4Addr::new(100, 72, 4, 0), 22)]);
        assert_eq!(
            four_filters(&session, "100.72.4.34"),
            [false; 4],
            "the device's overlay address, in keeps_local, keeps_remote_candidate, strip_sdp and ice_servers"
        );
        for not_ours in ["100.73.9.9", "100.80.1.2", "192.168.1.20"] {
            assert_eq!(
                four_filters(&session, not_ours),
                [true; 4],
                "{not_ours} is in no recorded block"
            );
        }
    }

    /// #3359 — `auto`, decided "stays off the mesh" at its offer, filters the
    /// way `off` does: by the footprint as it stands when an address is
    /// judged, whatever its copy held when it was requested.
    #[tokio::test]
    async fn an_auto_session_that_stays_off_filters_as_off_does() {
        // Process-global: asserts only on a block this test owns.
        let hints = RcOverlayAutoWire {
            viewers: vec!["100.65.4.33".into()],
            far_turn: Some(true),
        };
        let requested = RcOverlay::resolve_ranked(false, Some(&hints), RankMode::Off, Vec::new());
        assert!(requested.auto && !requested.allow && requested.v4_nets.is_empty());
        // A relayed mesh while the far TURN answers over UDP: the session
        // stays off the mesh (FR-128 §3.1).
        let stays_off = requested
            .decide_with(
                "s",
                |_| async { vec![MeshCarrier::Relayed] },
                || async { Some(true) },
            )
            .await;
        assert!(stays_off.auto && !stays_off.allow, "{stays_off:?}");
        footprint::note_v4_blocks(&[(Ipv4Addr::new(100, 72, 8, 0), 22)]);
        assert_eq!(
            four_filters(&stays_off, "100.72.8.21"),
            [false; 4],
            "the device's overlay address, in each of the four"
        );
        let off = RcOverlay::resolve_ranked(false, None, RankMode::Off, Vec::new());
        for addr in [
            "100.72.8.21",
            "fd72:6f6f:6d6c::6448:815",
            "100.73.9.9",
            "192.168.1.20",
        ] {
            assert_eq!(
                four_filters(&stays_off, addr),
                four_filters(&off, addr),
                "{addr}"
            );
        }
    }

    // ---- FR-148 P4: the carrier rank ----

    use tunnel_core::localapi::ConnectionType as C;

    /// The blocks a daemon snapshots in these cells.
    fn blocks() -> Vec<(Ipv4Addr, u8)> {
        vec![(Ipv4Addr::new(100, 65, 4, 0), 22)]
    }

    fn link(connection: Option<C>, upgrading: bool, stalled: bool) -> MeshLink {
        MeshLink {
            connection,
            upgrading,
            stalled,
        }
    }

    fn relayed() -> MeshLink {
        link(Some(C::Relay), false, false)
    }

    fn server(urls: &[&str]) -> IceServer {
        IceServer {
            urls: urls.iter().map(|u| u.to_string()).collect(),
            username: Some("u".into()),
            credential: Some("c".into()),
        }
    }

    /// An `on` session in rank mode, as the daemon resolves it.
    fn ranked_on() -> RcOverlay {
        RcOverlay::resolve_ranked(true, None, RankMode::Prio, blocks())
    }

    /// The offer's TURN servers: coturn, and the viewer's own relay on its
    /// mesh address.
    fn with_viewer_relay() -> Vec<IceServer> {
        vec![
            server(&["turn:turn.roomler.ai:3478?transport=udp"]),
            server(&["turn:100.65.4.33:47989"]),
        ]
    }

    #[test]
    fn the_rank_switch_reads_0_prio_or_1_and_anything_else_as_0() {
        assert_eq!(RankMode::parse(None), RankMode::Off);
        for v in ["0", "", " ", "on", "true", "2", "yes", "prio1", "cost"] {
            assert_eq!(RankMode::parse(Some(v)), RankMode::Off, "{v:?}");
        }
        assert_eq!(RankMode::parse(Some("prio")), RankMode::Prio);
        assert_eq!(RankMode::parse(Some(" PRIO ")), RankMode::Prio);
        assert_eq!(RankMode::parse(Some("1")), RankMode::Cost);
        assert_eq!(RankMode::parse(Some(" Route ")), RankMode::Route);
        for m in [
            RankMode::Off,
            RankMode::Prio,
            RankMode::Cost,
            RankMode::Route,
        ] {
            assert_eq!(RankMode::parse(Some(m.as_str())), m);
        }
    }

    /// FR-148 P4 step 2: the route rank reaches ICE under `route` alone, with
    /// the mesh's blocks ranked last as under `1`, and with its cost lever.
    #[test]
    fn only_route_hands_ice_the_route_rank() {
        for mode in [RankMode::Prio, RankMode::Cost, RankMode::Route] {
            let mut d = RcOverlay::resolve_ranked(true, None, mode, blocks());
            d.mesh_last = true;
            let rank = d.ice_rank().expect("rank mode");
            assert!(rank.mesh_last.is_some(), "{mode}");
            assert_eq!(rank.route.is_some(), mode == RankMode::Route, "{mode}");
            if let Some(route) = rank.route {
                assert_eq!(route.hold, ROUTE_HOLD);
                assert!((route.mesh)(ip("100.65.4.34")), "the mesh's own block");
            }
            assert_eq!(mode.signals_cost(), mode != RankMode::Prio, "{mode}");
        }
        assert!(
            RcOverlay::resolve_ranked(true, None, RankMode::Off, blocks())
                .ice_rank()
                .is_none()
        );
    }

    /// FR-148 P4 step 2, A2: the route rank's addresses are the mesh's own
    /// blocks and the ULA, on a direct link too, so its one predicate (which
    /// the follow gate, the checks above the selection, the opening and the
    /// carrier record read) takes an address of 100.64.0.0/10 outside them, a
    /// carrier's CGNAT or a corporate VPN's, for a real path. Under `prio` and
    /// `1` there is no route rank: the follow gate reads the whole range, as
    /// built.
    #[test]
    fn a2_the_route_ranks_predicate_reads_the_mesh_blocks_not_the_whole_range() {
        use webrtc::ice::agent::agent_config::rides_the_mesh;
        // A relay allocated over UDP, off the mesh.
        const UDP_RELAY: u32 = (1 << 8) * 65535 + 255;
        let relay = |address| (UDP_RELAY, address);
        for mesh_last in [true, false] {
            let mut d = RcOverlay::resolve_ranked(true, None, RankMode::Route, blocks());
            d.mesh_last = mesh_last;
            let rank = d.ice_rank().expect("rank mode");
            assert_eq!(rank.mesh_last.is_some(), mesh_last);
            let route = rank.route.expect("route");
            let rides = |(priority, local): (u32, &str), remote: &str| {
                rides_the_mesh(&*route.mesh, priority, 1, local, remote)
            };
            let link = if mesh_last { "relayed" } else { "direct" };
            assert!(
                !rides(relay("203.0.113.7"), "100.80.1.2"),
                "{link}: a carrier's CGNAT is a real path"
            );
            assert!(
                rides(relay("203.0.113.7"), "100.65.4.33"),
                "{link}: a mesh remote"
            );
            assert!(
                rides(relay("203.0.113.7"), "fd72:6f6f:6d6c::6441:421"),
                "{link}: the ULA"
            );
            assert!(
                rides((255, "203.0.113.7"), "198.51.100.4"),
                "{link}: ranked last"
            );
            assert!(
                rides((0, "100.65.4.34"), "198.51.100.4"),
                "{link}: a mesh local"
            );
            assert!(
                !rides(relay("203.0.113.7"), "4a5b6c7d.local"),
                "{link}: a name is no mesh address"
            );
        }
        for mode in [RankMode::Prio, RankMode::Cost] {
            let mut d = RcOverlay::resolve_ranked(true, None, mode, blocks());
            d.mesh_last = true;
            assert!(d.ice_rank().expect("rank mode").route.is_none(), "{mode}");
        }
    }

    /// `RcOverlay` before FR-148 P4, as an older daemon or GUI worker has it:
    /// the fields and their attributes, copied.
    #[derive(Serialize, Deserialize, Debug, Default, PartialEq)]
    struct Before {
        allow: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        auto: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        viewers: Vec<IpAddr>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        far_turn: Option<bool>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude: Vec<IpAddr>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        v4_nets: Vec<(Ipv4Addr, u8)>,
    }

    impl Before {
        fn of(p: &RcOverlay) -> Self {
            Self {
                allow: p.allow,
                auto: p.auto,
                viewers: p.viewers.clone(),
                far_turn: p.far_turn,
                exclude: p.exclude.clone(),
                v4_nets: p.v4_nets.clone(),
            }
        }
    }

    /// Switch `0` costs no byte on the delegation channel: every shape a
    /// session resolves or decides to serializes exactly as before. In rank
    /// mode the switch and the blocks cross, `on` included, so a delegated
    /// worker ranks on the daemon's blocks; and an older worker still reads
    /// the frame, as the `on` it always was.
    #[tokio::test]
    async fn switch_0_serializes_as_before_and_an_older_worker_reads_rank_mode() {
        let hints = RcOverlayAutoWire {
            viewers: vec!["100.65.4.33".into()],
            far_turn: Some(true),
        };
        let decided = auto(&["100.65.4.33", "100.65.4.40"], None)
            .decide_with(
                "s",
                |_| async { vec![MeshCarrier::Direct, MeshCarrier::Relayed] },
                || async { None },
            )
            .await;
        for p in [
            RcOverlay::resolve_ranked(false, None, RankMode::Off, blocks()),
            RcOverlay::resolve_ranked(true, None, RankMode::Off, blocks()),
            RcOverlay::resolve_ranked(false, Some(&hints), RankMode::Off, blocks()),
            decided,
            RcOverlay::default(),
        ] {
            let now = serde_json::to_string(&p).unwrap();
            assert_eq!(now, serde_json::to_string(&Before::of(&p)).unwrap());
            assert!(
                !now.contains("carrier_rank") && !now.contains("mesh_last"),
                "{now}"
            );
            assert_eq!(serde_json::from_str::<RcOverlay>(&now).unwrap(), p);
        }
        assert_eq!(
            serde_json::to_string(&RcOverlay::resolve_ranked(
                true,
                None,
                RankMode::Off,
                blocks()
            ))
            .unwrap(),
            r#"{"allow":true}"#,
            "`on` at switch 0 carries no blocks, as before"
        );

        let ranked = RcOverlay::resolve_ranked(true, None, RankMode::Cost, blocks());
        let json = serde_json::to_string(&ranked).unwrap();
        assert_eq!(
            json,
            r#"{"allow":true,"v4_nets":[["100.65.4.0",22]],"carrier_rank":"1"}"#
        );
        assert_eq!(serde_json::from_str::<RcOverlay>(&json).unwrap(), ranked);
        let older: Before = serde_json::from_str(&json).unwrap();
        assert!(older.allow && older.v4_nets == blocks(), "{older:?}");
        let prio = RcOverlay {
            mesh_last: true,
            ..RcOverlay::resolve_ranked(false, Some(&hints), RankMode::Prio, blocks())
        };
        let json = serde_json::to_string(&prio).unwrap();
        assert!(
            json.ends_with(r#""carrier_rank":"prio","mesh_last":true}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<RcOverlay>(&json).unwrap(), prio);
    }

    /// Under `on` the server names no viewer device. In rank mode they are
    /// the offer's TURN servers on a mesh address, each once; a STUN server, a
    /// name, and an address outside the blocks (a carrier's CGNAT) are none.
    /// Devices the server named (`auto`) win.
    #[tokio::test]
    async fn under_on_the_viewer_devices_are_the_offers_turn_servers_on_the_mesh() {
        let servers = vec![
            server(&[
                "stun:turn.roomler.ai:3478",
                "turn:turn.roomler.ai:3478?transport=udp",
                "turns:turn.roomler.ai:443?transport=tcp",
            ]),
            server(&[
                "turn:100.65.4.33:47989",
                "turns:100.65.4.33:47990?transport=tcp",
            ]),
            server(&[
                "turn:[fd72:6f6f:6d6c::6441:428]:47989",
                "stun:100.65.4.40:3478",
                "turn:100.80.1.9:3478",
                "turn:203.0.113.7:3478",
            ]),
        ];
        let (d, viewers, link) = ranked_on()
            .rank_with("s", &servers, |addrs: Vec<IpAddr>| async move {
                Some(vec![relayed(); addrs.len()])
            })
            .await;
        assert_eq!(
            viewers,
            vec![ip("100.65.4.33"), ip("fd72:6f6f:6d6c::6441:428")]
        );
        assert_eq!(link, Link::Read(relayed()));
        assert!(d.allow && d.mesh_last, "{d:?}");

        let named = RcOverlay {
            carrier_rank: RankMode::Prio,
            ..auto(&["100.65.4.40"], None)
        };
        let (_, viewers, _) = named
            .rank_with("s", &servers, |addrs: Vec<IpAddr>| async move {
                Some(vec![relayed(); addrs.len()])
            })
            .await;
        assert_eq!(viewers, vec![ip("100.65.4.40")], "named win");

        // No viewer relay at all: no device known, nothing read, ranked last.
        let (d, viewers, link) = ranked_on()
            .rank_with("s", &servers[..1], |_| async {
                panic!("no device, no read")
            })
            .await;
        assert!(viewers.is_empty() && link == Link::NoViewer && d.mesh_last);
    }

    /// Only a direct link keeps today's rank (the mesh host's type 90): every
    /// other answer ranks the mesh last, and so does no answer.
    #[tokio::test]
    async fn only_a_direct_link_keeps_todays_rank() {
        let ranked = |answer: Option<Vec<MeshLink>>| async move {
            ranked_on()
                .rank_with("s", &with_viewer_relay(), move |_| async move { answer })
                .await
                .0
        };
        let direct = ranked(Some(vec![link(Some(C::Direct), false, false)])).await;
        assert!(direct.allow && !direct.mesh_last);
        let rank = direct.ice_rank().expect("rank mode");
        assert!(rank.mesh_last.is_none(), "direct: nothing ranked last");
        for (what, answer) in [
            ("relayed", Some(vec![relayed()])),
            ("a tunnel", Some(vec![link(Some(C::Tunnel), false, false)])),
            ("upgrading", Some(vec![link(Some(C::Relay), true, false)])),
            (
                "direct mid-upgrade",
                Some(vec![link(Some(C::Direct), true, false)]),
            ),
            ("stalled", Some(vec![link(Some(C::Direct), false, true)])),
            ("blocked", Some(vec![link(Some(C::Blocked), false, false)])),
            ("offline", Some(vec![link(Some(C::Offline), false, false)])),
            ("not in the answer", Some(vec![link(None, false, false)])),
            ("no answer within 400 ms", None),
        ] {
            let d = ranked(answer).await;
            assert!(d.allow && d.mesh_last, "{what}: {d:?}");
            let rank = d.ice_rank().expect("rank mode");
            let mesh = rank.mesh_last.expect("ranked last");
            assert!(mesh(ip("100.65.4.34")), "{what}: a mesh address");
            assert!(mesh(ip("fd72:6f6f:6d6c::6441:422")), "{what}: the ULA");
            assert!(!mesh(ip("100.80.1.2")), "{what}: a carrier's CGNAT");
            assert!(!mesh(ip("192.168.1.20")) && !mesh(ip("203.0.113.7")));
        }
        // Two devices: the best link decides.
        let d = ranked(Some(vec![relayed(), link(Some(C::Direct), false, false)])).await;
        assert!(!d.mesh_last);
        let d = ranked(Some(vec![relayed(), link(None, false, false)])).await;
        assert!(d.mesh_last);
    }

    /// In rank mode `auto` behaves as `on` whatever the far-TURN hint: the
    /// mesh is allowed, nothing is excluded, and the blocks stay for the rank.
    #[tokio::test]
    async fn in_rank_mode_auto_is_on_whatever_the_hint() {
        for hint in [Some(true), Some(false), None] {
            let a = RcOverlay {
                carrier_rank: RankMode::Prio,
                ..auto(&["100.65.4.33", "100.65.4.40"], hint)
            };
            let (d, viewers, _) = a
                .rank_with("s", &[], |_| async {
                    Some(vec![link(Some(C::Direct), false, false), relayed()])
                })
                .await;
            assert!(d.allow && d.auto && !d.mesh_last, "{hint:?}: {d:?}");
            assert!(d.exclude.is_empty(), "as `on`: no device left out");
            assert_eq!(d.v4_nets, blocks());
            assert!(d.keeps_local(ip("100.65.4.34")) && d.filters_nothing());
            assert_eq!(viewers.len(), 2);
            let (d, _, _) = RcOverlay {
                carrier_rank: RankMode::Cost,
                ..auto(&["100.65.4.33"], hint)
            }
            .rank_with("s", &[], |_| async { Some(vec![relayed()]) })
            .await;
            assert!(
                d.allow && d.mesh_last,
                "{hint:?}: relayed, and still allowed"
            );
        }
    }

    /// An `off` policy stays off in rank mode: nothing is read, nothing ranked
    /// last, the filters hold; the agent's half of the rank (TLS below UDP,
    /// the checks after the selection) still applies.
    #[tokio::test]
    async fn in_rank_mode_an_off_policy_stays_off_and_reads_nothing() {
        let p = RcOverlay::resolve_ranked(false, None, RankMode::Cost, blocks());
        let (d, viewers, link) = p
            .rank_with("s", &with_viewer_relay(), |_| async {
                panic!("off: no read")
            })
            .await;
        assert!(!d.allow && !d.mesh_last && viewers.is_empty() && link == Link::Off);
        assert!(!d.keeps_local(ip("100.65.4.34")), "filtered as ever");
        assert!(d.ice_rank().unwrap().mesh_last.is_none());
    }

    /// Switch `0`: no rank reaches ICE, and an `on` session's offer reads
    /// nothing.
    #[tokio::test]
    async fn switch_0_hands_ice_nothing_and_its_offer_reads_nothing() {
        for p in [on(), off(), RcOverlay::default()] {
            assert!(p.ice_rank().is_none());
            assert_eq!(p.clone().decide("s", &with_viewer_relay()).await, p);
        }
    }

    /// What `f` logs, as the daemon's log file writes it: the same capture as
    /// `media_delivery`'s cells.
    fn logged(f: impl FnOnce()) -> String {
        let log = roomler_log_capture::Captured::default();
        let subscriber = roomler_log_capture::fmt(log.clone())
            .with_target(false)
            .finish();
        roomler_log_capture::with_default(subscriber, f);
        log.text()
    }

    /// `wants`, in this order, on the one line `out` holds.
    fn in_order(out: &str, wants: &[&str]) {
        let mut rest = out;
        for want in wants {
            let at = rest
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is not in order in {out:?}"));
            rest = &rest[at + want.len()..];
        }
        assert_eq!(out.lines().count(), 1, "{out:?}");
    }

    /// The offer's line, field by field.
    #[test]
    fn the_offer_line_names_its_fields() {
        let d = RcOverlay {
            mesh_last: true,
            ..RcOverlay::resolve_ranked(true, None, RankMode::Cost, blocks())
        };
        let upgrading = Link::Read(link(Some(C::Relay), true, false));
        let out = logged(|| {
            d.log_rank(
                "65a1b2c3d4e5f60718293a4b",
                upgrading,
                (1, ViewersFrom::RelayUrls),
                Duration::from_millis(12),
            )
        });
        in_order(
            &out,
            &[
                " INFO ",
                "rc ICE: carrier rank (FR-148 P4)",
                "session=65a1b2c3d4e5f60718293a4b",
                "carrier_rank=1",
                "link=relay",
                "upgrading=true",
                "stalled=false",
                "viewers=1",
                "viewers_from=relay-urls",
                "read_ms=12",
                "ranked_last=host,srflx,relay",
                "mesh_blocks=100.65.4.0/22,fd72:6f6f:6d6c::/96",
            ],
        );
        let kept = RcOverlay {
            carrier_rank: RankMode::Prio,
            mesh_last: false,
            ..d.clone()
        };
        let direct = Link::Read(link(Some(C::Direct), false, false));
        let out = logged(|| kept.log_rank("s", direct, (2, ViewersFrom::Named), Duration::ZERO));
        in_order(
            &out,
            &[
                "carrier_rank=prio",
                "link=direct",
                "viewers=2",
                "viewers_from=named",
                "read_ms=0",
                "ranked_last=none",
            ],
        );
        for (l, word) in [
            (Link::Off, "link=off"),
            (Link::NoViewer, "link=no-viewer"),
            (Link::NoAnswer, "link=no-answer"),
            (Link::Read(link(None, false, false)), "link=unknown"),
            (
                Link::Read(link(Some(C::Tunnel), false, false)),
                "link=tunnel",
            ),
        ] {
            let out = logged(|| kept.log_rank("s", l, (0, ViewersFrom::None), Duration::ZERO));
            assert!(
                out.contains(word) && out.contains("viewers_from=none"),
                "{out:?}"
            );
        }
    }

    /// The line about 10 s on, field by field.
    #[test]
    fn the_line_10_s_on_names_its_fields() {
        let out = logged(|| {
            log_link_again(
                "65a1b2c3d4e5f60718293a4b",
                Link::Read(relayed()),
                Link::Read(link(Some(C::Direct), false, false)),
                Duration::from_millis(3),
            )
        });
        in_order(
            &out,
            &[
                " INFO ",
                "rc ICE: carrier rank, the mesh link 10 s on (FR-148 P4)",
                "session=65a1b2c3d4e5f60718293a4b",
                "link_at_offer=relay",
                "link=direct",
                "upgrading=false",
                "stalled=false",
                "read_ms=3",
            ],
        );
    }
}
