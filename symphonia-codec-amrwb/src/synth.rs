// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AMR-WB core frame synthesis: ACELP low band at 12.8 kHz, 5/4 upsampling to 16 kHz and the
//! high band (6.4 kHz to 7 kHz) noise extension, in 32-bit floating point.
//!
//! This is a port of the floating-point decoder in FFmpeg's `libavcodec/amrwbdec.c` (and the
//! `celp_*`, `acelp_*` helpers it uses). The algorithm is 3GPP TS 26.190. Where FFmpeg computes
//! in `double` (C integer-to-double promotions of float expressions) the same promotions are
//! kept so that the output tracks FFmpeg closely.

use core::f64::consts::LOG2_10;

use crate::lsp::{amrwb_lsp2lpc, lsf2lspd, set_min_dist_lsf};
use crate::params::{
    Frame, P_DELAY_MAX, SubFrame, decode_fixed_vector, decode_pitch_lag_high, decode_pitch_lag_low,
};
use crate::tables::*;

/// Order of the linear prediction filter at 12.8 kHz.
const LP_ORDER: usize = 16;
/// Order of the extended linear prediction filter at 16 kHz.
const LP_ORDER_16K: usize = 20;
/// Amount of past data needed by the high-band filters.
const HB_FIR_SIZE: usize = 30;
/// Upsampling filter length.
const UPS_FIR_SIZE: usize = 12;
/// Upsampling memory length.
const UPS_MEM_SIZE: usize = 2 * UPS_FIR_SIZE;

/// Minimum ISF gap.
const MIN_ISF_SPACING: f64 = 128.0 / 32768.0;
/// ISF prediction factor.
const PRED_FACTOR: f64 = 1.0 / 3.0;
/// Initial innovation energy (dB).
const MIN_ENERGY: f32 = -14.0;
/// Mean innovation energy (dB) in all modes.
const ENERGY_MEAN: f32 = 30.0;
/// De-emphasis factor.
const PREEMPH_FAC: f32 = 0.68;

/// Samples per subframe at 12.8 kHz.
const SFR_SIZE: usize = 64;
/// Samples per subframe at 16 kHz.
const SFR_SIZE_16K: usize = 80;

/// Offset of the current excitation inside `excitation_buf`.
const EXC: usize = P_DELAY_MAX as usize + LP_ORDER + 1;
/// Length of the excitation buffer: history, current subframe and one extra look-ahead sample.
const EXC_BUF_LEN: usize = EXC + 1 + SFR_SIZE;

/// Samples per frame at 16 kHz.
pub const FRAME_SAMPLES: usize = 4 * SFR_SIZE_16K;

/// Mode index of the 6.60 kbit/s mode.
const MODE_6K60: usize = 0;
/// Mode index of the 8.85 kbit/s mode.
const MODE_8K85: usize = 1;
/// Mode index of the 23.85 kbit/s mode.
const MODE_23K85: usize = 8;

/// Energy prediction coefficients, in reverse order.
const ENERGY_PRED_FAC: [f32; 4] = [0.2, 0.3, 0.4, 0.5];
/// ISF/ISP interpolation coefficients for each subframe.
const ISFP_INTER: [f32; 4] = [0.45, 0.8, 0.96, 1.0];

/// High-pass filter coefficients for the 31 Hz and 400 Hz cutoffs.
const HPF_ZEROS: [f32; 2] = [-2.0, 1.0];
const HPF_31_POLES: [f32; 2] = [-1.978_881_8, 0.979_126];
const HPF_31_GAIN: f32 = 0.989_501_95;
const HPF_400_POLES: [f32; 2] = [-1.787_109_4, 0.864_257_8];
const HPF_400_GAIN: f32 = 0.893_554_7;

/// Additive lagged Fibonacci generator, identical to FFmpeg's `AVLFG` (`libavutil/lfg.c`).
#[derive(Clone)]
struct Lfg {
    state: [u32; 64],
    index: u32,
}

impl Lfg {
    fn new() -> Self {
        Lfg { state: LFG_INITIAL_STATE, index: 0 }
    }

    fn next(&mut self) -> u32 {
        let i = self.index as usize;
        let a = self.state[(i.wrapping_sub(24)) & 63]
            .wrapping_add(self.state[(i.wrapping_sub(55)) & 63]);
        self.state[i & 63] = a;
        self.index = self.index.wrapping_add(1);
        a
    }
}

#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut p = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        p += x * y;
    }
    p
}

/// Scales `v` so that its sum of squares equals `sum_of_squares`
/// (`ff_scale_vector_to_given_sum_of_squares`).
fn scale_to_sum_of_squares(v: &mut [f32], sum_of_squares: f32) {
    let mut scale = dot(v, v);
    if scale != 0.0 {
        scale = f64::from(sum_of_squares / scale).sqrt() as f32;
    }
    for x in v.iter_mut() {
        *x *= scale;
    }
}

/// Direct-form LP synthesis filter: `out[n] = in[n] - sum(coef[i-1] * out[n-i])`. The output is
/// written to `buf[start..start + input.len()]` and `buf[..start]` holds the filter memory
/// (`ff_celp_lp_synthesis_filterf`).
fn lp_synthesis(buf: &mut [f32], start: usize, coef: &[f32], input: &[f32]) {
    for (n, &x) in input.iter().enumerate() {
        let mut o = x;
        for (i, &c) in coef.iter().enumerate() {
            o -= c * buf[start + n - 1 - i];
        }
        buf[start + n] = o;
    }
}

/// Applies the transfer function `(1 + z0 z^-1 + z1 z^-2) / (1 + p0 z^-1 + p1 z^-2)` scaled by
/// `gain` (`ff_acelp_apply_order_2_transfer_function`).
fn order2_filter(
    out: &mut [f32],
    inp: &[f32],
    zeros: &[f32; 2],
    poles: &[f32; 2],
    gain: f32,
    mem: &mut [f32; 2],
) {
    for (o, &x) in out.iter_mut().zip(inp) {
        let tmp = gain * x - poles[0] * mem[0] - poles[1] * mem[1];
        *o = tmp + zeros[0] * mem[0] + zeros[1] * mem[1];
        mem[1] = mem[0];
        mem[0] = tmp;
    }
}

/// `exp10` as computed by FFmpeg (`ff_exp10`).
fn exp10(x: f64) -> f64 {
    (LOG2_10 * x).exp2()
}

/// Interpolates the past excitation at the pitch lag with a Hamming windowed sinc function
/// (`ff_acelp_interpolatef` with a precision of 4 and a filter length of 16). The result of
/// 65 samples is written to `buf[EXC..]`; `buf[in_start..]` is the interpolation source, which
/// may overlap the output (repetition of the pitch cycle).
fn interpolate_pitch(buf: &mut [f32; EXC_BUF_LEN], in_start: usize, frac_pos: usize) {
    for n in 0..=SFR_SIZE {
        let mut idx = 0;
        let mut v = 0.0f32;
        for i in 1..=LP_ORDER {
            v += buf[in_start + n + i - 1] * AC_INTER[idx + frac_pos];
            idx += 4;
            v += buf[in_start + n - i] * AC_INTER[idx - frac_pos];
        }
        buf[EXC + n] = v;
    }
}

/// Decoder state of the AMR-WB core (one channel).
#[derive(Clone)]
pub struct Core {
    /// Working ISF vector of the current frame.
    isf_cur: [f32; LP_ORDER],
    /// Quantized ISF vector of the previous frame.
    isf_q_past: [f32; LP_ORDER],
    /// Final processed ISF vector of the previous frame.
    isf_past_final: [f32; LP_ORDER],
    /// ISP vectors of the current frame.
    isp: [[f64; LP_ORDER]; 4],
    /// ISP vector for the fourth subframe of the previous frame.
    isp_sub4_past: [f64; LP_ORDER],
    /// Linear prediction coefficients of the four subframes.
    lp_coef: [[f32; LP_ORDER]; 4],

    /// Integer part of the pitch lag for the next relative subframe.
    base_pitch_lag: i32,
    /// Integer part of the pitch lag of the previous subframe.
    pitch_lag_int: usize,

    /// Current excitation and its history; the current excitation starts at `EXC`.
    excitation_buf: [f32; EXC_BUF_LEN],
    pitch_vector: [f32; SFR_SIZE],
    fixed_vector: [f32; SFR_SIZE],

    /// Quantified prediction errors `20 log10(gamma_gc)` of the previous four subframes.
    prediction_error: [f32; 4],
    /// Pitch gains of the current and the previous five subframes.
    pitch_gain: [f32; 6],
    /// Fixed gains of the current and the previous subframes.
    fixed_gain: [f32; 2],
    /// Tilt coefficient related to the voicing of the previous subframe.
    tilt_coef: f32,

    /// Previous impulse response filter number: 0 strong, 1 medium, 2 none.
    prev_ir_filter_nr: u8,
    /// Previous initial gain used by the noise enhancer.
    prev_tr_gain: f32,

    /// Low-band samples and memory from synthesis at 12.8 kHz.
    samples_az: [f32; LP_ORDER + SFR_SIZE],
    /// Low-band samples and memory processed for upsampling.
    samples_up: [f32; UPS_MEM_SIZE + SFR_SIZE],
    /// High-band samples and memory from synthesis at 16 kHz.
    samples_hb: [f32; LP_ORDER_16K + SFR_SIZE_16K],

    hpf_31_mem: [f32; 2],
    hpf_400_mem: [f32; 2],
    demph_mem: f32,
    bpf_6_7_mem: [f32; HB_FIR_SIZE],
    lpf_7_mem: [f32; HB_FIR_SIZE],

    /// White noise generator for the high-band excitation.
    prng: Lfg,
    first_frame: bool,
}

impl Core {
    pub fn new() -> Self {
        let mut isf_past_final = [0.0f32; LP_ORDER];
        for (d, &s) in isf_past_final.iter_mut().zip(ISF_INIT.iter()) {
            *d = f32::from(s) * (1.0 / 32768.0);
        }

        Core {
            isf_cur: [0.0; LP_ORDER],
            isf_q_past: [0.0; LP_ORDER],
            isf_past_final,
            isp: [[0.0; LP_ORDER]; 4],
            isp_sub4_past: [0.0; LP_ORDER],
            lp_coef: [[0.0; LP_ORDER]; 4],
            base_pitch_lag: 0,
            pitch_lag_int: 0,
            excitation_buf: [0.0; EXC_BUF_LEN],
            pitch_vector: [0.0; SFR_SIZE],
            fixed_vector: [0.0; SFR_SIZE],
            prediction_error: [MIN_ENERGY; 4],
            pitch_gain: [0.0; 6],
            fixed_gain: [0.0; 2],
            tilt_coef: 0.0,
            prev_ir_filter_nr: 0,
            prev_tr_gain: 0.0,
            samples_az: [0.0; LP_ORDER + SFR_SIZE],
            samples_up: [0.0; UPS_MEM_SIZE + SFR_SIZE],
            samples_hb: [0.0; LP_ORDER_16K + SFR_SIZE_16K],
            hpf_31_mem: [0.0; 2],
            hpf_400_mem: [0.0; 2],
            demph_mem: 0.0,
            bpf_6_7_mem: [0.0; HB_FIR_SIZE],
            lpf_7_mem: [0.0; HB_FIR_SIZE],
            prng: Lfg::new(),
            first_frame: true,
        }
    }

    /// Decodes a speech frame of mode 0 to 8 into 320 samples (scaled to +-1.0).
    pub fn decode_frame(&mut self, mode: usize, frame: &Frame, out: &mut [f32; FRAME_SAMPLES]) {
        // Decode the quantized ISF vector.
        if mode == MODE_6K60 {
            decode_isf_indices_36b(&frame.isp_id, &mut self.isf_cur);
        }
        else {
            decode_isf_indices_46b(&frame.isp_id, &mut self.isf_cur);
        }

        isf_add_mean_and_past(&mut self.isf_cur, &mut self.isf_q_past);
        set_min_dist_lsf(&mut self.isf_cur[..LP_ORDER - 1], MIN_ISF_SPACING);

        let stab_fac = stability_factor(&self.isf_cur, &self.isf_past_final);

        self.isf_cur[LP_ORDER - 1] *= 2.0;
        lsf2lspd(&mut self.isp[3], &self.isf_cur);

        // Generate an ISP vector for each subframe.
        if self.first_frame {
            self.first_frame = false;
            self.isp_sub4_past = self.isp[3];
        }
        interpolate_isp(&mut self.isp, &self.isp_sub4_past);

        for sub in 0..4 {
            amrwb_lsp2lpc(&self.isp[sub], &mut self.lp_coef[sub], LP_ORDER);
        }

        for (sub, (cur, sub_buf)) in
            frame.subframe.iter().zip(out.chunks_exact_mut(SFR_SIZE_16K)).enumerate()
        {
            self.decode_subframe(mode, sub, cur, frame.vad, stab_fac, sub_buf);
        }

        // Update the state for the next frame.
        self.isp_sub4_past = self.isp[3];
        self.isf_past_final = self.isf_cur;
    }

    fn decode_subframe(
        &mut self,
        mode: usize,
        sub: usize,
        cur: &SubFrame,
        vad: u16,
        stab_fac: f32,
        sub_buf: &mut [f32],
    ) {
        // Decode the adaptive codebook (pitch vector).
        self.decode_pitch_vector(mode, cur, sub);
        // Decode the innovative codebook (fixed vector).
        decode_fixed_vector(&mut self.fixed_vector, cur, mode);

        self.pitch_sharpening();

        // Decode the gains.
        let (pitch_gain, fixed_gain_factor) = decode_gains(cur.vq_gain, mode);
        self.pitch_gain[0] = pitch_gain;

        let mean_energy = dot(&self.fixed_vector, &self.fixed_vector) / SFR_SIZE as f32;
        self.fixed_gain[0] = self.set_fixed_gain(fixed_gain_factor, mean_energy);

        // Calculate the voice factor and store the tilt for the next subframe.
        let voice_fac = voice_factor(
            &self.pitch_vector,
            self.pitch_gain[0],
            &self.fixed_vector,
            self.fixed_gain[0],
        );
        self.tilt_coef = (f64::from(voice_fac) * 0.25 + 0.25) as f32;

        // Construct the current excitation.
        for i in 0..SFR_SIZE {
            let e = &mut self.excitation_buf[EXC + i];
            *e *= self.pitch_gain[0];
            *e += self.fixed_gain[0] * self.fixed_vector[i];
            *e = e.trunc();
        }

        // Post-processing of the excitation elements.
        let synth_fixed_gain =
            noise_enhancer(self.fixed_gain[0], &mut self.prev_tr_gain, voice_fac, stab_fac);

        // The fixed vector is not needed after this subframe, so the (possibly smoothed) copy used
        // by the synthesis can be modified freely.
        let mut spare_vector = [0.0f32; SFR_SIZE];
        let mut synth_fixed_vector = if self.anti_sparseness(mode, &mut spare_vector) {
            spare_vector
        }
        else {
            self.fixed_vector
        };

        pitch_enhancer(&mut synth_fixed_vector, voice_fac);

        // Synthesis.
        let mut synth_exc = [0.0f32; SFR_SIZE];
        self.synthesis(mode, sub, &mut synth_exc, synth_fixed_gain, &synth_fixed_vector);

        // Synthesis speech post-processing.
        self.de_emphasis();

        {
            let tmp: [f32; SFR_SIZE] = self.samples_up[UPS_MEM_SIZE..].try_into().expect("size");
            order2_filter(
                &mut self.samples_up[UPS_MEM_SIZE..],
                &tmp,
                &HPF_ZEROS,
                &HPF_31_POLES,
                HPF_31_GAIN,
                &mut self.hpf_31_mem,
            );
        }

        upsample_5_4(sub_buf, &self.samples_up);

        // High frequency band (6.4 kHz to 7.0 kHz) generation.
        let mut hb_samples = [0.0f32; SFR_SIZE_16K];
        order2_filter(
            &mut hb_samples[..SFR_SIZE],
            &self.samples_up[UPS_MEM_SIZE..],
            &HPF_ZEROS,
            &HPF_400_POLES,
            HPF_400_GAIN,
            &mut self.hpf_400_mem,
        );

        let hb_gain = self.find_hb_gain(mode, &hb_samples[..SFR_SIZE], cur.hb_gain, vad);

        let mut hb_exc = [0.0f32; SFR_SIZE_16K];
        self.scaled_hb_excitation(&mut hb_exc, &synth_exc, hb_gain);

        self.hb_synthesis(mode, sub, &hb_exc);

        // High-band post-processing filters.
        let mut hb_in = [0.0f32; SFR_SIZE_16K];
        hb_in.copy_from_slice(&self.samples_hb[LP_ORDER_16K..]);
        hb_fir_filter(&mut hb_samples, &BPF_6_7_COEF, &mut self.bpf_6_7_mem, &hb_in);

        if mode == MODE_23K85 {
            let tmp = hb_samples;
            hb_fir_filter(&mut hb_samples, &LPF_7_COEF, &mut self.lpf_7_mem, &tmp);
        }

        // Add the low and high frequency bands.
        for (s, &h) in sub_buf.iter_mut().zip(hb_samples.iter()) {
            *s = (*s + h) * (1.0 / 32768.0);
        }

        self.update_sub_state();
    }

    /// Finds the pitch vector by interpolating the past excitation at the pitch lag
    /// (TS 26.190 clause 6.1.1).
    fn decode_pitch_vector(&mut self, mode: usize, sub: &SubFrame, subframe: usize) {
        let adap = i32::from(sub.adap);
        let (lag_int, lag_frac) = if mode <= MODE_8K85 {
            decode_pitch_lag_low(adap, &mut self.base_pitch_lag, subframe, mode == MODE_6K60)
        }
        else {
            decode_pitch_lag_high(adap, &mut self.base_pitch_lag, subframe)
        };

        // The lags are within [34, 232] by construction of the index field widths (the relative
        // lags add at most 16 to a base of at most 216, plus one if the fraction is positive), so
        // the interpolation source `EXC + 1 - lag - 16` never underflows.
        self.pitch_lag_int = lag_int as usize;
        let pitch_lag_int = lag_int + i32::from(lag_frac > 0);

        // Interpolate at the pitch lag.
        let frac_pos = lag_frac + if lag_frac > 0 { 0 } else { 4 };
        interpolate_pitch(
            &mut self.excitation_buf,
            EXC + 1 - pitch_lag_int as usize,
            frac_pos as usize,
        );

        // Check which pitch signal path should be used: the 6.60 and 8.85 kbit/s modes have the
        // ltp flag set to 0.
        if sub.ltp != 0 {
            self.pitch_vector.copy_from_slice(&self.excitation_buf[EXC..EXC + SFR_SIZE]);
        }
        else {
            let exc = &self.excitation_buf;
            for i in 0..SFR_SIZE {
                self.pitch_vector[i] = (0.18 * f64::from(exc[EXC + i - 1])
                    + 0.64 * f64::from(exc[EXC + i])
                    + 0.18 * f64::from(exc[EXC + i + 1]))
                    as f32;
            }
            self.excitation_buf[EXC..EXC + SFR_SIZE].copy_from_slice(&self.pitch_vector);
        }
    }

    /// Applies the pitch sharpening filters to the fixed codebook vector (TS 26.190 clause
    /// 6.1.2). The specification states that this is applied when the pitch lag is less than 64,
    /// but the reference code (and FFmpeg) always applies it.
    fn pitch_sharpening(&mut self) {
        let fv = &mut self.fixed_vector;

        // Tilt part.
        for i in (1..SFR_SIZE).rev() {
            fv[i] -= fv[i - 1] * self.tilt_coef;
        }

        // Periodicity enhancement part.
        for i in self.pitch_lag_int..SFR_SIZE {
            fv[i] = (f64::from(fv[i]) + f64::from(fv[i - self.pitch_lag_int]) * 0.85) as f32;
        }
    }

    /// Computes the fixed gain from the correction factor and the MA-predicted innovation energy
    /// and updates the prediction error history (`ff_amr_set_fixed_gain`; TS 26.190 clause
    /// 6.1.3).
    fn set_fixed_gain(&mut self, fixed_gain_factor: f32, fixed_mean_energy: f32) -> f32 {
        // ^g_c = ^gamma_gc * 10^(0.05 * (predicted dB + mean dB - dB of the fixed vector)). Note
        // 10^(0.05 * -10log(average x^2)) = 1 / sqrt(average x^2).
        let pred = dot(&ENERGY_PRED_FAC, &self.prediction_error);
        let energy = if fixed_mean_energy != 0.0 { fixed_mean_energy } else { 1.0 };
        let val = f64::from(fixed_gain_factor) * exp10(0.05 * f64::from(pred + ENERGY_MEAN))
            / f64::from(energy.sqrt());

        // Update the quantified prediction error energy history.
        self.prediction_error.copy_within(1.., 0);
        self.prediction_error[3] = (20.0 * f64::from(fixed_gain_factor.log10())) as f32;

        val as f32
    }

    /// Reduces the fixed vector sparseness by smoothing it with one of three impulse response
    /// filters ("adaptive phase dispersion", TS 26.190 clause 6.1.2). Returns true if the
    /// filtered vector was written to `buf`, in which case it replaces the fixed vector.
    fn anti_sparseness(&mut self, mode: usize, buf: &mut [f32; SFR_SIZE]) -> bool {
        // No filtering in the higher modes.
        if mode > MODE_8K85 {
            return false;
        }

        let pitch_gain0 = f64::from(self.pitch_gain[0]);
        let mut ir_filter_nr = if pitch_gain0 < 0.6 {
            0 // Strong filtering.
        }
        else if pitch_gain0 < 0.9 {
            1 // Medium filtering.
        }
        else {
            2 // No filtering.
        };

        // Detect an "onset".
        if f64::from(self.fixed_gain[0]) > 3.0 * f64::from(self.fixed_gain[1]) {
            if ir_filter_nr < 2 {
                ir_filter_nr += 1;
            }
        }
        else {
            let count = self.pitch_gain.iter().filter(|&&g| f64::from(g) < 0.6).count();

            if count > 2 {
                ir_filter_nr = 0;
            }

            if ir_filter_nr > self.prev_ir_filter_nr + 1 {
                ir_filter_nr -= 1;
            }
        }

        // Update the impulse response filter strength history.
        self.prev_ir_filter_nr = ir_filter_nr;

        ir_filter_nr += u8::from(mode == MODE_8K85);

        if ir_filter_nr >= 2 {
            return false;
        }

        let coef = if ir_filter_nr == 0 { &IR_FILTER_STR } else { &IR_FILTER_MID };

        // Circular convolution: c2(n) = sum(i, 0, len-1) { c(i) * coef((n - i + len) % len) }.
        buf.fill(0.0);
        for (i, &fac) in self.fixed_vector.iter().enumerate() {
            if fac != 0.0 {
                // `ff_celp_circ_addf`.
                for k in 0..i {
                    buf[k] += fac * coef[SFR_SIZE + k - i];
                }
                for k in i..SFR_SIZE {
                    buf[k] += fac * coef[k - i];
                }
            }
        }

        true
    }

    /// Conducts the 16th order LP synthesis from the excitation (TS 26.190 clause 6.1.3).
    fn synthesis(
        &mut self,
        mode: usize,
        sub: usize,
        excitation: &mut [f32; SFR_SIZE],
        fixed_gain: f32,
        fixed_vector: &[f32; SFR_SIZE],
    ) {
        for i in 0..SFR_SIZE {
            excitation[i] =
                self.pitch_gain[0] * self.pitch_vector[i] + fixed_gain * fixed_vector[i];
        }

        // Emphasize the pitch vector contribution in the low bit rate modes.
        if f64::from(self.pitch_gain[0]) > 0.5 && mode <= MODE_8K85 {
            let energy = dot(excitation, excitation);

            // Weird part in both the reference code and the specification: an unknown parameter
            // {beta} seems to be identical to the current pitch gain.
            let pitch_factor =
                (0.25 * f64::from(self.pitch_gain[0]) * f64::from(self.pitch_gain[0])) as f32;

            for i in 0..SFR_SIZE {
                excitation[i] += pitch_factor * self.pitch_vector[i];
            }

            scale_to_sum_of_squares(excitation, energy);
        }

        lp_synthesis(&mut self.samples_az, LP_ORDER, &self.lp_coef[sub], excitation);
    }

    /// Applies the de-emphasis filter `1 / (1 - m z^-1)` to the synthesis.
    fn de_emphasis(&mut self) {
        let inp = &self.samples_az[LP_ORDER..];
        let out = &mut self.samples_up[UPS_MEM_SIZE..];

        out[0] = inp[0] + PREEMPH_FAC * self.demph_mem;
        for i in 1..SFR_SIZE {
            out[i] = inp[i] + out[i - 1] * PREEMPH_FAC;
        }
        self.demph_mem = out[SFR_SIZE - 1];
    }

    /// Calculates the high-band gain from the encoded index (23.85 kbit/s) or from the low-band
    /// speech signal and the voice activity flag (TS 26.190 clause 6.3.2).
    fn find_hb_gain(&self, mode: usize, synth: &[f32], hb_idx: u16, vad: u16) -> f32 {
        if mode == MODE_23K85 {
            return f32::from(QUA_HB_GAIN[usize::from(hb_idx)]) * (1.0 / 16384.0);
        }

        let wsp = f64::from(u8::from(vad > 0));
        let tmp = dot(&synth[..SFR_SIZE - 1], &synth[1..]);
        let tilt = if tmp > 0.0 { tmp / dot(synth, synth) } else { 0.0 };

        // The gain is bounded by [0.1, 1.0].
        (((1.0 - f64::from(tilt)) * (1.25 - 0.25 * wsp)) as f32).clamp(0.1, 1.0)
    }

    /// Generates the high-band white-noise excitation with the same energy as the low-band one,
    /// scaled by the given gain.
    fn scaled_hb_excitation(
        &mut self,
        hb_exc: &mut [f32; SFR_SIZE_16K],
        synth_exc: &[f32; SFR_SIZE],
        hb_gain: f32,
    ) {
        let energy = dot(synth_exc, synth_exc);

        for e in hb_exc.iter_mut() {
            *e = (32768.0 - f64::from(self.prng.next() as u16)) as f32;
        }

        scale_to_sum_of_squares(hb_exc, energy * hb_gain * hb_gain);
    }

    /// Conducts the 16 kHz high-band synthesis of the white-noise excitation: a 20th order filter
    /// extrapolated from the ISFs in the 6.60 kbit/s mode, the weighted low-band filter in the
    /// other modes (TS 26.190 clauses 6.3.2 and 6.3.3). The samples are appended to
    /// `samples_hb`.
    fn hb_synthesis(&mut self, mode: usize, sub: usize, exc: &[f32; SFR_SIZE_16K]) {
        let mut hb_lpc = [0.0f32; LP_ORDER_16K];

        if mode == MODE_6K60 {
            // ISF vector for extrapolation.
            let mut e_isf = [0.0f32; LP_ORDER_16K];
            let c = ISFP_INTER[sub];
            let wb = (1.0 - f64::from(c)) as f32;
            for i in 0..LP_ORDER {
                e_isf[i] = c * self.isf_past_final[i] + wb * self.isf_cur[i];
            }

            extrapolate_isf(&mut e_isf);

            e_isf[LP_ORDER_16K - 1] *= 2.0;
            let mut e_isp = [0.0f64; LP_ORDER_16K];
            lsf2lspd(&mut e_isp, &e_isf);
            amrwb_lsp2lpc(&e_isp, &mut hb_lpc, LP_ORDER_16K);

            lpc_weighting(&mut hb_lpc, 0.9);
        }
        else {
            hb_lpc[..LP_ORDER].copy_from_slice(&self.lp_coef[sub]);
            lpc_weighting(&mut hb_lpc[..LP_ORDER], 0.6);
        }

        let order = if mode == MODE_6K60 { LP_ORDER_16K } else { LP_ORDER };
        lp_synthesis(&mut self.samples_hb, LP_ORDER_16K, &hb_lpc[..order], exc);
    }

    /// Updates the state before the next subframe.
    fn update_sub_state(&mut self) {
        self.excitation_buf.copy_within(SFR_SIZE..SFR_SIZE + EXC, 0);

        self.pitch_gain.copy_within(0..5, 1);
        self.fixed_gain[1] = self.fixed_gain[0];

        self.samples_az.copy_within(SFR_SIZE..SFR_SIZE + LP_ORDER, 0);
        self.samples_up.copy_within(SFR_SIZE..SFR_SIZE + UPS_MEM_SIZE, 0);
        self.samples_hb.copy_within(SFR_SIZE_16K..SFR_SIZE_16K + LP_ORDER_16K, 0);
    }
}

/// Decodes the quantized ISF vector using the 36 bit indices (6.60 kbit/s mode only).
fn decode_isf_indices_36b(ind: &[u16; 7], isf_q: &mut [f32; LP_ORDER]) {
    const S: f32 = 1.0 / 32768.0;
    let ind = |i: usize| usize::from(ind[i]);

    for i in 0..9 {
        isf_q[i] = f32::from(DICO1_ISF[ind(0)][i]) * S;
    }
    for i in 0..7 {
        isf_q[i + 9] = f32::from(DICO2_ISF[ind(1)][i]) * S;
    }
    for i in 0..5 {
        isf_q[i] += f32::from(DICO21_ISF_36B[ind(2)][i]) * S;
    }
    for i in 0..4 {
        isf_q[i + 5] += f32::from(DICO22_ISF_36B[ind(3)][i]) * S;
    }
    for i in 0..7 {
        isf_q[i + 9] += f32::from(DICO23_ISF_36B[ind(4)][i]) * S;
    }
}

/// Decodes the quantized ISF vector using the 46 bit indices (all modes but 6.60 kbit/s).
fn decode_isf_indices_46b(ind: &[u16; 7], isf_q: &mut [f32; LP_ORDER]) {
    const S: f32 = 1.0 / 32768.0;
    let ind = |i: usize| usize::from(ind[i]);

    for i in 0..9 {
        isf_q[i] = f32::from(DICO1_ISF[ind(0)][i]) * S;
    }
    for i in 0..7 {
        isf_q[i + 9] = f32::from(DICO2_ISF[ind(1)][i]) * S;
    }
    for i in 0..3 {
        isf_q[i] += f32::from(DICO21_ISF[ind(2)][i]) * S;
    }
    for i in 0..3 {
        isf_q[i + 3] += f32::from(DICO22_ISF[ind(3)][i]) * S;
    }
    for i in 0..3 {
        isf_q[i + 6] += f32::from(DICO23_ISF[ind(4)][i]) * S;
    }
    for i in 0..3 {
        isf_q[i + 9] += f32::from(DICO24_ISF[ind(5)][i]) * S;
    }
    for i in 0..4 {
        isf_q[i + 12] += f32::from(DICO25_ISF[ind(6)][i]) * S;
    }
}

/// Applies the mean and the past ISF values using the prediction factor and updates the past ISF
/// vector.
fn isf_add_mean_and_past(isf_q: &mut [f32; LP_ORDER], isf_past: &mut [f32; LP_ORDER]) {
    for i in 0..LP_ORDER {
        let tmp = isf_q[i];
        isf_q[i] += f32::from(ISF_MEAN[i]) * (1.0 / 32768.0);
        isf_q[i] = (f64::from(isf_q[i]) + PRED_FACTOR * f64::from(isf_past[i])) as f32;
        isf_past[i] = tmp;
    }
}

/// Interpolates the fourth ISP vector between the current and the past frames to obtain an ISP
/// vector for each subframe.
fn interpolate_isp(isp_q: &mut [[f64; LP_ORDER]; 4], isp4_past: &[f64; LP_ORDER]) {
    for k in 0..3 {
        let c = f64::from(ISFP_INTER[k]);
        for i in 0..LP_ORDER {
            isp_q[k][i] = (1.0 - c) * isp4_past[i] + c * isp_q[3][i];
        }
    }
}

/// Decodes the pitch and fixed gain correction factor from the VQ index.
fn decode_gains(vq_gain: u16, mode: usize) -> (f32, f32) {
    let gains = if mode <= MODE_8K85 {
        QUA_GAIN_6B[usize::from(vq_gain)]
    }
    else {
        QUA_GAIN_7B[usize::from(vq_gain)]
    };

    (f32::from(gains[0]) * (1.0 / 16384.0), f32::from(gains[1]) * (1.0 / 2048.0))
}

/// Calculates the voice factor (-1.0 = unvoiced to 1.0 = voiced).
fn voice_factor(p_vector: &[f32], p_gain: f32, f_vector: &[f32], f_gain: f32) -> f32 {
    let p_ener = f64::from(dot(p_vector, p_vector)) * f64::from(p_gain) * f64::from(p_gain);
    let f_ener = f64::from(dot(f_vector, f_vector)) * f64::from(f_gain) * f64::from(f_gain);

    ((p_ener - f_ener) / (p_ener + f_ener + 0.01)) as f32
}

/// Calculates a stability factor based on the distance between the current and the past ISF. A
/// value of 1 shows maximum signal stability.
fn stability_factor(isf: &[f32; LP_ORDER], isf_past: &[f32; LP_ORDER]) -> f32 {
    let mut acc = 0.0f32;

    for i in 0..LP_ORDER - 1 {
        acc += (isf[i] - isf_past[i]) * (isf[i] - isf_past[i]);
    }

    // This part is not so clear from the reference code: the result is more accurate changing
    // the "/ 256" to "* 512".
    (1.25 - f64::from(acc) * 0.8 * 512.0).max(0.0) as f32
}

/// Applies a non-linear fixed gain smoothing to reduce the fluctuation of the excitation energy
/// (TS 26.190 clause 6.1.3).
fn noise_enhancer(fixed_gain: f32, prev_tr_gain: &mut f32, voice_fac: f32, stab_fac: f32) -> f32 {
    let sm_fac = (0.5 * f64::from(1.0 - voice_fac) * f64::from(stab_fac)) as f32;

    // The fixed-point constants used to increment/decrement the gain by 1.5 dB were taken from
    // the reference code.
    let g0 = if fixed_gain < *prev_tr_gain {
        prev_tr_gain.min(fixed_gain + fixed_gain * (6226.0 * (1.0 / 32768.0))) // +1.5 dB.
    }
    else {
        prev_tr_gain.max(fixed_gain * (27536.0 * (1.0 / 32768.0))) // -1.5 dB.
    };

    // Update the threshold of the next frame.
    *prev_tr_gain = g0;

    sm_fac * g0 + (1.0 - sm_fac) * fixed_gain
}

/// Filters the fixed vector to emphasize the higher frequencies.
fn pitch_enhancer(fixed_vector: &mut [f32; SFR_SIZE], voice_fac: f32) {
    let cpe = (0.125 * f64::from(1.0 + voice_fac)) as f32;
    // Holds c(i - 1).
    let mut last = fixed_vector[0];

    fixed_vector[0] -= cpe * fixed_vector[1];

    for i in 1..SFR_SIZE - 1 {
        let cur = fixed_vector[i];

        fixed_vector[i] -= cpe * (last + fixed_vector[i + 1]);
        last = cur;
    }

    fixed_vector[SFR_SIZE - 1] -= cpe * last;
}

/// Upsamples the signal by a ratio of 5/4 (12.8 kHz to 16 kHz) with an FIR interpolation filter.
/// `samples_up` holds the memory followed by the current 64 samples.
fn upsample_5_4(out: &mut [f32], samples_up: &[f32; UPS_MEM_SIZE + SFR_SIZE]) {
    let in_off = UPS_FIR_SIZE;
    let in0 = 1; // in - UPS_FIR_SIZE + 1.
    let mut int_part = 0;
    let mut i = 0;

    for _ in 0..SFR_SIZE_16K / 5 {
        out[i] = samples_up[in_off + int_part];
        let mut frac_part = 4;
        i += 1;

        for _ in 1..5 {
            out[i] = dot(
                &samples_up[in0 + int_part..in0 + int_part + UPS_MEM_SIZE],
                &UPSAMPLE_FIR[4 - frac_part],
            );
            int_part += 1;
            frac_part -= 1;
            i += 1;
        }
    }
}

/// Calculates the autocorrelation of the ISF difference vector.
fn auto_correlation(diff_isf: &[f32], mean: f32, lag: usize) -> f32 {
    let mut sum = 0.0f32;

    for i in 7..LP_ORDER - 2 {
        let prod = (diff_isf[i] - mean) * (diff_isf[i - lag] - mean);
        sum += prod * prod;
    }
    sum
}

/// Extrapolates an ISF vector to the 16 kHz range (20th order LP), used by the 6.60 kbit/s mode
/// high-band filter (TS 26.190 clause 6.3.2). `isf` holds `LP_ORDER` values on input.
fn extrapolate_isf(isf: &mut [f32; LP_ORDER_16K]) {
    let mut diff_isf = [0.0f32; LP_ORDER - 2];
    let mut corr_lag = [0.0f32; 3];

    isf[LP_ORDER_16K - 1] = isf[LP_ORDER - 1];

    // Calculate the difference vector.
    for i in 0..LP_ORDER - 2 {
        diff_isf[i] = isf[i + 1] - isf[i];
    }

    let mut diff_mean = 0.0f32;
    for &d in &diff_isf[2..LP_ORDER - 2] {
        diff_mean += d * (1.0 / (LP_ORDER - 4) as f32);
    }

    // Find which is the maximum autocorrelation.
    let mut i_max_corr = 0;
    for i in 0..3 {
        corr_lag[i] = auto_correlation(&diff_isf, diff_mean, i + 2);

        if corr_lag[i] > corr_lag[i_max_corr] {
            i_max_corr = i;
        }
    }
    i_max_corr += 1;

    for i in LP_ORDER - 1..LP_ORDER_16K - 1 {
        isf[i] = isf[i - 1] + isf[i - 1 - i_max_corr] - isf[i - 2 - i_max_corr];
    }

    // Calculate an estimate for ISF(18) and scale the ISF based on the error.
    let est = 7965.0 + f64::from(isf[2] - isf[3] - isf[4]) / 6.0;
    let scale = (0.5 * (est.min(7600.0) - f64::from(isf[LP_ORDER - 2]))
        / f64::from(isf[LP_ORDER_16K - 2] - isf[LP_ORDER - 2])) as f32;

    for (j, i) in (LP_ORDER - 1..LP_ORDER_16K - 1).enumerate() {
        diff_isf[j] = scale * (isf[i] - isf[i - 1]);
    }

    // Stability insurance.
    for i in 1..LP_ORDER_16K - LP_ORDER {
        if diff_isf[i] + diff_isf[i - 1] < 5.0 {
            if diff_isf[i] > diff_isf[i - 1] {
                diff_isf[i - 1] = 5.0 - diff_isf[i];
            }
            else {
                diff_isf[i] = 5.0 - diff_isf[i - 1];
            }
        }
    }

    for (j, i) in (LP_ORDER - 1..LP_ORDER_16K - 1).enumerate() {
        isf[i] = isf[i - 1] + diff_isf[j] * (1.0 / 32768.0);
    }

    // Scale the ISF vector for 16000 Hz.
    for v in isf[..LP_ORDER_16K - 1].iter_mut() {
        *v = (f64::from(*v) * 0.8) as f32;
    }
}

/// Spectrally expands the LP coefficients: `y[i] = x[i] * gamma^(i+1)`.
fn lpc_weighting(lpc: &mut [f32], gamma: f32) {
    let mut fac = gamma;

    for c in lpc.iter_mut() {
        *c *= fac;
        fac *= gamma;
    }
}

/// Applies a 30th order FIR filter to the high-band samples; `mem` holds the 30 previous input
/// samples. `inp` and `out` may hold the same data (`out` is written after `inp` is read).
fn hb_fir_filter(
    out: &mut [f32; SFR_SIZE_16K],
    fir_coef: &[f32; HB_FIR_SIZE + 1],
    mem: &mut [f32; HB_FIR_SIZE],
    inp: &[f32; SFR_SIZE_16K],
) {
    // Past and current samples.
    let mut data = [0.0f32; SFR_SIZE_16K + HB_FIR_SIZE];
    data[..HB_FIR_SIZE].copy_from_slice(mem);
    data[HB_FIR_SIZE..].copy_from_slice(inp);

    for (i, o) in out.iter_mut().enumerate() {
        let mut acc = 0.0f32;
        for (j, &c) in fir_coef.iter().enumerate() {
            acc += data[i + j] * c;
        }
        *o = acc;
    }

    mem.copy_from_slice(&data[SFR_SIZE_16K..]);
}
