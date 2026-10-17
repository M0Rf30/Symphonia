// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::io::{ErrorKind, Seek, SeekFrom};

use symphonia_codec_aac::detect_implicit_sbr;
use symphonia_common::mpeg::es::{AdtsEs, EsParser, EsTrack, LatmEs, MpaEs, OpusEs};
use symphonia_common::mpeg::pes::{parse_pes_header, split_pes};
use symphonia_common::mpeg::surround::{Ac3Es, Ac4Es, DcaEs, TrueHdEs};
use symphonia_common::mpeg::timeline::ticks_to_clock;
use symphonia_common::mpeg::track::{
    LastPes, PROBE_SCAN_LEN, SeekPlan, bisect_pts, build_audio_track, check_forward_only,
    earliest_base, find_seek_packet, plan_seek, scan_tail, tail_ends,
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

use crate::bdlpcm::BdLpcmEs;
use crate::psi::{
    PAT_PID, PatEntry, Pmt, PmtStream, SectionAssembler, StreamDescriptors, parse_pat, parse_pmt,
};
use crate::sniff::SniffEs;

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

/// The maximum size of a PES packet that is assembled: the largest PES packet with a bounded
/// length. A PES packet of unbounded length (only allowed for video) is released at this size.
const MAX_PES_LEN: usize = 6 + 65535;

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
    /// The continuity counter.
    cc: u8,
    /// The discontinuity indicator of the adaptation field.
    discontinuity: bool,
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

        // The discontinuity indicator is bit 7 of the adaptation field flags.
        let discontinuity = afc & 2 != 0 && buf[4] > 0 && buf[5] & 0x80 != 0;

        return Ok(Some(TsPacket {
            pos,
            pid,
            pusi,
            cc: buf[3] & 0x0f,
            discontinuity,
            payload: start..TS_PACKET_LEN,
        }));
    }
}

/// Reassembles the PES packets of an elementary stream.
#[derive(Default)]
struct PesAssembler {
    buf: Vec<u8>,
    active: bool,
    /// The continuity counter of the last packet accepted.
    cc: Option<u8>,
}

impl PesAssembler {
    /// Add the payload of a transport stream packet, checking the continuity counter (ISO/IEC
    /// 13818-1 §2.4.3.3). A duplicate packet is dropped. If packets were lost, the PES packet
    /// being assembled is damaged and is discarded, and assembly resumes at the next PES packet.
    /// Completed PES packets are appended to `out`.
    fn push_packet(&mut self, pkt: &TsPacket, payload: &[u8], out: &mut Vec<Vec<u8>>) {
        if !pkt.discontinuity {
            if self.cc == Some(pkt.cc) {
                // A duplicate: the packet was sent twice.
                return;
            }

            if self.cc.is_some_and(|cc| (cc + 1) & 0x0f != pkt.cc) {
                debug!("mpegts: continuity error at {}", pkt.pos);
                self.buf.clear();
                self.active = false;
            }
        }

        self.cc = Some(pkt.cc);
        self.push(pkt.pusi, payload, out);
    }

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

        if self.buf.len() >= MAX_PES_LEN {
            self.buf.truncate(MAX_PES_LEN);
            out.push(std::mem::take(&mut self.buf));
            self.active = false;
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
        self.cc = None;
    }
}

/// The `stream_id_extension` of the PES packets of the AC-3 stream that is embedded in a Blu-ray
/// TrueHD stream.
const EMBEDDED_AC3_STREAM_ID_EXT: u8 = 0x76;

/// The first track ID of the streams that share the PID of another stream: the PID plus this
/// offset, which is beyond the range of PIDs (13 bits).
const SHARED_PID_TRACK_ID_BASE: u32 = 0x2000;

/// Selects the PES packets of a PID that belong to a stream, for PIDs that carry two streams.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PesFilter {
    /// All of the PES packets.
    All,
    /// The PES packets of the TrueHD stream in a Blu-ray TrueHD PID: those that are not the
    /// embedded AC-3 stream.
    TrueHd,
    /// The PES packets of the AC-3 stream embedded in a Blu-ray TrueHD PID.
    EmbeddedAc3,
}

impl PesFilter {
    fn accepts(self, extended_stream_id: Option<u8>) -> bool {
        let embedded = extended_stream_id == Some(EMBEDDED_AC3_STREAM_ID_EXT);

        match self {
            PesFilter::All => true,
            PesFilter::TrueHd => !embedded,
            PesFilter::EmbeddedAc3 => embedded,
        }
    }
}

/// An audio elementary stream.
struct Stream {
    pid: u16,
    track_id: u32,
    filter: PesFilter,
    language: Option<String>,
    es: EsTrack,
    asm: PesAssembler,
    /// The (raw) PTS of the first PES packet.
    first_pts: Option<u64>,
}

impl Stream {
    fn handle_pes(&mut self, pes: &[u8]) {
        if let Some((hdr, payload)) = split_pes(pes) {
            if !self.filter.accepts(hdr.extended_stream_id) {
                return;
            }

            if self.first_pts.is_none() {
                self.first_pts = hdr.pts;
            }

            self.es.push_pes(hdr.pts, payload);
        }
    }
}

/// Returns true if the program is a Blu-ray program: its registration descriptor is `HDMV` or
/// `HDPR`, which enables the Blu-ray stream types.
fn is_hdmv(registration: Option<[u8; 4]>) -> bool {
    matches!(registration, Some(r) if r == *b"HDMV" || r == *b"HDPR")
}

/// Create the parser of an AC-3, AC-4 or DTS stream identified by the registration descriptor or a DVB
/// descriptor of a stream.
fn make_parser_from_descriptors(d: &StreamDescriptors) -> Option<Box<dyn EsParser>> {
    let is_dts = matches!(d.registration, Some(r) if [*b"DTS1", *b"DTS2", *b"DTS3"].contains(&r));
    let is_ac3 = matches!(d.registration, Some(r) if r == *b"AC-3" || r == *b"EAC3");

    if is_ac3 || d.has_ac3 || d.has_eac3 {
        Some(Box::new(Ac3Es::new()))
    }
    else if d.has_ac4 || d.registration == Some(*b"AC-4") {
        Some(Box::new(Ac4Es::new()))
    }
    else if is_dts || d.has_dts {
        Some(Box::new(DcaEs::new()))
    }
    else {
        None
    }
}

/// Returns why an audio stream that is recognised cannot be demuxed, if it cannot.
fn unsupported_stream(stream: &PmtStream) -> Option<&'static str> {
    match stream.stream_type {
        // HLS SAMPLE-AES (Apple, "MPEG-2 Stream Encryption Format for HTTP Live Streaming"): the
        // audio frames are AES-128 encrypted, and the key is not part of the stream. 0xc1 is
        // AC-3, 0xc2 is E-AC-3, and 0xcf is AAC (ADTS).
        0xc1 | 0xc2 | 0xcf => {
            Some("HLS SAMPLE-AES encrypted audio, the frames cannot be decrypted")
        }
        _ => None,
    }
}

/// Create the elementary stream parsers for a PMT stream, if the stream is a supported audio
/// stream. A Blu-ray TrueHD stream has two: TrueHD, and the AC-3 stream embedded in the PID.
/// `hdmv` is true if the program is a Blu-ray program.
fn make_parsers(stream: &PmtStream, hdmv: bool) -> Vec<(PesFilter, Box<dyn EsParser>)> {
    let d = &stream.descriptors;

    if let Some(reason) = unsupported_stream(stream) {
        warn!(
            "mpegts: PID {:#x} (stream type {:#04x}) is ignored: {reason}",
            stream.pid, stream.stream_type
        );
        return vec![];
    }

    let parser: Box<dyn EsParser> = match stream.stream_type {
        // MPEG-1 and MPEG-2 audio.
        0x03 | 0x04 => Box::new(MpaEs::new()),
        // AAC with ADTS.
        0x0f => Box::new(AdtsEs::with_detector(detect_implicit_sbr)),
        // AAC with LATM (LOAS).
        0x11 => Box::new(LatmEs::with_detector(detect_implicit_sbr)),
        // PES private data: Opus is identified by its registration descriptor or its extension
        // descriptor.
        0x06 if d.registration == Some(*b"Opus") || d.opus_channel_config.is_some() => {
            // A channel configuration of 0 is dual mono.
            let channels = match d.opus_channel_config {
                Some(0) => 2,
                Some(c @ 1..=8) => c,
                Some(c) => {
                    warn!("mpegts: unsupported opus channel configuration {c:#x}");
                    return vec![];
                }
                None => 2,
            };

            match OpusEs::new(channels) {
                Some(es) => Box::new(es),
                None => return vec![],
            }
        }
        // Blu-ray LPCM.
        0x80 => Box::new(BdLpcmEs::new()),
        // AC-3 (ATSC A/52, and Blu-ray), and E-AC-3 (ATSC).
        0x81 | 0x87 => Box::new(Ac3Es::new()),
        // E-AC-3 of Blu-ray: the primary (0x84) and secondary (0xa1) audio.
        0x84 | 0xa1 if hdmv => Box::new(Ac3Es::new()),
        // DTS and DTS-HD of Blu-ray (0x82, 0x85, 0x86) and the secondary audio (0xa2) (the latter
        // only in Blu-ray programs), and DTS (0x8a).
        0x82 | 0x85 | 0x86 | 0x8a => Box::new(DcaEs::new()),
        0xa2 if hdmv => Box::new(DcaEs::new()),
        // Dolby TrueHD of Blu-ray (also written without the Blu-ray registration descriptor, by
        // FFmpeg). The PID also carries an AC-3 version of the audio, in PES packets that are
        // identified by their stream ID extension. The AC-3 stream is dropped if there is none.
        0x83 => {
            return vec![
                (PesFilter::TrueHd, Box::new(TrueHdEs::new())),
                (PesFilter::EmbeddedAc3, Box::new(Ac3Es::new())),
            ];
        }
        // PES private data, and user private stream types, that are identified by their
        // descriptors (AC-3, E-AC-3, DTS). PES private data without any descriptors does not
        // identify its codec: the stream is identified from the data.
        0x06 | 0x80..=0xff => match make_parser_from_descriptors(d) {
            Some(parser) => parser,
            None if stream.stream_type == 0x06 && stream.descriptor_len == 0 => {
                Box::new(SniffEs::new(vec![
                    Box::new(AdtsEs::with_detector(detect_implicit_sbr)),
                    Box::new(LatmEs::with_detector(detect_implicit_sbr)),
                    Box::new(MpaEs::new()),
                ]))
            }
            None => return vec![],
        },
        _ => return vec![],
    };

    vec![(PesFilter::All, parser)]
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
            let hdmv = is_hdmv(pmt.registration);

            for stream in &pmt.streams {
                if streams.iter().any(|s| s.pid == stream.pid) {
                    continue;
                }

                for (i, (filter, parser)) in make_parsers(stream, hdmv).into_iter().enumerate() {
                    let track_id = u32::from(stream.pid) + i as u32 * SHARED_PID_TRACK_ID_BASE;

                    streams.push(Stream {
                        pid: stream.pid,
                        track_id,
                        filter,
                        language: stream.descriptors.language.clone(),
                        es: EsTrack::new(track_id, parser),
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
        // The entries of each section of the PAT, by section number, and the last section number.
        let mut pat_sections: Vec<Option<Vec<PatEntry>>> = vec![];
        // The PMT PIDs, with the assembler of each, and the programs (PAT entries) with the PMT of
        // each. Several programs may share a PMT PID.
        let mut pmt_asms: Vec<(u16, SectionAssembler)> = vec![];
        let mut programs: Option<Vec<(PatEntry, Option<Pmt>)>> = None;

        while mss.pos() - start < MAX_PSI_SCAN_LEN {
            let Some(pkt) = read_packet(mss, packet_size, buf)?
            else {
                break;
            };

            let payload = &buf[pkt.payload.clone()];

            if pkt.pid == PAT_PID && programs.is_none() {
                let mut section = pat_asm.push(pkt.pusi, payload);

                while let Some(sec) = section {
                    if let Some(entries) = parse_pat(&sec) {
                        // The section number and last section number.
                        let (num, last) = (usize::from(sec[6]), usize::from(sec[7]));

                        if pat_sections.len() != last + 1 {
                            pat_sections = vec![None; last + 1];
                        }

                        if let Some(slot) = pat_sections.get_mut(num) {
                            *slot = Some(entries);
                        }
                    }

                    section = pat_asm.take_section();
                }

                if !pat_sections.is_empty() && pat_sections.iter().all(|s| s.is_some()) {
                    let entries: Vec<PatEntry> =
                        pat_sections.drain(..).flatten().flatten().collect();

                    for e in &entries {
                        if !pmt_asms.iter().any(|a| a.0 == e.pmt_pid) {
                            pmt_asms.push((e.pmt_pid, SectionAssembler::default()));
                        }
                    }

                    programs = Some(entries.into_iter().map(|e| (e, None)).collect());
                }
            }
            else if let Some(programs) = programs.as_mut() {
                if let Some(asm) = pmt_asms.iter_mut().find(|a| a.0 == pkt.pid) {
                    let mut section = asm.1.push(pkt.pusi, payload);

                    while let Some(sec) = section {
                        if let Some(pmt) = parse_pmt(&sec) {
                            if let Some(prog) = programs.iter_mut().find(|p| {
                                p.1.is_none()
                                    && p.0.pmt_pid == pkt.pid
                                    && p.0.program_number == pmt.program_number
                            }) {
                                prog.1 = Some(pmt);
                            }
                        }

                        section = asm.1.take_section();
                    }
                }

                if programs.iter().all(|p| p.1.is_some()) {
                    break;
                }
            }
        }

        let pmts: Vec<Pmt> = programs.into_iter().flatten().filter_map(|p| p.1).collect();

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

            for stream in self.streams.iter_mut().filter(|s| s.pid == pkt.pid) {
                completed.clear();
                stream.asm.push_packet(&pkt, &buf[pkt.payload.clone()], &mut completed);

                for pes in completed.drain(..) {
                    stream.handle_pes(&pes);
                }

                stream.es.probe(false);
            }
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
        let base = earliest_base(self.streams.iter().map(|s| (&s.es, s.first_pts)));

        self.base_pts = base;

        for stream in self.streams.iter_mut() {
            stream.es.init_timeline(base);
        }

        // The durations of the streams, if the stream can be scanned from the end.
        let ends = self.scan_tail()?;

        for (i, stream) in self.streams.iter().enumerate() {
            let mut track = build_audio_track(
                stream.track_id,
                &stream.es,
                stream.first_pts,
                base,
                ends.get(i).copied().flatten(),
                "mpegts: invalid sample rate",
            )?;

            if let Some(lang) = &stream.language {
                track.with_language(lang);
            }

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
        let size = self.packet_size as u64;
        let start_pos = self.start_pos;
        let mut buf = [0u8; MAX_PACKET_SIZE];

        let last = scan_tail(len, start_pos, self.streams.len(), |begin, last| {
            let aligned = start_pos + (begin - start_pos).div_ceil(size) * size;

            self.reader.seek(SeekFrom::Start(aligned))?;

            let mut asms: Vec<PesAssembler> =
                self.streams.iter().map(|_| Default::default()).collect();
            let mut completed = vec![];

            // Keep the payload of the last PES packet with a PTS, and of the packets after it
            // (which may complete its last frame).
            let record = |pes: &[u8], filter: PesFilter, last: &mut LastPes| {
                if let Some((hdr, payload)) = split_pes(pes) {
                    if filter.accepts(hdr.extended_stream_id) {
                        last.record(hdr.pts, payload);
                    }
                }
            };

            while let Some(pkt) = read_packet(&mut self.reader, self.packet_size, &mut buf)? {
                for (i, stream) in self.streams.iter().enumerate().filter(|s| s.1.pid == pkt.pid) {
                    completed.clear();
                    asms[i].push_packet(&pkt, &buf[pkt.payload.clone()], &mut completed);

                    for pes in completed.drain(..) {
                        record(&pes, stream.filter, &mut last[i]);
                    }
                }
            }

            for (i, asm) in asms.iter_mut().enumerate() {
                if let Some(pes) = asm.flush() {
                    record(&pes, self.streams[i].filter, &mut last[i]);
                }
            }

            Ok::<_, Error>(aligned)
        })?;

        self.reader.seek(SeekFrom::Start(restore))?;

        Ok(tail_ends(&last, self.streams.iter().map(|s| &s.es), self.base_pts))
    }

    /// Find the first PES packet that starts with a PTS, of the stream with `pid`, at or after the
    /// byte position `pos`. Returns the position of the transport stream packet and the PTS.
    fn probe_pes(&mut self, pos: u64, pid: u16, filter: PesFilter) -> Result<Option<(u64, u64)>> {
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

            if let Some(pts) = parse_pes_header(&buf[pkt.payload.clone()])
                .filter(|h| filter.accepts(h.extended_stream_id))
                .and_then(|h| h.pts)
            {
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

        for stream in self.streams.iter_mut().filter(|s| s.pid == pkt.pid) {
            let mut completed = vec![];
            stream.asm.push_packet(&pkt, &buf[pkt.payload.clone()], &mut completed);

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
/// start indicator (as set for the first packet of a table or PES packet), the transport priority,
/// and the upper bits of the PID, with the transport error indicator clear.
macro_rules! ts_markers {
    ($($b:literal),* $(,)?) => {
        &[$(&[TS_SYNC, $b]),*]
    };
}

const TS_MARKERS: &[&[u8]] = ts_markers![
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
    0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f,
    0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f,
    0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f,
    0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f,
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
        let SeekPlan { track_idx, track_id, required_ts, start_ts, rate } =
            plan_seek(&self.tracks, self.streams.iter().map(|s| &s.es), to)?;

        debug!("mpegts: seeking to ts={required_ts} (start from {start_ts})");

        if self.reader.is_seekable() && self.reader.byte_len().is_some() {
            let len = self.reader.byte_len().unwrap_or(0);
            let (pid, filter) = (self.streams[track_idx].pid, self.streams[track_idx].filter);

            let Some(base) = self.base_pts
            else {
                return seek_error(SeekErrorKind::Unseekable);
            };

            let target_clock = ticks_to_clock(start_ts, rate);

            // Find the position of the last PES packet that is at or before the target.
            let start_pos = self.start_pos;

            let pos = bisect_pts(start_pos, len, base, target_clock, |pos| {
                self.probe_pes(pos, pid, filter)
            })?;

            self.reader.seek(SeekFrom::Start(pos))?;
            self.out.clear();
            self.eof = false;

            for stream in self.streams.iter_mut() {
                stream.asm.reset();
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

        info!("mpegts: seeked to ts={actual_ts}");

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

    #[test]
    fn verify_unbounded_pes_is_capped() {
        // A PES packet of an audio PID with the packet length 0, followed by a long run of
        // continuation packets.
        let mut asm = PesAssembler::default();
        let mut out = vec![];

        let mut first = vec![0, 0, 1, 0xc0, 0, 0, 0x80, 0, 0];
        first.resize(184, 0xaa);
        asm.push(true, &first, &mut out);

        for _ in 0..10_000 {
            asm.push(false, &[0xaa; 184], &mut out);
            assert!(asm.buf.len() <= MAX_PES_LEN);
        }

        assert!(!out.is_empty());
        assert!(out.iter().all(|pes| pes.len() <= MAX_PES_LEN));
    }

    fn pmt_stream(stream_type: u8, descriptors: StreamDescriptors) -> PmtStream {
        PmtStream { stream_type, pid: 0x101, descriptors, descriptor_len: 0 }
    }

    #[test]
    fn verify_encrypted_streams_are_reported_unsupported_and_ac4_is_parsed() {
        // HLS SAMPLE-AES streams are dropped even if their descriptors identify AC-3 or E-AC-3.
        for stream_type in [0xc1, 0xc2, 0xcf] {
            let d = StreamDescriptors { has_ac3: true, ..Default::default() };
            let stream = pmt_stream(stream_type, d);
            assert!(unsupported_stream(&stream).unwrap().contains("SAMPLE-AES"));
            assert!(make_parsers(&stream, false).is_empty());
        }

        // AC-4 in PES private data.
        let d = StreamDescriptors { has_ac4: true, ..Default::default() };
        let stream = pmt_stream(0x06, d);
        assert!(unsupported_stream(&stream).is_none());
        assert_eq!(make_parsers(&stream, false).len(), 1);

        // Streams that are supported are unaffected.
        let stream = pmt_stream(0x81, StreamDescriptors::default());
        assert!(unsupported_stream(&stream).is_none());
        assert_eq!(make_parsers(&stream, false).len(), 1);
    }
}
