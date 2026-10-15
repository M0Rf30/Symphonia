// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{ErrorKind, Seek, SeekFrom};
use std::sync::Arc;

use symphonia_core::audio::layouts;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_TWINVQ;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{
    Metadata, MetadataBuilder, MetadataInfo, MetadataLog, StandardTag, Tag,
};
use symphonia_core::packet::Packet;
use symphonia_core::packet::PacketBuilder;
use symphonia_core::support_format;
use symphonia_core::units::{Duration, TimeBase, Timestamp};

use log::{debug, info};

const VQF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"VQF ")),
    short_name: "vqf",
    long_name: "NTT TwinVQ",
};

const VQF_METADATA_INFO: MetadataInfo = MetadataInfo {
    metadata: symphonia_core::meta::MetadataId::new(FourCc::new(*b"VQF ")),
    short_name: "vqf",
    long_name: "NTT TwinVQ chunks",
};

/// The versions written by the NTT encoder; FFmpeg scores these as certain.
const KNOWN_VERSIONS: [&[u8; 8]; 2] = [b"97012000", b"00052200"];

/// Metadata chunks that FFmpeg skips: dates, and reserved chunks.
const SKIPPED_CHUNKS: [&[u8; 4]; 6] = [b"YEAR", b"ENCD", b"EXTR", b"_YMH", b"_NTT", b"_ID3"];

/// Returns the frame size in samples for a sample rate (Hz) and a bit rate per channel (kbit/s),
/// as FFmpeg's `vqf_read_header`.
fn frame_size(sample_rate: u32, kbps_per_channel: u32) -> Option<u32> {
    match (sample_rate / 1000, kbps_per_channel) {
        (11, 8) | (8, 8) | (11, 10) | (22, 32) => Some(512),
        (16, 16) | (22, 20) | (22, 24) => Some(1024),
        (44, 40) | (44, 48) => Some(2048),
        _ => None,
    }
}

/// Converts the bytes of a chunk to text: UTF-8 if valid, ISO 8859-1 otherwise.
fn chunk_text(data: &[u8]) -> String {
    let data = match data.iter().rposition(|&b| b != 0) {
        Some(last) => &data[..=last],
        None => &[],
    };

    match std::str::from_utf8(data) {
        Ok(s) => s.to_string(),
        Err(_) => data.iter().map(|&b| char::from(b)).collect(),
    }
}

/// Maps a text chunk to a tag. Chunk names are those in FFmpeg's `vqf_metadata_conv`.
fn make_tag(name: [u8; 4], text: String, out: &mut Vec<Tag>) {
    // The key is kept as is, including the trailing space of `(c) `.
    let key = chunk_text(&name);

    let value = Arc::new(text);

    let std = match &name {
        b"(c) " => Some(StandardTag::Copyright(value.clone())),
        b"ARNG" => Some(StandardTag::Arranger(value.clone())),
        b"AUTH" => Some(StandardTag::Artist(value.clone())),
        b"BAND" => Some(StandardTag::Ensemble(value.clone())),
        b"CDCT" => Some(StandardTag::Conductor(value.clone())),
        b"COMT" => Some(StandardTag::Comment(value.clone())),
        b"FILE" => Some(StandardTag::OriginalFile(value.clone())),
        b"GENR" => Some(StandardTag::Genre(value.clone())),
        b"LABL" => Some(StandardTag::Label(value.clone())),
        b"MUSC" => Some(StandardTag::Composer(value.clone())),
        b"NAME" => Some(StandardTag::TrackTitle(value.clone())),
        b"PROD" => Some(StandardTag::Producer(value.clone())),
        b"REMX" => Some(StandardTag::Remixer(value.clone())),
        b"SING" => Some(StandardTag::Performer(value.clone())),
        b"WORD" => Some(StandardTag::Lyricist(value.clone())),
        b"TRCK" => {
            // "number" or "number/total".
            let mut parts = value.splitn(2, '/');
            let number = parts.next().and_then(|n| n.trim().parse::<u64>().ok());
            if let Some(total) = parts.next().and_then(|t| t.trim().parse::<u64>().ok()) {
                out.push(Tag::new_from_parts(
                    "TRACK_TOTAL",
                    total,
                    Some(StandardTag::TrackTotal(total)),
                ));
            }
            number.map(StandardTag::TrackNumber)
        }
        // NOTE, PRSN and unknown chunks have no standard tag.
        _ => None,
    };

    out.push(Tag::new_from_parts(key, value.as_str(), std));
}

/// An NTT TwinVQ (`.vqf`) reader.
pub struct VqfReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    /// The position of the first byte of the bitstream.
    data_start: u64,
    /// The frame size in samples.
    frame_len: u32,
    /// The number of bits of a frame.
    frame_bits: i64,
    /// The number of bits of the last byte read that were not used by the previous frame; negative
    /// after a seek.
    remaining_bits: i64,
    /// The last byte of the previous packet.
    last_byte: u8,
    /// The timestamp of the next packet.
    next_ts: u64,
    /// The number of frames, if known.
    num_frames: Option<u64>,
}

impl Scoreable for VqfReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        // FFmpeg's `vqf_probe`.
        let mut header = [0u8; 16];
        src.read_buf_exact(&mut header)?;

        if &header[..4] != b"TWIN" {
            return Ok(Score::Unsupported);
        }

        if KNOWN_VERSIONS.iter().any(|v| &header[4..12] == *v) {
            return Ok(Score::Supported(255));
        }

        // An unknown version: confident only if the header size is plausible.
        let header_size = u32::from_be_bytes([header[12], header[13], header[14], header[15]]);
        Ok(Score::Supported(if header_size > (1 << 27) { 64 } else { 128 }))
    }
}

impl<'s> VqfReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        if mss.read_quad_bytes()? != *b"TWIN" {
            return unsupported_error("vqf: missing TWIN marker");
        }

        // The version string.
        mss.ignore_bytes(8)?;

        let mut header_size = i64::from(mss.read_be_u32()?);
        if header_size > i64::from(i32::MAX) {
            return decode_error("vqf: invalid header size");
        }

        let mut comm: Option<[u8; 12]> = None;
        let mut dsize = None;
        let mut tags = Vec::new();

        // Read the chunks until DATA, as FFmpeg does. The header size is the number of bytes of
        // the chunks, DATA excluded.
        loop {
            let name = mss.read_quad_bytes()?;

            if name == *b"DATA" {
                break;
            }

            let len = u64::from(mss.read_be_u32()?);

            if len > (i32::MAX / 2) as u64 || header_size < 8 {
                return decode_error("vqf: malformed header");
            }

            header_size -= 8;
            let avail = header_size as u64;

            match &name {
                b"COMM" => {
                    if len < 12 {
                        return decode_error("vqf: invalid COMM chunk");
                    }
                    let mut data = [0u8; 12];
                    mss.read_buf_exact(&mut data)?;
                    mss.ignore_bytes(len - 12)?;
                    comm = Some(data);
                }
                b"DSIZ" => {
                    // The size of the compressed data.
                    if len >= 4 {
                        dsize = Some(u64::from(mss.read_be_u32()?));
                        mss.ignore_bytes(len - 4)?;
                    }
                    else {
                        mss.ignore_bytes(len)?;
                    }
                }
                n if SKIPPED_CHUNKS.contains(&n) => {
                    mss.ignore_bytes(len.min(avail))?;
                }
                _ => {
                    let n = len.min(avail) as usize;
                    let data = mss.read_boxed_slice_exact(n)?;
                    make_tag(name, chunk_text(&data), &mut tags);
                }
            }

            header_size -= len as i64;

            if header_size < 0 {
                return decode_error("vqf: missing DATA chunk");
            }
        }

        let data_start = mss.pos();

        let Some(comm) = comm
        else {
            return decode_error("vqf: COMM chunk not found");
        };

        let be32 = |o: usize| u32::from_be_bytes([comm[o], comm[o + 1], comm[o + 2], comm[o + 3]]);

        let channels = be32(0).saturating_add(1);
        let kbps = be32(4);
        let rate_flag = be32(8);

        if channels > 2 {
            return unsupported_error("vqf: unsupported number of channels");
        }

        let sample_rate = match rate_flag {
            44 => 44100,
            22 => 22050,
            11 => 11025,
            8..=43 => rate_flag * 1000,
            _ => return decode_error("vqf: invalid rate flag"),
        };

        let ibps = kbps / channels;
        if ibps < 8 || ibps > 48 {
            return decode_error("vqf: invalid bit rate per channel");
        }

        let Some(frame_len) = frame_size(sample_rate, ibps)
        else {
            return unsupported_error("vqf: unsupported sample rate and bit rate combination");
        };

        let frame_bits = i64::from(kbps) * 1000 * i64::from(frame_len) / i64::from(sample_rate);

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_TWINVQ)
            .with_sample_rate(sample_rate)
            .with_channels(if channels == 1 {
                layouts::CHANNEL_LAYOUT_MONO
            }
            else {
                layouts::CHANNEL_LAYOUT_STEREO
            })
            .with_max_frames_per_packet(u64::from(frame_len))
            .with_extra_data(Box::from(&comm[..]));

        let mut track = Track::new(0);
        track
            .with_codec_params(CodecParameters::Audio(params))
            .with_time_base(
                TimeBase::try_from_recip(sample_rate)
                    .ok_or(Error::DecodeError("vqf: invalid sample rate"))?,
            )
            .with_start_ts(Timestamp::new(0));

        // The data runs to the end of the stream; the size in the header is used only if the
        // length of the stream is unknown.
        let data_len = match mss.byte_len() {
            Some(len) => Some(len.saturating_sub(data_start)),
            None => dsize,
        };

        let num_frames = data_len.map(|len| len * 8 / frame_bits as u64);

        if let Some(frames) = num_frames {
            let samples = frames * u64::from(frame_len);
            track.with_num_frames(samples);
            track.with_duration(Duration::new(samples));
        }

        info!(
            "vqf: {sample_rate} Hz, {channels} channel(s), {kbps} kbit/s, {frame_len} samples/frame"
        );

        let mut metadata = opts.external_data.metadata.unwrap_or_default();
        if !tags.is_empty() {
            let mut builder = MetadataBuilder::new(VQF_METADATA_INFO);
            for tag in tags {
                builder.add_tag(tag);
            }
            metadata.push(builder.build());
        }

        Ok(VqfReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            chapters: opts.external_data.chapters,
            metadata,
            data_start,
            frame_len,
            frame_bits,
            remaining_bits: 0,
            last_byte: 0,
            next_ts: 0,
            num_frames,
        })
    }

    fn next_packet_inner(&mut self) -> Result<Option<Packet>> {
        // The number of bytes that hold the bits of the frame that were not in the previous
        // packet.
        let size = ((self.frame_bits - self.remaining_bits + 7) >> 3) as usize;

        let mut data = vec![0u8; size + 2];

        match self.reader.read_buf_exact(&mut data[2..]) {
            Ok(()) => (),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        // The number of bits to skip. After a seek the offset can be larger than 8.
        data[0] = (8 - self.remaining_bits) as u8;
        data[1] = self.last_byte;

        self.last_byte = data[size + 1];
        self.remaining_bits += ((size as i64) << 3) - self.frame_bits;

        let pts = self.next_ts;
        self.next_ts += u64::from(self.frame_len);

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(pts as i64))
                .dur(Duration::new(u64::from(self.frame_len)))
                .data(data)
                .build(),
        ))
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

        // Seek to the start of the frame that contains the timestamp.
        let frame = required_ts.get() as u64 / u64::from(self.frame_len);

        if self.num_frames.is_some_and(|n| frame >= n) {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let actual_ts = Timestamp::new((frame * u64::from(self.frame_len)) as i64);

        // The bit position of the frame. Start at the byte that holds the bit 7 bits before the
        // frame, so that the skip of the packet (which is at least 8 bits) lands on the frame, as
        // in FFmpeg's `vqf_read_seek`.
        let (pos, remaining_bits) = if frame == 0 {
            (self.data_start, 0)
        }
        else {
            let bits = frame as i64 * self.frame_bits;
            let byte = (bits - 7) >> 3;
            (self.data_start + byte as u64, -7 - ((bits - 7) & 7))
        };

        if self.reader.is_seekable() {
            self.reader.seek(SeekFrom::Start(pos))?;
        }
        else {
            let current = self.reader.pos();
            if pos < current {
                return seek_error(SeekErrorKind::ForwardOnly);
            }
            self.reader.ignore_bytes(pos - current)?;
        }

        debug!("vqf: seek to ts={required_ts}, frame={frame}, pos={pos}");

        self.remaining_bits = remaining_bits;
        self.last_byte = 0;
        self.next_ts = actual_ts.get() as u64;

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts })
    }
}

impl ProbeableFormat<'_> for VqfReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(VqfReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            VQF_FORMAT_INFO,
            &["vqf", "vql", "vqe"],
            &["audio/x-twinvq", "audio/x-vqf"],
            &[b"TWIN"]
        )]
    }
}

impl FormatReader for VqfReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &VQF_FORMAT_INFO
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
