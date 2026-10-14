// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The parametric bit allocation of ETSI TS 102 366, clause 6.2.
//!
//! The structure of the routines follows the reference description: exponents are mapped to a
//! power spectral density (PSD), integrated per critical band, converted to a masking curve, and
//! finally compared with the PSD to give each mantissa a bit allocation pointer (bap).

use crate::tables::{BAND_START, HEARING_THRESHOLD, LOG_ADD};

/// Number of critical bands.
pub const NUM_BANDS: usize = 50;

/// The critical band containing each of the 253 coded transform coefficients.
const BIN_TO_BAND: [u8; 253] = {
    let mut t = [0u8; 253];
    let mut band = 0;
    while band < NUM_BANDS {
        let mut bin = BAND_START[band] as usize;
        while bin < BAND_START[band + 1] as usize {
            t[bin] = band as u8;
            bin += 1;
        }
        band += 1;
    }
    t
};

/// A segment of delta bit allocation.
#[derive(Copy, Clone, Default)]
pub struct DeltaSeg {
    pub offset: u8,
    pub len: u8,
    pub value: u8,
}

/// Delta bit allocation mode.
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum DeltaMode {
    /// Reuse the previously transmitted segments.
    #[default]
    Reuse,
    /// New segments follow.
    New,
    /// No delta bit allocation.
    None,
}

/// Bit allocation parameters shared by all channels of a block.
#[derive(Copy, Clone, Default)]
pub struct Params {
    /// The sample rate code indexing the hearing threshold table.
    pub sr_code: usize,
    /// Right shift implied by half/quarter sample rate bit streams.
    pub sr_shift: usize,
    pub slow_decay: i32,
    pub fast_decay: i32,
    pub slow_gain: i32,
    pub db_per_bit: i32,
    pub floor: i32,
    pub cpl_fast_leak: i32,
    pub cpl_slow_leak: i32,
}

/// Maps exponents to PSD values and integrates them into per-band PSD values.
pub fn calc_psd(
    exp: &[u8],
    start: usize,
    end: usize,
    psd: &mut [i16; 256],
    band_psd: &mut [i16; 64],
) {
    if start >= end || end > 253 {
        return;
    }

    for bin in start..end {
        psd[bin] = 3072 - (i16::from(exp[bin]) << 7);
    }

    let mut bin = start;
    let mut band = usize::from(BIN_TO_BAND[start]);
    loop {
        let mut v = i32::from(psd[bin]);
        bin += 1;
        let band_end = usize::from(BAND_START[band + 1]).min(end);
        while bin < band_end {
            let p = i32::from(psd[bin]);
            let max = v.max(p);
            // Log-add.
            let adr = (max - ((v + p + 1) >> 1)).min(255);
            v = max + i32::from(LOG_ADD[adr as usize]);
            bin += 1;
        }
        band_psd[band] = v as i16;
        band += 1;
        if end <= usize::from(BAND_START[band]) {
            break;
        }
    }
}

fn lowcomp_step(a: i32, b0: i32, b1: i32, c: i32) -> i32 {
    if b0 + 256 == b1 {
        c
    }
    else if b0 > b1 {
        (a - 64).max(0)
    }
    else {
        a
    }
}

fn lowcomp(a: i32, b0: i32, b1: i32, band: usize) -> i32 {
    if band < 7 {
        lowcomp_step(a, b0, b1, 384)
    }
    else if band < 20 {
        lowcomp_step(a, b0, b1, 320)
    }
    else {
        (a - 128).max(0)
    }
}

/// Computes the masking curve for the bins `start..end`. Returns false if the delta bit
/// allocation segments are invalid.
#[allow(clippy::too_many_arguments)]
pub fn calc_mask(
    p: &Params,
    band_psd: &[i16; 64],
    start: usize,
    end: usize,
    fast_gain: i32,
    is_lfe: bool,
    dba_mode: DeltaMode,
    dba: &[DeltaSeg],
    mask: &mut [i16; 64],
) -> bool {
    if end == 0 || end > 253 || start >= end {
        return false;
    }

    let psd = |b: usize| i32::from(band_psd[b]);
    let mut excite = [0i32; NUM_BANDS];

    let band_start = usize::from(BIN_TO_BAND[start]);
    let band_end = usize::from(BIN_TO_BAND[end - 1]) + 1;

    let mut begin;
    let mut fastleak;
    let mut slowleak;

    if band_start == 0 {
        let mut low = 0;
        low = lowcomp_step(low, psd(0), psd(1), 384);
        excite[0] = psd(0) - fast_gain - low;
        low = lowcomp_step(low, psd(1), psd(2), 384);
        excite[1] = psd(1) - fast_gain - low;

        begin = 7;
        fastleak = 0;
        slowleak = 0;

        for band in 2..7 {
            if !(is_lfe && band == 6) {
                low = lowcomp_step(low, psd(band), psd(band + 1), 384);
            }
            fastleak = psd(band) - fast_gain;
            slowleak = psd(band) - p.slow_gain;
            excite[band] = fastleak - low;
            if !(is_lfe && band == 6) && psd(band) <= psd(band + 1) {
                begin = band + 1;
                break;
            }
        }

        for band in begin..band_end.min(22) {
            if !(is_lfe && band == 6) {
                low = lowcomp(low, psd(band), psd(band + 1), band);
            }
            fastleak = (fastleak - p.fast_decay).max(psd(band) - fast_gain);
            slowleak = (slowleak - p.slow_decay).max(psd(band) - p.slow_gain);
            excite[band] = (fastleak - low).max(slowleak);
        }
        begin = 22;
    }
    else {
        // Coupling channel.
        begin = band_start;
        fastleak = (p.cpl_fast_leak << 8) + 768;
        slowleak = (p.cpl_slow_leak << 8) + 768;
    }

    for band in begin..band_end {
        fastleak = (fastleak - p.fast_decay).max(psd(band) - fast_gain);
        slowleak = (slowleak - p.slow_decay).max(psd(band) - p.slow_gain);
        excite[band] = fastleak.max(slowleak);
    }

    for band in band_start..band_end {
        let tmp = p.db_per_bit - psd(band);
        if tmp > 0 {
            excite[band] += tmp >> 2;
        }
        let hth = i32::from(HEARING_THRESHOLD[band >> p.sr_shift][p.sr_code]);
        mask[band] = hth.max(excite[band]) as i16;
    }

    if dba_mode != DeltaMode::None {
        let mut band = band_start;
        for seg in dba {
            band += usize::from(seg.offset);
            let len = usize::from(seg.len);
            if band >= NUM_BANDS || len > NUM_BANDS - band {
                return false;
            }
            let delta = if seg.value >= 4 {
                (i32::from(seg.value) - 3) * 128
            }
            else {
                (i32::from(seg.value) - 4) * 128
            };
            for m in &mut mask[band..band + len] {
                *m = (i32::from(*m) + delta) as i16;
            }
            band += len;
        }
    }

    true
}

/// Computes the bit allocation pointers for the bins `start..end`.
pub fn calc_bap(
    mask: &[i16; 64],
    psd: &[i16; 256],
    start: usize,
    end: usize,
    snr_offset: i32,
    floor: i32,
    bap_tab: &[u8; 64],
    bap: &mut [u8; 256],
) {
    if start >= end || end > 253 {
        return;
    }

    // If the SNR offset is -960 dB, all mantissas are allocated zero bits.
    if snr_offset == -960 {
        bap.fill(0);
        return;
    }

    let mut bin = start;
    let mut band = usize::from(BIN_TO_BAND[start]);
    loop {
        let m = ((i32::from(mask[band]) - snr_offset - floor).max(0) & 0x1FE0) + floor;
        band += 1;
        let band_end = usize::from(BAND_START[band]).min(end);
        while bin < band_end {
            let address = ((i32::from(psd[bin]) - m) >> 5).clamp(0, 63);
            bap[bin] = bap_tab[address as usize];
            bin += 1;
        }
        if end <= band_end {
            break;
        }
    }
}
