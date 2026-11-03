// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-65 P2 / FR-70 AC1 — the runtime canary.
//!
//! A task that asks to wake every 10 ms and records how late it woke. When
//! something holds a worker of the daemon's runtime (a synchronous FFI call, a
//! guard held across an `.await`, a poll that never yields), every task queued
//! behind it wakes late, and this one does too. The pump's stall watch times
//! the pump's own passes and cannot see a task that never got scheduled. This
//! measures that symptom directly, for the cost of one timer.
//!
//! It logs a summary every 10 minutes and warns, at most once a minute, when a
//! wake comes 100 ms late, the stall watch's own bar. Every event reaches
//! `agent_logs`, so a fleet read can put the two side by side: is a pump STALL
//! the runtime's, or the pump's own work?
//!
//! On Windows the daemon holds a 1 ms timer resolution for its whole life
//! (`win_timer`), so a late wake there is the scheduler, not the 15.6 ms tick.
//! A gap of 30 s or more is counted apart and never warned: that is the host
//! sleeping, and a runtime starved that long is the watchdog's to catch.
//!
//! ⚠️ It only reports.
//!
//! Kill switch: `ROOMLERD_RUNTIME_CANARY=0`.

use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use tunnel_core::env::node_env;

const PERIOD: Duration = Duration::from_millis(10);
const SUMMARY_EVERY: Duration = Duration::from_secs(600);
/// The stall watch's bar (`docs/stall-watch.md`), so the two read alike.
const WARN_AT: Duration = Duration::from_millis(100);
const WARN_EVERY: Duration = Duration::from_secs(60);
/// Later than this is the host sleeping, not the runtime starving.
const GAP_AT: Duration = Duration::from_secs(30);
/// The histogram's bucket bounds, in ms. A wake lands in the first bucket whose
/// bound it does not exceed; the last slot holds everything later.
const BOUNDS_MS: [u64; 8] = [1, 2, 5, 10, 20, 50, 100, 250];

/// One summary's worth of wakes. Pure, so the rules are testable without a
/// runtime to starve.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Window {
    counts: [u64; BOUNDS_MS.len() + 1],
    max_us: u64,
    gaps: u64,
}

impl Window {
    fn record(&mut self, late: Duration) {
        if late >= GAP_AT {
            self.gaps += 1;
            return;
        }
        let us = u64::try_from(late.as_micros()).unwrap_or(u64::MAX);
        let slot = BOUNDS_MS
            .iter()
            .position(|&ms| us <= ms * 1_000)
            .unwrap_or(BOUNDS_MS.len());
        self.counts[slot] += 1;
        self.max_us = self.max_us.max(us);
    }

    fn wakes(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// The bound under which at least `q` of the wakes fell, in ms. When that
    /// is past the last bound, the latest wake stands in for it.
    fn quantile_ms(&self, q: f64) -> f64 {
        let wakes = self.wakes();
        if wakes == 0 {
            return 0.0;
        }
        let need = (q * wakes as f64).ceil() as u64;
        let mut seen = 0;
        for (slot, &n) in self.counts.iter().enumerate() {
            seen += n;
            if seen >= need {
                return match BOUNDS_MS.get(slot) {
                    Some(&ms) => ms as f64,
                    None => self.max_ms(),
                };
            }
        }
        self.max_ms()
    }

    /// Wakes later than `ms`, which must be one of [`BOUNDS_MS`].
    fn later_than(&self, ms: u64) -> u64 {
        let slot = BOUNDS_MS.iter().position(|&b| b == ms).map_or(0, |i| i + 1);
        self.counts[slot..].iter().sum()
    }

    fn max_ms(&self) -> f64 {
        (self.max_us as f64 / 100.0).round() / 10.0
    }
}

/// Runs until `shutdown`, and does nothing on a device that switched it off.
pub async fn run(mut shutdown: watch::Receiver<bool>) {
    if node_env("RUNTIME_CANARY").as_deref() == Some("0") {
        return;
    }
    let mut window = Window::default();
    let mut summary_at = Instant::now() + SUMMARY_EVERY;
    let mut last_warn: Option<Instant> = None;
    let mut unwarned = 0u64;
    let mut due = Instant::now() + PERIOD;
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { return; }
                continue;
            }
            _ = tokio::time::sleep_until(due) => {}
        }
        let now = Instant::now();
        let late = now.saturating_duration_since(due);
        window.record(late);
        if (WARN_AT..GAP_AT).contains(&late) {
            if last_warn.is_none_or(|t| now.saturating_duration_since(t) >= WARN_EVERY) {
                last_warn = Some(now);
                tracing::warn!(
                    late_ms = late.as_millis() as u64,
                    unwarned,
                    "runtime canary: a 10 ms timer woke late, so the runtime did not schedule its tasks"
                );
                unwarned = 0;
            } else {
                unwarned += 1;
            }
        }
        // From now, not from the missed deadline: a long stall must not replay
        // as a burst of on-time wakes that would water the window down.
        due = now + PERIOD;
        if now >= summary_at {
            tracing::info!(
                wakes = window.wakes(),
                p50_ms = window.quantile_ms(0.50),
                p99_ms = window.quantile_ms(0.99),
                p999_ms = window.quantile_ms(0.999),
                max_ms = window.max_ms(),
                over_10ms = window.later_than(10),
                over_50ms = window.later_than(50),
                over_100ms = window.later_than(100),
                gaps = window.gaps,
                "runtime canary: how late a 10 ms timer woke, over 10 minutes"
            );
            window = Window::default();
            summary_at = now + SUMMARY_EVERY;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(lates_ms: &[u64]) -> Window {
        let mut w = Window::default();
        for &ms in lates_ms {
            w.record(Duration::from_millis(ms));
        }
        w
    }

    #[test]
    fn an_idle_runtime_reads_on_time() {
        let w = fed(&[0; 60_000]);
        assert_eq!(w.wakes(), 60_000);
        assert_eq!(w.quantile_ms(0.999), 1.0, "the first bucket's bound");
        assert_eq!(w.later_than(10), 0);
        assert_eq!(w.max_ms(), 0.0);
    }

    /// P2's measured case: a 13 ms synchronous encode on a worker, every frame
    /// at 30 fps. A third of the wakes queue behind one.
    #[test]
    fn a_worker_held_13_ms_a_frame_shows_in_the_tail() {
        let mut lates = vec![0u64; 2_000];
        lates.extend(std::iter::repeat_n(13, 1_000));
        let w = fed(&lates);
        assert_eq!(w.quantile_ms(0.50), 1.0);
        assert_eq!(w.quantile_ms(0.99), 20.0, "13 ms lands in the 20 ms bucket");
        assert_eq!(w.later_than(10), 1_000);
        assert_eq!(w.later_than(20), 0);
        assert_eq!(w.max_ms(), 13.0);
    }

    #[test]
    fn a_stall_past_the_last_bound_reports_its_own_length() {
        let mut lates = vec![0u64; 99];
        lates.push(1_800);
        let w = fed(&lates);
        assert_eq!(w.quantile_ms(1.0), 1_800.0);
        assert_eq!(w.later_than(250), 1);
        assert_eq!(w.later_than(100), 1);
    }

    #[test]
    fn a_host_sleep_is_a_gap_not_a_stall() {
        let w = fed(&[0, 0, 45_000, 0]);
        assert_eq!(w.gaps, 1);
        assert_eq!(w.wakes(), 3);
        assert_eq!(w.later_than(100), 0);
        assert_eq!(w.max_ms(), 0.0);
    }

    #[test]
    fn bounds_are_inclusive() {
        let w = fed(&[1, 2, 10, 100]);
        assert_eq!(w.counts[0], 1, "1 ms is the first bucket");
        assert_eq!(w.counts[1], 1);
        assert_eq!(w.counts[3], 1, "10 ms is the 10 ms bucket");
        assert_eq!(w.later_than(10), 1, "only the 100 ms wake is later than 10");
        assert_eq!(w.later_than(100), 0);
    }

    #[test]
    fn an_empty_window_reads_zero() {
        let w = Window::default();
        assert_eq!(w.quantile_ms(0.99), 0.0);
        assert_eq!(w.later_than(50), 0);
    }

    #[tokio::test]
    async fn a_closed_shutdown_channel_ends_the_canary() {
        let (tx, rx) = watch::channel(false);
        drop(tx);
        let ended = tokio::time::timeout(Duration::from_secs(5), run(rx)).await;
        assert!(ended.is_ok(), "the canary must end, not spin");
    }

    #[tokio::test]
    async fn shutdown_ends_a_running_canary() {
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(run(rx));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(true).unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(5), task).await;
        assert!(ended.is_ok(), "shutdown must end the canary");
    }
}
