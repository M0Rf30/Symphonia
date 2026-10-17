// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Nintendo GameCube DTK ADPCM (the disc streaming audio, `.adp`).
//!
//! Matches FFmpeg's `adpcm_dtk` decoder (`ADPCM_DTK` in `libavcodec/adpcm.c`).
//!
//! The stream is stereo. A block is 32 bytes and holds 28 samples of each channel: the header of
//! the left channel (byte 0) and of the right channel (byte 1), each repeated once in the bytes 2
//! and 3, then 28 bytes whose low nibble is a residual of the left and whose high nibble is the
//! residual of the right channel. A header has the filter in the upper and the shift in the lower
//! nibble. A second-order filter predicts each sample from the two preceding (unclipped, with 6
//! more fractional bits) samples of the channel:
//!
//! ```text
//! out[n] = clip16((((residual << 12 >> shift) << 6) + prediction) >> 6)
//! ```
//!
//! The history carries over from block to block.

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::util::clamp::clamp_i16;

use crate::common::from_i16_shift;

/// The size in bytes of a block (of both channels).
const BLOCK_BYTES: usize = 32;

/// The number of samples per channel in a block.
const BLOCK_SAMPLES: usize = 28;

/// The size in bytes of the headers at the start of a block.
const HEADER_BYTES: usize = 4;

pub(crate) struct DtkDecoder {
    /// The last two samples of each channel.
    state: [[i32; 2]; 2],
}

impl DtkDecoder {
    pub(crate) fn try_new(channels: usize) -> Result<Self> {
        if channels != 2 {
            return unsupported_error("adpcm (dtk): only stereo is supported");
        }

        Ok(DtkDecoder { state: [[0; 2]; 2] })
    }

    pub(crate) fn reset(&mut self) {
        self.state = [[0; 2]; 2];
    }

    /// Prepares to decode a packet. Returns the number of frames in the packet.
    pub(crate) fn begin(&self, data: &[u8]) -> Result<usize> {
        match data.len() / BLOCK_BYTES {
            0 => decode_error("adpcm (dtk): invalid packet size"),
            blocks => Ok(blocks * BLOCK_SAMPLES),
        }
    }

    /// Decodes the channel `ch` of the packet `data` into `out`, which must have been sized from
    /// the result of [`DtkDecoder::begin`].
    pub(crate) fn decode_channel(&mut self, data: &[u8], ch: usize, out: &mut [i32]) {
        let [mut s1, mut s2] = self.state[ch];

        for (block, out) in data.chunks_exact(BLOCK_BYTES).zip(out.chunks_exact_mut(BLOCK_SAMPLES))
        {
            let header = block[ch];
            let shift = u32::from(header & 0x0f);

            for (&byte, out) in block[HEADER_BYTES..].iter().zip(out.iter_mut()) {
                let prediction = match header >> 4 {
                    1 => s1 * 0x3c,
                    2 => s1 * 0x73 - s2 * 0x34,
                    3 => s1 * 0x62 - s2 * 0x37,
                    _ => 0,
                };

                // `av_clip_intp2(x, 21)`.
                let prediction = ((prediction + 0x20) >> 6).clamp(-(1 << 21), (1 << 21) - 1);

                let nibble = if ch == 0 { byte & 0x0f } else { byte >> 4 };
                // Sign extend the 4-bit residual.
                let d = i32::from((nibble << 4) as i8 >> 4);

                let s0 = (((d << 12) >> shift) << 6) + prediction;
                *out = from_i16_shift!(clamp_i16(s0 >> 6));
                s2 = s1;
                s1 = s0;
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
        assert!(DtkDecoder::try_new(1).is_err());
        assert!(DtkDecoder::try_new(3).is_err());
        assert!(DtkDecoder::try_new(2).is_ok());
    }

    #[test]
    fn packet_sizes() {
        let dec = DtkDecoder::try_new(2).unwrap();
        assert!(dec.begin(&[0; 31]).is_err());
        assert_eq!(dec.begin(&[0; 32]).unwrap(), 28);
        assert_eq!(dec.begin(&[0; 95]).unwrap(), 56);
    }

    #[test]
    fn nibbles_and_headers_belong_to_their_channel() {
        let mut dec = DtkDecoder::try_new(2).unwrap();
        let mut data = [0u8; BLOCK_BYTES];
        // The right channel has shift 0 and a residual of 1 in its first sample.
        data[1] = 0x00;
        data[HEADER_BYTES] = 0x10;
        let mut left = [0i32; BLOCK_SAMPLES];
        let mut right = [0i32; BLOCK_SAMPLES];
        dec.decode_channel(&data, 0, &mut left);
        dec.decode_channel(&data, 1, &mut right);
        assert!(left.iter().all(|&s| s == 0));
        assert_eq!(right[0], 1 << 12 << 16);
    }
}
