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
//! Module layout (post-split, Phase 3 commit 1):
//!
//! - `mod.rs` — the thread-pinned `MfEncoder` handle + Cmd loop; COM/MF
//!   lifecycle (CoInitializeEx/MFStartup/MFShutdown/CoUninitialize);
//!   shared D3D11 helpers (`create_d3d11_device_and_manager`,
//!   `activate_h264_encoder`). The latter two are kept here for now;
//!   Phase 3 commit 2 replaces them with adapter-aware variants.
//! - `sync_pipeline.rs` — `MfPipeline`: the synchronous MFT pipeline
//!   (input/output media types, ProcessInput + drain loop, codec-api
//!   tuning knobs, Annex-B NALU extraction).
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
    CLSID_MSH264EncoderMFT, IMFActivate, IMFDXGIDeviceManager, IMFTransform,
    MFCreateDXGIDeviceManager, MFMediaType_Video, MFShutdown, MFStartup, MFSTARTUP_FULL,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::core::Interface;

use super::{EncodedPacket, VideoEncoder};
use crate::capture::{Frame, PixelFormat};

mod sync_pipeline;

use sync_pipeline::MfPipeline;

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
        let mt: ID3D11Multithread = device.cast().map_err(|e| {
            anyhow!("ID3D11Multithread cast: {e:?}")
        })?;
        mt.SetMultithreadProtected(true);

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

/// Activate the best-available H.264 encoder MFT. Tries hardware first
/// (MFTEnumEx with MFT_ENUM_FLAG_HARDWARE — finds NVENC on NVIDIA,
/// QuickSync on Intel, AMF on AMD), falls back to the Microsoft
/// software MFT (CLSID_MSH264EncoderMFT) if no HW MFT is installed.
///
/// Returns the activated transform plus a short tag describing what
/// we got, used only for logging ("hw: NVIDIA NVENC ..." vs "sw: MS").
///
/// Currently unused — `MfPipeline::new` goes straight to the SW MFT.
/// Kept around because Phase 3 commit 2 builds on this enumeration
/// path (adapter-matched activation).
#[allow(dead_code)]
pub(super) unsafe fn activate_h264_encoder() -> Result<(IMFTransform, &'static str)> {
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
        // MFT_ENUM_FLAG is a newtype over i32; OR the inner values, then
        // rewrap so MFTEnumEx gets its expected parameter type.
        let flags = windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG(
            MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0 | MFT_ENUM_FLAG_SYNCMFT.0,
        );
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

/// MF version number passed to MFStartup. `MF_VERSION` is
/// `(MF_API_VERSION << 16) | MF_SDK_VERSION`; the `windows` crate
/// exposes the constants but not the composed value, so we compose it
/// here — this matches what the Microsoft headers produce.
const fn windows_mf_version() -> u32 {
    // From `mfapi.h`: MF_SDK_VERSION = 0x2, MF_API_VERSION = 0x70.
    0x0002_0070
}
