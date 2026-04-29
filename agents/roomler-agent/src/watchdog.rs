//! Liveness watchdog for the agent's main pumps.
//!
//! Each pump (signaling, encoder, capture) ticks the watchdog after
//! every iteration. A background tokio task scans for stalls and
//! force-exits the process via `std::process::exit(2)` when one is
//! detected — relying on the OS supervisor (Scheduled Task on
//! Windows with `RestartOnFailure`, `Restart=on-failure` on systemd,
//! `KeepAlive` on macOS) to relaunch a healthy copy.
//!
//! Why force-exit instead of panic? A panic on the watchdog task
//! doesn't unwind the application — `tokio::spawn` swallows it, the
//! agent continues with its hung pump. Hard exit is the only signal
//! that propagates to the supervisor's restart counter.
//!
//! ## Suspend / resume
//!
//! On a laptop close-lid → resume cycle, `Instant` is monotonic-but-
//! paused, so a 4-hour suspend looks like a 4-hour stall to a naive
//! scanner. The watchdog's `run` loop compares the actual sleep
//! duration to the expected scan interval; a delta beyond
//! [`SUSPEND_TOLERANCE`] is treated as a wall-clock jump and the
//! pump heartbeats are reset instead of triggering a stall exit.
//!
//! ## Watchdog-of-watchdog
//!
//! [`spawn_thread_watchdog`] starts a `std::thread` (NOT a tokio
//! task) that wakes every 30 s and force-exits if the async
//! watchdog's own heartbeat counter hasn't moved in 60 s. This is
//! the absolute last resort against a fully-deadlocked tokio
//! runtime — the only place in the codebase where we deliberately
//! step outside the async world.
//!
//! ## Global singleton
//!
//! The watchdog is a process-wide singleton accessed via
//! [`tick`] / [`gate`] / [`is_active`] free functions. Callers don't
//! need to thread an `Arc<Watchdog>` through their signatures, and
//! integration tests that don't [`install`] one just have ticks
//! become silent no-ops — the pump code stays clean.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Default scan cadence. Cheap enough to run every 5 s without
/// noticeable CPU; granular enough that a 90 s stall surfaces
/// within ~95 s.
pub const SCAN_INTERVAL: Duration = Duration::from_secs(5);

/// Suspend-detection threshold. If the watchdog loop sleeps for
/// `SCAN_INTERVAL + SUSPEND_TOLERANCE` or more, treat the gap as a
/// wall-clock jump (laptop suspend, VM resume) and reset pump
/// heartbeats instead of declaring everything stalled.
pub const SUSPEND_TOLERANCE: Duration = Duration::from_secs(60);

/// Sentinel exit code reserved for watchdog-forced terminations.
/// Distinct from 0 (clean) and 1 (unhandled error) so post-mortem
/// log aggregation can pick out watchdog kills cleanly.
pub const STALL_EXIT_CODE: i32 = 2;

/// Process-wide singleton. Set by `install`; read by the free
/// functions and the `run` task.
static WATCHDOG: OnceLock<Arc<Watchdog>> = OnceLock::new();

struct PumpState {
    last_tick: Instant,
    /// When false, the watchdog ignores this pump. Used to gate
    /// per-session pumps (encoder, capture) — they can legitimately
    /// go silent for hours when no controller is connected.
    active: bool,
    threshold: Duration,
}

pub struct Watchdog {
    pumps: Mutex<HashMap<&'static str, PumpState>>,
    /// Counter the watchdog-of-watchdog reads. Bumped every scan
    /// cycle; if it stops bumping for >60 s the async watchdog is
    /// presumed dead and the std::thread fallback force-exits.
    own_heartbeat: AtomicU64,
}

impl Watchdog {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pumps: Mutex::new(HashMap::new()),
            own_heartbeat: AtomicU64::new(0),
        })
    }

    /// Declare a pump and its stall threshold. `active=false` means
    /// the watchdog ignores ticks until the pump explicitly calls
    /// `gate(true)` — appropriate for per-session pumps.
    pub fn register(&self, name: &'static str, threshold: Duration, active: bool) {
        if let Ok(mut m) = self.pumps.lock() {
            m.insert(
                name,
                PumpState {
                    last_tick: Instant::now(),
                    active,
                    threshold,
                },
            );
        }
    }

    pub fn tick(&self, name: &'static str) {
        if let Ok(mut m) = self.pumps.lock()
            && let Some(p) = m.get_mut(name)
        {
            p.last_tick = Instant::now();
        }
    }

    /// Enable / disable watchdog for a pump. When transitioning
    /// `false` → `true` the tick is reset so a long gated-off
    /// window doesn't immediately fire on enable.
    pub fn gate(&self, name: &'static str, active: bool) {
        if let Ok(mut m) = self.pumps.lock()
            && let Some(p) = m.get_mut(name)
        {
            if active && !p.active {
                p.last_tick = Instant::now();
            }
            p.active = active;
        }
    }

    /// Pure scan: returns the (pump_name, stall_duration) pairs that
    /// have exceeded their threshold at the given clock instant.
    /// Names are sorted so test assertions are deterministic.
    pub fn scan_at(&self, now: Instant) -> Vec<(&'static str, Duration)> {
        let Ok(m) = self.pumps.lock() else {
            return Vec::new();
        };
        let mut stalled: Vec<_> = m
            .iter()
            .filter(|(_, p)| p.active)
            .filter_map(|(name, p)| {
                let elapsed = now.saturating_duration_since(p.last_tick);
                if elapsed > p.threshold {
                    Some((*name, elapsed))
                } else {
                    None
                }
            })
            .collect();
        stalled.sort_by_key(|(name, _)| *name);
        stalled
    }

    /// Force-reset every pump's heartbeat to `now`. Called by `run`
    /// when it detects a wall-clock jump (suspend/resume).
    fn reset_all(&self, now: Instant) {
        if let Ok(mut m) = self.pumps.lock() {
            for p in m.values_mut() {
                p.last_tick = now;
            }
        }
    }

    pub fn own_heartbeat(&self) -> u64 {
        self.own_heartbeat.load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------
// Global singleton helpers
// ---------------------------------------------------------------------------

/// Install the process-wide watchdog. Subsequent `install` calls are
/// a no-op (returns `Err(arg)` so the caller can detect double-init
/// in debug builds; release builds ignore the result).
pub fn install(wd: Arc<Watchdog>) -> Result<(), Arc<Watchdog>> {
    WATCHDOG.set(wd)
}

/// Tick a registered pump. Silent no-op when no watchdog is
/// installed (the integration-test path) or the pump isn't
/// registered (the wrong-name typo case — better to silently miss
/// stall detection than crash on a typo in a hot path).
pub fn tick(name: &'static str) {
    if let Some(wd) = WATCHDOG.get() {
        wd.tick(name);
    }
}

/// Enable or disable watchdog for a pump. Same no-op semantics as
/// `tick` when no watchdog is installed.
pub fn gate(name: &'static str, active: bool) {
    if let Some(wd) = WATCHDOG.get() {
        wd.gate(name, active);
    }
}

/// Whether a watchdog is installed. Used by tests + diagnostic
/// commands to confirm the runtime configuration.
pub fn is_active() -> bool {
    WATCHDOG.get().is_some()
}

// ---------------------------------------------------------------------------
// Run loop + thread-watchdog
// ---------------------------------------------------------------------------

/// Run the scan loop. Returns when shutdown is signalled or
/// `on_stall` returns false. Production path passes
/// [`force_exit_on_stall`] which never returns (calls `exit`).
pub async fn run(
    wd: Arc<Watchdog>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    on_stall: impl Fn(&[(&'static str, Duration)]) -> bool + Send + 'static,
) {
    let mut prev = Instant::now();
    loop {
        if *shutdown.borrow() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(SCAN_INTERVAL) => {},
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            },
        }
        let now = Instant::now();
        // Bump our own heartbeat first so the std::thread watchdog-
        // of-watchdog doesn't flag us mid-iteration on a slow host.
        wd.own_heartbeat.fetch_add(1, Ordering::Release);

        let actual = now.saturating_duration_since(prev);
        if actual > SCAN_INTERVAL + SUSPEND_TOLERANCE {
            tracing::warn!(
                lag_secs = actual.as_secs(),
                "watchdog detected wall-clock jump (suspend/resume); resetting pump heartbeats"
            );
            wd.reset_all(now);
            prev = now;
            continue;
        }
        prev = now;

        let stalled = wd.scan_at(now);
        if !stalled.is_empty() && !on_stall(&stalled) {
            return;
        }
    }
}

/// Spawn the watchdog-of-watchdog on a dedicated `std::thread`.
/// Force-exits the process if the async watchdog's heartbeat
/// counter has not moved in >60 s — catches the case where the
/// tokio runtime itself is deadlocked.
pub fn spawn_thread_watchdog(wd: Arc<Watchdog>) {
    let _ = std::thread::Builder::new()
        .name("roomler-agent-watchdog-of-watchdog".into())
        .spawn(move || {
            let mut prev_count = wd.own_heartbeat();
            let mut prev_at = Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(30));
                let now_count = wd.own_heartbeat();
                let now_at = Instant::now();
                if now_count != prev_count {
                    prev_count = now_count;
                    prev_at = now_at;
                    continue;
                }
                let stuck_for = now_at.saturating_duration_since(prev_at);
                if stuck_for > Duration::from_secs(60) {
                    eprintln!(
                        "watchdog-of-watchdog: async watchdog has not heartbeat in {:?}; \
                         forcing exit({STALL_EXIT_CODE})",
                        stuck_for
                    );
                    std::process::exit(STALL_EXIT_CODE);
                }
            }
        });
}

/// Default stall handler — log + `std::process::exit(STALL_EXIT_CODE)`.
/// Returns `false` so `run` interprets it as "stop the loop" in the
/// extremely-unlikely-but-technically-possible case where exit
/// doesn't terminate the process.
pub fn force_exit_on_stall(stalled: &[(&'static str, Duration)]) -> bool {
    let summary: Vec<String> = stalled
        .iter()
        .map(|(n, d)| format!("{n}={}s", d.as_secs()))
        .collect();
    let summary = summary.join(", ");
    eprintln!("watchdog: pumps stalled ({summary}); forcing exit({STALL_EXIT_CODE})");
    tracing::error!(stalled = %summary, "watchdog: forcing process exit");
    std::process::exit(STALL_EXIT_CODE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_pump_starts_unstalled() {
        let wd = Watchdog::new();
        wd.register("test", Duration::from_secs(30), true);
        assert!(wd.scan_at(Instant::now()).is_empty());
    }

    #[test]
    fn pump_stalls_after_threshold() {
        let wd = Watchdog::new();
        wd.register("test", Duration::from_millis(50), true);
        std::thread::sleep(Duration::from_millis(100));
        let stalled = wd.scan_at(Instant::now());
        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0].0, "test");
        assert!(stalled[0].1 >= Duration::from_millis(50));
    }

    #[test]
    fn tick_resets_stall() {
        let wd = Watchdog::new();
        wd.register("test", Duration::from_millis(50), true);
        std::thread::sleep(Duration::from_millis(80));
        wd.tick("test");
        assert!(wd.scan_at(Instant::now()).is_empty());
    }

    #[test]
    fn gated_off_pump_never_stalls() {
        let wd = Watchdog::new();
        wd.register("test", Duration::from_millis(50), false);
        std::thread::sleep(Duration::from_millis(100));
        assert!(wd.scan_at(Instant::now()).is_empty());
    }

    #[test]
    fn gate_on_resets_tick_after_long_gap() {
        let wd = Watchdog::new();
        wd.register("test", Duration::from_millis(50), false);
        std::thread::sleep(Duration::from_millis(100));
        wd.gate("test", true);
        assert!(
            wd.scan_at(Instant::now()).is_empty(),
            "gate(true) on a previously-inactive pump must reset tick"
        );
    }

    #[test]
    fn gate_on_to_on_does_not_reset_tick() {
        // Repeated gate(true) calls should be idempotent — they
        // shouldn't paper over a real stall.
        let wd = Watchdog::new();
        wd.register("test", Duration::from_millis(50), true);
        std::thread::sleep(Duration::from_millis(80));
        wd.gate("test", true); // already active
        let stalled = wd.scan_at(Instant::now());
        assert_eq!(stalled.len(), 1, "redundant gate(true) must not mask a stall");
    }

    #[test]
    fn multiple_stalled_pumps_all_reported_sorted() {
        let wd = Watchdog::new();
        wd.register("zebra", Duration::from_millis(50), true);
        wd.register("alpha", Duration::from_millis(50), true);
        wd.register("calm", Duration::from_secs(60), true); // not stalled
        std::thread::sleep(Duration::from_millis(120));
        let stalled = wd.scan_at(Instant::now());
        let names: Vec<&str> = stalled.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["alpha", "zebra"], "must be sorted by name");
    }

    #[test]
    fn unknown_pump_tick_is_silent_noop() {
        let wd = Watchdog::new();
        wd.register("known", Duration::from_secs(30), true);
        wd.tick("nonexistent"); // must not panic
        assert!(wd.scan_at(Instant::now()).is_empty());
    }

    #[test]
    fn reset_all_brings_pumps_back_to_now() {
        let wd = Watchdog::new();
        wd.register("a", Duration::from_millis(50), true);
        std::thread::sleep(Duration::from_millis(100));
        let now = Instant::now();
        wd.reset_all(now);
        assert!(wd.scan_at(now).is_empty());
    }

    #[test]
    fn global_helpers_are_noops_when_uninstalled() {
        // No `install` call in this test — `tick` / `gate` must not
        // panic. The global is process-shared with other tests but
        // those don't install it either, so the assertion holds in
        // a fresh process. (cargo test runs in a dedicated process
        // per test binary.)
        tick("unregistered");
        gate("unregistered", true);
        // is_active may be true if a prior test in this run installed
        // it; that's fine — what matters is no panic.
    }
}
