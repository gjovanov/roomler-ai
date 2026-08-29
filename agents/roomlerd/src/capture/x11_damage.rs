// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-28 P1 — "did anything change?" for the X11 capture path.
//!
//! `scrap` on Linux is XShm, and `Capturer::frame()` performs a full-screen
//! `GetImage` on every call. Unlike Windows DXGI Desktop Duplication — which
//! answers `WouldBlock` when the desktop is unchanged — XShm has no
//! "nothing changed" signal at all, so an idle desktop pays the full readback
//! at the target frame rate forever. Measured on a 1080p Fedora Asahi host:
//! ~20 ms per frame and ~50 % of a CPU core to transmit a static screen.
//!
//! The pump already knows what to do with "no frame": `peer.rs` logs
//! `capture produced no frame (idle screen)` when `next_frame` yields `None`.
//! This module exists purely to make that existing path *reachable* on Linux
//! by answering the question XShm cannot.
//!
//! It runs on its own X11 connection (scrap owns its own and does not expose
//! it) and is created on the capture worker thread, which is where the whole
//! backend is already pinned for XShm thread-affinity reasons.

use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{ConnectionExt as _, Damage as DamageId, ReportLevel};
use x11rb::rust_connection::RustConnection;

/// Force a capture at least this often even when the server reports no
/// damage. This bound is what makes P1 safe to default on: the failure mode
/// of a missed/coalesced damage event is a FROZEN stream, which is far worse
/// than a slow one, so a damage bug degrades to a stale tile that repairs
/// itself within a second rather than a dead session.
const DEFAULT_MAX_SKIP: Duration = Duration::from_millis(1000);

/// Tracks whether the X11 root window changed since the last delivered frame.
///
/// Construction is fail-open by design — every error path yields `None` and
/// the caller keeps today's unconditional-capture behaviour. A capture backend
/// that refuses to produce frames because an *optimisation* could not
/// initialise would be a far worse defect than the cost it saves.
pub struct DamageTracker {
    conn: RustConnection,
    damage: DamageId,
    /// When we last actually let a capture through, for the safety valve.
    last_capture: Instant,
    max_skip: Duration,
    /// Damage seen but not yet consumed by a capture. Kept across calls
    /// because the caller may ask more than once per delivered frame.
    pending: bool,
}

impl DamageTracker {
    /// Open a tracker, or `None` if damage tracking is unavailable or
    /// switched off. Never returns an error: the caller's fallback is the
    /// pre-FR-28 behaviour, which is always correct, only slower.
    pub fn open() -> Option<Self> {
        // Kill switch. "0"/"false" restores the pre-FR-28 path byte-for-byte.
        match std::env::var("ROOMLER_AGENT_X11_DAMAGE") {
            Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") => {
                tracing::info!(
                    "capture: X11 damage tracking disabled by ROOMLER_AGENT_X11_DAMAGE — every tick will do a full readback"
                );
                return None;
            }
            _ => {}
        }

        let max_skip = std::env::var("ROOMLER_AGENT_X11_DAMAGE_MAX_SKIP_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_MAX_SKIP);

        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| tracing::info!(%e, "capture: no X11 connection for damage tracking"))
            .ok()?;
        let root = conn.setup().roots.get(screen_num)?.root;

        // The extension must be present AND negotiated before any damage
        // request is legal.
        conn.damage_query_version(1, 1)
            .and_then(|c| c.reply().map_err(Into::into))
            .map_err(|e| tracing::info!(%e, "capture: X server has no DAMAGE extension"))
            .ok()?;

        let damage = conn.generate_id().ok()?;
        // NON_EMPTY: one event when the region goes empty -> non-empty, and
        // then silence until we subtract. That is exactly "has anything
        // changed since I last looked", with no per-rectangle event storm.
        conn.damage_create(damage, root, ReportLevel::NON_EMPTY)
            .and_then(|c| c.check().map_err(Into::into))
            .map_err(|e| tracing::info!(%e, "capture: could not watch the root window for damage"))
            .ok()?;
        let _ = conn.flush();

        tracing::info!(
            max_skip_ms = max_skip.as_millis() as u64,
            "capture: X11 damage tracking active — an unchanged screen now skips the XShm readback"
        );
        Some(Self {
            conn,
            damage,
            // Start due, so the very first tick always captures.
            last_capture: Instant::now() - max_skip,
            max_skip,
            pending: false,
        })
    }

    /// Should the caller perform a real capture this tick?
    ///
    /// `false` means "provably nothing changed and the safety valve is not
    /// due" — the caller should report no frame and skip the readback.
    pub fn should_capture(&mut self) -> bool {
        self.drain_events();

        if self.last_capture.elapsed() >= self.max_skip {
            // Safety valve. Re-arm as if we had consumed damage so a genuinely
            // idle screen keeps ticking at the valve rate rather than every frame.
            self.consume();
            return true;
        }
        if self.pending {
            self.consume();
            return true;
        }
        false
    }

    /// Absorb any queued events, latching whether the root changed.
    ///
    /// A connection error latches `pending` rather than clearing it: if we
    /// can no longer tell what changed, the safe answer is "assume it did".
    fn drain_events(&mut self) {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::DamageNotify(_))) => self.pending = true,
                Ok(Some(_)) => {}
                Ok(None) => return,
                Err(e) => {
                    tracing::warn!(
                        %e,
                        "capture: X11 damage connection failed — assuming the screen changed from here on"
                    );
                    self.pending = true;
                    return;
                }
            }
        }
    }

    /// Clear the server-side region and re-arm the NON_EMPTY report.
    ///
    /// ⚠️ ORDER IS LOAD-BEARING: the caller subtracts BEFORE grabbing, never
    /// after. Damage that lands *during* a readback then raises a fresh event
    /// and is picked up on the next tick — at worst one redundant capture.
    /// Subtracting after the grab would instead discard exactly that damage,
    /// and the screen would silently stop updating until the safety valve
    /// fired. Slow is recoverable; frozen is the bug this must not create.
    fn consume(&mut self) {
        self.pending = false;
        self.last_capture = Instant::now();
        if let Ok(cookie) = self
            .conn
            .damage_subtract(self.damage, x11rb::NONE, x11rb::NONE)
        {
            let _ = cookie.check();
        }
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kill switch must win even where an X server is present, because it
    /// is the operator's only lever if damage tracking misbehaves in the field.
    #[test]
    fn kill_switch_refuses_to_open() {
        // SAFETY: single-threaded test process; no other thread reads env here.
        unsafe { std::env::set_var("ROOMLER_AGENT_X11_DAMAGE", "0") };
        assert!(DamageTracker::open().is_none());
        unsafe { std::env::remove_var("ROOMLER_AGENT_X11_DAMAGE") };
    }

    /// `open()` must never panic and never propagate an error, on any host —
    /// including CI containers with no X server at all. Its failure mode is a
    /// `None` that costs performance, never a capture backend that won't start.
    #[test]
    fn open_is_infallible_without_a_display() {
        unsafe { std::env::remove_var("ROOMLER_AGENT_X11_DAMAGE") };
        unsafe { std::env::set_var("DISPLAY", ":-1-not-a-display") };
        let _ = DamageTracker::open();
        unsafe { std::env::remove_var("DISPLAY") };
    }
}
