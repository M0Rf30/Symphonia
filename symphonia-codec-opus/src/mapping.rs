// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `OpusHead` identification header parsing and channel mapping, per RFC 7845 section 5.1.
//!
//! `symphonia_core`'s codec parameters carry the raw `extra_data` (the identification header
//! bytes) for Opus tracks; this module parses that directly rather than depending on
//! `symphonia_common::xiph::audio::opus::OpusHead`, because that struct does not expose the
//! per-stream `stream_count`/`coupled_count`/mapping table needed for multistream (family 1
//! with >2 channels, and family 255) decoding — this module's [`ChannelMapping`] does.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingError {
    /// Too short, or the `OpusHead` magic signature did not match.
    InvalidHeader,
    /// A version/channel-count/mapping-table field was self-inconsistent.
    InvalidMapping,
    /// Encapsulation version newer than this parser understands (major version != 0).
    UnsupportedVersion,
}

impl fmt::Display for MappingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MappingError::InvalidHeader => write!(f, "invalid OpusHead"),
            MappingError::InvalidMapping => write!(f, "invalid channel mapping"),
            MappingError::UnsupportedVersion => write!(f, "unsupported OpusHead version"),
        }
    }
}

impl std::error::Error for MappingError {}

pub type Result<T, E = MappingError> = std::result::Result<T, E>;

const MAGIC: &[u8; 8] = b"OpusHead";

/// The RFC 7845 channel mapping table (mapping families 1 and 255).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMapping {
    pub family: u8,
    /// Number of Opus streams multiplexed into each packet.
    pub stream_count: u8,
    /// Number of those streams that are stereo-coupled (must be <= `stream_count`).
    pub coupled_count: u8,
    /// `mapping[output_channel] = decoded_channel_index`, or `255` for silence.
    /// Present (non-empty) only for family 1 and family 255; family 0 uses the implicit
    /// identity/Vorbis-order mapping for up to 2 channels and has an empty table here.
    pub table: Vec<u8>,
}

impl ChannelMapping {
    /// The implicit mapping used by family 0 (mono/stereo, no explicit table).
    fn family0(channels: u8) -> Self {
        ChannelMapping { family: 0, stream_count: 1, coupled_count: (channels == 2) as u8, table: Vec::new() }
    }
}

/// A parsed `OpusHead` identification header (RFC 7845 section 5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusHead {
    pub version: u8,
    pub channel_count: u8,
    /// Number of samples (at 48 kHz) to discard from the start of decoder output.
    pub pre_skip: u16,
    /// Sample rate of the original input, informational only (decoding is always at 48 kHz).
    pub input_sample_rate: u32,
    /// Q7.8 fixed-point output gain, in dB, to apply before mixdown.
    pub output_gain: i16,
    pub mapping: ChannelMapping,
}

impl OpusHead {
    /// Parses an `OpusHead` packet (or codec `extra_data`) per RFC 7845 section 5.1.
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 19 || &data[0..8] != MAGIC {
            return Err(MappingError::InvalidHeader);
        }

        let version = data[8];
        // RFC 7845: decoders should accept any version with the same major version number (the
        // upper nibble / >>4 is unspecified by the RFC in practice; libopusfile treats any
        // version <= 15 in the "0.x" family as acceptable and rejects only version 0 itself as
        // invalid encapsulation *and* anything with major version > 0. We mirror libopusfile's
        // permissive behaviour: accept 1..=15, reject 0 and >= 16 is undefined here so treat as
        // unsupported to be safe for forward compatibility.
        if version == 0 {
            return Err(MappingError::InvalidHeader);
        }
        if version >= 16 {
            return Err(MappingError::UnsupportedVersion);
        }

        let channel_count = data[9];
        if channel_count == 0 {
            return Err(MappingError::InvalidMapping);
        }

        let pre_skip = u16::from_le_bytes([data[10], data[11]]);
        let input_sample_rate = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
        let output_gain = i16::from_le_bytes([data[16], data[17]]);
        let channel_mapping_family = data[18];

        let mapping = match channel_mapping_family {
            0 => {
                if channel_count > 2 {
                    return Err(MappingError::InvalidMapping);
                }
                ChannelMapping::family0(channel_count)
            }
            1 | 255 => {
                if data.len() < 21 + channel_count as usize {
                    return Err(MappingError::InvalidHeader);
                }
                let stream_count = data[19];
                let coupled_count = data[20];
                if stream_count == 0 || coupled_count > stream_count {
                    return Err(MappingError::InvalidMapping);
                }
                let max_index = stream_count as u16 + coupled_count as u16;
                let table = data[21..21 + channel_count as usize].to_vec();
                for &idx in &table {
                    if idx != 255 && idx as u16 >= max_index {
                        return Err(MappingError::InvalidMapping);
                    }
                }
                if channel_mapping_family == 1 && channel_count > 8 {
                    // RFC 7845: family 1 (Vorbis channel order) is defined for up to 8 channels.
                    return Err(MappingError::InvalidMapping);
                }
                ChannelMapping { family: channel_mapping_family, stream_count, coupled_count, table }
            }
            _ => return Err(MappingError::InvalidMapping),
        };

        Ok(OpusHead { version, channel_count, pre_skip, input_sample_rate, output_gain, mapping })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_family0(channels: u8) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(MAGIC);
        v.push(1); // version
        v.push(channels);
        v.extend_from_slice(&100u16.to_le_bytes()); // pre_skip
        v.extend_from_slice(&48000u32.to_le_bytes()); // input rate
        v.extend_from_slice(&0i16.to_le_bytes()); // gain
        v.push(0); // family 0
        v
    }

    #[test]
    fn family0_mono_stereo() {
        for ch in [1u8, 2u8] {
            let bytes = build_family0(ch);
            let head = OpusHead::parse(&bytes).unwrap();
            assert_eq!(head.channel_count, ch);
            assert_eq!(head.mapping.family, 0);
            assert_eq!(head.mapping.stream_count, 1);
            assert_eq!(head.mapping.coupled_count, (ch == 2) as u8);
            assert!(head.mapping.table.is_empty());
        }
    }

    #[test]
    fn family0_rejects_more_than_stereo() {
        let bytes = build_family0(3);
        assert_eq!(OpusHead::parse(&bytes), Err(MappingError::InvalidMapping));
    }

    #[test]
    fn family1_5_1_surround() {
        // 5.1: 6 channels, 4 streams, 2 coupled (per RFC 7845 appendix A example).
        let mut v = Vec::new();
        v.extend_from_slice(MAGIC);
        v.push(1);
        v.push(6);
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&48000u32.to_le_bytes());
        v.extend_from_slice(&0i16.to_le_bytes());
        v.push(1); // family 1
        v.push(4); // stream_count
        v.push(2); // coupled_count
        v.extend_from_slice(&[0, 4, 1, 2, 3, 5]); // mapping table

        let head = OpusHead::parse(&v).unwrap();
        assert_eq!(head.mapping.stream_count, 4);
        assert_eq!(head.mapping.coupled_count, 2);
        assert_eq!(head.mapping.table, vec![0, 4, 1, 2, 3, 5]);
    }

    #[test]
    fn rejects_bad_magic_and_truncated() {
        assert_eq!(OpusHead::parse(b"NotOpus\0\0\0\0\0\0\0\0\0\0\0"), Err(MappingError::InvalidHeader));
        assert_eq!(OpusHead::parse(b"OpusHead"), Err(MappingError::InvalidHeader));
    }

    #[test]
    fn rejects_out_of_range_mapping_index() {
        let mut v = Vec::new();
        v.extend_from_slice(MAGIC);
        v.push(1);
        v.push(2);
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&48000u32.to_le_bytes());
        v.extend_from_slice(&0i16.to_le_bytes());
        v.push(255); // family 255
        v.push(1); // stream_count
        v.push(0); // coupled_count
        v.extend_from_slice(&[0, 9]); // index 9 is out of range (max_index=1)
        assert_eq!(OpusHead::parse(&v), Err(MappingError::InvalidMapping));
    }
}
