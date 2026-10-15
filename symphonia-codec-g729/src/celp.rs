// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fixed-point CELP helper routines.
//!
//! Ported from FFmpeg `libavcodec/celp_filters.c`, `acelp_filters.c`, `acelp_vectors.c`,
//! `acelp_pitch_delay.[ch]` and `celp_math.[ch]` (see `NOTICE`). The arithmetic mirrors the C
//! `int`/`int16_t` semantics of the reference, including the silent 16-bit truncation of
//! compound assignments and 32-bit wrap-around: it must, because the decoder output is expected to
//! match FFmpeg's bit for bit.

use crate::tables::{ACELP_INTERP_FILTER, TAB_LOG2};

/// Smallest pitch delay (integer samples).
pub(crate) const PITCH_DELAY_MIN: i32 = 20;
/// Largest pitch delay (integer samples).
pub(crate) const PITCH_DELAY_MAX: i32 = 143;

/// Clips `v` to the `i16` range (`av_clip_int16`).
#[inline]
pub(crate) fn clip_i16(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// Floor of the base 2 logarithm, with `av_log2(0) == 0` (`av_log2`).
#[inline]
pub(crate) fn log2(v: u32) -> i32 {
    31 - (v | 1).leading_zeros() as i32
}

/// Shifts left by `offset` bits, or right by `-offset` bits if `offset` is negative
/// (`bidir_sal`).
#[inline]
pub(crate) fn bidir_sal(value: u32, offset: i32) -> u32 {
    if offset < 0 {
        value.wrapping_shr((-offset) as u32)
    }
    else {
        value.wrapping_shl(offset as u32)
    }
}

/// Dot product of two `i16` slices with a saturated `i32` result (`scalarproduct_int16_c`).
#[inline]
pub(crate) fn scalarproduct(a: &[i16], b: &[i16]) -> i32 {
    let res: i64 = a.iter().zip(b).map(|(&x, &y)| i64::from(x) * i64::from(y)).sum();
    res.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// Base 2 logarithm of `value` in Q15 (`ff_log2_q15`).
pub(crate) fn log2_q15(value: u32) -> i32 {
    let power_int = log2(value) as u32;
    let value = value << (31 - power_int);

    // b31 is always non-zero now.
    let frac_x0 = ((value & 0x7c00_0000) >> 26) as usize;
    let frac_dx = (value & 0x03ff_f800) >> 11;

    let x0 = i32::from(TAB_LOG2[frac_x0]);
    let x1 = i32::from(TAB_LOG2[frac_x0 + 1]);
    let value = x0 + ((frac_dx as i32 * (x1 - x0)) >> 15);

    ((power_int << 15) as i32) + value
}

/// One output sample of the LP synthesis filter, before adding the excitation: `out` holds the
/// filter history up to (excluding) `pos`.
#[inline]
fn lp_synth_sum(out: &[i16], pos: usize, coeffs: &[i16], order: usize, rounder: i32) -> i32 {
    let mut sum = rounder;
    for i in 1..=order {
        // The reference accumulates in `unsigned`, i.e. wraps.
        sum = sum.wrapping_sub(i32::from(coeffs[i - 1]) * i32::from(out[pos - i]));
    }
    sum >> 12
}

/// LP synthesis filter `1/A(z)` with separate input and output (`ff_celp_lp_synthesis_filter`,
/// with `shift = 0`).
///
/// `out[start - order..start]` holds the filter history, `out[start..start + len]` receives the
/// output. `coeffs[0..order]` are `a[1]..a[order]` in Q12. Returns `true` if the output had to
/// be clipped and `stop_on_overflow` is set; the remaining output is then not written.
pub(crate) fn lp_synthesis_filter(
    out: &mut [i16],
    start: usize,
    coeffs: &[i16],
    input: &[i16],
    len: usize,
    order: usize,
    stop_on_overflow: bool,
    rounder: i32,
) -> bool {
    for n in 0..len {
        let sum1 = lp_synth_sum(out, start + n, coeffs, order, rounder) + i32::from(input[n]);
        let sum = clip_i16(sum1);
        if stop_on_overflow && i32::from(sum) != sum1 {
            return true;
        }
        out[start + n] = sum;
    }
    false
}

/// LP synthesis filter where the input is read from `buf[start..start + len]` and the output
/// replaces it in place (`ff_celp_lp_synthesis_filter` with `in == out`).
pub(crate) fn lp_synthesis_filter_inplace(
    buf: &mut [i16],
    start: usize,
    coeffs: &[i16],
    len: usize,
    order: usize,
    rounder: i32,
) {
    for n in 0..len {
        let sum1 = lp_synth_sum(buf, start + n, coeffs, order, rounder) + i32::from(buf[start + n]);
        buf[start + n] = clip_i16(sum1);
    }
}

/// Circular convolution of the sparse vector `fc_in` with `filter` (`ff_celp_convolve_circ`).
/// All slices have `len` elements.
pub(crate) fn convolve_circ(fc_out: &mut [i16], fc_in: &[i16], filter: &[i16], len: usize) {
    fc_out[..len].fill(0);

    for i in 0..len {
        if fc_in[i] != 0 {
            let x = i32::from(fc_in[i]);
            for k in 0..i {
                fc_out[k] =
                    (i32::from(fc_out[k]) + ((x * i32::from(filter[len + k - i])) >> 15)) as i16;
            }
            for k in i..len {
                fc_out[k] = (i32::from(fc_out[k]) + ((x * i32::from(filter[k - i])) >> 15)) as i16;
            }
        }
    }
}

/// One sample of the fractional-delay interpolation filter (`ff_acelp_interpolate`).
///
/// `inp[idx]` corresponds to `in[n]` of the reference; `filter_length` taps are applied on each
/// side. The caller guarantees `filter_length <= idx` and `idx + filter_length <= inp.len()`.
#[inline]
fn interpolate_sample(
    inp: &[i16],
    idx: usize,
    filter: &[i16],
    precision: usize,
    frac_pos: usize,
    filter_length: usize,
) -> i16 {
    let mut v: i32 = 0x4000;
    let mut fidx = 0;

    let mut i = 0;
    while i < filter_length {
        v = v.wrapping_add(i32::from(inp[idx + i]) * i32::from(filter[fidx + frac_pos]));
        fidx += precision;
        i += 1;
        v = v.wrapping_add(i32::from(inp[idx - i]) * i32::from(filter[fidx - frac_pos]));
    }
    // The reference only logs if this would need clipping; it truncates.
    (v >> 15) as i16
}

/// Fractional-delay interpolation (`ff_acelp_interpolate`).
///
/// `in_base` is the index of `in[0]` in `inp`.
pub(crate) fn interpolate(
    out: &mut [i16],
    inp: &[i16],
    in_base: usize,
    filter: &[i16],
    precision: usize,
    frac_pos: usize,
    filter_length: usize,
    length: usize,
) {
    for n in 0..length {
        out[n] = interpolate_sample(inp, in_base + n, filter, precision, frac_pos, filter_length);
    }
}

/// Adaptive codebook interpolation performed in place on the excitation buffer: the output
/// `buf[out_base..out_base + length]` is computed from `buf[in_base + n - 10 ..]`, which may
/// overlap the samples already produced (pitch delays below the subframe length).
pub(crate) fn interpolate_exc(
    buf: &mut [i16],
    out_base: usize,
    in_base: usize,
    frac_pos: usize,
    length: usize,
) {
    for n in 0..length {
        buf[out_base + n] =
            interpolate_sample(buf, in_base + n, &ACELP_INTERP_FILTER, 6, frac_pos, 10);
    }
}

/// Single element of the weighted vector sum (`ff_acelp_weighted_vector_sum`).
#[inline]
pub(crate) fn weighted_sum(a: i16, b: i16, wa: i16, wb: i16, rounder: i16, shift: u32) -> i16 {
    let v = (i32::from(a) * i32::from(wa))
        .wrapping_add(i32::from(b) * i32::from(wb))
        .wrapping_add(i32::from(rounder));
    clip_i16(v >> shift)
}

/// High-pass filter, 140 Hz cutoff (`ff_acelp_high_pass_filter`).
///
/// `inp[2..2 + length]` is the input; `inp[0..2]` holds the two previous input samples.
pub(crate) fn high_pass_filter(out: &mut [i16], hpf_f: &mut [i32; 2], inp: &[i16], length: usize) {
    for i in 0..length {
        let x0 = i32::from(inp[i + 2]);
        let x1 = i32::from(inp[i + 1]);
        let x2 = i32::from(inp[i]);

        let mut tmp = ((i64::from(hpf_f[0]) * 15836) >> 13) as i32;
        tmp = tmp.wrapping_add(((i64::from(hpf_f[1]) * -7667) >> 13) as i32);
        tmp = tmp.wrapping_add(7699i32.wrapping_mul(x0 - 2 * x1 + x2));

        // With the "+0x800" rounding, clipping is needed for the ALGTHM and SPEECH tests.
        out[i] = clip_i16(tmp.wrapping_add(0x800) >> 12);

        hpf_f[1] = hpf_f[0];
        hpf_f[0] = tmp;
    }
}

/// Fixed-codebook vector for pulses with one position table per track
/// (`ff_acelp_fc_pulse_per_track`).
pub(crate) fn fc_pulse_per_track(
    fc_v: &mut [i16],
    tab1: &[u8],
    tab2: &[u8],
    mut pulse_indexes: u32,
    mut pulse_signs: u32,
    pulse_count: usize,
    bits: u32,
) {
    let mask = (1u32 << bits) - 1;

    for i in 0..pulse_count {
        let pos = i + usize::from(tab1[(pulse_indexes & mask) as usize]);
        // +/-1 in (2.13).
        fc_v[pos] = fc_v[pos].wrapping_add(if pulse_signs & 1 != 0 { 8191 } else { -8192 });

        pulse_indexes >>= bits;
        pulse_signs >>= 1;
    }

    let pos = usize::from(tab2[pulse_indexes as usize]);
    fc_v[pos] = fc_v[pos].wrapping_add(if pulse_signs & 1 != 0 { 8191 } else { -8192 });
}

/// Pitch delay (1/3 units) of the first subframe from the 8-bit index
/// (`ff_acelp_decode_8bit_to_1st_delay3`).
pub(crate) fn decode_8bit_to_1st_delay3(ac_index: i32) -> i32 {
    let ac_index = ac_index + 58;
    if ac_index > 254 { 3 * ac_index - 510 } else { ac_index }
}

/// Pitch delay (1/3 units) of the second subframe from the 5/6-bit index
/// (`ff_acelp_decode_5_6_bit_to_2nd_delay3`).
pub(crate) fn decode_5_6_bit_to_2nd_delay3(ac_index: i32, pitch_delay_min: i32) -> i32 {
    3 * pitch_delay_min + ac_index - 2
}

/// Pitch delay (1/3 units) of the second subframe from the 4-bit index
/// (`ff_acelp_decode_4bit_to_2nd_delay3`).
pub(crate) fn decode_4bit_to_2nd_delay3(ac_index: i32, pitch_delay_min: i32) -> i32 {
    if ac_index < 4 {
        3 * (ac_index + pitch_delay_min)
    }
    else if ac_index < 12 {
        3 * pitch_delay_min + ac_index + 6
    }
    else {
        3 * (ac_index + pitch_delay_min) - 18
    }
}

/// Updates the past quantized energies (`ff_acelp_update_past_gain`, `log2_ma_pred_order == 2`).
pub(crate) fn update_past_gain(quant_energy: &mut [i16; 4], gain_corr_factor: i32, erasure: bool) {
    let mut avg_gain = i32::from(quant_energy[3]); // (5.10)

    for i in (1..4).rev() {
        avg_gain += i32::from(quant_energy[i - 1]);
        quant_energy[i] = quant_energy[i - 1];
    }

    quant_energy[0] = if erasure {
        // -10 and -4 in (5.10).
        ((avg_gain >> 2).max(-10240) - 4096) as i16
    }
    else {
        ((6165 * ((log2_q15(gain_corr_factor as u32) >> 2) - (13 << 13))) >> 13) as i16
    };
}

/// Decodes the fixed-codebook gain (`ff_acelp_decode_gain_code`, FFmpeg's default floating-point
/// variant, 4.1.5 and 3.9.1 of G.729). Returns the quantized fixed-codebook gain in (14.1).
pub(crate) fn decode_gain_code(
    gain_corr_factor: i32,
    fc_v: &[i16],
    mr_energy: i32,
    quant_energy: &[i16; 4],
    ma_prediction_coeff: &[u16; 4],
) -> i16 {
    let mut mr_energy = mr_energy << 10;

    for i in 0..4 {
        mr_energy =
            mr_energy.wrapping_add(i32::from(quant_energy[i]) * i32::from(ma_prediction_coeff[i]));
    }

    // FFmpeg's `ff_exp10(x)` is `exp2(M_LOG2_10 * x)`.
    let exp10 = (std::f64::consts::LOG2_10 * (f64::from(mr_energy) / f64::from(20 << 23))).exp2();
    let energy = f64::from(scalarproduct(fc_v, fc_v));
    let gain = f64::from(gain_corr_factor) * exp10 / energy.sqrt();

    // Out-of-range float to int conversions yield `INT_MIN` on x86, which the reference relies on.
    let gain = if gain.is_finite() && gain >= -2147483648.0 && gain < 2147483648.0 {
        gain as i32
    }
    else {
        i32::MIN
    };
    (gain >> 12) as i16
}
