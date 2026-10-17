// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared decoder state, translated from the structures of FFmpeg's `ac4dec.c` (see `NOTICE`).

use crate::mdct::ImdctSet;
use crate::tables::*;

/// Number of model bits of the SSF arithmetic decoder.
pub(crate) const SSF_MODEL_BITS: u32 = 15;
/// Model unit for the CDF specification.
pub(crate) const SSF_MODEL_UNIT: u32 = 1 << SSF_MODEL_BITS;
/// Number of range bits.
pub(crate) const SSF_RANGE_BITS: u32 = 30;
/// Half of the range unit.
pub(crate) const SSF_THRESHOLD_LARGE: u32 = 1 << (SSF_RANGE_BITS - 1);
/// Quarter of the range unit.
pub(crate) const SSF_THRESHOLD_SMALL: u32 = 1 << (SSF_RANGE_BITS - 2);

pub(crate) const CHANNEL_MODE_NB_CHANNELS: [u8; 17] =
    [1, 2, 3, 5, 6, 7, 8, 7, 8, 7, 8, 11, 12, 13, 14, 24, 0];

/// State of the SSF arithmetic decoder (`ACState`).
#[derive(Clone, Copy, Default)]
pub(crate) struct AcState {
    pub(crate) ui_low: u32,
    pub(crate) ui_range: u32,
    pub(crate) ui_offset: u32,
    pub(crate) ui_offset2: u32,
    pub(crate) ui_threshold_small: u32,
    pub(crate) ui_threshold_large: u32,
    pub(crate) ui_model_unit: u32,
    pub(crate) ui_range_bits: u32,
    pub(crate) ui_model_bits: u32,
}

// The C enumerations are kept as integer constants: the bitstream syntax uses them both as
// values and as table indices.

pub(crate) const ST_SUBSTREAM: i32 = 0;
pub(crate) const ST_PRESENTATION: i32 = 1;

pub(crate) const SM_LEVEL: i32 = 0;
pub(crate) const SM_BALANCE: i32 = 1;

pub(crate) const DT_SIGNAL: i32 = 0;
pub(crate) const DT_NOISE: i32 = 1;

pub(crate) const SF_ASF: i32 = 0;

/// Huffman codebook types (`HCBType`).
pub(crate) const F0: i32 = 0;
pub(crate) const DF: i32 = 1;
pub(crate) const DT: i32 = 2;

pub(crate) const CM_SIMPLE: i32 = 0;
pub(crate) const CM_ASPX: i32 = 1;
pub(crate) const CM_ASPX_ACPL_1: i32 = 2;
pub(crate) const CM_ASPX_ACPL_2: i32 = 3;
pub(crate) const CM_ASPX_ACPL_3: i32 = 4;

pub(crate) const FIXFIX: i32 = 0;
pub(crate) const FIXVAR: i32 = 1;
pub(crate) const VARFIX: i32 = 2;
pub(crate) const VARVAR: i32 = 3;

pub(crate) const LONG_STRIDE: i32 = 0;
pub(crate) const SHORT_STRIDE: i32 = 1;

pub(crate) const ACPL_FULL: i32 = 0;
pub(crate) const ACPL_PARTIAL: i32 = 1;

/// `ACPLDataType`.
pub(crate) const ALPHA1: usize = 0;
pub(crate) const ALPHA2: usize = 1;
pub(crate) const BETA1: usize = 2;
pub(crate) const BETA2: usize = 3;
pub(crate) const BETA3: usize = 4;
pub(crate) const GAMMA1: usize = 5;
pub(crate) const GAMMA2: usize = 6;
pub(crate) const GAMMA3: usize = 7;
pub(crate) const GAMMA4: usize = 8;
pub(crate) const GAMMA5: usize = 9;
pub(crate) const GAMMA6: usize = 10;

/// Per-channel window/scale-factor layout shared across a channel pair.
#[derive(Clone, Default)]
pub(crate) struct Scp {
    pub(crate) long_frame: u8,
    pub(crate) transf_length_idx: [u8; 2],
    pub(crate) transf_length: [i32; 2],

    pub(crate) different_framing: u8,
    pub(crate) max_sfb_side: [u8; 2],
    pub(crate) max_sfb: [u8; 2],
    pub(crate) scale_factor_grouping: [u8; 16],

    pub(crate) num_windows: u8,
    pub(crate) num_window_groups: u8,
    pub(crate) window_to_group: [u8; 16],
    pub(crate) num_win_in_group: [u8; 16],

    pub(crate) dual_maxsfb: u8,
    pub(crate) side_limited: u8,
    pub(crate) side_channel: u8,
}

/// A single decoded channel (FFmpeg's `SubstreamChannel`).
#[derive(Clone)]
pub(crate) struct Ssc {
    pub(crate) scp: Scp,

    pub(crate) master_reset: i32,
    pub(crate) num_sbg_master: i32,
    pub(crate) num_sb_aspx: i32,
    pub(crate) num_sbg_noise: i32,
    pub(crate) num_sbg_sig_highres: i32,
    pub(crate) num_sbg_sig_lowres: i32,
    pub(crate) sba: i32,
    pub(crate) sbx: i32,
    pub(crate) sbz: i32,

    pub(crate) sap_mode: i32,

    pub(crate) n_prev: i32,

    pub(crate) ms_used: [[u8; 128]; 16],
    pub(crate) sap_coeff_used: [[u8; 128]; 16],
    pub(crate) dpcm_alpha_q: [[i32; 128]; 16],

    pub(crate) delta_code_time: i32,

    pub(crate) num_sec_lsf: [i32; 16],
    pub(crate) num_sec: [i32; 16],
    pub(crate) sfb_cb: [[u8; 128]; 16],
    pub(crate) sect_cb: [[u8; 128]; 16],
    pub(crate) sect_start: [[i32; 128]; 16],
    pub(crate) sect_end: [[i32; 128]; 16],
    pub(crate) sect_sfb_offset: [[i32; 128]; 16],

    pub(crate) quant_spec: [i16; 2048],
    pub(crate) scaled_spec: [f32; 2048],
    pub(crate) spec_reord: [f32; 2048],
    pub(crate) offset2sfb: [i16; 2048],
    pub(crate) offset2g: [u8; 2048],
    pub(crate) win_offset: [i32; 16],
    pub(crate) overlap: [f32; 4096],

    pub(crate) max_quant_idx: [[i32; 128]; 16],
    pub(crate) dpcm_sf: [[i32; 128]; 16],
    pub(crate) dpcm_snf: [[i32; 128]; 16],
    pub(crate) snf_data_exists: i32,

    pub(crate) sf_gain: [[f32; 128]; 16],

    pub(crate) aspx_int_class: i32,
    pub(crate) aspx_num_noise: i32,
    pub(crate) aspx_num_noise_prev: i32,
    pub(crate) aspx_num_rel_left: i32,
    pub(crate) aspx_num_rel_right: i32,
    pub(crate) aspx_num_env: i32,
    pub(crate) aspx_num_env_prev: i32,
    pub(crate) aspx_freq_res: [i32; 5],
    pub(crate) aspx_var_bord_left: i32,
    pub(crate) aspx_var_bord_right: i32,
    pub(crate) aspx_rel_bord_left: [i32; 4],
    pub(crate) aspx_rel_bord_right: [i32; 4],
    pub(crate) aspx_tsg_ptr: i32,
    pub(crate) aspx_tsg_ptr_prev: i32,

    pub(crate) aspx_qmode_env: i32,
    pub(crate) aspx_sig_delta_dir: [i32; 8],
    pub(crate) aspx_noise_delta_dir: [i32; 2],
    pub(crate) aspx_tna_mode: [i32; 16],
    pub(crate) aspx_tna_mode_prev: [i32; 16],
    pub(crate) aspx_add_harmonic: [i32; 24],
    pub(crate) aspx_fic_used_in_sfb: [i32; 24],
    pub(crate) aspx_tic_used_in_slot: [i32; 16],
    pub(crate) aspx_xover_subband_offset: i32,
    pub(crate) aspx_balance: i32,

    pub(crate) atsg_freqres: [u8; 5],
    pub(crate) atsg_freqres_prev: [u8; 5],
    pub(crate) atsg_sig: [i32; 6],
    pub(crate) atsg_noise: [i32; 3],
    pub(crate) previous_stop_pos: i32,

    pub(crate) sbg_noise: [i32; 6],
    pub(crate) sbg_sig_lowres: [i32; 24],
    pub(crate) sbg_sig_highres: [i32; 24],
    pub(crate) sbg_lim: [i32; 32],
    pub(crate) sbg_patches: [i32; 6],
    pub(crate) sbg_patch_num_sb: [i32; 6],
    pub(crate) sbg_patch_start_sb: [i32; 6],
    pub(crate) sbg_master: [i32; 24],

    pub(crate) num_sbg_sig: [i32; 5],
    pub(crate) sbg_sig: [[i32; 24]; 5],
    pub(crate) num_sbg_patches: i32,
    pub(crate) num_sbg_lim: i32,

    pub(crate) aspx_data: [[[i32; 64]; 5]; 2],

    pub(crate) qscf_prev: [[i32; 64]; 5],
    pub(crate) qscf_noise_prev: [[i32; 64]; 2],
    pub(crate) qscf_sig_sbg: [[i32; 64]; 5],
    pub(crate) qscf_sig_sbg_prev: [[i32; 64]; 5],
    pub(crate) qscf_noise_sbg: [[i32; 64]; 2],
    pub(crate) scf_noise_sbg: [[f32; 64]; 2],
    pub(crate) scf_sig_sbg: [[f32; 64]; 5],
    pub(crate) scf_sig_sb: [[f32; 64]; 5],
    pub(crate) scf_noise_sb: [[f32; 64]; 5],

    pub(crate) gain_vec: [f32; 64],
    pub(crate) chirp_arr: [f32; 6],
    pub(crate) chirp_arr_prev: [f32; 6],
    pub(crate) est_sig_sb: [[f32; 64]; 5],
    pub(crate) sine_idx_sb: [[f32; 64]; 5],
    pub(crate) sine_idx_sb_prev: [[f32; 64]; 5],
    pub(crate) sine_area_sb: [[f32; 64]; 5],
    pub(crate) sine_lev_sb: [[f32; 64]; 5],
    pub(crate) noise_lev_sb: [[f32; 64]; 5],
    pub(crate) sig_gain_sb: [[f32; 64]; 5],
    pub(crate) max_sig_gain_sbg: [[f32; 64]; 5],
    pub(crate) max_sig_gain_sb: [[f32; 64]; 5],
    pub(crate) noise_lev_sb_lim: [[f32; 64]; 5],
    pub(crate) sig_gain_sb_lim: [[f32; 64]; 5],
    pub(crate) boost_fact_sbg: [[f32; 64]; 5],
    pub(crate) boost_fact_sb: [[f32; 64]; 5],
    pub(crate) sig_gain_sb_adj: [[f32; 64]; 5],
    pub(crate) noise_lev_sb_adj: [[f32; 64]; 5],
    pub(crate) sine_lev_sb_adj: [[f32; 64]; 5],

    pub(crate) sine_idx_prev: [[i8; 64]; 42],
    pub(crate) noise_idx_prev: [[i16; 64]; 42],

    pub(crate) pcm: [f32; 2048],

    pub(crate) qmf_filt: [f32; 640],
    pub(crate) qsyn_filt: [f32; 1280],
    pub(crate) q: Box<[[[f32; 64]; 42]; 2]>,
    pub(crate) q_prev: Box<[[[f32; 64]; 42]; 2]>,
    pub(crate) q_low: Box<[[[f32; 64]; 42]; 2]>,
    pub(crate) q_low_prev: Box<[[[f32; 64]; 42]; 2]>,
    pub(crate) q_low_ext: Box<[[[f32; 64]; 42]; 2]>,
    pub(crate) q_high: Box<[[[f32; 64]; 42]; 2]>,
    pub(crate) cov: [[[[f32; 2]; 3]; 3]; 64],
    pub(crate) alpha0: [[f32; 2]; 64],
    pub(crate) alpha1: [[f32; 2]; 64],
    pub(crate) y: Box<[[[f32; 64]; 42]; 2]>,
    pub(crate) y_prev: Box<[[[f32; 64]; 42]; 2]>,
    pub(crate) qmf_sine: Box<[[[f32; 64]; 42]; 2]>,
    pub(crate) qmf_noise: Box<[[[f32; 64]; 42]; 2]>,

    pub(crate) acpl_interpolation_type: i32,
    pub(crate) acpl_num_param_sets_cod: i32,
    pub(crate) acpl_param_timeslot: [i32; 2],
    pub(crate) acpl_data: [[i32; 16]; 11],

    pub(crate) start_block: i32,
    pub(crate) end_block: i32,
    pub(crate) stride_flag: i32,
    pub(crate) num_bands: i32,
    pub(crate) predictor_presence: [i32; 4],
    pub(crate) predictor_lag_delta: [i32; 4],
    pub(crate) predictor_lag: [i32; 4],
    pub(crate) variance_preserving: [i32; 4],
    pub(crate) alloc_offset: [i32; 4],
    pub(crate) delta: [i32; 4],
    pub(crate) gain_bits: [i32; 4],
    pub(crate) env_idx: [i32; 19],
    pub(crate) acs: AcState,
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
            aspx_add_harmonic: [0; 24],
            aspx_fic_used_in_sfb: [0; 24],
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
            gain_vec: [0.0; 64],
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
            q: Box::new([[[0.0; 64]; 42]; 2]),
            q_prev: Box::new([[[0.0; 64]; 42]; 2]),
            q_low: Box::new([[[0.0; 64]; 42]; 2]),
            q_low_prev: Box::new([[[0.0; 64]; 42]; 2]),
            q_low_ext: Box::new([[[0.0; 64]; 42]; 2]),
            q_high: Box::new([[[0.0; 64]; 42]; 2]),
            cov: [[[[0.0; 2]; 3]; 3]; 64],
            alpha0: [[0.0; 2]; 64],
            alpha1: [[0.0; 2]; 64],
            y: Box::new([[[0.0; 64]; 42]; 2]),
            y_prev: Box::new([[[0.0; 64]; 42]; 2]),
            qmf_sine: Box::new([[[0.0; 64]; 42]; 2]),
            qmf_noise: Box::new([[[0.0; 64]; 42]; 2]),
            acpl_interpolation_type: 0,
            acpl_num_param_sets_cod: 0,
            acpl_param_timeslot: [0; 2],
            acpl_data: [[0; 16]; 11],
            start_block: 0,
            end_block: 0,
            stride_flag: 0,
            num_bands: 0,
            predictor_presence: [0; 4],
            predictor_lag_delta: [0; 4],
            predictor_lag: [0; 4],
            variance_preserving: [0; 4],
            alloc_offset: [0; 4],
            delta: [0; 4],
            gain_bits: [0; 4],
            env_idx: [0; 19],
            acs: AcState::default(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Substream {
    pub(crate) codec_mode: i32,

    pub(crate) aspx_quant_mode_env: i32,
    pub(crate) aspx_start_freq: i32,
    pub(crate) prev_aspx_start_freq: i32,
    pub(crate) aspx_stop_freq: i32,
    pub(crate) prev_aspx_stop_freq: i32,
    pub(crate) aspx_master_freq_scale: i32,
    pub(crate) prev_aspx_master_freq_scale: i32,
    pub(crate) aspx_interpolation: i32,
    pub(crate) aspx_preflat: i32,
    pub(crate) aspx_limiter: i32,
    pub(crate) aspx_noise_sbg: i32,
    pub(crate) aspx_num_env_bits_fixfix: i32,
    pub(crate) aspx_freq_res_mode: i32,

    pub(crate) mode_2ch: u8,
    pub(crate) chel_matsel: u8,

    pub(crate) compand_on: [u8; 5],
    pub(crate) compand_avg: i32,

    pub(crate) coding_config: u8,
    pub(crate) mdct_stereo_proc: [u8; 2],
    pub(crate) matrix_stereo: Box<[[[[f32; 2]; 2]; 128]; 16]>,
    pub(crate) alpha_q: [[f32; 128]; 16],

    pub(crate) spec_frontend_l: i32,
    pub(crate) spec_frontend_r: i32,
    pub(crate) spec_frontend_m: i32,
    pub(crate) spec_frontend_s: i32,

    pub(crate) acpl_qmf_band: i32,
    pub(crate) acpl_param_band: i32,
    pub(crate) acpl_num_param_bands_id: i32,
    pub(crate) acpl_quant_mode: [i32; 2],
    pub(crate) max_sfb_master: i32,

    pub(crate) ssch: Vec<Ssc>,
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
            matrix_stereo: Box::new([[[[0.0; 2]; 2]; 128]; 16]),
            alpha_q: [[0.0; 128]; 16],
            spec_frontend_l: 0,
            spec_frontend_r: 0,
            spec_frontend_m: 0,
            spec_frontend_s: 0,
            acpl_qmf_band: 0,
            acpl_param_band: 0,
            acpl_num_param_bands_id: 0,
            acpl_quant_mode: [0; 2],
            max_sfb_master: 0,
            ssch: vec![Ssc::default(); 9],
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct SubstreamInfo {
    pub(crate) sus_ver: i32,
    pub(crate) channel_mode: i32,
    pub(crate) substream_index: i32,
    pub(crate) hsf_ext_substream_index: i32,
    pub(crate) sf_multiplier: i32,
    pub(crate) bitrate_indicator: i32,
    pub(crate) add_ch_base: i32,
    pub(crate) iframe: [i32; 4],
    pub(crate) back_channels_present: i32,
    pub(crate) centre_present: i32,
    pub(crate) top_channels_present: i32,
    pub(crate) meta: Metadata,
}

#[derive(Clone, Default)]
pub(crate) struct SubstreamGroupInfo {
    pub(crate) channel_coded: i32,
    pub(crate) group_index: i32,
    pub(crate) ssinfo: SubstreamInfo,
}

#[derive(Clone, Default)]
pub(crate) struct EmdfInfo {
    pub(crate) version: i32,
    pub(crate) key_id: i32,
    pub(crate) substream_index: i32,
}

#[derive(Clone, Default)]
pub(crate) struct PresentationSubstreamInfo {
    pub(crate) alternative: i32,
    pub(crate) pres_ndot: i32,
    pub(crate) substream_index: i32,
}

#[derive(Clone)]
pub(crate) struct PresentationInfo {
    pub(crate) single_substream: i32,
    pub(crate) enable_presentation: i32,
    pub(crate) presentation_config: i32,
    pub(crate) presentation_version: i32,
    pub(crate) add_emdf_substreams: i32,
    pub(crate) n_add_emdf_substreams: i32,
    pub(crate) n_substream_groups: i32,
    pub(crate) mdcompat: i32,
    pub(crate) presentation_id: i32,
    pub(crate) multiplier: i32,
    pub(crate) multiplier_bit: i32,
    pub(crate) pre_virtualized: i32,
    pub(crate) frame_rate_factor: i32,
    pub(crate) frame_rate_fraction: i32,
    pub(crate) multi_pid: i32,
    pub(crate) hsf_ext: i32,
    pub(crate) emdf: Vec<EmdfInfo>,
    pub(crate) psinfo: PresentationSubstreamInfo,
    pub(crate) ssinfo: SubstreamInfo,
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

#[derive(Clone, Default)]
pub(crate) struct Metadata {
    pub(crate) dialnorm_bits: i32,
    pub(crate) loudness_version: i32,
    pub(crate) loud_prac_type: i32,
    pub(crate) dialgate_prac_type: i32,
    pub(crate) loudcorr_type: i32,
    pub(crate) loudrelgat: i32,
    pub(crate) loudspchgat: i32,
    pub(crate) loudstrm3s: i32,
    pub(crate) max_loudstrm3s: i32,
    pub(crate) truepk: i32,
    pub(crate) max_truepk: i32,
    pub(crate) prgmbndy: i32,
    pub(crate) end_or_start: i32,
    pub(crate) prgmbndy_offset: i32,
    pub(crate) lra: i32,
    pub(crate) lra_prac_type: i32,
    pub(crate) loudmntry: i32,
    pub(crate) max_loudmntry: i32,
    pub(crate) drc_decoder_nr_modes: i32,
    pub(crate) drc_eac3_profile: i32,
    pub(crate) pre_dmixtyp_2ch: i32,
    pub(crate) phase90_info_2ch: i32,
    pub(crate) loro_center_mixgain: i32,
    pub(crate) loro_surround_mixgain: i32,
    pub(crate) loro_dmx_loud_corr: i32,
    pub(crate) ltrt_center_mixgain: i32,
    pub(crate) ltrt_surround_mixgain: i32,
    pub(crate) ltrt_dmx_loud_corr: i32,
    pub(crate) lfe_mixgain: i32,
    pub(crate) preferred_dmx_method: i32,
    pub(crate) pre_dmixtyp_5ch: i32,
    pub(crate) pre_upmixtyp_5ch: i32,
    pub(crate) pre_upmixtyp_3_4: i32,
    pub(crate) pre_upmixtyp_3_2_2: i32,
    pub(crate) phase90_info_mc: i32,
    pub(crate) surround_attenuation_known: i32,
    pub(crate) lfe_attenuation_known: i32,
    pub(crate) dc_block_on: i32,
}

/// AC-4 decoder: persistent state plus the per-frame parsing state (`AC4DecodeContext`).
pub(crate) struct Ac4Core {
    pub(crate) target_presentation: i32,

    pub(crate) version: i32,
    pub(crate) sequence_counter: i32,
    pub(crate) sequence_counter_prev: i32,
    pub(crate) wait_frames: i32,
    pub(crate) nb_wait_frames: i32,
    pub(crate) fs_index: i32,
    pub(crate) frame_rate_index: i32,
    pub(crate) frame_len_base: i32,
    pub(crate) frame_len_base_idx: i32,
    pub(crate) num_qmf_timeslots: i32,
    pub(crate) num_aspx_timeslots: i32,
    pub(crate) num_ts_in_ats: i32,
    pub(crate) ts_offset_hfgen: i32,
    pub(crate) iframe_global: i32,
    pub(crate) first_frame: bool,
    pub(crate) have_iframe: bool,
    pub(crate) nb_presentations: i32,
    pub(crate) payload_base: i32,
    pub(crate) short_program_id: i32,
    pub(crate) nb_substreams: i32,
    pub(crate) total_groups: i32,
    pub(crate) substream_size: [i32; 32],
    pub(crate) substream_type: [i32; 32],

    pub(crate) winl: Box<[f32; 2048]>,
    pub(crate) winr: Box<[f32; 2048]>,

    /// Inverse MDCT kernels, indexed `[frame_len_base_idx][transf_length_idx]`.
    pub(crate) imdct: ImdctSet,
    /// KBD windows, indexed `[frame_len_base_idx][transf_length_idx][n]`.
    pub(crate) kbd_window: Box<[[[f32; 2048]; 5]; 8]>,
    pub(crate) quant_lut: Box<[f32; 8192]>,
    pub(crate) cos_atab: Box<[[f32; 128]; 64]>,
    pub(crate) sin_atab: Box<[[f32; 128]; 64]>,
    pub(crate) cos_stab: Box<[[f32; 64]; 128]>,
    pub(crate) sin_stab: Box<[[f32; 64]; 128]>,

    pub(crate) pinfo: Vec<PresentationInfo>,
    pub(crate) ssgroup: Vec<SubstreamGroupInfo>,
    pub(crate) substream: Box<Substream>,
}

/// Number of Bessel I0 series iterations (FFmpeg `BESSEL_I0_ITER`).
const BESSEL_I0_ITER: u32 = 50;

/// A Kaiser-Bessel derived window of length `n` (FFmpeg's `ff_kbd_window_init`).
fn kbd_window(out: &mut [f32], alpha: f32, n: usize) {
    let mut local_window = [0.0f64; 2048];
    let mut sum = 0.0f64;
    let x = f64::from(alpha) * std::f64::consts::PI / n as f64;
    let alpha2 = x * x;

    for i in 0..n {
        let tmp = (i * (n - i)) as f64 * alpha2;
        let mut bessel = 1.0f64;
        for j in (1..=BESSEL_I0_ITER).rev() {
            bessel = bessel * tmp / f64::from(j * j) + 1.0;
        }
        sum += bessel;
        local_window[i] = sum;
    }

    sum += 1.0;

    for i in 0..n {
        out[i] = (local_window[i] / sum).sqrt() as f32;
    }
}

/// Returns the five transform lengths of a `frame_len_base_idx` (`transf_length_48khz[idx]`).
pub(crate) fn transf_length_48khz(idx: usize) -> &'static [u16; 5] {
    match idx {
        0 => &TRANSF_LENGTH_48KHZ_2048,
        1 => &TRANSF_LENGTH_48KHZ_1920,
        2 => &TRANSF_LENGTH_48KHZ_1536,
        3 => &TRANSF_LENGTH_48KHZ_1024,
        4 => &TRANSF_LENGTH_48KHZ_960,
        5 => &TRANSF_LENGTH_48KHZ_768,
        6 => &TRANSF_LENGTH_48KHZ_512,
        _ => &TRANSF_LENGTH_48KHZ_384,
    }
}

impl Ac4Core {
    pub fn new() -> Self {
        let mut kbd = Box::new([[[0.0f32; 2048]; 5]; 8]);
        for j in 0..8 {
            for (i, &n_w) in transf_length_48khz(j).iter().enumerate() {
                kbd_window(&mut kbd[j][i], KBD_WINDOW_ALPHA[j][i], usize::from(n_w));
            }
        }

        let mut quant_lut = Box::new([0.0f32; 8192]);
        for (i, v) in quant_lut.iter_mut().enumerate() {
            *v = (i as f32).powf(4.0 / 3.0);
        }

        let mut cos_atab = Box::new([[0.0f32; 128]; 64]);
        let mut sin_atab = Box::new([[0.0f32; 128]; 64]);
        let mut cos_stab = Box::new([[0.0f32; 64]; 128]);
        let mut sin_stab = Box::new([[0.0f32; 64]; 128]);
        for i in 0..64 {
            for n in 0..128 {
                // FFmpeg evaluates the angle in double precision (`M_PI/128*...`) and rounds it
                // to float for `cosf`/`sinf`; the angles reach ~400 rad, so single precision
                // arithmetic would leak low band energy into the high bands.
                let th_a = (std::f64::consts::PI / 128.0
                    * (i as f64 + 0.5)
                    * (2 * n as i32 - 1) as f64) as f32;
                cos_atab[i][n] = th_a.cos();
                sin_atab[i][n] = th_a.sin();
                let th_s = (std::f64::consts::PI / 128.0
                    * (i as f64 + 0.5)
                    * (2 * n as i32 - 255) as f64) as f32;
                cos_stab[n][i] = th_s.cos() / 64.0;
                sin_stab[n][i] = th_s.sin() / 64.0;
            }
        }

        Ac4Core {
            target_presentation: 0,
            version: 0,
            sequence_counter: 0,
            sequence_counter_prev: 0,
            wait_frames: 0,
            nb_wait_frames: 0,
            fs_index: 0,
            frame_rate_index: 0,
            frame_len_base: 0,
            frame_len_base_idx: 0,
            num_qmf_timeslots: 0,
            num_aspx_timeslots: 0,
            num_ts_in_ats: 0,
            ts_offset_hfgen: 0,
            iframe_global: 0,
            first_frame: true,
            have_iframe: false,
            nb_presentations: 0,
            payload_base: 0,
            short_program_id: 0,
            nb_substreams: 0,
            total_groups: 0,
            substream_size: [0; 32],
            substream_type: [0; 32],
            winl: Box::new([0.0; 2048]),
            winr: Box::new([0.0; 2048]),
            imdct: ImdctSet::new(),
            kbd_window: kbd,
            quant_lut,
            cos_atab,
            sin_atab,
            cos_stab,
            sin_stab,
            pinfo: vec![PresentationInfo::default(); 8],
            ssgroup: vec![SubstreamGroupInfo::default(); 8],
            substream: Box::new(Substream::default()),
        }
    }
}

/// Describes the output of a successfully decoded frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameInfo {
    pub(crate) sample_rate: u32,
    pub(crate) channels: usize,
    pub(crate) channel_mode: i32,
    /// Samples per channel (`frame_len_base`).
    pub(crate) nb_samples: usize,
}
