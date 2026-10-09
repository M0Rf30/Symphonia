// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sample accurate timelines for codecs that carry the duration of a packet in the packet.
//!
//! The timestamp of a Matroska block is only as precise as the timestamp scale of the segment
//! (usually 1ms, or 44 frames at 44.1kHz). Codecs with a constant frame duration can recover the
//! exact timestamp of a block from the grid the blocks lie on, but the duration of a Vorbis
//! packet depends on the block sizes of the packet and of the packet before it, and the duration
//! of an Opus packet on its TOC byte. For these codecs the exact start of a packet is the exact
//! end of the previous packet: the timeline is the running sum of the durations of the packets.
//! The block timestamps are then only used to anchor the timeline to the start of the stream, and
//! to detect gaps in it.

use symphonia_common::xiph::audio::vorbis::blocks::VorbisBlocks;
use symphonia_common::xiph::audio::vorbis::unpack_xiph_laced_extradata;

/// A codec for which the duration of a packet can be determined from the packet.
#[derive(Clone, Debug)]
pub(crate) enum PacketDurations {
    Vorbis(VorbisBlocks),
    Opus,
}

/// The block size, or lack thereof, of the packet preceding a packet.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Prev {
    /// The packet is the first packet of the stream.
    Start,
    /// The preceding packet is not known (e.g., after a seek without an index of the stream).
    Unknown,
    /// The preceding packet has a block size of `1 << exp`.
    Block(u8),
}

/// The exact position within a stream of the next packet of a track, and the state needed to
/// calculate its duration.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Cursor {
    /// The timestamp, in frames, of the start of the next packet. `None` if the timeline has yet
    /// to be anchored to the timestamp of the next block.
    pub(crate) next_pts: Option<i64>,
    pub(crate) prev: Prev,
    /// The timestamp is not exact, because the duration of the packet before it was a guess.
    pub(crate) approx: bool,
}

impl Cursor {
    /// The cursor for the start of a stream.
    pub(crate) const START: Cursor = Cursor { next_pts: None, prev: Prev::Start, approx: false };
    /// The cursor for a random position in a stream.
    pub(crate) const UNKNOWN: Cursor =
        Cursor { next_pts: None, prev: Prev::Unknown, approx: false };
}

/// The timing of a packet.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct PacketTiming {
    /// The number of frames decoded from the packet, including the frames that are to be
    /// discarded.
    pub(crate) dur: u64,
    /// The number of frames at the start of the decoded frames that are to be discarded.
    pub(crate) lead: u64,
    /// The packet is the first of the stream.
    pub(crate) is_start: bool,
    /// The duration is a guess, because the packet before the packet is not known.
    pub(crate) approx: bool,
}

impl PacketDurations {
    /// Get the packet duration parser of a codec, if the codec has one.
    pub(crate) fn new(codec_id: &str, codec_private: Option<&[u8]>) -> Option<Self> {
        match codec_id {
            "A_OPUS" => Some(PacketDurations::Opus),
            "A_VORBIS" => {
                let (ident, setup) = unpack_xiph_laced_extradata(codec_private?).ok()?;

                match VorbisBlocks::from_headers(ident, setup) {
                    Ok(blocks) => Some(PacketDurations::Vorbis(blocks)),
                    Err(err) => {
                        log::debug!("mkv: unable to parse the vorbis headers ({err})");
                        None
                    }
                }
            }
            _ => None,
        }
    }

    /// If the codec discards the frames of its delay itself: they are not decoded from the
    /// packets of the stream, and are already trimmed from the first packet.
    pub(crate) fn discards_delay(&self) -> bool {
        matches!(self, PacketDurations::Vorbis(_))
    }

    /// Get the difference, in frames, between the exact start of a packet and the timestamp of its
    /// block that is not taken for a break in the timeline, in addition to the precision of the
    /// timestamp.
    ///
    /// Muxers disagree on which frames of a Vorbis packet its timestamp is for: the frames that
    /// are decoded from it, or the ones in the window of the packet, which start up to half a
    /// long block earlier.
    pub(crate) fn timestamp_slack(&self) -> u64 {
        match self {
            PacketDurations::Vorbis(blocks) => (1u64 << blocks.bs1_exp) >> 1,
            PacketDurations::Opus => 0,
        }
    }

    /// Get the timing of a packet, and advance the state of the packet that precedes the next
    /// packet. Returns `None` if the packet is not an audio packet, and the state is unchanged.
    pub(crate) fn advance(&self, prev: &mut Prev, packet: &[u8]) -> Option<PacketTiming> {
        match self {
            PacketDurations::Vorbis(blocks) => {
                let exp = blocks.packet_block_exp(packet)?;

                let timing = match *prev {
                    // The first packet has nothing to overlap with. Its output is discarded.
                    Prev::Start => {
                        let half = (1u64 << exp) >> 1;
                        PacketTiming { dur: half, lead: half, is_start: true, approx: false }
                    }
                    // Not knowing the previous packet, assume it had the same block size.
                    Prev::Unknown => PacketTiming {
                        dur: (1u64 << exp) >> 1,
                        lead: 0,
                        is_start: false,
                        approx: true,
                    },
                    Prev::Block(prev_exp) => PacketTiming {
                        dur: VorbisBlocks::packet_duration(prev_exp, exp),
                        lead: 0,
                        is_start: false,
                        approx: false,
                    },
                };

                *prev = Prev::Block(exp);
                Some(timing)
            }
            PacketDurations::Opus => opus_packet_duration(packet).map(|dur| PacketTiming {
                dur,
                lead: 0,
                is_start: false,
                approx: false,
            }),
        }
    }
}

/// Get the number of frames (at 48kHz) of an Opus packet from its TOC (RFC 6716 section 3.1).
fn opus_packet_duration(packet: &[u8]) -> Option<u64> {
    let &toc = packet.first()?;

    // The frame duration in units of 2.5ms is selected by the configuration number.
    let frame = match toc >> 3 {
        // SILK-only: 10, 20, 40, 60ms.
        config @ 0..=11 => [4, 8, 16, 24][usize::from(config & 3)],
        // Hybrid: 10, 20ms.
        config @ 12..=15 => [4, 8][usize::from(config & 1)],
        // CELT-only: 2.5, 5, 10, 20ms.
        config => [1, 2, 4, 8][usize::from(config & 3)],
    };

    let num_frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u64::from(*packet.get(1)? & 0x3f),
    };

    // 2.5ms is 120 frames at 48kHz.
    Some(num_frames * frame * 120)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_durations() {
        // (TOC, frames).
        let cases = [
            (0x00, 480),  // SILK NB 10ms.
            (0x08, 960),  // SILK NB 20ms.
            (0x10, 1920), // 40ms.
            (0x18, 2880), // 60ms.
            (0x58, 2880), // SILK WB 60ms.
            (0x60, 480),  // Hybrid SWB 10ms.
            (0x68, 960),  // Hybrid SWB 20ms.
            (0x78, 960),  // Hybrid FB 20ms.
            (0x80, 120),  // CELT NB 2.5ms.
            (0x88, 240),  // 5ms.
            (0x90, 480),  // 10ms.
            (0x98, 960),  // 20ms.
            (0xf8, 960),  // CELT FB 20ms.
            (0x99, 1920), // Two frames, same size.
            (0x9a, 1920), // Two frames, different sizes.
        ];

        for (toc, frames) in cases {
            assert_eq!(opus_packet_duration(&[toc, 0]), Some(frames), "toc={toc:#010b}");
        }

        // Code 3: the second byte is the frame count.
        assert_eq!(opus_packet_duration(&[0x9b, 0b0000_0101]), Some(5 * 960));
        // The VBR and padding flags are ignored.
        assert_eq!(opus_packet_duration(&[0x9b, 0b1100_0110]), Some(6 * 960));
        // 120ms.
        assert_eq!(opus_packet_duration(&[0x1b, 2]), Some(5760));
        // Truncated.
        assert_eq!(opus_packet_duration(&[0x9b]), None);
        assert_eq!(opus_packet_duration(&[]), None);
    }

    #[test]
    fn vorbis_timeline() {
        let blocks = VorbisBlocks { bs0_exp: 8, bs1_exp: 11, num_modes: 2, mode_block_flags: 0b10 };
        let durations = PacketDurations::Vorbis(blocks);

        let short = [0b00u8, 0];
        let long = [0b10u8, 0];
        let header = [0x01u8, 0];

        let mut prev = Prev::Start;

        // The first packet is discarded entirely.
        assert_eq!(
            durations.advance(&mut prev, &short),
            Some(PacketTiming { dur: 128, lead: 128, is_start: true, approx: false })
        );
        assert_eq!(prev, Prev::Block(8));

        // Not an audio packet.
        assert_eq!(durations.advance(&mut prev, &header), None);
        assert_eq!(prev, Prev::Block(8));

        let t = |dur| Some(PacketTiming { dur, lead: 0, is_start: false, approx: false });
        assert_eq!(durations.advance(&mut prev, &short), t(128));
        assert_eq!(durations.advance(&mut prev, &long), t(64 + 512));
        assert_eq!(durations.advance(&mut prev, &long), t(1024));
        assert_eq!(durations.advance(&mut prev, &short), t(512 + 64));

        let mut prev = Prev::Unknown;
        assert_eq!(
            durations.advance(&mut prev, &long),
            Some(PacketTiming { dur: 1024, lead: 0, is_start: false, approx: true })
        );
        assert_eq!(prev, Prev::Block(11));
    }
}
