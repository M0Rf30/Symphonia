// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Floating-point CELP/ACELP helper routines.
//!
//! Ported from FFmpeg `libavcodec/celp_filters.c`, `celp_math.c`, `acelp_filters.c` and
//! `acelp_vectors.c` (LGPL-2.1+, see `NOTICE`). The arithmetic precision (`f32` versus `f64`) of
//! every operation follows the C original so that results agree with it to the last bits.
//!
//! Filters that need filter memory take buffers with the history in front of the samples.

use crate::tables::B60_SINC;

/// LPC filter order.
pub(crate) const LP_ORDER: usize = 10;
/// Samples per subframe.
pub(crate) const SUBFRAME: usize = 40;

/// Dot product, accumulated sequentially in single precision (`ff_scalarproduct_float_c`).
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut p = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        p += x * y;
    }
    p
}

/// `out[i] = wa * a[i] + wb * b[i]` (`ff_weighted_vector_sumf`).
pub(crate) fn weighted_sum(out: &mut [f32], a: &[f32], b: &[f32], wa: f32, wb: f32) {
    for ((o, x), y) in out.iter_mut().zip(a).zip(b) {
        *o = wa * x + wb * y;
    }
}

/// All-pole LPC synthesis filter (`ff_celp_lp_synthesis_filterf`), in place.
///
/// `buf` holds `LP_ORDER` samples of filter history followed by `n` input samples, which are
/// replaced by the filter output.
pub(crate) fn lp_synthesis(buf: &mut [f32], coeffs: &[f32; LP_ORDER], n: usize) {
    for k in LP_ORDER..LP_ORDER + n {
        let mut v = buf[k];
        for (i, c) in coeffs.iter().enumerate() {
            v -= c * buf[k - 1 - i];
        }
        buf[k] = v;
    }
}

/// All-zero LPC filter (`ff_celp_lp_zero_synthesis_filterf`).
///
/// `input` holds `LP_ORDER` samples of history followed by `out.len()` samples.
pub(crate) fn lp_zero_synthesis(out: &mut [f32], coeffs: &[f32; LP_ORDER], input: &[f32]) {
    for (n, o) in out.iter_mut().enumerate() {
        let k = n + LP_ORDER;
        let mut v = input[k];
        for (i, c) in coeffs.iter().enumerate() {
            v += c * input[k - 1 - i];
        }
        *o = v;
    }
}

/// `out[k] = inp[k] + fac * lagged[(k - lag) mod n]` (`ff_celp_circ_addf`).
pub(crate) fn circ_add(
    out: &mut [f32; SUBFRAME],
    inp: &[f32; SUBFRAME],
    lagged: &[f32; SUBFRAME],
    lag: usize,
    fac: f32,
) {
    let n = SUBFRAME;
    for k in 0..lag.min(n) {
        out[k] = inp[k] + fac * lagged[n + k - lag];
    }
    for k in lag..n {
        out[k] = inp[k] + fac * lagged[k - lag];
    }
}

/// In-place variant of [`circ_add`] with `out == inp` (the accumulating form).
pub(crate) fn circ_add_acc(
    out: &mut [f32; SUBFRAME],
    lagged: &[f32; SUBFRAME],
    lag: usize,
    fac: f32,
) {
    let n = SUBFRAME;
    for k in 0..lag.min(n) {
        out[k] += fac * lagged[n + k - lag];
    }
    for k in lag..n {
        out[k] += fac * lagged[k - lag];
    }
}

/// Fractional-delay interpolation of the excitation with the 1/6 resolution `B60_SINC` filter
/// (`ff_acelp_interpolatef` with precision 6 and filter length 10).
///
/// Reads `buf[src - 9 ..= src + SUBFRAME + 8]` and writes `buf[dst .. dst + SUBFRAME]` in place,
/// in increasing order like the reference (when the lag is short the reads see already written
/// samples). `src` is `dst + 1 - lag`.
pub(crate) fn interpolate(buf: &mut [f32], dst: usize, src: usize, frac_pos: usize) {
    for n in 0..SUBFRAME {
        let mut idx = 0;
        let mut v = 0.0f32;
        for i in 0..10 {
            v += buf[src + n + i] * B60_SINC[idx + frac_pos];
            idx += 6;
            v += buf[src + n - (i + 1)] * B60_SINC[idx - frac_pos];
        }
        buf[dst + n] = v;
    }
}

/// Second order IIR section (`ff_acelp_apply_order_2_transfer_function`), in place.
pub(crate) fn order_2_filter(
    samples: &mut [f32],
    zeros: &[f32; 2],
    poles: &[f32; 2],
    gain: f32,
    mem: &mut [f32; 2],
) {
    for s in samples.iter_mut() {
        let tmp = gain * *s - poles[0] * mem[0] - poles[1] * mem[1];
        *s = tmp + zeros[0] * mem[0] + zeros[1] * mem[1];
        mem[1] = mem[0];
        mem[0] = tmp;
    }
}

/// Tilt compensation filter (`ff_tilt_compensation`).
pub(crate) fn tilt_compensation(mem: &mut f32, tilt: f32, samples: &mut [f32; SUBFRAME]) {
    let new_mem = samples[SUBFRAME - 1];
    for i in (1..SUBFRAME).rev() {
        samples[i] -= tilt * samples[i - 1];
    }
    samples[0] -= tilt * *mem;
    *mem = new_mem;
}

/// Adaptive gain control (`ff_adaptive_gain_control`), in place.
pub(crate) fn adaptive_gain_control(
    samples: &mut [f32; SUBFRAME],
    speech_energy: f32,
    alpha: f32,
    gain_mem: &mut f32,
) {
    let post_energy = dot(samples, samples);
    let mut scale = 1.0f32;
    if post_energy != 0.0 {
        scale = f64::from(speech_energy / post_energy).sqrt() as f32;
    }
    scale = (f64::from(scale) * (1.0 - f64::from(alpha))) as f32;

    let mut mem = *gain_mem;
    for s in samples.iter_mut() {
        mem = alpha * mem + scale;
        *s *= mem;
    }
    *gain_mem = mem;
}

/// Scales `v` so that its sum of squares equals `sum_of_squares`
/// (`ff_scale_vector_to_given_sum_of_squares`).
pub(crate) fn scale_to_sum_of_squares(v: &mut [f32; SUBFRAME], sum_of_squares: f32) {
    let mut scale = dot(v, v);
    if scale != 0.0 {
        scale = f64::from(sum_of_squares / scale).sqrt() as f32;
    }
    for s in v.iter_mut() {
        *s *= scale;
    }
}

/// Sparse representation of an algebraic codebook vector (`AMRFixed`).
#[derive(Clone, Copy, Default)]
pub(crate) struct Fixed {
    pub n: usize,
    pub x: [usize; 10],
    pub y: [f32; 10],
    pub no_repeat_mask: u32,
    pub pitch_lag: usize,
    pub pitch_fac: f32,
}

impl Fixed {
    /// Adds the (pitch sharpened) pulses, scaled by `scale`, to `out` (`ff_set_fixed_vector`).
    pub fn add_to(&self, out: &mut [f32], scale: f32) {
        if self.pitch_lag == 0 {
            return;
        }
        for i in 0..self.n {
            let mut x = self.x[i];
            let repeats = (self.no_repeat_mask >> i) & 1 == 0;
            let mut y = self.y[i] * scale;
            loop {
                if x >= out.len() {
                    break;
                }
                out[x] += y;
                y *= self.pitch_fac;
                x += self.pitch_lag;
                if !(x < out.len() && repeats) {
                    break;
                }
            }
        }
    }

    /// Zeroes the positions touched by [`Fixed::add_to`] (`ff_clear_fixed_vector`).
    pub fn clear_in(&self, out: &mut [f32]) {
        if self.pitch_lag == 0 {
            return;
        }
        for i in 0..self.n {
            let mut x = self.x[i];
            let repeats = (self.no_repeat_mask >> i) & 1 == 0;
            loop {
                if x >= out.len() {
                    break;
                }
                out[x] = 0.0;
                x += self.pitch_lag;
                if !(x < out.len() && repeats) {
                    break;
                }
            }
        }
    }
}
