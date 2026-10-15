// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Block-based IMA ADPCM variants of `WAVE` with their own block layouts: Duck/On2 DK3
//! (`WAVE_FORMAT` 0x0062), Duck/On2 DK4 (0x0061) and Xbox (0x0069).
//!
//! These match FFmpeg's `adpcm_ima_dk3`, `adpcm_ima_dk4` and `adpcm_ima_xbox` decoders
//! (`libavcodec/adpcm.c`). All three expand codes with the multiply form of the IMA step, see
//! [`AdpcmImaBlockStatus::expand_direct`]. Every block starts with fresh predictors, so no state
//! is carried between blocks.

use symphonia_core::audio::AudioMut;
use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::ReadBytes;

use crate::common::{from_i16_shift, u16_to_i32};
use crate::common_ima::AdpcmImaBlockStatus;

/// Reads a 16-bit little-endian predictor and a 16-bit step index. FFmpeg rejects a step index
/// that is negative or above 88.
fn read_predictor_and_index<B: ReadBytes>(stream: &mut B) -> Result<AdpcmImaBlockStatus> {
    let predictor = u16_to_i32!(stream.read_u16()?);
    let step_index = u16_to_i32!(stream.read_u16()?);
    if !(0..=88).contains(&step_index) {
        return decode_error("adpcm (ima): invalid step index");
    }
    Ok(AdpcmImaBlockStatus { predictor, step_index })
}

/// Decodes one DK4 block of 1 or 2 channels.
///
/// The block is, per channel, a 16-bit predictor (also the first sample) and a 16-bit step index,
/// followed by one byte per code pair. Mono bytes hold two consecutive samples (the high nibble
/// first). Stereo bytes hold one frame: the high nibble is the left and the low nibble the right
/// channel. `frames_per_block` is `1 + 2 * data_bytes / channels`.
pub(crate) fn decode_dk4<B: ReadBytes, A: AudioMut<i32>>(
    stream: &mut B,
    buf: &mut A,
    channels: usize,
    frame_offset: usize,
    frames_per_block: usize,
    block_bytes: usize,
) -> Result<()> {
    let coded_frames = frames_per_block.saturating_sub(1);
    let data_bytes = coded_frames * channels / 2;
    if channels == 0 || channels > 2 || (channels == 1 && coded_frames % 2 != 0) {
        return decode_error("adpcm (ima_dk4): invalid frames per block");
    }
    let used_bytes = 4 * channels + data_bytes;
    if used_bytes > block_bytes {
        return decode_error("adpcm (ima_dk4): block is too small for frames per block");
    }

    let mut status =
        [read_predictor_and_index(stream)?, AdpcmImaBlockStatus { predictor: 0, step_index: 0 }];
    if channels == 2 {
        // The headers of the channels are consecutive; the first sample of each is its predictor.
        status[1] = read_predictor_and_index(stream)?;
    }

    let first = frame_offset + 1;
    if channels == 1 {
        let Some(plane) = buf.plane_mut(0)
        else {
            return decode_error("adpcm (ima_dk4): missing audio plane");
        };
        plane[frame_offset] = from_i16_shift!(status[0].predictor);
        for out in plane[first..first + coded_frames].chunks_exact_mut(2) {
            let byte = stream.read_u8()?;
            out[0] = status[0].expand_direct(byte >> 4);
            out[1] = status[0].expand_direct(byte & 0x0f);
        }
    }
    else {
        let Some((left, right)) = buf.plane_pair_mut(0, 1)
        else {
            return decode_error("adpcm (ima_dk4): missing audio planes");
        };
        left[frame_offset] = from_i16_shift!(status[0].predictor);
        right[frame_offset] = from_i16_shift!(status[1].predictor);
        let range = first..first + coded_frames;
        for (l, r) in left[range.clone()].iter_mut().zip(right[range].iter_mut()) {
            let byte = stream.read_u8()?;
            *l = status[0].expand_direct(byte >> 4);
            *r = status[1].expand_direct(byte & 0x0f);
        }
    }

    stream.ignore_bytes((block_bytes - used_bytes) as u64)?;
    Ok(())
}

/// Decodes one DK3 block (stereo only).
///
/// The block is a 16-byte header (10 bytes skipped, the 16-bit predictors of the sum and the
/// difference channel, then their 8-bit step indices), followed by a stream of 4-bit codes, low
/// nibble first. Every 3 codes decode to two frames: sum, difference, [frame], sum, [frame]. A
/// frame is `sum + difference` (left) and `sum - difference` (right), truncated to 16 bits as in
/// FFmpeg. `frames_per_block` must be even.
pub(crate) fn decode_dk3<B: ReadBytes, A: AudioMut<i32>>(
    stream: &mut B,
    buf: &mut A,
    frame_offset: usize,
    frames_per_block: usize,
    block_bytes: usize,
) -> Result<()> {
    if frames_per_block % 2 != 0 {
        return decode_error("adpcm (ima_dk3): frames per block must be even");
    }
    // Each pair of frames is 3 codes.
    let groups = frames_per_block / 2;
    let code_bytes = (3 * groups).div_ceil(2);
    let used_bytes = 16 + code_bytes;
    if used_bytes > block_bytes {
        return decode_error("adpcm (ima_dk3): block is too small for frames per block");
    }

    stream.ignore_bytes(10)?;
    let sum_predictor = u16_to_i32!(stream.read_u16()?);
    let diff_predictor = u16_to_i32!(stream.read_u16()?);
    let sum_step_index = i32::from(stream.read_u8()?);
    let diff_step_index = i32::from(stream.read_u8()?);
    if sum_step_index > 88 || diff_step_index > 88 {
        return decode_error("adpcm (ima_dk3): invalid step index");
    }
    let mut sum = AdpcmImaBlockStatus { predictor: sum_predictor, step_index: sum_step_index };
    let mut diff = AdpcmImaBlockStatus { predictor: diff_predictor, step_index: diff_step_index };

    let Some((left, right)) = buf.plane_pair_mut(0, 1)
    else {
        return decode_error("adpcm (ima_dk3): missing audio planes");
    };
    let range = frame_offset..frame_offset + frames_per_block;

    // Codes are consumed 3 at a time: the low nibble of a byte before its high nibble.
    let mut pending: Option<u8> = None;
    let mut next_code = |stream: &mut B| -> Result<u8> {
        match pending.take() {
            Some(code) => Ok(code),
            None => {
                let byte = stream.read_u8()?;
                pending = Some(byte >> 4);
                Ok(byte & 0x0f)
            }
        }
    };

    for (l, r) in left[range.clone()].chunks_exact_mut(2).zip(right[range].chunks_exact_mut(2)) {
        sum.expand_direct(next_code(stream)?);
        diff.expand_direct(next_code(stream)?);
        l[0] = frame_sample(sum.predictor + diff.predictor);
        r[0] = frame_sample(sum.predictor - diff.predictor);

        sum.expand_direct(next_code(stream)?);
        l[1] = frame_sample(sum.predictor + diff.predictor);
        r[1] = frame_sample(sum.predictor - diff.predictor);
    }

    stream.ignore_bytes((block_bytes - used_bytes) as u64)?;
    Ok(())
}

/// Truncates a sum or difference of two predictors to a 16-bit sample, as FFmpeg's store to an
/// `int16_t` does.
fn frame_sample(value: i32) -> i32 {
    from_i16_shift!(value as i16)
}

/// Decodes one Xbox IMA block of 1 or 2 channels.
///
/// The block is, per channel, a 16-bit predictor (also the first sample) and a 16-bit step
/// index, followed by groups of 4 bytes (8 codes, low nibble first) per channel with the
/// channels interleaved group-wise. FFmpeg decodes `frames_per_block` samples after the first and
/// drops the last one, so a block of `frames_per_block` frames is the predictor followed by the
/// first `frames_per_block - 1` decoded samples. `frames_per_block` must be a multiple of 8.
pub(crate) fn decode_xbox<B: ReadBytes, A: AudioMut<i32>>(
    stream: &mut B,
    buf: &mut A,
    channels: usize,
    frame_offset: usize,
    frames_per_block: usize,
    block_bytes: usize,
) -> Result<()> {
    if channels == 0 || channels > 2 || frames_per_block % 8 != 0 || frames_per_block == 0 {
        return decode_error("adpcm (ima_xbox): invalid frames per block");
    }
    let groups = frames_per_block / 8;
    let used_bytes = 4 * channels + 4 * groups * channels;
    if used_bytes > block_bytes {
        return decode_error("adpcm (ima_xbox): block is too small for frames per block");
    }

    let mut status =
        [read_predictor_and_index(stream)?, AdpcmImaBlockStatus { predictor: 0, step_index: 0 }];
    if channels == 2 {
        status[1] = read_predictor_and_index(stream)?;
    }

    // Decode into the frames of the block; the code that would be frame `frames_per_block` is
    // decoded for the state only.
    let mut planes: [Option<&mut [i32]>; 2] = [None, None];
    if channels == 1 {
        planes[0] = buf.plane_mut(0);
    }
    else if let Some((left, right)) = buf.plane_pair_mut(0, 1) {
        planes = [Some(left), Some(right)];
    }

    for ch in 0..channels {
        let Some(plane) = planes[ch].as_deref_mut()
        else {
            return decode_error("adpcm (ima_xbox): missing audio plane");
        };
        plane[frame_offset] = from_i16_shift!(status[ch].predictor);
    }

    let mut word = [0u8; 4];
    for group in 0..groups {
        for ch in 0..channels {
            stream.read_buf_exact(&mut word)?;
            let Some(plane) = planes[ch].as_deref_mut()
            else {
                return decode_error("adpcm (ima_xbox): missing audio plane");
            };
            for (i, &byte) in word.iter().enumerate() {
                for (half, code) in [byte & 0x0f, byte >> 4].into_iter().enumerate() {
                    let sample = status[ch].expand_direct(code);
                    let index = 1 + group * 8 + i * 2 + half;
                    if index < frames_per_block {
                        plane[frame_offset + index] = sample;
                    }
                }
            }
        }
    }

    stream.ignore_bytes((block_bytes - used_bytes) as u64)?;
    Ok(())
}
