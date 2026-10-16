// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure Rust decoders for the QDesign Music Codecs, QDM2 and QDMC (the "QDesign Music Codec 1").
//!
//! Both codecs are undocumented. The decoders are ports of FFmpeg's `libavcodec/qdm2.c` and
//! `libavcodec/qdmc.c` (see `NOTICE`). The codec's extra data is the QuickTime sound sample
//! description extension that contains the `frma` and `QDCA` atoms, as stored in the `wave`
//! atom of MP4/MOV files.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::comparison_chain)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::needless_late_init)]

mod bits;
#[allow(clippy::excessive_precision)]
mod mpa;
mod qdm2;
#[allow(clippy::excessive_precision, clippy::large_const_arrays)]
mod qdm2_tables;
mod qdmc;
#[allow(clippy::excessive_precision, clippy::large_const_arrays)]
mod qdmc_tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, Channels,
    GenericAudioBufferRef, layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_QDM2, CODEC_ID_QDMC};
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use qdm2::Qdm2;
use qdmc::Qdmc;

/// The decoder core, one of the two QDesign codecs.
enum Core {
    Qdm2(Box<Qdm2>),
    Qdmc(Box<Qdmc>),
}

impl Core {
    fn channels(&self) -> usize {
        match self {
            Core::Qdm2(core) => core.channels(),
            Core::Qdmc(core) => core.channels(),
        }
    }

    fn sample_rate(&self) -> u32 {
        match self {
            Core::Qdm2(core) => core.sample_rate(),
            Core::Qdmc(core) => core.sample_rate(),
        }
    }

    fn frames_per_block(&self) -> usize {
        match self {
            Core::Qdm2(core) => core.frames_per_block(),
            Core::Qdmc(core) => core.frames_per_block(),
        }
    }

    fn block_size(&self) -> usize {
        match self {
            Core::Qdm2(core) => core.block_size(),
            Core::Qdmc(core) => core.block_size(),
        }
    }

    fn decode_block(&mut self, buf: &[u8], out: &mut Vec<i16>) -> Result<()> {
        match self {
            Core::Qdm2(core) => core.decode_block(buf, out),
            Core::Qdmc(core) => core.decode_block(buf, out),
        }
    }
}

/// The QDesign decoder, for QDM2 and QDMC.
pub struct QdesignDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<i16>,
    pcm: Vec<i16>,
    core: Core,
}

fn layout_for(channels: usize) -> Result<Channels> {
    match channels {
        1 => Ok(layouts::CHANNEL_LAYOUT_MONO),
        2 => Ok(layouts::CHANNEL_LAYOUT_STEREO),
        _ => unsupported_error("qdesign: unsupported channel count"),
    }
}

impl QdesignDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        let extra = match params.extra_data.as_deref() {
            Some(extra) => extra,
            None => return unsupported_error("qdesign: extra data is required"),
        };

        let core = if params.codec == CODEC_ID_QDM2 {
            Core::Qdm2(Qdm2::new(extra)?)
        }
        else if params.codec == CODEC_ID_QDMC {
            Core::Qdmc(Qdmc::new(extra)?)
        }
        else {
            return unsupported_error("qdesign: invalid codec");
        };

        // As FFmpeg, the stream parameters are taken from the extra data.
        let channels = layout_for(core.channels())?;
        let spec = AudioSpec::new(core.sample_rate(), channels.clone());
        let buf = AudioBuffer::new(spec, core.frames_per_block());

        let mut params = params.clone();
        params.with_sample_rate(core.sample_rate()).with_channels(channels);

        Ok(QdesignDecoder { params, opts: *opts, buf, pcm: Vec::new(), core })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let block_size = self.core.block_size();
        let blocks = packet.data.len() / block_size;

        if blocks == 0 {
            return decode_error("qdesign: packet is shorter than a block");
        }

        // Like the reference decoder, decode every whole block in the packet.
        self.pcm.clear();
        for block in packet.data.chunks_exact(block_size) {
            self.core.decode_block(block, &mut self.pcm)?;
        }

        let channels = self.core.channels();
        let frames = self.pcm.len() / channels;

        if frames > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), frames);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(frames));
        self.buf.copy_from_slice_interleaved(&self.pcm);

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for QdesignDecoder {
    fn reset(&mut self) {
        // The extra data was validated by the constructor.
        let extra = self.params.extra_data.as_deref();
        if let Some(extra) = extra {
            match &mut self.core {
                Core::Qdm2(core) => {
                    if let Ok(fresh) = Qdm2::new(extra) {
                        *core = fresh;
                    }
                }
                Core::Qdmc(core) => {
                    if let Ok(fresh) = Qdmc::new(extra) {
                        *core = fresh;
                    }
                }
            }
        }
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()[0].info
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

impl RegisterableAudioDecoder for QdesignDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(QdesignDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[
            support_audio_codec!(CODEC_ID_QDM2, "qdm2", "QDesign Music Codec 2"),
            support_audio_codec!(CODEC_ID_QDMC, "qdmc", "QDesign Music Codec 1"),
        ]
    }
}
