// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Comfort noise generation (CNG): updates a smoothed NLSF/gain estimate during active silence
//! and synthesizes noise-filled residual during packet loss / DTX. Ported from libopus
//! `silk/CNG.c` (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::macros::{
    silk_add_sat16, silk_add_sat32, silk_lshift, silk_lshift_sat32, silk_rand, silk_rshift, silk_rshift_round,
    silk_sat16, silk_smlawb, silk_smultt, silk_smulwb, silk_smulww, silk_sqrt_approx, silk_sub_lshift32,
};
use crate::silk::nlsf2a::silk_nlsf2a;
use crate::silk::structs::{
    SilkDecoderControl, SilkDecoderState, CNG_BUF_MASK_MAX, CNG_GAIN_SMTH_Q16, CNG_GAIN_SMTH_THRESHOLD_Q16,
    CNG_NLSF_SMTH_Q16, MAX_LPC_ORDER, TYPE_NO_VOICE_ACTIVITY,
};

/// C: `silk_CNG_Reset`.
pub(crate) fn reset(dec: &mut SilkDecoderState) {
    let nlsf_step_q15 = i32::from(i16::MAX) / (dec.lpc_order + 1);
    let mut nlsf_acc_q15: i32 = 0;
    for i in 0..dec.lpc_order as usize {
        nlsf_acc_q15 += nlsf_step_q15;
        dec.s_cng.cng_smth_nlsf_q15[i] = nlsf_acc_q15 as i16;
    }
    dec.s_cng.cng_smth_gain_q16 = 0;
    dec.s_cng.rand_seed = 3_176_576;
}

/// C: `silk_CNG_exc`. Fills `exc_q14[0..length]` from `exc_buf_q14` (a `CNG_BUF_MASK_MAX + 1`
/// entry ring buffer), returning the updated `rand_seed`.
fn cng_exc(exc_q14: &mut [i32], exc_buf_q14: &[i32], length: usize, rand_seed: i32) -> i32 {
    let mut exc_mask = CNG_BUF_MASK_MAX;
    while exc_mask > length as i32 {
        exc_mask = silk_rshift(exc_mask, 1);
    }

    let mut seed = rand_seed;
    for e in exc_q14.iter_mut().take(length) {
        seed = silk_rand(seed);
        let idx = (silk_rshift(seed, 24) & exc_mask) as usize;
        *e = exc_buf_q14[idx];
    }
    seed
}

/// C: `silk_CNG`. Updates CNG estimate, and applies CNG when the packet was lost / during DTX.
/// `frame[0..length]` is the signal to update in place.
pub(crate) fn silk_cng(dec: &mut SilkDecoderState, ctrl: &SilkDecoderControl, frame: &mut [i16], length: usize) {
    if dec.fs_khz != dec.s_cng.fs_khz {
        reset(dec);
        dec.s_cng.fs_khz = dec.fs_khz;
    }

    if dec.loss_cnt == 0 && dec.prev_signal_type == TYPE_NO_VOICE_ACTIVITY {
        // Update CNG parameters.
        let order = dec.lpc_order as usize;

        // Smoothing of LSFs.
        for i in 0..order {
            let delta = dec.prev_nlsf_q15[i] as i32 - dec.s_cng.cng_smth_nlsf_q15[i] as i32;
            dec.s_cng.cng_smth_nlsf_q15[i] =
                (dec.s_cng.cng_smth_nlsf_q15[i] as i32 + silk_smulwb(delta, CNG_NLSF_SMTH_Q16)) as i16;
        }
        // Find the subframe with the highest gain.
        let mut max_gain_q16: i32 = 0;
        let mut subfr = 0usize;
        for i in 0..dec.nb_subfr as usize {
            if ctrl.gains_q16[i] > max_gain_q16 {
                max_gain_q16 = ctrl.gains_q16[i];
                subfr = i;
            }
        }
        // Update CNG excitation buffer with excitation from this subframe.
        let subfr_length = dec.subfr_length as usize;
        let nb_subfr = dec.nb_subfr as usize;
        dec.s_cng
            .cng_exc_buf_q14
            .copy_within(0..(nb_subfr - 1) * subfr_length, subfr_length);
        let src_start = subfr * subfr_length;
        let src: [i32; crate::silk::structs::MAX_SUB_FRAME_LENGTH] = {
            let mut tmp = [0i32; crate::silk::structs::MAX_SUB_FRAME_LENGTH];
            tmp[..subfr_length].copy_from_slice(&dec.exc_q14[src_start..src_start + subfr_length]);
            tmp
        };
        dec.s_cng.cng_exc_buf_q14[..subfr_length].copy_from_slice(&src[..subfr_length]);

        // Smooth gains.
        for i in 0..nb_subfr {
            dec.s_cng.cng_smth_gain_q16 =
                silk_smlawb(dec.s_cng.cng_smth_gain_q16, ctrl.gains_q16[i] - dec.s_cng.cng_smth_gain_q16, CNG_GAIN_SMTH_Q16);
            // If the smoothed gain is 3 dB greater than this subframe's gain, use this
            // subframe's gain to adapt faster.
            if silk_smulww(dec.s_cng.cng_smth_gain_q16, CNG_GAIN_SMTH_THRESHOLD_Q16) > ctrl.gains_q16[i] {
                dec.s_cng.cng_smth_gain_q16 = ctrl.gains_q16[i];
            }
        }
    }

    // Add CNG when packet is lost or during DTX.
    if dec.loss_cnt != 0 {
        let mut cng_sig_q14 = [0i32; crate::silk::structs::MAX_FRAME_LENGTH + MAX_LPC_ORDER];

        // Generate CNG excitation.
        let mut gain_q16 = silk_smulww(dec.s_plc.rand_scale_q14 as i32, dec.s_plc.prev_gain_q16[1]);
        if gain_q16 >= (1 << 21) || dec.s_cng.cng_smth_gain_q16 > (1 << 23) {
            gain_q16 = silk_smultt(gain_q16, gain_q16);
            gain_q16 = silk_sub_lshift32(silk_smultt(dec.s_cng.cng_smth_gain_q16, dec.s_cng.cng_smth_gain_q16), gain_q16, 5);
            gain_q16 = silk_lshift(silk_sqrt_approx(gain_q16), 16);
        } else {
            gain_q16 = silk_smulww(gain_q16, gain_q16);
            gain_q16 = silk_sub_lshift32(silk_smulww(dec.s_cng.cng_smth_gain_q16, dec.s_cng.cng_smth_gain_q16), gain_q16, 5);
            gain_q16 = silk_lshift(silk_sqrt_approx(gain_q16), 8);
        }
        let gain_q10 = silk_rshift(gain_q16, 6);

        dec.s_cng.rand_seed = cng_exc(&mut cng_sig_q14[MAX_LPC_ORDER..MAX_LPC_ORDER + length], &dec.s_cng.cng_exc_buf_q14, length, dec.s_cng.rand_seed);

        // Convert CNG NLSF to filter representation.
        let order = dec.lpc_order as usize;
        let mut a_q12 = [0i16; MAX_LPC_ORDER];
        silk_nlsf2a(&mut a_q12, &dec.s_cng.cng_smth_nlsf_q15, order);

        // Generate CNG signal, by synthesis filtering.
        cng_sig_q14[..MAX_LPC_ORDER].copy_from_slice(&dec.s_cng.cng_synth_state);
        debug_assert!(order == 10 || order == 16);
        for i in 0..length {
            // Avoids introducing a bias because silk_SMLAWB() always rounds to -inf.
            let mut lpc_pred_q10: i32 = (order as i32) >> 1;
            let base = MAX_LPC_ORDER + i;
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 1], a_q12[0] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 2], a_q12[1] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 3], a_q12[2] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 4], a_q12[3] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 5], a_q12[4] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 6], a_q12[5] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 7], a_q12[6] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 8], a_q12[7] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 9], a_q12[8] as i32);
            lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 10], a_q12[9] as i32);
            if order == 16 {
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 11], a_q12[10] as i32);
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 12], a_q12[11] as i32);
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 13], a_q12[12] as i32);
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 14], a_q12[13] as i32);
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 15], a_q12[14] as i32);
                lpc_pred_q10 = silk_smlawb(lpc_pred_q10, cng_sig_q14[base - 16], a_q12[15] as i32);
            }

            // Update states.
            cng_sig_q14[base] = silk_add_sat32(cng_sig_q14[base], silk_lshift_sat32(lpc_pred_q10, 4));

            // Scale with Gain and add to input signal.
            frame[i] = silk_add_sat16(frame[i], silk_sat16(silk_rshift_round(silk_smulww(cng_sig_q14[base], gain_q10), 8)) as i16);
        }
        dec.s_cng.cng_synth_state.copy_from_slice(&cng_sig_q14[length..length + MAX_LPC_ORDER]);
    } else {
        for v in dec.s_cng.cng_synth_state[..dec.lpc_order as usize].iter_mut() {
            *v = 0;
        }
    }
}
