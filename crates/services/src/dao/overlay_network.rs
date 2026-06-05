use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use mongodb::options::ReturnDocument;
use roomler_ai_remote_control::models::OverlayNetwork;

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
            mtu: OverlayNetwork::DEFAULT_MTU,
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

    /// Atomically claim the next host number for `network_id`, returning
    /// the value that was current BEFORE the increment (i.e. the host to
    /// assign). Monotonic; never recycled while node rows live.
    pub async fn allocate_host(&self, network_id: ObjectId) -> DaoResult<u32> {
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
}
