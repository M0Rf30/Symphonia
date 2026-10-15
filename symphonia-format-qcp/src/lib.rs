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

//! QCP (`.qcp`) demuxer.
//!
//! QCP is the RIFF-based file format of Qualcomm's PureVoice (RFC 3625, "The QCP File Format and
//! Media Types for Speech Data"). It stores QCELP 13K, EVRC, SMV or 4GV speech, one frame (of 160
//! samples at 8 kHz) per packet.
//!
//! The file is a RIFF chunk of the form type `QLCM`. The `fmt ` chunk holds the GUID of the codec,
//! the packet size, the sample rate and the rate-map table, which maps the rate of a packet to the
//! size of its payload. The `vrat` chunk tells whether the file has variable-rate packets, and the
//! number of packets. The `data` chunk is a sequence of packets of one rate byte followed by the
//! payload of the packet. The other chunks (`labl`, `cnfg`, `offs`, `text`, ...) are skipped.
//!
//! The packets returned have the rate byte followed by the payload, which is also the form of the
//! packets of QCELP and EVRC in the MP4 and AIFF containers.
//!
//! The packetization follows FFmpeg `libavformat/qcp.c`: if the file has a fixed packet size, all
//! packets have that size, whatever their rate byte; otherwise the size of the payload is looked
//! up in the rate-map table by the rate byte, and the bytes whose rate is not in the table are
//! skipped.

use std::io::{ErrorKind, Seek, SeekFrom};
use std::num::NonZero;

use log::{debug, warn};

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_EVRC, CODEC_ID_QCELP};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Error, Result, SeekErrorKind, decode_error, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{ChapterGroup, Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, TimeBase, Timestamp};

const RIFF_MARKER: [u8; 4] = *b"RIFF";
const QLCM_MARKER: [u8; 4] = *b"QLCM";
const FMT_MARKER: [u8; 4] = *b"fmt ";
const VRAT_MARKER: [u8; 4] = *b"vrat";
const DATA_MARKER: [u8; 4] = *b"data";

/// The minimum size of the `fmt ` chunk.
const FMT_MIN_LEN: u32 = 150;
/// The maximum number of chunks to look through for the `data` chunk.
const MAX_CHUNKS: usize = 4096;

/// The highest rate byte of the rate-map table.
const MAX_MODE: usize = 4;
/// The maximum number of entries of the rate-map table.
const MAX_RATE_ENTRIES: u32 = 8;

/// The number of samples in a packet, for QCELP and EVRC.
const FRAME_SAMPLES: u64 = 160;
/// The sample rate of QCELP and EVRC.
const DEFAULT_SAMPLE_RATE: u32 = 8000;

/// The last 15 of the 16 bytes of the GUID of QCELP-13K, as stored in the file. The first byte of
/// the GUID is 0x41 or 0x42.
const GUID_QCELP_13K_TAIL: [u8; 15] =
    [0x6d, 0x7f, 0x5e, 0x15, 0xb1, 0xd0, 0x11, 0xba, 0x91, 0x00, 0x80, 0x5f, 0xb4, 0xb9, 0x7e];
/// The GUID of EVRC, as stored in the file.
const GUID_EVRC: [u8; 16] = [
    0x8d, 0xd4, 0x89, 0xe6, 0x76, 0x90, 0xb5, 0x46, 0x91, 0xef, 0x73, 0x6a, 0x51, 0x00, 0xce, 0xb4,
];
/// The GUID of 4GV, as stored in the file.
const GUID_4GV: [u8; 16] = [
    0xca, 0x29, 0xfd, 0x3c, 0x53, 0xf6, 0xf5, 0x4e, 0x90, 0xe9, 0xf4, 0x23, 0x6d, 0x59, 0x9b, 0x61,
];
/// The GUID of SMV, as stored in the file.
const GUID_SMV: [u8; 16] = [
    0x75, 0x2b, 0x7c, 0x8d, 0x97, 0xa7, 0x49, 0xed, 0x98, 0x5e, 0xd5, 0x3c, 0x8c, 0xc7, 0x5f, 0x84,
];

const QCP_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"QCP\0")),
    short_name: "qcp",
    long_name: "QCP (RIFF QLCM)",
};

/// The codec of a file.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Codec {
    Qcelp,
    Evrc,
    /// SMV (Selectable Mode Vocoder) or 4GV: the packets are demuxed, but there is no decoder.
    Other,
}

impl Codec {
    fn from_guid(guid: &[u8; 16]) -> Result<Codec> {
        if (guid[0] == 0x41 || guid[0] == 0x42) && guid[1..] == GUID_QCELP_13K_TAIL {
            Ok(Codec::Qcelp)
        }
        else if *guid == GUID_EVRC {
            Ok(Codec::Evrc)
        }
        else if *guid == GUID_SMV || *guid == GUID_4GV {
            debug!("qcp: the codec (SMV or 4GV) is not supported by any decoder");
            Ok(Codec::Other)
        }
        else {
            decode_error("qcp: unknown codec GUID")
        }
    }
}

/// The parsed `fmt ` chunk.
struct FmtChunk {
    codec: Codec,
    sample_rate: u32,
    /// The size of a packet including the rate byte, or 0 if not fixed.
    packet_size: u16,
    /// For each rate byte, the size of the payload of a packet, if it is in the rate-map table.
    rates_per_mode: [Option<u8>; MAX_MODE + 1],
}

impl FmtChunk {
    fn read(reader: &mut MediaSourceStream<'_>, chunk_len: u32) -> Result<FmtChunk> {
        if chunk_len < FMT_MIN_LEN {
            return decode_error("qcp: fmt chunk is too small");
        }

        let _major_version = reader.read_u8()?;
        let _minor_version = reader.read_u8()?;

        let mut guid = [0u8; 16];
        reader.read_buf_exact(&mut guid)?;
        let codec = Codec::from_guid(&guid)?;

        let _codec_version = reader.read_u16()?;
        // The codec name.
        reader.ignore_bytes(80)?;
        let _average_bps = reader.read_u16()?;
        let packet_size = reader.read_u16()?;
        let _block_size = reader.read_u16()?;
        let sample_rate = u32::from(reader.read_u16()?);
        let _sample_size = reader.read_u16()?;

        let mut rates_per_mode = [None; MAX_MODE + 1];

        let num_rates = reader.read_u32()?.min(MAX_RATE_ENTRIES);

        for _ in 0..num_rates {
            let size = reader.read_u8()?;
            let mode = usize::from(reader.read_u8()?);

            match rates_per_mode.get_mut(mode) {
                Some(entry) => *entry = Some(size),
                None => warn!("qcp: unknown entry {} => {} in the rate-map table", mode, size),
            }
        }

        // The empty entries of the rate-map table, and 5 reserved words.
        reader.ignore_bytes(u64::from(16 - 2 * num_rates + 20))?;

        // The rest of the chunk (if the chunk is larger than the specified size).
        reader.ignore_bytes(u64::from(chunk_len - FMT_MIN_LEN))?;

        Ok(FmtChunk { codec, sample_rate, packet_size, rates_per_mode })
    }
}

/// The data of a file that is looked for in the chunks before the data chunk.
struct Header {
    fmt: FmtChunk,
    /// Whether the packets have a variable rate (from the `vrat` chunk).
    var_rate: bool,
    /// The number of packets (from the `vrat` chunk).
    num_packets: Option<u64>,
    /// The position of the start of the data of the `data` chunk.
    data_start: u64,
    /// The size of the data of the `data` chunk.
    data_len: u64,
}

fn read_header(reader: &mut MediaSourceStream<'_>) -> Result<Header> {
    if reader.read_quad_bytes()? != RIFF_MARKER {
        return decode_error("qcp: missing RIFF marker");
    }

    let _riff_len = reader.read_u32()?;

    if reader.read_quad_bytes()? != QLCM_MARKER {
        return decode_error("qcp: missing QLCM form type");
    }

    let mut fmt = None;
    let mut var_rate = false;
    let mut num_packets = None;

    for _ in 0..MAX_CHUNKS {
        // Chunks are padded to an even size.
        let id = reader.read_quad_bytes()?;
        let len = reader.read_u32()?;

        match id {
            FMT_MARKER => fmt = Some(FmtChunk::read(reader, len)?),
            VRAT_MARKER => {
                if len < 8 {
                    return decode_error("qcp: vrat chunk is too small");
                }
                var_rate = reader.read_u32()? != 0;
                num_packets = Some(u64::from(reader.read_u32()?));
                reader.ignore_bytes(u64::from(len - 8))?;
            }
            DATA_MARKER => {
                let Some(fmt) = fmt
                else {
                    return decode_error("qcp: data chunk before the fmt chunk");
                };

                let data_start = reader.pos();
                let mut data_len = u64::from(len);

                // Never trust the size of the data chunk beyond the end of the stream. A size of
                // 0xffff_ffff (an unknown size) is also clamped to the end of the stream.
                if let Some(byte_len) = reader.byte_len() {
                    data_len = data_len.min(byte_len.saturating_sub(data_start));
                }

                return Ok(Header { fmt, var_rate, num_packets, data_start, data_len });
            }
            _ => reader.ignore_bytes(u64::from(len))?,
        }

        // Skip the padding byte of an odd-sized chunk.
        if len & 1 == 1 {
            reader.ignore_bytes(1)?;
        }
    }

    decode_error("qcp: data chunk not found")
}

/// The layout of the packets in the data chunk.
#[derive(Copy, Clone, Debug)]
enum Layout {
    /// All packets have the same size (rate byte included).
    Fixed(u64),
    /// The size of the payload of a packet is looked up by its rate byte.
    Variable([Option<u8>; MAX_MODE + 1]),
}

/// Reads up to `buf.len()` bytes, until the end of the stream.
fn read_up_to<B: ReadBytes>(reader: &mut B, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;

    while n < buf.len() {
        match reader.read_buf(&mut buf[n..]) {
            Ok(0) => break,
            Ok(read) => n += read,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
    }

    Ok(n)
}

/// QCP format reader.
pub struct QcpReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    layout: Layout,
    data_start: u64,
    /// The end position of the data chunk.
    data_end: u64,
    /// The index of the next packet.
    next_frame: u64,
    /// The total number of packets, if known.
    num_frames: Option<u64>,
    /// The start positions of the packets that were seen, for variable-rate files.
    frame_pos: Vec<u64>,
    /// The frames per packet.
    frame_samples: u64,
}

impl<'s> QcpReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _opts: FormatOptions) -> Result<Self> {
        let header = read_header(&mut source)?;

        let layout = if header.var_rate || header.fmt.packet_size == 0 {
            Layout::Variable(header.fmt.rates_per_mode)
        }
        else {
            Layout::Fixed(u64::from(header.fmt.packet_size))
        };

        let sample_rate = match header.fmt.sample_rate {
            0 => DEFAULT_SAMPLE_RATE,
            rate => rate,
        };

        let mut params = AudioCodecParameters::new();
        params
            .with_sample_rate(sample_rate)
            .with_channels(Channels::Positioned(Position::FRONT_CENTER));

        match header.fmt.codec {
            Codec::Qcelp => params.for_codec(CODEC_ID_QCELP),
            Codec::Evrc => params.for_codec(CODEC_ID_EVRC),
            Codec::Other => &mut params,
        };

        params.with_frames_per_block(FRAME_SAMPLES).with_max_frames_per_packet(FRAME_SAMPLES);

        let data_end = header.data_start + header.data_len;

        // The number of packets is known for fixed-size packets. For variable-rate files, it is
        // the number of the `vrat` chunk.
        let num_frames = match layout {
            Layout::Fixed(size) => Some(header.data_len.div_ceil(size)),
            Layout::Variable(_) => header.num_packets.filter(|&n| n > 0),
        };

        let rate = NonZero::new(sample_rate).unwrap_or(NonZero::<u32>::MIN);
        let tb = TimeBase::new(NonZero::<u32>::MIN, rate);

        let mut track = Track::new(0);
        track.time_base = Some(tb);
        if let Some(n) = num_frames {
            track.num_frames = Some(n * FRAME_SAMPLES);
            track.duration = Some(Duration::new(n * FRAME_SAMPLES));
        }
        track.with_codec_params(CodecParameters::Audio(params));

        let mut media_info = MediaInfo::new();
        media_info.time_base = Some(tb);
        media_info.duration = num_frames.map(|n| Duration::new(n * FRAME_SAMPLES));

        Ok(QcpReader {
            reader: source,
            media_info,
            tracks: vec![track],
            metadata: MetadataLog::default(),
            layout,
            data_start: header.data_start,
            data_end,
            next_frame: 0,
            num_frames,
            frame_pos: Vec::new(),
            frame_samples: FRAME_SAMPLES,
        })
    }

    /// Reads the next packet, or skips it if `data` is false. Returns the packet (the rate byte and
    /// the payload) if `data` is true, or an empty slice if a packet was skipped, and `None` at
    /// the end of the data.
    fn read_packet(&mut self, data: bool) -> Result<Option<Box<[u8]>>> {
        loop {
            let pos = self.reader.pos();

            if pos >= self.data_end {
                return Ok(None);
            }

            let mode = self.reader.read_u8()?;
            // The data left after the rate byte.
            let left = self.data_end - pos - 1;

            let payload_len = match self.layout {
                Layout::Fixed(size) => size.saturating_sub(1),
                Layout::Variable(rates) => match rates.get(usize::from(mode)).copied().flatten() {
                    Some(size) => u64::from(size),
                    // A rate that is not in the table is skipped.
                    None => continue,
                },
            };

            if left < payload_len {
                warn!("qcp: the data chunk is too small for the last packet");
            }
            let payload_len = payload_len.min(left);

            // The start of this packet is known now.
            if self.next_frame == self.frame_pos.len() as u64 {
                self.frame_pos.push(pos);
            }

            if !data {
                self.reader.ignore_bytes(payload_len)?;
                return Ok(Some(Box::default()));
            }

            let mut buf = vec![0u8; 1 + payload_len as usize];
            buf[0] = mode;

            let read = read_up_to(&mut self.reader, &mut buf[1..])?;

            if read < payload_len as usize {
                // The end of the stream.
                warn!("qcp: unexpected end of data");
                if read == 0 && payload_len > 0 {
                    return Ok(None);
                }
                buf.truncate(1 + read);
            }

            return Ok(Some(buf.into_boxed_slice()));
        }
    }
}

impl Scoreable for QcpReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let marker = src.read_quad_bytes()?;
        src.ignore_bytes(4)?;
        let form = src.read_quad_bytes()?;
        let chunk = src.read_quad_bytes()?;

        Ok(if marker == RIFF_MARKER && form == QLCM_MARKER && chunk == FMT_MARKER {
            Score::Supported(255)
        }
        else {
            Score::Unsupported
        })
    }
}

impl ProbeableFormat<'_> for QcpReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(QcpReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            QCP_FORMAT_INFO,
            &["qcp"],
            &["audio/qcelp", "audio/evrc", "audio/vnd.qcelp", "audio/x-qcp"],
            &[b"RIFF"]
        )]
    }
}

impl FormatReader for QcpReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &QCP_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let Some(data) = self.read_packet(true)?
        else {
            return Ok(None);
        };

        let pts = self.next_frame * self.frame_samples;
        self.next_frame += 1;

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(pts as i64))
                .dur(Duration::new(self.frame_samples))
                .data(data)
                .build(),
        ))
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
        if !self.reader.is_seekable() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        let required_ts = match to {
            SeekTo::Timestamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                let tb =
                    self.tracks[0].time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).unwrap_or(Timestamp::ZERO)
            }
        };

        if required_ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let required_frame = required_ts.get() as u64 / self.frame_samples;

        if self.num_frames.is_some_and(|n| required_frame >= n) {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        match self.layout {
            Layout::Fixed(size) => {
                self.reader.seek(SeekFrom::Start(self.data_start + required_frame * size))?;
                self.next_frame = required_frame;
            }
            Layout::Variable(_) => {
                // Resume from the last known packet and scan forward to the required packet.
                let known = self.frame_pos.len() as u64;
                let from = required_frame.min(known.saturating_sub(1));

                match self.frame_pos.get(from as usize) {
                    Some(&pos) => self.reader.seek(SeekFrom::Start(pos))?,
                    None => self.reader.seek(SeekFrom::Start(self.data_start))?,
                };
                self.next_frame = from;

                while self.next_frame < required_frame {
                    if self.read_packet(false)?.is_none() {
                        return seek_error(SeekErrorKind::OutOfRange);
                    }
                    self.next_frame += 1;
                }
            }
        }

        Ok(SeekedTo {
            track_id: 0,
            required_ts,
            actual_ts: Timestamp::new((self.next_frame * self.frame_samples) as i64),
        })
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
