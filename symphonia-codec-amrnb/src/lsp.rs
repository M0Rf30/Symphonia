// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Line spectral frequency helpers, ported from FFmpeg `libavcodec/lsp.c` (LGPL-2.1+).

use crate::celp::LP_ORDER;

/// Enforces a minimum spacing between consecutive LSFs (`ff_set_min_dist_lsf`).
pub(crate) fn set_min_dist_lsf(lsf: &mut [f32; LP_ORDER], min_spacing: f64) {
    let mut prev = 0.0f32;
    for l in lsf.iter_mut() {
        let v = f64::from(*l).max(f64::from(prev) + min_spacing) as f32;
        *l = v;
        prev = v;
    }
}

/// Converts LSFs in `[0, 0.5]` (normalised frequency) to LSPs `cos(2 pi lsf)` (`ff_acelp_lsf2lspd`).
pub(crate) fn lsf2lsp(lsp: &mut [f64; LP_ORDER], lsf: &[f32; LP_ORDER]) {
    for (o, l) in lsp.iter_mut().zip(lsf) {
        *o = (2.0 * std::f64::consts::PI * f64::from(*l)).cos();
    }
}

/// Computes the `f1`/`f2` polynomial coefficients from LSPs (`lsp2polyf`, TIA/EIA/IS-733
/// 2.4.3.3.5-1/2). Only the first 6 coefficients are needed.
fn lsp2poly(lsp: &[f64], f: &mut [f64; 6], half_order: usize) {
    f[0] = 1.0;
    f[1] = -2.0 * lsp[0];
    for i in 2..=half_order {
        let val = -2.0 * lsp[2 * (i - 1)];
        f[i] = val * f[i - 1] + 2.0 * f[i - 2];
        for j in (2..i).rev() {
            f[j] += f[j - 1] * val + f[j - 2];
        }
        f[1] += val;
    }
}

/// Converts LSPs to LPC coefficients, order 10 (`ff_acelp_lspd2lpc` with half order 5).
pub(crate) fn lsp2lpc(lsp: &[f64; LP_ORDER], lpc: &mut [f32; LP_ORDER]) {
    const HALF: usize = LP_ORDER / 2;
    let mut pa = [0.0f64; 6];
    let mut qa = [0.0f64; 6];

    lsp2poly(lsp, &mut pa, HALF);
    lsp2poly(&lsp[1..], &mut qa, HALF);

    for h in (0..HALF).rev() {
        let paf = pa[h + 1] + pa[h];
        let qaf = qa[h + 1] - qa[h];

        lpc[h] = (0.5 * (paf + qaf)) as f32;
        lpc[LP_ORDER - 1 - h] = (0.5 * (paf - qaf)) as f32;
    }
}
