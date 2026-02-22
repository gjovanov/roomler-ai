use bson::{doc, oid::ObjectId, DateTime};
use mongodb::Database;
use roomler2_db::models::{Invite, InviteStatus};

use super::base::{BaseDao, DaoError, DaoResult, PaginatedResult, PaginationParams};

pub struct InviteDao {
    pub base: BaseDao<Invite>,
}

pub struct CreateInviteParams {
    pub target_email: Option<String>,
    pub max_uses: Option<u32>,
    pub expires_in_hours: Option<u64>,
    pub assign_role_ids: Vec<ObjectId>,
}

impl InviteDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, Invite::COLLECTION),
        }
    }

    pub async fn create(
        &self,
        tenant_id: ObjectId,
        inviter_id: ObjectId,
        params: CreateInviteParams,
    ) -> DaoResult<Invite> {
        let code = nanoid::nanoid!(21);
        let now = DateTime::now();

        let max_uses = if params.target_email.is_some() {
            Some(1)
        } else {
            params.max_uses
        };

        let expires_at = params.expires_in_hours.map(|hours| {
            let millis = now.timestamp_millis() + (hours as i64 * 3600 * 1000);
            DateTime::from_millis(millis)
        });

        let invite = Invite {
            id: None,
            tenant_id,
            room_id: None,
            code,
            inviter_id,
            target_email: params.target_email,
            target_user_id: None,
            max_uses,
            use_count: 0,
            expires_at,
            assign_role_ids: params.assign_role_ids,
            status: InviteStatus::Active,
            created_at: now,
            updated_at: now,
        };

        let id = self.base.insert_one(&invite).await?;
        self.base.find_by_id(id).await
    }

    pub async fn find_by_code(&self, code: &str) -> DaoResult<Invite> {
        self.base
            .find_one(doc! { "code": code })
            .await?
            .ok_or(DaoError::NotFound)
    }

    pub async fn list_by_tenant(
        &self,
        tenant_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<Invite>> {
        self.base
            .find_paginated(
                doc! { "tenant_id": tenant_id },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    /// Atomically increment use_count. Returns the updated invite if successful.
    /// Fails if use_count >= max_uses (invite exhausted).
    pub async fn increment_use_count(&self, invite_id: ObjectId) -> DaoResult<Invite> {
        use mongodb::options::FindOneAndUpdateOptions;
        use mongodb::options::ReturnDocument;

        // Atomic: only increment if status is Active and use_count < max_uses (or max_uses is null)
        let filter = doc! {
            "_id": invite_id,
            "status": "active",
            "$or": [
                { "max_uses": null },
                { "$expr": { "$lt": ["$use_count", "$max_uses"] } },
            ],
        };

        let update = doc! {
            "$inc": { "use_count": 1_i32 },
            "$set": { "updated_at": DateTime::now() },
        };

        let options = FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();

        let invite = self
            .base
            .collection()
            .find_one_and_update(filter, update)
            .with_options(options)
            .await
            .map_err(DaoError::Mongo)?
            .ok_or_else(|| {
                DaoError::Validation("Invite cannot be used (exhausted, expired, or revoked)".to_string())
            })?;

        // Auto-set status to Exhausted when use_count >= max_uses
        if let Some(max) = invite.max_uses
            && invite.use_count >= max
        {
            let _ = self
                .base
                .update_by_id(
                    invite_id,
                    doc! { "$set": { "status": "exhausted" } },
                )
                .await;
        }

        Ok(invite)
    }

    pub async fn revoke(&self, invite_id: ObjectId, tenant_id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": invite_id, "tenant_id": tenant_id },
                doc! { "$set": { "status": "revoked" } },
            )
            .await
    }

    /// Validate that an invite is still usable.
    pub fn validate(&self, invite: &Invite) -> DaoResult<()> {
        match invite.status {
            InviteStatus::Active => {}
            InviteStatus::Revoked => {
                return Err(DaoError::Validation("Invite has been revoked".to_string()));
            }
            InviteStatus::Exhausted => {
                return Err(DaoError::Validation("Invite has been fully used".to_string()));
            }
            InviteStatus::Expired => {
                return Err(DaoError::Validation("Invite has expired".to_string()));
            }
        }

        // Check expiry
        if let Some(expires_at) = invite.expires_at
            && DateTime::now().timestamp_millis() > expires_at.timestamp_millis()
        {
            return Err(DaoError::Validation("Invite has expired".to_string()));
        }

        // Check use count
        if let Some(max) = invite.max_uses
            && invite.use_count >= max
        {
            return Err(DaoError::Validation("Invite has been fully used".to_string()));
        }

        Ok(())
    }
}
