use bson::{doc, oid::ObjectId, DateTime};
use mongodb::Database;
use roomler2_db::models::{AuthorType, ContentType, Mentions, Message, MessageAttachment, MessageType, ReactionSummary};

use super::base::{BaseDao, DaoResult, PaginatedResult, PaginationParams};

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
            tenant_id, room_id, author_id, content,
            thread_id, referenced_message_id, nonce, mentions, Vec::new(),
        ).await
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
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };

        let id = self.base.insert_one(&message).await?;
        self.base.find_by_id(id).await
    }

    pub async fn find_in_room(
        &self,
        room_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<Message>> {
        self.base
            .find_paginated(
                doc! { "room_id": room_id, "deleted_at": null, "thread_id": null },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    pub async fn find_thread_replies(
        &self,
        thread_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<Message>> {
        self.base
            .find_paginated(
                doc! { "thread_id": thread_id, "deleted_at": null },
                Some(doc! { "created_at": 1 }),
                params,
            )
            .await
    }

    pub async fn find_pinned(
        &self,
        room_id: ObjectId,
    ) -> DaoResult<Vec<Message>> {
        self.base
            .find_many(
                doc! { "room_id": room_id, "is_pinned": true, "deleted_at": null },
                Some(doc! { "created_at": -1 }),
            )
            .await
    }

    pub async fn update_content(
        &self,
        tenant_id: ObjectId,
        message_id: ObjectId,
        author_id: ObjectId,
        content: String,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": message_id,
                    "tenant_id": tenant_id,
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

    pub async fn toggle_pin(
        &self,
        tenant_id: ObjectId,
        message_id: ObjectId,
        pinned: bool,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": message_id, "tenant_id": tenant_id },
                doc! { "$set": { "is_pinned": pinned } },
            )
            .await
    }

    pub async fn update_reaction_summary(
        &self,
        message_id: ObjectId,
        summary: &[ReactionSummary],
    ) -> DaoResult<bool> {
        let summary_bson: Vec<bson::Bson> = summary
            .iter()
            .map(|s| {
                bson::to_bson(s).unwrap_or(bson::Bson::Null)
            })
            .collect();

        self.base
            .update_one(
                doc! { "_id": message_id },
                doc! { "$set": { "reaction_summary": summary_bson } },
            )
            .await
    }
}
