//! Synchronous Media Foundation H.264 encoder pipeline.
//!
//! Owns the MFT, the D3D11 device + DXGI manager, and all input/output
//! media-type state. Every method runs on the pinned worker thread
//! created by [`super::MfEncoder`]; nothing here is thread-safe on its
//! own. The sync in the name distinguishes it from the async-MFT
//! pipeline that Phase 3 commit 4 introduces for Intel QSV.

#![cfg(all(target_os = "windows", feature = "mf-encoder"))]

use anyhow::{Result, anyhow, bail};

use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncH264CABACEnable, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFDXGIDeviceManager,
    IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_INFO, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    eAVEncCommonRateControlMode_CBR, eAVEncCommonRateControlMode_LowDelayVBR,
};
use windows::core::{GUID, Interface};

use super::super::EncodedPacket;
use crate::capture::Frame;
use crate::encode::color;

/// MF pipeline owner. Everything COM-touching lives in here, on the worker.
pub(super) struct MfPipeline {
    transform: IMFTransform,
    codec_api: ICodecAPI,
    /// D3D11 device + DXGI manager kept alive for the MFT's lifetime.
    /// Hardware MFTs (NVIDIA NVENC, Intel QSV, AMD AMF) and
    /// CLSID_MSH264EncoderMFT on a box with HW acceleration drivers
    /// installed require this handoff before they'll produce output.
    /// Without it NVENC ActivateObject returns 0x8000FFFF and the MS
    /// MFT silently returns NEED_MORE_INPUT forever. The device must
    /// outlive the transform — dropping it early would leave the MFT
    /// holding a dangling manager reference.
    _d3d_device: Option<ID3D11Device>,
    _d3d_manager: Option<IMFDXGIDeviceManager>,
    width: u32,
    height: u32,
    frame_count: u64,
}

impl MfPipeline {
    /// Build a pipeline from an already-activated [`IMFTransform`].
    ///
    /// Caller contract (satisfied by the cascade in [`super::activate`]):
    /// - transform has been activated via `IMFActivate::ActivateObject`
    ///   (HW path) or `CoCreateInstance(CLSID_MSH264EncoderMFT)` (SW).
    /// - D3D manager, if any, has already been bound via
    ///   `MFT_MESSAGE_SET_D3D_MANAGER`.
    /// - `MF_TRANSFORM_ASYNC_UNLOCK` has already been applied if the MFT
    ///   reports async. Async-only MFTs should never reach this path;
    ///   they get routed to the async pipeline instead.
    ///
    /// `backend_kind` selects rate-control mode: `"hw"` → CBR (NVENC,
    /// QSV, AMF all honour it), `"sw"` → LowDelayVBR (MS SW MFT rejects
    /// CBR + LowLatency combo and silently falls back to quality-VBR,
    /// overshooting target bitrate ~5×). Either value is shared with
    /// the cascade logger via the returned pipeline's name.
    ///
    /// `_d3d_device` and `_d3d_manager` are kept as fields so the MFT's
    /// weak references remain valid for the pipeline's lifetime.
    pub(super) fn new(
        transform: IMFTransform,
        d3d_device: Option<ID3D11Device>,
        d3d_manager: Option<IMFDXGIDeviceManager>,
        backend_kind: &'static str,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        unsafe {
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
            let rc_mode = if backend_kind == "hw" {
                eAVEncCommonRateControlMode_CBR.0 as u32
            } else {
                eAVEncCommonRateControlMode_LowDelayVBR.0 as u32
            };
            set_codec_u32(&codec_api, &CODECAPI_AVEncCommonRateControlMode, rc_mode)?;
            set_codec_u32(&codec_api, &CODECAPI_AVEncMPVGOPSize, 60)?;
            let initial_bps = crate::encode::initial_bitrate_for(width, height);
            set_codec_u32(&codec_api, &CODECAPI_AVEncCommonMeanBitRate, initial_bps)?;
            // Max bitrate cap for VBR modes — prevents the encoder
            // from bursting way over target on complex frames. 1.5×
            // the mean as a reasonable ceiling.
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
                backend = backend_kind,
                width,
                height,
                initial_bps,
                "mf-encoder: pipeline ready"
            );

            Ok(Self {
                transform,
                codec_api,
                _d3d_device: d3d_device,
                _d3d_manager: d3d_manager,
                width,
                height,
                frame_count: 0,
            })
        }
    }

    /// Pipeline dimensions. Used by the probe harness to shape its
    /// synthetic NV12 payload.
    pub(super) fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Encode a pre-converted NV12 payload. Shared path used by both
    /// the regular [`Self::encode`] (which does BGRA→NV12 upstream)
    /// and the cascade probe in [`super::probe`].
    pub(super) fn encode_nv12(
        &mut self,
        nv12: &[u8],
        frame_index: u64,
    ) -> Result<Vec<EncodedPacket>> {
        let sample = unsafe { build_input_sample(nv12, frame_index)? };
        let mut drained_first = false;
        loop {
            let rc = unsafe { self.transform.ProcessInput(0, &sample, 0) };
            match rc {
                Ok(()) => {
                    tracing::debug!(frame = frame_index, "mf ProcessInput: OK");
                    break;
                }
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    tracing::debug!(
                        frame = frame_index,
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
        self.frame_count = self.frame_count.wrapping_add(1);
        let packets = self.drain_output(Vec::new())?;
        Ok(packets)
    }

    pub(super) fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>> {
        // Defensive check — the MfEncoder handle also validates, but
        // a direct caller to MfPipeline (probe harness) can skip that
        // layer.
        if frame.width != self.width || frame.height != self.height {
            bail!(
                "mf-pipeline: frame dim mismatch: configured {}x{}, got {}x{}",
                self.width,
                self.height,
                frame.width,
                frame.height
            );
        }

        // BGRA → NV12 on the CPU. Phase 2 replaces this with
        // VideoProcessorMFT chained upstream (per-plan 1C.3).
        let nv12 = color::bgra_to_nv12(&frame.data, frame.width, frame.height, frame.stride)
            .map_err(|e| anyhow!("bgra_to_nv12: {e}"))?;

        let frame_index = self.frame_count;
        self.encode_nv12(&nv12, frame_index)
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

    pub(super) fn force_keyframe(&self) -> Result<()> {
        set_codec_u32(&self.codec_api, &CODECAPI_AVEncVideoForceKeyFrame, 1)
    }

    pub(super) fn set_bitrate(&self, bps: u32) -> Result<()> {
        set_codec_u32(&self.codec_api, &CODECAPI_AVEncCommonMeanBitRate, bps)
    }

    pub(super) fn end_stream(&self) -> Result<()> {
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
/// MFT quirk: different vendors reject "unsupported codec knob"
/// differently. MS SW MFT returns E_FAIL. Intel QSV / NVIDIA / some
/// older Windows builds return E_INVALIDARG. Either way we don't
/// want a single unsupported tuning knob to fail the whole init —
/// downgrade both to a debug log.
fn is_unsupported_codec_key_error(e: &windows::core::Error) -> bool {
    let code = e.code();
    code == E_FAIL || code == E_INVALIDARG
}

fn set_codec_bool(codec: &ICodecAPI, key: &GUID, value: bool) -> Result<()> {
    let var: windows::core::VARIANT = value.into();
    let hr = unsafe { codec.SetValue(key, &var) };
    match hr {
        Ok(()) => Ok(()),
        Err(e) if is_unsupported_codec_key_error(&e) => {
            tracing::debug!(?key, code = %e.code().0, "codec-api key not supported by MFT");
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
        Err(e) if is_unsupported_codec_key_error(&e) => {
            tracing::debug!(?key, value, code = %e.code().0, "codec-api key not supported by MFT");
            Ok(())
        }
        Err(e) => Err(anyhow!("codec SetValue u32: {e:?}")),
    }
}
