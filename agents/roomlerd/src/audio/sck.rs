// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-97 P2a — a Mac's system audio through ScreenCaptureKit.
//!
//! A session's audio track and a recording's "computer audio" take
//! [`SckAudioCapture`] on macOS, the way they take Pulse's monitor on Linux and
//! WASAPI loopback on Windows ([`super::open_default`]). An `SCStream` over the
//! main display with `capturesAudio` on hands an `SCStreamOutput` the system's
//! mix (every app's sound, this process's own excluded) as `CMSampleBuffer`s on
//! a private dispatch queue. Each becomes one [`AudioFrame`] on a bounded
//! channel the pump awaits.
//!
//! Measured on a tart Mac before it was written (FR-97 §8):
//! - **The format:** 48 kHz, two channels, 32-bit float, *non-interleaved*,
//!   960 frames a buffer, 50 buffers a second. That is Opus's own 20 ms
//!   frame, so nothing is resampled; [`super::planar_f32_to_i16`] interleaves.
//! - **The mix, not the device:** it heard two system sounds with the Mac's
//!   output **muted**. The lab's VMs pass their sound through to the host,
//!   so its cells run muted.
//! - **A window-server session is required.** The root daemon of a supervised
//!   Mac is in none and cannot capture; its GUI worker does, under the Screen
//!   Recording grant it already holds for the picture.
//! - **A stream must have a picture size even when only sound is wanted.**
//!   A 2×2 picture at one frame a second, never read, costs nothing
//!   measurable.

use std::sync::Mutex;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime,
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};
use tokio::sync::mpsc;

use super::{AudioCapture, AudioFrame};
use crate::audio_source::{AudioUnavailable, AudioUnavailableCode};

// The block buffer CoreMedia retains for us, released once its samples are
// copied. One symbol: not worth a CoreFoundation crate dependency.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const std::ffi::c_void);
}

/// The capture's rate and channel count: what the probe measured SCK to
/// deliver, and what the Opus encoder takes without resampling.
const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// About a second of 20 ms frames. A pump that falls further behind loses the
/// oldest sound rather than growing a backlog it will never play.
const QUEUE: usize = 50;
/// How long the shareable-content query and the stream's start may take.
const ANSWER_WAIT: Duration = Duration::from_secs(5);

/// What the output object holds: the channel's sending end, taken when the
/// stream stops so the pump's `recv` ends instead of waiting forever.
struct OutputIvars {
    tx: Mutex<Option<mpsc::Sender<AudioFrame>>>,
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `AudioOutput` does not implement `Drop`.
    // - Its ivars are `Send + Sync` (a mutex around a tokio sender), as the
    //   stream calls it from its own dispatch queue.
    #[unsafe(super(NSObject))]
    #[name = "RoomlerSckAudioOutput"]
    #[ivars = OutputIvars]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Audio {
                return;
            }
            let Some(frame) = frame_of(sample_buffer) else {
                return;
            };
            if let Some(tx) = self.ivars().tx.lock().ok().and_then(|g| g.clone()) {
                // Full: the pump is behind; this frame is dropped, not queued.
                let _ = tx.try_send(frame);
            }
        }
    }

    unsafe impl SCStreamDelegate for AudioOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            tracing::warn!(
                error = %error.localizedDescription(),
                code = error.code(),
                "audio: the ScreenCaptureKit stream stopped"
            );
            if let Ok(mut tx) = self.ivars().tx.lock() {
                tx.take();
            }
        }
    }
);

impl AudioOutput {
    fn new(tx: mpsc::Sender<AudioFrame>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars {
            tx: Mutex::new(Some(tx)),
        });
        // SAFETY: NSObject's designated initialiser, on a freshly allocated
        // instance whose ivars are set.
        unsafe { msg_send![super(this), init] }
    }
}

/// One `CMSampleBuffer` of the stream's audio as an [`AudioFrame`]: 16-bit,
/// interleaved, at the buffer's own rate and channel count. `None` for a
/// buffer this capture cannot read, which is only logged once a format is
/// known not to be the measured one.
fn frame_of(sb: &CMSampleBuffer) -> Option<AudioFrame> {
    // SAFETY: CoreMedia's documented calls on a live sample buffer the stream
    // lends for the duration of the callback. The buffer list is sized by
    // CoreMedia's own first answer and 8-byte aligned (`u64` storage); the
    // block buffer that owns the samples is released after they are copied.
    unsafe {
        let fd = sb.format_description()?;
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&fd);
        if asbd.is_null() {
            return None;
        }
        let asbd = &*asbd;
        let channels = asbd.mChannelsPerFrame as usize;
        let float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
        let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
        if !float || asbd.mBitsPerChannel != 32 || !(1..=8).contains(&channels) {
            unexpected_format_once(asbd.mFormatFlags, asbd.mBitsPerChannel, channels);
            return None;
        }
        let mut need = 0usize;
        sb.audio_buffer_list_with_retained_block_buffer(
            &mut need,
            std::ptr::null_mut(),
            0,
            None,
            None,
            0,
            std::ptr::null_mut(),
        );
        let need = need.max(std::mem::size_of::<AudioBufferList>());
        let mut storage = vec![0u64; need.div_ceil(8)];
        let list = storage.as_mut_ptr().cast::<AudioBufferList>();
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        let status = sb.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            list,
            need,
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &mut block,
        );
        /// One buffer's samples, borrowed from the block buffer that owns them.
        ///
        /// # Safety
        /// `b` describes `mDataByteSize` readable bytes of 32-bit floats at
        /// `mData` (or a null `mData`), alive for `'a`.
        unsafe fn floats<'a>(b: &'a AudioBuffer) -> &'a [f32] {
            if b.mData.is_null() {
                &[]
            } else {
                // SAFETY: the caller's contract.
                unsafe {
                    std::slice::from_raw_parts(
                        b.mData.cast::<f32>(),
                        b.mDataByteSize as usize / std::mem::size_of::<f32>(),
                    )
                }
            }
        }
        let frame = (status == 0).then(|| {
            let n = (*list).mNumberBuffers as usize;
            let buffers: &[AudioBuffer] = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), n);
            let samples = if planar {
                let planes: Vec<&[f32]> = buffers.iter().map(|b| floats(b)).collect();
                super::planar_f32_to_i16(&planes)
            } else {
                buffers
                    .first()
                    .map(|b| floats(b).iter().map(|&x| super::f32_to_i16(x)).collect())
                    .unwrap_or_default()
            };
            AudioFrame {
                samples,
                channels: if planar { n as u16 } else { channels as u16 },
                sample_rate: asbd.mSampleRate as u32,
            }
        });
        if !block.is_null() {
            CFRelease(block.cast_const().cast());
        }
        frame.filter(|f| !f.samples.is_empty() && f.channels > 0)
    }
}

/// Say once, not 50 times a second, that the stream's audio is not the format
/// this capture reads.
fn unexpected_format_once(flags: u32, bits: u32, channels: usize) {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        tracing::warn!(
            flags,
            bits,
            channels,
            "audio: ScreenCaptureKit delivered a format this capture does not read (it expects 32-bit float)"
        );
    });
}

/// A Mac's system audio for one session or one recording. Dropping it stops
/// the stream.
pub struct SckAudioCapture {
    rx: mpsc::Receiver<AudioFrame>,
    stream: Retained<SCStream>,
    _output: Retained<AudioOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: the stream is touched from one place after `open` returns — its
// `stopCaptureWithCompletionHandler:` in `Drop` — and ScreenCaptureKit's
// stream methods may be called from any thread. The output object's state is
// a mutex.
unsafe impl Send for SckAudioCapture {}

impl SckAudioCapture {
    /// Open the main display's audio. Refuses with a reason the controller
    /// reads: no window-server session (a supervised Mac's root half), no
    /// display, or the stream's own error (the Screen Recording grant above
    /// all).
    pub fn open() -> Result<Self, AudioUnavailable> {
        if !crate::tcc::has_gui_session() {
            return Err(AudioUnavailable::new(
                AudioUnavailableCode::NoSession,
                "this process has no window-server session (a Mac's root daemon); the GUI worker in \
                 the person's login session captures the sound",
            ));
        }
        let display = main_display()?;
        // SAFETY: the documented initialisers and setters, on objects this
        // function owns, with values in their documented ranges.
        let (filter, config) = unsafe {
            let excluded = NSArray::new();
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &excluded,
            );
            let config = SCStreamConfiguration::new();
            config.setCapturesAudio(true);
            config.setExcludesCurrentProcessAudio(true);
            config.setSampleRate(RATE as isize);
            config.setChannelCount(CHANNELS as isize);
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
            config.setQueueDepth(3);
            (filter, config)
        };
        let (tx, rx) = mpsc::channel(QUEUE);
        let output = AudioOutput::new(tx);
        let queue = DispatchQueue::new("ai.roomler.sck-audio", None);
        // SAFETY: as above; the delegate and the output are this one object,
        // kept alive by `Self` for as long as the stream is.
        let stream = unsafe {
            let delegate: &ProtocolObject<dyn SCStreamDelegate> =
                ProtocolObject::from_ref(&*output);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(delegate),
            );
            let sink: &ProtocolObject<dyn SCStreamOutput> = ProtocolObject::from_ref(&*output);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    sink,
                    SCStreamOutputType::Audio,
                    Some(&*queue),
                )
                .map_err(|e| backend("adding the audio output", &e))?;
            stream
        };
        let (done_tx, done_rx) = std_mpsc::channel::<Option<String>>();
        let started = RcBlock::new(move |err: *mut NSError| {
            // SAFETY: the stream hands a valid NSError or null.
            let said = unsafe { err.as_ref() }.map(describe);
            let _ = done_tx.send(said);
        });
        // SAFETY: a live stream and a block that outlives the call (it is
        // reference-counted and retained by ScreenCaptureKit until it runs).
        unsafe { stream.startCaptureWithCompletionHandler(Some(&*started)) };
        match done_rx.recv_timeout(ANSWER_WAIT) {
            Ok(None) => {}
            Ok(Some(why)) => {
                return Err(AudioUnavailable::new(
                    AudioUnavailableCode::BackendError,
                    format!("ScreenCaptureKit did not start: {why}"),
                ));
            }
            Err(_) => {
                return Err(AudioUnavailable::new(
                    AudioUnavailableCode::BackendError,
                    "ScreenCaptureKit did not answer the start within 5 s",
                ));
            }
        }
        tracing::info!(
            rate = RATE,
            channels = CHANNELS,
            "audio: ScreenCaptureKit system audio started"
        );
        Ok(Self {
            rx,
            stream,
            _output: output,
            _queue: queue,
        })
    }
}

impl Drop for SckAudioCapture {
    fn drop(&mut self) {
        // SAFETY: a live stream; a `None` handler is allowed.
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

#[async_trait]
impl AudioCapture for SckAudioCapture {
    async fn next_frame(&mut self) -> anyhow::Result<Option<AudioFrame>> {
        Ok(self.rx.recv().await)
    }
}

/// The main display, from ScreenCaptureKit's shareable content. Its refusal
/// is the first place a missing Screen Recording grant shows.
fn main_display() -> Result<Retained<objc2_screen_capture_kit::SCDisplay>, AudioUnavailable> {
    struct Answer(Option<Retained<SCShareableContent>>, Option<String>);
    // SAFETY: the content object crosses threads only to be read here; it is
    // immutable.
    unsafe impl Send for Answer {}
    let (tx, rx) = std_mpsc::channel::<Answer>();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        // SAFETY: ScreenCaptureKit hands valid objects or null; `retain`
        // takes our own reference before the call returns.
        let content = unsafe { Retained::retain(content) };
        let said = unsafe { err.as_ref() }.map(describe);
        let _ = tx.send(Answer(content, said));
    });
    // SAFETY: a class method with a block ScreenCaptureKit retains until it
    // runs.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, true, &handler,
        )
    };
    let Answer(content, said) = rx.recv_timeout(ANSWER_WAIT).map_err(|_| {
        AudioUnavailable::new(
            AudioUnavailableCode::BackendError,
            "ScreenCaptureKit did not list the displays within 5 s",
        )
    })?;
    let Some(content) = content else {
        return Err(AudioUnavailable::new(
            AudioUnavailableCode::BackendError,
            format!(
                "ScreenCaptureKit refused to list the displays: {} (the Screen Recording grant, \
                 System Settings → Privacy & Security → Screen Recording)",
                said.unwrap_or_else(|| "no reason given".into())
            ),
        ));
    };
    // SAFETY: a getter on a live content object.
    unsafe { content.displays() }.firstObject().ok_or_else(|| {
        AudioUnavailable::new(
            AudioUnavailableCode::BackendError,
            "ScreenCaptureKit lists no display",
        )
    })
}

fn describe(e: &NSError) -> String {
    format!("{} (code {})", e.localizedDescription(), e.code())
}

fn backend(what: &str, e: &NSError) -> AudioUnavailable {
    AudioUnavailable::new(
        AudioUnavailableCode::BackendError,
        format!("ScreenCaptureKit, {what}: {}", describe(e)),
    )
}
