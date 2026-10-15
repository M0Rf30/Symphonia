// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! LSP inverse quantisation, interpolation and LSP to LPC conversion.
//!
//! Follows FFmpeg `libavcodec/g723_1.c` (`ff_g723_1_inverse_quant`, `ff_g723_1_lsp_interpolate`,
//! `lsp2lpc`), see ITU-T G.723.1 sections 2.5 - 2.6 and 3.2.

use crate::celp::{clipl32, sat_dadd32, weighted_sum_sample};
use crate::tables::{COS_TAB, DC_LSP, LSP_BAND0, LSP_BAND1, LSP_BAND2};
use crate::{LPC_ORDER, SUBFRAMES};

/// Bit-exact `2ab` scaled by 1/2^16 (`MULL2` in `g723_1.h`).
#[inline]
fn mull2(a: i32, b: i32) -> i32 {
    (a >> 16).wrapping_mul(b).wrapping_mul(2).wrapping_add(((a & 0xffff).wrapping_mul(b)) >> 15)
}

/// Inverse quantises the LSP vector into `cur_lsp`.
///
/// If `bad_frame` is set the indices are zeroed and the prediction is stronger (frame erasure).
/// On a failure to stabilise the vector, `prev_lsp` is reused.
pub fn inverse_quant(
    cur_lsp: &mut [i16; LPC_ORDER],
    prev_lsp: &[i16; LPC_ORDER],
    lsp_index: &mut [u8; 3],
    bad_frame: bool,
) {
    let (min_dist, pred) = if !bad_frame {
        (0x100i32, 12288i32)
    }
    else {
        *lsp_index = [0; 3];
        (0x200, 23552)
    };

    // Get the VQ table entry corresponding to the transmitted index.
    cur_lsp[0..3].copy_from_slice(&LSP_BAND0[usize::from(lsp_index[0])]);
    cur_lsp[3..6].copy_from_slice(&LSP_BAND1[usize::from(lsp_index[1])]);
    cur_lsp[6..10].copy_from_slice(&LSP_BAND2[usize::from(lsp_index[2])]);

    // Add predicted vector & DC component to the previously quantised vector.
    for i in 0..LPC_ORDER {
        let dc = i32::from(DC_LSP[i]);
        let temp = ((i32::from(prev_lsp[i]) - dc) * pred + (1 << 14)) >> 15;
        cur_lsp[i] = (i32::from(cur_lsp[i]) + dc + temp) as i16;
    }

    let mut stable = false;
    for _ in 0..LPC_ORDER {
        cur_lsp[0] = cur_lsp[0].max(0x180);
        cur_lsp[LPC_ORDER - 1] = cur_lsp[LPC_ORDER - 1].min(0x7e00);

        // Stability check.
        for j in 1..LPC_ORDER {
            let temp = min_dist + i32::from(cur_lsp[j - 1]) - i32::from(cur_lsp[j]);
            if temp > 0 {
                let temp = temp >> 1;
                cur_lsp[j - 1] = (i32::from(cur_lsp[j - 1]) - temp) as i16;
                cur_lsp[j] = (i32::from(cur_lsp[j]) + temp) as i16;
            }
        }

        stable = true;
        for j in 1..LPC_ORDER {
            let temp = i32::from(cur_lsp[j - 1]) + min_dist - i32::from(cur_lsp[j]) - 4;
            if temp > 0 {
                stable = false;
                break;
            }
        }
        if stable {
            break;
        }
    }

    if !stable {
        *cur_lsp = *prev_lsp;
    }
}

/// Converts one set of LSP frequencies to LPC coefficients, in place (`lsp2lpc`).
fn lsp2lpc(lpc: &mut [i16]) {
    let mut f1 = [0i32; LPC_ORDER / 2 + 1];
    let mut f2 = [0i32; LPC_ORDER / 2 + 1];

    // Calculate negative cosine.
    for l in lpc.iter_mut().take(LPC_ORDER) {
        let index = ((*l >> 7) & 0x1ff) as usize;
        let offset = i32::from(*l & 0x7f);
        let c0 = i32::from(COS_TAB[index]);
        let c1 = i32::from(COS_TAB[index + 1]);
        let temp1 = c0.wrapping_mul(1 << 16);
        let temp2 = (c1 - c0).wrapping_mul(((offset << 8) + 0x80) << 1);

        *l = (-(sat_dadd32(1 << 15, temp1.wrapping_add(temp2)) >> 16)) as i16;
    }

    let l: [i32; LPC_ORDER] = std::array::from_fn(|i| i32::from(lpc[i]));

    // Compute sum and difference polynomial coefficients (bit-exact alternative to lsp2poly()).
    // Initialise with values in Q28.
    f1[0] = 1 << 28;
    f1[1] = (l[0] + l[2]) * (1 << 14);
    f1[2] = l[0] * l[2] + (2 << 28);

    f2[0] = 1 << 28;
    f2[1] = (l[1] + l[3]) * (1 << 14);
    f2[2] = l[1] * l[3] + (2 << 28);

    // Calculate and scale the coefficients by 1/2 in each iteration for a final scaling
    // factor of Q25.
    for i in 2..LPC_ORDER / 2 {
        f1[i + 1] = clipl32(i64::from(f1[i - 1]) + i64::from(mull2(f1[i], l[2 * i])));
        f2[i + 1] = clipl32(i64::from(f2[i - 1]) + i64::from(mull2(f2[i], l[2 * i + 1])));

        for j in (2..=i).rev() {
            f1[j] =
                mull2(f1[j - 1], l[2 * i]).wrapping_add(f1[j] >> 1).wrapping_add(f1[j - 2] >> 1);
            f2[j] = mull2(f2[j - 1], l[2 * i + 1])
                .wrapping_add(f2[j] >> 1)
                .wrapping_add(f2[j - 2] >> 1);
        }

        f1[0] >>= 1;
        f2[0] >>= 1;
        f1[1] = (((l[2 * i] * 65536) >> i) + f1[1]) >> 1;
        f2[1] = (((l[2 * i + 1] * 65536) >> i) + f2[1]) >> 1;
    }

    // Convert polynomial coefficients to LPC coefficients.
    for i in 0..LPC_ORDER / 2 {
        let ff1 = i64::from(f1[i + 1].wrapping_add(f1[i]));
        let ff2 = i64::from(f2[i + 1].wrapping_sub(f2[i]));

        lpc[i] = (clipl32((ff1 + ff2) * 8 + (1 << 15)) >> 16) as i16;
        lpc[LPC_ORDER - i - 1] = (clipl32((ff1 - ff2) * 8 + (1 << 15)) >> 16) as i16;
    }
}

/// Interpolates the LSP vectors of the four subframes and converts them to LPC coefficients
/// (`ff_g723_1_lsp_interpolate`).
pub fn lsp_interpolate(
    lpc: &mut [i16; SUBFRAMES * LPC_ORDER],
    cur_lsp: &[i16; LPC_ORDER],
    prev_lsp: &[i16; LPC_ORDER],
) {
    // cur_lsp * 0.25 + prev_lsp * 0.75, ... , cur_lsp.
    const WEIGHTS: [(i32, i32); 3] = [(4096, 12288), (8192, 8192), (12288, 4096)];

    for (sub, &(wa, wb)) in lpc.chunks_exact_mut(LPC_ORDER).zip(&WEIGHTS) {
        for i in 0..LPC_ORDER {
            sub[i] = weighted_sum_sample(cur_lsp[i], prev_lsp[i], wa, wb, 1 << 13, 14);
        }
    }
    lpc[3 * LPC_ORDER..].copy_from_slice(cur_lsp);

    for sub in lpc.chunks_exact_mut(LPC_ORDER) {
        lsp2lpc(sub);
    }
}
