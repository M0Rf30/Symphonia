// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::io::{ErrorKind, Seek, SeekFrom};

use symphonia_common::mpeg::es::{EsParser, EsTrack, MpaEs};
use symphonia_common::mpeg::pes::{parse_pes_header, split_pes};
use symphonia_common::mpeg::surround::{Ac3Es, DcaEs, TrueHdEs};
use symphonia_common::mpeg::timeline::ticks_to_clock;
use symphonia_common::mpeg::track::{
    PROBE_SCAN_LEN, SeekPlan, bisect_pts, build_audio_track, check_forward_only, earliest_base,
    find_seek_packet, plan_seek, scan_tail, tail_ends,
};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error, unsupported_error};
use symphonia_core::formats::TrackFlags;
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;

use log::{debug, info, warn};

use crate::lpcm::LpcmEs;

const PACK_START: u8 = 0xba;
const SYSTEM_HEADER: u8 = 0xbb;
const PROGRAM_END: u8 = 0xb9;
const PRIVATE_STREAM_1: u8 = 0xbd;

/// The minimum number of bytes to scan for audio streams while opening a stream, once the
/// parameters of all of the streams found are known.
const MIN_DISCOVERY_LEN: u64 = 512 * 1024;

/// The maximum number of bytes to scan for audio streams while opening a stream.
const MAX_DISCOVERY_LEN: u64 = 4 * 1024 * 1024;

/// The number of bytes of a stream to score.
const SCORE_MAX_PACKETS: usize = 64;

const MPEGPS_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"MPPS")),
    short_name: "mpegps",
    long_name: "MPEG Program Stream",
};

/// Returns the length of the pack header (including the start code) at the start of `b` if it is
/// valid, or `None`. At least 14 bytes must be provided for MPEG-2 pack headers, which have
/// stuffing.
fn pack_header_len(b: &[u8]) -> Option<usize> {
    if b.len() < 12 || b[..4] != [0x00, 0x00, 0x01, PACK_START] {
        return None;
    }

    if b[4] >> 6 == 0b01 {
        // MPEG-2: the marker bits of the system clock reference and the program mux rate.
        if b.len() < 14
            || b[4] & 0x04 == 0
            || b[6] & 0x04 == 0
            || b[8] & 0x04 == 0
            || b[9] & 1 == 0
            || b[12] & 3 != 3
        {
            return None;
        }

        Some(14 + usize::from(b[13] & 7))
    }
    else if b[4] >> 4 == 0b0010 {
        // MPEG-1.
        if b[4] & 1 == 0 || b[6] & 1 == 0 || b[8] & 1 == 0 || b[9] & 0x80 == 0 || b[11] & 1 == 0 {
            return None;
        }

        Some(12)
    }
    else {
        None
    }
}

/// Examine the start of a stream. Returns the number of packets that follow a pack header with
/// valid structure, and whether any of them is an audio packet. Returns `None` if the data is not
/// a program stream.
fn examine(buf: &[u8], eof: bool) -> Option<(usize, bool)> {
    let mut pos = pack_header_len(buf)?;
    let mut packets = 0;

    if pos > buf.len() {
        return None;
    }

    let mut audio = false;

    while packets < SCORE_MAX_PACKETS {
        let rest = &buf[pos..];

        // The end of the buffer, in the middle of a packet or between packets.
        if rest.len() < 6 {
            if rest.is_empty() || !eof {
                break;
            }
            return None;
        }

        if rest[..3] != [0x00, 0x00, 0x01] {
            // Packets may be followed by zero bytes that pad the sector (Video CD).
            let zeros = rest.iter().take_while(|&&b| b == 0).count();

            if zeros == rest.len() {
                break;
            }

            if zeros < 3 || rest[zeros] != 1 {
                return None;
            }

            pos += zeros - 2;
            continue;
        }

        match rest[3] {
            PACK_START => match pack_header_len(rest) {
                Some(len) => pos += len,
                None if rest.len() < 14 => break,
                None => return None,
            },
            PROGRAM_END => pos += 4,
            code if code >= SYSTEM_HEADER => {
                let len = usize::from(u16::from_be_bytes([rest[4], rest[5]]));

                if len == 0 {
                    // An unbounded PES packet (only allowed for video) extends to the next system
                    // start code (ISO/IEC 13818-1, PES_packet_length == 0). Scan for it rather
                    // than stopping.
                    let next = rest[6..]
                        .windows(4)
                        .position(|w| w[..3] == [0, 0, 1] && w[3] >= PROGRAM_END);

                    match next {
                        Some(n) => {
                            pos += 6 + n;
                            packets += 1;
                            continue;
                        }
                        None => break,
                    }
                }

                match code {
                    0xc0..=0xdf => audio = true,
                    PRIVATE_STREAM_1 => {
                        // The sub-stream ID is after the PES header.
                        if let Some(hdr) = parse_pes_header(rest) {
                            audio |= rest.get(hdr.payload_offset..).is_some_and(|p| {
                                p.first().is_some_and(|&b| {
                                    (0x80..=0xcf).contains(&b) && !(0x90..=0x97).contains(&b)
                                }) || p.starts_with(&[0x0b, 0x77])
                            });
                        }
                    }
                    _ => (),
                }

                pos += 6 + len;
                packets += 1;
            }
            _ => return None,
        }

        if pos > buf.len() {
            break;
        }
    }

    // A pack header alone is not enough to be confident.
    (packets > 0 || eof).then_some((packets, audio))
}

/// An audio elementary stream.
struct Stream {
    /// The track ID.
    id: u32,
    es: EsTrack,
    /// The (raw) PTS of the first PES packet with one.
    first_pts: Option<u64>,
}

/// A source of PES packets of the audio streams.
enum Item {
    /// A PES packet of an audio stream: the stream ID, the sub-stream ID of private stream 1, the
    /// PTS, and the payload (including the sub-stream ID for private stream 1).
    Pes { key: u32, pts: Option<u64>, payload: Vec<u8> },
}

/// Scan for the next system start code, a sequence of `00 00 01` followed by a byte that is a
/// system code (`0xb9` and above). Returns the code and the position of the start of the code.
fn next_start_code<B: ReadBytes>(reader: &mut B) -> Result<Option<(u8, u64)>> {
    let mut zeros = 0u32;
    let mut one = false;

    loop {
        let byte = match reader.read_byte() {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        if one && byte >= PROGRAM_END {
            return Ok(Some((byte, reader.pos() - 4)));
        }

        match byte {
            0 => {
                zeros = (zeros + 1).min(2);
                one = false;
            }
            1 if zeros >= 2 => {
                one = true;
                zeros = 0;
            }
            _ => {
                zeros = 0;
                one = false;
            }
        }
    }
}

/// The key flag of a stream of MLP in the sub-streams `0xa0`..`0xaf` (those of LPCM have none).
const KEY_MLP: u32 = 0x1_0000;

/// Classify a private stream 1 payload as ffmpeg's `mpegps_read_packet` does. Returns the stream
/// key and the payload to give the parser: the payload of DVD LPCM keeps its headers, the
/// payload of the other codecs is the elementary stream with the private stream header removed.
fn private_stream_1(payload: &[u8]) -> Option<(u32, &[u8])> {
    let sub = *payload.first()?;

    // Raw AC-3 without a private stream header.
    if payload.starts_with(&[0x0b, 0x77]) {
        return Some((u32::from(PRIVATE_STREAM_1) << 8 | 0x80, payload));
    }

    let key = u32::from(PRIVATE_STREAM_1) << 8 | u32::from(sub);

    match sub {
        // AC-3, DTS, and AC-3/E-AC-3 (EVOB): the sub-stream ID, the number of frame headers, and
        // the first access unit pointer.
        0x80..=0x8f | 0x98..=0x9f | 0xc0..=0xcf => payload.get(4..).map(|p| (key, p)),
        // The (DVD-Video LPCM) or MLP. DVD LPCM has 0x80 as the last byte of the audio header.
        0xa0..=0xaf => {
            if payload.len() < 7 {
                None
            }
            else if payload[6] == 0x80 {
                (sub <= 0xa7).then_some((key, payload))
            }
            else {
                payload.get(10..).map(|p| (key | KEY_MLP, p))
            }
        }
        // TrueHD: a further byte of header.
        0xb0..=0xbf => payload.get(5..).map(|p| (key, p)),
        _ => None,
    }
}

/// Read the next PES packet of an audio stream, skipping the packets of other streams, and pack
/// and system headers. Returns the position of the PES packet.
fn next_audio_pes<B: ReadBytes>(reader: &mut B) -> Result<Option<(u64, Item)>> {
    loop {
        let Some((code, pos)) = next_start_code(reader)?
        else {
            return Ok(None);
        };

        match code {
            PACK_START | PROGRAM_END => continue,
            _ => (),
        }

        let len = match reader.read_be_u16() {
            Ok(l) => usize::from(l),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let is_audio = matches!(code, 0xc0..=0xdf | PRIVATE_STREAM_1);

        if !is_audio || len == 0 {
            if len > 0 && reader.ignore_bytes(len as u64).is_err() {
                return Ok(None);
            }
            continue;
        }

        let mut pes = Vec::with_capacity(6 + len);
        pes.extend_from_slice(&[0, 0, 1, code, (len >> 8) as u8, len as u8]);
        pes.resize(6 + len, 0);

        match reader.read_buf_exact(&mut pes[6..]) {
            Ok(()) => (),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        let Some((hdr, payload)) = split_pes(&pes)
        else {
            continue;
        };

        let (key, payload) = match code {
            PRIVATE_STREAM_1 => match private_stream_1(payload) {
                Some(kp) => kp,
                None => continue,
            },
            _ => (u32::from(code), payload),
        };

        return Ok(Some((pos, Item::Pes { key, pts: hdr.pts, payload: payload.to_vec() })));
    }
}

/// MPEG program stream (MPEG-PS) format reader.
///
/// `MpegPsReader` implements an audio-only demuxer for MPEG-1 system streams and MPEG-2 program
/// streams (ISO/IEC 11172-1, ISO/IEC 13818-1), including the DVD-Video VOB. See the crate
/// documentation for the supported streams, and the timeline.
pub struct MpegPsReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    /// The position of the first pack header.
    start_pos: u64,
    streams: Vec<Stream>,
    out: VecDeque<Packet>,
    eof: bool,
    /// The (raw) PTS that is time 0 of the timeline.
    base_pts: Option<u64>,
    /// The timestamp of the last packet returned, in the timebase of the track it belongs to.
    last_ts: Option<(u32, i64)>,
}

/// Create the parser for a stream key (the stream ID, or the stream ID and sub-stream ID).
fn make_parser(key: u32) -> Box<dyn EsParser> {
    if key <= 0xff {
        return Box::new(MpaEs::new());
    }

    match key & 0xff {
        0x80..=0x87 | 0xc0..=0xcf => Box::new(Ac3Es::new()),
        0x88..=0x8f | 0x98..=0x9f => Box::new(DcaEs::new()),
        0xa0..=0xaf if key & KEY_MLP == 0 => Box::new(LpcmEs::new()),
        _ => Box::new(TrueHdEs::new()),
    }
}

impl<'s> MpegPsReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let start_pos = mss.pos();

        let mut peek = vec![0u8; 16];
        let n = mss.read_buf(&mut peek)?;
        mss.seek_buffered_rev(n);

        if pack_header_len(&peek[..n]).is_none() {
            return unsupported_error("mpegps: stream does not begin with a pack header");
        }

        let mut reader = MpegPsReader {
            reader: mss,
            media_info: MediaInfo::new(),
            tracks: vec![],
            metadata: opts.external_data.metadata.unwrap_or_default(),
            chapters: opts.external_data.chapters,
            start_pos,
            streams: vec![],
            out: VecDeque::new(),
            eof: false,
            base_pts: None,
            last_ts: None,
        };

        reader.discover_streams()?;

        if reader.streams.is_empty() {
            return unsupported_error("mpegps: no supported audio streams");
        }

        reader.build_tracks()?;

        // If the end of the stream was reached while discovering the streams, and it could not be
        // rewound, the frames that were parsed are all that is left.
        if reader.eof {
            reader.flush_streams();
        }

        Ok(reader)
    }

    /// Add the PES packet to its stream. Streams are created if `create` is true.
    fn handle_item(&mut self, item: Item, create: bool) {
        let Item::Pes { key, pts, payload } = item;

        let idx = match self.streams.iter().position(|s| s.id == key) {
            Some(idx) => idx,
            None if create => {
                debug!("mpegps: found audio stream {key:#x}");

                self.streams.push(Stream {
                    id: key,
                    es: EsTrack::new(key & 0xffff, make_parser(key)),
                    first_pts: None,
                });

                self.streams.len() - 1
            }
            None => return,
        };

        let stream = &mut self.streams[idx];

        if stream.first_pts.is_none() {
            stream.first_pts = pts;
        }

        stream.es.push_pes(pts, &payload);
    }

    /// Scan the start of the stream for the audio streams, and the parameters of each.
    fn discover_streams(&mut self) -> Result<()> {
        let start = self.reader.pos();

        loop {
            let scanned = self.reader.pos() - start;
            let ready = !self.streams.is_empty()
                && self.streams.iter().all(|s| s.es.info().is_some())
                && scanned >= MIN_DISCOVERY_LEN;

            if ready || scanned >= MAX_DISCOVERY_LEN {
                break;
            }

            match next_audio_pes(&mut self.reader)? {
                Some((_, item)) => {
                    self.handle_item(item, true);

                    for stream in self.streams.iter_mut() {
                        stream.es.probe(false);
                    }
                }
                None => {
                    self.eof = true;
                    break;
                }
            }
        }

        for stream in self.streams.iter_mut() {
            stream.es.probe(self.eof);
        }

        self.streams.retain(|s| {
            let keep = s.es.info().is_some();

            if !keep {
                warn!("mpegps: dropping stream {:#x}: could not parse the stream", s.id);
            }

            keep
        });

        // Read the data of the streams from the start again. A stream that cannot be rewound
        // continues from where it is.
        if self.reader.is_seekable() {
            self.reader.seek(SeekFrom::Start(self.start_pos))?;
            self.eof = false;

            for stream in self.streams.iter_mut() {
                stream.es.rewind();
            }
        }

        Ok(())
    }

    /// Create the tracks, and determine the durations.
    fn build_tracks(&mut self) -> Result<()> {
        // The timeline begins at the earliest first PTS.
        let base = earliest_base(self.streams.iter().map(|s| (&s.es, s.first_pts)));

        self.base_pts = base;

        for stream in self.streams.iter_mut() {
            stream.es.init_timeline(base);
        }

        let ends = self.scan_tail()?;

        for (i, stream) in self.streams.iter().enumerate() {
            let mut track = build_audio_track(
                stream.id & 0xffff,
                &stream.es,
                stream.first_pts,
                base,
                ends.get(i).copied().flatten(),
                "mpegps: invalid sample rate",
            )?;

            if i == 0 {
                track.with_flags(TrackFlags::DEFAULT);
            }

            self.tracks.push(track);
        }

        self.media_info = MediaInfo::from_tracks(&self.tracks);

        Ok(())
    }

    /// Scan the end of the stream for the last PES packets, and return the end timestamp of each
    /// stream in the stream's timebase.
    fn scan_tail(&mut self) -> Result<Vec<Option<i64>>> {
        let (Some(len), true) = (self.reader.byte_len(), self.reader.is_seekable())
        else {
            return Ok(vec![None; self.streams.len()]);
        };

        let restore = self.reader.pos();
        let start_pos = self.start_pos;

        let last = scan_tail(len, start_pos, self.streams.len(), |begin, last| {
            self.reader.seek(SeekFrom::Start(begin))?;

            // Find a pack header to synchronise to, from which the packets are chained.
            if begin > start_pos {
                loop {
                    match next_start_code(&mut self.reader)? {
                        Some((PACK_START, pos)) => {
                            self.reader.seek(SeekFrom::Start(pos))?;
                            break;
                        }
                        Some(_) => (),
                        None => break,
                    }
                }
            }

            // The payload of the last PES packet with a PTS and the packets after it, of each
            // stream.
            while let Some((_, Item::Pes { key, pts, payload })) = next_audio_pes(&mut self.reader)?
            {
                if let Some(i) = self.streams.iter().position(|s| s.id == key) {
                    last[i].record(pts, payload);
                }
            }

            Ok::<_, Error>(begin)
        })?;

        self.reader.seek(SeekFrom::Start(restore))?;

        Ok(tail_ends(&last, self.streams.iter().map(|s| &s.es), self.base_pts))
    }

    /// Find the first PES packet with a PTS of the stream with `key` at or after the byte position
    /// `pos`. Returns the position of the PES packet and the PTS.
    fn probe_pes(&mut self, pos: u64, key: u32) -> Result<Option<(u64, u64)>> {
        self.reader.seek(SeekFrom::Start(pos))?;

        loop {
            if self.reader.pos() - pos >= PROBE_SCAN_LEN {
                return Ok(None);
            }

            // Packets found by scanning from an arbitrary position are validated by checking that
            // the following packet begins with a start code.
            let Some((pes_pos, Item::Pes { key: k, pts, .. })) = next_audio_pes(&mut self.reader)?
            else {
                return Ok(None);
            };

            if k & 0xffff != key {
                continue;
            }

            let Some(pts) = pts
            else {
                continue;
            };

            match self.reader.read_triple_bytes() {
                Ok([0, 0, 1]) | Err(_) => return Ok(Some((pes_pos, pts))),
                Ok(_) => continue,
            }
        }
    }

    fn read_one(&mut self) -> Result<bool> {
        match next_audio_pes(&mut self.reader)? {
            Some((_, item)) => {
                let Item::Pes { key, .. } = &item;
                let key = *key;

                self.handle_item(item, false);

                if let Some(stream) = self.streams.iter_mut().find(|s| s.id == key) {
                    while let Some(packet) = stream.es.pop(false) {
                        self.out.push_back(packet);
                    }
                }

                Ok(true)
            }
            None => {
                self.flush_streams();
                Ok(false)
            }
        }
    }

    /// Flush the frames of all streams at the end of the stream.
    fn flush_streams(&mut self) {
        for stream in self.streams.iter_mut() {
            while let Some(packet) = stream.es.pop(true) {
                self.out.push_back(packet);
            }
        }

        self.eof = true;
    }

    fn next_packet_inner(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(packet) = self.out.pop_front() {
                self.last_ts = Some((packet.track_id, packet.pts.get()));
                return Ok(Some(packet));
            }

            if self.eof {
                return Ok(None);
            }

            self.read_one()?;
        }
    }
}

impl Scoreable for MpegPsReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut buf = vec![0u8; 16 * 1024];
        let mut len = 0;
        let mut eof = false;

        while len < buf.len() {
            match src.read_buf(&mut buf[len..]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => len += n,
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                    eof = true;
                    break;
                }
                Err(e) => return Err(e.into()),
            }
        }

        Ok(match examine(&buf[..len], eof) {
            Some((_, true)) => Score::Supported(255),
            // A program stream, but the audio stream did not appear in the part examined.
            Some((_, false)) => Score::Supported(100),
            None => Score::Unsupported,
        })
    }
}

impl ProbeableFormat<'_> for MpegPsReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(MpegPsReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            MPEGPS_FORMAT_INFO,
            &["mpg", "mpeg", "vob", "ps", "m2p", "mpe"],
            &["video/mpeg", "video/mp2p", "video/x-mpeg"],
            &[&[0x00, 0x00, 0x01, PACK_START]]
        )]
    }
}

impl FormatReader for MpegPsReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &MPEGPS_FORMAT_INFO
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
        let SeekPlan { track_id, required_ts, start_ts, rate, .. } =
            plan_seek(&self.tracks, self.streams.iter().map(|s| &s.es), to)?;

        debug!("mpegps: seeking to ts={required_ts} (start from {start_ts})");

        if self.reader.is_seekable() && self.reader.byte_len().is_some() {
            let len = self.reader.byte_len().unwrap_or(0);

            let Some(base) = self.base_pts
            else {
                return seek_error(SeekErrorKind::Unseekable);
            };

            let target_clock = ticks_to_clock(start_ts, rate);
            let start_pos = self.start_pos;

            let pos = bisect_pts(start_pos, len, base, target_clock, |pos| {
                self.probe_pes(pos, track_id)
            })?;

            self.reader.seek(SeekFrom::Start(pos))?;
            self.out.clear();
            self.eof = false;

            for stream in self.streams.iter_mut() {
                stream.es.seek(target_clock);
            }
        }
        else {
            check_forward_only(self.last_ts, track_id, required_ts)?;
        }

        // Scan forward to the first packet that ends after the timestamp to start from.
        let packet = find_seek_packet(|| self.next_packet_inner(), track_id, start_ts)?;
        let actual_ts = packet.pts;
        self.out.push_front(packet);

        // The packet returned next is at the position seeked to.
        self.last_ts = Some((track_id, actual_ts.get()));

        info!("mpegps: seeked to ts={actual_ts}");

        Ok(SeekedTo { track_id, required_ts, actual_ts })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_header() -> Vec<u8> {
        vec![0, 0, 1, 0xba, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x89, 0xc3, 0xf8]
    }

    #[test]
    fn verify_leading_unbounded_pes_does_not_end_examination() {
        let mut buf = pack_header();
        // An unbounded video PES packet with payload (including a slice start code), then an
        // audio PES packet.
        buf.extend_from_slice(&[0, 0, 1, 0xe0, 0, 0, 0x80, 0, 0, 0xaa, 0, 0, 1, 0x01, 0xbb]);
        buf.extend_from_slice(&[0, 0, 1, 0xc0, 0, 8, 0x80, 0, 0, 1, 2, 3, 4, 5]);
        buf.extend_from_slice(&[0, 0, 1, 0xc0, 0, 8, 0x80, 0, 0, 1, 2, 3, 4, 5]);

        let (packets, audio) = examine(&buf, false).expect("program stream");
        assert_eq!(packets, 3);
        assert!(audio);
    }
}
