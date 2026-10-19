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
/// Monkey's Audio does not encode a channel layout, only a channel count, but the WAV header that
/// it keeps may hold one (see `wave_channel_mask`). For mono and stereo (by far the most common
/// cases) the mapping is unambiguous. For higher channel counts a best-effort standard layout is
/// assumed; if no such mapping exists the channels are reported as
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

/// The channel mask of the `WAVE_FORMAT_EXTENSIBLE` format chunk of the WAV header that Monkey's
/// Audio stores in the file, if it is there and fits the number of channels.
fn wave_channel_mask(wav_header: &[u8], channels: u16) -> Option<Channels> {
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
    // The format tag, channels, sample rate, byte rate, block align, bits per sample, the size of
    // the extension, the valid bits per sample, and then the channel mask.
    const MASK_OFFSET: usize = 20;

    let le_u32 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);

    if wav_header.len() < 12 || &wav_header[0..4] != b"RIFF" || &wav_header[8..12] != b"WAVE" {
        return None;
    }

    let mut chunks = &wav_header[12..];
    while chunks.len() >= 8 {
        let size = le_u32(&chunks[4..8]) as usize;
        let body = &chunks[8..];

        if &chunks[0..4] == b"fmt " {
            let fmt = &body[..size.min(body.len())];
            if fmt.len() < MASK_OFFSET + 4
                || u16::from_le_bytes([fmt[0], fmt[1]]) != WAVE_FORMAT_EXTENSIBLE
            {
                return None;
            }
            let mask = le_u32(&fmt[MASK_OFFSET..]);
            if mask.count_ones() != u32::from(channels) {
                return None;
            }
            return Position::from_wave_channel_mask(mask).map(Channels::Positioned);
        }

        // The chunks are padded to an even size.
        let next = size.checked_add(size & 1)?;
        chunks = body.get(next..)?;
    }

    None
}

/// Pack the codec state the decoder needs to reconstruct its `FrameDecoder`, but
/// that isn't otherwise carried by `AudioCodecParameters`.
///
/// Layout (8 bytes, all little-endian): `version(u16)`, `compression_level(u16)`,
/// `channels(u16)`, `format_flags(u16)`.
fn build_extra_data(info: &crate::mac::format::ApeFileInfo) -> Box<[u8]> {
    let mut buf = [0u8; 8];
    buf[0..2].copy_from_slice(&info.descriptor.version.to_le_bytes());
    buf[2..4].copy_from_slice(&info.header.compression_level.to_le_bytes());
    buf[4..6].copy_from_slice(&info.header.channels.to_le_bytes());
    buf[6..8].copy_from_slice(&info.format_flags.to_le_bytes());
    Box::new(buf)
}

/// The size of the header of a packet: the number of bytes of the first word of the data that
/// precede the frame (u32), the bit of the first byte of the frame the frame starts at (u32), and
/// the packet flags (u32), all little-endian.
pub(crate) const PACKET_HEADER_BYTES: usize = 12;

/// A packet flag: the data ends before the frame does, because the file is truncated.
pub(crate) const PACKET_FLAG_TRUNCATED: u32 = 1;

/// The number of bytes of the file that follow the frame that are put in the packet. The decoders
/// read a few bytes beyond the end of a frame (the SDK reads 4: `CAPEDecompress::
/// ScheduleFrameDecode`), and those of the oldest versions up to a 32-bit word more.
const PACKET_READ_AHEAD_BYTES: usize = 16;

/// Monkey's Audio (APE) format reader (demuxer).
pub struct ApeReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    file_info: crate::mac::format::ApeFileInfo,
    current_frame: u32,
    /// The file ended in the middle of a frame, and the data of it was delivered: the next call
    /// of `next_packet` reports it.
    truncated: bool,
}

impl<'s> ApeReader<'s> {
    pub fn try_new(mut reader: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // Parse the APE descriptor, header, and seek table. This also validates the header (e.g.
        // channel count, blocks-per-frame) and independently locates the descriptor, so it is
        // robust regardless of where the probe left the stream position (e.g. after skipping a
        // leading ID3v2 tag).
        let file_info = crate::mac::format::parse(&mut reader).map_err(map_ape_error)?;
        let header = &file_info.header;

        if header.sample_rate == 0 {
            return decode_error("ape: invalid sample rate");
        }

        let channels = wave_channel_mask(&file_info.wav_header_data, header.channels)
            .unwrap_or_else(|| channels_from_count(header.channels));

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
            truncated: false,
        })
    }

    /// Where the data of the given frame starts, as the number of bytes of the 32-bit word that
    /// holds its start that precede it, and the bit of the first byte of the frame it starts at.
    ///
    /// The bytes of the compressed stream are grouped in words, counted from the start of the
    /// first frame. The frames of the files up to version 3800 start at any bit; the table of the
    /// bits of their starts gives the bit, and their starts the word.
    fn frame_start(&self, frame_idx: u32) -> (u32, u32) {
        if self.file_info.descriptor.version <= 3800 {
            let seek_bit = self.file_info.seek_bit_table.get(frame_idx as usize).copied();
            return (0, u32::from(seek_bit.unwrap_or(0)));
        }

        let seek_byte = self.file_info.seek_byte(frame_idx);
        let seek_byte_0 = self.file_info.seek_byte(0);
        ((seek_byte.saturating_sub(seek_byte_0) % 4) as u32, 0)
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

        if self.truncated {
            // Whatever the file held of the last frame was delivered.
            self.truncated = false;
            self.current_frame = self.file_info.header.total_frames;
            return Err(Error::IoError(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "ape: the file ends in the middle of a frame",
            )));
        }

        if frame_idx >= self.file_info.header.total_frames {
            return Ok(None);
        }

        let seek_byte = self.file_info.seek_byte(frame_idx);
        let (seek_remainder, seek_bit) = self.frame_start(frame_idx);
        let frame_bytes = self.file_info.frame_byte_count(frame_idx);
        let frame_blocks = self.file_info.frame_block_count(frame_idx);

        if frame_bytes > MAX_FRAME_BYTES {
            // Move past the frame, or every following call would report it again.
            self.current_frame += 1;
            return decode_error("ape: frame data exceeds maximum size");
        }

        // The decoder needs where the frame starts to correctly interpret the frame data; it is
        // in the header of the packet.
        let frame_len = seek_remainder as usize + frame_bytes as usize;
        let read_len = (frame_len + PACKET_READ_AHEAD_BYTES) as u64;

        self.reader.seek(SeekFrom::Start(seek_byte - u64::from(seek_remainder)))?;

        let mut packet_data = Vec::with_capacity(PACKET_HEADER_BYTES + read_len as usize);
        packet_data.resize(PACKET_HEADER_BYTES, 0);
        Read::by_ref(&mut self.reader).take(read_len).read_to_end(&mut packet_data)?;

        // A frame that ends after the end of the file (that has no seek table entry that tells
        // where the next one starts, or whose entry is beyond the end of the file) is truncated.
        let data_len = packet_data.len() - PACKET_HEADER_BYTES;
        let is_last = frame_idx + 1 == self.file_info.header.total_frames;
        let truncated = data_len < frame_len
            || (is_last && self.file_info.declared_data_end > self.file_info.file_bytes);

        if truncated {
            if data_len <= seek_remainder as usize {
                // Nothing of the frame is there.
                self.current_frame = self.file_info.header.total_frames;
                return Err(Error::IoError(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "ape: the file ends before the end of the frames",
                )));
            }
            self.truncated = true;
        }

        packet_data[0..4].copy_from_slice(&seek_remainder.to_le_bytes());
        packet_data[4..8].copy_from_slice(&seek_bit.to_le_bytes());
        let flags = if truncated { PACKET_FLAG_TRUNCATED } else { 0 };
        packet_data[8..12].copy_from_slice(&flags.to_le_bytes());

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
                let tb = track.time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
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
        self.truncated = false;

        Ok(SeekedTo { track_id: 0, required_ts: ts, actual_ts: Timestamp::new(actual_ts as i64) })
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
