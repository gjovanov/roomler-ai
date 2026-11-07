// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-135 R9 — one table of encoder cells, and the hello's encoder fields
//! derived from it.
//!
//! Every cell the agent can advertise is a row of [`CELLS`]: codec × backend ×
//! chroma, with what the code already knows about it (the paths it serves, its
//! input forms, how a rate move lands, whether its colour is described, the
//! platforms and the feature that carry it, where its `hw` comes from) and how
//! the pre-FR-77 wire spells it ([`Legacy`]). A host's probe adds an overlay of
//! proof ([`CellProof`], from [`ProbeFindings`]), and [`derive`] writes from the
//! two the six fields a probe answers: `hw_encoders`, `codecs`, `transports`,
//! `vp9_chroma`, `hevc_chroma` and `video_cells` ([`ProbedFields`]).
//!
//! **It drives nothing.** `caps::compute_caps_with` keeps writing the real
//! hello, through `caps::assemble`, from the same [`ProbeFindings`]. The table
//! derives beside it: a gate over recorded platform fixtures
//! (`tests/fixtures/cell-table.hellos.json`) proves the two byte-identical, and
//! [`shadow`] says once, at the real hello's build, if a host ever makes them
//! differ. That is FR-70 M3a's pattern, a shadow and a count of where it
//! disagrees. Where the old wire says something other than the row's own truth
//! is a named [`Divergence`], read per fixture: FR-135 §5.7's Dv rows.
//!
//! Columns of the spec's `CellSpec` with no source in the code today are left
//! out, never guessed: `max_tier` (no reader holds a proven tier per cell), and
//! in [`CellProof`] the frame counts, the decode check, the GPU and shared-device
//! proofs, the VUI reorder read, the last fault and the build id. An unmeasured
//! fact the row does carry says so ([`Describes::Unread`], `idr: None`).

use std::sync::atomic::{AtomicBool, Ordering};

use roomler_ai_remote_control::models::{ChromaFormat, VideoBackend, VideoCell, VideoCodec};
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

// ── The columns ──────────────────────────────────────────────────────────

/// A cell's stable name: the encoder as the denylist spells it, then the
/// chroma (`hevc_qsv:yuv420`, `hevc_d3d12va:yuv420`, `h264_mf:yuv420`). It is
/// exactly an `encoder_cells_deny` entry, so a list and a row compare by
/// equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellId(&'static str);

impl CellId {
    pub const fn wire(self) -> &'static str {
        self.0
    }

    /// The encoder's name, the part before the chroma.
    pub fn name(self) -> &'static str {
        self.0.split_once(':').map_or(self.0, |(name, _)| name)
    }
}

/// The encoder family that implements a row, in the spellings
/// `encoder-smoke`'s build line prints (`encode/bench.rs`, `BuildCaps`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    MediaFoundation,
    Openh264,
    Ffmpeg,
    Libvpx,
}

impl Family {
    pub const ALL: [Family; 4] = [
        Self::MediaFoundation,
        Self::Openh264,
        Self::Ffmpeg,
        Self::Libvpx,
    ];

    pub const fn wire(self) -> &'static str {
        match self {
            Self::MediaFoundation => "mf",
            Self::Openh264 => "openh264",
            Self::Ffmpeg => "ffmpeg",
            Self::Libvpx => "libvpx",
        }
    }
}

/// What a row's frames arrive as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputForm {
    /// Pixels in memory, converted on the CPU. Every row.
    Cpu,
    /// FR-118 — a D3D11 texture on the capture device (`ffmpeg::gpu_input`).
    /// Windows only, QSV's and NVENC's 4:2:0 encoders.
    D3d11,
    /// FR-118 P5 — a CVPixelBuffer over an IOSurface (`ffmpeg::vt_input`).
    /// macOS only, VideoToolbox's 4:2:0 HEVC and H.264.
    IoSurface,
}

impl InputForm {
    /// The one platform a GPU form exists on; `None` for the CPU form.
    pub const fn platform(self) -> Option<Platform> {
        match self {
            Self::Cpu => None,
            Self::D3d11 => Some(Platform::Windows),
            Self::IoSurface => Some(Platform::Macos),
        }
    }
}

/// A media path a row can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaPath {
    /// A DataChannel pump (`data-channel-*`).
    Dc,
    /// The RTP track's opener (`encode::open_for_codec`).
    Rtp,
    /// The screen recorder (`recording::child::encoder_factory`).
    Recording,
}

/// How a rate move lands on a row (FR-62).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateMove {
    /// The encoder takes the new rate where it is. `idr`: whether that costs a
    /// keyframe, `None` where the code states nothing.
    InPlace { idr: Option<bool> },
    /// A rate move rebuilds the encoder, and a rebuild ships an IDR.
    Rebuild,
    /// QSV: a rebuild, or in place with a keyframe assumed once
    /// `encoder_inplace_rate` is on (`resolve_rate_mode`).
    RebuildOrInPlace,
}

/// Whether a row's bitstream carries the colour description it was converted
/// with (FR-130).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Describes {
    /// Matrix, range, primaries and transfer, as read from its output.
    Full,
    /// Matrix and range only: VP9's header has no field for the rest.
    Partial,
    /// Writes no description, or an unspecified matrix.
    Unwritten,
    /// Nobody has read this row's output.
    Unread,
}

/// Where a row's `hw` comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HwTruth {
    /// Hardware by construction: an open is proof.
    Hardware,
    /// Software by construction.
    Software,
    /// The probe says: Media Foundation reports what its cascade landed on.
    ByProbe,
    /// The build says: a QSV open is hardware only on the oneVPL build
    /// (`qsv_is_hardware_by_construction`).
    ByBuild,
}

/// A platform a release ships for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Windows,
    LinuxX64,
    LinuxArm64,
    Macos,
}

/// What a row needs besides its own open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Factor {
    /// The cargo feature that compiles the family in.
    Feature(&'static str),
    /// A platform whose release build carries the encoder.
    Platform(Platform),
    /// A driver the row needs, or its floor.
    Driver(&'static str),
    /// A setting the host must carry for the open to succeed.
    DropIn(&'static str),
}

/// How the pre-FR-77 fields spell a row. Every way they differ from the row's
/// own truth is here as a column, so the difference is a fact of the table and
/// [`divergences`] can read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Legacy {
    /// Its group's winner writes the codec into `codecs`. Dv2: `codecs` is the
    /// union of what OpenH264, Media Foundation and FFmpeg's HEVC and AV1
    /// winners write, not a statement about any one path.
    pub in_codecs: bool,
    /// The `transports` entry its group's winner writes.
    pub transport: Option<&'static str>,
    /// This row's chroma joins `hevc_chroma` when its encoder is the HEVC
    /// winner. P7: 4:4:4 only for `hevc_nvenc` (ledger enc:I146).
    pub hevc_chroma: bool,
    /// Dv4: the label rides enumeration, not the cell (`mf-h264-hw`).
    pub label_on_enumeration: bool,
    /// Dv6: the cell's `hw` is the QSV build flag, whichever VP9 backend won
    /// (ledger enc:I185).
    pub hw_from_qsv_build: bool,
}

const NO_LEGACY: Legacy = Legacy {
    in_codecs: false,
    transport: None,
    hevc_chroma: false,
    label_on_enumeration: false,
    hw_from_qsv_build: false,
};

pub const DC_HEVC: &str = "data-channel-hevc";
pub const DC_AV1: &str = "data-channel-av1";
pub const DC_H264: &str = "data-channel-h264";
/// Stays on the wire forever, although it carries both chroma forms.
pub const DC_VP9_444: &str = "data-channel-vp9-444";

/// One encoder cell (FR-135 §6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellSpec {
    pub id: CellId,
    pub codec: VideoCodec,
    pub backend: VideoBackend,
    pub family: Family,
    pub ffmpeg_name: Option<&'static str>,
    pub chroma: ChromaFormat,
    pub input_forms: &'static [InputForm],
    pub paths: &'static [MediaPath],
    pub rate_move: RateMove,
    /// The DataChannel pump's maxrate factor for the row, in percent: the
    /// codec's built-in times the chroma column's (`rate_profile`), as
    /// `policy::rate_plan` multiplies them. `None` off FFmpeg's pump.
    pub rate_factor_pct: Option<u16>,
    pub describes: Describes,
    pub factors: &'static [Factor],
    pub hw: HwTruth,
    /// A software-only answer from the probe is still advertised (`ByProbe`
    /// rows): MF H.264 yes, MF HEVC and AV1 only with `ROOMLERD_ALLOW_SW_HEAVY`.
    pub advertise_software: bool,
    pub legacy: Legacy,
}

impl CellSpec {
    /// The form whose open decides whether the encoder is there at all: the
    /// other is asked only after it. 4:2:0 everywhere but libvpx, which opens
    /// 4:4:4 first (`caps::child::withdraw` reads the same rule).
    pub fn opens_first(&self) -> bool {
        let first = if self.family == Family::Libvpx {
            ChromaFormat::Yuv444
        } else {
            ChromaFormat::Yuv420
        };
        self.chroma == first
    }

    /// The legacy fields name one encoder per group: FFmpeg's first backend in
    /// cascade order that opens, per codec; every other encoder on its own.
    fn group(&self) -> (Family, VideoCodec, Option<VideoBackend>) {
        let backend = (self.family != Family::Ffmpeg).then_some(self.backend);
        (self.family, self.codec, backend)
    }

    /// The `hw_encoders` label, rendered from the row. Dv4: Media Foundation's
    /// says `-hw` whatever its cell says.
    pub fn legacy_label(&self) -> String {
        match self.family {
            Family::Openh264 => "openh264-sw".to_string(),
            Family::MediaFoundation => format!("mf-{}-hw", negotiation_name(self.codec)),
            Family::Ffmpeg => format!("ffmpeg-{}", self.id.name()),
            Family::Libvpx => "libvpx-vp9-444-sw".to_string(),
        }
    }

    pub fn has_path(&self, path: MediaPath) -> bool {
        self.paths.contains(&path)
    }

    pub fn on_platform(&self, platform: Platform) -> bool {
        self.factors.contains(&Factor::Platform(platform))
    }
}

/// The codec as `codecs` and the negotiation spell it (`h265`, not `hevc`).
pub fn negotiation_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "h265",
        VideoCodec::Av1 => "av1",
        VideoCodec::Vp9 => "vp9",
    }
}

// ── The rows ─────────────────────────────────────────────────────────────

/// Every cell the agent can advertise, in the order the probe opens them:
/// OpenH264, Media Foundation, FFmpeg's VP9 cascade, FFmpeg's HEVC, AV1 and
/// H.264 cascades (`encode/ffmpeg/encoder.rs`, `*_ENCODER_NAMES`), libvpx.
/// The hello's lists come out in this order, so it is part of the wire.
///
/// Sources, one per column. Platforms: the vendored FFmpeg builds'
/// `--enable-encoder` lists (`vendor-ffmpeg-windows.yml`, which also builds
/// both Linux trees, and `vendor-ffmpeg-macos.yml`), read by a test. Paths:
/// `open_for_codec` (RTP: H.264 through MF then OpenH264, HEVC through FFmpeg's
/// cascade then MF, AV1 through MF alone), the recorder's FFmpeg H.264 cascade
/// then OpenH264, and the DataChannel pumps. Input forms: `gpu_input::gpu_cell`
/// and `vt_input::gpu_cell`. Rate moves: `resolve_rate_mode` and
/// `reconfig_forces_idr_for`, and libvpx's `EncoderCaps`. Colour: FR-130's
/// reads (P1 on the dev box, P3 for QSV and VideoToolbox). `hw`: the probe's
/// assembly and `qsv_is_hardware_by_construction`.
pub static CELLS: &[CellSpec] = rows::CELLS;

mod rows {
    use super::*;
    use ChromaFormat::{Yuv420, Yuv444};
    use Describes::{Full, Partial, Unread, Unwritten};
    use Platform::{LinuxArm64, LinuxX64, Macos, Windows};
    use VideoBackend::{Amf, D3d12, Nvenc, Qsv, Vaapi, VideoToolbox, Vulkan};
    use VideoCodec::{Av1, H264, Hevc, Vp9};

    const CPU: &[InputForm] = &[InputForm::Cpu];
    const CPU_D3D11: &[InputForm] = &[InputForm::Cpu, InputForm::D3d11];
    const CPU_IOSURFACE: &[InputForm] = &[InputForm::Cpu, InputForm::IoSurface];

    const DC: &[MediaPath] = &[MediaPath::Dc];
    const DC_RTP: &[MediaPath] = &[MediaPath::Dc, MediaPath::Rtp];
    const DC_REC: &[MediaPath] = &[MediaPath::Dc, MediaPath::Recording];
    const RTP: &[MediaPath] = &[MediaPath::Rtp];
    const RTP_REC: &[MediaPath] = &[MediaPath::Rtp, MediaPath::Recording];

    const FFMPEG: Factor = Factor::Feature("ffmpeg-encoder");
    const F_OPENH264: &[Factor] = &[
        Factor::Feature("openh264-encoder"),
        Factor::Platform(Windows),
        Factor::Platform(LinuxX64),
        Factor::Platform(LinuxArm64),
        Factor::Platform(Macos),
    ];
    const F_MF: &[Factor] = &[Factor::Feature("mf-encoder"), Factor::Platform(Windows)];
    const F_LIBVPX: &[Factor] = &[
        Factor::Feature("vp9-444"),
        Factor::Platform(Windows),
        Factor::Platform(LinuxX64),
        Factor::Platform(LinuxArm64),
        Factor::Platform(Macos),
    ];

    /// What an FFmpeg backend brings to each of its rows.
    #[derive(Clone, Copy)]
    struct Backend {
        factors: &'static [Factor],
        hw: HwTruth,
        rate_move: RateMove,
    }

    const NVENC: Backend = Backend {
        factors: &[
            FFMPEG,
            Factor::Platform(Windows),
            Factor::Platform(LinuxX64),
            Factor::Platform(LinuxArm64),
            // The nv-codec-headers pin is the floor (docs/encoders.md).
            Factor::Driver("NVIDIA 570 on Windows, 530.41 on Linux"),
        ],
        hw: HwTruth::Hardware,
        // The vendored FFmpeg's nvenc patch: 0 of 20 rate moves were IDRs.
        rate_move: RateMove::InPlace { idr: Some(false) },
    };
    const QSV: Backend = Backend {
        factors: &[
            FFMPEG,
            Factor::Platform(Windows),
            Factor::Platform(LinuxX64),
        ],
        hw: HwTruth::ByBuild,
        rate_move: RateMove::RebuildOrInPlace,
    };
    const AMF: Backend = Backend {
        factors: &[
            FFMPEG,
            Factor::Platform(Windows),
            Factor::Platform(LinuxX64),
            Factor::Driver("AMD's proprietary AMF runtime on Linux"),
        ],
        hw: HwTruth::Hardware,
        rate_move: RateMove::Rebuild,
    };
    const VT: Backend = Backend {
        factors: &[FFMPEG, Factor::Platform(Macos)],
        hw: HwTruth::Hardware,
        rate_move: RateMove::Rebuild,
    };
    const VAAPI: Backend = Backend {
        factors: &[
            FFMPEG,
            Factor::Platform(LinuxX64),
            Factor::Platform(LinuxArm64),
        ],
        hw: HwTruth::Hardware,
        rate_move: RateMove::Rebuild,
    };
    const D3D12: Backend = Backend {
        factors: &[FFMPEG, Factor::Platform(Windows)],
        hw: HwTruth::Hardware,
        rate_move: RateMove::Rebuild,
    };
    const VULKAN: Backend = Backend {
        factors: &[
            FFMPEG,
            Factor::Platform(Windows),
            Factor::Platform(LinuxX64),
            Factor::Platform(LinuxArm64),
            // RADV exposes video encode only behind it (FR-78).
            Factor::DropIn("RADV_PERFTEST=video_encode on RADV"),
        ],
        hw: HwTruth::Hardware,
        rate_move: RateMove::Rebuild,
    };

    // What each FFmpeg cascade's winner writes into the legacy fields.
    const L_HEVC: Legacy = Legacy {
        in_codecs: true,
        transport: Some(DC_HEVC),
        hevc_chroma: true,
        ..NO_LEGACY
    };
    /// A HEVC 4:4:4 form P7's legacy field never names.
    const L_HEVC_P7: Legacy = Legacy {
        hevc_chroma: false,
        ..L_HEVC
    };
    const L_AV1: Legacy = Legacy {
        in_codecs: true,
        transport: Some(DC_AV1),
        ..NO_LEGACY
    };
    const L_H264: Legacy = Legacy {
        transport: Some(DC_H264),
        ..NO_LEGACY
    };
    /// FFmpeg's VP9 rides libvpx's transport and names no codec.
    const L_VP9: Legacy = Legacy {
        hw_from_qsv_build: true,
        ..NO_LEGACY
    };

    /// The pump's maxrate factor: the codec's built-in times the chroma
    /// column's, as `rate_plan` multiplies them (`rate_profile`).
    const fn rate_factor(codec: VideoCodec, chroma: ChromaFormat) -> u16 {
        let codec_pct: u32 = match codec {
            H264 => 150,
            Hevc | Vp9 => 125,
            Av1 => 100,
        };
        let chroma_pct: u32 = match chroma {
            Yuv420 => 100,
            Yuv444 => 150,
        };
        (codec_pct * chroma_pct / 100) as u16
    }

    /// An FFmpeg row. `name` is the cascade table's, `id` the same name with
    /// its chroma (a test holds the two together).
    #[allow(clippy::too_many_arguments)]
    const fn ff(
        id: &'static str,
        name: &'static str,
        codec: VideoCodec,
        backend: VideoBackend,
        chroma: ChromaFormat,
        input_forms: &'static [InputForm],
        paths: &'static [MediaPath],
        describes: Describes,
        b: Backend,
        legacy: Legacy,
    ) -> CellSpec {
        CellSpec {
            id: CellId(id),
            codec,
            backend,
            family: Family::Ffmpeg,
            ffmpeg_name: Some(name),
            chroma,
            input_forms,
            paths,
            rate_move: b.rate_move,
            rate_factor_pct: Some(rate_factor(codec, chroma)),
            describes,
            factors: b.factors,
            hw: b.hw,
            advertise_software: false,
            legacy,
        }
    }

    /// Compiled in, never opened: software by definition, so it is a cell even
    /// when the hardware probes are not run.
    const OPENH264: CellSpec = CellSpec {
        id: CellId("h264_openh264:yuv420"),
        codec: H264,
        backend: VideoBackend::Openh264,
        family: Family::Openh264,
        ffmpeg_name: None,
        chroma: Yuv420,
        input_forms: CPU,
        paths: RTP_REC,
        // `apply_runtime_bitrate`: an option on the live encoder.
        rate_move: RateMove::InPlace { idr: None },
        rate_factor_pct: None,
        describes: Full,
        factors: F_OPENH264,
        hw: HwTruth::Software,
        advertise_software: true,
        legacy: Legacy {
            in_codecs: true,
            ..NO_LEGACY
        },
    };

    /// The agent's own Media Foundation module: the RTP track's H.264, HEVC
    /// and AV1 on Windows. `SetBitrate` is a command to its worker.
    const fn mf(id: &'static str, codec: VideoCodec, describes: Describes) -> CellSpec {
        let h264 = matches!(codec, H264);
        CellSpec {
            id: CellId(id),
            codec,
            backend: VideoBackend::MediaFoundation,
            family: Family::MediaFoundation,
            ffmpeg_name: None,
            chroma: Yuv420,
            input_forms: CPU,
            paths: RTP,
            rate_move: RateMove::InPlace { idr: None },
            rate_factor_pct: None,
            describes,
            factors: F_MF,
            hw: HwTruth::ByProbe,
            advertise_software: h264,
            legacy: Legacy {
                in_codecs: !h264,
                label_on_enumeration: h264,
                ..NO_LEGACY
            },
        }
    }

    /// libvpx VP9, the software floor on the DataChannel. Its caps say a rate
    /// move is in place and costs no keyframe (`libvpx.rs`).
    const fn libvpx(id: &'static str, chroma: ChromaFormat) -> CellSpec {
        CellSpec {
            id: CellId(id),
            codec: Vp9,
            backend: VideoBackend::Libvpx,
            family: Family::Libvpx,
            ffmpeg_name: None,
            chroma,
            input_forms: CPU,
            paths: DC,
            rate_move: RateMove::InPlace { idr: Some(false) },
            rate_factor_pct: None,
            describes: Partial,
            factors: F_LIBVPX,
            hw: HwTruth::Software,
            advertise_software: true,
            legacy: Legacy {
                transport: Some(DC_VP9_444),
                ..NO_LEGACY
            },
        }
    }

    #[rustfmt::skip]
    pub(super) const CELLS: &[CellSpec] = &[
        OPENH264,
        // MF writes no description (FR-130 P1, Microsoft's encoders); AV1's is unread.
        mf("h264_mf:yuv420", H264, Unwritten),
        mf("hevc_mf:yuv420", Hevc, Unwritten),
        mf("av1_mf:yuv420", Av1, Unread),
        //  id                          name                 codec backend       chroma  inputs         paths   colour     backend legacy
        ff("vp9_qsv:yuv420",           "vp9_qsv",           Vp9,  Qsv,          Yuv420, CPU,           DC,     Unwritten, QSV,    L_VP9),
        ff("vp9_qsv:yuv444",           "vp9_qsv",           Vp9,  Qsv,          Yuv444, CPU,           DC,     Unwritten, QSV,    L_VP9),
        ff("vp9_vaapi:yuv420",         "vp9_vaapi",         Vp9,  Vaapi,        Yuv420, CPU,           DC,     Unwritten, VAAPI,  L_VP9),
        ff("vp9_vaapi:yuv444",         "vp9_vaapi",         Vp9,  Vaapi,        Yuv444, CPU,           DC,     Unwritten, VAAPI,  L_VP9),
        ff("hevc_nvenc:yuv420",        "hevc_nvenc",        Hevc, Nvenc,        Yuv420, CPU_D3D11,     DC_RTP, Full,      NVENC,  L_HEVC),
        ff("hevc_nvenc:yuv444",        "hevc_nvenc",        Hevc, Nvenc,        Yuv444, CPU,           DC,     Full,      NVENC,  L_HEVC),
        ff("hevc_qsv:yuv420",          "hevc_qsv",          Hevc, Qsv,          Yuv420, CPU_D3D11,     DC_RTP, Unread,    QSV,    L_HEVC),
        ff("hevc_qsv:yuv444",          "hevc_qsv",          Hevc, Qsv,          Yuv444, CPU,           DC,     Unread,    QSV,    L_HEVC_P7),
        ff("hevc_amf:yuv420",          "hevc_amf",          Hevc, Amf,          Yuv420, CPU,           DC_RTP, Full,      AMF,    L_HEVC),
        ff("hevc_videotoolbox:yuv420", "hevc_videotoolbox", Hevc, VideoToolbox, Yuv420, CPU_IOSURFACE, DC_RTP, Full,      VT,     L_HEVC),
        ff("hevc_vaapi:yuv420",        "hevc_vaapi",        Hevc, Vaapi,        Yuv420, CPU,           DC_RTP, Unread,    VAAPI,  L_HEVC),
        ff("hevc_vaapi:yuv444",        "hevc_vaapi",        Hevc, Vaapi,        Yuv444, CPU,           DC,     Unread,    VAAPI,  L_HEVC_P7),
        ff("hevc_d3d12va:yuv420",      "hevc_d3d12va",      Hevc, D3d12,        Yuv420, CPU,           DC_RTP, Full,      D3D12,  L_HEVC),
        ff("hevc_vulkan:yuv420",       "hevc_vulkan",       Hevc, Vulkan,       Yuv420, CPU,           DC_RTP, Full,      VULKAN, L_HEVC),
        ff("hevc_vulkan:yuv444",       "hevc_vulkan",       Hevc, Vulkan,       Yuv444, CPU,           DC,     Unread,    VULKAN, L_HEVC_P7),
        ff("av1_nvenc:yuv420",         "av1_nvenc",         Av1,  Nvenc,        Yuv420, CPU_D3D11,     DC,     Full,      NVENC,  L_AV1),
        ff("av1_qsv:yuv420",           "av1_qsv",           Av1,  Qsv,          Yuv420, CPU_D3D11,     DC,     Full,      QSV,    L_AV1),
        ff("av1_amf:yuv420",           "av1_amf",           Av1,  Amf,          Yuv420, CPU,           DC,     Unread,    AMF,    L_AV1),
        ff("av1_videotoolbox:yuv420",  "av1_videotoolbox",  Av1,  VideoToolbox, Yuv420, CPU,           DC,     Unread,    VT,     L_AV1),
        ff("av1_vaapi:yuv420",         "av1_vaapi",         Av1,  Vaapi,        Yuv420, CPU,           DC,     Unread,    VAAPI,  L_AV1),
        ff("av1_d3d12va:yuv420",       "av1_d3d12va",       Av1,  D3d12,        Yuv420, CPU,           DC,     Full,      D3D12,  L_AV1),
        ff("av1_vulkan:yuv420",        "av1_vulkan",        Av1,  Vulkan,       Yuv420, CPU,           DC,     Full,      VULKAN, L_AV1),
        ff("h264_nvenc:yuv420",        "h264_nvenc",        H264, Nvenc,        Yuv420, CPU_D3D11,     DC_REC, Full,      NVENC,  L_H264),
        ff("h264_nvenc:yuv444",        "h264_nvenc",        H264, Nvenc,        Yuv444, CPU,           DC,     Full,      NVENC,  L_H264),
        ff("h264_qsv:yuv420",          "h264_qsv",          H264, Qsv,          Yuv420, CPU_D3D11,     DC_REC, Full,      QSV,    L_H264),
        // AMF's H.264 writes matrix 2, unspecified (FR-130 P1).
        ff("h264_amf:yuv420",          "h264_amf",          H264, Amf,          Yuv420, CPU,           DC_REC, Unwritten, AMF,    L_H264),
        ff("h264_videotoolbox:yuv420", "h264_videotoolbox", H264, VideoToolbox, Yuv420, CPU_IOSURFACE, DC_REC, Full,      VT,     L_H264),
        ff("h264_vaapi:yuv420",        "h264_vaapi",        H264, Vaapi,        Yuv420, CPU,           DC_REC, Unread,    VAAPI,  L_H264),
        ff("h264_d3d12va:yuv420",      "h264_d3d12va",      H264, D3d12,        Yuv420, CPU,           DC_REC, Full,      D3D12,  L_H264),
        ff("h264_vulkan:yuv420",       "h264_vulkan",       H264, Vulkan,       Yuv420, CPU,           DC_REC, Full,      VULKAN, L_H264),
        libvpx("vp9_libvpx:yuv420", Yuv420),
        libvpx("vp9_libvpx:yuv444", Yuv444),
    ];
}

// ── What a probe found ───────────────────────────────────────────────────

/// What the probe found, in the shape the hello's assembly reads it: the
/// result of every open `caps::compute_caps_with` makes, and the few knobs its
/// assembly reads. A fixture is one of these, so the agent's own code and the
/// table can be handed the same host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeFindings {
    /// `openh264-encoder` is compiled in. Its cell needs no open.
    #[serde(default)]
    pub openh264: bool,
    /// The Media Foundation block ran: Windows, `mf-encoder`, a hardware probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mf: Option<MfFindings>,
    /// The FFmpeg block ran: `ffmpeg-encoder`, a hardware probe, and
    /// `ROOMLERD_USE_FFMPEG` not `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ffmpeg: Option<FfmpegFindings>,
    /// libvpx's 4:4:4 open succeeded (`vp9-444`, and a phase that runs it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub libvpx: Option<LibvpxFindings>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MfFindings {
    /// `None`: the cell is denied, or no adapter enumerates its MFT. Else what
    /// the activation said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h264: Option<Activation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hevc: Option<Activation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub av1: Option<Activation>,
    /// `ROOMLERD_ALLOW_SW_HEAVY`: HEVC and AV1 advertised on a software MFT.
    #[serde(default)]
    pub allow_sw_heavy: bool,
}

/// What a Media Foundation activation landed on (`caps::ProbeResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    Hardware,
    SoftwareOnly,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfmpegFindings {
    /// `qsv_is_hardware_by_construction`: this build's QSV opens are hardware.
    pub qsv_hw: bool,
    /// The first VP9 encoder that opened in 4:2:0 (the VP9 probe stops there),
    /// with every form it opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vp9: Option<Opened>,
    /// HEVC, AV1 and H.264 in the order they were probed: every encoder that
    /// opened in 4:2:0, in cascade order, with every form it opened. H.264 is
    /// absent under `ROOMLERD_DC_H264=0`.
    #[serde(default)]
    pub matrix: Vec<CodecOpened>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecOpened {
    #[serde(with = "codec_wire")]
    pub codec: VideoCodec,
    #[serde(default)]
    pub opened: Vec<Opened>,
}

/// An FFmpeg encoder that opened, by its cascade name, and the forms that did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opened {
    pub name: String,
    #[serde(with = "chroma_wire")]
    pub chroma: Vec<ChromaFormat>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibvpxFindings {
    /// Profile 0 opened too.
    pub yuv420: bool,
    /// `ROOMLERD_VP9_CHROMA` as `vp9_chroma_from_env` read it.
    pub vp9_chroma: String,
}

impl ProbeFindings {
    /// What the `base` child alone finds: FFmpeg's 4:4:4 forms are the `444`
    /// child's question, and libvpx's are not (its 4:4:4 open is its first).
    pub fn base_phase(&self) -> Self {
        let mut base = self.clone();
        if let Some(ff) = base.ffmpeg.as_mut() {
            let strip = |o: &mut Opened| o.chroma.retain(|c| *c == ChromaFormat::Yuv420);
            if let Some(vp9) = ff.vp9.as_mut() {
                strip(vp9);
            }
            for codec in &mut ff.matrix {
                codec.opened.iter_mut().for_each(strip);
            }
        }
        base
    }

    /// The FFmpeg encoders whose 4:4:4 form opened.
    pub fn opened_444(&self) -> Vec<String> {
        let Some(ff) = &self.ffmpeg else {
            return Vec::new();
        };
        ff.vp9
            .iter()
            .chain(ff.matrix.iter().flat_map(|c| c.opened.iter()))
            .filter(|o| o.chroma.contains(&ChromaFormat::Yuv444))
            .map(|o| o.name.clone())
            .collect()
    }
}

/// The six fields a probe answers, as the hello carries them: the same names,
/// order and `skip_serializing_if` as `AgentCaps` (a test holds them equal),
/// so a fixture's JSON is the wire's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbedFields {
    pub hw_encoders: Vec<String>,
    pub codecs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transports: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vp9_chroma: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hevc_chroma: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video_cells: Vec<VideoCell>,
}

impl ProbedFields {
    pub fn of(caps: &roomler_ai_remote_control::models::AgentCaps) -> Self {
        Self {
            hw_encoders: caps.hw_encoders.clone(),
            codecs: caps.codecs.clone(),
            transports: caps.transports.clone(),
            vp9_chroma: caps.vp9_chroma.clone(),
            hevc_chroma: caps.hevc_chroma.clone(),
            video_cells: caps.video_cells.clone(),
        }
    }
}

// ── The overlay ──────────────────────────────────────────────────────────

/// What proved a row on this host (FR-135 §6.5's `CellProof`, the columns the
/// probe has a source for). Dv10: what a probe proves differs by row, and the
/// hello does not say which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CellProof {
    /// A [`CellId`]'s wire name.
    pub id: String,
    pub proof: Proof,
    /// What the probe said about hardware, for a row whose `hw` it decides.
    pub hw: Option<bool>,
    /// Which probe child proved it.
    pub phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Proof {
    /// Compiled in; nothing was opened (OpenH264).
    Built,
    /// Its MFT enumerates, and activating it failed.
    Enumerated,
    /// Activated and torn down (Media Foundation).
    Activated,
    /// An encoder opened at the probe's size (FFmpeg, libvpx).
    Opened,
}

impl Proof {
    /// The row is there: everything but an enumeration whose open failed.
    pub fn opened(self) -> bool {
        !matches!(self, Self::Enumerated)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The `base` child: every 4:2:0 open and the software cells.
    Base,
    /// The `444` child: FFmpeg's 4:4:4 forms.
    Yuv444,
}

/// The overlay a probe's findings prove.
pub fn proofs(found: &ProbeFindings) -> Vec<CellProof> {
    let mut out = Vec::new();
    let mut push = |id: String, proof: Proof, hw: Option<bool>, phase: Phase| {
        out.push(CellProof {
            id,
            proof,
            hw,
            phase,
        })
    };
    if found.openh264 {
        push(
            "h264_openh264:yuv420".into(),
            Proof::Built,
            None,
            Phase::Base,
        );
    }
    if let Some(mf) = &found.mf {
        for (name, activation) in [
            (super::cells::MF_H264, mf.h264),
            (super::cells::MF_HEVC, mf.hevc),
            (super::cells::MF_AV1, mf.av1),
        ] {
            let Some(activation) = activation else {
                continue;
            };
            let (proof, hw) = match activation {
                Activation::Hardware => (Proof::Activated, Some(true)),
                Activation::SoftwareOnly => (Proof::Activated, Some(false)),
                Activation::Failed => (Proof::Enumerated, None),
            };
            push(format!("{name}:yuv420"), proof, hw, Phase::Base);
        }
    }
    if let Some(ff) = &found.ffmpeg {
        for opened in ff
            .vp9
            .iter()
            .chain(ff.matrix.iter().flat_map(|c| c.opened.iter()))
        {
            for chroma in &opened.chroma {
                let phase = match chroma {
                    ChromaFormat::Yuv420 => Phase::Base,
                    ChromaFormat::Yuv444 => Phase::Yuv444,
                };
                push(
                    format!("{}:{}", opened.name, chroma.wire()),
                    Proof::Opened,
                    None,
                    phase,
                );
            }
        }
    }
    if let Some(vpx) = &found.libvpx {
        let name = super::cells::LIBVPX_VP9;
        push(format!("{name}:yuv444"), Proof::Opened, None, Phase::Base);
        if vpx.yuv420 {
            push(format!("{name}:yuv420"), Proof::Opened, None, Phase::Base);
        }
    }
    out
}

/// The host facts the derive reads besides the proofs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeKnobs {
    /// The build's QSV flag (`qsv_is_hardware_by_construction`).
    pub qsv_hw: bool,
    /// `ROOMLERD_ALLOW_SW_HEAVY`.
    pub allow_sw_heavy: bool,
    /// `ROOMLERD_VP9_CHROMA`, as libvpx's probe read it.
    pub vp9_chroma: String,
}

impl ProbeKnobs {
    pub fn of(found: &ProbeFindings) -> Self {
        Self {
            qsv_hw: found.ffmpeg.as_ref().is_some_and(|f| f.qsv_hw),
            allow_sw_heavy: found.mf.as_ref().is_some_and(|m| m.allow_sw_heavy),
            vp9_chroma: found
                .libvpx
                .as_ref()
                .map(|v| v.vp9_chroma.clone())
                .unwrap_or_default(),
        }
    }
}

// ── The derive ───────────────────────────────────────────────────────────

/// One encoder (codec × backend): its rows, in table order.
fn encoders(table: &[CellSpec]) -> Vec<&[CellSpec]> {
    table
        .chunk_by(|a, b| (a.family, a.codec, a.backend) == (b.family, b.codec, b.backend))
        .collect()
}

fn proof_of<'a>(proofs: &'a [CellProof], row: &CellSpec) -> Option<&'a CellProof> {
    proofs.iter().find(|p| p.id == row.id.wire())
}

fn proven(proofs: &[CellProof], row: &CellSpec) -> bool {
    proof_of(proofs, row).is_some_and(|p| p.proof.opened())
}

/// What the row's own truth says about hardware.
fn truth_hw(row: &CellSpec, proof: Option<&CellProof>, knobs: &ProbeKnobs) -> bool {
    match row.hw {
        HwTruth::Hardware => true,
        HwTruth::Software => false,
        HwTruth::ByProbe => proof.and_then(|p| p.hw).unwrap_or(false),
        HwTruth::ByBuild => knobs.qsv_hw,
    }
}

/// What the hello says about hardware: the truth, but for Dv6's borrowed flag.
fn wire_hw(row: &CellSpec, proof: Option<&CellProof>, knobs: &ProbeKnobs) -> bool {
    if row.legacy.hw_from_qsv_build {
        knobs.qsv_hw
    } else {
        truth_hw(row, proof, knobs)
    }
}

/// An encoder as the hello sees it.
struct Encoder<'a> {
    rows: &'a [CellSpec],
    /// The hello's cell, when the encoder opened and is advertised.
    cell: Option<VideoCell>,
    /// Its first form has a proof of any kind, an enumeration included.
    enumerated: bool,
}

fn encoders_on_the_wire<'a>(
    table: &'a [CellSpec],
    proofs: &[CellProof],
    knobs: &ProbeKnobs,
) -> Vec<Encoder<'a>> {
    encoders(table)
        .into_iter()
        .map(|rows| {
            let gate = rows.iter().find(|r| r.opens_first());
            let gate_proof = gate.and_then(|g| proof_of(proofs, g));
            let cell = match (gate, gate_proof) {
                (Some(g), Some(p)) if p.proof.opened() => {
                    let hw = wire_hw(g, Some(p), knobs);
                    let advertised = g.hw != HwTruth::ByProbe
                        || hw
                        || g.advertise_software
                        || knobs.allow_sw_heavy;
                    advertised.then(|| {
                        let chroma: Vec<ChromaFormat> = ChromaFormat::ALL
                            .into_iter()
                            .filter(|c| rows.iter().any(|r| r.chroma == *c && proven(proofs, r)))
                            .collect();
                        VideoCell::new(g.codec, g.backend, &chroma, hw)
                    })
                }
                _ => None,
            };
            Encoder {
                rows,
                cell,
                enumerated: gate_proof.is_some(),
            }
        })
        .collect()
}

/// The hello's six probed fields, from the table and a host's proofs.
pub fn derive(table: &[CellSpec], proofs: &[CellProof], knobs: &ProbeKnobs) -> ProbedFields {
    let encoders = encoders_on_the_wire(table, proofs, knobs);
    let mut out = ProbedFields {
        video_cells: encoders.iter().filter_map(|e| e.cell.clone()).collect(),
        ..ProbedFields::default()
    };
    let groups = encoders.chunk_by(|a, b| {
        a.rows.first().map(CellSpec::group) == b.rows.first().map(CellSpec::group)
    });
    for group in groups {
        let winner = group.iter().find(|e| e.cell.is_some());
        let Some(encoder) = winner.or_else(|| {
            // Dv4: a label that rides enumeration stands with no cell behind it.
            group
                .iter()
                .find(|e| e.enumerated && e.rows.iter().any(|r| r.legacy.label_on_enumeration))
        }) else {
            continue;
        };
        let Some(row) = encoder.rows.first() else {
            continue;
        };
        out.hw_encoders.push(row.legacy_label());
        if winner.is_none() {
            continue;
        }
        let codec = negotiation_name(row.codec);
        if row.legacy.in_codecs && !out.codecs.iter().any(|c| c == codec) {
            out.codecs.push(codec.to_string());
        }
        if let Some(transport) = row.legacy.transport {
            out.transports.push(transport.to_string());
        }
        for c in ChromaFormat::ALL {
            if encoder
                .rows
                .iter()
                .any(|r| r.chroma == c && r.legacy.hevc_chroma && proven(proofs, r))
            {
                out.hevc_chroma.push(c.wire().to_string());
            }
        }
    }
    if out.transports.iter().any(|t| t == DC_VP9_444) {
        out.vp9_chroma = knobs.vp9_chroma.clone();
    }
    out
}

// ── The divergences ──────────────────────────────────────────────────────

/// One place the hello says something other than the table's rows. `dv` is
/// FR-135 §5.7's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Divergence {
    pub dv: String,
    /// The cell, codec or label it is about.
    pub at: String,
    pub what: String,
}

fn divergence(dv: &str, at: impl Into<String>, what: &str) -> Divergence {
    Divergence {
        dv: dv.to_string(),
        at: at.into(),
        what: what.to_string(),
    }
}

/// A host the divergences are read on: its platform, and the denylist its
/// probe ran with.
#[derive(Debug, Clone)]
pub struct Host<'a> {
    pub platform: Platform,
    pub deny: &'a [String],
}

/// Where `wire` (the hello the agent's own code wrote) says something other
/// than the table's rows, for one host. Each one is one of FR-135 §5.7's rows
/// read from the table.
pub fn divergences(
    table: &[CellSpec],
    proofs: &[CellProof],
    knobs: &ProbeKnobs,
    host: &Host<'_>,
    wire: &ProbedFields,
) -> Vec<Divergence> {
    let encoders = encoders_on_the_wire(table, proofs, knobs);
    let on_wire: Vec<(&CellSpec, &VideoCell)> = encoders
        .iter()
        .filter_map(|e| e.cell.as_ref().map(|c| (e.rows, c)))
        .flat_map(|(rows, cell)| {
            rows.iter()
                .filter(move |r| cell.chroma.iter().any(|w| w == r.chroma.wire()))
                .map(move |r| (r, cell))
        })
        .collect();
    let mut out = Vec::new();

    // Dv2 — `codecs` is a union: what the RTP column says the track can open,
    // against what the field lists.
    let mut rtp: Vec<&str> = Vec::new();
    for (row, _) in &on_wire {
        let name = negotiation_name(row.codec);
        if row.has_path(MediaPath::Rtp) && !rtp.contains(&name) {
            rtp.push(name);
        }
    }
    for codec in &wire.codecs {
        if !rtp.contains(&codec.as_str()) {
            out.push(divergence(
                "Dv2",
                codec.as_str(),
                "`codecs` lists it from a DataChannel cell; no RTP row the hello carries opens it",
            ));
        }
    }
    for codec in &rtp {
        if !wire.codecs.iter().any(|c| c == codec) {
            out.push(divergence(
                "Dv2",
                *codec,
                "an RTP row the hello carries opens it; `codecs` leaves it out",
            ));
        }
    }

    for e in &encoders {
        let Some(row) = e.rows.first() else {
            continue;
        };
        let label = row.legacy_label();
        // Dv4 — a `-hw` label with no hardware cell behind it.
        if label.ends_with("-hw")
            && wire.hw_encoders.contains(&label)
            && !e.cell.as_ref().is_some_and(|c| c.hw)
        {
            let what = if e.cell.is_some() {
                "the label says hardware; its cell says software"
            } else {
                "the label says hardware; the activation failed and there is no cell"
            };
            out.push(divergence("Dv4", label.as_str(), what));
        }
        // Dv10 — a label or cell resting on less than an open.
        if let Some(gate) = e.rows.iter().find(|r| r.opens_first())
            && let Some(p) = proof_of(proofs, gate)
            && wire.hw_encoders.contains(&label)
        {
            match p.proof {
                Proof::Built => out.push(divergence(
                    "Dv10",
                    gate.id.wire(),
                    "advertised from the build alone: nothing opened it",
                )),
                Proof::Enumerated => out.push(divergence(
                    "Dv10",
                    gate.id.wire(),
                    "advertised from an enumeration alone: its activation failed",
                )),
                Proof::Activated | Proof::Opened => {}
            }
        }
    }

    // Dv4 — P7: the HEVC winner carries a 4:4:4 form `hevc_chroma` cannot say.
    if let Some(winner) = encoders.iter().find(|e| {
        e.cell.is_some()
            && e.rows
                .first()
                .is_some_and(|r| r.family == Family::Ffmpeg && r.codec == VideoCodec::Hevc)
    }) {
        for row in winner.rows {
            if row.chroma == ChromaFormat::Yuv444 && !row.legacy.hevc_chroma && proven(proofs, row)
            {
                out.push(divergence(
                    "Dv4",
                    row.id.wire(),
                    "the HEVC winner opened 4:4:4; `hevc_chroma` says 4:2:0 only (P7 names hevc_nvenc alone)",
                ));
            }
        }
    }

    // Dv6 — the VP9 cell borrows the QSV build flag, and its probe stops at the
    // first opener.
    let vp9: Vec<&Encoder> = encoders
        .iter()
        .filter(|e| {
            e.rows
                .first()
                .is_some_and(|r| r.family == Family::Ffmpeg && r.codec == VideoCodec::Vp9)
        })
        .collect();
    for e in &vp9 {
        if let (Some(cell), Some(gate)) = (&e.cell, e.rows.iter().find(|r| r.opens_first())) {
            let truth = truth_hw(gate, proof_of(proofs, gate), knobs);
            if cell.hw != truth {
                out.push(divergence(
                    "Dv6",
                    gate.id.wire(),
                    "`hw` is the QSV build flag; the row says otherwise",
                ));
            }
        }
    }
    if let Some(won) = vp9.iter().position(|e| e.cell.is_some()) {
        for e in vp9.iter().skip(won + 1) {
            if let Some(gate) = e.rows.iter().find(|r| r.opens_first())
                && gate.on_platform(host.platform)
            {
                out.push(divergence(
                    "Dv6",
                    gate.id.wire(),
                    "never asked: the VP9 probe stops at its first opener",
                ));
            }
        }
    }

    // Dv7 — a cell the picker can offer with no DataChannel transport behind it.
    for e in &encoders {
        let (Some(cell), Some(row)) = (&e.cell, e.rows.first()) else {
            continue;
        };
        if row.codec == VideoCodec::H264 {
            // H.264 keeps the RTP track, which every host opens.
            continue;
        }
        let carried = table
            .iter()
            .filter(|r| r.codec == row.codec && r.has_path(MediaPath::Dc))
            .filter_map(|r| r.legacy.transport)
            .any(|t| wire.transports.iter().any(|w| w == t));
        if !carried {
            out.push(divergence(
                "Dv7",
                format!("{}/{}", cell.codec, cell.backend),
                "a cell with no DataChannel transport for its codec on the wire",
            ));
        }
    }

    // Dv11 — one predicate would deny it; the wire still carries it.
    for (row, _) in &on_wire {
        if host.deny.iter().any(|d| d == row.id.wire()) {
            out.push(divergence(
                "Dv11",
                row.id.wire(),
                "on the denylist and still advertised: no gate reads it for this row",
            ));
        }
    }

    // Dv12 — a GPU form a row has, whose proof never reaches the wire.
    for (row, _) in &on_wire {
        if row
            .input_forms
            .iter()
            .any(|f| f.platform() == Some(host.platform))
        {
            out.push(divergence(
                "Dv12",
                row.id.wire(),
                "has a GPU input form; whether it opened is not on the wire",
            ));
        }
    }

    // Dv19 — colour is a per-cell fact the hello states once for the host.
    for (row, _) in &on_wire {
        let what = match row.describes {
            Describes::Full => continue,
            Describes::Partial => "describes its colour in part (matrix and range)",
            Describes::Unwritten => "writes no colour description",
            Describes::Unread => "nobody has read whether it describes its colour",
        };
        out.push(divergence("Dv19", row.id.wire(), what));
    }
    out
}

// ── The shadow ───────────────────────────────────────────────────────────

/// What the table derives from `found`, where it differs from `real`, as one
/// line for the log. `None` when they agree.
pub fn disagreement(
    table: &[CellSpec],
    found: &ProbeFindings,
    real: &ProbedFields,
) -> Option<String> {
    let derived = derive(table, &proofs(found), &ProbeKnobs::of(found));
    if derived == *real {
        return None;
    }
    let json = |f: &ProbedFields| serde_json::to_string(f).unwrap_or_default();
    Some(format!("derived {} · hello {}", json(&derived), json(real)))
}

static SHADOW_WARNED: AtomicBool = AtomicBool::new(false);

/// FR-135 R9 — at the real hello's build: says once per process if the table
/// derives other fields than the hello carries. The hello is never touched, and
/// nothing here can fail: the derive is pure, and a disagreement is a line.
pub fn shadow(found: &ProbeFindings, real: &ProbedFields) {
    shadow_into(&SHADOW_WARNED, CELLS, found, real);
}

/// [`shadow`] with its once-flag and table passed in. True when it logged.
fn shadow_into(
    warned: &AtomicBool,
    table: &[CellSpec],
    found: &ProbeFindings,
    real: &ProbedFields,
) -> bool {
    if warned.load(Ordering::Relaxed) {
        return false;
    }
    let Some(diff) = disagreement(table, found, real) else {
        return false;
    };
    if warned.swap(true, Ordering::Relaxed) {
        return false;
    }
    tracing::warn!(
        %diff,
        "cell table: the derived encoder fields differ from the hello (FR-135 R9 shadow; the hello is unchanged)"
    );
    true
}

// ── Wire spellings for the fixtures' JSON ────────────────────────────────

mod codec_wire {
    use roomler_ai_remote_control::models::VideoCodec;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(codec: &VideoCodec, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(codec.wire())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<VideoCodec, D::Error> {
        let raw = String::deserialize(d)?;
        VideoCodec::from_wire(&raw).ok_or_else(|| D::Error::custom(format!("unknown codec {raw}")))
    }
}

mod chroma_wire {
    use roomler_ai_remote_control::models::ChromaFormat;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(list: &[ChromaFormat], s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(list.iter().map(|c| c.wire()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<ChromaFormat>, D::Error> {
        Vec::<String>::deserialize(d)?
            .iter()
            .map(|w| {
                ChromaFormat::from_wire(w)
                    .ok_or_else(|| D::Error::custom(format!("unknown chroma {w}")))
            })
            .collect()
    }
}
