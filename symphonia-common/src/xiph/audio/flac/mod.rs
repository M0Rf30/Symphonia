// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::{Channels, Position};
use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::*;

#[derive(PartialEq, Eq)]
pub enum MetadataBlockType {
    StreamInfo,
    Padding,
    Application,
    SeekTable,
    VorbisComment,
    Cuesheet,
    Picture,
    Unknown(u8),
}

fn flac_channels_to_channels(channels: u32) -> Channels {
    debug_assert!(channels > 0 && channels < 9);

    let positions = match channels {
        1 => Position::FRONT_LEFT,
        2 => Position::FRONT_LEFT | Position::FRONT_RIGHT,
        3 => Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::FRONT_CENTER,
        4 => {
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::REAR_LEFT
                | Position::REAR_RIGHT
        }
        5 => {
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::REAR_LEFT
                | Position::REAR_RIGHT
        }
        6 => {
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::LFE1
                | Position::REAR_LEFT
                | Position::REAR_RIGHT
        }
        7 => {
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::LFE1
                | Position::REAR_CENTER
                | Position::SIDE_LEFT
                | Position::SIDE_RIGHT
        }
        8 => {
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::LFE1
                | Position::REAR_LEFT
                | Position::REAR_RIGHT
                | Position::SIDE_LEFT
                | Position::SIDE_RIGHT
        }
        _ => unreachable!(),
    };

    Channels::Positioned(positions)
}

#[derive(Debug, Default)]
pub struct StreamInfo {
    /// The minimum and maximum number of decoded samples per block of audio.
    pub block_len_min: u16,
    pub block_len_max: u16,
    /// The minimum and maximum byte length of an encoded block (frame) of audio. Either value may
    /// be 0 if unknown.
    pub frame_byte_len_min: u32,
    pub frame_byte_len_max: u32,
    /// The sample rate in Hz.
    pub sample_rate: u32,
    /// The channel mask.
    pub channels: Channels,
    /// The number of bits per sample of the stream.
    pub bits_per_sample: u32,
    /// The total number of samples in the stream, if available.
    pub n_samples: Option<u64>,
    /// The MD5 hash value of the decoded audio.
    pub md5: Option<[u8; 16]>,
}

impl StreamInfo {
    /// Read a stream information block.
    pub fn read<B: ReadBytes>(reader: &mut B) -> Result<StreamInfo> {
        let mut info = StreamInfo {
            block_len_min: 0,
            block_len_max: 0,
            frame_byte_len_min: 0,
            frame_byte_len_max: 0,
            sample_rate: 0,
            channels: Channels::None,
            bits_per_sample: 0,
            n_samples: None,
            md5: None,
        };

        // Read the block length bounds in number of samples.
        info.block_len_min = reader.read_be_u16()?;
        info.block_len_max = reader.read_be_u16()?;

        // Validate the block length bounds are in the range [16, 65535] samples.
        if info.block_len_min < 16 || info.block_len_max < 16 {
            return decode_error("flac: minimum block length is 16 samples");
        }

        // Validate the maximum block size is greater than or equal to the minimum block size.
        if info.block_len_max < info.block_len_min {
            return decode_error(
                "flac: maximum block length is less than the minimum block length",
            );
        }

        // Read the frame byte length bounds.
        info.frame_byte_len_min = reader.read_be_u24()?;
        info.frame_byte_len_max = reader.read_be_u24()?;

        // Validate the maximum frame byte length is greater than or equal to the minimum frame byte
        // length if both are known. A value of 0 for either indicates the respective byte length is
        // unknown. Valid values are in the range [0, (2^24) - 1] bytes.
        if info.frame_byte_len_min > 0
            && info.frame_byte_len_max > 0
            && info.frame_byte_len_max < info.frame_byte_len_min
        {
            return decode_error(
                "flac: maximum frame length is less than the minimum frame length",
            );
        }

        let mut br = BitStreamLtr::new(reader);

        // Read sample rate, valid rates are [1, 655350] Hz.
        info.sample_rate = br.read_bits_leq32(20)?;

        if info.sample_rate < 1 || info.sample_rate > 655_350 {
            return decode_error("flac: stream sample rate out of bounds");
        }

        // Read number of channels minus 1. Valid number of channels are 1-8.
        let channels_enc = br.read_bits_leq32(3)? + 1;

        if channels_enc < 1 || channels_enc > 8 {
            return decode_error("flac: stream channels are out of bounds");
        }

        info.channels = flac_channels_to_channels(channels_enc);

        // Read bits per sample minus 1. Valid number of bits per sample are 4-32.
        info.bits_per_sample = br.read_bits_leq32(5)? + 1;

        if info.bits_per_sample < 4 || info.bits_per_sample > 32 {
            return decode_error("flac: stream bits per sample are out of bounds");
        }

        // Read the total number of samples. All values are valid. A value of 0 indiciates a stream
        // of unknown length.
        info.n_samples = match br.read_bits_leq64(36)? {
            0 => None,
            samples => Some(samples),
        };

        // Read the decoded audio data MD5. If the MD5 buffer is zeroed then no checksum is present.
        let mut md5 = [0; 16];
        reader.read_buf_exact(&mut md5)?;

        if md5 != [0; 16] {
            info.md5 = Some(md5);
        }

        Ok(info)
    }

    /// Check if the size is valid for a stream information block.
    pub fn is_valid_size(size: u64) -> bool {
        const STREAM_INFO_BLOCK_SIZE: u64 = 34;

        size == STREAM_INFO_BLOCK_SIZE
    }
}

pub struct MetadataBlockHeader {
    pub is_last: bool,
    pub block_type: MetadataBlockType,
    pub block_len: u32,
}

impl MetadataBlockHeader {
    /// Read a metadata block header.
    pub fn read<B: ReadBytes>(reader: &mut B) -> Result<MetadataBlockHeader> {
        let header_enc = reader.read_u8()?;

        // First bit of the header indicates if this is the last metadata block.
        let is_last = (header_enc & 0x80) == 0x80;

        // The next 7 bits of the header indicates the block type.
        let block_type_id = header_enc & 0x7f;

        let block_type = match block_type_id {
            0 => MetadataBlockType::StreamInfo,
            1 => MetadataBlockType::Padding,
            2 => MetadataBlockType::Application,
            3 => MetadataBlockType::SeekTable,
            4 => MetadataBlockType::VorbisComment,
            5 => MetadataBlockType::Cuesheet,
            6 => MetadataBlockType::Picture,
            _ => MetadataBlockType::Unknown(block_type_id),
        };

        let block_len = reader.read_be_u24()?;

        Ok(MetadataBlockHeader { is_last, block_type, block_len })
    }
}

/// Reads a big-endian unsigned integer encoded via extended UTF8, as used for FLAC frame and
/// sample sequence numbers. The encoded value may be up to 7 bytes long for a maximum integer bit
/// width of 36-bits.
///
/// Returns `None` if the first byte is not a valid start of a sequence, or if any continuation
/// byte is not of the form `0b10xx_xxxx`.
pub fn read_utf8_be_u64<B: ReadBytes>(src: &mut B) -> Result<Option<u64>> {
    let first = src.read_u8()?;
    let mut state = u64::from(first);

    // UTF8 prefixes 1s followed by a 0 to indicate the total number of bytes in the sequence.
    // Determine the mask of the data bits within the first byte. Values 0-127 are returned
    // immediately. Anything else is not the start of a sequence.
    let mask: u8 = match first {
        0x00..=0x7f => return Ok(Some(state)),
        0xc0..=0xdf => 0x1f,
        0xe0..=0xef => 0x0f,
        0xf0..=0xf7 => 0x07,
        0xf8..=0xfb => 0x03,
        0xfc..=0xfd => 0x01,
        0xfe => 0x00,
        _ => return Ok(None),
    };

    state &= u64::from(mask);

    // The number of continuation bytes is the number of leading zeros of the mask minus 2.
    for _ in 2..mask.leading_zeros() {
        let byte = src.read_u8()?;

        // Each continuation byte is prefixed with 0b10, leaving 6 data bits.
        if byte & 0xc0 != 0x80 {
            return Ok(None);
        }

        state = (state << 6) | u64::from(byte & 0x3f);
    }

    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::read_utf8_be_u64;
    use symphonia_core::io::BufReader;

    #[test]
    fn verify_read_utf8_be_u64() {
        let mut stream = BufReader::new(&[
            0x24, 0xc2, 0xa2, 0xe0, 0xa4, 0xb9, 0xe2, 0x82, //
            0xac, 0xf0, 0x90, 0x8d, 0x88, 0xff, 0x80, 0xbf, //
        ]);

        assert_eq!(read_utf8_be_u64(&mut stream).unwrap(), Some(36));
        assert_eq!(read_utf8_be_u64(&mut stream).unwrap(), Some(162));
        assert_eq!(read_utf8_be_u64(&mut stream).unwrap(), Some(2361));
        assert_eq!(read_utf8_be_u64(&mut stream).unwrap(), Some(8364));
        assert_eq!(read_utf8_be_u64(&mut stream).unwrap(), Some(66376));
        assert_eq!(read_utf8_be_u64(&mut stream).unwrap(), None);
        assert_eq!(read_utf8_be_u64(&mut stream).unwrap(), None);
        assert_eq!(read_utf8_be_u64(&mut stream).unwrap(), None);
    }

    #[test]
    fn invalid_continuation_byte_is_rejected() {
        // 0xc2 requires one continuation byte; 0x41 is not 0b10xx_xxxx.
        let mut stream = BufReader::new(&[0xc2, 0x41]);
        assert_eq!(read_utf8_be_u64(&mut stream).unwrap(), None);
    }
}
