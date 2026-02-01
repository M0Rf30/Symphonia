// SILK Decoder for Opus
// Ported from reference implementation
// SPDX-License-Identifier: MPL-2.0

use crate::entdec::RangeDecoder;
use std::cmp;

// Type alias for compatibility
type EntropyCoder<'a> = RangeDecoder<'a>;

// Constants from define.h
const MAX_FRAMES_PER_PACKET: usize = 3;
const MAX_NB_SUBFR: usize = 4;
const MAX_LPC_ORDER: usize = 16;
const MAX_FRAME_LENGTH: usize = 320; // 20ms at 16kHz
const MAX_SUB_FRAME_LENGTH: usize = 80; // 5ms at 16kHz
const LTP_ORDER: usize = 5;
const SHELL_CODEC_FRAME_LENGTH: usize = 16;
const LOG2_SHELL_CODEC_FRAME_LENGTH: usize = 4;
const N_RATE_LEVELS: usize = 10;
const SILK_MAX_PULSES: usize = 16;
const MAX_NB_SHELL_BLOCKS: usize = MAX_FRAME_LENGTH / SHELL_CODEC_FRAME_LENGTH;
const NLSF_QUANT_MAX_AMPLITUDE: i32 = 4;
const NLSF_QUANT_LEVEL_ADJ: f32 = 0.1;
const QUANT_LEVEL_ADJUST_Q10: i32 = 80;
const BWE_AFTER_LOSS_Q16: i32 = 63570;

// Signal types
const TYPE_NO_VOICE_ACTIVITY: i8 = 0;
const TYPE_UNVOICED: i8 = 1;
const TYPE_VOICED: i8 = 2;

// Conditional coding
const CODE_INDEPENDENTLY: i32 = 0;
const CODE_CONDITIONALLY: i32 = 2;

// Quantization offsets (Q10 format)
const OFFSET_VL_Q10: i32 = 32;
const OFFSET_VH_Q10: i32 = 100;
const OFFSET_UVL_Q10: i32 = 100;
const OFFSET_UVH_Q10: i32 = 240;

// NLSF Codebook structure
#[derive(Clone)]
pub struct NlsfCodebook {
    n_vectors: i16,
    order: i16,
    quant_step_size_q16: i16,
    inv_quant_step_size_q6: i16,
    cb1_nlsf_q8: &'static [u8],
    cb1_wght_q9: &'static [i16],
    cb1_icdf: &'static [u8],
    pred_q8: &'static [u8],
    ec_sel: &'static [u8],
    ec_icdf: &'static [u8],
    delta_min_q15: &'static [i16],
}

// Side information indices
#[derive(Default, Clone)]
struct SideInfoIndices {
    gains_indices: [i8; MAX_NB_SUBFR],
    ltp_index: [i8; MAX_NB_SUBFR],
    nlsf_indices: [i8; MAX_LPC_ORDER + 1],
    lag_index: i16,
    contour_index: i8,
    signal_type: i8,
    quant_offset_type: i8,
    nlsf_interp_coef_q2: i8,
    per_index: i8,
    ltp_scale_index: i8,
    seed: i8,
}

// Decoder control structure
struct SilkDecoderControl {
    pitch_l: [i32; MAX_NB_SUBFR],
    gains_q16: [i32; MAX_NB_SUBFR],
    pred_coef_q12: [[i16; MAX_LPC_ORDER]; 2],
    ltp_coef_q14: [i16; LTP_ORDER * MAX_NB_SUBFR],
    ltp_scale_q14: i32,
}

impl Default for SilkDecoderControl {
    fn default() -> Self {
        Self {
            pitch_l: [0; MAX_NB_SUBFR],
            gains_q16: [0; MAX_NB_SUBFR],
            pred_coef_q12: [[0; MAX_LPC_ORDER]; 2],
            ltp_coef_q14: [0; LTP_ORDER * MAX_NB_SUBFR],
            ltp_scale_q14: 0,
        }
    }
}

// Main SILK decoder state
pub struct SilkDecoder {
    // Decoder state
    prev_gain_q16: i32,
    exc_q14: [i32; MAX_FRAME_LENGTH],
    slpc_q14_buf: [i32; MAX_LPC_ORDER],
    out_buf: [i16; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH],
    lag_prev: i32,
    last_gain_index: i8,

    // Configuration
    fs_khz: i32,
    fs_api_hz: i32,
    nb_subfr: usize,
    frame_length: usize,
    subfr_length: usize,
    ltp_mem_length: usize,
    lpc_order: usize,

    // NLSF state
    prev_nlsf_q15: [i16; MAX_LPC_ORDER],
    first_frame_after_reset: bool,

    // Frame tracking
    n_frames_decoded: i32,
    n_frames_per_packet: i32,

    // Entropy coding state
    ec_prev_signal_type: i32,
    ec_prev_lag_index: i16,

    // Loss concealment
    loss_cnt: i32,
    prev_signal_type: i8,

    // Quantization indices
    indices: SideInfoIndices,

    // NLSF codebook pointer
    nlsf_cb: Option<&'static NlsfCodebook>,
}

impl SilkDecoder {
    /// Create a new SILK decoder
    pub fn new(sample_rate: u32) -> Self {
        let fs_khz = (sample_rate / 1000) as i32;
        let nb_subfr = if fs_khz == 8 { 2 } else { 4 };
        let frame_length = (fs_khz as usize) * 20; // 20ms frame
        let subfr_length = frame_length / nb_subfr;
        let ltp_mem_length = (fs_khz as usize) * 20; // LTP_MEM_LENGTH_MS = 20
        let lpc_order = if fs_khz <= 12 { 10 } else { 16 };

        Self {
            prev_gain_q16: 65536,
            exc_q14: [0; MAX_FRAME_LENGTH],
            slpc_q14_buf: [0; MAX_LPC_ORDER],
            out_buf: [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH],
            lag_prev: 0,
            last_gain_index: 0,
            fs_khz,
            fs_api_hz: sample_rate as i32,
            nb_subfr,
            frame_length,
            subfr_length,
            ltp_mem_length,
            lpc_order,
            prev_nlsf_q15: [0; MAX_LPC_ORDER],
            first_frame_after_reset: true,
            n_frames_decoded: 0,
            n_frames_per_packet: 1,
            ec_prev_signal_type: 0,
            ec_prev_lag_index: 0,
            loss_cnt: 0,
            prev_signal_type: TYPE_NO_VOICE_ACTIVITY,
            indices: SideInfoIndices::default(),
            nlsf_cb: None,
        }
    }

    /// Reset decoder state
    pub fn reset(&mut self) {
        self.prev_gain_q16 = 65536;
        self.exc_q14 = [0; MAX_FRAME_LENGTH];
        self.slpc_q14_buf = [0; MAX_LPC_ORDER];
        self.out_buf = [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH];
        self.lag_prev = 0;
        self.first_frame_after_reset = true;
        self.loss_cnt = 0;
    }

    /// Decode a SILK frame
    pub fn decode_frame(
        &mut self,
        ec: &mut EntropyCoder,
        output: &mut [i16],
        lost_flag: bool,
        cond_coding: i32,
    ) -> Result<usize, &'static str> {
        let mut dec_ctrl = SilkDecoderControl::default();

        if !lost_flag {
            // Allocate pulse buffer
            let mut pulses = vec![0i16; self.frame_length];

            // Decode quantization indices
            self.decode_indices(ec, 0, false, cond_coding)?;

            // Decode pulse signal
            self.decode_pulses(ec, &mut pulses)?;

            // Decode parameters
            self.decode_parameters(&mut dec_ctrl, cond_coding)?;

            // Run inverse NSQ (synthesis)
            self.decode_core(&dec_ctrl, output, &pulses)?;

            // Update output buffer
            let mv_len = self.ltp_mem_length - self.frame_length;
            self.out_buf.copy_within(self.frame_length..self.frame_length + mv_len, 0);
            self.out_buf[mv_len..mv_len + self.frame_length].copy_from_slice(&output[..self.frame_length]);

            self.loss_cnt = 0;
            self.prev_signal_type = self.indices.signal_type;
            self.first_frame_after_reset = false;
        } else {
            // Packet loss concealment (basic)
            output[..self.frame_length].fill(0);
            self.loss_cnt += 1;
        }

        self.lag_prev = dec_ctrl.pitch_l[self.nb_subfr - 1];

        Ok(self.frame_length)
    }

    /// Decode side information indices
    fn decode_indices(
        &mut self,
        ec: &mut EntropyCoder,
        _frame_index: i32,
        _decode_lbrr: bool,
        cond_coding: i32,
    ) -> Result<(), &'static str> {
        // Decode signal type and quantizer offset
        let ix = ec.decode_icdf(&SILK_TYPE_OFFSET_NO_VAD_ICDF, 8);
        self.indices.signal_type = (ix >> 1) as i8;
        self.indices.quant_offset_type = (ix & 1) as i8;

        // Decode gains - first subframe
        if cond_coding == CODE_CONDITIONALLY {
            self.indices.gains_indices[0] = ec.decode_icdf(&SILK_DELTA_GAIN_ICDF, 8) as i8;
        } else {
            let msb = ec.decode_icdf(&SILK_GAIN_ICDF[self.indices.signal_type as usize], 8);
            let lsb = ec.decode_icdf(&SILK_UNIFORM8_ICDF, 8);
            self.indices.gains_indices[0] = ((msb << 3) + lsb) as i8;
        }

        // Remaining subframes
        for i in 1..self.nb_subfr {
            self.indices.gains_indices[i] = ec.decode_icdf(&SILK_DELTA_GAIN_ICDF, 8) as i8;
        }

        // Decode NLSF indices (simplified)
        self.indices.nlsf_indices[0] = 0; // Placeholder
        for i in 1..=self.lpc_order {
            self.indices.nlsf_indices[i] = 0; // Placeholder
        }

        // NLSF interpolation factor
        if self.nb_subfr == MAX_NB_SUBFR {
            self.indices.nlsf_interp_coef_q2 = ec.decode_icdf(&SILK_NLSF_INTERPOLATION_FACTOR_ICDF, 8) as i8;
        } else {
            self.indices.nlsf_interp_coef_q2 = 4;
        }

        // Decode pitch parameters for voiced frames
        if self.indices.signal_type == TYPE_VOICED {
            // Decode lag index (simplified)
            let lag_base = ec.decode_icdf(&SILK_PITCH_LAG_ICDF, 8) as i32;
            self.indices.lag_index = (lag_base * (self.fs_khz / 2)) as i16;
            self.ec_prev_lag_index = self.indices.lag_index;

            // Contour index
            self.indices.contour_index = ec.decode_icdf(&SILK_PITCH_CONTOUR_ICDF, 8) as i8;

            // PER index
            self.indices.per_index = ec.decode_icdf(&SILK_LTP_PER_INDEX_ICDF, 8) as i8;

            // LTP indices
            for k in 0..self.nb_subfr {
                self.indices.ltp_index[k] = ec.decode_icdf(&SILK_LTP_GAIN_ICDF_0, 8) as i8;
            }

            // LTP scaling
            if cond_coding == CODE_INDEPENDENTLY {
                self.indices.ltp_scale_index = ec.decode_icdf(&SILK_LTPSCALE_ICDF, 8) as i8;
            } else {
                self.indices.ltp_scale_index = 0;
            }
        }

        self.ec_prev_signal_type = self.indices.signal_type as i32;

        // Decode seed
        self.indices.seed = ec.decode_icdf(&SILK_UNIFORM4_ICDF, 8) as i8;

        Ok(())
    }

    /// Decode pulse/excitation signal
    fn decode_pulses(&mut self, ec: &mut EntropyCoder, pulses: &mut [i16]) -> Result<(), &'static str> {
        let _signal_type = self.indices.signal_type;

        // Decode rate level
        let rate_level = ec.decode_icdf(&SILK_RATE_LEVELS_ICDF[0], 8) as usize;

        // Calculate number of shell blocks
        let mut iter = self.frame_length >> LOG2_SHELL_CODEC_FRAME_LENGTH;
        if iter * SHELL_CODEC_FRAME_LENGTH < self.frame_length {
            iter += 1;
        }

        let mut sum_pulses = vec![0usize; iter];
        let mut n_lshifts = vec![0usize; iter];

        // Decode sum of pulses per block
        let cdf_ptr = &SILK_PULSES_PER_BLOCK_ICDF[rate_level.min(N_RATE_LEVELS - 1)];
        for i in 0..iter {
            sum_pulses[i] = ec.decode_icdf(cdf_ptr, 8) as usize;

            // LSB indication
            while sum_pulses[i] == SILK_MAX_PULSES + 1 {
                n_lshifts[i] += 1;
                sum_pulses[i] = ec.decode_icdf(cdf_ptr, 8) as usize;
            }
        }

        // Shell decoding (simplified - just zero out for now)
        for i in 0..iter {
            let start = i * SHELL_CODEC_FRAME_LENGTH;
            let end = cmp::min(start + SHELL_CODEC_FRAME_LENGTH, pulses.len());
            pulses[start..end].fill(0);
        }

        // Decode signs (simplified)
        Ok(())
    }

    /// Decode parameters from indices
    fn decode_parameters(
        &mut self,
        dec_ctrl: &mut SilkDecoderControl,
        _cond_coding: i32,
    ) -> Result<(), &'static str> {
        // Dequantize gains (simplified linear mapping)
        for k in 0..self.nb_subfr {
            let gain_idx = self.indices.gains_indices[k] as i32;
            // Simple gain mapping: 2 dB to 88 dB range
            let gain_db = 2.0 + (gain_idx as f32 * 86.0 / 63.0);
            dec_ctrl.gains_q16[k] = ((10.0_f32.powf(gain_db / 20.0) * 65536.0) as i32).max(65536);
        }

        // Decode NLSFs and convert to LPC (simplified - use previous)
        for i in 0..self.lpc_order {
            dec_ctrl.pred_coef_q12[1][i] = 0; // Placeholder
        }

        // Handle interpolation
        if self.indices.nlsf_interp_coef_q2 < 4 {
            for i in 0..self.lpc_order {
                dec_ctrl.pred_coef_q12[0][i] = dec_ctrl.pred_coef_q12[1][i];
            }
        } else {
            // Copy without self-borrow issue
            let temp = dec_ctrl.pred_coef_q12[1];
            dec_ctrl.pred_coef_q12[0] = temp;
        }

        // Decode pitch parameters for voiced frames
        if self.indices.signal_type == TYPE_VOICED {
            // Decode pitch lags (simplified)
            let base_lag = self.indices.lag_index as i32;
            for k in 0..self.nb_subfr {
                dec_ctrl.pitch_l[k] = base_lag;
            }

            // LTP coefficients (simplified - use basic values)
            dec_ctrl.ltp_coef_q14.fill(4096); // 0.25 in Q14

            // LTP scaling
            dec_ctrl.ltp_scale_q14 = SILK_LTPSCALES_TABLE_Q14[self.indices.ltp_scale_index.min(2) as usize] as i32;
        } else {
            dec_ctrl.pitch_l.fill(0);
            dec_ctrl.ltp_coef_q14.fill(0);
            dec_ctrl.ltp_scale_q14 = 0;
        }

        Ok(())
    }

    /// Core synthesis: LTP + LPC synthesis
    fn decode_core(
        &mut self,
        dec_ctrl: &SilkDecoderControl,
        output: &mut [i16],
        pulses: &[i16],
    ) -> Result<(), &'static str> {
        let offset_q10 = SILK_QUANTIZATION_OFFSETS_Q10[self.indices.signal_type as usize >> 1]
            [self.indices.quant_offset_type as usize];

        // Decode excitation from pulses
        let mut rand_seed = self.indices.seed as i32;
        for i in 0..self.frame_length {
            rand_seed = silk_rand(rand_seed);
            let mut exc = (pulses[i] as i32) << 14;

            if exc > 0 {
                exc -= QUANT_LEVEL_ADJUST_Q10 << 4;
            } else if exc < 0 {
                exc += QUANT_LEVEL_ADJUST_Q10 << 4;
            }
            exc += (offset_q10 as i32) << 4;

            if rand_seed < 0 {
                exc = -exc;
            }

            self.exc_q14[i] = exc;
            rand_seed = rand_seed.wrapping_add(pulses[i] as i32);
        }

        // Copy LPC state
        let mut slpc_q14 = [0i32; MAX_LPC_ORDER + MAX_SUB_FRAME_LENGTH];
        slpc_q14[..MAX_LPC_ORDER].copy_from_slice(&self.slpc_q14_buf);

        // Process subframes
        let mut pexc_idx = 0;
        let mut pxq_idx = 0;

        for k in 0..self.nb_subfr {
            let a_q12 = &dec_ctrl.pred_coef_q12[k >> 1];
            let _b_q14 = &dec_ctrl.ltp_coef_q14[k * LTP_ORDER..];
            let gain_q10 = dec_ctrl.gains_q16[k] >> 6;

            // LPC synthesis for this subframe
            for i in 0..self.subfr_length {
                // LTP prediction (simplified - just use excitation)
                let lpc_exc = self.exc_q14[pexc_idx + i];

                // LPC prediction (order 10 or 16)
                let mut lpc_pred_q10 = self.lpc_order as i32 >> 1;
                for j in 0..self.lpc_order.min(10) {
                    lpc_pred_q10 += ((slpc_q14[MAX_LPC_ORDER + i - j - 1] as i64 * a_q12[j] as i64) >> 16) as i32;
                }

                // Add LPC prediction to excitation
                slpc_q14[MAX_LPC_ORDER + i] = lpc_exc + (lpc_pred_q10 << 4);

                // Apply gain and convert to output
                let sample = (((slpc_q14[MAX_LPC_ORDER + i] as i64 * gain_q10 as i64) >> 8) >> 8) as i32;
                output[pxq_idx + i] = sample.clamp(-32768, 32767) as i16;
            }

            // Update LPC state
            slpc_q14.copy_within(self.subfr_length..self.subfr_length + MAX_LPC_ORDER, 0);

            pexc_idx += self.subfr_length;
            pxq_idx += self.subfr_length;
        }

        // Save LPC state
        self.slpc_q14_buf.copy_from_slice(&slpc_q14[..MAX_LPC_ORDER]);

        Ok(())
    }
}

// Helper: SILK pseudo-random number generator
fn silk_rand(seed: i32) -> i32 {
    seed.wrapping_mul(196314165).wrapping_add(907633515)
}

// Entropy coding tables (simplified versions)
static SILK_TYPE_OFFSET_NO_VAD_ICDF: [u8; 2] = [184, 0];
static SILK_DELTA_GAIN_ICDF: [u8; 41] = [
    250, 245, 234, 203, 71, 50, 42, 38, 35, 33, 31, 29, 28, 27, 26, 25, 24, 23, 22, 21,
    20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0,
];
static SILK_GAIN_ICDF: [[u8; 8]; 3] = [
    [224, 112, 44, 15, 3, 2, 1, 0],
    [224, 112, 44, 15, 3, 2, 1, 0],
    [224, 112, 44, 15, 3, 2, 1, 0],
];
static SILK_UNIFORM4_ICDF: [u8; 4] = [192, 128, 64, 0];
static SILK_UNIFORM8_ICDF: [u8; 8] = [224, 192, 160, 128, 96, 64, 32, 0];
static SILK_NLSF_INTERPOLATION_FACTOR_ICDF: [u8; 5] = [243, 221, 192, 181, 0];
static SILK_PITCH_LAG_ICDF: [u8; 32] = [
    252, 250, 244, 237, 229, 219, 207, 194, 182, 168, 154, 140, 126, 113, 100, 88,
    76, 65, 55, 46, 38, 31, 25, 20, 15, 11, 8, 5, 3, 2, 1, 0,
];
static SILK_PITCH_CONTOUR_ICDF: [u8; 34] = [
    252, 250, 244, 237, 229, 221, 212, 202, 192, 182, 170, 158, 146, 134, 122, 110,
    98, 87, 77, 67, 58, 49, 41, 34, 28, 22, 17, 13, 9, 6, 4, 2, 1, 0,
];
static SILK_LTP_PER_INDEX_ICDF: [u8; 3] = [179, 99, 0];
static SILK_LTP_GAIN_ICDF_0: [u8; 8] = [224, 168, 119, 80, 50, 30, 16, 0];
static SILK_LTPSCALE_ICDF: [u8; 3] = [128, 64, 0];
static SILK_RATE_LEVELS_ICDF: [[u8; 9]; 2] = [
    [250, 245, 234, 203, 71, 50, 42, 38, 0],
    [250, 245, 234, 203, 71, 50, 42, 38, 0],
];
static SILK_PULSES_PER_BLOCK_ICDF: [[u8; 18]; N_RATE_LEVELS] = [
    [125, 51, 26, 18, 15, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
    [198, 105, 45, 22, 15, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
    [213, 162, 116, 83, 59, 43, 32, 24, 18, 15, 12, 9, 7, 6, 5, 3, 2, 0],
    [239, 187, 116, 59, 28, 16, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
    [250, 229, 188, 135, 86, 51, 30, 19, 13, 10, 8, 6, 5, 4, 3, 2, 1, 0],
    [249, 235, 213, 185, 156, 128, 103, 83, 66, 53, 42, 33, 26, 21, 17, 13, 10, 0],
    [254, 249, 235, 206, 164, 118, 77, 46, 27, 16, 10, 7, 5, 4, 3, 2, 1, 0],
    [255, 253, 249, 239, 220, 191, 156, 119, 85, 57, 37, 23, 15, 10, 6, 4, 2, 0],
    [255, 253, 251, 246, 237, 223, 203, 179, 152, 124, 98, 75, 55, 40, 29, 21, 15, 0],
    [255, 254, 253, 247, 220, 162, 106, 67, 42, 28, 18, 12, 9, 6, 4, 3, 2, 0],
];

// Quantization offset table
static SILK_QUANTIZATION_OFFSETS_Q10: [[i16; 2]; 2] = [
    [OFFSET_UVL_Q10 as i16, OFFSET_UVH_Q10 as i16], // Unvoiced
    [OFFSET_VL_Q10 as i16, OFFSET_VH_Q10 as i16],   // Voiced
];

// LTP scale table
static SILK_LTPSCALES_TABLE_Q14: [i16; 3] = [15565, 12288, 8192];
