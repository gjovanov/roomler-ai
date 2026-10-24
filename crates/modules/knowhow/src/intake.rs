// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! P4-3a-2 — what a scan writes (decision 42): what lives on its device, and
//! nothing else (the FR-90 spec, §3e "P4-3, scans, as designed").
//!
//! - Its **checkouts** (`checkout`, keyed `<node id>/<16 hex of the path's
//!   SHA-256>`, since a path is not a key) and its **listeners** (`endpoint`,
//!   keyed `<node id>:<port>/<protocol>`): the device's own, made, changed
//!   and archived by its scans alone, and named so (`device`).
//! - Its machine's **declared tunnel routes**: a fact of its own machine,
//!   `tunnel.declared`.
//! - **Shared** entities, a repository (`repo`, keyed by its remote
//!   normalized) and a cluster (`cluster`, keyed by its server's
//!   `host:port`): made bare when absent and never changed once there. No
//!   device owns one, so no scan changes or archives it.
//! - **Links** from its own entities: `checkout checked_out_at machine`,
//!   `checkout part_of repo` (the remote's name as the link's attribute),
//!   `endpoint runs_on machine`, and `cluster reachable_via machine` with
//!   the device's contexts for it as the link's attributes.
//!
//! ⚠️ A scan is its device's claim about itself, believed about nothing
//! else. A complete scan archives what the device's earlier scans wrote and
//! this one no longer reports; a `no_roots` answer archives all of it; a
//! truncated one archives nothing, since what it left out is unknown, not
//! gone.

use std::collections::{HashMap, HashSet};

use bson::{Bson, Document, doc, oid::ObjectId};
use roomler_ai_remote_control::knowhow::{Checkout, Finding, KubeContext, Listener, TunnelRoute};
use roomler_ai_services::dao::base::{DaoError, DaoResult};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::KnowhowState;
use crate::dao::{MAX_EDGES, MAX_ENTITIES};
use crate::model::{
    KnowhowEdge, KnowhowEntity, OP_ARCHIVE, OP_CREATE, OP_UPDATE, SOURCE_SCAN, STATUS_ACTIVE,
    STATUS_ARCHIVED, ScanWritten, TARGET_EDGE, TARGET_ENTITY, check_key, check_name,
    merge_facts_from,
};
use crate::sync::MACHINE;
use crate::write::{Stamp, archive_entity, record};

pub const CHECKOUT: &str = "checkout";
pub const ENDPOINT: &str = "endpoint";
pub const REPO: &str = "repo";
pub const CLUSTER: &str = "cluster";

/// The facts a scan owns on its own entities, by path.
pub const SCANNED_FACTS: &[&str] = &[
    "address",
    "branch",
    "path",
    "port",
    "process",
    "protocol",
    "remotes",
    "root_commit",
    "services",
];

/// The facts a scan owns on its device's machine, by path.
pub const SCANNED_MACHINE_FACTS: &[&str] = &["tunnel.declared"];

// A machine keeps room for the sync's facts and its scans', and a scan's own
// entity for its scans' (`routes::persons_room`).
const _: () = assert!(
    crate::sync::SYNCED_FACTS.len() + SCANNED_MACHINE_FACTS.len() < crate::model::MAX_FACTS
);
const _: () = assert!(SCANNED_FACTS.len() < crate::model::MAX_FACTS);

/// The writes one entity or link tries before leaving it to the next scan.
const ATTEMPTS: usize = 3;

// ─── keys ───────────────────────────────────────────────────────────────────

/// A checkout's key: its device's node id, then the first 16 hex of its
/// path's SHA-256. A path is not a key (case, spaces, separators); it is the
/// checkout's `path` fact.
pub fn checkout_key(device: ObjectId, path: &str) -> String {
    let digest = Sha256::digest(path.as_bytes());
    let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{}/{short}", device.to_hex())
}

/// A listener's key: its device's node id, its port and protocol.
pub fn endpoint_key(device: ObjectId, l: &Listener) -> String {
    format!("{}:{}/{}", device.to_hex(), l.port, l.protocol)
}

/// P4-4-2 — whether `key` has the shape a device's scans key their own
/// entities by ([`checkout_key`], [`endpoint_key`]): a node id, then `/` or
/// `:`. A session never proposes one: parked there, a proposal would hold the
/// key against the device's next scan, which leaves a key another source
/// holds as it is.
pub fn devices_key(key: &str) -> bool {
    let b = key.as_bytes();
    b.len() > 24
        && b[..24]
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        && matches!(b[24], b'/' | b':')
}

/// P4-4-2 — whether the link `from` `kind` `to` is one a device's scans make
/// (this module's header): from a scan's own checkout `checked_out_at` or
/// `part_of`, from its own endpoint `runs_on`, or a cluster `reachable_via` a
/// machine. A scan leaves a link another source holds as it is, so a
/// session's proposal of one would hold it against the device.
pub fn scans_link(from: &KnowhowEntity, kind: &str, to: &KnowhowEntity) -> bool {
    let own = from.source == SOURCE_SCAN && from.device.is_some();
    own && matches!(kind, "checked_out_at" | "part_of" | "runs_on")
        || kind == "reachable_via" && from.kind == CLUSTER && to.kind == MACHINE
}

/// A repository's key from a remote: its host and path, with no scheme,
/// user, port or `.git`, in lowercase. `None` for a local path, and for one
/// the key rules refuse.
pub fn repo_key(url: &str) -> Option<String> {
    let s = url.trim();
    let (host, path) = if let Some((_, rest)) = s.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (host.split(':').next().unwrap_or(""), path)
    } else {
        // `[user@]host:path`, as git spells ssh. A drive letter (`C:\…`) or
        // a path with a `/` before its `:` is local.
        let (userhost, path) = s.split_once(':')?;
        if userhost.len() <= 1 || userhost.contains('/') || userhost.contains('\\') {
            return None;
        }
        (userhost.rsplit_once('@').map_or(userhost, |(_, h)| h), path)
    };
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    let key = format!("{host}/{path}").to_ascii_lowercase();
    check_key(&key).ok().map(str::to_string)
}

/// A cluster's key from a context's server: its `host:port`, the scheme's
/// port when it names none, in lowercase. `None` for one the key rules
/// refuse.
pub fn cluster_key(server: &str) -> Option<String> {
    let (scheme, rest) = server.trim().split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let key = match authority.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => format!("{host}:{port}"),
        _ if scheme.eq_ignore_ascii_case("http") => format!("{authority}:80"),
        _ => format!("{authority}:443"),
    };
    let key = key.to_ascii_lowercase();
    check_key(&key).ok().map(str::to_string)
}

/// A name the map keeps, else the key.
fn named(wanted: &str, key: &str) -> String {
    check_name(wanted).unwrap_or_else(|_| key.to_string())
}

/// A path's last segment, whichever separator it uses.
fn last_segment(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
}

// ─── the facts ──────────────────────────────────────────────────────────────

fn text(s: &str) -> Bson {
    Bson::String(s.to_string())
}

/// A checkout's facts, every path of [`SCANNED_FACTS`] a checkout has.
fn checkout_facts(c: &Checkout) -> Vec<(String, Option<Bson>)> {
    let remotes = c
        .remotes
        .iter()
        .map(|r| Bson::Document(doc! { "name": r.name.as_str(), "url": r.url.as_str() }))
        .collect();
    let services = c
        .services
        .iter()
        .map(|s| {
            let mut d = doc! { "name": s.name.as_str() };
            if let Some(image) = &s.image {
                d.insert("image", image.as_str());
            }
            if let Some(file) = &s.file {
                d.insert("file", file.as_str());
            }
            Bson::Document(d)
        })
        .collect();
    vec![
        ("branch".into(), c.branch.as_deref().map(text)),
        ("path".into(), Some(text(&c.path))),
        ("remotes".into(), Some(Bson::Array(remotes))),
        ("root_commit".into(), c.root_commit.as_deref().map(text)),
        ("services".into(), Some(Bson::Array(services))),
    ]
}

/// A listener's facts.
fn listener_facts(l: &Listener) -> Vec<(String, Option<Bson>)> {
    vec![
        ("address".into(), Some(text(&l.address))),
        ("port".into(), Some(Bson::Int32(i32::from(l.port)))),
        ("process".into(), l.process.as_deref().map(text)),
        ("protocol".into(), Some(text(&l.protocol))),
    ]
}

/// A context as its cluster's link keeps it.
fn context_doc(k: &KubeContext) -> Bson {
    let mut d = doc! { "name": k.name.as_str() };
    if let Some(ns) = &k.namespace {
        d.insert("namespace", ns.as_str());
    }
    if let Some(ca) = &k.ca_sha256 {
        d.insert("ca_sha256", ca.as_str());
    }
    Bson::Document(d)
}

/// A declared tunnel route as its machine's fact keeps it.
fn route_doc(t: &TunnelRoute) -> Bson {
    let mut d = doc! {
        "id": t.id.as_str(), "flow": t.flow.as_str(), "local": i32::from(t.local),
        "node": t.node.as_str(), "enabled": t.enabled,
    };
    if let Some(remote) = &t.remote {
        d.insert("remote", remote.as_str());
    }
    Bson::Document(d)
}

// ─── the writes ─────────────────────────────────────────────────────────────

/// Write a scan's findings as `device`'s part of the map: what it found, the
/// shared entities it reaches, its links, and, when `complete`, the archive
/// of what its earlier scans wrote and this one no longer reports.
pub async fn write(
    state: &KnowhowState,
    tid: ObjectId,
    device: ObjectId,
    findings: &[Finding],
    complete: bool,
) -> DaoResult<ScanWritten> {
    let stamp = Stamp::scan(tid, device);
    let mut w = ScanWritten::default();
    let Some(machine) = machine_of(state, tid, device).await? else {
        warn!(tenant = %tid, %device, "knowhow: a scan's device has no machine on the map; its findings wait for the next scan");
        return Ok(w);
    };
    let Some(machine_id) = machine.id else {
        return Ok(w);
    };
    let mut keep: HashSet<ObjectId> = HashSet::new();
    let mut keep_links: HashSet<ObjectId> = HashSet::new();
    let mut clusters: HashMap<String, (String, Vec<Bson>)> = HashMap::new();
    let mut routes: Vec<Bson> = Vec::new();
    for finding in findings {
        match finding {
            Finding::Checkout(c) => {
                let key = checkout_key(device, &c.path);
                let name = named(
                    &format!("{} on {}", last_segment(&c.path), machine.name),
                    &key,
                );
                let facts = checkout_facts(c);
                let Some(id) = own(
                    state, &stamp, CHECKOUT, &key, &name, &machine, &facts, &mut w,
                )
                .await?
                else {
                    continue;
                };
                keep.insert(id);
                w.checkouts += 1;
                let at = link(
                    state,
                    &stamp,
                    id,
                    "checked_out_at",
                    machine_id,
                    doc! {},
                    &mut w,
                );
                keep_links.extend(at.await?);
                let mut repos = HashSet::new();
                for r in &c.remotes {
                    let Some(rk) = repo_key(&r.url) else { continue };
                    if !repos.insert(rk.clone()) {
                        continue;
                    }
                    let rname = named(rk.split_once('/').map_or(rk.as_str(), |(_, p)| p), &rk);
                    let Some(repo) = shared(state, &stamp, REPO, &rk, &rname, &mut w).await? else {
                        continue;
                    };
                    let attrs = doc! { "remote": r.name.as_str() };
                    keep_links
                        .extend(link(state, &stamp, id, "part_of", repo, attrs, &mut w).await?);
                }
            }
            Finding::Listener(l) => {
                let key = endpoint_key(device, l);
                let process = l.process.as_deref().unwrap_or("port");
                let name = named(&format!("{process}:{} on {}", l.port, machine.name), &key);
                let facts = listener_facts(l);
                let Some(id) = own(
                    state, &stamp, ENDPOINT, &key, &name, &machine, &facts, &mut w,
                )
                .await?
                else {
                    continue;
                };
                keep.insert(id);
                w.endpoints += 1;
                let on = link(state, &stamp, id, "runs_on", machine_id, doc! {}, &mut w);
                keep_links.extend(on.await?);
            }
            Finding::Context(k) => {
                if let Some(key) = cluster_key(&k.server) {
                    let entry = clusters
                        .entry(key)
                        .or_insert_with(|| (k.name.clone(), Vec::new()));
                    entry.1.push(context_doc(k));
                }
            }
            Finding::TunnelRoute(t) => routes.push(route_doc(t)),
        }
    }
    for (key, (name, contexts)) in clusters {
        let name = named(&name, &key);
        let Some(cluster) = shared(state, &stamp, CLUSTER, &key, &name, &mut w).await? else {
            continue;
        };
        let attrs = doc! { "contexts": contexts };
        keep_links.extend(
            link(
                state,
                &stamp,
                cluster,
                "reachable_via",
                machine_id,
                attrs,
                &mut w,
            )
            .await?,
        );
    }
    // Its machine's declared tunnel routes: none in a complete scan is none.
    let declared = (!routes.is_empty()).then_some(Bson::Array(routes));
    if complete || declared.is_some() {
        machine_fact(state, &stamp, device, "tunnel.declared", declared).await?;
    }
    if complete {
        w.retired = retire(state, &stamp, device, &keep, &keep_links).await?;
    }
    Ok(w)
}

/// The device's machine, mirrored first when the sync has not reached it.
async fn machine_of(
    state: &KnowhowState,
    tid: ObjectId,
    device: ObjectId,
) -> DaoResult<Option<KnowhowEntity>> {
    let key = device.to_hex();
    if let Some(m) = state.entities.find_live_by_key(tid, MACHINE, &key).await? {
        return Ok(Some(m));
    }
    crate::sync::device(state, tid, device).await?;
    state.entities.find_live_by_key(tid, MACHINE, &key).await
}

/// Make or bring up to date one of the device's own entities, owned by its
/// owner. `None` when the map is full, or when the key is held by an entity
/// no scan of this device made (a person's), which a scan does not take.
#[allow(clippy::too_many_arguments)]
async fn own(
    state: &KnowhowState,
    stamp: &Stamp,
    kind: &str,
    key: &str,
    name: &str,
    machine: &KnowhowEntity,
    facts: &[(String, Option<Bson>)],
    w: &mut ScanWritten,
) -> DaoResult<Option<ObjectId>> {
    let tid = stamp.tid;
    for _ in 0..ATTEMPTS {
        match state.entities.find_live_by_key(tid, kind, key).await? {
            None => {
                if state.entities.count_live(tid).await? >= MAX_ENTITIES {
                    w.full = true;
                    return Ok(None);
                }
                let (facts, said) = merge_facts_from(&[], facts, stamp.provenance(), stamp.at);
                let entity = KnowhowEntity {
                    id: None,
                    tenant_id: tid,
                    kind: kind.into(),
                    key: key.into(),
                    name: name.into(),
                    env: None,
                    owner_id: machine.owner_id,
                    attrs: Document::new(),
                    facts,
                    status: STATUS_ACTIVE.into(),
                    source: SOURCE_SCAN.into(),
                    device: stamp.device,
                    session: None,
                    confirmed: None,
                    rev: 1,
                    created_by: None,
                    created_at: stamp.at,
                    updated_by: None,
                    updated_at: stamp.at,
                    deleted_at: None,
                };
                match state.entities.create(&entity).await {
                    Ok(id) => {
                        let set = doc! {
                            "kind": kind, "key": key, "name": name,
                            "owner_id": machine.owner_id, "facts": said,
                        };
                        record(
                            state,
                            vec![stamp.row(TARGET_ENTITY, id, vec![id], 1, OP_CREATE, set)],
                        )
                        .await;
                        return Ok(Some(id));
                    }
                    Err(DaoError::DuplicateKey(_)) => continue,
                    Err(e) => return Err(e),
                }
            }
            Some(current) => {
                let Some(id) = current.id else {
                    return Ok(None);
                };
                if current.source != SOURCE_SCAN || current.device != stamp.device {
                    debug!(%key, "knowhow: a scan found a key another source holds; it is left as it is");
                    return Ok(None);
                }
                let mut set = Document::new();
                let mut said = Document::new();
                if current.name != name {
                    set.insert("name", name);
                    said.insert("name", name);
                }
                if current.owner_id != machine.owner_id {
                    set.insert("owner_id", machine.owner_id);
                    said.insert("owner_id", machine.owner_id);
                }
                let (facts, changed) =
                    merge_facts_from(&current.facts, facts, stamp.provenance(), stamp.at);
                if !changed.is_empty() {
                    set.insert("facts", bson::to_bson(&facts)?);
                    said.insert("facts", changed);
                }
                if set.is_empty() {
                    return Ok(Some(id));
                }
                set.extend(stamp.touched());
                match state.entities.update_at(tid, id, current.rev, set).await? {
                    Some(after) => {
                        let row =
                            stamp.row(TARGET_ENTITY, id, vec![id], after.rev, OP_UPDATE, said);
                        record(state, vec![row]).await;
                        return Ok(Some(id));
                    }
                    None => continue,
                }
            }
        }
    }
    Ok(None)
}

/// A shared entity: made bare when absent, never changed once there. No
/// device owns it (`device` none), and its history names the device whose
/// scan made it. `None` when the map is full.
async fn shared(
    state: &KnowhowState,
    stamp: &Stamp,
    kind: &str,
    key: &str,
    name: &str,
    w: &mut ScanWritten,
) -> DaoResult<Option<ObjectId>> {
    let tid = stamp.tid;
    for _ in 0..ATTEMPTS {
        if let Some(e) = state.entities.find_live_by_key(tid, kind, key).await? {
            return Ok(e.id);
        }
        if state.entities.count_live(tid).await? >= MAX_ENTITIES {
            w.full = true;
            return Ok(None);
        }
        let entity = KnowhowEntity {
            id: None,
            tenant_id: tid,
            kind: kind.into(),
            key: key.into(),
            name: name.into(),
            env: None,
            owner_id: None,
            attrs: Document::new(),
            facts: Vec::new(),
            status: STATUS_ACTIVE.into(),
            source: SOURCE_SCAN.into(),
            device: None,
            session: None,
            confirmed: None,
            rev: 1,
            created_by: None,
            created_at: stamp.at,
            updated_by: None,
            updated_at: stamp.at,
            deleted_at: None,
        };
        match state.entities.create(&entity).await {
            Ok(id) => {
                let set = doc! { "kind": kind, "key": key, "name": name };
                record(
                    state,
                    vec![stamp.row(TARGET_ENTITY, id, vec![id], 1, OP_CREATE, set)],
                )
                .await;
                if kind == REPO {
                    w.repos += 1;
                } else {
                    w.clusters += 1;
                }
                return Ok(Some(id));
            }
            Err(DaoError::DuplicateKey(_)) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// A link of the device's: made when absent, its attributes brought up to
/// date when it is the device's own. `Some` for a link this device's scans
/// own; `None` when another source's link already says it (left as it is),
/// or the map is full.
async fn link(
    state: &KnowhowState,
    stamp: &Stamp,
    from: ObjectId,
    kind: &str,
    to: ObjectId,
    attrs: Document,
    w: &mut ScanWritten,
) -> DaoResult<Option<ObjectId>> {
    let tid = stamp.tid;
    for _ in 0..ATTEMPTS {
        match state.edges.find_live(tid, from, kind, to).await? {
            None => {
                if state.edges.count_live(tid).await? >= MAX_EDGES {
                    w.full = true;
                    return Ok(None);
                }
                let edge = KnowhowEdge {
                    id: None,
                    tenant_id: tid,
                    from,
                    to,
                    kind: kind.into(),
                    attrs: attrs.clone(),
                    status: STATUS_ACTIVE.into(),
                    source: SOURCE_SCAN.into(),
                    device: stamp.device,
                    session: None,
                    confirmed: None,
                    rev: 1,
                    created_by: None,
                    created_at: stamp.at,
                    updated_by: None,
                    updated_at: stamp.at,
                    deleted_at: None,
                };
                match state.edges.create(&edge).await {
                    Ok(id) => {
                        let set = doc! { "from": from, "kind": kind, "to": to, "attrs": attrs };
                        let row = stamp.row(TARGET_EDGE, id, vec![from, to], 1, OP_CREATE, set);
                        record(state, vec![row]).await;
                        w.links += 1;
                        return Ok(Some(id));
                    }
                    Err(DaoError::DuplicateKey(_)) => continue,
                    Err(e) => return Err(e),
                }
            }
            Some(current) => {
                let Some(id) = current.id else {
                    return Ok(None);
                };
                if current.source != SOURCE_SCAN || current.device != stamp.device {
                    return Ok(None);
                }
                if current.attrs == attrs {
                    return Ok(Some(id));
                }
                let mut set = doc! { "attrs": attrs.clone() };
                set.extend(stamp.touched());
                match state.edges.update_at(tid, id, current.rev, set).await? {
                    Some(after) => {
                        let row = stamp.row(
                            TARGET_EDGE,
                            id,
                            vec![from, to],
                            after.rev,
                            OP_UPDATE,
                            doc! { "attrs": attrs },
                        );
                        record(state, vec![row]).await;
                        return Ok(Some(id));
                    }
                    None => continue,
                }
            }
        }
    }
    Ok(None)
}

/// A fact of the device's own machine, the scan's: set, or removed by none.
async fn machine_fact(
    state: &KnowhowState,
    stamp: &Stamp,
    device: ObjectId,
    path: &str,
    value: Option<Bson>,
) -> DaoResult<()> {
    let key = device.to_hex();
    for _ in 0..ATTEMPTS {
        let Some(m) = state
            .entities
            .find_live_by_key(stamp.tid, MACHINE, &key)
            .await?
        else {
            return Ok(());
        };
        let Some(id) = m.id else { return Ok(()) };
        let change = [(path.to_string(), value.clone())];
        let (facts, changed) = merge_facts_from(&m.facts, &change, stamp.provenance(), stamp.at);
        if changed.is_empty() {
            return Ok(());
        }
        let mut set = doc! { "facts": bson::to_bson(&facts)? };
        set.extend(stamp.touched());
        if let Some(after) = state.entities.update_at(stamp.tid, id, m.rev, set).await? {
            let said = doc! { "facts": changed };
            record(
                state,
                vec![stamp.row(TARGET_ENTITY, id, vec![id], after.rev, OP_UPDATE, said)],
            )
            .await;
            return Ok(());
        }
    }
    Ok(())
}

/// What the device's earlier scans wrote and this one no longer reports:
/// its own entities, archived with their links, then its own links. Never a
/// shared entity: no device owns one.
async fn retire(
    state: &KnowhowState,
    stamp: &Stamp,
    device: ObjectId,
    keep: &HashSet<ObjectId>,
    keep_links: &HashSet<ObjectId>,
) -> DaoResult<u32> {
    let mut retired = 0;
    for e in state.entities.live_of_device(stamp.tid, device).await? {
        let Some(id) = e.id else { continue };
        if keep.contains(&id) {
            continue;
        }
        if archive_entity(state, stamp, id, e.rev).await?.is_some() {
            retired += 1;
        }
    }
    for e in state.edges.live_of_device(stamp.tid, device).await? {
        let Some(id) = e.id else { continue };
        if keep_links.contains(&id) {
            continue;
        }
        if let Some(after) = state
            .edges
            .archive_live(stamp.tid, id, stamp.by, stamp.at)
            .await?
        {
            retired += 1;
            let set = doc! { "status": STATUS_ARCHIVED };
            let row = stamp.row(
                TARGET_EDGE,
                id,
                vec![after.from, after.to],
                after.rev,
                OP_ARCHIVE,
                set,
            );
            record(state, vec![row]).await;
        }
    }
    Ok(retired)
}

/// A removed device's scan part: its own entities archived with their links
/// (P4-2a's removal archives its machine, and the machine's links go with it).
pub async fn removed(state: &KnowhowState, tid: ObjectId, device: ObjectId) -> DaoResult<u32> {
    let stamp = Stamp::sync(tid);
    let mut archived = 0;
    for e in state.entities.live_of_device(tid, device).await? {
        let Some(id) = e.id else { continue };
        if archive_entity(state, &stamp, id, e.rev).await?.is_some() {
            archived += 1;
        }
    }
    Ok(archived)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_is_keyed_by_its_host_and_path() {
        for (url, key) in [
            ("git@github.com:acme/api.git", "github.com/acme/api"),
            (
                "ssh://git@github.com:22/acme/api.git",
                "github.com/acme/api",
            ),
            ("https://github.com/Acme/API.git", "github.com/acme/api"),
            (
                "https://dev@gitlab.example.com/group/sub/api",
                "gitlab.example.com/group/sub/api",
            ),
            ("git://example.org/api/", "example.org/api"),
        ] {
            assert_eq!(repo_key(url).as_deref(), Some(key), "{url}");
        }
        for local in [
            "/srv/git/api.git",
            "../api",
            "C:\\src\\api",
            "file:///srv/git/api.git",
            "https://example.org/",
            "https://exa mple.org/api",
        ] {
            assert_eq!(repo_key(local), None, "{local}");
        }
    }

    #[test]
    fn a_cluster_is_keyed_by_its_servers_host_and_port() {
        for (server, key) in [
            ("https://k8s.example.com:6443", "k8s.example.com:6443"),
            ("https://K8S.example.com", "k8s.example.com:443"),
            ("http://127.0.0.1:8080/api", "127.0.0.1:8080"),
            ("http://kube.local", "kube.local:80"),
        ] {
            assert_eq!(cluster_key(server).as_deref(), Some(key), "{server}");
        }
        assert_eq!(cluster_key("k8s.example.com:6443"), None, "no scheme");
        assert_eq!(
            cluster_key("https://[::1]:6443"),
            None,
            "a key cannot hold brackets"
        );
    }

    #[test]
    fn a_checkout_is_keyed_by_its_device_and_its_paths_hash() {
        let device = ObjectId::new();
        let a = checkout_key(device, "/home/dev/src/api");
        let b = checkout_key(device, "/home/dev/src/API");
        assert_ne!(a, b, "a path's case is its own");
        assert_eq!(a, checkout_key(device, "/home/dev/src/api"), "stable");
        let (node, short) = a.split_once('/').unwrap();
        assert_eq!(node, device.to_hex());
        assert_eq!(short.len(), 16);
        assert!(check_key(&a).is_ok(), "{a}");
        let l = Listener {
            port: 5432,
            protocol: "tcp".into(),
            address: "0.0.0.0".into(),
            process: None,
        };
        let e = endpoint_key(device, &l);
        assert_eq!(e, format!("{}:5432/tcp", device.to_hex()));
        assert!(check_key(&e).is_ok(), "{e}");

        // P4-4-2 — and either is told from a key a person or a session
        // writes, so a session never proposes one.
        assert!(devices_key(&a) && devices_key(&e));
        let node = device.to_hex();
        let replica = format!("{node}-replica");
        let short = format!("{}/api", &node[..23]);
        for other in [
            "github.com/acme/api",
            "prod-mongo",
            "db-1:5432",
            node.as_str(),
            replica.as_str(),
            short.as_str(),
        ] {
            assert!(!devices_key(other), "{other}");
        }
    }

    fn entity(kind: &str, source: &str, device: Option<ObjectId>) -> KnowhowEntity {
        let now = bson::DateTime::from_millis(1);
        KnowhowEntity {
            id: Some(ObjectId::new()),
            tenant_id: ObjectId::new(),
            kind: kind.into(),
            key: "k".into(),
            name: "n".into(),
            env: None,
            owner_id: None,
            attrs: Document::new(),
            facts: Vec::new(),
            status: STATUS_ACTIVE.into(),
            source: source.into(),
            device,
            session: None,
            confirmed: None,
            rev: 1,
            created_by: None,
            created_at: now,
            updated_by: None,
            updated_at: now,
            deleted_at: None,
        }
    }

    /// P4-4-2 — the links a scan makes, each told from one a person or a
    /// session may make between the same kinds.
    #[test]
    fn the_links_a_scan_makes_are_told_apart() {
        let device = Some(ObjectId::new());
        let checkout = entity(CHECKOUT, SOURCE_SCAN, device);
        let endpoint = entity(ENDPOINT, SOURCE_SCAN, device);
        let machine = entity(MACHINE, crate::model::SOURCE_SYNC, None);
        let repo = entity(REPO, SOURCE_SCAN, None);
        let cluster = entity(CLUSTER, SOURCE_SCAN, None);
        let service = entity("service", crate::model::SOURCE_SESSION, None);
        let persons_checkout = entity(CHECKOUT, crate::model::SOURCE_PERSON, None);
        for (from, kind, to) in [
            (&checkout, "checked_out_at", &machine),
            (&checkout, "part_of", &repo),
            (&endpoint, "runs_on", &machine),
            (&cluster, "reachable_via", &machine),
        ] {
            assert!(
                scans_link(from, kind, to),
                "{} {kind} {}",
                from.kind,
                to.kind
            );
        }
        for (from, kind, to) in [
            (&service, "runs_on", &machine),
            (&persons_checkout, "checked_out_at", &machine),
            (&checkout, "deployed_to", &cluster),
            (&service, "depends_on", &endpoint),
            (&cluster, "reachable_via", &endpoint),
        ] {
            assert!(
                !scans_link(from, kind, to),
                "{} {kind} {}",
                from.kind,
                to.kind
            );
        }
    }

    #[test]
    fn the_scanned_paths_are_fact_paths_each_once() {
        for p in SCANNED_FACTS.iter().chain(SCANNED_MACHINE_FACTS) {
            assert!(crate::model::check_fact_path(p).is_ok(), "{p}");
        }
        assert!(
            SCANNED_FACTS.windows(2).all(|w| w[0] < w[1]),
            "sorted, each once"
        );
        for p in SCANNED_MACHINE_FACTS {
            assert!(
                !crate::sync::SYNCED_FACTS.contains(p),
                "{p} is the scan's, not the sync's"
            );
        }
    }
}
