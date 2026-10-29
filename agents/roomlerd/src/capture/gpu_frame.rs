// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! FR-118 P2 — a captured frame that stays on the GPU: a BGRA texture on the
//! capture's own D3D11 device.
//!
//! A consumer that takes a GPU surface (the encoder, from P2-2) uses the
//! texture as it is. Every other consumer reads the pixels through
//! [`GpuFrame::cpu`], which copies them into CPU memory on the first call and
//! keeps them, so a frame is read back at most once however many consumers ask.
//!
//! ## Where the textures come from
//!
//! A capture backend copies each frame out of its capture surface (which it must
//! give back to the OS before the next frame) into a texture from a
//! [`TexturePool`]. The texture goes back to the pool when the last clone of
//! the frame is dropped, so a pool holds about as many textures as frames are
//! in flight: one being encoded, the pump's keepalive copy, and one being
//! captured.
//!
//! ## Threads
//!
//! A frame is read back on whichever thread asks, while the capture thread keeps
//! using the same device. The D3D11 immediate context is not thread-safe on its
//! own, so [`TexturePool::new`] turns the device's multithread protection on,
//! the same switch FFmpeg's D3D11VA device context turns on, and refuses a
//! device where it cannot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, anyhow};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Device,
    ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1, IDXGIFactory1,
};
use windows::core::Interface;

/// How many free textures a pool keeps. More than the frames a session holds
/// in flight; a texture beyond it is released rather than kept.
const POOL_KEEP: usize = 6;

/// FR-118 P2 — the kill switch: `ROOMLERD_ZERO_COPY` (config `zero_copy`),
/// default OFF. A capture backend reads it once, when it opens, and delivers
/// GPU frames only while it is on.
pub fn zero_copy_enabled() -> bool {
    zero_copy_from(tunnel_core::env::node_env("ZERO_COPY").as_deref())
}

/// `1`, `true`, `yes` or `on`, in any case, turn it on; anything else leaves
/// it off.
fn zero_copy_from(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        let v = v.trim();
        ["1", "true", "yes", "on"]
            .iter()
            .any(|on| v.eq_ignore_ascii_case(on))
    })
}

/// FR-118 P3 — the encoders whose GPU-input form the capability probe proved,
/// as the capture side sees them: the same list `encode::ffmpeg::gpu_input`
/// gates its opens on, installed once by `caps::detect` before any session.
static GPU_INPUT_CELLS: OnceLock<Vec<String>> = OnceLock::new();

pub fn install_gpu_input_cells(names: Vec<String>) {
    let _ = GPU_INPUT_CELLS.set(names);
}

/// FR-118 P3 — the PCI vendor whose adapter a capture device belongs on: the
/// vendor of the first proven GPU-input cell in the backend order every codec's
/// cascade opens in (NVENC, then QSV, then AMF; `encode::ffmpeg::encoder`'s
/// `*_ENCODER_NAMES`). `None` when no GPU-input cell is proven: the capture
/// keeps the default adapter, as before.
///
/// Open decision 1, measured on a hybrid laptop (FR-118 §9.4): WGC with its
/// device on the encoder's adapter lets the OS copy each frame across from the
/// panel's adapter for 0.3–0.6 ms, and the frame is then where the encoder
/// takes it. One vendor for every codec is a simplification: a host whose
/// NVENC lacks a codec that its QSV has opens that codec on QSV, on the other
/// adapter, and those sessions read `adapter_mismatch` and stay on CPU frames.
pub fn preferred_vendor(names: &[String]) -> Option<u32> {
    const ORDER: [(&str, u32); 3] = [("_nvenc", 0x10de), ("_qsv", 0x8086), ("_amf", 0x1002)];
    ORDER
        .iter()
        .find(|(suffix, _)| names.iter().any(|n| n.ends_with(suffix)))
        .map(|&(_, vendor)| vendor)
}

/// FR-118 P3 — the adapter of [`preferred_vendor`], with its name: the first
/// hardware adapter of that vendor `EnumAdapters1` lists. `None` before the
/// probe installed its cells, when no cell is proven, or when no adapter of
/// that vendor is present.
pub fn capture_adapter() -> Option<(IDXGIAdapter1, String)> {
    adapter_of_vendor(preferred_vendor(GPU_INPUT_CELLS.get()?)?)
}

/// FR-118 P3 — a D3D11 device (BGRA support) on the first hardware adapter of
/// `vendor`, for the capability probe's GPU-input opens: NVENC's forms are
/// proven on an NVIDIA device, the device a capture would hand them frames on.
pub fn device_on_vendor(vendor: u32) -> Option<ID3D11Device> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
    let (adapter, _) = adapter_of_vendor(vendor)?;
    let base: IDXGIAdapter = adapter.cast().ok()?;
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: an explicit adapter with driver type UNKNOWN, as the API requires;
    // valid out-params.
    unsafe {
        D3D11CreateDevice(
            &base,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            // The levels WGC's own device asks for, so the probe proves the
            // form on the device a session would hand it frames on.
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .ok()?;
    }
    device
}

/// The first hardware adapter of `vendor` that `EnumAdapters1` lists, with its
/// name.
fn adapter_of_vendor(vendor: u32) -> Option<(IDXGIAdapter1, String)> {
    // SAFETY: plain DXGI factory and adapter enumeration; every out-value is
    // owned by the returned interfaces.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut index = 0;
        while let Ok(adapter) = factory.EnumAdapters1(index) {
            index += 1;
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            let software = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
            if desc.VendorId != vendor || software {
                continue;
            }
            let len = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            return Some((adapter, String::from_utf16_lossy(&desc.Description[..len])));
        }
    }
    None
}

/// A BGRA frame on a D3D11 device. Cheap to clone: the clones share the
/// texture and the one readback.
#[derive(Clone)]
pub struct GpuFrame(Arc<Inner>);

struct Inner {
    texture: ID3D11Texture2D,
    device: ID3D11Device,
    width: u32,
    height: u32,
    cpu: OnceLock<Vec<u8>>,
    pool: Option<Arc<Mutex<Shared>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(pool) = &self.pool {
            let mut shared = pool.lock().unwrap_or_else(|e| e.into_inner());
            if shared.dims == (self.width, self.height) && shared.free.len() < POOL_KEEP {
                shared.free.push(self.texture.clone());
            }
        }
    }
}

impl GpuFrame {
    /// The texture: BGRA8, `DEFAULT` usage, bound as a render target.
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.0.texture
    }

    /// The device the texture lives on. A consumer that keeps GPU state for
    /// one device (an encoder's frame pool) compares it, because a capture
    /// backend rebuilds its device after a desktop switch.
    pub fn device(&self) -> &ID3D11Device {
        &self.0.device
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.0.width, self.0.height)
    }

    /// The pixels in CPU memory: BGRA, tightly packed (`width × 4` bytes a
    /// row), read back on the first call and kept.
    ///
    /// A readback that fails (the device was removed under a driver reset)
    /// yields a black frame of the right size, never a short buffer: every
    /// consumer trusts the frame's dims, and one that indexed past the end of
    /// a short buffer would panic in the media pump.
    pub fn cpu(&self) -> &[u8] {
        self.0.cpu.get_or_init(|| {
            let staging = self.0.pool.as_ref().map(Arc::clone);
            match read_bgra(
                &self.0.device,
                &self.0.texture,
                self.0.width,
                self.0.height,
                staging.as_deref(),
            ) {
                Ok(bytes) => bytes,
                Err(e) => {
                    static WARNED: AtomicBool = AtomicBool::new(false);
                    if !WARNED.swap(true, Ordering::Relaxed) {
                        tracing::warn!(error = %e, "FR-118: a GPU frame's readback failed — delivering a black frame");
                    }
                    vec![0u8; self.0.width as usize * 4 * self.0.height as usize]
                }
            }
        })
    }
}

/// What a pool shares with the frames it lent out.
struct Shared {
    /// The dims of the textures in `free`. A frame of other dims (the encode
    /// box moved) is not kept when it comes back.
    dims: (u32, u32),
    free: Vec<ID3D11Texture2D>,
    /// The CPU-readable copy target for [`GpuFrame::cpu`], at `staging_dims`.
    /// Held under the lock for the whole copy, `Map` and row copy, so two
    /// readbacks never share it at once.
    staging: Option<ID3D11Texture2D>,
    staging_dims: (u32, u32),
}

/// The textures one capture backend copies its frames into, on its device.
pub struct TexturePool {
    device: ID3D11Device,
    shared: Arc<Mutex<Shared>>,
}

impl TexturePool {
    /// A pool on `device`, whose multithread protection this turns on: the
    /// frames are read back on other threads while the capture thread keeps
    /// using the device's immediate context. A device that will not take it
    /// gets no pool, and its backend stays on CPU frames.
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        // SAFETY: plain COM calls on a live device; the immediate context is a
        // new reference, and the cast is a QueryInterface.
        unsafe {
            let context = device
                .GetImmediateContext()
                .map_err(|e| anyhow!("GetImmediateContext: {e}"))?;
            let multithread: ID3D11Multithread = context
                .cast()
                .map_err(|e| anyhow!("ID3D11Multithread is not available: {e}"))?;
            let _ = multithread.SetMultithreadProtected(true);
            if !multithread.GetMultithreadProtected().as_bool() {
                return Err(anyhow!("the device refused multithread protection"));
            }
        }
        Ok(Self {
            device: device.clone(),
            shared: Arc::new(Mutex::new(Shared {
                dims: (0, 0),
                free: Vec::new(),
                staging: None,
                staging_dims: (0, 0),
            })),
        })
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// A `width × height` BGRA texture to copy a frame into: a free one of
    /// those dims, or a new one.
    pub fn take(&self, width: u32, height: u32) -> Result<ID3D11Texture2D> {
        {
            let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if shared.dims != (width, height) {
                shared.dims = (width, height);
                shared.free.clear();
            }
            if let Some(texture) = shared.free.pop() {
                return Ok(texture);
            }
        }
        create_texture(&self.device, width, height)
    }

    /// `texture`, filled with a frame, as a [`GpuFrame`]. It comes back to this
    /// pool when the last clone of the frame is dropped.
    pub fn frame(&self, texture: ID3D11Texture2D, width: u32, height: u32) -> GpuFrame {
        GpuFrame(Arc::new(Inner {
            texture,
            device: self.device.clone(),
            width,
            height,
            cpu: OnceLock::new(),
            pool: Some(Arc::clone(&self.shared)),
        }))
    }

    /// How many textures are waiting in the pool (tests).
    #[cfg(test)]
    fn free_count(&self) -> usize {
        self.shared.lock().unwrap().free.len()
    }
}

/// A BGRA8 `DEFAULT` texture bound as a render target: a copy destination, and
/// both an input and an output of the D3D11 VideoProcessor (the DXGI arm's
/// scale writes into one, and the encoder's conversion reads it).
fn create_texture(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
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
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: a fully initialised desc, no initial data, a valid out-param.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| anyhow!("CreateTexture2D({width}x{height}): {e}"))?;
    texture.ok_or_else(|| anyhow!("CreateTexture2D returned no texture"))
}

/// Copy a BGRA texture into CPU memory, tightly packed: the staging copy, the
/// `Map` and the row copy every Windows capture backend makes today.
///
/// `pool` lends its staging texture; without one, a staging texture is made
/// for this copy.
fn read_bgra(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
    pool: Option<&Mutex<Shared>>,
) -> Result<Vec<u8>> {
    let mut guard = pool.map(|p| p.lock().unwrap_or_else(|e| e.into_inner()));
    let staging = match guard.as_deref_mut() {
        Some(shared) => {
            if shared.staging.is_none() || shared.staging_dims != (width, height) {
                shared.staging = Some(create_staging(device, width, height)?);
                shared.staging_dims = (width, height);
            }
            shared
                .staging
                .clone()
                .ok_or_else(|| anyhow!("no staging texture"))?
        }
        None => create_staging(device, width, height)?,
    };
    let row = width as usize * 4;
    let mut bytes = vec![0u8; row * height as usize];
    // SAFETY: `staging` and `texture` are textures of one format and dims on
    // `device`; the immediate context is a new reference; `Map` waits for the
    // copy and returns a pointer to at least `RowPitch × height` bytes, of
    // which each row copies `min(row, RowPitch)`. The staging texture is not
    // shared while `guard` is held.
    unsafe {
        let context = device
            .GetImmediateContext()
            .map_err(|e| anyhow!("GetImmediateContext: {e}"))?;
        context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| anyhow!("Map(staging): {e}"))?;
        let pitch = mapped.RowPitch as usize;
        let copy = row.min(pitch);
        let src = mapped.pData as *const u8;
        for y in 0..height as usize {
            std::ptr::copy_nonoverlapping(
                src.add(y * pitch),
                bytes.as_mut_ptr().add(y * row),
                copy,
            );
        }
        context.Unmap(&staging, 0);
    }
    drop(guard);
    Ok(bytes)
}

fn create_staging(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
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
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    // SAFETY: as in `create_texture`.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
        .map_err(|e| anyhow!("CreateTexture2D(staging {width}x{height}): {e}"))?;
    staging.ok_or_else(|| anyhow!("CreateTexture2D returned no staging texture"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{Damage, Frame, FrameData, PixelFormat};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
    };

    /// The switch is off unless it says on: unset, empty, `0` and anything
    /// unrecognised all leave a host on today's path.
    #[test]
    fn the_zero_copy_switch_is_off_unless_it_says_on() {
        for on in ["1", "true", "TRUE", "yes", "On", " on "] {
            assert!(zero_copy_from(Some(on)), "{on:?} turns it on");
        }
        for off in ["0", "false", "no", "off", "", "2", "enabled"] {
            assert!(!zero_copy_from(Some(off)), "{off:?} leaves it off");
        }
        assert!(!zero_copy_from(None));
    }

    /// P3 — the capture's device goes where the first proven GPU-input cell
    /// lives, in the order every codec's cascade opens backends: NVENC, QSV,
    /// AMF. Nothing proven keeps the default adapter.
    #[test]
    fn the_capture_follows_the_first_gpu_cell_in_the_cascade_s_backend_order() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(preferred_vendor(&names(&[])), None);
        assert_eq!(
            preferred_vendor(&names(&["av1_qsv", "h264_qsv"])),
            Some(0x8086)
        );
        assert_eq!(preferred_vendor(&names(&["hevc_amf"])), Some(0x1002));
        assert_eq!(
            preferred_vendor(&names(&["av1_qsv", "hevc_amf", "hevc_nvenc"])),
            Some(0x10de),
            "NVENC first, whatever order the probe listed them in"
        );
        assert_eq!(
            preferred_vendor(&names(&["h264_qsv", "hevc_amf"])),
            Some(0x8086)
        );
        assert_eq!(
            preferred_vendor(&names(&["libvpx-vp9-444-sw", "openh264-sw"])),
            None
        );
    }

    /// WARP: the software rasterizer every Windows has, so the test needs no GPU.
    fn warp() -> ID3D11Device {
        let mut device: Option<ID3D11Device> = None;
        // SAFETY: valid out-params; WARP needs no adapter.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .expect("a WARP device");
        device.expect("a WARP device")
    }

    /// A pattern no two pixels share in one row: B = x, G = y, R = x ^ y.
    fn pattern(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(width as usize * 4 * height as usize);
        for y in 0..height {
            for x in 0..width {
                bytes.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 0xff]);
            }
        }
        bytes
    }

    /// Fill a pool texture with `bytes` (tightly packed BGRA).
    fn fill(device: &ID3D11Device, texture: &ID3D11Texture2D, width: u32, bytes: &[u8]) {
        // SAFETY: `bytes` holds `height` rows of `width × 4` bytes; the texture
        // is a DEFAULT texture of those dims.
        unsafe {
            let context = device.GetImmediateContext().unwrap();
            context.UpdateSubresource(texture, 0, None, bytes.as_ptr().cast(), width * 4, 0);
        }
    }

    fn gpu_frame(pool: &TexturePool, width: u32, height: u32) -> (GpuFrame, Vec<u8>) {
        let expected = pattern(width, height);
        let texture = pool.take(width, height).unwrap();
        fill(pool.device(), &texture, width, &expected);
        (pool.frame(texture, width, height), expected)
    }

    /// AC2's GPU half: a GPU frame's `cpu()` is the bytes the texture holds,
    /// read back once and shared by every clone. The width is odd on purpose:
    /// the staging texture's rows are padded, and the copy must skip the padding.
    #[test]
    fn a_gpu_frame_reads_back_the_bytes_its_texture_holds() {
        let device = warp();
        let pool = TexturePool::new(&device).unwrap();
        let (frame, expected) = gpu_frame(&pool, 37, 21);
        assert_eq!(frame.cpu(), expected.as_slice());
        let first = frame.cpu().as_ptr();
        let clone = frame.clone();
        assert_eq!(
            clone.cpu().as_ptr(),
            first,
            "a clone shares the one readback"
        );
        assert_eq!(frame.dims(), (37, 21));
    }

    /// Through `Frame`: `cpu()` reads the texture, and `cpu_mut()` turns the
    /// frame into a CPU frame whose bytes can be changed in place.
    #[test]
    fn a_gpu_frame_becomes_a_cpu_frame_when_a_consumer_writes() {
        let device = warp();
        let pool = TexturePool::new(&device).unwrap();
        let (gpu, expected) = gpu_frame(&pool, 16, 8);
        let mut frame = Frame {
            width: 16,
            height: 8,
            stride: 16 * 4,
            pixel_format: PixelFormat::Bgra,
            data: FrameData::Gpu(gpu),
            monotonic_us: 0,
            monitor: 0,
            damage: Damage::Unknown,
            source: None,
        };
        assert_eq!(frame.cpu(), expected.as_slice());
        frame.cpu_mut()[0] = 0x42;
        assert!(matches!(frame.data, FrameData::Cpu(_)));
        assert_eq!(frame.cpu()[0], 0x42);
        assert_eq!(&frame.cpu()[1..], &expected[1..]);
    }

    /// A texture goes back to its pool when the last clone of its frame is
    /// dropped, and comes out again for the same dims, never for others.
    #[test]
    fn a_texture_returns_to_its_pool_when_the_frame_is_dropped() {
        let device = warp();
        let pool = TexturePool::new(&device).unwrap();
        let (frame, _) = gpu_frame(&pool, 8, 8);
        let raw = frame.texture().as_raw();
        let clone = frame.clone();
        drop(frame);
        assert_eq!(pool.free_count(), 0, "a clone still holds it");
        drop(clone);
        assert_eq!(pool.free_count(), 1);
        // Both textures stay alive below: a released one's address can be
        // reused by the next, so only live ones can be told apart by pointer.
        let again = pool.take(8, 8).unwrap();
        assert_eq!(again.as_raw(), raw, "the same texture again");
        let (other, _) = gpu_frame(&pool, 4, 4);
        assert_ne!(
            other.texture().as_raw(),
            again.as_raw(),
            "other dims get another texture"
        );
        drop(pool.frame(again, 8, 8));
        assert_eq!(
            pool.free_count(),
            0,
            "a texture of the old dims is not kept"
        );
        drop(other);
        assert_eq!(pool.free_count(), 1);
    }

    /// The pool turned the device's multithread protection on, and a frame
    /// read back on another thread gets the same bytes.
    #[test]
    fn a_gpu_frame_reads_back_on_another_thread() {
        let device = warp();
        let pool = TexturePool::new(&device).unwrap();
        // SAFETY: plain COM calls on a live device.
        let protected = unsafe {
            let context = device.GetImmediateContext().unwrap();
            let multithread: ID3D11Multithread = context.cast().unwrap();
            multithread.GetMultithreadProtected().as_bool()
        };
        assert!(protected);
        let (frame, expected) = gpu_frame(&pool, 24, 12);
        let bytes = std::thread::spawn(move || frame.cpu().to_vec())
            .join()
            .unwrap();
        assert_eq!(bytes, expected);
    }
}
