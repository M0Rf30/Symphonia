// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The inverse "reversed MLT" of Siren / G.722.1.
//!
//! `symphonia_core::dsp::mdct` only supports power-of-2 sizes, but Siren's frames are 320 (Siren 7)
//! or 640 (Siren 14) samples long. This module therefore evaluates the transform directly, using
//! the definition of FFmpeg's reference (`ff_tx_mdct_naive_inv` in `libavutil/tx_template.c`,
//! which is what `libavcodec/siren.c` calls), with a cosine table to avoid evaluating `cos` at run
//! time. At 50 frames per second the cost is small.

use std::f64::consts::PI;

/// An inverse MDCT of `m` coefficients producing `m` output samples (the middle half of the full
/// `2 * m` sample IMDCT, which is all the overlap-add needs).
pub(crate) struct Imdct {
    m: usize,
    scale: f32,
    /// `cos(2 * pi * n / (8 * m))` for `n` in `0..8 * m`.
    cos: Box<[f32]>,
}

impl Imdct {
    /// Create an `m`-coefficient IMDCT whose output is multiplied by `scale`.
    pub(crate) fn new(m: usize, scale: f32) -> Self {
        let n = 8 * m;
        let cos = (0..n).map(|i| (2.0 * PI * i as f64 / n as f64).cos() as f32).collect();
        Imdct { m, scale, cos }
    }

    /// Computes the transform of the first `valid` of the `m` coefficients in `coefs`, treating
    /// the others as zero.
    ///
    /// With `h = m / 2` and `x` the coefficients:
    ///
    /// ```text
    /// out[i]     =  scale * sum_j x[j] * cos(pi * (2j + 1) * (2m - 2i - 1) / (4m))
    /// out[i + h] = -scale * sum_j x[j] * cos(pi * (2j + 1) * (3m + 2i + 1) / (4m))
    /// ```
    pub(crate) fn imdct(&self, coefs: &[f32], valid: usize, out: &mut [f32]) {
        let m = self.m;
        let h = m / 2;
        let n = 8 * m;

        assert!(coefs.len() == m && out.len() == m && valid <= m);

        for i in 0..h {
            // The phase of coefficient `j` is `(2j + 1) * a`; it advances by `2 * a` per `j`.
            let a_down = 2 * m - 2 * i - 1;
            let a_up = 3 * m + 2 * i + 1;

            let (mut p_down, mut p_up) = (a_down % n, a_up % n);
            let (s_down, s_up) = ((2 * a_down) % n, (2 * a_up) % n);

            let mut sum_down = 0.0f32;
            let mut sum_up = 0.0f32;

            for &x in &coefs[..valid] {
                sum_down += x * self.cos[p_down];
                sum_up += x * self.cos[p_up];

                p_down += s_down;
                if p_down >= n {
                    p_down -= n;
                }
                p_up += s_up;
                if p_up >= n {
                    p_up -= n;
                }
            }

            out[i] = sum_down * self.scale;
            out[i + h] = -sum_up * self.scale;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table-driven transform must match the textbook definition.
    #[test]
    fn verify_imdct_against_definition() {
        let m = 320;
        let imdct = Imdct::new(m, 0.5);

        let mut state = 0x2545_f491_4f6c_dd1du64;
        let coefs: Vec<f32> = (0..m)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
            })
            .collect();

        let mut out = vec![0.0f32; m];
        imdct.imdct(&coefs, 280, &mut out);

        let h = m / 2;
        for i in 0..h {
            let (mut down, mut up) = (0.0f64, 0.0f64);
            for (j, &x) in coefs[..280].iter().enumerate() {
                let a = (2 * j + 1) as f64;
                let phase = PI / (4.0 * m as f64);
                down += x as f64 * (a * (2 * m - 2 * i - 1) as f64 * phase).cos();
                up += x as f64 * (a * (3 * m + 2 * i + 1) as f64 * phase).cos();
            }
            assert!((out[i] as f64 - down * 0.5).abs() < 1e-3, "down {i}");
            assert!((out[i + h] as f64 + up * 0.5).abs() < 1e-3, "up {i}");
        }
    }
}
