// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-85 P5b — an export's sound: the recording's own audio through the edit
//! list, and background music under it.
//!
//! - **The recording's audio follows the video's edit.** A kept stretch plays
//!   the source from the same moment and a cut is gone from both. A speed-up is
//!   MUTED: sped-up sound is noise, and a recording's audio is mostly speech.
//! - **Music** is decoded from the file the person picked with symphonia (pure
//!   Rust: a user-chosen file is untrusted input), streamed, never held whole,
//!   resampled to 48 kHz stereo by the recorder's own resampler, looped or not,
//!   faded, and mixed under the recording's audio through the recorder's soft
//!   clip.
//! - **One Opus track at the recorder's profile** (48 kHz stereo, 20 ms
//!   frames), produced in step with the video so the file interleaves as a
//!   recording does.

use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use super::audio::{CHANNELS, FRAME, RATE, RecordingOpus, Resampler, soft_clip};
use super::edit::{AudioSpan, EditList, Plan};
use super::mp4::{self, ProgressiveFile, ProgressiveSample};
use crate::audio::AudioFrame;

/// What an export's audio carries, for the `done` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carries {
    Original,
    Music,
    OriginalAndMusic,
}

impl Carries {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Music => "music",
            Self::OriginalAndMusic => "original_and_music",
        }
    }
}

/// The recording's own audio, decoded forward on demand.
struct Original {
    pf: ProgressiveFile,
    file: File,
    packets: Vec<ProgressiveSample>,
    next: usize,
    decoder: audiopus::coder::Decoder,
    channels: usize,
    /// Decoded interleaved stereo; `buf[0]` is source sample `buf_start`.
    buf: VecDeque<i16>,
    buf_start: u64,
    /// Decoder output still to drop: the encoder's lookahead (`pre_skip`).
    skip: u64,
    scratch: Vec<i16>,
}

impl Original {
    /// `None` when the recording has no audio track.
    fn open(source: &Path) -> Result<Option<Self>> {
        let pf = ProgressiveFile::open(source)?;
        let Some(format) = pf.audio_format()? else {
            return Ok(None);
        };
        if format.timescale != RATE {
            bail!(
                "the recording's audio runs at {} Hz, not {RATE}",
                format.timescale
            );
        }
        let (channels, layout) = match format.channels {
            1 => (1, audiopus::Channels::Mono),
            2 => (2, audiopus::Channels::Stereo),
            n => bail!("the recording's audio has {n} channels"),
        };
        let packets = pf.samples(mp4::AUDIO_TRACK_ID)?;
        let decoder = audiopus::coder::Decoder::new(audiopus::SampleRate::Hz48000, layout)
            .context("an Opus decoder")?;
        let file = File::open(source).with_context(|| format!("{}", source.display()))?;
        Ok(Some(Self {
            pf,
            file,
            packets,
            next: 0,
            decoder,
            channels,
            buf: VecDeque::new(),
            buf_start: 0,
            skip: u64::from(format.pre_skip),
            // The largest Opus frame is 120 ms.
            scratch: vec![0; 5760 * 2],
        }))
    }

    fn buffered_end(&self) -> u64 {
        self.buf_start + (self.buf.len() / CHANNELS) as u64
    }

    /// Decode one more packet onto the buffer. `false` once there are none.
    fn decode_next(&mut self) -> Result<bool> {
        let Some(p) = self.packets.get(self.next).copied() else {
            return Ok(false);
        };
        self.next += 1;
        let data = self.pf.read_sample(&mut self.file, &p)?;
        let packet: audiopus::packet::Packet<'_> = (&data[..])
            .try_into()
            .map_err(|e| anyhow!("audio packet {}: {e}", self.next - 1))?;
        let out: audiopus::MutSignals<'_, i16> = (&mut self.scratch[..])
            .try_into()
            .map_err(|e| anyhow!("audio buffer: {e}"))?;
        let n = self
            .decoder
            .decode(Some(packet), out, false)
            .map_err(|e| anyhow!("audio packet {}: {e}", self.next - 1))?;
        for i in 0..n {
            if self.skip > 0 {
                self.skip -= 1;
                continue;
            }
            let l = self.scratch[i * self.channels];
            let r = self.scratch[i * self.channels + self.channels - 1];
            self.buf.push_back(l);
            self.buf.push_back(r);
        }
        Ok(true)
    }

    /// Add `n` stereo frames of the source from sample `pos`, times `gain`,
    /// into `out`. Reads only forward; past the end is silence.
    fn add(&mut self, pos: u64, n: usize, gain: f32, out: &mut [i32]) -> Result<()> {
        if pos >= self.buffered_end() {
            self.buf_start = self.buffered_end();
            self.buf.clear();
        }
        while self.buffered_end() < pos + n as u64 {
            if !self.decode_next()? {
                break;
            }
        }
        if pos > self.buf_start {
            let drop = (((pos - self.buf_start) as usize) * CHANNELS).min(self.buf.len());
            self.buf.drain(..drop);
            self.buf_start += (drop / CHANNELS) as u64;
        }
        for (o, s) in out.iter_mut().take(n * CHANNELS).zip(self.buf.iter()) {
            *o += (f32::from(*s) * gain) as i32;
        }
        Ok(())
    }
}

/// The sample rates music is read at. Outside them a file is refused: at
/// 1 Hz every packet would grow 48 000-fold on its way to 48 kHz.
const MUSIC_RATES: std::ops::RangeInclusive<u32> = 1_000..=768_000;

/// A pass through the music shorter than this (100 ms at 48 kHz) is not
/// looped: looping a few samples would reopen the file thousands of times
/// per second of export.
const MIN_LOOP_FRAMES: u64 = 4_800;

/// Run symphonia on the person's file. A malformed file can make it PANIC
/// (0.5.5's probe does on a WAV declaring 0 Hz): that is this file being
/// unreadable, said by name, never the export process dying without an
/// answer. Whatever state the panic left is dropped with the export.
fn guarded<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        Err(anyhow!(
            "{}: the music decoder failed on this file",
            path.display()
        ))
    })
}

/// A music file, decoded as it is needed.
struct Reader {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track: u32,
}

/// What one read of the music file gave.
enum Read {
    /// The end of the file.
    End,
    /// Nothing to play: another track's packet, a damaged one, or no samples.
    Nothing,
    Audio(AudioFrame),
}

fn open_reader(path: &Path) -> Result<Reader> {
    guarded(path, || open_reader_unguarded(path))
}

fn open_reader_unguarded(path: &Path) -> Result<Reader> {
    let file = File::open(path).with_context(|| format!("{}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| {
            anyhow!(
                "{} is not a music file this build reads: {e}",
                path.display()
            )
        })?;
    let format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("{} has no audio", path.display()))?;
    // Said before the export starts when the header declares it; `fill`
    // checks every buffer as well, for a format that declares nothing.
    if let Some(rate) = track.codec_params.sample_rate
        && !MUSIC_RATES.contains(&rate)
    {
        bail!(
            "{}: a sample rate of {rate} Hz is not one music is read at",
            path.display()
        );
    }
    let decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| anyhow!("{}: {e}", path.display()))?;
    Ok(Reader {
        track: track.id,
        format,
        decoder,
    })
}

/// One packet of the music file, decoded. Called through [`guarded`].
fn read_one(r: &mut Reader, path: &Path) -> Result<Read> {
    let packet = match r.format.next_packet() {
        Ok(p) => p,
        Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok(Read::End);
        }
        Err(SymphoniaError::ResetRequired) => return Ok(Read::End),
        Err(e) => bail!("{}: {e}", path.display()),
    };
    if packet.track_id() != r.track {
        return Ok(Read::Nothing);
    }
    let decoded = match r.decoder.decode(&packet) {
        Ok(d) => d,
        // One damaged packet costs its own 20-odd ms, not the export.
        Err(SymphoniaError::DecodeError(_)) => return Ok(Read::Nothing),
        Err(e) => bail!("{}: {e}", path.display()),
    };
    let spec = *decoded.spec();
    let mut samples = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
    samples.copy_interleaved_ref(decoded);
    if samples.samples().is_empty() {
        return Ok(Read::Nothing);
    }
    Ok(Read::Audio(AudioFrame {
        samples: samples.samples().to_vec(),
        channels: spec.channels.count().max(1) as u16,
        sample_rate: spec.rate,
    }))
}

/// Background music, streamed: 48 kHz stereo on demand, from the top again
/// when it loops, silence after it when it does not.
struct MusicTrack {
    path: PathBuf,
    looped: bool,
    reader: Option<Reader>,
    /// Made at the first decoded buffer, at that buffer's rate.
    resampler: Option<(u32, Resampler)>,
    q: VecDeque<i16>,
    /// Output frames this pass produced: a pass shorter than
    /// [`MIN_LOOP_FRAMES`] (a file that decodes to nothing, or to a blip) is
    /// not looped.
    pass_frames: u64,
}

impl MusicTrack {
    fn open(path: &Path, looped: bool) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            looped,
            reader: Some(open_reader(path)?),
            resampler: None,
            q: VecDeque::new(),
            pass_frames: 0,
        })
    }

    fn end_of_pass(&mut self) -> Result<()> {
        if self.looped && self.pass_frames >= MIN_LOOP_FRAMES {
            self.reader = Some(open_reader(&self.path)?);
            self.resampler = None;
            self.pass_frames = 0;
        } else {
            self.reader = None;
        }
        Ok(())
    }

    /// Decode one packet onto the queue.
    fn fill(&mut self) -> Result<()> {
        let Some(r) = self.reader.as_mut() else {
            return Ok(());
        };
        let path = &self.path;
        let frame = match guarded(path, || read_one(r, path))? {
            Read::End => return self.end_of_pass(),
            Read::Nothing => return Ok(()),
            Read::Audio(frame) => frame,
        };
        let rate = frame.sample_rate;
        if !MUSIC_RATES.contains(&rate) {
            bail!(
                "{}: a sample rate of {rate} Hz is not one music is read at",
                self.path.display()
            );
        }
        let before = self.q.len();
        let resampler = match &mut self.resampler {
            Some((at, rs)) if *at == rate => rs,
            slot => &mut slot.insert((rate, Resampler::new(rate))).1,
        };
        resampler.push(&frame, 0.0, &mut self.q);
        self.pass_frames += ((self.q.len() - before) / CHANNELS) as u64;
        Ok(())
    }

    /// `n` stereo frames into `out` (cleared first); silence past the end.
    fn pull(&mut self, n: usize, out: &mut Vec<i16>) -> Result<()> {
        while self.q.len() < n * CHANNELS && self.reader.is_some() {
            self.fill()?;
        }
        out.clear();
        let take = (n * CHANNELS).min(self.q.len());
        out.extend(self.q.drain(..take));
        out.resize(n * CHANNELS, 0);
        Ok(())
    }
}

/// How the music sits in the export, in output samples.
struct Shape {
    volume: f32,
    start: u64,
    fade_in: u64,
    fade_out: u64,
    end: u64,
}

impl Shape {
    /// The music's gain at output sample `s`.
    fn gain(&self, s: u64) -> f32 {
        if s < self.start || s >= self.end {
            return 0.0;
        }
        let mut g = self.volume;
        if self.fade_in > 0 && s < self.start + self.fade_in {
            g *= (s - self.start) as f32 / self.fade_in as f32;
        }
        if self.fade_out > 0 && s + self.fade_out > self.end {
            g *= (self.end - s) as f32 / self.fade_out as f32;
        }
        g
    }
}

/// The export's audio: produced 20 ms at a time in step with the video.
pub struct ExportAudio {
    spans: Vec<AudioSpan>,
    total: u64,
    original: Option<Original>,
    original_gain: f32,
    music: Option<(MusicTrack, Shape)>,
    opus: RecordingOpus,
    produced: u64,
    acc: Vec<i32>,
    frame: Vec<i16>,
    music_buf: Vec<i16>,
}

/// Why an export's sound could not be made, named by the part that failed:
/// a full disk must not reach the person as a damaged recording.
#[derive(Debug)]
pub enum AudioError {
    /// The music file does not open or decode.
    Music(String),
    /// The recording's own audio does not decode.
    Original(String),
    /// The Opus encoder refused.
    Encode(String),
    /// A packet could not be written.
    Write(String),
}

/// Milliseconds as samples at the export's rate. The music's times are the
/// person's to write, so a huge one saturates (it means "never") instead of
/// wrapping to an early start.
fn samples_in(ms: u64) -> u64 {
    ms.saturating_mul(u64::from(RATE)) / 1000
}

impl ExportAudio {
    /// `Ok(None)` when the export has no sound: the recording has no audio
    /// (or it is at volume 0) and there is no music.
    pub fn new(source: &Path, plan: &Plan, edits: &EditList) -> Result<Option<Self>, AudioError> {
        let original_gain = edits.original_volume.unwrap_or(1.0);
        let original = if original_gain > 0.0 {
            Original::open(source).map_err(|e| AudioError::Original(format!("{e:#}")))?
        } else {
            None
        };
        let total = plan.out_samples(RATE);
        let music = match &edits.music {
            Some(m) => {
                let path = source
                    .parent()
                    .map(|d| d.join(&m.path))
                    .unwrap_or_else(|| PathBuf::from(&m.path));
                let track = MusicTrack::open(&path, m.looped)
                    .map_err(|e| AudioError::Music(format!("{e:#}")))?;
                let shape = Shape {
                    volume: m.volume,
                    start: samples_in(m.start_ms).min(total),
                    fade_in: samples_in(m.fade_in_ms),
                    fade_out: samples_in(m.fade_out_ms),
                    end: total,
                };
                (m.volume > 0.0).then_some((track, shape))
            }
            None => None,
        };
        if original.is_none() && music.is_none() {
            return Ok(None);
        }
        let opus = RecordingOpus::new().map_err(|e| AudioError::Encode(format!("{e:#}")))?;
        Ok(Some(Self {
            spans: plan.audio_spans(RATE),
            total,
            original,
            original_gain,
            music,
            opus,
            produced: 0,
            acc: vec![0; FRAME * CHANNELS],
            frame: vec![0; FRAME * CHANNELS],
            music_buf: Vec::with_capacity(FRAME * CHANNELS),
        }))
    }

    pub fn carries(&self) -> Carries {
        match (self.original.is_some(), self.music.is_some()) {
            (true, true) => Carries::OriginalAndMusic,
            (false, true) => Carries::Music,
            _ => Carries::Original,
        }
    }

    /// What a decoder drops from the start (the encoder's lookahead).
    pub fn pre_skip(&self) -> u16 {
        self.opus.pre_skip
    }

    /// Encode every 20 ms frame that STARTS before `until` output samples
    /// (capped at the export's end), handing each Opus packet to `sink`.
    pub fn produce_until(
        &mut self,
        until: u64,
        sink: &mut dyn FnMut(Vec<u8>) -> Result<()>,
    ) -> Result<(), AudioError> {
        let until = until.min(self.total);
        while self.produced < until {
            self.mix_one()?;
            let packet = self
                .opus
                .encode(&self.frame)
                .map_err(|e| AudioError::Encode(format!("{e:#}")))?;
            sink(packet).map_err(|e| AudioError::Write(format!("{e:#}")))?;
            self.produced += FRAME as u64;
        }
        Ok(())
    }

    /// Everything to the export's end; the last frame is padded with silence.
    pub fn finish(
        &mut self,
        sink: &mut dyn FnMut(Vec<u8>) -> Result<()>,
    ) -> Result<(), AudioError> {
        self.produce_until(self.total, sink)
    }

    fn mix_one(&mut self) -> Result<(), AudioError> {
        self.acc.fill(0);
        let s0 = self.produced;
        let end = (s0 + FRAME as u64).min(self.total);
        if let Some(original) = self.original.as_mut() {
            let mut s = s0;
            while s < end {
                let Some(span) = self
                    .spans
                    .iter()
                    .find(|sp| s >= sp.out_start && s < sp.out_end)
                    .copied()
                else {
                    break;
                };
                let run = (end.min(span.out_end) - s) as usize;
                if let Some(src) = span.src_start {
                    let at = ((s - s0) as usize) * CHANNELS;
                    original
                        .add(
                            src + (s - span.out_start),
                            run,
                            self.original_gain,
                            &mut self.acc[at..],
                        )
                        .map_err(|e| AudioError::Original(format!("{e:#}")))?;
                }
                s += run as u64;
            }
        }
        if let Some((track, shape)) = self.music.as_mut()
            && end > shape.start
        {
            let from = s0.max(shape.start);
            let n = (end - from) as usize;
            track
                .pull(n, &mut self.music_buf)
                .map_err(|e| AudioError::Music(format!("{e:#}")))?;
            let at = ((from - s0) as usize) * CHANNELS;
            for k in 0..n {
                let g = shape.gain(from + k as u64);
                for c in 0..CHANNELS {
                    self.acc[at + k * CHANNELS + c] +=
                        (f32::from(self.music_buf[k * CHANNELS + c]) * g) as i32;
                }
            }
        }
        for (o, a) in self.frame.iter_mut().zip(&self.acc) {
            *o = soft_clip(*a);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_fades_in_from_its_start_and_out_at_the_exports_end() {
        let s = Shape {
            volume: 0.5,
            start: 1000,
            fade_in: 1000,
            fade_out: 1000,
            end: 10_000,
        };
        assert_eq!(s.gain(0), 0.0, "before it starts");
        assert_eq!(s.gain(1000), 0.0, "the fade starts at zero");
        assert!((s.gain(1500) - 0.25).abs() < 1e-6);
        assert!((s.gain(5000) - 0.5).abs() < 1e-6, "full volume between");
        assert!((s.gain(9500) - 0.25).abs() < 1e-6);
        assert_eq!(s.gain(10_000), 0.0, "past the end");
    }

    #[test]
    fn a_huge_music_time_saturates_instead_of_wrapping() {
        assert_eq!(samples_in(1000), 48_000);
        assert_eq!(samples_in(0), 0);
        // u64::MAX ms times 48 kHz wraps (or panics) without the saturation;
        // saturated, it means "later than any export".
        assert_eq!(samples_in(u64::MAX), u64::MAX / 1000);
        assert!(samples_in(u64::MAX / 2) > samples_in(10_000));
    }
}
