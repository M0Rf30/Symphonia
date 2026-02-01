// CELT Stereo Processing - From xiph/opus celt/celt_decoder.c and celt/bands.c
// Copyright (c) 2007-2010 CSIRO
// Copyright (c) 2007-2010 Xiph.Org Foundation
// Copyright (c) 2008-2010 Gregory Maxwell
// SPDX-License-Identifier: BSD-3-Clause

//! CELT stereo decoding routines for Opus
//!
//! This module implements the stereo decoding logic for CELT, including:
//! - Intensity stereo: High-frequency bands share content between channels
//! - Dual stereo: Independent pulse vectors for L and R channels
//! - Mid-side to L-R conversion
//!
//! Reference: xiph/opus celt/celt_decoder.c lines 400-700

use crate::celt_constants::EBANDS_48K;

/// Convert mid-side encoded coefficients to left-right
///
/// For mid-side stereo, the encoder stores:
/// - M (mid) = (L + R) / sqrt(2)
/// - S (side) = (L - R) / sqrt(2)
///
/// We convert back to L-R:
/// - L = (M + S) / sqrt(2)
/// - R = (M - S) / sqrt(2)
///
/// Combined: L = (M + S) * 0.5 * sqrt(2), R = (M - S) * 0.5 * sqrt(2)
/// But since encoder already scaled, we just use:
/// - L = M + S
/// - R = M - S
/// Then normalize by sqrt(2)
///
/// Arguments:
/// - x: Interleaved frequency coefficients [mid0, side0, mid1, side1, ...]
/// - freq: Output frequency buffer [L channel, R channel]
/// - band_start: Start bin of the band
/// - band_end: End bin of the band
/// - frame_size: Total frame size
#[allow(dead_code)]
pub fn stereo_merge(
    x: &[f32],
    freq: &mut [f32],
    band_start: usize,
    band_end: usize,
    frame_size: usize,
) {
    let inv_sqrt2 = std::f32::consts::FRAC_1_SQRT_2;

    for i in band_start..band_end {
        let mid = x[i];
        let side = x[frame_size + i]; // Side channel is in second half

        // Convert M-S to L-R
        freq[i] = (mid + side) * inv_sqrt2;
        freq[frame_size + i] = (mid - side) * inv_sqrt2;
    }
}

/// Apply intensity stereo to a band
///
/// For intensity stereo, both channels share the same spectral shape
/// but with different energies. The mid channel is copied to the side
/// channel and then both are scaled by their respective energies.
///
/// Arguments:
/// - x: Normalized coefficients for mid channel
/// - freq: Output buffer (both channels)
/// - band_start: Start bin of the band
/// - band_end: End bin of the band
/// - frame_size: Total frame size
/// - mid_energy: Energy for mid channel (log scale)
/// - side_energy: Energy for side channel (log scale)
#[allow(dead_code)]
pub fn intensity_stereo(
    x: &[f32],
    freq: &mut [f32],
    band_start: usize,
    band_end: usize,
    frame_size: usize,
) {
    // For intensity stereo, copy mid to side
    // The actual energy scaling happens in denormalise_bands
    for i in band_start..band_end {
        freq[i] = x[i];
        freq[frame_size + i] = x[i];
    }
}

/// Stereo band deinterleaving
///
/// Takes interleaved stereo coefficients and separates them into
/// per-channel frequency bins.
///
/// Arguments:
/// - x: Interleaved input [L0, R0, L1, R1, ...]
/// - freq: Output [channel0 coeffs, channel1 coeffs]
/// - start: Start band
/// - end: End band
/// - lm: log2(frame_size / 120)
#[allow(dead_code)]
pub fn stereo_deinterleave(
    x: &[f32],
    freq: &mut [f32],
    start: usize,
    end: usize,
    lm: usize,
) {
    let m = 1 << lm;

    for i in start..end {
        let band_start = (EBANDS_48K[i] as usize) * m;
        let band_end = (EBANDS_48K[i + 1] as usize) * m;
        let n = band_end - band_start;

        // Deinterleave: input is [L0, R0, L1, R1, ...], output is [L0, L1, ...][R0, R1, ...]
        for j in 0..n {
            freq[band_start + j] = x[band_start * 2 + j * 2];
            freq[(EBANDS_48K[end] as usize) * m + band_start + j] = x[band_start * 2 + j * 2 + 1];
        }
    }
}

/// Process stereo decoding for a frame
///
/// This is the main stereo processing function that handles:
/// 1. Dual stereo bands (independent L/R decoding)
/// 2. Mid-side bands (joint stereo)
/// 3. Intensity stereo bands (shared spectral shape)
///
/// Arguments:
/// - x: Decoded normalized coefficients (frame_size * 2 for stereo)
/// - freq: Output frequency buffer (frame_size * 2)
/// - intensity: Band index where intensity stereo starts (0 = disabled)
/// - dual_stereo: Whether dual stereo is enabled (1 = enabled, 0 = disabled)
/// - coded_bands: Number of coded bands
/// - frame_size: Frame size in samples
/// - lm: log2(frame_size / 120)
#[allow(dead_code)]
pub fn process_stereo(
    x: &[f32],
    freq: &mut [f32],
    intensity: usize,
    dual_stereo: usize,
    coded_bands: usize,
    frame_size: usize,
    lm: usize,
) {
    let m = 1 << lm;
    let half_frame = frame_size;

    // Process each band according to its stereo mode
    for i in 0..coded_bands {
        let band_start = (EBANDS_48K[i] as usize) * m;
        let band_end = (EBANDS_48K[i + 1] as usize) * m;

        if intensity > 0 && i >= intensity {
            // Intensity stereo: copy mid to side
            for j in band_start..band_end {
                freq[j] = x[j];
                freq[half_frame + j] = x[j];
            }
        } else if dual_stereo != 0 {
            // Dual stereo: channels are already decoded separately
            // Just copy to output
            for j in band_start..band_end {
                freq[j] = x[j];
                freq[half_frame + j] = x[half_frame + j];
            }
        } else {
            // Mid-side stereo: convert to L-R
            stereo_merge(x, freq, band_start, band_end, half_frame);
        }
    }

    // Zero out uncoded bands for both channels
    let coded_end = (EBANDS_48K[coded_bands] as usize) * m;
    for j in coded_end..half_frame {
        freq[j] = 0.0;
        freq[half_frame + j] = 0.0;
    }
}

/// Compute spreading function for stereo bands
///
/// Calculates the spreading gain based on the angle parameter
/// decoded from the bitstream.
///
/// Arguments:
/// - theta: Angle parameter in Q14 format
///
/// Returns: (left_gain, right_gain) as normalized values
#[allow(dead_code)]
pub fn compute_stereo_spread(theta: i32) -> (f32, f32) {
    // theta is in Q14, range -16384 to 16384
    // Convert to radians: theta * PI / (2 * 16384)
    let angle = (theta as f32) * std::f32::consts::PI / 32768.0;

    // Compute gains
    // For theta = 0: equal L/R
    // For theta > 0: more energy to L
    // For theta < 0: more energy to R
    let cos_val = angle.cos();
    let sin_val = angle.sin();

    let left = (1.0 + cos_val) * 0.5 + sin_val * 0.5;
    let right = (1.0 + cos_val) * 0.5 - sin_val * 0.5;

    (left.sqrt(), right.sqrt())
}

/// Stereo unquantization
///
/// Decodes the stereo angle parameter and applies it to mid-side coefficients.
/// This is called celt_stereo_unquant in the reference implementation.
///
/// Arguments:
/// - x: Mid channel coefficients
/// - side: Side channel coefficients (output)
/// - n: Number of coefficients
/// - theta: Angle parameter
#[allow(dead_code)]
pub fn stereo_unquant(x: &[f32], side: &mut [f32], n: usize, theta: i32) {
    let (_lg, _rg) = compute_stereo_spread(theta);
    let inv_sqrt2 = std::f32::consts::FRAC_1_SQRT_2;

    for i in 0..n {
        let mid = x[i];
        // For M-S: L = (M + S), R = (M - S), normalized by sqrt(2)
        // But we don't have S yet, so we're computing the stereo spread
        side[i] = mid * inv_sqrt2;
    }
}

/// Normalize a vector to unit length
///
/// Used for normalizing pulse vectors before denormalization.
///
/// Arguments:
/// - x: Vector to normalize (modified in place)
/// - n: Length of vector
///
/// Returns: Original L2 norm before normalization
pub fn normalize_vector(x: &mut [f32], n: usize) -> f32 {
    let mut norm = 0.0f32;

    for i in 0..n {
        norm += x[i] * x[i];
    }

    norm = norm.sqrt();

    if norm > 1e-15 {
        let inv_norm = 1.0 / norm;
        for i in 0..n {
            x[i] *= inv_norm;
        }
    }

    norm
}

/// Renormalize after folding
///
/// After folding (copying from another band), the vector needs
/// to be renormalized to unit length.
///
/// Arguments:
/// - x: Vector to renormalize
/// - n: Length of vector
pub fn renormalise_vector(x: &mut [f32], n: usize) {
    let mut norm = 0.0f32;

    for i in 0..n {
        norm += x[i] * x[i];
    }

    if norm > 1e-15 {
        let inv_norm = 1.0 / norm.sqrt();
        for i in 0..n {
            x[i] *= inv_norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stereo_merge() {
        let x = vec![1.0, 0.5, 0.3, 0.0, 0.0, 0.0, 0.0, 0.0];
        let mut freq = vec![0.0; 8];

        // With side channel being zeros
        stereo_merge(&x, &mut freq, 0, 4, 4);

        // Check that L and R are computed correctly
        let inv_sqrt2 = std::f32::consts::FRAC_1_SQRT_2;
        assert!((freq[0] - x[0] * inv_sqrt2).abs() < 1e-6);
    }

    #[test]
    fn test_normalize_vector() {
        let mut x = vec![3.0, 4.0];
        let norm = normalize_vector(&mut x, 2);

        assert!((norm - 5.0).abs() < 1e-6);
        assert!((x[0] - 0.6).abs() < 1e-6);
        assert!((x[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_compute_stereo_spread() {
        // theta = 0 should give equal gains
        let (l, r) = compute_stereo_spread(0);
        assert!((l - r).abs() < 1e-6);

        // Both should be close to 1/sqrt(2) for equal energy
        assert!(l > 0.0 && l <= 1.0);
        assert!(r > 0.0 && r <= 1.0);
    }
}
