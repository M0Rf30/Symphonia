// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Gain scalar dequantization. Ported from libopus `silk/gain_quant.c` (decoder side only --
//! `silk_gains_quant`/`silk_gains_ID` are encoder-only and excluded). BSD-3-Clause, see NOTICE.

use crate::silk::macros::*;
use crate::silk::structs::{MAX_DELTA_GAIN_QUANT, MAX_NB_SUBFR, MIN_DELTA_GAIN_QUANT, MAX_QGAIN_DB, MIN_QGAIN_DB, N_LEVELS_QGAIN};

const OFFSET: i32 = (MIN_QGAIN_DB * 128) / 6 + 16 * 128;
const INV_SCALE_Q16: i32 = (65536 * (((MAX_QGAIN_DB - MIN_QGAIN_DB) * 128) / 6)) / (N_LEVELS_QGAIN - 1);

/// C: `silk_gains_dequant`. Gains scalar dequantization, uniform on log scale.
pub fn silk_gains_dequant(
    gain_q16: &mut [i32; MAX_NB_SUBFR],
    ind: &[i8; MAX_NB_SUBFR],
    prev_ind: &mut i8,
    conditional: bool,
    nb_subfr: usize,
) {
    let mut prev = *prev_ind as i32;
    for k in 0..nb_subfr {
        if k == 0 && !conditional {
            // Gain index is not allowed to go down more than 16 steps (~21.8 dB).
            prev = silk_max(ind[k] as i32, prev - 16);
        }
        else {
            // Delta index.
            let ind_tmp = ind[k] as i32 + MIN_DELTA_GAIN_QUANT;

            // Accumulate deltas.
            let double_step_size_threshold = 2 * MAX_DELTA_GAIN_QUANT - N_LEVELS_QGAIN + prev;
            if ind_tmp > double_step_size_threshold {
                prev += silk_lshift(ind_tmp, 1) - double_step_size_threshold;
            }
            else {
                prev += ind_tmp;
            }
        }
        prev = silk_limit(prev, 0, N_LEVELS_QGAIN - 1);

        // Scale and convert to linear scale.
        gain_q16[k] = silk_log2lin(silk_min(silk_smulwb(INV_SCALE_Q16, prev) + OFFSET, 3967));
    }
    *prev_ind = prev as i8;
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dequant_independent_first_subframe() {
        let mut gains = [0i32; MAX_NB_SUBFR];
        let ind = [32i8, 0, 0, 0];
        let mut prev = 0i8;
        silk_gains_dequant(&mut gains, &ind, &mut prev, false, 1);
        assert_eq!(prev, 32);
        assert!(gains[0] > 0);
    }
}
