// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-6-1a — `roomler_connect`, the server's half (the FR-90 spec, §3e
//! "P4-6, `roomler_connect`, as designed"; `docs/knowhow.md` §13.6).
//!
//! A session names an entity, and knowhow compiles what reaching it takes
//! from the map alone. The engine, the upstream and the credential handle
//! come from the entity's own facts (decision 59). The paths come from the
//! devices' findings, the session's own device first (decision 60). It
//! grants nothing: the device then asks the vault for the credential's leases
//! and, for a path through another device, for the flow, each decided as
//! before.
//!
//! ⚠️ Only an `active` or `stale` fact is read. A session's proposal stays
//! `proposed` until a person or a probe confirms it, so a session cannot
//! choose where a credential goes by proposing it: the answer reads the
//! entity as if the fact were absent, and names it in `proposed`.

use std::collections::HashSet;

use bson::{DateTime, oid::ObjectId};
use roomler_ai_remote_control::knowhow::{Engine, ProbeOutcome, SessionRefusal, access_facts};
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::KnowhowState;
use crate::model::{
    Fact, KnowhowEntity, KnowhowProbe, STATUS_ACTIVE, STATUS_PROPOSED, STATUS_STALE, check_key,
    check_kind_word,
};
use crate::probe::{ADDRESS, HOST, PORT, host_of, port_of, text};
use crate::session::Conn;
use crate::view::rfc3339;

/// How long a finding counts as fresh when `knowhow.connect_fresh_secs` is
/// 0 or absent: a day.
pub const DEFAULT_CONNECT_FRESH_SECS: u64 = 86_400;

/// The most paths, and the most findings beside them, one answer lists.
pub const MAX_PATHS: usize = 16;

/// `knowhow.connect_fresh_secs` read: a day when 0 or absent.
pub fn fresh_secs(setting: u64) -> u64 {
    if setting == 0 {
        DEFAULT_CONNECT_FRESH_SECS
    } else {
        setting
    }
}

/// What the session's own device's last finding says of the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Here {
    /// Its last finding is `ok` and fresh: it connects from here.
    Verified,
    /// Its last finding is fresh and `unreachable`: from here it does not.
    Unreachable,
    /// It has no finding, or its last is not fresh: it probes from here
    /// first.
    Unmeasured,
}

impl Here {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unreachable => "unreachable",
            Self::Unmeasured => "unmeasured",
        }
    }
}

/// The entity's fact at `path`, whatever its status.
fn fact<'a>(entity: &'a KnowhowEntity, path: &str) -> Option<&'a Fact> {
    entity.facts.iter().find(|f| f.path == path)
}

/// The fact at `path` an answer reads: an `active` or `stale` one.
fn read<'a>(entity: &'a KnowhowEntity, path: &str) -> Option<&'a Fact> {
    fact(entity, path).filter(|f| f.status == STATUS_ACTIVE || f.status == STATUS_STALE)
}

/// The text of the fact at `path` an answer reads, trimmed, when there is
/// any.
fn read_text<'a>(entity: &'a KnowhowEntity, path: &str) -> Option<&'a str> {
    read(entity, path)
        .and_then(|f| text(&f.value))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Whether the fact at `path` is there only as a proposal.
fn proposed_only(entity: &KnowhowEntity, path: &str) -> bool {
    fact(entity, path).is_some_and(|f| f.status == STATUS_PROPOSED)
}

/// `path` is missing from what the answer reads: say so, and say too when
/// only a proposal holds it.
fn lacks(
    entity: &KnowhowEntity,
    path: &'static str,
    missing: &mut Vec<&'static str>,
    proposed: &mut Vec<&'static str>,
) {
    missing.push(path);
    if proposed_only(entity, path) {
        proposed.push(path);
    }
}

/// The upstream, spelled as `vault_connect` takes it and as a secret's
/// `audiences` must name it: `host:port` for a database and for `tcp`,
/// `https://host[:port]` for `http` and `kubernetes`, `ssh://[user@]host[:port]`
/// for `ssh`, each default port left out. An IPv6 address is bracketed.
pub fn upstream(engine: Engine, host: &str, port: u16, user: Option<&str>) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match engine {
        Engine::Postgres | Engine::Mongodb | Engine::Tcp => format!("{host}:{port}"),
        Engine::Http | Engine::Kubernetes if port == 443 => format!("https://{host}"),
        Engine::Http | Engine::Kubernetes => format!("https://{host}:{port}"),
        Engine::Ssh => {
            let user = user.map(|u| format!("{u}@")).unwrap_or_default();
            if port == 22 {
                format!("ssh://{user}{host}")
            } else {
                format!("ssh://{user}{host}:{port}")
            }
        }
    }
}

/// When a finding was made: its answer, or its asking when it has none.
fn found_at(p: &KnowhowProbe) -> DateTime {
    p.answered_at.unwrap_or(p.requested_at)
}

/// What reaching `entity` takes, for a session on the device `here`: its
/// access from its facts, and its paths from `found`, each device's last
/// finding of each kind, newest first
/// ([`crate::dao::ProbeDao::last_per_device_and_kind`]). A device's newest
/// finding is its last word: a path when it is `ok` and no older than
/// `fresh_secs` at `now`, `here`'s first. `seen` holds the findings of every
/// device that is no path.
pub fn compile(
    entity: &KnowhowEntity,
    found: &[KnowhowProbe],
    here: ObjectId,
    now: DateTime,
    fresh_secs: u64,
) -> Value {
    let mut missing: Vec<&str> = Vec::new();
    let mut proposed: Vec<&str> = Vec::new();

    let engine_word = read_text(entity, access_facts::ENGINE);
    let engine = engine_word.and_then(Engine::from_word);
    if engine.is_none() {
        lacks(entity, access_facts::ENGINE, &mut missing, &mut proposed);
    }
    // `host` first: the name the upstream's certificate is checked against.
    let host = [HOST, ADDRESS]
        .into_iter()
        .find_map(|path| read(entity, path).and_then(|f| host_of(&f.value).ok()));
    if host.is_none() {
        lacks(entity, HOST, &mut missing, &mut proposed);
        if proposed_only(entity, ADDRESS) {
            proposed.push(ADDRESS);
        }
    }
    let port = read(entity, PORT).and_then(|f| port_of(&f.value));
    if port.is_none() {
        lacks(entity, PORT, &mut missing, &mut proposed);
    }
    let credential = read_text(entity, access_facts::CREDENTIAL);
    if engine.is_some_and(Engine::needs_credential) && credential.is_none() {
        lacks(
            entity,
            access_facts::CREDENTIAL,
            &mut missing,
            &mut proposed,
        );
    }
    let mut options = Map::new();
    for path in access_facts::OPTIONS {
        if let Some(value) = read_text(entity, path) {
            options.insert(path.to_string(), json!(value));
        }
    }
    let upstream = match (engine, host.as_deref(), port) {
        (Some(engine), Some(host), Some(port)) => Some(upstream(
            engine,
            host,
            port,
            options.get(access_facts::USER).and_then(Value::as_str),
        )),
        _ => None,
    };

    let back = i64::try_from(fresh_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    let cutoff = now.timestamp_millis().saturating_sub(back);
    let fresh = |p: &KnowhowProbe| found_at(p).timestamp_millis() >= cutoff;
    let usable = |p: &KnowhowProbe| p.state == ProbeOutcome::Ok.as_str() && fresh(p);
    // A device's newest finding, of either kind, is its last word on the
    // target: an older `ok` behind a newer `unreachable` is no path.
    let mut devices: HashSet<ObjectId> = HashSet::new();
    let last: Vec<&KnowhowProbe> = found
        .iter()
        .filter(|p| devices.insert(p.device_id))
        .collect();
    let own = last.iter().copied().find(|p| p.device_id == here);
    let here_says = match own {
        Some(p) if usable(p) => Here::Verified,
        Some(p) if fresh(p) => Here::Unreachable,
        _ => Here::Unmeasured,
    };
    let reached: Vec<&KnowhowProbe> = own
        .into_iter()
        .chain(last.iter().copied().filter(|p| p.device_id != here))
        .filter(|p| usable(p))
        .collect();
    let through: HashSet<ObjectId> = reached.iter().map(|p| p.device_id).collect();
    let paths: Vec<Value> = reached
        .iter()
        .take(MAX_PATHS)
        .map(|p| {
            json!({
                "device_id": p.device_id.to_hex(),
                "here": p.device_id == here,
                "probe_id": p.id.map(|id| id.to_hex()),
                "kind": p.kind,
                "carrier": p.carrier,
                "rtt_ms": p.rtt_ms,
                "address": p.address,
                "at": rfc3339(found_at(p)),
            })
        })
        .collect();
    let seen: Vec<Value> = found
        .iter()
        .filter(|p| !through.contains(&p.device_id))
        .take(MAX_PATHS)
        .map(|p| {
            json!({
                "device_id": p.device_id.to_hex(),
                "here": p.device_id == here,
                "kind": p.kind,
                "state": p.state,
                "at": rfc3339(found_at(p)),
                "fresh": fresh(p),
            })
        })
        .collect();

    let mut access = json!({
        "entity": {
            "id": entity.id.map(|id| id.to_hex()),
            "kind": entity.kind,
            "key": entity.key,
            "name": entity.name,
        },
        "engine": engine.map(Engine::as_str),
        "upstream": upstream,
        "host": host,
        "port": port,
        "credential": credential,
        "options": options,
        "missing": missing,
        "proposed": proposed,
        "here": here_says.as_str(),
        "paths": paths,
        "seen": seen,
        "fresh_secs": fresh_secs,
    });
    if engine.is_none()
        && let Some(word) = engine_word
    {
        access["unknown_engine"] = json!(word);
    }
    access
}

/// A session's connect (P4-6-1a): the live entity by its kind and key, its
/// access compiled for the session's device, the device on `conn`. The
/// session is believed, and its starter's `KNOWHOW_VIEW` read, before this
/// is asked (decision 61).
pub(crate) async fn access(
    state: &KnowhowState,
    conn: Conn,
    kind: &str,
    key: &str,
) -> Result<Value, SessionRefusal> {
    let kind = check_kind_word(kind.trim()).map_err(|_| SessionRefusal::BadRequest)?;
    let key = check_key(key.trim()).map_err(|_| SessionRefusal::BadRequest)?;
    let entity = match state
        .entities
        .find_live_by_key(conn.tenant_id, kind, key)
        .await
    {
        Ok(Some(e)) => e,
        Ok(None) => return Err(SessionRefusal::NotFound),
        Err(e) => {
            warn!(tenant = %conn.tenant_id, %e, "knowhow: a session's connect found no entity it could read");
            return Err(SessionRefusal::Unavailable);
        }
    };
    let id = entity.id.ok_or(SessionRefusal::Unavailable)?;
    let found = state
        .probes
        .last_per_device_and_kind(conn.tenant_id, id, crate::routes::CARD_FINDINGS)
        .await
        .map_err(|e| {
            warn!(tenant = %conn.tenant_id, entity = %id, %e, "knowhow: a session's connect read no findings");
            SessionRefusal::Unavailable
        })?;
    Ok(compile(
        &entity,
        &found,
        conn.device_id,
        DateTime::now(),
        fresh_secs(state.core.settings.knowhow.connect_fresh_secs),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProbeTarget, SOURCE_PERSON};
    use bson::{Bson, Document};

    const NOW: i64 = 1_800_000_000_000;

    fn s(v: &str) -> Bson {
        Bson::String(v.into())
    }

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
            kind: "database".into(),
            key: "prod-pg".into(),
            name: "prod pg".into(),
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

    /// A Postgres the map knows all of, its credential at `status`.
    fn postgres(status: &str) -> KnowhowEntity {
        entity(vec![
            fact("engine", s("postgres"), STATUS_ACTIVE),
            fact("host", s("db.example.com"), STATUS_ACTIVE),
            fact("port", Bson::Int32(5432), STATUS_ACTIVE),
            fact("credential", s("secret://grox/db/prod"), status),
            fact("database", s("app"), STATUS_ACTIVE),
        ])
    }

    /// One finding of `device`, of `kind`, `secs` before [`NOW`].
    fn finding(device: ObjectId, kind: &str, state: &str, secs: i64) -> KnowhowProbe {
        let at = DateTime::from_millis(NOW - secs * 1000);
        KnowhowProbe {
            id: Some(ObjectId::new()),
            tenant_id: ObjectId::new(),
            entity_id: ObjectId::new(),
            device_id: device,
            requested_by: None,
            session: None,
            requested_at: at,
            kind: kind.into(),
            target: ProbeTarget {
                host: "db.example.com".into(),
                host_path: "host".into(),
                host_value: s("db.example.com"),
                port: 5432,
                port_value: Bson::Int32(5432),
                sni: None,
            },
            state: state.into(),
            pending: None,
            delivered_at: Some(at),
            answered_at: Some(at),
            rtt_ms: Some(3),
            address: Some("10.0.0.5".into()),
            carrier: Some("direct".into()),
            tls: None,
            detail: None,
            measured: Vec::new(),
        }
    }

    fn compiled(e: &KnowhowEntity, found: &[KnowhowProbe], here: ObjectId) -> Value {
        compile(e, found, here, DateTime::from_millis(NOW), 86_400)
    }

    /// An entity that says all of it: the engine, the upstream spelled as
    /// `vault_connect` takes it, the credential handle and the options.
    #[test]
    fn an_entity_says_how_it_is_reached() {
        let a = compiled(&postgres(STATUS_ACTIVE), &[], ObjectId::new());
        assert_eq!(a["engine"], json!("postgres"));
        assert_eq!(a["upstream"], json!("db.example.com:5432"));
        assert_eq!(a["credential"], json!("secret://grox/db/prod"));
        assert_eq!(a["options"], json!({"database": "app"}));
        assert_eq!(a["missing"], json!([]));
        assert_eq!(a["proposed"], json!([]));
        assert_eq!(a["entity"]["key"], json!("prod-pg"));
        assert_eq!(a["fresh_secs"], json!(86_400));
    }

    /// The upstream's host is the entity's `host` before its `address`: the
    /// name a secret's `audiences` and the upstream's certificate carry. The
    /// address serves when there is no host.
    #[test]
    fn the_host_comes_before_the_address() {
        let mut e = postgres(STATUS_ACTIVE);
        e.facts.push(fact("address", s("10.0.4.12"), STATUS_ACTIVE));
        let a = compiled(&e, &[], ObjectId::new());
        assert_eq!(a["upstream"], json!("db.example.com:5432"));
        assert_eq!(a["host"], json!("db.example.com"));
        e.facts.retain(|f| f.path != "host");
        let a = compiled(&e, &[], ObjectId::new());
        assert_eq!(
            a["upstream"],
            json!("10.0.4.12:5432"),
            "no host: the address"
        );
        e.facts
            .push(fact("host", s("169.254.169.254"), STATUS_ACTIVE));
        let a = compiled(&e, &[], ObjectId::new());
        assert_eq!(
            a["upstream"],
            json!("10.0.4.12:5432"),
            "a host no probe reaches: the address"
        );
    }

    /// Each engine's upstream as `vault_connect` spells it: a default port
    /// left out, an ssh user before the host, an IPv6 address bracketed.
    #[test]
    fn each_engine_spells_its_upstream() {
        let h = "db.example.com";
        assert_eq!(
            upstream(Engine::Postgres, h, 5432, None),
            "db.example.com:5432"
        );
        assert_eq!(
            upstream(Engine::Mongodb, h, 27017, None),
            "db.example.com:27017"
        );
        assert_eq!(upstream(Engine::Tcp, h, 6379, None), "db.example.com:6379");
        assert_eq!(
            upstream(Engine::Http, h, 443, None),
            "https://db.example.com"
        );
        assert_eq!(
            upstream(Engine::Http, h, 8443, None),
            "https://db.example.com:8443"
        );
        assert_eq!(
            upstream(Engine::Kubernetes, h, 6443, None),
            "https://db.example.com:6443"
        );
        assert_eq!(upstream(Engine::Ssh, h, 22, None), "ssh://db.example.com");
        assert_eq!(
            upstream(Engine::Ssh, h, 2222, Some("deploy")),
            "ssh://deploy@db.example.com:2222"
        );
        assert_eq!(
            upstream(Engine::Postgres, "fd00::5", 5432, None),
            "[fd00::5]:5432"
        );
    }

    /// A session's proposal places no credential: it is read as absent, and
    /// named. A stale fact is still a person's word, and is read.
    #[test]
    fn a_proposal_places_no_credential() {
        let a = compiled(&postgres(STATUS_PROPOSED), &[], ObjectId::new());
        assert_eq!(a["credential"], Value::Null);
        assert_eq!(a["missing"], json!(["credential"]));
        assert_eq!(a["proposed"], json!(["credential"]));
        let a = compiled(&postgres(STATUS_STALE), &[], ObjectId::new());
        assert_eq!(a["credential"], json!("secret://grox/db/prod"));
        assert_eq!(a["missing"], json!([]));
    }

    /// What is missing is named: no engine, an engine this build does not
    /// know, a host no probe would reach, no port. `tcp` needs no credential.
    #[test]
    fn what_is_missing_is_named() {
        let none = compiled(&entity(vec![]), &[], ObjectId::new());
        assert_eq!(none["missing"], json!(["engine", "host", "port"]));
        assert_eq!(none["upstream"], Value::Null);
        let unknown = compiled(
            &entity(vec![
                fact("engine", s("mysql"), STATUS_ACTIVE),
                fact("host", s("db.example.com"), STATUS_ACTIVE),
                fact("port", Bson::Int32(3306), STATUS_ACTIVE),
            ]),
            &[],
            ObjectId::new(),
        );
        assert_eq!(unknown["missing"], json!(["engine"]));
        assert_eq!(unknown["unknown_engine"], json!("mysql"));
        let tcp = compiled(
            &entity(vec![
                fact("engine", s("tcp"), STATUS_ACTIVE),
                fact("address", s("10.0.4.12"), STATUS_ACTIVE),
                fact("port", Bson::Int32(6379), STATUS_ACTIVE),
            ]),
            &[],
            ObjectId::new(),
        );
        assert_eq!(tcp["missing"], json!([]));
        assert_eq!(tcp["upstream"], json!("10.0.4.12:6379"));
        let metadata = compiled(
            &entity(vec![
                fact("engine", s("http"), STATUS_ACTIVE),
                fact("address", s("169.254.169.254"), STATUS_ACTIVE),
                fact("port", Bson::Int32(80), STATUS_ACTIVE),
                fact("credential", s("secret://grox/api"), STATUS_ACTIVE),
            ]),
            &[],
            ObjectId::new(),
        );
        assert_eq!(
            metadata["missing"],
            json!(["host"]),
            "never a metadata address"
        );
    }

    /// Paths are measured ones: each device's last finding when it is `ok`
    /// and fresh, this device's first, one a device. A device whose last
    /// word is stale or unreachable is no path, and its findings are listed
    /// beside them.
    #[test]
    fn paths_are_measured_ones_this_devices_first() {
        let (here, x, y, z) = (
            ObjectId::new(),
            ObjectId::new(),
            ObjectId::new(),
            ObjectId::new(),
        );
        let found = vec![
            finding(x, "tcp", "ok", 5),
            finding(here, "tls", "ok", 10),
            finding(z, "tcp", "unreachable", 20),
            finding(x, "tls", "ok", 30),
            finding(y, "tls", "ok", 2 * 86_400),
        ];
        let a = compiled(&postgres(STATUS_ACTIVE), &found, here);
        assert_eq!(a["here"], json!("verified"));
        let paths: Vec<(String, bool)> = a["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["device_id"].as_str().unwrap().to_string(),
                    p["here"] == json!(true),
                )
            })
            .collect();
        assert_eq!(paths, [(here.to_hex(), true), (x.to_hex(), false)]);
        assert_eq!(a["paths"][1]["kind"], json!("tcp"), "a device's newest");
        let seen: Vec<(String, Value)> = a["seen"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["device_id"].as_str().unwrap().to_string(),
                    p["fresh"].clone(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [(z.to_hex(), json!(true)), (y.to_hex(), json!(false))]
        );
    }

    /// This device's own word is its last finding: unreachable when that is
    /// fresh and `unreachable`, unmeasured when it is not fresh, or there is
    /// none.
    #[test]
    fn this_device_is_verified_unreachable_or_unmeasured() {
        let (here, x) = (ObjectId::new(), ObjectId::new());
        let e = postgres(STATUS_ACTIVE);
        let unreachable = compiled(&e, &[finding(here, "tcp", "unreachable", 5)], here);
        assert_eq!(unreachable["here"], json!("unreachable"));
        assert_eq!(unreachable["paths"], json!([]));
        let stale = compiled(&e, &[finding(here, "tls", "ok", 3 * 86_400)], here);
        assert_eq!(stale["here"], json!("unmeasured"));
        let elsewhere = compiled(&e, &[finding(x, "tls", "ok", 5)], here);
        assert_eq!(elsewhere["here"], json!("unmeasured"));
        assert_eq!(elsewhere["paths"][0]["device_id"], json!(x.to_hex()));
        let again = compiled(
            &e,
            &[
                finding(here, "tls", "ok", 5),
                finding(here, "tcp", "unreachable", 30),
            ],
            here,
        );
        assert_eq!(again["here"], json!("verified"), "a newer ok is the word");
    }

    /// A device's last word decides, of either kind: an `ok` behind a newer
    /// `unreachable` is no path, and both are listed beside the paths.
    #[test]
    fn a_devices_last_word_decides() {
        let (here, w, x) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let found = vec![
            finding(here, "tcp", "unreachable", 5),
            finding(w, "tcp", "unreachable", 10),
            finding(x, "tls", "ok", 20),
            finding(here, "tls", "ok", 30),
            finding(w, "tls", "ok", 40),
        ];
        let a = compiled(&postgres(STATUS_ACTIVE), &found, here);
        assert_eq!(a["here"], json!("unreachable"));
        let paths: Vec<&str> = a["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["device_id"].as_str().unwrap())
            .collect();
        assert_eq!(paths, [x.to_hex()]);
        let seen: Vec<(String, String)> = a["seen"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["device_id"].as_str().unwrap().to_string(),
                    p["state"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let row = |d: ObjectId, s: &str| (d.to_hex(), s.to_string());
        assert_eq!(
            seen,
            [
                row(here, "unreachable"),
                row(w, "unreachable"),
                row(here, "ok"),
                row(w, "ok"),
            ]
        );
    }

    /// At most sixteen paths, however many devices reached it.
    #[test]
    fn at_most_sixteen_paths() {
        let found: Vec<KnowhowProbe> = (0..20)
            .map(|i| finding(ObjectId::new(), "tcp", "ok", i))
            .collect();
        let a = compiled(&postgres(STATUS_ACTIVE), &found, ObjectId::new());
        assert_eq!(a["paths"].as_array().unwrap().len(), MAX_PATHS);
    }

    /// The setting reads 0 as a day.
    #[test]
    fn fresh_reads_zero_as_a_day() {
        assert_eq!(fresh_secs(0), DEFAULT_CONNECT_FRESH_SECS);
        assert_eq!(fresh_secs(60), 60);
    }
}
