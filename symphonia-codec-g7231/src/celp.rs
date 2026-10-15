// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fixed-point primitives and CELP helpers.
//!
//! These follow FFmpeg's bit-exact `libavutil/common.h`, `libavcodec/celp_math.c`,
//! `libavcodec/celp_filters.c`, `libavcodec/acelp_vectors.c` and `libavcodec/g723_1.c`. Where the C
//! code relies on 32-bit `int` arithmetic that may overflow on corrupt streams, the wrapping
//! two's-complement result is reproduced explicitly.

use crate::tables::{ADAPTIVE_CB_GAIN85, ADAPTIVE_CB_GAIN170};
use crate::{LPC_ORDER, PITCH_MAX, PITCH_ORDER, SUBFRAME_LEN};

/// Clips a 64-bit value to the 32-bit range (`av_clipl_int32`).
#[inline]
pub fn clipl32(a: i64) -> i32 {
    a.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// Saturating 32-bit addition (`av_sat_add32`).
#[inline]
pub fn sat_add32(a: i32, b: i32) -> i32 {
    clipl32(i64::from(a) + i64::from(b))
}

/// Adds a doubled value to another with saturation at both stages (`av_sat_dadd32`).
#[inline]
pub fn sat_dadd32(a: i32, b: i32) -> i32 {
    sat_add32(a, sat_add32(b, b))
}

/// Clips to the signed 16-bit range (`av_clip_int16`).
#[inline]
pub fn clip16(a: i32) -> i16 {
    a.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// Integer `floor(log2(v))`, with `log2(0) = 0` (`av_log2`).
#[inline]
pub fn log2(v: u32) -> i32 {
    31 - (v | 1).leading_zeros() as i32
}

/// Number of left shifts required to normalise `num` to `width` bits
/// (`ff_g723_1_normalize_bits`). `num` is interpreted as unsigned, as in C.
#[inline]
pub fn normalize_bits(num: i32, width: i32) -> i32 {
    width - log2(num as u32) - 1
}

/// Signed left shift of a 32-bit value that never panics on out-of-range shift amounts.
#[inline]
pub fn shl(a: i32, n: i32) -> i32 {
    if (0..32).contains(&n) { a.wrapping_shl(n as u32) } else { 0 }
}

/// Arithmetic right shift that never panics on out-of-range shift amounts.
#[inline]
pub fn shr(a: i32, n: i32) -> i32 {
    a >> n.clamp(0, 31)
}

/// Division that yields 0 for a zero divisor instead of trapping.
#[inline]
pub fn div(a: i32, b: i32) -> i32 {
    if b == 0 { 0 } else { a.wrapping_div(b) }
}

/// Bit-exact `sqrt(val / 2)` (`square_root` in `g723_1dec.c`).
#[inline]
pub fn square_root(val: i32) -> i32 {
    let v = (val as u32).wrapping_shl(1);
    // `ff_sqrt` is the exact integer square root.
    let mut r = (f64::from(v)).sqrt() as u32;
    while u64::from(r) * u64::from(r) > u64::from(v) {
        r -= 1;
    }
    while u64::from(r + 1) * u64::from(r + 1) <= u64::from(v) {
        r += 1;
    }
    i32::from(((r >> 1) & !1) as i16)
}

/// Dot product of the first `len` elements (`ff_dot_product`).
#[inline]
pub fn dot_product(a: &[i16], b: &[i16], len: usize) -> i64 {
    a.iter().zip(b).take(len).map(|(&x, &y)| i64::from(x) * i64::from(y)).sum()
}

/// Saturated doubled dot product (`ff_g723_1_dot_product`). The 64-bit sum is truncated to
/// 32 bits before doubling, like the C code that stores it in an `int`.
#[inline]
pub fn dot_product_sat(a: &[i16], b: &[i16], len: usize) -> i32 {
    let sum = dot_product(a, b, len) as i32;
    sat_add32(sum, sum)
}

/// Scales `vector` based on its largest absolute value (`ff_g723_1_scale_vector`). Returns the
/// applied shift.
pub fn scale_vector(dst: &mut [i16], vector: &[i16]) -> i32 {
    let max = vector.iter().fold(0u32, |m, &v| m | u32::from(v.unsigned_abs()));
    // `av_log2_16bit`.
    let bits = (14 - log2(max)).max(0);
    for (d, &v) in dst.iter_mut().zip(vector) {
        *d = ((i32::from(v) << bits) >> 3) as i16;
    }
    bits - 3
}

/// Weighted sum of two vectors (`ff_acelp_weighted_vector_sum`).
#[inline]
pub fn weighted_sum_sample(a: i16, b: i16, wa: i32, wb: i32, rounder: i32, shift: u32) -> i16 {
    let sum = i32::from(a)
        .wrapping_mul(wa)
        .wrapping_add(i32::from(b).wrapping_mul(wb))
        .wrapping_add(rounder);
    clip16(sum >> shift)
}

/// LP synthesis filter, in place (`ff_celp_lp_synthesis_filter` with `shift = 1`,
/// `rounder = 1 << 12`, no overflow stop).
///
/// `buf[start..start + len]` holds the input on entry and the output on exit; the `LPC_ORDER`
/// samples before `start` are the filter memory.
pub fn lp_synthesis_filter(buf: &mut [i16], start: usize, coeffs: &[i16], len: usize) {
    for n in start..start + len {
        let mut sum: i32 = 1 << 12;
        for (i, &c) in coeffs.iter().enumerate().take(LPC_ORDER) {
            sum = sum.wrapping_sub(i32::from(c).wrapping_mul(i32::from(buf[n - 1 - i])));
        }
        buf[n] = clip16(((sum >> 12).wrapping_add(i32::from(buf[n]))) >> 1);
    }
}

/// Builds the adaptive codebook residual by repeating the past excitation with period `lag`
/// (`ff_g723_1_get_residual`).
///
/// `prev` is the 145-sample window of past excitation ending at the current position.
fn get_residual(residual: &mut [i16; SUBFRAME_LEN + PITCH_ORDER - 1], prev: &[i16], lag: usize) {
    // `lag` is in 17..=143 for all valid streams, so `offset >= 0`.
    let offset = PITCH_MAX - PITCH_ORDER / 2 - lag;
    residual[0] = prev[offset];
    residual[1] = prev[offset + 1];

    let offset = offset + 2;
    for (i, r) in residual.iter_mut().enumerate().skip(2) {
        *r = prev[offset + (i - 2) % lag];
    }
}

/// Generates a train of dirac functions with period `pitch_lag` (`ff_g723_1_gen_dirac_train`).
pub fn gen_dirac_train(buf: &mut [i16], pitch_lag: usize) {
    let mut vector = [0i16; SUBFRAME_LEN];
    vector.copy_from_slice(&buf[..SUBFRAME_LEN]);

    let mut i = pitch_lag;
    while i > 0 && i < SUBFRAME_LEN {
        for j in 0..SUBFRAME_LEN - i {
            buf[i + j] = buf[i + j].wrapping_add(vector[j]);
        }
        i += pitch_lag;
    }
}

/// Generates the adaptive codebook excitation (`ff_g723_1_gen_acb_excitation`).
///
/// `prev` is the 145-sample window of past excitation preceding the current subframe.
pub fn gen_acb_excitation(
    vector: &mut [i16; SUBFRAME_LEN],
    prev: &[i16],
    pitch_lag: i32,
    ad_cb_lag: i32,
    ad_cb_gain: i32,
    rate_6300: bool,
) {
    let mut residual = [0i16; SUBFRAME_LEN + PITCH_ORDER - 1];
    // The bitstream and the comfort noise generator keep this within 17..=143.
    let lag = (pitch_lag + ad_cb_lag - 1).clamp(1, (PITCH_MAX - PITCH_ORDER / 2) as i32) as usize;

    get_residual(&mut residual, prev, lag);

    // Select quantisation table.
    let table: &[i16] = if rate_6300 && pitch_lag < SUBFRAME_LEN as i32 - 2 {
        &ADAPTIVE_CB_GAIN85
    }
    else {
        &ADAPTIVE_CB_GAIN170
    };

    let entry = ad_cb_gain.max(0) as usize * 20;
    let cb = &table[entry.min(table.len() - 20)..][..PITCH_ORDER];

    for (i, v) in vector.iter_mut().enumerate() {
        let sum = dot_product(&residual[i..], cb, PITCH_ORDER) as i32;
        *v = (sat_dadd32(1 << 15, sat_add32(sum, sum)) >> 16) as i16;
    }
}
