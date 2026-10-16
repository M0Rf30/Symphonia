// Symphonia TTA Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `.tta` file reader.
//!
//! A file is the 22-byte header, the seek table (the size in bytes of every frame as a little
//! endian `u32`, and the CRC-32 of the table), and the frames, each ending in its CRC-32; see
//! FFmpeg `libavformat/tta.c`. Tags are not part of the container: ID3v2 tags at the start and
//! ID3v1 and APEv2 tags at the end are read by the probe.

use std::io::{Read, Seek, SeekFrom};

use symphonia_core::checksum::Crc32Ieee;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_TTA;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Error, Result, SeekErrorKind, decode_error, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::{MediaInfo, TrackFlags};
use symphonia_core::io::{MediaSource, MediaSourceStream, Monitor, ReadBytes, ScopedStream};
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;

use crate::header::{
    FORMAT_ENCRYPTED, FORMAT_SIMPLE, HEADER_LEN, Header, SIGNATURE, channels_from_count,
    check_params,
};

const TTA_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"TTA\0")),
    short_name: "tta",
    long_name: "True Audio",
};

/// The greatest number of frames: the table must be addressable with 32-bit offsets
/// (`tta_read_header`).
const MAX_FRAMES: u64 = (i32::MAX as u64 - 4) / 4;

/// The capacity of the packet buffer when the size of the frame cannot be checked against the
/// length of the stream. Larger frames grow the buffer as their data is read.
const UNCHECKED_PACKET_CAPACITY: usize = 1 << 20;

/// True Audio (`.tta`) format reader (demuxer).
pub struct TtaReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    /// The number of samples (per channel) of a frame, but the last.
    frame_length: u64,
    /// The number of samples of the last frame.
    last_frame_length: u64,
    /// The size of every frame in bytes.
    sizes: Vec<u32>,
    /// The position of the first frame in the stream.
    first_frame_pos: u64,
    /// The index of the frame `next_packet` reads next.
    next_frame: usize,
    /// The position of the frame `next_frame` in the stream.
    next_pos: u64,
}

impl<'s> TtaReader<'s> {
    pub fn try_new(mut reader: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let mut buf = [0u8; HEADER_LEN];
        ReadBytes::read_buf_exact(&mut reader, &mut buf)?;

        let header = Header::parse(&buf)?;

        check_params(header.channels, header.sample_rate)?;

        if header.bits_per_sample == 0 {
            return decode_error("tta: invalid bits per sample");
        }
        if header.format != FORMAT_SIMPLE && header.format != FORMAT_ENCRYPTED {
            return decode_error("tta: invalid format");
        }
        if header.data_length == 0 {
            return decode_error("tta: invalid number of samples");
        }
        if !Header::crc_matches(&buf) {
            return decode_error("tta: header crc mismatch");
        }

        let total_samples = u64::from(header.data_length);
        let frame_length = header.frame_length();

        let last_frame_length = match total_samples % frame_length {
            0 => frame_length,
            n => n,
        };
        let total_frames = total_samples.div_ceil(frame_length);

        if total_frames > MAX_FRAMES {
            return decode_error("tta: invalid number of frames");
        }

        // The seek table. The number of entries is not trusted for allocating, a stream that is
        // too short for them is invalid.
        let mut sizes = Vec::with_capacity((total_frames as usize).min(1 << 16));
        let mut crc = Crc32Ieee::new(!0);

        for _ in 0..total_frames {
            let mut entry = [0u8; 4];
            read_table(&mut reader, &mut entry)?;
            crc.process_buf_bytes(&entry);
            sizes.push(u32::from_le_bytes(entry));
        }

        let mut stored = [0u8; 4];
        read_table(&mut reader, &mut stored)?;
        if !crc.crc() != u32::from_le_bytes(stored) {
            return decode_error("tta: seek table crc mismatch");
        }

        let first_frame_pos = reader.pos();

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_TTA)
            .with_sample_rate(header.sample_rate)
            .with_bits_per_sample(u32::from(header.bits_per_sample))
            .with_channels(channels_from_count(header.channels))
            .with_max_frames_per_packet(frame_length)
            .with_extra_data(Box::from(buf));

        let mut track = Track::new(0);
        track
            .with_codec_params(CodecParameters::Audio(params))
            .with_num_frames(total_samples)
            .with_duration(Duration::new(total_samples))
            .with_flags(TrackFlags::DEFAULT);

        // The tags are read by the probe, and passed in the options.
        let metadata = opts.external_data.metadata.unwrap_or_default();

        Ok(TtaReader {
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            reader,
            metadata,
            frame_length,
            last_frame_length,
            sizes,
            first_frame_pos,
            next_frame: 0,
            next_pos: first_frame_pos,
        })
    }
}

/// Reads a part of the seek table. A stream that ends within it is invalid, not just short.
fn read_table(reader: &mut MediaSourceStream<'_>, buf: &mut [u8]) -> Result<()> {
    ReadBytes::read_buf_exact(reader, buf).map_err(|err| match err.kind() {
        std::io::ErrorKind::UnexpectedEof => Error::DecodeError("tta: truncated seek table"),
        _ => Error::IoError(err),
    })
}

impl Scoreable for TtaReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        // The same checks as the probe of FFmpeg's demuxer (`tta_probe`).
        let mut buf = [0u8; 14];
        if ReadBytes::read_buf_exact(&mut src, &mut buf).is_err() || buf[..4] != SIGNATURE {
            return Ok(Score::Unsupported);
        }

        let format = u16::from_le_bytes([buf[4], buf[5]]);
        let channels = u16::from_le_bytes([buf[6], buf[7]]);
        let bits = u16::from_le_bytes([buf[8], buf[9]]);
        let rate = u32::from_le_bytes([buf[10], buf[11], buf[12], buf[13]]);

        let is_header = (format == FORMAT_SIMPLE || format == FORMAT_ENCRYPTED)
            && channels > 0
            && bits > 0
            && rate > 0;

        Ok(if is_header { Score::Supported(255) } else { Score::Unsupported })
    }
}

impl ProbeableFormat<'_> for TtaReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(TtaReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(TTA_FORMAT_INFO, &["tta"], &["audio/x-tta", "audio/tta"], &[b"TTA1"])]
    }
}

impl FormatReader for TtaReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &TTA_FORMAT_INFO
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
        let Some(&size) = self.sizes.get(self.next_frame)
        else {
            return Ok(None);
        };

        let index = self.next_frame;
        let size = size as usize;

        // The bytes the stream has for the frame decide how much to reserve.
        let capacity = match self.reader.byte_len() {
            Some(len) => size.min(len.saturating_sub(self.next_pos) as usize),
            None => size.min(UNCHECKED_PACKET_CAPACITY),
        };

        let mut data = Vec::with_capacity(capacity);
        Read::by_ref(&mut self.reader).take(size as u64).read_to_end(&mut data)?;

        // The next frame is not read, if this one is incomplete.
        self.next_frame += 1;
        self.next_pos += size as u64;

        if data.len() < size {
            self.next_frame = self.sizes.len();
            return Err(Error::IoError(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "tta: the stream ends in the middle of a frame",
            )));
        }

        let dur =
            if index + 1 == self.sizes.len() { self.last_frame_length } else { self.frame_length };

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new((index as u64 * self.frame_length) as i64))
                .dur(Duration::new(dur))
                .data(data)
                .build(),
        ))
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if !self.reader.is_seekable() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        let track = &self.tracks[0];

        let required_ts = match to {
            SeekTo::Timestamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                let tb = track.time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        let total = track.num_frames.unwrap_or(0);

        if required_ts.is_negative() || required_ts.get() as u64 >= total {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // Every frame is a seek point, so a seek is always to the start of the frame that holds
        // the requested sample: "before" the requested position as `SeekMode::Accurate` is
        // defined to be.
        let index = (required_ts.get() as u64 / self.frame_length) as usize;

        let pos =
            self.first_frame_pos + self.sizes[..index].iter().map(|&s| u64::from(s)).sum::<u64>();
        self.reader.seek(SeekFrom::Start(pos))?;

        self.next_frame = index;
        self.next_pos = pos;

        let actual_ts = Timestamp::new((index as u64 * self.frame_length) as i64);

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts })
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
