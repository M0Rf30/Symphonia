// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Spherical vector quantization. Ported from libopus `celt/vq.c` (`exp_rotation`,
//! `exp_rotation1`, `normalise_residual`, `extract_collapse_mask`, `alg_unquant`,
//! `renormalise_vector`, `stereo_itheta`; encoder-only `op_pvq_search_c`/`alg_quant` are not
//! ported). Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltBitstream".
//!
//! Deviates from the wave-0 stub: `alg_unquant` now returns `u32` (the collapse mask), matching
//! libopus `unsigned alg_unquant(...)` (`celt/vq.h`) — the wave-0 stub had it return `()`, which
//! doesn't match the C API `bands.rs`'s `quant_partition`/`quant_band` need (the collapse mask
//! return value feeds `cm`/`collapse_masks`). Nothing outside this crate's `bands.rs` (also mine)
//! calls `alg_unquant`, so this is a self-contained fix, recorded here for the record.

use crate::celt::bands::{SPREAD_FACTOR, SPREAD_NONE};
use crate::celt::cwrs::decode_pulses;
use crate::celt::modes::{celt_cos_norm, celt_div, celt_rsqrt_norm, EPSILON};
use crate::range::RangeDecoder;

/// C: `celt_inner_prod_c` (`celt/pitch.h`), the generic (non-SIMD) scalar dot product. A
/// private copy here (not `crate::celt::pitch`, owned by "CeltSynthesis") since `vq.rs`/
/// `bands.rs` only ever need this trivial O(N) form, never the SIMD-dispatched variants that
/// live in the pitch-search/LPC machinery.
pub(crate) fn celt_inner_prod(x: &[f32], y: &[f32], n: i32) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..n as usize {
        sum += x[i] * y[i];
    }
    sum
}

/// C: `exp_rotation1`. Applies a 2-D (Givens) rotation by `(c, s)` in-place to pairs of
/// elements of `x` spaced `stride` apart.
fn exp_rotation1(x: &mut [f32], len: i32, stride: i32, c: f32, s: f32) {
    let ms = -s;
    let mut i = 0;
    while i < len - stride {
        let x1 = x[i as usize];
        let x2 = x[(i + stride) as usize];
        x[(i + stride) as usize] = c * x2 + s * x1;
        x[i as usize] = c * x1 + ms * x2;
        i += 1;
    }
    let mut i = len - 2 * stride - 1;
    while i >= 0 {
        let x1 = x[i as usize];
        let x2 = x[(i + stride) as usize];
        x[(i + stride) as usize] = c * x2 + s * x1;
        x[i as usize] = c * x1 + ms * x2;
        i -= 1;
    }
}

/// C: `exp_rotation`. Spreads the energy of a PVQ pulse vector across the band (`dir<0`) or
/// undoes that spread (`dir>0`), controlled by `spread` (`SPREAD_NONE`..`SPREAD_AGGRESSIVE`).
pub(crate) fn exp_rotation(x: &mut [f32], len: i32, dir: i32, stride: i32, k: i32, spread: i32) {
    if 2 * k >= len || spread == SPREAD_NONE {
        return;
    }
    let factor = SPREAD_FACTOR[(spread - 1) as usize];

    let gain = celt_div(len as f32, (len + factor * k) as f32);
    let theta = 0.5 * (gain * gain);

    let c = celt_cos_norm(theta);
    let s = celt_cos_norm(1.0 - theta); // sin(theta)

    let mut stride2 = 0;
    if len >= 8 * stride {
        stride2 = 1;
        // Increment stride2 as long as (stride2+0.5)^2 < len/stride, with rounding.
        while (stride2 * stride2 + stride2) * stride + (stride >> 2) < len {
            stride2 += 1;
        }
    }
    let len = len / stride;
    for i in 0..stride {
        let band = &mut x[(i * len) as usize..(i * len + len) as usize];
        if dir < 0 {
            if stride2 != 0 {
                exp_rotation1(band, len, stride2, s, c);
            }
            exp_rotation1(band, len, 1, c, s);
        }
        else {
            exp_rotation1(band, len, 1, c, -s);
            if stride2 != 0 {
                exp_rotation1(band, len, stride2, s, -c);
            }
        }
    }
}

/// C: `normalise_residual`. Takes the decoded (integer) pulse vector `iy` and the sum of its
/// squares `ryy`, scaling it to unit norm (times `gain`) into `x`.
fn normalise_residual(iy: &[i32], x: &mut [f32], n: i32, ryy: f32, gain: f32) {
    let g = celt_rsqrt_norm(ryy) * gain;
    for i in 0..n as usize {
        x[i] = g * iy[i] as f32;
    }
}

/// C: `extract_collapse_mask`. One bit per `B`-way sub-block of `iy`, set if that sub-block
/// received any pulses.
fn extract_collapse_mask(iy: &[i32], n: i32, b: i32) -> u32 {
    if b <= 1 {
        return 1;
    }
    let n0 = n / b;
    let mut collapse_mask = 0u32;
    for i in 0..b {
        let mut tmp = 0i32;
        for j in 0..n0 {
            tmp |= iy[(i * n0 + j) as usize];
        }
        collapse_mask |= ((tmp != 0) as u32) << i;
    }
    collapse_mask
}

/// C: `alg_unquant`. Decodes and denormalizes `n`-dimensional shape `x` with `k` pulses and
/// gain `gain`; returns the per-`blocks`-subblock collapse mask.
pub fn alg_unquant(x: &mut [f32], n: i32, k: i32, spread: i32, blocks: i32, gain: f32, rd: &mut RangeDecoder<'_>) -> u32 {
    debug_assert!(k > 0);
    debug_assert!(n > 1);
    let mut iy = vec![0i32; n as usize];
    let ryy = decode_pulses(&mut iy, n, k, rd);
    normalise_residual(&iy, x, n, ryy, gain);
    exp_rotation(x, n, -1, blocks, k, spread);
    extract_collapse_mask(&iy, n, blocks)
}

/// C: `renormalise_vector`. Rescales `x` (length `n`) to unit norm times `gain`.
pub fn renormalise_vector(x: &mut [f32], n: i32, gain: f32) {
    let e = EPSILON + celt_inner_prod(x, x, n);
    let g = celt_rsqrt_norm(e) * gain;
    for i in 0..n as usize {
        x[i] *= g;
    }
}

/// C: `stereo_itheta`. Computes the (Q14) angle between the mid and side energies of a stereo
/// band, used to decide/decode the mid/side split ratio.
pub fn stereo_itheta(x: &[f32], y: &[f32], stereo: bool, n: i32) -> i32 {
    let mut emid = EPSILON;
    let mut eside = EPSILON;
    if stereo {
        for i in 0..n as usize {
            let m = x[i] + y[i];
            let s = x[i] - y[i];
            emid += m * m;
            eside += s * s;
        }
    }
    else {
        emid += celt_inner_prod(x, x, n);
        eside += celt_inner_prod(y, y, n);
    }
    let mid = crate::celt::modes::celt_sqrt(emid);
    let side = crate::celt::modes::celt_sqrt(eside);
    (0.5 + 16384.0 * 0.63662 * crate::celt::modes::fast_atan2f(side, mid)).floor() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::celt::bands::{SPREAD_AGGRESSIVE, SPREAD_LIGHT, SPREAD_NORMAL};
    use crate::range::RangeDecoder;

    /// `exp_rotation` is an orthonormal (Givens) rotation of the band; applying it forward
    /// (`dir=-1`, as `alg_unquant` does) then backward (`dir=1`) must restore the original
    /// vector (up to float rounding) for every spread setting.
    #[test]
    fn exp_rotation_round_trips() {
        for &spread in &[SPREAD_LIGHT, SPREAD_NORMAL, SPREAD_AGGRESSIVE] {
            for &(len, stride, k) in &[(8, 1, 1), (16, 2, 2), (44, 1, 3), (44, 4, 1)] {
                let original: Vec<f32> = (0..len).map(|i| ((i * 37 + 11) % 23) as f32 - 11.0).collect();
                let mut x = original.clone();
                exp_rotation(&mut x, len, -1, stride, k, spread);
                exp_rotation(&mut x, len, 1, stride, k, spread);
                for (a, b) in x.iter().zip(original.iter()) {
                    assert!((a - b).abs() < 1e-3, "{a} vs {b} (spread={spread}, len={len}, stride={stride}, k={k})");
                }
            }
        }
    }

    /// `exp_rotation` is a no-op when `spread == SPREAD_NONE` or when `2*K >= len` (both early
    /// `return`s in `celt/vq.c`).
    #[test]
    fn exp_rotation_none_is_noop() {
        let original = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let mut x = original;
        exp_rotation(&mut x, 8, -1, 1, 1, crate::celt::bands::SPREAD_NONE);
        assert_eq!(x, original);

        let mut x = original;
        exp_rotation(&mut x, 8, -1, 1, 4, SPREAD_NORMAL); // 2*K=8 >= len=8
        assert_eq!(x, original);
    }

    /// `renormalise_vector` must produce a vector whose norm is exactly `gain` (up to float
    /// rounding), regardless of the input's original scale.
    #[test]
    fn renormalise_vector_sets_norm_to_gain() {
        for &gain in &[1.0f32, 0.5, 2.0] {
            let mut x = vec![3.0f32, -4.0, 0.0, 1.0, -1.0];
            let n = x.len() as i32;
            renormalise_vector(&mut x, n, gain);
            let norm: f32 = x.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!((norm - gain).abs() < 1e-3, "norm={norm} want {gain}");
        }
    }

    /// `alg_unquant` must produce a shape whose norm is `gain` (`normalise_residual` divides
    /// by `sqrt(Ryy)`, and `exp_rotation` is norm-preserving), for a range of `(N,K,spread)`
    /// combinations, decoding from arbitrary (but valid-range-coded) packet bytes.
    #[test]
    fn alg_unquant_norm_matches_gain() {
        let data = vec![0x5Au8; 256];
        for &(n, k, blocks) in &[(4, 3, 1), (8, 5, 2), (16, 10, 4), (44, 2, 1)] {
            for &spread in &[SPREAD_LIGHT, SPREAD_NORMAL, SPREAD_AGGRESSIVE] {
                for &gain in &[1.0f32, 0.7] {
                    let mut rd = RangeDecoder::new(&data);
                    let mut x = vec![0f32; n as usize];
                    let cm = alg_unquant(&mut x, n, k, spread, blocks, gain, &mut rd);
                    let norm: f32 = x.iter().map(|v| v * v).sum::<f32>().sqrt();
                    assert!((norm - gain).abs() < 1e-2, "N={n} K={k} spread={spread}: norm={norm} want {gain}");
                    assert!(cm <= (1u32 << blocks.max(1)) - 1 || blocks <= 1, "cm={cm} out of range for blocks={blocks}");
                }
            }
        }
    }

    /// `stereo_itheta` computes `m = X+Y`, `s = X-Y` (no `/2`, just a common scale that cancels
    /// out of the ratio), so `X==Y` (identical channels) gives `s==0`: pure mid, `itheta==0`;
    /// `X==-Y` gives `m==0`: pure side, `itheta==16384`.
    #[test]
    fn stereo_itheta_extremes() {
        let x = [1.0f32, -1.0, 2.0, -2.0];
        assert_eq!(stereo_itheta(&x, &x, true, 4), 0);
        let neg_x = [-1.0f32, 1.0, -2.0, 2.0];
        assert_eq!(stereo_itheta(&x, &neg_x, true, 4), 16384);
    }
}
