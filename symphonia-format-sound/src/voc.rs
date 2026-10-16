// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Creative Voice File (`.voc`) reader.
//!
//! The file is a header followed by blocks. The blocks of the types used are described in
//! "Creative Voice File format" (Creative Labs, Sound Blaster Programmer's Guide; the Multimedia
//! Wiki page "Creative Voice"), and the handling of the blocks, and of the codec numbers,
//! follows FFmpeg's `libavformat/vocdec.c`, `voc_packet.c` and `voc.c`.

use std::io::{self, Seek, SeekFrom};

use symphonia_common::pcm::{PcmLayout, PcmStream};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_ADPCM_CT, CODEC_ID_ADPCM_SBPRO_2, CODEC_ID_ADPCM_SBPRO_3, CODEC_ID_ADPCM_SBPRO_4,
    CODEC_ID_PCM_ALAW, CODEC_ID_PCM_MULAW, CODEC_ID_PCM_S16LE, CODEC_ID_PCM_U8,
};
use symphonia_core::codecs::audio::{AudioCodecId, AudioCodecParameters};
use symphonia_core::common::FourCc;
use symphonia_core::errors::unsupported_error;
use symphonia_core::errors::{Error, Result, SeekErrorKind, decode_error, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{
    Metadata, MetadataBuilder, MetadataId, MetadataInfo, MetadataLog, StandardTag, Tag,
};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use crate::common::{channels_from_count, clamp_data_end};

use log::{debug, warn};

/// The signature at the start of the file. It ends with the 0x1A (^Z) EOF character.
const VOC_MAGIC: &[u8; 20] = b"Creative Voice File\x1A";

/// The size of the header, as stated by it: the signature, this size, the version, and the
/// version check.
const VOC_HEADER_SIZE: u16 = 0x1a;

/// The limit of the text blocks that are read as comments.
const MAX_COMMENTS: usize = 64;
const MAX_COMMENT_LEN: u64 = 64 * 1024;

const VOC_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"VOC ")),
    short_name: "voc",
    long_name: "Creative Voice File",
};

const VOC_METADATA_INFO: MetadataInfo = MetadataInfo {
    metadata: MetadataId::new(FourCc::new(*b"VOC ")),
    short_name: "voc",
    long_name: "Creative Voice File text",
};

// Block types.
const BLOCK_EOF: u8 = 0x00;
const BLOCK_VOICE_DATA: u8 = 0x01;
const BLOCK_VOICE_DATA_CONT: u8 = 0x02;
const BLOCK_ASCII: u8 = 0x05;
const BLOCK_EXTENDED: u8 = 0x08;
const BLOCK_NEW_VOICE_DATA: u8 = 0x09;

/// The audio format stated by the first block of sound data.
#[derive(Copy, Clone, Debug)]
struct Format {
    rate: u32,
    channels: u32,
    /// The bits per sample stated by a "new voice data" block.
    bits: Option<u8>,
    /// The codec number.
    codec: u16,
}

/// A parsed block.
enum Block {
    /// The end of the blocks.
    End,
    /// Sound data of `len` bytes at the position `pos`. The format is stated by the blocks that
    /// begin the data, and is not by the continuation of it.
    Data { pos: u64, len: u64, format: Option<Format> },
    /// A text comment.
    Text(Vec<u8>),
    /// Any other block, which has been skipped.
    Other,
}

/// Parses blocks.
struct Blocks {
    /// The sample rate and channels of the extended block, which applies to the next block of
    /// voice data.
    extended: Option<(u32, u32)>,
}

impl Blocks {
    /// Reads the next block. The stream must be at the start of a block. A block of sound data is
    /// not skipped: the stream is left at the start of the samples.
    fn next(&mut self, mss: &mut MediaSourceStream<'_>, want_text: bool) -> Result<Block> {
        let block_type = match mss.read_u8() {
            Ok(t) => t,
            // A file that is cut off at a block boundary has no EOF block.
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(Block::End),
            Err(err) => return Err(err.into()),
        };

        if block_type == BLOCK_EOF {
            return Ok(Block::End);
        }

        let mut len = u64::from(mss.read_u24()?);

        // A block that is stated to be empty extends to the end of the file, if it is known.
        if len == 0 {
            match mss.byte_len() {
                Some(end) => len = end.saturating_sub(mss.pos()),
                None => return decode_error("voc: block of unknown length"),
            }
        }

        match block_type {
            BLOCK_VOICE_DATA => {
                if len < 2 {
                    return decode_error("voc: voice data block is too short");
                }

                let divisor = mss.read_u8()?;
                let codec = u16::from(mss.read_u8()?);

                let (rate, channels) = match self.extended.take() {
                    Some(ext) => ext,
                    None => (1_000_000 / (256 - u32::from(divisor)), 1),
                };

                let format = Format { rate, channels, bits: None, codec };

                Ok(Block::Data { pos: mss.pos(), len: len - 2, format: Some(format) })
            }
            BLOCK_VOICE_DATA_CONT => Ok(Block::Data { pos: mss.pos(), len, format: None }),
            BLOCK_EXTENDED => {
                if len < 4 {
                    return decode_error("voc: extended block is too short");
                }

                let time_constant = u32::from(mss.read_u16()?);
                let _pack = mss.read_u8()?;
                let mode = mss.read_u8()?;

                if mode > 1 {
                    return decode_error("voc: invalid extended block mode");
                }

                let channels = u32::from(mode) + 1;

                self.extended =
                    Some((256_000_000 / (channels * (65536 - time_constant)), channels));

                // The block states 4 bytes, even if it is stated to be longer.
                mss.ignore_bytes(len - 4)?;
                Ok(Block::Other)
            }
            BLOCK_NEW_VOICE_DATA => {
                if len < 12 {
                    return decode_error("voc: new voice data block is too short");
                }

                let rate = mss.read_u32()?;
                let bits = mss.read_u8()?;
                let channels = u32::from(mss.read_u8()?);
                let codec = mss.read_u16()?;
                let _reserved = mss.read_u32()?;

                let format = Format { rate, channels, bits: Some(bits), codec };

                Ok(Block::Data { pos: mss.pos(), len: len - 12, format: Some(format) })
            }
            BLOCK_ASCII if want_text && len <= MAX_COMMENT_LEN => {
                let mut text = vec![0; len as usize];
                mss.read_buf_exact(&mut text)?;
                Ok(Block::Text(text))
            }
            _ => {
                // Silence (3), markers (4), text, and repeats (6 and 7) do not hold samples. FFmpeg
                // skips them as well.
                mss.ignore_bytes(len)?;
                Ok(Block::Other)
            }
        }
    }
}

/// The codec of the first block of sound data.
struct Codec {
    id: AudioCodecId,
    layout: PcmLayout,
    /// The bits per coded sample of the codec parameters.
    bits: Option<u32>,
}

fn map_codec(format: &Format) -> Result<Codec> {
    let channels = format.channels;

    // The codec numbers are those of FFmpeg's `ff_voc_codec_tags`.
    let (id, layout, bits) = match (format.codec, format.bits) {
        (0x00, None | Some(8)) => {
            (CODEC_ID_PCM_U8, PcmLayout::from_sample_bits(8, channels), Some(8))
        }
        (0x04, None | Some(16)) => {
            (CODEC_ID_PCM_S16LE, PcmLayout::from_sample_bits(16, channels), Some(16))
        }
        (0x06, None | Some(8)) => {
            (CODEC_ID_PCM_ALAW, PcmLayout::from_sample_bits(8, channels), None)
        }
        (0x07, None | Some(8)) => {
            (CODEC_ID_PCM_MULAW, PcmLayout::from_sample_bits(8, channels), None)
        }
        // Sound Blaster Pro ADPCM: a byte is 2 samples (4-bit), 3 samples (2.6-bit), or 4
        // samples (2-bit) of one channel.
        (0x01, _) if channels == 1 => (CODEC_ID_ADPCM_SBPRO_4, PcmLayout::new(1, 2), Some(4)),
        (0x02, _) if channels == 1 => (CODEC_ID_ADPCM_SBPRO_3, PcmLayout::new(1, 3), Some(3)),
        (0x03, _) if channels == 1 => (CODEC_ID_ADPCM_SBPRO_2, PcmLayout::new(1, 4), Some(2)),
        // Creative ADPCM: a byte is 2 samples of one channel, or 1 of each of two channels.
        (0x200, _) if channels == 1 => (CODEC_ID_ADPCM_CT, PcmLayout::new(1, 2), Some(4)),
        (0x200, _) if channels == 2 => (CODEC_ID_ADPCM_CT, PcmLayout::new(1, 1), Some(4)),
        _ => return unsupported_error("voc: unsupported codec"),
    };

    let layout = layout.ok_or(Error::DecodeError("voc: invalid channel count"))?;

    Ok(Codec { id, layout, bits })
}

/// A block of sound data in the stream.
#[derive(Copy, Clone, Debug)]
struct Segment {
    /// The position of the first sample.
    pos: u64,
    /// The length in bytes.
    len: u64,
    /// The timestamp of the first frame.
    ts: u64,
}

/// The segment that packets are read from.
struct Current {
    stream: PcmStream,
    ts: u64,
}

/// Creative Voice File format reader.
///
/// `VocReader` implements a demuxer for Creative Voice Files. The sample rate, number of
/// channels, and codec are those of the first block of sound data; later blocks that change them
/// are read with the first format, as FFmpeg does. Silence blocks and repeat blocks are not
/// applied.
///
/// The codecs are unsigned 8-bit and signed 16-bit PCM, A-law, mu-law, Sound Blaster Pro ADPCM
/// (4, 2.6, and 2-bit), and Creative ADPCM. If the source is seekable, the blocks are read when
/// the reader is created, which gives the duration of the audio, and allows seeking.
pub struct VocReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    blocks: Blocks,
    layout: PcmLayout,
    /// The blocks of sound data, if the source is seekable.
    segments: Vec<Segment>,
    /// The segment that is being read.
    current: Option<Current>,
    /// The timestamp of the next segment.
    next_ts: u64,
}

impl<'s> VocReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, mut opts: FormatOptions) -> Result<Self> {
        let mut magic = [0; 20];
        mss.read_buf_exact(&mut magic)?;

        if &magic != VOC_MAGIC {
            return unsupported_error("voc: missing magic number");
        }

        // FFmpeg requires the size of the header to be that of version 1.10 and 1.20 files.
        if mss.read_u16()? != VOC_HEADER_SIZE {
            return unsupported_error("voc: unsupported header size");
        }

        let _version = mss.read_u16()?;
        let _version_check = mss.read_u16()?;

        let mut metadata = opts.external_data.metadata.take().unwrap_or_default();
        let mut comments = Vec::new();

        let mut blocks = Blocks { extended: None };

        // The first block of sound data states the format.
        let (first, format) = loop {
            match blocks.next(&mut mss, true)? {
                Block::End => return decode_error("voc: no sound data"),
                Block::Data { pos, len, format: Some(format) } => {
                    // The block may be longer than the file.
                    let len = clamp_data_end(&mss, pos, Some(len)).map_or(len, |end| end - pos);
                    break (Segment { pos, len, ts: 0 }, format);
                }
                Block::Data { len, .. } => {
                    // Continuation of data that was not started.
                    mss.ignore_bytes(len)?;
                }
                Block::Text(text) => comments.push(text),
                Block::Other => (),
            }
        };

        if format.rate == 0 {
            return decode_error("voc: invalid sample rate");
        }

        let codec = map_codec(&format)?;
        let layout = codec.layout;

        let mut params = AudioCodecParameters::new();

        params
            .for_codec(codec.id)
            .with_sample_rate(format.rate)
            .with_channels(channels_from_count(format.channels)?)
            .with_max_frames_per_packet(layout.max_frames_per_packet());

        if let Some(bits) = codec.bits {
            params.with_bits_per_coded_sample(bits);
        }

        let mut track = Track::new(0);

        track.with_codec_params(CodecParameters::Audio(params)).with_start_ts(Timestamp::new(0));

        // Read the remaining blocks if the source can be seeked to them: they give the duration,
        // and the places to seek to.
        let mut segments = Vec::new();

        if mss.is_seekable() {
            let first_pos = first.pos;

            segments.push(first);
            mss.ignore_bytes(first.len)?;

            let mut ts = layout.num_frames(first.len);

            while let Some(block) = scan_block(&mut blocks, &mut mss, &mut comments)? {
                let Segment { pos, len, .. } = block;

                segments.push(Segment { pos, len, ts });
                ts += layout.num_frames(len);
            }

            let num_frames = ts;

            track.with_num_frames(num_frames).with_duration(Duration::new(num_frames));

            mss.seek(SeekFrom::Start(first_pos))?;
            blocks.extended = None;
        }

        let mut builder = MetadataBuilder::new(VOC_METADATA_INFO);

        for text in comments.iter().take(MAX_COMMENTS) {
            // The text ends with a NUL.
            let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
            let text = String::from_utf8_lossy(&text[..end]).into_owned();

            if !text.is_empty() {
                let std = StandardTag::Comment(text.clone().into());
                builder.add_tag(Tag::new_from_parts("COMMENT", text, Some(std)));
            }
        }

        let revision = builder.build();

        if !revision.media.tags.is_empty() {
            metadata.push(revision);
        }

        debug!("voc: {} Hz, {} channels, codec {:#x}", format.rate, format.channels, format.codec);

        let current = Current {
            stream: PcmStream {
                layout,
                start: first.pos,
                end: Some(first.pos + first.len),
                track_id: 0,
            },
            ts: 0,
        };

        let next_ts = layout.num_frames(first.len);

        Ok(VocReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            metadata,
            chapters: opts.external_data.chapters,
            blocks,
            layout,
            segments,
            current: Some(current),
            next_ts,
        })
    }

    /// Reads the next block of sound data, from the stream position.
    fn next_segment(&mut self) -> Result<Option<Current>> {
        loop {
            match self.blocks.next(&mut self.reader, false)? {
                Block::End => return Ok(None),
                Block::Data { pos, len, format } => {
                    if format.is_some() {
                        debug!("voc: ignoring the format of a later sound block");
                    }

                    let ts = self.next_ts;
                    self.next_ts += self.layout.num_frames(len);

                    return Ok(Some(Current {
                        stream: PcmStream {
                            layout: self.layout,
                            start: pos,
                            end: Some(pos + len),
                            track_id: 0,
                        },
                        ts,
                    }));
                }
                Block::Text(_) | Block::Other => (),
            }
        }
    }
}

/// Reads the blocks until the next block of sound data, and skips its samples. Returns `None` at
/// the end.
fn scan_block(
    blocks: &mut Blocks,
    mss: &mut MediaSourceStream<'_>,
    comments: &mut Vec<Vec<u8>>,
) -> Result<Option<Segment>> {
    loop {
        match blocks.next(mss, comments.len() < MAX_COMMENTS)? {
            Block::End => return Ok(None),
            Block::Data { pos, len, format } => {
                if format.is_some() {
                    warn!("voc: ignoring the format of a later sound block");
                }

                // The block may be longer than the file.
                let end = mss.byte_len().map_or(pos + len, |end| (pos + len).min(end));
                let len = end.saturating_sub(pos);

                mss.ignore_bytes(len)?;
                return Ok(Some(Segment { pos, len, ts: 0 }));
            }
            Block::Text(text) => comments.push(text),
            Block::Other => (),
        }
    }
}

impl Scoreable for VocReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut magic = [0; 20];
        src.read_buf_exact(&mut magic)?;

        if &magic != VOC_MAGIC {
            return Ok(Score::Unsupported);
        }

        if src.read_u16()? != VOC_HEADER_SIZE {
            return Ok(Score::Unsupported);
        }

        // The version check of the header: the complement of the version, plus 0x1234.
        let version = src.read_u16()?;
        let check = src.read_u16()?;

        Ok(Score::Supported(if (!version).wrapping_add(0x1234) == check { 255 } else { 100 }))
    }
}

impl<'s> ProbeableFormat<'s> for VocReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'s>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + 's>> {
        Ok(Box::new(VocReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            VOC_FORMAT_INFO,
            &["voc"],
            &["audio/x-voc"],
            // The probe supports markers of up to 16 bytes: the rest of the signature is checked
            // when scoring.
            &[b"Creative Voice F"]
        )]
    }
}

impl FormatReader for VocReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &VOC_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(current) = &self.current {
                if let Some(mut packet) = current.stream.next_packet(&mut self.reader)? {
                    packet.pts = Timestamp::try_from(current.ts + packet.pts.get() as u64)
                        .map_err(|_| Error::DecodeError("voc: timestamp overflow"))?;

                    return Ok(Some(packet));
                }

                // Skip what is left of the block. A block that is cut off ends the stream.
                if let Some(end) = current.stream.end {
                    let pos = self.reader.pos();

                    if pos < end {
                        match self.reader.ignore_bytes(end - pos) {
                            Ok(()) => (),
                            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                                self.current = None;
                                return Ok(None);
                            }
                            Err(err) => return Err(err.into()),
                        }
                    }
                }
            }

            self.current = self.next_segment()?;

            if self.current.is_none() {
                return Ok(None);
            }
        }
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        self.chapters.as_ref()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if self.segments.is_empty() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        let track = &self.tracks[0];

        let required_ts = match to {
            SeekTo::Timestamp { ts, track_id: 0 } => ts,
            SeekTo::Time { time, track_id: None | Some(0) } => {
                let tb = track.time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
            _ => return seek_error(SeekErrorKind::Unseekable),
        };

        let Ok(ts) = u64::try_from(required_ts.get())
        else {
            return seek_error(SeekErrorKind::OutOfRange);
        };

        if track.num_frames.is_some_and(|n| ts > n) {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // The last segment that starts at or before the timestamp.
        let idx = self.segments.partition_point(|s| s.ts <= ts).saturating_sub(1);
        let segment = self.segments[idx];

        let stream = PcmStream {
            layout: self.layout,
            start: segment.pos,
            end: Some(segment.pos + segment.len),
            track_id: 0,
        };

        let relative = Timestamp::try_from(ts - segment.ts)
            .map_err(|_| Error::SeekError(SeekErrorKind::OutOfRange))?;

        let seeked = stream.seek(
            &mut self.reader,
            track,
            SeekTo::Timestamp { ts: relative, track_id: 0 },
        )?;

        self.blocks.extended = None;
        self.next_ts = segment.ts + self.layout.num_frames(segment.len);
        self.current = Some(Current { stream, ts: segment.ts });

        let actual_ts = Timestamp::try_from(segment.ts + seeked.actual_ts.get() as u64)
            .map_err(|_| Error::SeekError(SeekErrorKind::OutOfRange))?;

        Ok(SeekedTo { track_id: 0, actual_ts, required_ts })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}
