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
use crate::resampler::PolyphaseResampler;
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
    /// Polyphase FIR resampler for SILK upsampling (high quality)
    silk_resampler: Option<PolyphaseResampler>,
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
        // Create SILK resampler (initially None, will be created when SILK decoder is initialized)
        let silk_resampler = None;

        Ok(Self {
            params: params.clone(),
            channels,
            sample_rate,
            pre_skip,
            celt_decoder,
            silk_decoder,
            silk_resampler,
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
        if let Some(ref mut resampler) = self.silk_resampler {
            resampler.reset();
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

                    // Decode frame with bandwidth information from packet
                    let samples = match self.celt_decoder.decode(frame, &mut temp_output, opus_packet.bandwidth) {
                        Ok(s) => s,
                        Err(_) => return decode_error("opus: celt decode failed"),
                    };

                    // Copy non-interleaved output to audio buffer
                    // CELT outputs in [channel0][channel1] format, each with frame_size samples
                    for ch in 0..self.channels {
                        let channel_buf = self.output.chan_mut(ch);
                        let channel_offset = ch * frame_size;
                        for i in 0..frame_size {
                            if output_offset + i < channel_buf.len() {
                                channel_buf[output_offset + i] = temp_output[channel_offset + i];
                            }
                        }
                    }

                    output_offset += samples / self.channels;
                }

                // Handle pre-skip
                let rendered_frames = self.output.frames();
                if self.samples_decoded < self.pre_skip as u64 {
                    let skip = (self.pre_skip as u64 - self.samples_decoded).min(rendered_frames as u64) as usize;

                    // Trim output to remove pre-skip samples
                    if skip < rendered_frames {
                        self.output.trim(skip, rendered_frames - skip);
                    } else {
                        // Skip entire frame
                        self.output.clear();
                    }

                    self.samples_decoded += skip as u64;
                }

                self.samples_decoded += self.output.frames() as u64;

                Ok(self.output.as_audio_buffer_ref())
            }
            OpusMode::SilkOnly => {
                // Determine SILK internal sample rate from bandwidth
                // NB: 8 kHz, MB: 12 kHz, WB: 16 kHz, SWB: 24 kHz
                let silk_rate = match opus_packet.bandwidth {
                    crate::packet::OpusBandwidth::Narrowband => 8000,
                    crate::packet::OpusBandwidth::Mediumband => 12000,
                    crate::packet::OpusBandwidth::Wideband => 16000,
                    crate::packet::OpusBandwidth::SuperWideband => 24000,
                    _ => 16000, // Default to wideband
                };

                // Create or update SILK decoder if needed
                if self.silk_decoder.is_none() {
                    self.silk_decoder = Some(SilkDecoder::new(silk_rate));
                    // Create polyphase FIR resampler for high-quality upsampling
                    self.silk_resampler = Some(PolyphaseResampler::new(silk_rate, 48000));
                }

                let silk = self.silk_decoder.as_mut().unwrap();
                let resampler = self.silk_resampler.as_mut().unwrap();

                // Calculate frame sizes
                // SILK frame size is at internal rate, need to compute output at 48 kHz
                let upsample_ratio = resampler.up_ratio();
                let silk_frame_size = opus_packet.frame_size / upsample_ratio;
                let output_samples = opus_packet.frame_size; // Frame size is already at 48 kHz

                // Allocate buffers for SILK output (at internal rate)
                let mut silk_output = vec![0i16; silk_frame_size * self.channels];
                // Allocate buffer for resampled output (per channel)
                let mut resampled = vec![0i16; output_samples];

                // Use the first frame for simplicity
                if let Some(frame) = opus_packet.frames.first() {
                    use crate::entdec::RangeDecoder;

                    // Create range decoder for this frame
                    let mut ec = match RangeDecoder::new(frame) {
                        Ok(dec) => dec,
                        Err(_) => return decode_error("opus: failed to initialize range decoder"),
                    };

                    // Decode SILK frame (outputs at internal sample rate)
                    match silk.decode_frame(&mut ec, &mut silk_output, false, 0) {
                        Ok(_) => {}
                        Err(_) => return decode_error("opus: SILK decode failed"),
                    }

                    // Ensure output buffer has enough capacity
                    if self.output.capacity() < output_samples {
                        let spec =
                            SignalSpec::new(self.sample_rate, self.params.channels.unwrap_or_default());
                        self.output = AudioBuffer::new(output_samples as u64, spec);
                    }

                    self.output.render_reserved(Some(output_samples));

                    // Upsample each channel using polyphase FIR resampler
                    for ch in 0..self.channels {
                        // Extract single channel from interleaved SILK output
                        let mut channel_input = vec![0i16; silk_frame_size];
                        for i in 0..silk_frame_size {
                            channel_input[i] = silk_output[i * self.channels + ch];
                        }

                        // Resample using high-quality polyphase FIR filter
                        // This achieves >90 dB SNR vs ~40 dB for linear interpolation
                        resampler.resample(&channel_input, &mut resampled);

                        // Convert to f32 and write to output buffer
                        let channel_buf = self.output.chan_mut(ch);
                        for (i, &sample) in resampled.iter().enumerate().take(output_samples) {
                            channel_buf[i] = sample as f32 / 32768.0;
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
