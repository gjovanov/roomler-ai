// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-135 R9 — the cell table's gate, and the table read against the code it
//! describes.
//!
//! The gate is the composition baseline's pattern (`crates/tests`): recorded
//! hellos in `tests/fixtures/cell-table.hellos.json`, one per host class, and a
//! test that the agent's own assembly still writes each one, that the two-child
//! production path does too, and that the table derives it byte for byte. The
//! rest reads each column against the code or the build it comes from, and
//! each of FR-135 §5.7's divergences R9 can express against the table.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use roomler_ai_remote_control::models::AgentCaps;

use super::*;
use crate::encode::caps::{assemble, assemble_in_two_children};

const FIXTURES: &str = "tests/fixtures/cell-table.hellos.json";
/// Re-records every fixture's `hello` and `divergences` from the agent's code.
const UPDATE: &str = "CELL_TABLE_UPDATE";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FixtureFile {
    #[serde(rename = "_about")]
    about: Vec<String>,
    fixtures: Vec<Fixture>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Fixture {
    name: String,
    covers: String,
    source: String,
    platform: Platform,
    /// The denylist the probe ran with (the built-in one unless it says so).
    deny: Vec<String>,
    probe: ProbeFindings,
    #[serde(default)]
    hello: ProbedFields,
    #[serde(default)]
    divergences: Vec<Divergence>,
}

impl Fixture {
    fn derived(&self, table: &[CellSpec]) -> ProbedFields {
        derive(table, &proofs(&self.probe), &ProbeKnobs::of(&self.probe))
    }

    fn read_divergences(&self, table: &[CellSpec]) -> Vec<Divergence> {
        let host = Host {
            platform: self.platform,
            deny: &self.deny,
        };
        divergences(
            table,
            &proofs(&self.probe),
            &ProbeKnobs::of(&self.probe),
            &host,
            &assemble(&self.probe),
        )
    }
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURES)
}

fn load() -> FixtureFile {
    let path = fixture_path();
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no fixture file at {} ({e})", path.display()));
    // A `core.autocrlf=true` checkout hands it back with CRLF.
    serde_json::from_str(&raw.replace("\r\n", "\n")).expect("the fixture file parses")
}

fn fixture<'a>(file: &'a FixtureFile, name: &str) -> &'a Fixture {
    file.fixtures
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no fixture {name}"))
}

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("plain data")
}

/// Where `table` derives another hello than a fixture recorded: one line each.
fn gate(table: &[CellSpec], fixtures: &[Fixture]) -> Vec<String> {
    fixtures
        .iter()
        .filter_map(|f| {
            let derived = json(&f.derived(table));
            let recorded = json(&f.hello);
            (derived != recorded)
                .then(|| format!("{}: derived {derived}\n    recorded {recorded}", f.name))
        })
        .collect()
}

fn caps_of(fields: &ProbedFields) -> AgentCaps {
    AgentCaps {
        hw_encoders: fields.hw_encoders.clone(),
        codecs: fields.codecs.clone(),
        transports: fields.transports.clone(),
        vp9_chroma: fields.vp9_chroma.clone(),
        hevc_chroma: fields.hevc_chroma.clone(),
        video_cells: fields.video_cells.clone(),
        ..AgentCaps::default()
    }
}

/// The host classes the gate must hold, whatever else the file carries.
const REQUIRED: &[&str] = &[
    "windows-nvenc-dgpu",
    "windows-qsv-office-desktop",
    "windows-amf-apu",
    "windows-guest-no-gpu",
    "linux-amd-vaapi-vulkan",
    "linux-vp9-only",
    "macos-videotoolbox",
    "headless-no-hardware-probe",
    "signalling-only-build",
];

// ── The gate ─────────────────────────────────────────────────────────────

/// THE gate: for every recorded host, the agent's own assembly writes the
/// recorded hello, the production path's two children and their 4:4:4 merge
/// write it too, and the table derives it, byte for byte. The divergences it
/// reads are the recorded ones.
#[test]
fn the_table_derives_every_recorded_hello_byte_for_byte() {
    let file = load();
    // Floors, so an emptied or truncated file cannot pass by matching nothing.
    for name in REQUIRED {
        assert!(
            file.fixtures.iter().any(|f| f.name == *name),
            "the gate has lost the {name} fixture"
        );
    }
    let cells: usize = file
        .fixtures
        .iter()
        .map(|f| assemble(&f.probe).video_cells.len())
        .sum();
    assert!(
        cells >= 40,
        "only {cells} cells across the fixtures: the probes say nothing"
    );

    if std::env::var_os(UPDATE).is_some() {
        let mut out = file.clone();
        for f in &mut out.fixtures {
            f.hello = assemble(&f.probe);
            f.divergences = f.read_divergences(CELLS);
        }
        let rendered = serde_json::to_string_pretty(&out).expect("plain data") + "\n";
        std::fs::write(fixture_path(), rendered).expect("write the fixtures");
        println!(
            "cell-table fixtures re-recorded at {}",
            fixture_path().display()
        );
        return;
    }

    for f in &file.fixtures {
        let recorded = json(&f.hello);
        assert_eq!(
            json(&assemble(&f.probe)),
            recorded,
            "{}: the agent's assembly writes another hello than the recorded one. If the \
             change is intended, re-record with {UPDATE}=1 and say why in the commit message",
            f.name
        );
        assert_eq!(
            json(&assemble_in_two_children(&f.probe, &f.deny)),
            recorded,
            "{}: the base and 4:4:4 children write another hello than one process does",
            f.name
        );
        assert_eq!(
            f.read_divergences(CELLS),
            f.divergences,
            "{}: the divergences moved. Re-record with {UPDATE}=1 and say which and why",
            f.name
        );
    }
    let failed = gate(CELLS, &file.fixtures);
    assert!(
        failed.is_empty(),
        "the cell table derives another hello than the agent writes:\n{}",
        failed.join("\n")
    );
}

/// The gate can fail: one deliberately changed row is a mismatch on the hosts
/// that carry it, whichever column it changes.
#[test]
fn one_changed_row_fails_the_gate() {
    let file = load();
    assert!(gate(CELLS, &file.fixtures).is_empty());
    let changed = |id: &str, change: fn(&mut CellSpec)| -> Vec<String> {
        let mut table = CELLS.to_vec();
        let row = table
            .iter_mut()
            .find(|r| r.id.wire() == id)
            .unwrap_or_else(|| panic!("no row {id}"));
        change(row);
        gate(&table, &file.fixtures)
    };
    let failed_on =
        |failed: &[String], name: &str| failed.iter().any(|f| f.starts_with(&format!("{name}:")));

    let failed = changed("av1_nvenc:yuv420", |r| r.hw = HwTruth::Software);
    assert!(failed_on(&failed, "windows-nvenc-dgpu"), "{failed:#?}");
    assert!(failed_on(&failed, "linux-arm64-nvenc"), "{failed:#?}");
    assert!(
        !failed_on(&failed, "macos-videotoolbox"),
        "a host without the row is untouched"
    );

    let failed = changed("hevc_videotoolbox:yuv420", |r| r.legacy.transport = None);
    assert!(failed_on(&failed, "macos-videotoolbox"), "{failed:#?}");

    let failed = changed("h264_openh264:yuv420", |r| r.legacy.in_codecs = false);
    assert!(failed_on(&failed, "linux-vp9-only"), "{failed:#?}");

    let failed = changed("hevc_nvenc:yuv444", |r| r.legacy.hevc_chroma = false);
    assert!(failed_on(&failed, "windows-nvenc-dgpu"), "{failed:#?}");

    let failed = changed("h264_mf:yuv420", |r| {
        r.legacy.label_on_enumeration = false;
        r.advertise_software = false;
    });
    assert!(failed_on(&failed, "windows-guest-no-gpu"), "{failed:#?}");

    // A row gone, and two rows swapped: the hello's order is the table's.
    let mut table = CELLS.to_vec();
    table.retain(|r| r.id.wire() != "vp9_libvpx:yuv420");
    assert!(failed_on(&gate(&table, &file.fixtures), "linux-vp9-only"));
    let mut table = CELLS.to_vec();
    let amf = table
        .iter()
        .position(|r| r.id.wire() == "hevc_amf:yuv420")
        .unwrap();
    let d3d12 = table
        .iter()
        .position(|r| r.id.wire() == "hevc_d3d12va:yuv420")
        .unwrap();
    table.swap(amf, d3d12);
    assert!(failed_on(&gate(&table, &file.fixtures), "windows-amf-apu"));
}

/// The fixture's JSON is the wire's: the six fields serialise from `AgentCaps`
/// with the same names, the same skips and in the same order.
#[test]
fn probed_fields_serialise_as_the_hello_does() {
    let file = load();
    const KEYS: [&str; 6] = [
        "hw_encoders",
        "codecs",
        "transports",
        "vp9_chroma",
        "hevc_chroma",
        "video_cells",
    ];
    for f in &file.fixtures {
        let caps = caps_of(&f.hello);
        let wire: serde_json::Value = serde_json::to_value(&caps).expect("plain data");
        let mine: serde_json::Value = serde_json::to_value(&f.hello).expect("plain data");
        for key in KEYS {
            assert_eq!(wire.get(key), mine.get(key), "{}: {key}", f.name);
        }
        assert_eq!(ProbedFields::of(&caps), f.hello, "{}", f.name);
        let text = serde_json::to_string(&caps).expect("plain data");
        let at: Vec<usize> = KEYS
            .iter()
            .filter_map(|k| text.find(&format!("\"{k}\":")))
            .collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "{}: {text}", f.name);
    }
}

/// Every proof a fixture carries names a row, and a row of its platform: the
/// table's platform column agrees with what each recorded host opened.
#[test]
fn every_fixture_proof_is_a_row_of_its_platform() {
    let file = load();
    for f in &file.fixtures {
        for p in proofs(&f.probe) {
            let row = CELLS
                .iter()
                .find(|r| r.id.wire() == p.id)
                .unwrap_or_else(|| panic!("{}: no row for {}", f.name, p.id));
            assert!(
                row.on_platform(f.platform),
                "{}: {} is not a {:?} row",
                f.name,
                p.id,
                f.platform
            );
            assert!(
                !f.deny.contains(&p.id) || row.family == Family::Openh264,
                "{}: the probe opened {}, which its denylist names",
                f.name,
                p.id
            );
        }
    }
}

// ── The table against what it describes ──────────────────────────────────

#[test]
fn every_row_names_itself() {
    let mut ids = HashSet::new();
    for row in CELLS {
        let id = row.id.wire();
        assert!(ids.insert(id), "{id} twice");
        assert_eq!(id, format!("{}:{}", row.id.name(), row.chroma.wire()));
        assert!(row.input_forms.contains(&InputForm::Cpu), "{id}");
        assert!(!row.paths.is_empty(), "{id}");
        let features: Vec<_> = row
            .factors
            .iter()
            .filter(|f| matches!(f, Factor::Feature(_)))
            .collect();
        assert_eq!(features.len(), 1, "{id}: one feature compiles a row in");
        assert!(
            row.factors.iter().any(|f| matches!(f, Factor::Platform(_))),
            "{id}: no platform"
        );
        match row.family {
            Family::Ffmpeg => {
                assert_eq!(row.ffmpeg_name, Some(row.id.name()), "{id}");
                assert_eq!(
                    VideoBackend::from_ffmpeg_name(row.id.name()),
                    Some((row.codec, row.backend)),
                    "{id}"
                );
                assert!(row.rate_factor_pct.is_some(), "{id}");
            }
            Family::MediaFoundation => {
                assert_eq!(row.backend, VideoBackend::MediaFoundation, "{id}");
                let name = match row.codec {
                    VideoCodec::H264 => crate::encode::cells::MF_H264,
                    VideoCodec::Hevc => crate::encode::cells::MF_HEVC,
                    VideoCodec::Av1 => crate::encode::cells::MF_AV1,
                    VideoCodec::Vp9 => panic!("{id}: Media Foundation has no VP9"),
                };
                assert_eq!(row.id.name(), name);
            }
            Family::Libvpx => assert_eq!(row.id.name(), crate::encode::cells::LIBVPX_VP9),
            Family::Openh264 => assert_eq!(row.id.name(), "h264_openh264"),
        }
        if row.family != Family::Ffmpeg {
            assert_eq!(row.ffmpeg_name, None, "{id}");
            assert_eq!(row.rate_factor_pct, None, "{id}");
        }
    }
    let mut seen = HashSet::new();
    for rows in encoders(CELLS) {
        let name = rows[0].id.name();
        assert!(seen.insert(name), "{name}'s rows are split");
        assert_eq!(
            rows.iter().filter(|r| r.opens_first()).count(),
            1,
            "{name}: one form opens first"
        );
        assert!(rows.iter().all(|r| r.id.name() == name), "{name}");
    }
}

/// The 4:4:4 rows are exactly the names the probe asks for 4:4:4.
#[test]
fn the_444_rows_are_the_names_the_probe_asks_for_444() {
    let rows: BTreeSet<&str> = CELLS
        .iter()
        .filter(|r| r.family == Family::Ffmpeg && r.chroma == ChromaFormat::Yuv444)
        .map(|r| r.id.name())
        .collect();
    let asked: BTreeSet<&str> = crate::encode::cells::FFMPEG_444_CAPABLE
        .iter()
        .copied()
        .collect();
    assert_eq!(rows, asked);
    // libvpx's 4:4:4 is its first open; Media Foundation and OpenH264 have none.
    assert!(CELLS.iter().any(|r| r.id.wire() == "vp9_libvpx:yuv444"));
}

/// The quoted names of a `const NAME: &[&str] = &[...]` in `source`.
fn names_in(source: &str, constant: &str) -> Vec<String> {
    let head = format!("const {constant}: &[&str] = &[");
    let start = source
        .find(&head)
        .unwrap_or_else(|| panic!("no `{head}` in the source"))
        + head.len();
    let body = &source[start..start + source[start..].find("];").expect("the list ends")];
    body.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// FFmpeg's rows, per codec, are the cascade tables in their order, and the
/// codecs come in the order the probe walks them: VP9, then HEVC, AV1, H.264.
/// Read from the source, so every lane checks it, not only an FFmpeg build.
#[test]
fn the_ffmpeg_rows_are_the_cascade_tables_in_their_order() {
    const SOURCE: &str = include_str!("../ffmpeg/encoder.rs");
    let mut order: Vec<VideoCodec> = Vec::new();
    for rows in encoders(CELLS) {
        let row = rows[0];
        if row.family == Family::Ffmpeg && order.last() != Some(&row.codec) {
            order.push(row.codec);
        }
    }
    assert_eq!(
        order,
        [
            VideoCodec::Vp9,
            VideoCodec::Hevc,
            VideoCodec::Av1,
            VideoCodec::H264
        ]
    );
    for (codec, constant) in [
        (VideoCodec::Vp9, "VP9_ENCODER_NAMES"),
        (VideoCodec::Hevc, "HEVC_ENCODER_NAMES"),
        (VideoCodec::Av1, "AV1_ENCODER_NAMES"),
        (VideoCodec::H264, "H264_ENCODER_NAMES"),
    ] {
        let rows: Vec<&str> = CELLS
            .iter()
            .filter(|r| {
                r.family == Family::Ffmpeg && r.codec == codec && r.chroma == ChromaFormat::Yuv420
            })
            .map(|r| r.id.name())
            .collect();
        assert_eq!(rows, names_in(SOURCE, constant), "{constant}");
        #[cfg(feature = "ffmpeg-encoder")]
        assert_eq!(
            rows,
            crate::encode::ffmpeg::FfmpegEncoder::cascade_names(codec),
            "{constant}"
        );
    }
}

/// The platform column is what each release's vendored FFmpeg carries, read
/// from the jobs that build it: a row on a platform whose FFmpeg lacks it, or
/// a vendored encoder no row names, fails here.
#[test]
fn the_platform_column_is_the_vendored_ffmpeg_builds() {
    const WINDOWS_AND_LINUX: &str =
        include_str!("../../../../../.github/workflows/vendor-ffmpeg-windows.yml");
    const MACOS: &str = include_str!("../../../../../.github/workflows/vendor-ffmpeg-macos.yml");
    let mut built: Vec<(Platform, BTreeSet<String>)> = Vec::new();
    for (file, is_macos) in [(WINDOWS_AND_LINUX, false), (MACOS, true)] {
        for line in file.lines().map(str::trim) {
            if line.starts_with('#') || line.starts_with("/*") {
                continue;
            }
            let names: BTreeSet<String> = line
                .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .filter_map(|t| t.strip_prefix("--enable-encoder="))
                .flat_map(|list| list.split(','))
                .filter(|n| VideoBackend::from_ffmpeg_name(n).is_some())
                .map(str::to_string)
                .collect();
            if names.is_empty() {
                continue;
            }
            let has = |backend: &str| names.iter().any(|n| n.ends_with(backend));
            let platform = if is_macos {
                Platform::Macos
            } else if has("_d3d12va") {
                Platform::Windows
            } else if has("_qsv") {
                Platform::LinuxX64
            } else {
                Platform::LinuxArm64
            };
            match built.iter_mut().find(|(p, _)| *p == platform) {
                Some((_, all)) => all.extend(names),
                None => built.push((platform, names)),
            }
        }
    }
    assert_eq!(built.len(), 4, "a vendored build per platform: {built:?}");
    for (platform, names) in &built {
        let rows: BTreeSet<String> = CELLS
            .iter()
            .filter(|r| r.family == Family::Ffmpeg && r.on_platform(*platform))
            .map(|r| r.id.name().to_string())
            .collect();
        assert_eq!(&rows, names, "{platform:?}");
    }
}

/// The GPU input forms are the cells `gpu_input` and `vt_input` open on GPU
/// frames: 4:2:0 only. Read from the source in every lane, and from the code
/// where the build has it.
#[test]
fn the_gpu_forms_are_the_cells_that_take_gpu_frames() {
    fn gpu_cells(source: &str) -> BTreeSet<String> {
        let start = source.find("fn gpu_cell(").expect("gpu_cell");
        let body = &source[start..start + source[start..].find("\n}").expect("its end")];
        body.split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    }
    for (form, source) in [
        (InputForm::D3d11, include_str!("../ffmpeg/gpu_input.rs")),
        (InputForm::IoSurface, include_str!("../ffmpeg/vt_input.rs")),
    ] {
        let rows: BTreeSet<String> = CELLS
            .iter()
            .filter(|r| r.input_forms.contains(&form))
            .map(|r| {
                assert_eq!(r.chroma, ChromaFormat::Yuv420, "{}", r.id.wire());
                r.id.name().to_string()
            })
            .collect();
        assert_eq!(rows, gpu_cells(source), "{form:?}");
    }
    #[cfg(all(
        target_os = "windows",
        feature = "ffmpeg-encoder",
        any(feature = "mf-encoder", feature = "wgc-capture")
    ))]
    for row in CELLS.iter().filter(|r| r.family == Family::Ffmpeg) {
        assert_eq!(
            row.input_forms.contains(&InputForm::D3d11),
            crate::encode::ffmpeg::gpu_input::gpu_cell(
                row.id.name(),
                row.chroma == ChromaFormat::Yuv444
            ),
            "{}",
            row.id.wire()
        );
    }
}

/// ledger peer:T06 — the factor is a column: each FFmpeg row's is what
/// `rate_plan` multiplies from the pump's codec label and the chroma column.
#[test]
fn the_rate_factor_column_is_the_rate_plans() {
    let _guard = crate::encode::RELAY_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _saved: Vec<tunnel_core::env::test_env::Saved> = ["H264", "HEVC", "VP9", "AV1"]
        .iter()
        .flat_map(|c| [format!("RATE_FACTOR_{c}"), format!("RATE_FACTOR_{c}_444")])
        .map(|knob| tunnel_core::env::test_env::Saved::cleared(&knob))
        .collect();
    for row in CELLS.iter().filter(|r| r.family == Family::Ffmpeg) {
        // `FfmpegDcCodec::label` (peer.rs), the vocabulary the factor is keyed by.
        let label = match row.codec {
            VideoCodec::H264 => "H264",
            VideoCodec::Hevc => "HEVC",
            VideoCodec::Vp9 => "VP9",
            VideoCodec::Av1 => "AV1",
        };
        let want = crate::encode::rate_profile::codec_rate_factor_pct(label)
            * crate::encode::rate_profile::chroma_rate_factor_pct(
                label,
                row.chroma == ChromaFormat::Yuv444,
            )
            / 100;
        assert_eq!(
            row.rate_factor_pct.map(usize::from),
            Some(want),
            "{}",
            row.id.wire()
        );
    }
}

// ── FR-135 §5.7, read against the table ──────────────────────────────────

/// Dv1 — the DataChannel pick reads the union, not the transport: a session on
/// `data-channel-h264` is bound to AV1 when the browser lists it, though the
/// table's row behind that transport is H.264. On the RTP track the pick has
/// followed the transport since #3245 (Dv16).
#[test]
fn dv1_a_datachannel_pick_reads_the_union_not_the_transport() {
    let file = load();
    let caps = caps_of(&fixture(&file, "windows-nvenc-dgpu").hello);
    let browser: Vec<String> = ["av1", "h265", "vp9", "h264"].map(String::from).to_vec();
    let behind: BTreeSet<&str> = CELLS
        .iter()
        .filter(|r| r.legacy.transport == Some(DC_H264))
        .map(|r| negotiation_name(r.codec))
        .collect();
    assert_eq!(behind, BTreeSet::from(["h264"]));
    let picked = crate::peer::session_codec(&browser, &caps, Some(DC_H264));
    assert_eq!(
        picked, "av1",
        "Dv1 reads as fixed: the DataChannel pick follows its transport now"
    );
}

/// Dv3 — the RTP track's codec is patched by rules, not derived from a row: a
/// DataChannel session whose offer lacks the pump's codec gets a VP8 track, a
/// codec no row carries, and an RTP offer without it is refused by name.
#[test]
fn dv3_the_rtp_track_is_patched_not_derived() {
    let vp8_only = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 VP8/90000\r\n";
    let track = crate::peer::rtp_track_codec("h264", Some(DC_VP9_444), vp8_only);
    assert_eq!(track, "vp8");
    assert!(
        !CELLS.iter().any(|r| negotiation_name(r.codec) == track),
        "Dv3 — a row now carries {track}"
    );
    assert!(crate::peer::rtp_track_unanswerable("h264", None, vp8_only));
}

/// Dv13 — the fps verdict cache keys a class by the encoder's name alone, so
/// the table's cells of one encoder (each chroma, each input form) share one
/// verdict.
#[test]
fn dv13_the_fps_class_key_cannot_tell_a_cell_from_its_other_forms() {
    use crate::encode::frame_rate::ClassKey;
    let key = |name: &str| ClassKey {
        backend: "wgc".into(),
        refresh_hz: 60,
        encoder: name.into(),
        long_edge: 1920,
        fps: 120,
    };
    let ffmpeg = || CELLS.iter().filter(|r| r.family == Family::Ffmpeg);
    let forms: Vec<(&str, InputForm)> = ffmpeg()
        .flat_map(|r| r.input_forms.iter().map(move |f| (r.id.wire(), *f)))
        .collect();
    let keys: HashSet<ClassKey> = ffmpeg().map(|r| key(r.id.name())).collect();
    assert!(
        forms.len() > keys.len(),
        "{} forms, {} keys",
        forms.len(),
        keys.len()
    );
    let hevc_nvenc: Vec<_> = forms
        .iter()
        .filter(|(id, _)| id.starts_with("hevc_nvenc:"))
        .collect();
    assert_eq!(hevc_nvenc.len(), 3, "4:2:0 on CPU and D3D11, 4:4:4 on CPU");
    assert_eq!(
        hevc_nvenc
            .iter()
            .map(|(id, _)| key(id.split(':').next().unwrap_or_default()))
            .collect::<HashSet<_>>()
            .len(),
        1,
        "one verdict for the three"
    );
}

/// Dv15 — "H.264" is four cascades: the RTP track's (Media Foundation, then
/// OpenH264), the DataChannel's (FFmpeg's), the recorder's (FFmpeg's, then
/// OpenH264), and Media Foundation's own across its MFTs inside one row. The
/// table holds the rows of each path; their orders differ from each other and
/// from the table's one (the probe's), which is what a per-path order fixes.
#[test]
fn dv15_h264_is_four_cascades() {
    let h264 = |path: MediaPath| -> Vec<&str> {
        CELLS
            .iter()
            .filter(|r| {
                r.codec == VideoCodec::H264 && r.chroma == ChromaFormat::Yuv420 && r.has_path(path)
            })
            .map(|r| r.id.name())
            .collect()
    };
    let rtp = h264(MediaPath::Rtp);
    let dc = h264(MediaPath::Dc);
    let recording = h264(MediaPath::Recording);
    assert_eq!(rtp, ["h264_openh264", "h264_mf"], "the table's order");
    // `open_default` under Auto tries Media Foundation first.
    let rtp_opens: Vec<&str> = rtp.iter().rev().copied().collect();
    assert_ne!(rtp, rtp_opens, "the RTP track's order is not the table's");
    assert_eq!(dc.len(), 7, "FFmpeg's H.264 cascade");
    assert!(dc.iter().all(|n| !rtp.contains(n)));
    assert_eq!(
        recording.first(),
        Some(&"h264_openh264"),
        "the table's order"
    );
    // The recorder tries FFmpeg first and OpenH264 last.
    assert_eq!(
        recording.iter().filter(|n| dc.contains(n)).count(),
        7,
        "the recorder's hardware rungs are the DataChannel's"
    );
    let paths = [MediaPath::Rtp, MediaPath::Dc, MediaPath::Recording]
        .map(h264)
        .into_iter()
        .filter(|rows| !rows.is_empty())
        .count();
    assert_eq!(paths, 3, "three paths, plus Media Foundation's own cascade");
}

/// Dv16 — on the RTP track the pick binds H.264 first, as the table's RTP
/// column does: every host opens H.264 there. Since #3245 this reads as no
/// divergence; the DataChannel rank stays best-of.
#[test]
fn dv16_the_rtp_track_binds_h264_first_as_the_table_does() {
    let file = load();
    let caps = caps_of(&fixture(&file, "windows-nvenc-dgpu").hello);
    let browser: Vec<String> = ["av1", "h265", "h264"].map(String::from).to_vec();
    assert!(
        CELLS
            .iter()
            .any(|r| r.codec == VideoCodec::H264 && r.has_path(MediaPath::Rtp))
    );
    assert_eq!(crate::peer::session_codec(&browser, &caps, None), "h264");
    assert_eq!(
        crate::encode::caps::pick_best_codec(&browser, &caps.codecs),
        "av1",
        "the DataChannel's rank is best-of"
    );
}

/// Dv20 — the rate move is resolved from the encoder's name far from the cell
/// (`resolve_rate_mode` matches substrings). The column holds the same answer,
/// read against that code, so a reader can move to the row with a proof.
#[cfg(feature = "ffmpeg-encoder")]
#[test]
fn dv20_the_rate_move_column_is_what_resolve_rate_mode_says() {
    use crate::encode::ffmpeg::encoder::{
        RateReconfig, reconfig_forces_idr_for, resolve_rate_mode,
    };
    for row in CELLS.iter().filter(|r| r.family == Family::Ffmpeg) {
        let name = row.id.name();
        let (off, on) = (
            resolve_rate_mode(name, false),
            resolve_rate_mode(name, true),
        );
        let want = match (off, on) {
            (RateReconfig::InPlaceVbr, RateReconfig::InPlaceVbr) => RateMove::InPlace {
                idr: Some(reconfig_forces_idr_for(off, false)),
            },
            (RateReconfig::Rebuild, RateReconfig::Rebuild) => RateMove::Rebuild,
            (RateReconfig::Rebuild, RateReconfig::InPlaceCbr) => RateMove::RebuildOrInPlace,
            other => panic!("{name}: no column for {other:?}"),
        };
        assert_eq!(row.rate_move, want, "{name}");
    }
}

/// The divergences the recorded hosts carry include the ones FR-135 §5.7
/// names for them, so each reads as a divergence against the table today.
#[test]
fn the_recorded_hosts_read_the_named_divergences() {
    let file = load();
    let has = |name: &str, dv: &str, at: &str| {
        fixture(&file, name)
            .divergences
            .iter()
            .any(|d| d.dv == dv && d.at == at)
    };
    // Dv2: AV1 in `codecs` from av1_nvenc, which no RTP opener reaches.
    assert!(has("windows-nvenc-dgpu", "Dv2", "av1"));
    // Dv4: the label says hardware over Microsoft's software MFT.
    assert!(has("windows-guest-no-gpu", "Dv4", "mf-h264-hw"));
    // Dv4 (P7): a QSV HEVC winner's 4:4:4 that `hevc_chroma` cannot say.
    assert!(has("windows-qsv-444-asked", "Dv4", "hevc_qsv:yuv444"));
    // Dv6: vp9_vaapi's `hw` is the QSV build flag, false on arm64.
    assert!(has("linux-arm64-vaapi", "Dv6", "vp9_vaapi:yuv420"));
    // Dv7: Media Foundation's HEVC with no DataChannel transport.
    assert!(has("windows-ffmpeg-opted-out", "Dv7", "hevc/mf"));
    // Dv10: OpenH264's cell, from the build alone.
    assert!(has("linux-vp9-only", "Dv10", "h264_openh264:yuv420"));
    // Dv12: NVENC's GPU form, unproven on the wire.
    assert!(has("windows-nvenc-dgpu", "Dv12", "hevc_nvenc:yuv420"));
    // Dv19: libvpx describes its colour in part; vp9_qsv not at all.
    assert!(has("linux-vp9-only", "Dv19", "vp9_libvpx:yuv444"));
    assert!(has("windows-qsv-office-desktop", "Dv19", "vp9_qsv:yuv420"));
    // And a host the table agrees with leaves no Dv2.
    assert!(
        !fixture(&file, "macos-videotoolbox")
            .divergences
            .iter()
            .any(|d| d.dv == "Dv2")
    );
}

/// Dv11 — one predicate over every row would deny what the denylist names;
/// OpenH264 has no gate, so a list naming it changes nothing on the wire.
#[test]
fn dv11_a_row_no_gate_reads() {
    let file = load();
    let f = fixture(&file, "linux-vp9-only");
    let deny = ["h264_openh264:yuv420".to_string()];
    let host = Host {
        platform: f.platform,
        deny: &deny,
    };
    let read = divergences(
        CELLS,
        &proofs(&f.probe),
        &ProbeKnobs::of(&f.probe),
        &host,
        &assemble(&f.probe),
    );
    assert!(
        read.iter()
            .any(|d| d.dv == "Dv11" && d.at == "h264_openh264:yuv420"),
        "{read:#?}"
    );
}

// ── The shadow ───────────────────────────────────────────────────────────

/// The shadow says nothing where the table agrees, says it once where it does
/// not, and never changes what it compares.
#[test]
fn the_shadow_says_once_and_leaves_the_hello_alone() {
    let file = load();
    let f = fixture(&file, "windows-nvenc-dgpu");
    let real = assemble(&f.probe);
    let before = real.clone();
    let warned = AtomicBool::new(false);
    assert!(!shadow_into(&warned, CELLS, &f.probe, &real));
    assert_eq!(disagreement(CELLS, &f.probe, &real), None);

    let mut table = CELLS.to_vec();
    table.retain(|r| r.id.wire() != "av1_nvenc:yuv420");
    let diff = disagreement(&table, &f.probe, &real).expect("a missing row disagrees");
    assert!(diff.contains("av1"), "{diff}");
    assert!(shadow_into(&warned, &table, &f.probe, &real), "said");
    assert!(!shadow_into(&warned, &table, &f.probe, &real), "once");
    assert_eq!(real, before, "the hello is not touched");
}

/// A host outside the table (an encoder no row names, an empty probe, a
/// repeated open) still yields an answer and never a panic.
#[test]
fn the_shadow_takes_a_host_outside_the_table() {
    let file = load();
    let mut odd = fixture(&file, "linux-nvenc-wsl").probe.clone();
    if let Some(ff) = odd.ffmpeg.as_mut() {
        ff.matrix[0].opened.push(Opened {
            name: "vp9_nvenc".into(),
            chroma: vec![ChromaFormat::Yuv420],
        });
        ff.matrix[0].opened.push(Opened {
            name: "not_an_encoder".into(),
            chroma: vec![ChromaFormat::Yuv444],
        });
        ff.vp9 = Some(Opened {
            name: "vp9_qsv".into(),
            chroma: Vec::new(),
        });
    }
    let real = assemble(&odd);
    let warned = AtomicBool::new(false);
    assert!(disagreement(CELLS, &odd, &real).is_some());
    assert!(shadow_into(&warned, CELLS, &odd, &real));
    let empty = ProbeFindings::default();
    assert_eq!(disagreement(CELLS, &empty, &assemble(&empty)), None);
    assert_eq!(
        derive(&[], &proofs(&odd), &ProbeKnobs::of(&odd)),
        ProbedFields::default()
    );
}
