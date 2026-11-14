//! `roomlerd encoder-smoke` (#3652): one cell, opened the way a session opens
//! it, ten solid-colour frames encoded, and the run judged by the FR-16 L1
//! bench's [`bench::verdict`]: what OPENED against what was ASKED.
//!
//! The opener is the bench's [`CellOpener`] seam, so the judgement is tested
//! with a fake opener and no encoder hardware.

use std::fmt;
use std::io::Write;
use std::sync::Arc;

use anyhow::Context as _;

use super::EncoderPreference;
use super::bench::{
    self, Backend, BuildCaps, CellOpener, CellPath, CellPlan, CellSpec, Codec, OpenedId, Reach,
    Verdict,
};
use crate::capture::synthetic_scene::Scene;
use crate::capture::{Damage, Frame, PixelFormat};
use roomler_ai_remote_control::models::VideoBackend;

pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 480;
pub const FRAMES: u32 = 10;
pub const FPS: u32 = 30;

/// What the smoke was asked for, from its flags.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub pref: EncoderPreference,
    /// `--codec`, as typed.
    pub codec: &'a str,
    /// `--name`: one encoder by name, past every cascade.
    pub name: Option<&'a str>,
}

impl Request<'_> {
    /// The flags that asked, for a failure's first line.
    pub fn describe(&self) -> String {
        match self.name {
            Some(name) => format!("--name {name}"),
            None => format!(
                "--encoder {} --codec {}",
                pref_label(self.pref),
                self.codec.to_ascii_lowercase()
            ),
        }
    }
}

fn pref_label(pref: EncoderPreference) -> &'static str {
    match pref {
        EncoderPreference::Auto => "auto",
        EncoderPreference::Hardware => "hardware",
        EncoderPreference::Software => "software",
    }
}

/// A request this build cannot serve, refused before anything opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// `--codec` names no codec the smoke opens.
    Codec(String),
    /// `--encoder software` for a codec with no software encoder.
    NoSoftwareEncoder(String),
    /// `--name` names no FFmpeg or Media Foundation encoder.
    Name(String),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codec(c) => write!(
                f,
                "encoder smoke: --codec {c:?} is not h264, h265 (hevc) or av1 \
                 (VP9 is the DataChannel's: `encoder-smoke --bench --bench-cells dc:vp9:444`)"
            ),
            Self::NoSoftwareEncoder(c) => write!(
                f,
                "encoder smoke: no software {c} encoder exists in any build; \
                 ask for `--encoder hardware` or `--encoder auto`"
            ),
            Self::Name(n) => write!(
                f,
                "encoder smoke: --name {n:?} is not an FFmpeg or Media Foundation encoder name"
            ),
        }
    }
}

impl std::error::Error for Refused {}

/// The cell the request asks for, on this build.
///
/// SEAM (#3652 AC2): master's mapping, byte for byte. It never refuses: an
/// unknown codec opens H.264, as master's `open_for_codec` did.
pub fn plan(
    req: &Request<'_>,
    width: u32,
    height: u32,
    build: BuildCaps,
) -> Result<CellPlan, Refused> {
    let cell = match req.name {
        Some(name) => {
            let codec = VideoBackend::from_ffmpeg_name(name)
                .and_then(|(codec, _)| Codec::parse(codec.wire()))
                .unwrap_or(Codec::H264);
            CellSpec::new(CellPath::Named, codec, false)
        }
        None => {
            let codec = match req.codec.to_ascii_lowercase().as_str() {
                "h265" | "hevc" => Codec::H265,
                "av1" => Codec::Av1,
                _ => Codec::H264,
            };
            CellSpec::new(CellPath::Rtp, codec, false)
        }
    };
    Ok(CellPlan {
        cell,
        name: req.name.map(str::to_string),
        width,
        height,
        fps: FPS,
        target_bps: super::initial_bitrate_for_fps(width, height, FPS),
        motion: Scene::Static,
        requested_hw: bench::requests_hardware(cell.path, req.pref),
        hardware_on_path: build.hardware_on_path(cell),
    })
}

/// What the smoke tried on this OS and build.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tried {
    /// Each backend the request could reach, and whether this build has it.
    pub backends: Vec<(Backend, Reach)>,
    /// Candidates that failed to open: (name, why).
    pub failed: Vec<(String, String)>,
    /// Candidates the cell denylist kept out.
    pub denied: Vec<String>,
}

impl fmt::Display for Tried {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut sep = "";
        for (backend, reach) in &self.backends {
            write!(f, "{sep}{}: {}", backend.label(), reach.label())?;
            sep = "; ";
        }
        for (name, why) in &self.failed {
            write!(f, "{sep}{name} failed to open: {why}")?;
            sep = "; ";
        }
        for name in &self.denied {
            write!(f, "{sep}{name} denied (encoder_cells_deny)")?;
            sep = "; ";
        }
        if sep.is_empty() {
            write!(f, "nothing")?;
        }
        Ok(())
    }
}

/// A passing run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passed {
    pub backend: &'static str,
    pub verdict: Verdict,
    pub keyframes: usize,
    pub total_bytes: usize,
}

/// A failing run: what was asked, what opened, what was tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub verdict: Verdict,
    pub cell: CellSpec,
    pub request: String,
    pub opened: Option<OpenedId>,
    pub tried: Tried,
    /// The reason in words, when the verdict alone does not say it.
    pub why: Option<String>,
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(why) = &self.why {
            return write!(f, "{why}");
        }
        write!(
            f,
            "encoder smoke FAILED: verdict={} cell={}:{} request={}",
            self.verdict.label(),
            self.cell.path.label(),
            self.cell.codec.label(),
            self.request
        )
    }
}

impl std::error::Error for Failed {}

/// Where `--dump` writes the raw bitstream.
pub struct Dump<'a> {
    pub path: &'a str,
    pub file: &'a mut (dyn Write + Send),
}

/// Open the planned cell, encode [`FRAMES`] frames, and judge the run.
///
/// SEAM (#3652 AC2): master's judgement, byte for byte. A pass is anything
/// but the NoopEncoder that produced a keyframe; `verdict` only records
/// whether master would have warned of a demotion.
pub async fn run(
    plan: &CellPlan,
    req: &Request<'_>,
    _build: BuildCaps,
    opener: &mut (dyn CellOpener + Send),
    out: &mut (dyn Write + Send),
    mut dump: Option<Dump<'_>>,
) -> anyhow::Result<Passed> {
    let (w, h) = (plan.width, plan.height);
    let codec = req.codec.to_ascii_lowercase();
    tracing::info!(width = w, height = h, pref = ?req.pref, codec = %codec, "encoder smoke: opening encoder");

    let failed = |verdict: Verdict, why: String| Failed {
        verdict,
        cell: plan.cell,
        request: req.describe(),
        opened: None,
        tried: Tried::default(),
        why: Some(why),
    };
    let opened = match opener.open(plan) {
        Ok(o) => o,
        Err(e) => {
            let why = match req.name {
                Some(name) => format!(
                    "encoder {name} did not open on this host (not a cascade name, no FFmpeg in this build, or no device accepts it)"
                ),
                None => e,
            };
            return Err(failed(Verdict::None, why).into());
        }
    };
    let mut enc = opened.enc;
    // Master named the codec a named encoder produces by its wire name.
    let actual_codec = if plan.cell.path == CellPath::Named && opened.codec == "h265" {
        "hevc"
    } else {
        opened.codec
    };
    let backend = enc.name();
    tracing::info!(backend, actual_codec = %actual_codec, "encoder smoke: backend selected");
    let mut verdict = Verdict::Ok;
    if codec != "h264" && actual_codec != codec {
        verdict = Verdict::Demoted;
        tracing::warn!(
            requested = %codec,
            actual = %actual_codec,
            "encoder smoke: demoted from requested codec"
        );
    }

    let mut keyframes = 0usize;
    let mut total_bytes = 0usize;
    for i in 0..FRAMES {
        let frame = solid_frame(w, h, i);
        if i == 5 {
            enc.request_keyframe();
        }
        let packets = enc.encode(frame).await?;
        for p in &packets {
            total_bytes += p.data.len();
            if p.is_keyframe {
                keyframes += 1;
            }
            if let Some(d) = dump.as_mut() {
                d.file
                    .write_all(&p.data)
                    .context("encoder smoke: --dump write failed")?;
            }
        }
    }
    if let Some(d) = dump.take() {
        d.file
            .flush()
            .context("encoder smoke: --dump flush failed")?;
        tracing::info!(
            path = d.path,
            total_bytes,
            "encoder smoke: bitstream dumped"
        );
    }
    tracing::info!(backend, keyframes, total_bytes, "encoder smoke: done");
    if backend == "noop" {
        return Err(failed(
            Verdict::None,
            "encoder smoke: fell through to NoopEncoder — HW and SW backends both failed".into(),
        )
        .into());
    }
    if keyframes == 0 {
        return Err(failed(
            Verdict::Error,
            format!("encoder smoke: no keyframes produced (backend={backend})"),
        )
        .into());
    }
    writeln!(
        out,
        "encoder smoke PASSED: backend={backend} keyframes={keyframes} total_bytes={total_bytes}"
    )?;
    Ok(Passed {
        backend,
        verdict,
        keyframes,
        total_bytes,
    })
}

/// Frame `i` of the smoke: alternating solid colours, so the encoder has
/// content to encode.
fn solid_frame(w: u32, h: u32, i: u32) -> Arc<Frame> {
    let mut data = vec![0u8; (w * h * 4) as usize];
    let (b, g, r) = match i % 3 {
        0 => (255, 0, 0),
        1 => (0, 255, 0),
        _ => (0, 0, 255),
    };
    for px in data.chunks_exact_mut(4) {
        px[0] = b;
        px[1] = g;
        px[2] = r;
        px[3] = 255;
    }
    Arc::new(Frame {
        width: w,
        height: h,
        stride: w * 4,
        pixel_format: PixelFormat::Bgra,
        data: data.into(),
        monotonic_us: u64::from(i) * 33_333,
        monitor: 0,
        damage: Damage::Unknown,
        source: None,
    })
}
