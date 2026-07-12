//! Desktop / system audio capture → Opus for the WebRTC audio track.
//!
//! Opt-in per session (`ServerMsg::Request.audio_enabled`, threaded from
//! the controller's `rc:session.request`). When enabled AND the agent was
//! built with the `audio` Cargo feature, `peer.rs` adds a second
//! sendonly WebRTC track (audio/opus, PT 111) and spawns `audio_pump`,
//! which drives [`AudioCapture`] → [`opus_encode::OpusEncoder`] → 20 ms
//! Opus packets → `TrackLocalStaticSample::write_sample`.
//!
//! Capture backend ([`open_default`]):
//!   * **Linux** — PulseAudio *monitor* source (the loopback of the
//!     default sink), via cpal. Under WSLg `PULSE_SERVER` must point at
//!     `/mnt/wslg/PulseServer` — see `virtual_desktop.rs`.
//!   * **Windows** — WASAPI loopback: cpal opens the default OUTPUT
//!     device with `build_input_stream` (cpal 0.15 special-cases an
//!     output device driven as an input into a loopback capture).
//!   * **macOS / everything else** — [`NoopAudioCapture`] (real
//!     ScreenCaptureKit system-audio is a later phase).
//!
//! The whole module is gated behind `#[cfg(feature = "audio")]` at the
//! `lib.rs` `pub mod audio;` site, so default / signalling-only builds
//! never pull in cpal or audiopus.

use async_trait::async_trait;

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod cpal_backend;
pub mod opus_encode;

/// One buffer of interleaved PCM samples straight off the capture
/// backend, BEFORE resample / upmix. `channels` and `sample_rate`
/// describe THIS frame's format (the capture device's native format);
/// the [`opus_encode::OpusEncoder`] resamples/upmixes to 48 kHz stereo
/// as needed.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Interleaved samples: `[L0, R0, L1, R1, …]` for stereo,
    /// `[S0, S1, …]` for mono. Length is a multiple of `channels`.
    pub samples: Vec<i16>,
    pub channels: u16,
    pub sample_rate: u32,
}

/// Pull-based capture source. `next_frame` yields the next available
/// buffer, `Ok(None)` when the source is permanently exhausted (or a
/// Noop that never produces), and `Err` on a hard backend failure.
///
/// `Send` (not `Sync`) — the pump owns it exclusively on one task.
#[async_trait]
pub trait AudioCapture: Send {
    async fn next_frame(&mut self) -> anyhow::Result<Option<AudioFrame>>;
}

/// Capture backend that never produces audio. Used on macOS (until
/// ScreenCaptureKit lands) and as the fallback when the cpal backend
/// fails to open a device. `next_frame` parks so the pump task idles
/// cheaply instead of busy-looping on `Ok(None)`.
pub struct NoopAudioCapture;

#[async_trait]
impl AudioCapture for NoopAudioCapture {
    async fn next_frame(&mut self) -> anyhow::Result<Option<AudioFrame>> {
        // Park indefinitely rather than return Ok(None) immediately —
        // a hot Ok(None) loop in the pump would spin a core. The pump's
        // JoinHandle is aborted on `AgentPeer::close()`, which cancels
        // this future.
        std::future::pending::<()>().await;
        Ok(None)
    }
}

/// Open the best available system-audio capture for this platform.
/// Never fails: on any backend error (or on macOS / unsupported
/// targets) it logs and returns a [`NoopAudioCapture`] so the audio
/// pump degrades to silence rather than tearing down the session.
pub fn open_default() -> Box<dyn AudioCapture> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        match cpal_backend::CpalLoopbackCapture::open() {
            Ok(cap) => {
                tracing::info!("audio: opened cpal loopback capture");
                return Box::new(cap);
            }
            Err(e) => {
                tracing::warn!(%e, "audio: cpal loopback capture failed to open — falling back to silence");
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        tracing::info!(
            "audio: no system-audio capture backend on this platform (macOS ScreenCaptureKit is a later phase) — using silent capture"
        );
    }
    Box::new(NoopAudioCapture)
}
