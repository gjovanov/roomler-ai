// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A tunnel client lasts only as long as its owner's org membership.
//!
//! A tunnel client acts for the person whose enrollment token enrolled it
//! (`TunnelClient::owner_user_id`; for a paired client, the person who
//! approved the pairing): the forward gate matches `AllUsers` and that
//! person's `UserId` grants, and its overlay node peers with the org's mesh.
//! So when that person stops being a member of the org (an admin removes
//! them, or they leave), the tunnel clients they own there are removed the
//! way a device manager's `DELETE` removes one
//! ([`end_tunnel_client`]: the row, then the overlay lease):
//! [`MembershipLifecycle::org_member_removed`], run by the host's member route
//! once the membership row is gone. Their sockets close at the next
//! revocation poll (60 s), on whichever pod holds them.
//!
//! Devices (agents) the person enrolled are not touched here.
//!
//! **This hook never stops the cascade.** It tries every client, logs each
//! one it could not finish at ERROR (`alert = membership_tunnel_end`), and
//! answers `Ok`. The cascade stops on a failing holder because `chat` must
//! not drop the room rows `conference` reads to find a person's calls; a
//! failure here touches neither, and stopping would leave the person in a
//! live call and on the room fan-out because of a tunnel-client write. What
//! a failure leaves (a live client, or the live lease of a removed one, of
//! someone who is not a member) is exactly what [`prune`] finds and ends:
//! `POST /api/admin/tunnel-clients/prune`, an operator step, also for the
//! clients of people removed before this hook existed.
//!
//! [`end_tunnel_client`]: crate::routes::tunnel::end_tunnel_client

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::{Json, extract::State};
use bson::{Bson, Document, doc, oid::ObjectId};
use roomler_ai_services::dao::base::DaoError;
use roomler_core::{ApiError, MembershipLifecycle, extractors::auth::AuthUser};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::NetworkState;
use crate::overlay::ReleasedNode;
use crate::routes::tunnel::end_tunnel_client;

/// The `alert` field of every log line about a tunnel client a membership's
/// end could not remove.
pub(crate) const ALERT: &str = "membership_tunnel_end";

/// The reason the overlay releases of a membership's end are logged with.
const REASON: &str = "member_removed";

/// How long a role change waits before its re-fan starts, so a burst of
/// changes to one org (an admin assigning a role to several people) costs one
/// re-fan, not one each.
const ROLES_REFAN_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(750);

/// Network's [`MembershipLifecycle`]: the hook `Module::hooks` registers.
pub struct MembershipHooks {
    pub state: NetworkState,
    /// The overlay re-fans role changes ask for, at most one per org at a time
    /// ([`MembershipLifecycle::org_roles_changed`]).
    roles_refan: RefanQueue,
}

impl MembershipHooks {
    pub fn new(state: NetworkState) -> Self {
        Self {
            state,
            roles_refan: RefanQueue::new(ROLES_REFAN_DEBOUNCE),
        }
    }
}

#[async_trait]
impl MembershipLifecycle for MembershipHooks {
    /// Who holds which role in the org changed: re-fan its overlay so the
    /// nodes stop (or start) matching role-scoped rules now, not at their next
    /// reconnect. Scheduled, never inline, and never two at once for one org
    /// ([`RefanQueue`]).
    async fn org_roles_changed(&self, tenant_id: ObjectId) {
        let state = self.state.clone();
        self.roles_refan.request(tenant_id, move |tenant| {
            let state = state.clone();
            async move {
                crate::routes::overlay_policy::refan_for_role_change(&state, tenant).await;
            }
        });
    }

    async fn org_member_removed(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
    ) -> anyhow::Result<()> {
        let ended = end_tunnel_clients_of(&self.state, tenant_id, user_id).await;
        if ended.clients > 0 || ended.leases > 0 || ended.failures > 0 {
            info!(
                tenant = %tenant_id, user = %user_id,
                clients = ended.clients, leases = ended.leases, failures = ended.failures,
                "tunnel clients ended with the org membership"
            );
        }
        // Never `Err`: see the module doc.
        Ok(())
    }
}

/// The overlay re-fans role changes ask for: **at most one per org at a time**,
/// and none lost.
///
/// The first change for an org starts a worker for it. The worker waits the
/// debounce, clears the org's "one more run" flag and runs the re-fan. A change
/// that arrives while the worker waits or runs only sets the flag, so any
/// number of changes during a run cost one more run after it. The flag is
/// cleared BEFORE the run reads anything, so every change is followed by a run
/// that started after it. The worker leaves only when a run ends with the flag
/// unset, and it removes the org's mark under the same lock as that check, so a
/// change after that point starts a new worker.
///
/// One re-fan of an org reads on the order of N² rows for N nodes, so runs
/// overlapping for one org would multiply that load on a shared pod.
///
/// ⚠️ The mutex is held for one map operation at a time, never across an
/// await. A worker that panics or is cancelled clears its org's mark in a drop
/// guard ([`WorkerMark`]), so the next change starts a new worker instead of
/// finding the org stuck behind a dead one.
#[derive(Clone)]
pub(crate) struct RefanQueue {
    /// org → "one more run is owed". An org is present while its worker lives.
    orgs: Arc<std::sync::Mutex<HashMap<ObjectId, bool>>>,
    debounce: std::time::Duration,
}

impl RefanQueue {
    pub(crate) fn new(debounce: std::time::Duration) -> Self {
        Self {
            orgs: Arc::default(),
            debounce,
        }
    }

    fn orgs(&self) -> std::sync::MutexGuard<'_, HashMap<ObjectId, bool>> {
        self.orgs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask for a run of `refan` for `org`; returns at once. The run starts
    /// after the debounce, and never while another run for `org` is going.
    pub(crate) fn request<F, Fut>(&self, org: ObjectId, refan: F)
    where
        F: Fn(ObjectId) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        {
            let mut orgs = self.orgs();
            if let Some(owed) = orgs.get_mut(&org) {
                *owed = true;
                return;
            }
            orgs.insert(org, false);
        }
        let queue = self.clone();
        tokio::spawn(async move {
            let mut mark = WorkerMark {
                orgs: queue.orgs.clone(),
                org,
                left: false,
            };
            loop {
                tokio::time::sleep(queue.debounce).await;
                queue.orgs().insert(org, false);
                refan(org).await;
                let again = {
                    let mut orgs = queue.orgs();
                    let owed = orgs.get(&org).copied().unwrap_or(false);
                    if !owed {
                        orgs.remove(&org);
                    }
                    owed
                };
                if !again {
                    mark.left = true;
                    return;
                }
            }
        });
    }

    /// Whether `org` has a worker (waiting or running).
    #[cfg(test)]
    fn busy(&self, org: ObjectId) -> bool {
        self.orgs().contains_key(&org)
    }
}

/// Removes an org's mark when its worker ends without leaving itself (a re-fan
/// that panicked, or a task the runtime dropped): the next change for the org
/// then starts a new worker.
struct WorkerMark {
    orgs: Arc<std::sync::Mutex<HashMap<ObjectId, bool>>>,
    org: ObjectId,
    /// Set by the worker when it removed its own mark.
    left: bool,
}

impl Drop for WorkerMark {
    fn drop(&mut self) {
        if !self.left {
            self.orgs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.org);
        }
    }
}

/// What [`end_tunnel_clients_of`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ended {
    /// Live clients this call removed.
    pub clients: u64,
    /// Overlay leases this call released.
    pub leases: u64,
    /// Steps that failed, each logged; what they left is the prune's.
    pub failures: u64,
}

/// Remove every tunnel client `user_id` owns in org `tenant_id`, and release
/// any overlay lease one of their clients still holds.
///
/// Per client, the removal's own order ([`end_tunnel_client`]: tombstone,
/// then the release); a client whose tombstone failed is not released, so
/// no lease is freed for a client that can still join. Then, for that client
/// and for every client of theirs removed before this call, the lease is read
/// back: the release logs its failures instead of returning them, and a join
/// that raced an earlier removal can leave a lease nobody else would release
/// (`handle_overlay_join`). A lease still live is released once more and read
/// again. So a second call finds nothing left, and finishes what a first call
/// that failed half-way left.
pub(crate) async fn end_tunnel_clients_of(
    state: &NetworkState,
    tenant_id: ObjectId,
    user_id: ObjectId,
) -> Ended {
    let mut ended = Ended::default();
    let clients = match state
        .tunnel_clients
        .list_owned_in_tenant(tenant_id, user_id)
        .await
    {
        Ok(clients) => clients,
        Err(e) => {
            error!(
                tenant = %tenant_id, user = %user_id, %e, alert = ALERT,
                "membership end: could not read the person's tunnel clients; the operator step will find them"
            );
            ended.failures += 1;
            return ended;
        }
    };
    for client in clients {
        let Some(client_id) = client.id else {
            continue;
        };
        if client.deleted_at.is_none() {
            match end_tunnel_client(state, tenant_id, client_id, &client.machine_id, REASON).await {
                Ok(done) => {
                    ended.clients += u64::from(done.tombstoned);
                    ended.leases += u64::from(done.released.is_some());
                }
                Err(e) => {
                    error!(
                        tenant = %tenant_id, user = %user_id, client = %client_id, %e, alert = ALERT,
                        "membership end: could not remove a tunnel client; the operator step will find it"
                    );
                    ended.failures += 1;
                    continue;
                }
            }
        }
        match release_leftover_lease(state, tenant_id, client_id).await {
            Ok(Some(_)) => ended.leases += 1,
            Ok(None) => {}
            Err(e) => {
                error!(
                    tenant = %tenant_id, user = %user_id, client = %client_id, %e, alert = ALERT,
                    "membership end: a removed tunnel client still holds an overlay lease; the operator step will find it"
                );
                ended.failures += 1;
            }
        }
    }
    ended
}

/// The live overlay lease of a REMOVED tunnel client, released and read
/// back: `Ok(None)` when it holds none (the normal case), `Err` when one is
/// still live after the release.
async fn release_leftover_lease(
    state: &NetworkState,
    tenant_id: ObjectId,
    client_id: ObjectId,
) -> anyhow::Result<Option<ReleasedNode>> {
    let Some(node) = state
        .overlay_nodes
        .find_live_by_tunnel_client(tenant_id, client_id)
        .await?
    else {
        return Ok(None);
    };
    let released = crate::overlay::release_overlay_node(state, &node, REASON).await;
    if state
        .overlay_nodes
        .find_live_by_tunnel_client(tenant_id, client_id)
        .await?
        .is_some()
    {
        anyhow::bail!(
            "its overlay lease {} could not be released",
            node.overlay_ip
        );
    }
    Ok(released)
}

/// Does `user_id` still hold anything [`end_tunnel_clients_of`] removes in
/// org `tenant_id`: a live tunnel client, or a live lease of a removed one?
async fn still_holds(
    state: &NetworkState,
    tenant_id: ObjectId,
    user_id: ObjectId,
) -> Result<bool, DaoError> {
    let clients = state
        .tunnel_clients
        .list_owned_in_tenant(tenant_id, user_id)
        .await?;
    if clients.iter().any(|c| c.deleted_at.is_none()) {
        return Ok(true);
    }
    for client_id in clients.iter().filter_map(|c| c.id) {
        if state
            .overlay_nodes
            .find_live_by_tunnel_client(tenant_id, client_id)
            .await?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What one prune call does by default, and at most.
const DEFAULT_PRUNE_LIMIT: u32 = 200;
const MAX_PRUNE_LIMIT: u32 = 2_000;

fn yes() -> bool {
    true
}

/// `POST /api/admin/tunnel-clients/prune`.
#[derive(Debug, Deserialize)]
pub struct PruneRequest {
    /// Count and list only, change nothing. On unless the body says otherwise.
    #[serde(default = "yes")]
    pub dry_run: bool,
    /// How many (org, person) pairs one call may end:
    /// [`DEFAULT_PRUNE_LIMIT`] by default, [`MAX_PRUNE_LIMIT`] at most.
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct PruneResponse {
    pub dry_run: bool,
    /// (org, person) pairs with no org membership that hold a live tunnel
    /// client, or a live overlay lease of a removed one, up to the limit.
    pub pairs: u64,
    /// The live tunnel clients those pairs hold.
    pub clients: u64,
    /// The live overlay leases their clients hold, removed clients' included.
    pub leases: u64,
    /// Pairs this call ended: nothing of theirs is left live (always 0 in a
    /// dry run).
    pub ended: u64,
    /// Pairs the call could not end. Each is in the log, and a later call
    /// tries again.
    pub failed: u64,
    /// More pairs may be waiting past the limit: call again.
    pub more: bool,
    /// What each pair holds: what a real run ends, for reading before one.
    pub items: Vec<PrunePair>,
}

#[derive(Debug, Serialize)]
pub struct PrunePair {
    pub tenant_id: String,
    pub user_id: String,
    /// The pair's live tunnel clients.
    pub clients: Vec<PruneClient>,
    /// The live overlay leases of the pair's clients, removed ones included.
    pub leases: u64,
}

#[derive(Debug, Serialize)]
pub struct PruneClient {
    pub id: String,
    pub name: String,
    /// RFC 3339.
    pub last_seen_at: String,
}

/// End the tunnel clients that outlived their owner's org membership: the
/// clients a removal left before the membership hook existed, and what a
/// removal whose hook could not finish left behind.
///
/// Platform operators only (404 for anyone else, like every `/api/admin`
/// route). A dry run unless `dry_run: false`, bounded by `limit` pairs per
/// call, and idempotent: each pair goes through the same cascade as a live
/// removal ([`HookRegistry::org_member_removed`]: the tunnel clients, then the
/// call seats, then the room rows), re-checked first, so a person who is a
/// member again by now is left alone, and a second call finds nothing left.
/// A pair counts as ended only when nothing of theirs is left live after the
/// cascade, because this module's hook answers `Ok` when it could not finish.
///
/// [`HookRegistry::org_member_removed`]: roomler_core::HookRegistry::org_member_removed
pub async fn prune(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Json(body): Json<PruneRequest>,
) -> Result<Json<PruneResponse>, ApiError> {
    roomler_core::guards::require_platform_admin(&state, &auth)?;
    let limit = body
        .limit
        .unwrap_or(DEFAULT_PRUNE_LIMIT)
        .clamp(1, MAX_PRUNE_LIMIT) as usize;

    let mut pairs = stale_pairs(&state, limit + 1).await?;
    let more = pairs.len() > limit;
    pairs.truncate(limit);
    let clients = pairs.iter().map(|p| p.clients.len() as u64).sum();
    let leases = pairs.iter().map(|p| p.leases).sum();

    let (mut ended, mut failed) = (0, 0);
    if !body.dry_run {
        for pair in &pairs {
            let (tenant_id, user_id) = (pair.tenant_id, pair.user_id);
            if state.tenants.is_member(tenant_id, user_id).await? {
                continue;
            }
            if let Err(e) = state.hooks.org_member_removed(tenant_id, user_id).await {
                failed += 1;
                warn!(
                    tenant = %tenant_id, user = %user_id, %e,
                    "tunnel-client prune: the membership's end failed"
                );
                continue;
            }
            match still_holds(&state, tenant_id, user_id).await {
                Ok(false) => ended += 1,
                Ok(true) => {
                    failed += 1;
                    warn!(
                        tenant = %tenant_id, user = %user_id, alert = ALERT,
                        "tunnel-client prune: a tunnel client or lease is still live after the membership's end"
                    );
                }
                Err(e) => {
                    failed += 1;
                    warn!(
                        tenant = %tenant_id, user = %user_id, %e,
                        "tunnel-client prune: could not read back what the pair holds"
                    );
                }
            }
        }
    }
    info!(
        operator = %auth.user_id, dry_run = body.dry_run, pairs = pairs.len(), clients, leases,
        ended, failed, more,
        "tunnel-client prune"
    );
    Ok(Json(PruneResponse {
        dry_run: body.dry_run,
        pairs: pairs.len() as u64,
        clients,
        leases,
        ended,
        failed,
        more,
        items: pairs
            .into_iter()
            .map(|p| PrunePair {
                tenant_id: p.tenant_id.to_hex(),
                user_id: p.user_id.to_hex(),
                clients: p.clients,
                leases: p.leases,
            })
            .collect(),
    }))
}

/// One (org, person) pair with no org membership and something live.
struct StalePair {
    tenant_id: ObjectId,
    user_id: ObjectId,
    clients: Vec<PruneClient>,
    leases: u64,
}

/// Up to `limit` pairs, in a stable order: every tunnel client grouped by
/// (org, owner), the groups whose owner has no `tenant_members` row in that
/// org (its unique `(tenant_id, user_id)` index), kept when they hold a live
/// client or a live lease of any of their clients. "Live" is `deleted_at`
/// null OR absent, the predicate every live read of these collections uses.
async fn stale_pairs(state: &NetworkState, limit: usize) -> Result<Vec<StalePair>, ApiError> {
    use futures::TryStreamExt;

    let live = |field: &str| doc! { "$eq": [ { "$ifNull": [field, Bson::Null] }, Bson::Null ] };
    let pipeline = vec![
        doc! { "$group": {
            "_id": { "tenant_id": "$tenant_id", "user_id": "$owner_user_id" },
            "ids": { "$push": "$_id" },
            "all": { "$push": {
                "id": "$_id",
                "name": "$name",
                "last_seen_at": "$last_seen_at",
                "deleted_at": "$deleted_at",
            } },
        } },
        doc! { "$lookup": {
            "from": "tenant_members",
            "let": { "t": "$_id.tenant_id", "u": "$_id.user_id" },
            "pipeline": [
                { "$match": { "$expr": { "$and": [
                    { "$eq": ["$tenant_id", "$$t"] },
                    { "$eq": ["$user_id", "$$u"] },
                ] } } },
                { "$limit": 1 },
                { "$project": { "_id": 1 } },
            ],
            "as": "membership",
        } },
        doc! { "$match": { "membership": { "$size": 0 } } },
        doc! { "$lookup": {
            "from": "overlay_nodes",
            "let": { "t": "$_id.tenant_id", "ids": "$ids" },
            "pipeline": [
                { "$match": { "$expr": { "$and": [
                    { "$eq": ["$tenant_id", "$$t"] },
                    { "$eq": ["$node_ref.kind", "tunnel_client"] },
                    { "$in": ["$node_ref.id", "$$ids"] },
                    live("$deleted_at"),
                ] } } },
                { "$project": { "_id": 1 } },
            ],
            "as": "leases",
        } },
        doc! { "$addFields": { "live": { "$filter": {
            "input": "$all",
            "as": "c",
            "cond": live("$$c.deleted_at"),
        } } } },
        doc! { "$match": { "$or": [
            { "live.0": { "$exists": true } },
            { "leases.0": { "$exists": true } },
        ] } },
        doc! { "$sort": { "_id.tenant_id": 1, "_id.user_id": 1 } },
        doc! { "$limit": limit as i64 },
        doc! { "$project": { "live": 1, "leases": { "$size": "$leases" } } },
    ];
    let mut cursor = state
        .tunnel_clients
        .base
        .collection()
        .clone_with_type::<Document>()
        .aggregate(pipeline)
        .allow_disk_use(true)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut out = Vec::new();
    while let Some(group) = cursor
        .try_next()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    {
        let Ok(key) = group.get_document("_id") else {
            continue;
        };
        let (Ok(tenant_id), Ok(user_id)) =
            (key.get_object_id("tenant_id"), key.get_object_id("user_id"))
        else {
            continue;
        };
        let clients = group
            .get_array("live")
            .map(|live| {
                live.iter()
                    .filter_map(Bson::as_document)
                    .filter_map(|c| {
                        Some(PruneClient {
                            id: c.get_object_id("id").ok()?.to_hex(),
                            name: c.get_str("name").unwrap_or_default().to_string(),
                            last_seen_at: c
                                .get_datetime("last_seen_at")
                                .ok()
                                .and_then(|t| t.try_to_rfc3339_string().ok())
                                .unwrap_or_default(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let leases = group
            .get_i32("leases")
            .map(|n| n.max(0) as u64)
            .or_else(|_| group.get_i64("leases").map(|n| n.max(0) as u64))
            .unwrap_or(0);
        out.push(StalePair {
            tenant_id,
            user_id,
            clients,
            leases,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod refan_queue_tests {
    //! [`RefanQueue`] with a probe re-fan instead of the overlay: it counts its
    //! runs and the most that ran at once, and can be held until released.

    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use bson::oid::ObjectId;
    use tokio::sync::watch;

    use super::RefanQueue;

    type Run = Pin<Box<dyn Future<Output = ()> + Send>>;

    #[derive(Clone)]
    struct Probe {
        runs: Arc<AtomicUsize>,
        running: Arc<AtomicUsize>,
        most_at_once: Arc<AtomicUsize>,
        /// A run waits here until this reads `true`.
        gate: Arc<watch::Sender<bool>>,
    }

    impl Probe {
        fn new(open: bool) -> Self {
            Self {
                runs: Arc::default(),
                running: Arc::default(),
                most_at_once: Arc::default(),
                gate: Arc::new(watch::channel(open).0),
            }
        }

        fn open(&self) {
            self.gate.send_replace(true);
        }

        fn runs(&self) -> usize {
            self.runs.load(Ordering::SeqCst)
        }

        fn refan(&self) -> impl Fn(ObjectId) -> Run + Send + 'static {
            let p = self.clone();
            move |_org| {
                let p = p.clone();
                Box::pin(async move {
                    p.runs.fetch_add(1, Ordering::SeqCst);
                    let now = p.running.fetch_add(1, Ordering::SeqCst) + 1;
                    p.most_at_once.fetch_max(now, Ordering::SeqCst);
                    let mut gate = p.gate.subscribe();
                    let _ = gate.wait_for(|open| *open).await;
                    p.running.fetch_sub(1, Ordering::SeqCst);
                })
            }
        }
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..400 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    /// A burst of changes while a re-fan runs costs exactly one more run after
    /// it, and two runs for one org never overlap.
    #[tokio::test]
    async fn a_burst_during_a_run_costs_one_more_run_and_runs_never_overlap() {
        let queue = RefanQueue::new(Duration::from_millis(5));
        let org = ObjectId::new();
        let probe = Probe::new(false);
        queue.request(org, probe.refan());
        until("the first run starts", || probe.runs() == 1).await;
        for _ in 0..50 {
            queue.request(org, probe.refan());
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(probe.runs(), 1, "no second run while the first is held");
        probe.open();
        until("the org is idle", || !queue.busy(org)).await;
        assert_eq!(probe.runs(), 2, "fifty changes during a run cost one more");
        assert_eq!(probe.most_at_once.load(Ordering::SeqCst), 1);
    }

    /// Changes that arrive during the debounce join the run it starts.
    #[tokio::test]
    async fn changes_during_the_debounce_join_one_run() {
        let queue = RefanQueue::new(Duration::from_millis(50));
        let org = ObjectId::new();
        let probe = Probe::new(true);
        for _ in 0..20 {
            queue.request(org, probe.refan());
        }
        until("the org is idle", || !queue.busy(org)).await;
        assert_eq!(probe.runs(), 1);
    }

    /// A change after the worker left starts a new one: nothing is missed.
    #[tokio::test]
    async fn a_change_after_a_run_ended_starts_a_new_run() {
        let queue = RefanQueue::new(Duration::from_millis(5));
        let org = ObjectId::new();
        let probe = Probe::new(true);
        queue.request(org, probe.refan());
        until("the first run ends", || !queue.busy(org)).await;
        queue.request(org, probe.refan());
        until("the second run ends", || !queue.busy(org)).await;
        assert_eq!(probe.runs(), 2);
    }

    /// Two orgs do not wait for each other.
    #[tokio::test]
    async fn two_orgs_run_side_by_side() {
        let queue = RefanQueue::new(Duration::from_millis(5));
        let (a, b) = (ObjectId::new(), ObjectId::new());
        let held = Probe::new(false);
        let free = Probe::new(true);
        queue.request(a, held.refan());
        until("org a's run starts", || held.runs() == 1).await;
        queue.request(b, free.refan());
        until("org b's run ends", || !queue.busy(b)).await;
        assert_eq!(free.runs(), 1, "org b ran while org a's run was held");
        assert!(queue.busy(a));
        held.open();
        until("org a is idle", || !queue.busy(a)).await;
    }

    fn a_refan_that_panics() {
        panic!("a re-fan that panics (expected in this test)");
    }

    /// A re-fan that panics does not leave the org stuck: the next change runs.
    #[tokio::test]
    async fn a_run_that_panics_leaves_the_org_free() {
        let queue = RefanQueue::new(Duration::from_millis(5));
        let org = ObjectId::new();
        queue.request(org, |_org| -> Run {
            Box::pin(async { a_refan_that_panics() })
        });
        until("the panicked worker clears its mark", || !queue.busy(org)).await;
        let probe = Probe::new(true);
        queue.request(org, probe.refan());
        until("the org is idle", || !queue.busy(org)).await;
        assert_eq!(probe.runs(), 1);
    }
}
