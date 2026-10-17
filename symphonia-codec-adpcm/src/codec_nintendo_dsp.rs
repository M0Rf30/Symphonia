// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Nintendo DSP-ADPCM (GameCube, Wii, 3DS, Wii U and Switch; also called THP or GC ADPCM).
//!
//! Matches the decode loop of FFmpeg's `adpcm_thp` and `adpcm_thp_le` decoders
//! (`libavcodec/adpcm.c`, `case AV_CODEC_ID_ADPCM_THP`). The audio of a channel is a sequence of
//! 8 byte frames of 14 samples. The first byte of a frame holds the index of a coefficient pair in
//! bits 4 to 6 and the exponent of the residuals in bits 0 to 3. The 14 residuals that follow are
//! signed 4-bit values, the most significant nibble first. A second order predictor makes
//! each sample from the two preceding output samples of the channel:
//!
//! ```text
//! out[n] = clip16(((s1 * c[2 * index] + s2 * c[2 * index + 1]) >> 11) + (residual[n] << exp))
//! ```
//!
//! A last frame that is not complete holds `1 + ceil(n / 2)` bytes for its `n` samples.
//!
//! # Normalised packet contract
//!
//! The demuxers of the container formats (BRSTM, BFSTM, BCSTM and DSP) differ in byte order and
//! in the way they lay out the channels. They normalise the stream to the form below, so that
//! this decoder does not depend on the container.
//!
//! * `extra_data`: for every channel the 16 coefficients, each a little-endian 16-bit integer
//!   (`32 * channels` bytes). The coefficient pairs are those of the DSP-ADPCM header.
//! * A packet is `flags`, the optional history, and then the data of the channels one after the
//!   other (planar, like FFmpeg's THP packets):
//!
//!   ```text
//!   u8                      flags: bit 0 set if the history is present, no other bit may be set
//!   channels x 2 x i16 LE   (the history is only here if bit 0 is set) the two preceding output
//!                           samples `yn1` and `yn2` of every channel
//!   channels x [u8; len]    the frames of every channel, `len` bytes each
//!   ```
//!
//! * The number of samples per channel `n` of a packet is its duration (including the part that is
//!   trimmed), and `len` is `8 * (n / 14)`, plus `1 + ceil((n % 14) / 2)` if `n` is not a multiple
//!   of 14. A packet that is not exactly the size this gives is malformed.
//! * The decoder keeps the history of every channel from packet to packet, so the packets that
//!   follow one another do not carry it. A packet that holds the history, as the first one after a
//!   seek does, replaces the history of the decoder. The history of a decoder that has been reset
//!   and that is not given any is zero.

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::util::clamp::clamp_i16;

use crate::common::from_i16_shift;

/// The largest number of channels. The container formats store up to 8 channels in practice, and
/// the size of a stream header is limited in the format that FFmpeg reads.
pub(crate) const MAX_CHANNELS: usize = 64;

/// The size in bytes of a full frame.
const FRAME_BYTES: usize = 8;

/// The number of samples in a full frame.
const FRAME_SAMPLES: usize = 14;

/// The number of coefficients of a channel (8 pairs).
const COEFFS_PER_CHANNEL: usize = 16;

/// The size in bytes of the coefficients of a channel in the extra data.
const COEFF_BYTES: usize = 2 * COEFFS_PER_CHANNEL;

/// The bit of the flags of a packet that tells that the history is present.
const FLAG_HISTORY: u8 = 0x01;

/// The size in bytes of the history of a channel in a packet.
const HISTORY_BYTES: usize = 4;

/// Returns the size in bytes of the frames of `samples` samples of one channel.
pub(crate) const fn data_len(samples: usize) -> usize {
    let partial = samples % FRAME_SAMPLES;
    FRAME_BYTES * (samples / FRAME_SAMPLES) + if partial == 0 { 0 } else { 1 + partial.div_ceil(2) }
}

/// The state and the coefficients of a channel.
#[derive(Clone, Copy, Default)]
struct Channel {
    coeffs: [i16; COEFFS_PER_CHANNEL],
    /// The preceding output sample (`yn1`).
    s1: i32,
    /// The output sample before it (`yn2`).
    s2: i32,
}

pub(crate) struct NintendoDspDecoder {
    channels: Vec<Channel>,
}

impl NintendoDspDecoder {
    /// Creates a decoder from the coefficients of the channels, 16 little-endian 16-bit integers
    /// each.
    pub(crate) fn try_new(extra_data: &[u8], channels: usize) -> Result<Self> {
        if channels == 0 || channels > MAX_CHANNELS {
            return unsupported_error("adpcm (nintendo dsp): unsupported number of channels");
        }

        if extra_data.len() != COEFF_BYTES * channels {
            return unsupported_error("adpcm (nintendo dsp): the coefficients are required");
        }

        let channels = extra_data
            .chunks_exact(COEFF_BYTES)
            .map(|bytes| {
                let mut ch = Channel::default();
                for (coeff, pair) in ch.coeffs.iter_mut().zip(bytes.chunks_exact(2)) {
                    *coeff = i16::from_le_bytes([pair[0], pair[1]]);
                }
                ch
            })
            .collect();

        Ok(NintendoDspDecoder { channels })
    }

    pub(crate) fn reset(&mut self) {
        for ch in &mut self.channels {
            ch.s1 = 0;
            ch.s2 = 0;
        }
    }

    /// Prepares to decode a packet of `frames` samples per channel. Checks the size of the packet
    /// and applies the history that it holds. Returns the position of the data of the first
    /// channel.
    pub(crate) fn begin(&mut self, data: &[u8], frames: usize) -> Result<usize> {
        let channels = self.channels.len();

        let Some((&flags, rest)) = data.split_first()
        else {
            return decode_error("adpcm (nintendo dsp): empty packet");
        };

        if flags & !FLAG_HISTORY != 0 {
            return decode_error("adpcm (nintendo dsp): invalid packet flags");
        }

        if frames == 0 {
            return decode_error("adpcm (nintendo dsp): empty packet duration");
        }

        let has_history = flags & FLAG_HISTORY != 0;
        let history_len = if has_history { HISTORY_BYTES * channels } else { 0 };

        // The length is checked before anything is changed, so that a malformed packet leaves
        // the state of the decoder as it was.
        if rest.len() != history_len + channels * data_len(frames) {
            return decode_error("adpcm (nintendo dsp): invalid packet size");
        }

        if has_history {
            for (ch, hist) in self.channels.iter_mut().zip(rest.chunks_exact(HISTORY_BYTES)) {
                ch.s1 = i32::from(i16::from_le_bytes([hist[0], hist[1]]));
                ch.s2 = i32::from(i16::from_le_bytes([hist[2], hist[3]]));
            }
        }

        Ok(1 + history_len)
    }

    /// Decodes the frames of the channel `ch`, `bytes` of them from the position that
    /// [`NintendoDspDecoder::begin`] returned, into `out`.
    pub(crate) fn decode_channel(&mut self, ch: usize, bytes: &[u8], out: &mut [i32]) {
        let Some(channel) = self.channels.get_mut(ch)
        else {
            return;
        };

        let Channel { coeffs, mut s1, mut s2 } = *channel;

        for (frame, out) in bytes.chunks(FRAME_BYTES).zip(out.chunks_mut(FRAME_SAMPLES)) {
            let header = frame[0];
            let index = usize::from((header >> 4) & 7);
            let exp = u32::from(header & 0x0f);
            let c1 = i64::from(coeffs[2 * index]);
            let c2 = i64::from(coeffs[2 * index + 1]);

            for (n, out) in out.iter_mut().enumerate() {
                let byte = frame[1 + n / 2];
                let nibble = if n & 1 == 0 { byte >> 4 } else { byte & 0x0f };
                // Sign extend the 4-bit residual.
                let residual = i32::from((nibble << 4) as i8 >> 4);

                // The prediction is at most 2^20 in magnitude, the scaled residual at most 2^18.
                let predicted = ((i64::from(s1) * c1 + i64::from(s2) * c2) >> 11) as i32;
                let sample = i32::from(clamp_i16(predicted + (residual << exp)));

                s2 = s1;
                s1 = sample;
                *out = from_i16_shift!(sample);
            }
        }

        channel.s1 = s1;
        channel.s2 = s2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extra(channels: usize) -> Vec<u8> {
        // Pair 0 is the identity predictor (2048 >> 11 = 1), pair 1 repeats the sample before it
        // and pair 2 extrapolates linearly.
        let mut coeffs = [0i16; COEFFS_PER_CHANNEL];
        coeffs[0] = 2048;
        coeffs[3] = 2048;
        coeffs[4] = 4096;
        coeffs[5] = -2048;
        (0..channels).flat_map(|_| coeffs.iter().flat_map(|c| c.to_le_bytes())).collect()
    }

    fn decode(dec: &mut NintendoDspDecoder, data: &[u8], frames: usize) -> Vec<Vec<i32>> {
        let mut pos = dec.begin(data, frames).unwrap();
        let len = data_len(frames);
        (0..dec.channels.len())
            .map(|ch| {
                let mut out = vec![0; frames];
                dec.decode_channel(ch, &data[pos..pos + len], &mut out);
                pos += len;
                out
            })
            .collect()
    }

    #[test]
    fn data_lengths() {
        assert_eq!(data_len(1), 2);
        assert_eq!(data_len(2), 2);
        assert_eq!(data_len(3), 3);
        assert_eq!(data_len(13), 8);
        assert_eq!(data_len(14), 8);
        assert_eq!(data_len(15), 10);
        assert_eq!(data_len(28), 16);
        assert_eq!(data_len(14336), 8192);
    }

    #[test]
    fn decodes_residuals_and_exponent() {
        let mut dec = NintendoDspDecoder::try_new(&extra(1), 1).unwrap();
        // Predictor 0 (the previous sample), exponent 2. Residuals: 1, -1, 7, -8 and zeros.
        let mut data = vec![0u8, 0x02, 0x1f, 0x78];
        data.extend_from_slice(&[0; 5]);
        let out = decode(&mut dec, &data, 14);
        let samples: Vec<i32> = out[0].iter().map(|s| s >> 16).collect();
        assert_eq!(&samples[..5], &[4, 0, 28, -4, -4]);
        assert!(samples[5..].iter().all(|&s| s == -4));
    }

    #[test]
    fn history_is_kept_between_packets() {
        let mut dec = NintendoDspDecoder::try_new(&extra(1), 1).unwrap();
        // One residual of 1 (exponent 4) on top of the previous sample.
        let data = [0u8, 0x04, 0x10, 0, 0, 0, 0, 0, 0];
        let a = decode(&mut dec, &data, 14);
        assert_eq!(a[0][0] >> 16, 16);
        let b = decode(&mut dec, &data[..], 14);
        assert_eq!(b[0][0] >> 16, 32);
        // A reset starts from silence.
        dec.reset();
        assert_eq!(decode(&mut dec, &data, 14)[0][0] >> 16, 16);
    }

    #[test]
    fn packet_history_replaces_the_state() {
        let mut dec = NintendoDspDecoder::try_new(&extra(2), 2).unwrap();
        // Predictor 2: 2 * yn1 - yn2 with yn1 = 100 and yn2 = 90, and 100 and -3 for channel 1.
        let mut data = vec![FLAG_HISTORY];
        for h in [100i16, 90, 7, -3] {
            data.extend_from_slice(&h.to_le_bytes());
        }
        for _ in 0..2 {
            // Frame header: predictor 2, exponent 0, residual 0 and a partial frame of 2 samples.
            data.extend_from_slice(&[0x20, 0x00]);
        }
        let out = decode(&mut dec, &data, 2);
        // 2 * 100 - 90 = 110, then 2 * 110 - 100 = 120.
        assert_eq!(out[0].iter().map(|s| s >> 16).collect::<Vec<_>>(), [110, 120]);
        // 2 * 7 + 3 = 17, then 2 * 17 - 7 = 27.
        assert_eq!(out[1].iter().map(|s| s >> 16).collect::<Vec<_>>(), [17, 27]);
    }

    #[test]
    fn clips_to_16_bits() {
        let mut dec = NintendoDspDecoder::try_new(&extra(1), 1).unwrap();
        // The largest residuals with the largest exponent: 7 << 15 twice, and -8 << 15.
        let data = [0u8, 0x0f, 0x77, 0x80, 0, 0, 0, 0, 0];
        let out = decode(&mut dec, &data, 14);
        assert_eq!(out[0][0] >> 16, 32767);
        assert_eq!(out[0][1] >> 16, 32767);
        assert_eq!(out[0][2] >> 16, -32768);
        assert_eq!(out[0][3] >> 16, -32768);
    }

    #[test]
    fn rejects_malformed_packets() {
        let mut dec = NintendoDspDecoder::try_new(&extra(2), 2).unwrap();
        let ok = vec![0u8; 1 + 2 * 8];
        assert!(dec.begin(&ok, 14).is_ok());
        // Empty, a size that is not the one of the duration and a history that is missing.
        assert!(dec.begin(&[], 14).is_err());
        assert!(dec.begin(&ok[..ok.len() - 1], 14).is_err());
        assert!(dec.begin(&ok, 15).is_err());
        assert!(dec.begin(&ok, 0).is_err());
        let mut hist = ok.clone();
        hist[0] = FLAG_HISTORY;
        assert!(dec.begin(&hist, 14).is_err());
        // Unknown flags.
        hist[0] = 2;
        assert!(dec.begin(&hist, 14).is_err());
        // The state is left alone by a packet that is rejected.
        let mut bad = vec![FLAG_HISTORY];
        bad.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0]);
        assert!(dec.begin(&bad, 14).is_err());
        assert_eq!((dec.channels[0].s1, dec.channels[0].s2), (0, 0));
    }

    #[test]
    fn rejects_bad_extra_data() {
        assert!(NintendoDspDecoder::try_new(&extra(1), 2).is_err());
        assert!(NintendoDspDecoder::try_new(&extra(2), 1).is_err());
        assert!(NintendoDspDecoder::try_new(&[], 1).is_err());
        assert!(NintendoDspDecoder::try_new(&extra(1), 0).is_err());
        assert!(NintendoDspDecoder::try_new(&extra(MAX_CHANNELS + 1), MAX_CHANNELS + 1).is_err());
    }
}
