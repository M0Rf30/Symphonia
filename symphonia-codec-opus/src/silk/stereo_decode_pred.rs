// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Stereo predictor entropy decoding. Ported from libopus `silk/stereo_decode_pred.c`
//! (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::range::RangeDecoder;
use crate::silk::macros::{silk_div32_16, silk_fix_const, silk_smlabb, silk_smulwb};
use crate::silk::tables::{
    STEREO_ONLY_CODE_MID_ICDF, STEREO_PRED_JOINT_ICDF, STEREO_PRED_QUANT_Q13, UNIFORM3_ICDF,
    UNIFORM5_ICDF,
};

/// Number of quantization sub-steps used when dequantizing the stereo predictors.
/// C: `STEREO_QUANT_SUB_STEPS`.
const STEREO_QUANT_SUB_STEPS: i32 = 5;

/// C: `silk_stereo_decode_pred`. Decode mid/side predictors.
pub fn silk_stereo_decode_pred(rd: &mut RangeDecoder<'_>, pred_q13: &mut [i32; 2]) {
    let mut ix = [[0i32; 3]; 2];

    // Entropy decoding
    let n = rd.dec_icdf(STEREO_PRED_JOINT_ICDF, 8);
    ix[0][2] = silk_div32_16(n, 5);
    ix[1][2] = n - 5 * ix[0][2];
    for item in ix.iter_mut() {
        item[0] = rd.dec_icdf(UNIFORM3_ICDF, 8);
        item[1] = rd.dec_icdf(UNIFORM5_ICDF, 8);
    }

    // Dequantize
    for n in 0..2 {
        ix[n][0] += 3 * ix[n][2];
        let low_q13 = STEREO_PRED_QUANT_Q13[ix[n][0] as usize] as i32;
        let step_q13 = silk_smulwb(
            STEREO_PRED_QUANT_Q13[(ix[n][0] + 1) as usize] as i32 - low_q13,
            silk_fix_const(0.5 / STEREO_QUANT_SUB_STEPS as f64, 16),
        );
        pred_q13[n] = silk_smlabb(low_q13, step_q13, 2 * ix[n][1] + 1);
    }

    // Subtract second from first predictor (helps when actually applying these)
    pred_q13[0] -= pred_q13[1];
}

/// C: `silk_stereo_decode_mid_only`. Decode mid-only flag. Returns `decode_only_mid`, the
/// flag that only the mid channel has been coded (the C out-param `opus_int *decode_only_mid`
/// is always 0/1, so a `bool` return is equivalent).
pub fn silk_stereo_decode_mid_only(rd: &mut RangeDecoder<'_>) -> bool {
    rd.dec_icdf(STEREO_ONLY_CODE_MID_ICDF, 8) != 0
}
