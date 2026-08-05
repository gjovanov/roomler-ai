use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use rand::Rng;
use roomler_ai_db::models::{
    ConferenceSettings, MediaSettings, ParticipantRole, ParticipantSession, Room, RoomMember,
};

use super::base::{BaseDao, DaoError, DaoResult, PaginatedResult, PaginationParams};

/// Outcome of `leave_participant`: whether an open session was actually
/// closed, and whether it was the user's last one (⇒ the count was
/// decremented and the caller may need to auto-end the call).
#[derive(Debug, Clone, Copy)]
pub struct LeaveOutcome {
    pub closed: bool,
    pub was_last_session: bool,
    /// Stats PR-2 — the `(joined_at, left_at)` of every session THIS call
    /// closed (multi-close happens on the legacy user-level leave). The
    /// caller books call minutes from these, clamped to the call window.
    pub closed_sessions: Vec<(DateTime, DateTime)>,
}

pub struct RoomDao {
    pub base: BaseDao<Room>,
    pub members: BaseDao<RoomMember>,
    db: Database,
}

impl RoomDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, Room::COLLECTION),
            members: BaseDao::new(db, RoomMember::COLLECTION),
            db: db.clone(),
        }
    }

    // ── Room CRUD ───────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        tenant_id: ObjectId,
        name: String,
        parent_id: Option<ObjectId>,
        creator_id: ObjectId,
        is_open: bool,
        media_settings: Option<MediaSettings>,
        conference_settings: Option<ConferenceSettings>,
    ) -> DaoResult<Room> {
        let path = if let Some(pid) = parent_id {
            let parent = self.base.find_by_id_in_tenant(tenant_id, pid).await?;
            format!("{}.{}", parent.path, name)
        } else {
            name.clone()
        };

        let (meeting_code, join_url) = if media_settings.is_some() || conference_settings.is_some()
        {
            let code = generate_meeting_code();
            let url = format!("/join/{}", code);
            (Some(code), Some(url))
        } else {
            (None, None)
        };

        let now = DateTime::now();
        let room = Room {
            id: None,
            tenant_id,
            parent_id,
            name,
            path,
            emoji: None,
            topic: None,
            purpose: None,
            icon: None,
            position: 0,
            is_open,
            is_archived: false,
            is_read_only: false,
            is_default: false,
            permission_overwrites: Vec::new(),
            tags: Vec::new(),
            media_settings,
            conference_settings,
            conference_status: None,
            current_call_id: None,
            meeting_code,
            join_url,
            organizer_id: None,
            co_organizer_ids: Vec::new(),
            creator_id,
            last_message_id: None,
            last_activity_at: None,
            member_count: 1,
            message_count: 0,
            participant_count: 0,
            peak_participant_count: 0,
            actual_start_time: None,
            actual_end_time: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };

        let room_id = self.base.insert_one(&room).await?;

        // Auto-join creator
        self.join(tenant_id, room_id, creator_id).await?;

        self.base.find_by_id(room_id).await
    }

    pub async fn find_by_tenant(&self, tenant_id: ObjectId) -> DaoResult<Vec<Room>> {
        self.base
            .find_many(
                doc! { "tenant_id": tenant_id, "deleted_at": null },
                Some(doc! { "parent_id": 1, "position": 1 }),
            )
            .await
    }

    pub async fn find_user_rooms(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
    ) -> DaoResult<Vec<Room>> {
        let memberships = self
            .members
            .find_many(doc! { "tenant_id": tenant_id, "user_id": user_id }, None)
            .await?;

        let room_ids: Vec<ObjectId> = memberships.iter().map(|m| m.room_id).collect();

        if room_ids.is_empty() {
            return Ok(Vec::new());
        }

        self.base
            .find_many(
                doc! { "_id": { "$in": room_ids }, "deleted_at": null },
                Some(doc! { "parent_id": 1, "position": 1 }),
            )
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        name: Option<String>,
        topic: Option<String>,
        purpose: Option<String>,
        is_open: Option<bool>,
        is_archived: Option<bool>,
        is_read_only: Option<bool>,
    ) -> DaoResult<bool> {
        let mut set_doc = doc! {};

        if let Some(name) = name {
            set_doc.insert("name", name);
        }
        if let Some(topic) = topic {
            set_doc.insert("topic", doc! { "value": &topic });
        }
        if let Some(purpose) = purpose {
            set_doc.insert("purpose", purpose);
        }
        if let Some(is_open) = is_open {
            set_doc.insert("is_open", is_open);
        }
        if let Some(is_archived) = is_archived {
            set_doc.insert("is_archived", is_archived);
        }
        if let Some(is_read_only) = is_read_only {
            set_doc.insert("is_read_only", is_read_only);
        }

        if set_doc.is_empty() {
            return Ok(false);
        }

        self.base
            .update_one(
                doc! { "_id": room_id, "tenant_id": tenant_id },
                doc! { "$set": set_doc },
            )
            .await
    }

    pub async fn soft_delete(&self, tenant_id: ObjectId, room_id: ObjectId) -> DaoResult<bool> {
        self.base.soft_delete_in_tenant(tenant_id, room_id).await
    }

    /// Hard-delete a room and cascade to all related resources:
    /// messages, reactions, room_members, files (soft), recordings.
    /// (Legacy `call_chat_messages` cleanup kept as a raw-collection sweep —
    /// the orphaned call-chat stack was removed 2026-08-04; pre-existing
    /// rows may still linger in old databases.)
    pub async fn cascade_delete(&self, tenant_id: ObjectId, room_id: ObjectId) -> DaoResult<()> {
        // 1. Delete all messages in the room
        let msg_coll = self.db.collection::<bson::Document>("messages");
        msg_coll
            .delete_many(doc! { "room_id": room_id, "tenant_id": tenant_id })
            .await?;

        // 2. Delete all reactions in the room
        let react_coll = self.db.collection::<bson::Document>("reactions");
        react_coll
            .delete_many(doc! { "room_id": room_id, "tenant_id": tenant_id })
            .await?;

        // 3. Delete all room members
        self.members
            .hard_delete(doc! { "room_id": room_id })
            .await?;

        // 4. Delete any legacy call chat messages (orphaned collection)
        let call_chat_coll = self.db.collection::<bson::Document>("call_chat_messages");
        call_chat_coll
            .delete_many(doc! { "room_id": room_id })
            .await?;

        // 5. Soft-delete all files associated with the room
        let files_coll = self.db.collection::<bson::Document>("files");
        files_coll
            .update_many(
                doc! { "tenant_id": tenant_id, "context.room_id": room_id },
                doc! { "$set": { "deleted_at": DateTime::now() } },
            )
            .await?;

        // 6. Delete all recordings
        let rec_coll = self.db.collection::<bson::Document>("recordings");
        rec_coll.delete_many(doc! { "room_id": room_id }).await?;

        // 7. Hard-delete the room itself
        self.base
            .hard_delete(doc! { "_id": room_id, "tenant_id": tenant_id })
            .await?;

        Ok(())
    }

    pub async fn explore(&self, tenant_id: ObjectId, query: &str) -> DaoResult<Vec<Room>> {
        let escaped: String = query
            .chars()
            .flat_map(|c| {
                if ".*+?^${}()|[]\\".contains(c) {
                    vec!['\\', c]
                } else {
                    vec![c]
                }
            })
            .collect();

        self.base
            .find_many(
                doc! {
                    "tenant_id": tenant_id,
                    "deleted_at": null,
                    "is_open": true,
                    "$or": [
                        { "name": { "$regex": &escaped, "$options": "i" } },
                        { "purpose": { "$regex": &escaped, "$options": "i" } },
                        { "tags": { "$regex": &escaped, "$options": "i" } },
                    ]
                },
                Some(doc! { "member_count": -1 }),
            )
            .await
    }

    // ── Hierarchy ───────────────────────────────────────────────

    pub async fn get_children(&self, room_id: ObjectId) -> DaoResult<Vec<Room>> {
        self.base
            .find_many(
                doc! { "parent_id": room_id, "deleted_at": null },
                Some(doc! { "position": 1 }),
            )
            .await
    }

    /// Returns all ancestor rooms by parsing the dot-path.
    pub async fn get_ancestors(&self, path: &str) -> DaoResult<Vec<Room>> {
        let parts: Vec<&str> = path.split('.').collect();
        if parts.len() <= 1 {
            return Ok(Vec::new());
        }

        let mut ancestor_paths = Vec::new();
        for i in 1..parts.len() {
            ancestor_paths.push(parts[..i].join("."));
        }

        self.base
            .find_many(
                doc! { "path": { "$in": &ancestor_paths }, "deleted_at": null },
                Some(doc! { "path": 1 }),
            )
            .await
    }

    // ── Membership ──────────────────────────────────────────────

    pub async fn join(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        user_id: ObjectId,
    ) -> DaoResult<RoomMember> {
        let now = DateTime::now();
        let member = RoomMember {
            id: None,
            tenant_id,
            room_id,
            user_id: Some(user_id),
            display_name: None,
            email: None,
            is_external: false,
            role: None,
            sessions: Vec::new(),
            joined_at: now,
            last_read_message_id: None,
            last_read_at: None,
            unread_count: 0,
            mention_count: 0,
            notification_override: None,
            is_muted: false,
            is_pinned: false,
            is_video_on: false,
            is_screen_sharing: false,
            is_hand_raised: false,
            total_duration: 0,
            created_at: now,
            updated_at: now,
        };

        let id = self.members.insert_one(&member).await?;

        self.base
            .update_by_id(room_id, doc! { "$inc": { "member_count": 1 } })
            .await?;

        self.members.find_by_id(id).await
    }

    pub async fn leave(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        user_id: ObjectId,
    ) -> DaoResult<bool> {
        let deleted = self
            .members
            .hard_delete(doc! {
                "tenant_id": tenant_id,
                "room_id": room_id,
                "user_id": user_id,
            })
            .await?;

        if deleted > 0 {
            self.base
                .update_by_id(room_id, doc! { "$inc": { "member_count": -1 } })
                .await?;
        }

        Ok(deleted > 0)
    }

    pub async fn list_members(
        &self,
        room_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<RoomMember>> {
        self.members
            .find_paginated(
                doc! { "room_id": room_id },
                Some(doc! { "joined_at": 1 }),
                params,
            )
            .await
    }

    pub async fn find_member_user_ids(&self, room_id: ObjectId) -> DaoResult<Vec<ObjectId>> {
        use futures::TryStreamExt;

        let filter = doc! { "room_id": room_id };
        let projection = doc! { "user_id": 1, "_id": 0 };
        let coll = self
            .members
            .collection()
            .clone_with_type::<bson::Document>();
        let mut cursor = coll.find(filter).projection(projection).await?;

        let mut user_ids = Vec::new();
        while let Some(doc) = cursor.try_next().await? {
            if let Ok(uid) = doc.get_object_id("user_id") {
                user_ids.push(uid);
            }
        }
        Ok(user_ids)
    }

    // ── Conference / Call operations ────────────────────────────

    /// Transition-gated call start (stats PR-2): only the caller that
    /// actually flips the room to `in_progress` gets `Some(started_at)` —
    /// `call/start` is re-invokable by design (two pods racing, UI
    /// retries), and a second start must neither reset
    /// `actual_start_time` nor mint a second call instance.
    pub async fn start_call(
        &self,
        room_id: ObjectId,
        call_id: ObjectId,
    ) -> DaoResult<Option<DateTime>> {
        let now = DateTime::now();
        let pre = self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": room_id, "conference_status": { "$ne": "in_progress" } },
                doc! {
                    "$set": {
                        "conference_status": "in_progress",
                        "actual_start_time": now,
                        "current_call_id": call_id,
                        "updated_at": now,
                    }
                },
            )
            .await
            .map_err(DaoError::Mongo)?;
        Ok(pre.map(|_| now))
    }

    /// End the call, clearing `current_call_id` and returning it so the
    /// caller can close the matching `call_sessions` document.
    pub async fn end_call(&self, room_id: ObjectId) -> DaoResult<Option<ObjectId>> {
        let pre = self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": room_id },
                doc! {
                    "$set": {
                        "conference_status": "ended",
                        "actual_end_time": DateTime::now(),
                    },
                    "$unset": { "current_call_id": "" },
                },
            )
            .await
            .map_err(DaoError::Mongo)?;
        Ok(pre.and_then(|r| r.current_call_id))
    }

    /// Join a call as a participant (add session, update media state on RoomMember).
    ///
    /// `connection_id` scopes the session to one WS connection so the same
    /// user can hold independent sessions from several browsers/devices.
    pub async fn join_participant(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        user_id: ObjectId,
        display_name: String,
        device_type: String,
        connection_id: Option<String>,
    ) -> DaoResult<RoomMember> {
        let now = DateTime::now();
        let session = ParticipantSession {
            joined_at: now,
            left_at: None,
            duration: None,
            device_type,
            connection_id,
        };

        // Check for existing active session (already in call)
        let existing = self
            .members
            .collection()
            .find_one(doc! {
                "room_id": room_id,
                "user_id": user_id,
                "sessions.left_at": null,
            })
            .await
            .map_err(DaoError::Mongo)?;

        if let Some(existing) = existing {
            let eid = existing.id.unwrap();

            // Idempotent per connection: a rehome-rejoin re-invokes call/join
            // on the same WS connection — close the stale open session that
            // connection left behind so one connection never holds two open
            // sessions (which would break the last-session leave decrement).
            if let Some(cid) = session.connection_id.as_deref() {
                let opts = mongodb::options::UpdateOptions::builder()
                    .array_filters(vec![
                        doc! { "elem.connection_id": cid, "elem.left_at": null },
                    ])
                    .build();
                self.members
                    .collection()
                    .update_one(
                        doc! { "_id": eid },
                        doc! { "$set": { "sessions.$[elem].left_at": now } },
                    )
                    .with_options(opts)
                    .await
                    .map_err(DaoError::Mongo)?;
            }

            self.members
                .collection()
                .update_one(
                    doc! { "_id": eid },
                    doc! {
                        "$push": { "sessions": bson::to_bson(&session).unwrap() },
                        "$set": { "updated_at": now },
                    },
                )
                .await
                .map_err(DaoError::Mongo)?;

            return self.members.find_by_id(eid).await;
        }

        // Check for member who previously left call (all sessions closed)
        let rejoining = self
            .members
            .collection()
            .find_one(doc! {
                "room_id": room_id,
                "user_id": user_id,
            })
            .await
            .map_err(DaoError::Mongo)?;

        if let Some(rejoining) = rejoining {
            let rid = rejoining.id.unwrap();
            self.members
                .collection()
                .update_one(
                    doc! { "_id": rid },
                    doc! {
                        "$push": { "sessions": bson::to_bson(&session).unwrap() },
                        "$set": {
                            "updated_at": now,
                            "display_name": &display_name,
                            "role": bson::to_bson(&ParticipantRole::Attendee).unwrap(),
                            "is_video_on": true,
                        },
                    },
                )
                .await
                .map_err(DaoError::Mongo)?;

            self.bump_participant_count(room_id, false).await?;

            return self.members.find_by_id(rid).await;
        }

        // Brand-new member (not yet in the room at all) — create membership + session
        let member = RoomMember {
            id: None,
            tenant_id,
            room_id,
            user_id: Some(user_id),
            display_name: Some(display_name),
            email: None,
            is_external: false,
            role: Some(ParticipantRole::Attendee),
            sessions: vec![session],
            joined_at: now,
            last_read_message_id: None,
            last_read_at: None,
            unread_count: 0,
            mention_count: 0,
            notification_override: None,
            is_muted: false,
            is_pinned: false,
            is_video_on: true,
            is_screen_sharing: false,
            is_hand_raised: false,
            total_duration: 0,
            created_at: now,
            updated_at: now,
        };

        let id = self.members.insert_one(&member).await?;

        // Increment both member_count and participant_count
        self.bump_participant_count(room_id, true).await?;

        self.members.find_by_id(id).await
    }

    /// `$inc` the distinct-user participant count (and `member_count` for a
    /// brand-new member), then `$max` the previously-dead
    /// `peak_participant_count` from the post-inc value (stats PR-2). The
    /// two writes aren't atomic together, so the peak can be off by one
    /// under concurrent joins — accepted; the per-call peak on
    /// `call_sessions` comes from the media sampler's gauge instead.
    async fn bump_participant_count(&self, room_id: ObjectId, also_member: bool) -> DaoResult<()> {
        let inc = if also_member {
            doc! { "member_count": 1, "participant_count": 1 }
        } else {
            doc! { "participant_count": 1 }
        };
        let opts = mongodb::options::FindOneAndUpdateOptions::builder()
            .return_document(mongodb::options::ReturnDocument::After)
            .build();
        let post = self
            .base
            .collection()
            .find_one_and_update(doc! { "_id": room_id }, doc! { "$inc": inc })
            .with_options(opts)
            .await
            .map_err(DaoError::Mongo)?;
        if let Some(room) = post {
            self.base
                .collection()
                .update_one(
                    doc! { "_id": room_id },
                    doc! { "$max": { "peak_participant_count": room.participant_count as i64 } },
                )
                .await
                .map_err(DaoError::Mongo)?;
        }
        Ok(())
    }

    /// Close call session(s) for a user. `connection_id: Some(_)` closes ONLY
    /// that connection's session (multi-device: the user's other browsers keep
    /// theirs); `None` is the legacy user-level leave closing all open ones.
    ///
    /// `participant_count` counts DISTINCT USERS, so it is decremented only
    /// when the user's last open session closed. The close runs as a single
    /// `find_one_and_update` whose filter DEMANDS a matching open session and
    /// whose post-image decides "last": two concurrent leaves serialize on the
    /// document, exactly one observes zero open sessions, and a duplicate
    /// leave matches nothing — the double-decrement that used to drive the
    /// count to 0 under another live user (ending their call) can't happen.
    pub async fn leave_participant(
        &self,
        room_id: ObjectId,
        user_id: ObjectId,
        connection_id: Option<&str>,
    ) -> DaoResult<LeaveOutcome> {
        let now = DateTime::now();
        let now_b = bson::Bson::DateTime(now);
        let filter = match connection_id {
            Some(cid) => doc! {
                "room_id": room_id,
                "user_id": user_id,
                "sessions": { "$elemMatch": { "connection_id": cid, "left_at": null } },
            },
            None => doc! {
                "room_id": room_id,
                "user_id": user_id,
                "sessions.left_at": null,
            },
        };
        // Pipeline update (stats PR-2): array-filter `$set` can't write a
        // DIFFERENT duration per matched element, so the close is a `$map`
        // that stamps `left_at` AND the per-session `duration` (previously
        // declared-but-never-written) in the same atomic write.
        let close_cond = match connection_id {
            Some(cid) => doc! { "$and": [
                { "$eq": [ "$$s.connection_id", cid ] },
                { "$eq": [ "$$s.left_at", null ] },
            ]},
            None => doc! { "$eq": [ "$$s.left_at", null ] },
        };
        let update = vec![doc! { "$set": {
            "sessions": { "$map": { "input": "$sessions", "as": "s", "in": {
                "$cond": [
                    close_cond,
                    { "$mergeObjects": [ "$$s", {
                        "left_at": now_b.clone(),
                        "duration": { "$toLong": { "$divide": [
                            { "$subtract": [ now_b.clone(), "$$s.joined_at" ] },
                            1000,
                        ] } },
                    } ] },
                    "$$s",
                ]
            }}},
            "updated_at": now_b,
        }}];
        let opts = mongodb::options::FindOneAndUpdateOptions::builder()
            .return_document(mongodb::options::ReturnDocument::After)
            .build();
        let post = self
            .members
            .collection()
            .find_one_and_update(filter, update)
            .with_options(opts)
            .await
            .map_err(DaoError::Mongo)?;

        let Some(post) = post else {
            return Ok(LeaveOutcome {
                closed: false,
                was_last_session: false,
                closed_sessions: Vec::new(),
            });
        };
        let was_last_session = post.sessions.iter().all(|s| s.left_at.is_some());
        let closed_sessions: Vec<(DateTime, DateTime)> = post
            .sessions
            .iter()
            .filter(|s| s.left_at == Some(now))
            .map(|s| (s.joined_at, now))
            .collect();
        // Fill the previously-dead lifetime counter from what just closed.
        let closed_secs: i64 = closed_sessions
            .iter()
            .map(|(j, l)| (l.timestamp_millis() - j.timestamp_millis()).max(0) / 1000)
            .sum();
        if closed_secs > 0
            && let Some(mid) = post.id
        {
            self.members
                .collection()
                .update_one(
                    doc! { "_id": mid },
                    doc! { "$inc": { "total_duration": closed_secs } },
                )
                .await
                .map_err(DaoError::Mongo)?;
        }

        if was_last_session {
            // Guarded decrement: only when participant_count > 0, so a stray
            // leave after the join was never recorded can't underflow the u32
            // and break GET /room with a "expected u32, got -1" deserialize 500.
            self.base
                .collection()
                .update_one(
                    doc! { "_id": room_id, "participant_count": { "$gt": 0 } },
                    doc! {
                        "$inc": { "participant_count": -1 },
                        "$set": { "updated_at": now },
                    },
                )
                .await
                .map_err(DaoError::Mongo)?;
        }

        Ok(LeaveOutcome {
            closed: true,
            was_last_session,
            closed_sessions,
        })
    }

    /// Room in which the given WS connection still holds an OPEN call session.
    /// Disconnect-path fallback for when the in-memory media maps have no
    /// entry (HTTP call/join succeeded but `media:join` never happened).
    pub async fn find_call_room_for_connection(
        &self,
        user_id: ObjectId,
        connection_id: &str,
    ) -> DaoResult<Option<ObjectId>> {
        let member = self
            .members
            .collection()
            .find_one(doc! {
                "user_id": user_id,
                "sessions": { "$elemMatch": { "connection_id": connection_id, "left_at": null } },
            })
            .await
            .map_err(DaoError::Mongo)?;
        Ok(member.map(|m| m.room_id))
    }

    pub async fn list_participants(&self, room_id: ObjectId) -> DaoResult<Vec<RoomMember>> {
        self.members
            .find_many(
                doc! { "room_id": room_id, "sessions.left_at": null },
                Some(doc! { "created_at": 1 }),
            )
            .await
    }

    pub async fn find_participant_user_ids(&self, room_id: ObjectId) -> DaoResult<Vec<ObjectId>> {
        let participants = self
            .members
            .find_many(doc! { "room_id": room_id }, None)
            .await?;
        Ok(participants.into_iter().filter_map(|p| p.user_id).collect())
    }

    pub async fn find_participant_name(
        &self,
        room_id: ObjectId,
        user_id: ObjectId,
    ) -> DaoResult<String> {
        let participant = self
            .members
            .collection()
            .find_one(doc! {
                "room_id": room_id,
                "user_id": user_id,
            })
            .await
            .map_err(DaoError::Mongo)?;

        Ok(participant
            .and_then(|p| p.display_name)
            .unwrap_or_else(|| user_id.to_hex()[..8].to_string()))
    }

    // ── Room list with call filter ──────────────────────────────

    pub async fn list_by_tenant(
        &self,
        tenant_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<Room>> {
        self.base
            .find_paginated(
                doc! { "tenant_id": tenant_id, "deleted_at": null },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }
}

fn generate_meeting_code() -> String {
    let mut rng = rand::rng();
    let parts: Vec<String> = (0..3)
        .map(|_| {
            let n: u32 = rng.random_range(100..999);
            n.to_string()
        })
        .collect();
    parts.join("-")
}
