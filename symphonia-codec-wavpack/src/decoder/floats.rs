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

/// Parsed contents of an `ID_FLOAT_INFO` sub-block (always exactly 4 bytes), plus the two
/// 5-bit fields that start an `ID_WVX_NEW_BITSTREAM` of float data.
#[derive(Default, Clone, Copy)]
pub struct FloatInfo {
    pub flags: u8,
    pub shift: u8,
    pub max_exp: u8,
    #[allow(dead_code)]
    pub norm_exp: u8,
    /// `float_min_shifted_zeros`: only set by `ID_WVX_NEW_BITSTREAM`, otherwise 0.
    pub min_shifted_zeros: i32,
    /// `float_max_shifted_ones`: only set by `ID_WVX_NEW_BITSTREAM`, otherwise 0.
    pub max_shifted_ones: i32,
}

pub fn parse_float_info(data: &[u8]) -> Option<FloatInfo> {
    if data.len() != 4 {
        return None;
    }
    Some(FloatInfo {
        flags: data[0],
        shift: data[1],
        max_exp: data[2],
        norm_exp: data[3],
        ..Default::default()
    })
}

/// Restore `values` (raw 24-bit-shifted integer magnitudes produced by the entropy
/// decoder + decorrelator) into IEEE-754 `f32` bit patterns in place.
///
/// `wvx` is the `ID_WVX_BITSTREAM` sub-block bit reader (the "extension" stream that
/// carries the bits needed for bit-exact float reconstruction), if present. It is embedded
/// in the same WavPack block as the main audio bitstream for lossless files, and in the
/// `.wvc` correction block for hybrid-lossless ones.
/// Mirrors `float_values()` / `float_values_nowvx()` in unpack_floats.c. With a `wvx`, the
/// CRC of the restored values (to compare against the one stored in the sub-block) is returned.
pub fn float_values(
    values: &mut [i32],
    info: &FloatInfo,
    wvx: Option<&mut Bits<'_>>,
) -> Option<u32> {
    match wvx {
        Some(bs) => Some(float_values_wvx(values, info, bs)),
        None => {
            float_values_nowvx(values, info);
            None
        }
    }
}

/// Port of `float_values()` in unpack_floats.c (the "has wvx bitstream" path).
fn float_values_wvx(values: &mut [i32], info: &FloatInfo, bs: &mut Bits<'_>) -> u32 {
    let min_shifted_zeros = info.min_shifted_zeros;
    let max_shifted_ones = info.max_shifted_ones;
    let mut crc: u32 = 0xffff_ffff;

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

                        if max_shifted_ones != 0 && shift_count > max_shifted_ones {
                            num_zeros = shift_count - max_shifted_ones;
                        }
                        if min_shifted_zeros > num_zeros {
                            num_zeros = if min_shifted_zeros > shift_count {
                                shift_count
                            }
                            else {
                                min_shifted_zeros
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

        crc = crc
            .wrapping_mul(27)
            .wrapping_add((outval & 0x007f_ffff).wrapping_mul(9))
            .wrapping_add(((outval >> 23) & 0xff).wrapping_mul(3))
            .wrapping_add(outval >> 31);

        *v = outval as i32;
    }

    crc
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
