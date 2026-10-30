// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Windows Media Foundation hardware H.264 encoder backend.
//!
//! The MFT-backed alternative to [`openh264_backend`]. Wraps
//! `CLSID_MSH264EncoderMFT` (the Microsoft-shipped H264 Encoder MFT,
//! which ships the hardware fast path on every IHV driver since
//! Windows 8.1) in a pinned-worker-thread wrapper that matches the
//! VideoEncoder trait.
//!
//! Layering:
//!
//! ```text
//! peer.rs media_pump
//!       │  Arc<Frame>  (BGRA, variable dims)
//!       ▼
//! MfEncoder::encode
//!       │  mpsc::Cmd → worker thread
//!       ▼
//! worker:
//!       │  color::bgra_to_nv12()  (software — phase 1)
//!       │  IMFSample + IMFMediaBuffer (NV12, timestamped)
//!       │  ProcessInput
//!       │  ProcessOutput (drain until NEED_MORE_INPUT)
//!       │  NALU extraction from output sample
//!       ▼  Vec<EncodedPacket>
//! ```
//!
//! Module layout (Phase 3 commit 3 — probe-and-rollback cascade):
//!
//! - `mod.rs` — thread-pinned `MfEncoder` handle + Cmd loop; COM/MF
//!   lifecycle (CoInitializeEx / MFStartup / MFShutdown /
//!   CoUninitialize); shared `create_d3d11_device_and_manager`
//!   helper (default-adapter path).
//! - `adapter.rs` — DXGI adapter enumeration + adapter-scoped D3D11
//!   device creation (Phase 3 commit 2).
//! - `activate.rs` — adapter × MFT cascade with probe-and-rollback.
//!   Replaces the monolithic `activate_h264_encoder` helper.
//! - `probe.rs` — single-frame probe harness used by the cascade to
//!   verify an activated MFT actually emits bytes.
//! - `sync_pipeline.rs` — `MfPipeline`: synchronous MFT pipeline
//!   (media-type setup, ProcessInput + drain loop, codec-api tuning
//!   knobs, Annex-B NALU extraction). Assumes pre-activated MFT.
//!
//! Latency knobs applied via `ICodecAPI` (all off by default,
//! every one a must-set for sub-100 ms interactive streaming):
//!
//! * `AVLowLatencyMode = true`
//! * `AVEncCommonRateControlMode = CBR`
//! * `AVEncCommonMeanBitRate = <resolution-scaled>`
//! * `AVEncMPVGOPSize = 60` (IDR every ~2 s at 30 fps)
//! * `AVEncH264CABACEnable = true` (better quality at our bitrate)

#![cfg(all(target_os = "windows", feature = "mf-encoder"))]

use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread;

use anyhow::{Result, anyhow, bail};
use tokio::sync::oneshot;

use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIDeviceManager, MFCreateDXGIDeviceManager, MFSTARTUP_FULL, MFShutdown, MFStartup,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::Interface;

use super::{EncodedPacket, VideoEncoder};
use crate::capture::{Frame, PixelFormat};

mod activate;
mod adapter;
mod probe;
mod sync_pipeline;

/// Cheap availability check used by capability detection (2A.1).
/// Returns the number of HW H.264 MFTs installed on this host as
/// reported by MFTEnumEx. Doesn't activate them or run the cascade
/// — that's the heavy operation kept inside MfEncoder::new. 0 means
/// no HW encoder; capability reporting falls back to SW only.
pub(crate) fn probe_adapter_count() -> Result<usize> {
    activate::enumerate_hw_h264_mfts().map(|v| v.len())
}

/// Same as [`probe_adapter_count`] but for HEVC encoders. Lights up
/// the H.265 chip in AgentsSection when HW HEVC is available.
pub(crate) fn probe_hevc_adapter_count() -> Result<usize> {
    activate::enumerate_hw_hevc_mfts().map(|v| v.len())
}

/// Same as [`probe_adapter_count`] but for AV1 encoders. Lights up
/// the AV1 chip in AgentsSection when HW AV1 is available. Expected
/// 0 on most boxes today; AV1 HW encode ships on Windows 11 24H2+ with
/// RTX 40+ / Intel Arc / RDNA3+ drivers.
pub(crate) fn probe_av1_adapter_count() -> Result<usize> {
    activate::enumerate_hw_av1_mfts().map(|v| v.len())
}

use sync_pipeline::{MfPipeline, OutputCodec};

/// Public-facing handle. Owns only the command channel; the IMFTransform
/// and every other COM interface live on the pinned worker thread.
pub struct MfEncoder {
    cmd_tx: std_mpsc::Sender<Cmd>,
    width: u32,
    height: u32,
    codec: OutputCodec,
    /// "hw" when the cascade winner was a dedicated HW MFT with a
    /// live D3D11 manager, "sw" when it fell through to
    /// `CLSID_MSH264EncoderMFT` / `HEVCVideoExtensionEncoder`, and
    /// [`BACKEND_PURE_SW`] when that fallback also ignored the D3D
    /// manager. Sent up from the worker through the ready channel at
    /// startup. Consulted by `VideoEncoder::is_hardware`; the peer media
    /// pump reads that to trigger auto-downscale on SW HEVC at 4K.
    backend_kind: &'static str,
}

/// #2283 — the backend kind of the default-adapter software MFT when it
/// also ignored the D3D manager: Microsoft's encoder with no GPU behind it,
/// as on a VM whose only adapter is the Basic Render Driver. On the RTP
/// track it held frames for ~850 ms of input to photon where OpenH264 took
/// 37 ms on the same host, so Auto does not keep it
/// ([`MfEncoder::is_pure_software`]). Rate control reads it as `"sw"`.
pub(crate) const BACKEND_PURE_SW: &str = "sw-pure";

enum Cmd {
    Encode {
        frame: Arc<Frame>,
        reply: oneshot::Sender<Result<Vec<EncodedPacket>>>,
    },
    RequestKeyframe,
    SetBitrate(u32),
    Shutdown,
}

impl MfEncoder {
    /// Build an MF H.264 encoder for a given output resolution.
    /// Convenience wrapper around [`MfEncoder::new_h264`]; pre-existing
    /// call sites in the agent use this.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        Self::new_h264(width, height)
    }

    /// H.264 entry point. Preserves the original probe-and-rollback
    /// cascade behaviour; falls back to the SW `CLSID_MSH264EncoderMFT`
    /// when every HW candidate fails.
    pub fn new_h264(width: u32, height: u32) -> Result<Self> {
        Self::new_internal(width, height, OutputCodec::H264)
    }

    /// HEVC entry point. Walks the same DXGI adapter × HW MFT cascade
    /// but filters for `CLSID_MSH265EncoderMFT` / IHV HEVC encoders.
    /// Returns Err when no HW HEVC path succeeds — Windows ships no
    /// software HEVC encoder so the caller must demote to H.264.
    pub fn new_hevc(width: u32, height: u32) -> Result<Self> {
        Self::new_internal(width, height, OutputCodec::Hevc)
    }

    /// AV1 entry point. HW-only like HEVC; returns Err on cascade
    /// exhaustion so the caller demotes. Expected to fail on most
    /// boxes today — AV1 HW encoders ship on Windows 11 24H2+ with
    /// RTX 40+ / Intel Arc / RDNA3+. On the RTX 5090 Laptop dev box
    /// the NVIDIA AV1 MFT is likely to hit the same Blackwell
    /// `ActivateObject 0x8000FFFF` regression as H.264/HEVC and skip
    /// through to the next candidate.
    pub fn new_av1(width: u32, height: u32) -> Result<Self> {
        Self::new_internal(width, height, OutputCodec::Av1)
    }

    fn new_internal(width: u32, height: u32, codec: OutputCodec) -> Result<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("mf-encoder: require non-zero, even dimensions, got {width}x{height}");
        }

        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<&'static str>>();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<Cmd>();

        thread::Builder::new()
            .name(format!("roomlerd-mf-encoder-{}", codec.backend_name()))
            .spawn(move || {
                // 1. Initialise COM for this thread. MTA because MF is
                //    happy with it and we never touch UI.
                let coinit = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if coinit.is_err() {
                    let _ = ready_tx.send(Err(anyhow!("CoInitializeEx failed: {:?}", coinit)));
                    return;
                }

                // 2. Start Media Foundation.
                if let Err(e) = unsafe { MFStartup(windows_mf_version(), MFSTARTUP_FULL) } {
                    unsafe { CoUninitialize() };
                    let _ = ready_tx.send(Err(anyhow!("MFStartup failed: {e:?}")));
                    return;
                }

                // 3. Run the codec-parametric cascade.
                let pipeline =
                    match activate::activate_and_probe_pipeline_for_codec(codec, width, height) {
                        Ok(p) => p,
                        Err(e) => {
                            unsafe { MFShutdown().ok() };
                            unsafe { CoUninitialize() };
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };

                let backend_kind = pipeline.backend_kind();
                let _ = ready_tx.send(Ok(backend_kind));
                run_worker(pipeline, cmd_rx);

                unsafe { MFShutdown().ok() };
                unsafe { CoUninitialize() };
            })
            .map_err(|e| anyhow!("spawning mf worker: {e}"))?;

        let backend_kind = ready_rx
            .recv()
            .map_err(|e| anyhow!("mf worker ack: {e}"))??;

        Ok(Self {
            cmd_tx,
            width,
            height,
            codec,
            backend_kind,
        })
    }

    /// #2283 — did the cascade end on Microsoft's software MFT with no GPU
    /// behind it ([`BACKEND_PURE_SW`])? The same CLSID inside the hardware
    /// loop, or a fallback that took the D3D manager, may hand frames to a
    /// GPU and is not this.
    pub fn is_pure_software(&self) -> bool {
        self.backend_kind == BACKEND_PURE_SW
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
    }
}

#[async_trait::async_trait]
impl VideoEncoder for MfEncoder {
    async fn encode(&mut self, frame: Arc<Frame>) -> Result<Vec<EncodedPacket>> {
        if frame.pixel_format != PixelFormat::Bgra {
            bail!(
                "mf-encoder: expected BGRA input, got {:?}",
                frame.pixel_format
            );
        }
        if frame.width != self.width || frame.height != self.height {
            bail!(
                "mf-encoder: frame dim mismatch: configured {}x{}, got {}x{}",
                self.width,
                self.height,
                frame.width,
                frame.height
            );
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Encode {
                frame,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("mf worker gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("mf worker dropped reply"))?
    }

    fn request_keyframe(&mut self) {
        let _ = self.cmd_tx.send(Cmd::RequestKeyframe);
    }

    fn set_bitrate(&mut self, bps: u32) {
        let _ = self.cmd_tx.send(Cmd::SetBitrate(bps));
    }

    fn name(&self) -> &'static str {
        self.codec.backend_name()
    }

    fn is_hardware(&self) -> bool {
        self.backend_kind == "hw"
    }
}

// ---------------------------------------------------------------------
// Worker loop. Runs on the pinned encoder thread; all COM calls happen
// through `pipeline`.
// ---------------------------------------------------------------------

fn run_worker(mut pipeline: MfPipeline, cmd_rx: std_mpsc::Receiver<Cmd>) {
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Cmd::Encode { frame, reply } => {
                let result = pipeline.encode(&frame);
                let _ = reply.send(result);
            }
            Cmd::RequestKeyframe => {
                if let Err(e) = pipeline.force_keyframe() {
                    tracing::debug!(%e, "mf-encoder: force-keyframe failed");
                }
            }
            Cmd::SetBitrate(bps) => {
                if let Err(e) = pipeline.set_bitrate(bps) {
                    tracing::debug!(%e, "mf-encoder: set-bitrate failed");
                }
            }
            Cmd::Shutdown => break,
        }
    }
    // Best-effort flush on the way out.
    let _ = pipeline.end_stream();
}

// ---------------------------------------------------------------------
// Shared D3D11 helpers. Kept in mod.rs until Phase 3 commit 2 replaces
// the default-adapter path with DXGI adapter enumeration.
// ---------------------------------------------------------------------

/// Create a D3D11 device suitable for MF hardware-accelerated video
/// encoding, wrap it in an IMFDXGIDeviceManager, return both. The
/// device needs BGRA_SUPPORT (our input) + VIDEO_SUPPORT (HW video
/// codec access) + multithread protection (MF worker threads call
/// into the device outside our control).
///
/// Feature-level list starts at 11_1 and falls back through 11_0,
/// 10_1, 10_0. Anything below 10_0 can't run MF video encoders.
pub(super) fn create_d3d11_device_and_manager() -> Result<(ID3D11Device, IMFDXGIDeviceManager)> {
    unsafe {
        let feature_levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        let mut device: Option<ID3D11Device> = None;
        let mut actual_level = D3D_FEATURE_LEVEL_11_0;
        D3D11CreateDevice(
            None, // default adapter
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut actual_level),
            None, // no immediate context needed here
        )
        .map_err(|e| anyhow!("D3D11CreateDevice: {e:?}"))?;
        let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice: null device"))?;

        // Make the device multithread-protected: MF spins its own
        // worker threads that call methods on the D3D device
        // concurrently with ours; without this MF will hit undefined
        // behaviour under contention.
        let mt: ID3D11Multithread = device
            .cast()
            .map_err(|e| anyhow!("ID3D11Multithread cast: {e:?}"))?;
        // Returns the previous protection state as BOOL; we don't care.
        let _ = mt.SetMultithreadProtected(true);

        let mut reset_token: u32 = 0;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut reset_token, &mut mgr)
            .map_err(|e| anyhow!("MFCreateDXGIDeviceManager: {e:?}"))?;
        let mgr = mgr.ok_or_else(|| anyhow!("MFCreateDXGIDeviceManager: null"))?;
        mgr.ResetDevice(&device, reset_token)
            .map_err(|e| anyhow!("IMFDXGIDeviceManager::ResetDevice: {e:?}"))?;

        tracing::info!(
            level = actual_level.0,
            reset_token,
            "mf-encoder: D3D11 device + DXGI manager created"
        );
        Ok((device, mgr))
    }
}

/// MF version number passed to MFStartup. `MF_VERSION` is
/// `(MF_API_VERSION << 16) | MF_SDK_VERSION`; the `windows` crate
/// exposes the constants but not the composed value, so we compose it
/// here — this matches what the Microsoft headers produce.
const fn windows_mf_version() -> u32 {
    // From `mfapi.h`: MF_SDK_VERSION = 0x2, MF_API_VERSION = 0x70.
    0x0002_0070
}

/// FR-130 P1 — what each Media Foundation encoder on this host writes about
/// colour, read from its own output.
#[cfg(test)]
mod colour_tests {
    use super::activate::for_each_pipeline;
    use super::sync_pipeline::{MfPipeline, OutputCodec};
    use super::windows_mf_version;
    use crate::encode::bitstream_color::{self, Signalled, Vocabulary};
    use crate::encode::color_desc::DESCRIPTION;
    use tunnel_core::env::test_env::{Saved, set};
    use windows::Win32::Media::MediaFoundation::{MFSTARTUP_FULL, MFShutdown, MFStartup};
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

    /// Eight frames of the bars through the pipeline's own BGRA path, a
    /// keyframe forced halfway, and the parameter sets wherever this MFT puts
    /// them: in its samples, or on its output type.
    fn read(codec: OutputCodec, p: &mut MfPipeline) -> Result<(Signalled, &'static str), String> {
        let (w, h) = p.dims();
        let mut stream = Vec::new();
        for i in 0..8u64 {
            if i == 4 {
                p.force_keyframe()
                    .map_err(|e| format!("force a keyframe: {e:#}"))?;
            }
            for pkt in p
                .encode(&bitstream_color::bars(w, h, i))
                .map_err(|e| format!("encode: {e:#}"))?
            {
                stream.extend_from_slice(&pkt.data);
            }
        }
        let parse = |bytes: &[u8]| match codec {
            OutputCodec::H264 => bitstream_color::h264(bytes),
            OutputCodec::Hevc => bitstream_color::hevc(bytes),
            OutputCodec::Av1 => bitstream_color::av1(bytes),
        };
        if codec != OutputCodec::Av1 {
            let types = bitstream_color::nal_types(&stream, codec == OutputCodec::Hevc);
            eprintln!(
                "FR-130 MF: the samples carry {} bytes, NAL types {types:?}",
                stream.len()
            );
        }
        match parse(&stream) {
            Ok(s) => Ok((s, "in the samples")),
            Err(in_band) => match p.sequence_header() {
                Some(seq) => parse(&seq)
                    .map(|s| (s, "MF_MT_MPEG_SEQUENCE_HEADER"))
                    .map_err(|e| format!("samples: {in_band}; MF_MT_MPEG_SEQUENCE_HEADER: {e}")),
                None => Err(format!(
                    "samples ({} bytes): {in_band}; no MF_MT_MPEG_SEQUENCE_HEADER",
                    stream.len()
                )),
            },
        }
    }

    /// MFTs MEASURED to write no colour description whatever they are told
    /// (the media-type attributes on both types, and the encoder API's
    /// `CODECAPI_AVEncVideo{Input,Output}Color*`, tried and removed), with the
    /// evidence. For them the test holds the switch-on stream to "nothing",
    /// so it fails the day one starts describing (delete its entry).
    const WRITES_NO_DESCRIPTION: &[(&str, &str)] = &[
        (
            "CLSID_MSH264EncoderMFT",
            "Microsoft's H.264 encoder: its SPS rides MF_MT_MPEG_SEQUENCE_HEADER with no \
             video_signal_type. neo16, Windows 11, 2026-10-30",
        ),
        (
            "HEVCVideoExtensionEncoder",
            "Microsoft's HEVC Video Extension: VPS/SPS/PPS in band, the SPS VUI with no \
             video_signal_type. neo16 (on the RTX 5090 and the Radeon 610M), 2026-10-30",
        ),
    ];

    /// Every MF encoder this host opens (each adapter × hardware MFT, and
    /// Microsoft's software one) takes the colour attributes on its types
    /// and, with the switch on, writes `color_desc::DESCRIPTION` in MF's
    /// vocabulary (one BT.601 matrix, one BT.709 curve), except the MFTs
    /// measured to write none ([`WRITES_NO_DESCRIPTION`]), which must still
    /// write none. With the switch off every one writes no description. An
    /// MFT that refused the attributes fails here: the fallback keeps the
    /// host's encoder, this keeps the refusal visible.
    #[test]
    fn every_mf_encoder_here_describes_its_colour() {
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _switch = Saved::cleared("COLOR_DESCRIPTION");
        // SAFETY: COM (MTA) and Media Foundation for this thread, released
        // below, as the encoder's worker thread does.
        unsafe {
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                eprintln!("FR-130 MF: no COM on this thread — nothing to read");
                return;
            }
            if let Err(e) = MFStartup(windows_mf_version(), MFSTARTUP_FULL) {
                CoUninitialize();
                eprintln!("FR-130 MF: no Media Foundation here ({e:?}) — nothing to read");
                return;
            }
        }
        let mut read_count = 0;
        let mut failures = Vec::new();
        for codec in [OutputCodec::H264, OutputCodec::Hevc, OutputCodec::Av1] {
            for on in [true, false] {
                // SAFETY: serialised by DENY_ENV_LOCK; the guard restores it.
                unsafe { set("COLOR_DESCRIPTION", if on { "1" } else { "0" }) };
                let switch = if on { "on" } else { "off" };
                for_each_pipeline(codec, 1280, 720, |mft, adapter, p| {
                    let cell = format!(
                        "{} {mft} on {adapter}, switch {switch}",
                        codec.backend_name()
                    );
                    let silent = WRITES_NO_DESCRIPTION.iter().find(|(name, _)| *name == mft);
                    let verdict = read(codec, p).and_then(|(s, place)| {
                        eprintln!("FR-130 MF {cell}: {s:?} ({place})");
                        if !on {
                            // The switch must reach the open itself: an MFT
                            // that writes nothing either way cannot show it in
                            // the stream.
                            return if p.described() {
                                Err("the types carry the description with the switch off".into())
                            } else if s.unspecified() {
                                Ok(())
                            } else {
                                Err(format!("a description with the switch off: {s:?}"))
                            };
                        }
                        if !p.described() {
                            return Err("the MFT refused the colour attributes".into());
                        }
                        match silent {
                            Some((_, evidence)) if s.unspecified() => {
                                eprintln!("FR-130 MF {cell}: measured, writes none: {evidence}");
                                Ok(())
                            }
                            Some(_) => Err(format!(
                                "listed as writing no description, and wrote {s:?}: delete its entry"
                            )),
                            None => s.check(DESCRIPTION, Vocabulary::Merged),
                        }
                    });
                    read_count += 1;
                    if let Err(e) = verdict {
                        failures.push(format!("{cell}: {e}"));
                    }
                });
            }
        }
        // SAFETY: the matching shutdown of the startup above.
        unsafe {
            MFShutdown().ok();
            CoUninitialize();
        }
        eprintln!("FR-130 MF: {read_count} pipelines read");
        assert!(failures.is_empty(), "{failures:#?}");
    }
}
