// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3 — knowhow's scan vocabulary: what a device says it found, and
//! the words and bounds both ends read (the FR-90 spec, §3e "P4-3, scans, as
//! designed").
//!
//! The frames (`crate::signaling`): the server's `rc:knowhow.scan` and the
//! device's `rc:knowhow.scanned`. A finding is metadata about the device
//! itself (a checkout, a kube context, a port it listens on, a tunnel route
//! it declares), never a file's contents and never a credential.
//!
//! ⚠️ Findings travel as raw JSON and are read one at a time
//! ([`Finding::read`]): a kind this build does not know is skipped and
//! counted, and a known kind with a field this build does not know is
//! refused. So a newer device's answer is never lost whole, and no known
//! kind can carry an extra field: a closed shape is what keeps a credential
//! from riding along in one nobody looks at.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// At most this many findings in one answer; the device stops there and says
/// the scan was cut.
pub const MAX_FINDINGS: usize = 2_000;
/// An answer's findings, serialised, at most: well under the socket's 8 MiB.
pub const MAX_SCAN_BYTES: usize = 1024 * 1024;
/// A name, a remote, a server, an image: at most this many bytes.
pub const MAX_TEXT: usize = 512;
/// A path: at most this many bytes.
pub const MAX_PATH: usize = 4_096;
/// A checkout's remotes, at most.
pub const MAX_REMOTES: usize = 16;
/// A checkout's manifests' services, at most.
pub const MAX_SERVICES: usize = 64;
/// An answer's `detail`, at most, in characters.
pub const MAX_DETAIL: usize = 200;

/// How a scan ended, as the device says it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanOutcome {
    /// Scanned; the findings are what it found.
    Done,
    /// Its owner lists no root: an answer, and the only one a device with an
    /// empty `knowhow_scan_roots` gives (decision 38).
    NoRoots,
    /// It could not scan; why is the answer's `detail`.
    Failed,
}

impl ScanOutcome {
    /// The spelling on the wire and in the record. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::NoRoots => "no_roots",
            Self::Failed => "failed",
        }
    }
}

/// An answer that names no outcome failed.
pub(crate) fn failed() -> ScanOutcome {
    ScanOutcome::Failed
}

/// A word a newer device sends, or none, is `failed`: an answer must never
/// read as a scan done when it was not.
pub(crate) fn outcome_lenient<'de, D>(de: D) -> Result<ScanOutcome, D::Error>
where
    D: Deserializer<'de>,
{
    let Value::String(word) = Value::deserialize(de)? else {
        return Ok(ScanOutcome::Failed);
    };
    Ok(ScanOutcome::deserialize(
        serde::de::value::StrDeserializer::<serde::de::value::Error>::new(word.as_str()),
    )
    .unwrap_or(ScanOutcome::Failed))
}

/// A git checkout under one of the device's roots.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Checkout {
    /// Where it is on the device, as the device spells it.
    pub path: String,
    #[serde(default)]
    pub remotes: Vec<Remote>,
    /// The repository's first commit: what a fork and its upstream share.
    #[serde(default)]
    pub root_commit: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    /// The services its compose and Kubernetes manifests name.
    #[serde(default)]
    pub services: Vec<ManifestService>,
}

/// A checkout's remote, as `git remote -v` names it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub name: String,
    pub url: String,
}

/// A service a manifest in a checkout names.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestService {
    pub name: String,
    #[serde(default)]
    pub image: Option<String>,
    /// The manifest, relative to its checkout.
    #[serde(default)]
    pub file: Option<String>,
}

/// A kube context the device's account holds. Its server and CA, never its
/// user: the device never reads one (P4-3b-0 withdrew the vault offer).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KubeContext {
    pub name: String,
    pub server: String,
    #[serde(default)]
    pub namespace: Option<String>,
    /// The CA's SHA-256, hex.
    #[serde(default)]
    pub ca_sha256: Option<String>,
}

/// A port the device listens on.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    pub port: u16,
    /// `tcp` or `udp`.
    pub protocol: String,
    /// The address it is bound to.
    pub address: String,
    #[serde(default)]
    pub process: Option<String>,
}

/// A tunnel route the device declares (`[[tunnel_routes]]`): the
/// route's descriptor less its transport and organization.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TunnelRoute {
    pub id: String,
    /// The descriptor's kind, `forward` or `socks5`, as it spells it. Named
    /// `flow` here: `kind` is the finding's own.
    pub flow: String,
    /// Its loopback listen port.
    pub local: u16,
    /// The node it reaches (hex agent id).
    pub node: String,
    /// `host:port` for a forward.
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

fn enabled() -> bool {
    true
}

/// One thing a scan found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    Checkout(Checkout),
    Context(KubeContext),
    Listener(Listener),
    TunnelRoute(TunnelRoute),
}

/// Why a finding of a known kind was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Not an object, or no `kind`.
    NotAFinding,
    /// A field this build does not know, a missing one, or a wrong type.
    Malformed(String),
    /// A value past its bound.
    TooLong(&'static str),
    /// A value outside its words (a protocol, a route's kind).
    Unknown(&'static str),
    /// A string holding a credential's shape ([`holds_credential`]).
    Credential(&'static str),
}

impl Finding {
    /// Its `kind` on the wire.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Checkout(_) => "checkout",
            Self::Context(_) => "context",
            Self::Listener(_) => "listener",
            Self::TunnelRoute(_) => "tunnel_route",
        }
    }

    /// The wire form: its fields, and `kind` beside them.
    pub fn to_value(&self) -> Value {
        let fields = match self {
            Self::Checkout(f) => serde_json::to_value(f),
            Self::Context(f) => serde_json::to_value(f),
            Self::Listener(f) => serde_json::to_value(f),
            Self::TunnelRoute(f) => serde_json::to_value(f),
        };
        let mut obj = match fields {
            Ok(Value::Object(obj)) => obj,
            _ => Map::new(),
        };
        obj.insert("kind".into(), Value::String(self.kind().into()));
        Value::Object(obj)
    }

    /// Read one finding: `Ok(None)` for a kind this build does not know, so a
    /// newer device's other findings are still read.
    pub fn read(v: &Value) -> Result<Option<Finding>, Refused> {
        let Value::Object(obj) = v else {
            return Err(Refused::NotAFinding);
        };
        let Some(Value::String(kind)) = obj.get("kind") else {
            return Err(Refused::NotAFinding);
        };
        let mut fields = obj.clone();
        fields.remove("kind");
        let fields = Value::Object(fields);
        let malformed = |e: serde_json::Error| Refused::Malformed(e.to_string());
        let finding = match kind.as_str() {
            "checkout" => Finding::Checkout(serde_json::from_value(fields).map_err(malformed)?),
            "context" => Finding::Context(serde_json::from_value(fields).map_err(malformed)?),
            "listener" => Finding::Listener(serde_json::from_value(fields).map_err(malformed)?),
            "tunnel_route" => {
                Finding::TunnelRoute(serde_json::from_value(fields).map_err(malformed)?)
            }
            _ => return Ok(None),
        };
        finding.check()?;
        Ok(Some(finding))
    }

    /// Its bounds, its words, and no credential in any of its strings.
    pub fn check(&self) -> Result<(), Refused> {
        match self {
            Self::Checkout(c) => {
                text("path", &c.path, MAX_PATH)?;
                if c.remotes.len() > MAX_REMOTES {
                    return Err(Refused::TooLong("remotes"));
                }
                for r in &c.remotes {
                    text("remote", &r.name, MAX_TEXT)?;
                    text("remote", &r.url, MAX_TEXT)?;
                }
                opt_text("root_commit", c.root_commit.as_deref(), MAX_TEXT)?;
                opt_text("branch", c.branch.as_deref(), MAX_TEXT)?;
                if c.services.len() > MAX_SERVICES {
                    return Err(Refused::TooLong("services"));
                }
                for s in &c.services {
                    text("service", &s.name, MAX_TEXT)?;
                    opt_text("service", s.image.as_deref(), MAX_TEXT)?;
                    opt_text("service", s.file.as_deref(), MAX_PATH)?;
                }
            }
            Self::Context(k) => {
                text("context", &k.name, MAX_TEXT)?;
                text("server", &k.server, MAX_TEXT)?;
                opt_text("namespace", k.namespace.as_deref(), MAX_TEXT)?;
                opt_text("ca_sha256", k.ca_sha256.as_deref(), MAX_TEXT)?;
            }
            Self::Listener(l) => {
                if !matches!(l.protocol.as_str(), "tcp" | "udp") {
                    return Err(Refused::Unknown("protocol"));
                }
                text("address", &l.address, MAX_TEXT)?;
                opt_text("process", l.process.as_deref(), MAX_TEXT)?;
            }
            Self::TunnelRoute(t) => {
                if !matches!(t.flow.as_str(), "forward" | "socks5") {
                    return Err(Refused::Unknown("flow"));
                }
                text("id", &t.id, MAX_TEXT)?;
                text("node", &t.node, MAX_TEXT)?;
                opt_text("remote", t.remote.as_deref(), MAX_TEXT)?;
            }
        }
        Ok(())
    }
}

fn text(field: &'static str, s: &str, max: usize) -> Result<(), Refused> {
    if s.len() > max {
        return Err(Refused::TooLong(field));
    }
    if holds_credential(s) {
        return Err(Refused::Credential(field));
    }
    Ok(())
}

fn opt_text(field: &'static str, s: Option<&str>, max: usize) -> Result<(), Refused> {
    s.map_or(Ok(()), |s| text(field, s, max))
}

/// The prefixes of the tokens this build knows by shape, as each is spelled:
/// GitHub's, GitLab's, Slack's, Stripe's live keys. An AWS access key has a
/// shape of its own ([`aws_key_shaped`]).
const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "sk_live_",
    "rk_live_",
];

/// Does `s` hold a credential's shape? A URL's password (`user:secret@`), a
/// token by a prefix this build knows, an AWS access key, a PEM block, or a
/// JWT. Shapes, not proof: what it cannot see it cannot refuse, which is why
/// a scan reads metadata alone and a context's token never leaves the
/// device.
pub fn holds_credential(s: &str) -> bool {
    if s.contains("-----BEGIN") || url_password(s) {
        return true;
    }
    let words = |dots: bool| {
        s.split(move |c: char| {
            !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || (dots && c == '.'))
        })
    };
    words(false).any(|w| token_shaped(w) || aws_key_shaped(w)) || words(true).any(jwt_shaped)
}

/// `scheme://user:password@host`: a password in the URL's userinfo.
fn url_password(s: &str) -> bool {
    let Some((_, rest)) = s.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let Some((userinfo, _host)) = authority.rsplit_once('@') else {
        return false;
    };
    userinfo
        .split_once(':')
        .is_some_and(|(_, pw)| !pw.is_empty())
        || token_shaped(userinfo)
}

/// A known prefix, then a body long enough to be a token's.
fn token_shaped(word: &str) -> bool {
    TOKEN_PREFIXES.iter().any(|p| {
        word.strip_prefix(p).is_some_and(|body| {
            body.len() >= 16
                && body
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    })
}

/// `AKIA` or `ASIA`, then sixteen upper-case letters and digits.
fn aws_key_shaped(word: &str) -> bool {
    word.len() == 20
        && (word.starts_with("AKIA") || word.starts_with("ASIA"))
        && word[4..]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// Three base64url segments, the first a JSON object's opening (`eyJ`).
fn jwt_shaped(word: &str) -> bool {
    let parts: Vec<&str> = word.split('.').collect();
    parts.len() == 3
        && parts[0].starts_with("eyJ")
        && parts.iter().all(|p| {
            p.len() >= 8
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A token's shape, built here and never written down whole: a scanner
    /// reading this repository must not take a test for a leak.
    fn token(prefix: &str) -> String {
        format!("{prefix}{}", "a1B2".repeat(9))
    }

    /// A commit's shape, built here for the same reason.
    fn commit() -> String {
        "0123456789abcdef".repeat(3)[..40].to_string()
    }

    fn checkout() -> Value {
        json!({
            "kind": "checkout",
            "path": "/home/dev/src/api",
            "remotes": [{ "name": "origin", "url": "git@github.com:acme/api.git" }],
            "root_commit": commit(),
            "branch": "main",
            "services": [{ "name": "api", "image": "ghcr.io/acme/api:1.2", "file": "deploy/k8s.yaml" }],
        })
    }

    /// WIRE LOCK — the outcome words; a word a newer device sends, or none,
    /// is `failed`.
    #[test]
    fn outcomes_are_locked_and_an_unknown_one_failed() {
        for (o, w) in [
            (ScanOutcome::Done, "done"),
            (ScanOutcome::NoRoots, "no_roots"),
            (ScanOutcome::Failed, "failed"),
        ] {
            assert_eq!(o.as_str(), w);
            assert_eq!(serde_json::to_value(o).unwrap(), json!(w));
        }
        #[derive(Deserialize)]
        struct W {
            #[serde(deserialize_with = "outcome_lenient")]
            o: ScanOutcome,
        }
        for v in [json!({"o": "partial"}), json!({"o": 3}), json!({"o": null})] {
            let w: W = serde_json::from_value(v).unwrap();
            assert_eq!(w.o, ScanOutcome::Failed);
        }
    }

    /// Every kind reads back what it wrote, and `kind` names it.
    #[test]
    fn a_finding_round_trips_with_its_kind() {
        let findings = [
            Finding::read(&checkout()).unwrap().unwrap(),
            Finding::Context(KubeContext {
                name: "prod".into(),
                server: "https://k8s.example.com:6443".into(),
                namespace: Some("api".into()),
                ca_sha256: Some("ab".repeat(32)),
            }),
            Finding::Listener(Listener {
                port: 5432,
                protocol: "tcp".into(),
                address: "0.0.0.0".into(),
                process: Some("postgres".into()),
            }),
            Finding::TunnelRoute(TunnelRoute {
                id: "db".into(),
                flow: "forward".into(),
                local: 15432,
                node: bson::oid::ObjectId::new().to_hex(),
                remote: Some("10.0.4.12:5432".into()),
                enabled: true,
            }),
        ];
        for f in findings {
            let v = f.to_value();
            assert_eq!(v["kind"], json!(f.kind()));
            assert_eq!(Finding::read(&v).unwrap(), Some(f));
        }
    }

    /// A kind this build does not know is skipped, never an error, so a newer
    /// device's other findings are still read.
    #[test]
    fn an_unknown_kind_is_skipped() {
        assert_eq!(
            Finding::read(&json!({"kind": "bucket", "name": "logs"})).unwrap(),
            None
        );
    }

    /// A known kind is closed: an extra field refuses it, and so does a
    /// missing one, a wrong word, or no kind at all.
    #[test]
    fn a_known_kind_is_closed() {
        let mut extra = checkout();
        extra["token"] = json!("x");
        assert!(matches!(Finding::read(&extra), Err(Refused::Malformed(_))));
        assert!(matches!(
            Finding::read(&json!({"kind": "checkout"})),
            Err(Refused::Malformed(_))
        ));
        assert_eq!(
            Finding::read(
                &json!({"kind": "listener", "port": 1, "protocol": "sctp", "address": "::"})
            ),
            Err(Refused::Unknown("protocol"))
        );
        assert_eq!(
            Finding::read(&json!({"path": "/x"})),
            Err(Refused::NotAFinding)
        );
        assert_eq!(Finding::read(&json!("checkout")), Err(Refused::NotAFinding));
    }

    /// A value past its bound refuses the finding.
    #[test]
    fn a_value_past_its_bound_is_refused() {
        let mut long = checkout();
        long["path"] = json!("/".repeat(MAX_PATH + 1));
        assert_eq!(Finding::read(&long), Err(Refused::TooLong("path")));
        let mut many = checkout();
        many["remotes"] = json!(vec![json!({"name": "o", "url": "u"}); MAX_REMOTES + 1]);
        assert_eq!(Finding::read(&many), Err(Refused::TooLong("remotes")));
    }

    /// A credential's shape anywhere in a finding refuses it whole.
    #[test]
    fn a_credential_shape_is_refused() {
        let pat = token("ghp_");
        for url in [
            format!("https://dev:{pat}@github.com/acme/api.git"),
            format!("https://{pat}@github.com/acme/api.git"),
            format!("https://dev:{}@git.example.com/api.git", "hunter2"),
        ] {
            let mut c = checkout();
            c["remotes"] = json!([{ "name": "origin", "url": url }]);
            assert_eq!(
                Finding::read(&c),
                Err(Refused::Credential("remote")),
                "{url}"
            );
        }
        let pem = format!("{}{}", "-----BEG", "IN CERTIFICATE-----");
        let jwt = format!(
            "eyJ{}.{}.{}",
            "x".repeat(10),
            "y".repeat(10),
            "z".repeat(10)
        );
        let aws = format!("AKIA{}", "ABCDEFGH23456789");
        for value in [
            pem,
            jwt,
            aws,
            token("glpat-"),
            format!("{}.", token("ghs_")),
        ] {
            let v = json!({"kind": "context", "name": "prod", "server": "https://k8s:6443", "namespace": value});
            assert_eq!(
                Finding::read(&v),
                Err(Refused::Credential("namespace")),
                "{v}"
            );
        }
    }

    /// What is not a credential stays readable: an ssh remote's user, a
    /// user-only https remote, a commit hash, an image digest, a UUID path,
    /// a bare prefix, and words that only start like a key.
    #[test]
    fn what_is_not_a_credential_is_read() {
        let digest = format!("ghcr.io/acme/api@sha256:{}", "0123456789abcdef".repeat(4));
        let head = commit();
        for s in [
            "git@github.com:acme/api.git",
            "https://dev@github.com/acme/api.git",
            head.as_str(),
            digest.as_str(),
            "/srv/7e0f8a3c-2b1d-4c5e-9f6a-0b1c2d3e4f5a/api.git",
            "ghp_",
            "asia-pacific-deployments",
            "akiak-is-not-a-key-at-all",
        ] {
            assert!(!holds_credential(s), "{s}");
        }
    }
}
