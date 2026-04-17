use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_remote_control::models::{
    EndReason, RemoteSession, SessionPhase, SessionStats,
};
use roomler_ai_remote_control::permissions::Permissions;

use super::base::{BaseDao, DaoResult, PaginatedResult, PaginationParams};

pub struct RemoteSessionDao {
    pub base: BaseDao<RemoteSession>,
}

impl RemoteSessionDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, RemoteSession::COLLECTION),
        }
    }

    pub async fn create(
        &self,
        session_id: ObjectId,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        controller_user_id: ObjectId,
        permissions: Permissions,
    ) -> DaoResult<RemoteSession> {
        let now = DateTime::now();
        let session = RemoteSession {
            id: Some(session_id),
            agent_id,
            tenant_id,
            controller_user_id,
            watchers: Vec::new(),
            permissions,
            phase: SessionPhase::Pending,
            created_at: now,
            started_at: None,
            ended_at: None,
            end_reason: None,
            recording_url: None,
            stats: SessionStats::default(),
        };
        // Insert with explicit _id so the Hub-provided ObjectId remains the canonical key.
        self.base.collection().insert_one(&session).await?;
        Ok(session)
    }

    pub async fn find_in_tenant(
        &self,
        tenant_id: ObjectId,
        session_id: ObjectId,
    ) -> DaoResult<RemoteSession> {
        self.base.find_by_id_in_tenant(tenant_id, session_id).await
    }

    pub async fn list_for_agent(
        &self,
        agent_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<RemoteSession>> {
        self.base
            .find_paginated(
                doc! { "agent_id": agent_id },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    pub async fn list_for_user(
        &self,
        controller_user_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<RemoteSession>> {
        self.base
            .find_paginated(
                doc! { "controller_user_id": controller_user_id },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    pub async fn mark_phase(
        &self,
        session_id: ObjectId,
        phase: SessionPhase,
    ) -> DaoResult<bool> {
        let mut set = doc! { "phase": bson::to_bson(&phase).unwrap() };
        if phase == SessionPhase::Active {
            set.insert("started_at", DateTime::now());
        }
        self.base
            .update_by_id(session_id, doc! { "$set": set })
            .await
    }

    pub async fn mark_ended(
        &self,
        session_id: ObjectId,
        reason: EndReason,
        stats: SessionStats,
    ) -> DaoResult<bool> {
        let stats_bson = bson::to_bson(&stats).unwrap_or(bson::Bson::Null);
        self.base
            .update_by_id(
                session_id,
                doc! {
                    "$set": {
                        "phase": bson::to_bson(&SessionPhase::Closed).unwrap(),
                        "ended_at": DateTime::now(),
                        "end_reason": bson::to_bson(&reason).unwrap(),
                        "stats": stats_bson,
                    }
                },
            )
            .await
    }
}
