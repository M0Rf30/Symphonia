// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! NLSF vector decoder: predictive residual dequantization plus inverse-weighted first-stage
//! codebook lookup, followed by stabilization. Ported from libopus `silk/NLSF_decode.c`
//! (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::macros::{silk_add16, silk_div32_16, silk_fix_const, silk_limit, silk_lshift, silk_smlawb, silk_smulbb, silk_sub16};
use crate::silk::nlsf_stabilize::silk_nlsf_stabilize;
use crate::silk::nlsf_unpack::silk_nlsf_unpack;
use crate::silk::structs::{NlsfCbStruct, MAX_LPC_ORDER};

/// C: `NLSF_QUANT_LEVEL_ADJ` (`silk/define.h`).
const NLSF_QUANT_LEVEL_ADJ: f64 = 0.1;

/// C: `silk_NLSF_residual_dequant`. Predictive dequantizer for NLSF residuals. `indices` and
/// `pred_coef_q8` each have `order` entries; `x_q10` (output) has `order` entries.
fn nlsf_residual_dequant(x_q10: &mut [i16], indices: &[i8], pred_coef_q8: &[u8], quant_step_size_q16: i32, order: usize) {
    let mut out_q10: i32 = 0;
    for i in (0..order).rev() {
        let pred_q10 = (silk_smulbb(out_q10, pred_coef_q8[i] as i16 as i32)) >> 8;
        out_q10 = silk_lshift(indices[i] as i32, 10);
        if out_q10 > 0 {
            out_q10 = silk_sub16(out_q10 as i16, silk_fix_const(NLSF_QUANT_LEVEL_ADJ, 10) as i16) as i32;
        } else if out_q10 < 0 {
            out_q10 = silk_add16(out_q10 as i16, silk_fix_const(NLSF_QUANT_LEVEL_ADJ, 10) as i16) as i32;
        }
        out_q10 = silk_smlawb(pred_q10, out_q10, quant_step_size_q16);
        x_q10[i] = out_q10 as i16;
    }
}

/// C: `silk_NLSF_decode`. `nlsf_indices` has `order + 1` entries (`NLSFIndices[0]` is the CB1
/// index, the rest are per-coefficient residual indices).
pub(crate) fn silk_nlsf_decode(p_nlsf_q15: &mut [i16], nlsf_indices: &[i8], ps_nlsf_cb: &NlsfCbStruct) {
    let order = ps_nlsf_cb.order as usize;
    let mut pred_q8 = [0u8; MAX_LPC_ORDER];
    let mut ec_ix = [0i16; MAX_LPC_ORDER];
    let mut res_q10 = [0i16; MAX_LPC_ORDER];

    // Unpack entropy table indices and predictor for current CB1 index.
    let cb1_index = nlsf_indices[0] as usize;
    silk_nlsf_unpack(&mut ec_ix, &mut pred_q8, ps_nlsf_cb, cb1_index);

    // Predictive residual dequantizer.
    nlsf_residual_dequant(
        &mut res_q10[..order],
        &nlsf_indices[1..=order],
        &pred_q8[..order],
        ps_nlsf_cb.quant_step_size_q16 as i32,
        order,
    );

    // Apply inverse square-rooted weights to first stage and add to output.
    let cb_element = &ps_nlsf_cb.cb1_nlsf_q8[cb1_index * order..cb1_index * order + order];
    let cb_wght = &ps_nlsf_cb.cb1_wght_q9[cb1_index * order..cb1_index * order + order];
    for i in 0..order {
        let nlsf_q15_tmp =
            silk_div32_16(silk_lshift(res_q10[i] as i32, 14), cb_wght[i] as i32) + silk_lshift(cb_element[i] as i32, 7);
        p_nlsf_q15[i] = silk_limit(nlsf_q15_tmp, 0, 32767) as i16;
    }

    // NLSF stabilization.
    silk_nlsf_stabilize(&mut p_nlsf_q15[..order], ps_nlsf_cb.delta_min_q15, order);
}
