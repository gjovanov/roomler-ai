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
//! Latency knobs applied via `ICodecAPI` (all off by default,
//! every one a must-set for sub-100 ms interactive streaming):
//!
//! * `AVLowLatencyMode = true`
//! * `AVEncCommonRateControlMode = CBR`
//! * `AVEncCommonMeanBitRate = <resolution-scaled>`
//! * `AVEncMPVGOPSize = 60` (IDR every ~2 s at 30 fps)
//! * `AVEncH264CABACEnable = true` (better quality at our bitrate)
//!
//! # What we don't do yet (phase 2+)
//!
//! * Chain `CLSID_VideoProcessorMFT` upstream for GPU-side
//!   BGRA→NV12. We pay a CPU conversion cost today.
//! * Pool `IMFSample`s. Allocated per-frame; fine at 30 fps, revisit
//!   if profiling shows allocator pressure.
//! * D3D11-backed GPU textures end-to-end. Phase 2 alongside the
//!   DXGI Desktop Duplication capture backend.
//!
//! All `windows`-crate calls are wrapped in a Rust-friendly error
//! surface; the module returns `anyhow::Error` to keep the trait
//! contract matching the openh264 backend.

#![cfg(all(target_os = "windows", feature = "mf-encoder"))]

use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread;

use anyhow::{Result, anyhow, bail};
use tokio::sync::oneshot;

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264EncoderMFT, CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncH264CABACEnable, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, IMFActivate, ICodecAPI,
    IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFStartup, MFSTARTUP_FULL, MFShutdown,
    MFMediaType_Video, MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_FRIENDLY_NAME_Attribute,
    MFT_REGISTER_TYPE_INFO, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_INFO,
    eAVEncCommonRateControlMode_CBR, eAVEncCommonRateControlMode_LowDelayVBR,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    CoTaskMemFree,
};
use windows::core::{GUID, Interface, PWSTR};

use super::{EncodedPacket, VideoEncoder};
use crate::capture::{Frame, PixelFormat};
use crate::encode::color;

/// Public-facing handle. Owns only the command channel; the IMFTransform
/// and every other COM interface live on the pinned worker thread.
pub struct MfEncoder {
    cmd_tx: std_mpsc::Sender<Cmd>,
    width: u32,
    height: u32,
}

enum Cmd {
    Encode {
        frame: Arc<Frame>,
        reply: oneshot::Sender<Result<Vec<EncodedPacket>>>,
    },
    RequestKeyframe,
    SetBitrate(u32),
    Shutdown,
}

/// MF pipe owner. Everything COM-touching lives in here, on the worker.
struct MfPipeline {
    transform: IMFTransform,
    codec_api: ICodecAPI,
    width: u32,
    height: u32,
    frame_count: u64,
}

impl MfEncoder {
    /// Build the MF encoder for a given output resolution.
    ///
    /// NV12 requires even dimensions. We reject the construction up-front
    /// so the cascade in `open_default` can fall back to openh264 cleanly
    /// rather than hitting the error later on the first encode call.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            bail!("mf-encoder: require non-zero, even dimensions, got {width}x{height}");
        }

        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<Cmd>();

        thread::Builder::new()
            .name("roomler-agent-mf-encoder".into())
            .spawn(move || {
                // 1. Initialise COM for this thread. MTA because MF is
                //    happy with it and we never touch UI.
                let coinit = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if coinit.is_err() {
                    let _ = ready_tx
                        .send(Err(anyhow!("CoInitializeEx failed: {:?}", coinit)));
                    return;
                }

                // 2. Start Media Foundation.
                if let Err(e) = unsafe { MFStartup(windows_mf_version(), MFSTARTUP_FULL) } {
                    unsafe { CoUninitialize() };
                    let _ = ready_tx.send(Err(anyhow!("MFStartup failed: {e:?}")));
                    return;
                }

                // 3. Build the pipeline.
                let pipeline = match MfPipeline::new(width, height) {
                    Ok(p) => p,
                    Err(e) => {
                        unsafe { MFShutdown().ok() };
                        unsafe { CoUninitialize() };
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };

                let _ = ready_tx.send(Ok(()));
                run_worker(pipeline, cmd_rx);

                unsafe { MFShutdown().ok() };
                unsafe { CoUninitialize() };
            })
            .map_err(|e| anyhow!("spawning mf worker: {e}"))?;

        ready_rx.recv().map_err(|e| anyhow!("mf worker ack: {e}"))??;

        Ok(Self {
            cmd_tx,
            width,
            height,
        })
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
        "mf-h264"
    }
}

// ---------------------------------------------------------------------
// Worker-thread-internal types below. Everything here touches COM.
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

impl MfPipeline {
    fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            // Activate the H.264 Encoder MFT. The CLSID covers the
            // Microsoft-shipped encoder which, on any recent Windows,
            // delegates to the IHV driver's hardware encoder MFT when
            // one is available. There's no separate "HW only" CLSID
            // in v1 — MFTEnumEx with MFT_ENUM_FLAG_HARDWARE is the
            // path to vendor-specific MFTs (phase 3).
            // Prefer a vendor hardware H.264 MFT (NVENC / QuickSync /
            // AMF) enumerated via MFTEnumEx. CoCreateInstance on the
            // plain CLSID always returns Microsoft's *software* MFT,
            // which on a desktop CPU caps out at ~10 fps at 4K and
            // defeats the whole point of this backend. Fall back to the
            // SW MFT only if HW enumeration finds nothing.
            let (transform, backend_kind) = activate_h264_encoder()?;
            tracing::info!(backend = backend_kind, "mf-encoder: activated MFT");

            // Detect + tame async mode. On systems with hardware H.264
            // acceleration (NVIDIA, Intel QSV, AMD AMF), the MS encoder
            // MFT can switch itself into async mode, where it silently
            // buffers every ProcessInput and never returns output to a
            // caller using the sync ProcessOutput loop. The fix is to
            // set MF_TRANSFORM_ASYNC_UNLOCK=1, which makes the MFT
            // honour sync semantics even when its internal worker is
            // async. On MFTs that are already sync-only (WARP fallback,
            // pure-SW Windows), the attribute set is a no-op.
            if let Ok(attrs) = transform.GetAttributes() {
                let is_async = attrs
                    .GetUINT32(&MF_TRANSFORM_ASYNC)
                    .map(|v| v != 0)
                    .unwrap_or(false);
                tracing::info!(is_async, "mf-encoder: MFT async-mode probe");
                if is_async
                    && let Err(e) = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                {
                    tracing::warn!(
                        %e,
                        "mf-encoder: async unlock failed — MFT will stay in async mode; \
                         sync ProcessOutput will buffer forever. Expect zero output."
                    );
                }
            } else {
                tracing::debug!("mf-encoder: MFT has no attribute store");
            }

            // Set output type first (required by the MFT contract).
            let out_type = build_output_media_type(width, height)?;
            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(|e| anyhow!("SetOutputType: {e:?}"))?;

            let in_type = build_input_media_type(width, height)?;
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(|e| anyhow!("SetInputType: {e:?}"))?;

            // Latency + rate-control knobs.
            let codec_api: ICodecAPI = transform
                .cast()
                .map_err(|e| anyhow!("MFT does not expose ICodecAPI: {e:?}"))?;
            set_codec_bool(&codec_api, &CODECAPI_AVLowLatencyMode, true)?;
            set_codec_bool(&codec_api, &CODECAPI_AVEncH264CABACEnable, true)?;
            // Rate-control mode: prefer CBR on hardware MFTs (NVENC,
            // QuickSync, AMF all honour it); fall back to LowDelayVBR
            // on the Microsoft software MFT, which silently ignores
            // CBR and then defaults to quality-based VBR producing
            // 200-700 KB frames at 4K instead of respecting a 12 Mbps
            // target. LowDelayVBR IS supported by the SW MFT.
            let rc_mode = if backend_kind == "hw" {
                eAVEncCommonRateControlMode_CBR.0 as u32
            } else {
                eAVEncCommonRateControlMode_LowDelayVBR.0 as u32
            };
            set_codec_u32(&codec_api, &CODECAPI_AVEncCommonRateControlMode, rc_mode)?;
            set_codec_u32(&codec_api, &CODECAPI_AVEncMPVGOPSize, 60)?;
            let initial_bps =
                crate::encode::initial_bitrate_for(width, height);
            set_codec_u32(&codec_api, &CODECAPI_AVEncCommonMeanBitRate, initial_bps)?;
            // Max bitrate cap for VBR modes — prevents the encoder
            // from bursting way over target on complex frames. We use
            // 1.5× the mean as a reasonable ceiling.
            set_codec_u32(
                &codec_api,
                &CODECAPI_AVEncCommonMaxBitRate,
                initial_bps.saturating_mul(3) / 2,
            )?;

            // Start streaming.
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| anyhow!("BEGIN_STREAMING: {e:?}"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| anyhow!("START_OF_STREAM: {e:?}"))?;

            tracing::info!(
                width,
                height,
                initial_bps,
                "mf-encoder: pipeline ready"
            );

            Ok(Self {
                transform,
                codec_api,
                width,
                height,
                frame_count: 0,
            })
        }
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>> {
        // 1. Convert BGRA → NV12 on the CPU. Phase 2 replaces this with
        //    VideoProcessorMFT chained upstream.
        let nv12 = color::bgra_to_nv12(&frame.data, frame.width, frame.height, frame.stride)
            .map_err(|e| anyhow!("bgra_to_nv12: {e}"))?;

        // 2. Wrap the NV12 payload in an IMFSample.
        let sample = unsafe { build_input_sample(&nv12, self.frame_count)? };
        self.frame_count = self.frame_count.wrapping_add(1);

        // 3. ProcessInput. If the MFT refuses, we drain its output first
        //    and retry — required by the trait-like MFT contract.
        let mut drained_first = false;
        loop {
            let rc = unsafe { self.transform.ProcessInput(0, &sample, 0) };
            match rc {
                Ok(()) => {
                    tracing::debug!(
                        frame = self.frame_count.saturating_sub(1),
                        "mf ProcessInput: OK"
                    );
                    break;
                }
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    tracing::debug!(
                        frame = self.frame_count.saturating_sub(1),
                        "mf ProcessInput: NOTACCEPTING — draining first"
                    );
                    if drained_first {
                        return Err(anyhow!(
                            "mf-encoder: MFT would not accept input after drain"
                        ));
                    }
                    let _ = self.drain_output(Vec::new())?;
                    drained_first = true;
                }
                Err(e) => bail!("ProcessInput: {e:?}"),
            }
        }

        // 4. Drain output. Returns any encoded packets.
        let packets = self.drain_output(Vec::new())?;
        Ok(packets)
    }

    /// Drain `ProcessOutput` until it signals `NEED_MORE_INPUT`.
    /// Collects NALU bytes from each output sample into `EncodedPacket`s.
    fn drain_output(&mut self, mut acc: Vec<EncodedPacket>) -> Result<Vec<EncodedPacket>> {
        // Safety valve: the MS H.264 Encoder MFT can, in rare cases, keep
        // emitting STREAM_CHANGE notifications if we negotiate the output
        // type wrong. Cap the drain loop so a pathological MFT can't spin
        // forever.
        const MAX_ITERATIONS: u32 = 64;
        for iter in 0..MAX_ITERATIONS {
            let output_info: MFT_OUTPUT_STREAM_INFO =
                unsafe { self.transform.GetOutputStreamInfo(0)? };

            let needs_sample = (output_info.dwFlags & 0x100) == 0; // MFT_OUTPUT_STREAM_PROVIDES_SAMPLES
            let sample_slot = if needs_sample {
                let sample = unsafe { MFCreateSample()? };
                let buffer =
                    unsafe { MFCreateMemoryBuffer(output_info.cbSize.max(1_048_576))? };
                unsafe { sample.AddBuffer(&buffer)? };
                Some(sample)
            } else {
                None
            };

            let mut output_buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(sample_slot.clone()),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;

            let rc = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output_buffer), &mut status)
            };
            let produced: Option<IMFSample> =
                unsafe { std::mem::ManuallyDrop::take(&mut output_buffer.pSample) };
            let _events = unsafe { std::mem::ManuallyDrop::take(&mut output_buffer.pEvents) };

            match rc {
                Ok(()) => {
                    if let Some(s) = produced {
                        match read_packet_from_sample(&s)? {
                            Some(pkt) => {
                                tracing::debug!(
                                    bytes = pkt.data.len(),
                                    is_keyframe = pkt.is_keyframe,
                                    dw_status = status,
                                    "mf ProcessOutput produced"
                                );
                                acc.push(pkt);
                            }
                            None => {
                                tracing::debug!(
                                    dw_status = status,
                                    "mf ProcessOutput returned zero-byte sample"
                                );
                            }
                        }
                    } else {
                        tracing::debug!(
                            dw_status = status,
                            "mf ProcessOutput Ok but no sample produced"
                        );
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    tracing::debug!(
                        iter,
                        produced = acc.len(),
                        "mf ProcessOutput: NEED_MORE_INPUT (drain done)"
                    );
                    return Ok(acc);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The MFT changed its output media type (common on
                    // the first ProcessOutput — MS H.264 Encoder MFT
                    // renegotiates the exact profile/level once it sees
                    // the first input). Re-query + re-apply and retry
                    // the drain loop. Without this, every subsequent
                    // ProcessOutput buffers input but produces zero
                    // output — the symptom observed in 0.1.15 smoke.
                    tracing::info!(iter, "mf ProcessOutput: STREAM_CHANGE — renegotiating output type");
                    unsafe {
                        let new_type = self.transform.GetOutputAvailableType(0, 0)?;
                        self.transform.SetOutputType(0, &new_type, 0)?;
                    }
                    // Loop continues, retry ProcessOutput with the new
                    // type. The MFT will now accept / produce output.
                }
                Err(e) => bail!("ProcessOutput: {e:?}"),
            }
            let _ = iter; // unused in non-trace builds
        }
        tracing::warn!(
            iterations = MAX_ITERATIONS,
            "mf drain_output hit iteration cap — suspect stream-change loop"
        );
        Ok(acc)
    }

    fn force_keyframe(&self) -> Result<()> {
        set_codec_u32(&self.codec_api, &CODECAPI_AVEncVideoForceKeyFrame, 1)
    }

    fn set_bitrate(&self, bps: u32) -> Result<()> {
        set_codec_u32(&self.codec_api, &CODECAPI_AVEncCommonMeanBitRate, bps)
    }

    fn end_stream(&self) -> Result<()> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .ok();
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .ok();
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)
                .ok();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Helpers (all unsafe-COM, kept in one place for easier auditing).
// ---------------------------------------------------------------------

/// Activate the best-available H.264 encoder MFT. Tries hardware first
/// (MFTEnumEx with MFT_ENUM_FLAG_HARDWARE — finds NVENC on NVIDIA,
/// QuickSync on Intel, AMF on AMD), falls back to the Microsoft
/// software MFT (CLSID_MSH264EncoderMFT) if no HW MFT is installed.
///
/// Returns the activated transform plus a short tag describing what
/// we got, used only for logging ("hw: NVIDIA NVENC ..." vs "sw: MS").
unsafe fn activate_h264_encoder() -> Result<(IMFTransform, &'static str)> {
    unsafe {
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };

        // Hardware MFTs first. `SORTANDFILTER` asks MF to order results
        // by merit so the best-scoring hardware encoder is index 0.
        let flags =
            MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0 | MFT_ENUM_FLAG_SYNCMFT.0;
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        let enum_rc = MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input_info),
            Some(&output_info),
            &mut activates,
            &mut count,
        );

        if enum_rc.is_ok() && count > 0 && !activates.is_null() {
            // Walk the returned IMFActivate array, try each one. The MF
            // idiom is: activate to IMFTransform, release the activate.
            let slice = std::slice::from_raw_parts(activates, count as usize);
            let mut last_err: Option<windows::core::Error> = None;
            for (i, maybe_act) in slice.iter().enumerate() {
                let Some(act) = maybe_act else { continue };
                // Best-effort friendly-name log for diagnostics — it's
                // how we'll know "oh yeah NVENC" vs "Intel Quick Sync".
                let mut name_buf: [u16; 256] = [0; 256];
                let mut name_len: u32 = 0;
                let _ = act.GetString(
                    &MFT_FRIENDLY_NAME_Attribute,
                    &mut name_buf,
                    Some(&mut name_len),
                );
                let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
                match act.ActivateObject::<IMFTransform>() {
                    Ok(transform) => {
                        tracing::info!(
                            index = i,
                            name = %name,
                            total = count,
                            "mf-encoder: activated hardware MFT"
                        );
                        // Free the remaining IMFActivate references and the array.
                        for other in &slice[i + 1..] {
                            if let Some(a) = other {
                                drop(a.clone()); // release our clone
                            }
                        }
                        CoTaskMemFree(Some(activates as *const _));
                        return Ok((transform, "hw"));
                    }
                    Err(e) => {
                        tracing::warn!(
                            index = i,
                            name = %name,
                            %e,
                            "mf-encoder: ActivateObject on hardware MFT failed — trying next"
                        );
                        last_err = Some(e);
                    }
                }
            }
            CoTaskMemFree(Some(activates as *const _));
            if let Some(e) = last_err {
                tracing::warn!(%e, "mf-encoder: all hardware MFTs failed to activate — falling back to SW");
            }
        } else {
            tracing::info!(
                count,
                "mf-encoder: no hardware H.264 MFT enumerated — falling back to SW"
            );
        }

        // SW fallback — CLSID_MSH264EncoderMFT is always present on
        // Windows 8+ and is sync-only, producing ~10 fps at 4K on a
        // desktop CPU. Good for low-res screens and pure-SW VMs.
        let transform: IMFTransform =
            CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| anyhow!("CoCreateInstance MSH264Encoder (SW fallback): {e:?}"))?;
        Ok((transform, "sw"))
    }
}

unsafe fn build_output_media_type(width: u32, height: u32) -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        t.SetUINT32(
            &MF_MT_AVG_BITRATE,
            crate::encode::initial_bitrate_for(width, height),
        )?;
        set_ratio(&t, &MF_MT_FRAME_SIZE, width, height)?;
        set_ratio(&t, &MF_MT_FRAME_RATE, 30, 1)?;
        set_ratio(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        t.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )?;
        Ok(t)
    }
}

unsafe fn build_input_media_type(width: u32, height: u32) -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        set_ratio(&t, &MF_MT_FRAME_SIZE, width, height)?;
        set_ratio(&t, &MF_MT_FRAME_RATE, 30, 1)?;
        set_ratio(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        t.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )?;
        Ok(t)
    }
}

/// MF encodes a pair of u32 values into a single u64 for ratio-type
/// attributes like frame size and frame rate.
unsafe fn set_ratio(t: &IMFMediaType, key: &GUID, hi: u32, lo: u32) -> Result<()> {
    let packed: u64 = ((hi as u64) << 32) | (lo as u64);
    unsafe { t.SetUINT64(key, packed)? };
    Ok(())
}

unsafe fn build_input_sample(nv12: &[u8], frame_index: u64) -> Result<IMFSample> {
    unsafe {
        let sample: IMFSample = MFCreateSample()?;
        let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(nv12.len() as u32)?;

        // Lock, copy, SetCurrentLength, unlock.
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
        if (max_len as usize) < nv12.len() {
            let _ = buffer.Unlock();
            bail!("mf: buffer too small: {} < {}", max_len, nv12.len());
        }
        std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
        buffer.SetCurrentLength(nv12.len() as u32)?;
        buffer.Unlock()?;

        sample.AddBuffer(&buffer)?;
        // MF timestamps are 100-ns units. At 30 fps we advance by 333_333.
        let ts_100ns: i64 = frame_index as i64 * 333_333;
        sample.SetSampleTime(ts_100ns)?;
        sample.SetSampleDuration(333_333)?;
        Ok(sample)
    }
}

/// Read the NALU run out of an output IMFSample and wrap it in an
/// `EncodedPacket`. Returns `None` if the sample is empty (e.g. the
/// MFT handed us a format-change notification).
fn read_packet_from_sample(sample: &IMFSample) -> Result<Option<EncodedPacket>> {
    unsafe {
        let total_len: u32 = sample.GetTotalLength()?;
        if total_len == 0 {
            return Ok(None);
        }
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
        let data = std::slice::from_raw_parts(ptr, cur_len as usize).to_vec();
        buffer.Unlock()?;

        // MF emits Annex-B NALUs by default (same as openh264). The H264
        // payloader on the webrtc side looks for [0 0 0 1] start codes to
        // split into RTP packets, so we can pass the bitstream through.
        let is_keyframe = nalu_contains_idr(&data);
        Ok(Some(EncodedPacket {
            data,
            is_keyframe,
            duration_us: 33_333,
        }))
    }
}

/// Scan an Annex-B bitstream for an IDR NAL (nal_unit_type == 5).
/// Good-enough heuristic for the `is_keyframe` flag — the RTP layer
/// doesn't actually use this, it's just observability.
fn nalu_contains_idr(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < buf.len() {
        // Annex-B start code: 00 00 00 01 or 00 00 01.
        let (nal_off, next) = if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 0 && buf[i + 3] == 1
        {
            (i + 4, i + 4)
        } else if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            (i + 3, i + 3)
        } else {
            i += 1;
            continue;
        };
        if nal_off < buf.len() {
            let nal_type = buf[nal_off] & 0x1f;
            if nal_type == 5 {
                return true;
            }
        }
        i = next + 1;
    }
    false
}

/// Set a boolean codec-api property. `windows` 0.58 exposes a
/// high-level `VARIANT` from `windows::core` with `From<bool>`, so we
/// skip the union-field dance of the raw Win32 VARIANT. An `E_FAIL`
/// from the MFT is interpreted as "key not supported" — non-fatal,
/// since we try to set a superset of knobs that any given driver may
/// or may not recognise.
fn set_codec_bool(codec: &ICodecAPI, key: &GUID, value: bool) -> Result<()> {
    let var: windows::core::VARIANT = value.into();
    let hr = unsafe { codec.SetValue(key, &var) };
    match hr {
        Ok(()) => Ok(()),
        Err(e) if e.code() == E_FAIL => {
            tracing::debug!(?key, "codec-api key not supported by MFT");
            Ok(())
        }
        Err(e) => Err(anyhow!("codec SetValue bool: {e:?}")),
    }
}

fn set_codec_u32(codec: &ICodecAPI, key: &GUID, value: u32) -> Result<()> {
    let var: windows::core::VARIANT = value.into();
    let hr = unsafe { codec.SetValue(key, &var) };
    match hr {
        Ok(()) => Ok(()),
        Err(e) if e.code() == E_FAIL => {
            tracing::debug!(?key, value, "codec-api key not supported by MFT");
            Ok(())
        }
        Err(e) => Err(anyhow!("codec SetValue u32: {e:?}")),
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
