// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AC-4 (ETSI TS 103 190) decoder, following FFmpeg's `ac4dec.c` (see `NOTICE`).

use std::sync::LazyLock;

use symphonia_core::errors::{Result, unsupported_error};

use crate::bit::BitReader;
use crate::mdct::ImdctSet;
use crate::tables::*;
use crate::vlc::Vlc;

/// Number of model bits (unused in the supported subset, kept for completeness).
const _SSF_MODEL_BITS: u32 = 15;

const CHANNEL_MODE_NB_CHANNELS: [u8; 17] =
    [1, 2, 3, 5, 6, 7, 8, 7, 8, 7, 8, 11, 12, 13, 14, 24, 0];

#[derive(Clone, Copy, PartialEq, Eq)]
enum SubstreamType {
    Substream,
    Presentation,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StereoMode {
    Level,
    Balance,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DataType {
    Signal,
    Noise,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpectralFrontend {
    Asf,
    Ssf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HcbType {
    F0,
    Df,
    Dt,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CodecMode {
    Simple,
    Aspx,
    AspxAcpl1,
    AspxAcpl2,
    AspxAcpl3,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IntervalClass {
    Fixfix,
    Fixvar,
    Varfix,
    Varvar,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StrideFlag {
    LongStride,
    ShortStride,
}

/// Per-channel window/scale-factor layout shared across a channel pair.
#[derive(Clone)]
struct Scp {
    long_frame: u8,
    transf_length_idx: [u8; 2],
    transf_length: [i32; 2],

    different_framing: u8,
    max_sfb_side: [u8; 2],
    max_sfb: [u8; 2],
    scale_factor_grouping: [u8; 16],

    num_windows: u8,
    num_window_groups: u8,
    window_to_group: [u8; 16],
    num_win_in_group: [u8; 16],

    dual_maxsfb: u8,
    side_limited: u8,
    side_channel: u8,
}

impl Default for Scp {
    fn default() -> Self {
        Scp {
            long_frame: 0,
            transf_length_idx: [0; 2],
            transf_length: [0; 2],
            different_framing: 0,
            max_sfb_side: [0; 2],
            max_sfb: [0; 2],
            scale_factor_grouping: [0; 16],
            num_windows: 0,
            num_window_groups: 0,
            window_to_group: [0; 16],
            num_win_in_group: [0; 16],
            dual_maxsfb: 0,
            side_limited: 0,
            side_channel: 0,
        }
    }
}

/// A single decoded channel (FFmpeg's `SubstreamChannel`).
#[derive(Clone)]
struct Ssc {
    scp: Scp,

    master_reset: i32,
    num_sbg_master: i32,
    num_sb_aspx: i32,
    num_sbg_noise: i32,
    num_sbg_sig_highres: i32,
    num_sbg_sig_lowres: i32,
    sba: i32,
    sbx: i32,
    sbz: i32,

    sap_mode: i32,

    n_prev: i32,

    ms_used: [[u8; 128]; 16],
    sap_coeff_used: [[u8; 128]; 16],
    dpcm_alpha_q: [[i32; 128]; 16],

    delta_code_time: i32,

    num_sec_lsf: [i32; 16],
    num_sec: [i32; 16],
    sfb_cb: [[u8; 128]; 16],
    sect_cb: [[u8; 128]; 16],
    sect_start: [[i32; 128]; 16],
    sect_end: [[i32; 128]; 16],
    sect_sfb_offset: [[i32; 128]; 16],

    quant_spec: [i16; 2048],
    scaled_spec: [f32; 2048],
    spec_reord: [f32; 2048],
    offset2sfb: [i16; 2048],
    offset2g: [u8; 2048],
    win_offset: [i32; 16],
    overlap: [f32; 4096],

    max_quant_idx: [[i32; 128]; 16],
    dpcm_sf: [[i32; 128]; 16],
    dpcm_snf: [[i32; 128]; 16],
    snf_data_exists: i32,

    sf_gain: [[f32; 128]; 16],

    aspx_int_class: i32,
    aspx_num_noise: i32,
    aspx_num_noise_prev: i32,
    aspx_num_rel_left: i32,
    aspx_num_rel_right: i32,
    aspx_num_env: i32,
    aspx_num_env_prev: i32,
    aspx_freq_res: [i32; 5],
    aspx_var_bord_left: i32,
    aspx_var_bord_right: i32,
    aspx_rel_bord_left: [i32; 4],
    aspx_rel_bord_right: [i32; 4],
    aspx_tsg_ptr: i32,
    aspx_tsg_ptr_prev: i32,

    aspx_qmode_env: i32,
    aspx_sig_delta_dir: [i32; 8],
    aspx_noise_delta_dir: [i32; 2],
    aspx_tna_mode: [i32; 16],
    aspx_tna_mode_prev: [i32; 16],
    aspx_add_harmonic: [i32; 16],
    aspx_fic_used_in_sfb: [i32; 16],
    aspx_tic_used_in_slot: [i32; 16],
    aspx_xover_subband_offset: i32,
    aspx_balance: i32,

    atsg_freqres: [u8; 5],
    atsg_freqres_prev: [u8; 5],
    atsg_sig: [i32; 6],
    atsg_noise: [i32; 3],
    previous_stop_pos: i32,

    sbg_noise: [i32; 6],
    sbg_sig_lowres: [i32; 24],
    sbg_sig_highres: [i32; 24],
    sbg_lim: [i32; 32],
    sbg_patches: [i32; 6],
    sbg_patch_num_sb: [i32; 6],
    sbg_patch_start_sb: [i32; 6],
    sbg_master: [i32; 24],

    num_sbg_sig: [i32; 5],
    sbg_sig: [[i32; 24]; 5],
    num_sbg_patches: i32,
    num_sbg_lim: i32,

    aspx_data: [[[i32; 64]; 5]; 2],

    qscf_prev: [[i32; 64]; 5],
    qscf_noise_prev: [[i32; 64]; 2],
    qscf_sig_sbg: [[i32; 64]; 5],
    qscf_sig_sbg_prev: [[i32; 64]; 5],
    qscf_noise_sbg: [[i32; 64]; 2],
    scf_noise_sbg: [[f32; 64]; 2],
    scf_sig_sbg: [[f32; 64]; 5],
    scf_sig_sb: [[f32; 64]; 5],
    scf_noise_sb: [[f32; 64]; 5],

    gain_vec: [f32; 32],
    chirp_arr: [f32; 6],
    chirp_arr_prev: [f32; 6],
    est_sig_sb: [[f32; 64]; 5],
    sine_idx_sb: [[f32; 64]; 5],
    sine_idx_sb_prev: [[f32; 64]; 5],
    sine_area_sb: [[f32; 64]; 5],
    sine_lev_sb: [[f32; 64]; 5],
    noise_lev_sb: [[f32; 64]; 5],
    sig_gain_sb: [[f32; 64]; 5],
    max_sig_gain_sbg: [[f32; 64]; 5],
    max_sig_gain_sb: [[f32; 64]; 5],
    noise_lev_sb_lim: [[f32; 64]; 5],
    sig_gain_sb_lim: [[f32; 64]; 5],
    boost_fact_sbg: [[f32; 64]; 5],
    boost_fact_sb: [[f32; 64]; 5],
    sig_gain_sb_adj: [[f32; 64]; 5],
    noise_lev_sb_adj: [[f32; 64]; 5],
    sine_lev_sb_adj: [[f32; 64]; 5],

    sine_idx_prev: [[i8; 64]; 42],
    noise_idx_prev: [[i16; 64]; 42],

    pcm: [f32; 2048],

    qmf_filt: [f32; 640],
    qsyn_filt: [f32; 1280],
    q: [[[f32; 64]; 42]; 2],
    q_prev: [[[f32; 64]; 42]; 2],
    q_low: [[[f32; 64]; 42]; 2],
    q_low_prev: [[[f32; 64]; 42]; 2],
    q_low_ext: [[[f32; 64]; 42]; 2],
    q_high: [[[f32; 64]; 42]; 2],
    cov: [[[[f32; 2]; 3]; 3]; 64],
    alpha0: [[f32; 2]; 64],
    alpha1: [[f32; 2]; 64],
    y: [[[f32; 64]; 42]; 2],
    y_prev: [[[f32; 64]; 42]; 2],
    qmf_sine: [[[f32; 64]; 42]; 2],
    qmf_noise: [[[f32; 64]; 42]; 2],
}

impl Default for Ssc {
    fn default() -> Self {
        Ssc {
            scp: Scp::default(),
            master_reset: 0,
            num_sbg_master: 0,
            num_sb_aspx: 0,
            num_sbg_noise: 0,
            num_sbg_sig_highres: 0,
            num_sbg_sig_lowres: 0,
            sba: 0,
            sbx: 0,
            sbz: 0,
            sap_mode: 0,
            n_prev: 0,
            ms_used: [[0; 128]; 16],
            sap_coeff_used: [[0; 128]; 16],
            dpcm_alpha_q: [[0; 128]; 16],
            delta_code_time: 0,
            num_sec_lsf: [0; 16],
            num_sec: [0; 16],
            sfb_cb: [[0; 128]; 16],
            sect_cb: [[0; 128]; 16],
            sect_start: [[0; 128]; 16],
            sect_end: [[0; 128]; 16],
            sect_sfb_offset: [[0; 128]; 16],
            quant_spec: [0; 2048],
            scaled_spec: [0.0; 2048],
            spec_reord: [0.0; 2048],
            offset2sfb: [0; 2048],
            offset2g: [0; 2048],
            win_offset: [0; 16],
            overlap: [0.0; 4096],
            max_quant_idx: [[0; 128]; 16],
            dpcm_sf: [[0; 128]; 16],
            dpcm_snf: [[0; 128]; 16],
            snf_data_exists: 0,
            sf_gain: [[0.0; 128]; 16],
            aspx_int_class: 0,
            aspx_num_noise: 0,
            aspx_num_noise_prev: 0,
            aspx_num_rel_left: 0,
            aspx_num_rel_right: 0,
            aspx_num_env: 0,
            aspx_num_env_prev: 0,
            aspx_freq_res: [0; 5],
            aspx_var_bord_left: 0,
            aspx_var_bord_right: 0,
            aspx_rel_bord_left: [0; 4],
            aspx_rel_bord_right: [0; 4],
            aspx_tsg_ptr: 0,
            aspx_tsg_ptr_prev: 0,
            aspx_qmode_env: 0,
            aspx_sig_delta_dir: [0; 8],
            aspx_noise_delta_dir: [0; 2],
            aspx_tna_mode: [0; 16],
            aspx_tna_mode_prev: [0; 16],
            aspx_add_harmonic: [0; 16],
            aspx_fic_used_in_sfb: [0; 16],
            aspx_tic_used_in_slot: [0; 16],
            aspx_xover_subband_offset: 0,
            aspx_balance: 0,
            atsg_freqres: [0; 5],
            atsg_freqres_prev: [0; 5],
            atsg_sig: [0; 6],
            atsg_noise: [0; 3],
            previous_stop_pos: 0,
            sbg_noise: [0; 6],
            sbg_sig_lowres: [0; 24],
            sbg_sig_highres: [0; 24],
            sbg_lim: [0; 32],
            sbg_patches: [0; 6],
            sbg_patch_num_sb: [0; 6],
            sbg_patch_start_sb: [0; 6],
            sbg_master: [0; 24],
            num_sbg_sig: [0; 5],
            sbg_sig: [[0; 24]; 5],
            num_sbg_patches: 0,
            num_sbg_lim: 0,
            aspx_data: [[[0; 64]; 5]; 2],
            qscf_prev: [[0; 64]; 5],
            qscf_noise_prev: [[0; 64]; 2],
            qscf_sig_sbg: [[0; 64]; 5],
            qscf_sig_sbg_prev: [[0; 64]; 5],
            qscf_noise_sbg: [[0; 64]; 2],
            scf_noise_sbg: [[0.0; 64]; 2],
            scf_sig_sbg: [[0.0; 64]; 5],
            scf_sig_sb: [[0.0; 64]; 5],
            scf_noise_sb: [[0.0; 64]; 5],
            gain_vec: [0.0; 32],
            chirp_arr: [0.0; 6],
            chirp_arr_prev: [0.0; 6],
            est_sig_sb: [[0.0; 64]; 5],
            sine_idx_sb: [[0.0; 64]; 5],
            sine_idx_sb_prev: [[0.0; 64]; 5],
            sine_area_sb: [[0.0; 64]; 5],
            sine_lev_sb: [[0.0; 64]; 5],
            noise_lev_sb: [[0.0; 64]; 5],
            sig_gain_sb: [[0.0; 64]; 5],
            max_sig_gain_sbg: [[0.0; 64]; 5],
            max_sig_gain_sb: [[0.0; 64]; 5],
            noise_lev_sb_lim: [[0.0; 64]; 5],
            sig_gain_sb_lim: [[0.0; 64]; 5],
            boost_fact_sbg: [[0.0; 64]; 5],
            boost_fact_sb: [[0.0; 64]; 5],
            sig_gain_sb_adj: [[0.0; 64]; 5],
            noise_lev_sb_adj: [[0.0; 64]; 5],
            sine_lev_sb_adj: [[0.0; 64]; 5],
            sine_idx_prev: [[0; 64]; 42],
            noise_idx_prev: [[0; 64]; 42],
            pcm: [0.0; 2048],
            qmf_filt: [0.0; 640],
            qsyn_filt: [0.0; 1280],
            q: [[[0.0; 64]; 42]; 2],
            q_prev: [[[0.0; 64]; 42]; 2],
            q_low: [[[0.0; 64]; 42]; 2],
            q_low_prev: [[[0.0; 64]; 42]; 2],
            q_low_ext: [[[0.0; 64]; 42]; 2],
            q_high: [[[0.0; 64]; 42]; 2],
            cov: [[[[0.0; 2]; 3]; 3]; 64],
            alpha0: [[0.0; 2]; 64],
            alpha1: [[0.0; 2]; 64],
            y: [[[0.0; 64]; 42]; 2],
            y_prev: [[[0.0; 64]; 42]; 2],
            qmf_sine: [[[0.0; 64]; 42]; 2],
            qmf_noise: [[[0.0; 64]; 42]; 2],
        }
    }
}

struct Substream {
    codec_mode: i32,

    aspx_quant_mode_env: i32,
    aspx_start_freq: i32,
    prev_aspx_start_freq: i32,
    aspx_stop_freq: i32,
    prev_aspx_stop_freq: i32,
    aspx_master_freq_scale: i32,
    prev_aspx_master_freq_scale: i32,
    aspx_interpolation: i32,
    aspx_preflat: i32,
    aspx_limiter: i32,
    aspx_noise_sbg: i32,
    aspx_num_env_bits_fixfix: i32,
    aspx_freq_res_mode: i32,

    mode_2ch: u8,
    chel_matsel: u8,

    compand_on: [u8; 5],
    compand_avg: i32,

    coding_config: u8,
    mdct_stereo_proc: [u8; 2],
    matrix_stereo: [[[[f32; 2]; 2]; 128]; 16],
    alpha_q: [[f32; 128]; 16],

    spec_frontend_l: i32,
    spec_frontend_r: i32,

    ssch: Vec<Ssc>,
}

impl Default for Substream {
    fn default() -> Self {
        Substream {
            codec_mode: 0,
            aspx_quant_mode_env: 0,
            aspx_start_freq: 0,
            prev_aspx_start_freq: 0,
            aspx_stop_freq: 0,
            prev_aspx_stop_freq: 0,
            aspx_master_freq_scale: 0,
            prev_aspx_master_freq_scale: 0,
            aspx_interpolation: 0,
            aspx_preflat: 0,
            aspx_limiter: 0,
            aspx_noise_sbg: 0,
            aspx_num_env_bits_fixfix: 0,
            aspx_freq_res_mode: 0,
            mode_2ch: 0,
            chel_matsel: 0,
            compand_on: [0; 5],
            compand_avg: 0,
            coding_config: 0,
            mdct_stereo_proc: [0; 2],
            matrix_stereo: [[[[0.0; 2]; 2]; 128]; 16],
            alpha_q: [[0.0; 128]; 16],
            spec_frontend_l: 0,
            spec_frontend_r: 0,
            ssch: vec![Ssc::default(); 9],
        }
    }
}

struct SubstreamInfo {
    sus_ver: i32,
    channel_mode: i32,
    substream_index: i32,
    hsf_ext_substream_index: i32,
    sf_multiplier: i32,
    bitrate_indicator: i32,
    add_ch_base: i32,
    iframe: [i32; 4],
}

impl Default for SubstreamInfo {
    fn default() -> Self {
        SubstreamInfo {
            sus_ver: 0,
            channel_mode: 0,
            substream_index: 0,
            hsf_ext_substream_index: 0,
            sf_multiplier: 0,
            bitrate_indicator: 0,
            add_ch_base: 0,
            iframe: [0; 4],
        }
    }
}

struct SubstreamGroupInfo {
    channel_coded: i32,
    group_index: i32,
    ssinfo: SubstreamInfo,
}

impl Default for SubstreamGroupInfo {
    fn default() -> Self {
        SubstreamGroupInfo { channel_coded: 0, group_index: 0, ssinfo: SubstreamInfo::default() }
    }
}

struct EmdfInfo {
    version: i32,
    key_id: i32,
    substream_index: i32,
}

impl Default for EmdfInfo {
    fn default() -> Self {
        EmdfInfo { version: 0, key_id: 0, substream_index: 0 }
    }
}

struct PresentationSubstreamInfo {
    alternative: i32,
    pres_ndot: i32,
    substream_index: i32,
}

impl Default for PresentationSubstreamInfo {
    fn default() -> Self {
        PresentationSubstreamInfo { alternative: 0, pres_ndot: 0, substream_index: 0 }
    }
}

struct PresentationInfo {
    single_substream: i32,
    enable_presentation: i32,
    presentation_config: i32,
    presentation_version: i32,
    add_emdf_substreams: i32,
    n_add_emdf_substreams: i32,
    n_substream_groups: i32,
    mdcompat: i32,
    presentation_id: i32,
    multiplier: i32,
    multiplier_bit: i32,
    pre_virtualized: i32,
    frame_rate_factor: i32,
    frame_rate_fraction: i32,
    multi_pid: i32,
    hsf_ext: i32,
    emdf: Vec<EmdfInfo>,
    psinfo: PresentationSubstreamInfo,
    ssinfo: SubstreamInfo,
}

impl Default for PresentationInfo {
    fn default() -> Self {
        PresentationInfo {
            single_substream: 0,
            enable_presentation: 0,
            presentation_config: 0,
            presentation_version: 0,
            add_emdf_substreams: 0,
            n_add_emdf_substreams: 0,
            n_substream_groups: 0,
            mdcompat: 0,
            presentation_id: 0,
            multiplier: 0,
            multiplier_bit: 0,
            pre_virtualized: 0,
            frame_rate_factor: 0,
            frame_rate_fraction: 0,
            multi_pid: 0,
            hsf_ext: 0,
            emdf: vec![EmdfInfo::default(); 32],
            psinfo: PresentationSubstreamInfo::default(),
            ssinfo: SubstreamInfo::default(),
        }
    }
}

struct Metadata {
    dialnorm_bits: i32,
    loudness_version: i32,
    loud_prac_type: i32,
    dialgate_prac_type: i32,
    loudcorr_type: i32,
    loudrelgat: i32,
    loudspchgat: i32,
    loudstrm3s: i32,
    max_loudstrm3s: i32,
    truepk: i32,
    max_truepk: i32,
    prgmbndy: i32,
    end_or_start: i32,
    prgmbndy_offset: i32,
    lra: i32,
    lra_prac_type: i32,
    loudmntry: i32,
    max_loudmntry: i32,
    drc_decoder_nr_modes: i32,
    drc_eac3_profile: i32,
    pre_dmixtyp_2ch: i32,
    phase90_info_2ch: i32,
    loro_center_mixgain: i32,
    loro_surround_mixgain: i32,
    loro_dmx_loud_corr: i32,
    ltrt_center_mixgain: i32,
    ltrt_surround_mixgain: i32,
    ltrt_dmx_loud_corr: i32,
    lfe_mixgain: i32,
    preferred_dmx_method: i32,
    pre_dmixtyp_5ch: i32,
    pre_upmixtyp_5ch: i32,
    pre_upmixtyp_3_4: i32,
    pre_upmixtyp_3_2_2: i32,
    phase90_info_mc: i32,
    surround_attenuation_known: i32,
    lfe_attenuation_known: i32,
    dc_block_on: i32,
}

impl Default for Metadata {
    fn default() -> Self {
        Metadata {
            dialnorm_bits: 0,
            loudness_version: 0,
            loud_prac_type: 0,
            dialgate_prac_type: 0,
            loudcorr_type: 0,
            loudrelgat: 0,
            loudspchgat: 0,
            loudstrm3s: 0,
            max_loudstrm3s: 0,
            truepk: 0,
            max_truepk: 0,
            prgmbndy: 0,
            end_or_start: 0,
            prgmbndy_offset: 0,
            lra: 0,
            lra_prac_type: 0,
            loudmntry: 0,
            max_loudmntry: 0,
            drc_decoder_nr_modes: 0,
            drc_eac3_profile: 0,
            pre_dmixtyp_2ch: 0,
            phase90_info_2ch: 0,
            loro_center_mixgain: 0,
            loro_surround_mixgain: 0,
            loro_dmx_loud_corr: 0,
            ltrt_center_mixgain: 0,
            ltrt_surround_mixgain: 0,
            ltrt_dmx_loud_corr: 0,
            lfe_mixgain: 0,
            preferred_dmx_method: 0,
            pre_dmixtyp_5ch: 0,
            pre_upmixtyp_5ch: 0,
            pre_upmixtyp_3_4: 0,
            pre_upmixtyp_3_2_2: 0,
            phase90_info_mc: 0,
            surround_attenuation_known: 0,
            lfe_attenuation_known: 0,
            dc_block_on: 0,
        }
    }
}

// =====================================================================================
// Static/persistent decoder state.
// =====================================================================================

/// AC-4 decoder. All state that must persist across frames lives here; per-frame parsing
/// state lives in `Frame`.
pub struct Ac4Decoder {
    /// Sample rate index: 0 = 44100, 1 = 48000.
    fs_index: u32,
    /// Number of samples per channel in a frame (`frame_len_base`).
    frame_len_base: usize,
    /// Index of `frame_len_base` into the 8-entry 48 kHz table family.
    frame_len_base_idx: usize,
    /// Number of QMF time slots in a frame.
    num_qmf_timeslots: usize,
    /// Number of A-SPX time slots in a frame.
    num_aspx_timeslots: usize,
    /// Number of time slots per A-SPX time slot (2 for 1536..2048, else 1).
    num_ts_in_ats: usize,
    /// High-frequency generation delay in QMF slots.
    ts_offset_hfgen: usize,

    /// Sequence number of the previous frame (for `check_sequence`).
    sequence_counter_prev: i32,
    /// Whether an I-frame has been seen yet (gates output).
    have_iframe: bool,

    /// Current presentation (fixed at 0; multi-presentation streams pick presentation 0).
    target_presentation: usize,

    /// Static transforms / windows / quantization tables.
    init: Init,
}

/// Precomputed, frame-size-independent tables.
struct Init {
    mdct: ImdctSet,
    /// KBD windows: `[frame_len_base_idx][transf_length_idx][2048]`.
    kbd_window: Box<[[[f32; 2048]; 5]; 8]>,
    quant_lut: Box<[f32; 8192]>,
    /// `cos_atab[i][n]`, `sin_atab[i][n]` for `i in 0..64`, `n in 0..128`.
    cos_atab: Box<[[f32; 128]; 64]>,
    sin_atab: Box<[[f32; 128]; 64]>,
    /// `cos_stab[n][i]`, `sin_stab[n][i]` for `n in 0..128`, `i in 0..64`.
    cos_stab: Box<[[f32; 64]; 128]>,
    sin_stab: Box<[[f32; 64]; 128]>,
}

impl Init {
    fn new() -> Self {
        let kbd_window = Box::new([[[0.0f32; 2048]; 5]; 8]);
        let mut this = Init {
            mdct: ImdctSet::default(),
            kbd_window,
            quant_lut: Box::new([0.0f32; 8192]),
            cos_atab: Box::new([[0.0f32; 128]; 64]),
            sin_atab: Box::new([[0.0f32; 128]; 64]),
            cos_stab: Box::new([[0.0f32; 64]; 128]),
            sin_stab: Box::new([[0.0f32; 64]; 128]),
        };

        for i in 0..8192 {
            this.quant_lut[i] = (i as f32).powf(4.0 / 3.0);
        }

        for i in 0..64 {
            for n in 0..128 {
                let th = std::f32::consts::PI / 128.0 * (i as f32 + 0.5) * (2.0 * n as f32 - 1.0);
                this.cos_atab[i][n] = th.cos();
                this.sin_atab[i][n] = th.sin();
            }
        }
        for n in 0..128 {
            for i in 0..64 {
                let th = std::f32::consts::PI / 128.0 * (i as f32 + 0.5) * (2.0 * n as f32 - 255.0);
                this.cos_stab[n][i] = th.cos() / 64.0;
                this.sin_stab[n][i] = th.sin() / 64.0;
            }
        }

        // KBD windows for every (frame_len_base_idx, transf_length_idx) pair.
        for j in 0..8 {
            let transf = transf_lengths_48khz(j);
            for (i, &n_w) in transf.iter().enumerate() {
                kbd_window(&mut this.kbd_window[j][i], KBD_WINDOW_ALPHA[j][i], n_w);
            }
        }

        this
    }
}

/// A Kaiser-Bessel derived window (FFmpeg's `ff_kbd_window_init`).
fn kbd_window(out: &mut [f32], alpha: f32, n: usize) {
    let alpha2 = (alpha * std::f32::consts::PI / n as f32) * (alpha * std::f32::consts::PI / n as f32);
    let mut sum = 0.0f64;
    let mut local_sum = [0.0f64; 2048];
    for i in 0..n {
        let tmp = i as f64 * (n - i) as f64 * alpha2 as f64;
        let mut bessel = 1.0f64;
        for j in 1..=50 {
            bessel = bessel * tmp / (j * j) as f64 + 1.0;
        }
        sum += bessel;
        local_sum[i] = sum;
    }
    sum += 1.0;
    for i in 0..n {
        out[i] = ((local_sum[i] / sum) as f32).sqrt();
    }
}

/// Per-frame decoding state (mirrors `AC4DecodeContext` plus the `Substream` scratch).
struct Frame<'a> {
    data: &'a [u8],
    gb: BitReader<'a>,
    version: i32,
    sequence_counter: i32,
    iframe_global: i32,
    nb_presentations: i32,
    payload_base: usize,
    nb_substreams: i32,
    substream_size: [i32; 32],
    substream_type: [i32; 32],
    short_program_id: i32,
    total_groups: i32,
    pinfo: [PresentationInfo; 8],
    ssgroup: [SubstreamGroupInfo; 8],
    substream: Substream,
    first_frame: bool,
}

impl<'a> Frame<'a> {
    fn new(data: &'a [u8]) -> Self {
        Frame {
            data,
            gb: BitReader::new(data),
            version: 0,
            sequence_counter: 0,
            iframe_global: 0,
            nb_presentations: 0,
            payload_base: 0,
            nb_substreams: 0,
            substream_size: [0; 32],
            substream_type: [0; 32],
            short_program_id: 0,
            total_groups: 0,
            pinfo: std::array::from_fn(|_| PresentationInfo::default()),
            ssgroup: std::array::from_fn(|_| SubstreamGroupInfo::default()),
            substream: Substream::default(),
            first_frame: false,
        }
    }
}

// =====================================================================================
// VLC codebook construction (built once, shared).
// =====================================================================================

/// A lazily-built codebook registry. Each entry is built exactly once.
static VLC_REGISTRY: LazyLock<VlcRegistry> = LazyLock::new(build_vlc_registry);

struct VlcRegistry {
    channel_mode: Vlc,
    bitrate_indicator: Vlc,
    scale_factors: Vlc,
    snf: Vlc,
    asf: Box<[Vlc; 11]>,
    aspx_int_class: Vlc,
    aspx_signal: Box<[[[Vlc; 3]; 2]; 2]>,
    aspx_noise: Box<[[Vlc; 3]; 2]>,
}

fn build_vlc_registry() -> VlcRegistry {
        let asf = Box::new([
            Vlc::new(&ASF_CODEBOOK_1_BITS, &ASF_CODEBOOK_1_CODES),
            Vlc::new(&ASF_CODEBOOK_2_BITS, &ASF_CODEBOOK_2_CODES),
            Vlc::new(&ASF_CODEBOOK_3_BITS, &ASF_CODEBOOK_3_CODES),
            Vlc::new(&ASF_CODEBOOK_4_BITS, &ASF_CODEBOOK_4_CODES),
            Vlc::new(&ASF_CODEBOOK_5_BITS, &ASF_CODEBOOK_5_CODES),
            Vlc::new(&ASF_CODEBOOK_6_BITS, &ASF_CODEBOOK_6_CODES),
            Vlc::new(&ASF_CODEBOOK_7_BITS, &ASF_CODEBOOK_7_CODES),
            Vlc::new(&ASF_CODEBOOK_8_BITS, &ASF_CODEBOOK_8_CODES),
            Vlc::new(&ASF_CODEBOOK_9_BITS, &ASF_CODEBOOK_9_CODES),
            Vlc::new(&ASF_CODEBOOK_10_BITS, &ASF_CODEBOOK_10_CODES),
            Vlc::new(&ASF_CODEBOOK_11_BITS, &ASF_CODEBOOK_11_CODES),
        ]);

        let aspx_signal = Box::new([
            [
                [
                    Vlc::new(&ASPX_HCB_ENV_LEVEL_15_F0_BITS, &ASPX_HCB_ENV_LEVEL_15_F0_CODES),
                    Vlc::new(&ASPX_HCB_ENV_LEVEL_15_DF_BITS, &ASPX_HCB_ENV_LEVEL_15_DF_CODES),
                    Vlc::new(&ASPX_HCB_ENV_LEVEL_15_DT_BITS, &ASPX_HCB_ENV_LEVEL_15_DT_CODES),
                ],
                [
                    Vlc::new(&ASPX_HCB_ENV_LEVEL_30_F0_BITS, &ASPX_HCB_ENV_LEVEL_30_F0_CODES),
                    Vlc::new(&ASPX_HCB_ENV_LEVEL_30_DF_BITS, &ASPX_HCB_ENV_LEVEL_30_DF_CODES),
                    Vlc::new(&ASPX_HCB_ENV_LEVEL_30_DT_BITS, &ASPX_HCB_ENV_LEVEL_30_DT_CODES),
                ],
            ],
            [
                [
                    Vlc::new(&ASPX_HCB_ENV_BALANCE_15_F0_BITS, &ASPX_HCB_ENV_BALANCE_15_F0_CODES),
                    Vlc::new(&ASPX_HCB_ENV_BALANCE_15_DF_BITS, &ASPX_HCB_ENV_BALANCE_15_DF_CODES),
                    Vlc::new(&ASPX_HCB_ENV_BALANCE_15_DT_BITS, &ASPX_HCB_ENV_BALANCE_15_DT_CODES),
                ],
                [
                    Vlc::new(&ASPX_HCB_ENV_BALANCE_30_F0_BITS, &ASPX_HCB_ENV_BALANCE_30_F0_CODES),
                    Vlc::new(&ASPX_HCB_ENV_BALANCE_30_DF_BITS, &ASPX_HCB_ENV_BALANCE_30_DF_CODES),
                    Vlc::new(&ASPX_HCB_ENV_BALANCE_30_DT_BITS, &ASPX_HCB_ENV_BALANCE_30_DT_CODES),
                ],
            ],
        ]);

        let aspx_noise = Box::new([
            [
                Vlc::new(&ASPX_HCB_NOISE_LEVEL_F0_BITS, &ASPX_HCB_NOISE_LEVEL_F0_CODES),
                Vlc::new(&ASPX_HCB_NOISE_LEVEL_DF_BITS, &ASPX_HCB_NOISE_LEVEL_DF_CODES),
                Vlc::new(&ASPX_HCB_NOISE_LEVEL_DT_BITS, &ASPX_HCB_NOISE_LEVEL_DT_CODES),
            ],
            [
                Vlc::new(&ASPX_HCB_NOISE_BALANCE_F0_BITS, &ASPX_HCB_NOISE_BALANCE_F0_CODES),
                Vlc::new(&ASPX_HCB_NOISE_BALANCE_DF_BITS, &ASPX_HCB_NOISE_BALANCE_DF_CODES),
                Vlc::new(&ASPX_HCB_NOISE_BALANCE_DT_BITS, &ASPX_HCB_NOISE_BALANCE_DT_CODES),
            ],
        ]);

        VlcRegistry {
            channel_mode: Vlc::new(&CHANNEL_MODE_BITS, &CHANNEL_MODE_CODES),
            bitrate_indicator: Vlc::new(&BITRATE_INDICATOR_BITS, &BITRATE_INDICATOR_CODES),
            scale_factors: Vlc::new(&SCALE_FACTORS_BITS, &SCALE_FACTORS_CODES),
            snf: Vlc::new(&SNF_BITS, &SNF_CODES),
            asf,
            aspx_int_class: Vlc::new(&ASPX_INT_CLASS_BITS, &ASPX_INT_CLASS_CODES),
            aspx_signal,
            aspx_noise,
        }
}

/// Resolves the A-SPX signal codebook for `(stereo_mode, quant_mode, hcb_type)`.
fn aspx_signal_hcb(stereo_mode: usize, quant_mode: usize, hcb: HcbType) -> &'static Vlc {
    &VLC_REGISTRY.aspx_signal[stereo_mode][quant_mode][hcb as usize]
}

/// Resolves the A-SPX noise codebook for `(stereo_mode, hcb_type)`.
fn aspx_noise_hcb(stereo_mode: usize, hcb: HcbType) -> &'static Vlc {
    &VLC_REGISTRY.aspx_noise[stereo_mode][hcb as usize]
}

/// Resolves the A-SPX signal codebook offset.
fn aspx_signal_off(stereo_mode: usize, quant_mode: usize, hcb: HcbType) -> i32 {
    ASPX_CODEBOOK_SIGNAL_OFF[stereo_mode][quant_mode][hcb as usize] as i32
}

/// Resolves the A-SPX noise codebook offset.
fn aspx_noise_off(stereo_mode: usize, hcb: HcbType) -> i32 {
    ASPX_CODEBOOK_NOISE_OFF[stereo_mode][hcb as usize] as i32
}

// =====================================================================================
// Table indirection helpers.
// =====================================================================================

/// The 5 transform lengths for a `frame_len_base_idx` (0..7).
fn transf_lengths_48khz(idx: usize) -> &'static [u16; 5] {
    match idx {
        0 => &TRANSF_LENGTH_48KHZ_2048,
        1 => &TRANSF_LENGTH_48KHZ_1920,
        2 => &TRANSF_LENGTH_48KHZ_1536,
        3 => &TRANSF_LENGTH_48KHZ_1024,
        4 => &TRANSF_LENGTH_48KHZ_960,
        5 => &TRANSF_LENGTH_48KHZ_768,
        6 => &TRANSF_LENGTH_48KHZ_512,
        7 => &TRANSF_LENGTH_48KHZ_384,
        _ => unreachable!(),
    }
}

fn get_transfer_length_from_idx(frame_len_base: usize, idx: usize) -> usize {
    let tab: &'static [u16; 5] = match frame_len_base {
        2048 => &TRANSF_LENGTH_48KHZ_2048,
        1920 => &TRANSF_LENGTH_48KHZ_1920,
        1536 => &TRANSF_LENGTH_48KHZ_1536,
        1024 => &TRANSF_LENGTH_48KHZ_1024,
        960 => &TRANSF_LENGTH_48KHZ_960,
        768 => &TRANSF_LENGTH_48KHZ_768,
        512 => &TRANSF_LENGTH_48KHZ_512,
        384 => &TRANSF_LENGTH_48KHZ_384,
        _ => unreachable!(),
    };
    tab[idx] as usize
}

fn get_sfb_offset(transf_length: usize) -> &'static [u16] {
    match transf_length {
        2048 => &SFB_OFFSET_48KHZ_2048,
        1920 => &SFB_OFFSET_48KHZ_1920,
        1536 => &SFB_OFFSET_48KHZ_1536,
        1024 => &SFB_OFFSET_48KHZ_1024,
        960 => &SFB_OFFSET_48KHZ_960,
        768 => &SFB_OFFSET_48KHZ_768,
        512 => &SFB_OFFSET_48KHZ_512,
        480 => &SFB_OFFSET_48KHZ_480,
        384 => &SFB_OFFSET_48KHZ_384,
        256 => &SFB_OFFSET_48KHZ_256,
        240 => &SFB_OFFSET_48KHZ_240,
        192 => &SFB_OFFSET_48KHZ_192,
        128 => &SFB_OFFSET_48KHZ_128,
        120 => &SFB_OFFSET_48KHZ_120,
        96 => &SFB_OFFSET_48KHZ_96,
        _ => unreachable!(),
    }
}

fn num_sfb_48(transf_length: usize) -> usize {
    match transf_length {
        2048 => 63,
        1920 => 61,
        1536 => 55,
        1024 | 960 => 49,
        768 => 43,
        512 | 480 => 36,
        384 => 33,
        256 | 240 => 20,
        192 => 18,
        128 | 120 => 14,
        96 => 12,
        _ => unreachable!(),
    }
}

// =====================================================================================
// Top-level decode.
// =====================================================================================

impl Ac4Decoder {
    pub fn new() -> Self {
        Ac4Decoder {
            fs_index: 0,
            frame_len_base: 0,
            frame_len_base_idx: 0,
            num_qmf_timeslots: 0,
            num_aspx_timeslots: 0,
            num_ts_in_ats: 0,
            ts_offset_hfgen: 0,
            sequence_counter_prev: 0,
            have_iframe: false,
            target_presentation: 0,
            init: Init::new(),
        }
    }

    /// Decodes one AC-4 frame (either a raw frame or a sync-wrapped 0xAC40/0xAC41 frame).
    /// Returns interleaved `f32` PCM (one `frame_len_base` sample per channel), or an empty
    /// vector when the frame produces no output (before the first I-frame).
    ///
    /// NOTE: the frame decoding pipeline is not yet wired up; this entry point is a placeholder.
    pub fn decode_frame(&mut self, _data: &[u8]) -> Result<Vec<f32>> {
        unsupported_error("ac4: frame decode not yet implemented")
    }
}
