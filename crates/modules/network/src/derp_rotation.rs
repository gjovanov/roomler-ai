// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 P1d — what a token rotation ends of this pod's DERP relay, and what
//! a DERP registration admits (`docs/fr/FR-155-one-row-new-credentials.md`,
//! "Only the device's current generation holds its DERP registration").
//!
//! [`DerpGenerations`] keeps, per tenant and agent, each of the agent's DERP
//! connections on this pod with the generation its upgrade admitted it under,
//! and the floor of the agent's last rotation.
//!
//! * **The end** ([`DerpGenerations::end_below_generation`]): a rotation to
//!   generation G+1 has committed, on this pod (the hook
//!   `agent_credentials_rotated`, `crate::credential_rotation`) or, through
//!   fleet's control event `end_below_generation`, on another (the hook
//!   `end_below_generation`, `crate::hooks`). Each of the agent's DERP
//!   connections on this pod admitted below G+1 closes by its cancel, as a
//!   cluster rehome closes one (`crate::derp_cluster::local_rehome_close`),
//!   and its teardown removes its registration only while it is the one in
//!   place (`crate::derp::handle_derp_socket`). A connection at G+1 or above
//!   stays, so an event that arrives after the device reconnected ends
//!   nothing.
//! * **The floor**: the end keeps, per tenant and agent, the generation it
//!   named for [`ROTATION_FLOOR_SECS`], and a registration below it is
//!   refused ([`BelowGeneration`]) and writes nothing.
//! * **The registration's read**: after the registration frame, and before
//!   anything is written, the socket reads the row on the primary and judges
//!   the connection's generation against its `token_gen` with FR-103's rule
//!   (`crate::derp::handle_derp_socket`). A DERP registration so admits only
//!   the device's current generation, whatever the timing.
//! * **The registration of a higher generation stays**: a registration below
//!   the generation of the registration it would replace is refused as one
//!   below the floor is ([`DerpGenerations::admit`]). An equal or a higher
//!   one replaces it, as a reconnect always has.
//! * **The per-pod check** ([`spawn_check`]): every pod ends a DERP
//!   connection below its row's generation within one period of
//!   `rc.presence_sweep_secs` (never under 5 s), plus the time of one read,
//!   also a pod that the rotation's control event does not reach.
//!
//! Each comparison is strict, so the equal case is admitted and stays: a
//! registration at the floor's generation is admitted, a reconnect at the
//! generation in place replaces it as before, and an end at G+1 leaves a
//! connection at G+1 alone.
//!
//! **The read and the floor together.** Take a DERP connection that its
//! upgrade admitted at G, and a rotation that commits G+1. If the commit
//! comes before the registration's read, the read sees G+1 on the primary
//! and refuses. If it comes after the read, this pod's end runs after the
//! commit too: on the pod that commits, in the hook, before the route
//! answers; on any other, when the control event arrives. An end before the
//! admit has stored the floor, which the admit reads under the agent's lock
//! and refuses; an end after the admit finds the connection and closes it
//! (the lock below). The floor stands at the admit: nothing is awaited
//! between the read and the admit, and the read is bounded
//! (`crate::derp::REGISTRATION_READ_SECS`) far below [`ROTATION_FLOOR_SECS`].
//! So a pod that applies a commit admits no DERP registration below it,
//! whatever the timing. And whatever the order of events, every pod's own
//! check closes a connection below its row's generation within one period
//! ([`spawn_check`]), also on a pod that the control event does not reach.
//!
//! The read, the rule of the higher generation and the check run only while
//! the rotation is on (fleet's `Hub::token_rotation`): with it off, a
//! registration reads nothing new, last-writer-wins as before, and no check
//! runs. The end and its floor run when
//! a rotation commits, and none commits while `rc.agent_token_rotation` is
//! off: the record then holds no floor, and a registration costs one entry in
//! it.
//!
//! ⚠️ **One lock orders the floor and the connections.** Both halves run
//! inside the exclusive lock of the record's shard that holds the agent's
//! (tenant, agent) key, which `DashMap::entry` takes and keeps until the
//! entry is dropped. The registration reads the floor and the registration
//! it would replace, writes the registry and the cancel, and records itself,
//! in that lock ([`DerpGenerations::admit`]); the end stores the floor and
//! fires the cancel of each connection below it in the same lock. Two
//! critical sections of one lock are totally ordered: if the end's runs
//! first, the registration's reads the floor it stored and refuses; if the
//! registration's runs first, the end's finds its connection and closes it.
//! Neither can miss the other, and no atomic ordering is involved. The locks
//! are taken the record first, then the registry, then the cancels, never
//! the reverse, and no section awaits or touches the record again: a shard's
//! lock is not reentrant. A connection leaves the record only after its
//! registration left the registry (its teardown's order), so while a
//! registration is in place, its connection is in the record.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use roomler_ai_mod_fleet::hub::ROTATION_FLOOR_SECS;
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

use crate::NetworkState;
use crate::derp_types::{DerpCancelRegistry, DerpKey, DerpRegistry};

/// One DERP connection asking to register its key.
pub struct Registration {
    pub tenant_id: ObjectId,
    pub agent_id: ObjectId,
    /// The generation its upgrade admitted it under.
    pub generation: u32,
    pub key: DerpKey,
    /// Its outbound queue: what the registry routes the key's frames to.
    pub tx: mpsc::Sender<Vec<u8>>,
    /// What closes it (its read loop's cancel arm).
    pub cancel: Arc<Notify>,
}

/// A registration in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admitted {
    /// This connection's id in the record, for its teardown.
    pub id: u64,
    /// Whether it took the key from a registration in place.
    pub replaced: bool,
}

/// A registration refused: its generation is below the lowest one this pod
/// admits for the device, which is the generation a rotation named
/// ([`DerpGenerations::end_below_generation`]) or, while the rotation is on,
/// that of the registration it would replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BelowGeneration {
    pub floor: u32,
}

/// The generation a rotation named for one agent of one tenant, and until
/// when this pod refuses a registration below it.
#[derive(Clone, Copy)]
struct Floor {
    generation: u32,
    until: Instant,
}

/// One DERP connection of the agent on this pod.
struct Connection {
    id: u64,
    generation: u32,
    tx: mpsc::Sender<Vec<u8>>,
    cancel: Arc<Notify>,
    /// An end fired its cancel.
    ended: bool,
}

/// One agent's floor and DERP connections on this pod.
#[derive(Default)]
struct AgentDerp {
    floor: Option<Floor>,
    connections: Vec<Connection>,
}

impl AgentDerp {
    fn floor_at(&self, now: Instant) -> Option<u32> {
        self.floor
            .filter(|floor| now < floor.until)
            .map(|floor| floor.generation)
    }

    fn idle_at(&self, now: Instant) -> bool {
        self.connections.is_empty() && self.floor_at(now).is_none()
    }
}

/// The pod's record of its DERP connections and their generations, keyed by
/// (tenant, agent). The module doc says what it guarantees, and why.
#[derive(Default)]
pub struct DerpGenerations {
    agents: dashmap::DashMap<(ObjectId, ObjectId), AgentDerp>,
    next_id: AtomicU64,
}

impl DerpGenerations {
    pub fn new() -> Self {
        Self::default()
    }

    /// The registration's half: `registration` goes in place, in the registry
    /// and the cancels, unless a live floor of its tenant and agent is above
    /// its generation, or, while the `rotation` is on, the registration it
    /// would replace is the agent's and of a higher generation. A refusal
    /// writes nothing.
    pub fn admit(
        &self,
        registry: &DerpRegistry,
        cancels: &DerpCancelRegistry,
        registration: Registration,
        rotation: bool,
    ) -> Result<Admitted, BelowGeneration> {
        let Registration {
            tenant_id,
            agent_id,
            generation,
            key,
            tx,
            cancel,
        } = registration;
        let now = Instant::now();
        let mut agent = self.agents.entry((tenant_id, agent_id)).or_default();
        if let Some(floor) = agent.floor_at(now)
            && generation < floor
        {
            return Err(BelowGeneration { floor });
        }
        // The registration of a higher generation stays, and one below it is
        // refused as one below the floor is.
        if rotation {
            let in_place = registry.get(&key).map(|held| held.value().clone());
            if let Some(in_place) = in_place
                && let Some(held) = agent
                    .connections
                    .iter()
                    .find(|connection| connection.tx.same_channel(&in_place))
                && generation < held.generation
            {
                return Err(BelowGeneration {
                    floor: held.generation,
                });
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let replaced = registry.insert(key, tx.clone()).is_some();
        cancels.insert(key, cancel.clone());
        agent.connections.push(Connection {
            id,
            generation,
            tx,
            cancel,
            ended: false,
        });
        Ok(Admitted { id, replaced })
    }

    /// FR-155 P1d — a rotation of `agent_id`'s token to `generation` has
    /// committed: this pod keeps the floor, and closes each of the agent's
    /// DERP connections admitted below `generation` in `tenant_id`, by its
    /// cancel. Returns how many it closed. Idempotent, and a late or repeated
    /// call closes nothing admitted at the generation or above.
    pub fn end_below_generation(
        &self,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        generation: u32,
    ) -> usize {
        self.end_below_generation_for(
            agent_id,
            tenant_id,
            generation,
            Duration::from_secs(ROTATION_FLOOR_SECS),
        )
    }

    /// [`Self::end_below_generation`], with the floor kept for `keep`: the
    /// integration cells set with it a floor that ends within a cell.
    pub fn end_below_generation_for(
        &self,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        generation: u32,
        keep: Duration,
    ) -> usize {
        let now = Instant::now();
        let until = now.checked_add(keep).unwrap_or(now);
        let ended = {
            let mut agent = self.agents.entry((tenant_id, agent_id)).or_default();
            let floor = match agent.floor {
                Some(floor) if now < floor.until => Floor {
                    generation: floor.generation.max(generation),
                    until: floor.until.max(until),
                },
                _ => Floor { generation, until },
            };
            agent.floor = Some(floor);
            let mut ended = 0;
            for connection in agent
                .connections
                .iter_mut()
                .filter(|connection| connection.generation < generation && !connection.ended)
            {
                connection.ended = true;
                // `notify_one`, as the rehome close does: it latches for a
                // read loop that is not waiting yet.
                connection.cancel.notify_one();
                ended += 1;
            }
            ended
        };
        // Outside the entry's lock: no other agent's registration waits on it.
        // Each connection it closed says so in its own close line
        // (`crate::derp::handle_derp_socket`).
        self.agents.retain(|_, agent| !agent.idle_at(now));
        ended
    }

    /// The teardown's half: the connection `id` of the agent is gone. Called
    /// after its registration left the registry.
    pub fn release(&self, agent_id: ObjectId, tenant_id: ObjectId, id: u64) {
        let now = Instant::now();
        if let Some(mut agent) = self.agents.get_mut(&(tenant_id, agent_id)) {
            agent.connections.retain(|connection| connection.id != id);
        }
        self.agents
            .remove_if(&(tenant_id, agent_id), |_, agent| agent.idle_at(now));
    }

    /// Whether an end closed the connection `id` (its close line says why).
    pub fn ended(&self, agent_id: ObjectId, tenant_id: ObjectId, id: u64) -> bool {
        self.agents
            .get(&(tenant_id, agent_id))
            .is_some_and(|agent| {
                agent
                    .connections
                    .iter()
                    .any(|connection| connection.id == id && connection.ended)
            })
    }

    /// Each connection on this pod that no end has closed: its agent, its
    /// tenant and the generation it was admitted under. Collected before it
    /// returns, so the caller holds no guard ([`check`] awaits a read next).
    pub fn generations(&self) -> Vec<(ObjectId, ObjectId, u32)> {
        let mut out = Vec::new();
        for agent in self.agents.iter() {
            let (tenant_id, agent_id) = *agent.key();
            out.extend(
                agent
                    .connections
                    .iter()
                    .filter(|connection| !connection.ended)
                    .map(|connection| (agent_id, tenant_id, connection.generation)),
            );
        }
        out
    }
}

/// FR-155 P1d — start this pod's DERP check, unless the rotation is off. The
/// first round runs one period after the start. Each pod checks its own
/// connections on its own loop and reads Mongo only, as fleet's
/// `generation_check` does for the hub.
pub fn spawn_check(state: NetworkState) {
    if !state.fleet.rc_hub.token_rotation() {
        return;
    }
    let every = Duration::from_secs(state.fleet.settings.rc.presence_sweep_secs.max(5));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let ended = check(&state).await;
            if ended > 0 {
                info!(
                    ended,
                    "derp generation check: connections below their row's generation ended"
                );
            }
        }
    });
}

/// One round: how many connections it closed. It reads the live rows'
/// `token_gen` on the primary in one query, and closes each connection below
/// its row's generation through [`DerpGenerations::end_below_generation`]:
/// the cancel, the teardown and the floor, as at a commit. A removed or
/// quarantined row is not read: the removal's own paths answer it. A failed
/// read closes nothing, and the next round reads again.
pub async fn check(state: &NetworkState) -> usize {
    let held = state.derp_generations.generations();
    if held.is_empty() {
        return 0;
    }
    let ids: Vec<ObjectId> = held
        .iter()
        .map(|(agent_id, _, _)| *agent_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let rows: HashMap<ObjectId, (ObjectId, u32)> = match state
        .fleet
        .agents
        .live_generations_on_the_primary(&ids)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|(agent_id, tenant_id, generation)| (agent_id, (tenant_id, generation)))
            .collect(),
        Err(e) => {
            warn!(
                %e, connections = held.len(),
                "derp generation check: rows unreadable; the next round reads again"
            );
            return 0;
        }
    };
    let mut below = HashSet::new();
    for (agent_id, tenant_id, generation) in held {
        if let Some(&(row_tenant, row_generation)) = rows.get(&agent_id)
            && row_tenant == tenant_id
            && generation < row_generation
        {
            below.insert((agent_id, tenant_id, row_generation));
        }
    }
    below
        .into_iter()
        .map(|(agent_id, tenant_id, row_generation)| {
            state
                .derp_generations
                .end_below_generation(agent_id, tenant_id, row_generation)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use futures::FutureExt as _;

    /// One connection's sender and cancel, as a test watches them.
    struct Probe {
        agent_id: ObjectId,
        tenant_id: ObjectId,
        generation: u32,
        key: DerpKey,
        tx: mpsc::Sender<Vec<u8>>,
        cancel: Arc<Notify>,
    }

    impl Probe {
        fn new(agent_id: ObjectId, tenant_id: ObjectId, generation: u32, key: DerpKey) -> Self {
            let (tx, _rx) = mpsc::channel(4);
            Self {
                agent_id,
                tenant_id,
                generation,
                key,
                tx,
                cancel: Arc::new(Notify::new()),
            }
        }

        fn registration(&self) -> Registration {
            Registration {
                tenant_id: self.tenant_id,
                agent_id: self.agent_id,
                generation: self.generation,
                key: self.key,
                tx: self.tx.clone(),
                cancel: self.cancel.clone(),
            }
        }

        /// The registry routes the key to this connection, and its cancel is
        /// the key's.
        fn in_place(&self, registry: &DerpRegistry, cancels: &DerpCancelRegistry) -> bool {
            registry
                .get(&self.key)
                .is_some_and(|held| held.same_channel(&self.tx))
                && cancels
                    .get(&self.key)
                    .is_some_and(|held| Arc::ptr_eq(held.value(), &self.cancel))
        }

        /// Its cancel fired (a permit is stored for its read loop).
        fn cancelled(&self) -> bool {
            self.cancel.notified().now_or_never().is_some()
        }
    }

    fn maps() -> (DerpRegistry, DerpCancelRegistry) {
        (Arc::new(DashMap::new()), Arc::new(DashMap::new()))
    }

    /// T20 (FR-155 P1d): with the rotation on, a DERP registration below the
    /// generation of the registration it would replace is refused, and the
    /// registration in place stays, its cancel unfired. An equal or a higher
    /// generation replaces it, and two registrations of two generations at
    /// once leave the higher one in place, 500 times. With the rotation off,
    /// last-writer-wins as before.
    #[test]
    fn the_derp_registry_keeps_the_registration_of_the_higher_generation() {
        let (agent, tenant) = (ObjectId::new(), ObjectId::new());
        let key: DerpKey = (ObjectId::new(), [7; 32]);

        let (registry, cancels) = maps();
        let generations = DerpGenerations::new();
        let held = Probe::new(agent, tenant, 3, key);
        generations
            .admit(&registry, &cancels, held.registration(), true)
            .expect("the first registration");
        let below = Probe::new(agent, tenant, 2, key);
        assert_eq!(
            generations.admit(&registry, &cancels, below.registration(), true),
            Err(BelowGeneration { floor: 3 }),
            "the registration of the higher generation stays, and a registration below it is refused"
        );
        assert!(
            held.in_place(&registry, &cancels),
            "the registration in place stays"
        );
        assert!(!held.cancelled(), "its cancel unfired");

        let equal = Probe::new(agent, tenant, 3, key);
        let admitted = generations
            .admit(&registry, &cancels, equal.registration(), true)
            .expect("an equal generation replaces it");
        assert!(admitted.replaced);
        assert!(equal.in_place(&registry, &cancels));
        let higher = Probe::new(agent, tenant, 4, key);
        let admitted = generations
            .admit(&registry, &cancels, higher.registration(), true)
            .expect("a higher generation replaces it");
        assert!(admitted.replaced);
        assert!(higher.in_place(&registry, &cancels));

        for round in 0..500 {
            let (registry, cancels) = maps();
            let generations = DerpGenerations::new();
            let low = Probe::new(agent, tenant, 5, key);
            let high = Probe::new(agent, tenant, 6, key);
            let start = std::sync::Barrier::new(2);
            std::thread::scope(|s| {
                for probe in [&low, &high] {
                    let (registry, cancels, generations, start) =
                        (&registry, &cancels, &generations, &start);
                    s.spawn(move || {
                        start.wait();
                        let _ = generations.admit(registry, cancels, probe.registration(), true);
                    });
                }
            });
            assert!(
                high.in_place(&registry, &cancels),
                "round {round}: two registrations at once leave the higher generation in place"
            );
        }

        let (registry, cancels) = maps();
        let generations = DerpGenerations::new();
        let first = Probe::new(agent, tenant, 3, key);
        generations
            .admit(&registry, &cancels, first.registration(), false)
            .expect("the first registration");
        let last = Probe::new(agent, tenant, 2, key);
        let admitted = generations
            .admit(&registry, &cancels, last.registration(), false)
            .expect("with the rotation off, a registration is admitted as before");
        assert!(admitted.replaced);
        assert!(
            last.in_place(&registry, &cancels),
            "with the rotation off, last-writer-wins as before"
        );
    }

    /// T23 (FR-155 P1d): in either order, the end of an agent's DERP
    /// registrations below a generation and a registration below it leave no
    /// registration below it. After the end, a registration below it is
    /// refused (the end keeps a floor) and writes nothing. Before the end, the
    /// end closes it by its cancel, also when a newer registration took its
    /// key. A registration at the end's generation is admitted either way,
    /// and the end leaves it alone.
    #[test]
    fn the_derp_registry_admits_nothing_below_an_ended_generation() {
        let (agent, tenant) = (ObjectId::new(), ObjectId::new());
        let key: DerpKey = (ObjectId::new(), [9; 32]);

        let (registry, cancels) = maps();
        let generations = DerpGenerations::new();
        assert_eq!(
            generations.end_below_generation(agent, tenant, 4),
            0,
            "nothing to end yet"
        );
        let after_end = Probe::new(agent, tenant, 3, key);
        assert_eq!(
            generations.admit(&registry, &cancels, after_end.registration(), false),
            Err(BelowGeneration { floor: 4 }),
            "after the end, a registration below its generation is refused"
        );
        assert!(
            registry.get(&key).is_none() && cancels.get(&key).is_none(),
            "a refused registration writes nothing"
        );
        let current = Probe::new(agent, tenant, 4, key);
        generations
            .admit(&registry, &cancels, current.registration(), false)
            .expect("a registration at the end's generation");
        assert!(current.in_place(&registry, &cancels));

        let (registry, cancels) = maps();
        let generations = DerpGenerations::new();
        let before_end = Probe::new(agent, tenant, 3, key);
        generations
            .admit(&registry, &cancels, before_end.registration(), false)
            .expect("a registration before the end");
        let current = Probe::new(agent, tenant, 4, key);
        generations
            .admit(&registry, &cancels, current.registration(), false)
            .expect("a registration at the end's generation, before the end");
        assert_eq!(
            generations.end_below_generation(agent, tenant, 4),
            1,
            "the end closes the registration below its generation"
        );
        assert!(before_end.cancelled(), "by its cancel");
        assert!(
            !current.cancelled() && current.in_place(&registry, &cancels),
            "the end leaves the registration at its generation alone"
        );
        assert_eq!(
            generations.end_below_generation(ObjectId::new(), tenant, 9),
            0,
            "another agent's end closes nothing of this one"
        );
        assert_eq!(
            generations.end_below_generation(agent, ObjectId::new(), 9),
            0,
            "the end is the tenant's"
        );
        assert!(!current.cancelled());
    }
}
