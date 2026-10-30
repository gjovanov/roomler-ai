// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P0e — the model sidecar: a loopback HTTP endpoint that a session's
//! Claude Code talks to instead of the provider (`docs/roomler-hive-design.md`
//! §11.2). P8-2c-3 gives each session its own port, served in the daemon
//! directly or (where the device's owner turned workers on) handed to the
//! session's own worker — [`handed`], mirroring
//! [`toolbelt::handed`](super::toolbelt::handed)'s own shape.
//!
//! # What it is for
//!
//! The provider key never reaches the session. The daemon runs the device's
//! `hive_api_key_helper` itself (as SYSTEM/root) and keeps what it prints; the
//! harness gets `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>` — this session's
//! own port is the whole address, since P8-2c-3 there is no longer a shared
//! one to route within — and a per-session TOKEN in `ANTHROPIC_API_KEY`. Each
//! call is checked, then sent on with the real key:
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
//! The device's credential goes on in the form it takes: an API key in
//! `x-api-key`, or a Claude subscription's token (`claude setup-token`) as a
//! bearer token under the OAuth beta (FR-90 P2k-1). Whichever it is, the
//! session's own credential headers never leave the host.
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
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zeroize::Zeroizing;

use super::supervisor::Supervisor;
use super::vault_connect::handed::Secret;

pub(crate) mod handed;

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

    /// FR-90 P8-2c-3 — this refusal, narrowed to a kind that crosses the
    /// worker's channel: a fixed, closed set, so a worker rebuilds the
    /// exact status and error vocabulary [`sidecar_admit`] and
    /// [`Supervisor::wait_for_lease_word`] already chose, never a generic
    /// one. An unrecognized `kind` (there should be none) falls back to
    /// `api_error`'s 503, never a guess that could read as a client error.
    fn into_wire(self) -> (AdmitRefusalKind, String) {
        let kind = match self.kind {
            "authentication_error" => AdmitRefusalKind::Authentication,
            "overloaded_error" => AdmitRefusalKind::Overloaded,
            _ => AdmitRefusalKind::Api,
        };
        (kind, self.message)
    }
}

/// FR-90 P8-2c-3 — why an admit call was refused, narrow enough to cross
/// the worker's channel and rebuild [`Refusal`]'s exact status and error
/// kind on the other side (never a raw [`StatusCode`]/`&'static str` pair,
/// neither of which round-trips through serde cleanly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum AdmitRefusalKind {
    Authentication,
    Overloaded,
    Api,
}

impl AdmitRefusalKind {
    fn into_refusal(self, message: String) -> Refusal {
        let (status, kind) = match self {
            Self::Authentication => (StatusCode::UNAUTHORIZED, "authentication_error"),
            Self::Overloaded => (StatusCode::SERVICE_UNAVAILABLE, "overloaded_error"),
            Self::Api => (StatusCode::SERVICE_UNAVAILABLE, "api_error"),
        };
        Refusal::new(status, kind, message)
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

    /// P2e-2 — a leased run calls the model only once the server has said,
    /// on the connection that is the latest now, that this device still
    /// holds it. After a reconnect the call waits for that word, at most
    /// `lease_wait`, and is refused past it in the provider's "overloaded"
    /// shape, which Claude Code retries. A call of any other run is the
    /// admission's to judge ([`Self::sidecar_admit`]).
    ///
    /// ⚠️ While the device is offline the latest connection is the one that
    /// closed, so its word still counts and `offline_grace` decides, as for
    /// every session (P0e). Only a NEW connection voids the word: the server
    /// may have moved the session while this device could not hear.
    pub(crate) async fn wait_for_lease_word(
        &self,
        session: ObjectId,
        token: Option<&str>,
    ) -> Result<(), Refusal> {
        let Some((owner, fence)) = token.and_then(|t| self.tokens().resolve(t)) else {
            return Ok(());
        };
        if owner != session || !self.leased(session, fence) {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + self.lease_wait();
        loop {
            // Registered before the check, so a word that lands between the
            // two still wakes this call.
            let heard = self.lease_heard().notified();
            tokio::pin!(heard);
            heard.as_mut().enable();
            if self.has_word(session, fence) {
                return Ok(());
            }
            if tokio::time::timeout_at(deadline, heard).await.is_err() {
                return Err(Refusal::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "overloaded_error",
                    format!(
                        "this device has not yet heard from Roomler that it still holds this \
                         session (waited {} s); ask again in a moment",
                        self.lease_wait().as_secs()
                    ),
                ));
            }
        }
    }

    /// FR-90 P8-2c-3 — [`wait_for_lease_word`](Self::wait_for_lease_word),
    /// [`sidecar_admit`](Self::sidecar_admit) and
    /// [`model_key`](Self::model_key), in that order, as one call: what a
    /// worker's [`handed::admit`](super::sidecar::handed::admit) asks of
    /// the daemon for one sidecar request, and what [`DaemonSource`] asks
    /// of `self` directly where there is no worker. The refusal crosses
    /// the channel as [`AdmitRefusalKind`]; a worker-hosted call rebuilds
    /// the same status and error kind this returns a daemon-hosted one
    /// directly.
    pub(crate) async fn sidecar_admit_and_key(
        &self,
        session: ObjectId,
        token: Option<&str>,
    ) -> Result<String, (AdmitRefusalKind, String)> {
        self.wait_for_lease_word(session, token)
            .await
            .map_err(Refusal::into_wire)?;
        self.sidecar_admit(session, token)
            .map_err(Refusal::into_wire)?;
        self.model_key()
            .await
            .map_err(|e| (AdmitRefusalKind::Api, e))
    }
}

/// Run the device's key helper and return the first line it prints. Never
/// logged — not the output, not the error's stdout.
///
/// P1i-2 — on Windows through `cmd.exe /d /s /c "<helper>"`, by its full path:
/// `/s` makes cmd strip the outer quotes and run the helper as written, as
/// `/bin/sh -c` does, and the line is passed raw, so no quoting of Rust's own
/// can change it. `/d`: no AutoRun from the registry runs first.
pub(crate) async fn run_key_helper(helper: &str) -> Result<String, String> {
    #[cfg(unix)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(helper);
        cmd
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new(crate::hive_win::system_cmd());
        cmd.raw_arg(format!("/d /s /c \"{helper}\""))
            // CREATE_NO_WINDOW: no console flashes on the user's desktop.
            .creation_flags(0x0800_0000);
        cmd
    };
    cmd.stdin(std::process::Stdio::null())
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

/// FR-90 P8-2c-3 — where [`handle`] asks admission of, and gets the
/// provider's key from: the daemon's own [`Supervisor`] directly where no
/// worker serves this session ([`DaemonSource`]), or a worker's channel to
/// it otherwise ([`handed::WorkerSource`]). Every sidecar is one session's
/// now (P8-2c-3 drops the old shared port's `/s/<sid>` multiplexing: the
/// port itself is the session boundary), so neither impl takes a session
/// id at call time — each already knows its own.
pub(crate) trait Source: Clone + Send + Sync + 'static {
    /// Whether `token` may call the model now, and if so, the device's
    /// key for this one call — never retained past it.
    fn admit(
        &self,
        token: Option<&str>,
    ) -> impl std::future::Future<Output = Result<Secret, Refusal>> + Send;
    /// The provider refused the key: fetch it again next time.
    fn forget_key(&self) -> impl std::future::Future<Output = ()> + Send;
    fn upstream(&self) -> &str;
    fn workspace(&self) -> Option<&str>;
    /// Tracing only.
    fn session(&self) -> ObjectId;
}

/// A sidecar's [`Source`] where no worker serves its session: the
/// daemon's own [`Supervisor`], called directly. `upstream`/`workspace`
/// are read once, at [`bind_for`]'s own call, since neither changes while
/// the daemon runs.
#[derive(Clone)]
struct DaemonSource {
    sup: Weak<Supervisor>,
    session: ObjectId,
    upstream: String,
    workspace: Option<String>,
}

impl Source for DaemonSource {
    async fn admit(&self, token: Option<&str>) -> Result<Secret, Refusal> {
        let Some(sup) = self.sup.upgrade() else {
            return Err(Refusal::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded_error",
                "this device is shutting down",
            ));
        };
        sup.sidecar_admit_and_key(self.session, token)
            .await
            .map(|k| Secret(Zeroizing::new(k)))
            .map_err(|(kind, message)| kind.into_refusal(message))
    }

    async fn forget_key(&self) {
        if let Some(sup) = self.sup.upgrade() {
            sup.forget_model_key().await;
        }
    }

    fn upstream(&self) -> &str {
        &self.upstream
    }

    fn workspace(&self) -> Option<&str> {
        self.workspace.as_deref()
    }

    fn session(&self) -> ObjectId {
        self.session
    }
}

/// A session's own sidecar, bound and serving — in the daemon directly,
/// or (P8-2c-3) handed to the session's worker. Mirrors
/// [`Toolbelt`](super::toolbelt::Toolbelt)'s own shape exactly.
pub(crate) struct Sidecar {
    port: u16,
    stop: CancellationToken,
    /// This run and the endpoint it was handed as, where the sidecar is
    /// handed to its worker instead of served here. `None` where it is
    /// served here (no worker up for this run yet, or `hive_worker` off).
    #[cfg(worker_calls)]
    handed: Option<(ObjectId, u64, u64)>,
}

impl Sidecar {
    /// The loopback port `sidecar_base_url` names.
    pub(crate) fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.stop.cancel();
        // Handed to a worker: forgotten here (so a later `rehand` finds
        // nothing), and told to stop — on a task of its own, since `Drop`
        // cannot await it. Mirrors `Toolbelt`'s own `Drop`.
        #[cfg(worker_calls)]
        if let Some((session, fence, endpoint)) = self.handed {
            handed::forget(session, fence);
            tokio::spawn(async move {
                if let Some(link) = crate::hive::worker::link_of(session, fence) {
                    let call = super::vault_connect::handed::Call::SidecarClose { endpoint };
                    #[cfg(unix)]
                    let _ = link.call(call, Vec::new()).await;
                    #[cfg(windows)]
                    let _ = link.call(call).await;
                }
            });
        }
    }
}

/// Bind `session`'s own sidecar and start serving it — handed to its
/// worker where one exists for `(session, fence)` ([`handed::prepare`]),
/// served here directly otherwise, exactly as
/// [`toolbelt::open`](super::toolbelt::open) does for the toolbelt's own
/// listener. `upstream`/`workspace` are the device's config, read once
/// here since neither changes while the daemon runs.
pub(crate) async fn open(
    sup: &Arc<Supervisor>,
    session: ObjectId,
    fence: u64,
    upstream: String,
    workspace: Option<String>,
) -> Result<Sidecar, String> {
    let (listener, port) = bind().await?;
    let stop = CancellationToken::new();
    #[cfg(worker_calls)]
    let prepared = handed::prepare(
        session,
        fence,
        &listener,
        Arc::downgrade(sup),
        upstream.clone(),
        workspace.clone(),
    )?;
    #[cfg(not(worker_calls))]
    let prepared: Option<u64> = None;
    if let Some(endpoint) = prepared {
        return Ok(Sidecar {
            port,
            stop,
            #[cfg(worker_calls)]
            handed: Some((session, fence, endpoint)),
        });
    }
    let source = DaemonSource {
        sup: Arc::downgrade(sup),
        session,
        upstream,
        workspace,
    };
    serve(listener, source, stop.clone());
    Ok(Sidecar {
        port,
        stop,
        #[cfg(worker_calls)]
        handed: None,
    })
}

/// Serve `source`'s sidecar on `listener` until `stop`. `source` is
/// cloned once a connection, never shared mutable state across one: each
/// request asks it afresh, so a device whose key rotated mid-connection
/// still gets the new one on its next request.
pub(crate) fn serve<S: Source>(listener: TcpListener, source: S, stop: CancellationToken) {
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
            let stream = tokio::select! {
                () = stop.cancelled() => return,
                accepted = listener.accept() => match accepted {
                    Ok((s, _)) => s,
                    Err(e) => {
                        debug!(%e, "hive: the model sidecar's accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                },
            };
            let source = source.clone();
            let client = client.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req| {
                    let source = source.clone();
                    let client = client.clone();
                    async move { Ok::<_, Infallible>(handle(req, &source, &client).await) }
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

/// FR-90 P2k-1 — how a Claude subscription's token begins, as `claude
/// setup-token` prints it (Anthropic's own prefix; an API key begins
/// `sk-ant-api`).
const SUBSCRIPTION_TOKEN_PREFIX: &str = "sk-ant-oat";
/// The beta under which the provider takes a subscription's token.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// Put the device's credential on a forwarded call, in the form it takes. A
/// subscription's token goes as a bearer token, with the OAuth beta added
/// once to the betas the harness asked for; it has no workspace, so none is
/// sent, the session's own included. Any other credential is an API key, in
/// `x-api-key`, with the device's workspace when it has one (P0g, field
/// 2026-10-07: a key not scoped to a workspace needs it on every call); the
/// device's replaces the session's.
fn put_credential(
    headers: &mut HeaderMap,
    key: &str,
    workspace: Option<&str>,
) -> Result<(), Refusal> {
    let not_a_key = || {
        Refusal::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "hive_api_key_helper printed something that is not a key",
        )
    };
    if key.starts_with(SUBSCRIPTION_TOKEN_PREFIX) {
        let bearer = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| not_a_key())?;
        headers.insert(header::AUTHORIZATION, bearer);
        let mut betas: Vec<String> = Vec::new();
        for value in headers.get_all("anthropic-beta") {
            for beta in value.to_str().unwrap_or("").split(',') {
                let beta = beta.trim();
                if !beta.is_empty() && !betas.iter().any(|b| b == beta) {
                    betas.push(beta.to_string());
                }
            }
        }
        if !betas.iter().any(|b| b == OAUTH_BETA) {
            betas.push(OAUTH_BETA.to_string());
        }
        // Each one came from a header value, or is ours: a valid value.
        let joined = HeaderValue::from_str(&betas.join(",")).map_err(|_| not_a_key())?;
        headers.insert("anthropic-beta", joined);
        headers.remove("anthropic-workspace-id");
        return Ok(());
    }
    headers.insert(
        "x-api-key",
        HeaderValue::from_str(key).map_err(|_| not_a_key())?,
    );
    if let Some(workspace) = workspace {
        let workspace = HeaderValue::from_str(workspace).map_err(|_| {
            Refusal::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "hive_api_workspace_id is not a workspace id",
            )
        })?;
        headers.insert("anthropic-workspace-id", workspace);
    }
    Ok(())
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

async fn handle<S: Source>(
    req: Request<Incoming>,
    source: &S,
    client: &reqwest::Client,
) -> Response<Body> {
    let session = source.session();
    let path = req.uri().path();
    if !forwarded(req.method(), path) {
        debug!(%session, method = %req.method(), path, "hive: a model call to a path the sidecar does not forward");
        return refusal(Refusal::new(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "the model sidecar forwards only POST /v1/messages and /v1/messages/count_tokens",
        ));
    }
    let path = path.to_string();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let token = presented_token(req.headers());
    // P2e-2's lease wait, `sidecar_admit`'s checks and the device's key,
    // in one round trip — the daemon directly, or (P8-2c-3) its worker's
    // channel to it.
    let key = match source.admit(token.as_deref()).await {
        Ok(k) => k,
        Err(r) => {
            debug!(%session, status = r.status.as_u16(), "hive: a model call was refused");
            return refusal(r);
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
    if let Err(r) = put_credential(&mut headers, &key.0, source.workspace()) {
        return refusal(r);
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

    let url = format!("{}{path}{query}", source.upstream().trim_end_matches('/'));
    let upstream = match client
        .request(method, &url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(%session, %e, "hive: the model provider could not be reached");
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
        source.forget_key().await;
        info!(%session, "hive: the provider refused the device's key — it will be fetched again");
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

#[cfg(all(test, unix))]
mod tests;
