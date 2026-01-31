// Symphonia DSD Codec
// Copyright (c) 2026 M0Rf30
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]

mod bitstream;
mod cic;
mod decimator;
mod fir;

use decimator::{DecimationConfig, DsdDecimator};

use symphonia_core::audio::{AsAudioBufferRef, AudioBuffer, AudioBufferRef, Signal, SignalSpec};
use symphonia_core::codecs::{decl_codec_type, CodecDescriptor, CodecParameters, CodecType};
use symphonia_core::codecs::{Decoder, DecoderOptions, FinalizeResult};
use symphonia_core::errors::{decode_error, unsupported_error, Result};
use symphonia_core::formats::Packet;
use symphonia_core::support_codec;

use log::debug;

/// DSD codec type "DSD\0"
pub const CODEC_TYPE_DSD: CodecType = decl_codec_type(b"DSD\0");

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
    params: CodecParameters,
    mode: DecoderMode,
    // Pass-through mode buffer
    dsd_buf: AudioBuffer<u8>,
    // PCM mode state
    pcm_buf: Option<AudioBuffer<f32>>,
    decimator: Option<DsdDecimator>,
}

impl Decoder for DsdDecoder {
    fn try_new(params: &CodecParameters, options: &DecoderOptions) -> Result<Self> {
        // Verify this is a DSD codec
        if params.codec != CODEC_TYPE_DSD {
            return unsupported_error("dsd: codec type is not DSD");
        }

        // Get the signal specification
        let input_sample_rate = match params.sample_rate {
            Some(rate) => rate,
            None => return decode_error("dsd: missing sample rate"),
        };

        let channels = match params.channels {
            Some(ch) => ch,
            None => return decode_error("dsd: missing channel layout"),
        };

        let bit_order = params.bit_order.unwrap_or(symphonia_core::codecs::BitOrder::LsbFirst);

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
                let spec = SignalSpec::new(input_sample_rate, channels);
                let duration = params.max_frames_per_packet.unwrap_or(4096);

                debug!(
                    "DSD decoder (pass-through): rate={}, channels={}, duration={}",
                    spec.rate,
                    spec.channels.count(),
                    duration
                );

                (None, None, params.clone())
            }
            DecoderMode::Pcm { output_rate } => {
                // PCM mode
                let config = DecimationConfig::new(input_sample_rate, output_rate)?;
                let decimator = DsdDecimator::new(config, channels.count(), bit_order);

                // Calculate PCM buffer size
                let input_duration = params.max_frames_per_packet.unwrap_or(4096);
                let pcm_duration = (input_duration / config.total_decimation as u64).max(512);

                let pcm_spec = SignalSpec::new(output_rate, channels);
                let pcm_buffer = AudioBuffer::new(pcm_duration, pcm_spec);

                debug!(
                    "DSD decoder (PCM mode): {} Hz -> {} Hz, decimation {}x, channels={}, pcm_duration={}",
                    input_sample_rate,
                    output_rate,
                    config.total_decimation,
                    channels.count(),
                    pcm_duration
                );

                // Update codec params for PCM output
                let mut output_params = params.clone();
                output_params.sample_rate = Some(output_rate);
                output_params.bits_per_sample = Some(32); // f32
                if let Some(n_frames) = params.n_frames {
                    output_params.n_frames = Some(n_frames / config.total_decimation as u64);
                }

                (Some(pcm_buffer), Some(decimator), output_params)
            }
        };

        // DSD buffer always needed for input
        let dsd_spec = SignalSpec::new(input_sample_rate, channels);
        let dsd_duration = params.max_frames_per_packet.unwrap_or(4096);
        let dsd_buf = AudioBuffer::new(dsd_duration, dsd_spec);

        Ok(DsdDecoder { params: output_params, mode, dsd_buf, pcm_buf, decimator })
    }

    fn supported_codecs() -> &'static [CodecDescriptor] {
        &[support_codec!(CODEC_TYPE_DSD, "dsd", "Direct Stream Digital")]
    }

    fn reset(&mut self) {
        self.dsd_buf.clear();
        if let Some(ref mut pcm_buf) = self.pcm_buf {
            pcm_buf.clear();
        }
        if let Some(ref mut decimator) = self.decimator {
            decimator.reset();
        }
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    fn decode(&mut self, packet: &Packet) -> Result<AudioBufferRef<'_>> {
        match self.mode {
            DecoderMode::PassThrough => self.decode_passthrough(packet),
            DecoderMode::Pcm { .. } => self.decode_pcm(packet),
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> AudioBufferRef<'_> {
        match self.mode {
            DecoderMode::PassThrough => self.dsd_buf.as_audio_buffer_ref(),
            DecoderMode::Pcm { .. } => {
                if let Some(ref pcm_buf) = self.pcm_buf {
                    pcm_buf.as_audio_buffer_ref()
                }
                else {
                    // Shouldn't happen
                    self.dsd_buf.as_audio_buffer_ref()
                }
            }
        }
    }
}

impl DsdDecoder {
    /// Decode in pass-through mode (native DSD output)
    fn decode_passthrough(&mut self, packet: &Packet) -> Result<AudioBufferRef<'_>> {
        let data = packet.buf();
        let channels = self.dsd_buf.spec().channels.count();

        // Validate we have data for at least one sample per channel
        if data.is_empty() || channels == 0 {
            return decode_error("dsd: invalid packet or channel count");
        }

        // Calculate samples per channel in this packet
        let samples_per_channel = data.len() / channels;

        // Make sure we have enough space
        if samples_per_channel > self.dsd_buf.capacity() {
            return decode_error("dsd: packet too large for buffer");
        }

        // Validate samples_per_channel is within reasonable bounds
        debug_assert!(samples_per_channel > 0, "samples_per_channel must be > 0");
        debug_assert!(
            samples_per_channel <= self.dsd_buf.capacity(),
            "samples_per_channel exceeds buffer capacity"
        );

        // Clear and resize buffer for this packet
        self.dsd_buf.clear();
        self.dsd_buf.render_reserved(Some(samples_per_channel));

        // Fill buffer with DSD data
        // DSD data is interleaved by default in DSF files
        self.dsd_buf.fill(|audio_planes, idx| -> Result<()> {
            let data_offset = idx * channels;
            for (ch, plane) in audio_planes.planes().iter_mut().enumerate() {
                if data_offset + ch < data.len() {
                    plane[idx] = data[data_offset + ch];
                }
                else {
                    // DSD silence pattern: 0x55 = 0b01010101 (alternating bits)
                    // This represents zero signal in DSD's 1-bit encoding
                    plane[idx] = 0x55;
                }
            }
            Ok(())
        })?;

        Ok(self.dsd_buf.as_audio_buffer_ref())
    }

    /// Decode in PCM mode (convert DSD to PCM)
    fn decode_pcm(&mut self, packet: &Packet) -> Result<AudioBufferRef<'_>> {
        let data = packet.buf();
        let channels = self.dsd_buf.spec().channels.count();

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

        // Calculate samples per channel
        let samples_per_channel = data.len() / channels;

        // For planar processing, we need to extract each channel's data
        // Input data is interleaved: [ch0, ch1, ch0, ch1, ...]
        // We need planar: [[ch0, ch0, ...], [ch1, ch1, ...]]

        let mut dsd_planes: Vec<Vec<u8>> = vec![Vec::with_capacity(samples_per_channel); channels];
        for (idx, &byte) in data.iter().enumerate() {
            let ch = idx % channels;
            dsd_planes[ch].push(byte);
        }

        // Prepare planar references for decimator
        let dsd_plane_refs: Vec<&[u8]> = dsd_planes.iter().map(|v| v.as_slice()).collect();

        // Clear PCM buffer
        pcm_buf.clear();

        // Get mutable plane references
        let mut pcm_plane_data: Vec<Vec<f32>> = (0..channels)
            .map(|_| vec![0.0f32; pcm_buf.capacity()])
            .collect();

        let mut pcm_plane_refs: Vec<&mut [f32]> = pcm_plane_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        // Process through decimator
        let samples_produced = decimator.process_planar(&dsd_plane_refs, &mut pcm_plane_refs)?;

        if samples_produced == 0 {
            return decode_error("dsd: decimator produced no output");
        }

        // Render PCM buffer with produced samples
        pcm_buf.render_reserved(Some(samples_produced));

        // Copy data into PCM buffer
        pcm_buf.fill(|audio_planes, idx| -> Result<()> {
            for (ch, plane) in audio_planes.planes().iter_mut().enumerate() {
                if idx < samples_produced {
                    plane[idx] = pcm_plane_data[ch][idx];
                }
                else {
                    plane[idx] = 0.0;
                }
            }
            Ok(())
        })?;

        Ok(pcm_buf.as_audio_buffer_ref())
    }
}
