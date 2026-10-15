// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! EVRC (Enhanced Variable Rate Codec) decoder.
//!
//! EVRC is the variable-rate speech codec of TIA/EIA/IS-127 (CDMA Service Option 3), used by CDMA
//! phones, QCP files and 3GPP2 / QuickTime media. A frame is 160 samples at 8 kHz, mono, and is
//! coded at full, half or eighth rate, or is blank or erased.
//!
//! A packet is a single frame: one rate byte (0 = blank, 1 = eighth, 3 = half, 4 = full rate)
//! followed by 0, 2, 10 or 22 bytes of payload. As in the reference decoder (FFmpeg
//! `libavcodec/evrcdec.c`, see `NOTICE`), a packet whose rate byte is missing has its rate guessed
//! from its size, and a packet that cannot be decoded (a size that matches no rate, quarter rate,
//! failing sanity checks of the parameters) is treated as an erased frame, which is concealed from
//! the preceding frames. Every packet therefore yields 160 samples. The adaptive postfilter of the
//! specification is always applied.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
// Disable to better express the specification.
#![allow(clippy::collapsible_else_if)]
// The floating-point expressions mirror the reference decoder, including its promotions to double.
#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]

mod tables;

use std::f64::consts::PI;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef, layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_EVRC;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use tables::{
    ENERGY_QUANT, LSPQ_FULL_CODEBOOK1, LSPQ_FULL_CODEBOOK2, LSPQ_FULL_CODEBOOK3,
    LSPQ_FULL_CODEBOOK4, LSPQ_HALF_CODEBOOK1, LSPQ_HALF_CODEBOOK2, LSPQ_HALF_CODEBOOK3,
    LSPQ_QUANT_CODEBOOK1, LSPQ_QUANT_CODEBOOK2,
};

/// The number of samples in a frame.
const FRAME_SAMPLES: usize = 160;
const FILTER_ORDER: usize = 10;
const NB_SUBFRAMES: usize = 3;
const SUBFRAME_SIZE: usize = 54;
const MIN_DELAY: i32 = 20;
const MAX_DELAY: i32 = 120;
const ACB_SIZE: usize = 128;
/// The minimum separation of the LSPs at the codebook splits.
const MIN_LSP_SEP: f64 = 0.05 / (2.0 * PI);

/// The size of the subframes (TIA/IS-127 table 4.1-1).
const SUBFRAME_SIZES: [usize; NB_SUBFRAMES] = [53, 53, 54];
/// The pitch gain quantizer (TIA/IS-127 table 4.12-1).
const PITCH_GAIN_VQ: [f32; 8] = [0.0, 0.3, 0.55, 0.7, 0.8, 0.9, 1.0, 1.2];
/// The delay estimates of a lost frame (TIA/IS-127 5.2.3.4).
const ESTIMATION_DELAY: [f32; 5] = [55.0, 80.0, 39.0, 71.0, 33.0];

/// A rate of a packet. `RATE_ERRS` is not a rate, but a packet whose rate cannot be determined.
type Rate = i32;

const RATE_ERRS: Rate = -1;
const SILENCE: Rate = 0;
const RATE_QUANT: Rate = 1;
const RATE_QUARTER: Rate = 2;
const RATE_HALF: Rate = 3;
const RATE_FULL: Rate = 4;

/// The parameters of the adaptive postfilter of a rate (TIA/IS-127 table 5.9.1-1).
struct PfCoeff {
    tilt: f32,
    ltgain: f32,
    p1: f32,
    p2: f32,
}

const POSTFILTER_COEFFS: [PfCoeff; 5] = [
    PfCoeff { tilt: 0.0, ltgain: 0.0, p1: 0.0, p2: 0.0 },
    PfCoeff { tilt: 0.0, ltgain: 0.0, p1: 0.57, p2: 0.57 },
    PfCoeff { tilt: 0.0, ltgain: 0.0, p1: 0.0, p2: 0.0 },
    PfCoeff { tilt: 0.35, ltgain: 0.50, p1: 0.50, p2: 0.75 },
    PfCoeff { tilt: 0.20, ltgain: 0.50, p1: 0.57, p2: 0.75 },
];

/// The unpacked parameters of a frame (TIA/IS-127 table 4.21-1).
#[derive(Default, PartialEq)]
struct Frame {
    lpc_flag: u8,
    lsp: [u16; 4],
    pitch_delay: u8,
    delay_diff: u8,
    acb_gain: [u8; 3],
    fcb_shape: [[u16; 4]; 3],
    fcb_gain: [u8; 3],
    energy_gain: u8,
    tty: u8,
}

/// A reader of big-endian bit fields, which reads zeros past the end of the data.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn get(&mut self, n: usize) -> u16 {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.data.get(self.pos >> 3).copied().unwrap_or(0);
            v = (v << 1) | u32::from((byte >> (7 - (self.pos & 7))) & 1);
            self.pos += 1;
        }
        v as u16
    }
}

fn unpack_frame(rate: Rate, data: &[u8]) -> Frame {
    let mut f = Frame::default();
    let mut gb = Bits { data, pos: 0 };

    if rate == RATE_FULL {
        f.lpc_flag = gb.get(1) as u8;
        f.lsp[0] = gb.get(6);
        f.lsp[1] = gb.get(6);
        f.lsp[2] = gb.get(9);
        f.lsp[3] = gb.get(7);
        f.pitch_delay = gb.get(7) as u8;
        f.delay_diff = gb.get(5) as u8;
        for i in 0..3 {
            f.acb_gain[i] = gb.get(3) as u8;
            f.fcb_shape[i][0] = gb.get(8);
            f.fcb_shape[i][1] = gb.get(8);
            f.fcb_shape[i][2] = gb.get(8);
            f.fcb_shape[i][3] = gb.get(11);
            f.fcb_gain[i] = gb.get(5) as u8;
        }
        f.tty = gb.get(1) as u8;
    }
    else if rate == RATE_HALF {
        f.lsp[0] = gb.get(7);
        f.lsp[1] = gb.get(7);
        f.lsp[2] = gb.get(8);
        f.pitch_delay = gb.get(7) as u8;
        for i in 0..3 {
            f.acb_gain[i] = gb.get(3) as u8;
            f.fcb_shape[i][0] = gb.get(10);
            f.fcb_gain[i] = gb.get(4) as u8;
        }
    }
    else if rate == RATE_QUANT {
        f.lsp[0] = gb.get(4);
        f.lsp[1] = gb.get(4);
        f.energy_gain = gb.get(8) as u8;
    }

    f
}

fn size_to_rate(size: usize) -> Rate {
    match size {
        23 => RATE_FULL,
        11 => RATE_HALF,
        6 => RATE_QUARTER,
        3 => RATE_QUANT,
        1 => SILENCE,
        _ => RATE_ERRS,
    }
}

/// Determines the rate from the size and the first byte of the packet, and returns it with the
/// payload.
fn determine_rate(data: &[u8]) -> Option<(Rate, &[u8])> {
    let rate = size_to_rate(data.len());

    if rate >= 0 {
        let first = Rate::from(*data.first()?);
        let rate = if rate > first {
            // The claimed rate and the size mismatch: the lower rate is used.
            first
        }
        else if rate < first {
            // The packet is too small for the claimed rate.
            return None;
        }
        else {
            rate
        };
        Some((rate, &data[1..]))
    }
    else {
        // The rate byte is missing: guess the rate from the size.
        let rate = size_to_rate(data.len() + 1);
        if rate >= 0 { Some((rate, data)) } else { None }
    }
}

/// The LSP codebooks of a rate and their row sizes.
fn lspq_codebooks(rate: Rate) -> (&'static [&'static [f32]], &'static [usize]) {
    static FULL: [&[f32]; 4] =
        [&LSPQ_FULL_CODEBOOK1, &LSPQ_FULL_CODEBOOK2, &LSPQ_FULL_CODEBOOK3, &LSPQ_FULL_CODEBOOK4];
    static HALF: [&[f32]; 3] = [&LSPQ_HALF_CODEBOOK1, &LSPQ_HALF_CODEBOOK2, &LSPQ_HALF_CODEBOOK3];
    static QUANT: [&[f32]; 2] = [&LSPQ_QUANT_CODEBOOK1, &LSPQ_QUANT_CODEBOOK2];

    if rate == RATE_FULL {
        (&FULL, &[2, 2, 3, 3])
    }
    else if rate == RATE_HALF {
        (&HALF, &[3, 3, 4])
    }
    else {
        (&QUANT, &[5, 5])
    }
}

/// Interpolation of the LSPs (TIA/IS-127 5.2.3.1, 5.7.3.2).
fn interpolate_lsp(
    ilsp: &mut [f32; FILTER_ORDER],
    lsp: &[f32; FILTER_ORDER],
    prev: &[f32; FILTER_ORDER],
    index: usize,
) {
    const FACTORS: [f32; 3] = [0.1667, 0.5, 0.8333];
    let wa = (1.0 - f64::from(FACTORS[index])) as f32;
    let wb = FACTORS[index];
    for i in 0..FILTER_ORDER {
        ilsp[i] = wa * prev[i] + wb * lsp[i];
    }
}

/// Reconstruction of the delay contour (TIA/IS-127 5.2.2.3.2).
fn interpolate_delay(dst: &mut [f32; 3], current: f32, prev: f32, index: usize) {
    const FACTORS: [f32; 5] = [0.0, 0.3313, 0.6625, 1.0, 1.0];
    for k in 0..3 {
        let f = FACTORS[index + k];
        dst[k] = ((1.0 - f64::from(f)) * f64::from(prev) + f64::from(f * current)) as f32;
    }
}

/// Converts the quantized, interpolated LSFs to prediction coefficients (TIA/IS-127 5.2.3.2,
/// 4.7.2.2).
fn decode_predictor_coeffs(ilspf: &[f32; FILTER_ORDER], ilpc: &mut [f32; FILTER_ORDER]) {
    const HALF: usize = FILTER_ORDER / 2;

    let mut lsp = [0.0f64; FILTER_ORDER];
    for i in 0..FILTER_ORDER {
        lsp[i] = (2.0 * PI * f64::from(ilspf[i])).cos();
    }

    let mut a = [0.0f32; HALF + 1];
    let mut b = [0.0f32; HALF + 1];
    let mut a1 = [0.0f32; HALF];
    let mut a2 = [0.0f32; HALF];
    let mut b1 = [0.0f32; HALF];
    let mut b2 = [0.0f32; HALF];

    for k in 0..=FILTER_ORDER {
        a[0] = if k < 2 { 0.25 } else { 0.0 };
        b[0] = if k < 2 { if k < 1 { 0.25 } else { -0.25 } } else { 0.0 };

        for i in 0..HALF {
            a[i + 1] =
                (f64::from(a[i]) - 2.0 * lsp[i * 2] * f64::from(a1[i]) + f64::from(a2[i])) as f32;
            b[i + 1] = (f64::from(b[i]) - 2.0 * lsp[i * 2 + 1] * f64::from(b1[i])
                + f64::from(b2[i])) as f32;
            a2[i] = a1[i];
            a1[i] = a[i];
            b2[i] = b1[i];
            b1[i] = b[i];
        }

        if k > 0 {
            ilpc[k - 1] = (2.0 * f64::from(a[HALF] + b[HALF])) as f32;
        }
    }
}

/// The bandwidth expansion `coeff[i] = inbuf[i] * gamma^(i+1)`.
fn bandwidth_expansion(coeff: &mut [f32; FILTER_ORDER], inbuf: &[f32; FILTER_ORDER], gamma: f32) {
    let g = f64::from(gamma);
    let mut fac = g;
    for i in 0..FILTER_ORDER {
        coeff[i] = (f64::from(inbuf[i]) * fac) as f32;
        fac *= g;
    }
}

/// The all-pole synthesis filter (TIA/IS-127 5.2.3.15, 5.7.3.4).
fn synthesis_filter(
    input: &[f32],
    coeffs: &[f32; FILTER_ORDER],
    memory: &mut [f32; FILTER_ORDER],
    out: &mut [f32],
) {
    for (o, &x) in out.iter_mut().zip(input) {
        let mut s = x;
        for j in (1..FILTER_ORDER).rev() {
            s -= coeffs[j] * memory[j];
            memory[j] = memory[j - 1];
        }
        s -= coeffs[0] * memory[0];
        memory[0] = s;
        *o = s;
    }
}

/// The all-zero residual filter.
fn residual_filter(
    output: &mut [f32],
    input: &[f32],
    coef: &[f32; FILTER_ORDER],
    memory: &mut [f32; FILTER_ORDER],
) {
    for (o, &x) in output.iter_mut().zip(input) {
        let mut sum = x;
        for j in (1..FILTER_ORDER).rev() {
            sum += coef[j] * memory[j];
            memory[j] = memory[j - 1];
        }
        sum += coef[0] * memory[0];
        memory[0] = x;
        *o = sum;
    }
}

/// Decodes the 8 pulses of a 35-bit full-rate fixed codebook index.
fn decode_8_pulses_35bits(fixed_index: &[u16; 4], cod: &mut [f32; SUBFRAME_SIZE + 6]) {
    let offset = usize::from((fixed_index[3] >> 9) & 3);

    for i in 0..3 {
        let idx = usize::from(fixed_index[i] & 0x7f);
        let pos1 = (idx / 11) * 5 + ((i + offset) % 5);
        let pos2 = (idx % 11) * 5 + ((i + offset) % 5);

        cod[pos1] = if fixed_index[i] & 0x80 != 0 { -1.0 } else { 1.0 };

        if pos2 < pos1 {
            cod[pos2] = -cod[pos1];
        }
        else {
            cod[pos2] += cod[pos1];
        }
    }

    let idx = usize::from(fixed_index[3] & 0x7f);
    let pos1 = (idx / 11) * 5 + ((3 + offset) % 5);
    let pos2 = (idx % 11) * 5 + ((4 + offset) % 5);

    cod[pos1] = if fixed_index[3] & 0x100 != 0 { -1.0 } else { 1.0 };
    cod[pos2] = if fixed_index[3] & 0x80 != 0 { -1.0 } else { 1.0 };
}

/// Decodes the 3 pulses of a 10-bit half-rate fixed codebook index.
fn decode_3_pulses_10bits(fixed_index: u16, cod: &mut [f32; SUBFRAME_SIZE + 6]) {
    let sign = if fixed_index & 0x200 != 0 { -1.0 } else { 1.0 };

    cod[usize::from(fixed_index & 7) * 7 + 4] += sign;
    cod[usize::from((fixed_index >> 3) & 7) * 7 + 2] -= sign;
    cod[usize::from((fixed_index >> 6) & 7) * 7] += sign;
}

/// The EVRC decoder state.
struct Core {
    bitrate: Rate,
    last_valid_bitrate: Rate,
    frame: Frame,

    lspf: [f32; FILTER_ORDER],
    prev_lspf: [f32; FILTER_ORDER],
    synthesis: [f32; FILTER_ORDER],
    postfilter_fir: [f32; FILTER_ORDER],
    postfilter_iir: [f32; FILTER_ORDER],
    postfilter_residual: [f32; ACB_SIZE + SUBFRAME_SIZE],
    pitch_delay: f32,
    prev_pitch_delay: f32,
    /// The average adaptive codebook gain.
    avg_acb_gain: f32,
    /// The average fixed codebook gain.
    avg_fcb_gain: f32,
    pitch: [f32; ACB_SIZE + FILTER_ORDER + SUBFRAME_SIZE],
    pitch_back: [f32; ACB_SIZE],
    interpolation_coeffs: [f32; 136],
    energy_vector: [f32; NB_SUBFRAMES],
    fade_scale: f32,
    last: f32,

    prev_energy_gain: u8,
    prev_error_flag: bool,
}

impl Core {
    /// Initializes the decoder (TIA/IS-127 5.2).
    fn new() -> Self {
        let mut prev_lspf = [0.0f32; FILTER_ORDER];
        for i in 0..FILTER_ORDER {
            prev_lspf[i] = ((i + 1) as f64 * 0.048) as f32;
        }

        let denom = (2.0 / (2.0 * 8.0 + 1.0)) as f32;
        let mut interpolation_coeffs = [0.0f32; 136];
        let mut idx = 0;

        for i in 0..8 {
            let tt = ((f64::from(i) - 8.0 / 2.0) / 8.0) as f32;

            for n in -8i32..=8 {
                let arg1 = (PI * 0.9 * f64::from(tt - n as f32)) as f32;
                let arg2 = (PI * f64::from(tt - n as f32)) as f32;

                let mut c = 0.9f32;
                if arg1 != 0.0 {
                    let w = (0.54 + 0.46 * f64::from(arg2 * denom).cos()) * f64::from(arg1).sin()
                        / f64::from(arg1);
                    c = (f64::from(c) * w) as f32;
                }
                interpolation_coeffs[idx] = c;
                idx += 1;
            }
        }

        Core {
            bitrate: RATE_ERRS,
            last_valid_bitrate: RATE_QUANT,
            frame: Frame::default(),
            lspf: [0.0; FILTER_ORDER],
            prev_lspf,
            synthesis: [0.0; FILTER_ORDER],
            postfilter_fir: [0.0; FILTER_ORDER],
            postfilter_iir: [0.0; FILTER_ORDER],
            postfilter_residual: [0.0; ACB_SIZE + SUBFRAME_SIZE],
            pitch_delay: 0.0,
            prev_pitch_delay: 40.0,
            avg_acb_gain: 0.0,
            avg_fcb_gain: 0.0,
            pitch: [0.0; ACB_SIZE + FILTER_ORDER + SUBFRAME_SIZE],
            pitch_back: [0.0; ACB_SIZE],
            interpolation_coeffs,
            energy_vector: [0.0; NB_SUBFRAMES],
            fade_scale: 1.0,
            last: 0.0,
            prev_energy_gain: 0,
            prev_error_flag: false,
        }
    }

    /// Decodes the LSFs from the LSP codes and checks for a badly received packet (TIA/IS-127
    /// 5.2.1, 5.7.1). Returns false if the packet is bad.
    fn decode_lspf(&mut self) -> bool {
        let (codebooks, row_sizes) = lspq_codebooks(self.bitrate);

        let mut k = 0;
        for (i, (&cb, &row_size)) in codebooks.iter().zip(row_sizes).enumerate() {
            let base = usize::from(self.frame.lsp[i]) * row_size;
            for j in 0..row_size {
                self.lspf[k] = cb.get(base + j).copied().unwrap_or(0.0);
                k += 1;
            }
        }

        // Check for monotonic LSPs.
        for i in 1..FILTER_ORDER {
            if self.lspf[i] <= self.lspf[i - 1] {
                return false;
            }
        }

        // Check for the minimum separation of the LSPs at the splits.
        let mut k = 0;
        for &row_size in &row_sizes[..row_sizes.len() - 1] {
            k += row_size;
            if f64::from(self.lspf[k] - self.lspf[k - 1]) <= MIN_LSP_SEP {
                return false;
            }
        }

        true
    }

    /// Band-limited interpolation of the excitation at `pos` for a fractional `delay`.
    fn bl_intrp(&mut self, pos: usize, delay: f32) {
        let mut offset = delay.round_ties_even() as i32;

        let mut t = ((f64::from(offset as f32 - delay) + 0.5) * 8.0 + 0.5) as i16;
        if t == 8 {
            t = 0;
            offset -= 1;
        }

        let base = pos as i32 - offset - 8;
        let coef_idx = (t.max(0) as usize) * (2 * 8 + 1);

        let mut sum = 0.0f32;
        for i in 0..2 * 8 + 1 {
            let x = usize::try_from(base + i as i32)
                .ok()
                .and_then(|p| if p == pos { None } else { self.pitch.get(p) })
                .copied()
                .unwrap_or(0.0);
            sum += self.interpolation_coeffs.get(coef_idx + i).copied().unwrap_or(0.0) * x;
        }
        self.pitch[pos] = sum;
    }

    /// Adaptive codebook excitation (TIA/IS-127 5.2.2.3.3, 4.12.5.2).
    fn acb_excitation(&mut self, gain: f32, delay: &[f32; 3], length: usize) {
        let invl = (1.0f64 / f64::from(length as f32)) as f32;
        let dpr = length as f32;

        // The first samples.
        let denom = (delay[1] - delay[0]) * invl;
        for i in 0..length {
            let locdelay = delay[0] + i as f32 * denom;
            self.bl_intrp(ACB_SIZE + i, locdelay);
        }

        // The extra samples, which the interpolation of the next samples needs.
        let denom = (delay[2] - delay[1]) * invl;
        for i in length..length + 10 {
            let locdelay = delay[1] + (i as f32 - dpr) * denom;
            self.bl_intrp(ACB_SIZE + i, locdelay);
        }

        for i in 0..length {
            self.pitch[ACB_SIZE + i] *= gain;
        }
    }

    /// Reconstruction of the fixed codebook excitation (TIA/IS-127 5.2.3.7).
    fn fcb_excitation(
        &self,
        codebook: &[u16; 4],
        excitation: &mut [f32; SUBFRAME_SIZE + 6],
        pitch_gain: f32,
        pitch_lag: i32,
        subframe_size: usize,
    ) {
        if self.bitrate == RATE_FULL {
            decode_8_pulses_35bits(codebook, excitation);
        }
        else {
            decode_3_pulses_10bits(codebook[0], excitation);
        }

        let pitch_gain = pitch_gain.clamp(0.2, 0.9);
        let lag = usize::try_from(pitch_lag).unwrap_or(0);

        for i in lag..subframe_size {
            excitation[i] += pitch_gain * excitation[i - lag];
        }
    }

    /// The adaptive postfilter (TIA/IS-127 5.9).
    fn postfilter(
        &mut self,
        input: &[f32; SUBFRAME_SIZE + 6],
        coeff: &[f32; FILTER_ORDER],
        out: &mut [f32],
        idx: i32,
        pfc: &PfCoeff,
        length: usize,
    ) {
        let input = &input[..length];
        let mut wcoef1 = [0.0f32; FILTER_ORDER];
        let mut wcoef2 = [0.0f32; FILTER_ORDER];
        let mut scratch = [0.0f32; SUBFRAME_SIZE];
        let mut temp = [0.0f32; SUBFRAME_SIZE];
        let mut mem;
        let mut tilt = pfc.tilt;

        bandwidth_expansion(&mut wcoef1, coeff, pfc.p1);
        bandwidth_expansion(&mut wcoef2, coeff, pfc.p2);

        // Tilt compensation filter, TIA/IS-127 5.9.1.
        let mut sum2 = 0.0f32;
        for i in 0..length - 1 {
            sum2 += input[i] * input[i + 1];
        }
        if sum2 < 0.0 {
            tilt = 0.0;
        }

        for i in 0..length {
            scratch[i] = input[i] - tilt * self.last;
            self.last = input[i];
        }

        // Short term residual filter, TIA/IS-127 5.9.2.
        residual_filter(
            &mut self.postfilter_residual[ACB_SIZE..ACB_SIZE + length],
            &scratch[..length],
            &wcoef1,
            &mut self.postfilter_fir,
        );

        // Long term postfilter.
        let res = &self.postfilter_residual;
        let at = |n: usize, lag: i32| res.get((n as i32 - lag) as usize).copied().unwrap_or(0.0);

        let mut sum1 = 0.0f32;
        let mut best = idx;
        for i in MIN_DELAY.min(idx - 3)..=MAX_DELAY.max(idx + 3) {
            let mut sum2 = 0.0f32;
            for n in ACB_SIZE..ACB_SIZE + length {
                sum2 += res[n] * at(n, i);
            }
            if sum2 > sum1 {
                sum1 = sum2;
                best = i;
            }
        }

        let mut sum1 = 0.0f32;
        for i in ACB_SIZE..ACB_SIZE + length {
            sum1 += at(i, best) * at(i, best);
        }
        let mut sum2 = 0.0f32;
        for i in ACB_SIZE..ACB_SIZE + length {
            sum2 += res[i] * at(i, best);
        }

        if sum2 * sum1 == 0.0 || self.bitrate == RATE_QUANT {
            temp[..length].copy_from_slice(&res[ACB_SIZE..ACB_SIZE + length]);
        }
        else {
            let mut gamma = sum2 / sum1;
            if gamma < 0.5 {
                temp[..length].copy_from_slice(&res[ACB_SIZE..ACB_SIZE + length]);
            }
            else {
                gamma = gamma.min(1.0);
                for i in 0..length {
                    temp[i] = res[ACB_SIZE + i] + gamma * pfc.ltgain * at(ACB_SIZE + i, best);
                }
            }
        }

        scratch[..length].copy_from_slice(&temp[..length]);
        mem = self.postfilter_iir;
        let src = scratch;
        synthesis_filter(&src[..length], &wcoef2, &mut mem, &mut scratch[..length]);

        // Gain computation, TIA/IS-127 5.9.4-2.
        let mut sum1 = 0.0f32;
        let mut sum2 = 0.0f32;
        for i in 0..length {
            sum1 += input[i] * input[i];
            sum2 += scratch[i] * scratch[i];
        }
        let gain = if sum2 != 0.0 { f64::from(sum1 / sum2).sqrt() as f32 } else { 1.0 };

        for t in temp[..length].iter_mut() {
            *t *= gain;
        }

        // Short term postfilter.
        synthesis_filter(&temp[..length], &wcoef2, &mut self.postfilter_iir, out);

        self.postfilter_residual.copy_within(length..length + ACB_SIZE, 0);
    }

    /// Concealment of an erased frame.
    fn frame_erasure(&mut self, samples: &mut [f32; FRAME_SAMPLES]) {
        let mut ilspf = [0.0f32; FILTER_ORDER];
        let mut ilpc = [0.0f32; FILTER_ORDER];
        let mut idelay = [0.0f32; NB_SUBFRAMES];
        let mut tmp = [0.0f32; SUBFRAME_SIZE + 6];

        for i in 0..FILTER_ORDER {
            if self.bitrate != RATE_QUANT {
                self.lspf[i] =
                    (f64::from(self.prev_lspf[i]) * 0.875 + 0.125 * (i + 1) as f64 * 0.048) as f32;
            }
            else {
                self.lspf[i] = self.prev_lspf[i];
            }
        }

        if self.prev_error_flag {
            self.avg_acb_gain = (f64::from(self.avg_acb_gain) * 0.75) as f32;
        }
        if self.bitrate == RATE_FULL {
            self.pitch_back.copy_from_slice(&self.pitch[..ACB_SIZE]);
        }
        self.bitrate = if self.last_valid_bitrate == RATE_QUANT { RATE_QUANT } else { RATE_FULL };

        if self.bitrate == RATE_FULL || self.bitrate == RATE_HALF {
            self.pitch_delay = self.prev_pitch_delay;
        }
        else {
            let mut sum = 0.0f32;

            idelay = [MIN_DELAY as f32; 3];

            for i in 0..NB_SUBFRAMES {
                sum += ENERGY_QUANT[usize::from(self.prev_energy_gain) * 3 + i];
            }
            sum /= NB_SUBFRAMES as f32;
            sum = 10.0f64.powf(f64::from(sum)) as f32;
            self.energy_vector = [sum; NB_SUBFRAMES];
        }

        if f64::from((self.pitch_delay - self.prev_pitch_delay).abs()) > 15.0 {
            self.prev_pitch_delay = self.pitch_delay;
        }

        let mut out_pos = 0;
        for i in 0..NB_SUBFRAMES {
            let subframe_size = SUBFRAME_SIZES[i];

            let (lspf, prev_lspf) = (self.lspf, self.prev_lspf);
            interpolate_lsp(&mut ilspf, &lspf, &prev_lspf, i);

            if self.bitrate != RATE_QUANT {
                if self.avg_acb_gain < 0.3 {
                    idelay =
                        [ESTIMATION_DELAY[i], ESTIMATION_DELAY[i + 1], ESTIMATION_DELAY[i + 2]];
                }
                else {
                    interpolate_delay(&mut idelay, self.pitch_delay, self.prev_pitch_delay, i);
                }
            }

            let pitch_lag = ((idelay[1] + idelay[0]) / 2.0).round_ties_even() as i32;
            decode_predictor_coeffs(&ilspf, &mut ilpc);

            if self.bitrate != RATE_QUANT {
                self.acb_excitation(self.avg_acb_gain, &idelay, subframe_size);
                for j in 0..subframe_size {
                    self.pitch[ACB_SIZE + j] *= self.fade_scale;
                }
                self.fade_scale = f64::from(self.fade_scale - 0.05).max(0.0) as f32;
            }
            else {
                for j in 0..subframe_size {
                    self.pitch[ACB_SIZE + j] = self.energy_vector[i];
                }
            }

            self.pitch.copy_within(subframe_size..subframe_size + ACB_SIZE, 0);

            if self.bitrate != RATE_QUANT && self.avg_acb_gain < 0.4 {
                let f = (0.1 * f64::from(self.avg_fcb_gain)) as f32;
                for j in 0..subframe_size {
                    self.pitch[ACB_SIZE + j] += f;
                }
            }
            else if self.bitrate == RATE_QUANT {
                for j in 0..subframe_size {
                    self.pitch[ACB_SIZE + j] = self.energy_vector[i];
                }
            }

            let exc: [f32; SUBFRAME_SIZE] = std::array::from_fn(|j| self.pitch[ACB_SIZE + j]);
            synthesis_filter(
                &exc[..subframe_size],
                &ilpc,
                &mut self.synthesis,
                &mut tmp[..subframe_size],
            );
            let pfc = &POSTFILTER_COEFFS[self.bitrate as usize];
            self.postfilter(
                &tmp,
                &ilpc,
                &mut samples[out_pos..out_pos + subframe_size],
                pitch_lag,
                pfc,
                subframe_size,
            );

            out_pos += subframe_size;
        }
    }

    /// Decodes a frame without erasure. Returns false if the frame is to be treated as erased.
    fn decode_normal(&mut self, data: &[u8], samples: &mut [f32; FRAME_SAMPLES]) -> bool {
        let Some((rate, payload)) = determine_rate(data)
        else {
            self.bitrate = RATE_ERRS;
            return false;
        };
        self.bitrate = rate;

        if rate <= SILENCE || rate == RATE_QUARTER {
            return false;
        }
        if rate == RATE_QUANT && self.last_valid_bitrate == RATE_FULL && !self.prev_error_flag {
            return false;
        }

        self.frame = unpack_frame(rate, payload);

        if rate != RATE_QUANT {
            if self.frame == Frame::default() {
                return false;
            }
        }
        else if self.frame.lsp[0] == 0xf
            && self.frame.lsp[1] == 0xf
            && self.frame.energy_gain == 0xff
        {
            return false;
        }

        if !self.decode_lspf() {
            return false;
        }

        let mut idelay = [0.0f32; 3];

        if rate == RATE_FULL || rate == RATE_HALF {
            // Pitch delay parameter checking as per TIA/IS-127 5.1.5.1.
            if i32::from(self.frame.pitch_delay) > MAX_DELAY - MIN_DELAY {
                return false;
            }

            self.pitch_delay = f32::from(self.frame.pitch_delay) + MIN_DELAY as f32;

            // Delay diff parameter checking as per TIA/IS-127 5.1.5.2.
            if self.frame.delay_diff != 0 {
                let p = (self.pitch_delay - f32::from(self.frame.delay_diff) + 16.0) as i32;
                if p < MIN_DELAY || p > MAX_DELAY {
                    return false;
                }
            }

            // Delay contour reconstruction as per TIA/IS-127 5.2.2.2.
            if self.frame.delay_diff != 0 && rate == RATE_FULL && self.prev_error_flag {
                self.pitch[..ACB_SIZE].copy_from_slice(&self.pitch_back);

                let mut delay = self.prev_pitch_delay;
                self.prev_pitch_delay =
                    (f64::from(delay - f32::from(self.frame.delay_diff)) + 16.0) as f32;

                if f64::from((self.pitch_delay - delay).abs()) > 15.0 {
                    delay = self.pitch_delay;
                }

                for i in 0..NB_SUBFRAMES {
                    let subframe_size = SUBFRAME_SIZES[i];

                    interpolate_delay(&mut idelay, delay, self.prev_pitch_delay, i);
                    self.acb_excitation(self.avg_acb_gain, &idelay, subframe_size);
                    self.pitch.copy_within(subframe_size..subframe_size + ACB_SIZE, 0);
                }
            }

            // Smoothing of the decoded delay as per TIA/IS-127 5.2.2.5.
            if f64::from((self.pitch_delay - self.prev_pitch_delay).abs()) > 15.0 {
                self.prev_pitch_delay = self.pitch_delay;
            }

            self.avg_acb_gain = 0.0;
            self.avg_fcb_gain = 0.0;
        }
        else {
            idelay = [MIN_DELAY as f32; 3];

            // Decode the frame energy vectors as per TIA/IS-127 5.7.2.
            let gain = usize::from(self.frame.energy_gain);
            for i in 0..NB_SUBFRAMES {
                self.energy_vector[i] = 10.0f64.powf(f64::from(ENERGY_QUANT[gain * 3 + i])) as f32;
            }
            self.prev_energy_gain = self.frame.energy_gain;
        }

        let mut out_pos = 0;
        for i in 0..NB_SUBFRAMES {
            let mut tmp = [0.0f32; SUBFRAME_SIZE + 6];
            let subframe_size = SUBFRAME_SIZES[i];
            let mut ilspf = [0.0f32; FILTER_ORDER];
            let mut ilpc = [0.0f32; FILTER_ORDER];

            let (lspf, prev_lspf) = (self.lspf, self.prev_lspf);
            interpolate_lsp(&mut ilspf, &lspf, &prev_lspf, i);

            if rate != RATE_QUANT {
                interpolate_delay(&mut idelay, self.pitch_delay, self.prev_pitch_delay, i);
            }

            let pitch_lag = ((idelay[1] + idelay[0]) / 2.0).round_ties_even() as i32;
            decode_predictor_coeffs(&ilspf, &mut ilpc);

            // Bandwidth expansion as per TIA/IS-127 5.2.3.3.
            if self.frame.lpc_flag != 0 && self.prev_error_flag {
                let src = ilpc;
                bandwidth_expansion(&mut ilpc, &src, 0.75);
            }

            if rate != RATE_QUANT {
                let f = (if rate == RATE_HALF { 0.5f64 } else { 0.25f64 }
                    * f64::from(i32::from(self.frame.fcb_gain[i]) + 1))
                .exp() as f32;
                let acb_sum = PITCH_GAIN_VQ[usize::from(self.frame.acb_gain[i] & 7)];
                self.avg_acb_gain += acb_sum / NB_SUBFRAMES as f32;
                self.avg_fcb_gain += f / NB_SUBFRAMES as f32;

                self.acb_excitation(acb_sum, &idelay, subframe_size);
                let shape = self.frame.fcb_shape[i];
                self.fcb_excitation(&shape, &mut tmp, acb_sum, pitch_lag, subframe_size);

                // Total excitation generation as per TIA/IS-127 5.2.3.9.
                for j in 0..subframe_size {
                    self.pitch[ACB_SIZE + j] += f * tmp[j];
                }
                self.fade_scale = (f64::from(self.fade_scale) + 0.2).min(1.0) as f32;
            }
            else {
                for j in 0..subframe_size {
                    self.pitch[ACB_SIZE + j] = self.energy_vector[i];
                }
            }

            self.pitch.copy_within(subframe_size..subframe_size + ACB_SIZE, 0);

            let exc: [f32; SUBFRAME_SIZE] = std::array::from_fn(|j| self.pitch[ACB_SIZE + j]);
            synthesis_filter(
                &exc[..subframe_size],
                &ilpc,
                &mut self.synthesis,
                &mut tmp[..subframe_size],
            );
            let pfc = &POSTFILTER_COEFFS[rate as usize];
            self.postfilter(
                &tmp,
                &ilpc,
                &mut samples[out_pos..out_pos + subframe_size],
                pitch_lag,
                pfc,
                subframe_size,
            );

            out_pos += subframe_size;
        }

        true
    }

    /// Decodes a packet of a frame to 160 samples.
    fn decode(&mut self, data: &[u8], out: &mut [f32; FRAME_SAMPLES]) {
        let error_flag = !self.decode_normal(data, out);
        if error_flag {
            self.frame_erasure(out);
        }

        self.prev_lspf = self.lspf;
        self.prev_error_flag = error_flag;
        self.last_valid_bitrate = self.bitrate;

        if self.bitrate != RATE_QUANT {
            self.prev_pitch_delay = self.pitch_delay;
        }

        for s in out.iter_mut() {
            *s /= 32768.0;
        }
    }
}

/// EVRC decoder.
pub struct EvrcDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    core: Box<Core>,
}

impl EvrcDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_EVRC {
            return unsupported_error("evrc: invalid codec");
        }

        // EVRC is 8 kHz mono. The sample rate is often missing from the container.
        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            _ => 8000,
        };

        if let Some(channels) = &params.channels {
            if channels.count() != 1 {
                return unsupported_error("evrc: only mono is supported");
            }
        }

        let spec = AudioSpec::new(rate, layouts::CHANNEL_LAYOUT_MONO);
        let buf = AudioBuffer::new(spec, FRAME_SAMPLES);

        let mut params = params.clone();
        params.with_sample_rate(rate).with_channels(layouts::CHANNEL_LAYOUT_MONO);

        Ok(EvrcDecoder { params, opts: *opts, buf, core: Box::new(Core::new()) })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();
        self.buf.render_uninit(Some(FRAME_SAMPLES));

        let Some(plane) = self.buf.plane_mut(0)
        else {
            return decode_error("evrc: no output plane");
        };
        let Ok(out) = <&mut [f32; FRAME_SAMPLES]>::try_from(plane)
        else {
            return decode_error("evrc: bad output plane");
        };

        self.core.decode(packet.data, out);

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for EvrcDecoder {
    fn reset(&mut self) {
        *self.core = Core::new();
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

impl RegisterableAudioDecoder for EvrcDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(EvrcDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_EVRC, "evrc", "EVRC")]
    }
}
