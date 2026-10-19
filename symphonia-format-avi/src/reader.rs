// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom};
use std::sync::Arc;

use log::{debug, warn};

use symphonia_common::pcm::required_ts;
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_ADPCM_IMA_WAV, CODEC_ID_ADPCM_MS, CODEC_ID_MP2, CODEC_ID_MP3, CODEC_ID_PCM_ALAW,
    CODEC_ID_PCM_F32LE, CODEC_ID_PCM_F64LE, CODEC_ID_PCM_MULAW, CODEC_ID_PCM_S16LE,
    CODEC_ID_PCM_S24LE, CODEC_ID_PCM_S32LE, CODEC_ID_PCM_U8,
};
use symphonia_core::codecs::audio::{AudioCodecId, AudioCodecParameters};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::TrackFlags;
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{
    Metadata, MetadataBuilder, MetadataId, MetadataInfo, MetadataLog, StandardTag, Tag,
};
use symphonia_core::packet::Packet;
use symphonia_core::support_format;
use symphonia_core::units::{Duration, TimeBase, Timestamp};

use crate::mpeg;

const AVI_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"AVI ")),
    short_name: "avi",
    long_name: "Audio Video Interleave",
};

const AVI_METADATA_INFO: MetadataInfo = MetadataInfo {
    metadata: MetadataId::new(FourCc::new(*b"AVI ")),
    short_name: "avi",
    long_name: "AVI INFO list",
};

/// The largest chunk or header read into memory.
const MAX_HEADER_LEN: u32 = 16 * 1024 * 1024;

/// The maximum number of frames of a PCM packet.
const MAX_PCM_FRAMES: u32 = 1152;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_ADPCM: u16 = 0x0002;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_ALAW: u16 = 0x0006;
const WAVE_FORMAT_MULAW: u16 = 0x0007;
const WAVE_FORMAT_IMA_ADPCM: u16 = 0x0011;
const WAVE_FORMAT_MPEG: u16 = 0x0050;
const WAVE_FORMAT_MPEGLAYER3: u16 = 0x0055;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;

/// How the chunks of a stream are split into packets.
#[derive(Copy, Clone, Debug)]
enum Layout {
    /// Packets of whole blocks of `block` bytes and `frames` frames each, `per_packet` at most.
    Blocks { block: u32, frames: u32, per_packet: u32 },
    /// MPEG audio frames.
    Mpeg,
}

struct Stream {
    /// The index of the stream in the file.
    index: u32,
    layout: Layout,
    /// The timestamp of the next packet.
    next_ts: u64,
}

/// A parsed `WAVEFORMATEX`.
struct WaveFormat {
    tag: u16,
    channels: u16,
    rate: u32,
    block_align: u16,
    bits: u16,
    mask: Option<u32>,
    extra: Vec<u8>,
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn parse_wave_format(b: &[u8]) -> Option<WaveFormat> {
    let mut tag = le16(b, 0)?;
    let channels = le16(b, 2)?;
    let rate = le32(b, 4)?;
    let block_align = le16(b, 12)?;
    let bits = le16(b, 14)?;
    let cb = usize::from(le16(b, 16).unwrap_or(0));
    let ext = b.get(18..).map(|e| &e[..e.len().min(cb)]).unwrap_or(&[]);

    let mut mask = None;
    let mut extra = ext.to_vec();

    if tag == WAVE_FORMAT_EXTENSIBLE {
        // validBitsPerSample (2), channelMask (4), SubFormat GUID (16).
        mask = le32(ext, 2);
        tag = le16(ext, 6)?;
        extra = Vec::new();
    }

    Some(WaveFormat { tag, channels, rate, block_align, bits, mask, extra })
}

/// Maps a stream format to codec parameters and a packet layout.
fn make_params(fmt: &WaveFormat) -> Option<(AudioCodecParameters, Layout)> {
    if fmt.rate == 0 || fmt.channels == 0 {
        return None;
    }

    let ch = u32::from(fmt.channels);
    let bits = u32::from(fmt.bits);

    let channels = fmt
        .mask
        .and_then(Position::from_wave_channel_mask)
        .filter(|pos| pos.bits().count_ones() == ch)
        .or_else(|| Position::from_wave_channel_count(ch))?;

    let mut params = AudioCodecParameters::new();
    params.with_sample_rate(fmt.rate).with_channels(Channels::Positioned(channels));

    let pcm = |params: &mut AudioCodecParameters, codec: AudioCodecId, bits: u32| {
        params.for_codec(codec).with_bits_per_sample(bits).with_bits_per_coded_sample(bits);
        let block = bits / 8 * ch;
        Some(Layout::Blocks { block, frames: 1, per_packet: MAX_PCM_FRAMES })
    };

    let layout = match fmt.tag {
        WAVE_FORMAT_PCM => {
            let codec = match bits {
                8 => CODEC_ID_PCM_U8,
                16 => CODEC_ID_PCM_S16LE,
                24 => CODEC_ID_PCM_S24LE,
                32 => CODEC_ID_PCM_S32LE,
                _ => return None,
            };
            pcm(&mut params, codec, bits)?
        }
        WAVE_FORMAT_IEEE_FLOAT => {
            let codec = match bits {
                32 => CODEC_ID_PCM_F32LE,
                64 => CODEC_ID_PCM_F64LE,
                _ => return None,
            };
            pcm(&mut params, codec, bits)?
        }
        WAVE_FORMAT_ALAW | WAVE_FORMAT_MULAW => {
            let codec =
                if fmt.tag == WAVE_FORMAT_ALAW { CODEC_ID_PCM_ALAW } else { CODEC_ID_PCM_MULAW };
            params.for_codec(codec);
            Layout::Blocks { block: ch, frames: 1, per_packet: MAX_PCM_FRAMES }
        }
        WAVE_FORMAT_ADPCM => {
            let align = u32::from(fmt.block_align);
            let hdr = 7 * ch;
            if align <= hdr {
                return None;
            }
            params.for_codec(CODEC_ID_ADPCM_MS).with_bits_per_coded_sample(4);
            if !fmt.extra.is_empty() {
                params.with_extra_data(fmt.extra.clone().into_boxed_slice());
            }
            Layout::Blocks { block: align, frames: (align - hdr) * 8 / (4 * ch) + 2, per_packet: 1 }
        }
        WAVE_FORMAT_IMA_ADPCM => {
            let align = u32::from(fmt.block_align);
            let hdr = 4 * ch;
            if bits != 4 || align <= hdr {
                return None;
            }
            params.for_codec(CODEC_ID_ADPCM_IMA_WAV).with_bits_per_coded_sample(4);
            if !fmt.extra.is_empty() {
                params.with_extra_data(fmt.extra.clone().into_boxed_slice());
            }
            Layout::Blocks { block: align, frames: (align - hdr) / (4 * ch) * 8 + 1, per_packet: 1 }
        }
        WAVE_FORMAT_MPEG | WAVE_FORMAT_MPEGLAYER3 => {
            // The decoder is selected from the frame headers: use layer III unless the tag says
            // otherwise; the decoders of layers I and II take the same packets.
            let codec = if fmt.tag == WAVE_FORMAT_MPEGLAYER3 { CODEC_ID_MP3 } else { CODEC_ID_MP2 };
            params.for_codec(codec);
            Layout::Mpeg
        }
        _ => return None,
    };

    if let Layout::Blocks { frames, per_packet, .. } = layout {
        params
            .with_max_frames_per_packet(u64::from(frames) * u64::from(per_packet))
            .with_frames_per_block(u64::from(frames));
    }

    Some((params, layout))
}

/// Reads an INFO list into tags.
fn read_info<R: ReadBytes>(reader: &mut R, end: u64, pos: &mut u64) -> Result<Vec<Tag>> {
    let mut tags = Vec::new();

    while *pos + 8 <= end {
        let id = reader.read_quad_bytes()?;
        let len = reader.read_u32()?;
        *pos += 8;

        let padded = u64::from(len) + u64::from(len & 1);
        if len > 1 << 20 || *pos + padded > end {
            break;
        }

        let mut buf = vec![0; len as usize];
        reader.read_buf_exact(&mut buf)?;
        reader.ignore_bytes(u64::from(len & 1))?;
        *pos += padded;

        let text: String = String::from_utf8_lossy(&buf).trim_end_matches('\0').to_owned();
        let value = Arc::new(text.clone());

        let std = match &id {
            b"INAM" => Some(StandardTag::TrackTitle(value)),
            b"IART" => Some(StandardTag::Artist(value)),
            b"IPRD" => Some(StandardTag::Album(value)),
            b"IGNR" => Some(StandardTag::Genre(value)),
            _ => None,
        };

        tags.push(Tag::new_from_parts(String::from_utf8_lossy(&id).into_owned(), text, std));
    }

    Ok(tags)
}

/// An AVI demuxer. See the crate documentation for the conventions.
pub struct AviReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    streams: Vec<Stream>,
    metadata: MetadataLog,
    /// The first `movi` list: the start of its content, and the end of it and of its RIFF.
    first: Segment,
    /// The segment being read.
    cur: Segment,
    /// Packets that have been read but not returned.
    pending: VecDeque<Packet>,
    seekable: bool,
}

#[derive(Copy, Clone)]
struct Segment {
    /// The start and end of the `movi` list content.
    movi: (u64, u64),
    /// The end of the RIFF chunk.
    riff_end: u64,
}

impl<'s> AviReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        if mss.read_quad_bytes()? != *b"RIFF" {
            return unsupported_error("avi: missing RIFF marker");
        }

        let riff_len = u64::from(mss.read_u32()?);

        if mss.read_quad_bytes()? != *b"AVI " {
            return unsupported_error("avi: not an AVI RIFF form");
        }

        let riff_end = clamp(&mss, 8 + riff_len);

        let mut metadata = opts.external_data.metadata.unwrap_or_default();
        let mut tracks: Vec<Track> = Vec::new();
        let mut streams = Vec::new();
        let mut stream_count = 0u32;
        let mut movi = None;

        while mss.pos() + 8 <= riff_end {
            let id = mss.read_quad_bytes()?;
            let len = u64::from(mss.read_u32()?);
            let body = mss.pos();
            let end = body + len;

            if id == *b"LIST" {
                let form = mss.read_quad_bytes()?;

                match &form {
                    b"movi" => {
                        movi = Some((body + 4, clamp(&mss, end)));
                        break;
                    }
                    b"hdrl" => {
                        read_hdrl(&mut mss, end, &mut stream_count, &mut tracks, &mut streams)?;
                    }
                    b"INFO" => {
                        let mut pos = body + 4;
                        let tags = read_info(&mut mss, end, &mut pos).unwrap_or_default();
                        if !tags.is_empty() {
                            let mut builder = MetadataBuilder::new(AVI_METADATA_INFO);
                            for tag in tags {
                                builder.add_tag(tag);
                            }
                            metadata.push(builder.build());
                        }
                    }
                    _ => (),
                }
            }

            // Move to the next chunk (chunks are word aligned).
            let next = end + (len & 1);
            skip_to(&mut mss, next)?;
        }

        let Some(movi) = movi
        else {
            return decode_error("avi: no movi list");
        };

        if tracks.is_empty() {
            return unsupported_error("avi: no supported audio streams");
        }

        tracks[0].with_flags(TrackFlags::DEFAULT);

        skip_to(&mut mss, movi.0)?;

        let first = Segment { movi, riff_end };

        debug!("avi: {} audio tracks, movi at {}..{}", tracks.len(), movi.0, movi.1);

        Ok(AviReader {
            media_info: MediaInfo::from_tracks(&tracks),
            seekable: mss.is_seekable(),
            reader: mss,
            tracks,
            streams,
            metadata,
            first,
            cur: first,
            pending: VecDeque::new(),
        })
    }

    /// Moves to the `movi` list of the next `RIFF AVIX` chunk. Returns false at the end.
    fn next_segment(&mut self) -> Result<bool> {
        skip_to(&mut self.reader, self.cur.riff_end)?;

        loop {
            let pos = self.reader.pos();

            if self.reader.byte_len().is_some_and(|len| pos + 12 > len) {
                return Ok(false);
            }

            let Ok(marker) = self.reader.read_quad_bytes()
            else {
                return Ok(false);
            };
            let len = u64::from(self.reader.read_u32()?);
            let Ok(form) = self.reader.read_quad_bytes()
            else {
                return Ok(false);
            };

            if marker != *b"RIFF" || form != *b"AVIX" {
                return Ok(false);
            }

            let riff_end = clamp(&self.reader, pos + 8 + len);

            // Find the movi list.
            while self.reader.pos() + 12 <= riff_end {
                let id = self.reader.read_quad_bytes()?;
                let len = u64::from(self.reader.read_u32()?);
                let body = self.reader.pos();
                let end = body + len;

                if id == *b"LIST" && self.reader.read_quad_bytes()? == *b"movi" {
                    self.cur = Segment { movi: (body + 4, clamp(&self.reader, end)), riff_end };
                    return Ok(true);
                }

                skip_to(&mut self.reader, end + (len & 1))?;
            }

            skip_to(&mut self.reader, riff_end)?;
            self.cur.riff_end = riff_end;
        }
    }

    /// Reads chunks until a packet of a track is available.
    fn fill(&mut self) -> Result<bool> {
        loop {
            if self.reader.pos() + 8 > self.cur.movi.1 {
                if !self.next_segment()? {
                    return Ok(false);
                }
                continue;
            }

            let id = self.reader.read_quad_bytes()?;
            let len = self.reader.read_u32()?;
            let body = self.reader.pos();

            // Descend into lists (`rec `).
            if id == *b"LIST" {
                if len < 4 {
                    return decode_error("avi: invalid list size");
                }
                self.reader.ignore_bytes(4)?;
                continue;
            }

            let next = body + u64::from(len) + u64::from(len & 1);

            let stream = match (id[0], id[1], &id[2..]) {
                (a @ b'0'..=b'9', b @ b'0'..=b'9', b"wb") => {
                    Some(u32::from(a - b'0') * 10 + u32::from(b - b'0'))
                }
                _ => None,
            };

            let Some(si) = stream.and_then(|n| self.streams.iter().position(|s| s.index == n))
            else {
                skip_to(&mut self.reader, next)?;
                continue;
            };

            // Truncated chunk: read what is there.
            let avail = self.cur.movi.1.saturating_sub(body).min(u64::from(len)) as usize;
            let mut data = Vec::new();
            data.try_reserve_exact(avail).map_err(|_| Error::DecodeError("avi: out of memory"))?;
            let read = std::io::Read::read_to_end(
                &mut std::io::Read::take(&mut self.reader, avail as u64),
                &mut data,
            )?;
            data.truncate(read);
            skip_to(&mut self.reader, next)?;

            let track_id = self.tracks[si].id;
            let stream = &mut self.streams[si];

            match stream.layout {
                Layout::Blocks { block, frames, per_packet } => {
                    let step = (block * per_packet) as usize;
                    let whole = data.len() / block as usize * block as usize;

                    for piece in data[..whole].chunks(step) {
                        let n = (piece.len() / block as usize) as u64 * u64::from(frames);
                        self.pending.push_back(Packet::new(
                            track_id,
                            Timestamp::new(stream.next_ts as i64),
                            Duration::new(n),
                            piece.to_vec().into_boxed_slice(),
                        ));
                        stream.next_ts += n;
                    }
                }
                Layout::Mpeg => {
                    for (a, b, samples) in mpeg::split_frames(&data) {
                        self.pending.push_back(Packet::new(
                            track_id,
                            Timestamp::new(stream.next_ts as i64),
                            Duration::new(samples),
                            data[a..b].to_vec().into_boxed_slice(),
                        ));
                        stream.next_ts += samples;
                    }
                }
            }

            if !self.pending.is_empty() {
                return Ok(true);
            }
        }
    }

    fn next_inner(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(packet) = self.pending.pop_front() {
                return Ok(Some(packet));
            }

            if !self.fill()? {
                return Ok(None);
            }
        }
    }

    fn track_time_base(&self, track_id: u32) -> Option<TimeBase> {
        self.tracks.iter().find(|t| t.id == track_id).and_then(|t| t.time_base)
    }

    fn reset(&mut self) -> Result<()> {
        self.pending.clear();
        self.cur = self.first;
        self.reader.seek(SeekFrom::Start(self.first.movi.0))?;
        for s in &mut self.streams {
            s.next_ts = 0;
        }
        Ok(())
    }
}

/// Clamps an end position to the length of the stream, if known.
fn clamp(mss: &MediaSourceStream<'_>, end: u64) -> u64 {
    mss.byte_len().map_or(end, |len| end.min(len))
}

/// Moves forward to a position, seeking if possible.
fn skip_to(mss: &mut MediaSourceStream<'_>, pos: u64) -> Result<()> {
    let cur = mss.pos();

    if pos > cur {
        if mss.is_seekable() {
            mss.seek(SeekFrom::Start(pos))?;
        }
        else {
            mss.ignore_bytes(pos - cur)?;
        }
    }

    Ok(())
}

fn read_hdrl(
    mss: &mut MediaSourceStream<'_>,
    end: u64,
    stream_count: &mut u32,
    tracks: &mut Vec<Track>,
    streams: &mut Vec<Stream>,
) -> Result<()> {
    while mss.pos() + 8 <= end {
        let id = mss.read_quad_bytes()?;
        let len = mss.read_u32()?;
        let body = mss.pos();
        let chunk_end = body + u64::from(len);

        if id == *b"LIST" && len >= 4 && mss.read_quad_bytes()? == *b"strl" {
            let index = *stream_count;
            *stream_count += 1;

            if let Some((track, stream)) = read_strl(mss, chunk_end, index)? {
                tracks.push(track);
                streams.push(stream);
            }
        }

        skip_to(mss, chunk_end + u64::from(len & 1))?;
    }

    Ok(())
}

fn read_strl(
    mss: &mut MediaSourceStream<'_>,
    end: u64,
    index: u32,
) -> Result<Option<(Track, Stream)>> {
    let mut is_audio = false;
    let mut sample_size = 0u32;
    let mut length = 0u32;
    let mut format = None;

    while mss.pos() + 8 <= end {
        let id = mss.read_quad_bytes()?;
        let len = mss.read_u32()?;
        let body = mss.pos();
        let chunk_end = body + u64::from(len);

        if id == *b"strh" && len >= 48 {
            is_audio = mss.read_quad_bytes()? == *b"auds";
            // handler, flags, priority, language, initial frames, scale, rate, start.
            mss.ignore_bytes(4 + 4 + 2 + 2 + 4 + 4 + 4 + 4)?;
            length = mss.read_u32()?;
            // Suggested buffer size, quality.
            mss.ignore_bytes(8)?;
            sample_size = mss.read_u32()?;
        }
        else if id == *b"strf" && len <= MAX_HEADER_LEN && len >= 16 {
            let mut buf = vec![0; len as usize];
            mss.read_buf_exact(&mut buf)?;
            format = parse_wave_format(&buf);
        }

        skip_to(mss, chunk_end + u64::from(len & 1))?;
    }

    if !is_audio {
        return Ok(None);
    }

    let Some(fmt) = format
    else {
        warn!("avi: audio stream {index} has no format");
        return Ok(None);
    };

    let Some((params, layout)) = make_params(&fmt)
    else {
        warn!("avi: ignoring audio stream {index} with format tag {:#06x}", fmt.tag);
        return Ok(None);
    };

    let Some(time_base) = TimeBase::try_from_recip(fmt.rate)
    else {
        return Ok(None);
    };

    let mut track = Track::new(index);

    track
        .with_codec_params(CodecParameters::Audio(params))
        .with_time_base(time_base)
        .with_start_ts(Timestamp::new(0));

    // For PCM the stream length is in frames, when each frame is a stream sample.
    if let Layout::Blocks { block, frames: 1, .. } = layout {
        if sample_size == block && length > 0 {
            track
                .with_num_frames(u64::from(length))
                .with_duration(Duration::new(u64::from(length)));
        }
    }

    Ok(Some((track, Stream { index, layout, next_ts: 0 })))
}

impl Scoreable for AviReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        if src.read_quad_bytes()? != *b"RIFF" {
            return Ok(Score::Unsupported);
        }

        src.ignore_bytes(4)?;

        if src.read_quad_bytes()? != *b"AVI " {
            return Ok(Score::Unsupported);
        }

        Ok(Score::Supported(255))
    }
}

impl<'s> ProbeableFormat<'s> for AviReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'s>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + 's>> {
        Ok(Box::new(AviReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(AVI_FORMAT_INFO, &["avi"], &["video/x-msvideo", "video/avi"], &[b"RIFF"])]
    }
}

impl FormatReader for AviReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &AVI_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        self.next_inner()
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        None
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        let track_id = match to {
            SeekTo::Timestamp { track_id, .. } => track_id,
            SeekTo::Time { track_id, .. } => track_id.unwrap_or(self.tracks[0].id),
        };

        let Some(si) = self.tracks.iter().position(|t| t.id == track_id)
        else {
            return seek_error(SeekErrorKind::Unseekable);
        };

        let required = required_ts(&self.tracks[si], track_id, to)?;
        let target = required.get() as u64;

        // A packet that is at or after the current position may be reached without rewinding.
        let ahead = self.streams[si].next_ts <= target;

        if !ahead {
            if !self.seekable {
                return seek_error(SeekErrorKind::ForwardOnly);
            }
            self.reset()?;
        }

        // Packets of the other tracks that are read while scanning are kept, so that every track
        // resumes at its first packet that ends after the target time.
        let mut skipped = Vec::new();

        loop {
            let Some(packet) = self.next_inner()?
            else {
                return seek_error(SeekErrorKind::OutOfRange);
            };

            if packet.track_id != track_id {
                skipped.push(packet);
                continue;
            }

            let start = packet.pts.get() as u64;
            let end = start + packet.dur.get();

            if target < end {
                let actual_ts = packet.pts;
                let time_base = self.tracks[si].time_base;

                // Packets of the other tracks that end at or before the target time are
                // dropped. They are given back before the packets that are still pending.
                let target_time = time_base.and_then(|tb| tb.calc_time(required));

                for p in skipped.into_iter().rev() {
                    let before = match (target_time, self.track_time_base(p.track_id)) {
                        (Some(t), Some(tb)) => {
                            tb.calc_time(p.pts.saturating_add(p.dur)).is_some_and(|e| e <= t)
                        }
                        _ => false,
                    };

                    if !before {
                        self.pending.push_front(p);
                    }
                }

                self.pending.push_front(packet);
                return Ok(SeekedTo { track_id, actual_ts, required_ts: required });
            }
        }
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}
