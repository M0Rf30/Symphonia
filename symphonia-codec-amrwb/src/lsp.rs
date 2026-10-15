// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Line spectral frequency / pair helpers (port of the floating-point routines of FFmpeg's
//! `libavcodec/lsp.c`; 3GPP TS 26.190 clause 5.2).

use core::f64::consts::PI;

/// Largest supported LP filter order divided by two.
const MAX_LP_HALF_ORDER: usize = 10;

/// Enforces a minimum distance between consecutive frequencies (`ff_set_min_dist_lsf`).
pub fn set_min_dist_lsf(lsf: &mut [f32], min_spacing: f64) {
    let mut prev = 0.0f32;
    for v in lsf.iter_mut() {
        prev = (f64::from(*v)).max(f64::from(prev) + min_spacing) as f32;
        *v = prev;
    }
}

/// Converts normalized frequencies to the cosine domain: `lsp[i] = cos(2 pi lsf[i])`
/// (`ff_acelp_lsf2lspd`).
pub fn lsf2lspd(lsp: &mut [f64], lsf: &[f32]) {
    for (p, &f) in lsp.iter_mut().zip(lsf) {
        *p = (2.0 * PI * f64::from(f)).cos();
    }
}

/// Computes the Pa / (1 + z^-1) or Qa / (1 - z^-1) polynomial coefficients from the cosine domain
/// line spectral pairs (TIA/EIA/IS-733 2.4.3.3.5-1/2). Only `half + 1` values are produced.
fn lsp2polyf(lsp: &[f64], f: &mut [f64], half: usize) {
    f[0] = 1.0;
    f[1] = -2.0 * lsp[0];
    for i in 2..=half {
        let val = -2.0 * lsp[2 * i - 2];
        f[i] = val * f[i - 1] + 2.0 * f[i - 2];
        for j in (2..i).rev() {
            f[j] += f[j - 1] * val + f[j - 2];
        }
        f[1] += val;
    }
}

/// Converts line spectral pairs to linear prediction coefficients (`ff_amrwb_lsp2lpc`,
/// 3GPP TS 26.190 clause 5.2.4). `lp` receives `order` coefficients.
pub fn amrwb_lsp2lpc(lsp: &[f64], lp: &mut [f32], order: usize) {
    let half = order >> 1;
    // `qa[i]` is stored at `buf[i + 1]` so that `qa[-1]` (zero) is addressable.
    let mut buf = [0.0f64; MAX_LP_HALF_ORDER + 1];
    let mut pa = [0.0f64; MAX_LP_HALF_ORDER + 1];

    lsp2polyf(lsp, &mut pa, half);
    lsp2polyf(&lsp[1..], &mut buf[1..], half - 1);

    let last = lsp[order - 1];
    let mut j = order - 1;
    for i in 1..half {
        let paf = pa[i] * (1.0 + last);
        let qaf = (buf[i + 1] - buf[i - 1]) * (1.0 - last);
        lp[i - 1] = ((paf + qaf) * 0.5) as f32;
        lp[j - 1] = ((paf - qaf) * 0.5) as f32;
        j -= 1;
    }

    lp[half - 1] = ((1.0 + last) * pa[half] * 0.5) as f32;
    lp[order - 1] = last as f32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With all line spectral pairs at pi/2 apart the polynomial is `1 + z^-order`-like and the
    /// conversion must be finite; this guards the index arithmetic for both filter orders.
    #[test]
    fn verify_lsp2lpc_orders() {
        for order in [16usize, 20] {
            let lsf: Vec<f32> =
                (0..order).map(|i| (i as f32 + 1.0) / (2.0 * order as f32)).collect();
            let mut lsp = vec![0.0f64; order];
            lsf2lspd(&mut lsp, &lsf);
            let mut lp = vec![0.0f32; order];
            amrwb_lsp2lpc(&lsp, &mut lp, order);
            assert!(lp.iter().all(|v| v.is_finite()));
            assert_eq!(lp[order - 1], lsp[order - 1] as f32);
        }
    }

    #[test]
    fn verify_min_dist() {
        let mut v = [0.0f32, 0.0, 0.5, 0.5];
        set_min_dist_lsf(&mut v, 0.25);
        assert_eq!(v, [0.25, 0.5, 0.75, 1.0]);
    }
}
