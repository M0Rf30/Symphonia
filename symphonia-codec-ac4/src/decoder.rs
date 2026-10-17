// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, Channels,
    GenericAudioBufferRef, Position,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AC4;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::ctx::{Ac4Core, FrameInfo};

/// The largest number of samples per channel in a frame (`frame_len_base`).
const MAX_FRAME_LEN: usize = 2048;

/// AC-4 audio decoder.
///
/// Decodes the first presentation of an AC-4 elementary stream, either as raw frames (ISO BMFF
/// samples) or as frames wrapped in the `0xAC40`/`0xAC41` sync header (ATSC 3.0, MPEG-TS).
pub struct Ac4Decoder {
    params: AudioCodecParameters,
    core: Box<Ac4Core>,
    buf: AudioBuffer<f32>,
}

impl Ac4Decoder {
    pub fn try_new(params: &AudioCodecParameters, _opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_AC4 {
            return unsupported_error("ac4: invalid codec");
        }

        let rate = params.sample_rate.unwrap_or(48_000);
        let channels = params
            .channels
            .clone()
            .unwrap_or(Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT));

        Ok(Ac4Decoder {
            params: params.clone(),
            core: Box::new(Ac4Core::new()),
            buf: AudioBuffer::new(AudioSpec::new(rate, channels), MAX_FRAME_LEN),
        })
    }

    fn decode_packet(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();

        let Some(info) = self.core.decode_frame(packet.data)?
        else {
            // No I-frame has been seen yet: the frame produces no output.
            return Ok(());
        };

        let spec = AudioSpec::new(info.sample_rate, channels_of(&info)?);
        if self.buf.spec() != &spec {
            self.buf = AudioBuffer::new(spec, MAX_FRAME_LEN);
        }

        let n = info.nb_samples;
        self.buf.grow_capacity(n);
        self.buf.render_uninit(Some(n));

        for ch in 0..info.channels {
            if let Some(plane) = self.buf.plane_mut(ch) {
                self.core.copy_channel(ch, info.channel_mode, &mut plane[..n]);
            }
        }

        Ok(())
    }
}

/// Maps the AC-4 channel mode to the Symphonia channel layout. The decoder emits the channels
/// in ascending channel-position order.
fn channels_of(info: &FrameInfo) -> Result<Channels> {
    let pos = match info.channel_mode {
        0 => Position::FRONT_CENTER,
        1 => Position::FRONT_LEFT | Position::FRONT_RIGHT,
        3 => {
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::SIDE_LEFT
                | Position::SIDE_RIGHT
        }
        4 => {
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::LFE1
                | Position::SIDE_LEFT
                | Position::SIDE_RIGHT
        }
        _ => return unsupported_error("ac4: unsupported channel mode"),
    };
    Ok(Channels::Positioned(pos))
}

impl AudioDecoder for Ac4Decoder {
    fn reset(&mut self) {
        self.core.flush();
        self.buf.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()[0].info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_packet(packet) {
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

impl RegisterableAudioDecoder for Ac4Decoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(Ac4Decoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_AC4, "ac4", "Dolby AC-4")]
    }
}
