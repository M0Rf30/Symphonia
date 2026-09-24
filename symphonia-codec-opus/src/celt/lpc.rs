// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Linear prediction, used only by CELT PLC. Ported from libopus `celt/celt_lpc.c`
//! (`_celt_lpc`, `celt_fir_c`, `celt_iir`'s `SMALL_FOOTPRINT` branch, `_celt_autocorr`), float
//! build. Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltSynthesis".

use crate::celt::pitch::celt_pitch_xcorr;

/// C: `CELT_LPC_ORDER`.
pub const CELT_LPC_ORDER: usize = 24;

/// Largest `n` any caller passes (`MAX_PERIOD` from `celt_decoder.c`, and
/// `DECODE_BUFFER_SIZE/2` from `pitch_downsample`); used to size a fixed-size stack scratch
/// buffer so the (non-hot-path, PLC-only) autocorrelation never needs a heap allocation.
const MAX_AUTOCORR_N: usize = 1024;

/// C: `_celt_lpc`. Computes LPC coefficients `lpc` (length `p`, `p <= CELT_LPC_ORDER`) from
/// autocorrelation `ac` (length `p+1`) via Levinson-Durbin recursion.
pub fn celt_lpc(lpc_out: &mut [f32], ac: &[f32], p: i32) {
    let p = p as usize;
    let mut lpc = [0f32; CELT_LPC_ORDER];
    let mut error = ac[0];
    if ac[0] > 1e-10 {
        for i in 0..p {
            let mut rr = 0f32;
            for j in 0..i {
                rr += lpc[j] * ac[i - j];
            }
            rr += ac[i + 1];
            let r = -rr / error;
            lpc[i] = r;
            for j in 0..(i + 1) / 2 {
                let tmp1 = lpc[j];
                let tmp2 = lpc[i - 1 - j];
                lpc[j] = tmp1 + r * tmp2;
                lpc[i - 1 - j] = tmp2 + r * tmp1;
            }
            error -= r * r * error;
            if error <= 0.001 * ac[0] {
                break;
            }
        }
    }
    lpc_out[..p].copy_from_slice(&lpc[..p]);
}

/// C: `celt_fir_c`. Filters `x[x_pos..x_pos+n]` (`x` must have >= `ord` samples of valid
/// history before `x_pos`, matching the C convention of a pointer with negative-offset
/// lookback) through the FIR with coefficients `num` (length `ord`), writing `y` (length `n`).
/// The wave-0 stub's `mem` parameter doesn't exist on the real `celt_fir_c` (it re-reads
/// history directly from `x`), so it's replaced here with the `x_pos` base-offset convention
/// used throughout this port for C's negative-index pointer arithmetic.
pub fn celt_fir(x: &[f32], x_pos: usize, num: &[f32], y: &mut [f32], n: i32, ord: i32) {
    let n = n as usize;
    let ord = ord as usize;
    let mut rnum = [0f32; CELT_LPC_ORDER];
    for i in 0..ord {
        rnum[i] = num[ord - i - 1];
    }
    for i in 0..n {
        let mut sum = x[x_pos + i];
        for j in 0..ord {
            sum += rnum[j] * x[x_pos + i + j - ord];
        }
        y[i] = sum;
    }
}

/// C: `celt_iir` (`SMALL_FOOTPRINT` branch: portable, not the `xcorr_kernel`-unrolled one).
/// `mem` (length `ord`) is both the incoming filter memory and is updated in place for the next
/// call, matching the C calling convention.
pub fn celt_iir(x: &[f32], den: &[f32], y: &mut [f32], n: i32, ord: i32, mem: &mut [f32]) {
    let n = n as usize;
    let ord = ord as usize;
    for i in 0..n {
        let mut sum = x[i];
        for j in 0..ord {
            sum -= den[j] * mem[j];
        }
        for j in (1..ord).rev() {
            mem[j] = mem[j - 1];
        }
        if ord > 0 {
            mem[0] = sum;
        }
        y[i] = sum;
    }
}

/// C: `_celt_autocorr`. Computes `lag+1` autocorrelation values of `x[..n]` (optionally
/// windowed over the first/last `overlap` samples) into `ac`. The C fixed-point-only `shift`
/// return value is always `0` in the float build and is dropped here to match the wave-0
/// signature.
pub fn celt_autocorr(x: &[f32], ac: &mut [f32], window: Option<&[f32]>, overlap: i32, lag: i32, n: i32) {
    let n = n as usize;
    let lag = lag as usize;
    let overlap = overlap as usize;
    let fast_n = n - lag;
    assert!(n <= MAX_AUTOCORR_N, "_celt_autocorr: n={n} exceeds the static scratch buffer size");

    let mut xx = [0f32; MAX_AUTOCORR_N];
    let xptr: &[f32] = if overlap == 0 {
        &x[..n]
    }
    else {
        let window = window.expect("_celt_autocorr: window required when overlap > 0");
        xx[..n].copy_from_slice(&x[..n]);
        for i in 0..overlap {
            xx[i] = x[i] * window[i];
            xx[n - i - 1] = x[n - i - 1] * window[i];
        }
        &xx[..n]
    };

    celt_pitch_xcorr(xptr, xptr, &mut ac[..=lag], fast_n, lag + 1);
    for k in 0..=lag {
        let mut d = 0f32;
        for i in (k + fast_n)..n {
            d += xptr[i] * xptr[i - k];
        }
        ac[k] += d;
    }
}
