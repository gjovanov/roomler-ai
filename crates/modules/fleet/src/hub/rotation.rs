// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 P1b and P1c — what a token rotation ends on this pod, and what a
//! registration admits (`docs/fr/FR-155-one-row-new-credentials.md`, "What
//! the commit ends").
//!
//! * **The sweep** ([`Hub::end_below_generation`]): a rotation to generation
//!   G+1 has committed, on this pod or, through the control channel's
//!   `end_below_generation`, on another. The device's entry admitted below
//!   G+1 leaves the hub and its cancel fires with [`CancelReason::Rotated`],
//!   so the disconnect line reads `by=rotated`. Nothing is sent to the device:
//!   `AgentDeleted` would stop the daemon, and the device that rotated is
//!   already connecting with its new token. The sessions the connection
//!   carried go as at any loss of its socket. An entry at G+1 or above stays,
//!   so an envelope that arrives after the device reconnected ends nothing.
//! * **The floor**: a socket whose upgrade was checked before the commit can
//!   reach the hub after the sweep. So the sweep keeps, per tenant and agent,
//!   the generation it named for [`ROTATION_FLOOR_SECS`], and a registration
//!   below it is refused ([`BelowFloor`]) without changing anything.
//! * **The registration's read (P1c)**: after the hello, and before anything
//!   is written, the socket reads the row on the primary and judges the
//!   connection's generation against its `token_gen` with FR-103's rule
//!   (`crate::socket::handle_agent_socket`). A registration so admits only
//!   the device's current generation, whatever the timing.
//! * **The entry of a higher generation stays (P1c)**: a registration below
//!   the entry it would replace is refused as one below the floor is
//!   ([`Hub::admit_agent`]).
//! * **The per-pod check (P1c)**: every pod ends an entry below its row's
//!   generation within one period of `rc.presence_sweep_secs`, also a pod
//!   that the rotation's control event does not reach
//!   (`crate::generation_check`).
//!
//! **The read and the floor together.** Take a connection that its upgrade
//! admitted at G, and a rotation that commits G+1. If the commit comes before
//! the registration's read, the read sees G+1 on the primary and refuses. If
//! it comes after the read, this pod's sweep runs after the commit too: on
//! the pod that commits, before the route answers; on any other, when the
//! control event arrives. A sweep before the admit has stored the floor,
//! which the admit reads under the entry's lock and refuses; a sweep after
//! the admit finds the entry and ends it (the lock below). The floor stands
//! at the admit, since [`ROTATION_FLOOR_SECS`] covers the time from the read
//! to the admit. So a pod that applies a commit admits no registration below
//! it, whatever the timing. And whatever the order of events, every pod's
//! own check ends an entry below its row's generation within one period
//! (`crate::generation_check`), also on a pod that the control event does
//! not reach.
//!
//! P1c's three parts run only while the rotation is on
//! ([`Hub::set_token_rotation`]): with it off, a registration reads nothing
//! new and is admitted as before, and no check runs. P1b's sweep and floor
//! run only when a rotation commits, and none commits while
//! `rc.agent_token_rotation` is off: the floors stay empty, and a
//! registration costs one lookup in that empty map.
//!
//! ⚠️ **One lock orders the floor and the entries.** Both halves run inside
//! the exclusive lock of the `agents` shard that holds the agent's key, which
//! `DashMap::entry` takes and keeps until the entry is dropped. The
//! registration reads the floor and inserts its entry in that lock
//! ([`Hub::admit_agent`]); the sweep stores the floor and removes an entry
//! below it in the same lock. Two critical sections of one lock are totally
//! ordered: if the sweep's runs first, the registration's reads the floor it
//! stored and refuses; if the registration's runs first, the sweep's finds
//! the entry and ends it. Neither can miss the other, and no atomic ordering
//! is involved. The locks are taken `agents` before `floors`, never the
//! reverse, and neither section awaits or touches `agents` again: a shard's
//! lock is not reentrant.
//!
//! P1c's check of the entry a registration would replace reads that entry in
//! the registration's section, so it is ordered with every other
//! registration and sweep of the agent the same way. The switch it reads
//! there is set once at startup and orders nothing.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use dashmap::mapref::entry::Entry;
use tracing::info;

use super::{CancelReason, ConnectedAgent, Hub};

/// How long a pod keeps a rotation's floor. While the rotation is on
/// ([`Hub::set_token_rotation`]), it covers the time between a registration's
/// read of the row, after the hello, and its admit: only the hello's write
/// and at most one read of the resume scope stand between the two, so the
/// floor outlasts that time by far. The module doc says why the read and the
/// floor together admit no registration below a commit this pod has applied.
pub const ROTATION_FLOOR_SECS: u64 = 600;

/// The generation a rotation named for one agent of one tenant, and until
/// when this pod refuses a registration below it.
pub(super) struct Floor {
    generation: u32,
    until: Instant,
}

/// A registration the hub refused: the generation its connection was
/// admitted under is below the lowest one this pod admits for the device,
/// which is the generation a rotation named ([`Hub::end_below_generation`])
/// or, while the rotation is on, that of the entry it would replace (P1c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BelowFloor {
    /// The generation the rotation named, or the entry's.
    pub floor: u32,
}

impl Hub {
    /// FR-155 P1c — whether this pod runs the token rotation: on while
    /// `rc.agent_token_rotation` and `rc.token_generation_check` both are,
    /// since a rotation commits only then. Set once at startup. While it is
    /// on, the agent socket reads the row at its registration, the entry of a
    /// higher generation stays ([`Self::admit_agent`]), and the pod runs its
    /// check (`crate::generation_check`).
    pub fn set_token_rotation(&self, on: bool) {
        self.inner.token_rotation.store(on, Ordering::Relaxed);
    }

    /// What [`Self::set_token_rotation`] set.
    pub fn token_rotation(&self) -> bool {
        self.inner.token_rotation.load(Ordering::Relaxed)
    }

    /// The registration's half: `entry` goes into the hub unless a live floor
    /// of its tenant and agent is above the generation it was admitted under,
    /// or, while the rotation is on, the entry it would replace is of the same
    /// tenant and a higher generation (P1c).
    /// Returns the entry it displaced.
    pub(super) fn admit_agent(
        &self,
        entry: ConnectedAgent,
    ) -> Result<Option<ConnectedAgent>, BelowFloor> {
        let slot = self.inner.agents.entry(entry.agent_id);
        if let Some(floor) = self.floor_of(entry.tenant_id, entry.agent_id)
            && entry.generation < floor
        {
            return Err(BelowFloor { floor });
        }
        // P1c — the entry of a higher generation stays, and the registration
        // below it is refused as one below the floor is.
        if let Entry::Occupied(held) = &slot
            && self.token_rotation()
            && held.get().tenant_id == entry.tenant_id
            && entry.generation < held.get().generation
        {
            return Err(BelowFloor {
                floor: held.get().generation,
            });
        }
        Ok(match slot {
            Entry::Occupied(mut held) => Some(held.insert(entry)),
            Entry::Vacant(free) => {
                free.insert(entry);
                None
            }
        })
    }

    /// The live floor of one tenant's agent, if any.
    fn floor_of(&self, tenant_id: ObjectId, agent_id: ObjectId) -> Option<u32> {
        let now = Instant::now();
        self.inner
            .floors
            .get(&(tenant_id, agent_id))
            .filter(|floor| now < floor.until)
            .map(|floor| floor.generation)
    }

    /// FR-155 P1c — each entry on this pod: its agent, its tenant and the
    /// generation it was admitted under. Collected before it returns, so the
    /// caller holds no guard (`crate::generation_check` awaits a read next).
    pub fn agent_generations(&self) -> Vec<(ObjectId, ObjectId, u32)> {
        self.inner
            .agents
            .iter()
            .map(|a| (a.agent_id, a.tenant_id, a.generation))
            .collect()
    }

    /// FR-155 P1b — a rotation of `agent_id`'s token to `generation` has
    /// committed: this pod keeps the floor, and ends the agent's entry
    /// admitted below `generation` in `tenant_id`. Returns whether an entry
    /// ended. Idempotent, and a late or repeated call ends nothing admitted
    /// at the generation or above.
    pub fn end_below_generation(
        &self,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        generation: u32,
    ) -> bool {
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
    ) -> bool {
        let now = Instant::now();
        let until = now.checked_add(keep).unwrap_or(now);
        let ended = {
            let slot = self.inner.agents.entry(agent_id);
            self.inner
                .floors
                .entry((tenant_id, agent_id))
                .and_modify(|floor| {
                    if now < floor.until {
                        floor.generation = floor.generation.max(generation);
                        floor.until = floor.until.max(until);
                    } else {
                        floor.generation = generation;
                        floor.until = until;
                    }
                })
                .or_insert(Floor { generation, until });
            match slot {
                Entry::Occupied(held)
                    if held.get().tenant_id == tenant_id && held.get().generation < generation =>
                {
                    Some(held.remove())
                }
                _ => None,
            }
        };
        // Outside the entry's lock: no other agent's registration waits on it.
        self.inner.floors.retain(|_, floor| now < floor.until);
        let Some(entry) = ended else {
            return false;
        };
        info!(
            %agent_id, generation, admitted = entry.generation,
            "agent offline (rotated)"
        );
        // `notify_one`, as the displacement does: it latches for a read loop
        // that is not waiting yet.
        entry.cancel.set_reason(CancelReason::Rotated);
        entry.cancel.notify_one();
        match entry.resume_window {
            Some(window_secs) => self.detach_agent_sessions(agent_id, window_secs),
            None => self.end_agent_sessions(agent_id),
        }
        true
    }
}
