//! P5 (multi-org program) — shared-floor encoder for the DC video transports.
//!
//! With `rc_max_sessions` ≥ 2 (rc.302), every concurrent viewer used to run
//! its OWN capture + encoder: two viewers of one host = two DXGI
//! duplications + two HW encode streams — double the GPU cost for identical
//! pixels, and the DDA seat limit (~4) burns twice as fast. This module
//! makes DC-transport sessions with an IDENTICAL hard profile (same
//! transport + codec + chroma) share ONE capture + ONE encoder:
//!
//!   * The FIRST session's pump runs unchanged ("owner": capture, encode,
//!     AIMD, flip/settle/backstop machinery all stay per-pump).
//!   * Later same-profile sessions register as FOLLOWERS: they get every
//!     encoded packet the owner emits, via their own DC chunker task, and
//!     never open a capturer or encoder.
//!   * Per-viewer inputs merge into the owner's loop as a FLOOR: any
//!     viewer's keyframe request forces an IDR for the shared stream; the
//!     frame-skip divisor is the MAX across viewers (slowest decoder wins);
//!     the resolution/priority dials merge to the most conservative; a
//!     congested follower gates frame PRODUCTION exactly like the owner's
//!     own full send-queue (rc.111) — the AIMD then lowers the shared
//!     bitrate. Frames are never dropped per-viewer post-encode: skipping
//!     one viewer's deltas would break ITS reference chain (the 13-byte DC
//!     header has no sequence/recovery), so the floor paces the SOURCE.
//!   * A JOINER decodes nothing until a keyframe: joining sets a
//!     pipeline-wide IDR request and the follower's fan-out stays gated
//!     until the first key-flagged packet (the existing per-pump min-gap
//!     clamps still bound IDR churn).
//!   * SPILL: when one viewer's sustained rate sits far from the others
//!     (it would drag the floor down — or be dragged — indefinitely), the
//!     most deviant FOLLOWER detaches and re-dispatches into its own pump,
//!     bounded so an agent never runs more than [`MAX_PIPELINES`] encoder
//!     pipelines because of spilling.
//!   * The owner leaving closes the pipeline: followers detach and
//!     re-dispatch (first one becomes the new owner, the rest re-join) —
//!     a one-time IDR hiccup instead of fragile in-place promotion.
//!
//! WebRTC-track sessions are excluded by design (mediasoup-style per-track
//! REMB rate control stays per-session). Escape hatch:
//! `ROOMLER_AGENT_SHARED_ENCODER=0` (config key `shared_encoder`) restores
//! the rc.302 pump-per-session behaviour without a rebuild.
//!
//! Everything stateful here is abort-safe: both the owner's [`Pipeline`]
//! and a follower's [`FollowerGuard`] detach in `Drop`, and session
//! teardown aborts the media task — the guards run at the await point.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use webrtc::data_channel::RTCDataChannel;

use crate::encode::viewer_rate::ViewerRateController;
use crate::peer::TargetResolution;

/// Ceiling on encoder pipelines created BY SPILLING. Organic distinct
/// profiles (different codecs negotiated) still each get a pump — the
/// session cap bounds those; this bound only stops spill from re-deriving
/// one-encoder-per-viewer.
pub const MAX_PIPELINES: usize = 2;

/// Divisor ratio (max/min across the pipeline's viewers) that counts as a
/// spill-worthy deviation…
pub const SPILL_RATIO: u32 = 4;
/// …once it has held for this many consecutive viewer-rate windows (~1 s
/// each). Resets as soon as the ratio drops below [`SPILL_RESET_RATIO`].
pub const SPILL_AFTER_WINDOWS: u32 = 10;
/// Below this ratio the deviation counter resets (hysteresis: a viewer
/// hovering around the threshold never ping-pongs between shared and own).
pub const SPILL_RESET_RATIO: u32 = 2;

/// Follower DC send-queue depth. Matches the owner pump's direct-path depth;
/// a slower follower link surfaces as a full queue → the production gate +
/// AIMD floor react, so the depth is a burst buffer, not a latency budget.
const FOLLOWER_SEND_DEPTH: usize = 12;

/// Hard profile key — the part of a session's negotiated video profile that
/// CANNOT merge (the browser built its decoder for it).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PipelineKey {
    /// FFmpeg DC pump — keyed by the codec label ("HEVC"/"AV1"/"VP9"/"H264").
    FfmpegDc(&'static str),
    /// libvpx VP9 DC pump — keyed by chroma (444 vs 420 are different VP9
    /// profiles; the viewer's decoder config differs).
    Vp9Dc { chroma_444: bool },
}

/// Whether shared pipelines are enabled (default ON;
/// `ROOMLER_AGENT_SHARED_ENCODER=0` / `false` reverts to pump-per-session).
pub fn sharing_enabled() -> bool {
    !matches!(
        tunnel_core::env::node_env("SHARED_ENCODER").as_deref(),
        Some("0") | Some("false")
    )
}

/// Why a follower's wait ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DetachReason {
    /// The owner pump exited (session closed / encoder fatal) — re-dispatch;
    /// re-joining (or becoming the new owner) is correct.
    PipelineClosed,
    /// The spill gate evicted this viewer — spawn an OWN pump; do not
    /// immediately re-join the pipeline it was just evicted from.
    Spilled,
}

/// The per-session handles a follower contributes to the shared pipeline —
/// clones of exactly the Arcs its own pump would have consumed.
pub struct FollowerSink {
    pub session_id: bson::oid::ObjectId,
    pub video_bytes_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    pub control_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    pub keyframe_requested: Arc<AtomicBool>,
    pub target_resolution: Arc<Mutex<TargetResolution>>,
    pub quality_state: Arc<AtomicU8>,
    pub viewer_report: Arc<AtomicU32>,
    pub priority: Arc<AtomicU8>,
    pub capture_native_dims: Arc<AtomicU64>,
    pub encoded_dims: Arc<AtomicU64>,
}

struct DetachState {
    notify: tokio::sync::Notify,
    reason: Mutex<Option<DetachReason>>,
}

struct Follower {
    sink: FollowerSink,
    send_tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
    /// False until the first key-flagged packet is forwarded — the joiner
    /// gate (a delta before the first IDR is undecodable for this viewer).
    synced: bool,
    viewer_rate: ViewerRateController,
    divisor: u32,
    /// Consecutive spill-qualifying windows (see [`SPILL_AFTER_WINDOWS`]).
    spill_strikes: u32,
    detach: Arc<DetachState>,
}

impl Follower {
    fn detach(&self, reason: DetachReason) {
        let mut r = self.detach.reason.lock().unwrap();
        if r.is_none() {
            *r = Some(reason);
        }
        drop(r);
        self.detach.notify.notify_waiters();
    }
}

#[derive(Default)]
struct PipelineInner {
    followers: Vec<Follower>,
    /// Set on follower join (and on a synced follower's forced desync) —
    /// consumed by the owner's keyframe-merge point.
    kf_needed: bool,
    /// Last `rc:video-info` payload the owner published — replayed to
    /// joiners so their stats badge is honest without waiting for the next
    /// owner-side refresh.
    video_info: Option<String>,
    /// Owner marks this when its pump exits; blocks late joins racing the
    /// registry removal.
    closed: bool,
}

type Registry = Mutex<HashMap<PipelineKey, Arc<Mutex<PipelineInner>>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How many shared pipelines are currently registered (spill budget input).
fn pipeline_count() -> usize {
    registry().lock().unwrap().len()
}

// ─── Owner side ─────────────────────────────────────────────────────────

/// Owner-held handle for one shared pipeline. Register at pump start, call
/// the merge helpers from the loop, `fan_out` at the send point. Drop (incl.
/// task abort) detaches every follower with [`DetachReason::PipelineClosed`]
/// and removes the registry entry.
pub struct Pipeline {
    key: PipelineKey,
    inner: Arc<Mutex<PipelineInner>>,
    /// False when another pump already owned the key (duplicate profile
    /// race) — this pipeline then runs standalone and never gets followers.
    registered: bool,
    owner_session: bson::oid::ObjectId,
}

impl Pipeline {
    /// Register the calling pump as the owner for `key`. If the key is
    /// already owned (two same-profile pumps raced), the returned pipeline
    /// is standalone — valid, merge helpers are no-ops, no followers.
    pub fn register(key: PipelineKey, owner_session: bson::oid::ObjectId) -> Self {
        let inner = Arc::new(Mutex::new(PipelineInner::default()));
        let mut reg = registry().lock().unwrap();
        let registered = match reg.entry(key) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(inner.clone());
                true
            }
        };
        drop(reg);
        if !registered {
            tracing::debug!(?key, %owner_session, "media_share: key already owned — running standalone pump");
        }
        Self {
            key,
            inner,
            registered,
            owner_session,
        }
    }

    /// Viewers on this pipeline beyond the owner.
    pub fn follower_count(&self) -> usize {
        self.inner.lock().unwrap().followers.len()
    }

    /// Any viewer (follower atomics + the join flag) wants an IDR. Consumes
    /// the requests, mirroring `keyframe_requested.swap(false)`.
    pub fn take_keyframe_requested(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let mut wanted = std::mem::take(&mut inner.kf_needed);
        for f in &inner.followers {
            if f.sink
                .keyframe_requested
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                wanted = true;
            }
        }
        wanted
    }

    /// Floor-merge of the resolution dial: the smallest requested area wins
    /// (`Native` requests no constraint). Composes BEFORE the relay/auto
    /// caps exactly like the owner's own dial.
    pub fn merged_target(&self, own: TargetResolution) -> TargetResolution {
        let inner = self.inner.lock().unwrap();
        let mut best = own;
        for f in &inner.followers {
            let t = *f.sink.target_resolution.lock().unwrap();
            best = smaller_target(best, t);
        }
        best
    }

    /// Floor-merge of the Priority dial's relay resolution cap: the smallest
    /// cap across viewers (None = uncapped for that viewer).
    pub fn merged_priority_cap(&self, own_cap: Option<u32>, constrained: bool) -> Option<u32> {
        let inner = self.inner.lock().unwrap();
        let mut cap = own_cap;
        for f in &inner.followers {
            let p = f.sink.priority.load(Relaxed);
            let fc = crate::encode::priority_relay_cap(p, constrained);
            cap = match (cap, fc) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (None, Some(b)) => Some(b),
                (a, None) => a,
            };
        }
        cap
    }

    /// Floor-merge of the quality dial (lowest value = most conservative;
    /// the VP9 pump's semantics). The FFmpeg pump ignores quality.
    pub fn min_quality(&self, own: u8) -> u8 {
        let inner = self.inner.lock().unwrap();
        inner
            .followers
            .iter()
            .map(|f| f.sink.quality_state.load(Relaxed))
            .fold(own, u8::min)
    }

    /// True when any follower's send queue is full — folded into the
    /// owner's rc.111 production gate so the whole pipeline paces to the
    /// slowest link (the pre-encode floor; never per-viewer delta drops).
    pub fn followers_congested(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.followers.iter().any(|f| f.send_tx.capacity() == 0)
    }

    /// Step every follower's viewer-rate controller for this ~1 s window and
    /// return the MAX divisor across followers (the owner takes the max with
    /// its own). Also runs the spill gate: a sustained large deviation
    /// between the pipeline's fastest and slowest viewer detaches the most
    /// deviant FOLLOWER (bounded by the pipeline budget).
    pub fn step_viewer_windows(&self, own_divisor: u32, capture_fps: u32) -> u32 {
        let mut inner = self.inner.lock().unwrap();
        let mut max_div = 1u32;
        for f in inner.followers.iter_mut() {
            let raw = f.sink.viewer_report.swap(0, Relaxed);
            let (fps, struggling) = crate::encode::viewer_rate::unpack_report(raw);
            f.divisor = f.viewer_rate.observe(fps, struggling, capture_fps);
            max_div = max_div.max(f.divisor);
        }
        // Spill gate — only meaningful with ≥2 viewers total and only when
        // spawning another pipeline stays inside the budget.
        if !inner.followers.is_empty() && pipeline_count() < MAX_PIPELINES {
            let divs: Vec<u32> = std::iter::once(own_divisor)
                .chain(inner.followers.iter().map(|f| f.divisor))
                .collect();
            let lo = *divs.iter().min().unwrap_or(&1);
            let hi = *divs.iter().max().unwrap_or(&1);
            let deviating = ratio_qualifies(lo, hi, SPILL_RATIO);
            let resets = !ratio_qualifies(lo, hi, SPILL_RESET_RATIO);
            let mut spill_idx: Option<usize> = None;
            for (i, f) in inner.followers.iter_mut().enumerate() {
                if resets {
                    f.spill_strikes = 0;
                    continue;
                }
                // Only the most deviant follower accumulates strikes: the
                // one whose divisor sits farthest from the OWNER's (the
                // owner cannot spill — it holds the capture/encoder).
                let is_most_deviant = deviating
                    && f.divisor.abs_diff(own_divisor)
                        >= divs
                            .iter()
                            .map(|d| d.abs_diff(own_divisor))
                            .max()
                            .unwrap_or(0)
                    && f.divisor != own_divisor;
                if is_most_deviant {
                    f.spill_strikes += 1;
                    if f.spill_strikes >= SPILL_AFTER_WINDOWS {
                        spill_idx = Some(i);
                    }
                } else {
                    f.spill_strikes = 0;
                }
            }
            if let Some(i) = spill_idx {
                let f = inner.followers.remove(i);
                tracing::info!(
                    session = %f.sink.session_id,
                    owner = %self.owner_session,
                    divisor = f.divisor,
                    own_divisor,
                    "media_share: sustained rate deviation — spilling viewer to its own encoder"
                );
                f.detach(DetachReason::Spilled);
            }
        }
        max_div
    }

    /// Forward one framed wire packet to every follower and mirror the dim
    /// atomics (the cursor pumps of follower sessions scale off them).
    /// Called on the owner's hot path — sync + try_send only.
    ///
    /// Sync gating: an unsynced follower skips deltas until the first
    /// keyframe. A synced follower whose queue is full at a delta is
    /// DESYNCED (its reference chain just broke) and a pipeline IDR is
    /// requested — the production-gate floor makes this rare (races only).
    pub fn fan_out(
        &self,
        wire: &bytes::Bytes,
        is_keyframe: bool,
        native_dims: u64,
        encoded_dims: u64,
    ) {
        let mut inner = self.inner.lock().unwrap();
        if inner.followers.is_empty() {
            return;
        }
        let mut need_kf = false;
        for f in inner.followers.iter_mut() {
            f.sink.capture_native_dims.store(native_dims, Relaxed);
            f.sink.encoded_dims.store(encoded_dims, Relaxed);
            if !f.synced {
                if !is_keyframe {
                    continue;
                }
                f.synced = true;
            }
            match f.send_tx.try_send(wire.clone()) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // Whatever was dropped (delta OR the IDR later deltas
                    // will reference), this viewer's chain is broken —
                    // desync and resync at the next keyframe. The
                    // production-gate floor makes this a race-only path.
                    f.synced = false;
                    need_kf = true;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    // Chunker died (guard drop races) — the follower's guard
                    // removal will prune it; skip meanwhile.
                }
            }
        }
        if need_kf {
            inner.kf_needed = true;
        }
    }

    /// Publish the owner's `rc:video-info` payload: stored for joiner replay
    /// and pushed to every follower's control DC (spawned — control DCs are
    /// async).
    pub fn publish_video_info(&self, payload: String) {
        let mut inner = self.inner.lock().unwrap();
        inner.video_info = Some(payload.clone());
        let targets: Vec<_> = inner
            .followers
            .iter()
            .map(|f| f.sink.control_dc.clone())
            .collect();
        drop(inner);
        for cdc in targets {
            let payload = payload.clone();
            tokio::spawn(async move {
                if let Some(dc) = cdc.lock().await.clone() {
                    let _ = dc.send_text(payload).await;
                }
            });
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if self.registered {
            registry().lock().unwrap().remove(&self.key);
        }
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        for f in inner.followers.drain(..) {
            f.detach(DetachReason::PipelineClosed);
        }
    }
}

// ─── Follower side ──────────────────────────────────────────────────────

/// Follower-held handle. Await [`FollowerGuard::detached`]; Drop (session
/// teardown aborts the media task) removes the follower from the pipeline.
pub struct FollowerGuard {
    inner: Arc<Mutex<PipelineInner>>,
    session_id: bson::oid::ObjectId,
    detach: Arc<DetachState>,
}

impl FollowerGuard {
    /// Resolves when the pipeline closes or the spill gate evicts this
    /// viewer.
    pub async fn detached(&self) -> DetachReason {
        loop {
            if let Some(r) = *self.detach.reason.lock().unwrap() {
                return r;
            }
            self.detach.notify.notified().await;
        }
    }
}

impl Drop for FollowerGuard {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .followers
            .retain(|f| f.sink.session_id != self.session_id);
    }
}

/// Join `key`'s pipeline as a follower. `None` when sharing is off, no
/// pipeline owns the key, or the pipeline is closing. On success the
/// follower's DC chunker task is running, a pipeline IDR is requested, and
/// the last `rc:video-info` (if any) is replayed to this viewer.
pub fn try_join(
    key: PipelineKey,
    sink: FollowerSink,
    capture_fps_hint: u32,
) -> Option<FollowerGuard> {
    if !sharing_enabled() {
        return None;
    }
    let inner = registry().lock().unwrap().get(&key)?.clone();
    let mut guard = inner.lock().unwrap();
    if guard.closed {
        return None;
    }
    let session_id = sink.session_id;
    let (send_tx, send_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(FOLLOWER_SEND_DEPTH);
    spawn_follower_chunker(session_id, sink.video_bytes_dc.clone(), send_rx);
    let detach = Arc::new(DetachState {
        notify: tokio::sync::Notify::new(),
        reason: Mutex::new(None),
    });
    // Replay the badge so the joiner's stats chip is honest immediately.
    if let Some(info) = guard.video_info.clone() {
        let cdc = sink.control_dc.clone();
        tokio::spawn(async move {
            if let Some(dc) = cdc.lock().await.clone() {
                let _ = dc.send_text(info).await;
            }
        });
    }
    guard.followers.push(Follower {
        sink,
        send_tx,
        synced: false,
        viewer_rate: ViewerRateController::new(capture_fps_hint.max(1)),
        divisor: 1,
        spill_strikes: 0,
        detach: detach.clone(),
    });
    // The joiner needs an IDR before anything it receives decodes.
    guard.kf_needed = true;
    drop(guard);
    tracing::info!(%session_id, ?key, "media_share: joined shared encoder pipeline as follower");
    Some(FollowerGuard {
        inner,
        session_id,
        detach,
    })
}

/// The follower's DC send task — the same 16 KiB chunk discipline as the
/// owner pumps' send tasks (single consumer per DC keeps chunk order for
/// the browser reassembler).
fn spawn_follower_chunker(
    session_id: bson::oid::ObjectId,
    video_bytes_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    mut rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
) {
    tokio::spawn(async move {
        const SCTP_CHUNK_SIZE: usize = 16 * 1024;
        while let Some(wire) = rx.recv().await {
            let Some(dc) = video_bytes_dc.lock().await.clone() else {
                continue;
            };
            if dc.ready_state()
                != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
            {
                continue;
            }
            let total = wire.len();
            let mut off = 0usize;
            while off < total {
                let end = (off + SCTP_CHUNK_SIZE).min(total);
                if let Err(e) = dc.send(&wire.slice(off..end)).await {
                    tracing::debug!(session = %session_id, %e, "media_share follower: DC send failed");
                    break;
                }
                off = end;
            }
        }
        tracing::debug!(session = %session_id, "media_share follower chunker exiting");
    });
}

// ─── Pure helpers ───────────────────────────────────────────────────────

/// The smaller (more conservative) of two resolution dials by target area;
/// `Native` imposes no constraint.
fn smaller_target(a: TargetResolution, b: TargetResolution) -> TargetResolution {
    match (a, b) {
        (TargetResolution::Native, other) => other,
        (me, TargetResolution::Native) => me,
        (
            TargetResolution::Fixed {
                width: aw,
                height: ah,
            },
            TargetResolution::Fixed {
                width: bw,
                height: bh,
            },
        ) => {
            if (bw as u64 * bh as u64) < (aw as u64 * ah as u64) {
                TargetResolution::Fixed {
                    width: bw,
                    height: bh,
                }
            } else {
                TargetResolution::Fixed {
                    width: aw,
                    height: ah,
                }
            }
        }
    }
}

/// Whether `hi` deviates from `lo` by at least `ratio`× (divisor space:
/// divisor 1 vs 4 = a 4× fps gap).
fn ratio_qualifies(lo: u32, hi: u32, ratio: u32) -> bool {
    hi >= lo.max(1).saturating_mul(ratio)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::oid::ObjectId;

    /// The registry (and its `pipeline_count()` spill-budget input) is
    /// process-global, and cargo runs tests concurrently — a sibling test's
    /// live pipeline would flip the budget gate mid-test. Serialize every
    /// registry-touching test. `unwrap_or_else(into_inner)` un-poisons after
    /// a failed test so the rest still report their own results.
    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn sink(session: ObjectId) -> FollowerSink {
        FollowerSink {
            session_id: session,
            video_bytes_dc: Arc::new(tokio::sync::Mutex::new(None)),
            control_dc: Arc::new(tokio::sync::Mutex::new(None)),
            keyframe_requested: Arc::new(AtomicBool::new(false)),
            target_resolution: Arc::new(Mutex::new(TargetResolution::Native)),
            quality_state: Arc::new(AtomicU8::new(2)),
            viewer_report: Arc::new(AtomicU32::new(0)),
            priority: Arc::new(AtomicU8::new(1)),
            capture_native_dims: Arc::new(AtomicU64::new(0)),
            encoded_dims: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Unique keys per test — the registry is process-global and cargo runs
    /// tests concurrently, so tests share it. Vp9Dc's two chroma variants +
    /// distinct FfmpegDc labels give each test an isolated slot.
    #[tokio::test]
    async fn join_requires_a_registered_owner_and_owner_drop_detaches() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-A");
        assert!(
            try_join(key, sink(ObjectId::new()), 60).is_none(),
            "no owner yet"
        );

        let owner = Pipeline::register(key, ObjectId::new());
        let guard = try_join(key, sink(ObjectId::new()), 60).expect("join");
        assert_eq!(owner.follower_count(), 1);
        // Joining requested a pipeline IDR.
        assert!(owner.take_keyframe_requested());
        assert!(!owner.take_keyframe_requested(), "consumed");

        drop(owner);
        assert_eq!(guard.detached().await, DetachReason::PipelineClosed);
        // Registry slot is free again.
        assert!(registry().lock().unwrap().get(&key).is_none());
    }

    #[tokio::test]
    async fn duplicate_owner_registration_runs_standalone() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-B");
        let first = Pipeline::register(key, ObjectId::new());
        let second = Pipeline::register(key, ObjectId::new());
        assert!(first.registered);
        assert!(!second.registered);
        // Standalone pipeline never sees followers; dropping it must NOT
        // free the first owner's registry slot.
        drop(second);
        assert!(registry().lock().unwrap().get(&key).is_some());
        drop(first);
        assert!(registry().lock().unwrap().get(&key).is_none());
    }

    #[tokio::test]
    async fn follower_guard_drop_leaves_the_pipeline() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-C");
        let owner = Pipeline::register(key, ObjectId::new());
        let guard = try_join(key, sink(ObjectId::new()), 60).expect("join");
        assert_eq!(owner.follower_count(), 1);
        drop(guard); // session teardown aborts the media task → guard drops
        assert_eq!(owner.follower_count(), 0);
    }

    #[tokio::test]
    async fn fan_out_gates_joiner_on_keyframe_then_streams() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-D");
        let owner = Pipeline::register(key, ObjectId::new());
        let _guard = try_join(key, sink(ObjectId::new()), 60).expect("join");

        let delta = bytes::Bytes::from_static(b"delta");
        let kf = bytes::Bytes::from_static(b"key");
        // Deltas before the first keyframe are skipped for the joiner.
        owner.fan_out(&delta, false, 0, 0);
        {
            let inner = owner.inner.lock().unwrap();
            assert!(!inner.followers[0].synced);
            assert_eq!(
                inner.followers[0].send_tx.capacity(),
                FOLLOWER_SEND_DEPTH,
                "no delta queued pre-sync"
            );
        }
        // The keyframe syncs and is forwarded; deltas flow after.
        owner.fan_out(&kf, true, 0, 0);
        owner.fan_out(&delta, false, 0, 0);
        let inner = owner.inner.lock().unwrap();
        assert!(inner.followers[0].synced);
        assert_eq!(
            inner.followers[0].send_tx.capacity(),
            FOLLOWER_SEND_DEPTH - 2,
            "kf + delta queued"
        );
    }

    #[tokio::test]
    async fn fan_out_full_queue_desyncs_and_requests_idr() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-E");
        let owner = Pipeline::register(key, ObjectId::new());
        let _guard = try_join(key, sink(ObjectId::new()), 60).expect("join");

        let kf = bytes::Bytes::from_static(b"key");
        let delta = bytes::Bytes::from_static(b"delta");
        owner.fan_out(&kf, true, 0, 0);
        // Fill the queue (chunker can't drain: DC handle is None → it parks
        // on recv, then discards; but we saturate faster than it drains by
        // never yielding to it inside this sync loop).
        for _ in 0..(FOLLOWER_SEND_DEPTH + 4) {
            owner.fan_out(&delta, false, 0, 0);
        }
        let inner = owner.inner.lock().unwrap();
        assert!(!inner.followers[0].synced, "overflow desyncs the follower");
        assert!(inner.kf_needed, "resync IDR requested");
    }

    #[tokio::test]
    async fn merges_take_the_floor() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-F");
        let owner = Pipeline::register(key, ObjectId::new());
        let s = sink(ObjectId::new());
        *s.target_resolution.lock().unwrap() = TargetResolution::Fixed {
            width: 1280,
            height: 720,
        };
        s.quality_state.store(0, Relaxed);
        let _g = try_join(key, s, 60).expect("join");

        // Resolution: the smaller area wins.
        let merged = owner.merged_target(TargetResolution::Fixed {
            width: 2560,
            height: 1440,
        });
        assert_eq!(
            merged,
            TargetResolution::Fixed {
                width: 1280,
                height: 720
            }
        );
        // Native owner + Fixed follower → the Fixed constraint.
        let merged = owner.merged_target(TargetResolution::Native);
        assert_eq!(
            merged,
            TargetResolution::Fixed {
                width: 1280,
                height: 720
            }
        );
        // Quality: min.
        assert_eq!(owner.min_quality(2), 0);
    }

    #[tokio::test]
    async fn step_viewer_windows_returns_max_follower_divisor() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-G");
        let owner = Pipeline::register(key, ObjectId::new());
        let s = sink(ObjectId::new());
        let report = s.viewer_report.clone();
        let _g = try_join(key, s, 60).expect("join");

        // The follower reports a struggling 20 fps decode → its controller
        // caps below 20 → divisor > 1. Owner's divisor 1 → max wins.
        report.store(crate::encode::viewer_rate::pack_report(20, true), Relaxed);
        let div = owner.step_viewer_windows(1, 60);
        assert!(
            div > 1,
            "struggling follower must raise the shared divisor, got {div}"
        );
    }

    #[tokio::test]
    async fn spill_evicts_the_deviant_follower_after_sustained_windows() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-H");
        let owner = Pipeline::register(key, ObjectId::new());
        let s = sink(ObjectId::new());
        let report = s.viewer_report.clone();
        let guard = try_join(key, s, 60).expect("join");

        // Drive the follower to the floor divisor (cap 12 → divisor 5 at 60)
        // while the owner stays at divisor 1 → ratio 5 ≥ SPILL_RATIO.
        for _ in 0..SPILL_AFTER_WINDOWS + 6 {
            report.store(crate::encode::viewer_rate::pack_report(5, true), Relaxed);
            owner.step_viewer_windows(1, 60);
        }
        assert_eq!(owner.follower_count(), 0, "deviant follower spilled");
        assert_eq!(guard.detached().await, DetachReason::Spilled);
    }

    #[tokio::test]
    async fn spill_respects_the_pipeline_budget() {
        let _s = serial();
        // Two pipelines registered = budget exhausted → no spilling even
        // under sustained deviation.
        let key = PipelineKey::FfmpegDc("TEST-I");
        let other = Pipeline::register(PipelineKey::FfmpegDc("TEST-I2"), ObjectId::new());
        let owner = Pipeline::register(key, ObjectId::new());
        let s = sink(ObjectId::new());
        let report = s.viewer_report.clone();
        let _g = try_join(key, s, 60).expect("join");

        for _ in 0..SPILL_AFTER_WINDOWS * 2 {
            report.store(crate::encode::viewer_rate::pack_report(5, true), Relaxed);
            owner.step_viewer_windows(1, 60);
        }
        assert_eq!(
            owner.follower_count(),
            1,
            "budget-capped pipeline keeps the slow viewer (floor holds)"
        );
        drop(other);
    }

    #[test]
    fn ratio_and_target_helpers() {
        assert!(ratio_qualifies(1, 4, 4));
        assert!(!ratio_qualifies(1, 3, 4));
        assert!(ratio_qualifies(2, 8, 4));
        assert!(!ratio_qualifies(0, 0, 2));

        let a = TargetResolution::Fixed {
            width: 1920,
            height: 1080,
        };
        let b = TargetResolution::Fixed {
            width: 1280,
            height: 720,
        };
        assert_eq!(smaller_target(a, b), b);
        assert_eq!(smaller_target(b, a), b);
        assert_eq!(smaller_target(TargetResolution::Native, a), a);
        assert_eq!(
            smaller_target(TargetResolution::Native, TargetResolution::Native),
            TargetResolution::Native
        );
    }
}
