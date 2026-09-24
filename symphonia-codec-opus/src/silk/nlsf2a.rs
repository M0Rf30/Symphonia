// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversion from normalized line spectral frequencies (NLSFs) to LPC (AR) prediction filter
//! coefficients, via a piecewise-linear cos(LSF) approximation. Ported from libopus
//! `silk/NLSF2A.c` (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::bwexpander::silk_bwexpander_32;
use crate::silk::lpc_fit::silk_lpc_fit;
use crate::silk::lpc_inv_pred_gain::silk_lpc_inverse_pred_gain;
use crate::silk::macros::{silk_lshift, silk_mul, silk_rshift, silk_rshift_round, silk_smull};
use crate::silk::structs::MAX_LPC_ORDER;
use crate::silk::tables::LSF_COS_TAB_FIX_Q12;

const QA: i32 = 16;
/// C: `MAX_LPC_STABILIZE_ITERATIONS` (`silk/define.h`).
const MAX_LPC_STABILIZE_ITERATIONS: i32 = 16;

const ORDERING16: [u8; 16] = [0, 15, 8, 7, 4, 11, 12, 3, 2, 13, 10, 5, 6, 9, 14, 1];
const ORDERING10: [u8; 10] = [0, 9, 6, 3, 4, 5, 8, 1, 2, 7];

/// C: `silk_NLSF2A_find_poly`. `out` has length `dd + 1`; `c_lsf` is stride-2 into the
/// interleaved `cos_lsf_qa` array (even indices for `P`, odd for `Q`).
fn nlsf2a_find_poly(out: &mut [i32], c_lsf: &[i32], dd: usize) {
    out[0] = 1 << QA;
    out[1] = -c_lsf[0];
    for k in 1..dd {
        let ftmp = c_lsf[2 * k];
        out[k + 1] = silk_lshift(out[k - 1], 1) - (silk_rshift_round64_i32(silk_smull(ftmp, out[k]), QA));
        let mut n = k;
        while n > 1 {
            out[n] += out[n - 2] - silk_rshift_round64_i32(silk_smull(ftmp, out[n - 1]), QA);
            n -= 1;
        }
        out[1] -= ftmp;
    }
}

#[inline]
fn silk_rshift_round64_i32(a: i64, shift: i32) -> i32 {
    crate::silk::macros::silk_rshift_round64(a, shift) as i32
}

/// C: `silk_NLSF2A`. Compute whitening filter coefficients (Q12) from normalized line spectral
/// frequencies (Q15). `d` (filter order) must be 10 or 16.
pub(crate) fn silk_nlsf2a(a_q12: &mut [i16], nlsf: &[i16], d: usize) {
    debug_assert!(d == 10 || d == 16);

    let ordering: &[u8] = if d == 16 { &ORDERING16 } else { &ORDERING10 };
    let mut cos_lsf_qa = [0i32; MAX_LPC_ORDER];

    // Convert LSFs to 2*cos(LSF), using piecewise linear curve from table.
    for k in 0..d {
        debug_assert!(nlsf[k] >= 0);

        // f_int on a scale 0-127 (rounded down).
        let f_int = silk_rshift(nlsf[k] as i32, 15 - 7);
        // f_frac, range: 0..255.
        let f_frac = nlsf[k] as i32 - silk_lshift(f_int, 15 - 7);

        debug_assert!(f_int >= 0);
        debug_assert!((f_int as usize) < LSF_COS_TAB_FIX_Q12.len() - 1);

        // Read start and end value from table.
        let cos_val = LSF_COS_TAB_FIX_Q12[f_int as usize] as i32;
        let delta = LSF_COS_TAB_FIX_Q12[f_int as usize + 1] as i32 - cos_val;

        // Linear interpolation.
        cos_lsf_qa[ordering[k] as usize] =
            silk_rshift_round(silk_lshift(cos_val, 8) + silk_mul(delta, f_frac), 20 - QA);
    }

    let dd = d >> 1;

    // Generate even and odd polynomials using convolution.
    let mut p = [0i32; MAX_LPC_ORDER / 2 + 1];
    let mut q = [0i32; MAX_LPC_ORDER / 2 + 1];
    nlsf2a_find_poly(&mut p[..dd + 1], &cos_lsf_qa[0..], dd);
    nlsf2a_find_poly(&mut q[..dd + 1], &cos_lsf_qa[1..], dd);

    // Convert even and odd polynomials to i32 Q12 filter coefs.
    let mut a32_qa1 = [0i32; MAX_LPC_ORDER];
    for k in 0..dd {
        let ptmp = p[k + 1] + p[k];
        let qtmp = q[k + 1] - q[k];

        // The Ptmp and Qtmp values at this stage need to fit in i32.
        a32_qa1[k] = -qtmp - ptmp; // QA+1
        a32_qa1[d - k - 1] = qtmp - ptmp; // QA+1
    }

    // Convert int32 coefficients to Q12 int16 coefs.
    silk_lpc_fit(a_q12, &mut a32_qa1[..d], 12, QA + 1, d);

    let mut i = 0;
    while silk_lpc_inverse_pred_gain(a_q12, d) == 0 && i < MAX_LPC_STABILIZE_ITERATIONS {
        // Prediction coefficients are (too close to) unstable; apply bandwidth expansion on
        // the unscaled coefficients, convert to Q12 and measure again.
        silk_bwexpander_32(&mut a32_qa1[..d], d, 65536 - (2 << i));
        for k in 0..d {
            a_q12[k] = silk_rshift_round(a32_qa1[k], QA + 1 - 12) as i16;
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_order_uniform_nlsf_is_stable() {
        // Uniformly spaced NLSFs (as at reset) should produce a stable, non-trivial filter.
        let mut nlsf = [0i16; 10];
        let step = 32767 / 11;
        let mut acc = 0i32;
        for n in nlsf.iter_mut() {
            acc += step;
            *n = acc as i16;
        }
        let mut a_q12 = [0i16; 10];
        silk_nlsf2a(&mut a_q12, &nlsf, 10);
        assert!(silk_lpc_inverse_pred_gain(&a_q12, 10) > 0);
    }

    #[test]
    fn sixteen_order_uniform_nlsf_is_stable() {
        let mut nlsf = [0i16; 16];
        let step = 32767 / 17;
        let mut acc = 0i32;
        for n in nlsf.iter_mut() {
            acc += step;
            *n = acc as i16;
        }
        let mut a_q12 = [0i16; 16];
        silk_nlsf2a(&mut a_q12, &nlsf, 16);
        assert!(silk_lpc_inverse_pred_gain(&a_q12, 16) > 0);
    }
}
