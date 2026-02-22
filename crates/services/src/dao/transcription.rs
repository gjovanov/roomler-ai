use bson::{doc, oid::ObjectId, DateTime};
use mongodb::Database;
use roomler2_db::models::{self, transcription::*};

use super::base::{BaseDao, DaoResult, PaginatedResult, PaginationParams};

pub struct TranscriptionDao {
    pub base: BaseDao<models::Transcription>,
}

impl TranscriptionDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, models::Transcription::COLLECTION),
        }
    }

    pub async fn create(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        recording_id: Option<ObjectId>,
        language: String,
    ) -> DaoResult<models::Transcription> {
        let now = DateTime::now();
        let transcription = models::Transcription {
            id: None,
            tenant_id,
            room_id,
            recording_id,
            status: TranscriptionStatus::Processing,
            language,
            format: TranscriptFormat::Json,
            content_url: String::new(),
            segments: Vec::new(),
            summary: None,
            action_items: Vec::new(),
            created_at: now,
            updated_at: now,
        };

        let id = self.base.insert_one(&transcription).await?;
        self.base.find_by_id(id).await
    }

    pub async fn find_by_room(
        &self,
        room_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<models::Transcription>> {
        self.base
            .find_paginated(
                doc! { "room_id": room_id },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    pub async fn update_content(
        &self,
        id: ObjectId,
        segments: Vec<TranscriptSegment>,
        summary: Option<String>,
        action_items: Vec<ActionItem>,
        content_url: String,
    ) -> DaoResult<bool> {
        let segments_bson: Vec<bson::Bson> = segments
            .iter()
            .map(|s| bson::to_bson(s).unwrap_or(bson::Bson::Null))
            .collect();
        let actions_bson: Vec<bson::Bson> = action_items
            .iter()
            .map(|a| bson::to_bson(a).unwrap_or(bson::Bson::Null))
            .collect();

        self.base
            .update_by_id(
                id,
                doc! {
                    "$set": {
                        "status": "available",
                        "segments": segments_bson,
                        "summary": summary,
                        "action_items": actions_bson,
                        "content_url": content_url,
                    }
                },
            )
            .await
    }

    pub async fn update_status(
        &self,
        id: ObjectId,
        status: TranscriptionStatus,
    ) -> DaoResult<bool> {
        self.base
            .update_by_id(
                id,
                doc! { "$set": { "status": bson::to_bson(&status).unwrap_or_default() } },
            )
            .await
    }
}
