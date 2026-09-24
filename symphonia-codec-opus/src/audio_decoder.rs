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
//! For channel counts > 2, this decoder uses [`Channels::Discrete`] rather than
//! [`Channels::Positioned`] for the output [`AudioSpec`]: [`crate::multistream::MultistreamDecoder::decode`]
//! already scatters channels into RFC 7845 "Vorbis channel order" (i.e. output channel index `c`
//! already has the semantic role RFC 7845 assigns it for the given channel count), and
//! `Discrete(n)` guarantees plane `c` corresponds to output index `c` with no dependency on
//! `Position`'s bit-value ordering happening to match that convention.
//!
//! # Pre-roll
//!
//! Per RFC 7845 section 4.3, Opus decoders need audio *before* a seek target to "warm up" state
//! (SILK LPC history, CELT MDCT overlap, the post-filter) -- 80 ms is the RFC's recommended
//! pre-roll. Neither `symphonia-format-ogg` nor `symphonia-format-mkv` currently seek back by any
//! pre-roll margin or signal one to the decoder (`FormatReader::seek` seeks to the target packet
//! directly); [`Self::reset`] resets decoder state cleanly on any discontinuity, but the first
//! `80` ms of post-seek output will not be bit-exact with a non-seeking decode until fresh
//! decoder state has "warmed up" on its own (typically inaudible, but not a guarantee). Fixing
//! this precisely requires the demuxer to rewind and re-decode (discarding) a pre-roll window,
//! which is out of scope here -- flagged for the format-reader owners.

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

/// Opus decoder, implementing Symphonia's [`AudioDecoder`] trait over
/// [`crate::multistream::MultistreamDecoder`].
pub struct OpusAudioDecoder {
    opts: AudioDecoderOptions,
    params: AudioCodecParameters,
    decoder: MultistreamDecoder,
    channels: u8,
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
        let channels = if head.channel_count == 1 {
            Channels::Positioned(Position::FRONT_LEFT)
        }
        else if head.channel_count == 2 {
            Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
        }
        else {
            Channels::Discrete(head.channel_count as u16)
        };
        let spec = AudioSpec::new(48_000, channels);

        Ok(OpusAudioDecoder {
            opts: *opts,
            params: params.clone(),
            decoder,
            channels: head.channel_count,
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
        self.buf.render_with(Some(n), |i, planes| {
            for (c, plane) in planes.iter_mut().enumerate() {
                plane[i] = scratch[i * ch + c];
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
