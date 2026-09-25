// Symphonia Musepack demuxer+decoder
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Requantization coefficients and the scale-factor ladder.
//!
//! Ported from libmpcdec `requant.c` (BSD-3-Clause), see `NOTICE`. As with `synth.rs`, this only
//! implements the non-fixed-point (float) build: `MAKE_MPC_SAMPLE_EX(X, shift)` is a plain cast
//! to `f32` in that build, so the coefficients below are the literal double-precision constants
//! from the C source, narrowed to `f32`.

/// `__Cc`: requantization coefficients, indexable as `Cc[-1..=17]` in the C source (`Cc` is
/// `__Cc + 1`). [`cc`] below reproduces that offset.
#[rustfmt::skip]
const CC_RAW: [f32; 19] = [
    111.285962475327, 65536.000000000000, 21845.333333333332, 13107.200000000001, 9362.285714285713,
    7281.777777777777, 4369.066666666666, 2114.064516129032, 1040.253968253968, 516.031496062992,
    257.003921568627, 128.250489236790, 64.062561094819, 32.015632633121, 16.003907203907,
    8.000976681723, 4.000244155527, 2.000061037018, 1.000015259021,
];

/// `__Dc`: requantization offsets, indexable as `Dc[-1..=17]` (`Dc` is `__Dc + 1`).
#[rustfmt::skip]
const DC_RAW: [i32; 19] =
    [2, 0, 1, 2, 3, 4, 7, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767];

/// `Res_bit`: bits per raw sample for quantizer indices `8..=17`.
pub const RES_BIT: [u8; 18] = [0, 0, 0, 0, 0, 0, 0, 0, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

/// `Cc[res]` for `res` in `-1..=17`. Out-of-range `res` (only reachable via a malformed stream)
/// safely returns `0.0` instead of panicking.
#[inline]
pub fn cc(res: i32) -> f32 {
    let idx = res + 1;
    if idx < 0 {
        0.0
    }
    else {
        CC_RAW.get(idx as usize).copied().unwrap_or(0.0)
    }
}

/// `Dc[res]` for `res` in `-1..=17`.
#[inline]
pub fn dc(res: i32) -> i32 {
    let idx = res + 1;
    if idx < 0 {
        0
    }
    else {
        DC_RAW.get(idx as usize).copied().unwrap_or(0)
    }
}

/// Ratio between successive entries of the scale-factor ladder (`0.83298066476582673961`).
const SCF_RATIO: f64 = 0.83298066476582673961;

/// Ported from libmpcdec `requant.c` (`mpc_decoder_scale_output` / `mpc_decoder_init_quant`).
///
/// Builds the 256-entry scale-factor lookup table (`d->SCF`). `scale_factor` corresponds to the
/// same parameter of `mpc_decoder_init_quant`; this crate always initializes decoders with
/// `1.0` (matching bitstream decode with no replay-gain post-scaling -- gain application, if
/// desired, is a player-level concern exposed via the `ReplayGain*` tags).
pub fn build_scf_table(scale_factor: f64) -> [f32; 256] {
    // `factor *= 1.0 / (1 << (MPC_FIXED_POINT_SHIFT - 1))`, `MPC_FIXED_POINT_SHIFT == 16`.
    let factor = scale_factor / f64::from(1u32 << 15);

    let mut scf = [0f32; 256];
    scf[1] = factor as f32;

    let mut f1 = factor * SCF_RATIO;
    let mut f2 = factor / SCF_RATIO;
    for n in 1u32..=128 {
        let up = (1u32.wrapping_add(n) as u8) as usize;
        let down = (1i32.wrapping_sub(n as i32) as u8) as usize;
        scf[up] = f1 as f32;
        scf[down] = f2 as f32;
        f1 *= SCF_RATIO;
        f2 *= 1.0 / SCF_RATIO;
    }
    scf
}

/// Maps a raw (possibly negative, out-of-`u8`-range) SCF index to `0..256` exactly like the C
/// `& 0xFF` on `d->SCF[SCF_Index[...] & 0xFF]`.
#[inline]
pub fn scf_index(x: i32) -> usize {
    (x as u8) as usize
}
