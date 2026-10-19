//! Fixed-point arithmetic reproducing the Fraunhofer FDK reference semantics.
//!
//! The reference decoder operates on signed 32-bit Q31 values (`FIXP_DBL`, `LONG`)
//! and 16-bit Q15 values (`FIXP_SGL`, `SHORT`), using arithmetic right shifts for
//! all scaling. Reproducing these semantics bit-exactly lets us validate against
//! the reference decoder output.

/// Q31 fixed-point value.
pub type Dbl = i32;
/// Q15 fixed-point value.
pub type Sgl = i16;

pub const DFRACT_BITS: i32 = 32;
pub const FRACT_BITS: i32 = 16;

/// `fixmuldiv2_DD`: (a * b) >> 32.
#[inline]
pub fn f_mult_div2(a: Dbl, b: Dbl) -> Dbl {
    (((a as i64) * (b as i64)) >> 32) as Dbl
}

/// `fMult` (DD): `fixmuldiv2_DD(a, b) << 1`.
#[inline]
pub fn f_mult(a: Dbl, b: Dbl) -> Dbl {
    f_mult_div2(a, b).wrapping_shl(1)
}

/// `fMultDiv2` (DS): SGL x DBL -> DBL, `(a << 16) * b >> 32`.
#[inline]
pub fn f_mult_div2_ds(a: Sgl, b: Dbl) -> Dbl {
    (((a as i32 as i64) << 16) * (b as i64) >> 32) as Dbl
}

/// `fMult` (DS): SGL x DBL -> DBL.
#[inline]
pub fn f_mult_ds(a: Sgl, b: Dbl) -> Dbl {
    f_mult_div2_ds(a, b).wrapping_shl(1)
}

/// `fMultDiv2` (SD): DBL x SGL -> DBL.
#[inline]
pub fn f_mult_div2_sd(a: Dbl, b: Sgl) -> Dbl {
    f_mult_div2_ds(b, a)
}

/// `fPow2Div2` (D): (a * a) >> 32.
#[inline]
pub fn f_pow2_div2(a: Dbl) -> Dbl {
    (((a as i64) * (a as i64)) >> 32) as Dbl
}

/// `fixnormz_D`: number of leading sign bits, i.e. position of the highest set bit
/// counted from the top (1..32).
#[inline]
pub fn fixnormz_d(a: Dbl) -> i32 {
    // Highest set bit position (1-based from MSB) == 32 - clz.
    let v = a as u32;
    let clz = v.leading_zeros() as i32;
    32 - clz
}

/// `fixnorm_D`: like `fixnormz_D` minus 1.
#[inline]
pub fn fixnorm_d(a: Dbl) -> i32 {
    fixnormz_d(a) - 1
}

/// `CntLeadingZeros` for `maxVal` (a non-negative DBL): 32 - 1 - fNormz.
#[inline]
pub fn cnt_leading_zeros(a: Dbl) -> i32 {
    DFRACT_BITS - 1 - fixnormz_d(a)
}

/// `scaleValue`: arithmetic shift by `scalefactor` (left if positive, right if negative).
#[inline]
pub fn scale_value(value: Dbl, scalefactor: i32) -> Dbl {
    if scalefactor > 0 {
        value.wrapping_shl(scalefactor as u32)
    } else {
        value >> (-scalefactor)
    }
}

/// `scaleValue` for SGL.
#[inline]
pub fn scale_value_sgl(value: Sgl, scalefactor: i32) -> Sgl {
    if scalefactor > 0 {
        value.wrapping_shl(scalefactor as u32)
    } else {
        value >> (-scalefactor)
    }
}

/// `scaleValueSaturate` for DBL, matching the reference `scale.h` implementation.
#[inline]
pub fn scale_value_saturate(value: Dbl, scalefactor: i32) -> Dbl {
    let headroom = fixnormz_d(value ^ (value >> 31)); // 1..32
    let minval_plus1: Dbl = i32::MIN + 1;
    if scalefactor >= 0 {
        if headroom <= scalefactor {
            if value > 0 {
                i32::MAX
            } else {
                minval_plus1
            }
        } else {
            (value.wrapping_shl(scalefactor as u32)).max(minval_plus1)
        }
    } else {
        let sf = -scalefactor;
        if (DFRACT_BITS - headroom) < sf {
            0
        } else {
            (value >> sf).max(minval_plus1)
        }
    }
}

/// `scaleValuesSaturate(vector, len, scalefactor)` in place.
pub fn scale_values_saturate(v: &mut [Dbl], scalefactor: i32) {
    for x in v.iter_mut() {
        *x = scale_value_saturate(*x, scalefactor);
    }
}

/// `getScalefactor`: returns the number of redundant sign bits in the maximum
/// absolute value of the vector (so that the vector can be left-shifted by this
/// amount). Returns 31 if all values are zero.
pub fn get_scalefactor(v: &[Dbl]) -> i32 {
    let mut max_abs: Dbl = 0;
    for &x in v {
        let a = if x < 0 { x.wrapping_neg() } else { x };
        if a > max_abs {
            max_abs = a;
        }
    }
    if max_abs == 0 {
        31
    } else {
        DFRACT_BITS - 1 - fixnormz_d(max_abs)
    }
}

/// `getScalefactor` for SGL.
pub fn get_scalefactor_sgl(v: &[Sgl]) -> i32 {
    let mut max_abs: Sgl = 0;
    for &x in v {
        let a = if x < 0 { x.wrapping_neg() } else { x };
        if a > max_abs {
            max_abs = a;
        }
    }
    if max_abs == 0 {
        15
    } else {
        15 - (max_abs as u16).leading_zeros() as i32
    }
}

/// `SATURATE_RIGHT_SHIFT` for SGL output (`dBits = 16`).
#[inline]
pub fn saturate_right_shift_sgl(src: i32, scale: i32) -> Sgl {
    let shifted = src >> scale;
    let max: i32 = 0x7fff;
    let min: i32 = -0x8000;
    if shifted > max {
        max as Sgl
    } else if shifted < min {
        min as Sgl
    } else {
        shifted as Sgl
    }
}

/// Convert a Q31 DBL to f32 (`FX_DBL2FL`).
#[inline]
pub fn dbl_to_f32(v: Dbl) -> f32 {
    v as f32 / 2147483648.0
}

/// Convert a Q15 SGL to f32.
#[inline]
pub fn sgl_to_f32(v: Sgl) -> f32 {
    v as f32 / 32768.0
}

/// `FL2FXCONST_SGL` rounding (matches the reference macro, including its 0.5 rounding).
pub fn fl2fx_sgl(f: f32) -> Sgl {
    let scaled = f * 32768.0;
    let r = if f > 0.0 {
        (scaled + 0.5).floor()
    } else {
        (scaled - 0.5).ceil()
    };
    if r >= 32767.0 {
        0x7fff
    } else if r <= -32768.0 {
        -0x8000
    } else {
        r as Sgl
    }
}

/// `FL2FXCONST_DBL` rounding.
pub fn fl2fx_dbl(f: f64) -> Dbl {
    let scaled = f * 2147483648.0;
    let r = if f >= 0.0 {
        (scaled + 0.5).floor()
    } else {
        (scaled - 0.5).ceil()
    };
    if r >= 2147483647.0 {
        i32::MAX
    } else if r <= -2147483648.0 {
        i32::MIN
    } else {
        r as Dbl
    }
}
