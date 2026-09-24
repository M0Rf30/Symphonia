// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Band energy quantization (decode side). Ported from libopus `celt/quant_bands.c`
//! (`unquant_coarse_energy`, `unquant_fine_energy`, `unquant_energy_finalise`, `amp2Log2`).
//! Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltBitstream".

use crate::celt::laplace::ec_laplace_decode;
use crate::celt::modes::{celt_log2, CeltMode};
use crate::range::RangeDecoder;

/// C: `eMeans` (`celt/quant_bands.c`, float build). Mean energy in each band, `pub(crate)` so
/// `bands.rs`'s `denormalise_bands` can use it too (matches `bands.c`'s
/// `#include "quant_bands.h"`).
#[rustfmt::skip]
pub(crate) static E_MEANS: [f32; 25] = [
    6.437500, 6.250000, 5.750000, 5.312500, 5.062500,
    4.812500, 4.500000, 4.375000, 4.875000, 4.687500,
    4.562500, 4.437500, 4.875000, 4.625000, 4.312500,
    4.500000, 4.375000, 4.625000, 4.750000, 4.437500,
    3.750000, 3.750000, 3.750000, 3.750000, 3.750000,
];

/// C: `pred_coef` (`celt/quant_bands.c`, float build). Prediction coefficients: 0.9, 0.8, 0.65,
/// 0.5.
static PRED_COEF: [f32; 4] = [29440.0 / 32768.0, 26112.0 / 32768.0, 21248.0 / 32768.0, 16384.0 / 32768.0];

/// C: `beta_coef` (`celt/quant_bands.c`, float build).
static BETA_COEF: [f32; 4] = [30147.0 / 32768.0, 22282.0 / 32768.0, 12124.0 / 32768.0, 6554.0 / 32768.0];

/// C: `beta_intra` (`celt/quant_bands.c`, float build).
const BETA_INTRA: f32 = 4915.0 / 32768.0;

/// C: `e_prob_model[4][2][42]` (`celt/quant_bands.c`). Parameters of the Laplace-like
/// probability models used for the coarse energy: one pair (p0, decay), in Q8, per frame size
/// (indexed `[LM]`), prediction type (`[intra]`), and band number.
#[rustfmt::skip]
static E_PROB_MODEL: [[[u8; 42]; 2]; 4] = [
    // 120 sample frames
    [
        // Inter
        [
            72, 127, 65, 129, 66, 128, 65, 128, 64, 128, 62, 128, 64, 128,
            64, 128, 92, 78, 92, 79, 92, 78, 90, 79, 116, 41, 115, 40,
            114, 40, 132, 26, 132, 26, 145, 17, 161, 12, 176, 10, 177, 11,
        ],
        // Intra
        [
            24, 179, 48, 138, 54, 135, 54, 132, 53, 134, 56, 133, 55, 132,
            55, 132, 61, 114, 70, 96, 74, 88, 75, 88, 87, 74, 89, 66,
            91, 67, 100, 59, 108, 50, 120, 40, 122, 37, 97, 43, 78, 50,
        ],
    ],
    // 240 sample frames
    [
        // Inter
        [
            83, 78, 84, 81, 88, 75, 86, 74, 87, 71, 90, 73, 93, 74,
            93, 74, 109, 40, 114, 36, 117, 34, 117, 34, 143, 17, 145, 18,
            146, 19, 162, 12, 165, 10, 178, 7, 189, 6, 190, 8, 177, 9,
        ],
        // Intra
        [
            23, 178, 54, 115, 63, 102, 66, 98, 69, 99, 74, 89, 71, 91,
            73, 91, 78, 89, 86, 80, 92, 66, 93, 64, 102, 59, 103, 60,
            104, 60, 117, 52, 123, 44, 138, 35, 133, 31, 97, 38, 77, 45,
        ],
    ],
    // 480 sample frames
    [
        // Inter
        [
            61, 90, 93, 60, 105, 42, 107, 41, 110, 45, 116, 38, 113, 38,
            112, 38, 124, 26, 132, 27, 136, 19, 140, 20, 155, 14, 159, 16,
            158, 18, 170, 13, 177, 10, 187, 8, 192, 6, 175, 9, 159, 10,
        ],
        // Intra
        [
            21, 178, 59, 110, 71, 86, 75, 85, 84, 83, 91, 66, 88, 73,
            87, 72, 92, 75, 98, 72, 105, 58, 107, 54, 115, 52, 114, 55,
            112, 56, 129, 51, 132, 40, 150, 33, 140, 29, 98, 35, 77, 42,
        ],
    ],
    // 960 sample frames
    [
        // Inter
        [
            42, 121, 96, 66, 108, 43, 111, 40, 117, 44, 123, 32, 120, 36,
            119, 33, 127, 33, 134, 34, 139, 21, 147, 23, 152, 20, 158, 25,
            154, 26, 166, 21, 173, 16, 184, 13, 184, 10, 150, 13, 139, 15,
        ],
        // Intra
        [
            22, 178, 63, 114, 74, 82, 84, 83, 92, 82, 103, 62, 96, 72,
            96, 67, 101, 73, 107, 72, 113, 55, 118, 52, 125, 52, 118, 52,
            117, 55, 135, 49, 137, 39, 157, 32, 145, 29, 97, 33, 77, 40,
        ],
    ],
];

/// C: `small_energy_icdf` (`celt/quant_bands.c`).
static SMALL_ENERGY_ICDF: [u8; 3] = [2, 1, 0];

/// Maximum number of fine-energy bits per band per channel. C: `MAX_FINE_BITS` (`celt/rate.h`).
const MAX_FINE_BITS: i32 = 8;

/// C: `unquant_coarse_energy`. Decodes coarse per-band log-energy into `old_e_bands`
/// (length `channels * mode.nb_ebands`, dB-scaled `Q8` fixed-point-equivalent as `f32`).
///
/// Deviates from the wave-0 stub by adding `budget: i32` (bits): C derives this internally
/// from `dec->storage*8` (the byte length of the buffer the `ec_dec` was initialized with),
/// which this crate's [`RangeDecoder`] does not expose publicly; the caller (owner of the
/// `RangeDecoder::new(data)` call) already has `data.len()` and should pass `data.len() as i32
/// * 8`.
#[allow(clippy::too_many_arguments)]
pub fn unquant_coarse_energy(
    mode: &CeltMode,
    start: i32,
    end: i32,
    old_e_bands: &mut [f32],
    intra: bool,
    rd: &mut RangeDecoder<'_>,
    channels: i32,
    lm: i32,
    budget: i32,
) {
    let prob_model = &E_PROB_MODEL[lm as usize][intra as usize];
    let (coef, beta) = if intra { (0.0, BETA_INTRA) } else { (PRED_COEF[lm as usize], BETA_COEF[lm as usize]) };
    let nb_ebands = mode.nb_ebands;
    let mut prev = [0.0f32; 2];

    for i in start..end {
        for c in 0..channels {
            let tell = rd.tell();
            let qi: i32;
            if budget - tell >= 15 {
                let pi = (2 * i.min(20)) as usize;
                qi = ec_laplace_decode(rd, (prob_model[pi] as u32) << 7, (prob_model[pi + 1] as u32) << 6);
            }
            else if budget - tell >= 2 {
                let q = rd.dec_icdf(&SMALL_ENERGY_ICDF, 2);
                qi = (q >> 1) ^ -(q & 1);
            }
            else if budget - tell >= 1 {
                qi = -(rd.dec_bit_logp(1) as i32);
            }
            else {
                qi = -1;
            }
            let q = qi as f32;

            let idx = (i + c * nb_ebands) as usize;
            old_e_bands[idx] = old_e_bands[idx].max(-9.0);
            let tmp = coef * old_e_bands[idx] + prev[c as usize] + q;
            old_e_bands[idx] = tmp;
            prev[c as usize] = prev[c as usize] + q - beta * q;
        }
    }
}

/// C: `unquant_fine_energy`.
pub fn unquant_fine_energy(
    mode: &CeltMode,
    start: i32,
    end: i32,
    old_e_bands: &mut [f32],
    fine_quant: &[i32],
    rd: &mut RangeDecoder<'_>,
    channels: i32,
) {
    let nb_ebands = mode.nb_ebands;
    for i in start..end {
        let fq = fine_quant[i as usize];
        if fq <= 0 {
            continue;
        }
        for c in 0..channels {
            let q2 = rd.dec_bits(fq as u32) as i32;
            let offset = (q2 as f32 + 0.5) * ((1i32 << (14 - fq)) as f32) * (1.0 / 16384.0) - 0.5;
            old_e_bands[(i + c * nb_ebands) as usize] += offset;
        }
    }
}

/// C: `unquant_energy_finalise`.
#[allow(clippy::too_many_arguments)]
pub fn unquant_energy_finalise(
    mode: &CeltMode,
    start: i32,
    end: i32,
    old_e_bands: &mut [f32],
    fine_quant: &[i32],
    fine_priority: &[i32],
    bits_left: i32,
    rd: &mut RangeDecoder<'_>,
    channels: i32,
) {
    let nb_ebands = mode.nb_ebands;
    let mut bits_left = bits_left;
    for prio in 0..2 {
        let mut i = start;
        while i < end && bits_left >= channels {
            if fine_quant[i as usize] >= MAX_FINE_BITS || fine_priority[i as usize] != prio {
                i += 1;
                continue;
            }
            for c in 0..channels {
                let q2 = rd.dec_bits(1) as i32;
                let offset =
                    (q2 as f32 - 0.5) * ((1i32 << (14 - fine_quant[i as usize] - 1)) as f32) * (1.0 / 16384.0);
                old_e_bands[(i + c * nb_ebands) as usize] += offset;
                bits_left -= 1;
            }
            i += 1;
        }
    }
}

/// C: `amp2Log2` (used indirectly by anti-collapse and denormalisation on the decode side).
pub fn amp2_log2(mode: &CeltMode, effective_end: i32, end: i32, band_e: &[f32], band_log_e: &mut [f32], channels: i32) {
    let nb_ebands = mode.nb_ebands;
    for c in 0..channels {
        for i in 0..effective_end {
            band_log_e[(i + c * nb_ebands) as usize] =
                celt_log2(band_e[(i + c * nb_ebands) as usize]) - E_MEANS[i as usize];
        }
        for i in effective_end..end {
            band_log_e[(c * nb_ebands + i) as usize] = -14.0;
        }
    }
}
