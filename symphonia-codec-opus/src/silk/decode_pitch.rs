// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pitch lag decoding: reconstruct the per-subframe pitch lags from the lag index + contour
//! index codebook selection. Ported from libopus `silk/decode_pitch.c` (BSD-3-Clause), see
//! NOTICE.

#![allow(dead_code)]

use crate::silk::macros::{silk_limit, silk_smulbb};
use crate::silk::structs::MAX_NB_SUBFR;
use crate::silk::tables::{CB_LAGS_STAGE2, CB_LAGS_STAGE2_10_MS, CB_LAGS_STAGE3, CB_LAGS_STAGE3_10_MS};

/// C: `PE_MIN_LAG_MS`/`PE_MAX_LAG_MS` (`silk/pitch_est_defines.h`).
const PE_MIN_LAG_MS: i32 = 2;
const PE_MAX_LAG_MS: i32 = 18;

/// C: `silk_decode_pitch`. Fills `pitch_lags[0..nb_subfr]`.
pub(crate) fn silk_decode_pitch(
    lag_index: i16,
    contour_index: i8,
    pitch_lags: &mut [i32; MAX_NB_SUBFR],
    fs_khz: i32,
    nb_subfr: usize,
) {
    let ci = contour_index as usize;
    let min_lag = silk_smulbb(PE_MIN_LAG_MS, fs_khz);
    let max_lag = silk_smulbb(PE_MAX_LAG_MS, fs_khz);
    let lag = min_lag + lag_index as i32;

    for k in 0..nb_subfr {
        let delta = if fs_khz == 8 {
            if nb_subfr == MAX_NB_SUBFR {
                CB_LAGS_STAGE2[k][ci] as i32
            } else {
                CB_LAGS_STAGE2_10_MS[k][ci] as i32
            }
        } else if nb_subfr == MAX_NB_SUBFR {
            CB_LAGS_STAGE3[k][ci] as i32
        } else {
            CB_LAGS_STAGE3_10_MS[k][ci] as i32
        };
        pitch_lags[k] = silk_limit(lag + delta, min_lag, max_lag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wb_20ms_lag_within_bounds() {
        let mut lags = [0i32; MAX_NB_SUBFR];
        silk_decode_pitch(50, 5, &mut lags, 16, 4);
        for &l in lags.iter() {
            assert!(l >= 2 * 16 && l <= 18 * 16);
        }
    }

    #[test]
    fn nb_10ms_lag_within_bounds() {
        let mut lags = [0i32; MAX_NB_SUBFR];
        silk_decode_pitch(20, 1, &mut lags, 8, 2);
        for &l in lags[..2].iter() {
            assert!(l >= 2 * 8 && l <= 18 * 8);
        }
    }
}
