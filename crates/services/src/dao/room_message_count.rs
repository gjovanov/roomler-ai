// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4448 — a room's stored `message_count`, set to its live messages.
//!
//! A room's `message_count` is the number of its live messages: the
//! `messages` documents whose `room_id` is the room and whose `deleted_at` is
//! null, thread replies and quotes included. [`MessageDao`] moves the count
//! with each message it writes ([`MessageDao::create_with_attachments`], +1)
//! and each one it deletes ([`MessageDao::soft_delete_in_room`], −1 when the
//! delete matched). The room's `cascade_delete` takes the count with the room.
//! Until #4448 nothing wrote the count, so every room made until then reads 0.
//! [`MessageCountRepair::pass`] sets each room's count to its live messages;
//! chat runs one pass at every boot of every pod (`room-message-count`, a
//! startup job that is not leader-gated).
//!
//! The pass is the member-count repair's
//! ([`super::room_member_count`]) with `messages` for `room_members`, and it
//! holds what that one holds:
//!
//! - It reads rooms in `_id` order, [`BATCH`] at a time with only `_id` and
//!   `message_count`. It stops after [`CAP`] rooms, or at the first batch
//!   boundary past [`BUDGET`]. A pass that stops early leaves the last room of
//!   the last batch it finished in the `locks` document [`CURSOR`], and the
//!   next pass begins after it; the pass that reaches the last room removes it.
//!   The batch is smaller than the member repair's because a room holds far
//!   more messages than members: a batch's `$group` reads every live message
//!   of its rooms, through the `{room_id, created_at}` index and a fetch of
//!   each document for its `deleted_at`.
//! - It counts a batch's live messages with one `$group` on `room_id`, after
//!   it has read the batch's counts.
//! - It writes only where a count differs from the live messages, only
//!   `message_count`, and only on a compare with the count it read. A post or
//!   a delete whose count lands after that read fails the compare, and the
//!   room is left for the next pass (`skipped`).
//! - Every read goes to the primary whatever the connection string prefers.
//!
//! What a pass cannot see is a post or a delete half done at the moment of
//! its write: the message written or deleted and counted, its count not yet
//! moved. That room ends one off, and the next pass sets it right. The same
//! holds for a message a pod of the old code writes during a roll, with no
//! count beside it: the pass of the last pod to boot counts it.
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
use mongodb::Collection;
use mongodb::Database;
use roomler_ai_db::models::{Message, Room};

use super::base::DaoResult;
use super::room_member_count::{count_of, on_the_primary};

/// Rooms read at a time.
pub const BATCH: usize = 100;
/// Rooms one pass reads at most.
pub const CAP: usize = 20_000;
/// A pass starts no new batch after this long.
pub const BUDGET: Duration = Duration::from_secs(10);
/// The `locks` document that holds where a pass that stopped early stopped.
pub const CURSOR: &str = "room_message_count";

/// What one pass did: the counts its one log line carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageCountPass {
    /// Rooms read.
    pub read: u64,
    /// Rooms whose count it set to their live messages.
    pub changed: u64,
    /// Rooms whose count differed from their live messages but moved between
    /// the read and the write, left for the next pass.
    pub skipped: u64,
    /// It stopped at its cap or its budget: the next pass begins after the
    /// last room it finished.
    pub more: bool,
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

    /// One pass: [`BATCH`] rooms at a time, [`CAP`] rooms and [`BUDGET`] at
    /// most.
    pub async fn pass(&self) -> DaoResult<MessageCountPass> {
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
    ) -> DaoResult<MessageCountPass> {
        let (batch, cap) = (batch.max(1), cap.max(1));
        let started = Instant::now();
        let started_after = self.cursor().await?;
        let mut after = started_after;
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
            .aggregate(vec![
                doc! { "$match": { "room_id": { "$in": ids.to_vec() }, "deleted_at": null } },
                doc! { "$group": { "_id": "$room_id", "live": { "$sum": 1 } } },
            ])
            .await?;
        let mut out = HashMap::with_capacity(ids.len());
        while let Some(group) = groups.try_next().await? {
            if let (Ok(id), Some(n)) = (group.get_object_id("_id"), count_of(group.get("live"))) {
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
