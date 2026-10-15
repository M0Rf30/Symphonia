// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AMR-NB (Adaptive Multi-Rate narrowband) speech decoder.
//!
//! Decodes all eight speech modes (4.75 - 12.2 kbit/s) of 3GPP TS 26.071/26.090 to 8 kHz PCM,
//! 160 samples per frame. The synthesis is a port of FFmpeg's floating point `amrnb` decoder
//! (see `NOTICE`).
//!
//! # Packet layout
//!
//! One packet holds one AMR frame per channel in the RFC 4867 "storage" (file) format, as stored
//! in `.amr` files and in the `samr` sample entry of MP4/3GP files:
//!
//! ```text
//! [TOC byte: P(1) FT(4) Q(1) P(2)] [speech bits, MSB first, zero padded to a whole byte]
//! ```
//!
//! `FT` is the frame type (0-7 speech modes 4.75-12.2 kbit/s, 8 SID, 15 no data) and `Q` the
//! frame quality bit. The size of the speech bits is given by `FT` (12, 13, 15, 17, 19, 20, 26,
//! 31 bytes for the speech modes, 5 for SID).

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
// Index loops mirror the reference implementation.
#![allow(clippy::needless_range_loop)]

mod celp;
mod core;
mod lsp;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef, layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AMR_NB;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::core::{Core, FRAME_SAMPLES};
use crate::tables::FRAME_SIZES_NB;

/// Sample rate of AMR-NB.
const SAMPLE_RATE: u32 = 8000;
/// Maximum number of channels (FFmpeg supports up to two interleaved frames per packet).
const MAX_CHANNELS: usize = 2;

/// AMR-NB decoder.
pub struct AmrNbDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    channels: Vec<Core>,
    scratch: [f32; FRAME_SAMPLES],
}

impl AmrNbDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_AMR_NB {
            return unsupported_error("amrnb: invalid codec");
        }

        let (layout, count) = match params.channels.as_ref().map(|c| c.count()) {
            None | Some(1) => (layouts::CHANNEL_LAYOUT_MONO, 1),
            Some(2) => (layouts::CHANNEL_LAYOUT_STEREO, 2),
            Some(_) => return unsupported_error("amrnb: more than 2 channels are not supported"),
        };
        debug_assert!(count <= MAX_CHANNELS);

        // The codec is defined for 8 kHz only; like FFmpeg, a missing rate defaults to it.
        match params.sample_rate {
            None | Some(SAMPLE_RATE) => (),
            Some(_) => return unsupported_error("amrnb: only 8 kHz is supported"),
        }

        let spec = AudioSpec::new(SAMPLE_RATE, layout.clone());
        let buf = AudioBuffer::new(spec, FRAME_SAMPLES);

        let mut params = params.clone();
        params
            .with_sample_rate(SAMPLE_RATE)
            .with_channels(layout)
            .with_max_frames_per_packet(FRAME_SAMPLES as u64);

        Ok(AmrNbDecoder {
            params,
            opts: *opts,
            buf,
            channels: (0..count).map(|_| *Core::new()).collect(),
            scratch: [0.0; FRAME_SAMPLES],
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();
        self.buf.render_uninit(Some(FRAME_SAMPLES));

        let mut data = packet.data;

        for ch in 0..self.channels.len() {
            let Some((&toc, rest)) = data.split_first()
            else {
                return decode_error("amrnb: missing frame");
            };

            let mode = (toc >> 3) & 0x0f;

            // Frame types 9..=14 and 15 (no data) carry no speech.
            let Some(&size) = FRAME_SIZES_NB.get(usize::from(mode))
            else {
                return decode_error("amrnb: no data");
            };
            let size = usize::from(size);

            if rest.len() < size {
                return decode_error("amrnb: truncated frame");
            }

            if mode == core::MODE_DTX {
                return unsupported_error("amrnb: SID frames are not supported");
            }

            self.channels[ch].decode_speech(mode, &rest[..size], &mut self.scratch)?;

            let Some(plane) = self.buf.plane_mut(ch)
            else {
                return decode_error("amrnb: invalid channel");
            };
            plane.copy_from_slice(&self.scratch);

            data = &rest[size..];
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for AmrNbDecoder {
    fn reset(&mut self) {
        for c in self.channels.iter_mut() {
            *c = *Core::new();
        }
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

impl RegisterableAudioDecoder for AmrNbDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(AmrNbDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_AMR_NB, "amrnb", "AMR-NB (Adaptive Multi-Rate Narrowband)")]
    }
}
