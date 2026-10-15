// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The bit allocation: the masking curve that is derived from the exponents of a group, and the
//! number of bits (as an index into the mantissa sizes) of each of its mantissas.
//!
//! Ported from `bit_allocate`, `calc_lowcomp` and `log_add` of FFmpeg's `libavcodec/dolby_e.c`
//! (LGPL-2.1-or-later, Copyright (c) 2017 foo86). All values are integers.

use crate::tables::*;

/// The maximum number of exponents of a group.
pub(crate) const MAX_BIAS_EXP: usize = 50;

/// The sum of two values in the logarithmic domain.
fn log_add(a: i32, b: i32) -> i32 {
    let c = ((a - b).abs() >> 1).min(211) as usize;
    a.max(b) + i32::from(LOG_ADD[c])
}

/// Raise the masking curve (of a group of 128 coefficients) around the peaks.
fn calc_lowcomp(msk_val: &mut [i32; MAX_BIAS_EXP]) {
    let mut lwc_val = [0i32; 17];

    for i in 0..11usize {
        let mut max_j = 0usize;
        let mut max_v = i32::MIN;
        let mut thr = 0;

        for (k, j) in (i.saturating_sub(3)..=i + 3).enumerate() {
            let v = msk_val[j] + i32::from(LWC_GAIN[i][k]);

            if v > max_v {
                max_j = j;
                max_v = v;
            }

            thr = log_add(thr, v);
        }

        if msk_val[i] < thr {
            let k0 = 3usize.saturating_sub(max_j);

            for (k, j) in (max_j.saturating_sub(3)..=max_j + 3).enumerate() {
                lwc_val[j] += i32::from(LWC_ADJ[k0 + k]);
            }
        }
    }

    for i in 0..16 {
        let v = lwc_val[i].max(-512);
        msk_val[i] = (msk_val[i] + v).max(0);
    }
}

/// Allocate the bits of a group of `nb_exponent` exponents: store in `bap` the index into the
/// mantissa sizes of each mantissa.
///
/// `nb_code` is the transform of the group (0, 1, or 2), and `fr_code` the frame rate code
/// (1 to 5).
pub(crate) fn bit_allocate(
    nb_exponent: usize,
    nb_code: usize,
    fr_code: usize,
    exp: &[i32],
    bap: &mut [u8],
    fg_spc: usize,
    fg_ofs: usize,
    msk_mod: usize,
    snr_ofs: i32,
) {
    let mut msk_val = [0i32; MAX_BIAS_EXP];
    let mut psd_val = [0i32; MAX_BIAS_EXP];
    let mut fast_leak = 0i32;
    let mut slow_leak = 0i32;

    let dc_code = usize::from(DC_CODE[fr_code - 1]);
    let ht_code = usize::from(HT_CODE[fr_code - 1]);
    let fast_gain = i32::from(FAST_GAIN[fg_ofs]);
    let slow_decay = i32::from(SLOW_DECAY[dc_code][msk_mod]);
    let misc_decay = i32::from(MISC_DECAY[nb_code][dc_code][msk_mod]);
    let slow_gain = &SLOW_GAIN[nb_code][msk_mod];
    let fast_decay = &FAST_DECAY[nb_code][dc_code][msk_mod];
    let fast_gain_adj = &FAST_GAIN_ADJ[nb_code][dc_code][usize::from(BAND_OFS[nb_code][fg_spc])..];
    let hearing_thresh = &HEARING_THRESH[nb_code][ht_code];

    for i in 0..nb_exponent {
        psd_val[i] = (48 - exp[i]) * 64;
    }

    for i in 0..nb_exponent {
        fast_leak = log_add(
            fast_leak - i32::from(fast_decay[i]),
            psd_val[i] - fast_gain + i32::from(fast_gain_adj[i]),
        );
        slow_leak = log_add(slow_leak - slow_decay, psd_val[i] - i32::from(slow_gain[i]));
        msk_val[i] = fast_leak.max(slow_leak);
    }

    fast_leak = 0;
    for i in (usize::from(BAND_LOW[nb_code]) + 1..nb_exponent).rev() {
        fast_leak = log_add(fast_leak - misc_decay, psd_val[i] - fast_gain);
        msk_val[i] = msk_val[i].max(fast_leak);
    }

    for i in 0..nb_exponent {
        msk_val[i] = msk_val[i].max(i32::from(hearing_thresh[i]));
    }

    if nb_code == 0 {
        calc_lowcomp(&mut msk_val);
    }

    for i in 0..nb_exponent {
        let v = (16 * (snr_ofs - 64) + psd_val[i] - msk_val[i]) >> 5;
        bap[i] = BAP[v.clamp(0, 63) as usize];
    }
}
