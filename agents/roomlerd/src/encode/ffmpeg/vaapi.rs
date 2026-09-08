// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! FR-77 P4 — VAAPI: the hardware device and frame contexts the `*_vaapi`
//! encoders need, which nothing else in the cascade does.
//!
//! NVENC, QSV and AMF open their own device behind the encoder; a VAAPI
//! encoder takes `AV_PIX_FMT_VAAPI` frames from an `AVHWFramesContext` bound
//! to an `AVHWDeviceContext`, so the pump has to (1) open a render node,
//! (2) allocate a frame pool in the encoder's software format (NV12, or
//! packed VUYX for 4:4:4), and (3) upload every software frame into that
//! pool before `send_frame`. ffmpeg-next 9 wraps none of this, hence the raw
//! `ffmpeg_sys_next` calls, each one bounded to this file.
//!
//! The device is opened ONCE per process and shared (a `OnceLock`): every
//! encoder, probe and rebuild in the process uses the same render node,
//! which is also what makes the caps probe's answer describe the session's
//! device. Candidates, in order: the pinned `vaapi_device` config key
//! (`ROOMLERD_VAAPI_DEVICE`), then `/dev/dri/renderD128`…`renderD135` that
//! exist. The first node libva can open wins; a host with none has no
//! VAAPI cells and says so once.
//!
//! `/dev/dxg` (WSL2's GPU device) is deliberately NOT a candidate, measured
//! 2026-09-08: a WSL2 distro has no `/dev/dri` at all, `/dev/dxg` is a
//! misc-major node (10:125), and libva's DRM display refuses anything that
//! is not a DRM-major character device with nothing but an `fstat` —
//! `vainfo --display drm --device /dev/dxg` says "Failed to a DRM display
//! for the given device" even with `LIBVA_DRIVER_NAME=d3d12` and Mesa's
//! `d3d12_drv_video.so` installed. That driver is reachable only through a
//! Wayland/X11 display, which a root daemon on a headless host has no
//! business depending on. WSL is the negative cell; NVENC still works there.

use anyhow::{Result, anyhow};
use ffmpeg_next::frame;
use ffmpeg_next::sys as ff;

/// The render-node candidates, in order. `pinned` is the config key's
/// value; `exists` answers for a path so the order is testable without a
/// `/dev`.
// Read by the Linux device opener only; every other platform has the stub.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn candidates(pinned: Option<&str>, exists: &dyn Fn(&str) -> bool) -> Vec<String> {
    if let Some(p) = pinned.map(str::trim).filter(|p| !p.is_empty()) {
        return vec![p.to_string()];
    }
    (128..=135)
        .map(|n| format!("/dev/dri/renderD{n}"))
        .filter(|p| exists(p))
        .collect()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn pinned_device() -> Option<String> {
    tunnel_core::env::node_env("VAAPI_DEVICE").filter(|v| !v.trim().is_empty())
}

/// The frame pool's software format: what the pump uploads.
pub(crate) fn sw_format(chroma444: bool) -> ff::AVPixelFormat {
    if chroma444 {
        ff::AVPixelFormat::AV_PIX_FMT_VUYX
    } else {
        ff::AVPixelFormat::AV_PIX_FMT_NV12
    }
}

/// FFmpeg's error text for a negative return.
#[allow(dead_code)]
fn ff_err(rc: i32) -> String {
    let mut buf = [0u8; 128];
    // SAFETY: fixed-size buffer, FFmpeg NUL-terminates within it.
    unsafe {
        ff::av_strerror(rc, buf.as_mut_ptr() as *mut std::os::raw::c_char, buf.len());
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    format!("{} ({})", String::from_utf8_lossy(&buf[..end]), rc)
}

#[cfg(target_os = "linux")]
mod real {
    use super::*;
    use std::ffi::CString;
    use std::sync::OnceLock;

    /// The process-wide VAAPI device: an `AVBufferRef` to the
    /// `AVHWDeviceContext`. The node it was opened on is logged at open;
    /// nothing reads it afterwards, so it is not kept.
    pub(crate) struct Device {
        buf: *mut ff::AVBufferRef,
    }
    // SAFETY: the device context is reference-counted and thread-safe to
    // share by FFmpeg's contract (every hwframes ctx takes its own ref); the
    // pointer is never mutated after creation.
    unsafe impl Send for Device {}
    unsafe impl Sync for Device {}

    static DEVICE: OnceLock<Option<Device>> = OnceLock::new();

    /// Open (once) the first candidate node libva accepts. `None` = no
    /// VAAPI on this host; logged once with every path tried.
    pub(crate) fn device() -> Option<&'static Device> {
        DEVICE
            .get_or_init(|| {
                let exists = |p: &str| std::path::Path::new(p).exists();
                let cands = candidates(pinned_device().as_deref(), &exists);
                if cands.is_empty() {
                    tracing::info!(
                        "vaapi: no render node on this host (no /dev/dri/renderD*) — no VAAPI cells"
                    );
                    return None;
                }
                for path in &cands {
                    match open_device(path) {
                        Ok(buf) => {
                            tracing::info!(device = %path, tried = ?cands, "vaapi: device opened");
                            return Some(Device { buf });
                        }
                        Err(e) => tracing::info!(device = %path, %e, "vaapi: device did not open"),
                    }
                }
                tracing::info!(tried = ?cands, "vaapi: no candidate node opened — no VAAPI cells");
                None
            })
            .as_ref()
    }

    fn open_device(path: &str) -> Result<*mut ff::AVBufferRef> {
        let c = CString::new(path).map_err(|_| anyhow!("device path has a NUL"))?;
        let mut buf: *mut ff::AVBufferRef = std::ptr::null_mut();
        // SAFETY: `buf` is a valid out-pointer, `c` outlives the call, no
        // options dict, no flags. On failure FFmpeg leaves `buf` null.
        let rc = unsafe {
            ff::av_hwdevice_ctx_create(
                &mut buf,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                c.as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        if rc < 0 || buf.is_null() {
            return Err(anyhow!(
                "av_hwdevice_ctx_create({path}) failed: {}",
                ff_err(rc)
            ));
        }
        Ok(buf)
    }

    /// One encoder's frame pool on the shared device.
    pub(crate) struct Frames {
        buf: *mut ff::AVBufferRef,
    }
    // SAFETY: the frames context is reference-counted; the encoder that
    // owns this handle is the only user of the pool from the media thread.
    unsafe impl Send for Frames {}

    impl Frames {
        pub(crate) fn new(
            dev: &Device,
            sw_format: ff::AVPixelFormat,
            width: u32,
            height: u32,
        ) -> Result<Self> {
            // SAFETY: `dev.buf` is a live device ref; the returned buffer's
            // data IS an AVHWFramesContext by FFmpeg's contract.
            let buf = unsafe { ff::av_hwframe_ctx_alloc(dev.buf) };
            if buf.is_null() {
                return Err(anyhow!("av_hwframe_ctx_alloc failed"));
            }
            // SAFETY: as above; the fields are plain ints/enums set before
            // init, exactly as `ffmpeg -vaapi_device` does.
            let rc = unsafe {
                let ctx = (*buf).data as *mut ff::AVHWFramesContext;
                (*ctx).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
                (*ctx).sw_format = sw_format;
                (*ctx).width = width as i32;
                (*ctx).height = height as i32;
                // A generous pool: the pump holds at most a handful in
                // flight (async_depth=1), the rest is headroom for the
                // driver's own reordering.
                (*ctx).initial_pool_size = 20;
                ff::av_hwframe_ctx_init(buf)
            };
            if rc < 0 {
                let mut b = buf;
                // SAFETY: unref the buffer we own; sets it null.
                unsafe { ff::av_buffer_unref(&mut b) };
                return Err(anyhow!("av_hwframe_ctx_init failed: {}", ff_err(rc)));
            }
            Ok(Self { buf })
        }

        /// A NEW reference for an encoder's `hw_frames_ctx` (the codec
        /// context unrefs it when it is freed).
        pub(crate) fn new_ref(&self) -> *mut ff::AVBufferRef {
            // SAFETY: `self.buf` is live for as long as `self`.
            unsafe { ff::av_buffer_ref(self.buf) }
        }

        /// Upload one software frame (NV12 / VUYX, pts + pict_type set) into
        /// a pool frame the encoder accepts. `copy_props` carries the pts and
        /// the forced-I picture type across.
        pub(crate) fn upload(&self, sw: &frame::Video) -> Result<frame::Video> {
            let mut hw = frame::Video::empty();
            // SAFETY: `hw` is an empty AVFrame we own; the pool hands it a
            // VAAPI surface; `sw` is a fully populated software frame.
            unsafe {
                let rc = ff::av_hwframe_get_buffer(self.buf, hw.as_mut_ptr(), 0);
                if rc < 0 {
                    return Err(anyhow!("av_hwframe_get_buffer failed: {}", ff_err(rc)));
                }
                let rc = ff::av_hwframe_transfer_data(hw.as_mut_ptr(), sw.as_ptr(), 0);
                if rc < 0 {
                    return Err(anyhow!("av_hwframe_transfer_data failed: {}", ff_err(rc)));
                }
                let rc = ff::av_frame_copy_props(hw.as_mut_ptr(), sw.as_ptr());
                if rc < 0 {
                    return Err(anyhow!("av_frame_copy_props failed: {}", ff_err(rc)));
                }
            }
            Ok(hw)
        }
    }

    impl Drop for Frames {
        fn drop(&mut self) {
            // SAFETY: we own exactly one ref; unref sets the pointer null.
            unsafe { ff::av_buffer_unref(&mut self.buf) };
        }
    }
}

#[cfg(target_os = "linux")]
#[allow(unused_imports)]
pub(crate) use real::{Device, Frames, device};

/// Every other platform: no device, so no `*_vaapi` open ever gets past the
/// cascade's first check. The types exist so the encoder's code has ONE
/// shape everywhere.
#[cfg(not(target_os = "linux"))]
mod stub {
    use super::*;

    pub(crate) struct Device;

    pub(crate) fn device() -> Option<&'static Device> {
        None
    }

    pub(crate) struct Frames;

    impl Frames {
        pub(crate) fn new(_dev: &Device, _sw: ff::AVPixelFormat, _w: u32, _h: u32) -> Result<Self> {
            Err(anyhow!("VAAPI is Linux-only"))
        }
        pub(crate) fn new_ref(&self) -> *mut ff::AVBufferRef {
            std::ptr::null_mut()
        }
        pub(crate) fn upload(&self, _sw: &frame::Video) -> Result<frame::Video> {
            Err(anyhow!("VAAPI is Linux-only"))
        }
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(unused_imports)]
pub(crate) use stub::{Device, Frames, device};

#[cfg(test)]
mod tests {
    use super::*;

    /// The pin wins outright; otherwise the render nodes in numeric order —
    /// only the ones that exist, and never `/dev/dxg` (WSL2: libva refuses
    /// it, see the module doc).
    #[test]
    fn candidates_pin_then_render_nodes_never_dxg() {
        let all = |_: &str| true;
        assert_eq!(
            candidates(Some(" /dev/dri/renderD129 "), &all),
            vec!["/dev/dri/renderD129"]
        );
        assert_eq!(
            candidates(Some("  "), &all).first().map(String::as_str),
            Some("/dev/dri/renderD128"),
            "a blank pin is no pin"
        );
        let v = candidates(None, &all);
        assert_eq!(v.len(), 8);
        assert_eq!(v[0], "/dev/dri/renderD128");
        assert_eq!(v[7], "/dev/dri/renderD135");
        assert!(!v.iter().any(|p| p.contains("dxg")));
        let wsl = |p: &str| p == "/dev/dxg";
        assert!(
            candidates(None, &wsl).is_empty(),
            "a WSL2 box (dxg only) has no candidate — the opener says so once"
        );
        let none = |_: &str| false;
        assert!(candidates(None, &none).is_empty());
    }

    #[test]
    fn the_pool_format_follows_the_chroma() {
        assert_eq!(sw_format(false), ff::AVPixelFormat::AV_PIX_FMT_NV12);
        assert_eq!(sw_format(true), ff::AVPixelFormat::AV_PIX_FMT_VUYX);
    }
}
