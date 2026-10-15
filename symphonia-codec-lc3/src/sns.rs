// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Spectral noise shaping: a port of the decoding side of liblc3 `src/sns.c` (ETSI TS 103 634
//! clause 3.4.8 "SNS decoder").

use crate::bits::Bits;
use crate::fastmath::exp2f;
use crate::tables::{self, SNS_DCT16_M, SNS_HFCB, SNS_LFCB, SNS_MPVQ_OFFSETS};

/// Maximum number of bands (`LC3_MAX_BANDS`).
const MAX_BANDS: usize = 64;

/// Bitstream data of the spectral noise shaping (`struct lc3_sns_data`).
#[derive(Copy, Clone, Default)]
pub struct SnsData {
    lfcb: usize,
    hfcb: usize,
    shape: usize,
    gain: usize,
    idx_a: i32,
    idx_b: i32,
    ls_a: bool,
    ls_b: bool,
}

/// Quantization gains of the shape vectors (`lc3_sns_vq_gains`).
const VQ_GAINS: [&[f32]; 4] = [
    &[8915.0 / 4096.0, 12054.0 / 4096.0],
    &[6245.0 / 4096.0, 15043.0 / 4096.0, 17861.0 / 4096.0, 21014.0 / 4096.0],
    &[7099.0 / 4096.0, 9132.0 / 4096.0, 11253.0 / 4096.0, 14808.0 / 4096.0],
    &[
        4336.0 / 4096.0,
        5067.0 / 4096.0,
        5895.0 / 4096.0,
        8149.0 / 4096.0,
        10235.0 / 4096.0,
        12825.0 / 4096.0,
        16868.0 / 4096.0,
        19882.0 / 4096.0,
    ],
];

/// Inverse DCT-16 transformation (`dct16_inverse`).
fn dct16_inverse(x: &[f32; 16], y: &mut [f32; 16]) {
    for i in 0..16 {
        y[i] = 0.0;
        for j in 0..16 {
            y[i] += x[j] * SNS_DCT16_M[i][j];
        }
    }
}

/// Gets the bitstream data (`lc3_sns_get_data`). Returns `None` on an invalid bitstream.
pub fn get_data(bits: &mut Bits<'_>) -> Option<SnsData> {
    // Codebooks.
    let mut data = SnsData {
        lfcb: bits.get_bits(5) as usize,
        hfcb: bits.get_bits(5) as usize,
        ..SnsData::default()
    };

    // Shape, gain and vectors.
    let shape_msb = bits.get_bit();
    data.gain = bits.get_bits(1 + shape_msb as i32) as usize;
    data.ls_a = bits.get_bit() != 0;

    let mut mux_code = bits.get_bits(25 - shape_msb as i32) as i32;

    if shape_msb == 0 {
        const SIZE_A: i32 = 2390004;

        if mux_code >= SIZE_A * 14 {
            return None;
        }

        data.idx_a = mux_code % SIZE_A;
        mux_code /= SIZE_A;

        data.shape = usize::from(mux_code < 2);

        if data.shape == 0 {
            data.idx_b = (mux_code - 2) / 2;
            data.ls_b = (mux_code - 2) % 2 != 0;
        }
        else {
            data.gain = (data.gain << 1) + (mux_code % 2) as usize;
        }
    }
    else {
        const SIZE_A: i32 = 15158272;

        if mux_code >= SIZE_A + 1549824 {
            return None;
        }

        data.shape = 2 + usize::from(mux_code >= SIZE_A);
        if data.shape == 2 {
            data.idx_a = mux_code;
        }
        else {
            mux_code -= SIZE_A;
            data.idx_a = mux_code / 2;
            data.gain = (data.gain << 1) + (mux_code % 2) as usize;
        }
    }

    Some(data)
}

/// Sub-procedure of `deenumerate`, the deenumeration of a vector (`deenum_mvpq`).
fn deenum_mvpq(mut idx: i32, mut ls: bool, mut npulses: usize, c: &mut [i32]) {
    let n = c.len();
    let mut o = 0;
    let mut i = n as i32 - 1;

    // Scan for coefficients.
    while i >= 0 && idx != 0 {
        let mut ci = 0;

        while idx < SNS_MPVQ_OFFSETS[i as usize][npulses - ci] {
            ci += 1;
        }
        idx -= SNS_MPVQ_OFFSETS[i as usize][npulses - ci];

        c[o] = if ls { -(ci as i32) } else { ci as i32 };
        o += 1;
        npulses -= ci;
        if ci > 0 {
            ls = idx & 1 != 0;
            idx >>= 1;
        }

        i -= 1;
    }

    // Set last significant.
    let ci = npulses as i32;

    if i >= 0 {
        c[o] = if ls { -ci } else { ci };
        o += 1;
    }
    i -= 1;

    while i >= 0 {
        c[o] = 0;
        o += 1;
        i -= 1;
    }
}

/// Deenumerates the PVQ configuration (`deenumerate`).
fn deenumerate(data: &SnsData, c: &mut [i32; 16]) {
    let npulses_a = [10, 10, 8, 6][data.shape];

    let na = if data.shape < 2 { 10 } else { 16 };
    deenum_mvpq(data.idx_a, data.ls_a, npulses_a, &mut c[..na]);

    if data.shape == 0 {
        deenum_mvpq(data.idx_b, data.ls_b, 1, &mut c[10..16]);
    }
    else if data.shape == 1 {
        c[10..16].fill(0);
    }
}

/// Normalizes the pulse configuration to a unit energy (`normalize`). Returns `false` if the
/// configuration has no pulse, which the reference turns into NaN values.
fn normalize(c: &[i32; 16], cn: &mut [f32; 16]) -> bool {
    let c2_sum: i32 = c.iter().map(|&ci| ci.wrapping_mul(ci)).fold(0, i32::wrapping_add);
    if c2_sum <= 0 {
        return false;
    }

    let c_norm = 1.0 / (c2_sum as f32).sqrt();

    for i in 0..16 {
        cn[i] = c[i] as f32 * c_norm;
    }

    true
}

/// Unquantizes the scale factors (`unquantize`).
fn unquantize(data: &SnsData, c: &[f32; 16], scf: &mut [f32; 16]) {
    let lfcb = &SNS_LFCB[data.lfcb];
    let hfcb = &SNS_HFCB[data.hfcb];
    let g = VQ_GAINS[data.shape][data.gain];

    dct16_inverse(c, scf);

    for i in 0..8 {
        scf[i] = lfcb[i] + g * scf[i];
    }

    for i in 8..16 {
        scf[i] = hfcb[i - 8] + g * scf[i];
    }
}

/// Spectral shaping, inverse (`spectral_shaping` with `inv = true`).
fn spectral_shaping(dt: usize, sr: usize, scf_q: &[f32; 16], x: &[f32], y: &mut [f32]) -> bool {
    let Some(lim) = tables::band_lim(dt, sr)
    else {
        return false;
    };

    // Interpolate scale factors.
    let mut scf = [0.0f32; MAX_BANDS];
    let mut s1 = -scf_q[0];

    scf[0] = s1;
    scf[1] = s1;
    for i in 0..15 {
        let s0 = s1;
        s1 = -scf_q[i + 1];
        scf[4 * i + 2] = s0 + 0.125 * (s1 - s0);
        scf[4 * i + 3] = s0 + 0.375 * (s1 - s0);
        scf[4 * i + 4] = s0 + 0.625 * (s1 - s0);
        scf[4 * i + 5] = s0 + 0.875 * (s1 - s0);
    }
    let s0 = -scf_q[14];
    scf[62] = s1 + 0.125 * (s1 - s0);
    scf[63] = s1 + 0.375 * (s1 - s0);

    let nb = lim.len() - 1;
    let n4 = if nb < 32 { 32 % nb } else { 0 };
    let n2 = if nb < 32 { nb - n4 } else { MAX_BANDS - nb };

    for i4 in 0..n4 {
        scf[i4] = 0.25 * (scf[4 * i4] + scf[4 * i4 + 1] + scf[4 * i4 + 2] + scf[4 * i4 + 3]);
    }

    for i2 in n4..n4 + n2 {
        scf[i2] = 0.5 * (scf[2 * (n4 + i2)] + scf[2 * (n4 + i2) + 1]);
    }

    scf.copy_within(4 * n4 + 2 * n2..4 * n4 + 2 * n2 + (nb - n4 - n2), n4 + n2);

    // Spectral shaping.
    let mut i = 0;
    for ib in 0..nb {
        let g_sns = exp2f(-scf[ib]);

        while i < usize::from(lim[ib + 1]) {
            y[i] = x[i] * g_sns;
            i += 1;
        }
    }

    true
}

/// SNS synthesis (`lc3_sns_synthesize`). Shapes `x` into `y`. Returns `false` if the frame is
/// invalid.
pub fn synthesize(dt: usize, sr: usize, data: &SnsData, x: &[f32], y: &mut [f32]) -> bool {
    let mut c = [0i32; 16];
    let mut cn = [0.0f32; 16];
    let mut scf = [0.0f32; 16];

    deenumerate(data, &mut c);

    if !normalize(&c, &mut cn) {
        return false;
    }

    unquantize(data, &cn, &mut scf);

    spectral_shaping(dt, sr, &scf, x, y)
}
