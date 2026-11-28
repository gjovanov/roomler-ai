// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4574 — the one rule that admits a tunnel client's control socket.
//!
//! A tunnel client's socket is tested against its row in two places: the
//! `/ws?role=tunnel-client` upgrade, which decides whether the socket starts
//! ([`crate::tunnel::ws_upgrade_tunnel_client`]), and the 60 s check, on every
//! tick while the socket is open (`tunnel::spawn_revocation_check`). Both ask
//! [`refusal`], with the generation of the token the upgrade admitted the
//! socket with, and nothing else decides whether the socket may stay open.
//!
//! It lives in `network`, beside the socket it admits: both callers are this
//! module's, and the generation test it reuses as it is,
//! [`generation_refusal`], is fleet's, which the `network → fleet` edge of the
//! module DAG reaches.

use roomler_ai_mod_fleet::auth_agent::generation_refusal;
use roomler_ai_remote_control::models::{AgentStatus, TunnelClient};

/// Why a tunnel client's row refuses a socket. [`Refusal::reason`] is what the
/// socket is told in `rc:tunnel.revoked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The row is deleted. Checked first, so a deleted client is told so
    /// whatever its status and its generation say.
    Deleted,
    /// The row is live and quarantined.
    Quarantined,
    /// The row is live and unenrolled.
    Unenrolled,
    /// The row is live, `Online` or `Offline`, and the socket's token is not of
    /// the row's generation: [`generation_refusal`]'s reason, as it is.
    Generation(&'static str),
}

impl Refusal {
    /// What the socket is told in `rc:tunnel.revoked`.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Deleted => "tunnel client has been deleted",
            Self::Quarantined => "tunnel client is quarantined",
            Self::Unenrolled => "tunnel client is unenrolled",
            Self::Generation(reason) => reason,
        }
    }
}

/// The rule: whether `client`'s row admits a socket whose token is of
/// generation `admitted_gen`. `None` admits it. `enforced` is
/// `rc.token_generation_check`, the generation test's kill switch.
///
/// A row admits a socket while it is live, its status is `Online` or
/// `Offline`, and the socket's generation is the row's.
pub fn refusal(client: &TunnelClient, admitted_gen: u32, enforced: bool) -> Option<Refusal> {
    refusal_parts(
        client.deleted_at.is_some(),
        client.status,
        client.token_gen,
        admitted_gen,
        enforced,
    )
}

/// The rule over just the fields it reads, split out so the table below can
/// drive it, as `auth_agent::refusal_reason_parts` is: `TunnelClient` has no
/// constructor, and a struct literal in a test breaks whenever the model gains
/// a field.
fn refusal_parts(
    deleted: bool,
    status: AgentStatus,
    row_gen: u32,
    admitted_gen: u32,
    enforced: bool,
) -> Option<Refusal> {
    if deleted {
        return Some(Refusal::Deleted);
    }
    // Every status by name, so a status added to `AgentStatus` does not
    // compile here until it has an answer.
    match status {
        AgentStatus::Online | AgentStatus::Offline => {}
        AgentStatus::Quarantined => return Some(Refusal::Quarantined),
        AgentStatus::Unenrolled => return Some(Refusal::Unenrolled),
    }
    // A tunnel client does not rotate (FR-155).
    generation_refusal(admitted_gen, row_gen, false, enforced).map(Refusal::Generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentStatus::{Offline, Online, Quarantined, Unenrolled};

    const EVERY_STATUS: [AgentStatus; 4] = [Online, Offline, Quarantined, Unenrolled];

    /// `(row_gen, admitted_gen)` pairs whose two generations are equal: both at
    /// 0, and both at a later generation.
    const EQUAL: [(u32, u32); 2] = [(0, 0), (3, 3)];

    /// Pairs whose two generations differ: the row's above the socket's, and
    /// the socket's above the row's.
    const UNEQUAL: [(u32, u32); 2] = [(1, 0), (0, 1)];

    fn every_generation_pair() -> impl Iterator<Item = (u32, u32)> {
        EQUAL.into_iter().chain(UNEQUAL)
    }

    /// One row of the table: every input to one answer.
    fn answer(
        deleted: bool,
        status: AgentStatus,
        (row_gen, admitted_gen): (u32, u32),
        enforced: bool,
    ) -> Option<Refusal> {
        refusal_parts(deleted, status, row_gen, admitted_gen, enforced)
    }

    /// The generation refusal, in fleet's words.
    fn generation() -> Refusal {
        Refusal::Generation(
            generation_refusal(0, 1, false, true)
                .expect("fleet refuses a generation not the row's"),
        )
    }

    #[test]
    fn a_deleted_row_refuses_whatever_its_status_generation_and_switch_say() {
        for status in EVERY_STATUS {
            for gens in every_generation_pair() {
                for enforced in [true, false] {
                    assert_eq!(
                        answer(true, status, gens, enforced),
                        Some(Refusal::Deleted),
                        "deleted, {status:?}, (row, socket) generations {gens:?}, switch {enforced}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_live_row_refuses_every_status_but_online_and_offline() {
        for (status, want) in [
            (Quarantined, Refusal::Quarantined),
            (Unenrolled, Refusal::Unenrolled),
        ] {
            for gens in every_generation_pair() {
                for enforced in [true, false] {
                    assert_eq!(
                        answer(false, status, gens, enforced),
                        Some(want),
                        "live, {status:?}, (row, socket) generations {gens:?}, switch {enforced}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_online_or_offline_row_admits_a_socket_of_its_own_generation_only() {
        for status in [Online, Offline] {
            for gens in EQUAL {
                for enforced in [true, false] {
                    assert_eq!(
                        answer(false, status, gens, enforced),
                        None,
                        "live, {status:?}, (row, socket) generations {gens:?}, switch {enforced}"
                    );
                }
            }
            for gens in UNEQUAL {
                assert_eq!(
                    answer(false, status, gens, true),
                    Some(generation()),
                    "live, {status:?}, (row, socket) generations {gens:?}, switch on"
                );
            }
        }
    }

    #[test]
    fn the_switch_off_admits_a_socket_of_any_generation() {
        for status in [Online, Offline] {
            for gens in UNEQUAL {
                assert_eq!(
                    answer(false, status, gens, false),
                    None,
                    "live, {status:?}, (row, socket) generations {gens:?}, switch off"
                );
            }
        }
    }

    #[test]
    fn each_refusal_says_its_own_reason_and_the_generation_says_fleets() {
        let own =
            [Refusal::Deleted, Refusal::Quarantined, Refusal::Unenrolled].map(Refusal::reason);
        for (i, reason) in own.iter().enumerate() {
            assert!(!reason.is_empty(), "{reason:?}");
            assert!(!own[i + 1..].contains(reason), "{reason:?} is said once");
        }
        let fleet = generation_refusal(0, 1, false, true).expect("a generation not the row's");
        assert_eq!(Refusal::Generation(fleet).reason(), fleet);
        assert!(!own.contains(&fleet), "{fleet:?}");
    }
}
