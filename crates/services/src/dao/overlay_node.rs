use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_remote_control::models::{AgentStatus, NodeRef, OverlayNode};

use super::base::{BaseDao, DaoResult};

/// Overlay membership rows — one per host (agent or tunnel-client) that
/// has joined a tenant's virtual LAN. Keyed for rehydrate by
/// `(tenant_id, machine_id)` like `agents` / `tunnel_clients`.
pub struct OverlayNodeDao {
    pub base: BaseDao<OverlayNode>,
}

impl OverlayNodeDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, OverlayNode::COLLECTION),
        }
    }

    /// Locate a node by `(tenant_id, machine_id)` regardless of
    /// soft-delete state — the join path calls this first and rehydrates
    /// (keeping the leased overlay IP) instead of allocating a new one.
    pub async fn find_by_tenant_and_machine(
        &self,
        tenant_id: ObjectId,
        machine_id: &str,
    ) -> DaoResult<Option<OverlayNode>> {
        self.base
            .find_one(doc! { "tenant_id": tenant_id, "machine_id": machine_id })
            .await
    }

    /// Insert a fresh node with a freshly-allocated overlay IP.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        tenant_id: ObjectId,
        node_ref: NodeRef,
        network_id: ObjectId,
        machine_id: String,
        name: String,
        overlay_ip: String,
        wg_public_key: String,
        key_epoch: u32,
        endpoints: Vec<String>,
        supports_quic: bool,
        advertised_routes: Vec<String>,
    ) -> DaoResult<OverlayNode> {
        let now = DateTime::now();
        let node = OverlayNode {
            id: None,
            tenant_id,
            node_ref,
            network_id,
            machine_id,
            name,
            overlay_ip,
            wg_public_key,
            key_epoch,
            // The join carries the agent's DIRECT LAN candidates; seed BOTH
            // buckets. The relay trickle later replaces `endpoints` only —
            // `lan_endpoints` survives so peers keep seeing the LAN address.
            lan_endpoints: endpoints.clone(),
            endpoints,
            relay_home: None,
            supports_quic,
            // Phase 1 — the node's claimed routes; nothing approved until an
            // admin acts, so a fresh node routes for no one.
            advertised_routes,
            approved_routes: Vec::new(),
            status: AgentStatus::Online,
            last_seen_at: now,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let id = self.base.insert_one(&node).await?;
        self.base.find_by_id(id).await
    }

    /// Re-join: clear `deleted_at`, refresh the WG key + endpoints +
    /// node_ref, mark Online. Keeps the existing `overlay_ip`. Returns
    /// the updated row.
    #[allow(clippy::too_many_arguments)]
    pub async fn rehydrate(
        &self,
        node_id: ObjectId,
        node_ref: &NodeRef,
        name: &str,
        wg_public_key: &str,
        key_epoch: u32,
        endpoints: &[String],
        supports_quic: bool,
        advertised_routes: &[String],
    ) -> DaoResult<OverlayNode> {
        let node_ref_bson = bson::to_bson(node_ref).unwrap_or(bson::Bson::Null);
        self.base
            .update_by_id(
                node_id,
                doc! {
                    "$set": {
                        "node_ref": node_ref_bson,
                        // Phase 0 — the join handler passes the stable name
                        // (existing name reused, or a freshly deduped one when
                        // backfilling a pre-Phase-0 row).
                        "name": name,
                        // Phase 1 — refresh the CLAIMED routes on each join.
                        // `approved_routes` is admin-controlled and intentionally
                        // NOT touched here, so approvals survive a rejoin.
                        "advertised_routes": advertised_routes,
                        "wg_public_key": wg_public_key,
                        "key_epoch": key_epoch as i64,
                        // Refresh BOTH buckets from the join (rc.135) — a DHCP
                        // IP change is picked up, and the LAN bucket is restored
                        // before the next relay trickle replaces `endpoints`.
                        "endpoints": endpoints,
                        "lan_endpoints": endpoints,
                        // rc.142 — refresh the QUIC capability on each re-join
                        // (an operator may flip ROOMLER_AGENT_OVERLAY_QUIC).
                        "supports_quic": supports_quic,
                        "status": bson::to_bson(&AgentStatus::Online).unwrap(),
                        "last_seen_at": DateTime::now(),
                        "updated_at": DateTime::now(),
                        "deleted_at": bson::Bson::Null,
                    }
                },
            )
            .await?;
        self.base.find_by_id(node_id).await
    }

    /// All active (non-deleted) nodes in a network — the netmap source.
    pub async fn list_active_in_network(
        &self,
        tenant_id: ObjectId,
        network_id: ObjectId,
    ) -> DaoResult<Vec<OverlayNode>> {
        self.base
            .find_many(
                doc! { "tenant_id": tenant_id, "network_id": network_id, "deleted_at": null },
                Some(doc! { "created_at": 1 }),
            )
            .await
    }

    /// Replace the node's trickled connectivity candidates.
    pub async fn update_endpoints(
        &self,
        node_id: ObjectId,
        endpoints: &[String],
    ) -> DaoResult<bool> {
        self.base
            .update_by_id(
                node_id,
                doc! { "$set": {
                    "endpoints": endpoints,
                    "last_seen_at": DateTime::now(),
                    "updated_at": DateTime::now(),
                } },
            )
            .await
    }

    pub async fn mark_status(&self, node_id: ObjectId, status: AgentStatus) -> DaoResult<bool> {
        self.base
            .update_by_id(
                node_id,
                doc! { "$set": {
                    "status": bson::to_bson(&status).unwrap(),
                    "last_seen_at": DateTime::now(),
                    "updated_at": DateTime::now(),
                } },
            )
            .await
    }

    /// Phase 1 — set the admin-APPROVED subnet routes for a node and return the
    /// updated row. The caller must first verify the node belongs to the admin's
    /// tenant and that each route is among the node's `advertised_routes`.
    pub async fn set_approved_routes(
        &self,
        node_id: ObjectId,
        approved_routes: &[String],
    ) -> DaoResult<OverlayNode> {
        self.base
            .update_by_id(
                node_id,
                doc! { "$set": {
                    "approved_routes": approved_routes,
                    "updated_at": DateTime::now(),
                } },
            )
            .await?;
        self.base.find_by_id(node_id).await
    }

    pub async fn touch_heartbeat(&self, node_id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_by_id(
                node_id,
                doc! { "$set": { "last_seen_at": DateTime::now() } },
            )
            .await
    }
}
