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
        // HEVC / AV1 codec detection lands with the corresponding
        // backend (2C.1 / 2C.3). Today both probe-and-fail cleanly so
        // no stub entries here.
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
