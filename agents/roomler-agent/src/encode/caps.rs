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
        // AV1 enumeration: MFTEnumEx with MFVideoFormat_AV1. Windows
        // 11 24H2+ with recent NVIDIA / Intel / AMD drivers exposes
        // HW AV1 MFTs. Enumeration surfacing a candidate doesn't
        // guarantee the cascade will succeed (IHV activation bugs,
        // driver-adapter mismatches) — the encoder opener demotes to
        // HEVC or H.264 if the runtime cascade fails. Advertising AV1
        // here drives the browser's codec preference; if negotiation
        // lands on AV1 and the cascade demotes at runtime the browser
        // sees undecodable bytes, so conservative: advertise only
        // when at least one AV1 MFT enumerates.
        if let Ok(adapters) = super::mf::probe_av1_adapter_count()
            && adapters > 0
        {
            codecs.push("av1".into());
            hw_encoders.push("mf-av1-hw".into());
        }
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

/// Intersection + priority for codec negotiation (Phase 2 2B.2).
/// Takes the browser-advertised codec list + the agent's supported
/// codec list, returns the best codec both sides support.
///
/// Priority order: **av1 > h265 > vp9 > h264 > vp8**. AV1 + HEVC
/// cut 30-50% off the bitrate at equal quality vs H.264; VP9 is
/// closer to H.264 but natively supported in every WebRTC stack so
/// we prefer it over H.264 when available. H.264 is the universal
/// fallback.
///
/// Returns `"h264"` on empty inputs — maintains back-compat with
/// pre-2B.1 browsers that don't advertise anything.
pub fn pick_best_codec(browser_caps: &[String], agent_caps: &[String]) -> String {
    const PRIORITY: &[&str] = &["av1", "h265", "vp9", "h264", "vp8"];
    let browser_has = |c: &str| {
        browser_caps
            .iter()
            .any(|b| b.eq_ignore_ascii_case(c))
    };
    let agent_has = |c: &str| {
        agent_caps
            .iter()
            .any(|a| a.eq_ignore_ascii_case(c))
    };
    for candidate in PRIORITY {
        if browser_has(candidate) && agent_has(candidate) {
            return (*candidate).to_string();
        }
    }
    // Fallback — universal baseline. If the browser advertises nothing
    // (pre-2B.1 controller) we assume it decodes H.264.
    "h264".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_av1_when_both_sides_support() {
        let chosen = pick_best_codec(
            &["h264".into(), "av1".into(), "h265".into()],
            &["h264".into(), "av1".into(), "h265".into()],
        );
        assert_eq!(chosen, "av1");
    }

    #[test]
    fn picks_h265_over_h264_when_browser_lacks_av1() {
        let chosen = pick_best_codec(
            &["h264".into(), "h265".into()],
            &["h264".into(), "av1".into(), "h265".into()],
        );
        assert_eq!(chosen, "h265");
    }

    #[test]
    fn picks_h264_when_only_common_codec() {
        let chosen = pick_best_codec(&["h264".into()], &["h264".into(), "h265".into()]);
        assert_eq!(chosen, "h264");
    }

    #[test]
    fn falls_back_to_h264_on_empty_browser_caps() {
        // Pre-2B.1 controller that doesn't advertise anything.
        let chosen = pick_best_codec(&[], &["h264".into(), "h265".into()]);
        assert_eq!(chosen, "h264");
    }

    #[test]
    fn falls_back_to_h264_on_no_intersection() {
        // Browser advertises only VP8, agent only H.264. No overlap;
        // we return h264 so the caller has a usable default.
        let chosen = pick_best_codec(&["vp8".into()], &["h264".into()]);
        assert_eq!(chosen, "h264");
    }

    #[test]
    fn case_insensitive_match() {
        let chosen = pick_best_codec(&["H264".into(), "H265".into()], &["h265".into()]);
        assert_eq!(chosen, "h265");
    }

    #[test]
    fn prefers_vp9_over_h264() {
        let chosen = pick_best_codec(
            &["h264".into(), "vp9".into()],
            &["h264".into(), "vp9".into()],
        );
        assert_eq!(chosen, "vp9");
    }
}
