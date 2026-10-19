// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::AudioMut;
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::ReadBytes;

use crate::common::{Nibble, from_i16_shift, u16_to_i32};

#[rustfmt::skip]
const MS_ADAPTATION_TABLE: [i32; 16] = [
    230, 230, 230, 230, 307, 409, 512, 614,
    768, 614, 512, 409, 307, 230, 230, 230,
];

/// The standard coefficient set (Microsoft ADPCM, `wNumCoef` = 7).
pub(crate) const MS_STANDARD_COEFFS: [[i32; 2]; 7] =
    [[256, 0], [512, -256], [0, 0], [192, 64], [240, 0], [460, -208], [392, -232]];

/// Parses the coefficient set from the `WAVEFORMATEX` extension of a Microsoft ADPCM stream:
/// `wSamplesPerBlock: u16`, `wNumCoef: u16`, then `wNumCoef` pairs of `i16` (`iCoef1`,
/// `iCoef2`), all little-endian. Returns `None` if `extra_data` is not a complete extension, in
/// which case the standard set applies.
pub(crate) fn parse_coeffs(extra_data: &[u8]) -> Option<Vec<[i32; 2]>> {
    let num_coef = usize::from(u16::from_le_bytes([*extra_data.get(2)?, *extra_data.get(3)?]));
    let table = extra_data.get(4..4 + 4 * num_coef)?;
    if num_coef == 0 {
        return None;
    }
    Some(
        table
            .chunks_exact(4)
            .map(|c| {
                [
                    i32::from(i16::from_le_bytes([c[0], c[1]])),
                    i32::from(i16::from_le_bytes([c[2], c[3]])),
                ]
            })
            .collect(),
    )
}

const DELTA_MIN: i32 = 16;

/// The largest delta for which `delta * MS_ADAPTATION_TABLE[n]` (max 768) cannot overflow an i32.
/// Valid streams never come close to this; it only bounds corrupt streams. Matches FFmpeg.
const DELTA_MAX: i32 = i32::MAX / 768;

fn coeffs_for(coeffs: &[[i32; 2]], block_predictor: u8) -> Result<[i32; 2]> {
    match coeffs.get(usize::from(block_predictor)) {
        Some(c) => Ok(*c),
        None => unsupported_error("adpcm: block predictor exceeds range"),
    }
}

pub fn signed_nibble(nibble: u8) -> i8 {
    if (nibble & 0x08) != 0 { nibble as i8 - 0x10 } else { nibble as i8 }
}

/// `AdpcmMsBlockStatus` contains values to decode a block
struct AdpcmMsBlockStatus {
    coeff1: i32,
    coeff2: i32,
    delta: i32,
    sample1: i32,
    sample2: i32,
}

impl AdpcmMsBlockStatus {
    fn read_mono_preamble<B: ReadBytes>(stream: &mut B, coeffs: &[[i32; 2]]) -> Result<Self> {
        let [coeff1, coeff2] = coeffs_for(coeffs, stream.read_byte()?)?;
        let status = Self {
            coeff1,
            coeff2,
            delta: u16_to_i32!(stream.read_u16()?),
            sample1: u16_to_i32!(stream.read_u16()?),
            sample2: u16_to_i32!(stream.read_u16()?),
        };
        Ok(status)
    }

    fn read_stereo_preamble<B: ReadBytes>(
        stream: &mut B,
        coeffs: &[[i32; 2]],
    ) -> Result<(Self, Self)> {
        let [left_coeff1, left_coeff2] = coeffs_for(coeffs, stream.read_byte()?)?;
        let [right_coeff1, right_coeff2] = coeffs_for(coeffs, stream.read_byte()?)?;
        let left_delta = u16_to_i32!(stream.read_u16()?);
        let right_delta = u16_to_i32!(stream.read_u16()?);
        let left_sample1 = u16_to_i32!(stream.read_u16()?);
        let right_sample1 = u16_to_i32!(stream.read_u16()?);
        let left_sample2 = u16_to_i32!(stream.read_u16()?);
        let right_sample2 = u16_to_i32!(stream.read_u16()?);
        Ok((
            Self {
                coeff1: left_coeff1,
                coeff2: left_coeff2,
                delta: left_delta,
                sample1: left_sample1,
                sample2: left_sample2,
            },
            Self {
                coeff1: right_coeff1,
                coeff2: right_coeff2,
                delta: right_delta,
                sample1: right_sample1,
                sample2: right_sample2,
            },
        ))
    }

    fn expand_nibble(&mut self, byte: u8, nibble: Nibble) -> i32 {
        let nibble = nibble.get_nibble(byte);
        let signed_nibble = signed_nibble(nibble) as i32;
        // Computed in i64 so that extreme coefficients (-32768) cannot overflow. For in-range
        // inputs this is identical to FFmpeg's `adpcm_ms_expand_nibble` int arithmetic.
        let predictor = ((self.sample1 as i64 * self.coeff1 as i64)
            + (self.sample2 as i64 * self.coeff2 as i64))
            / 256
            + signed_nibble as i64 * self.delta as i64;
        self.sample2 = self.sample1;
        self.sample1 = predictor.clamp(i16::MIN as i64, i16::MAX as i64) as i32;
        self.delta = (MS_ADAPTATION_TABLE[nibble as usize] * self.delta) / 256;
        self.delta = self.delta.clamp(DELTA_MIN, DELTA_MAX);
        from_i16_shift!(self.sample1)
    }
}

pub(crate) fn decode_mono<B: ReadBytes>(
    stream: &mut B,
    buffer: &mut [i32],
    frames_per_block: usize,
    coeffs: &[[i32; 2]],
) -> Result<()> {
    let mut status = AdpcmMsBlockStatus::read_mono_preamble(stream, coeffs)?;
    buffer[0] = from_i16_shift!(status.sample2);
    buffer[1] = from_i16_shift!(status.sample1);
    for byte in 1..(frames_per_block / 2) {
        let nibbles = stream.read_u8()?;
        buffer[byte * 2] = status.expand_nibble(nibbles, Nibble::Upper);
        buffer[byte * 2 + 1] = status.expand_nibble(nibbles, Nibble::Lower);
    }
    Ok(())
}

#[allow(clippy::needless_range_loop)]
pub(crate) fn decode_stereo<B: ReadBytes>(
    stream: &mut B,
    buffers: [&mut [i32]; 2],
    frames_per_block: usize,
    coeffs: &[[i32; 2]],
) -> Result<()> {
    let (mut left_status, mut right_status) =
        AdpcmMsBlockStatus::read_stereo_preamble(stream, coeffs)?;
    buffers[0][0] = from_i16_shift!(left_status.sample2);
    buffers[0][1] = from_i16_shift!(left_status.sample1);
    buffers[1][0] = from_i16_shift!(right_status.sample2);
    buffers[1][1] = from_i16_shift!(right_status.sample1);
    // NOTE: This loop causes a false positive with clippy.
    for frame in 2..frames_per_block {
        let nibbles = stream.read_u8()?;
        buffers[0][frame] = left_status.expand_nibble(nibbles, Nibble::Upper);
        buffers[1][frame] = right_status.expand_nibble(nibbles, Nibble::Lower);
    }
    Ok(())
}

/// Decodes one block of 3 or more channels. Unlike mono and stereo blocks, FFmpeg lays the
/// channels out one after the other: each channel is a complete mono block (preamble, then the
/// codes of that channel), and channel `n + 1` starts where channel `n` ends.
pub(crate) fn decode_planar<B: ReadBytes, A: AudioMut<i32>>(
    stream: &mut B,
    buf: &mut A,
    channels: usize,
    frame_offset: usize,
    frames_per_block: usize,
    coeffs: &[[i32; 2]],
) -> Result<()> {
    if frames_per_block < 2 {
        return decode_error("adpcm (ms): frames per block must be at least 2");
    }
    for ch in 0..channels {
        let Some(plane) = buf.plane_mut(ch)
        else {
            return decode_error("adpcm (ms): missing audio plane");
        };
        let out = &mut plane[frame_offset..frame_offset + frames_per_block];
        let mut status = AdpcmMsBlockStatus::read_mono_preamble(stream, coeffs)?;
        out[0] = from_i16_shift!(status.sample2);
        out[1] = from_i16_shift!(status.sample1);
        for pair in out[2..].chunks_exact_mut(2) {
            let nibbles = stream.read_u8()?;
            pair[0] = status.expand_nibble(nibbles, Nibble::Upper);
            pair[1] = status.expand_nibble(nibbles, Nibble::Lower);
        }
    }
    Ok(())
}
