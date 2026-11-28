// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 P1d — what a token rotation ends of this pod's DERP relay, and what
//! a DERP registration admits (`docs/fr/FR-155-one-row-new-credentials.md`).
//!
//! This commit carries the cells only: the record keeps today's behaviour
//! (the newest registration of a key is the one in place, and an end ends
//! nothing), and no path calls it yet.

use std::sync::Arc;
use std::time::Duration;

use bson::oid::ObjectId;
use tokio::sync::{Notify, mpsc};

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
/// admits for the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BelowGeneration {
    pub floor: u32,
}

/// The pod's record of its DERP connections and their generations.
#[derive(Default)]
pub struct DerpGenerations {}

impl DerpGenerations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Put `registration` in place.
    pub fn admit(
        &self,
        registry: &DerpRegistry,
        cancels: &DerpCancelRegistry,
        registration: Registration,
        rotation: bool,
    ) -> Result<Admitted, BelowGeneration> {
        let _ = rotation;
        let replaced = registry.insert(registration.key, registration.tx).is_some();
        cancels.insert(registration.key, registration.cancel);
        Ok(Admitted { id: 0, replaced })
    }

    /// End the agent's DERP connections below `generation` on this pod.
    pub fn end_below_generation(
        &self,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        generation: u32,
    ) -> usize {
        let _ = (agent_id, tenant_id, generation);
        0
    }

    /// [`Self::end_below_generation`], with the floor kept for `keep`.
    pub fn end_below_generation_for(
        &self,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        generation: u32,
        keep: Duration,
    ) -> usize {
        let _ = (agent_id, tenant_id, generation, keep);
        0
    }
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
    /// the newest registration is the one in place, whatever its generation.
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
        let lower = Probe::new(agent, tenant, 2, key);
        let admitted = generations
            .admit(&registry, &cancels, lower.registration(), false)
            .expect("with the rotation off, a registration is admitted as before");
        assert!(admitted.replaced);
        assert!(
            lower.in_place(&registry, &cancels),
            "with the rotation off, the newest registration is the one in place"
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
        let late = Probe::new(agent, tenant, 3, key);
        assert_eq!(
            generations.admit(&registry, &cancels, late.registration(), false),
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
        let early = Probe::new(agent, tenant, 3, key);
        generations
            .admit(&registry, &cancels, early.registration(), false)
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
        assert!(early.cancelled(), "by its cancel");
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
