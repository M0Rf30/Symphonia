// Band Processing and Synthesis - Rewritten from xiph/opus celt/bands.c
// Copyright (c) 2007-2008 CSIRO
// Copyright (c) 2007-2009 Xiph.Org Foundation
// Copyright (c) 2008-2009 Gregory Maxwell
// SPDX-License-Identifier: BSD-3-Clause

use crate::celt_constants::EBANDS_48K;
use crate::quant_bands::E_MEANS;

/// Linear congruential generator for random number generation
/// Used for anti-collapse noise injection
#[inline]
pub fn celt_lcg_rand(seed: u32) -> u32 {
    seed.wrapping_mul(1664525).wrapping_add(1013904223)
}

/// Convert from log energy to linear amplitude
///
/// Converts logarithmic energy values (in dB) to linear gain values.
/// Uses the formula: amplitude = 2^(energy/4)
///
/// Arguments:
/// - log_energy: Energy in dB * 16 (Q4 format)
#[inline]
fn exp2_db(log_energy: f32) -> f32 {
    // DB_SHIFT = 4, so divide by 4 to get dB
    2.0f32.powf(log_energy / 4.0)
}

/// Denormalize bands to restore full amplitude
///
/// Takes unit-normalized frequency domain coefficients and applies the
/// decoded energy values to restore full amplitude.
///
/// Arguments:
/// - x: Normalized input coefficients
/// - freq: Output frequency-domain signal
/// - band_log_e: Log energy for each band
/// - ebands: Band boundaries
/// - start: First band to process
/// - end: Last band to process + 1
/// - m: Multiplier for band boundaries (related to frame size)
pub fn denormalise_bands(
    x: &[f32],
    freq: &mut [f32],
    band_log_e: &[f32],
    ebands: &[i16],
    start: usize,
    end: usize,
    m: usize,
) {
    let n = m * 120; // shortMdctSize = 120 for standard mode
    let bound = m * ebands[end] as usize;

    // Zero out bins before start
    if start != 0 {
        for i in 0..(m * ebands[start] as usize) {
            freq[i] = 0.0;
        }
    }

    let mut x_idx = m * ebands[start] as usize;
    let mut f_idx = m * ebands[start] as usize;

    // Process each band
    for i in start..end {
        let band_start = m * ebands[i] as usize;
        let band_end = m * ebands[i + 1] as usize;

        // Compute gain from log energy
        // Add mean energy and convert from log to linear
        let lg = band_log_e[i] + E_MEANS[i];
        let g = exp2_db(lg.min(32.0));

        // Apply gain to normalized coefficients
        for _j in band_start..band_end {
            freq[f_idx] = x[x_idx] * g;
            x_idx += 1;
            f_idx += 1;
        }
    }

    // Zero out remaining bins
    for i in bound..n {
        freq[i] = 0.0;
    }
}

/// Anti-collapse processing
///
/// Prevents energy collapse for transients with multiple short MDCTs by
/// injecting noise into bands that lost significant energy.
///
/// Arguments:
/// - x: Frequency coefficients (modified in place)
/// - collapse_masks: Mask indicating which bands need noise injection
/// - lm: log2(frame_size / 120)
/// - channels: Number of channels
/// - start: First band
/// - end: Last band + 1
/// - log_e: Current log energies
/// - prev1_log_e: Previous frame log energies
/// - prev2_log_e: Two frames ago log energies
/// - pulses: Number of pulses allocated per band
/// - seed: Random seed for noise generation
/// - nb_bands: Total number of bands
pub fn anti_collapse(
    x: &mut [f32],
    collapse_masks: &[u8],
    lm: usize,
    channels: usize,
    start: usize,
    end: usize,
    log_e: &[f32],
    prev1_log_e: &[f32],
    prev2_log_e: &[f32],
    pulses: &[i32],
    mut seed: u32,
    nb_bands: usize,
) {
    let ebands = &EBANDS_48K;

    for i in start..end {
        let n0 = (ebands[i + 1] - ebands[i]) as usize;

        // Compute depth (bit allocation density) in 1/8 bits
        let depth = ((1 + pulses[i]) / (n0 as i32)) >> lm;

        // Threshold depends on bit allocation density
        let thresh = 0.5 * 2.0f32.powf(-0.125 * depth as f32);
        let sqrt_1 = 1.0 / ((n0 << lm) as f32).sqrt();

        for c in 0..channels {
            let prev1 = prev1_log_e[c * nb_bands + i];
            let prev2 = prev2_log_e[c * nb_bands + i];

            // Compute energy difference from previous frames
            let e_diff = (log_e[c * nb_bands + i] - prev1.min(prev2)).max(0.0);

            // Compute noise level based on energy difference
            let mut r = 2.0 * 2.0f32.powf(-e_diff);
            if lm == 3 {
                r *= 1.41421356; // sqrt(2) for long blocks
            }
            r = r.min(thresh) * sqrt_1;

            let band_start = (ebands[i] as usize) << lm;
            let band_size = n0 << lm;

            // Check if this band needs anti-collapse
            for k in 0..(1 << lm) {
                let mask_idx = i * channels + c;
                if mask_idx < collapse_masks.len() && (collapse_masks[mask_idx] & (1 << k)) == 0 {
                    // Fill with random noise
                    for j in 0..n0 {
                        seed = celt_lcg_rand(seed);
                        let idx = band_start + (j << lm) + k;
                        if idx < x.len() {
                            x[idx] = if (seed & 0x8000) != 0 { r } else { -r };
                        }
                    }

                    // Renormalize the band
                    let mut norm = 0.0f32;
                    for j in 0..band_size {
                        let idx = band_start + j;
                        if idx < x.len() {
                            norm += x[idx] * x[idx];
                        }
                    }
                    if norm > 0.0 {
                        let scale = 1.0 / norm.sqrt();
                        for j in 0..band_size {
                            let idx = band_start + j;
                            if idx < x.len() {
                                x[idx] *= scale;
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lcg_rand() {
        let seed = 12345;
        let next = celt_lcg_rand(seed);
        assert_ne!(seed, next);

        // Check deterministic
        let next2 = celt_lcg_rand(seed);
        assert_eq!(next, next2);
    }

    #[test]
    fn test_exp2_db() {
        // 0 dB should give gain of 1.0
        let gain = exp2_db(0.0);
        assert!((gain - 1.0).abs() < 0.01);

        // Positive dB should give gain > 1.0
        let gain = exp2_db(4.0); // +4 dB
        assert!(gain > 1.0);

        // Negative dB should give gain < 1.0
        let gain = exp2_db(-4.0); // -4 dB
        assert!(gain < 1.0);
    }

    #[test]
    fn test_denormalise_bands_bounds() {
        let x = vec![1.0; 960];
        let mut freq = vec![0.0; 960];
        let band_log_e = vec![0.0; 21];
        let ebands = &EBANDS_48K;

        denormalise_bands(&x, &mut freq, &band_log_e, ebands, 0, 21, 8);

        // Check that output is bounded
        for &val in &freq {
            assert!(val.is_finite());
            assert!(val.abs() < 1000.0); // Reasonable amplitude
        }
    }
}
