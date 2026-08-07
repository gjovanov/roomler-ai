use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_remote_control::models::{
    OVERLAY_BLOCK_FIRST_SLOT, OVERLAY_BLOCK_SLOT_COUNT, OverlayBlock, OverlayBlockState,
    block_align_slot, block_cidr_for_slot, block_slots_for_prefix,
};

use super::base::{BaseDao, DaoError, DaoResult};

/// Fleet-wide slot accounting — see [`OverlayBlockDao::headroom`].
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct BlockHeadroom {
    /// Slots the allocator may ever hand out (the `/10` minus the legacy
    /// `/16` reservation).
    pub total: u32,
    /// Slots below the allocation cursor. Monotonic: never goes down.
    pub used: u32,
    /// Slots inside quarantined blocks — spent and, unless an operator
    /// reclaims them, never re-issued.
    pub burned: u32,
    /// Slots an operator HAS reclaimed. Available again, but only as the
    /// allocator's exhaustion fallback.
    pub reclaimed: u32,
}

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
            let cidr = match block_cidr_for_slot(slot, slots) {
                Some(c) => c,
                // Monotonic space is gone. THIS is the only situation that
                // justifies re-issuing a range an operator has reclaimed —
                // the alternative is handing the tenant no address at all.
                None => {
                    if let Some(b) = self.take_reclaimed(tenant_id, network_id, slots).await? {
                        tracing::warn!(
                            %tenant_id, cidr = %b.cidr, slot = b.slot,
                            "overlay blocks: monotonic space exhausted; re-issued a \
                             RECLAIMED range"
                        );
                        return Ok(b);
                    }
                    return Err(DaoError::Validation(format!(
                        "overlay block space exhausted: no aligned /{prefix} left below \
                         slot {OVERLAY_BLOCK_SLOT_COUNT} (highest allocated end {after}), \
                         and no reclaimed range of that size is available"
                    )));
                }
            };
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

    /// Exhaustion fallback — take over a RECLAIMED row of exactly `slots`.
    ///
    /// Reuses the row rather than inserting a new one: the `slot` index is
    /// unique, so a reclaimed row occupying that slot would collide with any
    /// fresh insert there. The CAS on `state` is what makes two racing
    /// allocators safe — the loser sees zero modified and asks for another.
    ///
    /// Exact-size only, deliberately. Splitting a reclaimed run or merging
    /// adjacent ones is real allocator work, and the payoff is nil for a
    /// registry that has to be 4032 blocks deep before it gets here at all.
    async fn take_reclaimed(
        &self,
        tenant_id: ObjectId,
        network_id: ObjectId,
        slots: u32,
    ) -> DaoResult<Option<OverlayBlock>> {
        let now = DateTime::now();
        let candidates = self
            .base
            .find_many(
                doc! {
                    "state": bson::to_bson(&OverlayBlockState::Reclaimed)
                        .unwrap_or(bson::Bson::Null),
                    "slots": slots,
                },
                Some(doc! { "slot": 1 }),
            )
            .await?;
        for c in candidates {
            let Some(id) = c.id else { continue };
            let claimed = self
                .base
                .update_one(
                    doc! {
                        "_id": id,
                        "state": bson::to_bson(&OverlayBlockState::Reclaimed)
                            .unwrap_or(bson::Bson::Null),
                    },
                    doc! { "$set": {
                        "state": bson::to_bson(&OverlayBlockState::Assigned)
                            .unwrap_or(bson::Bson::Null),
                        "tenant_id": tenant_id,
                        "network_id": network_id,
                        "released_reason": bson::Bson::Null,
                        "released_at": bson::Bson::Null,
                        "updated_at": now,
                    }},
                )
                .await?;
            if claimed {
                return self.base.find_by_id(id).await.map(Some);
            }
        }
        Ok(None)
    }

    /// Mark a QUARANTINED block reclaimable — see
    /// [`OverlayBlockState::Reclaimed`]. Refuses anything not currently
    /// quarantined, so an assigned block can never be reclaimed by mistake.
    pub async fn reclaim(&self, block_id: ObjectId, reason: &str) -> DaoResult<bool> {
        let now = DateTime::now();
        self.base
            .update_one(
                doc! {
                    "_id": block_id,
                    "state": bson::to_bson(&OverlayBlockState::Quarantined)
                        .unwrap_or(bson::Bson::Null),
                },
                doc! { "$set": {
                    "state": bson::to_bson(&OverlayBlockState::Reclaimed)
                        .unwrap_or(bson::Bson::Null),
                    "released_reason": reason,
                    "updated_at": now,
                }},
            )
            .await
    }

    /// Every quarantined row, oldest release first — the reclaim candidate
    /// list, which the route then filters by age and live occupancy.
    pub async fn list_quarantined(&self) -> DaoResult<Vec<OverlayBlock>> {
        self.base
            .find_many(
                doc! { "state": bson::to_bson(&OverlayBlockState::Quarantined)
                .unwrap_or(bson::Bson::Null) },
                Some(doc! { "released_at": 1 }),
            )
            .await
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

    /// Fleet-wide slot accounting for the block space.
    ///
    /// Quarantined blocks are never re-issued: retiring one BURNS its slots
    /// for the life of the deployment. That is deliberate — a re-issued
    /// range could collide with an agent still holding stale peers from the
    /// previous tenant — and with 4032 slots it is nowhere near mattering.
    /// "Nowhere near" should be a number an operator can read, though, not a
    /// belief, so it is reported: the day `burned` becomes a meaningful
    /// fraction of `total` is the day a reclaim path earns its risk
    /// (docs/multi-org.md §12).
    ///
    /// `used` is the allocation cursor, not a live count — allocation only
    /// ever moves upward, so it already includes everything burned below it.
    pub async fn headroom(&self) -> DaoResult<BlockHeadroom> {
        let cursor = self.highest_end_slot().await?.max(OVERLAY_BLOCK_FIRST_SLOT);
        let quarantined = self
            .base
            .find_many(
                doc! { "state": bson::to_bson(&OverlayBlockState::Quarantined)
                .unwrap_or(bson::Bson::Null) },
                None,
            )
            .await?;
        let reclaimed = self
            .base
            .find_many(
                doc! { "state": bson::to_bson(&OverlayBlockState::Reclaimed)
                .unwrap_or(bson::Bson::Null) },
                None,
            )
            .await?;
        Ok(BlockHeadroom {
            total: OVERLAY_BLOCK_SLOT_COUNT - OVERLAY_BLOCK_FIRST_SLOT,
            used: cursor - OVERLAY_BLOCK_FIRST_SLOT,
            burned: quarantined.iter().map(|b| b.slots).sum(),
            reclaimed: reclaimed.iter().map(|b| b.slots).sum(),
        })
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
