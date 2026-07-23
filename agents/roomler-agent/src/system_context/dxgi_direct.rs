//! Direct (adapter-bound) DXGI Desktop Duplication backend for the
//! SYSTEM-context capture path.
//!
//! ## Why this exists (rc.108, Phase 1 of the hybrid-GPU fix)
//!
//! The existing [`super::dxgi_dup::DxgiDupBackend`] wraps `scrap-0.5.0`,
//! which creates its D3D11 device on the *default* adapter and gives the
//! caller no say in adapter / output selection. On a single-GPU box that
//! is fine. On a hybrid "Optimus" laptop — Intel iGPU drives the display,
//! NVIDIA dGPU is render-only with **zero attached outputs** — `scrap`
//! can bind Desktop Duplication to the render-only dGPU; `DuplicateOutput`
//! then fails (the dGPU owns no output to duplicate) and the capture pump
//! falls through to the slow GDI BitBlt path (~85 ms/frame ⇒ ~12 fps).
//! Field host PC55331 (rc.105 telemetry: `backend=gdi`, Intel owns the
//! primary output, NVIDIA reports 0 outputs, `scrap::Capturer::new:
//! permission denied`) is the motivating case.
//!
//! This backend talks the `windows` crate's DXGI/D3D11 API directly so we
//! can:
//!   1. enumerate adapters + their outputs (reusing the same logic the
//!      rc.105 [`super::dxgi_util`] diagnostic already proved correct),
//!   2. pick the adapter that **owns the primary output** (its top-left is
//!      the virtual-desktop origin 0,0), and
//!   3. create the D3D11 device on *that* adapter and `DuplicateOutput`
//!      the primary output — so on Optimus we bind to the iGPU, exactly
//!      where the display lives, and DXGI stays on the fast path.
//!
//! On an Intel-only host (PC50054, already 62 fps via the scrap path) the
//! primary-output adapter IS the only adapter, so this backend binds to
//! the same GPU `scrap` would have — no behaviour change, just an explicit
//! adapter handle.
//!
//! ## Same `BackendBail` contract as the scrap backend
//!
//! Implements [`super::dxgi_dup::DxgiCapture`] so the capture pump consumes
//! either backend through one trait object. HRESULT → `BackendBail`:
//!
//! | HRESULT | `BackendBail` | Pump action |
//! |---|---|---|
//! | `DXGI_ERROR_WAIT_TIMEOUT` | `Transient` | retry next tick (static desktop) |
//! | `DXGI_ERROR_ACCESS_LOST` | `AccessLost` | `reset()` (desktop transition / device-lost) |
//! | `E_ACCESSDENIED` | `DesktopMismatch` | `try_change_desktop` then retry |
//! | (other) | `HardError` | fall to scrap → GDI |
//!
//! ## Threading
//!
//! D3D11 device/context + the duplication object have thread affinity at
//! runtime. The capture pump owns one of these on its dedicated
//! `roomler-agent-system-capture` thread and drives it synchronously;
//! nobody else touches it. We never send it across threads.
//!
//! ## Feature gating
//!
//! Compiled only when BOTH `mf-encoder` (pulls in the `windows` crate) and
//! `scrap-capture` (the [`DxgiFrame`] / [`DxgiCapture`] surface lives under
//! that flag, and we keep the scrap backend as the fallback) are enabled —
//! i.e. the production `full-hw,system-context` MSI. Builds without
//! `mf-encoder` keep the scrap-only path unchanged.

#![cfg(all(
    target_os = "windows",
    feature = "mf-encoder",
    feature = "scrap-capture"
))]

use std::io;

use windows::Win32::Foundation::{E_ACCESSDENIED, E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1,
    IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use super::dxgi_dup::{BackendBail, DxgiCapture, DxgiFrame};
use crate::fp16;

/// Map a `windows::core::Error` HRESULT to the capture pump's typed bail.
/// Mirrors the scrap backend's `io::ErrorKind`-based table so both DXGI
/// backends route identically through the pump.
fn map_dxgi_err(e: windows::core::Error) -> BackendBail {
    let code = e.code();
    if code == DXGI_ERROR_WAIT_TIMEOUT {
        // No new frame since the last AcquireNextFrame — the desktop is
        // static. Fires constantly on an idle screen; never log it.
        BackendBail::Transient
    } else if code == DXGI_ERROR_ACCESS_LOST {
        // Desktop transition (lock/unlock) or GPU device-lost. Caller
        // rebuilds via reset().
        BackendBail::AccessLost
    } else if code == E_ACCESSDENIED {
        // Thread's desktop binding doesn't match the input desktop.
        BackendBail::DesktopMismatch
    } else {
        BackendBail::HardError(io::Error::other(format!("DXGI-direct: {e}")))
    }
}

/// Adapter-bound DXGI Desktop Duplication. Owns the D3D11 device/context,
/// the duplication object, and a lazily-(re)created CPU-readable staging
/// texture. Not driven from more than one thread.
pub struct DxgiDirectBackend {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    width: u32,
    height: u32,
    /// CPU-readable copy target. `CopyResource` the acquired (GPU-only)
    /// desktop texture into this, then `Map` it for readback. Recreated
    /// when the source dimensions / format change (resolution swap).
    staging: Option<ID3D11Texture2D>,
    staging_w: u32,
    staging_h: u32,
    staging_fmt: DXGI_FORMAT,
    /// rc.207 — half-bits → sRGB-u8 table for FP16 (scRGB) desktops (ACM /
    /// HDR). Built lazily on the first FP16 frame; None on plain BGRA8
    /// desktops. See [`crate::fp16`] for the field incident that motivated
    /// accepting FP16 here instead of bailing to the scrap path (which reads
    /// FP16 surfaces as BGRA8 → purple 2×-zoomed garbage).
    lut: Option<Box<[u8; 65536]>>,
}

impl DxgiDirectBackend {
    /// Enumerate adapters, find the one owning the primary output, create
    /// a D3D11 device on it, and start Desktop Duplication on that output.
    ///
    /// Returns `BackendBail::HardError` when no adapter owns a primary
    /// output, the desktop format is neither BGRA8 nor FP16 (10-bit etc. —
    /// out of scope; let the caller fall to scrap/GDI), or any DXGI call
    /// fails for a non-typed reason. The capture pump treats a `HardError`
    /// here as "try the next backend" (scrap, then GDI). FP16 (scRGB — the
    /// ACM/HDR desktop composition format) is ACCEPTED since rc.207 and
    /// converted to sRGB BGRA8 per frame via [`crate::fp16`] — critically,
    /// this backend must own that case because the scrap fallback misreads
    /// FP16 surfaces as BGRA8 (field DESKTOP-V6FJE58: purple 2×-zoomed
    /// flicker on every recomposited frame).
    pub fn primary() -> Result<Self, BackendBail> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(map_dxgi_err)?;

            let (adapter, output, adapter_name) =
                find_primary_output(&factory).ok_or_else(|| {
                    BackendBail::HardError(io::Error::other(
                        "DXGI-direct: no adapter owns a primary output at origin (0,0)",
                    ))
                })?;

            // Each step maps to a distinct HRESULT on failure (create:
            // DXGI_ERROR_UNSUPPORTED on an idle Optimus adapter; DuplicateOutput:
            // E_INVALIDARG cross-adapter / E_ACCESSDENIED on the secure desktop),
            // and try_build_dxgi logs the whole error before falling back to
            // scrap — so the message is self-disambiguating without per-step logs.
            let (device, context) = create_device_on(&adapter).map_err(map_dxgi_err)?;

            // Desktop Duplication lives on IDXGIOutput1.
            let output1: IDXGIOutput1 = output.cast().map_err(map_dxgi_err)?;
            let duplication = output1.DuplicateOutput(&device).map_err(map_dxgi_err)?;

            // IDXGIOutputDuplication::GetDesc returns the desc by value
            // (no out-param) in windows-rs 0.58.
            let desc = duplication.GetDesc();
            let width = desc.ModeDesc.Width;
            let height = desc.ModeDesc.Height;

            let desktop_fmt = desc.ModeDesc.Format;
            let fp16 = desktop_fmt == DXGI_FORMAT_R16G16B16A16_FLOAT;
            if desktop_fmt != DXGI_FORMAT_B8G8R8A8_UNORM && !fp16 {
                // 10-bit scanout (R10G10B10A2) and other exotic formats stay
                // out of scope — bail so the pump falls to scrap/GDI. FP16 is
                // handled below (rc.207): it's what ACM/HDR desktops hand out,
                // and the scrap fallback misreads it as BGRA8 (purple garbage),
                // so the direct backend must own that case.
                return Err(BackendBail::HardError(io::Error::other(format!(
                    "DXGI-direct: desktop format {:?} is not BGRA8/FP16 — falling back",
                    desktop_fmt.0
                ))));
            }
            if fp16 {
                // Loud on purpose: this is the observable marker that a host
                // composites in scRGB (Settings → Display → Advanced display →
                // "Automatically manage color for apps", or true HDR). Costs a
                // few ms/frame of CPU convert; turning ACM/HDR off on the host
                // removes it. Field: DESKTOP-V6FJE58 purple-flicker incident.
                tracing::warn!(
                    width,
                    height,
                    "DXGI-direct: FP16 (scRGB) desktop detected — ACM/HDR is ON; converting to sRGB on CPU (disable 'Automatically manage color for apps' on this host to avoid the convert cost)"
                );
            }

            tracing::info!(
                adapter = %adapter_name,
                width,
                height,
                fp16,
                "DXGI-direct: bound Desktop Duplication to the primary-output adapter (hybrid-GPU fix)"
            );

            Ok(Self {
                device,
                context,
                duplication,
                width,
                height,
                staging: None,
                staging_w: 0,
                staging_h: 0,
                staging_fmt: desktop_fmt,
                lut: None,
            })
        }
    }

    /// Ensure `self.staging` is a STAGING texture matching the acquired
    /// frame's dimensions + format. Recreates on a resolution change and
    /// updates `self.width`/`height` to the authoritative texture size.
    fn ensure_staging(&mut self, src: &ID3D11Texture2D) -> Result<(), BackendBail> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: src is a valid ID3D11Texture2D from AcquireNextFrame.
        unsafe { src.GetDesc(&mut desc) };

        let stale = match self.staging {
            Some(_) => {
                self.staging_w != desc.Width
                    || self.staging_h != desc.Height
                    || self.staging_fmt != desc.Format
            }
            None => true,
        };
        if !stale {
            return Ok(());
        }

        // Start from the source texture's desc and override usage/access so
        // the staging copy is CPU-readable. The flag fields are raw u32 in
        // windows-rs 0.58 (not the typed newtypes used at the call sites).
        let mut sdesc = desc;
        sdesc.Usage = D3D11_USAGE_STAGING;
        sdesc.BindFlags = 0;
        sdesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        sdesc.MiscFlags = 0;

        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY: sdesc is fully initialised; no initial data; out-param.
        unsafe {
            self.device
                .CreateTexture2D(&sdesc, None, Some(&mut staging))
        }
        .map_err(map_dxgi_err)?;
        let staging = staging.ok_or_else(|| {
            BackendBail::HardError(io::Error::other(
                "DXGI-direct: CreateTexture2D null staging",
            ))
        })?;

        self.staging = Some(staging);
        self.staging_w = desc.Width;
        self.staging_h = desc.Height;
        self.staging_fmt = desc.Format;
        // Authoritative dimensions come from the actual desktop texture,
        // not the duplication ModeDesc (handles a mid-session resolution
        // change without a full reset()).
        self.width = desc.Width;
        self.height = desc.Height;
        Ok(())
    }

    /// Copy the acquired GPU texture into the staging texture, map it, and
    /// read out a tightly-packed BGRA8 buffer. Called between
    /// AcquireNextFrame and ReleaseFrame.
    fn read_acquired(&mut self, resource: Option<IDXGIResource>) -> Result<DxgiFrame, BackendBail> {
        let resource = resource.ok_or_else(|| {
            BackendBail::HardError(io::Error::other(
                "DXGI-direct: AcquireNextFrame returned a null resource",
            ))
        })?;
        let src: ID3D11Texture2D = resource.cast().map_err(map_dxgi_err)?;

        self.ensure_staging(&src)?;
        let staging = self
            .staging
            .as_ref()
            .expect("ensure_staging guarantees Some on Ok");

        // GPU copy desktop → CPU-readable staging.
        // SAFETY: staging + src are valid, format/dim-matched textures.
        unsafe { self.context.CopyResource(staging, &src) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging is a STAGING texture with CPU read access; the
        // mapped out-param is valid for the duration of the call.
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .map_err(map_dxgi_err)?;

        let w = self.width as usize;
        let h = self.height as usize;
        let stride = w * 4;
        let mut bytes = vec![0u8; stride * h];
        if self.staging_fmt == DXGI_FORMAT_R16G16B16A16_FLOAT {
            // rc.207 — FP16 (scRGB) desktop: convert each RGBA16F row (8 B/px)
            // to BGRA8 through the half→sRGB LUT. `ensure_staging` keeps
            // `staging_fmt` in sync with the ACTUAL acquired texture, so an
            // ACM toggle mid-session flips this branch on the next frame.
            let lut = self.lut.get_or_insert_with(fp16::build_half_to_srgb_lut);
            // SAFETY: mapped.pData points to at least RowPitch*height bytes;
            // each row slice is bounded by min(w, RowPitch/8) pixels so we
            // never over-read a row (RowPitch >= w*8 in practice — driver
            // pads rows up), and the dst row is exactly `stride` bytes.
            unsafe {
                let src_ptr = mapped.pData as *const u8;
                let row_pitch = mapped.RowPitch as usize;
                let px = w.min(row_pitch / 8);
                for y in 0..h {
                    let src_row = std::slice::from_raw_parts(src_ptr.add(y * row_pitch), px * 8);
                    fp16::convert_row_rgba16f_to_bgra8(
                        src_row,
                        &mut bytes[y * stride..(y + 1) * stride],
                        px,
                        lut,
                    );
                }
                self.context.Unmap(staging, 0);
            }
        } else {
            // SAFETY: mapped.pData points to at least RowPitch*height bytes;
            // we copy min(stride, RowPitch) per row into a stride*height buf,
            // so neither side is over-read / over-written. RowPitch >= stride
            // always (driver pads rows up), so copy_w == stride in practice.
            unsafe {
                let src_ptr = mapped.pData as *const u8;
                let row_pitch = mapped.RowPitch as usize;
                let copy_w = stride.min(row_pitch);
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(
                        src_ptr.add(y * row_pitch),
                        bytes.as_mut_ptr().add(y * stride),
                        copy_w,
                    );
                }
                self.context.Unmap(staging, 0);
            }
        }

        Ok(DxgiFrame {
            bytes,
            width: self.width,
            height: self.height,
            stride: stride as u32,
        })
    }
}

impl DxgiCapture for DxgiDirectBackend {
    fn frame(&mut self) -> Result<DxgiFrame, BackendBail> {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // timeout=0 → non-blocking: returns DXGI_ERROR_WAIT_TIMEOUT
        // immediately on a static desktop (mapped to Transient). The
        // capture pump owns cadence; we never block the worker thread.
        // SAFETY: out-params are valid; duplication is live.
        unsafe {
            self.duplication
                .AcquireNextFrame(0, &mut frame_info, &mut resource)
        }
        .map_err(map_dxgi_err)?;

        // From here we hold the frame and MUST ReleaseFrame before the
        // next AcquireNextFrame, on every path. read_acquired never
        // touches self.duplication, so the borrows don't overlap.
        let result = self.read_acquired(resource);
        // SAFETY: pairs with the AcquireNextFrame that just succeeded.
        unsafe {
            if let Err(e) = self.duplication.ReleaseFrame() {
                tracing::trace!(?e, "DXGI-direct: ReleaseFrame (non-fatal)");
            }
        }
        result
    }

    fn reset(&mut self) -> Result<(), BackendBail> {
        // Rebuild everything (adapter re-enum + device + duplication). The
        // display config may have changed across the AccessLost (resolution
        // swap during a lock screen), so re-deriving from scratch is the
        // safe move — same as the scrap backend's reset().
        *self = Self::primary()?;
        Ok(())
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn kind(&self) -> &'static str {
        // Distinct name so the capture-timing heartbeat (`backend=` field in
        // agent_logs) shows fleet-wide which hosts are paying the FP16
        // convert — the observable half of the rc.207 ACM/HDR fix.
        if self.staging_fmt == DXGI_FORMAT_R16G16B16A16_FLOAT {
            "dxgi-direct-fp16"
        } else {
            "dxgi-direct"
        }
    }
}

/// Walk every DXGI adapter + output; return the (adapter, output, adapter
/// name) that owns the primary output — the one whose desktop rect top-left
/// is the virtual-desktop origin (0,0). Software adapters are skipped. This
/// is the adapter Desktop Duplication must bind to; on Optimus it's the
/// iGPU (the dGPU owns no output).
///
/// # Safety
/// Calls DXGI enumeration vtable methods; `factory` must be a live
/// `IDXGIFactory1`.
unsafe fn find_primary_output(
    factory: &IDXGIFactory1,
) -> Option<(IDXGIAdapter1, IDXGIOutput, String)> {
    let mut adapter_index = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(a) => a,
            Err(_) => break,
        };
        adapter_index += 1;

        let desc1 = unsafe { adapter.GetDesc1() }.ok();
        // Skip software / WARP adapters — they own no real display output.
        if let Some(d) = &desc1
            && (DXGI_ADAPTER_FLAG(d.Flags as i32).0 & DXGI_ADAPTER_FLAG_SOFTWARE.0) != 0
        {
            continue;
        }
        let name = desc1
            .as_ref()
            .map(|d| utf16_trim(&d.Description))
            .unwrap_or_default();

        let mut output_index = 0u32;
        loop {
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(_) => break,
            };
            output_index += 1;
            if let Ok(od) = unsafe { output.GetDesc() } {
                let r = od.DesktopCoordinates;
                if od.AttachedToDesktop.as_bool() && r.left == 0 && r.top == 0 {
                    return Some((adapter, output, name));
                }
            }
        }
    }
    None
}

/// Build a D3D11 device + immediate context bound to a specific adapter.
/// Driver type MUST be `UNKNOWN` when an explicit adapter is supplied
/// (passing HARDWARE is the canonical DXGI foot-gun → `E_INVALIDARG`).
/// BGRA support is required so the BGRA8 desktop texture maps cleanly.
///
/// # Safety
/// Calls `D3D11CreateDevice`; `adapter` must be a live `IDXGIAdapter1`.
unsafe fn create_device_on(
    adapter: &IDXGIAdapter1,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let adapter_base: IDXGIAdapter = adapter.cast()?;
    let feature_levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut level = D3D_FEATURE_LEVEL_11_0;
    unsafe {
        D3D11CreateDevice(
            &adapter_base,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )?;
    }
    let device = device.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    let context = context.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    Ok((device, context))
}

/// Trim a fixed-size NUL-terminated UTF-16 buffer (adapter description) to
/// a `String`.
fn utf16_trim(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_dxgi_err_classifies_typed_hresults() {
        assert!(matches!(
            map_dxgi_err(windows::core::Error::from(DXGI_ERROR_WAIT_TIMEOUT)),
            BackendBail::Transient
        ));
        assert!(matches!(
            map_dxgi_err(windows::core::Error::from(DXGI_ERROR_ACCESS_LOST)),
            BackendBail::AccessLost
        ));
        assert!(matches!(
            map_dxgi_err(windows::core::Error::from(E_ACCESSDENIED)),
            BackendBail::DesktopMismatch
        ));
        assert!(matches!(
            map_dxgi_err(windows::core::Error::from(E_FAIL)),
            BackendBail::HardError(_)
        ));
    }

    #[test]
    fn utf16_trim_stops_at_nul() {
        let mut buf = [0u16; 8];
        for (i, c) in "GPU".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(utf16_trim(&buf), "GPU");
    }

    #[test]
    fn primary_does_not_panic_under_test_runner() {
        // On a real Win11 desktop this binds to the primary-output
        // adapter; on headless CI it returns HardError at factory /
        // enumeration. Lock against panic, not a specific outcome.
        let _ = DxgiDirectBackend::primary();
    }
}
