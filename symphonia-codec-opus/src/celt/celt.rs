// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared CELT decode-path helpers with no single natural C file (spread across `celt/celt.c`'s
//! `comb_filter`/`comb_filter_const_c`/`resampling_factor`/`init_caps`/`tf_select_table`).
//! Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltSynthesis".
//!
//! `comb_filter` in C takes two aliasable pointers (`y`,`x`) that are sometimes literally the
//! same buffer (the post-filter applied in place after synthesis) and sometimes genuinely
//! distinct buffers (the pre-filter applied while folding PLC state, which always calls with
//! `window == NULL`/`overlap == 0`, i.e. no crossfade region). Safe Rust can't alias a `&mut`
//! and a `&` over the same memory, so this is split into the two shapes actually used:
//! [`comb_filter_inplace`] (post-filter: aliased, crossfade region present) and
//! [`comb_filter_copy`] (pre-filter/fold: distinct buffers, no crossfade — for which C's `T0`/
//! `g0`/`tapset0` become dead parameters once `overlap == 0`, so they're dropped here).

use crate::celt::modes::CeltMode;

/// C: `COMBFILTER_MINPERIOD`.
pub const COMBFILTER_MINPERIOD: i32 = 15;

/// C: `comb_filter`'s static `gains[3][3]` table.
const GAINS: [[f32; 3]; 3] = [
    [0.3066406250, 0.2170410156, 0.1296386719],
    [0.4638671875, 0.2680664062, 0.0],
    [0.7998046875, 0.1000976562, 0.0],
];

/// C: `resampling_factor`.
pub fn resampling_factor(rate: i32) -> i32 {
    match rate {
        48000 => 1,
        24000 => 2,
        16000 => 3,
        12000 => 4,
        8000 => 6,
        _ => 0,
    }
}

/// C: `init_caps` (per-band bit-allocation caps for the current channel count/LM).
pub fn init_caps(mode: &CeltMode, caps: &mut [i32], lm: i32, channels: i32) {
    for i in 0..mode.nb_ebands as usize {
        let n = (mode.e_bands[i + 1] - mode.e_bands[i]) as i32 * (1 << lm);
        let idx = mode.nb_ebands * (2 * lm + channels - 1) + i as i32;
        caps[i] = (mode.cache.caps[idx as usize] as i32 + 64) * channels * n >> 2;
    }
}

/// C: `comb_filter` restricted to the in-place (`y == x`, same buffer/position) case used by
/// `celt_decode_with_ec`'s post-filter, applied to `buf` at logical position `pos` (i.e. C's
/// `x[k]`/`y[k]` is `buf[pos + k]`, `k` possibly negative as long as `pos + k` doesn't
/// underflow, which the decoder's persistent overlap buffers guarantee).
#[allow(clippy::too_many_arguments)]
pub fn comb_filter_inplace(
    buf: &mut [f32],
    pos: usize,
    t0: i32,
    t1: i32,
    n: i32,
    g0: f32,
    g1: f32,
    tapset0: i32,
    tapset1: i32,
    window: &[f32],
    overlap: i32,
) {
    if g0 == 0.0 && g1 == 0.0 {
        // C: `if (x!=y) OPUS_MOVE(y,x,N);` — no-op since `x` and `y` are the same buffer here.
        return;
    }
    let t0 = t0.max(COMBFILTER_MINPERIOD);
    let t1 = t1.max(COMBFILTER_MINPERIOD);
    let g00 = g0 * GAINS[tapset0 as usize][0];
    let g01 = g0 * GAINS[tapset0 as usize][1];
    let g02 = g0 * GAINS[tapset0 as usize][2];
    let g10 = g1 * GAINS[tapset1 as usize][0];
    let g11 = g1 * GAINS[tapset1 as usize][1];
    let g12 = g1 * GAINS[tapset1 as usize][2];

    // If the filter didn't change, we don't need the overlap.
    let overlap = if g0 == g1 && t0 == t1 && tapset0 == tapset1 { 0 } else { overlap };

    let at = |i: i32| -> usize { (pos as i64 + i as i64) as usize };

    let mut x1 = buf[at(-t1 + 1)];
    let mut x2 = buf[at(-t1)];
    let mut x3 = buf[at(-t1 - 1)];
    let mut x4 = buf[at(-t1 - 2)];

    let mut i = 0;
    while i < overlap {
        let x0 = buf[at(i - t1 + 2)];
        let w = window[i as usize];
        let f = w * w;
        let one_minus_f = 1.0 - f;
        let xi = buf[at(i)];
        let y = xi
            + one_minus_f * g00 * buf[at(i - t0)]
            + one_minus_f * g01 * (buf[at(i - t0 + 1)] + buf[at(i - t0 - 1)])
            + one_minus_f * g02 * (buf[at(i - t0 + 2)] + buf[at(i - t0 - 2)])
            + f * g10 * x2
            + f * g11 * (x1 + x3)
            + f * g12 * (x0 + x4);
        buf[at(i)] = y;
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
        i += 1;
    }
    if g1 == 0.0 {
        // C: `if (x!=y) OPUS_MOVE(y+overlap,x+overlap,N-overlap);` — no-op, same buffer.
        return;
    }
    // C: `comb_filter_const(y+i, x+i, T1, N-i, g10, g11, g12)` (portable, non-ARM-unrolled
    // branch), continuing the `x1..x4` recurrence in place from where the crossfade left off.
    while i < n {
        let x0 = buf[at(i - t1 + 2)];
        let y = buf[at(i)] + g10 * x2 + g11 * (x1 + x3) + g12 * (x0 + x4);
        buf[at(i)] = y;
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
        i += 1;
    }
}

/// C: `comb_filter` restricted to the distinct-buffer, no-crossfade case (`window == NULL`,
/// `overlap == 0`), used only by `prefilter_and_fold`. With the crossfade region empty, `T0`/
/// `g0`/`tapset0` are never read by the reference implementation either (they only matter
/// inside the crossfade loop), so they're omitted here.
pub fn comb_filter_copy(y: &mut [f32], x: &[f32], x_pos: usize, t1: i32, n: i32, g1: f32, tapset1: i32) {
    let n = n as usize;
    if g1 == 0.0 {
        y[..n].copy_from_slice(&x[x_pos..x_pos + n]);
        return;
    }
    let t1 = t1.max(COMBFILTER_MINPERIOD) as i64;
    let g10 = g1 * GAINS[tapset1 as usize][0];
    let g11 = g1 * GAINS[tapset1 as usize][1];
    let g12 = g1 * GAINS[tapset1 as usize][2];

    let at = |i: i64| -> usize { (x_pos as i64 + i) as usize };

    let mut x1 = x[at(-t1 + 1)];
    let mut x2 = x[at(-t1)];
    let mut x3 = x[at(-t1 - 1)];
    let mut x4 = x[at(-t1 - 2)];
    for i in 0..n as i64 {
        let x0 = x[at(i - t1 + 2)];
        y[i as usize] = x[at(i)] + g10 * x2 + g11 * (x1 + x3) + g12 * (x0 + x4);
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
    }
}

/// C: `tf_select_table[4][8]`. Positive values mean better frequency resolution (longer
/// effective window); negative values mean better time resolution (shorter effective window).
/// The second index is `4*isTransient + 2*tf_select + per_band_flag`.
pub const TF_SELECT_TABLE: [[i8; 8]; 4] = [
    [0, -1, 0, -1, 0, -1, 0, -1], // 2.5 ms
    [0, -1, 0, -2, 1, 0, 1, -1],  // 5 ms
    [0, -2, 0, -3, 2, 0, 1, -1],  // 10 ms
    [0, -2, 0, -3, 3, 0, 1, -1],  // 20 ms
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Direct transliteration of libopus `celt/celt.c`'s `comb_filter` (the reference, not the
    /// code under test), operating on an owned `Vec` rather than the in-place-friendly
    /// `buf`/`pos` convention used by [`comb_filter_inplace`]/[`comb_filter_copy`]. Used as an
    /// independent oracle: same algorithm, different (more direct, C-shaped) code path.
    fn comb_filter_direct(
        x_full: &[f32],
        x_pos: usize,
        t0: i32,
        t1: i32,
        n: i32,
        g0: f32,
        g1: f32,
        tapset0: i32,
        tapset1: i32,
        window: &[f32],
        overlap: i32,
        aliased: bool,
    ) -> Vec<f32> {
        // When `aliased`, `y` mirrors the *entire* buffer (history included) and is both read
        // from and written to as the loop advances, matching `comb_filter_inplace`'s real
        // `comb_filter(y, x, ...)` call site where `x == y` alias the same memory (the decoder's
        // post-filter is applied in place) — the g0-path terms and the `x0` recurrence source
        // deliberately observe already-filtered output for small periods. When not `aliased`
        // (matching `comb_filter_copy`'s distinct-buffer usage), reads always see the pristine
        // `x_full`, independent of what's already been written to `y`.
        let mut y = x_full.to_vec();
        let src = if aliased { None } else { Some(x_full.to_vec()) };
        let read = |y: &[f32], k: i64| -> f32 {
            let i = (x_pos as i64 + k) as usize;
            match &src {
                Some(s) => s[i],
                None => y[i],
            }
        };
        if g0 == 0.0 && g1 == 0.0 {
            return y[x_pos..x_pos + n as usize].to_vec();
        }
        let t0 = t0.max(COMBFILTER_MINPERIOD) as i64;
        let t1 = t1.max(COMBFILTER_MINPERIOD) as i64;
        let g00 = g0 * GAINS[tapset0 as usize][0];
        let g01 = g0 * GAINS[tapset0 as usize][1];
        let g02 = g0 * GAINS[tapset0 as usize][2];
        let g10 = g1 * GAINS[tapset1 as usize][0];
        let g11 = g1 * GAINS[tapset1 as usize][1];
        let g12 = g1 * GAINS[tapset1 as usize][2];
        let overlap = if g0 == g1 && t0 == t1 && tapset0 == tapset1 { 0 } else { overlap };
        let idx = |k: i64| -> usize { (x_pos as i64 + k) as usize };
        let mut x1 = read(&y, -t1 + 1);
        let mut x2 = read(&y, -t1);
        let mut x3 = read(&y, -t1 - 1);
        let mut x4 = read(&y, -t1 - 2);
        let mut i = 0i64;
        while i < overlap as i64 {
            let x0 = read(&y, i - t1 + 2);
            let w = window[i as usize];
            let f = w * w;
            let omf = 1.0 - f;
            let yi = read(&y, i)
                + omf * g00 * read(&y, i - t0)
                + omf * g01 * (read(&y, i - t0 + 1) + read(&y, i - t0 - 1))
                + omf * g02 * (read(&y, i - t0 + 2) + read(&y, i - t0 - 2))
                + f * g10 * x2
                + f * g11 * (x1 + x3)
                + f * g12 * (x0 + x4);
            y[idx(i)] = yi;
            x4 = x3;
            x3 = x2;
            x2 = x1;
            x1 = x0;
            i += 1;
        }
        if g1 == 0.0 {
            return y[x_pos..x_pos + n as usize].to_vec();
        }
        while i < n as i64 {
            let x0 = read(&y, i - t1 + 2);
            let yi = read(&y, i) + g10 * x2 + g11 * (x1 + x3) + g12 * (x0 + x4);
            y[idx(i)] = yi;
            x4 = x3;
            x3 = x2;
            x2 = x1;
            x1 = x0;
            i += 1;
        }
        y[x_pos..x_pos + n as usize].to_vec()
    }

    fn make_signal(len: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s = s.wrapping_mul(1103515245).wrapping_add(12345);
                (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect()
    }

    #[test]
    fn comb_filter_inplace_matches_direct_form() {
        let history = 100usize; // > max(t0,t1)+2 for the periods used below.
        let n = 200i32;
        let overlap = 40i32;
        let window: Vec<f32> = (0..overlap)
            .map(|i| ((i as f64 + 0.5) * std::f64::consts::PI / (2.0 * overlap as f64)).sin() as f32)
            .collect();
        for (t0, t1, g0, g1, ts0, ts1) in
            [(30, 35, 0.5f32, 0.7f32, 0, 1), (50, 50, 0.3, 0.3, 2, 2), (20, 60, 0.9, 0.2, 1, 0)]
        {
            let full = make_signal(history + n as usize, 7);

            let expected =
                comb_filter_direct(&full, history, t0, t1, n, g0, g1, ts0, ts1, &window, overlap, true);

            let mut buf = full.clone();
            comb_filter_inplace(&mut buf, history, t0, t1, n, g0, g1, ts0, ts1, &window, overlap);
            for i in 0..n as usize {
                assert!(
                    (buf[history + i] - expected[i]).abs() < 1e-6,
                    "t0={t0} t1={t1} i={i}: got {} expected {}",
                    buf[history + i],
                    expected[i]
                );
            }
        }
    }

    #[test]
    fn comb_filter_copy_matches_direct_form_no_crossfade() {
        let history = 100usize;
        let n = 100i32;
        let full = make_signal(history + n as usize, 11);
        for (t1, g1, ts1) in [(25, 0.6f32, 0), (40, 0.0, 1), (10, 0.9, 2)] {
            let expected = comb_filter_direct(&full, history, t1, t1, n, 0.0, g1, 0, ts1, &[], 0, false);
            let mut y = vec![0f32; n as usize];
            comb_filter_copy(&mut y, &full, history, t1, n, g1, ts1);
            for i in 0..n as usize {
                assert!(
                    (y[i] - expected[i]).abs() < 1e-6,
                    "t1={t1} i={i}: got {} expected {}",
                    y[i],
                    expected[i]
                );
            }
        }
    }
}
