// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Observability sample sinks (stats PR-1).
//!
//! All sample collections use deterministic string `_id`s
//! (`"{key}:{bucket_start_unix}"`) so every writer is an **idempotent
//! upsert** — with two API pods that is the whole concurrency story: the
//! same tick from both pods lands on one document, and the loser of the
//! rare first-insert race gets E11000 and retries once as a plain update.
//!
//! The relay-sample pair encodes a monotonic healthy-vote: the success
//! path does a full `$set`, the failure path `$setOnInsert`s only — so a
//! pod that CAN'T reach a PoP never overwrites the sample of a pod that
//! could ("healthy if any pod reached it", matching the load poller's
//! fail-open philosophy).

use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use mongodb::{Collection, Database};

use super::base::DaoResult;

pub const STATS_RELAY: &str = "stats_relay";
pub const STATS_MACHINE: &str = "stats_machine";
pub const STATS_EVENTS: &str = "stats_events";
pub const STATS_CALL: &str = "stats_call";
/// Per-PARTICIPANT call throughput (wave 3). `stats_call` answers "how
/// busy was this room"; this answers "how much did this user move", which
/// is what per-user usage accounting needs and the room-level bucket can
/// never be disaggregated back into.
pub const STATS_CALL_USER: &str = "stats_call_user";
pub const STATS_CALL_USER_1H: &str = "stats_call_user_1h";
pub const STATS_CALL_USER_1D: &str = "stats_call_user_1d";
pub const STATS_MESH: &str = "stats_mesh";
/// FR-20 — the cost ledger. One bucket per `(tenant, meter, minute)`.
///
/// ⚠ **Only server-measured quantities go in here.** `tunnel_audit`'s byte
/// columns are reported by the client endpoint on flow close — the payload is
/// P2P, so the server never saw it — which makes them a *claim by a host we do
/// not control*. Same distinction as `ssh_audit` (our decision, authoritative)
/// versus `ssh_activity` (the device's account of itself), and the same rule:
/// **never fold them together.** Client-reported bytes stay in analytics,
/// labelled as such; only what we measured ourselves may drive cost.
///
/// ⚠ **Never metered:** direct P2P sessions, device count, signalling, mesh
/// coordination, chat. A direct session costs the control plane kilobytes of
/// signalling — metering it would invert the growth model and bill against
/// exactly the outcome the NAT-traversal work exists to produce.
pub const STATS_USAGE: &str = "stats_usage";
pub const STATS_USAGE_1H: &str = "stats_usage_1h";
pub const STATS_USAGE_1D: &str = "stats_usage_1d";
pub const CALL_SESSIONS: &str = "call_sessions";
pub const STATS_META: &str = "stats_meta";

/// Raw-sample bucket widths (seconds).
pub const RELAY_BUCKET_SECS: i64 = 30;
pub const MACHINE_BUCKET_SECS: i64 = 60;
pub const CALL_BUCKET_SECS: i64 = 30;

/// Round a unix-seconds timestamp down to its bucket start.
pub fn bucket_start(unix_secs: i64, bucket_secs: i64) -> i64 {
    unix_secs - unix_secs.rem_euclid(bucket_secs)
}

/// FR-20 usage buckets are one minute wide. Narrow enough that a pod restart
/// loses at most a minute of a tenant's bytes, wide enough that the flush is a
/// handful of upserts rather than a write per frame.
pub const USAGE_BUCKET_SECS: i64 = 60;

/// A cost driver. The wire/storage spelling is the `snake_case` variant name,
/// so adding one is a `match` arm the compiler demands.
///
/// ⚠ Deliberately NOT a free-form string: a typo'd meter name would create a
/// silent second ledger line that sums to nothing and reconciles against
/// nothing, and it would look exactly like a tenant with no usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Meter {
    /// Relay bytes forwarded on this tenant's behalf by an API pod's `/derp`.
    DerpBytes,
    /// coturn-relayed bytes (FR-20 P3).
    TurnBytes,
    /// The SFU's real marginal cost (FR-20 P4).
    SfuParticipantSeconds,
}

impl Meter {
    pub fn as_str(self) -> &'static str {
        match self {
            Meter::DerpBytes => "derp_bytes",
            Meter::TurnBytes => "turn_bytes",
            Meter::SfuParticipantSeconds => "sfu_participant_seconds",
        }
    }
}

/// One successful relay-PoP `/stats` poll, ready to persist.
#[derive(Debug, Clone, Default)]
pub struct RelaySample {
    pub region: String,
    /// Sample time, unix seconds (bucketed to [`RELAY_BUCKET_SECS`]).
    pub unix: i64,
    pub poll_rtt_ms: u32,
    pub cpus: f64,
    pub load1: f64,
    pub load5: f64,
    pub mem_total_kb: f64,
    pub mem_available_kb: f64,
    pub rx_mbps: f64,
    pub tx_mbps: f64,
    pub allocations: f64,
    pub coturn_sessions: f64,
    pub derp_registrations: f64,
    pub uptime_s: f64,
}

pub struct StatsDao {
    db: Database,
}

impl StatsDao {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }

    pub fn coll(&self, name: &str) -> Collection<Document> {
        self.db.collection::<Document>(name)
    }

    /// Upsert, retrying exactly once on the concurrent-first-insert E11000
    /// (the retry finds the winner's document and becomes a plain update).
    async fn upsert(&self, coll: &str, filter: Document, update: Document) -> DaoResult<()> {
        let c = self.coll(coll);
        match c
            .update_one(filter.clone(), update.clone())
            .upsert(true)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_dup_key(&e) => {
                c.update_one(filter, update).upsert(true).await?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// FR-20 — add `value` to a tenant's meter for the bucket containing `unix`.
    ///
    /// The `_id` is deterministic (`{tenant}:{meter}:{bucket}`), so both pods
    /// address the same document and Mongo's `$inc` sums their contributions
    /// atomically. That, not a lease, is what makes the 2-pod deployment
    /// race-free — the same property `stats_relay` and `stats_machine` rely on.
    ///
    /// ⚠ `$inc` is additive, not idempotent: a **retried** flush would
    /// double-count. So a failed flush is dropped, never retried, and the
    /// bucket under-reports. That is the same trade the cumulative PoP and
    /// host-total counters already make, and it is the right direction — a
    /// cost ledger that occasionally under-bills is recoverable; one that
    /// double-bills is a refund and a trust problem.
    pub async fn add_usage(
        &self,
        tenant_id: ObjectId,
        meter: Meter,
        unix: i64,
        value: i64,
    ) -> DaoResult<()> {
        if value <= 0 {
            return Ok(());
        }
        let bucket = bucket_start(unix, USAGE_BUCKET_SECS);
        let id = format!("{}:{}:{}", tenant_id.to_hex(), meter.as_str(), bucket);
        self.upsert(
            STATS_USAGE,
            doc! { "_id": &id },
            doc! {
                "$inc": { "value": value },
                // Immutable identity, written once. Kept out of `$inc` so a
                // concurrent writer cannot rewrite what the bucket IS.
                "$setOnInsert": {
                    "tenant_id": tenant_id,
                    "meter": meter.as_str(),
                    "ts": DateTime::from_millis(bucket * 1000),
                },
            },
        )
        .await
    }

    /// Successful poll → full `$set` (overwrites a failure-only bucket).
    pub async fn upsert_relay_sample(&self, s: &RelaySample) -> DaoResult<()> {
        let bucket = bucket_start(s.unix, RELAY_BUCKET_SECS);
        let id = format!("{}:{}", s.region, bucket);
        let ts = DateTime::from_millis(bucket * 1000);
        self.upsert(
            STATS_RELAY,
            doc! { "_id": &id },
            doc! { "$set": {
                "region": &s.region,
                "ts": ts,
                "healthy": true,
                "poll_rtt_ms": s.poll_rtt_ms as i64,
                "cpus": s.cpus,
                "load1": s.load1,
                "load5": s.load5,
                "mem_total_kb": s.mem_total_kb,
                "mem_available_kb": s.mem_available_kb,
                "rx_mbps": s.rx_mbps,
                "tx_mbps": s.tx_mbps,
                "allocations": s.allocations,
                "coturn_sessions": s.coturn_sessions,
                "derp_registrations": s.derp_registrations,
                "uptime_s": s.uptime_s,
            }},
        )
        .await
    }

    /// Failed poll → `$setOnInsert` ONLY, so it can never clobber a
    /// success recorded by the other pod for the same bucket.
    pub async fn upsert_relay_unreachable(&self, region: &str, unix: i64) -> DaoResult<()> {
        let bucket = bucket_start(unix, RELAY_BUCKET_SECS);
        let id = format!("{region}:{bucket}");
        let ts = DateTime::from_millis(bucket * 1000);
        self.upsert(
            STATS_RELAY,
            doc! { "_id": &id },
            doc! { "$setOnInsert": {
                "region": region,
                "ts": ts,
                "healthy": false,
            }},
        )
        .await
    }

    /// Per-agent minute bucket from the heartbeat handler. `sys` is the
    /// v2 telemetry block (None until the fleet ships it — legacy
    /// hardcoded-zero scalars are deliberately NOT persisted so "no data"
    /// stays distinguishable from a real zero).
    pub async fn upsert_machine_sample(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        unix: i64,
        active_sessions: u8,
        sys: Option<Document>,
    ) -> DaoResult<()> {
        let bucket = bucket_start(unix, MACHINE_BUCKET_SECS);
        let id = format!("{}:{}", agent_id.to_hex(), bucket);
        let ts = DateTime::from_millis(bucket * 1000);
        let mut set = doc! {
            "tenant_id": tenant_id,
            "agent_id": agent_id,
            "ts": ts,
            "online": true,
            "active_sessions": active_sessions as i32,
        };
        if let Some(sys) = sys {
            set.insert("sys", sys);
        }
        self.upsert(STATS_MACHINE, doc! { "_id": &id }, doc! { "$set": set })
            .await
    }

    /// Wave 3 — ONE participant's throughput inside a live call.
    ///
    /// Rates, not cumulative byte counters: a participant's transports are
    /// recreated on rejoin (and on an ICE restart), so a running total
    /// would step backwards and any `$max` would freeze at the pre-churn
    /// peak. The read side integrates instead — `Σ bps × bucket / 8` — which
    /// is churn-proof and matches how the room-level series is already read.
    ///
    /// Directions are named from the USER's point of view, not the SFU's:
    /// `up` is what they sent, `down` what they received.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_call_user_sample(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        call_id: Option<ObjectId>,
        user_id: ObjectId,
        unix: i64,
        up_bps: f64,
        down_bps: f64,
    ) -> DaoResult<()> {
        let bucket = bucket_start(unix, CALL_BUCKET_SECS);
        let id = format!("{}:{}:{}", room_id.to_hex(), user_id.to_hex(), bucket);
        let mut set = doc! {
            "tenant_id": tenant_id,
            "room_id": room_id,
            "user_id": user_id,
            "ts": DateTime::from_millis(bucket * 1000),
            "up_bps": up_bps,
            "down_bps": down_bps,
        };
        if let Some(cid) = call_id {
            set.insert("call_id", cid);
        }
        self.upsert(STATS_CALL_USER, doc! { "_id": &id }, doc! { "$set": set })
            .await
    }

    /// Wave 2 — this agent's view of the overlay mesh, replaced whole on
    /// every heartbeat (`_id` = the agent hex, so there is exactly one
    /// row per agent and no growth). The graph reader merges the two
    /// ends' opinions of each edge; keeping them SEPARATE here is what
    /// makes that possible — and a disagreement is itself diagnostic.
    pub async fn upsert_mesh_snapshot(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        links: &[Document],
    ) -> DaoResult<()> {
        self.upsert(
            STATS_MESH,
            doc! { "_id": agent_id.to_hex() },
            doc! { "$set": {
                "tenant_id": tenant_id,
                "agent_id": agent_id,
                "ts": DateTime::now(),
                "links": links,
            }},
        )
        .await
    }

    /// Presence transition ledger entry. The caller (`note_transition`)
    /// already won the `agents.last_presence` CAS, so this is
    /// exactly-once across pods by construction.
    pub async fn append_presence_event(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        presence: &str,
    ) -> DaoResult<()> {
        self.coll(STATS_EVENTS)
            .insert_one(doc! {
                "tenant_id": tenant_id,
                "agent_id": agent_id,
                "ts": DateTime::now(),
                "presence": presence,
            })
            .await?;
        Ok(())
    }

    // ── call_sessions lifecycle (stats PR-2) ─────────────────────────────

    /// One document per call INSTANCE, `_id` = the room's
    /// `current_call_id`. Insert-once: the transition-gated `start_call`
    /// already guarantees a single winner, and a duplicate insert (retry)
    /// is swallowed as success.
    pub async fn create_call_session(
        &self,
        call_id: ObjectId,
        tenant_id: ObjectId,
        room_id: ObjectId,
        started_by: ObjectId,
        started_at: DateTime,
    ) -> DaoResult<()> {
        match self
            .coll(CALL_SESSIONS)
            .insert_one(doc! {
                "_id": call_id,
                "tenant_id": tenant_id,
                "room_id": room_id,
                "started_by": started_by,
                "started_at": started_at,
                "ended_at": Bson::Null,
                "peak_participants": 0_i32,
                "participant_seconds": 0_i64,
            })
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_dup_key(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Close a call — three closers can race (explicit end, last-leaver
    /// auto-end, stale reset); the `ended_at: null` filter picks ONE
    /// winner. Returns whether this caller closed it.
    pub async fn close_call_session(&self, call_id: ObjectId, reason: &str) -> DaoResult<bool> {
        let r = self
            .coll(CALL_SESSIONS)
            .update_one(
                doc! { "_id": call_id, "ended_at": Bson::Null },
                doc! { "$set": { "ended_at": DateTime::now(), "end_reason": reason } },
            )
            .await?;
        Ok(r.modified_count > 0)
    }

    /// The call's accounting window: `(started_at, ended_at)`.
    pub async fn call_window(
        &self,
        call_id: ObjectId,
    ) -> DaoResult<Option<(DateTime, Option<DateTime>)>> {
        let found = self
            .coll(CALL_SESSIONS)
            .find_one(doc! { "_id": call_id })
            .await?;
        Ok(found.map(|d| {
            let started = d
                .get_datetime("started_at")
                .copied()
                .unwrap_or_else(|_| DateTime::now());
            let ended = d.get_datetime("ended_at").ok().copied();
            (started, ended)
        }))
    }

    pub async fn add_call_participant_seconds(
        &self,
        call_id: ObjectId,
        secs: i64,
    ) -> DaoResult<()> {
        self.coll(CALL_SESSIONS)
            .update_one(
                doc! { "_id": call_id },
                doc! { "$inc": { "participant_seconds": secs } },
            )
            .await?;
        Ok(())
    }

    /// `$max` the per-call participant peak from the media sampler's gauge
    /// (consistent with the rest of the call stats; the join-path
    /// room-level `$max` is ±1 under races).
    pub async fn max_call_peak(&self, call_id: ObjectId, participants: i32) -> DaoResult<()> {
        self.coll(CALL_SESSIONS)
            .update_one(
                doc! { "_id": call_id },
                doc! { "$max": { "peak_participants": participants } },
            )
            .await?;
        Ok(())
    }

    /// Read a rollup watermark (unix ms), if one was ever written.
    pub async fn rollup_watermark(&self, key: &str) -> DaoResult<Option<i64>> {
        let found = self.coll(STATS_META).find_one(doc! { "_id": key }).await?;
        Ok(found.and_then(|d| d.get_i64("watermark_ms").ok()))
    }

    /// Advance a rollup watermark — called only AFTER the family's
    /// `$merge` completed, so a death mid-run re-merges idempotently.
    pub async fn set_rollup_watermark(&self, key: &str, watermark_ms: i64) -> DaoResult<()> {
        self.upsert(
            STATS_META,
            doc! { "_id": key },
            doc! { "$set": { "watermark_ms": watermark_ms } },
        )
        .await
    }
}

fn is_dup_key(e: &mongodb::error::Error) -> bool {
    match &*e.kind {
        mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteError(we)) => {
            we.code == 11000
        }
        mongodb::error::ErrorKind::Command(ce) => ce.code == 11000,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::bucket_start;

    #[test]
    fn bucket_start_floors_to_bucket_boundary() {
        assert_eq!(bucket_start(1754400029, 30), 1754400000);
        assert_eq!(bucket_start(1754400030, 30), 1754400030);
        assert_eq!(bucket_start(1754400059, 60), 1754400000);
        // Determinism across writers is the whole point: same second →
        // same bucket id on both pods.
        assert_eq!(bucket_start(1754400015, 30), bucket_start(1754400029, 30));
    }
}

#[cfg(test)]
mod fr20_tests {
    use super::*;

    /// The ledger `_id` is the whole concurrency story: both pods must compute
    /// the SAME id for the same (tenant, meter, minute) so Mongo's `$inc` sums
    /// their contributions instead of them racing. If this ever became
    /// non-deterministic, the two pods would write two rows and the bill would
    /// silently halve per pod.
    #[test]
    fn the_usage_id_is_deterministic_per_tenant_meter_minute() {
        let t = ObjectId::parse_str("69a1dbbad2000f26adc875ce").unwrap();
        let id = |unix| {
            format!(
                "{}:{}:{}",
                t.to_hex(),
                Meter::DerpBytes.as_str(),
                bucket_start(unix, USAGE_BUCKET_SECS)
            )
        };
        // Same minute, different seconds, different pods ⇒ one document.
        assert_eq!(id(1754400001), id(1754400059));
        // Next minute ⇒ a new document.
        assert_ne!(id(1754400059), id(1754400060));
        assert!(id(1754400001).ends_with(":derp_bytes:1754400000"));
    }

    /// A meter's stored spelling is a persisted key: renaming one orphans every
    /// historical bucket, and the new name reads as a tenant with no usage
    /// rather than as an error.
    #[test]
    fn meter_wire_names_are_locked() {
        assert_eq!(Meter::DerpBytes.as_str(), "derp_bytes");
        assert_eq!(Meter::TurnBytes.as_str(), "turn_bytes");
        assert_eq!(
            Meter::SfuParticipantSeconds.as_str(),
            "sfu_participant_seconds"
        );
    }

    /// Two meters for one tenant in one minute are two ledger lines, never one.
    #[test]
    fn meters_do_not_collide_in_one_bucket() {
        let t = ObjectId::new();
        let b = bucket_start(1754400001, USAGE_BUCKET_SECS);
        let derp = format!("{}:{}:{}", t.to_hex(), Meter::DerpBytes.as_str(), b);
        let turn = format!("{}:{}:{}", t.to_hex(), Meter::TurnBytes.as_str(), b);
        assert_ne!(derp, turn);
    }
}
