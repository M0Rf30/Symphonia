// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pitch search, used only by CELT PLC. Ported from libopus `celt/pitch.c`
//! (`pitch_downsample`, `pitch_search`, `celt_pitch_xcorr`, `remove_doubling`).
//! Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltSynthesis".

/// C: `pitch_downsample`.
pub fn pitch_downsample(x: &[&[f32]], x_lp: &mut [f32], len: i32, channels: i32) {
    let _ = (x, x_lp, len, channels);
    todo!("wave 1 (celt/CeltSynthesis): pitch_downsample")
}

/// C: `pitch_search`. Returns the estimated pitch lag.
pub fn pitch_search(x_lp: &[f32], y: &[f32], len: i32, max_pitch: i32) -> i32 {
    let _ = (x_lp, y, len, max_pitch);
    todo!("wave 1 (celt/CeltSynthesis): pitch_search")
}

/// C: `remove_doubling`. Refines a coarse pitch estimate, returning the pitch gain (`Q15` in
/// the C fixed-point build; plain `f32` here since this crate's CELT is float).
#[allow(clippy::too_many_arguments)]
pub fn remove_doubling(
    x: &[f32],
    maxperiod: i32,
    minperiod: i32,
    n: i32,
    t0: &mut i32,
    prev_period: i32,
    prev_gain: f32,
) -> f32 {
    let _ = (x, maxperiod, minperiod, n, t0, prev_period, prev_gain);
    todo!("wave 1 (celt/CeltSynthesis): remove_doubling")
}
