// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IFF 8SVX delta-compressed sample data: Fibonacci-delta (`sCompression` 1) and exponential
//! (`sCompression` 2) coding.
//!
//! Matches FFmpeg's `8svx_fib` and `8svx_exp` decoders (`libavcodec/8svx.c`, from the tables and
//! the `delta_decode` function there). The packet is the whole `BODY` chunk, in which the data of
//! each channel (up to 2) is a separate run of equal size. A run begins with a pad byte, then
//! the initial value of the signal (minus 128), then the codes: two to a byte, the low nibble
//! first, each an index into a table of deltas.

/// The delta table of the Fibonacci-delta compression. Source: `fibonacci` in `8svx.c`.
const FIBONACCI: [i8; 16] = [-34, -21, -13, -8, -5, -3, -2, -1, 0, 1, 2, 3, 5, 8, 13, 21];

/// The delta table of the exponential compression. Source: `exponential` in `8svx.c`.
const EXPONENTIAL: [i8; 16] = [-128, -64, -32, -16, -8, -4, -2, -1, 0, 1, 2, 4, 8, 16, 32, 64];

/// The size of the pad byte and the initial value that start the data of a channel.
const HEADER_LEN: usize = 2;

/// The maximum number of channels.
pub(crate) const MAX_CHANNELS: usize = 2;

pub(crate) struct EightSvxDecoder {
    table: &'static [i8; 16],
}

impl EightSvxDecoder {
    pub(crate) fn new(exponential: bool) -> Self {
        EightSvxDecoder { table: if exponential { &EXPONENTIAL } else { &FIBONACCI } }
    }

    /// The length of the data of a channel in `len` bytes of packet, without its header. A last
    /// byte that does not complete the runs is ignored.
    fn channel_len(len: usize, channels: usize) -> usize {
        (len / channels).saturating_sub(HEADER_LEN)
    }

    /// Whether `len` bytes hold the header and at least a byte of codes of every channel.
    pub(crate) fn is_valid(len: usize, channels: usize) -> bool {
        len >= (HEADER_LEN + 1) * channels
    }

    /// The number of frames that `len` bytes decode to.
    pub(crate) fn frames_in(len: usize, channels: usize) -> usize {
        2 * Self::channel_len(len, channels)
    }

    /// Decodes the channel `ch` of the packet `data` to `out`, which holds
    /// `frames_in(data.len(), channels)` samples.
    pub(crate) fn decode_channel(&self, data: &[u8], channels: usize, ch: usize, out: &mut [i32]) {
        let chan_len = Self::channel_len(data.len(), channels);
        let start = ch * (HEADER_LEN + chan_len);

        let Some(run) = data.get(start..start + HEADER_LEN + chan_len)
        else {
            return;
        };

        // The first byte is padding. The signal is an unsigned 8-bit one.
        let mut val = run[1].wrapping_add(128);

        for (&code, out) in run[HEADER_LEN..].iter().zip(out.chunks_exact_mut(2)) {
            val = clip_u8(i32::from(val) + i32::from(self.table[usize::from(code & 0x0f)]));
            out[0] = to_sample(val);
            val = clip_u8(i32::from(val) + i32::from(self.table[usize::from(code >> 4)]));
            out[1] = to_sample(val);
        }
    }
}

fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Converts an unsigned 8-bit sample to the full scale of a signed 32-bit sample.
fn to_sample(v: u8) -> i32 {
    (i32::from(v) - 128) << 24
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_fibonacci() {
        // The initial value is 0 + 128 = 128. The codes: low nibble first, 0xa (+2) and 0x9 (+1),
        // then 0x0 (-34) and 0xf (+21).
        let dec = EightSvxDecoder::new(false);
        let data = [0, 0, 0x9a, 0xf0];
        let mut out = [0; 4];

        assert_eq!(EightSvxDecoder::frames_in(data.len(), 1), 4);

        dec.decode_channel(&data, 1, 0, &mut out);

        assert_eq!(out, [130 - 128, 131 - 128, 97 - 128, 118 - 128].map(|v| v << 24));
    }

    #[test]
    fn verify_clipping_and_channels() {
        let dec = EightSvxDecoder::new(true);

        // Two channels, 2 bytes of codes each. The first channel starts at 250 (122 + 128), and
        // adds 64 twice (clipped to 255), then subtracts 128 twice. The second starts at 5
        // (133 + 128 wraps) and subtracts 128 (clipped to 0).
        let data = [0, 122, 0xff, 0x00, 0, 133, 0x00, 0x00];
        let mut out = [0; 4];

        assert_eq!(EightSvxDecoder::frames_in(data.len(), 2), 4);

        dec.decode_channel(&data, 2, 0, &mut out);
        assert_eq!(out, [255, 255, 127, 0].map(|v| (v - 128) << 24));

        dec.decode_channel(&data, 2, 1, &mut out);
        assert_eq!(out, [-128 << 24; 4]);
    }
}
