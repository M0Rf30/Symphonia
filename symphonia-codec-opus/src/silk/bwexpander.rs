// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bandwidth expansion (chirp) of LPC/AR coefficients. Ported from libopus `silk/bwexpander.c`
//! and `silk/bwexpander_32.c` (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::macros::{silk_mul, silk_rshift_round, silk_smulww};

/// C: `silk_bwexpander`. Chirp (bandwidth expand) LP AR filter, Q12 coefficients.
pub(crate) fn silk_bwexpander(ar: &mut [i16], d: usize, chirp_q16_init: i32) {
    let mut chirp_q16 = chirp_q16_init;
    let chirp_minus_one_q16 = chirp_q16 - 65536;

    // NB: Don't use silk_SMULWB, instead of silk_RSHIFT_ROUND(silk_MUL(), 16), below.
    // Bias in silk_SMULWB can lead to unstable filters.
    for i in 0..d - 1 {
        ar[i] = silk_rshift_round(silk_mul(chirp_q16, ar[i] as i32), 16) as i16;
        chirp_q16 += silk_rshift_round(silk_mul(chirp_q16, chirp_minus_one_q16), 16);
    }
    ar[d - 1] = silk_rshift_round(silk_mul(chirp_q16, ar[d - 1] as i32), 16) as i16;
}

/// C: `silk_bwexpander_32`. Chirp (bandwidth expand) LP AR filter, Q(anything) 32-bit
/// coefficients.
pub(crate) fn silk_bwexpander_32(ar: &mut [i32], d: usize, chirp_q16_init: i32) {
    let mut chirp_q16 = chirp_q16_init;
    let chirp_minus_one_q16 = chirp_q16 - 65536;

    for i in 0..d - 1 {
        ar[i] = silk_smulww(chirp_q16, ar[i]);
        chirp_q16 += silk_rshift_round(silk_mul(chirp_q16, chirp_minus_one_q16), 16);
    }
    ar[d - 1] = silk_smulww(chirp_q16, ar[d - 1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bwexpander_shrinks_magnitude() {
        let mut ar = [1000i16, -2000, 3000, -4000, 500, 600, 700, 800, 900, 1000];
        let before: i32 = ar.iter().map(|&x| (x as i32).abs()).sum();
        silk_bwexpander(&mut ar, 10, 60000);
        let after: i32 = ar.iter().map(|&x| (x as i32).abs()).sum();
        assert!(after <= before);
    }

    #[test]
    fn bwexpander_32_identity_at_full_chirp() {
        let mut ar = [100_000i32, -200_000, 300_000];
        silk_bwexpander_32(&mut ar, 3, 65536);
        // chirp = 1.0 (Q16) throughout -> unchanged (silk_SMULWW(x, 1<<16) == x)
        assert_eq!(ar, [100_000, -200_000, 300_000]);
    }
}
