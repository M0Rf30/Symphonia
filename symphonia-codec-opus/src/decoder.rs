// Opus Decoder - Symphonia Integration
// Implements the Decoder trait for full Symphonia integration
// SPDX-License-Identifier: MPL-2.0

use symphonia_core::audio::{AudioBuffer, AudioBufferRef, AsAudioBufferRef, Signal, SignalSpec};
use symphonia_core::codecs::{
    CodecDescriptor, CodecParameters, Decoder, DecoderOptions, FinalizeResult, CODEC_TYPE_OPUS,
};
use symphonia_core::errors::{decode_error, unsupported_error, Result};
use symphonia_core::formats::Packet;
use symphonia_core::support_codec;

use crate::celt_decoder::CeltDecoder;
use crate::silk_decoder::SilkDecoder;
use crate::packet::{OpusMode, OpusPacket};

/// Opus decoder implementing Symphonia's Decoder trait
pub struct OpusDecoder {
    /// Codec parameters
    params: CodecParameters,
    /// Number of channels
    channels: usize,
    /// Sample rate (always 48000 Hz for Opus)
    sample_rate: u32,
    /// Pre-skip (samples to discard at start)
    pre_skip: u32,
    /// CELT decoder for music/fullband content
    celt_decoder: CeltDecoder,
    /// SILK decoder for speech content
    silk_decoder: Option<SilkDecoder>,
    /// Output audio buffer
    output: AudioBuffer<f32>,
    /// Total samples decoded (for pre-skip handling)
    samples_decoded: u64,
}

impl OpusDecoder {
    /// Parse Opus identification header from extra data
    ///
    /// The OpusHead packet contains:
    /// - Magic signature "OpusHead" (8 bytes)
    /// - Version (1 byte)
    /// - Channel count (1 byte)
    /// - Pre-skip (2 bytes, little-endian)
    /// - Sample rate (4 bytes, little-endian) - informational only
    /// - Output gain (2 bytes, little-endian)
    /// - Channel mapping (1 byte)
    fn parse_opus_header(extra_data: &[u8]) -> Result<(u8, u32)> {
        if extra_data.len() < 19 {
            return decode_error("opus: identification header too short");
        }

        // Check magic signature
        if &extra_data[0..8] != b"OpusHead" {
            return decode_error("opus: invalid identification header");
        }

        // Extract channel count
        let channels = extra_data[9];
        if channels == 0 {
            return decode_error("opus: invalid channel count");
        }

        // Extract pre-skip (little-endian)
        let pre_skip = u16::from_le_bytes([extra_data[10], extra_data[11]]) as u32;

        Ok((channels, pre_skip))
    }
}

impl Decoder for OpusDecoder {
    fn try_new(params: &CodecParameters, _options: &DecoderOptions) -> Result<Self> {
        // Verify codec type
        if params.codec != CODEC_TYPE_OPUS {
            return unsupported_error("opus: invalid codec type");
        }

        // Get extra data (OpusHead packet)
        let extra_data = match params.extra_data.as_ref() {
            Some(buf) => buf,
            None => return unsupported_error("opus: missing identification header"),
        };

        // Parse Opus header
        let (channel_count, pre_skip) = Self::parse_opus_header(extra_data)?;

        // Opus always outputs at 48000 Hz
        let sample_rate = 48000;
        let channels = channel_count as usize;

        // Create CELT decoder (default to 960-sample frames = 20ms at 48kHz)
        // Frame size will be adjusted based on actual packet TOC byte
        let celt_decoder = CeltDecoder::new(sample_rate, channels, 960);

        // Create output buffer (maximum frame size is 2880 samples = 60ms)
        let spec = SignalSpec::new(sample_rate, params.channels.unwrap_or_default());
        let output = AudioBuffer::new(2880, spec);

        // Create SILK decoder (initially None, will be created on first SILK packet)
        let silk_decoder = None;

        Ok(Self {
            params: params.clone(),
            channels,
            sample_rate,
            pre_skip,
            celt_decoder,
            silk_decoder,
            output,
            samples_decoded: 0,
        })
    }

    fn supported_codecs() -> &'static [CodecDescriptor] {
        &[support_codec!(CODEC_TYPE_OPUS, "opus", "Opus")]
    }

    fn reset(&mut self) {
        self.celt_decoder.reset();
        if let Some(ref mut silk) = self.silk_decoder {
            silk.reset();
        }
        self.output.clear();
        self.samples_decoded = 0;
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    fn decode(&mut self, packet: &Packet) -> Result<AudioBufferRef<'_>> {
        // Clear output buffer
        self.output.clear();

        // Parse Opus packet to extract frames
        let opus_packet = match OpusPacket::parse(packet.buf()) {
            Ok(pkt) => pkt,
            Err(_) => return decode_error("opus: failed to parse packet"),
        };

        // For now, only support CELT frames
        // SILK and Hybrid support will be added later
        match opus_packet.mode {
            OpusMode::CeltOnly => {
                // Determine frame size from packet
                let frame_size = opus_packet.frame_size;

                // Recreate CELT decoder if frame size changed
                if frame_size != self.celt_decoder.frame_size {
                    self.celt_decoder = CeltDecoder::new(self.sample_rate, self.channels, frame_size);
                }

                // Decode each frame in the packet
                let total_samples = opus_packet.frame_count * frame_size;

                // Ensure output buffer has enough capacity
                if self.output.capacity() < total_samples {
                    let spec = SignalSpec::new(self.sample_rate, self.params.channels.unwrap_or_default());
                    self.output = AudioBuffer::new(total_samples as u64, spec);
                }

                // Render all frames
                self.output.render_reserved(Some(total_samples));

                // Get mutable slice for decoding
                let mut output_offset = 0;
                for frame in &opus_packet.frames {
                    // Create temporary buffer for this frame
                    let mut temp_output = vec![0.0f32; frame_size * self.channels];

                    // Decode frame
                    let samples = match self.celt_decoder.decode(frame, &mut temp_output) {
                        Ok(s) => s,
                        Err(_) => return decode_error("opus: celt decode failed"),
                    };

                    // Interleave into output buffer
                    for ch in 0..self.channels {
                        let channel_buf = self.output.chan_mut(ch);
                        for i in 0..frame_size {
                            if output_offset + i < channel_buf.len() {
                                channel_buf[output_offset + i] = temp_output[i * self.channels + ch];
                            }
                        }
                    }

                    output_offset += samples / self.channels;
                }

                // Handle pre-skip
                if self.samples_decoded < self.pre_skip as u64 {
                    let skip = (self.pre_skip as u64 - self.samples_decoded).min(total_samples as u64) as usize;

                    // Trim output to remove pre-skip samples
                    self.output.trim(skip, total_samples);

                    self.samples_decoded += skip as u64;
                }

                self.samples_decoded += self.output.frames() as u64;

                Ok(self.output.as_audio_buffer_ref())
            }
            OpusMode::SilkOnly => {
                // Create SILK decoder if not already initialized
                if self.silk_decoder.is_none() {
                    // SILK operates at internal sample rate (8/12/16/24 kHz)
                    // For now, use 16 kHz as default
                    self.silk_decoder = Some(SilkDecoder::new(16000));
                }

                let silk = self.silk_decoder.as_mut().unwrap();

                // Decode SILK frame
                let frame_size = opus_packet.frame_size;
                let mut silk_output = vec![0i16; frame_size * self.channels];

                // Use the first frame for simplicity
                if let Some(frame) = opus_packet.frames.first() {
                    use crate::entdec::RangeDecoder;

                    // Create range decoder for this frame
                    let mut ec = match RangeDecoder::new(frame) {
                        Ok(dec) => dec,
                        Err(_) => return decode_error("opus: failed to initialize range decoder"),
                    };

                    // Decode SILK frame
                    match silk.decode_frame(&mut ec, &mut silk_output, false, 0) {
                        Ok(_) => {},
                        Err(_) => return decode_error("opus: SILK decode failed"),
                    }

                    // SILK outputs 16-bit PCM at internal rate (e.g., 16 kHz)
                    // Need to resample to 48 kHz for Opus output
                    // For now, do simple linear interpolation
                    let upsample_factor = 3; // 16 kHz -> 48 kHz
                    let output_samples = frame_size * upsample_factor;

                    // Ensure output buffer has enough capacity
                    if self.output.capacity() < output_samples {
                        let spec = SignalSpec::new(self.sample_rate, self.params.channels.unwrap_or_default());
                        self.output = AudioBuffer::new(output_samples as u64, spec);
                    }

                    self.output.render_reserved(Some(output_samples));

                    // Upsample and convert to f32
                    for ch in 0..self.channels {
                        let channel_buf = self.output.chan_mut(ch);
                        for i in 0..frame_size {
                            let sample = silk_output[i * self.channels + ch] as f32 / 32768.0;
                            // Simple linear interpolation
                            for j in 0..upsample_factor {
                                let idx = i * upsample_factor + j;
                                if idx < output_samples {
                                    let next_sample = if i + 1 < frame_size {
                                        silk_output[(i + 1) * self.channels + ch] as f32 / 32768.0
                                    } else {
                                        sample
                                    };
                                    let t = j as f32 / upsample_factor as f32;
                                    channel_buf[idx] = sample * (1.0 - t) + next_sample * t;
                                }
                            }
                        }
                    }

                    self.samples_decoded += self.output.frames() as u64;
                    Ok(self.output.as_audio_buffer_ref())
                } else {
                    decode_error("opus: no SILK frames in packet")
                }
            }
            OpusMode::Hybrid => {
                unsupported_error("opus: Hybrid mode not yet implemented")
            }
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> AudioBufferRef<'_> {
        self.output.as_audio_buffer_ref()
    }
}
