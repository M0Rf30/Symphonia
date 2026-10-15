// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! QCELP 13K (PureVoice) decoder.
//!
//! QCELP is the variable-rate speech codec of TIA/EIA/IS-733 ("QCELP-13K"), used by CDMA phones,
//! QCP files and 3GPP2 / QuickTime media. A frame is 160 samples at 8 kHz, mono, and is coded at
//! full, half, quarter or eighth rate, or is blank or erased.
//!
//! A packet is a single frame: one rate byte (0 = blank, 1 = eighth, 2 = quarter, 3 = half,
//! 4 = full rate) followed by 0, 3, 7, 16 or 34 bytes of payload. As in the reference decoder
//! (FFmpeg `libavcodec/qcelpdec.c`, see `NOTICE`), a packet whose rate byte is missing has its
//! rate guessed from its size, and a packet that cannot be decoded (a size that matches no rate,
//! reserved bits set, failing sanity checks of the parameters) is treated as an erased frame,
//! which is concealed from the preceding frames. Every packet therefore yields 160 samples.

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

mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef, layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_QCELP;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use tables::{
    G12GA, LSPVQ1, LSPVQ2, LSPVQ3, LSPVQ4, LSPVQ5, RATE_FULL_BITMAP, RATE_FULL_CODEBOOK,
    RATE_HALF_BITMAP, RATE_HALF_CODEBOOK, RATE_OCTAVE_BITMAP, RATE_QUARTER_BITMAP,
};

/// The number of samples in a frame.
const FRAME_SAMPLES: usize = 160;
/// The number of samples in a pitch subframe.
const SUBFRAME_SAMPLES: usize = 40;
/// The order of the linear predictor.
const LPC_ORDER: usize = 10;
/// The length of the pitch filter memories: 143 samples of history and one frame.
const PITCH_MEM_LEN: usize = 303;
/// The number of samples of history in the pitch filter memories.
const PITCH_HISTORY: usize = 143;
/// The final gain scalefactor (the table of codebook gains is in units of 1 / `QCELP_SCALE`).
const QCELP_SCALE: f64 = 8192.0;
/// sqrt(1.887), the maximum of the pseudorandom white sequence of the rate 1/4 and 1/8 codebooks
/// (TIA/EIA/IS-733 2.4.8.1.2).
const QCELP_SQRT1887: f64 = 1.373681186;
/// The LSP spread factor used for rate 1/8 and erased frames.
const QCELP_LSP_SPREAD_FACTOR: f64 = 0.02;
/// The predictor coefficient for the conversion of LSP codes of rate 1/8 and erased frames.
const QCELP_LSP_OCTAVE_PREDICTOR: f64 = 29.0 / 32.0;
/// The initial coefficient for the bandwidth expansion of the LPC (TIA/EIA/IS-733 2.4.3.3.5).
const QCELP_BANDWIDTH_EXPANSION_COEFF: f64 = 0.9883;

/// The pre-calculated hamming-windowed sinc table of the fractional pitch interpolation. Only half
/// the table is needed because of symmetry (TIA/EIA/IS-733 2.4.5.2-2/3).
const HAMMSINC_TABLE: [f32; 4] = [-0.006822, 0.041249, -0.143459, 0.588863];

/// The impulse response of the band-pass filter of the rate 1/4 white excitation. Only half the
/// table is needed because of symmetry (TIA/EIA/IS-733 2.4.8.1.2-1.1).
const RND_FIR_COEFS: [f64; 11] = [
    -1.344519e-1,
    1.735384e-2,
    -6.905826e-2,
    2.434368e-2,
    -8.210701e-2,
    3.041388e-2,
    -9.251384e-2,
    3.501983e-2,
    -9.918777e-2,
    3.749518e-2,
    8.985137e-1,
];

/// The postfilter pole weights, 0.775^(n+1) (TIA/EIA/IS-733 2.4.8.6).
const POW_0_775: [f32; LPC_ORDER] = [
    0.775000, 0.600625, 0.465484, 0.360750, 0.279582, 0.216676, 0.167924, 0.130141, 0.100859,
    0.078166,
];
/// The postfilter zero weights, 0.625^(n+1) (TIA/EIA/IS-733 2.4.8.6).
const POW_0_625: [f32; LPC_ORDER] = [
    0.625000, 0.390625, 0.244141, 0.152588, 0.095367, 0.059605, 0.037253, 0.023283, 0.014552,
    0.009095,
];

/// The table for computing Ga, the decoded linear codebook gain magnitude (TIA/EIA/IS-733
/// 2.4.6.2.1-3).
const G12GA_F32: [f32; 61] = {
    let mut t = [0f32; 61];
    let mut i = 0;
    while i < 61 {
        t[i] = (G12GA[i] / QCELP_SCALE) as f32;
        i += 1;
    }
    t
};

// Offsets of the parameters in the unpacked frame; they are the byte offsets of the bitmap tables.
const CBSIGN: usize = 0;
const CBGAIN: usize = 16;
const CINDEX: usize = 32;
const PLAG: usize = 48;
const PFRAC: usize = 52;
const PGAIN: usize = 56;
const LSPV: usize = 60;
const RESERVED: usize = 70;
/// The length of the unpacked frame.
const FRAME_LEN: usize = 71;

/// The rate of a packet. The order is the order of the rates in the reference decoder; the value
/// is the rate byte of a packet, except for `InsufficientQuality`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i8)]
enum Rate {
    /// Insufficient frame quality (an erased frame).
    InsufficientQuality = -1,
    Silence = 0,
    Octave = 1,
    Quarter = 2,
    Half = 3,
    Full = 4,
}

impl Rate {
    fn from_size(size: usize) -> Rate {
        match size {
            35 => Rate::Full,
            17 => Rate::Half,
            8 => Rate::Quarter,
            4 => Rate::Octave,
            1 => Rate::Silence,
            _ => Rate::InsufficientQuality,
        }
    }

    fn from_byte(b: u8) -> Rate {
        match b {
            0 => Rate::Silence,
            1 => Rate::Octave,
            2 => Rate::Quarter,
            3 => Rate::Half,
            4 => Rate::Full,
            _ => Rate::InsufficientQuality,
        }
    }

    fn bitmap(self) -> &'static [(u8, u8, u8)] {
        match self {
            Rate::Full => &RATE_FULL_BITMAP,
            Rate::Half => &RATE_HALF_BITMAP,
            Rate::Quarter => &RATE_QUARTER_BITMAP,
            Rate::Octave => &RATE_OCTAVE_BITMAP,
            Rate::Silence | Rate::InsufficientQuality => &[],
        }
    }
}

/// Determines the rate and the payload of a packet from its size and its first byte
/// (TIA/EIA/IS-733 2.4.8.7.1).
fn determine_rate(data: &[u8]) -> (Rate, &[u8]) {
    let rate = Rate::from_size(data.len());

    if rate >= Rate::Silence {
        // The packet has a rate byte.
        let claimed = data.first().copied().unwrap_or(0);
        let payload = data.get(1..).unwrap_or(&[]);

        if rate > Rate::from_byte(claimed) {
            // The claimed rate and the size of the packet mismatch. Trust the rate byte if the
            // packet is large enough for it.
            (Rate::from_byte(claimed), payload)
        }
        else if rate < Rate::from_byte(claimed) {
            // The packet is too small for the claimed rate (or the rate byte is invalid).
            (Rate::InsufficientQuality, payload)
        }
        else {
            (rate, payload)
        }
    }
    else {
        // The rate byte may be missing. Guess the rate from the size of the packet.
        let rate = Rate::from_size(data.len() + 1);
        (rate, data)
    }
}

/// Reads the unpacked frame from the payload of a packet.
fn unpack(rate: Rate, payload: &[u8], frame: &mut [u8; FRAME_LEN]) -> Result<()> {
    *frame = [0; FRAME_LEN];

    let mut bs = BitReaderLtr::new(payload);

    for &(index, pos, len) in rate.bitmap() {
        let v = match bs.read_bits_leq32(u32::from(len)) {
            Ok(v) => v,
            Err(_) => return decode_error("qcelp: truncated frame"),
        };
        frame[usize::from(index)] |= (v << pos) as u8;
    }

    Ok(())
}

/// The sum of the products of two vectors.
fn scalar_product(a: &[f32], b: &[f32]) -> f32 {
    let mut sum = 0.0f32;
    for (&x, &y) in a.iter().zip(b) {
        sum += x * y;
    }
    sum
}

/// Scales `v_in` to the given sum of squares.
fn scale_vector_to_given_sum_of_squares(out: &mut [f32], v_in: &[f32], sum_of_squares: f32) {
    let mut scale = scalar_product(v_in, v_in);
    if scale != 0.0 {
        scale = f64::from(sum_of_squares / scale).sqrt() as f32;
    }
    for (o, &i) in out.iter_mut().zip(v_in) {
        *o = i * scale;
    }
}

/// Computes the Pa / (1 + z(-1)) or Qa / (1 - z(-1)) coefficients needed for the LSP to LPC
/// conversion, from every second element of `lsp` (TIA/EIA/IS-733 2.4.3.3.5-1/2).
fn lsp2polyf(lsp: &[f64], f: &mut [f64; 6], half_order: usize) {
    f[0] = 1.0;
    f[1] = -2.0 * lsp[0];
    for i in 2..=half_order {
        let val = -2.0 * lsp[2 * i - 2];
        f[i] = val * f[i - 1] + 2.0 * f[i - 2];
        for j in (2..i).rev() {
            f[j] += f[j - 1] * val + f[j - 2];
        }
        f[1] += val;
    }
}

/// Converts 10 LSPs in the cosine domain to 10 LPC coefficients. FFmpeg `ff_acelp_lspd2lpc`.
fn lspd2lpc(lsp: &[f64; LPC_ORDER], lpc: &mut [f32; LPC_ORDER]) {
    const HALF_ORDER: usize = LPC_ORDER / 2;

    let mut pa = [0.0f64; 6];
    let mut qa = [0.0f64; 6];

    lsp2polyf(lsp, &mut pa, HALF_ORDER);
    lsp2polyf(&lsp[1..], &mut qa, HALF_ORDER);

    for h in (0..HALF_ORDER).rev() {
        let paf = pa[h + 1] + pa[h];
        let qaf = qa[h + 1] - qa[h];

        lpc[h] = (0.5 * (paf + qaf)) as f32;
        lpc[LPC_ORDER - 1 - h] = (0.5 * (paf - qaf)) as f32;
    }
}

/// Reconstructs the LPC coefficients from the line spectral pair frequencies and performs the
/// bandwidth expansion (TIA/EIA/IS-733 2.4.3.3.5).
fn lspf2lpc(lspf: &[f32; LPC_ORDER], lpc: &mut [f32; LPC_ORDER]) {
    let mut lsp = [0.0f64; LPC_ORDER];
    for (l, &f) in lsp.iter_mut().zip(lspf) {
        *l = (std::f64::consts::PI * f64::from(f)).cos();
    }

    lspd2lpc(&lsp, lpc);

    let mut coeff = QCELP_BANDWIDTH_EXPANSION_COEFF;
    for c in lpc.iter_mut() {
        *c = (f64::from(*c) * coeff) as f32;
        coeff *= QCELP_BANDWIDTH_EXPANSION_COEFF;
    }
}

/// Applies the pitch filter in pitch-subframe steps (TIA/EIA/IS-733 2.4.5.2, 2.4.8.7.2).
///
/// `memory` holds the 143 previous output samples followed by the 160 new output samples.
fn do_pitchfilter(
    memory: &mut [f32; PITCH_MEM_LEN],
    v_in: &[f32],
    gain: &[f32; 4],
    lag: &[u8; 4],
    pfrac: &[u8; 4],
) {
    for i in 0..4 {
        for k in 0..SUBFRAME_SAMPLES {
            let n = i * SUBFRAME_SAMPLES + k;
            let out = PITCH_HISTORY + n;
            let input = v_in.get(n).copied().unwrap_or(0.0);

            if gain[i] != 0.0 {
                let lagged = out.saturating_sub(usize::from(lag[i]));

                let v = if pfrac[i] != 0 {
                    // A fractional lag.
                    let mut v = 0.0f32;
                    for j in 0..4 {
                        let a = memory.get((lagged + j).wrapping_sub(4)).copied().unwrap_or(0.0);
                        let b = memory.get(lagged + 3 - j).copied().unwrap_or(0.0);
                        v += HAMMSINC_TABLE[j] * (a + b);
                    }
                    v
                }
                else {
                    memory.get(lagged).copied().unwrap_or(0.0)
                };

                memory[out] = input + gain[i] * v;
            }
            else {
                memory[out] = input;
            }
        }
    }

    memory.copy_within(FRAME_SAMPLES.., 0);
}

/// The state of the decoder.
struct Core {
    rate: Rate,
    prev_rate: Rate,
    /// The unpacked frame.
    frame: [u8; FRAME_LEN],
    erasure_count: u8,
    /// The count of consecutive rate 1/8 frames.
    octave_count: u8,
    prev_lspf: [f32; LPC_ORDER],
    /// The LSP predictor for rate 1/8 and erased frames.
    predictor_lspf: [f32; LPC_ORDER],
    pitch_synthesis_filter_mem: [f32; PITCH_MEM_LEN],
    pitch_pre_filter_mem: [f32; PITCH_MEM_LEN],
    rnd_fir_filter_mem: [f32; 180],
    formant_mem: [f32; 170],
    last_codebook_gain: f32,
    prev_g1: [i32; 2],
    pitch_gain: [f32; 4],
    pitch_lag: [u8; 4],
    first16bits: u16,
    postfilter_synth_mem: [f32; LPC_ORDER],
    postfilter_agc_mem: f32,
    postfilter_tilt_mem: f32,
}

impl Core {
    /// Initializes the decoder according to TIA/EIA/IS-733 2.4.9.
    fn new() -> Self {
        let mut prev_lspf = [0.0f32; LPC_ORDER];
        for (i, l) in prev_lspf.iter_mut().enumerate() {
            *l = ((i + 1) as f64 / 11.0) as f32;
        }

        Core {
            rate: Rate::Silence,
            prev_rate: Rate::Silence,
            frame: [0; FRAME_LEN],
            erasure_count: 0,
            octave_count: 0,
            prev_lspf,
            predictor_lspf: [0.0; LPC_ORDER],
            pitch_synthesis_filter_mem: [0.0; PITCH_MEM_LEN],
            pitch_pre_filter_mem: [0.0; PITCH_MEM_LEN],
            rnd_fir_filter_mem: [0.0; 180],
            formant_mem: [0.0; 170],
            last_codebook_gain: 0.0,
            prev_g1: [0; 2],
            pitch_gain: [0.0; 4],
            pitch_lag: [0; 4],
            first16bits: 0,
            postfilter_synth_mem: [0.0; LPC_ORDER],
            postfilter_agc_mem: 0.0,
            postfilter_tilt_mem: 0.0,
        }
    }

    /// Decodes the 10 quantized LSP frequencies from the LSPV/LSP transmission codes of any rate,
    /// and checks for badly received packets. Returns `false` if the packet is badly received
    /// (TIA/EIA/IS-733 2.4.3.2.6.2-2, 2.4.8.7.3).
    fn decode_lspf(&mut self, lspf: &mut [f32; LPC_ORDER]) -> bool {
        if self.rate == Rate::Octave || self.rate == Rate::InsufficientQuality {
            let predictors =
                if self.prev_rate != Rate::Octave && self.prev_rate != Rate::InsufficientQuality {
                    self.prev_lspf
                }
                else {
                    self.predictor_lspf
                };

            let smooth: f32 = if self.rate == Rate::Octave {
                self.octave_count = self.octave_count.wrapping_add(1);

                for i in 0..LPC_ORDER {
                    let spread = if self.frame[LSPV + i] != 0 {
                        QCELP_LSP_SPREAD_FACTOR
                    }
                    else {
                        -QCELP_LSP_SPREAD_FACTOR
                    };
                    let v = (spread
                        + f64::from(predictors[i]) * 29.0 / 32.0
                        + (i + 1) as f64 * ((1.0 - QCELP_LSP_OCTAVE_PREDICTOR) / 11.0))
                        as f32;
                    lspf[i] = v;
                    self.predictor_lspf[i] = v;
                }
                if self.octave_count < 10 { 0.875 } else { 0.1 }
            }
            else {
                let mut erasure_coeff = QCELP_LSP_OCTAVE_PREDICTOR as f32;

                if self.erasure_count > 1 {
                    let f = if self.erasure_count < 4 { 0.9 } else { 0.7 };
                    erasure_coeff = (f64::from(erasure_coeff) * f) as f32;
                }

                for i in 0..LPC_ORDER {
                    let v = (i + 1) as f32 * (1.0 - erasure_coeff) / 11.0
                        + erasure_coeff * predictors[i];
                    lspf[i] = v;
                    self.predictor_lspf[i] = v;
                }
                0.125
            };

            // Check the stability of the LSP frequencies.
            lspf[0] = f64::from(lspf[0]).max(QCELP_LSP_SPREAD_FACTOR) as f32;
            for i in 1..LPC_ORDER {
                lspf[i] =
                    f64::from(lspf[i]).max(f64::from(lspf[i - 1]) + QCELP_LSP_SPREAD_FACTOR) as f32;
            }

            lspf[9] = f64::from(lspf[9]).min(1.0 - QCELP_LSP_SPREAD_FACTOR) as f32;
            for i in (1..LPC_ORDER).rev() {
                lspf[i - 1] =
                    f64::from(lspf[i - 1]).min(f64::from(lspf[i]) - QCELP_LSP_SPREAD_FACTOR) as f32;
            }

            // Low-pass filter the LSP frequencies.
            let weight_b = (1.0 - f64::from(smooth)) as f32;
            for i in 0..LPC_ORDER {
                lspf[i] = smooth * lspf[i] + weight_b * self.prev_lspf[i];
            }
        }
        else {
            self.octave_count = 0;

            const VQ: [&[[u16; 2]]; 5] = [&LSPVQ1, &LSPVQ2, &LSPVQ3, &LSPVQ4, &LSPVQ5];

            let mut tmp_lspf = 0.0f32;
            for i in 0..5 {
                let Some(e) = VQ[i].get(usize::from(self.frame[LSPV + i]))
                else {
                    return false;
                };
                tmp_lspf = (f64::from(tmp_lspf) + f64::from(e[0]) * 0.0001) as f32;
                lspf[2 * i] = tmp_lspf;
                tmp_lspf = (f64::from(tmp_lspf) + f64::from(e[1]) * 0.0001) as f32;
                lspf[2 * i + 1] = tmp_lspf;
            }

            // Check for badly received packets.
            if self.rate == Rate::Quarter {
                if f64::from(lspf[9]) <= 0.70 || f64::from(lspf[9]) >= 0.97 {
                    return false;
                }
                for i in 3..LPC_ORDER {
                    if f64::from((lspf[i] - lspf[i - 2]).abs()) < 0.08 {
                        return false;
                    }
                }
            }
            else {
                if f64::from(lspf[9]) <= 0.66 || f64::from(lspf[9]) >= 0.985 {
                    return false;
                }
                for i in 4..LPC_ORDER {
                    if f64::from((lspf[i] - lspf[i - 4]).abs()) < 0.0931 {
                        return false;
                    }
                }
            }
        }

        true
    }

    /// Converts the codebook transmission codes to gain and index (TIA/EIA/IS-733 2.4.6.2).
    fn decode_gain_and_index(&mut self, gain: &mut [f32; 16]) {
        let mut g1 = [0i32; 16];

        if self.rate >= Rate::Quarter {
            let subframes = match self.rate {
                Rate::Full => 16,
                Rate::Half => 4,
                _ => 5,
            };

            for i in 0..subframes {
                g1[i] = 4 * i32::from(self.frame[CBGAIN + i]);
                if self.rate == Rate::Full && (i + 1) & 3 == 0 {
                    g1[i] += ((g1[i - 1] + g1[i - 2] + g1[i - 3]) / 3 - 6).clamp(0, 32);
                }

                gain[i] = g12ga(g1[i]);

                if self.frame[CBSIGN + i] != 0 {
                    gain[i] = -gain[i];
                    let c = i32::from(self.frame[CINDEX + i]);
                    self.frame[CINDEX + i] = ((c - 89) & 127) as u8;
                }
            }

            self.prev_g1 = [g1[subframes - 2], g1[subframes - 1]];
            self.last_codebook_gain = g12ga(g1[subframes - 1]);

            if self.rate == Rate::Quarter {
                // Provide smoothing of the unvoiced excitation energy.
                let g = |v: f64| v as f32;
                gain[7] = gain[4];
                gain[6] = g(0.4 * f64::from(gain[3]) + 0.6 * f64::from(gain[4]));
                gain[5] = gain[3];
                gain[4] = g(0.8 * f64::from(gain[2]) + 0.2 * f64::from(gain[3]));
                gain[3] = g(0.2 * f64::from(gain[1]) + 0.8 * f64::from(gain[2]));
                gain[2] = gain[1];
                gain[1] = g(0.6 * f64::from(gain[0]) + 0.4 * f64::from(gain[1]));
            }
        }
        else if self.rate != Rate::Silence {
            let subframes = if self.rate == Rate::Octave {
                g1[0] = 2 * i32::from(self.frame[CBGAIN])
                    + ((self.prev_g1[0] + self.prev_g1[1]) / 2 - 5).clamp(0, 54);
                8
            }
            else {
                g1[0] = self.prev_g1[1];
                match self.erasure_count {
                    1 => (),
                    2 => g1[0] -= 1,
                    3 => g1[0] -= 2,
                    _ => g1[0] -= 6,
                }
                if g1[0] < 0 {
                    g1[0] = 0;
                }
                4
            };

            // This interpolation is done to produce smoother background noise.
            let slope =
                (0.5 * f64::from(g12ga(g1[0]) - self.last_codebook_gain) / subframes as f64) as f32;
            for i in 1..=subframes {
                gain[i - 1] = self.last_codebook_gain + slope * i as f32;
            }

            self.last_codebook_gain = gain[subframes - 1];
            self.prev_g1 = [self.prev_g1[1], g1[0]];
        }
    }

    /// Computes the scaled codebook vector Cdn from INDEX and GAIN for all rates
    /// (TIA/EIA/IS-733 2.4.8.1.1).
    fn compute_svector(&mut self, gain: &[f32; 16], cdn: &mut [f32; FRAME_SAMPLES]) {
        match self.rate {
            Rate::Full => {
                for i in 0..16 {
                    let tmp_gain = (f64::from(gain[i]) * 0.01) as f32;
                    let mut cindex = 0u16.wrapping_sub(u16::from(self.frame[CINDEX + i]));
                    for j in 0..10 {
                        cdn[i * 10 + j] =
                            tmp_gain * f32::from(RATE_FULL_CODEBOOK[usize::from(cindex & 127)]);
                        cindex = cindex.wrapping_add(1);
                    }
                }
            }
            Rate::Half => {
                for i in 0..4 {
                    let tmp_gain = (f64::from(gain[i]) * 0.5) as f32;
                    let mut cindex = 0u16.wrapping_sub(u16::from(self.frame[CINDEX + i]));
                    for j in 0..SUBFRAME_SAMPLES {
                        cdn[i * SUBFRAME_SAMPLES + j] =
                            tmp_gain * f32::from(RATE_HALF_CODEBOOK[usize::from(cindex & 127)]);
                        cindex = cindex.wrapping_add(1);
                    }
                }
            }
            Rate::Quarter => {
                let lspv = |i: usize| u16::from(self.frame[LSPV + i]);
                let mut cbseed: u16 = (0x0003 & lspv(4)) << 14
                    | (0x003F & lspv(3)) << 8
                    | (0x0060 & lspv(2)) << 1
                    | (0x0007 & lspv(1)) << 3
                    | (0x0038 & lspv(0)) >> 3;

                // The rnd pointer starts 20 samples into the memory.
                let mem = &mut self.rnd_fir_filter_mem;
                let mut r = 20;
                for i in 0..8 {
                    let tmp_gain = (f64::from(gain[i]) * (QCELP_SQRT1887 / 32768.0)) as f32;
                    for k in 0..20 {
                        cbseed = cbseed.wrapping_mul(521).wrapping_add(259);
                        mem[r] = f32::from(cbseed as i16);

                        // FIR filter.
                        let mut fir = 0.0f32;
                        for j in 0..10 {
                            fir = (f64::from(fir)
                                + RND_FIR_COEFS[j] * f64::from(mem[r - j] + mem[r + j - 20]))
                                as f32;
                        }
                        fir = (f64::from(fir) + RND_FIR_COEFS[10] * f64::from(mem[r - 10])) as f32;

                        cdn[i * 20 + k] = tmp_gain * fir;
                        r += 1;
                    }
                }
                mem.copy_within(160..180, 0);
            }
            Rate::Octave => {
                let mut cbseed = self.first16bits;
                for i in 0..8 {
                    let tmp_gain = (f64::from(gain[i]) * (QCELP_SQRT1887 / 32768.0)) as f32;
                    for j in 0..20 {
                        cbseed = cbseed.wrapping_mul(521).wrapping_add(259);
                        cdn[i * 20 + j] = tmp_gain * f32::from(cbseed as i16);
                    }
                }
            }
            Rate::InsufficientQuality => {
                // The random codebook index.
                let mut cbseed = 0u16.wrapping_sub(44);
                for i in 0..4 {
                    let tmp_gain = (f64::from(gain[i]) * 0.01) as f32;
                    for j in 0..SUBFRAME_SAMPLES {
                        cdn[i * SUBFRAME_SAMPLES + j] =
                            tmp_gain * f32::from(RATE_FULL_CODEBOOK[usize::from(cbseed & 127)]);
                        cbseed = cbseed.wrapping_add(1);
                    }
                }
            }
            Rate::Silence => cdn.fill(0.0),
        }
    }

    /// Applies the pitch synthesis filter and the pitch prefilter to the scaled codebook vector
    /// (TIA/EIA/IS-733 2.4.5.2, 2.4.8.7.2).
    fn apply_pitch_filters(&mut self, cdn: &mut [f32; FRAME_SAMPLES]) {
        if self.rate >= Rate::Half
            || self.rate == Rate::Silence
            || (self.rate == Rate::InsufficientQuality && self.prev_rate >= Rate::Half)
        {
            let mut pfrac = [0u8; 4];

            if self.rate >= Rate::Half {
                // Compute the gain and lag for the whole frame.
                for i in 0..4 {
                    let plag = self.frame[PLAG + i];
                    self.pitch_gain[i] = if plag != 0 {
                        (f64::from(i32::from(self.frame[PGAIN + i]) + 1) * 0.25) as f32
                    }
                    else {
                        0.0
                    };
                    self.pitch_lag[i] = plag.wrapping_add(16);
                    pfrac[i] = self.frame[PFRAC + i];
                }
            }
            else {
                let max_pitch_gain: f32 = if self.rate == Rate::InsufficientQuality {
                    if self.erasure_count < 3 {
                        (0.9 - 0.3 * (f64::from(self.erasure_count) - 1.0)) as f32
                    }
                    else {
                        0.0
                    }
                }
                else {
                    1.0
                };

                for g in self.pitch_gain.iter_mut() {
                    *g = g.min(max_pitch_gain);
                }

                // The fractional lags of the frame are cleared.
                self.frame[PFRAC..PFRAC + 4].fill(0);
            }

            // The pitch synthesis filter.
            do_pitchfilter(
                &mut self.pitch_synthesis_filter_mem,
                cdn,
                &self.pitch_gain,
                &self.pitch_lag,
                &pfrac,
            );

            // The pitch prefilter update.
            for g in self.pitch_gain.iter_mut() {
                *g = (0.5 * f64::from(g.min(1.0))) as f32;
            }

            let synthesis = &self.pitch_synthesis_filter_mem[PITCH_HISTORY..];

            do_pitchfilter(
                &mut self.pitch_pre_filter_mem,
                synthesis,
                &self.pitch_gain,
                &self.pitch_lag,
                &pfrac,
            );

            // Apply the gain control (TIA/EIA/IS-733 2.4.8.3, 2.4.8.6).
            let pre_filtered = &self.pitch_pre_filter_mem[PITCH_HISTORY..];

            for i in (0..FRAME_SAMPLES).step_by(SUBFRAME_SAMPLES) {
                let r = i..i + SUBFRAME_SAMPLES;
                let res = scalar_product(&synthesis[r.clone()], &synthesis[r.clone()]);
                scale_vector_to_given_sum_of_squares(&mut cdn[r.clone()], &pre_filtered[r], res);
            }
        }
        else {
            self.pitch_synthesis_filter_mem[..PITCH_HISTORY].copy_from_slice(&cdn[17..]);
            self.pitch_pre_filter_mem[..PITCH_HISTORY].copy_from_slice(&cdn[17..]);
            self.pitch_gain = [0.0; 4];
            self.pitch_lag = [0; 4];
        }
    }

    /// Interpolates the LSP frequencies and computes the LPC coefficients for a pitch subframe
    /// (TIA/EIA/IS-733 2.4.3.3.4, 2.4.8.7.2).
    fn interpolate_lpc(
        &self,
        curr_lspf: &[f32; LPC_ORDER],
        lpc: &mut [f32; LPC_ORDER],
        subframe: usize,
    ) {
        let weight: f32 = if self.rate >= Rate::Quarter {
            (0.25 * (subframe + 1) as f64) as f32
        }
        else if self.rate == Rate::Octave && subframe == 0 {
            0.625
        }
        else {
            1.0
        };

        if weight != 1.0 {
            let mut interpolated = [0.0f32; LPC_ORDER];
            let weight_b = (1.0 - f64::from(weight)) as f32;
            for i in 0..LPC_ORDER {
                interpolated[i] = weight * curr_lspf[i] + weight_b * self.prev_lspf[i];
            }
            lspf2lpc(&interpolated, lpc);
        }
        else if self.rate >= Rate::Quarter
            || (self.rate == Rate::InsufficientQuality && subframe == 0)
        {
            lspf2lpc(curr_lspf, lpc);
        }
        else if self.rate == Rate::Silence && subframe == 0 {
            lspf2lpc(&self.prev_lspf, lpc);
        }
    }

    /// The postfilter (TIA/EIA/IS-733 2.4.8.6).
    fn postfilter(&mut self, samples: &mut [f32; FRAME_SAMPLES], lpc: &[f32; LPC_ORDER]) {
        let mut lpc_s = [0.0f32; LPC_ORDER];
        let mut lpc_p = [0.0f32; LPC_ORDER];
        let mut pole_out = [0.0f32; 170];
        let mut zero_out = [0.0f32; FRAME_SAMPLES];

        for n in 0..LPC_ORDER {
            lpc_s[n] = lpc[n] * POW_0_625[n];
            lpc_p[n] = lpc[n] * POW_0_775[n];
        }

        // The zero (FIR) filter over the formant synthesis output.
        for n in 0..FRAME_SAMPLES {
            let mut v = self.formant_mem[LPC_ORDER + n];
            for i in 1..=LPC_ORDER {
                v += lpc_s[i - 1] * self.formant_mem[LPC_ORDER + n - i];
            }
            zero_out[n] = v;
        }

        // The pole (IIR) filter.
        pole_out[..LPC_ORDER].copy_from_slice(&self.postfilter_synth_mem);
        lp_synthesis_filter(&mut pole_out, &lpc_p, &zero_out);
        self.postfilter_synth_mem.copy_from_slice(&pole_out[FRAME_SAMPLES..]);

        let filtered = &mut pole_out[LPC_ORDER..];

        // The tilt compensation.
        let tilt = 0.3f32;
        let new_tilt_mem = filtered[FRAME_SAMPLES - 1];
        for i in (1..FRAME_SAMPLES).rev() {
            filtered[i] -= tilt * filtered[i - 1];
        }
        filtered[0] -= tilt * self.postfilter_tilt_mem;
        self.postfilter_tilt_mem = new_tilt_mem;

        // The adaptive gain control.
        let speech_energy = scalar_product(
            &self.formant_mem[LPC_ORDER..LPC_ORDER + FRAME_SAMPLES],
            &self.formant_mem[LPC_ORDER..LPC_ORDER + FRAME_SAMPLES],
        );
        let alpha = 0.9375f32;
        let postfilter_energy = scalar_product(filtered, filtered);
        let mut gain_scale_factor = 1.0f32;
        if postfilter_energy != 0.0 {
            gain_scale_factor = f64::from(speech_energy / postfilter_energy).sqrt() as f32;
        }
        gain_scale_factor = (f64::from(gain_scale_factor) * (1.0 - f64::from(alpha))) as f32;

        let mut mem = self.postfilter_agc_mem;
        for (o, &i) in samples.iter_mut().zip(filtered.iter()) {
            mem = alpha * mem + gain_scale_factor;
            *o = i * mem;
        }
        self.postfilter_agc_mem = mem;
    }

    /// Synthesizes the speech from the excitation, then postfilters it.
    fn synthesize(&mut self, quantized_lspf: &[f32; LPC_ORDER], out: &mut [f32; FRAME_SAMPLES]) {
        let mut lpc = [0.0f32; LPC_ORDER];

        for i in 0..4 {
            self.interpolate_lpc(quantized_lspf, &mut lpc, i);

            let base = LPC_ORDER + i * SUBFRAME_SAMPLES;
            lp_synthesis_filter(
                &mut self.formant_mem[base - LPC_ORDER..base + SUBFRAME_SAMPLES],
                &lpc,
                &out[i * SUBFRAME_SAMPLES..(i + 1) * SUBFRAME_SAMPLES],
            );
        }

        // The postfilter, as per TIA/EIA/IS-733 2.4.8.6.
        self.postfilter(out, &lpc);

        self.formant_mem.copy_within(FRAME_SAMPLES..FRAME_SAMPLES + LPC_ORDER, 0);
        self.prev_lspf = *quantized_lspf;
        self.prev_rate = self.rate;
    }

    /// Unpacks and checks a packet. Returns `Ok(true)` if the packet was decoded to `out`,
    /// `Ok(false)` if the frame must be concealed as erased.
    fn decode_normal(
        &mut self,
        payload: &[u8],
        out: &mut [f32; FRAME_SAMPLES],
        quantized_lspf: &mut [f32; LPC_ORDER],
    ) -> Result<bool> {
        if self.rate == Rate::InsufficientQuality {
            return Ok(false);
        }

        if self.rate == Rate::Octave {
            self.first16bits = u16::from_be_bytes([
                payload.first().copied().unwrap_or(0),
                payload.get(1).copied().unwrap_or(0),
            ]);
            if self.first16bits == 0xffff {
                // Rate 1/8 and the first 16 bits are on.
                return Ok(false);
            }
        }

        if self.rate > Rate::Silence {
            unpack(self.rate, payload, &mut self.frame)?;

            // Check for erasures/blanks on rates 1, 1/4 and 1/8.
            if self.frame[RESERVED] != 0 {
                return Ok(false);
            }
            if self.rate == Rate::Quarter && !codebook_sanity_check_for_rate_quarter(&self.frame) {
                return Ok(false);
            }

            if self.rate >= Rate::Half {
                for i in 0..4 {
                    if self.frame[PFRAC + i] != 0 && self.frame[PLAG + i] >= 124 {
                        // Cannot initialize the pitch filter.
                        return Ok(false);
                    }
                }
            }
        }

        let mut gain = [0.0f32; 16];
        self.decode_gain_and_index(&mut gain);
        self.compute_svector(&gain, out);

        if !self.decode_lspf(quantized_lspf) {
            // Badly received packets in the frame.
            return Ok(false);
        }

        self.apply_pitch_filters(out);

        Ok(true)
    }

    /// Decodes a packet to 160 samples.
    fn decode(&mut self, data: &[u8], out: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        let mut quantized_lspf = [0.0f32; LPC_ORDER];

        let (rate, payload) = determine_rate(data);
        self.rate = rate;

        if !self.decode_normal(payload, out, &mut quantized_lspf)? {
            // Conceal the erased frame.
            let mut gain = [0.0f32; 16];

            self.rate = Rate::InsufficientQuality;
            self.erasure_count = self.erasure_count.wrapping_add(1);
            self.decode_gain_and_index(&mut gain);
            self.compute_svector(&gain, out);
            self.decode_lspf(&mut quantized_lspf);
            self.apply_pitch_filters(out);
        }
        else {
            self.erasure_count = 0;
        }

        self.synthesize(&quantized_lspf, out);

        Ok(())
    }
}

/// The table of decoded linear codebook gain magnitudes. The index is always in range for a
/// successfully unpacked frame; it is clamped to be safe.
fn g12ga(index: i32) -> f32 {
    G12GA_F32[index.clamp(0, 60) as usize]
}

/// If the received packet is rate 1/4, a further sanity check is made of the codebook gain
/// (TIA/EIA/IS-733 2.4.8.7.3). Returns `false` if the check fails.
fn codebook_sanity_check_for_rate_quarter(frame: &[u8; FRAME_LEN]) -> bool {
    let cbgain = &frame[CBGAIN..CBGAIN + 5];
    let mut prev_diff = 0i32;

    for i in 1..5 {
        let diff = i32::from(cbgain[i]) - i32::from(cbgain[i - 1]);
        if diff.abs() > 10 || (diff - prev_diff).abs() > 12 {
            return false;
        }
        prev_diff = diff;
    }

    true
}

/// An all-pole linear prediction synthesis filter, `out[n] = in[n] - sum(filter[i - 1] * out[n -
/// i])`. The first `LPC_ORDER` samples of `out` are the filter memory and `out` is 10 samples
/// longer than `input`.
fn lp_synthesis_filter(out: &mut [f32], filter: &[f32; LPC_ORDER], input: &[f32]) {
    for (n, &x) in input.iter().enumerate() {
        let mut v = x;
        for i in 1..=LPC_ORDER {
            v -= filter[i - 1] * out[LPC_ORDER + n - i];
        }
        out[LPC_ORDER + n] = v;
    }
}

/// QCELP decoder.
pub struct QcelpDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    core: Box<Core>,
}

impl QcelpDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_QCELP {
            return unsupported_error("qcelp: invalid codec");
        }

        // QCELP is 8 kHz mono. The sample rate is often missing from the container.
        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            _ => 8000,
        };

        if let Some(channels) = &params.channels {
            if channels.count() != 1 {
                return unsupported_error("qcelp: only mono is supported");
            }
        }

        let spec = AudioSpec::new(rate, layouts::CHANNEL_LAYOUT_MONO);
        let buf = AudioBuffer::new(spec, FRAME_SAMPLES);

        let mut params = params.clone();
        params.with_sample_rate(rate).with_channels(layouts::CHANNEL_LAYOUT_MONO);

        Ok(QcelpDecoder { params, opts: *opts, buf, core: Box::new(Core::new()) })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();
        self.buf.render_uninit(Some(FRAME_SAMPLES));

        let Some(plane) = self.buf.plane_mut(0)
        else {
            return decode_error("qcelp: no output plane");
        };
        let Ok(out) = <&mut [f32; FRAME_SAMPLES]>::try_from(plane)
        else {
            return decode_error("qcelp: bad output plane");
        };

        self.core.decode(packet.data, out)?;

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for QcelpDecoder {
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

impl RegisterableAudioDecoder for QcelpDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(QcelpDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_QCELP, "qcelp", "QCELP / PureVoice")]
    }
}
