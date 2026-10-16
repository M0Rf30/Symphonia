// Symphonia Shorten decoder
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `AudioDecoder` glue around [`crate::core::Core`]: packet framing and `AudioBuffer` rendering.
//! The decoding algorithm itself lives in `core.rs`.

use symphonia_core::audio::sample::Sample;
use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, Channels,
    GenericAudioBufferRef, layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_SHORTEN;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Error, Result, decode_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::bits::BitReader;
use crate::core::{Core, Group};
use crate::header::{FileType, Header};
use crate::reader::{TAG_START_BIT_MASK, TAG_STATE};

/// The decoded samples, in the sample type of the stream's file type.
enum Output {
    U8(AudioBuffer<u8>),
    S8(AudioBuffer<i8>),
    S16(AudioBuffer<i16>),
    U16(AudioBuffer<u16>),
}

/// Renders the planar `i32` samples of `core` into `buf`, converting (and clipping) each sample
/// with `convert`.
fn render<S: Sample>(
    buf: &mut AudioBuffer<S>,
    core: &Core,
    convert: impl Fn(i32) -> S,
) -> Result<()> {
    buf.clear();
    buf.render_uninit(Some(core.blocksize()));
    for chan in 0..buf.spec().channels().count() {
        let Some(plane) = buf.plane_mut(chan) else {
            return decode_error("shorten: missing output plane");
        };
        for (out, &sample) in plane.iter_mut().zip(core.samples(chan)) {
            *out = convert(sample);
        }
    }
    Ok(())
}

/// Shorten decoder.
pub struct ShortenDecoder {
    params: AudioCodecParameters,
    core: Core,
    /// FFmpeg byte-swaps the 16-bit samples of AIFC sourced streams.
    swap: bool,
    out: Output,
}

impl ShortenDecoder {
    pub fn try_new(params: &AudioCodecParameters, _opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_SHORTEN {
            return decode_error("shorten: invalid codec");
        }
        let extra =
            params.extra_data.as_ref().ok_or(Error::DecodeError("shorten: missing extra data"))?;
        let (header, swap) = Header::from_extra_data(extra)?;

        let sample_rate =
            params.sample_rate.ok_or(Error::DecodeError("shorten: missing sample rate"))?;
        let channels = match header.channels {
            1 => layouts::CHANNEL_LAYOUT_MONO,
            2 => layouts::CHANNEL_LAYOUT_STEREO,
            n => Channels::Discrete(n as u16),
        };
        let spec = AudioSpec::new(sample_rate, channels);
        let capacity = header.blocksize;
        let out = match header.file_type {
            FileType::U8 => Output::U8(AudioBuffer::new(spec, capacity)),
            FileType::S8 => Output::S8(AudioBuffer::new(spec, capacity)),
            FileType::S16 => Output::S16(AudioBuffer::new(spec, capacity)),
            FileType::U16 => Output::U16(AudioBuffer::new(spec, capacity)),
        };

        Ok(ShortenDecoder { params: params.clone(), core: Core::new(&header), swap, out })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        // Every packet starts with a tag byte: the bit offset of the first command in the next
        // byte and, after a seek, a flag that the decoder state follows.
        let (&tag, mut data) =
            packet.data.split_first().ok_or(Error::DecodeError("shorten: empty packet"))?;
        if tag & TAG_STATE != 0 {
            self.core.read_state(data)?;
            data = &data[self.core.state_len()..];
        }

        let mut br = BitReader::new(data, usize::from(tag & TAG_START_BIT_MASK));
        match self.core.decode_group(&mut br, true)? {
            Group::Audio => (),
            Group::Quit => return decode_error("shorten: unexpected end of stream command"),
        }

        let swap = self.swap;
        match &mut self.out {
            Output::U8(buf) => render(buf, &self.core, |v| v.clamp(0, 0xff) as u8),
            Output::S8(buf) => render(buf, &self.core, |v| v.clamp(-0x80, 0x7f) as i8),
            Output::S16(buf) => render(buf, &self.core, |v| {
                let v = v.clamp(-0x8000, 0x7fff) as i16;
                if swap { v.swap_bytes() } else { v }
            }),
            Output::U16(buf) => render(buf, &self.core, |v| {
                let v = v.clamp(0, 0xffff) as u16;
                if swap { v.swap_bytes() } else { v }
            }),
        }
    }

    fn clear(&mut self) {
        match &mut self.out {
            Output::U8(buf) => buf.clear(),
            Output::S8(buf) => buf.clear(),
            Output::S16(buf) => buf.clear(),
            Output::U16(buf) => buf.clear(),
        }
    }

    fn generic(&self) -> GenericAudioBufferRef<'_> {
        match &self.out {
            Output::U8(buf) => buf.as_generic_audio_buffer_ref(),
            Output::S8(buf) => buf.as_generic_audio_buffer_ref(),
            Output::S16(buf) => buf.as_generic_audio_buffer_ref(),
            Output::U16(buf) => buf.as_generic_audio_buffer_ref(),
        }
    }
}

impl AudioDecoder for ShortenDecoder {
    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs().first().expect("at least one codec registered").info
    }

    fn reset(&mut self) {
        // Nothing to restore: the demuxer tags the first packet after a seek with the complete
        // decoder state.
        self.clear();
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_inner(packet) {
            self.clear();
            Err(e)
        } else {
            Ok(self.generic())
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.generic()
    }
}

impl RegisterableAudioDecoder for ShortenDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(ShortenDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_SHORTEN, "shorten", "Shorten")]
    }
}
