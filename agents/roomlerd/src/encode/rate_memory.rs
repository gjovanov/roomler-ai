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

/// Below this many bytes the opening burst says nothing about the pipe.
pub const OPENER_MIN_BYTES: u64 = 100_000;
/// Below this queue-wait the burst never queued — it fit the transport's own
/// send buffer (SCTP's, on a DataChannel) and drained at whatever rate
/// without telling us. Field 2026-08-30: a 238 KB opener with a 0 ms wait on
/// the same coturn path that a 451 KB opener had just measured at 4.5 Mbps.
pub const OPENER_QUEUED_MIN_US: u64 = 100_000;
/// A burst that never queued proves only "not slower than this": grow by a
/// bounded step per clean session instead — the next, larger opener then
/// queues and measures.
pub const OPENER_UNQUEUED_STEP_PCT: u64 = 150;

/// FR-79 V2 — the memory target the opening burst implies, capped at `hi`.
///
/// A burst that **never queued** (`wait_us` under [`OPENER_QUEUED_MIN_US`])
/// fit inside the transport's own send buffer and drained at whatever rate
/// without telling us: it proves only "not slower than this", so the memory
/// grows by a bounded step and the next, larger opener does the measuring.
///
/// A burst that **did** queue is measured the way every other window is
/// measured — the goodput estimator's byte-weighted bytes-over-blocked-time,
/// passed in as `measured_bps` and already gated by [`super::evidence`], so a
/// stall cannot contribute to it. `None` there means the estimator had no
/// confidence yet, and the honest target is then nothing at all.
///
/// ⚠️ Until FR-79 this divided the WHOLE burst by the LONGEST SINGLE frame's
/// wait, which is a rate only if that frame waited for the entire burst.
/// Measured on CORPLAP-1, 2026-09-08 (the operator's six trials): 829 KB with
/// an 829 ms worst wait read as 8.0 Mbps and recorded 6.0 M; 1.19 MB with a
/// 440 ms wait recorded the 8 M cap. The pipe measured 1.79–3.41 M in those
/// same sessions. `record_session` keeps the maximum, so the memory ratcheted
/// to the cap and the next session opened 2–4× over the relay, dropped
/// hundreds of frames at the byte gate and had its ceiling abandoned a second
/// in — the other half of the operator's "starts very blurred".
pub fn opener_growth_target_bps(
    bytes: u64,
    wait_us: u64,
    opener_maxrate_bps: u32,
    measured_bps: Option<u32>,
    hi_bps: u32,
) -> u32 {
    if bytes < OPENER_MIN_BYTES || hi_bps == 0 {
        return 0;
    }
    let target = if wait_us < OPENER_QUEUED_MIN_US {
        (opener_maxrate_bps as u64) * OPENER_UNQUEUED_STEP_PCT / 100
    } else {
        u64::from(measured_bps.unwrap_or(0))
    };
    target.min(hi_bps as u64) as u32
}

/// FR-79 V2 — the carrier half of the memory key.
///
/// The key was the nominated pair's remote address alone, which on an overlay
/// pair is the peer's overlay IP — one key for a path whose carrier moves
/// underneath it between direct, a UDP relay and DERP over TLS, with four
/// times the capacity between the ends of that range. FR-59 P6's own comment
/// named the consequence ("relay-keyed rate memory can carry a fast day onto a
/// slow one") and 2026-09-08 measured it: six demote-follows onto DERP in one
/// day on CORPLAP-1, a memory holding 8 Mbps, and sessions opening at 6.8 M
/// into a pipe that measured 1.8–3.4 M.
///
/// So the carrier is part of the key. `None` = do not qualify the key (the
/// carrier is mid-churn or unknown), which leaves exactly the pre-FR-79
/// behaviour for that session.
///
/// ⚠️ Entries written before this are keyed by the bare address and simply
/// stop matching; they age out with [`TTL`]. The cost is one session of
/// learning per pair and carrier, once.
pub fn carrier_tag(
    connection: &str,
    relay_kind: Option<&str>,
    relay_transport: Option<&str>,
) -> Option<String> {
    match connection {
        "direct" => Some("direct".to_string()),
        "tunnel" => Some("tunnel".to_string()),
        "relay" => Some(match (relay_kind, relay_transport) {
            // `relay:derp/tcp` and `relay:turn/udp` are different pipes, and
            // the whole point of the key is that they are not the same day.
            (Some(k), Some(t)) if !k.is_empty() && !t.is_empty() => format!("relay:{k}/{t}"),
            (Some(k), _) if !k.is_empty() => format!("relay:{k}"),
            _ => "relay".to_string(),
        }),
        // `blocked` / `offline` are the carrier mid-churn: not a pipe to
        // remember anything about.
        _ => None,
    }
}

/// The memory key for a pair on a carrier. Without a tag it is the bare
/// address, which is what every entry written before FR-79 V2 used.
pub fn memory_key(remote_addr: &str, carrier: Option<&str>) -> String {
    match carrier {
        Some(tag) => format!("{remote_addr}|{tag}"),
        None => remote_addr.to_string(),
    }
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
    /// pipe's burst capacity (`opener_growth_target_bps`); a session that saw no
    /// decrease grows the memory toward it, capped at `hi`, so a pair reaches
    /// its crisp opener in a few sessions instead of after minutes of sustained
    /// drag (the first field runs: ≈3 learner steps per minute of drag, and the
    /// operator's sessions last seconds). ⚠️ The learner only reports a stable
    /// rate ABOVE the nominal, so the common short session lands here with
    /// `stable_bps == 0` — the write-back must run on the opener evidence ALONE,
    /// which is why the guard's Drop no longer short-circuits on `stable == 0`
    /// (P3b, field 2026-08-30: a clean session measured 7.3 Mbps and discarded
    /// it). A decrease from a real learner rate still lowers to it.
    pub fn record_session(
        &mut self,
        peer: &str,
        stable_bps: u32,
        had_decrease: bool,
        growth_target_bps: u32,
        now_unix: u64,
    ) -> u32 {
        // The ceiling learner only reports a stable rate ABOVE the nominal, so a
        // short static session — the common case — arrives here with
        // `stable_bps == 0`. That is NOT evidence to lower anything: the memory
        // stays at least what was remembered, and the opener still grows it.
        // A decrease from a real learner rate (`stable_bps > 0`) is the one
        // path allowed to lower.
        let old = self.seed_for(peer, now_unix).unwrap_or(0);
        let value = if had_decrease && stable_bps > 0 {
            stable_bps
        } else {
            stable_bps.max(old).max(growth_target_bps)
        };
        if value == 0 {
            // Nothing to remember (empty memory, idle session, no growth) — do
            // not write a zero entry.
            return 0;
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
        let now = 1_788_000_000u64;
        let mut m = RateMemory::default();
        assert_eq!(m.record_session("p", 3_598_387, false, 0, now), 3_598_387);
        // Idle session opened at 85 % and held it: no evidence, keep the old.
        assert_eq!(
            m.record_session("p", 3_058_628, false, 0, now + 60),
            3_598_387
        );
        assert_eq!(m.entries["p"].at_unix, now + 60, "timestamp refreshed");
        // A session that saw a decrease may lower it.
        assert_eq!(
            m.record_session("p", 3_058_628, true, 0, now + 120),
            3_058_628
        );
        // A higher value always replaces, decrease or not.
        assert_eq!(
            m.record_session("p", 4_000_000, false, 0, now + 180),
            4_000_000
        );
        assert_eq!(
            m.record_session("p", 4_100_000, true, 0, now + 240),
            4_100_000
        );
    }

    /// P3 — the opener grows the memory in ONE clean session, never past `hi`,
    /// never on a session that saw a decrease, and never below what the
    /// session itself proved.
    #[test]
    fn a_clean_session_grows_the_memory_from_the_opener() {
        const HI: u32 = 8_000_000;
        let now = 1_788_000_000u64;
        // Field 2026-08-30, coturn path: a 451 KB opener that queued, with the
        // estimator measuring 4.5 Mbps over the blocked sends — the target IS
        // the measurement (FR-79 V2; before it, the whole burst over the
        // longest single wait, which is not a rate).
        let measured = opener_growth_target_bps(451_464, 802_000, 2_550_000, Some(4_504_000), HI);
        assert_eq!(measured, 4_504_000);
        let mut m = RateMemory::default();
        assert_eq!(
            m.record_session("p", 2_709_375, false, measured, now),
            4_504_000
        );
        // Same path, next session: a 238 KB opener that never queued (0 ms —
        // it fit SCTP's buffer) is NOT a fat pipe: a bounded ×1.5 step on the
        // opener's own maxrate, so the next, larger opener measures.
        let unqueued = opener_growth_target_bps(238_306, 0, 2_870_000, None, HI);
        assert_eq!(unqueued, 4_305_000);
        // …and it is BELOW what the pair already proved, so the memory keeps
        // what it held: the step grows a memory, it never shrinks one.
        assert_eq!(
            m.record_session("p", 2_870_000, false, unqueued, now + 60),
            4_504_000
        );
        // A thin pipe: the estimator measured 1.5 Mbps, below the session's own
        // 3 Mbps — the memory does not go below what was held.
        let thin = opener_growth_target_bps(131_000, 524_000, 3_000_000, Some(1_500_000), HI);
        assert_eq!(thin, 1_500_000);
        let mut t = RateMemory::default();
        assert_eq!(
            t.record_session("q", 3_000_000, false, thin, now),
            3_000_000
        );
        // A measured fat pipe saturates at `hi`.
        let fat = opener_growth_target_bps(2_000_000, 200_000, 3_000_000, Some(60_000_000), HI);
        assert_eq!(fat, HI);
        assert_eq!(t.record_session("q", 3_000_000, false, fat, now + 60), HI);
        // A decrease in the session wins over the evidence.
        assert_eq!(
            t.record_session("q", 3_400_000, true, fat, now + 120),
            3_400_000
        );
        // 🔑 The COMMON case: a short static session reports stable=0 (the
        // learner never rose above the nominal). The opener evidence alone must
        // still grow the memory — before P3b the guard's Drop discarded it.
        let mut u = RateMemory::default();
        assert_eq!(u.record_session("z", 5_000_000, false, 0, now), 5_000_000);
        assert_eq!(
            u.record_session("z", 0, false, 7_307_342, now + 60),
            7_307_342
        );
        // stable=0 with NO growth keeps the memory (idle keep-alive), never zeroes it.
        assert_eq!(u.record_session("z", 0, false, 0, now + 120), 7_307_342);
        assert_eq!(
            u.entries["z"].at_unix,
            now + 120,
            "timestamp refreshed on idle"
        );
        // stable=0 + a decrease flag must NOT lower a learned memory (a decrease
        // is only real evidence when the learner had a rate above the nominal).
        assert_eq!(u.record_session("z", 0, true, 0, now + 180), 7_307_342);
        // An empty memory with no evidence at all records nothing (returns 0).
        let mut e = RateMemory::default();
        assert_eq!(e.record_session("none", 0, false, 0, now), 0);
        assert!(!e.entries.contains_key("none"));
        // Learning off (hi = 0): nothing to learn.
        assert_eq!(
            opener_growth_target_bps(451_464, 802_000, 2_550_000, Some(4_504_000), 0),
            0
        );
    }

    #[test]
    fn a_small_opening_burst_is_not_evidence() {
        assert_eq!(
            opener_growth_target_bps(30_000, 5_000, 3_000_000, None, 8_000_000),
            0
        );
        assert_eq!(
            opener_growth_target_bps(99_999, 900_000, 3_000_000, Some(2_000_000), 8_000_000),
            0
        );
        // The bounded step never passes hi either.
        assert_eq!(
            opener_growth_target_bps(100_000, 0, 7_000_000, None, 8_000_000),
            8_000_000
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

    /// FR-79 V2, the operator's 2026-09-08 trials — a burst that queued is
    /// worth what the estimator measured, not what dividing it by one frame's
    /// wait suggested. CORPLAP-1 16:41:51: 829 KB with an 829 ms worst wait on
    /// a relay the same session then measured at 3.41 Mbps. The old rule
    /// recorded 6.0 M (and 8.0 M twice more that hour), so the next session
    /// opened at 6.8 M into a 3.4 M pipe, dropped 212 frames at the byte gate
    /// in two seconds and had its ceiling abandoned — one half of "starts very
    /// blurred". The other half is the same memory after a P6 abandonment
    /// wrote back the stall's own number.
    #[test]
    fn a_queued_opener_is_worth_what_the_estimator_measured() {
        const HI: u32 = 8_000_000;
        let target = opener_growth_target_bps(829_241, 829_000, 6_800_000, Some(3_408_128), HI);
        assert_eq!(target, 3_408_128, "the measurement, not the arithmetic");
        // With no confidence yet the honest target is nothing: the session
        // learns from its own windows instead.
        assert_eq!(
            opener_growth_target_bps(829_241, 829_000, 6_800_000, None, HI),
            0
        );
        // …and the unqueued branch is untouched: a burst that never queued
        // still proves "not slower than this" and grows by its bounded step.
        assert_eq!(
            opener_growth_target_bps(1_156_688, 0, 6_800_000, None, HI),
            HI,
            "6.8 M x 1.5, capped at hi"
        );
    }

    /// FR-79 V2 — the carrier is part of the key, and a DERP day is not a
    /// relay day. CORPLAP-1, 2026-09-08: one overlay address, six demotions
    /// onto DERP, and a memory that carried 8 Mbps onto a 1.8 M pipe.
    #[test]
    fn the_carrier_is_part_of_the_key() {
        assert_eq!(
            carrier_tag("relay", Some("derp"), Some("tcp")).as_deref(),
            Some("relay:derp/tcp")
        );
        assert_eq!(
            carrier_tag("relay", Some("turn"), Some("udp")).as_deref(),
            Some("relay:turn/udp")
        );
        assert_eq!(carrier_tag("relay", None, None).as_deref(), Some("relay"));
        assert_eq!(carrier_tag("direct", None, None).as_deref(), Some("direct"));
        assert_eq!(carrier_tag("tunnel", None, None).as_deref(), Some("tunnel"));
        // Mid-churn is not a pipe: no tag, so the session keeps the bare key.
        assert_eq!(carrier_tag("blocked", None, None), None);
        assert_eq!(carrier_tag("offline", None, None), None);

        assert_eq!(
            memory_key("100.65.0.5", Some("relay:derp/tcp")),
            "100.65.0.5|relay:derp/tcp"
        );
        assert_eq!(memory_key("100.65.0.5", None), "100.65.0.5");
        // The two carriers keep their own memories: a fast day cannot open a
        // slow one.
        let now = 1_788_000_000u64;
        let mut m = RateMemory::default();
        m.record_session(
            &memory_key("100.65.0.5", Some("direct")),
            0,
            false,
            8_000_000,
            now,
        );
        assert_eq!(
            m.seed_for(&memory_key("100.65.0.5", Some("relay:derp/tcp")), now),
            None
        );
        assert_eq!(
            m.seed_for(&memory_key("100.65.0.5", Some("direct")), now),
            Some(8_000_000)
        );
    }
}
