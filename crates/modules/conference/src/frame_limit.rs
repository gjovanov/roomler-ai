// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A per-socket limit on `media:*` frames: one token bucket per connection,
//! `[meetings] media_frame_burst` frames deep, refilled at
//! `media_frame_rate` frames a second.
//!
//! Every `media:*` frame costs the pod work on the caller's behalf: a join
//! reads the database and builds two transports, a consume builds a consumer
//! on the worker, and a produce, a pause or a playback is announced to
//! everyone in the call. A socket's frames are handled one at a time, so a
//! socket that sends them as fast as it can spends that work back to back.
//! The bucket holds it to the rate after the burst.
//!
//! **Where it is counted.** On the pod that holds the socket, as the frame
//! arrives ([`crate::ws_media::Media`]), before placement: a frame forwarded
//! to the call's owner pod was counted where it came in, once.
//!
//! **How deep.** The deepest legitimate burst is a join into a large meeting:
//! the join, one `media:consume` for every producer already in the call, sent
//! back to back as fast as the answers come (`consumeChain`,
//! `ui/src/stores/conference.ts`), both transports' connects, the client's own
//! produces and the view reports its tiles trigger. `conference_rate_tests`
//! measures it in a meeting of Business's 100 people, each with a camera and
//! a microphone, and checks the default holds it.
//!
//! **What an over-limit frame gets.** It is not run. The socket hears
//! `media:error` with the stable code [`RATE_LIMITED`] and `retry_after_ms`,
//! the time until the bucket holds a frame again, and stays open: a client
//! that overruns loses that frame, never its socket or its call. Each refusal
//! counts in `media_frames_rate_limited_total` (`/api/cluster/status`), the
//! first one of a socket is logged, and so is the socket's total when it
//! closes.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use roomler_core::WsCtx;
use tracing::{info, warn};

use crate::ConferenceState;

/// The `code` of the `media:error` an over-limit frame gets.
pub const RATE_LIMITED: &str = "rate_limited";

/// Count one `media:*` frame of `ctx`'s socket against its budget. `true`
/// when it was over the limit: the socket has been answered, the refusal
/// counted, and the frame must not run.
pub(crate) async fn refused(state: &ConferenceState, ctx: &WsCtx, msg_type: &str) -> bool {
    let Verdict::Refused { retry_after, first } = state.media_frames.admit(&ctx.connection_id)
    else {
        return false;
    };
    roomler_core::cluster::metrics::bump(
        &roomler_core::cluster::metrics::MEDIA_FRAMES_RATE_LIMITED_TOTAL,
    );
    if first {
        warn!(
            user_id = ?ctx.principal,
            connection_id = %ctx.connection_id,
            msg_type,
            "media frames over the socket's limit; refusing until it slows down"
        );
    }
    let msg = serde_json::json!({
        "type": "media:error",
        "data": {
            "message": "Too many media messages; slow down",
            "code": RATE_LIMITED,
            "retry_after_ms": u64::try_from(retry_after.as_nanos().div_ceil(1_000_000))
                .unwrap_or(u64::MAX),
        }
    });
    roomler_core::ws::dispatcher::send_to_connection_routed(
        &state.ws_storage,
        &state.redis_pubsub,
        &ctx.connection_id,
        &msg,
    )
    .await;
    true
}

/// The socket closed: drop its bucket, and log how many of its frames were
/// refused, when any were.
pub(crate) fn socket_closed(state: &ConferenceState, ctx: &WsCtx) {
    let refused = state.media_frames.forget(&ctx.connection_id);
    if refused > 0 {
        info!(
            user_id = ?ctx.principal,
            connection_id = %ctx.connection_id,
            refused,
            "socket closed; media frames refused over its limit"
        );
    }
}

/// One socket's bucket.
#[derive(Debug)]
struct Bucket {
    /// Frames it may send now; fractional between refills.
    tokens: f64,
    /// When `tokens` was last brought up to date.
    at: Instant,
    /// Frames refused over the socket's life.
    refused: u64,
}

/// What [`MediaFrameLimiter::admit`] decided for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Run it.
    Run,
    /// Over the limit: answer it, and run nothing.
    Refused {
        /// How long until the bucket holds a frame again.
        retry_after: Duration,
        /// The socket's first refused frame, which is the one logged.
        first: bool,
    },
}

/// The buckets of this pod's sockets, by connection id.
#[derive(Debug)]
pub struct MediaFrameLimiter {
    burst: u32,
    rate: u32,
    sockets: DashMap<String, Bucket>,
}

impl MediaFrameLimiter {
    /// `burst` frames at once, refilled at `rate` a second. Either at `0`
    /// turns the limit off.
    pub fn new(burst: u32, rate: u32) -> Self {
        Self {
            burst,
            rate,
            sockets: DashMap::new(),
        }
    }

    pub fn from_settings(meetings: &roomler_ai_config::MeetingsSettings) -> Self {
        Self::new(meetings.media_frame_burst, meetings.media_frame_rate)
    }

    /// The limit is switched off (`media_frame_burst` or `media_frame_rate`
    /// is `0`): every frame runs and nothing is kept per socket.
    pub fn is_off(&self) -> bool {
        self.burst == 0 || self.rate == 0
    }

    /// One frame from `connection_id`, now.
    pub fn admit(&self, connection_id: &str) -> Verdict {
        self.admit_at(connection_id, Instant::now())
    }

    fn admit_at(&self, connection_id: &str, now: Instant) -> Verdict {
        if self.is_off() {
            return Verdict::Run;
        }
        let (burst, rate) = (f64::from(self.burst), f64::from(self.rate));
        // A plain entry, released before the caller awaits anything.
        let mut bucket = self
            .sockets
            .entry(connection_id.to_string())
            .or_insert_with(|| Bucket {
                tokens: burst,
                at: now,
                refused: 0,
            });
        let elapsed = now.saturating_duration_since(bucket.at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(burst);
        // Only ever forward: an instant from before the last update must not
        // rewind the clock and be refilled a second time.
        bucket.at = bucket.at.max(now);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Verdict::Run;
        }
        bucket.refused += 1;
        Verdict::Refused {
            retry_after: Duration::from_secs_f64((1.0 - bucket.tokens) / rate),
            first: bucket.refused == 1,
        }
    }

    /// The socket closed: drop its bucket. Returns how many of its frames
    /// were refused.
    pub fn forget(&self, connection_id: &str) -> u64 {
        self.sockets
            .remove(connection_id)
            .map(|(_, bucket)| bucket.refused)
            .unwrap_or(0)
    }

    /// How many sockets hold a bucket on this pod.
    pub fn sockets(&self) -> usize {
        self.sockets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(v: Verdict) -> bool {
        matches!(v, Verdict::Refused { .. })
    }

    #[test]
    fn a_socket_sends_its_burst_at_once_and_the_next_frame_is_refused() {
        let limiter = MediaFrameLimiter::new(5, 2);
        let t0 = Instant::now();
        for i in 0..5 {
            assert_eq!(limiter.admit_at("a", t0), Verdict::Run, "frame {i}");
        }
        match limiter.admit_at("a", t0) {
            Verdict::Refused { retry_after, first } => {
                assert!(first, "the socket's first refusal is the one logged");
                // An empty bucket at two a second holds a frame again in 500 ms.
                assert_eq!(retry_after, Duration::from_millis(500));
            }
            v => panic!("the sixth frame ran: {v:?}"),
        }
        match limiter.admit_at("a", t0) {
            Verdict::Refused { first, .. } => assert!(!first, "logged once per socket"),
            v => panic!("the seventh frame ran: {v:?}"),
        }
    }

    #[test]
    fn the_bucket_refills_at_the_rate_and_never_past_the_burst() {
        let limiter = MediaFrameLimiter::new(4, 10);
        let t0 = Instant::now();
        for _ in 0..4 {
            assert_eq!(limiter.admit_at("a", t0), Verdict::Run);
        }
        assert!(refused(limiter.admit_at("a", t0)));
        // 100 ms at ten a second is one frame, and only one.
        let t1 = t0 + Duration::from_millis(100);
        assert_eq!(limiter.admit_at("a", t1), Verdict::Run);
        assert!(refused(limiter.admit_at("a", t1)));
        // An hour idle fills the bucket to its depth, no further.
        let t2 = t1 + Duration::from_secs(3600);
        for _ in 0..4 {
            assert_eq!(limiter.admit_at("a", t2), Verdict::Run);
        }
        assert!(refused(limiter.admit_at("a", t2)));
    }

    #[test]
    fn each_socket_has_its_own_bucket() {
        let limiter = MediaFrameLimiter::new(1, 1);
        let t0 = Instant::now();
        assert_eq!(limiter.admit_at("a", t0), Verdict::Run);
        assert!(refused(limiter.admit_at("a", t0)));
        assert_eq!(
            limiter.admit_at("b", t0),
            Verdict::Run,
            "another socket of the same person is not held back"
        );
        assert_eq!(limiter.sockets(), 2);
    }

    #[test]
    fn a_closed_socket_reports_its_refusals_and_leaves_nothing_behind() {
        let limiter = MediaFrameLimiter::new(1, 1);
        let t0 = Instant::now();
        limiter.admit_at("a", t0);
        limiter.admit_at("a", t0);
        limiter.admit_at("a", t0);
        assert_eq!(limiter.forget("a"), 2);
        assert_eq!(limiter.sockets(), 0);
        assert_eq!(limiter.forget("a"), 0, "a second close finds nothing");
        // A new socket with the same id starts full.
        assert_eq!(limiter.admit_at("a", t0), Verdict::Run);
    }

    #[test]
    fn zero_turns_the_limit_off() {
        for (burst, rate) in [(0, 20), (400, 0), (0, 0)] {
            let limiter = MediaFrameLimiter::new(burst, rate);
            assert!(limiter.is_off());
            let t0 = Instant::now();
            for _ in 0..10_000 {
                assert_eq!(limiter.admit_at("a", t0), Verdict::Run);
            }
            assert_eq!(limiter.sockets(), 0, "an off limit keeps no buckets");
        }
    }

    /// An instant from before the bucket's last update (two frames racing
    /// their clock reads) refills nothing, and does not rewind the bucket so
    /// that the next frame is refilled twice.
    #[test]
    fn an_earlier_instant_refills_nothing() {
        let limiter = MediaFrameLimiter::new(2, 1);
        let t0 = Instant::now() + Duration::from_secs(10);
        assert_eq!(limiter.admit_at("a", t0), Verdict::Run);
        assert_eq!(limiter.admit_at("a", t0), Verdict::Run);
        assert!(refused(limiter.admit_at("a", t0 - Duration::from_secs(5))));
        assert!(
            refused(limiter.admit_at("a", t0)),
            "the step back bought five seconds of frames"
        );
    }

    #[test]
    fn the_shipped_default_holds_the_measured_join_burst() {
        let d = roomler_ai_config::MeetingsSettings::default();
        let limiter = MediaFrameLimiter::from_settings(&d);
        assert!(!limiter.is_off(), "the limit ships on");
        // `conference_rate_tests` measures the burst against these numbers;
        // this pins them so a change to either is a reviewed line here too.
        assert_eq!((d.media_frame_burst, d.media_frame_rate), (400, 20));
    }
}
