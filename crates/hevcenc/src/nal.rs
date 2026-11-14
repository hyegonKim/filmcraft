//! NAL units and parameter sets: VPS (7.3.2.1), SPS (7.3.2.2) with VUI (E.2.1), PPS (7.3.2.3),
//! slice segment headers (7.3.6.1) and the NAL unit header (7.3.1.2).

use filmcraft_bitstream::{BitWriter, escape_rbsp};

pub const NAL_TRAIL_R: u8 = 1;
pub const NAL_IDR_W_RADL: u8 = 19;
pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
pub const NAL_PREFIX_SEI: u8 = 39;

/// A NAL unit (header + escaped payload) without a start code or length prefix.
pub fn nal(nal_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut v = vec![nal_type << 1, 1]; // forbidden_zero_bit 0, nuh_layer_id 0, nuh_temporal_id_plus1 1
    v.extend_from_slice(&escape_rbsp(rbsp));
    v
}

/// The colour signal written in the VUI (E.2.1): colour_primaries, transfer_characteristics and
/// matrix_coeffs code points, limited range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorDescription {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
}

impl Default for ColorDescription {
    /// BT.709.
    fn default() -> Self {
        ColorDescription { primaries: 1, transfer: 1, matrix: 1 }
    }
}

/// The sequence-level settings every parameter set is written from.
#[derive(Clone, Debug)]
pub struct SeqParams {
    /// Coded size (multiples of the minimum coding block, 8).
    pub coded_w: u32,
    pub coded_h: u32,
    /// Output (conformance window) size.
    pub width: u32,
    pub height: u32,
    pub bit_depth: u32,
    pub log2_ctb: u32,
    pub level_idc: u8,
    pub color: ColorDescription,
    /// Frame rate as time_scale / num_units_in_tick (VUI timing).
    pub fps_num: u32,
    pub fps_den: u32,
    /// Sample (pixel) aspect ratio, width : height; square when equal or either is 0.
    pub sar: (u16, u16),
}

impl SeqParams {
    pub fn profile_idc(&self) -> u8 {
        if self.bit_depth > 8 { 2 } else { 1 }
    }

    pub fn ctb_size(&self) -> u32 {
        1 << self.log2_ctb
    }

    pub fn width_in_ctbs(&self) -> u32 {
        self.coded_w.div_ceil(self.ctb_size())
    }

    pub fn height_in_ctbs(&self) -> u32 {
        self.coded_h.div_ceil(self.ctb_size())
    }
}

/// general_profile_compatibility_flag[j] (bit 31 - j) of a profile: Main streams are also Main 10
/// streams, so Main 10 decoders take them.
fn compatibility_flags(profile: u8) -> u32 {
    if profile == 1 { (1 << (31 - 1)) | (1 << (31 - 2)) } else { 1 << (31 - 2) }
}

/// The 48 general constraint flags, first flag in bit 47: progressive source and frame only (the
/// interlaced, non-packed, 43 reserved and inbld flags are 0).
const CONSTRAINT_FLAGS: u64 = (1 << 47) | (1 << 44);

/// profile_tier_level(1, 0) (7.3.3): Main or Main 10, Main tier, progressive frames.
fn profile_tier_level(w: &mut BitWriter, p: &SeqParams) {
    let profile = p.profile_idc();
    w.write_bits(0, 2); // general_profile_space
    w.write_bit(false); // general_tier_flag (Main)
    w.write_bits(u32::from(profile), 5);
    w.write_bits(compatibility_flags(profile), 32);
    w.write_bits((CONSTRAINT_FLAGS >> 16) as u32, 32);
    w.write_bits((CONSTRAINT_FLAGS & 0xffff) as u32, 16);
    w.write_bits(u32::from(p.level_idc), 8);
}

/// The HEVCDecoderConfigurationRecord (`hvcC`, ISO/IEC 14496-15 8.3.3.1) of the stream: the
/// profile / tier / level the parameter sets carry, 4:2:0 at the sequence bit depth, one temporal
/// layer (nested), 4-byte NAL unit lengths, and one complete array each of `vps`, `sps` and `pps`
/// (NAL units without a length prefix; each shorter than 64 KiB).
pub fn hvcc(p: &SeqParams, vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let profile = p.profile_idc();
    let depth = (p.bit_depth - 8) as u8;
    let mut v = vec![1, profile]; // configurationVersion; profile_space 0, Main tier, profile_idc
    v.extend_from_slice(&compatibility_flags(profile).to_be_bytes());
    v.extend_from_slice(&CONSTRAINT_FLAGS.to_be_bytes()[2..]);
    v.push(p.level_idc);
    v.extend_from_slice(&[0xf0, 0x00]); // reserved, min_spatial_segmentation_idc 0
    v.push(0xfc); // reserved, parallelismType 0 (unknown)
    v.push(0xfc | 1); // reserved, chroma_format_idc 1
    v.push(0xf8 | depth); // reserved, bit_depth_luma_minus8
    v.push(0xf8 | depth); // reserved, bit_depth_chroma_minus8
    v.extend_from_slice(&[0, 0]); // avgFrameRate: unspecified
    // constantFrameRate 0, numTemporalLayers 1, temporalIdNested 1, lengthSizeMinusOne 3
    v.push((1 << 3) | (1 << 2) | 3);
    v.push(3); // numOfArrays
    for (kind, unit) in [(NAL_VPS, vps), (NAL_SPS, sps), (NAL_PPS, pps)] {
        v.push(0x80 | kind); // array_completeness 1
        v.extend_from_slice(&[0, 1]); // numNalus
        v.extend_from_slice(&(unit.len().min(usize::from(u16::MAX)) as u16).to_be_bytes());
        v.extend_from_slice(unit);
    }
    v
}

pub fn vps(p: &SeqParams) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(0, 4); // vps_video_parameter_set_id
    w.write_bit(true); // vps_base_layer_internal_flag
    w.write_bit(true); // vps_base_layer_available_flag
    w.write_bits(0, 6); // vps_max_layers_minus1
    w.write_bits(0, 3); // vps_max_sub_layers_minus1
    w.write_bit(true); // vps_temporal_id_nesting_flag
    w.write_bits(0xffff, 16); // vps_reserved_0xffff_16bits
    profile_tier_level(&mut w, p);
    w.write_bit(true); // vps_sub_layer_ordering_info_present_flag
    w.write_ue(1); // vps_max_dec_pic_buffering_minus1: the reference + the current picture
    w.write_ue(0); // vps_max_num_reorder_pics
    w.write_ue(0); // vps_max_latency_increase_plus1
    w.write_bits(0, 6); // vps_max_layer_id
    w.write_ue(0); // vps_num_layer_sets_minus1
    w.write_bit(false); // vps_timing_info_present_flag
    w.write_bit(false); // vps_extension_flag
    w.rbsp_trailing();
    nal(NAL_VPS, &w.finish())
}

pub fn sps(p: &SeqParams) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(0, 4); // sps_video_parameter_set_id
    w.write_bits(0, 3); // sps_max_sub_layers_minus1
    w.write_bit(true); // sps_temporal_id_nesting_flag
    profile_tier_level(&mut w, p);
    w.write_ue(0); // sps_seq_parameter_set_id
    w.write_ue(1); // chroma_format_idc: 4:2:0
    w.write_ue(p.coded_w);
    w.write_ue(p.coded_h);
    let (crop_r, crop_b) = ((p.coded_w - p.width) / 2, (p.coded_h - p.height) / 2);
    let crop = crop_r != 0 || crop_b != 0;
    w.write_bit(crop); // conformance_window_flag
    if crop {
        // offsets in chroma samples (SubWidthC = SubHeightC = 2)
        w.write_ue(0);
        w.write_ue(crop_r);
        w.write_ue(0);
        w.write_ue(crop_b);
    }
    w.write_ue(p.bit_depth - 8); // bit_depth_luma_minus8
    w.write_ue(p.bit_depth - 8); // bit_depth_chroma_minus8
    w.write_ue(LOG2_MAX_POC_LSB - 4); // log2_max_pic_order_cnt_lsb_minus4
    w.write_bit(true); // sps_sub_layer_ordering_info_present_flag
    w.write_ue(1); // sps_max_dec_pic_buffering_minus1
    w.write_ue(0); // sps_max_num_reorder_pics
    w.write_ue(0); // sps_max_latency_increase_plus1
    w.write_ue(0); // log2_min_luma_coding_block_size_minus3 (8x8)
    w.write_ue(p.log2_ctb - 3); // log2_diff_max_min_luma_coding_block_size
    w.write_ue(0); // log2_min_luma_transform_block_size_minus2 (4x4)
    w.write_ue(3); // log2_diff_max_min_luma_transform_block_size (32x32)
    w.write_ue(0); // max_transform_hierarchy_depth_inter: transform unit = coding unit
    w.write_ue(0); // max_transform_hierarchy_depth_intra
    w.write_bit(false); // scaling_list_enabled_flag
    w.write_bit(false); // amp_enabled_flag
    w.write_bit(false); // sample_adaptive_offset_enabled_flag
    w.write_bit(false); // pcm_enabled_flag
    w.write_ue(1); // num_short_term_ref_pic_sets
    // st_ref_pic_set(0): the previous picture
    w.write_ue(1); // num_negative_pics
    w.write_ue(0); // num_positive_pics
    w.write_ue(0); // delta_poc_s0_minus1
    w.write_bit(true); // used_by_curr_pic_s0_flag
    w.write_bit(false); // long_term_ref_pics_present_flag
    w.write_bit(false); // sps_temporal_mvp_enabled_flag
    w.write_bit(true); // strong_intra_smoothing_enabled_flag
    w.write_bit(true); // vui_parameters_present_flag
    vui(&mut w, p);
    w.write_bit(false); // sps_extension_present_flag
    w.rbsp_trailing();
    nal(NAL_SPS, &w.finish())
}

/// aspect_ratio_idc of a sample aspect ratio given as sar_width : sar_height (Table E.1).
const EXTENDED_SAR: u32 = 255;

/// vui_parameters() (E.2.1): the sample aspect ratio (when not square), the colour description
/// and the frame rate.
fn vui(w: &mut BitWriter, p: &SeqParams) {
    let (sw, sh) = p.sar;
    let sar = sw > 0 && sh > 0 && sw != sh;
    w.write_bit(sar); // aspect_ratio_info_present_flag
    if sar {
        w.write_bits(EXTENDED_SAR, 8); // aspect_ratio_idc
        w.write_bits(u32::from(sw), 16);
        w.write_bits(u32::from(sh), 16);
    }
    w.write_bit(false); // overscan_info_present_flag
    w.write_bit(true); // video_signal_type_present_flag
    w.write_bits(5, 3); // video_format: unspecified
    w.write_bit(false); // video_full_range_flag
    w.write_bit(true); // colour_description_present_flag
    w.write_bits(u32::from(p.color.primaries), 8);
    w.write_bits(u32::from(p.color.transfer), 8);
    w.write_bits(u32::from(p.color.matrix), 8);
    w.write_bit(false); // chroma_loc_info_present_flag
    w.write_bit(false); // neutral_chroma_indication_flag
    w.write_bit(false); // field_seq_flag
    w.write_bit(false); // frame_field_info_present_flag
    w.write_bit(false); // default_display_window_flag
    let timing = p.fps_num > 0 && p.fps_den > 0;
    w.write_bit(timing); // vui_timing_info_present_flag
    if timing {
        w.write_bits(p.fps_den, 32); // vui_num_units_in_tick
        w.write_bits(p.fps_num, 32); // vui_time_scale
        w.write_bit(false); // vui_poc_proportional_to_timing_flag
        w.write_bit(false); // vui_hrd_parameters_present_flag
    }
    w.write_bit(false); // bitstream_restriction_flag
}

pub fn pps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_ue(0); // pps_pic_parameter_set_id
    w.write_ue(0); // pps_seq_parameter_set_id
    w.write_bit(false); // dependent_slice_segments_enabled_flag
    w.write_bit(false); // output_flag_present_flag
    w.write_bits(0, 3); // num_extra_slice_header_bits
    w.write_bit(false); // sign_data_hiding_enabled_flag
    w.write_bit(false); // cabac_init_present_flag
    w.write_ue(0); // num_ref_idx_l0_default_active_minus1
    w.write_ue(0); // num_ref_idx_l1_default_active_minus1
    w.write_se(0); // init_qp_minus26
    w.write_bit(false); // constrained_intra_pred_flag
    w.write_bit(false); // transform_skip_enabled_flag
    w.write_bit(false); // cu_qp_delta_enabled_flag
    w.write_se(0); // pps_cb_qp_offset
    w.write_se(0); // pps_cr_qp_offset
    w.write_bit(false); // pps_slice_chroma_qp_offsets_present_flag
    w.write_bit(false); // weighted_pred_flag
    w.write_bit(false); // weighted_bipred_flag
    w.write_bit(false); // transquant_bypass_enabled_flag
    w.write_bit(false); // tiles_enabled_flag
    w.write_bit(false); // entropy_coding_sync_enabled_flag
    w.write_bit(false); // pps_loop_filter_across_slices_enabled_flag
    w.write_bit(true); // deblocking_filter_control_present_flag
    w.write_bit(false); // deblocking_filter_override_enabled_flag
    w.write_bit(true); // pps_deblocking_filter_disabled_flag
    w.write_bit(false); // pps_scaling_list_data_present_flag
    w.write_bit(false); // lists_modification_present_flag
    w.write_ue(0); // log2_parallel_merge_level_minus2
    w.write_bit(false); // slice_segment_header_extension_present_flag
    w.write_bit(false); // pps_extension_present_flag
    w.rbsp_trailing();
    nal(NAL_PPS, &w.finish())
}

/// log2 of MaxPicOrderCntLsb.
pub const LOG2_MAX_POC_LSB: u32 = 12;

/// A slice segment header (7.3.6.1) for the encoder's I and P slices.
pub struct SliceHeader {
    pub first_in_pic: bool,
    pub idr: bool,
    /// slice_segment_address (first CTB of the slice, raster order).
    pub address: u32,
    /// Number of bits of slice_segment_address: Ceil(Log2(PicSizeInCtbsY)).
    pub address_bits: u32,
    pub intra: bool,
    pub poc: i32,
    pub qp: i32,
}

impl SliceHeader {
    pub fn write(&self, w: &mut BitWriter) {
        w.write_bit(self.first_in_pic);
        if self.idr {
            w.write_bit(false); // no_output_of_prior_pics_flag
        }
        w.write_ue(0); // slice_pic_parameter_set_id
        if !self.first_in_pic {
            w.write_bits(self.address, self.address_bits);
        }
        w.write_ue(if self.intra { 2 } else { 1 }); // slice_type
        if !self.idr {
            w.write_bits((self.poc & ((1 << LOG2_MAX_POC_LSB) - 1)) as u32, LOG2_MAX_POC_LSB);
            w.write_bit(true); // short_term_ref_pic_set_sps_flag (the SPS's only set: no index)
        }
        if !self.intra {
            w.write_bit(false); // num_ref_idx_active_override_flag
        }
        if !self.intra {
            w.write_ue(4); // five_minus_max_num_merge_cand: one merge candidate
        }
        w.write_se(self.qp - 26); // slice_qp_delta
        // the deblocking filter is off in the PPS, SAO off in the SPS: nothing else to signal
        // byte_alignment()
        w.write_bit(true);
        w.align_zero();
    }
}

/// The lowest HEVC level_idc (general_level_idc = 30 x level) whose Main tier limits take
/// `width` x `height` pictures at `fps`, a peak bitrate of `max_kbps` (0: not bounded) and `slices`
/// slice segments per picture (Tables A.6 and A.8: MaxLumaPs, the picture side limit
/// sqrt(8 x MaxLumaPs), MaxLumaSr, Main tier MaxBR and MaxSliceSegmentsPerPicture). Level 6.2
/// when none does.
pub fn level_for(width: u32, height: u32, fps: f64, max_kbps: u32, slices: usize) -> u8 {
    // (level_idc, MaxLumaPs, MaxLumaSr, Main tier MaxBR in kbit/s, MaxSliceSegmentsPerPicture)
    const LEVELS: [(u8, u64, u64, u64, u64); 13] = [
        (30, 36_864, 552_960, 128, 16),
        (60, 122_880, 3_686_400, 1_500, 16),
        (63, 245_760, 7_372_800, 3_000, 20),
        (90, 552_960, 16_588_800, 6_000, 30),
        (93, 983_040, 33_177_600, 10_000, 40),
        (120, 2_228_224, 66_846_720, 12_000, 75),
        (123, 2_228_224, 133_693_440, 20_000, 75),
        (150, 8_912_896, 267_386_880, 25_000, 200),
        (153, 8_912_896, 534_773_760, 40_000, 200),
        (156, 8_912_896, 1_069_547_520, 60_000, 200),
        (180, 35_651_584, 1_069_547_520, 60_000, 600),
        (183, 35_651_584, 2_139_095_040, 120_000, 600),
        (186, 35_651_584, 4_278_190_080, 240_000, 600),
    ];
    let ps = u64::from(width) * u64::from(height);
    let sr = (ps as f64 * fps.max(1.0)).ceil() as u64;
    let side = u64::from(width.max(height));
    LEVELS
        .iter()
        .find(|(_, mps, msr, mbr, msl)| ps <= *mps && side * side <= mps * 8 && sr <= *msr && u64::from(max_kbps) <= *mbr && slices as u64 <= *msl)
        .map_or(186, |(l, ..)| *l)
}

/// SEI payloadType of the mastering display colour volume (D.2.28) and the content light level
/// information (D.2.35).
const SEI_MASTERING_DISPLAY: u8 = 137;
const SEI_CONTENT_LIGHT: u8 = 144;

/// The RBSP of a prefix SEI NAL unit carrying the HDR10 static metadata: `mastering` is the
/// 24-byte mastering_display_colour_volume payload (display primaries, white point, maximum and
/// minimum luminance, big-endian, the layout of the MP4 `mdcv` box) and `content_light` is
/// (max_content_light_level, max_pic_average_light_level). `None` when there is neither.
pub fn sei_hdr(mastering: Option<&[u8; 24]>, content_light: Option<(u16, u16)>) -> Option<Vec<u8>> {
    if mastering.is_none() && content_light.is_none() {
        return None;
    }
    let mut rbsp = Vec::with_capacity(36);
    if let Some(md) = mastering {
        rbsp.extend_from_slice(&[SEI_MASTERING_DISPLAY, 24]);
        rbsp.extend_from_slice(md);
    }
    if let Some((max_cll, max_fall)) = content_light {
        rbsp.extend_from_slice(&[SEI_CONTENT_LIGHT, 4]);
        rbsp.extend_from_slice(&max_cll.to_be_bytes());
        rbsp.extend_from_slice(&max_fall.to_be_bytes());
    }
    // rbsp_trailing_bits: both payloads end byte-aligned
    rbsp.push(0x80);
    Some(rbsp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_follows_size_rate_bitrate_and_slices() {
        // 1080p30 (coded 1920x1088): Level 4, 4.1 once the ceiling passes 12 Mbit/s
        assert_eq!(level_for(1920, 1088, 30.0, 9_000, 17), 120);
        assert_eq!(level_for(1920, 1088, 30.0, 15_000, 17), 123);
        // 2160p60: Level 5.1; 8K30: Level 6, 6.1 at the 8K preset's 150 Mbit/s ceiling
        assert_eq!(level_for(3840, 2160, 60.0, 0, 34), 153);
        assert_eq!(level_for(7680, 4320, 30.0, 0, 68), 180);
        assert_eq!(level_for(7680, 4320, 30.0, 149_300, 68), 186);
        assert_eq!(level_for(7680, 4320, 30.0, 99_533, 68), 183);
        // the picture side limit: 8192x128 fits Level 4's luma samples but not its sqrt(8 x MaxLumaPs)
        assert_eq!(level_for(8192, 128, 30.0, 0, 1), 150);
        // too many slices for Level 3.1
        assert_eq!(level_for(1280, 720, 30.0, 0, 41), 120);
        assert_eq!(level_for(1280, 720, 30.0, 0, 12), 93);
    }

    #[test]
    fn hdr_sei_payloads() {
        assert_eq!(sei_hdr(None, None), None);
        let s = sei_hdr(Some(&[7; 24]), Some((1000, 400))).unwrap_or_default();
        assert_eq!(s.len(), 2 + 24 + 2 + 4 + 1);
        assert_eq!(&s[..3], &[137, 24, 7]);
        assert_eq!(&s[26..], &[144, 4, 0x03, 0xe8, 0x01, 0x90, 0x80]);
        assert_eq!(sei_hdr(None, Some((1, 2))), Some(vec![144, 4, 0, 1, 0, 2, 0x80]));
    }
}
