//! Cross-platform screen capture backed by the `scrap` crate.
//!
//! `scrap` is a thin wrapper that picks the right kernel primitive per OS:
//!   - Linux  → XShm (X11 shared-memory pixmap)
//!   - Windows → DXGI Desktop Duplication
//!   - macOS  → CoreGraphics `CGDisplayStream` fallback
//!
//! `scrap::Capturer` is `!Send` (XShm handles have thread affinity), so we
//! pin it to a dedicated OS thread and drive it via oneshot commands: the
//! async `next_frame` sends a oneshot sender, the worker captures, fills
//! the oneshot. That keeps the async runtime free while respecting the
//! underlying thread-affinity requirement.
//!
//! BGRA is always emitted (scrap's native format); the encoder layer is
//! responsible for any colour conversion.

use anyhow::{Context, Result, anyhow};
use scrap::{Capturer, Display};
use std::io::ErrorKind::WouldBlock;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

use super::{DownscalePolicy, Frame, PixelFormat, ScreenCapture};

pub const DEFAULT_TARGET_FPS: u32 = 30;

type CaptureReply = Result<Option<Frame>>;
type CaptureCmd = oneshot::Sender<CaptureReply>;

pub struct ScrapCapture {
    cmd_tx: std_mpsc::Sender<CaptureCmd>,
    width: u32,
    height: u32,
    monitor: u8,
    target_frame_period: Duration,
    last_frame_at: Option<Instant>,
}

impl ScrapCapture {
    pub fn primary(target_fps: u32, downscale: DownscalePolicy) -> Result<Self> {
        // Build the Capturer on the worker thread so it never crosses
        // thread boundaries; use a ready-ack channel to surface any
        // init failure back to the caller synchronously.
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(u32, u32)>>();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<CaptureCmd>();

        thread::Builder::new()
            .name("roomler-agent-capture".into())
            .spawn(move || {
                let init = || -> Result<(Capturer, u32, u32)> {
                    let display = Display::primary().context("no primary display")?;
                    let w = display.width() as u32;
                    let h = display.height() as u32;
                    let cap = Capturer::new(display).context("creating scrap::Capturer")?;
                    Ok((cap, w, h))
                };
                let (mut cap, w, h) = match init() {
                    Ok(v) => {
                        let _ = ready_tx.send(Ok((v.1, v.2)));
                        v
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let start = Instant::now();

                // Wait for capture requests.
                while let Ok(res_tx) = cmd_rx.recv() {
                    let reply = capture_one_blocking(&mut cap, w, h, start, downscale);
                    let _ = res_tx.send(reply);
                }
            })
            .context("spawning capture thread")?;

        let (width, height) = ready_rx
            .recv()
            .context("capture thread never responded")??;

        Ok(Self {
            cmd_tx,
            width,
            height,
            monitor: 0,
            target_frame_period: Duration::from_millis(1000 / target_fps.max(1) as u64),
            last_frame_at: None,
        })
    }

    pub fn width(&self) -> u32 { self.width }
    pub fn height(&self) -> u32 { self.height }
}

/// Downsample 2× when the source has more pixels than this threshold.
/// Software openh264 at 4K SW encode caps out around 6–12 fps on a
/// typical desktop CPU; halving each dimension cuts pixel work by 4×
/// and typically brings us back to 25–30 fps, which matters far more
/// for perceived smoothness than the extra detail.
///
/// Measured in pixels (not width) so QHD 2560×1440 panels (3.7 Mpx)
/// trigger the downscale as well — an earlier width-only threshold
/// missed them because QHD width=2560 fell under the 2561 cutoff.
const DOWNSCALE_TRIGGER_PIXELS: u64 = 3_500_000;

fn capture_one_blocking(
    cap: &mut Capturer,
    width: u32,
    height: u32,
    start: Instant,
    downscale: DownscalePolicy,
) -> CaptureReply {
    // Give the compositor a budget — if nothing is ready within ~100 ms we
    // return None and let the async side decide whether to retry.
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        match cap.frame() {
            Ok(buf) => {
                let stride = (buf.len() as u32) / height.max(1);
                let monotonic_us = start.elapsed().as_micros() as u64;
                let pixel_count = u64::from(width) * u64::from(height);
                let should_downscale = match downscale {
                    DownscalePolicy::Never => false,
                    DownscalePolicy::Always => width >= 2 && height >= 2,
                    DownscalePolicy::Auto => {
                        pixel_count >= DOWNSCALE_TRIGGER_PIXELS && width >= 2 && height >= 2
                    }
                };
                let (data, out_w, out_h, out_stride) = if should_downscale {
                    let (dst, dw, dh) = downscale_bgra_2x(&buf, width, height, stride);
                    (dst, dw, dh, dw * 4)
                } else {
                    (buf.to_vec(), width, height, stride)
                };
                return Ok(Some(Frame {
                    width: out_w,
                    height: out_h,
                    stride: out_stride,
                    pixel_format: PixelFormat::Bgra,
                    data,
                    monotonic_us,
                    monitor: 0,
                    // scrap doesn't expose a dirty-rect API on any
                    // platform; encoder treats empty as "full-frame
                    // dirty" / no ROI hints. WGC backend (1C.1) will
                    // populate this from Direct3D11CaptureFrame::
                    // DirtyRegion() once it lands.
                    dirty_rects: Vec::new(),
                }));
            }
            Err(e) if e.kind() == WouldBlock => {
                if Instant::now() > deadline {
                    return Ok(None);
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(e) => return Err(anyhow!("scrap frame error: {e}")),
        }
    }
}

/// 2×2 box downsample over BGRA. Output dimensions are floor(w/2), floor(h/2).
/// Averages each 2×2 block per channel with a +2/4 round. Naive scalar
/// loop — at 4K (8.3 Mpx in, 2.1 Mpx out) this runs in ~15 ms in release
/// mode on a desktop CPU, well under the ~30 ms budget per frame at 30 fps
/// and comfortably less than openh264 would have spent encoding the full
/// 4K frame it replaces.
fn downscale_bgra_2x(src: &[u8], src_w: u32, src_h: u32, src_stride: u32) -> (Vec<u8>, u32, u32) {
    let dw = src_w / 2;
    let dh = src_h / 2;
    let sw = src_stride as usize;
    let mut dst = vec![0u8; (dw * dh * 4) as usize];
    for y in 0..dh as usize {
        let row0 = 2 * y * sw;
        let row1 = (2 * y + 1) * sw;
        for x in 0..dw as usize {
            let sx = 2 * x * 4;
            let dx = (y * dw as usize + x) * 4;
            for c in 0..4 {
                let p00 = src[row0 + sx + c] as u32;
                let p10 = src[row0 + sx + 4 + c] as u32;
                let p01 = src[row1 + sx + c] as u32;
                let p11 = src[row1 + sx + 4 + c] as u32;
                dst[dx + c] = ((p00 + p10 + p01 + p11 + 2) / 4) as u8;
            }
        }
    }
    (dst, dw, dh)
}

#[async_trait::async_trait]
impl ScreenCapture for ScrapCapture {
    async fn next_frame(&mut self) -> Result<Option<Frame>> {
        // FPS gate.
        if let Some(last) = self.last_frame_at {
            let elapsed = last.elapsed();
            if elapsed < self.target_frame_period {
                tokio::time::sleep(self.target_frame_period - elapsed).await;
            }
        }

        let (res_tx, res_rx) = oneshot::channel();
        self.cmd_tx
            .send(res_tx)
            .map_err(|_| anyhow!("capture worker exited"))?;
        let reply = res_rx.await.map_err(|_| anyhow!("capture worker dropped reply"))?;
        self.last_frame_at = Some(Instant::now());
        let _ = self.monitor; // (exercised below by `monitor_count`)
        reply
    }

    fn monitor_count(&self) -> u8 {
        Display::all()
            .map(|v| v.len().min(u8::MAX as usize) as u8)
            .unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a headless host there may be no $DISPLAY / X server, so we accept
    /// either a successful capture or a clean construction failure. We only
    /// fail the test if construction *succeeds* but the captured frame
    /// looks wrong.
    #[tokio::test]
    async fn captures_one_frame_if_display_is_available() {
        let Ok(mut cap) = ScrapCapture::primary(30, DownscalePolicy::Auto) else {
            eprintln!("no display available — skipping");
            return;
        };
        assert!(cap.width() > 0);
        assert!(cap.height() > 0);
        assert!(cap.monitor_count() >= 1);

        // Budget a few attempts because the compositor needs to paint once.
        let mut got_frame = None;
        for _ in 0..20 {
            if let Some(f) = cap.next_frame().await.unwrap() {
                got_frame = Some(f);
                break;
            }
        }
        let Some(frame) = got_frame else {
            eprintln!("no frame within budget — compositor may be idle, skipping assertions");
            return;
        };
        // `cap.width()` is the source dim; Frame.width is the output
        // dim, which is floor(source/2) when DownscalePolicy::Auto
        // kicks in on displays ≥ 3.5 Mpx (QHD / 4K). Accept either.
        let src_w = cap.width();
        let src_h = cap.height();
        let down = (u64::from(src_w) * u64::from(src_h)) >= 3_500_000;
        let expected_w = if down { src_w / 2 } else { src_w };
        let expected_h = if down { src_h / 2 } else { src_h };
        assert_eq!(frame.width, expected_w);
        assert_eq!(frame.height, expected_h);
        assert_eq!(frame.pixel_format, PixelFormat::Bgra);
        assert!(
            frame.data.len() >= (frame.width * frame.height * 3) as usize,
            "unexpectedly small capture buffer"
        );
        assert!(frame.stride >= frame.width * 4);
    }
}
