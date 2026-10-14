// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::AudioMut;
use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::ReadBytes;

use crate::common::{from_i16_shift, u16_to_i32};
use crate::common_ima::AdpcmImaBlockStatus;

fn read_preamble<B: ReadBytes>(stream: &mut B) -> Result<AdpcmImaBlockStatus> {
    let predictor = u16_to_i32!(stream.read_u16()?);
    let step_index = stream.read_byte()? as i32;
    if step_index > 88 {
        return decode_error("adpcm (ima): invalid step index");
    }
    //reserved byte
    let _ = stream.read_byte()?;
    let status = AdpcmImaBlockStatus { predictor, step_index };
    Ok(status)
}

/// The maximum number of channels supported by [`decode_block`].
pub(crate) const MAX_CHANNELS: usize = 8;

/// For each supported bits per sample (2 to 5): the number of bytes per channel in one chunk of
/// the interleaved sample data, and the number of samples that chunk holds.
const CHUNKS: [(usize, usize); 4] = [(4, 16), (12, 32), (4, 8), (20, 32)];

/// Decode one block of IMA ADPCM WAV with 1 to [`MAX_CHANNELS`] channels and 2 to 5 bits per
/// sample.
///
/// The block is `channels` 4-byte headers (initial sample, step index, reserved), followed by
/// the sample codes. The codes of each channel are a continuous LSB-first bit stream that is
/// stored in 4-byte words, with the words of the channels interleaved. Any trailing bytes in the
/// block that do not form a whole chunk (see [`CHUNKS`]) are skipped; the block is `block_bytes`
/// long in total.
pub(crate) fn decode_block<B: ReadBytes, A: AudioMut<i32>>(
    stream: &mut B,
    buf: &mut A,
    channels: usize,
    bits_per_sample: u32,
    frame_offset: usize,
    frames_per_block: usize,
    block_bytes: usize,
) -> Result<()> {
    if channels == 0 || channels > MAX_CHANNELS {
        return decode_error("adpcm (ima): unsupported channel count");
    }
    if !(2..=5).contains(&bits_per_sample) {
        return decode_error("adpcm (ima): unsupported bits per sample");
    }
    let (chunk_bytes, chunk_samples) = CHUNKS[bits_per_sample as usize - 2];
    if frames_per_block == 0 || (frames_per_block - 1) % chunk_samples != 0 {
        return decode_error("adpcm (ima): frames per block is not a whole number of chunks");
    }
    let chunks = (frames_per_block - 1) / chunk_samples;
    let used_bytes = 4 * channels + chunks * chunk_bytes * channels;
    if used_bytes > block_bytes {
        return decode_error("adpcm (ima): block is too small for frames per block");
    }

    let mut status: [AdpcmImaBlockStatus; MAX_CHANNELS] =
        std::array::from_fn(|_| AdpcmImaBlockStatus { predictor: 0, step_index: 0 });

    for (ch, status) in status.iter_mut().enumerate().take(channels) {
        *status = read_preamble(stream)?;
        match buf.plane_mut(ch) {
            Some(plane) => plane[frame_offset] = from_i16_shift!(status.predictor),
            None => return decode_error("adpcm (ima): missing audio plane"),
        }
    }

    // One chunk of one channel, with a spare byte so a 16-bit window can always be read.
    let mask = (1u16 << bits_per_sample) - 1;
    let mut chunk = [[0u8; 21]; MAX_CHANNELS];

    for chunk_id in 0..chunks {
        for word in 0..chunk_bytes / 4 {
            for channel_chunk in chunk.iter_mut().take(channels) {
                stream.read_buf_exact(&mut channel_chunk[word * 4..word * 4 + 4])?;
            }
        }

        let first = frame_offset + 1 + chunk_id * chunk_samples;
        for (ch, (status, bits)) in status.iter_mut().zip(chunk.iter()).enumerate().take(channels) {
            let plane = match buf.plane_mut(ch) {
                Some(plane) => plane,
                None => return decode_error("adpcm (ima): missing audio plane"),
            };
            let mut bit = 0usize;
            for out in &mut plane[first..first + chunk_samples] {
                let window = u16::from(bits[bit >> 3]) | (u16::from(bits[(bit >> 3) + 1]) << 8);
                let code = ((window >> (bit & 7)) & mask) as u8;
                *out = status.expand_code(code, bits_per_sample);
                bit += bits_per_sample as usize;
            }
        }
    }

    stream.ignore_bytes((block_bytes - used_bytes) as u64)?;
    Ok(())
}
