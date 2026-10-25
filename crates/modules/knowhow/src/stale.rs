// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-5-3-1 — staleness (decisions 40 and 57; the FR-90 spec, §3e
//! "P4-5-3, staleness, as designed"; `docs/knowhow.md` §13.4).
//!
//! A person's fact, or a session's proposal a person or a probe confirmed,
//! that nothing has confirmed for `knowhow.stale_days` turns `stale`. The
//! sync's facts and a scan's own never do: the sync changes with its source,
//! and a scan retires what it no longer finds, which is its staleness. A
//! probe that finds a stale fact as it says turns it `active` again
//! ([`crate::probe::measure`]), and so do a person's confirm and a person's
//! write of it.
//!
//! Every pod sweeps, as every pod reconciles (`sync.rs`): the host does not
//! schedule periodic jobs yet (`run_startup_jobs` logs and skips
//! `Cadence::Every`), and the compare-and-set at the entity's rev makes two
//! pods' sweeps of one entity one write.

use std::time::Duration;

use bson::{DateTime, Document, doc};
use tracing::{info, warn};

use crate::KnowhowState;
use crate::model::{
    Fact, KnowhowEntity, OP_STALE, SOURCE_PERSON, SOURCE_SESSION, STATUS_ACTIVE, STATUS_STALE,
    TARGET_ENTITY,
};
use crate::write::{Stamp, record};

/// The days a fact may go unconfirmed when `knowhow.stale_days` is absent.
pub const DEFAULT_STALE_DAYS: u64 = 30;

/// How often every pod sweeps when `knowhow.stale_sweep_secs` is 0 or
/// absent.
pub const DEFAULT_STALE_SWEEP_SECS: u64 = 3600;

/// The entities one read of a sweep takes.
const PAGE: i64 = 200;

/// The writes one entity's turn tries before leaving it to the next sweep.
const ATTEMPTS: usize = 3;

const DAY_MS: i64 = 86_400_000;

/// `knowhow.stale_days` read: 30 days when absent, never at `0`.
pub fn days(setting: Option<u64>) -> Option<u64> {
    match setting {
        None => Some(DEFAULT_STALE_DAYS),
        Some(0) => None,
        Some(days) => Some(days),
    }
}

/// Whether a fact can age: `active`, and a person's or a session's. A
/// session's fact is `active` only once a person or a probe confirmed it.
pub fn ages(fact: &Fact) -> bool {
    fact.status == STATUS_ACTIVE && (fact.source == SOURCE_PERSON || fact.source == SOURCE_SESSION)
}

/// When a fact was last confirmed: the newest of a probe finding it as it
/// says, a person's confirmation, and its writing.
pub fn confirmed_at(fact: &Fact) -> DateTime {
    [fact.verified_at, fact.confirmed.as_ref().map(|c| c.at)]
        .into_iter()
        .flatten()
        .fold(fact.observed_at, |newest, at| newest.max(at))
}

/// The facts that turn stale at `cutoff`, by index: each one that can age
/// and was last confirmed before it.
pub fn turning(facts: &[Fact], cutoff: DateTime) -> Vec<usize> {
    facts
        .iter()
        .enumerate()
        .filter(|(_, f)| ages(f) && confirmed_at(f) < cutoff)
        .map(|(i, _)| i)
        .collect()
}

/// The moment `days` before `now`.
pub fn cutoff(now: DateTime, days: u64) -> DateTime {
    let back = i64::try_from(days)
        .unwrap_or(i64::MAX / DAY_MS)
        .saturating_mul(DAY_MS);
    DateTime::from_millis(now.timestamp_millis().saturating_sub(back))
}

/// One sweep at `now`: every fact past `days`, in every organization
/// knowhow serves, turned stale. How many it turned.
pub async fn sweep_once(state: &KnowhowState, now: DateTime, days: u64) -> usize {
    let cutoff = cutoff(now, days);
    let listed = state.scope.listed();
    let mut after = None;
    let mut turned = 0;
    loop {
        let page = match state
            .entities
            .ageing(listed.as_deref(), cutoff, after, PAGE)
            .await
        {
            Ok(page) => page,
            Err(e) => {
                warn!(%e, "knowhow: the stale sweep could not read the map; the next sweep tries");
                return turned;
            }
        };
        for entity in &page {
            if state.scope.serves(entity.tenant_id) {
                turned += turn(state, entity.clone(), cutoff).await;
            }
        }
        if page.len() < PAGE as usize {
            return turned;
        }
        after = page.last().and_then(|e| e.id);
    }
}

/// Turn one entity's aged facts stale, at the rev it was read, reading it
/// again when a write came between; once, with its history row. How many.
async fn turn(state: &KnowhowState, mut current: KnowhowEntity, cutoff: DateTime) -> usize {
    let Some(id) = current.id else {
        return 0;
    };
    for _ in 0..ATTEMPTS {
        let aged = turning(&current.facts, cutoff);
        if aged.is_empty() {
            return 0;
        }
        let stamp = Stamp::system(current.tenant_id);
        let mut facts = current.facts.clone();
        let mut said: Vec<Document> = Vec::with_capacity(aged.len());
        for &i in &aged {
            said.push(doc! {
                "path": facts[i].path.as_str(),
                "status": STATUS_STALE,
                "since": confirmed_at(&facts[i]),
            });
            facts[i].status = STATUS_STALE.into();
        }
        let facts = match bson::to_bson(&facts) {
            Ok(b) => b,
            Err(e) => {
                warn!(entity = %id, %e, "knowhow: an entity's facts could not be written stale");
                return 0;
            }
        };
        let mut set = doc! { "facts": facts };
        set.extend(stamp.touched());
        match state
            .entities
            .update_at(current.tenant_id, id, current.rev, set)
            .await
        {
            Ok(Some(after)) => {
                let row = stamp.row(
                    TARGET_ENTITY,
                    id,
                    vec![id],
                    after.rev,
                    OP_STALE,
                    doc! { "facts": said },
                );
                record(state, vec![row]).await;
                info!(
                    tenant = %current.tenant_id, entity = %id, facts = aged.len(),
                    "knowhow: facts nothing confirmed for knowhow.stale_days turned stale"
                );
                return aged.len();
            }
            Ok(None) => match state.entities.get_live(current.tenant_id, id).await {
                Ok(Some(again)) => current = again,
                Ok(None) => return 0,
                Err(e) => {
                    warn!(entity = %id, %e, "knowhow: an entity was not read again; the next sweep tries");
                    return 0;
                }
            },
            Err(e) => {
                warn!(entity = %id, %e, "knowhow: facts were not turned stale; the next sweep tries");
                return 0;
            }
        }
    }
    warn!(entity = %id, "knowhow: an entity kept changing; its aged facts wait for the next sweep");
    0
}

/// The sweep, every `secs` seconds on this pod, until the process ends.
pub fn spawn_sweeper(state: KnowhowState, secs: u64, days: u64) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(secs.max(1))).await;
            sweep_once(&state, DateTime::now(), days).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Confirmed, SOURCE_SCAN, SOURCE_SYNC, STATUS_PROPOSED};
    use bson::{Bson, oid::ObjectId};

    const NOW: i64 = 1_800_000_000_000;

    fn at_days_ago(days: i64) -> DateTime {
        DateTime::from_millis(NOW - days * DAY_MS)
    }

    fn fact(path: &str, source: &str, status: &str, written_days_ago: i64) -> Fact {
        Fact {
            path: path.into(),
            value: Bson::Int32(1),
            source: source.into(),
            by: None,
            device: None,
            session: None,
            confirmed: None,
            observed_at: at_days_ago(written_days_ago),
            verified_at: None,
            verified: None,
            status: status.into(),
        }
    }

    /// Absent is 30 days, `0` is never, anything else as it is written.
    #[test]
    fn the_setting_reads_absent_as_thirty_and_zero_as_never() {
        assert_eq!(days(None), Some(30));
        assert_eq!(days(Some(0)), None);
        assert_eq!(days(Some(7)), Some(7));
    }

    /// Only a person's fact and a confirmed proposal age: never the sync's,
    /// never a scan's own, never a proposal still waiting, never one stale
    /// already.
    #[test]
    fn only_a_persons_fact_and_a_confirmed_proposal_age() {
        assert!(ages(&fact("port", SOURCE_PERSON, STATUS_ACTIVE, 40)));
        assert!(ages(&fact("port", SOURCE_SESSION, STATUS_ACTIVE, 40)));
        assert!(!ages(&fact("port", SOURCE_SESSION, STATUS_PROPOSED, 40)));
        assert!(!ages(&fact("os", SOURCE_SYNC, STATUS_ACTIVE, 40)));
        assert!(!ages(&fact("path", SOURCE_SCAN, STATUS_ACTIVE, 40)));
        assert!(!ages(&fact("port", SOURCE_PERSON, STATUS_STALE, 40)));
    }

    /// A fact's age runs from the newest of a probe finding it as it says,
    /// a person's confirmation and its writing.
    #[test]
    fn a_fact_is_as_young_as_its_last_confirmation() {
        let mut f = fact("port", SOURCE_SESSION, STATUS_ACTIVE, 50);
        assert_eq!(confirmed_at(&f), at_days_ago(50));
        f.confirmed = Some(Confirmed {
            by: ObjectId::new(),
            at: at_days_ago(20),
        });
        assert_eq!(confirmed_at(&f), at_days_ago(20));
        f.verified_at = Some(at_days_ago(3));
        assert_eq!(confirmed_at(&f), at_days_ago(3));
        f.verified_at = Some(at_days_ago(45));
        assert_eq!(
            confirmed_at(&f),
            at_days_ago(20),
            "an older probe changes nothing"
        );
    }

    /// At 30 days: a person's fact written 31 days ago turns, one written
    /// 29 days ago does not, nor one verified since; the sync's and a
    /// scan's own of any age never do.
    #[test]
    fn what_turns_stale_at_the_cutoff() {
        let mut verified = fact("tls.names", SOURCE_PERSON, STATUS_ACTIVE, 90);
        verified.verified_at = Some(at_days_ago(1));
        let facts = vec![
            fact("address", SOURCE_PERSON, STATUS_ACTIVE, 31),
            fact("port", SOURCE_PERSON, STATUS_ACTIVE, 29),
            verified,
            fact("os", SOURCE_SYNC, STATUS_ACTIVE, 400),
            fact("checkout.path", SOURCE_SCAN, STATUS_ACTIVE, 400),
            fact("version", SOURCE_SESSION, STATUS_PROPOSED, 400),
            fact("owner", SOURCE_SESSION, STATUS_ACTIVE, 31),
        ];
        let cut = cutoff(DateTime::from_millis(NOW), 30);
        assert_eq!(cut, at_days_ago(30));
        assert_eq!(turning(&facts, cut), [0, 6]);
        assert!(turning(&facts, cutoff(DateTime::from_millis(NOW), 400)).is_empty());
    }

    /// A cutoff never wraps, however many days are asked: past what an i64
    /// holds, and within it but past what its milliseconds hold.
    #[test]
    fn a_huge_setting_reads_as_the_beginning_of_time() {
        for days in [u64::MAX, i64::MAX as u64, u64::MAX / 3] {
            let cut = cutoff(DateTime::from_millis(NOW), days);
            assert!(cut.timestamp_millis() <= 0, "{days} days: {cut}");
        }
    }
}
