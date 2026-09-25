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

use symphonia_core::audio::{AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, GenericAudioBufferRef};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_MUSEPACK;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{decode_error, Error, Result};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::bits::BitReader;
use crate::decoder_core::{Decoder as Core, FRAME_LENGTH};

/// Musepack (SV7/SV8) decoder.
pub struct MpcDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    core: Core,
    stream_version: u32,
    block_pwr: u8,
    channels: usize,
    buf: AudioBuffer<f32>,
    scratch: Vec<f32>,
}

struct ExtraData {
    stream_version: u32,
    max_band: i32,
    ms: bool,
    channels: u32,
    block_pwr: u8,
    decoder_samples: u64,
    beg_silence: u64,
}

fn parse_extra_data(data: &[u8]) -> Result<ExtraData> {
    if data.len() < 21 {
        return decode_error("musepack: extra data too short");
    }
    let decoder_samples = u64::from_le_bytes(data[5..13].try_into().unwrap());
    let beg_silence = u64::from_le_bytes(data[13..21].try_into().unwrap());
    Ok(ExtraData {
        stream_version: u32::from(data[0]),
        max_band: i32::from(data[1]),
        ms: data[2] != 0,
        channels: u32::from(data[3]),
        block_pwr: data[4],
        decoder_samples,
        beg_silence,
    })
}

impl MpcDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_MUSEPACK {
            return decode_error("musepack: invalid codec");
        }
        let extra = params
            .extra_data
            .as_ref()
            .ok_or(Error::DecodeError("musepack: missing extra data"))?;
        let extra = parse_extra_data(extra)?;

        let sample_rate =
            params.sample_rate.ok_or(Error::DecodeError("musepack: missing sample rate"))?;
        let channels =
            params.channels.clone().ok_or(Error::DecodeError("musepack: missing channels"))?;

        let mut core = Core::new(extra.stream_version, extra.max_band, extra.ms, extra.channels);
        core.set_total_samples(extra.decoder_samples, extra.beg_silence);

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
            scratch: vec![0.0; FRAME_LENGTH * extra.channels.max(1) as usize],
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;
        let channels = self.channels;

        let block_frames: u64 =
            if self.stream_version >= 8 { 1u64 << self.block_pwr.min(20) } else { 1 };

        self.buf.clear();

        let mut r = BitReader::new(data);
        for i in 0..block_frames {
            self.scratch.iter_mut().for_each(|s| *s = 0.0);
            let is_key_frame = i == 0;
            let result = self.core.decode_frame(&mut r, is_key_frame, &mut self.scratch);
            if result.samples == 0 {
                continue;
            }
            let start = self.buf.frames();
            self.buf.render_uninit(Some(result.samples));
            for ch in 0..channels {
                if let Some(plane) = self.buf.plane_mut(ch) {
                    for (n, sample) in plane[start..start + result.samples].iter_mut().enumerate()
                    {
                        *sample = self.scratch[n * channels + ch];
                    }
                }
            }
        }

        // Gapless trimming is inherent to `decoder_core::Decoder::decode_frame` (it mirrors
        // libmpcdec's own encoder-delay/padding handling exactly, via `samples_to_skip` and the
        // stream's total sample count -- see `decoder_core.rs`), so by the time execution
        // reaches here the buffer already contains exactly the audible samples for this packet.
        // `opts.gapless` has no additional effect for this codec (there is no "untrimmed" mode to
        // fall back to, unlike e.g. MP3's post-hoc `AudioBuffer::trim`), which we document rather
        // than silently ignore.
        let _ = self.opts.gapless;

        Ok(())
    }
}

impl AudioDecoder for MpcDecoder {
    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs().first().expect("at least one codec registered").info
    }

    fn reset(&mut self) {
        // Recreate the core decoder, but preserve stream configuration. Any seek that landed us
        // here is frame-accurate (see `demuxer::sv7`/`demuxer::sv8`'s `seek`), and for SV8 every
        // packet's first frame is a self-contained "key frame" (`is_key_frame == true` forces a
        // fresh `Max_used_Band`/`DSCF_Flag` independent of any prior decoder state -- see
        // `decoder_core::Decoder::read_bitstream_sv8`), so a fresh decoder is always valid there.
        //
        // For SV7, band-to-band deltas are frame-local, but scale-factor (SCF) deltas persist
        // across frames with no independent "key frame" concept; libmpcdec's own seek
        // (`mpc_demux_seek_sample_inner`) handles this by calling `mpc_decoder_reset_scf(d, 1)`
        // (a neutral, non-zero baseline) before resuming decode at an arbitrary frame, which we
        // replicate here.
        let params = self.params.clone();
        if let Some(extra) = params.extra_data.as_ref().and_then(|d| parse_extra_data(d).ok()) {
            let mut core =
                Core::new(extra.stream_version, extra.max_band, extra.ms, extra.channels);
            core.set_total_samples(extra.decoder_samples, extra.beg_silence);
            if extra.stream_version == 7 {
                core.reset_scf(1);
            }
            self.core = core;
        }
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
        &[support_audio_codec!(CODEC_ID_MUSEPACK, "musepack", "Musepack (SV7/SV8)")]
    }
}
