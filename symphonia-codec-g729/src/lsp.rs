// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Line spectral frequency (LSF/LSP) handling.
//!
//! Ported from FFmpeg `libavcodec/lsp.c` (the fixed-point routines used by `g729dec.c`).

use crate::tables::TAB_COS;

/// Number of LP coefficients.
pub(crate) const LP_ORDER: usize = 10;

/// Sorts the quantized LSFs and enforces a minimum distance and range (`ff_acelp_reorder_lsf`).
pub(crate) fn reorder_lsf(
    lsfq: &mut [i16; LP_ORDER],
    lsfq_min_distance: i32,
    lsfq_min: i32,
    lsfq_max: i32,
) {
    // Sort in ascending order (bubble sort, O(n) if already sorted).
    for i in 0..LP_ORDER - 1 {
        let mut j = i;
        loop {
            if lsfq[j] > lsfq[j + 1] {
                lsfq.swap(j, j + 1);
            }
            else {
                break;
            }
            if j == 0 {
                break;
            }
            j -= 1;
        }
    }

    let mut min = lsfq_min;
    for v in lsfq.iter_mut() {
        *v = i32::from(*v).max(min) as i16;
        min = i32::from(*v) + lsfq_min_distance;
    }
    lsfq[LP_ORDER - 1] = i32::from(lsfq[LP_ORDER - 1]).min(lsfq_max) as i16;
}

/// Linearly interpolated cosine (`ff_cos`). `arg` is in `[0, 0x3fff]` for valid input; larger
/// values are clamped to the table instead of reading out of range.
fn cos_q15(arg: u16) -> i16 {
    let offset = i32::from(arg & 0xff);
    let ind = usize::from(arg >> 8).min(TAB_COS.len() - 2);

    let c0 = i32::from(TAB_COS[ind]);
    let c1 = i32::from(TAB_COS[ind + 1]);
    (c0 + ((offset * (c1 - c0)) >> 8)) as i16
}

/// Converts LSFs to LSPs, `lsp = cos(lsf)` (`ff_acelp_lsf2lsp`).
pub(crate) fn lsf2lsp(lsp: &mut [i16; LP_ORDER], lsf: &[i16; LP_ORDER]) {
    for (l, &f) in lsp.iter_mut().zip(lsf) {
        // 20861 = 2.0 / PI in (0.15); divide by PI and (0,13) -> (0,14).
        *l = cos_q15(((i32::from(f) * 20861) >> 15) as u16);
    }
}

/// Decodes the polynomial coefficients from the LSPs (`lsp2poly`). `lsp` holds every other LSP
/// of the full set, starting with the one at index `0`.
fn lsp2poly(f: &mut [i32; LP_ORDER / 2 + 1], lsp: &[i16], lp_half_order: usize) {
    f[0] = 0x40_0000; // 1.0 in (3.22)
    f[1] = -i32::from(lsp[0]) * 256; // *2 and (0.15) -> (3.22)

    for i in 2..=lp_half_order {
        let c = i64::from(lsp[2 * i - 2]);
        f[i] = f[i - 2];
        for j in (2..=i).rev() {
            let m = ((i64::from(f[j - 1]) * c) >> 14) as i32;
            f[j] = f[j].wrapping_sub(m.wrapping_sub(f[j - 2]));
        }
        f[1] = f[1].wrapping_sub(i32::from(lsp[2 * i - 2]) * 256);
    }
}

/// LSP to LP conversion (3.2.6 of G.729, `acelp_lsp2lpc`).
fn lsp2lpc(lp: &mut [i16; LP_ORDER + 1], lsp: &[i16; LP_ORDER]) {
    const HALF: usize = LP_ORDER / 2;
    let mut f1 = [0i32; HALF + 1]; // (3.22)
    let mut f2 = [0i32; HALF + 1]; // (3.22)

    lsp2poly(&mut f1, lsp, HALF);
    lsp2poly(&mut f2, &lsp[1..], HALF);

    // 3.2.6 of G.729, equations 25 and 26.
    lp[0] = 4096;
    for i in 1..HALF + 1 {
        let mut ff1 = f1[i].wrapping_add(f1[i - 1]); // (3.22)
        let ff2 = f2[i].wrapping_sub(f2[i - 1]); // (3.22)

        ff1 = ff1.wrapping_add(1 << 10); // for rounding
        lp[i] = (ff1.wrapping_add(ff2) >> 11) as i16; // divide by 2 and (3.22) -> (3.12)
        lp[(HALF << 1) + 1 - i] = (ff1.wrapping_sub(ff2) >> 11) as i16;
    }
}

/// Decodes the LP coefficients of both subframes from the current and previous LSPs
/// (`ff_acelp_lp_decode`, FFmpeg's default rounding of the first-subframe interpolation).
pub(crate) fn lp_decode(
    lp_1st: &mut [i16; LP_ORDER + 1],
    lp_2nd: &mut [i16; LP_ORDER + 1],
    lsp_2nd: &[i16; LP_ORDER],
    lsp_prev: &[i16; LP_ORDER],
) {
    let mut lsp_1st = [0i16; LP_ORDER]; // (0.15)

    // LSP values for the first subframe (3.2.5 of G.729, equation 24).
    for i in 0..LP_ORDER {
        lsp_1st[i] = ((i32::from(lsp_2nd[i]) + i32::from(lsp_prev[i])) >> 1) as i16;
    }

    lsp2lpc(lp_1st, &lsp_1st);

    // LSP values for the second subframe (3.2.5 of G.729).
    lsp2lpc(lp_2nd, lsp_2nd);
}
