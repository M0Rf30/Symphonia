// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sample-rate conversion from the SILK-internal decode rate (8/12/16 kHz) to the API output
//! rate. Ported from libopus `silk/resampler.c`, `silk/resampler_private_up2_HQ.c`,
//! `silk/resampler_private_IIR_FIR.c`, `silk/resampler_private_down_FIR.c`,
//! `silk/resampler_private_AR2.c`, and the ROM tables of `silk/resampler_rom.c`
//! (BSD-3-Clause), see NOTICE. `silk_resampler_down2`/`silk_resampler_down2_3` (used only by
//! the encoder's VAD/low-pass bandwidth-switch detector) are excluded.

#![allow(dead_code)]

use crate::silk::macros::{
    silk_add32, silk_add_lshift32, silk_div32, silk_lshift, silk_min, silk_rshift, silk_rshift_round, silk_sat16,
    silk_smlabb, silk_smlawb, silk_smulbb, silk_smulwb, silk_sub32,
};

/// C: `SILK_RESAMPLER_MAX_FIR_ORDER`.
const MAX_FIR_ORDER: usize = 36;
/// C: `SILK_RESAMPLER_MAX_IIR_ORDER`.
const MAX_IIR_ORDER: usize = 6;
/// C: `RESAMPLER_MAX_BATCH_SIZE_MS`.
const MAX_BATCH_SIZE_MS: i32 = 10;
/// C: `RESAMPLER_MAX_FS_KHZ`.
const MAX_FS_KHZ: i32 = 48;
/// C: `RESAMPLER_MAX_BATCH_SIZE_IN`.
const MAX_BATCH_SIZE_IN: usize = (MAX_BATCH_SIZE_MS * MAX_FS_KHZ) as usize;
/// C: `RESAMPLER_ORDER_FIR_12`.
const ORDER_FIR_12: usize = 8;
/// C: `RESAMPLER_DOWN_ORDER_FIR0/1/2`.
const DOWN_ORDER_FIR0: usize = 18;
const DOWN_ORDER_FIR1: usize = 24;
const DOWN_ORDER_FIR2: usize = 36;

// -------------------------------------------------------------------------------------------
// ROM tables (`silk/resampler_rom.c`)
// -------------------------------------------------------------------------------------------

const UP2_HQ_0: [i16; 3] = [1746, 14986, -26453];
const UP2_HQ_1: [i16; 3] = [6854, 25769, -9994];

const RESAMPLER_3_4_COEFS: [i16; 2 + 3 * DOWN_ORDER_FIR0 / 2] = [
    -20694, -13867, -49, 64, 17, -157, 353, -496, 163, 11047, 22205, -39, 6, 91, -170, 186, 23, -896, 6336, 19928,
    -19, -36, 102, -89, -24, 328, -951, 2568, 15909,
];
const RESAMPLER_2_3_COEFS: [i16; 2 + 2 * DOWN_ORDER_FIR0 / 2] = [
    -14457, -14019, 64, 128, -122, 36, 310, -768, 584, 9267, 17733, 12, 128, 18, -142, 288, -117, -865, 4123, 14459,
];
const RESAMPLER_1_2_COEFS: [i16; 2 + DOWN_ORDER_FIR1 / 2] =
    [616, -14323, -10, 39, 58, -46, -84, 120, 184, -315, -541, 1284, 5380, 9024];
const RESAMPLER_1_3_COEFS: [i16; 2 + DOWN_ORDER_FIR2 / 2] = [
    16102, -15162, -13, 0, 20, 26, 5, -31, -43, -4, 65, 90, 7, -157, -248, -44, 593, 1583, 2612, 3271,
];
const RESAMPLER_1_4_COEFS: [i16; 2 + DOWN_ORDER_FIR2 / 2] = [
    22500, -15099, 3, -14, -20, -15, 2, 25, 37, 25, -16, -71, -107, -79, 50, 292, 623, 982, 1288, 1464,
];
const RESAMPLER_1_6_COEFS: [i16; 2 + DOWN_ORDER_FIR2 / 2] = [
    27540, -15257, 17, 12, 8, 1, -10, -22, -30, -32, -22, 3, 44, 100, 168, 243, 317, 381, 429, 455,
];

/// C: `silk_resampler_frac_FIR_12` (`[12][ORDER_FIR_12/2]`).
const FRAC_FIR_12: [[i16; ORDER_FIR_12 / 2]; 12] = [
    [189, -600, 617, 30567],
    [117, -159, -1070, 29704],
    [52, 221, -2392, 28276],
    [-4, 529, -3350, 26341],
    [-48, 758, -3956, 23973],
    [-80, 905, -4235, 21254],
    [-99, 972, -4222, 18278],
    [-107, 967, -3957, 15143],
    [-103, 896, -3487, 11950],
    [-91, 773, -2865, 8798],
    [-71, 611, -2143, 5784],
    [-46, 425, -1375, 2996],
];

/// C: `delay_matrix_enc[5][3]` (`in` in {8,12,16,24,48}, `out` in {8,12,16}).
const DELAY_MATRIX_ENC: [[i8; 3]; 5] = [[6, 0, 3], [0, 7, 3], [0, 1, 10], [0, 2, 6], [18, 10, 12]];
/// C: `delay_matrix_dec[3][5]` (`in` in {8,12,16}, `out` in {8,12,16,24,48}).
const DELAY_MATRIX_DEC: [[i8; 5]; 3] = [[4, 0, 2, 0, 0], [0, 9, 4, 7, 4], [0, 3, 12, 7, 7]];

/// C: `rateID(R)`: maps `{8000,12000,16000,24000,48000}` to `{0,1,2,3,4}`.
fn rate_id(r: i32) -> usize {
    ((((r >> 12) - i32::from(r > 16000)) >> i32::from(r > 24000)) - 1) as usize
}

/// C: `silk_resampler_state_struct`'s `resampler_function` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ResamplerFn {
    #[default]
    Copy,
    Up2HqWrapper,
    IirFir,
    DownFir,
}

/// C: `silk_resampler_state_struct`.
#[derive(Debug, Clone)]
pub struct SilkResamplerState {
    /// C: `sIIR` (also reused as the 2-element AR2 state for the down-sampling path).
    s_iir: [i32; MAX_IIR_ORDER],
    /// C: `sFIR.i32` -- used by the down-sampling (`down_FIR`) path.
    s_fir_i32: [i32; MAX_FIR_ORDER],
    /// C: `sFIR.i16` -- used by the up-sampling (`IIR_FIR`) path.
    s_fir_i16: [i16; MAX_FIR_ORDER],
    delay_buf: [i16; 48],
    resampler_function: ResamplerFn,
    batch_size: i32,
    inv_ratio_q16: i32,
    fir_order: usize,
    fir_fracs: i32,
    fs_in_khz: i32,
    fs_out_khz: i32,
    input_delay: i32,
    coefs: &'static [i16],
}

impl Default for SilkResamplerState {
    fn default() -> Self {
        SilkResamplerState {
            s_iir: [0; MAX_IIR_ORDER],
            s_fir_i32: [0; MAX_FIR_ORDER],
            s_fir_i16: [0; MAX_FIR_ORDER],
            delay_buf: [0; 48],
            resampler_function: ResamplerFn::Copy,
            batch_size: 0,
            inv_ratio_q16: 0,
            fir_order: 0,
            fir_fracs: 0,
            fs_in_khz: 0,
            fs_out_khz: 0,
            input_delay: 0,
            coefs: &[],
        }
    }
}

/// C: `silk_resampler_init`. `for_enc` selects the encoder's wider sample-rate range; the
/// decoder always passes `false`.
pub(crate) fn silk_resampler_init(s: &mut SilkResamplerState, fs_hz_in: i32, fs_hz_out: i32, for_enc: bool) -> i32 {
    *s = SilkResamplerState::default();

    if for_enc {
        if !matches!(fs_hz_in, 8000 | 12000 | 16000 | 24000 | 48000) || !matches!(fs_hz_out, 8000 | 12000 | 16000) {
            return -1;
        }
        s.input_delay = DELAY_MATRIX_ENC[rate_id(fs_hz_in)][rate_id(fs_hz_out)] as i32;
    } else {
        if !matches!(fs_hz_in, 8000 | 12000 | 16000) || !matches!(fs_hz_out, 8000 | 12000 | 16000 | 24000 | 48000) {
            return -1;
        }
        s.input_delay = DELAY_MATRIX_DEC[rate_id(fs_hz_in)][rate_id(fs_hz_out)] as i32;
    }

    s.fs_in_khz = fs_hz_in / 1000;
    s.fs_out_khz = fs_hz_out / 1000;

    // Number of samples processed per batch.
    s.batch_size = s.fs_in_khz * MAX_BATCH_SIZE_MS;

    // Find resampler with the right sampling ratio.
    let mut up2x = 0i32;
    if fs_hz_out > fs_hz_in {
        // Upsample.
        if fs_hz_out == fs_hz_in * 2 {
            s.resampler_function = ResamplerFn::Up2HqWrapper;
        } else {
            s.resampler_function = ResamplerFn::IirFir;
            up2x = 1;
        }
    } else if fs_hz_out < fs_hz_in {
        // Downsample.
        s.resampler_function = ResamplerFn::DownFir;
        if fs_hz_out * 4 == fs_hz_in * 3 {
            s.fir_fracs = 3;
            s.fir_order = DOWN_ORDER_FIR0;
            s.coefs = &RESAMPLER_3_4_COEFS;
        } else if fs_hz_out * 3 == fs_hz_in * 2 {
            s.fir_fracs = 2;
            s.fir_order = DOWN_ORDER_FIR0;
            s.coefs = &RESAMPLER_2_3_COEFS;
        } else if fs_hz_out * 2 == fs_hz_in {
            s.fir_fracs = 1;
            s.fir_order = DOWN_ORDER_FIR1;
            s.coefs = &RESAMPLER_1_2_COEFS;
        } else if fs_hz_out * 3 == fs_hz_in {
            s.fir_fracs = 1;
            s.fir_order = DOWN_ORDER_FIR2;
            s.coefs = &RESAMPLER_1_3_COEFS;
        } else if fs_hz_out * 4 == fs_hz_in {
            s.fir_fracs = 1;
            s.fir_order = DOWN_ORDER_FIR2;
            s.coefs = &RESAMPLER_1_4_COEFS;
        } else if fs_hz_out * 6 == fs_hz_in {
            s.fir_fracs = 1;
            s.fir_order = DOWN_ORDER_FIR2;
            s.coefs = &RESAMPLER_1_6_COEFS;
        } else {
            return -1;
        }
    } else {
        // Input and output sampling rates are equal: copy.
        s.resampler_function = ResamplerFn::Copy;
    }

    // Ratio of input/output samples.
    s.inv_ratio_q16 = silk_lshift(silk_div32(silk_lshift(fs_hz_in, 14 + up2x), fs_hz_out), 2);
    // Make sure the ratio is rounded up.
    while silk_smulww_i64(s.inv_ratio_q16, fs_hz_out) < silk_lshift(fs_hz_in, up2x) {
        s.inv_ratio_q16 += 1;
    }

    0
}

/// `silk_SMULWW`-precision helper for the `invRatio_Q16` rounding loop above (both operands can
/// exceed 16 bits, unlike `silk_SMULWB`).
#[inline]
fn silk_smulww_i64(a: i32, b: i32) -> i32 {
    (((a as i64) * (b as i64)) >> 16) as i32
}

/// C: `silk_resampler`. Resamples `input[0..in_len]` into `out`, returning the number of
/// samples written.
pub(crate) fn silk_resampler(s: &mut SilkResamplerState, out: &mut [i16], input: &[i16], in_len: i32) -> i32 {
    debug_assert!(in_len >= s.fs_in_khz);
    debug_assert!(s.input_delay <= s.fs_in_khz);

    let n_samples = (s.fs_in_khz - s.input_delay) as usize;

    // Copy to delay buffer.
    s.delay_buf[s.input_delay as usize..s.input_delay as usize + n_samples].copy_from_slice(&input[..n_samples]);

    let fs_in_khz = s.fs_in_khz as usize;
    let fs_out_khz = s.fs_out_khz as usize;
    let delay_buf = s.delay_buf;

    let second_len = in_len as usize - fs_in_khz;
    match s.resampler_function {
        ResamplerFn::Up2HqWrapper => {
            up2_hq(&mut s.s_iir, out, &delay_buf[..fs_in_khz]);
            up2_hq(&mut s.s_iir, &mut out[fs_out_khz..], &input[n_samples..n_samples + second_len]);
        }
        ResamplerFn::IirFir => {
            iir_fir(s, out, &delay_buf[..fs_in_khz]);
            iir_fir(s, &mut out[fs_out_khz..], &input[n_samples..n_samples + second_len]);
        }
        ResamplerFn::DownFir => {
            down_fir(s, out, &delay_buf[..fs_in_khz]);
            down_fir(s, &mut out[fs_out_khz..], &input[n_samples..n_samples + second_len]);
        }
        ResamplerFn::Copy => {
            out[..fs_in_khz].copy_from_slice(&delay_buf[..fs_in_khz]);
            out[fs_out_khz..fs_out_khz + second_len]
                .copy_from_slice(&input[n_samples..n_samples + second_len]);
        }
    }

    // Copy to delay buffer.
    let tail_start = in_len as usize - s.input_delay as usize;
    s.delay_buf[..s.input_delay as usize].copy_from_slice(&input[tail_start..tail_start + s.input_delay as usize]);

    silk_div32(in_len * s.fs_out_khz, s.fs_in_khz)
}

// -------------------------------------------------------------------------------------------
// `silk_resampler_private_up2_HQ` / wrapper
// -------------------------------------------------------------------------------------------

/// C: `silk_resampler_private_up2_HQ`. `state` is `S[0..6]`.
fn up2_hq(state: &mut [i32; MAX_IIR_ORDER], out: &mut [i16], input: &[i16]) {
    for (k, &x) in input.iter().enumerate() {
        let in32 = silk_lshift(x as i32, 10);

        // First all-pass section for even output sample.
        let y = silk_sub32(in32, state[0]);
        let x0 = silk_smulwb(y, UP2_HQ_0[0] as i32);
        let out32_1 = silk_add32(state[0], x0);
        state[0] = silk_add32(in32, x0);

        // Second all-pass section for even output sample.
        let y = silk_sub32(out32_1, state[1]);
        let x1 = silk_smulwb(y, UP2_HQ_0[1] as i32);
        let out32_2 = silk_add32(state[1], x1);
        state[1] = silk_add32(out32_1, x1);

        // Third all-pass section for even output sample.
        let y = silk_sub32(out32_2, state[2]);
        let x2 = silk_smlawb(y, y, UP2_HQ_0[2] as i32);
        let out32_1e = silk_add32(state[2], x2);
        state[2] = silk_add32(out32_2, x2);

        out[2 * k] = silk_sat16(silk_rshift_round(out32_1e, 10)) as i16;

        // First all-pass section for odd output sample.
        let y = silk_sub32(in32, state[3]);
        let x0 = silk_smulwb(y, UP2_HQ_1[0] as i32);
        let out32_1 = silk_add32(state[3], x0);
        state[3] = silk_add32(in32, x0);

        // Second all-pass section for odd output sample.
        let y = silk_sub32(out32_1, state[4]);
        let x1 = silk_smulwb(y, UP2_HQ_1[1] as i32);
        let out32_2 = silk_add32(state[4], x1);
        state[4] = silk_add32(out32_1, x1);

        // Third all-pass section for odd output sample.
        let y = silk_sub32(out32_2, state[5]);
        let x2 = silk_smlawb(y, y, UP2_HQ_1[2] as i32);
        let out32_1o = silk_add32(state[5], x2);
        state[5] = silk_add32(out32_2, x2);

        out[2 * k + 1] = silk_sat16(silk_rshift_round(out32_1o, 10)) as i16;
    }
}

// -------------------------------------------------------------------------------------------
// `silk_resampler_private_IIR_FIR`
// -------------------------------------------------------------------------------------------

fn iir_fir_interpol(out: &mut [i16], buf: &[i16], max_index_q16: i32, index_increment_q16: i32) -> usize {
    let mut n = 0usize;
    let mut index_q16 = 0i32;
    while index_q16 < max_index_q16 {
        let table_index = silk_smulwb(index_q16 & 0xFFFF, 12) as usize;
        let b = (index_q16 >> 16) as usize;

        let mut res_q15 = silk_smulbb(buf[b] as i32, FRAC_FIR_12[table_index][0] as i32);
        res_q15 = silk_smlabb(res_q15, buf[b + 1] as i32, FRAC_FIR_12[table_index][1] as i32);
        res_q15 = silk_smlabb(res_q15, buf[b + 2] as i32, FRAC_FIR_12[table_index][2] as i32);
        res_q15 = silk_smlabb(res_q15, buf[b + 3] as i32, FRAC_FIR_12[table_index][3] as i32);
        res_q15 = silk_smlabb(res_q15, buf[b + 4] as i32, FRAC_FIR_12[11 - table_index][3] as i32);
        res_q15 = silk_smlabb(res_q15, buf[b + 5] as i32, FRAC_FIR_12[11 - table_index][2] as i32);
        res_q15 = silk_smlabb(res_q15, buf[b + 6] as i32, FRAC_FIR_12[11 - table_index][1] as i32);
        res_q15 = silk_smlabb(res_q15, buf[b + 7] as i32, FRAC_FIR_12[11 - table_index][0] as i32);

        out[n] = silk_sat16(silk_rshift_round(res_q15, 15)) as i16;
        n += 1;
        index_q16 += index_increment_q16;
    }
    n
}

/// C: `silk_resampler_private_IIR_FIR`. One self-contained call (the C function is called
/// TWICE per [`silk_resampler`] invocation, each time with fresh local state but persistent
/// `S->sFIR`/`S->sIIR`); this Rust port mirrors that 1:1.
fn iir_fir(s: &mut SilkResamplerState, mut out: &mut [i16], mut input: &[i16]) {
    let mut buf = [0i16; 2 * MAX_BATCH_SIZE_IN + ORDER_FIR_12];
    buf[..ORDER_FIR_12].copy_from_slice(&s.s_fir_i16[..ORDER_FIR_12]);

    let index_increment_q16 = s.inv_ratio_q16;
    let mut in_len = input.len() as i32;
    let mut n_samples_in;
    loop {
        n_samples_in = silk_min(in_len, s.batch_size) as usize;

        up2_hq(&mut s.s_iir, &mut buf[ORDER_FIR_12..], &input[..n_samples_in]);

        let max_index_q16 = silk_lshift(n_samples_in as i32, 16 + 1);
        let n = iir_fir_interpol(out, &buf, max_index_q16, index_increment_q16);
        out = &mut out[n..];

        input = &input[n_samples_in..];
        in_len -= n_samples_in as i32;

        if in_len > 0 {
            // Copy last part of filtered signal to beginning of buffer.
            let src = n_samples_in << 1;
            for j in 0..ORDER_FIR_12 {
                buf[j] = buf[src + j];
            }
        } else {
            break;
        }
    }

    // Copy last part of filtered signal to the state for the next call.
    let src = n_samples_in << 1;
    s.s_fir_i16[..ORDER_FIR_12].copy_from_slice(&buf[src..src + ORDER_FIR_12]);
}

// -------------------------------------------------------------------------------------------
// `silk_resampler_private_down_FIR` + `silk_resampler_private_AR2`
// -------------------------------------------------------------------------------------------

/// C: `silk_resampler_private_AR2`. `state` is the first 2 elements of `S->sIIR`.
fn ar2(state: &mut [i32], out_q8: &mut [i32], input: &[i16], a_q14: &[i16]) {
    for (k, &x) in input.iter().enumerate() {
        let mut out32 = silk_add_lshift32(state[0], x as i32, 8);
        out_q8[k] = out32;
        out32 = silk_lshift(out32, 2);
        let s0 = silk_smlawb(state[1], out32, a_q14[0] as i32);
        let s1 = silk_smulwb(out32, a_q14[1] as i32);
        state[0] = s0;
        state[1] = s1;
    }
}

fn down_fir_interpol(
    out: &mut [i16],
    buf: &[i32],
    fir_coefs: &[i16],
    fir_order: usize,
    fir_fracs: i32,
    max_index_q16: i32,
    index_increment_q16: i32,
) -> usize {
    let mut n = 0usize;
    let mut index_q16 = 0i32;
    match fir_order {
        DOWN_ORDER_FIR0 => {
            while index_q16 < max_index_q16 {
                let b = silk_rshift(index_q16, 16) as usize;
                let interpol_ind = silk_smulwb(index_q16 & 0xFFFF, fir_fracs) as usize;
                let ip = &fir_coefs[DOWN_ORDER_FIR0 / 2 * interpol_ind..];
                let mut res_q6 = silk_smulwb(buf[b], ip[0] as i32);
                for j in 1..9 {
                    res_q6 = silk_smlawb(res_q6, buf[b + j], ip[j] as i32);
                }
                let ip2 = &fir_coefs[DOWN_ORDER_FIR0 / 2 * (fir_fracs as usize - 1 - interpol_ind)..];
                for j in 0..9 {
                    res_q6 = silk_smlawb(res_q6, buf[b + 17 - j], ip2[j] as i32);
                }
                out[n] = silk_sat16(silk_rshift_round(res_q6, 6)) as i16;
                n += 1;
                index_q16 += index_increment_q16;
            }
        }
        DOWN_ORDER_FIR1 => {
            while index_q16 < max_index_q16 {
                let b = silk_rshift(index_q16, 16) as usize;
                let mut res_q6 = silk_smulwb(silk_add32(buf[b], buf[b + 23]), fir_coefs[0] as i32);
                for j in 1..12 {
                    res_q6 = silk_smlawb(res_q6, silk_add32(buf[b + j], buf[b + 23 - j]), fir_coefs[j] as i32);
                }
                out[n] = silk_sat16(silk_rshift_round(res_q6, 6)) as i16;
                n += 1;
                index_q16 += index_increment_q16;
            }
        }
        DOWN_ORDER_FIR2 => {
            while index_q16 < max_index_q16 {
                let b = silk_rshift(index_q16, 16) as usize;
                let mut res_q6 = silk_smulwb(silk_add32(buf[b], buf[b + 35]), fir_coefs[0] as i32);
                for j in 1..18 {
                    res_q6 = silk_smlawb(res_q6, silk_add32(buf[b + j], buf[b + 35 - j]), fir_coefs[j] as i32);
                }
                out[n] = silk_sat16(silk_rshift_round(res_q6, 6)) as i16;
                n += 1;
                index_q16 += index_increment_q16;
            }
        }
        _ => unreachable!("invalid FIR_Order"),
    }
    n
}

/// C: `silk_resampler_private_down_FIR`. One self-contained call, called TWICE per
/// [`silk_resampler`] invocation (see [`iir_fir`]'s doc comment).
fn down_fir(s: &mut SilkResamplerState, mut out: &mut [i16], mut input: &[i16]) {
    let fir_order = s.fir_order;
    let mut buf = [0i32; MAX_BATCH_SIZE_IN + MAX_FIR_ORDER];
    buf[..fir_order].copy_from_slice(&s.s_fir_i32[..fir_order]);

    // C: `FIR_Coefs = &S->Coefs[2]` (the first 2 entries are the AR2 coefficients).
    let fir_coefs = &s.coefs[2..];
    let ar_coefs = [s.coefs[0], s.coefs[1]];
    let index_increment_q16 = s.inv_ratio_q16;

    let mut in_len = input.len() as i32;
    let mut n_samples_in;
    loop {
        n_samples_in = silk_min(in_len, s.batch_size) as usize;

        // Second-order AR filter (output in Q8).
        ar2(&mut s.s_iir[..2], &mut buf[fir_order..fir_order + n_samples_in], &input[..n_samples_in], &ar_coefs);

        let max_index_q16 = silk_lshift(n_samples_in as i32, 16);
        let n = down_fir_interpol(out, &buf, fir_coefs, fir_order, s.fir_fracs, max_index_q16, index_increment_q16);
        out = &mut out[n..];

        input = &input[n_samples_in..];
        in_len -= n_samples_in as i32;

        if in_len > 1 {
            // Copy last part of filtered signal to beginning of buffer.
            let src = n_samples_in;
            for j in 0..fir_order {
                buf[j] = buf[src + j];
            }
        } else {
            break;
        }
    }

    // Copy last part of filtered signal to the state for the next call.
    let src = n_samples_in;
    s.s_fir_i32[..fir_order].copy_from_slice(&buf[src..src + fir_order]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_resample(fs_in: i32, fs_out: i32, total_in: usize) -> Vec<i16> {
        let mut state = SilkResamplerState::default();
        assert_eq!(silk_resampler_init(&mut state, fs_in, fs_out, false), 0);
        let input: Vec<i16> = (0..total_in).map(|i| ((i % 2000) as i16) - 1000).collect();
        let fs_in_khz = (fs_in / 1000) as usize;
        // silk_resampler requires inLen >= Fs_in_kHz per call; feed it 20 ms at a time, like a
        // real 20 ms SILK frame.
        let step = fs_in_khz * 20;
        let mut out = vec![0i16; total_in * 8]; // generous upper bound
        let mut in_pos = 0usize;
        let mut out_pos = 0usize;
        while in_pos + step <= total_in {
            let n = silk_resampler(&mut state, &mut out[out_pos..], &input[in_pos..in_pos + step], step as i32);
            out_pos += n as usize;
            in_pos += step;
        }
        out.truncate(out_pos);
        out
    }

    #[test]
    fn resample_8k_to_48k_produces_output_and_no_panic() {
        let out = run_resample(8000, 48000, 8 * 200);
        assert!(!out.is_empty());
        // 8 kHz -> 48 kHz is a 6x ratio: each 20 ms/8kHz step (160 in) should yield ~960 out.
        assert!(out.len() >= 900 * (out.len() / 960).max(1));
    }

    #[test]
    fn resample_12k_to_48k_produces_output_and_no_panic() {
        let out = run_resample(12000, 48000, 12 * 200);
        assert!(!out.is_empty());
    }

    #[test]
    fn resample_16k_to_48k_produces_output_and_no_panic() {
        let out = run_resample(16000, 48000, 16 * 200);
        assert!(!out.is_empty());
    }

    #[test]
    fn identity_rate_is_copy() {
        let mut state = SilkResamplerState::default();
        assert_eq!(silk_resampler_init(&mut state, 16000, 16000, false), 0);
        assert_eq!(state.resampler_function, ResamplerFn::Copy);
        let input = [100i16; 320];
        let mut out = [0i16; 340];
        let n = silk_resampler(&mut state, &mut out, &input, 320);
        assert_eq!(n, 320);
        // The "copy" path still applies `inputDelay` samples of latency (to equalize delay
        // across resampling modes), so only the tail is guaranteed to already be steady-state.
        assert!(out[320 - 32..320].iter().all(|&x| x == 100));
    }

    #[test]
    fn downsample_16k_to_8k_no_panic() {
        let out = run_resample(16000, 8000, 16 * 200);
        assert!(!out.is_empty());
    }
}
