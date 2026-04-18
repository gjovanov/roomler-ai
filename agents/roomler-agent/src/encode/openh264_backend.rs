//! Software H.264 encoder built on [OpenH264].
//!
//! Pinned to a dedicated OS thread the same way the capture backend is —
//! the openh264 encoder isn't `Send`-safe to bounce between tokio workers,
//! and the encode call is CPU-bound so it belongs off the async runtime
//! anyway. The async front end hands frames in via a bounded mpsc and
//! reads encoded packets back via a second mpsc.
//!
//! BGRA → I420 conversion is done inline with a simple BT.601 matrix.
//! That's good enough for a v1 software path; a follow-up can plug in
//! libyuv / an NV12-producing capture path to skip the conversion cost.
//!
//! [OpenH264]: https://www.openh264.org/

use anyhow::{Context, Result, anyhow};
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod};
use openh264::formats::YUVBuffer;
use std::sync::mpsc as std_mpsc;
use std::thread;
use tokio::sync::oneshot;

use super::{EncodedPacket, VideoEncoder};
use crate::capture::{Frame, PixelFormat};

use super::initial_bitrate_for;

const TARGET_FPS: u32 = 30;

pub struct Openh264Encoder {
    /// Worker thread; commands go in, packets come out.
    cmd_tx: std_mpsc::Sender<Cmd>,
}

enum Cmd {
    Encode {
        frame: std::sync::Arc<Frame>,
        reply: oneshot::Sender<Result<Vec<EncodedPacket>>>,
    },
    RequestKeyframe,
    SetBitrate(u32),
    Shutdown,
}

impl Openh264Encoder {
    /// Build an encoder sized for the given frame dimensions. OpenH264 needs
    /// the width/height at construction time; if the capture source resizes
    /// (dock/undock) the caller must rebuild this encoder.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<Cmd>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();

        thread::Builder::new()
            .name("roomler-agent-encoder".into())
            .spawn(move || {
                let bitrate_bps = initial_bitrate_for(width, height);
                let init = || -> Result<Encoder> {
                    let api = openh264::OpenH264API::from_source();
                    let cfg = EncoderConfig::new()
                        .bitrate(BitRate::from_bps(bitrate_bps))
                        .max_frame_rate(FrameRate::from_hz(TARGET_FPS as f32))
                        // Force an IDR every 60 frames (≈2 s @ 30 fps).
                        // Without a bounded IDR interval, openh264 can go
                        // 300+ frames between keyframes on a static
                        // desktop; a single lost packet then freezes the
                        // decoder for ~10 s. 60 gives <2 s recovery floor
                        // even if the RTCP-PLI round-trip drops.
                        .intra_frame_period(IntraFramePeriod::from_num_frames(60));
                    Encoder::with_api_config(api, cfg).map_err(|e| anyhow!("encoder init: {e}"))
                };
                tracing::info!(bitrate_bps, width, height, "openh264 encoder init");

                let mut enc = match init() {
                    Ok(e) => {
                        let _ = ready_tx.send(Ok(()));
                        e
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                run_worker(&mut enc, width, height, cmd_rx);
            })
            .context("spawning encoder thread")?;

        ready_rx.recv().context("encoder thread never signalled")??;
        Ok(Self { cmd_tx })
    }
}

impl Drop for Openh264Encoder {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
    }
}

fn run_worker(
    enc: &mut Encoder,
    width: u32,
    height: u32,
    cmd_rx: std_mpsc::Receiver<Cmd>,
) {
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Cmd::Encode { frame, reply } => {
                let result = encode_one(enc, width, height, frame);
                let _ = reply.send(result);
            }
            Cmd::RequestKeyframe => {
                enc.force_intra_frame();
            }
            Cmd::SetBitrate(bps) => {
                // Best-effort — the openh264 crate exposes a getter but
                // mid-stream bitrate control depends on crate version.
                // Logged for now; a follow-up can plumb this when we
                // add TWCC handling.
                tracing::debug!(bps, "openh264 set_bitrate (ignored in this version)");
            }
            Cmd::Shutdown => break,
        }
    }
}

fn encode_one(
    enc: &mut Encoder,
    width: u32,
    height: u32,
    frame: std::sync::Arc<Frame>,
) -> Result<Vec<EncodedPacket>> {
    if frame.pixel_format != PixelFormat::Bgra {
        return Err(anyhow!(
            "openh264 backend needs BGRA input; got {:?}",
            frame.pixel_format
        ));
    }
    if frame.width != width || frame.height != height {
        return Err(anyhow!(
            "frame size mismatch: encoder configured for {}x{}, got {}x{}",
            width,
            height,
            frame.width,
            frame.height
        ));
    }

    let yuv = bgra_to_yuv_buffer(&frame.data, frame.width as usize, frame.height as usize, frame.stride as usize);
    let bitstream = enc.encode(&yuv).map_err(|e| anyhow!("encode: {e}"))?;

    // Walk the layered bitstream and return one packet per NAL-unit
    // grouped by layer. For our SW path with a single layer there's
    // effectively one packet, but keeping the structure faithful means
    // the SFU / track feeder can treat multi-layer output the same way.
    let mut packets = Vec::new();
    let is_keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
    for layer_idx in 0..bitstream.num_layers() {
        if let Some(layer) = bitstream.layer(layer_idx) {
            let mut data = Vec::new();
            for nal_idx in 0..layer.nal_count() {
                if let Some(nal) = layer.nal_unit(nal_idx) {
                    data.extend_from_slice(nal);
                }
            }
            if !data.is_empty() {
                packets.push(EncodedPacket {
                    data,
                    is_keyframe,
                    duration_us: 0,
                });
            }
        }
    }
    Ok(packets)
}

#[async_trait::async_trait]
impl VideoEncoder for Openh264Encoder {
    async fn encode(&mut self, frame: std::sync::Arc<Frame>) -> Result<Vec<EncodedPacket>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Encode { frame, reply: reply_tx })
            .map_err(|_| anyhow!("encoder worker gone"))?;
        reply_rx.await.map_err(|_| anyhow!("encoder reply dropped"))?
    }

    fn request_keyframe(&mut self) {
        let _ = self.cmd_tx.send(Cmd::RequestKeyframe);
    }

    fn set_bitrate(&mut self, bps: u32) {
        let _ = self.cmd_tx.send(Cmd::SetBitrate(bps));
    }

    fn name(&self) -> &'static str {
        "openh264"
    }
}

/// Convert a BGRA buffer (what scrap produces on Linux/Windows/macOS) into
/// an I420 [`YUVBuffer`] suitable for OpenH264. Uses BT.601 studio swing
/// because that's the WebRTC convention for sub-HD content; BT.709 would
/// be a valid choice for ≥720p and can be swapped in later.
///
/// Output layout: a single packed vector `[Y... U... V...]` passed to
/// `YUVBuffer::from_vec`, with plane sizes `w*h`, `(w/2)*(h/2)`,
/// `(w/2)*(h/2)` respectively. Width and height must be even.
fn bgra_to_yuv_buffer(bgra: &[u8], width: usize, height: usize, stride: usize) -> YUVBuffer {
    // OpenH264 needs even dimensions; callers should size the encoder
    // accordingly. We round down here defensively.
    let w = width & !1;
    let h = height & !1;

    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);
    let mut packed = vec![0u8; y_size + 2 * uv_size];

    // Y plane.
    for row in 0..h {
        let src_row = &bgra[row * stride..row * stride + w * 4];
        let dst_row = &mut packed[row * w..(row + 1) * w];
        for (col, px) in src_row.chunks_exact(4).enumerate() {
            let b = px[0] as i32;
            let g = px[1] as i32;
            let r = px[2] as i32;
            let y_val = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            dst_row[col] = y_val.clamp(0, 255) as u8;
        }
    }

    // U / V planes: 2x2 chroma subsample. Average the 2x2 block first then
    // apply the coefficient matrix — cheap and keeps visual quality OK.
    let u_base = y_size;
    let v_base = y_size + uv_size;
    let cw = w / 2;
    for row in (0..h).step_by(2) {
        for col in (0..w).step_by(2) {
            let i0 = row * stride + col * 4;
            let i1 = row * stride + (col + 1) * 4;
            let i2 = (row + 1) * stride + col * 4;
            let i3 = (row + 1) * stride + (col + 1) * 4;

            let b_avg = ((bgra[i0] as i32 + bgra[i1] as i32 + bgra[i2] as i32 + bgra[i3] as i32) + 2) >> 2;
            let g_avg = ((bgra[i0 + 1] as i32 + bgra[i1 + 1] as i32 + bgra[i2 + 1] as i32 + bgra[i3 + 1] as i32) + 2) >> 2;
            let r_avg = ((bgra[i0 + 2] as i32 + bgra[i1 + 2] as i32 + bgra[i2 + 2] as i32 + bgra[i3 + 2] as i32) + 2) >> 2;

            let u_val = ((-38 * r_avg - 74 * g_avg + 112 * b_avg + 128) >> 8) + 128;
            let v_val = ((112 * r_avg - 94 * g_avg - 18 * b_avg + 128) >> 8) + 128;

            let chroma_idx = (row / 2) * cw + (col / 2);
            packed[u_base + chroma_idx] = u_val.clamp(0, 255) as u8;
            packed[v_base + chroma_idx] = v_val.clamp(0, 255) as u8;
        }
    }

    YUVBuffer::from_vec(packed, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bgra_frame(width: u32, height: u32, r: u8, g: u8, b: u8) -> Frame {
        let stride = width * 4;
        let mut data = vec![0u8; (stride * height) as usize];
        for px in data.chunks_exact_mut(4) {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
        Frame {
            width,
            height,
            stride,
            pixel_format: PixelFormat::Bgra,
            data,
            monotonic_us: 0,
            monitor: 0,
        }
    }

    #[tokio::test]
    async fn encodes_first_frame_as_keyframe() {
        let mut enc = Openh264Encoder::new(320, 240).expect("encoder");
        let frame = std::sync::Arc::new(make_bgra_frame(320, 240, 100, 150, 200));
        let packets = enc.encode(frame).await.expect("encode");
        assert!(!packets.is_empty(), "no packets produced");
        assert!(packets.iter().any(|p| p.is_keyframe));
        let total: usize = packets.iter().map(|p| p.data.len()).sum();
        assert!(total > 10, "unexpectedly small bitstream: {total} bytes");
    }

    #[tokio::test]
    async fn rejects_wrong_pixel_format() {
        let mut enc = Openh264Encoder::new(64, 64).expect("encoder");
        let mut frame = make_bgra_frame(64, 64, 0, 0, 0);
        frame.pixel_format = PixelFormat::Nv12;
        assert!(enc.encode(std::sync::Arc::new(frame)).await.is_err());
    }

    #[tokio::test]
    async fn rejects_size_mismatch() {
        let mut enc = Openh264Encoder::new(320, 240).expect("encoder");
        let frame = std::sync::Arc::new(make_bgra_frame(640, 480, 0, 0, 0));
        assert!(enc.encode(frame).await.is_err());
    }

    #[tokio::test]
    async fn request_keyframe_is_noisy_next_frame() {
        let mut enc = Openh264Encoder::new(64, 64).expect("encoder");
        let _ = enc.encode(std::sync::Arc::new(make_bgra_frame(64, 64, 0, 0, 0))).await.unwrap();
        enc.request_keyframe();
        let out = enc.encode(std::sync::Arc::new(make_bgra_frame(64, 64, 255, 255, 255))).await.unwrap();
        assert!(!out.is_empty());
    }
}
