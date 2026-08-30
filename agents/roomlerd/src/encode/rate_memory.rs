// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-35 P2 — per-peer rate memory.
//!
//! The session's stable rate (see [`super::ceiling_learn`]) is remembered per
//! peer so the NEXT session on the same pair opens at 85 % of it instead of
//! at the fleet constant — which is what sizes the opening keyframe and the
//! repair speed on an NVENC relay session (FR-31). Entries expire after
//! [`TTL`]; a pair never seen, or seen too long ago, opens at the constant.
//!
//! Storage is one JSON file in the daemon's data dir, written whole and
//! atomically (temp + rename), read once per session start. It is a cache,
//! never a source of truth: a missing or unreadable file is an empty memory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A remembered rate older than this is ignored (and dropped on the next save).
pub const TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// P3 — the memory grows to this fraction of the opener's measured drain rate.
pub const OPENER_DRAIN_PCT: u64 = 75;
/// Below this many bytes the opening burst says nothing about the pipe.
pub const OPENER_MIN_BYTES: u64 = 100_000;
/// A burst that never queued at all is clamped to this wait, i.e. the estimate
/// saturates at `bytes × 8 / 20 ms` (then `hi` caps it).
pub const OPENER_MIN_WAIT_US: u64 = 20_000;

/// The pipe's burst drain rate implied by the opening burst: `bytes` sent and
/// the longest queue-wait a frame saw while draining it. The last frame of a
/// burst waits for everything before it, so `bytes / wait` is the rate the
/// pipe actually drained at — an over-estimate by one frame's share, which
/// [`OPENER_DRAIN_PCT`] absorbs. `0` = too small a burst to judge.
pub fn opener_drain_bps(bytes: u64, wait_us: u64) -> u32 {
    if bytes < OPENER_MIN_BYTES {
        return 0;
    }
    let wait = wait_us.max(OPENER_MIN_WAIT_US);
    (bytes.saturating_mul(8).saturating_mul(1_000_000) / wait).min(u32::MAX as u64) as u32
}

const FILE_NAME: &str = "rate_memory.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub stable_bps: u32,
    /// Unix seconds when the rate was recorded.
    pub at_unix: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RateMemory {
    #[serde(default)]
    pub entries: BTreeMap<String, Entry>,
}

impl RateMemory {
    /// The remembered stable rate for `peer`, if fresh.
    pub fn seed_for(&self, peer: &str, now_unix: u64) -> Option<u32> {
        let e = self.entries.get(peer)?;
        let fresh = now_unix.saturating_sub(e.at_unix) <= TTL.as_secs();
        fresh.then_some(e.stable_bps)
    }

    /// Record `stable_bps` for `peer`, dropping expired entries as we go.
    pub fn record(&mut self, peer: &str, stable_bps: u32, now_unix: u64) {
        self.entries
            .retain(|_, e| now_unix.saturating_sub(e.at_unix) <= TTL.as_secs());
        self.entries.insert(
            peer.to_string(),
            Entry {
                stable_bps,
                at_unix: now_unix,
            },
        );
    }

    /// Record a SESSION's stable rate. An idle session's "stable rate" is
    /// just the seed it opened at (85 % of what was remembered), so writing it
    /// back would decay the memory by 15 % per idle session until it sits at
    /// the nominal — measured on the first field run (3.60 → 3.06 Mbps after a
    /// 14-s idle session). The rule: a LOWER value is only accepted when the
    /// session saw a decrease (real evidence the pair could not carry the old
    /// memory); otherwise the old value is kept and its timestamp refreshed.
    /// Returns the value now on record.
    ///
    /// P3 — growth without drag. The opening burst is a free probe of the
    /// pipe's burst capacity (`opener_drain_bps`); a session that saw no
    /// decrease grows the memory toward [`OPENER_DRAIN_PCT`] of it, capped
    /// at `hi_bps`, so a pair reaches its crisp opener in one session instead
    /// of after minutes of sustained drag (the first field runs: ≈3 learner
    /// steps per minute of drag, and the operator's sessions last seconds).
    /// A decrease still lowers the memory to the session's stable rate.
    pub fn record_session(
        &mut self,
        peer: &str,
        stable_bps: u32,
        had_decrease: bool,
        opener_drain_bps: u32,
        hi_bps: u32,
        now_unix: u64,
    ) -> u32 {
        let mut value = stable_bps;
        if !had_decrease {
            if let Some(old) = self.seed_for(peer, now_unix)
                && old > value
            {
                value = old;
            }
            let target =
                ((opener_drain_bps as u64) * OPENER_DRAIN_PCT / 100).min(hi_bps as u64) as u32;
            if target > value {
                value = target;
            }
        }
        self.record(peer, value, now_unix);
        value
    }

    /// Missing or unreadable ⇒ empty (logged at debug by the caller if it
    /// cares); a cache must never fail a session.
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Whole-file atomic write: temp sibling + rename.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }
}

/// The memory file's location: the daemon's data dir. `None` when no data
/// dir resolves (memory is then simply off for this process).
pub fn default_path() -> Option<PathBuf> {
    let dirs = crate::appdirs::project_dirs()?;
    Some(dirs.data_dir().join(FILE_NAME))
}

/// Seconds since the Unix epoch, saturating at 0 if the clock is before it.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_the_file_and_seeds_fresh_entries_only() {
        let dir = std::env::temp_dir().join(format!("rate-memory-test-{}", std::process::id()));
        let path = dir.join(FILE_NAME);
        let now = 1_788_000_000u64;
        let mut m = RateMemory::default();
        m.record("100.65.4.2", 6_000_000, now);
        m.record("100.65.4.30", 2_000_000, now - TTL.as_secs() - 1); // already stale
        m.save(&path).expect("save");
        let back = RateMemory::load(&path);
        assert_eq!(back.seed_for("100.65.4.2", now), Some(6_000_000));
        assert_eq!(
            back.seed_for("100.65.4.2", now + TTL.as_secs()),
            Some(6_000_000)
        );
        assert_eq!(back.seed_for("100.65.4.2", now + TTL.as_secs() + 1), None);
        assert_eq!(back.seed_for("unknown", now), None);
        // The stale entry was dropped by the record that followed it? No —
        // record() drops entries stale at ITS time; the second record was
        // itself stale, so it is present but never seeds.
        assert_eq!(back.seed_for("100.65.4.30", now), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_corrupt_file_is_an_empty_memory() {
        let dir = std::env::temp_dir().join(format!("rate-memory-test2-{}", std::process::id()));
        let path = dir.join(FILE_NAME);
        assert_eq!(RateMemory::load(&path), RateMemory::default());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(RateMemory::load(&path), RateMemory::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_idle_session_never_lowers_the_memory_but_a_decrease_does() {
        const HI: u32 = 8_000_000;
        let now = 1_788_000_000u64;
        let mut m = RateMemory::default();
        assert_eq!(
            m.record_session("p", 3_598_387, false, 0, HI, now),
            3_598_387
        );
        // Idle session opened at 85 % and held it: no evidence, keep the old.
        assert_eq!(
            m.record_session("p", 3_058_628, false, 0, HI, now + 60),
            3_598_387
        );
        assert_eq!(m.entries["p"].at_unix, now + 60, "timestamp refreshed");
        // A session that saw a decrease may lower it.
        assert_eq!(
            m.record_session("p", 3_058_628, true, 0, HI, now + 120),
            3_058_628
        );
        // A higher value always replaces, decrease or not.
        assert_eq!(
            m.record_session("p", 4_000_000, false, 0, HI, now + 180),
            4_000_000
        );
        assert_eq!(
            m.record_session("p", 4_100_000, true, 0, HI, now + 240),
            4_100_000
        );
    }

    /// P3 — the opener's drain rate grows the memory in ONE clean session,
    /// never past `hi`, never on a session that saw a decrease, and never
    /// below what the session itself proved.
    #[test]
    fn a_clean_session_grows_the_memory_toward_the_opener_drain_rate() {
        const HI: u32 = 8_000_000;
        let now = 1_788_000_000u64;
        let mut m = RateMemory::default();
        // First session on the pair at the 3 Mbps nominal; the 221 KB opener
        // drained in 221 ms ⇒ 8 Mbps burst capacity ⇒ memory 6 Mbps.
        let drain = opener_drain_bps(221_000, 221_000);
        assert_eq!(drain, 8_000_000);
        assert_eq!(
            m.record_session("p", 3_000_000, false, drain, HI, now),
            6_000_000
        );
        // A thin pipe: 131 KB in 524 ms ⇒ 2 Mbps ⇒ target 1.5 Mbps < the
        // session's own 3 Mbps — the memory does not go below what was held.
        let thin = opener_drain_bps(131_000, 524_000);
        assert_eq!(thin, 2_000_000);
        let mut t = RateMemory::default();
        assert_eq!(
            t.record_session("q", 3_000_000, false, thin, HI, now),
            3_000_000
        );
        // A fat pipe (no queueing at all) saturates at `hi`.
        let fat = opener_drain_bps(221_000, 3_000);
        assert!(fat > HI);
        assert_eq!(
            t.record_session("q", 3_000_000, false, fat, HI, now + 60),
            HI
        );
        // A decrease in the session wins over the drain evidence.
        assert_eq!(
            t.record_session("q", 3_400_000, true, fat, HI, now + 120),
            3_400_000
        );
        // Learning off (hi = 0): the drain never grows anything.
        assert_eq!(
            m.record_session("r", 3_000_000, false, drain, 0, now),
            3_000_000
        );
    }

    #[test]
    fn a_small_opening_burst_is_not_evidence() {
        assert_eq!(opener_drain_bps(30_000, 5_000), 0);
        assert_eq!(opener_drain_bps(99_999, 1), 0);
        assert_eq!(
            opener_drain_bps(100_000, 0),
            40_000_000,
            "clamped to a 20 ms wait"
        );
    }

    #[test]
    fn record_replaces_and_prunes() {
        let now = 1_788_000_000u64;
        let mut m = RateMemory::default();
        m.record("a", 1, now - TTL.as_secs() - 10);
        m.record("b", 2, now);
        assert!(
            !m.entries.contains_key("a"),
            "stale entry pruned on the next record"
        );
        m.record("b", 3, now + 1);
        assert_eq!(m.entries["b"].stable_bps, 3);
    }
}
