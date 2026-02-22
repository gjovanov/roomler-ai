use bson::{doc, oid::ObjectId, Document};
use mongodb::{Collection, Database};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Error)]
pub enum DaoError {
    #[error("MongoDB error: {0}")]
    Mongo(#[from] mongodb::error::Error),
    #[error("BSON serialization error: {0}")]
    BsonSer(#[from] bson::ser::Error),
    #[error("BSON deserialization error: {0}")]
    BsonDe(#[from] bson::de::Error),
    #[error("Entity not found")]
    NotFound,
    #[error("Duplicate key: {0}")]
    DuplicateKey(String),
    #[error("Forbidden: {0}")]
    Forbidden(String),
    #[error("Validation: {0}")]
    Validation(String),
}

pub type DaoResult<T> = Result<T, DaoError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginationParams {
    #[serde(default = "default_page")]
    pub page: u64,
    #[serde(default = "default_per_page")]
    pub per_page: u64,
}

impl Default for PaginationParams {
    fn default() -> Self {
        Self {
            page: default_page(),
            per_page: default_per_page(),
        }
    }
}

fn default_page() -> u64 {
    1
}

fn default_per_page() -> u64 {
    25
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginatedResult<T> {
    pub items: Vec<T>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

pub struct BaseDao<T: Send + Sync> {
    collection: Collection<T>,
}

impl<T> BaseDao<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Unpin + Send + Sync,
{
    pub fn new(db: &Database, collection_name: &str) -> Self {
        Self {
            collection: db.collection::<T>(collection_name),
        }
    }

    pub fn collection(&self) -> &Collection<T> {
        &self.collection
    }

    pub async fn find_by_id(&self, id: ObjectId) -> DaoResult<T> {
        self.collection
            .find_one(doc! { "_id": id })
            .await?
            .ok_or(DaoError::NotFound)
    }

    pub async fn find_by_id_in_tenant(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
    ) -> DaoResult<T> {
        self.collection
            .find_one(doc! { "_id": id, "tenant_id": tenant_id })
            .await?
            .ok_or(DaoError::NotFound)
    }

    pub async fn find_one(&self, filter: Document) -> DaoResult<Option<T>> {
        Ok(self.collection.find_one(filter).await?)
    }

    pub async fn find_many(
        &self,
        filter: Document,
        sort: Option<Document>,
    ) -> DaoResult<Vec<T>> {
        let mut cursor = if let Some(sort) = sort {
            self.collection
                .find(filter)
                .sort(sort)
                .await?
        } else {
            self.collection.find(filter).await?
        };

        let mut results = Vec::new();
        use futures::TryStreamExt;
        while let Some(doc) = cursor.try_next().await? {
            results.push(doc);
        }
        Ok(results)
    }

    pub async fn find_paginated(
        &self,
        filter: Document,
        sort: Option<Document>,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<T>> {
        let total = self.collection.count_documents(filter.clone()).await?;
        let skip = (params.page - 1) * params.per_page;

        let sort = sort.unwrap_or_else(|| doc! { "created_at": -1 });

        let mut cursor = self
            .collection
            .find(filter)
            .sort(sort)
            .skip(skip)
            .limit(params.per_page as i64)
            .await?;

        let mut items = Vec::new();
        use futures::TryStreamExt;
        while let Some(doc) = cursor.try_next().await? {
            items.push(doc);
        }

        let total_pages = total.div_ceil(params.per_page);

        Ok(PaginatedResult {
            items,
            total,
            page: params.page,
            per_page: params.per_page,
            total_pages,
        })
    }

    pub async fn insert_one(&self, doc: &T) -> DaoResult<ObjectId> {
        let result = self.collection.insert_one(doc).await.map_err(|e| {
            if let mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteError(
                ref write_error,
            )) = *e.kind
                && write_error.code == 11000
            {
                return DaoError::DuplicateKey(write_error.message.clone());
            }
            DaoError::Mongo(e)
        })?;

        let id = result
            .inserted_id
            .as_object_id()
            .expect("inserted_id should be ObjectId");
        debug!(?id, "Inserted document");
        Ok(id)
    }

    pub async fn update_one(
        &self,
        filter: Document,
        update: Document,
    ) -> DaoResult<bool> {
        let update_with_timestamp = doc! {
            "$set": {
                "updated_at": bson::DateTime::now(),
            },
            "$setOnInsert": {},
        };

        // Merge update into the $set
        let mut final_update = update;
        if let Ok(set_doc) = final_update.get_document_mut("$set") {
            set_doc.insert("updated_at", bson::DateTime::now());
        } else {
            let mut merged = update_with_timestamp;
            for (key, value) in final_update.iter() {
                if key == "$set" {
                    if let Ok(existing_set) = merged.get_document_mut("$set")
                        && let Some(new_set) = value.as_document()
                    {
                        for (k, v) in new_set.iter() {
                            existing_set.insert(k, v.clone());
                        }
                    }
                } else {
                    merged.insert(key, value.clone());
                }
            }
            final_update = merged;
        }

        let result = self.collection.update_one(filter, final_update).await?;
        Ok(result.modified_count > 0)
    }

    pub async fn update_by_id(&self, id: ObjectId, update: Document) -> DaoResult<bool> {
        self.update_one(doc! { "_id": id }, update).await
    }

    pub async fn soft_delete(&self, id: ObjectId) -> DaoResult<bool> {
        self.update_one(
            doc! { "_id": id },
            doc! { "$set": { "deleted_at": bson::DateTime::now() } },
        )
        .await
    }

    pub async fn soft_delete_in_tenant(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
    ) -> DaoResult<bool> {
        self.update_one(
            doc! { "_id": id, "tenant_id": tenant_id },
            doc! { "$set": { "deleted_at": bson::DateTime::now() } },
        )
        .await
    }

    pub async fn hard_delete(&self, filter: Document) -> DaoResult<u64> {
        let result = self.collection.delete_many(filter).await?;
        Ok(result.deleted_count)
    }

    pub async fn count(&self, filter: Document) -> DaoResult<u64> {
        Ok(self.collection.count_documents(filter).await?)
    }
}
