use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_remote_control::models::{
    OverlayPolicy, OverlayRule, OverlaySelector, OverlayTarget,
};

use super::base::{BaseDao, DaoResult, PaginatedResult, PaginationParams};

/// CRUD for the overlay L3 ACL. Mirrors [`super::tunnel_policy::TunnelPolicyDao`]
/// deliberately — same soft-delete + tenant-scoping conventions — but the rows
/// drive netmap shaping rather than per-flow forward decisions.
pub struct OverlayPolicyDao {
    pub base: BaseDao<OverlayPolicy>,
}

impl OverlayPolicyDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, OverlayPolicy::COLLECTION),
        }
    }

    pub async fn create(
        &self,
        tenant_id: ObjectId,
        name: String,
        enabled: bool,
        sources: Vec<OverlaySelector>,
        via: Vec<OverlayTarget>,
        destinations: Vec<OverlayRule>,
    ) -> DaoResult<OverlayPolicy> {
        let now = DateTime::now();
        let policy = OverlayPolicy {
            id: None,
            tenant_id,
            name,
            enabled,
            sources,
            via,
            destinations,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let id = self.base.insert_one(&policy).await?;
        self.base.find_by_id(id).await
    }

    /// All live policies for a tenant, newest first.
    ///
    /// Read on every overlay join and on every re-fan. Unlike the tunnel gate
    /// (which reads per FLOW), the overlay reads per NETMAP EVENT — joins,
    /// leaves and admin edits — so the query rate is orders of magnitude lower
    /// and a cache would be premature.
    pub async fn list_active_for_tenant(
        &self,
        tenant_id: ObjectId,
    ) -> DaoResult<Vec<OverlayPolicy>> {
        self.base
            .find_many(
                doc! { "tenant_id": tenant_id, "deleted_at": null },
                Some(doc! { "created_at": -1 }),
            )
            .await
    }

    pub async fn list_for_tenant(
        &self,
        tenant_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<OverlayPolicy>> {
        self.base
            .find_paginated(
                doc! { "tenant_id": tenant_id, "deleted_at": null },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    pub async fn find_in_tenant(
        &self,
        tenant_id: ObjectId,
        policy_id: ObjectId,
    ) -> DaoResult<OverlayPolicy> {
        self.base.find_by_id_in_tenant(tenant_id, policy_id).await
    }

    pub async fn update(
        &self,
        tenant_id: ObjectId,
        policy_id: ObjectId,
        name: Option<String>,
        enabled: Option<bool>,
        sources: Option<Vec<OverlaySelector>>,
        via: Option<Vec<OverlayTarget>>,
        destinations: Option<Vec<OverlayRule>>,
    ) -> DaoResult<bool> {
        let mut set = doc! { "updated_at": DateTime::now() };
        if let Some(n) = name {
            set.insert("name", n);
        }
        if let Some(e) = enabled {
            set.insert("enabled", e);
        }
        if let Some(s) = sources {
            set.insert("sources", bson::to_bson(&s).unwrap_or(bson::Bson::Null));
        }
        if let Some(v) = via {
            set.insert("via", bson::to_bson(&v).unwrap_or(bson::Bson::Null));
        }
        if let Some(d) = destinations {
            set.insert(
                "destinations",
                bson::to_bson(&d).unwrap_or(bson::Bson::Null),
            );
        }
        self.base
            .update_one(
                doc! { "_id": policy_id, "tenant_id": tenant_id },
                doc! { "$set": set },
            )
            .await
    }

    pub async fn soft_delete(&self, tenant_id: ObjectId, policy_id: ObjectId) -> DaoResult<bool> {
        self.base.soft_delete_in_tenant(tenant_id, policy_id).await
    }
}
