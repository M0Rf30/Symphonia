// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inverse MDCT: a port of the inverse transform of liblc3 `src/mdct.c`
//! (`lc3_mdct_inverse` and its helpers).
//!
//! The C code transforms in place, aliasing its buffers. This port uses distinct scratch buffers
//! and keeps the order of the floating point operations of the reference.

use crate::tables::{self, FFT_BF3_15, FFT_BF3_45};

/// A complex number (`struct lc3_complex`).
#[derive(Copy, Clone, Default)]
pub struct Cx {
    pub re: f32,
    pub im: f32,
}

type Tw = (f32, f32);

/// Returns the butterfly 2 twiddles of the stage `i2` of a transform with `i3` radix 3 stages
/// (`lc3_fft_twiddles_bf2`).
fn fft_twiddles_bf2(i2: usize, i3: usize) -> Option<&'static [Tw]> {
    use tables::*;

    match (i2, i3) {
        (0, 0) => Some(&FFT_BF2_10),
        (0, 1) => Some(&FFT_BF2_30),
        (0, 2) => Some(&FFT_BF2_90),
        (1, 0) => Some(&FFT_BF2_20),
        (1, 1) => Some(&FFT_BF2_60),
        (1, 2) => Some(&FFT_BF2_180),
        (2, 0) => Some(&FFT_BF2_40),
        (2, 1) => Some(&FFT_BF2_120),
        (3, 0) => Some(&FFT_BF2_80),
        (3, 1) => Some(&FFT_BF2_240),
        (4, 0) => Some(&FFT_BF2_160),
        (4, 1) => Some(&FFT_BF2_480),
        _ => None,
    }
}

/// FFT 5 points (`fft_5`). `x` and `y` are of size `5 * n`.
fn fft_5(x: &[Cx], y: &mut [Cx], n: usize) {
    const COS1: f32 = 0.3090169944; // cos(-2Pi 1/5)
    const COS2: f32 = -0.8090169944; // cos(-2Pi 2/5)

    const SIN1: f32 = -0.9510565163; // sin(-2Pi 1/5)
    const SIN2: f32 = -0.5877852523; // sin(-2Pi 2/5)

    for i in 0..n {
        let x0 = x[i];
        let x1 = x[n + i];
        let x2 = x[2 * n + i];
        let x3 = x[3 * n + i];
        let x4 = x[4 * n + i];

        let s14 = Cx { re: x1.re + x4.re, im: x1.im + x4.im };
        let d14 = Cx { re: x1.re - x4.re, im: x1.im - x4.im };

        let s23 = Cx { re: x2.re + x3.re, im: x2.im + x3.im };
        let d23 = Cx { re: x2.re - x3.re, im: x2.im - x3.im };

        let y = &mut y[5 * i..5 * i + 5];

        y[0].re = x0.re + s14.re + s23.re;

        y[0].im = x0.im + s14.im + s23.im;

        y[1].re = x0.re + s14.re * COS1 - d14.im * SIN1 + s23.re * COS2 - d23.im * SIN2;

        y[1].im = x0.im + s14.im * COS1 + d14.re * SIN1 + s23.im * COS2 + d23.re * SIN2;

        y[2].re = x0.re + s14.re * COS2 - d14.im * SIN2 + s23.re * COS1 + d23.im * SIN1;

        y[2].im = x0.im + s14.im * COS2 + d14.re * SIN2 + s23.im * COS1 - d23.re * SIN1;

        y[3].re = x0.re + s14.re * COS2 + d14.im * SIN2 + s23.re * COS1 - d23.im * SIN1;

        y[3].im = x0.im + s14.im * COS2 - d14.re * SIN2 + s23.im * COS1 + d23.re * SIN1;

        y[4].re = x0.re + s14.re * COS1 + d14.im * SIN1 + s23.re * COS2 + d23.im * SIN2;

        y[4].im = x0.im + s14.im * COS1 - d14.re * SIN1 + s23.im * COS2 - d23.re * SIN2;
    }
}

/// FFT butterfly 3 points (`fft_bf3`), `n` is the number of interleaved transforms.
fn fft_bf3(twiddles: &[[Tw; 2]], x: &[Cx], y: &mut [Cx], n: usize) {
    let n3 = twiddles.len() / 3;
    let (w0, rest) = twiddles.split_at(n3);
    let (w1, w2) = rest.split_at(n3);

    for i in 0..n {
        for j in 0..n3 {
            let k = i * n3 + j;
            let x0 = x[k];
            let x1 = x[n * n3 + k];
            let x2 = x[2 * n * n3 + k];

            let o = i * 3 * n3 + j;

            for (w, o) in [(w0, o), (w1, o + n3), (w2, o + 2 * n3)] {
                let [a, b] = w[j];
                y[o].re = x0.re + x1.re * a.0 - x1.im * a.1 + x2.re * b.0 - x2.im * b.1;
                y[o].im = x0.im + x1.im * a.0 + x1.re * a.1 + x2.im * b.0 + x2.re * b.1;
            }
        }
    }
}

/// FFT butterfly 2 points (`fft_bf2`), `n` is the number of interleaved transforms.
fn fft_bf2(w: &[Tw], x: &[Cx], y: &mut [Cx], n: usize) {
    let n2 = w.len();

    for i in 0..n {
        for j in 0..n2 {
            let k = i * n2 + j;
            let x0 = x[k];
            let x1 = x[n * n2 + k];

            let o = i * 2 * n2 + j;

            y[o].re = x0.re + x1.re * w[j].0 - x1.im * w[j].1;
            y[o].im = x0.im + x1.im * w[j].0 + x1.re * w[j].1;

            y[o + n2].re = x0.re - x1.re * w[j].0 + x1.im * w[j].1;
            y[o + n2].im = x0.im - x1.im * w[j].0 - x1.re * w[j].1;
        }
    }
}

/// Performs a FFT of `n` points (`fft`), `n` being one of 10, 20, 30, 40, 60, 80, 90, 120, 160,
/// 180, 240, 480.
///
/// `x` is the input, `a` and `b` are scratch buffers of size `n`. Returns `true` if the result
/// is held by `a`, `false` if it is held by `b`; or `None` if `n` is not supported.
fn fft(x: &[Cx], mut n: usize, a: &mut [Cx], b: &mut [Cx]) -> Option<bool> {
    // The number of points `n` can be decomposed as :
    //
    //   n = 5^1 * 3^n3 * 2^n2
    //
    //   for n = 10, 20, 40, 80, 160    n3 = 0, n2 = [1..5]
    //       n = 30, 60, 120, 240, 480  n3 = 1, n2 = [1..5]
    //       n = 90, 180                n3 = 2, n2 = [1..2]

    n /= 5;
    fft_5(x, b, n);

    // The result is in `cur`, the other buffer is `other`.
    let mut cur_is_a = false;
    let (mut cur, mut other) = (b, a);

    let mut i3 = 0;
    while n & (n - 1) != 0 {
        let tw = [&FFT_BF3_15[..], &FFT_BF3_45[..]].get(i3).copied()?;
        n /= 3;
        fft_bf3(tw, cur, other, n);
        std::mem::swap(&mut cur, &mut other);
        cur_is_a = !cur_is_a;
        i3 += 1;
    }

    let mut i2 = 0;
    while n > 1 {
        let tw = fft_twiddles_bf2(i2, i3)?;
        n >>= 1;
        fft_bf2(tw, cur, other, n);
        std::mem::swap(&mut cur, &mut other);
        cur_is_a = !cur_is_a;
        i2 += 1;
    }

    Some(cur_is_a)
}

/// Pre-rotates the IMDCT coefficients, before the FFT (`imdct_pre_fft`).
///
/// The real and imaginary parts of the output are swapped, to operate on a FFT instead of an
/// IFFT.
fn imdct_pre_fft(w: &[Tw], x: &[f32], y: &mut [Cx]) {
    let n4 = w.len();

    let (mut x0, mut x1) = (0, 2 * n4);
    let (mut w0, mut w1) = (0, n4);
    let (mut y0, mut y1) = (0, n4);

    while x0 < x1 {
        let u0 = x[x0];
        x0 += 1;
        x1 -= 1;
        let u1 = x[x1];
        let v0 = x[x0];
        x0 += 1;
        x1 -= 1;
        let v1 = x[x1];

        let uw = w[w0];
        w0 += 1;
        w1 -= 1;
        let vw = w[w1];

        y[y0].re = -u0 * uw.0 - u1 * uw.1;
        y[y0].im = -u1 * uw.0 + u0 * uw.1;
        y0 += 1;

        y1 -= 1;
        y[y1].re = -v1 * vw.0 - v0 * vw.1;
        y[y1].im = -v0 * vw.0 + v1 * vw.1;
    }
}

/// Post-rotates the FFT coefficients, resulting in the IMDCT coefficients (`imdct_post_fft`).
///
/// The real and imaginary parts of the input are swapped, to operate on a FFT instead of an
/// IFFT.
fn imdct_post_fft(w: &[Tw], x: &[Cx], y: &mut [f32]) {
    let n4 = w.len();

    let (mut w0, mut w1) = (0, n4);
    let (mut x0, mut x1) = (0, n4);
    let (mut y0, mut y1) = (0, 2 * n4);

    while x0 < x1 {
        let uz = x[x0];
        x0 += 1;
        x1 -= 1;
        let vz = x[x1];
        let uw = w[w0];
        w0 += 1;
        w1 -= 1;
        let vw = w[w1];

        y[y0] = uz.re * uw.1 - uz.im * uw.0;
        y0 += 1;
        y1 -= 1;
        y[y1] = uz.re * uw.0 + uz.im * uw.1;

        y1 -= 1;
        y[y1] = vz.re * vw.1 - vz.im * vw.0;
        y[y0] = vz.re * vw.0 + vz.im * vw.1;
        y0 += 1;
    }
}

/// Applies the windowing of the samples (`imdct_window`).
///
/// `x` holds the middle half of the IMDCT coefficients (`ns` values), `d` the delayed samples
/// (`nd` values), and `y` is the output (`ns` samples). `d` is updated.
fn imdct_window(win: &[f32], ns: usize, nd: usize, x: &[f32], d: &mut [f32], y: &mut [f32]) {
    // The full MDCT coefficients is given by symmetry :
    //   T[   0 ..  n/4-1] = -half[n/4-1 .. 0    ]
    //   T[ n/4 ..  n/2-1] =  half[0     .. n/4-1]
    //   T[ n/2 .. 3n/4-1] =  half[n/4   .. n/2-1]
    //   T[3n/4 ..    n-1] =  half[n/2-1 .. n/4  ]

    let n4 = ns >> 1;

    let (mut w2, mut w0, mut w1) = (0, 3 * n4, 3 * n4);

    let mut x0 = nd - n4;
    let mut x1 = x0;
    let mut xi = 0;
    let mut y0 = nd - n4;
    let mut y1 = y0;
    let mut y2 = nd;
    let mut y3 = 0;

    while y0 > 0 {
        for _ in 0..2 {
            y0 -= 1;
            x0 -= 1;
            y[y0] = d[x0] - x[xi] * win[w1];
            w1 += 1;

            w0 -= 1;
            y[y1] = d[x1] + x[xi] * win[w0];
            y1 += 1;
            x1 += 1;
            xi += 1;
        }
    }

    while y1 < nd {
        for _ in 0..2 {
            w0 -= 1;
            y[y1] = d[x1] + x[xi] * win[w0];
            y1 += 1;
            x1 += 1;
            xi += 1;
        }
    }

    while y1 < 2 * n4 {
        for _ in 0..2 {
            w0 -= 1;
            y[y1] = x[xi] * win[w0];
            y1 += 1;
            y2 -= 1;
            d[y2] = x[xi] * win[w2];
            w2 += 1;
            xi += 1;
        }
    }

    while y2 > y3 {
        for _ in 0..2 {
            w0 -= 1;
            d[y3] = x[xi] * win[w0];
            y3 += 1;
            y2 -= 1;
            d[y2] = x[xi] * win[w2];
            w2 += 1;
            xi += 1;
        }
    }
}

/// Scratch buffers of the inverse MDCT.
pub struct MdctScratch {
    za: Vec<Cx>,
    zb: Vec<Cx>,
    zc: Vec<Cx>,
    u: Vec<f32>,
}

impl MdctScratch {
    /// Allocates the scratch buffers for frames of up to `ns` samples.
    pub fn new(ns: usize) -> Self {
        MdctScratch {
            za: vec![Cx::default(); ns / 2],
            zb: vec![Cx::default(); ns / 2],
            zc: vec![Cx::default(); ns / 2],
            u: vec![0.0; ns],
        }
    }
}

/// Inverse MDCT (`lc3_mdct_inverse`, for the same PCM and coded sample rate).
///
/// `x` are the `ns` MDCT coefficients, `d` the `nd` delayed samples, and `y` receives the `ns`
/// output samples. Returns `false` if the frame duration and sample rate are not supported.
pub fn inverse(
    dt: usize,
    sr: usize,
    x: &[f32],
    d: &mut [f32],
    y: &mut [f32],
    s: &mut MdctScratch,
) -> bool {
    let (Some(rot), Some(win)) = (tables::mdct_rot(dt, sr), tables::mdct_win(dt, sr))
    else {
        return false;
    };

    let ns = 2 * rot.len();
    let nd = win.len() - ns;
    let n = ns / 2;

    imdct_pre_fft(rot, x, &mut s.za[..n]);

    let Some(in_a) = fft(&s.za[..n], n, &mut s.zb[..n], &mut s.zc[..n])
    else {
        return false;
    };
    let z = if in_a { &s.zb[..n] } else { &s.zc[..n] };

    imdct_post_fft(rot, z, &mut s.u[..ns]);

    imdct_window(win, ns, nd, &s.u[..ns], d, y);

    true
}
