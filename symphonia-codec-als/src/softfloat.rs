// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The software floating point arithmetic of the ALS floating-point (RAFP) reconstruction, a port
//! of FFmpeg `libavutil/softfloat_ieee754.h` and the `multiply` function of
//! `libavcodec/alsdec.c` (LGPL-2.1+, Copyright (c) 2009 Thilo Borgmann), see the `NOTICE` file.
//!
//! The arithmetic is deliberately bit-exact with FFmpeg, including its handling of the rounding
//! and of the sign in [`multiply`].

const EXP_BIAS: i32 = 127;
const MANT_BITS: u32 = 23;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoftFloat {
    pub sign: i32,
    pub mant: u64,
    pub exp: i32,
}

pub const FLOAT_0: SoftFloat = SoftFloat { sign: 0, mant: 0, exp: -126 };
pub const FLOAT_1: SoftFloat = SoftFloat { sign: 0, mant: 0, exp: 0 };

impl SoftFloat {
    /// Normalize as defined by the IEEE 754 single precision format.
    fn normalize(mut self) -> SoftFloat {
        while self.mant >= 0x100_0000 {
            self.exp += 1;
            self.mant >>= 1;
        }
        self.mant &= 0x007f_ffff;
        self
    }

    /// The softfloat of value `n * 2^e`.
    pub fn from_int(n: i64, e: i32) -> SoftFloat {
        let (sign, n) = if n < 0 { (1, n.wrapping_neg()) } else { (0, n) };
        SoftFloat { sign, mant: (n as u64) << MANT_BITS, exp: e }.normalize()
    }

    /// Make a softfloat out of IEEE 754 bits.
    pub fn from_bits(n: u32) -> SoftFloat {
        SoftFloat {
            sign: ((n & 0x8000_0000) >> 31) as i32,
            mant: u64::from(n & 0x7f_ffff),
            exp: i32::from(((n & 0x7f80_0000) >> 23) as u8 as i8),
        }
    }

    /// Divide `self` by `b`, which must not be zero.
    pub fn div(self, b: SoftFloat) -> SoftFloat {
        let a = self.normalize();
        let b = b.normalize();
        let sign = a.sign ^ b.sign;
        let mant = ((a.mant | 0x80_0000) << MANT_BITS) / (b.mant | 0x80_0000);
        // The reference stores the quotient in an `int32_t`.
        let mant = i64::from(mant as u32 as i32) as u64;
        SoftFloat { sign, mant, exp: a.exp - b.exp }.normalize()
    }

    /// Strict equality after normalisation.
    pub fn equals(self, b: SoftFloat) -> bool {
        self.normalize() == b.normalize()
    }
}

/// Multiply two softfloats and handle the rounding (`multiply` of FFmpeg `alsdec.c`).
pub fn multiply(a: SoftFloat, b: SoftFloat) -> SoftFloat {
    let sign = a.sign ^ b.sign;

    // Multiply the mantissa bits in a 64-bit register.
    let mut mantissa_temp = a.mant.wrapping_mul(b.mant);
    let mut mask_64 = 1u64 << 47;

    if mantissa_temp == 0 {
        return FLOAT_0;
    }

    // Count the valid bit count.
    let mut bit_count = 48i32;
    while mantissa_temp & mask_64 == 0 && mask_64 != 0 {
        bit_count -= 1;
        mask_64 >>= 1;
    }

    // Round off.
    let cutoff_bit_count = bit_count - 24;
    if cutoff_bit_count > 0 {
        let low32 = mantissa_temp as u32;
        let last_2_bits = (low32 >> (cutoff_bit_count - 1)) & 0x3;
        let rest = u64::from(low32) & ((1u64 << (cutoff_bit_count - 1)) - 1);
        if last_2_bits == 0x3 || (last_2_bits == 0x1 && rest != 0) {
            // Need to round up.
            mantissa_temp = mantissa_temp.wrapping_add(1u64 << cutoff_bit_count);
        }
    }

    let mut mantissa = if cutoff_bit_count >= 0 {
        (mantissa_temp >> cutoff_bit_count) as u32
    }
    else {
        (mantissa_temp << (-cutoff_bit_count)) as u32
    };

    // Need one more shift?
    if mantissa & 0x0100_0000 != 0 {
        bit_count += 1;
        mantissa >>= 1;
    }

    let mut return_val = if sign == 0 { 0x8000_0000u32 } else { 0 };

    let exp = (a.exp + b.exp + bit_count - 47).clamp(-126, 127);
    return_val |= ((exp as u32) << 23) & 0x7f80_0000;
    return_val |= mantissa;

    SoftFloat::from_bits(return_val)
}

/// Combine the integer part `pcm` of a sample with its difference-signal mantissa to a float,
/// following Tables 14.45 and 14.46 of ISO/IEC 14496-3 as implemented by FFmpeg. Returns the
/// IEEE 754 bit pattern.
pub fn reconstruct(pcm: i32, acf: SoftFloat, raw_mantissa: i32, shift_value: i32) -> u32 {
    let scale = SoftFloat::from_int(1, 23);
    let mut pcm_sf = SoftFloat::from_int(i64::from(pcm), 0).div(scale);

    if !acf.equals(FLOAT_1) {
        pcm_sf = multiply(acf, pcm_sf);
    }

    let sign = pcm_sf.sign as u32;
    let mut e = pcm_sf.exp as u32;
    let mut mantissa = ((pcm_sf.mant | 0x80_0000) as u32).wrapping_add(raw_mantissa as u32);

    while mantissa >= 0x100_0000 {
        e = e.wrapping_add(1);
        mantissa >>= 1;
    }

    if mantissa != 0 {
        e = e.wrapping_add((shift_value - 127) as u32);
    }
    mantissa &= 0x007f_ffff;

    (sign << 31) | (e.wrapping_add(EXP_BIAS as u32) << 23) | mantissa
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_int_conversion() {
        // 1 = 1.0 * 2^0, 3 = 1.5 * 2^1.
        assert_eq!(SoftFloat::from_int(1, 0), SoftFloat { sign: 0, mant: 0, exp: 0 });
        assert_eq!(SoftFloat::from_int(3, 0), SoftFloat { sign: 0, mant: 0x40_0000, exp: 1 });
        assert_eq!(SoftFloat::from_int(-3, 0).sign, 1);
    }

    #[test]
    fn verify_reconstruct_identity() {
        // With an acf of 1.0, no mantissa and a zero shift offset of 127, an integer sample
        // n / 2^23 becomes the float n / 2^23.
        let bits = reconstruct(1 << 22, FLOAT_1, 0, 127);
        assert_eq!(f32::from_bits(bits), 0.5);
        let bits = reconstruct(-(3 << 20), FLOAT_1, 0, 127);
        assert_eq!(f32::from_bits(bits), -0.375);
    }
}
