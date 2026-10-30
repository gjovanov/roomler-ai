// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-16 L1 — what this host's encoders actually cost.
//!
//! `roomlerd encoder-smoke --bench` opens each cell through the constructor a
//! session calls for it, encodes a synthetic motion profile ([`Scene`]) through
//! it, and prints one table row per cell: encode time p50/p95/max, the bitrate
//! produced against the one asked for, the keyframe (IDR) and delta frame size
//! distributions, and **which encoder actually opened**. That last column is
//! read from the opened encoder itself (`name()`, `is_hardware()`, the codec
//! and chroma it produces), never from what was requested or advertised, so a
//! path that has a hardware encoder and silently runs a software one says
//! `fallback` in its own row.
//!
//! The quick profile fits inside `roomler exec`'s 30 s default, so the fleet
//! profiles itself without a longer timeout; `--bench-full` is the minutes-long
//! matrix. Rows print as each cell finishes, every line starting `L1 `: the
//! console log shares stdout, and a run cut short still leaves the rows it
//! finished. The column set is a contract (fleet aggregation splits rows on
//! whitespace): columns are only ever appended. Reference:
//! `docs/testing.md` § L1.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use roomler_ai_remote_control::models::VideoBackend;

use super::{EncoderPreference, VideoEncoder};
use crate::capture::synthetic_scene::Scene;
use crate::capture::{Damage, Frame, PixelFormat};

/// FR-16 L1 — the bench's flags, flattened into `encoder-smoke`. Every one but
/// `--bench` itself requires `--bench`, so none of them can reach the plain
/// smoke the release lane runs.
#[derive(Debug, Clone, clap::Args)]
pub struct BenchArgs {
    /// FR-16 L1 — instead of the 10-frame smoke, bench this host's encoders.
    /// Each cell opens the way a session opens it and encodes a synthetic
    /// motion profile; one `L1 ` row per cell gives encode ms p50/p95, the
    /// bitrate produced against the target, IDR and delta frame sizes, and the
    /// encoder that actually opened (`fallback` when hardware was asked for
    /// and software answered). The default fits `roomler exec`'s 30 s timeout.
    #[arg(long, conflicts_with_all = ["reconfigure_sweep", "dump", "json"])]
    pub bench: bool,
    /// The full matrix: three sizes, two bitrates, three motion profiles and
    /// both chroma formats. Minutes, not seconds: pair it with
    /// `roomler exec --timeout 300`.
    #[arg(long, requires = "bench")]
    pub bench_full: bool,
    /// Cells, `path:codec[:chroma]`: path `rtp` (the RTP track) or `dc` (the
    /// DataChannel pumps), codec `h264`, `h265`, `av1` or `vp9`, chroma `420`
    /// (default) or `444`, e.g. `rtp:h264,dc:vp9:444`. Default: every cell a
    /// session on this build can open.
    #[arg(
        long,
        requires = "bench",
        conflicts_with = "name",
        value_delimiter = ','
    )]
    pub bench_cells: Vec<String>,
    /// Frame sizes, `WxH` with even sides, e.g. `1280x720,1920x1080`.
    /// Default `1280x720`.
    #[arg(long, requires = "bench", value_delimiter = ',')]
    pub bench_sizes: Vec<String>,
    /// Target bitrates: `auto` (what a session opens at for the size) or kbps
    /// (`3000`, `3000k`, `3M`). Default `auto`.
    #[arg(long, requires = "bench", value_delimiter = ',')]
    pub bench_bitrates: Vec<String>,
    /// Motion profiles: `static` (an idle desktop), `drag[:px/s]` (a window
    /// dragged at a fixed speed, 1000 px/s by default) or `pan` (every pixel
    /// moves). Default `drag`.
    #[arg(long, requires = "bench", value_delimiter = ',')]
    pub bench_motions: Vec<String>,
    /// Frames encoded per row. A keyframe is requested once a second.
    #[arg(
        long,
        requires = "bench",
        default_value_t = 90,
        value_parser = clap::value_parser!(u32).range(10..=3600)
    )]
    pub bench_frames: u32,
    /// The frame rate the encoder is told it runs at.
    #[arg(
        long,
        requires = "bench",
        default_value_t = 30,
        value_parser = clap::value_parser!(u32).range(1..=240)
    )]
    pub bench_fps: u32,
    /// Feed frames at `--bench-fps` instead of back to back. Back to back
    /// measures the encoder's own cost; paced runs at a session's cadence,
    /// where a CPU that clocks down between frames shows (the preamble's
    /// `clock=` says whether the bench's thread holds its clock).
    #[arg(long, requires = "bench")]
    pub bench_paced: bool,
    /// Start no cell after this many seconds; the rest print as `skipped`.
    /// Default 25 (inside exec's 30 s), 280 with `--bench-full`.
    #[arg(
        long,
        requires = "bench",
        value_parser = clap::value_parser!(u64).range(1..=3600)
    )]
    pub bench_budget_secs: Option<u64>,
}

/// What `encoder-smoke` runs. The plain smoke is what the release lane
/// invokes (`encoder-smoke --encoder hardware`); the FR-62 sweep and this
/// bench are opt-in modes beside it, and `--bench` conflicts with the sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmokeMode {
    Smoke,
    Sweep,
    Bench,
}

pub fn smoke_mode(reconfigure_sweep: bool, bench: bool) -> SmokeMode {
    if bench {
        SmokeMode::Bench
    } else if reconfigure_sweep {
        SmokeMode::Sweep
    } else {
        SmokeMode::Smoke
    }
}

/// Which pump's encoder a cell opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellPath {
    /// The RTP track's pump: `encode::open_for_codec`.
    Rtp,
    /// The DataChannel pumps: FFmpeg's hardware cascades, and libvpx for VP9.
    Dc,
    /// One FFmpeg encoder by name (`--name`), past every cascade.
    Named,
}

impl CellPath {
    fn label(self) -> &'static str {
        match self {
            Self::Rtp => "rtp",
            Self::Dc => "dc",
            Self::Named => "named",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
    Vp9,
}

impl Codec {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "h264" => Some(Self::H264),
            "h265" | "hevc" => Some(Self::H265),
            "av1" => Some(Self::Av1),
            "vp9" => Some(Self::Vp9),
            _ => None,
        }
    }

    /// The name `open_for_codec` takes and answers with.
    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
        }
    }
}

/// One codec on one path at one chroma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellSpec {
    pub path: CellPath,
    pub codec: Codec,
    pub chroma444: bool,
}

impl CellSpec {
    const fn new(path: CellPath, codec: Codec, chroma444: bool) -> Self {
        Self {
            path,
            codec,
            chroma444,
        }
    }

    /// `rtp:h264`, `dc:vp9:444`. A session cell only: `named` comes from
    /// `--name`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let lower = raw.trim().to_ascii_lowercase();
        let mut parts = lower.split(':');
        let path = match parts.next() {
            Some("rtp") => CellPath::Rtp,
            Some("dc") => CellPath::Dc,
            _ => return Err(format!("cell {raw:?}: the path is `rtp` or `dc`")),
        };
        let codec = parts
            .next()
            .and_then(Codec::parse)
            .ok_or_else(|| format!("cell {raw:?}: the codec is h264, h265, av1 or vp9"))?;
        let chroma444 = match parts.next() {
            None | Some("420") => false,
            Some("444") => true,
            Some(_) => return Err(format!("cell {raw:?}: the chroma is 420 or 444")),
        };
        if parts.next().is_some() {
            return Err(format!("cell {raw:?}: expected path:codec[:chroma]"));
        }
        let cell = Self::new(path, codec, chroma444);
        cell.check()?;
        Ok(cell)
    }

    /// Refuse what no session opens.
    fn check(self) -> Result<(), String> {
        match (self.path, self.codec, self.chroma444) {
            (CellPath::Rtp, Codec::Vp9, _) => {
                Err("the RTP track carries H.264, HEVC and AV1; VP9 rides the DataChannel".into())
            }
            (CellPath::Rtp, _, true) => Err("the RTP track is 4:2:0 only".into()),
            (_, Codec::Av1, true) => Err("no AV1 cell is 4:4:4".into()),
            _ => Ok(()),
        }
    }
}

/// The cells the quick profile benches, before the build filters them: each
/// codec on each path at the chroma a session opens it with. The VP9
/// DataChannel is 4:4:4 by default (`ROOMLERD_VP9_CHROMA`'s default).
const QUICK_CELLS: [CellSpec; 7] = [
    CellSpec::new(CellPath::Rtp, Codec::H264, false),
    CellSpec::new(CellPath::Rtp, Codec::H265, false),
    CellSpec::new(CellPath::Rtp, Codec::Av1, false),
    CellSpec::new(CellPath::Dc, Codec::H264, false),
    CellSpec::new(CellPath::Dc, Codec::H265, false),
    CellSpec::new(CellPath::Dc, Codec::Av1, false),
    CellSpec::new(CellPath::Dc, Codec::Vp9, true),
];

/// The full matrix's cells: the quick ones and every other chroma a path takes.
const FULL_CELLS: [CellSpec; 10] = [
    CellSpec::new(CellPath::Rtp, Codec::H264, false),
    CellSpec::new(CellPath::Rtp, Codec::H265, false),
    CellSpec::new(CellPath::Rtp, Codec::Av1, false),
    CellSpec::new(CellPath::Dc, Codec::H264, false),
    CellSpec::new(CellPath::Dc, Codec::H264, true),
    CellSpec::new(CellPath::Dc, Codec::H265, false),
    CellSpec::new(CellPath::Dc, Codec::H265, true),
    CellSpec::new(CellPath::Dc, Codec::Av1, false),
    CellSpec::new(CellPath::Dc, Codec::Vp9, false),
    CellSpec::new(CellPath::Dc, Codec::Vp9, true),
];

const QUICK_SIZES: [(u32, u32); 1] = [(1280, 720)];
const FULL_SIZES: [(u32, u32); 3] = [(1280, 720), (1920, 1080), (2560, 1440)];
const QUICK_BUDGET_SECS: u64 = 25;
const FULL_BUDGET_SECS: u64 = 280;

/// A target bitrate: what a session opens at for the size, or a fixed rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bitrate {
    Auto,
    Bps(u32),
}

impl Bitrate {
    /// `auto`, or kbps: `3000`, `3000k`, `3M` (`1.5M` too). 100 kbps..100 Mbps.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let s = raw.trim().to_ascii_lowercase();
        if s == "auto" {
            return Ok(Self::Auto);
        }
        let (num, scale) = if let Some(n) = s.strip_suffix('m') {
            (n, 1_000_000.0)
        } else if let Some(n) = s.strip_suffix('k') {
            (n, 1_000.0)
        } else {
            (s.as_str(), 1_000.0)
        };
        let bps = num
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| v * scale)
            .ok_or_else(|| format!("bitrate {raw:?}: `auto` or kbps, e.g. 3000, 3000k, 3M"))?;
        if !(100_000.0..=100_000_000.0).contains(&bps) {
            return Err(format!("bitrate {raw:?}: between 100k and 100M"));
        }
        Ok(Self::Bps(bps.round() as u32))
    }

    /// The rate for a cell: `auto` is what a session's encoder opens at for
    /// the size and rate (`encode::initial_bitrate_for_fps`).
    fn resolve(self, width: u32, height: u32, fps: u32) -> u32 {
        match self {
            Self::Auto => super::initial_bitrate_for_fps(width, height, fps),
            Self::Bps(bps) => bps,
        }
    }
}

/// `WxH`, both sides even and 64..=7680 by 64..=4320.
pub fn parse_size(raw: &str) -> Result<(u32, u32), String> {
    let bad = || format!("size {raw:?}: WxH with even sides, e.g. 1280x720");
    let (w, h) = raw.trim().split_once(['x', 'X']).ok_or_else(bad)?;
    let w: u32 = w.trim().parse().map_err(|_| bad())?;
    let h: u32 = h.trim().parse().map_err(|_| bad())?;
    if !(64..=7680).contains(&w)
        || !(64..=4320).contains(&h)
        || !w.is_multiple_of(2)
        || !h.is_multiple_of(2)
    {
        return Err(bad());
    }
    Ok((w, h))
}

/// What this build can open, per path. [`BuildCaps::this_build`] reads the
/// compiled features; a test passes its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildCaps {
    /// Media Foundation: the RTP track's hardware H.264, HEVC and AV1 (Windows).
    pub mf: bool,
    /// OpenH264: the RTP track's software H.264.
    pub openh264: bool,
    /// FFmpeg's hardware cascades: the DataChannel pumps, and the RTP track's
    /// HEVC. Off when `ROOMLERD_USE_FFMPEG=0`, as it is for a session.
    pub ffmpeg: bool,
    /// libvpx: the DataChannel's software VP9.
    pub libvpx: bool,
}

impl BuildCaps {
    pub fn this_build() -> Self {
        Self {
            mf: cfg!(all(target_os = "windows", feature = "mf-encoder")),
            openh264: cfg!(feature = "openh264-encoder"),
            ffmpeg: ffmpeg_on(),
            libvpx: cfg!(feature = "vp9-444"),
        }
    }

    /// Could a session on this build open `cell` at all?
    fn opens(self, cell: CellSpec) -> bool {
        match (cell.path, cell.codec) {
            (CellPath::Rtp, Codec::H264) => self.mf || self.openh264,
            (CellPath::Rtp, Codec::H265) => self.mf || self.ffmpeg,
            (CellPath::Rtp, Codec::Av1) => self.mf,
            (CellPath::Rtp, Codec::Vp9) => false,
            (CellPath::Dc, Codec::Vp9) => self.ffmpeg || self.libvpx,
            (CellPath::Dc, _) | (CellPath::Named, _) => self.ffmpeg,
        }
    }

    /// Does `cell`'s path have a hardware encoder in this build? Without one,
    /// software answering a request for hardware is the build, not a fallback.
    fn hardware_on_path(self, cell: CellSpec) -> bool {
        match (cell.path, cell.codec) {
            (CellPath::Rtp, Codec::H265) => self.mf || self.ffmpeg,
            (CellPath::Rtp, _) => self.mf,
            (CellPath::Dc, _) | (CellPath::Named, _) => self.ffmpeg,
        }
    }

    fn label(self) -> String {
        let on: Vec<&str> = [
            (self.mf, "mf"),
            (self.openh264, "openh264"),
            (self.ffmpeg, "ffmpeg"),
            (self.libvpx, "libvpx"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| *name)
        .collect();
        if on.is_empty() {
            "none".into()
        } else {
            on.join(",")
        }
    }
}

fn ffmpeg_on() -> bool {
    #[cfg(feature = "ffmpeg-encoder")]
    {
        super::ffmpeg::available()
    }
    #[cfg(not(feature = "ffmpeg-encoder"))]
    {
        false
    }
}

/// Does the session ask this path for hardware? The RTP pump asks for what
/// the preference says; the DataChannel cascades and a named encoder are
/// hardware by construction, whatever the preference.
fn requests_hardware(path: CellPath, pref: EncoderPreference) -> bool {
    match path {
        CellPath::Rtp => pref == EncoderPreference::Hardware,
        CellPath::Dc | CellPath::Named => true,
    }
}

/// One row to run: a cell at one size, rate and motion.
#[derive(Debug, Clone, PartialEq)]
pub struct CellPlan {
    pub cell: CellSpec,
    /// The FFmpeg encoder name, for a `named` cell.
    pub name: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub target_bps: u32,
    pub motion: Scene,
    pub requested_hw: bool,
    pub hardware_on_path: bool,
}

/// Everything a run needs, parsed and checked before anything opens.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchPlan {
    pub plans: Vec<CellPlan>,
    pub frames: u32,
    pub fps: u32,
    pub paced: bool,
    pub budget: Duration,
}

/// Build the plan from the flags. A bad value is an error here, before any
/// encoder opens.
pub fn plan(
    args: &BenchArgs,
    pref: EncoderPreference,
    name: Option<&str>,
    build: BuildCaps,
) -> Result<BenchPlan, String> {
    let fps = args.bench_fps;
    let cells: Vec<(CellSpec, Option<String>)> = if let Some(name) = name {
        let (codec, _) = VideoBackend::from_ffmpeg_name(name)
            .ok_or_else(|| format!("--name {name:?} is not an FFmpeg encoder name"))?;
        let codec = Codec::parse(codec.wire())
            .ok_or_else(|| format!("--name {name:?}: no bench codec for it"))?;
        vec![(
            CellSpec::new(CellPath::Named, codec, false),
            Some(name.to_string()),
        )]
    } else if !args.bench_cells.is_empty() {
        args.bench_cells
            .iter()
            .map(|raw| CellSpec::parse(raw).map(|c| (c, None)))
            .collect::<Result<_, _>>()?
    } else {
        let preset: &[CellSpec] = if args.bench_full {
            &FULL_CELLS
        } else {
            &QUICK_CELLS
        };
        preset
            .iter()
            .filter(|c| build.opens(**c))
            .map(|c| (*c, None))
            .collect()
    };
    if cells.is_empty() {
        return Err(format!(
            "this build opens no encoder cell (built with: {})",
            build.label()
        ));
    }
    let sizes: Vec<(u32, u32)> = if !args.bench_sizes.is_empty() {
        args.bench_sizes
            .iter()
            .map(|s| parse_size(s))
            .collect::<Result<_, _>>()?
    } else if args.bench_full {
        FULL_SIZES.to_vec()
    } else {
        QUICK_SIZES.to_vec()
    };
    let rates: Vec<Bitrate> = if !args.bench_bitrates.is_empty() {
        args.bench_bitrates
            .iter()
            .map(|s| Bitrate::parse(s))
            .collect::<Result<_, _>>()?
    } else if args.bench_full {
        vec![Bitrate::Auto, Bitrate::Bps(super::MIN_BITRATE_BPS)]
    } else {
        vec![Bitrate::Auto]
    };
    let motions: Vec<Scene> = if !args.bench_motions.is_empty() {
        args.bench_motions
            .iter()
            .map(|s| {
                Scene::parse(s).ok_or_else(|| {
                    format!("motion {s:?}: static, drag, drag:<px/s> (1..20000) or pan")
                })
            })
            .collect::<Result<_, _>>()?
    } else if args.bench_full {
        vec![
            Scene::Static,
            Scene::parse("drag").expect("the default drag parses"),
            Scene::Pan,
        ]
    } else {
        vec![Scene::parse("drag").expect("the default drag parses")]
    };
    let mut plans = Vec::new();
    for &(width, height) in &sizes {
        for (cell, name) in &cells {
            let cell = *cell;
            for &rate in &rates {
                for &motion in &motions {
                    plans.push(CellPlan {
                        cell,
                        name: name.clone(),
                        width,
                        height,
                        fps,
                        target_bps: rate.resolve(width, height, fps),
                        motion,
                        requested_hw: requests_hardware(cell.path, pref),
                        hardware_on_path: build.hardware_on_path(cell),
                    });
                }
            }
        }
    }
    let budget = args.bench_budget_secs.unwrap_or(if args.bench_full {
        FULL_BUDGET_SECS
    } else {
        QUICK_BUDGET_SECS
    });
    Ok(BenchPlan {
        plans,
        frames: args.bench_frames,
        fps,
        paced: args.bench_paced,
        budget: Duration::from_secs(budget),
    })
}

/// The bench's answer for one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// What opened is what was asked for.
    Ok,
    /// Hardware was asked for on a path that has a hardware encoder in this
    /// build, and a software encoder answered.
    Fallback,
    /// Hardware was asked for on a path with no hardware encoder in this
    /// build (the RTP track's H.264 off Windows, libvpx without FFmpeg).
    SwOnly,
    /// It opened, but produces another codec or chroma than asked for.
    Demoted,
    /// Nothing opened.
    None,
    /// It opened and an encode failed.
    Error,
    /// The budget ran out before the cell started.
    Skipped,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Fallback => "fallback",
            Self::SwOnly => "sw-only",
            Self::Demoted => "demoted",
            Self::None => "none",
            Self::Error => "error",
            Self::Skipped => "skipped",
        }
    }
}

/// What actually opened, read from the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenedId {
    /// `VideoEncoder::name()`.
    pub name: &'static str,
    /// `VideoEncoder::is_hardware()`: `mf-h264` names both Microsoft's
    /// hardware and software MFTs, and only this tells them apart.
    pub hw: bool,
    /// The codec it produces, as `open_for_codec` names it.
    pub codec: &'static str,
    pub chroma444: bool,
}

/// The verdict for a cell from what opened. `None` when nothing did.
pub fn verdict(plan: &CellPlan, opened: Option<&OpenedId>) -> Verdict {
    let Some(o) = opened else {
        return Verdict::None;
    };
    if o.name == "noop" {
        return Verdict::None;
    }
    if o.codec != plan.cell.codec.label() || o.chroma444 != plan.cell.chroma444 {
        return Verdict::Demoted;
    }
    if plan.requested_hw && !o.hw {
        return if plan.hardware_on_path {
            Verdict::Fallback
        } else {
            Verdict::SwOnly
        };
    }
    Verdict::Ok
}

/// The `p`-th percentile (0..=100) of `sorted` by nearest rank: the smallest
/// sample with at least `p` % of the samples at or below it. `None` when empty.
pub fn percentile<T: Copy>(sorted: &[T], p: u32) -> Option<T> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    let rank = (p.min(100) as usize * n).div_ceil(100).clamp(1, n);
    Some(sorted[rank - 1])
}

/// A frame-size distribution, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SizeDist {
    pub n: usize,
    pub p50: usize,
    pub p95: usize,
    pub max: usize,
}

pub fn size_dist(sizes: &[usize]) -> SizeDist {
    let mut sorted = sizes.to_vec();
    sorted.sort_unstable();
    SizeDist {
        n: sorted.len(),
        p50: percentile(&sorted, 50).unwrap_or(0),
        p95: percentile(&sorted, 95).unwrap_or(0),
        max: sorted.last().copied().unwrap_or(0),
    }
}

/// The bitrate produced over `frames` frames at `fps`: the content's own
/// duration, not the wall clock, so a paced and a back-to-back run of one cell
/// agree.
pub fn produced_bps(total_bytes: u64, frames: u32, fps: u32) -> f64 {
    if frames == 0 {
        return 0.0;
    }
    total_bytes as f64 * 8.0 * fps as f64 / frames as f64
}

/// Produced over target: 1.0 is on target. A CBR encoder (QSV, libvpx's
/// realtime CBR) is held to it; a quality-driven one under a cap (NVENC, AMF,
/// VAAPI, VideoToolbox) sits below it on easy content, which is headroom, not
/// a miss (the FR-62 A0 sweep found the same).
pub fn rate_ratio(produced_bps: f64, target_bps: u32) -> f64 {
    if target_bps == 0 {
        0.0
    } else {
        produced_bps / target_bps as f64
    }
}

/// One row's measurements.
#[derive(Debug, Clone, PartialEq)]
pub struct CellResult {
    pub plan: CellPlan,
    pub opened: Option<OpenedId>,
    pub verdict: Verdict,
    pub open_ms: Option<f64>,
    /// Frames encoded; the percentiles are over these.
    pub frames: u32,
    pub enc_p50_ms: Option<f64>,
    pub enc_p95_ms: Option<f64>,
    pub enc_max_ms: Option<f64>,
    pub produced_bps: Option<f64>,
    pub idr: SizeDist,
    pub delta: SizeDist,
}

/// The table's columns, in order. A contract: fleet aggregation splits each
/// `L1 ` row on whitespace and reads columns by position, so a column is only
/// ever appended, never renamed, moved or dropped.
pub const COLUMNS: [&str; 23] = [
    "path",
    "codec",
    "chroma",
    "size",
    "motion",
    "target_kbps",
    "opened",
    "hw",
    "got",
    "verdict",
    "open_ms",
    "frames",
    "enc_p50_ms",
    "enc_p95_ms",
    "enc_max_ms",
    "out_kbps",
    "rate",
    "idr_n",
    "idr_p50_B",
    "idr_max_B",
    "delta_p50_B",
    "delta_p95_B",
    "delta_max_B",
];

/// Display widths, so the columns line up for a person; a parser splits on
/// whitespace and never needs them.
const WIDTHS: [usize; 23] = [
    5, 5, 6, 9, 9, 11, 17, 3, 8, 8, 8, 6, 10, 10, 10, 8, 5, 5, 9, 9, 11, 11, 11,
];

/// Every table line starts with this, because the console log shares stdout.
pub const PREFIX: &str = "L1 ";

fn line(fields: &[String]) -> String {
    let mut out = String::from(PREFIX);
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(&format!("{f:<width$}", width = WIDTHS[i]));
    }
    out.trim_end().to_string()
}

pub fn header_line() -> String {
    line(&COLUMNS.map(String::from))
}

pub fn row_line(r: &CellResult) -> String {
    let dash = || "-".to_string();
    let ms = |v: Option<f64>| v.map_or_else(dash, |v| format!("{v:.2}"));
    let bytes = |n: usize, v: usize| if n == 0 { dash() } else { v.to_string() };
    let chroma = |c444: bool| if c444 { "444" } else { "420" };
    let p = &r.plan;
    let fields = [
        p.cell.path.label().to_string(),
        p.cell.codec.label().to_string(),
        chroma(p.cell.chroma444).to_string(),
        format!("{}x{}", p.width, p.height),
        p.motion.label(),
        (p.target_bps / 1000).to_string(),
        r.opened.map_or_else(dash, |o| o.name.to_string()),
        r.opened
            .map_or_else(dash, |o| if o.hw { "hw" } else { "sw" }.to_string()),
        r.opened
            .map_or_else(dash, |o| format!("{}:{}", o.codec, chroma(o.chroma444))),
        r.verdict.label().to_string(),
        ms(r.open_ms),
        if r.frames == 0 {
            dash()
        } else {
            r.frames.to_string()
        },
        ms(r.enc_p50_ms),
        ms(r.enc_p95_ms),
        ms(r.enc_max_ms),
        r.produced_bps
            .map_or_else(dash, |b| format!("{:.0}", b / 1000.0)),
        r.produced_bps
            .map_or_else(dash, |b| format!("{:.2}", rate_ratio(b, p.target_bps))),
        r.idr.n.to_string(),
        bytes(r.idr.n, r.idr.p50),
        bytes(r.idr.n, r.idr.max),
        bytes(r.delta.n, r.delta.p50),
        bytes(r.delta.n, r.delta.p95),
        bytes(r.delta.n, r.delta.max),
    ];
    line(&fields)
}

/// An opened encoder and what it produces.
pub struct Opened {
    pub enc: Box<dyn VideoEncoder>,
    /// The codec it produces, as `open_for_codec` names it (`h265`, not `hevc`).
    pub codec: &'static str,
    pub chroma444: bool,
}

/// How a cell is opened: the seam. [`SessionOpener`] opens through the
/// constructors a session calls; a test passes a fake, so the verdict and the
/// table are exercised with no encoder hardware.
pub trait CellOpener {
    /// Open the cell, already set to the plan's target rate.
    fn open(&mut self, plan: &CellPlan) -> Result<Opened, String>;
}

/// Opens each cell through the constructor a session calls for it, so the
/// encoder that answers is the one a session would get.
pub struct SessionOpener {
    pub pref: EncoderPreference,
}

impl CellOpener for SessionOpener {
    fn open(&mut self, plan: &CellPlan) -> Result<Opened, String> {
        let (w, h) = (plan.width, plan.height);
        match plan.cell.path {
            CellPath::Named => {
                let name = plan.name.as_deref().unwrap_or_default();
                let (mut enc, codec) = super::open_named(name, w, h).ok_or_else(|| {
                    format!(
                        "{name} did not open (not a cascade name, no FFmpeg, or no device takes it)"
                    )
                })?;
                enc.set_bitrate(plan.target_bps);
                Ok(Opened {
                    enc,
                    codec: codec_label(codec),
                    chroma444: false,
                })
            }
            CellPath::Rtp => {
                // peer.rs's RTP pump: `open_for_codec`, then the pump's rate.
                let (mut enc, codec) =
                    super::open_for_codec(plan.cell.codec.label(), w, h, self.pref);
                enc.set_bitrate(plan.target_bps);
                Ok(Opened {
                    enc,
                    codec: codec_label(codec),
                    chroma444: false,
                })
            }
            CellPath::Dc => match plan.cell.codec {
                Codec::Vp9 => open_dc_vp9(plan),
                _ => open_dc_ffmpeg(plan),
            },
        }
    }
}

/// `VideoCodec::wire()` says `hevc` where `open_for_codec` says `h265`.
fn codec_label(wire: &'static str) -> &'static str {
    if wire == "hevc" { "h265" } else { wire }
}

/// peer.rs's `FfmpegDcCodec::open` at a session's first open: no remembered
/// backend yet, no quality bias, a direct transport.
#[cfg(feature = "ffmpeg-encoder")]
fn open_dc_ffmpeg(plan: &CellPlan) -> Result<Opened, String> {
    use super::ffmpeg::FfmpegEncoder;
    let (w, h, fps) = (plan.width, plan.height, plan.fps);
    let maxrate = plan.target_bps as usize;
    let c444 = plan.cell.chroma444;
    let enc = match plan.cell.codec {
        Codec::H264 => FfmpegEncoder::new_h264_adaptive(w, h, fps, maxrate, 0, c444, false),
        Codec::H265 => FfmpegEncoder::new_hevc_adaptive(w, h, fps, maxrate, 0, c444, false),
        Codec::Av1 => FfmpegEncoder::new_av1_adaptive(w, h, fps, maxrate, 0, false),
        Codec::Vp9 => FfmpegEncoder::new_vp9_adaptive(w, h, fps, maxrate, 0, c444, false),
    }
    .map_err(|e| format!("{e:#}"))?;
    let chroma444 = enc.chroma444();
    Ok(Opened {
        enc: Box::new(enc),
        codec: plan.cell.codec.label(),
        chroma444,
    })
}

#[cfg(not(feature = "ffmpeg-encoder"))]
fn open_dc_ffmpeg(_plan: &CellPlan) -> Result<Opened, String> {
    Err("this build carries no FFmpeg backend".into())
}

/// peer.rs's `data-channel-vp9-444` arm: FFmpeg's VP9 when its quick probe
/// opens the hardware cell, libvpx otherwise.
fn open_dc_vp9(plan: &CellPlan) -> Result<Opened, String> {
    #[cfg(feature = "ffmpeg-encoder")]
    {
        if super::ffmpeg::available() && vp9_hardware_probe(plan.cell.chroma444) {
            return open_dc_ffmpeg(plan);
        }
    }
    open_libvpx(plan)
}

/// The session's quick probe, at the capability probe's size: the first
/// 4:4:4-capable VP9 encoder the denylist leaves, or `new_vp9` for 4:2:0.
#[cfg(feature = "ffmpeg-encoder")]
fn vp9_hardware_probe(chroma444: bool) -> bool {
    use super::ffmpeg::FfmpegEncoder;
    use roomler_ai_remote_control::models::VideoCodec;
    if chroma444 {
        super::cells::names_444(VideoCodec::Vp9)
            .first()
            .copied()
            .is_some_and(|name| FfmpegEncoder::new_named_probe(name, 480, 270, true).is_ok())
    } else {
        FfmpegEncoder::new_vp9(480, 270).is_ok()
    }
}

#[cfg(feature = "vp9-444")]
fn open_libvpx(plan: &CellPlan) -> Result<Opened, String> {
    use super::libvpx::{Vp9Chroma, Vp9Encoder};
    let chroma = if plan.cell.chroma444 {
        Vp9Chroma::Yuv444
    } else {
        Vp9Chroma::Yuv420
    };
    let mut enc = Vp9Encoder::new_with_fps_chroma(plan.width, plan.height, plan.fps, chroma)
        .map_err(|e| format!("{e:#}"))?;
    enc.set_bitrate(plan.target_bps);
    Ok(Opened {
        enc: Box::new(enc),
        codec: "vp9",
        // libvpx opens the chroma it is given or not at all.
        chroma444: plan.cell.chroma444,
    })
}

#[cfg(not(feature = "vp9-444"))]
fn open_libvpx(_plan: &CellPlan) -> Result<Opened, String> {
    Err("this build carries neither FFmpeg's VP9 nor libvpx".into())
}

/// Run one row: open, encode `frames` frames of the motion, measure.
async fn run_cell(
    plan: &CellPlan,
    opener: &mut dyn CellOpener,
    frames: u32,
    paced: bool,
    deadline: Instant,
) -> CellResult {
    let mut result = CellResult {
        plan: plan.clone(),
        opened: None,
        verdict: Verdict::None,
        open_ms: None,
        frames: 0,
        enc_p50_ms: None,
        enc_p95_ms: None,
        enc_max_ms: None,
        produced_bps: None,
        idr: SizeDist::default(),
        delta: SizeDist::default(),
    };
    tracing::info!(
        cell = %format!("{}:{}", plan.cell.path.label(), plan.cell.codec.label()),
        chroma444 = plan.cell.chroma444,
        w = plan.width,
        h = plan.height,
        motion = %plan.motion.label(),
        target_bps = plan.target_bps,
        "L1 bench: opening"
    );
    let t = Instant::now();
    let opened = opener.open(plan);
    result.open_ms = Some(t.elapsed().as_secs_f64() * 1000.0);
    let mut opened = match opened {
        Ok(o) => o,
        Err(why) => {
            tracing::info!(%why, "L1 bench: the cell did not open");
            return result;
        }
    };
    let id = OpenedId {
        name: opened.enc.name(),
        hw: opened.enc.is_hardware(),
        codec: opened.codec,
        chroma444: opened.chroma444,
    };
    result.opened = Some(id);
    result.verdict = verdict(plan, Some(&id));
    if result.verdict == Verdict::None {
        return result;
    }
    opened.enc.set_frame_rate(plan.fps);

    let (w, h, fps) = (plan.width, plan.height, plan.fps);
    let mut enc_ms: Vec<f64> = Vec::with_capacity(frames as usize);
    let mut idr_sizes = Vec::new();
    let mut delta_sizes = Vec::new();
    let mut total_bytes: u64 = 0;
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let start = Instant::now();
    for k in 0..frames {
        if Instant::now() >= deadline {
            break;
        }
        let frame = Arc::new(Frame {
            width: w,
            height: h,
            stride: w * 4,
            pixel_format: PixelFormat::Bgra,
            data: plan.motion.render(k as u64, w, h, fps).into(),
            monotonic_us: k as u64 * 1_000_000 / fps as u64,
            monitor: 0,
            damage: Damage::Unknown,
            source: None,
        });
        if k > 0 && k.is_multiple_of(fps) {
            opened.enc.request_keyframe();
        }
        let t = Instant::now();
        let packets = match opened.enc.encode(frame).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(%e, frame = k, "L1 bench: encode failed");
                result.verdict = Verdict::Error;
                break;
            }
        };
        enc_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        // A frame is everything one `encode()` returns: OpenH264 hands an IDR
        // back as two packets (the parameter sets, then the slice), and a
        // count of packets would count every IDR twice.
        let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
        if bytes > 0 {
            total_bytes += bytes as u64;
            if packets.iter().any(|p| p.is_keyframe) {
                idr_sizes.push(bytes);
            } else {
                delta_sizes.push(bytes);
            }
        }
        if paced {
            let due = start + interval * (k + 1);
            let now = Instant::now();
            if due > now {
                tokio::time::sleep(due - now).await;
            }
        }
    }
    result.frames = enc_ms.len() as u32;
    enc_ms.sort_unstable_by(f64::total_cmp);
    result.enc_p50_ms = percentile(&enc_ms, 50);
    result.enc_p95_ms = percentile(&enc_ms, 95);
    result.enc_max_ms = enc_ms.last().copied();
    if result.frames > 0 {
        result.produced_bps = Some(produced_bps(total_bytes, result.frames, fps));
    }
    result.idr = size_dist(&idr_sizes);
    result.delta = size_dist(&delta_sizes);
    result
}

fn skipped(plan: &CellPlan) -> CellResult {
    CellResult {
        plan: plan.clone(),
        opened: None,
        verdict: Verdict::Skipped,
        open_ms: None,
        frames: 0,
        enc_p50_ms: None,
        enc_p95_ms: None,
        enc_max_ms: None,
        produced_bps: None,
        idr: SizeDist::default(),
        delta: SizeDist::default(),
    }
}

/// Run every row, printing each as it finishes. Returns the results.
pub async fn run_plan(
    plan: &BenchPlan,
    opener: &mut dyn CellOpener,
    out: &mut (dyn Write + Send),
) -> std::io::Result<Vec<CellResult>> {
    let started = Instant::now();
    let deadline = started + plan.budget;
    writeln!(out, "{}", header_line())?;
    out.flush()?;
    let mut results = Vec::with_capacity(plan.plans.len());
    for cell in &plan.plans {
        let r = if Instant::now() >= deadline {
            skipped(cell)
        } else {
            run_cell(cell, opener, plan.frames, plan.paced, deadline).await
        };
        writeln!(out, "{}", row_line(&r))?;
        out.flush()?;
        results.push(r);
    }
    let count = |v: Verdict| results.iter().filter(|r| r.verdict == v).count();
    writeln!(
        out,
        "{PREFIX}# done cells={} ok={} fallback={} sw-only={} demoted={} none={} error={} skipped={} elapsed_s={:.1}",
        results.len(),
        count(Verdict::Ok),
        count(Verdict::Fallback),
        count(Verdict::SwOnly),
        count(Verdict::Demoted),
        count(Verdict::None),
        count(Verdict::Error),
        count(Verdict::Skipped),
        started.elapsed().as_secs_f64()
    )?;
    writeln!(
        out,
        "{PREFIX}# verdict: ok = opened as asked; fallback = this path has a hardware encoder and \
         software answered; sw-only = no hardware encoder on this path in this build; demoted = \
         another codec or chroma; none = nothing opened; skipped = past the budget"
    )?;
    out.flush()?;
    Ok(results)
}

/// What the bench's thread got when it asked for its clock, for the preamble.
pub fn clock_label(outcome: super::clock::Outcome) -> String {
    use super::clock::Outcome;
    match outcome {
        Outcome::Clamped => "clamped".into(),
        Outcome::Off => "off".into(),
        Outcome::Refused(errno) => format!("refused:{errno}"),
        Outcome::NotLinux => "n/a".into(),
    }
}

/// `encoder-smoke --bench`: plan, print the run's settings, run every row.
///
/// Every cell opens, encodes and drops on ONE thread of the bench's own, the
/// way a session's encoder lives on its thread (`encode::thread`): the thread
/// asks for its clock first (FR-112 P1, Linux), so the threads an encoder
/// creates inherit it, and a thread-affine encoder (QSV) never changes threads
/// between frames, which a paced row's sleeps would allow on the runtime.
pub async fn run(args: &BenchArgs, pref_raw: &str, name: Option<&str>) -> anyhow::Result<()> {
    let pref: EncoderPreference = pref_raw
        .parse()
        .map_err(|e| anyhow::anyhow!("bad encoder preference {pref_raw:?}: {e}"))?;
    let build = BuildCaps::this_build();
    let bench = plan(args, pref, name, build).map_err(|e| anyhow::anyhow!("encoder bench: {e}"))?;
    let worker = std::thread::Builder::new()
        .name("rc-enc-l1-bench".into())
        .spawn(move || -> anyhow::Result<()> {
            let clock = super::clock::ask_for_this_thread("bench");
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()?;
            let mut out = std::io::stdout();
            writeln!(
                out,
                "{PREFIX}# FR-16 L1 encoder bench roomlerd={} os={} arch={} build={} encoder={} \
                 fps={} frames={} pace={} budget_s={} rows={} clock={}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH,
                build.label(),
                pref.wire(),
                bench.fps,
                bench.frames,
                if bench.paced { "paced" } else { "back-to-back" },
                bench.budget.as_secs(),
                bench.plans.len(),
                clock_label(clock)
            )?;
            let mut opener = SessionOpener { pref };
            rt.block_on(run_plan(&bench, &mut opener, &mut out))?;
            Ok(())
        })?;
    tokio::task::spawn_blocking(move || worker.join())
        .await?
        .map_err(|_| anyhow::anyhow!("encoder bench: the bench thread panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::EncodedPacket;

    fn args() -> BenchArgs {
        BenchArgs {
            bench: true,
            bench_full: false,
            bench_cells: Vec::new(),
            bench_sizes: Vec::new(),
            bench_bitrates: Vec::new(),
            bench_motions: Vec::new(),
            bench_frames: 90,
            bench_fps: 30,
            bench_paced: false,
            bench_budget_secs: None,
        }
    }

    const ALL: BuildCaps = BuildCaps {
        mf: true,
        openh264: true,
        ffmpeg: true,
        libvpx: true,
    };
    /// A Linux or macOS release build: no Media Foundation.
    const RELEASE_LINUX: BuildCaps = BuildCaps {
        mf: false,
        openh264: true,
        ffmpeg: true,
        libvpx: true,
    };
    /// The software build this PR's container run used.
    const SOFTWARE: BuildCaps = BuildCaps {
        mf: false,
        openh264: true,
        ffmpeg: false,
        libvpx: true,
    };

    fn cells(p: &BenchPlan) -> Vec<String> {
        p.plans
            .iter()
            .map(|c| {
                let base = format!("{}:{}", c.cell.path.label(), c.cell.codec.label());
                if c.cell.chroma444 {
                    format!("{base}:444")
                } else {
                    base
                }
            })
            .collect()
    }

    #[test]
    fn nearest_rank_percentiles_on_fixed_inputs() {
        assert_eq!(percentile::<u32>(&[], 50), None);
        assert_eq!(percentile(&[7], 0), Some(7));
        assert_eq!(percentile(&[7], 50), Some(7));
        assert_eq!(percentile(&[7], 100), Some(7));
        let hundred: Vec<u32> = (1..=100).collect();
        assert_eq!(percentile(&hundred, 0), Some(1));
        assert_eq!(percentile(&hundred, 50), Some(50));
        assert_eq!(percentile(&hundred, 95), Some(95));
        assert_eq!(percentile(&hundred, 100), Some(100));
        // 90 frames, the default row: p95 is the 86th sample (⌈85.5⌉).
        let ninety: Vec<u32> = (1..=90).collect();
        assert_eq!(percentile(&ninety, 95), Some(86));
        assert_eq!(percentile(&ninety, 50), Some(45));
        assert_eq!(percentile(&[1, 2, 3, 4], 50), Some(2));
    }

    #[test]
    fn size_distributions_on_fixed_inputs() {
        assert_eq!(size_dist(&[]), SizeDist::default());
        assert_eq!(
            size_dist(&[300, 100, 200]),
            SizeDist {
                n: 3,
                p50: 200,
                p95: 300,
                max: 300
            }
        );
        let mut deltas: Vec<usize> = (1..=20).map(|i| i * 100).collect();
        deltas.reverse();
        assert_eq!(
            size_dist(&deltas),
            SizeDist {
                n: 20,
                p50: 1000,
                p95: 1900,
                max: 2000
            }
        );
    }

    #[test]
    fn bitrate_accuracy_is_produced_over_target_on_the_contents_clock() {
        // 90 frames at 30 fps is 3 s; 1 875 000 bytes over 3 s is 5 Mbps.
        let bps = produced_bps(1_875_000, 90, 30);
        assert!((bps - 5_000_000.0).abs() < 1e-6, "{bps}");
        assert!((rate_ratio(bps, 5_000_000) - 1.0).abs() < 1e-9);
        assert!((rate_ratio(bps, 4_000_000) - 1.25).abs() < 1e-9);
        assert!((rate_ratio(bps, 10_000_000) - 0.5).abs() < 1e-9);
        assert_eq!(produced_bps(1_000, 0, 30), 0.0);
        assert_eq!(rate_ratio(1_000.0, 0), 0.0);
    }

    fn cell_plan(cell: &str, requested_hw: bool, hardware_on_path: bool) -> CellPlan {
        CellPlan {
            cell: CellSpec::parse(cell).unwrap(),
            name: None,
            width: 320,
            height: 240,
            fps: 30,
            target_bps: 1_500_000,
            motion: Scene::Static,
            requested_hw,
            hardware_on_path,
        }
    }

    fn id(name: &'static str, hw: bool, codec: &'static str, chroma444: bool) -> OpenedId {
        OpenedId {
            name,
            hw,
            codec,
            chroma444,
        }
    }

    /// The fallback verdict comes from what OPENED: asked for hardware on a
    /// path that has it, got software, reads `fallback`; opened as asked reads
    /// clean.
    #[test]
    fn the_verdict_is_read_from_what_opened() {
        let rtp = cell_plan("rtp:h264", true, true);
        assert_eq!(
            verdict(&rtp, Some(&id("mf-h264", false, "h264", false))),
            Verdict::Fallback,
            "Microsoft's software MFT answering a hardware request (#2283's class)"
        );
        assert_eq!(
            verdict(&rtp, Some(&id("mf-h264", true, "h264", false))),
            Verdict::Ok
        );
        assert_eq!(
            verdict(&rtp, Some(&id("openh264", false, "h264", false))),
            Verdict::Fallback
        );
        // No hardware encoder on this path in this build: the build, not a fallback.
        let no_hw = cell_plan("rtp:h264", true, false);
        assert_eq!(
            verdict(&no_hw, Some(&id("openh264", false, "h264", false))),
            Verdict::SwOnly
        );
        // Software asked for, software opened.
        let sw = cell_plan("rtp:h264", false, true);
        assert_eq!(
            verdict(&sw, Some(&id("openh264", false, "h264", false))),
            Verdict::Ok
        );
        // The DataChannel's VP9 falling to libvpx.
        let vp9 = cell_plan("dc:vp9:444", true, true);
        assert_eq!(
            verdict(&vp9, Some(&id("libvpx-vp9-444", false, "vp9", true))),
            Verdict::Fallback
        );
        assert_eq!(
            verdict(&vp9, Some(&id("vp9_qsv", true, "vp9", true))),
            Verdict::Ok
        );
        // 4:4:4 asked, 4:2:0 produced (HEVC 4:4:4 is NVENC-only), and a codec swap.
        let hevc444 = cell_plan("dc:h265:444", true, true);
        assert_eq!(
            verdict(&hevc444, Some(&id("hevc_qsv", true, "h265", false))),
            Verdict::Demoted
        );
        assert_eq!(
            verdict(&rtp, Some(&id("mf-h265", true, "h265", false))),
            Verdict::Demoted
        );
        // Nothing opened.
        assert_eq!(verdict(&rtp, None), Verdict::None);
        assert_eq!(
            verdict(&rtp, Some(&id("noop", false, "h264", false))),
            Verdict::None
        );
    }

    #[test]
    fn cells_parse_and_what_no_session_opens_is_refused() {
        assert_eq!(
            CellSpec::parse("rtp:h264"),
            Ok(CellSpec::new(CellPath::Rtp, Codec::H264, false))
        );
        assert_eq!(
            CellSpec::parse(" DC:HEVC:444 "),
            Ok(CellSpec::new(CellPath::Dc, Codec::H265, true))
        );
        assert_eq!(
            CellSpec::parse("dc:vp9:420"),
            Ok(CellSpec::new(CellPath::Dc, Codec::Vp9, false))
        );
        for bad in [
            "rtp:vp9",
            "rtp:h264:444",
            "dc:av1:444",
            "srt:h264",
            "dc:h266",
            "dc:h264:422",
            "dc:h264:444:x",
            "",
        ] {
            assert!(CellSpec::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn sizes_and_bitrates_parse() {
        assert_eq!(parse_size("1280x720"), Ok((1280, 720)));
        assert_eq!(parse_size(" 1920X1080 "), Ok((1920, 1080)));
        for bad in ["1281x720", "1280x721", "1280", "x720", "32x32", "9000x720"] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
        assert_eq!(Bitrate::parse("auto"), Ok(Bitrate::Auto));
        assert_eq!(Bitrate::parse("3000"), Ok(Bitrate::Bps(3_000_000)));
        assert_eq!(Bitrate::parse("3000k"), Ok(Bitrate::Bps(3_000_000)));
        assert_eq!(Bitrate::parse("3M"), Ok(Bitrate::Bps(3_000_000)));
        assert_eq!(Bitrate::parse("1.5m"), Ok(Bitrate::Bps(1_500_000)));
        for bad in ["", "fast", "50", "200M", "-3M", "NaN"] {
            assert!(Bitrate::parse(bad).is_err(), "{bad:?}");
        }
        // `auto` is what a session opens at for the size.
        assert_eq!(
            Bitrate::Auto.resolve(1280, 720, 30),
            crate::encode::initial_bitrate_for_fps(1280, 720, 30)
        );
    }

    /// The quick profile is every cell a session on the build can open, at one
    /// size, one rate and one motion.
    #[test]
    fn the_quick_profile_follows_the_build() {
        let hw = EncoderPreference::Hardware;
        let p = plan(&args(), hw, None, ALL).unwrap();
        assert_eq!(
            cells(&p),
            [
                "rtp:h264",
                "rtp:h265",
                "rtp:av1",
                "dc:h264",
                "dc:h265",
                "dc:av1",
                "dc:vp9:444"
            ]
        );
        assert_eq!(
            cells(&plan(&args(), hw, None, RELEASE_LINUX).unwrap()),
            [
                "rtp:h264",
                "rtp:h265",
                "dc:h264",
                "dc:h265",
                "dc:av1",
                "dc:vp9:444"
            ]
        );
        let sw = plan(&args(), hw, None, SOFTWARE).unwrap();
        assert_eq!(cells(&sw), ["rtp:h264", "dc:vp9:444"]);
        let first = &sw.plans[0];
        assert_eq!((first.width, first.height, first.fps), (1280, 720, 30));
        assert_eq!(first.motion, Scene::Drag { px_per_s: 1000 });
        assert_eq!(
            first.target_bps,
            crate::encode::initial_bitrate_for_fps(1280, 720, 30)
        );
        // Off Windows the RTP track has no hardware H.264: `sw-only`, not `fallback`.
        assert!(first.requested_hw && !first.hardware_on_path);
        // The VP9 DataChannel asks for hardware, which this build lacks.
        assert!(sw.plans[1].requested_hw && !sw.plans[1].hardware_on_path);
        assert_eq!(sw.budget, Duration::from_secs(25));
        assert!(!sw.paced);
        // A build with no encoder at all has nothing to bench.
        let none = BuildCaps {
            mf: false,
            openh264: false,
            ffmpeg: false,
            libvpx: false,
        };
        assert!(plan(&args(), hw, None, none).is_err());
    }

    #[test]
    fn the_full_matrix_and_explicit_axes() {
        let mut a = args();
        a.bench_full = true;
        let p = plan(&a, EncoderPreference::Hardware, None, ALL).unwrap();
        // 10 cells × 3 sizes × 2 rates × 3 motions.
        assert_eq!(p.plans.len(), 10 * 3 * 2 * 3);
        assert_eq!(p.budget, Duration::from_secs(280));

        let mut a = args();
        a.bench_cells = vec!["rtp:av1".into(), "dc:vp9".into()];
        a.bench_sizes = vec!["640x360".into()];
        a.bench_bitrates = vec!["2M".into(), "auto".into()];
        a.bench_motions = vec!["static".into(), "drag:300".into()];
        a.bench_budget_secs = Some(5);
        a.bench_paced = true;
        // Named cells run even where the build cannot open them: the row says `none`.
        let p = plan(&a, EncoderPreference::Software, None, SOFTWARE).unwrap();
        assert_eq!(p.plans.len(), 2 * 2 * 2);
        assert_eq!(p.budget, Duration::from_secs(5));
        assert!(p.paced);
        let r = &p.plans[0];
        assert_eq!(cells(&p)[0], "rtp:av1");
        assert_eq!((r.width, r.height, r.target_bps), (640, 360, 2_000_000));
        assert_eq!(r.motion, Scene::Static);
        assert_eq!(p.plans[1].motion, Scene::Drag { px_per_s: 300 });
        // `--encoder software` asks the RTP track for software; the DataChannel
        // cascades ask for hardware whatever the preference.
        assert!(!r.requested_hw);
        assert!(p.plans[4].requested_hw);

        for (field, bad) in [
            ("cells", "rtp:vp9"),
            ("sizes", "1281x720"),
            ("bitrates", "fast"),
            ("motions", "walk"),
        ] {
            let mut a = args();
            match field {
                "cells" => a.bench_cells = vec![bad.into()],
                "sizes" => a.bench_sizes = vec![bad.into()],
                "bitrates" => a.bench_bitrates = vec![bad.into()],
                _ => a.bench_motions = vec![bad.into()],
            }
            assert!(
                plan(&a, EncoderPreference::Hardware, None, ALL).is_err(),
                "{field} {bad}"
            );
        }
    }

    #[test]
    fn a_named_encoder_is_one_cell_of_its_own_codec() {
        let p = plan(&args(), EncoderPreference::Auto, Some("hevc_vaapi"), ALL).unwrap();
        assert_eq!(p.plans.len(), 1);
        let c = &p.plans[0];
        assert_eq!(c.cell, CellSpec::new(CellPath::Named, Codec::H265, false));
        assert_eq!(c.name.as_deref(), Some("hevc_vaapi"));
        assert!(c.requested_hw && c.hardware_on_path);
        assert!(plan(&args(), EncoderPreference::Auto, Some("x264"), ALL).is_err());
    }

    #[test]
    fn the_header_is_the_contract() {
        assert_eq!(
            header_line(),
            "L1 path   codec  chroma  size       motion     target_kbps  opened             hw   \
             got       verdict   open_ms   frames  enc_p50_ms  enc_p95_ms  enc_max_ms  out_kbps  \
             rate   idr_n  idr_p50_B  idr_max_B  delta_p50_B  delta_p95_B  delta_max_B"
        );
        let header = header_line();
        let fields: Vec<&str> = header.split_whitespace().skip(1).collect();
        assert_eq!(fields, COLUMNS);
    }

    fn result(plan: CellPlan) -> CellResult {
        CellResult {
            plan,
            opened: None,
            verdict: Verdict::None,
            open_ms: None,
            frames: 0,
            enc_p50_ms: None,
            enc_p95_ms: None,
            enc_max_ms: None,
            produced_bps: None,
            idr: SizeDist::default(),
            delta: SizeDist::default(),
        }
    }

    /// Every row splits into exactly the header's columns, filled or not.
    #[test]
    fn a_row_has_every_column_in_order() {
        let mut r = result(cell_plan("dc:vp9:444", true, true));
        r.opened = Some(id("libvpx-vp9-444", false, "vp9", true));
        r.verdict = Verdict::Fallback;
        r.open_ms = Some(12.346);
        r.frames = 90;
        r.enc_p50_ms = Some(4.0);
        r.enc_p95_ms = Some(6.5);
        r.enc_max_ms = Some(11.25);
        r.produced_bps = Some(1_200_000.0);
        r.idr = SizeDist {
            n: 3,
            p50: 41_000,
            p95: 45_000,
            max: 45_000,
        };
        r.delta = SizeDist {
            n: 87,
            p50: 2_300,
            p95: 5_100,
            max: 8_000,
        };
        let row = row_line(&r);
        assert!(row.starts_with(PREFIX));
        let fields: Vec<&str> = row.split_whitespace().skip(1).collect();
        assert_eq!(
            fields,
            [
                "dc",
                "vp9",
                "444",
                "320x240",
                "static",
                "1500",
                "libvpx-vp9-444",
                "sw",
                "vp9:444",
                "fallback",
                "12.35",
                "90",
                "4.00",
                "6.50",
                "11.25",
                "1200",
                "0.80",
                "3",
                "41000",
                "45000",
                "2300",
                "5100",
                "8000"
            ]
        );
        // A cell that never opened still prints every column.
        let empty = row_line(&result(cell_plan("rtp:h264", true, false)));
        let fields: Vec<&str> = empty.split_whitespace().skip(1).collect();
        assert_eq!(
            fields,
            [
                "rtp", "h264", "420", "320x240", "static", "1500", "-", "-", "-", "none", "-", "-",
                "-", "-", "-", "-", "-", "0", "-", "-", "-", "-", "-"
            ]
        );
    }

    /// A fake encoder: a keyframe on its first frame and on every request,
    /// returned as OpenH264 returns one (a 30-byte parameter-set packet, then
    /// the 5 000-byte slice), and a 500-byte delta otherwise.
    struct FakeEncoder {
        name: &'static str,
        hw: bool,
        frames: u32,
        key_next: bool,
        fail_at: Option<u32>,
    }

    #[async_trait::async_trait]
    impl VideoEncoder for FakeEncoder {
        async fn encode(&mut self, _frame: Arc<Frame>) -> anyhow::Result<Vec<EncodedPacket>> {
            if Some(self.frames) == self.fail_at {
                anyhow::bail!("the device went away");
            }
            let key = self.frames == 0 || self.key_next;
            self.key_next = false;
            self.frames += 1;
            let packet = |len: usize| EncodedPacket {
                data: vec![0; len],
                is_keyframe: key,
                duration_us: 33_333,
                qp: None,
            };
            Ok(if key {
                vec![packet(30), packet(5_000)]
            } else {
                vec![packet(500)]
            })
        }
        fn request_keyframe(&mut self) {
            self.key_next = true;
        }
        fn set_bitrate(&mut self, _bps: u32) {}
        fn name(&self) -> &'static str {
            self.name
        }
        fn is_hardware(&self) -> bool {
            self.hw
        }
    }

    /// Opens a software fake on the RTP track, a hardware one on the
    /// DataChannel, fails `dc:av1`, and fails `dc:h265`'s third frame.
    struct FakeOpener;

    impl CellOpener for FakeOpener {
        fn open(&mut self, plan: &CellPlan) -> Result<Opened, String> {
            let (name, hw, fail_at) = match (plan.cell.path, plan.cell.codec) {
                (CellPath::Rtp, _) => ("fake-sw", false, None),
                (CellPath::Dc, Codec::Av1) => return Err("no AV1 encoder here".into()),
                (CellPath::Dc, Codec::H265) => ("fake-hw", true, Some(2)),
                _ => ("fake-hw", true, None),
            };
            Ok(Opened {
                enc: Box::new(FakeEncoder {
                    name,
                    hw,
                    frames: 0,
                    key_next: false,
                    fail_at,
                }),
                codec: plan.cell.codec.label(),
                chroma444: plan.cell.chroma444,
            })
        }
    }

    /// The whole run through the seam, no encoder hardware: the rows, their
    /// verdicts, the IDR count (one opening + one a second) and the summary.
    #[tokio::test]
    async fn a_run_through_the_seam_prints_one_verdict_per_row() {
        let mut a = args();
        a.bench_cells = vec![
            "rtp:h264".into(),
            "dc:h264".into(),
            "dc:av1".into(),
            "dc:h265".into(),
        ];
        a.bench_sizes = vec!["320x240".into()];
        a.bench_bitrates = vec!["1500".into()];
        let p = plan(&a, EncoderPreference::Hardware, None, ALL).unwrap();
        let mut out = Vec::new();
        let results = run_plan(&p, &mut FakeOpener, &mut out).await.unwrap();
        let verdicts: Vec<Verdict> = results.iter().map(|r| r.verdict).collect();
        assert_eq!(
            verdicts,
            [
                Verdict::Fallback,
                Verdict::Ok,
                Verdict::None,
                Verdict::Error
            ]
        );
        let rtp = &results[0];
        assert_eq!(rtp.opened.map(|o| o.name), Some("fake-sw"));
        assert_eq!(rtp.frames, 90);
        // 90 frames at 30 fps: the opening keyframe and one at frames 30 and
        // 60, each counted once, at the size of both its packets.
        assert_eq!(rtp.idr.n, 3);
        assert_eq!((rtp.idr.p50, rtp.idr.max), (5_030, 5_030));
        assert_eq!((rtp.delta.n, rtp.delta.p50), (87, 500));
        // 3 × 5030 + 87 × 500 bytes over 3 s.
        let bps = rtp.produced_bps.unwrap();
        assert!((bps - 156_240.0).abs() < 1e-6, "{bps}");
        // The failing encode keeps the frames before it.
        assert_eq!(results[3].frames, 2);

        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], header_line());
        assert!(lines[1].contains("fake-sw") && lines[1].contains("fallback"));
        assert!(lines[3].contains("none"));
        assert!(lines.iter().all(|l| l.starts_with(PREFIX)));
        assert!(
            text.contains(
                "# done cells=4 ok=1 fallback=1 sw-only=0 demoted=0 none=1 error=1 skipped=0"
            ),
            "{text}"
        );
    }

    /// A spent budget starts no cell: every row says `skipped`.
    #[tokio::test]
    async fn past_the_budget_a_cell_is_skipped_not_started() {
        let mut p = plan(&args(), EncoderPreference::Hardware, None, SOFTWARE).unwrap();
        p.budget = Duration::ZERO;
        let mut out = Vec::new();
        let results = run_plan(&p, &mut FakeOpener, &mut out).await.unwrap();
        assert!(results.iter().all(|r| r.verdict == Verdict::Skipped));
        assert!(results.iter().all(|r| r.open_ms.is_none()));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("skipped=2"), "{text}");
    }

    #[test]
    fn only_bench_picks_the_bench() {
        assert_eq!(smoke_mode(false, false), SmokeMode::Smoke);
        assert_eq!(smoke_mode(true, false), SmokeMode::Sweep);
        assert_eq!(smoke_mode(false, true), SmokeMode::Bench);
    }

    /// The preamble's `clock=` spellings, which a fleet run reads beside its
    /// paced rows.
    #[test]
    fn the_clock_says_what_the_bench_thread_got() {
        use crate::encode::clock::Outcome;
        assert_eq!(clock_label(Outcome::Clamped), "clamped");
        assert_eq!(clock_label(Outcome::Off), "off");
        assert_eq!(clock_label(Outcome::Refused(22)), "refused:22");
        assert_eq!(clock_label(Outcome::NotLinux), "n/a");
    }
}
