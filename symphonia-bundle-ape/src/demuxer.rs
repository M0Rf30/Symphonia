// Symphonia APE demuxer
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Read, Seek, SeekFrom};

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_MONKEYS_AUDIO;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Error, Result, SeekErrorKind, decode_error, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use crate::map_ape_error;

/// Current-format (version >= 3980) descriptor magic.
const APE_MAGIC: [u8; 4] = *b"MAC ";
/// Old-format (version < 3980) descriptor magic.
const APEF_MAGIC: [u8; 4] = *b"MACF";

/// The maximum size of a single compressed APE frame that will be read into memory.
const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;

const APE_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"APE\0")),
    short_name: "ape",
    long_name: "Monkey's Audio",
};

/// Map an APE channel count onto Symphonia's channel position bitmask.
///
/// Monkey's Audio does not encode a channel layout, only a channel count. For mono and stereo
/// (by far the most common cases) the mapping is unambiguous. For higher channel counts a
/// best-effort standard layout is assumed; if no such mapping exists the channels are reported as
/// discrete (unpositioned).
fn channels_from_count(count: u16) -> Channels {
    let pos = match count {
        1 => Position::FRONT_CENTER,
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
        _ => return Channels::Discrete(count),
    };

    if pos.bits().count_ones() != u32::from(count) {
        return Channels::Discrete(count);
    }

    Channels::Positioned(pos)
}

/// Pack the codec state the decoder needs to reconstruct its `ape_decoder::FrameDecoder`, but
/// that isn't otherwise carried by `AudioCodecParameters`.
///
/// Layout (6 bytes, all little-endian): `version(u16)`, `compression_level(u16)`,
/// `channels(u16)`.
fn build_extra_data(info: &ape_decoder::ApeFileInfo) -> Box<[u8]> {
    let mut buf = [0u8; 6];
    buf[0..2].copy_from_slice(&info.descriptor.version.to_le_bytes());
    buf[2..4].copy_from_slice(&info.header.compression_level.to_le_bytes());
    buf[4..6].copy_from_slice(&info.header.channels.to_le_bytes());
    Box::new(buf)
}

/// Monkey's Audio (APE) format reader (demuxer).
pub struct ApeReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    file_info: ape_decoder::ApeFileInfo,
    current_frame: u32,
}

impl<'s> ApeReader<'s> {
    pub fn try_new(mut reader: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // Parse the APE descriptor, header, and seek table. This also validates the header (e.g.
        // channel count, blocks-per-frame) and independently locates the descriptor, so it is
        // robust regardless of where the probe left the stream position (e.g. after skipping a
        // leading ID3v2 tag).
        let file_info = ape_decoder::format::parse(&mut reader).map_err(map_ape_error)?;
        let header = &file_info.header;

        if header.sample_rate == 0 {
            return decode_error("ape: invalid sample rate");
        }

        let channels = channels_from_count(header.channels);

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_MONKEYS_AUDIO)
            .with_sample_rate(header.sample_rate)
            .with_bits_per_sample(u32::from(header.bits_per_sample))
            .with_channels(channels)
            .with_max_frames_per_packet(u64::from(header.blocks_per_frame))
            .with_extra_data(build_extra_data(&file_info));

        let total_blocks = file_info.total_blocks.max(0) as u64;

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(params)).with_num_frames(total_blocks);

        // Standalone APEv1/APEv2 and ID3v1/ID3v2 metadata readers are registered separately with
        // the probe (anchored at the end of the stream, or scanned from the start). Any metadata
        // they found is threaded through `FormatOptions::external_data`; simply pick it up here
        // rather than re-parsing tags ourselves.
        let metadata = opts.external_data.metadata.unwrap_or_default();

        Ok(ApeReader {
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            reader,
            metadata,
            file_info,
            current_frame: 0,
        })
    }

    /// The byte alignment remainder for the given frame relative to the start of the first frame.
    fn seek_remainder(&self, frame_idx: u32) -> u32 {
        let seek_byte = self.file_info.seek_byte(frame_idx);
        let seek_byte_0 = self.file_info.seek_byte(0);
        ((seek_byte - seek_byte_0) % 4) as u32
    }
}

impl Scoreable for ApeReader<'_> {
    fn score(mut mss: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(match mss.read_quad_bytes() {
            Ok(b) if b == APE_MAGIC || b == APEF_MAGIC => Score::Supported(255),
            _ => Score::Unsupported,
        })
    }
}

impl ProbeableFormat<'_> for ApeReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(ApeReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            APE_FORMAT_INFO,
            &["ape"],
            &["audio/x-ape", "audio/ape"],
            &[b"MAC ", b"MACF"]
        )]
    }
}

impl FormatReader for ApeReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &APE_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let frame_idx = self.current_frame;

        if frame_idx >= self.file_info.header.total_frames {
            return Ok(None);
        }

        let seek_byte = self.file_info.seek_byte(frame_idx);
        let seek_remainder = self.seek_remainder(frame_idx);
        let frame_bytes = self.file_info.frame_byte_count(frame_idx);
        let frame_blocks = self.file_info.frame_block_count(frame_idx);

        if frame_bytes > MAX_FRAME_BYTES {
            return decode_error("ape: frame data exceeds maximum size");
        }

        // The decoder needs the byte alignment remainder to correctly interpret the frame data;
        // prepend it as a 4-byte little-endian header.
        let read_len = 4 + seek_remainder as usize + frame_bytes as usize;

        self.reader.seek(SeekFrom::Start(seek_byte - u64::from(seek_remainder)))?;

        let mut packet_data = vec![0u8; read_len];
        packet_data[0..4].copy_from_slice(&seek_remainder.to_le_bytes());
        self.reader.read_exact(&mut packet_data[4..])?;

        let ts = u64::from(frame_idx) * u64::from(self.file_info.header.blocks_per_frame);

        self.current_frame += 1;

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(ts as i64))
                .dur(Duration::new(u64::from(frame_blocks)))
                .data(packet_data)
                .build(),
        ))
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if !self.reader.is_seekable() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        let track = &self.tracks[0];

        let ts = match to {
            SeekTo::Timestamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                let tb =
                    track.time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        if ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let total = self.file_info.total_blocks.max(0) as u64;

        if ts.get() as u64 >= total {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let bpf = u64::from(self.file_info.header.blocks_per_frame);
        let frame_idx = (ts.get() as u64 / bpf) as u32;
        let actual_ts = frame_idx as u64 * bpf;

        self.current_frame = frame_idx;

        Ok(SeekedTo {
            track_id: 0,
            required_ts: ts,
            actual_ts: Timestamp::new(actual_ts as i64),
        })
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
