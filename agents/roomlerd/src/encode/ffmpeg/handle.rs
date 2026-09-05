// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-70 M1 — one handle, two homes for the FFmpeg encoder.
//!
//! The pump holds an [`EncoderHandle`] and calls the same nine methods it
//! always did; whether the encoder lives inline (today's `block_in_place` on
//! a runtime worker) or on its own thread ([`crate::encode::thread`]) is a
//! constructor choice made once per open from the `media_thread` switch.
//! That is what keeps the kill switch honest: one call site per operation,
//! no second loop, `off` = the shipped path verbatim.

use std::sync::Arc;

use anyhow::Result;

use super::encoder::{FfmpegEncoder, RateStats, RebuildSpec, RebuiltEncoder};
use crate::capture::Frame;
use crate::encode::EncodedPacket;
use crate::encode::thread::{EncoderCaps, EncoderOps, EncoderThread};

impl EncoderOps for FfmpegEncoder {
    type Rebuilt = RebuiltEncoder;
    type RebuildSpec = RebuildSpec;
    type Stats = RateStats;

    fn encode_sync(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>> {
        FfmpegEncoder::encode_sync(self, frame)
    }
    fn set_bitrate(&mut self, bps: u32) {
        crate::encode::VideoEncoder::set_bitrate(self, bps)
    }
    fn request_keyframe(&mut self) {
        crate::encode::VideoEncoder::request_keyframe(self)
    }
    fn adopt_rebuilt(&mut self, rebuilt: RebuiltEncoder) -> bool {
        FfmpegEncoder::adopt_rebuilt(self, rebuilt)
    }
    fn rebuild_spec(&self, bps: u32) -> Option<RebuildSpec> {
        FfmpegEncoder::rebuild_spec(self, bps)
    }
    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            name: crate::encode::VideoEncoder::name(self),
            supports_dynamic_bitrate: self.supports_dynamic_bitrate(),
            reconfig_forces_idr: self.reconfig_forces_idr(),
            chroma444: self.chroma444(),
        }
    }
    fn current_maxrate_bps(&self) -> u32 {
        FfmpegEncoder::current_maxrate_bps(self)
    }
    fn rate_stats(&self) -> RateStats {
        FfmpegEncoder::rate_stats(self)
    }
}

/// The FFmpeg encoder as the pump sees it.
pub enum EncoderHandle {
    /// Today's path: the encoder is a local of the pump, encodes under
    /// `block_in_place` on whichever worker polls the pump.
    Inline(FfmpegEncoder),
    /// FR-70 M1: the encoder lives on `rc-enc-<session>`; every call below
    /// is a message and an awaited reply.
    Threaded(EncoderThread<FfmpegEncoder>),
}

impl EncoderHandle {
    /// `threaded` = the `media_thread` switch. A thread that cannot be spawned
    /// (resource exhaustion) falls back to the inline path with a warning
    /// rather than failing the open — the switch must never cost a session.
    pub fn new(enc: FfmpegEncoder, threaded: bool, label: &str) -> Self {
        if !threaded {
            return Self::Inline(enc);
        }
        // A failed spawn hands the encoder back, so the open is not lost.
        match EncoderThread::spawn(enc, label) {
            Ok(t) => Self::Threaded(t),
            Err((enc, e)) => {
                tracing::warn!(%e, "FR-70 M1: encoder thread unavailable — encoding inline");
                Self::Inline(enc)
            }
        }
    }

    pub fn is_threaded(&self) -> bool {
        matches!(self, Self::Threaded(_))
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Inline(e) => crate::encode::VideoEncoder::name(e),
            Self::Threaded(t) => t.caps().name,
        }
    }

    pub fn supports_dynamic_bitrate(&self) -> bool {
        match self {
            Self::Inline(e) => e.supports_dynamic_bitrate(),
            Self::Threaded(t) => t.caps().supports_dynamic_bitrate,
        }
    }

    pub fn reconfig_forces_idr(&self) -> bool {
        match self {
            Self::Inline(e) => e.reconfig_forces_idr(),
            Self::Threaded(t) => t.caps().reconfig_forces_idr,
        }
    }

    pub fn chroma444(&self) -> bool {
        match self {
            Self::Inline(e) => e.chroma444(),
            Self::Threaded(t) => t.caps().chroma444,
        }
    }

    pub fn current_maxrate_bps(&self) -> u32 {
        match self {
            Self::Inline(e) => e.current_maxrate_bps(),
            Self::Threaded(t) => t.current_maxrate_bps(),
        }
    }

    /// The encode. Inline: `block_in_place`, exactly as shipped (the
    /// multi-thread runtime only, which the agent always runs). Threaded: a
    /// message and the awaited reply; the worker is free meanwhile.
    pub async fn encode(&mut self, frame: &Arc<Frame>) -> Result<Vec<EncodedPacket>> {
        match self {
            Self::Inline(e) => tokio::task::block_in_place(|| e.encode_sync(frame)),
            Self::Threaded(t) => t.encode(frame.clone()).await,
        }
    }

    /// Applied when the await returns, on both paths. A dead thread is logged
    /// here and surfaces as the next encode's error, which the pump's ladder
    /// already turns into a rebuild.
    pub async fn set_bitrate(&mut self, bps: u32) {
        match self {
            Self::Inline(e) => crate::encode::VideoEncoder::set_bitrate(e, bps),
            Self::Threaded(t) => {
                if let Err(e) = t.set_bitrate(bps).await {
                    tracing::warn!(%e, bps, "FR-70 M1: set_bitrate not applied");
                }
            }
        }
    }

    pub async fn request_keyframe(&mut self) {
        match self {
            Self::Inline(e) => crate::encode::VideoEncoder::request_keyframe(e),
            Self::Threaded(t) => {
                if let Err(e) = t.request_keyframe().await {
                    tracing::warn!(%e, "FR-70 M1: keyframe request not applied");
                }
            }
        }
    }

    /// `false` on a refused adoption AND on a dead thread — either way the
    /// rebuilt encoder is dropped and the current one keeps serving.
    pub async fn adopt_rebuilt(&mut self, rebuilt: RebuiltEncoder) -> bool {
        match self {
            Self::Inline(e) => e.adopt_rebuilt(rebuilt),
            Self::Threaded(t) => t.adopt_rebuilt(rebuilt).await.unwrap_or_else(|e| {
                tracing::warn!(%e, "FR-70 M1: adoption not applied");
                false
            }),
        }
    }

    pub async fn rebuild_spec(&mut self, bps: u32) -> Option<RebuildSpec> {
        match self {
            Self::Inline(e) => e.rebuild_spec(bps),
            Self::Threaded(t) => t.rebuild_spec(bps).await.unwrap_or_else(|e| {
                tracing::warn!(%e, "FR-70 M1: rebuild spec unavailable");
                None
            }),
        }
    }

    pub async fn rate_stats(&mut self) -> RateStats {
        match self {
            Self::Inline(e) => e.rate_stats(),
            Self::Threaded(t) => t.rate_stats().await.unwrap_or_else(|e| {
                tracing::warn!(%e, "FR-70 M1: rate stats unavailable");
                RateStats::default()
            }),
        }
    }
}
