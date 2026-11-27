// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 P1b — what a token rotation ends on this pod
//! (`docs/fr/FR-155-one-row-new-credentials.md`, "What the commit ends").
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
//!
//! Nothing here runs unless a rotation commits, and none commits while
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

use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use dashmap::mapref::entry::Entry;
use tracing::info;

use super::{CancelReason, ConnectedAgent, Hub};

/// How long a pod keeps a rotation's floor. Longer than a connection takes
/// from its upgrade's check to its registration (its hello, a hello write
/// and a read), so the floor outlasts every socket upgraded before the
/// commit that registers at all in that time.
pub const ROTATION_FLOOR_SECS: u64 = 600;

/// The generation a rotation named for one agent of one tenant, and until
/// when this pod refuses a registration below it.
pub(super) struct Floor {
    generation: u32,
    until: Instant,
}

/// A registration the floor refused: its connection was admitted below the
/// generation a rotation named ([`Hub::end_below_generation`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BelowFloor {
    /// The generation the rotation named.
    pub floor: u32,
}

impl Hub {
    /// The registration's half: `entry` goes into the hub unless a live floor
    /// of its tenant and agent is above the generation it was admitted under.
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

    /// [`Self::end_below_generation`], with the floor kept for `keep`.
    pub(super) fn end_below_generation_for(
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
