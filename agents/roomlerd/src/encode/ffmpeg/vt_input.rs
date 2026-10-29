// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! FR-118 P5 — VideoToolbox takes the capture's IOSurface.
//!
//! A `*_videotoolbox` encoder opened in its GPU form (`AV_PIX_FMT_VIDEOTOOLBOX`,
//! no frame pool) takes every frame as a `CVPixelBuffer`, in `data[3]`.
//! FFmpeg 9.0.1's encoder hands that buffer to the compression session as it is
//! (`create_cv_pixel_buffer`, `videotoolboxenc.c`), and VideoToolbox converts the
//! BGRA itself:
//!
//! - an IOSurface frame is wrapped as it is (`CVPixelBufferCreateWithIOSurface`):
//!   no copy into memory, no CPU conversion, no copy into VideoToolbox's buffer;
//! - a frame that arrives in memory (the first-frame prime, a CPU-downscaled
//!   frame) is copied into a buffer from this input's pool, the copy FFmpeg
//!   makes for every frame on the CPU form.
//!
//! The frame's `AVBufferRef` holds the `CVPixelBuffer`, and for an IOSurface
//! frame the frame itself, so the surface's use count is held until the encoder
//! lets go of it: `CGDisplayStream` cannot draw into a surface VideoToolbox is
//! still reading.
//!
//! Every gate is read at the open, as `gpu_input::plan` reads them on Windows:
//! the switch, the cell, both denylists, and the probe child's proof.

use std::ffi::c_void;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use ffmpeg_next::{ffi as ff, frame};

use crate::capture::iosurface_frame::IoSurfaceFrame;

type CfTypeRef = *const c_void;
type CvPixelBufferRef = *mut c_void;
type CvPixelBufferPoolRef = *mut c_void;

/// `kCVPixelFormatType_32BGRA`, `'BGRA'`.
const PIXEL_FORMAT_BGRA: i32 = 0x4247_5241;
/// `kCFNumberSInt32Type`.
const CF_NUMBER_SINT32: i32 = 3;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CfTypeRef;
    static kCVPixelBufferWidthKey: CfTypeRef;
    static kCVPixelBufferHeightKey: CfTypeRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CfTypeRef;
    fn CVPixelBufferCreateWithIOSurface(
        allocator: CfTypeRef,
        surface: *mut c_void,
        attributes: CfTypeRef,
        out: *mut CvPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferPoolCreate(
        allocator: CfTypeRef,
        pool_attributes: CfTypeRef,
        buffer_attributes: CfTypeRef,
        out: *mut CvPixelBufferPoolRef,
    ) -> i32;
    fn CVPixelBufferPoolCreatePixelBuffer(
        allocator: CfTypeRef,
        pool: CvPixelBufferPoolRef,
        out: *mut CvPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(buffer: CvPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CvPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: CvPixelBufferRef) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: CvPixelBufferRef) -> usize;
    static kCVImageBufferYCbCrMatrixKey: CfTypeRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_601_4: CfTypeRef;
    fn CVBufferSetAttachment(buffer: CfTypeRef, key: CfTypeRef, value: CfTypeRef, mode: u32);
}

/// `kCVAttachmentMode_ShouldPropagate`.
const ATTACHMENT_SHOULD_PROPAGATE: u32 = 1;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFRelease(cf: CfTypeRef);
    fn CFNumberCreate(allocator: CfTypeRef, kind: i32, value: *const c_void) -> CfTypeRef;
    fn CFDictionaryCreate(
        allocator: CfTypeRef,
        keys: *const CfTypeRef,
        values: *const CfTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CfTypeRef;
}

/// The cells whose GPU form P5 opens: VideoToolbox's 4:2:0 HEVC and H.264.
/// (`av1_videotoolbox` opens on no shipped Mac.)
pub(crate) fn gpu_cell(name: &str, chroma444: bool) -> bool {
    !chroma444 && matches!(name, "hevc_videotoolbox" | "h264_videotoolbox")
}

/// The encoders whose GPU form the capability probe proved in its child
/// process. `caps::detect` installs them once, before any session.
static PROVEN: OnceLock<Vec<String>> = OnceLock::new();

pub(crate) fn install_proven(names: Vec<String>) {
    let _ = PROVEN.set(names);
}

/// Whether `name`'s GPU form may open in this process: the probe child proved
/// it, or this process IS the probe child, whose job is proving it.
fn proven(name: &str) -> bool {
    #[cfg(test)]
    if let Some(proven) = TEST_PROVEN.with(|p| p.borrow().clone()) {
        return proven.iter().any(|n| n == name);
    }
    crate::encode::caps::in_probe_child()
        || PROVEN
            .get()
            .is_some_and(|names| names.iter().any(|n| n == name))
}

#[cfg(test)]
thread_local! {
    /// A test's own proof, for its thread only: `PROVEN` is set once per
    /// process, and a test that installed it would decide every other test's
    /// `unproven`.
    static TEST_PROVEN: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// Run `f` with `names` proven on this thread, as if the probe child had
/// proven them.
#[cfg(test)]
pub(crate) fn with_proven<T>(names: &[&str], f: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_PROVEN.with(|p| *p.borrow_mut() = None);
        }
    }
    TEST_PROVEN.with(|p| *p.borrow_mut() = Some(names.iter().map(|n| n.to_string()).collect()));
    let _reset = Reset;
    f()
}

/// `Ok` when `name` opens in its GPU form, or the reason it takes CPU frames
/// (the heartbeat's `input_reason`). Every gate is read here, at the open.
pub(crate) fn plan(name: &str, chroma444: bool) -> std::result::Result<(), &'static str> {
    if !crate::capture::zero_copy_enabled() {
        return Err("switch_off");
    }
    if !gpu_cell(name, chroma444) {
        return Err("no_gpu_cell");
    }
    if super::gpu_hint::is_denied(&super::gpu_hint::denied(), name) {
        return Err("denied");
    }
    if !proven(name) {
        return Err("unproven");
    }
    Ok(())
}

/// A `CVPixelBufferPool` of BGRA, IOSurface-backed buffers at one size.
struct Pool(CvPixelBufferPoolRef);

// SAFETY: a CoreVideo pool is a thread-safe, reference-counted object; this
// type only creates buffers from it and releases it once.
unsafe impl Send for Pool {}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: the one reference `Pool::new` created.
        unsafe { CFRelease(self.0) };
    }
}

impl Pool {
    fn new(width: u32, height: u32) -> Result<Self> {
        // SAFETY: CoreFoundation creation calls on values that outlive them;
        // each object is released once, here, after the pool has its copy.
        unsafe {
            let num = |v: i32| {
                CFNumberCreate(
                    std::ptr::null(),
                    CF_NUMBER_SINT32,
                    (&v as *const i32).cast(),
                )
            };
            let empty = CFDictionaryCreate(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            );
            let keys = [
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferWidthKey,
                kCVPixelBufferHeightKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            ];
            let values = [
                num(PIXEL_FORMAT_BGRA),
                num(width as i32),
                num(height as i32),
                empty,
            ];
            let attributes = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                keys.len() as isize,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            );
            for v in values {
                CFRelease(v);
            }
            let mut pool = std::ptr::null_mut();
            let rc =
                CVPixelBufferPoolCreate(std::ptr::null(), std::ptr::null(), attributes, &mut pool);
            CFRelease(attributes);
            if rc != 0 || pool.is_null() {
                return Err(anyhow!("CVPixelBufferPoolCreate: {rc}"));
            }
            Ok(Self(pool))
        }
    }
}

/// A VideoToolbox encoder's GPU input: how a frame becomes the
/// `AV_PIX_FMT_VIDEOTOOLBOX` frame the encoder takes.
pub(crate) struct VtInput {
    width: u32,
    height: u32,
    /// For frames in memory; made on the first one.
    pool: Option<Pool>,
}

impl VtInput {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pool: None,
        }
    }

    /// An IOSurface frame as VideoToolbox takes it: the surface wrapped in a
    /// `CVPixelBuffer`, no copy. The returned frame holds `surface` until the
    /// encoder lets go of it.
    pub(crate) fn surface_frame(&self, surface: &IoSurfaceFrame) -> Result<frame::Video> {
        let mut buffer: CvPixelBufferRef = std::ptr::null_mut();
        // SAFETY: a live surface, held by `surface` for the call; the new
        // buffer retains it.
        let rc = unsafe {
            CVPixelBufferCreateWithIOSurface(
                std::ptr::null(),
                surface.surface(),
                std::ptr::null(),
                &mut buffer,
            )
        };
        if rc != 0 || buffer.is_null() {
            return Err(anyhow!("CVPixelBufferCreateWithIOSurface: {rc}"));
        }
        wrap(buffer, Some(surface.clone()), self.width, self.height)
    }

    /// A BGRA frame in memory, `stride` bytes a row, copied into a buffer from
    /// this input's pool.
    pub(crate) fn bytes_frame(&mut self, bytes: &[u8], stride: u32) -> Result<frame::Video> {
        let row = self.width as usize * 4;
        if (stride as usize) < row || bytes.len() < stride as usize * self.height as usize {
            return Err(anyhow!(
                "a {}x{} BGRA frame needs {} bytes a row and {} in all; got {} and {}",
                self.width,
                self.height,
                row,
                row * self.height as usize,
                stride,
                bytes.len()
            ));
        }
        if self.pool.is_none() {
            self.pool = Some(Pool::new(self.width, self.height)?);
        }
        let pool = self
            .pool
            .as_ref()
            .map(|p| p.0)
            .unwrap_or(std::ptr::null_mut());
        let mut buffer: CvPixelBufferRef = std::ptr::null_mut();
        // SAFETY: a live pool. The buffer's base address is written only
        // between its lock and unlock, row by row within both pitches.
        unsafe {
            let rc = CVPixelBufferPoolCreatePixelBuffer(std::ptr::null(), pool, &mut buffer);
            if rc != 0 || buffer.is_null() {
                return Err(anyhow!("CVPixelBufferPoolCreatePixelBuffer: {rc}"));
            }
            if CVPixelBufferLockBaseAddress(buffer, 0) != 0 {
                CFRelease(buffer);
                return Err(anyhow!("CVPixelBufferLockBaseAddress"));
            }
            let base = CVPixelBufferGetBaseAddress(buffer) as *mut u8;
            let pitch = CVPixelBufferGetBytesPerRow(buffer);
            if base.is_null() || pitch < row {
                CVPixelBufferUnlockBaseAddress(buffer, 0);
                CFRelease(buffer);
                return Err(anyhow!(
                    "the pool's buffer has no room for a {row}-byte row"
                ));
            }
            for y in 0..self.height as usize {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(y * stride as usize),
                    base.add(y * pitch),
                    row,
                );
            }
            CVPixelBufferUnlockBaseAddress(buffer, 0);
        }
        wrap(buffer, None, self.width, self.height)
    }
}

/// `buffer` (one reference, which this takes over) as an
/// `AV_PIX_FMT_VIDEOTOOLBOX` frame. `data[3]` is the buffer, where FFmpeg's
/// encoder reads it. `buf[0]` owns the reference, and `keep` with it, until
/// FFmpeg unrefs the frame.
///
/// The buffer is marked BT.601, so VideoToolbox converts its BGRA with the CPU
/// path's matrix (`encode/color.rs`) and the switch never changes the picture
/// (FR-118 open decision 3). Measured on the MacBook, macOS 27, 2026-10-29,
/// `h264_videotoolbox` on 75 % colour bars: unmarked, VideoToolbox converts
/// with BT.709 (green's Y 133 against the CPU path's 112). Marked, it converts
/// with BT.601, within 2 code values in every plane. Either way the stream
/// carries no colour description, as the CPU path's does not. The session's
/// own colour properties are no substitute: they tag the stream BT.601 and
/// still convert with BT.709.
fn wrap(
    buffer: CvPixelBufferRef,
    keep: Option<IoSurfaceFrame>,
    width: u32,
    height: u32,
) -> Result<frame::Video> {
    // SAFETY: a live buffer this function owns; CoreVideo retains the
    // attachment's value, a constant string.
    unsafe {
        CVBufferSetAttachment(
            buffer,
            kCVImageBufferYCbCrMatrixKey,
            kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            ATTACHMENT_SHOULD_PROPAGATE,
        )
    };
    let opaque = Box::into_raw(Box::new(keep)).cast::<c_void>();
    // SAFETY: `release` gives back exactly what this creates: the buffer's one
    // reference and the boxed frame.
    let buf = unsafe {
        ff::av_buffer_create(
            buffer.cast::<u8>(),
            std::mem::size_of::<CvPixelBufferRef>(),
            Some(release),
            opaque,
            0,
        )
    };
    if buf.is_null() {
        // SAFETY: nothing took ownership; give both back here.
        unsafe {
            CFRelease(buffer);
            drop(Box::from_raw(opaque.cast::<Option<IoSurfaceFrame>>()));
        }
        return Err(anyhow!("av_buffer_create"));
    }
    let mut out = frame::Video::empty();
    // SAFETY: an empty frame this function owns; the fields are FFmpeg's
    // documented layout for a VideoToolbox frame.
    unsafe {
        let f = out.as_mut_ptr();
        (*f).format = ff::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX as i32;
        (*f).width = width as i32;
        (*f).height = height as i32;
        (*f).data[3] = buffer.cast::<u8>();
        (*f).buf[0] = buf;
    }
    Ok(out)
}

unsafe extern "C" fn release(opaque: *mut c_void, data: *mut u8) {
    // SAFETY: called once, by FFmpeg, with what `wrap` created.
    unsafe {
        CFRelease(data.cast_const().cast::<c_void>());
        drop(Box::from_raw(opaque.cast::<Option<IoSurfaceFrame>>()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::iosurface_frame::new_bgra;
    use crate::capture::{Damage, Frame, FrameData, PixelFormat};
    use crate::encode::ffmpeg::FfmpegEncoder;
    use tunnel_core::env::test_env::{Saved, set};

    #[link(name = "IOSurface", kind = "framework")]
    unsafe extern "C" {
        fn IOSurfaceGetUseCount(buffer: *mut c_void) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRetain(cf: *const c_void) -> *const c_void;
    }

    /// The lock every test that sets the zero-copy knobs (or the cell
    /// denylist) takes: the environment is the process's.
    fn knobs() -> std::sync::MutexGuard<'static, ()> {
        crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// A gradient that moves with `i`, so consecutive frames differ.
    fn shade(i: u64, x: u32, y: u32, c: u32) -> u8 {
        let v = ((x + y + i as u32 * 8) & 0xff) as u8;
        match c {
            0 => v,
            1 => v.wrapping_mul(3),
            2 => 255 - v,
            _ => 0xff,
        }
    }

    fn surface_frame_at(i: u64, w: u32, h: u32) -> Frame {
        let surface = new_bgra(w, h, |x, y, c| shade(i, x, y, c)).expect("a surface");
        Frame {
            width: w,
            height: h,
            stride: surface.stride(),
            pixel_format: PixelFormat::Bgra,
            data: FrameData::IoSurface(surface),
            monotonic_us: i * 16_667,
            monitor: 0,
            damage: Damage::Unknown,
            source: None,
        }
    }

    fn memory_frame_at(i: u64, w: u32, h: u32) -> Frame {
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                for c in 0..4 {
                    bgra.push(shade(i, x, y, c));
                }
            }
        }
        Frame {
            width: w,
            height: h,
            stride: w * 4,
            pixel_format: PixelFormat::Bgra,
            data: bgra.into(),
            monotonic_us: i * 16_667,
            monitor: 0,
            damage: Damage::Unknown,
            source: None,
        }
    }

    /// Every gate is read at the open: the switch, the cell, the denylist,
    /// and the probe's proof, in that order.
    #[test]
    fn plan_reads_every_gate_at_the_open() {
        let _guard = knobs();
        let _switch = Saved::cleared("ZERO_COPY");
        let _deny = Saved::cleared("ZERO_COPY_DENY");
        let reason = |name, chroma444| match plan(name, chroma444) {
            Ok(()) => "gpu",
            Err(reason) => reason,
        };
        assert_eq!(reason("hevc_videotoolbox", false), "switch_off");
        // SAFETY: serialised by DENY_ENV_LOCK; the guards restore both.
        unsafe { set("ZERO_COPY", "1") };
        assert_eq!(reason("hevc_qsv", false), "no_gpu_cell");
        assert_eq!(reason("av1_videotoolbox", false), "no_gpu_cell");
        assert_eq!(reason("hevc_videotoolbox", true), "no_gpu_cell");
        assert_eq!(reason("hevc_videotoolbox", false), "unproven");
        with_proven(&["hevc_videotoolbox"], || {
            assert_eq!(reason("hevc_videotoolbox", false), "gpu");
            assert_eq!(reason("h264_videotoolbox", false), "unproven");
            // SAFETY: as above.
            unsafe { set("ZERO_COPY_DENY", "hevc_videotoolbox:gpu") };
            assert_eq!(reason("hevc_videotoolbox", false), "denied");
        });
    }

    /// A surface frame is the IOSurface as it is, in `data[3]`, and holds the
    /// surface (its use count) until FFmpeg lets go of the frame, even after
    /// the capture's own frame has gone.
    #[test]
    fn a_surface_frame_holds_its_surface_until_the_encoder_lets_go() {
        let frame =
            new_bgra(64, 32, |x, _, c| if c == 3 { 0xff } else { x as u8 }).expect("a surface");
        let surface = frame.surface();
        // SAFETY: the test's own retain, for the queries after the frames
        // have gone; given back at the end.
        unsafe { CFRetain(surface) };
        let vt = VtInput::new(64, 32);
        let av = vt.surface_frame(&frame).expect("a VideoToolbox frame");
        // SAFETY: a frame `surface_frame` built; read-only field reads.
        unsafe {
            assert_eq!(
                (*av.as_ptr()).format,
                ff::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX as i32
            );
            assert!(!(*av.as_ptr()).data[3].is_null(), "the CVPixelBuffer");
        }
        drop(frame);
        // SAFETY: a live surface (the test's retain); a read-only query.
        // The frame's own hold, and CoreVideo's: a `CVPixelBuffer` made over a
        // surface holds a use count of its own while it lives (2 measured on
        // macOS 27, 2026-10-29).
        assert!(
            unsafe { IOSurfaceGetUseCount(surface) } >= 1,
            "the VideoToolbox frame holds it after the capture's frame has gone"
        );
        drop(av);
        // SAFETY: as above, then the test's retain is given back.
        unsafe {
            assert_eq!(
                IOSurfaceGetUseCount(surface),
                0,
                "given back when FFmpeg lets go"
            );
            CFRelease(surface);
        }
    }

    /// VideoToolbox's GPU form end to end: with the switch on and the form
    /// proven, HEVC and H.264 open in it and take both IOSurface frames and
    /// frames in memory, and hand back packets.
    #[test]
    fn videotoolbox_encodes_iosurface_and_memory_frames_in_its_gpu_form() {
        let _guard = knobs();
        let _switch = Saved::cleared("ZERO_COPY");
        let _deny = Saved::cleared("ZERO_COPY_DENY");
        // SAFETY: serialised by DENY_ENV_LOCK; the guards restore both.
        unsafe { set("ZERO_COPY", "1") };
        let (w, h) = (480u32, 270u32);
        for name in ["hevc_videotoolbox", "h264_videotoolbox"] {
            with_proven(&[name], || {
                let mut encoder = FfmpegEncoder::new_named_probe(name, w, h, false)
                    .unwrap_or_else(|e| panic!("{name} opens: {e}"));
                let input = encoder.rate_stats().input;
                assert!(input.gpu, "{name} opened on CPU frames ({})", input.reason);
                let mut bytes = 0usize;
                for i in 0..30u64 {
                    let frame = if i % 3 == 2 {
                        memory_frame_at(i, w, h)
                    } else {
                        surface_frame_at(i, w, h)
                    };
                    bytes += encoder
                        .encode_sync(&frame)
                        .unwrap_or_else(|e| panic!("{name} frame {i}: {e}"))
                        .iter()
                        .map(|p| p.data.len())
                        .sum::<usize>();
                }
                assert!(bytes > 0, "{name}: 30 frames encoded to nothing");
            });
        }
    }

    /// Eight flat bars, 60 px wide each across 480 px: black, white, grey, and
    /// 75 % red, green, blue, yellow and cyan, as BGRA.
    const BARS: [[u8; 3]; 8] = [
        [0, 0, 0],
        [255, 255, 255],
        [128, 128, 128],
        [0, 0, 191],
        [0, 191, 0],
        [191, 0, 0],
        [0, 191, 191],
        [191, 191, 0],
    ];

    fn bar(x: u32, c: u32) -> u8 {
        if c == 3 {
            0xff
        } else {
            BARS[(x / 60).min(7) as usize][c as usize]
        }
    }

    /// The last picture `packets` decode to, with FFmpeg's software H.264
    /// decoder: the planes (Y, U, V at 4:2:0) and the colour tags the bitstream
    /// carried.
    fn decode_h264(packets: &[u8]) -> (ffmpeg_next::frame::Video, String) {
        let codec =
            ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::H264).expect("an H.264 decoder");
        let mut decoder = ffmpeg_next::codec::Context::new_with_codec(codec)
            .decoder()
            .video()
            .expect("open the decoder");
        decoder
            .send_packet(&ffmpeg_next::Packet::copy(packets))
            .expect("send the stream");
        decoder.send_eof().expect("send eof");
        let mut last = None;
        let mut f = ffmpeg_next::frame::Video::empty();
        while decoder.receive_frame(&mut f).is_ok() {
            last = Some(f.clone());
        }
        let last = last.expect("a decoded picture");
        // SAFETY: a decoded frame; read-only field reads.
        let tags = unsafe {
            let p = last.as_ptr();
            format!(
                "matrix {:?}, range {:?}, primaries {:?}",
                (*p).colorspace,
                (*p).color_range,
                (*p).color_primaries
            )
        };
        (last, tags)
    }

    /// The mean of a plane over a `side`-square block at (`x`, `y`).
    fn block_mean(
        frame: &ffmpeg_next::frame::Video,
        plane: usize,
        x: usize,
        y: usize,
        side: usize,
    ) -> f64 {
        let stride = frame.stride(plane);
        let data = frame.data(plane);
        let mut sum = 0u64;
        for row in y..y + side {
            for col in x..x + side {
                sum += u64::from(data[row * stride + col]);
            }
        }
        sum as f64 / (side * side) as f64
    }

    /// FR-118 P5 — the colour the GPU form encodes, against the CPU form, on
    /// the same bars. VideoToolbox converts the BGRA itself on the GPU form,
    /// and the CPU form encodes our BT.601 limited-range NV12, so this is the
    /// measurement open decision 3 asks for: AC3's ±2 code values per plane,
    /// with the colour tags each bitstream carries printed beside it.
    #[test]
    fn the_gpu_form_encodes_the_bars_as_the_cpu_form_does() {
        let _guard = knobs();
        let _switch = Saved::cleared("ZERO_COPY");
        let _deny = Saved::cleared("ZERO_COPY_DENY");
        let (w, h) = (480u32, 270u32);
        let name = "h264_videotoolbox";
        let encode = |gpu: bool| -> Vec<u8> {
            let mut encoder = FfmpegEncoder::new_named_probe(name, w, h, false)
                .unwrap_or_else(|e| panic!("{name} opens: {e}"));
            assert_eq!(encoder.rate_stats().input.gpu, gpu, "the form asked for");
            let mut stream = Vec::new();
            for i in 0..20u64 {
                let frame = if gpu {
                    let surface = new_bgra(w, h, |x, _, c| bar(x, c)).expect("a surface");
                    Frame {
                        width: w,
                        height: h,
                        stride: surface.stride(),
                        pixel_format: PixelFormat::Bgra,
                        data: FrameData::IoSurface(surface),
                        monotonic_us: i * 16_667,
                        monitor: 0,
                        damage: Damage::Unknown,
                        source: None,
                    }
                } else {
                    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
                    for _ in 0..h {
                        for x in 0..w {
                            for c in 0..4 {
                                bgra.push(bar(x, c));
                            }
                        }
                    }
                    Frame {
                        width: w,
                        height: h,
                        stride: w * 4,
                        pixel_format: PixelFormat::Bgra,
                        data: bgra.into(),
                        monotonic_us: i * 16_667,
                        monitor: 0,
                        damage: Damage::Unknown,
                        source: None,
                    }
                };
                for p in encoder.encode_sync(&frame).expect("encode") {
                    stream.extend_from_slice(&p.data);
                }
            }
            drop(encoder);
            stream
        };
        let cpu = encode(false);
        // SAFETY: serialised by DENY_ENV_LOCK; the guards restore both.
        unsafe { set("ZERO_COPY", "1") };
        let gpu = with_proven(&[name], || encode(true));
        let (cpu_pic, cpu_tags) = decode_h264(&cpu);
        let (gpu_pic, gpu_tags) = decode_h264(&gpu);
        println!("CPU form: {cpu_tags}");
        println!("GPU form: {gpu_tags}");
        assert_eq!(
            cpu_tags, gpu_tags,
            "the GPU form describes its colour as the CPU form does"
        );
        let mut worst = 0f64;
        for (i, bgr) in BARS.iter().enumerate() {
            let cx = i * 60 + 22;
            let cy = 120;
            let mut row = format!("bar {i} BGR {bgr:?}:");
            for (plane, (x, y, side)) in [(cx, cy, 16), (cx / 2, cy / 2, 8), (cx / 2, cy / 2, 8)]
                .into_iter()
                .enumerate()
            {
                let a = block_mean(&cpu_pic, plane, x, y, side);
                let b = block_mean(&gpu_pic, plane, x, y, side);
                worst = worst.max((a - b).abs());
                row.push_str(&format!(" {} {a:.1}/{b:.1}", ["Y", "U", "V"][plane]));
            }
            println!("{row}  (CPU/GPU)");
        }
        assert!(
            worst <= 2.0,
            "the GPU form's colour is not the CPU form's: worst plane difference {worst:.1} \
             (CPU tags: {cpu_tags}; GPU tags: {gpu_tags}); see the rows above"
        );
    }
}
