// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `ALSSpecificConfig` (ISO/IEC 14496-3 subpart 11, Table 11.1) as read by FFmpeg
//! `libavcodec/alsdec.c` `read_specific_config`.

use symphonia_core::errors::{Result, decode_error, limit_error, unsupported_error};

use crate::bits::Bits;

/// The most channels that are accepted (FFmpeg `FF_SANE_NB_CHANNELS`).
pub const MAX_CHANNELS: usize = 512;

/// The MPEG-4 audio object type of ALS.
const AOT_ALS: u32 = 36;

/// The "AudioSpecificConfig" escape of the object type: the type is `32 + 6 more bits`.
const AOT_ESCAPE: u32 = 31;

/// The mandatory fields of the config are at least 30 bytes.
const MIN_CONFIG_BITS: i64 = 30 * 8;

/// The `ceil(log2(x))` of FFmpeg `av_ceil_log2`.
pub fn ceil_log2(x: u32) -> u32 {
    if x <= 1 { 0 } else { 32 - (x - 1).leading_zeros() }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub sample_rate: u32,
    /// The number of samples per channel, `0xffff_ffff` if unknown.
    pub samples: u32,
    pub channels: usize,
    /// 0: 8-bit, 1: 16-bit, 2: 24-bit, 3: 32-bit.
    pub resolution: u32,
    pub floating: bool,
    /// The original CRC was calculated on big-endian data.
    pub msb_first: bool,
    pub frame_length: usize,
    pub ra_distance: u32,
    /// 0: no size of the random access units, 1: in the frames, 2: in the header.
    pub ra_flag: u32,
    pub adapt_order: bool,
    pub coef_table: u32,
    pub long_term_prediction: bool,
    pub max_order: usize,
    pub block_switching: u32,
    pub bgmc: bool,
    pub sb_part: bool,
    pub joint_stereo: bool,
    pub mc_coding: bool,
    pub rlslms: bool,
    /// The stored CRC-32 of the original samples, if `crc_enabled`.
    pub crc: Option<u32>,
    /// `chan_pos[i]` is the decoded channel to output as channel `i`, if channels are rearranged.
    pub chan_pos: Option<Vec<usize>>,
    /// The number of bits of a sample of the decoded integer signal.
    pub bits_per_sample: u32,
}

impl Config {
    /// Parse the codec extra data: either the `AudioSpecificConfig` of object type ALS followed by
    /// the `ALSSpecificConfig`, or the bare `ALSSpecificConfig` as at the start of an `.als` file.
    pub fn parse(extra_data: &[u8]) -> Result<Config> {
        let mut bits = Bits::new(extra_data);

        if !extra_data.starts_with(b"ALS\0") {
            // AudioSpecificConfig: object type, sampling frequency, channel configuration.
            let mut object_type = bits.get(5);
            if object_type == AOT_ESCAPE {
                object_type = 32 + bits.get(6);
            }
            if object_type != AOT_ALS {
                return unsupported_error("als: not an ALS audio specific config");
            }
            if bits.get(4) == 0xf {
                bits.skip(24);
            }
            bits.skip(4);

            // fillBits. Old conformance streams have three more bytes before the ALS ID.
            bits.skip(5);
            if bits.get(24) == 0x41_4c_53 {
                // "ALS": the ID starts here.
                bits.rewind(24);
            }
        }

        Self::parse_specific(&mut bits)
    }

    fn parse_specific(bits: &mut Bits<'_>) -> Result<Config> {
        if bits.left() < MIN_CONFIG_BITS {
            return decode_error("als: config too short");
        }

        if bits.get(32) != 0x414c_5300 {
            return decode_error("als: missing ALS id");
        }

        let sample_rate = bits.get(32);
        let samples = bits.get(32);
        let channels = bits.get(16) as usize + 1;
        bits.skip(3); // file_type
        let resolution = bits.get(3);
        let floating = bits.get1() != 0;
        let msb_first = bits.get1() != 0;
        let frame_length = bits.get(16) as usize + 1;
        let ra_distance = bits.get(8);
        let ra_flag = bits.get(2);
        let adapt_order = bits.get1() != 0;
        let coef_table = bits.get(2);
        let long_term_prediction = bits.get1() != 0;
        let max_order = bits.get(10) as usize;
        let block_switching = bits.get(2);
        let bgmc = bits.get1() != 0;
        let sb_part = bits.get1() != 0;
        let joint_stereo = bits.get1() != 0;
        let mc_coding = bits.get1() != 0;
        let chan_config = bits.get1() != 0;
        let chan_sort = bits.get1() != 0;
        let crc_enabled = bits.get1() != 0;
        let rlslms = bits.get1() != 0;
        bits.skip(5); // reserved
        bits.skip(1); // aux_data_enabled

        if sample_rate == 0 || sample_rate > i32::MAX as u32 {
            return decode_error("als: invalid sample rate");
        }

        if channels > MAX_CHANNELS {
            return limit_error("als: too many channels");
        }

        if chan_config {
            bits.skip(16); // chan_config_info
        }

        let mut chan_pos = None;

        if chan_sort && channels > 1 {
            let chan_pos_bits = ceil_log2(channels as u32);
            if bits.left() < (channels as i64) * i64::from(chan_pos_bits) + 7 {
                return decode_error("als: config too short");
            }

            let mut pos = vec![usize::MAX; channels];
            let mut valid = true;

            for i in 0..channels {
                let idx = bits.get(chan_pos_bits) as usize;
                if idx >= channels || pos[idx] != usize::MAX {
                    // Invalid channel reordering: ignored.
                    valid = false;
                    break;
                }
                pos[idx] = i;
            }

            if valid {
                chan_pos = Some(pos);
            }

            bits.align();
        }

        // Header and trailer sizes: 0xffff_ffff means that there is no such data.
        if bits.left() < 64 {
            return decode_error("als: config too short");
        }

        let header_size = bits.get(32);
        let trailer_size = bits.get(32);
        let header_size = if header_size == u32::MAX { 0 } else { header_size };
        let trailer_size = if trailer_size == u32::MAX { 0 } else { trailer_size };
        let ht_bits = (u64::from(header_size) + u64::from(trailer_size)) << 3;

        if bits.left() < ht_bits as i64 {
            return decode_error("als: config too short");
        }
        if ht_bits > i32::MAX as u64 {
            return unsupported_error("als: header and trailer too large");
        }
        bits.skip(ht_bits as usize);

        let mut crc = None;
        if crc_enabled {
            if bits.left() < 32 {
                return decode_error("als: config too short");
            }
            crc = Some(bits.get(32));
        }

        // The `ra_unit_size` and auxiliary data fields are not needed.

        let bits_per_sample = if floating { 32 } else { (resolution + 1) * 8 };
        if bits_per_sample > 32 {
            return decode_error("als: invalid resolution");
        }

        Ok(Config {
            sample_rate,
            samples,
            channels,
            resolution,
            floating,
            msb_first,
            frame_length,
            ra_distance,
            ra_flag,
            adapt_order,
            coef_table,
            long_term_prediction,
            max_order,
            block_switching,
            bgmc,
            sb_part,
            joint_stereo,
            mc_coding,
            rlslms,
            crc,
            chan_pos,
            bits_per_sample,
        })
    }
}
