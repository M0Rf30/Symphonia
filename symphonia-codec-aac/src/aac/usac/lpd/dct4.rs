// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The type IV discrete cosine transform of the LPD coder (`acelp_mdct` of libxaac).
//!
//! `X[k] = sum_m x[m] cos(pi / N (m + 1/2) (k + 1/2))`, without normalisation, computed by a
//! complex transform of `N / 2` points with the pre and post rotations of libxaac
//! (`ixheaacd_acelp_mdct.c`).

use core::f64::consts::PI;

/// A transform of one length.
pub struct Dct4 {
    len: usize,
    pre: Vec<(f64, f64)>,
    post: Vec<(f64, f64)>,
    /// Twiddles of the radix-2 transform or of the direct transform.
    twiddle: Vec<(f64, f64)>,
    re: Vec<f64>,
    im: Vec<f64>,
    tmp_re: Vec<f64>,
    tmp_im: Vec<f64>,
}

impl Dct4 {
    /// `len` is even.
    pub fn new(len: usize) -> Self {
        let n = len / 2;

        let pre = (0..n)
            .map(|i| {
                let theta = (4 * i + 1) as f64 * PI / (8 * n) as f64;
                (theta.cos(), -theta.sin())
            })
            .collect();

        let post = (0..n)
            .map(|i| {
                let phi = i as f64 * PI / (2 * n) as f64;
                (phi.cos(), -phi.sin())
            })
            .collect();

        let twiddle = (0..n)
            .map(|k| {
                let a = -2.0 * PI * k as f64 / n as f64;
                (a.cos(), a.sin())
            })
            .collect();

        Dct4 {
            len,
            pre,
            post,
            twiddle,
            re: vec![0.0; n],
            im: vec![0.0; n],
            tmp_re: vec![0.0; n],
            tmp_im: vec![0.0; n],
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Transform `input` (`len` samples) into `output`.
    pub fn transform(&mut self, input: &[f32], output: &mut [f32]) {
        let n = self.len / 2;

        for i in 0..n {
            let a = f64::from(input[2 * i]);
            let b = f64::from(input[self.len - 1 - 2 * i]);
            let (c, s) = self.pre[i];
            self.re[i] = a * c - b * s;
            self.im[i] = a * s + b * c;
        }

        self.fft();

        for i in 0..n {
            let (c, s) = self.post[i];
            let (r, m) = (self.tmp_re[i], self.tmp_im[i]);
            output[2 * i] = (r * c - m * s) as f32;
            output[self.len - 1 - 2 * i] = (-(r * s + m * c)) as f32;
        }
    }

    /// The forward discrete Fourier transform of `re`/`im` into `tmp_re`/`tmp_im`.
    fn fft(&mut self) {
        let n = self.re.len();

        if n.is_power_of_two() {
            let bits = n.trailing_zeros();

            for i in 0..n {
                let j = if bits == 0 { 0 } else { i.reverse_bits() >> (usize::BITS - bits) };
                self.tmp_re[j] = self.re[i];
                self.tmp_im[j] = self.im[i];
            }

            let mut size = 2;

            while size <= n {
                let step = n / size;

                for start in (0..n).step_by(size) {
                    for k in 0..size / 2 {
                        let (wr, wi) = self.twiddle[k * step];
                        let (i0, i1) = (start + k, start + k + size / 2);
                        let tr = self.tmp_re[i1] * wr - self.tmp_im[i1] * wi;
                        let ti = self.tmp_re[i1] * wi + self.tmp_im[i1] * wr;
                        self.tmp_re[i1] = self.tmp_re[i0] - tr;
                        self.tmp_im[i1] = self.tmp_im[i0] - ti;
                        self.tmp_re[i0] += tr;
                        self.tmp_im[i0] += ti;
                    }
                }

                size *= 2;
            }
        }
        else {
            for k in 0..n {
                let (mut sr, mut si) = (0.0, 0.0);

                for m in 0..n {
                    let (wr, wi) = self.twiddle[(k * m) % n];
                    sr += self.re[m] * wr - self.im[m] * wi;
                    si += self.re[m] * wi + self.im[m] * wr;
                }

                self.tmp_re[k] = sr;
                self.tmp_im[k] = si;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(x: &[f32]) -> Vec<f32> {
        let n = x.len();
        (0..n)
            .map(|k| {
                (0..n)
                    .map(|m| {
                        f64::from(x[m])
                            * (PI / n as f64 * (m as f64 + 0.5) * (k as f64 + 0.5)).cos()
                    })
                    .sum::<f64>() as f32
            })
            .collect()
    }

    /// The transform is the type IV DCT, for the power of two and the multiple of three lengths.
    #[test]
    fn matches_the_definition() {
        for len in [16usize, 48, 96, 128, 192] {
            let x: Vec<f32> = (0..len).map(|i| ((i * 37 + 11) % 23) as f32 - 11.0).collect();
            let mut out = vec![0.0; len];
            Dct4::new(len).transform(&x, &mut out);
            let want = reference(&x);

            for (a, b) in out.iter().zip(&want) {
                assert!((a - b).abs() < 1e-3 * (1.0 + b.abs()), "{len}: {a} vs {b}");
            }
        }
    }
}
