// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4448 — a room's stored `message_count`, kept equal to its live messages.
//!
//! A room's `message_count` is the number of its live messages: the
//! `messages` documents whose `room_id` is the room and that match the live
//! predicate every reader of `messages` uses, `deleted_at: null` (null, or no
//! such field: a message stored before the field existed is live), thread
//! replies and quotes included. [`MessageDao`] is the
//! one writer of `messages`, and it moves the count with each message it
//! writes ([`MessageDao::create_with_attachments`], +1) and each one it deletes
//! ([`MessageDao::soft_delete_in_room`], −1 when the delete matched a live
//! message). The room's `cascade_delete` takes the count with the room. Until
//! #4448 nothing wrote the count, so every room made until then reads 0.
//!
//! [`MessageCountRepair`] sets each room's count back to its live messages. It
//! runs as SWEEPS over every room, each sweep made of bounded PASSES, and chat
//! drives it on two clocks:
//!
//! - at every boot of every pod, [`MessageCountRepair::begin_sweep`] and one
//!   pass (`room-message-count`, a startup job that is not leader-gated): a
//!   roll's last pod begins a sweep after the last process of the old code is
//!   gone, so it also counts what a pod of the old code wrote with no count
//!   beside it;
//! - on every pod's periodic job (`room-message-count-sweep`, every
//!   [`TICK`]), one pass whenever [`MessageCountRepair::due`]: a sweep is
//!   under way, or the last one finished more than [`RESWEEP`] ago. So a sweep
//!   that a pass's budget stopped finishes without waiting for a deploy, and a
//!   count a lost `$inc` left one off is set right within a day, boots or not.
//!
//! A pass is bounded, and it ends with the same rows when it runs twice or on
//! two pods at once, beside a pod that is serving posts and deletes:
//!
//! - It reads rooms in `_id` order, [`BATCH`] at a time with only `_id` and
//!   `message_count`. It stops after [`CAP`] rooms, or at the first batch
//!   boundary past [`BUDGET`].
//! - It counts a batch's live messages with one `$group` on `room_id`, after
//!   it has read the batch's counts, through the index [`LIVE_INDEX`]
//!   (`{room_id: 1, deleted_at: 1}`, named in a hint so the plan never
//!   depends on the planner's trial): a missing `deleted_at` and a null one
//!   are both keyed null, so it examines one index key per live message of
//!   the batch's rooms, and none of a deleted message or of another room.
//! - It writes only where a count differs from the live messages, only
//!   `message_count`, and only on a compare with the count it read. A post or
//!   a delete whose count lands after that read fails the compare, and the
//!   room is left for the next pass (`skipped`).
//! - Where it stands lives in the `locks` document [`CURSOR`]: `after`, the
//!   last room of the last batch it finished (null: the sweep begins at the
//!   first room; absent: no sweep under way), and `swept_at`, when the last
//!   sweep finished. A pass moves it only on a compare with what it read when
//!   it began, so two passes that began at the same place move it once, and a
//!   boot's new sweep is never undone by a pass that was already running.
//! - Every read goes to the primary whatever the connection string prefers.
//!
//! What a pass cannot see is a post or a delete half done at the moment of
//! its write: the message written or deleted and counted, its count not yet
//! moved. That room ends one off, and the next sweep sets it right.
//!
//! [`MessageDao`]: super::message::MessageDao
//! [`MessageDao::create_with_attachments`]: super::message::MessageDao::create_with_attachments
//! [`MessageDao::soft_delete_in_room`]: super::message::MessageDao::soft_delete_in_room

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use mongodb::Database;
use mongodb::{Collection, options::Hint};
use roomler_ai_db::models::{Message, Room};

use super::base::DaoResult;
use super::room_member_count::{count_of, on_the_primary};

/// Rooms read at a time.
pub const BATCH: usize = 100;
/// Rooms one pass reads at most.
pub const CAP: usize = 20_000;
/// A pass starts no new batch after this long.
pub const BUDGET: Duration = Duration::from_secs(5);
/// The `locks` document that holds where the sweep stands.
pub const CURSOR: &str = "room_message_count";
/// How often each pod asks whether a pass is due.
pub const TICK: Duration = Duration::from_secs(10 * 60);
/// A finished sweep is begun again after this long.
pub const RESWEEP: Duration = Duration::from_secs(24 * 60 * 60);
/// The index a pass counts live messages through, `{room_id: 1, deleted_at:
/// 1}` under the name Mongo gives it. Chat's `index_plan` declares it.
pub const LIVE_INDEX: &str = "room_id_1_deleted_at_1";

/// The pipeline that counts the live messages of each room of `ids`, run with
/// a hint on [`LIVE_INDEX`] (public for the test that reads its plan).
pub fn live_pipeline(ids: &[ObjectId]) -> Vec<Document> {
    vec![
        doc! { "$match": { "room_id": { "$in": ids.to_vec() }, "deleted_at": null } },
        doc! { "$group": { "_id": "$room_id", "live": { "$sum": 1 } } },
    ]
}

/// Where the sweep stands, as [`CURSOR`] holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sweep {
    /// No sweep under way; when the last one finished, if one ever did.
    Idle { swept_at: Option<DateTime> },
    /// A sweep under way: the next pass begins after this room (`None`: at
    /// the first room).
    At(Option<ObjectId>),
}

/// What one pass did: the counts its one log line carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageCountPass {
    /// Rooms read.
    pub read: u64,
    /// Rooms whose count it set to their live messages.
    pub changed: u64,
    /// By how much it raised the counts it set, summed (a stored count that
    /// is no number reads as 0 here).
    pub raised: u64,
    /// By how much it lowered the counts it set, summed.
    pub lowered: u64,
    /// Rooms whose count differed from their live messages but moved between
    /// the read and the write, left for the next pass.
    pub skipped: u64,
    /// It stopped at its cap or its budget: the sweep goes on from the last
    /// room it finished.
    pub more: bool,
    /// The sweep had moved since this pass began (another pass, or a boot's
    /// new sweep), so this pass left the cursor as it found it.
    pub cursor_moved: bool,
}

impl MessageCountPass {
    /// It set any count: in steady state a pass sets none, so a non-zero one
    /// says a writer missed a message (or a half-done post raced it).
    pub fn corrected(&self) -> bool {
        self.changed > 0
    }
}

pub struct MessageCountRepair {
    rooms: Collection<Document>,
    messages: Collection<Document>,
    locks: Collection<Document>,
}

impl MessageCountRepair {
    /// The repair over `db`'s rooms, messages and `locks`, each read on the
    /// primary whatever `db` prefers.
    pub fn new(db: &Database) -> Self {
        Self {
            rooms: on_the_primary(db, Room::COLLECTION),
            messages: on_the_primary(db, Message::COLLECTION),
            locks: on_the_primary(db, "locks"),
        }
    }

    /// Where the sweep stands.
    pub async fn sweep(&self) -> DaoResult<Sweep> {
        let held = self.locks.find_one(doc! { "_id": CURSOR }).await?;
        Ok(match held.as_ref().and_then(|d| d.get("after")) {
            Some(Bson::ObjectId(id)) => Sweep::At(Some(*id)),
            // Null, or anything else a hand could leave there: begin again.
            Some(_) => Sweep::At(None),
            None => Sweep::Idle {
                swept_at: held.and_then(|d| d.get_datetime("swept_at").ok().copied()),
            },
        })
    }

    /// Begin a new sweep at the first room, whatever the sweep held.
    pub async fn begin_sweep(&self) -> DaoResult<()> {
        self.locks
            .update_one(
                doc! { "_id": CURSOR },
                doc! { "$set": { "after": Bson::Null, "begun_at": DateTime::now() } },
            )
            .upsert(true)
            .await?;
        Ok(())
    }

    /// A pass is due at `now`: a sweep is under way, none ever finished, or
    /// the last one finished [`RESWEEP`] or more ago.
    pub async fn due(&self, now: DateTime) -> DaoResult<bool> {
        Ok(match self.sweep().await? {
            Sweep::At(_) => true,
            Sweep::Idle { swept_at: None } => true,
            Sweep::Idle { swept_at: Some(at) } => {
                now.timestamp_millis() - at.timestamp_millis() >= RESWEEP.as_millis() as i64
            }
        })
    }

    /// One pass: [`BATCH`] rooms at a time, [`CAP`] rooms and [`BUDGET`] at
    /// most.
    pub async fn pass(&self) -> DaoResult<MessageCountPass> {
        self.pass_with(BATCH, CAP, BUDGET).await
    }

    /// One pass: `batch` rooms at a time (at least 1), `cap` rooms at most (at
    /// least 1), and no new batch once `budget` has run out. A pass always
    /// finishes its first batch, so a budget of zero still moves the sweep.
    /// A pass that begins with no sweep under way begins one at the first
    /// room.
    pub async fn pass_with(
        &self,
        batch: usize,
        cap: usize,
        budget: Duration,
    ) -> DaoResult<MessageCountPass> {
        let began = self.sweep().await?;
        self.pass_from(began, batch, cap, budget).await
    }

    /// [`MessageCountRepair::pass_with`] from `began`, the sweep as the pass
    /// read it when it began: it moves the sweep only if it still stands
    /// there. Public so a test can replay a pass that read the sweep before
    /// another pass moved it.
    pub async fn pass_from(
        &self,
        began: Sweep,
        batch: usize,
        cap: usize,
        budget: Duration,
    ) -> DaoResult<MessageCountPass> {
        let (batch, cap) = (batch.max(1), cap.max(1));
        let started = Instant::now();
        let mut after = match began {
            Sweep::At(after) => after,
            Sweep::Idle { .. } => None,
        };
        let mut out = MessageCountPass::default();
        loop {
            let left = cap - out.read as usize;
            if left == 0 || (out.read > 0 && started.elapsed() >= budget) {
                out.more = true;
                break;
            }
            let limit = batch.min(left);
            let filter = match after {
                Some(id) => doc! { "_id": { "$gt": id } },
                None => doc! {},
            };
            let rooms: Vec<Document> = self
                .rooms
                .find(filter)
                .projection(doc! { "_id": 1, "message_count": 1 })
                .sort(doc! { "_id": 1 })
                .limit(limit as i64)
                .await?
                .try_collect()
                .await?;
            let ids: Vec<ObjectId> = rooms
                .iter()
                .filter_map(|r| r.get_object_id("_id").ok())
                .collect();
            if ids.is_empty() {
                break;
            }
            // Counted after the batch's counts are read: the order is what
            // keeps a post that lands between the two reads (the member
            // repair's module doc walks the case).
            let live = self.live_of(&ids).await?;
            let mut groups: Vec<(Option<&Bson>, i64, Vec<ObjectId>)> = Vec::new();
            for room in &rooms {
                let Ok(id) = room.get_object_id("_id") else {
                    continue;
                };
                let n = live.get(&id).copied().unwrap_or(0);
                let read = room.get("message_count");
                if count_of(read) == Some(n) {
                    continue;
                }
                match groups.iter_mut().find(|(r, m, _)| *r == read && *m == n) {
                    Some((_, _, of)) => of.push(id),
                    None => groups.push((read, n, vec![id])),
                }
            }
            for (read, n, of) in &groups {
                let wrote = self.set_if(of, *read, *n).await?;
                let was = count_of(*read).unwrap_or(0);
                let by = (*n - was).unsigned_abs() * wrote;
                if *n > was {
                    out.raised += by;
                } else {
                    out.lowered += by;
                }
                out.changed += wrote;
                out.skipped += of.len() as u64 - wrote;
            }
            out.read += rooms.len() as u64;
            after = ids.last().copied();
            if rooms.len() < limit {
                break;
            }
        }
        let next = if out.more {
            doc! { "$set": { "after": after.map_or(Bson::Null, Bson::ObjectId), "at": DateTime::now() } }
        } else {
            doc! { "$unset": { "after": "" }, "$set": { "swept_at": DateTime::now() } }
        };
        out.cursor_moved = !self.move_sweep(began, next).await?;
        Ok(out)
    }

    /// Set `message_count` to `live` in each room of `ids` that still holds
    /// `read`, the value a pass read (`None`: no such field), and nothing
    /// else. How many it wrote.
    pub async fn set_if(&self, ids: &[ObjectId], read: Option<&Bson>, live: i64) -> DaoResult<u64> {
        let held = match read {
            Some(v) => v.clone(),
            None => Bson::Document(doc! { "$exists": false }),
        };
        let res = self
            .rooms
            .update_many(
                doc! { "_id": { "$in": ids.to_vec() }, "message_count": held },
                doc! { "$set": { "message_count": live } },
            )
            .await?;
        Ok(res.modified_count)
    }

    /// The live messages of each room in `ids` that has any.
    async fn live_of(&self, ids: &[ObjectId]) -> DaoResult<HashMap<ObjectId, i64>> {
        let mut groups = self
            .messages
            .aggregate(live_pipeline(ids))
            .hint(Hint::Name(LIVE_INDEX.to_string()))
            .await?;
        let mut out = HashMap::with_capacity(ids.len());
        while let Some(group) = groups.try_next().await? {
            if let (Ok(id), Some(n)) = (group.get_object_id("_id"), count_of(group.get("live"))) {
                out.insert(id, n);
            }
        }
        Ok(out)
    }

    /// Apply `update` to the sweep if it still stands where it stood at
    /// `began`. `false` when it had moved.
    async fn move_sweep(&self, began: Sweep, update: Document) -> DaoResult<bool> {
        let at = match began {
            Sweep::Idle { .. } => doc! { "$exists": false },
            Sweep::At(None) => doc! { "$type": "null" },
            Sweep::At(Some(id)) => doc! { "$eq": id },
        };
        match self
            .locks
            .update_one(doc! { "_id": CURSOR, "after": at }, update)
            .upsert(true)
            .await
        {
            Ok(r) => Ok(r.matched_count > 0 || r.upserted_id.is_some()),
            // The document is there and stands elsewhere: the upsert's insert
            // collided on `_id`.
            Err(e) if is_duplicate_key(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

fn is_duplicate_key(e: &mongodb::error::Error) -> bool {
    matches!(
        *e.kind,
        mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteError(ref w))
            if w.code == 11000
    )
}
