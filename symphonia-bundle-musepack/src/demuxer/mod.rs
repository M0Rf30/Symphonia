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

mod seek_table;
mod sv7;
mod sv8;

/// First byte of every packet's data (SV7: followed by the realigned frame bits, SV8: by the
/// `AP` block payload), for a packet that needs no decoder state.
pub(crate) const PACKET_TAG_PLAIN: u8 = 0;
/// First byte of an SV7 packet's data: an encoded `Sv7Sync` follows, then the frame bits. Set on
/// the first packet after a seek.
pub(crate) const PACKET_TAG_SYNC: u8 = 1;
/// First byte of an SV8 packet's data: the noise generator state (`r1`, `r2`, little-endian
/// `u32`s) follows, then the packet payload. Set on the first packet after a seek.
pub(crate) const PACKET_TAG_NOISE: u8 = 2;

use std::sync::Arc;

use symphonia_core::audio::layouts;
use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_MUSEPACK;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::errors::{decode_error, seek_error, Error, Result, SeekErrorKind};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_MUSEPACK;
use symphonia_core::formats::{MediaInfo, TrackFlags};
use symphonia_core::io::{MediaSourceStream, ReadBytes, ScopedStream};
use symphonia_core::meta::{
    Metadata, MetadataBuilder, MetadataInfo, MetadataLog, RawValue, StandardTag, Tag, well_known,
};
use symphonia_core::support_format;

use crate::decoder_core::{FRAME_LENGTH, SYNTH_DELAY};

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
    /// Total number of 1152-sample frames in the stream, or `None` if the length is unknown.
    pub total_frames: Option<u64>,
    pub gain_title: u16,
    pub peak_title: u16,
    pub gain_album: u16,
    pub peak_album: u16,
    pub encoder: Option<String>,
}

impl StreamInfo {
    /// Number of frames in one packet (SV8 packs `2^block_pwr` frames into one `AP` block).
    pub fn block_frames(&self) -> u64 {
        1u64 << self.block_pwr.min(20)
    }

    /// Number of sample-frames the decoder produces from one packet.
    pub fn block_samples(&self) -> u64 {
        self.block_frames() * FRAME_LENGTH as u64
    }

    /// Number of leading sample-frames the decoder produces that are not part of the audio:
    /// the synthesis filter's delay plus (SV8) the encoder's `beg_silence`.
    pub fn skip_samples(&self) -> u64 {
        u64::from(SYNTH_DELAY) + self.beg_silence
    }

    /// Builds packet number `block` carrying `data`.
    ///
    /// Timestamps are in the *output* timeline, in which sample 0 is the first audible sample
    /// (`pts` is therefore negative for packets that contain encoder delay). `trim_start` /
    /// `trim_end` express the delay, the padding beyond the stream's length, and (for
    /// `trim_until`, the decoder-timeline position set by a seek) everything before the seek
    /// target, so that `dur + trim_start + trim_end` is always exactly the number of samples the
    /// decoder produces for the packet. Returns `None` if the packet lies beyond the stream's
    /// declared length.
    pub fn packet(&self, block: u64, trim_until: u64, data: Vec<u8>) -> Option<Packet> {
        let frame_len = FRAME_LENGTH as u64;
        let first_frame = block * self.block_frames();
        let frames = match self.total_frames {
            Some(total) if first_frame >= total => return None,
            Some(total) => self.block_frames().min(total - first_frame),
            None => self.block_frames(),
        };

        let block_dur = frames * frame_len;
        let start = first_frame * frame_len;
        let skip = self.skip_samples();

        let delay = skip.saturating_sub(start);
        let seek = trim_until.saturating_sub(start);
        let trim_start = delay.max(seek).min(block_dur);
        let trim_end = match self.total_frames {
            Some(_) => (start + block_dur).saturating_sub(skip + self.display_samples),
            None => 0,
        }
        .min(block_dur - trim_start);

        let pts = Timestamp::new(start as i64 - skip as i64);
        let mut packet =
            Packet::new(0, pts, Duration::new(block_dur - trim_start - trim_end), data);
        packet.trim_start = Duration::new(trim_start);
        packet.trim_end = Duration::new(trim_end);
        Some(packet)
    }
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
    /// Index of the packet `next_packet` returns next.
    next_block: u64,
    /// Everything the decoder produces before this position (in the decoder's own timeline,
    /// i.e. *including* the encoder delay) is trimmed from the packets; set by a seek.
    trim_until: u64,
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

        Ok(MpcReader {
            reader: mss,
            media_info,
            tracks: vec![track],
            metadata,
            info,
            inner,
            next_block: 0,
            trim_until: 0,
        })
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

        let Some(packet) = self.info.packet(self.next_block, self.trim_until, data) else {
            // Past the stream's declared length: trailing data is not audio.
            return Ok(None);
        };
        self.next_block += 1;

        Ok(Some(packet))
    }

    fn seek(&mut self, mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        let time_base = self.tracks.first().and_then(|t| t.time_base);
        let required_ts = match to {
            SeekTo::Timestamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                let tb = time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        // Position in the output (gapless) timeline; anything before the start seeks to it.
        let target = required_ts.get().max(0) as u64;

        if self.info.total_frames.is_some() && target > self.info.display_samples {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // The same position in the decoder's timeline, which leads the output by the encoder
        // delay (and SV8's leading silence), and the packet that contains it.
        let block_samples = self.info.block_samples();
        let target_decoded = target + self.info.skip_samples();
        let mut block = target_decoded / block_samples;
        if let Some(total) = self.info.total_frames {
            let last_block = total.div_ceil(self.info.block_frames()).saturating_sub(1);
            block = block.min(last_block);
        }

        // The synthesis filter bank has a memory of a few sub-bands, and SV7 scale factors are
        // delta coded across frames, so decoding can only be started cleanly one packet early;
        // that packet is decoded (it is trimmed away entirely by `InfoExt::packet`) to warm the
        // decoder up.
        let start_block = block.saturating_sub(1);
        match &mut self.inner {
            Inner::Sv7(s) => s.seek_frame(start_block)?,
            Inner::Sv8(s) => s.seek_block(&mut self.reader, start_block, self.info.block_pwr)?,
        }
        self.next_block = start_block;

        let first_decoded = block * block_samples;
        let (trim_until, actual) = match mode {
            // Deliver exactly from the requested sample: the first packet is trimmed to it.
            SeekMode::Accurate => (target_decoded, target),
            // Deliver from the start of the packet containing the requested sample.
            SeekMode::Coarse => {
                (first_decoded, first_decoded.saturating_sub(self.info.skip_samples()))
            }
        };
        self.trim_until = trim_until;

        Ok(SeekedTo { track_id: 0, actual_ts: Timestamp::new(actual as i64), required_ts })
    }

    fn into_inner<'s2>(self: Box<Self>) -> MediaSourceStream<'s2>
    where
        Self: 's2,
    {
        self.reader
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(block_pwr: u8, beg_silence: u64, samples: u64) -> StreamInfo {
        StreamInfo {
            stream_version: 8,
            sample_rate: 44100,
            channels: 2,
            max_band: 20,
            ms: true,
            block_pwr,
            decoder_samples: samples,
            beg_silence,
            display_samples: samples - beg_silence,
            total_frames: Some((samples + u64::from(SYNTH_DELAY)).div_ceil(FRAME_LENGTH as u64)),
            gain_title: 0,
            peak_title: 0,
            gain_album: 0,
            peak_album: 0,
            encoder: None,
        }
    }

    fn trims(info: &StreamInfo, block: u64, trim_until: u64) -> Option<(i64, u64, u64, u64)> {
        info.packet(block, trim_until, vec![]).map(|p| {
            (p.pts.get(), p.dur.get(), p.trim_start.get(), p.trim_end.get())
        })
    }

    #[test]
    fn packets_trim_delay_and_padding() {
        // 3 packets of one frame: 3456 decoded samples, 3456 - 481 = 2975 audible.
        let i = info(0, 0, 2975);
        assert_eq!(trims(&i, 0, 0), Some((-481, 671, 481, 0)));
        assert_eq!(trims(&i, 1, 0), Some((671, 1152, 0, 0)));
        assert_eq!(trims(&i, 2, 0), Some((1823, 1152, 0, 0)));
        assert_eq!(trims(&i, 3, 0), None);

        // Fewer audible samples: the padding is trimmed from the end of the last packet.
        let i = info(0, 0, 2900);
        assert_eq!(trims(&i, 2, 0), Some((1823, 1077, 0, 75)));

        // One more packet is needed (the delay pushes the tail into a further frame).
        let i = info(0, 0, 2976);
        assert_eq!(trims(&i, 3, 0), Some((2975, 1, 0, 1151)));
    }

    #[test]
    fn packets_trim_leading_silence_across_blocks() {
        // Blocks of 4 frames; 5000 samples of leading silence span more than one block.
        let i = info(2, 5000, 20000);
        let block = 4 * FRAME_LENGTH as u64;
        assert_eq!(trims(&i, 0, 0).map(|t| (t.1, t.2)), Some((0, block)));
        assert_eq!(trims(&i, 1, 0).map(|t| (t.1, t.2)), Some((block - (5481 - block), 5481 - block)));
        // The last block holds only the frames that exist.
        let (_, dur, ts, te) = trims(&i, 4, 0).unwrap();
        assert_eq!(dur + ts + te, (i.total_frames.unwrap() - 16) * FRAME_LENGTH as u64);
    }

    #[test]
    fn seek_trim_discards_preroll_and_leading_samples() {
        let i = info(0, 0, 100_000);
        let target_decoded = 5 * 1152 + 100;
        // The pre-roll packet is dropped entirely, the target packet from its start up to the
        // target, everything after untouched.
        assert_eq!(trims(&i, 4, target_decoded), Some((4 * 1152 - 481, 0, 1152, 0)));
        assert_eq!(trims(&i, 5, target_decoded), Some((5 * 1152 - 481, 1052, 100, 0)));
        assert_eq!(trims(&i, 6, target_decoded), Some((6 * 1152 - 481, 1152, 0, 0)));
    }
}
