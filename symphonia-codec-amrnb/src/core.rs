// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-channel AMR-NB speech synthesis state.
//!
//! The synthesis follows FFmpeg's floating point `libavcodec/amrnbdec.c` (LGPL-2.1+, see
//! `NOTICE`) which in turn follows 3GPP TS 26.090/26.071.

use symphonia_core::errors::{Result, decode_error};

use crate::celp::{
    Fixed, LP_ORDER, SUBFRAME, adaptive_gain_control, circ_add, circ_add_acc, dot, interpolate,
    lp_synthesis, lp_zero_synthesis, order_2_filter, scale_to_sum_of_squares, tilt_compensation,
    weighted_sum,
};
use crate::lsp::{lsf2lsp, lsp2lpc, set_min_dist_lsf};
use crate::tables::*;

/// Samples per frame.
pub(crate) const FRAME_SAMPLES: usize = 160;

/// Frame types (table 1a of 3GPP TS 26.101). The speech modes are `0..=7`.
pub(crate) const MODE_4K75: u8 = 0;
pub(crate) const MODE_5K15: u8 = 1;
pub(crate) const MODE_5K9: u8 = 2;
pub(crate) const MODE_6K7: u8 = 3;
pub(crate) const MODE_7K4: u8 = 4;
pub(crate) const MODE_7K95: u8 = 5;
pub(crate) const MODE_10K2: u8 = 6;
pub(crate) const MODE_12K2: u8 = 7;
/// Frame type of SID (comfort noise) frames.
pub(crate) const MODE_DTX: u8 = 8;

/// Largest pitch delay.
const PITCH_DELAY_MAX: usize = 143;
/// Smallest pitch delay.
const PITCH_DELAY_MIN: i32 = 20;
/// Lower bound on the decoded lag search in the 12.2 kbit/s mode.
const PITCH_LAG_MIN_MODE_12K2: i32 = 18;

/// Scale from the constructed speech to [-1, 1]: AMR upscales by two (3GPP TS 26.090 6.2.2) and
/// produces 16-bit PCM.
const SAMPLE_SCALE: f64 = 2.0 / 32768.0;
/// Threshold for synthesis overflow.
const SAMPLE_BOUND: f32 = 32768.0;
/// Prediction factor for the 12.2 kbit/s mode.
const PRED_FAC_MODE_12K2: f64 = 0.65;
/// LSF residual tables to Hertz.
const LSF_R_FAC: f64 = 8000.0 / 32768.0;
/// Minimum LSF spacing, ensures stability of the LPC filter.
const MIN_LSF_SPACING: f64 = 50.0488 / 8000.0;
/// Initial prediction error energy in dB.
const MIN_ENERGY: f32 = -14.0;
/// Maximum sharpening factor. The specification says 0.8 (13107 in Q14) but the reference code
/// uses 13017.
const SHARP_MAX: f32 = 0.794_494_63;
/// Number of impulse response coefficients used for the tilt factor.
const TILT_RESPONSE: usize = 22;
/// Tilt factor = first reflection coefficient * gamma_t.
const TILT_GAMMA_T: f64 = 0.8;
/// Adaptive gain control factor used in the post-filter.
const AGC_ALPHA: f32 = 0.9;

/// Size of the excitation buffer: history, interpolation margin and current subframe.
const EXC_LEN: usize = PITCH_DELAY_MAX + LP_ORDER + 1 + SUBFRAME;
/// Offset of the current subframe in the excitation buffer.
const EXC_OFF: usize = PITCH_DELAY_MAX + LP_ORDER + 1;

/// Unpacked frame parameters, indexed as described in [`crate::tables`].
pub(crate) type Params = [u16; FRAME_PARAMS];

/// Extracts the parameters of a speech frame from its bits (`ff_amr_bit_reorder`).
///
/// `data` are the speech bytes following the TOC byte.
pub(crate) fn unpack(params: &mut Params, data: &[u8], mode: u8) {
    params.fill(0);

    let mut order = ORDER_PER_MODE[usize::from(mode)].iter().copied();
    while let Some(nbits) = order.next() {
        if nbits == 0 {
            break;
        }
        let Some(field) = order.next()
        else {
            break;
        };
        let mut value = 0u16;
        for _ in 0..nbits {
            let Some(bit) = order.next()
            else {
                break;
            };
            let byte = data.get(usize::from(bit >> 3)).copied().unwrap_or(0);
            value = (value << 1) | u16::from((byte >> (bit & 7)) & 1);
        }
        if let Some(p) = params.get_mut(usize::from(field)) {
            *p = value;
        }
    }
}

/// Decoder state of one channel.
pub(crate) struct Core {
    /// LSF residual of the previous frame.
    prev_lsf_r: [i16; LP_ORDER],
    /// LSPs of the four subframes of the current frame.
    lsp: [[f64; LP_ORDER]; 4],
    /// LSP vector of the fourth subframe of the previous frame.
    prev_lsp_sub4: [f64; LP_ORDER],
    /// Interpolated LSF vectors for fixed gain smoothing.
    lsf_q: [[f32; LP_ORDER]; 4],
    /// Averaged LSF vector.
    lsf_avg: [f32; LP_ORDER],
    /// LPC coefficients of the four subframes.
    lpc: [[f32; LP_ORDER]; 4],
    /// Integer part of the pitch lag of the current subframe.
    pitch_lag_int: u8,
    /// Current excitation and its history; the current subframe is at `EXC_OFF`.
    excitation_buf: [f32; EXC_LEN],
    /// Adaptive codebook (pitch) vector.
    pitch_vector: [f32; SUBFRAME],
    /// Algebraic codebook (fixed) vector; zero between subframes.
    fixed_vector: [f32; SUBFRAME],
    /// Quantified prediction errors 20log10(^gamma_gc) of the previous four subframes.
    prediction_error: [f32; 4],
    /// Pitch gains of the current and the previous four subframes.
    pitch_gain: [f32; 5],
    /// Fixed gains of the current and the previous four subframes.
    fixed_gain: [f32; 5],
    /// Previous pitch gain bounded by `[0, SHARP_MAX]`.
    beta: f32,
    /// Number of subframes for which the LSF distance has been above 0.65.
    diff_count: u8,
    /// Number of subframes since a hangover period started.
    hang_count: u8,
    /// Previous fixed gain; used by anti-sparseness processing to detect an onset.
    prev_sparse_fixed_gain: f32,
    /// Previous impulse response filter number: 0 strong, 1 medium, 2 none.
    prev_ir_filter_nr: u8,
    /// Impulse response filter onset counter.
    ir_filter_onset: u8,
    /// Intermediate values of the post-filter formant filter.
    postfilter_mem: [f32; LP_ORDER],
    /// Previous input of the tilt compensation filter.
    tilt_mem: f32,
    /// Previous adaptive gain control factor.
    postfilter_agc: f32,
    /// High-pass filter memory.
    high_pass_mem: [f32; 2],
    /// Synthesised samples with `LP_ORDER` samples of history.
    samples_in: [f32; LP_ORDER + SUBFRAME],
    /// Unpacked parameters of the current frame.
    params: Params,
}

impl Core {
    pub fn new() -> Box<Self> {
        let mut c = Box::new(Core {
            prev_lsf_r: [0; LP_ORDER],
            lsp: [[0.0; LP_ORDER]; 4],
            prev_lsp_sub4: [0.0; LP_ORDER],
            lsf_q: [[0.0; LP_ORDER]; 4],
            lsf_avg: [0.0; LP_ORDER],
            lpc: [[0.0; LP_ORDER]; 4],
            pitch_lag_int: 0,
            excitation_buf: [0.0; EXC_LEN],
            pitch_vector: [0.0; SUBFRAME],
            fixed_vector: [0.0; SUBFRAME],
            prediction_error: [MIN_ENERGY; 4],
            pitch_gain: [0.0; 5],
            fixed_gain: [0.0; 5],
            beta: 0.0,
            diff_count: 0,
            hang_count: 0,
            prev_sparse_fixed_gain: 0.0,
            prev_ir_filter_nr: 0,
            ir_filter_onset: 0,
            postfilter_mem: [0.0; LP_ORDER],
            tilt_mem: 0.0,
            postfilter_agc: 0.0,
            high_pass_mem: [0.0; 2],
            samples_in: [0.0; LP_ORDER + SUBFRAME],
            params: [0; FRAME_PARAMS],
        });

        for i in 0..LP_ORDER {
            // Initial values of the decoder reset of 3GPP TS 26.073 (`Decoder_amr_reset`).
            c.prev_lsp_sub4[i] =
                f64::from((i32::from(LSP_SUB4_INIT[i]) * 1000) as f32 / 32768.0f32);
            c.lsf_q[3][i] = f32::from(LSP_AVG_INIT[i]) / 32768.0f32;
            c.lsf_avg[i] = c.lsf_q[3][i];
        }
        c
    }

    /// Decodes a speech frame of mode `mode` from the speech bytes `data` into `out`.
    pub fn decode_speech(
        &mut self,
        mode: u8,
        data: &[u8],
        out: &mut [f32; FRAME_SAMPLES],
    ) -> Result<()> {
        unpack(&mut self.params, data, mode);

        if mode == MODE_12K2 {
            self.lsf2lsp_5();
        }
        else {
            self.lsf2lsp_3(mode);
        }

        for i in 0..4 {
            lsp2lpc(&self.lsp[i], &mut self.lpc[i]);
        }

        // Fixed vector up to anti-sparseness processing. Like the reference, it persists over
        // subframes (only `no_repeat_mask` is sticky).
        let mut fixed_sparse = Fixed::default();

        for subframe in 0..4 {
            let sf = SUBFRAME_BASE + SUBFRAME_PARAMS * subframe;
            let sub: &[u16] = &self.params[sf..sf + SUBFRAME_PARAMS];
            let (p_lag, p_gain, fixed_gain_idx) = (sub[0], sub[1], sub[2]);
            let mut pulses = [0u16; 10];
            pulses.copy_from_slice(&sub[3..13]);

            self.decode_pitch_vector(mode, p_lag, subframe)?;

            decode_fixed_sparse(&mut fixed_sparse, &pulses, mode, subframe);

            // The fixed gain depends on the fixed vector, but the fixed vector calculation uses
            // pitch sharpening based on the pitch gain. So the order is: pitch gain, pitch
            // sharpening, fixed gain.
            let fixed_gain_factor = self.decode_gains(mode, p_gain, fixed_gain_idx, subframe);

            self.pitch_sharpening(subframe, mode, &mut fixed_sparse);

            if fixed_sparse.pitch_lag == 0 {
                return decode_error("amrnb: pitch lag of zero");
            }
            fixed_sparse.add_to(&mut self.fixed_vector, 1.0);

            self.fixed_gain[4] = set_fixed_gain(
                fixed_gain_factor,
                dot(&self.fixed_vector, &self.fixed_vector) / SUBFRAME as f32,
                &mut self.prediction_error,
                ENERGY_MEAN[usize::from(mode)],
            );

            // The excitation feedback is calculated without any processing such as fixed gain
            // smoothing.
            let pitch_gain = self.pitch_gain[4];
            let fixed_gain = self.fixed_gain[4];
            {
                let exc = &mut self.excitation_buf[EXC_OFF..];
                for e in exc.iter_mut() {
                    *e *= pitch_gain;
                }
                fixed_sparse.add_to(exc, fixed_gain);

                // In the reference decoder the excitation is stored with no fractional bits.
                // This step prevents buzz in silent periods.
                for e in exc.iter_mut() {
                    *e = e.trunc();
                }
            }

            // Smooth the fixed gain. In the reference source the smoothed value is not fed back
            // into later fixed gain smoothing.
            let synth_fixed_gain = self.fixed_gain_smooth(subframe, mode);

            let mut spare_vector = [0.0f32; SUBFRAME];
            let use_spare =
                self.anti_sparseness(&fixed_sparse, synth_fixed_gain, mode, &mut spare_vector);
            let fixed_vector = if use_spare { spare_vector } else { self.fixed_vector };

            if self.synthesis(subframe, mode, synth_fixed_gain, &fixed_vector, false) {
                // Overflow detected: rerun the synthesis with the pitch vector scaled down by a
                // factor of 4, skipping the pitch emphasis.
                self.synthesis(subframe, mode, synth_fixed_gain, &fixed_vector, true);
            }

            let mut sub_out = [0.0f32; SUBFRAME];
            self.postfilter(subframe, mode, &mut sub_out);
            out[subframe * SUBFRAME..(subframe + 1) * SUBFRAME].copy_from_slice(&sub_out);

            // Update the buffers and history.
            fixed_sparse.clear_in(&mut self.fixed_vector);
            self.update_state();
        }

        order_2_filter(
            out,
            &HIGHPASS_ZEROS,
            &HIGHPASS_POLES,
            (f64::from(HIGHPASS_GAIN) * SAMPLE_SCALE) as f32,
            &mut self.high_pass_mem,
        );

        // Update the averaged LSF vector (used for fixed gain smoothing). The average does not
        // incorporate the current frame's LSFs for `fixed_gain_smooth`; the reference decoder uses
        // qbar(n-1) rather than qbar(n) in equation 71 of the specification.
        let old_avg = self.lsf_avg;
        weighted_sum(&mut self.lsf_avg, &old_avg, &self.lsf_q[3], 0.84, 0.16);

        Ok(())
    }

    /// Interpolates the LSF vector (for fixed gain smoothing) over all four subframes.
    fn interpolate_lsf(&mut self, lsf_new: &[f32; LP_ORDER]) {
        let last = self.lsf_q[3];
        for i in 0..4 {
            weighted_sum(
                &mut self.lsf_q[i],
                &last,
                lsf_new,
                0.25 * (3 - i) as f32,
                0.25 * (i + 1) as f32,
            );
        }
    }

    /// Decodes one half of the split-matrix quantized LSFs of the 12.2 kbit/s mode into an LSP
    /// vector.
    fn lsf2lsp_for_mode12k2(
        &mut self,
        lsp_idx: usize,
        lsf_no_r: &[f32; LP_ORDER],
        quantizer: &[&[i16; 4]; 5],
        offset: usize,
        sign: bool,
        update: bool,
    ) {
        let mut lsf_r = [0i16; LP_ORDER];
        for (i, q) in quantizer.iter().enumerate() {
            lsf_r[2 * i] = q[offset];
            lsf_r[2 * i + 1] = q[offset + 1];
        }

        if sign {
            lsf_r[4] = lsf_r[4].wrapping_neg();
            lsf_r[5] = lsf_r[5].wrapping_neg();
        }

        if update {
            self.prev_lsf_r = lsf_r;
        }

        let mut lsf_q = [0.0f32; LP_ORDER];
        for i in 0..LP_ORDER {
            lsf_q[i] = (f64::from(lsf_r[i]) * (LSF_R_FAC / 8000.0)
                + f64::from(lsf_no_r[i]) * (1.0 / 8000.0)) as f32;
        }

        set_min_dist_lsf(&mut lsf_q, MIN_LSF_SPACING);

        if update {
            self.interpolate_lsf(&lsf_q);
        }

        lsf2lsp(&mut self.lsp[lsp_idx], &lsf_q);
    }

    /// Decodes a set of 5 split-matrix quantized LSF indices into 2 LSP vectors (12.2 kbit/s).
    fn lsf2lsp_5(&mut self) {
        let p = &self.params;
        // The index widths guarantee the table bounds (see the `table_bounds` test).
        let quantizer = [
            &LSF_5_1[usize::from(p[0])],
            &LSF_5_2[usize::from(p[1])],
            &LSF_5_3[usize::from(p[2] >> 1)],
            &LSF_5_4[usize::from(p[3])],
            &LSF_5_5[usize::from(p[4])],
        ];
        let sign = p[2] & 1 != 0;

        let mut lsf_no_r = [0.0f32; LP_ORDER];
        for i in 0..LP_ORDER {
            lsf_no_r[i] = (f64::from(self.prev_lsf_r[i]) * LSF_R_FAC * PRED_FAC_MODE_12K2
                + f64::from(LSF_5_MEAN[i])) as f32;
        }

        self.lsf2lsp_for_mode12k2(1, &lsf_no_r, &quantizer, 0, sign, false);
        self.lsf2lsp_for_mode12k2(3, &lsf_no_r, &quantizer, 2, sign, true);

        // Interpolate the LSP vectors at subframes 1 and 3.
        for j in 0..LP_ORDER {
            self.lsp[0][j] = 0.5 * self.prev_lsp_sub4[j] + 0.5 * self.lsp[1][j];
            self.lsp[2][j] = 0.5 * self.lsp[1][j] + 0.5 * self.lsp[3][j];
        }
    }

    /// Decodes a set of 3 split-matrix quantized LSF indices into an LSP vector (modes other
    /// than 12.2 kbit/s) and interpolates the vectors of the subframes.
    fn lsf2lsp_3(&mut self, mode: u8) {
        let p = &self.params;
        let mut lsf_r = [0i16; LP_ORDER];

        let q1 = if mode == MODE_7K95 {
            &LSF_3_1_MODE_7K95[usize::from(p[0])]
        }
        else {
            &LSF_3_1[usize::from(p[0])]
        };
        lsf_r[0..3].copy_from_slice(q1);

        let q2 = &LSF_3_2[usize::from(p[1]) << usize::from(mode <= MODE_5K15)];
        lsf_r[3..6].copy_from_slice(q2);

        let q3 = if mode <= MODE_5K15 {
            &LSF_3_3_MODE_5K15[usize::from(p[2])]
        }
        else {
            &LSF_3_3[usize::from(p[2])]
        };
        lsf_r[6..10].copy_from_slice(q3);

        // Calculate the mean-removed LSF vector and add the mean.
        let mut lsf_q = [0.0f32; LP_ORDER];
        for i in 0..LP_ORDER {
            let v = f32::from(lsf_r[i]) + f32::from(self.prev_lsf_r[i]) * PRED_FAC[i];
            lsf_q[i] = (f64::from(v) * (LSF_R_FAC / 8000.0)
                + f64::from(LSF_3_MEAN[i]) * (1.0 / 8000.0)) as f32;
        }

        set_min_dist_lsf(&mut lsf_q, MIN_LSF_SPACING);

        // Store data for computing the next frame's LSFs.
        self.interpolate_lsf(&lsf_q);
        self.prev_lsf_r = lsf_r;

        lsf2lsp(&mut self.lsp[3], &lsf_q);

        // Interpolate the LSP vectors at subframes 1, 2 and 3.
        for i in 1..=3usize {
            for j in 0..LP_ORDER {
                self.lsp[i - 1][j] = self.prev_lsp_sub4[j]
                    + (self.lsp[3][j] - self.prev_lsp_sub4[j]) * 0.25 * i as f64;
            }
        }
    }

    /// Decodes the pitch lag and builds the adaptive codebook vector at the current excitation
    /// position.
    fn decode_pitch_vector(&mut self, mode: u8, p_lag: u16, subframe: usize) -> Result<()> {
        let (mut lag_int, mut lag_frac);

        if mode == MODE_12K2 {
            (lag_int, lag_frac) =
                decode_pitch_lag_1_6(i32::from(p_lag), i32::from(self.pitch_lag_int), subframe);
        }
        else {
            (lag_int, lag_frac) = decode_pitch_lag(
                i32::from(p_lag),
                i32::from(self.pitch_lag_int),
                subframe,
                mode != MODE_4K75 && mode != MODE_5K15,
                if mode <= MODE_6K7 {
                    4
                }
                else if mode == MODE_7K95 {
                    5
                }
                else {
                    6
                },
            );
            lag_frac *= 2;
        }

        // Stored as a u8 like the reference.
        self.pitch_lag_int = lag_int as u8;

        lag_int += i32::from(lag_frac > 0);

        // The interpolation reads 10 samples before and after the lagged block; the reference
        // never produces lags outside this range for valid streams.
        if lag_int < 10 || lag_int as usize > EXC_OFF - 9 {
            return decode_error("amrnb: pitch lag out of range");
        }

        // Calculate the pitch vector by interpolating the past excitation at the pitch lag using
        // a hamming windowed sinc function.
        let src = EXC_OFF + 1 - lag_int as usize;
        let frac_pos = (lag_frac + 6 - 6 * i32::from(lag_frac > 0)) as usize;
        interpolate(&mut self.excitation_buf, EXC_OFF, src, frac_pos);

        self.pitch_vector.copy_from_slice(&self.excitation_buf[EXC_OFF..EXC_OFF + SUBFRAME]);
        Ok(())
    }

    /// Decodes the pitch gain and the fixed gain factor (part of section 6.1.3). Returns the
    /// fixed gain factor.
    fn decode_gains(&mut self, mode: u8, p_gain: u16, fixed_gain_idx: u16, subframe: usize) -> f32 {
        if mode == MODE_12K2 || mode == MODE_7K95 {
            self.pitch_gain[4] =
                (f64::from(QUA_GAIN_PIT[usize::from(p_gain)]) * (1.0 / 16384.0)) as f32;
            (f64::from(QUA_GAIN_CODE[usize::from(fixed_gain_idx)]) * (1.0 / 2048.0)) as f32
        }
        else {
            let gains = if mode >= MODE_6K7 {
                &GAINS_HIGH[usize::from(p_gain)]
            }
            else if mode >= MODE_5K15 {
                &GAINS_LOW[usize::from(p_gain)]
            }
            else {
                // The gain index is only coded in subframes 0 and 2 for the 4.75 kbit/s mode.
                let idx = SUBFRAME_BASE + SUBFRAME_PARAMS * (subframe & 2) + 1;
                &GAINS_MODE_4K75[(usize::from(self.params[idx]) << 1) + (subframe & 1)]
            };

            self.pitch_gain[4] = (f64::from(gains[0]) * (1.0 / 16384.0)) as f32;
            (f64::from(gains[1]) * (1.0 / 4096.0)) as f32
        }
    }

    /// Applies the pitch lag to obtain the sharpened fixed vector (section 6.1.2).
    fn pitch_sharpening(&mut self, subframe: usize, mode: u8, fixed_sparse: &mut Fixed) {
        // The specification suggests the current pitch gain is always used, but in other modes
        // the pitch and codebook gains are jointly quantized (section 5.8.2) so the codebook gain
        // cannot depend on the quantized pitch gain.
        if mode == MODE_12K2 {
            self.beta = self.pitch_gain[4].min(1.0);
        }

        fixed_sparse.pitch_lag = usize::from(self.pitch_lag_int);
        fixed_sparse.pitch_fac = self.beta;

        // Save the pitch sharpening factor for the next subframe. The 4.75 kbit/s mode only
        // updates on the 2nd and 4th subframes since the gains of two subframes are jointly
        // quantized.
        if mode != MODE_4K75 || subframe & 1 != 0 {
            self.beta = self.pitch_gain[4].clamp(0.0, SHARP_MAX);
        }
    }

    /// Fixed gain smoothing (section 6.1.4). Note that where the specification specifies the
    /// "spectrum in the q domain", frequencies are used in practice.
    fn fixed_gain_smooth(&mut self, subframe: usize, mode: u8) -> f32 {
        let lsf = &self.lsf_q[subframe];
        let mut diff = 0.0f32;
        for i in 0..LP_ORDER {
            let d = f64::from(self.lsf_avg[i] - lsf[i]).abs() / f64::from(self.lsf_avg[i]);
            diff = (f64::from(diff) + d) as f32;
        }

        // If the difference is large for ten subframes, disable smoothing for a 40-subframe
        // hangover period.
        self.diff_count = self.diff_count.saturating_add(1);
        if f64::from(diff) <= 0.65 {
            self.diff_count = 0;
        }

        if self.diff_count > 10 {
            self.hang_count = 0;
            self.diff_count -= 1;
        }

        if self.hang_count < 40 {
            self.hang_count += 1;
        }
        else if mode < MODE_7K4 || mode == MODE_10K2 {
            let smoothing_factor = (4.0 * f64::from(diff) - 1.6).clamp(0.0, 1.0) as f32;
            let g = &self.fixed_gain;
            let fixed_gain_mean = ((g[0] + g[1] + g[2] + g[3] + g[4]) as f64 * 0.2) as f32;
            return (f64::from(smoothing_factor) * f64::from(g[4])
                + (1.0 - f64::from(smoothing_factor)) * f64::from(fixed_gain_mean))
                as f32;
        }
        self.fixed_gain[4]
    }

    /// Reduces the sparseness of the fixed vector by smoothing it with one of three impulse
    /// response filters ("adaptive phase dispersion", 3GPP TS 26.090 section 6.1(5)).
    ///
    /// Returns true if the filtered vector was written to `out`.
    fn anti_sparseness(
        &mut self,
        fixed_sparse: &Fixed,
        fixed_gain: f32,
        mode: u8,
        out: &mut [f32; SUBFRAME],
    ) -> bool {
        let mut ir_filter_nr = if self.pitch_gain[4] < 0.6 {
            0 // strong filtering
        }
        else if self.pitch_gain[4] < 0.9 {
            1 // medium filtering
        }
        else {
            2 // no filtering
        };

        // Detect an onset.
        if f64::from(fixed_gain) > 2.0 * f64::from(self.prev_sparse_fixed_gain) {
            self.ir_filter_onset = 2;
        }
        else if self.ir_filter_onset != 0 {
            self.ir_filter_onset -= 1;
        }

        if self.ir_filter_onset == 0 {
            let count = self.pitch_gain.iter().filter(|&&g| g < 0.6).count();
            if count > 2 {
                ir_filter_nr = 0;
            }
            if ir_filter_nr > self.prev_ir_filter_nr + 1 {
                ir_filter_nr -= 1;
            }
        }
        else if ir_filter_nr < 2 {
            ir_filter_nr += 1;
        }

        // Disable filtering for a very low fixed gain. This step is not in the technical
        // description but is in the reference source (function `Ph_disp`).
        if fixed_gain < 5.0 {
            ir_filter_nr = 2;
        }

        let mut filtered = false;
        if mode != MODE_7K4 && mode < MODE_10K2 && ir_filter_nr < 2 {
            let filters = if mode == MODE_7K95 { &IR_FILTERS_7K95 } else { &IR_FILTERS };
            apply_ir_filter(out, fixed_sparse, filters[usize::from(ir_filter_nr)]);
            filtered = true;
        }

        // Update the impulse response filter strength history.
        self.prev_ir_filter_nr = ir_filter_nr;
        self.prev_sparse_fixed_gain = fixed_gain;

        filtered
    }

    /// 10th order LPC synthesis. Returns true if an overflow was detected.
    fn synthesis(
        &mut self,
        subframe: usize,
        mode: u8,
        fixed_gain: f32,
        fixed_vector: &[f32; SUBFRAME],
        overflow: bool,
    ) -> bool {
        // If an overflow has been detected, the pitch vector is scaled down by a factor of 4.
        if overflow {
            for v in self.pitch_vector.iter_mut() {
                *v *= 0.25;
            }
        }

        let pitch_gain = self.pitch_gain[4];
        let mut excitation = [0.0f32; SUBFRAME];
        weighted_sum(&mut excitation, &self.pitch_vector, fixed_vector, pitch_gain, fixed_gain);

        // Emphasize the pitch vector contribution.
        if pitch_gain > 0.5 && !overflow {
            let energy = dot(&excitation, &excitation);
            let pitch_factor = (f64::from(pitch_gain)
                * if mode == MODE_12K2 {
                    0.25 * f64::from(pitch_gain.min(1.0))
                }
                else {
                    0.5 * f64::from(pitch_gain.min(SHARP_MAX))
                }) as f32;

            for (e, p) in excitation.iter_mut().zip(&self.pitch_vector) {
                *e += pitch_factor * p;
            }

            scale_to_sum_of_squares(&mut excitation, energy);
        }

        self.samples_in[LP_ORDER..].copy_from_slice(&excitation);
        lp_synthesis(&mut self.samples_in, &self.lpc[subframe], SUBFRAME);

        // Detect an overflow.
        self.samples_in[LP_ORDER..].iter().any(|s| s.abs() > SAMPLE_BOUND)
    }

    /// Updates the buffers and history at the end of a subframe.
    fn update_state(&mut self) {
        self.prev_lsp_sub4 = self.lsp[3];

        self.excitation_buf.copy_within(SUBFRAME..SUBFRAME + EXC_OFF, 0);

        self.pitch_gain.copy_within(1..5, 0);
        self.fixed_gain.copy_within(1..5, 0);

        self.samples_in.copy_within(SUBFRAME..SUBFRAME + LP_ORDER, 0);
    }

    /// Gets the tilt factor of a formant filter from its transfer function.
    fn tilt_factor(lpc_n: &[f32; LP_ORDER], lpc_d: &[f32; LP_ORDER]) -> f32 {
        // LP_ORDER prior zeros are needed for the synthesis filter.
        let mut impulse = [0.0f32; LP_ORDER + TILT_RESPONSE];
        impulse[LP_ORDER] = 1.0;
        impulse[LP_ORDER + 1..LP_ORDER + 1 + LP_ORDER].copy_from_slice(lpc_n);
        lp_synthesis(&mut impulse, lpc_d, TILT_RESPONSE);

        let hf = &impulse[LP_ORDER..];
        let rh0 = dot(hf, hf);
        let rh1 = dot(hf, &hf[1..TILT_RESPONSE]);

        // The specification only specifies this check for the 12.2 and 10.2 kbit/s modes, but in
        // the reference source the tilt is always non-negative.
        if rh1 >= 0.0 { (f64::from(rh1 / rh0) * TILT_GAMMA_T) as f32 } else { 0.0 }
    }

    /// Adaptive post-filtering (section 6.2.1).
    fn postfilter(&mut self, subframe: usize, mode: u8, buf_out: &mut [f32; SUBFRAME]) {
        let lpc = &self.lpc[subframe];
        let samples = &self.samples_in[LP_ORDER..];
        let speech_gain = dot(samples, samples);

        let (gamma_n, gamma_d) = if mode == MODE_12K2 || mode == MODE_10K2 {
            (&POW_0_7, &POW_0_75)
        }
        else {
            (&POW_0_55, &POW_0_7)
        };

        let mut lpc_n = [0.0f32; LP_ORDER];
        let mut lpc_d = [0.0f32; LP_ORDER];
        for i in 0..LP_ORDER {
            lpc_n[i] = lpc[i] * gamma_n[i];
            lpc_d[i] = lpc[i] * gamma_d[i];
        }

        // Pole filter.
        let mut pole_out = [0.0f32; SUBFRAME + LP_ORDER];
        pole_out[..LP_ORDER].copy_from_slice(&self.postfilter_mem);
        pole_out[LP_ORDER..].copy_from_slice(samples);
        lp_synthesis(&mut pole_out, &lpc_d, SUBFRAME);
        self.postfilter_mem.copy_from_slice(&pole_out[SUBFRAME..]);

        // Zero filter.
        lp_zero_synthesis(buf_out, &lpc_n, &pole_out);

        tilt_compensation(&mut self.tilt_mem, Self::tilt_factor(&lpc_n, &lpc_d), buf_out);

        adaptive_gain_control(buf_out, speech_gain, AGC_ALPHA, &mut self.postfilter_agc);
    }
}

/// Decodes the pitch lag with 1/6 resolution (12.2 kbit/s mode).
fn decode_pitch_lag_1_6(pitch_index: i32, prev_lag_int: i32, subframe: usize) -> (i32, i32) {
    if subframe == 0 || subframe == 2 {
        if pitch_index < 463 {
            let lag_int = ((pitch_index + 107) * 10923) >> 16;
            (lag_int, pitch_index - lag_int * 6 + 105)
        }
        else {
            (pitch_index - 368, 0)
        }
    }
    else {
        let lag_int = (((pitch_index + 5) * 10923) >> 16) - 1;
        let lag_frac = pitch_index - lag_int * 6 - 3;
        (
            lag_int + (prev_lag_int - 5).clamp(PITCH_LAG_MIN_MODE_12K2, PITCH_DELAY_MAX as i32 - 9),
            lag_frac,
        )
    }
}

/// Decodes the pitch lag with 1/3 resolution (`ff_decode_pitch_lag`).
fn decode_pitch_lag(
    mut pitch_index: i32,
    prev_lag_int: i32,
    subframe: usize,
    third_as_first: bool,
    resolution: u8,
) -> (i32, i32) {
    let max = PITCH_DELAY_MAX as i32;

    // Note n * 10923 >> 15 is floor(x / 3) for 0 <= n <= 32767.
    if subframe == 0 || (subframe == 2 && third_as_first) {
        if pitch_index < 197 {
            pitch_index += 59;
        }
        else {
            pitch_index = 3 * pitch_index - 335;
        }
    }
    else if resolution == 4 {
        let search_range_min = (prev_lag_int - 5).clamp(PITCH_DELAY_MIN, max - 9);

        // Decoding with 4-bit resolution.
        if pitch_index < 4 {
            // Integer only precision for [search_range_min, search_range_min + 3].
            pitch_index = 3 * (pitch_index + search_range_min) + 1;
        }
        else if pitch_index < 12 {
            // 1/3 fractional precision for [search_range_min + 3 1/3, search_range_min + 5 2/3].
            pitch_index += 3 * search_range_min + 7;
        }
        else {
            // Integer only precision for [search_range_min + 6, search_range_min + 9].
            pitch_index = 3 * (pitch_index + search_range_min - 6) + 1;
        }
    }
    else {
        // Decoding with 5 or 6 bit resolution, 1/3 fractional precision.
        pitch_index -= 1;

        if resolution == 5 {
            pitch_index += 3 * (prev_lag_int - 10).clamp(PITCH_DELAY_MIN, max - 19);
        }
        else {
            pitch_index += 3 * (prev_lag_int - 5).clamp(PITCH_DELAY_MIN, max - 9);
        }
    }

    let lag_int = (pitch_index * 10923) >> 15;
    (lag_int, pitch_index - 3 * lag_int - 1)
}

/// Decodes a 10-bit algebraic codebook index from a 10.2 kbit/s frame: 7+3 bits with the 3 LSBs
/// being, individually, the LSB of 1 of the 3 pulses and the upper 7 bits coded in base 5.
fn decode_10bit_pulse(code: u16, pulse_position: &mut [usize; 8], i1: usize, i2: usize, i3: usize) {
    let positions = &BASE_FIVE_TABLE[usize::from(code >> 3) & 127];
    pulse_position[i1] = (usize::from(positions[2]) << 1) + usize::from(code & 1);
    pulse_position[i2] = (usize::from(positions[1]) << 1) + usize::from((code >> 1) & 1);
    pulse_position[i3] = (usize::from(positions[0]) << 1) + usize::from((code >> 2) & 1);
}

/// Decodes the algebraic codebook index of the 10.2 kbit/s mode (8 pulses, 31 bits).
fn decode_8_pulses_31bits(fixed_index: &[u16; 10], fixed_sparse: &mut Fixed) {
    let mut pulse_position = [0usize; 8];

    decode_10bit_pulse(fixed_index[4], &mut pulse_position, 0, 4, 1);
    decode_10bit_pulse(fixed_index[5], &mut pulse_position, 2, 6, 5);

    // Coded using 5+2 bits with the 2 LSBs being, individually, the LSB of 1 of the 2 pulses and
    // the upper 5 bits being coded in base 5.
    let temp = ((usize::from(fixed_index[6]) >> 2) * 25 + 12) >> 5;
    pulse_position[3] = temp % 5;
    pulse_position[7] = temp / 5;
    if pulse_position[7] & 1 != 0 {
        pulse_position[3] = 4 - pulse_position[3];
    }
    pulse_position[3] = (pulse_position[3] << 1) + usize::from(fixed_index[6] & 1);
    pulse_position[7] = (pulse_position[7] << 1) + usize::from((fixed_index[6] >> 1) & 1);

    fixed_sparse.n = 8;
    for i in 0..4 {
        let pos1 = (pulse_position[i] << 2) + i;
        let pos2 = (pulse_position[i + 4] << 2) + i;
        let sign = if fixed_index[i] != 0 { -1.0f32 } else { 1.0 };
        fixed_sparse.x[i] = pos1;
        fixed_sparse.x[i + 4] = pos2;
        fixed_sparse.y[i] = sign;
        fixed_sparse.y[i + 4] = if pos2 < pos1 { -sign } else { sign };
    }
}

/// Decodes the 10 pulse, 35 bit algebraic codebook of the 12.2 kbit/s mode
/// (`ff_decode_10_pulses_35bits` with `half_pulse_count = 5` and `bits = 3`).
fn decode_10_pulses_35bits(fixed_index: &[u16; 10], fixed_sparse: &mut Fixed) {
    fixed_sparse.no_repeat_mask = 0;
    fixed_sparse.n = 10;
    for i in 0..5 {
        let pos1 = usize::from(GRAY_DECODE[usize::from(fixed_index[2 * i + 1] & 7)]) + i;
        let pos2 = usize::from(GRAY_DECODE[usize::from(fixed_index[2 * i] & 7)]) + i;
        let sign = if fixed_index[2 * i + 1] & 8 != 0 { -1.0f32 } else { 1.0 };
        fixed_sparse.x[2 * i + 1] = pos1;
        fixed_sparse.x[2 * i] = pos2;
        fixed_sparse.y[2 * i + 1] = sign;
        fixed_sparse.y[2 * i] = if pos2 < pos1 { -sign } else { sign };
    }
}

/// Decodes the algebraic codebook index to pulse positions and signs.
///
/// | Mode                  | Pulses | Bits encoding the pulses |
/// |-----------------------|--------|--------------------------|
/// | 4.75, 5.15 kbit/s     | 2      | 1-3, 4-6, 7              |
/// | 5.90 kbit/s           | 2      | 1, 2-4, 5-6, 7-9         |
/// | 6.70 kbit/s           | 3      | 1-3, 4, 5-7, 8, 9-11     |
/// | 7.40, 7.95 kbit/s     | 4      | 1-3, 4-6, 7-9, 10, 11-13 |
fn decode_fixed_sparse(fixed_sparse: &mut Fixed, pulses: &[u16; 10], mode: u8, subframe: usize) {
    if mode == MODE_12K2 {
        decode_10_pulses_35bits(pulses, fixed_sparse);
    }
    else if mode == MODE_10K2 {
        decode_8_pulses_31bits(pulses, fixed_sparse);
    }
    else {
        let fixed_index = usize::from(pulses[0]);
        let pos = &mut fixed_sparse.x;

        if mode <= MODE_5K15 {
            let pulse_subset = ((fixed_index >> 3) & 8) + (subframe << 1);
            pos[0] = (fixed_index & 7) * 5 + usize::from(TRACK_POSITION[pulse_subset]);
            pos[1] = ((fixed_index >> 3) & 7) * 5 + usize::from(TRACK_POSITION[pulse_subset + 1]);
            fixed_sparse.n = 2;
        }
        else if mode == MODE_5K9 {
            let pulse_subset = ((fixed_index & 1) << 1) + 1;
            pos[0] = ((fixed_index >> 1) & 7) * 5 + pulse_subset;
            let pulse_subset = (fixed_index >> 4) & 3;
            pos[1] = ((fixed_index >> 6) & 7) * 5 + pulse_subset + usize::from(pulse_subset == 3);
            fixed_sparse.n = if pos[0] == pos[1] { 1 } else { 2 };
        }
        else if mode == MODE_6K7 {
            pos[0] = (fixed_index & 7) * 5;
            let pulse_subset = (fixed_index >> 2) & 2;
            pos[1] = ((fixed_index >> 4) & 7) * 5 + pulse_subset + 1;
            let pulse_subset = (fixed_index >> 6) & 2;
            pos[2] = ((fixed_index >> 8) & 7) * 5 + pulse_subset + 2;
            fixed_sparse.n = 3;
        }
        else {
            // Up to 7.95 kbit/s.
            pos[0] = usize::from(GRAY_DECODE[fixed_index & 7]);
            pos[1] = usize::from(GRAY_DECODE[(fixed_index >> 3) & 7]) + 1;
            pos[2] = usize::from(GRAY_DECODE[(fixed_index >> 6) & 7]) + 2;
            let pulse_subset = (fixed_index >> 9) & 1;
            pos[3] = usize::from(GRAY_DECODE[(fixed_index >> 10) & 7]) + pulse_subset + 3;
            fixed_sparse.n = 4;
        }
        for i in 0..fixed_sparse.n {
            fixed_sparse.y[i] = if (pulses[1] >> i) & 1 != 0 { 1.0 } else { -1.0 };
        }
    }
}

/// Circularly convolves a sparse fixed vector with a phase dispersion impulse response filter
/// (D.6.2 of G.729 and 6.1.5 of AMR): `out[n] = sum_i in[i] * filter[(len + n - i) % len]`.
fn apply_ir_filter(out: &mut [f32; SUBFRAME], inp: &Fixed, filter: &[f32; SUBFRAME]) {
    let mut filter1 = [0.0f32; SUBFRAME]; // filters at pitch lag * 1 and * 2
    let mut filter2 = [0.0f32; SUBFRAME];
    let lag = inp.pitch_lag;
    let fac = inp.pitch_fac;

    if lag < SUBFRAME {
        circ_add(&mut filter1, filter, filter, lag, fac);

        if lag < SUBFRAME >> 1 {
            circ_add(&mut filter2, filter, &filter1, lag, fac);
        }
    }

    out.fill(0.0);
    for i in 0..inp.n {
        let x = inp.x[i];
        let y = inp.y[i];

        let filterp = if x >= SUBFRAME - lag.min(SUBFRAME) {
            filter
        }
        else if x >= SUBFRAME.saturating_sub(lag << 1) {
            &filter1
        }
        else {
            &filter2
        };

        circ_add_acc(out, filterp, x, y);
    }
}

/// Computes the fixed gain from the gain correction factor and the predicted energy
/// (`ff_amr_set_fixed_gain`, equations 66-69 of 3GPP TS 26.090), and updates the history of
/// quantified prediction error energies.
fn set_fixed_gain(
    fixed_gain_factor: f32,
    fixed_mean_energy: f32,
    prediction_error: &mut [f32; 4],
    energy_mean: f32,
) -> f32 {
    // ^g_c = ^gamma_gc * 100.05 (predicted dB + mean dB - dB of the fixed vector).
    // Note 10^(0.05 * -10log(average x2)) = 1/sqrt(average x2).
    let exponent = 0.05 * f64::from(dot(&ENERGY_PRED_FAC, prediction_error) + energy_mean);
    let exp10 = (std::f64::consts::LOG2_10 * exponent).exp2();
    let denom = if fixed_mean_energy != 0.0 { fixed_mean_energy } else { 1.0 };
    let val = (f64::from(fixed_gain_factor) * exp10 / f64::from(denom.sqrt())) as f32;

    // Update the quantified prediction error energy history.
    prediction_error.copy_within(1..4, 0);
    prediction_error[3] = (20.0 * f64::from(fixed_gain_factor.log10())) as f32;

    val
}
