// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Symphonia [`AudioDecoder`]/[`RegisterableAudioDecoder`] integration: [`OpusAudioDecoder`]
//! wraps [`crate::multistream::MultistreamDecoder`] to decode an Ogg Opus (or ISO/MP4, or MKV)
//! `CODEC_ID_OPUS` track.
//!
//! # Channel mapping
//!
//! `params.extra_data` (the `OpusHead` identification header bytes, per RFC 7845 section 5.1,
//! however the container transports it) is parsed via [`crate::mapping::OpusHead`], which -- unlike
//! `symphonia_common::xiph::audio::opus::OpusHead` -- exposes the mapping family / stream count /
//! coupled count / mapping table needed for family 1 (>2 channels) and family 255 (arbitrary)
//! layouts. Family 0 (mono/stereo) is handled by [`crate::multistream::MultistreamDecoder`]
//! itself via an implicit identity mapping.
//!
//! For mapping family 1 with 3 to 8 channels the output [`AudioSpec`] is
//! [`Channels::Positioned`] (the RFC 7845 layout), and the "Vorbis channel order" produced by
//! [`crate::multistream::MultistreamDecoder::decode`] is reordered so plane `p` holds the channel
//! at the `p`-th set `Position` bit, exactly as the Vorbis decoder does. Families 2 and 255 are
//! exposed as `Discrete(n)` channels in mapping-table order.
//!
//! # Output level
//!
//! The decoded samples are libopus' unclipped float decode (`opus_decode_float`), so they may
//! exceed +/-1.0. libopus applies its soft clipper (`opus_pcm_soft_clip`) only on its 16-bit
//! output path (what e.g. ffmpeg's `libopus` decoder uses by default); a comparison with such
//! output differs in every packet that overshoots full-scale. Compare against a float decode.
//!
//! # Pre-roll
//!
//! Per RFC 7845 section 4.3, an Opus decoder needs audio *before* a seek target to "warm up"
//! its state (SILK LPC history, CELT MDCT overlap, the post-filter, and the CELT inter-frame
//! energy prediction). `symphonia-format-ogg` and `symphonia-format-mkv` seek back by the
//! recommended 80 ms pre-roll, and the caller decodes and discards the audio up to the requested
//! timestamp after calling [`Self::reset`]. Output after exactly 80 ms is close to, but not
//! bit-exact with, a continuous decode: the decoder state is identical to libopus' (decoding a
//! stream from a cold start matches libopus exactly), and the CELT energy predictor converges
//! geometrically (about 6 dB of SNR per 20 ms frame), reaching bit-exactness after a few hundred
//! milliseconds. SILK is different: its gains and pitch/LTP state are coded relative to the
//! previous frame, so a decoder that starts cold never fully converges to a continuous decode
//! (libopus itself differs by only ~45-50 dB SNR from a continuous decode, measured on a speech
//! stream, however long the pre-roll).

use symphonia_core::audio::{AsGenericAudioBufferRef, AudioBuffer, AudioSpec, Channels, GenericAudioBufferRef, Position};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::decoder::SampleRate;
use crate::mapping::OpusHead;
use crate::multistream::MultistreamDecoder;

/// The largest number of samples a single Opus packet can ever decode to (RFC 6716: 120 ms at
/// 48 kHz), regardless of channel/stream count. C: `Fs/25*3`.
const MAX_OPUS_FRAME_SAMPLES: usize = 5760;

/// Returns the positioned layout, and the plane index of each Vorbis-ordered channel, for a
/// mapping family 1 stream with `channels` (3..=8) channels (RFC 7845 section 5.1.1.2).
fn vorbis_layout(channels: u8) -> (Position, &'static [usize]) {
    use Position as P;

    match channels {
        3 => (P::FRONT_LEFT | P::FRONT_CENTER | P::FRONT_RIGHT, &[0, 2, 1]),
        4 => (P::FRONT_LEFT | P::FRONT_RIGHT | P::REAR_LEFT | P::REAR_RIGHT, &[0, 1, 2, 3]),
        5 => (
            P::FRONT_LEFT | P::FRONT_CENTER | P::FRONT_RIGHT | P::REAR_LEFT | P::REAR_RIGHT,
            &[0, 2, 1, 3, 4],
        ),
        6 => (
            P::FRONT_LEFT
                | P::FRONT_CENTER
                | P::FRONT_RIGHT
                | P::REAR_LEFT
                | P::REAR_RIGHT
                | P::LFE1,
            &[0, 2, 1, 4, 5, 3],
        ),
        7 => (
            P::FRONT_LEFT
                | P::FRONT_CENTER
                | P::FRONT_RIGHT
                | P::SIDE_LEFT
                | P::SIDE_RIGHT
                | P::REAR_CENTER
                | P::LFE1,
            &[0, 2, 1, 5, 6, 4, 3],
        ),
        _ => (
            P::FRONT_LEFT
                | P::FRONT_CENTER
                | P::FRONT_RIGHT
                | P::SIDE_LEFT
                | P::SIDE_RIGHT
                | P::REAR_LEFT
                | P::REAR_RIGHT
                | P::LFE1,
            &[0, 2, 1, 6, 7, 4, 5, 3],
        ),
    }
}

/// Opus decoder, implementing Symphonia's [`AudioDecoder`] trait over
/// [`crate::multistream::MultistreamDecoder`].
pub struct OpusAudioDecoder {
    opts: AudioDecoderOptions,
    params: AudioCodecParameters,
    decoder: MultistreamDecoder,
    channels: u8,
    /// `plane_map[decoded_channel]` is the audio buffer plane the channel is written to.
    plane_map: Vec<usize>,
    /// Interleaved scratch buffer for one packet's decode output, reused across calls.
    scratch: Vec<f32>,
    buf: AudioBuffer<f32>,
}

impl OpusAudioDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_OPUS {
            return unsupported_error("opus: invalid codec");
        }

        let extra_data = match params.extra_data.as_ref() {
            Some(buf) => buf,
            None => return unsupported_error("opus: missing extra data (OpusHead)"),
        };

        let head = match OpusHead::parse(extra_data) {
            Ok(h) => h,
            Err(_) => return decode_error("opus: invalid OpusHead"),
        };

        let mut decoder = match MultistreamDecoder::try_new(SampleRate::Hz48000, head.channel_count, head.mapping.clone()) {
            Ok(d) => d,
            Err(_) => return decode_error("opus: invalid channel mapping"),
        };
        // RFC 7845 section 5.1: `output_gain` (Q7.8, i.e. 1/256th dB, matching `OPUS_SET_GAIN`'s
        // units directly) must be applied by the decoder.
        decoder.set_gain(head.output_gain);

        // Decoding always happens at 48 kHz regardless of the header's informational
        // `input_sample_rate`.
        //
        // Mapping family 1 (Vorbis channel order) with 3..=8 channels is presented with the
        // positioned layout implied by RFC 7845 section 5.1.1.2, and the decoded channels are
        // reordered into the audio buffer's plane order (ascending `Position` bit order) so
        // the data agrees with the `AudioSpec`. All other layouts (families 2 and 255, whose
        // channels have no defined speaker positions) are exposed as `Discrete` channels in
        // table order.
        let (channels, plane_map) = match (head.mapping.family, head.channel_count) {
            (0 | 1, 1) => (Channels::Positioned(Position::FRONT_LEFT), vec![0]),
            (0 | 1, 2) => {
                (Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT), vec![0, 1])
            }
            (1, 3..=8) => {
                let (positions, map) = vorbis_layout(head.channel_count);
                (Channels::Positioned(positions), map.to_vec())
            }
            _ => (
                Channels::Discrete(u16::from(head.channel_count)),
                (0..usize::from(head.channel_count)).collect(),
            ),
        };
        let spec = AudioSpec::new(48_000, channels);

        Ok(OpusAudioDecoder {
            opts: *opts,
            params: params.clone(),
            decoder,
            channels: head.channel_count,
            plane_map,
            scratch: vec![0f32; MAX_OPUS_FRAME_SAMPLES * head.channel_count as usize],
            buf: AudioBuffer::new(spec, MAX_OPUS_FRAME_SAMPLES),
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let ch = self.channels as usize;

        let n = match self.decoder.decode(Some(packet.data), &mut self.scratch, MAX_OPUS_FRAME_SAMPLES, false) {
            Ok(n) => n,
            Err(_) => return decode_error("opus: decode failed"),
        };

        self.buf.clear();
        let scratch = &self.scratch;
        let plane_map = &self.plane_map;
        self.buf.render_with(Some(n), |i, planes| {
            for (c, &p) in plane_map.iter().enumerate() {
                planes[p][i] = scratch[i * ch + c];
            }
            Ok(())
        })?;

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for OpusAudioDecoder {
    fn reset(&mut self) {
        // Opus needs decoder state (SILK LPC/LTP history, CELT MDCT overlap, post-filter) to
        // "warm up"; per RFC 7845 4.3 a seek should ideally re-decode an 80 ms pre-roll window
        // starting from this reset point (see module docs). This resets decoder state cleanly;
        // it's the format reader's responsibility to supply that pre-roll if bit-exactness
        // immediately after a seek matters.
        self.decoder.reset();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs().first().expect("at least one codec registered").info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_inner(packet) {
            self.buf.clear();
            Err(e)
        }
        else {
            Ok(self.buf.as_generic_audio_buffer_ref())
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for OpusAudioDecoder {
    fn try_registry_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(OpusAudioDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_OPUS, "opus", "Opus")]
    }
}
