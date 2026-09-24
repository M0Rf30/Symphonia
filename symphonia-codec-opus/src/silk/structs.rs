// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decoder-side state structs and constants. Ported from libopus `silk/structs.h` and
//! `silk/define.h` (BSD-3-Clause), see NOTICE. Encoder-only fields are omitted.

#![allow(dead_code)]

use crate::silk::tables;

// -------------------------------------------------------------------------------------------
// Constants (`silk/define.h`)
// -------------------------------------------------------------------------------------------

pub const MAX_FRAMES_PER_PACKET: usize = 3;
pub const DECODER_NUM_CHANNELS: usize = 2;

pub const MAX_FS_KHZ: i32 = 16;
pub const SUB_FRAME_LENGTH_MS: i32 = 5;
pub const MAX_NB_SUBFR: usize = 4;
pub const MAX_SUB_FRAME_LENGTH: usize = (SUB_FRAME_LENGTH_MS * MAX_FS_KHZ) as usize; // 80
pub const MAX_FRAME_LENGTH_MS: i32 = SUB_FRAME_LENGTH_MS * MAX_NB_SUBFR as i32; // 20
pub const MAX_FRAME_LENGTH: usize = (MAX_FRAME_LENGTH_MS * MAX_FS_KHZ) as usize; // 320
pub const LTP_MEM_LENGTH_MS: i32 = 20;
/// C: `LTP_MEM_LENGTH_MS * MAX_FS_KHZ` -- the largest possible `ltp_mem_length` (16 kHz).
pub const MAX_LTP_MEM_LENGTH: usize = (LTP_MEM_LENGTH_MS * MAX_FS_KHZ) as usize; // 320

pub const MAX_LPC_ORDER: usize = 16;
pub const MIN_LPC_ORDER: usize = 10;
pub const LTP_ORDER: usize = 5;
pub const NB_LTP_CBKS: usize = 3;

pub const SHELL_CODEC_FRAME_LENGTH: usize = 16;
pub const LOG2_SHELL_CODEC_FRAME_LENGTH: i32 = 4;
pub const MAX_NB_SHELL_BLOCKS: usize = MAX_FRAME_LENGTH / SHELL_CODEC_FRAME_LENGTH; // 20
pub const N_RATE_LEVELS: usize = 10;
pub const SILK_MAX_PULSES: i32 = 16;
pub const NSQ_LPC_BUF_LENGTH: usize = MAX_LPC_ORDER;

pub const N_LEVELS_QGAIN: i32 = 64;
pub const MIN_QGAIN_DB: i32 = 2;
pub const MAX_QGAIN_DB: i32 = 88;
pub const MAX_DELTA_GAIN_QUANT: i32 = 36;
pub const MIN_DELTA_GAIN_QUANT: i32 = -4;
pub const QUANT_LEVEL_ADJUST_Q10: i32 = 80;

pub const NLSF_QUANT_MAX_AMPLITUDE: i32 = 4;

pub const TYPE_NO_VOICE_ACTIVITY: i32 = 0;
pub const TYPE_UNVOICED: i32 = 1;
pub const TYPE_VOICED: i32 = 2;

pub const CODE_INDEPENDENTLY: i32 = 0;
pub const CODE_INDEPENDENTLY_NO_LTP_SCALING: i32 = 1;
pub const CODE_CONDITIONALLY: i32 = 2;

pub const STEREO_INTERP_LEN_MS: i32 = 8;

pub const BWE_AFTER_LOSS_Q16: i32 = 63570;

pub const CNG_BUF_MASK_MAX: i32 = 255;
pub const CNG_GAIN_SMTH_Q16: i32 = 4634;
pub const CNG_GAIN_SMTH_THRESHOLD_Q16: i32 = 46396;
pub const CNG_NLSF_SMTH_Q16: i32 = 16348;

// -------------------------------------------------------------------------------------------
// `silk_NLSF_CB_struct` (`silk/structs.h`)
// -------------------------------------------------------------------------------------------

/// C: `silk_NLSF_CB_struct`.
pub struct NlsfCbStruct {
    pub n_vectors: i16,
    pub order: i16,
    pub quant_step_size_q16: i16,
    pub inv_quant_step_size_q6: i16,
    pub cb1_nlsf_q8: &'static [u8],
    pub cb1_wght_q9: &'static [i16],
    pub cb1_icdf: &'static [u8],
    pub pred_q8: &'static [u8],
    pub ec_sel: &'static [u8],
    pub ec_icdf: &'static [u8],
    pub ec_rates_q5: &'static [u8],
    pub delta_min_q15: &'static [i16],
}

// -------------------------------------------------------------------------------------------
// `SideInfoIndices`
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct SideInfoIndices {
    pub gains_indices: [i8; MAX_NB_SUBFR],
    pub ltp_index: [i8; MAX_NB_SUBFR],
    pub nlsf_indices: [i8; MAX_LPC_ORDER + 1],
    pub lag_index: i16,
    pub contour_index: i8,
    pub signal_type: i8,
    pub quant_offset_type: i8,
    pub nlsf_interp_coef_q2: i8,
    pub per_index: i8,
    pub ltp_scale_index: i8,
    pub seed: i8,
}

// -------------------------------------------------------------------------------------------
// `silk_decoder_control`
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SilkDecoderControl {
    pub pitch_l: [i32; MAX_NB_SUBFR],
    pub gains_q16: [i32; MAX_NB_SUBFR],
    pub pred_coef_q12: [[i16; MAX_LPC_ORDER]; 2],
    pub ltp_coef_q14: [i16; LTP_ORDER * MAX_NB_SUBFR],
    pub ltp_scale_q14: i32,
}

impl Default for SilkDecoderControl {
    fn default() -> Self {
        SilkDecoderControl {
            pitch_l: [0; MAX_NB_SUBFR],
            gains_q16: [0; MAX_NB_SUBFR],
            pred_coef_q12: [[0; MAX_LPC_ORDER]; 2],
            ltp_coef_q14: [0; LTP_ORDER * MAX_NB_SUBFR],
            ltp_scale_q14: 0,
        }
    }
}

// -------------------------------------------------------------------------------------------
// `stereo_dec_state`
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct StereoDecState {
    pub pred_prev_q13: [i32; 2],
    pub s_mid: [i16; 2],
    pub s_side: [i16; 2],
}

// -------------------------------------------------------------------------------------------
// `silk_PLC_struct`
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SilkPlcState {
    pub pitch_l_q8: i32,
    pub ltp_coef_q14: [i16; LTP_ORDER],
    pub prev_lpc_q12: [i16; MAX_LPC_ORDER],
    pub rand_seed: i32,
    pub rand_scale_q14: i16,
    pub conc_energy: i32,
    pub conc_energy_shift: i32,
    pub prev_ltp_scale_q14: i16,
    pub prev_gain_q16: [i32; 2],
    pub fs_khz: i32,
    pub nb_subfr: i32,
    pub subfr_length: i32,
}

impl Default for SilkPlcState {
    fn default() -> Self {
        SilkPlcState {
            pitch_l_q8: 0,
            ltp_coef_q14: [0; LTP_ORDER],
            prev_lpc_q12: [0; MAX_LPC_ORDER],
            rand_seed: 0,
            rand_scale_q14: 0,
            conc_energy: 0,
            conc_energy_shift: 0,
            prev_ltp_scale_q14: 0,
            prev_gain_q16: [0; 2],
            fs_khz: 0,
            nb_subfr: 0,
            subfr_length: 0,
        }
    }
}

// -------------------------------------------------------------------------------------------
// `silk_CNG_struct`
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SilkCngState {
    pub cng_exc_buf_q14: [i32; MAX_FRAME_LENGTH],
    pub cng_smth_nlsf_q15: [i16; MAX_LPC_ORDER],
    pub cng_synth_state: [i32; MAX_LPC_ORDER],
    pub cng_smth_gain_q16: i32,
    pub rand_seed: i32,
    pub fs_khz: i32,
}

impl Default for SilkCngState {
    fn default() -> Self {
        SilkCngState {
            cng_exc_buf_q14: [0; MAX_FRAME_LENGTH],
            cng_smth_nlsf_q15: [0; MAX_LPC_ORDER],
            cng_synth_state: [0; MAX_LPC_ORDER],
            cng_smth_gain_q16: 0,
            rand_seed: 0,
            fs_khz: 0,
        }
    }
}

// -------------------------------------------------------------------------------------------
// `silk_decoder_state` (one per channel)
// -------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct SilkDecoderState {
    pub prev_gain_q16: i32,
    pub exc_q14: [i32; MAX_FRAME_LENGTH],
    pub s_lpc_q14_buf: [i32; MAX_LPC_ORDER],
    pub out_buf: [i16; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH],
    pub lag_prev: i32,
    pub last_gain_index: i8,
    pub fs_khz: i32,
    pub fs_api_hz: i32,
    pub nb_subfr: i32,
    pub frame_length: i32,
    pub subfr_length: i32,
    pub ltp_mem_length: i32,
    pub lpc_order: i32,
    pub prev_nlsf_q15: [i16; MAX_LPC_ORDER],
    pub first_frame_after_reset: bool,
    pub pitch_lag_low_bits_icdf: &'static [u8],
    pub pitch_contour_icdf: &'static [u8],

    pub n_frames_decoded: i32,
    pub n_frames_per_packet: i32,

    pub ec_prev_signal_type: i32,
    pub ec_prev_lag_index: i16,

    pub vad_flags: [bool; MAX_FRAMES_PER_PACKET],
    pub lbrr_flag: bool,
    pub lbrr_flags: [bool; MAX_FRAMES_PER_PACKET],

    pub resampler_state: crate::silk::resampler::SilkResamplerState,

    pub ps_nlsf_cb: &'static NlsfCbStruct,

    pub indices: SideInfoIndices,

    pub s_cng: SilkCngState,

    pub loss_cnt: i32,
    pub prev_signal_type: i32,
    pub arch: i32,

    pub s_plc: SilkPlcState,
}

impl SilkDecoderState {
    /// C: `silk_init_decoder`.
    pub fn new() -> Self {
        let mut s = SilkDecoderState {
            prev_gain_q16: 0,
            exc_q14: [0; MAX_FRAME_LENGTH],
            s_lpc_q14_buf: [0; MAX_LPC_ORDER],
            out_buf: [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH],
            lag_prev: 0,
            last_gain_index: 0,
            fs_khz: 0,
            fs_api_hz: 0,
            nb_subfr: 0,
            frame_length: 0,
            subfr_length: 0,
            ltp_mem_length: 0,
            lpc_order: 0,
            prev_nlsf_q15: [0; MAX_LPC_ORDER],
            first_frame_after_reset: false,
            pitch_lag_low_bits_icdf: tables::UNIFORM8_ICDF,
            pitch_contour_icdf: tables::PITCH_CONTOUR_NB_ICDF,
            n_frames_decoded: 0,
            n_frames_per_packet: 0,
            ec_prev_signal_type: 0,
            ec_prev_lag_index: 0,
            vad_flags: [false; MAX_FRAMES_PER_PACKET],
            lbrr_flag: false,
            lbrr_flags: [false; MAX_FRAMES_PER_PACKET],
            resampler_state: Default::default(),
            ps_nlsf_cb: &tables::SILK_NLSF_CB_NB_MB,
            indices: SideInfoIndices::default(),
            s_cng: SilkCngState::default(),
            loss_cnt: 0,
            prev_signal_type: TYPE_NO_VOICE_ACTIVITY,
            arch: 0,
            s_plc: SilkPlcState::default(),
        };
        s.reset();
        s
    }

    /// C: `silk_reset_decoder`. Resets everything after `SILK_DECODER_STATE_RESET_START`
    /// (`prev_gain_Q16` onward), i.e. everything except nothing here since this Rust struct
    /// only ever models the "reset" portion (there is no persistent pre-reset state in wave 1).
    pub fn reset(&mut self) {
        self.prev_gain_q16 = 65536;
        self.exc_q14 = [0; MAX_FRAME_LENGTH];
        self.s_lpc_q14_buf = [0; MAX_LPC_ORDER];
        self.out_buf = [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH];
        self.lag_prev = 0;
        self.last_gain_index = 0;
        self.fs_khz = 0;
        self.fs_api_hz = 0;
        self.lpc_order = 0;
        self.frame_length = 0;
        self.subfr_length = 0;
        self.ltp_mem_length = 0;
        self.prev_nlsf_q15 = [0; MAX_LPC_ORDER];
        self.first_frame_after_reset = true;
        self.n_frames_decoded = 0;
        self.n_frames_per_packet = 0;
        self.ec_prev_signal_type = 0;
        self.ec_prev_lag_index = 0;
        self.vad_flags = [false; MAX_FRAMES_PER_PACKET];
        self.lbrr_flag = false;
        self.lbrr_flags = [false; MAX_FRAMES_PER_PACKET];
        self.indices = SideInfoIndices::default();
        self.loss_cnt = 0;
        self.prev_signal_type = TYPE_NO_VOICE_ACTIVITY;
        crate::silk::cng::reset(self);
        crate::silk::plc::reset(self);
    }
}

impl Default for SilkDecoderState {
    fn default() -> Self {
        Self::new()
    }
}
