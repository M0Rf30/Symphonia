// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Packet loss concealment (PLC): on loss, extrapolates voiced/unvoiced excitation through the
//! last known LTP/LPC filters with decaying gain; on a good frame, updates the PLC state and
//! smoothly glues the previous (possibly concealed) tail into the new frame. Ported from
//! libopus `silk/PLC.c` (BSD-3-Clause; `ENABLE_DEEP_PLC`/neural-net PLC is excluded), see
//! NOTICE.

#![allow(dead_code)]

use crate::silk::bwexpander::silk_bwexpander;
use crate::silk::lpc_analysis_filter::silk_lpc_analysis_filter;
use crate::silk::lpc_inv_pred_gain::silk_lpc_inverse_pred_gain;
use crate::silk::macros::{
    silk_add_sat32, silk_clz32, silk_div32, silk_fix_const, silk_inverse32_varq, silk_lshift, silk_lshift_sat32,
    silk_max, silk_max_32, silk_min, silk_min_32, silk_rand, silk_rshift, silk_rshift_round, silk_sat16,
    silk_smlawb, silk_smulbb, silk_smulwb, silk_smulww, silk_sqrt_approx,
};
use crate::silk::structs::{
    SilkDecoderControl, SilkDecoderState, LTP_ORDER, MAX_LPC_ORDER, MAX_LTP_MEM_LENGTH, TYPE_VOICED,
};
use crate::silk::sum_sqr_shift::silk_sum_sqr_shift;

const NB_ATT: usize = 2;
const HARM_ATT_Q15: [i16; NB_ATT] = [32440, 31130];
const PLC_RAND_ATTENUATE_V_Q15: [i16; NB_ATT] = [31130, 26214];
const PLC_RAND_ATTENUATE_UV_Q15: [i16; NB_ATT] = [32440, 29491];

/// C: `BWE_COEF` (`silk/PLC.h`).
const BWE_COEF: f64 = 0.99;
/// C: `V_PITCH_GAIN_START_MIN_Q14`.
const V_PITCH_GAIN_START_MIN_Q14: i32 = 11469;
/// C: `V_PITCH_GAIN_START_MAX_Q14`.
const V_PITCH_GAIN_START_MAX_Q14: i32 = 15565;
/// C: `MAX_PITCH_LAG_MS`.
const MAX_PITCH_LAG_MS: i32 = 18;
/// C: `RAND_BUF_SIZE`/`RAND_BUF_MASK`.
const RAND_BUF_SIZE: i32 = 128;
const RAND_BUF_MASK: i32 = RAND_BUF_SIZE - 1;
/// C: `LOG2_INV_LPC_GAIN_HIGH_THRES`/`LOG2_INV_LPC_GAIN_LOW_THRES`.
const LOG2_INV_LPC_GAIN_HIGH_THRES: i32 = 3;
const LOG2_INV_LPC_GAIN_LOW_THRES: i32 = 8;
/// C: `PITCH_DRIFT_FAC_Q16`.
const PITCH_DRIFT_FAC_Q16: i32 = 655;

/// C: `silk_PLC_Reset`.
pub(crate) fn reset(dec: &mut SilkDecoderState) {
    dec.s_plc.pitch_l_q8 = silk_lshift(dec.frame_length, 8 - 1);
    dec.s_plc.prev_gain_q16[0] = silk_fix_const(1.0, 16);
    dec.s_plc.prev_gain_q16[1] = silk_fix_const(1.0, 16);
    dec.s_plc.subfr_length = 20;
    dec.s_plc.nb_subfr = 2;
}

/// C: `silk_PLC`. On `lost`, extrapolate concealment into `frame`; otherwise update PLC state
/// from the just-decoded `ctrl`.
pub(crate) fn silk_plc(dec: &mut SilkDecoderState, ctrl: &mut SilkDecoderControl, frame: &mut [i16], lost: bool) {
    if dec.fs_khz != dec.s_plc.fs_khz {
        reset(dec);
        dec.s_plc.fs_khz = dec.fs_khz;
    }

    if lost {
        plc_conceal(dec, ctrl, frame);
        dec.loss_cnt += 1;
    } else {
        plc_update(dec, ctrl);
    }
}

/// C: `silk_PLC_update`.
fn plc_update(dec: &mut SilkDecoderState, ctrl: &mut SilkDecoderControl) {
    let nb_subfr = dec.nb_subfr as usize;

    dec.prev_signal_type = dec.indices.signal_type as i32;
    let mut ltp_gain_q14: i32 = 0;
    if dec.indices.signal_type == TYPE_VOICED as i8 {
        // Find the parameters for the last subframe which contains a pitch pulse.
        let mut j = 0usize;
        while (j as i32) * dec.subfr_length < ctrl.pitch_l[nb_subfr - 1] {
            if j == nb_subfr {
                break;
            }
            let base = (nb_subfr - 1 - j) * LTP_ORDER;
            let temp_ltp_gain_q14: i32 = ctrl.ltp_coef_q14[base..base + LTP_ORDER].iter().map(|&c| c as i32).sum();
            if temp_ltp_gain_q14 > ltp_gain_q14 {
                ltp_gain_q14 = temp_ltp_gain_q14;
                dec.s_plc.ltp_coef_q14.copy_from_slice(&ctrl.ltp_coef_q14[base..base + LTP_ORDER]);
                dec.s_plc.pitch_l_q8 = silk_lshift(ctrl.pitch_l[nb_subfr - 1 - j], 8);
            }
            j += 1;
        }

        for c in dec.s_plc.ltp_coef_q14.iter_mut() {
            *c = 0;
        }
        dec.s_plc.ltp_coef_q14[LTP_ORDER / 2] = ltp_gain_q14 as i16;

        // Limit LT coefs.
        if ltp_gain_q14 < V_PITCH_GAIN_START_MIN_Q14 {
            let tmp = silk_lshift(V_PITCH_GAIN_START_MIN_Q14, 10);
            let scale_q10 = silk_div32(tmp, silk_max(ltp_gain_q14, 1));
            for c in dec.s_plc.ltp_coef_q14.iter_mut() {
                *c = silk_rshift(silk_smulbb(*c as i32, scale_q10), 10) as i16;
            }
        } else if ltp_gain_q14 > V_PITCH_GAIN_START_MAX_Q14 {
            let tmp = silk_lshift(V_PITCH_GAIN_START_MAX_Q14, 14);
            let scale_q14 = silk_div32(tmp, silk_max(ltp_gain_q14, 1));
            for c in dec.s_plc.ltp_coef_q14.iter_mut() {
                *c = silk_rshift(silk_smulbb(*c as i32, scale_q14), 14) as i16;
            }
        }
    } else {
        dec.s_plc.pitch_l_q8 = silk_lshift(silk_smulbb(dec.fs_khz, 18), 8);
        for c in dec.s_plc.ltp_coef_q14.iter_mut() {
            *c = 0;
        }
    }

    // Save LPC coefficients.
    let order = dec.lpc_order as usize;
    dec.s_plc.prev_lpc_q12[..order].copy_from_slice(&ctrl.pred_coef_q12[1][..order]);
    dec.s_plc.prev_ltp_scale_q14 = ctrl.ltp_scale_q14 as i16;

    // Save last two gains.
    dec.s_plc.prev_gain_q16.copy_from_slice(&ctrl.gains_q16[nb_subfr - 2..nb_subfr]);

    dec.s_plc.subfr_length = dec.subfr_length;
    dec.s_plc.nb_subfr = dec.nb_subfr;
}

/// C: `silk_PLC_energy`. Returns `(energy1, shift1, energy2, shift2)`.
fn plc_energy(exc_q14: &[i32], prev_gain_q10: &[i32; 2], subfr_length: usize, nb_subfr: usize) -> (i32, i32, i32, i32) {
    let mut exc_buf = [0i16; 2 * crate::silk::structs::MAX_SUB_FRAME_LENGTH];
    for k in 0..2 {
        let src_start = (k + nb_subfr - 2) * subfr_length;
        for i in 0..subfr_length {
            exc_buf[k * subfr_length + i] =
                silk_sat16(silk_rshift(silk_smulww(exc_q14[src_start + i], prev_gain_q10[k]), 8)) as i16;
        }
    }
    let (energy1, shift1) = silk_sum_sqr_shift(&exc_buf[..subfr_length]);
    let (energy2, shift2) = silk_sum_sqr_shift(&exc_buf[subfr_length..2 * subfr_length]);
    (energy1, shift1, energy2, shift2)
}

/// C: `silk_PLC_conceal`.
fn plc_conceal(dec: &mut SilkDecoderState, ctrl: &mut SilkDecoderControl, frame: &mut [i16]) {
    let ltp_mem_length = dec.ltp_mem_length as usize;
    let frame_length = dec.frame_length as usize;
    let order = dec.lpc_order as usize;

    let mut s_ltp = [0i16; MAX_LTP_MEM_LENGTH];
    let mut s_ltp_q14 = [0i32; MAX_LTP_MEM_LENGTH + crate::silk::structs::MAX_FRAME_LENGTH];

    let prev_gain_q10 = [silk_rshift(dec.s_plc.prev_gain_q16[0], 6), silk_rshift(dec.s_plc.prev_gain_q16[1], 6)];

    if dec.first_frame_after_reset {
        for v in dec.s_plc.prev_lpc_q12.iter_mut() {
            *v = 0;
        }
    }

    let (energy1, shift1, energy2, shift2) =
        plc_energy(&dec.exc_q14, &prev_gain_q10, dec.subfr_length as usize, dec.nb_subfr as usize);

    let rand_ptr_start = if silk_rshift(energy1, shift2) < silk_rshift(energy2, shift1) {
        // First sub-frame has lowest energy.
        silk_max(0, (dec.s_plc.nb_subfr - 1) * dec.s_plc.subfr_length - RAND_BUF_SIZE) as usize
    } else {
        // Second sub-frame has lowest energy.
        silk_max(0, dec.s_plc.nb_subfr * dec.s_plc.subfr_length - RAND_BUF_SIZE) as usize
    };

    // Set up Gain to random noise component.
    let mut b_q14 = dec.s_plc.ltp_coef_q14;
    let mut rand_scale_q14 = dec.s_plc.rand_scale_q14 as i32;

    // Set up attenuation gains.
    let att_idx = silk_min(NB_ATT as i32 - 1, dec.loss_cnt) as usize;
    let harm_gain_q15 = HARM_ATT_Q15[att_idx] as i32;
    let mut rand_gain_q15 = if dec.prev_signal_type == TYPE_VOICED {
        PLC_RAND_ATTENUATE_V_Q15[att_idx] as i32
    } else {
        PLC_RAND_ATTENUATE_UV_Q15[att_idx] as i32
    };

    // LPC concealment. Apply BWE to previous LPC.
    silk_bwexpander(&mut dec.s_plc.prev_lpc_q12[..order], order, silk_fix_const(BWE_COEF, 16));

    // Preload LPC coefficients to array on stack.
    let mut a_q12 = [0i16; MAX_LPC_ORDER];
    a_q12[..order].copy_from_slice(&dec.s_plc.prev_lpc_q12[..order]);

    // First lost frame.
    if dec.loss_cnt == 0 {
        rand_scale_q14 = 1 << 14;

        // Reduce random noise gain for voiced frames.
        if dec.prev_signal_type == TYPE_VOICED {
            for i in 0..LTP_ORDER {
                rand_scale_q14 -= b_q14[i] as i32;
            }
            rand_scale_q14 = silk_max(3277, rand_scale_q14);
            rand_scale_q14 = silk_rshift(silk_smulbb(rand_scale_q14, dec.s_plc.prev_ltp_scale_q14 as i32), 14);
        } else {
            // Reduce random noise for unvoiced frames with high LPC gain.
            let inv_gain_q30 = silk_lpc_inverse_pred_gain(&dec.s_plc.prev_lpc_q12, order);

            let mut down_scale_q30 = silk_min_32(silk_rshift(1i32 << 30, LOG2_INV_LPC_GAIN_HIGH_THRES), inv_gain_q30);
            down_scale_q30 = silk_max_32(silk_rshift(1i32 << 30, LOG2_INV_LPC_GAIN_LOW_THRES), down_scale_q30);
            down_scale_q30 = silk_lshift(down_scale_q30, LOG2_INV_LPC_GAIN_HIGH_THRES);

            rand_gain_q15 = silk_rshift(silk_smulwb(down_scale_q30, rand_gain_q15), 14);
        }
    }

    let mut rand_seed = dec.s_plc.rand_seed;
    let mut lag = silk_rshift_round(dec.s_plc.pitch_l_q8, 8) as usize;
    let mut s_ltp_buf_idx = ltp_mem_length;

    // Rewhiten LTP state.
    //
    // `lag` derives from `s_plc.pitch_l_q8`, which is cached from whichever earlier packet last
    // updated it (`plc_update`) and can therefore reflect a different `nb_subfr`/`subfr_length`
    // combination than the *current* PLC call requests (SILK's `nb_subfr` depends only on the
    // requested payload duration, independent of `fs_khz`/PLC-state-reset, so this genuinely can
    // happen for a crafted lost-packet sequence, not just a hypothetical). `idx` clamped to
    // `[0, ltp_mem_length]` -- a no-op whenever the normal invariant `idx > 0` holds (every real
    // encoder/decoder session) -- keeps the slicing below in-bounds regardless.
    let idx = ltp_mem_length as isize - lag as isize - order as isize - (LTP_ORDER / 2) as isize;
    let idx = idx.clamp(0, ltp_mem_length as isize) as usize;
    {
        let in_len = ltp_mem_length - idx;
        let mut in_local = [0i16; MAX_LTP_MEM_LENGTH];
        in_local[..in_len].copy_from_slice(&dec.out_buf[idx..idx + in_len]);
        let mut out_local = [0i16; MAX_LTP_MEM_LENGTH];
        silk_lpc_analysis_filter(&mut out_local[..in_len], &in_local[..in_len], &a_q12[..order], in_len, order);
        s_ltp[idx..ltp_mem_length].copy_from_slice(&out_local[..in_len]);
    }
    // Scale LTP state.
    let mut inv_gain_q30 = silk_inverse32_varq(dec.s_plc.prev_gain_q16[1], 46);
    inv_gain_q30 = silk_min(inv_gain_q30, i32::MAX >> 1);
    for i in idx + order..ltp_mem_length {
        s_ltp_q14[i] = silk_smulwb(inv_gain_q30, s_ltp[i] as i32);
    }

    // LTP synthesis filtering. Same staleness hazard as above: clamp every `s_ltp_q14` index to
    // stay in-bounds (a no-op for any in-range `lag`/`s_ltp_buf_idx` combination, i.e. every real
    // session) rather than trust the cross-call-cached `lag` blindly.
    let ltp_q14_last = s_ltp_q14.len() as isize - 1;
    let clamp_ltp_idx = |x: isize| -> usize { x.clamp(0, ltp_q14_last) as usize };
    for _k in 0..dec.nb_subfr as usize {
        let pred_lag_base = s_ltp_buf_idx as isize - lag as isize + (LTP_ORDER / 2) as isize;
        for i in 0..dec.subfr_length as usize {
            let p = pred_lag_base + i as isize;
            let mut ltp_pred_q12: i32 = 2;
            ltp_pred_q12 = silk_smlawb(ltp_pred_q12, s_ltp_q14[clamp_ltp_idx(p)], b_q14[0] as i32);
            ltp_pred_q12 = silk_smlawb(ltp_pred_q12, s_ltp_q14[clamp_ltp_idx(p - 1)], b_q14[1] as i32);
            ltp_pred_q12 = silk_smlawb(ltp_pred_q12, s_ltp_q14[clamp_ltp_idx(p - 2)], b_q14[2] as i32);
            ltp_pred_q12 = silk_smlawb(ltp_pred_q12, s_ltp_q14[clamp_ltp_idx(p - 3)], b_q14[3] as i32);
            ltp_pred_q12 = silk_smlawb(ltp_pred_q12, s_ltp_q14[clamp_ltp_idx(p - 4)], b_q14[4] as i32);

            // Generate LPC excitation.
            rand_seed = silk_rand(rand_seed);
            let ridx = (silk_rshift(rand_seed, 25) & RAND_BUF_MASK) as usize;
            let widx = clamp_ltp_idx(s_ltp_buf_idx as isize);
            s_ltp_q14[widx] =
                silk_lshift(silk_smlawb(ltp_pred_q12, dec.exc_q14[rand_ptr_start + ridx], rand_scale_q14 as i16 as i32), 2);
            s_ltp_buf_idx += 1;
        }


        // Gradually reduce LTP gain.
        for c in b_q14.iter_mut() {
            *c = silk_rshift(silk_smulbb(harm_gain_q15, *c as i32), 15) as i16;
        }
        // Gradually reduce excitation gain.
        rand_scale_q14 = silk_rshift(silk_smulbb(rand_scale_q14, rand_gain_q15), 15);

        // Slowly increase pitch lag.
        dec.s_plc.pitch_l_q8 = silk_smlawb(dec.s_plc.pitch_l_q8, dec.s_plc.pitch_l_q8, PITCH_DRIFT_FAC_Q16);
        dec.s_plc.pitch_l_q8 = silk_min_32(dec.s_plc.pitch_l_q8, silk_lshift(silk_smulbb(MAX_PITCH_LAG_MS, dec.fs_khz), 8));
        lag = silk_rshift_round(dec.s_plc.pitch_l_q8, 8) as usize;
    }

    // LPC synthesis filtering.
    let slpc_base = ltp_mem_length - MAX_LPC_ORDER;

    // Copy LPC state.
    for j in 0..MAX_LPC_ORDER {
        s_ltp_q14[slpc_base + j] = dec.s_lpc_q14_buf[j];
    }

    debug_assert!(order >= 10);
    for i in 0..frame_length {
        let base = slpc_base + MAX_LPC_ORDER + i;
        let mut lpc_pred_q10: i32 = (order as i32) >> 1;
        lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_ltp_q14[base - 1], a_q12[0] as i32);
        lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_ltp_q14[base - 2], a_q12[1] as i32);
        lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_ltp_q14[base - 3], a_q12[2] as i32);
        lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_ltp_q14[base - 4], a_q12[3] as i32);
        lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_ltp_q14[base - 5], a_q12[4] as i32);
        lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_ltp_q14[base - 6], a_q12[5] as i32);
        lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_ltp_q14[base - 7], a_q12[6] as i32);
        lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_ltp_q14[base - 8], a_q12[7] as i32);
        lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_ltp_q14[base - 9], a_q12[8] as i32);
        lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_ltp_q14[base - 10], a_q12[9] as i32);
        for j in 10..order {
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_ltp_q14[base - j - 1], a_q12[j] as i32);
        }

        // Add prediction to LPC excitation.
        s_ltp_q14[base] = silk_add_sat32(s_ltp_q14[base], silk_lshift_sat32(lpc_pred_q10, 4));

        // Scale with Gain.
        frame[i] = silk_sat16(silk_rshift_round(silk_smulww(s_ltp_q14[base], prev_gain_q10[1]), 8)) as i16;
    }

    // Save LPC state.
    for j in 0..MAX_LPC_ORDER {
        dec.s_lpc_q14_buf[j] = s_ltp_q14[slpc_base + frame_length + j];
    }

    // Update states.
    dec.s_plc.rand_seed = rand_seed;
    dec.s_plc.rand_scale_q14 = rand_scale_q14 as i16;
    for p in ctrl.pitch_l.iter_mut() {
        *p = lag as i32;
    }
}

/// C: `silk_PLC_glue_frames`. Smooths the transition between a concealed tail and a following
/// good frame (or accumulates energy stats while concealing).
pub(crate) fn silk_plc_glue_frames(dec: &mut SilkDecoderState, frame: &mut [i16], length: usize) {
    if dec.loss_cnt != 0 {
        // Calculate energy in concealed residual.
        let (energy, shift) = silk_sum_sqr_shift(&frame[..length]);
        dec.s_plc.conc_energy = energy;
        dec.s_plc.conc_energy_shift = shift;
        dec.s_plc.last_frame_lost = true;
    } else {
        if dec.s_plc.last_frame_lost {
            // Calculate residual in decoded signal if last frame was lost.
            let (mut energy, energy_shift) = silk_sum_sqr_shift(&frame[..length]);

            // Normalize energies.
            if energy_shift > dec.s_plc.conc_energy_shift {
                dec.s_plc.conc_energy = silk_rshift(dec.s_plc.conc_energy, energy_shift - dec.s_plc.conc_energy_shift);
            } else if energy_shift < dec.s_plc.conc_energy_shift {
                energy = silk_rshift(energy, dec.s_plc.conc_energy_shift - energy_shift);
            }

            // Fade in the energy difference.
            if energy > dec.s_plc.conc_energy {
                let lz = silk_clz32(dec.s_plc.conc_energy) - 1;
                dec.s_plc.conc_energy = silk_lshift(dec.s_plc.conc_energy, lz);
                let energy = silk_rshift(energy, silk_max_32(24 - lz, 0));

                let frac_q24 = silk_div32(dec.s_plc.conc_energy, silk_max(energy, 1));

                let gain_q16_start = silk_lshift(silk_sqrt_approx(frac_q24), 4);
                let mut gain_q16 = gain_q16_start;
                let slope_q16 = silk_lshift(silk_div32(((1i32) << 16) - gain_q16, length as i32), 2);

                for f in frame[..length].iter_mut() {
                    *f = silk_smulwb(gain_q16, *f as i32) as i16;
                    gain_q16 += slope_q16;
                    if gain_q16 > 1i32 << 16 {
                        break;
                    }
                }
            }
        }
        dec.s_plc.last_frame_lost = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_sets_pitch_and_gains() {
        let mut dec = SilkDecoderState::new();
        dec.frame_length = 320;
        reset(&mut dec);
        assert_eq!(dec.s_plc.pitch_l_q8, 320 << 7);
        assert_eq!(dec.s_plc.prev_gain_q16[0], 1 << 16);
        assert_eq!(dec.s_plc.subfr_length, 20);
        assert_eq!(dec.s_plc.nb_subfr, 2);
    }
}
