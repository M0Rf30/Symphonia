// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Adaptive Mid/Side to Left/Right stereo conversion. Ported from libopus
//! `silk/stereo_MS_to_LR.c` (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::macros::{
    silk_add_lshift32, silk_div32_16, silk_lshift, silk_rshift_round, silk_sat16, silk_smlawb, silk_smulbb,
};
use crate::silk::structs::{StereoDecState, STEREO_INTERP_LEN_MS};

/// C: `silk_stereo_MS_to_LR`. Convert adaptive Mid/Side representation to Left/Right stereo
/// signal, in place.
///
/// `x1` ("Left input signal, becomes mid signal") and `x2` ("Right input signal, becomes side
/// signal") each hold two leading carried-over buffer samples plus `frame_length` samples of
/// current-frame PCM, matching the C indexing (`x1[0..=frame_length+1]`); callers MUST pass
/// slices of length `frame_length + 2`.
pub fn silk_stereo_ms_to_lr(
    state: &mut StereoDecState,
    x1: &mut [i16],
    x2: &mut [i16],
    pred_q13: &[i32; 2],
    fs_khz: i32,
    frame_length: usize,
) {
    debug_assert!(x1.len() >= frame_length + 2);
    debug_assert!(x2.len() >= frame_length + 2);

    // Buffering
    let s_mid_old = state.s_mid;
    let s_side_old = state.s_side;
    x1[0] = s_mid_old[0];
    x1[1] = s_mid_old[1];
    x2[0] = s_side_old[0];
    x2[1] = s_side_old[1];
    state.s_mid[0] = x1[frame_length];
    state.s_mid[1] = x1[frame_length + 1];
    state.s_side[0] = x2[frame_length];
    state.s_side[1] = x2[frame_length + 1];

    // Interpolate predictors and add prediction to side channel
    let mut pred0_q13 = state.pred_prev_q13[0];
    let mut pred1_q13 = state.pred_prev_q13[1];
    let interp_len = (STEREO_INTERP_LEN_MS * fs_khz) as usize;
    let denom_q16 = silk_div32_16(1i32 << 16, STEREO_INTERP_LEN_MS * fs_khz);
    let delta0_q13 = silk_rshift_round(silk_smulbb(pred_q13[0] - state.pred_prev_q13[0], denom_q16), 16);
    let delta1_q13 = silk_rshift_round(silk_smulbb(pred_q13[1] - state.pred_prev_q13[1], denom_q16), 16);
    for n in 0..interp_len {
        pred0_q13 += delta0_q13;
        pred1_q13 += delta1_q13;
        let mut sum = silk_lshift(
            silk_add_lshift32(x1[n] as i32 + x1[n + 2] as i32, x1[n + 1] as i32, 1),
            9,
        ); // Q11
        sum = silk_smlawb(silk_lshift(x2[n + 1] as i32, 8), sum, pred0_q13); // Q8
        sum = silk_smlawb(sum, silk_lshift(x1[n + 1] as i32, 11), pred1_q13); // Q8
        x2[n + 1] = silk_sat16(silk_rshift_round(sum, 8)) as i16;
    }
    pred0_q13 = pred_q13[0];
    pred1_q13 = pred_q13[1];
    for n in interp_len..frame_length {
        let mut sum = silk_lshift(
            silk_add_lshift32(x1[n] as i32 + x1[n + 2] as i32, x1[n + 1] as i32, 1),
            9,
        ); // Q11
        sum = silk_smlawb(silk_lshift(x2[n + 1] as i32, 8), sum, pred0_q13); // Q8
        sum = silk_smlawb(sum, silk_lshift(x1[n + 1] as i32, 11), pred1_q13); // Q8
        x2[n + 1] = silk_sat16(silk_rshift_round(sum, 8)) as i16;
    }
    state.pred_prev_q13[0] = pred_q13[0];
    state.pred_prev_q13[1] = pred_q13[1];

    // Convert to left/right signals
    for n in 0..frame_length {
        let sum = x1[n + 1] as i32 + x2[n + 1] as i32;
        let diff = x1[n + 1] as i32 - x2[n + 1] as i32;
        x1[n + 1] = silk_sat16(sum) as i16;
        x2[n + 1] = silk_sat16(diff) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Silence in, silence out: an all-zero mid/side buffer with zero predictors and zero
    /// carried-over state must decode to an all-zero left/right buffer.
    #[test]
    fn silence_in_silence_out() {
        let mut state = StereoDecState::default();
        let frame_length = 80usize;
        let fs_khz = 8i32;
        let mut x1 = vec![0i16; frame_length + 2];
        let mut x2 = vec![0i16; frame_length + 2];
        let pred_q13 = [0i32, 0i32];

        silk_stereo_ms_to_lr(&mut state, &mut x1, &mut x2, &pred_q13, fs_khz, frame_length);

        assert!(x1.iter().all(|&v| v == 0));
        assert!(x2.iter().all(|&v| v == 0));
        assert_eq!(state.pred_prev_q13, [0, 0]);
        assert_eq!(state.s_mid, [0, 0]);
        assert_eq!(state.s_side, [0, 0]);
    }
}
