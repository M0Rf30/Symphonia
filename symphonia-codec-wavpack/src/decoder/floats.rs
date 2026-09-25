// Symphonia
// Copyright (c) 2019-2024 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// WavPack IEEE 32-bit floating point restoration.
// Ported from unpack_floats.c by Conifer Software / David Bryant
// (https://github.com/dbry/WavPack, BSD-3-Clause licence, see NOTICE).
//
// This module deals with the restoration of floating-point data. Note that no
// floating point math is involved here...the values are only processed with
// bit operations that directly access the mantissa, exponent and sign fields
// (the WavPack reference calls this the `f32` type: a plain `int32_t` used
// purely as an IEEE-754 bit pattern).

use super::v4v5::Bits;

// flags for float_flags (wavpack_local.h)
const FLOAT_SHIFT_ONES: u8 = 1; // bits left-shifted into float = '1'
const FLOAT_SHIFT_SAME: u8 = 2; // bits left-shifted into float are the same
const FLOAT_SHIFT_SENT: u8 = 4; // bits shifted into float are sent literally
const FLOAT_ZEROS_SENT: u8 = 8; // "zeros" are not all real zeros
const FLOAT_NEG_ZEROS: u8 = 0x10; // contains negative zeros

/// Parsed contents of an `ID_FLOAT_INFO` sub-block (always exactly 4 bytes).
#[derive(Default, Clone, Copy)]
pub struct FloatInfo {
    pub flags: u8,
    pub shift: u8,
    pub max_exp: u8,
    #[allow(dead_code)]
    pub norm_exp: u8,
}

pub fn parse_float_info(data: &[u8]) -> Option<FloatInfo> {
    if data.len() != 4 {
        return None;
    }
    Some(FloatInfo { flags: data[0], shift: data[1], max_exp: data[2], norm_exp: data[3] })
}

/// Restore `values` (raw 24-bit-shifted integer magnitudes produced by the entropy
/// decoder + decorrelator) into IEEE-754 `f32` bit patterns in place.
///
/// `wvx` is the `ID_WVX_BITSTREAM` sub-block bit reader (the "extension" stream that
/// carries the bits needed for bit-exact float reconstruction), if present in this
/// block. It is embedded in the same WavPack block as the main audio bitstream (not a
/// separate file), so no external file is required for lossless float decoding.
/// Mirrors `float_values()` / `float_values_nowvx()` in unpack_floats.c.
pub fn float_values(values: &mut [i32], info: &FloatInfo, wvx: Option<&mut Bits<'_>>) {
    match wvx {
        Some(bs) => float_values_wvx(values, info, bs),
        None => float_values_nowvx(values, info),
    }
}

// `float_min_shifted_zeros` / `float_max_shifted_ones` are only ever populated from the
// 5-bit prefix carried by `ID_WVX_NEW_BITSTREAM`, which the reference encoder only ever
// emits for the 32-bit-integer optimization path (`CONFIG_OPTIMIZE_32BIT`), never for
// `FLOAT_DATA`. For float streams these two fields are therefore always their
// zero-initialized default, which is what is hard-coded below.
const FLOAT_MIN_SHIFTED_ZEROS: i32 = 0;
const FLOAT_MAX_SHIFTED_ONES: i32 = 0;

/// Port of `float_values()` in unpack_floats.c (the "has wvx bitstream" path).
fn float_values_wvx(values: &mut [i32], info: &FloatInfo, bs: &mut Bits<'_>) {
    for v in values.iter_mut() {
        let mut shift_count: i32 = 0;
        let mut exp = info.max_exp as i32;
        let mut outval: u32 = 0;

        if *v == 0 {
            if info.flags & FLOAT_ZEROS_SENT != 0 {
                if bs.getbit() != 0 {
                    let mantissa = bs.getbits(23);
                    outval = (outval & !0x007f_ffff) | (mantissa & 0x007f_ffff);

                    if exp >= 25 {
                        let e = bs.getbits(8);
                        outval = (outval & !0x7f80_0000) | ((e & 0xff) << 23);
                    }

                    let sign = bs.getbit();
                    outval = (outval & 0x7fff_ffff) | (sign << 31);
                }
                else if info.flags & FLOAT_NEG_ZEROS != 0 {
                    let sign = bs.getbit();
                    outval = (outval & 0x7fff_ffff) | (sign << 31);
                }
            }
        }
        else {
            let mut mag = (*v as u32).wrapping_shl((info.shift & 0x1f) as u32);

            if (mag as i32) < 0 {
                mag = mag.wrapping_neg();
                outval |= 1 << 31;
            }

            if mag == 0x0100_0000 {
                if bs.getbit() != 0 {
                    let mantissa = bs.getbits(23);
                    outval = (outval & !0x007f_ffff) | (mantissa & 0x007f_ffff);
                }
                outval = (outval & !0x7f80_0000) | (0xffu32 << 23);
            }
            else {
                if exp != 0 {
                    loop {
                        if mag & 0x0080_0000 != 0 {
                            break;
                        }
                        exp -= 1;
                        if exp == 0 {
                            break;
                        }
                        shift_count += 1;
                        mag <<= 1;
                    }
                }

                shift_count &= 0x1f;

                if shift_count != 0 {
                    if (info.flags & FLOAT_SHIFT_ONES) != 0
                        || ((info.flags & FLOAT_SHIFT_SAME) != 0 && bs.getbit() != 0)
                    {
                        mag |= (1u32 << shift_count) - 1;
                    }
                    else if (info.flags & FLOAT_SHIFT_SENT) != 0 {
                        let mask = (1u32 << shift_count) - 1;
                        let mut num_zeros = 0i32;

                        if FLOAT_MAX_SHIFTED_ONES != 0 && shift_count > FLOAT_MAX_SHIFTED_ONES {
                            num_zeros = shift_count - FLOAT_MAX_SHIFTED_ONES;
                        }
                        if FLOAT_MIN_SHIFTED_ZEROS > num_zeros {
                            num_zeros = if FLOAT_MIN_SHIFTED_ZEROS > shift_count {
                                shift_count
                            }
                            else {
                                FLOAT_MIN_SHIFTED_ZEROS
                            };
                        }

                        let read_bits = shift_count - num_zeros;
                        if read_bits > 0 {
                            let temp = bs.getbits(read_bits as u32);
                            mag |= (temp << num_zeros) & mask;
                        }
                    }
                }

                outval = (outval & !0x007f_ffff) | (mag & 0x007f_ffff);
                outval = (outval & !0x7f80_0000) | (((exp as u32) & 0xff) << 23);
            }
        }

        *v = outval as i32;
    }
}

/// Port of `float_values_nowvx()` in unpack_floats.c (no correction/extension bits
/// available: used whenever the block carries no `ID_WVX_BITSTREAM` sub-block, which
/// happens for hybrid-lossy float streams and for streams encoded with `-x0`/skip-wvx).
fn float_values_nowvx(values: &mut [i32], info: &FloatInfo) {
    for v in values.iter_mut() {
        let mut shift_count: i32 = 0;
        let mut exp = info.max_exp as i32;
        let mut outval: u32 = 0;

        if *v != 0 {
            let mut mag = (*v as u32).wrapping_shl((info.shift & 0x1f) as u32);

            if (mag as i32) < 0 {
                mag = mag.wrapping_neg();
                outval |= 1 << 31;
            }

            if mag >= 0x0100_0000 {
                while mag & 0x0f00_0000 != 0 {
                    mag >>= 1;
                    exp += 1;
                }
            }
            else if exp != 0 {
                loop {
                    if mag & 0x0080_0000 != 0 {
                        break;
                    }
                    exp -= 1;
                    if exp == 0 {
                        break;
                    }
                    shift_count += 1;
                    mag <<= 1;
                }

                shift_count &= 0x1f;

                if shift_count != 0 && (info.flags & FLOAT_SHIFT_ONES) != 0 {
                    mag |= (1u32 << shift_count) - 1;
                }
            }

            outval = (outval & 0x8000_0000) | (mag & 0x007f_ffff) | (((exp as u32) & 0xff) << 23);
        }

        *v = outval as i32;
    }
}
