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
//!
//! Detection is **probe-gated** for codecs without a safe demotion
//! path (HEVC, AV1): we actually run a tiny MfEncoder::new at startup
//! and only advertise codecs that successfully activate. This closes
//! the "enumerates but won't activate" false-advertising gap (e.g.
//! NVIDIA RTX 5090 Blackwell where the AV1 MFT enumerates but every
//! `ActivateObject` returns 0x8000FFFF). Without this guard a browser
//! session could negotiate AV1, the pump's runtime cascade would fail,
//! and the fail-closed NoopEncoder would leave the browser with a
//! black screen. The probe result is cached behind a `OnceLock` so
//! the ~300ms / codec init cost runs once per agent process, not per
//! `rc:agent.hello`.

use roomler_ai_remote_control::models::AgentCaps;
use std::sync::OnceLock;

static CACHED_CAPS: OnceLock<AgentCaps> = OnceLock::new();

/// Probe dimensions for codec activation checks (HEVC, AV1, VP9-444).
/// Even number, small enough that any encoder accepts it, matching
/// what the internal `probe_pipeline` uses for MFT output
/// verification. Used by the MF cascade probes (Windows-only,
/// `mf-encoder` feature) and the libvpx VP9-444 probe (any platform
/// with `vp9-444` feature). The `dead_code` allowance covers builds
/// that compile in neither feature group.
#[allow(dead_code)]
const PROBE_WIDTH: u32 = 480;
#[allow(dead_code)]
const PROBE_HEIGHT: u32 = 270;

/// Detect the codecs and HW backends compiled into this agent build
/// and currently functional on this host. First call runs the
/// activation probes (~300ms per codec on HEVC/AV1-capable boxes,
/// <10ms on boxes with no HW encoder); subsequent calls return the
/// cached result.
pub fn detect() -> AgentCaps {
    CACHED_CAPS.get_or_init(compute_caps).clone()
}

fn compute_caps() -> AgentCaps {
    // `mut` is only consumed inside the cfg-gated push blocks below
    // (openh264-encoder / mf-encoder). Default-feature builds skip
    // both blocks and the vecs stay empty; silence the unused-mut
    // lint to keep the CI `cargo clippy --workspace -- -D warnings`
    // build green on Linux.
    #[allow(unused_mut)]
    let mut codecs: Vec<String> = Vec::new();
    #[allow(unused_mut)]
    let mut hw_encoders: Vec<String> = Vec::new();

    #[cfg(feature = "openh264-encoder")]
    {
        codecs.push("h264".into());
        hw_encoders.push("openh264-sw".into());
    }

    #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
    {
        // H.264: enumeration is sufficient. If any H.264 MFT
        // enumerates the cascade always succeeds (at worst it falls
        // through to the default-adapter SW MFT via
        // CLSID_MSH264EncoderMFT); runtime activation failure would
        // be caught by open_default's triple-fallback (MF → openh264
        // → Noop). No probe needed.
        if let Ok(adapters) = super::mf::probe_adapter_count()
            && adapters > 0
        {
            hw_encoders.push("mf-h264-hw".into());
        }

        let allow_sw = allow_sw_heavy_override();
        let advertise = |r: ProbeResult| -> bool {
            matches!(r, ProbeResult::Hardware)
                || (allow_sw && matches!(r, ProbeResult::SoftwareOnly))
        };

        // HEVC: enumeration + real activation probe. MFTs that
        // enumerate but fail ActivateObject (driver/adapter
        // mismatches, missing HEVC Video Extension) would poison a
        // negotiated session — the track is bound to video/HEVC
        // before the encoder opens, so failure means black video not
        // fallback-decode. Gate advertising on a successful HW probe;
        // SW-only paths are dropped so H.264 wins negotiation
        // (mediasoup-screenshare-grade quality on iGPU hosts).
        if let Ok(adapters) = super::mf::probe_hevc_adapter_count()
            && adapters > 0
        {
            let probe = activates(CodecProbe::Hevc);
            if advertise(probe) {
                codecs.push("h265".into());
                hw_encoders.push("mf-h265-hw".into());
            }
        }

        // AV1: same reasoning as HEVC, with sharper impact — the
        // RTX 5090 Blackwell regression causes the NVIDIA AV1 MFT to
        // enumerate-and-fail on every activation on dev hardware
        // (see `Known Issues` in CLAUDE.md). Probe-at-startup
        // filters this out so the agent doesn't advertise a codec
        // it can't actually produce.
        if let Ok(adapters) = super::mf::probe_av1_adapter_count()
            && adapters > 0
        {
            let probe = activates(CodecProbe::Av1);
            if advertise(probe) {
                codecs.push("av1".into());
                hw_encoders.push("mf-av1-hw".into());
            }
        }
    }

    #[allow(unused_mut)]
    let mut transports: Vec<String> = Vec::new();

    // rc.77 — FFmpeg HEVC over DataChannel.
    //
    // Gated behind `ROOMLER_AGENT_USE_FFMPEG=1` env var so that an
    // accidental FFmpeg dep in the build doesn't change negotiation
    // for existing field sessions. When the env var IS set, probe
    // `FfmpegEncoder::new_hevc` at the standard probe resolution; on
    // success advertise both the `h265` codec (additive to whatever
    // MF found) and the `data-channel-hevc` transport. The browser's
    // rc:session.request can then ask for codec=h265 + transport=
    // data-channel-hevc and `peer.rs::media_pump` will route to the
    // HEVC DC pump.
    //
    // Pre-flight WebCodecs spike (2026-05-26) confirmed Chrome + Edge
    // decode Annex-B no-description HEVC. Gate 0 smoke (2026-05-29)
    // confirmed hevc_qsv works on Iris Xe Tiger Lake AND hevc_nvenc
    // works on RTX 5090 Blackwell — the two boxes MF was broken on.
    #[cfg(feature = "ffmpeg-encoder")]
    if crate::encode::ffmpeg::available() {
        // `name()` is on the `VideoEncoder` trait — need the trait in
        // scope at the call site for method-resolution.
        use super::VideoEncoder;

        // rc.83 — probe vp9_qsv to surface in caps + heartbeat whether
        // this host can use Intel HW VP9. The transport advertisement
        // (`data-channel-vp9-444`) stays gated on the libvpx probe
        // above — both encoder paths emit the same VP9 bitstream that
        // the same browser worker decodes; only the encoder source
        // differs. The runtime peer.rs dispatch picks vp9_qsv at
        // session-establish time when this probe passed AND the host
        // didn't request 4:4:4 chroma (which vp9_qsv doesn't support).
        {
            let start_vp9 = std::time::Instant::now();
            match crate::encode::ffmpeg::FfmpegEncoder::new_vp9(PROBE_WIDTH, PROBE_HEIGHT) {
                Ok(enc) => {
                    let name = enc.name();
                    drop(enc);
                    tracing::info!(
                        encoder = name,
                        elapsed_ms = start_vp9.elapsed().as_millis(),
                        "caps probe: ffmpeg VP9 (vp9_qsv) encoder activates — runtime peer dispatch will prefer it over libvpx SW on data-channel-vp9-444 sessions"
                    );
                    hw_encoders.push(format!("ffmpeg-{name}"));
                }
                Err(e) => {
                    tracing::info!(
                        %e,
                        elapsed_ms = start_vp9.elapsed().as_millis(),
                        "caps probe: ffmpeg vp9_qsv not available (NVIDIA/AMD host, Intel without QSV, or Intel driver issue) — VP9 sessions stay on libvpx SW"
                    );
                }
            }
        }

        let start = std::time::Instant::now();
        match crate::encode::ffmpeg::FfmpegEncoder::new_hevc(PROBE_WIDTH, PROBE_HEIGHT) {
            Ok(enc) => {
                let name = enc.name();
                drop(enc);
                tracing::info!(
                    encoder = name,
                    elapsed_ms = start.elapsed().as_millis(),
                    "caps probe: ffmpeg HEVC encoder activates — advertising h265 + data-channel-hevc"
                );
                if !codecs.iter().any(|c| c == "h265") {
                    codecs.push("h265".into());
                }
                transports.push("data-channel-hevc".into());
                hw_encoders.push(format!("ffmpeg-{name}"));
            }
            Err(e) => {
                tracing::warn!(
                    %e,
                    elapsed_ms = start.elapsed().as_millis(),
                    "caps probe: ffmpeg HEVC encoder failed to init — NOT advertising data-channel-hevc"
                );
            }
        }
    }

    #[cfg(feature = "vp9-444")]
    {
        // Phase Y.4 caps probe (Y.runtime-encoder rewrite landed,
        // 0.1.47). Try to instantiate the libvpx encoder at a probe
        // resolution; on success advertise both the transport and
        // the encoder label, on failure stay silent so no session
        // ever negotiates onto a broken path. The probe runs once
        // per agent process via the OnceLock cache.
        let start = std::time::Instant::now();
        match crate::encode::libvpx::Vp9Encoder::new(PROBE_WIDTH, PROBE_HEIGHT) {
            Ok(enc) => {
                drop(enc);
                tracing::info!(
                    elapsed_ms = start.elapsed().as_millis(),
                    "caps probe: vp9-444 libvpx encoder activates — advertising data-channel-vp9-444 transport"
                );
                transports.push("data-channel-vp9-444".into());
                hw_encoders.push("libvpx-vp9-444-sw".into());
            }
            Err(e) => {
                tracing::warn!(
                    %e,
                    elapsed_ms = start.elapsed().as_millis(),
                    "caps probe: vp9-444 libvpx encoder failed to init — NOT advertising"
                );
            }
        }
    }

    // File-DC v2 capability list. Always advertise upload + download
    // + download-folder (always built in this agent). `browse` is
    // gated on the runtime `enable_remote_browse` flag so old
    // browsers that see an empty `files` array fall back to
    // `supports_file_transfer` (upload-only) and new browsers
    // grey out the drawer button when the host has browse disabled.
    //
    // File-DC v3 (rc.19) adds `resume` — the agent stages uploads
    // under `<dest_dir>/.roomler-partial/<id>/` and can resume a
    // mid-flight transfer after a DC drop (auto-update mid-upload,
    // network blip, agent crash). Browsers that don't see `resume`
    // fall back to the rc.18 fail-fast path.
    let mut files = vec![
        "upload".to_string(),
        "download".to_string(),
        "download-folder".to_string(),
        "resume".to_string(),
    ];
    if crate::files::is_remote_browse_enabled() {
        files.push("browse".to_string());
    }

    // rc.61 — surface VP9 chroma format in caps so the browser worker
    // picks the right codec string for VideoDecoder.configure(). Empty
    // when vp9-444 transport isn't advertised (so we don't lie to the
    // client about a format we don't emit).
    let vp9_chroma: String = if transports.iter().any(|t| t == "data-channel-vp9-444") {
        #[cfg(feature = "vp9-444")]
        {
            crate::encode::libvpx::vp9_chroma_from_env()
                .as_str()
                .to_string()
        }
        #[cfg(not(feature = "vp9-444"))]
        {
            String::new()
        }
    } else {
        String::new()
    };

    AgentCaps {
        hw_encoders,
        codecs,
        has_input_permission: cfg!(feature = "enigo-input"),
        supports_clipboard: cfg!(feature = "clipboard"),
        supports_file_transfer: true,
        max_simultaneous_sessions: 1,
        transports,
        files,
        vp9_chroma,
    }
}

/// Codec to probe. We only probe codecs that fail closed on activation
/// error (HEVC + AV1 today); H.264 has a working triple-fallback path
/// and is not gated.
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
#[derive(Debug, Clone, Copy)]
enum CodecProbe {
    Hevc,
    Av1,
}

/// Outcome of a codec probe. We split SW from HW because shipping
/// HEVC over the SW MFT (`HEVCVideoExtensionEncoder`) is a UX
/// regression vs negotiating H.264 with the host's HW H.264 path
/// (Intel QuickSync, NVENC, AMF). Two reasons: chroma artefacts at
/// low bitrate, and roughly 3x CPU cost vs HW H.264. Field reports
/// 2026-04-24 and 2026-04-26 from boxes where the IHV HEVC MFT
/// (Intel Hardware H265 Encoder MFT) fails ActivateObject 0x80004005
/// and the cascade falls to SW HEVC. Demoting those hosts out of
/// HEVC advertising forces the browser to negotiate H.264 where the
/// cascade lands on real HW.
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeResult {
    /// Cascade landed on dedicated HW MFT — codec is safe to advertise.
    Hardware,
    /// Cascade activated, but only on the SW fallback (`backend="sw"`).
    /// Caller decides whether to advertise; default policy is to drop
    /// HEVC/AV1 when SW-only and let H.264 win negotiation.
    SoftwareOnly,
    /// No working encoder found at all. Caller MUST drop from caps.
    Failed,
}

/// Spin up the real MF encoder for `codec` at a tiny probe resolution,
/// inspect the resulting backend kind, then drop it. Logs the verdict
/// at info / warn so the cascade outcome is visible in startup logs.
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
fn activates(codec: CodecProbe) -> ProbeResult {
    let start = std::time::Instant::now();
    let result = match codec {
        CodecProbe::Hevc => super::mf::MfEncoder::new_hevc(PROBE_WIDTH, PROBE_HEIGHT),
        CodecProbe::Av1 => super::mf::MfEncoder::new_av1(PROBE_WIDTH, PROBE_HEIGHT),
    };
    let elapsed_ms = start.elapsed().as_millis();
    match result {
        Ok(enc) => {
            use super::VideoEncoder;
            let is_hw = enc.is_hardware();
            // Dropping `enc` triggers the worker's Shutdown cmd which
            // in turn runs MFShutdown + CoUninitialize on its thread.
            drop(enc);
            if is_hw {
                tracing::info!(
                    codec = ?codec,
                    elapsed_ms,
                    "caps probe: codec activates on HW — advertising"
                );
                ProbeResult::Hardware
            } else {
                tracing::warn!(
                    codec = ?codec,
                    elapsed_ms,
                    "caps probe: codec activates only on SW — NOT advertising (H.264 HW likely better). Set ROOMLER_AGENT_ALLOW_SW_HEAVY=1 to override."
                );
                ProbeResult::SoftwareOnly
            }
        }
        Err(e) => {
            tracing::warn!(
                codec = ?codec,
                %e,
                elapsed_ms,
                "caps probe: codec enumerates but does NOT activate — NOT advertising"
            );
            ProbeResult::Failed
        }
    }
}

/// Operator escape hatch: advertise HEVC/AV1 even when the cascade
/// only lands on SW. Off by default. Useful when the host has no
/// working H.264 HW path and SW HEVC is a strict improvement over
/// SW H.264 (rare but possible on machines without Intel QSV / NVENC
/// / AMF).
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
fn allow_sw_heavy_override() -> bool {
    std::env::var("ROOMLER_AGENT_ALLOW_SW_HEAVY")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
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
    let browser_has = |c: &str| browser_caps.iter().any(|b| b.eq_ignore_ascii_case(c));
    let agent_has = |c: &str| agent_caps.iter().any(|a| a.eq_ignore_ascii_case(c));
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

    /// Y.4: in default-feature (no `vp9-444`) builds, the transports
    /// list must NOT advertise `data-channel-vp9-444`. The browser
    /// reads this list to decide whether to even open the DC; an
    /// agent that lies about transport support would crash the
    /// session at media-pump time.
    #[cfg(not(feature = "vp9-444"))]
    #[test]
    fn detect_omits_vp9_444_transport_when_feature_disabled() {
        let caps = compute_caps();
        assert!(
            !caps.transports.iter().any(|t| t == "data-channel-vp9-444"),
            "default-feature build advertised vp9-444 transport: {:?}",
            caps.transports
        );
        assert!(
            !caps.hw_encoders.iter().any(|e| e == "libvpx-vp9-444-sw"),
            "default-feature build advertised libvpx encoder: {:?}",
            caps.hw_encoders
        );
    }

    /// Y.4 caps probe (post Y.runtime-encoder rewrite, 0.1.47): the
    /// libvpx encoder at the probe resolution must successfully
    /// activate, and `compute_caps` must surface both the
    /// `data-channel-vp9-444` transport and the `libvpx-vp9-444-sw`
    /// encoder label. CI runs this with `libvpx-dev` apt-installed
    /// so the link succeeds. If the probe ever regresses (libvpx
    /// missing on the build host, encoder init failure on the probe
    /// dimensions), this test catches it before a session ever
    /// negotiates onto a broken transport.
    #[cfg(feature = "vp9-444")]
    #[test]
    fn detect_advertises_vp9_444_transport_when_encoder_works() {
        let caps = compute_caps();
        assert!(
            caps.transports.iter().any(|t| t == "data-channel-vp9-444"),
            "vp9-444 transport must be advertised when libvpx probe succeeds; got {:?}",
            caps.transports
        );
        assert!(
            caps.hw_encoders.iter().any(|e| e == "libvpx-vp9-444-sw"),
            "libvpx encoder label must be advertised when probe succeeds; got {:?}",
            caps.hw_encoders
        );
    }

    /// rc.19 file-DC v3 capability lock. The browser opts into
    /// resumable uploads ONLY when this string appears in
    /// `caps.files`. Removing or renaming it would silently disable
    /// the resume path for every rc.19+ browser — lock here.
    #[test]
    fn detect_advertises_resume_files_cap() {
        let caps = compute_caps();
        assert!(
            caps.files.iter().any(|s| s == "resume"),
            "rc.19 caps.files must include \"resume\"; got {:?}",
            caps.files
        );
    }
}
