// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sony PlayStation ADPCM (VAG, SPU-ADPCM).
//!
//! Matches FFmpeg's `adpcm_psx` decoder (`ADPCM_PSX` in `libavcodec/adpcm.c`).
//!
//! A frame of one channel is 16 bytes and holds 28 samples: a filter/shift byte (the filter in the
//! upper and the shift in the lower nibble), a flag byte (only the three low bits are used, 7 marks
//! silence) and 14 bytes of 4-bit signed residuals, the lower nibble of a byte first. The two
//! preceding samples of the channel are predicted with one of the five filters of CD-ROM XA
//! ADPCM (`xa_adpcm_table`):
//!
//! ```text
//! out[n] = (residual << 12 >> shift) + (c0 * out[n-1] + c1 * out[n-2]) / 64
//! ```
//!
//! The history carries over from frame to frame, and it is not clipped to 16 bits as the output is.
//!
//! A packet is made of blocks of the channels. A block holds the same number of consecutive frames
//! of each channel, one channel after the other (a block of one frame per channel is the plain
//! frame interleave). The number of frames per channel of a block comes from the codec parameter
//! `frames_per_block` (a multiple of 28, 28 by default). The last block of a packet may be shorter
//! than a block; its channels then hold the same, shorter number of frames. FFmpeg decodes nothing
//! for such a block, whereas it is decoded here.

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::util::clamp::clamp_i16;

use crate::common::from_i16_shift;

/// The largest number of channels (`ff_adpcm_decode_init`, `libavcodec/adpcm.c`).
pub(crate) const MAX_CHANNELS: usize = 8;

/// The size in bytes of the frame of one channel.
const FRAME_BYTES: usize = 16;

/// The number of samples in the frame of one channel.
const FRAME_SAMPLES: usize = 28;

/// The filters (`xa_adpcm_table`, `libavcodec/adpcm.c`).
const FILTERS: [[i32; 2]; 5] = [[0, 0], [60, 0], [115, -52], [98, -55], [122, -60]];

pub(crate) struct PsxDecoder {
    channels: usize,
    /// The number of frames per channel in a block.
    block_frames: usize,
    /// The last two samples of each channel.
    state: [[i32; 2]; MAX_CHANNELS],
}

impl PsxDecoder {
    pub(crate) fn try_new(channels: usize, frames_per_block: Option<u64>) -> Result<Self> {
        if channels == 0 || channels > MAX_CHANNELS {
            return unsupported_error("adpcm (psx): unsupported number of channels");
        }

        let samples = frames_per_block.unwrap_or(FRAME_SAMPLES as u64);
        if samples == 0 || samples % FRAME_SAMPLES as u64 != 0 {
            return unsupported_error("adpcm (psx): frames per block must be a multiple of 28");
        }

        let block_frames = usize::try_from(samples / FRAME_SAMPLES as u64)
            .or_else(|_| unsupported_error("adpcm (psx): frames per block is too large"))?;

        Ok(PsxDecoder { channels, block_frames, state: [[0; 2]; MAX_CHANNELS] })
    }

    pub(crate) fn reset(&mut self) {
        self.state = [[0; 2]; MAX_CHANNELS];
    }

    /// The number of bytes of `len` that are decoded: the whole blocks and the whole frame groups
    /// of the last, short block.
    fn used_len(&self, len: usize) -> usize {
        let group = FRAME_BYTES * self.channels;
        let block = group.saturating_mul(self.block_frames);
        len / block * block + len % block / group * group
    }

    /// Prepares to decode a packet. Returns the number of frames in the packet.
    pub(crate) fn begin(&self, data: &[u8]) -> Result<usize> {
        let used = self.used_len(data.len());

        if used == 0 {
            return decode_error("adpcm (psx): invalid packet size");
        }

        // FFmpeg rejects the packet if any filter is out of range.
        if data[..used].chunks_exact(FRAME_BYTES).any(|frame| usize::from(frame[0] >> 4) >= 5) {
            return decode_error("adpcm (psx): invalid filter");
        }

        Ok(used / (FRAME_BYTES * self.channels) * FRAME_SAMPLES)
    }

    /// Decodes the channel `ch` of the packet `data` into `out`, which must have been sized from
    /// the result of [`PsxDecoder::begin`].
    pub(crate) fn decode_channel(&mut self, data: &[u8], ch: usize, out: &mut [i32]) {
        let used = self.used_len(data.len());
        let group = FRAME_BYTES * self.channels;
        let block_bytes = group.saturating_mul(self.block_frames);
        let [mut s1, mut s2] = self.state[ch];

        let mut out = out.chunks_exact_mut(FRAME_SAMPLES);

        for block in data[..used].chunks(block_bytes) {
            // The channels of a block are consecutive.
            let chunk_len = block.len() / self.channels;
            let chunk = &block[ch * chunk_len..][..chunk_len];

            for (frame, out) in chunk.chunks_exact(FRAME_BYTES).zip(&mut out) {
                let shift = u32::from(frame[0] & 0x0f);
                let [c0, c1] = FILTERS[usize::from(frame[0] >> 4).min(FILTERS.len() - 1)];
                let flag = frame[1] & 0x07;

                for (n, out) in out.iter_mut().enumerate() {
                    let byte = frame[2 + n / 2];
                    let nibble = if n & 1 == 0 { byte } else { byte >> 4 };
                    // Sign extend the 4-bit residual.
                    let d = i32::from(((nibble & 0x0f) << 4) as i8 >> 4);

                    // The flag 7 marks silence. The history is not clipped, and wraps in the
                    // (pathological) streams for which FFmpeg overflows.
                    let sample = if flag < 0x07 {
                        ((d << 12) >> shift).wrapping_add(
                            s1.wrapping_mul(c0).wrapping_add(s2.wrapping_mul(c1)) / 64,
                        )
                    }
                    else {
                        0
                    };

                    *out = from_i16_shift!(clamp_i16(sample));
                    s2 = s1;
                    s1 = sample;
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
        assert!(PsxDecoder::try_new(0, None).is_err());
        assert!(PsxDecoder::try_new(9, None).is_err());
        assert!(PsxDecoder::try_new(2, Some(30)).is_err());
        assert!(PsxDecoder::try_new(2, Some(0)).is_err());
        assert!(PsxDecoder::try_new(2, Some(56)).is_ok());
    }

    #[test]
    fn rejects_bad_packets() {
        let dec = PsxDecoder::try_new(2, None).unwrap();
        // Too short for a frame of each channel.
        assert!(dec.begin(&[0; 31]).is_err());
        // A filter out of range.
        let mut data = [0u8; 32];
        data[16] = 0x50;
        assert!(dec.begin(&data).is_err());
        data[16] = 0x40;
        assert_eq!(dec.begin(&data).unwrap(), 28);
        // A trailing partial group of frames is dropped.
        assert_eq!(dec.begin(&[0; 63]).unwrap(), 28);
    }

    #[test]
    fn short_last_block_has_shorter_channel_chunks() {
        // Two channels, 2 frames per channel in a block, 3 frames in the packet: the last block has
        // one frame (16 bytes) per channel.
        let mut dec = PsxDecoder::try_new(2, Some(56)).unwrap();
        let mut data = vec![0u8; 3 * 2 * FRAME_BYTES];
        // First frame of the right channel in the last block.
        data[5 * FRAME_BYTES] = 0x00;
        data[5 * FRAME_BYTES + 2] = 0x01;
        assert_eq!(dec.begin(&data).unwrap(), 3 * 28);
        let mut right = vec![0i32; 3 * 28];
        dec.decode_channel(&data, 1, &mut right);
        assert_eq!(right[2 * 28], 1 << 12 << 16);
        assert!(right[..2 * 28].iter().all(|&s| s == 0));
    }
}
