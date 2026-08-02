use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use mongodb::options::ReturnDocument;
use roomler_ai_remote_control::models::{OverlayAclMode, OverlayNetwork};

use super::base::{BaseDao, DaoError, DaoResult};

/// IPAM authority for tenant overlay networks. One row per tenant; the
/// host cursor (`next_host`) is bumped atomically so concurrent joins
/// never collide on an overlay IP.
pub struct OverlayNetworkDao {
    pub base: BaseDao<OverlayNetwork>,
}

impl OverlayNetworkDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, OverlayNetwork::COLLECTION),
        }
    }

    /// Fetch the tenant's overlay network, creating it with the default
    /// CIDR/MTU on first use. Race-safe via the `(tenant_id)` unique
    /// index: a losing concurrent insert re-reads the winner's row.
    pub async fn get_or_create(&self, tenant_id: ObjectId) -> DaoResult<OverlayNetwork> {
        if let Some(net) = self.base.find_one(doc! { "tenant_id": tenant_id }).await? {
            return Ok(net);
        }
        let now = DateTime::now();
        let net = OverlayNetwork {
            id: None,
            tenant_id,
            cidr: OverlayNetwork::DEFAULT_CIDR.to_string(),
            // Host 0 is the network address — start handing out from 1.
            next_host: 1,
            free_hosts: Vec::new(),
            mtu: OverlayNetwork::DEFAULT_MTU,
            // New networks start permissive too — the ACL is opt-in per tenant
            // so that turning the feature on can never black-hole a live mesh.
            acl_mode: OverlayAclMode::default(),
            created_at: now,
            updated_at: now,
        };
        match self.base.insert_one(&net).await {
            Ok(id) => self.base.find_by_id(id).await,
            // Lost the create race — the other writer's row is now
            // present; read it back.
            Err(DaoError::DuplicateKey(_)) => self
                .base
                .find_one(doc! { "tenant_id": tenant_id })
                .await?
                .ok_or(DaoError::NotFound),
            Err(e) => Err(e),
        }
    }

    /// Set the tenant's overlay ACL posture (`off` | `warn` | `enforce`).
    ///
    /// Creates the network row if the tenant has never joined the overlay, so
    /// an admin can stage the posture before the first node appears.
    pub async fn set_acl_mode(&self, tenant_id: ObjectId, mode: OverlayAclMode) -> DaoResult<bool> {
        self.get_or_create(tenant_id).await?;
        self.base
            .update_one(
                doc! { "tenant_id": tenant_id },
                doc! { "$set": {
                    "acl_mode": bson::to_bson(&mode).unwrap_or(bson::Bson::Null),
                    "updated_at": DateTime::now(),
                }},
            )
            .await
    }

    /// Claim a host number for `network_id`. Prefers a RECYCLED host from the
    /// tenant's free pool (FIFO — oldest release first, so an address gets the
    /// longest cool-down before reuse); falls back to bumping the monotonic
    /// `next_host` cursor when the pool is empty.
    ///
    /// Both branches are single atomic `find_one_and_update`s, so N concurrent
    /// joiners can never be handed the same host: MongoDB serialises updates to
    /// one document, and each racer gets its OWN pre-image — racer A sees
    /// `[7, 9]` and takes 7 while racer B sees `[9]` and takes 9.
    pub async fn allocate_host(&self, network_id: ObjectId) -> DaoResult<u32> {
        // 1) The pool. `free_hosts.0: {$exists: true}` is the "array is
        //    non-empty" test — an empty pool simply fails to match and we fall
        //    through to the cursor.
        let popped = self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": network_id, "free_hosts.0": { "$exists": true } },
                doc! {
                    "$pop": { "free_hosts": -1 },
                    "$set": { "updated_at": DateTime::now() },
                },
            )
            .return_document(ReturnDocument::Before)
            .await?;
        if let Some(before) = popped
            && let Some(host) = before.free_hosts.first().copied()
        {
            return Ok(host);
        }

        // 2) Pool empty — bump the cursor.
        let before = self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": network_id },
                doc! { "$inc": { "next_host": 1 }, "$set": { "updated_at": DateTime::now() } },
            )
            .return_document(ReturnDocument::Before)
            .await?
            .ok_or(DaoError::NotFound)?;
        Ok(before.next_host)
    }

    /// Return a host number to the tenant's free pool so a future join can
    /// recycle it. `true` when the pool actually accepted it.
    ///
    /// `$addToSet` (not `$push`) is deliberate: a double release — an admin
    /// evicting a node at the same moment the agent DELETE fires — would
    /// otherwise seed the pool with a duplicate, hand two live nodes the same
    /// overlay IP, and trip the unique `(tenant_id, network_id, overlay_ip)`
    /// index on the second `create`, locking that device out of the overlay
    /// permanently. The caller's compare-and-swap tombstone makes that
    /// unreachable; this is the second layer.
    ///
    /// A rejected release is never an error for the caller: the host simply
    /// LEAKS out of a 4.2 M-address `/10`.
    pub async fn release_host(&self, network_id: ObjectId, host: u32) -> DaoResult<bool> {
        // Host 0 is the network address and was never issued.
        if host == 0 {
            return Ok(false);
        }
        let mut filter = doc! {
            "_id": network_id,
            // Only recycle a host the cursor actually issued. Guards against a
            // CIDR edit making the ip→host inverse meaningless.
            "next_host": { "$gt": host as i64 },
        };
        // Size cap: the element at index MAX being absent ⇒ len <= MAX.
        filter.insert(
            format!("free_hosts.{}", OverlayNetwork::MAX_FREE_HOSTS),
            doc! { "$exists": false },
        );

        // NOT via `BaseDao::update_one`: that helper builds an empty
        // `$setOnInsert` when the update carries no `$set`, which Mongo rejects.
        let res = self
            .base
            .collection()
            .update_one(
                filter,
                doc! {
                    "$addToSet": { "free_hosts": host as i64 },
                    "$set": { "updated_at": DateTime::now() },
                },
            )
            .await?;
        Ok(res.modified_count > 0)
    }
}
