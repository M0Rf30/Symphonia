// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The envelope adjuster ("CalculateSbrEnvelope", spec section 11.9).
//!
//! Values that the reference stores as `float` are rounded to `f32` at the same points (see [`r`]);
//! everything else is evaluated in `f64`.

use super::super::{
    FreqTables, MAX_BANDS_HIGH, MAX_ENVELOPES, MAX_NOISE_ENVELOPES, SbrFrame, SbrHeader,
};
use super::qmf::{ROWS, Rows};
use super::tables::{LIM_GAINS, RANDOM_PHASE, SBR_MAX_GAIN, SMOOTH_FILTER, SMOOTH_LENGTHS};

/// Row of the row array holding window column 0.
const BASE: usize = 2;
/// QMF columns per SBR time slot.
const COLS_PER_SLOT: usize = 2;
/// Last QMF window column that can be processed (6 look-ahead columns after the 18 frame columns).
const MAX_COL: usize = ROWS - BASE;
/// Guard added to the denominators of the gain formulas.
const EPS: f64 = 1e-12;
/// Mask of the noise table index (512 entries).
const NOISE_MASK: usize = 0x1ff;

/// Rounds to the nearest `f32` and widens again: emulates a store to a `float` variable.
#[inline(always)]
fn r(x: f64) -> f64 {
    f64::from(x as f32)
}

/// State carried between frames by one channel.
pub(super) struct State {
    /// Limited, unsmoothed gain of the previous envelope.
    hist_gain: [f64; MAX_BANDS_HIGH],
    /// Noise level of the previous envelope.
    hist_noise: [f64; MAX_BANDS_HIGH],
    /// The histories must be replaced by the next envelope's values.
    init: bool,
    /// Running index into the noise table.
    noise_idx: usize,
    /// 0 if the previous frame's transient position equalled its envelope count, else -1.
    prev_la: i32,
}

impl State {
    pub fn new() -> Self {
        State {
            hist_gain: [0.0; MAX_BANDS_HIGH],
            hist_noise: [0.0; MAX_BANDS_HIGH],
            init: true,
            noise_idx: 0,
            prev_la: -1,
        }
    }

    /// Header reset: the histories are re-initialised and the noise index restarts; `prev_la` is
    /// kept.
    pub fn reset(&mut self) {
        self.init = true;
        self.noise_idx = 0;
    }

    /// Shapes the generated HF bands `kx..k2` of the columns covered by the frame's envelopes.
    pub fn process(
        &mut self,
        rows: &mut Rows,
        frame: &SbrFrame,
        header: &SbrHeader,
        tables: &FreqTables,
    ) {
        let kx = tables.kx;
        let k_count = (tables.k2 - kx).min(MAX_BANDS_HIGH);
        let nenv = frame.num_env.min(MAX_ENVELOPES);
        let la = i32::from(frame.l_a);
        let lim_gain = f64::from(LIM_GAINS[usize::from(header.limiter_gains).min(3)]);
        let smooth_len = SMOOTH_LENGTHS[usize::from(header.smoothing_mode).min(1)];
        let interpolate = header.interpol_freq != 0;

        let mut noise_set = 0usize;
        let mut nidx = 0usize;
        let mut noise_idx = self.noise_idx;

        for e in 0..nenv {
            let (tab, nbands) = if frame.freq_res[e] {
                (&tables.f_hi[..], tables.n_high)
            }
            else {
                (&tables.f_lo[..], tables.n_low)
            };
            let border = usize::from(frame.borders[e]);
            let c0 = COLS_PER_SLOT * border;
            let c1 = (COLS_PER_SLOT * usize::from(frame.borders[e + 1])).min(MAX_COL);

            if nidx + 1 < frame.noise_borders.len()
                && border == usize::from(frame.noise_borders[1 + nidx])
            {
                noise_set += 1;
                nidx += 1;
            }
            let ns = noise_set.min(MAX_NOISE_ENVELOPES - 1);

            let is_tr = e as i32 == la || e as i32 == self.prev_la;
            let smooth = if is_tr { 0 } else { smooth_len };

            // Mean energy per bin over the envelope's columns.
            let ncols = c1.saturating_sub(c0).max(1) as f64;
            let mut e_bin = [0.0f64; MAX_BANDS_HIGH];
            for row in (BASE + c0)..(BASE + c1) {
                for k in 0..k_count {
                    let (xr, xi) = (rows.re[row][kx + k], rows.im[row][kx + k]);
                    e_bin[k] += xr * xr + xi * xi;
                }
            }
            for v in e_bin[..k_count].iter_mut() {
                *v = r(*v / ncols);
            }

            let mut est = [0.0f64; MAX_BANDS_HIGH];
            let mut nrg = [0.0f64; MAX_BANDS_HIGH];
            let mut gain = [0.0f64; MAX_BANDS_HIGH];
            let mut noise_lvl = [0.0f64; MAX_BANDS_HIGH];

            let mut nb_i = 0usize;
            let mut nthr = noise_edge(tables, 1) - kx as isize;
            for b in 0..nbands {
                let k0 = usize::from(tab[b]) - kx;
                let k1 = (usize::from(tab[b + 1]) - kx).min(k_count);
                est[k0..k1].copy_from_slice(&e_bin[k0..k1]);
                if !interpolate {
                    let m = est[k0..k1].iter().sum::<f64>() / (k1 - k0) as f64;
                    est[k0..k1].fill(f64::from(m as f32));
                }
                let tv = f64::from(frame.env[e][b]);
                for k in k0..k1 {
                    if nthr <= k as isize {
                        nb_i += 1;
                    }
                    nthr = noise_edge(tables, nb_i + 1) - kx as isize;
                    nrg[k] = tv;
                    let q = f64::from(frame.noise[ns][nb_i.min(tables.nq.saturating_sub(1))]);
                    let qq = q / (q + 1.0);
                    gain[k] = if is_tr {
                        (nrg[k] / (est[k] + 1.0)).sqrt()
                    }
                    else {
                        (qq * nrg[k] / (EPS + (1.0 + est[k]) * q)).sqrt()
                    };
                    noise_lvl[k] = (qq * nrg[k]).sqrt();
                }
            }
            for k in 0..k_count {
                gain[k] = r(gain[k]);
                noise_lvl[k] = r(noise_lvl[k]);
                nrg[k] = r(nrg[k]);
                est[k] = r(est[k]);
            }

            // Limiter: per limiter band the gain may not exceed the energy ratio of the band.
            for j in 0..tables.nl {
                let a = usize::from(tables.f_lim[j]);
                let b = usize::from(tables.f_lim[j + 1]).min(k_count);
                let sum_nrg: f64 = nrg[a..b].iter().sum();
                let sum_est: f64 = est[a..b].iter().sum();
                let lim =
                    (f64::from((((EPS + sum_nrg) / (EPS + sum_est)).sqrt()) as f32)) * lim_gain;
                let lim = lim.min(f64::from(SBR_MAX_GAIN));
                for g in gain[a..b].iter_mut() {
                    *g = r(g.min(lim));
                }
            }

            if self.init {
                self.hist_gain = gain;
                self.hist_noise = noise_lvl;
                self.init = false;
            }

            for c in c0..c1 {
                let j = c - c0;
                let f = if smooth >= 1 && j <= 3 { f64::from(SMOOTH_FILTER[j]) } else { 0.0 };
                let omf = r(1.0 - f);
                let row = BASE + c;
                for k in 0..k_count {
                    let g = gain[k] * omf + self.hist_gain[k] * f;
                    let nl = if is_tr { 0.0 } else { noise_lvl[k] * omf + self.hist_noise[k] * f };
                    let idx = (noise_idx + 1 + k) & NOISE_MASK;
                    let xr = rows.re[row][kx + k];
                    let xi = rows.im[row][kx + k];
                    rows.re[row][kx + k] = r(nl * f64::from(RANDOM_PHASE[2 * idx]) + xr * g);
                    rows.im[row][kx + k] = r(nl * f64::from(RANDOM_PHASE[2 * idx + 1]) + xi * g);
                }
                noise_idx = (noise_idx + k_count) & NOISE_MASK;
            }

            self.hist_gain = gain;
            self.hist_noise = noise_lvl;
        }

        self.noise_idx = noise_idx;
        self.prev_la = if la == frame.num_env as i32 { 0 } else { -1 };
    }
}

/// Noise-floor band border `i`, clamped to the table.
#[inline]
fn noise_edge(tables: &FreqTables, i: usize) -> isize {
    isize::from(tables.f_noise[i.min(tables.nq)])
}
