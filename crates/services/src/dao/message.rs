// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_db::models::{
    AuthorType, ContentType, Mentions, Message, MessageAttachment, MessageType, ReactionSummary,
};

use super::base::{BaseDao, DaoError, DaoResult, PaginatedResult, PaginationParams};

pub struct MessageDao {
    pub base: BaseDao<Message>,
}

impl MessageDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, Message::COLLECTION),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        author_id: ObjectId,
        content: String,
        thread_id: Option<ObjectId>,
        referenced_message_id: Option<ObjectId>,
        nonce: Option<String>,
        mentions: Option<Mentions>,
    ) -> DaoResult<Message> {
        self.create_with_attachments(
            tenant_id,
            room_id,
            author_id,
            content,
            thread_id,
            referenced_message_id,
            nonce,
            mentions,
            Vec::new(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_attachments(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        author_id: ObjectId,
        content: String,
        thread_id: Option<ObjectId>,
        referenced_message_id: Option<ObjectId>,
        nonce: Option<String>,
        mentions: Option<Mentions>,
        attachments: Vec<MessageAttachment>,
    ) -> DaoResult<Message> {
        // The thread a reply joins and the message it quotes are live messages
        // of this same room. Any other id answers `NotFound`, as an id that
        // names nothing does, before anything is written.
        for named in [thread_id, referenced_message_id].into_iter().flatten() {
            self.find_live_in_room(tenant_id, room_id, named).await?;
        }

        let now = DateTime::now();
        let message_type = if referenced_message_id.is_some() {
            MessageType::Reply
        } else {
            MessageType::Default
        };

        let message = Message {
            id: None,
            tenant_id,
            room_id,
            thread_id,
            is_thread_root: false,
            thread_metadata: None,
            author_id,
            author_type: AuthorType::User,
            author_display: None,
            binding: None,
            content,
            content_type: ContentType::Markdown,
            message_type,
            embeds: Vec::new(),
            attachments,
            mentions: mentions.unwrap_or_default(),
            reaction_summary: Vec::new(),
            referenced_message_id,
            is_pinned: false,
            is_edited: false,
            edited_at: None,
            nonce,
            readby: vec![author_id], // Author has read their own message
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };

        let id = self.base.insert_one(&message).await?;

        // Update thread metadata on parent message when a thread reply is created
        if let Some(parent_id) = thread_id {
            let _ = self
                .update_thread_metadata(tenant_id, room_id, parent_id, author_id)
                .await;
        }

        self.base.find_by_id(id).await
    }

    /// The live message `id` of `room_id` in `tenant_id`. A message of another
    /// room or tenant, a deleted one and an id that names nothing all answer
    /// the same `NotFound`, so the answer does not say which.
    pub async fn find_live_in_room(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        id: ObjectId,
    ) -> DaoResult<Message> {
        self.base
            .find_one(doc! {
                "_id": id,
                "tenant_id": tenant_id,
                "room_id": room_id,
                "deleted_at": null,
            })
            .await?
            .ok_or(DaoError::NotFound)
    }

    /// FR-32 — live messages in the tenant, for **reporting only**.
    ///
    /// `max_message_history` is deliberately NOT gated. It is a retention
    /// bound, not a cap on an action, and the three ways to enforce one are all
    /// bad in P1: deleting destroys customer data irreversibly, refusing writes
    /// turns a chat product read-only, and bounding reads honestly means
    /// touching `find_in_room`, `find_pinned`, `find_thread_replies`, both
    /// export paths and full-text search — miss the exports and the limit is
    /// theatre, since anyone can dump the full history to PDF.
    ///
    /// But P1's question — *who would be denied* — needs no gate to answer.
    /// This count answers it, so P2 and P3 get their input at the cost of one
    /// query instead of a six-call-site retention view built on a limit that
    /// may not survive re-pricing (messages are small documents; this limit has
    /// almost no cost basis and exists for upgrade pressure).
    pub async fn count_for_tenant(&self, tenant_id: ObjectId) -> DaoResult<u64> {
        self.base
            .count(doc! { "tenant_id": tenant_id, "deleted_at": null })
            .await
    }

    pub async fn find_in_room(
        &self,
        room_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<Message>> {
        let mut filter = doc! { "room_id": room_id, "deleted_at": null, "thread_id": null };

        // Support cursor-based pagination via `before` timestamp
        if let Some(ref before) = params.before
            && let Ok(dt) = bson::DateTime::parse_rfc3339_str(before)
        {
            filter.insert("created_at", doc! { "$lt": dt });
        }

        self.base
            .find_paginated(filter, Some(doc! { "created_at": -1 }), params)
            .await
    }

    /// The live replies of the thread rooted at `thread_id`, from the root's
    /// own room: pass the root's tenant and room.
    pub async fn find_thread_replies(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        thread_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<Message>> {
        self.base
            .find_paginated(
                doc! {
                    "thread_id": thread_id,
                    "tenant_id": tenant_id,
                    "room_id": room_id,
                    "deleted_at": null,
                },
                Some(doc! { "created_at": 1 }),
                params,
            )
            .await
    }

    pub async fn find_pinned(&self, room_id: ObjectId) -> DaoResult<Vec<Message>> {
        self.base
            .find_many(
                doc! { "room_id": room_id, "is_pinned": true, "deleted_at": null },
                Some(doc! { "created_at": -1 }),
            )
            .await
    }

    /// The author's edit of a live message of `room_id`. `false` when no such
    /// message matched: another author's, another room's, or a deleted one.
    pub async fn update_content(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        message_id: ObjectId,
        author_id: ObjectId,
        content: String,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": message_id,
                    "tenant_id": tenant_id,
                    "room_id": room_id,
                    "author_id": author_id,
                    "deleted_at": null,
                },
                doc! {
                    "$set": {
                        "content": content,
                        "is_edited": true,
                        "edited_at": DateTime::now(),
                    }
                },
            )
            .await
    }

    /// Pin or unpin a live message of `room_id`. `false` when no live message
    /// matched: another room's or tenant's, or a deleted one, which a delete
    /// that landed after the route's guard leaves (#3327). The route answers
    /// 404 for it. Matched, not modified: pinning a pinned message is still a
    /// pin of a live message.
    pub async fn toggle_pin(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        message_id: ObjectId,
        pinned: bool,
    ) -> DaoResult<bool> {
        let written = self
            .base
            .collection()
            .update_one(
                doc! {
                    "_id": message_id,
                    "tenant_id": tenant_id,
                    "room_id": room_id,
                    "deleted_at": null,
                },
                doc! { "$set": { "is_pinned": pinned, "updated_at": DateTime::now() } },
            )
            .await?;
        Ok(written.matched_count > 0)
    }

    /// Atomically update thread metadata on the parent message when a reply is
    /// created. Only a live parent in the reply's own tenant and room is
    /// written.
    pub async fn update_thread_metadata(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        parent_id: ObjectId,
        reply_author_id: ObjectId,
    ) -> DaoResult<bool> {
        let now = DateTime::now();
        let parent = doc! {
            "_id": parent_id,
            "tenant_id": tenant_id,
            "room_id": room_id,
            "deleted_at": null,
        };
        let mut parent_without_metadata = parent.clone();
        parent_without_metadata.insert("thread_metadata", bson::Bson::Null);
        // First, ensure thread_metadata is not null (MongoDB $inc/$addToSet fail on null subdocs)
        let _ = self
            .base
            .collection()
            .update_one(
                parent_without_metadata,
                doc! { "$set": {
                    "thread_metadata": {
                        "reply_count": 0_i32,
                        "last_reply_at": null,
                        "last_reply_user_id": null,
                        "participant_ids": [],
                    },
                }},
            )
            .await;
        // Now safely increment/update the nested fields
        self.base
            .update_one(
                parent,
                doc! {
                    "$set": {
                        "is_thread_root": true,
                        "thread_metadata.last_reply_at": now,
                        "thread_metadata.last_reply_user_id": reply_author_id,
                    },
                    "$inc": {
                        "thread_metadata.reply_count": 1_i32,
                    },
                    "$addToSet": {
                        "thread_metadata.participant_ids": reply_author_id,
                    },
                },
            )
            .await
    }

    /// Recount every thread root's metadata from its stored replies. The host
    /// runs it at boot: it was written to repair roots whose `reply_count` was
    /// never set (MongoDB `$inc` fails on a null subdocument), and it rewrites
    /// every root's metadata each time. A reply counts toward the root it
    /// names only when both are in the same tenant and room. Returns how many
    /// roots were rewritten.
    pub async fn rebuild_thread_metadata(&self) -> DaoResult<u64> {
        use futures::TryStreamExt;
        let msgs_coll = self.base.collection().clone_with_type::<bson::Document>();
        let pipeline = vec![
            doc! { "$match": { "thread_id": { "$ne": null } } },
            doc! { "$group": {
                "_id": {
                    "thread_id": "$thread_id",
                    "tenant_id": "$tenant_id",
                    "room_id": "$room_id",
                },
                "reply_count": { "$sum": 1 },
                "last_reply_at": { "$max": "$created_at" },
                "last_reply_user_id": { "$last": "$author_id" },
                "participant_ids": { "$addToSet": "$author_id" },
            }},
        ];
        let mut fixed = 0u64;
        let mut cursor = msgs_coll.aggregate(pipeline).await?;
        while let Ok(Some(group)) = cursor.try_next().await {
            let Ok(key) = group.get_document("_id") else {
                continue;
            };
            let (Ok(parent_id), Ok(tenant_id), Ok(room_id), Ok(count)) = (
                key.get_object_id("thread_id"),
                key.get_object_id("tenant_id"),
                key.get_object_id("room_id"),
                group.get_i32("reply_count"),
            ) else {
                continue;
            };
            let update = doc! {
                "$set": {
                    "is_thread_root": true,
                    "thread_metadata": {
                        "reply_count": count,
                        "last_reply_at": group.get("last_reply_at"),
                        "last_reply_user_id": group.get("last_reply_user_id"),
                        "participant_ids": group.get("participant_ids"),
                    },
                },
            };
            // The root in the replies' own tenant and room; replies grouped
            // under any other tenant or room match no root.
            if let Ok(written) = msgs_coll
                .update_one(
                    doc! { "_id": parent_id, "tenant_id": tenant_id, "room_id": room_id },
                    update,
                )
                .await
                && written.matched_count > 0
            {
                fixed += 1;
            }
        }
        Ok(fixed)
    }

    /// Mark messages in a room as read by a user
    pub async fn mark_read(
        &self,
        room_id: ObjectId,
        user_id: ObjectId,
        message_ids: &[ObjectId],
    ) -> DaoResult<u64> {
        let result = self
            .base
            .collection()
            .update_many(
                doc! {
                    "_id": { "$in": message_ids },
                    "room_id": room_id,
                    "readby": { "$ne": user_id },
                },
                doc! { "$addToSet": { "readby": user_id } },
            )
            .await?;
        Ok(result.modified_count)
    }

    /// Mark EVERY unread message in the room read for this user — the exact
    /// same filter as `unread_count`, so the badge is guaranteed to reach 0.
    /// (2026-08-04: unread counted messages outside the newest-25 fetch
    /// window and messages read inside the call view; a broken scroll-up
    /// cursor made the older ones unreachable → permanently stuck badges.)
    pub async fn mark_all_read(&self, room_id: ObjectId, user_id: ObjectId) -> DaoResult<u64> {
        let result = self
            .base
            .collection()
            .update_many(
                doc! {
                    "room_id": room_id,
                    "deleted_at": null,
                    "thread_id": null,
                    "readby": { "$ne": user_id },
                },
                doc! { "$addToSet": { "readby": user_id } },
            )
            .await?;
        Ok(result.modified_count)
    }

    /// Count unread messages for a user in a room
    pub async fn unread_count(&self, room_id: ObjectId, user_id: ObjectId) -> DaoResult<u64> {
        let count = self
            .base
            .collection()
            .count_documents(doc! {
                "room_id": room_id,
                "deleted_at": null,
                "thread_id": null,
                "readby": { "$ne": user_id },
            })
            .await?;
        Ok(count)
    }

    /// Count unread messages for a user across multiple rooms
    pub async fn unread_counts_by_room(
        &self,
        room_ids: &[ObjectId],
        user_id: ObjectId,
    ) -> DaoResult<Vec<(ObjectId, u64)>> {
        use bson::Bson;
        use futures::TryStreamExt;

        let pipeline = vec![
            doc! { "$match": {
                "room_id": { "$in": room_ids.iter().map(|id| Bson::ObjectId(*id)).collect::<Vec<_>>() },
                "deleted_at": null,
                "thread_id": null,
                "readby": { "$ne": user_id },
            }},
            doc! { "$group": {
                "_id": "$room_id",
                "count": { "$sum": 1 },
            }},
        ];

        let mut cursor = self.base.collection().aggregate(pipeline).await?;
        let mut results = Vec::new();
        while let Some(doc) = cursor.try_next().await? {
            if let (Some(room_id), Some(count)) =
                (doc.get_object_id("_id").ok(), doc.get_i32("count").ok())
            {
                results.push((room_id, count as u64));
            }
        }
        Ok(results)
    }

    /// Store a live message's reaction counts. A deleted message is not
    /// written: a reaction that raced its delete leaves the tombstone as the
    /// delete left it (#3327).
    pub async fn update_reaction_summary(
        &self,
        message_id: ObjectId,
        summary: &[ReactionSummary],
    ) -> DaoResult<bool> {
        let summary_bson: Vec<bson::Bson> = summary
            .iter()
            .map(|s| bson::to_bson(s).unwrap_or(bson::Bson::Null))
            .collect();

        self.base
            .update_one(
                doc! { "_id": message_id, "deleted_at": null },
                doc! { "$set": { "reaction_summary": summary_bson } },
            )
            .await
    }
}
