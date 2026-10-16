// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RealAudio 2.0 (28.8K) core.
//!
//! A G.728 LD-CELP decoder (ITU-T G.728, blocks 36 and 46-49) ported from FFmpeg's
//! `libavcodec/ra288.c`, `libavcodec/g728_template.c` and `libavcodec/lpc_functions.h`. Samples are
//! produced as 32-bit floats scaled like the reference. Like the reference, the arithmetic mixes
//! single and double precision; the same precision is used at every step here.

use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::{BitReaderRtl, ReadBitsRtl};

use crate::tables288::{AMP_TABLE, CODE_TABLE, GAIN_BW_TAB, GAIN_WINDOW, SYN_BW_TAB, SYN_WINDOW};

/// Size of an encoded frame in bytes (the `block_align` required by the reference).
pub(crate) const FRAME_SIZE: usize = 38;
/// Samples per vector.
const BLOCK_SIZE: usize = 5;
/// Vectors per frame.
const BLOCKS_PER_FRAME: usize = 32;
/// Number of samples decoded from one frame.
pub(crate) const FRAME_SAMPLES: usize = BLOCK_SIZE * BLOCKS_PER_FRAME;

const SP_ORDER: usize = 36;
const GAIN_ORDER: usize = 10;
/// Total length of the speech history, `order + n + non_rec` of the speech hybrid window.
const SP_HIST_LEN: usize = 111;
/// Total length of the log-gain history.
const GAIN_HIST_LEN: usize = 38;
/// Offset of the vector being decoded in the speech history: 70 backward-filter samples plus 36
/// samples of synthesis filter memory.
const SP_BLOCK: usize = 70 + SP_ORDER;
/// Offset of the vector being decoded in the log-gain history.
const GAIN_BLOCK: usize = 28;

/// Hybrid window attenuation (spec: ALPHA).
const ATTEN: f64 = 0.5625;

/// Dot product of equal-length slices, accumulated sequentially in single precision
/// (`ff_scalarproduct_float_c`).
fn scalar_product(a: &[f32], b: &[f32]) -> f32 {
    let mut p = 0.0f32;
    for (&x, &y) in a.iter().zip(b) {
        p += x * y;
    }
    p
}

/// `tgt[n] = sum_i src[off + i] * src[off + i - n]` for `n` in `0..=order` (`convolve`).
fn convolve(tgt: &mut [f32], work: &[f32], off: usize, len: usize, order: usize) {
    for (n, t) in tgt.iter_mut().enumerate().take(order + 1) {
        *t = scalar_product(&work[off..off + len], &work[off - n..off - n + len]);
    }
}

/// Hybrid window filtering, see blocks 36 and 49 of the G.728 specification (`do_hybrid_window`).
///
/// `out` receives the autocorrelation, `out2` is the recursive part of the autocorrelation.
fn hybrid_window(
    order: usize,
    n: usize,
    non_rec: usize,
    out: &mut [f32],
    hist: &[f32],
    out2: &mut [f32],
    window: &[f32],
) {
    let mut work = [0.0f32; 36 + 40 + 35];
    let total = order + n + non_rec;

    for ((w, &win), &h) in work.iter_mut().zip(window).zip(hist).take(total) {
        *w = win * h;
    }

    let mut buffer1 = [0.0f32; 37];
    let mut buffer2 = [0.0f32; 37];
    convolve(&mut buffer1, &work, order, n, order);
    convolve(&mut buffer2, &work, order + n, non_rec, order);

    for i in 0..=order {
        // `out2[i] * ATTEN` is a double-precision expression in the reference.
        out2[i] = (f64::from(out2[i]) * ATTEN + f64::from(buffer1[i])) as f32;
        out[i] = out2[i] + buffer2[i];
    }

    // Multiply by the white noise correcting factor (WNCF).
    out[0] = (f64::from(out[0]) * (257.0 / 256.0)) as f32;
}

/// Levinson-Durbin recursion (`compute_lpc_coefs` with `i = 0`, `lpc_stride = 0`, `fail = 1` and
/// `normalize = 1`). Produces LPC coefficients from autocorrelation data in `lpc[..max_order]`.
///
/// Returns `false` on failure, in which case `lpc` may have been partially updated, exactly as in
/// the reference.
fn compute_lpc_coefs(autoc: &[f32], max_order: usize, lpc: &mut [f32]) -> bool {
    let mut err = autoc[0];
    // The reference advances `autoc` past the zero-lag term.
    let autoc = &autoc[1..];

    if autoc[max_order - 1] == 0.0 || err <= 0.0 {
        return false;
    }

    for i in 0..max_order {
        let mut r = -autoc[i];

        for j in 0..i {
            r -= lpc[j] * autoc[i - j - 1];
        }

        if err != 0.0 {
            r /= err;
        }
        err *= 1.0 - (r * r);

        lpc[i] = r;

        for j in 0..(i + 1) >> 1 {
            let f = lpc[j];
            let b = lpc[i - 1 - j];
            lpc[j] = f + r * b;
            lpc[i - 1 - j] = b + r * f;
        }

        if err < 0.0 {
            return false;
        }
    }

    true
}

/// Backward-adaptive LPC analysis (`backward_filter`): finds the LPC coefficients from past data.
#[allow(clippy::too_many_arguments)]
fn backward_filter(
    hist: &mut [f32],
    rec: &mut [f32],
    window: &[f32],
    lpc: &mut [f32],
    tab: &[f32],
    order: usize,
    n: usize,
    non_rec: usize,
    move_size: usize,
) {
    let mut temp = [0.0f32; 37];

    hybrid_window(order, n, non_rec, &mut temp, hist, rec, window);

    if compute_lpc_coefs(&temp, order, lpc) {
        for (l, &t) in lpc.iter_mut().zip(tab).take(order) {
            *l *= t;
        }
    }

    hist.copy_within(n..n + move_size, 0);
}

/// All-pole synthesis filter of 5 samples: `out[n] = inp[n] - sum_i coeffs[i - 1] * out[n - i]`,
/// with `out[n]` at `hist[base + n]` (`ff_celp_lp_synthesis_filterf` with `buffer_length = 5` and
/// `filter_length = 36`).
///
/// Mirrors the reference's unrolled evaluation order for the first four samples, which differs
/// from the plain recursion in single-precision rounding only; the fifth sample uses the plain
/// recursion, as in the reference.
fn lp_synthesis_filter(
    hist: &mut [f32; SP_HIST_LEN],
    base: usize,
    coeffs: &[f32; SP_ORDER],
    inp: &[f32; BLOCK_SIZE],
) {
    // Fold the first three coefficients into the four-sample look-ahead terms.
    let a = coeffs[0];
    let b = coeffs[1] - coeffs[0] * coeffs[0];
    let mut c = coeffs[2] - coeffs[1] * coeffs[0];
    c -= coeffs[0] * b;

    let mut old0 = hist[base - 4];
    let mut old1 = hist[base - 3];
    let mut old2 = hist[base - 2];
    let mut old3 = hist[base - 1];

    let mut out0 = inp[0];
    let mut out1 = inp[1];
    let mut out2 = inp[2];
    let mut out3 = inp[3];

    out0 -= coeffs[2] * old1;
    out1 -= coeffs[2] * old2;
    out2 -= coeffs[2] * old3;

    out0 -= coeffs[1] * old2;
    out1 -= coeffs[1] * old3;

    out0 -= coeffs[0] * old3;

    let val = coeffs[3];
    out0 -= val * old0;
    out1 -= val * old1;
    out2 -= val * old2;
    out3 -= val * old3;

    for i in (5..SP_ORDER).step_by(2) {
        old3 = hist[base - i];
        let val = coeffs[i - 1];

        out0 -= val * old3;
        out1 -= val * old0;
        out2 -= val * old1;
        out3 -= val * old2;

        old2 = hist[base - i - 1];
        let val = coeffs[i];

        out0 -= val * old2;
        out1 -= val * old3;
        out2 -= val * old0;
        out3 -= val * old1;

        std::mem::swap(&mut old0, &mut old2);
        old1 = old3;
    }

    let (tmp0, tmp1, tmp2) = (out0, out1, out2);

    out3 -= a * tmp2;
    out2 -= a * tmp1;
    out1 -= a * tmp0;

    out3 -= b * tmp1;
    out2 -= b * tmp0;

    out3 -= c * tmp0;

    hist[base] = out0;
    hist[base + 1] = out1;
    hist[base + 2] = out2;
    hist[base + 3] = out3;

    // The remaining sample.
    let mut o = inp[4];
    for i in 1..=SP_ORDER {
        o -= coeffs[i - 1] * hist[base + 4 - i];
    }
    hist[base + 4] = o;
}

/// RealAudio 2.0 (28.8K) decoder state.
pub(crate) struct Ra288 {
    /// LPC coefficients for speech data (spec: A).
    sp_lpc: [f32; SP_ORDER],
    /// LPC coefficients for gain (spec: GB).
    gain_lpc: [f32; GAIN_ORDER],
    /// Speech data history (spec: SB). Its first 70 values are updated only at backward filtering.
    sp_hist: [f32; SP_HIST_LEN],
    /// Speech part of the gain autocorrelation (spec: REXP).
    sp_rec: [f32; SP_ORDER + 1],
    /// Log-gain history (spec: SBLG). Its first 28 values are updated only at backward filtering.
    gain_hist: [f32; GAIN_HIST_LEN],
    /// Recursive part of the gain autocorrelation (spec: REXPLG).
    gain_rec: [f32; GAIN_ORDER + 1],
}

impl Ra288 {
    pub(crate) fn new() -> Self {
        Ra288 {
            sp_lpc: [0.0; SP_ORDER],
            gain_lpc: [0.0; GAIN_ORDER],
            sp_hist: [0.0; SP_HIST_LEN],
            sp_rec: [0.0; SP_ORDER + 1],
            gain_hist: [0.0; GAIN_HIST_LEN],
            gain_rec: [0.0; GAIN_ORDER + 1],
        }
    }

    /// Decodes one vector of 5 samples, leaving it in `sp_hist[SP_BLOCK..]`.
    fn decode_vector(&mut self, gain: f32, cb_coef: usize) {
        self.sp_hist.copy_within(75..75 + SP_ORDER, 70);

        // Block 46 of the G.728 spec.
        let mut sum = 32.0f32;
        for i in 0..GAIN_ORDER {
            sum -= self.gain_hist[GAIN_BLOCK + 9 - i] * self.gain_lpc[i];
        }

        // Block 47 of the G.728 spec.
        sum = sum.clamp(0.0, 60.0);

        // Block 48 of the G.728 spec. exp(sum * 0.1151292546497) == pow(10.0, sum / 20).
        let sumsum =
            (f64::from(sum) * 0.1151292546497).exp() * f64::from(gain) * (1.0 / (1 << 23) as f64);

        let mut buffer = [0.0f32; BLOCK_SIZE];
        for (b, &c) in buffer.iter_mut().zip(&CODE_TABLE[cb_coef]) {
            *b = (f64::from(c) * sumsum) as f32;
        }

        let mut sum = scalar_product(&buffer, &buffer);

        // FFMAX(sum, 5.0 / (1 << 24)) is evaluated in double precision.
        let min_energy = 5.0 / (1 << 24) as f64;
        if f64::from(sum) <= min_energy {
            sum = min_energy as f32;
        }

        // Shift and store.
        self.gain_hist.copy_within(GAIN_BLOCK + 1..GAIN_BLOCK + 10, GAIN_BLOCK);

        self.gain_hist[GAIN_BLOCK + 9] = (10.0 * f64::from(sum).log10()
            + (10.0 * ((1 << 24) as f64 / 5.0).log10() - 32.0))
            as f32;

        lp_synthesis_filter(&mut self.sp_hist, SP_BLOCK, &self.sp_lpc, &buffer);
    }

    /// Decodes one 38-byte frame into 160 samples.
    pub(crate) fn decode_frame(&mut self, buf: &[u8], out: &mut [f32]) -> Result<()> {
        if buf.len() < FRAME_SIZE || out.len() < FRAME_SAMPLES {
            return decode_error("ra: frame too small");
        }

        // The reference reads this frame least-significant bit first.
        let mut bs = BitReaderRtl::new(&buf[..FRAME_SIZE]);

        let mut read = |n: u32| -> Result<u32> {
            match bs.read_bits_leq32(n) {
                Ok(v) => Ok(v),
                Err(_) => decode_error("ra: truncated frame"),
            }
        };

        for (i, o) in out.chunks_exact_mut(BLOCK_SIZE).take(BLOCKS_PER_FRAME).enumerate() {
            let gain = AMP_TABLE[read(3)? as usize];
            let cb_coef = read(6 + (i as u32 & 1))? as usize;

            self.decode_vector(gain, cb_coef);

            o.copy_from_slice(&self.sp_hist[SP_BLOCK..SP_BLOCK + BLOCK_SIZE]);

            if i & 7 == 3 {
                backward_filter(
                    &mut self.sp_hist,
                    &mut self.sp_rec,
                    &SYN_WINDOW,
                    &mut self.sp_lpc,
                    &SYN_BW_TAB,
                    36,
                    40,
                    35,
                    70,
                );

                backward_filter(
                    &mut self.gain_hist,
                    &mut self.gain_rec,
                    &GAIN_WINDOW,
                    &mut self.gain_lpc,
                    &GAIN_BW_TAB,
                    10,
                    8,
                    20,
                    28,
                );
            }
        }

        Ok(())
    }
}
