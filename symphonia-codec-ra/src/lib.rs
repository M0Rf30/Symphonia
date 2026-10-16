// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RealAudio 1.0 (14.4K) and RealAudio 2.0 (28.8K) decoders.
//!
//! * [`Ra144Decoder`] decodes RealAudio 1.0 (`lpcJ`, 14.4 kbit/s, an LPC/VSELP-style codec). One
//!   20-byte frame holds 160 samples; output is signed 16-bit mono.
//! * [`Ra288Decoder`] decodes RealAudio 2.0 (`28_8`, 28.8 kbit/s, a G.728 LD-CELP variant). One
//!   38-byte frame holds 160 samples; output is 32-bit float mono.
//!
//! Both codecs are mono at 8 kHz. Neither has extradata: the decoder takes the stream parameters
//! (sample rate, channels) from the container. A packet holds one or more whole frames; trailing
//! bytes shorter than a frame are ignored. Packets must already be deinterleaved, i.e. hold
//! consecutive coded frames (the RealMedia demuxer undoes the `sub_packet_h` interleaving of
//! RealAudio 2.0 and later).
//!
//! The decoders follow FFmpeg's `ra144dec.c` and `ra288.c` (see `NOTICE`).

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

mod ra144;
mod ra288;
mod tables144;
mod tables288;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
    layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_RA10, CODEC_ID_RA20};
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

/// The sample rate of both codecs.
const SAMPLE_RATE: u32 = 8000;

/// Number of frames the output buffer is initially sized for. Larger packets grow it.
const INITIAL_FRAMES: usize = 64;

/// Validates the stream parameters, returning the audio spec and the completed parameters.
fn check_params(
    params: &AudioCodecParameters,
    codec: symphonia_core::codecs::audio::AudioCodecId,
) -> Result<(AudioSpec, AudioCodecParameters)> {
    if params.codec != codec {
        return unsupported_error("ra: invalid codec");
    }

    // Only mono is defined.
    if let Some(channels) = &params.channels {
        if channels.count() != 1 {
            return unsupported_error("ra: only mono is supported");
        }
    }

    let rate = match params.sample_rate {
        Some(rate) if rate > 0 => rate,
        _ => SAMPLE_RATE,
    };

    let mut params = params.clone();
    params.with_sample_rate(rate).with_channels(layouts::CHANNEL_LAYOUT_MONO);

    Ok((AudioSpec::new(rate, layouts::CHANNEL_LAYOUT_MONO), params))
}

/// RealAudio 1.0 (14.4K) decoder.
pub struct Ra144Decoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<i16>,
    core: ra144::Ra144,
}

impl Ra144Decoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        let (spec, params) = check_params(params, CODEC_ID_RA10)?;
        let buf = AudioBuffer::new(spec, INITIAL_FRAMES * ra144::FRAME_SAMPLES);

        Ok(Ra144Decoder { params, opts: *opts, buf, core: ra144::Ra144::new() })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let frames = packet.data.len() / ra144::FRAME_SIZE;

        if frames == 0 {
            return decode_error("ra: packet is too small");
        }

        // Like the reference decoder, decode every whole frame in the packet.
        let samples = frames * ra144::FRAME_SAMPLES;

        if samples > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), samples);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(samples));

        let Some(out) = self.buf.plane_mut(0)
        else {
            return decode_error("ra: missing output plane");
        };

        for (frame, audio) in packet
            .data
            .chunks_exact(ra144::FRAME_SIZE)
            .zip(out.chunks_exact_mut(ra144::FRAME_SAMPLES))
        {
            self.core.decode_frame(frame, audio)?;
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for Ra144Decoder {
    fn reset(&mut self) {
        self.core = ra144::Ra144::new();
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

impl RegisterableAudioDecoder for Ra144Decoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(Ra144Decoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_RA10, "ra_144", "RealAudio 1.0 (14.4K)")]
    }
}

/// RealAudio 2.0 (28.8K) decoder.
pub struct Ra288Decoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    core: ra288::Ra288,
}

impl Ra288Decoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        let (spec, params) = check_params(params, CODEC_ID_RA20)?;
        let buf = AudioBuffer::new(spec, INITIAL_FRAMES * ra288::FRAME_SAMPLES);

        Ok(Ra288Decoder { params, opts: *opts, buf, core: ra288::Ra288::new() })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let frames = packet.data.len() / ra288::FRAME_SIZE;

        if frames == 0 {
            return decode_error("ra: packet is too small");
        }

        // Like the reference decoder, decode every whole frame in the packet.
        let samples = frames * ra288::FRAME_SAMPLES;

        if samples > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), samples);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(samples));

        let Some(out) = self.buf.plane_mut(0)
        else {
            return decode_error("ra: missing output plane");
        };

        for (frame, audio) in packet
            .data
            .chunks_exact(ra288::FRAME_SIZE)
            .zip(out.chunks_exact_mut(ra288::FRAME_SAMPLES))
        {
            self.core.decode_frame(frame, audio)?;
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for Ra288Decoder {
    fn reset(&mut self) {
        self.core = ra288::Ra288::new();
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

impl RegisterableAudioDecoder for Ra288Decoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(Ra288Decoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_RA20, "ra_288", "RealAudio 2.0 (28.8K)")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(codec: symphonia_core::codecs::audio::AudioCodecId) -> AudioCodecParameters {
        let mut p = AudioCodecParameters::new();
        p.for_codec(codec).with_sample_rate(SAMPLE_RATE);
        p
    }

    fn xorshift(state: &mut u64) -> u8 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state >> 32) as u8
    }

    /// Arbitrary (corrupt) input must never panic.
    #[test]
    fn verify_ra144_random_frames_do_not_panic() {
        let mut dec =
            Ra144Decoder::try_new(&params(CODEC_ID_RA10), &AudioDecoderOptions::default()).unwrap();
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut data = vec![0u8; ra144::FRAME_SIZE * 3 + 7];
        for _ in 0..20000 {
            data.iter_mut().for_each(|b| *b = xorshift(&mut state));
            let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
            let buf = dec.decode_ref(&pkt).unwrap();
            assert_eq!(buf.frames(), 3 * ra144::FRAME_SAMPLES);
        }
    }

    #[test]
    fn verify_ra288_random_frames_do_not_panic() {
        let mut dec =
            Ra288Decoder::try_new(&params(CODEC_ID_RA20), &AudioDecoderOptions::default()).unwrap();
        let mut state = 0x0fed_cba9_8765_4321u64;
        let mut data = vec![0u8; ra288::FRAME_SIZE * 3 + 5];
        for _ in 0..5000 {
            data.iter_mut().for_each(|b| *b = xorshift(&mut state));
            let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
            let buf = dec.decode_ref(&pkt).unwrap();
            assert_eq!(buf.frames(), 3 * ra288::FRAME_SAMPLES);
        }
    }

    #[test]
    fn verify_short_packets_are_errors() {
        let mut d144 =
            Ra144Decoder::try_new(&params(CODEC_ID_RA10), &AudioDecoderOptions::default()).unwrap();
        let mut d288 =
            Ra288Decoder::try_new(&params(CODEC_ID_RA20), &AudioDecoderOptions::default()).unwrap();
        let data = [0u8; 37];
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data[..19]);
        assert!(d144.decode_ref(&pkt).is_err());
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
        assert!(d288.decode_ref(&pkt).is_err());
    }

    #[test]
    fn verify_wrong_codec_and_stereo_rejected() {
        assert!(
            Ra144Decoder::try_new(&params(CODEC_ID_RA20), &AudioDecoderOptions::default()).is_err()
        );
        let mut p = params(CODEC_ID_RA10);
        p.with_channels(layouts::CHANNEL_LAYOUT_STEREO);
        assert!(Ra144Decoder::try_new(&p, &AudioDecoderOptions::default()).is_err());
    }
}
