// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 P1e — the work a token rotation's commit starts
//! (`docs/fr/FR-155-one-row-new-credentials.md`, "The hook
//! `agent_credentials_rotated`").
//!
//! [`CommitSeam`] is where the integration cells hold that work, right after
//! the commit. Unheld, it costs one lock per commit.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use tokio::sync::watch;

/// The point right after a rotation's commit, where a cell can hold the work
/// the commit starts (`crates/tests/src/token_rotation_tests.rs`, T24 and
/// T25). The precedent is the remote module's `resume_gate_runs`.
#[doc(hidden)]
#[derive(Default)]
pub struct CommitSeam {
    /// While a cell holds the seam: the gate the work waits on until it opens.
    gate: Mutex<Option<watch::Receiver<bool>>>,
    /// How many works wait at the seam now.
    waiting: AtomicUsize,
}

/// A held seam. [`Self::release`] or a drop opens it.
#[doc(hidden)]
pub struct SeamHold(watch::Sender<bool>);

impl SeamHold {
    /// Open the seam: each work waiting at it goes on.
    pub fn release(self) {}
}

impl Drop for SeamHold {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

/// Counts one work waiting at the seam while it lives.
struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl CommitSeam {
    /// Hold the seam: the work of each commit on this pod waits at it until
    /// the hold is released or dropped.
    pub fn hold(&self) -> SeamHold {
        let (open, gate) = watch::channel(false);
        *self.gate.lock().unwrap_or_else(PoisonError::into_inner) = Some(gate);
        SeamHold(open)
    }

    /// How many works wait at the seam now.
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    /// The work's side: returns at once unless a cell holds the seam, and
    /// otherwise once the hold ends.
    pub(crate) async fn pass(&self) {
        let gate = self
            .gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(mut gate) = gate else {
            return;
        };
        let open = *gate.borrow();
        if open {
            return;
        }
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let _waiting = Waiting(&self.waiting);
        let _ = gate.wait_for(|open| *open).await;
    }
}
