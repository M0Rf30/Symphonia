// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-band analysis and quantisation (encoder side). Ported from libopus `celt/bands.c`
//! (`compute_band_energies`, `normalise_bands`, `hysteresis_decision`, `spreading_decision`,
//! `intensity_stereo`, `stereo_split`, `compute_theta`, `quant_partition`, `quant_band`,
//! `quant_band_stereo`, `quant_all_bands`) and `celt/vq.c` (`op_pvq_search_c`, `alg_quant`),
//! float build, BSD-3-Clause, see NOTICE.
//!
//! # Simplifications relative to libopus
//!
//! libopus runs the same `quant_all_bands` for encoding and decoding and, in the encoder, only
//! re-synthesises the decoded spectrum when `resynth` is on (never in a plain encode) or for the
//! `theta_rdo` search (complexity >= 8). This port never re-synthesises, therefore:
//!
//! - the spectral-folding bookkeeping (`norm`, `lowband`, `fill`, `lowband_offset`) and the
//!   collapse masks are not tracked. They only feed the decoder-side noise filling and
//!   anti-collapse, which the encoder never needs to mirror in order to produce the bitstream;
//! - the gain parameter of `quant_band` is dropped (it only scales the re-synthesised output);
//! - `theta_rdo` (the two-pass "round theta down/up" stereo search) is not implemented.
//!
//! The bitstream produced is therefore exactly what libopus would produce at complexity < 8.

use super::entenc::RangeEncoder;
use crate::celt::bands::{
    QTHETA_OFFSET, QTHETA_OFFSET_TWOPHASE, SPREAD_AGGRESSIVE, SPREAD_LIGHT, SPREAD_NONE,
    SPREAD_NORMAL, bitexact_cos, bitexact_log2tan, compute_qn, deinterleave_hadamard, frac_mul16,
    haar1,
};
use crate::celt::cwrs::encode_pulses;
use crate::celt::modes::{CeltMode, EPSILON};
use crate::celt::rate::{bits2pulses, get_pulses, pulses2bits};
use crate::celt::vq::{exp_rotation, stereo_itheta};
use crate::range::BITRES;

/// Largest single-band (post-split) vector the PVQ search ever sees: a full 20 ms frame.
const MAX_BAND_N: usize = 960;

/// C: `compute_band_energies` (float build). `freq` holds `channels` blocks of `N` coefficients.
pub(crate) fn compute_band_energies(
    m: &CeltMode,
    freq: &[f32],
    band_e: &mut [f32],
    end: i32,
    channels: i32,
    lm: i32,
) {
    let n = (m.short_mdct_size << lm) as usize;
    let nb = m.nb_ebands as usize;
    for c in 0..channels as usize {
        for i in 0..end as usize {
            let lo = c * n + ((m.e_bands[i] as usize) << lm);
            let hi = c * n + ((m.e_bands[i + 1] as usize) << lm);
            let band = &freq[lo..hi];
            let sum = 1e-27f32 + band.iter().map(|v| v * v).sum::<f32>();
            band_e[i + c * nb] = sum.sqrt();
        }
    }
}

/// C: `normalise_bands` (float build): scales each band of `freq` to unit energy into `x`.
pub(crate) fn normalise_bands(
    m: &CeltMode,
    freq: &[f32],
    x: &mut [f32],
    band_e: &[f32],
    end: i32,
    channels: i32,
    shift: i32,
) {
    let n = (shift * m.short_mdct_size) as usize;
    let nb = m.nb_ebands as usize;
    for c in 0..channels as usize {
        for i in 0..end as usize {
            let g = 1.0 / (1e-27f32 + band_e[i + c * nb]);
            let lo = c * n + (shift * m.e_bands[i] as i32) as usize;
            let hi = c * n + (shift * m.e_bands[i + 1] as i32) as usize;
            for j in lo..hi {
                x[j] = freq[j] * g;
            }
        }
    }
}

/// C: `hysteresis_decision`.
pub(crate) fn hysteresis_decision(
    val: f32,
    thresholds: &[f32],
    hysteresis: &[f32],
    prev: usize,
) -> usize {
    let n = thresholds.len();
    let mut i = 0;
    while i < n {
        if val < thresholds[i] {
            break;
        }
        i += 1;
    }
    if i > prev && val < thresholds[prev] + hysteresis[prev] {
        i = prev;
    }
    if i < prev && val > thresholds[prev - 1] - hysteresis[prev - 1] {
        i = prev;
    }
    i
}

/// C: `spreading_decision` (without the `hf_average`/tapset part, which only matters when the
/// pitch pre-filter is on). `average` is the persistent recursive average.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spreading_decision(
    m: &CeltMode,
    x: &[f32],
    average: &mut i32,
    last_decision: i32,
    end: i32,
    channels: i32,
    shift: i32,
    spread_weight: &[i32],
) -> i32 {
    let n0 = (shift * m.short_mdct_size) as usize;
    if shift * (m.e_bands[end as usize] as i32 - m.e_bands[end as usize - 1] as i32) <= 8 {
        return SPREAD_NONE;
    }
    let mut sum = 0i32;
    let mut nb_bands = 0i32;
    for c in 0..channels as usize {
        for i in 0..end as usize {
            let n = shift * (m.e_bands[i + 1] as i32 - m.e_bands[i] as i32);
            if n <= 8 {
                continue;
            }
            let base = shift as usize * m.e_bands[i] as usize + c * n0;
            let band = &x[base..base + n as usize];
            // Rough CDF of |x[j]|.
            let mut tcount = [0i32; 3];
            for &v in band {
                let x2n = v * v * n as f32;
                if x2n < 0.25 {
                    tcount[0] += 1;
                }
                if x2n < 0.0625 {
                    tcount[1] += 1;
                }
                if x2n < 0.015625 {
                    tcount[2] += 1;
                }
            }
            let tmp = (2 * tcount[2] >= n) as i32
                + (2 * tcount[1] >= n) as i32
                + (2 * tcount[0] >= n) as i32;
            sum += tmp * spread_weight[i];
            nb_bands += spread_weight[i];
        }
    }
    debug_assert!(nb_bands > 0);
    let mut sum = (sum << 8) / nb_bands;
    // Recursive averaging.
    sum = (sum + *average) >> 1;
    *average = sum;
    // Hysteresis.
    sum = (3 * sum + (((3 - last_decision) << 7) + 64) + 2) >> 2;
    if sum < 80 {
        SPREAD_AGGRESSIVE
    }
    else if sum < 256 {
        SPREAD_NORMAL
    }
    else if sum < 384 {
        SPREAD_LIGHT
    }
    else {
        SPREAD_NONE
    }
}

/// Number of lanes the pulse search tests per block (see [`op_pvq_search`]).
const SEARCH_BLOCK: usize = 8;

/// C: `op_pvq_search_c` (float build). Finds the integer vector `iy` with `k` pulses that best
/// matches the direction of `x`. `x` is overwritten (sign removed). `CAP` is the capacity of the
/// stack scratch buffers (>= `n`); [`alg_quant`] picks the smallest that fits so small bands do
/// not pay for clearing a full-frame-sized buffer.
fn op_pvq_search<const CAP: usize>(x: &mut [f32], iy: &mut [i32], k: i32, n: usize) {
    debug_assert!(n <= CAP);
    let mut y_buf = [0f32; CAP];
    let mut signx_buf = [false; CAP];
    let y = &mut y_buf[..n];
    let signx = &mut signx_buf[..n];
    let x = &mut x[..n];
    let iy = &mut iy[..n];

    // Get rid of the sign.
    for j in 0..n {
        signx[j] = x[j] < 0.0;
        x[j] = x[j].abs();
        iy[j] = 0;
    }

    let mut xy = 0.0f32;
    let mut yy = 0.0f32;
    let mut pulses_left = k;

    // Do a pre-search by projecting on the pyramid.
    if k > (n as i32 >> 1) {
        let mut sum: f32 = x.iter().sum();
        // If X is too small (or not finite), just replace it with a pulse at 0. 64 is an
        // approximation of infinity here.
        if !(sum > EPSILON && sum < 64.0) {
            x[0] = 1.0;
            for v in x[1..].iter_mut() {
                *v = 0.0;
            }
            sum = 1.0;
        }
        // Using K+e with e < 1 guarantees we cannot get more than K pulses.
        let rcp = (k as f32 + 0.8) * (1.0 / sum);
        for j in 0..n {
            // `rcp * x[j]` is finite and non-negative here (`x` was made non-negative above and
            // `sum` is bounded), so truncation equals `floor`.
            let q = (rcp * x[j]) as i32;
            iy[j] = q;
            let yj = q as f32;
            yy += yj * yj;
            xy += x[j] * yj;
            y[j] = yj * 2.0;
            pulses_left -= q;
        }
    }
    debug_assert!(pulses_left >= 0);

    // This should never happen, but just in case it does (e.g. on silence) we fill the first
    // bin with pulses.
    if pulses_left > n as i32 + 3 {
        let tmp = pulses_left as f32;
        yy += tmp * tmp;
        yy += tmp * y[0];
        iy[0] += pulses_left;
        pulses_left = 0;
    }

    for _ in 0..pulses_left {
        // The squared magnitude term gets added anyway, so add it outside the loop.
        yy += 1.0;
        let mut best_id = 0usize;
        let rxy = xy + x[0];
        let ryy = yy + y[0];
        let mut best_num = rxy * rxy;
        let mut best_den = ryy;

        // The sequential scan updates `best_*` rarely, so test a whole block of candidates
        // against the current best with branch-free (vectorisable) arithmetic and fall back to
        // the exact scalar scan only for blocks that contain an improvement. Candidates are
        // evaluated with the same expressions in the same order as the plain loop, and `best_*`
        // cannot change before the first improving candidate, so the result is identical.
        let mut j = 1usize;
        while j + SEARCH_BLOCK <= n {
            let xb = &x[j..j + SEARCH_BLOCK];
            let yb = &y[j..j + SEARCH_BLOCK];
            let mut any = false;
            for l in 0..SEARCH_BLOCK {
                let rxy = xy + xb[l];
                let ryy = yy + yb[l];
                let num = rxy * rxy;
                any |= best_den * num > ryy * best_num;
            }
            if any {
                for l in 0..SEARCH_BLOCK {
                    let rxy = xy + xb[l];
                    let ryy = yy + yb[l];
                    let num = rxy * rxy;
                    if best_den * num > ryy * best_num {
                        best_den = ryy;
                        best_num = num;
                        best_id = j + l;
                    }
                }
            }
            j += SEARCH_BLOCK;
        }
        while j < n {
            let rxy = xy + x[j];
            let ryy = yy + y[j];
            let num = rxy * rxy;
            // num/ryy >= best_num/best_den without a division.
            if best_den * num > ryy * best_num {
                best_den = ryy;
                best_num = num;
                best_id = j;
            }
            j += 1;
        }
        xy += x[best_id];
        yy += y[best_id];
        y[best_id] += 2.0;
        iy[best_id] += 1;
    }

    // Put the original sign back.
    for j in 0..n {
        if signx[j] {
            iy[j] = -iy[j];
        }
    }
}

/// [`alg_quant`] with a `CAP`-sized pulse-vector scratch buffer.
fn alg_quant_cap<const CAP: usize>(
    x: &mut [f32],
    n: i32,
    k: i32,
    spread: i32,
    blocks: i32,
    enc: &mut RangeEncoder,
) {
    let mut iy = [0i32; CAP];
    let iy = &mut iy[..n as usize];
    exp_rotation(x, n, 1, blocks, k, spread);
    op_pvq_search::<CAP>(x, iy, k, n as usize);
    encode_pulses(iy, n, k, enc);
}

/// C: `alg_quant` without resynthesis: PVQ-encodes the `n`-dimensional band `x` with `k` pulses.
fn alg_quant(x: &mut [f32], n: i32, k: i32, spread: i32, blocks: i32, enc: &mut RangeEncoder) {
    debug_assert!(k > 0 && n > 1 && n as usize <= MAX_BAND_N);
    match n as usize {
        0..=16 => alg_quant_cap::<16>(x, n, k, spread, blocks, enc),
        17..=48 => alg_quant_cap::<48>(x, n, k, spread, blocks, enc),
        49..=160 => alg_quant_cap::<160>(x, n, k, spread, blocks, enc),
        _ => alg_quant_cap::<MAX_BAND_N>(x, n, k, spread, blocks, enc),
    }
}

/// C: `intensity_stereo` (float build): downmixes `x`/`y` into `x` weighted by the channel
/// energies.
fn intensity_stereo(band_e: &[f32], nb_ebands: usize, band: usize, x: &mut [f32], y: &[f32]) {
    let left = band_e[band];
    let right = band_e[band + nb_ebands];
    let norm = EPSILON + (EPSILON + left * left + right * right).sqrt();
    let a1 = left / norm;
    let a2 = right / norm;
    for (xv, &yv) in x.iter_mut().zip(y.iter()) {
        *xv = a1 * *xv + a2 * yv;
    }
}

/// C: `stereo_split`: L/R to mid/side (each scaled by 1/sqrt(2)).
fn stereo_split(x: &mut [f32], y: &mut [f32]) {
    const C: f32 = std::f32::consts::FRAC_1_SQRT_2;
    for (xv, yv) in x.iter_mut().zip(y.iter_mut()) {
        let l = C * *xv;
        let r = C * *yv;
        *xv = l + r;
        *yv = r - l;
    }
}

/// C: `struct band_ctx`, encode-only subset.
struct BandCtx<'a> {
    m: &'static CeltMode,
    i: i32,
    intensity: i32,
    spread: i32,
    tf_change: i32,
    enc: &'a mut RangeEncoder,
    remaining_bits: i32,
    band_e: &'a [f32],
    avoid_split_noise: bool,
}

/// C: `struct split_ctx`.
struct SplitCtx {
    delta: i32,
    itheta: i32,
    qalloc: i32,
}

/// C: `compute_theta`, encode direction. For stereo bands `x`/`y` are the L/R channels and are
/// converted in place into the signal actually coded (mid/side, or the intensity downmix).
#[allow(clippy::too_many_arguments)]
fn compute_theta(
    ctx: &mut BandCtx<'_>,
    x: &mut [f32],
    y: &mut [f32],
    n: i32,
    b: &mut i32,
    b0: i32,
    lm: i32,
    stereo: bool,
) -> SplitCtx {
    let i = ctx.i;
    let nb = ctx.m.nb_ebands as usize;

    // Decide on the resolution to give to the split parameter theta.
    let pulse_cap = ctx.m.log_n[i as usize] as i32 + lm * (1 << BITRES);
    let offset =
        (pulse_cap >> 1) - if stereo && n == 2 { QTHETA_OFFSET_TWOPHASE } else { QTHETA_OFFSET };
    let mut qn = compute_qn(n, *b, offset, pulse_cap, stereo);
    if stereo && i >= ctx.intensity {
        qn = 1;
    }

    // theta is the atan() of the ratio between the (normalized) side and mid.
    let mut itheta = stereo_itheta(x, y, stereo, n);
    let tell = ctx.enc.tell_frac();

    if qn != 1 {
        itheta = (itheta * qn + 8192) >> 14;
        if !stereo && ctx.avoid_split_noise && itheta > 0 && itheta < qn {
            // Check if the selected value of theta will cause the bit allocation to inject
            // noise on one side. If so, make sure the energy of that side is zero.
            let unquantized = itheta * 16384 / qn;
            let imid = bitexact_cos(unquantized as i16) as i32;
            let iside = bitexact_cos((16384 - unquantized) as i16) as i32;
            let delta = frac_mul16((n - 1) << 7, bitexact_log2tan(iside, imid));
            if delta > *b {
                itheta = qn;
            }
            else if delta < -*b {
                itheta = 0;
            }
        }
        // Entropy coding of the angle: a step pdf for stereo, uniform for the time split and a
        // triangular one for the rest.
        if stereo && n > 2 {
            let p0 = 3;
            let xv = itheta;
            let x0 = qn / 2;
            let ft = p0 * (x0 + 1) + x0;
            let (fl, fh) = if xv <= x0 {
                (p0 * xv, p0 * (xv + 1))
            }
            else {
                ((xv - 1 - x0) + (x0 + 1) * p0, (xv - x0) + (x0 + 1) * p0)
            };
            ctx.enc.encode(fl as u32, fh as u32, ft as u32);
        }
        else if b0 > 1 || stereo {
            ctx.enc.enc_uint(itheta as u32, (qn + 1) as u32);
        }
        else {
            let ft = ((qn >> 1) + 1) * ((qn >> 1) + 1);
            let (fl, fs) = if itheta <= (qn >> 1) {
                ((itheta * (itheta + 1)) >> 1, itheta + 1)
            }
            else {
                (ft - (((qn + 1 - itheta) * (qn + 2 - itheta)) >> 1), qn + 1 - itheta)
            };
            ctx.enc.encode(fl as u32, (fl + fs) as u32, ft as u32);
        }
        itheta = itheta * 16384 / qn;
        if stereo {
            if itheta == 0 {
                intensity_stereo(ctx.band_e, nb, i as usize, x, y);
            }
            else {
                stereo_split(x, y);
            }
        }
    }
    else if stereo {
        let inv = itheta > 8192;
        if inv {
            for v in y.iter_mut() {
                *v = -*v;
            }
        }
        intensity_stereo(ctx.band_e, nb, i as usize, x, y);
        if *b > 2 << BITRES && ctx.remaining_bits > 2 << BITRES {
            ctx.enc.enc_bit_logp(inv, 2);
        }
        itheta = 0;
    }
    let qalloc = ctx.enc.tell_frac() - tell;
    *b -= qalloc;

    let delta = if itheta == 0 {
        -16384
    }
    else if itheta == 16384 {
        16384
    }
    else {
        let imid = bitexact_cos(itheta as i16) as i32;
        let iside = bitexact_cos((16384 - itheta) as i16) as i32;
        // The mid vs side allocation that minimises squared error in that band.
        frac_mul16((n - 1) << 7, bitexact_log2tan(iside, imid))
    };
    SplitCtx { delta, itheta, qalloc }
}

/// C: `quant_band_n1`, encode direction: a single sign bit per channel.
fn quant_band_n1(ctx: &mut BandCtx<'_>, x: &[f32], y: Option<&[f32]>) {
    for ch in std::iter::once(x).chain(y) {
        if ctx.remaining_bits >= 1 << BITRES {
            ctx.enc.enc_bits((ch[0] < 0.0) as u32, 1);
            ctx.remaining_bits -= 1 << BITRES;
        }
    }
}

/// C: `quant_partition`, encode direction.
fn quant_partition(ctx: &mut BandCtx<'_>, x: &mut [f32], n: i32, b: i32, bb: i32, lm: i32) {
    let i = ctx.i;
    let cache_base = ctx.m.cache.index[((lm + 1) * ctx.m.nb_ebands + i) as usize] as usize;
    let cache = &ctx.m.cache.bits[cache_base..];

    // If we need 1.5 more bit than we can produce, split the band in two.
    if lm != -1 && b > cache[cache[0] as usize] as i32 + 12 && n > 2 {
        let mut b = b;
        let n_half = n >> 1;
        let (x_lo, x_hi) = x.split_at_mut(n_half as usize);
        let lm2 = lm - 1;
        let b0 = bb;
        let bb2 = (bb + 1) >> 1;

        let sctx = compute_theta(ctx, x_lo, x_hi, n_half, &mut b, b0, lm2, false);
        let mut delta = sctx.delta;
        let itheta = sctx.itheta;
        let qalloc = sctx.qalloc;

        // Give more bits to low-energy MDCTs than they would otherwise deserve.
        if b0 > 1 && (itheta & 0x3fff) != 0 {
            if itheta > 8192 {
                // Rough approximation for pre-echo masking.
                delta -= delta >> (4 - lm2);
            }
            else {
                // Corresponds to a forward-masking slope of 1.5 dB per 10 ms.
                delta = 0.min(delta + ((n_half << BITRES) >> (5 - lm2)));
            }
        }
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= qalloc;

        let rebalance = ctx.remaining_bits;
        if mbits >= sbits {
            quant_partition(ctx, x_lo, n_half, mbits, bb2, lm2);
            let rebalance2 = mbits - (rebalance - ctx.remaining_bits);
            if rebalance2 > 3 << BITRES && itheta != 0 {
                sbits += rebalance2 - (3 << BITRES);
            }
            quant_partition(ctx, x_hi, n_half, sbits, bb2, lm2);
        }
        else {
            quant_partition(ctx, x_hi, n_half, sbits, bb2, lm2);
            let rebalance2 = sbits - (rebalance - ctx.remaining_bits);
            if rebalance2 > 3 << BITRES && itheta != 16384 {
                mbits += rebalance2 - (3 << BITRES);
            }
            quant_partition(ctx, x_lo, n_half, mbits, bb2, lm2);
        }
    }
    else {
        // The basic no-split case.
        let mut q = bits2pulses(ctx.m, i, lm, b);
        let mut curr_bits = pulses2bits(ctx.m, i, lm, q);
        ctx.remaining_bits -= curr_bits;

        // Ensures we can never bust the budget.
        while ctx.remaining_bits < 0 && q > 0 {
            ctx.remaining_bits += curr_bits;
            q -= 1;
            curr_bits = pulses2bits(ctx.m, i, lm, q);
            ctx.remaining_bits -= curr_bits;
        }

        if q != 0 {
            let k = get_pulses(q);
            alg_quant(x, n, k, ctx.spread, bb, ctx.enc);
        }
        // Otherwise the decoder fills the band with noise/folding: nothing to code.
    }
}

/// C: `quant_band`, encode direction, mono (or one channel of a dual-stereo/split pair).
fn quant_band(ctx: &mut BandCtx<'_>, x: &mut [f32], n: i32, b: i32, bb: i32, lm: i32) {
    let n0 = n;
    let mut n_b = n / bb;
    let mut bb = bb;
    let mut time_divide = 0;
    let long_blocks = bb == 1;
    let mut tf_change = ctx.tf_change;

    // Special case for one sample.
    if n == 1 {
        quant_band_n1(ctx, x, None);
        return;
    }

    let recombine = tf_change.max(0);
    // Band recombining to increase frequency resolution.
    for k in 0..recombine {
        haar1(x, n0 >> k, 1 << k);
    }
    bb >>= recombine;
    n_b <<= recombine;

    // Increasing the time resolution.
    while (n_b & 1) == 0 && tf_change < 0 {
        haar1(x, n_b, bb);
        bb <<= 1;
        n_b >>= 1;
        time_divide += 1;
        tf_change += 1;
    }
    let _ = time_divide;
    let b0 = bb;

    // Reorganize the samples in time order instead of frequency order.
    if b0 > 1 {
        deinterleave_hadamard(x, n_b >> recombine, b0 << recombine, long_blocks);
    }

    quant_partition(ctx, x, n, b, bb, lm);
}

/// C: `MIN_STEREO_ENERGY` (float build).
const MIN_STEREO_ENERGY: f32 = 1e-10;

/// C: `quant_band_stereo`, encode direction.
fn quant_band_stereo(
    ctx: &mut BandCtx<'_>,
    x: &mut [f32],
    y: &mut [f32],
    n: i32,
    b: i32,
    bb: i32,
    lm: i32,
) {
    if n == 1 {
        quant_band_n1(ctx, x, Some(y));
        return;
    }
    let nb = ctx.m.nb_ebands as usize;
    let i = ctx.i as usize;
    if ctx.band_e[i] < MIN_STEREO_ENERGY || ctx.band_e[nb + i] < MIN_STEREO_ENERGY {
        if ctx.band_e[i] > ctx.band_e[nb + i] {
            y[..n as usize].copy_from_slice(&x[..n as usize]);
        }
        else {
            x[..n as usize].copy_from_slice(&y[..n as usize]);
        }
    }

    let mut b = b;
    let sctx = compute_theta(ctx, x, y, n, &mut b, bb, lm, true);
    let delta = sctx.delta;
    let itheta = sctx.itheta;
    let qalloc = sctx.qalloc;

    if n == 2 {
        // Special case for N=2 that only works for stereo: mid and side are orthogonal, so
        // the side needs only a sign bit.
        let mut mbits = b;
        let mut sbits = 0;
        // Only need one bit for the side.
        if itheta != 0 && itheta != 16384 {
            sbits = 1 << BITRES;
        }
        mbits -= sbits;
        let c = itheta > 8192;
        ctx.remaining_bits -= qalloc + sbits;

        let (x2, y2): (&mut [f32], &mut [f32]) = if c { (y, x) } else { (x, y) };
        if sbits != 0 {
            let sign = x2[0] * y2[1] - x2[1] * y2[0] < 0.0;
            ctx.enc.enc_bits(sign as u32, 1);
        }
        quant_band(ctx, x2, n, mbits, bb, lm);
    }
    else {
        // "Normal" split code.
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= qalloc;

        let rebalance = ctx.remaining_bits;
        if mbits >= sbits {
            quant_band(ctx, x, n, mbits, bb, lm);
            let rebalance2 = mbits - (rebalance - ctx.remaining_bits);
            if rebalance2 > 3 << BITRES && itheta != 0 {
                sbits += rebalance2 - (3 << BITRES);
            }
            quant_band(ctx, y, n, sbits, bb, lm);
        }
        else {
            quant_band(ctx, y, n, sbits, bb, lm);
            let rebalance2 = sbits - (rebalance - ctx.remaining_bits);
            if rebalance2 > 3 << BITRES && itheta != 16384 {
                mbits += rebalance2 - (3 << BITRES);
            }
            quant_band(ctx, x, n, mbits, bb, lm);
        }
    }
}

/// Parameters of one [`quant_all_bands`] call that are not buffers.
pub(crate) struct QuantParams<'a> {
    pub start: i32,
    pub end: i32,
    pub pulses: &'a [i32],
    pub short_blocks: bool,
    pub spread: i32,
    pub dual_stereo: bool,
    pub intensity: i32,
    pub tf_res: &'a [i32],
    pub total_bits: i32,
    pub balance: i32,
    pub lm: i32,
    pub coded_bands: i32,
}

/// C: `quant_all_bands` (`encode = 1`, no resynthesis). `x` holds the normalised left/mono
/// spectrum (`N` coefficients), `y` the normalised right one when coding stereo.
pub(crate) fn quant_all_bands(
    mode: &'static CeltMode,
    p: &QuantParams<'_>,
    x: &mut [f32],
    mut y: Option<&mut [f32]>,
    band_e: &[f32],
    enc: &mut RangeEncoder,
) {
    let e_bands = mode.e_bands;
    let mshift = 1i32 << p.lm;
    let b_blocks = if p.short_blocks { mshift } else { 1 };
    let mut balance = p.balance;
    let mut dual_stereo = p.dual_stereo;

    let mut ctx = BandCtx {
        m: mode,
        i: p.start,
        intensity: p.intensity,
        spread: p.spread,
        tf_change: 0,
        enc,
        remaining_bits: 0,
        band_e,
        // Avoid injecting noise in the first band on transients.
        avoid_split_noise: b_blocks > 1,
    };

    for i in p.start..p.end {
        ctx.i = i;
        let lo = (mshift * e_bands[i as usize] as i32) as usize;
        let hi = (mshift * e_bands[i as usize + 1] as i32) as usize;
        let n = (hi - lo) as i32;
        debug_assert!(n > 0);
        let tell = ctx.enc.tell_frac();

        // Compute how many bits we want to allocate to this band.
        if i != p.start {
            balance -= tell;
        }
        let remaining_bits = p.total_bits - tell - 1;
        ctx.remaining_bits = remaining_bits;
        let b = if i < p.coded_bands {
            let curr_balance = balance / 3.min(p.coded_bands - i);
            0.max((remaining_bits + 1).min(p.pulses[i as usize] + curr_balance)).min(16383)
        }
        else {
            0
        };
        ctx.tf_change = p.tf_res[i as usize];

        // Switch off dual stereo to do intensity.
        if dual_stereo && i == p.intensity {
            dual_stereo = false;
        }

        let xb = &mut x[lo..hi];
        if dual_stereo {
            let yb = &mut y.as_deref_mut().expect("dual stereo needs two channels")[lo..hi];
            quant_band(&mut ctx, xb, n, b / 2, b_blocks, p.lm);
            quant_band(&mut ctx, yb, n, b / 2, b_blocks, p.lm);
        }
        else if let Some(yfull) = y.as_deref_mut() {
            quant_band_stereo(&mut ctx, xb, &mut yfull[lo..hi], n, b, b_blocks, p.lm);
        }
        else {
            quant_band(&mut ctx, xb, n, b, b_blocks, p.lm);
        }

        balance += p.pulses[i as usize] + tell;
        // We only need to avoid noise on a split for the first band.
        ctx.avoid_split_noise = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::celt::cwrs::decode_pulses;
    use crate::range::RangeDecoder;

    #[test]
    fn pvq_search_places_exactly_k_pulses_and_round_trips() {
        for &(n, k) in &[
            (2usize, 1i32),
            (4, 3),
            (8, 5),
            (16, 2),
            (16, 12),
            (44, 6),
            (176, 4),
            (8, 36),
            (2, 128),
        ] {
            let mut x: Vec<f32> = (0..n).map(|i| ((i * 29 + 7) % 13) as f32 - 6.0 + 0.1).collect();
            let norm = x.iter().map(|v| v * v).sum::<f32>().sqrt();
            for v in x.iter_mut() {
                *v /= norm;
            }
            let mut iy = vec![0i32; n];
            let mut xs = x.clone();
            op_pvq_search::<MAX_BAND_N>(&mut xs, &mut iy, k, n);
            assert_eq!(iy.iter().map(|v| v.abs()).sum::<i32>(), k, "n={n} k={k}");
            // Signs follow the input.
            for j in 0..n {
                assert!(iy[j] == 0 || (iy[j] < 0) == (x[j] < 0.0));
            }
            let mut enc = RangeEncoder::new(256);
            encode_pulses(&iy, n as i32, k, &mut enc);
            let (buf, err) = enc.done();
            assert!(!err);
            let mut dec = RangeDecoder::new(&buf);
            let mut out = vec![0i32; n];
            decode_pulses(&mut out, n as i32, k, &mut dec);
            assert_eq!(out, iy);
        }
    }

    #[test]
    fn pvq_search_handles_degenerate_input() {
        let mut x = vec![0.0f32; 8];
        let mut iy = vec![0i32; 8];
        op_pvq_search::<MAX_BAND_N>(&mut x, &mut iy, 6, 8);
        assert_eq!(iy.iter().map(|v| v.abs()).sum::<i32>(), 6);
        let mut x = vec![f32::NAN; 8];
        op_pvq_search::<MAX_BAND_N>(&mut x, &mut iy, 3, 8);
        assert_eq!(iy.iter().map(|v| v.abs()).sum::<i32>(), 3);
    }

    #[test]
    fn hysteresis_keeps_previous_inside_band() {
        let th = [1.0f32, 2.0, 3.0];
        let hy = [0.5f32, 0.5, 0.5];
        assert_eq!(hysteresis_decision(0.5, &th, &hy, 0), 0);
        assert_eq!(hysteresis_decision(1.2, &th, &hy, 0), 0);
        assert_eq!(hysteresis_decision(1.6, &th, &hy, 0), 1);
        assert_eq!(hysteresis_decision(0.8, &th, &hy, 1), 1);
        assert_eq!(hysteresis_decision(0.4, &th, &hy, 1), 0);
        assert_eq!(hysteresis_decision(9.0, &th, &hy, 3), 3);
    }

    #[test]
    fn energies_and_normalisation_are_consistent() {
        let m = &crate::celt::modes::MODE_48000_960;
        let n = 960;
        let freq: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.37).sin() * 1000.0).collect();
        let mut e = vec![0f32; 21];
        compute_band_energies(m, &freq, &mut e, 21, 1, 3);
        let mut x = vec![0f32; n];
        normalise_bands(m, &freq, &mut x, &e, 21, 1, 8);
        for i in 0..21 {
            let lo = 8 * m.e_bands[i] as usize;
            let hi = 8 * m.e_bands[i + 1] as usize;
            let en: f32 = x[lo..hi].iter().map(|v| v * v).sum();
            assert!((en - 1.0).abs() < 1e-3, "band {i}: {en}");
        }
    }
}
