// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared CELT decode-path helpers with no single natural C file (spread across
//! `celt/celt_decoder.c`'s file-local statics). Ported from libopus (BSD-3-Clause), see
//! NOTICE. Owner (wave 1): "CeltSynthesis".

/// C: `comb_filter` (post-filter applied after synthesis when post-filter params were coded).
#[allow(clippy::too_many_arguments)]
pub fn comb_filter(
    y: &mut [f32],
    x: &[f32],
    t0: i32,
    t1: i32,
    n: i32,
    g0: f32,
    g1: f32,
    tapset0: i32,
    tapset1: i32,
) {
    let _ = (y, x, t0, t1, n, g0, g1, tapset0, tapset1);
    todo!("wave 1 (celt/CeltSynthesis): comb_filter")
}

/// C: `tf_select_table` — a static lookup, not a function, but declared here as a `todo!`-
/// backed accessor until wave 1 transcribes the table so `decoder.rs` has something to call.
pub fn tf_select_table(lm: i32, is_transient: bool, tf_select: i32, is_hybrid: bool, tf_res_index: i32) -> i32 {
    let _ = (lm, is_transient, tf_select, is_hybrid, tf_res_index);
    todo!("wave 1 (celt/CeltSynthesis): tf_select_table lookup")
}

/// C: `resampling_factor`.
pub fn resampling_factor(rate: i32) -> i32 {
    let _ = rate;
    todo!("wave 1 (celt/CeltSynthesis): resampling_factor")
}

/// C: `init_caps` (per-band bit-allocation caps for the current channel count/LM).
pub fn init_caps(mode: &crate::celt::modes::CeltMode, caps: &mut [i32], lm: i32, channels: i32) {
    let _ = (mode, caps, lm, channels);
    todo!("wave 1 (celt/CeltSynthesis): init_caps")
}
