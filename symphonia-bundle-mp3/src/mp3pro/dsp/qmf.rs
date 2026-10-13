// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The QMF banks of the mp3PRO SBR decoder (spec sections 11.3 and 11.4).

use std::f64::consts::PI;

use super::tables::QMF_WINDOW;

/// Number of QMF time columns per 576-sample core frame.
pub(super) const COLUMNS: usize = 18;
/// Number of rows of the row array: 2 history rows, 18 frame rows and 6 look-ahead rows.
pub(super) const ROWS: usize = 26;
/// Number of QMF bands at the output rate.
pub(super) const BANDS: usize = 64;
/// First row written by the analysis.
pub(super) const FIRST_ROW: usize = 8;
/// Number of analysis bands.
const ANALYSIS_BANDS: usize = 32;
/// Number of core samples kept from the previous frame by the analysis filter.
pub(super) const HISTORY: usize = 288;
/// Number of core samples per frame.
pub(super) const FRAME_LEN: usize = 576;
/// Number of analysis window samples per column.
const ANALYSIS_TAPS: usize = 320;
/// Number of samples in the analysis input buffer (history followed by the new frame).
pub(super) const ANALYSIS_BUF: usize = HISTORY + FRAME_LEN;
/// Number of output samples of one synthesis call.
pub(super) const OUT_LEN: usize = COLUMNS * BANDS;
/// Number of synthesis window taps.
const SYNTH_TAPS: usize = 640;

/// The complex QMF row array `X[26][64]` of one channel (spec section 11.5).
#[derive(Clone)]
pub(super) struct Rows {
    pub re: [[f64; BANDS]; ROWS],
    pub im: [[f64; BANDS]; ROWS],
}

impl Rows {
    pub fn zeroed() -> Self {
        Rows { re: [[0.0; BANDS]; ROWS], im: [[0.0; BANDS]; ROWS] }
    }
}

/// The constant matrices of the analysis and synthesis banks, shared by all channels.
pub(super) struct QmfTables {
    /// The odd taps of the prototype window as `[block m][j] = w[1 + 2 (64 m + j)]`.
    analysis_window: [[f64; BANDS]; 5],
    /// `2 cos(pi/32 (k + 1/2) (n - 1/4))`, indexed `[n][k]`.
    analysis_cos: [[f64; ANALYSIS_BANDS]; BANDS],
    /// `2 sin(pi/32 (k + 1/2) (n - 1/4))`, indexed `[n][k]`.
    analysis_sin: [[f64; ANALYSIS_BANDS]; BANDS],
    /// `-1/64 cos(pi/64 (k + 1/2) (l + 1/2))`, indexed `[k][l]`.
    synth_cos: [[f64; BANDS]; BANDS],
    /// `-1/64 sin(pi/64 (k + 1/2) (l + 1/2))`, indexed `[k][l]`.
    synth_sin: [[f64; BANDS]; BANDS],
    /// The prototype window.
    window: [f64; SYNTH_TAPS],
}

impl QmfTables {
    pub fn new() -> Box<Self> {
        let mut t = Box::new(QmfTables {
            analysis_window: [[0.0; BANDS]; 5],
            analysis_cos: [[0.0; ANALYSIS_BANDS]; BANDS],
            analysis_sin: [[0.0; ANALYSIS_BANDS]; BANDS],
            synth_cos: [[0.0; BANDS]; BANDS],
            synth_sin: [[0.0; BANDS]; BANDS],
            window: [0.0; SYNTH_TAPS],
        });

        for (dst, &src) in t.window.iter_mut().zip(QMF_WINDOW.iter()) {
            *dst = f64::from(src);
        }

        for m in 0..5 {
            for j in 0..BANDS {
                t.analysis_window[m][j] = f64::from(QMF_WINDOW[1 + 2 * (BANDS * m + j)]);
            }
        }

        for n in 0..BANDS {
            for k in 0..ANALYSIS_BANDS {
                let ang = PI / 32.0 * (k as f64 + 0.5) * (n as f64 - 0.25);
                t.analysis_cos[n][k] = 2.0 * ang.cos();
                t.analysis_sin[n][k] = 2.0 * ang.sin();
            }
        }

        // The -1/64 synthesis gain is a power of two: folding it into the matrices is exact.
        for k in 0..BANDS {
            for l in 0..BANDS {
                let ph = PI / 64.0 * (k as f64 + 0.5) * (l as f64 + 0.5);
                t.synth_cos[k][l] = -ph.cos() / 64.0;
                t.synth_sin[k][l] = -ph.sin() / 64.0;
            }
        }

        t
    }

    /// 32-band complex analysis of the 864-sample buffer `[288 history | 576 new]`.
    ///
    /// Column `t` (0..18) is written to row `FIRST_ROW + t`, bands `0..kx`:
    /// `u[n] = sum_m w[1 + 2 (64 m + 63 - n)] x[m][63 - n]`,
    /// `X[k] = 2 sum_n u[n] exp(+i pi/32 (k + 1/2) (n - 1/4))` (spec section 11.3).
    pub fn analyse(&self, tbuf: &[f64; ANALYSIS_BUF], kx: usize, rows: &mut Rows) {
        let kx = kx.min(ANALYSIS_BANDS);

        for t in 0..COLUMNS {
            let x = &tbuf[32 * t..32 * t + ANALYSIS_TAPS];

            let mut u = [0.0f64; BANDS];
            for j in 0..BANDS {
                let mut s = 0.0;
                for m in 0..5 {
                    s += self.analysis_window[m][j] * x[BANDS * m + j];
                }
                u[BANDS - 1 - j] = s;
            }

            let mut xr = [0.0f64; ANALYSIS_BANDS];
            let mut xi = [0.0f64; ANALYSIS_BANDS];
            for n in 0..BANDS {
                let un = u[n];
                let c = &self.analysis_cos[n];
                let s = &self.analysis_sin[n];
                for k in 0..kx {
                    xr[k] += un * c[k];
                    xi[k] += un * s[k];
                }
            }

            rows.re[FIRST_ROW + t][..kx].copy_from_slice(&xr[..kx]);
            rows.im[FIRST_ROW + t][..kx].copy_from_slice(&xi[..kx]);
        }
    }
}

/// The 64-band synthesis filter bank state of one channel: the overlap-add buffer.
pub(super) struct Synth {
    buf: [f64; OUT_LEN + SYNTH_TAPS],
}

impl Synth {
    pub fn new() -> Self {
        Synth { buf: [0.0; OUT_LEN + SYNTH_TAPS] }
    }

    /// Synthesises the 18 columns held in rows `2..20` of `rows` into 1152 samples.
    ///
    /// Per column, `V[l] = -1/64 Re sum_k X_k exp(+i pi/128 (2k + 1)(l + 1/2))` is evaluated through a
    /// DCT-IV of the real part and a DST-IV of the imaginary part, and accumulated through the
    /// window into the overlap-add buffer (spec section 11.4).
    pub fn run(&mut self, tables: &QmfTables, rows: &Rows, out: &mut [f64; OUT_LEN]) {
        let buf = &mut self.buf;
        buf.copy_within(0..SYNTH_TAPS, OUT_LEN);
        let mut p = OUT_LEN;

        for c in 0..COLUMNS {
            let re = &rows.re[2 + c];
            let im = &rows.im[2 + c];

            let mut cr = [0.0f64; BANDS];
            let mut si = [0.0f64; BANDS];
            for k in 0..BANDS {
                let (r, i) = (re[k], im[k]);
                let dc = &tables.synth_cos[k];
                let ds = &tables.synth_sin[k];
                for l in 0..BANDS {
                    cr[l] += r * dc[l];
                    si[l] += i * ds[l];
                }
            }

            // v[i] = V[127 - i].
            let mut v = [0.0f64; 2 * BANDS];
            for i in 0..32 {
                v[127 - i] = cr[i] - si[i];
                v[i] = -(si[i] + cr[i]);
                v[64 + i] = cr[63 - i] - si[63 - i];
                v[63 - i] = -(cr[63 - i] + si[63 - i]);
            }

            for m in 0..5 {
                let dst = &mut buf[p + 128 * m..p + 128 * m + 128];
                let w = &tables.window[128 * m..128 * m + 128];
                for ((d, &w), &v) in dst.iter_mut().zip(w).zip(v.iter()) {
                    *d += w * v;
                }
            }

            for i in 0..BANDS {
                out[BANDS * c + BANDS - 1 - i] = buf[p + 576 + i];
            }

            p -= BANDS;
            buf[p..p + BANDS].fill(0.0);
        }
    }
}
