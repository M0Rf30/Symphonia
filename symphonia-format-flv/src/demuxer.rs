// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::io::{ErrorKind, Seek, SeekFrom};

use symphonia_common::mpeg::es::{EsParser, EsTrack, MpaEs};
use symphonia_common::mpeg::pes::{PTS_MODULUS, unwrap_pts};
use symphonia_common::mpeg::seek::bisect;
use symphonia_common::mpeg::timeline::ticks_to_clock;
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_PCM_ALAW, CODEC_ID_PCM_MULAW, CODEC_ID_PCM_S16BE, CODEC_ID_PCM_S16LE, CODEC_ID_PCM_U8,
};
use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error, unsupported_error};
use symphonia_core::formats::TrackFlags;
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_FLV;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, TimeBase};

use log::{debug, info, warn};

use crate::amf::{Amf, parse_values};
use crate::es::{PcmEs, RawAacEs};

const FLV_SIGNATURE: [u8; 4] = *b"FLV\x01";

/// The length of the FLV header.
const FLV_HEADER_LEN: usize = 9;

const TAG_AUDIO: u8 = 8;
const TAG_VIDEO: u8 = 9;
const TAG_SCRIPT: u8 = 18;

/// The length of the header of a tag.
const TAG_HEADER_LEN: usize = 11;

/// The maximum size of a script tag that is parsed.
const MAX_SCRIPT_LEN: usize = 4 * 1024 * 1024;

/// The maximum number of bytes to scan for the first audio tags while opening a stream.
const MAX_DISCOVERY_LEN: u64 = 8 * 1024 * 1024;

/// The range, in bytes, below which the bisection stops, and the stream is scanned forward.
const BISECT_MIN_GAP: u64 = 64 * 1024;

/// The number of bytes to scan for an audio tag when probing during a seek.
const PROBE_SCAN_LEN: u64 = 512 * 1024;

/// The maximum number of tags to examine looking backwards from the end of the stream for the last
/// audio tag.
const MAX_TAIL_TAGS: usize = 512;

// The sound formats of audio tags.
const SOUND_PCM_PLATFORM: u8 = 0;
const SOUND_ADPCM: u8 = 1;
const SOUND_MP3: u8 = 2;
const SOUND_PCM_LE: u8 = 3;
const SOUND_NELLYMOSER_16K: u8 = 4;
const SOUND_NELLYMOSER_8K: u8 = 5;
const SOUND_NELLYMOSER: u8 = 6;
const SOUND_ALAW: u8 = 7;
const SOUND_MULAW: u8 = 8;
const SOUND_EX_HEADER: u8 = 9;
const SOUND_AAC: u8 = 10;
const SOUND_SPEEX: u8 = 11;
const SOUND_MP3_8K: u8 = 14;

const FLV_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_FLV, short_name: "flv", long_name: "Flash Video" };

/// The header of a tag.
#[derive(Copy, Clone, Debug)]
struct TagHeader {
    /// The position of the tag header.
    pos: u64,
    ty: u8,
    /// The length of the data of the tag.
    size: usize,
    /// The time stamp in milliseconds.
    ts: u32,
}

impl TagHeader {
    fn parse(b: &[u8], pos: u64) -> Option<TagHeader> {
        let b = b.get(..TAG_HEADER_LEN)?;

        // The type of a tag is the lower 5 bits of the first byte (the upper bits are for filters
        // and reserved), and the stream ID is always 0.
        let ty = b[0];

        if !matches!(ty, TAG_AUDIO | TAG_VIDEO | TAG_SCRIPT) || b[8..11] != [0, 0, 0] {
            return None;
        }

        Some(TagHeader {
            pos,
            ty,
            size: usize::from(b[1]) << 16 | usize::from(b[2]) << 8 | usize::from(b[3]),
            ts: u32::from(b[7]) << 24
                | u32::from(b[4]) << 16
                | u32::from(b[5]) << 8
                | u32::from(b[6]),
        })
    }

    /// The position of the first byte after the tag and its trailing size.
    fn end(&self) -> u64 {
        self.pos + (TAG_HEADER_LEN + self.size + 4) as u64
    }
}

/// Read a tag header. Returns `None` at the end of the stream, or if the data is not a tag.
fn read_tag_header<B: ReadBytes>(reader: &mut B) -> Result<Option<TagHeader>> {
    let pos = reader.pos();
    let mut b = [0u8; TAG_HEADER_LEN];

    match reader.read_buf_exact(&mut b) {
        Ok(()) => (),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    Ok(TagHeader::parse(&b, pos))
}

/// The raw PTS of a tag time stamp in milliseconds.
fn ts_to_pts(ts: u32) -> u64 {
    (u64::from(ts) * 90) % PTS_MODULUS
}

/// The properties of the audio of an audio tag header.
struct AudioFlags {
    format: u8,
    /// The sample rate from the header.
    rate: u32,
    is_16_bit: bool,
    is_stereo: bool,
}

impl AudioFlags {
    fn parse(b: u8) -> AudioFlags {
        AudioFlags {
            format: b >> 4,
            rate: [5512, 11025, 22050, 44100][usize::from((b >> 2) & 3)],
            is_16_bit: b & 2 != 0,
            is_stereo: b & 1 != 0,
        }
    }

    fn channels(&self) -> Channels {
        if self.is_stereo {
            Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
        }
        else {
            Channels::Positioned(Position::FRONT_LEFT)
        }
    }
}

/// The name of an unsupported sound format.
fn unsupported_format_name(format: u8) -> &'static str {
    match format {
        SOUND_ADPCM => "swf adpcm",
        SOUND_NELLYMOSER_16K | SOUND_NELLYMOSER_8K | SOUND_NELLYMOSER => "nellymoser",
        SOUND_EX_HEADER => "enhanced rtmp audio",
        SOUND_SPEEX => "speex",
        _ => "unknown",
    }
}

/// Get the frame data of an audio tag, if the tag contains frames of the audio of `format`.
fn frame_data(data: &[u8], format: u8) -> Option<&[u8]> {
    if data.first()? >> 4 != format {
        return None;
    }

    match format {
        // AAC packet type 1 is a raw data block.
        SOUND_AAC if data.get(1) == Some(&1) => data.get(2..),
        SOUND_AAC => None,
        _ => data.get(1..),
    }
}

/// Create the elementary stream for the audio of the audio tag with data `data`. Returns `None` if
/// the audio cannot be configured by this tag (an AAC tag that is not the sequence header).
fn make_es(data: &[u8]) -> Result<Option<EsTrack>> {
    let Some(&flags) = data.first()
    else {
        return Ok(None);
    };

    let flags = AudioFlags::parse(flags);

    let parser: Box<dyn EsParser> = match flags.format {
        SOUND_MP3 | SOUND_MP3_8K => Box::new(MpaEs::new()),
        SOUND_AAC => match (data.get(1), data.get(2..)) {
            // The sequence header. Some muxers write an empty one before the real one.
            (Some(0), Some(asc)) if asc.len() >= 2 => Box::new(RawAacEs::new(asc)?),
            _ => return Ok(None),
        },
        SOUND_PCM_PLATFORM | SOUND_PCM_LE => {
            let codec = match (flags.format, flags.is_16_bit) {
                (_, false) => CODEC_ID_PCM_U8,
                // The platform endian format is taken to be big-endian, as other demuxers do.
                (SOUND_PCM_PLATFORM, true) => CODEC_ID_PCM_S16BE,
                (_, true) => CODEC_ID_PCM_S16LE,
            };

            let mut params = AudioCodecParameters::new();
            params
                .for_codec(codec)
                .with_sample_rate(flags.rate)
                .with_channels(flags.channels())
                .with_bits_per_sample(if flags.is_16_bit { 16 } else { 8 });

            let frame_bytes = flags.channels().count() * if flags.is_16_bit { 2 } else { 1 };

            Box::new(PcmEs::new(params, frame_bytes))
        }
        SOUND_ALAW | SOUND_MULAW => {
            let mut params = AudioCodecParameters::new();
            params
                .for_codec(if flags.format == SOUND_ALAW {
                    CODEC_ID_PCM_ALAW
                }
                else {
                    CODEC_ID_PCM_MULAW
                })
                .with_sample_rate(8000)
                .with_channels(flags.channels())
                .with_bits_per_sample(8);

            Box::new(PcmEs::new(params, flags.channels().count()))
        }
        format => {
            let name = unsupported_format_name(format);
            warn!("flv: unsupported audio format {format} ({name})");
            return unsupported_error("flv: unsupported audio codec");
        }
    };

    Ok(Some(EsTrack::new(0, parser)))
}

/// The result of scanning the start of a stream.
struct Discovered {
    es: EsTrack,
    format: u8,
    /// The position and time stamp of the first audio tag.
    first_audio: (u64, u32),
    meta: Option<Amf>,
}

/// Scan the tags at the start of the stream for the audio format, and the metadata.
fn discover<B: ReadBytes>(reader: &mut B) -> Result<Discovered> {
    let start = reader.pos();

    let mut es: Option<EsTrack> = None;
    let mut format = 0;
    let mut first_audio = None;
    let mut meta = None;
    let mut meta_checked = false;
    let mut tags = 0;

    while reader.pos() - start < MAX_DISCOVERY_LEN {
        let Some(tag) = read_tag_header(reader)?
        else {
            break;
        };

        tags += 1;

        let eof = match tag.ty {
            TAG_AUDIO => {
                let mut data = vec![0u8; tag.size];

                if reader.read_buf_exact(&mut data).is_err() {
                    break;
                }

                if es.is_none() {
                    // The format of the audio is that of the first audio tag.
                    es = make_es(&data)?;

                    if es.is_some() {
                        format = data[0] >> 4;
                        first_audio = Some((tag.pos, tag.ts));
                    }
                }

                if let (Some(es), Some(_)) = (es.as_mut(), first_audio) {
                    if let Some(payload) = frame_data(&data, format) {
                        es.push_pes(Some(ts_to_pts(tag.ts)), payload);
                        es.probe(false);
                    }
                }

                false
            }
            TAG_SCRIPT if !meta_checked && tag.size <= MAX_SCRIPT_LEN => {
                let mut data = vec![0u8; tag.size];

                if reader.read_buf_exact(&mut data).is_err() {
                    break;
                }

                let mut values = parse_values(&data).into_iter();

                if values.next().as_ref().and_then(Amf::as_str) == Some("onMetaData") {
                    meta = values.next();
                    meta_checked = true;
                }

                false
            }
            _ => reader.ignore_bytes(tag.size as u64).is_err(),
        };

        if eof || reader.ignore_bytes(4).is_err() {
            break;
        }

        // The audio format and the metadata, which is the first tag, are found.
        if es.as_ref().is_some_and(|es| es.info().is_some()) && (meta_checked || tags >= 8) {
            break;
        }
    }

    let Some(mut es) = es
    else {
        return unsupported_error("flv: no audio");
    };

    // Determine the stream info at the end of the stream, if not already.
    es.probe(true);

    if es.info().is_none() {
        return unsupported_error("flv: could not parse the audio");
    }

    Ok(Discovered { es, format, first_audio: first_audio.expect("audio was found"), meta })
}

/// Reads the keyframe index of the `onMetaData` script data, as pairs of time stamps in
/// milliseconds, and positions.
fn read_keyframes(meta: &Amf) -> Vec<(u32, u64)> {
    let Some(kf) = meta.get("keyframes")
    else {
        return vec![];
    };

    let (Some(Amf::Array(times)), Some(Amf::Array(positions))) =
        (kf.get("times"), kf.get("filepositions"))
    else {
        return vec![];
    };

    let mut entries: Vec<(u32, u64)> = times
        .iter()
        .zip(positions)
        .filter_map(|(t, p)| {
            let (t, p) = (t.as_f64()?, p.as_f64()?);
            (t >= 0.0 && p >= 0.0 && t < 4e6 && p < 1e15).then_some(((t * 1000.0) as u32, p as u64))
        })
        .collect();

    // The index must be ordered.
    entries.retain({
        let mut last = None;
        move |&(t, p)| {
            let ok = last.is_none_or(|(lt, lp)| t >= lt && p > lp);
            if ok {
                last = Some((t, p));
            }
            ok
        }
    });

    entries
}

/// Flash Video (FLV) format reader.
///
/// `FlvReader` implements an audio-only demuxer for FLV. See the crate documentation for the
/// supported audio, and the timeline.
pub struct FlvReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    es: EsTrack,
    /// The sound format of the track.
    format: u8,
    /// The position of the first tag.
    data_start: u64,
    /// The position and time stamp of the first audio tag.
    first_audio: (u64, u32),
    /// The seek index: (time stamp in milliseconds, position of the tag), in ascending order.
    index: Vec<(u32, u64)>,
    index_period_ms: u32,
    out: VecDeque<Packet>,
    eof: bool,
    /// The timestamp of the last packet returned.
    last_ts: Option<i64>,
}

impl<'s> FlvReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let header_pos = mss.pos();

        let mut hdr = [0u8; FLV_HEADER_LEN];
        mss.read_buf_exact(&mut hdr)?;

        if hdr[..4] != FLV_SIGNATURE {
            return unsupported_error("flv: invalid signature");
        }

        let data_offset = u64::from(u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]));

        if data_offset < FLV_HEADER_LEN as u64 {
            return unsupported_error("flv: invalid header size");
        }

        // Skip to the first tag, after the size of the previous tag.
        let data_start = header_pos + data_offset + 4;
        mss.ignore_bytes(data_offset - FLV_HEADER_LEN as u64 + 4)?;

        let Discovered { mut es, format, first_audio, meta } = discover(&mut mss)?;

        let info = es.info().expect("stream was probed").clone();
        let rate = info.rate;

        let base = ts_to_pts(first_audio.1);
        es.init_timeline(Some(base));

        // The seek index. The keyframe index is of the video keyframes of the file. It is only
        // used to find tags to begin scanning at.
        let mut index: Vec<(u32, u64)> = meta.as_ref().map(read_keyframes).unwrap_or_default();

        if index.first().is_none_or(|e| e.0 > first_audio.1) {
            index.insert(0, (first_audio.1, first_audio.0));
        }

        let mut reader = FlvReader {
            reader: mss,
            media_info: MediaInfo::new(),
            tracks: vec![],
            metadata: opts.external_data.metadata.unwrap_or_default(),
            chapters: opts.external_data.chapters,
            es,
            format,
            data_start,
            first_audio,
            index,
            index_period_ms: u32::from(opts.seek_index_fill_period_ms).max(1),
            out: VecDeque::new(),
            eof: false,
            last_ts: None,
        };

        // The duration.
        let mut end_ts = reader.scan_tail()?;

        if end_ts.is_none() {
            // The duration of the metadata.
            end_ts = meta
                .as_ref()
                .and_then(|m| m.get("duration"))
                .and_then(Amf::as_f64)
                .filter(|&d| d > 0.0 && d < 4e6)
                .map(|d| (d * f64::from(rate)).round() as i64);
        }

        if opts.prebuild_seek_index && reader.reader.is_seekable() {
            reader.prebuild_index()?;
        }

        let mut track = Track::new(0);

        track.with_time_base(
            TimeBase::try_from_recip(rate).ok_or(Error::DecodeError("flv: invalid sample rate"))?,
        );
        track.with_codec_params(CodecParameters::Audio(info.params.clone()));
        track.with_flags(TrackFlags::DEFAULT);

        if let Some(end_ts) = end_ts.filter(|&t| t > 0) {
            let out_rate = info.params.sample_rate.unwrap_or(rate);
            track.with_duration(Duration::new(end_ts as u64));
            track.with_num_frames(end_ts as u64 * u64::from(out_rate) / u64::from(rate));
        }

        reader.media_info = MediaInfo::from_track(&track);
        reader.tracks.push(track);

        Ok(reader)
    }

    /// Verify that there is a tag at `pos`, followed by the size of the tag and another tag (or the
    /// end of the stream). Returns the header of the tag.
    fn verify_tag(&mut self, pos: u64) -> Result<Option<TagHeader>> {
        self.reader.seek(SeekFrom::Start(pos))?;

        let Some(tag) = read_tag_header(&mut self.reader)?
        else {
            return Ok(None);
        };

        // The size of the tag follows the data of the tag.
        if self
            .reader
            .byte_len()
            .is_some_and(|len| tag.pos + (TAG_HEADER_LEN + tag.size) as u64 > len)
        {
            return Ok(None);
        }

        self.reader.seek(SeekFrom::Start(pos + (TAG_HEADER_LEN + tag.size) as u64))?;

        match self.reader.read_be_u32() {
            Ok(size) if size as usize == TAG_HEADER_LEN + tag.size => (),
            _ => return Ok(None),
        }

        // The tag is followed by another tag, or the end of the stream.
        match read_tag_header(&mut self.reader) {
            Ok(Some(_)) => Ok(Some(tag)),
            Ok(None) if self.reader.byte_len().is_none_or(|len| tag.end() >= len) => Ok(Some(tag)),
            _ => Ok(None),
        }
    }

    /// Scan forward from the byte position `from` for an audio tag. Returns the header of the first
    /// audio tag that is at least `from`.
    fn find_audio_tag(&mut self, from: u64, limit: u64) -> Result<Option<TagHeader>> {
        const CHUNK: usize = 64 * 1024;

        let mut pos = from;

        while pos - from < limit {
            self.reader.seek(SeekFrom::Start(pos))?;

            let mut buf = vec![0u8; CHUNK];
            let mut len = 0;

            while len < buf.len() {
                match self.reader.read_buf(&mut buf[len..]) {
                    Ok(0) => break,
                    Ok(n) => len += n,
                    Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e.into()),
                }
            }

            if len < TAG_HEADER_LEN {
                return Ok(None);
            }

            for i in 0..=len - TAG_HEADER_LEN {
                if buf[i] != TAG_AUDIO || buf[i + 8..i + 11] != [0, 0, 0] {
                    continue;
                }

                if self.verify_tag(pos + i as u64)?.is_some() {
                    let tag = TagHeader::parse(&buf[i..], pos + i as u64);
                    return Ok(tag);
                }
            }

            // Overlap the chunks by a tag header.
            pos += (len - TAG_HEADER_LEN + 1) as u64;
        }

        Ok(None)
    }

    /// Examine the tags at the end of the stream, if it is seekable, for the last audio tag.
    /// Returns the end timestamp of the audio.
    fn scan_tail(&mut self) -> Result<Option<i64>> {
        let (Some(len), true) = (self.reader.byte_len(), self.reader.is_seekable())
        else {
            return Ok(None);
        };

        let restore = self.reader.pos();
        let mut end = len;
        let mut result = None;

        for _ in 0..MAX_TAIL_TAGS {
            if end < self.data_start + (TAG_HEADER_LEN + 4) as u64 {
                break;
            }

            // The size of the previous tag.
            self.reader.seek(SeekFrom::Start(end - 4))?;
            let size = u64::from(self.reader.read_be_u32()?);

            let Some(pos) = (end - 4).checked_sub(size).filter(|&p| p >= self.data_start)
            else {
                break;
            };

            self.reader.seek(SeekFrom::Start(pos))?;

            let Some(tag) = read_tag_header(&mut self.reader)?
            else {
                break;
            };

            if (TAG_HEADER_LEN + tag.size) as u64 != size {
                break;
            }

            if tag.ty == TAG_AUDIO {
                let mut data = vec![0u8; tag.size];
                self.reader.read_buf_exact(&mut data)?;

                if let Some(payload) = frame_data(&data, self.format) {
                    let clock = self.rel_clock(tag.ts, 0);
                    let ticks = self.es.timeline().ticks_for_clock(clock);
                    let dur = self.es.payload_duration(&[payload.to_vec()]) as i64;

                    // The frame must contain samples.
                    if dur > 0 {
                        result = Some(ticks + dur);
                        break;
                    }
                }
            }

            end = pos;
        }

        self.reader.seek(SeekFrom::Start(restore))?;

        Ok(result)
    }

    /// Build the whole seek index by reading all of the tag headers.
    fn prebuild_index(&mut self) -> Result<()> {
        let restore = self.reader.pos();

        self.reader.seek(SeekFrom::Start(self.first_audio.0))?;

        while let Some(tag) = read_tag_header(&mut self.reader)? {
            if tag.ty == TAG_AUDIO {
                self.add_index_entry(tag.ts, tag.pos);
            }

            if self.reader.ignore_bytes(tag.size as u64 + 4).is_err() {
                break;
            }
        }

        self.reader.seek(SeekFrom::Start(restore))?;

        Ok(())
    }

    fn add_index_entry(&mut self, ts: u32, pos: u64) {
        if let Some(&(last_ts, last_pos)) = self.index.last() {
            if pos <= last_pos || ts < last_ts.saturating_add(self.index_period_ms) {
                return;
            }
        }

        self.index.push((ts, pos));
    }

    /// The time in 90 kHz clock ticks since the first audio tag of a tag time stamp.
    fn rel_clock(&self, ts: u32, hint: i64) -> i64 {
        unwrap_pts(ts_to_pts(ts), ts_to_pts(self.first_audio.1), hint)
    }

    /// Flush the frames of the stream at the end of the stream.
    fn flush(&mut self) {
        while let Some(packet) = self.es.pop(true) {
            self.out.push_back(packet);
        }

        self.eof = true;
    }

    /// Read and process one tag. Returns false at the end of the stream.
    fn read_one(&mut self) -> Result<bool> {
        let tag = match read_tag_header(&mut self.reader) {
            Ok(Some(tag)) => tag,
            Ok(None) => {
                // The end of the stream, or data that is not a tag.
                if self
                    .reader
                    .byte_len()
                    .is_some_and(|len| self.reader.pos() + TAG_HEADER_LEN as u64 <= len)
                {
                    // Resynchronise to the next tag.
                    let pos = self.reader.pos();
                    warn!("flv: lost sync at {pos}");

                    if self.reader.is_seekable() {
                        if let Some(tag) = self.find_audio_tag(
                            pos.saturating_sub(TAG_HEADER_LEN as u64 - 1),
                            1024 * 1024,
                        )? {
                            self.reader.seek(SeekFrom::Start(tag.pos))?;
                            return Ok(true);
                        }
                    }
                }

                self.flush();
                return Ok(false);
            }
            Err(e) => return Err(e),
        };

        if tag.ty == TAG_AUDIO {
            let mut data = vec![0u8; tag.size];

            if self.reader.read_buf_exact(&mut data).is_err() {
                self.flush();
                return Ok(false);
            }

            if let Some(payload) = frame_data(&data, self.format) {
                self.add_index_entry(tag.ts, tag.pos);
                self.es.push_pes(Some(ts_to_pts(tag.ts)), payload);

                while let Some(packet) = self.es.pop(false) {
                    self.out.push_back(packet);
                }
            }
        }
        else if self.reader.ignore_bytes(tag.size as u64).is_err() {
            self.flush();
            return Ok(false);
        }

        // The size of the tag.
        let _ = self.reader.ignore_bytes(4);

        Ok(true)
    }

    fn next_packet_inner(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(packet) = self.out.pop_front() {
                self.last_ts = Some(packet.pts.get());
                return Ok(Some(packet));
            }

            if self.eof {
                return Ok(None);
            }

            self.read_one()?;
        }
    }

    /// Find the first audio tag at or after `pos`. Returns the position of the tag, and the time
    /// stamp.
    fn probe_tag(&mut self, pos: u64) -> Result<Option<(u64, u32)>> {
        Ok(self.find_audio_tag(pos, PROBE_SCAN_LEN)?.map(|tag| (tag.pos, tag.ts)))
    }
}

impl Scoreable for FlvReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut hdr = [0u8; FLV_HEADER_LEN + 4];

        if src.read_buf_exact(&mut hdr).is_err() || hdr[..4] != FLV_SIGNATURE {
            return Ok(Score::Unsupported);
        }

        // The reserved bits of the flags are zero.
        if hdr[4] & 0xfa != 0 {
            return Ok(Score::Unsupported);
        }

        let data_offset = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]);

        if data_offset < FLV_HEADER_LEN as u32 {
            return Ok(Score::Unsupported);
        }

        // The size of the previous tag before the first tag is 0.
        if data_offset == FLV_HEADER_LEN as u32 && hdr[9..13] != [0, 0, 0, 0] {
            return Ok(Score::Unsupported);
        }

        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for FlvReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(FlvReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            FLV_FORMAT_INFO,
            &["flv"],
            &["video/x-flv", "audio/x-flv"],
            &[&FLV_SIGNATURE]
        )]
    }
}

impl FormatReader for FlvReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &FLV_FORMAT_INFO
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
        let track = &self.tracks[0];

        let required_ts = match to {
            SeekTo::Timestamp { ts, track_id } => {
                if track_id != track.id {
                    return seek_error(SeekErrorKind::Unseekable);
                }
                ts
            }
            SeekTo::Time { time, track_id } => {
                if track_id.is_some_and(|id| id != track.id) {
                    return seek_error(SeekErrorKind::Unseekable);
                }

                let tb = track.time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        if required_ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // The timestamp must be within the duration, if it is known.
        if let Some(dur) = track.duration {
            if required_ts.get() > dur.get() as i64 {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        let track_id = track.id;
        let rate = self.es.info().map_or(1, |i| i.rate);

        // The timestamp to start decoding from, so that a decoder reproduces a continuous decode
        // at the required timestamp.
        let start_ts = self.es.parser().seek_start_ts(required_ts.get()).max(0);

        debug!("flv: seeking to ts={required_ts} (start from {start_ts})");

        if self.reader.is_seekable() && self.reader.byte_len().is_some() {
            let len = self.reader.byte_len().unwrap_or(0);

            let target_clock = ticks_to_clock(start_ts, rate);
            let target_ms = i64::from(self.first_audio.1) + target_clock / 90;

            // Begin at the last entry of the index that is at or before the target.
            let idx = self.index.partition_point(|&(ts, _)| i64::from(ts) <= target_ms);
            let mut lo = self.first_audio.0;

            if idx > 0 {
                let (ts, pos) = self.index[idx - 1];

                // The index may have come from the file's metadata, which cannot be trusted.
                if pos >= self.first_audio.0
                    && i64::from(ts) <= target_ms
                    && self.verify_tag(pos)?.is_some()
                {
                    lo = pos;
                }
            }

            let first_ts = i64::from(self.first_audio.1);

            let pos = bisect(lo, len, target_ms, BISECT_MIN_GAP, |pos| {
                Ok::<_, Error>(self.probe_tag(pos)?.map(|(pos, ts)| {
                    // The time stamps are only 32 bits, and a stream may wrap.
                    (pos, first_ts + self.rel_clock(ts, target_clock) / 90)
                }))
            })?;

            self.reader.seek(SeekFrom::Start(pos))?;
            self.out.clear();
            self.eof = false;
            self.es.seek(target_clock);
        }
        else if self.last_ts.is_some_and(|ts| required_ts.get() < ts) {
            return seek_error(SeekErrorKind::ForwardOnly);
        }

        // Scan forward to the first packet that ends after the timestamp to start from.
        loop {
            match self.next_packet_inner()? {
                Some(packet)
                    if packet.pts.get().saturating_add(packet.dur.get() as i64) > start_ts =>
                {
                    let actual_ts = packet.pts;
                    self.out.push_front(packet);

                    // The packet returned next is at the position seeked to.
                    self.last_ts = Some(actual_ts.get());

                    info!("flv: seeked to ts={actual_ts}");

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_keyframe_index_is_read_and_validated() {
        let arr = |v: &[f64]| Amf::Array(v.iter().map(|&n| Amf::Number(n)).collect());

        let meta = |times: &[f64], pos: &[f64]| {
            Amf::Object(vec![(
                "keyframes".into(),
                Amf::Object(vec![("filepositions".into(), arr(pos)), ("times".into(), arr(times))]),
            )])
        };

        assert_eq!(
            read_keyframes(&meta(&[0.0, 1.5, 3.0], &[100.0, 2000.0, 4000.0])),
            vec![(0, 100), (1500, 2000), (3000, 4000)]
        );

        // Entries that are out of order, or negative, are dropped.
        assert_eq!(
            read_keyframes(&meta(
                &[0.0, 2.0, 1.0, 3.0, -1.0],
                &[100.0, 2000.0, 3000.0, 1500.0, 9.0]
            )),
            vec![(0, 100), (2000, 2000)]
        );

        // Mismatched or missing arrays.
        assert_eq!(read_keyframes(&meta(&[0.0], &[])), vec![]);
        assert_eq!(read_keyframes(&Amf::Object(vec![])), vec![]);
    }

    #[test]
    fn verify_tag_header_parse() {
        let mut b = [0u8; 11];
        b[0] = TAG_AUDIO;
        b[1..4].copy_from_slice(&[0, 1, 2]);
        b[4..7].copy_from_slice(&[0x12, 0x34, 0x56]);
        b[7] = 0x01;

        let tag = TagHeader::parse(&b, 10).unwrap();
        assert_eq!((tag.size, tag.ts, tag.pos), (258, 0x0112_3456, 10));
        assert_eq!(tag.end(), 10 + 11 + 258 + 4);

        // A stream ID that is not 0, and an unknown type.
        b[10] = 1;
        assert!(TagHeader::parse(&b, 0).is_none());
        b[10] = 0;
        b[0] = 3;
        assert!(TagHeader::parse(&b, 0).is_none());
    }
}
