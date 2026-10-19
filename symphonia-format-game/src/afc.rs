// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Nintendo GameCube AFC demuxer.
//!
//! The layout follows FFmpeg's `libavformat/afc.c`:
//!
//! ```text
//! 0x00  u32   size of the audio data
//! 0x04  u32   number of samples (per channel)
//! 0x08  u16   sample rate
//! 0x0a        reserved (22 bytes)
//! 0x20        the audio
//! ```
//!
//! The audio is stereo (like FFmpeg, the header is not asked for the channel count). It is a
//! sequence of 18 byte groups, which hold the 9 byte frame of the left channel and that of the
//! right channel, each of 16 samples. All fields are big-endian.
//!
//! The format has no signature. It is recognised by the plausibility of the header, with a low
//! score.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_ADPCM_AFC;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Result, SeekErrorKind, decode_error, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use crate::common::{channels_for, frame_time_base, read_up_to, required_timestamp, unseekable};

const AFC_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"AFC\0")),
    short_name: "afc",
    long_name: "Nintendo GameCube AFC",
};

/// The size in bytes of the header.
const HEADER_LEN: u64 = 32;

/// The number of channels.
const CHANNELS: u64 = 2;

/// The size in bytes of a group: a frame of each channel.
const GROUP_LEN: u64 = 9 * CHANNELS;

/// The number of samples per channel in a group.
const GROUP_SAMPLES: u64 = 16;

/// The number of groups in a packet (`18 * 128` bytes, `afc_read_packet` of `libavformat/afc.c`).
const GROUPS_PER_PACKET: u64 = 128;

/// The number of groups decoded and discarded before the requested position of an accurate seek.
/// The history of the ADPCM is not stored in the stream; its response to a wrong history decays
/// with the number of samples.
const SEEK_PREROLL_GROUPS: u64 = 8;

/// Nintendo GameCube AFC reader.
pub struct AfcReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    /// The position of the first group.
    data_start: u64,
    /// The index of the next group to read.
    next_group: u64,
    /// The number of groups of audio.
    num_groups: Option<u64>,
    /// The number of frames, if it is known.
    num_frames: Option<u64>,
}

impl<'s> AfcReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        let data_start = source.pos() + HEADER_LEN;

        let mut header = [0u8; HEADER_LEN as usize];
        source.read_buf_exact(&mut header)?;

        let size = u64::from(u32::from_be_bytes([header[0], header[1], header[2], header[3]]));
        let duration = u64::from(u32::from_be_bytes([header[4], header[5], header[6], header[7]]));
        let rate = u32::from(u16::from_be_bytes([header[8], header[9]]));

        if rate == 0 {
            return decode_error("afc: invalid sample rate");
        }

        // The audio ends where the header says or at the end of the stream.
        let mut bytes = size;
        if let Some(len) = source.byte_len() {
            bytes = bytes.min(len.saturating_sub(data_start));
        }

        let groups = bytes / GROUP_LEN;
        let available = groups * GROUP_SAMPLES;

        // A total of 0 was written by an encoder that did not know the length.
        let num_frames = if duration == 0 { available } else { duration.min(available) };

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_ADPCM_AFC)
            .with_sample_rate(rate)
            .with_bits_per_coded_sample(4)
            .with_channels(channels_for(CHANNELS as usize))
            // A frame of each channel in turn.
            .with_frames_per_block(GROUP_SAMPLES)
            .with_max_frames_per_packet(GROUPS_PER_PACKET * GROUP_SAMPLES);

        let time_base = frame_time_base(rate);

        let mut track = Track::new(0);
        track.time_base = time_base;
        track.num_frames = Some(num_frames);
        track.duration = Some(Duration::new(num_frames));
        track.with_codec_params(CodecParameters::Audio(params));

        let mut media_info = MediaInfo::new();
        media_info.time_base = time_base;
        media_info.duration = Some(Duration::new(num_frames));

        Ok(AfcReader {
            reader: source,
            media_info,
            tracks: vec![track],
            metadata: MetadataLog::default(),
            data_start,
            next_group: 0,
            num_groups: Some(groups),
            num_frames: Some(num_frames),
        })
    }
}

impl Scoreable for AfcReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut header = [0u8; HEADER_LEN as usize];

        if read_up_to(&mut src, &mut header)? < header.len() {
            return Ok(Score::Unsupported);
        }

        // There is no signature: look for a plausible sample rate and a size that is a whole
        // number of groups.
        let rate = u16::from_be_bytes([header[8], header[9]]);
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let duration = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        let groups = size / GROUP_LEN as u32;

        // The duration fits in the audio, and the audio fits in the file when its length is known.
        let fits = src
            .inner()
            .byte_len()
            .is_none_or(|len| u64::from(size) <= len.saturating_sub(HEADER_LEN));
        let plausible = rate >= 4000
            && size > 0
            && size % GROUP_LEN as u32 == 0
            && u64::from(duration) <= u64::from(groups) * GROUP_SAMPLES
            && fits;

        Ok(if plausible { Score::Supported(10) } else { Score::Unsupported })
    }
}

impl ProbeableFormat<'_> for AfcReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(AfcReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(AFC_FORMAT_INFO, &["afc"], &["audio/x-afc"], &[])]
    }
}

impl FormatReader for AfcReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &AFC_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let groups = match self.num_groups {
            Some(total) => total.saturating_sub(self.next_group).min(GROUPS_PER_PACKET),
            None => GROUPS_PER_PACKET,
        };

        if groups == 0 {
            return Ok(None);
        }

        let mut buf = vec![0u8; (groups * GROUP_LEN) as usize];
        let len = read_up_to(&mut self.reader, &mut buf)?;

        // A partial group at the end of the stream is dropped.
        let groups = len as u64 / GROUP_LEN;
        if groups == 0 {
            self.num_groups = Some(self.next_group);
            return Ok(None);
        }

        buf.truncate((groups * GROUP_LEN) as usize);

        let pts = self.next_group * GROUP_SAMPLES;
        let block_frames = groups * GROUP_SAMPLES;
        self.next_group += groups;

        // The frames after the total are the padding of the last group.
        let dur = match self.num_frames {
            Some(total) => total.saturating_sub(pts).min(block_frames),
            None => block_frames,
        };

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(pts as i64))
                .dur(Duration::new(dur))
                .trim_end(Duration::new(block_frames - dur))
                .data(buf)
                .build(),
        ))
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if !self.reader.is_seekable() {
            return unseekable();
        }

        let Some(time_base) = self.media_info.time_base
        else {
            return unseekable();
        };

        let required_ts = required_timestamp(to, time_base)?;

        if self.num_frames.is_some_and(|frames| required_ts >= frames) {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let mut group = required_ts / GROUP_SAMPLES;

        if mode == SeekMode::Accurate {
            group = group.saturating_sub(SEEK_PREROLL_GROUPS);
        }

        self.reader.seek(SeekFrom::Start(self.data_start + group * GROUP_LEN))?;
        self.next_group = group;

        Ok(SeekedTo {
            track_id: 0,
            required_ts: Timestamp::new(required_ts as i64),
            actual_ts: Timestamp::new((group * GROUP_SAMPLES) as i64),
        })
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
