// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-77 — the cell matrix's shared vocabulary: which FFmpeg encoders are
//! ever ASKED for 4:4:4, and the denylist that is the matrix's kill switch,
//! with the cells this host learned to deny on top of it (#3267).
//!
//! Shared by the caps probe (what the hello advertises) and the session pump
//! (what a session may open), so the two can never disagree: a cell the probe
//! would not advertise is a cell no session opens, and vice versa.

// Every item here is read by the FFmpeg probe and pump; a build without
// that backend has nothing to ask the matrix.
#![cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]

use roomler_ai_remote_control::models::ChromaFormat;

/// FFmpeg encoder names whose 4:4:4 open the probe ATTEMPTS. Taken from the
/// FFmpeg n9.0 sources, not from vendor marketing: `h264_nvenc` and
/// `hevc_nvenc` list yuv444p (runtime-gated by
/// `NV_ENC_CAPS_SUPPORT_YUV444_ENCODE`), `hevc_qsv` and `vp9_qsv` list a
/// packed 4:4:4 form (VUYX — P3 teaches the pump that layout),
/// `hevc_vaapi`/`vp9_vaapi` carry the Main444 / profile-1 rows (P4). Every
/// AV1 encoder, every AMF encoder, VideoToolbox and Media Foundation cannot,
/// so they are never asked and never cost a failed open. Locked by a test
/// against the vocabulary.
pub(crate) const FFMPEG_444_CAPABLE: &[&str] = &[
    "h264_nvenc",
    "hevc_nvenc",
    "hevc_qsv",
    "vp9_qsv",
    "hevc_vaapi",
    "vp9_vaapi",
    // FR-78 — Vulkan HEVC lists 4:4:4 as runtime-decided (planar yuv444p
    // against the driver's format list); AV1 stays 4:2:0 by the AV1 rule
    // and D3D12 video encode is NV12/P010 only.
    "hevc_vulkan",
];

/// Cells this build will not open or advertise until a field test takes
/// them off the list. `name:chroma`. HEVC 4:4:4 on QSV and VAAPI start here
/// (the code called QSV Rext encode unreliable before it was ever opened);
/// the operator's `ROOMLERD_ENCODER_CELLS_DENY` / the `encoder_cells_deny`
/// config key REPLACES this default.
// `vp9_qsv:yuv444` joined on 2026-09-08: CORPLAP-3's Intel media runtime died
// with 0xc0000005 on the first VUYX open (FR-77 P3b field read) — the cell
// opens only once a driver proves it, through `encoder_cells_deny`.
pub(crate) const DEFAULT_DENIED_CELLS: &[&str] = &[
    "hevc_qsv:yuv444",
    "hevc_vaapi:yuv444",
    "vp9_qsv:yuv444",
    // P4: the packed 4:4:4 open on VAAPI is as unproven as it was on QSV.
    "vp9_vaapi:yuv444",
    // FR-78 — as unproven as every other non-NVENC 4:4:4 form: denied
    // until a driver survives the open on a fleet host.
    "hevc_vulkan:yuv444",
    // FR-78 P3 (the dev box, RTX 5090, 2026-09-08).
    // The cell opens, encodes and reports nothing wrong, and its frames
    // cannot be decoded: dav1d fails every frame header ("Error parsing
    // frame header"), libaom aborts, Chrome paints garbage where the picture
    // changed. Reproduced with the FFmpeg CLI's own `av1_vulkan` on the same
    // GPU, with no roomler code in the loop — an FFmpeg 9.0.1 / NVIDIA
    // driver defect, not our packaging (the missing temporal delimiter WAS
    // ours to fix, and is). Leaves the list when a driver or FFmpeg release
    // decodes; the read is on the FR-78 spec.
    "av1_vulkan:yuv420",
    // #3267 (the dev box, Radeon 610M, 2026-11-01): from driver 32.0.21043
    // AMD's `amdenc64.dll` faults on this open. The fault lands in the base
    // probe's child, and a base child that dies costs the host every hardware
    // cell, NVENC on another GPU included. `h264_vulkan` opens on the same
    // driver, and on 32.0.13034 this cell did too. Windows only: the fault is
    // that driver's, jupiter's RADV opens the cell on Linux, and on Windows
    // every vendor's own cell sits ahead of this one in the cascade.
    #[cfg(windows)]
    "hevc_vulkan:yuv420",
];

/// The value that means "deny nothing". An EMPTY override means the same
/// (the P1 env contract); the word exists because a config key cannot carry
/// an empty string — `roomler config set encoder_cells_deny ""` CLEARS the
/// key back to the built-in default, so "nothing" needs a spelling.
pub(crate) const DENY_NOTHING: &str = "none";

pub(crate) fn ffmpeg_444_capable(name: &str) -> bool {
    FFMPEG_444_CAPABLE.contains(&name)
}

/// Parse an override value into denylist entries. `""` and `none` (any
/// case) are the empty list; entries are trimmed, blanks dropped.
pub(crate) fn parse_denylist(value: &str) -> Vec<String> {
    if value.trim().eq_ignore_ascii_case(DENY_NOTHING) {
        return Vec::new();
    }
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// #3267 — the names the denylist knows a cell by when no FFmpeg table carries
/// it: `<codec>_<backend>` in the cell vocabulary's own spellings, which is
/// also what `encoder_cells_deny` validates. The probe opens these in its
/// `base` child and a session opens them in the daemon, so a fault in one has
/// to be able to cost that cell like any other.
pub const MF_H264: &str = "h264_mf";
pub const MF_HEVC: &str = "hevc_mf";
pub const MF_AV1: &str = "av1_mf";
pub const LIBVPX_VP9: &str = "vp9_libvpx";

/// #3267 — one cell this host taught itself not to open, because a `base`
/// probe child died inside it. Never written to the config: it lives in this
/// process and, when `confirmed`, in the probe cache, whose key is what makes
/// it heal (`caps_cache`).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LearnedDeny {
    /// `name:chroma`, the spelling `encoder_cells_deny` takes.
    pub cell: String,
    /// How the child ended, for the log and the hello's reason.
    pub why: String,
    /// Two children in a row ended inside this open with a driver fault's
    /// status. Only a confirmed cell may be cached; any other (a hang, a kill)
    /// is denied for this process's life and tested again at the next start.
    pub confirmed: bool,
}

/// What this process learned. `RwLock`, not `OnceLock`: the probe adds to it
/// between runs of its child, and a test has to be able to take it back.
static LEARNED: std::sync::RwLock<Vec<LearnedDeny>> = std::sync::RwLock::new(Vec::new());

/// The suffix of the variable that hands [`learned_for_child`]'s list to a
/// process this daemon starts. It has no config key: an operator's list is
/// `encoder_cells_deny`, and this one must never be mistaken for it.
const LEARNED_ENV: &str = "ENCODER_CELLS_LEARNED";

/// Replace this process's learned list.
pub(crate) fn install_learned(learned: Vec<LearnedDeny>) {
    *LEARNED.write().unwrap_or_else(|e| e.into_inner()) = learned;
}

/// What this process learned, with why.
pub fn learned() -> Vec<LearnedDeny> {
    LEARNED.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The learned cells in force here: this process's own, and those its parent
/// handed down. A `caps-probe` child or a recorder runs no probe of its own to
/// learn them from, and it opens the same drivers.
fn learned_names() -> Vec<String> {
    let mut names: Vec<String> = learned().into_iter().map(|l| l.cell).collect();
    if let Some(v) = tunnel_core::env::node_env(LEARNED_ENV) {
        for name in parse_denylist(&v) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// The environment that hands the learned cells to a process this daemon
/// starts. `std::env::set_var` is not an option in a process this threaded.
pub(crate) fn learned_for_child() -> Vec<(String, String)> {
    let names = learned_names();
    if names.is_empty() {
        return Vec::new();
    }
    vec![(format!("ROOMLERD_{LEARNED_ENV}"), names.join(","))]
}

/// #3267 — `ROOMLERD_CAPS_LEARN_DENY=0` / `caps_learn_deny = false` puts back
/// what came before: one `base` child, and a death in it means no hardware.
pub(crate) fn learning_enabled() -> bool {
    tunnel_core::env::node_env("CAPS_LEARN_DENY")
        .map(|v| {
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(true)
}

/// The list an operator controls: the env/config override when set (it
/// REPLACES the built-in, it does not add to it), else the built-in.
fn configured_denied_cells() -> Vec<String> {
    match tunnel_core::env::node_env("ENCODER_CELLS_DENY") {
        Some(v) => parse_denylist(&v),
        None => DEFAULT_DENIED_CELLS.iter().map(|s| s.to_string()).collect(),
    }
}

/// The effective denylist: the operator's list, plus what this host learned.
/// The learned cells add to whichever list is in force and replace nothing, so
/// `encoder_cells_deny` keeps meaning what it says: it still replaces the
/// built-in, and clearing it still goes back to the built-in.
pub(crate) fn denied_cells() -> Vec<String> {
    let mut deny = configured_denied_cells();
    for cell in learned_names() {
        if !deny.contains(&cell) {
            deny.push(cell);
        }
    }
    deny
}

pub(crate) fn cell_denied(deny: &[String], name: &str, chroma: ChromaFormat) -> bool {
    let key = format!("{name}:{}", chroma.wire());
    deny.iter().any(|d| d == &key)
}

/// The encoder names a 4:4:4 session may try for `codec`, in cascade order:
/// every backend the source matrix can open in 4:4:4, minus the denylist.
/// Empty when no backend of this codec does 4:4:4 (AV1) or every one is
/// denied — the caller then runs the 4:2:0 cascade and reports the truth.
///
/// #3267 — a cell whose 4:2:0 open ended a probe child takes its 4:4:4 form
/// with it. The `444` child only asks for what `base` opened, so that form has
/// not been tested, and `chroma_pref` is the controller's word, which nothing
/// checks against the hello. An operator's entry still names one form.
#[cfg(feature = "ffmpeg-encoder")]
pub(crate) fn names_444(codec: roomler_ai_remote_control::models::VideoCodec) -> Vec<&'static str> {
    let deny = denied_cells();
    let learned = learned_names();
    crate::encode::ffmpeg::FfmpegEncoder::cascade_names(codec)
        .iter()
        .copied()
        .filter(|name| {
            ffmpeg_444_capable(name)
                && !cell_denied(&deny, name, ChromaFormat::Yuv444)
                && !cell_denied(&learned, name, ChromaFormat::Yuv420)
        })
        .collect()
}

/// The encoder names a 4:2:0 session may try for `codec`, in cascade order:
/// the whole table minus the denylist. Empty when every cell of the codec is
/// denied — the open then fails with the dispatcher's "no candidates" error,
/// which is the truth (the hello advertised nothing either), never a quiet
/// fallback to the raw table.
///
/// FR-78 P3 field read (jupiter, 2026-09-08): with `hevc_vaapi:yuv420` denied
/// the probe advertised only the Vulkan cells, and a HEVC session then opened
/// `hevc_vaapi` anyway — the 4:2:0 constructors handed the static table to the
/// dispatcher, and only the probe and the 4:4:4 list read the denylist. A kill
/// switch the probe honours and a session ignores is not a kill switch: the
/// probe opens in a child process, a session opens in the daemon — the crash
/// the list exists to keep out of the daemon was the one thing it did not
/// keep out.
#[cfg(feature = "ffmpeg-encoder")]
pub(crate) fn names_420(codec: roomler_ai_remote_control::models::VideoCodec) -> Vec<&'static str> {
    let deny = denied_cells();
    let names: Vec<&'static str> = crate::encode::ffmpeg::FfmpegEncoder::cascade_names(codec)
        .iter()
        .copied()
        .filter(|name| !cell_denied(&deny, name, ChromaFormat::Yuv420))
        .collect();
    if names.is_empty() {
        tracing::warn!(
            ?codec,
            ?deny,
            "every 4:2:0 cell of this codec is on the device denylist — nothing to open"
        );
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_remote_control::models::{VideoBackend, VideoCodec};

    /// The 4:4:4 attempt list names only encoders the FFmpeg n9.0 sources can
    /// actually open in 4:4:4: nothing AV1 (`av1_nvenc` hard-errors "AV1 High
    /// Profile not supported"; every other AV1 backend lists 4:2:0 only),
    /// nothing AMF, nothing VideoToolbox — and every entry must be a name the
    /// vocabulary can split, or the probe would open it for nothing.
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
        assert!(ffmpeg_444_capable("vp9_qsv"));
        assert!(!ffmpeg_444_capable("av1_nvenc"));
        assert!(!ffmpeg_444_capable("hevc_amf"));
        assert!(!ffmpeg_444_capable("hevc_videotoolbox"));
    }

    /// The denylist is the kill switch: the built-in default keeps the
    /// unproven cells closed, the override replaces it wholesale, and an
    /// explicitly EMPTY override — or the word `none` — denies nothing.
    #[test]
    fn denylist_default_env_override_empty_and_none() {
        use tunnel_core::env::test_env::Saved;
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _saved = Saved::cleared("ENCODER_CELLS_DENY");

        let deny = denied_cells();
        assert!(cell_denied(&deny, "hevc_qsv", ChromaFormat::Yuv444));
        assert!(cell_denied(&deny, "hevc_vaapi", ChromaFormat::Yuv444));
        assert!(
            cell_denied(&deny, "vp9_qsv", ChromaFormat::Yuv444),
            "CORPLAP-3, 2026-09-08"
        );
        assert!(!cell_denied(&deny, "hevc_qsv", ChromaFormat::Yuv420));
        assert!(!cell_denied(&deny, "hevc_nvenc", ChromaFormat::Yuv444));
        assert_eq!(
            cell_denied(&deny, "hevc_vulkan", ChromaFormat::Yuv420),
            cfg!(windows),
            "#3267: AMD's driver faults in this open on Windows, and only there"
        );
        assert!(
            !cell_denied(&deny, "h264_vulkan", ChromaFormat::Yuv420),
            "#3267: the same driver opens the H.264 form"
        );

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
        unsafe { tunnel_core::env::test_env::set("ENCODER_CELLS_DENY", " None ") };
        assert!(denied_cells().is_empty(), "`none` denies nothing, any case");
    }

    /// Clears this process's learned list when a test ends, pass or fail.
    struct LearnedGuard;
    impl Drop for LearnedGuard {
        fn drop(&mut self) {
            install_learned(Vec::new());
        }
    }

    fn learned_cell(cell: &str) -> LearnedDeny {
        LearnedDeny {
            cell: cell.to_string(),
            why: "exit code: 0xc0000005".to_string(),
            confirmed: true,
        }
    }

    /// #3267 — a learned cell adds to whichever list is in force and replaces
    /// nothing. `encoder_cells_deny` keeps meaning what it always meant: it
    /// still REPLACES the built-in, and clearing it (`roomler config clear`)
    /// still goes back to the built-in. A learned cell must never read as an
    /// override, or `none` would start denying what the operator allowed by
    /// accident of a fault, and clearing would not clear.
    #[test]
    fn a_learned_cell_adds_to_the_list_in_force_and_replaces_nothing() {
        use tunnel_core::env::test_env::{Saved, set};
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _deny = Saved::cleared("ENCODER_CELLS_DENY");
        let _handed = Saved::cleared(LEARNED_ENV);
        let _learned = LearnedGuard;
        install_learned(vec![learned_cell("h264_nvenc:yuv420")]);

        // The built-in stands, and the learned cell is on top of it.
        let deny = denied_cells();
        assert!(cell_denied(&deny, "h264_nvenc", ChromaFormat::Yuv420));
        assert!(cell_denied(&deny, "hevc_qsv", ChromaFormat::Yuv444));
        assert_eq!(
            configured_denied_cells(),
            DEFAULT_DENIED_CELLS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            "nothing learned ever reaches the list an operator controls"
        );

        // An override replaces the built-in and not the learned cell.
        // SAFETY: serialised by DENY_ENV_LOCK; the guards restore both.
        unsafe { set("ENCODER_CELLS_DENY", "vp9_qsv:yuv420") };
        let deny = denied_cells();
        assert!(cell_denied(&deny, "vp9_qsv", ChromaFormat::Yuv420));
        assert!(cell_denied(&deny, "h264_nvenc", ChromaFormat::Yuv420));
        assert!(
            !cell_denied(&deny, "hevc_qsv", ChromaFormat::Yuv444),
            "the override still REPLACES the built-in"
        );

        // `none` denies nothing an operator can name; the driver's fault is
        // not theirs to wave away by that word.
        unsafe { set("ENCODER_CELLS_DENY", "none") };
        assert_eq!(denied_cells(), vec!["h264_nvenc:yuv420".to_string()]);

        // The same entry on both lists is one entry.
        unsafe { set("ENCODER_CELLS_DENY", "h264_nvenc:yuv420") };
        assert_eq!(denied_cells(), vec!["h264_nvenc:yuv420".to_string()]);

        // A forgotten list leaves exactly the operator's.
        install_learned(Vec::new());
        unsafe { set("ENCODER_CELLS_DENY", "none") };
        assert!(denied_cells().is_empty());
    }

    /// #3267 — a process the daemon starts honours the daemon's list: the probe
    /// child that must not open the cell again, and a recorder that opens the
    /// same drivers. It reaches them as real env (`learned_for_child`), the only
    /// way across a process boundary, and nothing at all when there is nothing.
    #[test]
    fn the_learned_list_crosses_into_a_child_process() {
        use tunnel_core::env::test_env::{Saved, set};
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _deny = Saved::cleared("ENCODER_CELLS_DENY");
        let _handed = Saved::cleared(LEARNED_ENV);
        let _learned = LearnedGuard;
        assert!(learned_for_child().is_empty(), "nothing to hand over");

        install_learned(vec![
            learned_cell("hevc_vulkan:yuv420"),
            learned_cell("h264_amf:yuv420"),
        ]);
        assert_eq!(
            learned_for_child(),
            vec![(
                "ROOMLERD_ENCODER_CELLS_LEARNED".to_string(),
                "hevc_vulkan:yuv420,h264_amf:yuv420".to_string()
            )]
        );

        // The child's side: nothing installed, the list in its environment.
        install_learned(Vec::new());
        // SAFETY: serialised by DENY_ENV_LOCK; the guard restores it.
        unsafe { set(LEARNED_ENV, "hevc_vulkan:yuv420,h264_amf:yuv420") };
        let deny = denied_cells();
        assert!(cell_denied(&deny, "hevc_vulkan", ChromaFormat::Yuv420));
        assert!(cell_denied(&deny, "h264_amf", ChromaFormat::Yuv420));
        assert_eq!(
            learned_for_child().len(),
            1,
            "and a child can hand it on to one of its own"
        );
    }

    /// #3267 — the kill switch reads the usual spellings, default on.
    #[test]
    fn the_learning_switch_reads_the_usual_spellings() {
        use tunnel_core::env::test_env::{Saved, set};
        let _saved = Saved::cleared("CAPS_LEARN_DENY");
        assert!(learning_enabled(), "default on");
        for off in ["0", "false", "OFF", "no"] {
            // SAFETY: no other test touches this suffix.
            unsafe { set("CAPS_LEARN_DENY", off) };
            assert!(!learning_enabled(), "{off} must switch it off");
        }
        unsafe { set("CAPS_LEARN_DENY", "1") };
        assert!(learning_enabled());
    }

    /// #3267 — requirement 3: a session opens encoders in the daemon, where the
    /// fault that ended the probe's child would end the daemon. Every entry a
    /// session has honours a learned cell: both cascades, and the single-name
    /// rebuild, which refuses before it touches a driver. `hevc_vaapi` so that
    /// no other test on a Windows box is affected while the lock is held.
    #[cfg(feature = "ffmpeg-encoder")]
    #[test]
    fn a_session_cannot_reach_a_learned_cell() {
        use crate::encode::ffmpeg::FfmpegEncoder;
        use tunnel_core::env::test_env::Saved;
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _deny = Saved::cleared("ENCODER_CELLS_DENY");
        let _handed = Saved::cleared(LEARNED_ENV);
        let _learned = LearnedGuard;
        assert!(names_420(VideoCodec::Hevc).contains(&"hevc_vaapi"));

        install_learned(vec![
            learned_cell("hevc_vaapi:yuv420"),
            learned_cell("hevc_nvenc:yuv444"),
        ]);
        let hevc = names_420(VideoCodec::Hevc);
        assert!(!hevc.contains(&"hevc_vaapi"), "{hevc:?}");
        assert!(
            hevc.contains(&"hevc_nvenc"),
            "a deny names one form: {hevc:?}"
        );
        assert!(
            !names_444(VideoCodec::Hevc).contains(&"hevc_nvenc"),
            "the 4:4:4 cascade reads it too"
        );

        let refused =
            FfmpegEncoder::new_preferred("hevc_vaapi", 480, 270, 30, 3_000_000, 0, false).err();
        assert!(
            refused
                .as_ref()
                .is_some_and(|e| e.to_string().contains("denylist")),
            "a rebuild must refuse it, not open it: {refused:?}"
        );
    }

    /// #3267 — the hello leaves a learned cell out, and a controller may ask for
    /// 4:4:4 regardless: nothing checks `chroma_pref` against the hello. The 4:4:4
    /// form of a cell whose 4:2:0 open killed a process has never been opened, so
    /// the daemon must not be the first to try it. An operator's entry still
    /// names one form.
    #[cfg(feature = "ffmpeg-encoder")]
    #[test]
    fn a_learned_420_cell_takes_its_444_form_with_it() {
        use tunnel_core::env::test_env::{Saved, set};
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _deny = Saved::cleared("ENCODER_CELLS_DENY");
        let _handed = Saved::cleared(LEARNED_ENV);
        let _learned = LearnedGuard;
        assert_eq!(names_444(VideoCodec::H264), vec!["h264_nvenc"]);

        install_learned(vec![learned_cell("h264_nvenc:yuv420")]);
        assert!(names_444(VideoCodec::H264).is_empty());
        assert_eq!(
            names_444(VideoCodec::Hevc),
            vec!["hevc_nvenc"],
            "another codec's cell is another cell"
        );

        install_learned(Vec::new());
        // SAFETY: serialised by DENY_ENV_LOCK; the guard restores it.
        unsafe { set("ENCODER_CELLS_DENY", "h264_nvenc:yuv420") };
        assert_eq!(
            names_444(VideoCodec::H264),
            vec!["h264_nvenc"],
            "an operator's entry names one form"
        );
    }

    /// #3267 — Media Foundation opens through one constructor, and a session
    /// opens it in the daemon. `av1_mf`: no other test opens it.
    #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
    #[test]
    fn a_media_foundation_cell_can_be_denied() {
        use tunnel_core::env::test_env::Saved;
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _deny = Saved::cleared("ENCODER_CELLS_DENY");
        let _handed = Saved::cleared(LEARNED_ENV);
        let _learned = LearnedGuard;
        install_learned(vec![learned_cell("av1_mf:yuv420")]);
        let refused = crate::encode::mf::MfEncoder::new_av1(480, 270).err();
        assert!(
            refused
                .as_ref()
                .is_some_and(|e| e.to_string().contains("denylist")),
            "the open must be refused before it reaches a driver: {refused:?}"
        );
    }

    /// The names the probe announces for its non-FFmpeg cells are ones
    /// `encoder_cells_deny` can name, so a learned entry and an operator's mean
    /// the same thing (the config validates lowercase, digits, underscores).
    #[test]
    fn the_non_ffmpeg_cell_names_are_plain_deny_names() {
        for name in [MF_H264, MF_HEVC, MF_AV1, LIBVPX_VP9] {
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{name}"
            );
            let (codec, backend) = name.split_once('_').expect("codec_backend");
            assert!(VideoCodec::from_wire(codec).is_some(), "{name}");
            assert!(["mf", "libvpx"].contains(&backend), "{name}");
        }
    }

    #[test]
    fn parse_denylist_trims_and_drops_blanks() {
        assert_eq!(
            parse_denylist("a:yuv444, b:yuv420 ,,"),
            vec!["a:yuv444", "b:yuv420"]
        );
        assert!(parse_denylist("").is_empty());
        assert!(parse_denylist("NONE").is_empty());
    }

    /// The pump's 4:4:4 cascade is the probe's attempt list minus the
    /// denylist, in cascade order — so a session can never open a cell the
    /// hello did not advertise.
    #[cfg(feature = "ffmpeg-encoder")]
    #[test]
    fn names_444_follow_the_cascade_minus_the_denylist() {
        use tunnel_core::env::test_env::Saved;
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _saved = Saved::cleared("ENCODER_CELLS_DENY");
        assert_eq!(
            names_444(VideoCodec::Hevc),
            vec!["hevc_nvenc"],
            "hevc_qsv is denied by default"
        );
        assert!(
            names_444(VideoCodec::Vp9).is_empty(),
            "vp9_qsv 4:4:4 is denied by default"
        );
        assert_eq!(names_444(VideoCodec::H264), vec!["h264_nvenc"]);
        assert!(names_444(VideoCodec::Av1).is_empty());
        unsafe { tunnel_core::env::test_env::set("ENCODER_CELLS_DENY", "none") };
        assert_eq!(
            names_444(VideoCodec::Hevc),
            vec!["hevc_nvenc", "hevc_qsv", "hevc_vaapi", "hevc_vulkan"]
        );
        assert_eq!(names_444(VideoCodec::Vp9), vec!["vp9_qsv", "vp9_vaapi"]);
    }

    /// FR-78 P3 (jupiter, 2026-09-08): with `hevc_vaapi:yuv420` denied the
    /// probe advertised only the Vulkan cells and a session opened
    /// `hevc_vaapi` anyway — the 4:2:0 cascades never read the list. The
    /// session's 4:2:0 list is the cascade minus the denylist, like the 4:4:4
    /// one, so what the hello advertises is all a session can reach.
    #[cfg(feature = "ffmpeg-encoder")]
    #[test]
    fn names_420_follow_the_cascade_minus_the_denylist() {
        use crate::encode::ffmpeg::FfmpegEncoder;
        use tunnel_core::env::test_env::Saved;
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _saved = Saved::cleared("ENCODER_CELLS_DENY");
        for codec in [VideoCodec::Hevc, VideoCodec::H264, VideoCodec::Vp9] {
            // #3267 — apart from Windows' `hevc_vulkan`. The lane that runs this
            // is Linux's, so only a native Windows run sees that entry.
            let expected: Vec<&str> = FfmpegEncoder::cascade_names(codec)
                .iter()
                .copied()
                .filter(|name| !(cfg!(windows) && *name == "hevc_vulkan"))
                .collect();
            assert_eq!(
                names_420(codec),
                expected,
                "the built-in denylist names no other 4:2:0 cell of this codec"
            );
        }
        let av1 = names_420(VideoCodec::Av1);
        assert!(
            !av1.contains(&"av1_vulkan"),
            "av1_vulkan:yuv420 is denied by default (its frames do not decode): {av1:?}"
        );
        assert_eq!(
            av1.len(),
            FfmpegEncoder::cascade_names(VideoCodec::Av1).len() - 1,
            "and it is the only AV1 cell the built-in list removes"
        );
        unsafe {
            tunnel_core::env::test_env::set(
                "ENCODER_CELLS_DENY",
                "hevc_vaapi:yuv420,h264_vaapi:yuv420",
            )
        };
        let hevc = names_420(VideoCodec::Hevc);
        assert!(!hevc.contains(&"hevc_vaapi"), "{hevc:?}");
        assert!(
            hevc.contains(&"hevc_nvenc") && hevc.contains(&"hevc_vulkan"),
            "{hevc:?}"
        );
        assert_eq!(
            hevc.last(),
            Some(&"hevc_vulkan"),
            "the order is the cascade's"
        );
        assert!(!names_420(VideoCodec::H264).contains(&"h264_vaapi"));
        assert_eq!(
            names_420(VideoCodec::Av1),
            FfmpegEncoder::cascade_names(VideoCodec::Av1).to_vec(),
            "a deny names ONE cell, not a backend"
        );
        unsafe {
            tunnel_core::env::test_env::set(
                "ENCODER_CELLS_DENY",
                "hevc_nvenc:yuv420,hevc_qsv:yuv420,hevc_amf:yuv420,hevc_videotoolbox:yuv420,hevc_vaapi:yuv420,hevc_d3d12va:yuv420,hevc_vulkan:yuv420",
            )
        };
        assert!(
            names_420(VideoCodec::Hevc).is_empty(),
            "every cell denied ⇒ nothing to try, never a fallback to the raw table"
        );
    }
}
