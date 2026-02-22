use axum::{Json, extract::State, http::{HeaderMap, StatusCode, header}};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub username: String,
    pub display_name: String,
    pub password: String,
    pub tenant_name: Option<String>,
    pub tenant_slug: Option<String>,
    pub invite_code: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    pub user: UserResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invite_tenant: Option<InviteTenantResponse>,
}

#[derive(Debug, Serialize)]
pub struct InviteTenantResponse {
    pub tenant_id: String,
    pub tenant_name: String,
    pub tenant_slug: String,
}

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub id: String,
    pub email: String,
    pub username: String,
    pub display_name: String,
    pub avatar: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

pub async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> Result<(StatusCode, HeaderMap, Json<AuthResponse>), ApiError> {
    let password_hash = state.auth.hash_password(&body.password)?;

    let user = state
        .users
        .create(
            body.email.clone(),
            body.username.clone(),
            body.display_name.clone(),
            password_hash,
        )
        .await?;

    let user_id = user.id.unwrap();

    // Create a default tenant if requested
    if let (Some(tenant_name), Some(tenant_slug)) = (body.tenant_name, body.tenant_slug) {
        state
            .tenants
            .create(tenant_name, tenant_slug, user_id)
            .await?;
    }

    // Auto-accept invite if invite_code provided
    let invite_tenant = if let Some(ref invite_code) = body.invite_code {
        match auto_accept_invite(&state, user_id, &user.email, invite_code).await {
            Ok(tenant_info) => Some(tenant_info),
            Err(e) => {
                warn!("Failed to auto-accept invite during registration: {:?}", e);
                None
            }
        }
    } else {
        None
    };

    let tokens = state
        .auth
        .generate_tokens(user_id, &user.email, &user.username)?;

    let mut headers = HeaderMap::new();
    let cookie = format!(
        "access_token={}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}",
        tokens.access_token, tokens.expires_in
    );
    headers.insert(header::SET_COOKIE, cookie.parse().unwrap());

    let response = AuthResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
        user: UserResponse {
            id: user_id.to_hex(),
            email: user.email,
            username: user.username,
            display_name: user.display_name,
            avatar: user.avatar,
        },
        invite_tenant,
    };

    Ok((StatusCode::CREATED, headers, Json(response)))
}

pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<(HeaderMap, Json<AuthResponse>), ApiError> {
    let user = if let Some(ref username) = body.username {
        state.users.find_by_username(username).await
    } else if let Some(ref email) = body.email {
        state.users.find_by_email(email).await
    } else {
        return Err(ApiError::BadRequest("Either username or email is required".to_string()));
    }
    .map_err(|_| ApiError::Unauthorized("Invalid credentials".to_string()))?;

    let password_hash = user
        .password_hash
        .as_ref()
        .ok_or_else(|| ApiError::Unauthorized("No password set".to_string()))?;

    let valid = state.auth.verify_password(&body.password, password_hash)?;
    if !valid {
        return Err(ApiError::Unauthorized("Invalid credentials".to_string()));
    }

    let user_id = user.id.unwrap();
    let tokens = state
        .auth
        .generate_tokens(user_id, &user.email, &user.username)?;

    let mut headers = HeaderMap::new();
    let cookie = format!(
        "access_token={}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}",
        tokens.access_token, tokens.expires_in
    );
    headers.insert(header::SET_COOKIE, cookie.parse().unwrap());

    let response = AuthResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
        user: UserResponse {
            id: user_id.to_hex(),
            email: user.email,
            username: user.username,
            display_name: user.display_name,
            avatar: user.avatar,
        },
        invite_tenant: None,
    };

    Ok((headers, Json(response)))
}

pub async fn logout() -> Result<HeaderMap, ApiError> {
    let mut headers = HeaderMap::new();
    let cookie = "access_token=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0";
    headers.insert(header::SET_COOKIE, cookie.parse().unwrap());
    Ok(headers)
}

pub async fn me(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<UserResponse>, ApiError> {
    let user = state.users.base.find_by_id(auth.user_id).await?;

    Ok(Json(UserResponse {
        id: user.id.unwrap().to_hex(),
        email: user.email,
        username: user.username,
        display_name: user.display_name,
        avatar: user.avatar,
    }))
}

pub async fn refresh(
    State(state): State<AppState>,
    Json(body): Json<RefreshRequest>,
) -> Result<(HeaderMap, Json<AuthResponse>), ApiError> {
    let claims = state.auth.verify_refresh_token(&body.refresh_token)?;

    let user_id = bson::oid::ObjectId::parse_str(&claims.sub)
        .map_err(|_| ApiError::Unauthorized("Invalid user ID".to_string()))?;

    let user = state.users.base.find_by_id(user_id).await?;

    let tokens = state
        .auth
        .generate_tokens(user_id, &user.email, &user.username)?;

    let mut headers = HeaderMap::new();
    let cookie = format!(
        "access_token={}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}",
        tokens.access_token, tokens.expires_in
    );
    headers.insert(header::SET_COOKIE, cookie.parse().unwrap());

    let response = AuthResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
        user: UserResponse {
            id: user_id.to_hex(),
            email: user.email,
            username: user.username,
            display_name: user.display_name,
            avatar: user.avatar,
        },
        invite_tenant: None,
    };

    Ok((headers, Json(response)))
}

/// Auto-accept an invite for a newly registered user.
async fn auto_accept_invite(
    state: &AppState,
    user_id: bson::oid::ObjectId,
    email: &str,
    invite_code: &str,
) -> Result<InviteTenantResponse, ApiError> {
    let invite = state.invites.find_by_code(invite_code).await?;

    state
        .invites
        .validate(&invite)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // Check target_email constraint
    if let Some(ref target_email) = invite.target_email
        && target_email != email
    {
        return Err(ApiError::Forbidden(
            "This invite is for a different email address".to_string(),
        ));
    }

    // Determine roles
    let role_ids = if invite.assign_role_ids.is_empty() {
        let member_role = state
            .tenants
            .get_role_by_name(invite.tenant_id, "member")
            .await?;
        vec![member_role.id.unwrap()]
    } else {
        invite.assign_role_ids.clone()
    };

    // Add the user to the tenant
    state
        .tenants
        .add_member(invite.tenant_id, user_id, role_ids, Some(invite.inviter_id))
        .await?;

    // Increment use count
    state
        .invites
        .increment_use_count(invite.id.unwrap())
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let tenant = state.tenants.base.find_by_id(invite.tenant_id).await?;

    Ok(InviteTenantResponse {
        tenant_id: tenant.id.unwrap().to_hex(),
        tenant_name: tenant.name,
        tenant_slug: tenant.slug,
    })
}
