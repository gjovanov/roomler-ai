use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
};
use nanoid::nanoid;
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
pub struct ActivateRequest {
    pub user_id: String,
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub message: String,
}

/// Response shape for `POST /auth/register`. Always carries a
/// `message`; when `ROOMLER__AUTH__AUTO_VERIFY=true` (e2e overlay)
/// also returns access/refresh tokens + the user record so test
/// helpers can chain register → authenticated API calls without
/// an explicit login step. Production (auto_verify=false) returns
/// only `message` — clients still call `/auth/login` after the
/// email-link activation. Token fields skip-serialize when None
/// so the prod payload stays a single `{ "message": "..." }`.
#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<UserResponse>,
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
) -> Result<(StatusCode, Json<RegisterResponse>), ApiError> {
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

    // E2E auto-verify shortcut: when `ROOMLER__AUTH__AUTO_VERIFY=true`
    // (only set in the roomler-ai-e2e overlay), flip is_verified
    // immediately so Playwright specs can `register → login` without
    // an SMTP capture in cluster. Default false — production still
    // requires email-link activation. Mirrors the same `$set` the
    // `activate` handler does later in the email-driven flow.
    if state.settings.auth.auto_verify
        && let Err(e) = state
            .users
            .base
            .update_by_id(user_id, bson::doc! { "$set": { "is_verified": true } })
            .await
    {
        warn!(
            "auto_verify is set but failed to mark user verified: {:?}",
            e
        );
    }

    // Generate activation code and send email.
    //
    // Email send is fire-and-forget: a fresh SMTP connection (e2e overlay
    // hits Mailpit on first register) can take 5-6s on the initial
    // handshake, which would block the register HTTP response past the
    // frontend's redirect timeout. The activation code is persisted
    // synchronously above, so the user can still click the email link
    // when it arrives; the response can return tokens immediately.
    let token = nanoid!(7);
    if let Err(e) = state
        .activation_codes
        .create(
            user_id,
            token.clone(),
            state.settings.email.activation_token_ttl_minutes,
        )
        .await
    {
        warn!("Failed to create activation code: {:?}", e);
    } else if let Some(email_svc) = state.email.clone() {
        let activation_url = format!(
            "{}/auth/activate?userId={}&token={}",
            state.settings.app.frontend_url,
            user_id.to_hex(),
            token
        );
        let to_email = body.email.clone();
        let display_name = body.display_name.clone();
        let ttl = state.settings.email.activation_token_ttl_minutes;
        tokio::spawn(async move {
            if let Err(e) = email_svc
                .send_activation(&to_email, &display_name, &activation_url, ttl)
                .await
            {
                warn!("Failed to send activation email: {:?}", e);
            }
        });
    }

    // Create a default tenant if requested
    if let (Some(tenant_name), Some(tenant_slug)) = (body.tenant_name, body.tenant_slug) {
        state
            .tenants
            .create(tenant_name, tenant_slug, user_id)
            .await?;
    }

    // Auto-accept invite if invite_code provided
    if let Some(ref invite_code) = body.invite_code {
        match auto_accept_invite(&state, user_id, &user.email, invite_code).await {
            Ok(_) => {}
            Err(e) => {
                warn!("Failed to auto-accept invite during registration: {:?}", e);
            }
        }
    }

    // E2E auto-verify path: skip the email-link round-trip by
    // returning tokens directly. Test helpers (`registerUserViaApi`)
    // expect `{ access_token, user }` in the body; without this they
    // call subsequent endpoints with `Bearer undefined` → 401.
    if state.settings.auth.auto_verify {
        let tokens = state
            .auth
            .generate_tokens(user_id, &user.email, &user.username)?;
        return Ok((
            StatusCode::CREATED,
            Json(RegisterResponse {
                message: "Registration successful (auto-verified).".to_string(),
                access_token: Some(tokens.access_token),
                refresh_token: Some(tokens.refresh_token),
                expires_in: Some(tokens.expires_in),
                user: Some(UserResponse {
                    id: user_id.to_hex(),
                    email: user.email,
                    username: user.username,
                    display_name: user.display_name,
                    avatar: user.avatar,
                }),
            }),
        ));
    }

    Ok((
        StatusCode::CREATED,
        Json(RegisterResponse {
            message: "Registration successful. Please check your email to activate your account."
                .to_string(),
            access_token: None,
            refresh_token: None,
            expires_in: None,
            user: None,
        }),
    ))
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
        return Err(ApiError::BadRequest(
            "Either username or email is required".to_string(),
        ));
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

    if !user.is_verified {
        return Err(ApiError::Unauthorized(
            "Account not activated. Please check your email for the activation link.".to_string(),
        ));
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

pub async fn activate(
    State(state): State<AppState>,
    Json(body): Json<ActivateRequest>,
) -> Result<Json<MessageResponse>, ApiError> {
    let user_id = bson::oid::ObjectId::parse_str(&body.user_id)
        .map_err(|_| ApiError::BadRequest("Invalid user ID".to_string()))?;

    let _code = state
        .activation_codes
        .find_valid(user_id, &body.token)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {}", e)))?
        .ok_or_else(|| ApiError::BadRequest("Invalid or expired activation token".to_string()))?;

    // Activate the user
    state
        .users
        .base
        .update_by_id(user_id, bson::doc! { "$set": { "is_verified": true } })
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to activate user: {}", e)))?;

    // Delete used activation code
    let _ = state.activation_codes.delete_for_user(user_id).await;

    // Send success email — fire-and-forget so SMTP latency doesn't
    // block the activate response. Same reasoning as the send_activation
    // call in register above.
    if let Some(email_svc) = state.email.clone() {
        let user = state
            .users
            .base
            .find_by_id(user_id)
            .await
            .map_err(|e| ApiError::Internal(format!("User not found: {}", e)))?;
        let login_url = format!("{}/auth/login", state.settings.app.frontend_url);
        let to_email = user.email.clone();
        let display_name = user.display_name.clone();
        tokio::spawn(async move {
            if let Err(e) = email_svc
                .send_activation_success(&to_email, &display_name, &login_url)
                .await
            {
                warn!("Failed to send activation success email: {:?}", e);
            }
        });
    }

    Ok(Json(MessageResponse {
        message: "Account activated successfully. You can now sign in.".to_string(),
    }))
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
