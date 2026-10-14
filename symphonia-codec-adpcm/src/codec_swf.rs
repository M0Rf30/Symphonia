// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shockwave Flash (SWF / FLV sound format 1) ADPCM.
//!
//! Matches FFmpeg's `adpcm_swf` decoder (`libavcodec/adpcm.c`), which follows the SWF file format
//! specification ("ADPCM compression"): each packet begins with a 2-bit field giving the code
//! size minus 2, followed by blocks. A block holds, per channel, a 16-bit initial sample and a
//! 6-bit step index, then up to 4095 frames of `code size` bits per channel, channels
//! interleaved. All data is read MSB first. Packets are self-contained.

use symphonia_core::errors::Result;
use symphonia_core::util::clamp::clamp_i16;

use crate::common::from_i16_shift;
use crate::common_ima::IMA_STEP_TABLE;

/// Step index adjustments for 2 to 5 bit codes, indexed by the code without its sign bit
/// (`swf_index_tables`, FFmpeg).
const INDEX_TABLES: [&[i32]; 4] = [
    &[-1, 2],
    &[-1, -1, 2, 4],
    &[-1, -1, -1, -1, 2, 4, 6, 8],
    &[-1, -1, -1, -1, -1, -1, -1, -1, 1, 2, 4, 6, 8, 10, 13, 16],
];

/// The number of frames in one block following the initial sample.
const FRAMES_PER_BLOCK: usize = 4095;

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn read(&mut self, bits: usize) -> u32 {
        let mut value = 0;
        for _ in 0..bits {
            let byte = self.data[self.pos >> 3];
            value = (value << 1) | u32::from((byte >> (7 - (self.pos & 7))) & 1);
            self.pos += 1;
        }
        value
    }
}

fn code_bits(data: &[u8]) -> usize {
    usize::from(data[0] >> 6) + 2
}

/// Returns the number of frames a packet decodes to.
pub(crate) fn frames_in(data: &[u8], channels: usize) -> usize {
    if data.is_empty() {
        return 0;
    }
    let size = data.len() * 8;
    let bits = code_bits(data);
    let mut pos = 2;
    let mut frames = 0;
    while pos + 22 * channels <= size {
        pos += 22 * channels;
        frames += 1;
        let mut count = 0;
        while pos + bits * channels <= size && count < FRAMES_PER_BLOCK {
            pos += bits * channels;
            frames += 1;
            count += 1;
        }
    }
    frames
}

/// Decodes a packet into `planes` (one slice per channel, each [`frames_in`] long).
pub(crate) fn decode(data: &[u8], planes: &mut [&mut [i32]]) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let channels = planes.len();
    let size = data.len() * 8;
    let bits = code_bits(data);
    let table = INDEX_TABLES[bits - 2];
    let k0 = 1u32 << (bits - 2);
    let sign_mask = 1u32 << (bits - 1);

    let mut reader = BitReader { data, pos: 2 };
    let mut frame = 0;
    let mut predictor = [0i32; 2];
    let mut step_index = [0usize; 2];

    while reader.pos + 22 * channels <= size {
        for ch in 0..channels {
            predictor[ch] = i32::from(reader.read(16) as u16 as i16);
            step_index[ch] = reader.read(6) as usize;
            planes[ch][frame] = from_i16_shift!(predictor[ch]);
        }
        frame += 1;

        let mut count = 0;
        while reader.pos + bits * channels <= size && count < FRAMES_PER_BLOCK {
            for ch in 0..channels {
                let delta = reader.read(bits);
                let mut step = IMA_STEP_TABLE[step_index[ch]];
                // vpdiff = (delta + 0.5) * step / 4, as a sum of successively halved steps.
                let mut vpdiff = 0;
                let mut k = k0;
                while k != 0 {
                    if delta & k != 0 {
                        vpdiff += step;
                    }
                    step >>= 1;
                    k >>= 1;
                }
                vpdiff += step;

                predictor[ch] = if delta & sign_mask != 0 {
                    predictor[ch] - vpdiff
                }
                else {
                    predictor[ch] + vpdiff
                };
                step_index[ch] = (step_index[ch] as i32 + table[(delta & !sign_mask) as usize])
                    .clamp(0, 88) as usize;
                predictor[ch] = i32::from(clamp_i16(predictor[ch]));
                planes[ch][frame] = from_i16_shift!(predictor[ch]);
            }
            frame += 1;
            count += 1;
        }
    }
    Ok(())
}
