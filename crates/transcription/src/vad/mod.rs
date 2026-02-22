pub mod silero;

pub use silero::SileroVad;

/// Events emitted by the VAD state machine.
#[derive(Debug)]
pub enum VadEvent {
    /// Speech segment started (transition from silence to speech).
    SpeechStart,
    /// Speech segment ended. Contains the complete utterance audio (16kHz mono f32).
    SpeechEnd {
        audio: Vec<f32>,
        /// Duration of the speech segment in seconds.
        duration_secs: f64,
    },
}
