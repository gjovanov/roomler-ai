// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P9c — the carrier a remote-control session rides, recorded once
//! the session reaches Active (spec §3f).
//!
//! **Record, never decide.** Nothing here chooses a path: the nominated ICE
//! pair is read as webrtc-rs reports it, and — when that pair rides the
//! overlay — the overlay runtime's own carrier for the peer behind the
//! remote address is read from LocalAPI, exactly the lookup the adaptive
//! bitrate probe already makes ([`crate::peer`]'s constrained-transport
//! detection). The recorded value travels as an additive field on
//! `rc:session.stats` and lands beside `remote_sessions.stats`.
//!
//! The rule ([`carrier_of`]) is pure and tested for every overlay relay
//! kind. The cases, in the order they are checked:
//!
//! | The nominated pair | Recorded |
//! |---|---|
//! | the remote address is an overlay address and the overlay carries that peer **direct** | `overlay_direct` |
//! | … and the overlay carries it through a **TURN** allocation | `overlay_turn` |
//! | … through **DERP** | `derp` |
//! | … through an FR-19 **org relay** | `org_relay` |
//! | … and the overlay says something else — tunnel, blocked, offline, a relay kind this build does not know, the peer unknown, LocalAPI not answering | `unknown` — **never direct**: `Carrier::is_direct()` reads any new tier as direct, which is the trap this table exists to avoid |
//! | a `relay` candidate at a loopback or overlay address — the device's own fast TURN (`rc_local_turn`), not the far coturn | `turn_local` |
//! | a `relay` candidate anywhere else — coturn | `turn` |
//! | a `prflx` remote that is the far end's relay ([`RemoteRelay::Signalled`], #3259) | `turn_local` or `turn`, by the two rows above |
//! | host · srflx · prflx on both sides, no prflx shown to be a relay | `direct` |
//! | a candidate type webrtc-rs could not name | `unknown` |
//!
//! A prflx remote is learnt from a check that came from an address no
//! candidate the far end signalled had named yet. When that address is the
//! far end's TURN relay, its candidate can arrive after the check, and the
//! ICE agent keeps the prflx selected beside it. Such a remote counts as the
//! relay only when the agent holds a relay candidate the far end signalled
//! at its exact address, port and transport ([`remote_relay_of`]). A prflx nothing
//! shows to be a relay stays `direct`, since a direct session behind a
//! symmetric NAT learns its peer as prflx too. So does one whose read of the
//! held candidates did not finish (past its budget, or with no ICE agent):
//! that read shows nothing. A read on a task of its own waits
//! [`REMOTE_READ_BUDGET`] at most, and one on a pump's own path waits for
//! nothing. The read decides from what the agent holds at the read, never
//! from timing.
//!
//! ⚠️ Read **after a short settle**, not at the instant of `Connected`:
//! Chrome nominates a relay pair first and renominates to the host pair a
//! second or two later once the mDNS-resolved candidate succeeds, so the
//! very first pair would record `turn` for sessions that spend their life
//! direct. The pair is read once more [`SETTLE`] after the first nomination,
//! and that is what is recorded — once per session; a later path change is
//! not recorded (the bitrate clamp follows it live, this does not; under
//! `route` a line names it, below).
//!
//! The spellings are a wire and storage vocabulary: the server stores what
//! it is sent, the growth freeze groups by it. Never rename one.
//!
//! FR-148 P0 adds one field to the carrier line, `relay_transport`
//! ([`relay_transport_of`]): what carries the agent's own TURN allocation
//! for the nominated pair (`udp`, `tcp`, `tls`), `none` when the agent's
//! local candidate is no relay, and `unknown` when the agent cannot say.
//! It is a log field only: no wire carries it and the server stores
//! nothing of it.
//!
//! #3259 adds one more, `remote_relay` ([`RemoteRelay`]), last on the line:
//! `signalled` when the nominated pair's remote is the far end's relay by a
//! candidate the far end signalled (a `relay` remote, or a `prflx` remote at
//! the exact address, port and transport of one), `unknown` when the agent
//! cannot say (a `prflx` remote whose read did not finish, or a remote whose
//! type webrtc-rs could not name), and `none` otherwise. A field read
//! keys a relayed session on it with the carrier, whatever `remote_typ`
//! says. It is a log field only, as `relay_transport` is.
//!
//! FR-148 P4 step 2, phase B of D49: under `rc_carrier_rank = route` a
//! selection change after the opening writes one line, `session carrier
//! moved` ([`follow_moves`]): the carrier before and after, in this
//! vocabulary, the new pair's `route` as the path line names it
//! ([`crate::carrier_rank::path_route`]), and the new pair. The recorded value
//! stays the one read after the settle, and no wire carries the line. The
//! bitrate clamp is the pumps' own: they re-read the pair every 5 s
//! (relay-escape), with the opening's predicate. Under every other value
//! nothing follows the selection.

use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::Duration;

use bson::oid::ObjectId;
use tracing::{debug, info};
use webrtc::ice::agent::agent_stats::CandidateStats;
use webrtc::ice::url::{ProtoType, SchemeType};
use webrtc::ice_transport::RTCIceTransport;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

use crate::carrier_rank::PathRoute;

/// How long to wait for the first nomination after `Connected` (ICE has
/// usually nominated by then; the DC pumps wait the same 3 s).
pub const NOMINATION_WAIT: Duration = Duration::from_secs(3);
/// The settle before the recorded read (Chrome's relay→direct renomination
/// lands inside this).
pub const SETTLE: Duration = Duration::from_secs(2);
/// The LocalAPI budget for the overlay lookup — the same the bitrate probe
/// affords it.
pub const LOCALAPI_BUDGET: Duration = Duration::from_millis(400);
/// FR-148 P4 step 2, phase B of D49: how often a session under `route` reads
/// its selection again after the opening, for the moved line. A log line's
/// latency, not the media's.
pub const MOVE_TICK: Duration = Duration::from_secs(1);
/// #3259: how long a read of the selected pair on a task of its own waits
/// for the remote candidates the ICE agent holds: [`observe`]'s reads and
/// [`follow`]'s under `route`. A read on a pump's own path waits for nothing
/// ([`Duration::ZERO`]): the deadline lane's `Lane::read`, at the pump's
/// start and at each transport re-check, and the path memory's
/// `model_open`, at the pump's open. The path memory is a shadow, and a
/// shadow's read never puts a wait on the live path. The read takes
/// microseconds, and neither lock it takes is held across I/O once the agent
/// is made. A read that would wait past its budget reads
/// [`RemoteRelay::Unknown`], and the pair's carrier stays `direct`.
pub const REMOTE_READ_BUDGET: Duration = Duration::from_millis(250);

/// What a session rides. The wire spelling is [`SessionCarrier::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionCarrier {
    /// host · srflx · prflx, not over the overlay, and no prflx shown to be
    /// the far end's relay.
    Direct,
    /// A coturn allocation.
    Turn,
    /// The device's own TURN on a loopback or overlay address
    /// (`rc_local_turn`): a path on this host, not the far coturn.
    TurnLocal,
    /// The pair rides the overlay, whose carrier for the peer is direct UDP.
    OverlayDirect,
    /// The pair rides the overlay, carried through a coturn allocation.
    OverlayTurn,
    /// The pair rides the overlay, carried through the DERP floor.
    Derp,
    /// The pair rides the overlay, carried through an FR-19 org relay.
    OrgRelay,
    /// The agent could not say — never read as any of the above.
    Unknown,
}

impl SessionCarrier {
    /// Every value, for a test that must cover each.
    pub const ALL: [SessionCarrier; 8] = [
        SessionCarrier::Direct,
        SessionCarrier::Turn,
        SessionCarrier::TurnLocal,
        SessionCarrier::OverlayDirect,
        SessionCarrier::OverlayTurn,
        SessionCarrier::Derp,
        SessionCarrier::OrgRelay,
        SessionCarrier::Unknown,
    ];

    /// The wire and storage spelling. ⚠️ A compatibility surface: renaming
    /// one does not fail anywhere, it makes every session on that carrier
    /// read as a new, unexplained kind.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionCarrier::Direct => "direct",
            SessionCarrier::Turn => "turn",
            SessionCarrier::TurnLocal => "turn_local",
            SessionCarrier::OverlayDirect => "overlay_direct",
            SessionCarrier::OverlayTurn => "overlay_turn",
            SessionCarrier::Derp => "derp",
            SessionCarrier::OrgRelay => "org_relay",
            SessionCarrier::Unknown => "unknown",
        }
    }
}

/// An ICE candidate's type, as the rule reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Host,
    Srflx,
    Prflx,
    Relay,
    /// webrtc-rs's `Unspecified`: a type it could not name.
    Unspecified,
}

impl From<webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType> for CandidateKind {
    fn from(t: webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType) -> Self {
        use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType as T;
        match t {
            T::Host => CandidateKind::Host,
            T::Srflx => CandidateKind::Srflx,
            T::Prflx => CandidateKind::Prflx,
            T::Relay => CandidateKind::Relay,
            T::Unspecified => CandidateKind::Unspecified,
        }
    }
}

/// The nominated pair, the five facts the rule reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairView {
    pub local_kind: CandidateKind,
    pub remote_kind: CandidateKind,
    pub local_addr: String,
    pub remote_addr: String,
    /// #3259: whether the remote is the far end's relay, by a candidate the
    /// far end signalled ([`remote_relay_of`]).
    pub remote_relay: RemoteRelay,
}

/// #3259 — whether the nominated pair's remote is the far end's relay, by a
/// candidate the far end signalled. The carrier line names it last
/// (`remote_relay`).
///
/// A remote whose type is `relay` is one by construction: the ICE agent
/// gives a remote that type only from a candidate the far end signalled. A
/// `prflx` remote is learnt from a check whose source no signalled candidate
/// had named yet. When that source is the far end's TURN relay, its
/// candidate can come after the check, and the ICE agent keeps the prflx
/// selected beside it. Packets from a TURN allocation's relayed address and
/// port come from that relay alone, so such a remote is the far end's relay
/// exactly when the agent holds a relay candidate the far end signalled at
/// the prflx's address and port, over the prflx's transport.
///
/// | The nominated pair's remote | Recorded |
/// |---|---|
/// | a `relay` candidate | `signalled` |
/// | a `prflx` candidate at the exact address, port and transport of a `relay` candidate the far end signalled | `signalled` |
/// | a `prflx` candidate whose read of the held candidates did not finish: past its budget ([`REMOTE_READ_BUDGET`] on a task of its own, none on a pump's path), or with no ICE agent | `unknown` |
/// | a type webrtc-rs could not name | `unknown` |
/// | anything else: host, srflx, a prflx no signalled relay is at (a direct session behind a symmetric NAT learns its peer this way) | `none` |
///
/// Only a `prflx` remote reads the held candidates; every other remote reads
/// by its type alone. `none` says that no relay candidate the far end
/// signalled is at the remote, so a read that did not finish says `unknown`,
/// never `none`, as [`RelayTransport::Unknown`] does where the agent cannot
/// say. Neither `none` nor `unknown` shows a relay, so the carrier stays
/// what the candidate types give. Held facts, never timing: a relay
/// candidate that arrives later changes the next read, not one before it.
/// The spellings are a field read's vocabulary: never rename one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteRelay {
    /// Nothing the agent holds shows the remote to be the far end's relay.
    NotShown,
    /// The remote is the far end's relay, by a candidate it signalled.
    Signalled,
    /// The agent cannot say: its read did not finish, or the remote's type
    /// has no name.
    Unknown,
}

impl RemoteRelay {
    /// Every value, for a test that must cover each.
    pub const ALL: [RemoteRelay; 3] = [
        RemoteRelay::NotShown,
        RemoteRelay::Signalled,
        RemoteRelay::Unknown,
    ];

    /// The carrier line's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteRelay::NotShown => "none",
            RemoteRelay::Signalled => "signalled",
            RemoteRelay::Unknown => "unknown",
        }
    }
}

/// The transport a candidate names, as the remote relay's rule compares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateTransport {
    Udp,
    Tcp,
}

impl CandidateTransport {
    /// A candidate's protocol as webrtc-rs names it; `None` for neither.
    fn of_protocol(protocol: webrtc::ice_transport::ice_protocol::RTCIceProtocol) -> Option<Self> {
        use webrtc::ice_transport::ice_protocol::RTCIceProtocol as P;
        match protocol {
            P::Udp => Some(CandidateTransport::Udp),
            P::Tcp => Some(CandidateTransport::Tcp),
            P::Unspecified => None,
        }
    }

    /// An ICE network type's transport; `None` for an unspecified one.
    fn of_network(network: webrtc::ice::network_type::NetworkType) -> Option<Self> {
        if network.is_udp() {
            Some(CandidateTransport::Udp)
        } else if network.is_tcp() {
            Some(CandidateTransport::Tcp)
        } else {
            None
        }
    }
}

/// A relay candidate the far end signalled that the ICE agent holds: its
/// relayed address, its port and its transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldRelay {
    pub addr: String,
    pub port: u16,
    pub transport: CandidateTransport,
}

/// The remote relay's rule: the remote's type, address, port and transport
/// (`None` when it names neither), and the relay candidates the far end
/// signalled that the ICE agent holds, `None` when they could not be read
/// (a read past its budget, or no ICE agent). Only a `prflx`
/// remote looks at them. Addresses compare as IP addresses, an IPv4-mapped
/// IPv6 address as its IPv4 one, and an address that is not one matches
/// nothing; ports compare as numbers.
pub fn remote_relay_of(
    remote: CandidateKind,
    addr: &str,
    port: u16,
    transport: Option<CandidateTransport>,
    signalled_relays: Option<&[HeldRelay]>,
) -> RemoteRelay {
    let as_ip = |a: &str| {
        a.parse::<std::net::IpAddr>()
            .ok()
            .map(|ip| ip.to_canonical())
    };
    match remote {
        CandidateKind::Relay => RemoteRelay::Signalled,
        CandidateKind::Prflx => {
            let Some(held) = signalled_relays else {
                return RemoteRelay::Unknown;
            };
            match (as_ip(addr), transport) {
                (Some(at), Some(transport))
                    if held.iter().any(|r| {
                        r.port == port && r.transport == transport && as_ip(&r.addr) == Some(at)
                    }) =>
                {
                    RemoteRelay::Signalled
                }
                _ => RemoteRelay::NotShown,
            }
        }
        CandidateKind::Host | CandidateKind::Srflx => RemoteRelay::NotShown,
        CandidateKind::Unspecified => RemoteRelay::Unknown,
    }
}

/// What the overlay runtime says about the peer behind an overlay address
/// (LocalAPI `peers` → `connection` and `relay_kind`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayPath {
    Direct,
    /// Relayed; the kind as the daemon labels it (`turn` · `derp` · `org`),
    /// or none from a daemon older than the field.
    Relay(Option<String>),
    /// `tunnel` · `blocked` · `offline`: not a carrier for pixels.
    Other(String),
}

/// The rule. `overlay` is what the overlay said about the remote address,
/// `None` when nothing could be learned (the peer unknown, LocalAPI not
/// answering, the daemon not running); it is consulted only for an overlay
/// remote and must never turn into `direct` by default.
pub fn carrier_of(pair: &PairView, overlay: Option<&OverlayPath>) -> SessionCarrier {
    carrier_on(
        pair,
        crate::peer::addr_is_overlay_range(&pair.remote_addr),
        overlay,
    )
}

/// [`carrier_of`], given whether the pair rides the overlay. Under every
/// `rc_carrier_rank` value but `route` that is its remote in the overlay's
/// range ([`carrier_of`]); FR-148 P4 step 2 (D49 A2): under `route` it is the
/// ICE agent's read of the pair (`RTCIceTransport::route_read`), the route
/// rank's one predicate with the mesh's own blocks. Either end on the mesh
/// counts there, and an address of the range outside the blocks does not.
pub fn carrier_on(
    pair: &PairView,
    rides_the_overlay: bool,
    overlay: Option<&OverlayPath>,
) -> SessionCarrier {
    if rides_the_overlay {
        return match overlay {
            Some(OverlayPath::Direct) => SessionCarrier::OverlayDirect,
            Some(OverlayPath::Relay(kind)) => match kind.as_deref() {
                Some("turn") => SessionCarrier::OverlayTurn,
                Some("derp") => SessionCarrier::Derp,
                Some("org") => SessionCarrier::OrgRelay,
                _ => SessionCarrier::Unknown,
            },
            Some(OverlayPath::Other(_)) | None => SessionCarrier::Unknown,
        };
    }
    // #3259: a prflx remote shown to be the far end's relay takes a relay
    // remote's address rule.
    let remote_is_relay = pair.remote_kind == CandidateKind::Relay
        || (pair.remote_kind == CandidateKind::Prflx
            && pair.remote_relay == RemoteRelay::Signalled);
    for (relay, addr) in [
        (
            pair.local_kind == CandidateKind::Relay,
            pair.local_addr.as_str(),
        ),
        (remote_is_relay, pair.remote_addr.as_str()),
    ] {
        if relay {
            return if crate::encode::relay_addr_is_fast_local(addr) {
                SessionCarrier::TurnLocal
            } else {
                SessionCarrier::Turn
            };
        }
    }
    if pair.local_kind == CandidateKind::Unspecified
        || pair.remote_kind == CandidateKind::Unspecified
    {
        return SessionCarrier::Unknown;
    }
    SessionCarrier::Direct
}

/// FR-148 P0 — what carries the agent's own TURN allocation for the
/// nominated pair, which the carrier line names beside the carrier
/// (`relay_transport`). A relay candidate names its relayed address, which
/// is UDP whatever carries the allocation to the TURN server, so the pair's
/// candidates cannot tell TURN over UDP from TURN over TCP or TLS. The
/// vendored webrtc-ice stamps each relay candidate it gathers with its TURN
/// URL's transport and scheme (`Candidate::relay_transport`), and the
/// vendored webrtc hands it over with the pair
/// (`RTCIceCandidatePair::local_relay_transport`).
///
/// | The nominated pair's local candidate | Recorded |
/// |---|---|
/// | a relay allocated over `turn:…?transport=udp` | `udp` |
/// | a relay allocated over `turn:…?transport=tcp` (the fork does not gather one today) | `tcp` |
/// | a relay allocated over `turns:…?transport=tcp` (TLS over TCP, `tcp_turn_conn`) | `tls` |
/// | host · srflx · prflx: no allocation of the agent's carries the pair, though the far end's relay may | `none` |
/// | a relay without a stamp, a URL pair the fork does not gather (TURN over DTLS), or a type webrtc-rs could not name | `unknown`, never a guess |
///
/// The browser's own transport to its TURN server never reaches the agent:
/// a pair whose relay is the browser's reads `none`. The spellings are a
/// field read's vocabulary: never rename one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelayTransport {
    /// TURN over UDP.
    Udp,
    /// TURN over TCP.
    Tcp,
    /// TURN over TLS over TCP.
    Tls,
    /// The agent's local candidate is no relay.
    NoRelay,
    /// The agent could not say.
    Unknown,
}

impl RelayTransport {
    /// Every value, for a test that must cover each.
    pub const ALL: [RelayTransport; 5] = [
        RelayTransport::Udp,
        RelayTransport::Tcp,
        RelayTransport::Tls,
        RelayTransport::NoRelay,
        RelayTransport::Unknown,
    ];

    /// The carrier line's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RelayTransport::Udp => "udp",
            RelayTransport::Tcp => "tcp",
            RelayTransport::Tls => "tls",
            RelayTransport::NoRelay => "none",
            RelayTransport::Unknown => "unknown",
        }
    }
}

/// The relay transport's rule: the local candidate's type, and the stamp
/// its gather left (`None` when there is none).
pub fn relay_transport_of(
    local: CandidateKind,
    stamp: Option<(ProtoType, SchemeType)>,
) -> RelayTransport {
    match local {
        CandidateKind::Host | CandidateKind::Srflx | CandidateKind::Prflx => {
            RelayTransport::NoRelay
        }
        CandidateKind::Unspecified => RelayTransport::Unknown,
        CandidateKind::Relay => match stamp {
            Some((ProtoType::Udp, SchemeType::Turn)) => RelayTransport::Udp,
            Some((ProtoType::Tcp, SchemeType::Turn)) => RelayTransport::Tcp,
            Some((ProtoType::Tcp, SchemeType::Turns)) => RelayTransport::Tls,
            _ => RelayTransport::Unknown,
        },
    }
}

/// The nominated pair as the carrier line reads it: the pair, what carries
/// the agent's relay for it, and, under `route` alone, whether the ICE agent
/// reads it as riding the mesh (`None` under every other value).
pub(crate) type Nominated = (PairView, RelayTransport, Option<bool>);

/// The selected pair as read once: its two candidates, ports included, what
/// carries the agent's relay for it, under `route` alone whether the ICE
/// agent reads it as riding the mesh (`None` under every other value), and
/// whether its remote is the far end's relay (#3259).
#[derive(Debug, Clone, PartialEq)]
struct Selection {
    local: RTCIceCandidate,
    remote: RTCIceCandidate,
    relay: RelayTransport,
    rides: Option<bool>,
    remote_relay: RemoteRelay,
}

impl Selection {
    /// The five facts the carrier rule reads.
    fn view(&self) -> PairView {
        PairView {
            local_kind: self.local.typ.into(),
            remote_kind: self.remote.typ.into(),
            local_addr: self.local.address.clone(),
            remote_addr: self.remote.address.clone(),
            remote_relay: self.remote_relay,
        }
    }

    /// As the carrier line reads it.
    fn nominated(&self) -> Nominated {
        (self.view(), self.relay, self.rides)
    }

    /// Is `other` the same pair: both ends at the same address and port? Two
    /// sockets at one address are two pairs.
    fn same_pair(&self, other: &Selection) -> bool {
        let ends = |s: &Selection| {
            (
                s.local.address.clone(),
                s.local.port,
                s.remote.address.clone(),
                s.remote.port,
            )
        };
        ends(self) == ends(other)
    }
}

/// The selected pair, read once: `None` while nothing is selected. A prflx
/// remote's read of the held candidates waits `budget` at most: 250 ms or
/// nothing, by where it runs ([`REMOTE_READ_BUDGET`]).
async fn selection(ice: &RTCIceTransport, budget: Duration) -> Option<Selection> {
    let pair = ice.get_selected_candidate_pair().await?;
    let (local, remote) = (pair.local().clone(), pair.remote().clone());
    let relay = relay_transport_of(local.typ.into(), pair.local_relay_transport());
    let rides = ice.route_read(&pair).await.map(|read| read.rides_the_mesh);
    // #3259: only a prflx remote reads the relays the far end signalled.
    // Every other remote reads by its type alone, with no read at all.
    let kind = CandidateKind::from(remote.typ);
    let held = if kind == CandidateKind::Prflx {
        signalled_relays(ice, budget).await
    } else {
        None
    };
    let remote_relay = remote_relay_of(
        kind,
        &remote.address,
        remote.port,
        CandidateTransport::of_protocol(remote.protocol),
        held.as_deref(),
    );
    Some(Selection {
        local,
        remote,
        relay,
        rides,
        remote_relay,
    })
}

/// #3259 — the relay candidates the far end signalled that the ICE agent
/// holds, read within `budget`; `None` when the read did not finish within
/// it, or found no ICE agent. The ICE agent lists every remote candidate it
/// holds (`RTCIceTransport::remote_candidates_stats`, a patch of the
/// vendored webrtc): a prflx it learnt, and a relay candidate at the same
/// address beside it. A read that does not finish holds up nothing: the
/// remote reads `unknown`, and the carrier stays `direct`.
async fn signalled_relays(ice: &RTCIceTransport, budget: Duration) -> Option<Vec<HeldRelay>> {
    relays_within(ice.remote_candidates_stats(), budget).await
}

/// [`signalled_relays`] over its read, which a cell drives: the relay
/// candidates in what `read` returns within `budget`, `None` when it returns
/// `None` or does not return within it. A zero budget polls the read once
/// and waits for nothing, so a read that would wait reads `None` at once.
/// That poll runs outside the task's coop budget, so a pump that spent its
/// budget still reads what a free lock holds. Any other budget waits for the
/// read that long at most ([`REMOTE_READ_BUDGET`]).
async fn relays_within(
    read: impl Future<Output = Option<Vec<CandidateStats>>>,
    budget: Duration,
) -> Option<Vec<HeldRelay>> {
    use webrtc::ice::candidate::CandidateType;
    let held = if budget.is_zero() {
        futures::FutureExt::now_or_never(tokio::task::coop::unconstrained(read)).flatten()
    } else {
        tokio::time::timeout(budget, read).await.ok().flatten()
    }?;
    Some(
        held.into_iter()
            .filter(|c| c.candidate_type == CandidateType::Relay)
            .filter_map(|c| {
                Some(HeldRelay {
                    transport: CandidateTransport::of_network(c.network_type)?,
                    addr: c.ip,
                    port: c.port,
                })
            })
            .collect(),
    )
}

/// The selected pair, polled every 100 ms for up to `wait`; a prflx remote's
/// read of the held candidates waits `budget` at most.
async fn nominated(pc: &RTCPeerConnection, wait: Duration, budget: Duration) -> Option<Selection> {
    let sctp = pc.sctp();
    let dtls = sctp.transport();
    let ice = dtls.ice_transport();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Some(selected) = selection(ice, budget).await {
            return Some(selected);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The nominated pair, polled every 100 ms for up to `wait`; a prflx remote's
/// read of the held candidates waits `budget` at most: [`REMOTE_READ_BUDGET`]
/// on a task of its own, [`Duration::ZERO`] on a pump's own path (#3259).
pub(crate) async fn nominated_pair(
    pc: &RTCPeerConnection,
    wait: Duration,
    budget: Duration,
) -> Option<Nominated> {
    nominated(pc, wait, budget).await.map(|s| s.nominated())
}

/// What the overlay says about `remote_addr`'s peer, within
/// [`LOCALAPI_BUDGET`]; `None` when nothing could be learned. Also what the
/// path line names under `route` ([`crate::carrier_rank::log_path`]).
pub(crate) async fn overlay_path(remote_addr: &str) -> Option<OverlayPath> {
    use tunnel_core::localapi::ConnectionType;
    let query = async {
        let mut client = tunnel_core::localapi::connect().await.ok()?;
        let peers = client.peers().await.ok()?;
        peers.into_iter().find(|p| {
            p.overlay_ip.as_deref() == Some(remote_addr)
                || p.overlay_ip6.as_deref() == Some(remote_addr)
        })
    };
    let peer = tokio::time::timeout(LOCALAPI_BUDGET, query).await.ok()??;
    Some(match peer.connection {
        ConnectionType::Direct => OverlayPath::Direct,
        ConnectionType::Relay => OverlayPath::Relay(peer.relay_kind.clone()),
        other => OverlayPath::Other(format!("{other:?}").to_lowercase()),
    })
}

/// Observe the carrier of a session that just reached `Connected`: wait for
/// the nomination, settle, read the pair once more, ask the overlay when the
/// remote is one of its addresses, and record what the rule says. Logged at
/// `info` with the pair, so a field read can check the recorded value
/// against the addresses.
///
/// FR-148 P4 step 2, phase B of D49: under `route` (the agent reads a route
/// for the first pair) it also starts [`follow`] from that pair.
pub async fn observe(
    pc: &Arc<RTCPeerConnection>,
    session_id: bson::oid::ObjectId,
) -> SessionCarrier {
    let Some(first) = nominated(pc, NOMINATION_WAIT, REMOTE_READ_BUDGET).await else {
        debug!(%session_id, "session carrier: no nominated pair within the wait — unknown");
        return SessionCarrier::Unknown;
    };
    if first.rides.is_some() {
        tokio::spawn(follow(Arc::downgrade(pc), session_id, first.clone()));
    }
    tokio::time::sleep(SETTLE).await;
    let (pair, relay, rides) = nominated_pair(pc, Duration::from_millis(500), REMOTE_READ_BUDGET)
        .await
        .unwrap_or_else(|| first.nominated());
    // FR-148 P4 step 2: under `route` the agent's read decides; otherwise the
    // remote's range, as built.
    let rides_the_overlay =
        rides.unwrap_or_else(|| crate::peer::addr_is_overlay_range(&pair.remote_addr));
    let overlay = if rides_the_overlay {
        overlay_path(&pair.remote_addr).await
    } else {
        None
    };
    let carrier = carrier_on(&pair, rides_the_overlay, overlay.as_ref());
    emit(session_id, carrier, &pair, relay, overlay.as_ref());
    carrier
}

/// The carrier line. Its field names and their order are pinned by a test,
/// since a field read greps for them.
fn emit(
    session_id: bson::oid::ObjectId,
    carrier: SessionCarrier,
    pair: &PairView,
    relay: RelayTransport,
    overlay: Option<&OverlayPath>,
) {
    info!(
        %session_id,
        carrier = carrier.as_str(),
        relay_transport = relay.as_str(),
        local_typ = ?pair.local_kind,
        local_addr = %pair.local_addr,
        remote_typ = ?pair.remote_kind,
        remote_addr = %pair.remote_addr,
        overlay = ?overlay,
        remote_relay = pair.remote_relay.as_str(),
        "session carrier recorded (FR-102 P9c)"
    );
}

/// What one of [`follow`]'s reads found.
#[derive(Debug)]
enum Seen {
    /// The session is over: its peer is gone, closed or failed.
    Ended,
    /// Its selection, `None` while nothing is selected. Boxed: a pair is a
    /// few hundred bytes next to the unit variant (clippy
    /// `large_enum_variant`).
    Pair(Option<Box<Selection>>),
}

/// FR-148 P4 step 2, phase B of D49: follow a session under `route` from its
/// first pair, `opening`, reading its selection every [`MOVE_TICK`], until it
/// ends ([`follow_moves`]). It holds the peer weakly, as the observation's
/// spawn does: it never keeps a closed peer alive, and ends with it.
async fn follow(pc: Weak<RTCPeerConnection>, session_id: ObjectId, opening: Selection) {
    let read = || {
        let pc = pc.clone();
        async move {
            let Some(pc) = pc.upgrade() else {
                return Seen::Ended;
            };
            if matches!(
                pc.connection_state(),
                RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
            ) {
                return Seen::Ended;
            }
            let sctp = pc.sctp();
            let dtls = sctp.transport();
            Seen::Pair(
                selection(dtls.ice_transport(), REMOTE_READ_BUDGET)
                    .await
                    .map(Box::new),
            )
        }
    };
    let overlay = |remote_addr: String| async move { overlay_path(&remote_addr).await };
    follow_moves(session_id, opening, read, overlay, MOVE_TICK).await;
}

/// [`follow`]'s loop over its reads, which a cell scripts: every `tick`,
/// `read` the selection. A pair that is not the last one is a move: the
/// carrier it moves from is the last pair's, the one it moves to the new
/// pair's, each by [`carrier_on`] with the agent's read and, for a pair on the
/// mesh, what `overlay` says of its remote's peer at the move; then one
/// `session carrier moved` line ([`emit_moved`]). A read of the same pair
/// off the mesh that shows its remote to be the far end's relay where the
/// last read did not (#3259) writes nothing and becomes the last pair, so a
/// later move's `from` names that relay; a read that says `unknown` changes
/// nothing. It ends when the session
/// does, or when the agent reads no route for a pair, and never starts
/// without one for `opening`. Returns how many moves it named.
async fn follow_moves<R, RF, O, OF>(
    session_id: ObjectId,
    opening: Selection,
    mut read: R,
    mut overlay: O,
    tick: Duration,
) -> usize
where
    R: FnMut() -> RF,
    RF: Future<Output = Seen>,
    O: FnMut(String) -> OF,
    OF: Future<Output = Option<OverlayPath>>,
{
    if opening.rides.is_none() {
        return 0;
    }
    // The last pair, and its carrier once a move has named it.
    let (mut last, mut last_carrier) = (opening, None::<SessionCarrier>);
    let mut moves = 0;
    loop {
        tokio::time::sleep(tick).await;
        let next = match read().await {
            Seen::Ended => return moves,
            Seen::Pair(None) => continue,
            Seen::Pair(Some(next)) => *next,
        };
        if next.same_pair(&last) {
            // #3259: the same pair, read again, can show its remote to be the
            // far end's relay where an earlier read could not (the relay's
            // candidate came after the check). A later move's `from` then
            // names the carrier by the newer read. A read that could not say
            // (`unknown`) knows less than the last one, so it changes
            // nothing. On the mesh the overlay names the carrier, and the
            // newer read changes nothing.
            if next.remote_relay != last.remote_relay
                && next.remote_relay != RemoteRelay::Unknown
                && next.rides == Some(false)
            {
                last_carrier = last_carrier.map(|_| carrier_on(&next.view(), false, None));
                last = next;
            }
            continue;
        }
        let (Some(was_on_the_mesh), Some(on_the_mesh)) = (last.rides, next.rides) else {
            return moves;
        };
        let from = match last_carrier {
            Some(carrier) => carrier,
            None => {
                let said = if was_on_the_mesh {
                    overlay(last.remote.address.clone()).await
                } else {
                    None
                };
                carrier_on(&last.view(), was_on_the_mesh, said.as_ref())
            }
        };
        let said = if on_the_mesh {
            overlay(next.remote.address.clone()).await
        } else {
            None
        };
        let to = carrier_on(&next.view(), on_the_mesh, said.as_ref());
        let route =
            crate::carrier_rank::path_route(on_the_mesh, &next.local, &next.remote, said.as_ref());
        emit_moved(session_id, from, to, route, &next, said.as_ref());
        moves += 1;
        (last, last_carrier) = (next, Some(to));
    }
}

/// The moved line: the carrier before and after, the new pair's route, what
/// carries the agent's relay for it, and the pair. Its field names and their
/// order are pinned by a test, since a field read greps for them.
fn emit_moved(
    session_id: ObjectId,
    from: SessionCarrier,
    to: SessionCarrier,
    route: PathRoute,
    pair: &Selection,
    overlay: Option<&OverlayPath>,
) {
    info!(
        %session_id,
        from = from.as_str(),
        to = to.as_str(),
        route = %route,
        relay_transport = pair.relay.as_str(),
        local_typ = ?pair.local.typ,
        local_addr = %format!("{}:{}", pair.local.address, pair.local.port),
        remote_typ = ?pair.remote.typ,
        remote_addr = %format!("{}:{}", pair.remote.address, pair.remote.port),
        overlay = ?overlay,
        "session carrier moved"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(
        local: CandidateKind,
        local_addr: &str,
        remote: CandidateKind,
        remote_addr: &str,
    ) -> PairView {
        PairView {
            local_kind: local,
            remote_kind: remote,
            local_addr: local_addr.into(),
            remote_addr: remote_addr.into(),
            remote_relay: RemoteRelay::NotShown,
        }
    }

    /// [`pair`], its remote shown to be the far end's relay or not.
    fn pair_with(
        local: CandidateKind,
        local_addr: &str,
        remote: CandidateKind,
        remote_addr: &str,
        remote_relay: RemoteRelay,
    ) -> PairView {
        PairView {
            remote_relay,
            ..pair(local, local_addr, remote, remote_addr)
        }
    }

    /// The spellings are a compatibility surface: locked, one per value,
    /// all distinct.
    #[test]
    fn the_spellings_are_locked_and_distinct() {
        let spellings: Vec<&str> = SessionCarrier::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            spellings,
            [
                "direct",
                "turn",
                "turn_local",
                "overlay_direct",
                "overlay_turn",
                "derp",
                "org_relay",
                "unknown"
            ]
        );
        let distinct: std::collections::HashSet<&str> = spellings.iter().copied().collect();
        assert_eq!(distinct.len(), SessionCarrier::ALL.len());
    }

    /// Every overlay relay kind FR-19's `RelayKind` can name, plus direct,
    /// plus what must never read as direct.
    #[test]
    fn an_overlay_remote_records_the_overlays_own_carrier_and_never_guesses_direct() {
        let p = pair(
            CandidateKind::Host,
            "100.64.0.9",
            CandidateKind::Host,
            "100.64.0.7",
        );
        assert_eq!(
            carrier_of(&p, Some(&OverlayPath::Direct)),
            SessionCarrier::OverlayDirect
        );
        assert_eq!(
            carrier_of(&p, Some(&OverlayPath::Relay(Some("turn".into())))),
            SessionCarrier::OverlayTurn
        );
        assert_eq!(
            carrier_of(&p, Some(&OverlayPath::Relay(Some("derp".into())))),
            SessionCarrier::Derp
        );
        assert_eq!(
            carrier_of(&p, Some(&OverlayPath::Relay(Some("org".into())))),
            SessionCarrier::OrgRelay
        );
        // A relay kind this build does not know, a daemon older than the
        // field, a peer the overlay says is not carried, and nothing learned:
        // each is unknown — the `is_direct()` trap, closed.
        for not_direct in [
            Some(OverlayPath::Relay(Some("quantum".into()))),
            Some(OverlayPath::Relay(None)),
            Some(OverlayPath::Other("tunnel".into())),
            Some(OverlayPath::Other("blocked".into())),
            Some(OverlayPath::Other("offline".into())),
            None,
        ] {
            assert_eq!(
                carrier_of(&p, not_direct.as_ref()),
                SessionCarrier::Unknown,
                "{not_direct:?}"
            );
        }
        // The overlay answer is consulted ONLY for an overlay remote: a
        // public remote with a stray overlay answer is read from the pair.
        let public = pair(
            CandidateKind::Host,
            "100.64.0.9",
            CandidateKind::Srflx,
            "203.0.113.5",
        );
        assert_eq!(
            carrier_of(&public, Some(&OverlayPath::Relay(Some("derp".into())))),
            SessionCarrier::Direct
        );
        // The IPv6 overlay prefix is an overlay remote too.
        let v6 = pair(
            CandidateKind::Host,
            "fd72:6f6f:6d6c::6440:9",
            CandidateKind::Host,
            "fd72:6f6f:6d6c::6440:7",
        );
        assert_eq!(
            carrier_of(&v6, Some(&OverlayPath::Relay(Some("org".into())))),
            SessionCarrier::OrgRelay
        );
    }

    /// A coturn relay on either side is `turn`; the device's own TURN on a
    /// loopback or overlay address is `turn_local`; the plain kinds are
    /// `direct`; a type webrtc-rs could not name is `unknown`.
    #[test]
    fn relay_candidates_and_plain_pairs_read_from_the_pair_itself() {
        assert_eq!(
            carrier_of(
                &pair(
                    CandidateKind::Relay,
                    "5.9.157.221:49152",
                    CandidateKind::Srflx,
                    "203.0.113.5"
                ),
                None
            ),
            SessionCarrier::Turn
        );
        assert_eq!(
            carrier_of(
                &pair(
                    CandidateKind::Host,
                    "192.168.1.9",
                    CandidateKind::Relay,
                    "5.9.157.221"
                ),
                None
            ),
            SessionCarrier::Turn
        );
        assert_eq!(
            carrier_of(
                &pair(
                    CandidateKind::Relay,
                    "127.0.0.1",
                    CandidateKind::Host,
                    "127.0.0.1"
                ),
                None
            ),
            SessionCarrier::TurnLocal
        );
        assert_eq!(
            carrier_of(
                &pair(
                    CandidateKind::Host,
                    "192.168.1.9",
                    CandidateKind::Relay,
                    "::1"
                ),
                None
            ),
            SessionCarrier::TurnLocal
        );
        for (l, r) in [
            (CandidateKind::Host, CandidateKind::Host),
            (CandidateKind::Srflx, CandidateKind::Prflx),
            (CandidateKind::Prflx, CandidateKind::Srflx),
            (CandidateKind::Host, CandidateKind::Srflx),
        ] {
            assert_eq!(
                carrier_of(&pair(l, "192.168.1.9", r, "203.0.113.5"), None),
                SessionCarrier::Direct,
                "{l:?}/{r:?}"
            );
        }
        assert_eq!(
            carrier_of(
                &pair(
                    CandidateKind::Unspecified,
                    "192.168.1.9",
                    CandidateKind::Host,
                    "203.0.113.5"
                ),
                None
            ),
            SessionCarrier::Unknown
        );
        assert_eq!(
            carrier_of(
                &pair(
                    CandidateKind::Host,
                    "192.168.1.9",
                    CandidateKind::Unspecified,
                    "203.0.113.5"
                ),
                None
            ),
            SessionCarrier::Unknown
        );
    }

    /// #3259 — a prflx remote shown to be the far end's relay reads as a
    /// relay remote does: `turn`, or `turn_local` at a loopback or overlay
    /// address. A prflx nothing shows to be a relay stays `direct`, the
    /// field is read for a prflx remote alone, the agent's own relay is read
    /// first as on a relay-to-relay pair, and an overlay remote still takes
    /// the overlay's word.
    #[test]
    fn a_prflx_remote_shown_to_be_the_far_ends_relay_reads_as_that_relay() {
        use CandidateKind::{Host, Prflx, Relay, Srflx};
        use RemoteRelay::{NotShown, Signalled};
        for (p, want) in [
            (
                pair_with(Host, "192.168.1.9", Prflx, "203.0.113.7", Signalled),
                SessionCarrier::Turn,
            ),
            (
                pair_with(Srflx, "198.51.100.4", Prflx, "203.0.113.7", Signalled),
                SessionCarrier::Turn,
            ),
            (
                pair_with(Host, "127.0.0.1", Prflx, "127.0.0.1", Signalled),
                SessionCarrier::TurnLocal,
            ),
            (
                pair_with(Host, "::1", Prflx, "::1", Signalled),
                SessionCarrier::TurnLocal,
            ),
            // A direct session behind a symmetric NAT learns its peer as
            // prflx too: nothing shows a relay, and it stays direct.
            (
                pair_with(Host, "192.168.1.9", Prflx, "203.0.113.7", NotShown),
                SessionCarrier::Direct,
            ),
            // The field is read for a prflx remote alone.
            (
                pair_with(Host, "192.168.1.9", Host, "203.0.113.7", Signalled),
                SessionCarrier::Direct,
            ),
            (
                pair_with(Host, "192.168.1.9", Srflx, "203.0.113.7", Signalled),
                SessionCarrier::Direct,
            ),
            // A relay remote reads by its type, whatever the field says.
            (
                pair_with(Host, "192.168.1.9", Relay, "203.0.113.7", NotShown),
                SessionCarrier::Turn,
            ),
            // The agent's own relay is read first.
            (
                pair_with(Relay, "203.0.113.9", Prflx, "127.0.0.1", Signalled),
                SessionCarrier::Turn,
            ),
            (
                pair_with(Relay, "127.0.0.1", Prflx, "203.0.113.7", Signalled),
                SessionCarrier::TurnLocal,
            ),
        ] {
            assert_eq!(carrier_of(&p, None), want, "{p:?}");
        }
        let mesh = pair_with(Host, "100.64.0.9", Prflx, "100.64.0.7", Signalled);
        assert_eq!(
            carrier_of(&mesh, Some(&OverlayPath::Direct)),
            SessionCarrier::OverlayDirect
        );
        assert_eq!(carrier_of(&mesh, None), SessionCarrier::Unknown);
    }

    /// #3259 — the remote relay's rule: a relay remote is the far end's
    /// relay by construction; a prflx remote is one only at the exact
    /// address, port and transport of a relay candidate the far end
    /// signalled, and is `unknown` when those could not be read; a remote
    /// whose type has no name is `unknown`; nothing else is a relay.
    #[test]
    fn a_remote_is_the_far_ends_relay_only_by_a_candidate_it_signalled() {
        use CandidateKind::{Host, Prflx, Relay, Srflx, Unspecified};
        use CandidateTransport::{Tcp, Udp};
        use RemoteRelay::{NotShown, Signalled, Unknown};
        let relay = |addr: &str, port, transport| HeldRelay {
            addr: addr.to_owned(),
            port,
            transport,
        };
        let held = [
            relay("203.0.113.7", 49170, Udp),
            relay("2001:db8::7", 3479, Udp),
        ];
        let held = Some(&held[..]);
        let none: Option<&[HeldRelay]> = Some(&[]);
        for (kind, addr, port, transport, relays, want) in [
            (Relay, "203.0.113.7", 49170, Some(Udp), none, Signalled),
            (Relay, "198.51.100.9", 7001, None, held, Signalled),
            // A relay remote reads no candidates, so an unread list is no
            // matter to it.
            (Relay, "203.0.113.7", 49170, Some(Udp), None, Signalled),
            (Prflx, "203.0.113.7", 49170, Some(Udp), held, Signalled),
            (Prflx, "2001:db8::7", 3479, Some(Udp), held, Signalled),
            // An IPv4-mapped IPv6 address is its IPv4 one.
            (
                Prflx,
                "::ffff:203.0.113.7",
                49170,
                Some(Udp),
                held,
                Signalled,
            ),
            // A UDP relay's relayed address shows a UDP prflx, never a TCP
            // one, and a prflx that names no transport matches nothing.
            (Prflx, "203.0.113.7", 49170, Some(Tcp), held, NotShown),
            (Prflx, "203.0.113.7", 49170, None, held, NotShown),
            // No relay the far end signalled: a prflx is no relay.
            (Prflx, "203.0.113.7", 49170, Some(Udp), none, NotShown),
            // Another allocation on the same relay server is another relay.
            (Prflx, "203.0.113.7", 49171, Some(Udp), held, NotShown),
            // An address that is no IP address matches nothing.
            (Prflx, "a1b2.local", 49170, Some(Udp), held, NotShown),
            // A read that did not finish knows nothing: never `none`.
            (Prflx, "203.0.113.7", 49170, Some(Udp), None, Unknown),
            (Prflx, "198.51.100.9", 50000, Some(Udp), None, Unknown),
            // Only a prflx is learnt from a check; a host or srflx remote at
            // a relay's address is what the far end said it is, read or not.
            (Host, "203.0.113.7", 49170, Some(Udp), held, NotShown),
            (Srflx, "203.0.113.7", 49170, Some(Udp), held, NotShown),
            (Host, "203.0.113.7", 49170, Some(Udp), None, NotShown),
            (Srflx, "203.0.113.7", 49170, Some(Udp), None, NotShown),
            // A type webrtc-rs could not name: the agent cannot say.
            (Unspecified, "203.0.113.7", 49170, Some(Udp), held, Unknown),
            (Unspecified, "203.0.113.7", 49170, Some(Udp), None, Unknown),
        ] {
            assert_eq!(
                remote_relay_of(kind, addr, port, transport, relays),
                want,
                "{kind:?} {addr}:{port} {transport:?} {relays:?}"
            );
        }
        // A TCP relay candidate shows a TCP prflx.
        let tcp = [relay("203.0.113.7", 49170, Tcp)];
        assert_eq!(
            remote_relay_of(Prflx, "203.0.113.7", 49170, Some(Tcp), Some(&tcp)),
            Signalled
        );
        assert_eq!(
            remote_relay_of(Prflx, "203.0.113.7", 49170, Some(Udp), Some(&tcp)),
            NotShown
        );
    }

    /// #3259 — the transports the rule compares, from webrtc-rs's names: a
    /// candidate's protocol, and the ICE network type a held relay is kept
    /// under.
    #[test]
    fn candidate_transports_read_from_webrtc() {
        use webrtc::ice::network_type::NetworkType as N;
        use webrtc::ice_transport::ice_protocol::RTCIceProtocol as P;
        assert_eq!(
            CandidateTransport::of_protocol(P::Udp),
            Some(CandidateTransport::Udp)
        );
        assert_eq!(
            CandidateTransport::of_protocol(P::Tcp),
            Some(CandidateTransport::Tcp)
        );
        assert_eq!(CandidateTransport::of_protocol(P::Unspecified), None);
        for (n, want) in [
            (N::Udp4, Some(CandidateTransport::Udp)),
            (N::Udp6, Some(CandidateTransport::Udp)),
            (N::Tcp4, Some(CandidateTransport::Tcp)),
            (N::Tcp6, Some(CandidateTransport::Tcp)),
            (N::Unspecified, None),
        ] {
            assert_eq!(CandidateTransport::of_network(n), want, "{n:?}");
        }
    }

    /// #3259 — the remote relay's spellings: locked, one per value, all
    /// distinct.
    #[test]
    fn the_remote_relay_spellings_are_locked_and_distinct() {
        let spellings: Vec<&str> = RemoteRelay::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(spellings, ["none", "signalled", "unknown"]);
        let distinct: std::collections::HashSet<&str> = spellings.iter().copied().collect();
        assert_eq!(distinct.len(), RemoteRelay::ALL.len());
    }

    #[test]
    fn candidate_kinds_map_one_to_one_from_webrtc() {
        use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType as T;
        assert_eq!(CandidateKind::from(T::Host), CandidateKind::Host);
        assert_eq!(CandidateKind::from(T::Srflx), CandidateKind::Srflx);
        assert_eq!(CandidateKind::from(T::Prflx), CandidateKind::Prflx);
        assert_eq!(CandidateKind::from(T::Relay), CandidateKind::Relay);
        assert_eq!(
            CandidateKind::from(T::Unspecified),
            CandidateKind::Unspecified
        );
    }

    /// FR-148 P0 — the relay transport's spellings: locked, one per value,
    /// all distinct.
    #[test]
    fn the_relay_transport_spellings_are_locked_and_distinct() {
        let spellings: Vec<&str> = RelayTransport::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(spellings, ["udp", "tcp", "tls", "none", "unknown"]);
        let distinct: std::collections::HashSet<&str> = spellings.iter().copied().collect();
        assert_eq!(distinct.len(), RelayTransport::ALL.len());
    }

    /// FR-148 P0 — every answer of the relay transport's rule: a relay of the
    /// agent's names its stamp's transport, any other local candidate names
    /// none, and what the agent cannot say is `unknown`, never a guess.
    #[test]
    fn the_relay_transport_reads_the_local_relays_stamp_and_never_guesses() {
        use ProtoType::{Tcp, Udp};
        use SchemeType::{Stun, Turn, Turns};
        let relay = CandidateKind::Relay;
        assert_eq!(
            relay_transport_of(relay, Some((Udp, Turn))),
            RelayTransport::Udp
        );
        assert_eq!(
            relay_transport_of(relay, Some((Tcp, Turn))),
            RelayTransport::Tcp
        );
        assert_eq!(
            relay_transport_of(relay, Some((Tcp, Turns))),
            RelayTransport::Tls
        );
        // No allocation of the agent's carries the pair, whatever the far
        // end's candidate is, and whatever a stamp says.
        for kind in [
            CandidateKind::Host,
            CandidateKind::Srflx,
            CandidateKind::Prflx,
        ] {
            for stamp in [None, Some((Udp, Turn)), Some((Tcp, Turns))] {
                assert_eq!(
                    relay_transport_of(kind, stamp),
                    RelayTransport::NoRelay,
                    "{kind:?} {stamp:?}"
                );
            }
        }
        // A relay without a stamp, a pair the fork gathers no relay over
        // (TURN over DTLS, a STUN URL, a part it could not parse), and a
        // type webrtc-rs could not name.
        for stamp in [
            None,
            Some((Udp, Turns)),
            Some((Udp, Stun)),
            Some((ProtoType::Unknown, Turn)),
            Some((Tcp, SchemeType::Unknown)),
        ] {
            assert_eq!(
                relay_transport_of(relay, stamp),
                RelayTransport::Unknown,
                "{stamp:?}"
            );
        }
        for stamp in [None, Some((Udp, Turn))] {
            assert_eq!(
                relay_transport_of(CandidateKind::Unspecified, stamp),
                RelayTransport::Unknown,
                "{stamp:?}"
            );
        }
    }

    /// What `f` logs, as the daemon's log file writes it (`roomler_node_core::
    /// logging::init`'s file layer: time, level, message, fields; no target).
    fn logged(f: impl FnOnce()) -> String {
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let writer = buf.clone();
        // Two dispatchers, never one: with one, a callsite's interest is taken
        // once for every thread.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
    }

    /// FR-148 P0 — the carrier line's field names and their order, which a
    /// field read greps for (`scripts/rc-path-report.cjs`): the relay
    /// transport follows the carrier, and #3259's remote relay comes last.
    #[test]
    fn the_carrier_line_names_its_fields() {
        let sid = bson::oid::ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        let p = pair(
            CandidateKind::Relay,
            "203.0.113.9",
            CandidateKind::Srflx,
            "198.51.100.5",
        );
        let out = logged(|| emit(sid, SessionCarrier::Turn, &p, RelayTransport::Tls, None));
        let mut rest = out.as_str();
        for want in [
            " INFO ",
            "session carrier recorded (FR-102 P9c)",
            "session_id=65a1b2c3d4e5f60718293a4b",
            "carrier=\"turn\"",
            "relay_transport=\"tls\"",
            "local_typ=Relay",
            "local_addr=203.0.113.9",
            "remote_typ=Srflx",
            "remote_addr=198.51.100.5",
            "overlay=None",
            "remote_relay=\"none\"",
        ] {
            let at = rest
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is not in order in {out:?}"));
            rest = &rest[at + want.len()..];
        }
        assert_eq!(out.lines().count(), 1, "{out:?}");
        // #3259: a prflx remote shown to be the far end's relay keeps its
        // `remote_typ` and says why it is read as one.
        let p = pair_with(
            CandidateKind::Host,
            "192.0.2.10",
            CandidateKind::Prflx,
            "198.51.100.5",
            RemoteRelay::Signalled,
        );
        let out = logged(|| emit(sid, carrier_of(&p, None), &p, RelayTransport::NoRelay, None));
        let mut rest = out.as_str();
        for want in [
            "carrier=\"turn\"",
            "relay_transport=\"none\"",
            "local_typ=Host",
            "remote_typ=Prflx",
            "remote_addr=198.51.100.5",
            "remote_relay=\"signalled\"",
        ] {
            let at = rest
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is not in order in {out:?}"));
            rest = &rest[at + want.len()..];
        }
    }

    /// The credentials the loopback TURN server takes.
    struct Auth;

    impl webrtc::turn::auth::AuthHandler for Auth {
        fn auth_handle(
            &self,
            _username: &str,
            _realm: &str,
            _src_addr: std::net::SocketAddr,
        ) -> Result<Vec<u8>, webrtc::turn::Error> {
            Ok(webrtc::turn::auth::generate_auth_key(
                "user",
                "roomler.test",
                "pass",
            ))
        }
    }

    /// A peer that gathers relay candidates only, from the TURN server at
    /// `port` on loopback, over UDP.
    async fn relayed_peer(port: u16) -> std::sync::Arc<RTCPeerConnection> {
        use webrtc::ice_transport::ice_server::RTCIceServer;
        use webrtc::peer_connection::configuration::RTCConfiguration;
        use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec![format!("turn:127.0.0.1:{port}?transport=udp")],
                username: "user".to_owned(),
                credential: "pass".to_owned(),
            }],
            ice_transport_policy: RTCIceTransportPolicy::Relay,
            ..Default::default()
        };
        let api = webrtc::api::APIBuilder::new().build();
        std::sync::Arc::new(api.new_peer_connection(config).await.unwrap())
    }

    /// A TURN server on loopback over UDP, which hands out its relays on
    /// loopback, and its port.
    async fn loopback_turn() -> (webrtc::turn::server::Server, u16) {
        use webrtc::turn;
        let listener =
            std::sync::Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let port = listener.local_addr().unwrap().port();
        let server = turn::server::Server::new(turn::server::config::ServerConfig {
            realm: "roomler.test".to_owned(),
            auth_handler: std::sync::Arc::new(Auth),
            conn_configs: vec![turn::server::config::ConnConfig {
                conn: listener,
                relay_addr_generator: Box::new(
                    turn::relay::relay_none::RelayAddressGeneratorNone {
                        address: "127.0.0.1".to_owned(),
                        net: std::sync::Arc::new(webrtc::util::vnet::net::Net::new(None)),
                    },
                ),
            }],
            channel_bind_timeout: Duration::from_secs(0),
            alloc_close_notify: None,
        })
        .await
        .unwrap();
        (server, port)
    }

    /// FR-148 P0 — through the vendored webrtc-ice and webrtc: a session on
    /// the agent's own TURN allocation over UDP reads `udp`, from the pair
    /// the carrier line reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pair_on_the_agents_udp_relay_names_its_transport() {
        use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
        let (server, port) = loopback_turn().await;
        let viewer = relayed_peer(port).await;
        let agent = relayed_peer(port).await;
        let (tx, mut connected) = tokio::sync::mpsc::channel::<()>(1);
        agent.on_peer_connection_state_change(Box::new(move |s| {
            if s == RTCPeerConnectionState::Connected {
                let _ = tx.try_send(());
            }
            Box::pin(async {})
        }));
        // The viewer offers with a data channel, as the browser does, and
        // the agent answers; each with every candidate gathered.
        let _channel = viewer.create_data_channel("video", None).await.unwrap();
        let offer = viewer.create_offer(None).await.unwrap();
        let mut gathered = viewer.gathering_complete_promise().await;
        viewer.set_local_description(offer).await.unwrap();
        let _ = gathered.recv().await;
        let offer = viewer.local_description().await.unwrap();
        agent.set_remote_description(offer).await.unwrap();
        let answer = agent.create_answer(None).await.unwrap();
        let mut gathered = agent.gathering_complete_promise().await;
        agent.set_local_description(answer).await.unwrap();
        let _ = gathered.recv().await;
        let answer = agent.local_description().await.unwrap();
        viewer.set_remote_description(answer).await.unwrap();
        let up = tokio::time::timeout(Duration::from_secs(15), connected.recv()).await;
        let read = nominated_pair(&agent, Duration::from_secs(5), REMOTE_READ_BUDGET).await;
        // A peer must be closed: dropping one frees none of its sockets.
        for pc in [&agent, &viewer] {
            let _ = pc.close().await;
        }
        let _ = server.close().await;
        assert!(up.is_ok(), "the relayed pair did not connect in 15 s");
        let (view, relay, rides) = read.expect("no nominated pair");
        assert_eq!(view.local_kind, CandidateKind::Relay, "{view:?}");
        assert_eq!(relay, RelayTransport::Udp, "{view:?}");
        assert_eq!(rides, None, "no rank: the agent reads no route");
        // #3259: a relay remote is the far end's relay by construction.
        assert_eq!(view.remote_kind, CandidateKind::Relay, "{view:?}");
        assert_eq!(view.remote_relay, RemoteRelay::Signalled, "{view:?}");
    }

    /// A peer that gathers host candidates on loopback alone and asks no
    /// server: the agent's end of a pair whose far end is relayed.
    async fn host_peer() -> std::sync::Arc<RTCPeerConnection> {
        use webrtc::api::setting_engine::SettingEngine;
        use webrtc::ice::mdns::MulticastDnsMode;
        use webrtc::ice::network_type::NetworkType;
        let mut setting = SettingEngine::default();
        setting.set_include_loopback_candidate(true);
        setting.set_ip_filter(Box::new(|ip: std::net::IpAddr| ip.is_loopback()));
        setting.set_network_types(vec![NetworkType::Udp4]);
        setting.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        let api = webrtc::api::APIBuilder::new()
            .with_setting_engine(setting)
            .build();
        std::sync::Arc::new(api.new_peer_connection(Default::default()).await.unwrap())
    }

    /// #3259 — a read of the held candidates that cannot finish says
    /// `unknown`, never `none`. With no ICE agent, before one is made and
    /// after close, the accessor reads nothing and makes no agent, so a prflx
    /// remote reads `unknown`, and the pair's carrier stays `direct`. A read
    /// that finishes and finds no relay candidate says `none`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_with_no_ice_agent_reads_unknown_and_the_carrier_stays_direct() {
        use CandidateKind::{Host, Prflx};
        let agent = host_peer().await;
        let ice = agent.sctp().transport().ice_transport().clone();
        let at_a_prflx = |held: Option<Vec<HeldRelay>>| {
            remote_relay_of(
                Prflx,
                "203.0.113.7",
                49170,
                Some(CandidateTransport::Udp),
                held.as_deref(),
            )
        };
        // No agent yet: nothing is read, and the read makes no agent.
        assert!(ice.remote_candidates_stats().await.is_none());
        assert_eq!(signalled_relays(&ice, REMOTE_READ_BUDGET).await, None);
        assert_eq!(
            at_a_prflx(signalled_relays(&ice, REMOTE_READ_BUDGET).await),
            RemoteRelay::Unknown
        );
        // The offer makes the agent: the read finishes and holds no relay.
        agent.create_data_channel("carrier", None).await.unwrap();
        let offer = agent.create_offer(None).await.unwrap();
        agent.set_local_description(offer).await.unwrap();
        let held = signalled_relays(&ice, REMOTE_READ_BUDGET).await;
        assert_eq!(held, Some(Vec::new()));
        assert_eq!(at_a_prflx(held), RemoteRelay::NotShown);
        // After close the agent is gone.
        agent.close().await.unwrap();
        assert!(ice.remote_candidates_stats().await.is_none());
        let gone = at_a_prflx(signalled_relays(&ice, REMOTE_READ_BUDGET).await);
        assert_eq!(gone, RemoteRelay::Unknown);
        // `unknown` shows no relay: the label is what the types give.
        let view = pair_with(Host, "192.168.1.10", Prflx, "203.0.113.7", gone);
        assert_eq!(carrier_of(&view, None), SessionCarrier::Direct);
        assert_eq!(carrier_on(&view, false, None), SessionCarrier::Direct);
    }

    /// #3259 — the read's wait. A read on a pump's own path (`Lane::read`,
    /// `model_open`) waits for nothing, and one on a task of its own
    /// (`observe`, `follow`) waits [`REMOTE_READ_BUDGET`] at most. With the
    /// lock the read takes free, either read answers at once, and the
    /// zero-wait read does in a task that spent its coop budget too. With the
    /// lock held, the zero-wait read returns at once and the bounded read
    /// after its bound, and each reads `unknown`, so the carrier stays
    /// `direct`.
    #[tokio::test(start_paused = true)]
    async fn a_read_that_would_wait_reads_unknown_at_once_on_a_pumps_path() {
        use CandidateKind::{Host, Prflx};
        use webrtc::ice::candidate::CandidateType;
        use webrtc::ice::network_type::NetworkType;
        /// What the ICE agent's list gives, behind a lock as its own is.
        async fn read(
            candidates: &tokio::sync::Mutex<Vec<CandidateStats>>,
        ) -> Option<Vec<CandidateStats>> {
            Some(candidates.lock().await.clone())
        }
        let candidates = tokio::sync::Mutex::new(vec![CandidateStats {
            network_type: NetworkType::Udp4,
            ip: "203.0.113.7".to_owned(),
            port: 49170,
            candidate_type: CandidateType::Relay,
            ..Default::default()
        }]);
        let at_a_prflx = |held: Option<Vec<HeldRelay>>| {
            remote_relay_of(
                Prflx,
                "203.0.113.7",
                49170,
                Some(CandidateTransport::Udp),
                held.as_deref(),
            )
        };
        // The lock is free: either read answers, and neither waits.
        for budget in [Duration::ZERO, REMOTE_READ_BUDGET] {
            let start = tokio::time::Instant::now();
            let held = relays_within(read(&candidates), budget).await;
            assert_eq!(start.elapsed(), Duration::ZERO, "{budget:?}");
            assert_eq!(at_a_prflx(held), RemoteRelay::Signalled, "{budget:?}");
        }
        // A pump that spent its coop budget: the zero-wait read polls outside
        // it, so a free lock still answers.
        for _ in 0..1024 {
            if !tokio::task::coop::has_budget_remaining() {
                break;
            }
            let _ = futures::FutureExt::now_or_never(tokio::task::coop::consume_budget());
        }
        assert!(!tokio::task::coop::has_budget_remaining(), "not spent");
        let spent = relays_within(read(&candidates), Duration::ZERO).await;
        assert_eq!(at_a_prflx(spent), RemoteRelay::Signalled);
        // The lock is held: the read would wait.
        let _held = candidates.lock().await;
        let start = tokio::time::Instant::now();
        let zero = relays_within(read(&candidates), Duration::ZERO).await;
        assert_eq!(start.elapsed(), Duration::ZERO, "the pump's read waited");
        assert_eq!(zero, None);
        let start = tokio::time::Instant::now();
        let bounded = relays_within(read(&candidates), REMOTE_READ_BUDGET).await;
        let waited = start.elapsed();
        assert!(
            (REMOTE_READ_BUDGET..REMOTE_READ_BUDGET + Duration::from_millis(2)).contains(&waited),
            "{waited:?}"
        );
        assert_eq!(bounded, None);
        for unread in [zero, bounded] {
            let unknown = at_a_prflx(unread);
            assert_eq!(unknown, RemoteRelay::Unknown);
            let view = pair_with(Host, "192.168.1.10", Prflx, "203.0.113.7", unknown);
            assert_eq!(carrier_of(&view, None), SessionCarrier::Direct);
        }
    }

    /// An SDP without its candidates, and its candidate lines as a trickle
    /// sends them (`candidate:…`).
    fn without_candidates(sdp: &str) -> (String, Vec<String>) {
        let (mut kept, mut candidates) = (String::new(), Vec::new());
        for line in sdp.split_inclusive('\n') {
            match line.trim_end().strip_prefix("a=") {
                Some(c) if c.starts_with("candidate:") => candidates.push(c.to_owned()),
                Some("end-of-candidates") => {}
                _ => kept.push_str(line),
            }
        }
        (kept, candidates)
    }

    /// #3259 — a relay learnt as peer-reflexive. The viewer's first check
    /// reaches the agent through the viewer's relay before the agent holds
    /// the relay's candidate, so webrtc-ice names the remote `Prflx`. The
    /// candidate the viewer then trickles is kept beside it (`equal` compares
    /// the type) and the selection keeps the prflx (`find_remote_candidate`
    /// returns the first match): the agent holds a relay at exactly the
    /// selected remote's address and port. The carrier reads the far end's
    /// relay, `turn_local` on this loopback relay by the address rule a
    /// signalled relay remote takes, and FR-148's two readers follow the
    /// label: `class_of` gives `unknown`, since the agent's own candidate is
    /// no relay, and the path memory keys the pair by the relay's word.
    /// Before the candidate came nothing showed a relay, and the same pair
    /// read `direct`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_relay_learnt_as_prflx_before_its_candidate_reads_as_the_relay() {
        use crate::media_deadline::{CarrierClass, class_of};
        use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
        use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
        use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
        let (server, port) = loopback_turn().await;
        let viewer = relayed_peer(port).await;
        let agent = host_peer().await;
        let (tx, mut connected) = tokio::sync::mpsc::channel::<()>(1);
        agent.on_peer_connection_state_change(Box::new(move |s| {
            if s == RTCPeerConnectionState::Connected {
                let _ = tx.try_send(());
            }
            Box::pin(async {})
        }));
        let _channel = viewer.create_data_channel("video", None).await.unwrap();
        let offer = viewer.create_offer(None).await.unwrap();
        let mut gathered = viewer.gathering_complete_promise().await;
        viewer.set_local_description(offer).await.unwrap();
        let _ = gathered.recv().await;
        let offer = viewer.local_description().await.unwrap();
        // The viewer's candidates are held back: they reach the agent only
        // once the pair is up, after the check that taught it the prflx.
        let (offer, relays) = without_candidates(&offer.sdp);
        agent
            .set_remote_description(RTCSessionDescription::offer(offer).unwrap())
            .await
            .unwrap();
        let answer = agent.create_answer(None).await.unwrap();
        let mut gathered = agent.gathering_complete_promise().await;
        agent.set_local_description(answer).await.unwrap();
        let _ = gathered.recv().await;
        let answer = agent.local_description().await.unwrap();
        viewer.set_remote_description(answer).await.unwrap();
        let up = tokio::time::timeout(Duration::from_secs(15), connected.recv()).await;
        let learnt = nominated_pair(&agent, Duration::from_secs(5), REMOTE_READ_BUDGET).await;
        let mut trickled = Vec::new();
        for candidate in &relays {
            let init = RTCIceCandidateInit {
                candidate: candidate.clone(),
                ..Default::default()
            };
            trickled.push(
                agent
                    .add_ice_candidate(init)
                    .await
                    .map_err(|e| e.to_string()),
            );
        }
        // The ICE agent adds a trickled candidate on a task of its own: read
        // the pair until its carrier is no longer direct, 3 s at most.
        let until = tokio::time::Instant::now() + Duration::from_secs(3);
        let read = loop {
            let read = nominated_pair(&agent, Duration::from_secs(1), REMOTE_READ_BUDGET).await;
            let moved = read
                .as_ref()
                .is_some_and(|(view, _, _)| carrier_of(view, None) != SessionCarrier::Direct);
            if moved || tokio::time::Instant::now() >= until {
                break read;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        // A peer must be closed: dropping one frees none of its sockets.
        for pc in [&agent, &viewer] {
            let _ = pc.close().await;
        }
        let _ = server.close().await;
        assert!(up.is_ok(), "the relayed pair did not connect in 15 s");
        assert!(!relays.is_empty(), "the viewer gathered no relay candidate");
        assert!(trickled.iter().all(Result::is_ok), "{trickled:?}");
        let (learnt, _, _) = learnt.expect("no nominated pair before the trickle");
        assert_eq!(
            (learnt.local_kind, learnt.remote_kind),
            (CandidateKind::Host, CandidateKind::Prflx),
            "{learnt:?}"
        );
        assert_eq!(learnt.remote_relay, RemoteRelay::NotShown, "{learnt:?}");
        assert_eq!(
            carrier_of(&learnt, None),
            SessionCarrier::Direct,
            "{learnt:?}"
        );
        let (view, relay, rides) = read.expect("no nominated pair after the trickle");
        assert_eq!(
            (view.local_kind, view.remote_kind),
            (CandidateKind::Host, CandidateKind::Prflx),
            "{view:?}"
        );
        assert_eq!(view.remote_relay, RemoteRelay::Signalled, "{view:?}");
        assert_eq!(relay, RelayTransport::NoRelay, "{view:?}");
        assert_eq!(rides, None, "no rank: the agent reads no route");
        assert_eq!(
            carrier_of(&view, None),
            SessionCarrier::TurnLocal,
            "{view:?}"
        );
        assert_eq!(
            class_of(&view, relay, false, None),
            CarrierClass::Unknown,
            "{view:?}"
        );
        assert_eq!(
            crate::encode::pair_memory::carrier_word(carrier_of(&view, None), None).as_deref(),
            Some("turn_local"),
            "{view:?}"
        );
    }

    /// FR-148 P4 step 2 (A2): under `route` the carrier record reads the ICE
    /// agent's predicate, the one the follow gate and the opening read, with
    /// the mesh's own blocks. It agrees with that predicate on the field's two
    /// shapes, the far TURN, a native pair and a direct mesh pair, as the range
    /// rule does; the two part only where D49 says they must.
    #[test]
    fn under_route_the_carrier_record_agrees_with_the_route_ranks_predicate() {
        use CandidateKind::{Host, Relay, Srflx};
        use webrtc::ice::agent::agent_config::rides_the_mesh;
        let route = crate::rc_overlay::RcOverlay {
            allow: true,
            carrier_rank: crate::rc_overlay::RankMode::Route,
            mesh_last: true,
            v4_nets: vec![(std::net::Ipv4Addr::new(100, 65, 4, 0), 22)],
            ..Default::default()
        }
        .ice_rank()
        .and_then(|rank| rank.route)
        .expect("the route rank");
        // Priorities as the agent's candidates carry them.
        const HOST: u32 = 2_130_706_431;
        const TYPE_90: u32 = (1 << 24) * 90 + (1 << 8) * 65535 + 255;
        const UDP_RELAY: u32 = (1 << 8) * 65535 + 255;
        let derp = OverlayPath::Relay(Some("derp".to_owned()));
        let direct = OverlayPath::Direct;
        for (name, (local, local_addr, priority), (remote, remote_addr), overlay, want) in [
            // The field's two shapes: the viewer's relay on its device's mesh
            // address, with the agent's LAN host (A) or its mesh host (B).
            (
                "shape A",
                (Host, "192.168.1.10", HOST),
                (Relay, "100.65.4.33"),
                Some(&derp),
                SessionCarrier::Derp,
            ),
            (
                "shape B",
                (Host, "100.65.4.34", 255),
                (Relay, "100.65.4.33"),
                Some(&derp),
                SessionCarrier::Derp,
            ),
            (
                "far TURN",
                (Relay, "203.0.113.7", UDP_RELAY),
                (Srflx, "198.51.100.4"),
                None,
                SessionCarrier::Turn,
            ),
            (
                "native",
                (Host, "192.168.1.10", HOST),
                (Host, "192.168.1.20"),
                None,
                SessionCarrier::Direct,
            ),
            (
                "direct mesh",
                (Host, "100.65.4.34", TYPE_90),
                (Host, "100.65.4.33"),
                Some(&direct),
                SessionCarrier::OverlayDirect,
            ),
        ] {
            let p = pair(local, local_addr, remote, remote_addr);
            let rides = rides_the_mesh(&*route.mesh, priority, 1, local_addr, remote_addr);
            let carrier = carrier_on(&p, rides, overlay);
            assert_eq!(carrier, want, "{name}");
            assert_eq!(
                carrier_of(&p, overlay),
                want,
                "{name}: the range rule agrees"
            );
            let on_the_overlay = matches!(
                carrier,
                SessionCarrier::OverlayDirect
                    | SessionCarrier::OverlayTurn
                    | SessionCarrier::Derp
                    | SessionCarrier::OrgRelay
            );
            assert_eq!(on_the_overlay, rides, "{name}");
        }
        // Where they part. A remote in the /10 outside the mesh's blocks (a
        // carrier's CGNAT, a corporate VPN) is a real path under `route`.
        assert!(!rides_the_mesh(
            &*route.mesh,
            UDP_RELAY,
            1,
            "203.0.113.7",
            "100.80.1.2"
        ));
        let cgnat = pair(Relay, "203.0.113.7", Srflx, "100.80.1.2");
        assert_eq!(carrier_on(&cgnat, false, None), SessionCarrier::Turn);
        assert_eq!(
            carrier_of(&cgnat, None),
            SessionCarrier::Unknown,
            "as built"
        );
        // A pair whose local the mesh carries rides it, whatever its remote,
        // and the overlay cannot name a carrier for a remote that is no peer.
        assert!(rides_the_mesh(
            &*route.mesh,
            255,
            1,
            "100.65.4.34",
            "198.51.100.4"
        ));
        let from_the_mesh = pair(Host, "100.65.4.34", Srflx, "198.51.100.4");
        assert_eq!(
            carrier_on(&from_the_mesh, true, None),
            SessionCarrier::Unknown,
            "never direct"
        );
        assert_eq!(
            carrier_of(&from_the_mesh, None),
            SessionCarrier::Direct,
            "as built"
        );
    }

    // FR-148 P4 step 2, phase B of D49: the moved line.

    use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType as T;

    /// A pair as the follower reads it.
    fn selected(
        (local_typ, local_addr, local_port): (T, &str, u16),
        (remote_typ, remote_addr, remote_port): (T, &str, u16),
        relay: RelayTransport,
        rides: Option<bool>,
    ) -> Selection {
        let candidate = |typ, address: &str, port| RTCIceCandidate {
            typ,
            address: address.to_owned(),
            port,
            ..Default::default()
        };
        Selection {
            local: candidate(local_typ, local_addr, local_port),
            remote: candidate(remote_typ, remote_addr, remote_port),
            relay,
            rides,
            // As read with no relay candidate held: a relay remote only.
            remote_relay: remote_relay_of(
                remote_typ.into(),
                remote_addr,
                remote_port,
                None,
                Some(&[]),
            ),
        }
    }

    /// The field's pairs, under `route`: the agent's host candidate on its
    /// mesh address with the viewer's mesh address, learned from the viewer's
    /// check; the far TURN; a native pair.
    fn on_the_mesh() -> Selection {
        let (mesh_host, viewer_mesh) = (
            (T::Host, "100.65.4.34", 5001),
            (T::Prflx, "100.65.4.33", 40000),
        );
        selected(mesh_host, viewer_mesh, RelayTransport::NoRelay, Some(true))
    }
    fn on_the_far_turn() -> Selection {
        let (agent_relay, viewer_relay) = (
            (T::Relay, "203.0.113.7", 50000),
            (T::Relay, "198.51.100.9", 7001),
        );
        selected(agent_relay, viewer_relay, RelayTransport::Tls, Some(false))
    }
    fn native() -> Selection {
        let (lan, viewer_lan) = (
            (T::Host, "192.168.1.10", 5000),
            (T::Host, "192.168.1.20", 6000),
        );
        selected(lan, viewer_lan, RelayTransport::NoRelay, Some(false))
    }

    /// A read that finds `s` selected.
    fn seen(s: Selection) -> Seen {
        Seen::Pair(Some(Box::new(s)))
    }

    /// The viewer device's carrier, as the overlay names it: relayed through
    /// DERP. Nothing for an address that is no peer's.
    fn overlay_says_derp(addr: String) -> std::future::Ready<Option<OverlayPath>> {
        std::future::ready(
            (addr == "100.65.4.33").then(|| OverlayPath::Relay(Some("derp".to_owned()))),
        )
    }

    /// [`follow_moves`] over scripted reads, on a runtime of its own, with
    /// what it logged. The script ends the session when it runs out.
    fn followed(opening: Selection, script: Vec<Seen>) -> (usize, usize, String) {
        let sid = bson::oid::ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        let reads = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            script,
        )));
        let made = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let read = {
            let (reads, made) = (reads.clone(), made.clone());
            move || {
                made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let next = reads.lock().unwrap().pop_front().unwrap_or(Seen::Ended);
                std::future::ready(next)
            }
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let mut moves = 0;
        let out = logged(|| {
            moves = runtime.block_on(follow_moves(
                sid,
                opening,
                read,
                overlay_says_derp,
                Duration::from_millis(1),
            ));
        });
        (moves, made.load(std::sync::atomic::Ordering::SeqCst), out)
    }

    /// The moved line's field names and their order, which a field read
    /// greps for: the carrier before and after, the new pair's route, then
    /// the carrier line's own fields, with the pair's ports.
    #[test]
    fn the_moved_line_names_its_fields() {
        let sid = bson::oid::ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        let out = logged(|| {
            emit_moved(
                sid,
                SessionCarrier::Derp,
                SessionCarrier::Turn,
                PathRoute::Turn,
                &on_the_far_turn(),
                None,
            )
        });
        let mut rest = out.as_str();
        for want in [
            " INFO ",
            "session carrier moved",
            "session_id=65a1b2c3d4e5f60718293a4b",
            "from=\"derp\"",
            "to=\"turn\"",
            "route=turn",
            "relay_transport=\"tls\"",
            "local_typ=Relay",
            "local_addr=203.0.113.7:50000",
            "remote_typ=Relay",
            "remote_addr=198.51.100.9:7001",
            "overlay=None",
        ] {
            let at = rest
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is not in order in {out:?}"));
            rest = &rest[at + want.len()..];
        }
        assert_eq!(out.lines().count(), 1, "{out:?}");
    }

    /// Under `route`, each selection change after the opening writes one
    /// moved line, from the last pair's carrier to the new pair's, and a read
    /// of the same pair, or of none, writes nothing. A move back onto the mesh
    /// is named too.
    #[test]
    fn under_route_each_selection_change_after_the_opening_writes_one_moved_line() {
        let script = vec![
            seen(on_the_mesh()),
            Seen::Pair(None),
            seen(on_the_far_turn()),
            seen(on_the_far_turn()),
            seen(native()),
            seen(on_the_mesh()),
        ];
        let (moves, reads, out) = followed(on_the_mesh(), script);
        assert_eq!((moves, reads), (3, 7), "{out}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        for (line, (from, to, route, local)) in lines.iter().zip([
            ("derp", "turn", "turn", "203.0.113.7:50000"),
            ("turn", "direct", "native", "192.168.1.10:5000"),
            ("direct", "derp", "mesh-relayed", "100.65.4.34:5001"),
        ]) {
            assert!(line.contains("session carrier moved"), "{line}");
            for want in [
                format!("from=\"{from}\""),
                format!("to=\"{to}\""),
                format!("route={route}"),
                format!("local_addr={local}"),
            ] {
                assert!(line.contains(&want), "{want} in {line}");
            }
        }
    }

    /// #3259 — a relay remote reads `signalled` as the pair is read, and its
    /// carrier is the one it read before the field.
    #[test]
    fn a_relay_remote_reads_signalled_and_keeps_its_carrier() {
        let s = selected(
            (T::Host, "192.168.1.10", 5000),
            (T::Relay, "198.51.100.9", 7001),
            RelayTransport::NoRelay,
            None,
        );
        let view = s.view();
        assert_eq!(view.remote_relay, RemoteRelay::Signalled, "{view:?}");
        assert_eq!(carrier_of(&view, None), SessionCarrier::Turn, "{view:?}");
        let before = PairView {
            remote_relay: RemoteRelay::NotShown,
            ..view.clone()
        };
        assert_eq!(carrier_of(&before, None), carrier_of(&view, None));
    }

    /// #3259 — under `route`, the same pair read again with its remote shown
    /// to be the far end's relay writes no line, and the next move's `from`
    /// names that relay, not the opening read's `direct`. A read after it
    /// that cannot say (`unknown`) does not undo it.
    #[test]
    fn a_pair_whose_relay_shows_after_the_opening_moves_from_that_relay() {
        let learnt = selected(
            (T::Host, "192.168.1.10", 5000),
            (T::Prflx, "203.0.113.7", 49170),
            RelayTransport::NoRelay,
            Some(false),
        );
        assert_eq!(learnt.remote_relay, RemoteRelay::NotShown);
        let shown = Selection {
            remote_relay: RemoteRelay::Signalled,
            ..learnt.clone()
        };
        let unread = Selection {
            remote_relay: RemoteRelay::Unknown,
            ..learnt.clone()
        };
        let script = vec![
            seen(learnt.clone()),
            seen(shown),
            seen(unread),
            seen(native()),
        ];
        let (moves, reads, out) = followed(learnt, script);
        assert_eq!((moves, reads), (1, 5), "{out}");
        assert!(
            out.contains("from=\"turn\" to=\"direct\" route=native"),
            "{out}"
        );
    }

    /// Two sockets at one address are two pairs: a move between them is a move.
    #[test]
    fn a_move_to_another_port_at_the_same_address_is_a_move() {
        let other_socket = selected(
            (T::Host, "100.65.4.34", 5002),
            (T::Prflx, "100.65.4.33", 40000),
            RelayTransport::NoRelay,
            Some(true),
        );
        let (moves, _, out) = followed(on_the_mesh(), vec![seen(other_socket)]);
        assert_eq!(moves, 1, "{out}");
        assert!(
            out.contains("from=\"derp\" to=\"derp\" route=mesh-relayed"),
            "{out}"
        );
    }

    /// Under every value but `route` the agent reads no route: nothing
    /// follows the selection, nothing is read, and no line is written,
    /// whatever the selection does. Under `route`, a pair the agent reads no
    /// route for ends the follow without a line.
    #[test]
    fn without_route_nothing_follows_the_selection() {
        let unrouted = |s: Selection| Selection { rides: None, ..s };
        let script = || vec![seen(unrouted(on_the_far_turn())), seen(unrouted(native()))];
        let (moves, reads, out) = followed(unrouted(on_the_mesh()), script());
        assert_eq!((moves, reads, out.as_str()), (0, 0, ""));
        let (moves, reads, out) = followed(on_the_mesh(), script());
        assert_eq!((moves, reads, out.as_str()), (0, 1, ""));
    }

    /// The pumps' constrained flag per move class (phase B builds the line
    /// only). The flag is the pumps' own, unchanged: at the opening
    /// (`detect_constrained_transport`) and every 5 s after it (the
    /// relay-escape re-read, `current_pair_is_relay`), one predicate: a relay
    /// at either end that is not the device's own fast TURN
    /// ([`crate::encode::relay_addr_is_fast_local`]), or a remote in the
    /// overlay's range whose carrier is not direct. A move between relay
    /// tiers keeps it; a move to a native pair lifts it at the next re-read.
    #[test]
    fn the_constrained_flag_per_move_class() {
        let flag = |s: &Selection, overlay_direct: bool| {
            let relay = |c: &RTCIceCandidate| {
                c.typ == T::Relay && !crate::encode::relay_addr_is_fast_local(&c.address)
            };
            relay(&s.local)
                || relay(&s.remote)
                || (crate::peer::addr_is_overlay_range(&s.remote.address) && !overlay_direct)
        };
        let (relayed, direct) = (false, true);
        for (class, (before, before_direct), (after, after_direct), (was, now)) in [
            (
                "mesh-relayed to TURN",
                (on_the_mesh(), relayed),
                (on_the_far_turn(), relayed),
                (true, true),
            ),
            (
                "mesh-relayed to native",
                (on_the_mesh(), relayed),
                (native(), relayed),
                (true, false),
            ),
            (
                "mesh-direct to native",
                (on_the_mesh(), direct),
                (native(), direct),
                (false, false),
            ),
            (
                "TURN to native",
                (on_the_far_turn(), relayed),
                (native(), relayed),
                (true, false),
            ),
        ] {
            assert_eq!(
                (flag(&before, before_direct), flag(&after, after_direct)),
                (was, now),
                "{class}"
            );
        }
    }
}
