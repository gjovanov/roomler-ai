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

    // Transcriptions
    create_indexes(
        db,
        "transcriptions",
        vec![
            index(bson::doc! { "room_id": 1 }),
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

    // Background Tasks
    create_indexes(
        db,
        "background_tasks",
        vec![index(
            bson::doc! { "tenant_id": 1, "user_id": 1, "status": 1 },
        )],
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

fn index_unique_sparse(keys: bson::Document) -> IndexModel {
    IndexModel::builder()
        .keys(keys)
        .options(IndexOptions::builder().unique(true).sparse(true).build())
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
            // IndexKeySpecsConflict (code 86): an existing index has the same name
            // but different options (e.g. non-sparse vs sparse). Drop the conflicting
            // index and retry.
            if let mongodb::error::ErrorKind::Command(ref cmd_err) = *e.kind {
                if cmd_err.code == 86 {
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
            }
            Err(e)
        }
    }
}
