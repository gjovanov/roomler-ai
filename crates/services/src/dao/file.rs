use bson::{doc, oid::ObjectId, DateTime};
use mongodb::Database;
use roomler2_db::models::{self, FileContext, ScanStatus};
use roomler2_db::models::recording::{StorageProvider, Visibility};

use super::base::{BaseDao, DaoResult, PaginatedResult, PaginationParams};

pub struct FileDao {
    pub base: BaseDao<models::File>,
}

impl FileDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, models::File::COLLECTION),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        tenant_id: ObjectId,
        uploaded_by: ObjectId,
        context: FileContext,
        filename: String,
        content_type: String,
        size: u64,
        storage_bucket: String,
        storage_key: String,
        url: String,
    ) -> DaoResult<models::File> {
        let now = DateTime::now();
        let file = models::File {
            id: None,
            tenant_id,
            uploaded_by,
            context,
            filename: filename.clone(),
            display_name: Some(filename),
            description: None,
            storage_provider: StorageProvider::MinIO,
            storage_bucket,
            storage_key,
            url,
            content_type,
            size,
            checksum: None,
            dimensions: None,
            duration: None,
            thumbnails: Vec::new(),
            version: 1,
            previous_version_id: None,
            is_current_version: true,
            external_source: None,
            scan_status: ScanStatus::Pending,
            visibility: Visibility::Private,
            recognized_content: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };

        let id = self.base.insert_one(&file).await?;
        self.base.find_by_id(id).await
    }

    pub async fn find_by_room(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<models::File>> {
        self.base
            .find_paginated(
                doc! {
                    "tenant_id": tenant_id,
                    "context.room_id": room_id,
                    "deleted_at": null,
                },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    pub async fn find_by_user(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<models::File>> {
        self.base
            .find_paginated(
                doc! {
                    "tenant_id": tenant_id,
                    "uploaded_by": user_id,
                    "deleted_at": null,
                },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    pub async fn soft_delete(
        &self,
        tenant_id: ObjectId,
        file_id: ObjectId,
    ) -> DaoResult<bool> {
        self.base.soft_delete_in_tenant(tenant_id, file_id).await
    }
}
