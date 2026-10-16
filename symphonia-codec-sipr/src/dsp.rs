// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CELP/ACELP building blocks shared by the SIPR modes: FFmpeg's `celp_filters.c`, `lsp.c`,
//! `acelp_filters.c` and `acelp_vectors.c`.

use std::f64::consts::{LN_2, LN_10};

/// The sum of the products of two vectors (`ff_scalarproduct_float_c`).
pub fn scalar_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).fold(0.0, |sum, (&a, &b)| sum + a * b)
}

/// `10^x` (`ff_exp10`).
pub fn exp10(x: f64) -> f64 {
    (x * (LN_10 / LN_2)).exp2()
}

/// The all-pole synthesis filter `out[n] = input[n] - sum(coeffs[i - 1] * out[n - i])`
/// (`ff_celp_lp_synthesis_filterf`).
///
/// The output of `n` samples starts at `out[order]`; the `order` samples before it are the
/// filter memory.
pub fn lp_synthesis_filter(out: &mut [f32], coeffs: &[f32], input: &[f32], n: usize) {
    let order = coeffs.len();

    for i in 0..n {
        let mut v = input[i];

        for (k, &c) in coeffs.iter().enumerate() {
            v -= c * out[order + i - 1 - k];
        }

        out[order + i] = v;
    }
}

/// As [`lp_synthesis_filter`] for an input that is the output buffer itself: `out[order + n]` is
/// filtered in place.
pub fn lp_synthesis_filter_in_place(out: &mut [f32], coeffs: &[f32], n: usize) {
    let order = coeffs.len();

    for i in 0..n {
        let mut v = out[order + i];

        for (k, &c) in coeffs.iter().enumerate() {
            v -= c * out[order + i - 1 - k];
        }

        out[order + i] = v;
    }
}

/// The all-zero filter `out[n] = input[n] + sum(coeffs[i - 1] * input[n - i])`
/// (`ff_celp_lp_zero_synthesis_filterf`). `input[order..]` holds the `n` samples to filter and
/// the `order` samples before them are the filter memory.
pub fn lp_zero_synthesis_filter(out: &mut [f32], coeffs: &[f32], input: &[f32], n: usize) {
    let order = coeffs.len();

    for i in 0..n {
        let mut v = input[order + i];

        for (k, &c) in coeffs.iter().enumerate() {
            v += c * input[order + i - 1 - k];
        }

        out[i] = v;
    }
}

/// Fractional-delay interpolation of `buf` in place (`ff_acelp_interpolatef`). The `length`
/// samples at `out` are computed from the ones around `inp`, in order, so that they may overlap.
pub fn interpolate(
    buf: &mut [f32],
    out: usize,
    inp: usize,
    filter: &[f32],
    precision: usize,
    frac_pos: usize,
    filter_length: usize,
    length: usize,
) {
    for n in 0..length {
        let mut idx = 0;
        let mut v = 0.0;

        for i in 0..filter_length {
            v += buf[inp + n + i] * filter[idx + frac_pos];
            idx += precision;
            v += buf[inp + n - (i + 1)] * filter[idx - frac_pos];
        }

        buf[out + n] = v;
    }
}

/// The second order IIR filter that follows the synthesis
/// (`ff_acelp_apply_order_2_transfer_function`).
pub fn order_2_transfer_function(
    out: &mut [f32],
    input: &[f32],
    zero: [f32; 2],
    pole: [f32; 2],
    gain: f32,
    mem: &mut [f32; 2],
) {
    for (o, &i) in out.iter_mut().zip(input) {
        let tmp = gain * i - pole[0] * mem[0] - pole[1] * mem[1];
        *o = tmp + zero[0] * mem[0] + zero[1] * mem[1];

        mem[1] = mem[0];
        mem[0] = tmp;
    }
}

/// Tilt compensation of the post-filter (`ff_tilt_compensation`).
pub fn tilt_compensation(mem: &mut f32, tilt: f32, samples: &mut [f32]) {
    let new_mem = samples[samples.len() - 1];

    for i in (1..samples.len()).rev() {
        samples[i] -= tilt * samples[i - 1];
    }

    samples[0] -= tilt * *mem;
    *mem = new_mem;
}

/// Adaptive gain control of the post-filter, in place (`ff_adaptive_gain_control`).
pub fn adaptive_gain_control(samples: &mut [f32], speech_energy: f32, alpha: f32, mem: &mut f32) {
    let energy = scalar_product(samples, samples);
    let mut scale = 1.0f32;

    if energy != 0.0 {
        scale = f64::from(speech_energy / energy).sqrt() as f32;
    }

    scale = (f64::from(scale) * (1.0 - f64::from(alpha))) as f32;

    for s in samples {
        *mem = alpha * *mem + scale;
        *s *= *mem;
    }
}

/// `out = weight_a * a + weight_b * b` (`ff_weighted_vector_sumf`).
pub fn weighted_vector_sum(buf: &mut [f32], out: usize, b: &[f32], wa: f32, wb: f32) {
    for (i, &b) in b.iter().enumerate() {
        buf[out + i] = wa * buf[out + i] + wb * b;
    }
}

/// Sorts a nearly sorted vector with an insertion sort (`ff_sort_nearly_sorted_floats`).
pub fn sort_nearly_sorted(vals: &mut [f32]) {
    for i in 0..vals.len().saturating_sub(1) {
        for j in (0..=i).rev() {
            if vals[j] > vals[j + 1] {
                vals.swap(j, j + 1);
            }
            else {
                break;
            }
        }
    }
}

/// Enforces a minimum spacing between consecutive values (`ff_set_min_dist_lsf`).
pub fn set_min_dist(lsf: &mut [f32], min_spacing: f64) {
    let mut prev = 0.0f32;

    for l in lsf {
        *l = f64::from(*l).max(f64::from(prev) + min_spacing) as f32;
        prev = *l;
    }
}

/// The polynomial of an LSP half set (`lsp2polyf` of `lsp.c`), TIA/EIA/IS-733 2.4.3.3.5-1/2.
fn lsp_to_poly(lsp: &[f64], f: &mut [f64], half_order: usize) {
    f[0] = 1.0;
    f[1] = -2.0 * lsp[0];

    for i in 2..=half_order {
        let val = -2.0 * lsp[2 * i - 2];

        f[i] = val * f[i - 1] + 2.0 * f[i - 2];

        for j in (2..i).rev() {
            f[j] += f[j - 1] * val + f[j - 2];
        }

        f[1] += val;
    }
}

/// Converts the LSPs of an AMR-WB style filter of `order` to LPCs (`ff_amrwb_lsp2lpc`).
pub fn amrwb_lsp_to_lpc(lsp: &[f64], lp: &mut [f32], order: usize) {
    let half = order >> 1;
    let last = lsp[order - 1];

    let mut pa = [0.0f64; 11];
    // `qa[i + 1]` is the `qa[i]` of the reference; `qa[0]` is its `qa[-1]`.
    let mut qa = [0.0f64; 11];

    lsp_to_poly(lsp, &mut pa, half);
    lsp_to_poly(&lsp[1..], &mut qa[1..], half - 1);

    let (mut i, mut j) = (1, order - 1);

    while i < half {
        let paf = pa[i] * (1.0 + last);
        let qaf = (qa[i + 1] - qa[i - 1]) * (1.0 - last);

        lp[i - 1] = ((paf + qaf) * 0.5) as f32;
        lp[j - 1] = ((paf - qaf) * 0.5) as f32;

        i += 1;
        j -= 1;
    }

    lp[half - 1] = ((1.0 + last) * pa[half] * 0.5) as f32;
    lp[order - 1] = last as f32;
}

/// Converts LSPs to LPCs (`ff_acelp_lspd2lpc`).
pub fn lspd_to_lpc(lsp: &[f64], lpc: &mut [f32], half_order: usize) {
    let mut pa = [0.0f64; 11];
    let mut qa = [0.0f64; 11];

    lsp_to_poly(lsp, &mut pa, half_order);
    lsp_to_poly(&lsp[1..], &mut qa, half_order);

    for h in (0..half_order).rev() {
        let paf = pa[h + 1] + pa[h];
        let qaf = qa[h + 1] - qa[h];

        lpc[h] = (0.5 * (paf + qaf)) as f32;
        lpc[2 * half_order - 1 - h] = (0.5 * (paf - qaf)) as f32;
    }
}

/// A sparse fixed-codebook vector (`AMRFixed`).
#[derive(Default)]
pub struct Fixed {
    pub n: usize,
    pub x: [usize; 10],
    pub y: [f32; 10],
    pub pitch_lag: usize,
    pub pitch_fac: f32,
}

impl Fixed {
    /// Writes the pulses into `out`, repeating each at multiples of the pitch lag
    /// (`ff_set_fixed_vector`, with every pulse repeating).
    pub fn set_vector(&self, out: &mut [f32], scale: f32) {
        if self.pitch_lag == 0 {
            return;
        }

        for i in 0..self.n {
            let mut x = self.x[i];
            let mut y = self.y[i] * scale;

            loop {
                out[x] += y;
                y *= self.pitch_fac;
                x += self.pitch_lag;

                if x >= out.len() {
                    break;
                }
            }
        }
    }
}

/// Computes the fixed-codebook gain and updates the prediction error history
/// (`ff_amr_set_fixed_gain`).
pub fn set_fixed_gain(
    factor: f32,
    mean_energy: f32,
    history: &mut [f32; 4],
    energy_mean: f32,
    pred: &[f32; 4],
) -> f32 {
    let mean = if mean_energy != 0.0 { mean_energy } else { 1.0 };

    let val = f64::from(factor)
        * exp10(0.05 * f64::from(scalar_product(pred, history) + energy_mean))
        / f64::from(mean.sqrt());

    history.copy_within(1.., 0);
    history[3] = (20.0 * f64::from(factor).log10()) as f32;

    val as f32
}

/// The pitch lag decoding of AMR (`ff_decode_pitch_lag`) for a 6 bit resolution. Returns the
/// integer lag and the fraction in {-1, 0, 1} thirds.
pub fn decode_pitch_lag(
    pitch_index: i32,
    prev_lag: i32,
    subframe: usize,
    third_as_first: bool,
) -> (i32, i32) {
    const PITCH_DELAY_MIN: i32 = 20;
    const PITCH_DELAY_MAX: i32 = 143;

    let idx = if subframe == 0 || (subframe == 2 && third_as_first) {
        if pitch_index < 197 { pitch_index + 59 } else { 3 * pitch_index - 335 }
    }
    else {
        pitch_index - 1 + 3 * (prev_lag - 5).clamp(PITCH_DELAY_MIN, PITCH_DELAY_MAX - 9)
    };

    // n * 10923 >> 15 is floor(n / 3) for 0 <= n <= 32767.
    let lag = (idx * 10923) >> 15;
    (lag, idx - 3 * lag - 1)
}
