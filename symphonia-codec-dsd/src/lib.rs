// Symphonia DSD Codec
// Copyright (c) 2026 M0Rf30
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]

// Public modules for benchmarking
pub mod bitstream;
pub mod cic;
pub mod decimator;
pub mod dst;
pub mod fir;

use decimator::{DecimationConfig, DsdDecimator};
use dst::{DST_MAX_CHANNELS, DstDecoder, dst_frame_crc};

use log::debug;
use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBuffer,
    GenericAudioBufferRef,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::{
    AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions, BitOrder,
    ChannelDataLayout, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

/// DSD codec ID
pub const CODEC_ID_DSD: AudioCodecId = AudioCodecId::new(FourCc::new(*b"DSD\0"));

/// DST (Direct Stream Transfer) codec ID: DSD compressed losslessly, as in compressed DSDIFF.
pub const CODEC_ID_DST: AudioCodecId = AudioCodecId::new(FourCc::new(*b"DST\0"));

/// The number of bytes before the DST frame in a DST packet: a flags byte and a 32-bit CRC.
const DST_PACKET_HEADER_LEN: usize = 5;

/// Bit 0 of the flags of a DST packet: the packet has the CRC of the decoded frame.
const DST_PACKET_HAS_CRC: u8 = 1;

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
///    A *frame* of the decoded U8 buffer is one byte per channel, i.e. 8 consecutive 1-bit DSD
///    samples of each channel. Therefore the `AudioSpec` of the decoded buffers has a rate of
///    `dsd_rate / 8` (e.g. 352800 Hz for DSD64), so that `frames() / spec().rate()` is the
///    duration of the buffer in seconds, and `frames()` is `packet.dur / 8`.
///
///    This differs from the *timeline* of the DSD format readers (`symphonia-format-dsd`), and the
///    `AudioCodecParameters` of the track, where timestamps, durations, `num_frames`, and
///    `sample_rate` count 1-bit DSD samples per channel (e.g. 2822400 Hz for DSD64). The codec
///    parameters returned by [`AudioDecoder::codec_params`] are not altered in pass-through mode:
///    `sample_rate` remains the DSD rate (needed, e.g., to encode DSD over PCM).
///
/// 2. **PCM mode**: Converts DSD to PCM F32 samples using linear phase decimation filters
///    (a CIC filter followed by half-band FIR filters, see the [`decimator`] module). Enable by
///    setting `extra_data` in the codec parameters to the PCM output rate as a little-endian
///    `u32`. The PCM rate must be the DSD rate divided by an even number of at least 8, e.g.
///    DSD64 (2.8224 MHz) to 44.1, 88.2, 176.4 or 352.8 kHz, but not to 48 kHz, and DSD at 48 kHz
///    times 64 (3.072 MHz) to 48 kHz and its multiples.
///
///    The output is flat to 0.4535 times the PCM rate (20 kHz at 44.1 kHz) within 0.001 dB,
///    and aliasing into that band is below -100 dB. The delay of the filters (about 1 ms) is
///    compensated by dropping the first frames of the stream (unless the decoder is created
///    with the option `gapless` disabled), so that frame `k` of the output is the audio at DSD
///    sample `k * R` for a decimation by `R`. The last frames are held back by that delay:
///    [`flush`](DsdDecoder::flush) outputs them at the end of the stream.
///
/// # DST
/// Streams of the codec [`CODEC_ID_DST`], DSD that is compressed by Direct Stream Transfer, are
/// first decompressed to DSD with [`DstDecoder`], and then output like DSD. A packet consists of
/// the following, as produced by the DSDIFF reader of `symphonia-format-dsd`:
///
/// | bytes | |
/// |-------|-|
/// | 1     | flags: bit 0 is set if the CRC of the decoded frame follows |
/// | 4     | CRC of the decoded frame, big-endian, 0 if not present |
/// | rest  | the DST frame |
///
/// `max_frames_per_packet` of the codec parameters is the number of DSD samples of a frame. The
/// frame data is interleaved by byte, with the most significant bit first. With
/// [`AudioDecoderOptions::verify`] each frame is also verified: against the CRC, if present, and
/// the arithmetic code of the frame for the end that the specification demands. A frame that does
/// not verify is an error.
pub struct DsdDecoder {
    params: AudioCodecParameters,
    channels: usize,
    layout: ChannelDataLayout,
    // Native DSD output buffer (pass-through mode)
    dsd_buf: GenericAudioBuffer,
    // The state of the DST decompression, if the stream is DST.
    dst: Option<DstStage>,
    // The state of the conversion to PCM, if the output is PCM.
    pcm: Option<PcmStage>,
}

/// DST decompression of the packets to DSD.
struct DstStage {
    decoder: DstDecoder,
    /// The DSD of the last decoded frame, interleaved, most significant bit first.
    frame: Vec<u8>,
    verify: bool,
    /// `Some(false)` if a frame did not verify.
    verify_ok: Option<bool>,
}

impl DstStage {
    fn decode(&mut self, packet: &[u8]) -> Result<()> {
        if packet.len() <= DST_PACKET_HEADER_LEN {
            return decode_error("dst: packet too short");
        }

        let result =
            self.decoder.decode(&packet[DST_PACKET_HEADER_LEN..], &mut self.frame, self.verify);

        // Verify against the CRC of the frame.
        let result = match result {
            Ok(()) if self.verify && packet[0] & DST_PACKET_HAS_CRC != 0 => {
                let crc = u32::from_be_bytes([packet[1], packet[2], packet[3], packet[4]]);
                if crc == dst_frame_crc(&self.frame) {
                    Ok(())
                }
                else {
                    decode_error("dst: frame CRC mismatch")
                }
            }
            other => other,
        };

        if self.verify {
            self.verify_ok = Some(self.verify_ok != Some(false) && result.is_ok());
        }
        result
    }
}

/// Conversion of DSD to PCM.
struct PcmStage {
    decimator: DsdDecimator,
    buf: GenericAudioBuffer,
    /// The bytes of a channel of a packet.
    scratch: Vec<u8>,
}

/// Make the PCM buffer hold `frames` frames to overwrite, and return it.
fn prepare_pcm(buf: &mut GenericAudioBuffer, frames: usize) -> &mut AudioBuffer<f32> {
    let GenericAudioBuffer::F32(buf) = buf
    else {
        unreachable!("the PCM buffer has the sample format F32")
    };
    buf.clear();
    buf.grow_capacity(frames);
    buf.render_uninit(Some(frames));
    buf
}

impl PcmStage {
    fn decode(
        &mut self,
        data: &[u8],
        channels: usize,
        layout: ChannelDataLayout,
    ) -> Result<GenericAudioBufferRef<'_>> {
        let spc = data.len() / channels;

        if spc == 0 {
            return decode_error("dsd: invalid packet");
        }

        let frames = self.decimator.output_len(spc);
        let buf = prepare_pcm(&mut self.buf, frames);

        for ch in 0..channels {
            self.scratch.clear();
            self.scratch.resize(spc, 0);
            copy_channel(data, channels, spc, ch, layout, &mut self.scratch);

            let Some(plane) = buf.plane_mut(ch)
            else {
                return decode_error("dsd: missing audio plane");
            };
            if self.decimator.process_channel(ch, &self.scratch, plane)? != frames {
                return decode_error("dsd: decimator produced an unexpected length");
            }
        }

        Ok(self.buf.as_generic_audio_buffer_ref())
    }

    fn flush(&mut self, channels: usize) -> Result<Option<GenericAudioBufferRef<'_>>> {
        let frames = self.decimator.pending_frames();
        if frames == 0 {
            return Ok(None);
        }

        let buf = prepare_pcm(&mut self.buf, frames);
        for ch in 0..channels {
            let Some(plane) = buf.plane_mut(ch)
            else {
                return decode_error("dsd: missing audio plane");
            };
            self.decimator.flush_channel(ch, plane)?;
        }
        Ok(Some(self.buf.as_generic_audio_buffer_ref()))
    }
}

impl AudioDecoder for DsdDecoder {
    fn reset(&mut self) {
        self.dsd_buf.clear();
        if let Some(pcm) = self.pcm.as_mut() {
            pcm.buf.clear();
            pcm.decimator.reset();
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
        // The DSD of the packet, which is the data of the packet itself, or the decompressed
        // frame of a DST packet.
        let data = match self.dst.as_mut() {
            Some(dst) => {
                dst.decode(packet.data)?;
                dst.frame.as_slice()
            }
            None => packet.data,
        };

        match self.pcm.as_mut() {
            Some(pcm) => pcm.decode(data, self.channels, self.layout),
            None => passthrough(&mut self.dsd_buf, data, self.channels, self.layout),
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult { verify_ok: self.dst.as_ref().and_then(|dst| dst.verify_ok) }
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        match self.pcm.as_ref() {
            Some(pcm) => pcm.buf.as_generic_audio_buffer_ref(),
            None => self.dsd_buf.as_generic_audio_buffer_ref(),
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
        &[
            support_audio_codec!(CODEC_ID_DSD, "dsd", "Direct Stream Digital"),
            support_audio_codec!(CODEC_ID_DST, "dst", "Direct Stream Transfer"),
        ]
    }
}

/// The rate of the frames of the pass-through output buffers: one frame is one byte per channel,
/// which holds 8 DSD samples.
fn dsd_byte_rate(dsd_rate: u32) -> u32 {
    (dsd_rate / 8).max(1)
}

/// Copy channel `ch` of a packet holding `channels` channels of `spc` bytes each into `dst`.
fn copy_channel(
    data: &[u8],
    channels: usize,
    spc: usize,
    ch: usize,
    layout: ChannelDataLayout,
    dst: &mut [u8],
) {
    match layout {
        ChannelDataLayout::Interleaved => {
            for (d, s) in dst.iter_mut().zip(data[ch..].iter().step_by(channels)) {
                *d = *s;
            }
        }
        ChannelDataLayout::Planar => {
            for (d, s) in dst.iter_mut().zip(&data[ch * spc..(ch + 1) * spc]) {
                *d = *s;
            }
        }
    }
}

/// Output the DSD `data` unchanged, as U8 samples of `buf`.
fn passthrough<'a>(
    buf: &'a mut GenericAudioBuffer,
    data: &[u8],
    channels: usize,
    layout: ChannelDataLayout,
) -> Result<GenericAudioBufferRef<'a>> {
    let spc = data.len() / channels;

    if spc == 0 {
        return decode_error("dsd: invalid packet");
    }

    buf.clear();

    match buf {
        GenericAudioBuffer::U8(buf) => {
            buf.grow_capacity(spc);
            buf.render_uninit(Some(spc));
            for ch in 0..channels {
                let Some(plane) = buf.plane_mut(ch)
                else {
                    return decode_error("dsd: missing audio plane");
                };
                copy_channel(data, channels, spc, ch, layout, plane);
            }
            Ok(buf.as_generic_audio_buffer_ref())
        }
        _ => unreachable!("the DSD buffer has the sample format U8"),
    }
}

impl DsdDecoder {
    pub fn try_new(params: &AudioCodecParameters, options: &AudioDecoderOptions) -> Result<Self> {
        let is_dst = params.codec == CODEC_ID_DST;

        // Verify this is a DSD or DST codec
        if params.codec != CODEC_ID_DSD && !is_dst {
            return unsupported_error("dsd: codec type is not DSD or DST");
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

        if channels_count == 0 {
            return decode_error("dsd: no channels");
        }

        let mut params = params.clone();

        // The DSD of DST is always channel-interleaved and most significant bit first.
        if is_dst {
            params.bit_order = Some(BitOrder::MsbFirst);
            params.channel_data_layout = Some(ChannelDataLayout::Interleaved);
        }

        let bit_order = params.bit_order.unwrap_or(BitOrder::LsbFirst);
        let layout = params.channel_data_layout.unwrap_or(ChannelDataLayout::Interleaved);

        log::info!("DSD decoder: bit_order={:?}", bit_order);

        // The number of bytes of a channel in a packet.
        let max_bytes = match (is_dst, params.max_frames_per_packet) {
            (true, None) | (true, Some(0)) => return decode_error("dst: missing frame length"),
            (true, Some(samples)) if samples % 8 != 0 => {
                return decode_error("dst: frame length is not a multiple of 8 samples");
            }
            (_, Some(samples)) => samples.div_ceil(8) as usize,
            (false, None) => 4096,
        };

        let dst = if is_dst {
            if channels_count > DST_MAX_CHANNELS {
                return unsupported_error("dst: more than 6 channels");
            }
            let decoder = DstDecoder::new(channels_count, max_bytes)?;
            Some(DstStage {
                frame: vec![0; decoder.frame_bytes()],
                decoder,
                verify: options.verify,
                verify_ok: None,
            })
        }
        else {
            None
        };

        // Determine mode: check if PCM output rate is requested in extra_data
        // Format: first 4 bytes = u32 PCM output rate (little-endian)
        let mode = match params.extra_data.as_ref() {
            Some(extra) if extra.len() >= 4 => {
                let pcm_rate = u32::from_le_bytes([extra[0], extra[1], extra[2], extra[3]]);
                if pcm_rate > 0 {
                    DecoderMode::Pcm { output_rate: pcm_rate }
                }
                else {
                    DecoderMode::PassThrough
                }
            }
            _ => DecoderMode::PassThrough,
        };

        let pcm = match mode {
            DecoderMode::PassThrough => {
                debug!(
                    "DSD decoder (pass-through): rate={}, channels={}, bytes={}",
                    input_sample_rate, channels_count, max_bytes
                );
                None
            }
            DecoderMode::Pcm { output_rate } => {
                let config = DecimationConfig::new(input_sample_rate, output_rate)?;
                log::info!(
                    "DSD decimation: {}x total (CIC={}x{} stages, {} FIR stages, {} frames delay)",
                    config.total_decimation,
                    config.cic_decimation,
                    config.cic_stages,
                    config.fir_stages,
                    config.delay_frames()
                );
                let decimator = DsdDecimator::with_delay_compensation(
                    config,
                    channels_count,
                    bit_order,
                    options.gapless,
                );

                // The buffer grows if a packet is larger than the maximum.
                let capacity = (max_bytes * 8).div_ceil(config.total_decimation) + 2;
                let pcm_spec = AudioSpec::new(output_rate, channels.clone());
                let buf = GenericAudioBuffer::new(SampleFormat::F32, pcm_spec, capacity);

                // Update codec params for PCM output
                params.sample_rate = Some(output_rate);
                params.bits_per_sample = Some(32); // f32

                Some(PcmStage { decimator, buf, scratch: Vec::new() })
            }
        };

        // The DSD buffer is the output in pass-through mode. Its capacity is in bytes of a channel,
        // it grows if a packet is larger than the maximum.
        let dsd_spec = AudioSpec::new(dsd_byte_rate(input_sample_rate), channels);
        let dsd_buf = GenericAudioBuffer::new(SampleFormat::U8, dsd_spec, max_bytes);

        Ok(DsdDecoder { params, channels: channels_count, layout, dsd_buf, dst, pcm })
    }

    /// Output the PCM that the delay of the filters has held back at the end of the stream, if
    /// the output is PCM, so that the PCM has as many frames as the DSD has samples divided by
    /// the decimation ratio.
    ///
    /// Call this after the last packet. It returns `None` if there is nothing more, and always
    /// for DSD output. The decoder must be [reset](AudioDecoder::reset) before it decodes more
    /// packets.
    pub fn flush(&mut self) -> Result<Option<GenericAudioBufferRef<'_>>> {
        match self.pcm.as_mut() {
            Some(pcm) => pcm.flush(self.channels),
            None => Ok(None),
        }
    }
}
