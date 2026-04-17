use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_remote_control::models::{
    AccessPolicy, Agent, AgentCaps, AgentStatus, DisplayInfo, OsKind,
};

use super::base::{BaseDao, DaoResult, PaginatedResult, PaginationParams};

pub struct AgentDao {
    pub base: BaseDao<Agent>,
}

impl AgentDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, Agent::COLLECTION),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        tenant_id: ObjectId,
        owner_user_id: ObjectId,
        name: String,
        machine_id: String,
        os: OsKind,
        agent_version: String,
        agent_token_hash: String,
    ) -> DaoResult<Agent> {
        let now = DateTime::now();
        let agent = Agent {
            id: None,
            tenant_id,
            owner_user_id,
            name,
            machine_id,
            os,
            agent_version,
            agent_token_hash,
            status: AgentStatus::Offline,
            last_seen_at: now,
            displays: Vec::new(),
            capabilities: AgentCaps::default(),
            access_policy: AccessPolicy::default(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let id = self.base.insert_one(&agent).await?;
        self.base.find_by_id(id).await
    }

    pub async fn find_by_tenant_and_machine(
        &self,
        tenant_id: ObjectId,
        machine_id: &str,
    ) -> DaoResult<Option<Agent>> {
        self.base
            .find_one(doc! {
                "tenant_id": tenant_id,
                "machine_id": machine_id,
                "deleted_at": null,
            })
            .await
    }

    pub async fn list_for_tenant(
        &self,
        tenant_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<Agent>> {
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
        agent_id: ObjectId,
    ) -> DaoResult<Agent> {
        self.base.find_by_id_in_tenant(tenant_id, agent_id).await
    }

    pub async fn update_hello(
        &self,
        agent_id: ObjectId,
        agent_version: &str,
        displays: &[DisplayInfo],
        capabilities: &AgentCaps,
    ) -> DaoResult<bool> {
        let displays_bson = bson::to_bson(displays).unwrap_or(bson::Bson::Array(vec![]));
        let caps_bson = bson::to_bson(capabilities).unwrap_or(bson::Bson::Null);
        self.base
            .update_by_id(
                agent_id,
                doc! {
                    "$set": {
                        "agent_version": agent_version,
                        "displays": displays_bson,
                        "capabilities": caps_bson,
                        "status": bson::to_bson(&AgentStatus::Online).unwrap(),
                        "last_seen_at": DateTime::now(),
                    }
                },
            )
            .await
    }

    pub async fn mark_status(
        &self,
        agent_id: ObjectId,
        status: AgentStatus,
    ) -> DaoResult<bool> {
        self.base
            .update_by_id(
                agent_id,
                doc! {
                    "$set": {
                        "status": bson::to_bson(&status).unwrap(),
                        "last_seen_at": DateTime::now(),
                    }
                },
            )
            .await
    }

    pub async fn update_access_policy(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        policy: &AccessPolicy,
    ) -> DaoResult<bool> {
        let policy_bson = bson::to_bson(policy).unwrap_or(bson::Bson::Null);
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "access_policy": policy_bson } },
            )
            .await
    }

    pub async fn rename(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        name: &str,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "name": name } },
            )
            .await
    }

    pub async fn soft_delete(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
    ) -> DaoResult<bool> {
        self.base.soft_delete_in_tenant(tenant_id, agent_id).await
    }
}
