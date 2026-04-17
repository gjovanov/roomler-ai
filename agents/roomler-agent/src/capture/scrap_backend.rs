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

use super::{Frame, PixelFormat, ScreenCapture};

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
    pub fn primary(target_fps: u32) -> Result<Self> {
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
                    let reply = capture_one_blocking(&mut cap, w, h, start);
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

fn capture_one_blocking(
    cap: &mut Capturer,
    width: u32,
    height: u32,
    start: Instant,
) -> CaptureReply {
    // Give the compositor a budget — if nothing is ready within ~100 ms we
    // return None and let the async side decide whether to retry.
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        match cap.frame() {
            Ok(buf) => {
                let stride = (buf.len() as u32) / height.max(1);
                let owned: Vec<u8> = buf.to_vec();
                let monotonic_us = start.elapsed().as_micros() as u64;
                return Ok(Some(Frame {
                    width,
                    height,
                    stride,
                    pixel_format: PixelFormat::Bgra,
                    data: owned,
                    monotonic_us,
                    monitor: 0,
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
        let Ok(mut cap) = ScrapCapture::primary(30) else {
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
        assert_eq!(frame.width, cap.width());
        assert_eq!(frame.height, cap.height());
        assert_eq!(frame.pixel_format, PixelFormat::Bgra);
        assert!(
            frame.data.len() >= (frame.width * frame.height * 3) as usize,
            "unexpectedly small capture buffer"
        );
        assert!(frame.stride >= frame.width * 4);
    }
}
