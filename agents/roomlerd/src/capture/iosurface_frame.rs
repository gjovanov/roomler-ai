// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! FR-118 P5 — a captured frame that stays on the GPU on macOS: the BGRA
//! `IOSurface` that `CGDisplayStream` delivered.
//!
//! An encoder that takes the surface (VideoToolbox, as a `CVPixelBuffer`) uses
//! it as it is. Every other consumer reads the pixels through
//! [`IoSurfaceFrame::cpu`], which copies them into memory on the first call
//! and keeps them, so a frame is read back at most once however many
//! consumers ask. That copy is the one the scrap arm made for every frame
//! before P5.
//!
//! ## Holding a surface
//!
//! The stream draws into a small pool of surfaces, and it skips a surface
//! whose use count is not zero. A frame therefore holds the surface's use
//! count (and a retain) from the moment the stream delivered it until the last
//! clone of the frame is dropped: the stream cannot draw the next picture into
//! a surface an encoder is still reading. The hold is meant to last about one
//! frame. A consumer that keeps a frame longer (the pump's keepalive copy)
//! costs the stream one surface, never a torn picture.

use std::ffi::c_void;
use std::sync::{Arc, OnceLock};

/// An `IOSurfaceRef`.
pub type IoSurfaceRef = *mut c_void;

/// `kIOSurfaceLockReadOnly`.
const LOCK_READ_ONLY: u32 = 0x0000_0001;

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceLock(buffer: IoSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(buffer: IoSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(buffer: IoSurfaceRef) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(buffer: IoSurfaceRef) -> usize;
    fn IOSurfaceGetAllocSize(buffer: IoSurfaceRef) -> usize;
    fn IOSurfaceIncrementUseCount(buffer: IoSurfaceRef);
    fn IOSurfaceDecrementUseCount(buffer: IoSurfaceRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

/// One retain and one use count on a surface, given back when dropped.
struct Held(IoSurfaceRef);

// SAFETY: an IOSurface is a kernel-backed, reference-counted object, safe to
// retain, release, lock and read from any thread. `Held` only does those.
unsafe impl Send for Held {}
// SAFETY: as above; shared access only reads the surface under its lock.
unsafe impl Sync for Held {}

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: `Held` owns exactly one use count and one retain on a live
        // surface, taken in `adopt` or `retain`; each is given back once.
        unsafe {
            IOSurfaceDecrementUseCount(self.0);
            CFRelease(self.0);
        }
    }
}

struct Inner {
    surface: Held,
    width: u32,
    height: u32,
    stride: u32,
    /// The bytes, read back on the first [`IoSurfaceFrame::cpu`].
    cpu: OnceLock<Vec<u8>>,
}

/// A BGRA frame in an `IOSurface`. Cheap to clone: the clones share the
/// surface and its one readback.
#[derive(Clone)]
pub struct IoSurfaceFrame {
    inner: Arc<Inner>,
}

impl IoSurfaceFrame {
    /// A frame over `surface`, taking over the retain and the use count the
    /// caller holds on it (scrap's `Frame::into_surface` hands over exactly
    /// those).
    ///
    /// # Safety
    /// `surface` is a live BGRA `IOSurfaceRef` of `width` × `height` pixels
    /// that the caller holds one retain and one use count on, and gives up.
    pub unsafe fn adopt(surface: IoSurfaceRef, width: u32, height: u32) -> Self {
        // SAFETY: the caller guarantees a live surface.
        let stride = unsafe { IOSurfaceGetBytesPerRow(surface) } as u32;
        Self {
            inner: Arc::new(Inner {
                surface: Held(surface),
                width,
                height,
                stride,
                cpu: OnceLock::new(),
            }),
        }
    }

    /// A frame over `surface`, taking a retain and a use count of its own.
    ///
    /// # Safety
    /// `surface` is a live BGRA `IOSurfaceRef` of `width` × `height` pixels.
    #[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
    pub unsafe fn retain(surface: IoSurfaceRef, width: u32, height: u32) -> Self {
        // SAFETY: the caller guarantees a live surface; the retain and the use
        // count taken here are the ones `adopt` takes over.
        unsafe {
            CFRetain(surface);
            IOSurfaceIncrementUseCount(surface);
            Self::adopt(surface, width, height)
        }
    }

    /// The surface, for a consumer that takes it on the GPU. It stays valid
    /// for as long as this frame (or a clone) lives.
    #[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
    pub fn surface(&self) -> IoSurfaceRef {
        self.inner.surface.0
    }

    #[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
    pub fn width(&self) -> u32 {
        self.inner.width
    }

    #[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
    pub fn height(&self) -> u32 {
        self.inner.height
    }

    /// Bytes between the starts of two rows: the surface's own pitch, which
    /// [`IoSurfaceFrame::cpu`] keeps.
    pub fn stride(&self) -> u32 {
        self.inner.stride
    }

    /// The bytes, `stride` × `height` of them, read back on the first call and
    /// kept. A surface that will not lock yields a black frame of the right
    /// size, never a short buffer: every consumer trusts the frame's dims.
    pub fn cpu(&self) -> &[u8] {
        self.inner
            .cpu
            .get_or_init(|| read_back(self.inner.surface.0, self.inner.stride, self.inner.height))
    }
}

/// The surface's first `stride` × `height` bytes, copied out under a
/// read-only lock.
fn read_back(surface: IoSurfaceRef, stride: u32, height: u32) -> Vec<u8> {
    let len = stride as usize * height as usize;
    // SAFETY: `surface` is live for the frame's lifetime. The base address is
    // read only between a successful lock and its unlock, and no more than
    // the surface's allocation is copied.
    unsafe {
        if IOSurfaceLock(surface, LOCK_READ_ONLY, std::ptr::null_mut()) != 0 {
            tracing::warn!(
                "FR-118: an IOSurface frame would not lock for its readback — a black frame"
            );
            return vec![0; len];
        }
        let base = IOSurfaceGetBaseAddress(surface) as *const u8;
        let available = IOSurfaceGetAllocSize(surface);
        let mut bytes = vec![0; len];
        if !base.is_null() {
            let n = len.min(available);
            std::ptr::copy_nonoverlapping(base, bytes.as_mut_ptr(), n);
        }
        IOSurfaceUnlock(surface, LOCK_READ_ONLY, std::ptr::null_mut());
        bytes
    }
}

// The surfaces `new_bgra` makes are for the capability probe (the encoder's
// build) and the tests only.
#[cfg(any(test, feature = "ffmpeg-encoder"))]
type CfDictionaryRef = *const c_void;
#[cfg(any(test, feature = "ffmpeg-encoder"))]
type CfTypeRef = *const c_void;

/// `kCFNumberSInt32Type`.
#[cfg(any(test, feature = "ffmpeg-encoder"))]
const CF_NUMBER_SINT32: i32 = 3;
/// `kCVPixelFormatType_32BGRA`.
#[cfg(any(test, feature = "ffmpeg-encoder"))]
const PIXEL_FORMAT_BGRA: i32 = 0x4247_5241;

#[cfg(any(test, feature = "ffmpeg-encoder"))]
#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    static kIOSurfaceWidth: CfTypeRef;
    static kIOSurfaceHeight: CfTypeRef;
    static kIOSurfaceBytesPerElement: CfTypeRef;
    static kIOSurfacePixelFormat: CfTypeRef;
    fn IOSurfaceCreate(properties: CfDictionaryRef) -> IoSurfaceRef;
}

#[cfg(any(test, feature = "ffmpeg-encoder"))]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFNumberCreate(allocator: CfTypeRef, kind: i32, value: *const c_void) -> CfTypeRef;
    fn CFDictionaryCreate(
        allocator: CfTypeRef,
        keys: *const CfTypeRef,
        values: *const CfTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CfDictionaryRef;
}

/// A new `width` × `height` BGRA surface holding `fill(x, y, channel)` (B, G,
/// R, A), as a frame. The capability probe proves VideoToolbox's GPU form on
/// these (`caps::probe_gpu`), and the tests read them back.
#[cfg(any(test, feature = "ffmpeg-encoder"))]
pub(crate) fn new_bgra(
    width: u32,
    height: u32,
    fill: impl Fn(u32, u32, u32) -> u8,
) -> anyhow::Result<IoSurfaceFrame> {
    // SAFETY: CoreFoundation and IOSurface creation calls on values that
    // outlive them. Each object created is released once here, except the
    // surface, whose creator's retain is given back after the frame took its
    // own. The surface is written only under its lock, within its pitch.
    unsafe {
        let num = |v: i32| {
            CFNumberCreate(
                std::ptr::null(),
                CF_NUMBER_SINT32,
                (&v as *const i32).cast(),
            )
        };
        let keys = [
            kIOSurfaceWidth,
            kIOSurfaceHeight,
            kIOSurfaceBytesPerElement,
            kIOSurfacePixelFormat,
        ];
        let values = [
            num(width as i32),
            num(height as i32),
            num(4),
            num(PIXEL_FORMAT_BGRA),
        ];
        let props = CFDictionaryCreate(
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
        let surface = IOSurfaceCreate(props);
        CFRelease(props);
        if surface.is_null() {
            anyhow::bail!("IOSurfaceCreate {width}x{height} BGRA");
        }
        let frame = IoSurfaceFrame::retain(surface, width, height);
        CFRelease(surface);
        if IOSurfaceLock(surface, 0, std::ptr::null_mut()) != 0 {
            anyhow::bail!("IOSurfaceLock for writing");
        }
        let base = IOSurfaceGetBaseAddress(surface) as *mut u8;
        let stride = frame.stride() as usize;
        for y in 0..height {
            for x in 0..width {
                for c in 0..4 {
                    *base.add(y as usize * stride + x as usize * 4 + c as usize) = fill(x, y, c);
                }
            }
        }
        IOSurfaceUnlock(surface, 0, std::ptr::null_mut());
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[link(name = "IOSurface", kind = "framework")]
    unsafe extern "C" {
        fn IOSurfaceGetUseCount(buffer: IoSurfaceRef) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFGetRetainCount(cf: *const c_void) -> isize;
    }

    /// The test pattern's byte at `(x, y)`, channel `c` (B, G, R, A).
    fn pattern(x: u32, y: u32, c: u32) -> u8 {
        if c == 3 {
            0xFF
        } else {
            ((x * 7 + y * 13 + c * 61) & 0xFF) as u8
        }
    }

    /// A frame's `cpu()` is the bytes its surface holds, row by row at the
    /// surface's own pitch (an odd width, so the rows are padded), and its
    /// clones share the one readback.
    #[test]
    fn an_iosurface_frame_reads_back_the_bytes_its_surface_holds() {
        let (w, h) = (37u32, 9u32);
        let frame = new_bgra(w, h, pattern).expect("a surface");
        let stride = frame.stride();
        assert!(stride >= w * 4, "a row holds its pixels");
        let bytes = frame.cpu();
        assert_eq!(bytes.len(), (stride * h) as usize);
        for y in 0..h {
            for x in 0..w {
                for c in 0..4 {
                    assert_eq!(
                        bytes[(y * stride + x * 4 + c) as usize],
                        pattern(x, y, c),
                        "pixel ({x}, {y}) channel {c}"
                    );
                }
            }
        }
        let copy = frame.clone();
        assert!(
            std::ptr::eq(copy.cpu().as_ptr(), bytes.as_ptr()),
            "a clone shares the one readback"
        );
    }

    /// A frame holds the surface's use count (so the stream will not draw into
    /// it) and a retain, until its last clone is dropped, and gives both back
    /// exactly once.
    #[test]
    fn a_frame_holds_the_surface_until_its_last_clone_goes() {
        let frame = new_bgra(16, 16, pattern).expect("a surface");
        let surface = frame.surface();
        // SAFETY: the test's own retain keeps the surface alive past the
        // frames, for the queries below; released at the end.
        unsafe { CFRetain(surface) };
        let copy = frame.clone();
        // SAFETY: a live surface; read-only queries.
        let retained = unsafe {
            assert_eq!(IOSurfaceGetUseCount(surface), 1, "held while a frame lives");
            CFGetRetainCount(surface)
        };
        drop(frame);
        // SAFETY: as above.
        assert_eq!(
            unsafe { IOSurfaceGetUseCount(surface) },
            1,
            "a clone still holds it"
        );
        drop(copy);
        // SAFETY: as above, then the test's own retain is given back.
        unsafe {
            assert_eq!(
                IOSurfaceGetUseCount(surface),
                0,
                "given back with the last clone"
            );
            assert_eq!(CFGetRetainCount(surface), retained - 1, "and its retain");
            CFRelease(surface);
        }
    }

    /// `cpu_mut()` turns an IOSurface frame into a CPU frame with the same
    /// bytes, and lets the surface go.
    #[test]
    fn cpu_mut_turns_an_iosurface_frame_into_a_cpu_frame() {
        let gpu = new_bgra(8, 4, pattern).expect("a surface");
        let surface = gpu.surface();
        // SAFETY: the test's own retain, given back at the end.
        unsafe { CFRetain(surface) };
        let stride = gpu.stride();
        let expected = gpu.cpu().to_vec();
        let mut frame = crate::capture::Frame {
            width: 8,
            height: 4,
            stride,
            pixel_format: crate::capture::PixelFormat::Bgra,
            data: crate::capture::FrameData::IoSurface(gpu),
            monotonic_us: 0,
            monitor: 0,
            damage: crate::capture::Damage::Unknown,
            source: None,
        };
        frame.cpu_mut()[0] ^= 0xFF;
        assert!(matches!(frame.data, crate::capture::FrameData::Cpu(_)));
        assert_eq!(frame.cpu()[0], expected[0] ^ 0xFF);
        assert_eq!(&frame.cpu()[1..], &expected[1..]);
        // SAFETY: as above.
        unsafe {
            assert_eq!(
                IOSurfaceGetUseCount(surface),
                0,
                "the CPU frame let the surface go"
            );
            CFRelease(surface);
        }
    }
}
