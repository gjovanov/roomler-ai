// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P0e — the model sidecar: a loopback HTTP endpoint inside the daemon
//! that every session's Claude Code talks to instead of the provider
//! (`docs/roomler-hive-design.md` §11.2).
//!
//! # What it is for
//!
//! The provider key never reaches the session. The daemon runs the device's
//! `hive_api_key_helper` itself (as SYSTEM/root) and keeps what it prints; the
//! harness gets `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/s/<sid>` and a
//! per-session TOKEN in `ANTHROPIC_API_KEY`. Each call is checked, then sent
//! on with the real key:
//!
//! 1. the token names THIS session, at the fence it was minted for, and the
//!    session still runs here at that fence — a stopped session's token is
//!    dead, and after a move a stale primary cannot spend;
//! 2. the device has heard from the server within `offline_grace` (120 s) —
//!    a partitioned primary stalls instead of diverging (design §4.6).
//!
//! # How it forwards
//!
//! Claude Code's LLM-gateway contract: the method, path, query, body and the
//! `anthropic-*` headers go upstream unchanged; the response streams back as
//! it arrives — an SSE turn is NEVER buffered — with `retry-after`,
//! `x-should-retry` and the rate-limit headers intact. Upstream TLS is
//! reqwest's: the OS trust store and the proxy environment, so a
//! TLS-inspecting corporate middlebox whose root the machine trusts works.
//!
//! Only what that contract needs is forwarded: `POST /v1/messages` and
//! `POST /v1/messages/count_tokens`, matched exactly. The device's key opens
//! more than inference — the Files API holds every session's uploads under
//! it, and a batch runs on after its session and its fence — so a session
//! token opens nothing else.
//!
//! Refusals answer in the provider's own error shape, so the harness shows a
//! person a sentence rather than a parse failure.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use bson::oid::ObjectId;
use bytes::Bytes;
use futures::StreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use super::supervisor::Supervisor;

/// Where model calls go when nothing else is configured.
pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";
/// How long a device that lost its control connection may keep calling the
/// model (design §4.6): long enough to ride out a reconnect, short enough
/// that a partitioned primary stops before a promoted one has gone far.
pub const OFFLINE_GRACE: Duration = Duration::from_secs(120);
/// The largest request body forwarded. A long session's request carries the
/// whole conversation, so this is generous — but bounded.
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
/// How long the key the helper printed is reused before it runs again.
pub const KEY_TTL: Duration = Duration::from_secs(5 * 60);
/// The helper's time limit.
const HELPER_TIMEOUT: Duration = Duration::from_secs(10);

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Body = BoxBody<Bytes, BoxError>;

/// The tokens the device minted for its sessions, each for one session at
/// one fence.
#[derive(Default)]
pub(crate) struct Tokens {
    map: Mutex<HashMap<String, (ObjectId, u64)>>,
}

impl Tokens {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (ObjectId, u64)>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A fresh token for `session` at `fence`: 256 random bits.
    pub(crate) fn mint(&self, session: ObjectId, fence: u64) -> String {
        let token = format!("hive-{}", hex::encode(rand::random::<[u8; 32]>()));
        self.lock().insert(token.clone(), (session, fence));
        token
    }

    /// `token` stops working. One run's token exactly: a run that ends just
    /// as the same session starts again here must not take the new run's
    /// with it.
    pub(crate) fn revoke(&self, token: &str) {
        self.lock().remove(token);
    }

    fn resolve(&self, token: &str) -> Option<(ObjectId, u64)> {
        self.lock().get(token).copied()
    }
}

/// Why a call was not forwarded, in the provider's error vocabulary.
pub(crate) struct Refusal {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
}

impl Refusal {
    pub(crate) fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
        }
    }
}

impl Supervisor {
    /// The sidecar's gates for one call: a token of THIS session, at the
    /// fence the session runs at here, from a device that is not offline
    /// past the grace.
    pub(crate) fn sidecar_admit(
        &self,
        session: ObjectId,
        token: Option<&str>,
    ) -> Result<(), Refusal> {
        let unauthorized =
            |m: &str| Refusal::new(StatusCode::UNAUTHORIZED, "authentication_error", m);
        let token = token.ok_or_else(|| unauthorized("no session token"))?;
        let (owner, fence) = self
            .tokens()
            .resolve(token)
            .ok_or_else(|| unauthorized("this is not a session token of this device"))?;
        if owner != session {
            return Err(unauthorized("the token belongs to another session"));
        }
        if !self.runs_at(session, fence) {
            return Err(unauthorized(
                "the session no longer runs on this device at this fence",
            ));
        }
        if let Some(offline) = self.offline_for()
            && offline > self.offline_grace()
        {
            return Err(Refusal::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded_error",
                format!(
                    "this device lost its connection to Roomler {} s ago; model calls stop after {} s",
                    offline.as_secs(),
                    self.offline_grace().as_secs()
                ),
            ));
        }
        Ok(())
    }
}

/// Run the device's key helper and return the first line it prints. Never
/// logged — not the output, not the error's stdout.
pub(crate) async fn run_key_helper(helper: &str) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(helper)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(HELPER_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            format!(
                "hive_api_key_helper did not finish within {} s",
                HELPER_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("hive_api_key_helper could not run: {e}"))?;
    if !out.status.success() {
        return Err(format!("hive_api_key_helper failed ({})", out.status));
    }
    let key = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if key.is_empty() {
        return Err("hive_api_key_helper printed no key".into());
    }
    Ok(key)
}

/// Bind the sidecar on a random loopback port. Loopback only: nothing off
/// this host can reach it, and a local user without a token gets nothing.
pub(crate) async fn bind() -> Result<(TcpListener, u16), String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|e| format!("binding the model sidecar: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("the model sidecar's address: {e}"))?
        .port();
    Ok((listener, port))
}

/// Serve the sidecar until the supervisor is gone.
pub(crate) fn serve(listener: TcpListener, sup: Weak<Supervisor>) {
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(%e, "hive: the model sidecar has no HTTP client — every call will fail");
            reqwest::Client::new()
        }
    };
    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _)) => s,
                Err(e) => {
                    debug!(%e, "hive: the model sidecar's accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Some(sup) = sup.upgrade() else { return };
            let client = client.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req| {
                    let sup = Arc::clone(&sup);
                    let client = client.clone();
                    async move { Ok::<_, Infallible>(handle(req, &sup, &client).await) }
                });
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await
                {
                    debug!(%e, "hive: a model sidecar connection ended with an error");
                }
            });
        }
    });
}

/// `/s/<session id><rest>` → the session and the path to forward.
fn split_session(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/s/")?;
    let (sid, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    Some((sid, tail))
}

/// Whether a session may make this provider call: inference and token
/// counting, nothing else. Exact, so no other spelling — a `..` segment, an
/// encoded `/`, a longer path — reaches the provider on the device's key.
fn forwarded(method: &hyper::Method, path: &str) -> bool {
    method == hyper::Method::POST && matches!(path, "/v1/messages" | "/v1/messages/count_tokens")
}

/// The token the harness presented: `x-api-key`, or a bearer token.
fn presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(v.trim().to_string());
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim().to_string())
}

/// Headers that belong to one hop, never forwarded either way.
fn hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
    )
}

fn refusal(r: Refusal) -> Response<Body> {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": r.kind, "message": r.message },
    })
    .to_string();
    let mut resp = Response::new(
        Full::new(Bytes::from(body))
            .map_err(|never| match never {})
            .boxed(),
    );
    *resp.status_mut() = r.status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

async fn handle(
    req: Request<Incoming>,
    sup: &Arc<Supervisor>,
    client: &reqwest::Client,
) -> Response<Body> {
    let Some((sid_hex, rest)) = split_session(req.uri().path()) else {
        return refusal(Refusal::new(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "not a session path",
        ));
    };
    let Ok(session) = ObjectId::parse_str(sid_hex) else {
        return refusal(Refusal::new(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "not a session path",
        ));
    };
    if !forwarded(req.method(), rest) {
        debug!(session = %session, method = %req.method(), path = rest, "hive: a model call to a path the sidecar does not forward");
        return refusal(Refusal::new(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "the model sidecar forwards only POST /v1/messages and /v1/messages/count_tokens",
        ));
    }
    let rest = rest.to_string();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let token = presented_token(req.headers());
    if let Err(r) = sup.sidecar_admit(session, token.as_deref()) {
        debug!(session = %session, status = r.status.as_u16(), "hive: a model call was refused");
        return refusal(r);
    }
    let key = match sup.model_key().await {
        Ok(k) => k,
        Err(e) => {
            return refusal(Refusal::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                e,
            ));
        }
    };

    let mut headers = HeaderMap::new();
    for (name, value) in req.headers() {
        if hop_by_hop(name)
            || name == "x-api-key"
            || name == header::AUTHORIZATION
            || name == header::CONTENT_LENGTH
        {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    match HeaderValue::from_str(&key) {
        Ok(v) => {
            headers.insert("x-api-key", v);
        }
        Err(_) => {
            return refusal(Refusal::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "hive_api_key_helper printed something that is not a key",
            ));
        }
    }
    // A key not scoped to a workspace needs this on every call (P0g, field
    // 2026-10-07). It travels with the key: the device's, never the
    // session's — whatever the session sent is replaced.
    if let Some(workspace) = sup.model_workspace() {
        match HeaderValue::from_str(workspace) {
            Ok(v) => {
                headers.insert("anthropic-workspace-id", v);
            }
            Err(_) => {
                return refusal(Refusal::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "api_error",
                    "hive_api_workspace_id is not a workspace id",
                ));
            }
        }
    }
    let method = req.method().clone();
    let body = match Limited::new(req.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return refusal(Refusal::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                format!("a model request is at most {MAX_REQUEST_BYTES} bytes"),
            ));
        }
    };

    let url = format!("{}{rest}{query}", sup.upstream().trim_end_matches('/'));
    let upstream = match client
        .request(method, &url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(session = %session, %e, "hive: the model provider could not be reached");
            return refusal(Refusal::new(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("the model provider could not be reached: {e}"),
            ));
        }
    };
    let status = upstream.status();
    if status == StatusCode::UNAUTHORIZED {
        // The key may have rotated: ask the helper again next time.
        sup.forget_model_key().await;
        info!(session = %session, "hive: the provider refused the device's key — it will be fetched again");
    }
    let mut resp = Response::builder().status(status);
    if let Some(h) = resp.headers_mut() {
        for (name, value) in upstream.headers() {
            if !hop_by_hop(name) {
                h.append(name.clone(), value.clone());
            }
        }
    }
    // Streamed as it arrives — an SSE turn is never buffered here.
    let stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map(Frame::data).map_err(|e| Box::new(e) as BoxError));
    match resp.body(BodyExt::boxed(StreamBody::new(stream))) {
        Ok(r) => r,
        Err(e) => refusal(Refusal::new(
            StatusCode::BAD_GATEWAY,
            "api_error",
            format!("the provider's answer could not be relayed: {e}"),
        )),
    }
}

#[cfg(test)]
mod tests;
