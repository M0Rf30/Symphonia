// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Linear prediction, used only by CELT PLC. Ported from libopus `celt/celt_lpc.c`
//! (`_celt_lpc`, `celt_fir`, `celt_iir`, `_celt_autocorr`). Ported from libopus
//! (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltSynthesis".

/// C: `_celt_autocorr`.
pub fn celt_autocorr(x: &[f32], ac: &mut [f32], window: Option<&[f32]>, overlap: i32, lag: i32, n: i32) {
    let _ = (x, ac, window, overlap, lag, n);
    todo!("wave 1 (celt/CeltSynthesis): _celt_autocorr")
}

/// C: `_celt_lpc`. Computes LPC coefficients `lpc` (length `p`) from autocorrelation `ac`.
pub fn celt_lpc(lpc: &mut [f32], ac: &[f32], p: i32) {
    let _ = (lpc, ac, p);
    todo!("wave 1 (celt/CeltSynthesis): _celt_lpc")
}

/// C: `celt_fir` (FIR filtering, used by PLC burg extrapolation).
pub fn celt_fir(x: &[f32], num: &[f32], y: &mut [f32], n: i32, ord: i32, mem: &mut [f32]) {
    let _ = (x, num, y, n, ord, mem);
    todo!("wave 1 (celt/CeltSynthesis): celt_fir")
}

/// C: `celt_iir`.
pub fn celt_iir(x: &[f32], den: &[f32], y: &mut [f32], n: i32, ord: i32, mem: &mut [f32]) {
    let _ = (x, den, y, n, ord, mem);
    todo!("wave 1 (celt/CeltSynthesis): celt_iir")
}
