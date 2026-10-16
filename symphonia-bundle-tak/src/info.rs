// Symphonia TAK Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Stream information, frame headers and the CRC-24 of TAK.
//!
//! Ported from FFmpeg's `libavcodec/tak.c` and `libavcodec/tak.h` (LGPL-2.1+, Paul B Mahol); the
//! field widths and the frame duration table are those of the reference.

use symphonia_core::errors::{Result, decode_error};

use crate::bits::BitReader;

const ENCODER_CODEC_BITS: u32 = 6;
const ENCODER_PROFILE_BITS: u32 = 4;
const SIZE_FRAME_DURATION_BITS: u32 = 4;
const SIZE_SAMPLES_NUM_BITS: u32 = 35;
const FORMAT_DATA_TYPE_BITS: u32 = 3;
const FORMAT_SAMPLE_RATE_BITS: u32 = 18;
const FORMAT_BPS_BITS: u32 = 5;
const FORMAT_CHANNEL_BITS: u32 = 4;
const FORMAT_VALID_BITS: usize = 5;
const FORMAT_CH_LAYOUT_BITS: u32 = 6;

const SAMPLE_RATE_MIN: u32 = 6000;
const BPS_MIN: u32 = 8;
const CHANNELS_MIN: u32 = 1;

const FRAME_HEADER_SYNC_ID: u32 = 0xA0FF;
const FRAME_HEADER_FLAGS_BITS: u32 = 3;
const FRAME_HEADER_NO_BITS: u32 = 21;
const FRAME_HEADER_SAMPLE_COUNT_BITS: u32 = 14;
const FRAME_DURATION_QUANT_SHIFT: u32 = 5;

/// The frame is the last one of the stream; its header holds its number of samples.
pub const FRAME_FLAG_IS_LAST: u32 = 0x1;
/// The frame header holds the stream information.
pub const FRAME_FLAG_HAS_INFO: u32 = 0x2;
/// The frame header holds metadata (unsupported by the reference decoder).
pub const FRAME_FLAG_HAS_METADATA: u32 = 0x4;

/// The codec type of mono and stereo streams.
pub const CODEC_MONO_STEREO: u32 = 2;
/// The codec type of multichannel streams.
pub const CODEC_MULTICHANNEL: u32 = 4;

/// The block types of the metadata of the file header.
pub const METADATA_END: u8 = 0;
pub const METADATA_STREAMINFO: u8 = 1;
pub const METADATA_MD5: u8 = 6;
pub const METADATA_LAST_FRAME: u8 = 7;

/// The most bytes a frame header holds (`TAK_MAX_FRAME_HEADER_BYTES`): 80 bits for the sync id,
/// the flags, the frame number, the CRC, and the size of the last frame; 182 bits for the largest
/// stream information (with the layout of 16 channels); and the optional 31 bits that follow it.
pub const MAX_FRAME_HEADER_BYTES: usize = (80usize + 182 + 31).div_ceil(8);

/// The initial value of the CRC-24 of TAK.
///
/// FFmpeg keeps this CRC as `av_crc(AV_CRC_24_IEEE, 0xCE04B7, ..)`, whose big endian tables hold
/// the register byte swapped: the equivalent most significant bit first register starts with the
/// bytes of that constant reversed, and holds a result that is stored in little endian order.
const CRC24_INIT: u32 = 0xB704CE;
/// The polynomial of the CRC-24 (`AV_CRC_24_IEEE`).
const CRC24_POLY: u32 = 0x864CFB;

const fn make_crc24_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 16;
        let mut j = 0;
        while j < 8 {
            c <<= 1;
            if c & 0x100_0000 != 0 {
                c ^= CRC24_POLY;
            }
            j += 1;
        }
        table[i] = c & 0xff_ffff;
        i += 1;
    }
    table
}

static CRC24_TABLE: [u32; 256] = make_crc24_table();

/// The CRC-24 of `buf`, as TAK uses it (`ff_tak_check_crc`).
pub fn crc24(buf: &[u8]) -> u32 {
    buf.iter().fold(CRC24_INIT, |crc, &b| {
        ((crc << 8) ^ CRC24_TABLE[(((crc >> 16) as u8) ^ b) as usize]) & 0xff_ffff
    })
}

/// Checks the CRC-24 held by the last 3 bytes of `buf` (little endian) against the other bytes.
pub fn check_crc(buf: &[u8]) -> bool {
    match buf.len().checked_sub(3) {
        Some(n) if n > 0 => {
            let stored = u32::from_le_bytes([buf[n], buf[n + 1], buf[n + 2], 0]);
            crc24(&buf[..n]) == stored
        }
        _ => false,
    }
}

/// The channel layout of TAK maps one of the 18 positions of a speaker to each channel, in the
/// order of the `WAVE_FORMAT_EXTENSIBLE` channel mask bits (index 1 is front left); this is the
/// `tak_channel_layouts` table of `tak.c`, whose entries are the matching `AV_CH_*` bits.
const NUM_LAYOUT_POSITIONS: u32 = 18;

/// The stream information of the file header and the frame headers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamInfo {
    pub codec: u32,
    pub data_type: u32,
    pub sample_rate: u32,
    pub bps: u32,
    pub channels: u32,
    /// The frame size in samples.
    pub frame_samples: u32,
    /// The channel layout as a `WAVE_FORMAT_EXTENSIBLE` channel mask (0 if the stream has none).
    pub ch_mask: u32,
    /// The number of samples of the stream (0 if unknown).
    pub samples: u64,
}

/// The sizes of the frames, in samples, that the frame size types 4 to 9 stand for, and
/// the quanta in 1/32 s of the types 0 to 3 (`frame_duration_type_quants`).
const FRAME_DURATION_TYPE_QUANTS: [u32; 10] = [3, 4, 6, 8, 4096, 8192, 16384, 512, 1024, 2048];

/// The frame size in samples of a frame size type (`tak_get_nb_samples`).
fn frame_samples_of(sample_rate: u32, frame_type: u32) -> Result<u32> {
    let quant = match FRAME_DURATION_TYPE_QUANTS.get(frame_type as usize) {
        Some(&q) => q,
        None => return decode_error("tak: invalid frame size type"),
    };

    let (nb_samples, max_nb_samples) = if frame_type <= 3 {
        // The sizes of the types 0 to 3 are times, 3/32 s to 8/32 s.
        ((u64::from(sample_rate) * u64::from(quant)) >> FRAME_DURATION_QUANT_SHIFT, 16384)
    }
    else {
        (
            u64::from(quant),
            (u64::from(sample_rate) * u64::from(FRAME_DURATION_TYPE_QUANTS[3]))
                >> FRAME_DURATION_QUANT_SHIFT,
        )
    };

    if nb_samples == 0 || nb_samples > max_nb_samples {
        return decode_error("tak: invalid frame size");
    }

    Ok(nb_samples as u32)
}

impl StreamInfo {
    /// Parses the stream information that starts at the current position of `br`
    /// (`tak_parse_streaminfo`).
    pub fn parse(br: &mut BitReader<'_>) -> Result<StreamInfo> {
        let codec = br.get(ENCODER_CODEC_BITS);
        br.skip(ENCODER_PROFILE_BITS as usize);

        let frame_type = br.get(SIZE_FRAME_DURATION_BITS);
        let samples = br.get64(SIZE_SAMPLES_NUM_BITS);

        let data_type = br.get(FORMAT_DATA_TYPE_BITS);
        let sample_rate = br.get(FORMAT_SAMPLE_RATE_BITS) + SAMPLE_RATE_MIN;
        let bps = br.get(FORMAT_BPS_BITS) + BPS_MIN;
        let channels = br.get(FORMAT_CHANNEL_BITS) + CHANNELS_MIN;

        let mut ch_mask = 0;

        if br.get1() {
            br.skip(FORMAT_VALID_BITS);
            if br.get1() {
                for _ in 0..channels {
                    let value = br.get(FORMAT_CH_LAYOUT_BITS);
                    if (1..=NUM_LAYOUT_POSITIONS).contains(&value) {
                        ch_mask |= 1 << (value - 1);
                    }
                }
            }
        }

        if br.bits_left() < 0 {
            return decode_error("tak: truncated stream info");
        }

        let frame_samples = frame_samples_of(sample_rate, frame_type)?;

        Ok(StreamInfo {
            codec,
            data_type,
            sample_rate,
            bps,
            channels,
            frame_samples,
            ch_mask,
            samples,
        })
    }

    /// Parses the stream information held by the `STREAMINFO` block of the file header.
    pub fn from_bytes(buf: &[u8]) -> Result<StreamInfo> {
        StreamInfo::parse(&mut BitReader::new(buf))
    }
}

/// A frame header.
#[derive(Clone, Debug)]
pub struct FrameHeader {
    pub flags: u32,
    pub frame_num: u32,
    /// The number of samples of the frame if it is the last of the stream, otherwise 0.
    pub last_frame_samples: u32,
    /// The stream information that the header holds, if it does.
    pub info: Option<StreamInfo>,
    /// The size of the header in bytes, including its CRC.
    pub size: usize,
}

impl FrameHeader {
    /// Parses and validates the frame header at the start of `buf` (`ff_tak_decode_frame_header`),
    /// and checks its CRC (`ff_tak_check_crc`).
    pub fn parse(buf: &[u8]) -> Result<FrameHeader> {
        let mut br = BitReader::new(buf);

        if br.get(16) != FRAME_HEADER_SYNC_ID {
            return decode_error("tak: missing sync id");
        }

        let flags = br.get(FRAME_HEADER_FLAGS_BITS);
        let frame_num = br.get(FRAME_HEADER_NO_BITS);

        let last_frame_samples = if flags & FRAME_FLAG_IS_LAST != 0 {
            let n = br.get(FRAME_HEADER_SAMPLE_COUNT_BITS) + 1;
            br.skip(2);
            n
        }
        else {
            0
        };

        let info = if flags & FRAME_FLAG_HAS_INFO != 0 {
            let info = StreamInfo::parse(&mut br)?;

            if br.get(6) != 0 {
                br.skip(25);
            }
            br.align();

            Some(info)
        }
        else {
            None
        };

        if flags & FRAME_FLAG_HAS_METADATA != 0 {
            return decode_error("tak: frame metadata is unsupported");
        }

        if br.bits_left() < 24 {
            return decode_error("tak: truncated frame header");
        }
        br.skip(24);

        // The position is on a byte boundary: either aligned after the stream information, or
        // the fixed part (16 + 3 + 21 bits, then 24 bits for the CRC) is 8 bit multiples, which
        // holds for the last frame count (16 bits) too.
        let size = br.bit_pos() / 8;

        if !check_crc(&buf[..size]) {
            return decode_error("tak: frame header crc mismatch");
        }

        Ok(FrameHeader { flags, frame_num, last_frame_samples, info, size })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_crc24_of_a_streaminfo_block() {
        // The stream information, and its CRC, of the TAK 2.2.0 sample of the FFmpeg samples
        // archive (A-codecs/tak/lostchord-tak_2_2_0.tak).
        let block = [0x82, 0x04, 0x80, 0x52, 0x15, 0x00, 0x40, 0x4d, 0x09, 0x02, 0xb7, 0xa8, 0xa2];
        assert!(check_crc(&block));

        let info = StreamInfo::from_bytes(&block[..10]).unwrap();
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 1);
        assert_eq!(info.bps, 16);
    }

    #[test]
    fn verify_frame_sizes() {
        // 3/32 s, 4/32 s, 6/32 s and 8/32 s at 44.1 kHz.
        assert_eq!(frame_samples_of(44100, 0).unwrap(), 4134);
        assert_eq!(frame_samples_of(44100, 3).unwrap(), 11025);
        assert_eq!(frame_samples_of(44100, 4).unwrap(), 4096);
        assert_eq!(frame_samples_of(44100, 5).unwrap(), 8192);
        // 16384 samples are more than 1/4 s at 44.1 kHz.
        assert!(frame_samples_of(44100, 6).is_err());
        assert_eq!(frame_samples_of(96000, 6).unwrap(), 16384);
        assert!(frame_samples_of(44100, 10).is_err());
        // A fixed size can not exceed 1/4 s.
        assert!(frame_samples_of(8000, 6).is_err());
    }
}
