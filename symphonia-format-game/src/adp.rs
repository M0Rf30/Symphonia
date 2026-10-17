// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Nintendo GameCube DTK (`.adp`, `.dtk`) demuxer.
//!
//! The layout follows FFmpeg's `libavformat/adp.c`. The file has no header: it is a sequence of
//! 32 byte blocks of 28 samples of a stereo stream at 48 kHz. The headers of a block are stored
//! twice (bytes 0 and 2 are equal, and bytes 1 and 3 are equal), which is what the probe checks.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_ADPCM_NINTENDO_DTK;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Result, SeekErrorKind, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use crate::common::{channels_for, frame_time_base, read_up_to, required_timestamp, unseekable};

const ADP_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"ADP\0")),
    short_name: "adp",
    long_name: "Nintendo GameCube DTK",
};

/// The sample rate (`adp_read_header`, `libavformat/adp.c`).
const SAMPLE_RATE: u32 = 48000;

/// The size in bytes of a block.
const BLOCK_LEN: u64 = 32;

/// The number of samples per channel in a block.
const BLOCK_SAMPLES: u64 = 28;

/// The number of blocks in a packet (1024 bytes, `adp_read_packet` of `libavformat/adp.c`).
const BLOCKS_PER_PACKET: u64 = 32;

/// The number of blocks decoded and discarded before the requested position of an accurate seek.
/// The history of the ADPCM is not stored in the stream; its response to a wrong history decays
/// with the number of samples.
const SEEK_PREROLL_BLOCKS: u64 = 8;

/// The number of bytes that the probe examines.
const PROBE_LEN: usize = 2048;

/// Nintendo GameCube DTK reader.
pub struct AdpReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    /// The position of the first block.
    data_start: u64,
    /// The index of the next block to read.
    next_block: u64,
    /// The number of blocks in the stream, if it is known.
    num_blocks: Option<u64>,
}

impl<'s> AdpReader<'s> {
    pub fn try_new(source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        let data_start = source.pos();
        let num_blocks = source.byte_len().map(|len| len.saturating_sub(data_start) / BLOCK_LEN);
        let num_frames = num_blocks.map(|blocks| blocks * BLOCK_SAMPLES);

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_ADPCM_NINTENDO_DTK)
            .with_sample_rate(SAMPLE_RATE)
            .with_bits_per_coded_sample(4)
            .with_channels(channels_for(2))
            .with_max_frames_per_packet(BLOCKS_PER_PACKET * BLOCK_SAMPLES);

        let time_base = frame_time_base(SAMPLE_RATE);

        let mut track = Track::new(0);
        track.time_base = time_base;
        track.num_frames = num_frames;
        track.duration = num_frames.map(Duration::new);
        track.with_codec_params(CodecParameters::Audio(params));

        let mut media_info = MediaInfo::new();
        media_info.time_base = time_base;
        media_info.duration = num_frames.map(Duration::new);

        Ok(AdpReader {
            reader: source,
            media_info,
            tracks: vec![track],
            metadata: MetadataLog::default(),
            data_start,
            next_block: 0,
            num_blocks,
        })
    }
}

impl Scoreable for AdpReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        // `adp_probe` of `libavformat/adp.c`.
        let mut buf = [0u8; PROBE_LEN];
        let len = read_up_to(&mut src, &mut buf)?;

        if len < BLOCK_LEN as usize {
            return Ok(Score::Unsupported);
        }

        let mut changes = 0;
        let mut last = 0;

        for block in buf[..len].chunks(BLOCK_LEN as usize) {
            if block.len() < 4 {
                break;
            }
            if block[0] != block[2] || block[1] != block[3] {
                return Ok(Score::Unsupported);
            }
            if block[0] != last {
                changes += 1;
            }
            last = block[0];
        }

        Ok(match (changes, len) {
            (..=1, _) => Score::Unsupported,
            (_, ..260) => Score::Supported(1),
            _ => Score::Supported(63),
        })
    }
}

impl ProbeableFormat<'_> for AdpReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(AdpReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(ADP_FORMAT_INFO, &["adp", "dtk"], &["audio/x-adp"], &[])]
    }
}

impl FormatReader for AdpReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &ADP_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let blocks = match self.num_blocks {
            Some(total) => total.saturating_sub(self.next_block).min(BLOCKS_PER_PACKET),
            None => BLOCKS_PER_PACKET,
        };

        if blocks == 0 {
            return Ok(None);
        }

        let mut buf = vec![0u8; (blocks * BLOCK_LEN) as usize];
        let len = read_up_to(&mut self.reader, &mut buf)?;

        // A partial block at the end of the stream is dropped.
        let blocks = len as u64 / BLOCK_LEN;
        if blocks == 0 {
            self.num_blocks = Some(self.next_block);
            return Ok(None);
        }

        buf.truncate((blocks * BLOCK_LEN) as usize);

        let pts = self.next_block * BLOCK_SAMPLES;
        self.next_block += blocks;

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(pts as i64))
                .dur(Duration::new(blocks * BLOCK_SAMPLES))
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

        if self.num_blocks.is_some_and(|blocks| required_ts >= blocks * BLOCK_SAMPLES) {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let mut block = required_ts / BLOCK_SAMPLES;

        if mode == SeekMode::Accurate {
            block = block.saturating_sub(SEEK_PREROLL_BLOCKS);
        }

        self.reader.seek(SeekFrom::Start(self.data_start + block * BLOCK_LEN))?;
        self.next_block = block;

        Ok(SeekedTo {
            track_id: 0,
            required_ts: Timestamp::new(required_ts as i64),
            actual_ts: Timestamp::new((block * BLOCK_SAMPLES) as i64),
        })
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
