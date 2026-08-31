// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-45 P4, daemon side — the seam between the input arbiter and the portal
//! helper's stdin.
//!
//! While a portal capture with granted input is live, injected events belong
//! to the portal session, not to the OS injector: on the hosts this backend
//! exists for, enigo's XTest reaches nothing and uinput's events have no
//! reader (FR-45 field log). [`super::backend::PortalCapture`] registers a
//! sender here when its helper announces input; the arbiter's single
//! injection funnel consults [`try_route`] per event.
//!
//! ## Why a per-event check and not a backend choice
//!
//! The arbiter creates ONE process-wide injector, lazily, at the first
//! injected event — and the portal capture opens around the same moment, from
//! another task. Deciding "portal or OS" once at injector creation would race
//! that startup and freeze the loser in place for the life of the process.
//! A per-event lookup costs a mutex and makes the answer always current:
//! capture restarts, session ends, kill switch — the route follows.
//!
//! ## Registration is generation-checked
//!
//! Two captures can overlap during a reopen (new helper up before the old
//! one's `Drop` ran). `register` hands back a generation token and
//! `unregister` is a no-op unless the token is current — so a stale `Drop`
//! can never tear down its successor's route.

use std::sync::Mutex;
use std::sync::mpsc::{SyncSender, TrySendError};

use crate::input::InputMsg;

struct Route {
    tx: SyncSender<InputMsg>,
    generation: u64,
}

static ROUTE: Mutex<Option<Route>> = Mutex::new(None);
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Install the live portal input sender. Returns the generation token
/// [`unregister`] needs.
pub fn register(tx: SyncSender<InputMsg>) -> u64 {
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let mut route = ROUTE.lock().unwrap_or_else(|e| e.into_inner());
    *route = Some(Route { tx, generation });
    generation
}

/// Remove the route installed by the matching [`register`]. A stale token —
/// a `Drop` racing its replacement — leaves the current route standing.
pub fn unregister(generation: u64) {
    let mut route = ROUTE.lock().unwrap_or_else(|e| e.into_inner());
    if route.as_ref().is_some_and(|r| r.generation == generation) {
        *route = None;
    }
}

/// Offer one event to the portal route.
///
/// `true` means the portal OWNS this event — including when the channel was
/// full and the event was dropped, because falling through to the OS injector
/// on overload would split one input stream across two backends mid-gesture.
/// `false` means no live route: the caller injects normally.
pub fn try_route(msg: &InputMsg) -> bool {
    let route = ROUTE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(r) = route.as_ref() else {
        return false;
    };
    match r.tx.try_send(msg.clone()) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            tracing::debug!("portal input route full — event dropped");
            true
        }
        // The helper is gone but Drop has not run yet. Claim the event anyway
        // — the session this input belongs to is the portal one, and it is
        // ending.
        Err(TrySendError::Disconnected(_)) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::InputMsg;

    fn mv() -> InputMsg {
        InputMsg::MouseMove {
            x: 0.5,
            y: 0.5,
            mon: 0,
        }
    }

    /// ⚠️ One test drives all the registry states: the registry is
    /// process-global and cargo runs tests in this binary concurrently, so
    /// splitting these into separate `#[test]`s would make them race each
    /// other's registrations.
    #[test]
    fn route_lifecycle() {
        // No route: the caller keeps the event.
        assert!(!try_route(&mv()));

        // Registered: events flow.
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let generation = register(tx);
        assert!(try_route(&mv()));
        assert!(matches!(rx.try_recv(), Ok(InputMsg::MouseMove { .. })));

        // Full: still OWNED (dropped), never split across backends.
        assert!(try_route(&mv()));
        assert!(try_route(&mv()));
        assert!(try_route(&mv()), "full channel still claims the event");

        // A STALE unregister must not tear down a successor's route.
        let (tx2, rx2) = std::sync::mpsc::sync_channel(2);
        let generation2 = register(tx2);
        unregister(generation); // the old capture's Drop, racing
        assert!(try_route(&mv()), "successor route survives a stale Drop");
        assert!(matches!(rx2.try_recv(), Ok(InputMsg::MouseMove { .. })));

        // The CURRENT unregister clears it.
        unregister(generation2);
        assert!(!try_route(&mv()));
    }
}
