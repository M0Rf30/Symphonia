// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The linear prediction coefficients of the USAC LPD coder (ISO/IEC 23003-3 7.14.1 and 7.14.3):
//! the dequantisation of the line spectral frequencies, their conversion to prediction
//! coefficients and the interpolation between frames.
//!
//! Derived from libxaac (`ixheaacd_lpc_dec.c`, `ixheaacd_tcx_fwd_mdct.c`, Apache-2.0).

use super::avq::rotated_gosset_mtx_dec;
use super::tables::{DICO_LSF_ABS_8B, WEIGHT_TABLE_AVQ};

/// The order of the linear prediction filter.
pub const ORDER: usize = 16;

const LSF_GAP: f32 = 50.0;
const FREQ_MAX: f32 = 6400.0;
const FREQ_DIV: f32 = 400.0;

const FACTOR_TABLE: [f32; 4] = [60.0, 65.0, 64.0, 63.0];

/// The weights of the AVQ refinement of the line spectral frequencies `lsfq`.
fn lsf_weight_2st(lsfq: &[f32; ORDER], mode: usize) -> [f32; ORDER] {
    let mut d = [0.0f32; ORDER + 1];
    d[0] = lsfq[0];
    d[ORDER] = FREQ_MAX - lsfq[ORDER - 1];

    for i in 1..ORDER {
        d[i] = lsfq[i] - lsfq[i - 1];
    }

    let mut w = [0.0f32; ORDER];

    for i in 0..ORDER {
        w[i] = (f64::from(FACTOR_TABLE[mode]) * f64::from(d[i] * d[i + 1]).sqrt()
            / f64::from(FREQ_DIV)) as f32;
    }

    w
}

/// Read the next quantiser index, or zero past the end of the (malformed) index list.
fn at(indx: &[i32], pos: usize) -> i32 {
    indx.get(pos).copied().unwrap_or(0)
}

/// The two RE8 vectors of a refinement: returns the 16 integers and the indexes consumed.
fn decoding_avq_tool(indx: &[i32]) -> ([i32; ORDER], usize) {
    let mut nvecq = [0i32; ORDER];
    let mut position = 2;

    for k in 0..2 {
        let qn = at(indx, k);
        let mut code_book_idx = 0;
        let mut kv = [0i32; 8];

        if qn > 0 {
            code_book_idx = at(indx, position);
            position += 1;

            for v in &mut kv {
                *v = at(indx, position);
                position += 1;
            }
        }

        let b = rotated_gosset_mtx_dec(qn, code_book_idx as u32, &kv);
        nvecq[k * 8..k * 8 + 8].copy_from_slice(&b);
    }

    (nvecq, position)
}

/// The absolute (first stage codebook) approximation of a line spectral frequency vector.
fn first_approx_abs(indx: &[i32]) -> ([f32; ORDER], usize) {
    let stage1 = (at(indx, 0).clamp(0, 255)) as usize;

    let mut lsf = [0.0f32; ORDER];
    lsf.copy_from_slice(&DICO_LSF_ABS_8B[stage1 * ORDER..stage1 * ORDER + ORDER]);

    let w = &WEIGHT_TABLE_AVQ[stage1 * ORDER..stage1 * ORDER + ORDER];

    let (avq, used) = decoding_avq_tool(indx.get(1..).unwrap_or(&[]));

    let mut lsf_min = LSF_GAP;

    for i in 0..ORDER {
        lsf[i] += w[i] * avq[i] as f32;

        if lsf[i] < lsf_min {
            lsf[i] = lsf_min;
        }

        lsf_min = lsf[i] + LSF_GAP;
    }

    lsf_min = FREQ_MAX - LSF_GAP;

    for i in (0..ORDER).rev() {
        if lsf[i] > lsf_min {
            lsf[i] = lsf_min;
        }

        lsf_min = lsf[i] - LSF_GAP;
    }

    (lsf, 1 + used)
}

/// The relative approximation: `lsf` holds the prediction on entry and the result on exit.
fn first_approx_rel(lsf: &mut [f32; ORDER], indx: &[i32], mode: usize) -> usize {
    let w = lsf_weight_2st(lsf, mode);
    let (avq, used) = decoding_avq_tool(indx);

    let mut lsf_min = LSF_GAP;

    for i in 0..ORDER {
        lsf[i] += w[i] * avq[i] as f32;

        if lsf[i] < lsf_min {
            lsf[i] = lsf_min;
        }

        lsf_min = lsf[i] + LSF_GAP;
    }

    used
}

/// The line spectral frequencies of the five LPC sets of a frame: set 4 is the end of the frame,
/// set 0 its beginning (the end of the previous frame, coded only for the first LPD frame), 2 the
/// middle and 1 and 3 the quarters.
///
/// `lsf[0]` holds the end of the previous frame on entry unless `first_lpd_flag` is set.
pub fn alg_vec_dequant(
    idx: &[i32],
    first_lpd_flag: bool,
    lsf: &mut [[f32; ORDER]; 5],
    mod_: &[i32; 4],
) {
    let mut pos = 0;

    let (l4, used) = first_approx_abs(&idx[pos.min(idx.len())..]);
    lsf[4] = l4;
    pos += used;

    if first_lpd_flag {
        let mode_lpc = at(idx, pos);
        pos += 1;

        if mode_lpc == 0 {
            let (l, used) = first_approx_abs(idx.get(pos..).unwrap_or(&[]));
            lsf[0] = l;
            pos += used;
        }
        else if mode_lpc == 1 {
            lsf[0] = lsf[4];
            pos += first_approx_rel(&mut lsf[0], idx.get(pos..).unwrap_or(&[]), 3);
        }
    }

    if mod_[0] < 3 {
        let mode_lpc = at(idx, pos);
        pos += 1;

        if mode_lpc == 0 {
            let (l, used) = first_approx_abs(idx.get(pos..).unwrap_or(&[]));
            lsf[2] = l;
            pos += used;
        }
        else if mode_lpc == 1 {
            lsf[2] = lsf[4];
            pos += first_approx_rel(&mut lsf[2], idx.get(pos..).unwrap_or(&[]), 3);
        }
    }

    if mod_[0] < 2 {
        let mode_lpc = at(idx, pos);
        pos += 1;

        if mode_lpc == 1 {
            for i in 0..ORDER {
                lsf[1][i] = 0.5 * (lsf[0][i] + lsf[2][i]);
            }
        }
        else if mode_lpc == 0 {
            let (l, used) = first_approx_abs(idx.get(pos..).unwrap_or(&[]));
            lsf[1] = l;
            pos += used;
        }
        else if mode_lpc == 2 {
            lsf[1] = lsf[2];
            pos += first_approx_rel(&mut lsf[1], idx.get(pos..).unwrap_or(&[]), 2);
        }
    }

    if mod_[2] < 2 {
        let mode_lpc = at(idx, pos);
        pos += 1;

        if mode_lpc == 0 {
            let (l, used) = first_approx_abs(idx.get(pos..).unwrap_or(&[]));
            lsf[3] = l;
            let _ = used;
        }
        else if mode_lpc == 1 {
            for i in 0..ORDER {
                lsf[3][i] = 0.5 * (lsf[2][i] + lsf[4][i]);
            }
            first_approx_rel(&mut lsf[3], idx.get(pos..).unwrap_or(&[]), 1);
        }
        else if mode_lpc == 2 {
            lsf[3] = lsf[2];
            first_approx_rel(&mut lsf[3], idx.get(pos..).unwrap_or(&[]), 2);
        }
        else if mode_lpc == 3 {
            lsf[3] = lsf[4];
            first_approx_rel(&mut lsf[3], idx.get(pos..).unwrap_or(&[]), 2);
        }
    }
}

/// `lsp = cos(2 pi lsf / 12800)`.
pub fn lsf_to_lsp(lsf: &[f32; ORDER]) -> [f32; ORDER] {
    let mut lsp = [0.0f32; ORDER];
    let k = core::f64::consts::PI / 6400.0;

    for i in 0..ORDER {
        lsp[i] = (f64::from(lsf[i]) * k).cos() as f32;
    }

    lsp
}

/// The inverse of [`lsf_to_lsp`].
pub fn lsp_to_lsf(lsp: &[f32; ORDER]) -> [f32; ORDER] {
    let mut lsf = [0.0f32; ORDER];
    let k = 6400.0 / core::f64::consts::PI;

    for i in 0..ORDER {
        lsf[i] = (f64::from(lsp[i]).acos() * k) as f32;
    }

    lsf
}

/// The polynomials `F1` and `F2` of the line spectral pairs.
fn compute_coeff_poly(lsp: &[f32; ORDER], f1: &mut [f32], f2: &mut [f32]) {
    f1[0] = 1.0;
    f2[0] = 1.0;

    // `f[i - 2]` is only read for `i >= 2` in the recursion below; for `i == 1` the C code reads
    // the element before the array start, which is the zero the caller keeps there.
    for i in 1..=ORDER / 2 {
        let b1 = -2.0 * lsp[2 * (i - 1)];
        let b2 = -2.0 * lsp[2 * (i - 1) + 1];

        let (p1, p2) = if i >= 2 { (f1[i - 2], f2[i - 2]) } else { (0.0, 0.0) };

        f1[i] = b1 * f1[i - 1] + 2.0 * p1;
        f2[i] = b2 * f2[i - 1] + 2.0 * p2;

        for j in (1..i).rev() {
            let (q1, q2) = if j >= 2 { (f1[j - 2], f2[j - 2]) } else { (0.0, 0.0) };
            f1[j] += b1 * f1[j - 1] + q1;
            f2[j] += b2 * f2[j - 1] + q2;
        }
    }
}

/// The prediction coefficients `A(z) = 1 + a[1] z^-1 + ...` of a line spectral pair vector.
pub fn lsp_to_lp(lsp: &[f32; ORDER]) -> [f32; ORDER + 1] {
    let mut poly1 = [0.0f32; ORDER / 2 + 2];
    let mut poly2 = [0.0f32; ORDER / 2 + 2];

    compute_coeff_poly(lsp, &mut poly1[1..], &mut poly2[1..]);

    for i in 0..ORDER / 2 {
        let a = ORDER / 2 + 1 - i;
        poly1[a] += poly1[a - 1];
        poly2[a] -= poly2[a - 1];
    }

    let mut a = [0.0f32; ORDER + 1];
    a[0] = 1.0;

    for i in 0..ORDER / 2 {
        a[1 + i] = 0.5 * (poly1[2 + i] + poly2[2 + i]);
        a[ORDER - i] = 0.5 * (poly1[2 + i] - poly2[2 + i]);
    }

    a
}

/// The `nb_subfr` interpolated coefficient sets between two line spectral pair vectors followed
/// by the set of `lsp_new`; `out` holds `nb_subfr + 1` sets of `ORDER + 1`.
pub fn interpolation_lsp_params(
    lsp_old: &[f32; ORDER],
    lsp_new: &[f32; ORDER],
    nb_subfr: usize,
    out: &mut [[f32; ORDER + 1]],
) {
    let factor = 1.0 / nb_subfr as f32;
    let mut x_plus_y = 0.5 * factor;

    for k in 0..nb_subfr {
        let x_minus_y = 1.0 - x_plus_y;
        let mut lsp = [0.0f32; ORDER];

        for i in 0..ORDER {
            lsp[i] = lsp_old[i] * x_minus_y + lsp_new[i] * x_plus_y;
        }

        x_plus_y += factor;

        out[k] = lsp_to_lp(&lsp);
    }

    out[nb_subfr] = lsp_to_lp(lsp_new);
}

/// The three coefficient sets of a TCX frame: the interpolation at the middle between the old
/// and new line spectral pairs, the old set and the new set.
pub fn lpc_coef_gen(
    lsp_old: &[f32; ORDER],
    lsp_new: &[f32; ORDER],
    nb_subfr: usize,
) -> [[f32; ORDER + 1]; 3] {
    let inc = 1.0 / nb_subfr as f32;
    let fnew = 0.5 - 0.5 * inc;
    let fold = 1.0 - fnew;

    let mut lsp = [0.0f32; ORDER];

    for i in 0..ORDER {
        lsp[i] = lsp_old[i] * fold + lsp_new[i] * fnew;
    }

    [lsp_to_lp(&lsp), lsp_to_lp(lsp_old), lsp_to_lp(lsp_new)]
}
