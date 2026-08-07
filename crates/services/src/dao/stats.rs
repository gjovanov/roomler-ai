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
pub const STATS_MESH: &str = "stats_mesh";
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
