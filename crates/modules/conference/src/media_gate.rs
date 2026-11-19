// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Who may use the `media:*` namespace, and for which room.
//!
//! A connection may send a `media:*` command for a room only while it is a
//! participant of that room's call. Two proofs carry that, and every handler
//! in `ws_media` takes one of them instead of a bare room id, user and
//! connection — their fields are private to this module, so a handler cannot
//! be reached without the gate:
//!
//! - **[`JoinPass`]** — a `media:join`, admitted by [`admit_join`] on the
//!   checks REST `call/join` makes: the caller is a member of the ROOM's
//!   tenant (read from the room, never from the caller), the room's
//!   visibility lets them in, and they hold an open call session that
//!   `call/join` wrote for THIS connection (or with no connection id, which an
//!   older client writes). The socket path admits only someone the REST path
//!   admitted.
//! - **[`Seat`]** — every other command: this connection holds a participant
//!   entry in this room's media, as this user. Only an admitted join creates
//!   one. [`seat`] reads it on the pod that serves the room, so a command
//!   forwarded to the owner pod over `media.cmd` is checked where the
//!   participant lives.
//!
//! A refusal answers the same thing whatever its reason
//! ([`NOT_A_PARTICIPANT`]): it says neither whether the room exists, nor whose
//! it is, nor whether a call is running in it.

use std::fmt;

use bson::oid::ObjectId;

use crate::ConferenceState;

/// What a refused command hears, for every reason.
pub(crate) const NOT_A_PARTICIPANT: &str = "Not a participant of this call";

/// Proof that a `media:join` passed [`admit_join`].
#[derive(Debug)]
pub(crate) struct JoinPass {
    room_id: ObjectId,
    tenant_id: ObjectId,
    user_id: ObjectId,
    connection_id: String,
}

impl JoinPass {
    pub(crate) fn room_id(&self) -> ObjectId {
        self.room_id
    }

    /// The room's tenant, as the room records it.
    pub(crate) fn tenant_id(&self) -> ObjectId {
        self.tenant_id
    }

    pub(crate) fn user_id(&self) -> ObjectId {
        self.user_id
    }

    pub(crate) fn connection_id(&self) -> &str {
        &self.connection_id
    }
}

/// Proof that a connection holds a participant entry in a room's media.
#[derive(Debug)]
pub(crate) struct Seat {
    room_id: ObjectId,
    user_id: ObjectId,
    connection_id: String,
}

impl Seat {
    pub(crate) fn room_id(&self) -> ObjectId {
        self.room_id
    }

    pub(crate) fn user_id(&self) -> ObjectId {
        self.user_id
    }

    pub(crate) fn connection_id(&self) -> &str {
        &self.connection_id
    }
}

/// Why a join was refused. For the log only: the caller hears
/// [`NOT_A_PARTICIPANT`] whatever this says.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// No room has this id.
    NoRoom,
    /// The room guard `call/join` runs said no: not a member of the room's
    /// tenant, or the room's visibility keeps the caller out.
    Room(String),
    /// No open call session for this connection.
    NoCallSession,
    /// A lookup failed. A join is refused rather than admitted on a guess.
    Lookup(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRoom => f.write_str("no such room"),
            Self::Room(why) => write!(f, "room guard: {why}"),
            Self::NoCallSession => f.write_str("no open call session for this connection"),
            Self::Lookup(e) => write!(f, "lookup failed: {e}"),
        }
    }
}

/// Admit a `media:join` of `room_id` by `user_id` on `connection_id`.
pub(crate) async fn admit_join(
    state: &ConferenceState,
    user_id: ObjectId,
    connection_id: &str,
    room_id: ObjectId,
) -> Result<JoinPass, Refusal> {
    // The tenant comes from the room. The frame names only a room id, so
    // the room is read by id once, to learn whose it is; it is then resolved
    // again WITHIN that tenant, behind the caller's membership of it, by the
    // one room guard `call/join` runs. Nothing the caller says picks the
    // tenant, so `is_member` here answers for the room's own tenant.
    let tenant_id = match state.rooms.base.find_by_id(room_id).await {
        Ok(room) => room.tenant_id,
        Err(roomler_ai_services::dao::base::DaoError::NotFound) => return Err(Refusal::NoRoom),
        Err(e) => return Err(Refusal::Lookup(e.to_string())),
    };
    crate::guards::require_room_in_tenant(state, tenant_id, room_id, user_id)
        .await
        .map_err(|e| Refusal::Room(e.to_string()))?;
    match state
        .rooms
        .has_open_call_session(tenant_id, room_id, user_id, connection_id)
        .await
    {
        Ok(true) => Ok(JoinPass {
            room_id,
            tenant_id,
            user_id,
            connection_id: connection_id.to_string(),
        }),
        Ok(false) => Err(Refusal::NoCallSession),
        Err(e) => Err(Refusal::Lookup(e.to_string())),
    }
}

/// The seat `connection_id` holds in `room_id`'s media as `user_id`, if any.
/// Read on this pod's room manager: call it where the room is served.
pub(crate) fn seat(
    state: &ConferenceState,
    user_id: ObjectId,
    connection_id: &str,
    room_id: ObjectId,
) -> Option<Seat> {
    state
        .room_manager
        .holds_seat(&room_id, connection_id, &user_id)
        .then(|| Seat {
            room_id,
            user_id,
            connection_id: connection_id.to_string(),
        })
}
