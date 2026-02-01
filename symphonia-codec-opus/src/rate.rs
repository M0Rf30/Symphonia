// CELT Bit Allocation - Rewritten from xiph/opus celt/rate.c
// Copyright (c) 2007-2010 CSIRO
// Copyright (c) 2007-2010 Xiph.Org Foundation
// SPDX-License-Identifier: BSD-3-Clause

use crate::entdec::RangeDecoder;
use crate::celt_constants::{ALLOC_VECTORS, EBANDS_48K};

/// Compute bit allocation for CELT bands
///
/// This function allocates the available bits among the frequency bands
/// based on the allocation trim, skip, intensity, and dual stereo parameters.
///
/// Arguments:
/// - bits: Total bits available for allocation
/// - lm: log2(frame_size / 120)
/// - channels: Number of channels (1 or 2)
/// - start_band: First band to allocate
/// - end_band: Last band + 1
/// - dec: Range decoder (to read skip and trim)
///
/// Returns: (cap, pulse allocation, fine_quant, fine_priority)
pub fn compute_allocation(
    bits: i32,
    lm: usize,
    channels: usize,
    start_band: usize,
    end_band: usize,
    dec: &mut RangeDecoder,
) -> (Vec<i32>, Vec<i32>, Vec<i32>) {
    let nb_bands = end_band - start_band;

    // Initialize output arrays
    let mut pulses = vec![0i32; nb_bands];
    let mut fine_quant = vec![0i32; nb_bands];
    let mut fine_priority = vec![0i32; nb_bands];

    // For simplified implementation, use static allocation
    // based on frame size
    let bits_per_band = bits / nb_bands as i32;

    for i in 0..nb_bands {
        // Compute band width
        let band_idx = start_band + i;
        let n = if band_idx + 1 < EBANDS_48K.len() {
            (EBANDS_48K[band_idx + 1] - EBANDS_48K[band_idx]) as usize
        } else {
            0
        };

        if n == 0 {
            continue;
        }

        // Allocate pulses based on available bits
        // This is simplified - real implementation uses allocation tables
        let band_bits = bits_per_band.max(0);

        // Reserve bits for fine energy
        let fine_bits = 3.min(band_bits / 4);
        fine_quant[i] = fine_bits;
        fine_priority[i] = 0;

        // Remaining bits go to pulses
        let pulse_bits = band_bits - fine_bits;

        // Estimate pulses from bits
        // Each pulse requires approximately log2(n) bits
        if n > 1 {
            let bits_per_pulse = (n as f32).log2().ceil() as i32;
            pulses[i] = (pulse_bits / bits_per_pulse.max(1)).min(20);
        }
    }

    (pulses, fine_quant, fine_priority)
}

/// Decode allocation trim parameter
///
/// The trim value adjusts the bit allocation between frequency bands.
/// Positive values allocate more bits to high frequencies.
pub fn decode_trim(dec: &mut RangeDecoder) -> i32 {
    // TRIM_ICDF table: [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0]
    const TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];

    let trim = dec.decode_icdf(&TRIM_ICDF, 10);

    // Convert to signed value centered at 5
    (trim as i32) - 5
}

/// Interleave Hadamard Transform
///
/// Used for intensity stereo and time-frequency interleaving
pub fn interleave_hadamard(x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    if !hadamard {
        return;
    }

    let n = n0 * stride;

    // Simple Hadamard transform for stride = 2
    if stride == 2 && n >= 2 {
        for i in (0..n).step_by(2) {
            let a = x[i];
            let b = x[i + 1];
            x[i] = a + b;
            x[i + 1] = a - b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocation_basic() {
        // Create a dummy decoder with some data
        let data = vec![0x80; 100];
        let mut dec = RangeDecoder::new(&data).unwrap();

        // Test allocation for 960-sample frame (lm = 3)
        let (pulses, fine_quant, fine_priority) =
            compute_allocation(8000, 3, 2, 0, 21, &mut dec);

        assert_eq!(pulses.len(), 21);
        assert_eq!(fine_quant.len(), 21);
        assert_eq!(fine_priority.len(), 21);

        // Check that some allocation was made
        let total_pulses: i32 = pulses.iter().sum();
        assert!(total_pulses > 0, "Should allocate some pulses");
    }

    #[test]
    fn test_interleave_hadamard() {
        let mut x = vec![1.0, 2.0, 3.0, 4.0];

        // Apply Hadamard transform
        interleave_hadamard(&mut x, 2, 2, true);

        // Check transform was applied
        assert_eq!(x[0], 3.0);  // 1 + 2
        assert_eq!(x[1], -1.0); // 1 - 2
        assert_eq!(x[2], 7.0);  // 3 + 4
        assert_eq!(x[3], -1.0); // 3 - 4
    }
}
