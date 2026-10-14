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
//!
//! Timeline: timestamp 0 is the first source sample, which is decoded sample
//! `encoder_lookahead + decoder_lookahead` (what `speexdec` outputs from). The granule positions
//! already exclude the encoder lookahead, and (except the clamped final one of `speexenc`) lag the
//! source timeline by the decoder lookahead. See `SpeexMapper::absgp_to_ts`.

use crate::common::SideData;

use super::{MapResult, Mapper, PacketParser};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_SPEEX;
use symphonia_core::errors::Result;
use symphonia_core::formats::Track;
use symphonia_core::io::BufReader;
use symphonia_core::meta::MetadataBuilder;
use symphonia_core::units::{Duration, Timestamp};
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
    /// The sample rate in Hz.
    rate: u32,
    /// The algorithmic delay of the decoder, in samples (see `decoder_lookahead`).
    decoder_lookahead: u32,
    /// The delay of the whole encode-decode chain, in samples: the encoder lookahead (excluded
    /// from the granule positions) plus the decoder lookahead.
    chain_delay: u32,
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

/// The lookahead of the encoder of each Speex mode (`speex_encoder_ctl(SPEEX_GET_LOOKAHEAD)`:
/// narrowband 40, wideband `2 * 40 + 63`, ultra-wideband `2 * 143 + 63` samples). `speexenc` and
/// libavformat both subtract it from the granule positions, so the first audio packet of a stream
/// starts at `-encoder_lookahead` in the granule timeline.
fn encoder_lookahead(mode: u32) -> u32 {
    match mode {
        0 => 40,
        1 => 143,
        2 => 349,
        _ => 0,
    }
}

/// The lookahead of the decoder of each Speex mode (`speex_decoder_ctl(SPEEX_GET_LOOKAHEAD)`:
/// narrowband 40, wideband `2 * 40`, ultra-wideband `2 * 2 * 40` samples). The granule positions
/// do not account for it: the decoded sample `n` of a stream is the source sample
/// `n - encoder_lookahead - decoder_lookahead`. `speexdec` discards both, the first from the
/// granule position of the first page and the second from the decoder.
fn decoder_lookahead(mode: u32) -> u32 {
    match mode {
        0 => 40,
        1 => 80,
        2 => 160,
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
        rate,
        decoder_lookahead: decoder_lookahead(mode),
        chain_delay: encoder_lookahead(mode) + decoder_lookahead(mode),
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

    track.with_codec_params(CodecParameters::Audio(codec_params)).with_delay(header.chain_delay);

    Ok(Some(Box::new(SpeexMapper {
        track,
        header,
        packets_seen: 0,
        parser: SpeexPacketParser::new(header),
    })))
}

/// Every Speex audio packet decodes to `frames_per_packet * frame_size` samples. A final packet
/// that carries fewer frames (padded with a terminator) is trimmed to the page's granule
/// position by the logical stream, exactly like the padding of other codecs. The decoder
/// lookahead is reported as the discard of the first packets of the stream. The encoder lookahead
/// is not: it is already excluded from the granule positions, so that the first packet starts at
/// a negative timestamp and the logical stream discards the samples before timestamp 0.
struct SpeexPacketParser {
    dur: u64,
    /// The start delay that has not yet been reported as discard.
    remaining_discard: u64,
}

impl SpeexPacketParser {
    fn new(header: SpeexHeader) -> Self {
        SpeexPacketParser {
            dur: header.packet_dur(),
            remaining_discard: u64::from(header.decoder_lookahead),
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

    fn absgp_to_ts(&self, absgp: u64, is_last_page: bool) -> Timestamp {
        // Timestamp 0 is the first source sample, i.e. decoded sample `chain_delay`. A granule
        // position `g` marks the decoded sample `g + encoder_lookahead`, which is the source
        // sample `g - decoder_lookahead`.
        //
        // The exception is the granule position of the last page. `speexenc` clamps it to the
        // number of source samples, so it already is a timestamp (and `speexdec` ends the stream
        // there). A stream by another muxer, whose last granule position follows the same rule as
        // the other pages, is not trimmed by this: its end timestamp lies beyond the decoded data.
        let absgp = absgp as i64;

        if is_last_page {
            Timestamp::new(absgp)
        }
        else {
            Timestamp::new(absgp.saturating_sub(i64::from(self.header.decoder_lookahead)))
        }
    }

    fn max_rap_period(&self) -> Duration {
        // Every Speex packet is decodable on its own, but the decoder is not memoryless: the
        // adaptive codebook (long-term predictor) and the synthesis filters recurse over the
        // past. After a seek the decoder starts from a blank state and converges to the state
        // of a continuous decode geometrically. Measured on constant bit-rate speexenc streams with libspeex-exact output over
        // many seek targets (narrowband, wideband, ultra-wideband, mono and stereo, 1 and 2 frames per
        // packet): the output is within 1 LSB of a continuous decode after at most 0.7 s, and
        // within 16 LSB after 0.5 s. So decode and discard one second before the seek target.
        // (A bit-exact match is not reachable with any pre-roll: the noise-excited submodes
        // and the packet-loss concealment of libspeex draw from a pseudo-random generator
        // (`st->seed`) that runs from the start of the stream, so a stream with such frames
        // differs from a continuous decode for as long as the effect of such a frame lasts.)
        Duration::new(u64::from(self.header.rate))
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
                // The wideband decoder lookahead is discarded from the start. (The encoder
                // lookahead is excluded by the granule positions.)
                assert_eq!(discard, Duration::new(80));
            }
            _ => panic!("expected stream data"),
        }

        // Only the first packet carries the start delay.
        match mapper.map_packet(&[0xff; 20]).unwrap() {
            MapResult::StreamData { discard, .. } => assert_eq!(discard, Duration::ZERO),
            _ => panic!("expected stream data"),
        }
    }

    /// A header of the given `mode` (0 narrowband, 1 wideband, 2 ultra-wideband) and `rate`.
    fn mode_header(mode: u32, rate: u32) -> Vec<u8> {
        let mut h = header(160, 1, 0);
        h[36..40].copy_from_slice(&rate.to_le_bytes());
        h[40..44].copy_from_slice(&mode.to_le_bytes());
        h
    }

    #[test]
    fn lookahead_and_timeline_by_mode() {
        // (mode, rate, encoder lookahead, decoder lookahead), from `SPEEX_GET_LOOKAHEAD` of the
        // libspeex 1.2.1 encoders and decoders.
        for (mode, rate, enc, dec) in [(0, 8000, 40, 40), (1, 16000, 143, 80), (2, 32000, 349, 160)]
        {
            let mut mapper = detect(1, &mode_header(mode, rate)).unwrap().expect("speex mapper");

            // The track starts with the delay of the whole encode-decode chain.
            assert_eq!(mapper.track().delay, Some(enc + dec), "mode {mode}");

            // A non-final granule position counts decoded samples without the encoder lookahead,
            // so it lags the source timeline by the decoder lookahead.
            assert_eq!(mapper.absgp_to_ts(10_000, false), Timestamp::new(10_000 - i64::from(dec)));
            // The final granule position of `speexenc` is the number of source samples.
            assert_eq!(mapper.absgp_to_ts(10_000, true), Timestamp::new(10_000));

            // One second is decoded and discarded before a seek target.
            assert_eq!(mapper.max_rap_period(), Duration::new(u64::from(rate)));

            // Only the decoder lookahead is a codec discard.
            mapper.map_packet(&[0u8; 8]).unwrap();
            match mapper.map_packet(&[0xff; 20]).unwrap() {
                MapResult::StreamData { discard, .. } => {
                    assert_eq!(discard, Duration::new(u64::from(dec)))
                }
                _ => panic!("expected stream data"),
            }
        }
    }

    #[test]
    fn rejects_bad_headers() {
        assert!(detect(1, &header(160, 1, 0)[..79]).unwrap().is_none());
        assert!(detect(1, &header(0, 1, 0)).unwrap().is_none());
    }
}
