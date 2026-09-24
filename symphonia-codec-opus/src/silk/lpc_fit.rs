// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Convert `int32` LPC coefficients to `int16`, applying bandwidth expansion if needed to avoid
//! wraparound. Ported from libopus `silk/LPC_fit.c` (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::bwexpander::silk_bwexpander_32;
use crate::silk::macros::{
    silk_abs, silk_div32, silk_fix_const, silk_lshift, silk_min, silk_mul, silk_rshift, silk_rshift_round,
    silk_sat16, SILK_INT16_MAX,
};

/// C: `silk_LPC_fit`. Convert `int32` coefficients (Q`q_in`) to `int16` coefs (Q`q_out`),
/// applying bandwidth expansion to `a_qin` in place if necessary to prevent wraparound. This
/// logic is reused (independently) in CELT's `_celt_lpc`.
pub(crate) fn silk_lpc_fit(a_qout: &mut [i16], a_qin: &mut [i32], q_out: i32, q_in: i32, d: usize) {
    let mut idx = 0usize;
    let mut i = 0;

    // Limit the maximum absolute value of the prediction coefficients, so that they'll fit in i16.
    while i < 10 {
        // Find maximum absolute value and its index.
        let mut maxabs: i32 = 0;
        for k in 0..d {
            let absval = silk_abs(a_qin[k]);
            if absval > maxabs {
                maxabs = absval;
                idx = k;
            }
        }
        let maxabs_shifted = silk_rshift_round(maxabs, q_in - q_out);

        if maxabs_shifted > SILK_INT16_MAX {
            // Reduce magnitude of prediction coefficients.
            // (silk_int32_MAX >> 14) + silk_int16_MAX = 163838
            let maxabs_clamped = silk_min(maxabs_shifted, 163_838);
            let chirp_q16 = silk_fix_const(0.999, 16)
                - silk_div32(silk_lshift(maxabs_clamped - SILK_INT16_MAX, 14), silk_rshift(silk_mul(maxabs_clamped, idx as i32 + 1), 2));
            silk_bwexpander_32(a_qin, d, chirp_q16);
        } else {
            break;
        }
        i += 1;
    }

    if i == 10 {
        // Reached the last iteration, clip the coefficients.
        for k in 0..d {
            a_qout[k] = silk_sat16(silk_rshift_round(a_qin[k], q_in - q_out)) as i16;
            a_qin[k] = (a_qout[k] as i32) << (q_in - q_out);
        }
    } else {
        for k in 0..d {
            a_qout[k] = silk_rshift_round(a_qin[k], q_in - q_out) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_small_values() {
        let mut a_qin = [1000i32, -2000, 500, 0, 0, 0, 0, 0, 0, 0];
        let mut a_qout = [0i16; 10];
        silk_lpc_fit(&mut a_qout, &mut a_qin, 12, 17, 10);
        assert_eq!(a_qout[0], (1000i32 >> 5) as i16);
    }
}
