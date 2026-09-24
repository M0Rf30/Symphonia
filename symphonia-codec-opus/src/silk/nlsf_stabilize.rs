// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! NLSF stabilizer: moves NLSFs apart (and away from the [0, 1] borders) if they are too close,
//! with a high-effort minimum-Euclidean-distance modification, falling back to insertion sort +
//! clamping if that doesn't converge. Ported from libopus `silk/NLSF_stabilize.c`
//! (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::macros::{silk_limit, silk_max, silk_min, silk_rshift, silk_rshift_round, silk_sat16};
use crate::silk::sort::silk_insertion_sort_increasing_all_values_int16;

const MAX_LOOPS: i32 = 20;

/// C: `silk_NLSF_stabilize`. `nlsf_q15` has `l` entries; `n_delta_min_q15` has `l + 1` entries
/// (minimum spacing, including the two border gaps), with `n_delta_min_q15[l] >= 1`.
pub(crate) fn silk_nlsf_stabilize(nlsf_q15: &mut [i16], n_delta_min_q15: &[i16], l: usize) {
    debug_assert!(n_delta_min_q15[l] >= 1);

    let mut i_idx: usize;
    let mut converged = false;

    for _loop in 0..MAX_LOOPS {
        // Find smallest distance.
        // First element.
        let mut min_diff_q15 = nlsf_q15[0] as i32 - n_delta_min_q15[0] as i32;
        i_idx = 0;
        // Middle elements.
        for i in 1..l {
            let diff_q15 = nlsf_q15[i] as i32 - (nlsf_q15[i - 1] as i32 + n_delta_min_q15[i] as i32);
            if diff_q15 < min_diff_q15 {
                min_diff_q15 = diff_q15;
                i_idx = i;
            }
        }
        // Last element.
        let diff_q15 = (1i32 << 15) - (nlsf_q15[l - 1] as i32 + n_delta_min_q15[l] as i32);
        if diff_q15 < min_diff_q15 {
            min_diff_q15 = diff_q15;
            i_idx = l;
        }

        // Now check if the smallest distance is non-negative.
        if min_diff_q15 >= 0 {
            converged = true;
            break;
        }

        if i_idx == 0 {
            // Move away from lower limit.
            nlsf_q15[0] = n_delta_min_q15[0];
        } else if i_idx == l {
            // Move away from higher limit.
            nlsf_q15[l - 1] = ((1i32 << 15) - n_delta_min_q15[l] as i32) as i16;
        } else {
            // Find the lower extreme for the location of the current center frequency.
            let mut min_center_q15: i32 = 0;
            for k in 0..i_idx {
                min_center_q15 += n_delta_min_q15[k] as i32;
            }
            min_center_q15 += silk_rshift(n_delta_min_q15[i_idx] as i32, 1);

            // Find the upper extreme for the location of the current center frequency.
            let mut max_center_q15: i32 = 1 << 15;
            for k in (i_idx + 1..=l).rev() {
                max_center_q15 -= n_delta_min_q15[k] as i32;
            }
            max_center_q15 -= silk_rshift(n_delta_min_q15[i_idx] as i32, 1);

            // Move apart, sorted by value, keeping the same center frequency.
            let center_freq_q15 = silk_limit(
                silk_rshift_round(nlsf_q15[i_idx - 1] as i32 + nlsf_q15[i_idx] as i32, 1),
                min_center_q15,
                max_center_q15,
            ) as i16;
            nlsf_q15[i_idx - 1] = (center_freq_q15 as i32 - silk_rshift(n_delta_min_q15[i_idx] as i32, 1)) as i16;
            nlsf_q15[i_idx] = (nlsf_q15[i_idx - 1] as i32 + n_delta_min_q15[i_idx] as i32) as i16;
        }
    }

    if !converged {
        // Safe and simple fall back method, which is less ideal than the above.
        // Insertion sort (fast for already almost sorted arrays).
        silk_insertion_sort_increasing_all_values_int16(&mut nlsf_q15[..l]);

        // First NLSF should be no less than NDeltaMin[0].
        nlsf_q15[0] = silk_max(nlsf_q15[0], n_delta_min_q15[0]);

        // Keep delta_min distance between the NLSFs.
        for i in 1..l {
            nlsf_q15[i] = silk_max(nlsf_q15[i], silk_sat16(nlsf_q15[i - 1] as i32 + n_delta_min_q15[i] as i32) as i16);
        }

        // Last NLSF should be no higher than 1 - NDeltaMin[L].
        nlsf_q15[l - 1] = silk_min(nlsf_q15[l - 1], ((1i32 << 15) - n_delta_min_q15[l] as i32) as i16);

        // Keep NDeltaMin distance between the NLSFs.
        for i in (0..l - 1).rev() {
            nlsf_q15[i] = silk_min(nlsf_q15[i], (nlsf_q15[i + 1] as i32 - n_delta_min_q15[i + 1] as i32) as i16);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn already_stable_is_unchanged() {
        let delta = [100i16; 11];
        let mut nlsf: Vec<i16> = (1..=10).map(|k| k as i16 * 3000).collect();
        let before = nlsf.clone();
        silk_nlsf_stabilize(&mut nlsf, &delta, 10);
        assert_eq!(nlsf, before);
    }

    #[test]
    fn collapsed_values_are_separated() {
        let delta = [10i16; 11];
        let mut nlsf = [1000i16; 10];
        silk_nlsf_stabilize(&mut nlsf, &delta, 10);
        for i in 1..10 {
            assert!(nlsf[i] as i32 - nlsf[i - 1] as i32 >= delta[i] as i32 - 1);
        }
        assert!(nlsf[0] >= 0);
        assert!((nlsf[9] as i32) <= 32767);
    }
}
