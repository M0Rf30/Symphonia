// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fixed-point helper macros. Ported from libopus `silk/SigProc_FIX.h`, `silk/macros.h`, and
//! `silk/Inlines.h` (BSD-3-Clause), see NOTICE.
//!
//! libopus's SILK fixed-point macros rely on plain two's-complement wraparound for signed
//! integer overflow in several multiply-accumulate macros (`silk_MLA`, `silk_SMLABB`, etc. --
//! technically undefined behaviour in C, but always compiled as wraparound in practice). This
//! module ports every one of them as an `#[inline]` function using Rust's `wrapping_*` operators
//! (or plain `<<`/`>>`, which already wrap bits without panicking on signed types) so the exact
//! bit pattern matches a real libopus build.

#![allow(dead_code)]

pub const SILK_INT16_MAX: i32 = 0x7FFF;
pub const SILK_INT16_MIN: i32 = -0x8000;
pub const SILK_INT32_MAX: i32 = 0x7FFF_FFFF;
pub const SILK_INT32_MIN: i32 = i32::MIN;

// ---------------------------------------------------------------------------------------------
// Plain add/sub (wraparound, matches C signed overflow on two's-complement machines)
// ---------------------------------------------------------------------------------------------

#[inline]
pub fn silk_add16(a: i16, b: i16) -> i16 {
    a.wrapping_add(b)
}
#[inline]
pub fn silk_add32(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}
#[inline]
pub fn silk_add64(a: i64, b: i64) -> i64 {
    a.wrapping_add(b)
}
#[inline]
pub fn silk_sub16(a: i16, b: i16) -> i16 {
    a.wrapping_sub(b)
}
#[inline]
pub fn silk_sub32(a: i32, b: i32) -> i32 {
    a.wrapping_sub(b)
}
#[inline]
pub fn silk_sub64(a: i64, b: i64) -> i64 {
    a.wrapping_sub(b)
}

/// C: `silk_ADD32_ovflw`/`silk_SUB32_ovflw`/`silk_LSHIFT_ovflw` -- explicitly-allowed-to-overflow
/// variants; identical bit pattern to the plain wrapping ops above.
#[inline]
pub fn silk_add32_ovflw(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}
#[inline]
pub fn silk_sub32_ovflw(a: i32, b: i32) -> i32 {
    a.wrapping_sub(b)
}
#[inline]
pub fn silk_lshift_ovflw(a: i32, shift: i32) -> i32 {
    ((a as u32) << shift) as i32
}
/// C: `silk_MLA_ovflw`.
#[inline]
pub fn silk_mla_ovflw(a32: i32, b32: i32, c32: i32) -> i32 {
    silk_add32_ovflw(a32, (b32 as u32).wrapping_mul(c32 as u32) as i32)
}
/// C: `silk_SMLABB_ovflw`.
#[inline]
pub fn silk_smlabb_ovflw(a32: i32, b32: i32, c32: i32) -> i32 {
    silk_add32_ovflw(a32, silk_smulbb(b32, c32))
}

// ---------------------------------------------------------------------------------------------
// Saturation
// ---------------------------------------------------------------------------------------------

#[inline]
pub fn silk_sat8(a: i32) -> i32 {
    a.clamp(-128, 127)
}
#[inline]
pub fn silk_sat16(a: i32) -> i32 {
    a.clamp(SILK_INT16_MIN, SILK_INT16_MAX)
}
#[inline]
pub fn silk_sat32(a: i64) -> i32 {
    a.clamp(SILK_INT32_MIN as i64, SILK_INT32_MAX as i64) as i32
}
/// C: `silk_ADD_SAT32`.
#[inline]
pub fn silk_add_sat32(a: i32, b: i32) -> i32 {
    silk_sat32(a as i64 + b as i64)
}
/// C: `silk_SUB_SAT32`.
#[inline]
pub fn silk_sub_sat32(a: i32, b: i32) -> i32 {
    silk_sat32(a as i64 - b as i64)
}

// ---------------------------------------------------------------------------------------------
// Shifts
// ---------------------------------------------------------------------------------------------

#[inline]
pub fn silk_lshift(a: i32, shift: i32) -> i32 {
    a << shift
}
#[inline]
pub fn silk_lshift64(a: i64, shift: i32) -> i64 {
    a << shift
}
#[inline]
pub fn silk_rshift(a: i32, shift: i32) -> i32 {
    a >> shift
}
#[inline]
pub fn silk_rshift64(a: i64, shift: i32) -> i64 {
    a >> shift
}
/// C: `silk_LSHIFT_SAT32`: saturate before shifting.
#[inline]
pub fn silk_lshift_sat32(a: i32, shift: i32) -> i32 {
    silk_lshift(silk_limit(a, silk_rshift(SILK_INT32_MIN, shift), silk_rshift(SILK_INT32_MAX, shift)), shift)
}
/// C: `silk_RSHIFT_ROUND`. Requires `shift > 0`.
#[inline]
pub fn silk_rshift_round(a: i32, shift: i32) -> i32 {
    if shift == 1 {
        (a >> 1) + (a & 1)
    } else {
        ((a >> (shift - 1)) + 1) >> 1
    }
}
/// C: `silk_RSHIFT_ROUND64`.
#[inline]
pub fn silk_rshift_round64(a: i64, shift: i32) -> i64 {
    if shift == 1 {
        (a >> 1) + (a & 1)
    } else {
        ((a >> (shift - 1)) + 1) >> 1
    }
}
/// C: `silk_ADD_LSHIFT32`.
#[inline]
pub fn silk_add_lshift32(a: i32, b: i32, shift: i32) -> i32 {
    silk_add32(a, silk_lshift(b, shift))
}
/// C: `silk_ADD_RSHIFT32`.
#[inline]
pub fn silk_add_rshift32(a: i32, b: i32, shift: i32) -> i32 {
    silk_add32(a, silk_rshift(b, shift))
}
/// C: `silk_SUB_LSHIFT32`.
#[inline]
pub fn silk_sub_lshift32(a: i32, b: i32, shift: i32) -> i32 {
    silk_sub32(a, silk_lshift(b, shift))
}

// ---------------------------------------------------------------------------------------------
// Min/max/limit/abs/sign
// ---------------------------------------------------------------------------------------------

#[inline]
pub fn silk_min<T: PartialOrd>(a: T, b: T) -> T {
    if a < b {
        a
    } else {
        b
    }
}
#[inline]
pub fn silk_max<T: PartialOrd>(a: T, b: T) -> T {
    if a > b {
        a
    } else {
        b
    }
}
/// C: `silk_LIMIT` (a.k.a. `silk_LIMIT_16`/`silk_LIMIT_32`/`silk_LIMIT_int`).
#[inline]
pub fn silk_limit<T: PartialOrd>(a: T, limit1: T, limit2: T) -> T {
    if limit1 > limit2 {
        if a > limit1 {
            limit1
        } else if a < limit2 {
            limit2
        } else {
            a
        }
    } else if a > limit2 {
        limit2
    } else if a < limit1 {
        limit1
    } else {
        a
    }
}
/// C: `silk_abs`. NOTE: matches the C macro's documented quirk of being wrong for `silk_intXX_MIN`
/// (it wraps back to the same negative value instead of panicking/saturating), which is what a
/// real two's-complement build does; kept bit-exact rather than "fixed".
#[inline]
pub fn silk_abs(a: i32) -> i32 {
    if a > 0 {
        a
    } else {
        a.wrapping_neg()
    }
}
#[inline]
pub fn silk_sign(a: i32) -> i32 {
    if a > 0 {
        1
    } else if a < 0 {
        -1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------------------------
// Multiplies
// ---------------------------------------------------------------------------------------------

#[inline]
pub fn silk_mul(a32: i32, b32: i32) -> i32 {
    a32.wrapping_mul(b32)
}
/// C: `silk_MLA`: `a32 + (b32 * c32)`.
#[inline]
pub fn silk_mla(a32: i32, b32: i32, c32: i32) -> i32 {
    silk_add32(a32, silk_mul(b32, c32))
}
/// C: `silk_SMULWB`: `(a32 * (opus_int16)b32) >> 16`.
#[inline]
pub fn silk_smulwb(a32: i32, b32: i32) -> i32 {
    (((a32 as i64) * (b32 as i16) as i64) >> 16) as i32
}
/// C: `silk_SMLAWB`.
#[inline]
pub fn silk_smlawb(a32: i32, b32: i32, c32: i32) -> i32 {
    silk_add32(a32, silk_smulwb(b32, c32))
}
/// C: `silk_SMULWT`: `(a32 * (b32 >> 16)) >> 16`.
#[inline]
pub fn silk_smulwt(a32: i32, b32: i32) -> i32 {
    (((a32 as i64) * ((b32 >> 16) as i64)) >> 16) as i32
}
/// C: `silk_SMLAWT`.
#[inline]
pub fn silk_smlawt(a32: i32, b32: i32, c32: i32) -> i32 {
    silk_add32(a32, silk_smulwt(b32, c32))
}
/// C: `silk_SMULBB`: `(opus_int16)a32 * (opus_int16)b32`.
#[inline]
pub fn silk_smulbb(a32: i32, b32: i32) -> i32 {
    ((a32 as i16) as i32).wrapping_mul((b32 as i16) as i32)
}
/// C: `silk_SMLABB`.
#[inline]
pub fn silk_smlabb(a32: i32, b32: i32, c32: i32) -> i32 {
    silk_add32(a32, silk_smulbb(b32, c32))
}
/// C: `silk_SMULBT`: `(opus_int16)a32 * (b32 >> 16)`.
#[inline]
pub fn silk_smulbt(a32: i32, b32: i32) -> i32 {
    ((a32 as i16) as i32).wrapping_mul(b32 >> 16)
}
/// C: `silk_SMLABT`.
#[inline]
pub fn silk_smlabt(a32: i32, b32: i32, c32: i32) -> i32 {
    silk_add32(a32, silk_smulbt(b32, c32))
}
/// C: `silk_SMULTT`: `(a32 >> 16) * (b32 >> 16)`.
#[inline]
pub fn silk_smultt(a32: i32, b32: i32) -> i32 {
    (a32 >> 16).wrapping_mul(b32 >> 16)
}
/// C: `silk_SMLATT`.
#[inline]
pub fn silk_smlatt(a32: i32, b32: i32, c32: i32) -> i32 {
    silk_add32(a32, silk_smultt(b32, c32))
}
/// C: `silk_SMULL`: exact 32x32 -> 64 bit multiply.
#[inline]
pub fn silk_smull(a32: i32, b32: i32) -> i64 {
    (a32 as i64) * (b32 as i64)
}
/// C: `silk_SMLAL`.
#[inline]
pub fn silk_smlal(a64: i64, b32: i32, c32: i32) -> i64 {
    silk_add64(a64, silk_smull(b32, c32))
}
/// C: `silk_SMLALBB`.
#[inline]
pub fn silk_smlalbb(a64: i64, b16: i16, c16: i16) -> i64 {
    silk_add64(a64, (b16 as i32).wrapping_mul(c16 as i32) as i64)
}
/// C: `silk_SMULWW`: `(a32 * b32) >> 16`, exact (both operands full 32-bit).
#[inline]
pub fn silk_smulww(a32: i32, b32: i32) -> i32 {
    (((a32 as i64) * (b32 as i64)) >> 16) as i32
}
/// C: `silk_SMLAWW`.
#[inline]
pub fn silk_smlaww(a32: i32, b32: i32, c32: i32) -> i32 {
    silk_add32(a32, silk_smulww(b32, c32))
}
/// C: `silk_SMMUL`: `(a32 * b32) >> 32`, i.e. the high 32 bits of the 64-bit product.
#[inline]
pub fn silk_smmul(a32: i32, b32: i32) -> i32 {
    (silk_smull(a32, b32) >> 32) as i32
}

// ---------------------------------------------------------------------------------------------
// Division
// ---------------------------------------------------------------------------------------------

/// C: `silk_DIV32`. Truncates toward zero, matching both C and Rust integer division.
#[inline]
pub fn silk_div32(a32: i32, b32: i32) -> i32 {
    a32.wrapping_div(b32)
}
/// C: `silk_DIV32_16`.
#[inline]
pub fn silk_div32_16(a32: i32, b16: i32) -> i32 {
    a32.wrapping_div(b16)
}

// ---------------------------------------------------------------------------------------------
// Bit counting / rotate
// ---------------------------------------------------------------------------------------------

/// C: `silk_CLZ16`.
#[inline]
pub fn silk_clz16(in16: i16) -> i32 {
    (in16 as u16).leading_zeros() as i32
}
/// C: `silk_CLZ32`.
#[inline]
pub fn silk_clz32(in32: i32) -> i32 {
    (in32 as u32).leading_zeros() as i32
}
/// C: `silk_CLZ64`.
#[inline]
pub fn silk_clz64(in64: i64) -> i32 {
    (in64 as u64).leading_zeros() as i32
}
/// C: `silk_ROR32`.
#[inline]
pub fn silk_ror32(a32: i32, rot: i32) -> i32 {
    let x = a32 as u32;
    if rot == 0 {
        a32
    } else if rot < 0 {
        x.rotate_left((-rot) as u32) as i32
    } else {
        x.rotate_right(rot as u32) as i32
    }
}
/// C: `silk_CLZ_FRAC`. Returns `(leading_zeros, frac_Q7)`.
#[inline]
pub fn silk_clz_frac(input: i32) -> (i32, i32) {
    let lz = silk_clz32(input);
    let frac_q7 = silk_ror32(input, 24 - lz) & 0x7f;
    (lz, frac_q7)
}

// ---------------------------------------------------------------------------------------------
// Approximations (Inlines.h)
// ---------------------------------------------------------------------------------------------

/// C: `silk_SQRT_APPROX`.
#[inline]
pub fn silk_sqrt_approx(x: i32) -> i32 {
    if x <= 0 {
        return 0;
    }
    let (lz, frac_q7) = silk_clz_frac(x);
    let mut y = if lz & 1 != 0 { 32768 } else { 46214 };
    y >>= silk_rshift(lz, 1);
    y = silk_smlawb(y, y, silk_smulbb(213, frac_q7));
    y
}

/// C: `silk_DIV32_varQ`.
#[inline]
pub fn silk_div32_varq(a32: i32, b32: i32, qres: i32) -> i32 {
    debug_assert!(b32 != 0);
    debug_assert!(qres >= 0);

    let a_headrm = silk_clz32(silk_abs(a32)) - 1;
    let mut a32_nrm = silk_lshift(a32, a_headrm);
    let b_headrm = silk_clz32(silk_abs(b32)) - 1;
    let b32_nrm = silk_lshift(b32, b_headrm);

    let b32_inv = silk_div32_16(SILK_INT32_MAX >> 2, silk_rshift(b32_nrm, 16));

    let mut result = silk_smulwb(a32_nrm, b32_inv);

    a32_nrm = silk_sub32_ovflw(a32_nrm, silk_lshift_ovflw(silk_smmul(b32_nrm, result), 3));

    result = silk_smlawb(result, a32_nrm, b32_inv);

    let lshift = 29 + a_headrm - b_headrm - qres;
    if lshift < 0 {
        silk_lshift_sat32(result, -lshift)
    } else if lshift < 32 {
        silk_rshift(result, lshift)
    } else {
        0
    }
}

/// C: `silk_INVERSE32_varQ`.
#[inline]
pub fn silk_inverse32_varq(b32: i32, qres: i32) -> i32 {
    debug_assert!(b32 != 0);
    debug_assert!(qres > 0);

    let b_headrm = silk_clz32(silk_abs(b32)) - 1;
    let b32_nrm = silk_lshift(b32, b_headrm);

    let b32_inv = silk_div32_16(SILK_INT32_MAX >> 2, silk_rshift(b32_nrm, 16));

    let mut result = silk_lshift(b32_inv, 16);

    let err_q32 = silk_lshift(((1i32) << 29) - silk_smulwb(b32_nrm, b32_inv), 3);

    result = silk_smlaww(result, err_q32, b32_inv);

    let lshift = 61 - b_headrm - qres;
    if lshift <= 0 {
        silk_lshift_sat32(result, -lshift)
    } else if lshift < 32 {
        silk_rshift(result, lshift)
    } else {
        0
    }
}

/// C: `RAND_MULTIPLIER`.
pub const RAND_MULTIPLIER: i32 = 196_314_165;
/// C: `RAND_INCREMENT`.
pub const RAND_INCREMENT: i32 = 907_633_515;
/// C: `silk_RAND`.
#[inline]
pub fn silk_rand(seed: i32) -> i32 {
    silk_mla_ovflw(RAND_INCREMENT, seed, RAND_MULTIPLIER)
}

/// C: `SILK_FIX_CONST`.
#[inline]
pub const fn silk_fix_const(c: f64, q: u32) -> i32 {
    (c * ((1i64 << q) as f64) + 0.5) as i32
}

/// C: `silk_lin2log` (`silk/lin2log.c`). Approximation of `128 * log2()`.
#[inline]
pub fn silk_lin2log(in_lin: i32) -> i32 {
    let (lz, frac_q7) = silk_clz_frac(in_lin);
    silk_add_lshift32(silk_smlawb(frac_q7, silk_mul(frac_q7, 128 - frac_q7), 179), 31 - lz, 7)
}

/// C: `silk_log2lin` (`silk/log2lin.c`). Approximation of `2^()`, the inverse of
/// [`silk_lin2log`].
#[inline]
pub fn silk_log2lin(in_log_q7: i32) -> i32 {
    if in_log_q7 < 0 {
        return 0;
    } else if in_log_q7 >= 3967 {
        return SILK_INT32_MAX;
    }
    let mut out = silk_lshift(1, silk_rshift(in_log_q7, 7));
    let frac_q7 = in_log_q7 & 0x7f;
    if in_log_q7 < 2048 {
        out = silk_add_rshift32(
            out,
            silk_mul(out, silk_smlawb(frac_q7, silk_smulbb(frac_q7, 128 - frac_q7), -174)),
            7,
        );
    } else {
        out = silk_mla(out, silk_rshift(out, 7), silk_smlawb(frac_q7, silk_smulbb(frac_q7, 128 - frac_q7), -174));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smulwb_matches_reference() {
        // 2^16 * 3 truncated as i16 (3) >> 16 = 3
        assert_eq!(silk_smulwb(1 << 16, 3), 3);
        assert_eq!(silk_smulwb(-(1 << 16), 3), -3);
        assert_eq!(silk_smulwb(1000, 70000), silk_smulwb(1000, (70000i32 as i16) as i32));
    }

    #[test]
    fn smulbb_truncates_to_i16() {
        assert_eq!(silk_smulbb(0x1_0005, 0x1_0007), 5 * 7);
    }

    #[test]
    fn sat16_clamps() {
        assert_eq!(silk_sat16(40000), SILK_INT16_MAX);
        assert_eq!(silk_sat16(-40000), SILK_INT16_MIN);
        assert_eq!(silk_sat16(100), 100);
    }

    #[test]
    fn rshift_round_matches_reference() {
        assert_eq!(silk_rshift_round(5, 1), 3); // (5>>1)+(5&1) = 2+1 = 3
        assert_eq!(silk_rshift_round(-5, 1), -2); // (-5>>1)+(-5&1) = -3 + 1 = -2
        assert_eq!(silk_rshift_round(9, 2), 2); // ((9>>1)+1)>>1 = (4+1)>>1 = 2
    }

    #[test]
    fn clz32_matches_reference() {
        assert_eq!(silk_clz32(0), 32);
        assert_eq!(silk_clz32(1), 31);
        assert_eq!(silk_clz32(-1), 0);
        assert_eq!(silk_clz32(0x0000_00FF), 24);
    }

    #[test]
    fn ror32_matches_reference() {
        assert_eq!(silk_ror32(1, 1), i32::MIN);
        assert_eq!(silk_ror32(1, 0), 1);
        assert_eq!(silk_ror32(2, -1), 4);
    }

    #[test]
    fn sqrt_approx_reasonable() {
        // sqrt(10000) = 100; approximation should be within ~10%.
        let y = silk_sqrt_approx(10000);
        assert!((90..=110).contains(&y), "sqrt_approx(10000) = {y}");
    }

    #[test]
    fn div32_varq_matches_plain_division() {
        // 100 / 4 in Q8 domain: (100<<8)/4 = 6400
        let r = silk_div32_varq(100, 4, 8);
        assert!((6300..=6500).contains(&r), "div32_varq = {r}");
    }

    #[test]
    fn rand_lcg_is_stable() {
        let s0 = 12345i32;
        let s1 = silk_rand(s0);
        let s2 = silk_rand(s1);
        assert_ne!(s1, s2);
        // Deterministic: same seed -> same output.
        assert_eq!(silk_rand(s0), s1);
    }

    #[test]
    fn add_sat32_saturates() {
        assert_eq!(silk_add_sat32(i32::MAX, 1), i32::MAX);
        assert_eq!(silk_sub_sat32(i32::MIN, 1), i32::MIN);
        assert_eq!(silk_add_sat32(1, 1), 2);
    }

    #[test]
    fn limit_matches_reference() {
        assert_eq!(silk_limit(5, 0, 10), 5);
        assert_eq!(silk_limit(-5, 0, 10), 0);
        assert_eq!(silk_limit(15, 0, 10), 10);
    }
}
