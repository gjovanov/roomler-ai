// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use mongodb::{Database, IndexModel, options::IndexOptions};
use serde::Serialize;
use tracing::info;

/// A pre-creation operation on a collection — applied before that
/// collection's indexes are created, in plan order.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IndexOp {
    /// `dropIndex`, tolerating both "index not found" (27) and "collection not
    /// found" (26); anything else is an error.
    DropIndexIfPresent {
        index: &'static str,
        /// Logged when applied — the one line a reader of the boot log needs.
        why: &'static str,
    },
}

/// One collection's index set.
#[derive(Debug, Clone, Serialize)]
pub struct IndexSet {
    pub collection: &'static str,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_ops: Vec<IndexOp>,
    pub indexes: Vec<IndexModel>,
}

/// Every index the app relies on, as data, in the order they are applied.
///
/// FR-69: this is what the composition baseline records and what
/// [`ensure_indexes`] applies — the two cannot drift, because the second is
/// defined in terms of the first. Modules contribute their own sets from P1
/// on; until then this one plan holds them all.
#[derive(Debug, Clone, Serialize)]
pub struct IndexPlan {
    pub multi_block: bool,
    pub sets: Vec<IndexSet>,
}

fn set(collection: &'static str, indexes: Vec<IndexModel>) -> IndexSet {
    IndexSet {
        collection,
        pre_ops: Vec::new(),
        indexes,
    }
}

/// Create every index the app relies on — [`index_plan`], applied in order.
///
/// `multi_block` (FR-47 P5c) selects between two mutually exclusive schemas
/// for `overlay_blocks`:
///
/// * `false` — the partial-unique index on `network_id` is created, enforcing
///   **one assigned block per network**. This is the shipped default and the
///   pre-P5c behaviour.
/// * `true` — that index is **dropped** if present and never created, because
///   it *is* the invariant multi-block removes.
///
/// ⚠️ Passing `true` is a one-way door in practice. Once a network holds two
/// assigned blocks, going back to `false` cannot recreate the index — Mongo
/// refuses it on the duplicate `network_id` — and the boot would fail loudly
/// rather than silently run without the guard. That is the intended failure:
/// a uniqueness guard that quietly gives up is worse than one that stops you.
pub async fn ensure_indexes(db: &Database, multi_block: bool) -> Result<(), mongodb::error::Error> {
    apply_index_sets(db, &index_plan(multi_block).sets).await?;
    info!("All indexes ensured");
    Ok(())
}

/// Apply index sets in order: each set's pre-ops, then its indexes.
///
/// FR-69 — the host calls this a second time with the module crates' sets
/// (`roomler_core::Module::indexes`), after the core plan above.
pub async fn apply_index_sets(
    db: &Database,
    sets: &[IndexSet],
) -> Result<(), mongodb::error::Error> {
    for entry in sets {
        for op in &entry.pre_ops {
            apply_op(db, entry.collection, op).await?;
        }
        create_indexes(db, entry.collection, entry.indexes.clone()).await?;
    }
    Ok(())
}

async fn apply_op(
    db: &Database,
    collection: &str,
    op: &IndexOp,
) -> Result<(), mongodb::error::Error> {
    match op {
        IndexOp::DropIndexIfPresent { index, why } => {
            // Two "nothing to drop" outcomes, and BOTH are normal — tolerating
            // only one of them is a bug this cost a test run to find:
            //
            //   * IndexNotFound (27)     — the collection exists without the
            //     index (a deployment that already ran with the op applied).
            //   * NamespaceNotFound (26) — the collection does not exist AT
            //     ALL, which is every fresh database, including every test
            //     database.
            //
            // 26 is the one that is easy to miss, because it never happens on
            // the deployment you are looking at while developing.
            if let Err(e) = db
                .collection::<bson::Document>(collection)
                .drop_index(*index)
                .await
                && !matches!(
                    &*e.kind,
                    mongodb::error::ErrorKind::Command(c) if c.code == 27 || c.code == 26
                )
            {
                return Err(e);
            }
            info!("{collection}: {why}");
            Ok(())
        }
    }
}

/// The plan: every collection's index set, in application order.
pub fn index_plan(multi_block: bool) -> IndexPlan {
    let mut sets: Vec<IndexSet> = Vec::new();

    // Tenants
    sets.push(set(
        "tenants",
        vec![
            index_unique(bson::doc! { "slug": 1 }),
            index(bson::doc! { "owner_id": 1 }),
        ],
    ));

    // Users
    sets.push(set(
        "users",
        vec![
            index_unique(bson::doc! { "email": 1 }),
            index_unique(bson::doc! { "username": 1 }),
            index_text(bson::doc! { "display_name": "text", "username": "text" }),
        ],
    ));

    // Tenant Members
    sets.push(set(
        "tenant_members",
        vec![
            index_unique(bson::doc! { "tenant_id": 1, "user_id": 1 }),
            index(bson::doc! { "user_id": 1 }),
        ],
    ));

    // Roles
    sets.push(set(
        "roles",
        vec![
            index_unique(bson::doc! { "tenant_id": 1, "name": 1 }),
            index(bson::doc! { "tenant_id": 1, "position": 1 }),
        ],
    ));

    // FR-69 P3 — `rooms`, `room_members`, `messages`, `reactions`, `files`
    // and `custom_emojis` are the `chat` module's: their sets live in
    // `roomler_ai_mod_chat::ChatState::indexes`.

    // Recordings
    sets.push(set(
        "recordings",
        vec![
            index(bson::doc! { "room_id": 1, "recording_type": 1 }),
            index(bson::doc! { "tenant_id": 1, "status": 1 }),
        ],
    ));

    // Invites
    sets.push(set(
        "invites",
        vec![
            index_unique(bson::doc! { "code": 1 }),
            index(bson::doc! { "tenant_id": 1, "status": 1 }),
        ],
    ));

    // Consent requests (Phase 4 — owner email/push consent). Unique capability
    // token; lookup by session; TTL-swept at `expires_at` (expireAfterSeconds=0
    // ⇒ the doc's own date is the expiry).
    sets.push(set(
        "consent_requests",
        vec![
            index_unique(bson::doc! { "token": 1 }),
            index(bson::doc! { "session_id": 1 }),
            index_ttl(bson::doc! { "expires_at": 1 }, 0),
        ],
    ));

    // Background Tasks
    sets.push(set(
        "background_tasks",
        vec![
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "status": 1 }),
            index_ttl(bson::doc! { "expires_at": 1 }, 0),
        ],
    ));

    // S5 — Stripe webhook idempotency ledger: `_id` = the Stripe event
    // id (natural unique key), rows expire after 30 days (Stripe stops
    // retrying long before that).
    sets.push(set(
        "stripe_events",
        vec![index_ttl(
            bson::doc! { "processed_at": 1 },
            30 * 24 * 60 * 60,
        )],
    ));

    // Notifications
    sets.push(set(
        "notifications",
        vec![
            index(bson::doc! { "user_id": 1, "is_read": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1 }),
        ],
    ));

    // FR-69 P2 — `subscribers`, `newsletter_issues` and `newsletter_sends`
    // are the `saas` module's: their sets live in
    // `roomler_ai_mod_saas::SaasState::indexes` and the host applies them
    // after this plan.

    // Activation Codes
    sets.push(set(
        "activation_codes",
        vec![
            index(bson::doc! { "user_id": 1 }),
            // TTL: auto-expire when valid_to passes
            index_ttl(bson::doc! { "valid_to": 1 }, 0),
        ],
    ));

    // Remote-control agents
    sets.push(set(
        "agents",
        vec![
            index_unique(bson::doc! { "tenant_id": 1, "machine_id": 1 }),
            index(bson::doc! { "tenant_id": 1, "status": 1 }),
            index(bson::doc! { "owner_user_id": 1 }),
            // FR-51 — the reaper's candidate scan (equality, equality, range:
            // ESR). Tiny today; what it buys is that a large ephemeral churn
            // (the CI-fleet case this feature exists for) never turns the
            // 60 s reap cycle into a collection scan.
            index(bson::doc! { "ephemeral": 1, "deleted_at": 1, "last_seen_at": 1 }),
        ],
    ));

    // Remote-control sessions
    sets.push(set(
        "remote_sessions",
        vec![
            index(bson::doc! { "agent_id": 1, "created_at": -1 }),
            index(bson::doc! { "controller_user_id": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "phase": 1 }),
        ],
    ));

    // Remote-control audit log — 90-day retention
    sets.push(set(
        "remote_audit",
        vec![
            index(bson::doc! { "session_id": 1, "at": 1 }),
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    ));

    // Remote-control agent crash reports — 90-day TTL on
    // `reported_at` (server clock). Compound index drives the admin
    // UI query: "last N crashes for this agent in this tenant",
    // sorted by client-supplied `crashed_at_unix` desc. See
    // `roomler_ai_remote_control::models::AgentCrashRecord` for the
    // shape (defined by the crash-report plan).
    sets.push(set(
        "agent_crashes",
        vec![
            index(bson::doc! { "tenant_id": 1, "agent_id": 1, "crashed_at_unix": -1 }),
            index_ttl(bson::doc! { "reported_at": 1 }, 90 * 24 * 60 * 60),
        ],
    ));

    // tunnel clients — same uniqueness contract as agents
    // (re-enroll-on-same-machine rehydrates the soft-deleted row in
    // place). `owner_user_id` index speeds the "my tunnel clients"
    // view on the user-facing dashboard.
    sets.push(set(
        "tunnel_clients",
        vec![
            index_unique(bson::doc! { "tenant_id": 1, "machine_id": 1 }),
            index(bson::doc! { "tenant_id": 1, "status": 1 }),
            index(bson::doc! { "owner_user_id": 1 }),
        ],
    ));

    // Overlay networks — one IPAM row per tenant. Unique on tenant_id
    // so `get_or_create` races collapse to one network.
    sets.push(set(
        "overlay_networks",
        vec![index_unique(bson::doc! { "tenant_id": 1 })],
    ));

    // Single-use token ledger (`_id` = the token's jti, so uniqueness is the
    // primary key and a claim is one insert). Rows expire an hour after use —
    // past every 10-minute enrollment token's lifetime, so a replay always
    // finds its record, while the ledger stays bounded.
    sets.push(set(
        "used_tokens",
        vec![index_ttl(bson::doc! { "used_at": 1 }, 60 * 60)],
    ));

    // FR-51 P2 — reusable ephemeral enrollment keys. `jti` is the value the
    // atomic use-claim is keyed by; unique GLOBALLY (jtis are uuid4, and a
    // cross-tenant collision would let one org's claim decrement another's
    // ceiling). No TTL: a dead key is a record until pruning is decided
    // explicitly (P4).
    sets.push(set(
        "enrollment_keys",
        vec![
            index_unique(bson::doc! { "jti": 1 }),
            index(bson::doc! { "tenant_id": 1, "created_at": -1 }),
        ],
    ));

    // FR-51 P2 — one row per successful key use: the trail that survives the
    // reap (ephemeral device rows hard-delete). 90-day TTL like the other
    // audit collections.
    sets.push(set(
        "enrollment_key_uses",
        vec![
            index(bson::doc! { "tenant_id": 1, "key_id": 1, "created_at": -1 }),
            index_ttl(bson::doc! { "created_at": 1 }, 90 * 24 * 60 * 60),
        ],
    ));

    // Multi-org P2b — the GLOBAL overlay block registry. Deliberately NOT
    // tenant-scoped: its entire job is guaranteeing that two tenants can
    // never hold overlapping slices of 100.64.0.0/10.
    //
    // `slot` unique is the structural half of that guarantee — the allocator
    // computes aligned, monotonic starts, so two racers either collide on the
    // same slot (this index arbitrates) or claim disjoint ranges. Without it
    // the allocator would need a lock.
    //
    // `network_id` unique is scoped to ASSIGNED rows: a renumbered tenant
    // keeps its quarantined predecessors forever (they hold their slots out
    // of circulation), and only one of its blocks may be live at a time.
    // FR-47 P5c — `slot` unique is what makes overlap unrepresentable and is
    // ALWAYS present. The `network_id` partial-unique is the separate
    // "one assigned block per network" rule, which multi-block removes.
    let mut overlay_block_indexes = vec![
        index_unique(bson::doc! { "slot": 1 }),
        // The allocator's "highest end" probe — one indexed sort+limit.
        index(bson::doc! { "end_slot": -1 }),
        index(bson::doc! { "tenant_id": 1 }),
        // Multi-block reads a network's blocks in allocation order.
        index(bson::doc! { "network_id": 1, "seq": 1 }),
    ];
    let mut overlay_block_ops = Vec::new();
    if multi_block {
        // Drop the guard rather than merely stop creating it: a deployment
        // that ran single-block already HAS the index, and an existing index
        // would refuse the second block with a duplicate key — which the
        // allocator's retry loop would then misreport as "lost too many
        // races" rather than as the schema problem it is. The drop runs in
        // `apply_op`, which tolerates both "nothing to drop" outcomes (27 and
        // 26) — see the comment there for why both are normal.
        overlay_block_ops.push(IndexOp::DropIndexIfPresent {
            index: "network_id_1",
            why: "multi-block schema — one-block-per-network guard removed",
        });
    } else {
        overlay_block_indexes.push(index_unique_partial(
            bson::doc! { "network_id": 1 },
            bson::doc! { "state": "assigned" },
        ));
    }
    sets.push(IndexSet {
        collection: "overlay_blocks",
        pre_ops: overlay_block_ops,
        indexes: overlay_block_indexes,
    });

    // Overlay nodes — virtual-LAN membership above agents/tunnel_clients.
    //
    // All three unique indexes are scoped to LIVE rows, because removing a
    // device from the fleet TOMBSTONES its node in place (keeping the address
    // and name as the forensic record of who held them) and returns the host
    // number to `overlay_networks.free_hosts` for reuse. A non-scoped unique
    // index would let a tombstone go on holding its IP and its name forever,
    // which is exactly the leak the release feature exists to close.
    //
    // The filter is `$type: "null"`, NOT `{deleted_at: null}`: equality-to-null
    // in Mongo also matches ABSENT, whereas `$type` matches only an explicit
    // BSON null. `OverlayNode.deleted_at` is declared without
    // `skip_serializing_if`/`serde(default)`, so it is written on every insert
    // and required on every read — "absent" is unreachable and `$type` is
    // exact. Tradeoff: a `$type`-filtered partial index is NOT usable by the
    // planner for a `{deleted_at: null}` query predicate. That is fine — these
    // three enforce uniqueness; the plain (tenant_id, network_id, deleted_at)
    // index below is what serves the netmap build query.
    sets.push(set(
        "overlay_nodes",
        vec![
            // Rehydrate key. Many tombstones per machine (a machine can be
            // removed and re-enrolled repeatedly, taking a fresh lease each
            // time) must coexist with AT MOST ONE live row.
            index_unique_partial(
                bson::doc! { "tenant_id": 1, "machine_id": 1 },
                bson::doc! { "deleted_at": { "$type": "null" } },
            ),
            // No two LIVE nodes share an overlay address.
            index_unique_partial(
                bson::doc! { "tenant_id": 1, "network_id": 1, "overlay_ip": 1 },
                bson::doc! { "deleted_at": { "$type": "null" } },
            ),
            index(bson::doc! { "tenant_id": 1, "network_id": 1, "deleted_at": 1 }),
            // Phase 0 — per-network-unique node name (MagicDNS). The `name > ""`
            // half keeps the empty names on pre-Phase-0 rows (backfilled on next
            // rejoin) from colliding; the `deleted_at` half releases the name on
            // removal so the next device can take it.
            index_unique_partial(
                bson::doc! { "tenant_id": 1, "network_id": 1, "name": 1 },
                bson::doc! { "$and": [
                    { "name": { "$gt": "" } },
                    { "deleted_at": { "$type": "null" } },
                ] },
            ),
            // Backs the by-node_ref lookups on the removal paths.
            index(bson::doc! { "tenant_id": 1, "node_ref.id": 1 }),
        ],
    ));

    // Tunnel policies — tenant-scoped allowlists. The server-side ACL
    // gate fetches `list_active_for_tenant(tenant_id)` on every
    // TcpForwardRequest; the (tenant_id, deleted_at) compound index
    // covers that query precisely.
    sets.push(set(
        "tunnel_policies",
        vec![
            index(bson::doc! { "tenant_id": 1, "deleted_at": 1 }),
            index(bson::doc! { "tenant_id": 1, "name": 1 }),
        ],
    ));

    // Tunnel audit log — 90-day retention mirroring remote_audit.
    // Compound index on (tenant_id, dst_host, at) backs the admin
    // "who connected to X in the last 7 days?" query in T4. The
    // standalone (session_id, at) entry mirrors the remote_audit
    // pattern for per-session reconstruction.
    sets.push(set(
        "tunnel_audit",
        vec![
            index(bson::doc! { "tunnel_session_id": 1, "at": 1 }),
            index(bson::doc! { "tenant_id": 1, "dst_host": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    ));

    // Fleet-RPC audit log — 90-day retention, same posture as remote_audit /
    // tunnel_audit. Every exec ATTEMPT lands here, allowed or denied, so the
    // (tenant_id, at) index backs the org-wide "what ran on my fleet?" view
    // and (agent_id, at) backs the per-device console history. The
    // (tenant_id, user_id, at) entry answers "what did this person run?" —
    // the question an incident review actually starts from.
    sets.push(set(
        "exec_audit",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "agent_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    ));

    // Remote-config decisions (`docs/remote-config.md`) — who asked for what
    // on which device, granted or refused. Same 90-day TTL as the other three
    // audit logs: a config change that opens exec is the same class of event
    // as using it, so it must not age out sooner.
    sets.push(set(
        "config_audit",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "agent_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    ));

    // FR-40 overlay-key rotation orders — who ordered which device to retire
    // its key, dispatched or refused. Same 90-day TTL as the other audit logs.
    sets.push(set(
        "key_rotation_audit",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "agent_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    ));

    // FR-19 peer-relay decisions — approvals (who made a device a relay) and
    // mints (what was routed through it), granted or refused. `agent_id`
    // answers both halves of the incident-review question in one query,
    // `requester_node_id` is what a rate-limit forensics pass walks, and the
    // TTL is the same 90 days as the other decision logs: making a device a
    // chokepoint for the tenant's traffic is the same class of event as
    // opening exec on it.
    sets.push(set(
        "peer_relay_audit",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "agent_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "requester_node_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    ));

    // Roomler-SSH grant decisions. Same three questions as `exec_audit`, same
    // 90-day TTL — an SSH session is the bigger power of the two, so its log
    // must not be the shorter-lived one.
    sets.push(set(
        "ssh_audit",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "agent_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    ));

    // Roomler-SSH session activity (P8) — what devices REPORT doing inside a
    // session, as opposed to `ssh_audit`'s record of what the server DECIDED.
    // Separate collection on purpose (see `SshActivityEvent`): one is
    // authoritative, the other is a claim by the host. Same 90-day TTL, and
    // `grant_id` is indexed because correlating a reported action back to the
    // authoritative decision row is the main thing a reader does here.
    sets.push(set(
        "ssh_activity",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "agent_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "grant_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    ));

    // Centralized log batches (rc.58). 7-day TTL on `created_at` so
    // operators have a one-week diagnostic window. The compound
    // tenant+agent+created_at index drives the admin UI query "last N
    // batches for this agent". The text index on `lines.msg` powers
    // full-text search in the admin UI; without it a tenant with 10k
    // batches/day would hit a collection scan on every search.
    sets.push(set(
        "agent_logs",
        vec![
            index(bson::doc! { "tenant_id": 1, "agent_id": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "source": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "session_id": 1 }),
            index_text(bson::doc! { "lines.msg": "text" }),
            index_ttl(bson::doc! { "created_at": 1 }, 7 * 24 * 60 * 60),
        ],
    ));

    // ── Observability / analytics (stats PR-1) ────────────────────────────
    // Sample collections use deterministic string `_id`s ("{key}:{bucket}")
    // so every writer is an idempotent upsert — that, not a lease, is what
    // makes the 2-pod deployment race-free. Raw samples are short-TTL; the
    // rollup task compacts them into _1h (90 d) and _1d (730 d).

    // Relay-PoP samples, one per region per 30 s poll tick (both pods write
    // the same bucket id). `{region, ts}` backs the history queries.
    sets.push(set(
        "stats_relay",
        vec![
            index(bson::doc! { "region": 1, "ts": 1 }),
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    ));

    // FR-20 — the cost ledger. One bucket per (tenant, meter, minute), with a
    // deterministic `_id` so both pods `$inc` the same document.
    //
    // ⚠ 7-day raw retention like its siblings: the durable record is the
    // rollup (`_1h` 90 d, `_1d` 730 d), and billing reads those. Raw minute
    // buckets exist to be compacted, not to be the ledger of record.
    sets.push(set(
        "stats_usage",
        vec![
            index(bson::doc! { "tenant_id": 1, "meter": 1, "ts": 1 }),
            // ⚠ No separate `{ts: 1}` index: the TTL index below already IS
            // one, and declaring both is an IndexOptionsConflict (same key
            // pattern, different options) as well as a wasted WiredTiger file
            // per test database. The siblings above deliberately don't either.
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    ));

    // Per-agent minute buckets from the heartbeat handler (the agent's
    // owning pod is the single writer).
    sets.push(set(
        "stats_machine",
        vec![
            index(bson::doc! { "tenant_id": 1, "ts": 1 }),
            index(bson::doc! { "tenant_id": 1, "agent_id": 1, "ts": 1 }),
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    ));

    // Wave 2 — platform user analytics. Neither collection stores an IP
    // or a raw User-Agent: the address is resolved to a country at
    // connect time and dropped, and page paths are normalised before
    // insert. 90-day retention, which is plenty for usage trends and
    // short enough that stale behavioural data doesn't accumulate.
    sets.push(set(
        "ws_sessions",
        vec![
            index(bson::doc! { "started_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "started_at": -1 }),
            index(bson::doc! { "user_id": 1, "started_at": -1 }),
            // The close path updates by _id + open-ness; this keeps the
            // "still open" scan (and the orphan sweep) cheap.
            index(bson::doc! { "ended_at": 1 }),
            index_ttl(bson::doc! { "started_at": 1 }, 90 * 24 * 60 * 60),
        ],
    ));
    sets.push(set(
        "page_views",
        vec![
            index(bson::doc! { "ts": -1 }),
            index(bson::doc! { "tenant_id": 1, "ts": -1 }),
            index(bson::doc! { "path": 1, "ts": -1 }),
            index_ttl(bson::doc! { "ts": 1 }, 90 * 24 * 60 * 60),
        ],
    ));

    // Wave 2 — per-agent overlay mesh snapshots (one row per agent,
    // replaced each heartbeat). TTL reaps the rows of agents that stop
    // reporting, so a decommissioned device leaves the graph on its own.
    sets.push(set(
        "stats_mesh",
        vec![
            index(bson::doc! { "tenant_id": 1, "ts": -1 }),
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    ));

    // Presence transition ledger (online|stale|offline), appended after the
    // `agents.last_presence` CAS — exactly-once across pods by construction.
    // Long TTL: transitions are rare and the 1-year uptime strips need them.
    sets.push(set(
        "stats_events",
        vec![
            index(bson::doc! { "tenant_id": 1, "agent_id": 1, "ts": 1 }),
            index_ttl(bson::doc! { "ts": 1 }, 730 * 24 * 60 * 60),
        ],
    ));

    // Per-conference-instance sample buckets from the mediasoup sampler
    // (owner pod of the room is the single writer).
    sets.push(set(
        "stats_call",
        vec![
            index(bson::doc! { "tenant_id": 1, "ts": 1 }),
            index(bson::doc! { "tenant_id": 1, "room_id": 1, "ts": 1 }),
            index(bson::doc! { "call_id": 1, "ts": 1 }),
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    ));

    // Wave 3 — the same sampler's PER-PARTICIPANT rows, backing per-user
    // usage accounting. `user_id` leads its own index because the platform
    // view queries one user ACROSS orgs, where a tenant-first index can't
    // help.
    sets.push(set(
        "stats_call_user",
        vec![
            index(bson::doc! { "tenant_id": 1, "ts": 1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "ts": 1 }),
            index(bson::doc! { "user_id": 1, "ts": 1 }),
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    ));

    // Wave 3 — per-user usage reads scan these two by (user, time); both
    // already have tenant-leading indexes for the org dashboards, neither
    // could serve a cross-org "what did this user do" query.
    sets.push(set(
        "remote_sessions",
        vec![index(bson::doc! { "tenant_id": 1, "created_at": -1 })],
    ));
    sets.push(set(
        "tunnel_audit",
        vec![index(bson::doc! { "user_id": 1, "at": -1 })],
    ));

    // One document per call instance (PR-2 lifecycle). `ended_at: null`
    // scan backs the orphan sweep; TTL on started_at bounds the ledger.
    sets.push(set(
        "call_sessions",
        vec![
            index(bson::doc! { "tenant_id": 1, "started_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "room_id": 1, "started_at": -1 }),
            index(bson::doc! { "ended_at": 1 }),
            index_ttl(bson::doc! { "started_at": 1 }, 730 * 24 * 60 * 60),
        ],
    ));

    // Hourly rollups (90 d) and daily rollups (730 d). The rollup task
    // whole-bucket-replaces via $merge on _id, so these are also upserts.
    for (coll, ttl_days) in [
        ("stats_usage_1h", 90u64),
        ("stats_relay_1h", 90),
        ("stats_machine_1h", 90),
        ("stats_call_1h", 90),
        ("stats_call_user_1h", 90),
        ("stats_usage_1d", 730),
        ("stats_relay_1d", 730),
        ("stats_machine_1d", 730),
        ("stats_call_1d", 730),
        ("stats_call_user_1d", 730),
    ] {
        let mut idx = vec![index_ttl(bson::doc! { "ts": 1 }, ttl_days * 24 * 60 * 60)];
        if coll.starts_with("stats_relay") {
            idx.push(index(bson::doc! { "region": 1, "ts": 1 }));
        } else {
            idx.push(index(bson::doc! { "tenant_id": 1, "ts": 1 }));
        }
        // The rolled-up usage tiers are read per USER as well as per org
        // (the platform view asks "this user, across every org").
        if coll.starts_with("stats_call_user") {
            idx.push(index(bson::doc! { "user_id": 1, "ts": 1 }));
        }
        sets.push(set(coll, idx));
    }

    IndexPlan { multi_block, sets }
}

pub fn index(keys: bson::Document) -> IndexModel {
    IndexModel::builder().keys(keys).build()
}

pub fn index_unique(keys: bson::Document) -> IndexModel {
    IndexModel::builder()
        .keys(keys)
        .options(IndexOptions::builder().unique(true).build())
        .build()
}

pub fn index_ttl(keys: bson::Document, expire_after_secs: u64) -> IndexModel {
    IndexModel::builder()
        .keys(keys)
        .options(
            IndexOptions::builder()
                .expire_after(std::time::Duration::from_secs(expire_after_secs))
                .build(),
        )
        .build()
}

pub fn index_text(keys: bson::Document) -> IndexModel {
    IndexModel::builder().keys(keys).build()
}

pub fn index_unique_sparse(keys: bson::Document) -> IndexModel {
    IndexModel::builder()
        .keys(keys)
        .options(IndexOptions::builder().unique(true).sparse(true).build())
        .build()
}

/// Unique index scoped by a partial filter — uniqueness is enforced only for
/// documents matching `filter` (e.g. non-empty `name`, so pre-Phase-0 rows with
/// an empty name don't collide).
pub fn index_unique_partial(keys: bson::Document, filter: bson::Document) -> IndexModel {
    IndexModel::builder()
        .keys(keys)
        .options(
            IndexOptions::builder()
                .unique(true)
                .partial_filter_expression(filter)
                .build(),
        )
        .build()
}

async fn create_indexes(
    db: &Database,
    collection: &str,
    indexes: Vec<IndexModel>,
) -> Result<(), mongodb::error::Error> {
    let coll = db.collection::<bson::Document>(collection);
    match coll.create_indexes(indexes.clone()).await {
        Ok(_) => {
            info!(collection, "Indexes created");
            Ok(())
        }
        Err(e) => {
            // IndexOptionsConflict (85) or IndexKeySpecsConflict (86): an existing
            // index has the same name but different options (e.g. adding TTL to an
            // existing index). Drop all indexes and recreate.
            if let mongodb::error::ErrorKind::Command(ref cmd_err) = *e.kind
                && (cmd_err.code == 85 || cmd_err.code == 86)
            {
                tracing::warn!(
                    collection,
                    "Index conflict detected, dropping conflicting indexes and retrying"
                );
                // Drop all non-_id indexes and recreate
                coll.drop_indexes().await?;
                coll.create_indexes(indexes).await?;
                info!(collection, "Indexes recreated after conflict resolution");
                return Ok(());
            }
            Err(e)
        }
    }
}
