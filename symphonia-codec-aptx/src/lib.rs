// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The decoder mirrors the fixed-point reference, and its index based loops keep the order of the
// arithmetic obvious.
#![allow(clippy::needless_range_loop)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

//! A pure Rust aptX and aptX HD decoder.
//!
//! aptX is a proprietary Bluetooth audio codec (Qualcomm, originally CSR/APT) with a fixed
//! compression ratio: a stereo block of 4 samples per channel is 4 bytes in aptX (16 bits per
//! channel) and 6 bytes in aptX HD (24 bits per channel). Each channel is split in 4 sub-bands by
//! a QMF tree, and each sub-band is coded with backward-adaptive ADPCM.
//!
//! The decoder is a port of FFmpeg's (`libavcodec/aptxdec.c`, `aptx.c`, `aptx.h`; LGPL-2.1+) and
//! reproduces its output bit for bit: planar 32-bit samples holding 24-bit values (shifted left
//! by 8). The streams are always stereo. The stream has no header and no sync word; the
//! synchronization is carried in the parity of the codewords, so a stream has to be decoded from
//! its first block, and a block whose parity is wrong is reported as an error.
//!
//! A packet is any number of whole blocks (4 bytes for aptX, 6 for aptX HD); bytes after the last
//! whole block are ignored.

mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, Channels,
    GenericAudioBufferRef, Position,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::FinalizeResult;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_APTX, CODEC_ID_APTX_HD};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use tables::{
    HD_TABLES, QMF_INNER_COEFFS, QMF_OUTER_COEFFS, QUANTIZATION_FACTORS, SubbandTables, TABLES,
};

/// The size in bytes of a block of 4 stereo samples of an aptX stream.
pub const APTX_BLOCK_SIZE: usize = 4;
/// The size in bytes of a block of 4 stereo samples of an aptX HD stream.
pub const APTX_HD_BLOCK_SIZE: usize = 6;
/// The sampling rate that FFmpeg assumes for raw streams, which carry no rate.
pub const DEFAULT_SAMPLE_RATE: u32 = 48000;

/// The number of samples per channel of a block.
const BLOCK_SAMPLES: usize = 4;
/// The number of blocks of a packet of FFmpeg's raw demuxer, the size the buffer is made for.
const NOMINAL_BLOCKS: usize = 256;
const NB_CHANNELS: usize = 2;
const NB_SUBBANDS: usize = 4;
const FILTER_TAPS: usize = 16;

/// `rshift32` of the reference: shift right with rounding, to even on ties.
fn rshift32(value: i32, shift: u32) -> i32 {
    let rounding = 1i32 << (shift - 1);
    let mask = (1i32 << (shift + 1)) - 1;
    (value.wrapping_add(rounding) >> shift) - i32::from((value & mask) == rounding)
}

/// `rshift64` of the reference.
fn rshift64(value: i64, shift: u32) -> i64 {
    let rounding = 1i64 << (shift - 1);
    let mask = (1i64 << (shift + 1)) - 1;
    (value.wrapping_add(rounding) >> shift) - i64::from((value & mask) == rounding)
}

/// `av_clip_intp2(a, 23)`: clip to a signed 24-bit value.
fn clip24(a: i32) -> i32 {
    a.clamp(-(1 << 23), (1 << 23) - 1)
}

/// `rshift64_clip24`. The reference passes the 64-bit result to an `int` parameter, so it is
/// truncated before clipping.
fn rshift64_clip24(value: i64, shift: u32) -> i32 {
    clip24(rshift64(value, shift) as i32)
}

/// `av_clip`.
fn clip(a: i32, min: i32, max: i32) -> i32 {
    if a < min {
        min
    }
    else if a > max {
        max
    }
    else {
        a
    }
}

/// `FFDIFFSIGN`.
fn diff_sign(x: i32, y: i32) -> i32 {
    i32::from(x > y) - i32::from(x < y)
}

/// The history of the samples of a filter (`FilterSignal`), a ring buffer stored twice so that
/// the taps are always contiguous.
#[derive(Clone, Default)]
struct FilterSignal {
    pos: usize,
    buffer: [i32; 2 * FILTER_TAPS],
}

impl FilterSignal {
    fn push(&mut self, sample: i32) {
        self.buffer[self.pos] = sample;
        self.buffer[self.pos + FILTER_TAPS] = sample;
        self.pos = (self.pos + 1) & (FILTER_TAPS - 1);
    }

    fn convolution(&self, coeffs: &[i32; FILTER_TAPS], shift: u32) -> i32 {
        let sig = &self.buffer[self.pos..self.pos + FILTER_TAPS];
        let mut e = 0i64;

        for i in 0..FILTER_TAPS {
            e = e.wrapping_add(i64::from(sig[i]) * i64::from(coeffs[i]));
        }

        rshift64_clip24(e, shift)
    }
}

/// The synthesis QMF tree of a channel (`QMFAnalysis`).
#[derive(Clone, Default)]
struct Qmf {
    outer: [FilterSignal; 2],
    inner: [[FilterSignal; 2]; 2],
}

/// `aptx_qmf_polyphase_synthesis`.
fn polyphase_synthesis(
    signal: &mut [FilterSignal; 2],
    coeffs: &[[i32; FILTER_TAPS]; 2],
    shift: u32,
    low: i32,
    high: i32,
) -> [i32; 2] {
    let subbands = [low.wrapping_add(high), low.wrapping_sub(high)];
    let mut samples = [0; 2];

    for i in 0..2 {
        signal[i].push(subbands[1 - i]);
        samples[i] = signal[i].convolution(&coeffs[i], shift);
    }

    samples
}

impl Qmf {
    /// `aptx_qmf_tree_synthesis`: join 4 sub-band samples into 4 samples.
    fn tree_synthesis(&mut self, subband_samples: &[i32; 4]) -> [i32; 4] {
        let mut intermediate = [0; 4];

        // Join 4 subbands into 2 intermediate subbands upsampled to 2 samples.
        for i in 0..2 {
            let s = polyphase_synthesis(
                &mut self.inner[i],
                &QMF_INNER_COEFFS,
                22,
                subband_samples[2 * i],
                subband_samples[2 * i + 1],
            );
            intermediate[2 * i] = s[0];
            intermediate[2 * i + 1] = s[1];
        }

        // Join 2 samples from intermediate subbands upsampled to 4 samples.
        let mut samples = [0; 4];

        for i in 0..2 {
            let s = polyphase_synthesis(
                &mut self.outer,
                &QMF_OUTER_COEFFS,
                21,
                intermediate[i],
                intermediate[2 + i],
            );
            samples[2 * i] = s[0];
            samples[2 * i + 1] = s[1];
        }

        samples
    }
}

/// The inverse quantizer of a sub-band (`InvertQuantize`).
#[derive(Clone, Default)]
struct InvertQuantize {
    quantization_factor: i32,
    factor_select: i32,
    reconstructed_difference: i32,
}

impl InvertQuantize {
    /// `aptx_invert_quantization`.
    fn invert(&mut self, quantized_sample: i32, dither: i32, tables: &SubbandTables) {
        let idx = ((quantized_sample ^ -i32::from(quantized_sample < 0)) + 1) as usize;
        let mut qr = tables.quantize_intervals.get(idx).copied().unwrap_or(0) / 2;
        if quantized_sample < 0 {
            qr = -qr;
        }

        let dither_factor = tables.invert_quantize_dither_factors.get(idx).copied().unwrap_or(0);
        let qr = rshift64_clip24(
            (i64::from(qr) << 32).wrapping_add(i64::from(dither) * i64::from(dither_factor)),
            32,
        );
        self.reconstructed_difference =
            ((i64::from(self.quantization_factor) * i64::from(qr)) >> 19) as i32;

        // Update the factor selection.
        let offset = tables.quantize_factor_select_offset.get(idx).copied().unwrap_or(0);
        let factor_select = 32620i32.wrapping_mul(self.factor_select);
        let factor_select = rshift32(factor_select.wrapping_add(i32::from(offset) * (1 << 15)), 15);
        self.factor_select = clip(factor_select, 0, tables.factor_max);

        // Update the quantization factor.
        let idx = ((self.factor_select & 0xff) >> 3) as usize;
        let shift = (tables.factor_max - self.factor_select) >> 8;
        self.quantization_factor = (QUANTIZATION_FACTORS[idx] << 11) >> shift;
    }
}

/// The predictor of a sub-band (`Prediction`).
#[derive(Clone)]
struct Prediction {
    prev_sign: [i32; 2],
    s_weight: [i32; 2],
    d_weight: [i32; 24],
    pos: usize,
    reconstructed_differences: [i32; 48],
    previous_reconstructed_sample: i32,
    predicted_difference: i32,
    predicted_sample: i32,
}

impl Default for Prediction {
    fn default() -> Self {
        Prediction {
            prev_sign: [1, 1],
            s_weight: [0; 2],
            d_weight: [0; 24],
            pos: 0,
            reconstructed_differences: [0; 48],
            previous_reconstructed_sample: 0,
            predicted_difference: 0,
            predicted_sample: 0,
        }
    }
}

impl Prediction {
    /// `aptx_prediction_filtering`.
    fn filter(&mut self, reconstructed_difference: i32, order: usize) {
        let reconstructed_sample =
            clip24(reconstructed_difference.wrapping_add(self.predicted_sample));
        let predictor = clip24(
            ((i64::from(self.s_weight[0]) * i64::from(self.previous_reconstructed_sample)
                + i64::from(self.s_weight[1]) * i64::from(reconstructed_sample))
                >> 22) as i32,
        );
        self.previous_reconstructed_sample = reconstructed_sample;

        // `aptx_reconstructed_differences_update`: the history is two halves of `order` entries,
        // and `base` is the index of the newest difference in the second half.
        let p = self.pos;
        self.reconstructed_differences[p] = self.reconstructed_differences[order + p];
        self.pos = (p + 1) % order;
        let base = order + self.pos;
        self.reconstructed_differences[base] = reconstructed_difference;

        let srd0 = diff_sign(reconstructed_difference, 0) * (1 << 23);
        let mut predicted_difference = 0i64;

        for i in 0..order {
            let srd = (self.reconstructed_differences[base - i - 1] >> 31) | 1;
            let w = self.d_weight[i];
            self.d_weight[i] = w.wrapping_sub(rshift32(w.wrapping_sub(srd.wrapping_mul(srd0)), 8));
            predicted_difference = predicted_difference.wrapping_add(
                i64::from(self.reconstructed_differences[base - i]) * i64::from(self.d_weight[i]),
            );
        }

        self.predicted_difference = clip24((predicted_difference >> 22) as i32);
        self.predicted_sample = clip24(predictor.wrapping_add(self.predicted_difference));
    }
}

/// A sub-band of a channel.
#[derive(Clone, Default)]
struct Subband {
    quantized_sample: i32,
    invert_quantize: InvertQuantize,
    prediction: Prediction,
}

impl Subband {
    /// `aptx_process_subband`.
    fn process(&mut self, dither: i32, tables: &SubbandTables) {
        let p = &mut self.prediction;
        self.invert_quantize.invert(self.quantized_sample, dither, tables);
        let rd = self.invert_quantize.reconstructed_difference;

        let sign = diff_sign(rd, p.predicted_difference.wrapping_neg());
        let same_sign = [sign * p.prev_sign[0], sign * p.prev_sign[1]];
        p.prev_sign[0] = p.prev_sign[1];
        p.prev_sign[1] = sign | 1;

        let range = 0x10_0000;
        let sw1 = rshift32(same_sign[1].wrapping_neg().wrapping_mul(p.s_weight[1]), 1);
        let sw1 = (clip(sw1, -range, range) & !0xf) * 16;

        let range = 0x30_0000;
        let weight0 = 254i32
            .wrapping_mul(p.s_weight[0])
            .wrapping_add(0x80_0000i32.wrapping_mul(same_sign[0]))
            .wrapping_add(sw1);
        p.s_weight[0] = clip(rshift32(weight0, 8), -range, range);

        let range = 0x3c_0000 - p.s_weight[0];
        let weight1 = 255i32
            .wrapping_mul(p.s_weight[1])
            .wrapping_add(0xc0_0000i32.wrapping_mul(same_sign[1]));
        p.s_weight[1] = clip(rshift32(weight1, 8), -range, range);

        p.filter(rd, tables.prediction_order);
    }
}

/// The state of a channel (`Channel`).
#[derive(Clone, Default)]
struct Channel {
    codeword_history: i32,
    dither_parity: i32,
    dither: [i32; NB_SUBBANDS],
    qmf: Qmf,
    subbands: [Subband; NB_SUBBANDS],
}

impl Channel {
    /// `aptx_quantized_parity`.
    fn quantized_parity(&self) -> i32 {
        let mut parity = self.dither_parity;
        for sb in &self.subbands {
            parity ^= sb.quantized_sample;
        }
        parity & 1
    }

    /// `ff_aptx_generate_dither`.
    fn generate_dither(&mut self) {
        // `aptx_update_codeword_history`.
        let cw = (self.subbands[0].quantized_sample & 3)
            + ((self.subbands[1].quantized_sample & 2) << 1)
            + ((self.subbands[2].quantized_sample & 1) << 3);
        self.codeword_history =
            (cw << 8).wrapping_add(((self.codeword_history as u32) << 4) as i32);

        let m = 5184443i64 * i64::from(self.codeword_history >> 7);
        let d = (m * 4).wrapping_add(m >> 22) as i32;

        for sb in 0..NB_SUBBANDS {
            self.dither[sb] = ((d as u32) << (23 - 5 * sb)) as i32;
        }
        self.dither_parity = (d >> 25) & 1;
    }

    /// `aptx_unpack_codeword`, and `aptxhd_unpack_codeword`.
    fn unpack_codeword(&mut self, codeword: u32, hd: bool) {
        let widths: [u32; 4] = if hd { [9, 6, 4, 5] } else { [7, 4, 2, 3] };
        let mut shift = 0;

        for (sb, &width) in widths.iter().enumerate() {
            self.subbands[sb].quantized_sample =
                symphonia_core::util::bits::sign_extend_leq32_to_i32(codeword >> shift, width);
            shift += width;
        }

        // The parity bit of the last sub-band.
        let q = self.subbands[3].quantized_sample;
        self.subbands[3].quantized_sample = (q & !1) | self.quantized_parity();
    }
}

/// An aptX or aptX HD decoder.
pub struct AptxDecoder {
    params: AudioCodecParameters,
    hd: bool,
    sync_idx: i32,
    channels: [Channel; NB_CHANNELS],
    buf: AudioBuffer<i32>,
}

impl AptxDecoder {
    pub fn try_new(params: &AudioCodecParameters, _opts: &AudioDecoderOptions) -> Result<Self> {
        let hd = match params.codec {
            CODEC_ID_APTX => false,
            CODEC_ID_APTX_HD => true,
            _ => return unsupported_error("aptx: invalid codec"),
        };

        // The streams are stereo.
        if params.channels.as_ref().is_some_and(|c| c.count() != NB_CHANNELS) {
            return unsupported_error("aptx: only stereo streams are supported");
        }

        let rate = params.sample_rate.filter(|&r| r != 0).unwrap_or(DEFAULT_SAMPLE_RATE);
        let channels = Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT);

        let spec = AudioSpec::new(rate, channels.clone());
        let buf = AudioBuffer::new(spec, NOMINAL_BLOCKS * BLOCK_SAMPLES);

        let mut params = params.clone();
        params.with_sample_rate(rate);
        params.with_channels(channels);

        Ok(AptxDecoder {
            params,
            hd,
            sync_idx: 0,
            channels: [Channel::default(), Channel::default()],
            buf,
        })
    }

    fn block_size(&self) -> usize {
        if self.hd { APTX_HD_BLOCK_SIZE } else { APTX_BLOCK_SIZE }
    }

    /// `aptx_decode_samples`: decode a block. Returns false if the synchronization is lost.
    fn decode_block(&mut self, input: &[u8]) -> Option<[[i32; 4]; NB_CHANNELS]> {
        let tables = if self.hd { &HD_TABLES } else { &TABLES };

        for (ch, channel) in self.channels.iter_mut().enumerate() {
            channel.generate_dither();

            let codeword = if self.hd {
                let b = input.get(3 * ch..3 * ch + 3)?;
                u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2])
            }
            else {
                let b = input.get(2 * ch..2 * ch + 2)?;
                u32::from(b[0]) << 8 | u32::from(b[1])
            };

            channel.unpack_codeword(codeword, self.hd);

            // `ff_aptx_invert_quantize_and_prediction`.
            for sb in 0..NB_SUBBANDS {
                let dither = channel.dither[sb];
                channel.subbands[sb].process(dither, &tables[sb]);
            }
        }

        // `aptx_check_parity`.
        let parity = self.channels[0].quantized_parity() ^ self.channels[1].quantized_parity();
        let eighth = i32::from(self.sync_idx == 7);
        self.sync_idx = (self.sync_idx + 1) & 7;
        let sync_lost = parity ^ eighth != 0;

        let mut samples = [[0; 4]; NB_CHANNELS];

        // `aptx_decode_channel`.
        for (ch, channel) in self.channels.iter_mut().enumerate() {
            let mut subband_samples = [0; 4];
            for sb in 0..NB_SUBBANDS {
                subband_samples[sb] = channel.subbands[sb].prediction.previous_reconstructed_sample;
            }
            samples[ch] = channel.qmf.tree_synthesis(&subband_samples);
        }

        if sync_lost { None } else { Some(samples) }
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();

        let block_size = self.block_size();

        if packet.data.len() < block_size {
            return decode_error("aptx: packet is too small");
        }

        let blocks = packet.data.len() / block_size;
        let num_samples = blocks * BLOCK_SAMPLES;

        if num_samples > self.buf.capacity() {
            let spec = self.buf.spec().clone();
            self.buf = AudioBuffer::new(spec, num_samples);
        }

        self.buf.render_uninit(Some(num_samples));

        for (b, input) in packet.data.chunks_exact(block_size).enumerate() {
            let Some(samples) = self.decode_block(input)
            else {
                return decode_error("aptx: synchronization error");
            };

            for ch in 0..NB_CHANNELS {
                let Some(plane) = self.buf.plane_mut(ch)
                else {
                    return decode_error("aptx: missing plane");
                };

                for (out, &s) in
                    plane[b * BLOCK_SAMPLES..(b + 1) * BLOCK_SAMPLES].iter_mut().zip(&samples[ch])
                {
                    // 24-bit samples, in the most significant bits.
                    *out = s.wrapping_mul(256);
                }
            }
        }

        Ok(())
    }
}

impl AudioDecoder for AptxDecoder {
    fn reset(&mut self) {
        self.sync_idx = 0;
        self.channels = [Channel::default(), Channel::default()];
        self.buf.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        let codecs = Self::supported_codecs();
        let idx = usize::from(self.hd);
        &codecs.get(idx).or(codecs.first()).expect("codecs registered").info
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

impl RegisterableAudioDecoder for AptxDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(AptxDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[
            support_audio_codec!(CODEC_ID_APTX, "aptx", "aptX"),
            support_audio_codec!(CODEC_ID_APTX_HD, "aptx_hd", "aptX HD"),
        ]
    }
}
