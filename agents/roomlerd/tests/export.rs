// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-85 P5a — the export engine, end to end: a recording whose every frame
//! paints its own index, an edit list, an export, and the frames that come
//! back out of a decoder are exactly the ones the edit list says. P5b, with
//! `audio` (`mod sound`): the same for the sound, second by second.
//!
//! ⚠️ This binary runs only when CI NAMES it (`--test export`), and `mod
//! sound` only in the run that also names `audio`.
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
        original_volume: None,
        music: None,
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
/// says so, with what its sound carries (nothing: the recording is silent).
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
    // The recording is silent and there is no music: no sound, said so.
    assert_eq!(done["audio"], "none", "{done}");
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

/// FR-85 P5b — an export's sound. The oracle is the audio counterpart of the
/// frame counter: a STEPPED tone, second k of the recording playing
/// (300 + 100·k) Hz, so every second of an export names the second of the
/// recording it came from.
#[cfg(feature = "audio")]
mod sound {
    use super::*;
    use roomlerd::recording::edit::Music;
    use roomlerd::recording::mp4::{AUDIO_TRACK_ID, AudioCodec, AudioTrack};

    const RATE: usize = 48_000;
    const FRAME: usize = 960;
    const PER_VIDEO_FRAME: usize = RATE / FPS as usize; // 1600

    /// Source second `t` plays this.
    fn stepped_hz(t: f64) -> f64 {
        300.0 + 100.0 * t.floor()
    }

    /// Like `make_source`, with an Opus track of the stepped tone.
    async fn make_source_with_sound(path: &Path, frames: u16) {
        use audiopus::coder::Encoder;
        use audiopus::{Application, Channels, SampleRate};
        let partial = path.with_extension("partial");
        let opus = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio).unwrap();
        let pre_skip = opus.lookahead().unwrap() as u16;
        let mut enc = Openh264Encoder::new_recording(W, H, FPS, FPS * 2).unwrap();
        let mut w = FragmentedWriter::create(
            &partial,
            VideoTrack {
                width: W,
                height: H,
                fps: FPS,
                color: ColorInfo::BT601_LIMITED,
            },
            Some(AudioTrack {
                sample_rate: RATE as u32,
                channels: 2,
                codec: AudioCodec::Opus { pre_skip },
            }),
        )
        .unwrap();
        let total = frames as usize * PER_VIDEO_FRAME;
        // The encoder's lookahead is paid up front, so decoded sample 0 (after
        // pre_skip) is time 0.
        let mut phase = 0.0f64;
        let mut pcm: Vec<i16> = vec![0; usize::from(pre_skip) * 2];
        for s in 0..total + FRAME {
            let hz = stepped_hz(s as f64 / RATE as f64);
            let v = (phase.sin() * 8000.0) as i16;
            phase += 2.0 * std::f64::consts::PI * hz / RATE as f64;
            pcm.extend_from_slice(&[v, v]);
        }
        let mut sent = 0usize;
        let mut out = vec![0u8; 4000];
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
            let until = (usize::from(i) + 1) * PER_VIDEO_FRAME;
            while sent < until {
                let chunk = &pcm[sent * 2..(sent + FRAME) * 2];
                let n = opus.encode(chunk, &mut out).unwrap();
                w.push_audio(&out[..n], FRAME as u32).unwrap();
                sent += FRAME;
            }
        }
        w.finish().unwrap();
        mp4::finalize(&partial, path).unwrap();
        std::fs::remove_file(&partial).unwrap();
    }

    /// The whole audio track, decoded, interleaved stereo, `pre_skip` dropped.
    /// `None` = no audio track.
    fn decode_sound(path: &Path) -> Option<Vec<i16>> {
        use audiopus::coder::Decoder;
        use audiopus::{Channels, SampleRate};
        let pf = ProgressiveFile::open(path).unwrap();
        let format = pf.audio_format().unwrap()?;
        let mut dec = Decoder::new(SampleRate::Hz48000, Channels::Stereo).unwrap();
        let mut f = std::fs::File::open(path).unwrap();
        let mut out = Vec::new();
        let mut buf = vec![0i16; 5760 * 2];
        for s in pf.samples(AUDIO_TRACK_ID).unwrap() {
            let data = pf.read_sample(&mut f, &s).unwrap();
            let packet: audiopus::packet::Packet<'_> = (&data[..]).try_into().unwrap();
            let signals: audiopus::MutSignals<'_, i16> = (&mut buf[..]).try_into().unwrap();
            let n = dec.decode(Some(packet), signals, false).unwrap();
            out.extend_from_slice(&buf[..n * 2]);
        }
        Some(out.split_off(usize::from(format.pre_skip) * 2))
    }

    /// The left channel between two times, in seconds.
    fn span(pcm: &[i16], from: f64, to: f64) -> Vec<f64> {
        let a = (from * RATE as f64) as usize;
        let b = ((to * RATE as f64) as usize).min(pcm.len() / 2);
        (a..b).map(|i| f64::from(pcm[i * 2])).collect()
    }

    fn rms(x: &[f64]) -> f64 {
        (x.iter().map(|v| v * v).sum::<f64>() / x.len().max(1) as f64).sqrt()
    }

    /// Frequency from zero crossings: two per cycle.
    fn hz(x: &[f64]) -> f64 {
        let crossings = x
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        crossings as f64 / 2.0 / (x.len() as f64 / RATE as f64)
    }

    /// A stereo 16-bit WAV at 44.1 kHz, so the music is resampled on its way
    /// in, as a real song usually is.
    fn write_wav(path: &Path, freq: f64, seconds: f64, amplitude: f64) {
        let rate = 44_100u32;
        let n = (seconds * f64::from(rate)) as u32;
        let samples: Vec<i16> = (0..n)
            .flat_map(|i| {
                let v = ((2.0 * std::f64::consts::PI * freq * f64::from(i) / f64::from(rate)).sin()
                    * amplitude) as i16;
                [v, v]
            })
            .collect();
        write_wav_raw(path, rate, &samples);
    }

    /// Interleaved stereo samples as a WAV declaring `rate`, whatever it is.
    fn write_wav_raw(path: &Path, rate: u32, samples: &[i16]) {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&rate.wrapping_mul(4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, wav).unwrap();
    }

    fn music(path: &str) -> Music {
        Music {
            path: path.into(),
            volume: 0.5,
            start_ms: 0,
            fade_in_ms: 0,
            fade_out_ms: 0,
            looped: true,
        }
    }

    async fn run(source: &Path, edits: &EditList) -> Result<export::ExportSummary, ExportError> {
        let dest = export::edited_name(source).unwrap();
        export::export(
            source,
            edits,
            &dest,
            software(),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .await
    }

    /// Keep 0–2 s · cut 2–4 s · 4× 4–8 s · keep 8–10 s: output second 0
    /// plays recording second 0 (300 Hz), second 1 plays second 1 (400 Hz),
    /// second 2 is the speed-up and is MUTED, seconds 3 and 4 play recording
    /// seconds 8 and 9 (1100, 1200 Hz). The cut and the offsets are proven,
    /// not just "some sound".
    #[tokio::test]
    async fn the_recordings_own_sound_follows_the_edit() {
        let d = dir();
        let source = d.path().join("source.mp4");
        make_source_with_sound(&source, 300).await;
        // The oracle discriminates: the recording itself, second 3, is 600 Hz.
        let original = decode_sound(&source).unwrap();
        assert!((hz(&span(&original, 3.2, 3.8)) - 600.0).abs() < 15.0);

        let s = run(
            &source,
            &edit_list(vec![
                seg(0, 2000, Action::Keep),
                seg(2000, 4000, Action::Cut),
                seg(4000, 8000, Action::Speed { speed: 4.0 }),
                seg(8000, 10_000, Action::Keep),
            ]),
        )
        .await
        .unwrap();
        assert_eq!(s.audio, "original");
        let pcm = decode_sound(&s.path).expect("an audio track");
        let secs = pcm.len() as f64 / 2.0 / RATE as f64;
        assert!(
            (4.95..=5.05).contains(&secs),
            "the sound lasts as long as the picture: {secs}"
        );
        for (second, want) in [(0.0, 300.0), (1.0, 400.0), (3.0, 1100.0), (4.0, 1200.0)] {
            let x = span(&pcm, second + 0.2, second + 0.8);
            assert!(rms(&x) > 3000.0, "second {second} is loud: {}", rms(&x));
            let got = hz(&x);
            assert!(
                (got - want).abs() < 15.0,
                "second {second}: {got} Hz, want {want}"
            );
        }
        let muted = rms(&span(&pcm, 2.2, 2.8));
        assert!(muted < 100.0, "the speed-up is muted: RMS {muted}");
    }

    /// Music plays under the whole export, the sped-up stretch included,
    /// loops when it is shorter, fades in, and sits at its volume.
    #[tokio::test]
    async fn music_plays_under_the_whole_export_loops_and_fades_in() {
        let d = dir();
        let source = d.path().join("source.mp4");
        make_source(&source, 300).await; // no sound of its own
        write_wav(&d.path().join("song.wav"), 880.0, 2.0, 16_000.0);
        let mut edits = edit_list(vec![
            seg(0, 2000, Action::Keep),
            seg(2000, 10_000, Action::Speed { speed: 4.0 }),
        ]);
        let mut m = music("song.wav");
        m.fade_in_ms = 1000;
        edits.music = Some(m);

        let s = run(&source, &edits).await.unwrap();
        assert_eq!(s.audio, "music");
        let pcm = decode_sound(&s.path).expect("an audio track");
        let secs = pcm.len() as f64 / 2.0 / RATE as f64;
        assert!((3.95..=4.05).contains(&secs), "{secs}");
        // Full volume: a 16000 sine at 0.5 has an RMS of 8000/√2 ≈ 5657.
        let full = rms(&span(&pcm, 1.2, 1.8));
        assert!((4500.0..=6800.0).contains(&full), "RMS {full}");
        assert!((hz(&span(&pcm, 1.2, 1.8)) - 880.0).abs() < 15.0);
        // Under the speed-up, and past the song's two seconds: it looped.
        let later = span(&pcm, 2.5, 3.5);
        assert!(rms(&later) > 4500.0, "the music loops: RMS {}", rms(&later));
        assert!((hz(&later) - 880.0).abs() < 15.0);
        // The fade-in: quieter at the start than at full volume.
        let early = rms(&span(&pcm, 0.05, 0.3));
        assert!(early < full * 0.4, "fading in: {early} vs {full}");
    }

    /// Both together: inside the speed-up only the music is heard.
    #[tokio::test]
    async fn a_speed_up_keeps_the_music_and_mutes_the_recording() {
        let d = dir();
        let source = d.path().join("source.mp4");
        make_source_with_sound(&source, 300).await;
        write_wav(&d.path().join("song.wav"), 2000.0, 1.0, 16_000.0);
        let mut edits = edit_list(vec![
            seg(0, 2000, Action::Keep),
            seg(2000, 10_000, Action::Speed { speed: 4.0 }),
        ]);
        edits.music = Some(music("song.wav"));
        let s = run(&source, &edits).await.unwrap();
        assert_eq!(s.audio, "original_and_music");
        let pcm = decode_sound(&s.path).unwrap();
        let sped = span(&pcm, 2.2, 3.8);
        assert!(
            (hz(&sped) - 2000.0).abs() < 30.0,
            "only the music: {} Hz",
            hz(&sped)
        );
    }

    #[tokio::test]
    async fn silence_is_no_track_and_bad_music_is_refused_by_name() {
        let d = dir();
        let source = d.path().join("source.mp4");
        make_source_with_sound(&source, 60).await;
        let mut edits = edit_list(vec![seg(0, 2000, Action::Keep)]);
        edits.original_volume = Some(0.0);
        let s = run(&source, &edits).await.unwrap();
        assert_eq!(s.audio, "none");
        assert!(decode_sound(&s.path).is_none(), "no audio track at all");

        std::fs::write(d.path().join("song.mp3"), b"not music").unwrap();
        let mut edits = edit_list(vec![seg(0, 2000, Action::Keep)]);
        edits.music = Some(music("song.mp3"));
        let e = run(&source, &edits).await.unwrap_err();
        assert_eq!(e.code(), "music_unreadable", "{}", e.detail());
    }

    /// The music is a file the person picked: untrusted input. A WAV
    /// declaring 0 Hz makes symphonia 0.5.5's probe PANIC, and one declaring
    /// 1 Hz would grow every packet 48 000-fold on its way to 48 kHz. Both
    /// are refused by name, and the export lives to say so.
    ///
    /// ⚠️ The 1 Hz cell is green with EITHER rate check deleted (the header
    /// at open, every buffer in `fill`): it is red only with the range
    /// itself widened. Keep both: `fill`'s is the gate (a container can
    /// declare one rate and its codec produce another), the open's says it
    /// before the export starts.
    #[tokio::test]
    async fn a_malformed_music_file_is_refused_by_name_not_a_crash() {
        let d = dir();
        let source = d.path().join("source.mp4");
        make_source(&source, 30).await;
        let tone: Vec<i16> = (0..4_000)
            .map(|i| ((i % 50) * 400 - 10_000) as i16)
            .collect();
        for (name, rate) in [("zero-hz.wav", 0u32), ("one-hz.wav", 1)] {
            write_wav_raw(&d.path().join(name), rate, &tone);
            let mut edits = edit_list(vec![seg(0, 1000, Action::Keep)]);
            edits.music = Some(music(name));
            let e = run(&source, &edits).await.unwrap_err();
            assert_eq!(e.code(), "music_unreadable", "{name}: {}", e.detail());
        }
        let staged: Vec<_> = std::fs::read_dir(d.path().join(".roomler-partial"))
            .map(|r| r.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(staged.is_empty(), "nothing staged left: {staged:?}");
    }

    /// A piece shorter than 100 ms plays once and is not looped: looping a
    /// few samples would reopen the file thousands of times per second of
    /// export.
    #[tokio::test]
    async fn a_piece_too_short_to_loop_plays_once() {
        let d = dir();
        let source = d.path().join("source.mp4");
        make_source(&source, 60).await;
        write_wav(&d.path().join("blip.wav"), 880.0, 0.05, 16_000.0);
        let mut edits = edit_list(vec![seg(0, 2000, Action::Keep)]);
        edits.music = Some(music("blip.wav"));
        let s = run(&source, &edits).await.unwrap();
        assert_eq!(s.audio, "music");
        let pcm = decode_sound(&s.path).unwrap();
        let blip = rms(&span(&pcm, 0.005, 0.035));
        assert!(blip > 3000.0, "it plays: RMS {blip}");
        let after = rms(&span(&pcm, 0.5, 1.5));
        assert!(after < 100.0, "once, not looped: RMS {after}");
    }
}
