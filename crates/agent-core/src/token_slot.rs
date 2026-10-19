// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-103 — the LIVE agent token of one enrollment.
//!
//! The daemon used to read `agent_token` once at start and copy the string
//! into everything that authenticates: the control socket, the DERP dialer,
//! the HTTP device list, the log and crash uploaders. A renewed token could
//! then reach none of them until a restart, and a copy that kept the old
//! token would fail only on that token's own expiry, months later.
//!
//! One slot per enrollment, shared by every consumer and read at the moment
//! of use, makes a renewal (or a token re-enrolled on disk) take effect on
//! the next connect, dial or request.
//!
//! #2512 — the slot also holds the server's verdict on its token, as the
//! control socket last heard it. A device whose row is gone had its control
//! socket climb the auth ladder (#2463), while its DERP dialer re-dialled
//! every 10 s and its log uploader posted every 5 s, each one a 401, forever.
//! Those loops now read this verdict rather than reach their own: while the
//! server refuses the token they hold their requests. A new token (a renewal,
//! or a token re-enrolled on disk), or the socket's next accepted upgrade,
//! lets them go again.
//!
//! ⚠️ The verdict is the control socket's alone. `/derp` answers 401 for ANY
//! lookup error (a Mongo blip included: it fails closed), so a dialer that
//! believed its own 401 would drop every device's DERP floor on one database
//! wobble. The control socket's upgrade refuses only on an authoritative
//! answer (no such row, a token from before the device's removal, a token
//! that fails verification) and lets a lookup error through.

use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// The current agent token of one enrollment, and whether the server refuses
/// it. Cloning shares the slot.
///
/// ⚠️ Secret: `Debug` redacts it, and nothing here logs it.
#[derive(Clone)]
pub struct TokenSlot(Arc<RwLock<Held>>);

struct Held {
    token: String,
    /// #2512 — the control socket's upgrade was answered 401 with exactly
    /// this token.
    refused: bool,
}

impl TokenSlot {
    pub fn new(token: impl Into<String>) -> Self {
        Self(Arc::new(RwLock::new(Held {
            token: token.into(),
            refused: false,
        })))
    }

    /// The token as of now. Read it at the moment of use, never once and
    /// kept: a kept copy is exactly what this type exists to remove.
    pub fn get(&self) -> String {
        self.read().token.clone()
    }

    /// Replace the token. Returns `false` when it was already this value.
    /// A different token clears the refusal: it has not been refused yet.
    pub fn set(&self, token: String) -> bool {
        let mut held = self.write();
        if held.token == token {
            return false;
        }
        held.token = token;
        held.refused = false;
        true
    }

    /// #2512 — the control socket's upgrade was refused (HTTP 401) with
    /// `token`, the value it read from this slot for that attempt. The slot
    /// is marked only while it still holds that token: one renewed since has
    /// not been refused. Returns whether this changed the verdict.
    pub fn mark_refused(&self, token: &str) -> bool {
        let mut held = self.write();
        if held.token != token || held.refused {
            return false;
        }
        held.refused = true;
        true
    }

    /// #2512 — the control socket's upgrade was accepted with `token`.
    /// Returns whether this lifted a refusal.
    pub fn mark_accepted(&self, token: &str) -> bool {
        let mut held = self.write();
        if held.token != token || !held.refused {
            return false;
        }
        held.refused = false;
        true
    }

    /// #2512 — does the server refuse the token this slot holds, as the
    /// control socket last heard it? A loop that would spend a request on
    /// that token (the DERP dialer, the log uploader) holds it instead.
    pub fn is_refused(&self) -> bool {
        self.read().refused
    }

    /// Do two handles share one slot?
    pub fn same_slot(&self, other: &TokenSlot) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    fn read(&self) -> RwLockReadGuard<'_, Held> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Held> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for TokenSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenSlot(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_clone_sees_a_new_token() {
        let slot = TokenSlot::new("old");
        let reader = slot.clone();
        assert!(slot.set("new".into()));
        assert_eq!(reader.get(), "new");
        assert!(!reader.set("new".into()), "the same value is not a change");
        assert!(reader.same_slot(&slot));
        assert!(!reader.same_slot(&TokenSlot::new("new")));
    }

    #[test]
    fn debug_never_prints_the_token() {
        let slot = TokenSlot::new("eyJ-super-secret");
        assert!(!format!("{slot:?}").contains("super-secret"));
    }

    /// #2512 — the refusal belongs to one token value, and every clone sees it.
    #[test]
    fn a_refusal_is_of_the_token_held_and_every_clone_sees_it() {
        let slot = TokenSlot::new("first");
        let reader = slot.clone();
        assert!(!reader.is_refused(), "a fresh slot is not refused");

        // A refusal of a token the slot no longer holds is stale: a renewal
        // landed while that attempt was in flight.
        assert!(!slot.mark_refused("older"));
        assert!(!reader.is_refused());

        assert!(slot.mark_refused("first"), "the verdict changed");
        assert!(reader.is_refused());
        assert!(!slot.mark_refused("first"), "already refused: no change");

        // An accept of a different token lifts nothing.
        assert!(!slot.mark_accepted("other"));
        assert!(reader.is_refused());
        assert!(slot.mark_accepted("first"));
        assert!(!reader.is_refused());
        assert!(!slot.mark_accepted("first"), "nothing left to lift");
    }

    /// #2512 — a new token (a renewal, a token re-enrolled on disk) lifts the
    /// refusal at once: it has not been refused, so the loops holding their
    /// requests may try it.
    #[test]
    fn a_new_token_lifts_the_refusal() {
        let slot = TokenSlot::new("refused");
        assert!(slot.mark_refused("refused"));
        assert!(
            !slot.set("refused".into()),
            "the same value is not a change"
        );
        assert!(slot.is_refused(), "and so lifts nothing");
        assert!(slot.set("renewed".into()));
        assert!(!slot.is_refused());
        assert_eq!(slot.get(), "renewed");
    }
}
