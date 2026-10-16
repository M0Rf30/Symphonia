// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The inverse MDCT used by the AC-4 decoder.
//!
//! The reference decoder (FFmpeg's `av_tx`, see `NOTICE`) evaluates the length-`n` inverse MDCT
//! `out[m] = 1/n · Σ_k spec[k]·cos(π/n·(m + 1/2)(k + 1/2))` and the caller unfolds those `n`
//! samples into the full 2·n IMDCT output using the inherent antisymmetry. This module computes
//! that half-IMDCT directly.

use std::collections::HashMap;

/// A precomputed length-`n` inverse MDCT kernel.
pub struct Imdct {
    n: usize,
    /// `cos[m*n + k] = cos(π/n·(m + 1/2)(k + 1/2))`.
    cos: Box<[f32]>,
}

impl Imdct {
    pub fn new(n: usize) -> Self {
        let mut cos = vec![0.0f32; n * n];
        for m in 0..n {
            for k in 0..n {
                let th = std::f64::consts::PI * (m as f64 + 0.5) * (k as f64 + 0.5) / (n as f64);
                cos[m * n + k] = th.cos() as f32;
            }
        }
        Imdct { n, cos: cos.into_boxed_slice() }
    }

    /// Performs the inverse MDCT. `spec` has length `n`; `out` has length `n`.
    pub fn imdct(&self, spec: &[f32], out: &mut [f32]) {
        let n = self.n;
        debug_assert_eq!(spec.len(), n);
        debug_assert_eq!(out.len(), n);

        for (m, chunk) in self.cos.chunks_exact(n).enumerate() {
            let mut acc = 0.0f32;
            for (k, &c) in chunk.iter().enumerate() {
                acc += spec[k] * c;
            }
            out[m] = acc / n as f32;
        }
    }
}

/// A cache of IMDCT transforms keyed by size.
#[derive(Default)]
pub struct ImdctSet {
    cache: HashMap<usize, Imdct>,
}

impl ImdctSet {
    pub fn get(&mut self, n: usize) -> &mut Imdct {
        self.cache.entry(n).or_insert_with(|| Imdct::new(n))
    }
}

#[cfg(test)]
mod tests {
    use super::Imdct;

    #[test]
    fn impulse_matches_confirmed_formula() {
        // Confirmed against FFmpeg `av_tx`: an impulse at `X[k]` yields
        // `out[m] = cos(pi/N (m + 0.5)(k + 0.5)) / N`.
        let n = 8;
        let imdct = Imdct::new(n);
        let mut spec = vec![0.0f32; n];
        spec[3] = 1.0;
        let mut out = vec![0.0f32; n];
        imdct.imdct(&spec, &mut out);
        for m in 0..n {
            let expect =
                ((std::f64::consts::PI * (m as f64 + 0.5) * 3.5 / n as f64).cos() / n as f64)
                    as f32;
            assert!((out[m] - expect).abs() < 1e-6, "m={m}: {} != {}", out[m], expect);
        }
    }
}
