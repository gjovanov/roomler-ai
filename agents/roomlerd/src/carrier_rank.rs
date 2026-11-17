// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-148 P4 — the carrier rank inside a session: what the session signals,
//! and how a data-channel pump's opening reads the selected pair. The switch,
//! the read of the mesh link at the offer and the rank handed to ICE are in
//! [`crate::rc_overlay`]; the rank itself is a patch of the vendored
//! webrtc-ice (`AgentConfig::carrier_rank`).
//!
//! | piece | function |
//! |---|---|
//! | a gathered candidate's line, and the `rc:ice` payload that signals it | [`gathered`] |
//! | the opening's pair read, which waits for the selection to move off a ranked-last pair | [`opening_pair`] |
//! | the `per-session ICE path detected` line, with how long the opening waited | [`path_detected`] |
//!
//! A ranked-last candidate is known by its priority alone: `256 - component`
//! (type preference 0, local preference 0), 255 for component 1, which no
//! other candidate on either side can have. Under switch `0` no candidate has
//! it, so nothing here changes a byte or waits a moment.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use webrtc::ice::agent::agent_config::ranked_last_priority;
use webrtc::ice_transport::RTCIceTransport;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_candidate_pair::RTCIceCandidatePair;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_gathering_state::RTCIceGatheringState;
use webrtc::peer_connection::RTCPeerConnection;

use crate::rc_overlay::RankMode;

/// What switch `1` appends to a ranked-last candidate's line: Chrome's
/// `network-cost` extension attribute at its costliest, which Chrome weighs
/// when it orders its candidate pairs.
pub const NETWORK_COST: &str = " network-cost 999";

/// Did the carrier rank put `c` last?
pub fn is_ranked_last(c: &RTCIceCandidate) -> bool {
    c.priority == ranked_last_priority(c.component)
}

/// One gathered local candidate: its `ICE: gathered local candidate` line,
/// and the `rc:ice` payload that signals it. Under switch `1` a ranked-last
/// candidate's line also ends with [`NETWORK_COST`]; under `prio` and `0` a
/// candidate is signalled exactly as gathered. `None`: it does not
/// serialize, which is logged.
pub fn gathered(
    session_id: ObjectId,
    c: &RTCIceCandidate,
    rank: RankMode,
) -> Option<serde_json::Value> {
    let ranked_last = is_ranked_last(c);
    tracing::info!(
        %session_id,
        typ = %c.typ,
        protocol = %c.protocol,
        address = %c.address,
        port = c.port,
        related = %format_args!("{}:{}", c.related_address, c.related_port),
        priority = c.priority,
        ranked_last,
        "ICE: gathered local candidate"
    );
    let mut json = match c.to_json() {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(%e, "failed to serialize ICE candidate");
            return None;
        }
    };
    if ranked_last && rank == RankMode::Cost {
        json.candidate.push_str(NETWORK_COST);
    }
    serde_json::to_value(&json).ok()
}

/// How long the opening's pair read waits, at most, for the selection to move
/// off a ranked-last pair.
pub const OPENING_WAIT: Duration = Duration::from_millis(1500);

/// How often the wait reads the selection again.
const OPENING_TICK: Duration = Duration::from_millis(50);

/// How long a connection's opening deadline is kept once it has passed.
const FORGET_AFTER: Duration = Duration::from_secs(60);

/// Why the opening's read stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpeningExit {
    /// The pair was not ranked last: read at once.
    NotRankedLast,
    /// The selection moved off the ranked-last pair.
    Moved,
    /// Gathering ended with no relay off the mesh: nothing better can come.
    NothingToWaitFor,
    /// 1.5 s passed: the opening goes on with the ranked-last pair.
    Deadline,
}

impl fmt::Display for OpeningExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotRankedLast => "none",
            Self::Moved => "moved",
            Self::NothingToWaitFor => "nothing-to-wait-for",
            Self::Deadline => "deadline",
        })
    }
}

/// How the opening's pair read went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opening {
    pub waited: Duration,
    pub exit: OpeningExit,
}

impl Opening {
    fn at_once() -> Self {
        Self {
            waited: Duration::ZERO,
            exit: OpeningExit::NotRankedLast,
        }
    }
}

/// Each peer connection's opening deadline, set by its first ranked-last
/// read and shared by the next: a pump reads the pair twice
/// (`detect_constrained_transport`, then `nominated_remote_ip`), and the two
/// wait 1.5 s between them, not 1.5 s each. Keyed by the connection's stats
/// id; a deadline is forgotten a minute after it passed.
static OPENINGS: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);

fn opening_deadline(pc: &str) -> Instant {
    let now = Instant::now();
    let mut openings = OPENINGS.lock().unwrap_or_else(PoisonError::into_inner);
    openings.retain(|_, deadline| now.saturating_duration_since(*deadline) < FORGET_AFTER);
    *openings.entry(pc.to_string()).or_insert(now + OPENING_WAIT)
}

/// The opening's pair read: `pair` is the selection the helper just read. A
/// pair that is not ranked last comes back at once. A ranked-last one waits,
/// up to [`OPENING_WAIT`] a connection, for the selection to move off it, and
/// stops at once when gathering has ended with no relay candidate off the
/// mesh: then nothing better can come. Returns the pair the opening fixes
/// itself on.
pub async fn opening_pair(
    pc: &RTCPeerConnection,
    ice: &RTCIceTransport,
    pair: RTCIceCandidatePair,
) -> (RTCIceCandidatePair, Opening) {
    if !is_ranked_last(pair.local()) {
        return (pair, Opening::at_once());
    }
    let deadline = opening_deadline(pc.get_stats_id());
    wait_for_move(
        pair,
        |p: &RTCIceCandidatePair| is_ranked_last(p.local()),
        move || ice.get_selected_candidate_pair(),
        move || async move {
            pc.ice_gathering_state() == RTCIceGatheringState::Complete
                && !ice
                    .local_candidates()
                    .await
                    .iter()
                    .any(|c| c.typ == RTCIceCandidateType::Relay && !is_ranked_last(c))
        },
        deadline,
    )
    .await
}

/// [`opening_pair`]'s wait, over the reads it makes: `read` the selection
/// again, and whether there is `nothing_to_wait_for`. Each is asked once a
/// tick until the selection is no longer `ranked_last`, nothing is left to
/// wait for, or `deadline` passes.
pub async fn wait_for_move<P, R, RF, N, NF>(
    first: P,
    ranked_last: impl Fn(&P) -> bool,
    mut read: R,
    mut nothing_to_wait_for: N,
    deadline: Instant,
) -> (P, Opening)
where
    R: FnMut() -> RF,
    RF: Future<Output = Option<P>>,
    N: FnMut() -> NF,
    NF: Future<Output = bool>,
{
    let started = Instant::now();
    if !ranked_last(&first) {
        return (first, Opening::at_once());
    }
    let mut pair = first;
    let exit = loop {
        if nothing_to_wait_for().await {
            break OpeningExit::NothingToWaitFor;
        }
        let now = Instant::now();
        if now >= deadline {
            break OpeningExit::Deadline;
        }
        tokio::time::sleep(OPENING_TICK.min(deadline - now)).await;
        if let Some(next) = read().await {
            let moved = !ranked_last(&next);
            pair = next;
            if moved {
                break OpeningExit::Moved;
            }
        }
    };
    let opening = Opening {
        waited: started.elapsed(),
        exit,
    };
    (pair, opening)
}

/// `per-session ICE path detected (adaptive bitrate)`: the pair a
/// data-channel pump's opening fixed its constrained flag on. It names the
/// ADDRESSES, not just the types: a "direct" Srflx↔Prflx pair whose remote is
/// the router's WAN IP is a NAT hairpin, while a 192.168.x remote is the true
/// LAN path. FR-148 P4 adds how long the opening waited, and why it stopped.
pub fn path_detected(
    session_id: ObjectId,
    relay: bool,
    local: &RTCIceCandidate,
    remote: &RTCIceCandidate,
    opening: Opening,
) {
    tracing::info!(
        %session_id,
        relay,
        local_typ = ?local.typ,
        local_proto = ?local.protocol,
        local_addr = %format!("{}:{}", local.address, local.port),
        remote_typ = ?remote.typ,
        remote_proto = ?remote.protocol,
        remote_addr = %format!("{}:{}", remote.address, remote.port),
        waited_ms = opening.waited.as_millis() as u64,
        opening = %opening.exit,
        "per-session ICE path detected (adaptive bitrate)"
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use webrtc::ice_transport::ice_protocol::RTCIceProtocol;

    use super::*;

    /// A type-90 host on the mesh, today's override.
    const TYPE_90: u32 = (1 << 24) * 90 + (1 << 8) * 65535 + 255;
    const UDP_RELAY: u32 = (1 << 8) * 65535 + 255;

    fn cand(typ: RTCIceCandidateType, address: &str, priority: u32) -> RTCIceCandidate {
        RTCIceCandidate {
            foundation: "1".into(),
            typ,
            address: address.into(),
            port: 50_000,
            protocol: RTCIceProtocol::Udp,
            priority,
            component: 1,
            related_address: if typ == RTCIceCandidateType::Host {
                String::new()
            } else {
                "0.0.0.0".into()
            },
            ..Default::default()
        }
    }

    fn sid() -> ObjectId {
        ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap()
    }

    /// The `candidate` string `gathered` signals.
    fn signalled(c: &RTCIceCandidate, rank: RankMode) -> String {
        let json = gathered(sid(), c, rank).expect("serializes");
        json["candidate"].as_str().unwrap().to_string()
    }

    #[test]
    fn only_the_ranks_own_priority_is_ranked_last() {
        assert!(is_ranked_last(&cand(
            RTCIceCandidateType::Host,
            "100.65.4.34",
            255
        )));
        assert!(!is_ranked_last(&cand(
            RTCIceCandidateType::Host,
            "100.65.4.34",
            TYPE_90
        )));
        assert!(!is_ranked_last(&cand(
            RTCIceCandidateType::Relay,
            "203.0.113.7",
            UDP_RELAY
        )));
        // A browser relay over TLS at local preference 1: the lowest Chrome
        // computes, and still not the rank's.
        assert!(!is_ranked_last(&cand(
            RTCIceCandidateType::Relay,
            "203.0.113.7",
            511
        )));
        let rtcp = RTCIceCandidate {
            component: 2,
            ..cand(RTCIceCandidateType::Host, "100.65.4.34", 254)
        };
        assert!(is_ranked_last(&rtcp));
    }

    /// Done 7 — ` network-cost 999` under switch `1`, on ranked-last
    /// candidates only; never under `prio` or `0`.
    #[test]
    fn network_cost_ends_only_a_ranked_last_line_under_switch_1() {
        let last = [
            cand(RTCIceCandidateType::Host, "100.65.4.34", 255),
            cand(RTCIceCandidateType::Srflx, "100.65.4.34", 255),
            cand(RTCIceCandidateType::Relay, "100.65.4.33", 255),
        ];
        for c in &last {
            let line = signalled(c, RankMode::Cost);
            assert!(line.starts_with("candidate:1 1 udp 255 "), "{line}");
            assert!(line.ends_with(" network-cost 999"), "{line}");
            assert_eq!(line.matches("network-cost").count(), 1, "{line}");
            let plain = signalled(c, RankMode::Prio);
            assert!(!plain.contains("network-cost"), "{plain}");
            assert_eq!(format!("{plain}{NETWORK_COST}"), line);
        }
        for c in [
            cand(RTCIceCandidateType::Host, "100.65.4.34", TYPE_90),
            cand(RTCIceCandidateType::Host, "192.168.1.20", 0),
            cand(RTCIceCandidateType::Relay, "203.0.113.7", UDP_RELAY),
        ] {
            for rank in [RankMode::Off, RankMode::Prio, RankMode::Cost] {
                let line = signalled(&c, rank);
                assert!(!line.contains("network-cost"), "{rank}: {line}");
            }
        }
        // The rest of the payload is the candidate's own.
        let json = gathered(sid(), &last[0], RankMode::Cost).unwrap();
        let plain = gathered(sid(), &last[0], RankMode::Off).unwrap();
        assert_eq!(json["sdpMid"], plain["sdpMid"]);
        assert_eq!(json["sdpMLineIndex"], plain["sdpMLineIndex"]);
    }

    /// What `f` logs, as the daemon's log file writes it: the same capture as
    /// `media_delivery`'s cells.
    fn logged(f: impl FnOnce()) -> String {
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

    #[test]
    fn the_gathered_line_names_its_fields() {
        let c = cand(RTCIceCandidateType::Srflx, "100.65.4.34", 255);
        let out = logged(|| {
            gathered(sid(), &c, RankMode::Prio);
        });
        in_order(
            &out,
            &[
                " INFO ",
                "ICE: gathered local candidate",
                "session_id=65a1b2c3d4e5f60718293a4b",
                "typ=srflx",
                "protocol=udp",
                "address=100.65.4.34",
                "port=50000",
                "related=0.0.0.0:0",
                "priority=255",
                "ranked_last=true",
            ],
        );
        let c = cand(RTCIceCandidateType::Host, "100.65.4.34", TYPE_90);
        let out = logged(|| {
            gathered(sid(), &c, RankMode::Off);
        });
        in_order(
            &out,
            &[
                "typ=host",
                &format!("priority={TYPE_90}"),
                "ranked_last=false",
            ],
        );
    }

    #[test]
    fn the_path_line_names_its_fields() {
        let local = cand(RTCIceCandidateType::Relay, "203.0.113.7", UDP_RELAY);
        let remote = cand(RTCIceCandidateType::Srflx, "198.51.100.4", 1_686_052_607);
        let opening = Opening {
            waited: Duration::from_millis(412),
            exit: OpeningExit::Moved,
        };
        let out = logged(|| path_detected(sid(), true, &local, &remote, opening));
        in_order(
            &out,
            &[
                " INFO ",
                "per-session ICE path detected (adaptive bitrate)",
                "session_id=65a1b2c3d4e5f60718293a4b",
                "relay=true",
                "local_typ=Relay",
                "local_proto=Udp",
                "local_addr=203.0.113.7:50000",
                "remote_typ=Srflx",
                "remote_proto=Udp",
                "remote_addr=198.51.100.4:50000",
                "waited_ms=412",
                "opening=moved",
            ],
        );
        for (exit, word) in [
            (OpeningExit::NotRankedLast, "opening=none"),
            (OpeningExit::NothingToWaitFor, "opening=nothing-to-wait-for"),
            (OpeningExit::Deadline, "opening=deadline"),
        ] {
            let opening = Opening {
                waited: Duration::ZERO,
                exit,
            };
            let out = logged(|| path_detected(sid(), false, &local, &remote, opening));
            assert!(out.contains(word) && out.contains("waited_ms=0"), "{out:?}");
        }
    }

    /// A pair as the wait sees it, by its local candidate's priority.
    fn last(p: &u32) -> bool {
        *p == 255
    }

    /// Successive reads of the selection.
    fn reads(seq: Vec<Option<u32>>) -> impl FnMut() -> std::future::Ready<Option<u32>> {
        let mut seq = seq.into_iter();
        move || std::future::ready(seq.next().flatten())
    }

    fn soon(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    /// Done 8 — a pair that is not ranked last is read at once: no read, no
    /// wait.
    #[tokio::test]
    async fn a_pair_not_ranked_last_is_taken_at_once() {
        let (pair, opening) = wait_for_move(
            TYPE_90,
            last,
            || async { panic!("no second read") },
            || async { panic!("nothing to ask") },
            soon(1_500),
        )
        .await;
        assert_eq!((pair, opening), (TYPE_90, Opening::at_once()));
    }

    /// Done 8 — the selection moves: the wait ends on the next read, with the
    /// new pair.
    #[tokio::test]
    async fn the_wait_ends_when_the_selection_moves() {
        let (pair, opening) = wait_for_move(
            255,
            last,
            reads(vec![Some(255), None, Some(UDP_RELAY)]),
            || async { false },
            soon(1_500),
        )
        .await;
        assert_eq!((pair, opening.exit), (UDP_RELAY, OpeningExit::Moved));
        assert!(
            opening.waited >= OPENING_TICK * 3 && opening.waited < Duration::from_millis(1_000),
            "{opening:?}"
        );
    }

    /// Done 8 — gathering has ended with no relay off the mesh: at once.
    #[tokio::test]
    async fn the_wait_ends_at_once_with_nothing_to_wait_for() {
        let (pair, opening) = wait_for_move(
            255,
            last,
            || async { panic!("no read") },
            || async { true },
            soon(1_500),
        )
        .await;
        assert_eq!((pair, opening.exit), (255, OpeningExit::NothingToWaitFor));
        assert!(opening.waited < OPENING_TICK, "{opening:?}");
        // ... and as soon as it ends, when it ends during the wait.
        let mut asked = 0;
        let (_, opening) = wait_for_move(
            255,
            last,
            reads(vec![Some(255); 10]),
            || {
                asked += 1;
                std::future::ready(asked == 3)
            },
            soon(1_500),
        )
        .await;
        assert_eq!(opening.exit, OpeningExit::NothingToWaitFor);
        assert!(opening.waited < Duration::from_millis(1_000), "{opening:?}");
    }

    /// Done 8 — nothing moves and something may still come: the wait gives up
    /// at its deadline and the opening goes on with the ranked-last pair.
    #[tokio::test]
    async fn the_wait_gives_up_at_the_deadline() {
        let (pair, opening) = wait_for_move(
            255,
            last,
            reads(vec![Some(255); 100]),
            || async { false },
            soon(300),
        )
        .await;
        assert_eq!((pair, opening.exit), (255, OpeningExit::Deadline));
        assert!(
            opening.waited >= Duration::from_millis(300)
                && opening.waited < Duration::from_millis(1_000),
            "{opening:?}"
        );
        // A deadline already passed: the second read of the pair waits no
        // more.
        let (_, opening) =
            wait_for_move(255, last, reads(vec![]), || async { false }, Instant::now()).await;
        assert_eq!(opening.exit, OpeningExit::Deadline);
        assert!(opening.waited < OPENING_TICK, "{opening:?}");
    }

    /// The two helpers that await [`opening_pair`] run inside the data-channel
    /// pumps, which `tokio::spawn` runs and which compile only with their
    /// encoders (in CI): this keeps the wait's future `Send` in every build
    /// that compiles the tests.
    #[allow(dead_code)]
    fn the_opening_wait_is_send(
        pc: &RTCPeerConnection,
        ice: &RTCIceTransport,
        pair: RTCIceCandidatePair,
    ) {
        fn send<T: Send>(_: &T) {}
        send(&opening_pair(pc, ice, pair));
    }

    /// The pump's two reads share one connection's deadline.
    #[test]
    fn a_connections_two_reads_share_one_deadline() {
        let first = opening_deadline("PeerConnection-cell-a");
        let second = opening_deadline("PeerConnection-cell-a");
        assert_eq!(first, second);
        assert!(first <= Instant::now() + OPENING_WAIT);
        let other = opening_deadline("PeerConnection-cell-b");
        assert!(other >= first);
    }
}
