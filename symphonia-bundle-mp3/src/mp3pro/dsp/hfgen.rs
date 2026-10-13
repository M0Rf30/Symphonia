// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The HF generator ("lppTransposer", spec section 11.8).
//!
//! Intermediate values that the reference stores as `float` are rounded to `f32` at the same
//! points (see [`r`]); everything else is evaluated in `f64`.

use super::super::{FreqTables, SbrFrame};
use super::qmf::{BANDS, COLUMNS, Rows};
use super::tables::{CROSSFADE_RISE, WH_FACTORS, WH_THRESHOLDS};

/// Maximum number of patches.
const MAX_PATCHES: usize = 6;
/// Number of look-ahead / cross-fade columns.
const DELAY: usize = 6;
/// Row of the row array holding window column 0.
const BASE: usize = 2;
/// Number of window columns (frame plus look-ahead).
const WINDOW_COLS: usize = COLUMNS + DELAY;
/// Guard factor of the LPC determinant, `f32(0.999999)`.
const DET_GUARD: f64 = 0.999999f32 as f64;
/// Bandwidth smoothing: new value lower than the previous one.
const BW_DOWN_NEW: f32 = 0.75;
const BW_DOWN_OLD: f32 = 0.25;
/// Bandwidth smoothing: new value not lower than the previous one.
const BW_UP_NEW: f32 = 0.90625;
const BW_UP_OLD: f32 = 0.09375;
/// Bandwidths below this are zeroed.
const BW_MIN: f32 = 0.015625;
/// Upper bound of the per-patch bandwidth increment.
const BW_MAX: f32 = 0.99609375;
/// LPC coefficients with `|alpha|^2 >=` this are discarded.
const COEF_LIMIT: f64 = 16.0;

/// Rounds to the nearest `f32` and widens again: emulates a store to a `float` variable.
#[inline(always)]
fn r(x: f64) -> f64 {
    f64::from(x as f32)
}

/// One patch: source bands `[src_lo, src_hi)` are copied `shift` bands up.
#[derive(Copy, Clone, Default)]
struct Patch {
    src_lo: usize,
    src_hi: usize,
    shift: usize,
}

/// The patch table of a header plus the bandwidth factor row (rebuilt on every header reset).
pub(super) struct PatchTable {
    patches: [Patch; MAX_PATCHES],
    n_patches: usize,
    /// The `whFactorsTableMP3` row `[f0..f5]` selected by the first SBR band.
    wh: [f32; 6],
    src_min: usize,
    src_max: usize,
    /// One past the highest generated band.
    tgt_max: usize,
}

/// State carried between frames by one channel.
#[derive(Default)]
pub(super) struct State {
    prev_bw: [f64; MAX_PATCHES],
    /// The previous frame was "extended" (its last border exceeded the nominal end).
    pub extended: bool,
    prev_invf: u8,
}

/// The LPC covariance sums of one source band (spec section 11.8, step 1).
struct Covariance {
    p11: f64,
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
    det: f64,
}

/// The filter coefficients `alpha0`, `alpha1` of one source band.
#[derive(Copy, Clone, Default)]
struct Coefs {
    a0r: f64,
    a0i: f64,
    a1r: f64,
    a1i: f64,
}

impl PatchTable {
    /// Builds the patch table (spec section 11.8, "Patch table"). Returns `None` if the header is
    /// rejected (no usable patch).
    pub fn new(tables: &FreqTables, data_extra: u8, out_rate: u32) -> Option<Self> {
        let out_rate = f64::from(out_rate);
        let nb = tables.n_high;
        let f_hi: Vec<i32> = tables.f_hi[..=nb].iter().map(|&v| i32::from(v)).collect();
        let kx = tables.kx as i32;
        let src_off = 1;

        let mut v17 = f_hi[0];
        let v11 = f_hi[nb];
        let mut a3 = kx - v17;
        if v17 - src_off < 4 {
            return None;
        }
        if data_extra == 1 {
            v17 += a3;
            a3 = 0;
        }

        // Cut-off band of the 16 kHz limit.
        let v5 = ((BANDS * 2) as f64 * 16000.0 / out_rate + 0.5) as i32;
        let v4 = f_hi[0];
        let mut v16 = if v5 <= v4 {
            v4
        }
        else {
            let mut v16 = f_hi[nb];
            if v5 < v16 {
                let mut i = 0;
                while f_hi[i] < v5 {
                    i += 1;
                }
                v16 = f_hi[i];
            }
            v16
        };
        let v14 = v17 + a3;
        if (v16 - v11).abs() <= 3 {
            v16 = v11;
        }

        let v6 = src_off;
        let mut v5 = a3 + v6;
        let mut patches = [Patch::default(); MAX_PATCHES];
        let mut n = 0;
        let mut v10 = v14;
        let mut guard = 0;

        while v10 < v11 {
            guard += 1;
            if n > MAX_PATCHES - 1 || guard > 64 {
                return None;
            }

            let mut v9 = v16 - v10;
            if v17 - v5 <= v9 {
                v5 = v17 + ((v10 - v5) & !1);
                v9 = f_hi[0];
                if v9 < v5 {
                    v9 = f_hi[nb];
                    let mut v8 = nb;
                    if v5 < v9 {
                        while v5 < v9 {
                            v9 = f_hi[v8 - 1];
                            v8 -= 1;
                        }
                        v9 = f_hi[v8];
                    }
                }
                v9 -= v10;
            }

            let v7 = ((v10 + v9) - v17 + 1) & !1;
            if v9 >= 1 {
                let src_lo = v10 - v7;
                if src_lo < 0 || v7 < 0 {
                    return None;
                }
                patches[n] = Patch {
                    src_lo: src_lo as usize,
                    src_hi: (src_lo + v9) as usize,
                    shift: v7 as usize,
                };
                n += 1;
                v10 += v9;
            }
            if (v10 - v16).abs() <= 2 {
                v16 = v11;
            }
            v5 = v6;
        }

        // Drop a trailing patch of width <= 2 unless it is the only one.
        if n >= 2 && patches[n - 1].src_hi - patches[n - 1].src_lo <= 2 {
            n -= 1;
        }
        if n == 0 {
            return None;
        }

        let used = &patches[..n];
        let src_min = used.iter().map(|p| p.src_lo).min()?;
        let src_max = used.iter().map(|p| p.src_hi).max()?;
        let tgt_max = used.iter().map(|p| p.shift + p.src_hi).max()?;

        // Bandwidth factor row, selected by the frequency of the first SBR band.
        let x = (0.5 * out_rate * (v14 as f64 / BANDS as f64)) as u32;
        let mut row = 1;
        while WH_THRESHOLDS[row] <= x {
            row += 1;
            if row > 8 {
                break;
            }
        }

        Some(PatchTable {
            patches,
            n_patches: n,
            wh: WH_FACTORS[row - 1],
            src_min,
            src_max,
            tgt_max,
        })
    }

    /// Per-patch bandwidth factors of this frame (float32-exact, spec section 11.8) and the
    /// updated smoothing state.
    fn bandwidths(&self, st: &State, invf: u8) -> [f64; MAX_PATCHES] {
        let f = &self.wh;
        let mut bw0 = match invf {
            1 => f[2],
            2 => f[3],
            3 => f[4],
            _ => {
                if st.prev_invf == 1 {
                    f[1]
                }
                else {
                    f[0]
                }
            }
        };

        let prev0 = st.prev_bw[0] as f32;
        if bw0 < prev0 {
            bw0 = prev0 * BW_DOWN_OLD + bw0 * BW_DOWN_NEW;
        }
        else {
            bw0 = prev0 * BW_UP_OLD + bw0 * BW_UP_NEW;
        }
        if bw0 < BW_MIN {
            bw0 = 0.0;
        }

        let mut bw = [0.0f64; MAX_PATCHES];
        bw[0] = f64::from(bw0);
        for slot in bw.iter_mut().take(self.n_patches).skip(1) {
            let mut b = bw0;
            if bw0 != 0.0 {
                b = bw0 + f[5];
                if b >= BW_MAX {
                    b = BW_MAX;
                }
            }
            *slot = f64::from(b);
        }
        bw
    }

    /// Runs the HF generator over the rows of one channel and updates the channel state.
    ///
    /// `snap` is scratch space for a snapshot of the source rows.
    pub fn process(&self, st: &mut State, rows: &mut Rows, snap: &mut Rows, frame: &SbrFrame) {
        let nenv = frame.num_env.min(frame.borders.len() - 1);
        let excess = i32::from(frame.borders[nenv]) - 9;
        let first_col = (2 * usize::from(frame.borders[0])).min(WINDOW_COLS);
        // End of the cross-fade/middle regions (`c1`).
        let c1 = ((2 * excess + COLUMNS as i32).max(0) as usize).min(WINDOW_COLS);
        let extended = excess != 0;
        let zero_end = if extended { c1 } else { WINDOW_COLS };

        let (hist_skip, n_cols) = if st.extended { (2, WINDOW_COLS - 2) } else { (0, WINDOW_COLS) };
        let bw = self.bandwidths(st, frame.invf);

        // Zero the bands above the last patch in the columns about to be regenerated.
        for col in first_col..zero_end {
            rows.re[BASE + col][self.tgt_max..].fill(0.0);
            rows.im[BASE + col][self.tgt_max..].fill(0.0);
        }

        snap.clone_from(rows);

        let region_a = first_col..(first_col + DELAY).min(WINDOW_COLS);
        let region_b = (first_col + DELAY)..c1;
        let region_c = c1..(c1 + DELAY);

        for k in self.src_min..self.src_max {
            let cov = covariance(snap, BASE + hist_skip, n_cols, k);
            let coefs = coefficients(&cov);

            for (pi, patch) in self.patches[..self.n_patches].iter().enumerate() {
                if !(patch.src_lo <= k && k < patch.src_hi) {
                    continue;
                }
                let t = k + patch.shift;
                if t >= BANDS {
                    continue;
                }

                // Region A: cross-fade from the previous frame's filter to this frame's.
                if st.extended {
                    let bwp = st.prev_bw[pi];
                    let c0r = r(bwp * snap.re[BASE - 2][k]);
                    let c0i = r(bwp * snap.im[BASE - 2][k]);
                    let bsq = r(bwp * bwp);
                    let c1r = r(bsq * snap.re[BASE - 1][k]);
                    let c1i = r(bsq * snap.im[BASE - 1][k]);
                    let prev = Coefs { a0r: c0r, a0i: c0i, a1r: c1r, a1i: c1i };
                    for (i, col) in region_a.clone().enumerate() {
                        fade_out(snap, rows, BASE + col, (k, t), &prev, bsq > 0.0, fall(i));
                    }
                }

                let b = bw[pi];
                let bsq = r(b * b);
                let cur = Coefs {
                    a0r: r(b * coefs.a0r),
                    a0i: r(b * coefs.a0i),
                    a1r: r(bsq * coefs.a1r),
                    a1i: r(bsq * coefs.a1i),
                };

                if bsq > 0.0 {
                    for (i, col) in region_a.clone().enumerate() {
                        let row = BASE + col;
                        let (fr, fi) = filter(snap, row, k, &cur);
                        let w = f64::from(CROSSFADE_RISE[i]);
                        rows.re[row][t] = r(w * fr + rows.re[row][t]);
                        rows.im[row][t] = r(rows.im[row][t] + fi * w);
                    }
                }

                // Region B: the plain filter.
                for col in region_b.clone() {
                    let row = BASE + col;
                    if bsq > 0.0 {
                        let (fr, fi) = filter(snap, row, k, &cur);
                        rows.re[row][t] = r(fr + snap.re[row][k]);
                        rows.im[row][t] = r(fi + snap.im[row][k]);
                    }
                    else {
                        rows.re[row][t] = snap.re[row][k];
                        rows.im[row][t] = snap.im[row][k];
                    }
                }

                // Region C: fade the filter out over the look-ahead columns.
                if !extended {
                    for (i, col) in region_c.clone().enumerate() {
                        fade_out(snap, rows, BASE + col, (k, t), &cur, bsq > 0.0, fall(i));
                    }
                }
            }

            // Park the coefficients for the next frame's cross-fade.
            if extended {
                rows.re[BASE - 2][k] = coefs.a0r;
                rows.im[BASE - 2][k] = coefs.a0i;
                rows.re[BASE - 1][k] = coefs.a1r;
                rows.im[BASE - 1][k] = coefs.a1i;
            }
        }

        st.extended = extended;
        st.prev_bw = bw;
        st.prev_invf = frame.invf;
    }
}

/// `alpha0 x[t-1] + alpha1 x[t-2]` of band `k` in `row` (real, imaginary).
#[inline(always)]
fn filter(s: &Rows, row: usize, k: usize, c: &Coefs) -> (f64, f64) {
    let (x1r, x1i) = (s.re[row - 1][k], s.im[row - 1][k]);
    let (x2r, x2i) = (s.re[row - 2][k], s.im[row - 2][k]);
    let fr = (c.a1r * x2r - c.a1i * x2i) + (c.a0r * x1r - c.a0i * x1i);
    let fi = (c.a0i * x1r + c.a0r * x1i) + (c.a1i * x2r + c.a1r * x2i);
    (fr, fi)
}

/// The falling cross-fade window `W_fall[i]`.
#[inline(always)]
fn fall(i: usize) -> f64 {
    f64::from(CROSSFADE_RISE[DELAY - 1 - i])
}

/// `y = x + w F(x)`, or the plain copy `y = x` if the filter is disabled. `(k, t)` are the
/// source and target bands.
#[inline(always)]
fn fade_out(
    snap: &Rows,
    rows: &mut Rows,
    row: usize,
    (k, t): (usize, usize),
    c: &Coefs,
    active: bool,
    w: f64,
) {
    if active {
        let (fr, fi) = filter(snap, row, k, c);
        rows.re[row][t] = r(w * fr + snap.re[row][k]);
        rows.im[row][t] = r(snap.im[row][k] + fi * w);
    }
    else {
        rows.re[row][t] = snap.re[row][k];
        rows.im[row][t] = snap.im[row][k];
    }
}

/// Second-order covariance sums of band `k` over `count` columns starting at row `base`, with
/// the float32 accumulation of the reference (spec section 11.8, step 1).
fn covariance(s: &Rows, base: usize, count: usize, k: usize) -> Covariance {
    let re = |t: isize| s.re[(base as isize + t) as usize][k];
    let im = |t: isize| s.im[(base as isize + t) as usize][k];
    let n = count as isize;

    let (mut a0, mut a1, mut a2, mut a3, mut a4) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for j in 0..n - 1 {
        a0 = r(im(j - 1) * im(j - 1) + re(j - 1) * re(j - 1) + a0);
        a1 = r(re(j) * re(j - 1) + im(j) * im(j - 1) + a1);
        a2 = r(im(j) * re(j - 1) - re(j) * im(j - 1) + a2);
        a3 = r(im(j) * im(j - 2) + re(j) * re(j - 2) + a3);
        a4 = r(im(j) * re(j - 2) - re(j) * im(j - 2) + a4);
    }

    let e1 = r(re(-2) * re(-2) + a0);
    let a7 = r(im(-2) * im(-2) + e1);
    let a5 = r(im(-1) * im(-2) + (a1 + re(-1) * re(-2)));
    let a6 = r(a2 + im(-1) * re(-2) - re(-1) * im(-2));

    a0 = r(re(n - 2) * re(n - 2) + im(n - 2) * im(n - 2) + a0);
    a1 = r(re(n - 1) * re(n - 2) + im(n - 1) * im(n - 2) + a1);
    a2 = r(a2 + (im(n - 1) * re(n - 2) - re(n - 1) * im(n - 2)));
    a3 = r(im(n - 1) * im(n - 3) + re(n - 1) * re(n - 3) + a3);
    a4 = r(a4 + (im(n - 1) * re(n - 3) - re(n - 1) * im(n - 3)));

    let det = r(r(a0 * a7) - DET_GUARD * (a6 * a6 + a5 * a5));

    Covariance { p11: a0, a: a1, b: a2, c: a3, d: a4, e: a5, f: a6, det }
}

/// Solves for `alpha0`, `alpha1` with the float32 intermediate stores and guards of the
/// reference.
fn coefficients(cv: &Covariance) -> Coefs {
    let Covariance { p11, a, b, c, d, e, f, det } = *cv;
    let mut co = Coefs::default();

    if det != 0.0 {
        let inv = f64::from((1.0 / det) as f32);
        co.a1i = r((a * f + b * e - d * p11) * inv);
        co.a1r = r((a * e - b * f - c * p11) * inv);
    }
    if p11 != 0.0 {
        let inv1 = f64::from((1.0 / p11) as f32);
        co.a0i = r(inv1 * -((e * co.a1i + b) - f * co.a1r));
        co.a0r = r(inv1 * -(f * co.a1i + (e * co.a1r + a)));
    }

    let m0 = co.a0i * co.a0i + co.a0r * co.a0r;
    let m1 = co.a1i * co.a1i + co.a1r * co.a1r;
    if m0 >= COEF_LIMIT || m1 >= COEF_LIMIT || m0.is_nan() || m1.is_nan() {
        co = Coefs::default();
    }
    co
}
