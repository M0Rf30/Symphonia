// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The inverse MDCT used by the AC-4 decoder.
//!
//! The reference decoder (FFmpeg's `av_tx`, `AV_TX_FLOAT_MDCT`, inverse, scale `1/n`) turns `n`
//! spectral coefficients into `n` time samples
//! `out[m] = 1/n · Σ_k spec[k]·cos(π/n·(n - 1 - m + 1/2)(k + 1/2))`, i.e. the time-reversed
//! DCT-IV, and the caller (`spectral_synthesis`) unfolds those `n` samples into the full 2·n
//! IMDCT output using the inherent antisymmetry. This module computes that half-IMDCT.
//!
//! The DCT-IV of length `n` is evaluated through a complex FFT of length `n/2` (pre- and
//! post-twiddle), and that FFT is a mixed radix 2/3/5 Stockham autosort FFT. All twiddles and
//! scratch buffers are allocated when the transform is built.

use symphonia_core::errors::{Result, decode_error};

use crate::ctx::transf_length_48khz;

#[derive(Clone, Copy, Default)]
struct Cpx {
    re: f32,
    im: f32,
}

impl Cpx {
    #[inline(always)]
    fn add(self, o: Cpx) -> Cpx {
        Cpx { re: self.re + o.re, im: self.im + o.im }
    }

    #[inline(always)]
    fn sub(self, o: Cpx) -> Cpx {
        Cpx { re: self.re - o.re, im: self.im - o.im }
    }

    #[inline(always)]
    fn scale(self, k: f32) -> Cpx {
        Cpx { re: self.re * k, im: self.im * k }
    }

    #[inline(always)]
    fn mul(self, o: Cpx) -> Cpx {
        Cpx { re: self.re * o.re - self.im * o.im, im: self.re * o.im + self.im * o.re }
    }

    /// Multiplies by `-i`.
    #[inline(always)]
    fn mul_neg_i(self) -> Cpx {
        Cpx { re: self.im, im: -self.re }
    }

    /// Multiplies by `+i`.
    #[inline(always)]
    fn mul_i(self) -> Cpx {
        Cpx { re: -self.im, im: self.re }
    }
}

/// One Stockham pass: `radix` points combined, `m` butterflies per column, `s` columns.
struct Stage {
    radix: usize,
    m: usize,
    s: usize,
    /// `tw[p * (radix - 1) + (k - 1)] = exp(-2πi·p·k / (m·radix))`.
    tw: Box<[Cpx]>,
}

impl Stage {
    fn new(radix: usize, m: usize, s: usize) -> Self {
        let len = (m * radix) as f64;
        let mut tw = Vec::with_capacity(m * (radix - 1));
        for p in 0..m {
            for k in 1..radix {
                let th = -2.0 * std::f64::consts::PI * (p * k) as f64 / len;
                tw.push(Cpx { re: th.cos() as f32, im: th.sin() as f32 });
            }
        }
        Stage { radix, m, s, tw: tw.into_boxed_slice() }
    }

    fn run(&self, x: &[Cpx], y: &mut [Cpx]) {
        let (r, m, s) = (self.radix, self.m, self.s);
        let c3 = (3.0f64.sqrt() / 2.0) as f32;
        let c1 = (2.0 * std::f64::consts::PI / 5.0).cos() as f32;
        let c2 = (4.0 * std::f64::consts::PI / 5.0).cos() as f32;
        let s1 = (2.0 * std::f64::consts::PI / 5.0).sin() as f32;
        let s2 = (4.0 * std::f64::consts::PI / 5.0).sin() as f32;

        for p in 0..m {
            let tw = &self.tw[p * (r - 1)..(p + 1) * (r - 1)];
            for q in 0..s {
                let at = |j: usize| x[q + s * (p + j * m)];
                let mut out = [Cpx::default(); 5];

                match r {
                    2 => {
                        let (a0, a1) = (at(0), at(1));
                        out[0] = a0.add(a1);
                        out[1] = a0.sub(a1);
                    }
                    3 => {
                        let (a0, a1, a2) = (at(0), at(1), at(2));
                        let t1 = a1.add(a2);
                        let t2 = a1.sub(a2).scale(c3);
                        let u = a0.sub(t1.scale(0.5));
                        out[0] = a0.add(t1);
                        out[1] = u.add(t2.mul_neg_i());
                        out[2] = u.add(t2.mul_i());
                    }
                    _ => {
                        let (a0, a1, a2, a3, a4) = (at(0), at(1), at(2), at(3), at(4));
                        let t1 = a1.add(a4);
                        let t2 = a2.add(a3);
                        let t3 = a1.sub(a4);
                        let t4 = a2.sub(a3);
                        let u1 = a0.add(t1.scale(c1)).add(t2.scale(c2));
                        let u2 = a0.add(t1.scale(c2)).add(t2.scale(c1));
                        let v1 = t3.scale(s1).add(t4.scale(s2));
                        let v2 = t3.scale(s2).sub(t4.scale(s1));
                        out[0] = a0.add(t1).add(t2);
                        out[1] = u1.add(v1.mul_neg_i());
                        out[4] = u1.add(v1.mul_i());
                        out[2] = u2.add(v2.mul_neg_i());
                        out[3] = u2.add(v2.mul_i());
                    }
                }

                y[q + s * (r * p)] = out[0];
                for k in 1..r {
                    y[q + s * (r * p + k)] = out[k].mul(tw[k - 1]);
                }
            }
        }
    }
}

/// A precomputed length-`n` inverse MDCT (`n` coefficients in, `n` samples out).
pub struct Imdct {
    n: usize,
    scale: f32,
    /// `exp(-iπ(8j + 1) / (8n))` for `j < n/2`.
    tw: Box<[Cpx]>,
    stages: Box<[Stage]>,
    a: Box<[Cpx]>,
    b: Box<[Cpx]>,
}

impl Imdct {
    /// Builds a transform of even length `n` whose half length has only the prime factors 2, 3
    /// and 5.
    pub fn new(n: usize) -> Option<Self> {
        if n < 2 || n % 2 != 0 {
            return None;
        }

        let n2 = n / 2;

        // Factorise the FFT length, largest radix first.
        let mut rest = n2;
        let mut radices = Vec::new();
        for r in [5usize, 3, 2] {
            while rest % r == 0 {
                radices.push(r);
                rest /= r;
            }
        }
        if rest != 1 {
            return None;
        }

        let mut stages = Vec::with_capacity(radices.len());
        let (mut len, mut s) = (n2, 1);
        for r in radices {
            len /= r;
            stages.push(Stage::new(r, len, s));
            s *= r;
        }

        let tw = (0..n2)
            .map(|j| {
                let th = -std::f64::consts::PI * (8 * j + 1) as f64 / (8 * n) as f64;
                Cpx { re: th.cos() as f32, im: th.sin() as f32 }
            })
            .collect();

        Some(Imdct {
            n,
            scale: 1.0 / n as f32,
            tw,
            stages: stages.into_boxed_slice(),
            a: vec![Cpx::default(); n2].into_boxed_slice(),
            b: vec![Cpx::default(); n2].into_boxed_slice(),
        })
    }

    /// Performs the inverse MDCT. `spec` and `out` must both have length `n`.
    pub fn imdct(&mut self, spec: &[f32], out: &mut [f32]) {
        let n = self.n;
        let n2 = n / 2;

        if spec.len() < n || out.len() < n {
            return;
        }

        // DCT-IV through a length n/2 complex FFT.
        for j in 0..n2 {
            let t = Cpx { re: spec[2 * j], im: spec[n - 1 - 2 * j] };
            self.a[j] = t.mul(self.tw[j]);
        }

        let mut in_a = true;
        for stage in self.stages.iter() {
            if in_a {
                stage.run(&self.a, &mut self.b);
            }
            else {
                stage.run(&self.b, &mut self.a);
            }
            in_a = !in_a;
        }
        let res = if in_a { &self.a } else { &self.b };

        // y[2k] = Re z, y[n-1-2k] = -Im z, and av_tx emits the time-reversed DCT-IV.
        for k in 0..n2 {
            let z = res[k].mul(self.tw[k]);
            out[n - 1 - 2 * k] = z.re * self.scale;
            out[2 * k] = -z.im * self.scale;
        }
    }
}

/// All inverse MDCTs needed by the AC-4 frame lengths.
pub struct ImdctSet {
    /// Transforms sorted by size.
    xforms: Vec<Imdct>,
}

impl Default for ImdctSet {
    fn default() -> Self {
        Self::new()
    }
}

impl ImdctSet {
    /// Builds one transform for every distinct size in `transf_length_48khz`.
    pub fn new() -> Self {
        let mut sizes: Vec<usize> = (0..8)
            .flat_map(|idx| transf_length_48khz(idx).iter().map(|&n| usize::from(n)))
            .collect();
        sizes.sort_unstable();
        sizes.dedup();

        ImdctSet { xforms: sizes.into_iter().filter_map(Imdct::new).collect() }
    }

    /// Performs the length-`n` inverse MDCT of `spec[..n]` into `out[..n]`.
    pub fn imdct(&mut self, n: usize, spec: &[f32], out: &mut [f32]) -> Result<()> {
        match self.xforms.binary_search_by_key(&n, |x| x.n) {
            Ok(i) if spec.len() >= n && out.len() >= n => {
                self.xforms[i].imdct(spec, out);
                Ok(())
            }
            _ => decode_error("ac4: unsupported imdct size"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Imdct, ImdctSet};
    use crate::ctx::transf_length_48khz;

    /// O(n^2) reference: `out[m] = 1/n · Σ_k spec[k]·cos(π/n·(n - 1 - m + 1/2)(k + 1/2))`.
    fn imdct_ref(spec: &[f32], out: &mut [f32]) {
        let n = spec.len();
        for m in 0..n {
            let mut acc = 0.0f64;
            for k in 0..n {
                let th =
                    std::f64::consts::PI * ((n - 1 - m) as f64 + 0.5) * (k as f64 + 0.5) / n as f64;
                acc += f64::from(spec[k]) * th.cos();
            }
            out[m] = (acc / n as f64) as f32;
        }
    }

    fn lcg(state: &mut u32) -> f32 {
        *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        (*state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    }

    #[test]
    fn impulse_matches_av_tx() {
        // Confirmed against FFmpeg `av_tx` (n = 16, impulse at X[0]): out[0] = 0.003066730,
        // out[15] = 0.062424719.
        let mut x = Imdct::new(16).unwrap();
        let mut spec = vec![0.0f32; 16];
        spec[0] = 1.0;
        let mut out = vec![0.0f32; 16];
        x.imdct(&spec, &mut out);
        assert!((out[0] - 0.003066730).abs() < 1e-7);
        assert!((out[15] - 0.062424719).abs() < 1e-7);
    }

    #[test]
    fn fast_matches_reference_for_all_sizes() {
        let mut set = ImdctSet::new();
        let mut seed = 12345u32;
        let mut sizes: Vec<usize> = (0..8)
            .flat_map(|idx| transf_length_48khz(idx).iter().map(|&n| usize::from(n)))
            .collect();
        sizes.sort_unstable();
        sizes.dedup();
        assert_eq!(
            sizes,
            [96, 120, 128, 192, 240, 256, 384, 480, 512, 768, 960, 1024, 1536, 1920, 2048]
        );

        for n in sizes {
            let spec: Vec<f32> = (0..n).map(|_| lcg(&mut seed)).collect();
            let mut want = vec![0.0f32; n];
            let mut got = vec![0.0f32; n];
            imdct_ref(&spec, &mut want);
            set.imdct(n, &spec, &mut got).unwrap();
            let peak = want.iter().fold(0.0f32, |a, &v| a.max(v.abs()));
            for m in 0..n {
                assert!(
                    (want[m] - got[m]).abs() <= 1e-4 * peak,
                    "n={n} m={m}: {} != {}",
                    got[m],
                    want[m]
                );
            }
        }
    }
}
