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
                codec: a.codec,
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
    #[tokio::test]
    async fn hevc_answered_by_hevc_is_no_demotion() {
        let mut o = Scripted::opens("mf-hevc", true, "h265");
        let (r, _) = smoke(WINDOWS_MSI, req(Hardware, "hevc"), &mut o).await;
        assert_eq!(r.expect("passes").verdict, Verdict::Ok);
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
