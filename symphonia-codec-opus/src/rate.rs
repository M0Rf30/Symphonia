// CELT Bit Allocation - From xiph/opus celt/rate.c
// Copyright (c) 2007-2010 CSIRO
// Copyright (c) 2007-2010 Xiph.Org Foundation
// SPDX-License-Identifier: BSD-3-Clause

use crate::entdec::RangeDecoder;
use crate::celt_constants::{ALLOC_VECTORS, EBANDS_48K, LOG_N};

const BITRES: i32 = 3;
const ALLOC_STEPS: i32 = 6;
const MAX_FINE_BITS: i32 = 8;
const FINE_OFFSET: i32 = 21;

/// Fractional log2 lookup table
const LOG2_FRAC_TABLE: [u8; 24] = [
    0, 8, 13, 16, 19, 21, 23, 24, 26, 27, 28, 29, 30, 31, 32,
    32, 33, 34, 34, 35, 36, 36, 37, 37,
];

/// Helper function for unsigned division (celt_udiv)
#[inline]
fn celt_udiv(n: i32, d: i32) -> i32 {
    if d > 0 {
        n / d
    } else {
        0
    }
}

/// Complete bit-to-pulse interpolation and allocation
///
/// This is the core CELT allocation algorithm that:
/// 1. Interpolates between two allocation vectors using binary search
/// 2. Determines which bands to skip working backwards from the end
/// 3. Decodes intensity stereo and dual stereo parameters
/// 4. Re-balances bits across bands
/// 5. Allocates fine energy bits and pulse bits per band
///
/// Based on interp_bits2pulses() from xiph/opus celt/rate.c
#[allow(clippy::too_many_arguments)]
pub fn interp_bits2pulses(
    start: usize,
    end: usize,
    skip_start: usize,
    bits1: &[i32],
    bits2: &[i32],
    thresh: &[i32],
    cap: &[i32],
    total: i32,
    skip_rsv: i32,
    intensity_rsv: i32,
    dual_stereo_rsv: i32,
    channels: usize,
    lm: usize,
    dec: &mut RangeDecoder,
    _prev: usize,
    _signal_bandwidth: usize,
) -> (usize, Vec<i32>, Vec<i32>, Vec<i32>, i32, usize, usize) {
    // Returns: (coded_bands, bits, ebits, fine_priority, balance, intensity, dual_stereo)

    let alloc_floor = (channels << BITRES) as i32;
    let stereo = if channels > 1 { 1 } else { 0 };
    let log_m = (lm << BITRES) as i32;

    // Binary search for interpolation parameter
    let mut lo = 0i32;
    let mut hi = 1i32 << ALLOC_STEPS;

    for _ in 0..ALLOC_STEPS {
        let mid = (lo + hi) >> 1;
        let mut psum = 0i32;
        let mut done = false;

        // Work backwards from end
        for j in (start..end).rev() {
            let tmp = bits1[j] + ((mid * bits2[j]) >> ALLOC_STEPS);
            if tmp >= thresh[j] || done {
                done = true;
                // Don't allocate more than we can actually use
                psum += tmp.min(cap[j]);
            } else if tmp >= alloc_floor {
                psum += alloc_floor;
            }
        }

        if psum > total {
            hi = mid;
        } else {
            lo = mid;
        }
    }

    // Compute initial bit allocation with found interpolation parameter
    let mut bits = vec![0i32; end];
    let mut psum = 0i32;
    let mut done = false;

    for j in (start..end).rev() {
        let mut tmp = bits1[j] + ((lo * bits2[j]) >> ALLOC_STEPS);
        if tmp < thresh[j] && !done {
            if tmp >= alloc_floor {
                tmp = alloc_floor;
            } else {
                tmp = 0;
            }
        } else {
            done = true;
        }
        // Don't allocate more than we can actually use
        tmp = tmp.min(cap[j]);
        bits[j] = tmp;
        psum += tmp;
    }

    // Decide which bands to skip, working backwards from the end
    let mut coded_bands = end;
    let mut total = total;
    let mut intensity_rsv = intensity_rsv;

    loop {
        let j = coded_bands - 1;

        // Never skip the first band, nor a band that has been boosted by dynalloc
        if j <= skip_start {
            // Give the bit we reserved to end skipping back
            total += skip_rsv;
            break;
        }

        // Figure out how many left-over bits we would be adding to this band
        let left = total - psum;
        let percoeff = celt_udiv(left, (EBANDS_48K[coded_bands] - EBANDS_48K[start]) as i32);
        let left = left - (EBANDS_48K[coded_bands] - EBANDS_48K[start]) as i32 * percoeff;
        let rem = 0.max(left - (EBANDS_48K[j] - EBANDS_48K[start]) as i32);
        let band_width = (EBANDS_48K[coded_bands] - EBANDS_48K[j]) as i32;
        let band_bits = bits[j] + percoeff * band_width + rem;

        // Only code a skip decision if we're above the threshold for this band
        if band_bits >= thresh[j].max(alloc_floor + (1 << BITRES)) {
            // Decode skip decision
            if dec.decode_bit_logp(1) {
                break;
            }
            // We used a bit to skip this band
            psum += 1 << BITRES;
        }

        // Reclaim the bits originally allocated to this band
        psum -= bits[j] + intensity_rsv;
        if intensity_rsv > 0 {
            intensity_rsv = LOG2_FRAC_TABLE[(j - start).min(23)] as i32;
        }
        psum += intensity_rsv;

        if band_bits >= alloc_floor {
            // If we have enough for a fine energy bit per channel, use it
            psum += alloc_floor;
            bits[j] = alloc_floor;
        } else {
            // Otherwise this band gets nothing at all
            bits[j] = 0;
        }

        coded_bands -= 1;
    }

    // Code the intensity and dual stereo parameters
    let intensity = if intensity_rsv > 0 {
        start + dec.decode_uint((coded_bands + 1 - start) as u32) as usize
    } else {
        0
    };

    let mut dual_stereo_rsv = dual_stereo_rsv;
    if intensity <= start {
        total += dual_stereo_rsv;
        dual_stereo_rsv = 0;
    }

    let dual_stereo = if dual_stereo_rsv > 0 {
        if dec.decode_bit_logp(1) { 1 } else { 0 }
    } else {
        0
    };

    // Allocate the remaining bits
    let left = total - psum;
    let percoeff = celt_udiv(left, (EBANDS_48K[coded_bands] - EBANDS_48K[start]) as i32);
    let mut left = left - (EBANDS_48K[coded_bands] - EBANDS_48K[start]) as i32 * percoeff;

    for j in start..coded_bands {
        bits[j] += percoeff * (EBANDS_48K[j + 1] - EBANDS_48K[j]) as i32;
    }

    for j in start..coded_bands {
        let tmp = left.min((EBANDS_48K[j + 1] - EBANDS_48K[j]) as i32);
        bits[j] += tmp;
        left -= tmp;
    }

    // Compute fine energy bits and PVQ pulse allocation
    let mut ebits = vec![0i32; end];
    let mut fine_priority = vec![0i32; end];
    let mut balance = 0i32;

    for j in start..coded_bands {
        let n0 = (EBANDS_48K[j + 1] - EBANDS_48K[j]) as i32;
        let n = n0 << lm;
        let bit = bits[j] + balance;

        let excess = if n > 1 {
            let excess = 0.max(bit - cap[j]);
            bits[j] = bit - excess;

            // Compensate for the extra DoF in stereo
            let den = (channels as i32) * n +
                if channels == 2 && n > 2 && dual_stereo == 0 && j < intensity { 1 } else { 0 };

            let nc_log_n = den * (LOG_N[j] as i32 + log_m);

            // Offset for the number of fine bits by log2(N)/2 + FINE_OFFSET
            let mut offset = (nc_log_n >> 1) - den * FINE_OFFSET;

            // N=2 is the only point that doesn't match the curve
            if n == 2 {
                offset += den << BITRES >> 2;
            }

            // Changing the offset for allocating the second and third fine energy bit
            if bits[j] + offset < den * 2 << BITRES {
                offset += nc_log_n >> 2;
            } else if bits[j] + offset < den * 3 << BITRES {
                offset += nc_log_n >> 3;
            }

            // Divide with rounding
            ebits[j] = 0.max(bits[j] + offset + (den << (BITRES - 1)));
            ebits[j] = celt_udiv(ebits[j], den) >> BITRES;

            // Make sure not to bust
            if (channels as i32) * ebits[j] > (bits[j] >> BITRES) {
                ebits[j] = bits[j] >> stereo >> BITRES;
            }

            // More than that is useless because that's about as far as PVQ can go
            ebits[j] = ebits[j].min(MAX_FINE_BITS);

            // If we rounded down or capped this band, make it a candidate for the final fine energy pass
            fine_priority[j] = if ebits[j] * (den << BITRES) >= bits[j] + offset { 1 } else { 0 };

            // Remove the allocated fine bits; the rest are assigned to PVQ
            bits[j] -= (channels as i32) * ebits[j] << BITRES;

            excess
        } else {
            // For N=1, all bits go to fine energy except for a single sign bit
            let excess = 0.max(bit - ((channels as i32) << BITRES));
            bits[j] = bit - excess;
            ebits[j] = 0;
            fine_priority[j] = 1;
            excess
        };

        // Fine energy can't take advantage of the re-balancing in quant_all_bands().
        // Instead, do the re-balancing here.
        if excess > 0 {
            let extra_fine = (excess >> (stereo + BITRES)).min(MAX_FINE_BITS - ebits[j]);
            ebits[j] += extra_fine;
            let extra_bits = extra_fine * (channels as i32) << BITRES;
            fine_priority[j] = if extra_bits >= excess - balance { 1 } else { 0 };
            balance = excess - extra_bits;
        } else {
            balance = excess;
        }
    }

    // The skipped bands use all their bits for fine energy
    for j in coded_bands..end {
        ebits[j] = bits[j] >> stereo >> BITRES;
        bits[j] = 0;
        fine_priority[j] = if ebits[j] < 1 { 1 } else { 0 };
    }

    (coded_bands, bits, ebits, fine_priority, balance, intensity, dual_stereo)
}

/// Compute bit allocation for CELT bands
///
/// This implements the complete allocation algorithm from xiph/opus rate.c
/// Computes thresholds, caps, and allocation vectors, then calls interp_bits2pulses
/// to perform the actual allocation.
///
/// Arguments:
/// - bits: Total bits available
/// - lm: log2(frame_size / 120)
/// - channels: Number of channels (1 or 2)
/// - start_band: First band to allocate
/// - end_band: Last band + 1
/// - alloc_trim: Allocation trim value (-5 to +5)
/// - dec: Range decoder for intensity/dual stereo/skip decisions
/// - prev: Previous coded bands (for hysteresis in skip decisions)
/// - signal_bandwidth: Signal bandwidth for skip decisions
///
/// Returns: (coded_bands, pulses, fine_quant, fine_priority, balance, intensity, dual_stereo)
pub fn compute_allocation(
    bits: i32,
    lm: usize,
    channels: usize,
    start_band: usize,
    end_band: usize,
    alloc_trim: i32,
    dec: &mut RangeDecoder,
    prev: usize,
    signal_bandwidth: usize,
) -> (usize, Vec<i32>, Vec<i32>, Vec<i32>, i32, usize, usize) {
    let nb_bands = end_band;
    let c = channels as i32;
    let lm_i32 = lm as i32;

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

    // Compute thresholds, trim offsets, and caps for each band
    let mut thresh = vec![0i32; nb_bands];
    let mut trim_offset = vec![0i32; nb_bands];
    let mut cap = vec![0i32; nb_bands];

    for j in start_band..end_band {
        if j + 1 >= EBANDS_48K.len() {
            continue;
        }

        let n = (EBANDS_48K[j + 1] - EBANDS_48K[j]) as i32;

        // Minimum threshold for PVQ allocation
        thresh[j] = (c << BITRES).max((3 * n << lm_i32 << BITRES) >> 4);

        // Tilt offset based on trim and band position
        let tilt = c * n * (alloc_trim - 5 - lm_i32) * (end_band as i32 - j as i32 - 1);
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

        for j in (start_band..end_band).rev() {
            if j + 1 >= EBANDS_48K.len() {
                continue;
            }

            let n = (EBANDS_48K[j + 1] - EBANDS_48K[j]) as i32;

            // Get allocation from vector
            let mut bitsj = (c * n * (ALLOC_VECTORS[mid][j] as i32) << lm_i32) >> 2;

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

    for j in start_band..end_band {
        if j + 1 >= EBANDS_48K.len() {
            continue;
        }

        let n = (EBANDS_48K[j + 1] - EBANDS_48K[j]) as i32;

        // Bits from lower vector
        let mut bits1j = if lo < nb_alloc_vectors {
            (c * n * (ALLOC_VECTORS[lo][j] as i32) << lm_i32) >> 2
        } else {
            0
        };

        // Bits from upper vector
        let mut bits2j = if hi < nb_alloc_vectors {
            (c * n * (ALLOC_VECTORS[hi][j] as i32) << lm_i32) >> 2
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

    // Call interp_bits2pulses to perform the complete allocation
    let skip_start = start_band; // TODO: Handle dynamic allocation boosting
    interp_bits2pulses(
        start_band,
        end_band,
        skip_start,
        &bits1,
        &bits2,
        &thresh,
        &cap,
        total,
        skip_rsv,
        intensity_rsv,
        dual_stereo_rsv,
        channels,
        lm,
        dec,
        prev,
        signal_bandwidth,
    )
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
    use crate::celt_constants::NB_BANDS;

    #[test]
    fn test_complete_allocation() {
        // Create a dummy decoder with sufficient data
        let dummy_data = vec![0xFFu8; 1024];
        let mut dec = RangeDecoder::new(&dummy_data).unwrap();

        // Test with typical parameters
        let (coded_bands, bits, ebits, fine_priority, _balance, _intensity, _dual_stereo) =
            compute_allocation(8000, 3, 2, 0, NB_BANDS, 0, &mut dec, NB_BANDS, NB_BANDS);

        assert!(coded_bands > 0 && coded_bands <= NB_BANDS);
        assert_eq!(bits.len(), NB_BANDS);
        assert_eq!(ebits.len(), NB_BANDS);
        assert_eq!(fine_priority.len(), NB_BANDS);

        // Check that allocation happened
        let total_bits: i32 = bits.iter().sum();
        assert!(total_bits >= 0, "Should have non-negative bit allocation");

        // Check fine quant is reasonable
        for &fq in &ebits {
            assert!(fq <= MAX_FINE_BITS, "Fine quant should be <= MAX_FINE_BITS");
        }
    }

    #[test]
    fn test_low_bitrate_allocation() {
        let dummy_data = vec![0xFFu8; 1024];
        let mut dec = RangeDecoder::new(&dummy_data).unwrap();

        // Very low bitrate
        let (coded_bands, bits, _ebits, _fine_priority, _balance, _intensity, _dual_stereo) =
            compute_allocation(1000, 3, 1, 0, NB_BANDS, 0, &mut dec, NB_BANDS, NB_BANDS);

        // Should code at least some bands
        assert!(coded_bands > 0);

        // Should have some allocation
        let total_bits: i32 = bits.iter().sum();
        assert!(total_bits >= 0);
    }

    #[test]
    fn test_high_bitrate_allocation() {
        let dummy_data = vec![0xFFu8; 1024];
        let mut dec = RangeDecoder::new(&dummy_data).unwrap();

        // High bitrate
        let (coded_bands, bits, ebits, _fine_priority, _balance, _intensity, _dual_stereo) =
            compute_allocation(40000, 3, 2, 0, NB_BANDS, 0, &mut dec, NB_BANDS, NB_BANDS);

        assert!(coded_bands > NB_BANDS / 2);

        let total_bits: i32 = bits.iter().sum();
        assert!(total_bits > 0);

        // Should have good fine quantization
        let high_fine = ebits.iter().filter(|&&fq| fq >= 3).count();
        assert!(high_fine > 0);
    }
}
