// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headerless PCM reader.

use symphonia_common::pcm::{PcmLayout, PcmStream};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::*;
use symphonia_core::codecs::audio::{AudioCodecId, AudioCodecParameters};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Error, Result, decode_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::units::{Duration, Timestamp};

use crate::common::{channels_from_count, clamp_data_end};

const RAW_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"RAW ")),
    short_name: "pcm",
    long_name: "Raw PCM",
};

/// The coding of the samples of a raw PCM stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RawPcmSampleFormat {
    /// Unsigned 8-bit integer.
    U8,
    /// Signed 8-bit integer.
    S8,
    /// Unsigned 16-bit integer, little-endian.
    U16Le,
    /// Unsigned 16-bit integer, big-endian.
    U16Be,
    /// Signed 16-bit integer, little-endian.
    S16Le,
    /// Signed 16-bit integer, big-endian.
    S16Be,
    /// Unsigned 24-bit integer, little-endian.
    U24Le,
    /// Unsigned 24-bit integer, big-endian.
    U24Be,
    /// Signed 24-bit integer, little-endian.
    S24Le,
    /// Signed 24-bit integer, big-endian.
    S24Be,
    /// Unsigned 32-bit integer, little-endian.
    U32Le,
    /// Unsigned 32-bit integer, big-endian.
    U32Be,
    /// Signed 32-bit integer, little-endian.
    S32Le,
    /// Signed 32-bit integer, big-endian.
    S32Be,
    /// 32-bit floating point, little-endian.
    F32Le,
    /// 32-bit floating point, big-endian.
    F32Be,
    /// 64-bit floating point, little-endian.
    F64Le,
    /// 64-bit floating point, big-endian.
    F64Be,
    /// A-law.
    ALaw,
    /// Mu-law.
    MuLaw,
}

impl RawPcmSampleFormat {
    /// The codec of the samples, and the bits of a sample.
    fn codec(self) -> (AudioCodecId, u32) {
        use RawPcmSampleFormat::*;

        match self {
            U8 => (CODEC_ID_PCM_U8, 8),
            S8 => (CODEC_ID_PCM_S8, 8),
            U16Le => (CODEC_ID_PCM_U16LE, 16),
            U16Be => (CODEC_ID_PCM_U16BE, 16),
            S16Le => (CODEC_ID_PCM_S16LE, 16),
            S16Be => (CODEC_ID_PCM_S16BE, 16),
            U24Le => (CODEC_ID_PCM_U24LE, 24),
            U24Be => (CODEC_ID_PCM_U24BE, 24),
            S24Le => (CODEC_ID_PCM_S24LE, 24),
            S24Be => (CODEC_ID_PCM_S24BE, 24),
            U32Le => (CODEC_ID_PCM_U32LE, 32),
            U32Be => (CODEC_ID_PCM_U32BE, 32),
            S32Le => (CODEC_ID_PCM_S32LE, 32),
            S32Be => (CODEC_ID_PCM_S32BE, 32),
            F32Le => (CODEC_ID_PCM_F32LE, 32),
            F32Be => (CODEC_ID_PCM_F32BE, 32),
            F64Le => (CODEC_ID_PCM_F64LE, 64),
            F64Be => (CODEC_ID_PCM_F64BE, 64),
            ALaw => (CODEC_ID_PCM_ALAW, 8),
            MuLaw => (CODEC_ID_PCM_MULAW, 8),
        }
    }
}

/// The parameters of a raw PCM stream, which has no header to state them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RawPcmParams {
    /// The coding of the samples.
    pub sample_format: RawPcmSampleFormat,
    /// The sample rate in Hz.
    pub sample_rate: u32,
    /// The number of channels. The samples of the channels are interleaved.
    pub channels: u16,
}

/// Headerless PCM reader.
///
/// `RawPcmReader` reads interleaved PCM samples that are not in a container, from the position of
/// the stream when the reader is created to the end of it. A raw stream has no header, and no
/// content that identifies it, so the reader cannot be found by probing and is not registered in
/// the default probe: it is created with [`RawPcmReader::try_new`], with the parameters of the
/// stream from the user, a file name extension that implies them, or another source.
///
/// ```no_run
/// use std::fs::File;
///
/// use symphonia_core::formats::FormatOptions;
/// use symphonia_core::io::MediaSourceStream;
/// use symphonia_format_sound::{RawPcmParams, RawPcmReader, RawPcmSampleFormat};
///
/// let file = File::open("audio.raw").unwrap();
/// let mss = MediaSourceStream::new(Box::new(file), Default::default());
///
/// let params =
///     RawPcmParams { sample_format: RawPcmSampleFormat::S16Le, sample_rate: 44100, channels: 2 };
///
/// let reader = RawPcmReader::try_new(mss, params, FormatOptions::default()).unwrap();
/// ```
pub struct RawPcmReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    stream: PcmStream,
}

impl<'s> RawPcmReader<'s> {
    /// Creates a reader of the stream `mss` from its current position, which has the parameters
    /// `params`.
    pub fn try_new(
        mss: MediaSourceStream<'s>,
        params: RawPcmParams,
        mut opts: FormatOptions,
    ) -> Result<Self> {
        if params.sample_rate == 0 {
            return decode_error("pcm: invalid sample rate");
        }

        let channels = channels_from_count(u32::from(params.channels))?;
        let (codec, bits) = params.sample_format.codec();

        let layout = PcmLayout::from_sample_bits(bits, u32::from(params.channels))
            .ok_or(Error::DecodeError("pcm: invalid channel count"))?;

        let mut codec_params = AudioCodecParameters::new();

        codec_params
            .for_codec(codec)
            .with_sample_rate(params.sample_rate)
            .with_channels(channels)
            .with_max_frames_per_packet(layout.max_frames_per_packet());

        // The PCM decoder defines the width of A-law and mu-law samples itself.
        if codec != CODEC_ID_PCM_ALAW && codec != CODEC_ID_PCM_MULAW {
            codec_params.with_bits_per_coded_sample(bits);
        }

        let start = mss.pos();
        let end = clamp_data_end(&mss, start, None);

        let mut track = Track::new(0);

        track
            .with_codec_params(CodecParameters::Audio(codec_params))
            .with_start_ts(Timestamp::new(0));

        if let Some(end) = end {
            let num_frames = layout.num_frames(end.saturating_sub(start));
            track.with_num_frames(num_frames).with_duration(Duration::new(num_frames));
        }

        Ok(RawPcmReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            metadata: opts.external_data.metadata.take().unwrap_or_default(),
            chapters: opts.external_data.chapters,
            stream: PcmStream { layout, start, end, track_id: 0 },
        })
    }
}

impl FormatReader for RawPcmReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &RAW_FORMAT_INFO
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
