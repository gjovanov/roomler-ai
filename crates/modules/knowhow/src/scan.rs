// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! P4-3a — device scans, the server's half (the FR-90 spec, §3e "P4-3,
//! scans, as designed"): a person's request, its delivery, and the device's
//! answer and its record.
//!
//! A scan is the server's request and the device's answer (decision 38). The
//! request names no root: the device reads only those its owner lists. It is
//! a row in `knowhow_scans`, and the row is the bus, the vault's way: the pod
//! holding the device's connection tells it, at once when the request lands
//! there, at its next sweep when it landed on another pod, and at the
//! device's next hello when it was offline. A module answers a device on the
//! connection's own sender, so knowhow needs no edge to fleet's Hub, which is
//! pod-local.
//!
//! ⚠️ P4-3a-1 records an answer: its outcome and what it held, by kind, each
//! finding read and checked by the wire's own rules
//! (`roomler_ai_remote_control::knowhow`). Nothing of a finding is stored
//! yet: writing the findings onto the map is P4-3a-2's (decision 42).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use bson::{DateTime, doc, oid::ObjectId};
use roomler_ai_remote_control::knowhow::{
    Finding, MAX_DETAIL, MAX_FINDINGS, MAX_SCAN_BYTES, ScanOutcome, holds_credential,
};
use roomler_ai_remote_control::models::{Agent, RpcCap, UpdateRefusal};
use roomler_ai_remote_control::signaling::{ClientMsg, ServerMsg};
use roomler_ai_services::dao::base::DaoError;
use roomler_core::{
    AgentCtx, AgentMsgHandler, AgentSocketLifecycle, ApiError, extractors::auth::AuthUser,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::KnowhowState;
use crate::dao::SCAN_ANSWER_SECS;
use crate::model::{
    KnowhowScan, SCAN_DELIVERED, SCAN_DONE, SCAN_FAILED, SCAN_NO_ROOTS, SCAN_REQUESTED, ScanCounts,
};
use crate::routes::{limit_of, parse_id, reader, writer};
use crate::view::ScanView;

/// How often every pod looks for scans asked of its devices when
/// `knowhow.scan_sweep_secs` is 0 or absent.
pub const DEFAULT_SCAN_SWEEP_SECS: u64 = 5;

/// How long a delivery waits on a connection's queue before it is put back
/// for the next sweep.
const SEND_WAIT: Duration = Duration::from_secs(2);

// ─── this pod's connections ─────────────────────────────────────────────────

/// The connections on this pod of devices that can scan or probe (P4-5), by
/// device: a newer connection replaces an older one, and a closing one takes
/// only its own.
#[derive(Default)]
pub struct Links {
    by_device: Mutex<HashMap<ObjectId, Link>>,
}

#[derive(Clone)]
pub(crate) struct Link {
    conn_id: String,
    tenant_id: ObjectId,
    pub(crate) tx: mpsc::Sender<ServerMsg>,
}

impl Links {
    fn insert(&self, device: ObjectId, link: Link) {
        self.lock().insert(device, link);
    }

    fn remove(&self, device: ObjectId, conn_id: &str) {
        let mut map = self.lock();
        if map.get(&device).is_some_and(|l| l.conn_id == conn_id) {
            map.remove(&device);
        }
    }

    /// The device's connection on this pod, in `tenant_id`. A connection
    /// whose socket is gone is forgotten here.
    pub(crate) fn get(&self, device: ObjectId, tenant_id: ObjectId) -> Option<Link> {
        let mut map = self.lock();
        let link = map.get(&device)?.clone();
        if link.tx.is_closed() {
            map.remove(&device);
            return None;
        }
        (link.tenant_id == tenant_id).then_some(link)
    }

    /// Every device connected here.
    pub(crate) fn devices(&self) -> Vec<ObjectId> {
        let mut map = self.lock();
        map.retain(|_, l| !l.tx.is_closed());
        map.keys().copied().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ObjectId, Link>> {
        self.by_device.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ─── who may be asked ───────────────────────────────────────────────────────

/// Why a device is not asked to scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Its last hello did not advertise `knowhow-scan`: an agent that cannot
    /// scan would never answer.
    CannotScan,
    /// A secondary enrollment: its roots are one device-owned list, scanned
    /// for its primary organization alone (decision 43).
    SecondaryEnrollment,
}

impl Refusal {
    pub fn message(self) -> String {
        match self {
            Self::CannotScan => {
                "this device's agent cannot scan: update it to one that advertises knowhow-scan"
                    .into()
            }
            Self::SecondaryEnrollment => {
                "a device is scanned for its primary organization only, and this is a secondary \
                 enrollment"
                    .into()
            }
        }
    }
}

/// Why `agent` is not asked to scan, or `None` when it is, by what its last
/// hello said.
pub fn refusal(agent: &Agent) -> Option<Refusal> {
    if !agent.capabilities.has_rpc(RpcCap::KnowhowScan) {
        return Some(Refusal::CannotScan);
    }
    // `update_mode: primary_org` is what every enrollment but the primary
    // says, whatever its updater does.
    if agent.capabilities.update_refusal() == Some(UpdateRefusal::PrimaryOrg) {
        return Some(Refusal::SecondaryEnrollment);
    }
    None
}

// ─── delivery ───────────────────────────────────────────────────────────────

/// Send `scan` on `link`. A requested one goes only from the caller that
/// moved it to delivered, and back to requested when it cannot go out, for
/// the next sweep. A delivered one goes again as it is: its connection may
/// have died with it, and the device runs a scan id once.
async fn deliver(state: &KnowhowState, scan: &KnowhowScan, link: &Link) {
    let Some(id) = scan.id else { return };
    let now = DateTime::now();
    let fresh = match scan.state.as_str() {
        SCAN_REQUESTED => match state.scans.deliver(id, now).await {
            Ok(true) => true,
            Ok(false) => return,
            Err(e) => {
                warn!(scan = %id, %e, "knowhow: a scan was not marked delivered; the next sweep tries again");
                return;
            }
        },
        SCAN_DELIVERED => false,
        _ => return,
    };
    let sent = tokio::time::timeout(
        SEND_WAIT,
        link.tx.send(ServerMsg::KnowhowScan { scan_id: id }),
    )
    .await;
    match sent {
        Ok(Ok(())) => {
            if !fresh && let Err(e) = state.scans.redelivered(id, now).await {
                warn!(scan = %id, %e, "knowhow: a scan sent again was not stamped");
            }
            info!(scan = %id, device = %scan.device_id, again = !fresh, "knowhow: scan sent");
        }
        _ if fresh => {
            debug!(scan = %id, "knowhow: a scan could not go out; put back for the next sweep");
            if let Err(e) = state.scans.undeliver(id).await {
                warn!(scan = %id, %e, "knowhow: an undelivered scan waits for the device's next hello");
            }
        }
        _ => {
            debug!(scan = %id, "knowhow: a scan sent again could not go out; the next hello tries")
        }
    }
}

/// A device's hello on this pod: link its connection when it can scan or
/// probe, and send what was asked while it was away, or sent to a connection
/// that died: its scan, and (P4-5) its probe.
pub(crate) async fn hello(state: &KnowhowState, ctx: &AgentCtx) {
    let agent = match state
        .agents
        .find_live_in_tenant(ctx.tenant_id, ctx.agent_id)
        .await
    {
        Ok(agent) => agent,
        Err(DaoError::NotFound) => return,
        Err(e) => {
            warn!(device = %ctx.agent_id, %e, "knowhow: a hello's device was not read; its scans and probes wait for the next");
            return;
        }
    };
    let scans = refusal(&agent).is_none();
    let probes = crate::probe::refusal(&agent).is_none();
    if !scans && !probes {
        return;
    }
    let link = Link {
        conn_id: ctx.conn_id.clone(),
        tenant_id: ctx.tenant_id,
        tx: ctx.tx.clone(),
    };
    state.links.insert(ctx.agent_id, link.clone());
    if scans {
        match state.scans.pending_for(ctx.tenant_id, ctx.agent_id).await {
            Ok(Some(scan)) => deliver(state, &scan, &link).await,
            Ok(None) => {}
            Err(e) => {
                warn!(device = %ctx.agent_id, %e, "knowhow: a hello's pending scan was not read; the sweep tries again")
            }
        }
    }
    if probes {
        crate::probe::hello(state, ctx, &link).await;
    }
}

/// One sweep on this pod: what waited 24 hours for an answer expires, and
/// what was asked of the devices connected here goes out.
pub async fn sweep_once(state: &KnowhowState) {
    let cutoff =
        DateTime::from_millis(DateTime::now().timestamp_millis() - SCAN_ANSWER_SECS * 1000);
    match state.scans.expire_before(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(
            expired = n,
            "knowhow: scans that were never answered expired"
        ),
        Err(e) => warn!(%e, "knowhow: unanswered scans were not expired; the next sweep tries"),
    }
    let devices = state.links.devices();
    if devices.is_empty() {
        return;
    }
    match state.scans.requested_for(&devices).await {
        Ok(scans) => {
            for scan in scans {
                if let Some(link) = state.links.get(scan.device_id, scan.tenant_id) {
                    deliver(state, &scan, &link).await;
                }
            }
        }
        Err(e) => warn!(%e, "knowhow: requested scans were not read; the next sweep tries"),
    }
}

/// The sweep, every `secs` seconds on this pod, until the process ends: the
/// scans, then (P4-5) the probes.
pub fn spawn_sweeper(state: KnowhowState, secs: u64) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(secs.max(1))).await;
            sweep_once(&state).await;
            crate::probe::sweep_once(&state).await;
        }
    });
}

// ─── the answer ─────────────────────────────────────────────────────────────

/// An answer, read: what its findings held by kind, its outcome as recorded,
/// and its detail clamped. Only a `done` answer's findings are read, and an
/// answer past its bounds is recorded as failed. Pure, so a test reads it
/// without a socket.
pub fn read_answer(outcome: ScanOutcome, detail: Option<String>, findings: &[Value]) -> Answer {
    let detail = detail.map(|d| {
        if holds_credential(&d) {
            "(withheld: it held a credential's shape)".to_string()
        } else {
            d.chars()
                .filter(|c| !c.is_control())
                .take(MAX_DETAIL)
                .collect()
        }
    });
    let mut answer = Answer {
        counts: ScanCounts::default(),
        outcome,
        detail,
        findings: Vec::new(),
    };
    if outcome != ScanOutcome::Done {
        return answer;
    }
    let bytes = serde_json::to_vec(findings).map_or(usize::MAX, |v| v.len());
    if findings.len() > MAX_FINDINGS || bytes > MAX_SCAN_BYTES {
        answer.outcome = ScanOutcome::Failed;
        answer.detail = Some(format!(
            "the answer held more than {MAX_FINDINGS} findings or {MAX_SCAN_BYTES} bytes"
        ));
        return answer;
    }
    for f in findings {
        let counts = &mut answer.counts;
        match Finding::read(f) {
            Ok(Some(finding)) => {
                match &finding {
                    Finding::Checkout(_) => counts.checkouts += 1,
                    Finding::Context(_) => counts.contexts += 1,
                    Finding::Listener(_) => counts.listeners += 1,
                    Finding::TunnelRoute(_) => counts.tunnel_routes += 1,
                }
                answer.findings.push(finding);
            }
            Ok(None) => counts.unknown += 1,
            Err(_) => counts.refused += 1,
        }
    }
    answer
}

/// An answer, read ([`read_answer`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub counts: ScanCounts,
    pub outcome: ScanOutcome,
    pub detail: Option<String>,
    /// What a `done` answer found, each finding read and checked: what the
    /// intake writes (P4-3a-2).
    pub findings: Vec<Finding>,
}

/// Record a device's answer: once, and only for a scan this device was sent.
async fn answered(
    state: &KnowhowState,
    ctx: &AgentCtx,
    scan_id: ObjectId,
    outcome: ScanOutcome,
    detail: Option<String>,
    truncated: bool,
    findings: Vec<Value>,
) {
    let answer = read_answer(outcome, detail, &findings);
    let word = match answer.outcome {
        ScanOutcome::Done => SCAN_DONE,
        ScanOutcome::NoRoots => SCAN_NO_ROOTS,
        ScanOutcome::Failed => SCAN_FAILED,
    };
    let counts_bson = match bson::to_bson(&answer.counts) {
        Ok(b) => b,
        Err(e) => {
            warn!(scan = %scan_id, %e, "knowhow: an answer's counts were not written");
            return;
        }
    };
    let mut set = doc! {
        "state": word,
        "answered_at": DateTime::now(),
        "counts": counts_bson,
        "truncated": truncated,
    };
    if let Some(d) = &answer.detail {
        set.insert("detail", d.as_str());
    }
    match state
        .scans
        .answer(scan_id, ctx.tenant_id, ctx.agent_id, set)
        .await
    {
        Ok(Some(_)) => info!(
            scan = %scan_id, device = %ctx.agent_id, outcome = word, counts = ?answer.counts,
            "knowhow: a scan was answered"
        ),
        Ok(None) => {
            warn!(
                scan = %scan_id, device = %ctx.agent_id,
                "knowhow: an answer for a scan this device was not sent, or one answered already, dropped"
            );
            return;
        }
        Err(e) => {
            warn!(scan = %scan_id, %e, "knowhow: an answer was not recorded; the scan expires unanswered");
            return;
        }
    }
    // P4-3a-2 — the map, once the answer took: one intake per scan. A
    // device that lists no root has no part of the map; a failed scan
    // changes nothing, and a truncated one archives nothing.
    let found: &[Finding] = match answer.outcome {
        ScanOutcome::Done => &answer.findings,
        ScanOutcome::NoRoots => &[],
        ScanOutcome::Failed => return,
    };
    let complete = !truncated || answer.outcome == ScanOutcome::NoRoots;
    match crate::intake::write(state, ctx.tenant_id, ctx.agent_id, found, complete).await {
        Ok(written) => {
            info!(scan = %scan_id, device = %ctx.agent_id, ?written, "knowhow: a scan's findings were written");
            if let Err(e) = state.scans.written(scan_id, &written).await {
                warn!(scan = %scan_id, %e, "knowhow: what a scan wrote was not recorded on it");
            }
        }
        Err(e) => {
            warn!(scan = %scan_id, %e, "knowhow: a scan's findings were not written; the next scan tries again")
        }
    }
}

/// The device's answers, dispatched here by `ClientMsg::namespace()`.
pub(crate) struct ScanAnswers {
    pub state: KnowhowState,
}

#[async_trait]
impl AgentMsgHandler for ScanAnswers {
    async fn handle(&self, ctx: &AgentCtx, msg: ClientMsg) -> Option<ClientMsg> {
        let ClientMsg::KnowhowScanned {
            scan_id,
            outcome,
            detail,
            truncated,
            findings,
        } = msg
        else {
            return Some(msg);
        };
        let (state, ctx) = (self.state.clone(), ctx.clone());
        // Off the socket's path, as a hello's sync is.
        tokio::spawn(async move {
            answered(&state, &ctx, scan_id, outcome, detail, truncated, findings).await;
        });
        None
    }
}

/// The agent socket's lifecycle: a hello mirrors the device (P4-2a) and
/// links its connection for scans; a closing unlinks it.
pub(crate) struct SocketHooks {
    pub state: KnowhowState,
}

#[async_trait]
impl AgentSocketLifecycle for SocketHooks {
    async fn hello(&self, ctx: &AgentCtx) {
        if !self.state.scope.serves(ctx.tenant_id) {
            return;
        }
        let (state, tid, aid) = (self.state.clone(), ctx.tenant_id, ctx.agent_id);
        tokio::spawn(async move {
            if let Err(e) = crate::sync::device(&state, tid, aid).await {
                warn!(tenant = %tid, device = %aid, %e, "knowhow: a device's hello did not reach its machine");
            }
        });
        let (state, ctx) = (self.state.clone(), ctx.clone());
        tokio::spawn(async move {
            hello(&state, &ctx).await;
        });
    }

    async fn closing(&self, ctx: &AgentCtx) {
        self.state.links.remove(ctx.agent_id, &ctx.conn_id);
    }
}

// ─── the routes ─────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct RequestScan {
    /// The device, by its id.
    pub device: String,
}

/// `POST /api/tenant/{tenant_id}/knowhow/scan` — ask a device to scan
/// (decision 41: `KNOWHOW_EDIT`). **202** with the scan, or the device's
/// pending one: a scan is never doubled. **409** for a device that cannot
/// scan or is a secondary enrollment, refused up front as an update push the
/// device would ignore is (FR-67), since it would never answer.
pub async fn request(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<RequestScan>,
) -> Result<(StatusCode, Json<ScanView>), ApiError> {
    let tid = writer(&state, &tenant_id, &auth).await?;
    let device = parse_id(&body.device, "device")?;
    let agent = match state.agents.find_live_in_tenant(tid, device).await {
        Ok(agent) => agent,
        Err(DaoError::NotFound) => return Err(ApiError::NotFound("no such device".into())),
        Err(e) => return Err(e.into()),
    };
    if let Some(why) = refusal(&agent) {
        return Err(ApiError::Conflict(why.message()));
    }
    let scan = KnowhowScan {
        id: None,
        tenant_id: tid,
        device_id: device,
        requested_by: auth.user_id,
        requested_at: DateTime::now(),
        state: SCAN_REQUESTED.into(),
        pending: Some(true),
        delivered_at: None,
        answered_at: None,
        counts: ScanCounts::default(),
        truncated: false,
        detail: None,
        written: None,
    };
    let id = match state.scans.create(&scan).await {
        Ok(id) => id,
        Err(DaoError::DuplicateKey(_)) => {
            return match state.scans.pending_for(tid, device).await? {
                Some(pending) => Ok((StatusCode::ACCEPTED, Json(ScanView::of(&pending)))),
                None => Err(ApiError::Conflict(
                    "the device's pending scan just ended: ask again".into(),
                )),
            };
        }
        Err(e) => return Err(e.into()),
    };
    info!(scan = %id, tenant = %tid, device = %device, by = %auth.user_id, "knowhow: a scan was asked");
    let scan = KnowhowScan {
        id: Some(id),
        ..scan
    };
    // At once, when the device is connected to this pod; any other pod's
    // sweep finds it otherwise.
    if let Some(link) = state.links.get(device, tid) {
        deliver(&state, &scan, &link).await;
    }
    let now = state.scans.base.find_by_id(id).await.unwrap_or(scan);
    Ok((StatusCode::ACCEPTED, Json(ScanView::of(&now))))
}

#[derive(Deserialize, Debug)]
pub struct ScanQuery {
    pub device: String,
    pub limit: Option<i64>,
}

/// `GET /api/tenant/{tenant_id}/knowhow/scan?device=…` — a device's scans,
/// newest first: `{scans}`. Another organization's device has none.
pub async fn list(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(q): Query<ScanQuery>,
) -> Result<Json<Value>, ApiError> {
    let tid = reader(&state, &tenant_id, &auth).await?;
    let device = parse_id(&q.device, "device")?;
    let limit = limit_of(q.limit)?;
    let scans = state.scans.list_for(tid, device, limit).await?;
    let views: Vec<ScanView> = scans.iter().map(ScanView::of).collect();
    Ok(Json(json!({ "scans": views })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout(path: &str) -> Value {
        json!({ "kind": "checkout", "path": path })
    }

    /// A done answer is counted by kind: what this build does not know is
    /// counted apart, and what it refuses too.
    #[test]
    fn a_done_answer_is_counted_by_kind() {
        let findings = vec![
            checkout("/src/a"),
            checkout("/src/b"),
            json!({"kind": "context", "name": "prod", "server": "https://k8s:6443"}),
            json!({"kind": "listener", "port": 22, "protocol": "tcp", "address": "0.0.0.0"}),
            json!({"kind": "tunnel_route", "id": "db", "kind_": "x"}),
            json!({"kind": "bucket", "name": "logs"}),
            json!("not a finding"),
        ];
        let a = read_answer(ScanOutcome::Done, None, &findings);
        assert_eq!(a.outcome, ScanOutcome::Done);
        assert_eq!(a.detail, None);
        assert_eq!(
            a.counts,
            ScanCounts {
                checkouts: 2,
                contexts: 1,
                listeners: 1,
                tunnel_routes: 0,
                unknown: 1,
                refused: 2,
            }
        );
        let kinds: Vec<&str> = a.findings.iter().map(Finding::kind).collect();
        assert_eq!(
            kinds,
            ["checkout", "checkout", "context", "listener"],
            "what the intake writes: the findings read, in order"
        );
    }

    /// Only a done answer's findings are read: a device that lists no root,
    /// or failed, holds none, whatever it sent.
    #[test]
    fn only_a_done_answers_findings_are_read() {
        for outcome in [ScanOutcome::NoRoots, ScanOutcome::Failed] {
            let a = read_answer(outcome, None, &[checkout("/src/a")]);
            assert_eq!((a.counts, a.outcome), (ScanCounts::default(), outcome));
            assert!(a.findings.is_empty());
        }
    }

    /// An answer past its bounds is recorded as failed, nothing counted.
    #[test]
    fn an_answer_past_its_bounds_failed() {
        let many = vec![checkout("/src/a"); MAX_FINDINGS + 1];
        let a = read_answer(ScanOutcome::Done, None, &many);
        assert_eq!(
            (a.counts, a.outcome),
            (ScanCounts::default(), ScanOutcome::Failed)
        );
        assert!(a.findings.is_empty());
        assert!(a.detail.unwrap().contains("findings"));
        let big = vec![checkout(&"/".repeat(4_000)); 300];
        assert_eq!(
            read_answer(ScanOutcome::Done, None, &big).outcome,
            ScanOutcome::Failed,
            "past {MAX_SCAN_BYTES} bytes"
        );
    }

    /// A detail is clamped and stripped of control characters, and one that
    /// holds a credential's shape is withheld.
    #[test]
    fn a_detail_is_clamped_and_never_a_credential() {
        let long = format!("git said\u{7} {}", "x".repeat(MAX_DETAIL * 2));
        let d = read_answer(ScanOutcome::Failed, Some(long), &[])
            .detail
            .unwrap();
        assert_eq!(d.chars().count(), MAX_DETAIL);
        assert!(!d.contains('\u{7}'));
        let leak = format!("fetch https://dev:{}@git.example.com failed", "hunter2");
        let d = read_answer(ScanOutcome::Failed, Some(leak), &[]).detail;
        assert_eq!(d.unwrap(), "(withheld: it held a credential's shape)");
    }
}
