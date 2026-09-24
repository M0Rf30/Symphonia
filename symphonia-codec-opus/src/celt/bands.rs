// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-band decode driver. Ported from libopus `celt/bands.c` (decoder path only:
//! `quant_all_bands`, `quant_band`, `quant_band_stereo`, `quant_partition`, `quant_band_n1`,
//! `compute_theta`, `haar1`, `(de)interleave_hadamard`, `compute_qn`, `bitexact_cos`,
//! `bitexact_log2tan`, `celt_lcg_rand`, `special_hybrid_folding`, `anti_collapse`,
//! `denormalise_bands`; `intensity_stereo`/`stereo_split` are ported too for completeness but,
//! like `spreading_decision`/`hysteresis_decision`/`compute_channel_weights`, are only ever
//! called from the *encode* side of `compute_theta` in libopus and are therefore unreachable
//! from this decode-only port — kept `#[allow(dead_code)]`). Ported from libopus
//! (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltBitstream".
//!
//! # Decode-only simplifications
//!
//! libopus's `bands.c` shares this code between the encoder and decoder behind `if (encode)`/
//! `ctx->resynth` branches. Since this crate only implements a decoder, `encode` is always
//! `false` and (crucially) `resynth = !encode || theta_rdo` is therefore always `true` (the
//! `theta_rdo` early-return-encoding path, which needs an `ec_enc` to roll back to, is `encode
//! && ...` and so is always `false` here too). Consequently:
//! - `struct band_ctx`'s `encode`/`resynth`/`theta_round`/`avoid_split_noise` fields are dropped
//!   (the first two are always `false`/`true`; the other two only gate `if (encode)`-only code).
//! - `quant_all_bands`'s `theta_rdo` distortion-comparison branch (`bands.c` ~L1583-1645, which
//!   speculatively decodes/encodes a band twice and keeps the cheaper one — meaningless without
//!   an encoder) is not ported; only its `else` branch (a single `quant_band_stereo` call) is.
//! - `X_save`/`Y_save`/`X_save2`/`Y_save2`/`norm_save2`/`bytes_save` (all `theta_rdo`-only
//!   scratch) are not allocated.
//!
//! `lowband_scratch`'s C aliasing trick (borrowing unused tail memory of the caller's `X_`
//! buffer to avoid a separate allocation, skipped — via a `NULL` scratch pointer — exactly when
//! the band being decoded is the last one or beyond `effEBands`, i.e. exactly when mutating the
//! fold source in place can never be observed again) is replaced by *always* copying the fold
//! source into a dedicated scratch buffer before any in-place transform (`haar1`/
//! `deinterleave_hadamard`) touches it. This is behaviourally identical (see the two call sites
//! above: when C skips the copy, it's precisely because nothing will re-read that memory either
//! way) and avoids replicating raw pointer aliasing in safe Rust.
//!
//! Only the single 48 kHz/960-sample mode is supported (see crate docs), for which
//! `effective_ebands == nb_ebands` always and `end <= effective_ebands`; the `i >=
//! m->effEBands` branch of `quant_all_bands` (which reroutes `X`/`Y` through the scratch `norm`
//! buffer) is therefore unreachable and is asserted against rather than implemented.

use crate::celt::modes::{celt_exp2, celt_rsqrt, celt_sqrt, celt_sudiv, ec_ilog, isqrt32, CeltMode};
use crate::celt::quant_bands::E_MEANS;
use crate::celt::rate::{bits2pulses, get_pulses, pulses2bits};
use crate::celt::vq::{alg_unquant, renormalise_vector};
use crate::range::{RangeDecoder, BITRES};

/// C: `SPREAD_NONE` (`celt/bands.h`).
pub(crate) const SPREAD_NONE: i32 = 0;
/// C: `SPREAD_LIGHT` (`celt/bands.h`).
pub(crate) const SPREAD_LIGHT: i32 = 1;
/// C: `SPREAD_NORMAL` (`celt/bands.h`).
pub(crate) const SPREAD_NORMAL: i32 = 2;
/// C: `SPREAD_AGGRESSIVE` (`celt/bands.h`).
pub(crate) const SPREAD_AGGRESSIVE: i32 = 3;
/// C: `SPREAD_FACTOR` (`celt/vq.c`, local to `exp_rotation`), indexed `[spread-1]`.
pub(crate) const SPREAD_FACTOR: [i32; 3] = [15, 10, 5];

/// C: `QTHETA_OFFSET` (`celt/rate.h`).
const QTHETA_OFFSET: i32 = 4;
/// C: `QTHETA_OFFSET_TWOPHASE` (`celt/rate.h`).
const QTHETA_OFFSET_TWOPHASE: i32 = 16;
/// C: `NORM_SCALING` / `Q15ONE` (`celt/arch.h`, float build): both `1.f`.
const NORM_SCALING: f32 = 1.0;

/// C: `celt_lcg_rand` (`celt/bands.c`).
pub fn celt_lcg_rand(seed: u32) -> u32 {
    1664525u32.wrapping_mul(seed).wrapping_add(1013904223)
}

/// C: `FRAC_MUL16` (`celt/mathops.h`): a 16x16->16 bit fractional multiply, bit-exact
/// regardless of the float/fixed-point build (used by the *bit-exact* `bitexact_cos`/
/// `bitexact_log2tan`, which must behave identically in both builds since their output affects
/// the bitstream).
fn frac_mul16(a: i32, b: i32) -> i32 {
    let a16 = a as i16 as i32;
    let b16 = b as i16 as i32;
    (16384 + a16 * b16) >> 15
}

/// C: `bitexact_cos`. A cosine approximation designed to be bit-exact on any platform (bit
/// exactness matters here because it affects the bit allocation).
pub fn bitexact_cos(x: i16) -> i16 {
    let xi = x as i32;
    let tmp: i32 = (4096 + xi * xi) >> 13;
    let x2 = tmp as i16 as i32;
    let inner = frac_mul16(-626, x2);
    let inner = frac_mul16(x2, 8277 + inner);
    let inner = frac_mul16(x2, -7651 + inner);
    let sum = (32767 - x2) + inner;
    let x2_new = sum as i16;
    (1 + x2_new as i32) as i16
}

/// C: `bitexact_log2tan`.
pub fn bitexact_log2tan(isin: i32, icos: i32) -> i32 {
    let lc = ec_ilog(icos as u32);
    let ls = ec_ilog(isin as u32);
    let icos = icos << (15 - lc);
    let isin = isin << (15 - ls);
    (ls - lc) * (1 << 11) + frac_mul16(isin, frac_mul16(isin, -2597) + 7932)
        - frac_mul16(icos, frac_mul16(icos, -2597) + 7932)
}

/// C: `haar1`. In-place Haar (Hadamard-like) butterfly transform, used for time/frequency
/// resolution changes.
pub fn haar1(x: &mut [f32], n0: i32, stride: i32) {
    const C: f32 = 0.70710678;
    let n0 = n0 >> 1;
    for i in 0..stride {
        for j in 0..n0 {
            let idx1 = (stride * 2 * j + i) as usize;
            let idx2 = (stride * (2 * j + 1) + i) as usize;
            let tmp1 = C * x[idx1];
            let tmp2 = C * x[idx2];
            x[idx1] = tmp1 + tmp2;
            x[idx2] = tmp1 - tmp2;
        }
    }
}

/// C: `ordery_table` (`celt/bands.c`): bit-reversed-Gray-with-DC-at-the-end index tables for
/// `stride` in `{2,4,8,16}`, concatenated (indexed from `[stride-2]`).
const ORDERY_TABLE: [i32; 30] =
    [1, 0, 3, 0, 2, 1, 7, 0, 4, 3, 6, 1, 5, 2, 15, 0, 8, 7, 12, 3, 11, 4, 14, 1, 9, 6, 13, 2, 10, 5];

/// C: `deinterleave_hadamard`.
pub fn deinterleave_hadamard(x: &mut [f32], n0: i32, stride: i32, hadamard: bool) {
    let n = (n0 * stride) as usize;
    let mut tmp = vec![0.0f32; n];
    if hadamard {
        let ordery = &ORDERY_TABLE[(stride - 2) as usize..];
        for i in 0..stride {
            for j in 0..n0 {
                tmp[(ordery[i as usize] * n0 + j) as usize] = x[(j * stride + i) as usize];
            }
        }
    }
    else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[(i * n0 + j) as usize] = x[(j * stride + i) as usize];
            }
        }
    }
    x[..n].copy_from_slice(&tmp);
}

/// C: `interleave_hadamard`.
pub fn interleave_hadamard(x: &mut [f32], n0: i32, stride: i32, hadamard: bool) {
    let n = (n0 * stride) as usize;
    let mut tmp = vec![0.0f32; n];
    if hadamard {
        let ordery = &ORDERY_TABLE[(stride - 2) as usize..];
        for i in 0..stride {
            for j in 0..n0 {
                tmp[(j * stride + i) as usize] = x[(ordery[i as usize] * n0 + j) as usize];
            }
        }
    }
    else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[(j * stride + i) as usize] = x[(i * n0 + j) as usize];
            }
        }
    }
    x[..n].copy_from_slice(&tmp);
}

/// C: `compute_qn`.
fn compute_qn(n: i32, b: i32, offset: i32, pulse_cap: i32, stereo: bool) -> i32 {
    const EXP2_TABLE8: [i32; 8] = [16384, 17866, 19483, 21247, 23170, 25267, 27554, 30048];
    let mut n2 = 2 * n - 1;
    if stereo && n == 2 {
        n2 -= 1;
    }
    let mut qb = celt_sudiv(b + n2 * offset, n2);
    qb = qb.min(b - pulse_cap - (4 << BITRES));
    qb = qb.min(8 << BITRES);
    if qb < (1 << BITRES >> 1) {
        1
    }
    else {
        let qn = EXP2_TABLE8[(qb & 0x7) as usize] >> (14 - (qb >> BITRES));
        ((qn + 1) >> 1) << 1
    }
}

/// C: `struct band_ctx` (`celt/bands.c`), restricted to decode-only fields (see module docs).
struct BandCtx<'m, 'rd, 'buf> {
    m: &'m CeltMode,
    i: i32,
    intensity: i32,
    spread: i32,
    tf_change: i32,
    rd: &'rd mut RangeDecoder<'buf>,
    remaining_bits: i32,
    seed: u32,
    disable_inv: bool,
}

/// C: `struct split_ctx` (`celt/bands.c`).
struct SplitCtx {
    inv: bool,
    imid: i32,
    iside: i32,
    delta: i32,
    itheta: i32,
    qalloc: i32,
}

/// C: `compute_theta` (decode direction only: `X`/`Y` are unused by the decoder — every read of
/// them in libopus is inside an `if (encode)` block — so they are dropped from the signature).
#[allow(clippy::too_many_arguments)]
fn compute_theta(ctx: &mut BandCtx, n: i32, b: &mut i32, bb: i32, b0: i32, lm: i32, stereo: bool, fill: &mut i32) -> SplitCtx {
    let i = ctx.i;
    let intensity = ctx.intensity;

    let pulse_cap = ctx.m.log_n[i as usize] as i32 + lm * (1 << BITRES);
    let offset = (pulse_cap >> 1) - if stereo && n == 2 { QTHETA_OFFSET_TWOPHASE } else { QTHETA_OFFSET };
    let mut qn = compute_qn(n, *b, offset, pulse_cap, stereo);
    if stereo && i >= intensity {
        qn = 1;
    }

    let tell = ctx.rd.tell_frac() as i32;
    let mut itheta = 0i32;
    let mut inv = false;

    if qn != 1 {
        if stereo && n > 2 {
            let p0 = 3i32;
            let x0 = qn / 2;
            let ft = p0 * (x0 + 1) + x0;
            let fs = ctx.rd.decode(ft as u32) as i32;
            let x = if fs < (x0 + 1) * p0 { fs / p0 } else { x0 + 1 + (fs - (x0 + 1) * p0) };
            let (fl, fh) = if x <= x0 {
                (p0 * x, p0 * (x + 1))
            }
            else {
                ((x - 1 - x0) + (x0 + 1) * p0, (x - x0) + (x0 + 1) * p0)
            };
            ctx.rd.update(fl as u32, fh as u32, ft as u32);
            itheta = x;
        }
        else if b0 > 1 || stereo {
            itheta = ctx.rd.dec_uint((qn + 1) as u32) as i32;
        }
        else {
            let ft = ((qn >> 1) + 1) * ((qn >> 1) + 1);
            let fm = ctx.rd.decode(ft as u32) as i32;
            let (fl, fs);
            if fm < ((qn >> 1) * ((qn >> 1) + 1) >> 1) {
                itheta = (isqrt32((8 * fm + 1) as u32) as i32 - 1) >> 1;
                fs = itheta + 1;
                fl = itheta * (itheta + 1) >> 1;
            }
            else {
                itheta = (2 * (qn + 1) - isqrt32((8 * (ft - fm - 1) + 1) as u32) as i32) >> 1;
                fs = qn + 1 - itheta;
                fl = ft - ((qn + 1 - itheta) * (qn + 2 - itheta) >> 1);
            }
            ctx.rd.update(fl as u32, (fl + fs) as u32, ft as u32);
        }
        itheta = (itheta * 16384) / qn;
    }
    else if stereo {
        if *b > 2 << BITRES && ctx.remaining_bits > 2 << BITRES {
            inv = ctx.rd.dec_bit_logp(2);
        }
        if ctx.disable_inv {
            inv = false;
        }
        itheta = 0;
    }

    let qalloc = ctx.rd.tell_frac() as i32 - tell;
    *b -= qalloc;

    let (imid, iside, delta);
    if itheta == 0 {
        imid = 32767;
        iside = 0;
        *fill &= (1 << bb) - 1;
        delta = -16384;
    }
    else if itheta == 16384 {
        imid = 0;
        iside = 32767;
        *fill &= ((1 << bb) - 1) << bb;
        delta = 16384;
    }
    else {
        imid = bitexact_cos(itheta as i16) as i32;
        iside = bitexact_cos((16384 - itheta) as i16) as i32;
        delta = frac_mul16((n - 1) << 7, bitexact_log2tan(iside, imid));
    }

    SplitCtx { inv, imid, iside, delta, itheta, qalloc }
}

/// C: `quant_band_n1` (decode direction).
fn quant_band_n1(ctx: &mut BandCtx, x: &mut [f32], y: Option<&mut [f32]>, lowband_out: Option<&mut [f32]>) -> u32 {
    let mut sign0 = false;
    if ctx.remaining_bits >= 1 << BITRES {
        sign0 = ctx.rd.dec_bits(1) != 0;
        ctx.remaining_bits -= 1 << BITRES;
    }
    x[0] = if sign0 { -NORM_SCALING } else { NORM_SCALING };
    if let Some(yy) = y {
        let mut sign1 = false;
        if ctx.remaining_bits >= 1 << BITRES {
            sign1 = ctx.rd.dec_bits(1) != 0;
            ctx.remaining_bits -= 1 << BITRES;
        }
        yy[0] = if sign1 { -NORM_SCALING } else { NORM_SCALING };
    }
    if let Some(lo) = lowband_out {
        lo[0] = x[0];
    }
    1
}

/// C: `quant_partition` (decode direction).
#[allow(clippy::too_many_arguments)]
fn quant_partition(ctx: &mut BandCtx, x: &mut [f32], n: i32, b: i32, bb: i32, lowband: Option<&[f32]>, lm: i32, gain: f32, fill: i32) -> u32 {
    let mut b = b;
    let mut fill = fill;
    let i = ctx.i;
    let spread = ctx.spread;

    let cache_base = ctx.m.cache.index[((lm + 1) * ctx.m.nb_ebands + i) as usize] as usize;
    let cache = &ctx.m.cache.bits[cache_base..];

    if lm != -1 && b > cache[cache[0] as usize] as i32 + 12 && n > 2 {
        let n_half = n >> 1;
        let (x_lo, x_hi) = x.split_at_mut(n_half as usize);
        let lm2 = lm - 1;
        if bb == 1 {
            fill = (fill & 1) | (fill << 1);
        }
        let bb2 = (bb + 1) >> 1;

        let sctx = compute_theta(ctx, n_half, &mut b, bb2, bb, lm2, false, &mut fill);
        let imid = sctx.imid;
        let iside = sctx.iside;
        let mut delta = sctx.delta;
        let itheta = sctx.itheta;
        let qalloc = sctx.qalloc;
        let mid = (1.0 / 32768.0) * imid as f32;
        let side = (1.0 / 32768.0) * iside as f32;

        if bb > 1 && (itheta & 0x3fff) != 0 {
            if itheta > 8192 {
                delta -= delta >> (4 - lm2);
            }
            else {
                delta = 0.min(delta + ((n_half << BITRES) >> (5 - lm2)));
            }
        }
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= qalloc;

        let next_lowband2: Option<&[f32]> = lowband.map(|lb| &lb[n_half as usize..]);

        let rebalance = ctx.remaining_bits;
        let cm;
        if mbits >= sbits {
            let cm_lo = quant_partition(ctx, x_lo, n_half, mbits, bb2, lowband, lm2, gain * mid, fill);
            let rebalance2 = mbits - (rebalance - ctx.remaining_bits);
            if rebalance2 > 3 << BITRES && itheta != 0 {
                sbits += rebalance2 - (3 << BITRES);
            }
            let cm_hi = quant_partition(ctx, x_hi, n_half, sbits, bb2, next_lowband2, lm2, gain * side, fill >> bb2);
            cm = cm_lo | (cm_hi << (bb >> 1));
        }
        else {
            let cm_hi = quant_partition(ctx, x_hi, n_half, sbits, bb2, next_lowband2, lm2, gain * side, fill >> bb2);
            let rebalance2 = sbits - (rebalance - ctx.remaining_bits);
            if rebalance2 > 3 << BITRES && itheta != 16384 {
                mbits += rebalance2 - (3 << BITRES);
            }
            let cm_lo = quant_partition(ctx, x_lo, n_half, mbits, bb2, lowband, lm2, gain * mid, fill);
            cm = cm_lo | (cm_hi << (bb >> 1));
        }
        cm
    }
    else {
        // The basic no-split case.
        let q = bits2pulses(ctx.m, i, lm, b);
        let mut curr_bits = pulses2bits(ctx.m, i, lm, q);
        ctx.remaining_bits -= curr_bits;

        let mut q = q;
        while ctx.remaining_bits < 0 && q > 0 {
            ctx.remaining_bits += curr_bits;
            q -= 1;
            curr_bits = pulses2bits(ctx.m, i, lm, q);
            ctx.remaining_bits -= curr_bits;
        }

        if q != 0 {
            let k = get_pulses(q);
            alg_unquant(x, n, k, spread, bb, gain, ctx.rd)
        }
        else {
            let cm_mask = (1u32 << bb) - 1;
            fill &= cm_mask as i32;
            if fill == 0 {
                x[..n as usize].fill(0.0);
                0
            }
            else {
                let cm;
                if let Some(lb) = lowband {
                    for j in 0..n as usize {
                        ctx.seed = celt_lcg_rand(ctx.seed);
                        let tmp: f32 = 1.0 / 256.0;
                        let tmp = if (ctx.seed & 0x8000) != 0 { tmp } else { -tmp };
                        x[j] = lb[j] + tmp;
                    }
                    cm = fill as u32;
                }
                else {
                    for j in 0..n as usize {
                        ctx.seed = celt_lcg_rand(ctx.seed);
                        x[j] = (ctx.seed as i32 >> 20) as f32;
                    }
                    cm = cm_mask;
                }
                renormalise_vector(x, n, gain);
                cm
            }
        }
    }
}


/// C: `quant_band` (decode direction).
#[allow(clippy::too_many_arguments)]
fn quant_band(
    ctx: &mut BandCtx,
    x: &mut [f32],
    n: i32,
    b: i32,
    b_param: i32,
    lowband: Option<&[f32]>,
    lm: i32,
    lowband_out: Option<&mut [f32]>,
    gain: f32,
    lowband_scratch: &mut [f32],
    fill: i32,
) -> u32 {
    let n0 = n;
    let mut n_b = n;
    let mut bb = b_param;
    let b0_initial = bb;
    let mut time_divide = 0i32;
    let mut recombine = 0i32;
    let long_blocks = b0_initial == 1;
    let mut fill = fill;

    let tf_change = ctx.tf_change;

    n_b /= bb;

    if n == 1 {
        return quant_band_n1(ctx, x, None, lowband_out);
    }

    if tf_change > 0 {
        recombine = tf_change;
    }

    // Always copy the fold source into the scratch buffer up front (see module docs: this is
    // behaviourally identical to libopus's conditional-aliasing optimisation).
    let mut lowband_buf: Option<&mut [f32]> = lowband.map(|lb| {
        lowband_scratch[..n as usize].copy_from_slice(&lb[..n as usize]);
        &mut lowband_scratch[..n as usize]
    });

    const BIT_INTERLEAVE_TABLE: [i32; 16] = [0, 1, 1, 1, 2, 3, 3, 3, 2, 3, 3, 3, 2, 3, 3, 3];
    for k in 0..recombine {
        haar1(x, n >> k, 1 << k);
        if let Some(lb) = lowband_buf.as_deref_mut() {
            haar1(lb, n >> k, 1 << k);
        }
        fill = BIT_INTERLEAVE_TABLE[(fill & 0xF) as usize] | (BIT_INTERLEAVE_TABLE[((fill >> 4) & 0xF) as usize] << 2);
    }
    bb >>= recombine;
    n_b <<= recombine;

    let mut tf_change_mut = tf_change;
    while (n_b & 1) == 0 && tf_change_mut < 0 {
        haar1(x, n_b, bb);
        if let Some(lb) = lowband_buf.as_deref_mut() {
            haar1(lb, n_b, bb);
        }
        fill |= fill << bb;
        bb <<= 1;
        n_b >>= 1;
        time_divide += 1;
        tf_change_mut += 1;
    }
    let b0_final = bb;
    let n_b0 = n_b;

    if b0_final > 1 {
        deinterleave_hadamard(x, n_b >> recombine, b0_final << recombine, long_blocks);
        if let Some(lb) = lowband_buf.as_deref_mut() {
            deinterleave_hadamard(lb, n_b >> recombine, b0_final << recombine, long_blocks);
        }
    }

    let mut cm = quant_partition(ctx, x, n, b, bb, lowband_buf.as_deref(), lm, gain, fill);

    // resynth (always true in decode).
    {
        if b0_final > 1 {
            interleave_hadamard(x, n_b >> recombine, b0_final << recombine, long_blocks);
        }
        let mut b_var = b0_final;
        let mut n_b_var = n_b0;
        for _ in 0..time_divide {
            b_var >>= 1;
            n_b_var <<= 1;
            cm |= cm >> b_var;
            haar1(x, n_b_var, b_var);
        }
        const BIT_DEINTERLEAVE_TABLE: [u32; 16] =
            [0x00, 0x03, 0x0C, 0x0F, 0x30, 0x33, 0x3C, 0x3F, 0xC0, 0xC3, 0xCC, 0xCF, 0xF0, 0xF3, 0xFC, 0xFF];
        for k in 0..recombine {
            cm = BIT_DEINTERLEAVE_TABLE[cm as usize];
            haar1(x, n0 >> k, 1 << k);
        }
        b_var <<= recombine;

        if let Some(lo) = lowband_out {
            let nrm = celt_sqrt(n0 as f32);
            for j in 0..n0 as usize {
                lo[j] = nrm * x[j];
            }
        }
        cm &= (1u32 << b_var) - 1;
    }
    cm
}

/// C: `quant_band_stereo` (decode direction).
#[allow(clippy::too_many_arguments)]
fn quant_band_stereo(
    ctx: &mut BandCtx,
    x: &mut [f32],
    y: &mut [f32],
    n: i32,
    b: i32,
    bb: i32,
    lowband: Option<&[f32]>,
    lm: i32,
    lowband_out: Option<&mut [f32]>,
    lowband_scratch: &mut [f32],
    fill: i32,
) -> u32 {
    if n == 1 {
        return quant_band_n1(ctx, x, Some(y), lowband_out);
    }

    let orig_fill = fill;
    let mut fill = fill;
    let mut b = b;
    let sctx = compute_theta(ctx, n, &mut b, bb, bb, lm, true, &mut fill);
    let inv = sctx.inv;
    let imid = sctx.imid;
    let iside = sctx.iside;
    let delta = sctx.delta;
    let itheta = sctx.itheta;
    let qalloc = sctx.qalloc;
    let mid = (1.0 / 32768.0) * imid as f32;
    let side = (1.0 / 32768.0) * iside as f32;

    let cm;
    if n == 2 {
        let mut mbits = b;
        let mut sbits = 0;
        if itheta != 0 && itheta != 16384 {
            sbits = 1 << BITRES;
        }
        mbits -= sbits;
        let c_sel = itheta > 8192;
        ctx.remaining_bits -= qalloc + sbits;

        let mut sign = false;
        if sbits != 0 {
            sign = ctx.rd.dec_bits(1) != 0;
        }
        let sign_mul: f32 = if sign { -1.0 } else { 1.0 };

        if c_sel {
            cm = quant_band(ctx, y, 2, mbits, bb, lowband, lm, lowband_out, NORM_SCALING, lowband_scratch, orig_fill);
            x[0] = -sign_mul * y[1];
            x[1] = sign_mul * y[0];
        }
        else {
            cm = quant_band(ctx, x, 2, mbits, bb, lowband, lm, lowband_out, NORM_SCALING, lowband_scratch, orig_fill);
            y[0] = -sign_mul * x[1];
            y[1] = sign_mul * x[0];
        }

        // resynth (always true in decode).
        x[0] = mid * x[0];
        x[1] = mid * x[1];
        y[0] = side * y[0];
        y[1] = side * y[1];
        let tmp0 = x[0];
        x[0] = tmp0 - y[0];
        y[0] = tmp0 + y[0];
        let tmp1 = x[1];
        x[1] = tmp1 - y[1];
        y[1] = tmp1 + y[1];
    }
    else {
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= qalloc;

        let rebalance = ctx.remaining_bits;
        if mbits >= sbits {
            let cm_x = quant_band(ctx, x, n, mbits, bb, lowband, lm, lowband_out, NORM_SCALING, lowband_scratch, fill);
            let rebalance2 = mbits - (rebalance - ctx.remaining_bits);
            if rebalance2 > 3 << BITRES && itheta != 0 {
                sbits += rebalance2 - (3 << BITRES);
            }
            let mut empty_scratch: [f32; 0] = [];
            let cm_y = quant_band(ctx, y, n, sbits, bb, None, lm, None, side, &mut empty_scratch, fill >> bb);
            cm = cm_x | cm_y;
        }
        else {
            let mut empty_scratch: [f32; 0] = [];
            let cm_y = quant_band(ctx, y, n, sbits, bb, None, lm, None, side, &mut empty_scratch, fill >> bb);
            let rebalance2 = sbits - (rebalance - ctx.remaining_bits);
            if rebalance2 > 3 << BITRES && itheta != 16384 {
                mbits += rebalance2 - (3 << BITRES);
            }
            let cm_x = quant_band(ctx, x, n, mbits, bb, lowband, lm, lowband_out, NORM_SCALING, lowband_scratch, fill);
            cm = cm_x | cm_y;
        }
    }

    // resynth (always true in decode).
    if n != 2 {
        stereo_merge(x, y, mid, n);
    }
    if inv {
        for j in 0..n as usize {
            y[j] = -y[j];
        }
    }
    cm
}

/// C: `intensity_stereo`. Ported for completeness (see module docs): both libopus call sites
/// are inside `if (encode)`, so this is unreachable from the decode-only paths above.
#[allow(dead_code)]
fn intensity_stereo(m: &CeltMode, x: &mut [f32], y: &[f32], band_e: &[f32], band_id: i32, n: i32) {
    let i = band_id as usize;
    let shift = (celt_zlog2(band_e[i].max(band_e[i + m.nb_ebands as usize])) - 13).max(0);
    let scale = (1u32 << shift) as f32;
    let left = band_e[i] / scale;
    let right = band_e[i + m.nb_ebands as usize] / scale;
    let norm = crate::celt::modes::EPSILON + celt_sqrt(crate::celt::modes::EPSILON + left * left + right * right);
    let a1 = left / norm;
    let a2 = right / norm;
    for j in 0..n as usize {
        let l = x[j];
        let r = y[j];
        x[j] = a1 * l + a2 * r;
    }
}

fn celt_zlog2(x: f32) -> i32 {
    if x <= 0.0 {
        0
    }
    else {
        ec_ilog(x as u32).max(1) - 1
    }
}

/// C: `stereo_split`. Ported for completeness (see module docs): unreachable from decode.
#[allow(dead_code)]
fn stereo_split(x: &mut [f32], y: &mut [f32], n: i32) {
    const C: f32 = 0.70710678;
    for j in 0..n as usize {
        let l = C * x[j];
        let r = C * y[j];
        x[j] = l + r;
        y[j] = r - l;
    }
}

/// C: `stereo_merge`.
fn stereo_merge(x: &mut [f32], y: &mut [f32], mid: f32, n: i32) {
    let (xp, side) = {
        let mut xp = 0.0f32;
        let mut side = 0.0f32;
        for j in 0..n as usize {
            xp += y[j] * x[j];
            side += y[j] * y[j];
        }
        (xp, side)
    };
    let xp = mid * xp;
    let mid2 = mid * 0.5;
    let el = mid2 * mid2 + side - 2.0 * xp;
    let er = mid2 * mid2 + side + 2.0 * xp;
    if er < 6e-4 || el < 6e-4 {
        y[..n as usize].copy_from_slice(&x[..n as usize]);
        return;
    }
    let lgain = celt_rsqrt(el);
    let rgain = celt_rsqrt(er);
    for j in 0..n as usize {
        let l = mid * x[j];
        let r = y[j];
        x[j] = lgain * (l - r);
        y[j] = rgain * (l + r);
    }
}

/// C: `special_hybrid_folding`.
fn special_hybrid_folding(m: &CeltMode, norm: &mut [f32], norm2: Option<&mut [f32]>, start: i32, mshift: i32, dual_stereo: bool) {
    let e_bands = m.e_bands;
    let n1 = (mshift * (e_bands[(start + 1) as usize] as i32 - e_bands[start as usize] as i32)) as usize;
    let n2 = (mshift * (e_bands[(start + 2) as usize] as i32 - e_bands[(start + 1) as usize] as i32)) as usize;
    let src = 2 * n1 - n2;
    norm.copy_within(src..src + (n2 - n1), n1);
    if dual_stereo {
        if let Some(n2buf) = norm2 {
            n2buf.copy_within(src..src + (n2 - n1), n1);
        }
    }
}

/// C: `quant_all_bands` (decode direction only). See module docs for the decode-only
/// simplifications relative to libopus.
#[allow(clippy::too_many_arguments)]
pub fn quant_all_bands(
    mode: &CeltMode,
    start: i32,
    end: i32,
    x: &mut [f32],
    y: Option<&mut [f32]>,
    collapse_masks: &mut [u8],
    pulses: &[i32],
    short_blocks: bool,
    spread: i32,
    dual_stereo: bool,
    intensity: i32,
    tf_res: &[i32],
    total_bits: i32,
    balance: i32,
    rd: &mut RangeDecoder<'_>,
    lm: i32,
    coded_bands: i32,
    seed: &mut u32,
    disable_inv: bool,
) {
    let e_bands = mode.e_bands;
    let nb_ebands = mode.nb_ebands;
    let mshift = 1i32 << lm;
    let b_blocks = if short_blocks { mshift } else { 1 };
    let norm_offset = mshift * e_bands[start as usize] as i32;
    let c_chan: i32 = if y.is_some() { 2 } else { 1 };
    let total_norm_len = (mshift * e_bands[(nb_ebands - 1) as usize] as i32 - norm_offset).max(0);

    let mut norm = vec![0f32; total_norm_len as usize];
    let mut norm2 = if c_chan == 2 { vec![0f32; total_norm_len as usize] } else { Vec::new() };
    let mut lowband_scratch = vec![0f32; (mode.short_mdct_size << mode.max_lm) as usize];

    let mut lowband_offset = 0i32;
    let mut update_lowband = true;
    let mut dual_stereo = dual_stereo;
    let intensity = intensity;
    let mut balance = balance;

    debug_assert!(end <= mode.effective_ebands);

    let mut ctx = BandCtx { m: mode, i: start, intensity, spread, tf_change: 0, rd, remaining_bits: 0, seed: *seed, disable_inv };

    let mut y_opt = y;

    for i in start..end {
        let last = i == end - 1;
        ctx.i = i;
        ctx.intensity = intensity;

        let band_lo = mshift * e_bands[i as usize] as i32;
        let band_hi = mshift * e_bands[(i + 1) as usize] as i32;
        let n = band_hi - band_lo;
        debug_assert!(n > 0);

        let tell = ctx.rd.tell_frac() as i32;
        if i != start {
            balance -= tell;
        }
        let remaining_bits = total_bits - tell - 1;
        ctx.remaining_bits = remaining_bits;
        let b = if i <= coded_bands - 1 {
            let curr_balance = celt_sudiv(balance, 3.min(coded_bands - i));
            0.max((remaining_bits + 1).min(pulses[i as usize] + curr_balance)).min(16383)
        }
        else {
            0
        };

        if (band_lo - n >= mshift * e_bands[start as usize] as i32 || i == start + 1) && (update_lowband || lowband_offset == 0) {
            lowband_offset = i;
        }
        if i == start + 1 {
            let norm2_opt = if c_chan == 2 { Some(&mut norm2[..]) } else { None };
            special_hybrid_folding(mode, &mut norm, norm2_opt, start, mshift, dual_stereo);
        }

        let tf_change = tf_res[i as usize];
        ctx.tf_change = tf_change;

        let mut effective_lowband: i32 = -1;
        let x_cm0: u32;
        let y_cm0: u32;
        if lowband_offset != 0 && (spread != SPREAD_AGGRESSIVE || b_blocks > 1 || tf_change < 0) {
            effective_lowband = 0.max(mshift * e_bands[lowband_offset as usize] as i32 - norm_offset - n);
            let mut fold_start = lowband_offset;
            loop {
                fold_start -= 1;
                if !(mshift * e_bands[fold_start as usize] as i32 > effective_lowband + norm_offset) {
                    break;
                }
            }
            let mut fold_end = lowband_offset - 1;
            loop {
                fold_end += 1;
                if !(fold_end < i && (mshift * e_bands[fold_end as usize] as i32) < effective_lowband + norm_offset + n) {
                    break;
                }
            }
            let mut xc = 0u32;
            let mut yc = 0u32;
            let mut fold_i = fold_start;
            loop {
                xc |= collapse_masks[(fold_i * c_chan) as usize] as u32;
                yc |= collapse_masks[(fold_i * c_chan + c_chan - 1) as usize] as u32;
                fold_i += 1;
                if fold_i >= fold_end {
                    break;
                }
            }
            x_cm0 = xc;
            y_cm0 = yc;
        }
        else {
            x_cm0 = (1u32 << b_blocks) - 1;
            y_cm0 = x_cm0;
        }

        if dual_stereo && i == intensity {
            dual_stereo = false;
            for j in 0..(band_lo - norm_offset) as usize {
                norm[j] = 0.5 * (norm[j] + norm2[j]);
            }
        }

        let split_point = (band_lo - norm_offset) as usize;
        let (x_cm, y_cm);
        if dual_stereo {
            let y_buf = y_opt.as_deref_mut().expect("dual_stereo implies a second channel");
            let x_band = &mut x[band_lo as usize..band_hi as usize];
            let y_band = &mut y_buf[band_lo as usize..band_hi as usize];

            let (norm_lo, norm_hi) = norm.split_at_mut(split_point);
            let lowband_src = if effective_lowband != -1 {
                Some(&norm_lo[effective_lowband as usize..(effective_lowband + n) as usize])
            }
            else {
                None
            };
            let lowband_dst = if !last { Some(&mut norm_hi[..n as usize]) } else { None };
            let xc = quant_band(&mut ctx, x_band, n, b / 2, b_blocks, lowband_src, lm, lowband_dst, NORM_SCALING, &mut lowband_scratch, x_cm0 as i32);

            let (norm2_lo, norm2_hi) = norm2.split_at_mut(split_point);
            let lowband_src2 = if effective_lowband != -1 {
                Some(&norm2_lo[effective_lowband as usize..(effective_lowband + n) as usize])
            }
            else {
                None
            };
            let lowband_dst2 = if !last { Some(&mut norm2_hi[..n as usize]) } else { None };
            let yc = quant_band(&mut ctx, y_band, n, b / 2, b_blocks, lowband_src2, lm, lowband_dst2, NORM_SCALING, &mut lowband_scratch, y_cm0 as i32);

            x_cm = xc;
            y_cm = yc;
        }
        else {
            let x_band = &mut x[band_lo as usize..band_hi as usize];
            let (norm_lo, norm_hi) = norm.split_at_mut(split_point);
            let lowband_src = if effective_lowband != -1 {
                Some(&norm_lo[effective_lowband as usize..(effective_lowband + n) as usize])
            }
            else {
                None
            };
            let lowband_dst = if !last { Some(&mut norm_hi[..n as usize]) } else { None };

            if let Some(y_buf) = y_opt.as_deref_mut() {
                let y_band = &mut y_buf[band_lo as usize..band_hi as usize];
                let cm = quant_band_stereo(
                    &mut ctx,
                    x_band,
                    y_band,
                    n,
                    b,
                    b_blocks,
                    lowband_src,
                    lm,
                    lowband_dst,
                    &mut lowband_scratch,
                    (x_cm0 | y_cm0) as i32,
                );
                x_cm = cm;
            }
            else {
                x_cm = quant_band(&mut ctx, x_band, n, b, b_blocks, lowband_src, lm, lowband_dst, NORM_SCALING, &mut lowband_scratch, (x_cm0 | y_cm0) as i32);
            }
            y_cm = x_cm;
        }

        collapse_masks[(i * c_chan) as usize] = x_cm as u8;
        collapse_masks[(i * c_chan + c_chan - 1) as usize] = y_cm as u8;
        balance += pulses[i as usize] + tell;

        update_lowband = b > (n << BITRES);
    }

    *seed = ctx.seed;
}

/// C: `anti_collapse`.
#[allow(clippy::too_many_arguments)]
pub fn anti_collapse(
    mode: &CeltMode,
    x: &mut [f32],
    collapse_masks: &[u8],
    lm: i32,
    channels: i32,
    size: i32,
    start: i32,
    end: i32,
    old_band_e: &[f32],
    old_log_e: &[f32],
    old_log_e2: &[f32],
    pulses: &[i32],
    seed: u32,
) {
    let e_bands = mode.e_bands;
    let nb_ebands = mode.nb_ebands;
    let mut seed = seed;

    for i in start..end {
        let n0 = e_bands[(i + 1) as usize] as i32 - e_bands[i as usize] as i32;
        debug_assert!(pulses[i as usize] >= 0);
        let depth = celt_sudiv(1 + pulses[i as usize], e_bands[(i + 1) as usize] as i32 - e_bands[i as usize] as i32) >> lm;

        let thresh = 0.5 * celt_exp2(-0.125 * depth as f32);
        let sqrt_1 = celt_rsqrt((n0 << lm) as f32);

        for c in 0..channels {
            let idx = (c * nb_ebands + i) as usize;
            let mut prev1 = old_log_e[idx];
            let mut prev2 = old_log_e2[idx];
            if channels == 1 {
                prev1 = prev1.max(old_log_e[(nb_ebands + i) as usize]);
                prev2 = prev2.max(old_log_e2[(nb_ebands + i) as usize]);
            }
            let ediff = (old_band_e[idx] - prev1.min(prev2)).max(0.0);
            let mut r = 2.0 * celt_exp2(-ediff);
            if lm == 3 {
                r *= 1.41421356;
            }
            r = thresh.min(r);
            r *= sqrt_1;

            let base = (c * size + ((e_bands[i as usize] as i32) << lm)) as usize;
            let band_len = (n0 << lm) as usize;
            let band = &mut x[base..base + band_len];

            let mut renormalize = false;
            for k in 0..(1i32 << lm) {
                if collapse_masks[(i * channels + c) as usize] & (1 << k) == 0 {
                    for j in 0..n0 as usize {
                        seed = celt_lcg_rand(seed);
                        band[(j << lm) + k as usize] = if seed & 0x8000 != 0 { r } else { -r };
                    }
                    renormalize = true;
                }
            }
            if renormalize {
                renormalise_vector(band, n0 << lm, NORM_SCALING);
            }
        }
    }
}

/// C: `denormalise_bands`.
#[allow(clippy::too_many_arguments)]
pub fn denormalise_bands(mode: &CeltMode, x: &[f32], freq: &mut [f32], band_log_e: &[f32], start: i32, end: i32, m: i32, downsample: i32, silence: bool) {
    let e_bands = mode.e_bands;
    let n = m * mode.short_mdct_size;
    let mut bound = m * e_bands[end as usize] as i32;
    if downsample != 1 {
        bound = bound.min(n / downsample);
    }
    let (start, end, bound) = if silence { (0, 0, 0) } else { (start, end, bound) };

    let lead = (m * e_bands[start as usize] as i32) as usize;
    for f in freq[..lead].iter_mut() {
        *f = 0.0;
    }

    let mut fpos = lead;
    let mut xpos = lead;
    let mut i = start;
    while i < end {
        let j0 = m * e_bands[i as usize] as i32;
        let band_end = m * e_bands[(i + 1) as usize] as i32;
        let lg = band_log_e[i as usize] + E_MEANS[i as usize];
        let g = celt_exp2(lg.min(32.0));
        let mut j = j0;
        while j < band_end {
            freq[fpos] = x[xpos] * g;
            fpos += 1;
            xpos += 1;
            j += 1;
        }
        i += 1;
    }
    for f in freq[bound as usize..n as usize].iter_mut() {
        *f = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spot-checks [`bitexact_cos`] against an independent re-implementation of libopus's exact
    /// `opus_int16`-wraparound formula (`celt/bands.c`), computed out-of-band in Python
    /// (`FRAC_MUL16`'s 16-bit truncating multiply, including the final `1+x2` cast wrapping at
    /// `x=0`/`x=-32768`, which the reference relies on since `compute_theta` never calls
    /// `bitexact_cos` for `itheta` outside `(0, 16384)`).
    #[test]
    fn bitexact_cos_matches_c_formula() {
        let cases: [(i16, i16); 8] = [
            (0, -32768),
            (1, -32768),
            (100, 32767),
            (1000, 32618),
            (8192, 23171),
            (16384_i32 as i16, 16554),
            (32767, -32758),
            (-8192, 23171),
        ];
        for (x, expected) in cases {
            assert_eq!(bitexact_cos(x), expected, "bitexact_cos({x})");
        }
    }

    /// Spot-checks [`bitexact_log2tan`] (the mid/side split-bias term) for representative
    /// `itheta` values (excluding the `itheta` extremes bands.c special-cases before ever
    /// calling it), against the same independent Python re-implementation.
    #[test]
    fn bitexact_log2tan_matches_c_formula() {
        let cases: [(i32, i32, i32); 5] = [
            (315, 32767, -13726),
            (12540, 30274, -2611),
            (23171, 23171, 0),
            (29916, 13371, 2381),
            (32746, 1206, 9754),
        ];
        for (isin, icos, expected) in cases {
            assert_eq!(bitexact_log2tan(isin, icos), expected, "bitexact_log2tan({isin},{icos})");
        }
    }

    /// `haar1` (with the `1/sqrt(2)` normalisation) is its own exact inverse: applying it twice
    /// returns the original values (each independent butterfly pair squares its `1/sqrt(2)`
    /// scale factors back to `1`). This is the property `quant_band`'s resynthesis relies on
    /// when it undoes the recombine/time-divide transforms it applied going in.
    #[test]
    fn haar1_is_involutive() {
        let original = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let mut x = original;
        haar1(&mut x, 8, 1);
        haar1(&mut x, 8, 1);
        for (a, b) in x.iter().zip(original.iter()) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
    }

    /// `celt_lcg_rand` is a direct 32-bit-wrapping LCG transcription; check the well-known
    /// first two outputs (any off-by-one in the multiply/add constants would show up here).
    #[test]
    fn celt_lcg_rand_matches_c() {
        let s1 = celt_lcg_rand(0);
        assert_eq!(s1, 1013904223);
        let s2 = celt_lcg_rand(s1);
        assert_eq!(s2, 1013904223u32.wrapping_mul(1664525).wrapping_add(1013904223));
    }

    /// `denormalise_bands` should scale each (unit-norm) band of `x` by `exp2(bandLogE +
    /// eMeans)`, i.e. round-trip `amp2_log2`'s definition (`quant_bands.rs`) for a single band
    /// with a known energy.
    #[test]
    fn denormalise_bands_sanity() {
        let mode = &crate::celt::modes::MODE_48000_960;
        let m = 1; // LM=0
        let start = 0;
        let end = 1;
        let n = (mode.e_bands[1] - mode.e_bands[0]) as usize; // band 0 width at M=1
        let x = vec![1.0f32; n];
        // Amplitude 1.0 in band 0: amp2Log2 => log2(1.0) - eMeans[0] = -eMeans[0].
        let band_log_e = [-crate::celt::quant_bands::E_MEANS[0]];
        let mut freq = vec![0.0f32; (mode.short_mdct_size) as usize];
        denormalise_bands(mode, &x, &mut freq, &band_log_e, start, end, m, 1, false);
        for &f in &freq[..n] {
            assert!((f - 1.0).abs() < 1e-3, "denormalised band0 sample = {f}, want ~1.0");
        }
        for &f in &freq[n..] {
            assert_eq!(f, 0.0);
        }
    }

    /// `denormalise_bands` with `silence=true` must produce an all-zero `freq`, regardless of
    /// `x`/`band_log_e` (matches `celt_decoder.c`'s use for lost/silence frames).
    #[test]
    fn denormalise_bands_silence_is_all_zero() {
        let mode = &crate::celt::modes::MODE_48000_960;
        let n = mode.e_bands[1] as usize;
        let x = vec![5.0f32; n];
        let band_log_e = [10.0f32];
        let mut freq = vec![1.0f32; mode.short_mdct_size as usize];
        denormalise_bands(mode, &x, &mut freq, &band_log_e, 0, 1, 1, 1, true);
        assert!(freq.iter().all(|&f| f == 0.0));
    }

    /// `compute_qn` must always return an even `qn` in `[1, 256]` (the caller's split logic
    /// assumes both, e.g. `qn/2` for the "uniform pdf" branch and `celt_assert(qn<=256)` in
    /// libopus).
    #[test]
    fn compute_qn_is_bounded_and_even() {
        for n in [2, 3, 4, 8, 16, 44] {
            for b in [0, 8, 64, 512, 4096] {
                for stereo in [false, true] {
                    let qn = compute_qn(n, b, 0, 0, stereo);
                    assert!(qn >= 1 && qn <= 256, "compute_qn({n},{b})={qn}");
                    assert!(qn == 1 || qn % 2 == 0, "compute_qn({n},{b})={qn} not even");
                }
            }
        }
    }
}
