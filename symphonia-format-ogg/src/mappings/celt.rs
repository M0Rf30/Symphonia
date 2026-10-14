// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CELT-in-Ogg mapping (the pre-standard CELT 0.x codec, as written by `celtenc`).
//!
//! The header layout is the `CELTHeader` structure of libcelt 0.x (`celt_header.h`), documented
//! on the Xiph wiki page "OggCELT" (all fields little-endian): `codec_id[8]` (`"CELT    "`),
//! `codec_version[20]`, `version_id`, `header_size`, `sample_rate`, `nb_channels`, `frame_size`,
//! `overlap`, `bytes_per_packet`, `extra_headers`; 60 bytes. The layout (header packet, Vorbis
//! comment packet, `extra_headers` additional header packets, then audio packets) mirrors the Speex
//! mapping, and is read the same way by FFmpeg's `oggparsecelt.c`.
//!
//! Every audio packet is one frame of `frame_size` samples. The granule positions are used as
//! they are.

use super::{MapResult, Mapper, PacketParser, read_bare_comment};

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_CELT;
use symphonia_core::errors::Result;
use symphonia_core::formats::Track;
use symphonia_core::units::Duration;

/// The signature of the CELT header packet.
const CELT_SIGNATURE: &[u8; 8] = b"CELT    ";

/// The size of the CELT header packet (`sizeof(CELTHeader)`).
const CELT_HEADER_SIZE: usize = 60;

/// The maximum number of extra header packets that will be skipped. This only bounds a corrupt
/// count.
const CELT_MAX_EXTRA_HEADERS: u32 = 1 << 16;

fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]])
}

pub fn detect(serial: u32, buf: &[u8]) -> Result<Option<Box<dyn Mapper>>> {
    if buf.len() < CELT_HEADER_SIZE || buf[..8] != *CELT_SIGNATURE {
        return Ok(None);
    }

    // Offsets: 28 version_id, 32 header_size, 36 sample_rate, 40 nb_channels, 44 frame_size,
    // 48 overlap, 52 bytes_per_packet, 56 extra_headers.
    let header_size = read_u32_le(buf, 32);
    let rate = read_u32_le(buf, 36);
    let nb_channels = read_u32_le(buf, 40);
    let frame_size = read_u32_le(buf, 44);
    let extra_headers = read_u32_le(buf, 56);

    // libcelt 0.x supports mono and stereo. A header that does not describe a decodable stream is
    // not mapped as CELT.
    if (header_size as usize) < CELT_HEADER_SIZE
        || rate == 0
        || !(1..=2).contains(&nb_channels)
        || frame_size == 0
        || extra_headers > CELT_MAX_EXTRA_HEADERS
    {
        return Ok(None);
    }

    let channels = if nb_channels == 1 {
        Channels::Positioned(Position::FRONT_LEFT)
    }
    else {
        Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
    };

    let mut codec_params = AudioCodecParameters::new();

    codec_params
        .for_codec(CODEC_ID_CELT)
        .with_sample_rate(rate)
        .with_channels(channels)
        .with_max_frames_per_packet(u64::from(frame_size))
        .with_extra_data(Box::from(buf));

    let mut track = Track::new(serial);

    track.with_codec_params(CodecParameters::Audio(codec_params));

    Ok(Some(Box::new(CeltMapper {
        track,
        frame_size: u64::from(frame_size),
        extra_headers,
        packets_seen: 0,
    })))
}

struct CeltPacketParser {
    frame_size: u64,
}

impl PacketParser for CeltPacketParser {
    fn parse_next_packet_dur(&mut self, _: &[u8]) -> (Duration, Duration) {
        (Duration::new(self.frame_size), Duration::new(0))
    }
}

struct CeltMapper {
    track: Track,
    frame_size: u64,
    extra_headers: u32,
    /// The number of packets mapped since the identification packet, up to the first audio packet.
    packets_seen: u32,
}

impl Mapper for CeltMapper {
    fn name(&self) -> &'static str {
        "celt"
    }

    fn reset(&mut self) {
        // Nothing to do. Header packets are only mapped while the physical stream starts.
    }

    fn track(&self) -> &Track {
        &self.track
    }

    fn track_mut(&mut self) -> &mut Track {
        &mut self.track
    }

    fn make_parser(&self) -> Option<Box<dyn PacketParser>> {
        Some(Box::new(CeltPacketParser { frame_size: self.frame_size }))
    }

    fn map_packet(&mut self, packet: &[u8]) -> Result<MapResult> {
        // The comment packet follows the header packet, then `extra_headers` more header packets.
        // Everything after that is audio.
        if u64::from(self.packets_seen) > u64::from(self.extra_headers) {
            return Ok(MapResult::StreamData {
                dur: Duration::new(self.frame_size),
                discard: Duration::new(0),
            });
        }

        self.packets_seen += 1;

        if self.packets_seen == 1 { read_bare_comment(packet) } else { Ok(MapResult::Setup) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(nb_channels: u32, frame_size: u32, extra_headers: u32) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(CELT_SIGNATURE);
        h.extend_from_slice(&[0; 20]); // version string
        h.extend_from_slice(&0x8000_000bu32.to_le_bytes()); // version id
        h.extend_from_slice(&60u32.to_le_bytes()); // header size
        h.extend_from_slice(&44100u32.to_le_bytes()); // rate
        h.extend_from_slice(&nb_channels.to_le_bytes());
        h.extend_from_slice(&frame_size.to_le_bytes());
        h.extend_from_slice(&128u32.to_le_bytes()); // overlap
        h.extend_from_slice(&120u32.to_le_bytes()); // bytes per packet
        h.extend_from_slice(&extra_headers.to_le_bytes());
        h
    }

    #[test]
    fn header_fields_and_packet_duration() {
        let mut mapper = detect(3, &header(2, 512, 1)).unwrap().expect("celt mapper");
        assert_eq!(mapper.name(), "celt");

        let comment = [0u8; 8];
        assert!(matches!(mapper.map_packet(&comment).unwrap(), MapResult::SideData { .. }));
        assert!(matches!(mapper.map_packet(&[1, 2, 3]).unwrap(), MapResult::Setup));

        match mapper.map_packet(&[0xff; 120]).unwrap() {
            MapResult::StreamData { dur, discard } => {
                assert_eq!(dur, Duration::new(512));
                assert_eq!(discard, Duration::new(0));
            }
            _ => panic!("expected stream data"),
        }

        let params = match &mapper.track().codec_params {
            Some(CodecParameters::Audio(params)) => params,
            _ => panic!("expected audio parameters"),
        };
        assert_eq!(params.codec, CODEC_ID_CELT);
        assert_eq!(params.sample_rate, Some(44100));
        assert_eq!(params.max_frames_per_packet, Some(512));
    }

    #[test]
    fn invalid_headers_are_not_mapped() {
        assert!(detect(1, &header(0, 512, 0)).unwrap().is_none());
        assert!(detect(1, &header(3, 512, 0)).unwrap().is_none());
        assert!(detect(1, &header(1, 0, 0)).unwrap().is_none());
        assert!(detect(1, &header(1, 512, 0)[..59]).unwrap().is_none());
    }
}
