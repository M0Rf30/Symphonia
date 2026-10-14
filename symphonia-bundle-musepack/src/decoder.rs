// Symphonia Musepack decoder
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `AudioDecoder` glue around [`crate::decoder_core::Decoder`].
//!
//! This file is Symphonia-specific plumbing (packet framing, `AudioBuffer` rendering, trait
//! impls) and is not ported from libmpcdec; the actual decode algorithm lives in
//! `decoder_core.rs`, `synth.rs`, `requant.rs`, `bits.rs` and `huffman/`.

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, GenericAudioBufferRef,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_MUSEPACK;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Error, Result, decode_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::bits::BitReader;
use crate::decoder_core::{Decoder as Core, FRAME_LENGTH, ScfSync};
use crate::demuxer::{PACKET_TAG_NOISE, PACKET_TAG_PLAIN, PACKET_TAG_SILENCE, PACKET_TAG_SYNC};

/// Musepack (SV4-SV8) decoder.
pub struct MpcDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    core: Core,
    stream_version: u32,
    block_pwr: u8,
    channels: usize,
    buf: AudioBuffer<f32>,
}

/// The part of `AudioCodecParameters::extra_data` the decoder uses (see
/// `demuxer::encode_extra_data`; the trailing sample counts it also carries are only relevant to
/// the demuxer, which turns them into packet trims).
struct ExtraData {
    stream_version: u32,
    max_band: i32,
    ms: bool,
    channels: u32,
    block_pwr: u8,
    /// SV4-SV5 stream features, see `Decoder::set_sv46_layout`.
    raw_res: bool,
    is_start: Option<usize>,
}

fn parse_extra_data(data: &[u8]) -> Result<ExtraData> {
    if data.len() < 21 {
        return decode_error("musepack: extra data too short");
    }
    Ok(ExtraData {
        stream_version: u32::from(data[0]),
        max_band: i32::from(data[1]),
        ms: data[2] != 0,
        channels: u32::from(data[3]),
        block_pwr: data[4],
        // Absent from the extra data of streams that have no use for them.
        raw_res: data.get(21).is_some_and(|&b| b != 0),
        is_start: data.get(22).filter(|&&b| b != 0xFF).map(|&b| usize::from(b)),
    })
}

impl MpcDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_MUSEPACK {
            return decode_error("musepack: invalid codec");
        }
        let extra =
            params.extra_data.as_ref().ok_or(Error::DecodeError("musepack: missing extra data"))?;
        let extra = parse_extra_data(extra)?;

        let sample_rate =
            params.sample_rate.ok_or(Error::DecodeError("musepack: missing sample rate"))?;
        let channels =
            params.channels.clone().ok_or(Error::DecodeError("musepack: missing channels"))?;

        let mut core = Core::new(extra.stream_version, extra.max_band, extra.ms, extra.channels);
        core.set_sv46_layout(extra.raw_res, extra.is_start);

        let spec = symphonia_core::audio::AudioSpec::new(sample_rate, channels);
        let max_frames = (1usize << extra.block_pwr.min(20)) * FRAME_LENGTH;
        let buf = AudioBuffer::new(spec, max_frames);

        Ok(MpcDecoder {
            params: params.clone(),
            opts: *opts,
            core,
            stream_version: extra.stream_version,
            block_pwr: extra.block_pwr,
            channels: extra.channels as usize,
            buf,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let channels = self.channels;

        // Every packet starts with a tag byte, optionally followed by decoder state that the
        // demuxer recovered for a seek (SV4-SV7: scale factors and noise generator, SV8: noise
        // generator; everything else is rewritten by each packet).
        let (&tag, mut data) =
            packet.data.split_first().ok_or(Error::DecodeError("musepack: empty packet"))?;
        match tag {
            PACKET_TAG_PLAIN => (),
            PACKET_TAG_SYNC if self.stream_version < 8 => {
                let sync = ScfSync::read_from(data)
                    .ok_or(Error::DecodeError("musepack: truncated SV7 state"))?;
                self.core.set_scf_sync(&sync);
                data = &data[ScfSync::ENCODED_LEN..];
            }
            PACKET_TAG_NOISE if self.stream_version >= 8 => {
                let state = data.get(..8).ok_or(Error::DecodeError("musepack: truncated state"))?;
                let word = |i: usize| {
                    u32::from_le_bytes([state[i], state[i + 1], state[i + 2], state[i + 3]])
                };
                self.core.set_noise_state([word(0), word(4)]);
                data = &data[8..];
            }
            PACKET_TAG_SILENCE if (4..7).contains(&self.stream_version) => (),
            _ => return decode_error("musepack: invalid packet tag"),
        }
        let silent = tag == PACKET_TAG_SILENCE;

        // The demuxer sizes `dur + trim_start + trim_end` to the number of frames actually
        // present (the last SV8 packet may hold fewer than `2^block_pwr`).
        let max_frames: u64 =
            if self.stream_version >= 8 { 1u64 << self.block_pwr.min(20) } else { 1 };
        let block_frames = match packet.block_dur().get() {
            0 => max_frames,
            dur => dur.div_ceil(FRAME_LENGTH as u64).min(max_frames),
        };

        self.buf.clear();

        let mut r = BitReader::new(data);
        for i in 0..block_frames {
            let is_key_frame = i == 0;
            let start = self.buf.frames();
            self.buf.render_uninit(Some(FRAME_LENGTH));
            let end = start + FRAME_LENGTH;
            let (core, buf) = (&mut self.core, &mut self.buf);
            let mut decode = |l: &mut [f32], r_plane: Option<&mut [f32]>| {
                if silent {
                    core.decode_silent_frame(l, r_plane)
                }
                else {
                    core.decode_frame(&mut r, is_key_frame, l, r_plane)
                }
            };
            let ok = if channels > 1 {
                match buf.plane_pair_mut(0, 1) {
                    Some((l, r_plane)) => {
                        decode(&mut l[start..end], Some(&mut r_plane[start..end]))
                    }
                    None => false,
                }
            }
            else {
                match buf.plane_mut(0) {
                    Some(l) => decode(&mut l[start..end], None),
                    None => false,
                }
            };
            if !ok {
                return decode_error("musepack: frame buffer too small");
            }
        }

        // Delay, padding and seek pre-roll are all expressed by the demuxer as packet trims.
        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for MpcDecoder {
    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs().first().expect("at least one codec registered").info
    }

    fn reset(&mut self) {
        // Nothing to do: every packet carries all the state needed to decode it, so a seek needs
        // no help from the caller. SV8 packets start with a key frame; the first SV7 packet
        // after a seek is tagged with the scale-factor state recovered by the demuxer (see
        // `ScfSync`); and the synthesis filter's history is flushed by the pre-roll packet the
        // demuxer emits (and trims away entirely) before the first audible one.
        self.buf.clear();
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

impl RegisterableAudioDecoder for MpcDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(MpcDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_MUSEPACK, "musepack", "Musepack (SV4-SV8)")]
    }
}
