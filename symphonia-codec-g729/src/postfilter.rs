// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! G.729 postfilter (section 4.2) and adaptive gain control (section 4.2.4).
//!
//! Ported from FFmpeg `libavcodec/g729postfilter.c` (non bit-exact build, as used by FFmpeg by
//! default). The arithmetic reproduces the C `int`/`int16_t` semantics of the original, see
//! `celp.rs`.

use crate::SUBFRAME_SIZE;
use crate::celp::{
    PITCH_DELAY_MAX, bidir_sal, clip_i16, interpolate, log2, lp_synthesis_filter,
    lp_synthesis_filter_inplace, scalarproduct, weighted_sum,
};
use crate::tables::{
    FORMANT_PP_FACTOR_DEN_POW, FORMANT_PP_FACTOR_NUM_POW, INTERP_FILT_LONG, INTERP_FILT_SHORT,
};

/// Tilt compensation factor (G.729, k1 > 0), 0.2 in Q15.
const G729_TILT_FACTOR_PLUS: i32 = 6554;
/// Tilt compensation factor (G.729, k1 < 0), 0.9 in Q15.
const G729_TILT_FACTOR_MINUS: i32 = 29491;
/// Gain adjustment factor (G.729, 4.2.4), 0.9875 in Q15.
const G729_AGC_FACTOR: i32 = 32358;
const G729_AGC_FAC1: i32 = 32768 - G729_AGC_FACTOR;
/// 1.0 / (1.0 + 0.5) in Q15, where 0.5 is the minimum value of the weight factor controlling the
/// amount of long-term postfiltering.
const MIN_LT_FILT_FACTOR_A: i64 = 21845;
/// Number of analyzed fractional pitch delays in the second stage of the long-term postfilter.
const ANALYZED_FRAC_DELAYS: usize = 7;
/// Length of the short interpolation filter.
const SHORT_INT_FILT_LEN: usize = 2;
/// Length of the long interpolation filter.
const LONG_INT_FILT_LEN: usize = 8;
/// Amount of past residual signal data stored in the buffer.
pub(crate) const RES_PREV_DATA_SIZE: usize = PITCH_DELAY_MAX as usize + LONG_INT_FILT_LEN + 1;

/// Postfilter state.
#[derive(Clone)]
pub(crate) struct Postfilter {
    /// Previous data for 4.2.3, equation 86.
    pub(crate) ht_prev_data: i16,
    /// Residual signal buffer (used in the long-term postfilter).
    residual: [i16; SUBFRAME_SIZE + RES_PREV_DATA_SIZE],
    /// Previous speech data for the residual calculation filter.
    res_filter_data: [i16; 10],
    /// Previous speech data for the short-term postfilter.
    pos_filter_data: [i16; SUBFRAME_SIZE + 10],
}

/// Residual signal calculation (4.2.1): filters `speech` through `A(z/FORMANT_PP_FACTOR_NUM)`.
///
/// `speech[0..10]` holds the previous speech data, `speech[10..]` the subframe.
fn residual_filter(out: &mut [i16], filter_coeffs: &[i16], speech: &[i16]) {
    for n in (0..SUBFRAME_SIZE).rev() {
        let mut sum: i32 = 0x800;
        for i in 0..10 {
            sum = sum.wrapping_add(i32::from(filter_coeffs[i]) * i32::from(speech[10 + n - i - 1]));
        }
        out[n] = (i32::from(speech[10 + n]).wrapping_add(sum >> 12)) as i16;
    }
}

/// Rescales the selected signal back to its original range.
fn rescale(sig: &mut [i16], shift: i32) {
    for v in sig.iter_mut().take(SUBFRAME_SIZE) {
        *v = if shift > 0 {
            (i32::from(*v) * (1 << shift)) as i16
        }
        else {
            (i32::from(*v) >> -shift) as i16
        };
    }
}

/// The signal selected by the long-term postfilter.
enum Selected {
    /// The long-interpolated signal in `residual_filt`.
    Filtered,
    /// A short-interpolated signal: `(fractional delay index, offset)`.
    Delayed(usize, usize),
    /// The unfiltered residual at the given index.
    Residual(usize),
}

/// Long-term postfilter (4.2.1). Returns `true` if the long-term prediction gain is at least 3 dB,
/// i.e. the subframe is periodic. `residual_filt` has `SUBFRAME_SIZE + 1` elements.
fn long_term_filter(
    pitch_delay_int: i32,
    residual: &[i16; SUBFRAME_SIZE + RES_PREV_DATA_SIZE],
    residual_filt: &mut [i16],
) -> bool {
    const N: usize = SUBFRAME_SIZE;
    const PREV: usize = RES_PREV_DATA_SIZE;

    let mut tmp = 0i32;
    for &r in residual.iter() {
        tmp |= i32::from(r).abs();
    }

    let shift = if tmp == 0 { 3 } else { log2(tmp as u32) - 11 };

    let mut sig_scaled = [0i16; N + PREV];
    for (s, &r) in sig_scaled.iter_mut().zip(residual.iter()) {
        *s = if shift > 0 {
            (i32::from(r) >> shift) as i16
        }
        else {
            (i32::from(r) << -shift) as i16
        };
    }

    let mut delayed_signal = [[0i16; N + 1]; ANALYZED_FRAC_DELAYS];
    let mut corr_den = [[0i32; 2]; ANALYZED_FRAC_DELAYS];

    // Start of best delay searching code.
    let mut gain_num: i16 = 0;
    let mut gain_den: i16 = 0;
    let mut sh_gain_num: i32 = 0;
    let mut sh_gain_den: i32 = 0;
    let mut best_delay_int: i32 = 0;
    let mut best_delay_frac: usize = 0;
    let mut delayed_signal_offset: usize = 1;

    let cur = &sig_scaled[PREV..PREV + N];
    let mut ener = scalarproduct(cur, cur);

    if ener != 0 {
        let sh_ener = (log2(ener as u32) - 14).max(0);
        ener >>= sh_ener;

        // Find the integer delay best_delay which maximizes the correlation R(T). This is also
        // the numerator of R'(0), since the fine search (second step) is done with 1/8 precision
        // around best_delay.
        let mut corr_int_num = 0i32;
        best_delay_int = pitch_delay_int - 1;
        for i in pitch_delay_int - 1..=pitch_delay_int + 1 {
            let start = PREV - (i as usize);
            let sum = scalarproduct(cur, &sig_scaled[start..start + N]);
            if sum > corr_int_num {
                corr_int_num = sum;
                best_delay_int = i;
            }
        }

        if corr_int_num != 0 {
            let best = best_delay_int as usize;

            // Denominator of the pseudo-normalized correlation R'(0).
            let lagged = &sig_scaled[PREV - best..PREV - best + N];
            let corr_int_den = scalarproduct(lagged, lagged);

            // Signals with non-integer delay k (1/8 precision), k in [0; 6]. The entire delay is
            // best_delay + (k + 1) / 8, obtained by a 33 tap interpolation filter.
            for k in 0..ANALYZED_FRAC_DELAYS {
                interpolate(
                    &mut delayed_signal[k],
                    &sig_scaled,
                    PREV - best,
                    &INTERP_FILT_SHORT,
                    ANALYZED_FRAC_DELAYS + 1,
                    8 - k - 1,
                    SHORT_INT_FILT_LEN,
                    N + 1,
                );
            }

            // Denominator of R'(k): corr_den[k][0] is for int(T) == int(T0), corr_den[k][1] for
            // int(T) == int(T0) + 1. Also compute the maximum of the denominators.
            tmp = corr_int_den;
            for k in 0..ANALYZED_FRAC_DELAYS {
                let d = &delayed_signal[k];
                let sum = scalarproduct(&d[1..N], &d[1..N]);
                corr_den[k][0] = sum.wrapping_add(i32::from(d[0]) * i32::from(d[0]));
                corr_den[k][1] = sum.wrapping_add(i32::from(d[N]) * i32::from(d[N]));

                tmp = tmp.max(corr_den[k][0]).max(corr_den[k][1]);
            }

            sh_gain_den = log2(tmp as u32) - 14;
            if sh_gain_den >= 0 {
                sh_gain_num = sh_gain_den.max(sh_ener);

                // Loop through all k and find the delay that maximizes R'(k). The search is done
                // in the [int(T0) - 1; int(T0) + 1] range with 1/8 precision.
                gain_den = (corr_int_den >> sh_gain_den) as i16;
                gain_num = (corr_int_num >> sh_gain_num) as i16;
                let mut gain_num_square = i32::from(gain_num) * i32::from(gain_num);
                for k in 0..ANALYZED_FRAC_DELAYS {
                    for i in 0..2 {
                        // Numerator of the pseudo-normalized correlation R'(k).
                        let sum = scalarproduct(&delayed_signal[k][i..i + N], cur);
                        let gain_num_short = (sum >> sh_gain_num).max(0) as i16;

                        //                 gain_num_short_square            gain_num_square
                        //    R'(T)^2 = ----------------------- , max R'(T)^2 = --------------
                        //                       den                              gain_den
                        let gain_num_short_square =
                            i32::from(gain_num_short) * i32::from(gain_num_short);
                        let gain_den_short = (corr_den[k][i] >> sh_gain_den) as i16;

                        let t1 =
                            ((i64::from(gain_num_short_square) * i64::from(gain_den)) >> 15) as i32;
                        let t2 =
                            ((i64::from(gain_num_square) * i64::from(gain_den_short)) >> 15) as i32;

                        // R'(T)^2 > max R'(T)^2
                        if t1 > t2 {
                            gain_num = gain_num_short;
                            gain_den = gain_den_short;
                            gain_num_square = gain_num_short_square;
                            delayed_signal_offset = i;
                            best_delay_frac = k + 1;
                        }
                    }
                }

                // 2 * R'(T)^2 / R(0) < 1
                let l0 = i64::from(gain_num_square).wrapping_shl(((sh_gain_num << 1) + 1) as u32);
                let l1 = (i64::from(gain_den) * i64::from(ener))
                    .wrapping_shl((sh_gain_den + sh_ener) as u32);
                if l0 < l1 {
                    gain_num = 0;
                }
            }
        }
    }
    // End of best delay searching code.

    if gain_num == 0 {
        residual_filt[..N].copy_from_slice(&residual[PREV..PREV + N]);

        // The long-term prediction gain is less than 3 dB: the long-term postfilter is disabled.
        return false;
    }

    let best = best_delay_int as usize;
    let selected;
    if best_delay_frac != 0 {
        // Recompute the delayed signal with an interpolation filter of length 129.
        interpolate(
            residual_filt,
            &sig_scaled,
            PREV - best + delayed_signal_offset,
            &INTERP_FILT_LONG,
            ANALYZED_FRAC_DELAYS + 1,
            8 - best_delay_frac,
            LONG_INT_FILT_LEN,
            N + 1,
        );

        // R'(k) correlation's numerator.
        let mut sum = scalarproduct(&residual_filt[..N], cur);
        let (gain_long_num, sh_gain_long_num);
        if sum < 0 {
            gain_long_num = 0i16;
            sh_gain_long_num = 0;
        }
        else {
            let t = (log2(sum as u32) - 14).max(0);
            sum >>= t;
            gain_long_num = sum as i16;
            sh_gain_long_num = t;
        }

        // R'(k) correlation's denominator.
        let mut sum = scalarproduct(&residual_filt[..N], &residual_filt[..N]);
        let t = (log2(sum as u32) - 14).max(0);
        sum >>= t;
        let gain_long_den = sum as i16;
        let sh_gain_long_den = t;

        // Select between the original and the delayed signal. The delayed signal is selected if
        // it increases the R'(k) correlation.
        let mut l_temp0 = i32::from(gain_num) * i32::from(gain_num);
        l_temp0 = ((i64::from(l_temp0) * i64::from(gain_long_den)) >> 15) as i32;

        let mut l_temp1 = i32::from(gain_long_num) * i32::from(gain_long_num);
        l_temp1 = ((i64::from(l_temp1) * i64::from(gain_den)) >> 15) as i32;

        let t = ((sh_gain_long_num - sh_gain_num) * 2) - (sh_gain_long_den - sh_gain_den);
        if t > 0 {
            // A shift count of 32 or more is undefined in C; x86 masks it to 5 bits.
            l_temp0 >>= t as u32 & 31;
        }
        else {
            l_temp1 >>= (-t).min(31);
        }

        // Check whether the longer filter increases the values of R'(k).
        if l_temp1 > l_temp0 {
            selected = Selected::Filtered;
            gain_num = gain_long_num;
            gain_den = gain_long_den;
            sh_gain_num = sh_gain_long_num;
            sh_gain_den = sh_gain_long_den;
        }
        else {
            selected = Selected::Delayed(best_delay_frac - 1, delayed_signal_offset);
        }

        // Rescale the selected signal to the original value.
        match selected {
            Selected::Filtered => rescale(residual_filt, shift),
            Selected::Delayed(k, off) => rescale(&mut delayed_signal[k][off..], shift),
            Selected::Residual(_) => {}
        }
    }
    else {
        selected = Selected::Residual(PREV - (best + 1 - delayed_signal_offset));
    }

    let l0 = (i64::from(gain_num).wrapping_shl(sh_gain_num as u32)) >> 1;
    let l1 = i64::from(gain_den).wrapping_shl(sh_gain_den as u32);
    // The reference divides unconditionally; a zero divisor would crash it.
    let lt_filt_factor_a = match (l1.wrapping_shl(15)).checked_div(l1.wrapping_add(l0)) {
        Some(q) => q.max(MIN_LT_FILT_FACTOR_A) as i32,
        None => MIN_LT_FILT_FACTOR_A as i32,
    };

    // Filter through the selected filter.
    let lt_filt_factor_b = 32767 - lt_filt_factor_a + 1;
    let wa = lt_filt_factor_a as i16;
    let wb = lt_filt_factor_b as i16;

    let res = &residual[PREV..PREV + N];
    match selected {
        Selected::Filtered => {
            for i in 0..N {
                residual_filt[i] = weighted_sum(res[i], residual_filt[i], wa, wb, 1 << 14, 15);
            }
        }
        Selected::Delayed(k, off) => {
            for i in 0..N {
                residual_filt[i] =
                    weighted_sum(res[i], delayed_signal[k][off + i], wa, wb, 1 << 14, 15);
            }
        }
        Selected::Residual(start) => {
            for i in 0..N {
                residual_filt[i] = weighted_sum(res[i], residual[start + i], wa, wb, 1 << 14, 15);
            }
        }
    }

    // The long-term prediction gain is larger than 3 dB.
    true
}

/// Calculates the reflection coefficient for the tilt compensation filter (4.2.3), in (3.12).
///
/// Also calculates the gain term of the short-term filter (gf) and multiplies the speech data by
/// 1/gf. All members of `lp_gn`, except 10 to 19, must be zero.
fn get_tilt_comp(lp_gn: &mut [i16; 33], lp_gd: &[i16; 11], speech: &mut [i16]) -> i32 {
    lp_gn[10] = 4096; // 1.0 in (3.12)

    // Apply the 1/A(z/FORMANT_PP_FACTOR_DEN) filter to hf. Afterwards lp_gn (starting at 10)
    // contains the impulse response of the A(z/FORMANT_PP_FACTOR_NUM)/A(z/FORMANT_PP_FACTOR_DEN)
    // filter.
    lp_synthesis_filter_inplace(lp_gn, 11, &lp_gd[1..], 22, 10, 0x800);

    let mut rh0 = scalarproduct(&lp_gn[10..30], &lp_gn[10..30]); // (3.12)
    let mut rh1 = scalarproduct(&lp_gn[10..30], &lp_gn[11..31]); // (3.12)

    // Downscale to avoid overflow.
    let temp = log2(rh0 as u32) - 14;
    if temp > 0 {
        rh0 >>= temp;
        rh1 >>= temp;
    }

    if rh1.abs() > rh0 || rh0 == 0 {
        return 0;
    }

    let mut gain_term = 0i32;
    for &v in &lp_gn[10..30] {
        gain_term += i32::from(v).abs();
    }
    gain_term >>= 2; // (3.12) -> (5.10)

    if gain_term > 0x400 {
        // 1.0 in (5.10)
        let temp = 0x200_0000 / gain_term; // 1.0 / gain_term in (0.15)
        for s in speech.iter_mut().take(SUBFRAME_SIZE) {
            *s = ((i32::from(*s) * temp + 0x4000) >> 15) as i16;
        }
    }

    -(rh1 * (1 << 15)) / rh0
}

/// Applies the tilt compensation filter (4.2.3). Returns the new value of `ht_prev_data`.
fn apply_tilt_comp(out: &mut [i16], res_pst: &[i16], refl_coeff: i32, ht_prev_data: i16) -> i16 {
    let (gt, fact, sh_fact);
    if refl_coeff > 0 {
        gt = (refl_coeff * G729_TILT_FACTOR_PLUS + 0x4000) >> 15;
        fact = 0x2000; // 0.5 in (0.15)
        sh_fact = 14;
    }
    else {
        gt = (refl_coeff * G729_TILT_FACTOR_MINUS + 0x4000) >> 15;
        fact = 0x400; // 0.5 in (3.12)
        sh_fact = 11;
    }
    let denom = i32::from(clip_i16(32768 - gt.abs())).max(1);
    let ga = (fact << 16) / denom;
    let gt = gt >> 1;

    // Apply the tilt compensation filter to the signal.
    let tmp = res_pst[SUBFRAME_SIZE - 1];

    for i in (1..SUBFRAME_SIZE).rev() {
        let mut tmp2 = (gt * i32::from(res_pst[i - 1])).wrapping_mul(2).wrapping_add(0x4000);
        tmp2 = i32::from(res_pst[i]).wrapping_add(tmp2 >> 15);

        tmp2 = tmp2.wrapping_mul(ga).wrapping_add(fact) >> sh_fact;
        out[i] = tmp2 as i16;
    }
    let mut tmp2 = (gt * i32::from(ht_prev_data)).wrapping_mul(2).wrapping_add(0x4000);
    tmp2 = i32::from(res_pst[0]).wrapping_add(tmp2 >> 15);
    tmp2 = tmp2.wrapping_mul(ga).wrapping_add(fact) >> sh_fact;
    out[0] = tmp2 as i16;

    tmp
}

impl Postfilter {
    pub(crate) fn new() -> Self {
        Postfilter {
            ht_prev_data: 0,
            residual: [0; SUBFRAME_SIZE + RES_PREV_DATA_SIZE],
            res_filter_data: [0; 10],
            pos_filter_data: [0; SUBFRAME_SIZE + 10],
        }
    }

    /// Postfilters one subframe (`ff_g729_postfilter`).
    ///
    /// `speech[10..]` is the subframe to filter (in place); `speech[..10]` is scratch space for
    /// the filter history. `voicing` is set if the subframe is periodic.
    pub(crate) fn run(
        &mut self,
        voicing: &mut bool,
        lp_filter_coeffs: &[i16; 11],
        pitch_delay_int: i32,
        speech: &mut [i16; SUBFRAME_SIZE + 10],
    ) {
        let mut residual_filt_buf = [0i16; SUBFRAME_SIZE + 11];
        // Zero-filling is necessary for the tilt compensation filter.
        let mut lp_gn = [0i16; 33]; // (3.12)
        let mut lp_gd = [0i16; 11]; // (3.12)

        // A(z/FORMANT_PP_FACTOR_NUM) filter coefficients.
        for i in 0..10 {
            lp_gn[i + 11] = ((i32::from(lp_filter_coeffs[i + 1])
                * i32::from(FORMANT_PP_FACTOR_NUM_POW[i])
                + 0x4000)
                >> 15) as i16;
        }

        // A(z/FORMANT_PP_FACTOR_DEN) filter coefficients.
        for i in 0..10 {
            lp_gd[i + 1] = ((i32::from(lp_filter_coeffs[i + 1])
                * i32::from(FORMANT_PP_FACTOR_DEN_POW[i])
                + 0x4000)
                >> 15) as i16;
        }

        // Residual signal calculation (one half of the short-term postfilter).
        speech[..10].copy_from_slice(&self.res_filter_data);
        residual_filter(&mut self.residual[RES_PREV_DATA_SIZE..], &lp_gn[11..21], &speech[..]);
        // Save data to use it in the next subframe.
        self.res_filter_data.copy_from_slice(&speech[SUBFRAME_SIZE..SUBFRAME_SIZE + 10]);

        // Long-term filter. If the long-term prediction gain is larger than 3 dB, declare the
        // current subframe as periodic.
        let periodic =
            long_term_filter(pitch_delay_int, &self.residual, &mut residual_filt_buf[10..]);
        *voicing |= periodic;

        // Shift the residual for use in the next subframe.
        self.residual.copy_within(SUBFRAME_SIZE.., 0);

        // Short-term filter tilt compensation.
        let tilt_comp_coeff = get_tilt_comp(&mut lp_gn, &lp_gd, &mut residual_filt_buf[10..]);

        // Apply the second half of the short-term postfilter: 1/A(z/FORMANT_PP_FACTOR_DEN).
        lp_synthesis_filter(
            &mut self.pos_filter_data,
            10,
            &lp_gd[1..],
            &residual_filt_buf[10..],
            SUBFRAME_SIZE,
            10,
            false,
            0x800,
        );
        self.pos_filter_data.copy_within(SUBFRAME_SIZE..SUBFRAME_SIZE + 10, 0);

        self.ht_prev_data = apply_tilt_comp(
            &mut speech[10..],
            &self.pos_filter_data[10..],
            tilt_comp_coeff,
            self.ht_prev_data,
        );
    }
}

/// Adaptive gain control (4.2.4). `gain_prev` is the previous gain coefficient in (3.12); the new
/// one is returned.
pub(crate) fn adaptive_gain_control(
    gain_before: i32,
    gain_after: i32,
    speech: &mut [i16],
    gain_prev: i16,
) -> i16 {
    if gain_after == 0 && gain_before != 0 {
        return 0;
    }

    let gain: u32 = if gain_before != 0 {
        let exp_before = 14 - log2(gain_before as u32);
        let gain_before = bidir_sal(gain_before as u32, exp_before) as i32;

        let exp_after = 14 - log2(gain_after as u32);
        let gain_after = bidir_sal(gain_after as u32, exp_after) as i32;

        let mut gain;
        if gain_before < gain_after {
            gain = ((gain_before as u32) << 15) / (gain_after as u32);
            gain = bidir_sal(gain, exp_after - exp_before - 1);
        }
        else {
            gain = ((((gain_before - gain_after) as u32) << 14) / (gain_after as u32)) + 0x4000;
            gain = bidir_sal(gain, exp_after - exp_before);
        }
        gain = gain.min(32767);
        // gain * (1 - 0.9875)
        (gain.wrapping_mul(G729_AGC_FAC1 as u32).wrapping_add(0x4000)) >> 15
    }
    else {
        0
    };

    let mut gain_prev = gain_prev;
    for s in speech.iter_mut().take(SUBFRAME_SIZE) {
        // gain_prev = gain + 0.9875 * gain_prev
        gain_prev = ((G729_AGC_FACTOR * i32::from(gain_prev) + 0x4000) >> 15) as i16;
        gain_prev = clip_i16((gain as i32).wrapping_add(i32::from(gain_prev)));
        *s = clip_i16((i32::from(*s) * i32::from(gain_prev) + 0x2000) >> 14);
    }
    gain_prev
}
