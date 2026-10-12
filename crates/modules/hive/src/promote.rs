// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2e-3 — promotion, the server's half: the session's owner moves it
//! to one of its members (spec §3b "Promotion"; design §4.6, §6.2).
//!
//! The promotion lives ON THE RECORD (`agent_sessions.promotion`), and every
//! step is a compare-and-set on the state it leaves. A device's frame drives
//! each one, on whichever pod reads it, and a frame that arrives twice, late
//! or after the promotion ended matches nothing. The only pod-local piece is
//! one timer per promotion ([`watch`]), which fails a step that ran past its
//! bound. A promotion past its deadline counts as failed anywhere, so one
//! whose pod died strands nothing.
//!
//! | State | Entered when | Leaves when | Past its bound |
//! |---|---|---|---|
//! | `preparing` | the owner promotes: `rc:hive.prepare` to the member | the member's `rc:hive.prepare_ack` | failed, nothing moved |
//! | `draining` | the member is ready: `rc:hive.drain` to the primary | the primary's `rc:hive.drained` | failed; the primary gets its lease word back |
//! | `syncing` | the primary drained, at `{seq, hash}` | the member's own tip equals it: the lease moves | failed, as above |
//! | `moving` | the lease moved: join, `rc:hive.move`, `rc:hive.start` to the member | its start's answer | nothing moves back on silence (P2e-6) |
//!
//! | Rule | Why |
//! |---|---|
//! | prepare first, the lease last: the member passes every start gate before anything moves, and the lease moves only once its tip is the drained one | a refusal or a stalled sync leaves the session where it was |
//! | the lease moves to `fence + 1` in one compare-and-set, member roles with it | the fence is the one word every device checks; two promotions raced from two browsers move it once |
//! | the old primary is demoted at the move, before the member is started | from the move on nothing it does counts; stopping its harness before the next starts is what keeps two executors from overlapping (AC4) |
//! | a failure before the move gives the primary its lease word back (`rc:hive.lease` at its fence) | the word lifts the drain: the lease stayed |
//! | a start refused after the move moves the lease back, to `fence + 2` | a fence handed out once is never handed out again |
//! | the room carries every outcome, in its words | FR-83: a move that cannot happen says so, with the gate that said no |

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State},
};
use bson::{DateTime, doc, oid::ObjectId};
use dashmap::DashMap;
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::{
    error::Error as HubError,
    hive::{
        HiveDrainRefusal, HiveRefusal, HiveReplicaRole, hive_limits, is_hash_hex, replica_limits,
    },
    signaling::ServerMsg,
};
use roomler_core::{ApiError, extractors::auth::AuthUser};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::HiveState;
use crate::model::{
    AgentSession, DrainedTip, MemberState, Promotion, PromotionState, SessionLocation,
    SessionOrigin, SessionStatus, SessionView,
};
use crate::placement::MemberRole;
use crate::room;
use crate::routes::{Audit, clean_folder, device_refusal_message, member_tenant, parse_oid};

/// How long each step before the move may take
/// (`hive.promote_{prepare,drain,sync}_secs`).
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    pub prepare: Duration,
    pub drain: Duration,
    pub sync: Duration,
}

impl Bounds {
    pub fn of(settings: &roomler_ai_config::HiveSettings) -> Self {
        Self {
            prepare: Duration::from_secs(settings.promote_prepare_secs),
            drain: Duration::from_secs(settings.promote_drain_secs),
            sync: Duration::from_secs(settings.promote_sync_secs),
        }
    }

    fn of_state(&self, s: PromotionState) -> Duration {
        match s {
            PromotionState::Preparing => self.prepare,
            PromotionState::Draining => self.drain,
            PromotionState::Syncing => self.sync,
            // Expiry only: a start the member never answered is re-sent on
            // its every connection for the start's own redelivery window.
            PromotionState::Moving => Duration::from_secs(
                u64::try_from(hive_limits::START_REDELIVERY_WINDOW_SECS).unwrap_or(600),
            ),
        }
    }
}

fn deadline_after(d: Duration) -> DateTime {
    let ms = i64::try_from(d.as_millis()).unwrap_or(i64::MAX / 2);
    DateTime::from_millis(DateTime::now().timestamp_millis().saturating_add(ms))
}

/// `POST …/session/{session_id}/promote`.
#[derive(Debug, Deserialize)]
pub struct PromoteBody {
    /// The member to move it to.
    pub device_id: String,
    /// Where it runs there, as typed. Defaults to the folder it runs in now.
    #[serde(default)]
    pub folder: Option<String>,
}

/// Why the SERVER refused a promotion. The member's own refusals are the
/// start's [`HiveRefusal`] words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteDenyReason {
    OrgArchived,
    NoPermission,
    NotRunning,
    InProgress,
    MovedTooOften,
    NotAMember,
    AlreadyPrimary,
    DeviceOffline,
    DeviceUnsupported,
    PrimaryOffline,
    PrimaryUnsupported,
    RateLimited,
}

impl PromoteDenyReason {
    /// The spelling in the answer and the audit. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OrgArchived => "org_archived",
            Self::NoPermission => "no_permission",
            Self::NotRunning => "session_not_running",
            Self::InProgress => "promotion_in_progress",
            Self::MovedTooOften => "moved_too_often",
            Self::NotAMember => "not_a_member",
            Self::AlreadyPrimary => "already_primary",
            Self::DeviceOffline => "device_offline",
            Self::DeviceUnsupported => "device_unsupported",
            Self::PrimaryOffline => "primary_offline",
            Self::PrimaryUnsupported => "primary_unsupported",
            Self::RateLimited => "rate_limited",
        }
    }

    /// What the person is told: each names its own next step.
    pub const fn message(self) -> &'static str {
        match self {
            Self::OrgArchived => "this organization is archived; its sessions do not move",
            Self::NoPermission => {
                "your role does not include \"Run agent sessions\" (HIVE_RUN); \
                 an owner can grant it"
            }
            Self::NotRunning => {
                "the session is not running: it is over, being stopped, not started yet, \
                 or adopted from a terminal"
            }
            Self::InProgress => "the session is being moved already",
            Self::MovedTooOften => {
                "the session has moved as many times as one session can; \
                 a new session can be moved again"
            }
            Self::NotAMember => {
                "the device holds no copy of the session: only a member of its replicaset \
                 that took its copy can run it"
            }
            Self::AlreadyPrimary => "the session runs on that device already",
            Self::DeviceOffline => "the device is offline",
            Self::DeviceUnsupported => {
                "the device's agent does not take part in moving sessions — update it"
            }
            Self::PrimaryOffline => {
                "the device the session runs on is offline, so it cannot hand the session over"
            }
            Self::PrimaryUnsupported => {
                "the device the session runs on does not take part in moving sessions — update it"
            }
            Self::RateLimited => "too many sessions started on that device in the last minute",
        }
    }

    pub const ALL: [PromoteDenyReason; 12] = [
        Self::OrgArchived,
        Self::NoPermission,
        Self::NotRunning,
        Self::InProgress,
        Self::MovedTooOften,
        Self::NotAMember,
        Self::AlreadyPrimary,
        Self::DeviceOffline,
        Self::DeviceUnsupported,
        Self::PrimaryOffline,
        Self::PrimaryUnsupported,
        Self::RateLimited,
    ];
}

#[derive(Debug, Serialize)]
pub struct PromoteResponse {
    /// `promoting` (the member is ready, and the primary drains), `refused`
    /// (by the server or the member — `reason` says which gate), or `pending`
    /// (the member has not answered yet; the session's `promotion` and its
    /// room say how it goes on).
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub session: SessionView,
}

impl PromoteResponse {
    fn refused(reason: &str, message: String, s: &AgentSession) -> Self {
        Self {
            outcome: "refused",
            reason: Some(reason.to_string()),
            message: Some(message),
            session: SessionView::from(s),
        }
    }
}

/// What the route that waits on a member's prepare is told.
#[derive(Debug, Clone)]
enum PrepareAnswer {
    Ready,
    /// The member's word, or the server's when the promotion failed first.
    Refused {
        reason: String,
        message: String,
    },
}

struct Waiter {
    device_id: ObjectId,
    tx: oneshot::Sender<PrepareAnswer>,
}

/// The routes waiting for a member's `rc:hive.prepare_ack`. Pod-local, like
/// the start's: the prepare is pushed only when the member's socket is on
/// this pod, and its answer arrives on that socket. Keyed by PROMOTION, not
/// by session: a route that gives up must never take the waiter of the next
/// promotion of the same session. Bound to the member, so another device's
/// answer wakes nobody.
#[derive(Default)]
pub struct PrepareAcks {
    waiters: DashMap<ObjectId, Waiter>,
}

impl PrepareAcks {
    pub fn new() -> Self {
        Self::default()
    }

    fn expect(&self, promotion: ObjectId, device_id: ObjectId) -> oneshot::Receiver<PrepareAnswer> {
        let (tx, rx) = oneshot::channel();
        self.waiters.insert(promotion, Waiter { device_id, tx });
        rx
    }

    fn deliver(&self, promotion: ObjectId, from_device: ObjectId, answer: PrepareAnswer) {
        if let Some((_, w)) = self
            .waiters
            .remove_if(&promotion, |_, w| w.device_id == from_device)
        {
            let _ = w.tx.send(answer);
        }
    }

    fn forget(&self, promotion: ObjectId) {
        self.waiters.remove(&promotion);
    }

    /// Routes waiting now. Tests and diagnostics only.
    pub fn pending(&self) -> usize {
        self.waiters.len()
    }
}

/// Gates 4 on, past the session and the device lookups. The outer `Result`
/// is the infrastructure, the inner one the policy — as the start's.
async fn authorize(
    state: &HiveState,
    s: &AgentSession,
    target: ObjectId,
    user_id: ObjectId,
) -> Result<Result<(), PromoteDenyReason>, ApiError> {
    let tid = s.tenant_id;
    let tenant = state.tenants.base.find_by_id(tid).await?;
    if tenant.is_archived {
        return Ok(Err(PromoteDenyReason::OrgArchived));
    }
    let perms = state.tenants.get_member_permissions(tid, user_id).await?;
    if !permissions::has(perms, permissions::HIVE_RUN) {
        return Ok(Err(PromoteDenyReason::NoPermission));
    }
    let launched = s.accepted_at.is_some() || SessionStatus::LAUNCHED.contains(&s.status);
    if !SessionStatus::REPORTABLE.contains(&s.status)
        || !launched
        || s.origin == SessionOrigin::Adopted
    {
        return Ok(Err(PromoteDenyReason::NotRunning));
    }
    if s.promotion
        .as_ref()
        .is_some_and(|p| p.in_flight(DateTime::now()))
    {
        return Ok(Err(PromoteDenyReason::InProgress));
    }
    // P2e-5b — a move keeps where one fence ended and a move back a second,
    // and a device takes a join naming at most MAX_FENCE_MARKS of them: one
    // more, and no member could ever join the session again.
    if moved_too_often(s) {
        return Ok(Err(PromoteDenyReason::MovedTooOften));
    }
    if target == s.location.device_id {
        return Ok(Err(PromoteDenyReason::AlreadyPrimary));
    }
    if member_role(s, target).is_none() {
        return Ok(Err(PromoteDenyReason::NotAMember));
    }
    let hub = &state.fleet.rc_hub;
    match hub.agent_supports_hive_promote(target, tid) {
        None => return Ok(Err(PromoteDenyReason::DeviceOffline)),
        Some(false) => return Ok(Err(PromoteDenyReason::DeviceUnsupported)),
        Some(true) => {}
    }
    // An unreachable primary is waited out (P2e-6): until then, never.
    match hub.agent_supports_hive_promote(s.location.device_id, tid) {
        None => return Ok(Err(PromoteDenyReason::PrimaryOffline)),
        Some(false) => return Ok(Err(PromoteDenyReason::PrimaryUnsupported)),
        Some(true) => {}
    }
    // LAST: only a promotion that would reach the member spends a token.
    if !state
        .start_limiter
        .check(user_id, target, hive_limits::START_RATE_PER_MINUTE)
    {
        return Ok(Err(PromoteDenyReason::RateLimited));
    }
    Ok(Ok(()))
}

/// Whether one more promotion could leave `s` with more fences' ends than a
/// device takes in one join ([`replica_limits::MAX_FENCE_MARKS`]): a move
/// keeps one, and the move back a refused start makes keeps a second.
fn moved_too_often(s: &AgentSession) -> bool {
    s.fences.len() + 2 > replica_limits::MAX_FENCE_MARKS
}

/// The role `device` holds a copy of `s` in, as a member that took it:
/// `joined`, never the primary.
fn member_role(s: &AgentSession, device: ObjectId) -> Option<String> {
    s.replicaset.as_ref()?.members.iter().find_map(|m| {
        (m.device_id == device
            && m.role != MemberRole::Primary.as_str()
            && m.state == MemberState::Joined)
            .then(|| m.role.clone())
    })
}

fn audit(
    s: &AgentSession,
    by: ObjectId,
    device: ObjectId,
    outcome: &'static str,
    reason: Option<&'static str>,
) -> Audit {
    Audit {
        tenant_id: s.tenant_id,
        user_id: by,
        target_id: None,
        device_id: device,
        session_id: s.id,
        action: "promote",
        outcome,
        reason,
    }
}

/// `POST /api/tenant/{tenant_id}/hive/session/{session_id}/promote` — the
/// owner moves the session to a member of its replicaset. Answered once the
/// member said whether it is ready (or it has not, within the start's bound);
/// the rest goes on without the caller, and the room says how it ended.
pub async fn promote(
    State(state): State<HiveState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
    Json(body): Json<PromoteBody>,
) -> Result<Json<PromoteResponse>, ApiError> {
    let tid = member_tenant(&state, &tenant_id, &auth).await?;
    if !state.replicaset {
        return Err(ApiError::NotFound(
            "moving sessions is off on this server".into(),
        ));
    }
    let sid = parse_oid(&session_id, "session_id")?;
    // The owner's alone: anyone else's read of the id is a 404.
    let s = state.sessions.find_owned(tid, auth.user_id, sid).await?;
    let target = parse_oid(&body.device_id, "device_id")?;
    let folder = clean_folder(body.folder.as_deref().unwrap_or(&s.location.folder))?;
    // LIVE in this org: a removed or foreign device answers like a bogus one.
    let device = state.fleet.agents.find_live_in_tenant(tid, target).await?;

    if let Err(reason) = authorize(&state, &s, target, auth.user_id).await? {
        info!(
            tenant = %tid, session = %sid, device = %target, user = %auth.user_id,
            reason = reason.as_str(), "hive: promotion refused"
        );
        audit(&s, auth.user_id, target, "refused", Some(reason.as_str()))
            .write(&state)
            .await;
        return Ok(Json(PromoteResponse::refused(
            reason.as_str(),
            reason.message().to_string(),
            &s,
        )));
    }
    let to_role = member_role(&s, target).unwrap_or_else(|| MemberRole::Owner.as_str().into());
    // Read before the promotion begins: nothing after it may fail on the way
    // to the push, or the promotion would wait out its bound for nothing.
    let user = state.users.base.find_by_id(s.owner_id).await?;
    let now = DateTime::now();
    let p = Promotion {
        id: ObjectId::new(),
        to: target,
        to_name: device.name.clone(),
        to_role,
        folder: folder.clone(),
        from: s.location.clone(),
        from_fence: s.fence,
        by: auth.user_id,
        state: PromotionState::Preparing,
        started_at: now,
        deadline: deadline_after(state.promote_bounds.prepare),
        drained: None,
    };
    if !state
        .sessions
        .begin_promotion(tid, auth.user_id, sid, &p)
        .await?
    {
        // Another tab's promotion won, or the session moved on meanwhile.
        let s = state.sessions.find_owned(tid, auth.user_id, sid).await?;
        let reason = if s.promotion.is_some() {
            PromoteDenyReason::InProgress
        } else {
            PromoteDenyReason::NotRunning
        };
        audit(&s, auth.user_id, target, "refused", Some(reason.as_str()))
            .write(&state)
            .await;
        return Ok(Json(PromoteResponse::refused(
            reason.as_str(),
            reason.message().to_string(),
            &s,
        )));
    }

    // The waiter before the push: the member's answer, however fast, must
    // find it.
    let answer = state.prepare_acks.expect(p.id, target);
    let fence = u64::try_from(s.fence + 1).unwrap_or_default();
    let msg = ServerMsg::HivePrepare {
        session_id: sid,
        fence,
        harness: s.harness.id.clone(),
        harness_session: s.harness.session.clone(),
        folder,
        user_id: s.owner_id,
        user_email: user.email,
        caller: user.display_name,
    };
    if let Err(e) = state.fleet.rc_hub.push_hive_promote(target, tid, msg) {
        // Gone, or reconnected as a build that takes no part, between the
        // gate and the push: nothing reached it.
        state.prepare_acks.forget(p.id);
        let _ = state
            .sessions
            .end_promotion(sid, p.id, &[PromotionState::Preparing])
            .await;
        let reason = match e {
            HubError::ExecUnsupported(_) => PromoteDenyReason::DeviceUnsupported,
            _ => PromoteDenyReason::DeviceOffline,
        };
        audit(&s, auth.user_id, target, "refused", Some(reason.as_str()))
            .write(&state)
            .await;
        let s = state.sessions.find_owned(tid, auth.user_id, sid).await?;
        return Ok(Json(PromoteResponse::refused(
            reason.as_str(),
            reason.message().to_string(),
            &s,
        )));
    }
    audit(&s, auth.user_id, target, "sent", None)
        .write(&state)
        .await;
    info!(
        tenant = %tid, session = %sid, from = %s.location.device_id, to = %target,
        user = %auth.user_id, "hive: promotion started — prepare sent"
    );
    tokio::spawn(watch(state.clone(), sid, p.id));

    let heard = tokio::time::timeout(
        Duration::from_secs(hive_limits::START_ACK_TIMEOUT_SECS),
        answer,
    )
    .await;
    state.prepare_acks.forget(p.id);
    let s = state.sessions.find_owned(tid, auth.user_id, sid).await?;
    Ok(Json(match heard {
        Ok(Ok(PrepareAnswer::Ready)) => PromoteResponse {
            outcome: "promoting",
            reason: None,
            message: None,
            session: SessionView::from(&s),
        },
        Ok(Ok(PrepareAnswer::Refused { reason, message })) => {
            PromoteResponse::refused(&reason, message, &s)
        }
        _ => PromoteResponse {
            outcome: "pending",
            reason: None,
            message: Some(
                "the device has not answered yet; the session says how the move goes on".into(),
            ),
            session: SessionView::from(&s),
        },
    }))
}

/// Why a promotion failed before the lease moved.
#[derive(Debug, Clone)]
pub(crate) enum Failure {
    /// The member said no to the prepare, in the start's words.
    PrepareRefused {
        word: HiveRefusal,
        detail: Option<String>,
    },
    /// A step ran past its bound.
    Timeout(PromotionState),
    /// The primary did not drain, in its words.
    DrainRefused {
        word: HiveDrainRefusal,
        detail: Option<String>,
    },
    /// The primary's answer named no tip that can be read.
    DrainUnreadable,
    /// The member's copy is not the primary's: past the drained tip, or
    /// another event at it.
    Diverged,
    /// The primary could not be told to drain.
    PrimaryGone,
    /// The member no longer holds a joined copy.
    MemberLeft,
}

impl Failure {
    /// The audit's word.
    fn reason(&self) -> &'static str {
        match self {
            Self::PrepareRefused { word, .. } => word.as_str(),
            Self::Timeout(PromotionState::Preparing) => "prepare_timeout",
            Self::Timeout(PromotionState::Draining) => "drain_timeout",
            Self::Timeout(PromotionState::Syncing) => "sync_timeout",
            Self::Timeout(PromotionState::Moving) => "start_timeout",
            Self::DrainRefused {
                word: HiveDrainRefusal::NotRunning,
                ..
            } => "drain_not_running",
            Self::DrainRefused { .. } => "drain_refused",
            Self::DrainUnreadable => "drain_unreadable",
            Self::Diverged => "diverged",
            Self::PrimaryGone => "primary_offline",
            Self::MemberLeft => "member_left",
        }
    }

    /// Why, for the room and the waiting caller, its names escaped.
    fn why(&self, p: &Promotion) -> String {
        let to = room::name_label(&p.to_name);
        let from = room::name_label(&p.from.device_name);
        let with = |detail: &Option<String>| {
            detail
                .as_deref()
                .map(|d| format!(" ({})", room::md_escape(d)))
                .unwrap_or_default()
        };
        match self {
            Self::PrepareRefused { word, detail } => format!(
                "{to} cannot run it: {}{}",
                device_refusal_message(word.as_str()),
                with(detail)
            ),
            Self::Timeout(PromotionState::Preparing) => format!("{to} did not answer in time"),
            Self::Timeout(PromotionState::Draining) => {
                format!("{from} did not finish its turn and hand the session over in time")
            }
            Self::Timeout(PromotionState::Syncing) | Self::Timeout(PromotionState::Moving) => {
                format!("the copy on {to} did not catch up in time")
            }
            Self::DrainRefused {
                word: HiveDrainRefusal::NotRunning,
                detail,
            } => format!("{from} does not run the session{}", with(detail)),
            Self::DrainRefused { detail, .. } => {
                format!("{from} could not hand the session over{}", with(detail))
            }
            Self::DrainUnreadable => format!("{from} answered in a way that could not be read"),
            Self::Diverged => format!("the copy on {to} is not the one {from} holds"),
            Self::PrimaryGone => format!("{from} could not be reached"),
            Self::MemberLeft => format!("{to} no longer holds a copy of the session"),
        }
    }
}

/// End the promotion `p` names, if it is still in the state `p` says — the
/// one its caller read, so a step taken meanwhile is never failed in the name
/// of the one before it: the room told why, the audit written, a route still
/// waiting answered, and — once the primary was asked to drain, and unless
/// the caller sends it itself — the primary given its lease word back, which
/// lifts the drain.
pub(crate) async fn fail(
    state: &HiveState,
    session_id: ObjectId,
    p: &Promotion,
    failure: Failure,
    give_back: bool,
) {
    if !PromotionState::BEFORE_THE_MOVE.contains(&p.state) {
        return;
    }
    let before = match state
        .sessions
        .end_promotion(session_id, p.id, &[p.state])
        .await
    {
        Ok(Some(b)) => b,
        Ok(None) => return,
        Err(e) => {
            return warn!(session = %session_id, %e, "hive: a failed promotion was not ended");
        }
    };
    let Some(was) = before.promotion.clone() else {
        return;
    };
    // A session stopped or ended meanwhile: its end says so, and it holds
    // no lease to give back.
    let runs = SessionStatus::REPORTABLE.contains(&before.status);
    let fence = u64::try_from(was.from_fence).unwrap_or_default();
    if give_back
        && runs
        && matches!(
            was.state,
            PromotionState::Draining | PromotionState::Syncing
        )
    {
        crate::lease::push(
            state,
            before.tenant_id,
            was.from.device_id,
            session_id,
            fence,
        );
    }
    let why = failure.why(&was);
    info!(
        session = %session_id, to = %was.to, state = was.state.as_str(),
        reason = failure.reason(), "hive: promotion failed — nothing moved"
    );
    audit(&before, was.by, was.to, "failed", Some(failure.reason()))
        .write(state)
        .await;
    if runs {
        room::note(state, &before, room::move_failed_note(&was, &why)).await;
    }
    let reason = match &failure {
        Failure::PrepareRefused { word, .. } => word.as_str().to_string(),
        other => other.reason().to_string(),
    };
    state.prepare_acks.deliver(
        was.id,
        was.to,
        PrepareAnswer::Refused {
            reason,
            message: why,
        },
    );
}

/// The one timer of a promotion: a step that runs past its bound fails it.
/// It reads the record at each deadline, so a step taken meanwhile — on any
/// pod — only moves the deadline on. Gone once the lease moved: from there
/// the start's own redelivery governs, and nothing moves back on silence.
async fn watch(state: HiveState, session_id: ObjectId, pid: ObjectId) {
    loop {
        let p = match state.sessions.find(session_id).await {
            Ok(Some(s)) => match s.promotion {
                Some(p) if p.id == pid => p,
                _ => return,
            },
            Ok(None) => return,
            Err(e) => {
                warn!(session = %session_id, %e, "hive: a promotion's record was not read");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        if p.state == PromotionState::Moving {
            return;
        }
        let left = p.deadline.timestamp_millis() - DateTime::now().timestamp_millis();
        if left <= 0 {
            return fail(&state, session_id, &p, Failure::Timeout(p.state), true).await;
        }
        tokio::time::sleep(Duration::from_millis(u64::try_from(left).unwrap_or(0) + 10)).await;
    }
}

/// The promotion `s` holds, if it is in `state` and `device` is the one
/// that step waits on.
fn awaited(s: &AgentSession, state: PromotionState, device: ObjectId) -> Option<Promotion> {
    let p = s.promotion.as_ref()?;
    let waits_on = match state {
        PromotionState::Draining => p.from.device_id,
        _ => p.to,
    };
    (p.state == state && waits_on == device).then(|| p.clone())
}

/// `rc:hive.prepare_ack`: the member's answer to the prepare of the
/// promotion moving the session to it, at the fence the prepare named.
pub(crate) async fn on_prepare_ack(
    state: &HiveState,
    device_id: ObjectId,
    session_id: ObjectId,
    fence: u64,
    refused: Option<HiveRefusal>,
    detail: Option<String>,
) {
    let s = match state.sessions.find(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            return warn!(session = %session_id, %e, "hive: a prepare answer's session was not read");
        }
    };
    let Some(p) = awaited(&s, PromotionState::Preparing, device_id)
        .filter(|p| u64::try_from(p.from_fence + 1).ok() == Some(fence))
    else {
        return debug!(
            session = %session_id, device = %device_id, fence,
            "hive: a prepare answer for no promotion preparing on this device at that fence"
        );
    };
    if let Some(word) = refused {
        let detail = crate::agent_socket::clamp_words(detail, hive_limits::MAX_DETAIL_LEN);
        return fail(
            state,
            session_id,
            &p,
            Failure::PrepareRefused { word, detail },
            true,
        )
        .await;
    }
    match state
        .sessions
        .advance_promotion(
            session_id,
            p.id,
            PromotionState::Preparing,
            PromotionState::Draining,
            deadline_after(state.promote_bounds.drain),
            doc! {},
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            return warn!(session = %session_id, %e, "hive: a promotion did not take its next step");
        }
    }
    let drain = ServerMsg::HiveDrain {
        session_id,
        fence: u64::try_from(p.from_fence).unwrap_or_default(),
    };
    if let Err(e) = state
        .fleet
        .rc_hub
        .push_hive_promote(p.from.device_id, s.tenant_id, drain)
    {
        debug!(session = %session_id, %e, "hive: the drain did not reach the primary");
        let draining = Promotion {
            state: PromotionState::Draining,
            ..p
        };
        return fail(state, session_id, &draining, Failure::PrimaryGone, true).await;
    }
    info!(session = %session_id, to = %device_id, "hive: the member is ready — the primary drains");
    let by = display_name(state, p.by).await;
    room::note(state, &s, room::moving_note(&p, &by)).await;
    state
        .prepare_acks
        .deliver(p.id, device_id, PrepareAnswer::Ready);
}

/// `rc:hive.drained`: the primary's answer to the drain, at the fence it runs
/// the session at. Its tip, once well formed, is what the member's copy must
/// reach before the lease moves.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn on_drained(
    state: &HiveState,
    device_id: ObjectId,
    session_id: ObjectId,
    fence: u64,
    seq: Option<u64>,
    hash: Option<String>,
    refused: Option<HiveDrainRefusal>,
    detail: Option<String>,
) {
    let s = match state.sessions.find(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            return warn!(session = %session_id, %e, "hive: a drain answer's session was not read");
        }
    };
    let Some(p) = awaited(&s, PromotionState::Draining, device_id)
        .filter(|p| u64::try_from(p.from_fence).ok() == Some(fence))
    else {
        return debug!(
            session = %session_id, device = %device_id, fence,
            "hive: a drain answer for no promotion draining this device at that fence"
        );
    };
    if let Some(word) = refused {
        let detail = crate::agent_socket::clamp_words(detail, hive_limits::MAX_DETAIL_LEN);
        return fail(
            state,
            session_id,
            &p,
            Failure::DrainRefused { word, detail },
            true,
        )
        .await;
    }
    let tip = match (seq.and_then(|q| i64::try_from(q).ok()), hash) {
        (Some(seq), Some(hash)) if is_hash_hex(&hash) => DrainedTip { seq, hash },
        _ => return fail(state, session_id, &p, Failure::DrainUnreadable, true).await,
    };
    match state
        .sessions
        .advance_promotion(
            session_id,
            p.id,
            PromotionState::Draining,
            PromotionState::Syncing,
            deadline_after(state.promote_bounds.sync),
            doc! { "promotion.drained": { "seq": tip.seq, "hash": &tip.hash } },
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            return warn!(session = %session_id, %e, "hive: a promotion did not take its next step");
        }
    }
    info!(session = %session_id, seq = tip.seq, "hive: the primary drained — the member's copy is to reach its tip");
    // The member may hold it already.
    if let Ok(Some(s)) = state.sessions.find(session_id).await {
        check_sync(state, &s).await;
    }
}

/// A member's tip landed (`rc:hive.replica.tip`, or a manifest's entry):
/// the promotion moving the session to that member, if one is syncing, may
/// move the lease now.
pub(crate) async fn on_tip(state: &HiveState, session_id: ObjectId, device_id: ObjectId) {
    match state
        .sessions
        .base
        .find_one(doc! {
            "_id": session_id,
            "promotion.state": PromotionState::Syncing.as_str(),
            "promotion.to": device_id,
        })
        .await
    {
        Ok(Some(s)) => check_sync(state, &s).await,
        Ok(None) => {}
        Err(e) => warn!(session = %session_id, %e, "hive: a tip's promotion was not read"),
    }
}

/// Where the member's copy stands against the drained tip, for a promotion
/// that is syncing.
#[derive(Debug, PartialEq, Eq)]
enum Sync {
    /// Not there yet, or no tip reported.
    Behind,
    /// The same event: the lease may move.
    Reached,
    /// Past it, or another event at its `seq`: never promoted.
    Diverged,
}

fn sync_of(s: &AgentSession, p: &Promotion) -> Sync {
    let Some(d) = &p.drained else {
        return Sync::Behind;
    };
    let tip = s
        .replicaset
        .as_ref()
        .and_then(|r| r.members.iter().find(|m| m.device_id == p.to))
        .and_then(|m| m.tip.as_ref());
    match tip {
        Some(t) if t.seq == d.seq && t.hash == d.hash => Sync::Reached,
        Some(t) if t.seq >= d.seq => Sync::Diverged,
        _ => Sync::Behind,
    }
}

async fn check_sync(state: &HiveState, s: &AgentSession) {
    let (Some(sid), Some(p)) = (s.id, s.promotion.clone()) else {
        return;
    };
    if p.state != PromotionState::Syncing {
        return;
    }
    match sync_of(s, &p) {
        Sync::Behind => {}
        Sync::Diverged => fail(state, sid, &p, Failure::Diverged, true).await,
        Sync::Reached => move_lease(state, s, &p).await,
    }
}

/// The lease CAS, then everything that follows it, in this order: the old
/// primary's open approvals withdrawn, every view and stream of the session
/// ended, the old primary demoted, every member's join at the new fence,
/// and the member's own join, move and start.
async fn move_lease(state: &HiveState, s: &AgentSession, p: &Promotion) {
    let Some(sid) = s.id else { return };
    let (role, member_state) = match crate::placement::old_primary_role(state, s, p.to).await {
        Ok(r) => r,
        Err(e) => {
            warn!(session = %sid, %e, "hive: the old primary's place was not read — it leaves");
            (MemberRole::Owner, MemberState::Leaving)
        }
    };
    let deadline = deadline_after(state.promote_bounds.of_state(PromotionState::Moving));
    match state
        .sessions
        .move_lease(sid, p, role.as_str(), member_state, deadline)
        .await
    {
        Ok(true) => {}
        Ok(false) => return changed(state, sid, p).await,
        // The timer fails it at the sync's bound, should no next tip move it.
        Err(e) => return warn!(session = %sid, %e, "hive: the lease did not move"),
    }
    let s = match state.sessions.find(sid).await {
        Ok(Some(s)) => s,
        _ => return warn!(session = %sid, "hive: the moved session was not read back"),
    };
    let next = u64::try_from(s.fence).unwrap_or_default();
    info!(
        session = %sid, from = %p.from.device_id, to = %p.to, fence = next,
        old_primary = member_state.as_str(), "hive: the lease moved"
    );
    audit(&s, p.by, p.to, "moved", None).write(state).await;
    // What the old primary's run had open belongs to a run that is over.
    room::withdraw_approvals(state, &s).await;
    crate::view::end_grants_of_session(state, sid, "moved").await;
    crate::stream::end_for_session(state, sid, "moved");
    let demote = ServerMsg::HiveDemote {
        session_id: sid,
        fence: next,
        reason: "promoted".to_string(),
    };
    if let Err(e) = state
        .fleet
        .rc_hub
        .push_hive(p.from.device_id, s.tenant_id, demote)
    {
        // Told at its next report of the session, or its next manifest.
        debug!(session = %sid, %e, "hive: the old primary was not told it was demoted");
    }
    send_member_joins(state, &s);
    send_start(state, &s, &p.from, p.by).await;
}

/// The lease CAS missed: the session moved on. Stopped, or moved already by
/// a frame on another pod: nothing to say. Still on the primary at its
/// fence: the member no longer holds a joined copy.
async fn changed(state: &HiveState, session_id: ObjectId, p: &Promotion) {
    let Ok(Some(s)) = state.sessions.find(session_id).await else {
        return;
    };
    let still_there = SessionStatus::REPORTABLE.contains(&s.status)
        && s.location.device_id == p.from.device_id
        && s.fence == p.from_fence;
    if still_there {
        fail(state, session_id, p, Failure::MemberLeft, true).await;
    } else if let Err(e) = state
        .sessions
        .end_promotion(session_id, p.id, &PromotionState::BEFORE_THE_MOVE)
        .await
    {
        warn!(session = %session_id, %e, "hive: a promotion overtaken by a stop was not ended");
    }
}

/// Every member owed its join at the session's fence: each in its role, the
/// primary's own left to [`send_start`].
fn send_member_joins(state: &HiveState, s: &AgentSession) {
    let Some(r) = s.replicaset.as_ref() else {
        return;
    };
    for m in &r.members {
        if m.state != MemberState::Pending || m.role == MemberRole::Primary.as_str() {
            continue;
        }
        if let Some(role) = crate::replica::wire_role(&m.role) {
            crate::replica::push_join(state, s, m.device_id, role);
        }
    }
}

/// What the device about to run `s` at its fence hears first: its join as
/// the primary, while it is owed one (its floor rises before it writes), and
/// the record's `rc:hive.move`. For a start at a fence past the first: a
/// promotion's, a move back's, or one of theirs re-sent on connect.
pub(crate) async fn before_start(
    state: &HiveState,
    s: &AgentSession,
    from: &SessionLocation,
    by: ObjectId,
) {
    let Some(sid) = s.id else { return };
    let device = s.location.device_id;
    let fence = u64::try_from(s.fence).unwrap_or_default();
    let owed = s.replicaset.as_ref().is_some_and(|r| {
        r.members.iter().any(|m| {
            m.device_id == device
                && m.role == MemberRole::Primary.as_str()
                && m.state == MemberState::Pending
        })
    });
    if owed {
        crate::replica::push_join(state, s, device, HiveReplicaRole::Primary);
    }
    let mv = ServerMsg::HiveMove {
        session_id: sid,
        fence,
        turns: s.last_turn.map(|t| t.turn).unwrap_or(0),
        from_device_id: from.device_id,
        from_device_name: from.device_name.clone(),
        from_folder: from.folder.clone(),
        moved_by: by,
        moved_by_name: display_name(state, by).await,
        moved_at_ms: u64::try_from(DateTime::now().timestamp_millis()).unwrap_or_default(),
    };
    if let Err(e) = state
        .fleet
        .rc_hub
        .push_hive_promote(device, s.tenant_id, mv)
    {
        debug!(session = %sid, %e, "hive: the move did not reach the device");
    }
}

/// The start of `s` on the device it is located on now, at its fence, as a
/// resume: [`before_start`], the session's frozen core memory, then
/// `rc:hive.start`. Unanswered, it is re-sent on the device's every
/// connection, as every start is.
async fn send_start(state: &HiveState, s: &AgentSession, from: &SessionLocation, by: ObjectId) {
    let Some(sid) = s.id else { return };
    let device = s.location.device_id;
    let fence = u64::try_from(s.fence).unwrap_or_default();
    before_start(state, s, from, by).await;
    match state.brain.snapshot_of(sid).await {
        Ok(Some(m)) => {
            let frame = crate::routes::memory_frame(&m, fence);
            let _ = state
                .fleet
                .rc_hub
                .push_hive_memory(device, s.tenant_id, frame);
        }
        Ok(None) => {}
        Err(e) => debug!(session = %sid, %e, "hive: core memory not sent with the move"),
    }
    let owner = match state.users.base.find_by_id(s.owner_id).await {
        Ok(u) => u,
        Err(e) => {
            return warn!(session = %sid, %e, "hive: the moved start was not sent — its owner could not be read; it is re-sent on connect");
        }
    };
    let replicated = state.replicaset && crate::replica::replicated(s);
    let msg = ServerMsg::HiveStart {
        session_id: sid,
        harness: s.harness.id.clone(),
        harness_session: s.harness.session.clone(),
        fence,
        folder: s.location.folder.clone(),
        user_id: s.owner_id,
        user_email: owner.email,
        caller: owner.display_name,
        resume: true,
        replicated,
        lease: replicated
            && state
                .fleet
                .rc_hub
                .agent_supports_hive_lease(device, s.tenant_id)
                == Some(true),
    };
    match state.fleet.rc_hub.push_hive(device, s.tenant_id, msg) {
        Ok(()) => info!(session = %sid, device = %device, fence, "hive: the moved start was sent"),
        Err(e) => {
            debug!(session = %sid, %e, "hive: the moved start was not sent — it is re-sent on connect")
        }
    }
}

/// The member answered the moved start `accepted`: the promotion is done,
/// and the room says where the session runs now. `true` when this answer
/// ended it (its note posted).
pub(crate) async fn on_launched(
    state: &HiveState,
    session_id: ObjectId,
    device_id: ObjectId,
    fence: u64,
) -> bool {
    let Ok(Some(s)) = state.sessions.find(session_id).await else {
        return false;
    };
    let Some(p) = awaited(&s, PromotionState::Moving, device_id)
        .filter(|p| u64::try_from(p.from_fence + 1).ok() == Some(fence))
    else {
        return false;
    };
    match state
        .sessions
        .end_promotion(session_id, p.id, &[PromotionState::Moving])
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return false,
        Err(e) => {
            warn!(session = %session_id, %e, "hive: a done promotion was not ended");
            return false;
        }
    }
    let s = match state.sessions.find(session_id).await {
        Ok(Some(after)) => after,
        _ => s,
    };
    info!(session = %session_id, device = %device_id, fence, "hive: promotion done — the session runs on its new primary");
    audit(&s, p.by, p.to, "done", None).write(state).await;
    let by = display_name(state, p.by).await;
    room::note(state, &s, room::moved_note(&s, &p, &by)).await;
    true
}

/// The member refused the moved start: the lease moves back
/// ([`crate::dao::AgentSessionDao::move_lease_back`]) and the session starts
/// again where it ran. `true` when this answer was the promotion's, and
/// handled here: the session is NOT refused.
pub(crate) async fn on_start_refused(
    state: &HiveState,
    session_id: ObjectId,
    device_id: ObjectId,
    fence: u64,
    word: HiveRefusal,
    detail: Option<String>,
) -> bool {
    let Ok(Some(s)) = state.sessions.find(session_id).await else {
        return false;
    };
    let Some(p) = awaited(&s, PromotionState::Moving, device_id)
        .filter(|p| u64::try_from(p.from_fence + 1).ok() == Some(fence))
    else {
        return false;
    };
    match state.sessions.move_lease_back(session_id, &p).await {
        Ok(true) => {}
        Ok(false) => return false,
        Err(e) => {
            warn!(session = %session_id, %e, "hive: the lease did not move back");
            return false;
        }
    }
    let s = match state.sessions.find(session_id).await {
        Ok(Some(after)) => after,
        _ => return true,
    };
    info!(
        session = %session_id, refused = word.as_str(), back_to = %p.from.device_id,
        fence = s.fence, "hive: the member refused the moved start — the lease moved back"
    );
    audit(&s, p.by, p.to, "moved_back", Some(word.as_str()))
        .write(state)
        .await;
    // P2e-5c — and every view and stream of the session ends, as at the
    // move. The member never ran it: a stream it was asked to serve would
    // tell its member the session runs nowhere, and the member would park a
    // copy of a session that runs on.
    crate::view::end_grants_of_session(state, session_id, "moved").await;
    crate::stream::end_for_session(state, session_id, "moved");
    let detail = crate::agent_socket::clamp_words(detail, hive_limits::MAX_DETAIL_LEN);
    let why = Failure::PrepareRefused { word, detail }.why(&p);
    room::note(state, &s, room::move_failed_note(&p, &why)).await;
    send_member_joins(state, &s);
    let here = s.location.clone();
    send_start(state, &s, &here, p.by).await;
    true
}

/// P2e-2's word for a session a manifest names, during a promotion: none
/// while the primary is drained for the move (the lease is in motion); a
/// promotion past its deadline is failed here, with no word of its own —
/// the manifest's answer sends it.
pub(crate) async fn withholds_word(state: &HiveState, s: &AgentSession) -> bool {
    let (Some(sid), Some(p)) = (s.id, s.promotion.as_ref()) else {
        return false;
    };
    if !matches!(p.state, PromotionState::Draining | PromotionState::Syncing) {
        return false;
    }
    if p.in_flight(DateTime::now()) {
        return true;
    }
    fail(state, sid, p, Failure::Timeout(p.state), false).await;
    false
}

async fn display_name(state: &HiveState, user: ObjectId) -> String {
    state
        .users
        .find_display_names(&[user])
        .await
        .ok()
        .and_then(|m| m.get(&user).cloned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HarnessRef, MemberTip, ReplicaMember, Replicaset};

    #[test]
    fn every_refusal_has_its_own_word() {
        let mut words: Vec<&str> = PromoteDenyReason::ALL.iter().map(|r| r.as_str()).collect();
        words.sort_unstable();
        words.dedup();
        assert_eq!(words.len(), PromoteDenyReason::ALL.len());
        for r in PromoteDenyReason::ALL {
            assert!(!r.message().is_empty(), "{r:?}");
        }
    }

    fn tip(seq: i64, fill: &str) -> MemberTip {
        MemberTip {
            fence: 1,
            seq,
            hash: fill.repeat(64 / fill.len()),
            checkpoint_seq: None,
            checkpoint_hash: None,
            at: DateTime::now(),
        }
    }

    fn syncing(
        member_tip: Option<MemberTip>,
        drained: Option<(i64, &str)>,
    ) -> (AgentSession, Promotion) {
        let now = DateTime::now();
        let (a, b) = (ObjectId::new(), ObjectId::new());
        let from = SessionLocation {
            device_id: a,
            device_name: "mars".into(),
            folder: "/src".into(),
            account: None,
        };
        let p = Promotion {
            id: ObjectId::new(),
            to: b,
            to_name: "venus".into(),
            to_role: "owner".into(),
            folder: "/src".into(),
            from: from.clone(),
            from_fence: 1,
            by: ObjectId::new(),
            state: PromotionState::Syncing,
            started_at: now,
            deadline: now,
            drained: drained.map(|(seq, fill)| DrainedTip {
                seq,
                hash: fill.repeat(64 / fill.len()),
            }),
        };
        let member = |device_id, role: &str, tip| ReplicaMember {
            device_id,
            role: role.into(),
            state: MemberState::Joined,
            refusal: None,
            answered_at: None,
            tip,
        };
        let s = AgentSession {
            id: Some(ObjectId::new()),
            tenant_id: ObjectId::new(),
            owner_id: p.by,
            drivers: Vec::new(),
            title: "t".into(),
            room_id: None,
            last_turn: None,
            harness: HarnessRef {
                id: "claude-code".into(),
                session: "u".into(),
            },
            location: from,
            status: SessionStatus::Idle,
            fence: 1,
            accepted_at: Some(now),
            refusal: None,
            end_reason: None,
            detail: None,
            created_at: now,
            updated_at: now,
            ended_at: None,
            brain_rev: None,
            origin: SessionOrigin::Started,
            replicaset: Some(Replicaset {
                policy_revision: 0,
                min: 2,
                max: 4,
                restricted_tags: Vec::new(),
                members: vec![
                    member(a, "primary", Some(tip(9, "ff"))),
                    member(b, "owner", member_tip),
                ],
                short: None,
                placed_at: now,
            }),
            promotion: Some(p.clone()),
            fences: Vec::new(),
        };
        (s, p)
    }

    /// The lease moves only on the member's OWN tip, and only at the drained
    /// event exactly: behind is waited for; past it, or another event at its
    /// seq, is a copy that is not the primary's. The primary's tip says
    /// nothing about the member's copy.
    #[test]
    fn the_lease_moves_only_once_the_members_own_tip_is_the_drained_one() {
        let (s, p) = syncing(Some(tip(9, "ab")), Some((9, "ab")));
        assert_eq!(
            sync_of(&s, &p),
            Sync::Reached,
            "the member's own tip at the drained one"
        );
        let (s, p) = syncing(Some(tip(8, "ab")), Some((9, "ab")));
        assert_eq!(sync_of(&s, &p), Sync::Behind, "behind it");
        let (s, p) = syncing(None, Some((9, "ff")));
        assert_eq!(
            sync_of(&s, &p),
            Sync::Behind,
            "no tip of its own: the primary's matching tip is not the member's"
        );
        let (s, p) = syncing(Some(tip(9, "cd")), Some((9, "ab")));
        assert_eq!(sync_of(&s, &p), Sync::Diverged, "another event at its seq");
        let (s, p) = syncing(Some(tip(10, "ab")), Some((9, "ab")));
        assert_eq!(sync_of(&s, &p), Sync::Diverged, "past the drained tip");
        let (s, p) = syncing(Some(tip(9, "ab")), None);
        assert_eq!(sync_of(&s, &p), Sync::Behind, "nothing drained yet");
    }

    /// P2e-5b — a promotion that could leave the session naming more fences'
    /// ends than a device takes in one join is refused before it begins: a
    /// move keeps one end, and the move back of a refused start a second.
    #[test]
    fn a_session_is_not_moved_past_the_ends_a_join_can_carry() {
        let max = replica_limits::MAX_FENCE_MARKS;
        let (mut s, _) = syncing(None, None);
        let mark = |fence: usize| crate::model::FenceMark {
            fence: fence as i64,
            seq: 1,
            hash: "ab".repeat(32),
        };
        assert!(!moved_too_often(&s), "never moved");
        s.fences = (1..=max - 2).map(mark).collect();
        assert!(!moved_too_often(&s), "room for a move and its move back");
        s.fences.push(mark(max - 1));
        assert!(
            moved_too_often(&s),
            "a move back would leave a join naming one end too many"
        );
    }

    /// The member's role, only as a joined member, never the primary's row.
    #[test]
    fn only_a_joined_member_can_be_promoted_to() {
        let (mut s, p) = syncing(None, None);
        assert_eq!(member_role(&s, p.to).as_deref(), Some("owner"));
        assert_eq!(member_role(&s, p.from.device_id), None, "the primary's row");
        assert_eq!(member_role(&s, ObjectId::new()), None, "a stranger");
        s.replicaset.as_mut().unwrap().members[1].state = MemberState::Pending;
        assert_eq!(member_role(&s, p.to), None, "pending: it holds no copy yet");
    }

    /// Each step waits on one device: the drain on the primary, every other
    /// on the member it moves to; and only in its own state.
    #[test]
    fn each_step_waits_on_its_own_device() {
        let (mut s, p) = syncing(None, None);
        assert!(
            awaited(&s, PromotionState::Syncing, p.to).is_some(),
            "the sync waits on the member"
        );
        assert!(
            awaited(&s, PromotionState::Syncing, p.from.device_id).is_none(),
            "not on the primary"
        );
        assert!(
            awaited(&s, PromotionState::Draining, p.to).is_none(),
            "another state"
        );
        s.promotion.as_mut().unwrap().state = PromotionState::Draining;
        assert!(
            awaited(&s, PromotionState::Draining, p.from.device_id).is_some(),
            "the drain waits on the primary"
        );
        assert!(
            awaited(&s, PromotionState::Draining, p.to).is_none(),
            "not on the member"
        );
    }

    /// Every failure has an audit word and a reason a person can read.
    #[test]
    fn every_failure_says_why_in_its_own_words() {
        let (_, p) = syncing(None, None);
        let all = [
            Failure::PrepareRefused {
                word: HiveRefusal::NoAccount,
                detail: Some("no *map*".into()),
            },
            Failure::Timeout(PromotionState::Preparing),
            Failure::Timeout(PromotionState::Draining),
            Failure::Timeout(PromotionState::Syncing),
            Failure::DrainRefused {
                word: HiveDrainRefusal::NotRunning,
                detail: None,
            },
            Failure::DrainRefused {
                word: HiveDrainRefusal::Other,
                detail: None,
            },
            Failure::DrainUnreadable,
            Failure::Diverged,
            Failure::PrimaryGone,
            Failure::MemberLeft,
        ];
        let mut words: Vec<&str> = all.iter().map(Failure::reason).collect();
        words.sort_unstable();
        words.dedup();
        assert_eq!(words.len(), all.len(), "{words:?}");
        let refused = all[0].why(&p);
        assert!(
            refused.starts_with("**venus** cannot run it: "),
            "{refused}"
        );
        assert!(refused.contains(r"(no \*map\*)"), "escaped: {refused}");
        assert_eq!(
            all[2].why(&p),
            "**mars** did not finish its turn and hand the session over in time"
        );
    }
}
