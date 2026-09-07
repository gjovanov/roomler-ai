// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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
// FR-77 — the cell vocabulary. Which of these a build uses depends on its
// encoder features, so the import is allowed to be partly idle.
#[allow(unused_imports)]
use roomler_ai_remote_control::models::{ChromaFormat, VideoBackend, VideoCell, VideoCodec};
use std::sync::OnceLock;

static CACHED_CAPS: OnceLock<AgentCaps> = OnceLock::new();

/// Running the hardware probes in a CHILD PROCESS.
///
/// A capability probe is untrusted third-party code by definition — it calls
/// into vendor drivers and, through them, GPU firmware. In-process, a fault
/// anywhere in that stack takes the daemon down, and the service manager
/// restarts it straight back into the same probe: a crash-LOOP, not a
/// degraded-but-running agent.
///
/// Observed on the WSL sibling, 2026-08-20: WSL ships
/// `/usr/lib/wsl/lib/libcuda.so.1` as a stub with no usable driver, so
/// `hevc_nvenc` **dlopens successfully** and then segfaults when `cuInit(0)`
/// fails — the loaded-but-unusable state that no "is it present?" check
/// catches. Hosts with no libcuda at all take the clean dlopen-failed branch
/// and were never affected, which is why this stayed latent.
///
/// So the probes run behind a process boundary and **"the child died" is read
/// as "codec unavailable"**. That retires the whole class rather than the one
/// CUDA symptom: any future driver that faults, hangs, or calls `exit()` costs
/// this host its HW advertisement and nothing more.
mod child {
    use super::AgentCaps;

    /// Marks the line carrying the child's JSON, so log output on the same
    /// stream cannot be mistaken for the result. Parsing the last line, or
    /// all of stdout, would break the first time anything logged there.
    pub(super) const MARKER: &str = "ROOMLER_CAPS_JSON:";

    /// Set in the child's environment. A belt-and-braces recursion guard:
    /// `detect()` in a process carrying this must never spawn again.
    const CHILD_ENV: &str = "ROOMLERD_CAPS_CHILD";

    /// Generous, because a cold GPU driver init on a loaded corp laptop is
    /// genuinely slow (~300 ms per codec, several codecs, plus process
    /// start). This is a backstop against a HUNG driver, not a performance
    /// budget — and even at the ceiling it beats the in-process behaviour it
    /// replaces, which was to hang forever.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

    /// Probe in a child. `None` = no usable answer, for ANY reason (spawn
    /// failed, non-zero exit, killed by a signal, timed out, unparseable
    /// output) — the caller treats every one of them the same way.
    pub(super) fn probe() -> Option<AgentCaps> {
        if std::env::var_os(CHILD_ENV).is_some() {
            // We ARE the child (or something re-entered). Probing in-process
            // is what this process was started to do.
            return Some(super::compute_caps(true));
        }
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(%e, "caps probe: cannot resolve our own path — skipping HW probes");
                return None;
            }
        };

        let started = std::time::Instant::now();
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("caps-probe")
            .env(CHILD_ENV, "1")
            // S2 config fallbacks are process-local: hand them to the child as
            // real env vars, or every knob it reads falls to its built-in
            // default (FR-19 P4c: `relay-server` was never advertised).
            .envs(tunnel_core::env::config_fallbacks_for_child())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            // Inherited on purpose: the child's own probe logging is the
            // diagnostic record of WHICH codec it died on, and it belongs in
            // the daemon's log next to the verdict.
            .stderr(std::process::Stdio::inherit());
        #[cfg(windows)]
        {
            // No console window when the daemon runs attended.
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut childp = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(%e, "caps probe: could not spawn the probe child — no HW advertisement");
                return None;
            }
        };

        // Read stdout on this thread while waiting, so a chatty child cannot
        // fill the pipe and deadlock against our own wait.
        let stdout = childp.stdout.take();
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            if let Some(mut s) = stdout {
                let _ = s.read_to_string(&mut buf);
            }
            buf
        });

        let status = match wait_bounded(&mut childp) {
            Some(s) => s,
            None => {
                // A hung driver. Kill it and carry on without HW.
                let _ = childp.kill();
                let _ = childp.wait();
                let _ = reader.join();
                tracing::warn!(
                    timeout_s = TIMEOUT.as_secs(),
                    "caps probe: the probe child hung — treating every hardware codec as \
                     unavailable. The daemon is unaffected; this is the process boundary \
                     doing its job."
                );
                return None;
            }
        };
        let out = reader.join().unwrap_or_default();

        if !status.success() {
            // THE case this module exists for. A signal death here is a
            // driver fault that would otherwise have been ours.
            tracing::error!(
                status = %status,
                elapsed_ms = started.elapsed().as_millis(),
                "caps probe: the probe child DIED — treating every hardware codec as \
                 unavailable. Before this ran out-of-process the same fault crash-looped \
                 the daemon; see the child's own log lines above for which codec it was."
            );
            return None;
        }

        let line = out.lines().find_map(|l| l.trim().strip_prefix(MARKER))?;
        match serde_json::from_str::<AgentCaps>(line) {
            Ok(mut caps) => {
                let elapsed_ms = started.elapsed().as_millis();
                // FR-77 — stamped by the PARENT so it covers the whole cost
                // the daemon paid (spawn + every open + parse), the number the
                // fleet gets to judge the matrix probe by.
                caps.probe_ms = Some(u32::try_from(elapsed_ms).unwrap_or(u32::MAX));
                tracing::info!(
                    elapsed_ms,
                    codecs = ?caps.codecs,
                    hw_encoders = ?caps.hw_encoders,
                    cells = caps.video_cells.len(),
                    "caps probe: child reported"
                );
                Some(caps)
            }
            Err(e) => {
                tracing::warn!(%e, "caps probe: child output was not parseable — no HW advertisement");
                None
            }
        }
    }

    /// `wait` with a deadline. `None` = still running when time ran out.
    ///
    /// Polling rather than a platform wait-with-timeout: it is a handful of
    /// wakeups on a path that runs ONCE per process, and it keeps this
    /// module free of per-platform process APIs.
    fn wait_bounded(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(s)) => return Some(s),
                // Treat "cannot ask" as death; the caller's fallback is the
                // safe answer either way.
                Err(_) => return None,
                Ok(None) => {}
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// The `caps-probe` child's entire job: probe, print one marked JSON line.
///
/// Printing a MARKED line rather than bare JSON so that anything else on
/// stdout — a library that logs there, a driver that prints a banner — cannot
/// be mistaken for the result.
pub fn print_probe_result() {
    let caps = detect_in_process();
    match serde_json::to_string(&caps) {
        Ok(json) => println!("{}{json}", child::MARKER),
        Err(e) => {
            tracing::error!(%e, "caps probe: could not serialise caps");
            std::process::exit(2);
        }
    }
}

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
    CACHED_CAPS
        .get_or_init(|| match child::probe() {
            Some(mut caps) => {
                // The RPC verbs are config-derived, not driver-probed: compute
                // them HERE, where the config fallbacks are registered,
                // whatever the child saw.
                caps.rpc = rpc_caps();
                caps
            }
            None => {
                // The child died, hung, or could not be launched. "Codec
                // unavailable" is the only honest reading: we have no
                // evidence this host can encode with any of them, and
                // advertising one we cannot produce costs a black session.
                // Everything that needs no driver still stands.
                compute_caps(false)
            }
        })
        .clone()
}

/// Compute caps IN THIS PROCESS, running the hardware probes. This is what
/// the `caps-probe` child executes; nothing else should call it, because a
/// vendor driver that faults takes the whole process with it.
pub fn detect_in_process() -> AgentCaps {
    compute_caps(true)
}

/// Whether the OS will let this process capture the screen.
///
/// macOS is the only platform with a gate, and it is a silent one:
/// `CGDisplayStream` opens successfully without the Screen Recording grant
/// and delivers wallpaper-only frames forever. Probed rather than assumed so
/// the server can be told the truth. Cheap — a preflight call, no prompt.
/// Can this process reach a GUI session at all?
///
/// macOS's root LaunchDaemon cannot: session 0 has no WindowServer, so capture
/// and input are unavailable there no matter what TCC says. Every other
/// platform always can.
fn gui_session_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::tcc::has_gui_session()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

fn capture_permission_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::tcc::screen_recording_granted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Whether the OS will let this process inject input.
///
/// Same shape as [`capture_permission_granted`]: macOS drops every
/// `CGEventPost` on the floor without the Accessibility grant and reports
/// success. Uses the TCC probe directly rather than constructing an injector,
/// which is lazy by design (first injected event) and has side effects.
fn input_permission_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::tcc::accessibility_trusted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// `run_hw_probes = false` computes everything that needs no driver call —
/// which is exactly the honest answer when the probe child did not come back.
///
/// `unused_variables` is allowed because EVERY consumer of the flag sits
/// behind a `#[cfg]` (mf-encoder / ffmpeg-encoder / vp9-444); a default-feature
/// build probes nothing and legitimately never reads it.
#[allow(unused_variables)]
fn compute_caps(run_hw_probes: bool) -> AgentCaps {
    // `mut` is only consumed inside the cfg-gated push blocks below
    // (openh264-encoder / mf-encoder). Default-feature builds skip
    // both blocks and the vecs stay empty; silence the unused-mut
    // lint to keep the CI `cargo clippy --workspace -- -D warnings`
    // build green on Linux.
    #[allow(unused_mut)]
    let mut codecs: Vec<String> = Vec::new();
    #[allow(unused_mut)]
    let mut hw_encoders: Vec<String> = Vec::new();
    // FR-77 — every cell (codec × chroma) this host OPENED, one entry per
    // encoder (codec × backend). Filled next to the legacy fields, which keep
    // their exact pre-FR-77 meaning; the cells carry the whole matrix.
    #[allow(unused_mut)]
    let mut video_cells: Vec<VideoCell> = Vec::new();

    #[cfg(feature = "openh264-encoder")]
    {
        codecs.push("h264".into());
        hw_encoders.push("openh264-sw".into());
        // Software by definition — needs no driver, so it is a cell even when
        // the hardware probes are not run.
        video_cells.push(VideoCell::new(
            VideoCodec::H264,
            VideoBackend::Openh264,
            &[ChromaFormat::Yuv420],
            false,
        ));
    }

    #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
    if run_hw_probes {
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
            // FR-77 — the cell must say whether the cascade lands on
            // silicon, and only an activation knows (the legacy label above
            // is enumeration-only and says `-hw` even for the SW MFT).
            let probe = activates(CodecProbe::H264);
            if !matches!(probe, ProbeResult::Failed) {
                video_cells.push(VideoCell::new(
                    VideoCodec::H264,
                    VideoBackend::MediaFoundation,
                    &[ChromaFormat::Yuv420],
                    matches!(probe, ProbeResult::Hardware),
                ));
            }
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
                video_cells.push(VideoCell::new(
                    VideoCodec::Hevc,
                    VideoBackend::MediaFoundation,
                    &[ChromaFormat::Yuv420],
                    matches!(probe, ProbeResult::Hardware),
                ));
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
                video_cells.push(VideoCell::new(
                    VideoCodec::Av1,
                    VideoBackend::MediaFoundation,
                    &[ChromaFormat::Yuv420],
                    matches!(probe, ProbeResult::Hardware),
                ));
            }
        }
    }

    #[allow(unused_mut)]
    let mut transports: Vec<String> = Vec::new();
    // P7 — chroma formats the HEVC DC transport can emit (see the probe's
    // Ok arm below + AgentCaps::hevc_chroma). Stays empty when the ffmpeg
    // HEVC probe fails or the feature is off.
    #[allow(unused_mut)]
    let mut hevc_chroma: Vec<String> = Vec::new();

    // rc.77 — FFmpeg HEVC over DataChannel.
    //
    // **DEFAULT ON** since rc.107 — `ffmpeg_backend_enabled()` is an explicit
    // opt-OUT (`ROOMLERD_USE_FFMPEG=0`), not the rc.77 opt-IN this comment
    // used to describe. The flip was forced in the field: an MSI
    // MajorUpgrade WIPES the service env block, so the dropped `=1`
    // silently disabled HEVC fleet-wide (black canvas, rc.105→rc.106).
    //
    // Advertisement is still gated on a REAL `FfmpegEncoder::new_hevc`
    // probe at the standard resolution, so a host without working HW
    // HEVC falls back cleanly regardless of the flag. On success
    // advertise both the `h265` codec (additive to whatever
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
    if run_hw_probes && crate::encode::ffmpeg::available() {
        // `name()` is on the `VideoEncoder` trait — need the trait in
        // scope at the call site for method-resolution.
        use super::VideoEncoder;
        use crate::encode::ffmpeg::FfmpegEncoder;

        // FR-77 — cells this build will not even TRY (the kill switch), and
        // whether a QSV open proves hardware on this build (see
        // `qsv_is_hardware_by_construction`).
        let deny = denied_cells();
        let qsv_hw = FfmpegEncoder::qsv_is_hardware_by_construction();
        if !deny.is_empty() {
            tracing::info!(denied = ?deny, "caps probe: cells on the denylist are not opened");
        }

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
                    // FR-77 — the VP9 cell. 4:4:4 (profile 1) is attempted
                    // when the FFmpeg source matrix allows it for this name
                    // and the cell is not denylisted; today the open asks for
                    // planar yuv444p, which vp9_qsv refuses (it wants packed
                    // VUYX) — an honest failure until P3 teaches the pump
                    // that format. The session pump keeps vp9_qsv 4:2:0-only
                    // regardless (`peer.rs`).
                    let mut chroma = vec![ChromaFormat::Yuv420];
                    if let Some((cell_codec, backend)) = VideoBackend::from_ffmpeg_name(name) {
                        if ffmpeg_444_capable(name)
                            && !cell_denied(&deny, name, ChromaFormat::Yuv444)
                        {
                            match FfmpegEncoder::new_named_probe(
                                name,
                                PROBE_WIDTH,
                                PROBE_HEIGHT,
                                true,
                            ) {
                                Ok(e) => {
                                    drop(e);
                                    chroma.push(ChromaFormat::Yuv444);
                                }
                                Err(e) => {
                                    tracing::debug!(encoder = name, %e, "caps probe: 4:4:4 cell did not open")
                                }
                            }
                        }
                        video_cells.push(VideoCell::new(cell_codec, backend, &chroma, qsv_hw));
                    }
                    // P4 — measure whether THIS host's vp9_qsv honours
                    // runtime forced IDRs, per low_power mode. Hosts that
                    // do get GOP 64800 + on-demand-only keys (kills the
                    // residual ~1 Hz natural-key pulse on VP9-over-DC);
                    // hosts that don't keep the rc.219 containment. Escape
                    // hatch ROOMLERD_VP9_QSV_IDR_PROBE=0.
                    if tunnel_core::env::node_env("VP9_QSV_IDR_PROBE").as_deref() != Some("0") {
                        let t_idr = std::time::Instant::now();
                        if let Some((lp1, lp0)) =
                            crate::encode::ffmpeg::FfmpegEncoder::probe_and_cache_vp9_qsv_idr()
                        {
                            tracing::info!(
                                honors_low_power = lp1,
                                honors_vme = lp0,
                                elapsed_ms = t_idr.elapsed().as_millis(),
                                "caps probe: vp9_qsv runtime-IDR verdict (either true → long GOP + on-demand keys; both false → GOP-60 containment)"
                            );
                        }
                    } else {
                        tracing::info!(
                            "caps probe: vp9_qsv IDR probe disabled (ROOMLERD_VP9_QSV_IDR_PROBE=0) — keeping GOP-60 containment"
                        );
                    }
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

        // FR-77 — the MATRIX pass for HEVC / AV1 / H.264. Every backend in
        // cascade order is opened (not just the first that works) so
        // `video_cells` carries the host's whole encoder × chroma matrix, and
        // every winner the FFmpeg source matrix says can do 4:4:4 is re-opened
        // in that format — asked for, never asserted by name (P7 used to
        // advertise HEVC 4:4:4 for `hevc_nvenc` without opening it). The
        // legacy fields (`codecs`, `transports`, `hw_encoders`, `hevc_chroma`)
        // keep their exact pre-FR-77 meaning: the FIRST backend in cascade
        // order that opens, because a session still cascades in that order
        // and a viewer older than FR-77 reads nothing else.
        //
        // P2 (Parsec-class plan) — H.264 over DataChannel joins the reliable-
        // DC + WebCodecs + canvas pipeline (the RTP track + <video> path stays
        // as the universal fallback). The bitstream is Annex-B with in-band
        // SPS/PPS — the contract the HEVC path ships. `ROOMLERD_DC_H264=0`
        // stops that advertisement without a rebuild; the H.264 cells are
        // then not probed either (a cell nobody can negotiate is noise).
        let dc_h264 = tunnel_core::env::node_env("DC_H264").as_deref() != Some("0");
        for codec in [VideoCodec::Hevc, VideoCodec::Av1, VideoCodec::H264] {
            if codec == VideoCodec::H264 && !dc_h264 {
                tracing::info!(
                    "caps probe: data-channel-h264 advertisement disabled (ROOMLERD_DC_H264=0)"
                );
                continue;
            }
            let start = std::time::Instant::now();
            let mut winner: Option<&'static str> = None;
            let mut winner_has_444 = false;
            for name in FfmpegEncoder::cascade_names(codec) {
                // A table name outside the vocabulary would open and then
                // advertise nothing; `every_cascade_name_is_in_the_cell_vocabulary`
                // makes that a test failure rather than a silent hole.
                let Some((cell_codec, backend)) = VideoBackend::from_ffmpeg_name(name) else {
                    continue;
                };
                let t = std::time::Instant::now();
                match FfmpegEncoder::new_named_probe(name, PROBE_WIDTH, PROBE_HEIGHT, false) {
                    Ok(enc) => {
                        drop(enc);
                        let mut chroma = vec![ChromaFormat::Yuv420];
                        if ffmpeg_444_capable(name)
                            && !cell_denied(&deny, name, ChromaFormat::Yuv444)
                        {
                            match FfmpegEncoder::new_named_probe(
                                name,
                                PROBE_WIDTH,
                                PROBE_HEIGHT,
                                true,
                            ) {
                                Ok(e) => {
                                    drop(e);
                                    chroma.push(ChromaFormat::Yuv444);
                                }
                                Err(e) => tracing::info!(
                                    encoder = name,
                                    %e,
                                    "caps probe: 4:4:4 cell did not open — advertising 4:2:0 only"
                                ),
                            }
                        }
                        let hw = match backend {
                            VideoBackend::Qsv => qsv_hw,
                            _ => true,
                        };
                        tracing::info!(
                            encoder = name,
                            chroma = ?chroma.iter().map(|c| c.wire()).collect::<Vec<_>>(),
                            hw,
                            elapsed_ms = t.elapsed().as_millis(),
                            "caps probe: ffmpeg encoder cell opened"
                        );
                        if winner.is_none() {
                            winner = Some(name);
                            winner_has_444 = chroma.contains(&ChromaFormat::Yuv444);
                        }
                        video_cells.push(VideoCell::new(cell_codec, backend, &chroma, hw));
                    }
                    Err(e) => {
                        // A backend the host lacks failing is the matrix doing
                        // its job — DEBUG, like the cascade's own candidates.
                        tracing::debug!(encoder = name, %e, "caps probe: ffmpeg encoder cell did not open");
                    }
                }
            }
            let elapsed_ms = start.elapsed().as_millis();
            match (codec, winner) {
                (VideoCodec::Hevc, Some(name)) => {
                    tracing::info!(
                        encoder = name,
                        elapsed_ms,
                        "caps probe: ffmpeg HEVC encoder activates — advertising h265 + data-channel-hevc"
                    );
                    if !codecs.iter().any(|c| c == "h265") {
                        codecs.push("h265".into());
                    }
                    transports.push("data-channel-hevc".into());
                    hw_encoders.push(format!("ffmpeg-{name}"));
                    // P7 — the session's 4:4:4 (Rext) path tries `hevc_nvenc`
                    // only, so the legacy field says 4:4:4 only when THAT
                    // backend won AND its 4:4:4 open succeeded just now.
                    hevc_chroma.push("yuv420".into());
                    if name == "hevc_nvenc" && winner_has_444 {
                        hevc_chroma.push("yuv444".into());
                    }
                }
                (VideoCodec::Hevc, None) => tracing::warn!(
                    elapsed_ms,
                    "caps probe: ffmpeg HEVC encoder failed to init — NOT advertising data-channel-hevc"
                ),
                (VideoCodec::Av1, Some(name)) => {
                    tracing::info!(
                        encoder = name,
                        elapsed_ms,
                        "caps probe: ffmpeg AV1 encoder activates — advertising av1 + data-channel-av1"
                    );
                    if !codecs.iter().any(|c| c == "av1") {
                        codecs.push("av1".into());
                    }
                    transports.push("data-channel-av1".into());
                    hw_encoders.push(format!("ffmpeg-{name}"));
                }
                (VideoCodec::Av1, None) => tracing::info!(
                    elapsed_ms,
                    "caps probe: ffmpeg AV1 encoder not available (no AV1 encode silicon on this host) — NOT advertising data-channel-av1"
                ),
                (VideoCodec::H264, Some(name)) => {
                    tracing::info!(
                        encoder = name,
                        elapsed_ms,
                        "caps probe: ffmpeg H.264 encoder activates — advertising data-channel-h264"
                    );
                    transports.push("data-channel-h264".into());
                    hw_encoders.push(format!("ffmpeg-{name}"));
                }
                (VideoCodec::H264, None) => tracing::info!(
                    elapsed_ms,
                    "caps probe: ffmpeg H.264 HW encoder not available — H.264 sessions stay on the RTP track"
                ),
                (VideoCodec::Vp9, _) => {}
            }
        }
    }

    #[cfg(feature = "vp9-444")]
    if run_hw_probes {
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
                // FR-77 — the libvpx cell: profile 0 (4:2:0) is opened for
                // real too, so the cell claims only what opened.
                let mut chroma = Vec::new();
                match crate::encode::libvpx::Vp9Encoder::new_with_fps_chroma(
                    PROBE_WIDTH,
                    PROBE_HEIGHT,
                    30,
                    crate::encode::libvpx::Vp9Chroma::Yuv420,
                ) {
                    Ok(e) => {
                        drop(e);
                        chroma.push(ChromaFormat::Yuv420);
                    }
                    Err(e) => tracing::debug!(%e, "caps probe: libvpx 4:2:0 cell did not open"),
                }
                chroma.push(ChromaFormat::Yuv444);
                video_cells.push(VideoCell::new(
                    VideoCodec::Vp9,
                    VideoBackend::Libvpx,
                    &chroma,
                    false,
                ));
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

    // Audio track advertisement. Only when the `audio` feature is
    // compiled in do we offer a WebRTC Opus track (system/desktop
    // audio, opt-in per session). The browser reads this list to decide
    // whether to set `audio_enabled` in `rc:session.request`; an empty
    // list means the agent won't add an audio track at all. Mirrors the
    // vp9-444 pattern (advertise a capability only when the code path
    // that fulfils it is actually built). No runtime probe: unlike the
    // video HW encoders, a failed audio-device open degrades to silence
    // (NoopAudioCapture) rather than a black/broken track, so it's safe
    // to advertise on feature presence alone.
    #[allow(unused_mut)]
    let mut audio: Vec<String> = Vec::new();
    #[cfg(feature = "audio")]
    {
        audio.push("opus".into());
    }

    // Remote app selection & launch (virtual-desktop hosts). Advertised
    // only when this process can actually manage a desktop (Linux VD
    // mode) AND the operator hasn't disabled it; the browser gates its
    // Apps menu on this list. Older agents omit the field → menu hidden.
    let mut apps: Vec<String> = Vec::new();
    if crate::apps::apps_supported() {
        apps.push("list".into());
        apps.push("focus".into());
        apps.push("launch".into());
    }

    // Clipboard protocol-v2 flags. Advertised purely on feature
    // presence (like `supports_clipboard`): the handlers for ack /
    // subscribe / image framing are compiled in with the `clipboard`
    // feature and degrade at runtime (a headless host without a
    // clipboard no-ops the DC exactly as v1 did). The browser gates
    // its auto-sync engine + image paths on these values.
    let clipboard: Vec<String> = if cfg!(feature = "clipboard") {
        let mut c: Vec<String> = vec![
            "ack".into(),
            "events".into(),
            "images".into(),
            // v2.1 — html lane: formatted text + web-hosted images
            // survive the round-trip (CF_HTML / text/html both ways).
            "html".into(),
        ];
        // v2.2 — native lane (RTF with embedded images). Windows-only:
        // the RTF read/write rides clipboard-win's raw format API.
        if cfg!(target_os = "windows") {
            c.push("native".into());
        }
        c
    } else {
        Vec::new()
    };

    // Keyboard-layout integration (rc.227) — status reporting +
    // manual set ride the control DC; both are Windows-only code in
    // `input::layout` behind enigo-input. The browser gates its
    // layout chip + picker on these.
    let layout: Vec<String> = if cfg!(all(target_os = "windows", feature = "enigo-input")) {
        vec!["report".into(), "set".into()]
    } else {
        Vec::new()
    };

    // What the OS has actually granted, not what we were compiled with.
    // See `AgentCaps::permissions`: `None` (a pre-rc.454 agent) and
    // `Some([])` mean opposite things, so this is always `Some` here.
    let mut permissions: Vec<String> = Vec::new();
    if !gui_session_available() {
        // THIRD state, distinct from granted and denied: this process is not
        // in a GUI login session (macOS's root LaunchDaemon), so capture and
        // input are impossible regardless of any grant. Without saying so, a
        // mesh-only daemon reports "holds neither permission" and the device
        // list tells the operator to go fix something that is not broken.
        permissions.push("no-gui-session".into());
    } else {
        if capture_permission_granted() {
            permissions.push("screen-capture".into());
        }
        if input_permission_granted() {
            permissions.push("input".into());
        }
    }

    AgentCaps {
        hw_encoders,
        codecs,
        // Was `cfg!(feature = "enigo-input")` — a COMPILE-TIME constant, so a
        // Mac with Accessibility denied still advertised working input while
        // silently dropping every event. The feature must be compiled in AND
        // the OS must have granted it.
        has_input_permission: cfg!(feature = "enigo-input")
            && gui_session_available()
            && input_permission_granted(),
        permissions: Some(permissions),
        supports_clipboard: cfg!(feature = "clipboard"),
        supports_file_transfer: true,
        max_simultaneous_sessions: rc_max_sessions(),
        transports,
        // FR-17 — this build frames every DataChannel message, so a
        // receiver can reassemble without relying on the channel being
        // ordered. Advertised unconditionally: it is a property of the
        // code, not of the host or its hardware.
        video: vec!["chunk-framing".to_string()],
        files,
        vp9_chroma,
        hevc_chroma,
        audio,
        apps,
        clipboard,
        layout,
        video_cells,
        // Stamped by the PARENT after the child reports (`child::probe`);
        // the driver-free fallback has no probe to time.
        probe_ms: None,
        // P6 — the InputArbiter runs on every build (injection degrades to
        // Noop without enigo-input, but the arbitration/floor semantics
        // hold), so the server can safely lift the P3 single-INPUT-holder
        // downgrade for this agent.
        input: vec!["arbiter".into(), "exclusive".into(), "ghost-cursor".into()],
        // Multi-org — this agent honours `rc:agent.join_org` (enroll into an
        // additional org from a pushed token, no restart). Build-independent:
        // the enroll + config paths are in every flavour.
        multi_org: vec!["join".into()],
        // Fleet RPC — build-independent (the exec engine is std process
        // spawning, no feature-gated backend). Advertising the capability
        // says only "this agent understands the verbs": the org kill-switch,
        // the device's ExecPolicy, and the agent's own `exec_enabled` config
        // key all still have to say yes before anything runs.
        rpc: rpc_caps(),
    }
}

/// Fleet-RPC + roomler-SSH verb capabilities.
///
/// `exec` / `originate` are build-independent (the exec engine is std process
/// spawning). `ssh` is NOT: it is gated on the `ssh-server` feature, because
/// `rc:ssh.grant` reaching a build without the server would be recorded by
/// nobody and the caller would hang against a port that answers nothing — the
/// exact failure the capability list exists to prevent.
///
/// Advertising a verb says only "this agent understands it". The org
/// kill-switch, the device's policy and the agent-local config key all still
/// have to say yes before anything happens.
fn rpc_caps() -> Vec<String> {
    use roomler_ai_remote_control::models::RpcCap;

    // Named by VARIANT, not by string literal: the wire spelling lives in
    // exactly one place (`RpcCap::wire`) that both this producer and every
    // server-side consumer go through, so the two can no longer drift.
    // `Config` is unconditional: understanding the frame is a property of the
    // BUILD, not of any feature or local setting. Whether the device obeys it
    // is a separate question answered by `remote_config_enabled` and by
    // whether the frame arrived on the primary org's socket — advertising
    // here must not imply either, or the server would read "will comply" from
    // a verb that only means "will parse".
    // `ConfigReport` rides alongside `Config` in THIS build, but it is a
    // separate verb because rc.457/rc.458 shipped `config` alone: those agents
    // apply a pushed config and never say a word about it. A server that read
    // "reports back" out of `config` would wait forever for an answer from
    // most of the fleet — the identical trap `ssh` / `ssh-consent` exists to
    // avoid, recurring exactly as that doc predicted it would.
    let mut caps = vec![
        RpcCap::Exec,
        RpcCap::Originate,
        RpcCap::Config,
        RpcCap::ConfigReport,
    ];
    if cfg!(feature = "ssh-server") {
        caps.push(RpcCap::Ssh);
        // P5d. Distinct from `ssh` because agents rc.419 and earlier advertise
        // `ssh` while silently ignoring `SshPolicy.consent_mode` — the server
        // refuses to store a non-auto consent policy for a device that cannot
        // honour it, rather than hand an admin a rule that reads as enforced
        // and isn't.
        caps.push(RpcCap::SshConsent);
    }
    // FR-19 — advertise the org-relay SERVER only when this device has opted
    // in (`relay_server_enabled`, gate 4). The server never installs a session
    // on a device that does not advertise this, so a device that has not opted
    // in is simply never asked — and a build without the overlay features has
    // no relay to advertise at all.
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    if tunnel_core::overlay::orgrelay::relay_server_enabled() {
        caps.push(RpcCap::RelayServer);
    }
    // FR-40 — a build with an overlay surface can retire its overlay key on
    // order. A property of the BUILD, like `config`: the device may still
    // refuse (`overlay_key_rotation=false`), and says so in its report.
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    caps.push(RpcCap::KeyRotate);
    caps.into_iter().map(|c| c.wire().to_string()).collect()
}

/// Multi-user P3 — how many CONCURRENT remote-control sessions this agent
/// advertises (the server's `AgentBusy` gate enforces the value). Pinned to
/// 1 since v1; the whole capacity model (server gate, per-session
/// `AgentPeer`s, per-session consent/permissions) was already N-ready.
///
/// Default 2 — pair-working: each extra session runs its OWN capture +
/// encoder until the P5 shared-floor encoder lands, so weak-GPU hosts
/// (Iris-Xe-class) may prefer `rc_max_sessions = 1`; the DXGI
/// duplication app-limit (~4) bounds the useful ceiling — clamp at 8.
/// Concurrent-INPUT chaos is prevented server-side (one INPUT holder per
/// agent until the P6 arbitration modes land).
///
/// `ROOMLERD_RC_MAX_SESSIONS` env > `rc_max_sessions` config key
/// (bridged via the S2 fallback map) > built-in 2.
pub(crate) fn rc_max_sessions() -> u8 {
    tunnel_core::env::node_env("RC_MAX_SESSIONS")
        .and_then(|v| v.trim().parse::<u8>().ok())
        .map(|n| n.clamp(1, 8))
        .unwrap_or(2)
}

/// FR-77 — FFmpeg encoder names whose 4:4:4 open the probe ATTEMPTS. Taken
/// from the FFmpeg n9.0 sources, not from vendor marketing: `h264_nvenc` and
/// `hevc_nvenc` list yuv444p (runtime-gated by `NV_ENC_CAPS_SUPPORT_YUV444_ENCODE`),
/// `hevc_qsv` and `vp9_qsv` list a 4:4:4 form (packed VUYX/XV30, which the
/// pump does not produce yet — P3), `hevc_vaapi`/`vp9_vaapi` carry the
/// Main444 / profile-1 rows (P4). Every AV1 encoder, every AMF encoder,
/// VideoToolbox and Media Foundation cannot, so they are never asked and
/// never cost a failed open. Locked by a test against the vocabulary.
#[allow(dead_code)]
const FFMPEG_444_CAPABLE: &[&str] = &[
    "h264_nvenc",
    "hevc_nvenc",
    "hevc_qsv",
    "vp9_qsv",
    "hevc_vaapi",
    "vp9_vaapi",
];

/// FR-77 — cells this build will not open or advertise until a field test
/// takes them off the list: the kill switch of the matrix. `name:chroma`.
/// HEVC 4:4:4 on QSV and VAAPI start here (the code called QSV Rext encode
/// unreliable before it was ever opened); the operator's
/// `ROOMLERD_ENCODER_CELLS_DENY` (comma-separated, an empty value = deny
/// nothing) REPLACES this default.
#[allow(dead_code)]
const DEFAULT_DENIED_CELLS: &[&str] = &["hevc_qsv:yuv444", "hevc_vaapi:yuv444"];

#[allow(dead_code)]
fn ffmpeg_444_capable(name: &str) -> bool {
    FFMPEG_444_CAPABLE.contains(&name)
}

/// The effective denylist: the env override when set, else the built-in.
#[allow(dead_code)]
fn denied_cells() -> Vec<String> {
    match tunnel_core::env::node_env("ENCODER_CELLS_DENY") {
        Some(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        None => DEFAULT_DENIED_CELLS.iter().map(|s| s.to_string()).collect(),
    }
}

#[allow(dead_code)]
fn cell_denied(
    deny: &[String],
    name: &str,
    chroma: roomler_ai_remote_control::models::ChromaFormat,
) -> bool {
    let key = format!("{name}:{}", chroma.wire());
    deny.iter().any(|d| d == &key)
}

/// Codec to probe. We only probe codecs that fail closed on activation
/// error (HEVC + AV1 today); H.264 has a working triple-fallback path
/// and is not gated.
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
#[derive(Debug, Clone, Copy)]
enum CodecProbe {
    /// FR-77 — probed for the CELL only (`video_cells` must say whether the
    /// cascade lands on silicon); the legacy `mf-h264-hw` label stays
    /// enumeration-gated as it always was.
    H264,
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
        CodecProbe::H264 => super::mf::MfEncoder::new_h264(PROBE_WIDTH, PROBE_HEIGHT),
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
                    "caps probe: codec activates only on SW — NOT advertising (H.264 HW likely better). Set ROOMLERD_ALLOW_SW_HEAVY=1 to override."
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
    use tunnel_core::env::node_env;
    node_env("ALLOW_SW_HEAVY")
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

    /// Closes the loop the `RpcCap` enum exists to close: every verb this
    /// agent PUTS ON THE WIRE must be one the server can parse back. A typo
    /// here used to be invisible — the device would simply look like it lacked
    /// the feature, which is indistinguishable from an old agent.
    #[test]
    fn every_advertised_verb_is_a_known_capability() {
        use roomler_ai_remote_control::models::RpcCap;

        let advertised = rpc_caps();
        assert!(
            !advertised.is_empty(),
            "every build advertises exec at least"
        );
        for verb in &advertised {
            assert!(
                RpcCap::from_wire(verb).is_some(),
                "advertised {verb:?} is not a verb the server knows"
            );
        }
        // Build-independent floor: the exec engine is plain process spawning,
        // so these hold in every feature configuration.
        assert!(advertised.iter().any(|v| v == RpcCap::Exec.wire()));
        assert!(advertised.iter().any(|v| v == RpcCap::Originate.wire()));

        // The SSH pair is feature-gated TOGETHER: advertising `ssh` without
        // `ssh-consent` is precisely the rc.419 state the server has to refuse
        // a consent policy for, so they must never drift apart in one build.
        assert_eq!(
            advertised.iter().any(|v| v == RpcCap::Ssh.wire()),
            advertised.iter().any(|v| v == RpcCap::SshConsent.wire()),
            "ssh and ssh-consent must be advertised together in a single build"
        );
        assert_eq!(
            advertised.iter().any(|v| v == RpcCap::Ssh.wire()),
            cfg!(feature = "ssh-server"),
            "the ssh verbs must track the ssh-server feature, not the version"
        );
    }

    /// Permissions are always REPORTED, even when everything is granted.
    ///
    /// `None` means "this agent is too old to know", and a server or UI must
    /// be able to tell that apart from "this agent holds nothing" — so a
    /// current agent must never send `None`. The list itself is
    /// platform-dependent (only macOS actually gates these), which is why this
    /// asserts the reporting contract rather than the contents.
    #[test]
    fn caps_always_report_permission_state() {
        let caps = compute_caps(false);
        let perms = caps
            .permissions
            .expect("a current agent must report permissions, so None can keep meaning 'unknown'");
        for p in &perms {
            assert!(
                matches!(p.as_str(), "screen-capture" | "input"),
                "unrecognised permission {p:?} — readers skip unknown values, so a typo \
                 here silently reads as 'not granted'"
            );
        }
        // Platforms with no permission model must not look muzzled.
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            perms,
            vec!["screen-capture".to_string(), "input".to_string()],
            "a platform that does not gate these must report both granted"
        );
    }

    /// `has_input_permission` must track the OS, not the build.
    ///
    /// It was `cfg!(feature = "enigo-input")` — a compile-time constant — so a
    /// Mac with Accessibility denied advertised working input while silently
    /// dropping every event. Without the feature it must stay false regardless.
    #[test]
    fn input_permission_is_not_merely_a_compile_flag() {
        let caps = compute_caps(false);
        if !cfg!(feature = "enigo-input") {
            assert!(!caps.has_input_permission);
        }
        assert_eq!(
            caps.has_input_permission,
            cfg!(feature = "enigo-input") && input_permission_granted(),
            "the advertised bool must agree with the runtime probe"
        );
    }

    /// THE property this slice exists for: a host whose hardware probes
    /// cannot be trusted still produces usable caps, and never claims a codec
    /// it has no evidence for.
    ///
    /// `compute_caps(false)` is exactly what `detect()` falls back to when the
    /// probe child dies, so this is the shape a driver fault now yields —
    /// where before it yielded a crash-looping daemon.
    #[test]
    fn a_failed_probe_still_yields_usable_caps_and_advertises_no_hardware() {
        let caps = compute_caps(false);

        // Nothing that required asking a driver.
        for name in ["mf-h264-hw", "mf-h265-hw", "mf-av1-hw"] {
            assert!(
                !caps.hw_encoders.iter().any(|h| h == name),
                "{name} was advertised without a successful probe: {:?}",
                caps.hw_encoders
            );
        }
        assert!(
            !caps.hw_encoders.iter().any(|h| h.starts_with("ffmpeg-")),
            "an ffmpeg HW encoder was advertised without a successful probe: {:?}",
            caps.hw_encoders
        );
        // Transports are the sharp end — negotiating one the host cannot
        // fulfil is a black session, not a downgrade.
        for t in [
            "data-channel-hevc",
            "data-channel-av1",
            "data-channel-h264",
            "data-channel-vp9-444",
        ] {
            assert!(
                !caps.transports.iter().any(|x| x == t),
                "{t} was advertised without a successful probe: {:?}",
                caps.transports
            );
        }

        // ...but the agent is still a working agent: the parts that never
        // depended on a driver survive, so a probe fault degrades the host
        // rather than disabling it.
        assert!(
            caps.files.iter().any(|f| f == "upload"),
            "file transfer must survive a probe failure: {:?}",
            caps.files
        );
        assert!(caps.max_simultaneous_sessions > 0);

        // FR-77 — no hardware cell without a probe, and no probe time either
        // (the parent stamps it only when the child came back).
        assert!(
            caps.video_cells.iter().all(|c| !c.hw),
            "a hardware cell was advertised without a probe: {:?}",
            caps.video_cells
        );
        assert!(caps.probe_ms.is_none());
        // Whatever software cells survive must speak the vocabulary.
        for cell in &caps.video_cells {
            assert!(cell.typed().is_some(), "{cell:?} is outside the vocabulary");
        }
    }

    /// FR-77 — the 4:4:4 attempt list names only encoders the FFmpeg n9.0
    /// sources can actually open in 4:4:4: nothing AV1 (`av1_nvenc` hard-errors
    /// "AV1 High Profile not supported"; every other AV1 backend lists 4:2:0
    /// only), nothing AMF, nothing VideoToolbox — and every entry must be a
    /// name the vocabulary can split, or the probe would open it for nothing.
    #[test]
    fn ffmpeg_444_attempt_list_matches_the_source_matrix() {
        for name in FFMPEG_444_CAPABLE {
            let (codec, backend) = VideoBackend::from_ffmpeg_name(name)
                .unwrap_or_else(|| panic!("{name} is outside the cell vocabulary"));
            assert_ne!(
                codec,
                VideoCodec::Av1,
                "{name}: no AV1 encoder can do 4:4:4"
            );
            assert!(
                !matches!(backend, VideoBackend::Amf | VideoBackend::VideoToolbox),
                "{name}: AMF and VideoToolbox have no 4:4:4 surface"
            );
        }
        assert!(ffmpeg_444_capable("hevc_nvenc"));
        assert!(ffmpeg_444_capable("h264_nvenc"));
        assert!(!ffmpeg_444_capable("av1_nvenc"));
        assert!(!ffmpeg_444_capable("hevc_amf"));
        assert!(!ffmpeg_444_capable("hevc_videotoolbox"));
    }

    /// FR-77 — the denylist is the kill switch: the built-in default keeps the
    /// unproven cells closed, the env override replaces it wholesale, and an
    /// explicitly EMPTY override denies nothing.
    #[test]
    fn denylist_default_env_override_and_empty_override() {
        use tunnel_core::env::test_env::Saved;
        let _saved = Saved::cleared("ENCODER_CELLS_DENY");

        let deny = denied_cells();
        assert!(cell_denied(&deny, "hevc_qsv", ChromaFormat::Yuv444));
        assert!(cell_denied(&deny, "hevc_vaapi", ChromaFormat::Yuv444));
        assert!(!cell_denied(&deny, "hevc_qsv", ChromaFormat::Yuv420));
        assert!(!cell_denied(&deny, "hevc_nvenc", ChromaFormat::Yuv444));

        unsafe {
            tunnel_core::env::test_env::set(
                "ENCODER_CELLS_DENY",
                " h264_nvenc:yuv444 ,vp9_qsv:yuv444,",
            )
        };
        let deny = denied_cells();
        assert!(cell_denied(&deny, "h264_nvenc", ChromaFormat::Yuv444));
        assert!(cell_denied(&deny, "vp9_qsv", ChromaFormat::Yuv444));
        assert!(
            !cell_denied(&deny, "hevc_qsv", ChromaFormat::Yuv444),
            "the override REPLACES the default, it does not add to it"
        );

        unsafe { tunnel_core::env::test_env::set("ENCODER_CELLS_DENY", "") };
        assert!(
            denied_cells().is_empty(),
            "an empty override denies nothing"
        );
    }

    /// The child's output has to survive the round trip, or the parent falls
    /// back on every host and the probe silently stops meaning anything.
    #[test]
    fn the_marked_json_line_round_trips() {
        let caps = compute_caps(false);
        let line = format!("{}{}", child::MARKER, serde_json::to_string(&caps).unwrap());
        let payload = line
            .trim()
            .strip_prefix(child::MARKER)
            .expect("the marker must be recoverable");
        let back: AgentCaps = serde_json::from_str(payload).expect("caps must parse back");
        assert_eq!(back.codecs, caps.codecs);
        assert_eq!(back.transports, caps.transports);
        assert_eq!(back.hw_encoders, caps.hw_encoders);
        assert_eq!(
            back.video_cells, caps.video_cells,
            "FR-77 cells must survive the round trip"
        );
    }

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
        // Probing ENABLED on purpose: the claim is that a build without the
        // feature never advertises the transport even when probes run. (The
        // vp9 probe itself is cfg'd out here, so nothing is actually opened.)
        let caps = compute_caps(true);
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
        // Probing ENABLED: this test's whole claim is that the probe runs and
        // succeeds. libvpx is our own software encoder, not a vendor driver,
        // so running it in-process here is safe.
        let caps = compute_caps(true);
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

    /// Audio caps: a build WITH the `audio` feature must advertise
    /// `opus` so the browser knows it may request `audio_enabled`.
    #[cfg(feature = "audio")]
    #[test]
    fn detect_advertises_opus_audio_when_feature_enabled() {
        // Feature-gated, never probe-gated — no driver call needed.
        let caps = compute_caps(false);
        assert!(
            caps.audio.iter().any(|c| c == "opus"),
            "audio-feature build must advertise opus; got {:?}",
            caps.audio
        );
    }

    /// Audio caps: a default (no `audio` feature) build must NOT
    /// advertise any audio codec — the agent adds no audio track, so a
    /// browser that saw `opus` and asked for `audio_enabled` would wait
    /// forever for a track that never arrives.
    #[cfg(not(feature = "audio"))]
    #[test]
    fn detect_omits_audio_when_feature_disabled() {
        // Audio advertisement is feature-gated, never probe-gated, so this
        // needs no driver call.
        let caps = compute_caps(false);
        assert!(
            caps.audio.is_empty(),
            "default build must not advertise audio; got {:?}",
            caps.audio
        );
    }

    /// rc.19 file-DC v3 capability lock. The browser opts into
    /// resumable uploads ONLY when this string appears in
    /// `caps.files`. Removing or renaming it would silently disable
    /// the resume path for every rc.19+ browser — lock here.
    #[test]
    fn detect_advertises_resume_files_cap() {
        // File caps never depended on a probe — and must survive one failing.
        let caps = compute_caps(false);
        assert!(
            caps.files.iter().any(|s| s == "resume"),
            "rc.19 caps.files must include \"resume\"; got {:?}",
            caps.files
        );
    }
}
