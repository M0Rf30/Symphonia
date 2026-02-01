// Band Energy Quantization - Rewritten from xiph/opus celt/quant_bands.c
// Copyright (c) 2007-2008 CSIRO
// Copyright (c) 2007-2009 Xiph.Org Foundation
// SPDX-License-Identifier: BSD-3-Clause

use crate::entdec::RangeDecoder;
use crate::laplace::ec_laplace_decode;

/// Mean energy in each band quantized in Q4 and converted back to float
/// From static_modes_float.h - these are perceptually-weighted average energies
pub const E_MEANS: [f32; 25] = [
    6.437500, 6.250000, 5.750000, 5.312500, 5.062500,
    4.812500, 4.500000, 4.375000, 4.875000, 4.687500,
    4.562500, 4.437500, 4.875000, 4.625000, 4.312500,
    4.500000, 4.375000, 4.625000, 4.750000, 4.437500,
    3.750000, 3.750000, 3.750000, 3.750000, 3.750000,
];

/// Prediction coefficients: 0.9, 0.8, 0.65, 0.5
const PRED_COEF: [f32; 4] = [
    29440.0 / 32768.0,
    26112.0 / 32768.0,
    21248.0 / 32768.0,
    16384.0 / 32768.0,
];

/// Beta coefficients for temporal prediction
const BETA_COEF: [f32; 4] = [
    30147.0 / 32768.0,
    22282.0 / 32768.0,
    12124.0 / 32768.0,
    6554.0 / 32768.0,
];

/// Beta for intra-frame (no temporal prediction)
const BETA_INTRA: f32 = 4915.0 / 32768.0;

/// Parameters of the Laplace-like probability models used for coarse energy
/// There is one pair of parameters for each frame size, prediction type
/// (inter/intra), and band number.
/// The first number of each pair is the probability of 0, and the second is the
/// decay rate, both in Q8 precision.
const E_PROB_MODEL: [[[[u8; 2]; 21]; 2]; 4] = [
    // 120 sample frames
    [
        // Inter
        [
            [72, 127], [65, 129], [66, 128], [65, 128], [64, 128],
            [62, 128], [64, 128], [64, 128], [92, 78], [92, 79],
            [92, 78], [90, 79], [116, 41], [115, 40], [114, 40],
            [132, 26], [132, 26], [145, 17], [161, 12], [176, 10],
            [177, 11],
        ],
        // Intra
        [
            [24, 179], [48, 138], [54, 135], [54, 132], [53, 134],
            [56, 133], [55, 132], [55, 132], [61, 114], [70, 96],
            [74, 88], [75, 88], [87, 74], [89, 66], [91, 67],
            [100, 59], [108, 50], [120, 40], [122, 37], [97, 43],
            [78, 50],
        ],
    ],
    // 240 sample frames
    [
        // Inter
        [
            [83, 78], [84, 81], [88, 75], [86, 74], [87, 71],
            [90, 73], [93, 74], [93, 74], [109, 40], [114, 36],
            [117, 34], [117, 34], [143, 17], [145, 18], [146, 19],
            [162, 12], [165, 10], [178, 7], [189, 6], [190, 8],
            [177, 9],
        ],
        // Intra
        [
            [23, 178], [54, 115], [63, 102], [66, 98], [69, 99],
            [74, 89], [71, 91], [73, 91], [78, 89], [86, 80],
            [92, 66], [93, 64], [102, 59], [103, 60], [104, 60],
            [117, 52], [123, 44], [138, 35], [133, 31], [97, 38],
            [77, 45],
        ],
    ],
    // 480 sample frames
    [
        // Inter
        [
            [61, 90], [93, 60], [105, 42], [107, 41], [110, 45],
            [116, 38], [113, 38], [112, 38], [124, 26], [132, 27],
            [136, 19], [140, 20], [155, 14], [159, 16], [158, 18],
            [170, 13], [177, 10], [187, 8], [192, 6], [175, 9],
            [159, 10],
        ],
        // Intra
        [
            [21, 178], [59, 110], [71, 86], [75, 85], [84, 83],
            [91, 66], [88, 73], [87, 72], [92, 75], [98, 72],
            [105, 58], [107, 54], [115, 52], [114, 55], [112, 56],
            [129, 51], [132, 40], [150, 33], [140, 29], [98, 35],
            [77, 42],
        ],
    ],
    // 960 sample frames
    [
        // Inter
        [
            [42, 121], [96, 66], [108, 43], [111, 40], [117, 44],
            [123, 32], [120, 36], [119, 33], [127, 33], [134, 34],
            [139, 21], [147, 23], [152, 20], [158, 25], [154, 26],
            [166, 21], [173, 16], [184, 13], [184, 10], [150, 13],
            [139, 15],
        ],
        // Intra
        [
            [22, 178], [63, 114], [74, 82], [84, 83], [92, 82],
            [103, 62], [96, 72], [96, 67], [101, 73], [107, 72],
            [113, 55], [118, 52], [125, 52], [118, 52], [117, 55],
            [135, 49], [137, 39], [157, 32], [145, 29], [97, 33],
            [77, 40],
        ],
    ],
];

/// Small energy ICDF for very low bit budgets
const SMALL_ENERGY_ICDF: [u8; 3] = [2, 1, 0];

/// Decode coarse energy values
///
/// This decodes the coarse (low-resolution) energy for each band using
/// temporal prediction and Laplace coding.
///
/// Arguments:
/// - start: First band to decode
/// - end: Last band to decode + 1
/// - old_bands: Previous frame energies (in/out)
/// - intra: Whether this is an intra frame (no temporal prediction)
/// - dec: Range decoder
/// - channels: Number of audio channels (1 or 2)
/// - lm: log2(frame_size / 120) - 0 for 120 samples, 3 for 960 samples
/// - nb_bands: Total number of bands
pub fn unquant_coarse_energy(
    start: usize,
    end: usize,
    old_bands: &mut [f32],
    intra: bool,
    dec: &mut RangeDecoder,
    channels: usize,
    lm: usize,
    nb_bands: usize,
) {
    let prob_model = &E_PROB_MODEL[lm][if intra { 1 } else { 0 }];
    let mut prev = [0.0f32, 0.0f32];

    let (coef, beta) = if intra {
        (0.0, BETA_INTRA)
    } else {
        (PRED_COEF[lm], BETA_COEF[lm])
    };

    let budget = dec.bits_left();

    // Decode at a fixed coarse resolution
    for i in start..end {
        for c in 0..channels {
            let tell = dec.tell_frac();

            let qi = if budget >= tell + 15 {
                // Use full Laplace decoder
                let pi = (i.min(20)) * 2;
                ec_laplace_decode(
                    dec,
                    (prob_model[pi][0] as u32) << 7,
                    (prob_model[pi][1] as i32) << 6,
                )
            } else if budget >= tell + 2 {
                // Use small energy ICDF
                let val = dec.decode_icdf(&SMALL_ENERGY_ICDF, 2);
                ((val >> 1) as i32) ^ -((val & 1) as i32)
            } else if budget >= tell + 1 {
                // Single bit
                -(dec.decode_bit_logp(1) as i32)
            } else {
                // No bits left
                -1
            };

            let q = (qi as f32) * 0.0625; // qi << DB_SHIFT, DB_SHIFT = 4 in float

            let old_e = old_bands[i + c * nb_bands].max(-9.0);
            let tmp = (coef * old_e + prev[c] + q).clamp(-28.0, 28.0);

            old_bands[i + c * nb_bands] = tmp;
            prev[c] = prev[c] + q - beta * q;
        }
    }
}

/// Decode fine energy values
///
/// This adds fine (high-resolution) adjustments to the coarse energy values.
///
/// Arguments:
/// - start: First band to decode
/// - end: Last band to decode + 1
/// - old_bands: Energy values (in/out)
/// - fine_quant: Number of fine bits per band
/// - dec: Range decoder
/// - channels: Number of audio channels
/// - nb_bands: Total number of bands
pub fn unquant_fine_energy(
    start: usize,
    end: usize,
    old_bands: &mut [f32],
    fine_quant: &[i32],
    dec: &mut RangeDecoder,
    channels: usize,
    nb_bands: usize,
) {
    // Decode finer resolution
    for i in start..end {
        if fine_quant[i] <= 0 {
            continue;
        }

        for c in 0..channels {
            // Read fine_quant[i] bits
            let q2 = dec.decode_bits(fine_quant[i] as u32);

            // Convert to offset in dB
            let offset = ((q2 as f32 + 0.5) / (1 << fine_quant[i]) as f32) - 0.5;

            old_bands[i + c * nb_bands] += offset;
        }
    }
}

/// Finalize energy decoding
///
/// This uses any remaining bits to add final adjustments to energy values.
///
/// Arguments:
/// - start: First band
/// - end: Last band + 1
/// - old_bands: Energy values (in/out)
/// - fine_quant: Number of fine bits per band
/// - fine_priority: Priority for using remaining bits
/// - bits_left: Number of bits remaining
/// - dec: Range decoder
/// - channels: Number of channels
/// - nb_bands: Total number of bands
pub fn unquant_energy_finalise(
    start: usize,
    end: usize,
    old_bands: &mut [f32],
    fine_quant: &[i32],
    fine_priority: &[i32],
    mut bits_left: i32,
    dec: &mut RangeDecoder,
    channels: usize,
    nb_bands: usize,
) {
    const MAX_FINE_BITS: i32 = 8;

    // Use up the remaining bits
    for prio in 0..2 {
        for i in start..end {
            if bits_left < channels as i32 {
                break;
            }

            if fine_quant[i] >= MAX_FINE_BITS || fine_priority[i] != prio {
                continue;
            }

            for c in 0..channels {
                let q2 = dec.decode_bits(1);
                let offset = ((q2 as f32) - 0.5) / (1 << (fine_quant[i] + 1)) as f32;

                old_bands[i + c * nb_bands] += offset;
                bits_left -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constants() {
        assert_eq!(E_MEANS.len(), 25);
        assert_eq!(PRED_COEF.len(), 4);
        assert_eq!(BETA_COEF.len(), 4);

        // Check that prediction coefficients are in valid range
        for &coef in &PRED_COEF {
            assert!(coef > 0.0 && coef < 1.0);
        }
    }

    #[test]
    fn test_prob_model_dimensions() {
        // 4 frame sizes, 2 prediction types (inter/intra), 21 bands, 2 params each
        assert_eq!(E_PROB_MODEL.len(), 4);
        assert_eq!(E_PROB_MODEL[0].len(), 2);
        assert_eq!(E_PROB_MODEL[0][0].len(), 21);
        assert_eq!(E_PROB_MODEL[0][0][0].len(), 2);
    }
}
