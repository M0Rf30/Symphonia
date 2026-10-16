// Symphonia Shorten demuxer
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shorten (`.shn`) format reader.
//!
//! A Shorten file is one bit stream of commands that is not byte aligned and has no framing, so
//! the reader finds block boundaries by parsing the stream (cheaply: residuals are skipped, not
//! reconstructed). Every packet is one group of commands that codes one block of every channel
//! (plus any non-audio commands before it), and consists of a tag byte, optionally the decoder
//! state, and the bytes of the group:
//!
//! ```text
//! tag:    bits 0..=2: the bit offset of the group's first command in its first byte
//!         bit 3 (TAG_STATE): the decoder state follows the tag
//! state:  see `Core::write_state` (only on the first packet after a seek)
//! bytes:  the bytes of the stream that hold the group
//! ```
//!
//! Prediction carries history across blocks, so a stream cannot be entered at an arbitrary block.
//! A seek therefore reconstructs the stream up to the target block (resuming from the nearest
//! block it has already reconstructed before) and hands the resulting state to the decoder in
//! the first packet.

use std::io::{Seek, SeekFrom};

use symphonia_core::audio::Channels;
use symphonia_core::audio::layouts;
use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_SHORTEN;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Error, Result, SeekErrorKind, decode_error, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::{MediaInfo, TrackFlags};
use symphonia_core::io::{MediaSource, MediaSourceStream, ReadBytes, ScopedStream, SeekBuffered};
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use crate::bits::{BitReader, is_eof};
use crate::core::{Core, Group};
use crate::header::{
    FileType, Header, SourceFormat, parse_source_header, read_params, read_source_format,
};

/// Packet tag bits 0..=2: the bit offset of the first command within the first byte.
pub(crate) const TAG_START_BIT_MASK: u8 = 0x07;
/// Packet tag bit 3: the decoder state precedes the group's bytes.
pub(crate) const TAG_STATE: u8 = 0x08;

const SHORTEN_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"SHN\0")),
    short_name: "shorten",
    long_name: "Shorten",
};

/// The magic number that opens every stream.
const MAGIC: [u8; 4] = *b"ajkg";

/// How many bytes are read from the source at a time.
const READ_CHUNK: usize = 32 * 1024;
/// The most bytes the header (parameters and the verbatim canonical header) may take.
const MAX_HEADER_BYTES: usize = 256 * 1024;
/// While reconstructing for a seek, a resume point is recorded this many frames after the last.
const ANCHOR_INTERVAL: u64 = 1 << 18;

/// A position the reader can resume decoding from.
struct Anchor {
    /// The timestamp of the group that starts here.
    ts: u64,
    /// The file offset of the byte holding the group's first bit.
    byte: u64,
    /// The bit offset of the group's first bit in that byte.
    bit: usize,
    /// The decoder state at the start of the group (`Core::write_state`).
    state: Vec<u8>,
}

/// Shorten format reader (demuxer).
pub struct ShortenReader<'s> {
    src: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    header: Header,
    /// Tracks block sizes while scanning for group boundaries; its sample state is meaningless.
    scan: Core,
    /// Reconstructs the stream while seeking. Allocated by the first seek.
    recon: Option<Core>,
    /// Bytes read from the source and not yet consumed: `buf[head..]`.
    buf: Vec<u8>,
    head: usize,
    /// The bit offset in `buf[head]` of the next group.
    bit: usize,
    /// The file offset of `buf[0]`.
    buf_pos: u64,
    /// The file offset after the last byte of the stream proper, if known (a seek table may
    /// follow it).
    stream_end: Option<u64>,
    /// The source has no more bytes.
    src_eof: bool,
    /// An `FN_QUIT` command or the end of the data was reached.
    finished: bool,
    /// The timestamp of the next packet.
    next_ts: u64,
    /// The decoder state to put in the next packet.
    pending_state: Option<Vec<u8>>,
    /// The number of frames to trim from the start of the next packet (to reach a seek target).
    trim: u64,
    /// Resume points in ascending timestamp order. The first is the start of the stream.
    anchors: Vec<Anchor>,
}

/// Reads up to `limit` bytes into `buf`, stopping early only at the end of the source.
fn read_into(src: &mut MediaSourceStream<'_>, buf: &mut Vec<u8>, limit: usize) -> Result<bool> {
    let old = buf.len();
    buf.resize(old + limit, 0);
    let mut filled = old;
    while filled < buf.len() {
        match src.read_buf(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            // The stream reports the end of the source as an error once it has run dry.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                buf.truncate(old);
                return Err(Error::IoError(e));
            }
        }
    }
    let eof = filled < buf.len();
    buf.truncate(filled);
    Ok(eof)
}

impl<'s> ShortenReader<'s> {
    pub fn try_new(mut src: MediaSourceStream<'s>, mut opts: FormatOptions) -> Result<Self> {
        let metadata = opts.external_data.metadata.take().unwrap_or_default();

        let buf_pos = src.pos();
        let stream_end = find_stream_end(&mut src, buf_pos)?;

        // Read enough to hold the header, which is not byte aligned and has no length field.
        let mut buf = Vec::new();
        let mut src_eof = false;
        let mut buf_pos = buf_pos;

        // The encoder's `-a` option copies the start of the input file, normally its WAVE or AIFF
        // header, in front of the stream. It is the only place the sample rate of streams
        // without a verbatim header can come from.
        let mut prefix_format = None;
        if matches!(src.read_quad_bytes(), Ok(m) if &m == b"RIFF" || &m == b"FORM") {
            src.seek_buffered_rev(4);
            src_eof = read_into(&mut src, &mut buf, READ_CHUNK)?;
            if let Some(at) = buf.windows(4).skip(12).position(|w| w == MAGIC) {
                let at = at + 12;
                prefix_format = Some(parse_source_header(&buf[..at])?);
                buf.drain(..at);
                buf_pos += at as u64;
            }
        }
        else {
            src.seek_buffered_rev(4);
        }

        let (header, format, header_bits) = loop {
            let want = (READ_CHUNK).max(buf.len());
            let want = match stream_end {
                Some(end) => want.min((end.saturating_sub(buf_pos + buf.len() as u64)) as usize),
                None => want,
            };
            src_eof = read_into(&mut src, &mut buf, want)? || want == 0 || src_eof;

            let mut br = BitReader::new(&buf, 0);
            let parsed = read_params(&mut br).and_then(|h| Ok((h, read_source_format(&mut br)?)));
            match parsed {
                Ok((header, format)) => {
                    let Some(format) = format.or_else(|| prefix_format.clone())
                    else {
                        return decode_error("shorten: the stream does not give its sample rate");
                    };
                    break (header, format, br.bit_pos());
                }
                Err(e) if is_eof(&e) && !src_eof && buf.len() < MAX_HEADER_BYTES => (),
                Err(e) if is_eof(&e) => return decode_error("shorten: truncated header"),
                Err(e) => return Err(e),
            }
        };

        let channels = match header.channels {
            1 => layouts::CHANNEL_LAYOUT_MONO,
            2 => layouts::CHANNEL_LAYOUT_STEREO,
            n => Channels::Discrete(n as u16),
        };
        let (sample_format, bits) = match header.file_type {
            FileType::U8 => (SampleFormat::U8, 8),
            FileType::S8 => (SampleFormat::S8, 8),
            FileType::S16 => (SampleFormat::S16, 16),
            FileType::U16 => (SampleFormat::U16, 16),
        };

        let total_frames = frames_in_header(&header, &format);

        let mut codec_params = AudioCodecParameters::new();
        codec_params
            .for_codec(CODEC_ID_SHORTEN)
            .with_sample_rate(format.sample_rate)
            .with_sample_format(sample_format)
            .with_bits_per_sample(bits)
            .with_bits_per_coded_sample(u32::from(format.bits_per_sample))
            .with_channels(channels)
            .with_max_frames_per_packet(header.blocksize as u64)
            .with_extra_data(header.to_extra_data(format.swap));

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));
        if let Some(frames) = total_frames {
            track.with_num_frames(frames);
            track.with_duration(Duration::new(frames));
        }
        track.with_flags(TrackFlags::DEFAULT);
        let media_info = MediaInfo::from_track(&track);

        let scan = Core::new(&header);
        let mut state = Vec::with_capacity(scan.state_len());
        scan.write_state(&mut state);

        let head = header_bits / 8;
        let bit = header_bits % 8;
        let start = Anchor { ts: 0, byte: buf_pos + head as u64, bit, state };

        Ok(ShortenReader {
            src,
            media_info,
            tracks: vec![track],
            metadata,
            header,
            scan,
            recon: None,
            buf,
            head,
            bit,
            buf_pos,
            stream_end,
            src_eof,
            finished: false,
            next_ts: 0,
            pending_state: None,
            trim: 0,
            anchors: vec![start],
        })
    }

    /// Reads more bytes after the unconsumed ones. Returns `false` if there are none.
    fn fill(&mut self) -> Result<bool> {
        if self.src_eof {
            return Ok(false);
        }
        // Drop the consumed bytes.
        if self.head > 0 {
            self.buf.drain(..self.head);
            self.buf_pos += self.head as u64;
            self.head = 0;
        }
        // Groups can be large, so grow geometrically to keep rescanning one cheap.
        let mut want = READ_CHUNK.max(self.buf.len());
        if let Some(end) = self.stream_end {
            want = want.min(end.saturating_sub(self.buf_pos + self.buf.len() as u64) as usize);
        }
        let before = self.buf.len();
        self.src_eof = read_into(&mut self.src, &mut self.buf, want)? || want == 0;
        Ok(self.buf.len() > before)
    }

    /// Moves to bit `bit` of file offset `byte`, discarding buffered data.
    fn reposition(&mut self, byte: u64, bit: usize) -> Result<()> {
        self.src.seek(SeekFrom::Start(byte)).map_err(Error::IoError)?;
        self.buf.clear();
        self.head = 0;
        self.bit = bit;
        self.buf_pos = byte;
        self.src_eof = false;
        Ok(())
    }

    /// Parses the group at the current position without consuming it. Returns the bit position
    /// (relative to `buf[head]`) after the group and the number of frames in it, or `None` at the
    /// end of the stream.
    fn scan_group(&mut self) -> Result<Option<(usize, usize)>> {
        loop {
            let mut br = BitReader::new(&self.buf[self.head..], self.bit);
            let result = if br.bits_left() < 5 {
                Err(crate::bits::eof())
            }
            else {
                self.scan.decode_group(&mut br, false)
            };
            match result {
                Ok(Group::Audio) => return Ok(Some((br.bit_pos(), self.scan.blocksize()))),
                Ok(Group::Quit) => return Ok(None),
                // A group (or even a command) cut short by the end of the data: read more, or
                // treat what is left as the end of the stream. FFmpeg instead decodes the
                // truncated group from whatever follows the data in memory, which is not
                // reproducible, so the group is dropped.
                Err(e) if is_eof(&e) => {
                    if !self.fill()? {
                        return Ok(None);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Consumes everything up to bit `end_bit` (relative to `buf[head]`).
    fn consume_to(&mut self, end_bit: usize) {
        self.head += end_bit / 8;
        self.bit = end_bit % 8;
    }

    /// The file offset of the byte holding the next group's first bit.
    fn byte_pos(&self) -> u64 {
        self.buf_pos + self.head as u64
    }

    /// Reconstructs the stream from the nearest anchor up to the group that contains `target`,
    /// and positions the reader at that group with the decoder state at its start pending.
    /// Returns the group's timestamp, or `None` if the stream ends before `target`.
    fn seek_to_group(&mut self, target: u64) -> Result<Option<u64>> {
        let at = self.anchors.partition_point(|a| a.ts <= target).saturating_sub(1);
        let (mut ts, byte, bit) = {
            let a = &self.anchors[at];
            (a.ts, a.byte, a.bit)
        };
        self.reposition(byte, bit)?;
        self.scan.read_state(&self.anchors[at].state)?;
        let mut recon = match self.recon.take() {
            Some(core) => core,
            None => Core::new(&self.header),
        };
        recon.read_state(&self.anchors[at].state)?;

        let found = loop {
            let Some((end_bit, frames)) = self.scan_group()?
            else {
                break None;
            };
            let frames = frames as u64;
            if ts + frames > target {
                break Some(ts);
            }

            // Not the target yet: run the state through the group.
            let mut br = BitReader::new(&self.buf[self.head..], self.bit);
            recon.decode_group(&mut br, true)?;
            self.consume_to(end_bit);
            ts += frames;

            // Remember frontier positions so later seeks can start from them.
            let frontier = at + 1 == self.anchors.len() || ts > self.anchors[at].ts;
            if frontier && self.anchors.last().is_some_and(|a| ts >= a.ts + ANCHOR_INTERVAL) {
                let mut state = Vec::with_capacity(recon.state_len());
                recon.write_state(&mut state);
                self.anchors.push(Anchor { ts, byte: self.byte_pos(), bit: self.bit, state });
            }
        };

        if found.is_some() {
            // The scan has advanced its block size past the target group: undo that.
            let mut state = Vec::with_capacity(recon.state_len());
            recon.write_state(&mut state);
            self.scan.read_state(&state)?;
            self.pending_state = Some(state);
        }
        self.recon = Some(recon);
        Ok(found)
    }
}

/// Finds the end of the Shorten stream proper: files made seekable by the `shorten` tool (or
/// ShnAmp) have a seek table appended, ending with a 12 byte trailer of the little-endian table
/// size and the signature `SHNAMPSK`. The table starts with `SEEK`.
///
/// Format: shorten-3.6.1 `include/shorten.h` (`SEEK_TRAILER_SIZE`) and `src/shorten.c`
/// (`FileHasSeekInfoAppended`).
fn find_stream_end(src: &mut MediaSourceStream<'_>, start: u64) -> Result<Option<u64>> {
    const TRAILER_LEN: u64 = 12;
    const SEEK_HEADER_LEN: u64 = 12;

    let Some(len) = src.byte_len().filter(|_| src.is_seekable())
    else {
        return Ok(None);
    };
    if len < start + TRAILER_LEN + SEEK_HEADER_LEN {
        return Ok(None);
    }

    let mut read_at = |pos: u64, out: &mut [u8]| -> Result<()> {
        src.seek(SeekFrom::Start(pos)).map_err(Error::IoError)?;
        src.read_buf_exact(out).map_err(Error::IoError)
    };

    let mut trailer = [0u8; TRAILER_LEN as usize];
    read_at(len - TRAILER_LEN, &mut trailer)?;
    let end = if &trailer[4..] == b"SHNAMPSK" {
        let size = u64::from(u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]));
        let mut magic = [0u8; 4];
        match len.checked_sub(size).filter(|&at| at >= start && size >= TRAILER_LEN) {
            Some(at) if read_at(at, &mut magic).is_ok() && &magic == b"SEEK" => Some(at),
            _ => None,
        }
    }
    else {
        None
    };

    src.seek(SeekFrom::Start(start)).map_err(Error::IoError)?;
    Ok(end)
}

/// The number of frames the header's audio size announces.
fn frames_in_header(header: &Header, format: &SourceFormat) -> Option<u64> {
    let frame_bytes = header.channels as u64 * u64::from(header.file_type.bytes_per_sample());
    format.data_bytes.map(|bytes| bytes / frame_bytes).filter(|&frames| frames > 0)
}

impl Scoreable for ShortenReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut window = [0u8; 40];
        let mut filled = 0;
        while filled < window.len() {
            match src.read_buf(&mut window[filled..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => filled += n,
            }
        }
        if filled < MAGIC.len() || window[..4] != MAGIC {
            return Ok(Score::Unsupported);
        }
        // The parameters must parse (unless the window ends inside them).
        let mut br = BitReader::new(&window[..filled], 0);
        Ok(match read_params(&mut br) {
            Ok(_) => Score::Supported(255),
            Err(e) if is_eof(&e) => Score::Supported(128),
            Err(_) => Score::Unsupported,
        })
    }
}

impl ProbeableFormat<'_> for ShortenReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(ShortenReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            SHORTEN_FORMAT_INFO,
            &["shn"],
            &["audio/x-shorten", "application/x-shorten"],
            &[b"ajkg"]
        )]
    }
}

impl FormatReader for ShortenReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &SHORTEN_FORMAT_INFO
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
        if self.finished {
            return Ok(None);
        }

        let (end_bit, frames) = match self.scan_group() {
            Ok(Some(group)) => group,
            Ok(None) => {
                self.finished = true;
                return Ok(None);
            }
            Err(e) => {
                // The position inside the stream is lost.
                self.finished = true;
                return Err(e);
            }
        };

        let start_bit = self.bit;
        let mut tag = start_bit as u8;
        let mut data = Vec::with_capacity(1 + end_bit / 8 + 1);
        data.push(0);
        if let Some(state) = self.pending_state.take() {
            tag |= TAG_STATE;
            data.extend_from_slice(&state);
        }
        data[0] = tag;
        data.extend_from_slice(&self.buf[self.head..self.head + end_bit.div_ceil(8)]);
        self.consume_to(end_bit);

        let frames = frames as u64;
        let trim = self.trim.min(frames);
        self.trim = 0;

        let mut packet =
            Packet::new(0, Timestamp::new(self.next_ts as i64), Duration::new(frames - trim), data);
        packet.trim_start = Duration::new(trim);
        self.next_ts += frames;
        Ok(Some(packet))
    }

    fn seek(&mut self, mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if !self.src.is_seekable() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        let required_ts = match to {
            SeekTo::Timestamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                let tb =
                    self.tracks[0].time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };
        if required_ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }
        let target = required_ts.get() as u64;

        self.finished = false;
        self.pending_state = None;
        self.trim = 0;

        match self.seek_to_group(target) {
            Ok(Some(ts)) => {
                self.next_ts = ts;
                let actual = match mode {
                    SeekMode::Accurate => {
                        self.trim = target - ts;
                        target
                    }
                    SeekMode::Coarse => ts,
                };
                Ok(SeekedTo { track_id: 0, actual_ts: Timestamp::new(actual as i64), required_ts })
            }
            result => {
                // Out of range (or an error): go back to the start of the stream.
                let start = &self.anchors[0];
                let (byte, bit) = (start.byte, start.bit);
                self.reposition(byte, bit)?;
                self.scan.read_state(&self.anchors[0].state)?;
                self.pending_state = Some(self.anchors[0].state.clone());
                self.next_ts = 0;
                self.finished = false;
                result?;
                seek_error(SeekErrorKind::OutOfRange)
            }
        }
    }

    fn into_inner<'s2>(self: Box<Self>) -> MediaSourceStream<'s2>
    where
        Self: 's2,
    {
        self.src
    }
}
