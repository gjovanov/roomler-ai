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
//! | host · srflx · prflx on both sides | `direct` |
//! | a candidate type webrtc-rs could not name | `unknown` |
//!
//! ⚠️ Read **after a short settle**, not at the instant of `Connected`:
//! Chrome nominates a relay pair first and renominates to the host pair a
//! second or two later once the mDNS-resolved candidate succeeds, so the
//! very first pair would record `turn` for sessions that spend their life
//! direct. The pair is read once more [`SETTLE`] after the first nomination,
//! and that is what is recorded — once per session; a later path change is
//! not recorded (the bitrate clamp follows it live, this does not).
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

use std::time::Duration;

use tracing::{debug, info};
use webrtc::ice::url::{ProtoType, SchemeType};
use webrtc::peer_connection::RTCPeerConnection;

/// How long to wait for the first nomination after `Connected` (ICE has
/// usually nominated by then; the DC pumps wait the same 3 s).
pub const NOMINATION_WAIT: Duration = Duration::from_secs(3);
/// The settle before the recorded read (Chrome's relay→direct renomination
/// lands inside this).
pub const SETTLE: Duration = Duration::from_secs(2);
/// The LocalAPI budget for the overlay lookup — the same the bitrate probe
/// affords it.
pub const LOCALAPI_BUDGET: Duration = Duration::from_millis(400);

/// What a session rides. The wire spelling is [`SessionCarrier::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionCarrier {
    /// host · srflx · prflx, not over the overlay.
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

/// The nominated pair, the four facts the rule reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairView {
    pub local_kind: CandidateKind,
    pub remote_kind: CandidateKind,
    pub local_addr: String,
    pub remote_addr: String,
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
    for (kind, addr) in [
        (pair.local_kind, pair.local_addr.as_str()),
        (pair.remote_kind, pair.remote_addr.as_str()),
    ] {
        if kind == CandidateKind::Relay {
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
type Nominated = (PairView, RelayTransport, Option<bool>);

/// The nominated pair, polled every 100 ms for up to `wait`.
async fn nominated_pair(pc: &RTCPeerConnection, wait: Duration) -> Option<Nominated> {
    let sctp = pc.sctp();
    let dtls = sctp.transport();
    let ice = dtls.ice_transport();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Some(pair) = ice.get_selected_candidate_pair().await {
            let (local, remote) = (pair.local(), pair.remote());
            let view = PairView {
                local_kind: local.typ.into(),
                remote_kind: remote.typ.into(),
                local_addr: local.address.clone(),
                remote_addr: remote.address.clone(),
            };
            let relay = relay_transport_of(view.local_kind, pair.local_relay_transport());
            let rides = ice.route_read(&pair).await.map(|read| read.rides_the_mesh);
            return Some((view, relay, rides));
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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
pub async fn observe(pc: &RTCPeerConnection, session_id: bson::oid::ObjectId) -> SessionCarrier {
    let Some(first) = nominated_pair(pc, NOMINATION_WAIT).await else {
        debug!(%session_id, "session carrier: no nominated pair within the wait — unknown");
        return SessionCarrier::Unknown;
    };
    tokio::time::sleep(SETTLE).await;
    let (pair, relay, rides) = nominated_pair(pc, Duration::from_millis(500))
        .await
        .unwrap_or(first);
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
        "session carrier recorded (FR-102 P9c)"
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
    /// transport follows the carrier.
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
        ] {
            let at = rest
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is not in order in {out:?}"));
            rest = &rest[at + want.len()..];
        }
        assert_eq!(out.lines().count(), 1, "{out:?}");
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

    /// FR-148 P0 — through the vendored webrtc-ice and webrtc: a session on
    /// the agent's own TURN allocation over UDP reads `udp`, from the pair
    /// the carrier line reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pair_on_the_agents_udp_relay_names_its_transport() {
        use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
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
        let read = nominated_pair(&agent, Duration::from_secs(5)).await;
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
}
