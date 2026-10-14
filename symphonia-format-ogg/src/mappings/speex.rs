// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Speex-in-Ogg mapping.
//!
//! The header layout is the `SpeexHeader` structure of libspeex (`speex_header.h`), and the
//! stream layout (header packet, Vorbis comment packet, `extra_headers` additional header
//! packets, then audio packets) is described by the Speex manual, "Ogg file format".

use crate::common::SideData;

use super::{MapResult, Mapper, PacketParser};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_SPEEX;
use symphonia_core::errors::Result;
use symphonia_core::formats::Track;
use symphonia_core::io::BufReader;
use symphonia_core::meta::MetadataBuilder;
use symphonia_core::units::Duration;
use symphonia_metadata::embedded::vorbis::{self, VORBIS_COMMENT_METADATA_INFO};

use symphonia_core::audio::{Channels, Position};

/// The signature of the Speex header packet.
const SPEEX_SIGNATURE: &[u8; 8] = b"Speex   ";

/// The size of the Speex header packet (`sizeof(SpeexHeader)`).
const SPEEX_HEADER_SIZE: usize = 80;

/// The maximum number of extra header packets that will be skipped. A real stream has none or
/// a handful (e.g. Skeleton-like metadata); this only bounds a corrupt count.
const SPEEX_MAX_EXTRA_HEADERS: u32 = 1 << 16;

/// The mapper-relevant fields of the Speex header packet.
#[derive(Copy, Clone)]
struct SpeexHeader {
    /// The encoder lookahead, in samples. The granule position of every page excludes this many
    /// leading samples (see `encoder_lookahead`).
    lookahead: u32,
    /// Samples per channel in one Speex frame (160 narrowband, 320 wideband, 640 ultra-wideband).
    frame_size: u32,
    /// Frames in each Ogg audio packet.
    frames_per_packet: u32,
    /// Number of header packets that follow the comment packet.
    extra_headers: u32,
}

impl SpeexHeader {
    /// The number of samples per channel decoded from an audio packet.
    fn packet_dur(&self) -> u64 {
        u64::from(self.frame_size) * u64::from(self.frames_per_packet)
    }
}

/// The encoder lookahead of each Speex mode (`speex_encoder_ctl(SPEEX_GET_LOOKAHEAD)`: narrowband
/// 40, wideband `2 * 40 + 63`, ultra-wideband `2 * 143 + 63` samples). `speexenc` and libavformat
/// both subtract it from the granule position, so the first audio packet of a stream starts at
/// `-lookahead` and these samples are the start delay. Verified against `speexenc` 1.2.1 output
/// (first page granule = `n * frame_size - lookahead`) and `libspeex` itself.
fn encoder_lookahead(mode: u32) -> u32 {
    match mode {
        0 => 40,
        1 => 143,
        2 => 349,
        _ => 0,
    }
}

fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]])
}

pub fn detect(serial: u32, buf: &[u8]) -> Result<Option<Box<dyn Mapper>>> {
    if buf.len() < SPEEX_HEADER_SIZE || buf[..8] != *SPEEX_SIGNATURE {
        return Ok(None);
    }

    // Field offsets: 8 speex_version[20], 28 speex_version_id, 32 header_size, 36 rate, 40 mode,
    // 44 mode_bitstream_version, 48 nb_channels, 52 bitrate, 56 frame_size, 60 vbr,
    // 64 frames_per_packet, 68 extra_headers, 72 reserved1, 76 reserved2. All little-endian.
    let header_size = read_u32_le(buf, 32);
    let rate = read_u32_le(buf, 36);
    let mode = read_u32_le(buf, 40);
    let nb_channels = read_u32_le(buf, 48);
    let frame_size = read_u32_le(buf, 56);
    let frames_per_packet = read_u32_le(buf, 64);
    let extra_headers = read_u32_le(buf, 68);

    // Speex supports one (mono) or two (intensity-stereo coded) channels. A header that does not
    // describe a decodable stream is not mapped as Speex.
    if (header_size as usize) < SPEEX_HEADER_SIZE
        || rate == 0
        || !(1..=2).contains(&nb_channels)
        || frame_size == 0
        || extra_headers > SPEEX_MAX_EXTRA_HEADERS
    {
        return Ok(None);
    }

    let channels = if nb_channels == 1 {
        Channels::Positioned(Position::FRONT_LEFT)
    }
    else {
        Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
    };

    let header = SpeexHeader {
        lookahead: encoder_lookahead(mode),
        frame_size,
        // The manual requires at least one frame per packet; treat 0 as 1 like libspeex does when
        // decoding a packet (it always decodes at least one frame).
        frames_per_packet: frames_per_packet.max(1),
        extra_headers,
    };

    let mut codec_params = AudioCodecParameters::new();

    codec_params
        .for_codec(CODEC_ID_SPEEX)
        .with_sample_rate(rate)
        .with_channels(channels)
        .with_max_frames_per_packet(header.packet_dur())
        .with_extra_data(Box::from(buf));

    let mut track = Track::new(serial);

    track.with_codec_params(CodecParameters::Audio(codec_params)).with_delay(header.lookahead);

    Ok(Some(Box::new(SpeexMapper {
        track,
        header,
        packets_seen: 0,
        parser: SpeexPacketParser::new(header),
    })))
}

/// Every Speex audio packet decodes to `frames_per_packet * frame_size` samples. A final packet
/// that carries fewer frames (padded with a terminator) is trimmed to the page's granule
/// position by the logical stream, exactly like the padding of other codecs. The encoder
/// lookahead is reported as the discard of the first packets of the stream.
struct SpeexPacketParser {
    dur: u64,
    /// The start delay that has not yet been reported as discard.
    remaining_discard: u64,
}

impl SpeexPacketParser {
    fn new(header: SpeexHeader) -> Self {
        SpeexPacketParser {
            dur: header.packet_dur(),
            remaining_discard: u64::from(header.lookahead),
        }
    }
}

impl PacketParser for SpeexPacketParser {
    fn parse_next_packet_dur(&mut self, _: &[u8]) -> (Duration, Duration) {
        let discard = self.remaining_discard.min(self.dur);
        self.remaining_discard -= discard;
        (Duration::new(self.dur), Duration::new(discard))
    }
}

struct SpeexMapper {
    track: Track,
    header: SpeexHeader,
    /// The number of packets mapped since the identification packet, up to the first audio packet.
    packets_seen: u32,
    parser: SpeexPacketParser,
}

impl Mapper for SpeexMapper {
    fn name(&self) -> &'static str {
        "speex"
    }

    fn reset(&mut self) {
        // Nothing to do. The start delay applies once, at the true start of the stream, and
        // header packets are only ever mapped while the physical stream starts.
    }

    fn reset_at_start(&mut self) {
        // The seek landed on the first audio page. The start delay applies again.
        self.parser = SpeexPacketParser::new(self.header);
    }

    fn max_rap_period(&self) -> Duration {
        // The Speex decoder carries the excitation history of the adaptive codebook (at most one
        // narrowband frame, 20 ms, back), and the synthesis filter memory. A packet is the
        // smallest unit that can be decoded, so seeks land `frames_per_packet` frames early, with
        // at least two frames, so that the decoder reaches a settled state by the target.
        let frames = self.header.frames_per_packet.max(2);
        Duration::new(u64::from(self.header.frame_size) * u64::from(frames))
    }

    fn track(&self) -> &Track {
        &self.track
    }

    fn track_mut(&mut self) -> &mut Track {
        &mut self.track
    }

    fn make_parser(&self) -> Option<Box<dyn PacketParser>> {
        Some(Box::new(SpeexPacketParser::new(self.header)))
    }

    fn map_packet(&mut self, packet: &[u8]) -> Result<MapResult> {
        // The comment packet is the first packet after the header packet, followed by
        // `extra_headers` more header packets. Everything after that is audio.
        let num_header_packets = 1 + u64::from(self.header.extra_headers);

        if u64::from(self.packets_seen) >= num_header_packets {
            let (dur, discard) = self.parser.parse_next_packet_dur(packet);

            return Ok(MapResult::StreamData { dur, discard });
        }

        self.packets_seen += 1;

        if self.packets_seen == 1 {
            // The comment packet is a bare Vorbis comment: no signature, no framing bit.
            let mut builder = MetadataBuilder::new(VORBIS_COMMENT_METADATA_INFO);
            let mut side_data = Default::default();

            vorbis::read_vorbis_comment(&mut BufReader::new(packet), &mut builder, &mut side_data)?;

            let rev = builder.build();

            return Ok(MapResult::SideData { data: SideData::Metadata { rev, side_data } });
        }

        Ok(MapResult::Setup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(frame_size: u32, frames_per_packet: u32, extra_headers: u32) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(SPEEX_SIGNATURE);
        h.extend_from_slice(&[0; 20]); // version string
        h.extend_from_slice(&1u32.to_le_bytes()); // version id
        h.extend_from_slice(&80u32.to_le_bytes()); // header size
        h.extend_from_slice(&8000u32.to_le_bytes()); // rate
        h.extend_from_slice(&1u32.to_le_bytes()); // mode: wideband
        h.extend_from_slice(&4u32.to_le_bytes()); // mode bitstream version
        h.extend_from_slice(&1u32.to_le_bytes()); // channels
        h.extend_from_slice(&(-1i32).to_le_bytes()); // bitrate
        h.extend_from_slice(&frame_size.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes()); // vbr
        h.extend_from_slice(&frames_per_packet.to_le_bytes());
        h.extend_from_slice(&extra_headers.to_le_bytes());
        h.extend_from_slice(&[0; 8]); // reserved
        h
    }

    #[test]
    fn header_fields_and_packet_duration() {
        let mut mapper = detect(1, &header(320, 3, 1)).unwrap().expect("speex mapper");

        // Comment (empty), one extra header, then audio.
        let comment = [0u8; 8];
        assert!(matches!(mapper.map_packet(&comment).unwrap(), MapResult::SideData { .. }));
        assert!(matches!(mapper.map_packet(&[1, 2, 3]).unwrap(), MapResult::Setup));

        match mapper.map_packet(&[0xff; 20]).unwrap() {
            MapResult::StreamData { dur, discard } => {
                assert_eq!(dur, Duration::new(960));
                // The wideband encoder lookahead is discarded from the start.
                assert_eq!(discard, Duration::new(143));
            }
            _ => panic!("expected stream data"),
        }

        // Only the first packet carries the start delay.
        match mapper.map_packet(&[0xff; 20]).unwrap() {
            MapResult::StreamData { discard, .. } => assert_eq!(discard, Duration::ZERO),
            _ => panic!("expected stream data"),
        }
    }

    #[test]
    fn rejects_bad_headers() {
        assert!(detect(1, &header(160, 1, 0)[..79]).unwrap().is_none());
        assert!(detect(1, &header(0, 1, 0)).unwrap().is_none());
    }
}
