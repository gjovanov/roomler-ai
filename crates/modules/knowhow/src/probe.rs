// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! P4-5-1a — probes, the server's half (the FR-90 spec, §3e "P4-5,
//! verification, as designed"): a person's request, its target read from the
//! entity's own facts, its delivery, and the device's answer recorded on the
//! facts it measured.
//!
//! A probe is the server's request and a device's answer, as a scan is
//! ([`crate::scan`]): a row in `knowhow_probes` is the bus, the pod holding
//! the device's connection tells it (on the same link a scan goes out on),
//! and the answer is recorded once. The device tries to reach one thing the
//! map says is there: the host and port the entity's own facts say, never a
//! place a request names, and only when its owner's `knowhow_probe_reach`
//! allows it (decision 53, the device's half, P4-5-1b).
//!
//! ⚠️ A probe confirms only what it measured (decision 56): `verified` on
//! each fact it measured, and a proposed fact it found as it says turned
//! `active`. A probe that found a fact otherwise records `ok: false` on it
//! and changes nothing else. `not_allowed` and `failed` measured nothing,
//! so they record nothing on a fact: "the device may not reach it" and "it
//! did not answer" stay two states, and the probe's row says which.
//!
//! ⚠️ A loopback address is the device itself. A probe of one is measured
//! only from the entity's own device, the one whose scan found it, since
//! from any other it would test that other device's own port.

use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use roomler_ai_remote_control::knowhow::{
    MAX_DETAIL, ProbeKind, ProbeOutcome, ProbeTls, fingerprint_hex, holds_credential, never_probed,
    probe_host, probe_limits,
};
use roomler_ai_remote_control::models::{Agent, RpcCap, UpdateRefusal};
use roomler_ai_remote_control::signaling::{ClientMsg, ServerMsg};
use roomler_ai_services::dao::base::{DaoError, DaoResult};
use roomler_core::{AgentCtx, AgentMsgHandler, ApiError, extractors::auth::AuthUser};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::KnowhowState;
use crate::dao::PROBE_ANSWER_SECS;
use crate::model::{
    CARRIER_DIRECT, CARRIER_OVERLAY, Fact, KnowhowEntity, KnowhowProbe, Measured, OP_VERIFY,
    PROBE_DELIVERED, PROBE_REQUESTED, ProbeTarget, ProbeTlsSeen, STATUS_ACTIVE, STATUS_PROPOSED,
    STATUS_STALE, TARGET_ENTITY, Verified,
};
use crate::routes::{limit_of, parse_id, reader, writer};
use crate::scan::Link;
use crate::view::ProbeView;
use crate::write::{Stamp, record};

/// The most probes one device is asked a minute, by anyone.
pub const RATE_PER_MINUTE: u32 = 30;

/// The facts a probe's target is read from, and measures.
pub const ADDRESS: &str = "address";
pub const HOST: &str = "host";
pub const PORT: &str = "port";
pub const SNI: &str = "sni";
pub const TLS_FINGERPRINT: &str = "tls.fingerprint";
pub const TLS_NAMES: &str = "tls.names";

/// How long a delivery waits on a connection's queue before it is put back
/// for the next sweep.
const SEND_WAIT: Duration = Duration::from_secs(2);

/// The writes a probe's record on its entity tries before the probe alone
/// keeps what it measured.
const ATTEMPTS: usize = 3;

// ─── who may be asked ───────────────────────────────────────────────────────

/// Why a device is not asked to probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Its last hello did not advertise `knowhow-probe`: an agent that cannot
    /// probe would never answer.
    CannotProbe,
    /// A secondary enrollment: what it may reach is one device-owned list,
    /// kept for its primary organization alone (decision 43's rule).
    SecondaryEnrollment,
}

impl Refusal {
    pub fn message(self) -> String {
        match self {
            Self::CannotProbe => {
                "this device's agent cannot probe: update it to one that advertises knowhow-probe"
                    .into()
            }
            Self::SecondaryEnrollment => {
                "a device probes for its primary organization only, and this is a secondary \
                 enrollment"
                    .into()
            }
        }
    }
}

/// Why `agent` is not asked to probe, or `None` when it is, by what its last
/// hello said.
pub fn refusal(agent: &Agent) -> Option<Refusal> {
    if !agent.capabilities.has_rpc(RpcCap::KnowhowProbe) {
        return Some(Refusal::CannotProbe);
    }
    if agent.capabilities.update_refusal() == Some(UpdateRefusal::PrimaryOrg) {
        return Some(Refusal::SecondaryEnrollment);
    }
    None
}

// ─── the target ─────────────────────────────────────────────────────────────

/// A host a probe measures as the device itself: a loopback address, or a
/// name that is one by rule (`localhost`, RFC 6761).
fn loopback(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => host == "localhost" || host.ends_with(".localhost"),
    }
}

/// A fact's value as a probe's host, or why not.
fn host_of(value: &Bson) -> Result<String, String> {
    let Bson::String(raw) = value else {
        return Err("is not text".into());
    };
    let host =
        probe_host(raw).ok_or_else(|| format!("{raw:?} is not an IP address or a DNS name"))?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        if ip.is_unspecified() {
            return Err(format!(
                "{raw:?} is where a listener binds, not where it is reached"
            ));
        }
        if never_probed(ip) {
            return Err(format!(
                "{raw:?} is never probed: a link-local, metadata or broadcast address"
            ));
        }
    }
    Ok(host)
}

/// A fact's value as a port: a whole number from 1 to 65535, or one written
/// as text.
fn port_of(value: &Bson) -> Option<u16> {
    let n = match value {
        Bson::Int32(n) => i64::from(*n),
        Bson::Int64(n) => *n,
        Bson::String(s) => s.parse::<i64>().ok()?,
        _ => return None,
    };
    u16::try_from(n).ok().filter(|p| *p != 0)
}

/// A name a TLS probe sends: a DNS name, never an address (RFC 6066).
fn name_of(value: &Bson) -> Option<String> {
    let Bson::String(raw) = value else {
        return None;
    };
    probe_host(raw).filter(|h| h.parse::<IpAddr>().is_err())
}

/// The target of a probe of `entity` by `kind` from `device`, read from the
/// entity's own facts (the FR-90 spec, §3e): its `address`, or its `host`
/// when it has no address a probe reaches; its `port`; and for `tls` the
/// server name, its `sni`, else whichever of its host and address is a name.
/// `Err` holds the 400's words.
pub fn target(
    entity: &KnowhowEntity,
    kind: ProbeKind,
    device: ObjectId,
) -> Result<ProbeTarget, String> {
    let fact = |path: &str| entity.facts.iter().find(|f| f.path == path);
    let mut passed: Vec<String> = Vec::new();
    let mut chosen: Option<(&'static str, String, Bson)> = None;
    for path in [ADDRESS, HOST] {
        let Some(f) = fact(path) else { continue };
        match host_of(&f.value) {
            Ok(host) => {
                chosen = Some((path, host, f.value.clone()));
                break;
            }
            Err(why) => passed.push(format!("its {path} {why}")),
        }
    }
    let Some((host_path, host, host_value)) = chosen else {
        return Err(if passed.is_empty() {
            "the entity says nowhere to probe: give it an address or a host fact, and a port".into()
        } else {
            format!(
                "the entity says nowhere a probe reaches: {}",
                passed.join("; ")
            )
        });
    };
    if loopback(&host) && entity.device != Some(device) {
        return Err(format!(
            "{host} is the device itself: a loopback address is probed only from the device \
             whose scan found it"
        ));
    }
    let Some(port_fact) = fact(PORT) else {
        return Err("the entity says no port: give it a port fact".into());
    };
    let port = port_of(&port_fact.value)
        .ok_or_else(|| "its port is not a number from 1 to 65535".to_string())?;
    let sni = match kind {
        ProbeKind::Tcp => None,
        ProbeKind::Tls => match fact(SNI) {
            Some(f) => {
                Some(name_of(&f.value).ok_or_else(|| "its sni is not a DNS name".to_string())?)
            }
            None => fact(HOST)
                .and_then(|f| name_of(&f.value))
                .or_else(|| host.parse::<IpAddr>().is_err().then(|| host.clone())),
        },
    };
    Ok(ProbeTarget {
        host,
        host_path: host_path.into(),
        host_value,
        port,
        port_value: port_fact.value.clone(),
        sni,
    })
}

// ─── delivery ───────────────────────────────────────────────────────────────

fn frame(probe: &KnowhowProbe, id: ObjectId) -> ServerMsg {
    ServerMsg::KnowhowProbe {
        probe_id: id,
        kind: probe.kind.clone(),
        host: probe.target.host.clone(),
        port: probe.target.port,
        sni: probe.target.sni.clone(),
    }
}

/// Send `probe` on `link`, as a scan is sent: a requested one only from the
/// caller that moved it to delivered, and back to requested when it cannot
/// go out; a delivered one again as it is, since the device runs a probe id
/// once.
async fn deliver(state: &KnowhowState, probe: &KnowhowProbe, link: &Link) {
    let Some(id) = probe.id else { return };
    let now = DateTime::now();
    let fresh = match probe.state.as_str() {
        PROBE_REQUESTED => match state.probes.deliver(id, now).await {
            Ok(true) => true,
            Ok(false) => return,
            Err(e) => {
                warn!(probe = %id, %e, "knowhow: a probe was not marked delivered; the next sweep tries again");
                return;
            }
        },
        PROBE_DELIVERED => false,
        _ => return,
    };
    let sent = tokio::time::timeout(SEND_WAIT, link.tx.send(frame(probe, id))).await;
    match sent {
        Ok(Ok(())) => {
            if !fresh && let Err(e) = state.probes.redelivered(id, now).await {
                warn!(probe = %id, %e, "knowhow: a probe sent again was not stamped");
            }
            info!(probe = %id, device = %probe.device_id, again = !fresh, "knowhow: probe sent");
        }
        _ if fresh => {
            debug!(probe = %id, "knowhow: a probe could not go out; put back for the next sweep");
            if let Err(e) = state.probes.undeliver(id).await {
                warn!(probe = %id, %e, "knowhow: an undelivered probe waits for the device's next hello");
            }
        }
        _ => {
            debug!(probe = %id, "knowhow: a probe sent again could not go out; the next hello tries")
        }
    }
}

/// A device's hello on this pod, its connection already linked
/// ([`crate::scan::hello`]): send the probe asked while it was away, or sent
/// to a connection that died.
pub(crate) async fn hello(state: &KnowhowState, ctx: &AgentCtx, link: &Link) {
    match state.probes.pending_for(ctx.tenant_id, ctx.agent_id).await {
        Ok(Some(probe)) => deliver(state, &probe, link).await,
        Ok(None) => {}
        Err(e) => {
            warn!(device = %ctx.agent_id, %e, "knowhow: a hello's pending probe was not read; the sweep tries again")
        }
    }
}

/// One sweep on this pod: what waited ten minutes for an answer expires,
/// freeing its device for the next, and what was asked of the devices
/// connected here goes out.
pub async fn sweep_once(state: &KnowhowState) {
    let cutoff =
        DateTime::from_millis(DateTime::now().timestamp_millis() - PROBE_ANSWER_SECS * 1000);
    match state.probes.expire_before(cutoff).await {
        Ok(0) => {}
        Ok(n) => info!(
            expired = n,
            "knowhow: probes that were never answered expired"
        ),
        Err(e) => warn!(%e, "knowhow: unanswered probes were not expired; the next sweep tries"),
    }
    let devices = state.links.devices();
    if devices.is_empty() {
        return;
    }
    match state.probes.requested_for(&devices).await {
        Ok(probes) => {
            for probe in probes {
                if let Some(link) = state.links.get(probe.device_id, probe.tenant_id) {
                    deliver(state, &probe, &link).await;
                }
            }
        }
        Err(e) => warn!(%e, "knowhow: requested probes were not read; the next sweep tries"),
    }
}

// ─── the answer ─────────────────────────────────────────────────────────────

/// An answer, read ([`read_answer`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub outcome: ProbeOutcome,
    pub rtt_ms: Option<u32>,
    pub address: Option<IpAddr>,
    pub tls: Option<ProbeTls>,
    pub detail: Option<String>,
}

/// An answer to a probe of `kind`, read: its outcome as recorded, a round
/// trip within bounds, an address only as an IP literal, what TLS showed
/// only when it can be read, and a detail clamped and never a credential.
/// An `ok` that measured less than its kind asks, or that says it reached an
/// address no probe reaches, is recorded as `failed`. Pure, so a test reads
/// it without a socket.
pub fn read_answer(
    kind: Option<ProbeKind>,
    outcome: ProbeOutcome,
    rtt_ms: Option<u32>,
    address: Option<String>,
    tls: Option<Value>,
    detail: Option<String>,
) -> Answer {
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
        outcome,
        rtt_ms: rtt_ms.filter(|r| *r <= probe_limits::MAX_RTT_MS),
        address: address.and_then(|a| a.parse::<IpAddr>().ok()),
        tls: None,
        detail,
    };
    let fail = |answer: &mut Answer, why: &str| {
        answer.outcome = ProbeOutcome::Failed;
        answer.detail = Some(why.to_string());
    };
    if answer.address.is_some_and(never_probed) {
        fail(
            &mut answer,
            "the device said it reached an address no probe reaches",
        );
        return answer;
    }
    if kind == Some(ProbeKind::Tls) && answer.outcome == ProbeOutcome::Ok {
        match tls.as_ref().map(ProbeTls::read) {
            Some(Ok(seen)) => answer.tls = Some(seen),
            Some(Err(why)) => fail(
                &mut answer,
                &format!("its certificate could not be read: {why}"),
            ),
            None => fail(&mut answer, "a tls probe's answer showed no certificate"),
        }
    }
    answer
}

/// A fact's value as text, or none.
fn text(value: &Bson) -> Option<&str> {
    match value {
        Bson::String(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Does a `tls.names` fact say what the certificate showed: every name it
/// lists, one or many, among the certificate's? An empty list or a value
/// holding anything but names says nothing a certificate could show.
fn names_hold(value: &Bson, seen: &[String]) -> bool {
    let listed: Vec<&str> = match value {
        Bson::String(s) => vec![s.as_str()],
        Bson::Array(items) => {
            let names: Option<Vec<&str>> = items.iter().map(text).collect();
            match names {
                Some(names) => names,
                None => return false,
            }
        }
        _ => return false,
    };
    !listed.is_empty()
        && listed
            .iter()
            .all(|n| seen.iter().any(|s| s.eq_ignore_ascii_case(n)))
}

/// What `answer` measured of `entity`'s facts (decision 56): the host's
/// fact and the port's, each while it still says what the probe was aimed
/// at, for an answer that connected or found nothing there; and for a `tls`
/// answer, the certificate's facts, compared with what they say. Returns the
/// facts after, and each fact measured. Pure, so a test reads it without a
/// database.
pub fn measure(
    entity: &KnowhowEntity,
    probe: &KnowhowProbe,
    answer: &Answer,
    carrier: Option<&str>,
    at: DateTime,
) -> (Vec<Fact>, Vec<Measured>) {
    let mut facts = entity.facts.clone();
    let reached = match answer.outcome {
        ProbeOutcome::Ok => true,
        ProbeOutcome::Unreachable => false,
        ProbeOutcome::NotAllowed | ProbeOutcome::Failed => return (facts, Vec::new()),
    };
    let Some(probe_id) = probe.id else {
        return (facts, Vec::new());
    };
    let own_device = entity.device == Some(probe.device_id);
    if !own_device && answer.address.is_some_and(|a| loopback(&a.to_string())) {
        return (facts, Vec::new());
    }
    let says = |path: &str, value: &Bson| facts.iter().any(|f| f.path == path && &f.value == value);
    let mut results: Vec<(String, bool)> = Vec::new();
    let t = &probe.target;
    if says(&t.host_path, &t.host_value) {
        results.push((t.host_path.clone(), reached));
    }
    if says(PORT, &t.port_value) {
        results.push((PORT.into(), reached));
    }
    if reached && let Some(seen) = &answer.tls {
        if let Some(f) = facts.iter().find(|f| f.path == TLS_FINGERPRINT) {
            let same = text(&f.value).and_then(fingerprint_hex).as_deref()
                == Some(seen.fingerprint.as_str());
            results.push((TLS_FINGERPRINT.into(), same));
        }
        if let Some(f) = facts.iter().find(|f| f.path == TLS_NAMES) {
            results.push((TLS_NAMES.into(), names_hold(&f.value, &seen.names)));
        }
    }
    let mut measured = Vec::with_capacity(results.len());
    for (path, ok) in results {
        let Some(f) = facts.iter_mut().find(|f| f.path == path) else {
            continue;
        };
        f.verified = Some(Verified {
            at,
            probe: probe_id,
            from_device: probe.device_id,
            rtt_ms: answer.rtt_ms,
            carrier: carrier.map(str::to_string),
            ok,
        });
        if ok {
            f.verified_at = Some(at);
            if f.status == STATUS_PROPOSED || f.status == STATUS_STALE {
                f.status = STATUS_ACTIVE.into();
            }
        }
        measured.push(Measured { path, ok });
    }
    (facts, measured)
}

/// How the device reached `address`: at an address in the organization's
/// mesh, or on its own network.
async fn carrier_of(
    state: &KnowhowState,
    tid: ObjectId,
    address: Option<IpAddr>,
) -> Option<&'static str> {
    let address = address?;
    let network = match state.overlay_networks.find_for_tenant(tid).await {
        Ok(network) => network,
        Err(e) => {
            warn!(tenant = %tid, %e, "knowhow: the organization's network was not read; a probe's carrier goes unsaid");
            return None;
        }
    };
    let in_mesh = network.is_some_and(|n| n.host_of_ip(&address.to_string()).is_some());
    Some(if in_mesh {
        CARRIER_OVERLAY
    } else {
        CARRIER_DIRECT
    })
}

/// Record what a probe measured on its entity's facts, at the rev it was
/// read, reading it again when a write came between: once, and in the
/// history. What it measured, or nothing when the entity is gone or every
/// attempt lost its race (the probe's row then keeps it alone).
async fn verify(
    state: &KnowhowState,
    probe: &KnowhowProbe,
    answer: &Answer,
    carrier: Option<&str>,
) -> DaoResult<Vec<Measured>> {
    let stamp = Stamp::probe(probe.tenant_id, probe.device_id);
    for _ in 0..ATTEMPTS {
        let Some(current) = state
            .entities
            .get_live(probe.tenant_id, probe.entity_id)
            .await?
        else {
            return Ok(Vec::new());
        };
        let (facts, measured) = measure(&current, probe, answer, carrier, stamp.at);
        if measured.is_empty() {
            return Ok(measured);
        }
        let mut set = doc! { "facts": bson::to_bson(&facts)? };
        set.extend(stamp.touched());
        let Some(after) = state
            .entities
            .update_at(probe.tenant_id, probe.entity_id, current.rev, set)
            .await?
        else {
            continue;
        };
        let said: Vec<Document> = measured
            .iter()
            .map(|m| {
                let status = facts
                    .iter()
                    .find(|f| f.path == m.path)
                    .map_or("", |f| f.status.as_str());
                doc! { "path": m.path.as_str(), "ok": m.ok, "status": status }
            })
            .collect();
        let row = stamp.row(
            TARGET_ENTITY,
            probe.entity_id,
            vec![probe.entity_id],
            after.rev,
            OP_VERIFY,
            doc! { "probe": probe.id.map(|i| i.to_hex()), "facts": said },
        );
        record(state, vec![row]).await;
        return Ok(measured);
    }
    warn!(probe = ?probe.id, entity = %probe.entity_id, "knowhow: a probe's entity kept changing; what it measured stays on the probe alone");
    Ok(Vec::new())
}

/// Record a device's answer: once, and only for a probe this device was
/// sent; then what it measured, on the facts it measured.
async fn answered(state: &KnowhowState, ctx: &AgentCtx, probe_id: ObjectId, raw: RawAnswer) {
    let probe = match state.probes.base.find_by_id(probe_id).await {
        Ok(p)
            if p.tenant_id == ctx.tenant_id
                && p.device_id == ctx.agent_id
                && p.pending == Some(true) =>
        {
            p
        }
        Ok(_) | Err(DaoError::NotFound) => {
            warn!(
                probe = %probe_id, device = %ctx.agent_id,
                "knowhow: an answer for a probe this device was not sent, or one answered already, dropped"
            );
            return;
        }
        Err(e) => {
            warn!(probe = %probe_id, %e, "knowhow: a probe's answer was not read; it expires unanswered");
            return;
        }
    };
    let answer = read_answer(
        ProbeKind::from_word(&probe.kind),
        raw.outcome,
        raw.rtt_ms,
        raw.address,
        raw.tls,
        raw.detail,
    );
    let carrier = carrier_of(state, ctx.tenant_id, answer.address).await;
    let mut set = doc! {
        "state": answer.outcome.as_str(),
        "answered_at": DateTime::now(),
    };
    if let Some(r) = answer.rtt_ms {
        set.insert("rtt_ms", i64::from(r));
    }
    if let Some(a) = answer.address {
        set.insert("address", a.to_string());
    }
    if let Some(c) = carrier {
        set.insert("carrier", c);
    }
    if let Some(seen) = &answer.tls {
        let seen = ProbeTlsSeen {
            names: seen.names.clone(),
            fingerprint: seen.fingerprint.clone(),
        };
        match bson::to_bson(&seen) {
            Ok(b) => {
                set.insert("tls", b);
            }
            Err(e) => {
                warn!(probe = %probe_id, %e, "knowhow: what a probe's certificate showed was not written")
            }
        }
    }
    if let Some(d) = &answer.detail {
        set.insert("detail", d.as_str());
    }
    let after = match state
        .probes
        .answer(probe_id, ctx.tenant_id, ctx.agent_id, set)
        .await
    {
        Ok(Some(after)) => after,
        Ok(None) => {
            warn!(probe = %probe_id, device = %ctx.agent_id, "knowhow: a probe was answered twice; the first stands");
            return;
        }
        Err(e) => {
            warn!(probe = %probe_id, %e, "knowhow: an answer was not recorded; the probe expires unanswered");
            return;
        }
    };
    info!(
        probe = %probe_id, device = %ctx.agent_id, entity = %after.entity_id,
        outcome = answer.outcome.as_str(), rtt_ms = ?answer.rtt_ms, carrier = ?carrier,
        "knowhow: a probe was answered"
    );
    match verify(state, &after, &answer, carrier).await {
        Ok(measured) if measured.is_empty() => {}
        Ok(measured) => {
            info!(probe = %probe_id, ?measured, "knowhow: what a probe measured was recorded on its facts");
            if let Err(e) = state.probes.measured(probe_id, &measured).await {
                warn!(probe = %probe_id, %e, "knowhow: what a probe recorded was not kept on it");
            }
        }
        Err(e) => {
            warn!(probe = %probe_id, %e, "knowhow: what a probe measured was not recorded on its facts")
        }
    }
}

/// An answer as the frame carried it.
struct RawAnswer {
    outcome: ProbeOutcome,
    rtt_ms: Option<u32>,
    address: Option<String>,
    tls: Option<Value>,
    detail: Option<String>,
}

/// The device's answers, dispatched here by `ClientMsg::namespace()`.
pub(crate) struct ProbeAnswers {
    pub state: KnowhowState,
}

#[async_trait]
impl AgentMsgHandler for ProbeAnswers {
    async fn handle(&self, ctx: &AgentCtx, msg: ClientMsg) -> Option<ClientMsg> {
        let ClientMsg::KnowhowProbed {
            probe_id,
            outcome,
            rtt_ms,
            address,
            tls,
            detail,
        } = msg
        else {
            return Some(msg);
        };
        let raw = RawAnswer {
            outcome,
            rtt_ms,
            address,
            tls,
            detail,
        };
        let (state, ctx) = (self.state.clone(), ctx.clone());
        // Off the socket's path, as a scan's answer is.
        tokio::spawn(async move {
            answered(&state, &ctx, probe_id, raw).await;
        });
        None
    }
}

// ─── the routes ─────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct RequestProbe {
    /// The entity whose facts name the target, by its id.
    pub entity: String,
    /// The device that probes, by its id.
    pub device: String,
    /// `tcp` or `tls` (decision 55).
    pub kind: String,
}

/// `POST /api/tenant/{tenant_id}/knowhow/probe` — ask a device to probe an
/// entity (decision 54: `KNOWHOW_EDIT`). The target is read from the
/// entity's facts, never from the request: **400** when they say nowhere a
/// probe reaches. **202** with the probe, or the device's pending one when it
/// is this same probe; **409** for a device busy with another, that cannot
/// probe, or is a secondary enrollment, since it would never answer; **429**
/// past [`RATE_PER_MINUTE`] for the device.
pub async fn request(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<RequestProbe>,
) -> Result<Response, ApiError> {
    let tid = writer(&state, &tenant_id, &auth).await?;
    let kind = ProbeKind::from_word(&body.kind).ok_or_else(|| {
        ApiError::BadRequest(format!("kind {:?} is not one of: tcp, tls", body.kind))
    })?;
    let entity_id = parse_id(&body.entity, "entity")?;
    let device = parse_id(&body.device, "device")?;
    let entity = state
        .entities
        .get_live(tid, entity_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("no such entity".into()))?;
    let agent = match state.agents.find_live_in_tenant(tid, device).await {
        Ok(agent) => agent,
        Err(DaoError::NotFound) => return Err(ApiError::NotFound("no such device".into())),
        Err(e) => return Err(e.into()),
    };
    if let Some(why) = refusal(&agent) {
        return Err(ApiError::Conflict(why.message()));
    }
    let target = target(&entity, kind, device).map_err(ApiError::BadRequest)?;
    // Keyed on the device in both slots: the ceiling is the device's,
    // whoever asks. Checked after every gate, so a refusal is attributable.
    if !state.probe_limiter.check(device, device, RATE_PER_MINUTE) {
        let body = json!({
            "error": "rate_limited",
            "message": format!("this device was asked {RATE_PER_MINUTE} probes this minute: ask again later"),
        });
        return Ok((StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response());
    }
    let probe = KnowhowProbe {
        id: None,
        tenant_id: tid,
        entity_id,
        device_id: device,
        requested_by: Some(auth.user_id),
        session: None,
        requested_at: DateTime::now(),
        kind: kind.as_str().into(),
        target,
        state: PROBE_REQUESTED.into(),
        pending: Some(true),
        delivered_at: None,
        answered_at: None,
        rtt_ms: None,
        address: None,
        carrier: None,
        tls: None,
        detail: None,
        measured: Vec::new(),
    };
    let id = match state.probes.create(&probe).await {
        Ok(id) => id,
        Err(DaoError::DuplicateKey(_)) => {
            return match state.probes.pending_for(tid, device).await? {
                Some(p) if p.entity_id == entity_id && p.kind == probe.kind => {
                    Ok((StatusCode::ACCEPTED, Json(ProbeView::of(&p))).into_response())
                }
                Some(_) => Err(ApiError::Conflict(
                    "this device is running another probe: ask again once it answers".into(),
                )),
                None => Err(ApiError::Conflict(
                    "the device's pending probe just ended: ask again".into(),
                )),
            };
        }
        Err(e) => return Err(e.into()),
    };
    info!(
        probe = %id, tenant = %tid, entity = %entity_id, device = %device, by = %auth.user_id,
        kind = kind.as_str(), "knowhow: a probe was asked"
    );
    let probe = KnowhowProbe {
        id: Some(id),
        ..probe
    };
    // At once, when the device is connected to this pod; any other pod's
    // sweep finds it otherwise.
    if let Some(link) = state.links.get(device, tid) {
        deliver(&state, &probe, &link).await;
    }
    let now = state.probes.base.find_by_id(id).await.unwrap_or(probe);
    Ok((StatusCode::ACCEPTED, Json(ProbeView::of(&now))).into_response())
}

#[derive(Deserialize, Debug)]
pub struct ProbeQuery {
    pub entity: Option<String>,
    pub device: Option<String>,
    pub limit: Option<i64>,
}

/// `GET /api/tenant/{tenant_id}/knowhow/probe?entity=…` or `?device=…` — an
/// entity's probes or a device's, newest first: `{probes}`. Another
/// organization's have none.
pub async fn list(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(q): Query<ProbeQuery>,
) -> Result<Json<Value>, ApiError> {
    let tid = reader(&state, &tenant_id, &auth).await?;
    let limit = limit_of(q.limit)?;
    let mut filter = doc! { "tenant_id": tid };
    match (q.entity.as_deref(), q.device.as_deref()) {
        (Some(e), None) => {
            filter.insert("entity_id", parse_id(e, "entity")?);
        }
        (None, Some(d)) => {
            filter.insert("device_id", parse_id(d, "device")?);
        }
        _ => {
            return Err(ApiError::BadRequest(
                "name an entity or a device, one of the two".into(),
            ));
        }
    }
    let probes = state.probes.list(filter, limit).await?;
    let views: Vec<ProbeView> = probes.iter().map(ProbeView::of).collect();
    Ok(Json(json!({ "probes": views })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SOURCE_PERSON, SOURCE_SCAN, SOURCE_SESSION};

    fn fact(path: &str, value: Bson, status: &str) -> Fact {
        Fact {
            path: path.into(),
            value,
            source: SOURCE_PERSON.into(),
            by: None,
            device: None,
            session: None,
            confirmed: None,
            observed_at: DateTime::from_millis(1),
            verified_at: None,
            verified: None,
            status: status.into(),
        }
    }

    fn entity(facts: Vec<Fact>) -> KnowhowEntity {
        KnowhowEntity {
            id: Some(ObjectId::new()),
            tenant_id: ObjectId::new(),
            kind: "endpoint".into(),
            key: "db".into(),
            name: "db".into(),
            env: None,
            owner_id: None,
            attrs: Document::new(),
            facts,
            status: STATUS_ACTIVE.into(),
            source: SOURCE_PERSON.into(),
            device: None,
            session: None,
            confirmed: None,
            rev: 1,
            created_by: None,
            created_at: DateTime::from_millis(1),
            updated_by: None,
            updated_at: DateTime::from_millis(1),
            deleted_at: None,
        }
    }

    fn s(v: &str) -> Bson {
        Bson::String(v.into())
    }

    /// The target is the entity's own facts: its address, else its host when
    /// the address is where a listener binds; its port as a number or as
    /// text; and for `tls` a name, never an address.
    #[test]
    fn the_target_is_read_from_the_entitys_facts() {
        let d = ObjectId::new();
        let e = entity(vec![
            fact(ADDRESS, s("10.0.4.12"), STATUS_ACTIVE),
            fact(HOST, s("DB.Example.com"), STATUS_ACTIVE),
            fact(PORT, Bson::Int64(5432), STATUS_PROPOSED),
        ]);
        let t = target(&e, ProbeKind::Tcp, d).unwrap();
        assert_eq!(
            (
                t.host.as_str(),
                t.host_path.as_str(),
                t.port,
                t.sni.as_deref()
            ),
            ("10.0.4.12", ADDRESS, 5432, None)
        );
        assert_eq!(t.host_value, s("10.0.4.12"));
        let t = target(&e, ProbeKind::Tls, d).unwrap();
        assert_eq!(
            t.sni.as_deref(),
            Some("db.example.com"),
            "the host names it"
        );

        let bound = entity(vec![
            fact(ADDRESS, s("0.0.0.0"), STATUS_ACTIVE),
            fact(HOST, s("db.example.com"), STATUS_ACTIVE),
            fact(PORT, s("443"), STATUS_ACTIVE),
            fact(SNI, s("api.example.com"), STATUS_ACTIVE),
        ]);
        let t = target(&bound, ProbeKind::Tls, d).unwrap();
        assert_eq!(
            (
                t.host.as_str(),
                t.host_path.as_str(),
                t.port,
                t.sni.as_deref()
            ),
            ("db.example.com", HOST, 443, Some("api.example.com"))
        );
        let only_ip = entity(vec![
            fact(ADDRESS, s("10.0.4.12"), STATUS_ACTIVE),
            fact(PORT, Bson::Int32(443), STATUS_ACTIVE),
        ]);
        assert_eq!(target(&only_ip, ProbeKind::Tls, d).unwrap().sni, None);
    }

    /// An entity that says nowhere a probe reaches is a 400 that says why: no
    /// address or host, one that is no host, a listener's bind address, a
    /// link-local or metadata address, no port or a bad one, a bad sni.
    #[test]
    fn nowhere_a_probe_reaches_is_refused_with_its_reason() {
        let d = ObjectId::new();
        let port = || fact(PORT, Bson::Int64(80), STATUS_ACTIVE);
        for (facts, says) in [
            (vec![port()], "nowhere to probe"),
            (
                vec![
                    fact(ADDRESS, s("https://x.example.com"), STATUS_ACTIVE),
                    port(),
                ],
                "not an IP address",
            ),
            (
                vec![fact(ADDRESS, s("0.0.0.0"), STATUS_ACTIVE), port()],
                "where a listener binds",
            ),
            (
                vec![fact(ADDRESS, s("169.254.169.254"), STATUS_ACTIVE), port()],
                "never probed",
            ),
            (
                vec![fact(HOST, s("fd00:ec2::254"), STATUS_ACTIVE), port()],
                "never probed",
            ),
            (
                vec![fact(ADDRESS, Bson::Int64(7), STATUS_ACTIVE), port()],
                "not text",
            ),
            (vec![fact(ADDRESS, s("10.0.0.1"), STATUS_ACTIVE)], "no port"),
            (
                vec![
                    fact(ADDRESS, s("10.0.0.1"), STATUS_ACTIVE),
                    fact(PORT, Bson::Int64(0), STATUS_ACTIVE),
                ],
                "1 to 65535",
            ),
            (
                vec![
                    fact(ADDRESS, s("10.0.0.1"), STATUS_ACTIVE),
                    fact(PORT, Bson::Int64(70_000), STATUS_ACTIVE),
                ],
                "1 to 65535",
            ),
        ] {
            let why = target(&entity(facts.clone()), ProbeKind::Tcp, d).unwrap_err();
            assert!(why.contains(says), "{facts:?}: {why}");
        }
        let bad_sni = entity(vec![
            fact(ADDRESS, s("10.0.0.1"), STATUS_ACTIVE),
            port(),
            fact(SNI, s("10.0.0.1"), STATUS_ACTIVE),
        ]);
        assert!(
            target(&bad_sni, ProbeKind::Tls, d)
                .unwrap_err()
                .contains("sni")
        );
        assert!(
            target(&bad_sni, ProbeKind::Tcp, d).is_ok(),
            "tcp reads no sni"
        );
    }

    /// A loopback target is the device itself: probed only from the device
    /// whose scan found it.
    #[test]
    fn loopback_is_probed_only_from_its_own_device() {
        let own = ObjectId::new();
        for host in ["127.0.0.1", "::1", "localhost", "db.localhost"] {
            let mut e = entity(vec![
                fact(ADDRESS, s(host), STATUS_ACTIVE),
                fact(PORT, Bson::Int64(5432), STATUS_ACTIVE),
            ]);
            assert!(
                target(&e, ProbeKind::Tcp, own)
                    .unwrap_err()
                    .contains("the device itself"),
                "{host}: no device's scan found this one"
            );
            e.source = SOURCE_SCAN.into();
            e.device = Some(own);
            assert!(
                target(&e, ProbeKind::Tcp, own).is_ok(),
                "{host} from its own device"
            );
            assert!(
                target(&e, ProbeKind::Tcp, ObjectId::new()).is_err(),
                "{host} from another"
            );
        }
    }

    fn probe_of(e: &KnowhowEntity, kind: ProbeKind, device: ObjectId) -> KnowhowProbe {
        KnowhowProbe {
            id: Some(ObjectId::new()),
            tenant_id: e.tenant_id,
            entity_id: e.id.unwrap(),
            device_id: device,
            requested_by: Some(ObjectId::new()),
            session: None,
            requested_at: DateTime::from_millis(5),
            kind: kind.as_str().into(),
            target: target(e, kind, device).unwrap(),
            state: PROBE_DELIVERED.into(),
            pending: Some(true),
            delivered_at: None,
            answered_at: None,
            rtt_ms: None,
            address: None,
            carrier: None,
            tls: None,
            detail: None,
            measured: Vec::new(),
        }
    }

    fn ok(rtt: u32, address: &str, tls: Option<ProbeTls>) -> Answer {
        Answer {
            outcome: ProbeOutcome::Ok,
            rtt_ms: Some(rtt),
            address: address.parse().ok(),
            tls,
            detail: None,
        }
    }

    /// Decision 56: what a probe found as it says is `verified` with `ok`,
    /// and a proposed one turns `active`; a certificate's facts are compared
    /// with what it showed, a different one recorded `ok: false` with its
    /// status kept; an unreachable target is `ok: false` on its host and
    /// port; and what was never measured is left as it was.
    #[test]
    fn a_probe_confirms_only_what_it_measured() {
        let device = ObjectId::new();
        let fp = "ab".repeat(32);
        // As `openssl x509 -fingerprint` prints it: the same SHA-256.
        let printed = ["AB"; 32].join(":");
        let mut e = entity(vec![
            fact(HOST, s("db.example.com"), STATUS_ACTIVE),
            fact(PORT, Bson::Int64(443), STATUS_PROPOSED),
            fact(TLS_FINGERPRINT, s(&printed), STATUS_PROPOSED),
            fact(
                TLS_NAMES,
                Bson::Array(vec![s("db.example.com")]),
                STATUS_PROPOSED,
            ),
            fact("version", s("16"), STATUS_PROPOSED),
        ]);
        e.facts[1].session = Some(ObjectId::new());
        e.facts[1].source = SOURCE_SESSION.into();
        let p = probe_of(&e, ProbeKind::Tls, device);
        let at = DateTime::from_millis(9);
        let seen = ProbeTls {
            names: vec!["db.example.com".into(), "*.example.com".into()],
            fingerprint: fp.clone(),
        };
        let (facts, measured) = measure(
            &e,
            &p,
            &ok(12, "10.0.4.12", Some(seen)),
            Some(CARRIER_DIRECT),
            at,
        );
        let by = |path: &str| facts.iter().find(|f| f.path == path).unwrap();
        assert_eq!(
            measured,
            [HOST, PORT, TLS_FINGERPRINT, TLS_NAMES].map(|p| Measured {
                path: p.into(),
                ok: true
            })
        );
        for path in [HOST, PORT, TLS_FINGERPRINT, TLS_NAMES] {
            let f = by(path);
            assert_eq!(f.status, STATUS_ACTIVE, "{path}");
            assert_eq!(f.verified_at, Some(at), "{path}");
            let v = f.verified.as_ref().unwrap();
            assert_eq!(
                (v.probe, v.from_device, v.rtt_ms, v.carrier.as_deref(), v.ok),
                (p.id.unwrap(), device, Some(12), Some(CARRIER_DIRECT), true)
            );
        }
        assert_eq!(by(PORT).confirmed, None, "a person's word stays a person's");
        assert_eq!(by("version"), &e.facts[4], "never measured, left as it was");

        // A different certificate: `ok: false`, the status kept.
        let other = ProbeTls {
            names: vec!["other.example.com".into()],
            fingerprint: "cd".repeat(32),
        };
        let (facts, measured) = measure(&e, &p, &ok(9, "10.0.4.12", Some(other)), None, at);
        assert!(measured.contains(&Measured {
            path: TLS_FINGERPRINT.into(),
            ok: false
        }));
        assert!(measured.contains(&Measured {
            path: TLS_NAMES.into(),
            ok: false
        }));
        let f = facts.iter().find(|f| f.path == TLS_FINGERPRINT).unwrap();
        assert_eq!((f.status.as_str(), f.verified_at), (STATUS_PROPOSED, None));
        assert!(!f.verified.as_ref().unwrap().ok);

        // Unreachable: the host and port, `ok: false`; the certificate's
        // facts were never measured.
        let none = Answer {
            outcome: ProbeOutcome::Unreachable,
            rtt_ms: None,
            address: None,
            tls: None,
            detail: Some("refused".into()),
        };
        let (_, measured) = measure(&e, &p, &none, None, at);
        assert_eq!(
            measured,
            [HOST, PORT].map(|p| Measured {
                path: p.into(),
                ok: false
            })
        );
    }

    /// `not_allowed` and `failed` measured nothing, a fact changed since the
    /// probe was asked is not what it measured, and a loopback answer from a
    /// device that is not the entity's measures nothing.
    #[test]
    fn what_was_not_measured_records_nothing() {
        let device = ObjectId::new();
        let e = entity(vec![
            fact(ADDRESS, s("10.0.4.12"), STATUS_PROPOSED),
            fact(PORT, Bson::Int64(5432), STATUS_PROPOSED),
        ]);
        let p = probe_of(&e, ProbeKind::Tcp, device);
        let at = DateTime::from_millis(9);
        for outcome in [ProbeOutcome::NotAllowed, ProbeOutcome::Failed] {
            let a = Answer {
                outcome,
                rtt_ms: None,
                address: None,
                tls: None,
                detail: None,
            };
            let (facts, measured) = measure(&e, &p, &a, None, at);
            assert!(measured.is_empty(), "{outcome:?}");
            assert_eq!(facts, e.facts);
        }
        let mut moved = e.clone();
        moved.facts[1].value = Bson::Int64(6432);
        let (_, measured) = measure(&moved, &p, &ok(3, "10.0.4.12", None), None, at);
        assert_eq!(
            measured,
            [Measured {
                path: ADDRESS.into(),
                ok: true
            }],
            "the port moved on"
        );
        let (_, measured) = measure(&e, &p, &ok(3, "127.0.0.1", None), None, at);
        assert!(
            measured.is_empty(),
            "the device's own loopback is not the entity's"
        );
    }

    /// An answer, read: `ok` from a `tls` probe that showed no certificate it
    /// can read, or from any probe that says it reached an address no probe
    /// reaches, is `failed`; a round trip past its bound and an address that
    /// is no IP are dropped; a detail is clamped and never a credential.
    #[test]
    fn an_answer_is_read_within_its_bounds() {
        let fp = "ab".repeat(32);
        let good = json!({"names": ["db.example.com"], "fingerprint": fp});
        let a = read_answer(
            Some(ProbeKind::Tls),
            ProbeOutcome::Ok,
            Some(5),
            Some("10.0.0.1".into()),
            Some(good.clone()),
            None,
        );
        assert_eq!(a.outcome, ProbeOutcome::Ok);
        assert_eq!(a.tls.unwrap().fingerprint, fp);
        for tls in [None, Some(json!({"fingerprint": "nope"}))] {
            let a = read_answer(
                Some(ProbeKind::Tls),
                ProbeOutcome::Ok,
                Some(5),
                None,
                tls,
                None,
            );
            assert_eq!(a.outcome, ProbeOutcome::Failed);
            assert!(a.detail.unwrap().contains("certificate"));
        }
        let a = read_answer(
            Some(ProbeKind::Tcp),
            ProbeOutcome::Ok,
            Some(5),
            None,
            Some(good),
            None,
        );
        assert_eq!(
            (a.outcome, a.tls),
            (ProbeOutcome::Ok, None),
            "a tcp probe reads no certificate"
        );
        let a = read_answer(
            Some(ProbeKind::Tcp),
            ProbeOutcome::Ok,
            Some(5),
            Some("169.254.169.254".into()),
            None,
            None,
        );
        assert_eq!(a.outcome, ProbeOutcome::Failed);
        let a = read_answer(
            Some(ProbeKind::Tcp),
            ProbeOutcome::Ok,
            Some(probe_limits::MAX_RTT_MS + 1),
            Some("db.example.com".into()),
            None,
            None,
        );
        assert_eq!((a.rtt_ms, a.address), (None, None));
        let leak = format!("refused by https://dev:{}@db.example.com", "hunter2");
        let a = read_answer(
            None,
            ProbeOutcome::Unreachable,
            None,
            None,
            None,
            Some(leak),
        );
        assert_eq!(
            a.detail.unwrap(),
            "(withheld: it held a credential's shape)"
        );
        let long = format!("no route\u{7} {}", "x".repeat(MAX_DETAIL * 2));
        let d = read_answer(
            None,
            ProbeOutcome::Unreachable,
            None,
            None,
            None,
            Some(long),
        )
        .detail
        .unwrap();
        assert_eq!(d.chars().count(), MAX_DETAIL);
        assert!(!d.contains('\u{7}'));
    }
}
