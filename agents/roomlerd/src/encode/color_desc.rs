// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-130 P1 — the colour description every encoder writes, defined ONCE.
//!
//! Every converter in the agent turns the captured BGRA into YUV with the
//! BT.601 matrix at limited (studio) range: the CPU converters
//! ([`super::color`], the FFmpeg CPU form's, openh264's and libvpx's), the D3D11
//! VideoProcessor (FR-118) and VideoToolbox's attached matrix
//! (`ffmpeg::vt_input`). Until FR-130 no encoder said so. The bitstream read
//! "unspecified", the browser decoded an HD picture as BT.709, and every
//! saturated colour shifted while the greys stayed exact (#3049; the spec's
//! 2.1 has the arithmetic, value for value).
//!
//! [`DESCRIPTION`] is the one place the four values live. An encoder family
//! reads it through [`active`] at every open (`None` while the
//! `color_description` switch is off, which writes exactly the undescribed
//! stream every earlier agent sent) and hands it to its API through the
//! mapping below:
//!
//! | Family | Mapping | What the API can say |
//! |---|---|---|
//! | FFmpeg, every backend | [`ffmpeg`]: the codec context's four fields, before the open | every value, as H.273 code points. `vp9_qsv` and `vp9_vaapi` carry none in FFmpeg 9.0.1 (`qsvenc.c:1233` skips VP9; `vaapi_encode_vp9.c` writes no colour) |
//! | Media Foundation | [`mf`]: `MF_MT_YUV_MATRIX`, `MF_MT_VIDEO_NOMINAL_RANGE`, `MF_MT_VIDEO_PRIMARIES`, `MF_MT_TRANSFER_FUNCTION` | no SMPTE 170M transfer: MF's `MFVideoTransFunc_709` is the same curve and is the closest exact value. One BT.601 matrix for both H.273 codes 5 and 6 |
//! | openh264 | [`openh264`]: the SPS VUI | every value |
//! | libvpx VP9 | [`vp9`]: `VP9E_SET_COLOR_SPACE`, `VP9E_SET_COLOR_RANGE` | the matrix and the range only: VP9's header has no primaries or transfer field |
//!
//! The tests that parse each family's output (`encode::bitstream_color`) read
//! their expectations from this same constant, so moving to another set (the
//! spec's open decision 1) is one edit here and nothing else.

use std::sync::Once;

/// The YCbCr matrix, as an H.273 `MatrixCoefficients` code point.
///
/// One variant on purpose: every converter the agent has is BT.601, and
/// signalling anything else would describe a conversion that never ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    /// SMPTE 170M (6), BT.601's coefficients. Not BT.470 BG (5), the same
    /// matrix under another code: AMF maps only SMPTE 170M, BT.709 and
    /// BT.2020 to an encoder colour profile and reads anything else as "auto
    /// by size" (FFmpeg 9.0.1 `libavutil/hwcontext_amf.c:155-184`).
    Smpte170m,
}

/// The source's colour primaries, as an H.273 `ColourPrimaries` code point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primaries {
    /// BT.709 (1): sRGB's primaries, which a desktop's pixels are in.
    Bt709,
    /// SMPTE 170M (6), the conventional BT.601 ("SMPTE C") primaries.
    Smpte170m,
}

/// The transfer characteristic, as an H.273 `TransferCharacteristics` code
/// point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    /// BT.709 (1).
    Bt709,
    /// SMPTE 170M (6): the BT.709 curve under the BT.601 name.
    Smpte170m,
    /// IEC 61966-2-1 (13): sRGB, which a desktop's pixels are encoded with.
    Srgb,
}

/// What a bitstream says about the YUV it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorDescription {
    pub matrix: Matrix,
    pub primaries: Primaries,
    pub transfer: Transfer,
    /// `false`: limited (studio, MPEG) range, Y 16-235 and C 16-240.
    pub full_range: bool,
}

/// **The** description: BT.601 matrix at limited range, which is what every
/// converter produces, with the primaries and transfer of the sRGB desktop the
/// pixels came from. It is the spec's set S, which P0 chose (open decision 1):
/// Chrome decodes it exactly as set A (SMPTE 170M ×3) and set B (BT.709
/// primaries and transfer), and of the three only S says what the desktop is.
pub const DESCRIPTION: ColorDescription = ColorDescription {
    matrix: Matrix::Smpte170m,
    primaries: Primaries::Bt709,
    transfer: Transfer::Srgb,
    full_range: false,
};

// The matrix and the range are what every converter produces, whichever set
// carries the primaries and the transfer: a description claiming another
// matrix would mis-decode every saturated colour again. Checked at compile
// time, so no build can ship one.
const _: () = assert!(
    DESCRIPTION.matrix.code() == 6 && !DESCRIPTION.full_range,
    "the converters are BT.601 (SMPTE 170M) at limited range"
);

impl Matrix {
    /// The H.273 code point.
    pub const fn code(self) -> u8 {
        match self {
            Self::Smpte170m => 6,
        }
    }
}

impl Primaries {
    /// The H.273 code point.
    pub const fn code(self) -> u8 {
        match self {
            Self::Bt709 => 1,
            Self::Smpte170m => 6,
        }
    }
}

impl Transfer {
    /// The H.273 code point.
    pub const fn code(self) -> u8 {
        match self {
            Self::Bt709 => 1,
            Self::Smpte170m => 6,
            Self::Srgb => 13,
        }
    }
}

static ANNOUNCED: Once = Once::new();

/// The description an encoder opening now writes: [`DESCRIPTION`], or `None`
/// while `color_description` is off (`ROOMLERD_COLOR_DESCRIPTION=0`), which
/// keeps today's undescribed stream byte for byte. Every family calls this at
/// every open, so the probe children, a recorder and a rebuild all follow the
/// switch the process was started with. The first call in a process logs the
/// answer once.
pub fn active() -> Option<ColorDescription> {
    let on = super::color_description_enabled();
    ANNOUNCED.call_once(|| {
        if on {
            tracing::info!(
                matrix = DESCRIPTION.matrix.code(),
                primaries = DESCRIPTION.primaries.code(),
                transfer = DESCRIPTION.transfer.code(),
                full_range = DESCRIPTION.full_range,
                "FR-130: encoders describe their colour (BT.601 matrix, limited range)"
            );
        } else {
            tracing::info!(
                "FR-130: color_description is off: encoders write no colour description"
            );
        }
    });
    on.then_some(DESCRIPTION)
}

/// FFmpeg's four codec-context fields for `d`.
#[cfg(feature = "ffmpeg-encoder")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FfmpegFields {
    pub space: ffmpeg_next::color::Space,
    pub primaries: ffmpeg_next::color::Primaries,
    pub transfer: ffmpeg_next::color::TransferCharacteristic,
    pub range: ffmpeg_next::color::Range,
}

/// `d` in FFmpeg's vocabulary. FFmpeg's enums are the H.273 code points, and
/// every backend passes them on as they are: as the H.264/HEVC SPS VUI and
/// the AV1 sequence header FFmpeg writes itself (VAAPI, D3D12, Vulkan), or as
/// the vendor runtime's own fields (NVENC, QSV, AMF, VideoToolbox), which then
/// write the header.
#[cfg(feature = "ffmpeg-encoder")]
pub fn ffmpeg(d: ColorDescription) -> FfmpegFields {
    use ffmpeg_next::color::{Primaries as P, Range, Space, TransferCharacteristic as T};
    FfmpegFields {
        space: match d.matrix {
            Matrix::Smpte170m => Space::SMPTE170M,
        },
        primaries: match d.primaries {
            Primaries::Bt709 => P::BT709,
            Primaries::Smpte170m => P::SMPTE170M,
        },
        transfer: match d.transfer {
            Transfer::Bt709 => T::BT709,
            Transfer::Smpte170m => T::SMPTE170M,
            Transfer::Srgb => T::IEC61966_2_1,
        },
        range: if d.full_range {
            Range::JPEG
        } else {
            Range::MPEG
        },
    }
}

/// Media Foundation's four media-type attributes for `d`.
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MfAttributes {
    pub matrix: windows::Win32::Media::MediaFoundation::MFVideoTransferMatrix,
    pub range: windows::Win32::Media::MediaFoundation::MFNominalRange,
    pub primaries: windows::Win32::Media::MediaFoundation::MFVideoPrimaries,
    pub transfer: windows::Win32::Media::MediaFoundation::MFVideoTransferFunction,
}

/// `d` in Media Foundation's vocabulary. MF names one BT.601 matrix for both
/// H.273 codes, and has no SMPTE 170M transfer: `MFVideoTransFunc_709` is the
/// same curve, so it is the closest exact value.
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
pub fn mf(d: ColorDescription) -> MfAttributes {
    use windows::Win32::Media::MediaFoundation as m;
    MfAttributes {
        matrix: match d.matrix {
            Matrix::Smpte170m => m::MFVideoTransferMatrix_BT601,
        },
        range: if d.full_range {
            m::MFNominalRange_0_255
        } else {
            m::MFNominalRange_16_235
        },
        primaries: match d.primaries {
            Primaries::Bt709 => m::MFVideoPrimaries_BT709,
            Primaries::Smpte170m => m::MFVideoPrimaries_SMPTE170M,
        },
        transfer: match d.transfer {
            Transfer::Bt709 | Transfer::Smpte170m => m::MFVideoTransFunc_709,
            Transfer::Srgb => m::MFVideoTransFunc_sRGB,
        },
    }
}

/// `d` as openh264's SPS VUI. The crate writes `video_format` 5
/// (unspecified) and the four fields as given.
#[cfg(feature = "openh264-encoder")]
pub fn openh264(d: ColorDescription) -> openh264::encoder::VuiConfig {
    use openh264::encoder::{
        ColorPrimaries as P, MatrixCoefficients as M, TransferCharacteristics as T, VuiConfig,
    };
    VuiConfig::new()
        .matrix_coefficients(match d.matrix {
            Matrix::Smpte170m => M::Smpte170M,
        })
        .color_primaries(match d.primaries {
            Primaries::Bt709 => P::Bt709,
            Primaries::Smpte170m => P::Smpte170M,
        })
        .transfer_characteristics(match d.transfer {
            Transfer::Bt709 => T::Bt709,
            Transfer::Smpte170m => T::Smpte170M,
            Transfer::Srgb => T::Srgb,
        })
        .full_range(d.full_range)
}

/// libvpx's two VP9 controls for `d`.
#[cfg(feature = "vp9-444")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9Controls {
    /// `VP9E_SET_COLOR_SPACE`: a `vpx_color_space`.
    pub color_space: std::os::raw::c_int,
    /// `VP9E_SET_COLOR_RANGE`: a `vpx_color_range`.
    pub color_range: std::os::raw::c_int,
}

/// `d` as libvpx's VP9 header controls. VP9's header carries the matrix and
/// the range only: it has no primaries or transfer field. SMPTE 170M is
/// `VPX_CS_SMPTE_170` (3), the matrix's own name, as FFmpeg's libvpx wrapper
/// maps it. `VPX_CS_BT_601` (1) would carry the same coefficients.
#[cfg(feature = "vp9-444")]
pub fn vp9(d: ColorDescription) -> Vp9Controls {
    use std::os::raw::c_int;
    use vpx_sys as vpx;
    Vp9Controls {
        color_space: match d.matrix {
            Matrix::Smpte170m => vpx::vpx_color_space::VPX_CS_SMPTE_170 as c_int,
        },
        color_range: if d.full_range {
            vpx::vpx_color_range::VPX_CR_FULL_RANGE as c_int
        } else {
            vpx::vpx_color_range::VPX_CR_STUDIO_RANGE as c_int
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tunnel_core::env::test_env::{Saved, set};

    /// The switch is default ON, `0` turns it off, and `active` follows it.
    #[test]
    fn the_switch_is_on_by_default_and_off_brings_back_no_description() {
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _switch = Saved::cleared("COLOR_DESCRIPTION");
        assert!(crate::encode::color_description_enabled());
        assert_eq!(active(), Some(DESCRIPTION));
        for off in ["0", "off", "false", "no"] {
            // SAFETY: serialised by DENY_ENV_LOCK; the guard restores it.
            unsafe { set("COLOR_DESCRIPTION", off) };
            assert!(!crate::encode::color_description_enabled(), "{off}");
            assert_eq!(active(), None, "{off}");
        }
        // SAFETY: as above.
        unsafe { set("COLOR_DESCRIPTION", "1") };
        assert_eq!(active(), Some(DESCRIPTION));
    }

    #[cfg(feature = "ffmpeg-encoder")]
    #[test]
    fn ffmpeg_fields_are_the_code_points() {
        let f = ffmpeg(DESCRIPTION);
        let space: ffmpeg_next::ffi::AVColorSpace = f.space.into();
        let primaries: ffmpeg_next::ffi::AVColorPrimaries = f.primaries.into();
        let transfer: ffmpeg_next::ffi::AVColorTransferCharacteristic = f.transfer.into();
        let range: ffmpeg_next::ffi::AVColorRange = f.range.into();
        assert_eq!(space as u32, u32::from(DESCRIPTION.matrix.code()));
        assert_eq!(primaries as u32, u32::from(DESCRIPTION.primaries.code()));
        assert_eq!(transfer as u32, u32::from(DESCRIPTION.transfer.code()));
        assert_eq!(
            range,
            if DESCRIPTION.full_range {
                ffmpeg_next::ffi::AVColorRange::AVCOL_RANGE_JPEG
            } else {
                ffmpeg_next::ffi::AVColorRange::AVCOL_RANGE_MPEG
            }
        );
    }
}
