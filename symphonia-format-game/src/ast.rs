// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Nintendo AST (StrmAST) demuxer.
//!
//! The layout follows FFmpeg's `libavformat/astdec.c` and `ast.c`. All fields are big-endian
//! unless noted:
//!
//! ```text
//! 0x00  "STRM"
//! 0x04  u32   size of the audio data
//! 0x08  u16   codec (0: AFC ADPCM, 1: planar 16-bit PCM)
//! 0x0a  u16   bits per sample (16)
//! 0x0c  u16   channel count
//! 0x0e  u16   loop flag
//! 0x10  u32   sample rate
//! 0x14  u32   number of samples (per channel)
//! 0x18  u32   loop start, u32 loop end
//! 0x20  u32   size of the first block (per channel)
//! ...         the header is 0x40 bytes long
//! ```
//!
//! The header is followed by blocks. A block is `"BLCK"`, the size of the data of one channel (u32)
//! and 24 bytes of padding, then the data of the channels one after the other: the channel data
//! of a block is contiguous. The history of the ADPCM carries over from block to block.
//!
//! A block whose data is cut short by the end of the stream is dropped, FFmpeg decodes it with the
//! channels misaligned.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_ADPCM_AFC, CODEC_ID_PCM_S16BE_PLANAR};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{
    Result, SeekErrorKind, decode_error, limit_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use crate::common::{channels_for, frame_time_base, read_up_to, required_timestamp, unseekable};

const AST_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"STRM")),
    short_name: "ast",
    long_name: "Nintendo AST (Audio Stream)",
};

/// The size in bytes of the file header.
const HEADER_LEN: u64 = 64;

/// The size in bytes of the header of a block.
const BLOCK_HEADER_LEN: u64 = 32;

/// The largest number of channels of the AFC ADPCM (`ff_adpcm_decode_init`).
const MAX_AFC_CHANNELS: u64 = 6;

/// The largest number of channels of the PCM (`FF_SANE_NB_CHANNELS`, `libavcodec/internal.h`).
const MAX_PCM_CHANNELS: u64 = 64;

/// The largest block of one channel in bytes.
const MAX_BLOCK_LEN: u64 = 1 << 24;

/// The size in bytes of an AFC frame, and the number of its samples.
const AFC_FRAME_BYTES: u64 = 9;
const AFC_FRAME_SAMPLES: u64 = 16;

#[derive(Clone, Copy, PartialEq)]
enum Codec {
    Afc,
    Pcm,
}

impl Codec {
    /// The number of samples in `len` bytes of the data of one channel.
    fn frames_in(self, len: u64) -> u64 {
        match self {
            Codec::Afc => len / AFC_FRAME_BYTES * AFC_FRAME_SAMPLES,
            Codec::Pcm => len / 2,
        }
    }
}

/// A block of the stream.
#[derive(Clone, Copy)]
struct Block {
    /// The position of the header of the block.
    pos: u64,
    /// The first frame of the block.
    pts: u64,
    /// The number of frames of the block.
    frames: u64,
}

/// Nintendo AST reader.
pub struct AstReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    codec: Codec,
    channels: u64,
    /// The blocks of the stream, if the source is seekable.
    index: Vec<Block>,
    /// The first frame of the next block.
    next_pts: u64,
    /// The number of frames of the stream, if it is known.
    num_frames: Option<u64>,
    eof: bool,
}

/// Reads the header of a block. Returns the size of the data of a channel, or None at the end of
/// the stream.
fn read_block_header<B: ReadBytes>(src: &mut B) -> Result<Option<u64>> {
    let mut header = [0u8; BLOCK_HEADER_LEN as usize];

    if read_up_to(src, &mut header)? < header.len() {
        return Ok(None);
    }

    let size = u64::from(u32::from_be_bytes([header[4], header[5], header[6], header[7]]));

    if &header[..4] != b"BLCK" {
        return decode_error("ast: unknown chunk");
    }

    Ok(Some(size))
}

impl<'s> AstReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        let base = source.pos();

        let mut header = [0u8; HEADER_LEN as usize];
        source.read_buf_exact(&mut header)?;

        if &header[..4] != b"STRM" {
            return unsupported_error("ast: not an AST stream");
        }

        let be16 = |o: usize| u64::from(u16::from_be_bytes([header[o], header[o + 1]]));
        let be32 = |o: usize| {
            u64::from(u32::from_be_bytes([header[o], header[o + 1], header[o + 2], header[o + 3]]))
        };

        let codec = match be16(8) {
            0 => Codec::Afc,
            1 => Codec::Pcm,
            _ => return unsupported_error("ast: unsupported codec"),
        };

        if be16(10) != 16 {
            return unsupported_error("ast: unsupported bit depth");
        }

        let channels = be16(12);
        let max_channels = if codec == Codec::Afc { MAX_AFC_CHANNELS } else { MAX_PCM_CHANNELS };

        if channels == 0 {
            return decode_error("ast: invalid number of channels");
        }

        if channels > max_channels {
            return unsupported_error("ast: unsupported number of channels");
        }

        let rate = u32::try_from(be32(16)).unwrap_or(0);
        if rate == 0 || rate > i32::MAX as u32 {
            return decode_error("ast: invalid sample rate");
        }

        let duration = be32(20);
        let first_block = be32(32);

        // The blocks are indexed to seek, and to find the largest.
        let mut index = Vec::new();
        let mut max_block = first_block;

        if source.is_seekable() {
            let total_len = source.byte_len();
            let mut pts = 0u64;

            loop {
                let pos = source.pos();

                let Ok(Some(size)) = read_block_header(&mut source)
                else {
                    break;
                };

                let end = pos + BLOCK_HEADER_LEN + size * channels;

                // A block that is cut short by the end of the stream is not audio.
                if size > MAX_BLOCK_LEN || total_len.is_some_and(|len| end > len) {
                    break;
                }

                let frames = codec.frames_in(size);
                index.push(Block { pos, pts, frames });
                pts += frames;
                max_block = max_block.max(size);

                source.seek(SeekFrom::Start(end))?;
            }

            source.seek(SeekFrom::Start(base + HEADER_LEN))?;
        }

        if max_block == 0 || max_block > MAX_BLOCK_LEN {
            return decode_error("ast: invalid block size");
        }

        // The number of samples of the header, or the total of the blocks if it was not written.
        let indexed = index.last().map(|b| b.pts + b.frames);
        let num_frames = match (duration, indexed) {
            (0, indexed) => indexed,
            (duration, Some(indexed)) => Some(duration.min(indexed)),
            (duration, None) => Some(duration),
        };

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(match codec {
                Codec::Afc => CODEC_ID_ADPCM_AFC,
                Codec::Pcm => CODEC_ID_PCM_S16BE_PLANAR,
            })
            .with_sample_rate(rate)
            .with_bits_per_coded_sample(if codec == Codec::Afc { 4 } else { 16 })
            .with_channels(channels_for(channels as usize))
            .with_max_frames_per_packet(codec.frames_in(max_block));

        let time_base = frame_time_base(rate);

        let mut track = Track::new(0);
        track.time_base = time_base;
        track.num_frames = num_frames;
        track.duration = num_frames.map(Duration::new);
        track.with_codec_params(CodecParameters::Audio(params));

        let mut media_info = MediaInfo::new();
        media_info.time_base = time_base;
        media_info.duration = num_frames.map(Duration::new);

        Ok(AstReader {
            reader: source,
            media_info,
            tracks: vec![track],
            metadata: MetadataLog::default(),
            codec,
            channels,
            index,
            next_pts: 0,
            num_frames,
            eof: false,
        })
    }
}

impl Scoreable for AstReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        // `ast_probe` of `libavformat/astdec.c`.
        let mut h = [0u8; 20];

        if read_up_to(&mut src, &mut h)? < h.len() || &h[..4] != b"STRM" {
            return Ok(Score::Unsupported);
        }

        let be16 = |o: usize| u16::from_be_bytes([h[o], h[o + 1]]);
        let rate = u32::from_be_bytes([h[16], h[17], h[18], h[19]]);

        Ok(Score::Supported(
            if be16(10) == 0 || be16(12) == 0 || be16(12) > 256 || rate == 0 || rate > 8 * 48000 {
                32
            }
            else {
                170
            },
        ))
    }
}

impl ProbeableFormat<'_> for AstReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(AstReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(AST_FORMAT_INFO, &["ast"], &["audio/x-ast"], &[b"STRM"])]
    }
}

impl FormatReader for AstReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &AST_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        if self.eof || self.num_frames.is_some_and(|total| self.next_pts >= total) {
            return Ok(None);
        }

        let size = match read_block_header(&mut self.reader) {
            Ok(Some(size)) => size,
            Ok(None) => {
                self.eof = true;
                return Ok(None);
            }
            Err(e) => {
                // Skip the chunk as FFmpeg does (the size was read before the error).
                return Err(e);
            }
        };

        if size > MAX_BLOCK_LEN {
            self.eof = true;
            return limit_error("ast: block is too large");
        }

        let mut buf = vec![0u8; (size * self.channels) as usize];
        let len = read_up_to(&mut self.reader, &mut buf)?;

        // A block that is cut short by the end of the stream is dropped.
        if len < buf.len() || buf.is_empty() {
            self.eof = true;
            return Ok(None);
        }

        let pts = self.next_pts;
        let frames = self.codec.frames_in(size);
        self.next_pts += frames;

        // The frames after the total are padding.
        let dur = match self.num_frames {
            Some(total) => total.saturating_sub(pts).min(frames),
            None => frames,
        };

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(pts as i64))
                .dur(Duration::new(dur))
                .trim_end(Duration::new(frames - dur))
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

        if self.num_frames.is_some_and(|frames| required_ts >= frames) {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // The block that holds the required frame.
        let mut idx = self.index.partition_point(|b| b.pts + b.frames <= required_ts);

        if idx >= self.index.len() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // The history of the ADPCM starts from silence: begin with the block before.
        if mode == SeekMode::Accurate && self.codec == Codec::Afc {
            idx = idx.saturating_sub(1);
        }

        let block = self.index[idx];
        self.reader.seek(SeekFrom::Start(block.pos))?;
        self.next_pts = block.pts;
        self.eof = false;

        Ok(SeekedTo {
            track_id: 0,
            required_ts: Timestamp::new(required_ts as i64),
            actual_ts: Timestamp::new(block.pts as i64),
        })
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
