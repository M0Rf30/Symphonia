// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CRI ADX ADPCM (SEGA / CRI Middleware).
//!
//! Matches FFmpeg's `adpcm_adx` decoder (`libavcodec/adxdec.c`, with the filter coefficients of
//! `ff_adx_calculate_coeffs` in `libavcodec/adx.c`).
//!
//! A packet is a whole number of block groups. A block group holds one 18 byte block for each
//! channel, in channel order. A block is a big-endian 16-bit scale followed by 32 signed 4-bit
//! residuals (most significant nibble first). A second-order fixed filter predicts each sample
//! from the two preceding output samples of the channel (the history carries over from block to
//! block):
//!
//! ```text
//! out[n] = clip16(residual[n] * scale + ((c0 * out[n-1] + c1 * out[n-2]) >> 12))
//! ```
//!
//! The coefficients depend on the high-pass cutoff frequency in the stream header and the sample
//! rate. The decoder takes the ADX stream header (from the start of the file up to and including
//! the `(c)CRI` copyright string) as `extra_data`.
//!
//! A block whose scale has bit 15 set marks the end of the stream. Decoding stops at the block
//! group with such a block and nothing is produced for it or for the packets that follow, until
//! the decoder is reset.

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::util::clamp::clamp_i16;

use crate::common::from_i16_shift;

/// The size in bytes of the block of one channel.
pub(crate) const BLOCK_SIZE: usize = 18;

/// The number of samples in the block of one channel.
pub(crate) const BLOCK_SAMPLES: usize = 32;

/// The largest number of channels (`MAX_CHANNELS`, `libavcodec/adx.h`).
pub(crate) const MAX_CHANNELS: usize = 6;

/// The precision of the filter coefficients in bits (`COEFF_BITS`, `libavcodec/adx.h`).
const COEFF_BITS: u32 = 12;

/// The shortest ADX header the decoder accepts: the fields through the cutoff frequency.
const MIN_HEADER_LEN: usize = 24;

/// The length of the `(c)CRI` copyright string that ends the header.
const COPYRIGHT: &[u8; 6] = b"(c)CRI";

/// Calculates the two filter coefficients (`ff_adx_calculate_coeffs`, `libavcodec/adx.c`).
fn calculate_coeffs(cutoff: u32, sample_rate: u32) -> [i32; 2] {
    let a = std::f64::consts::SQRT_2
        - (2.0 * std::f64::consts::PI * f64::from(cutoff) / f64::from(sample_rate)).cos();
    let b = std::f64::consts::SQRT_2 - 1.0;
    // The radicand is never negative in exact arithmetic (a >= b); guard rounding errors.
    let c = (a - ((a + b) * (a - b)).max(0.0).sqrt()) / b;
    let scale = f64::from(1u32 << COEFF_BITS);

    // FFmpeg rounds `(float)(x)` to the nearest integer, ties to even, with `lrintf`.
    let round = |x: f64| (x as f32).round_ties_even() as i32;

    [round(c * 2.0 * scale), round(-(c * c) * scale)]
}

/// The history of a channel: the last two output samples.
#[derive(Clone, Copy, Default)]
struct ChannelState {
    s1: i32,
    s2: i32,
}

pub(crate) struct AdxDecoder {
    channels: usize,
    coeff: [i32; 2],
    state: [ChannelState; MAX_CHANNELS],
    /// True once the end of stream block was seen.
    eof: bool,
}

impl AdxDecoder {
    /// Creates a decoder from the ADX stream header (`adx_decode_header`, `libavcodec/adxdec.c`).
    pub(crate) fn try_new(header: &[u8], channels: usize) -> Result<Self> {
        if header.len() < MIN_HEADER_LEN {
            return unsupported_error("adpcm (adx): the stream header is required");
        }

        if u16::from_be_bytes([header[0], header[1]]) != 0x8000 {
            return decode_error("adpcm (adx): invalid stream header");
        }

        let offset = usize::from(u16::from_be_bytes([header[2], header[3]])) + 4;

        // If the copyright string is within the header, validate it.
        if header.len() >= offset && offset >= 6 && &header[offset - 6..offset] != COPYRIGHT {
            return decode_error("adpcm (adx): invalid stream header");
        }

        // Only encoding 3 (fixed coefficients), 18 byte blocks of 4 bit samples are supported.
        if header[4] != 3 || header[5] != BLOCK_SIZE as u8 || header[6] != 4 {
            return unsupported_error("adpcm (adx): unsupported ADX encoding");
        }

        let header_channels = usize::from(header[7]);
        if header_channels == 0 || header_channels > MAX_CHANNELS {
            return decode_error("adpcm (adx): invalid number of channels");
        }

        if header_channels != channels {
            return decode_error("adpcm (adx): the channel count differs from the header");
        }

        let sample_rate = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
        if sample_rate < 1
            || u64::from(sample_rate) > (i32::MAX as u64) / (channels * BLOCK_SIZE * 8) as u64
        {
            return decode_error("adpcm (adx): invalid sample rate");
        }

        // The encryption of ADX versions 4.8 and 4.9 (see `adx.c` of vgmstream) is not supported.
        if matches!(u16::from_be_bytes([header[0x12], header[0x13]]), 0x0408 | 0x0409) {
            return unsupported_error("adpcm (adx): encrypted streams are unsupported");
        }

        let cutoff = u32::from(u16::from_be_bytes([header[16], header[17]]));

        Ok(AdxDecoder {
            channels,
            coeff: calculate_coeffs(cutoff, sample_rate),
            state: [ChannelState::default(); MAX_CHANNELS],
            eof: false,
        })
    }

    pub(crate) fn reset(&mut self) {
        self.state = [ChannelState::default(); MAX_CHANNELS];
        self.eof = false;
    }

    /// Prepares to decode a packet. Returns the number of frames in the packet.
    ///
    /// A packet that is not a whole number of block groups can only be the end of stream marker.
    pub(crate) fn begin(&mut self, data: &[u8]) -> Result<usize> {
        if self.eof {
            return Ok(0);
        }

        let group_len = BLOCK_SIZE * self.channels;
        let num_groups = data.len() / group_len;

        if num_groups == 0 || data.len() % group_len != 0 {
            if data.len() >= 4 && data[0] & 0x80 != 0 {
                self.eof = true;
                return Ok(0);
            }
            return decode_error("adpcm (adx): invalid packet size");
        }

        // The groups before the first with an end of stream block are decoded.
        let valid = data
            .chunks_exact(group_len)
            .take_while(|group| group.chunks_exact(BLOCK_SIZE).all(|block| block[0] & 0x80 == 0))
            .count();

        if valid < num_groups {
            self.eof = true;
        }

        Ok(valid * BLOCK_SAMPLES)
    }

    /// Decodes the channel `ch` of the packet `data` into `out`, which must have been sized from
    /// the result of [`AdxDecoder::begin`].
    pub(crate) fn decode_channel(&mut self, data: &[u8], ch: usize, out: &mut [i32]) {
        let group_len = BLOCK_SIZE * self.channels;
        let [c0, c1] = self.coeff;
        let ChannelState { mut s1, mut s2 } = self.state[ch];

        let blocks =
            data.chunks_exact(group_len).map(|group| &group[ch * BLOCK_SIZE..][..BLOCK_SIZE]);

        for (block, out) in blocks.zip(out.chunks_exact_mut(BLOCK_SAMPLES)) {
            let scale = i32::from(u16::from_be_bytes([block[0], block[1]]));

            for (i, out) in out.iter_mut().enumerate() {
                let byte = block[2 + i / 2];
                let nibble = if i & 1 == 0 { byte >> 4 } else { byte & 0x0f };
                // Sign extend the 4-bit residual.
                let d = i32::from((nibble << 4) as i8 >> 4);

                let s0 = d * scale + ((c0 * s1 + c1 * s2) >> COEFF_BITS);
                s2 = s1;
                s1 = i32::from(clamp_i16(s0));
                *out = from_i16_shift!(s1);
            }
        }

        self.state[ch] = ChannelState { s1, s2 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(channels: u8, rate: u32, cutoff: u16) -> Vec<u8> {
        let mut h = vec![0u8; 0x24];
        h[0] = 0x80;
        h[3] = 0x20;
        h[4] = 3;
        h[5] = 18;
        h[6] = 4;
        h[7] = channels;
        h[8..12].copy_from_slice(&rate.to_be_bytes());
        h[16..18].copy_from_slice(&cutoff.to_be_bytes());
        h[0x12] = 3;
        h[0x1e..0x24].copy_from_slice(COPYRIGHT);
        h
    }

    #[test]
    fn coefficients_match_ffmpeg() {
        // The values of `ff_adx_calculate_coeffs` (libavcodec/adx.c) built with gcc and glibc,
        // for (cutoff, sample rate).
        assert_eq!(calculate_coeffs(500, 44100), [7334, -3283]);
        assert_eq!(calculate_coeffs(500, 22050), [6569, -2634]);
        assert_eq!(calculate_coeffs(1000, 48000), [6688, -2730]);
        assert_eq!(calculate_coeffs(0, 44100), [8192, -4096]);
        assert_eq!(calculate_coeffs(100, 8000), [7252, -3210]);
    }

    #[test]
    fn rejects_bad_headers() {
        let mut h = header(2, 44100, 500);
        assert!(AdxDecoder::try_new(&h, 2).is_ok());
        // A channel count that differs from the stream.
        assert!(AdxDecoder::try_new(&h, 1).is_err());
        // The copyright string.
        h[0x20] = b'x';
        assert!(AdxDecoder::try_new(&h, 2).is_err());
        h[0x20] = b'c';
        // Unsupported encoding.
        h[4] = 4;
        assert!(AdxDecoder::try_new(&h, 2).is_err());
        h[4] = 3;
        // Encryption.
        h[0x12] = 4;
        h[0x13] = 8;
        assert!(AdxDecoder::try_new(&h, 2).is_err());
        // Too short.
        assert!(AdxDecoder::try_new(&h[..20], 2).is_err());
    }

    #[test]
    fn end_of_stream_block_stops_decoding() {
        let mut dec = AdxDecoder::try_new(&header(1, 44100, 500), 1).unwrap();
        let mut data = vec![0u8; 3 * BLOCK_SIZE];
        data[BLOCK_SIZE] = 0x80;
        assert_eq!(dec.begin(&data).unwrap(), BLOCK_SAMPLES);
        // Nothing more is produced until the decoder is reset.
        assert_eq!(dec.begin(&data[..BLOCK_SIZE]).unwrap(), 0);
        dec.reset();
        assert_eq!(dec.begin(&data[..BLOCK_SIZE]).unwrap(), BLOCK_SAMPLES);
    }
}
