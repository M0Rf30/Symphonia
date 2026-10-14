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
    AsGenericAudioBufferRef, AudioSpec, GenericAudioBuffer, GenericAudioBufferRef,
};
use symphonia_core::codecs::audio::{
    AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
// Signed Int PCM codecs
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_S8, CODEC_ID_PCM_S16LE};
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_PCM_S16BE, CODEC_ID_PCM_S24BE, CODEC_ID_PCM_S32BE,
};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_S24LE, CODEC_ID_PCM_S32LE};
// Unsigned Int PCM codecs
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_U8, CODEC_ID_PCM_U16LE};
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_PCM_U16BE, CODEC_ID_PCM_U24BE, CODEC_ID_PCM_U32BE,
};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_U24LE, CODEC_ID_PCM_U32LE};
// Floating point PCM codecs
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_F32BE, CODEC_ID_PCM_F32LE};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_F64BE, CODEC_ID_PCM_F64LE};
// Planar PCM codecs
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_PCM_F32BE_PLANAR, CODEC_ID_PCM_F32LE_PLANAR, CODEC_ID_PCM_F64BE_PLANAR,
    CODEC_ID_PCM_F64LE_PLANAR, CODEC_ID_PCM_S8_PLANAR, CODEC_ID_PCM_S16BE_PLANAR,
    CODEC_ID_PCM_S16LE_PLANAR, CODEC_ID_PCM_S24BE_PLANAR, CODEC_ID_PCM_S24LE_PLANAR,
    CODEC_ID_PCM_S32BE_PLANAR, CODEC_ID_PCM_S32LE_PLANAR, CODEC_ID_PCM_U8_PLANAR,
    CODEC_ID_PCM_U16BE_PLANAR, CODEC_ID_PCM_U16LE_PLANAR, CODEC_ID_PCM_U24BE_PLANAR,
    CODEC_ID_PCM_U24LE_PLANAR, CODEC_ID_PCM_U32BE_PLANAR, CODEC_ID_PCM_U32LE_PLANAR,
};
// G711 ALaw and MuLaw PCM codecs
use symphonia_core::audio::AudioMut;
use symphonia_core::audio::conv::IntoSample;
use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_ALAW, CODEC_ID_PCM_MULAW};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::ReadBytes;
use symphonia_core::packet::PacketRef;

macro_rules! read_pcm {
    ($buf:expr, $planar:expr, $num_frames:expr, $fmt:tt, $read:expr) => {
        // Get buffer of the correct sample format.
        match &mut $buf {
            GenericAudioBuffer::$fmt(buf) => {
                if $planar {
                    // A planar packet holds every frame of the first channel, then every frame of
                    // the second channel, and so on.
                    buf.render_uninit(Some($num_frames));
                    for plane in buf.iter_planes_mut() {
                        for sample in plane.iter_mut() {
                            *sample = $read;
                        }
                    }
                    Ok($num_frames)
                }
                else {
                    // Read samples.
                    buf.render_with(None, |idx, audio_planes| -> Result<()> {
                        for plane in audio_planes.iter_mut() {
                            plane[idx] = $read;
                        }
                        Ok(())
                    })
                }
            }
            _ => unreachable!(),
        }
    };
}

// alaw_to_linear and mulaw_to_linear are adaptations of alaw2linear and ulaw2linear from g711.c by
// SUN Microsystems (unrestricted use license), license text for these parts follow below.
//
// ---
//
// This source code is a product of Sun Microsystems, Inc. and is provided for
// unrestricted use.  Users may copy or modify this source code without
// charge.
//
// SUN SOURCE CODE IS PROVIDED AS IS WITH NO WARRANTIES OF ANY KIND INCLUDING THE
// WARRANTIES OF DESIGN, MERCHANTIBILITY AND FITNESS FOR A PARTICULAR
// PURPOSE, OR ARISING FROM A COURSE OF DEALING, USAGE OR TRADE PRACTICE.
//
// Sun source code is provided with no support and without any obligation on the
// part of Sun Microsystems, Inc. to assist in its use, correction,
// modification or enhancement.
//
// SUN MICROSYSTEMS, INC. SHALL HAVE NO LIABILITY WITH RESPECT TO THE
// INFRINGEMENT OF COPYRIGHTS, TRADE SECRETS OR ANY PATENTS BY THIS SOFTWARE
// OR ANY PART THEREOF.
//
// In no event will Sun Microsystems, Inc. be liable for any lost revenue
// or profits or other special, indirect and consequential damages, even if
// Sun has been advised of the possibility of such damages.
//
// Sun Microsystems, Inc.
// 2550 Garcia Avenue
// Mountain View, California  94043
const XLAW_QUANT_MASK: u8 = 0x0f;
const XLAW_SEG_MASK: u8 = 0x70;
const XLAW_SEG_SHIFT: u32 = 4;

fn alaw_to_linear(mut a_val: u8) -> i16 {
    a_val ^= 0x55;

    let mut t = i16::from((a_val & XLAW_QUANT_MASK) << 4);
    let seg = (a_val & XLAW_SEG_MASK) >> XLAW_SEG_SHIFT;

    match seg {
        0 => t += 0x8,
        1 => t += 0x108,
        _ => t = (t + 0x108) << (seg - 1),
    }

    if a_val & 0x80 == 0x80 { t } else { -t }
}

fn mulaw_to_linear(mut mu_val: u8) -> i16 {
    const BIAS: i16 = 0x84;

    // Complement to obtain normal u-law value.
    mu_val = !mu_val;

    // Extract and bias the quantization bits. Then shift up by the segment number and subtract out
    // the bias.
    let mut t = i16::from((mu_val & XLAW_QUANT_MASK) << 3) + BIAS;
    t <<= (mu_val & XLAW_SEG_MASK) >> XLAW_SEG_SHIFT;

    if mu_val & 0x80 == 0x80 { BIAS - t } else { t - BIAS }
}

fn is_supported_pcm_codec(codec_id: AudioCodecId) -> bool {
    matches!(
        codec_id,
        CODEC_ID_PCM_S32LE
            | CODEC_ID_PCM_S32BE
            | CODEC_ID_PCM_S24LE
            | CODEC_ID_PCM_S24BE
            | CODEC_ID_PCM_S16LE
            | CODEC_ID_PCM_S16BE
            | CODEC_ID_PCM_S8
            | CODEC_ID_PCM_U32LE
            | CODEC_ID_PCM_U32BE
            | CODEC_ID_PCM_U24LE
            | CODEC_ID_PCM_U24BE
            | CODEC_ID_PCM_U16LE
            | CODEC_ID_PCM_U16BE
            | CODEC_ID_PCM_U8
            | CODEC_ID_PCM_F32LE
            | CODEC_ID_PCM_F32BE
            | CODEC_ID_PCM_F64LE
            | CODEC_ID_PCM_F64BE
            | CODEC_ID_PCM_ALAW
            | CODEC_ID_PCM_MULAW
    )
}

/// Map a planar PCM codec to the interleaved codec with the same sample coding.
fn interleaved_codec(codec_id: AudioCodecId) -> Option<AudioCodecId> {
    let interleaved = match codec_id {
        CODEC_ID_PCM_S32LE_PLANAR => CODEC_ID_PCM_S32LE,
        CODEC_ID_PCM_S32BE_PLANAR => CODEC_ID_PCM_S32BE,
        CODEC_ID_PCM_S24LE_PLANAR => CODEC_ID_PCM_S24LE,
        CODEC_ID_PCM_S24BE_PLANAR => CODEC_ID_PCM_S24BE,
        CODEC_ID_PCM_S16LE_PLANAR => CODEC_ID_PCM_S16LE,
        CODEC_ID_PCM_S16BE_PLANAR => CODEC_ID_PCM_S16BE,
        CODEC_ID_PCM_S8_PLANAR => CODEC_ID_PCM_S8,
        CODEC_ID_PCM_U32LE_PLANAR => CODEC_ID_PCM_U32LE,
        CODEC_ID_PCM_U32BE_PLANAR => CODEC_ID_PCM_U32BE,
        CODEC_ID_PCM_U24LE_PLANAR => CODEC_ID_PCM_U24LE,
        CODEC_ID_PCM_U24BE_PLANAR => CODEC_ID_PCM_U24BE,
        CODEC_ID_PCM_U16LE_PLANAR => CODEC_ID_PCM_U16LE,
        CODEC_ID_PCM_U16BE_PLANAR => CODEC_ID_PCM_U16BE,
        CODEC_ID_PCM_U8_PLANAR => CODEC_ID_PCM_U8,
        CODEC_ID_PCM_F32LE_PLANAR => CODEC_ID_PCM_F32LE,
        CODEC_ID_PCM_F32BE_PLANAR => CODEC_ID_PCM_F32BE,
        CODEC_ID_PCM_F64LE_PLANAR => CODEC_ID_PCM_F64LE,
        CODEC_ID_PCM_F64BE_PLANAR => CODEC_ID_PCM_F64BE,
        _ => return None,
    };
    Some(interleaved)
}

/// Pulse Code Modulation (PCM) decoder for all raw PCM, and log-PCM codecs.
pub struct PcmDecoder {
    params: AudioCodecParameters,
    /// The interleaved codec whose sample coding is decoded.
    codec: AudioCodecId,
    /// True if the packets store each channel contiguously instead of interleaved.
    planar: bool,
    /// The number of bits a coded sample should be shifted up to reach the decoded sample width.
    shift: u32,
    /// The number of bytes per coded frame.
    bytes_per_coded_frame: usize,
    buf: GenericAudioBuffer,
}

impl PcmDecoder {
    pub fn try_new(params: &AudioCodecParameters, _opts: &AudioDecoderOptions) -> Result<Self> {
        // Planar codecs code samples like their interleaved counterparts.
        let (codec, planar) = match interleaved_codec(params.codec) {
            Some(codec) => (codec, true),
            None => (params.codec, false),
        };

        // This decoder only supports certain PCM codecs.
        if !is_supported_pcm_codec(codec) {
            return unsupported_error("pcm: invalid codec");
        }

        let rate = match params.sample_rate {
            Some(rate) => rate,
            _ => return unsupported_error("pcm: sample rate is required"),
        };

        let spec = if let Some(channels) = &params.channels {
            // Atleast one channel is required.
            if channels.count() < 1 {
                return unsupported_error("pcm: number of channels cannot be 0");
            }

            AudioSpec::new(rate, channels.clone())
        }
        else {
            return unsupported_error("pcm: channels or channel_layout is required");
        };

        // If the maximum number of frames is known, use it to pre-allocate the audio buffer.
        // Otherwise, start with a 0 capacity and let the buffer grow dynamically while decoding.
        let capacity = params.max_frames_per_packet.unwrap_or(0) as usize;

        // Determine the sample format for the audio buffer based on the codec ID.
        let (sample_format, sample_format_width, bytes_per_coded_sample) = match codec {
            CODEC_ID_PCM_S32LE | CODEC_ID_PCM_S32BE => (SampleFormat::S32, 32, 4),
            CODEC_ID_PCM_S24LE | CODEC_ID_PCM_S24BE => (SampleFormat::S24, 24, 3),
            CODEC_ID_PCM_S16LE | CODEC_ID_PCM_S16BE => (SampleFormat::S16, 16, 2),
            CODEC_ID_PCM_S8 => (SampleFormat::S8, 8, 1),
            CODEC_ID_PCM_U32LE | CODEC_ID_PCM_U32BE => (SampleFormat::U32, 32, 4),
            CODEC_ID_PCM_U24LE | CODEC_ID_PCM_U24BE => (SampleFormat::U24, 24, 3),
            CODEC_ID_PCM_U16LE | CODEC_ID_PCM_U16BE => (SampleFormat::U16, 16, 2),
            CODEC_ID_PCM_U8 => (SampleFormat::U8, 8, 1),
            CODEC_ID_PCM_F32LE | CODEC_ID_PCM_F32BE => (SampleFormat::F32, 32, 4),
            CODEC_ID_PCM_F64LE | CODEC_ID_PCM_F64BE => (SampleFormat::F64, 64, 8),
            CODEC_ID_PCM_ALAW => (SampleFormat::S16, 16, 1),
            CODEC_ID_PCM_MULAW => (SampleFormat::S16, 16, 1),
            _ => unreachable!(),
        };

        // Do not allow the stated bits per coded sample to exceed the intrinsic width of the sample
        // format.
        if let Some(bits_per_coded_sample) = params.bits_per_coded_sample {
            if bits_per_coded_sample > sample_format_width {
                return decode_error("pcm: bits per coded sample greater-than sample format");
            }
        }

        // Do not allow the stated bits per sample to exceed the intrinsic width of the sample
        // format.
        if let Some(bits_per_sample) = params.bits_per_sample {
            if bits_per_sample > sample_format_width {
                return decode_error("pcm: bits per sample is greater-than sample format");
            }

            // Do not allow the bits per coded sample to exceed the bits per sample.
            if let Some(bits_per_coded_sample) = params.bits_per_coded_sample {
                if bits_per_coded_sample > bits_per_sample {
                    return decode_error("pcm: bits per coded sample greater-than bits per sample");
                }
            }
        }

        let decoded_width = params.bits_per_sample.unwrap_or(sample_format_width);
        let coded_width = params.bits_per_coded_sample.unwrap_or(decoded_width);

        // A-Law, Mu-Law, and floating point codecs have an implicit coded sample bit-width. Check
        // the decoded and coded widths are not illogical.
        match codec {
            CODEC_ID_PCM_F32LE | CODEC_ID_PCM_F32BE | CODEC_ID_PCM_F64LE | CODEC_ID_PCM_F64BE
            | CODEC_ID_PCM_ALAW | CODEC_ID_PCM_MULAW => {
                if decoded_width != sample_format_width {
                    return decode_error("pcm: unexpected bits per sample");
                }
                if coded_width != sample_format_width {
                    return decode_error("pcm: unexpected bits per coded sample");
                }
            }
            _ => (),
        }

        // Signed and unsigned integer PCM codecs may code fewer bits per sample than the actual
        // decoded sample width.
        let shift = decoded_width - coded_width;

        // The number of bytes per coded audio frame.
        let bytes_per_coded_frame = bytes_per_coded_sample * spec.channels().count();

        // Create an audio buffer of the correct sample format.
        let buf = GenericAudioBuffer::new(sample_format, spec, capacity);

        Ok(PcmDecoder { params: params.clone(), codec, planar, shift, bytes_per_coded_frame, buf })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        // Calculate the number of complete audio frames per-packet.
        let num_frames = packet.data.len() / self.bytes_per_coded_frame;

        // Clear and grow the audio buffer to the required capacity.
        self.buf.clear();
        self.buf.grow_capacity(num_frames);

        let mut reader = packet.as_buf_reader();

        let _ = match self.codec {
            CODEC_ID_PCM_S32LE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    S32,
                    (reader.read_i32()? << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_S32BE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    S32,
                    (reader.read_be_i32()? << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_S24LE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    S24,
                    ((reader.read_i24()? << 8) << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_S24BE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    S24,
                    ((reader.read_be_i24()? << 8) << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_S16LE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    S16,
                    (reader.read_i16()? << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_S16BE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    S16,
                    (reader.read_be_i16()? << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_S8 => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    S8,
                    (reader.read_i8()? << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_U32LE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    U32,
                    (reader.read_u32()? << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_U32BE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    U32,
                    (reader.read_be_u32()? << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_U24LE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    U24,
                    ((reader.read_u24()? << 8) << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_U24BE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    U24,
                    ((reader.read_be_u24()? << 8) << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_U16LE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    U16,
                    (reader.read_u16()? << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_U16BE => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    U16,
                    (reader.read_be_u16()? << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_U8 => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    U8,
                    (reader.read_u8()? << self.shift).into_sample()
                )
            }
            CODEC_ID_PCM_F32LE => {
                read_pcm!(self.buf, self.planar, num_frames, F32, reader.read_f32()?)
            }
            CODEC_ID_PCM_F32BE => {
                read_pcm!(self.buf, self.planar, num_frames, F32, reader.read_be_f32()?)
            }
            CODEC_ID_PCM_F64LE => {
                read_pcm!(self.buf, self.planar, num_frames, F64, reader.read_f64()?)
            }
            CODEC_ID_PCM_F64BE => {
                read_pcm!(self.buf, self.planar, num_frames, F64, reader.read_be_f64()?)
            }
            CODEC_ID_PCM_ALAW => {
                read_pcm!(self.buf, self.planar, num_frames, S16, alaw_to_linear(reader.read_u8()?))
            }
            CODEC_ID_PCM_MULAW => {
                read_pcm!(
                    self.buf,
                    self.planar,
                    num_frames,
                    S16,
                    mulaw_to_linear(reader.read_u8()?)
                )
            }
            _ => unsupported_error("pcm: codec is unsupported"),
        };

        Ok(())
    }
}

impl AudioDecoder for PcmDecoder {
    fn reset(&mut self) {
        // No state is stored between packets, therefore do nothing.
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

impl RegisterableAudioDecoder for PcmDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(PcmDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[
            support_audio_codec!(
                CODEC_ID_PCM_S32LE,
                "pcm_s32le",
                "PCM Signed 32-bit Little-Endian Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_S32BE,
                "pcm_s32be",
                "PCM Signed 32-bit Big-Endian Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_S24LE,
                "pcm_s24le",
                "PCM Signed 24-bit Little-Endian Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_S24BE,
                "pcm_s24be",
                "PCM Signed 24-bit Big-Endian Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_S16LE,
                "pcm_s16le",
                "PCM Signed 16-bit Little-Endian Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_S16BE,
                "pcm_s16be",
                "PCM Signed 16-bit Big-Endian Interleaved"
            ),
            support_audio_codec!(CODEC_ID_PCM_S8, "pcm_s8", "PCM Signed 8-bit Interleaved"),
            support_audio_codec!(
                CODEC_ID_PCM_U32LE,
                "pcm_u32le",
                "PCM Unsigned 32-bit Little-Endian Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U32BE,
                "pcm_u32be",
                "PCM Unsigned 32-bit Big-Endian Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U24LE,
                "pcm_u24le",
                "PCM Unsigned 24-bit Little-Endian Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U24BE,
                "pcm_u24be",
                "PCM Unsigned 24-bit Big-Endian Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U16LE,
                "pcm_u16le",
                "PCM Unsigned 16-bit Little-Endian Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U16BE,
                "pcm_u16be",
                "PCM Unsigned 16-bit Big-Endian Interleaved"
            ),
            support_audio_codec!(CODEC_ID_PCM_U8, "pcm_u8", "PCM Unsigned 8-bit Interleaved"),
            support_audio_codec!(
                CODEC_ID_PCM_F32LE,
                "pcm_f32le",
                "PCM 32-bit Little-Endian Floating Point Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_F32BE,
                "pcm_f32be",
                "PCM 32-bit Big-Endian Floating Point Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_F64LE,
                "pcm_f64le",
                "PCM 64-bit Little-Endian Floating Point Interleaved"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_F64BE,
                "pcm_f64be",
                "PCM 64-bit Big-Endian Floating Point Interleaved"
            ),
            support_audio_codec!(CODEC_ID_PCM_ALAW, "pcm_alaw", "PCM A-law"),
            support_audio_codec!(CODEC_ID_PCM_MULAW, "pcm_mulaw", "PCM Mu-law"),
            support_audio_codec!(
                CODEC_ID_PCM_S32LE_PLANAR,
                "pcm_s32le_planar",
                "PCM Signed 32-bit Little-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_S32BE_PLANAR,
                "pcm_s32be_planar",
                "PCM Signed 32-bit Big-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_S24LE_PLANAR,
                "pcm_s24le_planar",
                "PCM Signed 24-bit Little-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_S24BE_PLANAR,
                "pcm_s24be_planar",
                "PCM Signed 24-bit Big-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_S16LE_PLANAR,
                "pcm_s16le_planar",
                "PCM Signed 16-bit Little-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_S16BE_PLANAR,
                "pcm_s16be_planar",
                "PCM Signed 16-bit Big-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_S8_PLANAR,
                "pcm_s8_planar",
                "PCM Signed 8-bit Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U32LE_PLANAR,
                "pcm_u32le_planar",
                "PCM Unsigned 32-bit Little-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U32BE_PLANAR,
                "pcm_u32be_planar",
                "PCM Unsigned 32-bit Big-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U24LE_PLANAR,
                "pcm_u24le_planar",
                "PCM Unsigned 24-bit Little-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U24BE_PLANAR,
                "pcm_u24be_planar",
                "PCM Unsigned 24-bit Big-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U16LE_PLANAR,
                "pcm_u16le_planar",
                "PCM Unsigned 16-bit Little-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U16BE_PLANAR,
                "pcm_u16be_planar",
                "PCM Unsigned 16-bit Big-Endian Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_U8_PLANAR,
                "pcm_u8_planar",
                "PCM Unsigned 8-bit Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_F32LE_PLANAR,
                "pcm_f32le_planar",
                "PCM 32-bit Little-Endian Floating Point Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_F32BE_PLANAR,
                "pcm_f32be_planar",
                "PCM 32-bit Big-Endian Floating Point Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_F64LE_PLANAR,
                "pcm_f64le_planar",
                "PCM 64-bit Little-Endian Floating Point Planar"
            ),
            support_audio_codec!(
                CODEC_ID_PCM_F64BE_PLANAR,
                "pcm_f64be_planar",
                "PCM 64-bit Big-Endian Floating Point Planar"
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use symphonia_core::audio::{Audio, Channels};
    use symphonia_core::units::{Duration, Timestamp};

    /// Decodes one packet and returns each channel's samples.
    fn decode_planes<S>(
        codec: AudioCodecId,
        channels: u16,
        max_frames: Option<u64>,
        data: &[u8],
        get: fn(GenericAudioBufferRef<'_>) -> Vec<Vec<S>>,
    ) -> Vec<Vec<S>> {
        let mut params = AudioCodecParameters::new();
        params.for_codec(codec).with_sample_rate(48000).with_channels(Channels::Discrete(channels));
        if let Some(max_frames) = max_frames {
            params.with_max_frames_per_packet(max_frames);
        }
        let mut decoder = PcmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(0), data);
        get(decoder.decode_ref(&packet).unwrap())
    }

    fn s16_planes(buf: GenericAudioBufferRef<'_>) -> Vec<Vec<i16>> {
        match buf {
            GenericAudioBufferRef::S16(b) => b.iter_planes().map(|p| p.to_vec()).collect(),
            _ => unreachable!(),
        }
    }

    fn f64_planes(buf: GenericAudioBufferRef<'_>) -> Vec<Vec<f64>> {
        match buf {
            GenericAudioBufferRef::F64(b) => b.iter_planes().map(|p| p.to_vec()).collect(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn s16le_planar_reads_channels_contiguously() {
        let left = [1i16, 2, 3];
        let right = [-1i16, -2, -3];
        let data: Vec<u8> = left.iter().chain(&right).flat_map(|s| s.to_le_bytes()).collect();
        let expected = vec![left.to_vec(), right.to_vec()];

        // The packet must decode to exactly its frames, also when the buffer is larger.
        for max_frames in [None, Some(3), Some(64)] {
            let planes = decode_planes(CODEC_ID_PCM_S16LE_PLANAR, 2, max_frames, &data, s16_planes);
            assert_eq!(planes, expected);
        }
    }

    #[test]
    fn f64be_planar_reads_channels_contiguously() {
        let left = [0.5f64, -0.25];
        let right = [0.125f64, 1.0];
        let data: Vec<u8> = left.iter().chain(&right).flat_map(|s| s.to_be_bytes()).collect();
        let planes = decode_planes(CODEC_ID_PCM_F64BE_PLANAR, 2, None, &data, f64_planes);
        assert_eq!(planes, vec![left.to_vec(), right.to_vec()]);
    }
}
