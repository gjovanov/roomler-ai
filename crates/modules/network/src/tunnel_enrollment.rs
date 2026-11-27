// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-159 P1 — what a tunnel-client enrollment does with the rows it finds.
//!
//! `POST /api/tunnel-client/enroll` (`routes::tunnel::enroll_tunnel_client`)
//! reads and acts; [`decide`] decides. The decision is one pure function of
//! four inputs: the request ([`InstanceRead`]), the row found by each id
//! ([`Found`]), the scope's answer for the row it would revive ([`Scope`],
//! #3328's `revive_scope`) and the plan cap's answer ([`Cap`]). It asks for
//! the scope's answer, and then for the cap's, only when it needs one
//! ([`Need`]), so the route reads each only when it decides something: an
//! owner's revival reads neither, as before FR-159.
//!
//! A request without an `instance_id` is an older client's, and takes the
//! path of before: the row by `machine_id`, revived within the scope, or a
//! new row under the cap. A request with one revives the row that holds it;
//! else claims, once, an unclaimed row with the request's `machine_id` that
//! the enroller may revive; else gets a row of its own, under the cap. Every
//! refusal is decided here, before the token is spent. The outcome table of
//! `docs/fr/FR-159-tunnel-client-identity.md` is this module's test, row by
//! row.

use bson::oid::ObjectId;
use roomler_ai_services::dao::tunnel_client::ReviveScope;
use serde::Serialize;

/// How long an install's id is: 32 lowercase hex characters (128 bits), the
/// shape the legacy `machine_id` has (D1).
const INSTANCE_ID_LEN: usize = 32;

/// What a malformed `instance_id` is told: 400, before anything is read or
/// spent.
pub const MALFORMED_INSTANCE_ID: &str = "instance_id must be 32 lowercase hexadecimal characters";

/// The request's `instance_id`, as the server reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceRead<'a> {
    /// The request carries none: an older client, answered as before FR-159.
    Absent,
    /// The request carries one and `rc.tunnel_instance_id` is off: looked up
    /// as an older client's request is, and told `keyed_by: machine_id`.
    Ignored,
    /// The install's own id, well formed, with the switch on.
    Used(&'a str),
}

/// Read the request's `instance_id` under the switch (`on`). `Err` is the
/// 400's sentence, for an id that is not 32 lowercase hex while the switch is
/// on. A JSON `null` is no id, as an absent field is.
pub fn read_instance_id(raw: Option<&str>, on: bool) -> Result<InstanceRead<'_>, &'static str> {
    match raw {
        None => Ok(InstanceRead::Absent),
        Some(_) if !on => Ok(InstanceRead::Ignored),
        Some(id) if is_instance_id(id) => Ok(InstanceRead::Used(id)),
        Some(_) => Err(MALFORMED_INSTANCE_ID),
    }
}

fn is_instance_id(id: &str) -> bool {
    id.len() == INSTANCE_ID_LEN && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `keyed_by` in the answer: the id the row is found by again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyedBy {
    /// The install's own id: the row holds it.
    InstanceId,
    /// The legacy `machine_id` alone, as before FR-159: the switch is off.
    MachineId,
}

/// What the answer says of `keyed_by`. Only a request that carried an
/// `instance_id` is told: an older client is answered as before, and an older
/// server never says it, which tells a newer client it was keyed the old way.
pub fn keyed_by(read: InstanceRead<'_>) -> Option<KeyedBy> {
    match read {
        InstanceRead::Absent => None,
        InstanceRead::Ignored => Some(KeyedBy::MachineId),
        InstanceRead::Used(_) => Some(KeyedBy::InstanceId),
    }
}

/// What the decision reads of a row the route found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row<'a> {
    pub id: ObjectId,
    /// The install that holds the row, if one does.
    pub instance_id: Option<&'a str>,
}

/// The rows the route found, live or removed: one by each id of the request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Found<'a> {
    /// By the request's `instance_id`: only an [`InstanceRead::Used`]
    /// request has one.
    pub by_instance: Option<Row<'a>>,
    /// By the request's `machine_id`. The route reads it only when no row
    /// holds the request's `instance_id`, and [`decide`] does not look at it
    /// when one does.
    pub by_machine: Option<Row<'a>>,
}

/// Which of the two rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    Instance,
    Machine,
}

/// `revive_scope`'s answer for the row an enrollment would revive (#3328).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The enroller may revive it, and the write holds this scope.
    In(ReviveScope),
    /// A live row of someone else, and the enroller does not manage devices.
    SomeoneElses,
    /// A removed row, and the enroller does not manage devices: its owner
    /// included.
    Removed,
}

/// The plan cap's answer for a new row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cap {
    /// A new row fits the plan.
    Room,
    /// The cap is reached: a new row is refused 403 with this sentence.
    Full(String),
}

/// One step of the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step<'a> {
    /// An answer the decision needs and was not given. Nothing is spent or
    /// written yet.
    Need(Need),
    /// The decision: what to do after the spend, or the refusal before it.
    Decided(Result<Action<'a>, Refusal>),
}

/// An answer [`decide`] asks the route for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// The scope's answer for the row found by this id.
    Scope(By),
    /// The plan cap's answer.
    Cap,
}

/// What an enrollment that is not refused does, after the token is spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action<'a> {
    /// Revive this row within the scope: the row found by the legacy
    /// `machine_id` for an older client, or the row the install's id holds.
    Revive { id: ObjectId, scope: ReviveScope },
    /// Claim this unclaimed row for the install `instance_id`: the revival,
    /// and the row takes the id in the same update, which matches only while
    /// the row holds none. A claim that lost its race to another install
    /// becomes `if_taken`.
    Claim {
        id: ObjectId,
        instance_id: &'a str,
        scope: ReviveScope,
        if_taken: IfTaken,
    },
    /// A row of its own, which the cap has room for: an install's, keyed by
    /// its `instance_id`, or an older client's, keyed by its `machine_id`.
    Create { instance_id: Option<&'a str> },
}

/// What a claim that lost its race to another install becomes: a new row,
/// so the cap is read before the spend for it too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IfTaken {
    /// A row of its own.
    Create,
    /// The cap is reached. The token is spent by then: like a revival whose
    /// scope was lost meanwhile, this is a race's answer.
    Refuse(Refusal),
}

/// A refusal. [`decide`] answers each one before the spend, so nothing is
/// written and the token can still enroll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// 409: a live row someone else owns, found by the legacy `machine_id`.
    SomeoneElses,
    /// 409: a live row someone else owns, found by the install's own id.
    /// Another name changes nothing there, so its sentence says what does.
    SomeoneElsesInstance,
    /// 409: a removed row, and the enroller does not manage devices.
    Removed,
    /// 403: the plan's cap, with its sentence.
    Cap(String),
}

/// What the row an enrollment found is to the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match<'a> {
    /// An older client's request (or the switch off), and the row its
    /// `machine_id` names, whatever install holds it: the path of before.
    Legacy,
    /// The row the request's own `instance_id` holds.
    Install,
    /// A row with the request's `machine_id` that no install holds yet,
    /// which the install `own` may claim.
    Unclaimed { own: &'a str },
}

/// What an enrollment does, one step at a time. [`Step::Need`] asks for the
/// scope's answer for the row found ([`Need::Scope`]), or for the cap's for a
/// row that may be made ([`Need::Cap`]); the route reads it and asks again
/// with the answer. [`Step::Decided`] is the decision, once every answer it
/// needs is given: the action after the spend, or the refusal before it.
pub fn decide<'a>(
    request: InstanceRead<'a>,
    found: &Found<'_>,
    scope: Option<Scope>,
    cap: Option<&Cap>,
) -> Step<'a> {
    let (by, row, found_as) = match request {
        InstanceRead::Absent | InstanceRead::Ignored => match found.by_machine {
            Some(row) => (By::Machine, row, Match::Legacy),
            None => return new_row(cap, None),
        },
        InstanceRead::Used(own) => match (found.by_instance, found.by_machine) {
            (Some(row), _) => (By::Instance, row, Match::Install),
            // Read second, and holding this install's id after all: the same
            // install claimed it between the two reads.
            (None, Some(row)) if row.instance_id == Some(own) => (By::Machine, row, Match::Install),
            (None, Some(row)) if row.instance_id.is_none() => {
                (By::Machine, row, Match::Unclaimed { own })
            }
            // Another install's row, or none: a row of its own.
            (None, _) => return new_row(cap, Some(own)),
        },
    };
    let Some(scope) = scope else {
        return Step::Need(Need::Scope(by));
    };
    match (found_as, scope) {
        (Match::Unclaimed { own }, Scope::In(scope)) => {
            // A claim that loses its race to another install becomes a new
            // row, so the cap is read before the spend for it too. The claim
            // itself takes no slot.
            let Some(cap) = cap else {
                return Step::Need(Need::Cap);
            };
            let if_taken = match cap {
                Cap::Room => IfTaken::Create,
                Cap::Full(why) => IfTaken::Refuse(Refusal::Cap(why.clone())),
            };
            Step::Decided(Ok(Action::Claim {
                id: row.id,
                instance_id: own,
                scope,
                if_taken,
            }))
        }
        (Match::Legacy | Match::Install, Scope::In(scope)) => {
            Step::Decided(Ok(Action::Revive { id: row.id, scope }))
        }
        // D4: someone else's live row, met by the legacy hash alone, is a
        // guess that it is this install's. A row of its own, as a new name
        // always made one.
        (Match::Unclaimed { own }, Scope::SomeoneElses) => new_row(cap, Some(own)),
        (Match::Install, Scope::SomeoneElses) => Step::Decided(Err(Refusal::SomeoneElsesInstance)),
        (Match::Legacy, Scope::SomeoneElses) => Step::Decided(Err(Refusal::SomeoneElses)),
        // A removed row comes back only through a device manager, by either
        // id (D4): no row of its own instead.
        (_, Scope::Removed) => Step::Decided(Err(Refusal::Removed)),
    }
}

/// A row of its own, under the cap.
fn new_row<'a>(cap: Option<&Cap>, instance_id: Option<&'a str>) -> Step<'a> {
    match cap {
        None => Step::Need(Need::Cap),
        Some(Cap::Room) => Step::Decided(Ok(Action::Create { instance_id })),
        Some(Cap::Full(why)) => Step::Decided(Err(Refusal::Cap(why.clone()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";
    const FULL: &str = "Tunnel-client limit reached";

    fn row(instance_id: Option<&str>) -> Row<'_> {
        Row {
            id: ObjectId::new(),
            instance_id,
        }
    }

    fn mine() -> ReviveScope {
        ReviveScope::LiveOwnedBy(ObjectId::from_bytes([1; 12]))
    }

    fn full() -> Cap {
        Cap::Full(FULL.to_string())
    }

    /// The decision once every answer it asks for is given, in the order it
    /// asks: the scope's answer first, then the cap's. Each ask is checked to
    /// be one the inputs make: the scope only of a row that was found, and
    /// never twice.
    fn settle<'a>(
        request: InstanceRead<'a>,
        found: &Found<'_>,
        scope: Option<Scope>,
        cap: Option<Cap>,
    ) -> (Result<Action<'a>, Refusal>, Vec<Need>) {
        let (mut given_scope, mut given_cap, mut asked) = (None, None, Vec::new());
        loop {
            match decide(request, found, given_scope, given_cap.as_ref()) {
                Step::Decided(decision) => return (decision, asked),
                Step::Need(need) => {
                    assert!(!asked.contains(&need), "asked twice for {need:?}");
                    asked.push(need);
                    match need {
                        Need::Scope(by) => {
                            let found_row = match by {
                                By::Instance => found.by_instance,
                                By::Machine => found.by_machine,
                            };
                            assert!(found_row.is_some(), "the scope of a row not found");
                            given_scope = Some(scope.expect("the case gives a scope"));
                        }
                        Need::Cap => given_cap = Some(cap.clone().expect("the case gives a cap")),
                    }
                }
            }
        }
    }

    /// The spec's outcome table ("How the server reads an enrollment"), one
    /// case per row, with the cap's answer both ways wherever a new row can
    /// be made. Each case names what the route is asked to read: an answer
    /// read when it decides nothing is a read too many (the cap's read also
    /// counts a collision), and one not read is a decision made without it.
    #[test]
    fn every_row_of_the_outcome_table() {
        let legacy = row(None);
        let claimed_by_other = row(Some(OTHER));
        let held = row(Some(OWN));
        let used = InstanceRead::Used(OWN);
        let room = Some(Cap::Room);
        let at_cap = Some(full());
        let removed_scope = ReviveScope::LiveOrRemoved(ObjectId::from_bytes([2; 12]));

        struct Case<'a> {
            what: &'a str,
            request: InstanceRead<'a>,
            found: Found<'a>,
            scope: Option<Scope>,
            cap: Option<Cap>,
            asks: Vec<Need>,
            then: Result<Action<'a>, Refusal>,
        }
        let cases = vec![
            // Row 1: no instance_id — exactly the path of before.
            Case {
                what: "an older client, its row in scope: revived, no cap read",
                request: InstanceRead::Absent,
                found: Found {
                    by_instance: None,
                    by_machine: Some(legacy),
                },
                scope: Some(Scope::In(mine())),
                cap: None,
                asks: vec![Need::Scope(By::Machine)],
                then: Ok(Action::Revive {
                    id: legacy.id,
                    scope: mine(),
                }),
            },
            Case {
                what: "an older client, someone else's live row: 409 as before",
                request: InstanceRead::Absent,
                found: Found {
                    by_instance: None,
                    by_machine: Some(legacy),
                },
                scope: Some(Scope::SomeoneElses),
                cap: None,
                asks: vec![Need::Scope(By::Machine)],
                then: Err(Refusal::SomeoneElses),
            },
            Case {
                what: "an older client, a removed row out of scope: 409 as before",
                request: InstanceRead::Absent,
                found: Found {
                    by_instance: None,
                    by_machine: Some(legacy),
                },
                scope: Some(Scope::Removed),
                cap: None,
                asks: vec![Need::Scope(By::Machine)],
                then: Err(Refusal::Removed),
            },
            Case {
                what: "an older client, a row an install claimed: revived as before",
                request: InstanceRead::Absent,
                found: Found {
                    by_instance: None,
                    by_machine: Some(claimed_by_other),
                },
                scope: Some(Scope::In(mine())),
                cap: None,
                asks: vec![Need::Scope(By::Machine)],
                then: Ok(Action::Revive {
                    id: claimed_by_other.id,
                    scope: mine(),
                }),
            },
            Case {
                what: "an older client, no row: a new one under the cap",
                request: InstanceRead::Absent,
                found: Found::default(),
                scope: None,
                cap: room.clone(),
                asks: vec![Need::Cap],
                then: Ok(Action::Create { instance_id: None }),
            },
            Case {
                what: "the switch off makes an older client's row for a request with an id",
                request: InstanceRead::Ignored,
                found: Found::default(),
                scope: None,
                cap: room.clone(),
                asks: vec![Need::Cap],
                then: Ok(Action::Create { instance_id: None }),
            },
            Case {
                what: "an older client, no row, at the cap: 403",
                request: InstanceRead::Absent,
                found: Found::default(),
                scope: None,
                cap: at_cap.clone(),
                asks: vec![Need::Cap],
                then: Err(Refusal::Cap(FULL.to_string())),
            },
            Case {
                what: "the switch off reads a request with an id as an older client's",
                request: InstanceRead::Ignored,
                found: Found {
                    by_instance: None,
                    by_machine: Some(legacy),
                },
                scope: Some(Scope::In(mine())),
                cap: None,
                asks: vec![Need::Scope(By::Machine)],
                then: Ok(Action::Revive {
                    id: legacy.id,
                    scope: mine(),
                }),
            },
            // Row 2: a row holds the instance_id, live or removed.
            Case {
                what: "the row the install's id holds, in scope: revived, no cap read",
                request: used,
                found: Found {
                    by_instance: Some(held),
                    by_machine: None,
                },
                scope: Some(Scope::In(mine())),
                cap: None,
                asks: vec![Need::Scope(By::Instance)],
                then: Ok(Action::Revive {
                    id: held.id,
                    scope: mine(),
                }),
            },
            Case {
                what: "the row the install's id holds, removed, for a device manager",
                request: used,
                found: Found {
                    by_instance: Some(held),
                    by_machine: None,
                },
                scope: Some(Scope::In(removed_scope)),
                cap: None,
                asks: vec![Need::Scope(By::Instance)],
                then: Ok(Action::Revive {
                    id: held.id,
                    scope: removed_scope,
                }),
            },
            Case {
                what: "the row the install's id holds, someone else's: its own 409",
                request: used,
                found: Found {
                    by_instance: Some(held),
                    by_machine: None,
                },
                scope: Some(Scope::SomeoneElses),
                cap: None,
                asks: vec![Need::Scope(By::Instance)],
                then: Err(Refusal::SomeoneElsesInstance),
            },
            Case {
                what: "the row the install's id holds, removed and out of scope: 409",
                request: used,
                found: Found {
                    by_instance: Some(held),
                    by_machine: None,
                },
                scope: Some(Scope::Removed),
                cap: None,
                asks: vec![Need::Scope(By::Instance)],
                then: Err(Refusal::Removed),
            },
            Case {
                what: "the row the install's id holds wins over a legacy row",
                request: used,
                found: Found {
                    by_instance: Some(held),
                    by_machine: Some(legacy),
                },
                scope: Some(Scope::In(mine())),
                cap: None,
                asks: vec![Need::Scope(By::Instance)],
                then: Ok(Action::Revive {
                    id: held.id,
                    scope: mine(),
                }),
            },
            Case {
                what: "the legacy row read second holds this install's id: a revival",
                request: used,
                found: Found {
                    by_instance: None,
                    by_machine: Some(held),
                },
                scope: Some(Scope::In(mine())),
                cap: None,
                asks: vec![Need::Scope(By::Machine)],
                then: Ok(Action::Revive {
                    id: held.id,
                    scope: mine(),
                }),
            },
            // Row 3: none holds it; an unclaimed row with the machine_id, in scope.
            Case {
                what: "an unclaimed legacy row in scope: claimed, and a lost race makes a row",
                request: used,
                found: Found {
                    by_instance: None,
                    by_machine: Some(legacy),
                },
                scope: Some(Scope::In(mine())),
                cap: room.clone(),
                asks: vec![Need::Scope(By::Machine), Need::Cap],
                then: Ok(Action::Claim {
                    id: legacy.id,
                    instance_id: OWN,
                    scope: mine(),
                    if_taken: IfTaken::Create,
                }),
            },
            Case {
                what: "an unclaimed legacy row in scope at the cap: claimed (no slot), a lost race is 403",
                request: used,
                found: Found {
                    by_instance: None,
                    by_machine: Some(legacy),
                },
                scope: Some(Scope::In(mine())),
                cap: at_cap.clone(),
                asks: vec![Need::Scope(By::Machine), Need::Cap],
                then: Ok(Action::Claim {
                    id: legacy.id,
                    instance_id: OWN,
                    scope: mine(),
                    if_taken: IfTaken::Refuse(Refusal::Cap(FULL.to_string())),
                }),
            },
            Case {
                what: "an unclaimed removed legacy row, for a device manager: claimed",
                request: used,
                found: Found {
                    by_instance: None,
                    by_machine: Some(legacy),
                },
                scope: Some(Scope::In(removed_scope)),
                cap: room.clone(),
                asks: vec![Need::Scope(By::Machine), Need::Cap],
                then: Ok(Action::Claim {
                    id: legacy.id,
                    instance_id: OWN,
                    scope: removed_scope,
                    if_taken: IfTaken::Create,
                }),
            },
            // Row 4: that row is live and someone else's (D4).
            Case {
                what: "someone else's live legacy row: a row of its own",
                request: used,
                found: Found {
                    by_instance: None,
                    by_machine: Some(legacy),
                },
                scope: Some(Scope::SomeoneElses),
                cap: room.clone(),
                asks: vec![Need::Scope(By::Machine), Need::Cap],
                then: Ok(Action::Create {
                    instance_id: Some(OWN),
                }),
            },
            Case {
                what: "someone else's live legacy row at the cap: 403",
                request: used,
                found: Found {
                    by_instance: None,
                    by_machine: Some(legacy),
                },
                scope: Some(Scope::SomeoneElses),
                cap: at_cap.clone(),
                asks: vec![Need::Scope(By::Machine), Need::Cap],
                then: Err(Refusal::Cap(FULL.to_string())),
            },
            // Row 5: that row is removed and outside scope (D4).
            Case {
                what: "a removed legacy row out of scope: 409, no cap read",
                request: used,
                found: Found {
                    by_instance: None,
                    by_machine: Some(legacy),
                },
                scope: Some(Scope::Removed),
                cap: None,
                asks: vec![Need::Scope(By::Machine)],
                then: Err(Refusal::Removed),
            },
            // Row 6: that row holds another instance_id.
            Case {
                what: "a legacy row another install claimed: a row of its own, no scope read",
                request: used,
                found: Found {
                    by_instance: None,
                    by_machine: Some(claimed_by_other),
                },
                scope: None,
                cap: room.clone(),
                asks: vec![Need::Cap],
                then: Ok(Action::Create {
                    instance_id: Some(OWN),
                }),
            },
            Case {
                what: "a legacy row another install claimed, at the cap: 403 (a split)",
                request: used,
                found: Found {
                    by_instance: None,
                    by_machine: Some(claimed_by_other),
                },
                scope: None,
                cap: at_cap.clone(),
                asks: vec![Need::Cap],
                then: Err(Refusal::Cap(FULL.to_string())),
            },
            // Row 7: no row at all.
            Case {
                what: "no row at all: a row of its own",
                request: used,
                found: Found::default(),
                scope: None,
                cap: room.clone(),
                asks: vec![Need::Cap],
                then: Ok(Action::Create {
                    instance_id: Some(OWN),
                }),
            },
            Case {
                what: "no row at all, at the cap: 403",
                request: used,
                found: Found::default(),
                scope: None,
                cap: at_cap.clone(),
                asks: vec![Need::Cap],
                then: Err(Refusal::Cap(FULL.to_string())),
            },
        ];

        for case in cases {
            let (then, asked) = settle(case.request, &case.found, case.scope, case.cap);
            assert_eq!(then, case.then, "{}", case.what);
            assert_eq!(asked, case.asks, "what it read: {}", case.what);
        }
    }

    /// The 400 is decided from the request alone, and only with the switch
    /// on: off, any value is read as an older client's, as an older server
    /// reads it.
    #[test]
    fn an_instance_id_is_32_lowercase_hex_while_the_switch_is_on() {
        assert_eq!(read_instance_id(None, true), Ok(InstanceRead::Absent));
        assert_eq!(
            read_instance_id(Some(OWN), true),
            Ok(InstanceRead::Used(OWN))
        );
        for bad in [
            "",
            "0123456789ABCDEF0123456789abcdef",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789abcdef0123456789abcdeg",
            " 123456789abcdef0123456789abcdef",
            // 32 bytes, one of them not ASCII: the length is not the check.
            "\u{ff10}123456789abcdef0123456789abcd",
        ] {
            assert_eq!(
                read_instance_id(Some(bad), true),
                Err(MALFORMED_INSTANCE_ID),
                "{bad:?}"
            );
        }
        assert_eq!(read_instance_id(None, false), Ok(InstanceRead::Absent));
        assert_eq!(
            read_instance_id(Some(OWN), false),
            Ok(InstanceRead::Ignored)
        );
        assert_eq!(
            read_instance_id(Some("NOT AN ID"), false),
            Ok(InstanceRead::Ignored)
        );
    }

    /// `keyed_by` is said only to a request that carried an id, and its
    /// words are the wire's.
    #[test]
    fn keyed_by_answers_only_a_request_that_carried_an_id() {
        assert_eq!(keyed_by(InstanceRead::Absent), None);
        assert_eq!(keyed_by(InstanceRead::Ignored), Some(KeyedBy::MachineId));
        assert_eq!(keyed_by(InstanceRead::Used(OWN)), Some(KeyedBy::InstanceId));
        assert_eq!(
            serde_json::to_value(KeyedBy::InstanceId).unwrap(),
            "instance_id"
        );
        assert_eq!(
            serde_json::to_value(KeyedBy::MachineId).unwrap(),
            "machine_id"
        );
    }
}
