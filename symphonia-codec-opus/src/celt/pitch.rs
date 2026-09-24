// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pitch search, used only by CELT PLC. Ported from libopus `celt/pitch.c`
//! (`pitch_downsample`, `pitch_search`, `celt_pitch_xcorr_c`, `find_best_pitch`,
//! `remove_doubling`, `compute_pitch_gain`), float build. Ported from libopus (BSD-3-Clause),
//! see NOTICE. Owner (wave 1): "CeltSynthesis".

use crate::celt::lpc::{celt_autocorr, celt_lpc};

/// C: `celt_inner_prod_c`.
pub(crate) fn celt_inner_prod(x: &[f32], y: &[f32], n: usize) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..n {
        sum += x[i] * y[i];
    }
    sum
}

/// C: `dual_inner_prod_c`.
pub(crate) fn dual_inner_prod(x: &[f32], y01: &[f32], y02: &[f32], n: usize) -> (f32, f32) {
    let mut xy1 = 0.0f32;
    let mut xy2 = 0.0f32;
    for i in 0..n {
        xy1 += x[i] * y01[i];
        xy2 += x[i] * y02[i];
    }
    (xy1, xy2)
}

/// C: `celt_pitch_xcorr_c` (float build: plain cross-correlation, no `maxcorr` tracking).
pub(crate) fn celt_pitch_xcorr(x: &[f32], y: &[f32], xcorr: &mut [f32], len: usize, max_pitch: usize) {
    for i in 0..max_pitch {
        xcorr[i] = celt_inner_prod(x, &y[i..], len);
    }
}

/// C: `find_best_pitch` (float build: no `yshift`/`maxcorr` fixed-point scaling).
fn find_best_pitch(xcorr: &[f32], y: &[f32], len: usize, max_pitch: usize) -> [usize; 2] {
    let mut syy = 1.0f32;
    let mut best_num = [-1.0f32, -1.0f32];
    let mut best_den = [0.0f32, 0.0f32];
    let mut best_pitch = [0usize, 1usize];

    for j in 0..len {
        syy += y[j] * y[j];
    }
    for i in 0..max_pitch {
        if xcorr[i] > 0.0 {
            let xcorr16 = xcorr[i] * 1e-12f32;
            let num = xcorr16 * xcorr16;
            if num * best_den[1] > best_num[1] * syy {
                if num * best_den[0] > best_num[0] * syy {
                    best_num[1] = best_num[0];
                    best_den[1] = best_den[0];
                    best_pitch[1] = best_pitch[0];
                    best_num[0] = num;
                    best_den[0] = syy;
                    best_pitch[0] = i;
                }
                else {
                    best_num[1] = num;
                    best_den[1] = syy;
                    best_pitch[1] = i;
                }
            }
        }
        syy += y[i + len] * y[i + len] - y[i] * y[i];
        syy = syy.max(1.0);
    }
    best_pitch
}

/// C: `celt_fir5` (a specialized 5-tap FIR used only by `pitch_downsample`'s LPC whitening).
fn celt_fir5(x: &mut [f32], num: &[f32; 5], n: usize) {
    let (num0, num1, num2, num3, num4) = (num[0], num[1], num[2], num[3], num[4]);
    let (mut mem0, mut mem1, mut mem2, mut mem3, mut mem4) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for i in 0..n {
        let mut sum = x[i];
        sum += num0 * mem0;
        sum += num1 * mem1;
        sum += num2 * mem2;
        sum += num3 * mem3;
        sum += num4 * mem4;
        mem4 = mem3;
        mem3 = mem2;
        mem2 = mem1;
        mem1 = mem0;
        mem0 = x[i];
        x[i] = sum;
    }
}

/// C: `pitch_downsample`. Downsamples (and lightly whitens) `x` (`channels` interleaved-by-
/// pointer input buffers of `len` samples each) into `x_lp` (`len/2` samples).
pub fn pitch_downsample(x: &[&[f32]], x_lp: &mut [f32], len: i32, channels: i32) {
    let len = len as usize;
    let c = channels as usize;
    const C1: f32 = 0.8;

    for i in 1..len / 2 {
        x_lp[i] = 0.25 * x[0][2 * i - 1] + 0.25 * x[0][2 * i + 1] + 0.5 * x[0][2 * i];
    }
    x_lp[0] = 0.25 * x[0][1] + 0.5 * x[0][0];
    if c == 2 {
        for i in 1..len / 2 {
            x_lp[i] += 0.25 * x[1][2 * i - 1] + 0.25 * x[1][2 * i + 1] + 0.5 * x[1][2 * i];
        }
        x_lp[0] += 0.25 * x[1][1] + 0.5 * x[1][0];
    }

    let mut ac = [0f32; 5];
    celt_autocorr(&x_lp[..len / 2], &mut ac, None, 0, 4, (len / 2) as i32);

    ac[0] *= 1.0001;
    for i in 1..=4 {
        ac[i] -= ac[i] * (0.008 * i as f32) * (0.008 * i as f32);
    }

    let mut lpc = [0f32; 4];
    celt_lpc(&mut lpc, &ac, 4);
    let mut tmp = 1.0f32;
    for c in lpc.iter_mut() {
        tmp *= 0.9;
        *c *= tmp;
    }
    let lpc2 = [
        lpc[0] + 0.8,
        lpc[1] + C1 * lpc[0],
        lpc[2] + C1 * lpc[1],
        lpc[3] + C1 * lpc[2],
        C1 * lpc[3],
    ];
    celt_fir5(&mut x_lp[..len / 2], &lpc2, len / 2);
}

/// C: `pitch_search`. Returns the estimated pitch lag.
pub fn pitch_search(x_lp: &[f32], y: &[f32], len: i32, max_pitch: i32) -> i32 {
    let len = len as usize;
    let max_pitch = max_pitch as usize;
    assert!(len > 0 && max_pitch > 0);
    let lag = len + max_pitch;

    let mut x_lp4 = vec![0f32; len / 4];
    let mut y_lp4 = vec![0f32; lag / 4];
    let mut xcorr = vec![0f32; max_pitch / 2];

    for j in 0..len / 4 {
        x_lp4[j] = x_lp[2 * j];
    }
    for j in 0..lag / 4 {
        y_lp4[j] = y[2 * j];
    }

    celt_pitch_xcorr(&x_lp4, &y_lp4, &mut xcorr, len / 4, max_pitch / 4);
    let best_pitch = find_best_pitch(&xcorr, &y_lp4, len / 4, max_pitch / 4);

    for i in 0..max_pitch / 2 {
        xcorr[i] = 0.0;
        if (i as i64 - 2 * best_pitch[0] as i64).abs() > 2 && (i as i64 - 2 * best_pitch[1] as i64).abs() > 2 {
            continue;
        }
        let sum = celt_inner_prod(x_lp, &y[i..], len / 2);
        xcorr[i] = sum.max(-1.0);
    }
    let best_pitch = find_best_pitch(&xcorr, y, len / 2, max_pitch / 2);

    let offset: i32 = if best_pitch[0] > 0 && best_pitch[0] < (max_pitch / 2) - 1 {
        let a = xcorr[best_pitch[0] - 1];
        let b = xcorr[best_pitch[0]];
        let c = xcorr[best_pitch[0] + 1];
        if (c - a) > 0.7 * (b - a) {
            1
        }
        else if (a - c) > 0.7 * (b - c) {
            -1
        }
        else {
            0
        }
    }
    else {
        0
    };
    2 * best_pitch[0] as i32 - offset
}

/// C: `compute_pitch_gain` (float build).
fn compute_pitch_gain(xy: f32, xx: f32, yy: f32) -> f32 {
    xy / (1.0 + xx * yy).sqrt()
}

/// C: `second_check`.
const SECOND_CHECK: [i32; 16] = [0, 0, 3, 2, 3, 2, 5, 2, 3, 2, 3, 2, 5, 2, 3, 2];

/// C: `remove_doubling`. Refines a coarse pitch estimate, returning the pitch gain. Not called
/// by `celt_decode_lost`'s pitch-based PLC in this libopus version (only `pitch_search` is), but
/// ported for API completeness against the wave-0 contract.
#[allow(clippy::too_many_arguments)]
pub fn remove_doubling(
    x: &[f32],
    maxperiod: i32,
    minperiod: i32,
    n: i32,
    t0: &mut i32,
    prev_period: i32,
    prev_gain: f32,
) -> f32 {
    let minperiod0 = minperiod;
    let maxperiod = (maxperiod / 2) as usize;
    let minperiod = (minperiod / 2) as usize;
    *t0 /= 2;
    let prev_period = (prev_period / 2) as usize;
    let n = (n / 2) as usize;
    // `x` is offset so that `x[0]` in C is `x[maxperiod]` here (C: `x += maxperiod`).
    let base = maxperiod;
    if *t0 as usize >= maxperiod {
        *t0 = maxperiod as i32 - 1;
    }

    let mut t = *t0 as usize;
    let t0_orig = t;

    let mut yy_lookup = vec![0f32; maxperiod + 1];
    let (xx, mut xy) = dual_inner_prod(&x[base..], &x[base..], &x[base - t0_orig..], n);
    yy_lookup[0] = xx;
    let mut yy = xx;
    for i in 1..=maxperiod {
        yy = yy + x[base - i] * x[base - i] - x[base + n - i] * x[base + n - i];
        yy_lookup[i] = yy.max(0.0);
    }
    yy = yy_lookup[t0_orig];
    let mut best_xy = xy;
    let mut best_yy = yy;
    let g0 = compute_pitch_gain(xy, xx, yy);
    let mut g = g0;

    for k in 2..=15usize {
        let t1 = (2 * t0_orig as i32 + k as i32) as usize / (2 * k);
        if t1 < minperiod {
            break;
        }
        let t1b = if k == 2 {
            if t1 + t0_orig > maxperiod {
                t0_orig
            }
            else {
                t0_orig + t1
            }
        }
        else {
            (2 * SECOND_CHECK[k] as usize * t0_orig + k) / (2 * k)
        };
        let (xy_a, xy_b) = dual_inner_prod(&x[base..], &x[base - t1..], &x[base - t1b..], n);
        xy = 0.5 * (xy_a + xy_b);
        yy = 0.5 * (yy_lookup[t1] + yy_lookup[t1b]);
        let g1 = compute_pitch_gain(xy, xx, yy);
        let cont = if (t1 as i64 - prev_period as i64).abs() <= 1 {
            prev_gain
        }
        else if (t1 as i64 - prev_period as i64).abs() <= 2 && 5 * (k * k) < t0_orig {
            0.5 * prev_gain
        }
        else {
            0.0
        };
        let mut thresh = (0.3f32).max(0.7 * g0 - cont);
        if t1 < 3 * minperiod {
            thresh = (0.4f32).max(0.85 * g0 - cont);
        }
        else if t1 < 2 * minperiod {
            thresh = (0.5f32).max(0.9 * g0 - cont);
        }
        if g1 > thresh {
            best_xy = xy;
            best_yy = yy;
            t = t1;
            g = g1;
        }
    }
    best_xy = best_xy.max(0.0);
    let mut pg = if best_yy <= best_xy { 1.0 } else { best_xy / (best_yy + 1.0) };

    let mut xcorr = [0f32; 3];
    for k in 0..3usize {
        let off = t + k - 1;
        xcorr[k] = celt_inner_prod(&x[base..], &x[base - off..], n);
    }
    let offset: i32 = if (xcorr[2] - xcorr[0]) > 0.7 * (xcorr[1] - xcorr[0]) {
        1
    }
    else if (xcorr[0] - xcorr[2]) > 0.7 * (xcorr[1] - xcorr[2]) {
        -1
    }
    else {
        0
    };
    if pg > g {
        pg = g;
    }
    *t0 = 2 * t as i32 + offset;
    if *t0 < minperiod0 {
        *t0 = minperiod0;
    }
    pg
}
