use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_remote_control::models::{AgentStatus, OsKind, TunnelClient};

use super::base::{BaseDao, DaoResult, PaginatedResult, PaginationParams};

pub struct TunnelClientDao {
    pub base: BaseDao<TunnelClient>,
}

impl TunnelClientDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, TunnelClient::COLLECTION),
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
        client_version: String,
    ) -> DaoResult<TunnelClient> {
        let now = DateTime::now();
        let client = TunnelClient {
            id: None,
            tenant_id,
            owner_user_id,
            name,
            machine_id,
            os,
            client_version,
            status: AgentStatus::Offline,
            last_seen_at: now,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let id = self.base.insert_one(&client).await?;
        self.base.find_by_id(id).await
    }

    /// Locate a tunnel client by `(tenant_id, machine_id)` regardless
    /// of soft-delete state. Mirrors `AgentDao::find_by_tenant_and_machine`
    /// — the unique index covers soft-deleted rows, so the enroll path
    /// calls this first and rehydrates instead of failing with E11000.
    pub async fn find_by_tenant_and_machine(
        &self,
        tenant_id: ObjectId,
        machine_id: &str,
    ) -> DaoResult<Option<TunnelClient>> {
        self.base
            .find_one(doc! {
                "tenant_id": tenant_id,
                "machine_id": machine_id,
            })
            .await
    }

    /// Refresh at re-enrollment: clear `deleted_at`, refresh
    /// name / os / client_version, bump `updated_at`. Returns the
    /// updated row so the caller can mint a fresh tunnel-client token.
    pub async fn rehydrate(
        &self,
        client_id: ObjectId,
        name: &str,
        os: OsKind,
        client_version: &str,
    ) -> DaoResult<TunnelClient> {
        let os_bson = bson::to_bson(&os).unwrap_or(bson::Bson::Null);
        self.base
            .update_by_id(
                client_id,
                doc! {
                    "$set": {
                        "name": name,
                        "os": os_bson,
                        "client_version": client_version,
                        "updated_at": DateTime::now(),
                        "deleted_at": bson::Bson::Null,
                    }
                },
            )
            .await?;
        self.base.find_by_id(client_id).await
    }

    pub async fn list_for_tenant(
        &self,
        tenant_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<TunnelClient>> {
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
        client_id: ObjectId,
    ) -> DaoResult<TunnelClient> {
        self.base.find_by_id_in_tenant(tenant_id, client_id).await
    }

    pub async fn mark_status(&self, client_id: ObjectId, status: AgentStatus) -> DaoResult<bool> {
        self.base
            .update_by_id(
                client_id,
                doc! {
                    "$set": {
                        "status": bson::to_bson(&status).unwrap(),
                        "last_seen_at": DateTime::now(),
                    }
                },
            )
            .await
    }

    /// Per plan §"What changed from v1" #4 — the WS handler polls
    /// this row every 60 s and closes the connection if `status`
    /// leaves `{Online, Offline}` (e.g. an admin sets `Quarantined`).
    /// The 60 s lag is acceptable for v1; a Redis pub/sub fast-path
    /// is a 3-day add later if it bites in practice.
    pub async fn touch_heartbeat(&self, client_id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_by_id(
                client_id,
                doc! { "$set": { "last_seen_at": DateTime::now() } },
            )
            .await
    }

    pub async fn rename(
        &self,
        tenant_id: ObjectId,
        client_id: ObjectId,
        name: &str,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": client_id, "tenant_id": tenant_id },
                doc! { "$set": { "name": name } },
            )
            .await
    }

    pub async fn quarantine(&self, tenant_id: ObjectId, client_id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": client_id, "tenant_id": tenant_id },
                doc! { "$set": {
                    "status": bson::to_bson(&AgentStatus::Quarantined).unwrap(),
                    "updated_at": DateTime::now(),
                } },
            )
            .await
    }

    pub async fn soft_delete(&self, tenant_id: ObjectId, client_id: ObjectId) -> DaoResult<bool> {
        self.base.soft_delete_in_tenant(tenant_id, client_id).await
    }
}
