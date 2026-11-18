// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! FR-148 P1 step 2 — the FFmpeg data-channel pump's pair memories, moved out
//! of the pump.
//!
//! Two memories, side by side, on a constrained session:
//!
//! | | today's (`encode::rate_memory`) | the path model's (`encode::pair_memory`) |
//! |---|---|---|
//! | file | `rate_memory.json`, written exactly as before | `path_memory.json`, its own |
//! | key | the selected remote and the overlay's carrier tag | the carrier, the controlling user and the viewer's network (D22) |
//! | read at the open | seeds the opener, as before | the open line only: nothing drives a rate in this step |
//! | written at the close | as before | what the session measured in valid windows (D24, D25) |
//! | lines | as before | [`OPEN_LINE`] and [`CLOSE_LINE`] |
//!
//! The switch at `off` keeps the path model's side out entirely: no read, no
//! write, no line (`media_delivery::Mode`).
//!
//! The viewer's facts reach the pump through [`PairFacts`], which the session's
//! peer registers under its id: the controlling user, set before the offer is
//! answered, and the viewer's server-reflexive candidates, taken as they are
//! trickled. Taking them there needs no lock at pump start; reading them from
//! the ICE agent would take its lock and the association's.
#![cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use tracing::{info, warn};
use webrtc::peer_connection::RTCPeerConnection;

use crate::encode::governor::RateGovernor;
use crate::encode::pair_memory::{
    self as pm, Closing, EntryState, KeyFacts, PairEntry, PairKey, PathMemory,
};
use crate::encode::rate_memory;
use crate::media_delivery::Mode;
use crate::session_carrier::{self as sc, OverlayPath, PairView};

/// The open line's message, which a field read greps for.
pub const OPEN_LINE: &str = "FR-148 P1 path memory: the pair's entry at the open";
/// The close line's message.
pub const CLOSE_LINE: &str = "FR-148 P1 path memory: what the close wrote";

/// At most this many server-reflexive candidates are kept per session.
const SRFLX_MAX: usize = 16;

// ---------------------------------------------------------------------------
// The viewer's facts, registered per session
// ---------------------------------------------------------------------------

/// What the agent learns about a session's viewer before its pump opens.
#[derive(Debug, Default)]
pub struct PairFacts {
    user: OnceLock<String>,
    srflx: Mutex<Vec<IpAddr>>,
}

static FACTS: LazyLock<Mutex<HashMap<ObjectId, Weak<PairFacts>>>> = LazyLock::new(Default::default);

impl PairFacts {
    /// A session's facts, found by its id while the returned `Arc` lives. A
    /// later peer of the same session replaces an earlier one's.
    pub fn register(session_id: ObjectId) -> Arc<Self> {
        let facts = Arc::new(Self::default());
        let mut all = FACTS.lock().unwrap_or_else(PoisonError::into_inner);
        all.retain(|_, f| f.strong_count() > 0);
        all.insert(session_id, Arc::downgrade(&facts));
        facts
    }

    /// The facts of a session whose peer still lives.
    pub fn of(session_id: ObjectId) -> Option<Arc<Self>> {
        let all = FACTS.lock().unwrap_or_else(PoisonError::into_inner);
        all.get(&session_id).and_then(Weak::upgrade)
    }

    /// The controlling user, from `rc:request`. Set once.
    pub fn set_user(&self, user: ObjectId) {
        let _ = self.user.set(user.to_hex());
    }

    /// A candidate the viewer trickled: a server-reflexive one is kept.
    pub fn note_candidate(&self, candidate: &str) {
        let Some(ip) = srflx_address(candidate) else {
            return;
        };
        let mut srflx = self.srflx.lock().unwrap_or_else(PoisonError::into_inner);
        if !srflx.contains(&ip) && srflx.len() < SRFLX_MAX {
            srflx.push(ip);
        }
    }

    fn user(&self) -> Option<String> {
        self.user.get().cloned()
    }

    fn srflx(&self) -> Vec<IpAddr> {
        self.srflx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The address of a server-reflexive candidate line (`candidate:<foundation>
/// <component> <transport> <priority> <address> <port> typ srflx ...`), or
/// `None` for any other.
pub fn srflx_address(candidate: &str) -> Option<IpAddr> {
    let line = candidate.trim();
    let line = line.strip_prefix("a=").unwrap_or(line);
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.get(6) != Some(&"typ") || f.get(7) != Some(&"srflx") {
        return None;
    }
    f.get(4)?.parse().ok()
}

// ---------------------------------------------------------------------------
// The overlay's answer and the selected pair
// ---------------------------------------------------------------------------

/// What the overlay says about the selected remote's peer.
#[derive(Debug, Clone)]
struct Overlay {
    connection: tunnel_core::localapi::ConnectionType,
    relay_kind: Option<String>,
    relay_transport: Option<String>,
}

impl Overlay {
    /// FR-79 V2 — today's carrier tag, exactly as the pump formed it.
    fn today_tag(&self) -> Option<String> {
        let connection = format!("{:?}", self.connection).to_lowercase();
        rate_memory::carrier_tag(
            &connection,
            self.relay_kind.as_deref(),
            self.relay_transport.as_deref(),
        )
    }

    /// FR-102's reading of the same answer, as `session_carrier` reads it.
    fn path(&self) -> OverlayPath {
        use tunnel_core::localapi::ConnectionType;
        match self.connection {
            ConnectionType::Direct => OverlayPath::Direct,
            ConnectionType::Relay => OverlayPath::Relay(self.relay_kind.clone()),
            other => OverlayPath::Other(format!("{other:?}").to_lowercase()),
        }
    }
}

/// FR-79 V2 — the overlay's answer for an overlay address, within the
/// LocalAPI budget; `None` for any other address, or no answer. The one query
/// `peer.rs` made for today's key, moved here.
async fn overlay_of(remote_addr: &str) -> Option<Overlay> {
    if !crate::peer::addr_is_overlay_range(remote_addr) {
        return None;
    }
    let query = async {
        let mut client = tunnel_core::localapi::connect().await.ok()?;
        let peers = client.peers().await.ok()?;
        peers.into_iter().find(|p| {
            p.overlay_ip.as_deref() == Some(remote_addr)
                || p.overlay_ip6.as_deref() == Some(remote_addr)
        })
    };
    let peer = tokio::time::timeout(sc::LOCALAPI_BUDGET, query)
        .await
        .ok()??;
    Some(Overlay {
        connection: peer.connection,
        relay_kind: peer.relay_kind,
        relay_transport: peer.relay_transport,
    })
}

/// The selected pair, read once: the pump's own read of it has already
/// waited for the nomination.
async fn selected_pair(pc: &RTCPeerConnection) -> Option<PairView> {
    let sctp = pc.sctp();
    let dtls = sctp.transport();
    let ice = dtls.ice_transport();
    let pair = ice.get_selected_candidate_pair().await?;
    let (local, remote) = (pair.local(), pair.remote());
    Some(PairView {
        local_kind: local.typ.into(),
        remote_kind: remote.typ.into(),
        local_addr: local.address.clone(),
        remote_addr: remote.address.clone(),
    })
}

// ---------------------------------------------------------------------------
// Today's memory, unchanged
// ---------------------------------------------------------------------------

/// FR-35 P2 — persists the session's stable rate for its peer when the pump
/// ends, whichever way it ends. The pump stores the governor's current stable
/// rate into `stable` once per viewer window; `0` = nothing worth keeping.
/// Moved from `peer.rs` as it was: `rate_memory.json` is written exactly as
/// before, in every mode.
struct RateMemoryGuard {
    path: Option<std::path::PathBuf>,
    peer: Option<String>,
    stable: AtomicU32,
    /// FR-79 V4 — what the OPENER measured about this carrier, when its burst
    /// queued enough for the estimator to say anything. Second-choice
    /// evidence: the session belief above is the whole session's answer.
    opener_measured: AtomicU32,
}

impl Drop for RateMemoryGuard {
    fn drop(&mut self) {
        let (Some(path), Some(peer)) = (self.path.as_ref(), self.peer.as_ref()) else {
            return;
        };
        // FR-79 V4 — a session writes what it MEASURED about this carrier, or
        // nothing at all. The belief the validity gate accepted is the
        // session's own answer; the opener's measurement stands in for a
        // session too short to have produced one. With neither, the entry
        // keeps its value AND its age, because refreshing a timestamp on no
        // evidence would keep a stale number alive past its TTL.
        let evidence = [
            self.stable.load(Ordering::Relaxed),
            self.opener_measured.load(Ordering::Relaxed),
        ]
        .into_iter()
        .find(|v| *v > 0);
        let mut mem = rate_memory::RateMemory::load(path);
        let kept = mem.record_session(peer, evidence, rate_memory::now_unix());
        if kept == 0 {
            return;
        }
        match mem.save(path) {
            Ok(()) => info!(
                peer,
                evidence_bps = ?evidence,
                kept_bps = kept,
                "FR-79 V4 rate memory: the carrier's measured rate remembered for the pair"
            ),
            Err(e) => {
                warn!(peer, %e, "FR-35 rate memory: could not persist (memory stays off for this pair)")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The path model's memory
// ---------------------------------------------------------------------------

/// The path model's side of one session: its key or why there is none, the
/// entry it opened from, and the last viewer window's would-be write.
struct ModelGuard {
    session_id: ObjectId,
    key: Result<PairKey, pm::NoKey>,
    path: Option<std::path::PathBuf>,
    /// What the open read of the pair's entry.
    state: EntryState,
    /// The fresh entry the session opened from.
    entry: Option<PairEntry>,
    /// The last viewer window's [`Closing`], before any change of path.
    last: Option<Closing>,
    /// A viewer window saw the path change: nothing after it is taken.
    frozen: bool,
}

impl ModelGuard {
    fn new(session_id: ObjectId, key: Result<PairKey, pm::NoKey>) -> Self {
        Self {
            session_id,
            key,
            path: None,
            state: EntryState::None,
            entry: None,
            last: None,
            frozen: false,
        }
    }

    /// One viewer window's would-be write. A session opens constrained, so a
    /// window that is not freezes the guard for good (design §2.7).
    fn take(&mut self, closing: Closing, constrained: bool) {
        if !constrained {
            self.frozen = true;
        }
        if !self.frozen {
            self.last = Some(closing);
        }
    }

    /// The close: what to write, and why.
    fn verdict(&self) -> (Option<PairEntry>, pm::Why) {
        if self.key.is_err() {
            return (None, pm::Why::NoKey);
        }
        match self.last {
            Some(c) => pm::close(self.entry.as_ref(), &c),
            None if self.frozen => (None, pm::Why::CarrierFlipped),
            None => (None, pm::Why::NothingMeasured),
        }
    }

    fn key_id(&self) -> String {
        self.key
            .as_ref()
            .map_or_else(|_| "none".to_string(), PairKey::id)
    }

    /// The open line: the pair's entry, and what the model would open at
    /// beside what today's rule opened at.
    fn open_line(&self, o: &OpenFacts, now_unix: u64) {
        let opening = pm::model_opening(
            o.nominal,
            o.hi_bps,
            self.entry.as_ref(),
            o.threshold,
            now_unix,
        );
        let floor = self.entry.map(|e| e.floor);
        let record = self.entry.and_then(|e| e.push_back);
        info!(
            session_id = %self.session_id,
            codec_label = o.codec_label,
            mode = %o.mode.word(),
            key_id = %self.key_id(),
            key_carrier = %self.key.as_ref().map_or("-", PairKey::carrier),
            key_net = %self.key.as_ref().map_or("-", |k| k.net().word()),
            no_key = %self.key.as_ref().err().map_or("-", |w| w.word()),
            entry = %self.state.word(),
            floor_bps = ?floor.map(|f| f.bps),
            floor_basis = %floor.map_or("-", |f| f.basis.word()),
            floor_age_s = ?floor.map(|f| now_unix.saturating_sub(f.at_unix)),
            push_back_bps = ?record.map(|r| r.bps),
            push_back_age_s = ?record.map(|r| now_unix.saturating_sub(r.at_unix)),
            push_back_windows = ?record.map(|r| r.windows),
            push_back_span_s = ?record.map(|r| r.span_s()),
            model_trusted = opening.trusted,
            nominal_bps = o.nominal,
            model_open_bps = opening.bps,
            model_profile = opening.profile,
            today_seed_bps = ?o.today_seed,
            today_open_bps = o.today_open,
            today_profile = o.today_profile,
            taken = %"today",
            "{OPEN_LINE}"
        );
    }
}

/// What the open line reads beside the entry.
struct OpenFacts {
    codec_label: &'static str,
    mode: Mode,
    /// N, the nominal constrained opening at the session's dial.
    nominal: u32,
    hi_bps: u32,
    /// The slow-link threshold as today's rule reads it; 0 = never.
    threshold: u32,
    today_seed: Option<u32>,
    /// What today's rule opened the session at.
    today_open: u32,
    today_profile: bool,
}

impl Drop for ModelGuard {
    fn drop(&mut self) {
        let (next, why) = self.verdict();
        let now_unix = rate_memory::now_unix();
        if let (Some(next), Ok(key), Some(path)) = (next, &self.key, &self.path) {
            let mut memory = PathMemory::load(path, now_unix);
            memory.write(key.text(), next, now_unix);
            if let Err(e) = memory.save(path) {
                warn!(
                    session_id = %self.session_id,
                    %e,
                    "FR-148 P1 path memory: could not write the model's file"
                );
            }
        }
        // The entry as it stands after the close: the one written, else the
        // one the session opened from.
        let stands = next.or(self.entry);
        let floor = stands.map(|e| e.floor);
        let record = stands.and_then(|e| e.push_back);
        let c = self.last;
        info!(
            session_id = %self.session_id,
            key_id = %self.key_id(),
            key_carrier = %self.key.as_ref().map_or("-", PairKey::carrier),
            key_net = %self.key.as_ref().map_or("-", |k| k.net().word()),
            wrote = %pm::wrote(self.entry.as_ref(), next.as_ref()),
            why = %why.word(),
            floor_bps = ?floor.map(|f| f.bps),
            floor_basis = %floor.map_or("-", |f| f.basis.word()),
            floor_age_s = ?floor.map(|f| now_unix.saturating_sub(f.at_unix)),
            best_bps = ?c.and_then(|c| c.best).map(|b| b.0),
            best_basis = %c.and_then(|c| c.best).map_or("-", |b| b.1.word()),
            push_back_bps = ?record.map(|r| r.bps),
            push_back_windows = ?record.map(|r| r.windows),
            push_back_span_s = ?record.map(|r| r.span_s()),
            samples_counted = c.map_or(0, |c| c.counted),
            samples_not_counted = c.map_or(0, |c| c.not_counted),
            samples_unread = c.map_or(0, |c| c.unread),
            record_ended = c.is_some_and(|c| c.ended),
            frozen = self.frozen,
            "{CLOSE_LINE}"
        );
    }
}

/// The path model's key and entry at the open.
async fn model_open(
    pc: &RTCPeerConnection,
    session_id: ObjectId,
    hi_bps: u32,
    remote: Option<&str>,
    overlay: Option<Overlay>,
) -> ModelGuard {
    if hi_bps == 0 {
        return ModelGuard::new(session_id, Err(pm::NoKey::LearnerOff));
    }
    if remote.is_none() {
        return ModelGuard::new(session_id, Err(pm::NoKey::NoPair));
    }
    let Some(pair) = selected_pair(pc).await else {
        return ModelGuard::new(session_id, Err(pm::NoKey::NoPair));
    };
    // The overlay's answer for the pair today's key was made from, unless the
    // pair moved since.
    let overlay = if Some(pair.remote_addr.as_str()) == remote {
        overlay
    } else {
        overlay_of(&pair.remote_addr).await
    };
    let path = overlay.as_ref().map(Overlay::path);
    let facts = PairFacts::of(session_id);
    let key = pm::pair_key(&KeyFacts {
        carrier: Some(sc::carrier_of(&pair, path.as_ref())),
        overlay_transport: overlay.and_then(|o| o.relay_transport),
        user: facts.as_ref().and_then(|f| f.user()),
        srflx: facts.as_ref().map(|f| f.srflx()).unwrap_or_default(),
        remote: pair
            .remote_addr
            .parse::<IpAddr>()
            .ok()
            .map(|ip| (pair.remote_kind, ip)),
    });
    let mut guard = ModelGuard::new(session_id, key);
    guard.path = pm::default_path();
    if let (Ok(key), Some(path)) = (&guard.key, &guard.path) {
        let now_unix = rate_memory::now_unix();
        let memory = PathMemory::load(path, now_unix);
        guard.state = memory.state(key.text(), now_unix);
        guard.entry = memory.fresh(key.text(), now_unix).copied();
    }
    guard
}

/// N — the nominal constrained opening: the relay cap at the session's
/// Priority dial, at least the nominal floor (design §3.1).
pub fn nominal_bps(priority: u8) -> u32 {
    let pct = crate::encode::dial_rate_factor_pct(priority) as u64;
    let bps = u64::from(crate::encode::relay_max_bps()) * pct / 100;
    u32::try_from(bps)
        .unwrap_or(u32::MAX)
        .max(crate::encode::MIN_BITRATE_BPS)
}

// ---------------------------------------------------------------------------
// The pump's side
// ---------------------------------------------------------------------------

/// The pump's pair memories, from its open to its end. Dropped with the pump:
/// today's file is written first, then the model's.
pub struct PumpMemory {
    codec_label: &'static str,
    mode: Mode,
    hi_bps: u32,
    seed: Option<u32>,
    seed_age: Option<Duration>,
    today: RateMemoryGuard,
    model: Option<ModelGuard>,
}

/// The pump's open: today's key and seed exactly as the pump formed them, and,
/// on a constrained session the switch has not turned `off`, the path model's
/// key and entry. `remote_ip` is the pump's own read of the selected remote.
pub async fn open<F, Fut>(
    pc: &RTCPeerConnection,
    session_id: ObjectId,
    codec_label: &'static str,
    mode: Mode,
    constrained: bool,
    hi_bps: u32,
    mut remote_ip: F,
) -> PumpMemory
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<String>>,
{
    // FR-35 P2 — per-peer rate memory: the pair's remembered stable rate
    // seeds the opening ceiling (85 % of it), so the opening keyframe is
    // sized by what the pair proved, not by the fleet constant. Keyed by the
    // nominated pair's remote address (the viewer host).
    let mut remote = None;
    let mut overlay = None;
    let peer_key = if constrained && hi_bps > 0 {
        // The pair may not be nominated yet at pump start (the 0.4.27 field
        // run's first packet came 4 s in): no key then means no seed AND no
        // write-back, i.e. the memory silently off for the session. Poll
        // briefly; the pump has nothing to send before the DC opens anyway.
        for _ in 0..20 {
            remote = remote_ip().await;
            if remote.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if remote.is_none() {
            info!(
                %session_id,
                "FR-35 rate memory: no nominated pair within 4 s of pump start — memory off for this session"
            );
        }
        // FR-79 V2 — the carrier is half the key: one overlay address can be
        // carried by a direct path, a UDP relay or DERP over TLS on different
        // days, with four times the capacity between the ends of that range.
        match remote.as_deref() {
            Some(addr) => {
                overlay = overlay_of(addr).await;
                let tag = overlay.as_ref().and_then(Overlay::today_tag);
                Some(rate_memory::memory_key(addr, tag.as_deref()))
            }
            None => None,
        }
    } else {
        None
    };
    let rate_memory_path = rate_memory::default_path();
    let (seed, seed_age) = match (&peer_key, &rate_memory_path) {
        (Some(peer), Some(path)) => crate::media_delivery::seed_at_open(path, peer),
        _ => (None, None),
    };
    if let Some(seed) = seed {
        info!(
            %session_id,
            codec_label,
            peer = peer_key.as_deref().unwrap_or("-"),
            seed_bps = seed,
            hi_bps,
            "FR-35 rate memory: opening ceiling seeded from the pair's remembered stable rate"
        );
    }
    let model = if constrained && mode.samples() {
        Some(model_open(pc, session_id, hi_bps, remote.as_deref(), overlay).await)
    } else {
        None
    };
    PumpMemory {
        codec_label,
        mode,
        hi_bps,
        seed,
        seed_age,
        today: RateMemoryGuard {
            path: rate_memory_path,
            peer: peer_key,
            stable: AtomicU32::new(0),
            opener_measured: AtomicU32::new(0),
        },
        model,
    }
}

impl PumpMemory {
    /// Today's seed, which opens the session as before.
    pub fn seed(&self) -> Option<u32> {
        self.seed
    }

    /// How old today's entry was at the open.
    pub fn seed_age(&self) -> Option<Duration> {
        self.seed_age
    }

    /// Right after the governor is built: open its memory side from the
    /// pair's entry, and write the open line, with what the model would open
    /// at beside what today's rule opens at. `today_profile` is whether
    /// today's slow-link profile engaged.
    pub fn at_open(&mut self, governor: &mut RateGovernor, priority: u8, today_profile: bool) {
        let Some(model) = self.model.as_ref() else {
            return;
        };
        let now_unix = rate_memory::now_unix();
        governor.open_memory(model.entry.as_ref(), now_unix, Instant::now());
        let nominal = nominal_bps(priority);
        // The slow-link threshold, as today's rule reads it (0 = never).
        let threshold = if crate::encode::slow_link_profile_enabled() {
            crate::encode::rate_profile::slow_link_profile_bps()
        } else {
            0
        };
        let today_open = governor
            .open_seed_bps()
            .unwrap_or_else(|| governor.ceiling_for(nominal, true));
        model.open_line(
            &OpenFacts {
                codec_label: self.codec_label,
                mode: self.mode,
                nominal,
                hi_bps: self.hi_bps,
                threshold,
                today_seed: self.seed,
                today_open,
                today_profile,
            },
            now_unix,
        );
    }

    /// At each viewer window: today's stable rate, as before, and the model's
    /// would-be write, until the first window whose path is not the opening's.
    pub fn window(&mut self, governor: &RateGovernor, constrained: bool) {
        let now = Instant::now();
        self.today.stable.store(
            governor.session_evidence_bps(now, constrained).unwrap_or(0),
            Ordering::Relaxed,
        );
        if let Some(model) = self.model.as_mut() {
            model.take(
                governor.memory().closing(now, rate_memory::now_unix()),
                constrained,
            );
        }
    }

    /// FR-79 V4 — what the opener measured, for today's memory.
    pub fn opener_measured(&self, bps: u32) {
        self.today.opener_measured.store(bps, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_carrier::{CandidateKind, SessionCarrier};

    const USER: &str = "65a1b2c3d4e5f60718293a4b";

    #[test]
    fn a_srflx_candidate_line_gives_its_address() {
        let srflx = "candidate:842163049 1 udp 1677729535 203.0.113.77 61413 typ srflx raddr 0.0.0.0 rport 0 generation 0 ufrag x network-cost 999";
        let ip: IpAddr = "203.0.113.77".parse().unwrap();
        assert_eq!(srflx_address(srflx), Some(ip));
        assert_eq!(srflx_address(&format!("a={srflx}")), Some(ip));
        let host = "candidate:1 1 udp 2122260223 192.168.1.20 61412 typ host generation 0";
        assert_eq!(srflx_address(host), None);
        let relay = "candidate:2 1 udp 41885439 198.51.100.10 3478 typ relay raddr 203.0.113.77 rport 61413";
        assert_eq!(srflx_address(relay), None);
        assert_eq!(srflx_address("garbage"), None);
        assert_eq!(srflx_address(""), None);
        let v6 = "candidate:3 1 udp 1677729535 2001:db8:77::5 50000 typ srflx raddr :: rport 0";
        assert_eq!(srflx_address(v6), Some("2001:db8:77::5".parse().unwrap()));
    }

    #[test]
    fn a_sessions_facts_live_as_long_as_its_peer() {
        let id = ObjectId::new();
        let facts = PairFacts::register(id);
        facts.set_user(ObjectId::parse_str(USER).unwrap());
        let srflx = "candidate:1 1 udp 1 203.0.113.77 1 typ srflx raddr 0.0.0.0 rport 0";
        facts.note_candidate(srflx);
        facts.note_candidate(srflx);
        facts.note_candidate("candidate:1 1 udp 1 192.168.1.2 1 typ host");
        let found = PairFacts::of(id).expect("registered");
        assert_eq!(found.user().as_deref(), Some(USER));
        assert_eq!(found.srflx(), ["203.0.113.77".parse::<IpAddr>().unwrap()]);
        drop(found);
        drop(facts);
        assert!(PairFacts::of(id).is_none());
    }

    /// Today's tag is formed from the overlay's answer exactly as the pump
    /// formed it (`format!("{:?}")`, lowercased, then `carrier_tag`).
    #[test]
    fn todays_tag_is_formed_as_before() {
        use tunnel_core::localapi::ConnectionType;
        let o = Overlay {
            connection: ConnectionType::Relay,
            relay_kind: Some("derp".into()),
            relay_transport: Some("tcp".into()),
        };
        assert_eq!(
            o.today_tag(),
            rate_memory::carrier_tag("relay", Some("derp"), Some("tcp"))
        );
        assert_eq!(o.path(), OverlayPath::Relay(Some("derp".into())));
        let direct = Overlay {
            connection: ConnectionType::Direct,
            relay_kind: None,
            relay_transport: None,
        };
        assert_eq!(direct.today_tag(), Some("direct".to_string()));
        assert_eq!(direct.path(), OverlayPath::Direct);
    }

    fn closing(bps: u32) -> Closing {
        Closing {
            floor: Some(pm::Floor {
                bps,
                basis: pm::FloorBasis::AppLimited,
                at_unix: 1_795_000_000,
            }),
            record: None,
            best: Some((bps, pm::FloorBasis::AppLimited)),
            not_counted: 0,
            unread: 0,
            counted: 0,
            ended: false,
            measured: true,
            at_unix: 1_795_000_000,
        }
    }

    fn turn_key() -> Result<PairKey, pm::NoKey> {
        pm::pair_key(&KeyFacts {
            carrier: Some(SessionCarrier::Turn),
            overlay_transport: None,
            user: Some(USER.into()),
            srflx: vec!["203.0.113.77".parse().unwrap()],
            remote: Some((CandidateKind::Relay, "198.51.100.10".parse().unwrap())),
        })
    }

    /// Design §2.7 — a relay session that escaped to direct writes what it
    /// measured on the relay: 2 M, not the 15 M after the escape. Master's
    /// per-window store keeps moving with every window.
    #[test]
    fn a_change_of_path_freezes_the_offer() {
        let mut g = ModelGuard::new(ObjectId::new(), turn_key());
        g.take(closing(2_000_000), true);
        g.take(closing(15_000_000), false);
        g.take(closing(15_000_000), true);
        assert!(g.frozen);
        let (next, why) = g.verdict();
        assert_eq!(why, pm::Why::FirstMeasurement);
        assert_eq!(next.map(|e| e.floor.bps), Some(2_000_000));
        // A change before any reading writes nothing.
        let mut early = ModelGuard::new(ObjectId::new(), turn_key());
        early.take(closing(15_000_000), false);
        assert_eq!(early.verdict(), (None, pm::Why::CarrierFlipped));
        // No key writes nothing, whatever was measured.
        let mut none = ModelGuard::new(ObjectId::new(), Err(pm::NoKey::NoUser));
        none.take(closing(2_000_000), true);
        assert_eq!(none.verdict(), (None, pm::Why::NoKey));
        // Nothing taken: nothing measured.
        let idle = ModelGuard::new(ObjectId::new(), turn_key());
        assert_eq!(idle.verdict(), (None, pm::Why::NothingMeasured));
    }

    /// What `f` logs, as the daemon's log file writes it (time, level,
    /// message, fields; no target), as `media_delivery`'s cells read theirs.
    fn logged(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let writer = buf.clone();
        // Two dispatchers, never one: with one, a callsite's interest is taken
        // once for every thread.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
    }

    /// A pump's memories with no file on either side: today's guard writes
    /// nothing, the model's writes only its line.
    fn pump(model: ModelGuard, seed: Option<u32>) -> PumpMemory {
        PumpMemory {
            codec_label: "AV1",
            mode: Mode::Shadow,
            hi_bps: 8_000_000,
            seed,
            seed_age: None,
            today: RateMemoryGuard {
                path: None,
                peer: None,
                stable: AtomicU32::new(0),
                opener_measured: AtomicU32::new(0),
            },
            model: Some(model),
        }
    }

    /// The pump's open, with a governor built as the pump builds it.
    fn open_line(p: &mut PumpMemory, now: Instant) -> String {
        let mut governor = RateGovernor::new(
            30,
            12,
            crate::encode::governor::GovernorFlags::from_env(),
            p.hi_bps,
            p.seed,
            now,
        );
        logged(|| p.at_open(&mut governor, 0, false))
    }

    fn assert_in_order(line: &str, fields: &[&str]) {
        let mut rest = line;
        for f in fields {
            let at = rest
                .find(&format!(" {f}="))
                .unwrap_or_else(|| panic!("{f} is not in order in {line:?}"));
            rest = &rest[at + f.len() + 2..];
        }
        assert_eq!(line.lines().count(), 1, "{line:?}");
    }

    const OPEN_FIELDS: [&str; 23] = [
        "session_id",
        "codec_label",
        "mode",
        "key_id",
        "key_carrier",
        "key_net",
        "no_key",
        "entry",
        "floor_bps",
        "floor_basis",
        "floor_age_s",
        "push_back_bps",
        "push_back_age_s",
        "push_back_windows",
        "push_back_span_s",
        "model_trusted",
        "nominal_bps",
        "model_open_bps",
        "model_profile",
        "today_seed_bps",
        "today_open_bps",
        "today_profile",
        "taken",
    ];

    const CLOSE_FIELDS: [&str; 19] = [
        "session_id",
        "key_id",
        "key_carrier",
        "key_net",
        "wrote",
        "why",
        "floor_bps",
        "floor_basis",
        "floor_age_s",
        "best_bps",
        "best_basis",
        "push_back_bps",
        "push_back_windows",
        "push_back_span_s",
        "samples_counted",
        "samples_not_counted",
        "samples_unread",
        "record_ended",
        "frozen",
    ];

    /// Both lines name their fields in this order, carry no address and no
    /// user, and read as below on the three sessions a field read meets first
    /// (printed with `--nocapture`): a pair's first session, a session that
    /// measured nothing, and a session that counted three push-backs, with the
    /// next session's open.
    #[test]
    fn the_two_lines_name_their_fields() {
        use crate::encode::pipe::Basis;
        let _env = crate::encode::RELAY_ENV_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let now_unix = rate_memory::now_unix();
        let sid = ObjectId::parse_str("65a1b2c3d4e5f60718293a5c").unwrap();
        // `ModelGuard` writes at its drop, so no struct update.
        let guard = |state: EntryState, entry: Option<PairEntry>| {
            let mut g = ModelGuard::new(sid, turn_key());
            g.state = state;
            g.entry = entry;
            g
        };
        let mut lines = Vec::new();

        // 1. The pair's first session: no entry; it delivers 2.4 M.
        let mut p = pump(guard(EntryState::None, None), None);
        let open = open_line(&mut p, t0);
        let mut mem = pm::SessionMemory::default();
        mem.open(None, now_unix, t0);
        mem.delivered(1_900_000, Basis::AppLimited, at(5));
        mem.delivered(2_400_000, Basis::AppLimited, at(20));
        p.model
            .as_mut()
            .unwrap()
            .take(mem.closing(at(60), now_unix), true);
        let close = logged(|| drop(p));
        assert!(open.contains(" entry=none ") && open.contains(" model_trusted=false "));
        assert!(
            close.contains(" wrote=floor why=first_measurement "),
            "{close}"
        );
        assert!(close.contains(" floor_bps=Some(2400000) floor_basis=app_limited "));
        lines.push(("a pair's first session", open, close));

        // 2. A session that measured nothing, on a 3-day-old floor.
        let entry = PairEntry {
            floor: pm::Floor {
                bps: 2_200_000,
                basis: pm::FloorBasis::AppLimited,
                at_unix: now_unix - 3 * 86_400,
            },
            push_back: None,
        };
        let mut p = pump(guard(EntryState::Fresh, Some(entry)), Some(2_200_000));
        let open = open_line(&mut p, t0);
        let mut mem = pm::SessionMemory::default();
        mem.open(Some(&entry), now_unix, t0);
        p.model
            .as_mut()
            .unwrap()
            .take(mem.closing(at(40), now_unix), true);
        let close = logged(|| drop(p));
        assert!(open.contains(" entry=fresh ") && open.contains(" floor_bps=Some(2200000) "));
        assert!(
            close.contains(" wrote=nothing why=nothing_measured "),
            "{close}"
        );
        lines.push(("a session that measured nothing", open, close));

        // 3. Three backlogged push-backs over 13 s under a 1.4 M floor ...
        let entry = PairEntry {
            floor: pm::Floor {
                bps: 1_400_000,
                basis: pm::FloorBasis::AppLimited,
                at_unix: now_unix - 86_400,
            },
            push_back: None,
        };
        let mut p = pump(guard(EntryState::Fresh, Some(entry)), None);
        let open = open_line(&mut p, t0);
        let mut mem = pm::SessionMemory::default();
        mem.open(Some(&entry), now_unix, t0);
        mem.delivered(1_200_000, Basis::AppLimited, at(5));
        mem.pushed_back(620_000, Basis::Backlogged, at(10));
        mem.pushed_back(600_000, Basis::Backlogged, at(16));
        mem.pushed_back(640_000, Basis::Backlogged, at(23));
        let c = mem.closing(at(30), now_unix);
        let (written, _) = pm::close(Some(&entry), &c);
        let written = written.expect("the push-backs are written");
        p.model.as_mut().unwrap().take(c, true);
        let close = logged(|| drop(p));
        assert!(
            close.contains(" wrote=floor_and_push_back why=pushed_back "),
            "{close}"
        );
        assert!(close.contains(" push_back_bps=Some(640000) push_back_windows=Some(3) "));
        lines.push(("three push-backs", open, close));
        // ... and the next session reads a trusted record.
        let mut p = pump(guard(EntryState::Fresh, Some(written)), None);
        let open = open_line(&mut p, t0);
        let expect = (u64::from(written.floor.bps) * 85 / 100) as u32;
        assert!(open.contains(" model_trusted=true "), "{open}");
        assert!(
            open.contains(&format!(" model_open_bps={expect} ")),
            "{expect}: {open}"
        );
        drop(p);
        lines.push(("the session after it", open, String::new()));

        for (what, open, close) in &lines {
            assert_in_order(open, &OPEN_FIELDS);
            assert!(open.contains(OPEN_LINE));
            if !close.is_empty() {
                assert_in_order(close, &CLOSE_FIELDS);
                assert!(close.contains(CLOSE_LINE));
            }
            for secret in ["203.0.113", "198.51.100", USER] {
                assert!(
                    !open.contains(secret) && !close.contains(secret),
                    "{secret}"
                );
            }
            println!(
                "{what}: open {} bytes, close {} bytes\n{open}{close}",
                open.len(),
                close.len()
            );
            assert!(open.len() < 800 && close.len() < 800);
        }
    }
}
