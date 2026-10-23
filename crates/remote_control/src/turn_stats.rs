// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-115 P4b — coturn's per-allocation totals, as its statistics DB
//! (`--redis-statsdb`) publishes them. Shared by the two readers, so they can
//! never disagree about what a message means:
//!
//! * the API's collector for the cluster's coturn
//!   (`modules/network/src/turn_usage.rs`), subscribed to the cluster Redis;
//! * a regional PoP's `derp-relay` (P4b-2), subscribed to the PoP's own Redis,
//!   which keeps the messages for the API to pull.
//!
//! When an allocation is deleted, coturn publishes its totals on
//! `turn/realm/<realm>/user/<username>/allocation/<id>/total_traffic` (the
//! client leg) and `…/total_traffic/peer` (the relay ↔ peer leg), as
//! `rcvp=…, rcvb=…, sentp=…, sentb=…` (coturn's
//! `src/apps/relay/ns_ioalib_engine_impl.c`). Both strings were read from
//! coturn 4.18.0 itself, the version the cluster and the PoPs run.

/// The two patterns that match an allocation's totals and nothing else: not
/// the periodic `traffic` deltas, and not the realm-less channels a server
/// without a realm publishes.
pub const CHANNELS: [&str; 2] = [
    "turn/realm/*/user/*/allocation/*/total_traffic",
    "turn/realm/*/user/*/allocation/*/total_traffic/peer",
];

/// One allocation leg's totals, as coturn published them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    pub username: String,
    pub allocation: String,
    pub peer_leg: bool,
    /// What coturn sent on this leg: toward the client, or toward the peer.
    pub sentb: u64,
}

/// Parse one coturn message. `channel` is
/// `turn/realm/<realm>/user/<username>/allocation/<id>/total_traffic[/peer]`
/// and `payload` is `rcvp=…, rcvb=…, sentp=…, sentb=…`. Anything else,
/// including the periodic `traffic` deltas and a session that never
/// authenticated (coturn publishes those with an EMPTY username), is `None`.
pub fn parse(channel: &str, payload: &str) -> Option<Reported> {
    let parts: Vec<&str> = channel.split('/').collect();
    let (peer_leg, len) = match parts.as_slice() {
        [.., "total_traffic", "peer"] => (true, 9),
        [.., "total_traffic"] => (false, 8),
        _ => return None,
    };
    if parts.len() != len
        || parts[0] != "turn"
        || parts[1] != "realm"
        || parts[3] != "user"
        || parts[5] != "allocation"
        || parts[4].is_empty()
        || parts[6].is_empty()
    {
        return None;
    }
    let sentb = payload.split(',').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        if k.trim() == "sentb" {
            v.trim().parse::<u64>().ok()
        } else {
            None
        }
    })?;
    Some(Reported {
        username: parts[4].to_string(),
        allocation: parts[6].to_string(),
        peer_leg,
        sentb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_creds::{charged, org_of_username};

    const ORG: &str = "6a975e17971e2d55ddcdedb2";

    fn user() -> String {
        let principal = charged("6a975e17971e2d55ddcdedb0", ORG);
        format!("1792000000:{principal}")
    }

    /// The two channels coturn publishes an allocation's totals on, in its
    /// own format.
    #[test]
    fn both_legs_of_a_total_parse() {
        let u = user();
        let client = parse(
            &format!("turn/realm/roomler.ai/user/{u}/allocation/001000000000000042/total_traffic"),
            "rcvp=10, rcvb=1200, sentp=12, sentb=34567",
        )
        .unwrap();
        assert_eq!(
            client,
            Reported {
                username: u.clone(),
                allocation: "001000000000000042".into(),
                peer_leg: false,
                sentb: 34_567,
            }
        );
        let peer = parse(
            &format!(
                "turn/realm/roomler.ai/user/{u}/allocation/001000000000000042/total_traffic/peer"
            ),
            "rcvp=12, rcvb=34567, sentp=10, sentb=1200",
        )
        .unwrap();
        assert!(peer.peer_leg);
        assert_eq!(peer.sentb, 1_200);
        assert_eq!(org_of_username(&peer.username), Some(ORG));
    }

    /// The periodic deltas, other shapes and a payload without `sentb` are
    /// not totals.
    #[test]
    fn only_totals_parse() {
        let u = user();
        assert_eq!(
            parse(
                &format!("turn/realm/r/user/{u}/allocation/1/traffic"),
                "rcvp=1, rcvb=1, sentp=1, sentb=1"
            ),
            None
        );
        assert_eq!(
            parse(
                &format!("turn/user/{u}/allocation/1/total_traffic"),
                "rcvp=1, rcvb=1, sentp=1, sentb=1"
            ),
            None,
            "realm-less channels are not subscribed to"
        );
        assert_eq!(
            parse(
                &format!("turn/realm/r/user/{u}/allocation/1/total_traffic"),
                "rcvp=1, rcvb=1"
            ),
            None
        );
        assert_eq!(parse("roomler:ws", "{}"), None);
    }

    /// coturn publishes `traffic` for a session that never authenticated with
    /// an empty username (seen on 4.18.0 in production). Its totals, should
    /// it publish any, name nobody and must not parse.
    #[test]
    fn an_unauthenticated_session_does_not_parse() {
        assert_eq!(
            parse(
                "turn/realm/roomler.ai/user//allocation/000000000000000001/total_traffic",
                "rcvp=1, rcvb=20, sentp=1, sentb=20"
            ),
            None
        );
    }
}
