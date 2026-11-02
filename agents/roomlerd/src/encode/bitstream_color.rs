// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-130 P1 — TEST-ONLY: read the colour description a bitstream carries.
//!
//! The vendored FFmpeg decodes H.264 only, and the repo had no SPS, AV1
//! sequence-header or VP9 header reader, so each encoder family's colour test
//! reads its own output through this module and compares it with
//! [`super::color_desc::DESCRIPTION`]:
//!
//! - H.264 and HEVC: the first SPS of an Annex-B stream, as far as its VUI's
//!   `video_signal_type` (ITU-T H.264 7.3.2.1.1 and E.1.1; H.265 7.3.2.2 and
//!   E.2.1), through scaling lists, frame cropping, sub-layers and the
//!   short-term reference picture sets on the way;
//! - AV1: the first sequence header OBU of a low-overhead OBU stream, through
//!   `color_config` (AV1 5.5.1-5.5.2);
//! - VP9: a keyframe's uncompressed header (VP9 6.2, `color_config` 6.2.2).
//!
//! The Annex-B and OBU walks are small and self-contained on purpose: the
//! repo's own (`recording::annexb::split_nals`, `mf::sync_pipeline`'s) sit
//! behind features (`recording`, `mf-encoder`) that the lanes running these
//! tests do not all build.

use super::color_desc::ColorDescription;

/// What a header says about colour. `None` = not signalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Signalled {
    /// H.264/HEVC `video_full_range_flag` (`None` without a
    /// `video_signal_type`); AV1 `color_range`, always present.
    pub full_range: Option<bool>,
    /// `colour_primaries`; `None` without a colour description.
    pub primaries: Option<u8>,
    /// `transfer_characteristics`; `None` without a colour description.
    pub transfer: Option<u8>,
    /// `matrix_coefficients`; `None` without a colour description.
    pub matrix: Option<u8>,
}

/// One of the four things a colour description says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Matrix,
    Primaries,
    Transfer,
    Range,
}

/// How a family's API names the description, which decides how exactly its
/// header can repeat [`super::color_desc::DESCRIPTION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vocabulary {
    /// H.273 code points, passed through as they are: FFmpeg's own header
    /// writers (VAAPI, D3D12, Vulkan), NVENC, QSV, openh264. Held exactly.
    CodePoints,
    /// AMF: primaries and transfer as code points, the matrix as a colour
    /// PROFILE (FFmpeg's `av_amf_get_color_profile`: 601), which the runtime
    /// turns into a matrix code of its own. Held to BT.601's meaning there.
    AmfProfile,
    /// Media Foundation's enums and CoreVideo's names (VideoToolbox): one
    /// BT.601 matrix for codes 5 and 6, one BT.709 curve for transfer codes 1
    /// and 6. Held to that meaning for both.
    Merged,
}

impl Vocabulary {
    fn merges_the_matrix(self) -> bool {
        self != Self::CodePoints
    }

    fn merges_the_transfer(self) -> bool {
        self == Self::Merged
    }
}

impl Signalled {
    /// No colour description: none written, or all three "unspecified" (2).
    pub fn unspecified(&self) -> bool {
        matches!(
            (self.primaries, self.transfer, self.matrix),
            (None, None, None) | (Some(2), Some(2), Some(2))
        )
    }

    /// The fields that do not say `d`, read in `v`'s terms: BT.470 BG (5) and
    /// SMPTE 170M (6) are one BT.601 matrix where `v` merges the matrix, and
    /// BT.709 (1) and SMPTE 170M (6) one curve where it merges the transfer.
    /// Primaries and range are always exact.
    pub fn mismatches(&self, d: ColorDescription, v: Vocabulary) -> Vec<Field> {
        let same = |got: Option<u8>, want: u8, merged: bool, class: fn(u8) -> bool| {
            got.is_some_and(|g| g == want || (merged && class(g) && class(want)))
        };
        let mut out = Vec::new();
        if !same(self.matrix, d.matrix.code(), v.merges_the_matrix(), bt601) {
            out.push(Field::Matrix);
        }
        if self.primaries != Some(d.primaries.code()) {
            out.push(Field::Primaries);
        }
        if !same(
            self.transfer,
            d.transfer.code(),
            v.merges_the_transfer(),
            bt709_curve,
        ) {
            out.push(Field::Transfer);
        }
        if self.full_range != Some(d.full_range) {
            out.push(Field::Range);
        }
        out
    }

    /// `Ok` when the header says `d` in `v`'s terms ([`Self::mismatches`]).
    pub fn check(&self, d: ColorDescription, v: Vocabulary) -> Result<(), String> {
        let wrong = self.mismatches(d, v);
        if wrong.is_empty() {
            Ok(())
        } else {
            Err(format!("{self:?}: {wrong:?} do not say {d:?}"))
        }
    }
}

fn bt601(code: u8) -> bool {
    matches!(code, 5 | 6)
}

fn bt709_curve(code: u8) -> bool {
    matches!(code, 1 | 6)
}

/// What a VP9 keyframe header says: its profile, `color_space` (a
/// `vpx_color_space`: 0 unknown, 1 BT.601, 2 BT.709, 3 SMPTE 170M, …, 7 sRGB)
/// and `color_range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Vp9Color {
    pub profile: u8,
    pub color_space: u8,
    pub full_range: bool,
}

/// The first H.264 SPS of an Annex-B stream.
pub(crate) fn h264(stream: &[u8]) -> Result<Signalled, String> {
    let nal = annexb_nals(stream)
        .into_iter()
        .find(|n| n.first().is_some_and(|h| h & 0x1f == 7))
        .ok_or("no H.264 SPS (NAL type 7) in the stream")?;
    h264_sps(nal)
}

/// The first HEVC SPS of an Annex-B stream.
pub(crate) fn hevc(stream: &[u8]) -> Result<Signalled, String> {
    let nal = annexb_nals(stream)
        .into_iter()
        .find(|n| n.first().is_some_and(|h| (h >> 1) & 0x3f == 33))
        .ok_or("no HEVC SPS (NAL type 33) in the stream")?;
    hevc_sps(nal)
}

/// The first sequence header of a low-overhead AV1 OBU stream (temporal
/// units back to back).
pub(crate) fn av1(stream: &[u8]) -> Result<Signalled, String> {
    let payload = av1_obus(stream)?
        .into_iter()
        .find(|(obu_type, _)| *obu_type == 1)
        .map(|(_, p)| p)
        .ok_or("no AV1 sequence header OBU (type 1) in the stream")?;
    av1_sequence_header(payload)
}

/// One H.264 SPS NAL unit, header byte included, emulation prevention as sent.
pub(crate) fn h264_sps(nal: &[u8]) -> Result<Signalled, String> {
    let rbsp = rbsp(nal.get(1..).ok_or("empty NAL")?);
    let mut r = Bits::new(&rbsp);
    let profile_idc = r.bits(8)?;
    r.skip(16)?; // constraint_set0..5 + reserved_zero_2bits, level_idc
    r.ue()?; // seq_parameter_set_id
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.skip(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.flag()? {
            // seq_scaling_matrix_present_flag
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.flag()? {
                    skip_h264_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.skip(1)?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            for _ in 0..r.ue()? {
                r.se()?; // offset_for_ref_frame[i]
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if !r.flag()? {
        // frame_mbs_only_flag = 0
        r.skip(1)?; // mb_adaptive_frame_field_flag
    }
    r.skip(1)?; // direct_8x8_inference_flag
    if r.flag()? {
        // frame_cropping_flag
        for _ in 0..4 {
            r.ue()?;
        }
    }
    if !r.flag()? {
        // vui_parameters_present_flag
        return Ok(Signalled::default());
    }
    vui_video_signal(&mut r)
}

/// H.264 7.3.2.1.1.1, skipping the values.
fn skip_h264_scaling_list(r: &mut Bits, size: usize) -> Result<(), String> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

/// One HEVC SPS NAL unit, its two header bytes included.
pub(crate) fn hevc_sps(nal: &[u8]) -> Result<Signalled, String> {
    let rbsp = rbsp(nal.get(2..).ok_or("short NAL")?);
    let mut r = Bits::new(&rbsp);
    r.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)?;
    r.skip(1)?; // sps_temporal_id_nesting_flag
    skip_profile_tier_level(&mut r, max_sub_layers_minus1)?;
    r.ue()?; // sps_seq_parameter_set_id
    if r.ue()? == 3 {
        // chroma_format_idc
        r.skip(1)?; // separate_colour_plane_flag
    }
    r.ue()?; // pic_width_in_luma_samples
    r.ue()?; // pic_height_in_luma_samples
    if r.flag()? {
        // conformance_window_flag
        for _ in 0..4 {
            r.ue()?;
        }
    }
    r.ue()?; // bit_depth_luma_minus8
    r.ue()?; // bit_depth_chroma_minus8
    let poc_lsb_bits = r.ue()? + 4;
    let first = if r.flag()? {
        // sps_sub_layer_ordering_info_present_flag
        0
    } else {
        max_sub_layers_minus1
    };
    for _ in first..=max_sub_layers_minus1 {
        r.ue()?; // sps_max_dec_pic_buffering_minus1
        r.ue()?; // sps_max_num_reorder_pics
        r.ue()?; // sps_max_latency_increase_plus1
    }
    // log2_min_luma_coding_block_size_minus3 … max_transform_hierarchy_depth_intra
    for _ in 0..6 {
        r.ue()?;
    }
    if r.flag()? && r.flag()? {
        // scaling_list_enabled_flag && sps_scaling_list_data_present_flag
        skip_hevc_scaling_list_data(&mut r)?;
    }
    r.skip(2)?; // amp_enabled_flag, sample_adaptive_offset_enabled_flag
    if r.flag()? {
        // pcm_enabled_flag
        r.skip(8)?; // pcm_sample_bit_depth_{luma,chroma}_minus1
        r.ue()?; // log2_min_pcm_luma_coding_block_size_minus3
        r.ue()?; // log2_diff_max_min_pcm_luma_coding_block_size
        r.skip(1)?; // pcm_loop_filter_disabled_flag
    }
    let mut sets: Vec<Rps> = Vec::new();
    for idx in 0..r.ue()? {
        // num_short_term_ref_pic_sets
        let rps = st_ref_pic_set(&mut r, idx as usize, &sets)?;
        sets.push(rps);
    }
    if r.flag()? {
        // long_term_ref_pics_present_flag
        for _ in 0..r.ue()? {
            r.skip(poc_lsb_bits)?; // lt_ref_pic_poc_lsb_sps
            r.skip(1)?; // used_by_curr_pic_lt_sps_flag
        }
    }
    r.skip(2)?; // sps_temporal_mvp_enabled_flag, strong_intra_smoothing_enabled_flag
    if !r.flag()? {
        // vui_parameters_present_flag
        return Ok(Signalled::default());
    }
    vui_video_signal(&mut r)
}

/// H.265 7.3.3 with `profilePresentFlag` = 1, skipping the values.
fn skip_profile_tier_level(r: &mut Bits, max_sub_layers_minus1: u32) -> Result<(), String> {
    // general_profile_space … general_inbld_flag: 2+1+5+32+4+43+1 bits, then
    // general_level_idc.
    r.skip(88 + 8)?;
    let n = max_sub_layers_minus1 as usize;
    let mut present = [(false, false); 7];
    for p in present.iter_mut().take(n) {
        *p = (r.flag()?, r.flag()?);
    }
    if n > 0 {
        r.skip(2 * (8 - n as u32))?; // reserved_zero_2bits
    }
    for &(profile, level) in present.iter().take(n) {
        if profile {
            r.skip(88)?;
        }
        if level {
            r.skip(8)?;
        }
    }
    Ok(())
}

/// H.265 7.3.4, skipping the values.
fn skip_hevc_scaling_list_data(r: &mut Bits) -> Result<(), String> {
    for size_id in 0..4u32 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0;
        while matrix_id < 6 {
            if r.flag()? {
                // scaling_list_pred_mode_flag
                let coefs = 64.min(1u32 << (4 + (size_id << 1)));
                if size_id > 1 {
                    r.se()?; // scaling_list_dc_coef_minus8
                }
                for _ in 0..coefs {
                    r.se()?; // scaling_list_delta_coef
                }
            } else {
                r.ue()?; // scaling_list_pred_matrix_id_delta
            }
            matrix_id += step;
        }
    }
    Ok(())
}

/// A short-term reference picture set's deltas, as H.265 (7-61)/(7-62)
/// derive them: an inter-predicted set's size depends on its reference's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Rps {
    s0: Vec<i32>,
    s1: Vec<i32>,
}

/// H.265 7.3.7 for a set inside the SPS (`stRpsIdx` < the count, so
/// `delta_idx_minus1` is absent and the reference is the previous set).
fn st_ref_pic_set(r: &mut Bits, idx: usize, sets: &[Rps]) -> Result<Rps, String> {
    let inter = if idx == 0 { false } else { r.flag()? };
    if !inter {
        let negative = r.ue()?;
        let positive = r.ue()?;
        if negative + positive > 32 {
            return Err(format!("{negative}+{positive} pictures in one RPS"));
        }
        let mut rps = Rps::default();
        let mut poc = 0i32;
        for _ in 0..negative {
            poc -= r.ue()? as i32 + 1;
            r.skip(1)?; // used_by_curr_pic_s0_flag
            rps.s0.push(poc);
        }
        poc = 0;
        for _ in 0..positive {
            poc += r.ue()? as i32 + 1;
            r.skip(1)?; // used_by_curr_pic_s1_flag
            rps.s1.push(poc);
        }
        return Ok(rps);
    }
    let reference = &sets[idx - 1];
    let negative_sign = r.flag()?; // delta_rps_sign
    let magnitude = r.ue()? as i32 + 1; // abs_delta_rps_minus1 + 1
    let delta_rps = if negative_sign { -magnitude } else { magnitude };
    let n_ref = reference.s0.len() + reference.s1.len();
    let mut use_delta = Vec::with_capacity(n_ref + 1);
    for _ in 0..=n_ref {
        let used_by_curr_pic = r.flag()?;
        // use_delta_flag is present only when used_by_curr_pic_flag is 0,
        // and inferred 1 otherwise.
        let use_delta_flag = if used_by_curr_pic { true } else { r.flag()? };
        use_delta.push(use_delta_flag);
    }
    // use_delta's entries follow the reference: its s0, its s1, then the one
    // for deltaRps itself, which (7-61)/(7-62) treat as a picture at 0 moved
    // by deltaRps. Each list takes its sign's pictures in the spec's order;
    // one that lands on 0 is the current picture, never a reference.
    let (for_s0, rest) = use_delta.split_at(reference.s0.len());
    let (for_s1, for_delta) = rest.split_at(reference.s1.len());
    let own = [0i32];
    let s0 = || reference.s0.iter().zip(for_s0);
    let s1 = || reference.s1.iter().zip(for_s1);
    let itself = || own.iter().zip(for_delta);
    let mut rps = Rps::default();
    for (&poc, &used) in s1().rev().chain(itself()).chain(s0()) {
        let d = poc + delta_rps;
        if d < 0 && used {
            rps.s0.push(d);
        }
    }
    for (&poc, &used) in s0().rev().chain(itself()).chain(s1()) {
        let d = poc + delta_rps;
        if d > 0 && used {
            rps.s1.push(d);
        }
    }
    Ok(rps)
}

/// The VUI's head, common to H.264 (E.1.1) and HEVC (E.2.1), up to and
/// including the colour description.
fn vui_video_signal(r: &mut Bits) -> Result<Signalled, String> {
    if r.flag()? {
        // aspect_ratio_info_present_flag
        if r.bits(8)? == 255 {
            // Extended_SAR: sar_width, sar_height
            r.skip(32)?;
        }
    }
    if r.flag()? {
        // overscan_info_present_flag
        r.skip(1)?; // overscan_appropriate_flag
    }
    let mut s = Signalled::default();
    if r.flag()? {
        // video_signal_type_present_flag
        r.skip(3)?; // video_format
        s.full_range = Some(r.flag()?);
        if r.flag()? {
            // colour_description_present_flag
            s.primaries = Some(r.bits(8)? as u8);
            s.transfer = Some(r.bits(8)? as u8);
            s.matrix = Some(r.bits(8)? as u8);
        }
    }
    Ok(s)
}

/// One AV1 sequence header OBU's payload (AV1 5.5.1), as far as
/// `color_config` (5.5.2).
pub(crate) fn av1_sequence_header(payload: &[u8]) -> Result<Signalled, String> {
    let mut r = Bits::new(payload);
    let seq_profile = r.bits(3)?;
    r.skip(1)?; // still_picture
    let reduced = r.flag()?; // reduced_still_picture_header
    if reduced {
        r.skip(5)?; // seq_level_idx[0]
    } else {
        let mut decoder_model_info = false;
        let mut buffer_delay_bits = 0;
        if r.flag()? {
            // timing_info_present_flag: timing_info()
            r.skip(64)?; // num_units_in_display_tick, time_scale
            if r.flag()? {
                // equal_picture_interval
                r.uvlc()?; // num_ticks_per_picture_minus_1
            }
            decoder_model_info = r.flag()?;
            if decoder_model_info {
                buffer_delay_bits = r.bits(5)? + 1; // buffer_delay_length_minus_1 + 1
                r.skip(32)?; // num_units_in_decoding_tick
                r.skip(10)?; // buffer_removal_time_length_minus_1, frame_presentation_time_length_minus_1
            }
        }
        let initial_display_delay = r.flag()?;
        for _ in 0..=r.bits(5)? {
            // operating_points_cnt_minus_1
            r.skip(12)?; // operating_point_idc
            if r.bits(5)? > 7 {
                // seq_level_idx
                r.skip(1)?; // seq_tier
            }
            if decoder_model_info && r.flag()? {
                // decoder_model_present_for_this_op: operating_parameters_info()
                r.skip(2 * buffer_delay_bits + 1)?;
            }
            if initial_display_delay && r.flag()? {
                r.skip(4)?; // initial_display_delay_minus_1
            }
        }
    }
    let width_bits = r.bits(4)? + 1;
    let height_bits = r.bits(4)? + 1;
    r.skip(width_bits + height_bits)?; // max_frame_{width,height}_minus_1
    if !reduced && r.flag()? {
        // frame_id_numbers_present_flag
        r.skip(4 + 3)?; // delta_frame_id_length_minus_2, additional_frame_id_length_minus_1
    }
    r.skip(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    if !reduced {
        r.skip(4)?; // interintra_compound, masked_compound, warped_motion, dual_filter
        let order_hint = r.flag()?;
        if order_hint {
            r.skip(2)?; // enable_jnt_comp, enable_ref_frame_mvs
        }
        // seq_choose_screen_content_tools, else seq_force_screen_content_tools;
        // SELECT_SCREEN_CONTENT_TOOLS (2) is > 0.
        let screen_content_tools = r.flag()? || r.flag()?;
        if screen_content_tools && !r.flag()? {
            // seq_choose_integer_mv = 0
            r.skip(1)?; // seq_force_integer_mv
        }
        if order_hint {
            r.skip(3)?; // order_hint_bits_minus_1
        }
    }
    r.skip(3)?; // enable_superres, enable_cdef, enable_restoration
    // color_config()
    let high_bitdepth = r.flag()?;
    if seq_profile == 2 && high_bitdepth {
        r.skip(1)?; // twelve_bit
    }
    let mono_chrome = if seq_profile == 1 { false } else { r.flag()? };
    let mut s = Signalled::default();
    let (cp, tc, mc) = if r.flag()? {
        // color_description_present_flag
        let cp = r.bits(8)? as u8;
        let tc = r.bits(8)? as u8;
        let mc = r.bits(8)? as u8;
        (s.primaries, s.transfer, s.matrix) = (Some(cp), Some(tc), Some(mc));
        (cp, tc, mc)
    } else {
        (2, 2, 2)
    };
    s.full_range = Some(if !mono_chrome && (cp, tc, mc) == (1, 13, 0) {
        // sRGB-identity: color_range is implied 1.
        true
    } else {
        r.flag()?
    });
    Ok(s)
}

/// A VP9 keyframe's uncompressed header (VP9 6.2), as far as `color_config`.
pub(crate) fn vp9_keyframe(frame: &[u8]) -> Result<Vp9Color, String> {
    let mut r = Bits::new(frame);
    if r.bits(2)? != 2 {
        return Err("not a VP9 frame: frame_marker is not 2".into());
    }
    let low = r.bits(1)?;
    let profile = (r.bits(1)? << 1) | low;
    if profile == 3 {
        r.skip(1)?; // reserved_zero
    }
    if r.flag()? {
        return Err("show_existing_frame carries no header".into());
    }
    if r.flag()? {
        return Err("not a keyframe (frame_type = 1)".into());
    }
    r.skip(2)?; // show_frame, error_resilient_mode
    if r.bits(24)? != 0x49_83_42 {
        return Err("bad frame_sync_code".into());
    }
    if profile >= 2 {
        r.skip(1)?; // ten_or_twelve_bit
    }
    let color_space = r.bits(3)? as u8;
    // CS_RGB (7) has no color_range bit: it is always full.
    let full_range = color_space == 7 || r.flag()?;
    Ok(Vp9Color {
        profile: profile as u8,
        color_space,
        full_range,
    })
}

/// A BGRA frame of the 75 % colour bars #3049 measured (grey, yellow, cyan,
/// green, magenta, red, blue, black), with a block that moves with `t` so
/// consecutive frames differ: what the families' colour tests encode. A
/// build with no encoder family has none of those tests.
#[cfg_attr(
    not(any(
        feature = "openh264-encoder",
        feature = "vp9-444",
        feature = "ffmpeg-encoder",
        all(target_os = "windows", feature = "mf-encoder")
    )),
    allow(dead_code)
)]
pub(crate) fn bars(width: u32, height: u32, t: u64) -> crate::capture::Frame {
    const RGB: [[u8; 3]; 8] = [
        [191, 191, 191],
        [191, 191, 0],
        [0, 191, 191],
        [0, 191, 0],
        [191, 0, 191],
        [191, 0, 0],
        [0, 0, 191],
        [0, 0, 0],
    ];
    let (w, h) = (width as usize, height as usize);
    let mut data = vec![0u8; w * h * 4];
    let moving = (t as usize * 16) % w.max(1);
    for y in 0..h {
        for x in 0..w {
            let [r, g, b] = if y < 16 && (moving..moving + 16).contains(&x) {
                [255, 255, 255]
            } else {
                RGB[x * RGB.len() / w]
            };
            let px = (y * w + x) * 4;
            data[px..px + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    crate::capture::Frame {
        width,
        height,
        stride: width * 4,
        pixel_format: crate::capture::PixelFormat::Bgra,
        data: data.into(),
        monotonic_us: t * 33_333,
        monitor: 0,
        damage: crate::capture::Damage::Unknown,
        source: None,
    }
}

/// The NAL unit types of an Annex-B stream, in order: for a test that wants to
/// say where an encoder put its parameter sets.
pub(crate) fn nal_types(stream: &[u8], hevc: bool) -> Vec<u8> {
    annexb_nals(stream)
        .into_iter()
        .map(|n| {
            if hevc {
                (n[0] >> 1) & 0x3f
            } else {
                n[0] & 0x1f
            }
        })
        .collect()
}

/// The NAL units of an Annex-B stream (3- or 4-byte start codes), trailing
/// zero bytes trimmed.
fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &start) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(data.len(), |next| next - 3);
        let mut nal = &data[start..end];
        while let [rest @ .., 0] = nal {
            nal = rest;
        }
        if !nal.is_empty() {
            out.push(nal);
        }
    }
    out
}

/// A NAL payload without its emulation prevention bytes (the `03` of every
/// `00 00 03`).
fn rbsp(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0;
    for &b in ebsp {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// The OBUs of a low-overhead AV1 stream: `(obu_type, payload)`.
fn av1_obus(data: &[u8]) -> Result<Vec<(u8, &[u8])>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let header = data[i];
        if header & 0x80 != 0 {
            return Err(format!("obu_forbidden_bit set at byte {i}"));
        }
        let obu_type = (header >> 3) & 0x0f;
        let extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        i += 1 + usize::from(extension);
        if !has_size {
            // The last OBU of a unit may run to its end.
            out.push((obu_type, data.get(i..).unwrap_or(&[])));
            break;
        }
        let (size, n) = leb128(data.get(i..).unwrap_or(&[]))?;
        i += n;
        let end = i
            .checked_add(size)
            .filter(|e| *e <= data.len())
            .ok_or_else(|| format!("an OBU of {size} bytes overruns the stream at {i}"))?;
        out.push((obu_type, &data[i..end]));
        i = end;
    }
    Ok(out)
}

fn leb128(b: &[u8]) -> Result<(usize, usize), String> {
    let mut v = 0u64;
    for (i, &x) in b.iter().take(8).enumerate() {
        v |= u64::from(x & 0x7f) << (7 * i);
        if x & 0x80 == 0 {
            return Ok((v as usize, i + 1));
        }
    }
    Err("a truncated or over-long leb128".into())
}

/// An MSB-first bit reader.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Result<u32, String> {
        let byte = *self
            .data
            .get(self.pos / 8)
            .ok_or_else(|| format!("truncated at bit {}", self.pos))?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(u32::from(b))
    }

    fn flag(&mut self) -> Result<bool, String> {
        Ok(self.bit()? == 1)
    }

    /// `n` ≤ 32 bits as an unsigned number.
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        debug_assert!(n <= 32);
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    fn skip(&mut self, n: u32) -> Result<(), String> {
        let end = self.pos + n as usize;
        if end > self.data.len() * 8 {
            return Err(format!("truncated skipping {n} bits at bit {}", self.pos));
        }
        self.pos = end;
        Ok(())
    }

    /// `ue(v)`, unsigned Exp-Golomb.
    fn ue(&mut self) -> Result<u32, String> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(format!(
                    "an Exp-Golomb code over 32 bits at bit {}",
                    self.pos
                ));
            }
        }
        Ok(((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)) as u32)
    }

    /// `se(v)`, signed Exp-Golomb.
    fn se(&mut self) -> Result<i32, String> {
        let k = i64::from(self.ue()?);
        Ok(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }

    /// AV1's `uvlc()`.
    fn uvlc(&mut self) -> Result<u32, String> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
        }
        if zeros >= 32 {
            return Ok(u32::MAX);
        }
        Ok(((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signalled shorthand: full range, then primaries / transfer / matrix.
    fn said(full: Option<bool>, cp: u8, tc: u8, mc: u8) -> Signalled {
        Signalled {
            full_range: full,
            primaries: Some(cp),
            transfer: Some(tc),
            matrix: Some(mc),
        }
    }

    fn annexb(nal: &[u8]) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 1];
        out.extend_from_slice(nal);
        out
    }

    // ── Fixtures from independent encoders (WSL ffmpeg 6.1.1, 2026-10-30) ──
    // Each is the SPS / sequence header / keyframe head of a two-frame
    // `testsrc2` encode with the colour tags in its name, and ffprobe's
    // reading of the file is in the comment.

    /// libx264 High, 64x64, `-color_primaries bt709 -color_trc iec61966-2-1
    /// -colorspace smpte170m -color_range tv` (ffprobe: tv / smpte170m /
    /// iec61966-2-1 / bt709). Carries emulation prevention bytes.
    const X264_HIGH_S: &[u8] = &[
        0x67, 0x64, 0x00, 0x0a, 0xac, 0xd9, 0x44, 0x26, 0xc0, 0x5a, 0x80, 0x86, 0x83, 0x20, 0x00,
        0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x07, 0x81, 0xe2, 0x44, 0xb2, 0xc0,
    ];
    /// libx264 Constrained Baseline, the same tags (no High-profile fields).
    const X264_CBP_S: &[u8] = &[
        0x67, 0x42, 0xc0, 0x0a, 0xda, 0x10, 0x9b, 0x01, 0x6a, 0x02, 0x1a, 0x0c, 0x80, 0x00, 0x00,
        0x03, 0x00, 0x80, 0x00, 0x00, 0x1e, 0x07, 0x89, 0x13, 0x50,
    ];
    /// libx264 High 4:4:4 Predictive, 66x50 (frame cropping), `cqm=jvt`
    /// (12 scaling lists), `bt470bg / gamma28 / bt470bg`, full range.
    const X264_444_CROP_CQM_FULL: &[u8] = &[
        0x67, 0xf4, 0x00, 0x0a, 0x91, 0x96, 0x85, 0x27, 0x8f, 0x8f, 0xc0, 0x5b, 0x82, 0x82, 0x82,
        0xa0, 0x00, 0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x07, 0x81, 0xe2, 0x44, 0xd4,
    ];
    /// libx264 Constrained Baseline, no colour tags (ffprobe: unknown ×4).
    const X264_UNTAGGED: &[u8] = &[
        0x67, 0x42, 0xc0, 0x0a, 0xda, 0x10, 0x9b, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00,
        0x00, 0x03, 0x03, 0xc0, 0xf1, 0x22, 0x6a,
    ];
    /// libx265 Main, 64x64, the set-S tags, limited range.
    const X265_MAIN_S: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x1e, 0xa0, 0x20, 0x81, 0x05, 0x96, 0x56, 0x54, 0xa4, 0xc2, 0xf0, 0x16, 0xa0,
        0x21, 0xa0, 0xc0, 0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x0f, 0x04,
    ];
    /// libx265 Rext 4:4:4, 66x50 (conformance window), `temporal-layers=1`
    /// (a sub-layer in profile_tier_level), `scaling-list=default`,
    /// `bt2020 / bt2020-10 / bt2020nc`, full range.
    const X265_444_SUBLAYER_FULL: &[u8] = &[
        0x42, 0x01, 0x02, 0x04, 0x08, 0x00, 0x00, 0x03, 0x00, 0x9e, 0x08, 0x00, 0x00, 0x03, 0x00,
        0x00, 0x1e, 0x00, 0x00, 0x90, 0x05, 0x10, 0x20, 0xe3, 0xe3, 0xf2, 0xca, 0xca, 0x56, 0x54,
        0xa4, 0xe1, 0x78, 0x0b, 0x70, 0x90, 0xe0, 0x90, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40, 0x00,
        0x00, 0x07, 0x82,
    ];
    /// libx265 Main, no colour tags.
    const X265_UNTAGGED: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x1e, 0xa0, 0x20, 0x81, 0x05, 0x96, 0x56, 0x54, 0xa4, 0xc2, 0xf0, 0x16, 0x80,
        0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x0f, 0x04,
    ];
    /// libaom Main 4:2:0, the set-S tags, limited range: the whole OBU.
    const AOM_420_S: &[u8] = &[
        0x0a, 0x0d, 0x00, 0x00, 0x00, 0x02, 0xaf, 0xff, 0x9b, 0x5f, 0x22, 0x02, 0x1a, 0x0c, 0x08,
    ];
    /// libaom High (seq_profile 1, 4:4:4), `bt2020 / bt2020-10 / bt2020nc`, full.
    const AOM_444_FULL: &[u8] = &[
        0x0a, 0x0d, 0x20, 0x00, 0x00, 0x02, 0xaf, 0xff, 0x9b, 0x5f, 0x24, 0x24, 0x38, 0x26, 0x40,
    ];
    /// libaom Main, no colour tags (no description; the range bit 0).
    const AOM_UNTAGGED: &[u8] = &[
        0x0a, 0x0a, 0x00, 0x00, 0x00, 0x02, 0xaf, 0xff, 0x9b, 0x5f, 0x20, 0x08,
    ];
    /// libvpx-vp9 profile 0, `-colorspace smpte170m -color_range tv`: the
    /// keyframe's first bytes.
    const VPX_P0_SMPTE170: &[u8] = &[
        0x82, 0x49, 0x83, 0x42, 0x60, 0x03, 0xf0, 0x03, 0xf6, 0x06, 0x38, 0x24, 0x1c, 0x18, 0x4a,
        0x00,
    ];
    /// libvpx-vp9 profile 0, no tags.
    const VPX_P0_UNTAGGED: &[u8] = &[
        0x82, 0x49, 0x83, 0x42, 0x00, 0x03, 0xf0, 0x03, 0xf6, 0x06, 0x38, 0x24, 0x1c, 0x18, 0x4a,
        0x00,
    ];
    /// libvpx-vp9 profile 1 (4:4:4), `-colorspace bt709 -color_range pc`.
    const VPX_P1_709_FULL: &[u8] = &[
        0xa2, 0x49, 0x83, 0x42, 0x50, 0x00, 0x7e, 0x00, 0x7e, 0xc0, 0xc7, 0x04, 0x83, 0x83, 0x09,
        0x40,
    ];

    #[test]
    fn h264_fixtures_read_as_ffprobe_reads_them() {
        assert_eq!(h264_sps(X264_HIGH_S).unwrap(), said(Some(false), 1, 13, 6));
        assert_eq!(h264_sps(X264_CBP_S).unwrap(), said(Some(false), 1, 13, 6));
        assert_eq!(
            h264_sps(X264_444_CROP_CQM_FULL).unwrap(),
            said(Some(true), 5, 5, 5)
        );
        let untagged = h264_sps(X264_UNTAGGED).unwrap();
        assert_eq!(untagged, Signalled::default());
        assert!(untagged.unspecified());
        // Through the Annex-B walk, behind other NAL units.
        let mut stream = annexb(&[0x09, 0xf0]); // access unit delimiter
        stream.extend(annexb(X264_HIGH_S));
        stream.extend(annexb(&[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0])); // a PPS
        assert_eq!(nal_types(&stream, false), vec![9, 7, 8]);
        assert_eq!(h264(&stream).unwrap(), said(Some(false), 1, 13, 6));
        assert!(h264(&annexb(&[0x68, 0xeb])).is_err(), "no SPS");
    }

    #[test]
    fn hevc_fixtures_read_as_ffprobe_reads_them() {
        assert_eq!(hevc_sps(X265_MAIN_S).unwrap(), said(Some(false), 1, 13, 6));
        assert_eq!(
            hevc_sps(X265_444_SUBLAYER_FULL).unwrap(),
            said(Some(true), 9, 14, 9)
        );
        let untagged = hevc_sps(X265_UNTAGGED).unwrap();
        assert!(untagged.unspecified(), "{untagged:?}");
        // A VPS ahead of it, as every HEVC encoder sends one.
        let mut stream = annexb(&[0x40, 0x01, 0x0c, 0x01]);
        stream.extend(annexb(X265_MAIN_S));
        assert_eq!(nal_types(&stream, true), vec![32, 33]);
        assert_eq!(hevc(&stream).unwrap(), said(Some(false), 1, 13, 6));
    }

    #[test]
    fn av1_fixtures_read_as_ffprobe_reads_them() {
        assert_eq!(av1(AOM_420_S).unwrap(), said(Some(false), 1, 13, 6));
        assert_eq!(av1(AOM_444_FULL).unwrap(), said(Some(true), 9, 14, 9));
        let untagged = av1(AOM_UNTAGGED).unwrap();
        assert!(untagged.unspecified(), "{untagged:?}");
        assert_eq!(untagged.full_range, Some(false));
        // A temporal delimiter first, as every temporal unit starts.
        let mut stream = vec![0x12, 0x00];
        stream.extend_from_slice(AOM_420_S);
        assert_eq!(av1(&stream).unwrap(), said(Some(false), 1, 13, 6));
        assert!(av1(&[0x12, 0x00]).is_err(), "no sequence header");
    }

    #[test]
    fn vp9_fixtures_read_as_ffprobe_reads_them() {
        let p0 = vp9_keyframe(VPX_P0_SMPTE170).unwrap();
        assert_eq!((p0.profile, p0.color_space, p0.full_range), (0, 3, false));
        let untagged = vp9_keyframe(VPX_P0_UNTAGGED).unwrap();
        assert_eq!(untagged.color_space, 0, "CS_UNKNOWN");
        let p1 = vp9_keyframe(VPX_P1_709_FULL).unwrap();
        assert_eq!((p1.profile, p1.color_space, p1.full_range), (1, 2, true));
    }

    // ── The branches no fixture above takes, built bit by bit ──

    /// An MSB-first bit writer with H.264's Exp-Golomb codes.
    #[derive(Default)]
    struct W {
        out: Vec<u8>,
        n: usize,
    }

    impl W {
        /// `bits` may exceed 64: the high ones are zeros.
        fn put(&mut self, v: u64, bits: u32) -> &mut Self {
            for i in (0..bits).rev() {
                if self.n.is_multiple_of(8) {
                    self.out.push(0);
                }
                if i < 64 && (v >> i) & 1 == 1 {
                    *self.out.last_mut().unwrap() |= 0x80 >> (self.n % 8);
                }
                self.n += 1;
            }
            self
        }
        fn ue(&mut self, v: u32) -> &mut Self {
            let x = u64::from(v) + 1;
            let len = 64 - x.leading_zeros();
            self.put(0, len - 1).put(x, len)
        }
        fn se(&mut self, v: i32) -> &mut Self {
            self.ue(if v > 0 {
                2 * v as u32 - 1
            } else {
                (-2 * v) as u32
            })
        }
        /// The bytes, with the trailing bits a real NAL ends with.
        fn rbsp(&mut self) -> Vec<u8> {
            self.put(1, 1);
            while !self.n.is_multiple_of(8) {
                self.put(0, 1);
            }
            self.out.clone()
        }
    }

    /// H.264 7.4.1: an encoder inserts `03` after any two zero bytes that a
    /// byte ≤ 3 follows.
    fn escape(rbsp: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut zeros = 0;
        for &b in rbsp {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    /// The VUI head both H.26x codecs share: an extended SAR, overscan, and
    /// the colour description.
    fn vui_head(w: &mut W, cp: u8, tc: u8, mc: u8, full: bool) {
        w.put(1, 1).put(255, 8).put(16, 16).put(9, 16); // aspect ratio, Extended_SAR
        w.put(1, 1).put(0, 1); // overscan
        w.put(1, 1).put(5, 3).put(u64::from(full), 1); // signal type, video_format 5
        w.put(1, 1)
            .put(cp.into(), 8)
            .put(tc.into(), 8)
            .put(mc.into(), 8);
        w.put(0, 1); // chroma_loc_info_present_flag: past what we read
    }

    #[test]
    fn exp_golomb_and_emulation_prevention_round_trip() {
        let mut w = W::default();
        for v in [0u32, 1, 2, 3, 7, 254, 255, 65_535, 1 << 20] {
            w.ue(v);
        }
        for v in [0i32, 1, -1, 2, -2, 127, -128] {
            w.se(v);
        }
        let bytes = w.rbsp();
        let mut r = Bits::new(&bytes);
        for v in [0u32, 1, 2, 3, 7, 254, 255, 65_535, 1 << 20] {
            assert_eq!(r.ue().unwrap(), v);
        }
        for v in [0i32, 1, -1, 2, -2, 127, -128] {
            assert_eq!(r.se().unwrap(), v);
        }
        let raw = [0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00];
        let sent = escape(&raw);
        // An escape restarts the zero count, so the raw `00 00 03` at the end
        // needs none: only one zero precedes its `03` once the one before is in.
        assert_eq!(
            sent,
            [
                0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x03, 0x00
            ]
        );
        assert_eq!(rbsp(&sent), raw);
        assert!(
            Bits::new(&[0x00, 0x00, 0x00, 0x00, 0x01]).ue().is_err(),
            "> 32 bits"
        );
    }

    /// pic_order_cnt_type 1 (the offset loop), interlaced (mb_adaptive_frame_field_flag),
    /// High 4:2:2 with eight scaling lists (one absent, two ending early on a
    /// zero `nextScale`: after it the list repeats its last scale and carries
    /// no more codes), an extended SAR, and the bytes escaped as an encoder
    /// sends them.
    #[test]
    fn h264_reads_past_every_optional_sps_field() {
        let mut w = W::default();
        w.put(122, 8).put(0, 8).put(40, 8).ue(0); // High 4:2:2, level 4.0, sps id
        w.ue(2).ue(0).ue(0).put(0, 1); // chroma 4:2:2, 8-bit, no bypass
        w.put(1, 1); // seq_scaling_matrix_present_flag
        for i in 0..8 {
            if i == 2 {
                w.put(0, 1); // this list absent
                continue;
            }
            w.put(1, 1);
            match i {
                // nextScale 0 at j = 0: the default list, one code.
                4 => {
                    w.se(-8);
                }
                // 9, then 0 at j = 1: two codes for 64 entries.
                6 => {
                    w.se(1).se(-9);
                }
                // 9, 8, 9, 8, …: never 0, so every entry carries a code.
                _ => {
                    for j in 0..if i < 6 { 16 } else { 64 } {
                        w.se(if j % 2 == 0 { 1 } else { -1 });
                    }
                }
            }
        }
        w.ue(0).ue(1); // log2_max_frame_num_minus4, pic_order_cnt_type 1
        w.put(0, 1).se(-1).se(2).ue(3).se(1).se(-2).se(3); // three offsets
        w.ue(4).put(0, 1).ue(119).ue(67); // refs, gaps, 1920 x (68*16)
        w.put(0, 1).put(1, 1); // frame_mbs_only 0, mb_adaptive_frame_field 1
        w.put(1, 1); // direct_8x8_inference
        w.put(1, 1).ue(0).ue(0).ue(0).ue(4); // cropping
        w.put(1, 1); // vui_parameters_present_flag
        vui_head(&mut w, 6, 6, 6, false);
        let mut nal = vec![0x67];
        nal.extend(escape(&w.rbsp()));
        assert_eq!(h264_sps(&nal).unwrap(), said(Some(false), 6, 6, 6));
    }

    /// A signal type without a colour description reads as a range and
    /// nothing else; no VUI at all reads as nothing.
    #[test]
    fn h264_range_without_a_description() {
        let mut w = W::default();
        w.put(66, 8)
            .put(0, 8)
            .put(30, 8)
            .ue(0)
            .ue(0)
            .ue(2)
            .ue(1)
            .put(0, 1);
        w.ue(39).ue(29).put(1, 1).put(1, 1).put(0, 1); // 640x480, no cropping
        w.put(1, 1).put(0, 1).put(0, 1); // VUI: no SAR, no overscan
        w.put(1, 1).put(5, 3).put(1, 1).put(0, 1); // full range, no description
        let mut nal = vec![0x67];
        nal.extend(escape(&w.rbsp()));
        let s = h264_sps(&nal).unwrap();
        assert_eq!(s.full_range, Some(true));
        assert!(s.unspecified());
    }

    /// Two sub-layers (one with its own profile and level), explicit scaling
    /// list data (predicted and coded lists, the 32×32 step of 3), PCM, four
    /// short-term RPS (the second to fourth inter-predicted, one dropping a
    /// picture through use_delta_flag), long-term pictures, and the VUI.
    #[test]
    fn hevc_reads_past_every_optional_sps_field() {
        let mut w = W::default();
        w.put(0, 4).put(1, 3).put(1, 1); // vps id, max_sub_layers_minus1 1, nesting
        w.put(0, 2)
            .put(0, 1)
            .put(4, 5)
            .put(0x0800_0000, 32)
            .put(0xb, 4)
            .put(0, 44); // general PTL (Rext)
        w.put(120, 8); // general_level_idc
        w.put(1, 1).put(1, 1); // sub_layer_profile/level_present_flag[0]
        w.put(0, 2 * 7); // reserved_zero_2bits for i = 1..8
        w.put(0, 88).put(90, 8); // the sub-layer's profile and level
        w.ue(0).ue(3).put(0, 1); // sps id, 4:4:4, separate_colour_plane 0
        w.ue(1920).ue(1080).put(1, 1).ue(0).ue(0).ue(0).ue(4); // dims, conformance window
        w.ue(0).ue(0).ue(4); // 8-bit, log2_max_poc_lsb 8
        w.put(1, 1); // sub-layer ordering info for both layers
        w.ue(4).ue(0).ue(0).ue(4).ue(0).ue(0);
        w.ue(0).ue(3).ue(0).ue(3).ue(1).ue(1); // block sizes, hierarchy depths
        w.put(1, 1).put(1, 1); // scaling_list_enabled + data present
        for size_id in 0..4u32 {
            let step = if size_id == 3 { 3 } else { 1 };
            let mut matrix_id = 0;
            while matrix_id < 6 {
                if matrix_id % 2 == 0 {
                    w.put(1, 1); // coded
                    if size_id > 1 {
                        w.se(8);
                    }
                    for k in 0..64.min(1u32 << (4 + (size_id << 1))) {
                        w.se(if k % 2 == 0 { 2 } else { -2 });
                    }
                } else {
                    w.put(0, 1).ue(1); // predicted from the previous list
                }
                matrix_id += step;
            }
        }
        w.put(1, 1).put(1, 1); // amp, sao
        w.put(1, 1).put(7, 4).put(7, 4).ue(0).ue(1).put(0, 1); // pcm
        w.ue(4); // num_short_term_ref_pic_sets
        w.ue(2).ue(1); // RPS 0: two before, one after
        w.ue(0).put(1, 1).ue(1).put(1, 1).ue(0).put(0, 1); // {-1, -3 | +1}
        // RPS 1 from RPS 0 (3 pictures, so 4 flag groups), deltaRps = -1:
        // -1-1 kept; -3-1 dropped by its use_delta_flag; +1-1 = 0 is never a
        // reference whatever its flag; deltaRps itself kept. {-1, -2 | }.
        w.put(1, 1).put(1, 1).ue(0);
        w.put(1, 1).put(0, 1).put(0, 1).put(1, 1).put(1, 1);
        // RPS 2 from RPS 1 (2 pictures: 3 groups), deltaRps = +2, all used:
        // -1+2 = 1 kept, -2+2 = 0 not, +2 kept. { | 1, 2}.
        w.put(1, 1).put(0, 1).ue(1);
        w.put(1, 1).put(1, 1).put(1, 1);
        // RPS 3 from RPS 2 (2 pictures: 3 groups), deltaRps = -1, all used.
        w.put(1, 1).put(1, 1).ue(0);
        w.put(1, 1).put(1, 1).put(1, 1);
        w.put(1, 1).ue(2).put(5, 8).put(1, 1).put(9, 8).put(0, 1); // long-term pictures
        w.put(1, 1).put(1, 1); // temporal_mvp, strong_intra_smoothing
        w.put(1, 1); // vui_parameters_present_flag
        vui_head(&mut w, 1, 13, 6, true);
        let mut nal = vec![0x42, 0x01];
        nal.extend(escape(&w.rbsp()));
        assert_eq!(hevc_sps(&nal).unwrap(), said(Some(true), 1, 13, 6));
    }

    /// The derivation the inter-predicted sets above rely on (H.265 (7-61),
    /// (7-62)), for the sets that test builds: a set's size decides how many
    /// flags the next one carries, so a wrong count desynchronises the read.
    #[test]
    fn hevc_inter_rps_follows_the_spec_derivation() {
        let rps0 = Rps {
            s0: vec![-1, -3],
            s1: vec![1],
        };
        let derive = |bits: &mut W, idx: usize, sets: &[Rps]| {
            let bytes = bits.rbsp();
            st_ref_pic_set(&mut Bits::new(&bytes), idx, sets).unwrap()
        };
        let mut w = W::default();
        w.put(1, 1).put(1, 1).ue(0); // inter, deltaRps = -1
        w.put(1, 1).put(0, 1).put(0, 1).put(1, 1).put(1, 1);
        let rps1 = derive(&mut w, 1, std::slice::from_ref(&rps0));
        assert_eq!(
            rps1,
            Rps {
                s0: vec![-1, -2],
                s1: vec![]
            }
        );
        let mut w = W::default();
        w.put(1, 1).put(0, 1).ue(1); // inter, deltaRps = +2
        w.put(1, 1).put(1, 1).put(1, 1);
        let rps2 = derive(&mut w, 2, &[rps0.clone(), rps1.clone()]);
        assert_eq!(
            rps2,
            Rps {
                s0: vec![],
                s1: vec![1, 2]
            }
        );
        let mut w = W::default();
        w.put(1, 1).put(1, 1).ue(0); // inter, deltaRps = -1
        w.put(1, 1).put(1, 1).put(1, 1);
        let rps3 = derive(&mut w, 3, &[rps0, rps1, rps2]);
        assert_eq!(
            rps3,
            Rps {
                s0: vec![-1],
                s1: vec![1]
            }
        );
    }

    /// Timing info with a decoder model (two operating points, one with a
    /// decoder model and an initial display delay), frame ids, order hints,
    /// forced screen-content tools, and a high-bitdepth profile 2 stream.
    #[test]
    fn av1_reads_past_every_optional_sequence_header_field() {
        let mut w = W::default();
        w.put(2, 3).put(0, 1).put(0, 1); // seq_profile 2, not still, not reduced
        w.put(1, 1).put(1001, 32).put(60_000, 32); // timing_info
        w.put(1, 1).ue(0); // equal_picture_interval, uvlc(0)
        w.put(1, 1).put(9, 5).put(90_000, 32).put(31, 5).put(15, 5); // decoder model, buffer delay 10 bits
        w.put(1, 1); // initial_display_delay_present_flag
        w.put(1, 5); // two operating points
        w.put(0x103, 12).put(12, 5).put(1, 1); // idc, level 12 (> 7): tier
        w.put(1, 1).put(500, 10).put(400, 10).put(1, 1); // decoder model for this op
        w.put(1, 1).put(9, 4); // initial display delay
        w.put(0x001, 12).put(5, 5); // level 5: no tier
        w.put(0, 1).put(0, 1); // no decoder model, no display delay
        w.put(11, 4).put(10, 4).put(1919, 12).put(1079, 11); // max frame size
        w.put(1, 1).put(5, 4).put(2, 3); // frame_id_numbers_present
        w.put(0b011, 3).put(0b1010, 4); // superblock/filters, compound tools
        w.put(1, 1).put(0b11, 2); // order hint, jnt_comp, ref_frame_mvs
        w.put(0, 1).put(1, 1); // screen content forced on
        w.put(0, 1).put(1, 1); // integer mv forced
        w.put(6, 3); // order_hint_bits_minus_1
        w.put(0b011, 3); // superres, cdef, restoration
        w.put(1, 1).put(1, 1); // high_bitdepth, twelve_bit
        w.put(0, 1); // mono_chrome
        w.put(1, 1).put(9, 8).put(16, 8).put(9, 8); // description: BT.2020 PQ
        w.put(1, 1); // color_range
        let bytes = w.rbsp();
        assert_eq!(
            av1_sequence_header(&bytes).unwrap(),
            said(Some(true), 9, 16, 9)
        );
    }

    /// A reduced still-picture header in monochrome, and the sRGB identity
    /// case whose range is implied rather than coded.
    #[test]
    fn av1_reduced_monochrome_and_srgb_identity() {
        let mut w = W::default();
        w.put(0, 3).put(1, 1).put(1, 1).put(4, 5); // reduced, level 4
        w.put(7, 4).put(7, 4).put(63, 8).put(63, 8); // 64x64
        w.put(0, 3).put(0, 3); // superblock/filters, superres/cdef/restoration
        w.put(0, 1).put(1, 1); // 8-bit, mono_chrome
        w.put(0, 1).put(1, 1); // no description, color_range 1
        let bytes = w.rbsp();
        let mono = av1_sequence_header(&bytes).unwrap();
        assert!(mono.unspecified());
        assert_eq!(mono.full_range, Some(true));

        let mut w = W::default();
        w.put(1, 3).put(0, 1).put(1, 1).put(4, 5); // profile 1, reduced
        w.put(7, 4).put(7, 4).put(63, 8).put(63, 8);
        w.put(0, 3).put(0, 3);
        w.put(0, 1); // 8-bit; profile 1 has no mono_chrome bit
        w.put(1, 1).put(1, 8).put(13, 8).put(0, 8); // BT.709 / sRGB / identity
        w.put(0, 1); // separate_uv_delta_q: no range bit before it
        let bytes = w.rbsp();
        assert_eq!(
            av1_sequence_header(&bytes).unwrap(),
            said(Some(true), 1, 13, 0)
        );
    }

    /// Profile 2's bit-depth bit, the RGB colour space (no range bit), and
    /// what is not a keyframe header at all.
    #[test]
    fn vp9_profile_2_rgb_and_refusals() {
        // frame_marker 2, profile 2 (low 0, high 1), shown keyframe, sync
        // code, ten_or_twelve_bit, CS_RGB, then profile 2's no subsampling.
        let mut w = W::default();
        w.put(2, 2)
            .put(0, 1)
            .put(1, 1)
            .put(0, 1)
            .put(0, 1)
            .put(1, 1)
            .put(0, 1);
        w.put(0x49_83_42, 24).put(1, 1).put(7, 3);
        let bytes = w.rbsp();
        let c = vp9_keyframe(&bytes).unwrap();
        assert_eq!((c.profile, c.color_space, c.full_range), (2, 7, true));
        // An inter frame (frame_type 1).
        assert!(vp9_keyframe(&[0x86, 0x00]).is_err());
        // show_existing_frame.
        assert!(vp9_keyframe(&[0x88, 0x00]).is_err());
        assert!(vp9_keyframe(&[0x00]).is_err(), "frame_marker");
    }

    /// `mismatches`: exact code points for a backend handed them; BT.601's
    /// matrix codes merged for AMF's profile; matrix and transfer merged for
    /// MF's and CoreVideo's names; primaries and range never merged.
    #[test]
    fn each_vocabulary_merges_only_what_its_api_cannot_tell_apart() {
        use crate::encode::color_desc::{ColorDescription, Matrix, Primaries, Transfer};
        use Field::{Matrix as M, Primaries as P, Range as R, Transfer as T};
        use Vocabulary::{AmfProfile, CodePoints, Merged};
        let d = ColorDescription {
            matrix: Matrix::Smpte170m,
            primaries: Primaries::Smpte170m,
            transfer: Transfer::Smpte170m,
            full_range: false,
        };
        assert!(said(Some(false), 6, 6, 6).check(d, CodePoints).is_ok());
        let five_one = said(Some(false), 6, 1, 5);
        assert_eq!(five_one.mismatches(d, CodePoints), vec![M, T]);
        assert_eq!(five_one.mismatches(d, AmfProfile), vec![T]);
        assert!(five_one.check(d, Merged).is_ok());
        assert_eq!(
            said(Some(false), 1, 6, 6).mismatches(d, Merged),
            vec![P],
            "primaries are never merged"
        );
        assert_eq!(said(Some(true), 6, 6, 6).mismatches(d, Merged), vec![R]);
        assert_eq!(
            said(Some(false), 6, 6, 2).mismatches(d, Merged),
            vec![M],
            "unspecified is no BT.601"
        );
        assert_eq!(Signalled::default().mismatches(d, Merged), vec![M, P, T, R]);
        assert!(said(None, 2, 2, 2).unspecified());
        assert!(!said(None, 2, 2, 6).unspecified());
    }
}
