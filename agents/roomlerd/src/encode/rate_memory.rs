// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-35 P2 — per-peer rate memory.
//!
//! The session's stable rate (see [`super::ceiling_learn`]) is remembered per
//! peer so the NEXT session on the same pair opens at 85 % of it instead of
//! at the fleet constant — which is what sizes the opening keyframe and the
//! repair speed on an NVENC relay session (FR-31). Entries expire after
//! [`TTL`]; a pair never seen, seen too long ago or assumed opens at the constant.
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

/// FR-79 V4 — move a remembered rate toward a measurement: fast down, slow up.
/// The two errors do not cost the same. Opening UNDER what the carrier
/// carries costs an AIMD climb the ceiling learner shortens; opening OVER it
/// costs a flooded opener, frames dropped at the byte gate, an abandoned
/// ceiling and a visible stall — the operator's own report on 2026-09-08.
/// The weights are `goodput`'s, not a second pair: one asymmetry, used inside
/// a session and across them.
pub fn damp(old: u32, measured: u32) -> u32 {
    let alpha = if measured < old {
        super::goodput::ALPHA_DOWN
    } else {
        super::goodput::ALPHA_UP
    };
    let next = f64::from(old) + alpha * (f64::from(measured) - f64::from(old));
    next.clamp(0.0, f64::from(u32::MAX)) as u32
}
const FILE_NAME: &str = "rate_memory.json";

/// FR-148 §4.1 — how a remembered rate was obtained. A measured rate may open
/// a session. An assumed one "is never stored, and it never holds the plan
/// below a demonstrated value".
///
/// On disk it is a string, and anything but `"measured"` is read as
/// [`Provenance::Assumed`]. That covers a missing field, which is every entry
/// written before FR-148 P1, and a value written by a later agent that this
/// one does not know.
///
/// ⚠️ Why (FR-148 §2.1): on 2026-11-16 the operator's pair was remembered at
/// 281,732 bit/s, which grew from a placeholder that had been written as
/// evidence before V5b. An entry under 1 Mbit/s opens on the slow-link
/// profile, and the sessions such an entry opens can only creep it upward.
/// V5b stops new placeholders. Reading the old entries as assumed lifts the
/// ones already on disk.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Provenance {
    /// Nothing this agent can vouch for. It seeds nothing, it is no base for
    /// [`damp`], and the next [`RateMemory::record`] drops it.
    #[default]
    Assumed,
    /// What a session measured about the carrier: the write-back's evidence,
    /// which since V5b is a reading or nothing.
    Measured,
}

impl From<String> for Provenance {
    fn from(s: String) -> Self {
        if s == "measured" {
            Self::Measured
        } else {
            Self::Assumed
        }
    }
}

impl From<Provenance> for String {
    fn from(p: Provenance) -> Self {
        match p {
            Provenance::Assumed => "assumed",
            Provenance::Measured => "measured",
        }
        .to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub stable_bps: u32,
    /// Unix seconds when the rate was recorded.
    pub at_unix: u64,
    /// FR-148 P1. An entry written before that phase has no such field, so it
    /// is read as [`Provenance::Assumed`].
    #[serde(default)]
    pub provenance: Provenance,
}

impl Entry {
    fn fresh(&self, now_unix: u64) -> bool {
        now_unix.saturating_sub(self.at_unix) <= TTL.as_secs()
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RateMemory {
    #[serde(default)]
    pub entries: BTreeMap<String, Entry>,
}

impl RateMemory {
    /// The remembered stable rate for `peer`, if fresh and measured. An
    /// assumed entry seeds nothing (FR-148 P1): the pair opens as one never
    /// seen, so with no slow-link profile and no opening below the floor.
    pub fn seed_for(&self, peer: &str, now_unix: u64) -> Option<u32> {
        let e = self.entries.get(peer)?;
        (e.provenance == Provenance::Measured && e.fresh(now_unix)).then_some(e.stable_bps)
    }

    /// The seed a session opens from, which is [`Self::seed_for`]'s. When the
    /// pair's fresh entry is assumed, one line says so. That line is the
    /// field's evidence that an entry from before FR-148 P1 no longer opens
    /// the pair.
    pub fn seed_at_open(&self, peer: &str, now_unix: u64) -> Option<u32> {
        if let Some(e) = self.entries.get(peer)
            && e.provenance == Provenance::Assumed
            && e.fresh(now_unix)
        {
            tracing::info!(
                peer,
                assumed_bps = e.stable_bps,
                age_s = now_unix.saturating_sub(e.at_unix),
                "FR-148 P1 rate memory: the pair's entry has no measured provenance, so it is read \
                 as assumed: no seed and no slow-link profile, the pair opens as one never seen"
            );
        }
        self.seed_for(peer, now_unix)
    }

    /// Record a measured `stable_bps` for `peer`. The file is written whole,
    /// so this also drops every entry that may not be written again: an
    /// expired one, and an assumed one (FR-148 §4.1: never stored).
    pub fn record(&mut self, peer: &str, stable_bps: u32, now_unix: u64) {
        self.entries
            .retain(|_, e| e.provenance == Provenance::Measured && e.fresh(now_unix));
        self.entries.insert(
            peer.to_string(),
            Entry {
                stable_bps,
                at_unix: now_unix,
                provenance: Provenance::Measured,
            },
        );
    }

    /// FR-79 V4 — record what this session MEASURED about the carrier.
    ///
    /// `evidence` is the session's own measurement — the belief the validity
    /// gate accepted (`RateGovernor::pipe_bps`), or the rate the ceiling
    /// learner proved the pair carried. `None` (or zero) means the session
    /// measured nothing, and then **nothing is written**: not the value, not
    /// the timestamp, because refreshing a timestamp on no evidence keeps a
    /// stale number alive past its TTL.
    ///
    /// A measurement moves the entry in EITHER direction, damped with the same
    /// asymmetry the goodput estimator uses inside a session — believe a drop
    /// quickly, a rise slowly. Returns the measured value now on record. It is
    /// 0 when there is none: no entry, a stale one or an assumed one. The
    /// write-back then saves nothing.
    ///
    /// ⚠️ Until FR-79 V4 this kept the **maximum** of the old value, the
    /// learner's rate and an opener "growth target", and a lower value needed a
    /// `had_decrease` flag to be accepted at all. That rule was a proxy for
    /// "do not let a non-measurement lower the memory", written when the code
    /// could not tell a measurement from a non-measurement; the gate now can,
    /// so the proxy goes. It had two measured costs: a carrier whose capacity
    /// moved 1.09 → 6.13 Mbps in six minutes was remembered at 6.13 (2026-09-08,
    /// `100.65.4.2|relay:derp/tcp`), and the opener's "a burst that never
    /// queued proves not-slower-than-this" step ratcheted to the `hi` cap on any
    /// host whose socket absorbs the burst — every opener on CORPLAP-2 that day.
    pub fn record_session(&mut self, peer: &str, evidence: Option<u32>, now_unix: u64) -> u32 {
        let Some(measured) = evidence.filter(|m| *m > 0) else {
            return self.seed_for(peer, now_unix).unwrap_or(0);
        };
        let value = match self.seed_for(peer, now_unix) {
            // First evidence for this pair and carrier, or the first after an
            // assumed entry: adopt it outright. Seeding from a constant, or
            // damping toward a placeholder, would bias every later reading
            // toward a number nothing measured.
            None => measured,
            Some(old) => damp(old, measured),
        };
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
            Some(8_000_000),
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

    /// FR-79 V4, the field series that questioned the rule — CORPLAP-1 on
    /// `100.65.4.2|relay:derp/tcp`, five sessions inside six minutes on
    /// 2026-09-08: 1.44, 1.09, (a session that measured nothing), 3.32, 6.13
    /// Mbps. The MAXIMUM keeps 6.13 and opens the next session at 5.2 M into a
    /// path that measured 1.09 M five minutes earlier; damped, the memory ends
    /// near where that carrier actually lived.
    #[test]
    fn the_memory_follows_the_measurements_instead_of_their_maximum() {
        let now = 1_788_000_000u64;
        let key = memory_key("100.65.4.2", Some("relay:derp/tcp"));
        let mut m = RateMemory::default();
        // First evidence is adopted outright.
        assert_eq!(m.record_session(&key, Some(1_440_672), now), 1_440_672);
        // A drop is believed quickly (ALPHA_DOWN = 0.5).
        assert_eq!(m.record_session(&key, Some(1_087_230), now + 50), 1_263_951);
        // A session that measured nothing writes nothing at all — not the
        // value, not the timestamp.
        let before = m.entries[&key].clone();
        assert_eq!(m.record_session(&key, None, now + 100), 1_263_951);
        assert_eq!(m.entries[&key], before, "no evidence, no write");
        // A rise is believed slowly (ALPHA_UP = 0.1), twice.
        assert_eq!(
            m.record_session(&key, Some(3_322_780), now + 150),
            1_469_833
        );
        assert_eq!(
            m.record_session(&key, Some(6_131_302), now + 200),
            1_935_979
        );
        // The old rule would have kept the best minute of the six.
        assert!(
            m.seed_for(&key, now + 200).unwrap() < 2_000_000,
            "the maximum would have remembered 6.13 M"
        );
    }

    /// A pair with no entry and a session that measured nothing leaves the
    /// memory empty rather than writing a zero.
    #[test]
    fn a_session_that_measured_nothing_writes_no_entry() {
        let now = 1_788_000_000u64;
        let mut m = RateMemory::default();
        assert_eq!(m.record_session("p", None, now), 0);
        assert_eq!(m.record_session("p", Some(0), now), 0);
        assert!(m.entries.is_empty());
    }

    /// FR-148 P1 — a memory file as every agent before this phase wrote it:
    /// `save`'s shape, with no provenance on any entry.
    fn pre_phase_file(test: &str, entries: &[(&str, u32, u64)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rate-memory-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut map = serde_json::Map::new();
        for (key, bps, at) in entries {
            map.insert(
                (*key).to_string(),
                serde_json::json!({ "stable_bps": bps, "at_unix": at }),
            );
        }
        let path = dir.join(FILE_NAME);
        let body = serde_json::json!({ "entries": map });
        std::fs::write(&path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
        path
    }

    /// A pair on the mesh relay, keyed as FR-79 V2 keys it.
    const PAIR: &str = "100.65.4.2|relay:derp/tcp";

    /// FR-148 P1 step 2 — an entry from before this phase is read as assumed.
    /// 281,732 bit/s is what the operator's pair was remembered at on
    /// 2026-11-16, grown from a placeholder written before V5b, and the
    /// sessions such an entry opens can only creep it (FR-148 §2.1). An
    /// assumed entry seeds nothing: the session opens as a pair never seen,
    /// with no slow-link profile and no opening below the floor.
    #[test]
    fn an_entry_from_before_this_phase_is_read_as_assumed() {
        let now = 1_793_000_000u64;
        let path = pre_phase_file("assumed", &[(PAIR, 281_732, now - 3_600)]);
        let mut m = RateMemory::load(&path);
        assert_eq!(m.entries.len(), 1, "the old file still reads");
        assert_eq!(
            m.seed_for(PAIR, now),
            None,
            "an assumed entry seeds nothing"
        );
        assert_eq!(m.seed_at_open(PAIR, now), None);
        assert_eq!(
            m.entries[PAIR].provenance,
            Provenance::Assumed,
            "no field reads as assumed"
        );
        // A session that measured nothing has nothing on record to keep: the
        // write-back saves nothing, and the entry stays as it was until it
        // ages out.
        let before = m.clone();
        assert_eq!(m.record_session(PAIR, None, now), 0);
        assert_eq!(m, before, "no evidence, no write");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The first measurement after an assumed entry replaces it outright. It
    /// is not damped toward the placeholder: rising a tenth of the way from
    /// 281,732 would have kept the pair at 454,086 bit/s.
    #[test]
    fn the_first_measurement_replaces_an_assumed_entry_outright() {
        let now = 1_793_000_000u64;
        let path = pre_phase_file("replaced", &[(PAIR, 281_732, now - 3_600)]);
        let mut m = RateMemory::load(&path);
        // What the same carrier measured for another viewer that day.
        assert_eq!(m.record_session(PAIR, Some(2_005_279), now), 2_005_279);
        assert_eq!(m.seed_for(PAIR, now), Some(2_005_279));
        assert_eq!(m.seed_at_open(PAIR, now), Some(2_005_279));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// AC7 — nothing assumed is written to the pair memory. The file is
    /// written whole, so a write for one pair must not carry another pair's
    /// assumed entry along with it. What is written is marked measured.
    #[test]
    fn nothing_assumed_is_written_back() {
        let now = 1_793_000_000u64;
        let other = memory_key("100.65.0.5", Some("direct"));
        let path = pre_phase_file(
            "written",
            &[
                (PAIR, 281_732, now - 3_600),
                (other.as_str(), 8_000_000, now - 60),
            ],
        );
        let mut m = RateMemory::load(&path);
        m.record_session(PAIR, Some(2_005_279), now);
        m.save(&path).expect("save");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains(&other),
            "the other pair's assumed entry is not written again:\n{text}"
        );
        assert!(
            text.contains("\"provenance\": \"measured\""),
            "a written entry says how it was obtained:\n{text}"
        );
        let back = RateMemory::load(&path);
        assert_eq!(back.seed_for(PAIR, now), Some(2_005_279));
        assert_eq!(back.seed_for(&other, now), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A provenance this agent does not know, written by a later one, is read
    /// as assumed, and the file still reads. A value nobody here can vouch
    /// for opens nothing.
    #[test]
    fn a_provenance_this_agent_does_not_know_is_read_as_assumed() {
        let now = 1_793_000_000u64;
        let json = format!(
            r#"{{"entries":{{"{PAIR}":{{"stable_bps":5000000,"at_unix":{now},"provenance":"measured-backlogged"}}}}}}"#
        );
        let m: RateMemory = serde_json::from_str(&json).expect("the file still reads");
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.seed_for(PAIR, now), None);
    }

    /// The door opens both ways. An older agent reads this file, because its
    /// entry does not deny unknown fields, and writes it back whole without
    /// the field. After a re-upgrade those entries are read as assumed, which
    /// costs one session of learning per pair and carrier.
    #[test]
    fn a_downgrade_reads_the_file_and_its_rewrite_reads_back_as_assumed() {
        /// The entry of every agent before this phase.
        #[derive(Serialize, Deserialize)]
        struct OldEntry {
            stable_bps: u32,
            at_unix: u64,
        }
        #[derive(Serialize, Deserialize)]
        struct OldMemory {
            #[serde(default)]
            entries: BTreeMap<String, OldEntry>,
        }
        let now = 1_793_000_000u64;
        let mut m = RateMemory::default();
        m.record_session(PAIR, Some(1_440_672), now);
        let bytes = serde_json::to_vec_pretty(&m).unwrap();
        let old: OldMemory = serde_json::from_slice(&bytes).expect("an older agent reads the file");
        assert_eq!(old.entries[PAIR].stable_bps, 1_440_672);
        let rewritten = serde_json::to_vec_pretty(&old).unwrap();
        let back: RateMemory = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.seed_for(PAIR, now), None);
    }
}
