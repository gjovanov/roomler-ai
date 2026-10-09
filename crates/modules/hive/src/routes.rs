// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The session routes: start, list, one, stop.
//!
//! # The start's gates, in order
//!
//! 1. membership — [`ApiError::NotAMember`]: the answer names devices, so a
//!    stranger must not reach the device lookup;
//! 2. the device, LIVE in this org — 404 otherwise, the answer a bogus or
//!    foreign id gets;
//! 3. the org is not archived — `org_archived`;
//! 4. `HIVE_RUN` — `no_permission`;
//! 5. the device is connected here and advertises `hive` —
//!    `device_offline` / `device_unsupported`;
//! 6. the per-(user, device) ceiling — `rate_limited`, LAST, so a refusal is
//!    attributable to an identity that passed the others and only a start
//!    that would reach a device spends a token.
//!
//! From 3 on, a refusal is a 200 carrying the reason, and is audited: the exec
//! convention — a policy refusal is a result, not a transport error, and the
//! `hive_audit` row is what remains when someone probes which devices will
//! run things for them. The DEVICE then applies its own gates —
//! `hive_enabled`, `hive_accounts`, `hive_roots` — and answers in
//! `rc:hive.start_ack`, which lands on the session record.
//!
//! A session is visible to its room's members — the owner, and whoever the
//! owner added (P1c, [`crate::participants`]) — and stoppable by its owner
//! alone; anyone else's read of its id is a 404. Stopping needs no
//! permission: ending one's own session must never be the thing a role
//! change blocks.

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::{
    error::Error as HubError,
    hive::{HARNESS_CLAUDE_CODE, hive_limits},
    signaling::ServerMsg,
};
use roomler_ai_services::dao::base::PaginationParams;
use roomler_core::{ApiError, extractors::auth::AuthUser, guards::parse_tid};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::model::{
    AgentSession, HarnessRef, HiveAuditEvent, SessionLocation, SessionOrigin, SessionStatus,
    SessionView,
};
use crate::{HiveState, room};

/// `POST …/hive/session`.
#[derive(Debug, Deserialize)]
pub struct StartBody {
    pub device_id: String,
    /// Where the session runs, as typed. The device resolves it.
    pub folder: String,
    /// Defaults to the folder's last component.
    #[serde(default)]
    pub title: Option<String>,
}

/// Why the SERVER refused a start. The device's own refusals are
/// [`roomler_ai_remote_control::hive::HiveRefusal`] words, on the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartDenyReason {
    OrgArchived,
    NoPermission,
    DeviceOffline,
    DeviceUnsupported,
    RateLimited,
}

impl StartDenyReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OrgArchived => "org_archived",
            Self::NoPermission => "no_permission",
            Self::DeviceOffline => "device_offline",
            Self::DeviceUnsupported => "device_unsupported",
            Self::RateLimited => "rate_limited",
        }
    }

    /// What the person is told — each names its own next step.
    pub const fn message(self) -> &'static str {
        match self {
            Self::OrgArchived => "this organization is archived; new sessions are blocked",
            Self::NoPermission => {
                "your role does not include \"Run agent sessions\" (HIVE_RUN); \
                 an owner can grant it"
            }
            Self::DeviceOffline => "the device is offline",
            Self::DeviceUnsupported => {
                "the device's agent does not run agent sessions — update it, \
                 or it was built without them"
            }
            Self::RateLimited => "too many sessions started on this device in the last minute",
        }
    }
}

/// What the device's refusal words mean to the person who started it.
pub(crate) fn device_refusal_message(word: &str) -> &'static str {
    match word {
        "hive_disabled" => "the device does not allow agent sessions (its hive_enabled is off)",
        "no_account" => "the device maps no local account to you (hive_accounts)",
        "no_console_user" => "nobody is signed in at the device, so there is no account to run as",
        "folder_not_allowed" => "the folder is outside the device's hive_roots",
        "harness_missing" => "the agent harness is not installed on the device",
        "launch_failed" => "the device could not start the agent harness",
        "at_capacity" => "the device is already running as many sessions as it allows",
        _ => "the device refused the session",
    }
}

#[derive(Debug, Serialize)]
pub struct StartResponse {
    /// `accepted` (the device is launching it), `refused` (by the server or
    /// the device — `reason` says which gate), or `pending` (the device has
    /// not answered yet; the session record updates when it does).
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Absent only when the server refused before creating a session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionView>,
}

impl StartResponse {
    fn refused_by_server(reason: StartDenyReason, session: Option<&AgentSession>) -> Self {
        Self {
            outcome: "refused",
            reason: Some(reason.as_str().to_string()),
            message: Some(reason.message().to_string()),
            session: session.map(SessionView::from),
        }
    }

    /// Read off the record — the one place the device's answer lands.
    fn from_record(s: &AgentSession) -> Self {
        if s.status == SessionStatus::Refused {
            let word = s.refusal.clone().unwrap_or_else(|| "other".into());
            return Self {
                outcome: "refused",
                message: Some(device_refusal_message(&word).to_string()),
                reason: Some(word),
                session: Some(SessionView::from(s)),
            };
        }
        // Accepted only on evidence: the device's answer, or a run state it
        // could only report for a session it launched. A session stopped (by
        // another tab) or lost before either is still unanswered.
        let answered = s.accepted_at.is_some()
            || s.status == SessionStatus::Ended
            || SessionStatus::LAUNCHED.contains(&s.status);
        Self {
            outcome: if answered { "accepted" } else { "pending" },
            reason: None,
            message: (!answered).then(|| {
                "the device has not answered yet; the session updates when it does".to_string()
            }),
            session: Some(SessionView::from(s)),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<SessionView>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

#[derive(Debug, Serialize)]
pub struct StopResponse {
    /// `stopping` (the device was told), `queued` (it is not connected here
    /// and is told when it connects), or `ended` (it already was, or its
    /// device can no longer be running it).
    pub outcome: &'static str,
    pub session: SessionView,
}

/// One server decision, for `hive_audit` — the routes' and the viewer
/// grants' (`crate::view`).
pub(crate) struct Audit {
    pub(crate) tenant_id: ObjectId,
    pub(crate) user_id: ObjectId,
    /// Whom it was about, when not the actor (P1c's participant changes).
    pub(crate) target_id: Option<ObjectId>,
    pub(crate) device_id: ObjectId,
    pub(crate) session_id: Option<ObjectId>,
    pub(crate) action: &'static str,
    pub(crate) outcome: &'static str,
    pub(crate) reason: Option<&'static str>,
}

impl Audit {
    pub(crate) async fn write(self, state: &HiveState) {
        let ev = HiveAuditEvent {
            id: None,
            tenant_id: self.tenant_id,
            user_id: self.user_id,
            target_id: self.target_id,
            device_id: Some(self.device_id),
            session_id: self.session_id,
            action: self.action.to_string(),
            outcome: self.outcome.to_string(),
            reason: self.reason.map(str::to_string),
            at: DateTime::now(),
        };
        if let Err(e) = state.audit.record(ev).await {
            tracing::warn!(%e, "hive: audit write failed");
        }
    }
}

/// P1g — what a member of an organization agent sessions do not serve is
/// told, on every route of the module.
pub(crate) const NOT_SERVED: &str = "agent sessions are not available to this organization";

/// The organization a route names, once the caller is shown to be in it and
/// agent sessions serve it (P1g). Membership first: someone outside the org
/// learns nothing about whether the pillar is open to it.
pub(crate) async fn member_tenant(
    state: &HiveState,
    tenant_id: &str,
    auth: &AuthUser,
) -> Result<ObjectId, ApiError> {
    let tid = parse_tid(tenant_id)?;
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::NotAMember);
    }
    if !state.scope.serves(tid) {
        return Err(ApiError::NotFound(NOT_SERVED.into()));
    }
    Ok(tid)
}

/// P1g — `GET /api/tenant/{tenant_id}/hive`: whether agent sessions serve
/// this organization. The SPA asks before it shows any of the module's pages;
/// an organization they do not serve is answered `404`, as every other route
/// here answers it.
pub async fn serves(
    State(state): State<HiveState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    member_tenant(&state, &tenant_id, &auth).await?;
    Ok(Json(serde_json::json!({ "enabled": true })))
}

pub(crate) fn parse_oid(raw: &str, what: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(raw).map_err(|_| ApiError::BadRequest(format!("Invalid {what}")))
}

/// The folder as typed, minus surrounding whitespace. The server does not
/// interpret it — the device resolves it as the mapped account — but refuses
/// what no filesystem path holds and what would forge lines in a log.
pub(crate) fn clean_folder(raw: &str) -> Result<String, ApiError> {
    let f = raw.trim();
    if f.is_empty() {
        return Err(ApiError::BadRequest("folder must not be empty".into()));
    }
    if f.len() > hive_limits::MAX_FOLDER_LEN {
        return Err(ApiError::BadRequest(format!(
            "folder is longer than {} bytes",
            hive_limits::MAX_FOLDER_LEN
        )));
    }
    if f.chars().any(char::is_control) {
        return Err(ApiError::BadRequest(
            "folder contains control characters".into(),
        ));
    }
    Ok(f.to_string())
}

/// The title, or the folder's last component; at most `MAX_TITLE_LEN`
/// characters either way (a default is cut, a typed one refused).
pub(crate) fn clean_title(raw: Option<&str>, folder: &str) -> Result<String, ApiError> {
    if let Some(t) = raw.map(str::trim).filter(|t| !t.is_empty()) {
        if t.chars().count() > hive_limits::MAX_TITLE_LEN {
            return Err(ApiError::BadRequest(format!(
                "title is longer than {} characters",
                hive_limits::MAX_TITLE_LEN
            )));
        }
        if t.chars().any(char::is_control) {
            return Err(ApiError::BadRequest(
                "title contains control characters".into(),
            ));
        }
        return Ok(t.to_string());
    }
    let last = folder
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(folder);
    Ok(last.chars().take(hive_limits::MAX_TITLE_LEN).collect())
}

/// Gates 3–6. The outer `Result` is the infrastructure (a database that did
/// not answer is a 500, never a policy verdict); the inner one is the policy.
async fn authorize(
    state: &HiveState,
    tenant_id: ObjectId,
    device_id: ObjectId,
    user_id: ObjectId,
) -> Result<Result<(), StartDenyReason>, ApiError> {
    let tenant = state.tenants.base.find_by_id(tenant_id).await?;
    if tenant.is_archived {
        return Ok(Err(StartDenyReason::OrgArchived));
    }
    let perms = state
        .tenants
        .get_member_permissions(tenant_id, user_id)
        .await?;
    if !permissions::has(perms, permissions::HIVE_RUN) {
        return Ok(Err(StartDenyReason::NoPermission));
    }
    match state.fleet.rc_hub.agent_supports_hive(device_id) {
        None => return Ok(Err(StartDenyReason::DeviceOffline)),
        Some(false) => return Ok(Err(StartDenyReason::DeviceUnsupported)),
        Some(true) => {}
    }
    if !state
        .start_limiter
        .check(user_id, device_id, hive_limits::START_RATE_PER_MINUTE)
    {
        return Ok(Err(StartDenyReason::RateLimited));
    }
    Ok(Ok(()))
}

/// `POST /api/tenant/{tenant_id}/hive/session` — start a session on a device.
pub async fn start(
    State(state): State<HiveState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<StartBody>,
) -> Result<Json<StartResponse>, ApiError> {
    let tid = member_tenant(&state, &tenant_id, &auth).await?;
    let folder = clean_folder(&body.folder)?;
    let title = clean_title(body.title.as_deref(), &folder)?;
    let device_id = parse_oid(&body.device_id, "device_id")?;
    // LIVE: a removed device runs nothing, and its id answers like a bogus one.
    let device = state
        .fleet
        .agents
        .find_live_in_tenant(tid, device_id)
        .await?;

    let user_id = auth.user_id;
    let audit =
        |session_id: Option<ObjectId>, outcome: &'static str, reason: Option<&'static str>| Audit {
            tenant_id: tid,
            user_id,
            target_id: None,
            device_id,
            session_id,
            action: "start",
            outcome,
            reason,
        };

    if let Err(reason) = authorize(&state, tid, device_id, auth.user_id).await? {
        info!(
            tenant = %tid, device = %device_id, user = %auth.user_id,
            reason = reason.as_str(), "hive: start refused"
        );
        audit(None, "refused", Some(reason.as_str()))
            .write(&state)
            .await;
        return Ok(Json(StartResponse::refused_by_server(reason, None)));
    }

    // The starter as the device will see them: the PROVEN address (or the
    // `.invalid` placeholder `users.email` holds otherwise) and a display name.
    let user = state.users.base.find_by_id(auth.user_id).await?;
    let now = DateTime::now();
    let sid = ObjectId::new();
    // P1e — core memory, rendered now and frozen with the session: a fact
    // written later reaches the next session, never this one. Only for a
    // device that understands it; one that cannot be rendered is logged and
    // the session runs without it — memory never blocks a start.
    let memory = if state.fleet.rc_hub.agent_supports_hive_memory(device_id) == Some(true) {
        match state
            .brain
            .snapshot(
                tid,
                sid,
                (auth.user_id, &user.display_name),
                (device_id, &device.name),
            )
            .await
        {
            Ok(m) => Some(m),
            Err(e) => {
                warn!(session = %sid, %e, "hive: core memory not rendered — the session starts without it");
                None
            }
        }
    } else {
        None
    };
    // The session's room first (P0d): the record names it, and a session
    // nobody could see is no session.
    let room_id = room::open(&state, tid, sid, &title, auth.user_id).await?;
    let session = AgentSession {
        id: Some(sid),
        tenant_id: tid,
        owner_id: auth.user_id,
        drivers: Vec::new(),
        title,
        room_id: Some(room_id),
        last_turn: None,
        harness: HarnessRef {
            id: HARNESS_CLAUDE_CODE.to_string(),
            session: uuid::Uuid::new_v4().to_string(),
        },
        location: SessionLocation {
            device_id,
            device_name: device.name.clone(),
            folder: folder.clone(),
            account: None,
        },
        status: SessionStatus::Starting,
        fence: 1,
        accepted_at: None,
        refusal: None,
        end_reason: None,
        detail: None,
        created_at: now,
        updated_at: now,
        ended_at: None,
        brain_rev: memory.as_ref().map(|m| m.brain_rev),
        origin: SessionOrigin::Started,
    };
    // The record BEFORE the push: the device's answer, however fast, must
    // find the session it is about.
    state.sessions.create(&session).await?;
    // P1e — the snapshot, kept for a re-send of the start, then sent ahead
    // of it on the same socket, so the device holds it when the launch comes.
    if let Some(m) = &memory {
        if let Err(e) = state.brain.store_snapshot(m).await {
            warn!(session = %sid, %e, "hive: core memory not stored — a re-sent start goes without it");
        }
        let frame = memory_frame(m, 1);
        if let Err(e) = state.fleet.rc_hub.push_hive_memory(device_id, tid, frame) {
            debug!(session = %sid, %e, "hive: core memory not sent");
        }
    }

    // And the waiter before the push, for the same reason.
    let pending = state.start_acks.expect(sid, device_id);
    let msg = ServerMsg::HiveStart {
        session_id: sid,
        harness: session.harness.id.clone(),
        harness_session: session.harness.session.clone(),
        fence: 1,
        folder,
        user_id: auth.user_id,
        user_email: user.email,
        caller: user.display_name,
        resume: false,
    };
    if let Err(e) = state.fleet.rc_hub.push_hive(device_id, tid, msg) {
        // The device left — or reconnected as a build without Hive — between
        // the gate and the push. Nothing reached it, and nothing will: the
        // start is not re-sent from here.
        let reason = match e {
            HubError::ExecUnsupported(_) => StartDenyReason::DeviceUnsupported,
            _ => StartDenyReason::DeviceOffline,
        };
        let lost = state
            .sessions
            .mark_lost(sid, "the device was gone before the start reached it")
            .await?;
        audit(Some(sid), "refused", Some(reason.as_str()))
            .write(&state)
            .await;
        let s = state.sessions.find(sid).await?;
        if lost && let Some(s) = &s {
            room::note(&state, s, room::ended_note(s)).await;
        }
        return Ok(Json(StartResponse::refused_by_server(reason, s.as_ref())));
    }
    audit(Some(sid), "sent", None).write(&state).await;
    info!(
        tenant = %tid, device = %device_id, user = %auth.user_id, session = %sid,
        "hive: start sent"
    );

    pending
        .wait(Duration::from_secs(hive_limits::START_ACK_TIMEOUT_SECS))
        .await;
    let s = state
        .sessions
        .find(sid)
        .await?
        .ok_or_else(|| ApiError::Internal("the session record vanished".into()))?;
    Ok(Json(StartResponse::from_record(&s)))
}

/// P1e — `rc:hive.memory` for a session's stored snapshot, at `fence`.
pub(crate) fn memory_frame(m: &crate::brain::SessionMemory, fence: u64) -> ServerMsg {
    ServerMsg::HiveMemory {
        session_id: m.session_id,
        fence,
        brain_rev: u64::try_from(m.brain_rev).unwrap_or_default(),
        claude_md: m.claude_md.clone(),
        memory_md: m.memory_md.clone(),
    }
}

/// `GET /api/tenant/{tenant_id}/hive/session` — the caller's sessions,
/// newest first.
pub async fn list(
    State(state): State<HiveState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<ListResponse>, ApiError> {
    let tid = member_tenant(&state, &tenant_id, &auth).await?;
    let page = state
        .sessions
        .list_owned(tid, auth.user_id, &params)
        .await?;
    Ok(Json(ListResponse {
        items: page.items.iter().map(SessionView::from).collect(),
        total: page.total,
        page: page.page,
        per_page: page.per_page,
        total_pages: page.total_pages,
    }))
}

/// `GET /api/tenant/{tenant_id}/hive/session/{session_id}` — for anyone who
/// may read it (P1c: its room's members, not only its owner).
pub async fn get_one(
    State(state): State<HiveState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
) -> Result<Json<SessionView>, ApiError> {
    let tid = member_tenant(&state, &tenant_id, &auth).await?;
    let sid = parse_oid(&session_id, "session_id")?;
    let s = crate::participants::readable(&state, tid, sid, auth.user_id).await?;
    Ok(Json(SessionView::from(&s)))
}

/// `POST /api/tenant/{tenant_id}/hive/session/{session_id}/stop`.
///
/// Idempotent: a session already stopping is told again (the device answers
/// a stop for a session it does not run with `ended`), and one already over
/// is answered `ended` without touching anything.
pub async fn stop(
    State(state): State<HiveState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
) -> Result<Json<StopResponse>, ApiError> {
    let tid = member_tenant(&state, &tenant_id, &auth).await?;
    let sid = parse_oid(&session_id, "session_id")?;
    let s = state.sessions.find_owned(tid, auth.user_id, sid).await?;
    if s.status.is_terminal() {
        return Ok(Json(StopResponse {
            outcome: "ended",
            session: SessionView::from(&s),
        }));
    }
    if s.status != SessionStatus::Stopping {
        // `false` = it ended meanwhile; the stop below is then a harmless
        // no-op on the device, and the re-read reports what happened.
        state.sessions.mark_stopping(tid, auth.user_id, sid).await?;
    }

    let device_id = s.location.device_id;
    let msg = ServerMsg::HiveStop {
        session_id: sid,
        fence: u64::try_from(s.fence).unwrap_or_default(),
        reason: "owner".to_string(),
    };
    let sent = match state.fleet.rc_hub.push_hive(device_id, tid, msg) {
        Ok(()) => "sent",
        Err(HubError::ExecUnsupported(_)) => {
            // Connected as a build that does not run sessions: nothing there
            // can still be running this one, so there is nobody to ask.
            if state
                .sessions
                .end_now(
                    sid,
                    "stopped",
                    "the device's agent no longer runs agent sessions",
                )
                .await?
            {
                room::note_ended(&state, sid).await;
            }
            "ended"
        }
        // Not connected here: told on its next connection
        // (`agent_socket::reconcile_on_connect`).
        Err(_) => "queued",
    };
    Audit {
        tenant_id: tid,
        user_id: auth.user_id,
        target_id: None,
        device_id,
        session_id: Some(sid),
        action: "stop",
        outcome: sent,
        reason: None,
    }
    .write(&state)
    .await;
    info!(tenant = %tid, device = %device_id, session = %sid, outcome = sent, "hive: stop");

    let s = state
        .sessions
        .find(sid)
        .await?
        .ok_or_else(|| ApiError::Internal("the session record vanished".into()))?;
    let outcome = if s.status.is_terminal() {
        "ended"
    } else if sent == "queued" {
        "queued"
    } else {
        "stopping"
    };
    Ok(Json(StopResponse {
        outcome,
        session: SessionView::from(&s),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_is_kept_as_typed_but_never_empty_overlong_or_forged() {
        assert_eq!(clean_folder("  /home/dev/src  ").unwrap(), "/home/dev/src");
        assert_eq!(clean_folder(r"C:\src\app").unwrap(), r"C:\src\app");
        assert!(clean_folder("   ").is_err());
        assert!(clean_folder("/a\nINFO forged log line").is_err());
        assert!(clean_folder("/a\0b").is_err());
        assert!(clean_folder(&"x".repeat(hive_limits::MAX_FOLDER_LEN + 1)).is_err());
    }

    #[test]
    fn the_default_title_is_the_last_folder_component() {
        assert_eq!(clean_title(None, "/home/dev/src/app").unwrap(), "app");
        assert_eq!(clean_title(None, "/home/dev/src/app/").unwrap(), "app");
        assert_eq!(clean_title(None, r"C:\src\app").unwrap(), "app");
        assert_eq!(clean_title(Some("  "), "/srv/x").unwrap(), "x");
        assert_eq!(clean_title(None, "/").unwrap(), "/");
        assert_eq!(clean_title(Some(" Fix CI "), "/srv/x").unwrap(), "Fix CI");
    }

    #[test]
    fn a_long_default_title_is_cut_but_a_long_typed_one_is_refused() {
        let folder = "y".repeat(hive_limits::MAX_TITLE_LEN + 50);
        assert_eq!(
            clean_title(None, &folder).unwrap().chars().count(),
            hive_limits::MAX_TITLE_LEN
        );
        assert!(clean_title(Some(&folder), "/srv").is_err());
        assert!(clean_title(Some("a\u{1b}[31mred"), "/srv").is_err());
    }

    #[test]
    fn every_server_reason_has_its_own_word() {
        let all = [
            StartDenyReason::OrgArchived,
            StartDenyReason::NoPermission,
            StartDenyReason::DeviceOffline,
            StartDenyReason::DeviceUnsupported,
            StartDenyReason::RateLimited,
        ];
        let mut words: Vec<&str> = all.iter().map(|r| r.as_str()).collect();
        words.sort_unstable();
        words.dedup();
        assert_eq!(words.len(), all.len());
    }

    /// A device word this build does not know still reads as a refusal.
    #[test]
    fn every_device_refusal_has_a_message() {
        for r in roomler_ai_remote_control::hive::HiveRefusal::ALL {
            assert!(!device_refusal_message(r.as_str()).is_empty());
        }
        assert_eq!(
            device_refusal_message("from_2027"),
            "the device refused the session"
        );
    }
}
