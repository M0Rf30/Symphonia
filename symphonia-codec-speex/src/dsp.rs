// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Signal processing primitives of the Speex decoder.
//!
//! Ported from the floating-point build of libspeex 1.2.1 (`filters.c`, `lsp.c`, `lpc.c`,
//! `math_approx.h`, `ltp.c`; BSD-3-Clause, see NOTICE). The reference mixes `float` and `double`
//! arithmetic (its constants are unsuffixed `double` literals and its math functions are the
//! `double` ones of `<math.h>`). That is kept exactly, with the `f64` steps spelled out, so that
//! the output matches the reference decoder to the last bit.

use std::f64::consts::PI;

/// A pseudo-random number with standard deviation `std` (`speex_rand`).
#[inline]
pub fn speex_rand(std: f32, seed: &mut u32) -> f32 {
    *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    let ran = f32::from_bits(0x3f80_0000 | (0x007f_ffff & *seed));
    let ran = (f64::from(ran) - 1.5) as f32;
    (3.4642 * f64::from(std) * f64::from(ran)) as f32
}

/// Root-mean-square of `x`, biased away from zero (`compute_rms`).
pub fn compute_rms(x: &[f32]) -> f32 {
    let mut sum = 0f32;
    for &v in x {
        sum += v * v;
    }
    (0.1 + f64::from(sum / x.len() as f32)).sqrt() as f32
}

/// Bandwidth expansion of an LPC filter (`bw_lpc`).
pub fn bw_lpc(gamma: f32, lpc_in: &[f32], lpc_out: &mut [f32]) {
    let mut tmp = gamma;
    for (o, &i) in lpc_out.iter_mut().zip(lpc_in) {
        *o = tmp * i;
        tmp *= gamma;
    }
}

/// Replaces values outside `[min, max]` (and NaN) by the nearest bound (`sanitize_values32`).
pub fn sanitize(vec: &mut [f32], min: f32, max: f32) {
    for v in vec {
        if !(*v >= min && *v <= max) {
            if *v < min {
                *v = min;
            }
            else if *v > max {
                *v = max;
            }
            else {
                *v = 0.0;
            }
        }
    }
}

/// The DC-blocking output high-pass filter, coefficient set `filt_id` (`highpass`).
pub fn highpass(y: &mut [f32], filt_id: usize, mem: &mut [f32; 2]) {
    const PCOEF: [[f32; 3]; 5] = [
        [1.00000, -1.91120, 0.91498],
        [1.00000, -1.92683, 0.93071],
        [1.00000, -1.93338, 0.93553],
        [1.00000, -1.97226, 0.97332],
        [1.00000, -1.37000, 0.39900],
    ];
    const ZCOEF: [[f32; 3]; 5] = [
        [0.95654, -1.91309, 0.95654],
        [0.96446, -1.92879, 0.96446],
        [0.96723, -1.93445, 0.96723],
        [0.98645, -1.97277, 0.98645],
        [0.88000, -1.76000, 0.88000],
    ];

    let den = &PCOEF[filt_id.min(4)];
    let num = &ZCOEF[filt_id.min(4)];

    for v in y {
        let x = *v;
        let vout = num[0] * x + mem[0];
        mem[0] = (mem[1] + num[1] * x) + (-den[1]) * vout;
        mem[1] = num[2] * x + (-den[2]) * vout;
        *v = vout;
    }
}

/// All-pole synthesis filter `1/A(z)` with filter memory `mem`, applied in place (`iir_mem16`).
pub fn iir_mem(x: &mut [f32], den: &[f32], mem: &mut [f32]) {
    let ord = den.len();
    for v in x {
        let yi = *v + mem[0];
        let nyi = -yi;
        for j in 0..ord - 1 {
            mem[j] = mem[j + 1] + den[j] * nyi;
        }
        mem[ord - 1] = den[ord - 1] * nyi;
        *v = yi;
    }
}

/// The 32-bit float approximation of `cos` used by the reference (`spx_cos`).
#[allow(clippy::excessive_precision)]
fn spx_cos(x: f32) -> f32 {
    const C1: f32 = 0.9999932946;
    const C2: f32 = -0.4999124376;
    const C3: f32 = 0.0414877472;
    const C4: f32 = -0.0012712095;

    // The literal is the exact (non-pi/2) constant of the reference.
    #[allow(clippy::approx_constant)]
    let half_pi = 1.5707963268;
    if f64::from(x) < half_pi {
        let x = x * x;
        C1 + x * (C2 + x * (C3 + C4 * x))
    }
    else {
        let x = (PI - f64::from(x)) as f32;
        let x = x * x;
        -(C1 + x * (C2 + x * (C3 + C4 * x)))
    }
}

/// Converts LSP frequencies (radians) to LPC coefficients (`lsp_to_lpc`). `ak.len()` is the
/// order, which is 8 or 10.
pub fn lsp_to_lpc(freq: &[f32], ak: &mut [f32]) {
    let order = ak.len();
    let m = order >> 1;

    let mut wp = [0f32; 4 * 5 + 2];
    let mut x_freq = [0f32; 10];

    for (x, &f) in x_freq.iter_mut().zip(freq) {
        *x = spx_cos(f);
    }

    let mut xin1 = 1.0f32;
    let mut xin2 = 1.0f32;

    for j in 0..=order {
        let mut i2 = 0;
        let mut n4 = 0;

        for i in 0..m {
            let n1 = i * 4;
            let n2 = n1 + 1;
            let n3 = n1 + 2;
            n4 = n1 + 3;

            let xout1 = xin1 - 2.0 * x_freq[i2] * wp[n1] + wp[n2];
            let xout2 = xin2 - 2.0 * x_freq[i2 + 1] * wp[n3] + wp[n4];
            wp[n2] = wp[n1];
            wp[n4] = wp[n3];
            wp[n1] = xin1;
            wp[n3] = xin2;
            xin1 = xout1;
            xin2 = xout2;
            i2 += 2;
        }

        let xout1 = xin1 + wp[n4 + 1];
        let xout2 = xin2 - wp[n4 + 2];

        if j > 0 {
            ak[j - 1] = (xout1 + xout2) * 0.5;
        }

        wp[n4 + 1] = xin1;
        wp[n4 + 2] = xin2;

        xin1 = 0.0;
        xin2 = 0.0;
    }
}

/// Interpolates the LSPs of sub-frame `subframe` between the previous and current frame and
/// enforces a minimum spacing of `margin` so the filter stays stable (`lsp_interpolate`).
pub fn lsp_interpolate(
    old_lsp: &[f32],
    new_lsp: &[f32],
    lsp: &mut [f32],
    subframe: usize,
    nb_subframes: usize,
    margin: f32,
) {
    let len = lsp.len();
    let tmp = (1.0f32 + subframe as f32) / nb_subframes as f32;

    for i in 0..len {
        lsp[i] = (1.0 - tmp) * old_lsp[i] + tmp * new_lsp[i];
    }

    if lsp[0] < margin {
        lsp[0] = margin;
    }
    if f64::from(lsp[len - 1]) > PI - f64::from(margin) {
        lsp[len - 1] = (PI - f64::from(margin)) as f32;
    }
    for i in 1..len - 1 {
        if lsp[i] < lsp[i - 1] + margin {
            lsp[i] = lsp[i - 1] + margin;
        }
        if lsp[i] > lsp[i + 1] - margin {
            lsp[i] = 0.5 * (lsp[i] + lsp[i + 1] - margin);
        }
    }
}

/// Inner product in groups of four, as the reference accumulates it (`inner_prod`).
fn inner_prod(x: &[f32], y: &[f32]) -> f32 {
    let mut sum = 0f32;
    for (xc, yc) in x.chunks_exact(4).zip(y.chunks_exact(4)) {
        let mut part = 0f32;
        part += xc[0] * yc[0];
        part += xc[1] * yc[1];
        part += xc[2] * yc[2];
        part += xc[3] * yc[3];
        sum += part;
    }
    sum
}

/// Fractional-delay interpolators of the pitch enhancer (`shift_filt`).
const SHIFT_FILT: [[f32; 7]; 3] = [
    [-0.011915, 0.046995, -0.152373, 0.614108, 0.614108, -0.152373, 0.046995],
    [-0.0324855, 0.0859768, -0.2042986, 0.9640297, 0.2086420, -0.0302054, -0.0063646],
    [-0.0063646, -0.0302054, 0.2086420, 0.9640297, -0.2042986, 0.0859768, -0.0324855],
];

/// Finds the best (fractional) delay around `pitch` and writes the delayed `len` samples of the
/// excitation to `interp` (`interp_pitch`). `exc[base..]` is the signal, with at least
/// `pitch + 3 + 3` samples of history before `base`.
fn interp_pitch(exc: &[f32], base: usize, interp: &mut [f32], pitch: i32, len: usize) {
    // Index of the sample `off` away from `base`, which the reference reads as `exc[off]`.
    let at = |off: isize| exc[(base as isize + off) as usize];

    let mut corr = [[0f32; 7]; 4];

    for (i, c) in corr[0].iter_mut().enumerate() {
        let start = (base as isize - pitch as isize - 3 + i as isize) as usize;
        *c = inner_prod(&exc[base..base + len], &exc[start..start + len]);
    }

    for i in 0..3 {
        for j in 0..7 {
            let i1 = (3 - j as i32).max(0) as usize;
            let i2 = (10 - j as i32).min(7) as usize;
            let mut tmp = 0f32;
            for k in i1..i2 {
                tmp += SHIFT_FILT[i][k] * corr[0][j + k - 3];
            }
            corr[i + 1][j] = tmp;
        }
    }

    let mut maxi = 0;
    let mut maxj = 0;
    let mut maxcorr = corr[0][0];
    for (i, row) in corr.iter().enumerate() {
        for (j, &c) in row.iter().enumerate() {
            if c > maxcorr {
                maxcorr = c;
                maxi = i;
                maxj = j;
            }
        }
    }

    let delay = pitch as isize - maxj as isize + 3;

    for (i, out) in interp.iter_mut().enumerate().take(len) {
        let mut tmp = 0f32;
        if maxi > 0 {
            for k in 0..7 {
                tmp += at(i as isize - delay + k as isize - 3) * SHIFT_FILT[maxi - 1][k];
            }
        }
        else {
            tmp = at(i as isize - delay);
        }
        *out = tmp;
    }
}

/// The comb-filter based perceptual enhancer of the excitation (`multicomb`). Reads the 80
/// samples at `exc[base..]` (with the history `interp_pitch` needs before it) and writes the
/// enhanced excitation to `new_exc`.
pub fn multicomb(
    exc: &[f32],
    base: usize,
    new_exc: &mut [f32],
    nsf: usize,
    pitch: i32,
    max_pitch: i32,
    comb_gain: f32,
) {
    let mut iexc = [0f32; 160];

    let corr_pitch = pitch;

    interp_pitch(exc, base, &mut iexc[..nsf], corr_pitch, 80);
    if corr_pitch > max_pitch {
        interp_pitch(exc, base, &mut iexc[nsf..2 * nsf], 2 * corr_pitch, 80);
    }
    else {
        interp_pitch(exc, base, &mut iexc[nsf..2 * nsf], -corr_pitch, 80);
    }

    let exc_cur = &exc[base..base + nsf];
    let (iexc0, iexc1) = iexc[..2 * nsf].split_at(nsf);

    let iexc0_mag = (f64::from(1000.0 + inner_prod(iexc0, iexc0))).sqrt() as f32;
    let iexc1_mag = (f64::from(1000.0 + inner_prod(iexc1, iexc1))).sqrt() as f32;
    let exc_mag = (f64::from(1.0 + inner_prod(exc_cur, exc_cur))).sqrt() as f32;

    let mut corr0 = inner_prod(iexc0, exc_cur);
    if corr0 < 0.0 {
        corr0 = 0.0;
    }
    let mut corr1 = inner_prod(iexc1, exc_cur);
    if corr1 < 0.0 {
        corr1 = 0.0;
    }

    let pgain1 = if corr0 > iexc0_mag * exc_mag { 1.0 } else { corr0 / exc_mag / iexc0_mag };
    let pgain2 = if corr1 > iexc1_mag * exc_mag { 1.0 } else { corr1 / exc_mag / iexc1_mag };

    let gg1 = exc_mag / iexc0_mag;
    let gg2 = exc_mag / iexc1_mag;

    let (c1, c2) = if comb_gain > 0.0 {
        let c1 = (0.4 * f64::from(comb_gain) + 0.07) as f32;
        let c2 = (0.5 + 1.72 * (f64::from(c1) - 0.07)) as f32;
        (c1, c2)
    }
    else {
        (0.0, 0.0)
    };

    let mut g1 = 1.0 - c2 * pgain1 * pgain1;
    let mut g2 = 1.0 - c2 * pgain2 * pgain2;
    if g1 < c1 {
        g1 = c1;
    }
    if g2 < c1 {
        g2 = c1;
    }
    g1 = c1 / g1;
    g2 = c1 / g2;

    let (gain0, gain1) = if corr_pitch > max_pitch {
        ((0.7 * f64::from(g1 * gg1)) as f32, (0.3 * f64::from(g2 * gg2)) as f32)
    }
    else {
        ((0.6 * f64::from(g1 * gg1)) as f32, (0.6 * f64::from(g2 * gg2)) as f32)
    };

    for i in 0..nsf {
        new_exc[i] = exc_cur[i] + (gain0 * iexc0[i] + gain1 * iexc1[i]);
    }

    let new_ener = compute_rms(&new_exc[..nsf]);
    let mut old_ener = compute_rms(exc_cur);
    let new_ener = new_ener.max(1.0);
    old_ener = old_ener.max(1.0);
    if old_ener > new_ener {
        old_ener = new_ener;
    }
    let ngain = old_ener / new_ener;

    for v in &mut new_exc[..nsf] {
        *v *= ngain;
    }
}

/// Number of taps of the QMF filter bank of the sub-band codec.
pub const QMF_ORDER: usize = 64;

/// Re-synthesises a signal from the QMF low-band, which is the first half of `y` on entry, and
/// the high-band `x2` (each `N/2` samples). The `N` samples of the result replace `y`
/// (`qmf_synth`). `mem1` and `mem2` are the filter memories, `xx1` and `xx2` scratch buffers of
/// at least `QMF_ORDER/2 + N/2` samples.
pub fn qmf_synth(
    x2: &[f32],
    a: &[f32; QMF_ORDER],
    y: &mut [f32],
    mem1: &mut [f32; QMF_ORDER],
    mem2: &mut [f32; QMF_ORDER],
    xx1: &mut [f32],
    xx2: &mut [f32],
) {
    const M2: usize = QMF_ORDER >> 1;
    let n2 = x2.len();

    for i in 0..n2 {
        xx1[i] = y[n2 - 1 - i];
        xx2[i] = x2[n2 - 1 - i];
    }
    for i in 0..M2 {
        xx1[n2 + i] = mem1[2 * i + 1];
        xx2[n2 + i] = mem2[2 * i + 1];
    }

    for i in (0..n2).step_by(2) {
        let mut y0 = 0f32;
        let mut y1 = 0f32;
        let mut y2 = 0f32;
        let mut y3 = 0f32;

        let mut x10 = xx1[n2 - 2 - i];
        let mut x20 = xx2[n2 - 2 - i];

        for j in (0..M2).step_by(2) {
            let a0 = a[2 * j];
            let a1 = a[2 * j + 1];
            let x11 = xx1[n2 - 1 + j - i];
            let x21 = xx2[n2 - 1 + j - i];

            y0 += a0 * (x11 - x21);
            y1 += a1 * (x11 + x21);
            y2 += a0 * (x10 - x20);
            y3 += a1 * (x10 + x20);

            let a0 = a[2 * j + 2];
            let a1 = a[2 * j + 3];
            x10 = xx1[n2 + j - i];
            x20 = xx2[n2 + j - i];

            y0 += a0 * (x10 - x20);
            y1 += a1 * (x10 + x20);
            y2 += a0 * (x11 - x21);
            y3 += a1 * (x11 + x21);
        }

        y[2 * i] = 2.0 * y0;
        y[2 * i + 1] = 2.0 * y1;
        y[2 * i + 2] = 2.0 * y2;
        y[2 * i + 3] = 2.0 * y3;
    }

    for i in 0..M2 {
        mem1[2 * i + 1] = xx1[i];
        mem2[2 * i + 1] = xx2[i];
    }
}
