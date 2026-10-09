// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::{
    audio::{Channels, Position},
    errors::{Result, decode_error, unsupported_error},
    io::ReadBytes,
};

/// The seek pre-roll, in frames at 48 kHz, that makes the output after a seek identical to a
/// continuous decode for streams without SILK frames (CELT-only): 1.5 s.
///
/// RFC 7845 section 4.6 only mandates 80 ms, enough to bring the decoder *close* to the continuous
/// decode, but the state of an Opus decoder never fully forgets a reset that quickly: the CELT
/// inter-frame energy prediction (and the post-filter and overlap state with it) converges
/// geometrically and the output becomes bit-identical to a continuous decode after about 640-960
/// ms, independent of the frame size (measured against libopus 1.6.1 on 2.5 to 120 ms frames).
pub const SEEK_PREROLL_CELT: u64 = 72_000;

/// The seek pre-roll, in frames at 48 kHz, for streams containing SILK or Hybrid frames: 10 s.
///
/// SILK is different. Its decoder is a fixed-point recursion (LPC synthesis plus the long-term
/// predictor fed back from the output history) whose rounding makes two trajectories with
/// different initial states settle into persistently
/// different orbits instead of contracting to the same one. Even libopus itself does not reach a
/// continuous decode after a cold start on loud (high gain) material, however long the pre-roll.
/// For quiet or narrowband material the trajectories do merge: measured against libopus 1.6.1,
/// the fraction of seeks that are bit-identical to a continuous decode grows with the pre-roll
/// (narrowband speech: 8/25 at 0.64 s, 17/25 at 1.3 s, 21/25 at 2.6 s, 25/25 at 10 s; wideband
/// SILK: 0/25, 3/25, 11/25 and 23/25), and the median SNR plateaus at 45-48 dB after about 8
/// frames otherwise. Pre-rolling 10 s is the longest practical choice: decoding SILK is several
/// hundred times faster than real time.
pub const SEEK_PREROLL_SILK: u64 = 480_000;

/// Returns the seek pre-roll in frames at 48 kHz for a stream that does (`silk`) or does not
/// contain SILK or Hybrid frames.
pub fn seek_preroll(silk: bool) -> u64 {
    if silk { SEEK_PREROLL_SILK } else { SEEK_PREROLL_CELT }
}

/// Returns `true` if the Opus packet with the first byte (table-of-contents byte) `toc` carries
/// SILK data (RFC 6716 section 3.1: configurations 0 to 11 are SILK-only, 12 to 15 Hybrid).
pub fn toc_has_silk(toc: u8) -> bool {
    toc >> 3 < 16
}

const OPUS_MAGIC_SIGNATURE: &[u8] = b"OpusHead";

#[derive(Debug, Default)]
pub struct OpusHead {
    pub version: u8,
    pub channels: Channels,
    pub original_sample_rate: u32,
    pub gain: i16,
    pub pre_skip: u16,
}

impl OpusHead {
    pub fn read<B: ReadBytes>(reader: &mut B, max_version: u8) -> Result<Self> {
        // The first 8 bytes are the magic signature ASCII bytes.
        let mut magic = [0; 8];
        reader.read_buf_exact(&mut magic)?;

        if magic != *OPUS_MAGIC_SIGNATURE {
            return unsupported_error("common (opus): invalid magic signature");
        }

        // The next byte is the encapsulation version. The max version is specified by the caller
        // since it depends on the container format used.
        let version = reader.read_byte()?;
        if version > max_version {
            return decode_error("common (opus): invalid version");
        }

        // The next byte is the number of channels and must not be 0.
        let channel_count = reader.read_byte()?;

        if channel_count == 0 {
            return decode_error("common (opus): invalid channel count");
        }

        // The next 16-bit integer is the pre-skip padding (# of samples at 48kHz to subtract from
        // the OGG granule position to obtain the PCM sample position).
        let pre_skip = reader.read_u16()?;

        // The next 32-bit integer is the sample rate of the original audio.
        let original_sample_rate = reader.read_u32()?;

        // Next, the 16-bit gain value.
        let gain = reader.read_i16()?;

        // The next byte indicates the channel mapping. Most of these values are reserved.
        let channel_mapping = reader.read_byte()?;

        // Families 2 (ambisonics, RFC 8486) and 255 (undefined) have no defined speaker positions.
        // The channels are presented as discrete channels in mapping table order.
        if channel_mapping == 2 || channel_mapping == 255 {
            return Ok(Self {
                version,
                channels: Channels::Discrete(u16::from(channel_count)),
                gain,
                original_sample_rate,
                pre_skip,
            });
        }

        let positions = match channel_mapping {
            // RTP Mapping
            0 if channel_count == 1 => Position::FRONT_LEFT,
            0 if channel_count == 2 => Position::FRONT_LEFT | Position::FRONT_RIGHT,
            // Vorbis Mapping
            1 => match channel_count {
                1 => Position::FRONT_LEFT,
                2 => Position::FRONT_LEFT | Position::FRONT_RIGHT,
                3 => Position::FRONT_LEFT | Position::FRONT_CENTER | Position::FRONT_RIGHT,
                4 => {
                    Position::FRONT_LEFT
                        | Position::FRONT_RIGHT
                        | Position::REAR_LEFT
                        | Position::REAR_RIGHT
                }
                5 => {
                    Position::FRONT_LEFT
                        | Position::FRONT_CENTER
                        | Position::FRONT_RIGHT
                        | Position::REAR_LEFT
                        | Position::REAR_RIGHT
                }
                6 => {
                    Position::FRONT_LEFT
                        | Position::FRONT_CENTER
                        | Position::FRONT_RIGHT
                        | Position::REAR_LEFT
                        | Position::REAR_RIGHT
                        | Position::LFE1
                }
                7 => {
                    Position::FRONT_LEFT
                        | Position::FRONT_CENTER
                        | Position::FRONT_RIGHT
                        | Position::SIDE_LEFT
                        | Position::SIDE_RIGHT
                        | Position::REAR_CENTER
                        | Position::LFE1
                }
                8 => {
                    Position::FRONT_LEFT
                        | Position::FRONT_CENTER
                        | Position::FRONT_RIGHT
                        | Position::SIDE_LEFT
                        | Position::SIDE_RIGHT
                        | Position::REAR_LEFT
                        | Position::REAR_RIGHT
                        | Position::LFE1
                }
                _ => return decode_error("common (opus): invalid vorbis channel mapping"),
            },
            // Reserved, and should NOT be supported for playback.
            _ => return unsupported_error("common (opus): unsupported channel mapping family"),
        };

        Ok(Self {
            version,
            channels: Channels::Positioned(positions),
            gain,
            original_sample_rate,
            pre_skip,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_core::io::BufReader;

    fn head(channels: u8, family: u8) -> Vec<u8> {
        let mut v = b"OpusHead".to_vec();
        v.extend_from_slice(&[1, channels]);
        v.extend_from_slice(&312u16.to_le_bytes());
        v.extend_from_slice(&48_000u32.to_le_bytes());
        v.extend_from_slice(&0i16.to_le_bytes());
        v.push(family);
        if family != 0 {
            v.extend_from_slice(&[u8::from(channels), 0]);
            v.extend(0..channels);
        }
        v
    }

    #[test]
    fn family_255_and_2_are_discrete() {
        for (ch, family) in [(3, 255), (11, 255), (16, 2)] {
            let buf = head(ch, family);
            let h = OpusHead::read(&mut BufReader::new(&buf), 15).unwrap();
            assert_eq!(h.channels, Channels::Discrete(u16::from(ch)));
            assert_eq!(h.pre_skip, 312);
        }
    }

    #[test]
    fn family_1_is_positioned() {
        let buf = head(6, 1);
        let h = OpusHead::read(&mut BufReader::new(&buf), 15).unwrap();
        assert!(matches!(h.channels, Channels::Positioned(_)));
    }
}
