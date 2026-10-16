// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sun/NeXT AU (`.snd`) reader.
//!
//! References: the Sun/NeXT audio file format description ("Audio File Formats FAQ", section 2.7,
//! and the `audio_filehdr.h` of Solaris), and FFmpeg's `libavformat/au.c` for the encoding codes
//! of the G.72x ADPCM variants.

use symphonia_common::pcm::{PcmLayout, PcmStream};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_ADPCM_G722, CODEC_ID_ADPCM_G726LE, CODEC_ID_PCM_ALAW, CODEC_ID_PCM_F32BE,
    CODEC_ID_PCM_F64BE, CODEC_ID_PCM_MULAW, CODEC_ID_PCM_S8, CODEC_ID_PCM_S16BE,
    CODEC_ID_PCM_S24BE, CODEC_ID_PCM_S32BE,
};
use symphonia_core::codecs::audio::{AudioCodecId, AudioCodecParameters};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{
    Metadata, MetadataBuilder, MetadataId, MetadataInfo, MetadataLog, StandardTag, Tag,
};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use crate::common::{channels_from_count, clamp_data_end};

use log::debug;

/// The magic number: ".snd".
const AU_MAGIC: [u8; 4] = *b".snd";

/// The size of the fixed fields of the header.
const AU_HEADER_LEN: u32 = 24;

/// The data size of a file that is being written, and so is not known.
const AU_UNKNOWN_SIZE: u32 = u32::MAX;

/// The maximum length of the annotation field that is read. The remainder is skipped.
const MAX_ANNOTATION_LEN: u32 = 1024 * 1024;

/// The encoding "7262" of FFmpeg: G.726 with 2 bits per sample, as written by some encoders.
const AU_ENCODING_G726_2BIT: u32 = u32::from_be_bytes(*b"7262");

const AU_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"AU  ")),
    short_name: "au",
    long_name: "Sun/NeXT AU",
};

const AU_METADATA_INFO: MetadataInfo = MetadataInfo {
    metadata: MetadataId::new(FourCc::new(*b"AU  ")),
    short_name: "au",
    long_name: "Sun/NeXT AU annotation",
};

/// A supported encoding.
struct Encoding {
    codec: AudioCodecId,
    /// The number of bits of a coded sample.
    bits: u32,
    /// The sample rate of the decoded audio, if the header does not state it.
    rate: Option<u32>,
}

/// Maps the encoding field of the header. Returns `None` for unsupported encodings.
fn map_encoding(encoding: u32) -> Option<Encoding> {
    // The codes are from the AU format description, and FFmpeg for the ADPCM ones (G.721 is G.726
    // at 32 kbit/s: 4 bits per sample, G.723 is at 24 and 40 kbit/s: 3 and 5 bits).
    let (codec, bits, rate) = match encoding {
        1 => (CODEC_ID_PCM_MULAW, 8, None),
        2 => (CODEC_ID_PCM_S8, 8, None),
        3 => (CODEC_ID_PCM_S16BE, 16, None),
        4 => (CODEC_ID_PCM_S24BE, 24, None),
        5 => (CODEC_ID_PCM_S32BE, 32, None),
        6 => (CODEC_ID_PCM_F32BE, 32, None),
        7 => (CODEC_ID_PCM_F64BE, 64, None),
        23 => (CODEC_ID_ADPCM_G726LE, 4, None),
        // G.722 always codes 16 kHz audio.
        24 => (CODEC_ID_ADPCM_G722, 4, Some(16_000)),
        25 => (CODEC_ID_ADPCM_G726LE, 3, None),
        26 => (CODEC_ID_ADPCM_G726LE, 5, None),
        27 => (CODEC_ID_PCM_ALAW, 8, None),
        AU_ENCODING_G726_2BIT => (CODEC_ID_ADPCM_G726LE, 2, None),
        _ => return None,
    };

    Some(Encoding { codec, bits, rate })
}

/// Parses the annotation field into tags, as FFmpeg does: `key=value` pairs, separated by a NUL
/// or a newline. The field ends at the first NUL that is not in a value.
fn read_annotation(data: &[u8]) -> Vec<Tag> {
    let mut tags = Vec::new();

    let mut rest = data;

    while let Some(eq) = rest.iter().position(|&b| b == b'=' || b == 0) {
        // A NUL before the "=" ends the field.
        if rest[eq] == 0 {
            break;
        }

        let key = String::from_utf8_lossy(&rest[..eq]).into_owned();
        rest = &rest[eq + 1..];

        let end = rest.iter().position(|&b| b == b'\n' || b == 0).unwrap_or(rest.len());
        let value = String::from_utf8_lossy(&rest[..end]).into_owned();

        let ended = rest.get(end) == Some(&0) || end == rest.len();

        push_tag(&mut tags, key, value);

        if ended {
            break;
        }

        rest = &rest[end + 1..];
    }

    tags
}

fn push_tag(tags: &mut Vec<Tag>, key: String, value: String) {
    use std::sync::Arc;

    let std = {
        let value = Arc::new(value.clone());

        match key.to_ascii_lowercase().as_str() {
            "title" => Some(StandardTag::TrackTitle(value)),
            "artist" => Some(StandardTag::Artist(value)),
            "album" => Some(StandardTag::Album(value)),
            "genre" => Some(StandardTag::Genre(value)),
            "track" => value
                .split('/')
                .next()
                .and_then(|n| n.trim().parse().ok())
                .map(StandardTag::TrackNumber),
            _ => None,
        }
    };

    tags.push(Tag::new_from_parts(&key, value, std));
}

/// Sun/NeXT AU format reader.
///
/// `AuReader` implements a demuxer for the big-endian `.snd` AU format. The audio is PCM (8, 16,
/// 24 and 32-bit integer, 32 and 64-bit floating point), mu-law, A-law, G.722 and G.726 (as
/// G.721 and G.723). The annotation field is read as `key=value` tags.
pub struct AuReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    stream: PcmStream,
}

impl<'s> AuReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, mut opts: FormatOptions) -> Result<Self> {
        if mss.read_quad_bytes()? != AU_MAGIC {
            return unsupported_error("au: missing .snd magic number");
        }

        let data_offset = mss.read_be_u32()?;
        let data_size = mss.read_be_u32()?;
        let encoding = mss.read_be_u32()?;
        let rate = mss.read_be_u32()?;
        let channels = mss.read_be_u32()?;

        // The annotation field is between the fixed fields and the data.
        if data_offset < AU_HEADER_LEN {
            return decode_error("au: invalid data offset");
        }

        let annotation_len = data_offset - AU_HEADER_LEN;

        let mut metadata = opts.external_data.metadata.take().unwrap_or_default();

        if annotation_len > 0 {
            let read_len = annotation_len.min(MAX_ANNOTATION_LEN);

            let mut annotation = vec![0; read_len as usize];
            mss.read_buf_exact(&mut annotation)?;
            mss.ignore_bytes(u64::from(annotation_len - read_len))?;

            let tags = read_annotation(&annotation);

            if !tags.is_empty() {
                let mut builder = MetadataBuilder::new(AU_METADATA_INFO);

                for tag in tags {
                    builder.add_tag(tag);
                }

                metadata.push(builder.build());
            }
        }

        let Some(encoding) = map_encoding(encoding)
        else {
            return unsupported_error("au: unsupported encoding");
        };

        if rate == 0 {
            return decode_error("au: invalid sample rate");
        }

        let channel_set = channels_from_count(channels)?;

        let Some(layout) = PcmLayout::from_sample_bits(encoding.bits, channels)
        else {
            return decode_error("au: invalid channel count");
        };

        let rate = encoding.rate.unwrap_or(rate);

        let mut params = AudioCodecParameters::new();

        params
            .for_codec(encoding.codec)
            .with_sample_rate(rate)
            .with_channels(channel_set)
            .with_max_frames_per_packet(layout.max_frames_per_packet());

        // The PCM decoder defines the width of A-law and mu-law samples itself.
        if encoding.codec != CODEC_ID_PCM_ALAW && encoding.codec != CODEC_ID_PCM_MULAW {
            params.with_bits_per_coded_sample(encoding.bits);
        }

        let start = mss.pos();

        let data_len = if data_size == AU_UNKNOWN_SIZE { None } else { Some(u64::from(data_size)) };
        let end = clamp_data_end(&mss, start, data_len);

        let mut track = Track::new(0);

        track.with_codec_params(CodecParameters::Audio(params)).with_start_ts(Timestamp::new(0));

        if let Some(end) = end {
            let num_frames = layout.num_frames(end - start);
            track.with_num_frames(num_frames).with_duration(Duration::new(num_frames));
        }

        debug!(
            "au: encoding {:?}, {rate} Hz, {channels} channels, data at {start}..{end:?}",
            encoding.codec
        );

        Ok(AuReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            metadata,
            chapters: opts.external_data.chapters,
            stream: PcmStream { layout, start, end, track_id: 0 },
        })
    }
}

impl Scoreable for AuReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        if src.read_quad_bytes()? != AU_MAGIC {
            return Ok(Score::Unsupported);
        }

        let data_offset = src.read_be_u32()?;
        let _data_size = src.read_be_u32()?;
        let encoding = src.read_be_u32()?;
        let rate = src.read_be_u32()?;
        let channels = src.read_be_u32()?;

        // The same fields that FFmpeg requires to be non-zero, and a known encoding.
        if data_offset < AU_HEADER_LEN || rate == 0 || channels == 0 {
            return Ok(Score::Unsupported);
        }

        Ok(Score::Supported(if map_encoding(encoding).is_some() { 255 } else { 100 }))
    }
}

impl<'s> ProbeableFormat<'s> for AuReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'s>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + 's>> {
        Ok(Box::new(AuReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            AU_FORMAT_INFO,
            &["au", "snd"],
            &["audio/basic", "audio/x-au", "audio/x-basic", "audio/x-snd"],
            &[&AU_MAGIC]
        )]
    }
}

impl FormatReader for AuReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &AU_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        self.stream.next_packet(&mut self.reader)
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
        self.stream.seek(&mut self.reader, &self.tracks[0], to)
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}
