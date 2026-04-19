// Symphonia DSD Codec
// Copyright (c) 2026 M0Rf30
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![allow(unsafe_code)]

pub mod bitstream;
pub mod cic;
pub mod decimator;
pub mod fir;

use decimator::{DecimationConfig, DsdDecimator};

use symphonia_core::audio::{AsGenericAudioBufferRef, Audio, AudioBuffer, GenericAudioBufferRef, AudioSpec};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, BitOrder, FinalizeResult,
};
pub use symphonia_core::codecs::audio::well_known::CODEC_ID_DSD;
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{decode_error, unsupported_error, Result};
use symphonia_core::packet::Packet;
use symphonia_core::support_audio_codec;

use log::debug;

static CODEC_INFO: CodecInfo = CodecInfo {
    short_name: "dsd",
    long_name: "Direct Stream Digital",
    profiles: &[],
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderMode {
    PassThrough,
    Pcm { output_rate: u32 },
}

pub struct DsdDecoder {
    params: AudioCodecParameters,
    mode: DecoderMode,
    dsd_buf: AudioBuffer<u8>,
    pcm_buf: Option<AudioBuffer<f32>>,
    decimator: Option<DsdDecimator>,
}

impl DsdDecoder {
    pub fn try_new(params: &AudioCodecParameters, _options: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_DSD {
            return unsupported_error("dsd: codec type is not DSD");
        }

        let input_sample_rate = match params.sample_rate {
            Some(rate) => rate,
            None => return decode_error("dsd: missing sample rate"),
        };

        let channels = match &params.channels {
            Some(ch) => ch.clone(),
            None => return decode_error("dsd: missing channel layout"),
        };

        let bit_order = params.bit_order.unwrap_or(BitOrder::LsbFirst);

        log::info!("DSD decoder: bit_order={:?}", bit_order);

        let mode = if let Some(extra) = params.extra_data.as_ref() {
            if extra.len() >= 4 {
                let pcm_rate = u32::from_le_bytes([extra[0], extra[1], extra[2], extra[3]]);
                if pcm_rate > 0 {
                    DecoderMode::Pcm { output_rate: pcm_rate }
                }
                else {
                    DecoderMode::PassThrough
                }
            }
            else {
                DecoderMode::PassThrough
            }
        }
        else {
            DecoderMode::PassThrough
        };

        let (pcm_buf, decimator, output_params) = match mode {
            DecoderMode::PassThrough => {
                let spec = AudioSpec::new(input_sample_rate, channels.clone());
                let duration = params.max_frames_per_packet.unwrap_or(4096);

                debug!(
                    "DSD decoder (pass-through): rate={}, channels={}, duration={}",
                    spec.rate(),
                    spec.channels().count(),
                    duration
                );

                (None, None, params.clone())
            }
            DecoderMode::Pcm { output_rate } => {
                let config = DecimationConfig::new(input_sample_rate, output_rate)?;
                log::info!("DSD decimation: {}x total (CIC={}x, FIR={}x), {} stages, {} taps",
                          config.total_decimation, config.cic_decimation, config.fir_decimation,
                          config.cic_stages, config.fir_taps);
                let decimator = DsdDecimator::new(config, channels.count(), bit_order);

                let input_duration = params.max_frames_per_packet.unwrap_or(4096);
                let pcm_duration = ((input_duration * 8) / config.total_decimation as u64).max(512);

                let pcm_spec = AudioSpec::new(output_rate, channels.clone());
                let pcm_buffer = AudioBuffer::new(pcm_spec, pcm_duration as usize);

                debug!(
                    "DSD decoder (PCM mode): {} Hz -> {} Hz, decimation {}x, channels={}, pcm_duration={}",
                    input_sample_rate,
                    output_rate,
                    config.total_decimation,
                    channels.count(),
                    pcm_duration
                );

                let mut output_params = params.clone();
                output_params.sample_rate = Some(output_rate);
                output_params.bits_per_sample = Some(32);

                (Some(pcm_buffer), Some(decimator), output_params)
            }
        };

        let dsd_spec = AudioSpec::new(input_sample_rate, channels);
        let dsd_duration = params.max_frames_per_packet.unwrap_or(4096);
        let dsd_buf = AudioBuffer::new(dsd_spec, dsd_duration as usize);

        Ok(DsdDecoder { params: output_params, mode, dsd_buf, pcm_buf, decimator })
    }

    fn decode_passthrough(&mut self, packet: &Packet) -> Result<GenericAudioBufferRef<'_>> {
        let data = &packet.data;
        let channels = self.dsd_buf.spec().channels().count();

        if data.is_empty() || channels == 0 {
            return decode_error("dsd: invalid packet or channel count");
        }

        let samples_per_channel = data.len() / channels;

        self.dsd_buf.clear();
        self.dsd_buf.grow_capacity(samples_per_channel);

        self.dsd_buf.render_with(Some(samples_per_channel), |idx, audio_planes| -> Result<()> {
            let data_offset = idx * channels;
            for (ch, plane) in audio_planes.iter_mut().enumerate() {
                if data_offset + ch < data.len() {
                    plane[idx] = data[data_offset + ch];
                }
                else {
                    plane[idx] = 0x55;
                }
            }
            Ok(())
        })?;

        Ok(self.dsd_buf.as_generic_audio_buffer_ref())
    }

    fn decode_pcm(&mut self, packet: &Packet) -> Result<GenericAudioBufferRef<'_>> {
        let data = &packet.data;
        let channels = self.dsd_buf.spec().channels().count();

        if data.is_empty() || channels == 0 {
            return decode_error("dsd: invalid packet or channel count");
        }

        let decimator = match self.decimator.as_mut() {
            Some(d) => d,
            None => return decode_error("dsd: no decimator"),
        };
        let pcm_buf = match self.pcm_buf.as_mut() {
            Some(b) => b,
            None => return decode_error("dsd: no PCM buffer"),
        };

        let samples_per_channel = data.len() / channels;

        let mut dsd_planes: Vec<Vec<u8>> = Vec::with_capacity(channels);
        for ch in 0..channels {
            let start = ch * samples_per_channel;
            let end = start + samples_per_channel;
            dsd_planes.push(data[start..end].to_vec());
        }

        let dsd_plane_refs: Vec<&[u8]> = dsd_planes.iter().map(|v| v.as_slice()).collect();

        pcm_buf.clear();

        let mut pcm_plane_data: Vec<Vec<f32>> = (0..channels)
            .map(|_| vec![0.0f32; pcm_buf.capacity()])
            .collect();

        let mut pcm_plane_refs: Vec<&mut [f32]> = pcm_plane_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let samples_produced = decimator.process_planar(&dsd_plane_refs, &mut pcm_plane_refs)?;

        if samples_produced == 0 {
            return decode_error("dsd: decimator produced no output");
        }

        pcm_buf.grow_capacity(samples_produced);

        pcm_buf.render_with(Some(samples_produced), |idx, audio_planes| -> Result<()> {
            for (ch, plane) in audio_planes.iter_mut().enumerate() {
                if idx < samples_produced {
                    plane[idx] = pcm_plane_data[ch][idx];
                }
                else {
                    plane[idx] = 0.0;
                }
            }
            Ok(())
        })?;

        Ok(pcm_buf.as_generic_audio_buffer_ref())
    }
}

impl AudioDecoder for DsdDecoder {
    fn reset(&mut self) {
        self.dsd_buf.clear();
        if let Some(ref mut pcm_buf) = self.pcm_buf {
            pcm_buf.clear();
        }
        if let Some(ref mut decimator) = self.decimator {
            decimator.reset();
        }
    }

    fn codec_info(&self) -> &CodecInfo {
        &CODEC_INFO
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode(&mut self, packet: &Packet) -> Result<GenericAudioBufferRef<'_>> {
        match self.mode {
            DecoderMode::PassThrough => self.decode_passthrough(packet),
            DecoderMode::Pcm { .. } => self.decode_pcm(packet),
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        match self.mode {
            DecoderMode::PassThrough => self.dsd_buf.as_generic_audio_buffer_ref(),
            DecoderMode::Pcm { .. } => {
                if let Some(ref pcm_buf) = self.pcm_buf {
                    pcm_buf.as_generic_audio_buffer_ref()
                }
                else {
                    self.dsd_buf.as_generic_audio_buffer_ref()
                }
            }
        }
    }
}

impl RegisterableAudioDecoder for DsdDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(DsdDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_DSD, "dsd", "Direct Stream Digital")]
    }
}
