// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::io::{ErrorKind, Seek, SeekFrom};

use symphonia_common::mpeg::es::{AdtsEs, EsParser, EsTrack, LatmEs, MpaEs, OpusEs};
use symphonia_common::mpeg::pes::{
    PTS_MODULUS, PesHeader, parse_pes_header, pts_delta, unwrap_pts,
};
use symphonia_common::mpeg::seek::bisect;
use symphonia_common::mpeg::timeline::ticks_to_clock;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error, unsupported_error};
use symphonia_core::formats::TrackFlags;
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, TimeBase, Timestamp};

use log::{debug, info, warn};

use crate::psi::{PAT_PID, Pmt, PmtStream, SectionAssembler, parse_pat, parse_pmt};

const TS_SYNC: u8 = 0x47;

/// The length of a transport stream packet.
const TS_PACKET_LEN: usize = 188;

/// The largest transport stream packet: 188 bytes plus 16 bytes of Reed-Solomon parity.
const MAX_PACKET_SIZE: usize = 204;

/// The transport stream packet sizes: plain, BDAV (M2TS, with a 4 byte time stamp), and with
/// Reed-Solomon parity.
const PACKET_SIZES: [usize; 3] = [188, 192, 204];

/// The number of consecutive packets, with sync bytes, required to be confident of a transport
/// stream.
const SCORE_SYNC_PACKETS: usize = 5;

/// The maximum number of bytes to scan for the PAT and PMTs.
const MAX_PSI_SCAN_LEN: u64 = 2 * 1024 * 1024;

/// The maximum number of bytes to scan for the first frames of the audio streams.
const MAX_ES_PROBE_LEN: u64 = 4 * 1024 * 1024;

/// The maximum number of bytes at the end of the stream to scan for the last PES packets of the
/// streams, to find the duration.
const MAX_TAIL_WINDOW: u64 = 64 * 1024 * 1024;

/// The range, in bytes, below which the bisection stops, and the stream is scanned forward.
const BISECT_MIN_GAP: u64 = 64 * 1024;

/// The number of bytes to scan for a PES packet with a PTS when probing during a seek.
const PROBE_SCAN_LEN: u64 = 512 * 1024;

const MPEGTS_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"MPTS")),
    short_name: "mpegts",
    long_name: "MPEG Transport Stream",
};

/// Finds the packet size of a transport stream from the first bytes. Returns the packet size, if
/// the data begins with at least [`SCORE_SYNC_PACKETS`] consecutive packets with valid headers, or
/// if `eof` and the data is (entirely) a smaller number of packets.
fn detect_layout(buf: &[u8], eof: bool) -> Option<(usize, usize)> {
    for size in PACKET_SIZES {
        let mut count = 0;

        while let Some(p) = buf.get(count * size..) {
            // The adaptation field control value 0 is reserved.
            if p.len() < 4 || p[0] != TS_SYNC || (p[3] >> 4) & 3 == 0 {
                break;
            }
            count += 1;
        }

        if count >= SCORE_SYNC_PACKETS {
            return Some((size, count));
        }

        // A short stream that consists only of packets (with the last possibly missing its
        // trailer).
        if eof && count >= 2 && buf.len() < (count + 1) * size - (size - TS_PACKET_LEN) {
            return Some((size, count));
        }
    }

    None
}

/// A transport stream packet with a payload.
struct TsPacket {
    /// The position of the sync byte of the packet.
    pos: u64,
    pid: u16,
    /// Payload unit start indicator.
    pusi: bool,
    /// The range of the payload in the packet buffer.
    payload: std::ops::Range<usize>,
}

/// Search for a sync byte that is followed, one packet later, by another. Positions the stream at
/// the sync byte. Returns false if the end of the stream was reached.
fn resync(reader: &mut MediaSourceStream<'_>, size: usize) -> Result<bool> {
    loop {
        match reader.read_byte() {
            Ok(TS_SYNC) => (),
            Ok(_) => continue,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(false),
            Err(e) => return Err(e.into()),
        }

        // Check for another sync byte one packet later.
        match reader.ignore_bytes(size as u64 - 1).and_then(|_| reader.read_byte()) {
            Ok(TS_SYNC) => {
                // Return to the first sync byte.
                reader.seek_buffered_rev(size + 1);
                return Ok(true);
            }
            Ok(_) => {
                // Continue the search after the false sync byte.
                reader.seek_buffered_rev(size);
            }
            // A single packet at the end of the stream.
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(false),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Read the next transport stream packet that has a payload. Returns `None` at the end of the
/// stream.
fn read_packet(
    reader: &mut MediaSourceStream<'_>,
    size: usize,
    buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<Option<TsPacket>> {
    loop {
        let pos = reader.pos();

        // The trailer (time stamp or parity) of the last packet may be missing.
        let mut n = 0;

        while n < size {
            match reader.read_buf(&mut buf[n..size]) {
                Ok(0) => break,
                Ok(read) => n += read,
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
        }

        if n < TS_PACKET_LEN {
            return Ok(None);
        }

        if buf[0] != TS_SYNC {
            debug!("mpegts: lost sync at {pos}");

            // Return to just after the bad sync byte, and search for sync.
            reader.seek_buffered_rev(n - 1);

            if !resync(reader, size)? {
                return Ok(None);
            }

            continue;
        }

        let pid = u16::from(buf[1] & 0x1f) << 8 | u16::from(buf[2]);
        let pusi = buf[1] & 0x40 != 0;
        let transport_error = buf[1] & 0x80 != 0;
        let scrambled = buf[3] >> 6 != 0;
        let afc = (buf[3] >> 4) & 3;

        if transport_error || scrambled || afc & 1 == 0 {
            continue;
        }

        let start = if afc & 2 != 0 { 5 + usize::from(buf[4]) } else { 4 };

        if start >= TS_PACKET_LEN {
            continue;
        }

        return Ok(Some(TsPacket { pos, pid, pusi, payload: start..TS_PACKET_LEN }));
    }
}

/// Reassembles the PES packets of an elementary stream.
#[derive(Default)]
struct PesAssembler {
    buf: Vec<u8>,
    active: bool,
}

impl PesAssembler {
    /// Add the payload of a packet. Completed PES packets are appended to `out`.
    fn push(&mut self, pusi: bool, payload: &[u8], out: &mut Vec<Vec<u8>>) {
        if pusi {
            if self.active && !self.buf.is_empty() {
                out.push(std::mem::take(&mut self.buf));
            }

            self.buf.clear();
            self.buf.extend_from_slice(payload);
            self.active = true;
        }
        else if self.active {
            self.buf.extend_from_slice(payload);
        }
        else {
            return;
        }

        // A bounded PES packet is complete once all of its bytes have been received.
        if self.buf.len() >= 6 && self.buf[..3] == [0, 0, 1] {
            let len = usize::from(u16::from_be_bytes([self.buf[4], self.buf[5]]));

            if len != 0 && self.buf.len() >= 6 + len {
                self.buf.truncate(6 + len);
                out.push(std::mem::take(&mut self.buf));
                self.active = false;
            }
        }
    }

    fn flush(&mut self) -> Option<Vec<u8>> {
        self.active = false;
        if self.buf.is_empty() { None } else { Some(std::mem::take(&mut self.buf)) }
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.active = false;
    }
}

/// Split a complete PES packet into the PES header and the payload.
fn split_pes(pes: &[u8]) -> Option<(PesHeader, &[u8])> {
    let hdr = parse_pes_header(pes)?;

    let end = if hdr.packet_len != 0 { (6 + hdr.packet_len).min(pes.len()) } else { pes.len() };
    let payload = pes.get(hdr.payload_offset..end)?;

    Some((hdr, payload))
}

/// The PTS and the payloads of the last PES packet with a PTS of a stream, and of those after it.
type LastPes = Option<(u64, Vec<Vec<u8>>)>;

/// An audio elementary stream.
struct Stream {
    pid: u16,
    language: Option<String>,
    es: EsTrack,
    asm: PesAssembler,
    /// The (raw) PTS of the first PES packet.
    first_pts: Option<u64>,
}

impl Stream {
    fn handle_pes(&mut self, pes: &[u8]) {
        if let Some((hdr, payload)) = split_pes(pes) {
            if self.first_pts.is_none() {
                self.first_pts = hdr.pts;
            }

            self.es.push_pes(hdr.pts, payload);
        }
    }
}

/// Create an elementary stream parser for a PMT stream, if the stream is a supported audio
/// stream.
fn make_parser(stream: &PmtStream) -> Option<Box<dyn EsParser>> {
    let d = &stream.descriptors;

    match stream.stream_type {
        // MPEG-1 and MPEG-2 audio.
        0x03 | 0x04 => Some(Box::new(MpaEs::new())),
        // AAC with ADTS.
        0x0f => Some(Box::new(AdtsEs::new())),
        // AAC with LATM (LOAS).
        0x11 => Some(Box::new(LatmEs::new())),
        // PES private data: Opus is identified by its registration descriptor or its extension
        // descriptor.
        0x06 if d.registration == Some(*b"Opus") || d.opus_channel_config.is_some() => {
            // A channel configuration of 0 is dual mono.
            let channels = match d.opus_channel_config {
                Some(0) => 2,
                Some(c @ 1..=8) => c,
                Some(c) => {
                    warn!("mpegts: unsupported opus channel configuration {c:#x}");
                    return None;
                }
                None => 2,
            };

            OpusEs::new(channels).map(|es| Box::new(es) as Box<dyn EsParser>)
        }
        _ => None,
    }
}

/// MPEG transport stream (MPEG-TS) format reader.
///
/// `MpegTsReader` implements an audio-only demuxer for MPEG-2 transport streams (ISO/IEC
/// 13818-1), including the 192 byte BDAV (M2TS) and 204 byte packet variants. See the crate
/// documentation for the supported streams, and the timeline.
pub struct MpegTsReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    /// The position of the first packet.
    start_pos: u64,
    packet_size: usize,
    streams: Vec<Stream>,
    out: VecDeque<Packet>,
    eof: bool,
    /// The (raw) PTS that is time 0 of the timeline.
    base_pts: Option<u64>,
    /// The timestamp of the last packet returned, in the timebase of the track it belongs to.
    last_ts: Option<(u32, i64)>,
}

impl<'s> MpegTsReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // The probe positions the stream at the first sync byte, but the stream may also be opened
        // directly, in which case it may begin with the time stamp of the first BDAV packet.
        let mut start_pos = mss.pos();

        // Determine the packet size.
        let mut peek = vec![0u8; (SCORE_SYNC_PACKETS + 1) * MAX_PACKET_SIZE];
        let mut peek_len = 0;

        while peek_len < peek.len() {
            match mss.read_buf(&mut peek[peek_len..]) {
                Ok(0) => break,
                Ok(n) => peek_len += n,
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
        }

        mss.seek_buffered_rev(peek_len);

        let eof = peek_len < peek.len();

        let layout = (0..MAX_PACKET_SIZE.min(peek_len)).find_map(|skip| {
            let (size, count) = detect_layout(&peek[skip..peek_len], eof)?;
            (skip == 0 || count >= SCORE_SYNC_PACKETS).then_some((skip, size))
        });

        let Some((skip, packet_size)) = layout
        else {
            return unsupported_error("mpegts: not a transport stream");
        };

        if skip > 0 {
            mss.ignore_bytes(skip as u64)?;
            start_pos += skip as u64;
        }

        debug!("mpegts: packet size {packet_size}");

        let mut buf = [0u8; MAX_PACKET_SIZE];

        // Find the programs.
        let pmts = Self::scan_psi(&mut mss, packet_size, &mut buf)?;

        let mut streams: Vec<Stream> = vec![];

        for pmt in &pmts {
            for stream in &pmt.streams {
                if streams.iter().any(|s| s.pid == stream.pid) {
                    continue;
                }

                if let Some(parser) = make_parser(stream) {
                    streams.push(Stream {
                        pid: stream.pid,
                        language: stream.descriptors.language.clone(),
                        es: EsTrack::new(u32::from(stream.pid), parser),
                        asm: Default::default(),
                        first_pts: None,
                    });
                }
            }
        }

        if streams.is_empty() {
            return unsupported_error("mpegts: no supported audio streams");
        }

        // Rewind to the start so that all of the data of the streams is read. A stream that cannot
        // be rewound continues from the PMT.
        if mss.is_seekable() {
            mss.seek(SeekFrom::Start(start_pos))?;
        }

        let mut reader = MpegTsReader {
            reader: mss,
            media_info: MediaInfo::new(),
            tracks: vec![],
            metadata: opts.external_data.metadata.unwrap_or_default(),
            chapters: opts.external_data.chapters,
            start_pos,
            packet_size,
            streams,
            out: VecDeque::new(),
            eof: false,
            base_pts: None,
            last_ts: None,
        };

        reader.probe_streams()?;

        if reader.streams.is_empty() {
            return unsupported_error(
                "mpegts: could not find the first frames of any audio stream",
            );
        }

        reader.build_tracks()?;

        // If the end of the stream was reached while probing, the frames that were parsed are all
        // that is left.
        if reader.eof {
            reader.flush_streams();
        }

        Ok(reader)
    }

    /// Scan the start of the stream for the PAT and the PMTs of the programs in the PAT.
    fn scan_psi(
        mss: &mut MediaSourceStream<'_>,
        packet_size: usize,
        buf: &mut [u8; MAX_PACKET_SIZE],
    ) -> Result<Vec<Pmt>> {
        let start = mss.pos();

        let mut pat_asm = SectionAssembler::default();
        let mut programs: Option<Vec<(u16, SectionAssembler, Option<Pmt>)>> = None;

        while mss.pos() - start < MAX_PSI_SCAN_LEN {
            let Some(pkt) = read_packet(mss, packet_size, buf)?
            else {
                break;
            };

            let payload = &buf[pkt.payload.clone()];

            if pkt.pid == PAT_PID && programs.is_none() {
                if let Some(entries) = pat_asm.push(pkt.pusi, payload).and_then(|s| parse_pat(&s)) {
                    programs = Some(
                        entries
                            .iter()
                            .map(|e| (e.pmt_pid, SectionAssembler::default(), None))
                            .collect(),
                    );
                }
            }
            else if let Some(programs) = programs.as_mut() {
                if let Some(prog) = programs.iter_mut().find(|p| p.0 == pkt.pid) {
                    if prog.2.is_none() {
                        prog.2 = prog.1.push(pkt.pusi, payload).and_then(|s| parse_pmt(&s));
                    }
                }

                if programs.iter().all(|p| p.2.is_some()) {
                    break;
                }
            }
        }

        let pmts: Vec<Pmt> = programs.into_iter().flatten().filter_map(|p| p.2).collect();

        if pmts.is_empty() {
            return unsupported_error("mpegts: no program map table found");
        }

        Ok(pmts)
    }

    /// Read the start of the streams until the codec parameters of all of the audio streams are
    /// known, and the first timestamps. Streams for which that fails are dropped.
    fn probe_streams(&mut self) -> Result<()> {
        let start = self.reader.pos();
        let mut buf = [0u8; MAX_PACKET_SIZE];
        let mut completed = vec![];

        while self.reader.pos() - start < MAX_ES_PROBE_LEN
            && !self.streams.iter().all(|s| s.es.info().is_some() && s.first_pts.is_some())
        {
            let Some(pkt) = read_packet(&mut self.reader, self.packet_size, &mut buf)?
            else {
                self.eof = true;
                break;
            };

            let Some(stream) = self.streams.iter_mut().find(|s| s.pid == pkt.pid)
            else {
                continue;
            };

            completed.clear();
            stream.asm.push(pkt.pusi, &buf[pkt.payload.clone()], &mut completed);

            for pes in completed.drain(..) {
                stream.handle_pes(&pes);
            }

            stream.es.probe(false);
        }

        // Flush the partial PES packets if the end of the stream was reached.
        for stream in self.streams.iter_mut() {
            if self.eof {
                if let Some(pes) = stream.asm.flush() {
                    stream.handle_pes(&pes);
                }
            }

            stream.es.probe(self.eof);
        }

        self.streams.retain(|s| {
            let keep = s.es.info().is_some();

            if !keep {
                warn!("mpegts: dropping pid {:#x}: could not parse the stream", s.pid);
            }

            keep
        });

        Ok(())
    }

    /// Create the tracks, and determine the durations.
    fn build_tracks(&mut self) -> Result<()> {
        // The timeline begins at the earliest first PTS, adjusted so that the first sample that is
        // not trimmed has the timestamp 0 for a single stream.
        let mut base: Option<u64> = None;

        for stream in &self.streams {
            let Some(pts) = stream.first_pts
            else {
                continue;
            };

            let rate = stream.es.info().map_or(1, |i| i.rate);
            let lead = ticks_to_clock(stream.es.lead_in().unwrap_or(0) as i64, rate);
            let pts = (pts + lead as u64) % PTS_MODULUS;

            base = Some(match base {
                Some(b) if pts_delta(pts, b) >= 0 => b,
                _ => pts,
            });
        }

        self.base_pts = base;

        for stream in self.streams.iter_mut() {
            stream.es.init_timeline(base);
        }

        // The durations of the streams, if the stream can be scanned from the end.
        let ends = self.scan_tail()?;

        for (i, stream) in self.streams.iter().enumerate() {
            let info = stream.es.info().expect("stream was probed");
            let rate = info.rate;

            let mut track = Track::new(u32::from(stream.pid));

            track.with_time_base(
                TimeBase::try_from_recip(rate)
                    .ok_or(Error::DecodeError("mpegts: invalid sample rate"))?,
            );
            track.with_codec_params(CodecParameters::Audio(info.params.clone()));

            if let Some(lang) = &stream.language {
                track.with_language(lang);
            }

            if i == 0 {
                track.with_flags(TrackFlags::DEFAULT);
            }

            // The timestamp of the first sample that is not trimmed.
            let lead_in = stream.es.lead_in().unwrap_or(0) as i64;

            let start_ts = match (stream.first_pts, base) {
                (Some(pts), Some(base)) => {
                    stream.es.timeline().ticks_for_clock(pts_delta(pts, base)) + lead_in
                }
                _ => 0,
            };

            if lead_in > 0 {
                track.with_delay(lead_in as u32);
            }

            track.with_start_ts(Timestamp::new(start_ts));

            if let Some(end_ts) = ends.get(i).copied().flatten() {
                if end_ts > start_ts {
                    let dur = (end_ts - start_ts) as u64;
                    let out_rate = info.params.sample_rate.unwrap_or(rate);
                    track.with_duration(Duration::new(dur));
                    track.with_num_frames(dur * u64::from(out_rate) / u64::from(rate));
                }
            }

            self.tracks.push(track);
        }

        self.media_info = MediaInfo::from_tracks(&self.tracks);

        Ok(())
    }

    /// Scan the end of the stream for the last PES packets, and return the end timestamp of each
    /// stream in the stream's timebase.
    fn scan_tail(&mut self) -> Result<Vec<Option<i64>>> {
        let mut ends = vec![None; self.streams.len()];

        let (Some(len), true) = (self.reader.byte_len(), self.reader.is_seekable())
        else {
            return Ok(ends);
        };

        let restore = self.reader.pos();
        let size = self.packet_size as u64;
        let mut buf = [0u8; MAX_PACKET_SIZE];

        let mut window = 1024 * 1024u64;

        loop {
            let begin = len.saturating_sub(window).max(self.start_pos);
            let aligned = self.start_pos + (begin - self.start_pos).div_ceil(size) * size;

            self.reader.seek(SeekFrom::Start(aligned))?;

            let mut asms: Vec<PesAssembler> =
                self.streams.iter().map(|_| Default::default()).collect();
            let mut last: Vec<LastPes> = vec![None; self.streams.len()];
            let mut completed = vec![];

            // Keep the payload of the last PES packet with a PTS, and of the packets after it
            // (which may complete its last frame).
            let record = |i: usize, pes: &[u8], last: &mut [LastPes]| {
                if let Some((hdr, payload)) = split_pes(pes) {
                    match (hdr.pts, last[i].as_mut()) {
                        (Some(pts), _) => last[i] = Some((pts, vec![payload.to_vec()])),
                        (None, Some((_, data))) => data.push(payload.to_vec()),
                        (None, None) => (),
                    }
                }
            };

            while let Some(pkt) = read_packet(&mut self.reader, self.packet_size, &mut buf)? {
                if let Some(i) = self.streams.iter().position(|s| s.pid == pkt.pid) {
                    completed.clear();
                    asms[i].push(pkt.pusi, &buf[pkt.payload.clone()], &mut completed);

                    for pes in completed.drain(..) {
                        record(i, &pes, &mut last);
                    }
                }
            }

            for (i, asm) in asms.iter_mut().enumerate() {
                if let Some(pes) = asm.flush() {
                    record(i, &pes, &mut last);
                }
            }

            let complete = last.iter().all(|l| l.is_some());

            if complete || aligned <= self.start_pos || window >= MAX_TAIL_WINDOW {
                for (i, l) in last.into_iter().enumerate() {
                    let Some((pts, payload)) = l
                    else {
                        continue;
                    };

                    let stream = &self.streams[i];
                    if let Some(base) = self.base_pts {
                        let clock = pts_delta(pts, base);
                        let end = stream.es.timeline().ticks_for_clock(clock)
                            + stream.es.payload_duration(&payload) as i64;
                        ends[i] = Some(end);
                    }
                }

                break;
            }

            window = window.saturating_mul(8);
        }

        self.reader.seek(SeekFrom::Start(restore))?;

        Ok(ends)
    }

    /// Find the first PES packet that starts with a PTS, of the stream with `pid`, at or after the
    /// byte position `pos`. Returns the position of the transport stream packet and the PTS.
    fn probe_pes(&mut self, pos: u64, pid: u16) -> Result<Option<(u64, u64)>> {
        let size = self.packet_size as u64;
        let aligned = self.start_pos + pos.saturating_sub(self.start_pos).div_ceil(size) * size;

        self.reader.seek(SeekFrom::Start(aligned))?;

        let mut buf = [0u8; MAX_PACKET_SIZE];

        while self.reader.pos() - aligned < PROBE_SCAN_LEN {
            let Some(pkt) = read_packet(&mut self.reader, self.packet_size, &mut buf)?
            else {
                return Ok(None);
            };

            if pkt.pid != pid || !pkt.pusi {
                continue;
            }

            if let Some(pts) = parse_pes_header(&buf[pkt.payload.clone()]).and_then(|h| h.pts) {
                return Ok(Some((pkt.pos, pts)));
            }
        }

        Ok(None)
    }

    /// Flush the partial PES packets and frames of all streams at the end of the stream.
    fn flush_streams(&mut self) {
        for stream in self.streams.iter_mut() {
            if let Some(pes) = stream.asm.flush() {
                stream.handle_pes(&pes);
            }

            while let Some(packet) = stream.es.pop(true) {
                self.out.push_back(packet);
            }
        }

        self.eof = true;
    }

    /// Read and process one transport stream packet. Returns false at the end of the stream.
    fn read_one(&mut self) -> Result<bool> {
        let mut buf = [0u8; MAX_PACKET_SIZE];

        let Some(pkt) = read_packet(&mut self.reader, self.packet_size, &mut buf)?
        else {
            self.flush_streams();
            return Ok(false);
        };

        if let Some(stream) = self.streams.iter_mut().find(|s| s.pid == pkt.pid) {
            let mut completed = vec![];
            stream.asm.push(pkt.pusi, &buf[pkt.payload.clone()], &mut completed);

            for pes in completed {
                stream.handle_pes(&pes);
            }

            while let Some(packet) = stream.es.pop(false) {
                self.out.push_back(packet);
            }
        }

        Ok(true)
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

impl Scoreable for MpegTsReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut buf = vec![0u8; SCORE_SYNC_PACKETS * MAX_PACKET_SIZE];
        let mut len = 0;

        while len < buf.len() {
            match src.read_buf(&mut buf[len..]) {
                Ok(0) => break,
                Ok(n) => len += n,
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
        }

        Ok(match detect_layout(&buf[..len], len < buf.len()) {
            Some((_, count)) if count >= SCORE_SYNC_PACKETS => Score::Supported(255),
            // A short stream.
            Some(_) => Score::Supported(64),
            None => Score::Unsupported,
        })
    }
}

/// The probe markers: the sync byte, followed by the first byte of the PID: the payload unit
/// start indicator (as set for the first packet of a table or PES packet) and the upper bits of
/// PID (the PAT, SDT, PMT, and audio PIDs commonly in the range `0x0000..=0x1fff`), or without
/// the payload unit start indicator.
macro_rules! ts_markers {
    ($($b:literal),* $(,)?) => {
        &[$(&[TS_SYNC, $b]),*]
    };
}

const TS_MARKERS: &[&[u8]] = ts_markers![
    0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f,
    0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f
];

impl ProbeableFormat<'_> for MpegTsReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(MpegTsReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            MPEGTS_FORMAT_INFO,
            &["ts", "m2ts", "mts", "m2t", "tsv", "trp"],
            &["video/mp2t", "audio/mp2t"],
            TS_MARKERS
        )]
    }
}

impl FormatReader for MpegTsReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &MPEGTS_FORMAT_INFO
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
        // Select the track, and the timestamp in its timebase.
        let (track_idx, required_ts) = match to {
            SeekTo::Timestamp { ts, track_id } => {
                match self.tracks.iter().position(|t| t.id == track_id) {
                    Some(idx) => (idx, ts),
                    None => return seek_error(SeekErrorKind::Unseekable),
                }
            }
            SeekTo::Time { time, track_id } => {
                let idx = match track_id {
                    Some(id) => self.tracks.iter().position(|t| t.id == id),
                    None => Some(0),
                };

                let Some(idx) = idx
                else {
                    return seek_error(SeekErrorKind::Unseekable);
                };

                let tb = self.tracks[idx]
                    .time_base
                    .ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                let ts =
                    tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

                (idx, ts)
            }
        };

        let track_id = self.tracks[track_idx].id;
        let track_start = self.tracks[track_idx].start_ts;
        let rate = self.streams[track_idx].es.info().map_or(1, |i| i.rate);

        if required_ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // The timestamp must be within the duration, if it is known.
        if let Some(dur) = self.tracks[track_idx].duration {
            if required_ts.get() > track_start.get().saturating_add(dur.get() as i64) {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        // The timestamp to start decoding from, so that a decoder reproduces a continuous decode
        // at the required timestamp.
        let start_ts = self.streams[track_idx]
            .es
            .parser()
            .seek_start_ts(required_ts.get())
            .max(track_start.get());

        debug!("mpegts: seeking to ts={required_ts} (start from {start_ts})");

        if self.reader.is_seekable() && self.reader.byte_len().is_some() {
            let len = self.reader.byte_len().unwrap_or(0);
            let pid = self.streams[track_idx].pid;

            let Some(base) = self.base_pts
            else {
                return seek_error(SeekErrorKind::Unseekable);
            };

            let target_clock = ticks_to_clock(start_ts, rate);

            // Find the position of the last PES packet that is at or before the target.
            let start_pos = self.start_pos;

            let pos = bisect(start_pos, len, target_clock, BISECT_MIN_GAP, |pos| {
                Ok::<_, Error>(
                    self.probe_pes(pos, pid)?
                        .map(|(pos, pts)| (pos, unwrap_pts(pts, base, target_clock))),
                )
            })?;

            self.reader.seek(SeekFrom::Start(pos))?;
            self.out.clear();
            self.eof = false;

            for stream in self.streams.iter_mut() {
                stream.asm.reset();
                stream.es.seek(target_clock);
            }
        }
        else if self.last_ts.is_some_and(|(id, ts)| id == track_id && required_ts.get() < ts) {
            return seek_error(SeekErrorKind::ForwardOnly);
        }

        // Scan forward to the first packet that ends after the timestamp to start from.
        loop {
            match self.next_packet_inner()? {
                Some(packet)
                    if packet.track_id == track_id
                        && packet.pts.get().saturating_add(packet.dur.get() as i64) > start_ts =>
                {
                    let actual_ts = packet.pts;
                    self.out.push_front(packet);

                    // The packet returned next is at the position seeked to.
                    self.last_ts = Some((track_id, actual_ts.get()));

                    info!("mpegts: seeked to ts={actual_ts}");

                    return Ok(SeekedTo { track_id, required_ts, actual_ts });
                }
                Some(_) => (),
                None => return seek_error(SeekErrorKind::OutOfRange),
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
