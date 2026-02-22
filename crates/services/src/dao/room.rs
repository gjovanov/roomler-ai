use bson::{doc, oid::ObjectId, DateTime};
use mongodb::Database;
use rand::Rng;
use roomler2_db::models::{
    CallChatMessage, ConferenceSettings, MediaSettings, ParticipantRole, ParticipantSession,
    Room, RoomMember,
};

use super::base::{BaseDao, DaoError, DaoResult, PaginatedResult, PaginationParams};

pub struct RoomDao {
    pub base: BaseDao<Room>,
    pub members: BaseDao<RoomMember>,
    pub chat_messages: BaseDao<CallChatMessage>,
}

impl RoomDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, Room::COLLECTION),
            members: BaseDao::new(db, RoomMember::COLLECTION),
            chat_messages: BaseDao::new(db, CallChatMessage::COLLECTION),
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
            .find_many(
                doc! { "tenant_id": tenant_id, "user_id": user_id },
                None,
            )
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

    pub async fn soft_delete(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
    ) -> DaoResult<bool> {
        self.base.soft_delete_in_tenant(tenant_id, room_id).await
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
        let members = self
            .members
            .find_many(doc! { "room_id": room_id }, None)
            .await?;
        Ok(members.into_iter().filter_map(|m| m.user_id).collect())
    }

    // ── Conference / Call operations ────────────────────────────

    pub async fn start_call(&self, room_id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_by_id(
                room_id,
                doc! {
                    "$set": {
                        "conference_status": "in_progress",
                        "actual_start_time": DateTime::now(),
                    }
                },
            )
            .await
    }

    pub async fn end_call(&self, room_id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_by_id(
                room_id,
                doc! {
                    "$set": {
                        "conference_status": "ended",
                        "actual_end_time": DateTime::now(),
                    }
                },
            )
            .await
    }

    /// Join a call as a participant (add session, update media state on RoomMember).
    pub async fn join_participant(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        user_id: ObjectId,
        display_name: String,
        device_type: String,
    ) -> DaoResult<RoomMember> {
        let now = DateTime::now();
        let session = ParticipantSession {
            joined_at: now,
            left_at: None,
            duration: None,
            device_type,
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

            self.base
                .update_by_id(
                    room_id,
                    doc! { "$inc": { "participant_count": 1 } },
                )
                .await?;

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
        self.base
            .update_by_id(
                room_id,
                doc! { "$inc": { "member_count": 1, "participant_count": 1 } },
            )
            .await?;

        self.members.find_by_id(id).await
    }

    pub async fn leave_participant(
        &self,
        room_id: ObjectId,
        user_id: ObjectId,
    ) -> DaoResult<bool> {
        let now = DateTime::now();
        let filter = doc! {
            "room_id": room_id,
            "user_id": user_id,
        };
        let update = doc! {
            "$set": {
                "sessions.$[elem].left_at": now,
                "updated_at": now,
            }
        };
        let opts = mongodb::options::UpdateOptions::builder()
            .array_filters(vec![doc! { "elem.left_at": null }])
            .build();
        self.members
            .collection()
            .update_one(filter, update)
            .with_options(opts)
            .await
            .map_err(DaoError::Mongo)?;

        self.base
            .update_by_id(
                room_id,
                doc! { "$inc": { "participant_count": -1 } },
            )
            .await?;

        Ok(true)
    }

    pub async fn list_participants(&self, room_id: ObjectId) -> DaoResult<Vec<RoomMember>> {
        self.members
            .find_many(
                doc! { "room_id": room_id, "sessions.left_at": null },
                Some(doc! { "created_at": 1 }),
            )
            .await
    }

    pub async fn find_participant_user_ids(
        &self,
        room_id: ObjectId,
    ) -> DaoResult<Vec<ObjectId>> {
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

    // ── Call Chat Messages ──────────────────────────────────────

    pub async fn create_chat_message(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        author_id: ObjectId,
        display_name: String,
        content: String,
    ) -> DaoResult<CallChatMessage> {
        let msg = CallChatMessage {
            id: None,
            tenant_id,
            room_id,
            author_id,
            display_name,
            content,
            created_at: DateTime::now(),
        };
        let id = self.chat_messages.insert_one(&msg).await?;
        self.chat_messages.find_by_id(id).await
    }

    pub async fn find_chat_messages(
        &self,
        room_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<CallChatMessage>> {
        self.chat_messages
            .find_paginated(
                doc! { "room_id": room_id },
                Some(doc! { "created_at": 1 }),
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
