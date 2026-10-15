// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! GSM 06.10 full-rate speech decoder.
//!
//! Two bitstream packings of the same codec (ETSI EN 300 961, GSM 06.10) are supported:
//!
//!  * [`CODEC_ID_GSM`]: 33-byte blocks holding one 160-sample frame, MSB first and prefixed by the
//!    4-bit signature `0xD` (raw `.gsm` files, QuickTime/AIFF-C `GSM `, CAF `agsm`, RTP).
//!  * [`CODEC_ID_GSM_MS`]: Microsoft GSM 6.10 (WAVE format tag `0x0031`, also known as WAV49),
//!    65-byte blocks holding two frames (320 samples), packed LSB first without a signature.
//!
//! The decoder is integer-exact with the reference implementation (libgsm) and FFmpeg's
//! `gsm`/`gsm_ms` decoders for valid streams, and produces mono 16-bit samples. A packet holds
//! one or more whole blocks (no upper limit); any trailing partial block is ignored. The state
//! (short-term synthesis filter, long-term predictor history, previous LARs, de-emphasis
//! memory) persists between blocks and is cleared by [`AudioDecoder::reset`].
//!
//! Deviations of FFmpeg that are not reproduced here, because the standard and libgsm define the
//! behaviour: a long-term lag outside 40..=120 keeps the previous lag (FFmpeg clamps it), and the
//! lattice filter saturates to 16 bits at every step (FFmpeg only clamps the output). They differ
//! only for streams no conforming encoder produces.

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

mod synth;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
    layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_GSM, CODEC_ID_GSM_MS};
use symphonia_core::codecs::audio::{
    AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{BitReaderLtr, BitReaderRtl, ReadBitsLtr, ReadBitsRtl};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use synth::{FRAME_SAMPLES, Frame, Synth};

/// Size of a full-rate block in bytes (4-bit signature + 260 payload bits).
const GSM_BLOCK_LEN: usize = 33;
/// Size of a Microsoft GSM block in bytes (two 260-bit frames).
const GSM_MS_BLOCK_LEN: usize = 65;
/// The frame signature of the full-rate packing (ETSI EN 300 961, annex / RTP payload format).
const GSM_SIGNATURE: u32 = 0xd;
/// The sample rate used when the container does not state one.
const GSM_SAMPLE_RATE: u32 = 8000;
/// Number of blocks the output buffer is initially sized for. Larger packets grow it.
const INITIAL_BLOCKS: usize = 16;

/// The bitstream packing of a block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Packing {
    /// 33-byte blocks, one frame, MSB first.
    FullRate,
    /// 65-byte blocks, two frames, LSB first.
    Microsoft,
}

impl Packing {
    fn block_len(self) -> usize {
        match self {
            Packing::FullRate => GSM_BLOCK_LEN,
            Packing::Microsoft => GSM_MS_BLOCK_LEN,
        }
    }

    fn block_samples(self) -> usize {
        match self {
            Packing::FullRate => FRAME_SAMPLES,
            Packing::Microsoft => 2 * FRAME_SAMPLES,
        }
    }
}

fn packing_of(codec: AudioCodecId) -> Option<Packing> {
    match codec {
        CODEC_ID_GSM => Some(Packing::FullRate),
        CODEC_ID_GSM_MS => Some(Packing::Microsoft),
        _ => None,
    }
}

/// GSM 06.10 full-rate decoder (plain and Microsoft packing).
pub struct GsmDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    packing: Packing,
    buf: AudioBuffer<i16>,
    synth: Synth,
}

impl GsmDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        let Some(packing) = packing_of(params.codec)
        else {
            return unsupported_error("gsm: invalid codec");
        };

        // Only mono is defined.
        if let Some(channels) = &params.channels {
            if channels.count() != 1 {
                return unsupported_error("gsm: only mono is supported");
            }
        }

        let rate = params.sample_rate.filter(|&r| r > 0).unwrap_or(GSM_SAMPLE_RATE);

        let spec = AudioSpec::new(rate, layouts::CHANNEL_LAYOUT_MONO);
        let buf = AudioBuffer::new(spec, INITIAL_BLOCKS * packing.block_samples());

        let mut params = params.clone();
        params.with_sample_rate(rate).with_channels(layouts::CHANNEL_LAYOUT_MONO);

        Ok(GsmDecoder { params, opts: *opts, packing, buf, synth: Synth::new() })
    }

    /// Decodes one block into `out`, which must hold `block_samples` samples.
    fn decode_block(
        packing: Packing,
        synth: &mut Synth,
        block: &[u8],
        out: &mut [i16],
    ) -> Result<()> {
        match packing {
            Packing::FullRate => {
                let mut br = BitReaderLtr::new(block);

                if br.read_bits_leq32(4)? != GSM_SIGNATURE {
                    return decode_error("gsm: invalid frame signature");
                }

                let frame = Frame::parse(|n| Ok(br.read_bits_leq32(n)?))?;
                synth.decode(&frame, out);
            }
            Packing::Microsoft => {
                let mut br = BitReaderRtl::new(block);

                // The second frame continues the bit stream at bit 260.
                let first = Frame::parse(|n| Ok(br.read_bits_leq32(n)?))?;
                let second = Frame::parse(|n| Ok(br.read_bits_leq32(n)?))?;

                let (a, b) = out.split_at_mut(FRAME_SAMPLES);
                synth.decode(&first, a);
                synth.decode(&second, b);
            }
        }

        Ok(())
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let block_len = self.packing.block_len();
        let block_samples = self.packing.block_samples();
        let blocks = packet.data.len() / block_len;

        if blocks == 0 {
            return decode_error("gsm: packet is too small");
        }

        if blocks * block_samples > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), blocks * block_samples);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(blocks * block_samples));

        let Some(out) = self.buf.plane_mut(0)
        else {
            return decode_error("gsm: missing output plane");
        };

        for (block, audio) in
            packet.data.chunks_exact(block_len).zip(out.chunks_exact_mut(block_samples))
        {
            Self::decode_block(self.packing, &mut self.synth, block, audio)?;
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for GsmDecoder {
    fn reset(&mut self) {
        self.synth = Synth::new();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()
            .iter()
            .find(|desc| desc.id == self.params.codec)
            .expect("codec registered in supported_codecs")
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

impl RegisterableAudioDecoder for GsmDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(GsmDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[
            support_audio_codec!(CODEC_ID_GSM, "gsm", "GSM 06.10 full-rate"),
            support_audio_codec!(CODEC_ID_GSM_MS, "gsm_ms", "Microsoft GSM 6.10"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder(codec: AudioCodecId) -> GsmDecoder {
        let mut p = AudioCodecParameters::new();
        p.for_codec(codec).with_sample_rate(8000);
        GsmDecoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
    }

    fn packet(data: &[u8]) -> PacketRef<'_> {
        PacketRef::new(0, Default::default(), Default::default(), data)
    }

    /// Arbitrary (corrupt) input must never panic, and must never produce samples with any of
    /// the three low bits set (the codec truncates to 13 bits).
    #[test]
    fn random_blocks_do_not_panic() {
        for (codec, block_len, samples) in
            [(CODEC_ID_GSM, GSM_BLOCK_LEN, 160usize), (CODEC_ID_GSM_MS, GSM_MS_BLOCK_LEN, 320)]
        {
            let mut dec = decoder(codec);
            let mut state = 0x1234_5678_9abc_def0u64;
            let mut data = vec![0u8; block_len * 3 + 5];
            for _ in 0..3000 {
                for b in data.iter_mut() {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *b = (state >> 32) as u8;
                }
                if codec == CODEC_ID_GSM {
                    // Make the signature valid so the synthesis runs.
                    for chunk in data.chunks_exact_mut(block_len) {
                        chunk[0] = 0xd0 | (chunk[0] & 0x0f);
                    }
                }
                let buf = dec.decode_ref(&packet(&data)).unwrap();
                if let GenericAudioBufferRef::S16(b) = buf {
                    assert_eq!(b.frames(), 3 * samples);
                    let mut v = Vec::new();
                    b.copy_to_vec_interleaved(&mut v);
                    assert!(v.iter().all(|s| s & 7 == 0));
                }
                else {
                    panic!("expected s16 output");
                }
            }
        }
    }

    #[test]
    fn short_packet_is_error() {
        for (codec, len) in [(CODEC_ID_GSM, GSM_BLOCK_LEN), (CODEC_ID_GSM_MS, GSM_MS_BLOCK_LEN)] {
            let mut dec = decoder(codec);
            let data = vec![0u8; len - 1];
            assert!(dec.decode_ref(&packet(&data)).is_err());
            assert!(dec.decode_ref(&packet(&[])).is_err());
        }
    }

    #[test]
    fn missing_signature_is_error() {
        let mut dec = decoder(CODEC_ID_GSM);
        assert!(dec.decode_ref(&packet(&[0u8; GSM_BLOCK_LEN])).is_err());
    }

    #[test]
    fn rejects_stereo_and_foreign_codecs() {
        use symphonia_core::codecs::audio::well_known::CODEC_ID_FLAC;

        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_GSM).with_channels(layouts::CHANNEL_LAYOUT_STEREO);
        assert!(GsmDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());

        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_FLAC);
        assert!(GsmDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());
    }

    /// An all-zero payload (silence-like parameters with a valid signature) decodes
    /// deterministically, and `reset` restores the initial state.
    #[test]
    fn reset_restores_initial_state() {
        let mut block = [0u8; GSM_BLOCK_LEN];
        block[0] = 0xd0 | 0x0a;
        block[1] = 0x5a;
        block[7] = 0x93;
        block[20] = 0xc7;

        let decode = |dec: &mut GsmDecoder| -> Vec<i16> {
            let mut v = Vec::new();
            match dec.decode_ref(&packet(&block)).unwrap() {
                GenericAudioBufferRef::S16(b) => b.copy_to_vec_interleaved(&mut v),
                _ => panic!("expected s16 output"),
            }
            v
        };

        let mut dec = decoder(CODEC_ID_GSM);
        let first = decode(&mut dec);
        let second = decode(&mut dec);
        assert_ne!(first, second, "filter state must carry over between frames");
        dec.reset();
        assert_eq!(first, decode(&mut dec));
    }
}
