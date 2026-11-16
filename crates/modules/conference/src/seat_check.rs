// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The pod serving a call checks its own seats (#3839): a seat whose
//! admission is gone ends there, also when no word of the membership's end
//! reached that pod.
//!
//! A membership's end withdraws the person's seats through `revoke`, on the
//! pod that serves the call's media, asked over the bus when that is another
//! pod. When that pod cannot be asked (the request is lost or late, there is
//! no bus, the pod runs an older image during a roll), or the membership went
//! by a path that never told conference, nothing reaches the seat. So the pod
//! serving a call looks at its own seats, in a loop of its own, every
//! [`SEAT_CHECK_SECS`]:
//!
//! - One bounded read, from the primary, of what admitted each person seated
//!   here ([`read_admissions`]): the room's document (its org and its
//!   visibility), the person's membership of that org, and, for a room whose
//!   visibility keeps it to its members, their membership of the room. These
//!   are two of the three checks `media:join` passes
//!   (`media_gate::admit_join`), with the room guard's own visibility rule
//!   (`RoomVisibility::requires_membership`) and the rows its `is_member`
//!   reads. The third, an open call session for the connection, is not read:
//!   a person's other tab, or a call whose sessions a sweep closed, would lose
//!   a seat they are entitled to.
//! - A seat whose admission reads as gone in [`MISSES_TO_END`] complete reads
//!   in a row, a pass apart, ends the way a withdrawal that reached it ends
//!   (`revoke::withdraw_these_seats_here`): its connections hear
//!   `room:call_ended` with `reason: "not_a_member"`, everyone left hears
//!   `media:peer_left`. Then the call's books, as a hang-up's: the person's
//!   open call sessions close, and a call they were the last one in ends. Each
//!   step finds nothing to do where the pod that took the membership's end did
//!   it already, so nothing counts twice.
//! - A read that fails, breaks off, cannot be decoded or is late changes
//!   nothing; from the [`FAILED_READS_TO_WARN`]th in a row each one warns. A
//!   person found admitted starts again from zero, and so does one in a room
//!   the read found no document for: ending a deleted room's call is the
//!   reconcile's (`media_cluster::end_calls_of_rooms_gone`).
//!
//! **The bound.** On a pod whose reads succeed, a seat whose admission is gone
//! ends within two passes and their reads of the membership's end:
//! `2 × (SEAT_CHECK_SECS + READ_TIMEOUT)`, 30 s.
//!
//! **The switch.** [`SWITCH`] (`ROOMLER__CONFERENCE__SEAT_CHECK`) set to
//! `false`, `0`, `off` or `no` turns the check off on a pod; it is read once,
//! as the pod starts, and the check is on by default.
//!
//! **The log.** Every seat the check ends is one warning in one wording,
//! [`ENDED`], with the org, the room, the person, how many connections closed
//! and what each of the two reads found (`alert = seat_check_ended`).
//!
//! **Who holds a seat.** Every participant entry is made by
//! `RoomManager::create_transports`, whose one caller is
//! `ws_media::handle_media_join`, which takes a `JoinPass` that only
//! `media_gate::admit_join` mints. So every seat this check reads was admitted
//! by the checks it re-reads. A recording is no seat: it is uploaded by a
//! member over REST, and the room manager's RTP taps are kept apart from its
//! participants. ⚠️ A seat made some other way (a recorder, a bot, a pipe
//! between pods or workers: anything no membership admits by design) must be
//! skipped here, or the check ends it.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bson::{Bson, Document, doc, oid::ObjectId};
use mongodb::options::{ReadPreference, SelectionCriteria};
use roomler_ai_db::models::RoomVisibility;
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::ConferenceState;

/// How often the pod serving a call checks its seats: the room reconcile's
/// cadence.
pub const SEAT_CHECK_SECS: u64 = 10;

/// How many complete reads in a row must find a seat's admission gone before
/// the seat ends. Two, a pass apart: one read never ends a seat.
pub const MISSES_TO_END: u8 = 2;

/// How long one pass's read may take, all three queries together. A slower
/// read counts as failed.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// From which failed read in a row the check warns: a check that cannot
/// read is seen in the log, not only at debug.
const FAILED_READS_TO_WARN: u32 = 3;

/// The environment setting that turns the check off on a pod.
pub const SWITCH: &str = "ROOMLER__CONFERENCE__SEAT_CHECK";

/// The one wording of the warning for every seat the check ends.
pub const ENDED: &str = "seat check: ended a call seat whose admission is gone";

/// A person seated in a room's media on this pod: `(room, user)`. The check
/// counts by the person in the room, because the admission is the person's:
/// every connection of theirs in that call stands or falls with it.
pub type SeatKey = (ObjectId, ObjectId);

/// Why a complete read finds a seat's admission gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gone {
    /// No membership of the room's org.
    NotInOrg,
    /// The room's visibility keeps it to its members, and the person is not
    /// one.
    NotInRoom,
}

impl Gone {
    /// What the read found, for the log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInOrg => "not a member of the room's org",
            Self::NotInRoom => "not a member of a room only its members may enter",
        }
    }
}

/// What one complete read says about a person seated in a room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// The admission holds.
    Admitted,
    /// The admission is gone.
    Gone(Gone),
    /// The room has no document: no verdict on its seats.
    NoRoom,
}

/// What the check keeps between its passes on one pod.
#[derive(Debug, Default)]
pub struct SeatCheckMemory {
    /// Each seat key's misses in a row, with what the first of them found.
    misses: HashMap<SeatKey, (u8, Gone)>,
    /// How many reads failed in a row.
    failed_reads: u32,
}

/// A seat key the verdict ends, with what its first and its last read found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub key: SeatKey,
    pub first: Gone,
    pub last: Gone,
}

/// One read's verdict on the seats this pod serves. `read` holds a reading
/// for each of `seated`, or is `None` when the read failed: a failed read
/// changes nothing, not even what is forgotten. A seat key read as gone
/// counts one more miss and ends (is returned) on its [`MISSES_TO_END`]th in
/// a row. Anything else read for it (admitted, no room document, no reading
/// at all) starts it again from zero. A key no longer seated is forgotten.
pub fn seats_to_end(
    misses: &mut HashMap<SeatKey, (u8, Gone)>,
    seated: &[SeatKey],
    read: Option<&HashMap<SeatKey, Reading>>,
) -> Vec<Verdict> {
    let Some(read) = read else {
        return Vec::new();
    };
    let here: HashSet<&SeatKey> = seated.iter().collect();
    misses.retain(|key, _| here.contains(key));
    let mut ended = Vec::new();
    for key in seated {
        let Some(Reading::Gone(why)) = read.get(key) else {
            misses.remove(key);
            continue;
        };
        let entry = misses.entry(*key).or_insert((0, *why));
        entry.0 = entry.0.saturating_add(1);
        if entry.0 >= MISSES_TO_END {
            let first = entry.1;
            misses.remove(key);
            ended.push(Verdict {
                key: *key,
                first,
                last: *why,
            });
        }
    }
    ended
}

/// Whether the check runs, given [`SWITCH`]'s value: on unless it says
/// `false`, `0`, `off` or `no`.
pub fn switched_on(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("false" | "0" | "off" | "no")
    )
}

/// Start the check on this pod, in a loop of its own, unless [`SWITCH`]
/// turns it off. Called once, as the module's state is built.
pub(crate) fn spawn(state: &ConferenceState) {
    if !switched_on(std::env::var(SWITCH).ok().as_deref()) {
        info!(
            switch = SWITCH,
            "seat check: off on this pod; a seat no withdrawal reached keeps its call"
        );
        return;
    }
    let st = state.clone();
    tokio::spawn(async move {
        let mut memory = SeatCheckMemory::default();
        loop {
            tokio::time::sleep(Duration::from_secs(SEAT_CHECK_SECS)).await;
            check_seats_once(&st, &mut memory).await;
        }
    });
}

/// The seats this pod serves, each person in each room with their
/// connections, read into owned values: no guard of the room manager
/// outlives this call.
fn seats_here(state: &ConferenceState) -> HashMap<SeatKey, Vec<String>> {
    let mut seats: HashMap<SeatKey, Vec<String>> = HashMap::new();
    for room in state.room_manager.rooms_ref().iter() {
        for seat in room.participants.iter() {
            seats
                .entry((*room.key(), seat.value().user_id))
                .or_default()
                .push(seat.key().clone());
        }
    }
    seats
}

/// One pass of the check, with the memory its loop keeps. Public so tests
/// drive it directly. Returns the seat keys it ended.
pub async fn check_seats_once(
    state: &ConferenceState,
    memory: &mut SeatCheckMemory,
) -> Vec<SeatKey> {
    let seats = seats_here(state);
    let seated: Vec<SeatKey> = seats.keys().copied().collect();
    let read = if seated.is_empty() {
        Ok(Admissions::default())
    } else {
        read_admissions(state, &seated).await
    };
    match &read {
        Ok(_) => memory.failed_reads = 0,
        Err(e) => {
            memory.failed_reads = memory.failed_reads.saturating_add(1);
            if memory.failed_reads >= FAILED_READS_TO_WARN {
                warn!(
                    seats = seated.len(), failed_reads = memory.failed_reads, %e,
                    "seat check: the admissions read keeps failing; no seat ends here until it reads again"
                );
            } else {
                debug!(seats = seated.len(), %e, "seat check: the admissions read failed; nothing changes");
            }
        }
    }
    let verdicts = seats_to_end(
        &mut memory.misses,
        &seated,
        read.as_ref().ok().map(|a| &a.readings),
    );
    let mut ended = Vec::new();
    for verdict in verdicts {
        let (room_id, user_id) = verdict.key;
        let conns = seats.get(&verdict.key).cloned().unwrap_or_default();
        let closed =
            crate::revoke::withdraw_these_seats_here(state, room_id, user_id, &conns).await;
        if closed.is_empty() {
            // Closed meanwhile: a hang-up, or a withdrawal that came late.
            continue;
        }
        let tenant = read
            .as_ref()
            .ok()
            .and_then(|a| a.tenants.get(&room_id))
            .map(|tenant| tenant.to_hex())
            .unwrap_or_default();
        warn!(
            tenant = %tenant, room = %room_id, user = %user_id, seats = closed.len(),
            first_read = verdict.first.as_str(), second_read = verdict.last.as_str(),
            alert = "seat_check_ended", "{ENDED}"
        );
        // The call's books, as a hang-up keeps them: the person's open call
        // sessions close and the count drops (none left after a removal whose
        // hook ran), then the call is settled, so one they were the last one
        // in ends here, where its media is.
        if let Err(e) = crate::call::finalize_call_leave_db(state, room_id, user_id, None).await {
            warn!(room = %room_id, user = %user_id, %e, "seat check: closing the call sessions of an ended seat failed");
        }
        if let Err(e) = crate::call::settle_call_after_departure(state, room_id).await {
            warn!(room = %room_id, %e, "seat check: settling the call of an ended seat failed");
        }
        ended.push(verdict.key);
    }
    ended
}

/// What one complete read found: a reading for every seat key it was asked
/// about, and the org of every room it found.
#[derive(Debug, Default)]
struct Admissions {
    readings: HashMap<SeatKey, Reading>,
    tenants: HashMap<ObjectId, ObjectId>,
}

/// The part of a room's document an admission rests on, decoded with the
/// room model's own visibility type and default.
#[derive(Debug, Deserialize)]
struct RoomAdmission {
    #[serde(rename = "_id")]
    id: ObjectId,
    tenant_id: ObjectId,
    #[serde(default)]
    visibility: RoomVisibility,
}

/// Read what admits each of `seated`, within [`READ_TIMEOUT`]. Only a read
/// that completes counts: a query error, a cursor that breaks off, a
/// document that does not decode and a late answer are each an error, never
/// "not a member".
async fn read_admissions(
    state: &ConferenceState,
    seated: &[SeatKey],
) -> Result<Admissions, String> {
    tokio::time::timeout(READ_TIMEOUT, read_admissions_unbounded(state, seated))
        .await
        .map_err(|_| format!("no answer within {READ_TIMEOUT:?}"))?
}

/// Three queries by ids, each with a projection and on its existing index,
/// read from the primary: the rooms (`_id`), the org memberships
/// (`tenant_members` `{tenant_id, user_id}`) and, for the rooms only their
/// members may enter, the room memberships (`room_members`
/// `{room_id, user_id}`). A membership counts exactly as `is_member` counts
/// it: `{tenant_id, user_id}` for the org, `{tenant_id, room_id, user_id}`
/// for the room.
async fn read_admissions_unbounded(
    state: &ConferenceState,
    seated: &[SeatKey],
) -> Result<Admissions, String> {
    let room_ids: HashSet<ObjectId> = seated.iter().map(|(rid, _)| *rid).collect();
    let rooms: HashMap<ObjectId, RoomAdmission> = read_all(
        state,
        "rooms",
        doc! { "_id": { "$in": as_bson(room_ids) } },
        doc! { "_id": 1, "tenant_id": 1, "visibility": 1 },
    )
    .await?
    .into_iter()
    .map(|d| {
        bson::from_document::<RoomAdmission>(d)
            .map(|room| (room.id, room))
            .map_err(|e| format!("a room did not decode: {e}"))
    })
    .collect::<Result<_, _>>()?;

    let seated_in_rooms: Vec<&SeatKey> = seated
        .iter()
        .filter(|(rid, _)| rooms.contains_key(rid))
        .collect();
    let tenants: HashSet<ObjectId> = rooms.values().map(|room| room.tenant_id).collect();
    let users: HashSet<ObjectId> = seated_in_rooms.iter().map(|(_, uid)| *uid).collect();
    let in_org: HashSet<(ObjectId, ObjectId)> = if users.is_empty() {
        HashSet::new()
    } else {
        read_all(
            state,
            "tenant_members",
            doc! { "tenant_id": { "$in": as_bson(tenants) }, "user_id": { "$in": as_bson(users) } },
            doc! { "_id": 0, "tenant_id": 1, "user_id": 1 },
        )
        .await?
        .iter()
        .map(|d| Ok((oid(d, "tenant_id")?, oid(d, "user_id")?)))
        .collect::<Result<_, String>>()?
    };

    let members_only: Vec<&SeatKey> = seated_in_rooms
        .iter()
        .copied()
        .filter(|(rid, _)| rooms[rid].visibility.requires_membership())
        .collect();
    let in_room: HashSet<(ObjectId, ObjectId, ObjectId)> = if members_only.is_empty() {
        HashSet::new()
    } else {
        let rids: HashSet<ObjectId> = members_only.iter().map(|(rid, _)| *rid).collect();
        let uids: HashSet<ObjectId> = members_only.iter().map(|(_, uid)| *uid).collect();
        read_all(
            state,
            "room_members",
            doc! { "room_id": { "$in": as_bson(rids) }, "user_id": { "$in": as_bson(uids) } },
            doc! { "_id": 0, "tenant_id": 1, "room_id": 1, "user_id": 1 },
        )
        .await?
        .iter()
        .map(|d| Ok((oid(d, "tenant_id")?, oid(d, "room_id")?, oid(d, "user_id")?)))
        .collect::<Result<_, String>>()?
    };

    let readings = seated
        .iter()
        .map(|&(rid, uid)| {
            let reading = match rooms.get(&rid) {
                None => Reading::NoRoom,
                Some(room) if !in_org.contains(&(room.tenant_id, uid)) => {
                    Reading::Gone(Gone::NotInOrg)
                }
                Some(room)
                    if room.visibility.requires_membership()
                        && !in_room.contains(&(room.tenant_id, rid, uid)) =>
                {
                    Reading::Gone(Gone::NotInRoom)
                }
                Some(_) => Reading::Admitted,
            };
            ((rid, uid), reading)
        })
        .collect();
    let tenants = rooms
        .values()
        .map(|room| (room.id, room.tenant_id))
        .collect();
    Ok(Admissions { readings, tenants })
}

/// Every document `filter` matches in `collection`, projected, from the
/// primary. An error, or a cursor that breaks off, is an error.
async fn read_all(
    state: &ConferenceState,
    collection: &str,
    filter: Document,
    projection: Document,
) -> Result<Vec<Document>, String> {
    use futures::TryStreamExt;
    let cursor = state
        .db
        .collection::<Document>(collection)
        .find(filter)
        .projection(projection)
        .selection_criteria(SelectionCriteria::ReadPreference(ReadPreference::Primary))
        .await
        .map_err(|e| format!("{collection}: {e}"))?;
    cursor
        .try_collect()
        .await
        .map_err(|e| format!("{collection}: {e}"))
}

fn as_bson(ids: HashSet<ObjectId>) -> Vec<Bson> {
    ids.into_iter().map(Bson::ObjectId).collect()
}

fn oid(d: &Document, key: &str) -> Result<ObjectId, String> {
    d.get_object_id(key).map_err(|e| format!("{key}: {e}"))
}

/// What one pass's read finds for each of `seated`, without a verdict: the
/// read [`check_seats_once`] makes, bounded the same way. Public so a test
/// holds it against the room guard (`guards::require_room_in_tenant`) cell by
/// cell: wherever the guard admits someone, this must read
/// [`Reading::Admitted`].
pub async fn read_readings(
    state: &ConferenceState,
    seated: &[SeatKey],
) -> Result<HashMap<SeatKey, Reading>, String> {
    read_admissions(state, seated).await.map(|a| a.readings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SeatKey {
        (ObjectId::new(), ObjectId::new())
    }

    fn read_of(pairs: &[(SeatKey, Reading)]) -> HashMap<SeatKey, Reading> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn a_seat_ends_on_its_second_miss_in_a_row_and_not_before() {
        let mut misses = HashMap::new();
        let seat = key();
        let gone = read_of(&[(seat, Reading::Gone(Gone::NotInOrg))]);
        assert!(seats_to_end(&mut misses, &[seat], Some(&gone)).is_empty());
        let ended = seats_to_end(&mut misses, &[seat], Some(&gone));
        assert_eq!(
            ended,
            vec![Verdict {
                key: seat,
                first: Gone::NotInOrg,
                last: Gone::NotInOrg
            }]
        );
        assert!(misses.is_empty(), "an ended seat is forgotten");
    }

    #[test]
    fn an_admitted_seat_never_ends() {
        let mut misses = HashMap::new();
        let seat = key();
        let held = read_of(&[(seat, Reading::Admitted)]);
        for _ in 0..5 {
            assert!(seats_to_end(&mut misses, &[seat], Some(&held)).is_empty());
        }
        assert!(misses.is_empty());
    }

    #[test]
    fn a_failed_read_changes_nothing() {
        let mut misses = HashMap::new();
        let seat = key();
        let gone = read_of(&[(seat, Reading::Gone(Gone::NotInRoom))]);
        assert!(seats_to_end(&mut misses, &[seat], Some(&gone)).is_empty());
        // A failed (or late, or broken-off) read: no count, no reset, nothing
        // forgotten, even for a seat no longer here.
        for _ in 0..3 {
            assert!(seats_to_end(&mut misses, &[], None).is_empty());
        }
        assert_eq!(misses.get(&seat), Some(&(1, Gone::NotInRoom)));
        let ended = seats_to_end(&mut misses, &[seat], Some(&gone));
        assert_eq!(ended.len(), 1, "the second complete read ends it");
    }

    #[test]
    fn one_miss_then_found_starts_again_from_zero() {
        let mut misses = HashMap::new();
        let seat = key();
        let gone = read_of(&[(seat, Reading::Gone(Gone::NotInOrg))]);
        let held = read_of(&[(seat, Reading::Admitted)]);
        assert!(seats_to_end(&mut misses, &[seat], Some(&gone)).is_empty());
        assert!(seats_to_end(&mut misses, &[seat], Some(&held)).is_empty());
        assert!(misses.is_empty());
        assert!(
            seats_to_end(&mut misses, &[seat], Some(&gone)).is_empty(),
            "a miss after a found reading is a first miss again"
        );
        assert_eq!(seats_to_end(&mut misses, &[seat], Some(&gone)).len(), 1);
    }

    #[test]
    fn no_room_document_and_no_reading_give_no_verdict_and_start_again() {
        let mut misses = HashMap::new();
        let seat = key();
        let gone = read_of(&[(seat, Reading::Gone(Gone::NotInOrg))]);
        assert!(seats_to_end(&mut misses, &[seat], Some(&gone)).is_empty());
        let no_room = read_of(&[(seat, Reading::NoRoom)]);
        assert!(seats_to_end(&mut misses, &[seat], Some(&no_room)).is_empty());
        assert!(misses.is_empty(), "a room with no document resets the seat");
        assert!(seats_to_end(&mut misses, &[seat], Some(&gone)).is_empty());
        // A read that holds nothing for the seat (a partial answer) is doubt.
        let partial = read_of(&[]);
        assert!(seats_to_end(&mut misses, &[seat], Some(&partial)).is_empty());
        assert!(misses.is_empty());
    }

    #[test]
    fn only_the_seat_whose_admission_is_gone_ends() {
        let mut misses = HashMap::new();
        let person = ObjectId::new();
        let this_room = (ObjectId::new(), person);
        // The same person in another room, which may be another org's.
        let other_room = (ObjectId::new(), person);
        let someone_else = (this_room.0, ObjectId::new());
        let seated = [this_room, other_room, someone_else];
        let read = read_of(&[
            (this_room, Reading::Gone(Gone::NotInRoom)),
            (other_room, Reading::Admitted),
            (someone_else, Reading::Admitted),
        ]);
        assert!(seats_to_end(&mut misses, &seated, Some(&read)).is_empty());
        let ended = seats_to_end(&mut misses, &seated, Some(&read));
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].key, this_room);
        assert!(misses.is_empty());
    }

    #[test]
    fn a_seat_no_longer_here_is_forgotten() {
        let mut misses = HashMap::new();
        let seat = key();
        let gone = read_of(&[(seat, Reading::Gone(Gone::NotInOrg))]);
        assert!(seats_to_end(&mut misses, &[seat], Some(&gone)).is_empty());
        // It hung up; a complete read of the seats left forgets it.
        assert!(seats_to_end(&mut misses, &[], Some(&HashMap::new())).is_empty());
        assert!(misses.is_empty());
        // Back in the call with the admission still gone: a first miss again.
        assert!(seats_to_end(&mut misses, &[seat], Some(&gone)).is_empty());
    }

    #[test]
    fn the_first_and_the_last_reading_are_both_reported() {
        let mut misses = HashMap::new();
        let seat = key();
        let room_gone = read_of(&[(seat, Reading::Gone(Gone::NotInRoom))]);
        let org_gone = read_of(&[(seat, Reading::Gone(Gone::NotInOrg))]);
        assert!(seats_to_end(&mut misses, &[seat], Some(&room_gone)).is_empty());
        let ended = seats_to_end(&mut misses, &[seat], Some(&org_gone));
        assert_eq!(
            ended,
            vec![Verdict {
                key: seat,
                first: Gone::NotInRoom,
                last: Gone::NotInOrg
            }]
        );
    }

    #[test]
    fn the_switch_turns_the_check_off_only_when_it_says_so() {
        assert!(switched_on(None));
        assert!(switched_on(Some("")));
        assert!(switched_on(Some("true")));
        assert!(switched_on(Some("1")));
        for off in ["false", "0", "off", "no", " FALSE ", "Off"] {
            assert!(!switched_on(Some(off)), "{off:?}");
        }
    }

    #[test]
    fn a_room_document_decodes_with_the_models_visibility_default() {
        let id = ObjectId::new();
        let tenant = ObjectId::new();
        let legacy: RoomAdmission =
            bson::from_document(doc! { "_id": id, "tenant_id": tenant }).unwrap();
        assert_eq!(legacy.visibility, RoomVisibility::Public);
        assert!(!legacy.visibility.requires_membership());
        let private: RoomAdmission =
            bson::from_document(doc! { "_id": id, "tenant_id": tenant, "visibility": "private" })
                .unwrap();
        assert!(private.visibility.requires_membership());
        assert!(
            bson::from_document::<RoomAdmission>(
                doc! { "_id": id, "tenant_id": tenant, "visibility": "unheard-of" }
            )
            .is_err(),
            "a visibility the model does not know fails the read, never reads as public"
        );
    }
}
