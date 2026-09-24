// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bit allocation. Ported from libopus `celt/rate.c` (`clt_compute_allocation` and its
//! `interp_bits2pulses`/`compute_pulse_cache` helpers). Ported from libopus (BSD-3-Clause), see
//! NOTICE. Owner (wave 1): "CeltBitstream".
//!
//! `compute_pulse_cache` (encoder-side mode setup, `CUSTOM_MODES`-only in libopus) is not
//! ported: the static 48 kHz mode's pulse cache is transcribed verbatim in `modes.rs`.
//!
//! Deviates from the wave-0 stub: `clt_compute_allocation` gained an `rd: &mut RangeDecoder<'_>`
//! parameter (per agreement with "CeltSynthesis" over `hub`) because the decode direction reads
//! range-coded band-skip/intensity/dual-stereo bits inline (`interp_bits2pulses`'s
//! `ec_dec_bit_logp`/`ec_dec_uint` calls); `encode`/`prev`/`signalBandwidth` are dropped since
//! they only affect the (unported) encode direction.

use crate::celt::modes::{celt_sudiv, CeltMode};
use crate::range::{RangeDecoder, BITRES};

/// C: `MAX_PSEUDO` (`celt/rate.h`).
const MAX_PSEUDO: i32 = 40;
/// C: `LOG_MAX_PSEUDO` (`celt/rate.h`).
const LOG_MAX_PSEUDO: i32 = 6;
/// C: `MAX_FINE_BITS` (`celt/rate.h`).
const MAX_FINE_BITS: i32 = 8;
/// C: `FINE_OFFSET` (`celt/rate.h`).
const FINE_OFFSET: i32 = 21;
/// C: `ALLOC_STEPS` (`celt/rate.c`).
const ALLOC_STEPS: i32 = 6;

/// C: `LOG2_FRAC_TABLE` (`celt/rate.c`).
static LOG2_FRAC_TABLE: [u8; 24] =
    [0, 8, 13, 16, 19, 21, 23, 24, 26, 27, 28, 29, 30, 31, 32, 32, 33, 34, 34, 35, 36, 36, 37, 37];

/// Helper: `m->eBands[i]` (`opus_int16`) widened to `i32` for arithmetic; the reference
/// stores `eBands` as `opus_int16` (`celt/modes.h`), but every consumer immediately does
/// 32-bit arithmetic on it.
#[inline]
fn eb(m: &CeltMode, i: usize) -> i32 {
    m.e_bands[i] as i32
}

/// C: `get_pulses` (`celt/rate.h`).
pub(crate) fn get_pulses(i: i32) -> i32 {
    if i < 8 {
        i
    }
    else {
        (8 + (i & 7)) << ((i >> 3) - 1)
    }
}

/// C: `bits2pulses` (`celt/rate.h`).
pub(crate) fn bits2pulses(m: &CeltMode, band: i32, lm: i32, bits: i32) -> i32 {
    let lm = lm + 1;
    let cache = &m.cache.bits[m.cache.index[(lm * m.nb_ebands + band) as usize] as usize..];
    let mut lo: i32 = 0;
    let mut hi: i32 = cache[0] as i32;
    let bits = bits - 1;
    for _ in 0..LOG_MAX_PSEUDO {
        let mid = (lo + hi + 1) >> 1;
        if cache[mid as usize] as i32 >= bits {
            hi = mid;
        }
        else {
            lo = mid;
        }
    }
    let lo_bits = if lo == 0 { -1 } else { cache[lo as usize] as i32 };
    if bits - lo_bits <= cache[hi as usize] as i32 - bits {
        lo
    }
    else {
        hi
    }
}

/// C: `pulses2bits` (`celt/rate.h`).
pub(crate) fn pulses2bits(m: &CeltMode, band: i32, lm: i32, pulses: i32) -> i32 {
    let lm = lm + 1;
    let cache = &m.cache.bits[m.cache.index[(lm * m.nb_ebands + band) as usize] as usize..];
    if pulses == 0 {
        0
    }
    else {
        cache[pulses as usize] as i32 + 1
    }
}

/// Output of [`clt_compute_allocation`]: per-band bit/pulse allocation. C: out-parameters
/// `pulses`, `ebits`, `fine_priority` of `clt_compute_allocation`, plus its `intensity`/`dual`
/// return-adjacent out-params, and the `balance`/total-bits return value.
pub struct Allocation {
    pub pulses: Vec<i32>,
    pub fine_energy_bits: Vec<i32>,
    pub fine_priority: Vec<i32>,
    pub intensity: i32,
    pub dual_stereo: bool,
    pub balance: i32,
    pub coded_bands: i32,
}

/// C: `interp_bits2pulses` (decode direction only, `encode == 0`).
#[allow(clippy::too_many_arguments)]
fn interp_bits2pulses(
    m: &CeltMode,
    start: i32,
    end: i32,
    skip_start: i32,
    bits1: &[i32],
    bits2: &[i32],
    thresh: &[i32],
    cap: &[i32],
    total: i32,
    skip_rsv: i32,
    intensity_rsv_in: i32,
    dual_stereo_rsv_in: i32,
    bits: &mut [i32],
    ebits: &mut [i32],
    fine_priority: &mut [i32],
    channels: i32,
    lm: i32,
    rd: &mut RangeDecoder<'_>,
) -> (i32, i32, bool, i32, i32) {
    let alloc_floor = channels << BITRES;
    let stereo = channels > 1;
    let log_m = lm << BITRES;

    let mut lo = 0i32;
    let mut hi = 1i32 << ALLOC_STEPS;
    for _ in 0..ALLOC_STEPS {
        let mid = (lo + hi) >> 1;
        let mut psum = 0i32;
        let mut done = false;
        for j in (start..end).rev() {
            let ji = j as usize;
            let tmp = bits1[ji] + ((mid * bits2[ji]) >> ALLOC_STEPS);
            if tmp >= thresh[ji] || done {
                done = true;
                psum += tmp.min(cap[ji]);
            }
            else if tmp >= alloc_floor {
                psum += alloc_floor;
            }
        }
        if psum > total {
            hi = mid;
        }
        else {
            lo = mid;
        }
    }

    let mut psum = 0i32;
    let mut done = false;
    for j in (start..end).rev() {
        let ji = j as usize;
        let tmp0 = bits1[ji] + ((lo * bits2[ji]) >> ALLOC_STEPS);
        let tmp = if tmp0 < thresh[ji] && !done {
            if tmp0 >= alloc_floor {
                alloc_floor
            }
            else {
                0
            }
        }
        else {
            done = true;
            tmp0
        };
        let tmp = tmp.min(cap[ji]);
        bits[ji] = tmp;
        psum += tmp;
    }

    // Decide which bands to skip, working backwards from the end.
    let mut coded_bands = end;
    let mut total = total;
    let mut intensity_rsv = intensity_rsv_in;
    let skip_start = skip_start;
    loop {
        let j = coded_bands - 1;
        if j <= skip_start {
            total += skip_rsv;
            break;
        }
        let left = total - psum;
        let percoeff = celt_sudiv(left, eb(m, coded_bands as usize) - eb(m, start as usize));
        let left = left - (eb(m, coded_bands as usize) - eb(m, start as usize)) * percoeff;
        let rem = (left - (eb(m, j as usize) - eb(m, start as usize))).max(0);
        let band_width = eb(m, coded_bands as usize) - eb(m, j as usize);
        let mut band_bits = bits[j as usize] + percoeff * band_width + rem;
        if band_bits >= thresh[j as usize].max(alloc_floor + (1 << BITRES)) {
            if rd.dec_bit_logp(1) {
                break;
            }
            psum += 1 << BITRES;
            band_bits -= 1 << BITRES;
        }
        psum -= bits[j as usize] + intensity_rsv;
        if intensity_rsv > 0 {
            intensity_rsv = LOG2_FRAC_TABLE[(j - start) as usize] as i32;
        }
        psum += intensity_rsv;
        if band_bits >= alloc_floor {
            psum += alloc_floor;
            bits[j as usize] = alloc_floor;
        }
        else {
            bits[j as usize] = 0;
        }
        coded_bands -= 1;
    }

    debug_assert!(coded_bands > start);

    // Code the intensity and dual stereo parameters.
    let intensity = if intensity_rsv > 0 { start + rd.dec_uint((coded_bands + 1 - start) as u32) as i32 } else { 0 };
    let mut dual_stereo_rsv = dual_stereo_rsv_in;
    if intensity <= start {
        total += dual_stereo_rsv;
        dual_stereo_rsv = 0;
    }
    let dual_stereo = if dual_stereo_rsv > 0 { rd.dec_bit_logp(1) } else { false };

    // Allocate the remaining bits.
    let left = total - psum;
    let percoeff = celt_sudiv(left, eb(m, coded_bands as usize) - eb(m, start as usize));
    let mut left = left - (eb(m, coded_bands as usize) - eb(m, start as usize)) * percoeff;
    for j in start..coded_bands {
        bits[j as usize] += percoeff * (eb(m, (j + 1) as usize) - eb(m, j as usize));
    }
    for j in start..coded_bands {
        let tmp = left.min(eb(m, (j + 1) as usize) - eb(m, j as usize));
        bits[j as usize] += tmp;
        left -= tmp;
    }

    let mut balance = 0i32;
    let mut j = start;
    while j < coded_bands {
        let ji = j as usize;
        let n0 = eb(m, ji + 1) - eb(m, ji);
        let n = n0 << lm;
        let bit = bits[ji] + balance;

        if n > 1 {
            let excess = (bit - cap[ji]).max(0);
            bits[ji] = bit - excess;

            let den = channels * n + if channels == 2 && n > 2 && !dual_stereo && j < intensity { 1 } else { 0 };
            let nc_log_n = den * (m.log_n[ji] as i32 + log_m);
            let mut offset = (nc_log_n >> 1) - den * FINE_OFFSET;
            if n == 2 {
                offset += (den << BITRES) >> 2;
            }
            if bits[ji] + offset < (den * 2) << BITRES {
                offset += nc_log_n >> 2;
            }
            else if bits[ji] + offset < (den * 3) << BITRES {
                offset += nc_log_n >> 3;
            }

            let mut eb = (bits[ji] + offset + (den << (BITRES - 1))).max(0);
            eb = celt_sudiv(eb, den) >> BITRES;
            if channels * eb > (bits[ji] >> BITRES) {
                eb = (bits[ji] >> (stereo as i32)) >> BITRES;
            }
            eb = eb.min(MAX_FINE_BITS);
            ebits[ji] = eb;

            fine_priority[ji] = (eb * (den << BITRES) >= bits[ji] + offset) as i32;

            bits[ji] -= (channels * eb) << BITRES;
            if excess > 0 {
                let extra_fine = (excess >> (stereo as i32 + BITRES as i32)).min(MAX_FINE_BITS - ebits[ji]);
                ebits[ji] += extra_fine;
                let extra_bits = (extra_fine * channels) << BITRES;
                fine_priority[ji] = (extra_bits >= excess - balance) as i32;
                balance = excess - extra_bits;
            }
            else {
                balance = excess;
            }
        }
        else {
            let excess = 0.max(bit - (channels << BITRES));
            bits[ji] = bit - excess;
            ebits[ji] = 0;
            fine_priority[ji] = 1;
            balance = excess;
        }
        j += 1;
    }

    // The skipped bands use all their bits for fine energy.
    while j < end {
        let ji = j as usize;
        ebits[ji] = (bits[ji] >> (stereo as i32)) >> BITRES;
        bits[ji] = 0;
        fine_priority[ji] = (ebits[ji] < 1) as i32;
        j += 1;
    }

    (coded_bands, balance, dual_stereo, intensity, total)
}

/// C: `clt_compute_allocation`.
#[allow(clippy::too_many_arguments)]
pub fn clt_compute_allocation(
    mode: &CeltMode,
    start: i32,
    end: i32,
    offsets: &[i32],
    caps: &[i32],
    alloc_trim: i32,
    total_bits: i32,
    lm: i32,
    channels: i32,
    rd: &mut RangeDecoder<'_>,
) -> Allocation {
    let len = mode.nb_ebands;
    let mut total = total_bits.max(0);
    let mut skip_start = start;
    let skip_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
    total -= skip_rsv;

    let mut intensity_rsv = 0i32;
    let mut dual_stereo_rsv = 0i32;
    if channels == 2 {
        intensity_rsv = LOG2_FRAC_TABLE[(end - start) as usize] as i32;
        if intensity_rsv > total {
            intensity_rsv = 0;
        }
        else {
            total -= intensity_rsv;
            dual_stereo_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
            total -= dual_stereo_rsv;
        }
    }

    let mut bits1 = vec![0i32; len as usize];
    let mut bits2 = vec![0i32; len as usize];
    let mut thresh = vec![0i32; len as usize];
    let mut trim_offset = vec![0i32; len as usize];

    for j in start..end {
        let ji = j as usize;
        let width = eb(mode, ji + 1) - eb(mode, ji);
        thresh[ji] = (channels << BITRES).max((3 * width << lm << BITRES) >> 4);
        trim_offset[ji] = channels * width * (alloc_trim - 5 - lm) * (end - j - 1) * (1 << (lm + BITRES as i32)) >> 6;
        if width << lm == 1 {
            trim_offset[ji] -= channels << BITRES;
        }
    }

    let nb_alloc_vectors = mode.nb_alloc_vectors();
    let mut lo = 1i32;
    let mut hi = nb_alloc_vectors - 1;
    loop {
        let mut done = false;
        let mut psum = 0i32;
        let mid = (lo + hi) >> 1;
        for j in (start..end).rev() {
            let ji = j as usize;
            let n = eb(mode, ji + 1) - eb(mode, ji);
            let mut bitsj = (channels * n * mode.alloc_vectors[(mid * len + j) as usize] as i32) << lm >> 2;
            if bitsj > 0 {
                bitsj = 0.max(bitsj + trim_offset[ji]);
            }
            bitsj += offsets[ji];
            if bitsj >= thresh[ji] || done {
                done = true;
                psum += bitsj.min(caps[ji]);
            }
            else if bitsj >= channels << BITRES {
                psum += channels << BITRES;
            }
        }
        if psum > total {
            hi = mid - 1;
        }
        else {
            lo = mid + 1;
        }
        if lo > hi {
            break;
        }
    }
    hi = lo;
    lo -= 1;

    for j in start..end {
        let ji = j as usize;
        let n = eb(mode, ji + 1) - eb(mode, ji);
        let mut bits1j = (channels * n * mode.alloc_vectors[(lo * len + j) as usize] as i32) << lm >> 2;
        let mut bits2j = if hi >= nb_alloc_vectors {
            caps[ji]
        }
        else {
            (channels * n * mode.alloc_vectors[(hi * len + j) as usize] as i32) << lm >> 2
        };
        if bits1j > 0 {
            bits1j = 0.max(bits1j + trim_offset[ji]);
        }
        if bits2j > 0 {
            bits2j = 0.max(bits2j + trim_offset[ji]);
        }
        if lo > 0 {
            bits1j += offsets[ji];
        }
        bits2j += offsets[ji];
        if offsets[ji] > 0 {
            skip_start = j;
        }
        bits2j = 0.max(bits2j - bits1j);
        bits1[ji] = bits1j;
        bits2[ji] = bits2j;
    }

    let mut pulses = vec![0i32; len as usize];
    let mut ebits = vec![0i32; len as usize];
    let mut fine_priority = vec![0i32; len as usize];

    let (coded_bands, balance, dual_stereo, intensity, _total) = interp_bits2pulses(
        mode,
        start,
        end,
        skip_start,
        &bits1,
        &bits2,
        &thresh,
        caps,
        total,
        skip_rsv,
        intensity_rsv,
        dual_stereo_rsv,
        &mut pulses,
        &mut ebits,
        &mut fine_priority,
        channels,
        lm,
        rd,
    );

    Allocation { pulses, fine_energy_bits: ebits, fine_priority, intensity, dual_stereo, balance, coded_bands }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::celt::modes::MODE_48000_960;
    use crate::range::RangeDecoder;

    /// `get_pulses` must be non-decreasing and `get_pulses(0) == 0` (`celt/rate.h`'s pseudo-log
    /// pulse-count table; `bits2pulses`'s binary search over `cache[]` relies on monotonicity).
    #[test]
    fn get_pulses_is_monotonic() {
        assert_eq!(get_pulses(0), 0);
        let mut prev = get_pulses(0);
        for i in 1..=40 {
            let v = get_pulses(i);
            assert!(v >= prev, "get_pulses({i})={v} < get_pulses({})={prev}", i - 1);
            prev = v;
        }
    }

    /// `bits2pulses` picks whichever of its two bracketing cache entries is *nearest* to
    /// `bits` (it can round up over budget — `quant_partition`'s no-split case, `bands.rs`,
    /// compensates with a decrement-until-it-fits loop). Check that pattern converges to a
    /// pulse count whose cost fits within budget (or `q == 0`, cost `0`), for every real
    /// band/LM of the static mode — this is the actual invariant the decoder relies on.
    #[test]
    fn bits2pulses_decrement_loop_fits_budget() {
        let m = &MODE_48000_960;
        for lm in 0..=m.max_lm {
            for band in 0..m.nb_ebands {
                for bits in [0, 1, 8, 16, 64, 128, 512, 2000] {
                    let mut q = bits2pulses(m, band, lm, bits);
                    let mut cost = pulses2bits(m, band, lm, q);
                    while cost > bits && q > 0 {
                        q -= 1;
                        cost = pulses2bits(m, band, lm, q);
                    }
                    assert!(cost <= bits.max(0), "band={band} lm={lm} bits={bits}: q={q} cost={cost}");
                    assert!(q >= 0);
                }
            }
        }
    }

    /// Builds a permissive `caps` array (`init_caps`'s decode-only consumer, owned by
    /// "CeltSynthesis"'s `celt.rs`, isn't available here) generous enough that `clt_compute_
    /// allocation`'s cap-clamping never binds, so these tests exercise the core bisection/
    /// `interp_bits2pulses` logic in isolation.
    fn loose_caps(m: &CeltMode, lm: i32, channels: i32) -> Vec<i32> {
        (0..m.nb_ebands)
            .map(|j| {
                let width = (m.e_bands[(j + 1) as usize] - m.e_bands[j as usize]) as i32;
                channels * width * 8 * (1 << lm)
            })
            .collect()
    }

    /// `clt_compute_allocation` must return a self-consistent allocation: `pulses[i] >= 0`,
    /// `coded_bands` within `(start, end]`, `intensity` within `[start, coded_bands]`, and
    /// `fine_energy_bits` within `[0, MAX_FINE_BITS]`, for a range of `total_bits` budgets and
    /// both mono/stereo, decoding the skip/intensity/dual-stereo bits from real (arbitrary, but
    /// valid-range-coded) packet bytes.
    #[test]
    fn clt_compute_allocation_invariants() {
        let m = &MODE_48000_960;
        let start = 0;
        let end = m.nb_ebands;
        let offsets = vec![0i32; m.nb_ebands as usize];
        for lm in 0..=m.max_lm {
            for channels in [1, 2] {
                let caps = loose_caps(m, lm, channels);
                for total_bits in [0, 100, 800, 3200, 8000, 20000] {
                    // Exercise a few different (arbitrary) byte payloads: `clt_compute_
                    // allocation` only needs *some* valid range-coded stream to pull its
                    // skip/intensity/dual-stereo bits from.
                    for seed in [0u8, 0x5A, 0xFF] {
                        let data = vec![seed; 64];
                        let mut rd = RangeDecoder::new(&data);
                        let alloc =
                            clt_compute_allocation(m, start, end, &offsets, &caps, 5, total_bits, lm, channels, &mut rd);

                        assert!(alloc.coded_bands > start && alloc.coded_bands <= end, "coded_bands={}", alloc.coded_bands);
                        assert!(
                            alloc.intensity >= start && alloc.intensity <= alloc.coded_bands,
                            "intensity={} coded_bands={}",
                            alloc.intensity,
                            alloc.coded_bands
                        );
                        assert_eq!(alloc.pulses.len(), m.nb_ebands as usize);
                        for j in 0..m.nb_ebands {
                            let ji = j as usize;
                            assert!(alloc.pulses[ji] >= 0, "pulses[{j}]={} < 0", alloc.pulses[ji]);
                            assert!(
                                alloc.fine_energy_bits[ji] >= 0 && alloc.fine_energy_bits[ji] <= MAX_FINE_BITS,
                                "fine_energy_bits[{j}]={}",
                                alloc.fine_energy_bits[ji]
                            );
                        }
                    }
                }
            }
        }
    }
}
