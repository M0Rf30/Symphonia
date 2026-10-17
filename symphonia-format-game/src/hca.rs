// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Demuxer of CRI HCA (`.hca`) files, after FFmpeg's `libavformat/hca.c`.
//!
//! A file is a header, a sequence of chunks that starts with the `HCA` signature and ends where
//! the data begins (the header size of the file), followed by frames of the size of the `comp`
//! chunk, each of 1024 samples per channel. The header is the extra data of the decoder. All
//! fields are big-endian. The top bit of the bytes of the chunk tags is set in files that are
//! encrypted, and is ignored here.
//!
//! Like FFmpeg, the muted samples at the start and the end of the stream are not trimmed.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_HCA;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Result, SeekErrorKind, decode_error, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use crate::common::{channels_for, frame_time_base, read_up_to, required_timestamp, unseekable};

const HCA_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"HCA\0")),
    short_name: "hca",
    long_name: "CRI HCA",
};

/// The number of samples per channel in a frame.
const FRAME_SAMPLES: u64 = 1024;

/// The number of frames that are decoded and discarded before the requested position of an
/// accurate seek: a frame overlaps the previous one.
const SEEK_PREROLL_FRAMES: u64 = 1;

/// Clears the top bit of the bytes of a tag.
fn masked(tag: &[u8]) -> Vec<u8> {
    tag.iter().map(|b| b & 0x7f).collect()
}

/// CRI HCA reader.
pub struct HcaReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    /// The position of the first frame.
    data_start: u64,
    /// The size in bytes of a frame.
    block_size: u64,
    /// The index of the next frame to read.
    next_block: u64,
    /// The number of frames in the stream.
    num_blocks: u64,
}

impl<'s> HcaReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        let base = source.pos();

        let mut head = [0u8; 8];
        source.read_buf_exact(&mut head)?;

        if masked(&head[..4]) != b"HCA\0" {
            return decode_error("hca: invalid signature");
        }

        let header_size = u64::from(u16::from_be_bytes([head[6], head[7]]));
        if header_size <= 8 {
            return decode_error("hca: invalid header size");
        }

        let mut extra = vec![0u8; header_size as usize];
        extra[..8].copy_from_slice(&head);
        source.read_buf_exact(&mut extra[8..])?;

        // The `fmt` chunk, then `comp` or `dec`.
        if extra.len() < 8 + 16 + 6 || masked(&extra[8..12]) != b"fmt\0" {
            return decode_error("hca: missing fmt chunk");
        }

        let channels = usize::from(extra[12]);
        let rate = u32::from_be_bytes([0, extra[13], extra[14], extra[15]]);
        let num_blocks =
            u64::from(u32::from_be_bytes([extra[16], extra[17], extra[18], extra[19]]));

        let chunk = masked(&extra[24..28]);
        if chunk != b"comp" && chunk != b"dec\0" {
            return decode_error("hca: missing comp or dec chunk");
        }

        let block_size = u64::from(u16::from_be_bytes([extra[28], extra[29]]));
        if block_size < 8 {
            return decode_error("hca: invalid block size");
        }

        if channels == 0 || rate == 0 {
            return decode_error("hca: invalid stream parameters");
        }

        let num_frames = num_blocks * FRAME_SAMPLES;

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_HCA)
            .with_sample_rate(rate)
            .with_channels(channels_for(channels))
            .with_max_frames_per_packet(FRAME_SAMPLES)
            .with_frames_per_block(FRAME_SAMPLES)
            .with_extra_data(extra.into_boxed_slice());

        let time_base = frame_time_base(rate);

        let mut track = Track::new(0);
        track.time_base = time_base;
        track.num_frames = Some(num_frames);
        track.duration = Some(Duration::new(num_frames));
        track.with_codec_params(CodecParameters::Audio(params));

        let mut media_info = MediaInfo::new();
        media_info.time_base = time_base;
        media_info.duration = Some(Duration::new(num_frames));

        Ok(HcaReader {
            reader: source,
            media_info,
            tracks: vec![track],
            metadata: MetadataLog::default(),
            data_start: base + header_size,
            block_size,
            next_block: 0,
            num_blocks,
        })
    }
}

impl Scoreable for HcaReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut head = [0u8; 12];

        if read_up_to(&mut src, &mut head)? < head.len() {
            return Ok(Score::Unsupported);
        }

        // `hca_probe` of FFmpeg.
        Ok(if &head[..4] == b"HCA\0" && &head[8..12] == b"fmt\0" {
            Score::Supported(255)
        }
        else if masked(&head[..4]) == b"HCA\0" && masked(&head[8..12]) == b"fmt\0" {
            Score::Supported(200)
        }
        else {
            Score::Unsupported
        })
    }
}

impl ProbeableFormat<'_> for HcaReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(HcaReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(HCA_FORMAT_INFO, &["hca"], &["audio/x-hca"], &[b"HCA\0"])]
    }
}

impl FormatReader for HcaReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &HCA_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        if self.next_block >= self.num_blocks {
            return Ok(None);
        }

        let mut buf = vec![0u8; self.block_size as usize];
        let len = read_up_to(&mut self.reader, &mut buf)?;

        // A frame cut short by the end of the stream is dropped.
        if (len as u64) < self.block_size {
            self.num_blocks = self.next_block;
            return Ok(None);
        }

        let pts = self.next_block * FRAME_SAMPLES;
        self.next_block += 1;

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(pts as i64))
                .dur(Duration::new(FRAME_SAMPLES))
                .data(buf)
                .build(),
        ))
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if !self.reader.is_seekable() {
            return unseekable();
        }

        let Some(time_base) = self.media_info.time_base
        else {
            return unseekable();
        };

        let required_ts = required_timestamp(to, time_base)?;

        if required_ts >= self.num_blocks * FRAME_SAMPLES {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let mut block = required_ts / FRAME_SAMPLES;

        if mode == SeekMode::Accurate {
            block = block.saturating_sub(SEEK_PREROLL_FRAMES);
        }

        self.reader.seek(SeekFrom::Start(self.data_start + block * self.block_size))?;
        self.next_block = block;

        Ok(SeekedTo {
            track_id: 0,
            required_ts: Timestamp::new(required_ts as i64),
            actual_ts: Timestamp::new((block * FRAME_SAMPLES) as i64),
        })
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
