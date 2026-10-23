// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-115 P4b — one allocation leg's TURN bytes into the paying org's
//! `turn_bytes`, once across pods. The one place both readers of coturn's
//! totals book them:
//!
//! * the cluster's collector (`modules/network/src/turn_usage.rs`), which
//!   every pod runs against the cluster Redis;
//! * the PoP pull (`relay_load.rs`, P4b-2), which every pod runs against
//!   every regional PoP's `derp-relay`.
//!
//! Rules, in order:
//!
//! * **Never guessed.** A username that names no org (the session-less
//!   pre-fetch, an uncharged overlay pair) adds to
//!   [`TURN_UNATTRIBUTED_BYTES`] and is not booked: FR-20's rule.
//! * **Once per allocation leg.** Every pod hears or pulls every total, so the
//!   first to win a `SET NX` on the leg books it. The key carries the username
//!   and the allocation id, because coturn numbers allocations per process and
//!   two coturns can reuse an id; a PoP's key also carries its region, so the
//!   cluster's coturn and each PoP's never share a key.
//! * **Under-reporting is the cheaper error.** A claim that cannot be made, or
//!   a bucket write that fails, books nothing and is never retried: a retry
//!   that half succeeded would book twice.

use std::sync::atomic::{AtomicU64, Ordering};

use bson::{DateTime, oid::ObjectId};
use roomler_ai_remote_control::{turn_creds::org_of_username, turn_stats::Reported};
use roomler_ai_services::dao::stats::{Meter, StatsDao};

use crate::ws::redis_pubsub::RedisPubSub;

/// How long an allocation leg's "counted" mark lives: far past any
/// allocation's life and past a PoP's retention (an hour), so a late
/// duplicate is still recognised.
pub const COUNTED_TTL_SECS: u64 = 7 * 86_400;

/// Bytes coturn reported under a username that names no org. A diagnostic,
/// not a ledger: nothing is claimed for these, so each pod counts all it
/// hears.
pub static TURN_UNATTRIBUTED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Which coturn reported a total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source<'a> {
    /// The cluster's coturn workers, heard on the cluster Redis.
    Cluster,
    /// A regional PoP's coturn, pulled from its `derp-relay`, by region id.
    Pop(&'a str),
}

/// What [`record`] did with one leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Booked as this org's `turn_bytes`.
    Booked(ObjectId),
    /// Another pod booked it first, or this one already did.
    AlreadyBooked,
    /// The username names no org.
    Unattributed,
    /// coturn sent nothing on this leg.
    Nothing,
    /// The claim or the bucket write failed, so nothing was booked.
    Failed(String),
}

/// The once-only mark for one leg. The cluster's form is the one P4b-1
/// shipped, byte for byte, so pods on either side of a rolling deploy agree
/// on it and cannot each book the same leg.
pub fn counted_key(db_name: &str, source: Source<'_>, r: &Reported) -> String {
    let leg = if r.peer_leg { "peer" } else { "client" };
    match source {
        Source::Cluster => format!(
            "roomler:turn_counted:{db_name}:{}:{}:{leg}",
            r.username, r.allocation
        ),
        Source::Pop(region) => format!(
            "roomler:turn_counted:{db_name}:pop:{region}:{}:{}:{leg}",
            r.username, r.allocation
        ),
    }
}

/// Book one leg against the org its username names, once across pods.
pub async fn record(
    redis: &RedisPubSub,
    stats: &StatsDao,
    db_name: &str,
    source: Source<'_>,
    r: &Reported,
) -> Outcome {
    let Some(org) = org_of_username(&r.username).and_then(|h| ObjectId::parse_str(h).ok()) else {
        TURN_UNATTRIBUTED_BYTES.fetch_add(r.sentb, Ordering::Relaxed);
        return Outcome::Unattributed;
    };
    if r.sentb == 0 {
        return Outcome::Nothing;
    }
    match redis
        .try_claim(&counted_key(db_name, source, r), COUNTED_TTL_SECS)
        .await
    {
        Ok(true) => {}
        Ok(false) => return Outcome::AlreadyBooked,
        Err(e) => return Outcome::Failed(format!("the once-only claim failed: {e}")),
    }
    let now = DateTime::now().timestamp_millis() / 1000;
    match stats
        .add_usage(org, Meter::TurnBytes, now, r.sentb as i64)
        .await
    {
        Ok(()) => Outcome::Booked(org),
        Err(e) => Outcome::Failed(format!("the usage bucket write failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(peer_leg: bool) -> Reported {
        Reported {
            username: "1792000000:u:org-6a975e17971e2d55ddcdedb2".into(),
            allocation: "001000000000000042".into(),
            peer_leg,
            sentb: 1,
        }
    }

    /// The cluster's key is P4b-1's, unchanged: a pod from before this module
    /// and one after it must claim the same key for the same leg.
    #[test]
    fn the_clusters_key_is_p4b_1s_unchanged() {
        assert_eq!(
            counted_key("roomler2", Source::Cluster, &leg(false)),
            "roomler:turn_counted:roomler2:1792000000:u:org-6a975e17971e2d55ddcdedb2:001000000000000042:client"
        );
        assert_eq!(
            counted_key("roomler2", Source::Cluster, &leg(true)),
            "roomler:turn_counted:roomler2:1792000000:u:org-6a975e17971e2d55ddcdedb2:001000000000000042:peer"
        );
    }

    /// The same username and allocation id on two coturns are two legs: the
    /// cluster's and each PoP's keys never meet.
    #[test]
    fn a_pops_key_carries_its_region() {
        let cluster = counted_key("roomler2", Source::Cluster, &leg(false));
        let east = counted_key("roomler2", Source::Pop("us-east"), &leg(false));
        let north = counted_key("roomler2", Source::Pop("eu-north"), &leg(false));
        assert!(east.starts_with("roomler:turn_counted:roomler2:pop:us-east:"));
        assert_ne!(east, cluster);
        assert_ne!(east, north);
    }
}
