// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-129 A1 — device pairing: `/api/pair/{start,poll,lookup,approve,deny}`.
//!
//! An installer that has no token asks for one the way RFC 8628 shapes it.
//! `start` hands it a secret **device code** and a short **user code**; the
//! person opens `/pair` in a browser, types the user code, sees the device,
//! and approves it into an org; the installer's `poll` then receives an
//! ordinary enrollment token. The spec
//! (`docs/fr/FR-129-guided-cross-platform-installer.md`, "The pairing API")
//! is the contract; these are the invariants the code below keeps:
//!
//! 1. **Two codes, two holders.** The device code is 32 CSPRNG bytes,
//!    base64url, held only by the installer, stored only as a SHA-256 hash,
//!    sent only in a POST body — never logged, never in a URL, never in any
//!    response but `start`'s. It is the ONLY credential that retrieves a
//!    token. The user code (8 consonants from `BCDFGHJKLMNPQRSTVWXZ`, shown
//!    `XXXX-XXXX`) names the pairing and retrieves nothing.
//! 2. **Mint on delivery, never on approval.** `approve` records a decision
//!    (CAS `pending → approved`). The first `poll` that sees `approved` wins
//!    the CAS `approved → delivered` and THAT is the right to mint; a loser
//!    gets `expired_token`. No token ever rests in the database.
//! 3. **The same gate, re-checked at delivery.** Daemon: `MANAGE_AGENTS`;
//!    tunnel: membership — at approve AND again at delivery. An approver
//!    demoted in between makes the pairing `denied`.
//! 4. **The ordinary tokens, by the existing calls**, FR-102 ledger
//!    included; everything downstream (the enroll route, the device cap, the
//!    archived-org refusal, the single-use claim) holds unchanged.
//! 5. **No pod-local state.** The Mongo row is the only state; the limiters
//!    are abuse ceilings, per pod, and nothing waits in memory.
//! 6. **Trusted client IP only**, through `roomler_core::client_ip`.
//! 7. **Every refusal is audited** (`pairing_audit`), best-effort.
//! 8. `[pairing] enabled = false` makes every route the empty 404 an older
//!    server answers, so a client falls back to the paste step.

use std::net::{IpAddr, Ipv4Addr};

use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::Engine;
use bson::{DateTime, oid::ObjectId};
use rand::RngCore;
use roomler_ai_db::models::{DevicePairing, PairingAuditEvent, PairingStatus, role::permissions};
use roomler_ai_remote_control::models::OsKind;
use roomler_ai_services::dao::base::DaoError;
use roomler_core::{
    ApiError,
    client_ip::{PeerAddr, client_ip_or_peer},
    extractors::auth::{AuthUser, OptionalAuthUser},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::{FleetState, agent::ENROLLMENT_TTL_SECS, agent::require_permission};

/// The user code's alphabet: the 20 consonants with no look-alikes, so a
/// code read off a screen and typed on a phone survives the trip.
pub const USER_CODE_ALPHABET: &[u8; 20] = b"BCDFGHJKLMNPQRSTVWXZ";
/// Eight of them, ~34.6 bits: shown as two groups of four.
pub const USER_CODE_LEN: usize = 8;
/// The device code: 256 random bits.
const DEVICE_CODE_BYTES: usize = 32;
/// RFC 8628 §3.5 — a poll sooner than the interval lengthens it by this.
pub const SLOW_DOWN_STEP_SECS: u32 = 5;
/// The interval never grows past this: a misbehaving client is throttled
/// by the governor, not by an interval that outlives the pairing.
pub const MAX_POLL_INTERVAL_SECS: u32 = 60;
/// The tunnel enrollment token's life — the network module's
/// `TUNNEL_ENROLLMENT_TTL_SECS` (`routes/tunnel.rs`), which fleet cannot
/// name without a graph edge; both are the agent token's 10 minutes.
const TUNNEL_ENROLLMENT_TTL_SECS: u64 = 600;
/// `machine_name`, `client` and `client_version` are trimmed and capped to
/// this many characters, never refused.
pub const FIELD_MAX_CHARS: usize = 64;
/// How many times `start` regenerates on a user-code collision before
/// answering 503.
const CODE_RETRIES: usize = 5;

/// The module the tunnel role's token is redeemed on.
const NETWORK_MODULE: &str = "network";

// ── the wire ────────────────────────────────────────────────────────────────

/// `arch` as the installer reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
}

impl Arch {
    pub fn wire(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }
}

/// Which enrollment token delivery mints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingRole {
    /// `roomlerd` — `MANAGE_AGENTS`, the agent enrollment token.
    Daemon,
    /// The `roomler` CLI — membership, the tunnel enrollment token.
    Tunnel,
}

impl PairingRole {
    pub fn wire(self) -> &'static str {
        match self {
            PairingRole::Daemon => "daemon",
            PairingRole::Tunnel => "tunnel",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "daemon" => Some(PairingRole::Daemon),
            "tunnel" => Some(PairingRole::Tunnel),
            _ => None,
        }
    }
}

/// `OsKind`'s wire spelling (its serde is `snake_case`), as the row and the
/// lookup carry it.
pub fn os_wire(os: OsKind) -> &'static str {
    match os {
        OsKind::Linux => "linux",
        OsKind::Macos => "macos",
        OsKind::Windows => "windows",
    }
}

/// The clients that start a pairing. An unknown spelling is clamped to
/// `other`, never refused — a newer installer must keep pairing against
/// this server ([`clamp_client`]).
pub const CLIENTS: [&str; 5] = ["wizard", "roomlerd", "roomler", "script-sh", "script-ps1"];

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    #[serde(default)]
    pub machine_name: String,
    pub os: OsKind,
    pub arch: Arch,
    pub role: PairingRole,
    /// 1, or 2 for a macOS daemon whose root half is a second enrollment.
    #[serde(default = "one")]
    pub halves: u8,
    #[serde(default)]
    pub client: String,
    #[serde(default)]
    pub client_version: String,
}

fn one() -> u8 {
    1
}

#[derive(Debug, Serialize)]
pub struct StartResponse {
    /// The installer's secret. This is the only response that carries it.
    pub device_code: String,
    /// `XXXX-XXXX`, for the person to type.
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
pub struct PollRequest {
    pub device_code: String,
}

#[derive(Debug, Deserialize)]
pub struct CodeRequest {
    pub user_code: String,
}

#[derive(Debug, Deserialize)]
pub struct ApproveRequest {
    pub user_code: String,
    pub tenant_id: String,
}

#[derive(Debug, Serialize)]
pub struct LookupResponse {
    pub machine_name: String,
    pub os: String,
    pub arch: String,
    pub role: String,
    pub halves: u8,
    pub client: String,
    pub client_version: String,
    pub country: Option<String>,
    pub started_at: String,
    pub expires_at: String,
}

// ── codes ───────────────────────────────────────────────────────────────────

/// Eight consonants from [`USER_CODE_ALPHABET`], without the hyphen (the
/// stored form). Rejection sampling, so every letter is equally likely.
pub fn generate_user_code() -> String {
    let mut rng = rand::rng();
    let mut out = String::with_capacity(USER_CODE_LEN);
    while out.len() < USER_CODE_LEN {
        let b = (rng.next_u32() & 0xff) as u8;
        // 20 × 12 = 240: anything at or above is thrown away, not folded.
        if b < 240 {
            out.push(USER_CODE_ALPHABET[(b % 20) as usize] as char);
        }
    }
    out
}

/// `BCDFGHJK` → `BCDF-GHJK`, the shape the installer shows and `/pair`
/// echoes.
pub fn format_user_code(stored: &str) -> String {
    if stored.len() == USER_CODE_LEN {
        format!("{}-{}", &stored[..4], &stored[4..])
    } else {
        stored.to_string()
    }
}

/// What the person typed → the stored form: case-insensitive, hyphens and
/// spaces optional. `None` for anything that is not exactly eight letters
/// of the alphabet — such a code names no pairing and is counted as a miss.
pub fn normalize_user_code(typed: &str) -> Option<String> {
    let mut out = String::with_capacity(USER_CODE_LEN);
    for c in typed.chars() {
        if c == '-' || c == ' ' {
            continue;
        }
        let up = c.to_ascii_uppercase();
        if !up.is_ascii_alphabetic() || !USER_CODE_ALPHABET.contains(&(up as u8)) {
            return None;
        }
        if out.len() == USER_CODE_LEN {
            return None;
        }
        out.push(up);
    }
    (out.len() == USER_CODE_LEN).then_some(out)
}

/// 32 CSPRNG bytes as unpadded base64url (43 characters).
pub fn generate_device_code() -> String {
    let mut bytes = [0u8; DEVICE_CODE_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `hex(SHA-256(device_code))` — the only form the database ever holds.
pub fn hash_device_code(device_code: &str) -> String {
    hex::encode(Sha256::digest(device_code.as_bytes()))
}

/// Trim and cap at [`FIELD_MAX_CHARS`] characters; never refuse.
pub fn clamp_field(s: &str) -> String {
    s.trim().chars().take(FIELD_MAX_CHARS).collect()
}

/// A client name in [`CLIENTS`], or `other` — the server's clamp, as
/// `InstallInfo::clamped` does for the install trace.
pub fn clamp_client(s: &str) -> &'static str {
    let s = s.trim();
    CLIENTS
        .iter()
        .copied()
        .find(|c| c.eq_ignore_ascii_case(s))
        .unwrap_or("other")
}

/// The request-shape rules a `start` must pass (400 with the code).
pub fn validate_start(req: &StartRequest) -> Result<(), &'static str> {
    match req.halves {
        1 => Ok(()),
        2 if req.os == OsKind::Macos && req.role == PairingRole::Daemon => Ok(()),
        2 => Err("halves: 2 is only for a macOS daemon"),
        _ => Err("halves must be 1 or 2"),
    }
}

// ── the poll decision ───────────────────────────────────────────────────────

/// What a poll finds, decided from the row and the clock alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollDecision {
    /// Denied by a person, or at delivery.
    Denied,
    /// Delivered, swept, or already expired: `expired_token`, no oracle.
    Gone,
    /// Live status but past `expires_at`: CAS to `expired`, audit once.
    Expired,
    /// Sooner than the row's interval: tell the installer the longer one.
    SlowDown {
        interval: u32,
    },
    Pending,
    /// Approved and live: re-check the gate and race for the mint.
    Deliver,
}

pub fn decide_poll(
    status: PairingStatus,
    expires_at_ms: i64,
    last_poll_at_ms: Option<i64>,
    poll_interval_secs: u32,
    now_ms: i64,
) -> PollDecision {
    match status {
        PairingStatus::Denied => return PollDecision::Denied,
        PairingStatus::Delivered | PairingStatus::Expired => return PollDecision::Gone,
        PairingStatus::Pending | PairingStatus::Approved => {}
    }
    if expires_at_ms <= now_ms {
        return PollDecision::Expired;
    }
    if let Some(last) = last_poll_at_ms
        && now_ms - last < i64::from(poll_interval_secs) * 1000
    {
        return PollDecision::SlowDown {
            interval: (poll_interval_secs + SLOW_DOWN_STEP_SECS).min(MAX_POLL_INTERVAL_SECS),
        };
    }
    match status {
        PairingStatus::Pending => PollDecision::Pending,
        _ => PollDecision::Deliver,
    }
}

// ── audit ───────────────────────────────────────────────────────────────────

/// One row for the decision log, filled by the route and written
/// best-effort: a failed insert is logged and the request proceeds.
struct Audit {
    action: &'static str,
    outcome: String,
    pairing: Option<ObjectId>,
    user_id: Option<ObjectId>,
    tenant_id: Option<ObjectId>,
    client_ip: Option<IpAddr>,
    machine_name: Option<String>,
    role: Option<String>,
}

impl Audit {
    fn new(action: &'static str) -> Self {
        Self {
            action,
            outcome: "ok".to_string(),
            pairing: None,
            user_id: None,
            tenant_id: None,
            client_ip: None,
            machine_name: None,
            role: None,
        }
    }

    fn refused(mut self, reason: &str) -> Self {
        self.outcome = format!("refused:{reason}");
        self
    }

    fn pairing(mut self, row: &DevicePairing) -> Self {
        self.pairing = row.id;
        self.machine_name = Some(row.machine_name.clone());
        self.role = Some(row.role.clone());
        if self.tenant_id.is_none() {
            self.tenant_id = row.tenant_id;
        }
        self
    }

    fn user(mut self, user_id: ObjectId) -> Self {
        self.user_id = Some(user_id);
        self
    }

    fn tenant(mut self, tenant_id: ObjectId) -> Self {
        self.tenant_id = Some(tenant_id);
        self
    }

    fn ip(mut self, ip: Option<IpAddr>) -> Self {
        self.client_ip = ip;
        self
    }

    async fn record(self, state: &FleetState) {
        let event = PairingAuditEvent {
            id: None,
            action: self.action.to_string(),
            outcome: self.outcome,
            pairing_id: self.pairing,
            user_id: self.user_id,
            tenant_id: self.tenant_id,
            client_ip: self.client_ip.map(|ip| ip.to_string()),
            machine_name: self.machine_name,
            role: self.role,
            at: DateTime::now(),
        };
        if let Err(e) = state.pairing_audit.record(event).await {
            warn!(action = self.action, error = %e, "pairing audit: could not record the decision");
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// `{error, message}` with a status the `ApiError` enum has no variant for
/// (429), in the shape every other error answers.
fn refuse(status: StatusCode, error: &'static str, message: &str) -> Response {
    (status, Json(json!({ "error": error, "message": message }))).into_response()
}

/// The kill switch: the empty 404 an older server answers, so a client
/// cannot tell "switched off" from "not there" and falls back to paste.
fn disabled(state: &FleetState) -> Option<Response> {
    (!state.settings.pairing.enabled).then(|| StatusCode::NOT_FOUND.into_response())
}

fn client_ip(state: &FleetState, headers: &HeaderMap, peer: PeerAddr) -> Option<IpAddr> {
    client_ip_or_peer(headers, peer.0, state.settings.app.rate_limit_trusted_hops)
}

/// The body as `T`, or the 400 to answer. The refusals below are boxed
/// because a `Response` is a large `Err` (clippy's `result_large_err`), and
/// every caller returns it at once.
fn parse_body<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, Box<Response>> {
    serde_json::from_slice(body).map_err(|e| {
        Box::new(refuse(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            &format!("Invalid request body: {e}"),
        ))
    })
}

fn status_json(status: &str) -> Response {
    Json(json!({ "status": status })).into_response()
}

fn rfc3339(t: DateTime) -> String {
    t.try_to_rfc3339_string().unwrap_or_default()
}

/// The signed-in person, after the kill switch: a disabled server answers
/// 404 even to a request with no token, so the extractor is optional and
/// the 401 is ours.
fn require_auth(auth: OptionalAuthUser) -> Result<AuthUser, Box<Response>> {
    auth.0.ok_or_else(|| {
        Box::new(ApiError::Unauthorized("No token provided".to_string()).into_response())
    })
}

/// A live `pending` row for a typed code, or the reason there is none. A
/// pending row found past its expiry is flipped to `expired` on the way
/// (audited once), so the code is free for the sweep and the person gets
/// the same 404 the sweep would have left.
async fn live_pending(
    state: &FleetState,
    typed: &str,
    now: DateTime,
) -> Result<Option<DevicePairing>, Box<Response>> {
    let Some(code) = normalize_user_code(typed) else {
        return Ok(None);
    };
    let row = state
        .device_pairings
        .find_pending_by_user_code(&code)
        .await
        .map_err(|e| Box::new(ApiError::from(e).into_response()))?;
    match row {
        Some(r) if r.expires_at > now => Ok(Some(r)),
        Some(r) => {
            expire_once(state, &r).await;
            Ok(None)
        }
        None => Ok(None),
    }
}

/// CAS the row to `expired` and audit the expiry if THIS call did it.
async fn expire_once(state: &FleetState, row: &DevicePairing) {
    let Some(id) = row.id else { return };
    match state.device_pairings.expire(id).await {
        Ok(true) => Audit::new("expire").pairing(row).record(state).await,
        Ok(false) => {}
        Err(e) => debug!(pairing = %id, error = %e, "pairing: could not mark expired"),
    }
}

/// The per-account miss ceiling, read before acting.
fn account_locked(state: &FleetState, user_id: ObjectId) -> bool {
    state
        .pairing_miss_limiter
        .at_limit(&user_id, state.settings.pairing.misses_per_account_per_hour)
}

fn locked_response() -> Response {
    refuse(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "Too many pairing codes that matched nothing; try again in an hour",
    )
}

/// The gate a role's approver must hold, as the approve and the delivery
/// both ask it. `Ok(Ok(()))` holds, `Ok(Err(..))` is a refusal with its
/// audit reason and the error to answer, and a database that could not
/// answer is an error the caller returns as is — a Mongo blip must not
/// deny a pairing.
///
/// ⚠️ A refusal here is always a plain `Forbidden`, NEVER
/// [`ApiError::NotAMember`] — even for a caller who is not in `tenant_id`.
/// The SPA navigates the person OUT of the org on the server-sent
/// `not_a_member` code (the one 403 with a navigation, `docs/security-
/// baseline.md`), and an approval refused mid-flow must not evict the
/// approver from the `/pair` page. The audit reason keeps the distinction.
async fn gate_holds(
    state: &FleetState,
    role: PairingRole,
    tenant_id: ObjectId,
    user_id: ObjectId,
) -> Result<Result<(), (&'static str, ApiError)>, ApiError> {
    let not_a_member = || {
        (
            "not_a_member",
            ApiError::Forbidden("Not a member of that organization".to_string()),
        )
    };
    match role {
        PairingRole::Daemon => {
            match require_permission(
                state,
                tenant_id,
                user_id,
                permissions::MANAGE_AGENTS,
                "MANAGE_AGENTS",
            )
            .await
            {
                Ok(()) => Ok(Ok(())),
                Err(ApiError::NotAMember) => Ok(Err(not_a_member())),
                Err(e @ ApiError::Forbidden(_)) => Ok(Err(("no_permission", e))),
                Err(e) => Err(e),
            }
        }
        PairingRole::Tunnel => {
            if state.tenants.is_member(tenant_id, user_id).await? {
                Ok(Ok(()))
            } else {
                Ok(Err(not_a_member()))
            }
        }
    }
}

// ── routes ──────────────────────────────────────────────────────────────────

/// `POST /api/pair/start` — public. The device code in the response is the
/// one time it leaves the server.
pub async fn start(
    State(state): State<FleetState>,
    headers: HeaderMap,
    peer: PeerAddr,
    body: Bytes,
) -> Response {
    if let Some(r) = disabled(&state) {
        return r;
    }
    let ip = client_ip(&state, &headers, peer);
    // The per-address ceiling, on top of the governor: a flood must not be
    // able to fill the user-code space with pending rows.
    let key = ip.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    if !state
        .pairing_start_limiter
        .check(key, state.settings.pairing.start_per_ip_per_hour)
    {
        Audit::new("start")
            .refused("ip_ceiling")
            .ip(ip)
            .record(&state)
            .await;
        return refuse(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many pairings started from this address; try again later",
        );
    }
    let req: StartRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(r) => {
            Audit::new("start")
                .refused("invalid_request")
                .ip(ip)
                .record(&state)
                .await;
            return *r;
        }
    };
    let machine_name = clamp_field(&req.machine_name);
    if let Err(reason) = validate_start(&req) {
        let mut a = Audit::new("start").refused("invalid_request").ip(ip);
        a.machine_name = Some(machine_name);
        a.role = Some(req.role.wire().to_string());
        a.record(&state).await;
        return refuse(StatusCode::BAD_REQUEST, "invalid_request", reason);
    }
    // A tunnel token is redeemed on the network module's enroll route; with
    // that module unmounted the token would have nowhere to go.
    if req.role == PairingRole::Tunnel && !state.mounted.contains(NETWORK_MODULE) {
        let mut a = Audit::new("start").refused("network_unmounted").ip(ip);
        a.machine_name = Some(machine_name);
        a.role = Some(req.role.wire().to_string());
        a.record(&state).await;
        return refuse(
            StatusCode::BAD_REQUEST,
            "network_unmounted",
            "This server does not serve tunnel clients",
        );
    }

    let settings = &state.settings.pairing;
    let now = DateTime::now();
    let expires_at =
        DateTime::from_millis(now.timestamp_millis() + settings.ttl_secs as i64 * 1000);
    // The address is used once, here, to resolve a country; the row keeps
    // the address for the audit and shows the person only the country.
    let country = ip.and_then(|ip| state.geoip.country(ip));
    let interval = u32::try_from(settings.poll_interval_secs).unwrap_or(u32::MAX);

    for attempt in 0..CODE_RETRIES {
        let device_code = generate_device_code();
        let user_code = generate_user_code();
        let row = DevicePairing {
            id: None,
            device_code_hash: hash_device_code(&device_code),
            user_code: user_code.clone(),
            status: PairingStatus::Pending,
            machine_name: machine_name.clone(),
            os: os_wire(req.os).to_string(),
            arch: req.arch.wire().to_string(),
            role: req.role.wire().to_string(),
            halves: req.halves,
            client: clamp_client(&req.client).to_string(),
            client_version: clamp_field(&req.client_version),
            start_ip: ip.map(|ip| ip.to_string()),
            country: country.clone(),
            created_at: now,
            expires_at,
            last_poll_at: None,
            poll_interval_secs: interval,
            approver_user_id: None,
            tenant_id: None,
            decided_at: None,
            delivered_at: None,
            updated_at: now,
        };
        match state.device_pairings.create(&row).await {
            Ok(id) => {
                let mut stored = row;
                stored.id = Some(id);
                info!(pairing = %id, role = stored.role, os = stored.os, "pairing: started");
                Audit::new("start")
                    .pairing(&stored)
                    .ip(ip)
                    .record(&state)
                    .await;
                return Json(StartResponse {
                    device_code,
                    user_code: format_user_code(&user_code),
                    verification_uri: format!(
                        "{}/pair",
                        state.settings.app.frontend_url.trim_end_matches('/')
                    ),
                    expires_in: settings.ttl_secs,
                    interval: settings.poll_interval_secs,
                })
                .into_response();
            }
            // The user code is held by another live pairing (or, in theory,
            // the hash collided): both codes are regenerated, never reused.
            Err(DaoError::DuplicateKey(_)) => {
                debug!(attempt, "pairing: code collision, regenerating");
            }
            Err(e) => {
                warn!(error = %e, "pairing: could not store the pairing");
                return ApiError::from(e).into_response();
            }
        }
    }
    ApiError::ServiceUnavailable("Could not allocate a pairing code; please retry".to_string())
        .into_response()
}

/// `POST /api/pair/poll` — public; the device code is the credential. An
/// unknown, swept, delivered or expired code all answer `expired_token`:
/// the response never says which.
pub async fn poll(
    State(state): State<FleetState>,
    headers: HeaderMap,
    peer: PeerAddr,
    body: Bytes,
) -> Response {
    if let Some(r) = disabled(&state) {
        return r;
    }
    let ip = client_ip(&state, &headers, peer);
    let req: PollRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let row = match state
        .device_pairings
        .find_by_hash(&hash_device_code(req.device_code.trim()))
        .await
    {
        Ok(Some(r)) => r,
        Ok(None) => {
            Audit::new("refuse")
                .refused("unknown_code")
                .ip(ip)
                .record(&state)
                .await;
            return status_json("expired_token");
        }
        Err(e) => return ApiError::from(e).into_response(),
    };
    let Some(id) = row.id else {
        return ApiError::Internal("pairing row has no id".to_string()).into_response();
    };
    let now = DateTime::now();
    match decide_poll(
        row.status,
        row.expires_at.timestamp_millis(),
        row.last_poll_at.map(|t| t.timestamp_millis()),
        row.poll_interval_secs,
        now.timestamp_millis(),
    ) {
        PollDecision::Denied => status_json("access_denied"),
        PollDecision::Gone => status_json("expired_token"),
        PollDecision::Expired => {
            expire_once(&state, &row).await;
            status_json("expired_token")
        }
        PollDecision::SlowDown { interval } => {
            if let Err(e) = state.device_pairings.note_poll(id, now, interval).await {
                debug!(pairing = %id, error = %e, "pairing: could not note the poll");
            }
            Audit::new("refuse")
                .refused("slow_down")
                .pairing(&row)
                .ip(ip)
                .record(&state)
                .await;
            Json(json!({ "status": "slow_down", "interval": interval })).into_response()
        }
        PollDecision::Pending => {
            if let Err(e) = state
                .device_pairings
                .note_poll(id, now, row.poll_interval_secs)
                .await
            {
                debug!(pairing = %id, error = %e, "pairing: could not note the poll");
            }
            status_json("authorization_pending")
        }
        PollDecision::Deliver => {
            if let Err(e) = state
                .device_pairings
                .note_poll(id, now, row.poll_interval_secs)
                .await
            {
                debug!(pairing = %id, error = %e, "pairing: could not note the poll");
            }
            deliver(&state, row, id, now, ip).await
        }
    }
}

/// The delivery: the gate re-checked, the CAS won, the tokens minted — in
/// that order, so a loser of the race mints nothing and a demoted approver
/// is refused before anything is minted.
async fn deliver(
    state: &FleetState,
    row: DevicePairing,
    id: ObjectId,
    now: DateTime,
    ip: Option<IpAddr>,
) -> Response {
    let (Some(approver), Some(tenant_id), Some(role)) = (
        row.approver_user_id,
        row.tenant_id,
        PairingRole::parse(&row.role),
    ) else {
        warn!(pairing = %id, "pairing: approved row without an approver, an org or a role");
        return ApiError::Internal("pairing is approved but incomplete".to_string())
            .into_response();
    };
    // The same gate as the approval, asked again now: an admin demoted in
    // between gets nothing, and the pairing is closed as denied.
    match gate_holds(state, role, tenant_id, approver).await {
        Ok(Ok(())) => {}
        Ok(Err((reason, _))) => {
            match state.device_pairings.deny_at_delivery(id, now).await {
                Ok(true) => {
                    info!(pairing = %id, approver = %approver, reason, "pairing: the approver's gate was gone at delivery");
                    Audit::new("refuse")
                        .refused("gate_lost")
                        .pairing(&row)
                        .user(approver)
                        .tenant(tenant_id)
                        .ip(ip)
                        .record(state)
                        .await;
                }
                Ok(false) => {}
                Err(e) => return ApiError::from(e).into_response(),
            }
            return status_json("access_denied");
        }
        Err(e) => return e.into_response(),
    }
    // Winning this CAS is the right to mint. A loser — a second poll in
    // flight, or one after a delivery — gets the same answer a swept row
    // gets: delivery is once.
    match state.device_pairings.deliver(id, now).await {
        Ok(true) => {}
        Ok(false) => {
            Audit::new("refuse")
                .refused("already_delivered")
                .pairing(&row)
                .ip(ip)
                .record(state)
                .await;
            return status_json("expired_token");
        }
        Err(e) => return ApiError::from(e).into_response(),
    }
    let mut tokens = Vec::with_capacity(usize::from(row.halves));
    for _ in 0..row.halves.max(1) {
        match role {
            PairingRole::Daemon => {
                // The ordinary single-use token, by the admin route's own
                // call, never anchoring (operator decision 4), on the
                // FR-102 ledger exactly as the admin route records it.
                let (token, jti) = match state.auth.issue_enrollment_token_with(
                    approver,
                    tenant_id,
                    ENROLLMENT_TTL_SECS,
                    false,
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(pairing = %id, error = %e, "pairing: could not mint the enrollment token");
                        return ApiError::from(e).into_response();
                    }
                };
                if let Err(e) = state
                    .enroll_tokens
                    .record_issued(&jti, tenant_id, approver, now)
                    .await
                {
                    warn!(tenant = %tenant_id, %jti, error = %e, "fr-102: could not record the issued enrollment token");
                }
                tokens.push(token);
            }
            PairingRole::Tunnel => {
                let (token, _jti) = match state.auth.issue_tunnel_enrollment_token(
                    approver,
                    tenant_id,
                    TUNNEL_ENROLLMENT_TTL_SECS,
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(pairing = %id, error = %e, "pairing: could not mint the tunnel enrollment token");
                        return ApiError::from(e).into_response();
                    }
                };
                tokens.push(token);
            }
        }
    }
    let org_name = state
        .tenants
        .base
        .find_by_id(tenant_id)
        .await
        .map(|t| t.name)
        .unwrap_or_default();
    let approver_name = state
        .users
        .base
        .find_by_id(approver)
        .await
        .map(|u| u.display_name)
        .unwrap_or_default();
    info!(pairing = %id, tenant = %tenant_id, approver = %approver, tokens = tokens.len(), "pairing: delivered");
    Audit::new("deliver")
        .pairing(&row)
        .user(approver)
        .tenant(tenant_id)
        .ip(ip)
        .record(state)
        .await;
    Json(json!({
        "status": "approved",
        "tokens": tokens,
        "org_name": org_name,
        "approver_name": approver_name,
    }))
    .into_response()
}

/// `POST /api/pair/lookup` — the signed-in person asks what a typed code
/// names, before approving it. 404 for anything but a live pending
/// pairing, and each 404 is a miss against the account.
pub async fn lookup(
    State(state): State<FleetState>,
    auth: OptionalAuthUser,
    headers: HeaderMap,
    peer: PeerAddr,
    body: Bytes,
) -> Response {
    if let Some(r) = disabled(&state) {
        return r;
    }
    let auth = match require_auth(auth) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let ip = client_ip(&state, &headers, peer);
    let req: CodeRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    if account_locked(&state, auth.user_id) {
        Audit::new("lookup")
            .refused("account_locked")
            .user(auth.user_id)
            .ip(ip)
            .record(&state)
            .await;
        return locked_response();
    }
    let now = DateTime::now();
    let row = match live_pending(&state, &req.user_code, now).await {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let Some(row) = row else {
        state.pairing_miss_limiter.record(auth.user_id);
        Audit::new("lookup")
            .refused("not_pending")
            .user(auth.user_id)
            .ip(ip)
            .record(&state)
            .await;
        return ApiError::NotFound("No pending pairing for that code".to_string()).into_response();
    };
    Audit::new("lookup")
        .pairing(&row)
        .user(auth.user_id)
        .ip(ip)
        .record(&state)
        .await;
    Json(LookupResponse {
        machine_name: row.machine_name,
        os: row.os,
        arch: row.arch,
        role: row.role,
        halves: row.halves,
        client: row.client,
        client_version: row.client_version,
        country: row.country,
        started_at: rfc3339(row.created_at),
        expires_at: rfc3339(row.expires_at),
    })
    .into_response()
}

/// `POST /api/pair/approve` — the decision, recorded; no token yet. The
/// role's gate in `tenant_id` (daemon: `MANAGE_AGENTS`; tunnel: a member)
/// is checked here and again at delivery.
pub async fn approve(
    State(state): State<FleetState>,
    auth: OptionalAuthUser,
    headers: HeaderMap,
    peer: PeerAddr,
    body: Bytes,
) -> Response {
    if let Some(r) = disabled(&state) {
        return r;
    }
    let auth = match require_auth(auth) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let ip = client_ip(&state, &headers, peer);
    let req: ApproveRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let Ok(tenant_id) = ObjectId::parse_str(&req.tenant_id) else {
        return ApiError::BadRequest("Invalid tenant_id".to_string()).into_response();
    };
    if account_locked(&state, auth.user_id) {
        Audit::new("approve")
            .refused("account_locked")
            .user(auth.user_id)
            .tenant(tenant_id)
            .ip(ip)
            .record(&state)
            .await;
        return locked_response();
    }
    let now = DateTime::now();
    let row = match live_pending(&state, &req.user_code, now).await {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let Some(row) = row else {
        state.pairing_miss_limiter.record(auth.user_id);
        Audit::new("approve")
            .refused("not_pending")
            .user(auth.user_id)
            .tenant(tenant_id)
            .ip(ip)
            .record(&state)
            .await;
        return ApiError::NotFound("No pending pairing for that code".to_string()).into_response();
    };
    let Some(id) = row.id else {
        return ApiError::Internal("pairing row has no id".to_string()).into_response();
    };
    let Some(role) = PairingRole::parse(&row.role) else {
        return ApiError::Internal("pairing row has an unknown role".to_string()).into_response();
    };
    if role == PairingRole::Tunnel && !state.mounted.contains(NETWORK_MODULE) {
        Audit::new("approve")
            .refused("network_unmounted")
            .pairing(&row)
            .user(auth.user_id)
            .tenant(tenant_id)
            .ip(ip)
            .record(&state)
            .await;
        return refuse(
            StatusCode::BAD_REQUEST,
            "network_unmounted",
            "This server does not serve tunnel clients",
        );
    }
    match gate_holds(&state, role, tenant_id, auth.user_id).await {
        Ok(Ok(())) => {}
        Ok(Err((reason, err))) => {
            // A refused approval counts against the account like a miss:
            // an org you cannot add devices to is as much a probe as a
            // code that names nothing.
            state.pairing_miss_limiter.record(auth.user_id);
            Audit::new("approve")
                .refused(reason)
                .pairing(&row)
                .user(auth.user_id)
                .tenant(tenant_id)
                .ip(ip)
                .record(&state)
                .await;
            return err.into_response();
        }
        Err(e) => return e.into_response(),
    }
    match state
        .device_pairings
        .approve(id, auth.user_id, tenant_id, now)
        .await
    {
        Ok(true) => {}
        // Lost a race with another decision or the clock: not a guess, so
        // not a miss, but the person is told as if the code names nothing.
        Ok(false) => {
            Audit::new("approve")
                .refused("not_pending")
                .pairing(&row)
                .user(auth.user_id)
                .tenant(tenant_id)
                .ip(ip)
                .record(&state)
                .await;
            return ApiError::NotFound("No pending pairing for that code".to_string())
                .into_response();
        }
        Err(e) => return ApiError::from(e).into_response(),
    }
    info!(pairing = %id, tenant = %tenant_id, approver = %auth.user_id, role = row.role, "pairing: approved");
    Audit::new("approve")
        .pairing(&row)
        .user(auth.user_id)
        .tenant(tenant_id)
        .ip(ip)
        .record(&state)
        .await;
    // The org's other admins are told (operator decision 3): spawned,
    // best-effort, never gating the answer.
    crate::pairing_notice::spawn_paired_notice(
        state.clone(),
        tenant_id,
        auth.user_id,
        row.machine_name.clone(),
        row.os.clone(),
        row.country.clone(),
    );
    Json(json!({ "approved": true })).into_response()
}

/// `POST /api/pair/deny` — the signed-in person closes a pairing they do
/// not recognise. 404 for anything but a live pending pairing, counted as
/// a miss.
pub async fn deny(
    State(state): State<FleetState>,
    auth: OptionalAuthUser,
    headers: HeaderMap,
    peer: PeerAddr,
    body: Bytes,
) -> Response {
    if let Some(r) = disabled(&state) {
        return r;
    }
    let auth = match require_auth(auth) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let ip = client_ip(&state, &headers, peer);
    let req: CodeRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    if account_locked(&state, auth.user_id) {
        Audit::new("deny")
            .refused("account_locked")
            .user(auth.user_id)
            .ip(ip)
            .record(&state)
            .await;
        return locked_response();
    }
    let now = DateTime::now();
    let row = match live_pending(&state, &req.user_code, now).await {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let Some(row) = row else {
        state.pairing_miss_limiter.record(auth.user_id);
        Audit::new("deny")
            .refused("not_pending")
            .user(auth.user_id)
            .ip(ip)
            .record(&state)
            .await;
        return ApiError::NotFound("No pending pairing for that code".to_string()).into_response();
    };
    let Some(id) = row.id else {
        return ApiError::Internal("pairing row has no id".to_string()).into_response();
    };
    match state.device_pairings.deny(id, auth.user_id, now).await {
        Ok(true) => {}
        Ok(false) => {
            Audit::new("deny")
                .refused("not_pending")
                .pairing(&row)
                .user(auth.user_id)
                .ip(ip)
                .record(&state)
                .await;
            return ApiError::NotFound("No pending pairing for that code".to_string())
                .into_response();
        }
        Err(e) => return ApiError::from(e).into_response(),
    }
    info!(pairing = %id, by = %auth.user_id, "pairing: denied");
    Audit::new("deny")
        .pairing(&row)
        .user(auth.user_id)
        .ip(ip)
        .record(&state)
        .await;
    Json(json!({ "denied": true })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_code_is_eight_unambiguous_consonants_shown_in_two_groups() {
        for _ in 0..200 {
            let code = generate_user_code();
            assert_eq!(code.len(), USER_CODE_LEN);
            assert!(
                code.bytes().all(|b| USER_CODE_ALPHABET.contains(&b)),
                "{code} strays from the alphabet"
            );
            let shown = format_user_code(&code);
            assert_eq!(shown.len(), 9);
            assert_eq!(shown.as_bytes()[4], b'-');
            assert_eq!(normalize_user_code(&shown).as_deref(), Some(code.as_str()));
        }
        // No vowel, no digit, no look-alike letter can ever appear.
        for forbidden in b"AEIOUY0123456789" {
            assert!(!USER_CODE_ALPHABET.contains(forbidden));
        }
    }

    #[test]
    fn typed_codes_are_normalized_and_anything_else_names_nothing() {
        assert_eq!(
            normalize_user_code("bcdf-ghjk").as_deref(),
            Some("BCDFGHJK")
        );
        assert_eq!(
            normalize_user_code(" BCDF GHJK ").as_deref(),
            Some("BCDFGHJK")
        );
        assert_eq!(normalize_user_code("BCDFGHJK").as_deref(), Some("BCDFGHJK"));
        assert_eq!(
            normalize_user_code("bc-df-gh-jk").as_deref(),
            Some("BCDFGHJK")
        );
        // Too short, too long, a vowel, a digit, a non-letter: none is a code.
        assert_eq!(normalize_user_code("BCDFGHJ"), None);
        assert_eq!(normalize_user_code("BCDFGHJKL"), None);
        assert_eq!(normalize_user_code("BCDFGHJA"), None);
        assert_eq!(normalize_user_code("BCDF-GHJ1"), None);
        assert_eq!(normalize_user_code("BCDF_GHJK"), None);
        assert_eq!(normalize_user_code(""), None);
        assert_eq!(normalize_user_code("----"), None);
    }

    #[test]
    fn device_code_is_url_safe_and_only_its_hash_is_ever_kept() {
        let a = generate_device_code();
        let b = generate_device_code();
        assert_eq!(a.len(), 43, "32 bytes, base64url, no padding");
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_ne!(a, b);
        let ha = hash_device_code(&a);
        assert_eq!(ha.len(), 64);
        assert_eq!(ha, hash_device_code(&a), "stable");
        assert_ne!(ha, hash_device_code(&b));
        assert!(!ha.contains(&a), "the hash does not carry the code");
    }

    #[test]
    fn fields_are_clamped_never_refused() {
        assert_eq!(clamp_field("  laptop  "), "laptop");
        assert_eq!(clamp_field(""), "");
        let long = "x".repeat(200);
        assert_eq!(clamp_field(&long).chars().count(), FIELD_MAX_CHARS);
        // Multi-byte text is cut at a character, never inside one.
        let wide = "é".repeat(100);
        assert_eq!(clamp_field(&wide).chars().count(), FIELD_MAX_CHARS);
        assert_eq!(clamp_client("wizard"), "wizard");
        assert_eq!(clamp_client(" Wizard "), "wizard");
        assert_eq!(clamp_client("script-sh"), "script-sh");
        assert_eq!(clamp_client("something-new"), "other");
        assert_eq!(clamp_client(""), "other");
    }

    fn start(os: OsKind, role: PairingRole, halves: u8) -> StartRequest {
        StartRequest {
            machine_name: "m".into(),
            os,
            arch: Arch::X86_64,
            role,
            halves,
            client: String::new(),
            client_version: String::new(),
        }
    }

    #[test]
    fn two_halves_are_only_a_macos_daemon() {
        assert!(validate_start(&start(OsKind::Macos, PairingRole::Daemon, 2)).is_ok());
        assert!(validate_start(&start(OsKind::Linux, PairingRole::Daemon, 1)).is_ok());
        assert!(validate_start(&start(OsKind::Macos, PairingRole::Tunnel, 2)).is_err());
        assert!(validate_start(&start(OsKind::Windows, PairingRole::Daemon, 2)).is_err());
        assert!(validate_start(&start(OsKind::Linux, PairingRole::Daemon, 0)).is_err());
        assert!(validate_start(&start(OsKind::Linux, PairingRole::Daemon, 3)).is_err());
    }

    #[test]
    fn os_wire_is_os_kinds_serde_spelling() {
        for os in [OsKind::Linux, OsKind::Macos, OsKind::Windows] {
            let via_serde = serde_json::to_value(os).unwrap();
            assert_eq!(via_serde.as_str(), Some(os_wire(os)));
        }
    }

    #[test]
    fn the_wire_spellings_round_trip() {
        let r: StartRequest = serde_json::from_str(
            r#"{"machine_name":"m","os":"macos","arch":"aarch64","role":"daemon","halves":2}"#,
        )
        .unwrap();
        assert_eq!(r.os, OsKind::Macos);
        assert_eq!(r.arch, Arch::Aarch64);
        assert_eq!(r.role, PairingRole::Daemon);
        assert_eq!(r.halves, 2);
        let r: StartRequest =
            serde_json::from_str(r#"{"os":"linux","arch":"x86_64","role":"tunnel"}"#).unwrap();
        assert_eq!(r.halves, 1, "halves defaults to one");
        assert_eq!(r.machine_name, "");
        assert!(
            serde_json::from_str::<StartRequest>(
                r#"{"os":"beos","arch":"x86_64","role":"daemon"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<StartRequest>(r#"{"os":"linux","arch":"mips","role":"daemon"}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<StartRequest>(
                r#"{"os":"linux","arch":"x86_64","role":"agent"}"#
            )
            .is_err()
        );
        assert_eq!(PairingRole::parse("daemon"), Some(PairingRole::Daemon));
        assert_eq!(PairingRole::parse("tunnel"), Some(PairingRole::Tunnel));
        assert_eq!(PairingRole::parse("ssh"), None);
    }

    const T0: i64 = 1_700_000_000_000;

    #[test]
    fn a_poll_is_decided_by_the_row_and_the_clock() {
        let live = T0 + 60_000;
        // A fresh pending row, never polled: pending.
        assert_eq!(
            decide_poll(PairingStatus::Pending, live, None, 5, T0),
            PollDecision::Pending
        );
        // Polled 2 s ago with a 5 s interval: slow down, interval + 5.
        assert_eq!(
            decide_poll(PairingStatus::Pending, live, Some(T0 - 2_000), 5, T0),
            PollDecision::SlowDown { interval: 10 }
        );
        // Polled exactly an interval ago: allowed.
        assert_eq!(
            decide_poll(PairingStatus::Pending, live, Some(T0 - 5_000), 5, T0),
            PollDecision::Pending
        );
        // An interval of 0 never slows down.
        assert_eq!(
            decide_poll(PairingStatus::Pending, live, Some(T0), 0, T0),
            PollDecision::Pending
        );
        // The interval caps.
        assert_eq!(
            decide_poll(PairingStatus::Approved, live, Some(T0), 58, T0),
            PollDecision::SlowDown { interval: 60 }
        );
        // Approved and allowed: deliver.
        assert_eq!(
            decide_poll(PairingStatus::Approved, live, Some(T0 - 9_000), 5, T0),
            PollDecision::Deliver
        );
        // Past expiry, whatever was polled when: expired (and the status
        // wins over the clock for the closed states).
        assert_eq!(
            decide_poll(PairingStatus::Pending, T0, Some(T0 - 1), 5, T0),
            PollDecision::Expired
        );
        assert_eq!(
            decide_poll(PairingStatus::Approved, T0 - 1, None, 5, T0),
            PollDecision::Expired
        );
        assert_eq!(
            decide_poll(PairingStatus::Denied, T0 - 1, None, 5, T0),
            PollDecision::Denied
        );
        assert_eq!(
            decide_poll(PairingStatus::Delivered, live, None, 5, T0),
            PollDecision::Gone
        );
        assert_eq!(
            decide_poll(PairingStatus::Expired, live, None, 5, T0),
            PollDecision::Gone
        );
    }
}
