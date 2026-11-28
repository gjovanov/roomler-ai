// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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
//!   * An owner whose capture cannot produce pixels (FR-80) tells every
//!     follower why, and every later joiner as it joins (#3260). A follower
//!     opens no capture of its own, so this is the only way its viewer
//!     learns that there is no picture, and why.
//!
//! WebRTC-track sessions are excluded by design (mediasoup-style per-track
//! REMB rate control stays per-session). Escape hatch:
//! `ROOMLERD_SHARED_ENCODER=0` (config key `shared_encoder`) restores
//! the rc.302 pump-per-session behaviour without a rebuild.
//!
//! Everything stateful here is abort-safe: both the owner's [`Pipeline`]
//! and a follower's [`FollowerGuard`] detach in `Drop`, and session
//! teardown aborts the media task — the guards run at the await point.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

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
/// `ROOMLERD_SHARED_ENCODER=0` / `false` reverts to pump-per-session).
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
    pub viewer_report: Arc<crate::encode::viewer_rate::ViewerFeedback>,
    pub priority: Arc<AtomicU8>,
    pub capture_native_dims: Arc<AtomicU64>,
    pub encoded_dims: Arc<AtomicU64>,
    /// FR-17 — whether THIS follower's controller negotiated per-chunk
    /// framing (`chunk-framing` in `rc:session.request`). It is a per-SESSION
    /// property, so a follower can disagree with the owner; the follower
    /// chunker MUST frame exactly as this follower's browser expects, or the
    /// receiver parses a chunk header out of raw payload and decodes nothing
    /// (field 2026-08-30: every chunk-framing follower was a black screen —
    /// the owner framed, the follower chunker did not).
    pub chunk_framing: bool,
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

/// #3260, #3423 — what the owner told its own controller about a capture with
/// no picture, kept so every follower is told too, and every viewer that joins
/// later.
#[derive(Clone)]
enum OwnerNotice {
    /// FR-80 — the capture cannot produce pixels: `rc:media-unavailable`.
    Unavailable(crate::capture::CaptureUnavailable),
    /// FR-93 — the capture waits on its host (`rc:media-waiting`), at the
    /// latest until `until`. A viewer that joins later is told the time left,
    /// not the owner's whole wait.
    Waiting {
        why: crate::capture::WaitFor,
        until: std::time::Instant,
    },
}

/// #3423 — what a viewer joining the pipeline is told, from the kept
/// [`OwnerNotice`].
enum JoinNotice {
    Unavailable(crate::capture::CaptureUnavailable),
    Waiting(crate::capture::OpenWait),
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
    /// #3260 — why the owner's capture produces no pixels (FR-80), once the
    /// owner knows. Such an owner never captures a frame, so it never
    /// publishes `video_info`: without this a follower's viewer had a black
    /// canvas and a stall, and blamed the network. #3423 — or what it waits
    /// for at the host (FR-93), for as long as it waits.
    notice: Option<OwnerNotice>,
    /// Owner marks this when its pump exits; blocks late joins racing the
    /// registry removal. #3671 — `retire_all` marks it too, for a pipeline
    /// whose capture was opened on an X server that is gone: its owner and
    /// followers go on, and nobody joins it any more. #3918 — and `try_join`,
    /// for a beating owner with no picture of the virtual desktop whose
    /// capture may read the desktop's X server.
    closed: bool,
    /// #3918 — the session whose pump owns the pipeline ([`try_join`]).
    owner: Option<bson::oid::ObjectId>,
    /// #3918 — the owner's capture is known not to read an X server. One
    /// setter: [`Pipeline::publish_video_info`], from the arm of the capturer
    /// that payload was made for. Two clearers, back to unknown (`false`):
    /// [`Pipeline::capture_opens_again`], because a reopen's new capturer can
    /// be of another arm (an explicit arm that falls through to the X server)
    /// and publishes its arm only after its first frame went out, or, after
    /// the FFmpeg pump's error reopen, only with the next `rc:video-info` sent
    /// for another reason (a viewer leaves, a transport flip, a change of
    /// dims or rate); and [`Pipeline::tell_unavailable`], because a capture
    /// with no pixels publishes nothing. Unknown at registration too.
    /// [`try_join`] refuses the pipeline of a marked owner that is beating
    /// unless this is known, so a fact that is stale or missing never lets a
    /// joiner follow a capture of an X server that is gone; an owner silent
    /// past [`PIPELINE_STALE`] is evicted first, marked or not, as on master.
    /// What a refusal costs is the joiner's own pump: one more capture and
    /// encoder; on a portal host without a restore token, a consent dialog;
    /// and while the owner's portal request is still pending, the joiner's
    /// request takes it over (one per host) and the owner's open falls
    /// through to the virtual desktop's X server.
    capture_not_x: bool,
    /// Millis (process-epoch) of the owner's last loop iteration —
    /// [`Pipeline::beat`]. Joiners treat a silent owner as DEAD (rc.443):
    /// field 2026-08-21, corplap — an av1_qsv driver hang froze the owner pump
    /// INSIDE `enc.encode()` (inline-blocking, no await to time out), its
    /// session was torn down, but the never-returning task never dropped
    /// its `Pipeline`, so the registry entry survived and every subsequent
    /// same-profile viewer joined a corpse ("no video after 4 attempts")
    /// until a daemon restart.
    last_beat_ms: AtomicU64,
}

/// Owner silence beyond this ⇒ the pipeline is a corpse a joiner may evict.
/// The owner beats every pump-loop iteration (including backpressure skips
/// and empty polls, ≤ ~35 ms apart in a healthy pump), so 10 s of silence
/// only ever means a wedged task.
const PIPELINE_STALE: Duration = Duration::from_secs(10);

/// Millis since a process-stable epoch (first use). Monotonic — safe for
/// staleness arithmetic across tasks. Biased by one minute so values near
/// process start can still be AGED backwards (`force_stale`'s subtraction
/// would otherwise saturate at 0 and read as fresh).
fn now_ms() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    60_000
        + EPOCH
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as u64
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
        let inner = Arc::new(Mutex::new(PipelineInner {
            owner: Some(owner_session),
            ..PipelineInner::default()
        }));
        inner.lock().unwrap().last_beat_ms.store(now_ms(), Relaxed);
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

    /// Owner-liveness beat — call once per pump-loop iteration (any arm:
    /// real frame, empty poll, backpressure skip). A joiner finding this
    /// stale by [`PIPELINE_STALE`] evicts the registration (rc.443 — a
    /// driver-hung owner can never drop its `Pipeline` itself).
    pub fn beat(&self) {
        self.inner
            .lock()
            .unwrap()
            .last_beat_ms
            .store(now_ms(), Relaxed);
    }

    /// Test hook: age the beat past the staleness horizon.
    #[cfg(test)]
    fn force_stale(&self) {
        self.inner.lock().unwrap().last_beat_ms.store(
            now_ms().saturating_sub(PIPELINE_STALE.as_millis() as u64 + 1_000),
            Relaxed,
        );
    }

    /// Viewers on this pipeline beyond the owner.
    pub fn follower_count(&self) -> usize {
        self.inner.lock().unwrap().followers.len()
    }

    /// FR-93 P6c — the sessions following this pipeline. An owner whose
    /// capture was stopped at the screen ends them before itself: had it gone
    /// first, they would detach, re-dispatch, and open the capture the person
    /// had just stopped.
    pub fn follower_sessions(&self) -> Vec<bson::oid::ObjectId> {
        self.inner
            .lock()
            .unwrap()
            .followers
            .iter()
            .map(|f| f.sink.session_id)
            .collect()
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

    /// FR-119 P2b-2 — floor-merge of the frame-rate ask: the smallest across
    /// the viewers, and a viewer that asked nothing asks for today's rate
    /// (`None` wins). A viewer on a 60 Hz screen never receives a heavier
    /// stream than it asked for.
    pub fn merged_fps_ask(&self, own: Option<u32>) -> Option<u32> {
        let inner = self.inner.lock().unwrap();
        inner.followers.iter().fold(own, |merged, f| {
            match (merged, f.sink.viewer_report.fps_ask()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                _ => None,
            }
        })
    }

    /// FR-135 R2 — the slowest screen among the viewers that have said. One
    /// stream serves them all, and a viewer yet to measure has not asked for a
    /// faster one, so it does not lift the floor (unlike [`Self::merged_fps_ask`]).
    pub fn merged_viewer_hz(&self, own: Option<u32>) -> Option<u32> {
        let inner = self.inner.lock().unwrap();
        inner
            .followers
            .iter()
            .filter_map(|f| f.sink.viewer_report.viewer_hz())
            .chain(own)
            .min()
    }

    /// Floor-merge of the Priority dial's relay resolution cap: the smallest
    /// cap across viewers (None = uncapped for that viewer).
    pub fn merged_priority_cap(&self, own_cap: Option<u32>, constrained: bool) -> Option<u32> {
        self.merged_priority_cap_by(own_cap, |p| {
            crate::encode::priority_relay_cap(p, constrained)
        })
    }

    /// [`Self::merged_priority_cap`] with the per-dial cap supplied — the
    /// FR-70 M3a plan producer's form, which resolves the dial caps once per
    /// pump (`encode::priority_relay_cap_for`) instead of reading env for
    /// every follower on every frame. The reader above is this applied.
    pub fn merged_priority_cap_by(
        &self,
        own_cap: Option<u32>,
        cap_of: impl Fn(u8) -> Option<u32>,
    ) -> Option<u32> {
        let inner = self.inner.lock().unwrap();
        let mut cap = own_cap;
        for f in &inner.followers {
            let p = f.sink.priority.load(Relaxed);
            let fc = cap_of(p);
            cap = match (cap, fc) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (None, Some(b)) => Some(b),
                (a, None) => a,
            };
        }
        cap
    }

    /// P7b (merge-aware idle refine) — is the pipeline's merged cap LIFTABLE
    /// at rest? True iff every viewer whose Priority dial contributes a cap
    /// (`priority_relay_cap(..) == Some`) also passes `idle_refine_applies`.
    /// Viewers contributing NO cap (Sharper; Balanced on direct) never block
    /// — they asked for native anyway. Vacuously true when nothing caps (the
    /// caller's merged-cap-clamps check already gates that case). Field
    /// 2026-08-20 (winhost-b): owner=Sharper + follower=Smoother parked the
    /// shared stream at 1024 with refine dead — the floor-merge took the
    /// follower's cap while eligibility read only the owner's dial, giving
    /// the worst combination (capped AND never crisp at rest). With a single
    /// viewer this reduces exactly to `idle_refine_applies(own_prio, _)`.
    pub fn merged_refine_eligible(&self, own_prio: u8, constrained: bool) -> bool {
        let blocks = |p: u8| {
            crate::encode::priority_relay_cap(p, constrained).is_some()
                && !crate::encode::idle_refine_applies(p, constrained)
        };
        if blocks(own_prio) {
            return false;
        }
        let inner = self.inner.lock().unwrap();
        !inner
            .followers
            .iter()
            .any(|f| blocks(f.sink.priority.load(Relaxed)))
    }

    /// P8 Phase 4 (mixed-dial telemetry) — does any follower's dial pair
    /// (Priority, resolution pick) differ from the owner's? This is the
    /// share-shape a per-viewer pipeline (SVC / tiered transcode) would
    /// serve better than the floor-merge; the pumps count it into the
    /// session stats per heartbeat window so the SVC decision is made on
    /// field data. False with zero followers (nothing shared).
    pub fn dials_mixed(&self, own_prio: u8, own_target: TargetResolution) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.followers.iter().any(|f| {
            f.sink.priority.load(Relaxed) != own_prio
                || *f.sink.target_resolution.lock().unwrap() != own_target
        })
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
            let raw = f.sink.viewer_report.take_report();
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
    ///
    /// #3918 — the payload is read before the pipeline's lock is taken, and
    /// the payload and what it says of the capture are stored under one hold
    /// of it: no reader sees one payload with another payload's fact.
    pub fn publish_video_info(&self, payload: String) {
        let capture_not_x = names_a_capture_off_x(&payload);
        let mut inner = self.inner.lock().unwrap();
        inner.video_info = Some(payload.clone());
        inner.capture_not_x = capture_not_x;
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

    /// #3918 — the owner's capture is about to be opened again
    /// (`peer::shared_media_waiting_hook`, built for each open of a
    /// registered pipeline's capture, as a test reads from `peer.rs`): what
    /// it reads is unknown until the owner publishes the new capture's arm,
    /// and until then a marked owner's pipeline takes no joiner
    /// ([`try_join`]).
    pub fn capture_opens_again(&self) {
        self.inner.lock().unwrap().capture_not_x = false;
    }

    /// #3260 — the owner's capture cannot produce pixels, and `reason` says
    /// why (FR-80). The owner tells its own controller; this tells every
    /// follower's, and keeps the reason for the viewers that join later.
    /// Each notice retries until its viewer's control channel takes it, as
    /// the owner's does: a follower joins before its channels exist.
    pub fn tell_unavailable(&self, reason: &crate::capture::CaptureUnavailable) {
        let mut inner = self.inner.lock().unwrap();
        inner.notice = Some(OwnerNotice::Unavailable(reason.clone()));
        inner.capture_not_x = false;
        let targets: Vec<_> = inner
            .followers
            .iter()
            .map(|f| (f.sink.session_id, f.sink.control_dc.clone()))
            .collect();
        drop(inner);
        for (session, cdc) in targets {
            crate::peer::spawn_capture_unavailable_notice(session, cdc, reason.clone());
        }
    }

    /// #3423 — the owner's capture waits on its host (FR-93), and `wait` is
    /// what the owner just told its own controller. This tells every
    /// follower's, the same way [`Self::tell_unavailable`] does, and keeps the
    /// wait for the viewers that join later, until the owner's next frame
    /// ([`Self::on_frame`]) or the wait's own deadline. The owner's loop
    /// calls it; an open waits off the loop and tells through
    /// [`Self::wait_teller`].
    pub fn tell_waiting(&self, wait: crate::capture::OpenWait) {
        tell_waiting(&self.inner, wait, &tokio::runtime::Handle::current());
    }

    /// #3423 — what the owner's capture open keeps of this pipeline. The
    /// open's wait hook runs on the blocking pool and outlives any borrow, so
    /// it holds the pipeline itself and the runtime that sends the notices.
    pub fn wait_teller(&self) -> WaitTeller {
        WaitTeller {
            inner: self.inner.clone(),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    /// #3423 — the owner's capture produced a frame, so whatever it waited
    /// for is over: a viewer that joins from now on is not told about it. The
    /// viewers told already clear their notices at their own first frame.
    /// #3260's reason goes too. Only `NoopCapture` has one, and it captures
    /// nothing, so a frame means a reopen found a capture after all.
    /// Called for every captured frame, so it only takes the lock that
    /// [`Self::beat`] takes on every pass.
    pub fn on_frame(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.notice = None;
    }

    /// Test hook: the wait kept for the viewers that join later, and the time
    /// it has left.
    #[cfg(test)]
    pub(crate) fn kept_wait(&self) -> Option<(crate::capture::WaitFor, Duration)> {
        match self.inner.lock().unwrap().notice {
            Some(OwnerNotice::Waiting { why, until }) => Some((
                why,
                until.saturating_duration_since(std::time::Instant::now()),
            )),
            _ => None,
        }
    }
}

/// #3423 — [`Pipeline::wait_teller`]'s handle.
pub struct WaitTeller {
    inner: Arc<Mutex<PipelineInner>>,
    runtime: tokio::runtime::Handle,
}

impl WaitTeller {
    /// [`Pipeline::tell_waiting`], from an open's wait hook.
    pub fn tell(&self, wait: crate::capture::OpenWait) {
        tell_waiting(&self.inner, wait, &self.runtime);
    }
}

fn tell_waiting(
    inner: &Mutex<PipelineInner>,
    wait: crate::capture::OpenWait,
    runtime: &tokio::runtime::Handle,
) {
    let mut guard = inner.lock().unwrap();
    guard.notice = Some(OwnerNotice::Waiting {
        why: wait.why,
        until: std::time::Instant::now() + wait.gives_up_in,
    });
    let targets: Vec<_> = guard
        .followers
        .iter()
        .map(|f| (f.sink.session_id, f.sink.control_dc.clone()))
        .collect();
    drop(guard);
    for (session, cdc) in targets {
        crate::peer::spawn_media_waiting_notice(runtime, session, cdc, wait);
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if self.registered {
            // Identity-aware removal (rc.443): a stale-evicted owner's LATE
            // drop (the wedged driver call finally returning, or process
            // teardown) must not delete the entry a NEW owner registered
            // after the eviction — remove only if the entry is still ours.
            let mut reg = registry().lock().unwrap();
            if let Some(cur) = reg.get(&self.key)
                && Arc::ptr_eq(cur, &self.inner)
            {
                reg.remove(&self.key);
            }
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

/// #3918 — the `rc:video-info` field that names the capture arm, spelled as
/// `peer::video_info_payload` writes it (a test reads that fn's source).
const CAPTURE_ARM: &str = "capture_arm";

/// #3918 — whether an `rc:video-info` payload names a capture arm that does
/// not read an X server ([`crate::capture::CaptureArm::reads_an_x_server`]).
/// Fails closed: `x11`, a spelling this build does not know, a payload
/// without the field and one that is not JSON all answer `false`.
fn names_a_capture_off_x(payload: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| {
            v.get(CAPTURE_ARM)
                .and_then(serde_json::Value::as_str)
                .and_then(crate::capture::CaptureArm::from_wire)
        })
        .is_some_and(|arm| !arm.reads_an_x_server())
}

/// Join `key`'s pipeline as a follower. `None` when sharing is off, no
/// pipeline owns the key, or the pipeline is closing. On success the
/// follower's DC chunker task is running, a pipeline IDR is requested, and
/// the last `rc:video-info` (if any) is replayed to this viewer, as is the
/// owner's `rc:media-unavailable` (#3260) or `rc:media-waiting` (#3423).
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
    // rc.443 — stale-owner eviction. An owner whose pump task is wedged
    // (field: av1_qsv driver hang inside the inline-blocking encode) never
    // drops its `Pipeline`, so the entry outlives the session and every
    // joiner becomes a follower of a corpse. A silent beat for
    // `PIPELINE_STALE` can only mean that wedge (a healthy loop beats every
    // few ms); evict the entry and refuse the join — the caller then runs
    // its own pump, which registers freshly as the new owner.
    let silent_ms = now_ms().saturating_sub(guard.last_beat_ms.load(Relaxed));
    if silent_ms > PIPELINE_STALE.as_millis() as u64 {
        guard.closed = true;
        for f in guard.followers.drain(..) {
            f.detach(DetachReason::PipelineClosed);
        }
        drop(guard);
        let mut reg = registry().lock().unwrap();
        if let Some(cur) = reg.get(&key)
            && Arc::ptr_eq(cur, &inner)
        {
            reg.remove(&key);
        }
        tracing::warn!(
            ?key,
            silent_ms,
            "media_share: evicted stale pipeline (owner pump silent — wedged encode?); joiner will own a fresh one"
        );
        return None;
    }
    // #3918 — an owner that is beating and is marked as having no picture
    // of this device's virtual desktop (open at an X server exit, or refused
    // while the desktop was down) shares nothing a new viewer should follow
    // while its capture may read the desktop's X server: the arm it
    // published is `x11`, or none is known (`capture_not_x`, which fails
    // closed). An owner silent past `PIPELINE_STALE` was evicted above,
    // marked or not, as on master: its followers re-dispatch. A capture
    // known to be off X (DRM, the portal, mutter) takes
    // joiners, marked owner or not, as on master. The pipeline is closed to
    // joins and its entry removed, as `retire_all` leaves one: the owner and
    // its followers go on, and the caller runs a pump of its own, which
    // registers under the key. The marks are kept only on Linux with
    // `enigo-input` (`input::mark_no_picture`): a build without it has no
    // input of the desktop to protect and keeps the supervisor's two
    // retires, and `full` and `full-hw` carry the feature. Off Linux nothing
    // changes. `ROOMLERD_INPUT_X11_FOLLOW=0` turns this reader off with the
    // arbiter's (`input::has_no_picture`): every pipeline takes joiners, as
    // before #3918. The registry is used as it is when poisoned, as
    // `retire_all` uses it.
    if guard.owner.is_some_and(crate::input::has_no_picture) && !guard.capture_not_x {
        guard.closed = true;
        drop(guard);
        let mut reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cur) = reg.get(&key)
            && Arc::ptr_eq(cur, &inner)
        {
            reg.remove(&key);
        }
        drop(reg);
        tracing::info!(
            ?key,
            "media_share: the pipeline's owner is marked as having no picture of the virtual \
             desktop, and its capture may read the desktop's X server — a new viewer runs its own \
             pipeline (#3918)"
        );
        return None;
    }
    let session_id = sink.session_id;
    let (send_tx, send_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(FOLLOWER_SEND_DEPTH);
    spawn_follower_chunker(
        session_id,
        sink.video_bytes_dc.clone(),
        send_rx,
        sink.chunk_framing,
    );
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
    // #3260 — and, when the owner's capture has no picture, why. Read under
    // the same lock as the push below, so an owner telling its followers at
    // this moment tells this one either way, and only once. #3423 — a wait
    // is told with the time it has left; one past its deadline is not told.
    let notice = match guard.notice.clone() {
        Some(OwnerNotice::Waiting { why, until }) => until
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
            .map(|gives_up_in| JoinNotice::Waiting(crate::capture::OpenWait { gives_up_in, why })),
        Some(OwnerNotice::Unavailable(reason)) => Some(JoinNotice::Unavailable(reason)),
        None => None,
    }
    .map(|tells| (tells, sink.control_dc.clone()));
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
    match notice {
        Some((JoinNotice::Unavailable(reason), cdc)) => {
            crate::peer::spawn_capture_unavailable_notice(session_id, cdc, reason);
        }
        Some((JoinNotice::Waiting(wait), cdc)) => {
            let runtime = tokio::runtime::Handle::current();
            crate::peer::spawn_media_waiting_notice(&runtime, session_id, cdc, wait);
        }
        None => {}
    }
    Some(FollowerGuard {
        inner,
        session_id,
        detach,
    })
}

/// #3671 — the X server that every registered pipeline's capture was opened
/// on has gone. The virtual desktop's supervisor calls this where it tells
/// the open sessions, and again once a restarted desktop is up. Each pipeline
/// is closed to joins and its entry removed, so the next session of a profile
/// runs a pump of its own on the server that answers now. It no longer
/// follows a capture of the old server (that capture's last frame, then
/// keepalives of it) or an open refused while the desktop was down. The
/// owners and their followers stay as they are: the supervisor tells those
/// sessions to reconnect, and a detach would send a follower back into an
/// open that is refused while the desktop is down, which registers a
/// `NoopCapture` owner under the same key. A retired owner's `Drop` leaves a
/// newer owner's entry alone, because it is identity-aware. Retired
/// pipelines no longer count against the spill budget (`MAX_PIPELINES`);
/// they end with their sessions.
///
/// No lock is held across another. The spill gate takes the registry while
/// it holds a pipeline (`pipeline_count()` under `inner`, in the owner's
/// floor merge), so the entries are taken out under the registry's lock and
/// each pipeline is closed after that lock is released. A joiner that
/// fetched a pipeline before it was taken out reads `closed` under the
/// pipeline's lock, so it is refused once this returns. A poisoned lock is
/// used as it is: the caller is the supervisor's thread, which must not
/// panic. Returns how many pipelines were retired.
#[cfg(target_os = "linux")]
pub(crate) fn retire_all() -> usize {
    let retired: Vec<_> = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain()
        .map(|(_, inner)| inner)
        .collect();
    for inner in &retired {
        inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
    }
    if !retired.is_empty() {
        tracing::info!(
            pipelines = retired.len(),
            "media_share: retired the shared pipelines of the desktop's old X server; the next session of each profile runs its own"
        );
    }
    retired.len()
}

/// P6/P5 field fix (2026-08-05) — a follower's control DC just opened.
///
/// `try_join` replays the owner's last `rc:video-info` at JOIN time, but a
/// follower registers during `AgentPeer::new`, before any data channel
/// exists — the replay found an empty stash and was dropped, so a
/// follower's badge fell back to the selection-derived label and never
/// showed `shared ×N`. Re-send it now that the DC is live. No-op when the
/// session isn't a follower or the owner hasn't published yet.
pub fn replay_video_info(session: bson::oid::ObjectId) {
    let pipelines: Vec<Arc<Mutex<PipelineInner>>> =
        registry().lock().unwrap().values().cloned().collect();
    for inner in pipelines {
        let guard = inner.lock().unwrap();
        let Some(info) = guard.video_info.clone() else {
            continue;
        };
        let Some(f) = guard
            .followers
            .iter()
            .find(|f| f.sink.session_id == session)
        else {
            continue;
        };
        let cdc = f.sink.control_dc.clone();
        drop(guard);
        tokio::spawn(async move {
            if let Some(dc) = cdc.lock().await.clone() {
                let _ = dc.send_text(info).await;
            }
        });
        return;
    }
}

/// A follower's `video-bytes` DC just opened.
///
/// The join forces a pipeline IDR the instant the follower registers (during
/// `AgentPeer::new`, ~33 ms after `try_join`), but this video DC finishes
/// negotiating hundreds of ms later — and [`spawn_follower_chunker`] DROPS
/// anything that arrives while the DC is `None`/`!Open`. So that join-IDR is
/// lost, yet [`Pipeline::fan_out`] already flipped the follower to `synced`
/// when it queued it: the follower then receives only deltas it cannot
/// decode — a BLACK SCREEN until the next natural keyframe. Now that the DC
/// is live, reset the follower to unsynced and request a fresh IDR so it
/// re-syncs onto a keyframe the chunker will actually deliver. No-op when the
/// session is a leader (not in any pipeline's follower list) — mirrors
/// [`replay_video_info`], which handles the same race for the control DC.
pub fn resync_follower(session: bson::oid::ObjectId) {
    let pipelines: Vec<Arc<Mutex<PipelineInner>>> =
        registry().lock().unwrap().values().cloned().collect();
    for inner in pipelines {
        let mut guard = inner.lock().unwrap();
        let found = guard.followers.iter_mut().any(|f| {
            if f.sink.session_id == session {
                f.synced = false;
                true
            } else {
                false
            }
        });
        if found {
            guard.kf_needed = true;
            tracing::info!(
                %session,
                "media_share: follower video DC live — re-syncing onto a fresh IDR (join-IDR was dropped pre-open)"
            );
            return;
        }
    }
}

/// The follower's DC send task — the same 16 KiB chunk discipline as the
/// owner pumps' send tasks (single consumer per DC keeps chunk order for
/// the browser reassembler).
fn spawn_follower_chunker(
    session_id: bson::oid::ObjectId,
    video_bytes_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    mut rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    chunk_framing: bool,
) {
    tokio::spawn(async move {
        const SCTP_CHUNK_SIZE: usize = 16 * 1024;
        // Per-follower frame sequence, exactly like the owner's send task:
        // the browser reassembler compares it for equality only, so a
        // follower's numbering being independent of the owner's is fine.
        let mut frame_seq: u32 = 0;
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
            let chunk_count = total.div_ceil(SCTP_CHUNK_SIZE).max(1) as u16;
            let mut chunk_idx: u16 = 0;
            frame_seq = frame_seq.wrapping_add(1);
            let mut off = 0usize;
            while off < total {
                let end = (off + SCTP_CHUNK_SIZE).min(total);
                // MUST match the owner's send task (`media_pump_ffmpeg_dc`):
                // apply the FR-17 8-byte chunk header when this follower
                // negotiated it, or send raw slices when it did not. Sending
                // raw slices to a chunk-framing browser hands it a chunk
                // header parsed from AV1 payload — undecodable, a black screen.
                let res = if chunk_framing {
                    let framed = crate::peer::chunk_framed(
                        frame_seq,
                        chunk_idx,
                        chunk_count,
                        &wire.slice(off..end),
                    );
                    dc.send(&bytes::Bytes::from(framed)).await
                } else {
                    dc.send(&wire.slice(off..end)).await
                };
                if let Err(e) = res {
                    tracing::debug!(session = %session_id, %e, "media_share follower: DC send failed");
                    break;
                }
                off = end;
                chunk_idx = chunk_idx.wrapping_add(1);
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

/// The registry (and its `pipeline_count()` spill-budget input) is
/// process-global, and cargo runs tests concurrently — a sibling test's live
/// pipeline would flip the budget gate mid-test. Every registry-touching test
/// holds this, in this module and elsewhere in the crate (FR-93 P6c's
/// `peer.rs` test builds a pipeline too). `unwrap_or_else(into_inner)`
/// un-poisons after a failed test so the rest still report their own results.
#[cfg(test)]
pub(crate) fn test_serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

/// #3671, #3918 — whether a new viewer joins `key`'s pipeline now, for a
/// test of the desktop's hooks outside this module: [`try_join`] itself, for
/// a viewer of its own that leaves at once, on a runtime of its own (a join
/// spawns the follower's sender). A refusal closes the pipeline to joins, as
/// it does for a real viewer.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn takes_joiners(key: PipelineKey) -> bool {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for one join");
    rt.block_on(async { try_join(key, tests::sink(bson::oid::ObjectId::new()), 60).is_some() })
}

#[cfg(test)]
// The SERIAL guard is deliberately held across awaits: it serializes whole
// tests against the process-global registry, and every await inside is a
// local Notify (no cross-test lock dependency → no deadlock).
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use bson::oid::ObjectId;

    use super::test_serial as serial;

    pub(super) fn sink(session: ObjectId) -> FollowerSink {
        FollowerSink {
            session_id: session,
            video_bytes_dc: Arc::new(tokio::sync::Mutex::new(None)),
            control_dc: Arc::new(tokio::sync::Mutex::new(None)),
            keyframe_requested: Arc::new(AtomicBool::new(false)),
            target_resolution: Arc::new(Mutex::new(TargetResolution::Native)),
            quality_state: Arc::new(AtomicU8::new(2)),
            viewer_report: Arc::new(crate::encode::viewer_rate::ViewerFeedback::new()),
            priority: Arc::new(AtomicU8::new(1)),
            capture_native_dims: Arc::new(AtomicU64::new(0)),
            encoded_dims: Arc::new(AtomicU64::new(0)),
            chunk_framing: false,
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

    /// FR-93 P6c — the owner names its followers' sessions, and a follower
    /// that left is no longer named.
    #[tokio::test]
    async fn the_owner_names_its_followers_sessions() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-P6C-NAMES");
        let owner = Pipeline::register(key, ObjectId::new());
        let (a, b) = (ObjectId::new(), ObjectId::new());
        let ga = try_join(key, sink(a), 60).expect("join a");
        let _gb = try_join(key, sink(b), 60).expect("join b");
        let mut named = owner.follower_sessions();
        named.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(named, want);
        drop(ga);
        assert_eq!(owner.follower_sessions(), vec![b]);
    }

    /// rc.443 — a joiner finding the owner's beat stale evicts the corpse
    /// and the key becomes ownable again (field: a driver-hung owner pump
    /// can never drop its `Pipeline`, so every viewer joined a dead
    /// pipeline until the daemon restarted).
    #[tokio::test]
    async fn stale_owner_is_evicted_and_key_becomes_ownable() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-STALE");
        let wedged = Pipeline::register(key, ObjectId::new());
        // A live follower on the corpse must be detached by the eviction.
        let orphan = try_join(key, sink(ObjectId::new()), 60).expect("join while fresh");

        wedged.force_stale();
        assert!(
            try_join(key, sink(ObjectId::new()), 60).is_none(),
            "stale owner must refuse the join (and evict)"
        );
        assert!(
            registry().lock().unwrap().get(&key).is_none(),
            "eviction must free the registry slot"
        );
        assert_eq!(orphan.detached().await, DetachReason::PipelineClosed);

        // The would-be joiner now registers as a fresh owner...
        let fresh = Pipeline::register(key, ObjectId::new());
        assert!(
            try_join(key, sink(ObjectId::new()), 60).is_some(),
            "fresh owner is joinable"
        );
        // ...and the wedged owner's LATE drop (driver call finally
        // returning) must NOT delete the fresh registration.
        drop(wedged);
        assert!(
            registry().lock().unwrap().get(&key).is_some(),
            "late drop of the evicted owner deleted the new registration"
        );
        drop(fresh);
        assert!(registry().lock().unwrap().get(&key).is_none());
    }

    /// rc.443 — a beating owner is never evicted: the staleness horizon
    /// only trips on genuine silence.
    #[tokio::test]
    async fn beating_owner_is_not_evicted() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-BEAT");
        let owner = Pipeline::register(key, ObjectId::new());
        owner.beat();
        assert!(
            try_join(key, sink(ObjectId::new()), 60).is_some(),
            "fresh beat must admit the join"
        );
        drop(owner);
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

    /// A control channel between two in-process peers: the device's end, and
    /// every text message the viewer's end receives. The viewer opens it, as
    /// a browser does, and every candidate travels in the SDP (no trickle),
    /// as `tests/privacy_dc.rs` does.
    async fn viewer_channel() -> (
        [Arc<webrtc::peer_connection::RTCPeerConnection>; 2],
        Arc<RTCDataChannel>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        // #4500 — the viewer and the device gather on loopback only
        // (crate::test_net).
        use webrtc::peer_connection::configuration::RTCConfiguration;
        let pc = || async {
            Arc::new(
                crate::test_net::api()
                    .new_peer_connection(RTCConfiguration::default())
                    .await
                    .expect("a peer connection"),
            )
        };
        let (viewer, device) = (pc().await, pc().await);
        let viewer_dc = viewer
            .create_data_channel("control", None)
            .await
            .expect("a data channel");
        let (heard_tx, heard) = tokio::sync::mpsc::unbounded_channel();
        viewer_dc.on_message(Box::new(move |msg| {
            let heard_tx = heard_tx.clone();
            Box::pin(async move {
                if msg.is_string
                    && let Ok(s) = std::str::from_utf8(&msg.data)
                {
                    let _ = heard_tx.send(s.to_string());
                }
            })
        }));
        let (dc_tx, dc_rx) = tokio::sync::oneshot::channel();
        let dc_tx = Mutex::new(Some(dc_tx));
        device.on_data_channel(Box::new(move |dc| {
            if let Some(tx) = dc_tx.lock().unwrap().take() {
                let _ = tx.send(dc);
            }
            Box::pin(async {})
        }));
        let offer = viewer.create_offer(None).await.expect("an offer");
        let mut gathered = viewer.gathering_complete_promise().await;
        viewer
            .set_local_description(offer)
            .await
            .expect("the offer");
        let _ = gathered.recv().await;
        let offer = viewer.local_description().await.expect("a local offer");
        device
            .set_remote_description(offer)
            .await
            .expect("the offer");
        let answer = device.create_answer(None).await.expect("an answer");
        let mut gathered = device.gathering_complete_promise().await;
        device
            .set_local_description(answer)
            .await
            .expect("the answer");
        let _ = gathered.recv().await;
        let answer = device.local_description().await.expect("a local answer");
        viewer
            .set_remote_description(answer)
            .await
            .expect("the answer");
        let dc = tokio::time::timeout(Duration::from_secs(30), dc_rx)
            .await
            .expect("the control channel never reached the device")
            .expect("the channel's sender");
        ([viewer, device], dc, heard)
    }

    fn no_display() -> crate::capture::CaptureUnavailable {
        crate::capture::CaptureUnavailable::new(
            crate::capture::CaptureUnavailableCode::NoDisplay,
            "no desktop session",
        )
    }

    /// The viewer heard the owner's FR-80 reason once: the first message
    /// within `WAIT`, and nothing more for two of the notice's retry periods.
    async fn told_once(heard: &mut tokio::sync::mpsc::UnboundedReceiver<String>) {
        const WAIT: Duration = Duration::from_secs(10);
        let first = tokio::time::timeout(WAIT, heard.recv())
            .await
            .expect("the follower's viewer was never told why there is no picture")
            .expect("the viewer's end closed");
        assert!(
            first.starts_with(r#"{"t":"rc:media-unavailable","code":"no_display","#),
            "the owner's reason, word for word: {first}"
        );
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(heard.try_recv().is_err(), "told once, not on every retry");
    }

    /// #3260 — an owner whose capture has no picture tells the followers it
    /// already has why. Before it, a follower opened no capture, heard
    /// nothing, and its viewer read the black canvas as a network fault.
    /// Red without `tell_unavailable`'s delivery.
    #[tokio::test]
    async fn a_follower_is_told_why_its_owner_has_no_picture() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3260-FOLLOWER");
        let owner = Pipeline::register(key, ObjectId::new());
        let (pcs, dc, mut heard) = viewer_channel().await;
        let follower = sink(ObjectId::new());
        *follower.control_dc.lock().await = Some(dc);
        let _guard = try_join(key, follower, 60).expect("join");
        owner.tell_unavailable(&no_display());
        told_once(&mut heard).await;
        for pc in pcs {
            let _ = pc.close().await;
        }
    }

    /// #3260 — and a viewer that joins after the owner said so is told as it
    /// joins. It joins before its control channel exists, as every follower
    /// does (it registers while its peer is built), so the notice waits for
    /// the channel. Red without `try_join`'s replay.
    #[tokio::test]
    async fn a_viewer_joining_later_is_told_too() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3260-JOINER");
        let owner = Pipeline::register(key, ObjectId::new());
        owner.tell_unavailable(&no_display());
        let joiner = sink(ObjectId::new());
        let control = joiner.control_dc.clone();
        let _guard = try_join(key, joiner, 60).expect("join");
        let (pcs, dc, mut heard) = viewer_channel().await;
        *control.lock().await = Some(dc);
        told_once(&mut heard).await;
        for pc in pcs {
            let _ = pc.close().await;
        }
    }

    /// The person at the screen is asked to share it, and the request gives up
    /// in `gives_up_in` (FR-93 P2d-2's portal dialog).
    fn dialog(gives_up_in: Duration) -> crate::capture::OpenWait {
        crate::capture::OpenWait {
            gives_up_in,
            why: crate::capture::WaitFor::Dialog { control: false },
        }
    }

    /// The viewer heard the owner's wait once, within `WAIT`, and nothing more
    /// for two of the notice's retry periods. Its `waits_ms`.
    async fn told_to_wait(heard: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> u64 {
        const WAIT: Duration = Duration::from_secs(10);
        let first = tokio::time::timeout(WAIT, heard.recv())
            .await
            .expect("the follower's viewer was never told that the capture waits on the host")
            .expect("the viewer's end closed");
        let told: serde_json::Value = serde_json::from_str(&first).expect("a JSON notice");
        assert_eq!(told["t"], "rc:media-waiting", "{first}");
        assert_eq!(
            told["for"], "person_at_screen",
            "what the owner waits for: {first}"
        );
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(heard.try_recv().is_err(), "told once, not on every retry");
        told["waits_ms"].as_u64().expect("waits_ms")
    }

    /// #3423 — an owner whose capture waits on its host tells the followers it
    /// already has, with its own deadline. Before it, a follower's viewer saw no
    /// frame and no reason, and its watchdog re-created the session every 10 s
    /// for as long as the wait lasted. Red without `tell_waiting`'s delivery.
    #[tokio::test]
    async fn a_follower_is_told_its_owner_waits() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3423-FOLLOWER");
        let owner = Pipeline::register(key, ObjectId::new());
        let (pcs, dc, mut heard) = viewer_channel().await;
        let follower = sink(ObjectId::new());
        *follower.control_dc.lock().await = Some(dc);
        let _guard = try_join(key, follower, 60).expect("join");
        owner.tell_waiting(dialog(Duration::from_secs(120)));
        assert_eq!(
            told_to_wait(&mut heard).await,
            120_000,
            "the owner's own deadline"
        );
        for pc in pcs {
            let _ = pc.close().await;
        }
    }

    /// #3423 — a viewer that joins while the owner waits is told the time the
    /// wait has left, not the owner's whole wait: its viewer holds its
    /// watchdog until then. Red without `try_join`'s replay of the wait.
    #[tokio::test]
    async fn a_viewer_joining_during_a_wait_is_told_the_time_left() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3423-JOINER");
        let owner = Pipeline::register(key, ObjectId::new());
        owner.tell_waiting(dialog(Duration::from_secs(120)));
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let joiner = sink(ObjectId::new());
        let control = joiner.control_dc.clone();
        let _guard = try_join(key, joiner, 60).expect("join");
        let (pcs, dc, mut heard) = viewer_channel().await;
        *control.lock().await = Some(dc);
        let left = told_to_wait(&mut heard).await;
        assert!(
            (60_000..=118_500).contains(&left),
            "the time the wait has left, 1.5 s in: {left} ms"
        );
        for pc in pcs {
            let _ = pc.close().await;
        }
    }

    /// #3423 — a wait is over at the owner's next frame, and one past its own
    /// deadline is over too: a viewer that joins after either is told nothing,
    /// so no notice outlives what it describes.
    #[tokio::test]
    async fn a_viewer_joining_after_a_wait_ends_is_told_nothing() {
        let _s = serial();
        for (key, by_frame, ends) in [
            ("TEST-3423-FRAME", true, "the owner's next frame"),
            ("TEST-3423-DEADLINE", false, "the wait's own deadline"),
        ] {
            let key = PipelineKey::FfmpegDc(key);
            let owner = Pipeline::register(key, ObjectId::new());
            if by_frame {
                owner.tell_waiting(dialog(Duration::from_secs(120)));
                owner.on_frame();
            } else {
                owner.tell_waiting(dialog(Duration::from_millis(200)));
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            let (pcs, dc, mut heard) = viewer_channel().await;
            let joiner = sink(ObjectId::new());
            *joiner.control_dc.lock().await = Some(dc);
            let _guard = try_join(key, joiner, 60).expect("join");
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            assert!(
                heard.try_recv().is_err(),
                "a joiner after {ends} was told of a wait that is over"
            );
            for pc in pcs {
                let _ = pc.close().await;
            }
        }
    }

    /// #3423 — a wait that ends with no capture (the dialog was refused, or
    /// gave up) is replaced by the reason: a viewer that joins then is told
    /// why there is no picture, and nothing about a wait.
    #[tokio::test]
    async fn a_wait_that_ends_with_no_capture_is_told_as_its_reason() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3423-REPLACED");
        let owner = Pipeline::register(key, ObjectId::new());
        owner.tell_waiting(dialog(Duration::from_secs(120)));
        owner.tell_unavailable(&no_display());
        let (pcs, dc, mut heard) = viewer_channel().await;
        let joiner = sink(ObjectId::new());
        *joiner.control_dc.lock().await = Some(dc);
        let _guard = try_join(key, joiner, 60).expect("join");
        told_once(&mut heard).await;
        for pc in pcs {
            let _ = pc.close().await;
        }
    }

    /// #3260, #3423 — a capture that had no picture and then has one (a reopen
    /// found a capture: the FFmpeg pump's transport flip reopens whatever it
    /// holds) drops the kept reason at its first frame, so a viewer that joins
    /// after that is not told there is no picture. Red if a frame clears only
    /// a wait.
    #[tokio::test]
    async fn a_viewer_joining_after_the_capture_recovers_is_told_nothing() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3423-RECOVERED");
        let owner = Pipeline::register(key, ObjectId::new());
        owner.tell_unavailable(&no_display());
        owner.on_frame();
        let (pcs, dc, mut heard) = viewer_channel().await;
        let joiner = sink(ObjectId::new());
        *joiner.control_dc.lock().await = Some(dc);
        let _guard = try_join(key, joiner, 60).expect("join");
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            heard.try_recv().is_err(),
            "a joiner after the capture recovered was told it has no picture"
        );
        for pc in pcs {
            let _ = pc.close().await;
        }
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

    /// FR-119 P2b-2 — the frame-rate ask merges to the smallest, and a viewer
    /// that asked nothing holds the stream at today's rate.
    #[tokio::test]
    async fn the_frame_rate_ask_takes_the_floor() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-FPS");
        let owner = Pipeline::register(key, ObjectId::new());
        assert_eq!(owner.merged_fps_ask(Some(144)), Some(144), "alone");
        let s = sink(ObjectId::new());
        let follower_report = s.viewer_report.clone();
        let _g = try_join(key, s, 60).expect("join");
        assert_eq!(owner.merged_fps_ask(Some(144)), None, "it asked nothing");
        follower_report.store_fps_ask(120);
        assert_eq!(owner.merged_fps_ask(Some(144)), Some(120));
        assert_eq!(owner.merged_fps_ask(None), None);
    }

    /// FR-135 R2 — the slowest screen that has said holds the stream, and a
    /// viewer yet to measure is no floor, or a late joiner would lift a cap the
    /// owner's own screen set.
    #[tokio::test]
    async fn the_viewers_refresh_takes_the_slowest_that_said() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-VIEWER-HZ");
        let owner = Pipeline::register(key, ObjectId::new());
        assert_eq!(owner.merged_viewer_hz(None), None, "nobody said");
        assert_eq!(owner.merged_viewer_hz(Some(144)), Some(144), "alone");
        let s = sink(ObjectId::new());
        let follower_report = s.viewer_report.clone();
        let _g = try_join(key, s, 60).expect("join");
        assert_eq!(
            owner.merged_viewer_hz(Some(144)),
            Some(144),
            "a follower yet to measure"
        );
        follower_report.store_viewer_hz(60);
        assert_eq!(owner.merged_viewer_hz(Some(144)), Some(60));
        assert_eq!(owner.merged_viewer_hz(None), Some(60), "the owner yet to");
    }

    // P8 Phase 4 — the mixed-dial predicate: false with no followers,
    // false when everyone matches the owner, true on either a Priority
    // or a resolution-pick divergence.
    #[tokio::test]
    async fn dials_mixed_detects_any_divergence() {
        let _s = serial();
        use crate::encode::priority::{BALANCED, SMOOTHER};
        let key = PipelineKey::FfmpegDc("TEST-MIXED");
        let owner = Pipeline::register(key, ObjectId::new());
        assert!(
            !owner.dials_mixed(BALANCED, TargetResolution::Native),
            "no followers = nothing shared, nothing mixed"
        );

        let s = sink(ObjectId::new());
        s.priority.store(BALANCED, Relaxed);
        let _g = try_join(key, s, 60).expect("join");
        assert!(
            !owner.dials_mixed(BALANCED, TargetResolution::Native),
            "identical dials are not mixed"
        );
        assert!(
            owner.dials_mixed(SMOOTHER, TargetResolution::Native),
            "priority divergence is mixed"
        );
        assert!(
            owner.dials_mixed(
                BALANCED,
                TargetResolution::Fixed {
                    width: 1280,
                    height: 720
                }
            ),
            "resolution-pick divergence is mixed"
        );
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
        report.store_report(crate::encode::viewer_rate::pack_report(20, true));
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
            report.store_report(crate::encode::viewer_rate::pack_report(5, true));
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
            report.store_report(crate::encode::viewer_rate::pack_report(5, true));
            owner.step_viewer_windows(1, 60);
        }
        assert_eq!(
            owner.follower_count(),
            1,
            "budget-capped pipeline keeps the slow viewer (floor holds)"
        );
        drop(other);
    }

    /// P7b — the merge-aware refine-eligibility decision table. Every
    /// cap-contributing dial must be refine-applicable; no-cap dials
    /// (Sharper anywhere; Balanced on direct) never block.
    ///
    /// Env hygiene: `idle_refine_applies` consults
    /// ROOMLERD_IDLE_REFINE_BALANCED for the Balanced rows — cleared
    /// up front so the table asserts the DEFAULT (ON since P7c) semantics,
    /// then set to "0" for the blocked-Balanced rows and removed again
    /// before the serial guard drops. (The env flip itself is locked by
    /// `encode::tests::idle_refine_applies_matrix`; that test restores the
    /// var after itself, so the cross-module interleaving window is µs.)
    #[tokio::test]
    async fn merged_refine_eligible_decision_table() {
        let _s = serial();
        // rc.445 — the dial caps (and with them this whole merge) exist
        // only behind the restore switch now; hold the shared env lock
        // since `encode::tests` also toggles this var.
        let _env = crate::encode::RELAY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: same contract as the sibling env-clearing tests.
        unsafe {
            tunnel_core::env::test_env::clear("IDLE_REFINE_BALANCED");
            tunnel_core::env::test_env::set_as("ROOMLERD_", "PRIORITY_RES_CAP", "1");
        }
        use crate::encode::priority::{BALANCED, SHARPER, SMOOTHER};
        let key = PipelineKey::FfmpegDc("TEST-REFINE");
        let owner = Pipeline::register(key, ObjectId::new());

        // Single viewer reduces to idle_refine_applies(own): Smoother
        // refines, Sharper is vacuously eligible (no cap — the caller's
        // merged-cap-clamps gate is what makes it a no-op), and since P7c
        // Balanced's B1 relay cap is liftable by default too.
        assert!(owner.merged_refine_eligible(SMOOTHER, true));
        assert!(owner.merged_refine_eligible(SMOOTHER, false));
        assert!(owner.merged_refine_eligible(SHARPER, true));
        assert!(owner.merged_refine_eligible(BALANCED, true));
        assert!(owner.merged_refine_eligible(BALANCED, false));

        // THE field case (winhost-b 2026-08-20): owner=Sharper +
        // follower=Smoother — the follower's liftable cap clamps, Sharper
        // contributes none ⇒ eligible.
        let s = sink(ObjectId::new());
        let follower_prio = s.priority.clone();
        follower_prio.store(SMOOTHER, std::sync::atomic::Ordering::Relaxed);
        let guard = try_join(key, s, 60).expect("join");
        assert!(owner.merged_refine_eligible(SHARPER, true));
        assert!(owner.merged_refine_eligible(SHARPER, false));

        // With the Balanced kill switch thrown, a Balanced dial's relay cap
        // is NOT liftable: single-viewer Balanced blocks, and a Smoother
        // owner + Balanced follower blocks on relay; on direct Balanced
        // contributes no cap ⇒ eligible either way.
        unsafe { tunnel_core::env::test_env::set_as("ROOMLERD_", "IDLE_REFINE_BALANCED", "0") };
        follower_prio.store(BALANCED, std::sync::atomic::Ordering::Relaxed);
        assert!(!owner.merged_refine_eligible(SMOOTHER, true));
        assert!(owner.merged_refine_eligible(SMOOTHER, false));
        assert!(!owner.merged_refine_eligible(BALANCED, true));
        unsafe { tunnel_core::env::test_env::clear("IDLE_REFINE_BALANCED") };

        // Default-on: the same mixed pair is eligible on relay too.
        assert!(owner.merged_refine_eligible(SMOOTHER, true));

        // Back to an all-liftable pair.
        follower_prio.store(SMOOTHER, std::sync::atomic::Ordering::Relaxed);
        assert!(owner.merged_refine_eligible(SMOOTHER, true));

        // Follower leaves → single-viewer semantics return.
        drop(guard);
        assert_eq!(owner.follower_count(), 0);
        assert!(owner.merged_refine_eligible(SHARPER, true));
        drop(owner);
        // rc.445 default (caps off): nothing can block — eligibility is
        // vacuous, matching the caller's capped_below_native=false gate.
        unsafe { tunnel_core::env::test_env::clear("PRIORITY_RES_CAP") };
        let owner = Pipeline::register(key, ObjectId::new());
        assert!(owner.merged_refine_eligible(SMOOTHER, true));
        assert!(owner.merged_refine_eligible(BALANCED, true));
        drop(owner);
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

    /// #3671 — once the desktop's X server is gone, a pipeline whose capture
    /// was opened on it takes no joiner: not one that finds it in the
    /// registry, and not one that fetched it before it was retired (it reads
    /// `closed` under the pipeline's lock). The next session of that profile
    /// owns the key afresh, with a pump and a capture of its own.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_retired_pipeline_takes_no_joiner_and_its_key_is_free_for_a_new_owner() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3671-RETIRED");
        let old = Pipeline::register(key, ObjectId::new());
        retire_all();
        assert!(
            try_join(key, sink(ObjectId::new()), 60).is_none(),
            "a joiner followed a capture of the old X server"
        );
        assert!(
            old.inner.lock().unwrap().closed,
            "a joiner that fetched the pipeline before the retire would join it"
        );
        let new = Pipeline::register(key, ObjectId::new());
        let joined = try_join(key, sink(ObjectId::new()), 60).expect("the new owner takes joiners");
        assert_eq!((new.follower_count(), old.follower_count()), (1, 0));
        drop(joined);
    }

    /// #3671 — a retired owner keeps the followers it had, since the desktop's
    /// supervisor tells those sessions to reconnect and a detach would send
    /// each into an open refused while the desktop is down. They leave when
    /// the owner ends, as before. Its end leaves the entry of the owner that
    /// registered the key after the retire.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_retired_owner_keeps_its_followers_and_its_end_leaves_the_new_owners_entry() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3671-KEPT");
        let old = Pipeline::register(key, ObjectId::new());
        let told = try_join(key, sink(ObjectId::new()), 60).expect("joined before the exit");
        retire_all();
        let new = Pipeline::register(key, ObjectId::new());
        assert!(new.registered, "the key was not free after the retire");
        assert_eq!(old.follower_count(), 1, "the retire detached a follower");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), told.detached())
                .await
                .is_err(),
            "the retire detached a follower"
        );
        drop(old);
        assert_eq!(told.detached().await, DetachReason::PipelineClosed);
        assert!(
            registry()
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|cur| Arc::ptr_eq(cur, &new.inner)),
            "the retired owner's end took the new owner's entry"
        );
    }

    /// #3918 — a pipeline whose owner was told that it has no picture of this
    /// device's virtual desktop takes no joiner while nothing known to be off
    /// X has been published for it (here nothing is published), whichever
    /// told it: the tell (its capture was on an X server that is gone) or its
    /// own refused open.
    /// It is closed and its entry removed, as a retire leaves one: its owner
    /// and the follower it had go on, told, and the next session of the
    /// profile owns the key afresh, with a capture of its own.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[tokio::test]
    async fn a_pipeline_whose_owner_has_no_picture_takes_no_joiner() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3918-NO-PICTURE");
        let owner = ObjectId::new();
        let old = Pipeline::register(key, owner);
        let told =
            try_join(key, sink(ObjectId::new()), 60).expect("joined before the owner was told");
        crate::input::mark_no_picture(owner);
        let refused = try_join(key, sink(ObjectId::new()), 60).is_none();
        let followers_after_the_refusal = old.follower_count();
        let new = Pipeline::register(key, ObjectId::new());
        let joined = try_join(key, sink(ObjectId::new()), 60);
        crate::input::x_follow::no_picture::forget(owner);
        assert!(
            refused,
            "a new viewer followed the pipeline of an owner with no picture"
        );
        assert!(old.inner.lock().unwrap().closed);
        // The refused joiner is not attached to the old pipeline, and the
        // told follower is not detached from it: exactly the one it had.
        assert_eq!(
            followers_after_the_refusal, 1,
            "the refused joiner was attached to the old pipeline, or its follower was detached"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), told.detached())
                .await
                .is_err(),
            "the refusal detached the told owner's follower"
        );
        assert!(new.registered, "the key was not free for a new owner");
        assert!(joined.is_some(), "the new owner takes joiners");
        // The old pipeline's end leaves the new owner's entry under the key.
        drop(old);
        assert_eq!(told.detached().await, DetachReason::PipelineClosed);
        assert!(
            registry()
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|cur| Arc::ptr_eq(cur, &new.inner)),
            "the told owner's end took the new owner's entry"
        );
        drop((joined, new));
    }

    /// #3918 — an owner silent past [`PIPELINE_STALE`] is
    /// evicted as on master (rc.443), marked or not: the join is refused, its
    /// followers are detached with `PipelineClosed`, so that they re-dispatch,
    /// and the key is ownable again. The mark's refusal is for an owner that
    /// is beating. Here the owner is marked, its capture is not known to be
    /// off X, and it has one follower.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[tokio::test]
    async fn a_marked_owner_silent_past_the_stale_horizon_is_evicted_with_its_followers() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3918-MARKED-STALE");
        let owner = ObjectId::new();
        let wedged = Pipeline::register(key, owner);
        let follower =
            try_join(key, sink(ObjectId::new()), 60).expect("joined before the owner was told");
        crate::input::mark_no_picture(owner);
        wedged.force_stale();
        let refused = try_join(key, sink(ObjectId::new()), 60).is_none();
        let entry_left = registry().lock().unwrap().get(&key).is_some();
        let detached = tokio::time::timeout(Duration::from_millis(50), follower.detached())
            .await
            .ok();
        let followers_left = wedged.follower_count();
        let fresh = Pipeline::register(key, ObjectId::new());
        let joined = try_join(key, sink(ObjectId::new()), 60);
        crate::input::x_follow::no_picture::forget(owner);
        assert!(refused, "a new viewer followed a marked owner's corpse");
        assert!(wedged.inner.lock().unwrap().closed);
        assert!(!entry_left, "the eviction left the marked corpse's entry");
        assert_eq!(
            (detached, followers_left),
            (Some(DetachReason::PipelineClosed), 0),
            "a marked owner silent past the stale horizon kept its follower"
        );
        assert!(fresh.registered, "the key was not ownable again");
        assert!(joined.is_some(), "the fresh owner takes joiners");
        // The evicted owner's late drop leaves the fresh owner's entry.
        drop(wedged);
        assert!(
            registry()
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|cur| Arc::ptr_eq(cur, &fresh.inner)),
            "the evicted owner's late drop took the fresh owner's entry"
        );
        drop((joined, fresh));
    }

    /// #3918 — every capture arm, in the one list the join tests walk. A new
    /// arm does not compile until [`reads_x`], right below, decides it; it
    /// reaches this list only by hand, so an arm decided there and left out
    /// here is not walked by the join tests. `CaptureArm::from_wire` keeps its
    /// own list: an arm this list names that is missing there fails closed,
    /// and `every_capture_arm_is_decided_on_or_off_x_and_what_is_unknown_fails_closed`
    /// turns red.
    const EVERY_ARM: [crate::capture::CaptureArm; 4] = [
        crate::capture::CaptureArm::Drm,
        crate::capture::CaptureArm::Mutter,
        crate::capture::CaptureArm::Portal,
        crate::capture::CaptureArm::X11,
    ];

    /// Whether `arm` reads an X server, decided here by hand. No wildcard: a
    /// new arm does not compile until it is decided, beside [`EVERY_ARM`].
    fn reads_x(arm: crate::capture::CaptureArm) -> bool {
        use crate::capture::CaptureArm;
        match arm {
            CaptureArm::X11 => true,
            CaptureArm::Drm | CaptureArm::Mutter | CaptureArm::Portal => false,
        }
    }

    /// An `rc:video-info` payload naming `arm` as [`CAPTURE_ARM`], in the
    /// producer's shape (`peer::video_info_payload`).
    fn info_naming(arm: &str) -> String {
        format!(r#"{{"t":"rc:video-info","{CAPTURE_ARM}":"{arm}","attended":false}}"#)
    }

    /// The text of `peer.rs` as compiled, for the tests that pin what it does
    /// with this module's pipelines and payloads.
    const PEER_RS: &str = include_str!("peer.rs");

    /// The fn of `peer.rs` whose head is `head`, to its closing brace at the
    /// start of a line. Fails unless there is exactly one, so a rename cannot
    /// make a test pass by reading nothing.
    fn peer_fn(head: &str) -> String {
        // A Windows checkout has CRLF line ends.
        let src = PEER_RS.replace("\r\n", "\n");
        assert_eq!(src.matches(head).count(), 1, "one `{head}` in peer.rs");
        let rest = &src[src.find(head).expect(head)..];
        rest[..rest.find("\n}\n").expect("the fn's end")].to_string()
    }

    /// `peer.rs` as compiled outside its tests: each `#[cfg(test)]` item at
    /// the top level (all of them are modules) is cut out, to its closing
    /// brace at the start of a line.
    fn peer_outside_tests() -> String {
        let mut src = PEER_RS.replace("\r\n", "\n");
        while let Some(at) = src.find("\n#[cfg(test)]\n") {
            let len = src[at + 1..].find("\n}\n").expect("a test item's end") + 3;
            src.replace_range(at + 1..at + 1 + len, "");
        }
        src
    }

    /// Every capture open in `body`, each to its closing paren, with where it
    /// starts: a call of `capture::open…(`, whichever open function it names
    /// (`open_default_async`, `open_async`, a later one). A mention that is
    /// not a call is left out, and so are the two `open_*` predicates, which
    /// open nothing.
    fn capture_opens(body: &str) -> Vec<(usize, &str)> {
        const OPEN: &str = "capture::open";
        const OPENS_NOTHING: [&str; 2] = ["capture::open_has_session(", "capture::open_abandoned("];
        body.match_indices(OPEN)
            .filter_map(|(at, _)| {
                let call = &body[at..];
                let paren =
                    call.find(|c: char| c != '_' && c != ':' && !c.is_ascii_alphanumeric())?;
                if !call[paren..].starts_with('(')
                    || OPENS_NOTHING.iter().any(|p| call.starts_with(p))
                {
                    return None;
                }
                let mut depth = 0usize;
                let end = call[paren..]
                    .char_indices()
                    .find_map(|(i, c)| match c {
                        '(' => {
                            depth += 1;
                            None
                        }
                        ')' => {
                            depth -= 1;
                            (depth == 0).then_some(paren + i)
                        }
                        _ => None,
                    })
                    .expect("the open's closing paren");
                Some((at, &call[..=end]))
            })
            .collect()
    }

    /// #3918 — every open of a registered pipeline's capturer
    /// is built with the clearing builder, `peer::shared_media_waiting_hook`:
    /// a reopen's start clears the capture the pipeline published
    /// ([`Pipeline::capture_opens_again`]). An open built with the plain
    /// `media_waiting_hook` would let a marked owner's pipeline take a joiner
    /// on the arm of the capturer it replaced. Read from the source as
    /// compiled, in both DC pumps by name, through whichever capture open
    /// function a call names; a pump, a register or an open
    /// this cannot find fails, and so does a `Pipeline::register(` outside
    /// the two pumps, which a third pump would escape the reads with. One
    /// exception, named: the VP9-444 pump's first open runs before it
    /// registers its pipeline, so nothing is published there to clear and no
    /// viewer can join yet.
    #[test]
    fn every_open_of_a_registered_pipelines_capturer_clears_its_published_capture() {
        const REGISTER: &str = "Pipeline::register(";
        const CLEARING: &str = "shared_media_waiting_hook(";
        const PLAIN: &str = "media_waiting_hook(";
        // The FFmpeg pump registers before its first open: every open clears.
        let ffmpeg = peer_fn("async fn media_pump_ffmpeg_dc(");
        let registered = ffmpeg.find(REGISTER).expect("the FFmpeg pump's register");
        let opens = capture_opens(&ffmpeg);
        assert!(!opens.is_empty(), "the FFmpeg pump opens no capturer");
        for (at, open) in &opens {
            assert!(
                registered < *at,
                "an FFmpeg open before its register: {open}"
            );
            assert!(
                open.contains(CLEARING),
                "an FFmpeg open that clears nothing: {open}"
            );
        }
        // The VP9-444 pump: its first open, before the register, is the one
        // with the plain builder; every open after the register clears.
        let vp9 = peer_fn("async fn media_pump_vp9_444_dc(");
        let registered = vp9.find(REGISTER).expect("the VP9-444 pump's register");
        let (first, reopens): (Vec<_>, Vec<_>) = capture_opens(&vp9)
            .into_iter()
            .partition(|(at, _)| *at < registered);
        assert_eq!(
            first.len(),
            1,
            "VP9-444 opens before its register: {first:?}"
        );
        assert!(
            first[0].1.contains(PLAIN) && !first[0].1.contains(CLEARING),
            "the VP9-444 first open: {}",
            first[0].1
        );
        assert!(!reopens.is_empty(), "the VP9-444 pump reopens no capturer");
        for (_, open) in &reopens {
            assert!(
                open.contains(CLEARING),
                "a VP9-444 reopen that clears nothing: {open}"
            );
        }
        // Pipelines are registered in these two pumps only.
        assert_eq!(
            peer_outside_tests().matches(REGISTER).count(),
            ffmpeg.matches(REGISTER).count() + vp9.matches(REGISTER).count(),
            "a `{REGISTER}` in peer.rs outside the two pumps read here: name its pump in this test"
        );
    }

    /// #3918 — the field [`names_a_capture_off_x`] reads is
    /// the one `peer::video_info_payload` writes, read from that fn's source
    /// as compiled: a rename on either side fails here. The join tests build
    /// their payloads with the same [`CAPTURE_ARM`] ([`info_naming`]).
    #[test]
    fn the_capture_arm_field_is_read_as_its_producer_spells_it() {
        let producer = peer_fn("fn video_info_payload(");
        let field = format!(r#","{CAPTURE_ARM}":""#);
        assert_eq!(
            producer.matches(&field).count(),
            1,
            "`{field}` in video_info_payload: {producer}"
        );
    }

    /// #3918 — every capture arm, its wire spelling and the
    /// join fact, pinned. A new arm does not compile until it is decided
    /// ([`reads_x`]), a renamed X spelling shows, and whatever this build
    /// cannot read fails closed: a marked owner's pipeline then takes no
    /// joiner.
    #[test]
    fn every_capture_arm_is_decided_on_or_off_x_and_what_is_unknown_fails_closed() {
        use crate::capture::CaptureArm;
        for arm in EVERY_ARM {
            assert_eq!(CaptureArm::from_wire(arm.wire()), Some(arm));
            assert_eq!(arm.reads_an_x_server(), reads_x(arm), "{arm:?}");
            assert_eq!(
                names_a_capture_off_x(&info_naming(arm.wire())),
                !reads_x(arm),
                "{arm:?}"
            );
        }
        assert_eq!(CaptureArm::X11.wire(), "x11");
        for payload in [
            info_naming("wayland-next"),
            format!(r#"{{"t":"rc:video-info","{CAPTURE_ARM}":7}}"#),
            r#"{"t":"rc:video-info","codec":"vp9"}"#.to_string(),
            "not json".to_string(),
            String::new(),
        ] {
            assert!(!names_a_capture_off_x(&payload), "{payload}");
        }
    }

    /// #3918 — through `try_join` itself: a marked owner's
    /// pipeline takes a joiner only while the capture its owner published is
    /// known not to read an X server. Every arm ([`EVERY_ARM`]) joins as
    /// [`reads_x`] decides it. Nothing published, a spelling this build does
    /// not know, a republish as `x11`, a capture with no pixels and the start
    /// of a reopen (the shared hook's builder, with the notices on or off)
    /// all refuse; a republished arm off X joins again. An owner with no mark
    /// takes joiners whatever its arm.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[tokio::test]
    async fn a_marked_owners_pipeline_takes_joiners_only_with_a_capture_known_off_x() {
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3918-EVERY-ARM");
        let info = info_naming;
        let down = crate::capture::CaptureUnavailable::virtual_desktop_down(
            "this device's virtual desktop is down: its X server (Xvfb) exited",
        );
        let reopen = |p: &Pipeline, notices: bool| {
            let no_dc = Arc::new(tokio::sync::Mutex::new(None));
            drop(crate::peer::shared_media_waiting_hook_as(
                ObjectId::new(),
                no_dc,
                p,
                notices,
            ));
        };
        // One case: a fresh pipeline of `owner`, `then` run on it, one joiner.
        let joins = |owner: ObjectId, then: &dyn Fn(&Pipeline)| {
            let p = Pipeline::register(key, owner);
            then(&p);
            let joined = try_join(key, sink(ObjectId::new()), 60);
            let admitted = joined.is_some();
            drop((joined, p));
            admitted
        };
        let marked = ObjectId::new();
        crate::input::mark_no_picture(marked);
        // Every arm, as its capturer publishes it.
        let by_arm: Vec<(&str, bool)> = EVERY_ARM
            .iter()
            .map(|arm| {
                let wire = arm.wire();
                (wire, joins(marked, &|p| p.publish_video_info(info(wire))))
            })
            .collect();
        let cases = [
            ("nothing published", joins(marked, &|_| {})),
            (
                "an unknown spelling",
                joins(marked, &|p| p.publish_video_info(info("wayland-next"))),
            ),
            (
                "portal, then x11",
                joins(marked, &|p| {
                    p.publish_video_info(info("portal"));
                    p.publish_video_info(info("x11"));
                }),
            ),
            (
                "portal, then no pixels",
                joins(marked, &|p| {
                    p.publish_video_info(info("portal"));
                    p.tell_unavailable(&down);
                }),
            ),
            (
                "portal, then a reopen starts",
                joins(marked, &|p| {
                    p.publish_video_info(info("portal"));
                    reopen(p, true);
                }),
            ),
            (
                "portal, then a reopen starts with the notices off",
                joins(marked, &|p| {
                    p.publish_video_info(info("portal"));
                    reopen(p, false);
                }),
            ),
            (
                "portal, a reopen, portal again",
                joins(marked, &|p| {
                    p.publish_video_info(info("portal"));
                    reopen(p, true);
                    p.publish_video_info(info("portal"));
                }),
            ),
            (
                "x11 under an owner with no mark",
                joins(ObjectId::new(), &|p| p.publish_video_info(info("x11"))),
            ),
        ];
        crate::input::x_follow::no_picture::forget(marked);
        let decided: Vec<(&str, bool)> = EVERY_ARM
            .iter()
            .map(|&arm| (arm.wire(), !reads_x(arm)))
            .collect();
        assert_eq!(by_arm, decided, "a marked owner's pipeline, by its arm");
        assert_eq!(
            cases,
            [
                ("nothing published", false),
                ("an unknown spelling", false),
                ("portal, then x11", false),
                ("portal, then no pixels", false),
                ("portal, then a reopen starts", false),
                ("portal, then a reopen starts with the notices off", false),
                ("portal, a reopen, portal again", true),
                ("x11 under an owner with no mark", true),
            ]
        );
    }

    /// #3918 — `ROOMLERD_INPUT_X11_FOLLOW=0`, the
    /// operator's way back, through the join itself: a marked owner's
    /// pipeline with nothing published of its capture takes a joiner as on
    /// master; with the switch on it takes none. The switch is set on this
    /// thread only (`x_follow::test_seam::follow`), and `try_join` reads it
    /// as production does, through `input::has_no_picture`.
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    #[tokio::test]
    async fn with_the_switch_off_a_marked_owners_pipeline_takes_a_joiner() {
        use crate::input::x_follow::{no_picture, test_seam};
        let _s = serial();
        let key = PipelineKey::FfmpegDc("TEST-3918-SWITCH");
        let marked = ObjectId::new();
        crate::input::mark_no_picture(marked);
        let joins = |on: bool| {
            let _switch = test_seam::follow(on);
            let p = Pipeline::register(key, marked);
            let joined = try_join(key, sink(ObjectId::new()), 60);
            let admitted = joined.is_some();
            drop((joined, p));
            admitted
        };
        let off = joins(false);
        let on = joins(true);
        no_picture::forget(marked);
        assert!(
            off,
            "with the switch off a marked owner's pipeline took no joiner"
        );
        assert!(
            !on,
            "with the switch on a marked owner's pipeline with nothing published took a joiner"
        );
    }

    /// #3671 — a retire touches only what was registered when it ran: it
    /// takes the pipeline registered before it (closed to joins, its entry
    /// removed), a pipeline registered after it takes joiners, and a second
    /// retire with nothing registered in between retires nothing. The first
    /// count is "at least one", because another test's pipeline can still be
    /// registered as this one starts and is retired with it; `before`'s own
    /// state is what shows that the retire took it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_retire_leaves_what_registers_after_it_and_a_second_one_retires_nothing() {
        let _s = serial();
        let before_key = PipelineKey::FfmpegDc("TEST-3671-BEFORE");
        let before = Pipeline::register(before_key, ObjectId::new());
        assert!(retire_all() >= 1, "the first retire found no pipeline");
        assert!(
            before.inner.lock().unwrap().closed,
            "the first retire left the pipeline registered before it open to joins"
        );
        assert!(
            !registry().lock().unwrap().contains_key(&before_key),
            "the first retire left the entry of the pipeline registered before it"
        );
        assert_eq!(retire_all(), 0, "a second retire found a pipeline");
        let key = PipelineKey::FfmpegDc("TEST-3671-AFTER");
        let after = Pipeline::register(key, ObjectId::new());
        let joined = try_join(key, sink(ObjectId::new()), 60)
            .expect("a pipeline registered after the retire takes joiners");
        assert!(!after.inner.lock().unwrap().closed);
        assert_eq!(after.follower_count(), 1);
        drop((joined, after, before));
    }
}
