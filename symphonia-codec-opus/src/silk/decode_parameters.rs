// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decode per-subframe parameters (gains, NLSFs/LPC coefficients, pitch lags, LTP
//! coefficients/scaling) from previously-decoded indices. Ported from libopus
//! `silk/decode_parameters.c` (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::bwexpander::silk_bwexpander;
use crate::silk::decode_pitch::silk_decode_pitch;
use crate::silk::gain_quant::silk_gains_dequant;
use crate::silk::macros::silk_lshift;
use crate::silk::nlsf2a::silk_nlsf2a;
use crate::silk::nlsf_decode::silk_nlsf_decode;
use crate::silk::structs::{SilkDecoderControl, SilkDecoderState, BWE_AFTER_LOSS_Q16, CODE_CONDITIONALLY, LTP_ORDER, MAX_LPC_ORDER, TYPE_VOICED};
use crate::silk::tables::{LTPSCALES_TABLE_Q14, LTP_VQ_PTRS_Q7};

/// C: `silk_decode_parameters`.
pub(crate) fn silk_decode_parameters(dec: &mut SilkDecoderState, ctrl: &mut SilkDecoderControl, cond_coding: i32) {
    let nb_subfr = dec.nb_subfr as usize;

    // Dequant gains.
    silk_gains_dequant(
        &mut ctrl.gains_q16,
        &dec.indices.gains_indices,
        &mut dec.last_gain_index,
        cond_coding == CODE_CONDITIONALLY,
        nb_subfr,
    );

    // Decode NLSFs.
    let mut p_nlsf_q15 = [0i16; MAX_LPC_ORDER];
    silk_nlsf_decode(&mut p_nlsf_q15, &dec.indices.nlsf_indices, dec.ps_nlsf_cb);

    // Convert NLSF parameters to AR prediction filter coefficients.
    let order = dec.lpc_order as usize;
    silk_nlsf2a(&mut ctrl.pred_coef_q12[1], &p_nlsf_q15, order);

    // If just reset, e.g., because internal Fs changed, do not allow interpolation improves the
    // case of packet loss in the first frame after a switch.
    if dec.first_frame_after_reset {
        dec.indices.nlsf_interp_coef_q2 = 4;
    }

    if dec.indices.nlsf_interp_coef_q2 < 4 {
        // Calculation of the interpolated NLSF0 vector from the interpolation factor, the
        // previous NLSF1, and the current NLSF1.
        let mut p_nlsf0_q15 = [0i16; MAX_LPC_ORDER];
        for i in 0..order {
            p_nlsf0_q15[i] = (dec.prev_nlsf_q15[i] as i32
                + ((dec.indices.nlsf_interp_coef_q2 as i32 * (p_nlsf_q15[i] as i32 - dec.prev_nlsf_q15[i] as i32)) >> 2))
                as i16;
        }

        // Convert NLSF parameters to AR prediction filter coefficients.
        silk_nlsf2a(&mut ctrl.pred_coef_q12[0], &p_nlsf0_q15, order);
    } else {
        // Copy LPC coefficients for first half from second half.
        let second = ctrl.pred_coef_q12[1];
        ctrl.pred_coef_q12[0][..order].copy_from_slice(&second[..order]);
    }

    dec.prev_nlsf_q15[..order].copy_from_slice(&p_nlsf_q15[..order]);

    // After a packet loss do BWE of LPC coefs.
    if dec.loss_cnt != 0 {
        silk_bwexpander(&mut ctrl.pred_coef_q12[0][..order], order, BWE_AFTER_LOSS_Q16);
        silk_bwexpander(&mut ctrl.pred_coef_q12[1][..order], order, BWE_AFTER_LOSS_Q16);
    }

    if dec.indices.signal_type == TYPE_VOICED as i8 {
        // Decode pitch lags.
        // Decode pitch values.
        silk_decode_pitch(dec.indices.lag_index, dec.indices.contour_index, &mut ctrl.pitch_l, dec.fs_khz, nb_subfr);

        // Decode codebook index: set pointer to start of codebook.
        let cbk_ptr_q7 = LTP_VQ_PTRS_Q7[dec.indices.per_index as usize];

        for k in 0..nb_subfr {
            let ix = dec.indices.ltp_index[k] as usize;
            for i in 0..LTP_ORDER {
                ctrl.ltp_coef_q14[k * LTP_ORDER + i] = silk_lshift(cbk_ptr_q7[ix * LTP_ORDER + i] as i32, 7) as i16;
            }
        }

        // Decode LTP scaling.
        let ix = dec.indices.ltp_scale_index as usize;
        ctrl.ltp_scale_q14 = LTPSCALES_TABLE_Q14[ix] as i32;
    } else {
        for k in 0..nb_subfr {
            ctrl.pitch_l[k] = 0;
        }
        for c in ctrl.ltp_coef_q14[..LTP_ORDER * nb_subfr].iter_mut() {
            *c = 0;
        }
        dec.indices.per_index = 0;
        ctrl.ltp_scale_q14 = 0;
    }
}
