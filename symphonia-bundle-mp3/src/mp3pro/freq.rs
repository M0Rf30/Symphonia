// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SBR frequency band tables (spec section 11.6).
//!
//! The tables are derived from the SBR header at the *output* rate (twice the core rate) on a
//! 64-band QMF grid. Every "round" of the specification is `trunc(x + 0.5)` for `x >= 0`.

use std::f64::consts::LN_2;

use super::{
    FreqTables, MAX_BANDS_HIGH, MAX_BANDS_LOW, MAX_LIMITER_BANDS, MAX_NOISE_BANDS, SbrHeader,
};

/// The number of QMF bands at the output rate.
const QMF_BANDS: i32 = 64;

/// The start band offsets for each `start_freq` value.
const START_OFFSETS: [i32; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16, 20, 24, 28, 33];

/// Limiter bands per octave for each `limiter_bands` value (single precision constants).
const LIMITER_BANDS_PER_OCTAVE: [f64; 4] = [1.0, 1.2f32 as f64, 2.0, 3.0];

/// The bandwidth warping of the upper region of a two-region master table (`alter_scale`).
const WARP: f64 = 1.3f32 as f64;

/// The `k2 / k0` ratio above which the master table is split into two regions.
const TWO_REGION_RATIO: f64 = 2.2449;

/// The capacity of the intermediate band tables.
const MAX_MASTER: usize = 64;

/// Rounds a non-negative value half up (`trunc(x + 0.5)`).
fn round(x: f64) -> i32 {
    (x + 0.5) as i32
}

/// Computes `round(num / den)` for positive integers exactly.
fn div_round(num: i32, den: i32) -> i32 {
    (2 * num + den) / (2 * den)
}

/// The start and stop band numerators (in Hz) for the output sample rate `fs`.
fn rate_constants(fs: u32) -> (i32, i32) {
    if fs <= 31999 {
        (6000, 12000)
    }
    else if fs <= 63999 {
        (8000, 16000)
    }
    else {
        (10000, 20000)
    }
}

/// The first and one-past-last SBR band (`k0`, `k2`), or `None` if the combination is invalid.
fn start_stop(fs: u32, start: u8, stop: u8) -> Option<(i32, i32)> {
    let (start_hz, stop_hz) = rate_constants(fs);
    let fs = fs as i32;

    let k0 = div_round(QMF_BANDS * start_hz, fs) + START_OFFSETS[usize::from(start & 15)];

    let k2 = match stop {
        0..=13 => {
            // `stopMin` plus the `stop` smallest of the 13 steps of a geometric progression up to
            // the last QMF band.
            let stop_min = div_round(QMF_BANDS * stop_hz, fs);
            let ratio = f64::from(QMF_BANDS) / f64::from(stop_min);
            let mut v = [0i32; 14];
            for (i, v) in v.iter_mut().enumerate() {
                *v = round(f64::from(stop_min) * ratio.powf(i as f64 / 13.0));
            }
            let mut steps = [0i32; 13];
            for i in 0..13 {
                steps[i] = v[i + 1] - v[i];
            }
            steps.sort_unstable();
            stop_min + steps[..usize::from(stop)].iter().sum::<i32>()
        }
        14 => 2 * k0,
        _ => 3 * k0,
    };
    let k2 = k2.min(QMF_BANDS);

    if (1..=MAX_BANDS_HIGH as i32).contains(&(k2 - k0)) { Some((k0, k2)) } else { None }
}

/// A list of at most [`MAX_MASTER`] band borders or band widths.
struct List {
    v: [i32; MAX_MASTER],
    n: usize,
}

impl List {
    fn new() -> Self {
        List { v: [0; MAX_MASTER], n: 0 }
    }

    fn push(&mut self, x: i32) -> Option<()> {
        *self.v.get_mut(self.n)? = x;
        self.n += 1;
        Some(())
    }

    fn as_slice(&self) -> &[i32] {
        &self.v[..self.n]
    }

    fn as_mut_slice(&mut self) -> &mut [i32] {
        &mut self.v[..self.n]
    }
}

/// The (even) number of bands between `klo` and `khi` for `bands` bands per octave.
fn number_of_bands(bands: i32, klo: i32, khi: i32, warp: f64) -> i32 {
    let ln_ratio = (f64::from(khi) / f64::from(klo)).ln();
    2 * round(f64::from(bands) * ln_ratio / (2.0 * LN_2 * warp))
}

/// The sorted band widths of `n` bands geometrically spaced between `klo` and `khi`.
fn band_widths(klo: i32, khi: i32, n: i32) -> Option<List> {
    let ratio = f64::from(khi) / f64::from(klo);
    let mut widths = List::new();
    let mut prev = klo;
    for i in 1..=n {
        let v = round(f64::from(klo) * ratio.powf(f64::from(i) / f64::from(n)));
        widths.push(v - prev)?;
        prev = v;
    }
    widths.as_mut_slice().sort_unstable();
    Some(widths)
}

/// Appends the cumulative sums of `widths` to `master`, which must end at the region start.
fn accumulate(master: &mut List, widths: &List) -> Option<()> {
    let mut last = master.v[master.n.checked_sub(1)?];
    for &w in widths.as_slice() {
        last += w;
        master.push(last)?;
    }
    Some(())
}

/// The master band border table, or `None` if no bands result.
fn master_table(k0: i32, k2: i32, freq_scale: u8, alter_scale: bool) -> Option<List> {
    let mut master = List::new();
    master.push(k0)?;

    if freq_scale == 0 {
        // Linear scale: `n` bands of width 1 (or 2 with `alter_scale`); the remainder is spread
        // from the last band downwards (positive) or from the first band upwards (negative).
        let (width, n) = if alter_scale {
            (2, 2 * round(f64::from(k2 - k0) / 4.0))
        }
        else {
            (1, 2 * ((k2 - k0) / 2))
        };
        if n <= 0 {
            return None;
        }
        let mut widths = List::new();
        for _ in 0..n {
            widths.push(width)?;
        }
        let rem = k2 - (n * width + k0);
        let n = n as usize;
        if rem > 0 {
            for w in widths.as_mut_slice()[n - (rem as usize).min(n)..].iter_mut() {
                *w += 1;
            }
        }
        else {
            for w in widths.as_mut_slice()[..((-rem) as usize).min(n)].iter_mut() {
                *w -= 1;
            }
        }
        accumulate(&mut master, &widths)?;
        return Some(master);
    }

    let bands = match freq_scale {
        1 => 12,
        2 => 10,
        _ => 8,
    };
    let warp = if alter_scale { WARP } else { 1.0 };

    if f64::from(k2) / f64::from(k0) <= TWO_REGION_RATIO {
        let n = number_of_bands(bands, k0, k2, 1.0);
        accumulate(&mut master, &band_widths(k0, k2, n)?)?;
        return Some(master);
    }

    // Two regions split at `k1 = 2 * k0`; the upper one may use the warped band density.
    let k1 = 2 * k0;
    let n0 = number_of_bands(bands, k0, k1, 1.0);
    let n1 = number_of_bands(bands, k1, k2, warp);
    let w0 = band_widths(k0, k1, n0)?;
    let mut w1 = band_widths(k1, k2, n1)?;
    if w0.n == 0 || w1.n == 0 {
        return None;
    }

    // Keep the widest band of the lower region from exceeding the narrowest of the upper one.
    let w0_max = w0.v[w0.n - 1];
    if w0_max > w1.v[0] {
        let change = (w0_max - w1.v[0]).min((w1.v[w1.n - 1] - w1.v[0]) / 2);
        w1.v[0] += change;
        let last = w1.n - 1;
        w1.v[last] -= change;
        w1.as_mut_slice().sort_unstable();
    }

    accumulate(&mut master, &w0)?;
    // The upper region starts exactly at `k1`.
    *master.v.get_mut(master.n - 1)? = k1;
    accumulate(&mut master, &w1)?;
    Some(master)
}

/// Picks `n + 1` borders out of the `nsrc + 1` entries of `src`, spreading the bands evenly.
fn down_sample(src: &[u8], nsrc: usize, n: usize, out: &mut [u8]) -> usize {
    let (mut idx, mut remaining, mut groups) = (0, nsrc, n);
    out[0] = src[0];
    for slot in out[1..=n].iter_mut() {
        let step = remaining / groups;
        groups -= 1;
        idx += step;
        remaining -= step;
        *slot = src[idx];
    }
    n
}

/// Builds the frequency tables of `header` for the output sample rate `fs_out`, or `None` if the
/// header is rejected.
pub(super) fn tables(header: &SbrHeader, fs_out: u32) -> Option<FreqTables> {
    let (k0, k2) = start_stop(fs_out, header.start_freq, header.stop_freq)?;
    let master = master_table(k0, k2, header.freq_scale & 3, header.alter_scale != 0)?;
    let n_master = master.n - 1;
    let xover = usize::from(header.xover_band);

    if xover >= n_master || master.v[n_master] > QMF_BANDS {
        return None;
    }

    // High resolution table.
    let n_high = n_master - xover;
    if n_high > MAX_BANDS_HIGH {
        return None;
    }
    let mut f_hi = [0u8; MAX_BANDS_HIGH + 1];
    for (dst, &src) in f_hi.iter_mut().zip(&master.as_slice()[xover..]) {
        *dst = src as u8;
    }

    // Low resolution table: every second high resolution border. With an odd number of bands the
    // first low resolution band spans one band less.
    let n_low = n_high.div_ceil(2);
    let mut f_lo = [0u8; MAX_BANDS_LOW + 1];
    f_lo[0] = f_hi[0];
    if n_high & 1 == 1 {
        for i in 1..=n_low {
            f_lo[i] = f_hi[2 * i - 1];
        }
    }
    else {
        for i in 1..=n_low {
            f_lo[i] = f_hi[2 * i];
        }
    }

    let (kx, k2) = (f_lo[0], f_lo[n_low]);
    if n_low < 1 || i32::from(kx) > QMF_BANDS / 2 || k2 <= kx {
        return None;
    }

    // Noise floor bands: `noise_bands` per octave over the SBR range.
    let ln_range = (f64::from(k2) / f64::from(kx)).ln();
    let nq = if header.noise_bands != 0 {
        round(f64::from(header.noise_bands) * ln_range / LN_2).max(1) as usize
    }
    else {
        1
    };
    if nq > MAX_NOISE_BANDS {
        return None;
    }
    let mut f_noise = [0u8; MAX_NOISE_BANDS + 1];
    down_sample(&f_lo, n_low, nq, &mut f_noise);

    // Limiter bands, relative to `kx`.
    let per_octave = LIMITER_BANDS_PER_OCTAVE[usize::from(header.limiter_bands & 3)];
    let nl = round(ln_range * per_octave / LN_2).clamp(1, MAX_LIMITER_BANDS as i32) as usize;
    let mut f_lim = [0u8; MAX_LIMITER_BANDS + 1];
    down_sample(&f_lo, n_low, nl, &mut f_lim);
    for b in f_lim[..=nl].iter_mut() {
        *b -= kx;
    }

    Some(FreqTables {
        kx: usize::from(kx),
        k2: usize::from(k2),
        f_hi,
        n_high,
        f_lo,
        n_low,
        f_noise,
        nq,
        f_lim,
        nl,
    })
}
