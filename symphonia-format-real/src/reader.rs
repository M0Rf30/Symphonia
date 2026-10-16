// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::io::{ErrorKind, Seek, SeekFrom};
use std::num::NonZero;
use std::sync::Arc;

use symphonia_core::audio::layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO};
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
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
use symphonia_core::units::{TimeBase, Timestamp};

use log::{debug, info, warn};

use crate::header::{self, AudioInfo, Interleave, RAW_PACKET_LEN};
use crate::stream::{AudioStream, FLAG_KEY};

const REAL_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"RMF ")),
    short_name: "real",
    long_name: "RealMedia",
};

const REAL_METADATA_INFO: MetadataInfo = MetadataInfo {
    metadata: MetadataId::new(FourCc::new(*b"RMF ")),
    short_name: "real",
    long_name: "RealMedia",
};

/// The signature of a RealAudio (`.ra`) file, without chunks.
const RA_SIGNATURE: [u8; 4] = *b".ra\xfd";

/// The largest chunk of the header that is read into memory.
const MAX_CHUNK_LEN: u64 = 1 << 22;

/// The largest number of entries of an index that is read, if the length of the file is not known.
const MAX_INDEX_ENTRIES: u32 = 1 << 22;

/// The minimum spacing in milliseconds of the entries of the seek index that is built from the
/// packets that are read, for streams in which all packets are points to start decoding at.
const INDEX_SPACING_MS: i64 = 500;

/// The time stamps of the container are in milliseconds.
const TIME_BASE: TimeBase = TimeBase::new(NonZero::<u32>::MIN, MILLIS);

const MILLIS: NonZero<u32> = match NonZero::new(1000) {
    Some(n) => n,
    None => panic!("1000 is non-zero"),
};

/// The properties of a headerless `.ra` file.
struct OldFormat {
    /// The number of bytes that make up a packet.
    read_len: usize,
    /// The number of bytes that make up a group of packets that a seek can start at.
    group_len: u64,
    /// True if the next packet is the first after the start of the data or a seek: a key frame.
    first: bool,
}

/// The header of a packet of the container.
struct PacketHeader {
    /// The position of the header.
    pos: u64,
    /// The length of the data.
    len: usize,
    /// The ID of the stream of the packet.
    stream: u32,
    /// The time stamp in milliseconds.
    ts: u32,
    flags: u8,
}

/// An `MDPR` chunk.
struct StreamHeader {
    id: u32,
    start_time: u32,
    duration: u32,
    audio: AudioInfo,
}

/// The data of the `PROP` chunk that is used.
struct FileProperties {
    duration: u32,
    index_pos: u64,
}

/// RealMedia (`.rm`, `.rmvb`, `.ra`) format reader.
///
/// `RealReader` implements an audio-only demuxer for RealMedia. See the crate documentation for
/// the supported audio and the timeline.
pub struct RealReader<'s> {
    reader: MediaSourceStream<'s>,
    tracks: Vec<Track>,
    media_info: MediaInfo,
    metadata: MetadataLog,
    /// The audio streams, in the order of the tracks.
    streams: Vec<AudioStream>,
    /// The packets that have been assembled.
    ready: VecDeque<Packet>,
    /// The payload of the packet being read.
    payload: Vec<u8>,
    /// The position of the first packet.
    data_start: u64,
    old: Option<OldFormat>,
    eof: bool,
    /// The time stamp of the last packet returned.
    last_ts: Option<i64>,
}

impl Scoreable for RealReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        if src.read_quad_bytes()? == RA_SIGNATURE {
            // The version of the header.
            let version = src.read_be_u16()?;
            return Ok(if (3..=5).contains(&version) {
                Score::Supported(255)
            }
            else {
                Score::Unsupported
            });
        }

        // The signature of a file with chunks is followed by the size of the file header, which
        // is smaller than 64 KiB.
        Ok(if src.read_be_u16()? == 0 { Score::Supported(255) } else { Score::Unsupported })
    }
}

impl ProbeableFormat<'_> for RealReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(RealReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            REAL_FORMAT_INFO,
            &["rm", "rmvb", "ra", "rma"],
            &[
                "audio/x-pn-realaudio",
                "audio/vnd.rn-realaudio",
                "audio/x-realaudio",
                "application/vnd.rn-realmedia"
            ],
            &[b".RMF\0\0", b".RMP\0\0", b".ra\xfd"]
        )]
    }
}

/// Read from `src` until `buf` is full or the end of the stream. Returns the length read.
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

/// Returns true if `err` is the end of the stream.
fn is_eof(err: &std::io::Error) -> bool {
    err.kind() == ErrorKind::UnexpectedEof
}

/// Map a number of channels to a channel layout.
fn channels_of(count: u32) -> Option<Channels> {
    match count {
        0 => None,
        1 => Some(CHANNEL_LAYOUT_MONO),
        2 => Some(CHANNEL_LAYOUT_STEREO),
        n => match Position::from_count(n) {
            Some(pos) if n <= 18 => Some(Channels::Positioned(pos)),
            _ => u16::try_from(n).ok().map(Channels::Discrete),
        },
    }
}

/// The text of a string of the metadata. The text is UTF-8 if it is valid, otherwise Latin-1.
fn text_value(bytes: &[u8]) -> String {
    let bytes = &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())];

    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
    }
}

/// Make the tag of a string of the header.
fn make_tag(key: &str, value: &[u8]) -> Tag {
    let value = text_value(value);

    let std = match key {
        "title" => Some(StandardTag::TrackTitle(Arc::new(value.clone()))),
        "author" => Some(StandardTag::Artist(Arc::new(value.clone()))),
        "copyright" => Some(StandardTag::Copyright(Arc::new(value.clone()))),
        "comment" => Some(StandardTag::Comment(Arc::new(value.clone()))),
        _ => None,
    };

    Tag::new_from_parts(key, value, std)
}

/// Parse the data of a stream. Returns the audio, or `None` if the stream is not audio.
fn parse_stream_data(
    data: &[u8],
    bit_rate: u32,
    mime: &[u8],
    fileinfo: &mut Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<Option<AudioInfo>> {
    let Some(sig) = data.first_chunk::<4>()
    else {
        return Ok(None);
    };

    if sig == &RA_SIGNATURE {
        let mut src = BufReader::new(&data[4..]);
        return header::read_audio_header(&mut src, bit_rate, false).map(Some);
    }

    if sig == b"LSD:" {
        let mut audio = header::read_ralf_header(data);
        audio.bit_rate = bit_rate;
        return Ok(Some(audio));
    }

    if mime == b"logical-fileinfo" {
        // The file properties (not a stream).
        let mut src = BufReader::new(&data[4..]);
        read_fileinfo(&mut src, fileinfo)?;
    }

    // Video, and other data.
    Ok(None)
}

/// Read the properties of the `logical-fileinfo` data of a stream.
fn read_fileinfo<B: ReadBytes>(src: &mut B, props: &mut Vec<(Vec<u8>, Vec<u8>)>) -> Result<()> {
    if src.read_be_u16()? != 0 {
        warn!("real: unsupported file info version");
        return Ok(());
    }

    let stream_count = u64::from(src.read_be_u16()?);
    src.ignore_bytes(6 * stream_count)?;
    let rule_count = u64::from(src.read_be_u16()?);
    src.ignore_bytes(2 * rule_count)?;

    for _ in 0..src.read_be_u16()? {
        src.read_be_u32()?;

        if src.read_be_u16()? != 0 {
            warn!("real: unsupported name value property version");
            return Ok(());
        }

        let name_len = usize::from(src.read_u8()?);
        let name = src.read_boxed_slice_exact(name_len)?;
        let ty = src.read_be_u32()?;
        let len = usize::from(src.read_be_u16()?);

        // Only the strings (type 2) are used.
        if ty == 2 {
            props.push((name.into_vec(), src.read_boxed_slice_exact(len)?.into_vec()));
        }
        else {
            src.ignore_bytes(len as u64)?;
        }
    }

    Ok(())
}

/// Parse an `MDPR` chunk. Returns the streams (more than one for a multi-rate stream), which may
/// not be audio, indicated by `None`.
fn parse_mdpr(
    body: &[u8],
    fileinfo: &mut Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<Vec<(StreamHeaderMeta, Option<AudioInfo>)>> {
    let mut src = BufReader::new(body);

    let id = u32::from(src.read_be_u16()?);
    src.read_be_u32()?; // Maximum bit rate.
    let bit_rate = src.read_be_u32()?;
    src.read_be_u32()?; // Maximum packet size.
    src.read_be_u32()?; // Average packet size.
    let start_time = src.read_be_u32()?;
    src.read_be_u32()?; // Preroll.
    let duration = src.read_be_u32()?;

    let desc_len = usize::from(src.read_u8()?);
    src.ignore_bytes(desc_len as u64)?;
    let mime_len = usize::from(src.read_u8()?);
    let mime = src.read_boxed_slice_exact(mime_len)?;

    let size = src.read_be_u32()? as usize;
    let data = src.read_buf_bytes_available_ref();

    // The data is limited to the chunk.
    let data = &data[..size.min(data.len())];

    let meta = StreamHeaderMeta { id, start_time, duration };

    let mut out = Vec::new();

    if data.first_chunk::<4>() == Some(b"MLTI") {
        // A multi-rate stream: a list of streams for the same ID, that are told apart by the
        // packet group of the packets.
        let mut src = BufReader::new(&data[4..]);

        for _ in 0..src.read_be_u16()? {
            src.read_be_u16()?;
        }

        let count = src.read_be_u16()?;

        for i in 0..count {
            let len = src.read_be_u32()? as usize;
            let part = src.read_buf_bytes_available_ref();
            let part = &part[..len.min(part.len())];

            // Skip the part that was taken.
            src.ignore_bytes(part.len() as u64)?;

            let audio = parse_stream_data(part, bit_rate, &mime, fileinfo)?;
            out.push((StreamHeaderMeta { id: id + (u32::from(i) << 16), ..meta }, audio));
        }
    }
    else {
        out.push((meta, parse_stream_data(data, bit_rate, &mime, fileinfo)?));
    }

    Ok(out)
}

/// The properties of an `MDPR` chunk that are not specific to the stream type.
#[derive(Copy, Clone)]
struct StreamHeaderMeta {
    id: u32,
    start_time: u32,
    duration: u32,
}

impl<'s> RealReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let tag = mss.read_quad_bytes()?;

        let metadata = opts.external_data.metadata.unwrap_or_default();

        if tag == RA_SIGNATURE {
            Self::read_old_header(mss, metadata)
        }
        else if &tag == b".RMF" || &tag == b".RMP" {
            Self::read_header(mss, metadata)
        }
        else {
            unsupported_error("real: missing RealMedia signature")
        }
    }

    /// Read the header of a file of chunks, which follows the signature.
    fn read_header(mut mss: MediaSourceStream<'s>, mut metadata: MetadataLog) -> Result<Self> {
        // The file header.
        let size = mss.read_be_u32()?;

        if size < 8 {
            return decode_error("real: invalid file header size");
        }

        mss.ignore_bytes(u64::from(size) - 8)?;

        let mut props = None;
        let mut text: Vec<(&'static str, Vec<u8>)> = Vec::new();
        let mut fileinfo = Vec::new();
        let mut headers: Vec<StreamHeader> = Vec::new();
        let ver;

        loop {
            let tag = mss.read_quad_bytes()?;
            let size = mss.read_be_u32()?;
            let chunk_ver = mss.read_be_u16()?;

            if (size < 10 && &tag != b"DATA") || (chunk_ver != 0 && chunk_ver != 2) {
                return decode_error("real: invalid chunk header");
            }

            let body_len = u64::from(size.saturating_sub(10));

            if &tag == b"DATA" {
                ver = chunk_ver;
                break;
            }

            if !matches!(&tag, b"PROP" | b"CONT" | b"MDPR") {
                mss.ignore_bytes(body_len)?;
                continue;
            }

            if body_len > MAX_CHUNK_LEN {
                warn!("real: chunk is too large");
                mss.ignore_bytes(body_len)?;
                continue;
            }

            let body = mss.read_boxed_slice_exact(body_len as usize)?;

            match &tag {
                b"PROP" => props = Self::parse_prop(&body, chunk_ver).ok(),
                b"CONT" => {
                    let mut src = BufReader::new(&body);
                    text = header::read_text(&mut src, true)?;
                }
                _ => match parse_mdpr(&body, &mut fileinfo) {
                    Ok(streams) => {
                        for (meta, audio) in streams {
                            if let Some(audio) = audio {
                                headers.push(StreamHeader {
                                    id: meta.id,
                                    start_time: meta.start_time,
                                    duration: meta.duration,
                                    audio,
                                });
                            }
                        }
                    }
                    Err(err) => warn!("real: skipping stream: {err}"),
                },
            }
        }

        // The data header.
        mss.read_be_u32()?; // The number of packets.

        if ver == 2 {
            mss.ignore_bytes(12)?;
        }

        mss.read_be_u32()?; // The next data header.

        let data_start = mss.pos();

        let mut streams = Vec::new();
        let mut tracks = Vec::new();

        for header in headers {
            if let Some((stream, track)) = Self::make_track(header, props.as_ref()) {
                if tracks.iter().any(|t: &Track| t.id == track.id) {
                    warn!("real: duplicate stream {}", track.id);
                    continue;
                }

                streams.push(stream);
                tracks.push(track);
            }
        }

        if tracks.is_empty() {
            return unsupported_error("real: no supported audio stream");
        }

        tracks[0].with_flags(TrackFlags::DEFAULT);

        // Read the index of the packets.
        if let Some(index_pos) = props.as_ref().map(|p| p.index_pos).filter(|&p| p != 0)
            && mss.is_seekable()
        {
            match Self::read_index(&mut mss, index_pos, &mut streams) {
                Ok(()) => info!("real: read the index"),
                Err(err) => debug!("real: ignoring the index: {err}"),
            }

            mss.seek(SeekFrom::Start(data_start))?;
        }

        for stream in &mut streams {
            Self::finish_index(stream);
        }

        // The metadata.
        let mut builder = MetadataBuilder::new(REAL_METADATA_INFO);
        let mut n_tags = 0;

        for (key, value) in &text {
            builder.add_tag(make_tag(key, value));
            n_tags += 1;
        }

        for (key, value) in &fileinfo {
            builder.add_tag(make_tag(&text_value(key), value));
            n_tags += 1;
        }

        if n_tags > 0 {
            metadata.push(builder.build());
        }

        Ok(RealReader {
            reader: mss,
            media_info: MediaInfo::from_tracks(&tracks),
            tracks,
            metadata,
            streams,
            ready: VecDeque::new(),
            payload: Vec::new(),
            data_start,
            old: None,
            eof: false,
            last_ts: None,
        })
    }

    /// Parse the `PROP` chunk.
    fn parse_prop(body: &[u8], ver: u16) -> Result<FileProperties> {
        let mut src = BufReader::new(body);

        // The bit rates, packet sizes, and the number of packets.
        src.ignore_bytes(20)?;
        let duration = src.read_be_u32()?;
        src.read_be_u32()?; // Preroll.
        let index_pos = if ver == 0 { u64::from(src.read_be_u32()?) } else { src.read_be_u64()? };

        Ok(FileProperties { duration, index_pos })
    }

    /// Make the stream and track of the audio of a stream header.
    fn make_track(
        header: StreamHeader,
        props: Option<&FileProperties>,
    ) -> Option<(AudioStream, Track)> {
        let StreamHeader { id, start_time, duration, audio } = header;

        let Some(codec) = audio.codec
        else {
            debug!("real: skipping stream {id} with an unknown codec");
            return None;
        };

        let mut params = AudioCodecParameters::new();
        params.for_codec(codec);

        if audio.sample_rate != 0 {
            params.with_sample_rate(audio.sample_rate);
        }

        if let Some(channels) = channels_of(audio.channels) {
            params.with_channels(channels);
        }

        if let Some(extra) = &audio.extra {
            params.with_extra_data(extra.clone());
        }

        let mut track = Track::new(id);

        track
            .with_time_base(TIME_BASE)
            .with_codec_params(CodecParameters::Audio(params))
            .with_start_ts(Timestamp::new(i64::from(start_time)));

        // The duration of the stream, or of the file if the stream does not have one.
        let duration = if duration > 0 { Some(duration) } else { props.map(|p| p.duration) };

        if let Some(duration) = duration.filter(|&d| d > 0) {
            track.with_duration(symphonia_core::units::Duration::new(u64::from(duration)));
        }

        Some((AudioStream::new(id, audio), track))
    }

    /// Read the chain of `INDX` chunks that begins at `pos`, and add the entries to the streams.
    fn read_index(
        mss: &mut MediaSourceStream<'s>,
        pos: u64,
        streams: &mut [AudioStream],
    ) -> Result<()> {
        mss.seek(SeekFrom::Start(pos))?;

        let total = mss.byte_len();

        loop {
            if mss.read_quad_bytes()? != *b"INDX" {
                return decode_error("real: missing index chunk");
            }

            let size = mss.read_be_u32()?;

            if size < 20 {
                return decode_error("real: invalid index size");
            }

            let ver = mss.read_be_u16()?;

            if ver != 0 && ver != 2 {
                return decode_error("real: invalid index version");
            }

            let n_pkts = mss.read_be_u32()?;
            let stream_id = u32::from(mss.read_be_u16()?);
            let next = u64::from(mss.read_be_u32()?);

            if ver == 2 {
                mss.ignore_bytes(4)?;
            }

            let stream = streams.iter_mut().find(|s| s.track_id == stream_id);

            let fits = match total {
                Some(total) => total.saturating_sub(mss.pos()) / 14 >= u64::from(n_pkts),
                None => n_pkts <= MAX_INDEX_ENTRIES,
            };

            match stream {
                Some(stream) if fits => {
                    stream.index.reserve(n_pkts as usize);

                    for _ in 0..n_pkts {
                        mss.ignore_bytes(2)?;
                        let ts = mss.read_be_u32()?;
                        let pos = if ver == 0 {
                            u64::from(mss.read_be_u32()?)
                        }
                        else {
                            mss.read_be_u64()?
                        };
                        mss.ignore_bytes(4)?;

                        stream.index.push((i64::from(ts), pos));
                    }
                }
                _ => debug!("real: skipping the index of stream {stream_id}"),
            }

            if next == 0 {
                return Ok(());
            }

            if mss.pos() < next {
                mss.seek(SeekFrom::Start(next))?;
            }
        }
    }

    /// Order the entries of the index of the stream by position, with increasing time.
    fn finish_index(stream: &mut AudioStream) {
        stream.index.sort_by_key(|&(_, pos)| pos);
        stream.index.dedup_by_key(|&mut (_, pos)| pos);

        let mut last = i64::MIN;

        stream.index.retain(|&(ts, _)| {
            let keep = ts >= last;
            last = last.max(ts);
            keep
        });
    }

    /// Read the header of a `.ra` file, which follows the signature.
    fn read_old_header(mut mss: MediaSourceStream<'s>, metadata: MetadataLog) -> Result<Self> {
        let audio = header::read_audio_header(&mut mss, 0, true)?;
        let data_start = mss.pos();

        // The length of the packets of FFmpeg's reader.
        let (w, h) = (audio.frame_size, audio.sub_packet_h);

        let read_len = match audio.interleave {
            Interleave::Genr | Interleave::Sipr => w,
            Interleave::Int4 => h / 2 * audio.coded_frame_size,
            Interleave::None if w == 0 => RAW_PACKET_LEN,
            Interleave::None => audio.coded_frame_size * h / 2,
            Interleave::Vbr => return unsupported_error("real: variable bit rate in a .ra file"),
        };

        if read_len == 0 {
            return decode_error("real: invalid packet size");
        }

        let group_len = if audio.is_grouped() { (read_len * h) as u64 } else { read_len as u64 };

        let text = audio.text.clone();

        // The time stamps are not in the file: the time of a packet is found from the bit rate.
        let mut track_duration = None;

        if let (Some(len), true) = (mss.byte_len(), audio.bit_rate > 0) {
            let bits = len.saturating_sub(data_start) * 8;
            track_duration = Some(bits * 1000 / u64::from(audio.bit_rate));
        }

        let header = StreamHeader { id: 0, start_time: 0, duration: 0, audio };

        let Some((stream, mut track)) = Self::make_track(header, None)
        else {
            return unsupported_error("real: unsupported audio codec");
        };

        if let Some(duration) = track_duration {
            track.with_duration(symphonia_core::units::Duration::new(duration));
        }

        track.with_flags(TrackFlags::DEFAULT);

        let mut metadata = metadata;
        let mut builder = MetadataBuilder::new(REAL_METADATA_INFO);

        for (key, value) in &text {
            builder.add_tag(make_tag(key, value));
        }

        if !text.is_empty() {
            metadata.push(builder.build());
        }

        Ok(RealReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            metadata,
            streams: vec![stream],
            ready: VecDeque::new(),
            payload: Vec::new(),
            data_start,
            old: Some(OldFormat { read_len, group_len, first: true }),
            eof: false,
            last_ts: None,
        })
    }

    /// Find the next packet of the container. Returns `None` at the end of the stream.
    ///
    /// The packets are found as FFmpeg's `rm_sync` finds them: a packet header is recognised
    /// by its version (0) and length, and bytes are skipped until one is found. This also skips
    /// the index chunks that follow the data.
    fn read_header_next(&mut self) -> Result<Option<PacketHeader>> {
        let mut state = 0xffff_ffffu32;

        loop {
            let byte = match self.reader.read_u8() {
                Ok(b) => b,
                Err(e) if is_eof(&e) => return Ok(None),
                Err(e) => return Err(e.into()),
            };

            state = (state << 8) | u32::from(byte);

            if state == u32::from_be_bytes(*b"INDX") {
                let (len, ver, n_pkts) = match (|| -> std::io::Result<_> {
                    Ok((
                        self.reader.read_be_u32()?,
                        self.reader.read_be_u16()?,
                        self.reader.read_be_u32()?,
                    ))
                })() {
                    Ok(v) => v,
                    Err(e) if is_eof(&e) => return Ok(None),
                    Err(e) => return Err(e.into()),
                };

                if ver != 0 && ver != 2 {
                    return decode_error("real: invalid index version");
                }

                let expected = if ver == 0 {
                    20 + 14 * u64::from(n_pkts)
                }
                else {
                    24 + 18 * u64::from(n_pkts)
                };

                // Some files do not include the entries in the size of the chunk.
                let len = if len == 20 && expected <= i32::MAX as u64 {
                    expected
                }
                else {
                    if u64::from(len) != expected {
                        warn!("real: index size {len} ({n_pkts} packets) is wrong");
                    }

                    u64::from(len)
                };

                if len >= 14 {
                    // The start of the chunk has been read.
                    match self.reader.ignore_bytes(len - 14) {
                        Err(e) if is_eof(&e) => return Ok(None),
                        Err(e) => return Err(e.into()),
                        Ok(()) => (),
                    }
                }

                state = 0xffff_ffff;
                continue;
            }

            // The version is 0, and the length includes the header.
            if state > 0xffff || state <= 12 {
                continue;
            }

            let pos = self.reader.pos() - 4;
            let len = (state - 12) as usize;

            let mut hdr = [0; 8];

            match self.reader.read_buf_exact(&mut hdr) {
                Ok(()) => (),
                Err(e) if is_eof(&e) => return Ok(None),
                Err(e) => return Err(e.into()),
            }

            let num = u32::from(u16::from_be_bytes([hdr[0], hdr[1]]));
            let ts = u32::from_be_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]);

            // The packet group, of a multi-rate stream.
            let group = u32::from((hdr[6] >> 1).saturating_sub(1));

            return Ok(Some(PacketHeader {
                pos,
                len,
                stream: (group << 16) + num,
                ts,
                flags: hdr[7],
            }));
        }
    }

    /// Release the packets that the streams hold back at the end of the data.
    fn finish(&mut self) {
        self.eof = true;

        for stream in &mut self.streams {
            stream.flush(&mut self.ready);
        }
    }

    /// Read a packet of the container, and assemble the packets of the streams from it.
    fn read_container_packet(&mut self) -> Result<()> {
        if self.old.is_some() {
            return self.read_old_packet();
        }

        let Some(hdr) = self.read_header_next()?
        else {
            self.finish();
            return Ok(());
        };

        let Some(si) = self.streams.iter().position(|s| s.track_id == hdr.stream)
        else {
            // A stream that is not audio.
            return match self.reader.ignore_bytes(hdr.len as u64) {
                Err(e) if is_eof(&e) => {
                    self.finish();
                    Ok(())
                }
                Err(e) => Err(e.into()),
                Ok(()) => Ok(()),
            };
        };

        let mut payload = std::mem::take(&mut self.payload);

        payload.resize(hdr.len, 0);

        // A file that is cut short ends in a partial packet: like the reference demuxer, use
        // what is there.
        let len = read_up_to(&mut self.reader, &mut payload)?;
        let truncated = len < payload.len();

        if len == 0 {
            self.payload = payload;
            self.finish();
            return Ok(());
        }

        payload.truncate(len);

        let stream = &mut self.streams[si];
        let ts = i64::from(hdr.ts);

        if stream.is_sync_point(hdr.flags) {
            // Remember where decoding can start at.
            let spaced = match stream.index.last() {
                None => true,
                Some(&(last_ts, last_pos)) => {
                    hdr.pos > last_pos
                        && (stream.info.is_grouped() || ts >= last_ts + INDEX_SPACING_MS)
                }
            };

            if spaced {
                stream.index.push((ts, hdr.pos));
            }
        }

        stream.process(&payload, Some(ts), hdr.flags, &mut self.ready);

        self.payload = payload;

        if truncated {
            self.finish();
        }

        Ok(())
    }

    /// Read a packet of a `.ra` file.
    fn read_old_packet(&mut self) -> Result<()> {
        let Some(old) = self.old.as_mut()
        else {
            return Ok(());
        };

        let mut payload = std::mem::take(&mut self.payload);

        payload.resize(old.read_len, 0);

        let len = read_up_to(&mut self.reader, &mut payload)?;

        let flags = if std::mem::take(&mut old.first) { FLAG_KEY } else { 0 };

        let stream = &mut self.streams[0];

        if len == 0 || (len < payload.len() && stream.info.is_grouped()) {
            // A group of frames that is not complete cannot be used.
            self.payload = payload;
            self.finish();
            return Ok(());
        }

        payload.truncate(len);

        stream.process(&payload, None, flags, &mut self.ready);

        self.payload = payload;

        if len < old.read_len {
            self.finish();
        }

        Ok(())
    }

    fn next_packet_inner(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(packet) = self.ready.pop_front() {
                self.last_ts = Some(packet.pts.get());
                return Ok(Some(packet));
            }

            if self.eof {
                return Ok(None);
            }

            self.read_container_packet()?;
        }
    }

    /// Move to `pos` and discard everything that has been read.
    fn reposition(&mut self, pos: u64) -> Result<()> {
        self.reader.seek(SeekFrom::Start(pos))?;
        self.ready.clear();
        self.eof = false;

        for stream in &mut self.streams {
            stream.reset();
        }

        if let Some(old) = self.old.as_mut() {
            old.first = true;
        }

        Ok(())
    }

    /// Move to the point that decoding of the stream `si` can start at, before `required`.
    fn seek_to_sync_point(&mut self, si: usize, required: i64) -> Result<()> {
        let track_id = self.streams[si].track_id;

        if let Some(old) = &self.old {
            // There are no time stamps: the position is found from the bit rate.
            let bit_rate = u64::from(self.streams[si].info.bit_rate);

            if bit_rate == 0 {
                return seek_error(SeekErrorKind::Unseekable);
            }

            let bytes = required.max(0) as u64 * bit_rate / 8000;
            let pos = bytes / old.group_len * old.group_len;

            if self.reader.byte_len().is_some_and(|len| self.data_start + pos >= len) {
                return seek_error(SeekErrorKind::OutOfRange);
            }

            self.reposition(self.data_start + pos)?;

            let ms = (pos * 8000 / bit_rate) as i64;
            self.streams[si].clock_mut().anchor(ms);

            return Ok(());
        }

        // Start from the last known point before the time, and find the last point that is
        // before the time by reading the headers of the packets.
        let index = &self.streams[si].index;
        let k = index.partition_point(|&(ts, _)| ts <= required);
        let start = k.checked_sub(1).map_or(self.data_start, |i| index[i].1);

        self.reposition(start)?;

        let mut best = start;

        while let Some(hdr) = self.read_header_next()? {
            let stream = &mut self.streams[si];

            if hdr.stream == track_id && stream.is_sync_point(hdr.flags) {
                let ts = i64::from(hdr.ts);

                if ts > required {
                    break;
                }

                best = hdr.pos;

                if stream.index.last().is_none_or(|&(last_ts, last_pos)| {
                    hdr.pos > last_pos && ts >= last_ts + INDEX_SPACING_MS
                }) {
                    stream.index.push((ts, hdr.pos));
                }
            }

            match self.reader.ignore_bytes(hdr.len as u64) {
                Err(e) if is_eof(&e) => break,
                Err(e) => return Err(e.into()),
                Ok(()) => (),
            }
        }

        self.reposition(best)
    }

    fn seek_inner(&mut self, to: SeekTo) -> Result<SeekedTo> {
        let (si, required_ts) = match to {
            SeekTo::Timestamp { ts, track_id } => {
                let si = self.streams.iter().position(|s| s.track_id == track_id);
                (si.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?, ts)
            }
            SeekTo::Time { time, track_id } => {
                let si = match track_id {
                    Some(id) => self.streams.iter().position(|s| s.track_id == id),
                    None => Some(0),
                };

                let ts = TIME_BASE
                    .calc_timestamp(time)
                    .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

                (si.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?, ts)
            }
        };

        let required = required_ts.get();

        if required < 0 {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let track_id = self.streams[si].track_id;

        if let Some(dur) = self.tracks[si].duration {
            if required > dur.get() as i64 {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        debug!("real: seeking track {track_id} to ts={required}");

        if self.reader.is_seekable() {
            self.seek_to_sync_point(si, required)?;
        }
        else if self.last_ts.is_some_and(|last| required < last) {
            return seek_error(SeekErrorKind::ForwardOnly);
        }

        // Scan forward to the first packet that ends after the time.
        let mut skipped = Vec::new();

        let packet = loop {
            match self.next_packet_inner()? {
                Some(p) if p.track_id != track_id => skipped.push(p),
                Some(p) if p.pts.get().saturating_add(p.dur.get() as i64) > required => break p,
                Some(_) => (),
                None => return seek_error(SeekErrorKind::OutOfRange),
            }
        };

        let actual_ts = packet.pts;

        // Packets of the other streams that have been read are given back, after the packet.
        for p in skipped.into_iter().rev() {
            self.ready.push_front(p);
        }

        self.ready.push_front(packet);

        Ok(SeekedTo { track_id, required_ts, actual_ts })
    }
}

impl FormatReader for RealReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &REAL_FORMAT_INFO
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
        self.next_packet_inner()
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
