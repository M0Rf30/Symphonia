// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Siren decoders: Polycom Siren 7 and Siren 14 (ITU-T G.722.1 and G.722.1 Annex C), and the
//! Windows Live Messenger ("MSN") variant of Siren 7.
//!
//! All of these are mono, 20 ms MLT transform codecs whose frames are a coded spectral envelope
//! followed by Huffman coded, vector quantized coefficients. The variant is chosen by the codec ID
//! and the sample rate of the codec parameters:
//!
//! | Codec ID                      | Sample rate | Frame                     | FFmpeg decoder |
//! |-------------------------------|-------------|---------------------------|----------------|
//! | `CODEC_ID_SIREN`              | 32000 Hz    | 640 samples (Siren 14)    | -              |
//! | `CODEC_ID_SIREN`              | other       | 320 samples (Siren 7)     | `siren`        |
//! | `CODEC_ID_MSN_SIREN`          | any         | 320 samples               | `msnsiren`     |
//!
//! A Siren packet is a single frame whose size sets the bit rate (a G.722.1 frame is 24, 32 or 48
//! kbps, or 960 to 1920 bits at 32 kHz). An MSN Siren packet holds one or more whole frames of
//! `sample_rate / 50` bits.
//!
//! The decoder follows FFmpeg's `siren` and `msnsiren` decoders and, for what they do not
//! cover, the ITU-T G.722.1 reference decoder; see `NOTICE`.

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

mod frame;
mod imdct;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
    layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_MSN_SIREN, CODEC_ID_SIREN};
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use frame::{Config, FrameDecoder, MAX_FRAME_SIZE};

/// The sample rate of Siren 14 streams.
const SIREN14_SAMPLE_RATE: u32 = 32000;

/// Siren / MSN Siren decoder.
pub struct SirenDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    frame: FrameDecoder,
    frame_size: usize,
    /// For MSN Siren, the size of a frame in bits; `None` if a packet is a single frame.
    msn_frame_bits: Option<usize>,
}

impl SirenDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            _ => return unsupported_error("siren: sample rate is required"),
        };

        // Only mono is defined.
        if let Some(channels) = &params.channels {
            if channels.count() != 1 {
                return unsupported_error("siren: only mono is supported");
            }
        }

        let (cfg, msn_frame_bits) = match params.codec {
            CODEC_ID_SIREN if rate == SIREN14_SAMPLE_RATE => (Config::siren14(), None),
            CODEC_ID_SIREN => (Config::siren7(), None),
            CODEC_ID_MSN_SIREN => {
                // As in FFmpeg, the size of a frame follows from the sample rate (16 kHz, 40
                // bytes).
                let bits = rate as usize / 50;

                if bits < 8 {
                    return unsupported_error("siren: sample rate is too low");
                }

                (Config::msn_siren(), Some(bits))
            }
            _ => return unsupported_error("siren: invalid codec"),
        };

        let spec = AudioSpec::new(rate, layouts::CHANNEL_LAYOUT_MONO);
        let buf = AudioBuffer::new(spec, MAX_FRAME_SIZE);

        let mut params = params.clone();
        params.with_channels(layouts::CHANNEL_LAYOUT_MONO);

        Ok(SirenDecoder {
            params,
            opts: *opts,
            buf,
            frame_size: cfg.frame_size,
            frame: FrameDecoder::new(cfg),
            msn_frame_bits,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;
        let m = self.frame_size;

        // The number of frames and the size of each in bits.
        let (frames, frame_bits) = match self.msn_frame_bits {
            Some(bits) => {
                let frames = data.len() / (bits / 8);

                if frames == 0 {
                    return decode_error("siren: packet is too small");
                }

                (frames, bits)
            }
            None => (1, data.len() * 8),
        };

        if frames * m > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), frames * m);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(frames * m));

        let out = self.buf.plane_mut(0).expect("mono plane");
        let mut offset = 0;

        for pcm in out.chunks_exact_mut(m) {
            self.frame.decode(&data[offset..], frame_bits, self.opts.verify, pcm)?;
            offset += frame_bits / 8;
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for SirenDecoder {
    fn reset(&mut self) {
        self.frame.reset();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()
            .iter()
            .find(|c| c.id == self.params.codec)
            .expect("codec was validated in try_new")
            .info
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

impl RegisterableAudioDecoder for SirenDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(SirenDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[
            support_audio_codec!(CODEC_ID_SIREN, "siren", "Siren / G.722.1"),
            support_audio_codec!(CODEC_ID_MSN_SIREN, "msnsiren", "MSN Siren"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_core::audio::Audio;

    fn decoder(codec: symphonia_core::codecs::audio::AudioCodecId, rate: u32) -> SirenDecoder {
        let mut p = AudioCodecParameters::new();
        p.for_codec(codec).with_sample_rate(rate);
        SirenDecoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
    }

    /// Arbitrary (corrupt) input must never panic and must always yield finite output.
    #[test]
    fn verify_random_packets_do_not_panic() {
        let cases = [
            (CODEC_ID_SIREN, 16000, 40),
            (CODEC_ID_SIREN, 16000, 60),
            (CODEC_ID_SIREN, 16000, 80),
            (CODEC_ID_SIREN, 32000, 120),
            (CODEC_ID_SIREN, 32000, 60),
            (CODEC_ID_MSN_SIREN, 16000, 80),
        ];

        for (codec, rate, len) in cases {
            let mut dec = decoder(codec, rate);
            let mut state = 0x1234_5678_9abc_def0u64;
            let mut data = vec![0u8; len];

            for _ in 0..3000 {
                for b in data.iter_mut() {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *b = (state >> 32) as u8;
                }
                let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
                if let Ok(GenericAudioBufferRef::F32(b)) = dec.decode_ref(&pkt) {
                    assert!(b.plane(0).unwrap().iter().all(|s| s.is_finite()));
                }
            }
        }
    }

    #[test]
    fn verify_short_and_empty_packets_are_errors() {
        let mut dec = decoder(CODEC_ID_MSN_SIREN, 16000);
        for len in [0, 1, 39] {
            let data = vec![0u8; len];
            let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
            assert!(dec.decode_ref(&pkt).is_err());
        }

        let mut dec = decoder(CODEC_ID_SIREN, 16000);
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &[]);
        assert!(dec.decode_ref(&pkt).is_err());
    }

    #[test]
    fn verify_requires_sample_rate_and_mono() {
        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_SIREN);
        assert!(SirenDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());

        p.with_sample_rate(16000).with_channels(layouts::CHANNEL_LAYOUT_STEREO);
        assert!(SirenDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());
    }
}
