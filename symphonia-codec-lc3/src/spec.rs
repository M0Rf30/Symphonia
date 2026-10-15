// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Spectral data decoding: a port of the decoding side of liblc3 `src/spec.c` (ETSI TS 103 634
//! clause 3.4.5 and 3.4.6 "Arithmetic and residual decoding", "Noise filling").

use crate::bits::Bits;
use crate::common::{self, BANDWIDTH_FB, DT_7M5, DT_10M};
use crate::tables::{SPECTRUM_LOOKUP, SPECTRUM_MODELS};

/// Spectral quantization side data (`struct lc3_spec_side`).
#[derive(Copy, Clone, Default)]
pub struct SpecSide {
    pub g_idx: i32,
    pub nq: usize,
    pub lsb_mode: bool,
}

/// Returns the number of bits coding the number of coded coefficients (`get_nbits_nq`).
fn get_nbits_nq(dt: usize, sr: usize) -> i32 {
    let ne = common::ne(dt, sr);
    4 + i32::from(ne > 32)
        + i32::from(ne > 64)
        + i32::from(ne > 128)
        + i32::from(ne > 256)
        + i32::from(ne > 512)
}

/// Gets the spectral quantization side data (`lc3_spec_get_side`). Returns `None` on an invalid
/// bitstream.
pub fn get_side(bits: &mut Bits<'_>, dt: usize, sr: usize) -> Option<SpecSide> {
    let nbits_nq = get_nbits_nq(dt, sr);
    let ne = common::ne(dt, sr);

    let nq = ((bits.get_bits(nbits_nq) as usize) + 1) << 1;
    let lsb_mode = bits.get_bit() != 0;
    let g_idx = bits.get_bits(8) as i32;

    if nq > ne { None } else { Some(SpecSide { g_idx, nq, lsb_mode }) }
}

/// Resolves the quantized gain index offset (`resolve_gain_offset`).
fn resolve_gain_offset(sr: usize, nbytes: usize) -> i32 {
    let sr_ind = if common::is_hr(sr) { 4 + (sr - common::SRATE_48K_HR) } else { sr } as i32;

    let g_off = (nbytes as i32 * 8) / (10 * (1 + sr_ind));
    (if sr >= common::SRATE_96K_HR { 181 } else { 255 })
        .min(105 + 5 * (1 + sr_ind) + g_off.min(115))
}

/// Unquantizes the gain (`unquantize_gain`).
fn unquantize_gain(mut g_int: i32) -> f32 {
    // Unquantization gain table :
    // G[i] = 10 ^ (i / 28) , i = [0..27]
    static IQ_TABLE: [f32; 28] = [
        1.00000000e+00,
        1.08571112e+00,
        1.17876863e+00,
        1.27980221e+00,
        1.38949549e+00,
        1.50859071e+00,
        1.63789371e+00,
        1.77827941e+00,
        1.93069773e+00,
        2.09617999e+00,
        2.27584593e+00,
        2.47091123e+00,
        2.68269580e+00,
        2.91263265e+00,
        3.16227766e+00,
        3.43332002e+00,
        3.72759372e+00,
        4.04708995e+00,
        4.39397056e+00,
        4.77058270e+00,
        5.17947468e+00,
        5.62341325e+00,
        6.10540230e+00,
        6.62870316e+00,
        7.19685673e+00,
        7.81370738e+00,
        8.48342898e+00,
        9.21055318e+00,
    ];

    let mut g = 1.0f32;

    while g_int < 0 {
        g_int += 28;
        g *= 0.1;
    }
    while g_int >= 28 {
        g_int -= 28;
        g *= 10.0;
    }

    g * IQ_TABLE[g_int as usize]
}

/// Resolves the high-bitrate mode according to the size of the frame (`resolve_modes`).
fn resolve_high_rate(sr: usize, nbytes: usize) -> bool {
    let sr_ind = if common::is_hr(sr) { 4 + (sr - common::SRATE_48K_HR) } else { sr };

    nbytes > 20 * (1 + sr_ind) && sr < common::SRATE_96K_HR
}

/// Reads the quantized spectrum (`get_quantized`). Returns the noise factor seed, or `None` on
/// an invalid bitstream.
fn get_quantized(
    bits: &mut Bits<'_>,
    dt: usize,
    sr: usize,
    nbytes: usize,
    nq: usize,
    lsb_mode: bool,
    x: &mut [f32],
) -> Option<u32> {
    let high_rate = resolve_high_rate(sr, nbytes);
    let ne = common::ne(dt, sr);

    let mut nf_seed: u32 = 0;

    // Loop on quantized coefficients.
    let mut state: u8 = 0;
    let mut i = 0;

    for h in 0..2 {
        let lut_coeff = &SPECTRUM_LOOKUP[usize::from(high_rate)][h];

        while i < nq.min((ne + 2) >> (1 - h)) {
            let lut = &lut_coeff[usize::from(state)];
            let max_shl = if common::is_hr(sr) { 22 } else { 14 };

            // LSB values
            // Until the symbol read indicates the escape value 16, read an LSB bit for each
            // values. The LSB mode discard the first LSB (at this step).

            let mut u: u32 = 0;
            let mut v: u32 = 0;
            let mut k = 0;
            let mut shl = 0;

            let mut s = bits.get_symbol(&SPECTRUM_MODELS[usize::from(lut[k])]);

            if lsb_mode && s >= 16 {
                k += 1;
                s = bits.get_symbol(&SPECTRUM_MODELS[usize::from(lut[k])]);
                shl += 1;
            }

            while s >= 16 && shl < max_shl {
                u |= bits.get_bit() << shl;
                v |= bits.get_bit() << shl;

                k += usize::from(k < 3);
                s = bits.get_symbol(&SPECTRUM_MODELS[usize::from(lut[k])]);
                shl += 1;
            }

            if s >= 16 {
                return None;
            }

            // MSB & sign values
            let a = (s % 4) as u32;
            let b = (s / 4) as u32;

            u |= a << shl;
            v |= b << shl;

            x[i] = if u != 0 && bits.get_bit() != 0 { -(u as f32) } else { u as f32 };
            x[i + 1] = if v != 0 && bits.get_bit() != 0 { -(v as f32) } else { v as f32 };

            nf_seed =
                (nf_seed + (u & 0x7fff) * (i as u32) + (v & 0x7fff) * (i as u32 + 1)) & 0xffff;

            // Update state
            let upd = if k > 1 { 12 + k as u32 } else { 1 + (a + b) * (k as u32 + 1) };
            state = (u32::from(state) << 4).wrapping_add(upd) as u8;

            i += 2;
        }
    }

    Some(nf_seed)
}

/// Reads the residual bits of the quantization (`get_residual`).
fn get_residual(bits: &mut Bits<'_>, mut nbits: i32, hrmode: bool, x: &mut [f32], n: usize) {
    let mut xq_off_1: f32 = if hrmode { 0.25 } else { 5.0 / 16.0 };
    let mut xq_off_2: f32 = if hrmode { 0.25 } else { 3.0 / 16.0 };

    let niter = if hrmode { 20 } else { 1 };

    for _ in 0..niter {
        if nbits <= 0 {
            break;
        }

        for xi in x[..n].iter_mut() {
            if nbits <= 0 {
                break;
            }

            if *xi == 0.0 {
                continue;
            }

            if bits.get_bit() == 0 {
                *xi -= if *xi < 0.0 { xq_off_1 } else { xq_off_2 };
            }
            else {
                *xi += if *xi > 0.0 { xq_off_1 } else { xq_off_2 };
            }

            nbits -= 1;
        }

        xq_off_1 *= 0.5;
        xq_off_2 *= 0.5;
    }
}

/// Reads the LSB values of the quantized spectrum (`get_lsb`).
fn get_lsb(bits: &mut Bits<'_>, mut nbits: i32, x: &mut [f32], nq: usize, nf_seed: &mut u32) {
    let mut i = 0;

    while i < nq && nbits > 0 {
        let a = x[i].abs();
        let b = x[i + 1].abs();

        if a.max(b) < 4.0 {
            i += 2;
            continue;
        }

        let have_bit = nbits > 0;
        nbits -= 1;
        if have_bit && bits.get_bit() != 0 {
            if a != 0.0 {
                x[i] += if x[i] < 0.0 { -1.0 } else { 1.0 };
                *nf_seed = (*nf_seed + i as u32) & 0xffff;
            }
            else {
                let have_bit = nbits > 0;
                nbits -= 1;
                if have_bit {
                    x[i] = if bits.get_bit() != 0 { -1.0 } else { 1.0 };
                    *nf_seed = (*nf_seed + i as u32) & 0xffff;
                }
            }
        }

        let have_bit = nbits > 0;
        nbits -= 1;
        if have_bit && bits.get_bit() != 0 {
            if b != 0.0 {
                x[i + 1] += if x[i + 1] < 0.0 { -1.0 } else { 1.0 };
                *nf_seed = (*nf_seed + i as u32 + 1) & 0xffff;
            }
            else {
                let have_bit = nbits > 0;
                nbits -= 1;
                if have_bit {
                    x[i + 1] = if bits.get_bit() != 0 { -1.0 } else { 1.0 };
                    *nf_seed = (*nf_seed + i as u32 + 1) & 0xffff;
                }
            }
        }

        i += 2;
    }
}

/// Fills the spectrum with noise (`fill_noise`).
fn fill_noise(dt: usize, bw: usize, nf: i32, mut nf_seed: u32, g: f32, x: &mut [f32], nq: usize) {
    let bw_stop = common::ne(dt, bw.min(BANDWIDTH_FB));
    let w = 1 + usize::from(dt >= DT_7M5) + usize::from(dt >= DT_10M);

    let s = g * (8 - nf) as f32 / 16.0;
    let mut z = 0;

    let mut i = 6 * (1 + dt) - w;
    while i < nq.min(bw_stop) {
        z = if x[i] != 0.0 { 0 } else { z + 1 };
        if z > 2 * w {
            nf_seed = (13849 + nf_seed.wrapping_mul(31821)) & 0xffff;
            x[i - w] = if nf_seed & 0x8000 != 0 { -s } else { s };
        }
        i += 1;
    }

    while i < bw_stop + w {
        z += 1;
        if z > 2 * w {
            nf_seed = (13849 + nf_seed.wrapping_mul(31821)) & 0xffff;
            x[i - w] = if nf_seed & 0x8000 != 0 { -s } else { s };
        }
        i += 1;
    }
}

/// Decodes the spectral coefficients (`lc3_spec_decode`). `x` receives `ne` coefficients.
/// Returns `false` on an invalid bitstream.
#[allow(clippy::too_many_arguments)]
pub fn decode(
    bits: &mut Bits<'_>,
    dt: usize,
    sr: usize,
    bw: usize,
    nbytes: usize,
    side: &SpecSide,
    x: &mut [f32],
) -> bool {
    let lsb_mode = side.lsb_mode;
    let nq = side.nq;

    let nf = bits.get_bits(3) as i32;

    let Some(mut nf_seed) = get_quantized(bits, dt, sr, nbytes, nq, lsb_mode, x)
    else {
        return false;
    };

    let nbits_left = bits.bits_left_clamped();

    if lsb_mode {
        get_lsb(bits, nbits_left, x, nq, &mut nf_seed);
    }
    else {
        get_residual(bits, nbits_left, common::is_hr(sr), x, nq);
    }

    let g_int = side.g_idx - resolve_gain_offset(sr, nbytes);

    // Spectrum quantization inverse (`unquantize`).
    let g = unquantize_gain(g_int);
    let ne = common::ne(dt, sr);

    for xi in x[..nq].iter_mut() {
        *xi *= g;
    }
    for xi in x[nq..ne].iter_mut() {
        *xi = 0.0;
    }

    if nq > 2 || x[0] != 0.0 || x[1] != 0.0 || side.g_idx > 0 || nf < 7 {
        fill_noise(dt, bw, nf, nf_seed, g, x, nq);
    }

    true
}
