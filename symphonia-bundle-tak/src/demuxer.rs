// Symphonia TAK demuxer
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The demuxer of `.tak` files.
//!
//! Ported from FFmpeg's `libavformat/takdec.c` (LGPL-2.1+) and the frame boundary detection of
//! `libavcodec/tak_parser.c`: the file header is a list of metadata blocks, which the stream
//! information, the position of the last frame and the MD5 of the audio come from; the audio is
//! a sequence of self-delimiting frames, that are found by their sync id and the CRC-24 of their
//! headers.

use std::io::{Read, Seek, SeekFrom};

use log::warn;

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_TAK;
use symphonia_core::codecs::audio::{AudioCodecParameters, VerificationCheck};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Error, Result, SeekErrorKind, decode_error, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, TimeBase, Timestamp};

use crate::info::{
    FRAME_FLAG_IS_LAST, FrameHeader, MAX_FRAME_HEADER_BYTES, METADATA_END,
    METADATA_LAST_FRAME, METADATA_MD5, METADATA_STREAMINFO, StreamInfo, crc24,
};

/// The marker of the file header.
const TAK_MARKER: [u8; 4] = *b"tBaK";

/// The size of the chunks the file is read in.
const CHUNK_BYTES: usize = 16 * 1024;

/// The largest metadata block (that is read into memory) the demuxer accepts.
const MAX_BLOCK_BYTES: u32 = 1 << 20;

/// The size of the data of the last frame metadata block: the 40-bit position of the last frame
/// from the start of the audio data, and its 24-bit size.
const LAST_FRAME_BLOCK_BYTES: u32 = 8;

const TAK_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"TAK\0")),
    short_name: "tak",
    long_name: "TAK (Tom's lossless Audio Kompressor)",
};

/// The channels of a stream that has no channel layout: a mono or a stereo layout, otherwise
/// unpositioned channels.
fn default_channels(count: u32) -> Channels {
    match count {
        1 => Channels::Positioned(Position::FRONT_CENTER),
        2 => Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
        _ => Channels::Discrete(count as u16),
    }
}

/// The channels of the stream information.
fn channels_of(info: &StreamInfo) -> Channels {
    // The channels are stored in the order of the bits of the layout mask, which is that of the
    // positions of the standard WAVE channel mask.
    match Position::from_wave_channel_mask(info.ch_mask) {
        Some(pos) if info.ch_mask != 0 && pos.bits().count_ones() == info.channels => {
            Channels::Positioned(pos)
        }
        _ => default_channels(info.channels),
    }
}

/// A frame boundary found by [`TakReader::find_header`].
struct FoundHeader {
    pos: u64,
    header: FrameHeader,
}

/// TAK format reader (demuxer).
pub struct TakReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    /// The stream information; the frame headers that hold it update it.
    info: StreamInfo,
    /// The position of the first frame.
    data_start: u64,
    /// The position that the frames end at, if known.
    data_end: Option<u64>,
    /// The data that is read and not yet delivered, from the position `buf_pos`.
    buf: Vec<u8>,
    buf_pos: u64,
    /// The end of the data was read.
    eof: bool,
}

/// Reads a metadata block of the file header, and checks its CRC-24 (which only warns, like the
/// reference does without strict error recognition).
fn read_block(reader: &mut MediaSourceStream<'_>, size: u32, kind: &str) -> Result<Box<[u8]>> {
    if size <= 3 || size > MAX_BLOCK_BYTES {
        return decode_error("tak: invalid metadata block size");
    }

    let data = reader.read_boxed_slice_exact(size as usize - 3)?;
    let crc = reader.read_u24()?;

    if crc24(&data) != crc {
        warn!("tak: {kind} metadata block crc mismatch");
    }

    Ok(data)
}

impl<'s> TakReader<'s> {
    pub fn try_new(mut reader: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let mut streaminfo: Option<Box<[u8]>> = None;
        let mut last_frame: Option<(u64, u64)> = None;
        let mut md5: Option<[u8; 16]> = None;

        if reader.read_quad_bytes()? == TAK_MARKER {
            loop {
                let kind = reader.read_byte()? & 0x7f;
                let size = reader.read_u24()?;

                match kind {
                    METADATA_STREAMINFO => {
                        if streaminfo.is_some() {
                            return decode_error("tak: duplicate stream info");
                        }
                        streaminfo = Some(read_block(&mut reader, size, "stream info")?);
                    }
                    METADATA_LAST_FRAME => {
                        let data = read_block(&mut reader, size, "last frame")?;
                        if size != LAST_FRAME_BLOCK_BYTES + 3 {
                            return decode_error("tak: invalid last frame metadata");
                        }
                        let mut br = crate::bits::BitReader::new(&data);
                        let pos = br.get64(40);
                        let len = br.get64(24);
                        last_frame = Some((pos, len));
                    }
                    METADATA_MD5 => {
                        if size != 19 {
                            return decode_error("tak: invalid md5 metadata");
                        }
                        let data = read_block(&mut reader, size, "md5")?;
                        let mut sum = [0u8; 16];
                        sum.copy_from_slice(&data);
                        md5 = Some(sum);
                    }
                    METADATA_END => break,
                    _ => reader.ignore_bytes(u64::from(size))?,
                }
            }
        }
        else {
            // A stream of frames, that has no file header.
            reader.seek_buffered_rev(4);
        }

        let data_start = reader.pos();

        let mut this = TakReader {
            reader,
            media_info: MediaInfo::default(),
            tracks: Vec::new(),
            metadata: opts.external_data.metadata.unwrap_or_default(),
            info: StreamInfo::default(),
            data_start,
            data_end: None,
            buf: Vec::new(),
            buf_pos: data_start,
            eof: false,
        };

        // The stream information of the file header, or if the file has no header, of the first
        // frame.
        let (info, extra_data) = match streaminfo {
            Some(data) => (StreamInfo::from_bytes(&data)?, data),
            None => this.info_of_first_frame()?,
        };

        if info.sample_rate == 0 || info.channels == 0 {
            return decode_error("tak: invalid stream info");
        }

        this.info = info;

        let byte_len = this.reader.byte_len();

        // The end of the frames, from the position and the size of the last frame.
        if let Some((pos, len)) = last_frame {
            let end = data_start.saturating_add(pos).saturating_add(len);
            this.data_end = Some(byte_len.map_or(end, |l| end.min(l)));
        }

        // The number of samples is in the stream information; streams that were written without
        // knowing it have 0 there, and then the last frame tells it.
        let mut num_frames = this.info.samples;
        if num_frames == 0 && this.reader.is_seekable() {
            num_frames = this.duration_from_last_frame(last_frame).unwrap_or(0);
        }

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_TAK)
            .with_sample_rate(this.info.sample_rate)
            .with_bits_per_sample(this.info.bps)
            .with_channels(channels_of(&this.info))
            .with_max_frames_per_packet(u64::from(this.info.frame_samples))
            .with_extra_data(extra_data);

        if let Some(md5) = md5 {
            params.with_verification_code(VerificationCheck::Md5(md5));
        }

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(params));

        if let Some(tb) = TimeBase::try_from_recip(this.info.sample_rate) {
            track.with_time_base(tb);
        }

        if num_frames > 0 {
            track.with_num_frames(num_frames);
        }

        this.media_info = MediaInfo::from_track(&track);
        this.tracks.push(track);

        // Frames are read from the start of the audio data.
        this.buf.clear();
        this.eof = false;
        if this.reader.pos() != data_start {
            if this.reader.is_seekable() {
                this.reader.seek(SeekFrom::Start(data_start))?;
            }
            else {
                this.reader.seek_buffered(data_start);
            }
        }

        Ok(this)
    }

    /// The stream information of a file without a file header, and the bytes that it is in,
    /// from the first frame.
    fn info_of_first_frame(&mut self) -> Result<(StreamInfo, Box<[u8]>)> {
        self.fill_to(MAX_FRAME_HEADER_BYTES)?;

        let header = FrameHeader::parse(&self.buf)?;

        match header.info {
            Some(info) => {
                // The stream information starts after the sync id, the flags and the number of the
                // frame (40 bits), and the size of the last frame (16 bits) if it is that.
                let first = if header.flags & FRAME_FLAG_IS_LAST != 0 { 7 } else { 5 };
                let data = &self.buf[first..header.size - 3];
                Ok((info, data.into()))
            }
            None => decode_error("tak: no stream info"),
        }
    }

    /// The number of samples of the stream from its last frame, that is the number of its
    /// samples before it and its own.
    fn duration_from_last_frame(&mut self, last_frame: Option<(u64, u64)>) -> Option<u64> {
        let (pos, _) = last_frame?;
        let found = self.find_header(self.data_start.checked_add(pos)?, None).ok()??;

        if found.header.flags & FRAME_FLAG_IS_LAST == 0 {
            return None;
        }

        let before = u64::from(found.header.frame_num) * u64::from(self.info.frame_samples);
        Some(before + u64::from(found.header.last_frame_samples))
    }

    /// Reads more data into the buffer. Returns the number of bytes read.
    fn fill(&mut self) -> Result<usize> {
        if self.eof {
            return Ok(0);
        }

        let mut want = CHUNK_BYTES;

        if let Some(end) = self.data_end {
            let have_to = self.buf_pos + self.buf.len() as u64;
            want = want.min(end.saturating_sub(have_to) as usize);
            if want == 0 {
                self.eof = true;
                return Ok(0);
            }
        }

        let old = self.buf.len();
        self.buf.resize(old + want, 0);

        let n = loop {
            match Read::read(&mut self.reader, &mut self.buf[old..]) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
                Err(e) => {
                    self.buf.truncate(old);
                    return Err(Error::IoError(e));
                }
            }
        };

        self.buf.truncate(old + n);

        if n == 0 {
            self.eof = true;
        }

        Ok(n)
    }

    /// Reads until the buffer holds at least `len` bytes, or the data ends.
    fn fill_to(&mut self, len: usize) -> Result<()> {
        while self.buf.len() < len && self.fill()? > 0 {}
        Ok(())
    }

    /// Finds the first frame header at or after the index `from` of the buffer, reading more data
    /// as needed. Returns `None` if there is none before the end of the data.
    fn find_header_in_buf(&mut self, mut from: usize) -> Result<Option<(usize, FrameHeader)>> {
        loop {
            // A header needs up to the most bytes of a header to be told from other data.
            while from + 1 < self.buf.len() {
                if self.buf[from] == 0xff && self.buf[from + 1] == 0xa0 {
                    if self.buf.len() - from < MAX_FRAME_HEADER_BYTES && !self.eof {
                        break;
                    }
                    if let Ok(header) = FrameHeader::parse(&self.buf[from..]) {
                        return Ok(Some((from, header)));
                    }
                }
                from += 1;
            }

            if self.fill()? == 0 {
                // No more data: the bytes that are left may only be the start of a header.
                while from + 1 < self.buf.len() {
                    if self.buf[from] == 0xff && self.buf[from + 1] == 0xa0 {
                        if let Ok(header) = FrameHeader::parse(&self.buf[from..]) {
                            return Ok(Some((from, header)));
                        }
                    }
                    from += 1;
                }
                return Ok(None);
            }
        }
    }

    /// Finds the first frame header at or after the position `from` of the stream, and before
    /// `to`, if given. The reader is left at an unspecified position.
    fn find_header(&mut self, from: u64, to: Option<u64>) -> Result<Option<FoundHeader>> {
        let limit = self.data_end.map_or(to, |e| Some(to.map_or(e, |t| t.min(e))));

        let mut tmp = vec![0u8; CHUNK_BYTES + MAX_FRAME_HEADER_BYTES];
        let mut pos = from;

        loop {
            if limit.is_some_and(|l| pos >= l) {
                return Ok(None);
            }

            self.reader.seek(SeekFrom::Start(pos))?;

            let mut n = 0;
            while n < tmp.len() {
                match Read::read(&mut self.reader, &mut tmp[n..]) {
                    Ok(0) => break,
                    Ok(c) => n += c,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
                    Err(e) => return Err(Error::IoError(e)),
                }
            }

            let at_end = n < tmp.len();
            // The positions that are searched in this chunk.
            let searched = if at_end { n } else { CHUNK_BYTES };

            for i in 0..searched.saturating_sub(1) {
                if limit.is_some_and(|l| pos + i as u64 >= l) {
                    return Ok(None);
                }

                if tmp[i] == 0xff && tmp[i + 1] == 0xa0 {
                    if let Ok(header) = FrameHeader::parse(&tmp[i..n]) {
                        return Ok(Some(FoundHeader { pos: pos + i as u64, header }));
                    }
                }
            }

            if at_end {
                return Ok(None);
            }

            pos += CHUNK_BYTES as u64;
        }
    }

    /// The duration in samples of the frame of a header.
    fn frame_duration(&self, header: &FrameHeader) -> u64 {
        let samples = match (header.last_frame_samples, &header.info) {
            (0, Some(info)) => info.frame_samples,
            (0, None) => self.info.frame_samples,
            (n, _) => n,
        };
        u64::from(samples)
    }

    /// The first sample of the frame of a header.
    fn frame_start(&self, header: &FrameHeader) -> u64 {
        let frame_samples = header.info.as_ref().map_or(self.info.frame_samples, |i| i.frame_samples);
        u64::from(header.frame_num) * u64::from(frame_samples)
    }
}

impl Scoreable for TakReader<'_> {
    fn score(mut mss: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(match mss.read_quad_bytes() {
            Ok(b) if b == TAK_MARKER => Score::Supported(255),
            _ => Score::Unsupported,
        })
    }
}

impl ProbeableFormat<'_> for TakReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(TakReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(TAK_FORMAT_INFO, &["tak"], &["audio/x-tak"], &[b"tBaK"])]
    }
}

impl FormatReader for TakReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &TAK_FORMAT_INFO
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
        // Find the start of a frame: the data is at the start of one, unless the reader was
        // positioned in the middle of the data, or the data is damaged.
        self.fill_to(MAX_FRAME_HEADER_BYTES)?;

        let header = match FrameHeader::parse(&self.buf) {
            Ok(header) => header,
            Err(_) => match self.find_header_in_buf(1)? {
                Some((idx, header)) => {
                    self.buf.drain(..idx);
                    self.buf_pos += idx as u64;
                    header
                }
                None => {
                    self.buf_pos += self.buf.len() as u64;
                    self.buf.clear();
                    return Ok(None);
                }
            },
        };

        // The frame ends where the next one starts, or where the data ends.
        let end = match self.find_header_in_buf(header.size)? {
            Some((idx, _)) => idx,
            None => self.buf.len(),
        };

        let data: Vec<u8> = self.buf.drain(..end).collect();
        self.buf_pos += end as u64;

        if let Some(info) = &header.info {
            self.info.frame_samples = info.frame_samples;
        }

        let ts = self.frame_start(&header);
        let dur = self.frame_duration(&header);

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(ts as i64))
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

        if track.num_frames.is_some_and(|n| ts.get() as u64 >= n) {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let end = match self.data_end.or(self.reader.byte_len()) {
            Some(end) => end,
            None => return seek_error(SeekErrorKind::Unseekable),
        };

        let frame_samples = u64::from(self.info.frame_samples.max(1));
        let target = ts.get() as u64 / frame_samples;

        // Search the frame by its number: `lo` is the position of a frame that is not after the
        // target, and the frames from `hi` are after it.
        let mut lo = match self.find_header(self.data_start, Some(end))? {
            Some(found) => found,
            None => return seek_error(SeekErrorKind::OutOfRange),
        };
        let mut hi = end;

        while hi - lo.pos > 2 * CHUNK_BYTES as u64 {
            let mid = lo.pos + (hi - lo.pos) / 2;

            match self.find_header(mid, Some(hi))? {
                Some(found) if u64::from(found.header.frame_num) <= target => lo = found,
                Some(found) => hi = found.pos,
                None => hi = mid,
            }
        }

        // Walk the frames of the remaining range.
        loop {
            let from = lo.pos + lo.header.size as u64;

            match self.find_header(from, Some(hi))? {
                Some(found) if u64::from(found.header.frame_num) <= target => lo = found,
                _ => break,
            }
        }

        let actual_ts = self.frame_start(&lo.header);

        self.reader.seek(SeekFrom::Start(lo.pos))?;
        self.buf.clear();
        self.buf_pos = lo.pos;
        self.eof = false;

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
