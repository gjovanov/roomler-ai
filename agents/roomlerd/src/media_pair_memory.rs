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
//! | read at the open | seeds the opener, as before, unless the model decides | under `on`, with a key and a fresh entry, decides the opening (D25) and the slow-link profile (D26); else the open line only |
//! | written at the close | as before | what the session measured in valid windows (D24, D25), under one lock |
//! | lines | as before | [`OPEN_LINE`] and [`CLOSE_LINE`] |
//!
//! The switch at `off` keeps the path model's side out entirely: no read, no
//! write, no line (`media_delivery::Mode`). At `shadow`, the default (D28),
//! the opener is today's byte for byte.
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
    /// Which rule opened the session (step 3), and when the open read the
    /// entry.
    taken: pm::Taken,
    opened_unix: u64,
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
            taken: pm::Taken::Today(pm::TodayWhy::Shadow),
            opened_unix: 0,
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

    /// The open line: the pair's entry, what the model opens at beside what
    /// today's rule opens at, and which of the two the session took.
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
            taken = %self.taken.word(),
            taken_why = %self.taken.why(),
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
    /// What today's rule opens the session at.
    today_open: u32,
    today_profile: bool,
}

impl Drop for ModelGuard {
    fn drop(&mut self) {
        let (next, why) = self.verdict();
        let now_unix = rate_memory::now_unix();
        if let (Some(next), Ok(key), Some(path)) = (next, &self.key, &self.path) {
            let written = PathMemory::update(path, now_unix, |memory| {
                memory.write(key.text(), next, now_unix);
            });
            if let Err(e) = written {
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
            samples_counted_overproduced = c.map_or(0, |c| c.counted_overproduced),
            samples_counted_clear = c.map_or(0, |c| c.counted_clear),
            samples_not_counted = c.map_or(0, |c| c.not_counted),
            samples_unread = c.map_or(0, |c| c.unread),
            samples_viewer_late = c.map_or(0, |c| c.viewer_late),
            samples_under_plan = c.map_or(0, |c| c.under_plan),
            record_ended = c.is_some_and(|c| c.ended),
            frozen = self.frozen,
            // FR-148 D46 B2 — of `samples_counted`, the delivery samples'
            // push-backs: P1 (a standing queue) and P3 (backlogged under the
            // in-flight budget, no still span in it).
            samples_counted_p1 = c.map_or(0, |c| c.counted_p1.0),
            samples_counted_p3 = c.map_or(0, |c| c.counted_p3.0),
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
    /// The slow-link threshold as today's rule reads it; 0 = never.
    threshold: u32,
    /// Today's seed and its age.
    seed: Option<u32>,
    seed_age: Option<Duration>,
    today: RateMemoryGuard,
    model: Option<ModelGuard>,
}

/// The pump's open: today's key and seed exactly as the pump formed them, and,
/// on a constrained session the switch has not turned `off`, the path model's
/// key and entry, and which rule opens the session. `remote_ip` is the pump's
/// own read of the selected remote.
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
    let mut memory = PumpMemory {
        codec_label,
        mode,
        hi_bps,
        // As today's rule reads it.
        threshold: if crate::encode::slow_link_profile_enabled() {
            crate::encode::rate_profile::slow_link_profile_bps()
        } else {
            0
        },
        seed,
        seed_age,
        today: RateMemoryGuard {
            path: rate_memory_path,
            peer: peer_key,
            stable: AtomicU32::new(0),
            opener_measured: AtomicU32::new(0),
        },
        model,
    };
    memory.decide(rate_memory::now_unix());
    memory
}

impl PumpMemory {
    /// Step 3 — which rule opens the session: the model's under `on` with a
    /// key and a fresh entry, else today's.
    fn decide(&mut self, now_unix: u64) {
        let on = self.mode == Mode::On;
        if let Some(m) = self.model.as_mut() {
            m.taken = pm::Taken::decide(on, m.key.is_ok(), m.state);
            m.opened_unix = now_unix;
        }
    }

    /// The entry and the opening that drive the session, when the model
    /// decided it. N does not enter them: after a trusted push-back the
    /// opening is `clamp(⌊0.85·D⌋, 200 k, hi)`, and without one the session
    /// opens at the ceiling, which the learner seeded with D makes
    /// `max(N, min(⌊0.85·D⌋, hi))` at the plan's own N. Only the open line
    /// needs N, for the model's opening it logs.
    fn driving(&self) -> Option<(PairEntry, pm::ModelOpening)> {
        let m = self.model.as_ref()?;
        let entry = m.entry.filter(|_| m.taken == pm::Taken::Model)?;
        let opening =
            pm::model_opening(0, self.hi_bps, Some(&entry), self.threshold, m.opened_unix);
        Some((entry, opening))
    }

    /// The remembered rate the slow-link profile and the viewer's cap detail
    /// read: today's seed, or, when the model decided, its opening when D26
    /// engages the profile (and nothing otherwise).
    pub fn seed(&self) -> Option<u32> {
        match self.driving() {
            Some((_, opening)) => opening.profile.then_some(opening.bps),
            None => self.seed,
        }
    }

    /// The age of what the P0 model remembers at the open: today's entry's,
    /// or the model's floor's when the model decided.
    pub fn seed_age(&self) -> Option<Duration> {
        match (self.driving(), self.model.as_ref()) {
            (Some((entry, _)), Some(m)) => Some(Duration::from_secs(
                m.opened_unix.saturating_sub(entry.floor.at_unix),
            )),
            _ => self.seed_age,
        }
    }

    /// What the governor is built from: today's seed, or the model's plan.
    pub fn plan(&self) -> pm::OpenPlan {
        match self.driving() {
            Some((entry, opening)) => pm::OpenPlan::model(&entry, opening),
            None => pm::OpenPlan::Today(self.seed),
        }
    }

    /// Right after the governor is built: open its memory side from the
    /// pair's entry, and write the open line: what the model opens at beside
    /// what today's rule opens at, and which the session took.
    /// `profile_engaged` is whether the session opened on the slow-link
    /// profile, which is today's rule's unless the model decided.
    pub fn at_open(&mut self, governor: &mut RateGovernor, priority: u8, profile_engaged: bool) {
        let Some(model) = self.model.as_ref() else {
            return;
        };
        let now_unix = rate_memory::now_unix();
        governor.open_memory(model.entry.as_ref(), now_unix, Instant::now());
        let nominal = nominal_bps(priority);
        let today_profile = match model.taken {
            pm::Taken::Today(_) => profile_engaged,
            pm::Taken::Model => crate::encode::rate_profile::slow_link_profile(
                self.seed,
                crate::encode::slow_link_profile_enabled(),
            )
            .is_some(),
        };
        let today_open = governor.opening_of(&pm::OpenPlan::Today(self.seed), nominal);
        model.open_line(
            &OpenFacts {
                codec_label: self.codec_label,
                mode: self.mode,
                nominal,
                hi_bps: self.hi_bps,
                threshold: self.threshold,
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
            viewer_late: 0,
            under_plan: 0,
            counted: 0,
            counted_overproduced: 0,
            counted_clear: 0,
            counted_p1: (0, 0),
            counted_p3: (0, 0),
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

    /// Master's opener (base `01e8455ab`, `RateGovernor::new(…, seed, …)`),
    /// recorded before step 3 changed a line, for each of today's seeds: with
    /// the relief on and the shadow controller, with the relief off, and with
    /// slow start on and no shadow controller. The fields are [`Opened`]'s.
    #[rustfmt::skip]
    const MASTER: [(Option<u32>, [Opened; 3]); 8] = {
        const fn s(v: u32) -> Option<u32> { Some(v) }
        const N: Option<u32> = None;
        [
            (None, [
                (N, 0, 2_550_000, N, N, s(2_550_000), N, s(200_000)),
                (N, 0, 2_550_000, N, N, s(2_550_000), N, s(200_000)),
                (N, 0, 2_550_000, N, N, s(300_000), s(300_000), N),
            ]),
            (s(200_000), [
                (s(200_000), 170_000, 2_550_000, s(200_000), s(200_000), s(200_000), s(200_000), s(200_000)),
                (N, 170_000, 2_550_000, N, N, s(2_550_000), N, s(200_000)),
                (s(200_000), 170_000, 2_550_000, s(200_000), s(200_000), s(200_000), s(200_000), N),
            ]),
            (s(900_000), [
                (s(900_000), 765_000, 2_550_000, s(900_000), s(900_000), s(900_000), s(765_000), s(765_000)),
                (N, 765_000, 2_550_000, N, N, s(2_550_000), N, s(765_000)),
                (s(900_000), 765_000, 2_550_000, s(900_000), s(900_000), s(900_000), s(765_000), N),
            ]),
            (s(1_200_000), [
                (s(1_200_000), 1_020_000, 2_550_000, s(1_200_000), s(1_200_000), s(1_200_000), s(1_020_000), s(1_020_000)),
                (N, 1_020_000, 2_550_000, N, N, s(2_550_000), N, s(1_020_000)),
                (s(1_200_000), 1_020_000, 2_550_000, s(1_200_000), s(1_200_000), s(1_200_000), s(1_020_000), N),
            ]),
            (s(1_600_000), [
                (N, 1_360_000, 2_550_000, N, N, s(2_550_000), N, s(1_360_000)),
                (N, 1_360_000, 2_550_000, N, N, s(2_550_000), N, s(1_360_000)),
                (N, 1_360_000, 2_550_000, N, N, s(300_000), s(300_000), N),
            ]),
            (s(2_000_000), [
                (N, 1_700_000, 2_550_000, N, N, s(2_550_000), N, s(1_700_000)),
                (N, 1_700_000, 2_550_000, N, N, s(2_550_000), N, s(1_700_000)),
                (N, 1_700_000, 2_550_000, N, N, s(300_000), s(300_000), N),
            ]),
            (s(3_500_000), [
                (N, 2_975_000, 2_975_000, N, N, s(2_975_000), N, s(2_975_000)),
                (N, 2_975_000, 2_975_000, N, N, s(2_975_000), N, s(2_975_000)),
                (N, 2_975_000, 2_975_000, N, N, s(300_000), s(300_000), N),
            ]),
            (s(9_400_000), [
                (N, 7_990_000, 7_990_000, N, N, s(7_990_000), N, s(7_990_000)),
                (N, 7_990_000, 7_990_000, N, N, s(7_990_000), N, s(7_990_000)),
                (N, 7_990_000, 7_990_000, N, N, s(300_000), s(300_000), N),
            ]),
        ]
    };

    /// The three governor set-ups [`MASTER`] was recorded under.
    const SETUPS: [(bool, bool, crate::encode::RatectlMode); 3] = [
        (true, false, crate::encode::RatectlMode::Shadow),
        (false, false, crate::encode::RatectlMode::Shadow),
        (true, true, crate::encode::RatectlMode::Off),
    ];

    /// D28's twin test: under `shadow` and `off` the opener is master's, bit
    /// for bit, whatever the path model holds. For each of today's seeds (none,
    /// 0.2 to 9.4 Mbit/s) and each model entry (none, a floor, a trusted
    /// push-back that would open under the profile), the pump's plan is
    /// today's seed, its profile seed and age are today's, and the governor
    /// built from it acts on master's recorded values, `open_at_bps` (what
    /// the pump's two encoder opens read; on master they read the seed)
    /// included; the open line's `today_open_bps` is master's expression.
    /// Change one filter (the 1.5 Mbit/s bound of P8) and a row goes red.
    /// One `N` and one `hi`: master's values for others were not recorded.
    #[test]
    fn shadow_is_todays_opener_exactly() {
        let now_unix = rate_memory::now_unix();
        let floor = pm::Floor {
            bps: 2_200_000,
            basis: pm::FloorBasis::AppLimited,
            at_unix: now_unix - 86_400,
        };
        let pushed = PairEntry {
            floor: pm::Floor {
                bps: 850_000,
                basis: pm::FloorBasis::Backlogged,
                ..floor
            },
            push_back: Some(pm::PushBack {
                bps: 400_000,
                at_unix: now_unix - 60,
                first_at_unix: now_unix - 120,
                windows: 3,
            }),
        };
        let entries = [
            (EntryState::None, None),
            (
                EntryState::Fresh,
                Some(PairEntry {
                    floor,
                    push_back: None,
                }),
            ),
            (EntryState::Fresh, Some(pushed)),
        ];
        for (seed, master) in MASTER {
            for (state, entry) in entries {
                for mode in [Mode::Shadow, Mode::Off] {
                    let guard = (mode != Mode::Off).then(|| guard_with(state, entry));
                    let p = pump_in(mode, guard, seed);
                    let at = format!("{mode:?} seed {seed:?} entry {entry:?}");
                    assert_eq!(p.plan(), pm::OpenPlan::Today(seed), "{at}");
                    assert_eq!(p.seed(), seed, "{at}");
                    assert_eq!(p.seed_age(), seed.map(|_| Duration::from_secs(3_600)));
                    for ((relief, slow_start, ratectl), want) in SETUPS.into_iter().zip(master) {
                        let (got, g) = opened(&p.plan(), relief, slow_start, ratectl);
                        assert_eq!(got, want, "{at} relief {relief} slow start {slow_start}");
                        // What the pump's two encoder opens read: `open_at_bps`
                        // here, `open_seed_bps` on master.
                        assert_eq!(g.open_at_bps(), want.0, "{at} relief {relief}: open_at");
                        // The open line's `today_open_bps`, as master wrote it.
                        assert_eq!(
                            g.opening_of(&pm::OpenPlan::Today(seed), 2_550_000),
                            want.0.unwrap_or(want.2),
                            "{at}"
                        );
                    }
                }
            }
        }
    }

    /// Steps 3 and 4 under `on` (design §3.3 and §4; N = 2.55 Mbit/s, hi =
    /// 8 Mbit/s): the governor built from the pump's plan opens where the
    /// model says, and the slow-link profile engages on a fresh trusted low
    /// push-back only. Beside each, what master opens the same pair at from
    /// today's seed, which `shadow` keeps.
    #[test]
    fn on_opens_where_the_model_says() {
        use crate::encode::RatectlMode::Shadow;
        use crate::encode::rate_profile::slow_link_profile;
        let now_unix = rate_memory::now_unix();
        let floor = |bps, basis| pm::Floor {
            bps,
            basis,
            at_unix: now_unix - 3_600,
        };
        let record = |bps, age: u64| {
            Some(pm::PushBack {
                bps,
                at_unix: now_unix - age,
                first_at_unix: now_unix - age - 60,
                windows: 3,
            })
        };
        let (app, back) = (pm::FloorBasis::AppLimited, pm::FloorBasis::Backlogged);
        // (floor, record, master's seed for the pair, the opening, profile)
        let rows = [
            // AC3's CORPLAP-2 chain: master wrote 179,980 and opens there,
            // on the profile.
            (floor(1_399_272, app), None, Some(179_980), 2_550_000, false),
            // The other chain: master wrote 3,322 and opens at the relief
            // floor, on the profile.
            (floor(15_915_240, app), None, Some(3_322), 8_000_000, false),
            // §2.1: an application-limited floor engages no profile; master
            // engages it at a measured 281,732.
            (floor(281_732, app), None, Some(281_732), 2_550_000, false),
            (floor(2_170_000, app), None, None, 2_550_000, false),
            (floor(6_500_000, back), None, None, 5_525_000, false),
            (
                floor(2_500_000, back),
                record(1_000_000, 60),
                None,
                2_125_000,
                false,
            ),
            (
                floor(1_300_000, back),
                record(400_000, 60),
                None,
                1_105_000,
                false,
            ),
            (
                floor(850_000, back),
                record(400_000, 60),
                None,
                722_500,
                true,
            ),
            // The same push-back 25 h later: a floor, at N.
            (
                floor(850_000, back),
                record(400_000, 25 * 3_600),
                None,
                2_550_000,
                false,
            ),
        ];
        for (floor, push_back, master_seed, open_bps, profile) in rows {
            let entry = PairEntry { floor, push_back };
            let p = pump_in(
                Mode::On,
                Some(guard_with(EntryState::Fresh, Some(entry))),
                master_seed,
            );
            let at = format!("{entry:?}");
            assert_eq!(p.model.as_ref().unwrap().taken.word(), "model", "{at}");
            let ((_, _, _, _, _, tick, _, _), g) = opened(&p.plan(), true, false, Shadow);
            assert_eq!(tick, Some(open_bps), "{at}");
            assert_eq!(
                g.open_at_bps(),
                (open_bps < 2_550_000).then_some(open_bps),
                "the first encoder opens there too: {at}"
            );
            assert_eq!(slow_link_profile(p.seed(), true).is_some(), profile, "{at}");
            let age = p.seed_age().map(|a| a.as_secs());
            assert!(
                age.is_some_and(|a| (3_600..3_603).contains(&a)),
                "the floor's: {age:?}"
            );
            // Master on the same pair.
            if let Some(s) = master_seed {
                let ((_, _, _, _, _, master_tick, _, _), _) =
                    opened(&pm::OpenPlan::Today(Some(s)), true, false, Shadow);
                assert!(master_tick.unwrap() < open_bps, "{at}: {master_tick:?}");
                assert!(slow_link_profile(Some(s), true).is_some(), "{at}");
            }
        }
        // With the relief off, an opening under the nominal floor is lifted
        // to it at the open, where the AIMD would lift it a tick later.
        let entry = PairEntry {
            floor: floor(850_000, back),
            push_back: record(400_000, 60),
        };
        let p = pump_in(
            Mode::On,
            Some(guard_with(EntryState::Fresh, Some(entry))),
            None,
        );
        let ((open_seed, _, _, _, _, tick, _, _), g) = opened(&p.plan(), false, false, Shadow);
        assert_eq!(tick, Some(crate::encode::MIN_BITRATE_BPS));
        assert_eq!(g.open_at_bps(), Some(crate::encode::MIN_BITRATE_BPS));
        assert_eq!(open_seed, None, "P8's seed needs the relief");
    }

    /// Ruling 3: the open line says which rule opened the session and, for
    /// today's under `on`, why. Every `today` opens from today's seed, as
    /// master does.
    #[test]
    fn the_open_line_says_which_rule_opened_and_why() {
        let _env = crate::encode::RELAY_ENV_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let now_unix = rate_memory::now_unix();
        let entry = PairEntry {
            floor: pm::Floor {
                bps: 2_200_000,
                basis: pm::FloorBasis::AppLimited,
                at_unix: now_unix - 3_600,
            },
            push_back: None,
        };
        let no_user = || Err(pm::NoKey::NoUser);
        let cases = [
            (
                Mode::On,
                turn_key(),
                EntryState::Fresh,
                Some(entry),
                "model",
                "-",
            ),
            (
                Mode::On,
                turn_key(),
                EntryState::None,
                None,
                "today",
                "no_entry",
            ),
            (
                Mode::On,
                turn_key(),
                EntryState::Invalid,
                None,
                "today",
                "entry_invalid",
            ),
            (
                Mode::On,
                turn_key(),
                EntryState::Future,
                None,
                "today",
                "entry_invalid",
            ),
            (
                Mode::On,
                turn_key(),
                EntryState::Expired,
                None,
                "today",
                "floor_expired",
            ),
            (
                Mode::On,
                no_user(),
                EntryState::None,
                None,
                "today",
                "no_key",
            ),
            (
                Mode::Shadow,
                turn_key(),
                EntryState::Fresh,
                Some(entry),
                "today",
                "shadow",
            ),
        ];
        for (mode, key, state, entry, taken, why) in cases {
            let mut g = ModelGuard::new(ObjectId::new(), key);
            g.state = state;
            g.entry = entry;
            let mut p = pump_in(mode, Some(g), Some(900_000));
            let line = open_line(&mut p, Instant::now());
            let says = format!(" taken={taken} taken_why={why}");
            assert!(line.contains(&says), "{says} in {line}");
            if taken == "today" {
                assert_eq!(p.plan(), pm::OpenPlan::Today(Some(900_000)), "{line}");
                assert_eq!(p.seed(), Some(900_000));
            }
        }
    }

    /// AC6 under `on` (design §7): one session delivers 6.5 Mbit/s, then
    /// twenty sessions of a still screen, application-limited at 2.17 Mbit/s
    /// at most. Each opens at 5.525 Mbit/s or more, and the entry's floor and
    /// its time do not move. Read and written through the file, as the pump
    /// reads and writes it.
    #[test]
    fn twenty_still_screens_keep_the_floor_and_its_age() {
        use crate::encode::RatectlMode::Shadow;
        use crate::encode::pipe::Basis;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(pm::FILE_NAME);
        let t0 = Instant::now();
        let now_unix = rate_memory::now_unix();
        let text = turn_key().unwrap().text().to_string();
        let session = |delivered: u32| {
            // The open, as `model_open` reads the file.
            let memory = PathMemory::load(&path, now_unix);
            let mut g = guard_with(
                memory.state(&text, now_unix),
                memory.fresh(&text, now_unix).copied(),
            );
            g.path = Some(path.clone());
            let mut p = pump_in(Mode::On, Some(g), None);
            let ((_, _, _, _, _, tick, _, _), _) = opened(&p.plan(), true, false, Shadow);
            let model = p.model.as_mut().unwrap();
            let mut mem = pm::SessionMemory::default();
            mem.open(model.entry.as_ref(), now_unix, t0);
            mem.delivered(delivered, Basis::AppLimited, t0 + Duration::from_secs(20));
            model.take(mem.closing(t0 + Duration::from_secs(60), now_unix), true);
            drop(p);
            tick.unwrap()
        };
        assert_eq!(session(6_500_000), 2_550_000, "the pair's first session");
        let first = PathMemory::load(&path, now_unix).pairs[&text];
        assert_eq!(first.floor.bps, 6_500_000);
        for n in 1..=20 {
            let tick = session(2_170_000);
            assert!(tick >= 5_525_000, "session {n} opened at {tick}");
            assert_eq!(PathMemory::load(&path, now_unix).pairs[&text], first, "{n}");
        }
    }

    /// Ruling 4: a move between two relay tiers inside a session (TURN to the
    /// mesh's relay, or back) keeps the path constrained, so it is no freeze:
    /// the close writes the last window under the opening carrier's key. Only
    /// a move off the relay freezes (design §2.7). Kept, not frozen: the pump
    /// reads relay-or-not at each recheck, not the carrier (docs).
    #[test]
    fn a_move_between_relay_tiers_keeps_the_write() {
        let mut g = ModelGuard::new(ObjectId::new(), turn_key());
        let opening_key = g.key_id();
        g.take(closing(2_000_000), true);
        // The pair moves to the mesh's relay: still constrained.
        g.take(closing(4_000_000), true);
        assert!(!g.frozen);
        assert_eq!(g.verdict().0.map(|e| e.floor.bps), Some(4_000_000));
        assert_eq!(g.key_id(), opening_key, "under the opening carrier's key");
        // A move to a direct pair freezes: nothing after it is taken.
        g.take(closing(15_000_000), false);
        assert!(g.frozen);
        assert_eq!(g.verdict().0.map(|e| e.floor.bps), Some(4_000_000));
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

    /// A pump's memories with no file on either side, under `shadow`: today's
    /// guard writes nothing, the model's writes only its line.
    fn pump(model: ModelGuard, seed: Option<u32>) -> PumpMemory {
        pump_in(Mode::Shadow, Some(model), seed)
    }

    /// The same under `mode` (no guard under `off`, as `open` builds it),
    /// decided at the open as `open` decides, with a slow-link threshold of
    /// 1 Mbit/s. Today's entry, when there is one, is an hour old.
    fn pump_in(mode: Mode, model: Option<ModelGuard>, seed: Option<u32>) -> PumpMemory {
        let mut p = PumpMemory {
            codec_label: "AV1",
            mode,
            hi_bps: 8_000_000,
            threshold: 1_000_000,
            seed,
            seed_age: seed.map(|_| Duration::from_secs(3_600)),
            today: RateMemoryGuard {
                path: None,
                peer: None,
                stable: AtomicU32::new(0),
                opener_measured: AtomicU32::new(0),
            },
            model,
        };
        p.decide(rate_memory::now_unix());
        p
    }

    /// A guard on the TURN key whose open read `state` and `entry`.
    fn guard_with(state: EntryState, entry: Option<PairEntry>) -> ModelGuard {
        let mut g = ModelGuard::new(ObjectId::new(), turn_key());
        g.state = state;
        g.entry = entry;
        g
    }

    /// What a session acts on at its open, from a governor built from `plan`
    /// as the pump builds it (relay cap 8 Mbit/s, plan ceiling N): the P8
    /// seed, the learned ceiling, the ceiling for N, the prior, the pipe
    /// stand-in, the first tick's target, the floor in force after it and the
    /// shadow controller's target.
    type Opened = (
        Option<u32>,
        u32,
        u32,
        Option<u32>,
        Option<u32>,
        Option<u32>,
        Option<u32>,
        Option<u32>,
    );

    fn opened(
        plan: &pm::OpenPlan,
        relief: bool,
        slow_start: bool,
        ratectl: crate::encode::RatectlMode,
    ) -> (Opened, RateGovernor) {
        use crate::encode::governor::GovernorFlags;
        let now = Instant::now();
        let flags = GovernorFlags {
            floor_relief: relief,
            slow_start,
            ratectl,
            viewer_rate_clamp: true,
            seed_contradiction: true,
            measured_evidence: true,
            measured_ceiling: true,
            ..GovernorFlags::default()
        };
        let mut g = RateGovernor::new_with(15, 12, flags, 8_000_000, plan, now);
        let built = (
            g.open_seed_bps(),
            g.learned_ceiling_bps(),
            g.ceiling_for(2_550_000, true),
            g.prior_bps(),
            g.pipe_bps(now, true),
        );
        let tick = g
            .pre_encode_tick(2_550_000, crate::encode::MIN_BITRATE_BPS, true, 12, now)
            .map(|a| a.bps);
        let o = (
            built.0,
            built.1,
            built.2,
            built.3,
            built.4,
            tick,
            g.relieved_floor_bps(),
            g.shadow_target_bps(),
        );
        (o, g)
    }

    /// The pump's open, with a governor built as the pump builds it.
    fn open_line(p: &mut PumpMemory, now: Instant) -> String {
        let mut governor = RateGovernor::new_with(
            30,
            12,
            crate::encode::governor::GovernorFlags::from_env(),
            p.hi_bps,
            &p.plan(),
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

    const OPEN_FIELDS: [&str; 24] = [
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
        "taken_why",
    ];

    const CLOSE_FIELDS: [&str; 25] = [
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
        "samples_counted_overproduced",
        "samples_counted_clear",
        "samples_not_counted",
        "samples_unread",
        "samples_viewer_late",
        "samples_under_plan",
        "record_ended",
        "frozen",
        "samples_counted_p1",
        "samples_counted_p3",
    ];

    /// Both lines name their fields in this order, carry no address and no
    /// user, and read as below on the three sessions a field read meets first
    /// (printed with `--nocapture`): a pair's first session, a session that
    /// measured nothing, and a session that counted three push-backs, with the
    /// next session's open.
    #[test]
    fn the_two_lines_name_their_fields() {
        use crate::encode::pipe::{Basis, PushedBy};
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
        let (back, over, clear) = (Basis::Backlogged, pm::Read::Overproduced, pm::Read::Clear);
        mem.pushed_back(620_000, back, over, at(10));
        mem.pushed_back(600_000, back, clear, at(16));
        mem.pushed_back(640_000, back, clear, at(23));
        // A late viewer's push-back beside them: counted apart, not taken.
        mem.pushed_back_in(500_000, back, pm::Take::ViewerLate, at(26));
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
        assert!(
            close.contains(
                " samples_counted=3 samples_counted_overproduced=1 samples_counted_clear=2 \
                 samples_not_counted=0 samples_unread=0 samples_viewer_late=1 \
                 samples_under_plan=0 "
            ),
            "{close}"
        );
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

        // 4. FR-148 D46 B2 — the delivery samples' push-backs under a 2 M
        // delivery: two queued (P1), one from an `Overproduced` window, and
        // one backlogged under the in-flight budget (P3), all counted and in
        // the split; a window's push-back beside them is today's.
        let mut p = pump(guard(EntryState::None, None), None);
        let open = open_line(&mut p, t0);
        let mut mem = pm::SessionMemory::default();
        mem.open(None, now_unix, t0);
        mem.delivered(2_000_000, Basis::AppLimited, at(4));
        let (over, clear) = (
            pm::Take::Sample(pm::Read::Overproduced),
            pm::Take::Sample(pm::Read::Clear),
        );
        mem.sample_pushed_back_in(1_800_000, PushedBy::P1, over, at(8));
        mem.sample_pushed_back_in(1_700_000, PushedBy::P1, clear, at(12));
        mem.sample_pushed_back_in(1_750_000, PushedBy::P3, clear, at(16));
        mem.pushed_back(1_600_000, Basis::Backlogged, pm::Read::Clear, at(20));
        let c = mem.closing(at(24), now_unix);
        assert_eq!((c.counted_p1, c.counted_p3), ((2, 1), (1, 1)));
        p.model.as_mut().unwrap().take(c, true);
        let close = logged(|| drop(p));
        assert!(
            close.contains(
                " samples_counted=4 samples_counted_overproduced=1 samples_counted_clear=3 "
            ),
            "{close}"
        );
        assert!(
            close.contains(" frozen=false samples_counted_p1=2 samples_counted_p3=1"),
            "{close}"
        );
        lines.push(("the delivery samples' push-backs", open, close));

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
