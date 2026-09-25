// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-85 P5a — the export engine, end to end: a recording whose every frame
//! paints its own index, an edit list, an export, and the frames that come
//! back out of a decoder are exactly the ones the edit list says.
//!
//! ⚠️ This binary runs only when CI NAMES it (`--test export`).
//!
//! The oracle is the recorder tests' counter: 16 luma blocks of 16×16 px per
//! frame, black or white, big enough to survive H.264 at any sane QP. It is
//! proven to discriminate here too: the same comparison against the unedited
//! source fails.

#![cfg(all(feature = "recording", feature = "openh264-encoder"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use roomlerd::capture::{Damage, Frame, PixelFormat};
use roomlerd::encode::VideoEncoder;
use roomlerd::encode::openh264_backend::Openh264Encoder;
use roomlerd::recording::annexb::length_prefixed_to_annexb;
use roomlerd::recording::edit::{Action, EditList, Segment, VERSION};
use roomlerd::recording::export::{self, ExportError};
use roomlerd::recording::mp4::{
    self, ColorInfo, FragmentedWriter, ProgressiveFile, VIDEO_TRACK_ID, VideoTrack,
};

const W: u32 = 320;
const H: u32 = 240;
const BLOCK: u32 = 16;
const BITS: u32 = 16;
const FPS: u32 = 30;
const TICK: u64 = 3000; // one frame at 30 fps, 90 kHz

fn render(counter: u16) -> Vec<u8> {
    let mut data = vec![0x80u8; (W * H * 4) as usize];
    for bit in 0..BITS {
        let on = (counter >> (BITS - 1 - bit)) & 1 == 1;
        let v = if on { 0xFF } else { 0x00 };
        for y in 0..BLOCK {
            for x in bit * BLOCK..(bit + 1) * BLOCK {
                let i = ((y * W + x) * 4) as usize;
                data[i..i + 3].copy_from_slice(&[v, v, v]);
                data[i + 3] = 0xFF;
            }
        }
    }
    data
}

fn read_counter(y_plane: &[u8], stride: usize) -> u16 {
    let mut v = 0u16;
    for bit in 0..BITS as usize {
        let mut sum = 0u32;
        for dy in 4..12usize {
            for dx in 4..12usize {
                sum += u32::from(y_plane[dy * stride + bit * BLOCK as usize + dx]);
            }
        }
        v = (v << 1) | u16::from(sum / 64 > 128);
    }
    v
}

fn software() -> roomlerd::recording::recorder::EncoderFactory {
    Box::new(|w, h| {
        let e = Openh264Encoder::new_recording(w, h, FPS, FPS * 2)?;
        Ok(Box::new(e) as Box<dyn VideoEncoder>)
    })
}

/// A progressive recording of `frames` frames at 30 fps in which frame `i`
/// paints `i`, through the recording encoder and writer the recorder uses —
/// deterministic, so frame i of the file IS counter i.
async fn make_source(path: &Path, frames: u16) {
    let partial = path.with_extension("partial");
    let mut enc = Openh264Encoder::new_recording(W, H, FPS, FPS * 2).unwrap();
    let mut w = FragmentedWriter::create(
        &partial,
        VideoTrack {
            width: W,
            height: H,
            fps: FPS,
            color: ColorInfo::BT601_LIMITED,
        },
        None,
    )
    .unwrap();
    for i in 0..frames {
        let f = Arc::new(Frame {
            width: W,
            height: H,
            stride: W * 4,
            pixel_format: PixelFormat::Bgra,
            data: render(i),
            monotonic_us: 0,
            monitor: 0,
            damage: Damage::Unknown,
            source: None,
        });
        let packets = enc.encode(f).await.unwrap();
        let key = packets.iter().any(|p| p.is_keyframe);
        let au: Vec<u8> = packets.into_iter().flat_map(|p| p.data).collect();
        w.push_video(u64::from(i) * TICK, &au, key).unwrap();
    }
    w.finish().unwrap();
    mp4::finalize(&partial, path).unwrap();
    std::fs::remove_file(&partial).unwrap();
}

/// Every frame of a progressive file, decoded, as its counter.
fn counters(path: &Path) -> Vec<u16> {
    use openh264::formats::YUVSource;
    let pf = ProgressiveFile::open(path).expect("open");
    assert!(pf.moov_offset < pf.mdat_offset, "moov must precede mdat");
    let samples = pf.samples(VIDEO_TRACK_ID).expect("samples");
    let ps = pf.avc_parameter_sets_annexb().expect("avcC");
    let mut dec = openh264::decoder::Decoder::new().expect("decoder");
    let mut f = std::fs::File::open(path).unwrap();
    let mut out = Vec::new();
    for s in &samples {
        let raw = pf.read_sample(&mut f, s).unwrap();
        let mut au = Vec::new();
        if s.sync {
            au.extend_from_slice(&ps);
        }
        au.extend_from_slice(&length_prefixed_to_annexb(&raw).unwrap());
        let pic = dec.decode(&au).unwrap().expect("a picture per sample");
        let (y_stride, _, _) = pic.strides();
        out.push(read_counter(pic.y(), y_stride));
    }
    out
}

fn edit_list(segments: Vec<Segment>) -> EditList {
    EditList {
        version: VERSION,
        source: "source.mp4".into(),
        segments,
    }
}

fn seg(start_ms: u64, end_ms: u64, action: Action) -> Segment {
    Segment {
        start_ms,
        end_ms,
        action,
    }
}

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// The plan's own oracle: keep 0–2 s, cut 2–4 s, 4× on 4–8 s, keep 8–10 s
/// of a 10 s recording is exactly frames 0..59, 120, 124 … 236, 240..299 —
/// five seconds, moov first, and the recording itself untouched.
#[tokio::test]
async fn an_export_shows_exactly_the_frames_its_edit_list_says() {
    let d = dir();
    let source = d.path().join("source.mp4");
    make_source(&source, 300).await;
    let before = std::fs::read(&source).unwrap();

    let edits = edit_list(vec![
        seg(0, 2000, Action::Keep),
        seg(2000, 4000, Action::Cut),
        seg(4000, 8000, Action::Speed { speed: 4.0 }),
        seg(8000, 10_000, Action::Keep),
    ]);
    let dest = export::edited_name(&source).unwrap();
    let mut seen = Vec::new();
    let s = export::export(
        &source,
        &edits,
        &dest,
        software(),
        &AtomicBool::new(false),
        |done, total| seen.push((done, total)),
    )
    .await
    .unwrap();

    let expected: Vec<u16> = (0..60)
        .chain((0..30).map(|k| 120 + 4 * k))
        .chain(240..300)
        .collect();
    let got = counters(&dest);
    assert_eq!(got, expected);
    assert_eq!(s.frames, 150);
    assert!(
        (4966..=5034).contains(&s.duration_ms),
        "five seconds ± a frame: {s:?}"
    );
    assert_eq!(seen.last(), Some(&(150, 150)), "progress reaches the end");
    assert_eq!(
        std::fs::read(&source).unwrap(),
        before,
        "the recording is never written"
    );
    assert!(
        !d.path()
            .join(".roomler-partial")
            .join(format!(
                "{}.partial",
                dest.file_name().unwrap().to_string_lossy()
            ))
            .exists(),
        "nothing staged is left behind"
    );

    // The oracle discriminates: the unedited source is NOT that sequence.
    let source_frames = counters(&source);
    assert_eq!(source_frames, (0..300).collect::<Vec<u16>>());
    assert_ne!(source_frames, expected);
}

/// The control for the engine itself: keeping everything reproduces the
/// source frame for frame — so the edit, not the engine, made the difference.
#[tokio::test]
async fn keeping_everything_reproduces_the_recording() {
    let d = dir();
    let source = d.path().join("source.mp4");
    make_source(&source, 90).await;
    let dest = export::edited_name(&source).unwrap();
    let s = export::export(
        &source,
        &edit_list(vec![seg(0, 3000, Action::Keep)]),
        &dest,
        software(),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(s.frames, 90);
    assert_eq!(counters(&dest), (0..90).collect::<Vec<u16>>());
}

#[tokio::test]
async fn a_cancelled_or_impossible_export_leaves_nothing_behind() {
    let d = dir();
    let source = d.path().join("source.mp4");
    make_source(&source, 60).await;
    let dest = export::edited_name(&source).unwrap();

    let cancelled = export::export(
        &source,
        &edit_list(vec![seg(0, 2000, Action::Keep)]),
        &dest,
        software(),
        &AtomicBool::new(true),
        |_, _| {},
    )
    .await;
    assert!(
        matches!(cancelled, Err(ExportError::Cancelled)),
        "{cancelled:?}"
    );

    let nothing = export::export(
        &source,
        &edit_list(vec![seg(0, 2000, Action::Cut)]),
        &dest,
        software(),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .await;
    let e = nothing.unwrap_err();
    assert_eq!(e.code(), "bad_edit_list");
    assert!(e.detail().contains("nothing to export"), "{}", e.detail());

    assert!(!dest.exists(), "no export file");
    let staged: Vec<PathBuf> = std::fs::read_dir(d.path().join(".roomler-partial"))
        .map(|r| r.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(staged.is_empty(), "nothing staged left: {staged:?}");
}

/// The process roomler-desktop runs: `roomlerd media probe` says the
/// recording is editable, and `roomlerd media export` writes the file and
/// says so, flagging that P5a carries no audio.
#[tokio::test]
async fn the_media_process_probes_and_exports() {
    use roomlerd::recording::child::parse_event_line;
    let d = dir();
    let source = d.path().join("source.mp4");
    make_source(&source, 60).await;
    let edl = d.path().join("source.mp4.edit.json");
    std::fs::write(
        &edl,
        serde_json::to_string(&edit_list(vec![
            seg(0, 1000, Action::Cut),
            seg(1000, 2000, Action::Speed { speed: 2.0 }),
        ]))
        .unwrap(),
    )
    .unwrap();

    let exe = env!("CARGO_BIN_EXE_roomlerd");
    let run = |args: &[&str]| {
        let out = std::process::Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(parse_event_line)
            .collect::<Vec<_>>()
    };

    let probe = run(&["media", "probe", source.to_str().unwrap()]);
    let p = probe
        .iter()
        .find(|e| e["ev"] == "probe")
        .expect("a probe event");
    assert_eq!(p["editable"], true, "{p}");
    assert_eq!(p["duration_ms"], 2000, "{p}");
    assert_eq!(p["width"], W, "{p}");

    let events = run(&[
        "media",
        "export",
        "--edl",
        edl.to_str().unwrap(),
        "--encoder",
        "software",
    ]);
    let done = events
        .iter()
        .find(|e| e["ev"] == "done")
        .unwrap_or_else(|| panic!("no done event: {events:?}"));
    assert_eq!(done["frames"], 15, "{done}");
    assert_eq!(done["audio"], "not_carried", "{done}");
    let path = PathBuf::from(done["path"].as_str().unwrap());
    assert!(path.ends_with("source (edited).mp4"), "{}", path.display());
    // Cut the first second, then 2× over the second: frames 30, 32 … 58.
    assert_eq!(
        counters(&path),
        (0..15).map(|k| 30 + 2 * k).collect::<Vec<u16>>()
    );

    // A list naming a file outside its folder is refused before anything
    // is read.
    std::fs::write(
        &edl,
        r#"{"version":1,"source":"../elsewhere.mp4","segments":[{"start_ms":0,"end_ms":1000,"action":"keep"}]}"#,
    )
    .unwrap();
    let refused = run(&["media", "export", "--edl", edl.to_str().unwrap()]);
    let r = refused
        .iter()
        .find(|e| e["ev"] == "refused")
        .expect("refused");
    assert_eq!(r["code"], "bad_edit_list", "{r}");
}
