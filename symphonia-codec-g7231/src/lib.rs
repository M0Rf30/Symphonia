// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ITU-T G.723.1 dual-rate speech decoder.
//!
//! G.723.1 is an 8 kHz mono speech codec with two rates: 6.3 kbit/s (MP-MLQ, 24-byte frames)
//! and 5.3 kbit/s (ACELP, 20-byte frames). Annex A adds 4-byte SID frames and 1-byte
//! untransmitted frames (silence compression). Every frame decodes to 240 samples (30 ms); the
//! frame type and rate are given by the two least significant bits of its first byte.
//!
//! This is a bit-exact fixed-point port of the FFmpeg `g723_1` decoder (see `NOTICE`), including
//! the postfilter, comfort noise generation and frame erasure concealment. A packet holds one or
//! more whole frames; a trailing partial frame is ignored.
#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
// Disable to better express the specification.
#![allow(clippy::collapsible_else_if)]
// The fixed-point code indexes several arrays in lock-step, exactly like the reference.
#![allow(clippy::needless_range_loop)]

mod celp;
mod channel;
mod cng;
mod lsp;
mod postfilter;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
    layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_G723_1;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use channel::{Channel, FRAME_SIZE};

pub(crate) const SUBFRAMES: usize = 4;
pub(crate) const SUBFRAME_LEN: usize = 60;
/// Number of samples per frame.
pub(crate) const FRAME_LEN: usize = SUBFRAME_LEN * SUBFRAMES;
pub(crate) const LPC_ORDER: usize = 10;
pub(crate) const PITCH_MIN: usize = 18;
pub(crate) const PITCH_MAX: usize = PITCH_MIN + 127;
pub(crate) const PITCH_ORDER: usize = 5;
pub(crate) const GRID_SIZE: usize = 2;
pub(crate) const PULSE_MAX: usize = 6;

/// The sample rate of G.723.1.
const SAMPLE_RATE: u32 = 8000;

/// Number of frames the output buffer is initially sized for. Larger packets grow it.
const INITIAL_FRAMES: usize = 4;

/// G.723.1 decoder.
pub struct G7231Decoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<i16>,
    channel: Box<Channel>,
}

impl G7231Decoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_G723_1 {
            return unsupported_error("g723.1: invalid codec");
        }

        // Only mono is defined.
        if let Some(channels) = &params.channels {
            if channels.count() != 1 {
                return unsupported_error("g723.1: only mono is supported");
            }
        }

        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            _ => SAMPLE_RATE,
        };

        let spec = AudioSpec::new(rate, layouts::CHANNEL_LAYOUT_MONO);
        let buf = AudioBuffer::new(spec, INITIAL_FRAMES * FRAME_LEN);

        let mut params = params.clone();
        params.with_sample_rate(rate).with_channels(layouts::CHANNEL_LAYOUT_MONO);

        Ok(G7231Decoder { params, opts: *opts, buf, channel: Box::new(Channel::new()) })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;

        // Count the whole frames in the packet.
        let mut frames = 0;
        let mut pos = 0;
        while let Some(&first) = data.get(pos) {
            let size = FRAME_SIZE[usize::from(first & 3)];
            if pos + size > data.len() {
                break;
            }
            pos += size;
            frames += 1;
        }

        if frames == 0 {
            return decode_error("g723.1: packet is too small");
        }

        if frames * FRAME_LEN > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), frames * FRAME_LEN);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(frames * FRAME_LEN));

        let out = self.buf.plane_mut(0).expect("mono plane");

        let mut pos = 0;
        for chunk in out.chunks_exact_mut(FRAME_LEN) {
            let size = FRAME_SIZE[usize::from(data[pos] & 3)];
            self.channel.decode_frame(&data[pos..pos + size], chunk);
            pos += size;
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for G7231Decoder {
    fn reset(&mut self) {
        *self.channel = Channel::new();
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

impl RegisterableAudioDecoder for G7231Decoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(G7231Decoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_G723_1, "g723.1", "G.723.1")]
    }
}
