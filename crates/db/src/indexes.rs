// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use mongodb::{Database, IndexModel, options::IndexOptions};
use tracing::info;

/// Create every index the app relies on.
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
    // Tenants
    create_indexes(
        db,
        "tenants",
        vec![
            index_unique(bson::doc! { "slug": 1 }),
            index(bson::doc! { "owner_id": 1 }),
        ],
    )
    .await?;

    // Users
    create_indexes(
        db,
        "users",
        vec![
            index_unique(bson::doc! { "email": 1 }),
            index_unique(bson::doc! { "username": 1 }),
            index_text(bson::doc! { "display_name": "text", "username": "text" }),
        ],
    )
    .await?;

    // Tenant Members
    create_indexes(
        db,
        "tenant_members",
        vec![
            index_unique(bson::doc! { "tenant_id": 1, "user_id": 1 }),
            index(bson::doc! { "user_id": 1 }),
        ],
    )
    .await?;

    // Roles
    create_indexes(
        db,
        "roles",
        vec![
            index_unique(bson::doc! { "tenant_id": 1, "name": 1 }),
            index(bson::doc! { "tenant_id": 1, "position": 1 }),
        ],
    )
    .await?;

    // Rooms
    create_indexes(
        db,
        "rooms",
        vec![
            index(bson::doc! { "tenant_id": 1, "parent_id": 1, "position": 1 }),
            index_unique(bson::doc! { "tenant_id": 1, "path": 1 }),
            index(bson::doc! { "tenant_id": 1, "name": 1 }),
            index(bson::doc! { "tenant_id": 1, "is_default": 1 }),
            index_unique_sparse(bson::doc! { "meeting_code": 1 }),
            index_text(bson::doc! { "name": "text", "purpose": "text", "tags": "text" }),
        ],
    )
    .await?;

    // Room Members
    create_indexes(
        db,
        "room_members",
        vec![
            index_unique(bson::doc! { "room_id": 1, "user_id": 1 }),
            index(bson::doc! { "user_id": 1, "tenant_id": 1 }),
        ],
    )
    .await?;

    // Messages
    create_indexes(
        db,
        "messages",
        vec![
            index(bson::doc! { "room_id": 1, "created_at": -1 }),
            index(bson::doc! { "thread_id": 1, "created_at": 1 }),
            index(bson::doc! { "tenant_id": 1, "author_id": 1, "created_at": -1 }),
            index(bson::doc! { "room_id": 1, "is_pinned": 1 }),
            index(bson::doc! { "mentions.users": 1 }),
            index_text(bson::doc! { "content": "text" }),
        ],
    )
    .await?;

    // Reactions
    create_indexes(
        db,
        "reactions",
        vec![index_unique(
            bson::doc! { "message_id": 1, "emoji.value": 1, "user_id": 1 },
        )],
    )
    .await?;

    // Recordings
    create_indexes(
        db,
        "recordings",
        vec![
            index(bson::doc! { "room_id": 1, "recording_type": 1 }),
            index(bson::doc! { "tenant_id": 1, "status": 1 }),
        ],
    )
    .await?;

    // Files
    create_indexes(
        db,
        "files",
        vec![
            index(bson::doc! { "tenant_id": 1, "context.context_type": 1, "context.entity_id": 1 }),
            index(bson::doc! { "tenant_id": 1, "uploaded_by": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "context.room_id": 1, "created_at": -1 }),
            index(bson::doc! { "external_source.provider": 1, "external_source.external_id": 1 }),
        ],
    )
    .await?;

    // Invites
    create_indexes(
        db,
        "invites",
        vec![
            index_unique(bson::doc! { "code": 1 }),
            index(bson::doc! { "tenant_id": 1, "status": 1 }),
        ],
    )
    .await?;

    // Consent requests (Phase 4 — owner email/push consent). Unique capability
    // token; lookup by session; TTL-swept at `expires_at` (expireAfterSeconds=0
    // ⇒ the doc's own date is the expiry).
    create_indexes(
        db,
        "consent_requests",
        vec![
            index_unique(bson::doc! { "token": 1 }),
            index(bson::doc! { "session_id": 1 }),
            index_ttl(bson::doc! { "expires_at": 1 }, 0),
        ],
    )
    .await?;

    // Background Tasks
    create_indexes(
        db,
        "background_tasks",
        vec![
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "status": 1 }),
            index_ttl(bson::doc! { "expires_at": 1 }, 0),
        ],
    )
    .await?;

    // S5 — Stripe webhook idempotency ledger: `_id` = the Stripe event
    // id (natural unique key), rows expire after 30 days (Stripe stops
    // retrying long before that).
    create_indexes(
        db,
        "stripe_events",
        vec![index_ttl(
            bson::doc! { "processed_at": 1 },
            30 * 24 * 60 * 60,
        )],
    )
    .await?;

    // Notifications
    create_indexes(
        db,
        "notifications",
        vec![
            index(bson::doc! { "user_id": 1, "is_read": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1 }),
        ],
    )
    .await?;

    // Subscribers (FR-39). `email` is unique so a re-submission updates the
    // existing row rather than creating a second one that the first row's
    // unsubscribe link could never reach. The two token indexes are unique
    // because each token is a capability resolved by lookup, and two rows
    // sharing one would make the resolution ambiguous.
    create_indexes(
        db,
        "subscribers",
        vec![
            index_unique(bson::doc! { "email": 1 }),
            index_unique(bson::doc! { "unsubscribe_token": 1 }),
            index(bson::doc! { "confirm_token": 1 }),
            index(bson::doc! { "created_at": -1 }),
        ],
    )
    .await?;

    // Newsletter issues (FR-58). `slug` is unique because create is explicit
    // (a typo'd slug on update must 404, never upsert a second issue), and the
    // unique index is what arbitrates two concurrent creates.
    create_indexes(
        db,
        "newsletter_issues",
        vec![
            index_unique(bson::doc! { "slug": 1 }),
            index(bson::doc! { "created_at": -1 }),
        ],
    )
    .await?;

    // Newsletter delivery ledger (FR-58). 🔑 The unique pair IS the send
    // program's at-most-once invariant: rows are claimed (inserted) before the
    // send attempt, so a resume — or even two pods fanning out concurrently —
    // resolves each recipient to exactly one winner.
    create_indexes(
        db,
        "newsletter_sends",
        vec![
            index_unique(bson::doc! { "issue_id": 1, "subscriber_id": 1 }),
            index(bson::doc! { "issue_id": 1, "status": 1 }),
        ],
    )
    .await?;

    // Custom Emojis
    create_indexes(
        db,
        "custom_emojis",
        vec![index_unique(bson::doc! { "tenant_id": 1, "name": 1 })],
    )
    .await?;

    // Activation Codes
    create_indexes(
        db,
        "activation_codes",
        vec![
            index(bson::doc! { "user_id": 1 }),
            // TTL: auto-expire when valid_to passes
            index_ttl(bson::doc! { "valid_to": 1 }, 0),
        ],
    )
    .await?;

    // Remote-control agents
    create_indexes(
        db,
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
    )
    .await?;

    // Remote-control sessions
    create_indexes(
        db,
        "remote_sessions",
        vec![
            index(bson::doc! { "agent_id": 1, "created_at": -1 }),
            index(bson::doc! { "controller_user_id": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "phase": 1 }),
        ],
    )
    .await?;

    // Remote-control audit log — 90-day retention
    create_indexes(
        db,
        "remote_audit",
        vec![
            index(bson::doc! { "session_id": 1, "at": 1 }),
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Remote-control agent crash reports — 90-day TTL on
    // `reported_at` (server clock). Compound index drives the admin
    // UI query: "last N crashes for this agent in this tenant",
    // sorted by client-supplied `crashed_at_unix` desc. See
    // `roomler_ai_remote_control::models::AgentCrashRecord` for the
    // shape (defined by the crash-report plan).
    create_indexes(
        db,
        "agent_crashes",
        vec![
            index(bson::doc! { "tenant_id": 1, "agent_id": 1, "crashed_at_unix": -1 }),
            index_ttl(bson::doc! { "reported_at": 1 }, 90 * 24 * 60 * 60),
        ],
    )
    .await?;

    // tunnel clients — same uniqueness contract as agents
    // (re-enroll-on-same-machine rehydrates the soft-deleted row in
    // place). `owner_user_id` index speeds the "my tunnel clients"
    // view on the user-facing dashboard.
    create_indexes(
        db,
        "tunnel_clients",
        vec![
            index_unique(bson::doc! { "tenant_id": 1, "machine_id": 1 }),
            index(bson::doc! { "tenant_id": 1, "status": 1 }),
            index(bson::doc! { "owner_user_id": 1 }),
        ],
    )
    .await?;

    // Overlay networks — one IPAM row per tenant. Unique on tenant_id
    // so `get_or_create` races collapse to one network.
    create_indexes(
        db,
        "overlay_networks",
        vec![index_unique(bson::doc! { "tenant_id": 1 })],
    )
    .await?;

    // Single-use token ledger (`_id` = the token's jti, so uniqueness is the
    // primary key and a claim is one insert). Rows expire an hour after use —
    // past every 10-minute enrollment token's lifetime, so a replay always
    // finds its record, while the ledger stays bounded.
    create_indexes(
        db,
        "used_tokens",
        vec![index_ttl(bson::doc! { "used_at": 1 }, 60 * 60)],
    )
    .await?;

    // FR-51 P2 — reusable ephemeral enrollment keys. `jti` is the value the
    // atomic use-claim is keyed by; unique GLOBALLY (jtis are uuid4, and a
    // cross-tenant collision would let one org's claim decrement another's
    // ceiling). No TTL: a dead key is a record until pruning is decided
    // explicitly (P4).
    create_indexes(
        db,
        "enrollment_keys",
        vec![
            index_unique(bson::doc! { "jti": 1 }),
            index(bson::doc! { "tenant_id": 1, "created_at": -1 }),
        ],
    )
    .await?;

    // FR-51 P2 — one row per successful key use: the trail that survives the
    // reap (ephemeral device rows hard-delete). 90-day TTL like the other
    // audit collections.
    create_indexes(
        db,
        "enrollment_key_uses",
        vec![
            index(bson::doc! { "tenant_id": 1, "key_id": 1, "created_at": -1 }),
            index_ttl(bson::doc! { "created_at": 1 }, 90 * 24 * 60 * 60),
        ],
    )
    .await?;

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
    if multi_block {
        // Drop the guard rather than merely stop creating it: a deployment
        // that ran single-block already HAS the index, and an existing index
        // would refuse the second block with a duplicate key — which the
        // allocator's retry loop would then misreport as "lost too many
        // races" rather than as the schema problem it is.
        //
        // Two "nothing to drop" outcomes, and BOTH are normal — tolerating
        // only one of them is a bug this cost a test run to find:
        //
        //   * IndexNotFound (27)     — the collection exists without the index
        //     (a deployment that already ran with multi-block on).
        //   * NamespaceNotFound (26) — the collection does not exist AT ALL,
        //     which is every fresh database, including every test database.
        //
        // 26 is the one that is easy to miss, because it never happens on the
        // deployment you are looking at while developing.
        if let Err(e) = db
            .collection::<bson::Document>("overlay_blocks")
            .drop_index("network_id_1")
            .await
            && !matches!(
                &*e.kind,
                mongodb::error::ErrorKind::Command(c) if c.code == 27 || c.code == 26
            )
        {
            return Err(e);
        }
        info!("overlay_blocks: multi-block schema — one-block-per-network guard removed");
    } else {
        overlay_block_indexes.push(index_unique_partial(
            bson::doc! { "network_id": 1 },
            bson::doc! { "state": "assigned" },
        ));
    }
    create_indexes(db, "overlay_blocks", overlay_block_indexes).await?;

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
    create_indexes(
        db,
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
    )
    .await?;

    // Tunnel policies — tenant-scoped allowlists. The server-side ACL
    // gate fetches `list_active_for_tenant(tenant_id)` on every
    // TcpForwardRequest; the (tenant_id, deleted_at) compound index
    // covers that query precisely.
    create_indexes(
        db,
        "tunnel_policies",
        vec![
            index(bson::doc! { "tenant_id": 1, "deleted_at": 1 }),
            index(bson::doc! { "tenant_id": 1, "name": 1 }),
        ],
    )
    .await?;

    // Tunnel audit log — 90-day retention mirroring remote_audit.
    // Compound index on (tenant_id, dst_host, at) backs the admin
    // "who connected to X in the last 7 days?" query in T4. The
    // standalone (session_id, at) entry mirrors the remote_audit
    // pattern for per-session reconstruction.
    create_indexes(
        db,
        "tunnel_audit",
        vec![
            index(bson::doc! { "tunnel_session_id": 1, "at": 1 }),
            index(bson::doc! { "tenant_id": 1, "dst_host": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Fleet-RPC audit log — 90-day retention, same posture as remote_audit /
    // tunnel_audit. Every exec ATTEMPT lands here, allowed or denied, so the
    // (tenant_id, at) index backs the org-wide "what ran on my fleet?" view
    // and (agent_id, at) backs the per-device console history. The
    // (tenant_id, user_id, at) entry answers "what did this person run?" —
    // the question an incident review actually starts from.
    create_indexes(
        db,
        "exec_audit",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "agent_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Remote-config decisions (`docs/remote-config.md`) — who asked for what
    // on which device, granted or refused. Same 90-day TTL as the other three
    // audit logs: a config change that opens exec is the same class of event
    // as using it, so it must not age out sooner.
    create_indexes(
        db,
        "config_audit",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "agent_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    )
    .await?;

    // FR-40 overlay-key rotation orders — who ordered which device to retire
    // its key, dispatched or refused. Same 90-day TTL as the other audit logs.
    create_indexes(
        db,
        "key_rotation_audit",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "agent_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    )
    .await?;

    // FR-19 peer-relay decisions — approvals (who made a device a relay) and
    // mints (what was routed through it), granted or refused. `agent_id`
    // answers both halves of the incident-review question in one query,
    // `requester_node_id` is what a rate-limit forensics pass walks, and the
    // TTL is the same 90 days as the other decision logs: making a device a
    // chokepoint for the tenant's traffic is the same class of event as
    // opening exec on it.
    create_indexes(
        db,
        "peer_relay_audit",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "agent_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "requester_node_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Roomler-SSH grant decisions. Same three questions as `exec_audit`, same
    // 90-day TTL — an SSH session is the bigger power of the two, so its log
    // must not be the shorter-lived one.
    create_indexes(
        db,
        "ssh_audit",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "agent_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Roomler-SSH session activity (P8) — what devices REPORT doing inside a
    // session, as opposed to `ssh_audit`'s record of what the server DECIDED.
    // Separate collection on purpose (see `SshActivityEvent`): one is
    // authoritative, the other is a claim by the host. Same 90-day TTL, and
    // `grant_id` is indexed because correlating a reported action back to the
    // authoritative decision row is the main thing a reader does here.
    create_indexes(
        db,
        "ssh_activity",
        vec![
            index(bson::doc! { "tenant_id": 1, "at": -1 }),
            index(bson::doc! { "agent_id": 1, "at": -1 }),
            index(bson::doc! { "tenant_id": 1, "grant_id": 1, "at": -1 }),
            index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Centralized log batches (rc.58). 7-day TTL on `created_at` so
    // operators have a one-week diagnostic window. The compound
    // tenant+agent+created_at index drives the admin UI query "last N
    // batches for this agent". The text index on `lines.msg` powers
    // full-text search in the admin UI; without it a tenant with 10k
    // batches/day would hit a collection scan on every search.
    create_indexes(
        db,
        "agent_logs",
        vec![
            index(bson::doc! { "tenant_id": 1, "agent_id": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "source": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "session_id": 1 }),
            index_text(bson::doc! { "lines.msg": "text" }),
            index_ttl(bson::doc! { "created_at": 1 }, 7 * 24 * 60 * 60),
        ],
    )
    .await?;

    // ── Observability / analytics (stats PR-1) ────────────────────────────
    // Sample collections use deterministic string `_id`s ("{key}:{bucket}")
    // so every writer is an idempotent upsert — that, not a lease, is what
    // makes the 2-pod deployment race-free. Raw samples are short-TTL; the
    // rollup task compacts them into _1h (90 d) and _1d (730 d).

    // Relay-PoP samples, one per region per 30 s poll tick (both pods write
    // the same bucket id). `{region, ts}` backs the history queries.
    create_indexes(
        db,
        "stats_relay",
        vec![
            index(bson::doc! { "region": 1, "ts": 1 }),
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    )
    .await?;

    // FR-20 — the cost ledger. One bucket per (tenant, meter, minute), with a
    // deterministic `_id` so both pods `$inc` the same document.
    //
    // ⚠ 7-day raw retention like its siblings: the durable record is the
    // rollup (`_1h` 90 d, `_1d` 730 d), and billing reads those. Raw minute
    // buckets exist to be compacted, not to be the ledger of record.
    create_indexes(
        db,
        "stats_usage",
        vec![
            index(bson::doc! { "tenant_id": 1, "meter": 1, "ts": 1 }),
            // ⚠ No separate `{ts: 1}` index: the TTL index below already IS
            // one, and declaring both is an IndexOptionsConflict (same key
            // pattern, different options) as well as a wasted WiredTiger file
            // per test database. The siblings above deliberately don't either.
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Per-agent minute buckets from the heartbeat handler (the agent's
    // owning pod is the single writer).
    create_indexes(
        db,
        "stats_machine",
        vec![
            index(bson::doc! { "tenant_id": 1, "ts": 1 }),
            index(bson::doc! { "tenant_id": 1, "agent_id": 1, "ts": 1 }),
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Wave 2 — platform user analytics. Neither collection stores an IP
    // or a raw User-Agent: the address is resolved to a country at
    // connect time and dropped, and page paths are normalised before
    // insert. 90-day retention, which is plenty for usage trends and
    // short enough that stale behavioural data doesn't accumulate.
    create_indexes(
        db,
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
    )
    .await?;
    create_indexes(
        db,
        "page_views",
        vec![
            index(bson::doc! { "ts": -1 }),
            index(bson::doc! { "tenant_id": 1, "ts": -1 }),
            index(bson::doc! { "path": 1, "ts": -1 }),
            index_ttl(bson::doc! { "ts": 1 }, 90 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Wave 2 — per-agent overlay mesh snapshots (one row per agent,
    // replaced each heartbeat). TTL reaps the rows of agents that stop
    // reporting, so a decommissioned device leaves the graph on its own.
    create_indexes(
        db,
        "stats_mesh",
        vec![
            index(bson::doc! { "tenant_id": 1, "ts": -1 }),
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Presence transition ledger (online|stale|offline), appended after the
    // `agents.last_presence` CAS — exactly-once across pods by construction.
    // Long TTL: transitions are rare and the 1-year uptime strips need them.
    create_indexes(
        db,
        "stats_events",
        vec![
            index(bson::doc! { "tenant_id": 1, "agent_id": 1, "ts": 1 }),
            index_ttl(bson::doc! { "ts": 1 }, 730 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Per-conference-instance sample buckets from the mediasoup sampler
    // (owner pod of the room is the single writer).
    create_indexes(
        db,
        "stats_call",
        vec![
            index(bson::doc! { "tenant_id": 1, "ts": 1 }),
            index(bson::doc! { "tenant_id": 1, "room_id": 1, "ts": 1 }),
            index(bson::doc! { "call_id": 1, "ts": 1 }),
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Wave 3 — the same sampler's PER-PARTICIPANT rows, backing per-user
    // usage accounting. `user_id` leads its own index because the platform
    // view queries one user ACROSS orgs, where a tenant-first index can't
    // help.
    create_indexes(
        db,
        "stats_call_user",
        vec![
            index(bson::doc! { "tenant_id": 1, "ts": 1 }),
            index(bson::doc! { "tenant_id": 1, "user_id": 1, "ts": 1 }),
            index(bson::doc! { "user_id": 1, "ts": 1 }),
            index_ttl(bson::doc! { "ts": 1 }, 7 * 24 * 60 * 60),
        ],
    )
    .await?;

    // Wave 3 — per-user usage reads scan these two by (user, time); both
    // already have tenant-leading indexes for the org dashboards, neither
    // could serve a cross-org "what did this user do" query.
    create_indexes(
        db,
        "remote_sessions",
        vec![index(bson::doc! { "tenant_id": 1, "created_at": -1 })],
    )
    .await?;
    create_indexes(
        db,
        "tunnel_audit",
        vec![index(bson::doc! { "user_id": 1, "at": -1 })],
    )
    .await?;

    // One document per call instance (PR-2 lifecycle). `ended_at: null`
    // scan backs the orphan sweep; TTL on started_at bounds the ledger.
    create_indexes(
        db,
        "call_sessions",
        vec![
            index(bson::doc! { "tenant_id": 1, "started_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "room_id": 1, "started_at": -1 }),
            index(bson::doc! { "ended_at": 1 }),
            index_ttl(bson::doc! { "started_at": 1 }, 730 * 24 * 60 * 60),
        ],
    )
    .await?;

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
        create_indexes(db, coll, idx).await?;
    }

    info!("All indexes ensured");
    Ok(())
}

fn index(keys: bson::Document) -> IndexModel {
    IndexModel::builder().keys(keys).build()
}

fn index_unique(keys: bson::Document) -> IndexModel {
    IndexModel::builder()
        .keys(keys)
        .options(IndexOptions::builder().unique(true).build())
        .build()
}

fn index_ttl(keys: bson::Document, expire_after_secs: u64) -> IndexModel {
    IndexModel::builder()
        .keys(keys)
        .options(
            IndexOptions::builder()
                .expire_after(std::time::Duration::from_secs(expire_after_secs))
                .build(),
        )
        .build()
}

fn index_text(keys: bson::Document) -> IndexModel {
    IndexModel::builder().keys(keys).build()
}

fn index_unique_sparse(keys: bson::Document) -> IndexModel {
    IndexModel::builder()
        .keys(keys)
        .options(IndexOptions::builder().unique(true).sparse(true).build())
        .build()
}

/// Unique index scoped by a partial filter — uniqueness is enforced only for
/// documents matching `filter` (e.g. non-empty `name`, so pre-Phase-0 rows with
/// an empty name don't collide).
fn index_unique_partial(keys: bson::Document, filter: bson::Document) -> IndexModel {
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
