// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! P4-2a — the fleet's devices, mirrored onto the map (design §9.2: the first
//! source, and the most trusted).
//!
//! Each live device of an organization knowhow serves is a `machine`,
//! keyed by its id. That is the node id a re-enrolment of a live machine
//! keeps and a removal retires, and it never moves when a label does. The
//! machine's name is the device's label (its display name, else its own
//! name), and its owner is the device's. What the fleet says of it is the
//! sync's: [`SYNCED_FACTS`], each fact sourced `sync`, with no person
//! behind it. A person may describe a machine further (an environment,
//! attributes, facts of their own), never what the fleet says of it
//! (`routes::the_fleets`).
//!
//! It hears of a device from core, never from a call by fleet:
//! - at a hello: an enrolment's first, and every reconnect;
//! - at a rename, and at a change of tags or owner;
//! - at a removal, which archives the machine and its links;
//! - at boot, and every `knowhow.sync_secs`, a reconcile of every served
//!   organization. It catches what no hook carries (a display name), and
//!   retires a machine whose device went while knowhow was not looking.
//!
//! ⚠️ A sync that fails is logged, never the caller's failure. A map is not a
//! gate: no rename, no update and above all no removal may fail because the
//! map could not be written. The next sync tries again.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bson::{Bson, Document, doc, oid::ObjectId};
use roomler_ai_remote_control::models::Agent;
use roomler_ai_services::dao::base::{DaoError, DaoResult};
use roomler_core::{
    AgentCtx, AgentSocketLifecycle, FleetLifecycle, ReleasedLease, RenamePropagation,
};
use tracing::{debug, info, warn};

use crate::KnowhowState;
use crate::dao::MAX_ENTITIES;
use crate::model::{
    KnowhowEntity, OP_CREATE, OP_UPDATE, SOURCE_SYNC, STATUS_ACTIVE, TARGET_ENTITY, check_name,
    merge_facts,
};
use crate::write::{Stamp, archive_entity, record};

/// The kind a device is on the map.
pub const MACHINE: &str = "machine";

/// What the fleet says of a machine: the facts the sync owns, by path.
pub const SYNCED_FACTS: &[&str] = &["arch", "ephemeral", "machine_name", "os", "tags", "version"];

/// How often every pod reconciles its served organizations' machines when
/// `knowhow.sync_secs` is 0 or absent.
pub const DEFAULT_SYNC_SECS: u64 = 600;

/// The writes one machine's sync tries before leaving it to the next.
const ATTEMPTS: usize = 3;

/// What a device is on the map: a name, an owner, and the facts the fleet
/// says.
#[derive(Debug, Clone, PartialEq)]
pub struct Mirror {
    pub name: String,
    pub owner_id: ObjectId,
    /// Every path of [`SYNCED_FACTS`]: a value, or `None` for one the device
    /// no longer reports (removed from the machine).
    pub facts: Vec<(String, Option<Bson>)>,
}

/// What the sync reads of a device's row.
#[derive(Debug, Clone, Copy)]
pub struct Device<'a> {
    pub id: ObjectId,
    pub name: &'a str,
    pub display_name: Option<&'a str>,
    pub owner_id: ObjectId,
    /// The OS as the row spells it (`linux`, `macos`, `windows`).
    pub os: Option<&'a str>,
    pub arch: Option<&'a str>,
    pub version: &'a str,
    pub ephemeral: bool,
    pub tags: &'a [String],
}

impl<'a> Device<'a> {
    /// The row's view. `None` for a row read without its id.
    pub fn of(agent: &'a Agent, os: Option<&'a str>) -> Option<Self> {
        Some(Self {
            id: agent.id?,
            name: &agent.name,
            display_name: agent.display_name.as_deref(),
            owner_id: agent.owner_user_id,
            os,
            arch: agent.arch.as_deref(),
            version: &agent.agent_version,
            ephemeral: agent.ephemeral,
            tags: &agent.tags,
        })
    }
}

/// A device's OS as its row spells it.
fn os_of(agent: &Agent) -> Option<String> {
    match bson::to_bson(&agent.os) {
        Ok(Bson::String(s)) => Some(s),
        _ => None,
    }
}

/// A device, as the map shows it.
pub fn mirror(d: Device<'_>) -> Mirror {
    let label = d
        .display_name
        .filter(|l| !l.trim().is_empty())
        .unwrap_or(d.name);
    // A label is a person's text: one the map cannot keep falls back to the
    // machine's own name, then to its id, never to nothing.
    let name = check_name(label)
        .or_else(|_| check_name(d.name))
        .unwrap_or_else(|_| d.id.to_hex());
    let text = |s: &str| Bson::String(s.to_string());
    let tags = Bson::Array(d.tags.iter().map(|t| text(t)).collect());
    let facts = vec![
        (
            "arch".to_string(),
            d.arch.filter(|a| !a.is_empty()).map(text),
        ),
        ("ephemeral".to_string(), Some(Bson::Boolean(d.ephemeral))),
        ("machine_name".to_string(), Some(text(d.name))),
        ("os".to_string(), d.os.map(text)),
        ("tags".to_string(), Some(tags)),
        (
            "version".to_string(),
            Some(d.version).filter(|v| !v.is_empty()).map(text),
        ),
    ];
    Mirror {
        name,
        owner_id: d.owner_id,
        facts,
    }
}

/// A row's mirror, or `None` for one read without its id.
fn mirror_of(agent: &Agent) -> Option<(ObjectId, Mirror)> {
    let os = os_of(agent);
    let d = Device::of(agent, os.as_deref())?;
    Some((d.id, mirror(d)))
}

/// What one machine's sync did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Synced {
    Created,
    Updated,
    Unchanged,
    /// The organization is not one knowhow serves.
    NotServed,
    /// The device is not live: nothing to mirror.
    Gone,
    /// The map is full; the machine waits for room.
    Full,
    /// Every attempt met another writer; the next sync tries again.
    Raced,
}

/// Mirror one device of `tenant_id`: its machine made, or brought up to date.
pub async fn device(
    state: &KnowhowState,
    tenant_id: ObjectId,
    agent_id: ObjectId,
) -> DaoResult<Synced> {
    if !state.scope.serves(tenant_id) {
        return Ok(Synced::NotServed);
    }
    let agent = match state.agents.find_live_in_tenant(tenant_id, agent_id).await {
        Ok(agent) => agent,
        Err(DaoError::NotFound) => return Ok(Synced::Gone),
        Err(e) => return Err(e),
    };
    match mirror_of(&agent) {
        Some((id, m)) => put(state, tenant_id, id, &m).await,
        None => Ok(Synced::Gone),
    }
}

/// Write one machine from its mirror. A compare-and-set like a person's
/// write: a person's change to the machine's own fields between the read and
/// the write is never lost, only retried around.
async fn put(
    state: &KnowhowState,
    tid: ObjectId,
    agent_id: ObjectId,
    m: &Mirror,
) -> DaoResult<Synced> {
    let key = agent_id.to_hex();
    for _ in 0..ATTEMPTS {
        let stamp = Stamp::sync(tid);
        match state.entities.find_live_by_key(tid, MACHINE, &key).await? {
            None => {
                if state.entities.count_live(tid).await? >= MAX_ENTITIES {
                    warn!(tenant = %tid, device = %agent_id, "knowhow: the map is full; a device has no machine yet");
                    return Ok(Synced::Full);
                }
                let (facts, said) = merge_facts(&[], &m.facts, SOURCE_SYNC, None, stamp.at);
                let entity = KnowhowEntity {
                    id: None,
                    tenant_id: tid,
                    kind: MACHINE.into(),
                    key: key.clone(),
                    name: m.name.clone(),
                    env: None,
                    owner_id: Some(m.owner_id),
                    attrs: Document::new(),
                    facts,
                    status: STATUS_ACTIVE.into(),
                    source: SOURCE_SYNC.into(),
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
                            "kind": MACHINE, "key": key.as_str(), "name": m.name.as_str(),
                            "owner_id": m.owner_id, "facts": said,
                        };
                        let row = stamp.row(TARGET_ENTITY, id, vec![id], 1, OP_CREATE, set);
                        record(state, vec![row]).await;
                        return Ok(Synced::Created);
                    }
                    // Another pod's sync made it first: read it again.
                    Err(DaoError::DuplicateKey(_)) => continue,
                    Err(e) => return Err(e),
                }
            }
            Some(current) => {
                let Some(id) = current.id else {
                    return Ok(Synced::Raced);
                };
                let mut set = Document::new();
                let mut said = Document::new();
                if current.name != m.name {
                    set.insert("name", m.name.as_str());
                    said.insert("name", m.name.as_str());
                }
                if current.owner_id != Some(m.owner_id) {
                    set.insert("owner_id", m.owner_id);
                    said.insert("owner_id", m.owner_id);
                }
                let (facts, changed) =
                    merge_facts(&current.facts, &m.facts, SOURCE_SYNC, None, stamp.at);
                if !changed.is_empty() {
                    let facts = bson::to_bson(&facts)?;
                    set.insert("facts", facts);
                    said.insert("facts", changed);
                }
                if set.is_empty() {
                    return Ok(Synced::Unchanged);
                }
                set.extend(stamp.touched());
                match state.entities.update_at(tid, id, current.rev, set).await? {
                    Some(after) => {
                        let row =
                            stamp.row(TARGET_ENTITY, id, vec![id], after.rev, OP_UPDATE, said);
                        record(state, vec![row]).await;
                        return Ok(Synced::Updated);
                    }
                    None => continue,
                }
            }
        }
    }
    Ok(Synced::Raced)
}

/// A removed device: its machine archived, with its links. `false` when the
/// map held none for it.
pub async fn removed(
    state: &KnowhowState,
    tenant_id: ObjectId,
    agent_id: ObjectId,
) -> DaoResult<bool> {
    if !state.scope.serves(tenant_id) {
        return Ok(false);
    }
    retire(state, tenant_id, &agent_id.to_hex()).await
}

/// Archive the machine keyed `key`, at whatever rev it holds now.
async fn retire(state: &KnowhowState, tid: ObjectId, key: &str) -> DaoResult<bool> {
    for _ in 0..ATTEMPTS {
        let Some(current) = state.entities.find_live_by_key(tid, MACHINE, key).await? else {
            return Ok(false);
        };
        let Some(id) = current.id else {
            return Ok(false);
        };
        if archive_entity(state, &Stamp::sync(tid), id, current.rev)
            .await?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What a reconcile did, across the organizations it covered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub organizations: usize,
    pub created: usize,
    pub updated: usize,
    pub retired: usize,
    pub failed: usize,
}

/// Every served organization's machines brought into line with its live
/// devices: made, updated, and retired where the device is gone. The
/// organizations are those with a live device, and those whose map still
/// holds a machine (every device of one gone).
pub async fn all(state: &KnowhowState) -> DaoResult<Reconciled> {
    let mut tenants: HashSet<ObjectId> = state
        .agents
        .base
        .collection()
        .distinct("tenant_id", doc! { "deleted_at": null })
        .await?
        .into_iter()
        .filter_map(|b| b.as_object_id())
        .collect();
    tenants.extend(state.entities.tenants_with(MACHINE).await?);
    let mut out = Reconciled::default();
    for tid in tenants.into_iter().filter(|t| state.scope.serves(*t)) {
        out.organizations += 1;
        organization(state, tid, &mut out).await?;
    }
    Ok(out)
}

/// One organization's reconcile.
async fn organization(state: &KnowhowState, tid: ObjectId, out: &mut Reconciled) -> DaoResult<()> {
    let devices = state.agents.list_all_active_for_tenant(tid).await?;
    let mut live = HashSet::new();
    for agent in &devices {
        let Some((aid, m)) = mirror_of(agent) else {
            continue;
        };
        live.insert(aid.to_hex());
        match put(state, tid, aid, &m).await {
            Ok(Synced::Created) => out.created += 1,
            Ok(Synced::Updated) => out.updated += 1,
            Ok(_) => {}
            Err(e) => {
                out.failed += 1;
                warn!(tenant = %tid, device = %aid, %e, "knowhow: a device's machine was not synced");
            }
        }
    }
    for machine in state.entities.live_of_kind(tid, MACHINE).await? {
        if !live.contains(&machine.key) {
            match retire(state, tid, &machine.key).await {
                Ok(true) => out.retired += 1,
                Ok(false) => {}
                Err(e) => {
                    out.failed += 1;
                    warn!(tenant = %tid, key = %machine.key, %e, "knowhow: a gone device's machine was not retired");
                }
            }
        }
    }
    Ok(())
}

/// The reconcile, every `secs` seconds on this pod, until the process ends.
/// Every pod runs it: each write is a compare-and-set that changes nothing
/// when nothing changed, so two pods at once cost reads, never a wrong map.
pub fn spawn_reconciler(state: KnowhowState, secs: u64) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(secs.max(1)));
        // The boot job runs the first; the interval's own first tick is now.
        tick.tick().await;
        loop {
            tick.tick().await;
            match all(&state).await {
                Ok(r) => debug!(?r, "knowhow: machines reconciled"),
                Err(e) => warn!(%e, "knowhow: the reconcile failed; the next one tries again"),
            }
        }
    });
}

/// The boot job's body: one reconcile, said in the log.
pub async fn at_boot(state: &KnowhowState) -> anyhow::Result<()> {
    let r = all(state).await?;
    info!(
        organizations = r.organizations,
        created = r.created,
        updated = r.updated,
        retired = r.retired,
        failed = r.failed,
        "knowhow: machines mirrored from the fleet"
    );
    Ok(())
}

/// A device's hello: its machine mirrored, off the socket's path.
pub(crate) struct HelloSync {
    pub state: KnowhowState,
}

#[async_trait]
impl AgentSocketLifecycle for HelloSync {
    async fn hello(&self, ctx: &AgentCtx) {
        let (state, tid, aid) = (self.state.clone(), ctx.tenant_id, ctx.agent_id);
        if !state.scope.serves(tid) {
            return;
        }
        tokio::spawn(async move {
            if let Err(e) = device(&state, tid, aid).await {
                warn!(tenant = %tid, device = %aid, %e, "knowhow: a device's hello did not reach its machine");
            }
        });
    }
}

/// Fleet's events: a rename, an update, a removal. ⚠️ Each answers `Ok`
/// whatever the map met (the module's own doc says why).
pub(crate) struct FleetSync {
    pub state: KnowhowState,
}

#[async_trait]
impl FleetLifecycle for FleetSync {
    async fn agent_removed(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        _machine_id: &str,
        _reason: &str,
    ) -> anyhow::Result<Option<ReleasedLease>> {
        if let Err(e) = removed(&self.state, tenant_id, agent_id).await {
            warn!(device = %agent_id, %e, "knowhow: a removed device's machine stays until the next reconcile");
        }
        Ok(None)
    }

    /// ⚠️ `NoLiveNode` always: the map holds no label a peer reads, and the
    /// registry keeps the first answer that is not this one (network's).
    async fn agent_renamed(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        _name: &str,
    ) -> anyhow::Result<RenamePropagation> {
        if let Err(e) = device(&self.state, tenant_id, agent_id).await {
            warn!(device = %agent_id, %e, "knowhow: a renamed device's machine waits for the next reconcile");
        }
        Ok(RenamePropagation::NoLiveNode)
    }

    async fn agent_updated(&self, tenant_id: ObjectId, agent_id: ObjectId) -> anyhow::Result<()> {
        if let Err(e) = device(&self.state, tenant_id, agent_id).await {
            warn!(device = %agent_id, %e, "knowhow: an updated device's machine waits for the next reconcile");
        }
        Ok(())
    }
}

/// The hooks a [`KnowhowState`] registers.
pub(crate) fn hello(state: &KnowhowState) -> Arc<HelloSync> {
    Arc::new(HelloSync {
        state: state.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device<'a>(tags: &'a [String], display: Option<&'a str>) -> Device<'a> {
        Device {
            id: ObjectId::new(),
            name: "build-01",
            display_name: display,
            owner_id: ObjectId::new(),
            os: Some("linux"),
            arch: None,
            version: "0.4.154",
            ephemeral: false,
            tags,
        }
    }

    #[test]
    fn a_device_is_named_by_its_label_and_said_by_the_fleet() {
        let tags = vec!["ci".to_string(), "eu".to_string()];
        let d = device(&tags, Some("Build box"));
        let m = mirror(d);
        assert_eq!(
            m.name, "Build box",
            "the display name, as the device list shows it"
        );
        assert_eq!(m.owner_id, d.owner_id);
        let paths: Vec<&str> = m.facts.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, SYNCED_FACTS, "every synced path, in its order");
        let fact = |p: &str| m.facts.iter().find(|(q, _)| q == p).unwrap().1.clone();
        assert_eq!(fact("machine_name"), Some(Bson::String("build-01".into())));
        assert_eq!(fact("os"), Some(Bson::String("linux".into())));
        assert_eq!(fact("version"), Some(Bson::String("0.4.154".into())));
        assert_eq!(fact("ephemeral"), Some(Bson::Boolean(false)));
        assert_eq!(
            fact("arch"),
            None,
            "an arch the device never said is no fact"
        );
        assert_eq!(
            fact("tags"),
            Some(Bson::Array(vec![
                Bson::String("ci".into()),
                Bson::String("eu".into())
            ]))
        );
    }

    #[test]
    fn a_label_the_map_cannot_keep_falls_back_to_the_machines_name() {
        let none: Vec<String> = Vec::new();
        assert_eq!(mirror(device(&none, None)).name, "build-01");
        assert_eq!(mirror(device(&none, Some("   "))).name, "build-01");
        assert_eq!(
            mirror(device(&none, Some("bad\u{7}label"))).name,
            "build-01"
        );
        let mut nameless = device(&none, None);
        nameless.name = "\u{1}";
        assert_eq!(
            mirror(nameless).name,
            nameless.id.to_hex(),
            "never nothing: the node id"
        );
    }

    #[test]
    fn the_synced_paths_are_fact_paths_each_once() {
        for p in SYNCED_FACTS {
            assert!(crate::model::check_fact_path(p).is_ok(), "{p}");
        }
        assert!(
            SYNCED_FACTS.windows(2).all(|w| w[0] < w[1]),
            "sorted, each once"
        );
    }
}
