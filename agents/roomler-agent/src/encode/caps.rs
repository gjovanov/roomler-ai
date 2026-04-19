//! Codec capability detection.
//!
//! Probes which video codecs the local host can encode and reports
//! them in the agent's `rc:agent.hello` payload. The result populates
//! `AgentCaps.codecs` (mime-style names like `"h264"`, `"h265"`,
//! `"av1"`) and `AgentCaps.hw_encoders` (descriptive labels like
//! `"mf-h264-hw"`, `"openh264-sw"`).
//!
//! Used by Phase 2 codec negotiation: the controller's browser
//! advertises its `RTCRtpReceiver.getCapabilities('video').codecs`
//! and the agent picks the best intersection.

use roomler_ai_remote_control::models::AgentCaps;

/// Detect the codecs and HW backends compiled into this agent build
/// and currently functional on this host. Cheap one-shot probe; safe
/// to call from `signaling::stub_caps`.
pub fn detect() -> AgentCaps {
    let mut codecs: Vec<String> = Vec::new();
    let mut hw_encoders: Vec<String> = Vec::new();

    #[cfg(feature = "openh264-encoder")]
    {
        codecs.push("h264".into());
        hw_encoders.push("openh264-sw".into());
    }

    #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
    {
        // Probe HW H.264 MFTs via the same MFTEnumEx path the cascade
        // uses. We don't activate them here (that's the cascade's job
        // and doing it twice doubles the COM lifecycle cost); just
        // enumeration is enough to know whether HW H.264 is even
        // installed. Failures (broken MF, no driver) silently return
        // an empty list — the agent still ships SW codec capability.
        if let Ok(adapters) = super::mf::probe_adapter_count()
            && adapters > 0
        {
            hw_encoders.push("mf-h264-hw".into());
            // h264 is already in `codecs` from openh264 above; the
            // hw_encoders list is what flags HW availability.
        }
        // HEVC enumeration. The full HEVC encode pipeline lands in
        // 2C.1 (parallel to mf/sync_pipeline.rs); for capability
        // reporting we just need to know whether the HW MFT exists.
        // Modern Windows + recent IHV drivers ship HEVC encoder MFTs
        // even on iGPUs; this lights up the "H.265 HW" chip in the
        // admin UI today, and the actual encode lights up once 2C.1
        // is wired through encoder selection.
        if let Ok(adapters) = super::mf::probe_hevc_adapter_count()
            && adapters > 0
        {
            codecs.push("h265".into());
            hw_encoders.push("mf-h265-hw".into());
        }
        // AV1 detection lands with 2C.3 (CLSID_MSAV1EncoderMFT,
        // ships on Windows 11 24H2+ with recent NVENC/AMF drivers).
        // Best-effort enumeration via MFTEnumEx with MFVideoFormat_AV1
        // requires a windows-rs constant we don't currently import;
        // wiring deferred until 2C.3 lands the encoder backend.
    }

    AgentCaps {
        hw_encoders,
        codecs,
        has_input_permission: cfg!(feature = "enigo-input"),
        supports_clipboard: false,
        supports_file_transfer: false,
        max_simultaneous_sessions: 1,
    }
}
