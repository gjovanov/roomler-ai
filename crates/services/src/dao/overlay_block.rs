use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_remote_control::models::{
    OVERLAY_BLOCK_FIRST_SLOT, OVERLAY_BLOCK_SLOT_COUNT, OverlayBlock, OverlayBlockState,
    block_align_slot, block_cidr_for_slot, block_slots_for_prefix,
};

use super::base::{BaseDao, DaoError, DaoResult};

/// Multi-org P2b — the GLOBAL registry of overlay address blocks.
///
/// One row per carved range. Unlike every other DAO here this collection is
/// deliberately NOT tenant-scoped: its job is to guarantee that two tenants
/// can never be handed overlapping slices of `100.64.0.0/10`, and that
/// guarantee only holds if the uniqueness is global.
pub struct OverlayBlockDao {
    pub base: BaseDao<OverlayBlock>,
}

impl OverlayBlockDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, OverlayBlock::COLLECTION),
        }
    }

    /// The block currently backing `network_id`, if it has one. Legacy
    /// networks (still on the shared `100.64.0.0/10`) have none.
    pub async fn find_assigned_for_network(
        &self,
        network_id: ObjectId,
    ) -> DaoResult<Option<OverlayBlock>> {
        self.base
            .find_one(doc! {
                "network_id": network_id,
                "state": bson::to_bson(&OverlayBlockState::Assigned).unwrap_or(bson::Bson::Null),
            })
            .await
    }

    /// Every block ever carved for a tenant, newest first — the assigned one
    /// plus its quarantined predecessors (the renumber trail).
    pub async fn list_for_tenant(&self, tenant_id: ObjectId) -> DaoResult<Vec<OverlayBlock>> {
        self.base
            .find_many(doc! { "tenant_id": tenant_id }, Some(doc! { "slot": -1 }))
            .await
    }

    /// Carve a fresh block of `prefix` for `(tenant_id, network_id)`.
    ///
    /// Monotonic-upward allocation: take the highest `end_slot` in the whole
    /// registry, round it up to this block's alignment, insert with a unique
    /// `slot`. Quarantined rows still occupy their slots, so a freed range is
    /// never re-issued (see [`OverlayBlockState::Quarantined`]).
    ///
    /// The `DuplicateKey` retry is what makes this safe without a lock: two
    /// concurrent allocations computed off the same "highest end" produce
    /// either the SAME start (one loses the unique index, retries, and lands
    /// above the winner) or buddy-aligned disjoint starts. There is no
    /// interleaving that yields a partial overlap — see
    /// `aligned_starts_are_never_partially_overlapping` in the models crate.
    pub async fn allocate(
        &self,
        tenant_id: ObjectId,
        network_id: ObjectId,
        prefix: u8,
    ) -> DaoResult<OverlayBlock> {
        let slots = block_slots_for_prefix(prefix).ok_or_else(|| {
            DaoError::Validation(format!(
                "unsupported overlay block prefix /{prefix} (supported: /16 … /22)"
            ))
        })?;

        const ATTEMPTS: usize = 8;
        for attempt in 1..=ATTEMPTS {
            let after = self.highest_end_slot().await?.max(OVERLAY_BLOCK_FIRST_SLOT);
            let slot = block_align_slot(after, slots);
            let cidr = block_cidr_for_slot(slot, slots).ok_or_else(|| {
                DaoError::Validation(format!(
                    "overlay block space exhausted: no aligned /{prefix} left below \
                     slot {OVERLAY_BLOCK_SLOT_COUNT} (highest allocated end {after})"
                ))
            })?;
            let now = DateTime::now();
            let block = OverlayBlock {
                id: None,
                slot,
                slots,
                end_slot: slot + slots,
                cidr,
                tenant_id,
                network_id,
                state: OverlayBlockState::Assigned,
                released_reason: None,
                released_at: None,
                created_at: now,
                updated_at: now,
            };
            match self.base.insert_one(&block).await {
                Ok(id) => return self.base.find_by_id(id).await,
                Err(DaoError::DuplicateKey(e)) if attempt < ATTEMPTS => {
                    // Either another allocator took this slot (retry lands
                    // above it) or this network already holds an assigned
                    // block — the partial-unique index on `network_id`. The
                    // caller's own `find_assigned_for_network` check makes the
                    // second case a lost race, so re-reading is correct.
                    if let Some(existing) = self.find_assigned_for_network(network_id).await? {
                        return Ok(existing);
                    }
                    tracing::debug!(
                        %tenant_id, slot, attempt, %e,
                        "overlay block: slot taken; re-allocating"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        Err(DaoError::Validation(
            "overlay block allocation lost too many races; try again".to_string(),
        ))
    }

    /// The range [`Self::allocate`] would hand out RIGHT NOW for `prefix`,
    /// without consuming it — the dry-run's view.
    ///
    /// Advisory by construction: another tenant migrating in between takes
    /// the range first. The apply path always echoes the block it actually
    /// got, and the plan is ordinal-based, so a different range shifts only
    /// the network part of every address.
    pub async fn preview_next_cidr(&self, prefix: u8) -> DaoResult<String> {
        let slots = block_slots_for_prefix(prefix).ok_or_else(|| {
            DaoError::Validation(format!(
                "unsupported overlay block prefix /{prefix} (supported: /16 … /22)"
            ))
        })?;
        let after = self.highest_end_slot().await?.max(OVERLAY_BLOCK_FIRST_SLOT);
        let slot = block_align_slot(after, slots);
        block_cidr_for_slot(slot, slots).ok_or_else(|| {
            DaoError::Validation(format!(
                "overlay block space exhausted: no aligned /{prefix} left below \
                 slot {OVERLAY_BLOCK_SLOT_COUNT} (highest allocated end {after})"
            ))
        })
    }

    /// Retire a block: it stops backing its network and its slots are never
    /// handed out again. `reason` lands in the row for the audit trail.
    pub async fn quarantine(&self, block_id: ObjectId, reason: &str) -> DaoResult<bool> {
        let now = DateTime::now();
        self.base
            .update_one(
                doc! { "_id": block_id },
                doc! { "$set": {
                    "state": bson::to_bson(&OverlayBlockState::Quarantined)
                        .unwrap_or(bson::Bson::Null),
                    "released_reason": reason,
                    "released_at": now,
                    "updated_at": now,
                }},
            )
            .await
    }

    /// Highest `slot + slots` in the registry, or 0 when it is empty. One
    /// indexed `sort + limit 1`.
    async fn highest_end_slot(&self) -> DaoResult<u32> {
        let top = self
            .base
            .collection()
            .find_one(doc! {})
            .sort(doc! { "end_slot": -1 })
            .await?;
        Ok(top.map(|b| b.end_slot).unwrap_or(0))
    }
}
