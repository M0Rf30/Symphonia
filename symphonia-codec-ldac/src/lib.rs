// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The decode follows the reference bit-exactly, and its index-based loops keep the order of the
// arithmetic obvious.
#![allow(clippy::needless_range_loop)]
// The tables are read-only and copied from the reference.
#![allow(clippy::large_const_arrays)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

//! A pure Rust LDAC (Sony Bluetooth A2DP) decoder.
//!
//! LDAC is a transform codec: each channel of a frame is windowed with a half-period sine taper
//! and transformed with an MDCT whose core is an unscaled DCT-IV. The coefficients are grouped
//! into quantization units, each with a scale factor and a gradient-derived word length, and
//! quantized (with an optional fine-grained residual); scale-factor differences are Huffman
//! coded, and the smallest word length (1) uses 2- and 4-dimensional joint code tables.
//!
//! A packet holds one frame (the sync word is `0xaa`); any bytes after the first frame are
//! ignored, as in the SBC decoder. The sample rate, the channel configuration and the number of
//! samples are taken from the header of each frame, and the output is 16-bit planar `f32`.
//!
//! The bitstream and the decode pipeline follow the Apache-2.0 licensed AOSP `libldac` reference
//! encoder, and were validated against the MIT licensed `libldacdec` decoder (see the crate
//! NOTICE).
//!
//! [`FrameHeader`] parses frame headers and gives the length of a frame, so that a stream of
//! consecutive frames (for example the payload of a Bluetooth A2DP media packet) can be split.

mod bit;
mod mdct;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, Channels,
    GenericAudioBufferRef, Position,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::FinalizeResult;
use symphonia_core::codecs::audio::well_known::CODEC_ID_LDAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Error, Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use bit::BitReader;
use mdct::LdacImdct;
use tables::{GRAD_CURVES, SF_WEIGHTS, SF0_HUFF, SF1_HUFF, SPEC_DECODE_2D, SPEC_DECODE_4D};

/// Builds a decode error value for use with `Option::ok_or`/`ok_or_else`.
fn derr(desc: &'static str) -> Error {
    Error::DecodeError(desc)
}

/// The LDAC frame sync word.
pub const SYNCWORD: u8 = 0xaa;

/// The sample rates of the supported sample rate indices (44.1 kHz, 48 kHz, 88.2 kHz, 96 kHz).
const SAMPLE_RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];

/// The base-2 logarithm of the number of samples per channel and frame, per sample rate index.
const SAMPLE_POWERS: [usize; 4] = [7, 7, 8, 8];

/// The maximum number of bands, per sample rate index (12 for 1x, 16 for 2x rates).
const MAX_BANDS: [usize; 4] = [12, 12, 16, 16];

/// The number of quantization units per band count (`ga_nqus_ldac`).
const NQUS: [usize; 17] = [0, 4, 8, 10, 12, 14, 16, 18, 20, 22, 24, 25, 26, 28, 30, 32, 34];

/// The first spectral line of each quantization unit (`ga_isp_ldac`).
const ISP: [usize; 35] = [
    0, 2, 4, 6, 8, 10, 12, 14, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60, 64, 72, 80, 88, 96,
    112, 128, 144, 160, 176, 192, 208, 224, 240, 256,
];

/// The number of spectral lines of each quantization unit (`ga_nsps_ldac`).
const NSPS: [usize; 34] = [
    2, 2, 2, 2, 2, 2, 2, 2, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 8, 8, 8, 8, 16, 16, 16, 16, 16, 16,
    16, 16, 16, 16,
];

/// The number of bits of the word length of a spectrum value, per word-length index
/// (`ga_wl_ldac`).
const WL: [i32; 16] = [0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

/// The most samples per channel of a frame (256 at 88.2/96 kHz).
const MAX_SAMPLES: usize = 256;

/// The largest number of quantization units (34 at 2x rates).
const MAX_QUS: usize = 34;

/// A parsed LDAC frame header.
#[derive(Clone, Copy, Debug)]
pub struct FrameHeader {
    sample_rate_id: u8,
    channel_config: u8,
    frame_length: u32,
    frame_status: u8,
}

impl FrameHeader {
    /// Parses a frame header from `data`, returning `None` when the buffer is too short or the
    /// sync word is wrong.
    pub fn parse(data: &[u8]) -> Option<FrameHeader> {
        if data.len() < 3 || data[0] != SYNCWORD {
            return None;
        }

        let sample_rate_id = data[1] >> 5;
        let channel_config = (data[1] >> 3) & 0x3;
        let frame_length = ((u32::from(data[1] & 0x7) << 6) | (u32::from(data[2]) >> 2)) + 1;
        let frame_status = data[2] & 0x3;

        Some(FrameHeader { sample_rate_id, channel_config, frame_length, frame_status })
    }

    /// The sample rate index (`0` = 44.1 kHz ... `3` = 96 kHz; `4`/`5` are 176.4/192 kHz and are
    /// not supported by this decoder).
    pub fn sample_rate_id(&self) -> u8 {
        self.sample_rate_id
    }

    /// The channel configuration index: `0` mono, `1` dual channel, `2` stereo.
    pub fn channel_config(&self) -> u8 {
        self.channel_config
    }

    /// The sample rate in Hz, or `None` for the unsupported 176.4/192 kHz indices.
    pub fn sample_rate(&self) -> Option<u32> {
        SAMPLE_RATES.get(self.sample_rate_id as usize).copied()
    }

    /// The number of output channels: `1` for mono, `2` otherwise.
    pub fn channels(&self) -> usize {
        if self.channel_config == 0 { 1 } else { 2 }
    }

    /// The number of samples per channel of the frame.
    pub fn samples_per_channel(&self) -> Option<usize> {
        SAMPLE_POWERS.get(self.sample_rate_id as usize).map(|&p| 1 << p)
    }

    /// The number of payload bytes that follow the three header bytes.
    pub fn frame_length(&self) -> u32 {
        self.frame_length
    }

    /// The frame status field (unused by the decoder).
    pub fn frame_status(&self) -> u8 {
        self.frame_status
    }

    /// The total size of the frame in bytes, including the three header bytes.
    pub fn total_frame_size(&self) -> usize {
        3 + self.frame_length as usize
    }
}

/// The per-channel state of the inverse transform.
struct Channel {
    imdct: LdacImdct,
}

fn channels_of(count: usize) -> Channels {
    if count == 1 {
        Channels::Positioned(Position::FRONT_LEFT)
    }
    else {
        Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
    }
}

/// The inverse of the coarse quantizer factor for word-length `wl` (`ga_iqf_ldac`).
fn coarse_step(wl: i32) -> f32 {
    2.0 / ((1i32 << (wl + 1)) - 1) as f32
}

/// The inverse of the fine (residual) quantizer factor for word-length `wl`. The reference
/// divides the coarse step of word-length 15 by the residual's inverse scale factor
/// `2^(wl+1)-1` (`QuantizerFineStepSize` in `libldacdec`); `wl == 0` means no residual.
fn fine_step(wl: i32) -> f32 {
    if wl == 0 { 0.0 } else { (2.0 / ((1i32 << 16) - 1) as f32) / ((1i32 << (wl + 1)) - 1) as f32 }
}

/// The spectrum scale `2^(sf - 15)` (`ga_isf_ldac`), for `sf` in `0..32`.
fn spectrum_scale(sf: i32) -> f32 {
    2.0f32.powi(sf - 15)
}

/// Decodes a scale-factor Huffman code and returns its symbol.
fn huff_decode(br: &mut BitReader<'_>, table: &[(u16, u8)]) -> Option<usize> {
    let max = table.iter().map(|&(_, len)| len).max()?;
    let peeked = br.peek(u32::from(max))?;

    for (symbol, &(code, len)) in table.iter().enumerate() {
        if len == 0 {
            continue;
        }

        if (peeked >> (max - len)) == u32::from(code) {
            br.read(u32::from(len))?;
            return Some(symbol);
        }
    }

    None
}

/// Extends an `n`-bit value to a signed 32-bit integer.
fn sign_extend(value: u32, n: i32) -> i32 {
    let shift = 32 - n;
    ((value << shift) as i32) >> shift
}

/// LDAC decoder.
pub struct LdacDecoder {
    params: AudioCodecParameters,
    channels: [Channel; 2],
    buf: AudioBuffer<f32>,
    // Per-channel decode scratch, reused between frames.
    scale_factors: [[i32; MAX_QUS]; 2],
    precisions: [[i32; MAX_QUS]; 2],
    precisions_fine: [[i32; MAX_QUS]; 2],
    gradient: [i32; MAX_QUS],
    qspec: [[i32; MAX_SAMPLES]; 2],
    rspec: [[i32; MAX_SAMPLES]; 2],
    spectra: [[f32; MAX_SAMPLES]; 2],
}

impl LdacDecoder {
    pub fn try_new(params: &AudioCodecParameters, _opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_LDAC {
            return unsupported_error("ldac: invalid codec");
        }

        // The format of the stream is that of its frames, so the parameters only size the buffer
        // until the first frame.
        let rate = params.sample_rate.filter(|&r| r != 0).unwrap_or(44_100);
        let channels = params.channels.as_ref().map_or(2, Channels::count);
        let channels = if channels == 1 { 1 } else { 2 };

        let spec = AudioSpec::new(rate, channels_of(channels));
        let buf = AudioBuffer::new(spec, MAX_SAMPLES);

        let mut params = params.clone();
        params.with_sample_rate(rate);
        params.with_channels(channels_of(channels));

        Ok(LdacDecoder {
            params,
            channels: [Channel { imdct: LdacImdct::new(7) }, Channel { imdct: LdacImdct::new(7) }],
            buf,
            scale_factors: [[0; MAX_QUS]; 2],
            precisions: [[0; MAX_QUS]; 2],
            precisions_fine: [[0; MAX_QUS]; 2],
            gradient: [0; MAX_QUS],
            qspec: [[0; MAX_SAMPLES]; 2],
            rspec: [[0; MAX_SAMPLES]; 2],
            spectra: [[0.0; MAX_SAMPLES]; 2],
        })
    }

    /// Decodes the block-local band info.
    fn decode_band(br: &mut BitReader<'_>, max_bands: usize) -> Result<usize> {
        let nbands = br.read(4).ok_or_else(|| derr("ldac: truncated frame"))? as usize + 2;
        br.read(1).ok_or_else(|| derr("ldac: truncated frame"))?;

        if nbands < 2 || nbands > max_bands {
            return decode_error("ldac: band count out of bounds");
        }

        Ok(nbands)
    }

    /// Decodes the block-local gradient info and computes the gradient curve, returning the
    /// gradient mode and the boundary (number of adjusted quantization units).
    fn decode_gradient(&mut self, br: &mut BitReader<'_>, nqus: usize) -> Result<(i32, i32)> {
        let mode = br.read(2).ok_or_else(|| derr("ldac: truncated frame"))? as i32;
        let (start_unit, end_unit, start_value, end_value);

        if mode == 0 {
            start_unit = br.read(6).ok_or_else(|| derr("ldac: truncated frame"))? as i32;
            end_unit = br.read(6).ok_or_else(|| derr("ldac: truncated frame"))? as i32 + 1;
            start_value = br.read(5).ok_or_else(|| derr("ldac: truncated frame"))? as i32;
            end_value = br.read(5).ok_or_else(|| derr("ldac: truncated frame"))? as i32;
        }
        else {
            start_unit = br.read(5).ok_or_else(|| derr("ldac: truncated frame"))? as i32;
            end_unit = 26;
            start_value = br.read(5).ok_or_else(|| derr("ldac: truncated frame"))? as i32;
            end_value = 31;
        }

        let boundary = br.read(5).ok_or_else(|| derr("ldac: truncated frame"))? as i32;

        let unit_count = end_unit - start_unit;
        let value_count = end_value - start_value;

        self.gradient.fill(0);

        for i in 0..end_unit.min(nqus as i32) {
            self.gradient[i as usize] = -start_value;
        }
        for i in end_unit.max(0)..nqus as i32 {
            self.gradient[i as usize] = -end_value;
        }

        // `unit_count - 1` indexes `GRAD_CURVES` (50 rows); the encoder never emits a wider
        // range, but a malformed frame must not panic.
        if (1..=50).contains(&unit_count) {
            let curve = &GRAD_CURVES[(unit_count - 1) as usize];

            for i in start_unit..end_unit {
                let i = i as usize;

                if i >= nqus {
                    break;
                }

                let curve_val = i32::from(curve[i - start_unit as usize]);

                if value_count > 0 {
                    self.gradient[i] -= ((curve_val * (value_count - 1)) >> 8) + 1;
                }
                else if value_count < 0 {
                    self.gradient[i] += ((curve_val * (-value_count - 1)) >> 8) + 1;
                }
            }
        }

        Ok((mode, boundary))
    }

    /// Decodes the scale factors of one channel and returns them in `scale_factors`.
    fn decode_scale_factors(
        scale_factors: &mut [i32; MAX_QUS],
        other: &[i32; MAX_QUS],
        ich: usize,
        nqus: usize,
        br: &mut BitReader<'_>,
    ) -> Result<()> {
        let mode = br.read(1).ok_or_else(|| derr("ldac: truncated frame"))?;

        if ich == 0 {
            if mode == 0 {
                Self::decode_sf0(scale_factors, nqus, br)
            }
            else {
                Self::decode_sf1(scale_factors, nqus, br)
            }
        }
        else if mode == 0 {
            Self::decode_sf0(scale_factors, nqus, br)
        }
        else {
            Self::decode_sf2(scale_factors, other, nqus, br)
        }
    }

    /// Scale-factor mode 0: delta Huffman coding relative to an offset.
    fn decode_sf0(
        scale_factors: &mut [i32; MAX_QUS],
        nqus: usize,
        br: &mut BitReader<'_>,
    ) -> Result<()> {
        let bitlen = br.read(2).ok_or_else(|| derr("ldac: truncated frame"))? as i32 + 3;
        let offset = br.read(5).ok_or_else(|| derr("ldac: truncated frame"))? as i32;
        let weight = br.read(3).ok_or_else(|| derr("ldac: truncated frame"))? as usize;
        let mask = (1i32 << bitlen) - 1;

        scale_factors[0] =
            br.read(bitlen as u32).ok_or_else(|| derr("ldac: truncated frame"))? as i32;
        let weights = &SF_WEIGHTS[weight];
        let huff = SF0_HUFF[(bitlen - 3) as usize];

        for i in 1..nqus {
            let diff = huff_decode(br, huff).ok_or_else(|| derr("ldac: invalid Huffman code"))?;
            scale_factors[i] = (scale_factors[i - 1] + diff as i32) & mask;
            scale_factors[i - 1] += offset - i32::from(weights[i - 1]);
        }

        scale_factors[nqus - 1] += offset - i32::from(weights[nqus - 1]);

        Ok(())
    }

    /// Scale-factor mode 1: direct values, possibly with a weight and an offset.
    fn decode_sf1(
        scale_factors: &mut [i32; MAX_QUS],
        nqus: usize,
        br: &mut BitReader<'_>,
    ) -> Result<()> {
        let bitlen = br.read(2).ok_or_else(|| derr("ldac: truncated frame"))? as i32 + 2;

        if bitlen > 4 {
            for i in 0..nqus {
                scale_factors[i] = br.read(5).ok_or_else(|| derr("ldac: truncated frame"))? as i32;
            }
        }
        else {
            let offset = br.read(5).ok_or_else(|| derr("ldac: truncated frame"))? as i32;
            let weight = br.read(3).ok_or_else(|| derr("ldac: truncated frame"))? as usize;
            let weights = &SF_WEIGHTS[weight];

            for i in 0..nqus {
                scale_factors[i] =
                    br.read(bitlen as u32).ok_or_else(|| derr("ldac: truncated frame"))? as i32
                        - i32::from(weights[i])
                        + offset;
            }
        }

        Ok(())
    }

    /// Scale-factor mode 2: signed Huffman-coded differences from the first channel.
    fn decode_sf2(
        scale_factors: &mut [i32; MAX_QUS],
        other: &[i32; MAX_QUS],
        nqus: usize,
        br: &mut BitReader<'_>,
    ) -> Result<()> {
        let bitlen = br.read(2).ok_or_else(|| derr("ldac: truncated frame"))? as i32 + 2;
        let huff = SF1_HUFF[(bitlen - 2) as usize];

        for i in 0..nqus {
            let symbol = huff_decode(br, huff).ok_or_else(|| derr("ldac: invalid Huffman code"))?;
            scale_factors[i] = other[i] + sign_extend(symbol as u32, bitlen);
        }

        Ok(())
    }

    /// Computes the precision mask and the coarse/fine word lengths of one channel.
    fn compute_precisions(
        precisions: &mut [i32; MAX_QUS],
        precisions_fine: &mut [i32; MAX_QUS],
        scale_factors: &[i32; MAX_QUS],
        gradient: &[i32; MAX_QUS],
        mode: i32,
        boundary: i32,
        nqus: usize,
    ) {
        let mut precision_mask = [0i32; MAX_QUS];

        for i in 1..nqus {
            let delta = scale_factors[i] - scale_factors[i - 1];

            if delta > 1 {
                precision_mask[i] += (delta - 1).min(5);
            }
            else if delta < -1 {
                precision_mask[i - 1] += (-delta - 1).min(5);
            }
        }

        for i in 0..nqus {
            let mut precision = scale_factors[i] + gradient[i];

            if mode != 0 {
                precision += precision_mask[i];
            }

            match mode {
                0 => {}
                1 => {
                    if precision > 0 {
                        precision /= 2;
                    }
                }
                2 => {
                    if precision > 0 {
                        precision = (precision * 3) / 8;
                    }
                }
                _ => {
                    if precision > 0 {
                        precision /= 4;
                    }
                }
            }

            precisions[i] = precision.max(1);
        }

        for p in precisions.iter_mut().take(boundary.max(0) as usize) {
            *p += 1;
        }

        for i in 0..nqus {
            precisions_fine[i] = 0;

            if precisions[i] > 15 {
                precisions_fine[i] = (precisions[i] - 15).min(15);
                precisions[i] = 15;
            }
        }
    }

    /// Decodes the quantized spectrum (coarse part).
    fn decode_spectrum(
        qspec: &mut [i32; MAX_SAMPLES],
        precisions: &[i32; MAX_QUS],
        nqus: usize,
        br: &mut BitReader<'_>,
    ) -> Result<()> {
        for i in 0..nqus {
            let start = ISP[i];
            let end = ISP[i + 1];
            let count = NSPS[i];
            let wl = WL[precisions[i] as usize];

            if precisions[i] == 1 {
                let mut index = start;

                if count == 2 {
                    let code = br.read(3).ok_or_else(|| derr("ldac: truncated frame"))? as usize;
                    let packed = i32::from(SPEC_DECODE_2D[code]);

                    if packed < 0 {
                        return decode_error("ldac: invalid 2-dim spectrum code");
                    }

                    qspec[index] = ((packed >> 2) & 3) - 1;
                    qspec[index + 1] = (packed & 3) - 1;
                }
                else {
                    for _ in 0..count / 4 {
                        let code =
                            br.read(7).ok_or_else(|| derr("ldac: truncated frame"))? as usize;
                        let packed = i32::from(SPEC_DECODE_4D[code]);

                        if packed < 0 {
                            return decode_error("ldac: invalid 4-dim spectrum code");
                        }

                        qspec[index] = ((packed >> 6) & 3) - 1;
                        qspec[index + 1] = ((packed >> 4) & 3) - 1;
                        qspec[index + 2] = ((packed >> 2) & 3) - 1;
                        qspec[index + 3] = (packed & 3) - 1;
                        index += 4;
                    }
                }
            }
            else {
                for j in start..end {
                    qspec[j] =
                        br.read_signed(wl as u32).ok_or_else(|| derr("ldac: truncated frame"))?;
                }
            }
        }

        Ok(())
    }

    /// Decodes the residual (fine) spectrum.
    fn decode_spectrum_fine(
        rspec: &mut [i32; MAX_SAMPLES],
        precisions_fine: &[i32; MAX_QUS],
        nqus: usize,
        br: &mut BitReader<'_>,
    ) -> Result<()> {
        rspec.fill(0);

        for i in 0..nqus {
            if precisions_fine[i] > 0 {
                let start = ISP[i];
                let end = ISP[i + 1];
                let wl = WL[precisions_fine[i] as usize];

                for j in start..end {
                    rspec[j] =
                        br.read_signed(wl as u32).ok_or_else(|| derr("ldac: truncated frame"))?;
                }
            }
        }

        Ok(())
    }

    /// Dequantizes and scales the spectrum of one channel.
    fn dequantize(
        spectra: &mut [f32; MAX_SAMPLES],
        scale_factors: &[i32; MAX_QUS],
        precisions: &[i32; MAX_QUS],
        precisions_fine: &[i32; MAX_QUS],
        qspec: &[i32; MAX_SAMPLES],
        rspec: &[i32; MAX_SAMPLES],
        nqus: usize,
    ) {
        spectra.fill(0.0);

        for i in 0..nqus {
            let start = ISP[i];
            let count = NSPS[i];
            let step = coarse_step(precisions[i]);
            let fine = fine_step(precisions_fine[i]);

            for sb in start..start + count {
                spectra[sb] = qspec[sb] as f32 * step + rspec[sb] as f32 * fine;
            }
        }

        for i in 0..nqus {
            let sf = scale_factors[i];

            if sf > 0 {
                let scale = spectrum_scale(sf.clamp(0, 31));
                let start = ISP[i];
                let count = NSPS[i];

                for sb in start..start + count {
                    spectra[sb] *= scale;
                }
            }
        }
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();

        let header = FrameHeader::parse(packet.data)
            .ok_or_else(|| derr("ldac: invalid or truncated frame header"))?;

        let rate = header.sample_rate().ok_or(Error::Unsupported("ldac: 176.4/192 kHz"))?;
        let channels = header.channels();
        let bits = *SAMPLE_POWERS
            .get(header.sample_rate_id as usize)
            .ok_or(Error::Unsupported("ldac: 176.4/192 kHz"))?;
        let samples = 1 << bits;
        let max_bands = MAX_BANDS[header.sample_rate_id as usize];
        let frame_size = header.total_frame_size();

        if packet.data.len() < frame_size {
            return decode_error("ldac: truncated frame");
        }

        // The transform size may change if the sample rate changes between frames.
        for channel in &mut self.channels {
            if channel.imdct.bits() != bits {
                channel.imdct = LdacImdct::new(bits);
            }
        }

        // The channel configuration may change between frames.
        if self.buf.spec().rate() != rate || self.buf.spec().channels().count() != channels {
            let spec = AudioSpec::new(rate, channels_of(channels));
            self.buf = AudioBuffer::new(spec, MAX_SAMPLES);
            self.params.with_sample_rate(rate);
            self.params.with_channels(channels_of(channels));
        }

        self.buf.render_uninit(Some(samples));

        let mut br = BitReader::new(&packet.data[3..frame_size]);

        // The per-block layout: mono and stereo are one block; dual channel is two mono blocks,
        // one per channel. In a mono block the single channel is the "first" (`ich == 0`), so
        // scale factors are coded absolutely; only the second channel of a stereo block uses the
        // relative (mode 2) coding.
        let blocks = if header.channel_config == 1 { 2 } else { 1 };
        let per_block = if header.channel_config == 2 { 2 } else { 1 };

        for block in 0..blocks {
            let nbands = Self::decode_band(&mut br, max_bands)?;
            let nqus = NQUS[nbands];

            let (gradient_mode, gradient_boundary) = self.decode_gradient(&mut br, nqus)?;
            let gradient = self.gradient;

            for ch in 0..per_block {
                let channel = block * per_block + ch;
                // The first channel of the block is the reference for mode-2 coding; copy it out
                // to avoid holding an immutable borrow while mutating the second channel.
                let other = self.scale_factors[block * per_block];

                Self::decode_scale_factors(
                    &mut self.scale_factors[channel],
                    &other,
                    ch,
                    nqus,
                    &mut br,
                )?;

                Self::compute_precisions(
                    &mut self.precisions[channel],
                    &mut self.precisions_fine[channel],
                    &self.scale_factors[channel],
                    &gradient,
                    gradient_mode,
                    gradient_boundary,
                    nqus,
                );

                Self::decode_spectrum(
                    &mut self.qspec[channel],
                    &self.precisions[channel],
                    nqus,
                    &mut br,
                )?;
                Self::decode_spectrum_fine(
                    &mut self.rspec[channel],
                    &self.precisions_fine[channel],
                    nqus,
                    &mut br,
                )?;
                Self::dequantize(
                    &mut self.spectra[channel],
                    &self.scale_factors[channel],
                    &self.precisions[channel],
                    &self.precisions_fine[channel],
                    &self.qspec[channel],
                    &self.rspec[channel],
                    nqus,
                );

                let plane =
                    self.buf.plane_mut(channel).ok_or_else(|| derr("ldac: missing plane"))?;
                self.channels[channel]
                    .imdct
                    .imdct(&self.spectra[channel][..samples], &mut plane[..samples]);
            }

            br.align(8);
        }

        // Convert the transform output to 16-bit planar float, as the reference does before
        // writing `s16` PCM.
        let scale = 1.0 / 32768.0;

        for ch in 0..channels {
            let plane = self.buf.plane_mut(ch).ok_or_else(|| derr("ldac: missing plane"))?;

            for sample in &mut plane[..samples] {
                let rounded = (*sample + 0.5).floor().clamp(-32768.0, 32767.0);
                *sample = rounded * scale;
            }
        }

        Ok(())
    }
}

impl AudioDecoder for LdacDecoder {
    fn reset(&mut self) {
        for channel in &mut self.channels {
            channel.imdct.reset();
        }
        self.buf.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs().first().expect("at least one codec registered").info
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

impl RegisterableAudioDecoder for LdacDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(LdacDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_LDAC, "ldac", "LDAC (Sony Bluetooth A2DP)")]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn header_parses_frame_length() {
        // `0xaa 0x15 0x18` is the header of a 330-byte HQ stereo frame at 44.1 kHz.
        let data = [SYNCWORD, 0x15, 0x18];
        let header = FrameHeader::parse(&data).unwrap();

        assert_eq!(header.sample_rate_id(), 0);
        assert_eq!(header.sample_rate(), Some(44_100));
        assert_eq!(header.channel_config(), 2);
        assert_eq!(header.channels(), 2);
        assert_eq!(header.samples_per_channel(), Some(128));
        assert_eq!(header.frame_length(), 327);
        assert_eq!(header.total_frame_size(), 330);
        assert_eq!(header.frame_status(), 0);
    }

    #[test]
    fn header_rejects_bad_sync() {
        assert!(FrameHeader::parse(&[0x00, 0x15, 0x18]).is_none());
        assert!(FrameHeader::parse(&[]).is_none());
        assert!(FrameHeader::parse(&[SYNCWORD, 0x15]).is_none());
    }
}
