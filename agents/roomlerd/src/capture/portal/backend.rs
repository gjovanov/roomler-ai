// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-45 P3c-ii — the sixth `ScreenCapture` backend.
//!
//! The daemon spawns [`super::helper`] in streaming mode, reads the handshake
//! line, then reads framed pixels off the child's stdout for as long as the
//! capture lasts.
//!
//! ⚠️ **stdout is binary after the handshake; diagnostics go to stderr.** The
//! same rule the SSH subsystem documents for `sftp-server` — a stray `println!`
//! in the child does not produce a stray log line here, it corrupts a frame and
//! desynchronises the stream. The wire format's magic exists so that failure is
//! loud at the next boundary rather than silent.

use anyhow::{Result, anyhow};

fn now_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

use super::wire::{FrameHeader, HEADER_LEN};
use crate::capture::{Damage, Frame, PixelFormat, ScreenCapture};

/// A portal capture session: a child in the user's session, and its frames.
pub struct PortalCapture {
    child: std::process::Child,
    /// Depth 1 and lossy: the pump wants the newest frame, and a queue of
    /// stale ones is latency with extra steps.
    rx: std::sync::mpsc::Receiver<Frame>,
    width: u32,
    height: u32,
}

impl PortalCapture {
    /// Spawn the helper and wait for its first frame.
    ///
    /// Returns an error rather than an empty capturer when anything fails, so
    /// `open_default` can fall through to the rest of the cascade with a reason
    /// in the log.
    pub fn open(target_fps: u32) -> Result<Self> {
        let mut child = super::helper::spawn_streaming(target_fps)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("the portal helper has no stdout"))?;
        let mut reader = std::io::BufReader::new(stdout);

        // 1. The handshake line, so a failure is reported by the child rather
        //    than showing up as a stream that never starts.
        let started = super::helper::read_stream_handshake(&mut reader)?;
        let (width, height) = (started.width, started.height);

        // 2. Frames, on their own thread. Bounded at 1: dropping a stale frame
        //    is the correct behaviour for a live capture.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            loop {
                match read_frame(&mut reader) {
                    Ok(Some(f)) => {
                        // Full channel means the pump is behind; drop rather
                        // than block, or the helper's pipe backs up and the
                        // compositor's producer stalls behind it.
                        let _ = tx.try_send(f);
                    }
                    Ok(None) => break, // clean EOF — the helper exited
                    Err(e) => {
                        tracing::warn!(error = %e, "portal capture: frame stream ended");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            child,
            rx,
            width,
            height,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
}

impl Drop for PortalCapture {
    /// ⚠️ Kill the helper explicitly. It holds a live portal session and a
    /// PipeWire stream; leaving it running would keep the compositor
    /// screencasting — and on GNOME that means the "screen is being shared"
    /// indicator stays up after the session ends, which is both a privacy
    /// surprise and a support call.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read one framed frame. `Ok(None)` is a clean end of stream.
fn read_frame(r: &mut impl std::io::Read) -> Result<Option<Frame>> {
    let mut head = [0u8; HEADER_LEN];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let h = FrameHeader::decode(&head).map_err(|e| anyhow!("{e}"))?;
    let mut data = vec![0u8; h.len as usize];
    r.read_exact(&mut data)?;

    Ok(Some(Frame {
        width: h.width,
        height: h.height,
        stride: h.stride,
        pixel_format: pixel_format_of(h.video_format)?,
        data,
        monotonic_us: now_us(),
        monitor: 0,
        // ⚠️ The portal reports no damage, so every frame is "assume
        // everything changed". `Unknown` is the pre-FR-29 contract and the
        // honest answer — claiming `Tracked(&[])` would tell the pump nothing
        // changed and freeze the picture.
        damage: Damage::Unknown,
        // Never scaled here: the compositor delivers what it negotiated.
        source: None,
    }))
}

/// Map `enum spa_video_format` onto what the encode path understands.
///
/// ⚠️ Refuses anything else rather than guessing. Our `EnumFormat` only offers
/// these four, so a fifth means the compositor ignored the offer — and
/// mislabelling a pixel order produces a picture with swapped colour channels,
/// which reads as a codec bug.
fn pixel_format_of(video_format: u32) -> Result<PixelFormat> {
    use super::pod::ty;
    match video_format {
        // BGRx and BGRA are the same byte order to a consumer that ignores
        // alpha, which is what the encode path does.
        ty::VIDEO_FORMAT_BGRX | ty::VIDEO_FORMAT_BGRA => Ok(PixelFormat::Bgra),
        other => Err(anyhow!(
            "the compositor chose spa_video_format {other}, which was not in our offer"
        )),
    }
}

#[async_trait::async_trait]
impl ScreenCapture for PortalCapture {
    async fn next_frame(&mut self) -> Result<Option<Frame>> {
        // The helper paces to the negotiated rate, so this waits on it rather
        // than sleeping on a timer of its own — two pacers would beat against
        // each other.
        let rx = &self.rx;
        match tokio::task::block_in_place(|| rx.recv_timeout(FRAME_WAIT)) {
            Ok(f) => Ok(Some(f)),
            // No frame inside the window is not an error: a still screen
            // legitimately produces nothing, and `Ok(None)` is the pump's
            // "nothing this tick".
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow!("the portal helper stopped producing frames"))
            }
        }
    }

    fn monitor_count(&self) -> u8 {
        // The portal hands us exactly the one source the user picked.
        1
    }
}

/// How long a single `next_frame` waits before reporting "nothing this tick".
/// Long enough not to spin on a still screen, short enough that the pump stays
/// responsive to a resize or a teardown.
const FRAME_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn framed(h: FrameHeader, fill: u8) -> Vec<u8> {
        let mut v = Vec::new();
        v.write_all(&h.encode()).unwrap();
        v.write_all(&vec![fill; h.len as usize]).unwrap();
        v
    }

    fn hdr(len: u32) -> FrameHeader {
        FrameHeader {
            width: 4,
            height: 2,
            stride: 16,
            video_format: 8,
            len,
        }
    }

    /// Two frames back to back must both come out — the reader has to consume
    /// exactly the declared payload or it lands mid-pixels on the next header.
    #[test]
    fn frames_are_read_back_to_back() {
        let mut buf = framed(hdr(32), 0xAB);
        buf.extend(framed(hdr(32), 0xCD));
        let mut cur = std::io::Cursor::new(buf);

        let a = read_frame(&mut cur).unwrap().unwrap();
        assert_eq!((a.width, a.height, a.stride), (4, 2, 16));
        assert!(a.data.iter().all(|b| *b == 0xAB));

        let b = read_frame(&mut cur).unwrap().unwrap();
        assert!(b.data.iter().all(|b| *b == 0xCD));

        assert!(
            read_frame(&mut cur).unwrap().is_none(),
            "clean EOF expected"
        );
    }

    /// A truncated payload is an error, not a short frame handed to the
    /// encoder — half a frame encodes as garbage rather than failing.
    #[test]
    fn a_truncated_payload_is_an_error() {
        let mut buf = framed(hdr(32), 1);
        buf.truncate(HEADER_LEN + 10);
        let mut cur = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cur).is_err());
    }

    /// Only the formats we offered are accepted; anything else would mean
    /// guessing a byte order, and a wrong guess looks like a codec bug.
    #[test]
    fn an_unoffered_pixel_format_is_refused() {
        use super::super::pod::ty;
        assert!(pixel_format_of(ty::VIDEO_FORMAT_BGRX).is_ok());
        assert!(pixel_format_of(ty::VIDEO_FORMAT_BGRA).is_ok());
        let e = pixel_format_of(99).unwrap_err().to_string();
        assert!(e.contains("not in our offer"), "{e}");
    }
}
