// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-6-1b — `roomler_connect`, the device's half (the FR-90 spec, §3e
//! "P4-6, `roomler_connect`, as designed"; `docs/knowhow.md` §12.1, §13.7).
//!
//! A session names an entity of the organization's map. The server answers
//! what reaching it takes (`rc:knowhow.access`, P4-6-1a, read as [`Access`]),
//! and this opens it, a person approving each call ([`plan`], [`call`]):
//!
//! | The access says | The tool |
//! |---|---|
//! | something is missing, or an engine this build does not speak | refuses `no_access`, naming each path, and which of them a session only proposed |
//! | this device's last finding is `ok` and fresh (`verified`) | opens it from here: what `vault_connect` opens, toward the access's upstream with its credential; a `tcp` entity takes none, and is answered where to dial |
//! | this device has no fresh finding (`unmeasured`) | asks one probe from here first, the session's own `knowhow_verify`, and opens it from here when the probe found it |
//! | otherwise | (P4-6-1b-2) through each other device that reached it lately, newest first, until one opens ([`Reach`]); a refusal no device changes ends the trying ([`Failed`]) |
//!
//! Through a device (P4-6-1b-2): a credentialed engine's upstream connection
//! rides a flow toward the upstream that only this daemon may use, and TLS is
//! verified against the upstream's host, so the device in between relays
//! ciphertext. `tcp` is the forward itself, served to the session's account,
//! as `net_forward` serves it; `ssh` is `vault_connect`'s agent beside such a
//! forward, since the agent signs and never dials, opened once the forward
//! has.
//!
//! ⚠️ Measured, never assumed (pillar 2's first commitment): nothing is
//! dialled that no device's probe found.
//!
//! ⚠️ The probe from here is the session's `knowhow_verify`, held to its
//! starter's `KNOWHOW_EDIT` (decision 54): a probe writes what it measured
//! onto the map. A starter who may only read the map has no probe asked.

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
    /// The upstream's host and port, as the upstream names them.
    pub host: Option<String>,
    pub port: Option<u16>,
    pub credential: Option<String>,
    pub options: Map<String, Value>,
    pub missing: Vec<String>,
    pub proposed: Vec<String>,
    /// This device's own word: `verified`, `unreachable` or `unmeasured`.
    pub here: String,
    pub paths: Vec<Value>,
    pub unknown_engine: Option<String>,
}

/// What the tool does with an access, from here.
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

/// P4-6-1b-2 — why a path through another device did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failed {
    /// That path: the vault refused that exit, or its flow was not made. The
    /// next device may still open it. Only a build with a vault makes one
    /// (`toolbelt::DaemonReach`).
    #[cfg_attr(not(feature = "vault"), allow(dead_code))]
    Path(String),
    /// Every path: this device's vault or egress is off, or the vault refused
    /// the credential. No other device changes that, so none is tried.
    Every(String),
}

/// How `roomler_connect` reaches what the map says: the run's vault and its
/// egress. The daemon's opens `vault_connect`'s endpoint and the session's
/// flows (`toolbelt::DaemonReach`); a test's records what it is asked.
#[async_trait::async_trait]
pub(crate) trait Reach: Send + Sync {
    /// `vault_connect`'s endpoint, from this device.
    async fn open(&self, asked: Value) -> Result<Value, String>;
    /// P4-6-1b-2 — `vault_connect`'s endpoint through `exit`: its upstream
    /// connections ride a flow toward `host:port` that only this daemon may
    /// use, and TLS is still verified against `host`.
    async fn open_through(
        &self,
        asked: Value,
        exit: ObjectId,
        host: &str,
        port: u16,
    ) -> Result<Value, Failed>;
    /// P4-6-1b-2 — a forward through `exit` to `host:port`, served to the
    /// session's account (`net_forward`'s).
    async fn forward(&self, exit: ObjectId, host: &str, port: u16) -> Result<Value, Failed>;
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

/// What `access` makes this tool do from here: a pure function of the
/// answer.
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

/// The paths through other devices, as the access lists them: newest first.
fn others(access: &Access) -> Vec<(ObjectId, &Value)> {
    access
        .paths
        .iter()
        .filter(|p| p["here"] != json!(true))
        .filter_map(|p| {
            let id = ObjectId::parse_str(p["device_id"].as_str()?).ok()?;
            Some((id, p))
        })
        .collect()
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

/// The engine the access names (`plan` has checked it is one).
fn engine_of(access: &Access) -> Option<Engine> {
    access.engine.as_deref().and_then(Engine::from_word)
}

/// Open it from this device.
async fn from_here(access: &Access, reach: &dyn Reach) -> Result<Value, String> {
    if engine_of(access) == Some(Engine::Tcp) {
        return Ok(json!({
            "engine": "tcp",
            "upstream": access.upstream,
            "connection": access.upstream,
            "note": "a tcp entity takes no credential: dial it directly from this device",
        }));
    }
    reach.open(connect_args(access)).await
}

/// P4-6-1b-2 — open it through `exit`, the device that reached it.
async fn through(access: &Access, reach: &dyn Reach, exit: ObjectId) -> Result<Value, Failed> {
    let (Some(host), Some(port)) = (access.host.as_deref(), access.port) else {
        return Err(Failed::Every(
            "the map's answer named no host and port to forward to".into(),
        ));
    };
    match engine_of(access) {
        Some(Engine::Tcp) => {
            let mut forward = reach.forward(exit, host, port).await?;
            if let Value::Object(map) = &mut forward {
                map.insert("engine".into(), json!("tcp"));
                map.insert(
                    "note".into(),
                    json!("a tcp entity takes no credential: dial the forward's address"),
                );
            }
            Ok(forward)
        }
        // The agent signs and never dials: the session's own ssh takes the
        // forward, its host key checked as the host's. The forward first, so
        // that the agent, which holds the key, is opened once and only beside
        // a path that opened. Should the vault refuse the key, the forward
        // ends with its grant, as `net_forward`'s does.
        Some(Engine::Ssh) => {
            let forward = reach.forward(exit, host, port).await?;
            let mut agent = reach
                .open(connect_args(access))
                .await
                .map_err(Failed::Every)?;
            let to = forward["port"].as_u64().unwrap_or_default();
            if let Value::Object(map) = &mut agent {
                map.insert("forward".into(), forward);
                map.insert(
                    "note".into(),
                    json!(format!(
                        "ssh through the forward: ssh -p {to} -o HostKeyAlias={host} <user>@127.0.0.1, \
                         with the agent and known_hosts this answer names"
                    )),
                );
            }
            Ok(agent)
        }
        _ => {
            reach
                .open_through(connect_args(access), exit, host, port)
                .await
        }
    }
}

/// Nothing opened: each path tried and why, in the order tried; `stopped`
/// where a refusal no other device changes ended the trying.
fn no_path(tried: &[(String, String)], stopped: bool) -> String {
    let mut out = String::from("no path reached it");
    for (path, why) in tried {
        out.push_str(&format!("; {path}: {why}"));
    }
    if stopped {
        out.push_str(". No other device was tried: that refusal is the same through any of them");
        return out;
    }
    if tried.len() == 1 {
        out.push_str(". No other device of the organization reached it lately");
    }
    out.push_str(". knowhow_verify measures it from here again");
    out
}

/// One `roomler_connect` call, for the session at `fence`, reached through
/// `reach`: from here when a probe from here found it, else through each
/// other device that did, newest first.
pub(crate) async fn call(
    knowhow: &dyn KnowhowTools,
    reach: &dyn Reach,
    session: ObjectId,
    fence: u64,
    args: &Value,
) -> Result<Value, String> {
    let (kind, key) = named(args)?;
    let raw = knowhow.access(session, fence, &kind, &key).await?;
    let access: Access = serde_json::from_value(raw)
        .map_err(|e| format!("the map's answer could not be read: {e}"))?;
    let entity = json!({ "kind": kind, "key": key });
    let here = match plan(&access) {
        Plan::NoAccess(why) => return Err(why),
        Plan::Here => Ok(None),
        Plan::ProbeFirst => match knowhow
            .verify(session, fence, &kind, &key, ProbeKind::Tcp)
            .await
        {
            Ok(probe) if probe["state"] == json!("ok") => Ok(Some(probe)),
            Ok(probe) => Err(format!(
                "it had not probed it lately, and the probe from here found it {}",
                probe["state"].as_str().unwrap_or("unknown")
            )),
            Err(why) => Err(format!(
                "it had not probed it lately, and no probe from here was asked: {why}"
            )),
        },
        Plan::Elsewhere => Err("its last probe found it unreachable".to_string()),
    };
    match here {
        // A path from here that cannot be opened is no reason to go through
        // another device: the vault refused, or the entity is misdescribed.
        Ok(probed) => {
            let mut answer = from_here(&access, reach).await?;
            if let Value::Object(map) = &mut answer {
                map.insert("entity".into(), entity);
                map.insert("path".into(), json!("direct"));
                if let Some(probe) = probed {
                    map.insert("probed_from_here".into(), probe);
                }
            }
            Ok(answer)
        }
        Err(why) => {
            let mut tried = vec![("this device".to_string(), why)];
            for (exit, path) in others(&access) {
                match through(&access, reach, exit).await {
                    Ok(mut answer) => {
                        if let Value::Object(map) = &mut answer {
                            map.insert("entity".into(), entity);
                            map.insert("path".into(), json!(format!("through {exit}")));
                            map.insert("carrier".into(), path["carrier"].clone());
                            map.insert(
                                "tried".into(),
                                json!(
                                    tried
                                        .iter()
                                        .map(|(p, w)| json!({"path": p, "why": w}))
                                        .collect::<Vec<_>>()
                                ),
                            );
                        }
                        return Ok(answer);
                    }
                    Err(Failed::Path(why)) => tried.push((format!("through {exit}"), why)),
                    Err(Failed::Every(why)) => {
                        tried.push((format!("through {exit}"), why));
                        return Err(no_path(&tried, true));
                    }
                }
            }
            Err(no_path(&tried, false))
        }
    }
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

    /// The vault and the egress as a test says them: every call recorded,
    /// each through a device refused unless its exit is `opens`, and every
    /// one with `every` where that is said; `open` refused with `refuse_open`.
    #[derive(Default)]
    struct FakeReach {
        opens: Option<ObjectId>,
        every: Option<String>,
        refuse_open: Option<String>,
        asked: Mutex<Vec<String>>,
    }

    impl FakeReach {
        fn opening_through(exit: ObjectId) -> Self {
            Self {
                opens: Some(exit),
                ..Self::default()
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }

        fn note(&self, what: String) {
            self.asked.lock().unwrap().push(what);
        }

        /// What a path through `exit` gets: `every`'s refusal, else
        /// `answer` where `exit` is `opens`, else that path's refusal.
        fn through(&self, exit: ObjectId, answer: Value) -> Result<Value, Failed> {
            if let Some(why) = &self.every {
                return Err(Failed::Every(why.clone()));
            }
            if self.opens == Some(exit) {
                Ok(answer)
            } else {
                Err(Failed::Path(
                    "the vault refused the forward (net:forward)".into(),
                ))
            }
        }
    }

    #[async_trait::async_trait]
    impl Reach for FakeReach {
        async fn open(&self, asked: Value) -> Result<Value, String> {
            self.note(format!("open {asked}"));
            if let Some(why) = &self.refuse_open {
                return Err(why.clone());
            }
            Ok(json!({"engine": asked["engine"], "uri": "postgresql://127.0.0.1:41000/app"}))
        }
        async fn open_through(
            &self,
            asked: Value,
            exit: ObjectId,
            host: &str,
            port: u16,
        ) -> Result<Value, Failed> {
            self.note(format!(
                "open {} through {exit} to {host}:{port}",
                asked["engine"]
            ));
            self.through(
                exit,
                json!({"engine": asked["engine"], "uri": "postgresql://127.0.0.1:41001/app"}),
            )
        }
        async fn forward(&self, exit: ObjectId, host: &str, port: u16) -> Result<Value, Failed> {
            self.note(format!("forward through {exit} to {host}:{port}"));
            self.through(
                exit,
                json!({"address": "127.0.0.1:41002", "host": "127.0.0.1", "port": 41002}),
            )
        }
    }

    const HERE: &str = "6adc000000000000000000a1";
    const A2: &str = "6adc000000000000000000a2";
    const A3: &str = "6adc000000000000000000a3";

    fn id(hex: &str) -> ObjectId {
        ObjectId::parse_str(hex).unwrap()
    }

    /// A Postgres the map says all of, its credential confirmed; this device
    /// said `here`, and two other devices reached it, `A2` the newer.
    fn postgres(here: &str) -> Value {
        json!({
            "entity": {"kind": "database", "key": "prod-pg"},
            "engine": "postgres", "upstream": "db.example.com:5432",
            "host": "db.example.com", "port": 5432,
            "credential": "db/prod", "options": {"database": "app", "user": "ignored"},
            "missing": [], "proposed": [], "here": here,
            "paths": [
                {"device_id": HERE, "here": true, "kind": "tls",
                 "carrier": "direct", "at": "2026-10-26T08:00:00Z"},
                {"device_id": A2, "here": false, "kind": "tcp",
                 "carrier": "overlay", "at": "2026-10-26T07:00:00Z"},
                {"device_id": A3, "here": false, "kind": "tls",
                 "carrier": "direct", "at": "2026-10-26T06:00:00Z"},
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
    /// rest, and one it does not know is no path from here.
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
    /// ssh upstream), and its answer names the path; no probe is asked, and
    /// no device is gone through.
    #[tokio::test]
    async fn a_path_from_here_opens_vault_connects_endpoint() {
        let map = Fake::new(postgres("verified"), Err("never".into()));
        let reach = FakeReach::default();
        let answer = call(&map, &reach, ObjectId::new(), 3, &db()).await.unwrap();
        assert_eq!(
            reach.asked(),
            [format!(
                "open {}",
                json!({"handle": "db/prod", "engine": "postgres",
                       "upstream": "db.example.com:5432", "database": "app"})
            )]
        );
        assert_eq!(answer["path"], json!("direct"));
        assert_eq!(answer["entity"], db());
        assert_eq!(map.asked(), ["access database/prod-pg"]);
    }

    /// Unmeasured: one tcp probe from here first, and only when it found the
    /// target is the endpoint opened from here, the probe beside it.
    #[tokio::test]
    async fn an_unmeasured_target_is_probed_from_here_first() {
        let map = Fake::new(
            postgres("unmeasured"),
            Ok(json!({"probe_id": "p1", "state": "ok", "rtt_ms": 4})),
        );
        let reach = FakeReach::default();
        let answer = call(&map, &reach, ObjectId::new(), 3, &db()).await.unwrap();
        assert_eq!(
            map.asked(),
            ["access database/prod-pg", "verify database/prod-pg tcp"]
        );
        assert_eq!(answer["probed_from_here"]["state"], json!("ok"));
        assert_eq!(answer["path"], json!("direct"));
        assert_eq!(
            reach.asked().len(),
            1,
            "opened from here: {:?}",
            reach.asked()
        );
    }

    /// P4-6-1b-2 — no path from here: each other device that reached it is
    /// gone through, newest first, until one opens: its endpoint rides a flow
    /// toward the upstream, and the answer names the device, the carrier and
    /// every path tried before it. Nothing is opened from here.
    #[tokio::test]
    async fn no_path_from_here_goes_through_the_devices_that_reached_it() {
        let cases = [
            (
                postgres("unmeasured"),
                Ok(json!({"probe_id": "p1", "state": "unreachable"})),
                "found it unreachable",
            ),
            (
                postgres("unmeasured"),
                Err("the person who started this session may not do that".into()),
                "no probe from here was asked: the person",
            ),
            (
                postgres("unreachable"),
                Err("never".into()),
                "its last probe found it unreachable",
            ),
        ];
        for (said, verify, why) in cases {
            let map = Fake::new(said, verify);
            let reach = FakeReach::opening_through(id(A3));
            let answer = call(&map, &reach, ObjectId::new(), 3, &db()).await.unwrap();
            assert_eq!(
                reach.asked(),
                [
                    format!("open \"postgres\" through {A2} to db.example.com:5432"),
                    format!("open \"postgres\" through {A3} to db.example.com:5432"),
                ],
                "newest first, never from here, never this device"
            );
            assert_eq!(answer["path"], json!(format!("through {A3}")));
            assert_eq!(answer["carrier"], json!("direct"));
            let tried = answer["tried"].as_array().unwrap();
            assert_eq!(tried.len(), 2, "{answer}");
            assert!(tried[0]["why"].as_str().unwrap().contains(why), "{answer}");
            assert_eq!(tried[1]["path"], json!(format!("through {A2}")));
        }
    }

    /// When no device's path opens, nothing is: the answer names each path
    /// tried and why, this device's first.
    #[tokio::test]
    async fn no_path_at_all_opens_nothing_and_says_what_was_tried() {
        let map = Fake::new(postgres("unreachable"), Err("never".into()));
        let reach = FakeReach::default();
        let err = call(&map, &reach, ObjectId::new(), 3, &db())
            .await
            .unwrap_err();
        assert!(
            err.starts_with("no path reached it; this device: its last probe"),
            "{err}"
        );
        assert!(
            err.contains(&format!("through {A2}: the vault refused")),
            "{err}"
        );
        assert!(
            err.contains(&format!("through {A3}: the vault refused")),
            "{err}"
        );
        assert!(
            !err.contains(HERE),
            "this device is never gone through: {err}"
        );
        assert!(
            err.ends_with(". knowhow_verify measures it from here again"),
            "{err}"
        );
    }

    /// A refusal no other device changes ends the trying: the vault's word on
    /// the credential, or this device's egress off. `ssh` opens its agent,
    /// which holds the key, once and only beside a forward that opened.
    #[tokio::test]
    async fn a_refusal_no_device_changes_ends_the_trying() {
        let map = Fake::new(postgres("unreachable"), Err("never".into()));
        let reach = FakeReach {
            every: Some("the vault refused db/prod toward db.example.com:5432".into()),
            ..FakeReach::default()
        };
        let err = call(&map, &reach, ObjectId::new(), 3, &db())
            .await
            .unwrap_err();
        assert_eq!(
            reach.asked(),
            [format!(
                "open \"postgres\" through {A2} to db.example.com:5432"
            )],
            "the older device is not tried"
        );
        assert!(
            err.contains(&format!("through {A2}: the vault refused db/prod")),
            "{err}"
        );
        assert!(err.ends_with("the same through any of them"), "{err}");

        let mut ssh = postgres("unreachable");
        ssh["engine"] = json!("ssh");
        ssh["upstream"] = json!("ssh://deploy@db.example.com");
        ssh["port"] = json!(22);
        let map = Fake::new(ssh.clone(), Err("never".into()));
        let reach = FakeReach::opening_through(id(A3));
        let answer = call(&map, &reach, ObjectId::new(), 3, &db()).await.unwrap();
        let asked = reach.asked();
        assert_eq!(
            asked[..2],
            [
                format!("forward through {A2} to db.example.com:22"),
                format!("forward through {A3} to db.example.com:22"),
            ],
            "{asked:?}"
        );
        assert_eq!(asked.len(), 3, "one agent: {asked:?}");
        assert!(asked[2].starts_with("open "), "{asked:?}");
        assert_eq!(answer["path"], json!(format!("through {A3}")));

        let map = Fake::new(ssh, Err("never".into()));
        let reach = FakeReach {
            refuse_open: Some("the vault refused ssh/deploy".into()),
            ..FakeReach::opening_through(id(A2))
        };
        let err = call(&map, &reach, ObjectId::new(), 3, &db())
            .await
            .unwrap_err();
        assert_eq!(reach.asked().len(), 2, "{:?}", reach.asked());
        assert!(
            err.contains(&format!("through {A2}: the vault refused ssh/deploy")),
            "{err}"
        );
        assert!(err.ends_with("the same through any of them"), "{err}");
    }

    /// `tcp` takes no credential: from here, where to dial; through a device,
    /// the forward itself, served to the session. `ssh` through a device is
    /// the agent beside a forward. What the map lacks is refused before
    /// anything is probed or opened, and so is a call that names no entity.
    #[tokio::test]
    async fn tcp_and_ssh_take_the_forward_and_a_refusal_opens_nothing() {
        let mut tcp = postgres("verified");
        tcp["engine"] = json!("tcp");
        tcp["upstream"] = json!("10.0.4.12:6379");
        tcp["credential"] = Value::Null;
        let map = Fake::new(tcp.clone(), Err("never".into()));
        let reach = FakeReach::default();
        let answer = call(&map, &reach, ObjectId::new(), 3, &db()).await.unwrap();
        assert_eq!(answer["connection"], json!("10.0.4.12:6379"));
        assert!(reach.asked().is_empty(), "nothing opened for tcp from here");

        tcp["here"] = json!("unreachable");
        tcp["host"] = json!("10.0.4.12");
        tcp["port"] = json!(6379);
        let map = Fake::new(tcp, Err("never".into()));
        let reach = FakeReach::opening_through(id(A2));
        let answer = call(&map, &reach, ObjectId::new(), 3, &db()).await.unwrap();
        assert_eq!(
            reach.asked(),
            [format!("forward through {A2} to 10.0.4.12:6379")]
        );
        assert_eq!(answer["address"], json!("127.0.0.1:41002"));
        assert_eq!(answer["path"], json!(format!("through {A2}")));

        let mut ssh = postgres("unreachable");
        ssh["engine"] = json!("ssh");
        ssh["upstream"] = json!("ssh://deploy@db.example.com");
        ssh["port"] = json!(22);
        let map = Fake::new(ssh, Err("never".into()));
        let reach = FakeReach::opening_through(id(A2));
        let answer = call(&map, &reach, ObjectId::new(), 3, &db()).await.unwrap();
        assert_eq!(
            reach.asked()[0],
            format!("forward through {A2} to db.example.com:22"),
            "{:?}",
            reach.asked()
        );
        assert!(
            reach.asked()[1].starts_with("open "),
            "the agent: {:?}",
            reach.asked()
        );
        assert_eq!(answer["forward"]["port"], json!(41002));
        assert!(
            answer["note"]
                .as_str()
                .unwrap()
                .contains("HostKeyAlias=db.example.com")
        );

        let mut lacking = postgres("verified");
        lacking["missing"] = json!(["engine"]);
        let map = Fake::new(lacking, Err("never".into()));
        let reach = FakeReach::default();
        let err = call(&map, &reach, ObjectId::new(), 3, &db())
            .await
            .unwrap_err();
        assert!(err.starts_with("no_access:"), "{err}");
        assert_eq!(map.asked(), ["access database/prod-pg"]);
        assert!(reach.asked().is_empty());

        let map = Fake::new(postgres("verified"), Err("never".into()));
        let err = call(
            &map,
            &reach,
            ObjectId::new(),
            3,
            &json!({"kind": "database"}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("kind and key are required"), "{err}");
        assert!(map.asked().is_empty(), "nothing asked of the map");
    }
}
