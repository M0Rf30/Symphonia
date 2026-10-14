// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure Rust decoder for Dolby TrueHD and Meridian Lossless Packing (MLP).
//!
//! Each packet must contain exactly one *access unit*, including its 4-byte access unit header
//! (length and timestamp), as stored in MPEG transport streams, Matroska (`A_TRUEHD`/`A_MLP`), and
//! MP4. The stream parameters (sample rate, bit depth, and channel layout) are read from the
//! major sync headers embedded in the stream, therefore, no out-of-band configuration is required.
//!
//! MLP and TrueHD streams are distinguished by the stream type in the major sync header, so
//! this decoder decodes both when registered for `CODEC_ID_TRUEHD`.
//!
//! The decoder always decodes the presentation with the most channels, up to the third substream.
//! The fourth substream of Dolby Atmos streams, which carries non-PCM spatial data, is ignored.

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

mod bits;
mod crc;
mod huffman;
mod layout;
mod mlp;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioSpec, Channels, GenericAudioBufferRef,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::FinalizeResult;
use symphonia_core::codecs::audio::well_known::CODEC_ID_TRUEHD;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use mlp::{MAX_BLOCKSIZE, MlpDecoder};

/// Dolby TrueHD and MLP decoder.
pub struct TrueHdDecoder {
    /// Codec parameters.
    params: AudioCodecParameters,
    /// The bitstream decoder.
    dec: Box<MlpDecoder>,
    /// Output buffer.
    buf: AudioBuffer<i32>,
    /// Decoder options.
    opts: AudioDecoderOptions,
}

impl TrueHdDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        // Verify codec ID.
        if params.codec != CODEC_ID_TRUEHD {
            return unsupported_error("truehd: invalid codec");
        }

        Ok(TrueHdDecoder {
            params: params.clone(),
            dec: MlpDecoder::new(),
            buf: AudioBuffer::default(),
            opts: *opts,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();

        // The decoder buffers nothing. If there is no output, the access unit was skipped
        // because the stream parameters have not been seen yet.
        if !self.dec.decode(packet.data)? {
            return Ok(());
        }

        let spec =
            AudioSpec::new(self.dec.sample_rate(), Channels::Positioned(self.dec.out_channels()));

        // (Re)create the output buffer if the stream parameters have changed.
        if *self.buf.spec() != spec || self.buf.capacity() < self.dec.access_unit_size() {
            self.params
                .with_sample_rate(spec.rate())
                .with_channels(spec.channels().clone())
                .with_bits_per_sample(u32::from(self.dec.bits_per_sample()));

            self.buf = AudioBuffer::new(spec, MAX_BLOCKSIZE);
        }

        self.buf.render_uninit(Some(self.dec.num_samples()));
        self.dec.pack_output(&mut self.buf);

        // Trim gaps.
        if self.opts.gapless && (packet.trim_start.get() != 0 || packet.trim_end.get() != 0) {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for TrueHdDecoder {
    fn reset(&mut self) {
        self.dec.reset();
        self.buf.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        // Only one codec is supported.
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

impl RegisterableAudioDecoder for TrueHdDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(TrueHdDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_TRUEHD, "truehd", "Dolby TrueHD / MLP")]
    }
}
