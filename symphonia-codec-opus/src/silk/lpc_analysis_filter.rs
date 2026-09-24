// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! LPC analysis (whitening/re-whitening) filter: state is always zero at the start, and the
//! first `d` output samples are set to zero. Ported from libopus `silk/LPC_analysis_filter.c`
//! (the non-`USE_CELT_FIR` C path only -- the FIR-based fast path is an equivalent
//! optimization, not a different result) (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::macros::{silk_lshift, silk_rshift_round, silk_sat16, silk_smlabb_ovflw, silk_smulbb, silk_sub32_ovflw};

/// C: `silk_LPC_analysis_filter`. `out[0..len]` receives the filtered signal (first `d` samples
/// zeroed); `in_sig[0..len]` is the input; `b` holds `d` Q12 MA prediction coefficients.
/// `d` must be even and `>= 6`, and `d <= len`.
pub(crate) fn silk_lpc_analysis_filter(out: &mut [i16], in_sig: &[i16], b: &[i16], len: usize, d: usize) {
    debug_assert!(d >= 6);
    debug_assert!(d % 2 == 0);
    debug_assert!(d <= len);

    for ix in d..len {
        // `in_ptr` conceptually points at `in_sig[ix - 1]`; indices below are relative to that.
        let in_ptr = ix - 1;

        let mut out32_q12 = silk_smulbb(in_sig[in_ptr] as i32, b[0] as i32);
        // Allowing wrap around so that two wraps can cancel each other. The rare cases where the
        // result wraps around can only be triggered by invalid streams.
        out32_q12 = silk_smlabb_ovflw(out32_q12, in_sig[in_ptr - 1] as i32, b[1] as i32);
        out32_q12 = silk_smlabb_ovflw(out32_q12, in_sig[in_ptr - 2] as i32, b[2] as i32);
        out32_q12 = silk_smlabb_ovflw(out32_q12, in_sig[in_ptr - 3] as i32, b[3] as i32);
        out32_q12 = silk_smlabb_ovflw(out32_q12, in_sig[in_ptr - 4] as i32, b[4] as i32);
        out32_q12 = silk_smlabb_ovflw(out32_q12, in_sig[in_ptr - 5] as i32, b[5] as i32);
        let mut j = 6;
        while j < d {
            out32_q12 = silk_smlabb_ovflw(out32_q12, in_sig[in_ptr - j] as i32, b[j] as i32);
            out32_q12 = silk_smlabb_ovflw(out32_q12, in_sig[in_ptr - j - 1] as i32, b[j + 1] as i32);
            j += 2;
        }

        // Subtract prediction.
        out32_q12 = silk_sub32_ovflw(silk_lshift(in_sig[in_ptr + 1] as i32, 12), out32_q12);

        // Scale to Q0.
        let out32 = silk_rshift_round(out32_q12, 12);

        // Saturate output.
        out[ix] = silk_sat16(out32) as i16;
    }

    // Set first d output samples to zero.
    for o in out[..d].iter_mut() {
        *o = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_input_gives_zero_output() {
        let input = [0i16; 40];
        let b = [100i16; 10];
        let mut out = [0i16; 40];
        silk_lpc_analysis_filter(&mut out, &input, &b, 40, 10);
        assert!(out.iter().all(|&x| x == 0));
    }

    #[test]
    fn leading_d_samples_are_zeroed() {
        let input = [1000i16; 40];
        let b = [50i16; 10];
        let mut out = [123i16; 40];
        silk_lpc_analysis_filter(&mut out, &input, &b, 40, 10);
        assert!(out[..10].iter().all(|&x| x == 0));
    }
}
