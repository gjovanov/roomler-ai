// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Cedar (FR-90 P3-2, `docs/roomler-hive-design.md` §8.4): the vault's
//! schema, the guardrails it seeds, how a policy is checked before it is
//! stored, and the one function that decides.
//!
//! | Piece | Rule |
//! |---|---|
//! | the schema | [`SCHEMA_SRC`]; every stored policy is validated against it, strictly |
//! | default | deny: nothing is allowed that no policy permits |
//! | `forbid` | beats every `permit`, as an explicit deny does in IAM |
//! | guardrails | [`GUARDRAILS`]: code, not rows, so no organization can lose one; they are in every organization's set from its first decision |
//! | a set that fails to build | is an error, and the caller DENIES: a `forbid` that did not parse must never vanish quietly |
//!
//! ⚠️ A policy can only name its own organization: a `Secret` or
//! `SecretFolder` literal must carry the organization's RRN prefix, a `Role`
//! one of the organization's roles, a `Group` one of its roomler roles, a
//! `User` one of its members (that last checked by the route, which can ask
//! the database). A `Session` is never named: sessions come and go, and a
//! policy for one would outlive it.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use bson::oid::ObjectId;
use cedar_policy::{
    Authorizer, Context, Decision, Entities, EntityUid, Policy, PolicyId, PolicySet, Request,
    Schema, ValidationMode, Validator,
};
use serde_json::{Value, json};

use crate::rrn::check_path;

/// The vault's Cedar schema.
pub const SCHEMA_SRC: &str = include_str!("policy.cedarschema");

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::from_cedarschema_str(SCHEMA_SRC)
        .map(|(schema, _warnings)| schema)
        .expect("the vault's Cedar schema parses — a unit test holds it")
});

/// The parsed schema.
pub fn schema() -> &'static Schema {
    &SCHEMA
}

/// The Cedar namespace every vault entity lives in.
pub const NS: &str = "Roomler";

/// The actions a policy may name, as the wire and the audit spell them.
pub const ACTIONS: &[&str] = &[ACTION_ASSUME, ACTION_USE, ACTION_LIST];
pub const ACTION_ASSUME: &str = "role:assume";
pub const ACTION_USE: &str = "secret:use";
/// P3-4b — a session listing or describing a secret.
pub const ACTION_LIST: &str = "secret:list";

/// The modes a session may ask a secret in (design §8.5). `raw` is not one:
/// it is a person's `secret:read`, never a session's use.
pub const MODES: &[&str] = &[
    "identity",
    "proxy",
    "placeholder",
    "short-lived",
    "file",
    "env",
    "exec",
];

/// The operating systems a device context may name — the spellings the agent
/// rows store (`OsKind`, snake case). Anything else is refused, never
/// lowercased into a match: a guard that assumes a casing is no guard.
pub const OSES: &[&str] = &["linux", "macos", "windows"];

/// A guardrail the vault seeds into every organization's set.
#[derive(Debug, Clone, Copy)]
pub struct Guardrail {
    /// The policy id it is decided under (`guardrail:<name>`).
    pub id: &'static str,
    /// Why it is there, for the listing.
    pub why: &'static str,
    pub text: &'static str,
}

/// The guardrails (design §8.4; decision 17). Organization-independent text:
/// no slug is spliced into Cedar, so no organization's name can change what
/// they say.
pub const GUARDRAILS: &[Guardrail] = &[
    Guardrail {
        id: "guardrail:windows-proxy-only",
        why: "Windows has no Claude Code sandbox, so a session there gets a secret only through \
              a proxy that authenticates for it (AC13; decision 17)",
        text: r#"forbid (principal, action == Roomler::Action::"secret:use", resource)
when { context.device.os == "windows" && context.mode != "proxy" };"#,
    },
    Guardrail {
        id: "guardrail:prod-never-materializes",
        why: "a production value never lands in a file, an environment or a command line: a \
              secret tagged `prod`, or under the folder `prod/`, is used in no materializing mode",
        text: r#"forbid (principal, action == Roomler::Action::"secret:use", resource)
when {
  (resource.tags.contains("prod") || resource.path == "prod" || resource.path like "prod/*") &&
  ["file", "env", "exec"].contains(context.mode)
};"#,
    },
    // P3-4c — the confused deputy's guard (design §8.5): a handle smuggled
    // into any other command resolves to nothing. The device sends the
    // program's bare name as the audience, and runs only that program.
    Guardrail {
        id: "guardrail:exec-only-allowed-programs",
        why: "in `exec` mode a value is handed only to a program its secret names in \
              `exec_allow`, by bare name: a handle smuggled into any other command is refused",
        text: r#"forbid (principal, action == Roomler::Action::"secret:use", resource)
when { context.mode == "exec" && !(resource.exec_allow.contains(context.audience)) };"#,
    },
];

/// What a policy may name of its organization.
#[derive(Debug, Clone, Default)]
pub struct OrgNames {
    /// The organization's RRN prefix for secrets: `rrn:{slug}:secret:`.
    pub secret_prefix: String,
    /// The organization's vault roles.
    pub roles: BTreeSet<String>,
    /// The organization's roomler role names (the `Group`s).
    pub groups: BTreeSet<String>,
}

/// A policy that passed every check: what to store and what it names.
#[derive(Debug, Clone)]
pub struct CheckedPolicy {
    /// `permit` | `forbid`.
    pub effect: &'static str,
    /// The `User` ids it names, for the route to check against membership.
    pub users: BTreeSet<ObjectId>,
}

/// Check one policy before it is stored: exactly one static Cedar policy,
/// valid against the schema in strict mode, naming only its organization.
pub fn check_policy(text: &str, names: &OrgNames) -> Result<CheckedPolicy, String> {
    if text.trim().is_empty() {
        return Err("the policy is empty".into());
    }
    if text.len() > MAX_POLICY_LEN {
        return Err(format!("a policy is at most {MAX_POLICY_LEN} bytes"));
    }
    let policy = Policy::parse(None, text).map_err(|e| format!("not a Cedar policy: {e}"))?;
    let mut set = PolicySet::new();
    set.add(policy.clone())
        .map_err(|e| format!("not a policy: {e}"))?;
    let result = Validator::new(schema().clone()).validate(&set, ValidationMode::Strict);
    if !result.validation_passed() {
        let errors: Vec<String> = result.validation_errors().map(|e| e.to_string()).collect();
        return Err(format!(
            "the policy does not fit the vault's schema: {}",
            errors.join("; ")
        ));
    }
    let est = policy
        .to_json()
        .map_err(|e| format!("the policy cannot be read back: {e}"))?;
    let mut refs = Vec::new();
    entity_refs(&est, &mut refs);
    let mut users = BTreeSet::new();
    for (ty, id) in refs {
        match ty.strip_prefix("Roomler::").unwrap_or(&ty) {
            "Secret" | "SecretFolder" => {
                let Some(path) = id.strip_prefix(&names.secret_prefix) else {
                    return Err(format!(
                        "{ty}::{id:?} is not this organization's: its RRN must begin {:?}",
                        names.secret_prefix
                    ));
                };
                check_path(path).map_err(|why| format!("{ty}::{id:?}: {why}"))?;
            }
            "Role" => {
                if !names.roles.contains(&id) {
                    return Err(format!(
                        "Role::{id:?} is not one of this organization's vault roles"
                    ));
                }
            }
            "Group" => {
                if !names.groups.contains(&id) {
                    return Err(format!(
                        "Group::{id:?} is not one of this organization's roles"
                    ));
                }
            }
            "User" => {
                let user = ObjectId::parse_str(&id)
                    .map_err(|_| format!("User::{id:?} is not a user id"))?;
                users.insert(user);
            }
            "Session" => {
                return Err(
                    "a policy never names one session: name the role sessions assume".into(),
                );
            }
            "Action" => {}
            other => return Err(format!("{other} is not a vault entity type")),
        }
    }
    Ok(CheckedPolicy {
        effect: match policy.effect() {
            cedar_policy::Effect::Permit => "permit",
            cedar_policy::Effect::Forbid => "forbid",
        },
        users,
    })
}

/// The longest policy, in bytes.
pub const MAX_POLICY_LEN: usize = 16 * 1024;

/// Every entity a policy's JSON form names, as `(type, id)`. Entity
/// references are the objects with exactly a string `type` and a string
/// `id`; a record literal's fields are expressions, never bare strings, so
/// one cannot pass for a reference.
fn entity_refs(v: &Value, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(m) => {
            if m.len() == 2
                && let (Some(Value::String(ty)), Some(Value::String(id))) =
                    (m.get("type"), m.get("id"))
            {
                out.push((ty.clone(), id.clone()));
                return;
            }
            for child in m.values() {
                entity_refs(child, out);
            }
        }
        Value::Array(a) => {
            for child in a {
                entity_refs(child, out);
            }
        }
        _ => {}
    }
}

/// Whether a stored policy names `Role::"<role>"` — read from the parsed
/// policy, so no spacing or escape in the text can hide it. A text that no
/// longer parses counts as naming it: the answer guards a deletion.
pub fn names_role(text: &str, role: &str) -> bool {
    let Ok(policy) = Policy::parse(None, text) else {
        return true;
    };
    let Ok(est) = policy.to_json() else {
        return true;
    };
    let mut refs = Vec::new();
    entity_refs(&est, &mut refs);
    refs.iter()
        .any(|(ty, id)| ty == &format!("{NS}::Role") && id == role)
}

/// The policy id a stored policy is decided under.
pub fn stored_id(id: ObjectId) -> String {
    format!("policy:{}", id.to_hex())
}

/// The organization's whole set: the guardrails, then its own policies.
/// ⚠️ A policy that does not parse is an ERROR for the whole set, never a
/// policy skipped: the one skipped could be a `forbid`.
pub fn policy_set<'a>(
    stored: impl IntoIterator<Item = (String, &'a str)>,
) -> Result<PolicySet, String> {
    let mut set = PolicySet::new();
    for g in GUARDRAILS {
        let p = Policy::parse(Some(PolicyId::new(g.id)), g.text)
            .map_err(|e| format!("guardrail {} does not parse: {e}", g.id))?;
        set.add(p).map_err(|e| format!("guardrail {}: {e}", g.id))?;
    }
    for (id, text) in stored {
        let p = Policy::parse(Some(PolicyId::new(id.clone())), text)
            .map_err(|e| format!("stored policy {id} does not parse: {e}"))?;
        set.add(p).map_err(|e| format!("stored policy {id}: {e}"))?;
    }
    Ok(set)
}

/// A device's facts, as a request's context carries them.
#[derive(Debug, Clone)]
pub struct DeviceFacts {
    /// One of [`OSES`].
    pub os: String,
    /// Lowercased by [`DeviceFacts::new`].
    pub tags: Vec<String>,
    pub owner: ObjectId,
}

impl DeviceFacts {
    /// `os` must be one of [`OSES`] exactly; tags are lowercased, because a
    /// device's tags are free-form and a policy compares by equality.
    pub fn new(os: &str, tags: &[String], owner: ObjectId) -> Result<Self, String> {
        if !OSES.contains(&os) {
            return Err(format!("os must be one of {}", OSES.join(", ")));
        }
        Ok(Self {
            os: os.to_string(),
            tags: tags.iter().map(|t| t.trim().to_lowercase()).collect(),
            owner,
        })
    }

    fn json(&self) -> Value {
        json!({ "os": self.os, "tags": self.tags, "owner": user_ref(self.owner) })
    }
}

/// A secret, as a request names it.
#[derive(Debug, Clone)]
pub struct SecretFacts {
    /// `rrn:{slug}:secret:`.
    pub prefix: String,
    pub path: String,
    pub kind: String,
    pub tags: Vec<String>,
    /// P3-4c — the programs a value may be handed to in `exec` mode, by bare
    /// name. Empty: no program at all.
    pub exec_allow: Vec<String>,
}

/// A session, as a `secret:use` names it.
#[derive(Debug, Clone)]
pub struct SessionFacts {
    pub id: String,
    pub owner: ObjectId,
    /// The role it assumed; none means no policy can name it, so nothing is
    /// allowed it.
    pub role: Option<String>,
    pub playbook: Option<String>,
}

/// One request to decide.
#[derive(Debug, Clone)]
pub enum Ask {
    /// `role:assume`: may `user` (in `groups`) assume `role` on `device`?
    AssumeRole {
        user: ObjectId,
        groups: Vec<String>,
        role: String,
        device: DeviceFacts,
    },
    /// `secret:use`: may `session` use `secret` in `mode`?
    UseSecret {
        session: SessionFacts,
        secret: SecretFacts,
        mode: String,
        audience: String,
        ttl_s: i64,
        device: DeviceFacts,
        approver: Option<ObjectId>,
    },
    /// P3-4b — `secret:list`: may `session` list or describe `secret`?
    ListSecret {
        session: SessionFacts,
        secret: SecretFacts,
        device: DeviceFacts,
    },
}

fn uid(ty: &str, id: &str) -> Value {
    json!({ "type": format!("{NS}::{ty}"), "id": id })
}

fn user_ref(id: ObjectId) -> Value {
    json!({ "__entity": uid("User", &id.to_hex()) })
}

fn parse_uid(v: &Value) -> Result<EntityUid, String> {
    EntityUid::from_json(v.clone()).map_err(|e| format!("an entity id: {e}"))
}

/// Every element a string, or `None`.
fn strings(a: &bson::Array) -> Option<Vec<String>> {
    a.iter().map(|b| b.as_str().map(String::from)).collect()
}

/// The session, in the role it assumed, and the secret, in its folders: the
/// principal, the resource and the entities a `secret:*` request is decided
/// over.
fn session_and_secret(session: &SessionFacts, secret: &SecretFacts) -> (Value, Value, Vec<Value>) {
    let session_uid = uid("Session", &session.id);
    let mut attrs = json!({ "owner": user_ref(session.owner) });
    if let Some(p) = &session.playbook {
        attrs["playbook"] = json!(p);
    }
    let parents: Vec<Value> = session.role.iter().map(|r| uid("Role", r)).collect();
    let mut entities = vec![json!({ "uid": session_uid, "attrs": attrs, "parents": parents })];
    if let Some(r) = &session.role {
        entities.push(json!({ "uid": uid("Role", r), "attrs": {}, "parents": [] }));
    }
    let chain = folders(&secret.path);
    let secret_uid = uid("Secret", &format!("{}{}", secret.prefix, secret.path));
    entities.push(json!({
        "uid": secret_uid,
        "attrs": {
            "path": secret.path,
            "kind": secret.kind,
            "tags": secret.tags,
            "exec_allow": secret.exec_allow,
        },
        "parents": chain.first()
            .map(|f| vec![uid("SecretFolder", &format!("{}{f}", secret.prefix))])
            .unwrap_or_default(),
    }));
    for (i, f) in chain.iter().enumerate() {
        let parents: Vec<Value> = chain
            .get(i + 1)
            .map(|up| vec![uid("SecretFolder", &format!("{}{up}", secret.prefix))])
            .unwrap_or_default();
        entities.push(json!({
            "uid": uid("SecretFolder", &format!("{}{f}", secret.prefix)),
            "attrs": {},
            "parents": parents,
        }));
    }
    (session_uid, secret_uid, entities)
}

/// The folders above a path, innermost first: `a/b/c` → `a/b`, `a`.
fn folders(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = path;
    while let Some(i) = rest.rfind('/') {
        rest = &rest[..i];
        out.push(rest);
    }
    out
}

impl Ask {
    /// The action's name.
    pub fn action(&self) -> &'static str {
        match self {
            Self::AssumeRole { .. } => ACTION_ASSUME,
            Self::UseSecret { .. } => ACTION_USE,
            Self::ListSecret { .. } => ACTION_LIST,
        }
    }

    /// P3-2b — the request as `vault_audit` keeps it, so a replay decides
    /// exactly what was decided: the groups, the device's facts, the mode,
    /// as they were then.
    pub fn to_doc(&self) -> bson::Document {
        let device = |d: &DeviceFacts| {
            bson::doc! { "os": &d.os, "tags": &d.tags, "owner": d.owner }
        };
        let session_doc = |s: &SessionFacts| {
            bson::doc! {
                "id": &s.id,
                "owner": s.owner,
                "role": s.role.clone(),
                "playbook": s.playbook.clone(),
            }
        };
        let secret_doc = |r: &SecretFacts| {
            bson::doc! {
                "prefix": &r.prefix,
                "path": &r.path,
                "kind": &r.kind,
                "tags": &r.tags,
                "exec_allow": &r.exec_allow,
            }
        };
        match self {
            Self::AssumeRole {
                user,
                groups,
                role,
                device: d,
            } => bson::doc! {
                "action": ACTION_ASSUME,
                "user": *user,
                "groups": groups,
                "role": role,
                "device": device(d),
            },
            Self::UseSecret {
                session,
                secret,
                mode,
                audience,
                ttl_s,
                device: d,
                approver,
            } => {
                let mut doc = bson::doc! {
                    "action": ACTION_USE,
                    "session": session_doc(session),
                    "secret": secret_doc(secret),
                    "mode": mode,
                    "audience": audience,
                    "ttl_s": *ttl_s,
                    "device": device(d),
                };
                if let Some(a) = approver {
                    doc.insert("approver", *a);
                }
                doc
            }
            Self::ListSecret {
                session,
                secret,
                device: d,
            } => bson::doc! {
                "action": ACTION_LIST,
                "session": session_doc(session),
                "secret": secret_doc(secret),
                "device": device(d),
            },
        }
    }

    /// The inverse of [`Self::to_doc`]; `None` for a row no replay can read
    /// (a later version's shape, or a damaged one).
    pub fn from_doc(doc: &bson::Document) -> Option<Self> {
        let device = |d: &bson::Document| -> Option<DeviceFacts> {
            Some(DeviceFacts {
                os: d.get_str("os").ok()?.to_string(),
                tags: strings(d.get_array("tags").ok()?)?,
                owner: d.get_object_id("owner").ok()?,
            })
        };
        let session = |s: &bson::Document| -> Option<SessionFacts> {
            Some(SessionFacts {
                id: s.get_str("id").ok()?.to_string(),
                owner: s.get_object_id("owner").ok()?,
                role: s.get_str("role").ok().map(String::from),
                playbook: s.get_str("playbook").ok().map(String::from),
            })
        };
        let secret = |r: &bson::Document| -> Option<SecretFacts> {
            Some(SecretFacts {
                prefix: r.get_str("prefix").ok()?.to_string(),
                path: r.get_str("path").ok()?.to_string(),
                kind: r.get_str("kind").ok()?.to_string(),
                tags: strings(r.get_array("tags").ok()?)?,
                // P3-4c — a request recorded before the field existed named
                // no program: decided again, its exec is refused.
                exec_allow: match r.get_array("exec_allow") {
                    Ok(a) => strings(a)?,
                    Err(_) => Vec::new(),
                },
            })
        };
        match doc.get_str("action").ok()? {
            ACTION_ASSUME => Some(Self::AssumeRole {
                user: doc.get_object_id("user").ok()?,
                groups: strings(doc.get_array("groups").ok()?)?,
                role: doc.get_str("role").ok()?.to_string(),
                device: device(doc.get_document("device").ok()?)?,
            }),
            ACTION_USE => Some(Self::UseSecret {
                session: session(doc.get_document("session").ok()?)?,
                secret: secret(doc.get_document("secret").ok()?)?,
                mode: doc.get_str("mode").ok()?.to_string(),
                audience: doc.get_str("audience").ok()?.to_string(),
                ttl_s: doc.get_i64("ttl_s").ok()?,
                device: device(doc.get_document("device").ok()?)?,
                approver: doc.get_object_id("approver").ok(),
            }),
            ACTION_LIST => Some(Self::ListSecret {
                session: session(doc.get_document("session").ok()?)?,
                secret: secret(doc.get_document("secret").ok()?)?,
                device: device(doc.get_document("device").ok()?)?,
            }),
            _ => None,
        }
    }

    /// The Cedar request, and the entities it is decided over, both checked
    /// against the schema.
    pub fn build(&self) -> Result<(Request, Entities), String> {
        let action = parse_uid(&uid("Action", self.action()))?;
        let (principal, resource, entities, context) = match self {
            Self::AssumeRole {
                user,
                groups,
                role,
                device,
            } => {
                let user_uid = uid("User", &user.to_hex());
                let role_uid = uid("Role", role);
                let mut entities = vec![json!({
                    "uid": user_uid,
                    "attrs": {},
                    "parents": groups.iter().map(|g| uid("Group", g)).collect::<Vec<_>>(),
                })];
                for g in groups {
                    entities.push(json!({ "uid": uid("Group", g), "attrs": {}, "parents": [] }));
                }
                entities.push(json!({ "uid": role_uid, "attrs": {}, "parents": [] }));
                (
                    user_uid,
                    role_uid,
                    entities,
                    json!({ "device": device.json() }),
                )
            }
            Self::UseSecret {
                session,
                secret,
                mode,
                audience,
                ttl_s,
                device,
                approver,
            } => {
                let (session_uid, secret_uid, entities) = session_and_secret(session, secret);
                let mut context = json!({
                    "mode": mode,
                    "audience": audience,
                    "ttl_s": ttl_s,
                    "device": device.json(),
                });
                if let Some(a) = approver {
                    context["approval"] = json!({ "approver": user_ref(*a) });
                }
                (session_uid, secret_uid, entities, context)
            }
            Self::ListSecret {
                session,
                secret,
                device,
            } => {
                let (session_uid, secret_uid, entities) = session_and_secret(session, secret);
                (
                    session_uid,
                    secret_uid,
                    entities,
                    json!({ "device": device.json() }),
                )
            }
        };
        let principal = parse_uid(&principal)?;
        let resource = parse_uid(&resource)?;
        let context = Context::from_json_value(context, Some((schema(), &action)))
            .map_err(|e| format!("the request's context: {e}"))?;
        let request = Request::new(principal, action, resource, context, Some(schema()))
            .map_err(|e| format!("the request: {e}"))?;
        let entities = Entities::from_json_value(Value::Array(entities), Some(schema()))
            .map_err(|e| format!("the request's entities: {e}"))?;
        Ok((request, entities))
    }
}

/// A decision, and what made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub allow: bool,
    /// The policies that determined it: the permits that allowed it, or the
    /// forbids that denied it. Empty for a default deny.
    pub determining: Vec<String>,
    /// Policies that erred while evaluating (they count as not applying).
    pub errors: Vec<String>,
}

impl Verdict {
    /// Deny, for a reason outside Cedar (a set that did not build, a request
    /// that did not fit the schema).
    pub fn deny(why: impl Into<String>) -> Self {
        Self {
            allow: false,
            determining: Vec::new(),
            errors: vec![why.into()],
        }
    }
}

/// Decide `ask` against `set`. Anything that keeps a request from being
/// decided is a DENY, with why.
pub fn decide(set: &PolicySet, ask: &Ask) -> Verdict {
    let (request, entities) = match ask.build() {
        Ok(built) => built,
        Err(why) => return Verdict::deny(why),
    };
    let response = Authorizer::new().is_authorized(&request, set, &entities);
    let mut determining: Vec<String> = response
        .diagnostics()
        .reason()
        .map(|id| id.to_string())
        .collect();
    determining.sort();
    Verdict {
        allow: response.decision() == Decision::Allow,
        determining,
        errors: response
            .diagnostics()
            .errors()
            .map(|e| e.to_string())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORG: &str = "rrn:grox:secret:";

    fn names() -> OrgNames {
        OrgNames {
            secret_prefix: ORG.into(),
            roles: ["dev", "deployer"].into_iter().map(String::from).collect(),
            groups: ["admin", "member"].into_iter().map(String::from).collect(),
        }
    }

    fn device(os: &str, tags: &[&str]) -> DeviceFacts {
        let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
        DeviceFacts::new(os, &tags, ObjectId::new()).unwrap()
    }

    fn use_secret(role: Option<&str>, path: &str, tags: &[&str], mode: &str, os: &str) -> Ask {
        Ask::UseSecret {
            session: SessionFacts {
                id: ObjectId::new().to_hex(),
                owner: ObjectId::new(),
                role: role.map(String::from),
                playbook: None,
            },
            // P3-4c — a program the secret names, so an `exec` here meets
            // every guardrail but the one a test is about.
            secret: SecretFacts {
                prefix: ORG.into(),
                path: path.into(),
                kind: "static".into(),
                tags: tags.iter().map(|t| t.to_string()).collect(),
                exec_allow: vec!["psql".into()],
            },
            mode: mode.into(),
            audience: "psql".into(),
            ttl_s: 900,
            device: device(os, &[]),
            approver: None,
        }
    }

    fn list_secret(role: Option<&str>, path: &str, os: &str) -> Ask {
        Ask::ListSecret {
            session: SessionFacts {
                id: ObjectId::new().to_hex(),
                owner: ObjectId::new(),
                role: role.map(String::from),
                playbook: None,
            },
            secret: SecretFacts {
                prefix: ORG.into(),
                path: path.into(),
                kind: "static".into(),
                tags: Vec::new(),
                exec_allow: Vec::new(),
            },
            device: device(os, &[]),
        }
    }

    const DEV_LISTS_STAGING: &str = r#"permit (principal in Roomler::Role::"dev",
        action == Roomler::Action::"secret:list",
        resource in Roomler::SecretFolder::"rrn:grox:secret:staging");"#;

    fn set_with(policies: &[&str]) -> PolicySet {
        policy_set(
            policies
                .iter()
                .enumerate()
                .map(|(i, t)| (format!("policy:{i}"), *t)),
        )
        .unwrap()
    }

    const DEV_STAGING: &str = r#"permit (principal in Roomler::Role::"dev",
        action == Roomler::Action::"secret:use",
        resource in Roomler::SecretFolder::"rrn:grox:secret:staging");"#;

    #[test]
    fn the_schema_and_every_guardrail_hold() {
        let _ = schema();
        let set = policy_set(std::iter::empty()).unwrap();
        assert_eq!(set.policies().count(), GUARDRAILS.len());
        // Each guardrail is valid against the schema, as any stored policy
        // must be.
        let mut only = PolicySet::new();
        for g in GUARDRAILS {
            only.add(Policy::parse(Some(PolicyId::new(g.id)), g.text).unwrap())
                .unwrap();
        }
        let r = Validator::new(schema().clone()).validate(&only, ValidationMode::Strict);
        assert!(
            r.validation_passed(),
            "{:?}",
            r.validation_errors()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn nothing_is_allowed_that_no_policy_permits() {
        let set = policy_set(std::iter::empty()).unwrap();
        let v = decide(
            &set,
            &use_secret(Some("dev"), "staging/db", &[], "proxy", "linux"),
        );
        assert!(!v.allow && v.determining.is_empty(), "{v:?}");
    }

    #[test]
    fn a_permit_allows_inside_its_folder_only() {
        let set = set_with(&[DEV_STAGING]);
        let ok = decide(
            &set,
            &use_secret(Some("dev"), "staging/db/main", &[], "proxy", "linux"),
        );
        assert!(ok.allow, "{ok:?}");
        assert_eq!(ok.determining, ["policy:0"]);
        for ask in [
            use_secret(Some("dev"), "prod/db", &[], "proxy", "linux"),
            use_secret(Some("deployer"), "staging/db", &[], "proxy", "linux"),
            use_secret(None, "staging/db", &[], "proxy", "linux"),
            // A sibling whose name merely starts the same is not inside.
            use_secret(Some("dev"), "staging2/db", &[], "proxy", "linux"),
        ] {
            assert!(!decide(&set, &ask).allow, "{ask:?}");
        }
    }

    #[test]
    fn windows_gets_a_secret_through_a_proxy_only() {
        let any = r#"permit (principal, action == Roomler::Action::"secret:use", resource);"#;
        let set = set_with(&[any]);
        assert!(
            decide(
                &set,
                &use_secret(Some("dev"), "staging/db", &[], "proxy", "windows")
            )
            .allow
        );
        for mode in [
            "file",
            "env",
            "exec",
            "short-lived",
            "identity",
            "placeholder",
        ] {
            let v = decide(
                &set,
                &use_secret(Some("dev"), "staging/db", &[], mode, "windows"),
            );
            assert!(!v.allow, "{mode}: {v:?}");
            assert_eq!(v.determining, ["guardrail:windows-proxy-only"], "{mode}");
        }
        // Linux and macOS are not held to it.
        for os in ["linux", "macos"] {
            assert!(decide(&set, &use_secret(Some("dev"), "staging/db", &[], "env", os)).allow);
        }
    }

    #[test]
    fn a_prod_value_never_materializes() {
        let any = r#"permit (principal, action == Roomler::Action::"secret:use", resource);"#;
        let set = set_with(&[any]);
        for (path, tags) in [
            ("prod", &[][..]),
            ("prod/db", &[][..]),
            ("svc/db", &["prod"][..]),
        ] {
            for mode in ["file", "env", "exec"] {
                let v = decide(&set, &use_secret(Some("ops"), path, tags, mode, "linux"));
                assert!(!v.allow, "{path} {tags:?} {mode}");
                assert_eq!(v.determining, ["guardrail:prod-never-materializes"]);
            }
            assert!(decide(&set, &use_secret(Some("ops"), path, tags, "proxy", "linux")).allow);
        }
        // `production/…` is not `prod/…`.
        assert!(
            decide(
                &set,
                &use_secret(Some("ops"), "production/db", &[], "env", "linux")
            )
            .allow
        );
    }

    /// P3-4c — `exec` hands a value only to a program its secret names, by
    /// bare name; every other mode is not held to the list.
    #[test]
    fn exec_reaches_only_a_program_its_secret_names() {
        let any = r#"permit (principal, action == Roomler::Action::"secret:use", resource);"#;
        let set = set_with(&[any]);
        let with = |audience: &str, allow: &[&str], mode: &str| {
            let mut ask = use_secret(Some("dev"), "staging/db", &[], mode, "linux");
            if let Ask::UseSecret {
                secret,
                audience: a,
                ..
            } = &mut ask
            {
                *a = audience.into();
                secret.exec_allow = allow.iter().map(|p| p.to_string()).collect();
            }
            ask
        };
        let ok = decide(&set, &with("psql", &["psql", "kubectl"], "exec"));
        assert!(ok.allow && ok.errors.is_empty(), "{ok:?}");
        for (audience, allow) in [
            ("curl", &["psql"][..]),
            ("/usr/bin/psql", &["psql"][..]),
            ("psql", &[][..]),
            ("", &["psql"][..]),
        ] {
            let v = decide(&set, &with(audience, allow, "exec"));
            assert!(!v.allow, "{audience:?} {allow:?}: {v:?}");
            assert_eq!(
                v.determining,
                ["guardrail:exec-only-allowed-programs"],
                "{audience:?}"
            );
        }
        for mode in ["env", "file", "proxy"] {
            assert!(decide(&set, &with("curl", &[], mode)).allow, "{mode}");
        }
    }

    /// P3-4c — a request recorded before `exec_allow` existed reads back
    /// naming no program, so decided again its exec is refused.
    #[test]
    fn a_request_recorded_without_exec_allow_reads_back_naming_no_program() {
        let ask = use_secret(Some("dev"), "staging/db", &[], "exec", "linux");
        let mut doc = ask.to_doc();
        doc.get_document_mut("secret")
            .unwrap()
            .remove("exec_allow")
            .expect("written today");
        let back = Ask::from_doc(&doc).expect("still reads");
        let Ask::UseSecret { secret, .. } = &back else {
            panic!("{back:?}")
        };
        assert!(secret.exec_allow.is_empty());
        let any = r#"permit (principal, action == Roomler::Action::"secret:use", resource);"#;
        let v = decide(&set_with(&[any]), &back);
        assert_eq!(v.determining, ["guardrail:exec-only-allowed-programs"]);
    }

    #[test]
    fn a_forbid_beats_every_permit() {
        let any = r#"permit (principal, action == Roomler::Action::"secret:use", resource);"#;
        let no_dev = r#"forbid (principal in Roomler::Role::"dev", action, resource);"#;
        let set = set_with(&[any, no_dev]);
        let v = decide(
            &set,
            &use_secret(Some("dev"), "staging/db", &[], "proxy", "linux"),
        );
        assert!(!v.allow);
        assert_eq!(v.determining, ["policy:1"]);
    }

    #[test]
    fn role_assume_reads_the_device() {
        let p = r#"permit (principal in Roomler::Group::"member",
            action == Roomler::Action::"role:assume", resource == Roomler::Role::"dev")
            when { context.device.owner == principal || context.device.tags.contains("build") };"#;
        let set = set_with(&[p]);
        let me = ObjectId::new();
        let ask = |groups: &[&str], role: &str, owner: ObjectId, tags: &[&str]| Ask::AssumeRole {
            user: me,
            groups: groups.iter().map(|g| g.to_string()).collect(),
            role: role.into(),
            device: DeviceFacts::new(
                "linux",
                &tags.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
                owner,
            )
            .unwrap(),
        };
        assert!(
            decide(&set, &ask(&["member"], "dev", me, &[])).allow,
            "own device"
        );
        assert!(
            decide(&set, &ask(&["member"], "dev", ObjectId::new(), &["Build"])).allow,
            "a build box — the device's tag lowercased"
        );
        assert!(!decide(&set, &ask(&["member"], "dev", ObjectId::new(), &[])).allow);
        assert!(
            !decide(&set, &ask(&["guest"], "dev", me, &[])).allow,
            "not a member"
        );
        assert!(!decide(&set, &ask(&["member"], "deployer", me, &[])).allow);
    }

    #[test]
    fn an_approval_can_be_required() {
        let p = r#"permit (principal in Roomler::Role::"deployer",
            action == Roomler::Action::"secret:use", resource)
            when { context.ttl_s <= 900 && context has approval };"#;
        let set = set_with(&[p]);
        let mut ask = use_secret(Some("deployer"), "deploy/key", &[], "proxy", "linux");
        assert!(!decide(&set, &ask).allow, "no approval");
        if let Ask::UseSecret { approver, .. } = &mut ask {
            *approver = Some(ObjectId::new());
        }
        assert!(decide(&set, &ask).allow);
        if let Ask::UseSecret { ttl_s, .. } = &mut ask {
            *ttl_s = 3600;
        }
        assert!(!decide(&set, &ask).allow, "too long");
    }

    #[test]
    fn a_policy_is_checked_before_it_is_kept() {
        let n = names();
        assert_eq!(check_policy(DEV_STAGING, &n).unwrap().effect, "permit");
        let refused = [
            ("", "empty"),
            ("permit (principal, action, resource)", "not a Cedar policy"),
            (
                "permit (principal, action, resource); permit (principal, action, resource);",
                "not a Cedar policy",
            ),
            (
                r#"permit (principal, action == Roomler::Action::"secret:read", resource);"#,
                "schema",
            ),
            (
                r#"permit (principal, action == Roomler::Action::"secret:use", resource) when { context.nope == 1 };"#,
                "schema",
            ),
            (
                r#"permit (principal, action == Roomler::Action::"secret:use", resource in Roomler::SecretFolder::"rrn:other:secret:prod");"#,
                "not this organization",
            ),
            (
                r#"permit (principal, action == Roomler::Action::"secret:use", resource in Roomler::SecretFolder::"rrn:grox:secret:Prod");"#,
                "segment",
            ),
            (
                r#"permit (principal in Roomler::Role::"admin2", action, resource);"#,
                "vault roles",
            ),
            (
                r#"permit (principal in Roomler::Group::"ghosts", action, resource);"#,
                "roles",
            ),
            (
                r#"permit (principal == Roomler::Session::"abc", action, resource);"#,
                "one session",
            ),
            (
                r#"permit (principal == Roomler::User::"not-an-id", action, resource);"#,
                "user id",
            ),
            (
                r#"permit (principal, action == Roomler::Action::"secret:use", resource) when { resource in Roomler::SecretFolder::"rrn:other:secret:x" };"#,
                "not this organization",
            ),
        ];
        for (text, why) in refused {
            let e = check_policy(text, &n).expect_err(text);
            assert!(e.contains(why), "{text}\n→ {e}");
        }
        let u = ObjectId::new();
        let named = format!(
            r#"forbid (principal == Roomler::User::"{}", action, resource);"#,
            u.to_hex()
        );
        let c = check_policy(&named, &n).unwrap();
        assert_eq!(
            (c.effect, c.users.iter().copied().collect::<Vec<_>>()),
            ("forbid", vec![u])
        );
    }

    #[test]
    fn a_set_that_does_not_build_is_an_error_not_a_skip() {
        let r = policy_set([(
            "policy:1".to_string(),
            "forbid (principal, action, resource",
        )]);
        assert!(r.is_err());
    }

    #[test]
    fn a_device_context_takes_the_rows_spellings_only() {
        assert!(DeviceFacts::new("Windows", &[], ObjectId::new()).is_err());
        assert!(DeviceFacts::new("win32", &[], ObjectId::new()).is_err());
        let d = DeviceFacts::new("macos", &[" Build ".into()], ObjectId::new()).unwrap();
        assert_eq!(d.tags, ["build"]);
    }

    #[test]
    fn folders_are_every_prefix_innermost_first() {
        assert_eq!(folders("a/b/c"), ["a/b", "a"]);
        assert!(folders("a").is_empty());
    }

    /// P3-4b — listing is its own permission: a role allowed to use a secret
    /// lists nothing until a policy lets it list, and listing uses nothing.
    #[test]
    fn a_list_permit_is_its_own_and_neither_implies_the_other() {
        let uses = set_with(&[DEV_STAGING]);
        assert!(
            !decide(&uses, &list_secret(Some("dev"), "staging/db", "linux")).allow,
            "a use permit lists nothing"
        );
        let lists = set_with(&[DEV_LISTS_STAGING]);
        let ok = decide(
            &lists,
            &list_secret(Some("dev"), "staging/db/main", "linux"),
        );
        assert!(ok.allow, "{ok:?}");
        assert_eq!(ok.determining, ["policy:0"]);
        for ask in [
            list_secret(Some("dev"), "prod/db", "linux"),
            list_secret(Some("deployer"), "staging/db", "linux"),
            list_secret(None, "staging/db", "linux"),
            use_secret(Some("dev"), "staging/db", &[], "proxy", "linux"),
        ] {
            assert!(!decide(&lists, &ask).allow, "{ask:?}");
        }
    }

    /// P3-4b — the guardrails are about using a value: neither stops a
    /// listing, on Windows or of a production secret.
    #[test]
    fn no_guardrail_stops_a_listing() {
        let any = r#"permit (principal, action == Roomler::Action::"secret:list", resource);"#;
        let set = set_with(&[any]);
        for ask in [
            list_secret(Some("dev"), "staging/db", "windows"),
            list_secret(Some("dev"), "prod/db", "linux"),
        ] {
            let v = decide(&set, &ask);
            assert!(v.allow && v.errors.is_empty(), "{ask:?} {v:?}");
        }
    }

    #[test]
    fn a_recorded_request_reads_back_as_it_was_decided() {
        // P3-2b — a replay decides exactly what was decided: every fact the
        // request held survives the audit row's round trip.
        let assume = Ask::AssumeRole {
            user: ObjectId::new(),
            groups: vec!["member".into(), "ops".into()],
            role: "dev".into(),
            device: device("windows", &["Build"]),
        };
        let mut used = use_secret(Some("deployer"), "prod/db", &["prod"], "proxy", "macos");
        if let Ask::UseSecret {
            approver, session, ..
        } = &mut used
        {
            *approver = Some(ObjectId::new());
            session.playbook = Some("deploy-prod".into());
        }
        let listed = list_secret(Some("dev"), "staging/db", "windows");
        for ask in [
            assume,
            used,
            use_secret(None, "x", &[], "env", "linux"),
            listed,
        ] {
            let back = Ask::from_doc(&ask.to_doc()).expect("reads back");
            assert_eq!(format!("{back:?}"), format!("{ask:?}"));
            // And decides alike.
            let set = set_with(&[r#"permit (principal, action, resource);"#]);
            assert_eq!(decide(&set, &back), decide(&set, &ask));
        }
        // A row of another shape is unreadable, never a guess.
        assert!(Ask::from_doc(&bson::doc! { "action": "secret:read" }).is_none());
        assert!(Ask::from_doc(&bson::doc! { "action": "role:assume", "user": "x" }).is_none());
    }
}
