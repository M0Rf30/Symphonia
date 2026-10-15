// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{ErrorKind, Seek, SeekFrom};

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_AMR_NB, CODEC_ID_AMR_WB};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error, unsupported_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::packet::Packet;
use symphonia_core::support_format;
use symphonia_core::units::{Duration, TimeBase, Timestamp};

use log::{debug, info};

const AMR_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"AMR ")),
    short_name: "amr",
    long_name: "AMR-NB / AMR-WB storage format",
};

/// The magic numbers, RFC 4867 sections 5.1 and 5.2.
const MAGIC_NB: &[u8] = b"#!AMR\n";
const MAGIC_WB: &[u8] = b"#!AMR-WB\n";
const MAGIC_NB_MC: &[u8] = b"#!AMR_MC1.0\n";
const MAGIC_WB_MC: &[u8] = b"#!AMR-WB_MC1.0\n";

/// The length of the longest magic number plus the channel description.
const MAX_HEADER_LEN: usize = 15 + 4;

/// The number of bytes of the frame, TOC byte included, by frame type. 0 for the frame types that
/// are not valid in a file. Frame sizes are from 3GPP TS 26.101 (AMR-NB) and 3GPP TS 26.201
/// (AMR-WB), as tabulated in FFmpeg's `libavformat/amr.c` (`amrnb_packed_size` and
/// `amrwb_packed_size`), with the frame types that RFC 4867 section 5.3 does not allow in files
/// (AMR-NB 9 to 14: comfort noise types other than SID and the unused types; AMR-WB 10 to 13:
/// unused types) removed. FT 14 (speech lost, AMR-WB only) and FT 15 (no data) are one byte.
const NB_FRAME_LEN: [u8; 16] = [13, 14, 16, 18, 20, 21, 27, 32, 6, 0, 0, 0, 0, 0, 0, 1];
const WB_FRAME_LEN: [u8; 16] = [18, 24, 33, 37, 41, 47, 51, 59, 61, 6, 0, 0, 0, 0, 1, 1];

/// The number of samples of a frame: 20 ms.
const NB_FRAME_SAMPLES: u64 = 160;
const WB_FRAME_SAMPLES: u64 = 320;

/// A frame index entry is added about every second.
const INDEX_PERIOD_FRAMES: u64 = 50;

/// The number of frame blocks examined when scoring.
const SCORE_BLOCKS: usize = 8;

#[derive(Copy, Clone, PartialEq, Eq)]
enum Variant {
    Nb,
    Wb,
}

impl Variant {
    fn frame_len_table(self) -> &'static [u8; 16] {
        match self {
            Variant::Nb => &NB_FRAME_LEN,
            Variant::Wb => &WB_FRAME_LEN,
        }
    }

    fn rate(self) -> u32 {
        match self {
            Variant::Nb => 8000,
            Variant::Wb => 16000,
        }
    }

    fn frame_samples(self) -> u64 {
        match self {
            Variant::Nb => NB_FRAME_SAMPLES,
            Variant::Wb => WB_FRAME_SAMPLES,
        }
    }

    /// The length of a frame from its TOC byte, or None if the frame type is not valid.
    fn frame_len(self, toc: u8) -> Option<usize> {
        match self.frame_len_table()[usize::from(toc >> 3 & 0xf)] {
            0 => None,
            len => Some(usize::from(len)),
        }
    }
}

struct Header {
    variant: Variant,
    channels: u32,
    /// The length of the header in bytes.
    len: usize,
}

/// Parse the header at the start of `buf`.
fn parse_header(buf: &[u8]) -> Option<Header> {
    // The single channel magic numbers are checked first: they are not prefixes of the others
    // because of the "\n".
    for (magic, variant, mc) in [
        (MAGIC_NB, Variant::Nb, false),
        (MAGIC_WB, Variant::Wb, false),
        (MAGIC_NB_MC, Variant::Nb, true),
        (MAGIC_WB_MC, Variant::Wb, true),
    ] {
        let Some(rest) = buf.strip_prefix(magic)
        else {
            continue;
        };

        if !mc {
            return Some(Header { variant, channels: 1, len: magic.len() });
        }

        // RFC 4867 section 5.2: 28 reserved bits (ignored) and 4 bits of channel count, in
        // network order.
        let desc = rest.get(..4)?;
        let channels = u32::from(desc[3] & 0xf);

        return (channels > 0).then_some(Header { variant, channels, len: magic.len() + 4 });
    }

    None
}

/// Read up to `buf.len()` bytes, returning the length read.
fn read_up_to<B: ReadBytes>(src: &mut B, buf: &mut [u8]) -> Result<usize> {
    let mut len = 0;

    while len < buf.len() {
        match src.read_buf(&mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
    }

    Ok(len)
}

/// Score the data at the current position of `src` as an AMR file.
fn score_stream(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
    let mut buf = [0; 1024];
    let len = read_up_to(&mut src, &mut buf)?;
    let buf = &buf[..len];

    let Some(header) = parse_header(buf)
    else {
        return Ok(Score::Unsupported);
    };

    // The frames that follow, if any, must have valid frame types.
    let mut pos = header.len;

    'blocks: for _ in 0..SCORE_BLOCKS {
        for _ in 0..header.channels {
            let Some(&toc) = buf.get(pos)
            else {
                break 'blocks;
            };

            match header.variant.frame_len(toc) {
                Some(len) => pos += len,
                None => return Ok(Score::Unsupported),
            }
        }
    }

    Ok(Score::Supported(255))
}

/// A reader of the AMR storage format.
pub struct AmrReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    variant: Variant,
    channels: u32,
    /// The position of the first frame block.
    start_pos: u64,
    /// The timestamp of the next frame block.
    next_ts: i64,
    /// The positions of frame blocks, at increasing timestamps, about every second.
    index: Vec<(i64, u64)>,
    /// The packet found by the last seek, which is returned next.
    pending: Option<Packet>,
}

impl<'s> AmrReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let mut buf = [0; MAX_HEADER_LEN];
        let len = read_up_to(&mut mss, &mut buf)?;

        let Some(header) = parse_header(&buf[..len])
        else {
            return unsupported_error("amr: not an AMR file");
        };

        // Return to the first frame block.
        mss.seek_buffered_rev(len - header.len);
        let start_pos = mss.pos();

        let variant = header.variant;
        let rate = variant.rate();

        let channels = match Position::from_count(header.channels) {
            Some(pos) => Channels::Positioned(pos),
            None => Channels::Discrete(header.channels as u16),
        };

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(match variant {
                Variant::Nb => CODEC_ID_AMR_NB,
                Variant::Wb => CODEC_ID_AMR_WB,
            })
            .with_sample_rate(rate)
            .with_channels(channels)
            .with_max_frames_per_packet(variant.frame_samples());

        let mut track = Track::new(0);
        track
            .with_time_base(
                TimeBase::try_from_recip(rate).ok_or(Error::DecodeError("amr: invalid rate"))?,
            )
            .with_codec_params(CodecParameters::Audio(params))
            .with_start_ts(Timestamp::new(0));

        let mut reader = AmrReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            metadata: opts.external_data.metadata.unwrap_or_default(),
            chapters: opts.external_data.chapters,
            variant,
            channels: header.channels,
            start_pos,
            next_ts: 0,
            index: Vec::new(),
            pending: None,
        };

        if opts.prebuild_seek_index && reader.reader.is_seekable() {
            info!("amr: scanning file for duration");

            while reader.read_block(None)?.is_some() {}
            let blocks = reader.next_ts;

            reader.reader.seek(SeekFrom::Start(start_pos))?;
            reader.next_ts = 0;

            let track = &mut reader.tracks[0];
            track.with_num_frames(blocks as u64);
            track.with_duration(Duration::new(blocks as u64));
            reader.media_info = MediaInfo::from_track(&reader.tracks[0]);
        }

        Ok(reader)
    }

    /// Read the next valid frame block, skipping garbage, and advance the timestamp. The frames
    /// are appended to `out`, if given, and skipped otherwise. Returns the timestamp of the block,
    /// or None at the end of the stream, where a block that is cut short is dropped.
    fn read_block(&mut self, mut out: Option<&mut Vec<u8>>) -> Result<Option<i64>> {
        let ts = self.next_ts;
        let block_pos = self.reader.pos();

        // Index the block if it is the first of a period and not already indexed.
        let period = (INDEX_PERIOD_FRAMES * self.variant.frame_samples()) as i64;
        let want_index = self.index.last().is_none_or(|&(last_ts, _)| ts >= last_ts + period);

        let mut skipped = 0u64;

        'resync: loop {
            let start = self.reader.pos();

            if let Some(out) = out.as_deref_mut() {
                out.clear();
            }

            for _ in 0..self.channels {
                let toc = match self.reader.read_byte() {
                    Ok(toc) => toc,
                    Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
                    Err(e) => return Err(e.into()),
                };

                let Some(len) = self.variant.frame_len(toc)
                else {
                    // Garbage: resume one byte after the start of the failed block.
                    skipped += 1;
                    self.reader.seek_buffered(start + 1);
                    continue 'resync;
                };

                let res = match out.as_deref_mut() {
                    Some(out) => {
                        let at = out.len();
                        out.resize(at + len, 0);
                        out[at] = toc;
                        self.reader.read_buf_exact(&mut out[at + 1..])
                    }
                    None => self.reader.ignore_bytes(len as u64 - 1),
                };

                match res {
                    Ok(()) => (),
                    Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                        debug!("amr: dropping truncated frame block");
                        return Ok(None);
                    }
                    Err(e) => return Err(e.into()),
                }
            }

            if want_index {
                self.index.push((ts, start));
            }

            break;
        }

        if skipped > 0 {
            debug!("amr: skipped {skipped} bytes of garbage at {block_pos}");
        }

        self.next_ts += self.variant.frame_samples() as i64;

        Ok(Some(ts))
    }
}

impl AmrReader<'_> {
    fn next_packet_inner(&mut self) -> Result<Option<Packet>> {
        if let Some(packet) = self.pending.take() {
            return Ok(Some(packet));
        }

        let mut data = Vec::new();

        match self.read_block(Some(&mut data))? {
            Some(ts) => {
                let dur = Duration::new(self.variant.frame_samples());
                Ok(Some(Packet::new(0, Timestamp::new(ts), dur, data)))
            }
            None => Ok(None),
        }
    }

    fn seek(&mut self, to: SeekTo) -> Result<SeekedTo> {
        let required_ts = match to {
            SeekTo::Timestamp { ts, track_id: 0 } => ts,
            SeekTo::Time { time, track_id: None | Some(0) } => {
                let tb =
                    self.tracks[0].time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
            _ => return seek_error(SeekErrorKind::Unseekable),
        };

        if required_ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let required = required_ts.get();

        if let Some(dur) = self.tracks[0].duration {
            if required >= dur.get() as i64 {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        debug!("amr: seeking to ts={required_ts}");

        let mut pending = self.pending.take();

        // The earliest timestamp that can be reached without moving the stream backwards.
        let cur_ts = pending.as_ref().map_or(self.next_ts, |p| p.pts.get());

        if self.reader.is_seekable() {
            // The last indexed block at or before the timestamp, or the first block.
            let idx = self.index.partition_point(|&(ts, _)| ts <= required);
            let (ts, pos) = idx.checked_sub(1).map_or((0, self.start_pos), |i| self.index[i]);

            // Jump if the timestamp is behind the stream, or the index is ahead of it.
            if cur_ts > required || ts > self.next_ts {
                self.reader.seek(SeekFrom::Start(pos))?;
                self.next_ts = ts;
                pending = None;
            }
        }
        else if cur_ts > required {
            self.pending = pending;
            return seek_error(SeekErrorKind::ForwardOnly);
        }

        let step = self.variant.frame_samples() as i64;

        // Scan forward, skipping payloads, to the block that contains the timestamp.
        let actual_ts = loop {
            if let Some(p) = pending.take() {
                if p.pts.get() + step > required {
                    let ts = p.pts.get();
                    self.pending = Some(p);
                    break ts;
                }
                continue;
            }

            // Stop before reading the block that contains the timestamp, so it is read in full.
            if self.next_ts + step > required {
                match self.next_packet_inner()? {
                    Some(p) => {
                        let ts = p.pts.get();
                        self.pending = Some(p);
                        break ts;
                    }
                    None => return seek_error(SeekErrorKind::OutOfRange),
                }
            }

            if self.read_block(None)?.is_none() {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        };

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts: Timestamp::new(actual_ts) })
    }
}

impl Scoreable for AmrReader<'_> {
    fn score(src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        score_stream(src)
    }
}

impl<'s> ProbeableFormat<'s> for AmrReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'s>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + 's>> {
        Ok(Box::new(AmrReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            AMR_FORMAT_INFO,
            &["amr", "awb"],
            &["audio/amr", "audio/amr-wb", "audio/AMR", "audio/AMR-WB"],
            &[MAGIC_NB, MAGIC_WB, MAGIC_NB_MC, MAGIC_WB_MC]
        )]
    }
}

impl FormatReader for AmrReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &AMR_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        self.next_packet_inner()
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
        self.seek(to)
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
