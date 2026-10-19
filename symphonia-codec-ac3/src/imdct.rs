// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The AC-3 inverse transform: a 512/256-point IMDCT built on a radix-2 complex FFT, the
//! Kaiser-Bessel-derived window, and the windowed overlap-add.

use std::f64::consts::PI;
use std::sync::LazyLock;

use symphonia_core::dsp::window::{WindowType, generate_window};

/// Number of PCM samples produced per channel per audio block.
pub const BLOCK_SIZE: usize = 256;

/// A half-IMDCT of `m` coefficients producing `m` output values (the middle half of the full
/// `2m`-point IMDCT, whose outer quarters follow by symmetry).
///
/// The transform is evaluated with an `m/2`-point complex FFT.
struct Imdct {
    m: usize,
    /// Pre/post-twiddle `-exp(j*2*pi*(8k+1)/(8*2m))`, `k < m/2`.
    twiddle: Vec<(f32, f32)>,
    /// FFT twiddles `exp(+j*2*pi*k/(m/2))`, `k < m/4`.
    fft_twiddle: Vec<(f32, f32)>,
    /// Bit-reversal permutation for `m/2` points.
    bitrev: Vec<u8>,
}

impl Imdct {
    fn new(m: usize) -> Self {
        let n2 = m / 2;
        let n = 2 * m;

        let twiddle = (0..n2)
            .map(|k| {
                let a = 2.0 * PI * (8 * k + 1) as f64 / (8 * n) as f64;
                (-(a.cos()) as f32, -(a.sin()) as f32)
            })
            .collect();

        let fft_twiddle = (0..n2 / 2)
            .map(|k| {
                let a = 2.0 * PI * k as f64 / n2 as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .collect();

        let bits = n2.trailing_zeros();
        let bitrev = (0..n2).map(|i| ((i as u32).reverse_bits() >> (32 - bits)) as u8).collect();

        Imdct { m, twiddle, fft_twiddle, bitrev }
    }

    /// Computes the half-IMDCT of `x[..m]` into `out[..m]`.
    fn transform(&self, x: &[f32], out: &mut [f32]) {
        let m = self.m;
        let n2 = m / 2;

        let mut z = [(0.0f32, 0.0f32); BLOCK_SIZE / 2];

        // Pre-twiddle and bit-reversed load.
        for k in 0..n2 {
            let (re, im) = (x[m - 2 * k - 1], x[2 * k]);
            let (c, s) = self.twiddle[k];
            z[usize::from(self.bitrev[k])] = (re * c - im * s, re * s + im * c);
        }

        // In-place radix-2 FFT with positive exponent.
        let stride_base = n2;
        let mut len = 2;
        while len <= n2 {
            let half = len / 2;
            let stride = stride_base / len;
            for start in (0..n2).step_by(len) {
                for j in 0..half {
                    let (wr, wi) = self.fft_twiddle[j * stride];
                    let (ur, ui) = z[start + j];
                    let (xr, xi) = z[start + j + half];
                    let (vr, vi) = (xr * wr - xi * wi, xr * wi + xi * wr);
                    z[start + j] = (ur + vr, ui + vi);
                    z[start + j + half] = (ur - vr, ui - vi);
                }
            }
            len *= 2;
        }

        // Post-twiddle and de-interleave.
        for t in 0..n2 {
            let (zr, zi) = z[t];
            let (c, s) = self.twiddle[t];
            let (yr, yi) = (zr * c - zi * s, zr * s + zi * c);
            out[2 * t] = -yr;
            out[m - 1 - 2 * t] = yi;
        }
    }
}

pub struct Transforms {
    long: Imdct,
    short: Imdct,
    /// The 256-entry rising half of the 512-sample KBD window (alpha = 5).
    window: [f32; BLOCK_SIZE],
}

static TRANSFORMS: LazyLock<Transforms> = LazyLock::new(|| {
    let mut window = [0.0f32; BLOCK_SIZE];
    generate_window(WindowType::KaiserBessel(5.0), 1.0, BLOCK_SIZE, true, &mut window);
    Transforms { long: Imdct::new(BLOCK_SIZE), short: Imdct::new(BLOCK_SIZE / 2), window }
});

pub fn transforms() -> &'static Transforms {
    &TRANSFORMS
}

impl Transforms {
    /// Inverse transforms one block of `256` coefficients, windows it, and overlap-adds it with
    /// `delay`, producing `256` PCM samples in `out`. `delay` is updated for the next block.
    pub fn block(
        &self,
        coeffs: &[f32; BLOCK_SIZE],
        short_blocks: bool,
        delay: &mut [f32; BLOCK_SIZE / 2],
        out: &mut [f32],
    ) {
        const HALF: usize = BLOCK_SIZE / 2;

        let mut cur = [0.0f32; HALF];

        if short_blocks {
            let mut x = [0.0f32; HALF];
            let mut tmp = [0.0f32; HALF];

            for (d, c) in x.iter_mut().zip(coeffs.iter().step_by(2)) {
                *d = *c;
            }
            self.short.transform(&x, &mut tmp);
            cur.copy_from_slice(&tmp);

            // The second short transform provides the next delay, after the overlap is computed.
            for (d, c) in x.iter_mut().zip(coeffs.iter().skip(1).step_by(2)) {
                *d = *c;
            }
            self.overlap(delay, &cur, out);
            self.short.transform(&x, delay);
        }
        else {
            let mut tmp = [0.0f32; BLOCK_SIZE];
            self.long.transform(coeffs, &mut tmp);
            cur.copy_from_slice(&tmp[..HALF]);
            self.overlap(delay, &cur, out);
            delay.copy_from_slice(&tmp[HALF..]);
        }
    }

    fn overlap(&self, delay: &[f32; BLOCK_SIZE / 2], cur: &[f32; BLOCK_SIZE / 2], out: &mut [f32]) {
        const HALF: usize = BLOCK_SIZE / 2;
        let w = &self.window;
        for k in 0..HALF {
            let (s0, s1) = (delay[k], cur[HALF - 1 - k]);
            out[k] = s0 * w[BLOCK_SIZE - 1 - k] - s1 * w[k];
            out[BLOCK_SIZE - 1 - k] = s0 * w[k] + s1 * w[BLOCK_SIZE - 1 - k];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(len: usize, mut seed: u32) -> Vec<f32> {
        (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect()
    }

    #[test]
    fn verify_imdct_matches_definition() {
        let t = transforms();
        for (imdct, m) in [(&t.long, 256), (&t.short, 128)] {
            let x = pseudo_random(m, 7 + m as u32);
            let mut out = vec![0.0; m];
            imdct.transform(&x, &mut out);

            // y[n] = sum_k x[k] cos(pi/m (n + 1/2 + m/2)(k + 1/2)), half[i] = -y[m/2 + i].
            for (i, &got) in out.iter().enumerate() {
                let n = (m / 2 + i) as f64 + 0.5 + m as f64 / 2.0;
                let want: f64 = -x
                    .iter()
                    .enumerate()
                    .map(|(k, &v)| f64::from(v) * (PI / m as f64 * n * (k as f64 + 0.5)).cos())
                    .sum::<f64>();
                assert!((f64::from(got) - want).abs() < 1e-4, "m={m} i={i} {got} vs {want}");
            }
        }
    }

    #[test]
    fn verify_window_is_power_complementary() {
        let w = &transforms().window;
        for k in 0..BLOCK_SIZE {
            let s = w[k] * w[k] + w[BLOCK_SIZE - 1 - k] * w[BLOCK_SIZE - 1 - k];
            assert!((s - 1.0).abs() < 1e-5);
        }
    }
}
