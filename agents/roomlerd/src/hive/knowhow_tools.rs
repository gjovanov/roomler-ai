// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-4-1b — the toolbelt's knowhow tools, the device's half (the FR-90
//! spec, §3e "P4-4, a session's reads and proposals, as designed";
//! `docs/knowhow.md` §12).
//!
//! A session asks the organization's map through `knowhow_search` and
//! `knowhow_get`, says what it learned through `knowhow_propose` (P4-4-2b),
//! and has this device probe an entity through `knowhow_verify` (P4-5-2b).
//! The daemon carries each question on its primary connection
//! (`rc:knowhow.search`, `.get`, `.propose`, `.verify`), naming the session
//! and the fence it runs at; the server believes the session from its record,
//! answers what its starter may read, writes a proposal `proposed`, and asks
//! a verify's probe of this device.
//!
//! | Piece | Where |
//! |---|---|
//! | the server's word that the map is there, and which tools it answers | [`on_served`] |
//! | a question on the primary's connection, its answer matched to it | [`Client`] |
//! | a verify's second answer, what its probe measured, matched by the probe | [`on_verified`] |
//! | what a run's toolbelt offers, fixed at its launch | [`daemon_knowhow`] |
//! | the tools, as a session sees them, and which ask a person | [`tools`], [`call`], [`allowed`], [`asked`] |
//!
//! ⚠️ Offered only on the server's word (decision 49): an unmounted knowhow
//! never answers, so a tool shown without that word would wait out its
//! deadline on every call. A device forgets the word at every connection.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use bson::oid::ObjectId;
use roomler_ai_remote_control::knowhow::{
    ProbeKind, SessionRefusal, TOOL_GET, TOOL_PROPOSE, TOOL_SEARCH, TOOL_VERIFY, session_limits,
};
use roomler_ai_remote_control::signaling::ClientMsg;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

/// How long a question waits for its answer.
pub(crate) const ANSWER_DEADLINE: Duration = Duration::from_secs(15);

/// P4-5-2b — how long a verify waits for what its probe measured, once the
/// probe was asked: this device's prober takes ten seconds at most
/// (`probe_limits::TIMEOUT_SECS`), and the server records its answer before
/// sending it back.
pub(crate) const VERIFY_DEADLINE: Duration = Duration::from_secs(30);

/// P4-5-2b — the probes' results kept while no verify waits for them yet.
/// A probe goes out as soon as it is asked, so what it measured can come
/// back before its `rc:knowhow.verifying` has reached the waiting call.
const RECENT_VERIFIED: usize = 16;

/// Every knowhow tool this build knows, in the order a session is shown them.
const KNOWN: [&str; 4] = [TOOL_SEARCH, TOOL_GET, TOOL_PROPOSE, TOOL_VERIFY];

/// The three things a session may propose, by the key that names each.
const PROPOSALS: [&str; 3] = ["entity", "fact", "link"];

/// What the map answered one question.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Answer {
    Found {
        entities: Vec<Value>,
        truncated: bool,
    },
    Card(Value),
    /// P4-4-2b — a proposal, written: what it was, the entity or link it
    /// wrote, and the proposals the session has left.
    Proposed {
        what: String,
        id: String,
        left: u32,
    },
    /// P4-5-2b — a verify, asked: the probe this device is sent.
    Verifying {
        probe_id: String,
    },
    /// P4-5-2b — what that probe measured, the probe as the server records
    /// it; `None` when it did not come back within [`VERIFY_DEADLINE`].
    Measured {
        probe_id: String,
        probe: Option<Value>,
    },
    Refused(String),
}

/// Why a question got no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskError {
    /// No primary connection, or its queue is full.
    NotConnected,
    /// Nothing came back within [`ANSWER_DEADLINE`], or the connection it
    /// went out on is gone.
    NoAnswer,
}

/// One daemon's questions: the process has one ([`CLIENT`]), a test makes
/// its own.
#[derive(Default)]
pub(crate) struct Client {
    /// The primary connection's outbound queue, from its hello on.
    outbound: Mutex<Option<mpsc::Sender<ClientMsg>>>,
    /// The tools the server said it answers on that connection
    /// (`rc:knowhow.served`); none until it says so.
    served: Mutex<Vec<&'static str>>,
    pending: Mutex<HashMap<String, oneshot::Sender<Answer>>>,
    /// P4-5-2b — what probes measured, by probe id.
    verified: Mutex<Verified>,
}

/// P4-5-2b — the verifies waiting for what their probe measured, and what
/// came back before any waited for it. One lock for both, so a result is
/// either given to a waiter or kept for the next, never neither.
#[derive(Default)]
struct Verified {
    waiting: HashMap<String, Vec<oneshot::Sender<Value>>>,
    recent: VecDeque<(String, Value)>,
}

/// The daemon's.
pub(crate) static CLIENT: LazyLock<Client> = LazyLock::new(Client::default);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Forgets one question when its wait ends, however it ends.
struct Forget<'a> {
    client: &'a Client,
    req_id: &'a str,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        lock(&self.client.pending).remove(self.req_id);
    }
}

/// P4-5-2b — lets go of a verify's wait for its probe when the wait ends,
/// however it ends: only senders whose waiter is gone, so another verify
/// waiting for the same probe keeps waiting.
struct ForgetProbe<'a> {
    client: &'a Client,
    probe_id: &'a str,
}

impl Drop for ForgetProbe<'_> {
    fn drop(&mut self) {
        let mut v = lock(&self.client.verified);
        if let Some(waiters) = v.waiting.get_mut(self.probe_id) {
            waiters.retain(|w| !w.is_closed());
            if waiters.is_empty() {
                v.waiting.remove(self.probe_id);
            }
        }
    }
}

fn new_req_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

impl Client {
    /// A primary connection is up: questions go out on it, and the server's
    /// word that the map is there is forgotten until it says it again. A
    /// question still waiting was asked on a connection that is gone, and is
    /// answered never: its waiter ends now.
    pub(crate) fn connected(&self, tx: mpsc::Sender<ClientMsg>) {
        *lock(&self.outbound) = Some(tx);
        lock(&self.served).clear();
        lock(&self.pending).clear();
        // A verify waiting for its probe ends too: it was asked on the
        // connection that is gone, and its probe is recorded when the
        // device answers it, whichever connection that is on.
        *lock(&self.verified) = Verified::default();
    }

    /// The server's word (decision 49): the tools it answers, each kept
    /// only when this build knows it.
    pub(crate) fn served(&self, tools: &[String]) {
        *lock(&self.served) = KNOWN
            .into_iter()
            .filter(|t| tools.iter().any(|s| s == t))
            .collect();
    }

    /// The tools the server answers now.
    pub(crate) fn offered(&self) -> Vec<&'static str> {
        lock(&self.served).clone()
    }

    /// An answer: given to the question it names, or dropped when none
    /// waits for it (one past its deadline, or from another connection).
    pub(crate) fn answered(&self, req_id: &str, answer: Answer) {
        match lock(&self.pending).remove(req_id) {
            Some(waiter) => {
                let _ = waiter.send(answer);
            }
            None => debug!(req_id, "knowhow: an answer no question waits for"),
        }
    }

    /// Ask, and wait for the answer.
    async fn ask(&self, req_id: &str, frame: ClientMsg) -> Result<Answer, AskError> {
        let Some(tx) = lock(&self.outbound).clone() else {
            return Err(AskError::NotConnected);
        };
        let (reply, answer) = oneshot::channel();
        lock(&self.pending).insert(req_id.to_string(), reply);
        // However the wait ends, its question waits no longer: answered,
        // past its deadline, or its asker gone (a cancelled tool call drops
        // this future), so nothing is left for a server that never answers.
        let _forget = Forget {
            client: self,
            req_id,
        };
        if tx.try_send(frame).is_err() {
            return Err(AskError::NotConnected);
        }
        match tokio::time::timeout(ANSWER_DEADLINE, answer).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) | Err(_) => Err(AskError::NoAnswer),
        }
    }

    /// `rc:knowhow.search`, as `session` at `fence`.
    pub(crate) async fn search(
        &self,
        session: ObjectId,
        fence: u64,
        ask: &Search,
    ) -> Result<Answer, AskError> {
        let req_id = new_req_id();
        let frame = ClientMsg::KnowhowSearch {
            req_id: req_id.clone(),
            session_id: session,
            fence,
            q: ask.q.clone(),
            kind: ask.kind.clone(),
            env: ask.env.clone(),
            limit: ask.limit,
        };
        self.ask(&req_id, frame).await
    }

    /// `rc:knowhow.get`, as `session` at `fence`.
    pub(crate) async fn get(
        &self,
        session: ObjectId,
        fence: u64,
        kind: &str,
        key: &str,
    ) -> Result<Answer, AskError> {
        let req_id = new_req_id();
        let frame = ClientMsg::KnowhowGet {
            req_id: req_id.clone(),
            session_id: session,
            fence,
            kind: kind.to_string(),
            key: key.to_string(),
        };
        self.ask(&req_id, frame).await
    }

    /// P4-4-2b — `rc:knowhow.propose`, as `session` at `fence`.
    pub(crate) async fn propose(
        &self,
        session: ObjectId,
        fence: u64,
        proposal: &Value,
        why: &str,
    ) -> Result<Answer, AskError> {
        let req_id = new_req_id();
        let frame = ClientMsg::KnowhowPropose {
            req_id: req_id.clone(),
            session_id: session,
            fence,
            proposal: proposal.clone(),
            why: why.to_string(),
        };
        self.ask(&req_id, frame).await
    }

    /// P4-5-2b — `rc:knowhow.verify`, as `session` at `fence`: the server's
    /// answer, and once it asked the probe ([`Answer::Verifying`]), what the
    /// probe measured ([`Answer::Measured`]).
    pub(crate) async fn verify(
        &self,
        session: ObjectId,
        fence: u64,
        kind: &str,
        key: &str,
        probe: ProbeKind,
    ) -> Result<Answer, AskError> {
        let req_id = new_req_id();
        let frame = ClientMsg::KnowhowVerify {
            req_id: req_id.clone(),
            session_id: session,
            fence,
            kind: kind.to_string(),
            key: key.to_string(),
            probe: probe.as_str().to_string(),
        };
        match self.ask(&req_id, frame).await? {
            Answer::Verifying { probe_id } => {
                let probe = self.measured(&probe_id).await;
                Ok(Answer::Measured { probe_id, probe })
            }
            other => Ok(other),
        }
    }

    /// What a probe measured (`rc:knowhow.verified`): given to the verifies
    /// waiting for it, or kept for the one about to, the oldest let go past
    /// [`RECENT_VERIFIED`].
    pub(crate) fn verified(&self, probe_id: &str, probe: Value) {
        let mut v = lock(&self.verified);
        match v.waiting.remove(probe_id) {
            Some(waiters) => {
                for w in waiters {
                    let _ = w.send(probe.clone());
                }
            }
            None => {
                if v.recent.len() >= RECENT_VERIFIED {
                    v.recent.pop_front();
                }
                v.recent.push_back((probe_id.to_string(), probe));
            }
        }
    }

    /// Wait for what the probe `probe_id` measured, at most
    /// [`VERIFY_DEADLINE`]: at once when it came back already.
    async fn measured(&self, probe_id: &str) -> Option<Value> {
        // Declared first, so it is dropped after the receiver below: its
        // sender is closed by then, and only closed ones are let go.
        let _forget = ForgetProbe {
            client: self,
            probe_id,
        };
        let answer = {
            let mut v = lock(&self.verified);
            if let Some(at) = v.recent.iter().position(|(id, _)| id == probe_id) {
                return v.recent.remove(at).map(|(_, probe)| probe);
            }
            let (tx, rx) = oneshot::channel();
            v.waiting.entry(probe_id.to_string()).or_default().push(tx);
            rx
        };
        // Bound first, so the receiver is gone before `_forget` looks.
        let got = tokio::time::timeout(VERIFY_DEADLINE, answer).await;
        got.ok().and_then(Result::ok)
    }

    /// The questions waiting for an answer.
    #[cfg(test)]
    fn waiting(&self) -> usize {
        lock(&self.pending).len()
    }

    /// The verifies waiting for a probe, and the results kept for none.
    #[cfg(test)]
    fn verifies(&self) -> (usize, usize) {
        let v = lock(&self.verified);
        (v.waiting.values().map(Vec::len).sum(), v.recent.len())
    }
}

// ─── the signaling loop's side ──────────────────────────────────────────────

/// A primary connection is up (`run_ws`, at each hello).
pub fn on_connected(tx: mpsc::Sender<ClientMsg>) {
    CLIENT.connected(tx);
}

/// `rc:knowhow.served` on the primary's connection.
pub fn on_served(tools: &[String]) {
    CLIENT.served(tools);
}

/// `rc:knowhow.found`.
pub fn on_found(req_id: &str, entities: Vec<Value>, truncated: bool) {
    CLIENT.answered(
        req_id,
        Answer::Found {
            entities,
            truncated,
        },
    );
}

/// `rc:knowhow.card`.
pub fn on_card(req_id: &str, card: Value) {
    CLIENT.answered(req_id, Answer::Card(card));
}

/// P4-4-2b — `rc:knowhow.proposed`.
pub fn on_proposed(req_id: &str, what: String, id: String, left: u32) {
    CLIENT.answered(req_id, Answer::Proposed { what, id, left });
}

/// P4-5-2b — `rc:knowhow.verifying`: a verify's probe, asked.
pub fn on_verifying(req_id: &str, probe_id: String) {
    CLIENT.answered(req_id, Answer::Verifying { probe_id });
}

/// P4-5-2b — `rc:knowhow.verified`: what a verify's probe measured.
pub fn on_verified(probe_id: &str, probe: Value) {
    CLIENT.verified(probe_id, probe);
}

/// `rc:knowhow.refused`.
pub fn on_refused(req_id: &str, reason: String) {
    CLIENT.answered(req_id, Answer::Refused(reason));
}

// ─── the toolbelt's side ────────────────────────────────────────────────────

/// A search, as a session asks it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Search {
    pub q: String,
    pub kind: String,
    pub env: String,
    pub limit: Option<u32>,
}

/// What a run's knowhow tools ask the map. The daemon's goes to the server
/// on the primary's connection ([`daemon_knowhow`]); a test's stands in for
/// it.
#[async_trait::async_trait]
pub(crate) trait KnowhowTools: Send + Sync {
    /// The tools this run offers: those its server answered for at the
    /// run's launch.
    fn offered(&self) -> Vec<&'static str>;
    async fn search(&self, session: ObjectId, fence: u64, ask: &Search) -> Result<Value, String>;
    async fn get(
        &self,
        session: ObjectId,
        fence: u64,
        kind: &str,
        key: &str,
    ) -> Result<Value, String>;
    /// P4-4-2b — `proposal` holds one key: `entity`, `fact` or `link`.
    async fn propose(
        &self,
        session: ObjectId,
        fence: u64,
        proposal: &Value,
        why: &str,
    ) -> Result<Value, String>;
    /// P4-5-2b — probe the entity by `kind` and `key` from this device.
    async fn verify(
        &self,
        session: ObjectId,
        fence: u64,
        kind: &str,
        key: &str,
        probe: ProbeKind,
    ) -> Result<Value, String>;
}

/// The daemon's knowhow tools for a run, where its server says the map is
/// there; none otherwise, and no knowhow tool is offered. What is offered is
/// fixed at the launch, as the vault's tools are: a session's tool list is
/// read once.
pub(crate) fn daemon_knowhow() -> Option<Arc<dyn KnowhowTools>> {
    let offered = CLIENT.offered();
    if offered.is_empty() {
        return None;
    }
    Some(Arc::new(DaemonKnowhow { offered }))
}

struct DaemonKnowhow {
    offered: Vec<&'static str>,
}

#[async_trait::async_trait]
impl KnowhowTools for DaemonKnowhow {
    fn offered(&self) -> Vec<&'static str> {
        self.offered.clone()
    }

    async fn search(&self, session: ObjectId, fence: u64, ask: &Search) -> Result<Value, String> {
        read(CLIENT.search(session, fence, ask).await)
    }

    async fn get(
        &self,
        session: ObjectId,
        fence: u64,
        kind: &str,
        key: &str,
    ) -> Result<Value, String> {
        read(CLIENT.get(session, fence, kind, key).await)
    }

    async fn propose(
        &self,
        session: ObjectId,
        fence: u64,
        proposal: &Value,
        why: &str,
    ) -> Result<Value, String> {
        match CLIENT.propose(session, fence, proposal, why).await {
            // Asked, and never answered: the server may have written it all
            // the same, and asking again is safe, since one already there is
            // refused `exists`.
            Err(AskError::NoAnswer) => Err(
                "the organization's map did not answer: the proposal may have been written all \
                 the same, and knowhow_get shows whether it was"
                    .into(),
            ),
            answer => read(answer),
        }
    }

    async fn verify(
        &self,
        session: ObjectId,
        fence: u64,
        kind: &str,
        key: &str,
        probe: ProbeKind,
    ) -> Result<Value, String> {
        match CLIENT.verify(session, fence, kind, key, probe).await {
            // Asked, and never answered: the probe may have been asked all
            // the same, and what it measured is recorded when it ends.
            Err(AskError::NoAnswer) => Err(
                "the organization's map did not answer: the probe may have been asked all the \
                 same, and knowhow_get shows what it measured once it has"
                    .into(),
            ),
            answer => read(answer),
        }
    }
}

/// An answer as the model reads it: what the map said, or why it said
/// nothing.
fn read(answer: Result<Answer, AskError>) -> Result<Value, String> {
    match answer {
        Ok(Answer::Found {
            entities,
            truncated,
        }) => Ok(json!({ "entities": entities, "truncated": truncated })),
        Ok(Answer::Card(card)) => Ok(card),
        Ok(Answer::Proposed { what, id, left }) => Ok(json!({
            "proposed": what,
            "id": id,
            "status": "proposed",
            "proposals_left": left,
        })),
        Ok(Answer::Measured {
            probe_id,
            probe: Some(probe),
        }) => Ok(measured(&probe_id, &probe)),
        Ok(Answer::Measured {
            probe_id,
            probe: None,
        })
        | Ok(Answer::Verifying { probe_id }) => Ok(json!({
            "probe_id": probe_id,
            "state": "pending",
            "means": format!(
                "the probe was asked, and had not answered within {} s: what it measures is \
                 recorded on the entity's facts when it does, and knowhow_get shows it",
                VERIFY_DEADLINE.as_secs()
            ),
        })),
        Ok(Answer::Refused(word)) => Err(refusal(&word)),
        Err(AskError::NotConnected) => Err("this device is not connected to its server".into()),
        Err(AskError::NoAnswer) => Err("the organization's map did not answer".into()),
    }
}

/// P4-5-2b — what a probe measured, as the model reads it: its outcome and
/// what that means, where it went, and what it found; none of the record's
/// ids, which name things the model never asks for.
fn measured(probe_id: &str, probe: &Value) -> Value {
    let state = probe["state"].as_str().unwrap_or("unknown");
    let means = match state {
        "ok" => {
            "it answered: each fact it measured is now marked verified, ok where the fact says \
             what it found (a proposed one turned active), not ok where it differs"
        }
        "unreachable" => {
            "nothing answered at that host and port from this device: the host and port facts \
             are recorded not ok, their status kept"
        }
        "not_allowed" => {
            "this device may not reach it: its owner has not listed it in knowhow_probe_reach, \
             or it is an address never probed. Nothing was recorded on the entity's facts"
        }
        "failed" => {
            "it could not be measured, as the detail says. Nothing was recorded on the entity's \
             facts"
        }
        _ => "the probe ended without an answer",
    };
    let mut out = Map::new();
    out.insert("probe_id".into(), json!(probe_id));
    out.insert("state".into(), json!(state));
    out.insert("means".into(), json!(means));
    for key in [
        "kind", "target", "rtt_ms", "address", "carrier", "tls", "detail", "measured",
    ] {
        if let Some(v) = probe.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    Value::Object(out)
}

/// What the model is told for each refusal: what it means, and whether
/// asking again could help.
pub(crate) fn refusal(word: &str) -> String {
    match SessionRefusal::from_wire(word) {
        Some(SessionRefusal::NotServed) => {
            "the organization's map is not kept for this organization".into()
        }
        Some(SessionRefusal::BadRequest) => format!(
            "the map refused the question as malformed or past a bound: a kind and a key \
             are lowercase, a search's text is at most {} characters, its limit 1 to {}",
            session_limits::MAX_QUERY_CHARS,
            session_limits::MAX_SEARCH
        ),
        Some(SessionRefusal::NotLocated) => {
            "the map does not know this session on this device".into()
        }
        Some(SessionRefusal::StaleFence) => {
            "this session runs elsewhere now: this run no longer holds it".into()
        }
        Some(SessionRefusal::Ended) => "this session is over".into(),
        Some(SessionRefusal::Denied) => {
            "the person who started this session may not do that with the organization's \
             map: reading it needs KNOWHOW_VIEW in their role, asking a probe KNOWHOW_EDIT"
                .into()
        }
        Some(SessionRefusal::NotFound) => {
            "nothing on the map by that kind and key: knowhow_search finds what is there".into()
        }
        Some(SessionRefusal::Busy) => "the map is busy: ask again in a moment".into(),
        Some(SessionRefusal::Unavailable) => {
            "the map could not be read just now: ask again later".into()
        }
        Some(SessionRefusal::Exists) => {
            "that is on the map already: a proposal adds, it never changes what is there \
             (knowhow_get shows it)"
                .into()
        }
        Some(SessionRefusal::Reserved) => {
            "that part of the map is a device's: the fleet and the device's own scans keep \
             machines, their checkouts and listeners, and what they say of them"
                .into()
        }
        Some(SessionRefusal::Refused) => {
            "the proposal holds text in a credential's shape, so nothing of it was written: \
             the map never keeps a secret"
                .into()
        }
        Some(SessionRefusal::Full) => {
            "the map, or that entity, holds its most entries: nothing more fits".into()
        }
        Some(SessionRefusal::Ceiling) => format!(
            "this session has made its most proposals ({}); say what else it learned in its \
             own words instead",
            session_limits::MAX_PROPOSALS
        ),
        Some(SessionRefusal::Unprobeable) => {
            "that entity's facts say nowhere a probe reaches: it needs an address or a host, \
             and a port, and a link-local, cloud metadata or listener-bind address is never \
             probed"
                .into()
        }
        None => format!("the map refused: {word}"),
    }
}

/// The tools a run offers that its settings allow with no person's approval
/// (decision 39): the reads and a proposal, which the server holds to what
/// the session's starter may read and writes `proposed`. Never
/// `knowhow_verify` ([`asked`]).
pub(crate) fn allowed(offered: &[&'static str]) -> Vec<String> {
    offered
        .iter()
        .filter(|t| **t != TOOL_VERIFY)
        .map(|t| settings_name(t))
        .collect()
}

/// P4-5-2b — the tools a run offers that its settings ask a person for at
/// each call (decision 54): `knowhow_verify`, which aims a connection from
/// this device, as the session's egress and its commands on other devices
/// ask (`net_socks`, `ssh_run`).
pub(crate) fn asked(offered: &[&'static str]) -> Vec<String> {
    offered
        .iter()
        .filter(|t| **t == TOOL_VERIFY)
        .map(|t| settings_name(t))
        .collect()
}

/// A tool as a session's settings name it.
fn settings_name(tool: &str) -> String {
    format!(
        "mcp__{}__{tool}",
        roomler_hive_node::launch::TOOLBELT_SERVER
    )
}

/// The tools a run offers, as a session is shown them.
pub(crate) fn tools(offered: &[&'static str]) -> Vec<Value> {
    let mut out = Vec::new();
    if offered.contains(&TOOL_SEARCH) {
        out.push(json!({
            "name": TOOL_SEARCH,
            "description": format!(
                "Search the organization's map (Roomler knowhow): its machines, repositories \
                 and their checkouts, databases, clusters, services and endpoints, by text in \
                 their key or name, by kind, by environment. Each entity comes back by its \
                 kind and key, with its status: `active`, or `proposed` (a session's claim no \
                 person or probe has confirmed). The map says where things are; it grants \
                 nothing, and reaching any of them is still asked of the vault and the \
                 network. At most {} a search.",
                session_limits::MAX_SEARCH
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "q": {
                        "type": "string",
                        "description": "text the key or name holds, case aside; none lists every entity",
                        "maxLength": session_limits::MAX_QUERY_CHARS,
                    },
                    "kind": {
                        "type": "string",
                        "description": "only this kind: machine, repo, checkout, database, cluster, namespace, service, endpoint, bucket, cloud_account, domain, environment, team, doc",
                    },
                    "env": {
                        "type": "string",
                        "description": "only this environment, one lowercase word (prod, staging)",
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": session_limits::MAX_SEARCH,
                    },
                },
            },
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false},
        }));
    }
    if offered.contains(&TOOL_GET) {
        out.push(json!({
            "name": TOOL_GET,
            "description": "One entity's card from the organization's map, by its kind and key as \
                            knowhow_search lists them: its facts, each with where it came from \
                            (a person, the fleet's sync, a device's scan) and its status, and its \
                            links to other entities. A machine is keyed by its device's id, with \
                            its overlay address and names among its facts. A fact a probe \
                            checked carries `verified`, the last probe's word; `probes` holds \
                            each device's last probe of the entity that found something, so a \
                            device whose network sees it otherwise shows beside the rest.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "description": "the entity's kind, as knowhow_search shows it"},
                    "key": {"type": "string", "description": "the entity's key, as knowhow_search shows it"},
                },
                "required": ["kind", "key"],
            },
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false},
        }));
    }
    if offered.contains(&TOOL_PROPOSE) {
        let named = json!({
            "type": "object",
            "description": "an entity on the map, by its kind and key as knowhow_search shows them",
            "properties": {
                "kind": {"type": "string"},
                "key": {"type": "string"},
            },
            "required": ["kind", "key"],
        });
        out.push(json!({
            "name": TOOL_PROPOSE,
            "description": format!(
                "Propose what this session learned for the organization's map (Roomler knowhow): \
                 exactly one of a new `entity`, a `fact` of an entity on the map at a path it does \
                 not hold yet, or a `link` between two entities on the map, with `why` this session \
                 believes it, in words a person reviewing it can check. It is written `proposed`, \
                 naming this session, until a person confirms it, and it never changes what is \
                 there. Machines, a device's own checkouts and listeners, and what the fleet or a \
                 device's scans say of them are theirs, never a session's. Never put a secret in \
                 a proposal: one holding a credential's shape is refused whole. At most {} \
                 proposals a session.",
                session_limits::MAX_PROPOSALS
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entity": {
                        "type": "object",
                        "description": "a new entity: its kind, a key no entity of that kind holds, and a name; its environment, attributes and facts when known",
                        "properties": {
                            "kind": {"type": "string", "description": "repo, checkout, database, cluster, namespace, service, endpoint, bucket, cloud_account, domain, environment, team or doc"},
                            "key": {"type": "string", "description": "lowercase segments of a-z 0-9 . _ : @ - joined by /, as github.com/acme/api or prod/payments"},
                            "name": {"type": "string"},
                            "env": {"type": "string", "description": "one lowercase word, as prod or staging"},
                            "attrs": {"type": "object"},
                            "facts": {"type": "object", "description": "path to value, as port or tls.fingerprint"},
                        },
                        "required": ["kind", "key", "name"],
                    },
                    "fact": {
                        "type": "object",
                        "description": "one fact of an entity on the map, at a path it does not hold",
                        "properties": {
                            "kind": {"type": "string"},
                            "key": {"type": "string"},
                            "path": {"type": "string", "description": "a-z 0-9 . _ -, as port or tls.fingerprint"},
                            "value": {"description": "any JSON but null"},
                        },
                        "required": ["kind", "key", "path", "value"],
                    },
                    "link": {
                        "type": "object",
                        "description": "a link `from` `kind` `to` between two entities on the map",
                        "properties": {
                            "from": named.clone(),
                            "kind": {"type": "string", "description": "runs_on, checked_out_at, deployed_to, depends_on, reads, writes, part_of, owned_by, documented_in, authenticates_with or reachable_via"},
                            "to": named,
                            "attrs": {"type": "object"},
                        },
                        "required": ["from", "kind", "to"],
                    },
                    "why": {
                        "type": "string",
                        "description": "why this session believes it: where it saw it",
                        "maxLength": session_limits::MAX_WHY_CHARS,
                    },
                },
                "required": ["why"],
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false},
        }));
    }
    if offered.contains(&TOOL_VERIFY) {
        out.push(json!({
            "name": TOOL_VERIFY,
            "description": "Check an entity of the organization's map (Roomler knowhow) from this \
                            device, by its kind and key as knowhow_search lists them: `tcp` \
                            connects to the host and port its facts say, then closes; `tls` also \
                            reads the certificate the server presents. Nothing is sent past the \
                            handshake and no credential is used. A person approves each call, and \
                            this device's owner decides where its probes may go \
                            (knowhow_probe_reach): outside it the answer is not_allowed. What it \
                            measured is recorded on the entity's facts: a fact found as it says \
                            is marked verified, and a proposed one turns active. The answer says \
                            the outcome (ok, unreachable, not_allowed, failed), the round trip, \
                            the address reached and how (direct, or over the organization's \
                            mesh), and for tls the certificate's names and fingerprint.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "description": "the entity's kind, as knowhow_search shows it"},
                    "key": {"type": "string", "description": "the entity's key, as knowhow_search shows it"},
                    "probe": {
                        "type": "string",
                        "enum": ProbeKind::ALL.map(ProbeKind::as_str),
                        "description": "tcp, a connect; tls, a connect and the certificate. tcp when absent",
                    },
                },
                "required": ["kind", "key"],
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true},
        }));
    }
    out
}

/// One knowhow tool call: its arguments read and checked here first, then
/// asked of the map. `name` is one the run offers (`tools/call` holds it).
pub(crate) async fn call(
    knowhow: &dyn KnowhowTools,
    session: ObjectId,
    fence: u64,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let text = |field: &str| -> Result<String, String> {
        match args.get(field) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(s)) => Ok(s.trim().to_string()),
            Some(_) => Err(format!("{field} is text")),
        }
    };
    match name {
        TOOL_SEARCH => {
            let limit = match args.get("limit") {
                None | Some(Value::Null) => None,
                Some(v) => match v.as_u64() {
                    Some(n) if (1..=u64::from(session_limits::MAX_SEARCH)).contains(&n) => {
                        u32::try_from(n).ok()
                    }
                    _ => {
                        return Err(format!(
                            "limit is a whole number, 1 to {}",
                            session_limits::MAX_SEARCH
                        ));
                    }
                },
            };
            let ask = Search {
                q: text("q")?,
                kind: text("kind")?,
                env: text("env")?,
                limit,
            };
            if ask.q.chars().count() > session_limits::MAX_QUERY_CHARS {
                return Err(format!(
                    "q is at most {} characters",
                    session_limits::MAX_QUERY_CHARS
                ));
            }
            knowhow.search(session, fence, &ask).await
        }
        TOOL_GET => {
            let (kind, key) = (text("kind")?, text("key")?);
            if kind.is_empty() || key.is_empty() {
                return Err("kind and key are required, as knowhow_search shows them".into());
            }
            knowhow.get(session, fence, &kind, &key).await
        }
        TOOL_PROPOSE => {
            let given: Vec<&str> = PROPOSALS
                .into_iter()
                .filter(|k| args.get(*k).is_some_and(|v| !v.is_null()))
                .collect();
            let what = match given.as_slice() {
                [one] => *one,
                _ => return Err("propose exactly one of entity, fact or link".into()),
            };
            let Some(body) = args.get(what).filter(|v| v.is_object()) else {
                return Err(format!("{what} is an object"));
            };
            let why = text("why")?;
            if why.is_empty() {
                return Err("why is required: where this session saw it".into());
            }
            if why.chars().count() > session_limits::MAX_WHY_CHARS {
                return Err(format!(
                    "why is at most {} characters",
                    session_limits::MAX_WHY_CHARS
                ));
            }
            let mut proposal = Map::new();
            proposal.insert(what.to_string(), body.clone());
            knowhow
                .propose(session, fence, &Value::Object(proposal), &why)
                .await
        }
        TOOL_VERIFY => {
            let (kind, key) = (text("kind")?, text("key")?);
            if kind.is_empty() || key.is_empty() {
                return Err("kind and key are required, as knowhow_search shows them".into());
            }
            let probe = match text("probe")?.as_str() {
                "" => ProbeKind::Tcp,
                word => ProbeKind::from_word(word).ok_or("probe is tcp or tls")?,
            };
            knowhow.verify(session, fence, &kind, &key, probe).await
        }
        other => Err(format!("no knowhow tool {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> (Client, mpsc::Receiver<ClientMsg>) {
        let c = Client::default();
        let (tx, rx) = mpsc::channel(8);
        c.connected(tx);
        (c, rx)
    }

    /// The question goes out naming its session and fence; the answer that
    /// names it comes back to it, and one that names no question is dropped.
    #[tokio::test]
    async fn an_answer_reaches_the_question_it_names() {
        let (c, mut rx) = client();
        let c = Arc::new(c);
        let session = ObjectId::new();
        let asking = {
            let c = Arc::clone(&c);
            tokio::spawn(async move { c.get(session, 3, "database", "prod-mongo").await })
        };
        let frame = rx.recv().await.unwrap();
        let ClientMsg::KnowhowGet {
            req_id,
            session_id,
            fence,
            kind,
            key,
        } = frame
        else {
            panic!("not a get: {frame:?}");
        };
        assert_eq!((session_id, fence), (session, 3));
        assert_eq!((kind.as_str(), key.as_str()), ("database", "prod-mongo"));
        c.answered("not-a-question", Answer::Card(json!({"x": 1})));
        assert_eq!(c.waiting(), 1, "an answer for no question is dropped");
        c.answered(
            &req_id,
            Answer::Card(json!({"entity": {"key": "prod-mongo"}})),
        );
        let answer = asking.await.unwrap().unwrap();
        assert_eq!(
            answer,
            Answer::Card(json!({"entity": {"key": "prod-mongo"}}))
        );
        assert_eq!(c.waiting(), 0);
    }

    /// No connection is `NotConnected`; no answer within the deadline is
    /// `NoAnswer`, and the question waits no longer.
    #[tokio::test(start_paused = true)]
    async fn a_question_without_a_connection_or_an_answer_ends() {
        let c = Client::default();
        let ask = Search::default();
        assert_eq!(
            c.search(ObjectId::new(), 1, &ask).await,
            Err(AskError::NotConnected)
        );
        let (c, _rx) = client();
        assert_eq!(
            c.search(ObjectId::new(), 1, &ask).await,
            Err(AskError::NoAnswer)
        );
        assert_eq!(c.waiting(), 0);
    }

    /// The server's word keeps only the tools this build knows, and a new
    /// connection forgets it until the server says it again.
    #[test]
    fn the_word_is_kept_until_the_next_connection() {
        let (c, _rx) = client();
        assert!(c.offered().is_empty(), "no word yet");
        c.served(&["knowhow_get".into(), "knowhow_teleport".into()]);
        assert_eq!(c.offered(), [TOOL_GET]);
        c.served(&["knowhow_search".into(), "knowhow_get".into()]);
        assert_eq!(c.offered(), [TOOL_SEARCH, TOOL_GET]);
        c.served(&[
            "knowhow_propose".into(),
            "knowhow_search".into(),
            "knowhow_get".into(),
        ]);
        assert_eq!(
            c.offered(),
            [TOOL_SEARCH, TOOL_GET, TOOL_PROPOSE],
            "in this build's order, whatever the server's"
        );
        c.served(&[
            "knowhow_verify".into(),
            "knowhow_search".into(),
            "knowhow_get".into(),
            "knowhow_propose".into(),
        ]);
        assert_eq!(c.offered(), KNOWN, "P4-5-2b's verify, where it is named");
        let (tx, _rx2) = mpsc::channel(1);
        c.connected(tx);
        assert!(
            c.offered().is_empty(),
            "a new connection waits for the word"
        );
    }

    /// The tools a session is shown are the ones offered, and none else.
    #[test]
    fn only_the_offered_tools_are_shown() {
        let names = |offered: &[&'static str]| -> Vec<String> {
            tools(offered)
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            names(&[TOOL_SEARCH, TOOL_GET]),
            ["knowhow_search", "knowhow_get"]
        );
        assert_eq!(names(&[TOOL_GET]), ["knowhow_get"]);
        assert_eq!(
            names(&KNOWN),
            [
                "knowhow_search",
                "knowhow_get",
                "knowhow_propose",
                "knowhow_verify"
            ]
        );
        assert_eq!(names(&[TOOL_PROPOSE]), ["knowhow_propose"]);
        assert!(names(&[]).is_empty());
        let propose = &tools(&[TOOL_PROPOSE])[0];
        assert_eq!(propose["inputSchema"]["required"], json!(["why"]));
        assert_eq!(propose["annotations"]["readOnlyHint"], false);
        let verify = &tools(&[TOOL_VERIFY])[0];
        assert_eq!(verify["inputSchema"]["required"], json!(["kind", "key"]));
        assert_eq!(
            verify["inputSchema"]["properties"]["probe"]["enum"],
            json!(["tcp", "tls"])
        );
        assert_eq!(verify["annotations"]["openWorldHint"], true);
    }

    /// P4-5-2b — the reads and a proposal are allowed outright; a verify is
    /// asked of a person at each call, and only where it is offered.
    #[test]
    fn a_verify_is_asked_and_never_allowed() {
        assert_eq!(
            allowed(&KNOWN),
            [
                "mcp__roomler__knowhow_search",
                "mcp__roomler__knowhow_get",
                "mcp__roomler__knowhow_propose"
            ]
        );
        assert_eq!(asked(&KNOWN), ["mcp__roomler__knowhow_verify"]);
        assert!(asked(&[TOOL_SEARCH, TOOL_GET, TOOL_PROPOSE]).is_empty());
        assert!(allowed(&[TOOL_VERIFY]).is_empty());
    }

    /// P4-5-2b — a verify goes out naming its session, fence, entity and
    /// probe; the server's first answer names the probe, and the call waits
    /// for what that probe measured, matched by its id.
    #[tokio::test]
    async fn a_verify_waits_for_what_its_probe_measured() {
        let (c, mut rx) = client();
        let c = Arc::new(c);
        let session = ObjectId::new();
        let asking = {
            let c = Arc::clone(&c);
            tokio::spawn(
                async move { c.verify(session, 4, "endpoint", "db", ProbeKind::Tls).await },
            )
        };
        let frame = rx.recv().await.unwrap();
        let ClientMsg::KnowhowVerify {
            req_id,
            session_id,
            fence,
            kind,
            key,
            probe,
        } = frame
        else {
            panic!("not a verify: {frame:?}");
        };
        assert_eq!((session_id, fence), (session, 4));
        assert_eq!(
            (kind.as_str(), key.as_str(), probe.as_str()),
            ("endpoint", "db", "tls")
        );
        c.answered(
            &req_id,
            Answer::Verifying {
                probe_id: "p1".into(),
            },
        );
        // Another probe's result reaches no one, and is kept for a while.
        c.verified("p0", json!({"state": "ok"}));
        while c.verifies().0 == 0 {
            tokio::task::yield_now().await;
        }
        c.verified("p1", json!({"state": "unreachable"}));
        assert_eq!(
            asking.await.unwrap(),
            Ok(Answer::Measured {
                probe_id: "p1".into(),
                probe: Some(json!({"state": "unreachable"})),
            })
        );
        assert_eq!(c.verifies(), (0, 1), "p0's is kept, p1's was taken");
    }

    /// P4-5-2b — what a probe measured may come back before its verify
    /// waits for it: kept, and given to the verify once it does; at most
    /// [`RECENT_VERIFIED`] kept, the oldest let go.
    #[tokio::test]
    async fn what_a_probe_measured_before_its_verify_waited_is_kept() {
        let (c, mut rx) = client();
        let c = Arc::new(c);
        let asking = {
            let c = Arc::clone(&c);
            tokio::spawn(async move {
                c.verify(ObjectId::new(), 1, "endpoint", "db", ProbeKind::Tcp)
                    .await
            })
        };
        let ClientMsg::KnowhowVerify { req_id, .. } = rx.recv().await.unwrap() else {
            panic!("not a verify");
        };
        c.verified("p1", json!({"state": "ok"}));
        c.answered(
            &req_id,
            Answer::Verifying {
                probe_id: "p1".into(),
            },
        );
        assert_eq!(
            asking.await.unwrap(),
            Ok(Answer::Measured {
                probe_id: "p1".into(),
                probe: Some(json!({"state": "ok"})),
            })
        );
        assert_eq!(c.verifies(), (0, 0));
        for n in 0..=RECENT_VERIFIED {
            c.verified(&format!("q{n}"), json!({"n": n}));
        }
        assert_eq!(c.verifies(), (0, RECENT_VERIFIED));
        assert!(
            lock(&c.verified).recent.iter().all(|(id, _)| id != "q0"),
            "the oldest is let go"
        );
    }

    /// P4-5-2b — a probe that never answers ends the wait pending, and a
    /// new connection ends it at once; neither leaves a waiter behind.
    #[tokio::test(start_paused = true)]
    async fn a_verify_whose_probe_never_answers_ends_pending() {
        let (c, mut rx) = client();
        let c = Arc::new(c);
        for reconnect in [false, true] {
            let asking = {
                let c = Arc::clone(&c);
                tokio::spawn(async move {
                    c.verify(ObjectId::new(), 1, "endpoint", "db", ProbeKind::Tcp)
                        .await
                })
            };
            let ClientMsg::KnowhowVerify { req_id, .. } = rx.recv().await.unwrap() else {
                panic!("not a verify");
            };
            c.answered(
                &req_id,
                Answer::Verifying {
                    probe_id: "p1".into(),
                },
            );
            while c.verifies().0 == 0 {
                tokio::task::yield_now().await;
            }
            let waited_from = tokio::time::Instant::now();
            if reconnect {
                let (tx, next) = mpsc::channel(8);
                c.connected(tx);
                rx = next;
            }
            let answer = asking.await.unwrap();
            // A new connection ends the wait at once; otherwise it is the
            // deadline that does.
            assert_eq!(
                waited_from.elapsed() < VERIFY_DEADLINE,
                reconnect,
                "reconnect: {reconnect}"
            );
            assert_eq!(
                answer,
                Ok(Answer::Measured {
                    probe_id: "p1".into(),
                    probe: None,
                }),
                "reconnect: {reconnect}"
            );
            assert_eq!(c.verifies(), (0, 0), "reconnect: {reconnect}");
            let read = read(answer).unwrap();
            assert_eq!(read["state"], "pending");
            assert_eq!(read["probe_id"], "p1");
        }
    }

    /// P4-5-2b — a measured probe reads as its outcome, what that means,
    /// where it went and what it found, without the record's ids.
    #[test]
    fn a_measured_probe_reads_as_its_outcome() {
        let probe = json!({
            "id": "p1", "entity_id": "e1", "device_id": "d1", "session": "s1",
            "requested_at": "2026-10-24T17:00:00Z", "kind": "tls",
            "target": {"host": "db.example.com", "from": "host", "port": 443, "sni": "db.example.com"},
            "state": "ok", "rtt_ms": 12, "address": "10.0.4.12", "carrier": "direct",
            "tls": {"names": ["db.example.com"], "fingerprint": "ab".repeat(32)},
            "measured": [{"path": "host", "ok": true}, {"path": "port", "ok": true}],
        });
        let read = read(Ok(Answer::Measured {
            probe_id: "p1".into(),
            probe: Some(probe.clone()),
        }))
        .unwrap();
        for key in [
            "state", "kind", "target", "rtt_ms", "address", "carrier", "tls", "measured",
        ] {
            assert_eq!(read[key], probe[key], "{key}");
        }
        for gone in ["id", "entity_id", "device_id", "session", "requested_at"] {
            assert!(read.get(gone).is_none(), "{gone}: {read}");
        }
        assert_eq!(read["probe_id"], "p1");
        let mut means = Vec::new();
        for state in ["ok", "unreachable", "not_allowed", "failed", "expired"] {
            let one = measured("p1", &json!({ "state": state }));
            assert_eq!(one["state"], state);
            means.push(one["means"].as_str().unwrap().to_string());
        }
        means.sort();
        means.dedup();
        assert_eq!(means.len(), 5, "each outcome has its own words");
    }

    /// P4-5-2b — a verify's arguments are checked before anything is
    /// asked: a kind and a key, and a probe that is tcp or tls, tcp when
    /// absent.
    #[tokio::test]
    async fn a_verifys_arguments_are_checked_before_asking() {
        let s = ObjectId::new();
        let asked = call(
            &Fake,
            s,
            1,
            TOOL_VERIFY,
            &json!({"kind": " endpoint ", "key": "db"}),
        )
        .await
        .unwrap();
        assert_eq!(
            asked,
            json!({"kind": "endpoint", "key": "db", "probe": "tcp"})
        );
        let tls = call(
            &Fake,
            s,
            1,
            TOOL_VERIFY,
            &json!({"kind": "endpoint", "key": "db", "probe": "tls"}),
        )
        .await
        .unwrap();
        assert_eq!(tls["probe"], "tls");
        for bad in [
            json!({"key": "db"}),
            json!({"kind": "endpoint"}),
            json!({"kind": "endpoint", "key": "db", "probe": "http"}),
            json!({"kind": "endpoint", "key": "db", "probe": 1}),
        ] {
            assert!(call(&Fake, s, 1, TOOL_VERIFY, &bad).await.is_err(), "{bad}");
        }
    }

    /// P4-4-2b — a proposal goes out as the session wrote it, naming its
    /// session and fence; its answer reads as what was written.
    #[tokio::test]
    async fn a_proposal_goes_out_whole_and_reads_back_as_written() {
        let (c, mut rx) = client();
        let c = Arc::new(c);
        let session = ObjectId::new();
        let proposal = json!({"fact": {"kind": "database", "key": "prod-mongo",
                                       "path": "port", "value": 27017}});
        let asking = {
            let (c, p) = (Arc::clone(&c), proposal.clone());
            tokio::spawn(async move { c.propose(session, 5, &p, "the env file names it").await })
        };
        let frame = rx.recv().await.unwrap();
        let ClientMsg::KnowhowPropose {
            req_id,
            session_id,
            fence,
            proposal: sent,
            why,
        } = frame
        else {
            panic!("not a proposal: {frame:?}");
        };
        assert_eq!((session_id, fence), (session, 5));
        assert_eq!((sent, why.as_str()), (proposal, "the env file names it"));
        let id = ObjectId::new().to_hex();
        c.answered(
            &req_id,
            Answer::Proposed {
                what: "fact".into(),
                id: id.clone(),
                left: 41,
            },
        );
        let answer = asking.await.unwrap();
        assert_eq!(
            read(answer),
            Ok(json!({"proposed": "fact", "id": id, "status": "proposed",
                      "proposals_left": 41}))
        );
    }

    /// Every refusal word reads as words a model can act on, and one this
    /// build does not know is named as it came.
    #[test]
    fn every_refusal_reads_as_words() {
        for r in SessionRefusal::ALL {
            let text = refusal(r.wire());
            assert_ne!(
                text,
                format!("the map refused: {}", r.wire()),
                "{r:?} has words"
            );
        }
        assert_eq!(refusal("teleported"), "the map refused: teleported");
    }

    struct Fake;

    #[async_trait::async_trait]
    impl KnowhowTools for Fake {
        fn offered(&self) -> Vec<&'static str> {
            KNOWN.to_vec()
        }
        async fn search(&self, _: ObjectId, _: u64, ask: &Search) -> Result<Value, String> {
            Ok(json!({ "q": ask.q, "kind": ask.kind, "env": ask.env, "limit": ask.limit }))
        }
        async fn get(&self, _: ObjectId, _: u64, kind: &str, key: &str) -> Result<Value, String> {
            Ok(json!({ "kind": kind, "key": key }))
        }
        async fn propose(
            &self,
            _: ObjectId,
            _: u64,
            proposal: &Value,
            why: &str,
        ) -> Result<Value, String> {
            Ok(json!({ "proposal": proposal, "why": why }))
        }
        async fn verify(
            &self,
            _: ObjectId,
            _: u64,
            kind: &str,
            key: &str,
            probe: ProbeKind,
        ) -> Result<Value, String> {
            Ok(json!({ "kind": kind, "key": key, "probe": probe.as_str() }))
        }
    }

    /// P4-4-2b — a proposal's arguments are checked before anything is
    /// asked: exactly one of entity, fact or link, an object, and a why
    /// within its bound; what goes out is that one, as the session wrote it.
    #[tokio::test]
    async fn a_proposals_arguments_are_checked_before_asking() {
        let s = ObjectId::new();
        let fact = || json!({"kind": "database", "key": "prod-mongo", "path": "port", "value": 1});
        let asked = call(
            &Fake,
            s,
            1,
            TOOL_PROPOSE,
            &json!({"fact": fact(), "entity": null, "why": "  its env file  "}),
        )
        .await
        .unwrap();
        assert_eq!(
            asked,
            json!({"proposal": {"fact": fact()}, "why": "its env file"})
        );
        for bad in [
            json!({"why": "x"}),
            json!({"fact": fact(), "link": {"kind": "reads"}, "why": "x"}),
            json!({"fact": "port 1", "why": "x"}),
            json!({"fact": fact()}),
            json!({"fact": fact(), "why": "  "}),
            json!({"fact": fact(), "why": 7}),
            json!({"fact": fact(), "why": "w".repeat(session_limits::MAX_WHY_CHARS + 1)}),
        ] {
            assert!(
                call(&Fake, s, 1, TOOL_PROPOSE, &bad).await.is_err(),
                "{bad}"
            );
        }
    }

    /// A call's arguments are read and held to the bounds before anything
    /// is asked: text trimmed, a limit 1 to 50, a get's kind and key
    /// required.
    #[tokio::test]
    async fn a_calls_arguments_are_checked_before_asking() {
        let s = ObjectId::new();
        let asked = call(
            &Fake,
            s,
            1,
            TOOL_SEARCH,
            &json!({"q": " mongo ", "limit": 5}),
        )
        .await
        .unwrap();
        assert_eq!(
            asked,
            json!({"q": "mongo", "kind": "", "env": "", "limit": 5})
        );
        for bad in [
            json!({"limit": 0}),
            json!({"limit": 51}),
            json!({"limit": "5"}),
            json!({"q": 7}),
            json!({"q": "m".repeat(201)}),
        ] {
            assert!(call(&Fake, s, 1, TOOL_SEARCH, &bad).await.is_err(), "{bad}");
        }
        assert!(
            call(&Fake, s, 1, TOOL_GET, &json!({"kind": "database"}))
                .await
                .is_err()
        );
        let got = call(
            &Fake,
            s,
            1,
            TOOL_GET,
            &json!({"kind": "database", "key": "prod-mongo"}),
        )
        .await
        .unwrap();
        assert_eq!(got, json!({"kind": "database", "key": "prod-mongo"}));
    }
}
