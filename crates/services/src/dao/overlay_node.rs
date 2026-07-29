use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use mongodb::options::ReturnDocument;
use roomler_ai_remote_control::models::{AgentStatus, DEFAULT_ROUTE_V4, NodeRef, OverlayNode};

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

    /// Locate the LIVE overlay node for `(tenant_id, machine_id)`. The join
    /// path calls this first and rehydrates (keeping the leased overlay IP)
    /// instead of allocating a new one.
    ///
    /// Scoped to `deleted_at: null` because removal tombstones IN PLACE: a
    /// released machine keeps its row as the forensic record, and re-enrolling
    /// the SAME machine must NOT revive it — removal is FINAL, so the device
    /// comes back as a brand-new node with a fresh IP and a freshly-deduped
    /// name.
    ///
    /// The scoping is MANDATORY, not cosmetic: the `(tenant_id, machine_id)`
    /// unique index is partial on live rows, so several tombstones may share a
    /// `machine_id` alongside one live row, and an unscoped `find_one` would
    /// return an arbitrary one of them — making a live node look absent.
    pub async fn find_live_by_tenant_and_machine(
        &self,
        tenant_id: ObjectId,
        machine_id: &str,
    ) -> DaoResult<Option<OverlayNode>> {
        self.base
            .find_one(doc! {
                "tenant_id": tenant_id,
                "machine_id": machine_id,
                "deleted_at": null,
            })
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
        supports_relay_single: bool,
        supports_derp: bool,
        supports_forced_derp: bool,
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
            // Phase B — no srflx until the node gathers it (post-join, via STUN)
            // and trickles it up on `rc:overlay.srflx`.
            srflx_endpoints: Vec::new(),
            // Phase C — NAT type is unknown until the node probes + trickles it.
            srflx_nat: None,
            relay_home: None,
            supports_quic,
            supports_relay_single,
            supports_derp,
            supports_forced_derp,
            // Phase 1 — the node's claimed routes; nothing approved until an
            // admin acts, so a fresh node routes for no one.
            advertised_routes,
            approved_routes: Vec::new(),
            // P5 — never an exit node until an admin toggles it on.
            is_exit_node: false,
            status: AgentStatus::Online,
            last_seen_at: now,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let id = self.base.insert_one(&node).await?;
        self.base.find_by_id(id).await
    }

    /// Re-join: refresh the WG key + endpoints + node_ref, mark Online. Keeps
    /// the existing `overlay_ip`. Returns the updated row.
    ///
    /// Only ever reached for a LIVE row (the lookup is
    /// [`Self::find_live_by_tenant_and_machine`]), so the `deleted_at: Null` in
    /// the `$set` below is a vestigial no-op — kept as belt-and-braces in case
    /// the lookup is ever widened.
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
        supports_relay_single: bool,
        supports_derp: bool,
        supports_forced_derp: bool,
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
                        // Phase B — clear stale srflx on re-join; the node
                        // re-gathers + re-trickles fresh srflx this connection
                        // (a prior session's NAT mapping is meaningless now).
                        "srflx_endpoints": [],
                        // Phase C (A8) — clear the stale NAT class in the SAME
                        // $set (a prior session's NAT type is meaningless after
                        // a roam); the node re-probes + re-trickles it.
                        "srflx_nat": bson::Bson::Null,
                        // rc.142 — refresh the QUIC capability on each re-join
                        // (an operator may flip ROOMLER_AGENT_OVERLAY_QUIC).
                        "supports_quic": supports_quic,
                        // Phase D — refresh the single-relay capability on each
                        // re-join (an operator may flip OVERLAY_RELAY_SINGLE).
                        "supports_relay_single": supports_relay_single,
                        // Phase D (DERP) — refresh the DERP capability on each
                        // re-join (an operator may flip OVERLAY_DERP).
                        "supports_derp": supports_derp,
                        // P7 — refresh the forced-DERP capability likewise.
                        "supports_forced_derp": supports_forced_derp,
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

    /// Compare-and-swap tombstone: mark the node released ONLY if it is still
    /// live, returning the PRE-image (so the caller can recover the overlay IP
    /// it was holding). `None` ⇒ someone else already released it — a
    /// double-clicked evict, or an evict racing the agent DELETE — and the
    /// caller must do nothing further.
    ///
    /// Winning this CAS is the release TOKEN: it is what keeps a host number
    /// from being returned to the free pool twice, which would hand two live
    /// nodes the same overlay IP.
    ///
    /// `overlay_ip`, `name`, `wg_public_key`, `machine_id` and `node_ref` are
    /// deliberately KEPT — the tombstone is the record of who held an address,
    /// which matters precisely because addresses now recycle. Uniqueness on the
    /// IP and the name is scoped to live rows, so a tombstone holds neither.
    pub async fn release(&self, node_id: ObjectId) -> DaoResult<Option<OverlayNode>> {
        let now = DateTime::now();
        Ok(self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": node_id, "deleted_at": null },
                doc! { "$set": {
                    // `Unenrolled`, not `Offline`: an offline node is coming
                    // back, a released one is not.
                    "status": bson::to_bson(&AgentStatus::Unenrolled).unwrap(),
                    // Revoke the ADMIN GRANTS. Not strictly required — a
                    // re-enroll creates a fresh row with both empty, and the
                    // approval surface only lists live nodes — but a future
                    // "restore node" action must never silently reinstate
                    // exit-node authority.
                    "approved_routes": Vec::<String>::new(),
                    "is_exit_node": false,
                    "deleted_at": now,
                    "updated_at": now,
                } },
            )
            .return_document(ReturnDocument::Before)
            .await?)
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

    /// Phase B/C — replace the node's server-reflexive (srflx) candidates and
    /// its probed NAT type. A SEPARATE bucket from `endpoints` (relay) /
    /// `lan_endpoints` (public NIC) so a srflx trickle never clobbers a relay/LAN
    /// address and vice-versa. `nat` (`Some("cone"|"symmetric")` / `None` when
    /// unknown) is stored alongside so peers can skip a both-symmetric punch.
    pub async fn update_srflx_endpoints(
        &self,
        node_id: ObjectId,
        srflx_endpoints: &[String],
        nat: Option<&str>,
    ) -> DaoResult<bool> {
        self.base
            .update_by_id(
                node_id,
                doc! { "$set": {
                    "srflx_endpoints": srflx_endpoints,
                    "srflx_nat": nat.map(|s| s.to_string()),
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

    /// P5 — designate (or un-designate) a node as an exit node. Sets the
    /// `is_exit_node` flag AND adds/removes `0.0.0.0/0` in `approved_routes`
    /// (the data-plane signal). The caller verifies tenant ownership and, when
    /// enabling, that the node advertised `0.0.0.0/0`. This is the ONLY path
    /// that puts a `/0` into `approved_routes` — the per-CIDR
    /// [`Self::set_approved_routes`] rejects it.
    pub async fn set_exit_node(&self, node_id: ObjectId, enabled: bool) -> DaoResult<OverlayNode> {
        // Read-modify-write the approved list so we preserve any subnet CIDRs
        // the node also routes while flipping just the default route.
        let node = self.base.find_by_id(node_id).await?;
        let mut approved: Vec<String> = node
            .approved_routes
            .into_iter()
            .filter(|r| r != DEFAULT_ROUTE_V4)
            .collect();
        if enabled {
            approved.push(DEFAULT_ROUTE_V4.to_string());
        }
        self.base
            .update_by_id(
                node_id,
                doc! { "$set": {
                    "is_exit_node": enabled,
                    "approved_routes": &approved,
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
