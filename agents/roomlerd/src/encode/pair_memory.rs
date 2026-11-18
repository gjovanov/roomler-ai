// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-148 P1 steps 2 to 4 — the pair memory stores the path model, under a
//! key that names the path, and under `on` the opener reads it (spec §8, D22
//! to D28). Pure: no clock read, no I/O but the file's own load and save.
//!
//! | Part | What it is |
//! |---|---|
//! | the key ([`pair_key`]) | `v2|<carrier>|u:<controlling user>|<viewer network>`. The relay worker's address is not in it (AC8) |
//! | the file ([`PathMemory`]) | `path_memory.json`, beside today's `rate_memory.json`, which is written exactly as before; one lock in the process around its read-modify-write ([`PathMemory::update`]) |
//! | an entry ([`PairEntry`]) | the model's floor, and a push-back record of its own (D25) |
//! | the session ([`SessionMemory`]) | the floor law from the entry's floor, and the record, fed only by counted samples of valid windows (D24, R1 to R3) |
//! | the close ([`close`]) | what a session writes, case by case, and why |
//! | the opening ([`model_opening`]) | what the model opens at (D25) and whether the slow-link profile engages (D26) |
//! | the decision ([`Taken`], [`OpenPlan`]) | which rule opens the session: the model's under `on` with a key and a fresh entry, else today's, and why |
//!
//! ⚠️ Under `off` and `shadow` (the default, D28) nothing here drives a rate:
//! the opener reads today's `rate_memory.json`, byte for byte.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::ceiling_learn::{CONTRADICTION_FACTOR, SEED_PCT};
use super::pipe::{Basis, PathModel};
use super::pipe_state::PipeState;
use crate::session_carrier::{CandidateKind, SessionCarrier};

/// The model's own file, in the directory of `rate_memory.json`. An earlier
/// release never opens it, so a rollback leaves it whole (D23).
pub const FILE_NAME: &str = "path_memory.json";
/// The file's version. Any other reads as an empty memory, and the next write
/// replaces the file.
pub const VERSION: u64 = 1;
/// A floor lives 7 days from the reading that set it, as today's entries do.
pub const FLOOR_TTL: Duration = super::rate_memory::TTL;
/// A push-back record whose newest sample is older than this is dropped at the
/// read and is never trusted (D25).
pub const RECORD_TTL: Duration = Duration::from_secs(24 * 3600);
/// D25 — a record is trusted from this many counted windows on ...
pub const TRUST_WINDOWS: u32 = 3;
/// ... spread over at least this long, first sample to newest.
pub const TRUST_SPAN: Duration = Duration::from_secs(10);
/// D23 — the rates an entry may hold. 50 kbit/s is the lower clamp of the
/// slow-link floor (`encode::slow_link_min_bitrate_bps`).
pub const MIN_BPS: u32 = 50_000;
pub const MAX_BPS: u32 = 100_000_000;
/// D23 — a time further ahead than this is a clock that jumped: the entry drops.
pub const FUTURE_SLACK: Duration = Duration::from_secs(300);
/// D25 — the lowest opening a trusted push-back may lead to.
pub const OPEN_MIN_BPS: u32 = 200_000;

// ---------------------------------------------------------------------------
// The key (D22)
// ---------------------------------------------------------------------------

/// What the key's network part is made from. The lines carry this word, never
/// the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetKind {
    /// The viewer's server-reflexive address, masked to /24 or /48.
    Srflx,
    /// The selected remote, when it is the viewer's own address, masked.
    Remote,
    /// The viewer's overlay address: a device, not a network.
    Overlay,
    /// None of the three.
    Unknown,
}

impl NetKind {
    pub fn word(self) -> &'static str {
        match self {
            NetKind::Srflx => "srflx",
            NetKind::Remote => "remote",
            NetKind::Overlay => "overlay",
            NetKind::Unknown => "unknown",
        }
    }
}

/// Why a session has no key, so nothing is read or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoKey {
    /// No selected pair within the pump's wait.
    NoPair,
    /// The carrier is unknown, or mid-churn.
    CarrierUnknown,
    /// No controlling user: an anonymous key would merge every viewer.
    NoUser,
    /// The constrained ceiling learner is off (`relay_max_hi_bps` is 0).
    LearnerOff,
}

impl NoKey {
    pub fn word(self) -> &'static str {
        match self {
            NoKey::NoPair => "no_pair",
            NoKey::CarrierUnknown => "carrier_unknown",
            NoKey::NoUser => "no_user",
            NoKey::LearnerOff => "learner_off",
        }
    }
}

/// The key, which stays in the local file. A line carries [`Self::id`],
/// [`Self::carrier`] and [`Self::net`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairKey {
    text: String,
    carrier: String,
    net: NetKind,
}

impl PairKey {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn carrier(&self) -> &str {
        &self.carrier
    }

    pub fn net(&self) -> NetKind {
        self.net
    }

    /// The first 12 hex of the key's SHA-256: what a line carries.
    pub fn id(&self) -> String {
        let digest = Sha256::digest(self.text.as_bytes());
        hex::encode(digest)[..12].to_string()
    }
}

/// What the pump knows when it opens, from which the key is made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyFacts {
    /// FR-102's carrier for the selected pair (`session_carrier::carrier_of`).
    pub carrier: Option<SessionCarrier>,
    /// The overlay's relay transport for an overlay relay carrier, as LocalAPI
    /// names it (`udp`, `tcp`).
    pub overlay_transport: Option<String>,
    /// The controlling user's id, 24 hex.
    pub user: Option<String>,
    /// The viewer's server-reflexive candidate addresses, as it trickled them.
    pub srflx: Vec<IpAddr>,
    /// The selected pair's remote candidate.
    pub remote: Option<(CandidateKind, IpAddr)>,
}

/// D22 — the carrier's word in the key. FR-102's words, which never give an
/// overlay carrier and any other the same word; an overlay relay adds its
/// transport when the overlay names it, as FR-79 V2's key did. `unknown` has
/// no word: no key.
pub fn carrier_word(carrier: SessionCarrier, overlay_transport: Option<&str>) -> Option<String> {
    use SessionCarrier as C;
    let word = carrier.as_str();
    match carrier {
        C::Unknown => None,
        C::OverlayTurn | C::Derp | C::OrgRelay => Some(
            match overlay_transport
                .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric()))
            {
                Some(t) => format!("{word}/{}", t.to_ascii_lowercase()),
                None => word.to_string(),
            },
        ),
        C::Direct | C::Turn | C::TurnLocal | C::OverlayDirect => Some(word.to_string()),
    }
}

fn is_overlay(ip: IpAddr) -> bool {
    crate::peer::addr_is_overlay_range(&ip.to_string())
}

/// A private, loopback, link-local or shared address: not the viewer's public
/// face.
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || (o[0] == 100 && (o[1] & 0b1100_0000) == 0b0100_0000)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

/// The address masked to /24 (IPv4) or /48 (IPv6).
fn masked(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2])
        }
    }
}

/// D22 — the viewer's network: the first of these that exists.
///
/// 1. Its server-reflexive candidates that are not overlay addresses, a public
///    one before a private one, then the lowest IPv4, else the lowest IPv6.
/// 2. The selected remote, when it is the viewer's own address: not a relay
///    candidate, not an overlay address.
/// 3. The selected remote when it is an overlay address, whole: it names a
///    device, not a network.
/// 4. Unknown.
pub fn viewer_net(srflx: &[IpAddr], remote: Option<(CandidateKind, IpAddr)>) -> (NetKind, String) {
    let pick = |public: bool| {
        let class = srflx
            .iter()
            .copied()
            .filter(|ip| !is_overlay(*ip) && is_private(*ip) != public);
        let v4 = class.clone().filter(IpAddr::is_ipv4).min();
        v4.or_else(|| class.filter(IpAddr::is_ipv6).min())
    };
    if let Some(ip) = pick(true).or_else(|| pick(false)) {
        return (NetKind::Srflx, format!("n:{}", masked(ip)));
    }
    match remote {
        Some((_, ip)) if is_overlay(ip) => (NetKind::Overlay, format!("o:{ip}")),
        Some((kind, ip)) if kind != CandidateKind::Relay => {
            (NetKind::Remote, format!("n:{}", masked(ip)))
        }
        _ => (NetKind::Unknown, "n:?".to_string()),
    }
}

/// D22 — the key, or why there is none.
pub fn pair_key(facts: &KeyFacts) -> Result<PairKey, NoKey> {
    if facts.remote.is_none() {
        return Err(NoKey::NoPair);
    }
    let carrier = facts
        .carrier
        .and_then(|c| carrier_word(c, facts.overlay_transport.as_deref()))
        .ok_or(NoKey::CarrierUnknown)?;
    let user = facts
        .user
        .as_deref()
        .filter(|u| u.len() == 24 && u.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or(NoKey::NoUser)?;
    let (net, part) = viewer_net(&facts.srflx, facts.remote);
    Ok(PairKey {
        text: format!("v2|{carrier}|u:{}|{part}", user.to_ascii_lowercase()),
        carrier,
        net,
    })
}

// ---------------------------------------------------------------------------
// An entry and the file (D23)
// ---------------------------------------------------------------------------

/// A stored floor's class. Only the two measured classes exist: nothing
/// assumed can be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FloorBasis {
    Backlogged,
    AppLimited,
}

impl FloorBasis {
    /// `None` for an assumed value, which is never stored.
    pub fn of(basis: Basis) -> Option<Self> {
        match basis {
            Basis::Backlogged => Some(FloorBasis::Backlogged),
            Basis::AppLimited => Some(FloorBasis::AppLimited),
            Basis::Assumed => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            FloorBasis::Backlogged => "backlogged",
            FloorBasis::AppLimited => "app_limited",
        }
    }
}

/// The model's floor: the value, its class, and the time of the reading that
/// set it (not the time of the close).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Floor {
    pub bps: u32,
    pub basis: FloorBasis,
    pub at_unix: u64,
}

/// D25 — the push-back record: `bps` is the highest sample it counted,
/// `at_unix` the time of its newest counted sample, `first_at_unix` of its
/// first, `windows` how many it counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushBack {
    pub bps: u32,
    pub at_unix: u64,
    pub first_at_unix: u64,
    pub windows: u32,
}

impl PushBack {
    /// Seconds from its first counted sample to its newest.
    pub fn span_s(&self) -> u64 {
        self.at_unix.saturating_sub(self.first_at_unix)
    }

    /// Its newest sample is at most [`RECORD_TTL`] old.
    pub fn fresh(&self, now_unix: u64) -> bool {
        now_unix.saturating_sub(self.at_unix) <= RECORD_TTL.as_secs()
    }
}

/// One pair's entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairEntry {
    pub floor: Floor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_back: Option<PushBack>,
}

impl PairEntry {
    /// The floor is at most [`FLOOR_TTL`] old.
    pub fn fresh(&self, now_unix: u64) -> bool {
        now_unix.saturating_sub(self.floor.at_unix) <= FLOOR_TTL.as_secs()
    }
}

/// D25 — a record is trusted when its newest sample is at most a day old, it
/// counted at least [`TRUST_WINDOWS`] windows, and its newest sample is at
/// least [`TRUST_SPAN`] after its first. One sample is never enough.
pub fn trusted(r: &PushBack, now_unix: u64) -> bool {
    r.fresh(now_unix) && r.windows >= TRUST_WINDOWS && r.span_s() >= TRUST_SPAN.as_secs()
}

/// Why an entry on disk was dropped at the read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// A rate out of range, a class this agent does not know, a field missing
    /// or of the wrong type, a record whose first sample is after its newest.
    Invalid,
    /// A time more than [`FUTURE_SLACK`] ahead.
    Future,
}

/// What the open line says of the pair's entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    None,
    Fresh,
    Expired,
    Future,
    Invalid,
}

impl EntryState {
    pub fn word(self) -> &'static str {
        match self {
            EntryState::None => "none",
            EntryState::Fresh => "fresh",
            EntryState::Expired => "expired",
            EntryState::Future => "future",
            EntryState::Invalid => "invalid",
        }
    }
}

/// D23 — one entry, checked on its own. A record older than a day is dropped
/// here and the entry kept (D25).
fn check(entry: PairEntry, now_unix: u64) -> Result<PairEntry, Dropped> {
    let in_range = |bps: u32| (MIN_BPS..=MAX_BPS).contains(&bps);
    let ahead = |at: u64| at > now_unix.saturating_add(FUTURE_SLACK.as_secs());
    if !in_range(entry.floor.bps) {
        return Err(Dropped::Invalid);
    }
    if ahead(entry.floor.at_unix) {
        return Err(Dropped::Future);
    }
    let mut entry = entry;
    if let Some(p) = entry.push_back {
        if !in_range(p.bps) || p.windows == 0 || p.first_at_unix > p.at_unix {
            return Err(Dropped::Invalid);
        }
        if ahead(p.at_unix) {
            return Err(Dropped::Future);
        }
        if !p.fresh(now_unix) {
            entry.push_back = None;
        }
    }
    Ok(entry)
}

/// The model's memory: one entry per key, each checked on its own when read.
/// A cache, never a source of truth: a missing or unreadable file, or another
/// version, is an empty memory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PathMemory {
    pub pairs: BTreeMap<String, PairEntry>,
    /// The keys whose entry was dropped at the read, and why. Never written.
    pub dropped: BTreeMap<String, Dropped>,
}

#[derive(Serialize)]
struct FileOut<'a> {
    v: u64,
    pairs: &'a BTreeMap<String, PairEntry>,
}

impl PathMemory {
    /// Read from the file's bytes at `now_unix`.
    pub fn parse(bytes: &[u8], now_unix: u64) -> Self {
        let mut memory = Self::default();
        let Ok(raw) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return memory;
        };
        if raw.get("v").and_then(serde_json::Value::as_u64) != Some(VERSION) {
            return memory;
        }
        let Some(pairs) = raw.get("pairs").and_then(serde_json::Value::as_object) else {
            return memory;
        };
        for (key, value) in pairs {
            let read = serde_json::from_value::<PairEntry>(value.clone())
                .map_err(|_| Dropped::Invalid)
                .and_then(|e| check(e, now_unix));
            match read {
                Ok(entry) => {
                    memory.pairs.insert(key.clone(), entry);
                }
                Err(why) => {
                    memory.dropped.insert(key.clone(), why);
                }
            }
        }
        memory
    }

    /// Missing or unreadable ⇒ empty.
    pub fn load(path: &Path, now_unix: u64) -> Self {
        std::fs::read(path).map_or_else(|_| Self::default(), |b| Self::parse(&b, now_unix))
    }

    /// The entry the session opens from: present and its floor fresh.
    pub fn fresh(&self, key: &str, now_unix: u64) -> Option<&PairEntry> {
        self.pairs.get(key).filter(|e| e.fresh(now_unix))
    }

    pub fn state(&self, key: &str, now_unix: u64) -> EntryState {
        match (self.pairs.get(key), self.dropped.get(key)) {
            (Some(e), _) if e.fresh(now_unix) => EntryState::Fresh,
            (Some(_), _) => EntryState::Expired,
            (None, Some(Dropped::Future)) => EntryState::Future,
            (None, Some(Dropped::Invalid)) => EntryState::Invalid,
            (None, None) => EntryState::None,
        }
    }

    /// Put `entry` under `key`. The file is written whole, so every expired
    /// entry goes with it.
    pub fn write(&mut self, key: &str, entry: PairEntry, now_unix: u64) {
        self.pairs.retain(|_, e| e.fresh(now_unix));
        self.pairs.insert(key.to_string(), entry);
    }

    /// The file's bytes: `{"v": 1, "pairs": {...}}`.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(&FileOut {
            v: VERSION,
            pairs: &self.pairs,
        })
        .unwrap_or_default()
    }

    /// Whole-file atomic write: temp sibling + rename, as `rate_memory` does.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, self.to_bytes())?;
        std::fs::rename(&tmp, path)
    }

    /// Read the file, change it and save it, under the one lock this process
    /// holds for it. Two pumps that end at once would otherwise write the
    /// same temp sibling, and the later save would drop the key the earlier
    /// one wrote. Two sessions of ONE key still end as the later close says.
    pub fn update(
        path: &Path,
        now_unix: u64,
        change: impl FnOnce(&mut PathMemory),
    ) -> std::io::Result<()> {
        let _held = FILE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let mut memory = Self::load(path, now_unix);
        change(&mut memory);
        memory.save(path)
    }
}

/// The lock around [`PathMemory::update`]: one per process (ruling 2).
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// The file's location: the directory of `rate_memory.json`.
pub fn default_path() -> Option<PathBuf> {
    let dirs = crate::appdirs::project_dirs()?;
    Some(dirs.data_dir().join(FILE_NAME))
}

// ---------------------------------------------------------------------------
// The opening the model would take (D25, D26)
// ---------------------------------------------------------------------------

/// What the model would open a session at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelOpening {
    pub bps: u32,
    pub profile: bool,
    pub trusted: bool,
}

/// D25, D26 — with N `nominal`, hi the relay cap, D the entry's floor, P its
/// record's rate and T the slow-link threshold (0 = never):
///
/// ```text
/// O       = if trusted { clamp(⌊0.85·D⌋, 200 k, hi) } else { max(N, min(⌊0.85·D⌋, hi)) }
/// profile = trusted && P ≤ T && O ≤ T
/// ```
///
/// No entry, an expired floor, or an entry that failed its check (it is not
/// in the memory then): O = N and no profile. The open line logs it in every
/// mode; under `on` it decides the opening ([`Taken`]).
pub fn model_opening(
    nominal: u32,
    hi: u32,
    entry: Option<&PairEntry>,
    threshold: u32,
    now_unix: u64,
) -> ModelOpening {
    let none = ModelOpening {
        bps: nominal,
        profile: false,
        trusted: false,
    };
    let Some(e) = entry.filter(|e| e.fresh(now_unix)) else {
        return none;
    };
    let scaled = u32::try_from(u64::from(e.floor.bps) * SEED_PCT / 100).unwrap_or(u32::MAX);
    let record = e.push_back.filter(|p| trusted(p, now_unix));
    let bps = match record {
        // `hi` under the lower clamp cannot occur (no key without a learner);
        // the clamp then stands, rather than a zero.
        Some(_) => scaled.max(OPEN_MIN_BPS).min(hi.max(OPEN_MIN_BPS)),
        None => nominal.max(scaled.min(hi)),
    };
    let profile = record.is_some_and(|p| threshold > 0 && p.bps <= threshold && bps <= threshold);
    ModelOpening {
        bps,
        profile,
        trusted: record.is_some(),
    }
}

// ---------------------------------------------------------------------------
// Which rule opens the session (step 3)
// ---------------------------------------------------------------------------

/// FR-148 P1 step 3 — what a constrained FFmpeg-pump session opens from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPlan {
    /// Today's rule: the measured seed of `rate_memory.json`, if any (FR-35,
    /// FR-59 P8). `off` and `shadow` always open from it.
    Today(Option<u32>),
    /// The model's (D25): the entry's floor D seeds the ceiling learner, the
    /// shadow controller and the P0 model, so the ceiling at the first tick
    /// is `max(N, min(⌊0.85·D⌋, hi))`; after a trusted push-back the session
    /// opens at `open_at_bps`, which may sit under the nominal.
    Model {
        floor_bps: u32,
        open_at_bps: Option<u32>,
    },
}

impl OpenPlan {
    /// The model's plan for `opening`, computed on `entry`.
    pub fn model(entry: &PairEntry, opening: ModelOpening) -> Self {
        OpenPlan::Model {
            floor_bps: entry.floor.bps,
            open_at_bps: opening.trusted.then_some(opening.bps),
        }
    }

    /// `(seed, open_at)` as the governor takes them: the seed of the ceiling
    /// learner, the shadow controller and the P0 model; and where the AIMD,
    /// the first encoder and slow start open when that sits under the
    /// ceiling.
    ///
    /// Today's is FR-59 P8 exactly: the seed when it is under the nominal
    /// floor and the floor relief is on. The model's opening is taken as it
    /// is, except that with the relief off one under the nominal floor is
    /// lifted to it: the AIMD's floor would lift it at the first tick, and an
    /// encoder opened under it would be rebuilt right there.
    pub fn resolve(&self, floor_relief: bool) -> (Option<u32>, Option<u32>) {
        let floor = super::MIN_BITRATE_BPS;
        match *self {
            OpenPlan::Today(seed) => (seed, seed.filter(|s| *s > 0 && *s < floor && floor_relief)),
            OpenPlan::Model {
                floor_bps,
                open_at_bps,
            } => (
                Some(floor_bps),
                open_at_bps.map(|o| if floor_relief { o } else { o.max(floor) }),
            ),
        }
    }
}

/// Why today's rule opened the session: under `on`, why the model did not
/// (ruling 3); under `shadow`, the switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodayWhy {
    Shadow,
    NoKey,
    NoEntry,
    EntryInvalid,
    FloorExpired,
}

impl TodayWhy {
    pub fn word(self) -> &'static str {
        match self {
            TodayWhy::Shadow => "shadow",
            TodayWhy::NoKey => "no_key",
            TodayWhy::NoEntry => "no_entry",
            TodayWhy::EntryInvalid => "entry_invalid",
            TodayWhy::FloorExpired => "floor_expired",
        }
    }
}

/// Which rule opened the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// The model's opening (D25) and profile (D26).
    Model,
    /// Today's rule, and why.
    Today(TodayWhy),
}

impl Taken {
    /// The model decides under `on` when the session has a key and its entry
    /// is fresh and passed its check. Anything else opens by today's rule:
    /// no entry, an entry dropped at the read (a bad value or a time ahead),
    /// a floor older than its TTL, or no key.
    pub fn decide(on: bool, keyed: bool, state: EntryState) -> Self {
        let why = match state {
            _ if !on => TodayWhy::Shadow,
            _ if !keyed => TodayWhy::NoKey,
            EntryState::Fresh => return Taken::Model,
            EntryState::None => TodayWhy::NoEntry,
            EntryState::Invalid | EntryState::Future => TodayWhy::EntryInvalid,
            EntryState::Expired => TodayWhy::FloorExpired,
        };
        Taken::Today(why)
    }

    pub fn word(self) -> &'static str {
        match self {
            Taken::Model => "model",
            Taken::Today(_) => "today",
        }
    }

    /// `-` for the model's; today's reason otherwise.
    pub fn why(self) -> &'static str {
        match self {
            Taken::Model => "-",
            Taken::Today(why) => why.word(),
        }
    }
}

// ---------------------------------------------------------------------------
// The session: what it measured, and what its close writes (D24, D25)
// ---------------------------------------------------------------------------

/// FR-148 P1 (R1) — could the classifier READ this window? The path memory has
/// one kind of evidence that the path pushed back, a counted sample, and a
/// sample counts only from a window whose state the classifier read: the
/// sender's own counters said `Overproduced`, or an age split said anything
/// else on an age that is a reading ([`super::viewer_rate::age_is_plausible`]).
/// `Unknown` is a window it could not read (no age, or no arrival stamp, to
/// split), and the gate accepts it. A `TransitStalled` verdict never reaches a
/// sample: the gate rejects that window and the one after it.
///
/// Inside the session such a window still acts exactly as before: a wrong cut
/// is undone in seconds. The memory's record stands for a day and decides the
/// next opening, so it does not take one.
pub fn window_read(state: PipeState, age_plausible: bool) -> bool {
    match state {
        PipeState::Unknown => false,
        PipeState::Overproduced => true,
        PipeState::Clear | PipeState::TransitStalled | PipeState::ViewerLate => age_plausible,
    }
}

/// FR-148 P1 (R3) — the plan-limited test: was the sender pushing against its
/// plan in this window? Yes when it wrote at least half the target the window
/// ran with (`sent_bps`, `target_bps`), and in any window the classifier calls
/// `Overproduced`, whose own counters show the sender pushing against the path
/// (what it wrote there is what the path took, not what the encoder made). A
/// target of 0, none applied yet, passes.
///
/// A still screen writes tens of kbit/s under a target of megabits. What
/// arrives then measures the screen, not the path, and with a viewer whose
/// paint age stays flat while its link report says the queue grows (R3's V2,
/// `governor::tests::the_screens_families_on_r1_r2_and_the_plan_limited_test`) the
/// classifier reads such a window `Clear`: before this test a still screen's
/// slowdowns left a trusted record at a few hundred kbit/s.
pub fn pushing_against_plan(state: PipeState, sent_bps: u32, target_bps: u32) -> bool {
    state == PipeState::Overproduced || u64::from(sent_bps) * 2 >= u64::from(target_bps)
}

/// FR-148 P1 — what the pair memory takes from a push-back in a valid window.
/// One definition of counted evidence: a sample counts only from a window the
/// classifier read (R1) in which the sender pushed against its plan (R3), and
/// then only when D25's test counts it. Inside the session every push-back acts
/// exactly as before, whatever this says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// A sample: D25 counts it, or calls it a dip.
    Sample,
    /// R1 — the classifier could not read the window ([`window_read`]).
    Unread,
    /// R3 — the sender wrote under half its target ([`pushing_against_plan`]).
    UnderPlan,
}

impl Take {
    /// The window's verdict, R1 first.
    pub fn of(state: PipeState, age_plausible: bool, sent_bps: u32, target_bps: u32) -> Self {
        if !window_read(state, age_plausible) {
            Take::Unread
        } else if !pushing_against_plan(state, sent_bps, target_bps) {
            Take::UnderPlan
        } else {
            Take::Sample
        }
    }
}

/// The samples a session counted itself, since the last end of the record.
#[derive(Debug, Clone, Copy)]
struct Own {
    bps: u32,
    first: Instant,
    newest: Instant,
    windows: u32,
}

/// One session's side of the memory. Fed by `Pipe` with the readings of valid
/// windows only (D24); opened from the pair's entry. Nothing reads it to drive
/// a rate.
#[derive(Debug, Default, Clone, Copy)]
pub struct SessionMemory {
    /// The floor law (`PathModel`), from the entry's floor as remembered.
    floor: PathModel,
    /// The entry's record, while it stands (fresh at the open, not ended).
    carried: Option<PushBack>,
    own: Option<Own>,
    /// The highest delivery this session measured in a valid window.
    best: Option<(u32, Basis)>,
    /// Backlogged push-backs under half of `best`: dips, not counted.
    not_counted: u32,
    /// R1 — backlogged push-backs from windows the classifier could not read
    /// ([`window_read`]): never offered to the record.
    unread: u32,
    /// R3 — backlogged push-backs from windows where the sender wrote under
    /// half its target ([`pushing_against_plan`]): never offered to the record.
    under_plan: u32,
    /// R2 — counted samples in the whole session, whether the record ended
    /// since or not: the close lowers a remembered floor only when this is
    /// above zero.
    counted: u32,
    /// A delivery ended the record in this session.
    ended: bool,
    /// A valid window gave any measured reading.
    measured: bool,
    /// Test only: the rule D25 corrected, "a delivery above the floor ends the
    /// record", to show the governor cell fails on it.
    #[cfg(test)]
    end_above_floor: bool,
    /// Test only: master's law before R1, every push-back offered to the
    /// record, to show the screen's cell fails on it.
    #[cfg(test)]
    read_any: bool,
    /// Test only: the law before R3, a push-back under the plan offered to the
    /// record, to show the screen's families fail on it.
    #[cfg(test)]
    plan_any: bool,
}

/// What a close would write, taken at a viewer window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closing {
    /// The floor the session's own readings set; `None` while the floor is
    /// still the remembered one, or nothing was measured.
    pub floor: Option<Floor>,
    /// The record as it stands: the entry's, with this session's samples.
    pub record: Option<PushBack>,
    pub best: Option<(u32, FloorBasis)>,
    pub not_counted: u32,
    /// R1 — samples from windows the classifier could not read.
    pub unread: u32,
    /// R3 — samples from windows where the sender wrote under half its target.
    pub under_plan: u32,
    /// R2 — counted samples in the whole session.
    pub counted: u32,
    pub ended: bool,
    pub measured: bool,
    /// The Unix second this was taken: the close reads the entry's TTL at it.
    pub at_unix: u64,
}

/// `at`, `now` and `now_unix` read together: `at`'s Unix second.
fn unix_of(at: Instant, now: Instant, now_unix: u64) -> u64 {
    now_unix.saturating_sub(now.saturating_duration_since(at).as_secs())
}

impl SessionMemory {
    /// Open from the pair's entry: its floor as remembered while fresh, and its
    /// record while its newest sample is at most a day old.
    pub fn open(&mut self, entry: Option<&PairEntry>, now_unix: u64, now: Instant) {
        *self = Self {
            #[cfg(test)]
            end_above_floor: self.end_above_floor,
            #[cfg(test)]
            read_any: self.read_any,
            #[cfg(test)]
            plan_any: self.plan_any,
            ..Self::default()
        };
        if let Some(e) = entry.filter(|e| e.fresh(now_unix)) {
            self.floor.remembered(e.floor.bps, now);
            self.carried = e.push_back.filter(|p| p.fresh(now_unix));
        }
    }

    /// The record's rate while it stands.
    fn standing_bps(&self) -> Option<u32> {
        match (self.carried, self.own) {
            (Some(c), Some(o)) => Some(c.bps.max(o.bps)),
            (Some(c), None) => Some(c.bps),
            (None, Some(o)) => Some(o.bps),
            (None, None) => None,
        }
    }

    fn end_record(&mut self) {
        if self.carried.is_some() || self.own.is_some() {
            self.ended = true;
        }
        self.carried = None;
        self.own = None;
    }

    /// A delivery in a valid window. It raises the floor, and ends the record
    /// only when it carries more than twice the record's rate.
    pub fn delivered(&mut self, bps: u32, basis: Basis, now: Instant) {
        if bps == 0 || basis == Basis::Assumed {
            return;
        }
        self.measured = true;
        if self.best.is_none_or(|(b, _)| bps > b) {
            self.best = Some((bps, basis));
        }
        if self
            .standing_bps()
            .is_some_and(|r| u64::from(bps) > u64::from(r) * u64::from(CONTRADICTION_FACTOR))
        {
            self.end_record();
        }
        #[cfg(test)]
        if self.end_above_floor
            && self
                .floor
                .floor(now)
                .is_some_and(|f| !f.remembered && bps > f.bps)
        {
            self.end_record();
        }
        self.floor.delivered(bps, basis, now);
    }

    /// A push-back in a valid window. Backlogged, it is a sample, which counts
    /// when it is at least half of the highest delivery so far; an
    /// application-limited one is a delivery.
    ///
    /// R2 — only a counted sample moves the floor. A dip moved it before R2,
    /// so a still screen's dip under a remembered 2.2 Mbit/s floor was written
    /// as the pair's floor (`a_quiet_session_keeps_the_remembered_floor`).
    pub fn pushed_back(&mut self, bps: u32, basis: Basis, now: Instant) {
        match basis {
            Basis::Assumed => {}
            Basis::AppLimited => self.delivered(bps, basis, now),
            Basis::Backlogged if bps > 0 => {
                self.measured = true;
                let counts = self.best.is_none_or(|(b, _)| {
                    u64::from(bps) * u64::from(CONTRADICTION_FACTOR) >= u64::from(b)
                });
                if counts {
                    self.counted = self.counted.saturating_add(1);
                    self.own = Some(match self.own {
                        Some(o) => Own {
                            bps: o.bps.max(bps),
                            newest: now,
                            windows: o.windows.saturating_add(1),
                            ..o
                        },
                        None => Own {
                            bps,
                            first: now,
                            newest: now,
                            windows: 1,
                        },
                    });
                    self.floor.pushed_back(bps, basis, now);
                } else {
                    self.not_counted = self.not_counted.saturating_add(1);
                }
            }
            Basis::Backlogged => {}
        }
    }

    /// A push-back in a valid window, as the window's [`Take`] says. A sample
    /// goes to [`Self::pushed_back`]. One from a window the classifier could
    /// not read (R1), or in which the sender wrote under half its target (R3),
    /// is counted for the close line and nothing more: it moves neither the
    /// record nor the floor, and its bytes already reached [`Self::delivered`]
    /// as the same window's delivery.
    pub fn pushed_back_in(&mut self, bps: u32, basis: Basis, take: Take, now: Instant) {
        #[cfg(test)]
        let take = match take {
            Take::Unread if self.read_any => Take::Sample,
            Take::UnderPlan if self.plan_any => Take::Sample,
            take => take,
        };
        let backlogged = basis == Basis::Backlogged && bps > 0;
        match take {
            Take::Sample => self.pushed_back(bps, basis, now),
            Take::Unread if backlogged => self.unread = self.unread.saturating_add(1),
            Take::UnderPlan if backlogged => {
                self.under_plan = self.under_plan.saturating_add(1);
            }
            Take::Unread | Take::UnderPlan => {}
        }
    }

    /// What a close would write now.
    pub fn closing(&self, now: Instant, now_unix: u64) -> Closing {
        let floor = self.floor.offer(now).and_then(|n| {
            Some(Floor {
                bps: n.bps,
                basis: FloorBasis::of(n.basis)?,
                at_unix: now_unix.saturating_sub(n.age.as_secs()),
            })
        });
        let record = match (self.carried, self.own) {
            (c, Some(o)) => {
                let (first, newest) = (
                    unix_of(o.first, now, now_unix),
                    unix_of(o.newest, now, now_unix),
                );
                Some(match c {
                    Some(c) => PushBack {
                        bps: c.bps.max(o.bps),
                        at_unix: c.at_unix.max(newest),
                        first_at_unix: c.first_at_unix.min(first),
                        windows: c.windows.saturating_add(o.windows),
                    },
                    None => PushBack {
                        bps: o.bps,
                        at_unix: newest,
                        first_at_unix: first,
                        windows: o.windows,
                    },
                })
            }
            (c, None) => c,
        };
        Closing {
            floor,
            record,
            best: self
                .best
                .and_then(|(bps, b)| FloorBasis::of(b).map(|b| (bps, b))),
            not_counted: self.not_counted,
            unread: self.unread,
            under_plan: self.under_plan,
            counted: self.counted,
            ended: self.ended,
            measured: self.measured,
            at_unix: now_unix,
        }
    }

    #[cfg(test)]
    fn with_end_above_floor(mut self) -> Self {
        self.set_end_above_floor();
        self
    }

    /// Test only: end the record on any delivery above the floor, as D25's
    /// first wording did.
    #[cfg(test)]
    pub(crate) fn set_end_above_floor(&mut self) {
        self.end_above_floor = true;
    }

    /// Test only: offer every push-back to the record, read window or not, as
    /// master did before R1.
    #[cfg(test)]
    pub(crate) fn set_read_any(&mut self) {
        self.read_any = true;
    }

    /// Test only: offer a push-back under the plan to the record, as the law
    /// did before R3.
    #[cfg(test)]
    pub(crate) fn set_plan_any(&mut self) {
        self.plan_any = true;
    }
}

/// Why a close wrote what it wrote: the close line's `why`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// No valid window gave a reading.
    NothingMeasured,
    /// Only readings at or below the remembered floor: the floor is not
    /// written back. The record may still have moved.
    RememberedOnly,
    /// No fresh entry before this session.
    FirstMeasurement,
    /// A delivery raised the floor.
    Raised,
    /// A backlogged push-back lowered it.
    PushedBack,
    /// The path changed before any reading (the guard froze).
    CarrierFlipped,
    /// The session had no key (`NoKey`). With the switch `off` there is no
    /// close line at all.
    NoKey,
    /// R2 — the session's floor equals the entry's: written, so its age is
    /// the session's.
    Refreshed,
    /// R2 — the session's floor is under the entry's and the session counted
    /// no push-back sample: the entry's floor stands, with its own age.
    FloorKept,
    /// R2 — the entry's floor passed its TTL during the session: the
    /// session's floor is written whatever it is, as before R2.
    FloorExpired,
}

impl Why {
    pub fn word(self) -> &'static str {
        match self {
            Why::NothingMeasured => "nothing_measured",
            Why::RememberedOnly => "remembered_only",
            Why::FirstMeasurement => "first_measurement",
            Why::Raised => "raised",
            Why::PushedBack => "pushed_back",
            Why::CarrierFlipped => "carrier_flipped",
            Why::NoKey => "no_key",
            Why::Refreshed => "refreshed",
            Why::FloorKept => "floor_kept",
            Why::FloorExpired => "floor_expired",
        }
    }
}

/// The close, case by case: the entry to write (`None` = write nothing) and
/// why. `entry` is the fresh entry the session opened from.
///
/// R2 — a floor is a lower bound on what the path carries, and lower bounds
/// combine by maximum: inside the entry's TTL, read at [`Closing::at_unix`],
/// the session's floor replaces a higher one only when the session counted a
/// push-back sample.
///
/// | What the session measured | floor | record | `why` |
/// |---|---|---|---|
/// | nothing in a valid window | not written | not written | `nothing_measured` |
/// | only readings at or below the remembered floor | the entry's, untouched | the entry's, with any counted sample | `remembered_only` |
/// | no entry yet | the session's, with its class and time | the session's | `first_measurement` |
/// | a floor above the entry's | the session's | kept, unless it delivered over twice the record's rate | `raised` |
/// | a floor equal to the entry's | the session's, so its age is the session's | as above | `refreshed` |
/// | a floor under the entry's, and a counted sample | the session's: damped from the floor by counted samples only, `backlogged` | the samples | `pushed_back` |
/// | a floor under the entry's, nothing counted | the entry's, with its own age | as above | `floor_kept` |
/// | any floor, the entry's past its TTL | the session's, as before R2 | as above | `floor_expired` |
pub fn close(entry: Option<&PairEntry>, c: &Closing) -> (Option<PairEntry>, Why) {
    if !c.measured {
        return (None, Why::NothingMeasured);
    }
    let (floor, why) = match (c.floor, entry) {
        (None, None) => return (None, Why::NothingMeasured),
        (None, Some(e)) => (e.floor, Why::RememberedOnly),
        (Some(f), None) => (f, Why::FirstMeasurement),
        (Some(f), Some(e)) if !e.fresh(c.at_unix) => (f, Why::FloorExpired),
        (Some(f), Some(e)) if f.bps > e.floor.bps => (f, Why::Raised),
        (Some(f), Some(e)) if f.bps == e.floor.bps => (
            Floor {
                at_unix: f.at_unix.max(e.floor.at_unix),
                ..f
            },
            Why::Refreshed,
        ),
        (Some(f), Some(_)) if c.counted > 0 => (f, Why::PushedBack),
        (Some(_), Some(e)) => (e.floor, Why::FloorKept),
    };
    let next = PairEntry {
        floor,
        push_back: c.record,
    };
    if entry == Some(&next) {
        return (None, why);
    }
    (Some(next), why)
}

/// The close line's `wrote`: which parts of the entry changed.
pub fn wrote(before: Option<&PairEntry>, after: Option<&PairEntry>) -> &'static str {
    let Some(after) = after else {
        return "nothing";
    };
    let floor = before.is_none_or(|b| b.floor != after.floor);
    let record = before.map_or(after.push_back.is_some(), |b| {
        b.push_back != after.push_back
    });
    match (floor, record) {
        (true, true) => "floor_and_push_back",
        (true, false) => "floor",
        (false, true) => "push_back",
        (false, false) => "nothing",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const USER: &str = "65a1b2c3d4e5f60718293a4b";
    const NOW: u64 = 1_795_000_000;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn facts(
        carrier: SessionCarrier,
        srflx: &[IpAddr],
        remote: (CandidateKind, IpAddr),
    ) -> KeyFacts {
        KeyFacts {
            carrier: Some(carrier),
            overlay_transport: None,
            user: Some(USER.to_string()),
            srflx: srflx.to_vec(),
            remote: Some(remote),
        }
    }

    fn key(f: &KeyFacts) -> String {
        pair_key(f).expect("a key").text().to_string()
    }

    // --- the key (AC8, D22) -------------------------------------------------

    /// AC8 — one viewer through two TURN workers is one entry. Master keys by
    /// the selected remote, which is the worker's relayed address: two
    /// entries (`rate_memory::memory_key`).
    #[test]
    fn two_relay_workers_are_one_entry() {
        let viewer = [v4(203, 0, 113, 77)];
        let a = facts(
            SessionCarrier::Turn,
            &viewer,
            (CandidateKind::Relay, v4(198, 51, 100, 10)),
        );
        let b = facts(
            SessionCarrier::Turn,
            &viewer,
            (CandidateKind::Relay, v4(198, 51, 100, 233)),
        );
        assert_eq!(key(&a), key(&b));
        assert_eq!(key(&a), format!("v2|turn|u:{USER}|n:203.0.113.0/24"));
        let master = |remote: &str| super::super::rate_memory::memory_key(remote, None);
        assert_ne!(master("198.51.100.10"), master("198.51.100.233"));
    }

    /// AC8, the shape of 2026-11-16 — two overlay addresses of one viewer, one
    /// network, one mesh relay: one entry. Master keys by the overlay address:
    /// two.
    #[test]
    fn two_overlay_addresses_of_one_viewer_on_one_network_are_one_entry() {
        let net = [v4(203, 0, 113, 77)];
        let mut a = facts(
            SessionCarrier::Derp,
            &net,
            (CandidateKind::Host, v4(100, 64, 0, 7)),
        );
        let mut b = facts(
            SessionCarrier::Derp,
            &net,
            (CandidateKind::Host, v4(100, 64, 0, 9)),
        );
        a.overlay_transport = Some("tcp".into());
        b.overlay_transport = Some("tcp".into());
        assert_eq!(key(&a), key(&b));
        assert_eq!(pair_key(&a).unwrap().carrier(), "derp/tcp");
        let master =
            |remote: &str| super::super::rate_memory::memory_key(remote, Some("relay:derp/tcp"));
        assert_ne!(master("100.64.0.7"), master("100.64.0.9"));
    }

    /// Two viewer networks are two entries.
    #[test]
    fn two_viewer_networks_are_two_entries() {
        let relay = (CandidateKind::Relay, v4(198, 51, 100, 10));
        let home = facts(SessionCarrier::Turn, &[v4(203, 0, 113, 77)], relay);
        let office = facts(SessionCarrier::Turn, &[v4(192, 0, 2, 14)], relay);
        assert_ne!(key(&home), key(&office));
    }

    /// With no server-reflexive candidate, an overlay remote keys the device,
    /// whole, and says so; a relay remote is no network at all.
    #[test]
    fn no_srflx_keys_the_overlay_device() {
        let k = pair_key(&facts(
            SessionCarrier::OverlayDirect,
            &[],
            (CandidateKind::Host, v4(100, 64, 0, 7)),
        ))
        .unwrap();
        assert_eq!(k.text(), format!("v2|overlay_direct|u:{USER}|o:100.64.0.7"));
        assert_eq!(k.net(), NetKind::Overlay);
        let relay = pair_key(&facts(
            SessionCarrier::Turn,
            &[],
            (CandidateKind::Relay, v4(198, 51, 100, 10)),
        ))
        .unwrap();
        assert_eq!(relay.net(), NetKind::Unknown);
        assert!(relay.text().ends_with("|n:?"));
        let own = pair_key(&facts(
            SessionCarrier::TurnLocal,
            &[],
            (CandidateKind::Srflx, v4(203, 0, 113, 77)),
        ))
        .unwrap();
        assert_eq!(own.net(), NetKind::Remote);
        assert!(own.text().ends_with("|n:203.0.113.0/24"));
    }

    /// No key without a user, a known carrier or a pair.
    #[test]
    fn no_key_without_a_user_or_a_carrier() {
        let base = facts(
            SessionCarrier::Turn,
            &[v4(203, 0, 113, 77)],
            (CandidateKind::Relay, v4(198, 51, 100, 10)),
        );
        let no_user = KeyFacts {
            user: None,
            ..base.clone()
        };
        assert_eq!(pair_key(&no_user), Err(NoKey::NoUser));
        let bad_user = KeyFacts {
            user: Some("Controller".into()),
            ..base.clone()
        };
        assert_eq!(pair_key(&bad_user), Err(NoKey::NoUser));
        let unknown = KeyFacts {
            carrier: Some(SessionCarrier::Unknown),
            ..base.clone()
        };
        assert_eq!(pair_key(&unknown), Err(NoKey::CarrierUnknown));
        let none = KeyFacts {
            carrier: None,
            ..base.clone()
        };
        assert_eq!(pair_key(&none), Err(NoKey::CarrierUnknown));
        let no_pair = KeyFacts {
            remote: None,
            ..base
        };
        assert_eq!(pair_key(&no_pair), Err(NoKey::NoPair));
    }

    /// D22 — an overlay server-reflexive address is never a network, and a
    /// public one wins over a private one, then the lowest IPv4, else IPv6.
    #[test]
    fn the_viewer_network_takes_a_public_srflx_and_never_an_overlay_one() {
        let remote = Some((CandidateKind::Relay, v4(198, 51, 100, 10)));
        let overlay_only = [v4(100, 64, 0, 7)];
        assert_eq!(viewer_net(&overlay_only, remote).0, NetKind::Unknown);
        let mixed = [v4(100, 64, 0, 7), v4(10, 1, 2, 3), v4(203, 0, 113, 77)];
        assert_eq!(viewer_net(&mixed, remote).1, "n:203.0.113.0/24");
        let private_only = [v4(192, 168, 7, 9), v4(10, 1, 2, 3)];
        assert_eq!(viewer_net(&private_only, remote).1, "n:10.1.2.0/24");
        let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0x77, 1, 0, 0, 0, 5));
        assert_eq!(viewer_net(&[v6], remote).1, "n:2001:db8:77::/48");
        let both = [v6, v4(203, 0, 113, 200), v4(203, 0, 112, 1)];
        assert_eq!(viewer_net(&both, remote).1, "n:203.0.112.0/24");
        let overlay6 = IpAddr::V6(Ipv6Addr::new(0xfd72, 0x6f6f, 0x6d6c, 0, 0, 0, 0, 9));
        assert_eq!(viewer_net(&[overlay6], remote).0, NetKind::Unknown);
    }

    /// D22 — an overlay carrier and any other never share a word: master's
    /// `carrier_tag` gives `direct` to both.
    #[test]
    fn overlay_and_other_carriers_never_share_a_word() {
        let mut words = Vec::new();
        for c in SessionCarrier::ALL {
            for t in [None, Some("udp"), Some("tcp")] {
                if let Some(w) = carrier_word(c, t) {
                    words.push((c, w));
                }
            }
        }
        let overlay = |c: SessionCarrier| {
            matches!(
                c,
                SessionCarrier::OverlayDirect
                    | SessionCarrier::OverlayTurn
                    | SessionCarrier::Derp
                    | SessionCarrier::OrgRelay
            )
        };
        for (a, wa) in &words {
            for (b, wb) in &words {
                if overlay(*a) != overlay(*b) {
                    assert_ne!(wa, wb, "{a:?} and {b:?}");
                }
            }
        }
        assert_eq!(carrier_word(SessionCarrier::Unknown, None), None);
        assert_eq!(
            super::super::rate_memory::carrier_tag("direct", None, None),
            Some("direct".to_string()),
            "master: the overlay's direct"
        );
        assert_eq!(
            SessionCarrier::Direct.as_str(),
            "direct",
            "master: ICE direct"
        );
    }

    /// A line carries 12 hex of the key's hash, never the key.
    #[test]
    fn a_line_carries_a_hash_of_the_key() {
        let k = pair_key(&facts(
            SessionCarrier::Turn,
            &[v4(203, 0, 113, 77)],
            (CandidateKind::Relay, v4(198, 51, 100, 10)),
        ))
        .unwrap();
        let id = k.id();
        assert_eq!(id.len(), 12);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!id.contains(USER) && !k.text().contains(&id));
    }

    // --- the file (D23) -----------------------------------------------------

    fn entry(bps: u32, at: u64) -> PairEntry {
        PairEntry {
            floor: Floor {
                bps,
                basis: FloorBasis::AppLimited,
                at_unix: at,
            },
            push_back: None,
        }
    }

    /// One bad entry drops alone. On master one `stable_bps` past `u32`
    /// empties the whole file (`rate_memory::RateMemory::load`).
    #[test]
    fn a_bad_entry_drops_alone() {
        let json = format!(
            r#"{{"v":1,"pairs":{{
                "good":{{"floor":{{"bps":2000000,"basis":"app_limited","at_unix":{NOW}}}}},
                "huge":{{"floor":{{"bps":5000000000,"basis":"app_limited","at_unix":{NOW}}}}},
                "low":{{"floor":{{"bps":49999,"basis":"backlogged","at_unix":{NOW}}}}},
                "word":{{"floor":{{"bps":2000000,"basis":"assumed","at_unix":{NOW}}}}},
                "shape":{{"floor":"2000000"}},
                "record":{{"floor":{{"bps":2000000,"basis":"backlogged","at_unix":{NOW}}},
                          "push_back":{{"bps":600000,"at_unix":{NOW},"first_at_unix":{},"windows":3}}}}
            }}}}"#,
            NOW + 1
        );
        let mut m = PathMemory::parse(json.as_bytes(), NOW);
        assert_eq!(m.pairs.keys().collect::<Vec<_>>(), ["good"]);
        for k in ["huge", "low", "word", "shape", "record"] {
            assert_eq!(m.dropped.get(k), Some(&Dropped::Invalid), "{k}");
            assert_eq!(m.state(k, NOW), EntryState::Invalid);
        }
        // AC7 — what was dropped is never written: the next write keeps the
        // good entry and the new one, and no class but the two measured ones.
        m.write("next", entry(2_100_000, NOW), NOW);
        let written = String::from_utf8(m.to_bytes()).unwrap();
        let back = PathMemory::parse(written.as_bytes(), NOW);
        assert_eq!(back.pairs.keys().collect::<Vec<_>>(), ["good", "next"]);
        assert!(back.dropped.is_empty() && !written.contains("assumed"));
        let master = format!(
            r#"{{"entries":{{"good":{{"stable_bps":2000000,"at_unix":{NOW},"provenance":"measured"}},
               "huge":{{"stable_bps":5000000000,"at_unix":{NOW},"provenance":"measured"}}}}}}"#
        );
        let dir = std::env::temp_dir().join(format!("pair-memory-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rate_memory.json");
        std::fs::write(&path, master).unwrap();
        assert!(
            super::super::rate_memory::RateMemory::load(&path)
                .entries
                .is_empty(),
            "master: one entry out of range empties the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A time more than 300 s ahead drops the entry. On master a future
    /// `at_unix` reads as fresh.
    #[test]
    fn a_future_entry_is_dropped() {
        let mut m = PathMemory::default();
        m.pairs.insert("soon".into(), entry(2_000_000, NOW + 300));
        m.pairs.insert("later".into(), entry(2_000_000, NOW + 301));
        let back = PathMemory::parse(&m.to_bytes(), NOW);
        assert!(back.pairs.contains_key("soon"));
        assert_eq!(back.dropped.get("later"), Some(&Dropped::Future));
        assert_eq!(back.state("later", NOW), EntryState::Future);
        let mut master = super::super::rate_memory::RateMemory::default();
        master.record("later", 2_000_000, NOW + 86_400);
        assert_eq!(master.seed_for("later", NOW), Some(2_000_000), "master");
    }

    /// Ruling 2: pumps that end at once each keep their key. Eight threads,
    /// started together, each write twenty keys of their own through
    /// [`PathMemory::update`]; the file ends with all 160. Without the lock
    /// two saves share the temp sibling and a load-then-save drops the other
    /// thread's key (the red run lost keys or failed a save).
    #[test]
    fn pumps_that_end_at_once_each_keep_their_key() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let (threads, rounds) = (8, 20);
        let start = Arc::new(Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (path, start) = (path.clone(), Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    (0..rounds)
                        .map(|r| {
                            PathMemory::update(&path, NOW, |m| {
                                m.write(&format!("k{t}-{r}"), entry(2_000_000, NOW), NOW);
                            })
                            .is_ok()
                        })
                        .filter(|ok| !ok)
                        .count()
                })
            })
            .collect();
        let failed: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(failed, 0, "every save went through");
        let memory = PathMemory::load(&path, NOW);
        assert_eq!(memory.pairs.len(), threads * rounds);
        assert!(memory.dropped.is_empty());
    }

    /// A version other than 1, or a file that is not this shape, is an empty
    /// memory; unknown fields are ignored.
    #[test]
    fn a_version_other_than_one_reads_as_empty() {
        let body = |v: &str| {
            format!(
                r#"{{"v":{v},"pairs":{{"k":{{"floor":{{"bps":2000000,"basis":"backlogged","at_unix":{NOW},"new":1}}}}}},"more":true}}"#
            )
        };
        assert_eq!(PathMemory::parse(body("1").as_bytes(), NOW).pairs.len(), 1);
        for v in ["2", "0", "\"1\"", "null"] {
            assert_eq!(
                PathMemory::parse(body(v).as_bytes(), NOW),
                PathMemory::default(),
                "v = {v}"
            );
        }
        assert_eq!(PathMemory::parse(b"{not json", NOW), PathMemory::default());
        assert_eq!(
            PathMemory::parse(br#"{"v":1,"pairs":[]}"#, NOW),
            PathMemory::default()
        );
    }

    /// The file round-trips, written whole, without the expired entries, and
    /// a record older than a day is dropped at the read while its entry stays.
    #[test]
    fn the_file_round_trips_and_drops_what_expired() {
        let dir = std::env::temp_dir().join(format!("pair-memory-rt-{}", std::process::id()));
        let path = dir.join(FILE_NAME);
        let mut m = PathMemory::default();
        m.pairs.insert(
            "old".into(),
            entry(2_000_000, NOW - FLOOR_TTL.as_secs() - 1),
        );
        let mut e = entry(2_200_000, NOW - 3_600);
        e.push_back = Some(PushBack {
            bps: 600_000,
            at_unix: NOW - RECORD_TTL.as_secs() - 1,
            first_at_unix: NOW - RECORD_TTL.as_secs() - 60,
            windows: 5,
        });
        m.write("k", e, NOW);
        assert!(!m.pairs.contains_key("old"), "pruned by the write");
        m.save(&path).expect("save");
        let back = PathMemory::load(&path, NOW);
        assert_eq!(back.pairs["k"].floor, e.floor);
        assert_eq!(back.pairs["k"].push_back, None, "a stale record is dropped");
        assert_eq!(back.state("k", NOW), EntryState::Fresh);
        assert_eq!(
            back.state("k", NOW - 3_600 + FLOOR_TTL.as_secs() + 1),
            EntryState::Expired
        );
        assert_eq!(back.state("absent", NOW), EntryState::None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- version skew (D23) -------------------------------------------------

    /// Today's file is written byte for byte as master writes it: the guard's
    /// write is `RateMemory::record_session` + `save`, untouched.
    #[test]
    fn todays_file_is_written_byte_for_byte_as_master() {
        let dir = std::env::temp_dir().join(format!("pair-memory-today-{}", std::process::id()));
        let path = dir.join("rate_memory.json");
        let mut m = super::super::rate_memory::RateMemory::default();
        m.record_session("203.0.113.221|relay:turn/udp", Some(2_005_279), NOW);
        m.save(&path).expect("save");
        let bytes = std::fs::read_to_string(&path).unwrap();
        let expected = format!(
            "{{\n  \"entries\": {{\n    \"203.0.113.221|relay:turn/udp\": {{\n      \"stable_bps\": 2005279,\n      \"at_unix\": {NOW},\n      \"provenance\": \"measured\"\n    }}\n  }}\n}}"
        );
        assert_eq!(bytes, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the one-file layout would lose: master's two types (frozen here)
    /// read a `rate_memory.json` with the model's shape inside `entries`. The
    /// entry has no `stable_bps`, so the WHOLE file fails, reads as empty, and
    /// the next save writes nothing back: every pair's entry is gone.
    #[test]
    fn a_frozen_master_loses_the_whole_file_if_the_model_shares_it() {
        #[derive(Serialize, Deserialize, Default)]
        struct MasterEntry {
            stable_bps: u32,
            at_unix: u64,
            #[serde(default)]
            provenance: String,
        }
        #[derive(Serialize, Deserialize, Default)]
        struct MasterMemory {
            #[serde(default)]
            entries: BTreeMap<String, MasterEntry>,
        }
        let shared = format!(
            r#"{{"entries":{{
                "203.0.113.221|relay:turn/udp":{{"stable_bps":2005279,"at_unix":{NOW},"provenance":"measured"}},
                "v2|turn|u:{USER}|n:203.0.113.0/24":{{"floor":{{"bps":2200000,"basis":"app_limited","at_unix":{NOW}}}}}
            }}}}"#
        );
        let read: MasterMemory = serde_json::from_str(&shared).unwrap_or_default();
        assert!(
            read.entries.is_empty(),
            "master reads the shared file as empty"
        );
        let saved = serde_json::to_string(&read).unwrap();
        assert_eq!(
            saved, r#"{"entries":{}}"#,
            "and its next save keeps nothing"
        );
        // Read by master's types, the model's own file holds nothing master
        // could open from, and master never opens it.
        let mut m = PathMemory::default();
        m.write(
            &format!("v2|turn|u:{USER}|n:203.0.113.0/24"),
            entry(2_200_000, NOW),
            NOW,
        );
        assert!(
            serde_json::from_slice::<MasterMemory>(&m.to_bytes())
                .is_ok_and(|x| x.entries.is_empty())
        );
    }

    /// With no new file, every pair's model says "no entry" and opens at N.
    #[test]
    fn with_no_new_file_every_pair_opens_at_the_nominal() {
        let path = std::env::temp_dir().join(format!(
            "pair-memory-none-{}/{FILE_NAME}",
            std::process::id()
        ));
        let m = PathMemory::load(&path, NOW);
        assert_eq!(m.state("any", NOW), EntryState::None);
        let o = model_opening(2_550_000, 8_000_000, m.fresh("any", NOW), 1_000_000, NOW);
        assert_eq!(
            o,
            ModelOpening {
                bps: 2_550_000,
                profile: false,
                trusted: false
            }
        );
    }

    // --- trust (D25) --------------------------------------------------------

    fn record(windows: u32, span: u64, age: u64) -> PushBack {
        PushBack {
            bps: 600_000,
            at_unix: NOW - age,
            first_at_unix: NOW - age - span,
            windows,
        }
    }

    #[test]
    fn trust_needs_three_windows_over_ten_seconds_within_a_day() {
        assert!(!trusted(&record(1, 0, 0), NOW), "one push-back");
        assert!(!trusted(&record(3, 9, 0), NOW), "three inside 10 s");
        assert!(trusted(&record(3, 10, 0), NOW), "three over 10 s");
        assert!(trusted(&record(3, 10, RECORD_TTL.as_secs()), NOW));
        assert!(
            !trusted(&record(3, 10, RECORD_TTL.as_secs() + 1), NOW),
            "older than a day"
        );
    }

    fn t0() -> Instant {
        Instant::now()
    }

    /// A session: `open` from `entry`, then each step `(seconds from the open,
    /// rate, push-back?)` as the governor feeds a valid window: the arrival
    /// rate as a delivery, then, if refused, the same rate as a push-back.
    fn session(
        entry: Option<&PairEntry>,
        at_unix: u64,
        steps: &[(u64, u32, bool)],
        mut m: SessionMemory,
    ) -> (SessionMemory, Closing) {
        let start = t0();
        m.open(entry, at_unix, start);
        let mut now = start;
        for &(s, bps, refused) in steps {
            now = start + Duration::from_secs(s);
            m.delivered(bps, Basis::of_window(refused), now);
            if refused {
                m.pushed_back(bps, Basis::Backlogged, now);
            }
        }
        let end = now + Duration::from_secs(1);
        let unix = at_unix + end.duration_since(start).as_secs();
        (m, m.closing(end, unix))
    }

    #[test]
    fn windows_add_up_across_two_sessions_while_the_record_is_fresh() {
        let (_, first) = session(
            None,
            NOW,
            &[(1, 600_000, true), (5, 600_000, true)],
            SessionMemory::default(),
        );
        let (written, why) = close(None, &first);
        assert_eq!(why, Why::FirstMeasurement);
        let written = written.expect("a first entry");
        let r = written.push_back.expect("a record");
        assert_eq!((r.windows, r.span_s()), (2, 4));
        assert!(!trusted(&r, NOW + 7));
        let later = NOW + 3_600;
        let (_, second) = session(
            Some(&written),
            later,
            &[(8, 600_000, true)],
            SessionMemory::default(),
        );
        let r = second.record.expect("carried");
        assert_eq!(r.windows, 3);
        assert_eq!(r.first_at_unix, NOW + 1);
        assert!(trusted(&r, later + 9));
        // The same entry a day and more later: its record is not carried.
        let stale = NOW + 5 + RECORD_TTL.as_secs() + 1;
        let (_, third) = session(
            Some(&written),
            stale,
            &[(8, 600_000, true)],
            SessionMemory::default(),
        );
        assert_eq!(third.record.map(|r| r.windows), Some(1), "not carried");
    }

    #[test]
    fn a_sample_under_half_the_sessions_best_valid_delivery_does_not_count() {
        let (_, c) = session(
            None,
            NOW,
            &[
                (1, 2_000_000, false),
                (2, 999_999, true),
                (3, 1_000_000, true),
            ],
            SessionMemory::default(),
        );
        assert_eq!(c.not_counted, 1);
        assert_eq!(c.record.map(|r| (r.windows, r.bps)), Some((1, 1_000_000)));
        assert_eq!(c.best, Some((2_000_000, FloorBasis::AppLimited)));
    }

    #[test]
    fn a_delivery_ends_the_record_only_above_twice_its_rate() {
        let carried = PairEntry {
            floor: Floor {
                bps: 640_000,
                basis: FloorBasis::Backlogged,
                at_unix: NOW - 60,
            },
            push_back: Some(PushBack {
                bps: 600_000,
                at_unix: NOW - 60,
                first_at_unix: NOW - 90,
                windows: 4,
            }),
        };
        // Above the floor, at most twice the record's rate: it stands.
        let (_, kept) = session(
            Some(&carried),
            NOW,
            &[(1, 1_200_000, false)],
            SessionMemory::default(),
        );
        assert!(!kept.ended);
        assert_eq!(kept.record, carried.push_back);
        assert_eq!(kept.floor.map(|f| f.bps), Some(1_200_000), "the floor rose");
        let (next, why) = close(Some(&carried), &kept);
        assert_eq!(
            (next.unwrap().push_back, why),
            (carried.push_back, Why::Raised)
        );
        // Above twice its rate: it ends, and leaves the entry.
        let (_, ended) = session(
            Some(&carried),
            NOW,
            &[(1, 1_200_001, false)],
            SessionMemory::default(),
        );
        assert!(ended.ended);
        assert_eq!(ended.record, None);
        let (next, _) = close(Some(&carried), &ended);
        assert_eq!(next.unwrap().push_back, None);
    }

    // --- the write (AC7) ----------------------------------------------------

    /// AC7 — a floor that is still the remembered one is not written back:
    /// a session whose readings stay at or under it writes nothing at all.
    /// Master's `PathModel::offer` offered the remembered seed.
    #[test]
    fn a_remembered_floor_is_not_written_back() {
        let e = entry(2_200_000, NOW - 86_400);
        let (_, c) = session(
            Some(&e),
            NOW,
            &[(1, 900_000, false), (2, 2_200_000, false)],
            SessionMemory::default(),
        );
        assert_eq!(c.floor, None);
        assert_eq!(close(Some(&e), &c), (None, Why::RememberedOnly));
    }

    /// A session that measured nothing writes nothing and leaves the entry's
    /// time alone.
    #[test]
    fn a_session_that_measured_nothing_writes_nothing() {
        let e = entry(2_200_000, NOW - 86_400);
        let (_, c) = session(Some(&e), NOW, &[], SessionMemory::default());
        assert_eq!(close(Some(&e), &c), (None, Why::NothingMeasured));
        let (_, c) = session(None, NOW, &[], SessionMemory::default());
        assert_eq!(close(None, &c), (None, Why::NothingMeasured));
    }

    /// The write takes the reading's class and time; a backlogged push-back
    /// lowers the floor by the in-session law.
    #[test]
    fn the_write_takes_the_readings_class_and_time() {
        let e = entry(2_200_000, NOW - 86_400);
        let (_, c) = session(
            Some(&e),
            NOW,
            &[(4, 3_000_000, false)],
            SessionMemory::default(),
        );
        let (next, why) = close(Some(&e), &c);
        assert_eq!(why, Why::Raised);
        assert_eq!(
            next.unwrap().floor,
            Floor {
                bps: 3_000_000,
                basis: FloorBasis::AppLimited,
                at_unix: NOW + 4
            }
        );
        assert_eq!(wrote(Some(&e), next.as_ref()), "floor");
        let (_, c) = session(
            Some(&e),
            NOW,
            &[(4, 400_000, true)],
            SessionMemory::default(),
        );
        let (next, why) = close(Some(&e), &c);
        assert_eq!(why, Why::PushedBack);
        let next = next.unwrap();
        assert_eq!(next.floor.basis, FloorBasis::Backlogged);
        assert_eq!(
            next.floor.bps,
            super::super::rate_memory::damp(2_200_000, 400_000)
        );
        assert_eq!(wrote(Some(&e), Some(&next)), "floor_and_push_back");
    }

    // --- the floor as a maximum inside its TTL (R2) -------------------------

    /// FR-148 P1 (R2) — a quiet session after a remembered 2.2 Mbit/s floor:
    /// a still screen delivers 300 kbit/s, and three windows push back at
    /// 120 kbit/s, under half of that, so none counts. Before R2 each dip
    /// damped the floor and the close wrote what was left as `pushed_back`. A
    /// floor is a lower bound on what the path carries, and it falls only on
    /// counted evidence: the remembered 2.2 M stands, with its own age.
    #[test]
    fn a_quiet_session_keeps_the_remembered_floor() {
        let e = entry(2_200_000, NOW - 86_400);
        let (_, c) = session(
            Some(&e),
            NOW,
            &[
                (1, 300_000, false),
                (5, 120_000, true),
                (9, 120_000, true),
                (13, 120_000, true),
                (17, 300_000, false),
            ],
            SessionMemory::default(),
        );
        assert_eq!((c.counted, c.not_counted), (0, 3));
        assert_eq!(c.floor, None, "the remembered floor stands");
        assert_eq!(close(Some(&e), &c), (None, Why::RememberedOnly));
    }

    /// R2 — a session with counted push-back under the remembered floor
    /// lowers it, by its counted samples only: the dips beside them move
    /// nothing.
    #[test]
    fn a_counted_push_back_lowers_the_floor_and_a_dip_beside_it_does_not() {
        let e = entry(2_200_000, NOW - 86_400);
        let (_, c) = session(
            Some(&e),
            NOW,
            &[
                (1, 300_000, false),
                // Counts: twice 200 k is at least the best, 300 k.
                (5, 200_000, true),
                // Dips: twice 120 k is under it.
                (9, 120_000, true),
                (13, 120_000, true),
            ],
            SessionMemory::default(),
        );
        assert_eq!((c.counted, c.not_counted), (1, 2));
        let (next, why) = close(Some(&e), &c);
        assert_eq!(why, Why::PushedBack);
        let floor = next.expect("written").floor;
        assert_eq!(
            (floor.bps, floor.basis),
            (
                super::super::rate_memory::damp(2_200_000, 200_000),
                FloorBasis::Backlogged
            )
        );
    }

    /// R2 at the close, against a remembered 2.2 Mbit/s floor a day old: the
    /// session's floor replaces it when it is higher, or equal (the age is
    /// then the session's), and a lower one replaces it only when the session
    /// counted a push-back sample; otherwise the entry's floor stands with its
    /// own age, and nothing is written. Past the floor's TTL the session's
    /// floor is written whatever it is, as before R2.
    #[test]
    fn the_close_keeps_the_higher_floor_inside_its_ttl() {
        let e = entry(2_200_000, NOW - 86_400);
        let closing = |bps: u32, counted: u32, at_unix: u64| Closing {
            floor: Some(Floor {
                bps,
                basis: FloorBasis::Backlogged,
                at_unix: at_unix - 5,
            }),
            record: None,
            best: Some((bps, FloorBasis::AppLimited)),
            not_counted: 0,
            unread: 0,
            under_plan: 0,
            counted,
            ended: false,
            measured: true,
            at_unix,
        };
        let floor = |bps: u32, at_unix: u64| Floor {
            bps,
            basis: FloorBasis::Backlogged,
            at_unix,
        };
        let written = |c: &Closing| {
            let (next, why) = close(Some(&e), c);
            (next.map(|n| n.floor), why)
        };
        // Higher.
        assert_eq!(
            written(&closing(3_000_000, 0, NOW)),
            (Some(floor(3_000_000, NOW - 5)), Why::Raised)
        );
        // Equal: written, so the age is the session's.
        assert_eq!(
            written(&closing(2_200_000, 0, NOW)),
            (Some(floor(2_200_000, NOW - 5)), Why::Refreshed)
        );
        // Lower, with a counted sample.
        assert_eq!(
            written(&closing(1_500_000, 1, NOW)),
            (Some(floor(1_500_000, NOW - 5)), Why::PushedBack)
        );
        // Lower, nothing counted: the entry stands, so nothing is written.
        assert_eq!(written(&closing(120_000, 0, NOW)), (None, Why::FloorKept));
        // The same past the floor's TTL: the session's floor is written.
        let past = e.floor.at_unix + FLOOR_TTL.as_secs() + 1;
        assert_eq!(
            written(&closing(120_000, 0, past)),
            (Some(floor(120_000, past - 5)), Why::FloorExpired)
        );
        // On the last second of the TTL the entry still stands.
        let last = e.floor.at_unix + FLOOR_TTL.as_secs();
        assert_eq!(written(&closing(120_000, 0, last)), (None, Why::FloorKept));
        // Kept with a record that ended: the entry's floor, and no record.
        let with_record = PairEntry {
            push_back: Some(PushBack {
                bps: 56_000,
                at_unix: NOW - 60,
                first_at_unix: NOW - 90,
                windows: 2,
            }),
            ..e
        };
        let ended = Closing {
            ended: true,
            ..closing(120_000, 0, NOW)
        };
        let (next, why) = close(Some(&with_record), &ended);
        assert_eq!(why, Why::FloorKept);
        assert_eq!(next, Some(e), "the entry's floor, its record gone");
        assert_eq!(wrote(Some(&with_record), next.as_ref()), "push_back");
    }

    // --- the opening (D25, D26) ---------------------------------------------

    const N: u32 = 2_550_000;
    const HI: u32 = 8_000_000;
    const T: u32 = 1_000_000;

    fn opening(floor: u32, record: Option<PushBack>) -> ModelOpening {
        let e = PairEntry {
            floor: Floor {
                bps: floor,
                basis: FloorBasis::Backlogged,
                at_unix: NOW - 60,
            },
            push_back: record,
        };
        model_opening(N, HI, Some(&e), T, NOW)
    }

    fn trusted_at(bps: u32) -> Option<PushBack> {
        Some(PushBack {
            bps,
            at_unix: NOW - 60,
            first_at_unix: NOW - 120,
            windows: 3,
        })
    }

    /// The design's §3.3 table (N = 2.55 M, hi = 8 M), and the rulings' rows.
    #[test]
    fn the_models_opening_follows_the_rule() {
        assert_eq!(model_opening(N, HI, None, T, NOW).bps, N, "no entry");
        let expired = entry(6_500_000, NOW - FLOOR_TTL.as_secs() - 1);
        assert_eq!(
            model_opening(N, HI, Some(&expired), T, NOW).bps,
            N,
            "expired"
        );
        assert_eq!(
            opening(1_399_272, None),
            ModelOpening {
                bps: N,
                profile: false,
                trusted: false
            }
        );
        assert_eq!(opening(2_170_000, None).bps, N);
        assert_eq!(opening(6_500_000, None).bps, 5_525_000);
        assert_eq!(opening(15_915_240, None).bps, HI);
        // A floor of 2.2 M and ONE push-back at 56 kbit/s: N, no profile.
        let one = Some(PushBack {
            bps: 56_000,
            at_unix: NOW - 60,
            first_at_unix: NOW - 60,
            windows: 1,
        });
        assert_eq!(
            opening(2_200_000, one),
            ModelOpening {
                bps: N,
                profile: false,
                trusted: false
            }
        );
        // The same floor with a trusted push-back: 0.85 × the damped floor.
        assert_eq!(
            opening(2_200_000, trusted_at(56_000)),
            ModelOpening {
                bps: 1_870_000,
                profile: false,
                trusted: true
            }
        );
        // §3.3: from 4 M, a push-back at 1 M damps D to 2.5 M: 2.125 M, O > T.
        assert_eq!(opening(2_500_000, trusted_at(1_000_000)).bps, 2_125_000);
        // Repeated push-backs at 0.4 M: D 2.2 → 1.3 → 0.85 M.
        assert_eq!(
            opening(2_200_000, trusted_at(400_000)),
            ModelOpening {
                bps: 1_870_000,
                profile: false,
                trusted: true
            }
        );
        assert_eq!(
            opening(1_300_000, trusted_at(400_000)),
            ModelOpening {
                bps: 1_105_000,
                profile: false,
                trusted: true
            }
        );
        assert_eq!(
            opening(850_000, trusted_at(400_000)),
            ModelOpening {
                bps: 722_500,
                profile: true,
                trusted: true
            }
        );
        // The profile needs trusted, P ≤ T and O ≤ T.
        assert!(
            !opening(
                850_000,
                Some(PushBack {
                    windows: 2,
                    ..trusted_at(400_000).unwrap()
                })
            )
            .profile
        );
        assert!(!opening(850_000, trusted_at(1_000_001)).profile, "P > T");
        assert!(!opening(1_300_000, trusted_at(400_000)).profile, "O > T");
        // The lower clamp, and the threshold off.
        assert_eq!(opening(100_000, trusted_at(60_000)).bps, OPEN_MIN_BPS);
        let e = PairEntry {
            floor: Floor {
                bps: 850_000,
                basis: FloorBasis::Backlogged,
                at_unix: NOW - 60,
            },
            push_back: trusted_at(400_000),
        };
        assert!(!model_opening(N, HI, Some(&e), 0, NOW).profile);
        // That push-back 25 h later: as a floor.
        let late = PairEntry {
            push_back: trusted_at(400_000).map(|p| PushBack {
                at_unix: NOW - 25 * 3_600,
                first_at_unix: NOW - 25 * 3_600 - 60,
                ..p
            }),
            ..e
        };
        assert_eq!(
            model_opening(N, HI, Some(&late), T, NOW),
            ModelOpening {
                bps: N,
                profile: false,
                trusted: false
            }
        );
    }

    /// D25 corrected: a delivery that only raises the floor leaves the record,
    /// and the rule it replaced ends it, on the same session.
    #[test]
    fn the_corrected_rule_keeps_the_record_the_first_wording_ended() {
        let steps = [(1, 600_000, true), (6, 610_000, true), (12, 640_000, true)];
        let (_, kept) = session(None, NOW, &steps, SessionMemory::default());
        assert!(trusted(&kept.record.unwrap(), NOW + 13));
        let (_, old) = session(
            None,
            NOW,
            &steps,
            SessionMemory::default().with_end_above_floor(),
        );
        assert!(old.ended);
        assert!(!old.record.is_some_and(|r| trusted(&r, NOW + 13)));
    }
}
