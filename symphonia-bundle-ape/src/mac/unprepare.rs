// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

//! Channel decorrelation (mid-side reversal) and PCM byte formatting for the Monkey's Audio
//! decoder.
//!
//! After the predictor produces decoded sample values (i32 per channel), this module reverses
//! mid-side encoding and writes little-endian PCM bytes. The values come in as one slice per
//! channel and go out as interleaved PCM bytes.
//!
//! Reference: `Prepare.cpp` -- `CPrepare::Unprepare`.

use crate::mac::error::{ApeError, ApeResult};

/// Reverse the mid-side encoding of a stereo sample: `x` and `y` in, left and right out.
#[inline(always)]
fn mid_side_wrapping(x: i32, y: i32) -> (i32, i32) {
    let first = x.wrapping_sub(y / 2);
    let second = first.wrapping_add(y);
    (first, second)
}

/// 24-bit special negative encoding for stereo/mono:
/// if value < 0: temp = (value + 0x800000) as u32 | 0x800000
/// else: temp = value as u32
/// Then write 3 bytes LE.
#[inline(always)]
fn write_24bit_special(value: i32, out: &mut [u8]) {
    let temp: u32 = if value < 0 { ((value + 0x80_0000) as u32) | 0x80_0000 } else { value as u32 };
    out[0] = temp as u8;
    out[1] = (temp >> 8) as u8;
    out[2] = (temp >> 16) as u8;
}

/// 24-bit simple encoding for multichannel: straight u32 cast, extract low 3 bytes.
#[inline(always)]
fn write_24bit_simple(value: i32, out: &mut [u8]) {
    let temp = value as u32;
    out[0] = temp as u8;
    out[1] = (temp >> 8) as u8;
    out[2] = (temp >> 16) as u8;
}

/// Unprepare a block of stereo samples (`x[i]`, `y[i]`) into interleaved PCM bytes.
///
/// `out` must hold exactly `x.len() * 2 * bits_per_sample / 8` bytes.
pub fn unprepare_stereo(
    bits_per_sample: u16,
    x: &[i32],
    y: &[i32],
    out: &mut [u8],
) -> ApeResult<()> {
    let n = x.len().min(y.len());
    let (x, y) = (&x[..n], &y[..n]);
    match bits_per_sample {
        32 => {
            // Mid-side decorrelation, write i32 LE. Wrapping to match C++ semantics.
            for ((o, &x), &y) in out.chunks_exact_mut(8).zip(x).zip(y) {
                let (first, second) = mid_side_wrapping(x, y);
                o[..4].copy_from_slice(&first.to_le_bytes());
                o[4..].copy_from_slice(&second.to_le_bytes());
            }
            Ok(())
        }
        16 => {
            // Overflow validation: 16-bit ONLY
            let mut overflow = false;
            for ((o, &x), &y) in out.chunks_exact_mut(4).zip(x).zip(y) {
                let (first, second) = mid_side_wrapping(x, y);
                overflow |= (first as i16 as i32 != first) | (second as i16 as i32 != second);
                o[..2].copy_from_slice(&(first as i16).to_le_bytes());
                o[2..].copy_from_slice(&(second as i16).to_le_bytes());
            }
            if overflow { Err(ApeError::DecodingError("16-bit sample overflow")) } else { Ok(()) }
        }
        8 => {
            // Mid-side with +128 offset, wrapping arithmetic. The +128 bias is integrated into
            // the mid-side formula.
            for ((o, &x), &y) in out.chunks_exact_mut(2).zip(x).zip(y) {
                let first: u8 = x.wrapping_sub(y / 2).wrapping_add(128) as u8;
                let second: u8 = i32::from(first).wrapping_add(y) as u8;
                o[0] = first;
                o[1] = second;
            }
            Ok(())
        }
        24 => {
            for ((o, &x), &y) in out.chunks_exact_mut(6).zip(x).zip(y) {
                let (first, second) = mid_side_wrapping(x, y);
                write_24bit_special(first, &mut o[..3]);
                write_24bit_special(second, &mut o[3..]);
            }
            Ok(())
        }
        _ => Err(ApeError::DecodingError("unsupported bit depth")),
    }
}

/// Unprepare a block of mono samples into PCM bytes.
pub fn unprepare_mono(bits_per_sample: u16, x: &[i32], out: &mut [u8]) -> ApeResult<()> {
    match bits_per_sample {
        32 => {
            for (o, &val) in out.chunks_exact_mut(4).zip(x) {
                o.copy_from_slice(&val.to_le_bytes());
            }
            Ok(())
        }
        16 => {
            for (o, &val) in out.chunks_exact_mut(2).zip(x) {
                o.copy_from_slice(&(val as i16).to_le_bytes());
            }
            Ok(())
        }
        8 => {
            for (o, &val) in out.iter_mut().zip(x) {
                *o = val.wrapping_add(128) as u8;
            }
            Ok(())
        }
        24 => {
            for (o, &val) in out.chunks_exact_mut(3).zip(x) {
                write_24bit_special(val, o);
            }
            Ok(())
        }
        _ => Err(ApeError::DecodingError("unsupported bit depth")),
    }
}

/// Unprepare a block of `n` samples of more than two channels, from one slice of values per
/// channel (at least `n` values each) into interleaved PCM bytes.
pub fn unprepare_multichannel(
    bits_per_sample: u16,
    channels: &[Vec<i32>],
    n: usize,
    out: &mut [u8],
) -> ApeResult<()> {
    let nc = channels.len();
    let bytes_per_sample = usize::from(bits_per_sample / 8);
    let mut input = vec![0i32; nc];
    let mut samples = vec![0i32; nc];

    match bits_per_sample {
        16 | 24 | 8 => (),
        _ => return Err(ApeError::DecodingError("unsupported bit depth")),
    }

    for (block, o) in out.chunks_exact_mut(nc * bytes_per_sample).take(n).enumerate() {
        for (v, c) in input.iter_mut().zip(channels) {
            *v = c[block];
        }

        match bits_per_sample {
            16 => {
                apply_multichannel_decorrelation(&input, &mut samples);

                // Write 16-bit LE with overflow check for mid-side pairs
                for (o, &val) in o.chunks_exact_mut(2).zip(&samples) {
                    if !(-32768..=32767).contains(&val) {
                        return Err(ApeError::DecodingError("16-bit sample overflow"));
                    }
                    o.copy_from_slice(&(val as i16).to_le_bytes());
                }
            }
            24 => {
                apply_multichannel_decorrelation(&input, &mut samples);

                // Multichannel 24-bit uses simple encoding (u32 cast, not special negative)
                for (o, &val) in o.chunks_exact_mut(3).zip(&samples) {
                    write_24bit_simple(val, o);
                }
            }
            _ => {
                // Multichannel 8-bit: passthrough all channels, value + 128.
                for (o, &val) in o.iter_mut().zip(&input) {
                    *o = val.wrapping_add(128) as u8;
                }
            }
        }
    }
    Ok(())
}

/// Apply multichannel decorrelation pattern based on channel count.
///
/// * 4 channels: two mid-side pairs (0,1) and (2,3).
/// * 6+ channels: (0,1) mid-side, (2,3) passthrough, (4,5) mid-side, (6,7) mid-side if 8+
///   channels, remaining passthrough.
/// * 3 or 5 channels: all passthrough.
fn apply_multichannel_decorrelation(input: &[i32], out: &mut [i32]) {
    let nc = input.len();

    match nc {
        4 => {
            let (f0, s0) = mid_side_wrapping(input[0], input[1]);
            let (f1, s1) = mid_side_wrapping(input[2], input[3]);
            out[0] = f0;
            out[1] = s0;
            out[2] = f1;
            out[3] = s1;
        }
        n if n >= 6 => {
            // Channels 0,1: mid-side
            let (f0, s0) = mid_side_wrapping(input[0], input[1]);
            out[0] = f0;
            out[1] = s0;

            // Channels 2,3: passthrough
            out[2] = input[2];
            out[3] = input[3];

            // Channels 4,5: mid-side
            let (f2, s2) = mid_side_wrapping(input[4], input[5]);
            out[4] = f2;
            out[5] = s2;

            if n >= 8 {
                // Channels 6,7: mid-side (rear pair)
                let (f3, s3) = mid_side_wrapping(input[6], input[7]);
                out[6] = f3;
                out[7] = s3;

                // Remaining channels (8+): passthrough
                out[8..n].copy_from_slice(&input[8..n]);
            }
            else {
                // 6 or 7 channels: remaining passthrough. For 7 channels, channel 6 is not
                // written by any mid-side block (matching SDK behavior): it stays 0 (`out` is
                // zero on entry for that channel, and is never written to by anything else).
                let start = if n == 7 { 7 } else { 6 };
                out[start..n].copy_from_slice(&input[start..n]);
            }
        }
        _ => {
            // 3 or 5 channels: passthrough all
            out[..nc].copy_from_slice(&input[..nc]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_16bit_basic() {
        // X=100, Y=20 -> first = 100 - 10 = 90, second = 90 + 20 = 110
        let mut out = [0u8; 4];
        unprepare_stereo(16, &[100], &[20], &mut out).expect("valid samples");
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), 90);
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), 110);
    }

    #[test]
    fn stereo_16bit_overflow() {
        // Values that exceed i16 range after mid-side
        let mut out = [0u8; 4];
        assert!(unprepare_stereo(16, &[40000], &[0], &mut out).is_err());
    }

    #[test]
    fn mono_8bit() {
        let mut out = [0u8; 3];
        unprepare_mono(8, &[0, -128, 127], &mut out).expect("valid samples");
        assert_eq!(out, [128u8, 0u8, 255u8]);
    }

    #[test]
    fn stereo_8bit_wrapping() {
        // X=0, Y=0 -> first = (0 - 0 + 128) as u8 = 128, second = (128 + 0) as u8 = 128
        let mut out = [0u8; 2];
        unprepare_stereo(8, &[0], &[0], &mut out).expect("valid samples");
        assert_eq!(out, [128u8, 128u8]);
    }

    #[test]
    fn stereo_32bit() {
        let mut out = [0u8; 8];
        unprepare_stereo(32, &[1000], &[200], &mut out).expect("valid samples");
        assert_eq!(i32::from_le_bytes([out[0], out[1], out[2], out[3]]), 1000 - 100);
        assert_eq!(i32::from_le_bytes([out[4], out[5], out[6], out[7]]), 900 + 200);
    }

    #[test]
    fn mono_24bit_negative() {
        let mut out = [0u8; 3];
        unprepare_mono(24, &[-1], &mut out).expect("valid samples");
        // -1 + 0x800000 = 0x7FFFFF, | 0x800000 = 0xFFFFFF
        assert_eq!(out, [0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn multichannel_seven_channels_leaves_channel_six_silent() {
        let channels: Vec<Vec<i32>> = (1..=7).map(|v| vec![v]).collect();
        let mut out = [0u8; 14];
        unprepare_multichannel(16, &channels, 1, &mut out).expect("valid samples");
        let samples: Vec<i16> =
            out.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        // (1, 2) -> (1 - 1, 0 + 2), (3, 4) passthrough, (5, 6) -> (5 - 3, 2 + 6), 7th is 0.
        assert_eq!(samples, [0, 2, 3, 4, 2, 8, 0]);
    }
}
