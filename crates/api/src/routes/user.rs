use axum::{
    Json,
    extract::{Path, Query, State},
};
use bson::{doc, oid::ObjectId};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler_ai_services::dao::base::PaginationParams;

#[derive(Debug, Serialize)]
pub struct MemberResponse {
    pub id: String,
    pub user_id: String,
    pub nickname: Option<String>,
    /// The user's display name (falls back to username), resolved from the users
    /// collection — so member pickers can show a name, not a raw id.
    pub display_name: String,
    pub role_ids: Vec<String>,
    pub joined_at: String,
}

#[derive(Debug, Serialize)]
pub struct ProfileResponse {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub avatar: Option<String>,
    pub bio: Option<String>,
    pub presence: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProfileRequest {
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub avatar: Option<String>,
    pub locale: Option<String>,
    pub timezone: Option<String>,
}

pub async fn list_members(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let result = state
        .tenants
        .members
        .find_paginated(
            doc! { "tenant_id": tid },
            Some(doc! { "joined_at": 1 }),
            &params,
        )
        .await?;

    // Resolve display names in one batch so the response carries names, not just
    // ids (used by member pickers, e.g. the agent owner-reassign dialog).
    let user_ids: Vec<ObjectId> = result.items.iter().map(|m| m.user_id).collect();
    let names = state
        .users
        .find_display_names(&user_ids)
        .await
        .unwrap_or_default();
    let items: Vec<MemberResponse> = result
        .items
        .into_iter()
        .map(|m| MemberResponse {
            id: m.id.unwrap().to_hex(),
            user_id: m.user_id.to_hex(),
            nickname: m.nickname,
            display_name: names.get(&m.user_id).cloned().unwrap_or_default(),
            role_ids: m.role_ids.iter().map(|r| r.to_hex()).collect(),
            joined_at: m.joined_at.try_to_rfc3339_string().unwrap_or_default(),
        })
        .collect();

    Ok(Json(serde_json::json!({
        "items": items,
        "total": result.total,
        "page": result.page,
        "per_page": result.per_page,
        "total_pages": result.total_pages,
    })))
}

pub async fn get_profile(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(user_id): Path<String>,
) -> Result<Json<ProfileResponse>, ApiError> {
    let uid = ObjectId::parse_str(&user_id)
        .map_err(|_| ApiError::BadRequest("Invalid user_id".to_string()))?;

    let user = state.users.base.find_by_id(uid).await?;

    Ok(Json(ProfileResponse {
        id: user.id.unwrap().to_hex(),
        username: user.username,
        display_name: user.display_name,
        avatar: user.avatar,
        bio: user.bio,
        presence: format!("{:?}", user.presence).to_lowercase(),
        created_at: user.created_at.try_to_rfc3339_string().unwrap_or_default(),
    }))
}

pub async fn update_profile(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<UpdateProfileRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .users
        .update_profile(
            auth.user_id,
            body.display_name,
            body.bio,
            body.avatar,
            body.locale,
            body.timezone,
        )
        .await?;

    Ok(Json(serde_json::json!({ "updated": true })))
}

#[derive(Debug, Serialize)]
pub struct MyMembershipResponse {
    /// Combined permission bitmask across all the caller's roles in this
    /// tenant (`role::permissions` flags OR-ed). Value stays < 2^27, so it
    /// is safe as a plain JSON number.
    pub permissions: u64,
    /// Whether the caller is the tenant's owner. Owners may hold no explicit
    /// role, so the UI treats this as an implicit all-permissions grant.
    pub is_owner: bool,
}

/// GET /api/tenant/{tenant_id}/member/me — the caller's own effective
/// permissions in this tenant. Purely informational for the client (nav
/// gating, disabled buttons); every mutating route still enforces its own
/// permission check server-side.
pub async fn my_membership(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<MyMembershipResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    // Non-members get the DAO's Forbidden straight through.
    let permissions = state
        .tenants
        .get_member_permissions(tid, auth.user_id)
        .await?;
    let is_owner = state
        .tenants
        .base
        .find_by_id(tid)
        .await
        .map(|t| t.owner_id == auth.user_id)
        .unwrap_or(false);

    Ok(Json(MyMembershipResponse {
        permissions,
        is_owner,
    }))
}

#[derive(Debug, Serialize)]
pub struct TenantUnreadSummary {
    pub tenant_id: String,
    pub name: String,
    /// Unread chat messages across the caller's rooms in this tenant.
    pub unread_messages: u64,
    /// How many of those rooms have ≥1 unread message.
    pub unread_rooms: u64,
    /// Unread bell notifications scoped to this tenant…
    pub notifications: u64,
    /// …of which mentions…
    pub mentions: u64,
    /// …and pending remote-control consent requests.
    pub consents: u64,
}

/// P4 — GET /api/user/unread-summary: the caller's unread state across
/// EVERY org they belong to, in one call. The org switcher's badge seed +
/// the `ws:reconnected` convergence fetch (there is no event replay — a
/// reconnecting client refetches instead). Rows come back for all
/// memberships, zeros included, so the client can render the full switcher
/// without merging.
pub async fn unread_summary(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenants = state.tenants.find_user_tenants(auth.user_id).await?;
    let notif_by_tenant = state.notifications.unread_by_tenant(auth.user_id).await?;

    let mut out: Vec<TenantUnreadSummary> = Vec::with_capacity(tenants.len());
    for t in tenants {
        let Some(tid) = t.id else { continue };
        let rooms = state.rooms.find_user_rooms(tid, auth.user_id).await?;
        let room_ids: Vec<ObjectId> = rooms.iter().filter_map(|r| r.id).collect();
        let (mut unread_messages, mut unread_rooms) = (0u64, 0u64);
        if !room_ids.is_empty() {
            for (_, count) in state
                .messages
                .unread_counts_by_room(&room_ids, auth.user_id)
                .await?
            {
                if count > 0 {
                    unread_rooms += 1;
                    unread_messages += count;
                }
            }
        }
        let notif = notif_by_tenant.iter().find(|n| n.tenant_id == tid);
        out.push(TenantUnreadSummary {
            tenant_id: tid.to_hex(),
            name: t.name,
            unread_messages,
            unread_rooms,
            notifications: notif.map(|n| n.total).unwrap_or(0),
            mentions: notif.map(|n| n.mentions).unwrap_or(0),
            consents: notif.map(|n| n.consents).unwrap_or(0),
        });
    }

    Ok(Json(serde_json::json!({ "tenants": out })))
}
