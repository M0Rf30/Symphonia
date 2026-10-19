// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

//! Demuxer for IEC 61937 data bursts (compressed audio in S/PDIF, `.spdif` files).
//!
//! A burst is the 16-bit words `Pa` = `0xF872` and `Pb` = `0x4E1F` (the sync words), `Pc` (the
//! data type in the low byte), `Pd` (the length of the payload), and the payload words, followed by
//! padding up to the burst repetition period of the data type. [`SpdifReader`] emits the payload of
//! each burst as a packet.
//!
//! The framing follows FFmpeg's `libavformat/spdifdec.c` (LGPL-2.1-or-later, Copyright (c) 2009
//! Bartlomiej Wolowiec, 2010 Anssi Hannula): the burst is found by a byte-wise search for the
//! sync words, `Pd` is a length in bits (in bytes for E-AC-3) that is rounded up to 16 bits, the
//! 16-bit words of the payload are byte-swapped to the bitstream order of the codec, and the
//! padding is skipped using the repetition period of the data type.
//!
//! Supported data types (those of FFmpeg's demuxer): AC-3 (1), MPEG-1 layer 1 (4), MPEG-1/2
//! layer 2 and 3 (5), MPEG-2 with extension (6), MPEG-2 AAC in ADTS (7), MPEG-2 low sampling
//! frequency layer 1, 2 and 3 (8, 9, 10), DTS type I, II and III (11, 12, 13), and E-AC-3 (21).
//! Other data types (ATRAC, DTS-HD, WMA Pro, TrueHD, ...) are not carried by FFmpeg's demuxer, and
//! are skipped.
//!
//! Streams with the sync words in either byte order are accepted: the words of files written by
//! FFmpeg (and by other tools on little-endian hosts) are little-endian, and the payload is then
//! byte-swapped. For big-endian streams the payload is already in bitstream order. The 20-bit and
//! 24-bit (S/PDIF subframe) forms are not supported.
//!
//! The stream has no timestamps: the timeline is the running sample count. Seeking scans the
//! whole stream once, and so requires a seekable source.

use std::io::{self, Seek, SeekFrom};

use symphonia_common::mpeg::es::{AdtsEs, EsInfo, EsParser, MpaEs};
use symphonia_common::mpeg::surround::{Ac3Es, Ac3Header, DcaEs};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, reset_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use log::{debug, info};

/// The number of bytes examined to probe the stream.
const PROBE_LEN: usize = 64 * 1024;

/// `Pa` and `Pb` as the bytes of the little-endian words (FFmpeg `SYNCWORD1`, `SYNCWORD2`).
const SYNC_LE: [u8; 4] = [0x72, 0xf8, 0x1f, 0x4e];

/// `Pa` and `Pb` as the bytes of the big-endian words.
const SYNC_BE: [u8; 4] = [0xf8, 0x72, 0x4e, 0x1f];

/// The size of the burst preamble: `Pa`, `Pb`, `Pc`, and `Pd` (FFmpeg `BURST_HEADER_SIZE`).
const BURST_HEADER_LEN: usize = 8;

/// The size of the ADTS header that carries the number of raw data blocks.
const ADTS_HEADER_LEN: usize = 7;

const SPDIF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"SPDF")),
    short_name: "spdif",
    long_name: "IEC 61937 (S/PDIF) data bursts",
};

/// The codec family of the payload of a burst, which selects the parser that determines the
/// parameters of the stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Payload {
    Ac3,
    Eac3,
    Mpeg,
    Aac,
    Dts,
}

impl Payload {
    /// A parser of the payload.
    fn parser(self) -> Box<dyn EsParser> {
        match self {
            Payload::Ac3 | Payload::Eac3 => Box::new(Ac3Es::new()),
            Payload::Mpeg => Box::new(MpaEs::new()),
            Payload::Aac => Box::new(AdtsEs::new()),
            Payload::Dts => Box::new(DcaEs::new()),
        }
    }
}

/// The IEC 61937 data type of a burst: the low byte of `Pc`.
///
/// Values: IEC 61937-1 Table 5 and FFmpeg `libavformat/spdif.h` (`enum IEC61937DataType`).
mod data_type {
    pub const AC3: u8 = 0x01;
    pub const MPEG1_LAYER1: u8 = 0x04;
    pub const MPEG1_LAYER23: u8 = 0x05;
    pub const MPEG2_EXT: u8 = 0x06;
    pub const MPEG2_AAC: u8 = 0x07;
    pub const MPEG2_LAYER1_LSF: u8 = 0x08;
    pub const MPEG2_LAYER2_LSF: u8 = 0x09;
    pub const MPEG2_LAYER3_LSF: u8 = 0x0a;
    pub const DTS1: u8 = 0x0b;
    pub const DTS2: u8 = 0x0c;
    pub const DTS3: u8 = 0x0d;
    pub const EAC3: u8 = 0x15;
}

/// The burst repetition period in bytes of MPEG audio, by `[is_mpeg1][layer - 1]` (FFmpeg
/// `spdif_mpeg_pkt_offset`).
const MPEG_PERIOD: [[usize; 3]; 2] = [[3072, 9216, 4608], [1536, 4608, 4608]];

/// The payload family and the burst repetition period in bytes of the burst of `data_type`, whose
/// payload (in bitstream order) is `payload` (FFmpeg `spdif_get_offset_and_codec`). The period is
/// four bytes per sample of the burst. Returns `None` if the data type is not supported, or the
/// payload is not valid for it.
fn burst_layout(data_type: u8, payload: &[u8]) -> Option<(Payload, usize)> {
    Some(match data_type {
        data_type::AC3 => (Payload::Ac3, 1536 << 2),
        data_type::MPEG1_LAYER1 => (Payload::Mpeg, MPEG_PERIOD[1][0]),
        // IEC 61937-4: 1152 samples of 4 bytes. FFmpeg's spdifdec uses `[1][0]` (1536) here,
        // which is the period of layer 1 and under-skips the padding.
        data_type::MPEG1_LAYER23 => (Payload::Mpeg, MPEG_PERIOD[1][1]),
        data_type::MPEG2_EXT => (Payload::Mpeg, 4608),
        data_type::MPEG2_AAC => {
            // The ADTS header gives the number of raw data blocks, of 1024 samples each
            // (FFmpeg `av_adts_header_parse`).
            let hdr = payload.get(..ADTS_HEADER_LEN)?;

            if hdr[0] != 0xff || hdr[1] & 0xf0 != 0xf0 {
                return None;
            }

            let samples = (usize::from(hdr[6] & 3) + 1) * 1024;
            (Payload::Aac, samples << 2)
        }
        data_type::MPEG2_LAYER1_LSF => (Payload::Mpeg, MPEG_PERIOD[0][0]),
        data_type::MPEG2_LAYER2_LSF => (Payload::Mpeg, MPEG_PERIOD[0][1]),
        data_type::MPEG2_LAYER3_LSF => (Payload::Mpeg, MPEG_PERIOD[0][2]),
        data_type::DTS1 => (Payload::Dts, 2048),
        data_type::DTS2 => (Payload::Dts, 4096),
        data_type::DTS3 => (Payload::Dts, 8192),
        data_type::EAC3 => (Payload::Eac3, 24576),
        _ => return None,
    })
}

/// The length in bytes of the payload for `Pd`: a length in bits rounded up to 16 bits, or in bytes
/// for E-AC-3 (FFmpeg `spdif_get_pkt_size_bits`).
fn payload_len(data_type: u8, pd: u16) -> usize {
    let bits = if data_type == data_type::EAC3 { usize::from(pd) << 3 } else { usize::from(pd) };
    bits.div_ceil(16) * 2
}

/// Swap the bytes of each 16-bit word, in place (FFmpeg `ff_spdif_bswap_buf16`).
fn swap_words(buf: &mut [u8]) {
    for w in buf.chunks_exact_mut(2) {
        w.swap(0, 1);
    }
}

/// The stream information determined from the payload of a burst.
fn stream_info(payload: Payload, data: &[u8]) -> Option<EsInfo> {
    let mut parser = payload.parser();
    parser.push(data);

    loop {
        let frame = parser.next_frame(true);

        if let Some(info) = parser.info() {
            return Some(info.clone());
        }

        frame?;
    }
}

/// The duration in samples of the burst.
fn burst_samples(payload: Payload, data: &[u8], period: usize) -> Option<u64> {
    match payload {
        Payload::Ac3 | Payload::Eac3 => Ac3Header::parse(data).map(|h| u64::from(h.samples)),
        // The period of the burst is not the duration of the frame, which is that of its layer.
        Payload::Mpeg => {
            let mut parser = payload.parser();
            parser.push(data);
            parser.next_frame(true).map(|f| f.dur)
        }
        _ => Some((period >> 2) as u64),
    }
}

/// A data burst.
struct Burst {
    /// The position of the sync words.
    pos: u64,
    data_type: u8,
    payload: Payload,
    /// The payload in bitstream order.
    data: Vec<u8>,
    /// The duration in samples.
    dur: u64,
}

/// Scan `buf` (the start of the stream) for the first burst. Returns the position, the byte
/// order, and the burst; and the position of the next sync words implied by the repetition period.
fn find_burst(buf: &[u8]) -> Option<(usize, bool, u8, Payload, usize, usize)> {
    // Try every sync candidate: the sync words may occur by chance in preceding data.
    buf.windows(4)
        .enumerate()
        .filter(|(_, w)| *w == SYNC_LE || *w == SYNC_BE)
        .find_map(|(pos, _)| parse_burst_at(buf, pos))
}

/// Parse the burst whose sync words are at `pos` in `buf`.
fn parse_burst_at(buf: &[u8], pos: usize) -> Option<(usize, bool, u8, Payload, usize, usize)> {
    let big_endian = buf[pos..pos + 4] == SYNC_BE;

    let hdr = buf.get(pos + 4..pos + BURST_HEADER_LEN)?;
    let (pc, pd) = if big_endian {
        (u16::from_be_bytes([hdr[0], hdr[1]]), u16::from_be_bytes([hdr[2], hdr[3]]))
    }
    else {
        (u16::from_le_bytes([hdr[0], hdr[1]]), u16::from_le_bytes([hdr[2], hdr[3]]))
    };

    let data_type = (pc & 0xff) as u8;
    let len = payload_len(data_type, pd);
    let start = pos + BURST_HEADER_LEN;
    let mut data = buf.get(start..start + len)?.to_vec();

    if !big_endian {
        swap_words(&mut data);
    }

    let (payload, period) = burst_layout(data_type, &data)?;

    // The payload must be valid for its data type.
    burst_samples(payload, &data, period)?;
    stream_info(payload, &data)?;

    Some((pos, big_endian, data_type, payload, period, len))
}

/// An IEC 61937 data burst reader.
///
/// `SpdifReader` emits the payload of every burst as a packet, in bitstream order. The codec is
/// that of the data type of the bursts. If it changes, the reader returns `ResetRequired` with
/// the new track, and then the packet.
pub struct SpdifReader<'s> {
    reader: MediaSourceStream<'s>,
    tracks: Vec<Track>,
    media_info: MediaInfo,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    /// `Some(true)` if the sync words are big-endian. Decided by the first burst.
    big_endian: Option<bool>,
    /// The position of the first burst.
    start_pos: u64,
    /// The data type of the last burst, to detect when to determine the stream again.
    last_type: Option<u8>,
    /// The timestamp of the next burst.
    next_ts: i64,
    /// The timestamp and position of every burst found, increasing.
    index: Vec<(i64, u64)>,
    /// True if `index` covers the whole stream.
    scanned: bool,
    /// The total number of frames of the stream, known once scanned.
    total_frames: u64,
    pending: Option<Packet>,
}

/// Map the end of the stream to `None`.
fn eof_to_none<T>(e: io::Error) -> Result<Option<T>> {
    if e.kind() == io::ErrorKind::UnexpectedEof { Ok(None) } else { Err(e.into()) }
}

impl<'s> SpdifReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        if !mss.is_seekable() {
            mss.ensure_seekback_buffer(PROBE_LEN);
        }

        let mut reader = SpdifReader {
            reader: mss,
            tracks: Vec::new(),
            media_info: MediaInfo::default(),
            metadata: opts.external_data.metadata.unwrap_or_default(),
            chapters: opts.external_data.chapters,
            big_endian: None,
            start_pos: 0,
            last_type: None,
            next_ts: 0,
            index: Vec::new(),
            scanned: false,
            total_frames: 0,
            pending: None,
        };

        // The first burst determines the stream. Bursts of other types are skipped until one
        // is found, or the stream ends.
        let burst = loop {
            match reader.read_burst() {
                Ok(Some(burst)) => break burst,
                Ok(None) => return unsupported_error("spdif: no data burst found"),
                Err(Error::DecodeError(_)) => (),
                Err(e) => return Err(e),
            }
        };

        reader.start_pos = burst.pos;

        let info = stream_info(burst.payload, &burst.data)
            .ok_or(Error::DecodeError("spdif: invalid payload"))?;

        reader.set_track(&info)?;
        reader.last_type = Some(burst.data_type);
        reader.pending = Some(reader.make_packet(burst));

        if opts.prebuild_seek_index && reader.reader.is_seekable() {
            reader.scan()?;
        }

        Ok(reader)
    }

    /// Replace the track with one of the stream `info`.
    fn set_track(&mut self, info: &EsInfo) -> Result<()> {
        let mut track = Track::new(0);

        track
            .with_time_base(
                TimeBase::try_from_recip(info.rate).ok_or(Error::DecodeError("spdif: bad rate"))?,
            )
            .with_codec_params(CodecParameters::Audio(info.params.clone()))
            .with_start_ts(Timestamp::new(0));

        if self.scanned {
            track.with_num_frames(self.total_frames);
            track.with_duration(Duration::new(self.total_frames));
        }

        debug!("spdif: {:?}, {} Hz", info.params.codec, info.rate);

        self.media_info = MediaInfo::from_track(&track);
        self.tracks = vec![track];
        Ok(())
    }

    /// Build the packet of `burst` and advance the timeline.
    fn make_packet(&mut self, burst: Burst) -> Packet {
        let ts = self.next_ts;
        self.next_ts += burst.dur as i64;

        if self.index.last().is_none_or(|&(_, pos)| burst.pos > pos) {
            self.index.push((ts, burst.pos));
        }

        let mut packet = Packet::new(0, Timestamp::new(ts), Duration::new(burst.dur), burst.data);
        packet.dts = Timestamp::new(ts);
        packet
    }

    /// Read the next burst. Returns `Ok(None)` at the end of the stream, and a decode error
    /// for a burst of an unsupported type (which is consumed).
    fn read_burst(&mut self) -> Result<Option<Burst>> {
        let mut state = [0u8; 4];

        // Search for the sync words byte by byte, as FFmpeg does.
        let big_endian = loop {
            match self.reader.read_byte() {
                Ok(b) => state = [state[1], state[2], state[3], b],
                Err(e) => return eof_to_none(e),
            }

            match self.big_endian {
                Some(be) if state == if be { SYNC_BE } else { SYNC_LE } => break be,
                None if state == SYNC_LE => break false,
                None if state == SYNC_BE => break true,
                _ => (),
            }
        };

        self.big_endian = Some(big_endian);

        let pos = self.reader.pos() - 4;

        let mut hdr = [0u8; 4];

        if let Err(e) = self.reader.read_buf_exact(&mut hdr) {
            return eof_to_none(e);
        }

        let (pc, pd) = if big_endian {
            (u16::from_be_bytes([hdr[0], hdr[1]]), u16::from_be_bytes([hdr[2], hdr[3]]))
        }
        else {
            (u16::from_le_bytes([hdr[0], hdr[1]]), u16::from_le_bytes([hdr[2], hdr[3]]))
        };

        let data_type = (pc & 0xff) as u8;
        let len = payload_len(data_type, pd);

        let mut data = vec![0; len];

        if let Err(e) = self.reader.read_buf_exact(&mut data) {
            return eof_to_none(e);
        }

        if !big_endian {
            swap_words(&mut data);
        }

        let Some((payload, period)) = burst_layout(data_type, &data)
        else {
            return decode_error("spdif: unsupported data type or invalid payload");
        };

        // Skip the padding up to the next burst.
        if let Some(pad) = period.checked_sub(len + BURST_HEADER_LEN) {
            if let Err(e) = self.reader.ignore_bytes(pad as u64) {
                if e.kind() != io::ErrorKind::UnexpectedEof {
                    return Err(e.into());
                }
            }
        }

        let Some(dur) = burst_samples(payload, &data, period)
        else {
            return decode_error("spdif: invalid payload");
        };

        Ok(Some(Burst { pos, data_type, payload, data, dur }))
    }

    /// Read the whole stream to build the index and the duration. Leaves the stream at the
    /// start of the first burst.
    fn scan(&mut self) -> Result<()> {
        info!("spdif: scanning stream for duration");

        self.reader.seek(SeekFrom::Start(self.start_pos))?;
        self.next_ts = 0;
        self.index.clear();

        loop {
            match self.read_burst() {
                Ok(Some(burst)) => {
                    drop(self.make_packet(burst));
                }
                Ok(None) => break,
                Err(Error::DecodeError(_)) => (),
                Err(e) => return Err(e),
            }
        }

        self.scanned = true;

        let total = self.next_ts as u64;
        self.total_frames = total;

        for track in &mut self.tracks {
            track.with_num_frames(total);
            track.with_duration(Duration::new(total));
        }

        self.media_info = MediaInfo::from_track(&self.tracks[0]);

        self.reader.seek(SeekFrom::Start(self.start_pos))?;
        self.next_ts = 0;
        self.pending = None;
        Ok(())
    }

    fn next_packet_inner(&mut self) -> Result<Option<Packet>> {
        if let Some(packet) = self.pending.take() {
            return Ok(Some(packet));
        }

        // Skip pause, null, and unsupported bursts, as the scan does.
        let burst = loop {
            match self.read_burst() {
                Ok(Some(burst)) => break burst,
                Ok(None) => return Ok(None),
                Err(Error::DecodeError(_)) => (),
                Err(e) => return Err(e),
            }
        };

        // A different data type may be a different codec.
        if self.last_type != Some(burst.data_type) {
            self.last_type = Some(burst.data_type);

            let info = stream_info(burst.payload, &burst.data)
                .ok_or(Error::DecodeError("spdif: invalid payload"))?;

            let same = match &self.tracks[0].codec_params {
                Some(CodecParameters::Audio(p)) => p.codec == info.params.codec,
                _ => false,
            };

            if !same {
                self.set_track(&info)?;
                self.pending = Some(self.make_packet(burst));
                return reset_error();
            }
        }

        Ok(Some(self.make_packet(burst)))
    }

    fn seek_inner(&mut self, to: SeekTo) -> Result<SeekedTo> {
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

        if !self.reader.is_seekable() {
            return seek_error(SeekErrorKind::ForwardOnly);
        }

        if !self.scanned {
            self.scan()?;
        }

        if required_ts.get() >= self.tracks[0].duration.map_or(0, |d| d.get() as i64) {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // Every burst can start decoding: the last burst that starts at or before the timestamp.
        let idx = self.index.partition_point(|&(ts, _)| ts <= required_ts.get());
        let Some(&(ts, pos)) = idx.checked_sub(1).and_then(|i| self.index.get(i))
        else {
            return seek_error(SeekErrorKind::OutOfRange);
        };

        debug!("spdif: seeking to ts={required_ts}, burst at {ts}");

        self.reader.seek(SeekFrom::Start(pos))?;
        self.next_ts = ts;
        self.pending = None;

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts: Timestamp::new(ts) })
    }
}

impl Scoreable for SpdifReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut buf = vec![0; PROBE_LEN];
        let mut len = 0;
        let mut eof = false;

        while len < buf.len() {
            match src.read_buf(&mut buf[len..]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => len += n,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    eof = true;
                    break;
                }
                Err(e) => return Err(e.into()),
            }
        }

        let buf = &buf[..len];

        let Some((pos, _, _, _, period, plen)) = find_burst(buf)
        else {
            return Ok(Score::Unsupported);
        };

        // A second burst where the repetition period says confirms the stream (FFmpeg requires
        // two consecutive bursts for the maximum score).
        let next = pos + period;

        if next + 4 <= buf.len()
            && (buf[next..next + 4] == SYNC_LE || buf[next..next + 4] == SYNC_BE)
        {
            Ok(Score::Supported(255))
        }
        else if eof && pos + BURST_HEADER_LEN + plen + 4 > buf.len() {
            // The whole of a short stream is one burst.
            Ok(Score::Supported(100))
        }
        else {
            Ok(Score::Supported(25))
        }
    }
}

impl<'s> ProbeableFormat<'s> for SpdifReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'s>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + 's>> {
        Ok(Box::new(SpdifReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            SPDIF_FORMAT_INFO,
            &["spdif", "iec61937"],
            &["audio/x-iec61937"],
            &[&SYNC_LE, &SYNC_BE]
        )]
    }
}

impl FormatReader for SpdifReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &SPDIF_FORMAT_INFO
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
        self.seek_inner(to)
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
