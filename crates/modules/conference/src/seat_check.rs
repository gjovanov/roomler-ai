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
//!   in a row, a pass apart, is a verdict, and the room guard `media:join`
//!   runs decides it: asked about the person, on the primary, within
//!   [`READ_TIMEOUT`], only its refusal ends the seat, and an admission
//!   ([`GUARD_ADMITS`]), a failure or no answer keeps it. The reads pick the
//!   seats to ask about; the guard's own answer decides. A seat ends the way
//!   a withdrawal that reached it ends (`revoke::withdraw_these_seats_here`),
//!   for each connection still holding the seat the pass read (the same
//!   person, the same send transport, which every join makes anew), so a
//!   rejoin since stays: the connection hears `room:call_ended` with
//!   `reason: "not_a_member"`, everyone left hears `media:peer_left`. Then the
//!   call's books, as a hang-up's: the person's open call sessions close, and
//!   the call is settled once (by `call::finalize_call_leave_db` when it
//!   closed their last session, else here), so a call they were the last one
//!   in ends. Where the pod that took an org membership's end settled the
//!   call already, this pod's settle repeats the same count.
//! - A read that fails, breaks off or is late changes nothing; from the
//!   [`FAILED_READS_TO_WARN`]th in a row each one warns. A document that does
//!   not decode fails no read: the seats it bears on get no verdict
//!   ([`Reading::Unreadable`]), every other seat is read as before, and it
//!   warns ([`ODD_DOCUMENT`]), once an hour at most while it stays odd. A
//!   person found admitted starts again from zero, and so does one in a room
//!   the read found no document for, or one a document that did not decode
//!   bears on: ending a deleted room's call is the reconcile's
//!   (`media_cluster::end_calls_of_rooms_gone`).
//!
//! **The bound.** On a pod whose reads succeed, a seat whose admission is gone
//! ends within `2 × (SEAT_CHECK_SECS + 2 × READ_TIMEOUT)`, 40 s, of the
//! membership's end: the read under way then, two sleeps, the two reads that
//! miss and one ask of the room guard, each at its limit. Asks about other
//! seats in the same passes add up to [`READ_TIMEOUT`] to each of the two
//! passes before the deciding one. A guard that does not answer keeps the
//! seat for two more passes.
//!
//! **The switch.** [`SWITCH`] (`ROOMLER__CONFERENCE__SEAT_CHECK`) is read
//! from the environment only, once, as the pod starts ([`spawn`]). `false`,
//! `0`, `off` or `no`, in any case, turns the check off on that pod; unset,
//! empty, `true`, `1`, `on` or `yes` leaves it on; any other value leaves it
//! on and warns ([`SWITCH_UNKNOWN`]), so a typo never turns it off unseen.
//!
//! **The log.** Each line has one wording and an `alert` field, a handle for
//! a person reading the pod's log (nothing files or pages on them):
//!
//! - [`ENDED`] (`alert = seat_check_ended`): every seat the check ends, with
//!   the org, the room, the person, how many connections closed, what each of
//!   the two reads found and the room guard's refusal (`guard`);
//! - [`GUARD_ADMITS`] (`alert = seat_check_guard_admits`): a verdict the room
//!   guard overruled, with what each of the two reads found;
//! - [`ODD_DOCUMENT`] (`alert = seat_check_odd_document`): a document a read
//!   could not decode, with its collection, its `_id` and its fields;
//! - [`SWITCH_UNKNOWN`] (`alert = seat_check_switch_unknown`): a value of
//!   [`SWITCH`] the check does not know, once, as the pod starts.
//!
//! A healthy pod logs none of the last three. A read that keeps failing warns
//! from its [`FAILED_READS_TO_WARN`]th in a row, and so does a room guard that
//! keeps not answering; the pod's start logs `seat check: on this pod`, or
//! `seat check: off on this pod`, at info.
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
use std::time::{Duration, Instant};

use bson::{Bson, Document, doc, oid::ObjectId};
use mediasoup::prelude::{Transport as _, TransportId};
use mongodb::options::{ReadPreference, SelectionCriteria};
use roomler_ai_db::models::RoomVisibility;
use roomler_ai_services::dao::{room::RoomDao, tenant::TenantDao};
use roomler_core::ApiError;
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

/// The one wording of the warning for a value of [`SWITCH`] that is neither
/// an on value nor an off value: the check runs.
pub const SWITCH_UNKNOWN: &str =
    "seat check: the switch has a value it does not know; the check runs on this pod";

/// The one wording of the warning for every seat the check ends.
pub const ENDED: &str = "seat check: ended a call seat whose admission is gone";

/// The one wording of the warning for a document a read could not decode.
pub const ODD_DOCUMENT: &str =
    "seat check: a document did not decode; the seats it bears on get no verdict";

/// How long the check keeps quiet about a document that stays odd, once it
/// warned about it: the document's next warning comes no sooner.
pub const ODD_DOCUMENT_WARN_EVERY: Duration = Duration::from_secs(3600);

/// The one wording of the warning for a verdict the room guard overrules:
/// the check's reads found the seat's admission gone, and the guard admits
/// the person.
pub const GUARD_ADMITS: &str =
    "seat check: the room guard admits a seat its reads found gone; the seat stays";

/// How many of one pass's room-guard reads run at once.
const GUARD_READS_AT_ONCE: usize = 8;

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
    /// A document the admission may rest on did not decode: the room's, or a
    /// membership row whose ids could name this seat. No verdict on it.
    Unreadable,
}

/// What the check keeps between its passes on one pod.
#[derive(Debug, Default)]
pub struct SeatCheckMemory {
    /// Each seat key's misses in a row, with what the first of them found.
    misses: HashMap<SeatKey, (u8, Gone)>,
    /// How many reads failed in a row.
    failed_reads: u32,
    /// The documents that did not decode, by collection and `_id`, with when
    /// the check last warned about each.
    odd_warned: HashMap<(&'static str, String), Instant>,
    /// How many room-guard reads in a row did not answer.
    guard_failures: u32,
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
/// a row. Anything else read for it (admitted, no room document, a document
/// that did not decode, no reading at all) starts it again from zero. A key
/// no longer seated is forgotten.
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

/// What a value of [`SWITCH`] says, case and the space around it aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    /// Unset, empty, `true`, `1`, `on` or `yes`: the check runs.
    On,
    /// `false`, `0`, `off` or `no`: the check does not run on this pod.
    Off,
    /// Any other value: the check runs, and its start warns
    /// ([`SWITCH_UNKNOWN`]).
    Unknown,
}

/// What `value`, the switch's value, says.
pub fn switch_of(value: Option<&str>) -> Switch {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None | Some("" | "true" | "1" | "on" | "yes") => Switch::On,
        Some("false" | "0" | "off" | "no") => Switch::Off,
        Some(_) => Switch::Unknown,
    }
}

/// Whether the check runs, given [`SWITCH`]'s value: on unless it says
/// `false`, `0`, `off` or `no`.
pub fn switched_on(value: Option<&str>) -> bool {
    switch_of(value) != Switch::Off
}

/// Start the check on this pod as [`SWITCH`] says. The switch is read from
/// the environment only (no config file sets it), once, here, as the
/// module's state is built.
pub(crate) fn spawn(state: &ConferenceState) {
    let value = std::env::var_os(SWITCH).map(|v| v.to_string_lossy().into_owned());
    start(state, value.as_deref());
}

/// Start the check's loop on this pod as `value`, the switch's value, says:
/// no loop for an off value, a loop for an on value, and a loop with a
/// warning for a value it does not know. Public so a test drives it with
/// each value. Returns the loop's task, if it started one.
pub fn start(state: &ConferenceState, value: Option<&str>) -> Option<tokio::task::JoinHandle<()>> {
    match switch_of(value) {
        Switch::Off => {
            info!(
                switch = SWITCH,
                "seat check: off on this pod; a seat no withdrawal reached keeps its call"
            );
            return None;
        }
        Switch::Unknown => warn!(
            switch = SWITCH,
            value = value.unwrap_or_default(),
            alert = "seat_check_switch_unknown",
            "{SWITCH_UNKNOWN}"
        ),
        Switch::On => {}
    }
    let st = state.clone();
    info!(every_secs = SEAT_CHECK_SECS, "seat check: on this pod");
    Some(tokio::spawn(async move {
        let mut memory = SeatCheckMemory::default();
        loop {
            tokio::time::sleep(Duration::from_secs(SEAT_CHECK_SECS)).await;
            check_seats_once(&st, &mut memory).await;
        }
    }))
}

/// A connection seated on this pod, with the seat it held when the pass
/// looked: its send transport's id, which every join makes anew, so a rejoin
/// on the same connection is another seat.
type HeldSeat = (String, TransportId);

/// The seats this pod serves, each person in each room with their
/// connections and the seat each holds, read into owned values: no guard of
/// the room manager outlives this call.
fn seats_here(state: &ConferenceState) -> HashMap<SeatKey, Vec<HeldSeat>> {
    let mut seats: HashMap<SeatKey, Vec<HeldSeat>> = HashMap::new();
    for room in state.room_manager.rooms_ref().iter() {
        for seat in room.participants.iter() {
            seats
                .entry((*room.key(), seat.value().user_id))
                .or_default()
                .push((seat.key().clone(), seat.value().send_transport.id()));
        }
    }
    seats
}

/// One pass of the check, with the memory its loop keeps: the read half
/// ([`read_seats`]), then the end half ([`end_seats`]). Public so tests drive
/// it directly. Returns the seat keys it ended.
pub async fn check_seats_once(
    state: &ConferenceState,
    memory: &mut SeatCheckMemory,
) -> Vec<SeatKey> {
    let read = read_seats(state, memory).await;
    end_seats(state, memory, &read).await
}

/// What the read half of a pass found, for its end half: the seats this pod
/// served when the pass looked, the verdicts the read gave them, and the org
/// of each room the read found.
#[derive(Debug, Default)]
pub struct SeatRead {
    /// The seat keys whose admission the read found gone for the
    /// [`MISSES_TO_END`]th time in a row: what the end half ends.
    pub verdicts: Vec<Verdict>,
    /// Why the read failed, when it did: then there is no verdict at all.
    pub failed: Option<String>,
    /// Each person in each room, with the connections the pass saw them on
    /// and the seat each held.
    seats: HashMap<SeatKey, Vec<HeldSeat>>,
    /// The org of each room the read found a document for.
    tenants: HashMap<ObjectId, ObjectId>,
}

/// The read half of a pass: the seats this pod serves, one bounded read of
/// what admits them, and the verdicts that read gives with what `memory`
/// holds from the passes before. Public so a test can act between the read
/// and the end of a pass.
pub async fn read_seats(state: &ConferenceState, memory: &mut SeatCheckMemory) -> SeatRead {
    let seats = seats_here(state);
    let seated: Vec<SeatKey> = seats.keys().copied().collect();
    let read = if seated.is_empty() {
        Ok(Admissions::default())
    } else {
        read_admissions(state, &seated).await
    };
    match &read {
        Ok(admissions) => {
            memory.failed_reads = 0;
            warn_odd_documents(&mut memory.odd_warned, &admissions.odd, Instant::now());
        }
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
    let (tenants, failed) = match read {
        Ok(admissions) => (admissions.tenants, None),
        Err(e) => (HashMap::new(), Some(e)),
    };
    SeatRead {
        verdicts,
        failed,
        seats,
        tenants,
    }
}

/// The end half of a pass. The room guard `media:join` runs is asked about
/// the person of each verdict in `read` ([`ask_the_room_guard`]), and only
/// its refusal ends the seat, as `revoke::withdraw_these_seats_here` ends it;
/// then the call's books are kept. A guard that admits the person, fails or
/// does not answer keeps the seat. `memory` is the loop's, the one the read
/// half keeps. Returns the seat keys it ended.
pub async fn end_seats(
    state: &ConferenceState,
    memory: &mut SeatCheckMemory,
    read: &SeatRead,
) -> Vec<SeatKey> {
    if read.verdicts.is_empty() {
        return Vec::new();
    }
    let words = ask_the_room_guard(state, read).await;
    let mut ended = Vec::new();
    for (verdict, word) in read.verdicts.iter().zip(words) {
        let (room_id, user_id) = verdict.key;
        let tenant = read
            .tenants
            .get(&room_id)
            .map(|tenant| tenant.to_hex())
            .unwrap_or_default();
        let refusal = match word {
            GuardWord::Refuses(refusal) => {
                memory.guard_failures = 0;
                refusal
            }
            GuardWord::Admits => {
                memory.guard_failures = 0;
                warn!(
                    tenant = %tenant, room = %room_id, user = %user_id,
                    first_read = verdict.first.as_str(), second_read = verdict.last.as_str(),
                    alert = "seat_check_guard_admits", "{GUARD_ADMITS}"
                );
                continue;
            }
            GuardWord::NoAnswer(e) => {
                memory.guard_failures = memory.guard_failures.saturating_add(1);
                if memory.guard_failures >= FAILED_READS_TO_WARN {
                    warn!(
                        tenant = %tenant, room = %room_id, user = %user_id,
                        guard_failures = memory.guard_failures, %e,
                        "seat check: the room guard keeps failing to answer; the seat stays"
                    );
                } else {
                    debug!(
                        tenant = %tenant, room = %room_id, user = %user_id, %e,
                        "seat check: the room guard did not answer; the seat stays"
                    );
                }
                continue;
            }
        };
        let seats = read.seats.get(&verdict.key).cloned().unwrap_or_default();
        let closed =
            crate::revoke::withdraw_these_seats_here(state, room_id, user_id, &seats).await;
        if closed.is_empty() {
            // Nothing the pass read is left: closed meanwhile (a hang-up, a
            // withdrawal that came late), or taken again since (a rejoin),
            // and a seat taken since is the next passes' to judge.
            continue;
        }
        warn!(
            tenant = %tenant, room = %room_id, user = %user_id, seats = closed.len(),
            first_read = verdict.first.as_str(), second_read = verdict.last.as_str(),
            guard = refusal, alert = "seat_check_ended", "{ENDED}"
        );
        // The call's books, as a hang-up keeps them: the person's open call
        // sessions close and the count drops, and the call is settled once,
        // by finalize when it closed their last session, else here (none was
        // left open after a removal whose hook ran, or finalize failed), so a
        // call they were the last one in ends here, where its media is.
        let settled = match crate::call::finalize_call_leave_db(state, room_id, user_id, None).await
        {
            Ok(settled) => settled,
            Err(e) => {
                warn!(room = %room_id, user = %user_id, %e, "seat check: closing the call sessions of an ended seat failed");
                false
            }
        };
        if !settled && let Err(e) = crate::call::settle_call_after_departure(state, room_id).await {
            warn!(room = %room_id, %e, "seat check: settling the call of an ended seat failed");
        }
        ended.push(verdict.key);
    }
    ended
}

/// What the room guard said about a verdict's seat.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GuardWord {
    /// It refuses the person, by the refusal named: the verdict stands.
    Refuses(&'static str),
    /// It admits the person: the seat stays.
    Admits,
    /// It failed, or did not answer within the pass's deadline: the seat
    /// stays.
    NoAnswer(String),
}

/// The room guard's answer as a word. Only a refusal, matched by its
/// variant, ends a seat: `NotAMember` (not in the room's org), `Forbidden`
/// (a private room the person is not in) or `NotFound` (no live room in the
/// org, or a secret room the person is not in). Any other error, a failed
/// read or a document that does not decode among them, is no answer.
fn guard_word<T>(answer: Result<T, ApiError>) -> GuardWord {
    match answer {
        Ok(_) => GuardWord::Admits,
        Err(ApiError::NotAMember) => GuardWord::Refuses("not_a_member"),
        Err(ApiError::Forbidden(_)) => GuardWord::Refuses("forbidden"),
        Err(ApiError::NotFound(_)) => GuardWord::Refuses("not_found"),
        Err(e) => GuardWord::NoAnswer(e.to_string()),
    }
}

/// Ask the room guard about the person of each verdict in `read`, in the
/// org the read found for the room: the function `media:join`'s gate runs
/// (`roomler_ai_mod_chat::guards::require_room_in_tenant_with`), over DAOs on
/// a handle that reads the primary, as the check's own reads do, so a lagging
/// secondary cannot answer for it. At most [`GUARD_READS_AT_ONCE`] run at a
/// time, all within one [`READ_TIMEOUT`]: a read still out at the deadline is
/// no answer. The words come back in the verdicts' order.
async fn ask_the_room_guard(state: &ConferenceState, read: &SeatRead) -> Vec<GuardWord> {
    use futures::StreamExt;
    let db = on_the_primary(&state.db);
    let (tenants, rooms) = (TenantDao::new(&db), RoomDao::new(&db));
    let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
    // The asks are made first, from a named function, so no closure that
    // makes futures sits in the stream the pass awaits.
    let asks: Vec<_> = read
        .verdicts
        .iter()
        .map(|verdict| {
            let (room_id, user_id) = verdict.key;
            let tenant_id = read.tenants.get(&room_id).copied();
            ask_once(&tenants, &rooms, tenant_id, room_id, user_id, deadline)
        })
        .collect();
    futures::stream::iter(asks)
        .buffered(GUARD_READS_AT_ONCE)
        .collect()
        .await
}

/// One ask of [`ask_the_room_guard`], answered by `deadline`.
async fn ask_once(
    tenants: &TenantDao,
    rooms: &RoomDao,
    tenant_id: Option<ObjectId>,
    room_id: ObjectId,
    user_id: ObjectId,
    deadline: tokio::time::Instant,
) -> GuardWord {
    let Some(tenant_id) = tenant_id else {
        return GuardWord::NoAnswer("the read found no org for the room".to_string());
    };
    let asked = roomler_ai_mod_chat::guards::require_room_in_tenant_with(
        tenants, rooms, tenant_id, room_id, user_id,
    );
    match tokio::time::timeout_at(deadline, asked).await {
        Ok(answer) => guard_word(answer),
        Err(_) => GuardWord::NoAnswer(format!("no answer within {READ_TIMEOUT:?}")),
    }
}

/// `db`, reading the primary whatever the connection string prefers.
fn on_the_primary(db: &mongodb::Database) -> mongodb::Database {
    db.client().database_with_options(
        db.name(),
        mongodb::options::DatabaseOptions::builder()
            .selection_criteria(SelectionCriteria::ReadPreference(ReadPreference::Primary))
            .build(),
    )
}

/// Warn about each document of `odd` the check has not warned about within
/// [`ODD_DOCUMENT_WARN_EVERY`], and remember when; one it warned about lately
/// goes to debug. Returns how many it warned about.
fn warn_odd_documents(
    warned: &mut HashMap<(&'static str, String), Instant>,
    odd: &[OddDocument],
    now: Instant,
) -> usize {
    warned.retain(|_, at| now.saturating_duration_since(*at) < ODD_DOCUMENT_WARN_EVERY);
    let mut warnings = 0;
    for doc in odd {
        let seen = (doc.collection, doc.id.clone());
        if warned.contains_key(&seen) {
            debug!(
                collection = doc.collection, id = %doc.id, why = %doc.why,
                "seat check: a document still does not decode"
            );
            continue;
        }
        warned.insert(seen, now);
        warnings += 1;
        warn!(
            collection = doc.collection, id = %doc.id, fields = %doc.fields, why = %doc.why,
            alert = "seat_check_odd_document", "{ODD_DOCUMENT}"
        );
    }
    warnings
}

/// What one complete read found: a reading for every seat key it was asked
/// about, the org of every room it decoded, and every document it could not
/// decode.
#[derive(Debug, Default)]
struct Admissions {
    readings: HashMap<SeatKey, Reading>,
    tenants: HashMap<ObjectId, ObjectId>,
    odd: Vec<OddDocument>,
}

/// A document a read returned and could not decode, for the warning.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OddDocument {
    /// The collection it is in.
    collection: &'static str,
    /// Its `_id`, as stored.
    id: String,
    /// The fields the read projected, as stored, cut at 300 characters.
    fields: String,
    /// Why it did not decode.
    why: String,
}

impl OddDocument {
    fn of(collection: &'static str, doc: &Document, why: String) -> Self {
        Self {
            collection,
            id: doc
                .get("_id")
                .map_or_else(|| "none".to_string(), ToString::to_string),
            fields: doc.to_string().chars().take(300).collect(),
            why,
        }
    }
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

/// The rooms one read found: each room whose document decoded, and the ids
/// of those whose document did not.
#[derive(Debug, Default)]
struct RoomsRead {
    found: HashMap<ObjectId, RoomAdmission>,
    odd: HashSet<ObjectId>,
    odd_docs: Vec<OddDocument>,
}

impl RoomsRead {
    /// Decode each room document on its own. One that does not decode is
    /// kept by its `_id`, which always decodes, because the read matched it
    /// by `_id`; a document without one fails the read.
    fn decode(docs: &[Document]) -> Result<Self, String> {
        let mut rooms = Self::default();
        for doc in docs {
            match bson::from_document::<RoomAdmission>(doc.clone()) {
                Ok(room) => {
                    rooms.found.insert(room.id, room);
                }
                Err(e) => {
                    let id = oid(doc, "_id").map_err(|e| format!("a room without its id: {e}"))?;
                    rooms.odd.insert(id);
                    rooms
                        .odd_docs
                        .push(OddDocument::of("rooms", doc, e.to_string()));
                }
            }
        }
        Ok(rooms)
    }
}

/// A membership row that did not decode, by the ids it could name. Each id
/// field holds its ObjectIds (the value itself, or an array's elements), or
/// `None` when it holds none, and then it could name any id.
#[derive(Debug, Clone, Default)]
struct OddRow {
    tenants: Option<HashSet<ObjectId>>,
    rooms: Option<HashSet<ObjectId>>,
    users: Option<HashSet<ObjectId>>,
}

impl OddRow {
    fn of(row: &Document) -> Self {
        Self {
            tenants: ids_in(row, "tenant_id"),
            rooms: ids_in(row, "room_id"),
            users: ids_in(row, "user_id"),
        }
    }

    /// As an org row: whether it could admit `user` to the org `tenant`. The
    /// room guard counts org rows without decoding them
    /// (`TenantDao::is_member`), so a row whose `tenant_id` is an array
    /// admits each org it holds.
    fn could_name_org(&self, tenant: ObjectId, user: ObjectId) -> bool {
        could_name(self.tenants.as_ref(), tenant) && could_name(self.users.as_ref(), user)
    }

    /// As a room row: whether it could admit `user` to `room`, of any org.
    fn could_name_room(&self, room: ObjectId, user: ObjectId) -> bool {
        could_name(self.rooms.as_ref(), room) && could_name(self.users.as_ref(), user)
    }
}

fn could_name(ids: Option<&HashSet<ObjectId>>, id: ObjectId) -> bool {
    ids.is_none_or(|ids| ids.contains(&id))
}

/// The ObjectIds `key` holds in `row`: the value itself, or an array's
/// elements. `None` when it holds none.
fn ids_in(row: &Document, key: &str) -> Option<HashSet<ObjectId>> {
    let ids: HashSet<ObjectId> = match row.get(key) {
        Some(Bson::ObjectId(id)) => HashSet::from([*id]),
        Some(Bson::Array(items)) => items.iter().filter_map(Bson::as_object_id).collect(),
        _ => HashSet::new(),
    };
    (!ids.is_empty()).then_some(ids)
}

/// Decode each membership row of `collection` on its own: the ids of each
/// row whose `keys` each hold one ObjectId, and each other row by the ids it
/// could name, with the document for the warning in `odd`.
fn decode_rows<const N: usize>(
    rows: &[Document],
    collection: &'static str,
    keys: [&str; N],
    odd: &mut Vec<OddDocument>,
) -> (HashSet<[ObjectId; N]>, Vec<OddRow>) {
    let mut found = HashSet::new();
    let mut odd_rows = Vec::new();
    for row in rows {
        match ids_of(row, keys) {
            Ok(ids) => {
                found.insert(ids);
            }
            Err(why) => {
                odd_rows.push(OddRow::of(row));
                odd.push(OddDocument::of(collection, row, why));
            }
        }
    }
    (found, odd_rows)
}

/// The values of `keys` in `row`, each one ObjectId, or why one is not.
fn ids_of<const N: usize>(row: &Document, keys: [&str; N]) -> Result<[ObjectId; N], String> {
    let mut ids = [ObjectId::from_bytes([0; 12]); N];
    for (slot, key) in ids.iter_mut().zip(keys) {
        *slot = oid(row, key)?;
    }
    Ok(ids)
}

/// The org rows' filter: the orgs of the rooms the read decoded, and the
/// people seated in them. `None` when nobody is.
fn org_rows_filter(seated: &[SeatKey], rooms: &RoomsRead) -> Option<Document> {
    let users: HashSet<ObjectId> = seated
        .iter()
        .filter(|(rid, _)| rooms.found.contains_key(rid))
        .map(|(_, uid)| *uid)
        .collect();
    if users.is_empty() {
        return None;
    }
    let tenants: HashSet<ObjectId> = rooms.found.values().map(|room| room.tenant_id).collect();
    Some(doc! { "tenant_id": { "$in": as_bson(tenants) }, "user_id": { "$in": as_bson(users) } })
}

/// The room rows' filter, for the rooms only their members may enter: their
/// orgs, the rooms and the people seated in them. These are the fields
/// `RoomDao::is_member` names, so a row of no org, or of another org, which
/// the room guard never reads, is not read here either. `None` when nobody
/// is seated in such a room.
fn room_rows_filter(seated: &[SeatKey], rooms: &RoomsRead) -> Option<Document> {
    let (mut tids, mut rids, mut uids) = (HashSet::new(), HashSet::new(), HashSet::new());
    for (rid, uid) in seated {
        if let Some(room) = rooms
            .found
            .get(rid)
            .filter(|room| room.visibility.requires_membership())
        {
            tids.insert(room.tenant_id);
            rids.insert(*rid);
            uids.insert(*uid);
        }
    }
    if rids.is_empty() {
        return None;
    }
    Some(doc! {
        "tenant_id": { "$in": as_bson(tids) },
        "room_id": { "$in": as_bson(rids) },
        "user_id": { "$in": as_bson(uids) },
    })
}

/// The readings of `seated` from the documents one complete read returned:
/// the rooms, and the org and room rows its queries matched. A seat that a
/// document which did not decode bears on gets no verdict
/// ([`Reading::Unreadable`]): its room's document, an org row that could name
/// the room's org and the person, or a room row that could name the room and
/// the person. Every other seat reads as it would with those documents gone.
fn readings_of(
    seated: &[SeatKey],
    rooms: RoomsRead,
    org_rows: &[Document],
    room_rows: &[Document],
) -> Admissions {
    let mut odd = rooms.odd_docs;
    let (in_org, odd_org) = decode_rows(
        org_rows,
        "tenant_members",
        ["tenant_id", "user_id"],
        &mut odd,
    );
    let (in_room, odd_room) = decode_rows(
        room_rows,
        "room_members",
        ["tenant_id", "room_id", "user_id"],
        &mut odd,
    );
    let readings = seated
        .iter()
        .map(|&(rid, uid)| {
            let reading = if rooms.odd.contains(&rid) {
                Reading::Unreadable
            } else if let Some(room) = rooms.found.get(&rid) {
                let tid = room.tenant_id;
                let doubt = odd_org.iter().any(|row| row.could_name_org(tid, uid))
                    || odd_room.iter().any(|row| row.could_name_room(rid, uid));
                if doubt {
                    Reading::Unreadable
                } else if !in_org.contains(&[tid, uid]) {
                    Reading::Gone(Gone::NotInOrg)
                } else if room.visibility.requires_membership()
                    && !in_room.contains(&[tid, rid, uid])
                {
                    Reading::Gone(Gone::NotInRoom)
                } else {
                    Reading::Admitted
                }
            } else {
                Reading::NoRoom
            };
            ((rid, uid), reading)
        })
        .collect();
    let tenants = rooms
        .found
        .values()
        .map(|room| (room.id, room.tenant_id))
        .collect();
    Admissions {
        readings,
        tenants,
        odd,
    }
}

/// Read what admits each of `seated`, within [`READ_TIMEOUT`]. Only a read
/// that completes counts: a query error, a cursor that breaks off and a late
/// answer are each an error, never "not a member". A document that does not
/// decode fails no read: it costs only the seats it bears on their verdict
/// ([`readings_of`]).
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
/// `{tenant_id, room_id, user_id}`, the fields `RoomDao::is_member` names).
/// A membership counts exactly as `is_member` counts it: `{tenant_id,
/// user_id}` for the org, `{tenant_id, room_id, user_id}` for the room. A
/// room whose document does not decode stays out of the membership reads.
async fn read_admissions_unbounded(
    state: &ConferenceState,
    seated: &[SeatKey],
) -> Result<Admissions, String> {
    let room_ids: HashSet<ObjectId> = seated.iter().map(|(rid, _)| *rid).collect();
    let room_docs = read_all(
        state,
        "rooms",
        doc! { "_id": { "$in": as_bson(room_ids) } },
        doc! { "_id": 1, "tenant_id": 1, "visibility": 1 },
    )
    .await?;
    let rooms = RoomsRead::decode(&room_docs)?;
    let org_rows = match org_rows_filter(seated, &rooms) {
        Some(filter) => {
            let projection = doc! { "_id": 1, "tenant_id": 1, "user_id": 1 };
            read_all(state, "tenant_members", filter, projection).await?
        }
        None => Vec::new(),
    };
    let room_rows = match room_rows_filter(seated, &rooms) {
        Some(filter) => {
            let projection = doc! { "_id": 1, "tenant_id": 1, "room_id": 1, "user_id": 1 };
            read_all(state, "room_members", filter, projection).await?
        }
        None => Vec::new(),
    };
    Ok(readings_of(seated, rooms, &org_rows, &room_rows))
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
    fn the_switch_knows_its_on_and_off_values_and_no_others() {
        for on in [
            None,
            Some(""),
            Some("true"),
            Some("1"),
            Some("on"),
            Some(" YES "),
        ] {
            assert_eq!(switch_of(on), Switch::On, "{on:?}");
        }
        for off in ["false", "0", "off", "no", " OFF ", "No"] {
            assert_eq!(switch_of(Some(off)), Switch::Off, "{off:?}");
        }
        // A typo, another word for off, a number, a value that was not
        // Unicode (read lossily): the check stays on, and its start warns.
        for unknown in ["of", "disabled", "2", "\u{fffd}"] {
            assert_eq!(switch_of(Some(unknown)), Switch::Unknown, "{unknown:?}");
            assert!(
                switched_on(Some(unknown)),
                "{unknown:?} leaves the check on"
            );
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
            "a visibility the model does not know does not decode: that room's seats get no \
             verdict, and it never reads as public"
        );
    }

    /// U1's world: one org with two public rooms and two members-only ones,
    /// another org's members-only room, and four people, each seat with a
    /// reading of its own, among them both kinds of gone.
    struct World {
        org: ObjectId,
        other_org: ObjectId,
        p1: ObjectId,
        q1: ObjectId,
        q2: ObjectId,
        z1: ObjectId,
        a: ObjectId,
        b: ObjectId,
        c: ObjectId,
        d: ObjectId,
        seated: Vec<SeatKey>,
        rooms: Vec<Document>,
        org_rows: Vec<Document>,
        room_rows: Vec<Document>,
    }

    fn room_doc(id: ObjectId, tenant: ObjectId, visibility: &str) -> Document {
        doc! { "_id": id, "tenant_id": tenant, "visibility": visibility }
    }

    fn org_row(tenant: impl Into<Bson>, user: impl Into<Bson>) -> Document {
        doc! { "_id": ObjectId::new(), "tenant_id": tenant.into(), "user_id": user.into() }
    }

    fn room_row(tenant: impl Into<Bson>, room: impl Into<Bson>, user: impl Into<Bson>) -> Document {
        doc! {
            "_id": ObjectId::new(),
            "tenant_id": tenant.into(),
            "room_id": room.into(),
            "user_id": user.into(),
        }
    }

    impl World {
        fn new() -> Self {
            let (org, other_org) = (ObjectId::new(), ObjectId::new());
            let [p1, p2, q1, q2, z1] = [(); 5].map(|()| ObjectId::new());
            let [a, b, c, d] = [(); 4].map(|()| ObjectId::new());
            Self {
                org,
                other_org,
                p1,
                q1,
                q2,
                z1,
                a,
                b,
                c,
                d,
                seated: vec![
                    (p1, a),
                    (p1, b),
                    (p2, c),
                    (q1, a),
                    (q1, b),
                    (q2, c),
                    (q2, d),
                    (z1, a),
                ],
                rooms: vec![
                    room_doc(p1, org, "public"),
                    room_doc(p2, org, "public"),
                    room_doc(q1, org, "private"),
                    room_doc(q2, org, "secret"),
                    room_doc(z1, other_org, "private"),
                ],
                // d is in neither org.
                org_rows: vec![
                    org_row(org, a),
                    org_row(org, b),
                    org_row(org, c),
                    org_row(other_org, a),
                ],
                // b has no row in q1; a row of another org for b there counts
                // for nothing, as `RoomDao::is_member` reads it.
                room_rows: vec![
                    room_row(org, q1, a),
                    room_row(other_org, q1, b),
                    room_row(org, q2, c),
                    room_row(other_org, z1, a),
                ],
            }
        }

        fn readings(&self) -> Admissions {
            let rooms = RoomsRead::decode(&self.rooms).expect("every room has its id");
            readings_of(&self.seated, rooms, &self.org_rows, &self.room_rows)
        }
    }

    /// `with_odd` gives exactly the seats of `bears_on` no verdict, and every
    /// other seat the reading `base` gives it.
    fn assert_costs_only(
        base: &HashMap<SeatKey, Reading>,
        with_odd: &HashMap<SeatKey, Reading>,
        bears_on: &[SeatKey],
        what: &str,
    ) {
        assert_eq!(
            base.len(),
            with_odd.len(),
            "{what}: a reading for every seat"
        );
        for (key, reading) in with_odd {
            if bears_on.contains(key) {
                assert_eq!(*reading, Reading::Unreadable, "{what}: {key:?}");
            } else {
                assert_eq!(Some(reading), base.get(key), "{what}: {key:?}");
            }
        }
    }

    #[test]
    fn the_worlds_readings_are_the_ones_master_gives() {
        let w = World::new();
        let read = w.readings();
        assert!(read.odd.is_empty());
        assert_eq!(read.readings.len(), w.seated.len());
        let gone_room = Reading::Gone(Gone::NotInRoom);
        let gone_org = Reading::Gone(Gone::NotInOrg);
        for (key, want) in [
            ((w.p1, w.a), Reading::Admitted),
            ((w.p1, w.b), Reading::Admitted),
            (w.seated[2], Reading::Admitted),
            ((w.q1, w.a), Reading::Admitted),
            ((w.q1, w.b), gone_room),
            ((w.q2, w.c), Reading::Admitted),
            ((w.q2, w.d), gone_org),
            ((w.z1, w.a), Reading::Admitted),
        ] {
            assert_eq!(read.readings.get(&key), Some(&want), "{key:?}");
        }
        assert_eq!(read.tenants.get(&w.z1), Some(&w.other_org));
    }

    #[test]
    fn a_room_document_that_does_not_decode_costs_only_its_own_seats_their_verdict() {
        let mut w = World::new();
        let base = w.readings().readings;
        let (o, tenant) = (ObjectId::new(), w.org);
        w.seated.extend([(o, w.a), (o, w.d)]);
        let mut with_room = base.clone();
        with_room.extend([((o, w.a), Reading::NoRoom), ((o, w.d), Reading::NoRoom)]);
        for (odd, what) in [
            (room_doc(o, tenant, "unheard-of"), "an unknown visibility"),
            (doc! { "_id": o, "visibility": "private" }, "no org"),
            (doc! { "_id": o, "tenant_id": "x" }, "an org that is no id"),
        ] {
            let mut rooms = w.rooms.clone();
            rooms.push(odd);
            let read = readings_of(
                &w.seated,
                RoomsRead::decode(&rooms).unwrap(),
                &w.org_rows,
                &w.room_rows,
            );
            assert_costs_only(&with_room, &read.readings, &[(o, w.a), (o, w.d)], what);
            assert_eq!(read.odd.len(), 1, "{what}");
            assert_eq!(read.odd[0].collection, "rooms", "{what}");
            assert_eq!(read.odd[0].id, Bson::ObjectId(o).to_string(), "{what}");
            assert!(!read.tenants.contains_key(&o), "{what}: no org is read");
        }
        // A room document without an id cannot be put to a seat: the read
        // fails, which changes nothing.
        let mut rooms = w.rooms.clone();
        rooms.push(doc! { "tenant_id": tenant, "visibility": "private" });
        assert!(RoomsRead::decode(&rooms).is_err());
    }

    #[test]
    fn an_org_row_that_does_not_decode_costs_only_the_seats_it_could_admit() {
        let w = World::new();
        let base = w.readings().readings;
        let org_rooms = |user: ObjectId| -> Vec<SeatKey> {
            w.seated
                .iter()
                .copied()
                .filter(|&(rid, uid)| uid == user && rid != w.z1)
                .collect()
        };
        for (odd, bears_on, what) in [
            // d's org row, its `tenant_id` an array that holds the org: the
            // guard counts it and admits d.
            (
                org_row(Bson::Array(vec![Bson::ObjectId(w.org)]), w.d),
                org_rooms(w.d),
                "an array of orgs",
            ),
            (
                org_row(w.org, Bson::Array(vec![w.b.into(), w.d.into()])),
                [org_rooms(w.b), org_rooms(w.d)].concat(),
                "an array of people",
            ),
            // Another org's array names no seat in this org's rooms.
            (
                org_row(Bson::Array(vec![w.other_org.into()]), w.d),
                vec![],
                "an array of another org",
            ),
            // A `user_id` that holds no id could be anyone's.
            (
                org_row(w.other_org, "someone"),
                vec![(w.z1, w.a)],
                "a person that is no id",
            ),
        ] {
            let mut org_rows = w.org_rows.clone();
            org_rows.push(odd);
            let read = readings_of(
                &w.seated,
                RoomsRead::decode(&w.rooms).unwrap(),
                &org_rows,
                &w.room_rows,
            );
            assert_costs_only(&base, &read.readings, &bears_on, what);
            assert_eq!(read.odd.len(), 1, "{what}");
            assert_eq!(read.odd[0].collection, "tenant_members", "{what}");
        }
    }

    #[test]
    fn a_room_row_that_does_not_decode_costs_only_the_seats_it_could_admit() {
        let w = World::new();
        let base = w.readings().readings;
        for (odd, bears_on, what) in [
            // b's row in q1, its `room_id` an array that holds q1.
            (
                room_row(w.org, Bson::Array(vec![w.q1.into()]), w.b),
                vec![(w.q1, w.b)],
                "an array of rooms",
            ),
            // A person that is no id could be anyone in q2.
            (
                room_row(w.org, w.q2, "someone"),
                vec![(w.q2, w.c), (w.q2, w.d)],
                "a person that is no id",
            ),
            // A row with no org, which the filter keeps out (below), would
            // cost only the seat it names, whatever org it is of.
            (
                doc! { "_id": ObjectId::new(), "room_id": w.q1, "user_id": w.b },
                vec![(w.q1, w.b)],
                "no org",
            ),
        ] {
            let mut room_rows = w.room_rows.clone();
            room_rows.push(odd);
            let read = readings_of(
                &w.seated,
                RoomsRead::decode(&w.rooms).unwrap(),
                &w.org_rows,
                &room_rows,
            );
            assert_costs_only(&base, &read.readings, &bears_on, what);
            assert_eq!(read.odd.len(), 1, "{what}");
            assert_eq!(read.odd[0].collection, "room_members", "{what}");
        }
    }

    #[test]
    fn the_room_rows_are_read_by_the_rooms_orgs_as_the_room_guard_reads_them() {
        let w = World::new();
        let rooms = RoomsRead::decode(&w.rooms).unwrap();
        let ids = |filter: &Document, key: &str| -> HashSet<ObjectId> {
            filter
                .get_document(key)
                .and_then(|clause| clause.get_array("$in"))
                .unwrap_or_else(|e| panic!("{key}: {e}"))
                .iter()
                .filter_map(Bson::as_object_id)
                .collect()
        };
        let filter = room_rows_filter(&w.seated, &rooms).expect("members-only rooms are seated");
        assert_eq!(
            ids(&filter, "tenant_id"),
            HashSet::from([w.org, w.other_org])
        );
        assert_eq!(ids(&filter, "room_id"), HashSet::from([w.q1, w.q2, w.z1]));
        assert_eq!(ids(&filter, "user_id"), HashSet::from([w.a, w.b, w.c, w.d]));
        let filter = org_rows_filter(&w.seated, &rooms).expect("people are seated");
        assert_eq!(
            ids(&filter, "tenant_id"),
            HashSet::from([w.org, w.other_org])
        );
        // Seats in public rooms only: no room rows are read.
        let public = [(w.p1, w.a)];
        assert!(room_rows_filter(&public, &rooms).is_none());
    }

    #[test]
    fn only_a_refusal_of_the_room_guard_ends_a_seat() {
        assert_eq!(guard_word(Ok(())), GuardWord::Admits);
        for (refusal, word) in [
            (ApiError::NotAMember, "not_a_member"),
            (
                ApiError::Forbidden("Not a member of this room".into()),
                "forbidden",
            ),
            (ApiError::NotFound("Resource not found".into()), "not_found"),
        ] {
            assert_eq!(guard_word::<()>(Err(refusal)), GuardWord::Refuses(word));
        }
        // A failed read, a document that does not decode, and every other
        // error: no answer, so the seat stays.
        for failure in [
            ApiError::Internal("Kind: Server selection timeout".into()),
            ApiError::ServiceUnavailable("unavailable".into()),
            ApiError::BadRequest("bad".into()),
            ApiError::Unauthorized("no".into()),
            ApiError::Conflict("conflict".into()),
            ApiError::Validation("invalid".into()),
        ] {
            assert!(
                matches!(guard_word::<()>(Err(failure)), GuardWord::NoAnswer(_)),
                "a failure is no answer"
            );
        }
    }

    #[test]
    fn a_document_that_stays_odd_warns_once_an_hour_at_most() {
        let odd = |collection: &'static str, id: &str| OddDocument {
            collection,
            id: id.to_string(),
            fields: String::new(),
            why: "does not decode".to_string(),
        };
        let mut warned = HashMap::new();
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        assert_eq!(
            warn_odd_documents(&mut warned, &[odd("rooms", "1")], at(0)),
            1
        );
        for pass in 1..=6 {
            assert_eq!(
                warn_odd_documents(&mut warned, &[odd("rooms", "1")], at(10 * pass)),
                0,
                "pass {pass}"
            );
        }
        assert_eq!(
            warn_odd_documents(
                &mut warned,
                &[
                    odd("rooms", "1"),
                    odd("room_members", "1"),
                    odd("rooms", "2")
                ],
                at(70),
            ),
            2,
            "each other document warns on its own"
        );
        assert_eq!(
            warn_odd_documents(&mut warned, &[odd("rooms", "1")], at(3599)),
            0
        );
        assert_eq!(
            warn_odd_documents(&mut warned, &[odd("rooms", "1")], at(3600)),
            1,
            "an hour after it warned, it warns again"
        );
    }
}
