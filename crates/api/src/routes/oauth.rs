// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{core_state::Core, error::ApiError};

/// `; Secure` in production, empty in dev — the http://localhost dev/test flow
/// must still receive the cookie, and prod is https end-to-end.
fn secure_attr(state: &Core) -> &'static str {
    if state.settings.app.environment == "production" {
        "; Secure"
    } else {
        ""
    }
}

/// Read one cookie value out of the request `Cookie` header.
///
/// Thin alias over [`crate::cookies::get`] — the parser used to be spelled out
/// here, and in the auth extractor, and in the `/ws` upgrade, each slightly
/// differently. See that module for why one copy is the point.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    crate::cookies::get(headers, name)
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: String,
    pub state: String,
}

pub async fn oauth_redirect(
    State(state): State<Core>,
    Path(provider): Path<String>,
) -> Result<Response, ApiError> {
    let oauth = state
        .oauth
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("OAuth not configured".to_string()))?;

    // CSRF: mint a random state, bind it to THIS browser via a short-lived
    // HttpOnly cookie (double-submit), and carry the same value in the auth
    // URL. The callback requires the two to match — without it an attacker
    // could feed a victim a pre-obtained code+state and silently sign them
    // into the ATTACKER's account (login CSRF / forced account takeover).
    let csrf_state = Uuid::new_v4().to_string();

    let auth_url = oauth
        .build_auth_url(&provider, &csrf_state)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cookie = format!(
        "oauth_state={}; HttpOnly; Path=/; SameSite=Lax; Max-Age=600{}",
        csrf_state,
        secure_attr(&state)
    );

    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, cookie.parse().unwrap());
    headers.insert(header::LOCATION, auth_url.parse().unwrap());
    Ok((StatusCode::TEMPORARY_REDIRECT, headers).into_response())
}

pub async fn oauth_callback(
    State(state): State<Core>,
    Path(provider): Path<String>,
    req_headers: HeaderMap,
    Query(params): Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    let oauth = state
        .oauth
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("OAuth not configured".to_string()))?;

    // CSRF: the query `state` MUST equal the `oauth_state` cookie we bound to
    // this browser at redirect time. A missing/mismatched value means the flow
    // was not initiated by this browser (login CSRF) — reject before touching
    // the code.
    if params.state.is_empty()
        || cookie_value(&req_headers, "oauth_state").as_deref() != Some(params.state.as_str())
    {
        return Err(ApiError::Forbidden("Invalid OAuth state".to_string()));
    }

    // Exchange code and fetch user info
    let user_info = oauth
        .authenticate(&provider, &params.code)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    if user_info.email.is_empty() {
        return Err(ApiError::BadRequest(
            "Could not retrieve email from OAuth provider".to_string(),
        ));
    }

    // Find or create user
    let user = state
        .users
        .find_or_create_by_oauth(
            &user_info.provider,
            &user_info.provider_id,
            &user_info.email,
            &user_info.name,
            user_info.avatar_url.as_deref(),
            user_info.email_verified,
        )
        .await?;

    let user_id = user.id.unwrap();

    // Generate JWT tokens
    let tokens = state
        .auth
        .generate_tokens(user_id, &user.email, &user.username)?;

    // Set the session cookie (Secure in prod) and clear the one-shot CSRF
    // state cookie now that it has been consumed.
    let cookie = format!(
        "access_token={}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}{}",
        tokens.access_token,
        tokens.expires_in,
        secure_attr(&state)
    );
    let clear_state = format!(
        "oauth_state=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0{}",
        secure_attr(&state)
    );

    let frontend_url = state.settings.oauth.base_url.replace(":5001", ":5000"); // API → UI port

    // The token goes in the FRAGMENT, not the query. A fragment is never sent
    // to a server, so it cannot land in an nginx access log or a `Referer` —
    // whereas `?token=<7-day JWT>` was written verbatim into the log of every
    // hop that served this redirect. The SPA reads `location.hash` and clears
    // it immediately; it still accepts the old query form for one deploy so a
    // cached older bundle keeps working.
    let redirect_url = format!(
        "{}/oauth/callback#token={}",
        frontend_url, tokens.access_token
    );

    let mut headers = HeaderMap::new();
    headers.append(header::SET_COOKIE, cookie.parse().unwrap());
    // An OAuth sign-in never got a refresh credential at all: the callback
    // redirect can only carry ONE value in the fragment, so the refresh token
    // was minted and thrown away, and the session simply died after the access
    // token's 7 days. A cookie has no such limit — so OAuth users get the same
    // 30-day renewable session as password users, and get it without anything
    // being written where script can read it.
    headers.append(
        header::SET_COOKIE,
        crate::routes::auth::refresh_cookie(&state, &tokens.refresh_token)
            .parse()
            .unwrap(),
    );
    headers.append(header::SET_COOKIE, clear_state.parse().unwrap());
    headers.insert(header::LOCATION, redirect_url.parse().unwrap());

    Ok((StatusCode::FOUND, headers).into_response())
}
