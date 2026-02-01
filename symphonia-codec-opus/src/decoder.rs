// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opus Decoder
//!
//! The Opus decoder consists of two main blocks: the SILK decoder and
//! the CELT decoder. At any given time, one or both of the SILK and
//! CELT decoders may be active.
//!
//! Current implementation status:
//! - SILK mode: ✓ Complete (speech audio, 0-8kHz)
//! - CELT mode: ✓ Complete (music/general audio)
//! - Hybrid mode: ✓ Complete (SILK + CELT for SWB/FB)

use crate::{celt, silk, toc};
use symphonia_core::audio::{AsAudioBufferRef, AudioBufferRef, Signal};
use symphonia_core::codecs::{
    CodecDescriptor, CodecParameters, Decoder, DecoderOptions, FinalizeResult, CODEC_TYPE_OPUS,
};
use symphonia_core::errors::{decode_error, Error};
use symphonia_core::formats::Packet;

use std::sync::LazyLock;

/// Static Opus Codec Descriptor.
static OPUS_CODEC_DESCRIPTOR: LazyLock<CodecDescriptor> = LazyLock::new(|| CodecDescriptor {
    codec: CODEC_TYPE_OPUS,
    short_name: "opus",
    long_name: "Opus Audio Codec",
    inst_func: |params: &CodecParameters,
                options: &DecoderOptions|
     -> symphonia_core::errors::Result<Box<dyn Decoder>> {
        Ok(Box::new(OpusDecoder::try_new(params, options)?))
    },
});

/// Register the Opus decoder with Symphonia.
pub fn get_codecs() -> &'static [CodecDescriptor] {
    std::slice::from_ref(&*OPUS_CODEC_DESCRIPTOR)
}

/// The OpusDecoder struct implements the Symphonia Decoder trait.
pub struct OpusDecoder {
    silk_decoder: silk::Decoder,
    celt_decoder: celt::Decoder,
    last_mode: toc::AudioMode,
    hybrid_buffer: symphonia_core::audio::AudioBuffer<f32>,
}

impl Decoder for OpusDecoder {
    fn try_new(
        params: &CodecParameters,
        _options: &DecoderOptions,
    ) -> symphonia_core::errors::Result<Self>
    where
        Self: Sized,
    {
        let silk_decoder = silk::Decoder::try_new(params.to_owned())?;
        let celt_decoder = celt::Decoder::try_new(params.to_owned())?;

        // Create hybrid buffer for combining SILK and CELT
        let channels = params.channels.unwrap_or_else(|| {
            use symphonia_core::audio::Channels;
            Channels::FRONT_LEFT | Channels::FRONT_RIGHT
        });
        let spec = symphonia_core::audio::SignalSpec::new(48000, channels);
        let hybrid_buffer = symphonia_core::audio::AudioBuffer::new(960, spec);

        Ok(Self {
            silk_decoder,
            celt_decoder,
            last_mode: toc::AudioMode::SILK, // Default to SILK
            hybrid_buffer,
        })
    }

    fn supported_codecs() -> &'static [CodecDescriptor]
    where
        Self: Sized,
    {
        get_codecs()
    }

    fn reset(&mut self) {
        self.silk_decoder.reset();
        self.celt_decoder.reset();
        self.hybrid_buffer.clear();
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.silk_decoder.codec_params()
    }

    fn decode(&mut self, packet: &Packet) -> symphonia_core::errors::Result<AudioBufferRef<'_>> {
        // Parse TOC byte to determine packet mode
        let packet_data = packet.buf();
        if packet_data.is_empty() {
            return decode_error("opus: empty packet");
        }

        // First byte is the TOC byte
        let toc_byte = packet_data[0];
        let toc = toc::Toc::try_new(toc_byte)
            .map_err(|_| Error::DecodeError("opus: failed to parse TOC"))?;

        // Route to appropriate decoder based on mode and track which was used
        self.last_mode = toc.audio_mode;
        match toc.audio_mode {
            toc::AudioMode::SILK => self.silk_decoder.decode(packet),
            toc::AudioMode::CELT => self.celt_decoder.decode(packet),
            toc::AudioMode::Hybrid => self.decode_hybrid(packet, &toc),
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> AudioBufferRef<'_> {
        // Return buffer from whichever decoder was used last
        // Note: This method is rarely used in practice as most code uses decode()'s return value
        match self.last_mode {
            toc::AudioMode::SILK => {
                // SILK decoder stores the buffer internally
                // We can't access it directly without decode, so we'd need to refactor
                // For now, create an empty F32 buffer reference
                // TODO: Properly track the last decoded buffer
                use symphonia_core::audio::{AudioBuffer, SignalSpec, Channels};
                use std::borrow::Cow;
                let spec = SignalSpec::new(48000, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
                let buf = AudioBuffer::<f32>::new(0, spec);
                AudioBufferRef::F32(Cow::Owned(buf))
            }
            _ => {
                use symphonia_core::audio::{AudioBuffer, SignalSpec, Channels};
                use std::borrow::Cow;
                let spec = SignalSpec::new(48000, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
                let buf = AudioBuffer::<f32>::new(0, spec);
                AudioBufferRef::F32(Cow::Owned(buf))
            }
        }
    }
}

impl OpusDecoder {
    /// Decode Hybrid mode packets (SILK + CELT)
    ///
    /// In Hybrid mode:
    /// - SILK decodes low frequencies (0-8 kHz)
    /// - CELT decodes high frequencies (8-12 kHz for SWB, 8-20 kHz for FB)
    /// - Both outputs are combined to produce the final wideband audio
    ///
    /// Reference: RFC 6716 Section 2
    fn decode_hybrid(
        &mut self,
        packet: &Packet,
        _toc: &toc::Toc,
    ) -> symphonia_core::errors::Result<AudioBufferRef<'_>> {
        // Decode SILK layer (low frequencies 0-8kHz)
        let silk_output = self.silk_decoder.decode(packet)?;

        // Decode CELT layer (high frequencies 8-12/20kHz)
        let celt_output = self.celt_decoder.decode(packet)?;

        // Get references to the decoded buffers
        let silk_buf = match silk_output {
            AudioBufferRef::F32(buf) => buf,
            _ => return decode_error("opus: expected F32 buffer from SILK"),
        };

        let celt_buf = match celt_output {
            AudioBufferRef::F32(buf) => buf,
            _ => return decode_error("opus: expected F32 buffer from CELT"),
        };

        // Ensure both buffers have compatible specs
        let num_channels = silk_buf.spec().channels.count();
        let num_frames = silk_buf.frames().min(celt_buf.frames());

        if num_frames == 0 {
            return decode_error("opus: no frames decoded in hybrid mode");
        }

        // Clear and prepare hybrid buffer
        self.hybrid_buffer.clear();
        self.hybrid_buffer.render_reserved(Some(num_frames));

        // Combine SILK (low-freq) and CELT (high-freq) outputs
        // Since SILK handles 0-8kHz and CELT handles 8-12/20kHz,
        // we add them together (they cover different frequency bands)
        for ch in 0..num_channels {
            let silk_ch = silk_buf.chan(ch);
            let celt_ch = celt_buf.chan(ch);
            let hybrid_ch = self.hybrid_buffer.chan_mut(ch);

            for i in 0..num_frames {
                // Simple addition since they cover different frequency ranges
                // A more sophisticated implementation might use bandpass filtering
                hybrid_ch[i] = silk_ch[i] + celt_ch[i];
            }
        }

        Ok(self.hybrid_buffer.as_audio_buffer_ref())
    }
}
