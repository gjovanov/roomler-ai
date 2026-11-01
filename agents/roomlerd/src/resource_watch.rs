// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-134 — the daemon notices its own handle leak.
//!
//! #3252 lost 59 handles a second for six months and nothing said so: a
//! self-update restarted each worker before the leak could hurt, and it was
//! found by accident. This samples the daemon's own handle count, logs it once
//! an hour, and warns while it keeps climbing. Every event reaches
//! `agent_logs`, so a fleet read finds a leak with no new wire.
//!
//! ⚠️ It only reports. It must never restart or exit the daemon: a daemon that
//! leaks may still be the only way into the machine, and exiting one nothing
//! supervises takes the device offline for good.
//!
//! Kill switch: `ROOMLERD_RESOURCE_WATCH=0`.
//! `ROOMLERD_RESOURCE_WATCH_MIN_RISE` lowers the bar for a field test.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::sync::watch;
use tunnel_core::env::node_env;

const SAMPLE_EVERY: Duration = Duration::from_secs(300);
/// An hour of samples, so the first and last are an hour apart.
const WINDOW: usize = 13;
/// Each end of the window is judged on this many samples, so one busy moment
/// can neither hide a climb nor fake one.
const ENDS: usize = 3;
/// Log the count once an hour: a read of `agent_logs` then has a history, and
/// silence means the watch stopped, not that nothing changed.
const LOG_EVERY_SAMPLES: u32 = 12;

/// The rise over a window that counts as a leak. Windows gives a process 16.7 M
/// handles and a busy worker opens hundreds. A Unix daemon can have as few as
/// 1,024 descriptors, so the bar is lower there.
#[cfg(windows)]
const MIN_RISE: u64 = 2_000;
#[cfg(not(windows))]
const MIN_RISE: u64 = 200;

/// The handles this process has open, or `None` if the OS will not say.
#[cfg(windows)]
pub fn open_handles() -> Option<u64> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut count = 0u32;
    // SAFETY: the pseudo-handle is always valid and `count` outlives the call.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    (ok != 0).then_some(u64::from(count))
}

/// `/dev/fd` lists this process's descriptors on both Linux and macOS. The
/// listing's own descriptor is in the count, which only offsets it by one.
#[cfg(not(windows))]
pub fn open_handles() -> Option<u64> {
    Some(std::fs::read_dir("/dev/fd").ok()?.count() as u64)
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Steady,
    Climbing { rise: u64 },
}

/// Pure: decides from a series of counts, so the rule is testable without a leak.
struct GrowthWatch {
    samples: VecDeque<u64>,
    min_rise: u64,
}

impl GrowthWatch {
    fn new(min_rise: u64) -> Self {
        Self {
            samples: VecDeque::with_capacity(WINDOW + 1),
            min_rise,
        }
    }

    /// A climb is the quietest of the newest samples sitting above the busiest
    /// of the oldest. A spike that fell back fails that, while a leak passes it
    /// even when it pauses or other handles come and go around it. A one-off
    /// step passes too until it leaves the oldest samples, so it can warn once
    /// where a leak keeps warning. `rise` is the whole window's, for the rate.
    fn observe(&mut self, count: u64) -> Verdict {
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(count);
        if self.samples.len() < WINDOW {
            return Verdict::Steady;
        }
        let oldest = self.samples.iter().take(ENDS).max();
        let newest = self.samples.iter().rev().take(ENDS).min();
        match (oldest, newest) {
            (Some(&old), Some(&new)) if new >= old.saturating_add(self.min_rise) => {
                let first = self.samples.front().copied().unwrap_or(0);
                Verdict::Climbing {
                    rise: count.saturating_sub(first),
                }
            }
            _ => Verdict::Steady,
        }
    }
}

fn min_rise() -> u64 {
    node_env("RESOURCE_WATCH_MIN_RISE")
        .and_then(|v| v.parse().ok())
        .unwrap_or(MIN_RISE)
}

/// Runs until `shutdown`. Does nothing on a device that switched it off, or
/// where the OS will not report a count.
pub async fn run(mut shutdown: watch::Receiver<bool>) {
    if node_env("RESOURCE_WATCH").as_deref() == Some("0") {
        return;
    }
    let mut watch = GrowthWatch::new(min_rise());
    let mut since_log = LOG_EVERY_SAMPLES;
    let mut tick = tokio::time::interval(SAMPLE_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick is immediate, and startup's own handles would read as a climb.
    tick.tick().await;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
                continue;
            }
            _ = tick.tick() => {}
        }
        let Some(handles) = open_handles() else {
            return;
        };
        since_log += 1;
        match watch.observe(handles) {
            Verdict::Climbing { rise } if since_log >= LOG_EVERY_SAMPLES => {
                since_log = 0;
                tracing::warn!(
                    handles,
                    rise,
                    per_second = rise / SAMPLE_EVERY.as_secs() / (WINDOW as u64 - 1),
                    "resource watch: open handles have climbed for an hour, a leak in this daemon?"
                );
            }
            _ if since_log >= LOG_EVERY_SAMPLES => {
                since_log = 0;
                tracing::info!(handles, "resource watch");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(series: &[u64]) -> Verdict {
        let mut w = GrowthWatch::new(2_000);
        let mut last = Verdict::Steady;
        for n in series {
            last = w.observe(*n);
        }
        last
    }

    #[test]
    fn this_process_has_handles_to_count() {
        assert!(open_handles().is_some_and(|n| n > 0));
    }

    #[test]
    fn a_leak_is_called_a_climb_once_the_window_is_full() {
        // 59 handles a second, sampled every 5 minutes: what #3252 did.
        let leak: Vec<u64> = (0..13).map(|i| 400 + i * 17_700).collect();
        assert_eq!(
            fed(&leak[..12]),
            Verdict::Steady,
            "under an hour of samples"
        );
        assert_eq!(fed(&leak), Verdict::Climbing { rise: 212_400 });
    }

    #[test]
    fn a_leak_that_pauses_is_still_a_climb() {
        // Keep-busy stops while the person is at the machine.
        let paused = [
            500, 1_000, 1_000, 1_000, 2_000, 2_000, 2_000, 2_000, 3_000, 3_000, 3_000, 4_000, 5_000,
        ];
        assert_eq!(fed(&paused), Verdict::Climbing { rise: 4_500 });
    }

    #[test]
    fn a_flat_daemon_is_steady_whatever_its_size() {
        assert_eq!(fed(&[50_000; 13]), Verdict::Steady);
    }

    #[test]
    fn a_spike_that_fell_back_is_steady() {
        let spike = [
            600, 600, 600, 9_000, 9_000, 9_000, 9_000, 600, 600, 600, 600, 600, 600,
        ];
        assert_eq!(fed(&spike), Verdict::Steady);
    }

    #[test]
    fn a_step_that_settled_early_is_gone_by_the_next_window() {
        // A burst of tunnel flows opened and stayed: one window can see the
        // step, and it clears once the step leaves the oldest samples.
        let mut w = GrowthWatch::new(2_000);
        let mut verdicts = Vec::new();
        for i in 0..40u64 {
            verdicts.push(w.observe(if i < 6 { 600 } else { 5_000 }));
        }
        assert_eq!(verdicts.last(), Some(&Verdict::Steady));
    }

    #[test]
    fn noise_around_a_flat_line_is_steady() {
        let noise = [
            900, 950, 880, 1_020, 910, 970, 890, 1_000, 930, 960, 900, 940, 920,
        ];
        assert_eq!(fed(&noise), Verdict::Steady);
    }

    #[test]
    fn counts_that_fall_cannot_underflow() {
        let falling: Vec<u64> = (0..13).rev().map(|i| i * 10_000).collect();
        assert_eq!(fed(&falling), Verdict::Steady);
    }
}
