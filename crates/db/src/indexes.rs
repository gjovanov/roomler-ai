use mongodb::{Database, IndexModel, options::IndexOptions};
use tracing::info;

pub async fn ensure_indexes(db: &Database) -> Result<(), mongodb::error::Error> {
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

    // Audit Logs
    create_indexes(
        db,
        "audit_logs",
        vec![
            index(bson::doc! { "tenant_id": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "action": 1, "created_at": -1 }),
            index(bson::doc! { "tenant_id": 1, "actor_id": 1, "created_at": -1 }),
            // Auto-expire audit logs after 90 days
            index_ttl(bson::doc! { "created_at": 1 }, 90 * 24 * 60 * 60),
        ],
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

    // roomler-tunnel clients — same uniqueness contract as agents
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

    // Overlay nodes — virtual-LAN membership above agents/tunnel_clients.
    // Same (tenant_id, machine_id) rehydrate contract as agents; the
    // (tenant_id, network_id, overlay_ip) unique index guarantees no two
    // live nodes share an overlay address. The (tenant_id, network_id,
    // deleted_at) index backs the netmap build query.
    create_indexes(
        db,
        "overlay_nodes",
        vec![
            index_unique(bson::doc! { "tenant_id": 1, "machine_id": 1 }),
            index_unique(bson::doc! { "tenant_id": 1, "network_id": 1, "overlay_ip": 1 }),
            index(bson::doc! { "tenant_id": 1, "network_id": 1, "deleted_at": 1 }),
            // Phase 0 — per-network-unique node name (MagicDNS). Partial so the
            // empty names on pre-Phase-0 rows (backfilled on next rejoin) don't
            // collide.
            index_unique_partial(
                bson::doc! { "tenant_id": 1, "network_id": 1, "name": 1 },
                bson::doc! { "name": { "$gt": "" } },
            ),
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
