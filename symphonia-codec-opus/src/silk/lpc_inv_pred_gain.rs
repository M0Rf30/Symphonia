// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Compute the inverse of the LPC prediction gain, and test filter stability. Ported from
//! libopus `silk/LPC_inv_pred_gain.c` (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::macros::{
    silk_abs, silk_clz32, silk_fix_const, silk_inverse32_varq, silk_lshift, silk_rshift_round64, silk_smmul,
    silk_smull, silk_sub32, silk_sub_sat32,
};
use crate::silk::structs::MAX_LPC_ORDER;

const QA: i32 = 24;
const A_LIMIT: i32 = silk_fix_const(0.99975, QA as u32);
/// C: `MAX_PREDICTION_POWER_GAIN` (`silk/define.h`).
const MAX_PREDICTION_POWER_GAIN: f64 = 1e4;

#[inline]
fn mul32_frac_q(a32: i32, b32: i32, q: i32) -> i32 {
    silk_rshift_round64(silk_smull(a32, b32), q) as i32
}

/// C: `LPC_inverse_pred_gain_QA_c`. Returns inverse prediction gain in energy domain, Q30 (0 if
/// unstable).
fn lpc_inverse_pred_gain_qa(a_qa: &mut [i32; MAX_LPC_ORDER], order: usize) -> i32 {
    let mut inv_gain_q30: i32 = silk_fix_const(1.0, 30);
    let mut k = order as i32 - 1;
    while k > 0 {
        let ku = k as usize;
        if a_qa[ku] > A_LIMIT || a_qa[ku] < -A_LIMIT {
            return 0;
        }

        // Set RC equal to negated AR coef.
        let rc_q31 = -silk_lshift(a_qa[ku], 31 - QA);

        // rc_mult1_Q30 range: [1 : 2^30].
        let rc_mult1_q30 = silk_sub32(silk_fix_const(1.0, 30), silk_smmul(rc_q31, rc_q31));

        // Update inverse gain. Range: [0 : 2^30].
        inv_gain_q30 = silk_lshift(silk_smmul(inv_gain_q30, rc_mult1_q30), 2);
        if inv_gain_q30 < silk_fix_const(1.0 / MAX_PREDICTION_POWER_GAIN, 30) {
            return 0;
        }

        // rc_mult2 range: [2^30 : i32::MAX].
        let mult2_q = 32 - silk_clz32(silk_abs(rc_mult1_q30));
        let rc_mult2 = silk_inverse32_varq(rc_mult1_q30, mult2_q + 30);

        // Update AR coefficient.
        let n_max = ((k + 1) >> 1) as usize;
        for n in 0..n_max {
            let tmp1 = a_qa[n];
            let tmp2 = a_qa[ku - n - 1];
            let t1 = silk_rshift_round64(
                silk_smull(silk_sub_sat32(tmp1, mul32_frac_q(tmp2, rc_q31, 31)), rc_mult2),
                mult2_q,
            );
            if t1 > i32::MAX as i64 || t1 < i32::MIN as i64 {
                return 0;
            }
            a_qa[n] = t1 as i32;
            let t2 = silk_rshift_round64(
                silk_smull(silk_sub_sat32(tmp2, mul32_frac_q(tmp1, rc_q31, 31)), rc_mult2),
                mult2_q,
            );
            if t2 > i32::MAX as i64 || t2 < i32::MIN as i64 {
                return 0;
            }
            a_qa[ku - n - 1] = t2 as i32;
        }
        k -= 1;
    }

    // k == 0 here.
    if a_qa[0] > A_LIMIT || a_qa[0] < -A_LIMIT {
        return 0;
    }
    let rc_q31 = -silk_lshift(a_qa[0], 31 - QA);
    let rc_mult1_q30 = silk_sub32(silk_fix_const(1.0, 30), silk_smmul(rc_q31, rc_q31));
    inv_gain_q30 = silk_lshift(silk_smmul(inv_gain_q30, rc_mult1_q30), 2);
    if inv_gain_q30 < silk_fix_const(1.0 / MAX_PREDICTION_POWER_GAIN, 30) {
        return 0;
    }

    inv_gain_q30
}

/// C: `silk_LPC_inverse_pred_gain_c`. Input coefficients in Q12. Returns inverse prediction
/// gain in energy domain, Q30 (0 if unstable).
pub(crate) fn silk_lpc_inverse_pred_gain(a_q12: &[i16], order: usize) -> i32 {
    let mut atmp_qa = [0i32; MAX_LPC_ORDER];
    let mut dc_resp: i32 = 0;
    for k in 0..order {
        dc_resp += a_q12[k] as i32;
        atmp_qa[k] = (a_q12[k] as i32) << (QA - 12);
    }
    // If the DC is unstable, we don't even need to do the full calculations.
    if dc_resp >= 4096 {
        return 0;
    }
    lpc_inverse_pred_gain_qa(&mut atmp_qa, order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_coefficients_are_maximally_stable() {
        let a_q12 = [0i16; 16];
        let gain = silk_lpc_inverse_pred_gain(&a_q12, 16);
        assert_eq!(gain, 1 << 30);
    }

    #[test]
    fn dc_unstable_returns_zero() {
        let a_q12 = [4096i16; 10];
        let gain = silk_lpc_inverse_pred_gain(&a_q12, 10);
        assert_eq!(gain, 0);
    }
}
