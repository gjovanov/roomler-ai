// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! P4-4-1 — a session's questions, the server's half (the FR-90 spec, §3e
//! "P4-4, a session's reads and proposals, as designed"): `rc:knowhow.search`
//! and `rc:knowhow.get` from a session's toolbelt, carried by its device, and
//! (P4-4-2) `rc:knowhow.propose`, answered by [`crate::propose`].
//!
//! A question names a session and its fence; the organization and the device
//! come from the socket. The session is believed from its record, never from
//! the frame, as a lease is (`roomler_ai_mod_vault::lease::identify`): this
//! organization's, on this device, at this fence, live. Then its starter's
//! `KNOWHOW_VIEW`, read at every question (decision 48), and the vault's rate.
//!
//! | Piece | Where |
//! |---|---|
//! | one queue a connection, answered in order by one task | [`Questions`], [`hello`] |
//! | who is asking | [`identify`] |
//! | what it may read | [`may_read`] |
//! | the answers, cut to their bounds | [`search`], [`get`], [`bounded`] |
//! | a proposal, within the session's ceiling (decisions 50 and 51) | [`crate::propose`] |
//! | the map is there for this device (decision 49) | [`served`] |
//!
//! ⚠️ Still a map, not a gate: an answer tells a session where things are, and
//! nothing that decides a reach reads it.

use std::time::Duration;

use bson::oid::ObjectId;
use dashmap::DashMap;
use roomler_ai_db::models::role::permissions;
use roomler_ai_mod_hive::model::{AgentSession, SessionStatus};
use roomler_ai_remote_control::knowhow::{
    SessionRefusal, TOOL_GET, TOOL_PROPOSE, TOOL_SEARCH, session_limits,
};
use roomler_ai_remote_control::models::{RpcCap, UpdateRefusal};
use roomler_ai_remote_control::signaling::{ClientMsg, ServerMsg};
use roomler_ai_services::dao::base::DaoError;
use roomler_core::AgentCtx;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, warn};

use crate::KnowhowState;
use crate::dao::Search;
use crate::model::{check_env, check_key, check_kind_word};
use crate::view::NodeView;

/// How long telling a device the map is there may wait on its connection.
const SEND_WAIT: Duration = Duration::from_secs(2);

/// One question, as its frame asked it.
#[derive(Debug)]
pub(crate) enum Question {
    Search {
        req_id: String,
        session_id: ObjectId,
        fence: u64,
        q: String,
        kind: String,
        env: String,
        limit: Option<u32>,
    },
    Get {
        req_id: String,
        session_id: ObjectId,
        fence: u64,
        kind: String,
        key: String,
    },
    /// P4-4-2 — a proposal ([`crate::propose`]).
    Propose {
        req_id: String,
        session_id: ObjectId,
        fence: u64,
        proposal: Value,
        why: String,
    },
}

impl Question {
    fn req_id(&self) -> &str {
        match self {
            Self::Search { req_id, .. }
            | Self::Get { req_id, .. }
            | Self::Propose { req_id, .. } => req_id,
        }
    }
}

/// The connection a question arrived on: whose device, in which organization.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Conn {
    pub tenant_id: ObjectId,
    pub device_id: ObjectId,
}

/// This pod's connections, each with the queue its questions wait in: one
/// task answers a connection's questions in order, so a slow read never holds
/// the socket's read loop, and a flood is answered `busy` instead of queued.
#[derive(Default)]
pub struct Questions {
    conns: DashMap<String, mpsc::Sender<Question>>,
}

impl Questions {
    /// Queue the question `msg` asks, or answer it `busy` when its
    /// connection's queue is full. The frame back when it asks no question.
    pub(crate) fn take(&self, ctx: &AgentCtx, msg: ClientMsg) -> Option<ClientMsg> {
        let question = match msg {
            ClientMsg::KnowhowSearch {
                req_id,
                session_id,
                fence,
                q,
                kind,
                env,
                limit,
            } => Question::Search {
                req_id,
                session_id,
                fence,
                q,
                kind,
                env,
                limit,
            },
            ClientMsg::KnowhowGet {
                req_id,
                session_id,
                fence,
                kind,
                key,
            } => Question::Get {
                req_id,
                session_id,
                fence,
                kind,
                key,
            },
            ClientMsg::KnowhowPropose {
                req_id,
                session_id,
                fence,
                proposal,
                why,
            } => Question::Propose {
                req_id,
                session_id,
                fence,
                proposal,
                why,
            },
            other => return Some(other),
        };
        let queued = match self.conns.get(&ctx.conn_id).map(|q| q.value().clone()) {
            Some(queue) => queue.try_send(question).map_err(|e| match e {
                TrySendError::Full(q) | TrySendError::Closed(q) => q,
            }),
            None => Err(question),
        };
        if let Err(question) = queued {
            debug!(device = %ctx.agent_id, "knowhow: a question found its connection's queue full");
            let _ = ctx
                .tx
                .try_send(refused(question.req_id(), SessionRefusal::Busy));
        }
        None
    }

    /// The connection is closing: dropping its sender ends its task once the
    /// queue is empty.
    pub(crate) fn close(&self, conn_id: &str) {
        self.conns.remove(conn_id);
    }
}

/// A device's hello on this pod: open its connection's queue at once, and
/// tell it, off the socket's path, that the map is there for its sessions
/// when it is (decision 49).
pub(crate) fn hello(state: &KnowhowState, ctx: &AgentCtx) {
    let (tx, rx) = mpsc::channel(session_limits::QUEUE_PER_CONNECTION);
    state.questions.conns.insert(ctx.conn_id.clone(), tx);
    let conn = Conn {
        tenant_id: ctx.tenant_id,
        device_id: ctx.agent_id,
    };
    // Held weakly: a closed or displaced connection's channel is not kept
    // open by the task that answers it.
    tokio::spawn(answer_in_order(state.clone(), conn, rx, ctx.tx.downgrade()));
    let (state, out) = (state.clone(), ctx.tx.downgrade());
    tokio::spawn(async move {
        let Some(frame) = served(&state, conn).await else {
            return;
        };
        let Some(out) = out.upgrade() else { return };
        let sent = tokio::time::timeout(SEND_WAIT, out.send(frame)).await;
        if !matches!(sent, Ok(Ok(()))) {
            debug!(device = %conn.device_id, "knowhow: served could not go out; the next hello says it again");
        }
    });
}

/// `rc:knowhow.served` for the device on `conn`, or `None` when the map is
/// not there for its sessions: knowhow does not serve its organization, its
/// last hello did not advertise `knowhow-tools`, or the connection is a
/// secondary enrollment's (a device runs sessions for its primary alone).
pub(crate) async fn served(state: &KnowhowState, conn: Conn) -> Option<ServerMsg> {
    if !state.scope.serves(conn.tenant_id) {
        return None;
    }
    let agent = match state
        .agents
        .find_live_in_tenant(conn.tenant_id, conn.device_id)
        .await
    {
        Ok(agent) => agent,
        Err(DaoError::NotFound) => return None,
        Err(e) => {
            warn!(device = %conn.device_id, %e, "knowhow: a hello's device was not read; its sessions learn of the map at the next");
            return None;
        }
    };
    if !agent.capabilities.has_rpc(RpcCap::KnowhowTools)
        || agent.capabilities.update_refusal() == Some(UpdateRefusal::PrimaryOrg)
    {
        return None;
    }
    Some(ServerMsg::KnowhowServed {
        tools: vec![
            TOOL_SEARCH.to_string(),
            TOOL_GET.to_string(),
            TOOL_PROPOSE.to_string(),
        ],
    })
}

/// Answer one connection's questions, in the order they came, until it goes.
async fn answer_in_order(
    state: KnowhowState,
    conn: Conn,
    mut rx: mpsc::Receiver<Question>,
    tx: mpsc::WeakSender<ServerMsg>,
) {
    while let Some(question) = rx.recv().await {
        let answer = answer(&state, conn, &question).await;
        let Some(tx) = tx.upgrade() else { break };
        if tx.send(answer).await.is_err() {
            break;
        }
    }
}

/// One question's answer, or its refusal.
pub(crate) async fn answer(state: &KnowhowState, conn: Conn, question: &Question) -> ServerMsg {
    let req_id = question.req_id();
    if req_id.is_empty() || req_id.len() > session_limits::MAX_REQ_ID_LEN {
        return refused(req_id, SessionRefusal::BadRequest);
    }
    let answered = match question {
        Question::Search {
            session_id,
            fence,
            q,
            kind,
            env,
            limit,
            ..
        } => match identify(state, conn, *session_id, *fence).await {
            Ok(_) => search(state, conn.tenant_id, q, kind, env, *limit)
                .await
                .map(|(entities, truncated)| ServerMsg::KnowhowFound {
                    req_id: req_id.to_string(),
                    entities,
                    truncated,
                }),
            Err(why) => Err(why),
        },
        Question::Get {
            session_id,
            fence,
            kind,
            key,
            ..
        } => match identify(state, conn, *session_id, *fence).await {
            Ok(_) => {
                get(state, conn.tenant_id, kind, key)
                    .await
                    .map(|card| ServerMsg::KnowhowCard {
                        req_id: req_id.to_string(),
                        card,
                    })
            }
            Err(why) => Err(why),
        },
        Question::Propose {
            session_id,
            fence,
            proposal,
            why,
            ..
        } => match identify(state, conn, *session_id, *fence).await {
            Ok(_) => crate::propose::propose(state, conn.tenant_id, *session_id, proposal, why)
                .await
                .map(|p| ServerMsg::KnowhowProposed {
                    req_id: req_id.to_string(),
                    what: p.what.to_string(),
                    id: p.id.to_hex(),
                    left: p.left,
                }),
            Err(refusal) => Err(refusal),
        },
    };
    answered.unwrap_or_else(|why| refused(req_id, why))
}

fn refused(req_id: &str, why: SessionRefusal) -> ServerMsg {
    ServerMsg::KnowhowRefused {
        req_id: clip(req_id, session_limits::MAX_REQ_ID_LEN),
        reason: why.wire().to_string(),
    }
}

/// At most `max` bytes of `s`, cut at a character's boundary.
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// The statuses a session asks in: from its start until a stop is ordered,
/// as a lease is held.
pub(crate) fn live(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Starting
            | SessionStatus::Idle
            | SessionStatus::Running
            | SessionStatus::AwaitingApproval
    )
}

/// Who is asking: knowhow serves the organization, and the session's record
/// places it on THIS device, at this fence, live, and its starter may read
/// the map now, within the session's per-minute ceiling. The record, read.
pub(crate) async fn identify(
    state: &KnowhowState,
    conn: Conn,
    session_id: ObjectId,
    fence: u64,
) -> Result<AgentSession, SessionRefusal> {
    if !state.scope.serves(conn.tenant_id) {
        return Err(SessionRefusal::NotServed);
    }
    let session = match state
        .sessions
        .find_in_tenant(conn.tenant_id, session_id)
        .await
    {
        Ok(s) if s.location.device_id == conn.device_id => s,
        Ok(_) | Err(DaoError::NotFound) => {
            warn!(
                device = %conn.device_id, session = %session_id,
                "knowhow: a question for a session this device does not run"
            );
            return Err(SessionRefusal::NotLocated);
        }
        Err(e) => {
            warn!(session = %session_id, %e, "knowhow: a question's session was not read");
            return Err(SessionRefusal::Unavailable);
        }
    };
    if u64::try_from(session.fence).ok() != Some(fence) {
        debug!(session = %session_id, fence, "knowhow: a question from a run at an older fence");
        return Err(SessionRefusal::StaleFence);
    }
    if !live(session.status) {
        return Err(SessionRefusal::Ended);
    }
    may_read(state, conn.tenant_id, session.owner_id).await?;
    if !state
        .session_limiter
        .check(session_id, conn.device_id, session_limits::RATE_PER_MINUTE)
    {
        return Err(SessionRefusal::Busy);
    }
    Ok(session)
}

/// Decision 48: what the session's starter may read, now: `KNOWHOW_VIEW`, or
/// `KNOWHOW_EDIT`, which includes it, read from their roles at every
/// question. A starter who left the organization reads nothing.
pub(crate) async fn may_read(
    state: &KnowhowState,
    tenant_id: ObjectId,
    starter: ObjectId,
) -> Result<(), SessionRefusal> {
    match state
        .tenants
        .get_member_permissions(tenant_id, starter)
        .await
    {
        Ok(perms)
            if permissions::has(perms, permissions::KNOWHOW_VIEW)
                || permissions::has(perms, permissions::KNOWHOW_EDIT) =>
        {
            Ok(())
        }
        Ok(_) | Err(DaoError::NotAMember) => Err(SessionRefusal::Denied),
        Err(e) => {
            warn!(tenant = %tenant_id, %e, "knowhow: a session's starter's roles were not read");
            Err(SessionRefusal::Unavailable)
        }
    }
}

/// The people's search, for a session: the live entities whose key or name
/// holds `q`, of a `kind`, in an `env`, each as a listing shows one; at most
/// [`session_limits::MAX_SEARCH`], and whether there were more.
pub(crate) async fn search(
    state: &KnowhowState,
    tenant_id: ObjectId,
    q: &str,
    kind: &str,
    env: &str,
    limit: Option<u32>,
) -> Result<(Vec<Value>, bool), SessionRefusal> {
    let limit = match limit {
        None | Some(0) => session_limits::DEFAULT_SEARCH,
        Some(n) if n <= session_limits::MAX_SEARCH => n,
        Some(_) => return Err(SessionRefusal::BadRequest),
    };
    let text = given(q);
    if text.is_some_and(|q| q.chars().count() > session_limits::MAX_QUERY_CHARS) {
        return Err(SessionRefusal::BadRequest);
    }
    let kind = given(kind)
        .map(check_kind_word)
        .transpose()
        .map_err(|_| SessionRefusal::BadRequest)?;
    let env = given(env)
        .map(check_env)
        .transpose()
        .map_err(|_| SessionRefusal::BadRequest)?;
    let mut found = state
        .entities
        .search(tenant_id, Search { text, kind, env }, i64::from(limit) + 1)
        .await
        .map_err(|e| {
            warn!(tenant = %tenant_id, %e, "knowhow: a session's search was not read");
            SessionRefusal::Unavailable
        })?;
    let truncated = found.len() > limit as usize;
    found.truncate(limit as usize);
    let entities = found
        .iter()
        .map(|n| serde_json::to_value(NodeView::from(n)).unwrap_or(Value::Null))
        .collect();
    Ok((entities, truncated))
}

/// A filter as a frame carries it: empty, or only spaces, is none.
fn given(s: &str) -> Option<&str> {
    Some(s.trim()).filter(|s| !s.is_empty())
}

/// The people's card, for a session: one live entity by its kind and key,
/// with at most [`session_limits::MAX_CARD_LINKS`] of its links, cut until
/// the whole fits [`session_limits::MAX_CARD_BYTES`].
pub(crate) async fn get(
    state: &KnowhowState,
    tenant_id: ObjectId,
    kind: &str,
    key: &str,
) -> Result<Value, SessionRefusal> {
    let kind = check_kind_word(kind.trim()).map_err(|_| SessionRefusal::BadRequest)?;
    let key = check_key(key.trim()).map_err(|_| SessionRefusal::BadRequest)?;
    let entity = match state.entities.find_live_by_key(tenant_id, kind, key).await {
        Ok(Some(e)) => e,
        Ok(None) => return Err(SessionRefusal::NotFound),
        Err(e) => {
            warn!(tenant = %tenant_id, %e, "knowhow: a session's card was not read");
            return Err(SessionRefusal::Unavailable);
        }
    };
    let card = crate::routes::card(state, tenant_id, entity, session_limits::MAX_CARD_LINKS)
        .await
        .map_err(|e| {
            warn!(tenant = %tenant_id, ?e, "knowhow: a session's card's links were not read");
            SessionRefusal::Unavailable
        })?;
    let card = serde_json::to_value(card).map_err(|_| SessionRefusal::Unavailable)?;
    Ok(bounded(card, session_limits::MAX_CARD_BYTES))
}

/// A card cut to `max` bytes, serialised: its links dropped from the end, and
/// `truncated` set, until it fits or none is left. The entity itself is never
/// cut: its own bounds (16 KiB of attributes, 64 facts of 2 KiB) hold it.
pub(crate) fn bounded(mut card: Value, max: usize) -> Value {
    let size = |v: &Value| serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX);
    while size(&card) > max {
        let Some(links) = card.get_mut("links").and_then(Value::as_array_mut) else {
            break;
        };
        if links.pop().is_none() {
            break;
        }
        card["truncated"] = Value::Bool(true);
    }
    card
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn card(links: usize, each: usize) -> Value {
        json!({
            "entity": {"kind": "database", "key": "prod-mongo", "name": "prod mongo"},
            "links": (0..links)
                .map(|i| json!({"direction": "out", "n": i, "pad": "x".repeat(each)}))
                .collect::<Vec<_>>(),
            "truncated": false,
        })
    }

    /// A card within its bound is as it was; one past it loses links from the
    /// end, says so, and fits.
    #[test]
    fn a_card_is_cut_from_its_last_link_until_it_fits() {
        let small = card(3, 10);
        assert_eq!(bounded(small.clone(), 64 * 1024), small);

        let big = bounded(card(50, 4 * 1024), 64 * 1024);
        let links = big["links"].as_array().unwrap();
        assert!(links.len() < 50 && !links.is_empty(), "{}", links.len());
        assert_eq!(links[0]["n"], 0, "cut from the end");
        assert_eq!(big["truncated"], true);
        assert!(serde_json::to_vec(&big).unwrap().len() <= 64 * 1024);
    }

    /// An entity too large for the bound on its own keeps itself whole: only
    /// links are ever cut.
    #[test]
    fn an_entity_is_never_cut() {
        let mut huge = card(2, 10);
        huge["entity"]["attrs"] = json!({"blob": "y".repeat(70 * 1024)});
        let cut = bounded(huge, 64 * 1024);
        assert!(cut["links"].as_array().unwrap().is_empty());
        assert_eq!(cut["truncated"], true);
        assert_eq!(cut["entity"]["key"], "prod-mongo");
    }

    /// A session asks from its start until a stop is ordered.
    #[test]
    fn a_session_asks_while_it_is_live() {
        for status in [
            SessionStatus::Starting,
            SessionStatus::Idle,
            SessionStatus::Running,
            SessionStatus::AwaitingApproval,
        ] {
            assert!(live(status), "{status:?}");
        }
        for status in [
            SessionStatus::Stopping,
            SessionStatus::Ended,
            SessionStatus::Refused,
            SessionStatus::Lost,
        ] {
            assert!(!live(status), "{status:?}");
        }
    }

    /// A connection's context, its answers read from the receiver.
    fn ctx() -> (AgentCtx, mpsc::Receiver<ServerMsg>) {
        let (tx, rx) = mpsc::channel(4);
        let ctx = AgentCtx {
            conn_id: "c1".into(),
            agent_id: ObjectId::new(),
            tenant_id: ObjectId::new(),
            owner_user_id: ObjectId::new(),
            agent_version: "0.4.159".into(),
            os: roomler_ai_remote_control::models::OsKind::Linux,
            dialed_tid: None,
            conn_established_ms: 0,
            tx,
        };
        (ctx, rx)
    }

    /// A frame that asks no question goes back untouched, for the Hub; a
    /// question on a connection with no queue (its hello not yet run here)
    /// is answered `busy`, never dropped.
    #[test]
    fn a_frame_that_asks_nothing_is_handed_back() {
        let (ctx, mut rx) = ctx();
        let questions = Questions::default();
        let scanned = ClientMsg::KnowhowScanned {
            scan_id: ObjectId::new(),
            outcome: roomler_ai_remote_control::knowhow::ScanOutcome::NoRoots,
            detail: None,
            truncated: false,
            findings: Vec::new(),
        };
        assert!(matches!(
            questions.take(&ctx, scanned),
            Some(ClientMsg::KnowhowScanned { .. })
        ));
        let get = ClientMsg::KnowhowGet {
            req_id: "q1".into(),
            session_id: ObjectId::new(),
            fence: 1,
            kind: "database".into(),
            key: "prod-mongo".into(),
        };
        assert!(questions.take(&ctx, get).is_none(), "a question is taken");
        match rx.try_recv() {
            Ok(ServerMsg::KnowhowRefused { req_id, reason }) => {
                assert_eq!((req_id.as_str(), reason.as_str()), ("q1", "busy"));
            }
            other => panic!("not a refusal: {other:?}"),
        }
    }

    /// A refusal names its question, clipped to the bound a device keeps.
    #[test]
    fn a_refusal_names_its_question_within_the_bound() {
        let long = "r".repeat(100);
        let ServerMsg::KnowhowRefused { req_id, reason } = refused(&long, SessionRefusal::Busy)
        else {
            panic!("not a refusal");
        };
        assert_eq!(req_id.len(), session_limits::MAX_REQ_ID_LEN);
        assert_eq!(reason, "busy");
    }
}
