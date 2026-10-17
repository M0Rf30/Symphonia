// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ACELP excitation of the USAC LPD coder (ISO/IEC 23003-3 7.14.4): the algebraic codebook
//! pulses, the gains and the adaptive codebook interpolation.
//!
//! Derived from libxaac (`ixheaacd_acelp_decode.c`, Apache-2.0).

use super::tables::{INT_LEAVE_GAIN, INTERPOL_FILT};

/// The number of samples of an ACELP sub-frame.
pub const LEN_SUBFR: usize = 64;

/// The order of the adaptive codebook interpolation filter.
pub const INTER_LP_FIL_ORDER: usize = 16;

const UP_SAMP: usize = 4;

const F_PIT_SHARP: f32 = 0.85;

/// The number of bits of the algebraic codebook per `acelp_core_mode`.
pub const CODE_BITS: [u32; 8] = [20, 28, 36, 44, 52, 64, 12, 16];

/// The number of bits of the four algebraic codebook indexes per `acelp_core_mode` (modes 0 to
/// 4, 6 and 7; mode 5 reads 2, 2, 2, 2, 14, 14, 14, 14).
pub const NUM_BITS_CELP_CODING: [[u32; 4]; 8] = [
    [5, 5, 5, 5],
    [9, 9, 5, 5],
    [9, 9, 9, 9],
    [13, 13, 9, 9],
    [13, 13, 13, 13],
    [16, 16, 16, 16],
    [1, 5, 1, 5],
    [1, 5, 5, 5],
];

/// Pitch sharpening of the algebraic codevector.
pub fn pitch_sharpening(x: &mut [f32; LEN_SUBFR], pit_lag: usize) {
    for i in pit_lag..LEN_SUBFR {
        x[i] += x[i - pit_lag] * F_PIT_SHARP;
    }
}

fn add(code: &mut [f32; LEN_SUBFR], pos: i32, track: i32, sign: bool) {
    // A malformed index cannot place a pulse outside of the sub-frame (positions are at most 15
    // for the codebook layouts), but stay in range regardless.
    let m = (((pos << 2) + track) as usize) & (LEN_SUBFR - 1);
    code[m] += if sign { -1.0 } else { 1.0 };
}

fn decode_1sp_per_track(idx: i32, m: u32, offset: i32, track: i32, code: &mut [f32; LEN_SUBFR]) {
    let mask = (1 << m) - 1;
    let sp_pos = (idx & mask) + offset;
    let sign = (idx >> m) & 1 == 1;
    add(code, sp_pos, track, sign);
}

fn decode_2sp_per_track(idx: i32, m: u32, offset: i32, track: i32, code: &mut [f32; LEN_SUBFR]) {
    let mask = (1 << m) - 1;
    let p0 = ((idx >> m) & mask) + offset;
    let p1 = (idx & mask) + offset;
    let sign = (idx >> (2 * m)) & 1 == 1;

    if p1 - p0 < 0 {
        add(code, p0, track, sign);
        add(code, p1, track, !sign);
    }
    else {
        add(code, p0, track, sign);
        add(code, p1, track, sign);
    }
}

fn decode_3sp_per_track(idx: i32, m: u32, offset: i32, track: i32, code: &mut [f32; LEN_SUBFR]) {
    let mask = (1 << (2 * m - 1)) - 1;
    let idx_2p = idx & mask;
    let mut j = offset;

    if (idx >> (2 * m - 1)) & 1 == 1 {
        j += 1 << (m - 1);
    }

    decode_2sp_per_track(idx_2p, m - 1, j, track, code);

    let mask = (1 << (m + 1)) - 1;
    let idx_1p = (idx >> (2 * m)) & mask;
    decode_1sp_per_track(idx_1p, m, offset, track, code);
}

fn decode_4sp_per_track_section(index: i32, offset: i32, track: i32, code: &mut [f32; LEN_SUBFR]) {
    let idx_2p = index & 31;
    let mut j = offset;

    if (index >> 5) & 1 == 1 {
        j += 4;
    }

    decode_2sp_per_track(idx_2p, 2, j, track, code);

    let idx_2p = (index >> 6) & 127;
    decode_2sp_per_track(idx_2p, 3, offset, track, code);
}

fn decode_4sp_per_track(idx: i32, track: i32, code: &mut [f32; LEN_SUBFR]) {
    match (idx >> 14) & 3 {
        0 => {
            if (idx >> 13) & 1 == 0 {
                decode_4sp_per_track_section(idx, 0, track, code);
            }
            else {
                decode_4sp_per_track_section(idx, 8, track, code);
            }
        }
        1 => {
            decode_1sp_per_track(idx >> 10, 3, 0, track, code);
            decode_3sp_per_track(idx, 3, 8, track, code);
        }
        2 => {
            decode_2sp_per_track(idx >> 7, 3, 0, track, code);
            decode_2sp_per_track(idx, 3, 8, track, code);
        }
        _ => {
            decode_3sp_per_track(idx >> 4, 3, 0, track, code);
            decode_1sp_per_track(idx, 3, 8, track, code);
        }
    }
}

fn add_pulse(pos: i32, track: i32, code: &mut [f32; LEN_SUBFR]) {
    let i = (((pos & 15) << 2) + track) as usize;
    code[i] += if pos & 16 == 0 { 1.0 } else { -1.0 };
}

fn decode_1p_n1(index: i32, n: u32, offset: i32) -> i32 {
    let mask = (1 << n) - 1;
    let mut pos = (index & mask) + offset;

    if (index >> n) & 1 == 1 {
        pos += 16;
    }

    pos
}

/// Decode the algebraic codevector of the `code_bits` bit codebook from its indexes.
pub fn decode_pulses_per_track(cb_index: &[i32; 8], code_bits: u32) -> [f32; LEN_SUBFR] {
    let mut code = [0.0f32; LEN_SUBFR];

    match code_bits {
        12 => {
            for track_idx in [0, 2] {
                let offset = cb_index[track_idx];
                let index = cb_index[track_idx + 1];
                let pos = decode_1p_n1(index, 4, 0);
                add_pulse(pos, 2 * offset + (track_idx / 2) as i32, &mut code);
            }
        }
        16 => {
            let mut i = 0;
            let offset = cb_index[i];
            i += 1;
            let skip = if offset == 0 { 1 } else { 3 };

            for track_idx in 0..4 {
                if track_idx != skip {
                    let pos = decode_1p_n1(cb_index[i], 4, 0);
                    i += 1;
                    add_pulse(pos, track_idx, &mut code);
                }
            }
        }
        20 => {
            for t in 0..4 {
                decode_1sp_per_track(cb_index[t], 4, 0, t as i32, &mut code);
            }
        }
        28 => {
            for t in 0..2 {
                decode_2sp_per_track(cb_index[t], 4, 0, t as i32, &mut code);
            }
            for t in 2..4 {
                decode_1sp_per_track(cb_index[t], 4, 0, t as i32, &mut code);
            }
        }
        36 => {
            for t in 0..4 {
                decode_2sp_per_track(cb_index[t], 4, 0, t as i32, &mut code);
            }
        }
        44 => {
            for t in 0..2 {
                decode_3sp_per_track(cb_index[t], 4, 0, t as i32, &mut code);
            }
            for t in 2..4 {
                decode_2sp_per_track(cb_index[t], 4, 0, t as i32, &mut code);
            }
        }
        52 => {
            for t in 0..4 {
                decode_3sp_per_track(cb_index[t], 4, 0, t as i32, &mut code);
            }
        }
        64 => {
            for t in 0..4 {
                let index = (cb_index[t] << 14) + cb_index[t + 4];
                decode_4sp_per_track(index, t as i32, &mut code);
            }
        }
        _ => {}
    }

    code
}

/// The gains of a sub-frame: returns the pitch gain, the codebook gain and the energy of the
/// codevector.
pub fn decode_gains(
    index: usize,
    code: &[f32; LEN_SUBFR],
    mean_exc_energy: f32,
) -> (f32, f32, f32) {
    let mut avg_innov_energy = 0.01f32;

    for c in code {
        avg_innov_energy += c * c;
    }

    let energy = avg_innov_energy;

    let avg_db = (10.0 * f64::from(avg_innov_energy / LEN_SUBFR as f32).log10()) as f32;
    let est_gain = (mean_exc_energy - avg_db) as f64;
    let est_gain = 10f64.powf(0.05 * est_gain) as f32;

    let index = index & 127;

    (INT_LEAVE_GAIN[index * 2], INT_LEAVE_GAIN[index * 2 + 1] * est_gain, energy)
}

/// The adaptive codebook excitation: `exc[base..base + LEN_SUBFR + 1]` is computed from the past
/// excitation with the fractional pitch lag `pitch_lag + frac / 4`.
pub fn cb_exc_calc(exc: &mut [f32], base: usize, pitch_lag: i32, frac: i32) {
    let mut x0 = base as isize - pitch_lag as isize;
    let mut frac = -frac;

    if frac < 0 {
        frac += UP_SAMP as i32;
        x0 -= 1;
    }

    let frac = frac as usize;

    for j in 0..=LEN_SUBFR {
        let mut x1 = x0 + j as isize;
        let mut c1 = frac;
        let mut c2 = UP_SAMP - frac;
        let mut s = 0.0f32;

        for x2 in (x1 + 1..).take(INTER_LP_FIL_ORDER) {
            s += exc[x1 as usize] * INTERPOL_FILT[c1] + exc[x2 as usize] * INTERPOL_FILT[c2];
            x1 -= 1;
            c1 += UP_SAMP;
            c2 += UP_SAMP;
        }

        exc[base + j] = s;
    }
}
