// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! FR-118 P2-2 — the encoder takes a GPU frame: the BGRA texture a capture
//! backend delivered ([`GpuFrame`]) becomes NV12 on the GPU, in the encoder's
//! own frame pool, without a trip through CPU memory.
//!
//! ## The colour conversion
//!
//! [`Converter`] is one D3D11 `VideoProcessorBlt`, BGRA in and NV12 out, with
//! the matrix the CPU path uses (`encode/color.rs`, dcv's `Bt601`): BT.601,
//! limited range. A host that flips the switch must not change colour, and
//! the test below holds the two paths to ±2 code values on flat colour.
//!
//! ## The QSV frame pool ([`QsvGpuInput`])
//!
//! FFmpeg's QSV encoders take only `AV_PIX_FMT_QSV` frames (or system memory),
//! so the pool is a QSV frames context on a QSV device **derived from the
//! capture's own D3D11 device**: one device, so the adapter rule (FR-118 §4.2)
//! holds by construction, and no frame crosses a device. Every choice below is
//! checked against FFmpeg 9.0.1's `hwcontext_qsv.c` / `hwcontext_d3d11va.c`,
//! the version this agent links:
//!
//! - **The pool is decoder-target** (`MFX_MEMTYPE_VIDEO_MEMORY_DECODER_TARGET`).
//!   FFmpeg builds a fixed D3D11 pool as ONE array texture whatever its bind
//!   flags, and maps a decoder-target surface to (that texture, its slice): the
//!   transcode path every QSV decode → encode pipeline runs. A processor-target
//!   (render-target) surface it tags `MFX_INFINITE`, "not an array slice", over
//!   the same array texture, a combination this code does not rely on. So the
//!   VideoProcessor writes into [`Converter`]'s own NV12 texture, and one
//!   `CopySubresourceRegion` puts it into the pool frame's slice.
//! - **The pool is created on the QSV device**, not derived from a D3D11 pool:
//!   FFmpeg then sizes its child textures to 16-aligned dims with the surface's
//!   crop at the real size, the layout the encoder expects.
//! - **One device context, two threads.** `av_hwdevice_ctx_init` turns the
//!   device's multithread protection on, as the capture's texture pool already
//!   has (FR-118 P2-1).

use std::ffi::c_void;

use anyhow::{Result, anyhow};
use ffmpeg_next::frame;
use ffmpeg_next::sys as ff;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_DECODER, D3D11_BIND_RENDER_TARGET, D3D11_BOX, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::Interface;

use crate::capture::gpu_frame::GpuFrame;

/// The libmfx memory type of a pool whose surfaces are slices of one
/// decoder-bound array texture: oneVPL `mfxstructures.h`,
/// `MFX_MEMTYPE_VIDEO_MEMORY_DECODER_TARGET = 0x0010` (the bindings carry no
/// libmfx headers). Why decoder-target: the module doc.
const MFX_MEMTYPE_VIDEO_MEMORY_DECODER_TARGET: i32 = 0x0010;

/// The start of the error a device without QSV gives (the derived device did
/// not open), which [`QsvGpuInput::open`] reports as `adapter_mismatch`.
const NO_QSV: &str = "no QSV on this device";

/// The QSV pool's size: the frames the encoder holds in flight, the one being
/// filled, and headroom. A fixed pool, because FFmpeg's QSV surfaces are
/// fixed unless the runtime is oneVPL 2.9 or newer (`AVQSVFramesContext`).
const POOL_SIZE: i32 = 12;

/// FFmpeg's error text for a negative return.
fn ff_err(rc: i32) -> String {
    let mut buf = [0u8; 128];
    // SAFETY: a fixed-size buffer FFmpeg NUL-terminates within.
    unsafe {
        ff::av_strerror(rc, buf.as_mut_ptr() as *mut std::os::raw::c_char, buf.len());
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    format!("{} ({rc})", String::from_utf8_lossy(&buf[..end]))
}

/// Why an encoder did not take GPU frames: the reason the heartbeat carries
/// (FR-118 §4.6), and the error behind it when there is one.
#[derive(Debug)]
pub(crate) struct Refused {
    pub(crate) reason: &'static str,
    pub(crate) detail: Option<String>,
}

impl Refused {
    fn because(reason: &'static str) -> Self {
        Self {
            reason,
            detail: None,
        }
    }
}

/// The cells that take GPU frames: QSV's and (FR-118 P3) NVENC's 4:2:0
/// encoders. (`vp9_qsv` has its own open-time probes and stays on CPU frames
/// until it is measured; NVENC's 4:4:4 forms wait for their own measurement.)
pub(crate) fn gpu_cell(name: &str, chroma444: bool) -> bool {
    !chroma444
        && matches!(
            name,
            "hevc_qsv" | "av1_qsv" | "h264_qsv" | "hevc_nvenc" | "av1_nvenc" | "h264_nvenc"
        )
}

/// FR-118 P3 — an NVENC cell, whose GPU input is a D3D11 frame pool on an
/// NVIDIA device rather than a QSV one.
fn is_nvenc(name: &str) -> bool {
    name.ends_with("_nvenc")
}

/// FR-118 P2-2 — the pool a `name` encoder opens on, or why it opens on CPU
/// frames. Every gate is read here, at the open: the switch, the device the
/// pump named (a GPU frame's), the cell, the denylist, the probe's proof
/// (P2-3); then the pool itself, which fails on an adapter that is not the
/// cell's (`adapter_mismatch`).
pub(crate) fn plan(
    name: &str,
    chroma444: bool,
    width: u32,
    height: u32,
) -> Result<GpuInput, Refused> {
    if !crate::capture::gpu_frame::zero_copy_enabled() {
        return Err(Refused::because("switch_off"));
    }
    let Some(device) = super::gpu_hint::current() else {
        return Err(Refused::because("cpu_frames"));
    };
    if !gpu_cell(name, chroma444) {
        return Err(Refused::because("no_gpu_cell"));
    }
    if super::gpu_hint::is_denied(&super::gpu_hint::denied(), name) {
        return Err(Refused::because("denied"));
    }
    if !proven(name) {
        return Err(Refused::because("unproven"));
    }
    if is_nvenc(name) {
        NvencGpuInput::open(&device, width, height).map(GpuInput::Nvenc)
    } else {
        QsvGpuInput::open(&device, width, height).map(GpuInput::Qsv)
    }
}

/// FR-118 — an encoder's GPU input: QSV's derived pool (P2-2), or NVENC's
/// D3D11 pool (P3). Both fill their pool from a [`GpuFrame`] through the same
/// [`Converter`] (BT.601 limited, the CPU path's matrix), so which backend
/// takes the frame never changes its colour.
pub(crate) enum GpuInput {
    Qsv(QsvGpuInput),
    Nvenc(NvencGpuInput),
}

impl GpuInput {
    /// The pixel format the encoder is configured with: the pool's frames.
    pub(crate) fn pix_fmt(&self) -> ff::AVPixelFormat {
        match self {
            GpuInput::Qsv(_) => ff::AVPixelFormat::AV_PIX_FMT_QSV,
            GpuInput::Nvenc(_) => ff::AVPixelFormat::AV_PIX_FMT_D3D11,
        }
    }

    /// The frame pool, for the encoder's configure step to take a NEW
    /// reference to per open attempt (`QsvGpuInput::frames_raw`).
    pub(crate) fn frames_raw(&self) -> *mut ff::AVBufferRef {
        match self {
            GpuInput::Qsv(q) => q.frames_raw(),
            GpuInput::Nvenc(n) => n.frames,
        }
    }

    /// The capture device the pool lives on.
    pub(crate) fn device(&self) -> &ID3D11Device {
        match self {
            GpuInput::Qsv(q) => q.device(),
            GpuInput::Nvenc(n) => &n.device,
        }
    }

    /// Whether `device` is the pool's (a capture backend rebuilds its device
    /// after a desktop switch).
    pub(crate) fn same_device(&self, device: &ID3D11Device) -> bool {
        self.device().as_raw() == device.as_raw()
    }

    /// A GPU frame as a frame of the pool's format, for `send_frame`.
    pub(crate) fn convert_gpu_frame(&mut self, gpu: &GpuFrame) -> Result<frame::Video> {
        match self {
            GpuInput::Qsv(q) => q.convert_gpu_frame(gpu),
            GpuInput::Nvenc(n) => n.convert_gpu_frame(gpu),
        }
    }

    /// A CPU frame (NV12, the pool's size, props stamped) uploaded into the
    /// pool: the frames that still arrive in memory.
    pub(crate) fn upload(&self, sw: &frame::Video) -> Result<frame::Video> {
        match self {
            GpuInput::Qsv(q) => q.upload(sw),
            GpuInput::Nvenc(n) => upload_into(n.frames, sw),
        }
    }
}

/// The PCI vendor of the adapter `device` was created on.
fn adapter_vendor(device: &ID3D11Device) -> Result<u32> {
    // SAFETY: QueryInterface and two getters on a live device; the adapter is
    // a new reference released at the end of the scope.
    unsafe {
        let dxgi: IDXGIDevice = device
            .cast()
            .map_err(|e| anyhow!("ID3D11Device -> IDXGIDevice: {e}"))?;
        let adapter = dxgi
            .GetAdapter()
            .map_err(|e| anyhow!("IDXGIDevice::GetAdapter: {e}"))?;
        let desc = adapter
            .GetDesc()
            .map_err(|e| anyhow!("IDXGIAdapter::GetDesc: {e}"))?;
        Ok(desc.VendorId)
    }
}

/// NVIDIA's PCI vendor id: the only adapter NVENC opens a DirectX session on.
const NVIDIA: u32 = 0x10de;

/// FR-118 P3 — an NVENC encoder's input on the capture's D3D11 device: an
/// `AV_PIX_FMT_D3D11` frame pool (NV12) the encoder is opened with, and the
/// conversion that fills it from a [`GpuFrame`].
///
/// FFmpeg 9.0.1's `nvenc.c` (checked in the source): an `hw_frames_ctx` of
/// format `AV_PIX_FMT_D3D11` makes the encoder open its session on that pool's
/// D3D11 device (`nvenc_setup_device`), and each frame is registered by its
/// texture and array slice (`data[0]`, `data[1]`; `nvenc_register_frame`), with
/// NV12 a supported buffer format. FFmpeg builds the pool as ONE array texture
/// (`d3d11va_frames_init`), so the converter writes its own NV12 texture of the
/// pool's size, and one whole-subresource copy puts it into the frame's slice.
pub(crate) struct NvencGpuInput {
    device: ID3D11Device,
    /// `AV_HWDEVICE_TYPE_D3D11VA`, wrapping `device`.
    d3d11: *mut ff::AVBufferRef,
    /// The `AV_PIX_FMT_D3D11` frames context the encoder's `hw_frames_ctx`
    /// refers to.
    frames: *mut ff::AVBufferRef,
    converter: Converter,
    width: u32,
    height: u32,
}

// SAFETY: as for `QsvGpuInput`: reference-counted FFmpeg contexts used by one
// encoder from one thread at a time; a free-threaded device whose immediate
// context is multithread-protected.
unsafe impl Send for NvencGpuInput {}

impl NvencGpuInput {
    /// The pool on `device` for a `width × height` encoder. Refuses a device
    /// that is not NVIDIA's (`adapter_mismatch`: NVENC opens a DirectX session
    /// on no other), odd dims (an NV12 texture has even ones), and leaves
    /// nothing behind when a step fails.
    pub(crate) fn open(device: &ID3D11Device, width: u32, height: u32) -> Result<Self, Refused> {
        let failed = |e: anyhow::Error| Refused {
            reason: "open_failed",
            detail: Some(e.to_string()),
        };
        match adapter_vendor(device) {
            Ok(NVIDIA) => {}
            Ok(vendor) => {
                return Err(Refused {
                    reason: "adapter_mismatch",
                    detail: Some(format!(
                        "the capture device is on vendor 0x{vendor:04x}, not NVIDIA's"
                    )),
                });
            }
            Err(e) => return Err(failed(e)),
        }
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(Refused {
                reason: "open_failed",
                detail: Some(format!("odd dims {width}x{height} for an NV12 pool")),
            });
        }
        let converter = Converter::new(device, width, height, (width, height)).map_err(failed)?;
        Self::open_with(device, converter, width, height).map_err(failed)
    }

    fn open_with(
        device: &ID3D11Device,
        converter: Converter,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let mut this = Self {
            device: device.clone(),
            d3d11: std::ptr::null_mut(),
            frames: std::ptr::null_mut(),
            converter,
            width,
            height,
        };
        // SAFETY: each FFmpeg call gets the buffers it documents; a failure
        // returns with the ones made so far owned by `this`, whose Drop unrefs
        // them.
        unsafe {
            this.d3d11 = ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if this.d3d11.is_null() {
                return Err(anyhow!("av_hwdevice_ctx_alloc(d3d11va) failed"));
            }
            let hw = (*this.d3d11).data as *mut ff::AVHWDeviceContext;
            let d3d = (*hw).hwctx as *mut ff::AVD3D11VADeviceContext;
            // FFmpeg releases the device when the context goes, so it gets a
            // reference of its own.
            (*d3d).device = device.clone().into_raw() as *mut ff::ID3D11Device;
            let rc = ff::av_hwdevice_ctx_init(this.d3d11);
            if rc < 0 {
                return Err(anyhow!("av_hwdevice_ctx_init(d3d11va): {}", ff_err(rc)));
            }
            this.frames = ff::av_hwframe_ctx_alloc(this.d3d11);
            if this.frames.is_null() {
                return Err(anyhow!("av_hwframe_ctx_alloc(d3d11va) failed"));
            }
            let ctx = (*this.frames).data as *mut ff::AVHWFramesContext;
            (*ctx).format = ff::AVPixelFormat::AV_PIX_FMT_D3D11;
            (*ctx).sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
            (*ctx).width = width as i32;
            (*ctx).height = height as i32;
            (*ctx).initial_pool_size = POOL_SIZE;
            // Decoder-bound, as the frames of every d3d11va-decode → NVENC
            // transcode are: the pool is one NV12 array texture, and the RTX
            // 5090 refuses that array (E_INVALIDARG from `CreateTexture2D`)
            // with no bind flags or as a render target, measured. A
            // decoder-bound slice still takes a copy, as QSV's pool does.
            let d3d_frames = (*ctx).hwctx as *mut ff::AVD3D11VAFramesContext;
            (*d3d_frames).BindFlags = D3D11_BIND_DECODER.0 as u32;
            let rc = ff::av_hwframe_ctx_init(this.frames);
            if rc < 0 {
                return Err(anyhow!("av_hwframe_ctx_init(d3d11va): {}", ff_err(rc)));
            }
        }
        Ok(this)
    }

    /// A GPU frame as a D3D11 pool frame for `send_frame`: converted to NV12 by
    /// the VideoProcessor, then copied into the pool frame's array slice.
    fn convert_gpu_frame(&mut self, gpu: &GpuFrame) -> Result<frame::Video> {
        if gpu.device().as_raw() != self.device.as_raw() {
            return Err(anyhow!("the frame is on another D3D11 device"));
        }
        if gpu.dims() != (self.width, self.height) {
            return Err(anyhow!(
                "a {:?} frame for a {}x{} pool",
                gpu.dims(),
                self.width,
                self.height
            ));
        }
        self.converter.convert(gpu.texture())?;
        let mut out = frame::Video::empty();
        // SAFETY: `out` is an empty frame the pool fills with its array texture
        // (data[0]) and slice index (data[1]), live while `out` is. The copy is
        // two NV12 textures of one size on this device: the converter's output
        // and one slice of the pool's array.
        unsafe {
            let rc = ff::av_hwframe_get_buffer(self.frames, out.as_mut_ptr(), 0);
            if rc < 0 {
                return Err(anyhow!("av_hwframe_get_buffer(d3d11va): {}", ff_err(rc)));
            }
            let raw = (*out.as_ptr()).data[0] as *mut c_void;
            let slice = (*out.as_ptr()).data[1] as usize as u32;
            let texture = ID3D11Texture2D::from_raw_borrowed(&raw)
                .ok_or_else(|| anyhow!("the pool frame has no texture"))?;
            let context = self
                .device
                .GetImmediateContext()
                .map_err(|e| anyhow!("GetImmediateContext: {e}"))?;
            // An array slice of a one-mip texture is subresource `slice`.
            context.CopySubresourceRegion(
                texture,
                slice,
                0,
                0,
                0,
                self.converter.output(),
                0,
                None,
            );
        }
        Ok(out)
    }
}

impl Drop for NvencGpuInput {
    fn drop(&mut self) {
        // SAFETY: each buffer is ours, once; unref of a null one is a no-op.
        // The frame pool goes first: it holds a reference to the device.
        unsafe {
            ff::av_buffer_unref(&mut self.frames);
            ff::av_buffer_unref(&mut self.d3d11);
        }
    }
}

/// A CPU frame (NV12, the pool's size, pts and picture type stamped) as a
/// frame of the pool `frames`: FFmpeg's upload into a pool frame.
fn upload_into(frames: *mut ff::AVBufferRef, sw: &frame::Video) -> Result<frame::Video> {
    let mut hw = frame::Video::empty();
    // SAFETY: an empty frame the pool fills, a populated software frame.
    unsafe {
        let rc = ff::av_hwframe_get_buffer(frames, hw.as_mut_ptr(), 0);
        if rc < 0 {
            return Err(anyhow!("av_hwframe_get_buffer: {}", ff_err(rc)));
        }
        let rc = ff::av_hwframe_transfer_data(hw.as_mut_ptr(), sw.as_ptr(), 0);
        if rc < 0 {
            return Err(anyhow!("av_hwframe_transfer_data(upload): {}", ff_err(rc)));
        }
        let rc = ff::av_frame_copy_props(hw.as_mut_ptr(), sw.as_ptr());
        if rc < 0 {
            return Err(anyhow!("av_frame_copy_props: {}", ff_err(rc)));
        }
    }
    Ok(hw)
}

/// FR-118 P2-3 — the encoders whose GPU-input form the capability probe proved
/// in its child process. `caps::detect` installs them once, before any session.
static PROVEN: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

pub(crate) fn install_proven(names: Vec<String>) {
    let _ = PROVEN.set(names);
}

/// Whether `name`'s GPU-input form may open in this process: the probe child
/// proved it, or this process IS the probe child, whose job is proving it. A
/// form no child has opened never opens in the daemon (`encoder-cells` §1: a
/// fault in a driver belongs in a process that can die).
fn proven(name: &str) -> bool {
    crate::encode::caps::in_probe_child()
        || PROVEN
            .get()
            .is_some_and(|names| names.iter().any(|n| n == name))
}

/// A QSV encoder's input on the capture's D3D11 device: the frame pool the
/// encoder is opened with, and the conversion that fills it from a
/// [`GpuFrame`].
pub(crate) struct QsvGpuInput {
    device: ID3D11Device,
    /// `AV_HWDEVICE_TYPE_D3D11VA`, wrapping `device`.
    d3d11: *mut ff::AVBufferRef,
    /// `AV_HWDEVICE_TYPE_QSV`, derived from `d3d11`.
    qsv: *mut ff::AVBufferRef,
    /// The QSV frames context the encoder's `hw_frames_ctx` refers to.
    frames: *mut ff::AVBufferRef,
    converter: Converter,
    width: u32,
    height: u32,
    /// Whether a pool slice is the converter's output size, so the copy takes
    /// the whole subresource (checked on the first frame).
    whole_slice: Option<bool>,
}

// SAFETY: the FFmpeg contexts are reference-counted and their owner (one
// encoder) uses them from one thread at a time; the D3D11 device is
// free-threaded and its immediate context multithread-protected.
unsafe impl Send for QsvGpuInput {}

impl QsvGpuInput {
    /// The pool on `device` for a `width × height` encoder. Fails, and leaves
    /// nothing behind, when the device has no QSV (any adapter that is not
    /// Intel's: `adapter_mismatch`) or no BGRA→NV12 video processor.
    pub(crate) fn open(device: &ID3D11Device, width: u32, height: u32) -> Result<Self, Refused> {
        let failed = |e: anyhow::Error| Refused {
            reason: "open_failed",
            detail: Some(e.to_string()),
        };
        // The converter writes into a texture of the pool slices' size: FFmpeg
        // makes them 16-aligned (`qsv_init_child_ctx`), and equal sizes make the
        // copy into a slice a whole subresource.
        let aligned = (width.next_multiple_of(16), height.next_multiple_of(16));
        let converter = Converter::new(device, width, height, aligned).map_err(failed)?;
        Self::open_with(device, converter, width, height).map_err(|e| {
            if e.to_string().starts_with(NO_QSV) {
                Refused {
                    reason: "adapter_mismatch",
                    detail: Some(e.to_string()),
                }
            } else {
                failed(e)
            }
        })
    }

    fn open_with(
        device: &ID3D11Device,
        converter: Converter,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let mut this = Self {
            device: device.clone(),
            d3d11: std::ptr::null_mut(),
            qsv: std::ptr::null_mut(),
            frames: std::ptr::null_mut(),
            converter,
            width,
            height,
            whole_slice: None,
        };
        // SAFETY: each FFmpeg call gets the buffers it documents; a failure
        // returns with the ones made so far owned by `this`, whose Drop
        // unrefs them.
        unsafe {
            this.d3d11 = ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if this.d3d11.is_null() {
                return Err(anyhow!("av_hwdevice_ctx_alloc(d3d11va) failed"));
            }
            let hw = (*this.d3d11).data as *mut ff::AVHWDeviceContext;
            let d3d = (*hw).hwctx as *mut ff::AVD3D11VADeviceContext;
            // FFmpeg releases the device when the context goes, so it gets a
            // reference of its own.
            (*d3d).device = device.clone().into_raw() as *mut ff::ID3D11Device;
            let rc = ff::av_hwdevice_ctx_init(this.d3d11);
            if rc < 0 {
                return Err(anyhow!("av_hwdevice_ctx_init(d3d11va): {}", ff_err(rc)));
            }
            let rc = ff::av_hwdevice_ctx_create_derived(
                &mut this.qsv,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV,
                this.d3d11,
                0,
            );
            if rc < 0 || this.qsv.is_null() {
                return Err(anyhow!("{NO_QSV}: {}", ff_err(rc)));
            }
            this.frames = ff::av_hwframe_ctx_alloc(this.qsv);
            if this.frames.is_null() {
                return Err(anyhow!("av_hwframe_ctx_alloc(qsv) failed"));
            }
            let ctx = (*this.frames).data as *mut ff::AVHWFramesContext;
            (*ctx).format = ff::AVPixelFormat::AV_PIX_FMT_QSV;
            (*ctx).sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
            (*ctx).width = width as i32;
            (*ctx).height = height as i32;
            (*ctx).initial_pool_size = POOL_SIZE;
            let qsv_frames = (*ctx).hwctx as *mut ff::AVQSVFramesContext;
            (*qsv_frames).frame_type = MFX_MEMTYPE_VIDEO_MEMORY_DECODER_TARGET;
            let rc = ff::av_hwframe_ctx_init(this.frames);
            if rc < 0 {
                return Err(anyhow!("av_hwframe_ctx_init(qsv): {}", ff_err(rc)));
            }
        }
        Ok(this)
    }

    /// The frame pool itself, for the encoder's configure step to take a NEW
    /// reference to (`av_buffer_ref`) per open attempt: a rejected codec
    /// context frees the one it was given.
    pub(crate) fn frames_raw(&self) -> *mut ff::AVBufferRef {
        self.frames
    }

    /// The capture device this pool lives on.
    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Whether `device` is the one this pool lives on. A capture backend
    /// rebuilds its device after a desktop switch, and a frame from another
    /// device cannot enter this pool.
    pub(crate) fn same_device(&self, device: &ID3D11Device) -> bool {
        self.device.as_raw() == device.as_raw()
    }

    /// A GPU frame as a QSV frame for `send_frame`: converted to NV12 by the
    /// VideoProcessor, then copied into a pool frame's slice. Nothing touches
    /// CPU memory. The pts and picture type are the caller's to stamp.
    pub(crate) fn convert_gpu_frame(&mut self, gpu: &GpuFrame) -> Result<frame::Video> {
        if !self.same_device(gpu.device()) {
            return Err(anyhow!("the frame is on another D3D11 device"));
        }
        if gpu.dims() != (self.width, self.height) {
            return Err(anyhow!(
                "a {:?} frame for a {}x{} pool",
                gpu.dims(),
                self.width,
                self.height
            ));
        }
        self.converter.convert(gpu.texture())?;
        let mut qsv = frame::Video::empty();
        // SAFETY: `qsv` is an empty frame the pool fills; mapping it to the
        // pool's child type (D3D11) yields the texture in data[0] and the
        // array slice in data[1] (`qsv_map_from`), both live while `d3d` is.
        // The copy is two NV12 textures on this device: the converter's
        // output, and a slice of the pool's array texture at least as large
        // (FFmpeg aligns the pool's dims up to 16).
        unsafe {
            let rc = ff::av_hwframe_get_buffer(self.frames, qsv.as_mut_ptr(), 0);
            if rc < 0 {
                return Err(anyhow!("av_hwframe_get_buffer(qsv): {}", ff_err(rc)));
            }
            let mut d3d = frame::Video::empty();
            (*d3d.as_mut_ptr()).format = ff::AVPixelFormat::AV_PIX_FMT_D3D11 as i32;
            let flags = (ff::AV_HWFRAME_MAP_WRITE as i32) | (ff::AV_HWFRAME_MAP_OVERWRITE as i32);
            let rc = ff::av_hwframe_map(d3d.as_mut_ptr(), qsv.as_ptr(), flags);
            if rc < 0 {
                return Err(anyhow!("av_hwframe_map(qsv → d3d11): {}", ff_err(rc)));
            }
            let raw = (*d3d.as_ptr()).data[0] as *mut c_void;
            let slice = (*d3d.as_ptr()).data[1] as usize as u32;
            let texture = ID3D11Texture2D::from_raw_borrowed(&raw)
                .ok_or_else(|| anyhow!("the pool frame has no texture"))?;
            let context = self
                .device
                .GetImmediateContext()
                .map_err(|e| anyhow!("GetImmediateContext: {e}"))?;
            // A slice the converter's size takes the whole subresource; one of
            // another size (an FFmpeg that aligns differently) takes the
            // picture's box. Checked on the first frame, logged if it differs.
            let whole = *self.whole_slice.get_or_insert_with(|| {
                let mut slice_desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut slice_desc);
                let mut out_desc = D3D11_TEXTURE2D_DESC::default();
                self.converter.output().GetDesc(&mut out_desc);
                let same = (slice_desc.Width, slice_desc.Height) == (out_desc.Width, out_desc.Height);
                if !same {
                    tracing::info!(
                        slice_w = slice_desc.Width,
                        slice_h = slice_desc.Height,
                        out_w = out_desc.Width,
                        out_h = out_desc.Height,
                        "FR-118: the QSV pool's slices are not the converter's size — copying the picture's box"
                    );
                }
                same
            });
            let area = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: self.width,
                bottom: self.height,
                back: 1,
            };
            // An array slice of a one-mip texture is subresource `slice`.
            context.CopySubresourceRegion(
                texture,
                slice,
                0,
                0,
                0,
                self.converter.output(),
                0,
                (!whole).then_some(&area as *const D3D11_BOX),
            );
        }
        Ok(qsv)
    }

    /// A CPU frame (NV12, this pool's size, pts and picture type stamped) as a
    /// QSV frame: FFmpeg's upload into a pool frame. For the frames that still
    /// arrive in memory: the GDI fallback, the first-frame prime, a resample.
    pub(crate) fn upload(&self, sw: &frame::Video) -> Result<frame::Video> {
        let mut qsv = frame::Video::empty();
        // SAFETY: an empty frame the pool fills, a populated software frame.
        unsafe {
            let rc = ff::av_hwframe_get_buffer(self.frames, qsv.as_mut_ptr(), 0);
            if rc < 0 {
                return Err(anyhow!("av_hwframe_get_buffer(qsv): {}", ff_err(rc)));
            }
            let rc = ff::av_hwframe_transfer_data(qsv.as_mut_ptr(), sw.as_ptr(), 0);
            if rc < 0 {
                return Err(anyhow!("av_hwframe_transfer_data(→ qsv): {}", ff_err(rc)));
            }
            let rc = ff::av_frame_copy_props(qsv.as_mut_ptr(), sw.as_ptr());
            if rc < 0 {
                return Err(anyhow!("av_frame_copy_props: {}", ff_err(rc)));
            }
        }
        Ok(qsv)
    }
}

impl Drop for QsvGpuInput {
    fn drop(&mut self) {
        // SAFETY: each buffer is ours, once; unref of a null one is a no-op.
        // The frame pool goes first: it holds references to both devices.
        unsafe {
            ff::av_buffer_unref(&mut self.frames);
            ff::av_buffer_unref(&mut self.qsv);
            ff::av_buffer_unref(&mut self.d3d11);
        }
    }
}

/// `CheckVideoProcessorFormat`'s answer bits: the format can be an input, an
/// output (`D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT` / `_OUTPUT`).
const FORMAT_INPUT: u32 = 0x1;
const FORMAT_OUTPUT: u32 = 0x2;

/// How many input views [`Converter`] keeps. A capture pool lends out a
/// handful of textures (FR-118 P2-1 keeps at most 6), so their views are made
/// once each.
const INPUT_VIEWS_KEPT: usize = 8;

/// One D3D11 VideoProcessor that turns a BGRA texture into an NV12 texture
/// of the same size, BT.601 limited range, on the device it was made for.
pub(crate) struct Converter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    venum: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    /// The NV12 result: `DEFAULT`, bound as a render target (a processor
    /// output must be). The picture is its top-left `width × height`; the
    /// texture itself may be larger (a QSV pool slice's 16-aligned size, so the
    /// copy into the slice is a whole subresource).
    out: ID3D11Texture2D,
    out_view: ID3D11VideoProcessorOutputView,
    /// Input views by texture, newest last. The texture is kept with its view
    /// so its address cannot be reused by another texture while it is here.
    inputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
}

impl Converter {
    /// A converter for `width × height` frames, writing into an NV12 texture of
    /// `out` dims (at least the frame's; the picture goes top-left).
    pub(crate) fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        out: (u32, u32),
    ) -> Result<Self> {
        let (out_width, out_height) = (out.0.max(width), out.1.max(height));
        // SAFETY: plain COM calls and QueryInterfaces on a live device, with
        // fully initialised descriptors and valid out-params.
        unsafe {
            let vdev: ID3D11VideoDevice = device
                .cast()
                .map_err(|e| anyhow!("ID3D11VideoDevice: {e}"))?;
            let context = device
                .GetImmediateContext()
                .map_err(|e| anyhow!("GetImmediateContext: {e}"))?;
            let vctx: ID3D11VideoContext = context
                .cast()
                .map_err(|e| anyhow!("ID3D11VideoContext: {e}"))?;
            let rate = DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: rate,
                OutputWidth: out_width,
                OutputHeight: out_height,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let venum = vdev
                .CreateVideoProcessorEnumerator(&content)
                .map_err(|e| anyhow!("CreateVideoProcessorEnumerator: {e}"))?;
            let bgra = venum
                .CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)
                .map_err(|e| anyhow!("CheckVideoProcessorFormat(BGRA): {e}"))?;
            let nv12 = venum
                .CheckVideoProcessorFormat(DXGI_FORMAT_NV12)
                .map_err(|e| anyhow!("CheckVideoProcessorFormat(NV12): {e}"))?;
            if bgra & FORMAT_INPUT == 0 || nv12 & FORMAT_OUTPUT == 0 {
                return Err(anyhow!(
                    "the video processor cannot convert BGRA to NV12 (BGRA {bgra:#x}, NV12 {nv12:#x})"
                ));
            }
            let vp = vdev
                .CreateVideoProcessor(&venum, 0)
                .map_err(|e| anyhow!("CreateVideoProcessor: {e}"))?;

            // No driver "enhancement" (Intel's adaptive contrast and denoise
            // change screen content), progressive frames, the whole frame in,
            // unscaled, at the output's top-left; nothing outside it is written.
            let rect = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&rect));
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&rect));
            vctx.VideoProcessorSetOutputTargetRect(&vp, true, Some(&rect));
            // Full-range RGB in, BT.601 studio-range YCbCr out: the CPU path's
            // matrix. `ID3D11VideoContext1` names the spaces exactly; the
            // older call says the same with bits (output: BT.601 matrix,
            // nominal range 16–235; input: full-range RGB).
            match vctx.cast::<ID3D11VideoContext1>() {
                Ok(vctx1) => {
                    vctx1.VideoProcessorSetStreamColorSpace1(
                        &vp,
                        0,
                        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                    );
                    vctx1.VideoProcessorSetOutputColorSpace1(
                        &vp,
                        DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
                    );
                }
                Err(_) => {
                    let input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                    // Nominal_Range (bits 4–5) = 16_235; YCbCr_Matrix (bit 2) = 0, BT.601.
                    let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 };
                    vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &input);
                    vctx.VideoProcessorSetOutputColorSpace(&vp, &output);
                }
            }

            let desc = D3D11_TEXTURE2D_DESC {
                Width: out_width,
                Height: out_height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut out: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut out))
                .map_err(|e| anyhow!("CreateTexture2D(NV12 {width}x{height}): {e}"))?;
            let out = out.ok_or_else(|| anyhow!("CreateTexture2D returned no texture"))?;
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
            vdev.CreateVideoProcessorOutputView(&out, &venum, &view_desc, Some(&mut out_view))
                .map_err(|e| anyhow!("CreateVideoProcessorOutputView: {e}"))?;
            let out_view =
                out_view.ok_or_else(|| anyhow!("CreateVideoProcessorOutputView returned none"))?;
            Ok(Self {
                vdev,
                vctx,
                venum,
                vp,
                out,
                out_view,
                inputs: Vec::new(),
            })
        }
    }

    /// The NV12 texture [`Self::convert`] writes.
    pub(crate) fn output(&self) -> &ID3D11Texture2D {
        &self.out
    }

    /// Convert `src` (BGRA, this converter's size, on its device) into
    /// [`Self::output`]. Queued on the GPU like any other command: whatever
    /// reads the output later on the same device sees it done.
    pub(crate) fn convert(&mut self, src: &ID3D11Texture2D) -> Result<()> {
        let view = self.input_view(src)?;
        // ⚠ windows-rs 0.58: `pInputSurface` is `ManuallyDrop<Option<…>>`, so
        // the reference placed here is reclaimed after the call, win or lose,
        // or one view leaks per frame (the DXGI arm's VP has the same note).
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        }];
        // SAFETY: the processor, output view and stream are valid for the call.
        let blt = unsafe {
            self.vctx
                .VideoProcessorBlt(&self.vp, &self.out_view, 0, &streams)
        };
        // SAFETY: reclaiming the reference placed above, exactly once.
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        blt.map_err(|e| anyhow!("VideoProcessorBlt: {e}"))
    }

    fn input_view(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        if let Some((_, view)) = self.inputs.iter().find(|(t, _)| t.as_raw() == src.as_raw()) {
            return Ok(view.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut view: Option<ID3D11VideoProcessorInputView> = None;
        // SAFETY: a live texture and enumerator, a valid desc and out-param.
        unsafe {
            self.vdev
                .CreateVideoProcessorInputView(src, &self.venum, &desc, Some(&mut view))
        }
        .map_err(|e| anyhow!("CreateVideoProcessorInputView: {e}"))?;
        let view = view.ok_or_else(|| anyhow!("CreateVideoProcessorInputView returned none"))?;
        if self.inputs.len() >= INPUT_VIEWS_KEPT {
            self.inputs.remove(0);
        }
        self.inputs.push((src.clone(), view.clone()));
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_USAGE_STAGING,
        D3D11CreateDevice,
    };

    fn device(kind: D3D_DRIVER_TYPE) -> Option<ID3D11Device> {
        let mut device: Option<ID3D11Device> = None;
        // SAFETY: valid out-params; no adapter (the driver type picks one).
        unsafe {
            D3D11CreateDevice(
                None,
                kind,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .ok()?;
        device
    }

    /// Flat colour, eight 8-pixel bars on the top half (white, yellow, cyan,
    /// green, magenta, red, blue, black) and an eight-step grey ramp on the
    /// bottom: every patch is 8×16, so 2×2 chroma never straddles two colours
    /// except at a bar's edge.
    fn bars(width: u32, height: u32) -> Vec<u8> {
        const TOP: [[u8; 3]; 8] = [
            [255, 255, 255],
            [255, 255, 0],
            [0, 255, 255],
            [0, 255, 0],
            [255, 0, 255],
            [255, 0, 0],
            [0, 0, 255],
            [0, 0, 0],
        ];
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let bar = (x / 8 % 8) as usize;
                let [r, g, b] = if y < height / 2 {
                    TOP[bar]
                } else {
                    let v = (bar as u8) * 36;
                    [v, v, v]
                };
                bytes.extend_from_slice(&[b, g, r, 0xff]);
            }
        }
        bytes
    }

    fn texture(device: &ID3D11Device, width: u32, height: u32, bgra: &[u8]) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: a valid desc and out-param; `bgra` holds `height` rows of
        // `width × 4` bytes.
        unsafe {
            device.CreateTexture2D(&desc, None, Some(&mut tex)).unwrap();
            let tex = tex.as_ref().unwrap();
            let context = device.GetImmediateContext().unwrap();
            context.UpdateSubresource(tex, 0, None, bgra.as_ptr().cast(), width * 4, 0);
        }
        tex.unwrap()
    }

    /// The top-left `width × height` of an NV12 texture of `tex_dims`, in CPU
    /// memory: Y (`width × height`), then the interleaved UV plane
    /// (`width × height / 2`).
    fn read_nv12(
        device: &ID3D11Device,
        tex: &ID3D11Texture2D,
        tex_dims: (u32, u32),
        width: u32,
        height: u32,
    ) -> (Vec<u8>, Vec<u8>) {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: tex_dims.0,
            Height: tex_dims.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let (w, h) = (width as usize, height as usize);
        let tex_h = tex_dims.1 as usize;
        let mut y = vec![0u8; w * h];
        let mut uv = vec![0u8; w * h / 2];
        // SAFETY: a staging texture of the same format and dims; the mapped
        // NV12 layout is the Y plane's texture-height rows then the UV plane's
        // half as many, both `RowPitch` apart, and the copies take the
        // top-left `w` bytes of the first `h` (and `h / 2`) rows.
        unsafe {
            let mut staging: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
            let staging = staging.unwrap();
            let context = device.GetImmediateContext().unwrap();
            context.CopyResource(&staging, tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            for row in 0..h {
                std::ptr::copy_nonoverlapping(
                    base.add(row * pitch),
                    y.as_mut_ptr().add(row * w),
                    w,
                );
            }
            for row in 0..h / 2 {
                std::ptr::copy_nonoverlapping(
                    base.add((tex_h + row) * pitch),
                    uv.as_mut_ptr().add(row * w),
                    w,
                );
            }
            context.Unmap(&staging, 0);
        }
        (y, uv)
    }

    /// The CPU path's conversion: dcv, BGRA to NV12, BT.601 (limited range).
    fn cpu_nv12(bgra: &[u8], width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
        use dcv_color_primitives::{ColorSpace, ImageFormat, PixelFormat, convert_image};
        let (w, h) = (width as usize, height as usize);
        let mut y = vec![0u8; w * h];
        let mut uv = vec![0u8; w * h / 2];
        let src = ImageFormat {
            pixel_format: PixelFormat::Bgra,
            color_space: ColorSpace::Rgb,
            num_planes: 1,
        };
        let dst = ImageFormat {
            pixel_format: PixelFormat::Nv12,
            color_space: ColorSpace::Bt601,
            num_planes: 2,
        };
        convert_image(
            width,
            height,
            &src,
            Some(&[w * 4]),
            &[bgra],
            &dst,
            Some(&[w, w]),
            &mut [&mut y[..], &mut uv[..]],
        )
        .unwrap();
        (y, uv)
    }

    /// The largest difference between two planes of `width` bytes a row,
    /// leaving out the `edge` bytes at each side of every 8-byte patch. A bar
    /// is 8 pixels: 8 luma bytes, and 4 UV pairs (8 bytes) of chroma, whose
    /// outer pairs the two paths' chroma siting may filter differently (the
    /// GPU's is left-sited, dcv averages each 2×2).
    fn max_diff(a: &[u8], b: &[u8], width: usize, rows: usize, edge: usize) -> u8 {
        let mut worst = 0u8;
        for row in 0..rows {
            for x in 0..width {
                let at = x % 8;
                if at < edge || at >= 8 - edge {
                    continue;
                }
                let i = row * width + x;
                worst = worst.max(a[i].abs_diff(b[i]));
            }
        }
        worst
    }

    /// Every hardware adapter's device, with its name, then WARP's.
    fn devices() -> Vec<(String, ID3D11Device)> {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
        let mut out = Vec::new();
        // SAFETY: plain DXGI enumeration; each device is made on its adapter
        // with valid out-params.
        unsafe {
            if let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() {
                let mut i = 0;
                while let Ok(adapter) = factory.EnumAdapters1(i) {
                    i += 1;
                    let adapter: IDXGIAdapter1 = adapter;
                    let name = adapter
                        .GetDesc1()
                        .map(|d| {
                            String::from_utf16_lossy(&d.Description)
                                .trim_end_matches('\0')
                                .to_string()
                        })
                        .unwrap_or_default();
                    let mut device: Option<ID3D11Device> = None;
                    let made = D3D11CreateDevice(
                        &adapter,
                        windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                        Some(&[D3D_FEATURE_LEVEL_11_0]),
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        None,
                    );
                    if let (Ok(()), Some(device)) = (made, device) {
                        out.push((name, device));
                    }
                }
            }
        }
        if let Some(warp) = device(D3D_DRIVER_TYPE_WARP) {
            out.push(("WARP".to_string(), warp));
        }
        out
    }

    /// The GPU conversion gives the CPU path's colours, ±2 code values in
    /// every plane, on flat patches, on every device here that has a
    /// BGRA→NV12 video processor. WARP has none, so on a CI runner (no GPU)
    /// there is nothing to compare and the test says so; the comparison runs
    /// on real hardware.
    #[test]
    fn the_gpu_conversion_matches_the_cpu_path() {
        // A 16-aligned frame in a texture its size, and a frame of other dims
        // in a larger, 16-aligned texture, as a QSV pool slice is: the picture
        // must land top-left, unscaled. Both keep the pattern's top/bottom
        // boundary on an even row: where two colours meet INSIDE a 2×2 chroma
        // block vertically, a VideoProcessor may take one row's chroma rather
        // than dcv's average (measured: AMD, 56 code values at a blue bar over
        // grey, on that one chroma row only), and flat colour is what this
        // holds the paths to.
        let cases = [((64u32, 32u32), (64u32, 32u32)), ((72, 36), (80, 48))];
        let mut compared = 0;
        for ((width, height), out) in cases {
            let bgra = bars(width, height);
            let (cpu_y, cpu_uv) = cpu_nv12(&bgra, width, height);
            let (w, h) = (width as usize, height as usize);
            for (name, device) in devices() {
                let mut converter = match Converter::new(&device, width, height, out) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("{name}: no conversion ({e})");
                        continue;
                    }
                };
                let src = texture(&device, width, height, &bgra);
                converter.convert(&src).unwrap();
                let (gpu_y, gpu_uv) = read_nv12(&device, converter.output(), out, width, height);
                // Luma: every sample (no siting question at full resolution).
                let y = max_diff(&gpu_y, &cpu_y, w, h, 0);
                // Chroma: leave out the UV pair at each side of every bar.
                let uv = max_diff(&gpu_uv, &cpu_uv, w, h / 2, 2);
                eprintln!("{name} {width}x{height} in {out:?}: max |GPU - CPU| Y {y}, UV {uv}");
                assert!(y <= 2, "{name}: luma differs by {y} code values");
                assert!(uv <= 2, "{name}: chroma differs by {uv} code values");
                compared += 1;
            }
        }
        if compared == 0 {
            eprintln!("no device here has a BGRA→NV12 video processor — nothing to compare");
        }
    }

    /// `plan` reads its gates at the open, in order: the switch, the device the
    /// pump named, the cell, the denylist, the probe's proof. No probe runs in a
    /// test process, so a cell past the denylist stops at `unproven`.
    #[test]
    fn plan_reads_every_gate_at_the_open() {
        use super::super::gpu_hint::opening_on;
        use tunnel_core::env::test_env::{Saved, set};
        // The lock every test that sets these knobs (or the cell denylist) takes.
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _switch = Saved::cleared("ZERO_COPY");
        let _deny = Saved::cleared("ZERO_COPY_DENY");
        let mut warp: Option<ID3D11Device> = None;
        // SAFETY: valid out-params; WARP needs no adapter. No VIDEO_SUPPORT:
        // WARP refuses it, and the plan's gates come before any video call.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut warp),
                None,
                None,
            )
        }
        .expect("a WARP device");
        let warp = warp.expect("a WARP device");
        let reason = |named: Option<&ID3D11Device>, name: &str, chroma444: bool| match opening_on(
            named.cloned(),
            || plan(name, chroma444, 64, 32),
        ) {
            Ok(_) => "gpu",
            Err(refused) => refused.reason,
        };
        assert_eq!(reason(Some(&warp), "hevc_qsv", false), "switch_off");
        // SAFETY: serialised by LOCK; the guards restore both on drop.
        unsafe { set("ZERO_COPY", "1") };
        assert_eq!(reason(None, "hevc_qsv", false), "cpu_frames");
        assert_eq!(reason(Some(&warp), "hevc_amf", false), "no_gpu_cell");
        assert_eq!(reason(Some(&warp), "vp9_qsv", false), "no_gpu_cell");
        assert_eq!(reason(Some(&warp), "hevc_qsv", true), "no_gpu_cell");
        assert_eq!(reason(Some(&warp), "hevc_nvenc", true), "no_gpu_cell");
        // SAFETY: as above.
        unsafe { set("ZERO_COPY_DENY", "hevc_qsv:gpu") };
        assert_eq!(reason(Some(&warp), "hevc_qsv", false), "denied");
        // FR-118 P2-3 — no probe has proven a form in this process.
        assert_eq!(reason(Some(&warp), "av1_qsv", false), "unproven");
        // SAFETY: as above.
        unsafe { set("ZERO_COPY_DENY", "none") };
        assert_eq!(reason(Some(&warp), "hevc_qsv", false), "unproven");
        // FR-118 P3 — NVENC's 4:2:0 cells are GPU cells, behind the same gates.
        assert_eq!(reason(Some(&warp), "hevc_nvenc", false), "unproven");
        // SAFETY: as above.
        unsafe { set("ZERO_COPY_DENY", "av1_nvenc:gpu") };
        assert_eq!(reason(Some(&warp), "av1_nvenc", false), "denied");
        // The pools themselves, the gate after the proof, have their own tests.
    }

    /// FR-118 P3 — the NVENC pool opens only on an NVIDIA device; elsewhere it
    /// refuses with `adapter_mismatch` before making anything. Where it opens,
    /// a GPU frame and a CPU frame both become D3D11 frames of the pool, and
    /// the GPU frame's NV12 is the converter's: the pool slice holds the
    /// picture the conversion made.
    #[test]
    fn the_nvenc_pool_opens_on_nvidia_and_refuses_elsewhere() {
        use crate::capture::gpu_frame::TexturePool;
        let (width, height) = (64u32, 32u32);
        let mut opened = 0;
        for (name, device) in devices() {
            let mut input = match NvencGpuInput::open(&device, width, height) {
                Ok(input) => input,
                Err(e) => {
                    eprintln!("{name}: no NVENC pool ({}: {:?})", e.reason, e.detail);
                    let nvidia = adapter_vendor(&device).ok() == Some(NVIDIA);
                    if nvidia {
                        panic!("{name}: an NVIDIA device refused the pool: {e:?}");
                    }
                    assert_eq!(e.reason, "adapter_mismatch", "{name}: {e:?}");
                    continue;
                }
            };
            // The D3D11 pool itself would open on any adapter; the refusal off
            // NVIDIA is the adapter rule's, and nothing else catches it before
            // NVENC's session fails in the encoder's open.
            assert_eq!(
                adapter_vendor(&device).ok(),
                Some(NVIDIA),
                "{name}: a pool opened on a device that is not NVIDIA's"
            );
            let pool = TexturePool::new(&device).unwrap();
            let bgra = bars(width, height);
            let texture = pool.take(width, height).unwrap();
            // SAFETY: `bgra` holds `height` rows of `width × 4` bytes.
            unsafe {
                device.GetImmediateContext().unwrap().UpdateSubresource(
                    &texture,
                    0,
                    None,
                    bgra.as_ptr().cast(),
                    width * 4,
                    0,
                );
            }
            let gpu = pool.frame(texture, width, height);
            let from_gpu = input.convert_gpu_frame(&gpu).unwrap();
            let mut sw = frame::Video::new(ffmpeg_next::format::Pixel::NV12, width, height);
            for plane in 0..2 {
                sw.data_mut(plane).fill(0x80);
            }
            let uploaded = upload_into(input.frames, &sw).unwrap();
            for f in [&from_gpu, &uploaded] {
                // SAFETY: reading a field of a valid frame.
                let format = unsafe { (*f.as_ptr()).format };
                assert_eq!(format, ff::AVPixelFormat::AV_PIX_FMT_D3D11 as i32);
            }
            // The pool slice holds the converter's picture: read the slice back
            // and compare it with the converter's own output.
            // SAFETY: the frame's data[0] is the pool's array texture, data[1]
            // its slice; both live while `from_gpu` is.
            let (slice_y, slice_uv) = unsafe {
                let raw = (*from_gpu.as_ptr()).data[0] as *mut c_void;
                let slice = (*from_gpu.as_ptr()).data[1] as usize as u32;
                let array = ID3D11Texture2D::from_raw_borrowed(&raw).unwrap();
                let single = texture_nv12(&device, width, height);
                let context = device.GetImmediateContext().unwrap();
                context.CopySubresourceRegion(&single, 0, 0, 0, 0, array, slice, None);
                read_nv12(&device, &single, (width, height), width, height)
            };
            let (out_y, out_uv) = read_nv12(
                &device,
                input.converter.output(),
                (width, height),
                width,
                height,
            );
            assert_eq!(
                slice_y, out_y,
                "{name}: the slice's luma is the converter's"
            );
            assert_eq!(
                slice_uv, out_uv,
                "{name}: the slice's chroma is the converter's"
            );
            eprintln!("{name}: NVENC pool opened; a GPU frame and a CPU frame both entered it");
            opened += 1;
        }
        if opened == 0 {
            eprintln!("no NVIDIA device here — the refusals above are the test");
        }
    }

    /// An NV12 `DEFAULT` texture, a copy target for reading one pool slice.
    fn texture_nv12(device: &ID3D11Device, width: u32, height: u32) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: a valid desc and out-param.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)).unwrap() };
        tex.unwrap()
    }

    /// The QSV pool opens only where QSV runs (an Intel adapter); elsewhere
    /// the open fails with a reason and releases what it had made (the D3D11VA
    /// context holds a device reference of its own). Where it opens, a GPU
    /// frame and a CPU frame both become QSV frames of the pool.
    #[test]
    fn the_qsv_pool_opens_where_qsv_runs_and_fails_cleanly_elsewhere() {
        use crate::capture::gpu_frame::TexturePool;
        let (width, height) = (64u32, 32u32);
        for (name, device) in devices() {
            let mut input = match QsvGpuInput::open(&device, width, height) {
                Ok(input) => input,
                Err(e) => {
                    eprintln!("{name}: no QSV pool ({}: {:?})", e.reason, e.detail);
                    assert!(
                        matches!(e.reason, "adapter_mismatch" | "open_failed"),
                        "{name}: {e:?}"
                    );
                    continue;
                }
            };
            let pool = TexturePool::new(&device).unwrap();
            let bgra = bars(width, height);
            let texture = pool.take(width, height).unwrap();
            // SAFETY: `bgra` holds `height` rows of `width × 4` bytes.
            unsafe {
                device.GetImmediateContext().unwrap().UpdateSubresource(
                    &texture,
                    0,
                    None,
                    bgra.as_ptr().cast(),
                    width * 4,
                    0,
                );
            }
            let gpu = pool.frame(texture, width, height);
            let from_gpu = input.convert_gpu_frame(&gpu).unwrap();
            let mut sw = frame::Video::new(ffmpeg_next::format::Pixel::NV12, width, height);
            for plane in 0..2 {
                sw.data_mut(plane).fill(0x80);
            }
            let uploaded = input.upload(&sw).unwrap();
            for f in [&from_gpu, &uploaded] {
                // SAFETY: reading a field of a valid frame.
                let format = unsafe { (*f.as_ptr()).format };
                assert_eq!(format, ff::AVPixelFormat::AV_PIX_FMT_QSV as i32);
            }
            eprintln!("{name}: QSV pool opened; a GPU frame and a CPU frame both entered it");
        }
    }
}
