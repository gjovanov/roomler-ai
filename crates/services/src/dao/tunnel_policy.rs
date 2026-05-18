use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_remote_control::models::{
    DestinationRule, PolicySubject, PolicyTarget, TunnelPolicy,
};

use super::base::{BaseDao, DaoResult, PaginatedResult, PaginationParams};

pub struct TunnelPolicyDao {
    pub base: BaseDao<TunnelPolicy>,
}

impl TunnelPolicyDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, TunnelPolicy::COLLECTION),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        tenant_id: ObjectId,
        name: String,
        subjects: Vec<PolicySubject>,
        targets: Vec<PolicyTarget>,
        allowlist: Vec<DestinationRule>,
        max_concurrent_flows: Option<u32>,
        max_bytes_per_session: Option<u64>,
    ) -> DaoResult<TunnelPolicy> {
        let now = DateTime::now();
        let policy = TunnelPolicy {
            id: None,
            tenant_id,
            name,
            subjects,
            targets,
            allowlist,
            max_concurrent_flows,
            max_bytes_per_session,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let id = self.base.insert_one(&policy).await?;
        self.base.find_by_id(id).await
    }

    /// All live (non-soft-deleted) policies for a tenant. The
    /// server-side ACL gate fetches this set on every
    /// `TcpForwardRequest` and runs the in-memory evaluator. v1
    /// keeps it simple — no cache; admin UI is the only write path
    /// and writes are rare.
    pub async fn list_active_for_tenant(
        &self,
        tenant_id: ObjectId,
    ) -> DaoResult<Vec<TunnelPolicy>> {
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
    ) -> DaoResult<PaginatedResult<TunnelPolicy>> {
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
    ) -> DaoResult<TunnelPolicy> {
        self.base.find_by_id_in_tenant(tenant_id, policy_id).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        tenant_id: ObjectId,
        policy_id: ObjectId,
        name: Option<String>,
        subjects: Option<Vec<PolicySubject>>,
        targets: Option<Vec<PolicyTarget>>,
        allowlist: Option<Vec<DestinationRule>>,
        max_concurrent_flows: Option<Option<u32>>,
        max_bytes_per_session: Option<Option<u64>>,
    ) -> DaoResult<bool> {
        let mut set = doc! { "updated_at": DateTime::now() };
        if let Some(n) = name {
            set.insert("name", n);
        }
        if let Some(s) = subjects {
            set.insert("subjects", bson::to_bson(&s).unwrap_or(bson::Bson::Null));
        }
        if let Some(t) = targets {
            set.insert("targets", bson::to_bson(&t).unwrap_or(bson::Bson::Null));
        }
        if let Some(a) = allowlist {
            set.insert("allowlist", bson::to_bson(&a).unwrap_or(bson::Bson::Null));
        }
        if let Some(c) = max_concurrent_flows {
            set.insert(
                "max_concurrent_flows",
                c.map(|v| bson::Bson::Int64(v as i64))
                    .unwrap_or(bson::Bson::Null),
            );
        }
        if let Some(b) = max_bytes_per_session {
            set.insert(
                "max_bytes_per_session",
                b.map(|v| bson::Bson::Int64(v as i64))
                    .unwrap_or(bson::Bson::Null),
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
