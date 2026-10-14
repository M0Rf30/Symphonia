// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{ErrorKind, Seek, SeekFrom};

use symphonia_common::mpeg::es::EsParser;
use symphonia_common::mpeg::surround::{Ac3Es, DcaEs, TrueHdEs};
use symphonia_core::codecs::CodecParameters;
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

/// The number of bytes read from the stream at a time.
const CHUNK_LEN: usize = 16 * 1024;

/// The number of bytes examined to probe the stream and determine its parameters.
const PROBE_LEN: usize = 64 * 1024;

/// The minimum number of consecutive frames at the start of the data for the data to be
/// confidently identified.
const CONFIDENT_FRAMES: u32 = 4;

/// The maximum number of frames that the duration is estimated from.
const ESTIMATE_FRAMES: u32 = 64;

/// The code of a stream: the differences of the readers.
#[derive(Copy, Clone)]
struct Kind {
    info: FormatInfo,
    /// A new frame parser.
    parser: fn() -> Box<dyn EsParser>,
    /// The number of bytes before the sync word that start a frame.
    lead: usize,
    /// True if the duration can be estimated from the average size of the frames.
    cbr: bool,
    /// Returns true if decoding can start at the frame.
    is_sync_point: fn(&[u8]) -> bool,
}

const AC3: Kind = Kind {
    info: FormatInfo {
        format: FormatId::new(FourCc::new(*b"AC3 ")),
        short_name: "ac3",
        long_name: "Dolby Digital (AC-3, E-AC-3) elementary stream",
    },
    parser: || Box::new(Ac3Es::new()),
    lead: 0,
    cbr: true,
    is_sync_point: |_| true,
};

const DTS: Kind = Kind {
    info: FormatInfo {
        format: FormatId::new(FourCc::new(*b"DTS ")),
        short_name: "dts",
        long_name: "DTS Coherent Acoustics elementary stream",
    },
    parser: || Box::new(DcaEs::new()),
    lead: 0,
    cbr: true,
    is_sync_point: |_| true,
};

const TRUEHD: Kind = Kind {
    info: FormatInfo {
        format: FormatId::new(FourCc::new(*b"THD ")),
        short_name: "truehd",
        long_name: "Dolby TrueHD / MLP elementary stream",
    },
    parser: || Box::new(TrueHdEs::new()),
    // The access unit header precedes the major sync word.
    lead: 4,
    cbr: false,
    // Only access units with a major sync can start decoding.
    is_sync_point: |au| {
        au.len() >= 8
            && u32::from_be_bytes([au[4], au[5], au[6], au[7]]) & 0xffff_fffe == 0xf872_6fba
    },
};

/// Reads from `src` until `buf` is full or the end of the stream. Returns the length read, and
/// if the end of the stream was reached.
fn read_up_to<B: ReadBytes>(src: &mut B, buf: &mut [u8]) -> Result<(usize, bool)> {
    let mut len = 0;

    while len < buf.len() {
        match src.read_buf(&mut buf[len..]) {
            Ok(0) => return Ok((len, true)),
            Ok(n) => len += n,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok((len, true)),
            Err(e) => return Err(e.into()),
        }
    }

    Ok((len, false))
}

/// Score the data at the current position of `src` as a stream of `kind`.
fn score_stream(kind: &Kind, mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
    if kind.lead > 0 {
        // The scope starts at the sync word, so rewind the inner stream directly.
        src.inner_mut().seek_buffered_rel(-(kind.lead as isize));
    }

    let mut buf = vec![0; PROBE_LEN];
    let (len, eof) = read_up_to(&mut src, &mut buf)?;

    let mut parser = (kind.parser)();
    parser.push(&buf[..len]);

    // Count the frames that are consecutive from the start of the data.
    let mut n_frames = 0;
    let mut next = 0u64;

    while let Some(frame) = parser.next_frame(eof) {
        if frame.start != next {
            break;
        }

        next += frame.data.len() as u64;
        n_frames += 1;
    }

    Ok(if n_frames >= CONFIDENT_FRAMES {
        Score::Supported(255)
    }
    else if eof && n_frames > 0 && next == len as u64 {
        // The whole of a short stream is frames.
        Score::Supported(if n_frames > 1 { 200 } else { 100 })
    }
    else {
        Score::Unsupported
    })
}

/// A reader of a raw elementary stream.
struct EsReader<'s> {
    reader: MediaSourceStream<'s>,
    kind: Kind,
    parser: Box<dyn EsParser>,
    tracks: Vec<Track>,
    media_info: MediaInfo,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    rate: u32,
    /// The byte position of the first frame.
    start_pos: u64,
    /// The timestamp of the next frame.
    next_ts: i64,
    /// The number of bytes pushed to the parser.
    pushed: u64,
    /// The stream position minus the offset the parser has for the same byte.
    origin: i64,
    eof: bool,
    /// The timestamp and position of frames that decoding can start at, increasing, one for
    /// about every quarter second.
    index: Vec<(i64, u64)>,
    /// The packet found by the last seek, which is returned next.
    pending: Option<Packet>,
    last_ts: Option<i64>,
}

impl<'s> EsReader<'s> {
    fn try_new(kind: Kind, mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // Rewind to the start of the first frame.
        if kind.lead > 0 {
            mss.seek_buffered_rev(kind.lead);
        }

        let start_pos = mss.pos();

        if !mss.is_seekable() {
            mss.ensure_seekback_buffer(PROBE_LEN);
        }

        // Examine the start of the stream.
        let mut buf = vec![0; PROBE_LEN];
        let (len, eof) = read_up_to(&mut mss, &mut buf)?;

        let mut probe = (kind.parser)();
        probe.push(&buf[..len]);

        let mut n_frames = 0u32;
        let mut bytes = 0u64;
        let mut first_frame = None;

        while let Some(frame) = probe.next_frame(eof) {
            if first_frame.is_none() {
                first_frame = Some(frame.start);
            }

            n_frames += 1;
            bytes += frame.data.len() as u64;

            if n_frames >= ESTIMATE_FRAMES {
                break;
            }
        }

        let (Some(info), Some(_)) = (probe.info().cloned(), first_frame)
        else {
            return unsupported_error("es: no frames found");
        };

        // Return to the first frame.
        if mss.is_seekable() {
            mss.seek(SeekFrom::Start(start_pos))?;
        }
        else {
            mss.seek_buffered(start_pos);
        }

        let rate = info.rate;

        let mut track = Track::new(0);

        track
            .with_time_base(
                TimeBase::try_from_recip(rate).ok_or(Error::DecodeError("es: invalid rate"))?,
            )
            .with_codec_params(CodecParameters::Audio(info.params.clone()))
            .with_start_ts(Timestamp::new(0));

        if let (true, Some(total), true) = (kind.cbr, mss.byte_len(), n_frames > 0) {
            // Estimate the number of frames from the average frame size.
            let frame_dur = probe.frame_grid();
            let frames = total.saturating_sub(start_pos) * u64::from(n_frames) / bytes.max(1);

            info!("{}: estimating duration from the frame size", kind.info.short_name);

            let num_frames = frames * frame_dur;
            track.with_num_frames(num_frames);
            track.with_duration(Duration::new(num_frames));
        }

        Ok(EsReader {
            reader: mss,
            kind,
            parser: (kind.parser)(),
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            metadata: opts.external_data.metadata.unwrap_or_default(),
            chapters: opts.external_data.chapters,
            rate,
            start_pos,
            next_ts: 0,
            pushed: 0,
            origin: start_pos as i64,
            eof: false,
            index: Vec::new(),
            pending: None,
            last_ts: None,
        })
    }

    fn next_packet_inner(&mut self) -> Result<Option<Packet>> {
        if let Some(packet) = self.pending.take() {
            return Ok(Some(packet));
        }

        loop {
            if let Some(frame) = self.parser.next_frame(self.eof) {
                let ts = self.next_ts;
                self.next_ts += frame.dur as i64;

                // Index frames that decoding can start at, if there is not one near.
                if (self.kind.is_sync_point)(&frame.data) {
                    let pos = (frame.start as i64 + self.origin).max(0) as u64;
                    let period = i64::from(self.rate / 4);

                    if self.index.last().is_none_or(|&(last_ts, _)| ts >= last_ts + period) {
                        self.index.push((ts, pos));
                    }
                }

                let mut packet =
                    Packet::new(0, Timestamp::new(ts), Duration::new(frame.dur), frame.data);
                packet.dts = Timestamp::new(ts);

                self.last_ts = Some(ts);

                return Ok(Some(packet));
            }

            if self.eof {
                return Ok(None);
            }

            let mut buf = [0; CHUNK_LEN];
            let (len, eof) = read_up_to(&mut self.reader, &mut buf)?;

            self.parser.push(&buf[..len]);
            self.pushed += len as u64;
            self.eof = eof;
        }
    }
}

macro_rules! es_reader {
    ($(#[$meta:meta])* $name:ident, $kind:expr, $exts:expr, $mimes:expr, $markers:expr) => {
        $(#[$meta])*
        pub struct $name<'s>(EsReader<'s>);

        impl<'s> $name<'s> {
            pub fn try_new(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
                Ok($name(EsReader::try_new($kind, mss, opts)?))
            }
        }

        impl Scoreable for $name<'_> {
            fn score(src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
                score_stream(&$kind, src)
            }
        }

        impl<'s> ProbeableFormat<'s> for $name<'_> {
            fn try_probe_new(
                mss: MediaSourceStream<'s>,
                opts: FormatOptions,
            ) -> Result<Box<dyn FormatReader + 's>> {
                Ok(Box::new($name::try_new(mss, opts)?))
            }

            fn probe_data() -> &'static [ProbeFormatData] {
                &[support_format!($kind.info, $exts, $mimes, $markers)]
            }
        }

        impl FormatReader for $name<'_> {
            fn format_info(&self) -> &FormatInfo {
                &$kind.info
            }

            fn media_info(&self) -> &MediaInfo {
                &self.0.media_info
            }

            fn next_packet(&mut self) -> Result<Option<Packet>> {
                self.0.next_packet_inner()
            }

            fn metadata(&mut self) -> Metadata<'_> {
                self.0.metadata.metadata()
            }

            fn chapters(&self) -> Option<&ChapterGroup> {
                self.0.chapters.as_ref()
            }

            fn tracks(&self) -> &[Track] {
                &self.0.tracks
            }

            fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
                self.0.seek(to)
            }

            fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
            where
                Self: 'a,
            {
                self.0.reader
            }
        }
    };
}

impl EsReader<'_> {
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

        if let Some(dur) = self.tracks[0].duration {
            if required_ts.get() > dur.get() as i64 {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        let start_ts = self.parser.seek_start_ts(required_ts.get()).max(0);

        debug!(
            "{}: seeking to ts={required_ts} (start from {start_ts})",
            self.kind.info.short_name
        );

        if self.reader.is_seekable() {
            // The last frame to start decoding from before the timestamp, or the first frame.
            let idx = self.index.partition_point(|&(ts, _)| ts <= start_ts);
            let (ts, pos) = idx.checked_sub(1).map_or((0, self.start_pos), |i| self.index[i]);

            // If the stream is ahead of the frame, and before the timestamp, keep going.
            let ahead = self.last_ts.is_some_and(|last| last >= ts && last <= start_ts);

            if !ahead || self.pending.is_some() {
                self.reader.seek(SeekFrom::Start(pos))?;
                self.parser.clear();
                self.origin = pos as i64 - self.pushed as i64;
                self.next_ts = ts;
                self.eof = false;
            }
        }
        else if self.last_ts.is_some_and(|last| required_ts.get() < last) {
            return seek_error(SeekErrorKind::ForwardOnly);
        }

        self.pending = None;

        // Scan forward to the first frame that ends after the timestamp to start from.
        let packet = loop {
            match self.next_packet_inner()? {
                Some(p) if p.pts.get().saturating_add(p.dur.get() as i64) > start_ts => break p,
                Some(_) => (),
                None => return seek_error(SeekErrorKind::OutOfRange),
            }
        };

        let actual_ts = packet.pts;
        self.pending = Some(packet);

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts })
    }
}

es_reader!(
    /// Dolby Digital (AC-3) and Dolby Digital Plus (E-AC-3) elementary stream reader.
    Ac3Reader,
    AC3,
    &["ac3", "eac3", "ec3"],
    &["audio/ac3", "audio/eac3", "audio/vnd.dolby.dd-raw"],
    &[&[0x0b, 0x77], &[0x77, 0x0b]]
);

es_reader!(
    /// DTS Coherent Acoustics elementary stream reader. Streams in the 16-bit and 14-bit forms,
    /// big and little endian, are supported, as are DTS-HD streams with a core.
    DtsReader,
    DTS,
    &["dts", "dtshd", "dtsma"],
    &["audio/vnd.dts", "audio/vnd.dts.hd"],
    &[
        &[0x7f, 0xfe, 0x80, 0x01],
        &[0xfe, 0x7f, 0x01, 0x80],
        &[0x1f, 0xff, 0xe8, 0x00],
        &[0xff, 0x1f, 0x00, 0xe8]
    ]
);

es_reader!(
    /// Dolby TrueHD and Meridian Lossless Packing (MLP) elementary stream reader.
    TrueHdReader,
    TRUEHD,
    &["thd", "truehd", "mlp"],
    &["audio/vnd.dolby.mlp"],
    &[&[0xf8, 0x72, 0x6f, 0xba], &[0xf8, 0x72, 0x6f, 0xbb]]
);
