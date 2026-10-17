// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Nintendo GameCube AFC ADPCM.
//!
//! Matches FFmpeg's `adpcm_afc` decoder (`ADPCM_AFC` in `libavcodec/adpcm.c`).
//!
//! A frame of one channel is 9 bytes and holds 16 samples: a byte with the scale (`1 << high
//! nibble`) and the index of a pair of filter coefficients (low nibble), then 16 signed 4-bit
//! residuals, the upper nibble of a byte first. The two preceding output samples of the channel
//! are predicted with the coefficients:
//!
//! ```text
//! out[n] = clip16(((c0 * out[n-1] + c1 * out[n-2]) >> 11) + residual * scale)
//! ```
//!
//! The history carries over from frame to frame.
//!
//! A packet is made of blocks of the channels. A block holds the same number of consecutive frames
//! of each channel, one channel after the other. The number of samples per channel of a block is
//! the codec parameter `frames_per_block` (a multiple of 16; FFmpeg's extra data byte is 16 for
//! the AFC files, a frame per channel); without it the whole packet is one block. The frames of a
//! trailing partial block are dropped.

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::util::clamp::clamp_i16;

use crate::common::from_i16_shift;

/// The largest number of channels (`ff_adpcm_decode_init`, `libavcodec/adpcm.c`).
pub(crate) const MAX_CHANNELS: usize = 6;

/// The size in bytes of the frame of one channel.
const FRAME_BYTES: usize = 9;

/// The number of samples in the frame of one channel.
const FRAME_SAMPLES: usize = 16;

/// The filter coefficients (`afc_coeffs`, `libavcodec/adpcm.c`).
const COEFFS: [[i32; 16]; 2] = [
    [0, 2048, 0, 1024, 4096, 3584, 3072, 4608, 4200, 4800, 5120, 2048, 1024, -1024, -1024, -2048],
    [0, 0, 2048, 1024, -2048, -1536, -1024, -2560, -2248, -2300, -3072, -2048, -1024, 1024, 0, 0],
];

pub(crate) struct AfcDecoder {
    channels: usize,
    /// The number of frames per channel in a block, or none if a packet is a block.
    block_frames: Option<usize>,
    /// The last two samples of each channel.
    state: [[i32; 2]; MAX_CHANNELS],
}

impl AfcDecoder {
    pub(crate) fn try_new(channels: usize, frames_per_block: Option<u64>) -> Result<Self> {
        if channels == 0 || channels > MAX_CHANNELS {
            return unsupported_error("adpcm (afc): unsupported number of channels");
        }

        let block_frames = match frames_per_block {
            None => None,
            Some(samples) if samples == 0 || samples % FRAME_SAMPLES as u64 != 0 => {
                return unsupported_error("adpcm (afc): frames per block must be a multiple of 16");
            }
            Some(samples) => Some(
                usize::try_from(samples / FRAME_SAMPLES as u64)
                    .or_else(|_| unsupported_error("adpcm (afc): frames per block is too large"))?,
            ),
        };

        Ok(AfcDecoder { channels, block_frames, state: [[0; 2]; MAX_CHANNELS] })
    }

    pub(crate) fn reset(&mut self) {
        self.state = [[0; 2]; MAX_CHANNELS];
    }

    /// The number of frames per channel in the blocks of a packet of `len` bytes.
    fn block_frames(&self, len: usize) -> usize {
        self.block_frames.unwrap_or(len / (FRAME_BYTES * self.channels))
    }

    /// The number of frames per channel in a packet of `len` bytes.
    fn packet_frames(&self, len: usize) -> usize {
        let frames = len / (FRAME_BYTES * self.channels);
        match self.block_frames(len) {
            0 => 0,
            block => frames / block * block,
        }
    }

    /// Prepares to decode a packet. Returns the number of frames in the packet.
    pub(crate) fn begin(&self, data: &[u8]) -> Result<usize> {
        let frames = self.packet_frames(data.len());

        if frames == 0 {
            return decode_error("adpcm (afc): invalid packet size");
        }

        Ok(frames * FRAME_SAMPLES)
    }

    /// Decodes the channel `ch` of the packet `data` into `out`, which must have been sized from
    /// the result of [`AfcDecoder::begin`].
    pub(crate) fn decode_channel(&mut self, data: &[u8], ch: usize, out: &mut [i32]) {
        let block_frames = self.block_frames(data.len());
        let block_bytes = block_frames * FRAME_BYTES * self.channels;
        let used = self.packet_frames(data.len()) * FRAME_BYTES * self.channels;
        let [mut s1, mut s2] = self.state[ch];

        let mut out = out.chunks_exact_mut(FRAME_SAMPLES);

        for block in data[..used].chunks_exact(block_bytes) {
            // The channels of a block are consecutive.
            let chunk_len = block_frames * FRAME_BYTES;
            let chunk = &block[ch * chunk_len..][..chunk_len];

            for (frame, out) in chunk.chunks_exact(FRAME_BYTES).zip(&mut out) {
                let scale = 1i32 << (frame[0] >> 4);
                let index = usize::from(frame[0] & 0x0f);
                let [c0, c1] = [COEFFS[0][index], COEFFS[1][index]];

                for (n, out) in out.iter_mut().enumerate() {
                    let byte = frame[1 + n / 2];
                    let nibble = if n & 1 == 0 { byte >> 4 } else { byte & 0x0f };
                    // Sign extend the 4-bit residual.
                    let d = i32::from((nibble << 4) as i8 >> 4);

                    let s0 = i32::from(clamp_i16(((s1 * c0 + s2 * c1) >> 11) + d * scale));
                    *out = from_i16_shift!(s0);
                    s2 = s1;
                    s1 = s0;
                }
            }
        }

        self.state[ch] = [s1, s2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_configuration() {
        assert!(AfcDecoder::try_new(0, None).is_err());
        assert!(AfcDecoder::try_new(7, None).is_err());
        assert!(AfcDecoder::try_new(2, Some(8)).is_err());
        assert!(AfcDecoder::try_new(2, Some(0)).is_err());
        assert!(AfcDecoder::try_new(2, Some(16)).is_ok());
    }

    #[test]
    fn packet_sizes() {
        let dec = AfcDecoder::try_new(2, Some(16)).unwrap();
        assert!(dec.begin(&[0; 17]).is_err());
        assert_eq!(dec.begin(&[0; 18]).unwrap(), 16);
        assert_eq!(dec.begin(&[0; 37]).unwrap(), 32);

        // Without a block size a packet holds consecutive frames of each channel.
        let dec = AfcDecoder::try_new(2, None).unwrap();
        assert_eq!(dec.begin(&[0; 54]).unwrap(), 48);
        assert!(dec.begin(&[]).is_err());
    }

    #[test]
    fn channels_are_consecutive_in_a_block() {
        let mut dec = AfcDecoder::try_new(2, None).unwrap();
        // One frame per channel: the right channel has a residual of 1 in its first sample.
        let mut data = [0u8; 18];
        data[9] = 0x00;
        data[10] = 0x10;
        assert_eq!(dec.begin(&data).unwrap(), 16);
        let mut left = [0i32; 16];
        let mut right = [0i32; 16];
        dec.decode_channel(&data, 0, &mut left);
        dec.decode_channel(&data, 1, &mut right);
        assert!(left.iter().all(|&s| s == 0));
        assert_eq!(right[0], 1 << 16);
    }
}
