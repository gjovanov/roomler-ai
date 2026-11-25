// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4409 — a room's stored `member_count`, set back to its member rows.
//!
//! A room's `member_count` is the number of its member rows: the
//! `room_members` documents whose `room_id` is the room, which the member
//! route lists and counts as its `total`. Each writer of a row moves the count
//! with it ([`RoomDao::join`], the first call join of
//! [`RoomDao::join_participant`], [`RoomDao::leave`]), and a new room is
//! stored at 0 because its creator's join counts them. Before #4409 a new
//! room was stored at 1 as well, so every room made until then reads one
//! ahead. [`MemberCountRepair::pass`] sets each room's count to its rows;
//! chat runs one pass at every boot of every pod (`room-member-count`, a
//! startup job that is not leader-gated).
//!
//! A pass is bounded, and it ends with the same rows when it runs twice or on
//! two pods at once, beside a pod that is serving joins and leaves:
//!
//! - It reads rooms in `_id` order, [`BATCH`] at a time with only `_id` and
//!   `member_count`. It stops after [`CAP`] rooms, or at the first batch
//!   boundary past [`BUDGET`]. A pass that stops early leaves the last room of
//!   the last batch it finished in the `locks` document [`CURSOR`], and the
//!   next pass begins after it; the pass that reaches the last room removes it.
//! - It counts a batch's member rows with one `$group` on `room_id`, after it
//!   has read the batch's counts.
//! - It writes only where a count differs from the rows, only
//!   `member_count`, and only on a compare with the count it read: one
//!   `update_many` per pair of (count read, rows) in the batch, each filtered
//!   on `_id` in the pair's rooms AND `member_count` still the count read. A
//!   join or a leave whose count lands after that read fails the compare, and
//!   the room is left for the next pass. The order is the point: rows counted
//!   before the count is read would lose a join that lands between the two
//!   reads (rows 1; the join makes rows 2 and the count 3; the read sees 3;
//!   the write sets 1).
//!
//! What a pass cannot see is a join or a leave half done at the moment of its
//! write: the row already written or removed and counted, its count not yet
//! moved. That room ends one off, and the next pass sets it right.
//!
//! [`RoomDao::join`]: super::room::RoomDao::join
//! [`RoomDao::join_participant`]: super::room::RoomDao::join_participant
//! [`RoomDao::leave`]: super::room::RoomDao::leave

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use mongodb::{Collection, Database};
use roomler_ai_db::models::{Room, RoomMember};

use super::base::DaoResult;

/// Rooms read at a time.
pub const BATCH: usize = 500;
/// Rooms one pass reads at most.
pub const CAP: usize = 20_000;
/// A pass starts no new batch after this long.
pub const BUDGET: Duration = Duration::from_secs(10);
/// The `locks` document that holds where a pass that stopped early stopped.
pub const CURSOR: &str = "room_member_count";

/// What one pass did: the counts its one log line carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemberCountPass {
    /// Rooms read.
    pub read: u64,
    /// Rooms whose count it set to their rows.
    pub changed: u64,
    /// Rooms whose count differed from their rows but moved between the read
    /// and the write, left for the next pass.
    pub skipped: u64,
    /// It stopped at its cap or its budget: the next pass begins after the
    /// last room it finished.
    pub more: bool,
}

pub struct MemberCountRepair {
    rooms: Collection<Document>,
    members: Collection<Document>,
    locks: Collection<Document>,
}

impl MemberCountRepair {
    pub fn new(db: &Database) -> Self {
        Self {
            rooms: db.collection(Room::COLLECTION),
            members: db.collection(RoomMember::COLLECTION),
            locks: db.collection("locks"),
        }
    }

    /// One pass: [`BATCH`] rooms at a time, [`CAP`] rooms and [`BUDGET`] at
    /// most.
    pub async fn pass(&self) -> DaoResult<MemberCountPass> {
        self.pass_with(BATCH, CAP, BUDGET).await
    }

    /// One pass: `batch` rooms at a time (at least 1), `cap` rooms at most (at
    /// least 1), and no new batch once `budget` has run out. A pass always
    /// finishes its first batch, so a budget of zero still moves the cursor.
    pub async fn pass_with(
        &self,
        batch: usize,
        cap: usize,
        budget: Duration,
    ) -> DaoResult<MemberCountPass> {
        let (batch, cap) = (batch.max(1), cap.max(1));
        let started = Instant::now();
        let started_after = self.cursor().await?;
        let mut after = started_after;
        let mut out = MemberCountPass::default();
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
                .projection(doc! { "_id": 1, "member_count": 1 })
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
            let rows = self.rows_of(&ids).await?;
            // The rooms to write, grouped by the count read and the rows.
            let mut groups: Vec<(Option<&Bson>, i64, Vec<ObjectId>)> = Vec::new();
            for room in &rooms {
                let Ok(id) = room.get_object_id("_id") else {
                    continue;
                };
                let n = rows.get(&id).copied().unwrap_or(0);
                let read = room.get("member_count");
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
                out.changed += wrote;
                out.skipped += of.len() as u64 - wrote;
            }
            out.read += rooms.len() as u64;
            after = ids.last().copied();
            if rooms.len() < limit {
                break;
            }
        }
        if out.more {
            self.keep_cursor(after).await?;
        } else if started_after.is_some() {
            self.keep_cursor(None).await?;
        }
        Ok(out)
    }

    /// Set `member_count` to `rows` in each room of `ids` that still holds
    /// `read`, the value a pass read (`None`: no such field), and nothing else.
    /// How many it wrote.
    pub async fn set_if(&self, ids: &[ObjectId], read: Option<&Bson>, rows: i64) -> DaoResult<u64> {
        let held = match read {
            Some(v) => v.clone(),
            None => Bson::Document(doc! { "$exists": false }),
        };
        let res = self
            .rooms
            .update_many(
                doc! { "_id": { "$in": ids.to_vec() }, "member_count": held },
                doc! { "$set": { "member_count": rows } },
            )
            .await?;
        Ok(res.modified_count)
    }

    /// The member rows of each room in `ids` that has any.
    async fn rows_of(&self, ids: &[ObjectId]) -> DaoResult<HashMap<ObjectId, i64>> {
        let mut groups = self
            .members
            .aggregate(vec![
                doc! { "$match": { "room_id": { "$in": ids.to_vec() } } },
                doc! { "$group": { "_id": "$room_id", "rows": { "$sum": 1 } } },
            ])
            .await?;
        let mut out = HashMap::with_capacity(ids.len());
        while let Some(group) = groups.try_next().await? {
            if let (Ok(id), Some(n)) = (group.get_object_id("_id"), count_of(group.get("rows"))) {
                out.insert(id, n);
            }
        }
        Ok(out)
    }

    async fn cursor(&self) -> DaoResult<Option<ObjectId>> {
        Ok(self
            .locks
            .find_one(doc! { "_id": CURSOR })
            .await?
            .and_then(|d| d.get_object_id("after").ok()))
    }

    async fn keep_cursor(&self, after: Option<ObjectId>) -> DaoResult<()> {
        match after {
            Some(id) => {
                self.locks
                    .update_one(
                        doc! { "_id": CURSOR },
                        doc! { "$set": { "after": id, "at": DateTime::now() } },
                    )
                    .upsert(true)
                    .await?;
            }
            None => {
                self.locks.delete_one(doc! { "_id": CURSOR }).await?;
            }
        }
        Ok(())
    }
}

/// A stored count as a whole number: an integer, or a double with no fraction.
/// Anything else (missing, a fraction, a string) is no count, so it differs
/// from every number of rows and a pass sets it.
fn count_of(v: Option<&Bson>) -> Option<i64> {
    match v? {
        Bson::Int32(n) => Some(i64::from(*n)),
        Bson::Int64(n) => Some(*n),
        Bson::Double(f) if f.fract() == 0.0 => Some(*f as i64),
        _ => None,
    }
}
