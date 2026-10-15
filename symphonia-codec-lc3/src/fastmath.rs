// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fast math approximations: a port of `lc3_exp2f` of liblc3 `src/fastmath.h`.
//!
//! The approximation is part of the reference decoder output, and is used instead of
//! `f32::exp2` to reproduce it.

/// Number of bits to shift the exponent of an IEEE 754 single (`LC3_IEEE754_EXP_SHL`).
const IEEE754_EXP_SHL: u32 = 23;

/// Fast `2^x` approximation (`lc3_exp2f`), for `x` in the range -100 to 100, with a maximum
/// relative error of about 4e-7.
pub fn exp2f(mut x: f32) -> f32 {
    // 2^(i/8) for i from 0 to 7
    const E: [f32; 8] = [
        1.00000000e+00,
        1.09050773e+00,
        1.18920712e+00,
        1.29683955e+00,
        1.41421356e+00,
        1.54221083e+00,
        1.68179283e+00,
        1.83400809e+00,
    ];

    // Polynomial approx in range 0 to 1/8
    const P: [f32; 4] = [1.00448128e-02, 5.54563260e-02, 2.40228756e-01, 6.93147140e-01];

    // 0x1.8p20f
    const ROUND: f32 = 1572864.0;

    // Split the operand, such as x = k/8 + y, with k an integer, and |y| < 0.5/8.
    let v = x + ROUND;
    let k = v.to_bits() as i32;
    x -= v - ROUND;

    // Compute 2^x, with |x| < 1.
    // Perform polynomial approximation in range -0.5/8 to 0.5/8, and multiply by the
    // precomputed value of 2^(i/8), i in [0:7].
    let mut y = P[0] * x;
    y = (y + P[1]) * x;
    y = (y + P[2]) * x;
    y = (y + P[3]) * x;
    y = (y + 1.0) * E[(k & 7) as usize];

    // Add the exponent.
    let s = (y.to_bits() as i32).wrapping_add((k >> 3).wrapping_shl(IEEE754_EXP_SHL));

    f32::from_bits(s as u32)
}
