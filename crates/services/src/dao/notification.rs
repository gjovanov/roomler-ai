use bson::{doc, oid::ObjectId, DateTime};
use mongodb::Database;
use roomler2_db::models::{Notification, NotificationSource, NotificationType};

use super::base::{BaseDao, DaoResult, PaginatedResult, PaginationParams};

pub struct NotificationDao {
    pub base: BaseDao<Notification>,
}

impl NotificationDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, Notification::COLLECTION),
        }
    }

    pub async fn create(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
        notification_type: NotificationType,
        title: String,
        body: String,
        link: Option<String>,
        source: NotificationSource,
    ) -> DaoResult<Notification> {
        let notification = Notification {
            id: None,
            tenant_id,
            user_id,
            notification_type,
            title,
            body,
            link,
            source,
            is_read: false,
            read_at: None,
            created_at: DateTime::now(),
        };
        let id = self.base.insert_one(&notification).await?;
        self.base.find_by_id(id).await
    }

    pub async fn find_for_user(
        &self,
        user_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<Notification>> {
        self.base
            .find_paginated(
                doc! { "user_id": user_id },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    pub async fn find_unread_for_user(
        &self,
        user_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<Notification>> {
        self.base
            .find_paginated(
                doc! { "user_id": user_id, "is_read": false },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    pub async fn unread_count(&self, user_id: ObjectId) -> DaoResult<u64> {
        self.base
            .collection()
            .count_documents(doc! { "user_id": user_id, "is_read": false })
            .await
            .map_err(Into::into)
    }

    pub async fn mark_read(&self, notification_id: ObjectId, user_id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": notification_id, "user_id": user_id },
                doc! { "$set": { "is_read": true, "read_at": DateTime::now() } },
            )
            .await
    }

    pub async fn mark_all_read(&self, user_id: ObjectId) -> DaoResult<u64> {
        let result = self
            .base
            .collection()
            .update_many(
                doc! { "user_id": user_id, "is_read": false },
                doc! { "$set": { "is_read": true, "read_at": DateTime::now() } },
            )
            .await?;
        Ok(result.modified_count)
    }
}
