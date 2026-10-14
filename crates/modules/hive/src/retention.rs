// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2g-3a — retention, the server's sweep: a session its
//! organization's `retention_days` no longer keep is purged from every device
//! that held it, exactly as its owner's own purge would be ([`crate::purge`],
//! reason `retention`). Design §4.4: "Retention and deletion are org policy
//! (default 90 days)."
//!
//! | Rule | Why |
//! |---|---|
//! | Only a session that is over (`ended`, `refused`, `lost`), counted from when it ended; one with no `ended_at`, from its last update | a live one renews by its own activity, and is its owner's to purge |
//! | `retention_days` is the organization's policy; with none, its default (90 days); `0` keeps every session for ever | the policy route's own default ([`crate::policy::HivePolicy::defaults`]) |
//! | One pod a cycle, by a claim in the database (the startup lease's `locks` pattern) | two pods would each write a purge of one session: the tombstone's compare-and-set lets one take, and the other's would be written for nothing |
//! | At most [`PER_PASS`] sessions an organization a pass | a backlog, the first sweep after the switch goes on, drains over several cycles instead of one long one |
//! | No sweep with `hive.replicaset` off, or `hive.retention_sweep_secs = 0` | P2's switch: an issued purge would wait anyway |
//!
//! ⚠️ This reaches a device that comes back. A device that never does keeps
//! its copy: the device's own clock (`retain_until`) is P2g-3b.

use std::time::Duration;

use bson::{Bson, DateTime, doc};
use tracing::{info, warn};

use crate::HiveState;
use crate::model::SessionStatus;
use crate::policy::HivePolicy;
use crate::purge::{self, Issued, PurgeReason};

/// How many sessions of one organization one pass purges at most.
pub(crate) const PER_PASS: i64 = 100;

/// The claim's document in `locks`, the startup lease's collection.
const CLAIM: &str = "hive_retention_sweep";

/// The statuses retention counts a session over in.
pub(crate) const OVER: [SessionStatus; 3] = [
    SessionStatus::Ended,
    SessionStatus::Refused,
    SessionStatus::Lost,
];

/// Spawn the sweep: every `hive.retention_sweep_secs`, the pod that claims
/// the cycle purges what retention no longer keeps. The first pass is a whole
/// period out, so a server that boots does not sweep in its first breath.
pub(crate) fn spawn(state: HiveState) {
    let period = state.retention_sweep;
    if !state.replicaset || period.is_zero() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(period).await;
            match claim(&state, period).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    warn!(%e, "hive: the retention sweep's claim failed — this cycle is skipped");
                    continue;
                }
            }
            match sweep(&state).await {
                Ok(0) => {}
                Ok(purged) => info!(purged, "hive: retention purged sessions"),
                Err(e) => {
                    warn!(%e, "hive: the retention sweep failed — the next cycle tries again")
                }
            }
        }
    });
}

/// Claim this cycle for half a period: `true` for the one pod whose write
/// lands. An expired claim is taken over; a held one, or a concurrent first
/// write (a duplicate `_id`), is someone else's.
pub(crate) async fn claim(state: &HiveState, period: Duration) -> anyhow::Result<bool> {
    let now = DateTime::now();
    let hold = i64::try_from(period.as_millis() / 2).unwrap_or(i64::MAX);
    let until = DateTime::from_millis(now.timestamp_millis().saturating_add(hold));
    let locks = state.core.db.collection::<bson::Document>("locks");
    match locks
        .update_one(
            doc! { "_id": CLAIM, "expires_at": { "$lt": now } },
            doc! { "$set": { "expires_at": until } },
        )
        .upsert(true)
        .await
    {
        Ok(r) => Ok(r.modified_count > 0 || r.upserted_id.is_some()),
        Err(e) if is_duplicate_key(&e) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn is_duplicate_key(e: &mongodb::error::Error) -> bool {
    matches!(
        *e.kind,
        mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteError(ref w))
            if w.code == 11000
    )
}

/// The instant before which a session that ended is past `days` of
/// retention; `None` = kept for ever (`0`).
pub(crate) fn cutoff(now: DateTime, days: u32) -> Option<DateTime> {
    (days > 0).then(|| {
        DateTime::from_millis(
            now.timestamp_millis()
                .saturating_sub(i64::from(days).saturating_mul(86_400_000)),
        )
    })
}

/// One pass: in every organization agent sessions serve, the sessions past
/// its retention, purged. How many purges took.
pub(crate) async fn sweep(state: &HiveState) -> anyhow::Result<usize> {
    let over: Vec<Bson> = OVER.iter().map(|s| Bson::from(s.as_str())).collect();
    let tenants = state
        .sessions
        .base
        .collection()
        .distinct(
            "tenant_id",
            doc! { "purged": Bson::Null, "status": { "$in": over } },
        )
        .await?;
    let mut purged = 0;
    for t in tenants {
        let Bson::ObjectId(tenant_id) = t else {
            continue;
        };
        if !state.scope.serves(tenant_id) {
            continue;
        }
        let days = match state.policies.get(tenant_id).await? {
            Some(p) => p.retention_days,
            None => HivePolicy::defaults(tenant_id).retention_days,
        };
        let Some(before) = cutoff(DateTime::now(), days) else {
            continue;
        };
        for s in state
            .sessions
            .past_retention(tenant_id, before, PER_PASS)
            .await?
        {
            purged += usize::from(purge_one(state, s).await);
        }
    }
    Ok(purged)
}

/// Purge `s` for retention, as its owner's purge would: `true` when it took.
async fn purge_one(state: &HiveState, s: crate::model::AgentSession) -> bool {
    let (sid, owner) = (s.id, s.owner_id);
    match purge::issue(state, s, owner, PurgeReason::Retention).await {
        Ok(Issued::Took(p, before)) => {
            purge::carry_out(state, &p, &before, owner).await;
            true
        }
        Ok(_) => false,
        Err(e) => {
            warn!(session = ?sid, %e, "hive: a session past its retention was not purged — the next cycle tries again");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `0` keeps every session for ever; a positive number counts whole days
    /// back from now.
    #[test]
    fn retention_counts_whole_days_back_and_zero_keeps_for_ever() {
        let now = DateTime::from_millis(100 * 86_400_000);
        assert_eq!(cutoff(now, 0), None);
        assert_eq!(cutoff(now, 1), Some(DateTime::from_millis(99 * 86_400_000)));
        assert_eq!(
            cutoff(now, 90),
            Some(DateTime::from_millis(10 * 86_400_000))
        );
        assert_eq!(
            cutoff(DateTime::from_millis(0), u32::MAX).map(|c| c.timestamp_millis() < 0),
            Some(true),
            "far back, never a wrap"
        );
        assert_eq!(PurgeReason::Retention.as_str(), "retention");
        assert_eq!(PurgeReason::Owner.as_str(), "owner");
        let every = [
            SessionStatus::Starting,
            SessionStatus::Idle,
            SessionStatus::Running,
            SessionStatus::AwaitingApproval,
            SessionStatus::Stopping,
            SessionStatus::Ended,
            SessionStatus::Refused,
            SessionStatus::Lost,
        ];
        let terminal: Vec<SessionStatus> = every.into_iter().filter(|s| s.is_terminal()).collect();
        assert_eq!(
            terminal,
            OVER.to_vec(),
            "every status a session is over in, and only those"
        );
    }
}
