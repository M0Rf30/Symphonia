// Symphonia TTA Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The 22-byte TTA1 header, and the stream parameters derived from it.
//!
//! Layout and derived values from FFmpeg `libavformat/tta.c` (`tta_read_header`) and
//! `libavcodec/tta.c` (`tta_decode_init`).

use symphonia_core::audio::{Channels, Position};
use symphonia_core::checksum::Crc32Ieee;
use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::Monitor;

/// The length of the header in bytes.
pub(crate) const HEADER_LEN: usize = 22;

/// The signature of the header.
pub(crate) const SIGNATURE: [u8; 4] = *b"TTA1";

/// The format value of a stream that is not encrypted.
pub(crate) const FORMAT_SIMPLE: u16 = 1;
/// The format value of a password-protected stream.
pub(crate) const FORMAT_ENCRYPTED: u16 = 2;

/// The greatest supported number of channels (`tta_decode_init`).
pub(crate) const MAX_CHANNELS: u16 = 16;

/// The greatest supported sample rate in Hz. The demuxer of FFmpeg rejects greater rates
/// (`tta_read_header`); the decoder accepts up to 0x7fffff Hz, which is far beyond any real
/// stream and would make a frame buffer of several hundred MiB necessary.
pub(crate) const MAX_SAMPLE_RATE: u32 = 1_000_000;

/// The greatest bit depth that is decoded (`tta_decode_init` supports 1 to 3 bytes per sample).
pub(crate) const MAX_BITS_PER_SAMPLE: u16 = 24;

/// The header of a TTA1 stream.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Header {
    /// `FORMAT_SIMPLE` or `FORMAT_ENCRYPTED`.
    pub format: u16,
    pub channels: u16,
    /// The bits per sample as written by the encoder.
    pub bits_per_sample: u16,
    pub sample_rate: u32,
    /// The number of samples (per channel) of the stream. Zero if unknown.
    pub data_length: u32,
}

impl Header {
    /// Parse a header. The CRC is not checked, see [`Header::crc_matches`].
    pub fn parse(buf: &[u8; HEADER_LEN]) -> Result<Header> {
        if buf[0..4] != SIGNATURE {
            return decode_error("tta: missing TTA1 signature");
        }

        let u16_at = |i: usize| u16::from_le_bytes([buf[i], buf[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);

        Ok(Header {
            format: u16_at(4),
            channels: u16_at(6),
            bits_per_sample: u16_at(8),
            sample_rate: u32_at(10),
            data_length: u32_at(14),
        })
    }

    /// Returns true if the CRC-32 stored in the last 4 bytes of `buf` matches the first 18.
    pub fn crc_matches(buf: &[u8; HEADER_LEN]) -> bool {
        crc32(&buf[..HEADER_LEN - 4]) == u32::from_le_bytes([buf[18], buf[19], buf[20], buf[21]])
    }

    /// The number of bytes of a decoded sample (`s->bps`).
    pub fn bytes_per_sample(&self) -> usize {
        usize::from(self.bits_per_sample).div_ceil(8)
    }

    /// The number of samples per channel of a frame: 256/245 s of audio.
    pub fn frame_length(&self) -> u64 {
        frame_length(self.sample_rate)
    }
}

/// The number of samples per channel of a frame: 256/245 s of audio (`tta_decode_init`).
pub(crate) fn frame_length(sample_rate: u32) -> u64 {
    256 * u64::from(sample_rate) / 245
}

/// The CRC-32 (reflected, polynomial 0xedb88320, initial value and final XOR of `!0`) used for the
/// header, the seek table and the frames.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc32Ieee::new(!0);
    crc.process_buf_bytes(data);
    !crc.crc()
}

/// Validates the properties of a stream that both the demuxer and the decoder rely on.
pub(crate) fn check_params(channels: u16, sample_rate: u32) -> Result<()> {
    if channels == 0 || channels > MAX_CHANNELS {
        return decode_error("tta: invalid number of channels");
    }
    if sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
        return decode_error("tta: invalid sample rate");
    }
    Ok(())
}

/// The layout of the channels of a stream of `count` channels.
///
/// The TTA header holds no channel mask. These are the layouts that FFmpeg assigns by channel
/// count (`tta_channel_layouts` in `libavcodec/tta.c`), all in the order of the channels in the
/// stream, which is the order of the bits of the channel positions. FFmpeg assigns no layout to
/// one channel (it is implicitly mono), to five channels, and to more than eight. The layout of
/// eight channels is `AV_CH_LAYOUT_7POINT1_WIDE`: with front-of-center and side channels.
pub(crate) fn channels_from_count(count: u16) -> Channels {
    use Position as P;

    let positions = match count {
        1 => P::FRONT_CENTER,
        2 => P::FRONT_LEFT | P::FRONT_RIGHT,
        3 => P::FRONT_LEFT | P::FRONT_RIGHT | P::LFE1,
        4 => P::FRONT_LEFT | P::FRONT_RIGHT | P::REAR_LEFT | P::REAR_RIGHT,
        6 => {
            P::FRONT_LEFT | P::FRONT_RIGHT | P::FRONT_CENTER | P::LFE1 | P::REAR_LEFT | P::REAR_RIGHT
        }
        7 => {
            P::FRONT_LEFT
                | P::FRONT_RIGHT
                | P::FRONT_CENTER
                | P::LFE1
                | P::REAR_LEFT
                | P::REAR_RIGHT
                | P::REAR_CENTER
        }
        8 => {
            P::FRONT_LEFT
                | P::FRONT_RIGHT
                | P::FRONT_CENTER
                | P::LFE1
                | P::FRONT_LEFT_CENTER
                | P::FRONT_RIGHT_CENTER
                | P::SIDE_LEFT
                | P::SIDE_RIGHT
        }
        _ => return Channels::Discrete(count),
    };

    Channels::Positioned(positions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_channel_counts() {
        for count in 1..=MAX_CHANNELS {
            assert_eq!(channels_from_count(count).count(), usize::from(count));
        }
    }

    #[test]
    fn verify_header_crc() {
        let mut hdr = [0u8; HEADER_LEN];
        hdr[0..4].copy_from_slice(b"TTA1");
        hdr[4..6].copy_from_slice(&1u16.to_le_bytes());
        hdr[6..8].copy_from_slice(&2u16.to_le_bytes());
        hdr[8..10].copy_from_slice(&16u16.to_le_bytes());
        hdr[10..14].copy_from_slice(&44100u32.to_le_bytes());
        hdr[14..18].copy_from_slice(&1000u32.to_le_bytes());
        let crc = crc32(&hdr[..18]);
        hdr[18..22].copy_from_slice(&crc.to_le_bytes());
        assert!(Header::crc_matches(&hdr));
        hdr[7] ^= 1;
        assert!(!Header::crc_matches(&hdr));
    }

    #[test]
    fn verify_frame_length() {
        // 256/245 s of audio.
        assert_eq!(frame_length(44100), 46080);
        assert_eq!(frame_length(8000), 8359);
        assert_eq!(frame_length(48000), 50155);
    }
}
