// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! OggPCM mapping, from the Xiph wiki draft "OggPCM".
//!
//! The main header packet is 28 bytes, big-endian: `"PCM     "`, the major (16) and minor (16)
//! version, the PCM format (32), the sample rate (32), the number of significant bits (8), the
//! number of channels (8), the maximum number of frames per packet (16, 0 meaning 65536), and the
//! number of extra header packets (32). It is followed by a bare Vorbis comment packet, the extra
//! header packets (channel mapping and conversion headers, which are skipped), and the data
//! packets of interleaved samples. A granule position is the number of frames.
//!
//! Samples with fewer significant bits than the format's width are left-justified, so they are
//! decoded at the width of the format.

use super::{MapResult, Mapper, PacketParser, read_bare_comment};

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::*;
use symphonia_core::codecs::audio::{AudioCodecId, AudioCodecParameters};
use symphonia_core::errors::Result;
use symphonia_core::formats::Track;
use symphonia_core::units::Duration;

/// The signature of the OggPCM main header packet.
const PCM_SIGNATURE: &[u8; 8] = b"PCM     ";

/// The size of the OggPCM main header packet.
const PCM_HEADER_SIZE: usize = 28;

/// The maximum number of extra header packets that will be skipped. This only bounds a corrupt
/// count.
const PCM_MAX_EXTRA_HEADERS: u32 = 1 << 16;

/// Get the codec and the size of a sample in bytes of an OggPCM format. Formats that are not
/// defined, or application specific (0x80000000 and above), are not supported.
fn pcm_format(format: u32) -> Option<(AudioCodecId, u32)> {
    let f = match format {
        0x00 => (CODEC_ID_PCM_S8, 1),
        0x01 => (CODEC_ID_PCM_U8, 1),
        0x02 => (CODEC_ID_PCM_S16LE, 2),
        0x03 => (CODEC_ID_PCM_S16BE, 2),
        0x04 => (CODEC_ID_PCM_S24LE, 3),
        0x05 => (CODEC_ID_PCM_S24BE, 3),
        0x06 => (CODEC_ID_PCM_S32LE, 4),
        0x07 => (CODEC_ID_PCM_S32BE, 4),
        0x10 => (CODEC_ID_PCM_MULAW, 1),
        0x11 => (CODEC_ID_PCM_ALAW, 1),
        0x20 => (CODEC_ID_PCM_F32LE, 4),
        0x21 => (CODEC_ID_PCM_F32BE, 4),
        0x22 => (CODEC_ID_PCM_F64LE, 8),
        0x23 => (CODEC_ID_PCM_F64BE, 8),
        _ => return None,
    };
    Some(f)
}

pub fn detect(serial: u32, buf: &[u8]) -> Result<Option<Box<dyn Mapper>>> {
    if buf.len() < PCM_HEADER_SIZE || buf[..8] != *PCM_SIGNATURE {
        return Ok(None);
    }

    let be16 = |o: usize| u16::from_be_bytes([buf[o], buf[o + 1]]);
    let be32 = |o: usize| u32::from_be_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);

    let major = be16(8);
    let format = be32(12);
    let rate = be32(16);
    let nb_channels = buf[21];
    let max_frames = match be16(22) {
        0 => 65536,
        n => u64::from(n),
    };
    let extra_headers = be32(24);

    // A major version above 0 breaks compatibility.
    let Some((codec, sample_bytes)) = pcm_format(format)
    else {
        return Ok(None);
    };

    if major != 0 || rate == 0 || nb_channels == 0 || extra_headers > PCM_MAX_EXTRA_HEADERS {
        return Ok(None);
    }

    let channels = match nb_channels {
        1 => Channels::Positioned(Position::FRONT_LEFT),
        2 => Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
        n => Channels::Discrete(u16::from(n)),
    };

    let mut codec_params = AudioCodecParameters::new();

    codec_params
        .for_codec(codec)
        .with_sample_rate(rate)
        .with_channels(channels)
        .with_max_frames_per_packet(max_frames);

    let mut track = Track::new(serial);

    track.with_codec_params(CodecParameters::Audio(codec_params));

    Ok(Some(Box::new(PcmMapper {
        track,
        frame_bytes: u64::from(sample_bytes) * u64::from(nb_channels),
        extra_headers,
        packets_seen: 0,
    })))
}

struct PcmPacketParser {
    frame_bytes: u64,
}

impl PacketParser for PcmPacketParser {
    fn parse_next_packet_dur(&mut self, packet: &[u8]) -> (Duration, Duration) {
        // A partial frame at the end of a packet is dropped.
        (Duration::new(packet.len() as u64 / self.frame_bytes), Duration::new(0))
    }
}

struct PcmMapper {
    track: Track,
    frame_bytes: u64,
    extra_headers: u32,
    /// The number of packets mapped since the main header packet, up to the first data packet.
    packets_seen: u32,
}

impl Mapper for PcmMapper {
    fn name(&self) -> &'static str {
        "pcm"
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
        Some(Box::new(PcmPacketParser { frame_bytes: self.frame_bytes }))
    }

    fn map_packet(&mut self, packet: &[u8]) -> Result<MapResult> {
        if u64::from(self.packets_seen) > u64::from(self.extra_headers) {
            let (dur, discard) =
                PcmPacketParser { frame_bytes: self.frame_bytes }.parse_next_packet_dur(packet);
            return Ok(MapResult::StreamData { dur, discard });
        }

        self.packets_seen += 1;

        if self.packets_seen == 1 { read_bare_comment(packet) } else { Ok(MapResult::Setup) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(format: u32, channels: u8, extra_headers: u32) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(PCM_SIGNATURE);
        h.extend_from_slice(&0u16.to_be_bytes()); // major
        h.extend_from_slice(&0u16.to_be_bytes()); // minor
        h.extend_from_slice(&format.to_be_bytes());
        h.extend_from_slice(&48000u32.to_be_bytes());
        h.push(0); // significant bits
        h.push(channels);
        h.extend_from_slice(&0u16.to_be_bytes()); // max frames per packet
        h.extend_from_slice(&extra_headers.to_be_bytes());
        h
    }

    #[test]
    fn header_fields_and_packet_duration() {
        // 24-bit little-endian stereo: 6 bytes per frame.
        let mut mapper = detect(1, &header(4, 2, 1)).unwrap().expect("pcm mapper");
        assert_eq!(mapper.name(), "pcm");

        assert!(matches!(mapper.map_packet(&[0u8; 8]).unwrap(), MapResult::SideData { .. }));
        assert!(matches!(mapper.map_packet(&[0, 0, 0, 0]).unwrap(), MapResult::Setup));

        // 600 bytes: 100 frames. A trailing partial frame is not counted.
        match mapper.map_packet(&[0u8; 603]).unwrap() {
            MapResult::StreamData { dur, discard } => {
                assert_eq!(dur, Duration::new(100));
                assert_eq!(discard, Duration::new(0));
            }
            _ => panic!("expected stream data"),
        }

        let params = match &mapper.track().codec_params {
            Some(CodecParameters::Audio(params)) => params,
            _ => panic!("expected audio parameters"),
        };
        assert_eq!(params.codec, CODEC_ID_PCM_S24LE);
        assert_eq!(params.sample_rate, Some(48000));
        assert_eq!(params.max_frames_per_packet, Some(65536));
    }

    #[test]
    fn formats_map_to_codecs() {
        let cases = [
            (0x00, CODEC_ID_PCM_S8),
            (0x01, CODEC_ID_PCM_U8),
            (0x03, CODEC_ID_PCM_S16BE),
            (0x07, CODEC_ID_PCM_S32BE),
            (0x10, CODEC_ID_PCM_MULAW),
            (0x11, CODEC_ID_PCM_ALAW),
            (0x21, CODEC_ID_PCM_F32BE),
            (0x22, CODEC_ID_PCM_F64LE),
        ];

        for (format, expected) in cases {
            let mapper = detect(1, &header(format, 1, 0)).unwrap().expect("pcm mapper");
            match &mapper.track().codec_params {
                Some(CodecParameters::Audio(params)) => assert_eq!(params.codec, expected),
                _ => panic!("expected audio parameters"),
            }
        }
    }

    #[test]
    fn invalid_headers_are_not_mapped() {
        // Application specific and undefined formats, no channels, and a short header.
        assert!(detect(1, &header(0x8000_0000, 1, 0)).unwrap().is_none());
        assert!(detect(1, &header(0x08, 1, 0)).unwrap().is_none());
        assert!(detect(1, &header(2, 0, 0)).unwrap().is_none());
        assert!(detect(1, &header(2, 1, 0)[..27]).unwrap().is_none());
    }
}
