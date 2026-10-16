// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tools shared by the ATRAC1 and ATRAC3 decoders: scale factor table, 48-tap QMF synthesis
//! filter and gain compensation. Port of FFmpeg's `libavcodec/atrac.c` (LGPL-2.1+, see `NOTICE`).

use std::io;

use symphonia_core::errors::{Error, Result};

use crate::tables::QMF_48TAP_HALF;

/// Number of samples carried between two calls of the QMF synthesis filter.
pub(crate) const QMF_DELAY: usize = 46;

/// Converts the error of a bit reader into a decode error.
pub(crate) fn bits<T>(r: io::Result<T>) -> Result<T> {
    r.map_err(|_| Error::DecodeError("atrac: truncated or invalid frame"))
}

/// Generates the scale factor table: `2^((i - 15) / 3)` (`atrac_generate_tables`).
pub(crate) fn make_sf_table() -> [f32; 64] {
    let mut t = [0.0; 64];
    for (i, v) in t.iter_mut().enumerate() {
        *v = 2.0f64.powf((i as f64 - 15.0) / 3.0) as f32;
    }
    t
}

/// Generates the symmetric 48-tap QMF window from its first half (`atrac_generate_tables`).
pub(crate) fn make_qmf_window() -> [f32; 48] {
    let mut w = [0.0; 48];
    for (i, &h) in QMF_48TAP_HALF.iter().enumerate() {
        let s = (f64::from(h) * 2.0) as f32;
        w[i] = s;
        w[47 - i] = s;
    }
    w
}

/// First half of the inverse QMF (`ff_atrac_iqmf`): sums and differences of the two bands are
/// interleaved after the delay line into `temp`, which must hold `QMF_DELAY + 2 * lo.len()`
/// samples.
///
/// The inputs may alias the output of [`iqmf_finish`], which is why the filter is split in two.
pub(crate) fn iqmf_prepare(
    lo: &[f32],
    hi: &[f32],
    delay: &[f32; QMF_DELAY],
    temp: &mut [f32],
) {
    temp[..QMF_DELAY].copy_from_slice(delay);

    for (k, (&l, &h)) in lo.iter().zip(hi).enumerate() {
        temp[QMF_DELAY + 2 * k] = l + h;
        temp[QMF_DELAY + 2 * k + 1] = l - h;
    }
}

/// Second half of the inverse QMF (`ff_atrac_iqmf`): filters `temp` into `out` (`2 * n_in`
/// samples) and stores the new delay line.
pub(crate) fn iqmf_finish(
    window: &[f32; 48],
    n_in: usize,
    temp: &[f32],
    out: &mut [f32],
    delay: &mut [f32; QMF_DELAY],
) {
    for (j, o) in out[..2 * n_in].chunks_exact_mut(2).enumerate() {
        let p1 = &temp[2 * j..2 * j + 48];

        let mut s1 = 0.0f32;
        let mut s2 = 0.0f32;

        for (p, w) in p1.chunks_exact(2).zip(window.chunks_exact(2)) {
            s1 += p[0] * w[0];
            s2 += p[1] * w[1];
        }

        o[0] = s2;
        o[1] = s1;
    }

    delay.copy_from_slice(&temp[2 * n_in..2 * n_in + QMF_DELAY]);
}

/// Gain control parameters of one subband (`AtracGainInfo`).
#[derive(Copy, Clone, Default)]
pub(crate) struct GainInfo {
    /// Number of gain control points.
    pub num_points: usize,
    /// Level at the corresponding control point.
    pub lev_code: [u8; 7],
    /// Location of the gain control points.
    pub loc_code: [u8; 7],
}

/// Gain compensation tables (`AtracGCContext`).
pub(crate) struct GainCompensation {
    /// Gain compensation level table.
    gain_tab1: [f32; 16],
    /// Gain compensation interpolation table.
    gain_tab2: [f32; 31],
    /// Offset for converting a level index into a level exponent.
    id2exp_offset: usize,
    /// Scale of the location code: `2^loc_scale` samples.
    loc_scale: u32,
    /// Size of a location code in samples.
    loc_size: usize,
}

impl GainCompensation {
    /// `ff_atrac_init_gain_compensation`.
    pub fn new(id2exp_offset: usize, loc_scale: u32) -> Self {
        let loc_size = 1usize << loc_scale;

        let mut gain_tab1 = [0.0f32; 16];
        for (i, v) in gain_tab1.iter_mut().enumerate() {
            *v = 2.0f32.powf(id2exp_offset as f32 - i as f32);
        }

        let mut gain_tab2 = [0.0f32; 31];
        for (i, v) in gain_tab2.iter_mut().enumerate() {
            let k = i as i32 - 15;
            *v = 2.0f32.powf(-1.0f32 / loc_size as f32 * k as f32);
        }

        GainCompensation { gain_tab1, gain_tab2, id2exp_offset, loc_scale, loc_size }
    }

    /// Applies the gain compensation and the MDCT overlap (`ff_atrac_gain_compensation`).
    ///
    /// `input` holds `2 * out.len()` samples of IMDCT output. Its first half is added to `prev`
    /// and gain compensated into `out`, and its second half becomes the new `prev`.
    pub fn apply(
        &self,
        input: &[f32],
        prev: &mut [f32],
        gc_now: &GainInfo,
        gc_next: &GainInfo,
        out: &mut [f32],
    ) {
        let n = out.len();

        let gc_scale = if gc_next.num_points > 0 {
            self.gain_tab1[usize::from(gc_next.lev_code[0] & 15)]
        }
        else {
            1.0f32
        };

        if gc_now.num_points == 0 {
            for ((o, &i), &p) in out.iter_mut().zip(input).zip(prev.iter()) {
                *o = i * gc_scale + p;
            }
        }
        else {
            let mut pos = 0;

            for i in 0..gc_now.num_points {
                let lastpos = (usize::from(gc_now.loc_code[i]) << self.loc_scale).min(n);

                let mut lev = self.gain_tab1[usize::from(gc_now.lev_code[i] & 15)];

                let next_lev = if i + 1 < gc_now.num_points {
                    usize::from(gc_now.lev_code[i + 1])
                }
                else {
                    self.id2exp_offset
                };
                let gain_inc = self.gain_tab2
                    [(next_lev + 15).saturating_sub(usize::from(gc_now.lev_code[i])).min(30)];

                // Apply a constant gain level and overlap.
                while pos < lastpos {
                    out[pos] = (input[pos] * gc_scale + prev[pos]) * lev;
                    pos += 1;
                }

                // Interpolate between two different gain levels.
                let end = (lastpos + self.loc_size).min(n);
                while pos < end {
                    out[pos] = (input[pos] * gc_scale + prev[pos]) * lev;
                    lev *= gain_inc;
                    pos += 1;
                }
            }

            while pos < n {
                out[pos] = input[pos] * gc_scale + prev[pos];
                pos += 1;
            }
        }

        // Copy the overlapping part into the delay buffer.
        prev.copy_from_slice(&input[n..2 * n]);
    }
}
