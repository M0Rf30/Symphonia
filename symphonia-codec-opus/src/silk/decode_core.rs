// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Core decoder: excitation reconstruction plus inverse noise-shape-quantization (long-term +
//! short-term prediction synthesis). Ported from libopus `silk/decode_core.c`
//! (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::lpc_analysis_filter::silk_lpc_analysis_filter;
use crate::silk::macros::{
    silk_add_lshift32, silk_add_sat32, silk_div32_varq, silk_inverse32_varq, silk_lshift, silk_lshift_sat32,
    silk_rand, silk_rshift, silk_rshift_round, silk_sat16, silk_smlawb, silk_smulwb, silk_smulww, silk_fix_const,
};
use crate::silk::structs::{
    SilkDecoderControl, SilkDecoderState, LTP_ORDER, MAX_FRAME_LENGTH, MAX_LPC_ORDER, MAX_LTP_MEM_LENGTH,
    MAX_NB_SUBFR, MAX_SUB_FRAME_LENGTH, QUANT_LEVEL_ADJUST_Q10, TYPE_VOICED,
};
use crate::silk::tables::QUANTIZATION_OFFSETS_Q10;

/// C: `silk_decode_core`. `pulses` holds `psDec->frame_length` excitation pulses; `xq` receives
/// `psDec->frame_length` decoded PCM samples.
pub(crate) fn silk_decode_core(dec: &mut SilkDecoderState, ctrl: &SilkDecoderControl, xq: &mut [i16], pulses: &[i16]) {
    debug_assert!(dec.prev_gain_q16 != 0);

    let nb_subfr = dec.nb_subfr as usize;
    let frame_length = dec.frame_length as usize;
    let subfr_length = dec.subfr_length as usize;
    let ltp_mem_length = dec.ltp_mem_length as usize;
    let lpc_order = dec.lpc_order as usize;

    let mut s_ltp = [0i16; MAX_LTP_MEM_LENGTH];
    let mut s_ltp_q15 = [0i32; MAX_LTP_MEM_LENGTH + MAX_FRAME_LENGTH];
    let mut res_q14 = [0i32; MAX_SUB_FRAME_LENGTH];
    let mut s_lpc_q14 = [0i32; MAX_SUB_FRAME_LENGTH + MAX_LPC_ORDER];

    let offset_q10 =
        QUANTIZATION_OFFSETS_Q10[(dec.indices.signal_type as usize) >> 1][dec.indices.quant_offset_type as usize] as i32;

    let nlsf_interpolation_flag = dec.indices.nlsf_interp_coef_q2 < 4;

    // Decode excitation.
    let mut rand_seed = dec.indices.seed as i32;
    for i in 0..frame_length {
        rand_seed = silk_rand(rand_seed);
        let mut exc = silk_lshift(pulses[i] as i32, 14);
        if exc > 0 {
            exc -= QUANT_LEVEL_ADJUST_Q10 << 4;
        } else if exc < 0 {
            exc += QUANT_LEVEL_ADJUST_Q10 << 4;
        }
        exc += offset_q10 << 4;
        if rand_seed < 0 {
            exc = -exc;
        }
        dec.exc_q14[i] = exc;

        rand_seed = rand_seed.wrapping_add(pulses[i] as i32);
    }

    // Copy LPC state.
    s_lpc_q14[..MAX_LPC_ORDER].copy_from_slice(&dec.s_lpc_q14_buf);

    let mut pexc_idx = 0usize; // index into dec.exc_q14
    let mut pxq_idx = 0usize; // index into xq
    let mut s_ltp_buf_idx = ltp_mem_length;
    let mut lag: usize = 0;

    for k in 0..nb_subfr {
        let a_q12 = ctrl.pred_coef_q12[k >> 1];
        let mut a_q12_tmp = [0i16; MAX_LPC_ORDER];
        a_q12_tmp[..lpc_order].copy_from_slice(&a_q12[..lpc_order]);

        let b_q14_base = k * LTP_ORDER;
        let signal_type_orig = dec.indices.signal_type as i32;

        let gain_q10 = silk_rshift(ctrl.gains_q16[k], 6);
        let mut inv_gain_q31 = silk_inverse32_varq(ctrl.gains_q16[k], 47);

        // Calculate gain adjustment factor.
        let gain_adj_q16 = if ctrl.gains_q16[k] != dec.prev_gain_q16 {
            let g = silk_div32_varq(dec.prev_gain_q16, ctrl.gains_q16[k], 16);
            for v in s_lpc_q14.iter_mut() {
                *v = silk_smulww(g, *v);
            }
            g
        } else {
            1i32 << 16
        };

        debug_assert!(inv_gain_q31 != 0);
        dec.prev_gain_q16 = ctrl.gains_q16[k];

        // Avoid abrupt transition from voiced PLC to unvoiced normal decoding.
        let mut b_q14 = [0i16; LTP_ORDER];
        let mut signal_type = signal_type_orig;
        let mut pitch_l_k = ctrl.pitch_l[k];
        if dec.loss_cnt != 0 && dec.prev_signal_type == TYPE_VOICED && signal_type_orig != TYPE_VOICED && k < MAX_NB_SUBFR / 2
        {
            b_q14[LTP_ORDER / 2] = silk_fix_const(0.25, 14) as i16;
            signal_type = TYPE_VOICED;
            pitch_l_k = dec.lag_prev;
        } else {
            b_q14.copy_from_slice(&ctrl.ltp_coef_q14[b_q14_base..b_q14_base + LTP_ORDER]);
        }

        if signal_type == TYPE_VOICED {
            // Voiced.
            lag = pitch_l_k as usize;

            // Re-whitening.
            if k == 0 || (k == 2 && nlsf_interpolation_flag) {
                // Rewhiten with new A coefs.
                let start_idx = ltp_mem_length as isize - lag as isize - lpc_order as isize - (LTP_ORDER / 2) as isize;
                debug_assert!(start_idx > 0);
                let start_idx = start_idx as usize;

                if k == 2 {
                    dec.out_buf[ltp_mem_length..ltp_mem_length + 2 * subfr_length]
                        .copy_from_slice(&xq[..2 * subfr_length]);
                }

                let in_slice_start = start_idx + k * subfr_length;
                let filt_len = ltp_mem_length - start_idx;
                let mut in_local = [0i16; MAX_LTP_MEM_LENGTH];
                in_local[..filt_len].copy_from_slice(&dec.out_buf[in_slice_start..in_slice_start + filt_len]);
                let mut out_local = [0i16; MAX_LTP_MEM_LENGTH];
                silk_lpc_analysis_filter(&mut out_local[..filt_len], &in_local[..filt_len], &a_q12_tmp[..lpc_order], filt_len, lpc_order);
                s_ltp[start_idx..ltp_mem_length].copy_from_slice(&out_local[..filt_len]);

                // After rewhitening the LTP state is unscaled.
                if k == 0 {
                    // Do LTP downscaling to reduce inter-packet dependency.
                    inv_gain_q31 = silk_lshift(silk_smulwb(inv_gain_q31, ctrl.ltp_scale_q14), 2);
                }
                for i in 0..lag + LTP_ORDER / 2 {
                    s_ltp_q15[s_ltp_buf_idx - i - 1] = silk_smulwb(inv_gain_q31, s_ltp[ltp_mem_length - i - 1] as i32);
                }
            } else {
                // Update LTP state when Gain changes.
                if gain_adj_q16 != 1i32 << 16 {
                    for i in 0..lag + LTP_ORDER / 2 {
                        s_ltp_q15[s_ltp_buf_idx - i - 1] = silk_smulww(gain_adj_q16, s_ltp_q15[s_ltp_buf_idx - i - 1]);
                    }
                }
            }
        }

        // Long-term prediction.
        // `pres_q14` either points at `res_q14` (voiced) or is `dec.exc_q14[pexc_idx..]` (unvoiced).
        if signal_type == TYPE_VOICED {
            let pred_lag_base = s_ltp_buf_idx as isize - lag as isize + (LTP_ORDER / 2) as isize;
            for i in 0..subfr_length {
                let p = pred_lag_base + i as isize;
                // Unrolled loop. Avoids introducing a bias because silk_SMLAWB() always rounds
                // to -inf.
                let mut ltp_pred_q13: i32 = 2;
                ltp_pred_q13 = silk_smlawb(ltp_pred_q13, s_ltp_q15[(p) as usize], b_q14[0] as i32);
                ltp_pred_q13 = silk_smlawb(ltp_pred_q13, s_ltp_q15[(p - 1) as usize], b_q14[1] as i32);
                ltp_pred_q13 = silk_smlawb(ltp_pred_q13, s_ltp_q15[(p - 2) as usize], b_q14[2] as i32);
                ltp_pred_q13 = silk_smlawb(ltp_pred_q13, s_ltp_q15[(p - 3) as usize], b_q14[3] as i32);
                ltp_pred_q13 = silk_smlawb(ltp_pred_q13, s_ltp_q15[(p - 4) as usize], b_q14[4] as i32);

                // Generate LPC excitation.
                res_q14[i] = silk_add_lshift32(dec.exc_q14[pexc_idx + i], ltp_pred_q13, 1);

                // Update states.
                s_ltp_q15[s_ltp_buf_idx] = silk_lshift(res_q14[i], 1);
                s_ltp_buf_idx += 1;
            }
        } else {
            res_q14[..subfr_length].copy_from_slice(&dec.exc_q14[pexc_idx..pexc_idx + subfr_length]);
        }

        for i in 0..subfr_length {
            // Short-term prediction.
            debug_assert!(lpc_order == 10 || lpc_order == 16);
            // Avoids introducing a bias because silk_SMLAWB() always rounds to -inf.
            let mut lpc_pred_q10: i32 = (lpc_order as i32) >> 1;
            let base = MAX_LPC_ORDER + i;
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 1], a_q12_tmp[0] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 2], a_q12_tmp[1] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 3], a_q12_tmp[2] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 4], a_q12_tmp[3] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 5], a_q12_tmp[4] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 6], a_q12_tmp[5] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 7], a_q12_tmp[6] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 8], a_q12_tmp[7] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 9], a_q12_tmp[8] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 10], a_q12_tmp[9] as i32);
            if lpc_order == 16 {
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 11], a_q12_tmp[10] as i32);
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 12], a_q12_tmp[11] as i32);
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 13], a_q12_tmp[12] as i32);
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 14], a_q12_tmp[13] as i32);
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 15], a_q12_tmp[14] as i32);
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, s_lpc_q14[base - 16], a_q12_tmp[15] as i32);
            }

            // Add prediction to LPC excitation.
            s_lpc_q14[base] = silk_add_sat32(res_q14[i], silk_lshift_sat32(lpc_pred_q10, 4));

            // Scale with gain.
            xq[pxq_idx + i] = silk_sat16(silk_rshift_round(silk_smulww(s_lpc_q14[base], gain_q10), 8)) as i16;
        }

        // Update LPC filter state.
        for j in 0..MAX_LPC_ORDER {
            s_lpc_q14[j] = s_lpc_q14[subfr_length + j];
        }
        pexc_idx += subfr_length;
        pxq_idx += subfr_length;
    }

    // Save LPC state.
    dec.s_lpc_q14_buf.copy_from_slice(&s_lpc_q14[..MAX_LPC_ORDER]);
}
