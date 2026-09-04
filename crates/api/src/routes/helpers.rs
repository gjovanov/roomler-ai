// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::oid::ObjectId;
use roomler_ai_db::models::{Message, NotificationSource, NotificationType, Room};

use crate::error::ApiError;
use crate::ws;
use crate::{core_state::Core, state::AppState};

/// Parameters for creating and dispatching notifications.
pub struct NotifyParams {
    pub tenant_id: ObjectId,
    pub notification_type: NotificationType,
    pub title: String,
    pub body: String,
    pub link: String,
    pub source: NotificationSource,
    pub ws_type_label: &'static str,
}

/// Create a notification for a single user and send it via WebSocket.
/// S6 — cross-pod connectivity check for the offline push/email dedupe.
/// Local sockets win (fast path); otherwise consult the Redis online
/// registry so a user whose WS lives on the OTHER pod isn't spammed
/// with push+email for every mention. Registry unavailable → treat as
/// offline (matches the pre-S6 single-pod behaviour of "not here = not
/// connected").
async fn user_online_anywhere(state: &Core, user_id: &ObjectId) -> bool {
    if state.ws_storage.is_connected(user_id) {
        return true;
    }
    if let Some(pubsub) = &state.redis_pubsub
        && let Ok(online) = pubsub.online_anywhere(&user_id.to_hex()).await
    {
        return online;
    }
    false
}

async fn create_and_send_notification(
    state: &Core,
    params: &NotifyParams,
    user_id: ObjectId,
) -> bool {
    match state
        .notifications
        .create(
            params.tenant_id,
            user_id,
            params.notification_type.clone(),
            params.title.clone(),
            params.body.clone(),
            Some(params.link.clone()),
            params.source.clone(),
        )
        .await
    {
        Ok(notification) => {
            let notif_event = serde_json::json!({
                "type": "notification:new",
                "data": {
                    "id": notification.id.unwrap().to_hex(),
                    // P4 — clients route non-active-org notifications into the
                    // per-org badge store; without the tenant they can't.
                    "tenant_id": params.tenant_id.to_hex(),
                    "title": notification.title,
                    "body": notification.body,
                    "link": notification.link,
                    "notification_type": params.ws_type_label,
                    "created_at": notification.created_at.try_to_rfc3339_string().unwrap_or_default(),
                }
            });
            ws::dispatcher::send_to_user_with_redis(
                &state.ws_storage,
                &state.redis_pubsub,
                &user_id,
                &notif_event,
            )
            .await;
            true
        }
        Err(e) => {
            tracing::error!(
                "Failed to create {} notification for {}: {}",
                params.ws_type_label,
                user_id,
                e
            );
            false
        }
    }
}

/// Send push notifications for a list of offline user IDs (spawns a background task).
fn spawn_push_for_offline(
    state: &Core,
    offline_user_ids: Vec<ObjectId>,
    title: String,
    body: String,
    link: String,
) {
    if offline_user_ids.is_empty() {
        return;
    }
    if let Some(ref push_svc) = state.push {
        let push = push_svc.clone();
        let subs_dao = state.push_subscriptions.clone();
        tokio::spawn(async move {
            if let Ok(subs) = subs_dao.find_by_users(&offline_user_ids).await {
                for sub in subs {
                    let _ = push
                        .send(
                            &sub.endpoint,
                            &sub.keys.auth,
                            &sub.keys.p256dh,
                            &title,
                            &body,
                            Some(&link),
                        )
                        .await;
                }
            }
        });
    }
}

/// Send email notification for a single offline user about a mention (spawns a background task).
fn spawn_mention_email(
    state: &Core,
    user_id: ObjectId,
    mentioner_name: String,
    room_name: String,
    preview: String,
    tenant_id_str: &str,
    room_id_str: &str,
) {
    if let Some(ref email_svc) = state.email {
        let email_svc = email_svc.clone();
        let users = state.users.clone();
        let link_url = format!(
            "{}/tenant/{}/room/{}",
            state.settings.oauth.base_url, tenant_id_str, room_id_str
        );
        tokio::spawn(async move {
            if let Ok(user) = users.base.find_by_id(user_id).await
                && let Err(e) = email_svc
                    .send_mention_notification(
                        &user.email,
                        &mentioner_name,
                        &room_name,
                        &preview,
                        &link_url,
                    )
                    .await
            {
                tracing::warn!(%e, "Failed to send mention email");
            }
        });
    }
}

/// Create notifications and send push/email for mentioned users in a message.
#[allow(clippy::too_many_arguments)]
pub async fn notify_mentions(
    state: &Core,
    tenant_id: ObjectId,
    _room_id: ObjectId,
    message_id: ObjectId,
    author_id: ObjectId,
    mentioned_user_ids: &[ObjectId],
    room_name: &str,
    content_preview: &str,
    mentioner_name: &str,
    tenant_id_str: &str,
    room_id_str: &str,
) {
    let params = NotifyParams {
        tenant_id,
        notification_type: NotificationType::Mention,
        title: format!("Mentioned in #{}", room_name),
        body: content_preview.chars().take(200).collect(),
        link: format!(
            "/tenant/{}/room/{}?msg={}",
            tenant_id_str,
            room_id_str,
            message_id.to_hex()
        ),
        source: NotificationSource {
            entity_type: "message".to_string(),
            entity_id: message_id,
            actor_id: Some(author_id),
        },
        ws_type_label: "mention",
    };

    let mut offline_ids = Vec::new();

    for user_id in mentioned_user_ids {
        if *user_id == author_id {
            continue;
        }

        create_and_send_notification(state, &params, *user_id).await;

        if !user_online_anywhere(state, user_id).await {
            spawn_mention_email(
                state,
                *user_id,
                mentioner_name.to_string(),
                room_name.to_string(),
                params.body.clone(),
                tenant_id_str,
                room_id_str,
            );
            offline_ids.push(*user_id);
        }
    }

    spawn_push_for_offline(
        state,
        offline_ids,
        params.title,
        params.body,
        format!("/tenant/{}/room/{}", tenant_id_str, room_id_str),
    );
}

/// Create call-started notifications for room members and send push to offline users.
#[allow(clippy::too_many_arguments)]
pub async fn notify_call_started(
    state: &Core,
    tenant_id: ObjectId,
    room_id: ObjectId,
    caller_id: ObjectId,
    member_ids: &[ObjectId],
    room_name: &str,
    caller_name: &str,
    tenant_id_str: &str,
    room_id_str: &str,
) {
    let params = NotifyParams {
        tenant_id,
        notification_type: NotificationType::Call,
        title: format!("Call started in #{}", room_name),
        body: format!("{} started a call", caller_name),
        link: format!("/tenant/{}/room/{}/call", tenant_id_str, room_id_str),
        source: NotificationSource {
            entity_type: "room".to_string(),
            entity_id: room_id,
            actor_id: Some(caller_id),
        },
        ws_type_label: "call",
    };

    let mut offline_ids = Vec::new();

    for uid in member_ids {
        if *uid == caller_id {
            continue;
        }

        create_and_send_notification(state, &params, *uid).await;

        if !user_online_anywhere(state, uid).await {
            offline_ids.push(*uid);
        }
    }

    spawn_push_for_offline(state, offline_ids, params.title, params.body, params.link);
}

/// Object-level authorization gate for room-scoped collaboration routes.
///
/// Returns the room ONLY if it belongs to `tenant_id` AND the caller is a
/// member of that tenant. This is the invariant the older collaboration
/// handlers were missing: `is_member(tid)` alone is satisfied by any tenant
/// the caller belongs to (a user can create their own tenant for free), so it
/// does NOT stop reading or mutating ANOTHER tenant's room by id — the
/// cross-tenant IDOR. Resolving the room *within* the tenant (`{_id,
/// tenant_id}`) closes it: a foreign room resolves to nothing → 404, leaking
/// neither its content nor its existence.
/// Tenant membership + the room resolved WITHIN that tenant, and nothing else.
///
/// This is [`require_room_in_tenant`] without the room-level visibility gate,
/// and it exists for exactly one caller: `join`. Joining is how someone
/// BECOMES a member, so routing it through a check that requires membership
/// would make Private rooms unjoinable — a room you can see, cannot read, and
/// cannot ask to enter.
///
/// ⚠️ Do not reach for this to "skip the check" anywhere else. Every other
/// room-scoped route wants [`require_room_in_tenant`]; a second caller here
/// would be a visibility bypass wearing a helper's name.
pub async fn resolve_room_in_tenant(
    state: &AppState,
    tenant_id: ObjectId,
    room_id: ObjectId,
    user_id: ObjectId,
) -> Result<Room, ApiError> {
    if !state.tenants.is_member(tenant_id, user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }
    Ok(state
        .rooms
        .base
        .find_by_id_in_tenant(tenant_id, room_id)
        .await?)
}

pub async fn require_room_in_tenant(
    state: &AppState,
    tenant_id: ObjectId,
    room_id: ObjectId,
    user_id: ObjectId,
) -> Result<Room, ApiError> {
    let room = resolve_room_in_tenant(state, tenant_id, room_id, user_id).await?;

    // Room-level read authorization. Tenant membership answers "may you be
    // here at all"; this answers "may you be in THIS room" — the question that
    // previously had no answer, so every member could read every room while
    // the sidebar drew a padlock on most of them.
    //
    // `Public` (the default, and what every pre-existing room reads back as)
    // short-circuits, so this costs no query for the overwhelming majority of
    // requests and changes no behaviour on the day it ships.
    if room.visibility.requires_membership()
        && !state.rooms.is_member(tenant_id, room_id, user_id).await?
    {
        // NOT FOUND, not FORBIDDEN, for a `Secret` room: 403 would confirm it
        // exists to someone who is not supposed to know that, which is the
        // whole point of Secret. `Private` is listed anyway, so its existence
        // is not a secret and a 403 is the more useful answer.
        return Err(if room.visibility.hidden_from_non_members() {
            ApiError::NotFound("Resource not found".to_string())
        } else {
            ApiError::Forbidden("Not a member of this room".to_string())
        });
    }

    Ok(room)
}

/// The message-keyed sibling of [`require_room_in_tenant`]. Handlers keyed by
/// `message_id` (reactions, thread replies, edits) cannot rely on the room
/// check because the id is decoupled from the path room: a caller can pass
/// their own tenant + room but another tenant's message id. Resolving the
/// message within the tenant (`{_id, tenant_id}`) is the binding check.
pub async fn require_message_in_tenant(
    state: &AppState,
    tenant_id: ObjectId,
    message_id: ObjectId,
    user_id: ObjectId,
) -> Result<Message, ApiError> {
    if !state.tenants.is_member(tenant_id, user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }
    Ok(state
        .messages
        .base
        .find_by_id_in_tenant(tenant_id, message_id)
        .await?)
}
