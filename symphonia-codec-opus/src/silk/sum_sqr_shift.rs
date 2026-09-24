// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Compute the energy of an `i16` vector along with the right-shift needed for it to fit in an
//! `i32`. Ported from libopus `silk/sum_sqr_shift.c` (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::macros::{silk_clz32, silk_max, silk_smlabb_ovflw, silk_smulbb};

/// C: `silk_sum_sqr_shift`. Returns `(energy, shift)`.
pub(crate) fn silk_sum_sqr_shift(x: &[i16]) -> (i32, i32) {
    let len = x.len() as i32;

    // Do a first run with the maximum shift we could have.
    let mut shft = 31 - silk_clz32(len);
    // Let's be conservative with rounding and start with nrg = len.
    let mut nrg: i32 = len;
    let mut i = 0usize;
    while i + 1 < x.len() {
        let mut nrg_tmp = silk_smulbb(x[i] as i32, x[i] as i32) as u32;
        nrg_tmp = silk_smlabb_ovflw(nrg_tmp as i32, x[i + 1] as i32, x[i + 1] as i32) as u32;
        nrg = ((nrg as i64) + ((nrg_tmp as i64) >> shft)) as i32;
        i += 2;
    }
    if i < x.len() {
        let nrg_tmp = silk_smulbb(x[i] as i32, x[i] as i32) as u32;
        nrg = ((nrg as i64) + ((nrg_tmp as i64) >> shft)) as i32;
    }
    debug_assert!(nrg >= 0);

    // Make sure the result will fit in a 32-bit signed integer with two bits of headroom.
    shft = silk_max(0, shft + 3 - silk_clz32(nrg));
    nrg = 0;
    i = 0;
    while i + 1 < x.len() {
        let mut nrg_tmp = silk_smulbb(x[i] as i32, x[i] as i32) as u32;
        nrg_tmp = silk_smlabb_ovflw(nrg_tmp as i32, x[i + 1] as i32, x[i + 1] as i32) as u32;
        nrg = ((nrg as i64) + ((nrg_tmp as i64) >> shft)) as i32;
        i += 2;
    }
    if i < x.len() {
        let nrg_tmp = silk_smulbb(x[i] as i32, x[i] as i32) as u32;
        nrg = ((nrg as i64) + ((nrg_tmp as i64) >> shft)) as i32;
    }
    debug_assert!(nrg >= 0);

    (nrg, shft)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_signal_has_zero_energy() {
        let x = [0i16; 40];
        let (energy, _shift) = silk_sum_sqr_shift(&x);
        assert_eq!(energy, 0);
    }

    #[test]
    fn nonzero_signal_has_positive_energy() {
        let x = [1000i16, -2000, 3000, -1500, 500];
        let (energy, shift) = silk_sum_sqr_shift(&x);
        assert!(energy > 0);
        assert!(shift >= 0);
    }
}
