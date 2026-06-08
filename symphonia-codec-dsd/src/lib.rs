// Symphonia DSD Codec
// Copyright (c) 2026 M0Rf30
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![allow(unsafe_code)] // Allow for SIMD optimizations

// Public modules for benchmarking
pub mod bitstream;
pub mod cic;
pub mod decimator;
pub mod fir;

use decimator::{DecimationConfig, DsdDecimator};

use symphonia_core::audio::{AsGenericAudioBufferRef, AudioSpec, GenericAudioBuffer, GenericAudioBufferRef};
use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::codecs::audio::{
    AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions, BitOrder, ChannelDataLayout, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{decode_error, unsupported_error, Result};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;
use log::debug;


/// DSD codec ID
pub const CODEC_ID_DSD: AudioCodecId = AudioCodecId::new(FourCc::new(*b"DSD\0"));

/// Decoder output mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderMode {
    /// Pass-through mode: output native DSD (u8 samples)
    PassThrough,
    /// PCM mode: convert DSD to PCM (f32 samples)
    Pcm { output_rate: u32 },
}

/// DSD Decoder
///
/// This decoder supports two modes:
/// 1. **Pass-through mode** (default): Outputs native DSD as U8 samples
///    (1 byte = 8 DSD bits). For audio outputs that support native DSD.
///
/// 2. **PCM mode**: Converts DSD to PCM F32 samples using high-quality
///    decimation filters (CIC + FIR). Enable by setting extra_data in
///    CodecParameters with PCM output rate.
///
/// # PCM Conversion
/// The PCM mode uses a two-stage decimation:
/// - Stage 1: CIC (Cascaded Integrator-Comb) filter for efficient high-ratio decimation
/// - Stage 2: FIR filter for passband droop compensation and anti-aliasing
///
/// Supported conversions:
/// - DSD64 (2.8224 MHz) -> 44.1kHz, 88.2kHz
/// - DSD128 (5.6448 MHz) -> 44.1kHz, 88.2kHz, 176.4kHz
/// - DSD256 (11.2896 MHz) -> 44.1kHz, 88.2kHz, 176.4kHz
/// - DSD512 (22.5792 MHz) -> 44.1kHz, 88.2kHz, 176.4kHz
pub struct DsdDecoder {
    params: AudioCodecParameters,
    mode: DecoderMode,
    // Pass-through mode buffer
    dsd_buf: GenericAudioBuffer,
    // PCM mode state
    pcm_buf: Option<GenericAudioBuffer>,
    decimator: Option<DsdDecimator>,
    scratch_dsd: Vec<Vec<u8>>,
    scratch_pcm: Vec<Vec<f32>>,
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


impl DsdDecoder {
    pub fn try_new(params: &AudioCodecParameters, _options: &AudioDecoderOptions) -> Result<Self> {
        // Verify this is a DSD codec
        if params.codec != CODEC_ID_DSD {
            return unsupported_error("dsd: codec type is not DSD");
        }

        // Get the signal specification
        let input_sample_rate = match params.sample_rate {
            Some(rate) => rate,
            None => return decode_error("dsd: missing sample rate"),
        };

        let channels = match &params.channels {
            Some(ch) => ch.clone(),
            None => return decode_error("dsd: missing channel layout"),
        };

        let channels_count = channels.count();

        let bit_order = params.bit_order.unwrap_or(BitOrder::LsbFirst);

        log::info!("DSD decoder: bit_order={:?}", bit_order);

        // Determine mode: check if PCM output rate is requested in extra_data
        // Format: first 4 bytes = u32 PCM output rate (little-endian)
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
                // Pass-through mode
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
                // PCM mode
                let config = DecimationConfig::new(input_sample_rate, output_rate)?;
                log::info!("DSD decimation: {}x total (CIC={}x, FIR={}x), {} stages, {} taps",
                          config.total_decimation, config.cic_decimation, config.fir_decimation,
                          config.cic_stages, config.fir_taps);
                let decimator = DsdDecimator::new(config, channels_count, bit_order);

                // Calculate PCM buffer size
                let input_duration = params.max_frames_per_packet.unwrap_or(4096);
                let pcm_duration = ((input_duration * 8) / config.total_decimation as u64).max(512);

                let pcm_spec = AudioSpec::new(output_rate, channels.clone());
                let pcm_buffer = GenericAudioBuffer::new(SampleFormat::F32, pcm_spec, pcm_duration as usize);

                debug!(
                    "DSD decoder (PCM mode): {} Hz -> {} Hz, decimation {}x, channels={}, pcm_duration={}",
                    input_sample_rate,
                    output_rate,
                    config.total_decimation,
                    channels_count,
                    pcm_duration
                );

                // Update codec params for PCM output
                let mut output_params = params.clone();
                output_params.sample_rate = Some(output_rate);
                output_params.bits_per_sample = Some(32); // f32

                (Some(pcm_buffer), Some(decimator), output_params)
            }
        };

        // DSD buffer always needed for input
        let dsd_spec = AudioSpec::new(input_sample_rate, channels.clone());
        let dsd_duration = params.max_frames_per_packet.unwrap_or(4096);
        let dsd_buf = GenericAudioBuffer::new(SampleFormat::U8, dsd_spec, dsd_duration as usize);

        Ok(DsdDecoder { params: output_params, mode, dsd_buf, pcm_buf, decimator, scratch_dsd: Vec::new(), scratch_pcm: Vec::new() })
    }

    /// Decode in pass-through mode (native DSD output)
    fn decode_passthrough(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        let data = packet.data;
        let channels = self.dsd_buf.spec().channels().count();

        if data.is_empty() || channels == 0 {
            return decode_error("dsd: invalid packet or channel count");
        }

        let samples_per_channel = data.len() / channels;

        if samples_per_channel > self.dsd_buf.capacity() {
            return decode_error("dsd: packet too large for buffer");
        }

        debug_assert!(samples_per_channel > 0, "samples_per_channel must be > 0");
        debug_assert!(
            samples_per_channel <= self.dsd_buf.capacity(),
            "samples_per_channel exceeds buffer capacity"
        );

        let spc = samples_per_channel;
        let layout = self.params.channel_data_layout.unwrap_or(ChannelDataLayout::Interleaved);

        self.dsd_buf.clear();

        match &mut self.dsd_buf {
            GenericAudioBuffer::U8(buf) => {
                buf.render_with(Some(samples_per_channel), |idx, audio_planes| -> Result<()> {
                    for (ch, plane) in audio_planes.iter_mut().enumerate() {
                        let src = match layout {
                            ChannelDataLayout::Interleaved => idx * channels + ch,
                            ChannelDataLayout::Planar => ch * spc + idx,
                        };
                        if src < data.len() {
                            plane[idx] = data[src];
                        } else {
                            // DSD idle/silence byte: 0x69 = 0b01101001
                            plane[idx] = 0x69;
                        }
                    }
                    Ok(())
                })?;
                Ok(buf.as_generic_audio_buffer_ref())
            }
            _ => unreachable!(),
        }
    }

    /// Decode in PCM mode (convert DSD to PCM)
    fn decode_pcm(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        let data = packet.data;
        let channels = self.dsd_buf.spec().channels().count();

        if data.is_empty() || channels == 0 {
            return decode_error("dsd: invalid packet or channel count");
        }

        let spc = data.len() / channels;
        let layout = self.params.channel_data_layout.unwrap_or(ChannelDataLayout::Interleaved);

        // Obtain pcm_capacity before taking any other borrows
        let pcm_capacity = match self.pcm_buf.as_ref() {
            Some(b) => b.capacity(),
            None => return decode_error("dsd: no PCM buffer"),
        };

        // Build per-channel DSD planes in scratch_dsd (inner vecs reused; no hot-path allocation)
        self.scratch_dsd.resize_with(channels, Vec::new);
        for ch in 0..channels {
            self.scratch_dsd[ch].clear();
            match layout {
                ChannelDataLayout::Interleaved => {
                    self.scratch_dsd[ch].extend((0..spc).map(|i| data[i * channels + ch]));
                }
                ChannelDataLayout::Planar => {
                    self.scratch_dsd[ch].extend_from_slice(&data[ch * spc..ch * spc + spc]);
                }
            }
        }

        // Resize PCM scratch vecs (reuses inner vec capacity after first packet)
        self.scratch_pcm.resize_with(channels, Vec::new);
        for ch in 0..channels {
            self.scratch_pcm[ch].resize(pcm_capacity, 0.0);
        }

        let decimator = match self.decimator.as_mut() {
            Some(d) => d,
            None => return decode_error("dsd: no decimator"),
        };

        let dsd_plane_refs: Vec<&[u8]> = self.scratch_dsd.iter().map(|v| v.as_slice()).collect();
        let mut pcm_plane_refs: Vec<&mut [f32]> = self.scratch_pcm.iter_mut().map(|v| v.as_mut_slice()).collect();
        let samples_produced = decimator.process_planar(&dsd_plane_refs, &mut pcm_plane_refs)?;
        drop(pcm_plane_refs);

        if samples_produced == 0 {
            return decode_error("dsd: decimator produced no output");
        }

        // Read-only snapshot refs into scratch_pcm for the render closure (small Vec-of-refs; acceptable per-call alloc)
        let pcm_snapshot: Vec<&[f32]> = self.scratch_pcm.iter().map(|v| v.as_slice()).collect();

        let pcm_buf = match self.pcm_buf.as_mut() {
            Some(b) => b,
            None => return decode_error("dsd: no PCM buffer"),
        };
        pcm_buf.clear();

        match pcm_buf {
            GenericAudioBuffer::F32(buf) => {
                buf.render_with(Some(samples_produced), |idx, audio_planes| -> Result<()> {
                    for (ch, plane) in audio_planes.iter_mut().enumerate() {
                        plane[idx] = pcm_snapshot[ch][idx];
                    }
                    Ok(())
                })?;
                Ok(buf.as_generic_audio_buffer_ref())
            }
            _ => unreachable!(),
        }
    }
}