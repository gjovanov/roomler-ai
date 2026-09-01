// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
};
use nanoid::nanoid;
use roomler_ai_db::models::TutorialState;
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
    /// Stats PR-3 — member of the platform-operator allowlist
    /// (`ROOMLER__STATS__PLATFORM_ADMINS`, user OBJECTIDS). Purely
    /// informational for the client (nav gating); every /api/admin/stats
    /// route re-checks server-side.
    #[serde(default)]
    pub is_platform_admin: bool,
    /// FR-12 P3 — the caller's tutorial state, carried on the response the
    /// client already fetches at boot rather than costing a second round
    /// trip. Empty for a brand-new account, by construction.
    #[serde(default)]
    pub tutorial: TutorialResponse,
}

/// The wire shape of `TutorialState`.
///
/// It exists for one reason: `bson::DateTime` serialises as
/// `{"$date":{"$numberLong":"…"}}`, which a browser has no business parsing
/// and which happens to be TRUTHY, so a client testing presence would work by
/// accident and a client formatting it would print `[object Object]`. Every
/// other timestamp this API returns is RFC 3339; this one is too.
#[derive(Debug, Serialize, Default)]
pub struct TutorialResponse {
    pub done: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen_at: Option<String>,
}

impl From<TutorialState> for TutorialResponse {
    fn from(t: TutorialState) -> Self {
        Self {
            done: t.done,
            seen_at: t.seen_at.and_then(|d| d.try_to_rfc3339_string().ok()),
        }
    }
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

#[derive(Debug, Deserialize, Default)]
pub struct RefreshRequest {
    /// Optional since the refresh cookie landed. A browser sends `{}` and lets
    /// the `refresh_token` cookie carry the credential; older cached bundles
    /// still put it here and keep working. Native/scripted callers may use
    /// either.
    #[serde(default)]
    pub refresh_token: Option<String>,
}

pub async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> Result<(StatusCode, HeaderMap, Json<RegisterResponse>), ApiError> {
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
        // Auto-verified registration IS a login, so it must leave the same
        // cookies behind as one. Without this the account is "signed in"
        // according to the response body but carries no session cookie, which
        // only worked because the SPA kept the token in localStorage.
        let mut headers = HeaderMap::new();
        headers.append(
            header::SET_COOKIE,
            format!(
                "access_token={}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}{}",
                tokens.access_token,
                tokens.expires_in,
                secure_attr(&state)
            )
            .parse()
            .unwrap(),
        );
        headers.append(
            header::SET_COOKIE,
            refresh_cookie(&state, &tokens.refresh_token)
                .parse()
                .unwrap(),
        );
        return Ok((
            StatusCode::CREATED,
            headers,
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
                    is_platform_admin: state.platform_admins.contains(&user_id),
                    tutorial: user.tutorial.into(),
                }),
            }),
        ));
    }

    // No tokens on this path (the account is not activated yet), so no
    // cookies either — an empty header map, not an absent one.
    Ok((
        StatusCode::CREATED,
        HeaderMap::new(),
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
    let lookup = if let Some(ref username) = body.username {
        state.users.find_by_username(username).await
    } else if let Some(ref email) = body.email {
        state.users.find_by_email(email).await
    } else {
        return Err(ApiError::BadRequest(
            "Either username or email is required".to_string(),
        ));
    };

    // Every failure below answers with the SAME error after doing the SAME
    // work. Previously an unknown account returned before any Argon2 verify
    // (~tens of ms cheaper — a timing oracle for "is this address
    // registered"), and an OAuth-only account answered "No password set",
    // which states outright that the address exists. Both are account
    // enumeration; the rate limiter throttles it but does not remove it.
    let invalid = || ApiError::Unauthorized("Invalid credentials".to_string());
    let user = lookup.ok().filter(|u| u.password_hash.is_some());
    let hash = user
        .as_ref()
        .and_then(|u| u.password_hash.as_deref())
        .unwrap_or_else(|| dummy_password_hash(&state));

    // Verified even when the account is absent, so the Argon2 cost is paid on
    // every path. `unwrap_or(false)`, not `?`: a stored hash that fails to
    // parse must read as "wrong password", not as a 500 that distinguishes
    // this account from a nonexistent one.
    let valid = state
        .auth
        .verify_password(&body.password, hash)
        .unwrap_or(false);
    let Some(user) = user.filter(|_| valid) else {
        return Err(invalid());
    };

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
        "access_token={}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}{}",
        tokens.access_token,
        tokens.expires_in,
        secure_attr(&state)
    );
    // ⚠️ APPEND, not insert. `HeaderMap::insert` REPLACES every existing value
    // for the name, so a second `insert` of Set-Cookie would silently drop the
    // access cookie and log the user straight back out.
    headers.append(header::SET_COOKIE, cookie.parse().unwrap());
    headers.append(
        header::SET_COOKIE,
        refresh_cookie(&state, &tokens.refresh_token)
            .parse()
            .unwrap(),
    );

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
            is_platform_admin: state.platform_admins.contains(&user_id),
            tutorial: user.tutorial.into(),
        },
        invite_tenant: None,
    };

    Ok((headers, Json(response)))
}

/// A real Argon2 PHC string that no supplied password can match, computed
/// once per process from a random secret.
///
/// Exists so the "no such account" branch of [`login`] performs the same
/// Argon2 verification as the real one — without it, an unknown address
/// answered measurably faster than a known one, which is an account-existence
/// oracle. It is never compared against anything a caller controls, so the
/// random input is only there to guarantee no one can pre-image it.
fn dummy_password_hash(state: &AppState) -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| {
        state
            .auth
            .hash_password(&uuid::Uuid::new_v4().to_string())
            .unwrap_or_default()
    })
}

/// `; Secure` in production, empty in dev — the session cookie is a full API
/// credential (the auth extractor accepts it), so it must never travel over
/// cleartext http; the http://localhost dev/test flow still needs it set.
fn secure_attr(state: &AppState) -> &'static str {
    if state.settings.app.environment == "production" {
        "; Secure"
    } else {
        ""
    }
}

/// The refresh cookie's name and the ONE path it is sent to.
///
/// A refresh token is the longest-lived browser credential there is — 30 days
/// by default, and it re-mints access tokens, so stealing it is worth far more
/// than stealing an access token. Scoping it to the exact endpoint that spends
/// it keeps it off every other request.
///
/// ⚠️ `Path` is NOT a security boundary against script on the same origin —
/// it is not a defence against XSS, and `HttpOnly` is what does that work
/// here. What the narrow path buys is that the credential stops riding along
/// on requests that have no use for it, which is where accidental logging,
/// header echo and proxy mishandling live.
const REFRESH_COOKIE: &str = "refresh_token";
const REFRESH_COOKIE_PATH: &str = "/api/auth/refresh";

/// `Set-Cookie` for the refresh token, scoped to the refresh endpoint.
pub(crate) fn refresh_cookie(state: &AppState, token: &str) -> String {
    format!(
        "{}={}; HttpOnly; Path={}; SameSite=Lax; Max-Age={}{}",
        REFRESH_COOKIE,
        token,
        REFRESH_COOKIE_PATH,
        state.settings.jwt.refresh_token_ttl_secs,
        secure_attr(state)
    )
}

/// `Set-Cookie` that expires the refresh cookie. The attributes other than
/// `Max-Age` must match the ones it was set with, or the browser keeps the
/// original — a "logout" that leaves a 30-day credential in place.
fn clear_refresh_cookie(state: &AppState) -> String {
    format!(
        "{}=; HttpOnly; Path={}; SameSite=Lax; Max-Age=0{}",
        REFRESH_COOKIE,
        REFRESH_COOKIE_PATH,
        secure_attr(state)
    )
}

pub async fn logout(State(state): State<AppState>) -> Result<HeaderMap, ApiError> {
    let mut headers = HeaderMap::new();
    let cookie = format!(
        "access_token=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0{}",
        secure_attr(&state)
    );
    headers.append(header::SET_COOKIE, cookie.parse().unwrap());
    // Clearing the refresh cookie is the load-bearing half: an access token
    // expires on its own in days, a refresh token would keep re-minting them
    // for a month. (Neither is REVOKED — the tokens stay valid if they were
    // captured; see the stateless-session item. This ends the browser's copy.)
    headers.append(
        header::SET_COOKIE,
        clear_refresh_cookie(&state).parse().unwrap(),
    );
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
        is_platform_admin: state.platform_admins.contains(&auth.user_id),
        tutorial: user.tutorial.into(),
    }))
}

pub async fn refresh(
    State(state): State<AppState>,
    req_headers: HeaderMap,
    Json(body): Json<RefreshRequest>,
) -> Result<(HeaderMap, Json<AuthResponse>), ApiError> {
    // Cookie first, body as the compatibility path. Same ordering rule as the
    // `/ws` work: the server has to accept the cookie BEFORE the UI stops
    // putting the token in the body, because during a rolling deploy a browser
    // can hit either pod.
    let presented = crate::cookies::get(&req_headers, REFRESH_COOKIE)
        .or_else(|| body.refresh_token.clone())
        .ok_or_else(|| ApiError::Unauthorized("No refresh token in cookie or body".to_string()))?;
    let claims = state.auth.verify_refresh_token(&presented)?;

    let user_id = bson::oid::ObjectId::parse_str(&claims.sub)
        .map_err(|_| ApiError::Unauthorized("Invalid user ID".to_string()))?;

    let user = state.users.base.find_by_id(user_id).await?;

    let tokens = state
        .auth
        .generate_tokens(user_id, &user.email, &user.username)?;

    let mut headers = HeaderMap::new();
    let cookie = format!(
        "access_token={}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}{}",
        tokens.access_token,
        tokens.expires_in,
        secure_attr(&state)
    );
    // ⚠️ APPEND, not insert. `HeaderMap::insert` REPLACES every existing value
    // for the name, so a second `insert` of Set-Cookie would silently drop the
    // access cookie and log the user straight back out.
    headers.append(header::SET_COOKIE, cookie.parse().unwrap());
    headers.append(
        header::SET_COOKIE,
        refresh_cookie(&state, &tokens.refresh_token)
            .parse()
            .unwrap(),
    );

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
            is_platform_admin: state.platform_admins.contains(&user_id),
            tutorial: user.tutorial.into(),
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
