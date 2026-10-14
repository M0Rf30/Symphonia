// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::support_audio_codec;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
};
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_ADPCM_G722, CODEC_ID_ADPCM_G726, CODEC_ID_ADPCM_G726LE, CODEC_ID_ADPCM_IMA_QT,
    CODEC_ID_ADPCM_IMA_WAV, CODEC_ID_ADPCM_MS, CODEC_ID_ADPCM_SWF, CODEC_ID_ADPCM_YAMAHA,
};
use symphonia_core::codecs::audio::{AudioCodecId, AudioCodecParameters, AudioDecoderOptions};
use symphonia_core::codecs::audio::{AudioDecoder, FinalizeResult};
use symphonia_core::errors::{Error, Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;

mod codec_g722;
mod codec_g726;
mod codec_ima_qt;
mod codec_ima_wav;
mod codec_ms;
mod codec_swf;
mod codec_yamaha;
mod common;
mod common_ima;

fn is_supported_adpcm_codec(codec_id: AudioCodecId) -> bool {
    matches!(
        codec_id,
        CODEC_ID_ADPCM_MS
            | CODEC_ID_ADPCM_IMA_WAV
            | CODEC_ID_ADPCM_IMA_QT
            | CODEC_ID_ADPCM_G722
            | CODEC_ID_ADPCM_G726
            | CODEC_ID_ADPCM_G726LE
            | CODEC_ID_ADPCM_SWF
            | CODEC_ID_ADPCM_YAMAHA
    )
}

enum InnerDecoder {
    /// Microsoft ADPCM with its coefficient set.
    Ms {
        coeffs: Vec<[i32; 2]>,
    },
    /// IMA ADPCM WAV with 2 to 5 bits per sample.
    ImaWav {
        bits_per_sample: u32,
    },
    /// QuickTime IMA ADPCM with the per-channel state carried between packets.
    ImaQt {
        state: [codec_ima_qt::QtChannelState; 2],
    },
    Yamaha(codec_yamaha::YamahaDecoder),
    Swf,
    G722(codec_g722::G722Decoder),
    G726(codec_g726::G726Decoder),
}

/// Adaptive Differential Pulse Code Modulation (ADPCM) decoder.
pub struct AdpcmDecoder {
    params: AudioCodecParameters,
    gapless: bool,
    inner_decoder: InnerDecoder,
    buf: AudioBuffer<i32>,
}

impl AdpcmDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        // This decoder only supports certain ADPCM codecs.
        if !is_supported_adpcm_codec(params.codec) {
            return unsupported_error("adpcm: invalid codec");
        }

        let frames = match params.max_frames_per_packet {
            Some(frames) => frames as usize,
            _ => return unsupported_error("adpcm: maximum frames per packet is required"),
        };

        // The block based codecs need the block size.
        let block_based = matches!(
            params.codec,
            CODEC_ID_ADPCM_MS | CODEC_ID_ADPCM_IMA_WAV | CODEC_ID_ADPCM_IMA_QT
        );

        if block_based && params.frames_per_block.is_none_or(|v| v == 0) {
            return unsupported_error("adpcm: valid frames per block is required");
        }

        // A QuickTime IMA ADPCM packet is always 34 bytes and decodes to exactly 64
        // samples. Any other frames per block value would make the decoder write past
        // the output slice.
        if params.codec == CODEC_ID_ADPCM_IMA_QT && params.frames_per_block != Some(64) {
            return unsupported_error("adpcm (ima-qt): frames per block must be 64");
        }

        let rate = match params.sample_rate {
            Some(rate) => rate,
            _ => return unsupported_error("adpcm: sample rate is required"),
        };

        let channel_count = match &params.channels {
            Some(channels) => channels.count(),
            None => return unsupported_error("adpcm: channels or channel_layout is required"),
        };

        // Only IMA WAV defines a layout for more than two channels, and the ITU codecs are mono.
        let max_channels = match params.codec {
            CODEC_ID_ADPCM_IMA_WAV => codec_ima_wav::MAX_CHANNELS,
            CODEC_ID_ADPCM_G722 | CODEC_ID_ADPCM_G726 | CODEC_ID_ADPCM_G726LE => 1,
            _ => 2,
        };
        if channel_count > max_channels {
            return unsupported_error("adpcm: unsupported number of channels");
        }

        let bits = params.bits_per_coded_sample;

        let inner_decoder = match params.codec {
            CODEC_ID_ADPCM_MS => {
                // The WAVEFORMATEX extension may carry its own coefficient set.
                let coeffs = params
                    .extra_data
                    .as_deref()
                    .and_then(codec_ms::parse_coeffs)
                    .unwrap_or_else(|| codec_ms::MS_STANDARD_COEFFS.to_vec());
                InnerDecoder::Ms { coeffs }
            }
            CODEC_ID_ADPCM_IMA_WAV => match bits.unwrap_or(4) {
                bits_per_sample @ 2..=5 => InnerDecoder::ImaWav { bits_per_sample },
                _ => return unsupported_error("adpcm (ima-wav): bits per sample must be 2 to 5"),
            },
            CODEC_ID_ADPCM_IMA_QT => InnerDecoder::ImaQt {
                state: [codec_ima_qt::new_state(), codec_ima_qt::new_state()],
            },
            CODEC_ID_ADPCM_YAMAHA => InnerDecoder::Yamaha(codec_yamaha::YamahaDecoder::new()),
            CODEC_ID_ADPCM_SWF => InnerDecoder::Swf,
            CODEC_ID_ADPCM_G722 => match bits.unwrap_or(8) {
                bits_per_codeword @ 6..=8 => {
                    InnerDecoder::G722(codec_g722::G722Decoder::new(bits_per_codeword))
                }
                _ => return unsupported_error("adpcm (g722): bits per codeword must be 6 to 8"),
            },
            CODEC_ID_ADPCM_G726 | CODEC_ID_ADPCM_G726LE => match bits {
                Some(bits_per_sample @ 2..=5) => InnerDecoder::G726(codec_g726::G726Decoder::new(
                    bits_per_sample,
                    params.codec == CODEC_ID_ADPCM_G726,
                )),
                _ => return unsupported_error("adpcm (g726): bits per sample must be 2 to 5"),
            },
            _ => return unsupported_error("adpcm: codec is unsupported"),
        };

        let spec = AudioSpec::new(rate, params.channels.clone().expect("checked above"));

        Ok(AdpcmDecoder {
            params: params.clone(),
            gapless: opts.gapless,
            inner_decoder,
            buf: AudioBuffer::new(spec, frames),
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let mut stream = packet.as_buf_reader();
        let data = packet.data;

        // The ITU codecs are sample (not block) based: the number of frames is given by the size
        // of the packet.
        match &mut self.inner_decoder {
            InnerDecoder::Yamaha(dec) => {
                let channels = self.buf.spec().channels().count();
                let num_frames = if channels == 1 { 2 * data.len() } else { data.len() };
                if num_frames > self.buf.capacity() {
                    return decode_error("adpcm: packet exceeds maximum frames per packet");
                }
                self.buf.clear();
                self.buf.render_uninit(Some(num_frames));
                if channels == 1 {
                    let out = self.buf.plane_mut(0).ok_or(Error::DecodeError("adpcm: no plane"))?;
                    dec.decode_mono(data, out);
                }
                else {
                    let (l, r) = self
                        .buf
                        .plane_pair_mut(0, 1)
                        .ok_or(Error::DecodeError("adpcm: no planes"))?;
                    dec.decode_stereo(data, l, r);
                }
                return self.finish(packet);
            }
            InnerDecoder::Swf => {
                let channels = self.buf.spec().channels().count();
                let num_frames = codec_swf::frames_in(data, channels);
                if num_frames > self.buf.capacity() {
                    return decode_error("adpcm: packet exceeds maximum frames per packet");
                }
                self.buf.clear();
                self.buf.render_uninit(Some(num_frames));
                if channels == 1 {
                    let out = self.buf.plane_mut(0).ok_or(Error::DecodeError("adpcm: no plane"))?;
                    codec_swf::decode(data, &mut [out])?;
                }
                else {
                    let (l, r) = self
                        .buf
                        .plane_pair_mut(0, 1)
                        .ok_or(Error::DecodeError("adpcm: no planes"))?;
                    codec_swf::decode(data, &mut [l, r])?;
                }
                return self.finish(packet);
            }
            InnerDecoder::G722(dec) => {
                let num_frames = 2 * data.len();
                if num_frames > self.buf.capacity() {
                    return decode_error("adpcm: packet exceeds maximum frames per packet");
                }
                self.buf.clear();
                self.buf.render_uninit(Some(num_frames));
                let out = self.buf.plane_mut(0).ok_or(Error::DecodeError("adpcm: no plane"))?;
                dec.decode(data, out)?;
                return self.finish(packet);
            }
            InnerDecoder::G726(dec) => {
                let num_frames = dec.frames_in(data.len());
                if num_frames > self.buf.capacity() {
                    return decode_error("adpcm: packet exceeds maximum frames per packet");
                }
                self.buf.clear();
                self.buf.render_uninit(Some(num_frames));
                let out = self.buf.plane_mut(0).ok_or(Error::DecodeError("adpcm: no plane"))?;
                dec.decode(data, out)?;
                return self.finish(packet);
            }
            _ => (),
        }

        let frames_per_block = match self.params.frames_per_block {
            Some(v) => v as usize,
            None => return decode_error("adpcm: missing frames_per_block"),
        };

        let block_count = packet.block_dur().get() as usize / frames_per_block;
        let num_frames = block_count * frames_per_block;

        if num_frames > self.buf.capacity() {
            return decode_error("adpcm: packet duration exceeds maximum frames per packet");
        }

        self.buf.clear();
        self.buf.render_uninit(Some(num_frames));

        let channel_count = self.buf.spec().channels().count();

        match &mut self.inner_decoder {
            InnerDecoder::ImaWav { bits_per_sample } => {
                // The blocks may be padded to the block align, which is the size of a block.
                let block_bytes = data.len().checked_div(block_count).unwrap_or(0);
                for block_id in 0..block_count {
                    codec_ima_wav::decode_block(
                        &mut stream,
                        &mut self.buf,
                        channel_count,
                        *bits_per_sample,
                        frames_per_block * block_id,
                        frames_per_block,
                        block_bytes,
                    )?;
                }
            }
            InnerDecoder::Ms { coeffs } => match channel_count {
                1 => {
                    let buffer =
                        self.buf.plane_mut(0).ok_or(Error::DecodeError("adpcm: no plane"))?;
                    for block in buffer.chunks_exact_mut(frames_per_block).take(block_count) {
                        codec_ms::decode_mono(&mut stream, block, frames_per_block, coeffs)?;
                    }
                }
                2 => {
                    let (left, right) = self
                        .buf
                        .plane_pair_mut(0, 1)
                        .ok_or(Error::DecodeError("adpcm: no planes"))?;
                    for (l, r) in left
                        .chunks_exact_mut(frames_per_block)
                        .zip(right.chunks_exact_mut(frames_per_block))
                        .take(block_count)
                    {
                        codec_ms::decode_stereo(&mut stream, [l, r], frames_per_block, coeffs)?;
                    }
                }
                _ => return unsupported_error("adpcm (ms): unsupported number of channels"),
            },
            InnerDecoder::ImaQt { state } => match channel_count {
                1 => {
                    let buffer =
                        self.buf.plane_mut(0).ok_or(Error::DecodeError("adpcm: no plane"))?;
                    for block in buffer.chunks_exact_mut(frames_per_block).take(block_count) {
                        codec_ima_qt::decode_mono(&mut stream, block, &mut state[0])?;
                    }
                }
                2 => {
                    let (left, right) = self
                        .buf
                        .plane_pair_mut(0, 1)
                        .ok_or(Error::DecodeError("adpcm: no planes"))?;
                    for (l, r) in left
                        .chunks_exact_mut(frames_per_block)
                        .zip(right.chunks_exact_mut(frames_per_block))
                        .take(block_count)
                    {
                        codec_ima_qt::decode_stereo(&mut stream, [l, r], state)?;
                    }
                }
                _ => return unsupported_error("adpcm (ima-qt): unsupported number of channels"),
            },
            InnerDecoder::Yamaha(_)
            | InnerDecoder::Swf
            | InnerDecoder::G722(_)
            | InnerDecoder::G726(_) => unreachable!(),
        }

        self.finish(packet)
    }

    /// Trims the decoded packet if gapless playback is enabled.
    fn finish(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        if self.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }
        Ok(())
    }
}

impl AudioDecoder for AdpcmDecoder {
    fn reset(&mut self) {
        // Forget the state kept between packets so the next packet is decoded afresh.
        match &mut self.inner_decoder {
            InnerDecoder::ImaQt { state } => {
                *state = [codec_ima_qt::new_state(), codec_ima_qt::new_state()]
            }
            InnerDecoder::Yamaha(dec) => dec.reset(),
            InnerDecoder::G722(dec) => dec.reset(),
            InnerDecoder::G726(dec) => dec.reset(),
            InnerDecoder::Ms { .. } | InnerDecoder::ImaWav { .. } | InnerDecoder::Swf => (),
        }
    }

    fn codec_info(&self) -> &CodecInfo {
        // Return the codec that's in-use.
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

impl RegisterableAudioDecoder for AdpcmDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(AdpcmDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[
            support_audio_codec!(CODEC_ID_ADPCM_MS, "adpcm_ms", "Microsoft ADPCM"),
            support_audio_codec!(CODEC_ID_ADPCM_IMA_WAV, "adpcm_ima_wav", "ADPCM IMA WAV"),
            support_audio_codec!(CODEC_ID_ADPCM_IMA_QT, "adpcm_ima_qt", "ADPCM IMA QT"),
            support_audio_codec!(CODEC_ID_ADPCM_YAMAHA, "adpcm_yamaha", "Yamaha ADPCM"),
            support_audio_codec!(CODEC_ID_ADPCM_SWF, "adpcm_swf", "Shockwave Flash ADPCM"),
            support_audio_codec!(CODEC_ID_ADPCM_G722, "adpcm_g722", "ITU-T G.722 ADPCM"),
            support_audio_codec!(CODEC_ID_ADPCM_G726, "adpcm_g726", "ITU-T G.726 ADPCM"),
            support_audio_codec!(CODEC_ID_ADPCM_G726LE, "adpcm_g726le", "ITU-T G.726 ADPCM (LE)"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_core::audio::Channels;

    fn adpcm_params(codec: AudioCodecId, frames_per_block: u64) -> AudioCodecParameters {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(codec)
            .with_sample_rate(44100)
            .with_channels(Channels::Discrete(1))
            .with_max_frames_per_packet(frames_per_block)
            .with_frames_per_block(frames_per_block);
        params
    }

    #[test]
    fn ima_qt_rejects_frames_per_block_not_64() {
        // Issue #561: a CAF ima4 track whose frames_per_packet is a legal value
        // below 64 made the decoder write past the output slice and abort. QuickTime
        // IMA ADPCM packets always decode to exactly 64 samples, so any other frames
        // per block value is rejected.
        let opts = AudioDecoderOptions::default();
        for frames_per_block in [2, 32, 63, 65, 128] {
            let params = adpcm_params(CODEC_ID_ADPCM_IMA_QT, frames_per_block);
            assert!(AdpcmDecoder::try_new(&params, &opts).is_err());
        }
    }

    #[test]
    fn ima_qt_accepts_64_frames_per_block() {
        let params = adpcm_params(CODEC_ID_ADPCM_IMA_QT, 64);
        assert!(AdpcmDecoder::try_new(&params, &AudioDecoderOptions::default()).is_ok());
    }

    #[test]
    fn other_adpcm_codecs_frames_per_block_unchanged() {
        // ADPCM IMA WAV and MS derive their bounds from frames_per_block and must not
        // be restricted by the ima-qt check.
        let opts = AudioDecoderOptions::default();
        let params = adpcm_params(CODEC_ID_ADPCM_IMA_WAV, 505);
        assert!(AdpcmDecoder::try_new(&params, &opts).is_ok());
        let params = adpcm_params(CODEC_ID_ADPCM_MS, 500);
        assert!(AdpcmDecoder::try_new(&params, &opts).is_ok());
    }

    #[test]
    fn rejects_packet_duration_above_capacity() {
        use symphonia_core::units::{Duration, Timestamp};

        let params = adpcm_params(CODEC_ID_ADPCM_IMA_WAV, 505);
        let mut decoder = AdpcmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let data = [0u8; 4096];
        let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(505 * 4), &data);
        assert!(decoder.decode_ref(&packet).is_err());
    }

    /// Decodes one packet and returns the interleaved samples, shifted down to 16 bits.
    fn decode_packet_s16(decoder: &mut AdpcmDecoder, data: &[u8], frames: u64) -> Result<Vec<i32>> {
        use symphonia_core::units::{Duration, Timestamp};
        let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(frames), data);
        let buf = decoder.decode_ref(&packet)?;
        let channels = buf.spec().channels().count();
        let mut out = vec![0i32; buf.frames() * channels];
        match buf {
            GenericAudioBufferRef::S32(b) => b.copy_to_slice_interleaved(&mut out),
            _ => unreachable!(),
        }
        Ok(out.iter().map(|s| s >> 16).collect())
    }

    fn decode_packet(decoder: &mut AdpcmDecoder, data: &[u8], frames: u64) -> Result<Vec<i32>> {
        // Mono helper retained for tests that compare shifted samples.
        decode_packet_s16(decoder, data, frames).map(|v| v.iter().map(|s| s << 16).collect())
    }

    #[test]
    fn ima_wav_uses_reference_successive_approximation() {
        // Nibble 7 at step index 0 (step 7): reference IMA gives 7>>3 + 7 + 7>>1 + 7>>2 = 11,
        // whereas ((2 * 7 + 1) * 7) >> 3 = 13.
        let mut params = adpcm_params(CODEC_ID_ADPCM_IMA_WAV, 9);
        params.with_max_frames_per_packet(9);
        let mut decoder = AdpcmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let data = [0, 0, 0, 0, 0x07, 0, 0, 0];
        let out = decode_packet(&mut decoder, &data, 9).unwrap();
        assert_eq!(out[1], 11 << 16);
    }

    #[test]
    fn ms_extreme_delta_does_not_overflow() {
        // Header delta 0x7fff and all-0x88 nibbles grow the delta by 3x per sample; this used to
        // overflow an i32.
        let params = adpcm_params(CODEC_ID_ADPCM_MS, 500);
        let mut decoder = AdpcmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let mut data = vec![0x88u8; 7 + 249];
        data[..7].copy_from_slice(&[0, 0xff, 0x7f, 0, 0, 0, 0]);
        assert!(decode_packet(&mut decoder, &data, 500).is_ok());
    }

    #[test]
    fn ima_qt_keeps_running_predictor_between_packets() {
        // Header predictor is truncated to 9 bits; a packet continuing the previous one (same
        // step index, predictor within 0x7f) keeps the full-precision running predictor.
        let params = adpcm_params(CODEC_ID_ADPCM_IMA_QT, 64);
        let mut decoder = AdpcmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let mut first = [0u8; 34];
        first[0] = 0x10; // predictor 0x1000, step index 0.
        first[2..].fill(0x31);
        let first_out = decode_packet(&mut decoder, &first, 64).unwrap();
        let mut second = [0u8; 34];
        second[0] = 0x00; // placeholder, set below to the continuing header.
        let (state, step_index) = match &decoder.inner_decoder {
            InnerDecoder::ImaQt { state } => (state[0].predictor, state[0].step_index),
            _ => unreachable!(),
        };
        assert_eq!(first_out[63] >> 16, state);
        assert_ne!(state & 0x7f, 0);
        let header = ((state & 0xff80) as u16) | step_index as u16;
        second[..2].copy_from_slice(&header.to_be_bytes());
        let cont = decode_packet(&mut decoder, &second, 64).unwrap();
        // After a reset the header predictor is used instead.
        decoder.reset();
        let fresh = decode_packet(&mut decoder, &second, 64).unwrap();
        assert_eq!((cont[0] >> 16) - (fresh[0] >> 16), state & 0x7f);
    }

    fn ima_wav_vector(bits: u32, channels: u16, block: &[u8], frames: u64, expected: &[i16]) {
        let mut params = adpcm_params(CODEC_ID_ADPCM_IMA_WAV, frames);
        params.with_bits_per_coded_sample(bits).with_channels(Channels::Discrete(channels));
        let mut decoder = AdpcmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let out = decode_packet_s16(&mut decoder, block, frames).unwrap();
        // The helper only returns the first plane; compare it against that channel.
        let expected: Vec<i32> = expected.iter().map(|&v| i32::from(v)).collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn ima_wav_5_bit_matches_ffmpeg() {
        // Generated with FFmpeg's adpcm_ima_wav decoder from a random 24 byte block.
        let block = [
            0xf6, 0xed, 0x25, 0x00, 0x66, 0xb7, 0x47, 0xef, 0x9e, 0x9e, 0xa0, 0x4e, 0xc2, 0x1e,
            0xd5, 0x57, 0xaa, 0x8f, 0x41, 0x41, 0x8a, 0x0e, 0x17, 0x51,
        ];
        let expected = [
            -4618, -4414, -4743, -4055, -2002, -4741, -8891, -14677, -17796, -29548, -16957, 4664,
            -32768, -14338, -8752, -32768, -18435, -32768, -32768, -32768, -32768, -32768, -2052,
            22155, 32767, 32767, 24304, 31997, -5777, -7824, 32767, 32767, 32767,
        ];
        ima_wav_vector(5, 1, &block, 33, &expected);
    }

    #[test]
    fn ima_wav_2_bit_stereo_matches_ffmpeg() {
        // Same, 2 bits per sample, 2 channels (4-byte words interleaved per channel).
        let block = [
            0x61, 0x2a, 0x0e, 0x00, 0xfb, 0x18, 0x07, 0x00, 0xaa, 0x11, 0xb4, 0xf1, 0xf4, 0xc2,
            0xab, 0x16,
        ];
        let expected = [
            10849, 6395, 10835, 6402, 10823, 6421, 10812, 6397, 10802, 6369, 10830, 6358, 10841,
            6368, 10872, 6377, 10884, 6352, 10895, 6321, 10926, 6309, 10889, 6298, 10874, 6288,
            10916, 6279, 10933, 6304, 10887, 6335, 10832, 6347,
        ];
        ima_wav_vector(2, 2, &block, 17, &expected);
    }

    #[test]
    fn ms_uses_coefficients_from_extra_data() {
        // wSamplesPerBlock = 4, wNumCoef = 1, coefficient pair (0, 0).
        let extra = [4u8, 0, 1, 0, 0, 0, 0, 0];
        let block = [0u8, 16, 0, 100, 0, 0, 0, 0x10];

        let mut params = adpcm_params(CODEC_ID_ADPCM_MS, 4);
        let mut decoder = AdpcmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        // Standard coefficient set 0 is (256, 0): the prediction repeats the previous sample.
        let out = decode_packet_s16(&mut decoder, &block, 4).unwrap();
        assert_eq!(out, [0, 100, 116, 116]);

        params.with_extra_data(extra.to_vec().into_boxed_slice());
        let mut decoder = AdpcmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let out = decode_packet_s16(&mut decoder, &block, 4).unwrap();
        assert_eq!(out, [0, 100, 16, 0]);

        // A block predictor outside the file's table is an error.
        let mut bad = block;
        bad[0] = 1;
        assert!(decode_packet_s16(&mut decoder, &bad, 4).is_err());
    }
}
