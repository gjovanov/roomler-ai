// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Per-(caller, device) request limiter for the exec / SSH control planes.
//!
//! Both subsystems DOCUMENT a per-minute ceiling (`exec::RATE_LIMIT_PER_MINUTE`,
//! `ssh::RATE_LIMIT_PER_MINUTE`) and both define a `RateLimited` deny reason,
//! but nothing ever enforced them: the constants had no readers and the deny
//! reason was constructed only in tests. That left two real gaps —
//!
//! * the global `tower_governor` is per-IP and HTTP-only, so it never saw the
//!   device-originated `rc:rpc.request` / `rc:ssh.request` WebSocket legs; and
//! * an SSH caller could burst grants faster than the target's 16-slot pending
//!   table, evicting a legitimate caller's un-redeemed grant (the table drops
//!   the OLDEST) — a targeted denial of someone else's access.
//!
//! Enforcement lives in the shared `authorize` of each subsystem, which is the
//! one place BOTH the HTTP route and the WS leg funnel through, so neither
//! transport can acquire an unlimited path by accident.
//!
//! Sliding window rather than a token bucket: the documented unit is literally
//! "N per minute", and a window keeps that exact meaning with no refill-rate
//! translation for an operator to get wrong.

use bson::oid::ObjectId;
use dashmap::DashMap;
use std::{
    hash::Hash,
    time::{Duration, Instant},
};

/// Window length. The configured ceilings are all expressed per minute.
const WINDOW: Duration = Duration::from_secs(60);

/// How many idle keys may accumulate before a sweep runs. Bounded so a fleet
/// sweep (many distinct devices, each hit once) cannot grow the map forever.
const SWEEP_THRESHOLD: usize = 1024;

/// Sliding-window counter keyed by `(caller, device)`.
#[derive(Default)]
pub struct RateLimiter {
    hits: DashMap<(ObjectId, ObjectId), Vec<Instant>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an attempt and report whether it is WITHIN `limit_per_minute`.
    ///
    /// Returns `true` when the caller may proceed. The attempt is recorded
    /// either way: a refused attempt still consumed a slot, so a caller that
    /// keeps hammering stays refused for the rest of the window instead of
    /// being handed a fresh allowance.
    pub fn check(&self, caller: ObjectId, device: ObjectId, limit_per_minute: u32) -> bool {
        // 0 disables the limiter rather than blocking everything — a
        // misconfigured ceiling must not take a fleet offline.
        if limit_per_minute == 0 {
            return true;
        }
        let now = Instant::now();
        self.maybe_sweep(now);

        let mut entry = self.hits.entry((caller, device)).or_default();
        entry.retain(|t| now.duration_since(*t) < WINDOW);
        let allowed = entry.len() < limit_per_minute as usize;
        entry.push(now);
        allowed
    }

    /// Drop keys whose whole window has expired. Cheap and amortized: only
    /// runs once the map is large enough to be worth walking.
    fn maybe_sweep(&self, now: Instant) {
        if self.hits.len() < SWEEP_THRESHOLD {
            return;
        }
        self.hits
            .retain(|_, v| v.iter().any(|t| now.duration_since(*t) < WINDOW));
    }
}

/// FR-129 A1 — a sliding-window counter over any key, with its own window.
///
/// [`RateLimiter`] is fixed to `(caller, device)` and one minute, which is
/// what exec, SSH and the relay mint document; the pairing routes need a
/// client **address** and an **account** over an **hour**, so this is a
/// sibling rather than a change to a limiter three control planes already
/// rely on. Two ways to use it, for two shapes of ceiling:
///
/// * [`Self::check`] — "N attempts per window": records the attempt and
///   answers whether it was within the ceiling (`start` per address).
/// * [`Self::record`] + [`Self::at_limit`] — "N misses per window lock the
///   key": the caller records only the events that count (a lookup that
///   found nothing) and asks, before acting, whether the key is locked.
///
/// ⚠️ Per pod, in memory, like [`RateLimiter`]: a caller spread across pods
/// gets each pod's allowance. These are abuse ceilings over a code space of
/// ~2.6 × 10¹⁰ and a 15-minute pairing life, not the security boundary —
/// the device code is — so a per-pod count is the right trade against a
/// Redis round trip on every poll.
pub struct WindowLimiter<K: Hash + Eq> {
    window: Duration,
    hits: DashMap<K, Vec<Instant>>,
}

impl<K: Hash + Eq + Clone> WindowLimiter<K> {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            hits: DashMap::new(),
        }
    }

    /// One hour — the pairing ceilings' unit.
    pub fn hourly() -> Self {
        Self::new(Duration::from_secs(60 * 60))
    }

    /// Record an attempt and report whether it is WITHIN `limit` for this
    /// window. [`RateLimiter::check`]'s contract: `0` disables the ceiling,
    /// and a refused attempt still counts, so hammering stays refused.
    pub fn check(&self, key: K, limit: u32) -> bool {
        if limit == 0 {
            return true;
        }
        let now = Instant::now();
        self.maybe_sweep(now);
        let window = self.window;
        let mut entry = self.hits.entry(key).or_default();
        entry.retain(|t| now.duration_since(*t) < window);
        let allowed = entry.len() < limit as usize;
        entry.push(now);
        allowed
    }

    /// Record one event against `key` without deciding anything.
    pub fn record(&self, key: K) {
        let now = Instant::now();
        self.maybe_sweep(now);
        let window = self.window;
        let mut entry = self.hits.entry(key).or_default();
        entry.retain(|t| now.duration_since(*t) < window);
        entry.push(now);
    }

    /// Whether `key` has `limit` or more events in the window. `0` never
    /// locks. Reads only — a locked key stays locked until its oldest event
    /// leaves the window, however often it is asked.
    pub fn at_limit(&self, key: &K, limit: u32) -> bool {
        if limit == 0 {
            return false;
        }
        let now = Instant::now();
        let window = self.window;
        self.hits
            .get(key)
            .map(|v| {
                v.iter()
                    .filter(|t| now.duration_since(**t) < window)
                    .count()
            })
            .unwrap_or(0)
            >= limit as usize
    }

    fn maybe_sweep(&self, now: Instant) {
        if self.hits.len() < SWEEP_THRESHOLD {
            return;
        }
        let window = self.window;
        self.hits
            .retain(|_, v| v.iter().any(|t| now.duration_since(*t) < window));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_the_limit_then_refuses() {
        let rl = RateLimiter::new();
        let (caller, device) = (ObjectId::new(), ObjectId::new());
        for i in 0..5 {
            assert!(rl.check(caller, device, 5), "attempt {i} should be allowed");
        }
        assert!(!rl.check(caller, device, 5), "the 6th must be refused");
    }

    #[test]
    fn limit_is_per_caller_device_pair() {
        let rl = RateLimiter::new();
        let (a, b) = (ObjectId::new(), ObjectId::new());
        let (dev1, dev2) = (ObjectId::new(), ObjectId::new());
        for _ in 0..3 {
            assert!(rl.check(a, dev1, 3));
        }
        assert!(!rl.check(a, dev1, 3));
        // A different device, and a different caller, each get their own
        // budget — one noisy pair must not lock out the rest of the fleet.
        assert!(rl.check(a, dev2, 3));
        assert!(rl.check(b, dev1, 3));
    }

    #[test]
    fn zero_disables_the_limiter() {
        let rl = RateLimiter::new();
        let (caller, device) = (ObjectId::new(), ObjectId::new());
        for _ in 0..100 {
            assert!(rl.check(caller, device, 0));
        }
    }

    #[test]
    fn refused_attempts_do_not_refill_the_window() {
        let rl = RateLimiter::new();
        let (caller, device) = (ObjectId::new(), ObjectId::new());
        assert!(rl.check(caller, device, 1));
        // Hammering while refused keeps it refused.
        for _ in 0..10 {
            assert!(!rl.check(caller, device, 1));
        }
    }

    // ── FR-129 A1 — WindowLimiter ─────────────────────────────────────

    #[test]
    fn window_limiter_check_counts_per_key_and_keeps_refusing() {
        let wl: WindowLimiter<std::net::IpAddr> = WindowLimiter::hourly();
        let a: std::net::IpAddr = "203.0.113.9".parse().unwrap();
        let b: std::net::IpAddr = "203.0.113.10".parse().unwrap();
        for i in 0..3 {
            assert!(wl.check(a, 3), "attempt {i} should be allowed");
        }
        assert!(!wl.check(a, 3), "the 4th must be refused");
        assert!(!wl.check(a, 3), "and the refused attempt counted too");
        // Another address has its own budget.
        assert!(wl.check(b, 3));
        // 0 disables the ceiling rather than refusing everything.
        for _ in 0..50 {
            assert!(wl.check(a, 0));
        }
    }

    #[test]
    fn window_limiter_misses_lock_a_key_only_at_the_limit() {
        let wl: WindowLimiter<ObjectId> = WindowLimiter::hourly();
        let user = ObjectId::new();
        assert!(!wl.at_limit(&user, 2), "no misses yet");
        wl.record(user);
        assert!(!wl.at_limit(&user, 2), "one miss is under a ceiling of two");
        wl.record(user);
        assert!(wl.at_limit(&user, 2), "two misses reach it");
        // Asking does not count: a locked key is not pushed further.
        assert!(wl.at_limit(&user, 2));
        assert!(!wl.at_limit(&ObjectId::new(), 2), "another account is free");
        assert!(!wl.at_limit(&user, 0), "0 never locks");
    }

    #[test]
    fn window_limiter_forgets_events_older_than_its_window() {
        let wl: WindowLimiter<u8> = WindowLimiter::new(Duration::from_millis(30));
        wl.record(7);
        wl.record(7);
        assert!(wl.at_limit(&7, 2));
        std::thread::sleep(Duration::from_millis(60));
        assert!(!wl.at_limit(&7, 2), "the misses aged out of the window");
        assert!(wl.check(7, 1), "and a check starts a fresh count");
    }
}
