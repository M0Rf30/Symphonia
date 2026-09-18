// Symphonia APE decoder
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::{AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_MONKEYS_AUDIO;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Error, Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::map_ape_error;

/// Fallback packet size cap (in frames) if the demuxer did not provide one.
const DEFAULT_MAX_FRAMES_PER_PACKET: u64 = 73728 * 4;

/// Monkey's Audio (APE) decoder.
///
/// This wraps `ape_decoder::FrameDecoder`, a stateful frame decoder purpose-built for external
/// demuxer integration: it only decodes already-demuxed compressed frame bytes and holds no I/O
/// of its own. All demuxing (header/seek-table parsing, frame boundary lookup) is done by
/// [`crate::ApeReader`].
pub struct ApeDecoder {
    params: AudioCodecParameters,
    /// Decoded audio, normalized to 32-bit width regardless of the source bit depth (matching the
    /// convention used by the FLAC and ALAC decoders in this workspace).
    buf: AudioBuffer<i32>,
    frame_decoder: ape_decoder::FrameDecoder,
    channels: u16,
    bits_per_sample: u16,
}

impl ApeDecoder {
    pub fn try_new(params: &AudioCodecParameters, _options: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_MONKEYS_AUDIO {
            return unsupported_error("ape: invalid codec");
        }

        let sample_rate =
            params.sample_rate.ok_or(Error::DecodeError("ape: missing sample rate"))?;
        let channels = params.channels.clone().ok_or(Error::DecodeError("ape: missing channels"))?;
        let bits_per_sample =
            params.bits_per_sample.ok_or(Error::DecodeError("ape: missing bits per sample"))? as u16;
        let max_frames =
            params.max_frames_per_packet.unwrap_or(DEFAULT_MAX_FRAMES_PER_PACKET) as usize;

        // Extra data layout (6 bytes, little-endian): version(u16), compression_level(u16),
        // channels(u16). See `demuxer::build_extra_data`.
        let extra = params.extra_data.as_ref().ok_or(Error::DecodeError("ape: missing extra data"))?;
        if extra.len() < 6 {
            return decode_error("ape: extra data too short");
        }

        let version = u16::from_le_bytes([extra[0], extra[1]]);
        let compression_level = u16::from_le_bytes([extra[2], extra[3]]);
        let channels_count = u16::from_le_bytes([extra[4], extra[5]]);

        let frame_decoder =
            ape_decoder::FrameDecoder::new(version, channels_count, bits_per_sample, compression_level)
                .map_err(map_ape_error)?;

        let spec = AudioSpec::new(sample_rate, channels);
        let buf = AudioBuffer::new(spec, max_frames);

        Ok(ApeDecoder { params: params.clone(), buf, frame_decoder, channels: channels_count, bits_per_sample })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;

        // The demuxer prepends the 4-byte little-endian seek (byte alignment) remainder.
        if data.len() < 5 {
            return decode_error("ape: packet too short");
        }

        let seek_remainder = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let frame_data = &data[4..];
        let frame_blocks = packet.dur.get() as usize;
        let ch_count = self.channels as usize;

        let pcm_bytes = self
            .frame_decoder
            .decode_frame(frame_data, seek_remainder, frame_blocks)
            .map_err(map_ape_error)?;

        self.buf.clear();
        self.buf.render_uninit(Some(frame_blocks));

        match self.bits_per_sample {
            8 => {
                if pcm_bytes.len() < frame_blocks * ch_count {
                    return decode_error("ape: pcm data too short for 8-bit");
                }
                for ch in 0..ch_count {
                    let plane = self.buf.plane_mut(ch).expect("channel count matches buffer spec");
                    for (frame, sample) in plane.iter_mut().enumerate() {
                        // APE "unprepare" biases 8-bit samples by +128 (silence = 128), which is
                        // exactly Symphonia's unsigned 8-bit PCM convention; no rebias needed.
                        *sample = i32::from(pcm_bytes[frame * ch_count + ch]) - 128;
                    }
                }
            }
            16 => {
                if pcm_bytes.len() < frame_blocks * ch_count * 2 {
                    return decode_error("ape: pcm data too short for 16-bit");
                }
                for ch in 0..ch_count {
                    let plane = self.buf.plane_mut(ch).expect("channel count matches buffer spec");
                    for (frame, sample) in plane.iter_mut().enumerate() {
                        let off = (frame * ch_count + ch) * 2;
                        *sample = i32::from(i16::from_le_bytes([pcm_bytes[off], pcm_bytes[off + 1]]));
                    }
                }
            }
            24 => {
                if pcm_bytes.len() < frame_blocks * ch_count * 3 {
                    return decode_error("ape: pcm data too short for 24-bit");
                }
                for ch in 0..ch_count {
                    let plane = self.buf.plane_mut(ch).expect("channel count matches buffer spec");
                    for (frame, sample) in plane.iter_mut().enumerate() {
                        let off = (frame * ch_count + ch) * 3;
                        let raw = u32::from(pcm_bytes[off])
                            | (u32::from(pcm_bytes[off + 1]) << 8)
                            | (u32::from(pcm_bytes[off + 2]) << 16);
                        // Sign-extend the 24-bit value to 32 bits.
                        *sample = ((raw << 8) as i32) >> 8;
                    }
                }
            }
            32 => {
                if pcm_bytes.len() < frame_blocks * ch_count * 4 {
                    return decode_error("ape: pcm data too short for 32-bit");
                }
                for ch in 0..ch_count {
                    let plane = self.buf.plane_mut(ch).expect("channel count matches buffer spec");
                    for (frame, sample) in plane.iter_mut().enumerate() {
                        let off = (frame * ch_count + ch) * 4;
                        *sample = i32::from_le_bytes([
                            pcm_bytes[off],
                            pcm_bytes[off + 1],
                            pcm_bytes[off + 2],
                            pcm_bytes[off + 3],
                        ]);
                    }
                }
            }
            _ => return unsupported_error("ape: unsupported bit depth"),
        }

        // Normalize samples to 32-bit width (left-justify), matching the FLAC/ALAC convention.
        if self.bits_per_sample < 32 {
            let shift = 32 - u32::from(self.bits_per_sample);
            self.buf.apply(|sample| sample << shift);
        }

        Ok(())
    }
}

impl AudioDecoder for ApeDecoder {
    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs().first().expect("at least one codec registered").info
    }

    fn reset(&mut self) {
        // APE frames are independently decodable (predictors/entropy/range coder are reset at
        // the start of each frame by `ape_decoder::FrameDecoder`), so there is no persistent
        // state to clear here.
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

impl RegisterableAudioDecoder for ApeDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(ApeDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_MONKEYS_AUDIO, "ape", "Monkey's Audio (APE)")]
    }
}
