// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-6-1b-1 — `roomler_connect`, the device's half, for a path from
//! this device (the FR-90 spec, §3e "P4-6, `roomler_connect`, as designed";
//! `docs/knowhow.md` §12.1, §13.6).
//!
//! A session names an entity of the organization's map. The server answers
//! what reaching it takes (`rc:knowhow.access`, P4-6-1a, read as [`Access`]),
//! and this opens it from here, a person approving each call ([`plan`]):
//!
//! | The access says | The tool |
//! |---|---|
//! | something is missing, or an engine this build does not speak | refuses `no_access`, naming each path, and which of them a session only proposed |
//! | this device's last finding is `ok` and fresh (`verified`) | opens what `vault_connect` opens, toward the access's upstream with its credential; a `tcp` entity takes none, and is answered where to dial |
//! | this device has no fresh finding (`unmeasured`) | asks one probe from here first, the session's own `knowhow_verify`, and opens it when the probe found it |
//! | this device's last finding is `unreachable`, or the probe from here found nothing | no path from here: the answer names the devices that reached it |
//!
//! ⚠️ Measured, never assumed (pillar 2's first commitment): nothing is
//! dialled that no probe from here found. A path through another device is
//! P4-6-1b-2's.
//!
//! ⚠️ The probe from here is the session's `knowhow_verify`, held to its
//! starter's `KNOWHOW_EDIT` (decision 54): a probe writes what it measured
//! onto the map. A starter who may only read the map is told what the
//! devices found, and no probe is asked.

use std::future::Future;

use bson::oid::ObjectId;
use roomler_ai_remote_control::knowhow::{Engine, ProbeKind, access_facts};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::knowhow_tools::KnowhowTools;

/// The server's access, as this device reads it: what P4-6-1a compiles
/// (`crates/modules/knowhow/src/connect.rs`). A field a newer server adds is
/// ignored, and one an older server lacks reads as empty.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Access {
    pub engine: Option<String>,
    pub upstream: Option<String>,
    pub credential: Option<String>,
    pub options: Map<String, Value>,
    pub missing: Vec<String>,
    pub proposed: Vec<String>,
    /// This device's own word: `verified`, `unreachable` or `unmeasured`.
    pub here: String,
    pub paths: Vec<Value>,
    pub unknown_engine: Option<String>,
}

/// What the tool does with an access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// The map does not say enough to reach it: refused, in words.
    NoAccess(String),
    /// This device's last finding is `ok` and fresh: open it from here.
    Here,
    /// This device has no fresh finding: one probe from here first.
    ProbeFirst,
    /// This device's last finding is `unreachable`, or its word is one this
    /// build does not know: no path from here.
    Elsewhere,
}

/// The engines a session may reach through the map, as their words.
fn engines() -> String {
    Engine::ALL.map(Engine::as_str).join(", ")
}

/// Paths joined for a sentence: `a`, `a and b`, `a, b and c`.
fn listed(paths: &[String]) -> String {
    match paths {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The map lacks `missing`; a session only proposed `proposed` of them.
fn no_access(missing: &[String], proposed: &[String]) -> String {
    let mut why = format!(
        "no_access: the map does not say how to reach it: it has no {} fact",
        listed(missing)
    );
    if missing.len() > 1 {
        why.push('s');
    }
    if !proposed.is_empty() {
        why.push_str(&format!(
            ". A session proposed {}, which no person has confirmed yet: until one does, it is \
             read as absent (knowhow_get shows the proposal)",
            listed(proposed)
        ));
    }
    why.push_str(
        ". A person who may edit the map adds what is missing on the entity: engine (what it \
         speaks), host or address, port, and credential (a vault handle)",
    );
    why
}

/// What `access` makes this tool do: a pure function of the answer.
pub(crate) fn plan(access: &Access) -> Plan {
    if let Some(word) = &access.unknown_engine {
        return Plan::NoAccess(format!(
            "no_access: the map names the engine {word:?}, which this device does not speak \
             ({})",
            engines()
        ));
    }
    if !access.missing.is_empty() {
        return Plan::NoAccess(no_access(&access.missing, &access.proposed));
    }
    // What the server compiled, read again here: an answer that names no
    // engine this build speaks, no upstream, or no credential for an engine
    // that needs one opens nothing.
    let engine = access.engine.as_deref().and_then(Engine::from_word);
    let upstream = access.upstream.as_deref().filter(|u| !u.is_empty());
    let credential = access.credential.as_deref().filter(|c| !c.is_empty());
    match (engine, upstream) {
        (Some(engine), Some(_)) if !engine.needs_credential() || credential.is_some() => {}
        _ => {
            return Plan::NoAccess(
                "no_access: the map's answer names no engine, upstream or credential this device \
                 can use"
                    .into(),
            );
        }
    }
    match access.here.as_str() {
        "verified" => Plan::Here,
        "unmeasured" => Plan::ProbeFirst,
        _ => Plan::Elsewhere,
    }
}

/// No path from here: why, and what the other devices found.
fn elsewhere(access: &Access, why: Option<String>) -> String {
    let others: Vec<String> = access
        .paths
        .iter()
        .filter(|p| p["here"] != json!(true))
        .map(|p| {
            format!(
                "{} by {} over {}, {}",
                p["device_id"].as_str().unwrap_or("a device"),
                p["kind"].as_str().unwrap_or("a probe"),
                p["carrier"].as_str().unwrap_or("an unknown carrier"),
                p["at"].as_str().unwrap_or("lately")
            )
        })
        .collect();
    let mut out = String::from("no path from this device");
    match why {
        Some(why) => out.push_str(&format!(": {why}")),
        None => out.push_str(": its last probe found it unreachable"),
    }
    if others.is_empty() {
        out.push_str(". No other device of the organization reached it lately either");
    } else {
        out.push_str(&format!(
            ". {} other device{} reached it lately ({}): a path through another device is not \
             offered by this device yet. knowhow_verify measures it from here again",
            others.len(),
            if others.len() == 1 { "" } else { "s" },
            others.join("; ")
        ));
    }
    out
}

/// `kind` and `key`, as a session names an entity.
fn named(args: &Value) -> Result<(String, String), String> {
    let text = |field: &str| -> Result<String, String> {
        match args.get(field) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(s)) => Ok(s.trim().to_string()),
            Some(_) => Err(format!("{field} is text")),
        }
    };
    let (kind, key) = (text("kind")?, text("key")?);
    if kind.is_empty() || key.is_empty() {
        return Err("kind and key are required, as knowhow_search shows them".into());
    }
    Ok((kind, key))
}

/// What `vault_connect` is asked for the access: the credential toward its
/// upstream, with the options the entity says.
pub(crate) fn connect_args(access: &Access) -> Value {
    let mut asked = json!({
        "handle": access.credential,
        "engine": access.engine,
        "upstream": access.upstream,
    });
    for option in [
        access_facts::DATABASE,
        access_facts::AUTH,
        access_facts::AUTH_SOURCE,
        access_facts::CA,
    ] {
        if let Some(value) = access.options.get(option) {
            asked[option] = value.clone();
        }
    }
    asked
}

/// One `roomler_connect` call, for the session at `fence`: `open` opens
/// `vault_connect`'s endpoint with the arguments it is given (the daemon's
/// calls `super::vault_connect::call` with the run's vault).
pub(crate) async fn call<F, Fut>(
    knowhow: &dyn KnowhowTools,
    session: ObjectId,
    fence: u64,
    args: &Value,
    open: F,
) -> Result<Value, String>
where
    F: FnOnce(Value) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let (kind, key) = named(args)?;
    let raw = knowhow.access(session, fence, &kind, &key).await?;
    let access: Access = serde_json::from_value(raw)
        .map_err(|e| format!("the map's answer could not be read: {e}"))?;
    let probed = match plan(&access) {
        Plan::NoAccess(why) => return Err(why),
        Plan::Elsewhere => return Err(elsewhere(&access, None)),
        Plan::Here => None,
        Plan::ProbeFirst => {
            match knowhow
                .verify(session, fence, &kind, &key, ProbeKind::Tcp)
                .await
            {
                Ok(probe) if probe["state"] == json!("ok") => Some(probe),
                Ok(probe) => {
                    let state = probe["state"].as_str().unwrap_or("unknown").to_string();
                    return Err(elsewhere(
                        &access,
                        Some(format!(
                            "it had not probed it lately, and the probe from here found it {state}"
                        )),
                    ));
                }
                Err(why) => {
                    return Err(elsewhere(
                        &access,
                        Some(format!(
                            "it had not probed it lately, and no probe from here was asked: {why}"
                        )),
                    ));
                }
            }
        }
    };
    let engine = access.engine.as_deref().unwrap_or_default();
    let mut answer = if engine == Engine::Tcp.as_str() {
        json!({
            "engine": engine,
            "upstream": access.upstream,
            "connection": access.upstream,
            "note": "a tcp entity takes no credential: dial it directly from this device",
        })
    } else {
        open(connect_args(&access)).await?
    };
    if let Value::Object(map) = &mut answer {
        map.insert("entity".into(), json!({ "kind": kind, "key": key }));
        map.insert("path".into(), json!("direct"));
        if let Some(probe) = probed {
            map.insert("probed_from_here".into(), probe);
        }
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The map as a test says it: one access, and one verify's answer, each
    /// call recorded.
    struct Fake {
        access: Value,
        verify: Result<Value, String>,
        asked: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(access: Value, verify: Result<Value, String>) -> Self {
            Self {
                access,
                verify,
                asked: Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl KnowhowTools for Fake {
        fn offered(&self) -> Vec<&'static str> {
            Vec::new()
        }
        async fn search(
            &self,
            _: ObjectId,
            _: u64,
            _: &super::super::knowhow_tools::Search,
        ) -> Result<Value, String> {
            unreachable!("a connect never searches")
        }
        async fn get(&self, _: ObjectId, _: u64, _: &str, _: &str) -> Result<Value, String> {
            unreachable!("a connect never reads a card")
        }
        async fn propose(&self, _: ObjectId, _: u64, _: &Value, _: &str) -> Result<Value, String> {
            unreachable!("a connect never proposes")
        }
        async fn verify(
            &self,
            _: ObjectId,
            _: u64,
            kind: &str,
            key: &str,
            probe: ProbeKind,
        ) -> Result<Value, String> {
            self.asked
                .lock()
                .unwrap()
                .push(format!("verify {kind}/{key} {}", probe.as_str()));
            self.verify.clone()
        }
        async fn access(
            &self,
            _: ObjectId,
            _: u64,
            kind: &str,
            key: &str,
        ) -> Result<Value, String> {
            self.asked
                .lock()
                .unwrap()
                .push(format!("access {kind}/{key}"));
            Ok(self.access.clone())
        }
    }

    /// A Postgres the map says all of, its credential confirmed; this device
    /// said `here`.
    fn postgres(here: &str) -> Value {
        json!({
            "entity": {"kind": "database", "key": "prod-pg"},
            "engine": "postgres", "upstream": "db.example.com:5432",
            "host": "db.example.com", "port": 5432,
            "credential": "db/prod", "options": {"database": "app", "user": "ignored"},
            "missing": [], "proposed": [], "here": here,
            "paths": [
                {"device_id": "6adc000000000000000000a1", "here": true, "kind": "tls",
                 "carrier": "direct", "at": "2026-10-26T08:00:00Z"},
                {"device_id": "6adc000000000000000000a2", "here": false, "kind": "tcp",
                 "carrier": "overlay", "at": "2026-10-26T07:00:00Z"},
            ],
            "seen": [], "fresh_secs": 86400,
        })
    }

    fn access(v: Value) -> Access {
        serde_json::from_value(v).unwrap()
    }

    fn db() -> Value {
        json!({"kind": "database", "key": "prod-pg"})
    }

    /// What is missing is refused `no_access`, naming each path and what a
    /// session only proposed; an engine this build does not speak, or an
    /// answer that cannot be opened, too. This device's word decides the
    /// rest, and one it does not know is no path.
    #[test]
    fn the_access_decides_what_the_tool_does() {
        let mut missing = postgres("verified");
        missing["missing"] = json!(["credential", "port"]);
        missing["proposed"] = json!(["credential"]);
        let Plan::NoAccess(why) = plan(&access(missing)) else {
            panic!("missing facts open nothing");
        };
        assert!(why.starts_with("no_access:"), "{why}");
        assert!(why.contains("no credential and port facts"), "{why}");
        assert!(why.contains("A session proposed credential"), "{why}");
        let mut unknown = postgres("verified");
        unknown["unknown_engine"] = json!("mysql");
        assert!(
            matches!(plan(&access(unknown)), Plan::NoAccess(w) if w.contains("\"mysql\"")),
            "an engine this build does not speak"
        );
        let mut bare = postgres("verified");
        bare["credential"] = Value::Null;
        assert!(
            matches!(plan(&access(bare)), Plan::NoAccess(_)),
            "postgres needs a credential, whatever the server said"
        );
        assert_eq!(plan(&access(postgres("verified"))), Plan::Here);
        assert_eq!(plan(&access(postgres("unmeasured"))), Plan::ProbeFirst);
        assert_eq!(plan(&access(postgres("unreachable"))), Plan::Elsewhere);
        assert_eq!(plan(&access(postgres("teleported"))), Plan::Elsewhere);
        let mut tcp = postgres("verified");
        tcp["engine"] = json!("tcp");
        tcp["credential"] = Value::Null;
        assert_eq!(plan(&access(tcp)), Plan::Here, "tcp takes no credential");
    }

    /// Verified from here: `vault_connect` is asked for the credential toward
    /// the upstream with the entity's options (never `user`, which is in an
    /// ssh upstream), and its answer names the path; no probe is asked.
    #[tokio::test]
    async fn a_path_from_here_opens_vault_connects_endpoint() {
        let map = Fake::new(postgres("verified"), Err("never".into()));
        let opened = Mutex::new(Vec::new());
        let answer = call(&map, ObjectId::new(), 3, &db(), |asked| {
            opened.lock().unwrap().push(asked);
            async { Ok(json!({"engine": "postgres", "uri": "postgresql://127.0.0.1:41000/app"})) }
        })
        .await
        .unwrap();
        assert_eq!(
            opened.into_inner().unwrap(),
            [json!({"handle": "db/prod", "engine": "postgres",
                    "upstream": "db.example.com:5432", "database": "app"})]
        );
        assert_eq!(answer["path"], json!("direct"));
        assert_eq!(answer["entity"], db());
        assert_eq!(answer["uri"], json!("postgresql://127.0.0.1:41000/app"));
        assert_eq!(map.asked(), ["access database/prod-pg"]);
    }

    /// Unmeasured: one tcp probe from here first, and only when it found the
    /// target is the endpoint opened, the probe beside it.
    #[tokio::test]
    async fn an_unmeasured_target_is_probed_from_here_first() {
        let map = Fake::new(
            postgres("unmeasured"),
            Ok(json!({"probe_id": "p1", "state": "ok", "rtt_ms": 4})),
        );
        let answer = call(&map, ObjectId::new(), 3, &db(), |_| async {
            Ok(json!({"engine": "postgres"}))
        })
        .await
        .unwrap();
        assert_eq!(
            map.asked(),
            ["access database/prod-pg", "verify database/prod-pg tcp"]
        );
        assert_eq!(answer["probed_from_here"]["state"], json!("ok"));
        assert_eq!(answer["path"], json!("direct"));
    }

    /// Nothing is dialled that no probe from here found: a probe that found
    /// it unreachable, a probe the starter may not ask, and a last finding
    /// already unreachable each open nothing, and say which other devices
    /// reached it.
    #[tokio::test]
    async fn no_path_from_here_opens_nothing() {
        let cases = [
            (
                postgres("unmeasured"),
                Ok(json!({"probe_id": "p1", "state": "unreachable"})),
                "found it unreachable",
                2,
            ),
            (
                postgres("unmeasured"),
                Err("the person who started this session may not do that".into()),
                "no probe from here was asked: the person",
                2,
            ),
            (
                postgres("unreachable"),
                Err("never".into()),
                "its last probe found it unreachable",
                1,
            ),
        ];
        for (said, verify, why, asks) in cases {
            let map = Fake::new(said, verify);
            let opened = Mutex::new(0);
            let err = call(&map, ObjectId::new(), 3, &db(), |_| {
                *opened.lock().unwrap() += 1;
                async { Ok(json!({})) }
            })
            .await
            .unwrap_err();
            assert_eq!(*opened.lock().unwrap(), 0, "nothing opened: {err}");
            assert!(err.starts_with("no path from this device"), "{err}");
            assert!(err.contains(why), "{err}");
            assert!(
                err.contains("6adc000000000000000000a2 by tcp over overlay"),
                "the other device that reached it: {err}"
            );
            assert!(!err.contains("6adc000000000000000000a1"), "{err}");
            assert_eq!(map.asked().len(), asks, "{:?}", map.asked());
        }
    }

    /// A `tcp` entity takes no credential: the answer says where to dial, and
    /// no endpoint is opened. What the map lacks is refused before anything
    /// is probed or opened, and so is a call that names no entity.
    #[tokio::test]
    async fn tcp_takes_no_credential_and_a_refusal_opens_nothing() {
        let mut tcp = postgres("verified");
        tcp["engine"] = json!("tcp");
        tcp["upstream"] = json!("10.0.4.12:6379");
        tcp["credential"] = Value::Null;
        let map = Fake::new(tcp, Err("never".into()));
        let answer = call(&map, ObjectId::new(), 3, &db(), |_| async {
            Err::<Value, String>("no endpoint for tcp".into())
        })
        .await
        .unwrap();
        assert_eq!(answer["connection"], json!("10.0.4.12:6379"));
        assert_eq!(answer["path"], json!("direct"));

        let mut lacking = postgres("verified");
        lacking["missing"] = json!(["engine"]);
        let map = Fake::new(lacking, Err("never".into()));
        let err = call(&map, ObjectId::new(), 3, &db(), |_| async {
            Err::<Value, String>("never opened".into())
        })
        .await
        .unwrap_err();
        assert!(err.starts_with("no_access:"), "{err}");
        assert_eq!(map.asked(), ["access database/prod-pg"]);

        let map = Fake::new(postgres("verified"), Err("never".into()));
        let err = call(
            &map,
            ObjectId::new(),
            3,
            &json!({"kind": "database"}),
            |_| async { Err::<Value, String>("never opened".into()) },
        )
        .await
        .unwrap_err();
        assert!(err.contains("kind and key are required"), "{err}");
        assert!(map.asked().is_empty(), "nothing asked of the map");
    }
}
