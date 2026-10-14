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

use std::sync::{Arc, RwLock};

/// The current agent token of one enrollment. Cloning shares the slot.
///
/// ⚠️ Secret: `Debug` redacts it, and nothing here logs it.
#[derive(Clone)]
pub struct TokenSlot(Arc<RwLock<String>>);

impl TokenSlot {
    pub fn new(token: impl Into<String>) -> Self {
        Self(Arc::new(RwLock::new(token.into())))
    }

    /// The token as of now. Read it at the moment of use, never once and
    /// kept: a kept copy is exactly what this type exists to remove.
    pub fn get(&self) -> String {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replace the token. Returns `false` when it was already this value.
    pub fn set(&self, token: String) -> bool {
        let mut current = self.0.write().unwrap_or_else(|e| e.into_inner());
        if *current == token {
            return false;
        }
        *current = token;
        true
    }

    /// Do two handles share one slot?
    pub fn same_slot(&self, other: &TokenSlot) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
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
}
