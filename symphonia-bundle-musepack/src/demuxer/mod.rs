// Symphonia Musepack demuxer
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Musepack (SV7/SV8) format reader.
//!
//! The block/stream-info parsing is ported from libmpcdec `mpc_demux.c` / `streaminfo.c`
//! (BSD-3-Clause), see `NOTICE`. The `FormatReader` glue (probing, `Track`/`Packet`
//! construction, tag mapping) is Symphonia-specific and not ported from any external source.

mod sv7;
mod sv8;

use std::sync::Arc;

use symphonia_core::audio::layouts;
use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_MUSEPACK;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::errors::{decode_error, Error, Result, SeekErrorKind};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_MUSEPACK;
use symphonia_core::formats::{MediaInfo, TrackFlags};
use symphonia_core::io::{MediaSourceStream, ReadBytes, ScopedStream};
use symphonia_core::meta::{
    Metadata, MetadataBuilder, MetadataInfo, MetadataLog, RawValue, StandardTag, Tag, well_known,
};
use symphonia_core::support_format;

const MUSEPACK_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_MUSEPACK, short_name: "musepack", long_name: "Musepack" };

/// Sample count and configuration shared by both stream versions, plus everything the
/// `AudioDecoder` needs (packed into `AudioCodecParameters::extra_data`, see [`encode_extra_data`]).
pub(crate) struct StreamInfo {
    pub stream_version: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub max_band: i32,
    pub ms: bool,
    pub block_pwr: u8,
    /// Matches `mpc_decoder_t::samples` (`mpc_decoder_set_streaminfo`): the raw total sample
    /// count fed to the decoder, *before* subtracting `beg_silence` (SV8) / already rounded to
    /// a frame boundary for SV7 true-gapless streams.
    pub decoder_samples: u64,
    /// Leading samples to discard (SV8 `beg_silence`; always `0` for SV7).
    pub beg_silence: u64,
    /// Exact reported (playable, gapless) sample count for `Track` duration purposes
    /// (`mpc_streaminfo_get_length_samples`): `si.samples - si.beg_silence`, computed *before*
    /// SV7's frame-boundary rounding of `decoder_samples`.
    pub display_samples: u64,
    pub gain_title: u16,
    pub peak_title: u16,
    pub gain_album: u16,
    pub peak_album: u16,
    pub encoder: Option<String>,
}

/// Packs the fields an `AudioDecoder` needs to reconstruct its `decoder_core::Decoder`, that
/// aren't otherwise carried by `AudioCodecParameters`.
///
/// Layout (21 bytes, all little-endian): `stream_version(u8)`, `max_band(u8)`, `ms(u8)`,
/// `channels(u8)`, `block_pwr(u8)`, `decoder_samples(u64)`, `beg_silence(u64)`.
pub(crate) fn encode_extra_data(info: &StreamInfo) -> Box<[u8]> {
    let mut buf = Vec::with_capacity(21);
    buf.push(info.stream_version as u8);
    buf.push(info.max_band.clamp(0, 31) as u8);
    buf.push(u8::from(info.ms));
    buf.push(info.channels as u8);
    buf.push(info.block_pwr);
    buf.extend_from_slice(&info.decoder_samples.to_le_bytes());
    buf.extend_from_slice(&info.beg_silence.to_le_bytes());
    buf.into_boxed_slice()
}

fn raw_gain_db(raw: u16) -> f64 {
    f64::from(raw as i16) / 256.0
}

fn raw_peak_db(raw: u16) -> f64 {
    f64::from(raw) / 256.0
}

fn push_replaygain_tags(builder: &mut MetadataBuilder, info: &StreamInfo) {
    if info.gain_title != 0 {
        let db = raw_gain_db(info.gain_title);
        let value = format!("{:.2} dB", db);
        builder.add_tag(Tag::new_from_parts(
            "REPLAYGAIN_TRACK_GAIN",
            RawValue::from(value.clone()),
            Some(StandardTag::ReplayGainTrackGain(Arc::new(value))),
        ));
    }
    if info.peak_title != 0 {
        let db = raw_peak_db(info.peak_title);
        let value = format!("{:.2} dB", db);
        builder.add_tag(Tag::new_from_parts(
            "REPLAYGAIN_TRACK_PEAK",
            RawValue::from(value.clone()),
            Some(StandardTag::ReplayGainTrackPeak(Arc::new(value))),
        ));
    }
    if info.gain_album != 0 {
        let db = raw_gain_db(info.gain_album);
        let value = format!("{:.2} dB", db);
        builder.add_tag(Tag::new_from_parts(
            "REPLAYGAIN_ALBUM_GAIN",
            RawValue::from(value.clone()),
            Some(StandardTag::ReplayGainAlbumGain(Arc::new(value))),
        ));
    }
    if info.peak_album != 0 {
        let db = raw_peak_db(info.peak_album);
        let value = format!("{:.2} dB", db);
        builder.add_tag(Tag::new_from_parts(
            "REPLAYGAIN_ALBUM_PEAK",
            RawValue::from(value.clone()),
            Some(StandardTag::ReplayGainAlbumPeak(Arc::new(value))),
        ));
    }
    if let Some(enc) = &info.encoder {
        builder.add_tag(Tag::new_from_parts(
            "ENCODER",
            RawValue::from(enc.clone()),
            Some(StandardTag::Encoder(Arc::new(enc.clone()))),
        ));
    }
}

enum Inner {
    Sv7(sv7::Sv7State),
    Sv8(sv8::Sv8State),
}

/// Musepack (SV7/SV8) format reader (demuxer).
pub struct MpcReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    info: StreamInfo,
    inner: Inner,
    next_ts: i64,
}

impl<'s> MpcReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, mut opts: FormatOptions) -> Result<Self> {
        let external = opts.external_data.metadata.take().unwrap_or_default();

        let magic = mss.read_quad_bytes().map_err(Error::IoError)?;

        let (info, inner) = if &magic[..3] == b"MP+" {
            let (info, state) = sv7::read_header(&mut mss, magic[3])?;
            (info, Inner::Sv7(state))
        }
        else if &magic == b"MPCK" {
            let (info, state) = sv8::read_header(&mut mss)?;
            (info, Inner::Sv8(state))
        }
        else {
            return decode_error("musepack: not a Musepack stream");
        };

        if info.sample_rate == 0
            || info.channels == 0
            || info.channels > 2
            || !(0..32).contains(&info.max_band)
        {
            return decode_error("musepack: invalid stream header");
        }

        let channel_layout = if info.channels == 1 {
            layouts::CHANNEL_LAYOUT_MONO
        }
        else {
            layouts::CHANNEL_LAYOUT_STEREO
        };

        let mut codec_params = AudioCodecParameters::new();
        codec_params
            .for_codec(CODEC_ID_MUSEPACK)
            .with_sample_rate(info.sample_rate)
            .with_sample_format(SampleFormat::F32)
            .with_bits_per_sample(32)
            .with_channels(channel_layout)
            .with_max_frames_per_packet(
                (1u64 << info.block_pwr) * crate::decoder_core::FRAME_LENGTH as u64,
            )
            .with_extra_data(encode_extra_data(&info));

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));
        track.with_num_frames(info.display_samples);
        // `with_codec_params` derives a `TimeBase` of `1 / sample_rate` from the audio codec
        // parameters when one isn't already set, so a duration in that timebase's ticks is
        // exactly the sample count.
        track.with_duration(Duration::new(info.display_samples));
        track.with_flags(TrackFlags::DEFAULT);

        let media_info = MediaInfo::from_track(&track);

        let mut meta_builder = MetadataBuilder::new(MetadataInfo {
            metadata: well_known::METADATA_ID_APEV2,
            short_name: "musepack",
            long_name: "Musepack Stream Info",
        });
        push_replaygain_tags(&mut meta_builder, &info);
        let revision = meta_builder.build();

        let mut metadata = external;
        if !revision.media.tags.is_empty() {
            metadata.push(revision);
        }

        Ok(MpcReader { reader: mss, media_info, tracks: vec![track], metadata, info, inner, next_ts: 0 })
    }
}

impl Scoreable for MpcReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(254))
    }
}

impl ProbeableFormat<'_> for MpcReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(MpcReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[
            support_format!(
                MUSEPACK_FORMAT_INFO,
                &["mpc", "mp+", "mpp"],
                &["audio/x-musepack"],
                &[b"MPCK"]
            ),
            support_format!(
                MUSEPACK_FORMAT_INFO,
                &["mpc", "mp+", "mpp"],
                &["audio/x-musepack"],
                &[b"MP+\x07", b"MP+\x17"]
            ),
        ]
    }
}

impl FormatReader for MpcReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &MUSEPACK_FORMAT_INFO
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
        let data = match &mut self.inner {
            Inner::Sv7(s) => s.next_packet(&mut self.reader)?,
            Inner::Sv8(s) => s.next_packet(&mut self.reader)?,
        };

        let Some(data) = data
        else {
            return Ok(None);
        };

        let frames = match &self.inner {
            Inner::Sv7(_) => crate::decoder_core::FRAME_LENGTH as u64,
            Inner::Sv8(_) => {
                (1u64 << self.info.block_pwr.min(31)) * crate::decoder_core::FRAME_LENGTH as u64
            }
        };

        let pts = self.next_ts;
        self.next_ts += frames as i64;

        Ok(Some(Packet::new(0, Timestamp::new(pts), Duration::new(frames), data)))
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        let time_base = self.tracks.first().and_then(|t| t.time_base);
        let ts = match to {
            SeekTo::Timestamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                let tb = time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        let target_sample = ts.get().max(0) as u64;

        let actual_frames = match &mut self.inner {
            Inner::Sv7(s) => s.seek(&mut self.reader, target_sample)?,
            Inner::Sv8(s) => s.seek(&mut self.reader, target_sample, self.info.block_pwr)?,
        };

        self.next_ts = actual_frames as i64;

        Ok(SeekedTo {
            track_id: 0,
            actual_ts: Timestamp::new(actual_frames as i64),
            required_ts: ts,
        })
    }

    fn into_inner<'s2>(self: Box<Self>) -> MediaSourceStream<'s2>
    where
        Self: 's2,
    {
        self.reader
    }
}
