// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-36 P1 — DRM/KMS capture: read the scanout framebuffer from the kernel,
//! **below the compositor**.
//!
//! This exists because a Wayland desktop is uncapturable by every other
//! backend we have: XShm needs an X root window, and `xdg-desktop-portal`'s
//! ScreenCast is structurally an *attended* API — it needs an interactive
//! picker, it only lives inside an active user session, and while the session
//! is LOCKED mutter refuses to create *or restore* a screencast (consuming the
//! saved restore token in the process). A locked host is the normal state of
//! an unattended machine, so the portal refuses in exactly the case remote
//! access exists for. Reading the scanout plane sidesteps the compositor and
//! its permission prompt entirely: one code path for GNOME, KDE, XFCE, X11 —
//! and for no session at all. See `docs/fr/FR-36-wayland-capture.md`.
//!
//! ## What this backend deliberately does NOT do
//!
//! - **No damage tracking.** DRM tells us nothing about which pixels changed,
//!   and neither buffer identity nor a page flip is a proxy: X11 renders
//!   in-place into a stable framebuffer (same handle, new content) while a
//!   Wayland compositor flips between buffers every frame (new handle,
//!   possibly identical content). So every frame is [`Damage::Unknown`] and
//!   `frames_unchanged` stays 0 — the honest answers. This is why the backend
//!   is **opt-in** rather than the Linux default; see [`env_enabled`].
//! - **Primary CRTC only.** Multi-monitor is per-CRTC capture with origin and
//!   scale mapping, deferred by the spec.
//! - **No detiling.** Measured on Apple Silicon, the `apple-drm` plane
//!   advertises `DRM_FORMAT_MOD_LINEAR` and nothing else, so a tiled scanout
//!   buffer is not representable there. Tiled-scanout GPUs (Intel `*_RC_CCS`,
//!   Nvidia block-linear) need an EGL import step that is FR-36 P2; until it
//!   lands this backend REFUSES a non-linear modifier rather than emitting
//!   garbage, because garbage pixels read as a codec bug and cost days.

use anyhow::{Result, anyhow, bail};
use drm::Device as BasicDevice;
use drm::control::Device as ControlDevice;
use std::fs::{File, OpenOptions};
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::info;

use drm::buffer::DrmFourcc;

use super::{Damage, DownscalePolicy, Frame, PixelFormat, ScreenCapture};

/// `/dev/dri/cardN` nodes to probe. The kernel numbers these from 0, but the
/// numbering is NOT stable across kernel upgrades — on the Asahi field host the
/// display controller moved from `card0` to `card2` when the kernel was
/// updated, with the render-only `asahi` node landing on `card1`. Hence the
/// scan-and-discriminate in [`open_display_node`] rather than a fixed path.
const MAX_CARD_INDEX: u32 = 16;

/// A DRM device node. `drm`'s traits are blanket-implemented for anything that
/// can hand over a borrowed fd, so this newtype is the whole adapter.
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl BasicDevice for Card {}
impl ControlDevice for Card {}

/// An `mmap`ed dma-buf, unmapped on drop.
///
/// Held across frames and keyed by framebuffer handle: re-exporting and
/// re-mapping every frame measured 2.07 ms against 1.58 ms for a cached
/// mapping at 1080p, and a compositor cycles through a small set of buffers,
/// so the cache hits nearly always after the first few frames.
struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: the pointer is a private `mmap` of a dma-buf owned by this struct.
// Nothing else aliases it, and `Drop` is the only unmap.
unsafe impl Send for Mapping {}

impl Mapping {
    fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr`/`len` come from a successful `mmap` and stay valid for
        // the lifetime of `self`; the region is mapped PROT_READ.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what we mapped, once.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// Runtime gate. **Default OFF** — deliberately the inverse of the usual
/// "kill switch" shape, because switching every Linux host to this backend
/// would silently undo FR-29: the X11 path tracks damage and idles at ~2.8 %
/// of a core, whereas DRM has no damage information at all and would grab
/// every frame forever. So a host opts IN (`ROOMLERD_DRM_CAPTURE=1`) — which
/// is what a Wayland or headless host wants — and everything else keeps the
/// existing cascade byte-for-byte.
pub fn env_enabled() -> bool {
    tunnel_core::env::flag("DRM_CAPTURE", false)
}

/// Open the first `/dev/dri/cardN` that is a **display** node.
///
/// The discriminator is `resource_handles()` reporting at least one CRTC and
/// one connector: a render-only node (Asahi's `asahi`, Nvidia's `renderD*`
/// sibling) has neither. Never trust the index — see [`MAX_CARD_INDEX`].
fn open_display_node() -> Result<(Card, String)> {
    let mut tried = 0usize;
    for i in 0..MAX_CARD_INDEX {
        let path = format!("/dev/dri/card{i}");
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        tried += 1;
        let card = Card(file);
        let Ok(res) = card.resource_handles() else {
            continue;
        };
        if res.crtcs().is_empty() || res.connectors().is_empty() {
            continue;
        }
        return Ok((card, path));
    }
    bail!(
        "no DRM display node found ({tried} /dev/dri/card* opened, none had CRTCs+connectors). \
         A render-only node is not enough; this needs the display controller."
    )
}

/// The live scanout framebuffer on the primary plane, if there is one.
///
/// ⚠️ `plane_handles()` returns ONLY overlay planes until
/// `DRM_CLIENT_CAP_UNIVERSAL_PLANES` is set — without it the primary plane is
/// invisible and this returns `None` on a perfectly healthy display. That is a
/// silent empty result, not an error, which is why the cap is set once at open
/// and asserted here by comment rather than discovered again later.
fn primary_plane_fb(card: &Card) -> Option<drm::control::framebuffer::Handle> {
    let planes = card.plane_handles().ok()?;
    let mut fallback = None;
    for ph in planes {
        let Ok(info) = card.get_plane(ph) else {
            continue;
        };
        let (Some(_crtc), Some(fb)) = (info.crtc(), info.framebuffer()) else {
            continue;
        };
        // A plane bound to a CRTC and holding a framebuffer is live scanout.
        // Prefer the first such plane; cursor/overlay planes are normally
        // unbound (`crtc=None`) so this resolves to the primary in practice.
        if fallback.is_none() {
            fallback = Some(fb);
        }
    }
    fallback
}

/// Repack one scanout framebuffer into tightly-packed BGRA.
///
/// ⚠️ **The fourcc must be honoured, not assumed.** X11 handed us 8-bit
/// `XR24`, GNOME/mutter handed us 10-bit `XR30` on the same display — and
/// decoding `XR30` with the 8-bit layout yields a *structurally perfect,
/// psychedelic* frame: correct geometry, correct layout, colours pure noise.
/// That reads as a colour-space bug rather than a format bug, so it is exactly
/// the mistake that survives review.
fn repack_to_bgra(src: &[u8], width: u32, height: u32, pitch: u32, ten_bit: bool) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let pitch = pitch as usize;
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let row = &src[y * pitch..y * pitch + w * 4];
        let dst = &mut out[y * w * 4..(y + 1) * w * 4];
        if ten_bit {
            // XR30/AR30: one packed LE word, x:R:G:B 2:10:10:10. Drop the low
            // 2 bits per channel to reach 8-bit BGRA; alpha is opaque because
            // the 2-bit `x` field is padding on the X-variant.
            //
            // Written as paired `chunks_exact` rather than indexed arithmetic
            // so the bounds checks fold away and LLVM can vectorise: this loop
            // runs 8.8 M times per frame at 4K and was measured as the single
            // dominant cost of the whole backend there.
            for (d, s) in dst.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                d[0] = ((v & 0x3FF) >> 2) as u8; // B
                d[1] = (((v >> 10) & 0x3FF) >> 2) as u8; // G
                d[2] = (((v >> 20) & 0x3FF) >> 2) as u8; // R
                d[3] = 0xFF;
            }
        } else {
            // XR24/AR24 is already B,G,R,X in memory — the Frame contract's
            // byte order. Copy the row wholesale, then force alpha opaque so
            // an X-variant (where the 4th byte is undefined) cannot render
            // transparent downstream.
            dst.copy_from_slice(row);
            for px in dst.chunks_exact_mut(4) {
                px[3] = 0xFF;
            }
        }
    }
    out
}

enum WorkerReply {
    Frame(Box<Option<Frame>>),
    Failed(String),
}

type FrameRequest = oneshot::Sender<WorkerReply>;

/// DRM/KMS screen capture over the primary CRTC's scanout plane.
pub struct DrmCapture {
    cmd_tx: std_mpsc::Sender<FrameRequest>,
    width: u32,
    height: u32,
    target_frame_period: Duration,
    last_frame_at: Option<Instant>,
}

impl DrmCapture {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Open the primary CRTC's scanout plane.
    ///
    /// Fails (rather than degrading) when there is no display node, no live
    /// framebuffer, or the scanout modifier is not linear — each of those
    /// produces a wrong picture rather than a slow one, and the caller's
    /// cascade has a working fallback.
    pub fn primary(target_fps: u32, downscale: DownscalePolicy) -> Result<Self> {
        // Build the device on the worker thread so the fd and every mapping
        // stay on one thread for their whole life, and ack readiness back
        // synchronously so an init failure surfaces to the cascade here rather
        // than as a silently dead capturer.
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(u32, u32, String)>>();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<FrameRequest>();

        std::thread::Builder::new()
            .name("drm-capture".into())
            .spawn(move || worker(ready_tx, cmd_rx, downscale))
            .map_err(|e| anyhow!("spawn drm-capture worker: {e}"))?;

        let (width, height, path) = ready_rx
            .recv()
            .map_err(|_| anyhow!("drm-capture worker exited before reporting readiness"))??;

        info!(
            node = path.as_str(),
            width, height, "capture: backend=drm (scanout plane, below the compositor)"
        );

        Ok(Self {
            cmd_tx,
            width,
            height,
            target_frame_period: Duration::from_micros(1_000_000 / target_fps.max(1) as u64),
            last_frame_at: None,
        })
    }
}

/// The capture thread: owns the device, the cached mapping, and every unsafe
/// region in this module.
fn worker(
    ready_tx: std_mpsc::Sender<Result<(u32, u32, String)>>,
    cmd_rx: std_mpsc::Receiver<FrameRequest>,
    downscale: DownscalePolicy,
) {
    let (card, path) = match open_display_node() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // ⚠️ Without this the primary plane is INVISIBLE to `plane_handles()` and
    // every grab would come back empty on a working display.
    if let Err(e) = card.set_client_capability(drm::ClientCapability::UniversalPlanes, true) {
        let _ = ready_tx.send(Err(anyhow!(
            "DRM_CLIENT_CAP_UNIVERSAL_PLANES refused ({e}) — the primary plane would be invisible"
        )));
        return;
    }

    // Resolve the geometry once so the caller gets a real size, and so an
    // unusable modifier is refused at open rather than per frame.
    let Some(fb) = primary_plane_fb(&card) else {
        let _ = ready_tx.send(Err(anyhow!(
            "no plane is bound to a CRTC with a live framebuffer — is anything being scanned out? \
             (a blanked screen stops scanout, which is why screen blanking must be disabled)"
        )));
        return;
    };
    let info = match card.get_planar_framebuffer(fb) {
        Ok(i) => i,
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow!(
                "drmModeGetFB2 failed ({e}) — this needs CAP_SYS_ADMIN to receive the GEM handle"
            )));
            return;
        }
    };
    if !modifier_is_linear(&info) {
        let _ = ready_tx.send(Err(anyhow!(
            "scanout modifier {:?} is not linear — detiling is FR-36 P2 and is not implemented; \
             refusing rather than emitting garbage pixels",
            info.modifier()
        )));
        return;
    }
    let (w, h) = info.size();
    if ready_tx.send(Ok((w, h, path))).is_err() {
        return;
    }

    let mut cached: Option<(drm::control::framebuffer::Handle, Mapping)> = None;

    while let Ok(reply_tx) = cmd_rx.recv() {
        let reply = match grab(&card, &mut cached, downscale) {
            Ok(f) => WorkerReply::Frame(Box::new(f)),
            Err(e) => WorkerReply::Failed(format!("{e:#}")),
        };
        if reply_tx.send(reply).is_err() {
            // Caller went away mid-frame; keep serving the next one.
            continue;
        }
    }
}

/// `None` means "the driver did not report a modifier", which the kernel uses
/// for pre-modifier framebuffers — those are linear by definition.
fn modifier_is_linear(info: &drm::control::framebuffer::PlanarInfo) -> bool {
    match info.modifier() {
        None => true,
        Some(m) => u64::from(m) == 0,
    }
}

fn grab(
    card: &Card,
    cached: &mut Option<(drm::control::framebuffer::Handle, Mapping)>,
    downscale: DownscalePolicy,
) -> Result<Option<Frame>> {
    let Some(fb) = primary_plane_fb(card) else {
        // Nothing being scanned out right now (mode change, blanked panel).
        // Not an error: report "no frame" and let the pump retry.
        return Ok(None);
    };
    let info = card
        .get_planar_framebuffer(fb)
        .map_err(|e| anyhow!("drmModeGetFB2: {e}"))?;

    if !modifier_is_linear(&info) {
        bail!(
            "scanout modifier changed to {:?} (non-linear) — detiling is FR-36 P2",
            info.modifier()
        );
    }

    let (width, height) = info.size();
    let pitch = info.pitches()[0];
    let ten_bit = match info.pixel_format() {
        DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888 => false,
        DrmFourcc::Xrgb2101010 | DrmFourcc::Argb2101010 => true,
        other => bail!("unhandled scanout pixel format {other:?}"),
    };
    let len = pitch as usize * height as usize;

    // Re-map only when the compositor handed us a different buffer.
    let needs_map = !matches!(cached, Some((h, m)) if *h == fb && m.len == len);
    if needs_map {
        let handle = info.buffers()[0]
            .ok_or_else(|| anyhow!("framebuffer has no GEM handle — CAP_SYS_ADMIN missing?"))?;
        let prime = card
            .buffer_to_prime_fd(handle, 0)
            .map_err(|e| anyhow!("PRIME export: {e}"))?;
        // SAFETY: a fresh private read-only mapping of the exported dma-buf,
        // sized from the framebuffer's own pitch × height.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                prime.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(anyhow!(
                "mmap of the scanout dma-buf failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        *cached = Some((fb, Mapping { ptr, len }));
    }

    let Some((_, mapping)) = cached.as_ref() else {
        return Ok(None);
    };
    // Decide the output size BEFORE touching pixels, so a downscaled frame is
    // produced in ONE pass instead of building a full-size intermediate and
    // filtering it afterwards. At 4K that is the difference between
    // read 35 MB → write 35 MB → read 35 MB → write 8.8 MB and
    // read 35 MB → write 8.8 MB; the backend there is memory-bandwidth bound,
    // so the pass count is the cost.
    let scaled = if wants_half(width, height, downscale) {
        Scaled {
            data: repack_bgra_half(mapping.as_slice(), width, height, pitch, ten_bit),
            width: width / 2,
            height: height / 2,
            stride: (width / 2) * 4,
            source: Some((width, height)),
        }
    } else {
        Scaled {
            data: repack_to_bgra(mapping.as_slice(), width, height, pitch, ten_bit),
            width,
            height,
            stride: width * 4,
            source: None,
        }
    };

    Ok(Some(Frame {
        width: scaled.width,
        height: scaled.height,
        stride: scaled.stride,
        pixel_format: PixelFormat::Bgra,
        data: scaled.data,
        monotonic_us: now_us(),
        monitor: 0,
        // DRM reports nothing about which pixels changed, and neither buffer
        // identity nor a page flip is a proxy for it. See the module docs.
        damage: Damage::Unknown,
        // Pre-downscale dims, so the pump can still reason about the real
        // screen size; `None` when we delivered native.
        source: scaled.source,
    }))
}

/// A frame's pixels plus the geometry that describes them.
struct Scaled {
    data: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
    /// Native capture dims when a downscale happened, else `None` — the
    /// `Frame::source` contract.
    source: Option<(u32, u32)>,
}

/// Mirror the scrap backend's policy — sharing `DOWNSCALE_TRIGGER_PIXELS`
/// rather than restating the threshold — so a host does not get a different
/// picture size purely because it switched capture backend.
fn wants_half(width: u32, height: u32, policy: DownscalePolicy) -> bool {
    if width < 2 || height < 2 {
        return false;
    }
    match policy {
        DownscalePolicy::Always => true,
        DownscalePolicy::Never => false,
        DownscalePolicy::Auto => (width as u64 * height as u64) >= super::DOWNSCALE_TRIGGER_PIXELS,
    }
}

/// Repack **and** 2×2 box-downsample in a single pass over the scanout buffer.
///
/// Same filter as `capture::downscale_bgra_2x` (average four samples per
/// channel, `+2/4` round) and therefore the same picture — but it never
/// materialises the full-size frame, which is the whole point: this backend is
/// memory-bandwidth bound, so the win is one fewer read and one 4×-smaller
/// write, not a cheaper inner loop.
fn repack_bgra_half(src: &[u8], width: u32, height: u32, pitch: u32, ten_bit: bool) -> Vec<u8> {
    let (dw, dh) = ((width / 2) as usize, (height / 2) as usize);
    let pitch = pitch as usize;
    let mut out = vec![0u8; dw * dh * 4];
    // ⚠️ Copy each source row pair into cached scratch FIRST, then filter from
    // there. Reading the two rows directly would alternate between addresses a
    // whole pitch apart on every output pixel, and the scanout mapping punishes
    // strided access brutally: the naive version measured 329 ms/frame at 4K
    // against 53 ms for read-everything-then-filter. Sequential reads out of
    // this mapping are the constraint — not the number of passes over it.
    let row_bytes = (width as usize) * 4;
    let mut scratch = vec![0u8; row_bytes * 2];
    for y in 0..dh {
        scratch[..row_bytes].copy_from_slice(&src[2 * y * pitch..2 * y * pitch + row_bytes]);
        scratch[row_bytes..]
            .copy_from_slice(&src[(2 * y + 1) * pitch..(2 * y + 1) * pitch + row_bytes]);
        let (r0, r1) = (0usize, row_bytes);
        let src = &scratch[..];
        let dst = &mut out[y * dw * 4..(y + 1) * dw * 4];
        for x in 0..dw {
            let o = 2 * x * 4;
            // Average in the SOURCE's own precision, then narrow — narrowing
            // first would throw away the two bits that make averaging 10-bit
            // samples worth doing at all.
            let (mut b, mut g, mut r) = (0u32, 0u32, 0u32);
            for base in [r0 + o, r0 + o + 4, r1 + o, r1 + o + 4] {
                if ten_bit {
                    let v = u32::from_le_bytes([
                        src[base],
                        src[base + 1],
                        src[base + 2],
                        src[base + 3],
                    ]);
                    b += v & 0x3FF;
                    g += (v >> 10) & 0x3FF;
                    r += (v >> 20) & 0x3FF;
                } else {
                    b += u32::from(src[base]);
                    g += u32::from(src[base + 1]);
                    r += u32::from(src[base + 2]);
                }
            }
            let narrow = |sum: u32| -> u8 {
                if ten_bit {
                    // 4 samples of 10 bits → /4 back to 10, then >>2 to 8.
                    (((sum + 2) / 4) >> 2) as u8
                } else {
                    ((sum + 2) / 4) as u8
                }
            };
            dst[x * 4] = narrow(b);
            dst[x * 4 + 1] = narrow(g);
            dst[x * 4 + 2] = narrow(r);
            dst[x * 4 + 3] = 0xFF;
        }
    }
    out
}

fn now_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[async_trait::async_trait]
impl ScreenCapture for DrmCapture {
    async fn next_frame(&mut self) -> Result<Option<Frame>> {
        if let Some(last) = self.last_frame_at {
            let elapsed = last.elapsed();
            if elapsed < self.target_frame_period {
                tokio::time::sleep(self.target_frame_period - elapsed).await;
            }
        }
        let (res_tx, res_rx) = oneshot::channel();
        self.cmd_tx
            .send(res_tx)
            .map_err(|_| anyhow!("drm capture worker exited"))?;
        let reply = res_rx
            .await
            .map_err(|_| anyhow!("drm capture worker dropped reply"))?;
        self.last_frame_at = Some(Instant::now());
        match reply {
            WorkerReply::Frame(f) => {
                if let Some(fr) = f.as_ref() {
                    self.width = fr.width;
                    self.height = fr.height;
                }
                Ok(*f)
            }
            WorkerReply::Failed(e) => Err(anyhow!(e)),
        }
    }

    fn monitor_count(&self) -> u8 {
        // P1 captures the primary CRTC only; per-CRTC capture with origin and
        // scale mapping is deferred by the spec. Reporting the true count here
        // would advertise monitors this backend cannot deliver.
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 10-bit path is the one that fails SILENTLY — an 8-bit decode of
    /// XR30 keeps every shape and ruins every colour, which reads as a
    /// colour-space bug. Pin the channel extraction against hand-computed
    /// values so a future edit to the shifts fails loudly instead.
    #[test]
    fn repack_unpacks_xr30_channels() {
        // x=0b11, R=0x3FF (255), G=0x200 (128), B=0x000 (0)
        let word: u32 = (0b11 << 30) | (0x3FF << 20) | (0x200 << 10);
        let src = word.to_le_bytes().to_vec();
        let out = repack_to_bgra(&src, 1, 1, 4, true);
        assert_eq!(out[0], 0, "B");
        assert_eq!(out[1], 128, "G");
        assert_eq!(out[2], 255, "R");
        assert_eq!(out[3], 255, "alpha must be forced opaque");
    }

    /// XR24 is already BGRA in memory, so the repack is a copy — except for
    /// alpha, which is UNDEFINED in the X-variant and must be forced opaque or
    /// the frame can render transparent downstream.
    #[test]
    fn repack_passes_xr24_through_and_forces_alpha() {
        let src = vec![10u8, 20, 30, 0x00];
        let out = repack_to_bgra(&src, 1, 1, 4, false);
        assert_eq!(&out[..3], &[10, 20, 30]);
        assert_eq!(out[3], 0xFF);
    }

    /// Scanout pitch is nearly always wider than width×4 (4096-wide 10-bit
    /// scanout measured a 16384-byte pitch against a 4096×4 = 16384 exact fit,
    /// but 1920×1080 measured 7680 = exact). Padding must be skipped, or every
    /// row after the first is offset and the image shears.
    #[test]
    fn repack_honours_a_padded_pitch() {
        // 2×2 image, pitch 12 bytes (2 px × 4 + 4 bytes of padding).
        let mut src = vec![0u8; 12 * 2];
        src[0] = 1; // row 0, px 0, B
        src[12] = 2; // row 1, px 0, B  — only correct if pitch is honoured
        let out = repack_to_bgra(&src, 2, 2, 12, false);
        assert_eq!(out[0], 1);
        assert_eq!(
            out[2 * 4],
            2,
            "row 1 must start at src[pitch], not src[w*4]"
        );
    }

    /// The fused path exists to save memory passes, NOT to change the picture.
    /// Pin it against the two-step route it replaced: repack to full-size BGRA,
    /// then run the shared 2×2 box filter, and require identical bytes.
    ///
    /// ⚠️ This is the test that would catch a fusion that quietly shifted the
    /// sample grid by a pixel — which looks fine in a screenshot and is wrong.
    #[cfg(feature = "scrap-capture")]
    #[test]
    fn fused_half_matches_repack_then_downscale_for_xr24() {
        // 4×4 XR24 with a padded pitch, values chosen so every 2×2 block
        // averages to something non-uniform.
        let (w, h, pitch) = (4u32, 4u32, 20u32);
        let mut src = vec![0u8; (pitch * h) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let o = y * pitch as usize + x * 4;
                src[o] = (x * 17 + y * 3) as u8;
                src[o + 1] = (x * 5 + y * 29) as u8;
                src[o + 2] = (x * 11 + y * 7) as u8;
                src[o + 3] = 0x00;
            }
        }
        let full = repack_to_bgra(&src, w, h, pitch, false);
        let (two_step, tw, th) = super::super::downscale_bgra_2x(&full, w, h, w * 4);
        let fused = repack_bgra_half(&src, w, h, pitch, false);
        assert_eq!((tw, th), (w / 2, h / 2));
        assert_eq!(fused, two_step, "fusing must not change the picture");
    }

    /// 10-bit has no two-step equivalent to compare against (the shared filter
    /// only knows BGRA), so pin the arithmetic itself: averaging must happen in
    /// the SOURCE's precision and narrow afterwards. Narrowing first would
    /// discard exactly the bits that make averaging 10-bit samples useful.
    #[test]
    fn fused_half_averages_xr30_in_source_precision() {
        // Four pixels with 10-bit R = 4, 8, 12, 16 → mean 10 → 8-bit 10>>2 = 2.
        let (w, h, pitch) = (2u32, 2u32, 8u32);
        let mut src = vec![0u8; (pitch * h) as usize];
        for (i, r) in [4u32, 8, 12, 16].iter().enumerate() {
            let (x, y) = (i % 2, i / 2);
            let word = r << 20;
            let o = y * pitch as usize + x * 4;
            src[o..o + 4].copy_from_slice(&word.to_le_bytes());
        }
        let out = repack_bgra_half(&src, w, h, pitch, true);
        assert_eq!(out.len(), 4, "2x2 → 1 pixel");
        assert_eq!(out[2], 2, "R: mean of 4,8,12,16 in 10-bit is 10 → 8-bit 2");
        assert_eq!(out[3], 0xFF);
    }

    /// The policy has to match scrap's, or a host gets a different picture size
    /// purely from switching backend.
    #[test]
    fn wants_half_follows_the_shared_threshold() {
        assert!(!wants_half(1920, 1080, DownscalePolicy::Never));
        assert!(wants_half(1920, 1080, DownscalePolicy::Always));
        // 1080p is under the trigger; 4K is over it.
        assert!(!wants_half(1920, 1080, DownscalePolicy::Auto));
        assert!(wants_half(4096, 2160, DownscalePolicy::Auto));
        // Degenerate sizes must never halve into nothing.
        assert!(!wants_half(1, 1, DownscalePolicy::Always));
    }

    /// A framebuffer with no reported modifier is a pre-modifier one, which is
    /// linear by definition — treating `None` as non-linear would refuse to
    /// capture on older drivers that work perfectly.
    #[test]
    fn env_gate_defaults_off() {
        // Default OFF is load-bearing: defaulting ON would silently undo
        // FR-29's idle-CPU win on every X11 host, because this backend has no
        // damage information and would grab every frame forever.
        assert!(!tunnel_core::env::flag(
            "DRM_CAPTURE_DEFINITELY_UNSET_XYZ",
            false
        ));
    }
}
