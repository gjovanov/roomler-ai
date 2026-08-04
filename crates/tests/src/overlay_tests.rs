//! Integration tests for overlay IP allocation and RELEASE.
//!
//! Removing a device from the fleet returns its host number to the tenant's
//! free pool so a future join can recycle it. Each test here locks one decision
//! that the release design turns on — reuse, idempotency under a double
//! release, the live-scoped uniqueness that lets a tombstone stop holding its
//! address, and "removal is final" for a machine that re-enrolls.
//!
//! The IPAM tests drive the DAOs directly against the spawned app's database:
//! allocation happens on the `rc:overlay.join` WS message, and standing up two
//! real agent WS sessions to exercise a pure counter would test the harness
//! rather than the allocator.

use bson::{doc, oid::ObjectId};
use roomler_ai_remote_control::models::{NodeRef, OverlayNetwork, OverlayNode};
use roomler_ai_services::dao::base::DaoError;
use roomler_ai_services::dao::overlay_network::OverlayNetworkDao;
use roomler_ai_services::dao::overlay_node::OverlayNodeDao;
use serde_json::Value;

use crate::fixtures::test_app::TestApp;

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

struct Ipam {
    networks: OverlayNetworkDao,
    nodes: OverlayNodeDao,
    tenant_id: ObjectId,
    network_id: ObjectId,
}

impl Ipam {
    async fn new(app: &TestApp, tenant_id: ObjectId) -> Self {
        let networks = OverlayNetworkDao::new(&app.db);
        let nodes = OverlayNodeDao::new(&app.db);
        let network_id = networks
            .get_or_create(tenant_id)
            .await
            .unwrap()
            .id
            .expect("network _id");
        Self {
            networks,
            nodes,
            tenant_id,
            network_id,
        }
    }

    async fn network(&self) -> OverlayNetwork {
        self.networks
            .base
            .find_by_id(self.network_id)
            .await
            .unwrap()
    }

    /// Allocate a host bounded by the default `/10` ceiling — what
    /// `handle_overlay_join` passes for an unmigrated tenant (P2a).
    async fn alloc(&self) -> Result<u32, DaoError> {
        self.networks
            .allocate_host(self.network_id, default_max_host())
            .await
    }

    /// Insert a node holding `overlay_ip`, as `handle_overlay_join` would.
    async fn create_node(
        &self,
        machine_id: &str,
        name: &str,
        overlay_ip: &str,
    ) -> Result<OverlayNode, DaoError> {
        self.nodes
            .create(
                self.tenant_id,
                NodeRef::Agent {
                    agent_id: ObjectId::new(),
                },
                self.network_id,
                machine_id.to_string(),
                name.to_string(),
                overlay_ip.to_string(),
                "pk-base64".to_string(),
                0,
                vec![],
                false,
                false,
                false,
                false,
                vec![],
            )
            .await
    }
}

/// The tenant `_id` behind a seeded tenant.
fn tid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

/// The default `/10`'s block ceiling (multi-org P2a bound).
fn default_max_host() -> u32 {
    roomler_ai_remote_control::models::cidr_max_host(
        roomler_ai_remote_control::models::OverlayNetwork::DEFAULT_CIDR,
    )
    .expect("default CIDR has a ceiling")
}

// ────────────────────────────────────────────────────────────────────────────
// IPAM: the block ceiling (multi-org P2a)
// ────────────────────────────────────────────────────────────────────────────

/// The cursor refuses to lease past the network's own block, exhaustion is a
/// loud Validation error (never a neighbor-block address), and a pooled host
/// stranded above a since-shrunk ceiling is discarded — leak, never conflict.
#[tokio::test]
async fn allocate_host_stops_at_the_block_ceiling() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovipamcap").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    // A /30-sized ceiling: hosts 1..=2 leaseable.
    let max_host = 2u32;
    let a = ipam
        .networks
        .allocate_host(ipam.network_id, max_host)
        .await
        .unwrap();
    let b = ipam
        .networks
        .allocate_host(ipam.network_id, max_host)
        .await
        .unwrap();
    assert_eq!((a, b), (1, 2));
    let err = ipam
        .networks
        .allocate_host(ipam.network_id, max_host)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, DaoError::Validation(m) if m.contains("exhausted")),
        "expected loud exhaustion, got {err:?}"
    );
    // The cursor did NOT walk past the ceiling.
    assert_eq!(ipam.network().await.next_host, 3);

    // Release host 2 back to the pool, then shrink the ceiling below it: the
    // pooled entry is discarded and allocation reports exhaustion (host 1 is
    // still leased; the cursor is past the new ceiling of 1).
    assert!(
        ipam.networks
            .release_host(ipam.network_id, 2)
            .await
            .unwrap()
    );
    let err = ipam
        .networks
        .allocate_host(ipam.network_id, 1)
        .await
        .unwrap_err();
    assert!(matches!(&err, DaoError::Validation(m) if m.contains("exhausted")));
    assert!(
        ipam.network().await.free_hosts.is_empty(),
        "the above-ceiling pooled host must be discarded, not re-leased"
    );

    // The full-range ceiling still hands out the next cursor host.
    let c = ipam.alloc().await.unwrap();
    assert_eq!(c, 3);
}

// ────────────────────────────────────────────────────────────────────────────
// IPAM: the free pool
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn allocate_host_is_monotonic_when_the_pool_is_empty() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovipam1").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(ipam.alloc().await.unwrap());
    }
    assert_eq!(got, vec![1, 2, 3], "host 0 is the network address");
    let net = ipam.network().await;
    assert_eq!(net.next_host, 4);
    assert!(net.free_hosts.is_empty());
}

#[tokio::test]
async fn release_host_pools_the_number_and_the_next_allocate_reuses_it() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovipam2").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    for _ in 0..3 {
        ipam.alloc().await.unwrap();
    }
    assert!(
        ipam.networks
            .release_host(ipam.network_id, 2)
            .await
            .unwrap()
    );
    assert_eq!(ipam.network().await.free_hosts, vec![2]);

    // The recycled number comes back BEFORE the cursor moves.
    assert_eq!(ipam.alloc().await.unwrap(), 2);
    let net = ipam.network().await;
    assert_eq!(net.next_host, 4, "the cursor did not move");
    assert!(net.free_hosts.is_empty());
}

#[tokio::test]
async fn release_host_is_fifo() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovipam3").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    for _ in 0..4 {
        ipam.alloc().await.unwrap();
    }
    ipam.networks
        .release_host(ipam.network_id, 2)
        .await
        .unwrap();
    ipam.networks
        .release_host(ipam.network_id, 3)
        .await
        .unwrap();

    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(ipam.alloc().await.unwrap());
    }
    assert_eq!(
        got,
        vec![2, 3, 5],
        "oldest release first, then back to the cursor"
    );
}

/// An admin evicting a node at the same moment the agent DELETE fires must not
/// seed the pool twice — that would hand two live nodes one address and lock the
/// second out of the overlay on the unique index.
#[tokio::test]
async fn release_host_is_idempotent() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovipam4").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    for _ in 0..3 {
        ipam.alloc().await.unwrap();
    }
    ipam.networks
        .release_host(ipam.network_id, 2)
        .await
        .unwrap();
    ipam.networks
        .release_host(ipam.network_id, 2)
        .await
        .unwrap();
    assert_eq!(ipam.network().await.free_hosts, vec![2], "no duplicate");

    let a = ipam.alloc().await.unwrap();
    let b = ipam.alloc().await.unwrap();
    assert_eq!((a, b), (2, 4), "2 is handed out once, never twice");
}

#[tokio::test]
async fn release_host_rejects_hosts_the_cursor_never_issued() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovipam5").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    for _ in 0..3 {
        ipam.alloc().await.unwrap();
    }
    // Host 0 is the network address; 9_999 was never issued (cursor is at 4).
    assert!(
        !ipam
            .networks
            .release_host(ipam.network_id, 0)
            .await
            .unwrap()
    );
    assert!(
        !ipam
            .networks
            .release_host(ipam.network_id, 9_999)
            .await
            .unwrap()
    );
    assert!(ipam.network().await.free_hosts.is_empty());
}

/// The core race: N joiners hitting a pool with M entries must each get a
/// distinct host. The pop and the cursor bump are separate atomic updates, so
/// this is the test that would catch a double-hand across the two branches.
#[tokio::test]
async fn concurrent_allocate_never_double_hands_a_pooled_host() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovipam6").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    const POOLED: u32 = 50;
    const RACERS: usize = 80;

    // Advance the cursor past the numbers we're about to pool (release_host
    // only accepts hosts the cursor actually issued), then pool them.
    for _ in 0..POOLED {
        ipam.alloc().await.unwrap();
    }
    for h in 1..=POOLED {
        assert!(
            ipam.networks
                .release_host(ipam.network_id, h)
                .await
                .unwrap()
        );
    }
    assert_eq!(ipam.network().await.free_hosts.len(), POOLED as usize);

    let results = futures::future::join_all((0..RACERS).map(|_| {
        let networks = OverlayNetworkDao::new(&app.db);
        let nid = ipam.network_id;
        async move {
            networks
                .allocate_host(nid, default_max_host())
                .await
                .unwrap()
        }
    }))
    .await;

    let mut sorted = results.clone();
    sorted.sort_unstable();
    let mut deduped = sorted.clone();
    deduped.dedup();
    assert_eq!(
        sorted, deduped,
        "every racer got a DISTINCT host: {results:?}"
    );
    // The 50 pooled numbers plus 30 fresh ones off the cursor.
    let expected: Vec<u32> = (1..=POOLED).chain(POOLED + 1..=POOLED + 30).collect();
    assert_eq!(sorted, expected);
    assert!(ipam.network().await.free_hosts.is_empty());
}

// ────────────────────────────────────────────────────────────────────────────
// The tombstone / index contract
// ────────────────────────────────────────────────────────────────────────────

/// A machine can be removed and re-enrolled repeatedly, so many tombstones must
/// coexist with at most one live row. Reaching this assertion at all also proves
/// MongoDB accepted the `$type: "null"` partial filters — a rejected filter
/// would have failed `ensure_indexes` back in `TestApp::spawn`.
#[tokio::test]
async fn two_tombstones_can_share_a_machine_id() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovtomb1").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    let first = ipam
        .create_node("mach-A", "box", "100.64.0.1")
        .await
        .unwrap();
    ipam.nodes.release(first.id.unwrap()).await.unwrap();

    let second = ipam
        .create_node("mach-A", "box-2", "100.64.0.2")
        .await
        .unwrap();
    ipam.nodes.release(second.id.unwrap()).await.unwrap();

    // A third live row alongside two tombstones for the same machine.
    ipam.create_node("mach-A", "box-3", "100.64.0.3")
        .await
        .expect("a live row coexists with its own tombstones");
}

#[tokio::test]
async fn a_released_node_stops_holding_its_ip_and_name() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovtomb2").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    let node = ipam
        .create_node("mach-A", "laptop", "100.64.0.7")
        .await
        .unwrap();
    let before = ipam.nodes.release(node.id.unwrap()).await.unwrap().unwrap();
    assert_eq!(
        before.overlay_ip, "100.64.0.7",
        "the CAS returns the pre-image so the caller can recover the address"
    );

    // A different machine takes over both the address and the name.
    ipam.create_node("mach-B", "laptop", "100.64.0.7")
        .await
        .expect("the tombstone holds neither its address nor its name");
}

/// The other half: the partial filters must not be so loose that two LIVE nodes
/// can collide.
#[tokio::test]
async fn two_live_nodes_still_cannot_share_an_ip_or_a_name() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovtomb3").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    ipam.create_node("mach-A", "laptop", "100.64.0.7")
        .await
        .unwrap();

    let dup_ip = ipam.create_node("mach-B", "desktop", "100.64.0.7").await;
    assert!(
        matches!(dup_ip, Err(DaoError::DuplicateKey(_))),
        "two live nodes cannot share an address, got {dup_ip:?}"
    );

    let dup_name = ipam.create_node("mach-C", "laptop", "100.64.0.8").await;
    assert!(
        matches!(dup_name, Err(DaoError::DuplicateKey(_))),
        "two live nodes cannot share a name, got {dup_name:?}"
    );
}

#[tokio::test]
async fn find_live_by_tenant_and_machine_ignores_tombstones() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovtomb4").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    let dead = ipam
        .create_node("mach-A", "old", "100.64.0.1")
        .await
        .unwrap();
    ipam.nodes.release(dead.id.unwrap()).await.unwrap();

    assert!(
        ipam.nodes
            .find_live_by_tenant_and_machine(ipam.tenant_id, "mach-A")
            .await
            .unwrap()
            .is_none(),
        "a tombstone is not a live node"
    );

    let live = ipam
        .create_node("mach-A", "new", "100.64.0.2")
        .await
        .unwrap();
    let found = ipam
        .nodes
        .find_live_by_tenant_and_machine(ipam.tenant_id, "mach-A")
        .await
        .unwrap()
        .expect("the live row is found alongside its tombstone");
    assert_eq!(found.id, live.id);
}

/// The CAS is the release token: only one caller may pool the host.
#[tokio::test]
async fn release_is_a_compare_and_swap() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovtomb5").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    let node = ipam
        .create_node("mach-A", "box", "100.64.0.1")
        .await
        .unwrap();
    let id = node.id.unwrap();

    assert!(
        ipam.nodes.release(id).await.unwrap().is_some(),
        "first wins"
    );
    assert!(
        ipam.nodes.release(id).await.unwrap().is_none(),
        "the second caller must NOT also pool the host"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// REST: the evict endpoint
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn evict_releases_the_ip_and_the_next_allocate_reuses_it() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovevict1").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    // Two nodes; evict the one holding host 2.
    ipam.alloc().await.unwrap();
    ipam.alloc().await.unwrap();
    ipam.create_node("mach-A", "keep", "100.64.0.1")
        .await
        .unwrap();
    let victim = ipam
        .create_node("mach-B", "go", "100.64.0.2")
        .await
        .unwrap();
    let victim_id = victim.id.unwrap().to_hex();

    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/overlay-node/{victim_id}", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["released"], true);
    assert_eq!(body["overlay_ip"], "100.64.0.2");
    assert_eq!(body["host_recycled"], true);

    // Gone from the admin list, and its host is back in the pool.
    let list: Value = app
        .auth_get(
            &format!("/api/tenant/{}/overlay-node", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&victim_id.as_str()));

    assert_eq!(ipam.network().await.free_hosts, vec![2]);
    assert_eq!(
        ipam.alloc().await.unwrap(),
        2,
        "the next joiner recycles the released address"
    );
}

#[tokio::test]
async fn evict_is_404_for_an_already_released_node() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovevict2").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;
    ipam.alloc().await.unwrap();
    let node = ipam
        .create_node("mach-A", "box", "100.64.0.1")
        .await
        .unwrap();
    let path = format!(
        "/api/tenant/{}/overlay-node/{}",
        seeded.tenant_id,
        node.id.unwrap().to_hex()
    );

    let first = app
        .auth_delete(&path, &seeded.admin.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 200);

    let second = app
        .auth_delete(&path, &seeded.admin.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status().as_u16(), 404);
}

/// The tenant scoping is structural (the lookup runs against the caller's own
/// network), so a foreign node must read as absent rather than evictable.
#[tokio::test]
async fn evict_is_cross_tenant_safe() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("ovevictA").await;
    let b = app.seed_tenant("ovevictB").await;
    let ipam_b = Ipam::new(&app, tid(&b.tenant_id)).await;
    ipam_b.alloc().await.unwrap();
    let victim = ipam_b
        .create_node("mach-B", "box", "100.64.0.1")
        .await
        .unwrap();

    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/overlay-node/{}",
                a.tenant_id,
                victim.id.unwrap().to_hex()
            ),
            &a.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "not 200 — and not evicted");

    let still_there = ipam_b
        .nodes
        .base
        .find_by_id(victim.id.unwrap())
        .await
        .unwrap();
    assert!(still_there.deleted_at.is_none());
}

#[tokio::test]
async fn evict_requires_manage_agents() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovevict3").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;
    ipam.alloc().await.unwrap();
    let node = ipam
        .create_node("mach-A", "box", "100.64.0.1")
        .await
        .unwrap();

    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/overlay-node/{}",
                seeded.tenant_id,
                node.id.unwrap().to_hex()
            ),
            // A plain member, not the admin.
            &seeded.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

// ────────────────────────────────────────────────────────────────────────────
// REST: the agent-delete cascade
// ────────────────────────────────────────────────────────────────────────────

/// Enroll an agent and return `(agent_id, machine_id)`.
async fn enroll_agent(
    app: &TestApp,
    tenant_id: &str,
    admin_token: &str,
    machine_id: &str,
) -> String {
    let et: Value = app
        .auth_post(
            &format!("/api/tenant/{tenant_id}/agent/enroll-token"),
            admin_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp: Value = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&serde_json::json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": machine_id,
            "machine_name": "Test Box",
            "os": "linux",
            "agent_version": "0.3.0",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["agent_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn agent_delete_releases_the_overlay_node() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovcasc1").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    let agent_id = enroll_agent(
        &app,
        &seeded.tenant_id,
        &seeded.admin.access_token,
        "mach-cascade",
    )
    .await;

    // The node the agent's `rc:overlay.join` would have created.
    ipam.alloc().await.unwrap();
    let node = ipam
        .nodes
        .create(
            ipam.tenant_id,
            NodeRef::Agent {
                agent_id: tid(&agent_id),
            },
            ipam.network_id,
            "mach-cascade".to_string(),
            "testbox".to_string(),
            "100.64.0.1".to_string(),
            "pk".to_string(),
            0,
            vec![],
            false,
            false,
            false,
            false,
            vec![],
        )
        .await
        .unwrap();

    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/{agent_id}", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["deleted"], true);
    assert_eq!(body["overlay_released"], true);
    assert_eq!(body["overlay_ip"], "100.64.0.1");

    let after = ipam.nodes.base.find_by_id(node.id.unwrap()).await.unwrap();
    assert!(after.deleted_at.is_some(), "the node is tombstoned");
    assert_eq!(ipam.network().await.free_hosts, vec![1]);
}

#[tokio::test]
async fn agent_delete_is_404_for_an_unknown_agent() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovcasc2").await;

    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/agent/{}",
                seeded.tenant_id,
                ObjectId::new().to_hex()
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "deleting a nonexistent agent used to report success"
    );
}

/// Removal is FINAL. Re-enrolling the same machine rehydrates the AGENT row (a
/// long-standing contract), but its overlay node is a brand-new row with a fresh
/// lease — the tombstone is never revived.
#[tokio::test]
async fn a_re_enrolled_removed_machine_gets_a_fresh_overlay_node() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovcasc3").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    let agent_id = enroll_agent(
        &app,
        &seeded.tenant_id,
        &seeded.admin.access_token,
        "mach-rejoin",
    )
    .await;
    ipam.alloc().await.unwrap();
    let first = ipam
        .nodes
        .create(
            ipam.tenant_id,
            NodeRef::Agent {
                agent_id: tid(&agent_id),
            },
            ipam.network_id,
            "mach-rejoin".to_string(),
            "rejoiner".to_string(),
            "100.64.0.1".to_string(),
            "pk".to_string(),
            0,
            vec![],
            false,
            false,
            false,
            false,
            vec![],
        )
        .await
        .unwrap();

    app.auth_delete(
        &format!("/api/tenant/{}/agent/{agent_id}", seeded.tenant_id),
        &seeded.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Re-enrolling the same machine revives the AGENT row in place…
    let again = enroll_agent(
        &app,
        &seeded.tenant_id,
        &seeded.admin.access_token,
        "mach-rejoin",
    )
    .await;
    assert_eq!(again, agent_id, "the agent row rehydrates to the same id");

    // …but the overlay lookup no longer finds the tombstone, so the join path
    // takes the fresh-node branch.
    assert!(
        ipam.nodes
            .find_live_by_tenant_and_machine(ipam.tenant_id, "mach-rejoin")
            .await
            .unwrap()
            .is_none()
    );
    let second = ipam
        .create_node("mach-rejoin", "rejoiner", "100.64.0.9")
        .await
        .expect("a fresh node row for the re-enrolled machine");
    assert_ne!(second.id, first.id, "a NEW node id, not the revived one");

    let rows = ipam
        .nodes
        .base
        .find_many(
            doc! { "tenant_id": ipam.tenant_id, "machine_id": "mach-rejoin" },
            None,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "one tombstone + one live row");
    assert_eq!(
        rows.iter().filter(|r| r.deleted_at.is_none()).count(),
        1,
        "exactly one live row"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// REST: the tunnel-client delete
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tunnel_client_delete_releases_the_overlay_node() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovtc1").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    let et: Value = app
        .auth_post(
            &format!(
                "/api/tenant/{}/tunnel-client/enroll-token",
                seeded.tenant_id
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let enrolled: Value = app
        .client
        .post(app.url("/api/tunnel-client/enroll"))
        .json(&serde_json::json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": "mach-tc",
            "machine_name": "Laptop",
            "os": "linux",
            "client_version": "0.3.0",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = enrolled["tunnel_client_id"].as_str().unwrap().to_string();

    ipam.alloc().await.unwrap();
    let node = ipam
        .nodes
        .create(
            ipam.tenant_id,
            NodeRef::TunnelClient {
                tunnel_client_id: tid(&client_id),
            },
            ipam.network_id,
            "mach-tc".to_string(),
            "laptop".to_string(),
            "100.64.0.1".to_string(),
            "pk".to_string(),
            0,
            vec![],
            false,
            false,
            false,
            false,
            vec![],
        )
        .await
        .unwrap();

    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/tunnel-client/{client_id}", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["deleted"], true);
    assert_eq!(body["overlay_released"], true);
    assert_eq!(body["overlay_ip"], "100.64.0.1");

    let after = ipam.nodes.base.find_by_id(node.id.unwrap()).await.unwrap();
    assert!(after.deleted_at.is_some());
    assert_eq!(ipam.network().await.free_hosts, vec![1]);

    // And it drops out of the tunnel-client list.
    let list: Value = app
        .auth_get(
            &format!("/api/tenant/{}/tunnel-client", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["items"].as_array().unwrap().len(), 0);
}

/// An agent and a tunnel client on the SAME box share a `machine_id`, and only
/// one of them can own the overlay node. Deleting the agent must not release a
/// node the still-enrolled tunnel client owns.
#[tokio::test]
async fn agent_delete_does_not_release_a_tunnel_clients_node() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("ovtc2").await;
    let ipam = Ipam::new(&app, tid(&seeded.tenant_id)).await;

    let agent_id = enroll_agent(
        &app,
        &seeded.tenant_id,
        &seeded.admin.access_token,
        "mach-shared",
    )
    .await;

    // The node on that machine belongs to a TUNNEL CLIENT, not the agent.
    ipam.alloc().await.unwrap();
    let node = ipam
        .nodes
        .create(
            ipam.tenant_id,
            NodeRef::TunnelClient {
                tunnel_client_id: ObjectId::new(),
            },
            ipam.network_id,
            "mach-shared".to_string(),
            "shared".to_string(),
            "100.64.0.1".to_string(),
            "pk".to_string(),
            0,
            vec![],
            false,
            false,
            false,
            false,
            vec![],
        )
        .await
        .unwrap();

    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/{agent_id}", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["overlay_released"], false, "the node was not ours");

    let after = ipam.nodes.base.find_by_id(node.id.unwrap()).await.unwrap();
    assert!(
        after.deleted_at.is_none(),
        "the tunnel client keeps its overlay node"
    );
    assert!(ipam.network().await.free_hosts.is_empty());
}
