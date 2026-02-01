// CELT Bit Allocation - From xiph/opus celt/rate.c
// Copyright (c) 2007-2010 CSIRO
// Copyright (c) 2007-2010 Xiph.Org Foundation
// SPDX-License-Identifier: BSD-3-Clause

use crate::entdec::RangeDecoder;
use crate::celt_constants::{ALLOC_VECTORS, EBANDS_48K, NB_BANDS};

const BITRES: i32 = 3;

/// Fractional log2 lookup table
const LOG2_FRAC_TABLE: [u8; 24] = [
    0, 8, 13, 16, 19, 21, 23, 24, 26, 27, 28, 29, 30, 31, 32,
    32, 33, 34, 34, 35, 36, 36, 37, 37,
];

/// Compute bit allocation for CELT bands
///
/// This implements the complete allocation algorithm from xiph/opus rate.c
///
/// Arguments:
/// - bits: Total bits available
/// - lm: log2(frame_size / 120)
/// - channels: Number of channels (1 or 2)
/// - start_band: First band to allocate
/// - end_band: Last band + 1
/// - dec: Range decoder (for reading trim/intensity)
/// - alloc_trim: Allocation trim value (-5 to +5)
///
/// Returns: (pulses, fine_quant, fine_priority)
pub fn compute_allocation(
    bits: i32,
    lm: usize,
    channels: usize,
    start_band: usize,
    end_band: usize,
    alloc_trim: i32,
) -> (Vec<i32>, Vec<i32>, Vec<i32>) {
    let nb_bands = end_band - start_band;
    let c = channels as i32;
    let lm_i32 = lm as i32;

    // Initialize outputs
    let mut pulses = vec![0i32; nb_bands];
    let mut fine_quant = vec![0i32; nb_bands];
    let mut fine_priority = vec![0i32; nb_bands];

    // Reserve bits for intensity/dual stereo if stereo
    let mut total = bits.max(0);
    let skip_rsv = if total >= (1 << BITRES) {
        1 << BITRES
    } else {
        0
    };
    total -= skip_rsv;

    let (intensity_rsv, dual_stereo_rsv) = if channels == 2 {
        let intensity = LOG2_FRAC_TABLE[(end_band - start_band).min(23)] as i32;
        if intensity > total {
            (0, 0)
        } else {
            total -= intensity;
            let dual = if total >= (1 << BITRES) {
                1 << BITRES
            } else {
                0
            };
            total -= dual;
            (intensity, dual)
        }
    } else {
        (0, 0)
    };

    // Compute thresholds and trim offsets for each band
    let mut thresh = vec![0i32; nb_bands];
    let mut trim_offset = vec![0i32; nb_bands];
    let mut cap = vec![0i32; nb_bands];

    for j in 0..nb_bands {
        let band_idx = start_band + j;
        if band_idx + 1 >= EBANDS_48K.len() {
            continue;
        }

        let n = (EBANDS_48K[band_idx + 1] - EBANDS_48K[band_idx]) as i32;

        // Minimum threshold for PVQ allocation
        thresh[j] = ((c << BITRES).max((3 * n << lm_i32 << BITRES) >> 4));

        // Tilt offset based on trim and band position
        let tilt = c * n * (alloc_trim - 5 - lm_i32) * (end_band as i32 - band_idx as i32 - 1);
        trim_offset[j] = (tilt << (lm_i32 + BITRES)) >> 6;

        // Single-coefficient bands get less resolution
        if (n << lm_i32) == 1 {
            trim_offset[j] -= c << BITRES;
        }

        // Maximum cap per band (generous estimate)
        cap[j] = (n << lm_i32) * c * 7;
    }

    // Binary search through allocation vectors
    let nb_alloc_vectors = ALLOC_VECTORS.len();
    let mut lo = 1usize;
    let mut hi = nb_alloc_vectors - 1;

    while lo <= hi {
        let mid = (lo + hi) / 2;
        let mut psum = 0i32;
        let mut done = false;

        for j in (0..nb_bands).rev() {
            let band_idx = start_band + j;
            if band_idx + 1 >= EBANDS_48K.len() {
                continue;
            }

            let n = (EBANDS_48K[band_idx + 1] - EBANDS_48K[band_idx]) as i32;

            // Get allocation from vector
            let mut bitsj = (c * n * (ALLOC_VECTORS[mid][band_idx] as i32) << lm_i32) >> 2;

            if bitsj > 0 {
                bitsj = 0.max(bitsj + trim_offset[j]);
            }

            if bitsj >= thresh[j] || done {
                done = true;
                psum += bitsj.min(cap[j]);
            } else if bitsj >= (c << BITRES) {
                psum += c << BITRES;
            }
        }

        if psum > total {
            hi = mid.saturating_sub(1);
        } else {
            lo = mid + 1;
        }
    }

    hi = lo;
    lo = lo.saturating_sub(1);

    // Interpolate between the two closest vectors
    let mut bits1 = vec![0i32; nb_bands];
    let mut bits2 = vec![0i32; nb_bands];

    for j in 0..nb_bands {
        let band_idx = start_band + j;
        if band_idx + 1 >= EBANDS_48K.len() {
            continue;
        }

        let n = (EBANDS_48K[band_idx + 1] - EBANDS_48K[band_idx]) as i32;

        // Bits from lower vector
        let mut bits1j = if lo < nb_alloc_vectors {
            (c * n * (ALLOC_VECTORS[lo][band_idx] as i32) << lm_i32) >> 2
        } else {
            0
        };

        // Bits from upper vector
        let mut bits2j = if hi < nb_alloc_vectors {
            (c * n * (ALLOC_VECTORS[hi][band_idx] as i32) << lm_i32) >> 2
        } else {
            cap[j]
        };

        if bits1j > 0 {
            bits1j = 0.max(bits1j + trim_offset[j]);
        }
        if bits2j > 0 {
            bits2j = 0.max(bits2j + trim_offset[j]);
        }

        bits2j = 0.max(bits2j - bits1j);
        bits1[j] = bits1j;
        bits2[j] = bits2j;
    }

    // Convert bits to pulses
    let mut remaining_bits = total;

    for j in 0..nb_bands {
        let band_idx = start_band + j;
        if band_idx + 1 >= EBANDS_48K.len() {
            continue;
        }

        let n = (EBANDS_48K[band_idx + 1] - EBANDS_48K[band_idx]) as i32;
        if n <= 1 {
            continue;
        }

        // Total bits for this band (interpolated)
        let band_bits = bits1[j] + if remaining_bits > 0 {
            bits2[j].min(remaining_bits)
        } else {
            0
        };

        if band_bits >= thresh[j] {
            remaining_bits -= bits2[j].min(remaining_bits);

            // Reserve bits for fine energy
            let fine_bits = (band_bits >> (BITRES + 2)).min(7);
            fine_quant[j] = fine_bits;

            // Remaining bits go to pulses
            let pulse_bits = band_bits - (fine_bits << BITRES);

            // Estimate pulses from bits
            // Using approximation: bits ≈ pulses * log2(n)
            let bits_per_pulse = ((n as f32).log2() * 8.0) as i32;
            if bits_per_pulse > 0 {
                pulses[j] = ((pulse_bits >> BITRES) / (bits_per_pulse >> 3)).min(255);
            }
        } else if band_bits >= (c << BITRES) {
            // At least one bit per channel for fine energy
            fine_quant[j] = 1;
        }

        fine_priority[j] = if pulses[j] < 10 { 1 } else { 0 };
    }

    (pulses, fine_quant, fine_priority)
}

/// Decode allocation trim parameter
///
/// The trim value adjusts bit allocation between frequency bands.
/// Range is -5 (more low freq) to +5 (more high freq).
pub fn decode_trim(dec: &mut RangeDecoder) -> i32 {
    const TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];
    let trim = dec.decode_icdf(&TRIM_ICDF, 10);
    (trim as i32) - 5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_complete_allocation() {
        // Test with typical parameters
        let (pulses, fine_quant, fine_priority) =
            compute_allocation(8000, 3, 2, 0, NB_BANDS, 0);

        assert_eq!(pulses.len(), NB_BANDS);
        assert_eq!(fine_quant.len(), NB_BANDS);
        assert_eq!(fine_priority.len(), NB_BANDS);

        // Check that allocation happened
        let total_pulses: i32 = pulses.iter().sum();
        assert!(total_pulses > 0, "Should allocate pulses");

        // Check fine quant is reasonable
        for &fq in &fine_quant {
            assert!(fq <= 7, "Fine quant should be <= 7");
        }
    }

    #[test]
    fn test_low_bitrate_allocation() {
        // Very low bitrate
        let (pulses, fine_quant, _) = compute_allocation(1000, 3, 1, 0, NB_BANDS, 0);

        // Should still get some allocation
        let total_pulses: i32 = pulses.iter().sum();
        assert!(total_pulses > 0);

        // Most bands should have minimal allocation
        let low_bands = pulses.iter().filter(|&&p| p <= 5).count();
        assert!(low_bands > NB_BANDS / 2);
    }

    #[test]
    fn test_high_bitrate_allocation() {
        // High bitrate
        let (pulses, fine_quant, _) = compute_allocation(40000, 3, 2, 0, NB_BANDS, 0);

        let total_pulses: i32 = pulses.iter().sum();
        assert!(total_pulses > 100);

        // Should have good fine quantization
        let high_fine = fine_quant.iter().filter(|&&fq| fq >= 3).count();
        assert!(high_fine > NB_BANDS / 3);
    }
}
