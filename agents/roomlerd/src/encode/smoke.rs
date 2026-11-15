// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! `roomlerd encoder-smoke` (#3652): one cell, opened the way a session opens
//! it, ten solid-colour frames encoded, and the run judged by the FR-16 L1
//! bench's [`bench::verdict`]: what OPENED against what was ASKED. Only `ok`
//! passes, so hardware answered by software, a codec answered by another
//! codec, or nothing opening fails, and the failure says what was tried on
//! this OS and build.
//!
//! The opener is the bench's [`CellOpener`] seam, so the judgement is tested
//! with a fake opener and no encoder hardware.

use std::fmt;
use std::io::Write;
use std::sync::Arc;

use anyhow::Context as _;

use super::bench::{
    self, Backend, BuildCaps, CellOpener, CellPath, CellPlan, CellSpec, Codec, OpenedId, Reach,
    Verdict,
};
use super::{EncoderPreference, VideoEncoder};
use crate::capture::synthetic_scene::Scene;
use crate::capture::{Damage, Frame, PixelFormat};

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
                self.pref.wire(),
                self.codec.to_ascii_lowercase()
            ),
        }
    }
}

/// `rtp:h264`, `named:h265`: a cell as the bench's rows name it (the smoke
/// opens 4:2:0 only).
fn cell_label(cell: CellSpec) -> String {
    format!("{}:{}", cell.path.label(), cell.codec.label())
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

/// The cell the request asks for on this build, or why no build serves it.
pub fn plan(
    req: &Request<'_>,
    width: u32,
    height: u32,
    build: BuildCaps,
) -> Result<CellPlan, Refused> {
    let (cell, requested_hw, hardware_on_path) = match req.name {
        Some(name) => {
            let named =
                bench::named_encoder(name).ok_or_else(|| Refused::Name(name.to_string()))?;
            let cell = CellSpec::new(CellPath::Named, named.codec, false);
            if named.media_foundation {
                // A named Media Foundation encoder asks for Media Foundation,
                // either MFT (D2 A): Microsoft's software H.264 MFT is all a
                // GPU-less runner has.
                (cell, false, build.mf)
            } else {
                (cell, true, build.hardware_on_path(cell))
            }
        }
        None => {
            let lower = req.codec.to_ascii_lowercase();
            let codec = match lower.as_str() {
                "h264" => Codec::H264,
                "h265" | "hevc" => Codec::H265,
                "av1" => Codec::Av1,
                _ => return Err(Refused::Codec(lower)),
            };
            // D3 A: no build has a software HEVC or AV1 encoder.
            if req.pref == EncoderPreference::Software && codec != Codec::H264 {
                return Err(Refused::NoSoftwareEncoder(codec.label().to_string()));
            }
            let rtp = CellSpec::new(CellPath::Rtp, codec, false);
            // D1 A: hardware asked for where the RTP track has none, and
            // FFmpeg's cascade is here: the DataChannel's cell answers.
            let cell = if req.pref == EncoderPreference::Hardware
                && !build.hardware_on_path(rtp)
                && build.ffmpeg
            {
                CellSpec::new(CellPath::Dc, codec, false)
            } else {
                rtp
            };
            (
                cell,
                bench::requests_hardware(cell.path, req.pref),
                build.hardware_on_path(cell),
            )
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
        requested_hw,
        hardware_on_path,
    })
}

/// The backends `plan`'s open can reach, in the order its opener tries them.
fn backends_for(plan: &CellPlan, pref: EncoderPreference, build: BuildCaps) -> Vec<Backend> {
    use Backend::{Ffmpeg, MediaFoundation as Mf, Openh264};
    match (plan.cell.path, plan.cell.codec) {
        // [`plan`] asks hardware of a named FFmpeg encoder, and not of a
        // named Media Foundation one.
        (CellPath::Named, _) if !plan.requested_hw => vec![Mf],
        (CellPath::Named, _) => vec![Ffmpeg],
        // D1 A: the RTP track's Media Foundation is not in this build, so the
        // DataChannel's FFmpeg cascade answered for hardware.
        (CellPath::Dc, _) => vec![Mf, Ffmpeg],
        (CellPath::Rtp, Codec::H264) => match pref {
            EncoderPreference::Software => vec![Openh264],
            EncoderPreference::Hardware if !build.mf => vec![Mf, Ffmpeg, Openh264],
            _ => vec![Mf, Openh264],
        },
        (CellPath::Rtp, Codec::H265) => vec![Ffmpeg, Mf],
        (CellPath::Rtp, _) => vec![Mf],
    }
}

/// The candidates of `plan`'s FFmpeg cascade that the cell denylist
/// (`encoder_cells_deny`) keeps out: the cascade skips them without a word.
/// Media Foundation's refusal of a denied cell comes back as its open's
/// failure, and `--name` passes the list by design.
fn denied_for(plan: &CellPlan, backends: &[(Backend, Reach)]) -> Vec<String> {
    let ffmpeg_tried = backends.contains(&(Backend::Ffmpeg, Reach::InBuild));
    if plan.cell.path == CellPath::Named || !ffmpeg_tried {
        return Vec::new();
    }
    #[cfg(feature = "ffmpeg-encoder")]
    {
        use roomler_ai_remote_control::models::{ChromaFormat, VideoCodec};
        let codec = match plan.cell.codec {
            Codec::H264 => VideoCodec::H264,
            Codec::H265 => VideoCodec::Hevc,
            Codec::Av1 => VideoCodec::Av1,
            Codec::Vp9 => VideoCodec::Vp9,
        };
        let deny = super::cells::denied_cells();
        super::ffmpeg::FfmpegEncoder::cascade_names(codec)
            .iter()
            .filter(|name| {
                super::ffmpeg::FfmpegEncoder::carried(name)
                    && super::cells::cell_denied(&deny, name, ChromaFormat::Yuv420)
            })
            .map(|name| name.to_string())
            .collect()
    }
    #[cfg(not(feature = "ffmpeg-encoder"))]
    {
        Vec::new()
    }
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
    /// The flags that asked ([`Request::describe`]).
    pub request: String,
    /// What opened, when something did (a `noop` included).
    pub opened: Option<OpenedId>,
    pub tried: Tried,
    /// The opener's answer when nothing opened, or how the encode failed.
    pub why: Option<String>,
}

impl Failed {
    /// The first line a failing smoke prints: the verdict, and what opened.
    /// Like the PASSED line, its fields are only ever appended.
    pub fn headline(&self) -> String {
        let (backend, hw) = self.opened.map_or(("none", false), |o| (o.name, o.hw));
        format!(
            "encoder smoke FAILED: verdict={} cell={} request={:?} backend={backend} hw={hw}",
            self.verdict.label(),
            cell_label(self.cell),
            self.request,
        )
    }

    /// The verdict, in words.
    fn reason(&self) -> String {
        let answered = self.opened.map_or("nothing", |o| o.name);
        match (self.verdict, self.opened) {
            (Verdict::Fallback, _) => format!(
                "hardware was asked for, and software answered ({answered}), although this \
                 build has a hardware encoder on that path"
            ),
            (Verdict::SwOnly, _) => format!(
                "hardware was asked for, and this build has no hardware encoder on that path, \
                 so software answered ({answered})"
            ),
            (Verdict::Demoted, Some(o)) => format!(
                "{} was asked for, and {} answered with {}{}",
                self.cell.codec.label(),
                o.name,
                o.codec,
                if o.chroma444 { " 4:4:4" } else { "" }
            ),
            (Verdict::None, _) => "nothing opened".into(),
            (Verdict::Error, _) => format!("{answered} opened, and its encode failed"),
            _ => "the run did not complete".into(),
        }
    }
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.headline(), self.reason())?;
        if let Some(why) = &self.why {
            write!(f, ": {why}")?;
        }
        write!(f, "; tried: {}", self.tried)?;
        if matches!(self.verdict, Verdict::Fallback | Verdict::SwOnly) {
            write!(
                f,
                ". With no flags, encoder-smoke asks for hardware; \
                 the software smoke is `encoder-smoke --encoder software`"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for Failed {}

/// One open, judged by [`bench::verdict`]: what opened, and what was tried on
/// the way. The smoke and the FR-62 sweep both open through [`open`].
pub struct Judged {
    /// The encoder, when one opened (a `NoopEncoder` included).
    pub enc: Option<Box<dyn VideoEncoder>>,
    pub opened: Option<OpenedId>,
    pub verdict: Verdict,
    pub tried: Tried,
    /// The opener's answer when nothing opened.
    pub why: Option<String>,
}

impl Judged {
    /// The encoder, when what opened is what was asked for; else why not, as
    /// a [`Failed`] inside the error. An encoder that did not pass is dropped
    /// unused, and it is never an FFmpeg one: an FFmpeg encoder always
    /// produces what its cell asks for.
    pub fn into_ok(
        self,
        plan: &CellPlan,
        req: &Request<'_>,
    ) -> anyhow::Result<(Box<dyn VideoEncoder>, OpenedId)> {
        match (self.verdict, self.enc, self.opened) {
            (Verdict::Ok, Some(enc), Some(id)) => Ok((enc, id)),
            (verdict, _, opened) => Err(Failed {
                verdict,
                cell: plan.cell,
                request: req.describe(),
                opened,
                tried: self.tried,
                why: self.why,
            }
            .into()),
        }
    }
}

/// Open `plan`'s cell through `opener`, recording each candidate that failed
/// (`media_plan::OpenTrail`, as a DataChannel pump does), and judge what
/// opened against what was asked.
pub fn open(
    plan: &CellPlan,
    pref: EncoderPreference,
    build: BuildCaps,
    opener: &mut (dyn CellOpener + Send),
) -> Judged {
    let trail = crate::media_plan::OpenTrail::default();
    let opened = {
        let _recording = trail.recording();
        opener.open(plan)
    };
    let backends: Vec<(Backend, Reach)> = backends_for(plan, pref, build)
        .into_iter()
        .map(|b| (b, build.reach(b)))
        .collect();
    let tried = Tried {
        denied: denied_for(plan, &backends),
        backends,
        failed: trail.take(),
    };
    match opened {
        Err(why) => Judged {
            enc: None,
            opened: None,
            verdict: Verdict::None,
            tried,
            why: Some(why),
        },
        Ok(o) => {
            let id = OpenedId {
                name: o.enc.name(),
                hw: o.enc.is_hardware(),
                codec: o.codec,
                chroma444: o.chroma444,
            };
            Judged {
                enc: Some(o.enc),
                opened: Some(id),
                verdict: bench::verdict(plan, Some(&id)),
                tried,
                why: None,
            }
        }
    }
}

/// Where `--dump` writes the raw bitstream.
pub struct Dump<'a> {
    pub path: &'a str,
    pub file: &'a mut (dyn Write + Send),
}

/// Print a failure's two lines and hand it back.
fn fail(out: &mut (dyn Write + Send), failed: Failed) -> anyhow::Result<Passed> {
    writeln!(out, "{}", failed.headline())?;
    writeln!(out, "encoder smoke tried: {}", failed.tried)?;
    Err(failed.into())
}

/// Open the planned cell, encode [`FRAMES`] frames, and judge the run by the
/// bench's verdict: only `ok` passes. An encoder that opened and then fails
/// to encode, or produces no keyframe, is the `error` verdict. A failure
/// prints its verdict and what was tried, and comes back as a [`Failed`], so
/// `main` exits 1.
pub async fn run(
    plan: &CellPlan,
    req: &Request<'_>,
    build: BuildCaps,
    opener: &mut (dyn CellOpener + Send),
    out: &mut (dyn Write + Send),
    mut dump: Option<Dump<'_>>,
) -> anyhow::Result<Passed> {
    let (w, h) = (plan.width, plan.height);
    tracing::info!(
        width = w,
        height = h,
        pref = ?req.pref,
        request = %req.describe(),
        cell = %cell_label(plan.cell),
        "encoder smoke: opening encoder"
    );
    let Judged {
        enc,
        opened,
        mut verdict,
        tried,
        why: open_why,
    } = open(plan, req.pref, build, opener);
    let (mut enc, id) = match (enc, opened) {
        (Some(enc), Some(id)) if verdict != Verdict::None => (enc, id),
        (_, opened) => {
            return fail(
                out,
                Failed {
                    verdict: Verdict::None,
                    cell: plan.cell,
                    request: req.describe(),
                    opened,
                    tried,
                    why: open_why,
                },
            );
        }
    };
    tracing::info!(
        backend = id.name,
        hw = id.hw,
        codec = id.codec,
        verdict = verdict.label(),
        "encoder smoke: backend selected"
    );

    let mut keyframes = 0usize;
    let mut total_bytes = 0usize;
    let mut encode_failed = None;
    for i in 0..FRAMES {
        let frame = solid_frame(w, h, i);
        if i == 5 {
            enc.request_keyframe();
        }
        let packets = match enc.encode(frame).await {
            Ok(packets) => packets,
            Err(e) => {
                encode_failed = Some(format!("frame {i}: {e:#}"));
                break;
            }
        };
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
    tracing::info!(
        backend = id.name,
        keyframes,
        total_bytes,
        "encoder smoke: done"
    );

    let why = match encode_failed {
        Some(e) => Some(format!("the encode failed at {e}")),
        None if keyframes == 0 => Some(format!("no keyframe in {FRAMES} frames")),
        None => None,
    };
    if why.is_some() {
        verdict = Verdict::Error;
    }
    if verdict != Verdict::Ok {
        return fail(
            out,
            Failed {
                verdict,
                cell: plan.cell,
                request: req.describe(),
                opened: Some(id),
                tried,
                why,
            },
        );
    }
    writeln!(
        out,
        "encoder smoke PASSED: backend={} keyframes={keyframes} total_bytes={total_bytes} \
         verdict=ok cell={} hw={}",
        id.name,
        cell_label(plan.cell),
        id.hw
    )?;
    Ok(Passed {
        backend: id.name,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::bench::Opened;
    use crate::encode::{EncodedPacket, VideoEncoder};

    /// The Linux release `.deb`: FFmpeg's cascades, OpenH264, libvpx.
    const LINUX_FFMPEG: BuildCaps = BuildCaps {
        mf: false,
        openh264: true,
        ffmpeg: true,
        libvpx: true,
        windows: false,
        ffmpeg_compiled: true,
    };
    /// `--features openh264-encoder` alone (the AC1 build).
    const LINUX_SW: BuildCaps = BuildCaps {
        mf: false,
        openh264: true,
        ffmpeg: false,
        libvpx: false,
        windows: false,
        ffmpeg_compiled: false,
    };
    /// The `.deb` with `ROOMLERD_USE_FFMPEG=0`.
    const LINUX_FFMPEG_OFF: BuildCaps = BuildCaps {
        ffmpeg: false,
        ..LINUX_FFMPEG
    };
    /// The Windows MSI: everything.
    const WINDOWS_MSI: BuildCaps = BuildCaps {
        mf: true,
        openh264: true,
        ffmpeg: true,
        libvpx: true,
        windows: true,
        ffmpeg_compiled: true,
    };

    /// A keyframe on its first frame and on request, unless `never_key`;
    /// an encode error at frame `fail_at`.
    struct FakeEncoder {
        name: &'static str,
        hw: bool,
        frames: u32,
        key_next: bool,
        fail_at: Option<u32>,
        never_key: bool,
    }

    #[async_trait::async_trait]
    impl VideoEncoder for FakeEncoder {
        async fn encode(&mut self, _frame: Arc<Frame>) -> anyhow::Result<Vec<EncodedPacket>> {
            if Some(self.frames) == self.fail_at {
                anyhow::bail!("the device went away");
            }
            let key = !self.never_key && (self.frames == 0 || self.key_next);
            self.key_next = false;
            self.frames += 1;
            Ok(vec![EncodedPacket {
                data: vec![0; if key { 5_000 } else { 500 }],
                is_keyframe: key,
                duration_us: 33_333,
                qp: None,
            }])
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

    /// What the scripted opener answers with.
    #[derive(Clone)]
    struct Answer {
        name: &'static str,
        hw: bool,
        codec: &'static str,
        fail_at: Option<u32>,
        never_key: bool,
    }

    /// Answers every open with `answer` (or `Err(refuse)`), records the plan
    /// it got, and notes `cascade_failures` as a cascade would.
    struct Scripted {
        answer: Option<Answer>,
        refuse: String,
        cascade_failures: Vec<(&'static str, &'static str)>,
        saw: Vec<CellPlan>,
    }

    impl Scripted {
        fn opens(name: &'static str, hw: bool, codec: &'static str) -> Self {
            Self {
                answer: Some(Answer {
                    name,
                    hw,
                    codec,
                    fail_at: None,
                    never_key: false,
                }),
                refuse: String::new(),
                cascade_failures: Vec::new(),
                saw: Vec::new(),
            }
        }
        fn refuses(why: &str) -> Self {
            Self {
                answer: None,
                refuse: why.into(),
                cascade_failures: Vec::new(),
                saw: Vec::new(),
            }
        }
    }

    impl CellOpener for Scripted {
        fn open(&mut self, plan: &CellPlan) -> Result<Opened, String> {
            self.saw.push(plan.clone());
            for (name, why) in &self.cascade_failures {
                crate::media_plan::note_candidate_failed(name, why);
            }
            let Some(a) = self.answer.clone() else {
                return Err(self.refuse.clone());
            };
            Ok(Opened {
                enc: Box::new(FakeEncoder {
                    name: a.name,
                    hw: a.hw,
                    frames: 0,
                    key_next: false,
                    fail_at: a.fail_at,
                    never_key: a.never_key,
                }),
                // `a.codec` is spelt as its constructor returns it, and read
                // as `SessionOpener` reads it.
                codec: bench::codec_label(a.codec),
                chroma444: false,
            })
        }
    }

    fn req<'a>(pref: EncoderPreference, codec: &'a str) -> Request<'a> {
        Request {
            pref,
            codec,
            name: None,
        }
    }

    /// The whole smoke through the seam: the result and what it printed.
    async fn smoke(
        build: BuildCaps,
        req: Request<'_>,
        opener: &mut Scripted,
    ) -> (anyhow::Result<Passed>, String) {
        let plan = plan(&req, WIDTH, HEIGHT, build).expect("the request plans");
        let mut out = Vec::new();
        let r = run(&plan, &req, build, opener, &mut out, None).await;
        (r, String::from_utf8(out).expect("utf-8"))
    }

    /// `main` maps any `Err` to exit 1.
    fn exit_code(r: &anyhow::Result<Passed>) -> i32 {
        i32::from(r.is_err())
    }

    fn failed(r: &anyhow::Result<Passed>) -> &Failed {
        r.as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<Failed>())
            .expect("the smoke failed with a Failed")
    }

    fn cell_of(build: BuildCaps, req: Request<'_>) -> String {
        let p = plan(&req, WIDTH, HEIGHT, build).expect("plans");
        format!("{}:{}", p.cell.path.label(), p.cell.codec.label())
    }

    use EncoderPreference::{Auto, Hardware, Software};

    // RED on the seam (D1 A: the DataChannel's FFmpeg cascade answers a
    // hardware H.264 request where the RTP track has no hardware encoder).
    #[test]
    fn the_cell_per_request_and_build() {
        assert_eq!(cell_of(LINUX_FFMPEG, req(Hardware, "h264")), "dc:h264");
        assert_eq!(cell_of(LINUX_FFMPEG_OFF, req(Hardware, "h264")), "rtp:h264");
        assert_eq!(cell_of(LINUX_SW, req(Hardware, "h264")), "rtp:h264");
        assert_eq!(cell_of(WINDOWS_MSI, req(Hardware, "h264")), "rtp:h264");
        assert_eq!(cell_of(LINUX_FFMPEG, req(Software, "h264")), "rtp:h264");
        assert_eq!(cell_of(LINUX_FFMPEG, req(Auto, "h264")), "rtp:h264");
        assert_eq!(cell_of(LINUX_FFMPEG, req(Hardware, "hevc")), "rtp:h265");
        assert_eq!(cell_of(LINUX_FFMPEG, req(Hardware, "H265")), "rtp:h265");
        assert_eq!(cell_of(WINDOWS_MSI, req(Auto, "av1")), "rtp:av1");
        let p = plan(&req(Hardware, "h264"), WIDTH, HEIGHT, LINUX_SW).expect("plans");
        assert!(p.requested_hw && !p.hardware_on_path);
        let p = plan(&req(Software, "h264"), WIDTH, HEIGHT, WINDOWS_MSI).expect("plans");
        assert!(!p.requested_hw);
    }

    // RED on the seam: it refuses nothing.
    #[test]
    fn a_request_no_build_can_serve_is_refused_before_anything_opens() {
        let refused = |build, r: Request<'_>| plan(&r, WIDTH, HEIGHT, build).expect_err("refused");
        assert!(matches!(
            refused(WINDOWS_MSI, req(Software, "hevc")),
            Refused::NoSoftwareEncoder(_)
        ));
        assert!(matches!(
            refused(WINDOWS_MSI, req(Software, "av1")),
            Refused::NoSoftwareEncoder(_)
        ));
        assert_eq!(
            refused(LINUX_FFMPEG, req(Hardware, "vp9")),
            Refused::Codec("vp9".into())
        );
        assert!(matches!(
            refused(LINUX_FFMPEG, req(Hardware, "mpeg2")),
            Refused::Codec(_)
        ));
        let bad_name = Request {
            pref: Hardware,
            codec: "h264",
            name: Some("not_an_encoder"),
        };
        assert!(matches!(refused(LINUX_FFMPEG, bad_name), Refused::Name(_)));
    }

    // RED on the seam (guard 2: the release lane's OLD request now fails on a
    // runner whose only H.264 MFT is Microsoft's software one).
    #[tokio::test]
    async fn hardware_h264_answered_by_the_software_mft_is_a_fallback() {
        let mut o = Scripted::opens("mf-h264", false, "h264");
        let (r, out) = smoke(WINDOWS_MSI, req(Hardware, "h264"), &mut o).await;
        assert_eq!(failed(&r).verdict, Verdict::Fallback, "{out}");
        assert_eq!(exit_code(&r), 1);
        assert!(!out.contains("PASSED"), "{out}");
    }

    // RED on the seam.
    #[tokio::test]
    async fn hardware_h264_answered_by_openh264_on_windows_is_a_fallback() {
        let mut o = Scripted::opens("openh264", false, "h264");
        let (r, _) = smoke(WINDOWS_MSI, req(Hardware, "h264"), &mut o).await;
        assert_eq!(failed(&r).verdict, Verdict::Fallback);
    }

    // RED on the seam: it passes, and says nothing of what it tried.
    #[tokio::test]
    async fn hardware_h264_on_a_software_only_build_is_sw_only_and_says_what_it_tried() {
        let mut o = Scripted::opens("openh264", false, "h264");
        let (r, _) = smoke(LINUX_SW, req(Hardware, "h264"), &mut o).await;
        let f = failed(&r);
        assert_eq!(f.verdict, Verdict::SwOnly);
        assert_eq!(
            f.tried.backends,
            [
                (Backend::MediaFoundation, Reach::WindowsOnly),
                (Backend::Ffmpeg, Reach::NotCompiled),
                (Backend::Openh264, Reach::InBuild),
            ]
        );
        let text = format!("{f}");
        assert!(text.contains("verdict=sw-only"), "{text}");
        assert!(text.contains("media-foundation: Windows only"), "{text}");
        assert!(
            text.contains("ffmpeg: not compiled into this build"),
            "{text}"
        );
        assert!(text.contains("--encoder software"), "the advice: {text}");
    }

    // RED on the seam.
    #[tokio::test]
    async fn ffmpeg_switched_off_is_named_as_switched_off() {
        let mut o = Scripted::opens("openh264", false, "h264");
        let (r, _) = smoke(LINUX_FFMPEG_OFF, req(Hardware, "h264"), &mut o).await;
        let f = failed(&r);
        assert_eq!(f.verdict, Verdict::SwOnly);
        assert!(
            f.tried
                .backends
                .contains(&(Backend::Ffmpeg, Reach::SwitchedOff)),
            "{:?}",
            f.tried
        );
    }

    // RED on the seam: the PASSED line gains its appended fields.
    #[tokio::test]
    async fn hardware_h264_on_nvenc_passes_and_says_what_passed() {
        let mut o = Scripted::opens("h264_nvenc", true, "h264");
        let (r, out) = smoke(LINUX_FFMPEG, req(Hardware, "h264"), &mut o).await;
        let p = r.expect("passes");
        assert_eq!(p.verdict, Verdict::Ok);
        assert!(
            out.starts_with("encoder smoke PASSED: backend=h264_nvenc keyframes="),
            "the old fields stay first: {out}"
        );
        assert!(out.contains(" verdict=ok cell=dc:h264 hw=true"), "{out}");
        assert_eq!(
            o.saw[0].cell,
            CellSpec::new(CellPath::Dc, Codec::H264, false)
        );
    }

    // RED on the seam: master warned "demoted" for `hevc` answered by HEVC.
    // Each opener answers in its own spelling: the RTP track's
    // `open_for_codec` says `h265`, and `open_named` says `hevc`
    // (`VideoCodec::wire()`), which `bench::codec_label` reads as `h265`.
    #[tokio::test]
    async fn hevc_answered_by_hevc_is_no_demotion() {
        let mut o = Scripted::opens("mf-hevc", true, "h265");
        let (r, _) = smoke(WINDOWS_MSI, req(Hardware, "hevc"), &mut o).await;
        assert_eq!(r.expect("passes").verdict, Verdict::Ok);
        let named = Request {
            pref: Hardware,
            codec: "h264",
            name: Some("hevc_nvenc"),
        };
        let mut o = Scripted::opens("hevc_nvenc", true, "hevc");
        let (r, out) = smoke(LINUX_FFMPEG, named, &mut o).await;
        assert_eq!(r.expect("passes").verdict, Verdict::Ok, "{out}");
        assert!(out.contains(" verdict=ok cell=named:h265 hw=true"), "{out}");
    }

    // RED on the seam: it passed with a warning.
    #[tokio::test]
    async fn av1_answered_by_h264_is_demoted() {
        let mut o = Scripted::opens("mf-h264", true, "h264");
        let (r, _) = smoke(WINDOWS_MSI, req(Auto, "av1"), &mut o).await;
        assert_eq!(failed(&r).verdict, Verdict::Demoted);
        assert_eq!(exit_code(&r), 1);
    }

    // CONTROL: green on both.
    #[tokio::test]
    async fn auto_passes_on_the_software_mft_and_on_openh264() {
        let mut o = Scripted::opens("mf-h264", false, "h264");
        let (r, _) = smoke(WINDOWS_MSI, req(Auto, "h264"), &mut o).await;
        assert_eq!(exit_code(&r), 0);
        let mut o = Scripted::opens("openh264", false, "h264");
        let (r, _) = smoke(LINUX_SW, req(Auto, "h264"), &mut o).await;
        assert_eq!(exit_code(&r), 0);
    }

    // CONTROL: the `.deb` lanes' call.
    #[tokio::test]
    async fn software_h264_passes_on_openh264() {
        let mut o = Scripted::opens("openh264", false, "h264");
        let (r, _) = smoke(LINUX_FFMPEG, req(Software, "h264"), &mut o).await;
        assert_eq!(exit_code(&r), 0);
    }

    // RED on the seam: `tried` is empty.
    #[tokio::test]
    async fn nothing_opening_is_none_with_what_failed() {
        let mut o = Scripted::opens("noop", false, "h265");
        o.cascade_failures = vec![
            ("hevc_nvenc", "no device"),
            ("hevc_vaapi", "no render node"),
        ];
        let (r, _) = smoke(LINUX_FFMPEG, req(Hardware, "hevc"), &mut o).await;
        let f = failed(&r);
        assert_eq!(f.verdict, Verdict::None);
        assert_eq!(
            f.tried.failed,
            [
                ("hevc_nvenc".to_string(), "no device".to_string()),
                ("hevc_vaapi".to_string(), "no render node".to_string()),
            ]
        );
    }

    // RED on the seam: it propagated the raw encode error.
    #[tokio::test]
    async fn an_encode_error_is_the_error_verdict() {
        let mut o = Scripted::opens("h264_nvenc", true, "h264");
        if let Some(a) = o.answer.as_mut() {
            a.fail_at = Some(2);
        }
        let (r, _) = smoke(LINUX_FFMPEG, req(Hardware, "h264"), &mut o).await;
        assert_eq!(failed(&r).verdict, Verdict::Error);
    }

    // CONTROL.
    #[tokio::test]
    async fn no_keyframe_is_the_error_verdict() {
        let mut o = Scripted::opens("h264_nvenc", true, "h264");
        if let Some(a) = o.answer.as_mut() {
            a.never_key = true;
        }
        let (r, _) = smoke(LINUX_FFMPEG, req(Hardware, "h264"), &mut o).await;
        assert_eq!(failed(&r).verdict, Verdict::Error);
    }

    // The FR-62 sweep's only judgement: it hands the encoder back on `ok`
    // alone, and any other verdict comes back as a `Failed` that names it and
    // what opened, as the plain smoke reports it.
    #[test]
    fn into_ok_hands_the_encoder_back_on_ok_alone() {
        let r = req(Hardware, "h264");
        let p = plan(&r, WIDTH, HEIGHT, LINUX_FFMPEG).expect("plans");
        let id = OpenedId {
            name: "h264_nvenc",
            hw: true,
            codec: "h264",
            chroma444: false,
        };
        let judged = |verdict| Judged {
            enc: Some(Box::new(FakeEncoder {
                name: id.name,
                hw: id.hw,
                frames: 0,
                key_next: false,
                fail_at: None,
                never_key: false,
            })),
            opened: Some(id),
            verdict,
            tried: Tried::default(),
            why: None,
        };
        for verdict in [
            Verdict::Ok,
            Verdict::Fallback,
            Verdict::SwOnly,
            Verdict::Demoted,
            Verdict::None,
            Verdict::Error,
            Verdict::Skipped,
        ] {
            match judged(verdict).into_ok(&p, &r) {
                Ok((enc, opened)) => {
                    assert_eq!(verdict, Verdict::Ok, "{} passed", verdict.label());
                    assert_eq!((enc.name(), opened), (id.name, id));
                }
                Err(e) => {
                    let f = e
                        .downcast_ref::<Failed>()
                        .unwrap_or_else(|| panic!("not the smoke's verdict: {e:#}"));
                    assert_ne!(verdict, Verdict::Ok, "ok did not pass: {e:#}");
                    assert_eq!((f.verdict, f.opened, f.cell), (verdict, Some(id), p.cell));
                    assert_eq!(f.request, r.describe());
                }
            }
        }
        // `ok` with no encoder in hand is no pass either.
        let no_encoder = Judged {
            enc: None,
            ..judged(Verdict::Ok)
        };
        assert!(no_encoder.into_ok(&p, &r).is_err());
    }

    // CONTROL on the seam, the NEW lane request (D2 A): either MFT passes.
    #[tokio::test]
    async fn the_lanes_named_mf_request_passes_on_the_software_mft() {
        let lane = Request {
            pref: Hardware,
            codec: "h264",
            name: Some("h264_mf"),
        };
        let mut o = Scripted::opens("mf-h264", false, "h264");
        let (r, _) = smoke(WINDOWS_MSI, lane, &mut o).await;
        assert_eq!(exit_code(&r), 0);
        let mut o = Scripted::refuses("no H.264 MFT on this host");
        let (r, _) = smoke(WINDOWS_MSI, lane, &mut o).await;
        assert_eq!(failed(&r).verdict, Verdict::None);
        assert_eq!(exit_code(&r), 1);
    }

    // CONTROL: each backend's reach, per build.
    #[test]
    fn reach_per_build() {
        assert_eq!(WINDOWS_MSI.reach(Backend::MediaFoundation), Reach::InBuild);
        assert_eq!(
            LINUX_FFMPEG.reach(Backend::MediaFoundation),
            Reach::WindowsOnly
        );
        let windows_no_mf = BuildCaps {
            mf: false,
            ..WINDOWS_MSI
        };
        assert_eq!(
            windows_no_mf.reach(Backend::MediaFoundation),
            Reach::NotCompiled
        );
        assert_eq!(LINUX_FFMPEG_OFF.reach(Backend::Ffmpeg), Reach::SwitchedOff);
        assert_eq!(LINUX_SW.reach(Backend::Ffmpeg), Reach::NotCompiled);
        assert_eq!(LINUX_SW.reach(Backend::Openh264), Reach::InBuild);
    }
}
