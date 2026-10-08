// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P1c — who may read a session, and who may drive it, in one place:
//! the viewer's grant, the session routes and the room's stubs all ask here,
//! so none of them can drift from the others.
//!
//! | who | reads it | prompts it, answers its approvals | stops it, names its drivers |
//! |---|---|---|---|
//! | its owner | ✅ | ✅ while it is live | ✅ |
//! | a driver the owner named | ✅ | ✅ while it is live | — |
//! | another member of its room | ✅ | — | — |
//! | anyone else, in the org or not | — (a 404, like a bogus id) | — | — |
//!
//! ⚠️ A room member who is not a driver READS and TALKS — "message the room"
//! is ordinary chat — but nothing they write reaches the harness: a
//! multi-user room would otherwise be a prompt-injection channel with a seat
//! for every member (design §4.5).

use bson::oid::ObjectId;

use crate::HiveState;
use crate::model::AgentSession;

/// May `user` read `s`? In the org, and its owner or in the session's room —
/// chat's own membership rule. The owner reads it even out of its room: a
/// Secret room cannot be re-entered, and leaving it must not lock the owner
/// out of their own session. A session from before rooms (P0b/P0c) is its
/// owner's alone.
pub(crate) async fn may_read(state: &HiveState, s: &AgentSession, user: ObjectId) -> bool {
    if !matches!(state.tenants.is_member(s.tenant_id, user).await, Ok(true)) {
        return false;
    }
    if s.owner_id == user {
        return true;
    }
    match s.room_id {
        Some(room) => matches!(
            state.chat.is_member(s.tenant_id, room, user).await,
            Ok(true)
        ),
        None => false,
    }
}

/// May `user` — already known to READ `s` ([`may_read`]: a driver who left
/// the room or the org drives nothing) — prompt it and answer its approvals
/// NOW: a driver, and the session still live.
pub(crate) fn drives_now(s: &AgentSession, user: ObjectId) -> bool {
    s.drives(user) && !s.status.is_terminal()
}
