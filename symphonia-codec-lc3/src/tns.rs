// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Temporal noise shaping: a port of the decoding side of liblc3 `src/tns.c` (ETSI TS 103 634
//! clause 3.4.7 "TNS decoder").

use crate::bits::Bits;
use crate::common::{self, BANDWIDTH_FB, BANDWIDTH_SWB, DT_5M};
use crate::tables::{TNS_COEFFS_MODELS, TNS_ORDER_MODELS};

/// Bitstream data of the temporal noise shaping (`struct lc3_tns_data`).
#[derive(Copy, Clone, Default)]
pub struct TnsData {
    pub nfilters: usize,
    pub rc_order: [usize; 2],
    pub rc: [[i32; 8]; 2],
}

/// Resolves the LPC weighting indication according to the bitrate (`resolve_lpc_weighting`).
fn resolve_lpc_weighting(dt: usize, nbytes: usize) -> bool {
    nbytes * 8 < 120 * (1 + dt)
}

/// Gets the bitstream data (`lc3_tns_get_data`). Returns `None` on an invalid bitstream.
pub fn get_data(bits: &mut Bits<'_>, dt: usize, bw: usize, nbytes: usize) -> Option<TnsData> {
    let mut data = TnsData {
        nfilters: 1 + usize::from(dt >= DT_5M && bw >= BANDWIDTH_SWB),
        ..TnsData::default()
    };
    let lpc_weighting = usize::from(resolve_lpc_weighting(dt, nbytes));

    for f in 0..data.nfilters {
        data.rc_order[f] = bits.get_bit() as usize;
        if data.rc_order[f] == 0 {
            continue;
        }

        data.rc_order[f] += bits.get_symbol(&TNS_ORDER_MODELS[lpc_weighting]);
        // The reference does not check the order against the 8 coefficients of a filter.
        if (dt <= DT_5M && data.rc_order[f] > 4) || data.rc_order[f] > 8 {
            return None;
        }

        for i in 0..data.rc_order[f] {
            data.rc[f][i] = bits.get_symbol(&TNS_COEFFS_MODELS[i]) as i32 - 8;
        }
    }

    Some(data)
}

/// Unquantizes the reflection coefficients (`unquantize_rc`).
fn unquantize_rc(rc_q: &[i32; 8], order: usize, rc: &mut [f32; 8]) {
    // Quantization table, sin(delta * i), delta = Pi / 17,
    // rounded to fixed point Q15 value (LC3-Plus HR requirements).
    const Q_INV: [f32; 9] = [
        0x0000 as f32 / 32768.0,
        0x1785 as f32 / 32768.0,
        0x2e3d as f32 / 32768.0,
        0x4362 as f32 / 32768.0,
        0x563c as f32 / 32768.0,
        0x6625 as f32 / 32768.0,
        0x7295 as f32 / 32768.0,
        0x7b1d as f32 / 32768.0,
        0x7f74 as f32 / 32768.0,
    ];

    for i in 0..order {
        let rc_m = Q_INV[rc_q[i].unsigned_abs() as usize];
        rc[i] = if rc_q[i] < 0 { -rc_m } else { rc_m };
    }
}

/// Inverse filtering (`inverse_filtering`).
fn inverse_filtering(
    dt: usize,
    bw: usize,
    rc_order: &[usize; 2],
    rc: &[[f32; 8]; 2],
    x: &mut [f32],
) {
    let nfilters = 1 + usize::from(dt >= DT_5M && bw >= BANDWIDTH_SWB);
    let nf = common::ne(dt, bw.min(BANDWIDTH_FB)) >> (nfilters - 1);
    let mut ie = 3 * (1 + dt);

    let mut s = [0.0f32; 8];

    for f in 0..nfilters {
        let i0 = ie;
        ie = nf * (1 + f);

        if rc_order[f] == 0 {
            continue;
        }

        for xi in x[i0..ie].iter_mut() {
            let mut v = *xi;

            v -= s[7] * rc[f][7];
            for k in (0..7).rev() {
                v -= s[k] * rc[f][k];
                s[k + 1] = s[k] + rc[f][k] * v;
            }
            s[0] = v;
            *xi = v;
        }

        for sk in s[rc_order[f]..].iter_mut() {
            *sk = 0.0;
        }
    }
}

/// TNS synthesis (`lc3_tns_synthesize`). `x` holds the spectral coefficients, filtered in place.
pub fn synthesize(dt: usize, bw: usize, data: &TnsData, x: &mut [f32]) {
    let mut rc = [[0.0f32; 8]; 2];

    for f in 0..data.nfilters {
        if data.rc_order[f] != 0 {
            unquantize_rc(&data.rc[f], data.rc_order[f], &mut rc[f]);
        }
    }

    inverse_filtering(dt, bw, &data.rc_order, &rc, x);
}
