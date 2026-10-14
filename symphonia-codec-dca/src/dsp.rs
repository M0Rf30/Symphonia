// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Ported from FFmpeg libavcodec dcadsp.c, dcadct.c, dcamath.h, synth_filter.c (LGPL-2.1+), see
// NOTICE. The tables of the fixed-point IMDCT are generated from dcadct.c.

//! Synthesis filter banks of the DTS core: the 32-band (and 64-band, for X96) QMF bank and the
//! LFE interpolation filters, in floating-point and bit-exact fixed-point (as used by the lossless
//! XLL extension) variants.

use crate::decoder::{ADPCM_COEFFS, SUBBAND_STRIDE, clip23, norm};
use crate::tables::{
    FIR_32BANDS_NONPERFECT, FIR_32BANDS_NONPERFECT_FIXED, FIR_32BANDS_PERFECT,
    FIR_32BANDS_PERFECT_FIXED, FIR_64BANDS, FIR_64BANDS_FIXED, LFE_FIR_64, LFE_FIR_64_FIXED,
    LFE_FIR_128,
};

use crate::tables_dct::{DCT_A, DCT_B, MOD_A, MOD_B, MOD_C, MOD64_A, MOD64_B, MOD64_C};

// Fixed-point math helpers (dcamath.h). All of them wrap like the C code does on corrupt input.

#[inline]
#[allow(dead_code)]
pub(crate) fn norm13(a: i64) -> i32 {
    norm(a, 13)
}

#[inline]
#[allow(dead_code)]
pub(crate) fn norm16(a: i64) -> i32 {
    norm(a, 16)
}

#[inline]
pub(crate) fn mul15(a: i32, b: i32) -> i32 {
    norm(i64::from(a) * i64::from(b), 15)
}

#[inline]
pub(crate) fn mul16(a: i32, b: i32) -> i32 {
    norm(i64::from(a) * i64::from(b), 16)
}

#[inline]
#[allow(dead_code)]
pub(crate) fn mul17(a: i32, b: i32) -> i32 {
    norm(i64::from(a) * i64::from(b), 17)
}

#[inline]
#[allow(dead_code)]
pub(crate) fn mul22(a: i32, b: i32) -> i32 {
    norm(i64::from(a) * i64::from(b), 22)
}

#[inline]
pub(crate) fn mul23(a: i32, b: i32) -> i32 {
    norm(i64::from(a) * i64::from(b), 23)
}

/// Half inverse MDCT as used by the DTS QMF synthesis.
///
/// For `L` input coefficients `X[k]` it produces the `L` middle samples of the full `2L` sample
/// inverse MDCT: `y[m] = -sum_k X[k] cos(pi / L * (m + L + 1/2) * (k + 1/2))`, which is the
/// reversed DCT-IV of the input.
pub struct ImdctHalf {
    len: usize,
    /// `len * len` matrix, row `m` holds the coefficients of output `m`.
    matrix: Vec<f32>,
}

impl ImdctHalf {
    pub fn new(len: usize) -> Self {
        let mut matrix = vec![0f32; len * len];
        for m in 0..len {
            for k in 0..len {
                let arg = std::f64::consts::PI / len as f64
                    * (m as f64 + len as f64 + 0.5)
                    * (k as f64 + 0.5);
                matrix[m * len + k] = (-arg.cos()) as f32;
            }
        }
        ImdctHalf { len, matrix }
    }

    #[inline]
    pub fn process(&self, input: &[f32], out: &mut [f32]) {
        let n = self.len;
        for (o, row) in out[..n].iter_mut().zip(self.matrix.chunks_exact(n)) {
            let mut acc = [0f32; 8];
            for (r, x) in row.chunks_exact(8).zip(input[..n].chunks_exact(8)) {
                for i in 0..8 {
                    acc[i] += r[i] * x[i];
                }
            }
            *o = ((acc[0] + acc[4]) + (acc[1] + acc[5])) + ((acc[2] + acc[6]) + (acc[3] + acc[7]));
        }
    }
}

/// Per-channel state of the QMF synthesis filter bank.
pub struct SynthState {
    hist1: Vec<f32>,
    hist2: [f32; 64],
    offset: usize,
}

impl Default for SynthState {
    fn default() -> Self {
        SynthState { hist1: vec![0.0; 1024], hist2: [0.0; 64], offset: 0 }
    }
}

impl SynthState {
    pub fn reset(&mut self) {
        self.hist1.fill(0.0);
        self.hist2 = [0.0; 64];
        self.offset = 0;
    }

    /// Run one step of the synthesis filter: `bands` input subband samples produce `bands`
    /// output PCM samples.
    fn step(
        &mut self,
        imdct: &ImdctHalf,
        window: &[f32],
        out: &mut [f32],
        input: &[f32],
        scale: f32,
    ) {
        let bands = imdct.len;
        let half = bands / 2;
        let mask = bands * 16 - 1;
        let off = self.offset;

        imdct.process(input, &mut self.hist1[off..off + bands]);

        let h = &self.hist1;
        let step = bands * 2;
        for i in 0..half {
            let mut a = self.hist2[i];
            let mut b = self.hist2[i + half];
            let mut c = 0f32;
            let mut d = 0f32;

            let mut j = 0;
            while j < bands * 16 {
                a += window[i + j] * -h[(off + half - 1 - i + j) & mask];
                b += window[i + j + half] * h[(off + i + j) & mask];
                c += window[i + j + bands] * h[(off + half + i + j) & mask];
                d += window[i + j + bands + half] * h[(off + bands - 1 - i + j) & mask];
                j += step;
            }

            out[i] = a * scale;
            out[i + half] = b * scale;
            self.hist2[i] = c;
            self.hist2[i + half] = d;
        }

        self.offset = (off + bands * 16 - bands) & mask;
    }
}

/// The DTS synthesis filter banks and interpolation filters (immutable data).
pub struct Filters {
    imdct32: ImdctHalf,
    imdct64: ImdctHalf,
}

impl Default for Filters {
    fn default() -> Self {
        Filters { imdct32: ImdctHalf::new(32), imdct64: ImdctHalf::new(64) }
    }
}

/// A view of the subband samples of one channel: `get(band, n)` returns sample `n` of subband
/// `band`.
pub struct Subbands<'a> {
    data: &'a [i32],
    base: usize,
}

impl<'a> Subbands<'a> {
    /// `base` is the index of the first history sample of subband 0 in `data`.
    pub fn new(data: &'a [i32], base: usize) -> Self {
        Subbands { data, base }
    }

    #[inline]
    fn get(&self, band: usize, n: usize) -> i32 {
        self.data[self.base + band * SUBBAND_STRIDE + ADPCM_COEFFS + n]
    }
}

impl Filters {
    /// 32-band synthesis of `npcmblocks` blocks of subband samples into `npcmblocks * 32` PCM
    /// samples.
    pub fn sub_qmf32(
        &self,
        state: &mut SynthState,
        perfect: bool,
        pcm: &mut [f32],
        lo: &Subbands<'_>,
        npcmblocks: usize,
        scale: f32,
    ) {
        let window: &[f32] = if perfect { &FIR_32BANDS_PERFECT } else { &FIR_32BANDS_NONPERFECT };
        let mut input = [0f32; 32];

        for j in 0..npcmblocks {
            for (i, x) in input.iter_mut().enumerate() {
                let v = lo.get(i, j);
                *x = if (i as i32 - 1) & 2 != 0 { -v } else { v } as f32;
            }
            state.step(&self.imdct32, window, &mut pcm[j * 32..j * 32 + 32], &input, scale);
        }
    }

    /// 64-band synthesis (X96). `hi` supplies the X96 subband samples, if present for the channel.
    pub fn sub_qmf64(
        &self,
        state: &mut SynthState,
        pcm: &mut [f32],
        lo: &Subbands<'_>,
        hi: Option<&Subbands<'_>>,
        npcmblocks: usize,
        scale: f32,
    ) {
        let mut input = [0f32; 64];

        for j in 0..npcmblocks {
            if let Some(hi) = hi {
                for (i, x) in input.iter_mut().enumerate() {
                    let v = if i < 32 { lo.get(i, j) + hi.get(i, j) } else { hi.get(i, j) };
                    *x = if (i as i32 - 1) & 2 != 0 { -v } else { v } as f32;
                }
            }
            else {
                for (i, x) in input[..32].iter_mut().enumerate() {
                    let v = lo.get(i, j);
                    *x = if (i as i32 - 1) & 2 != 0 { -v } else { v } as f32;
                }
            }
            state.step(&self.imdct64, &FIR_64BANDS, &mut pcm[j * 64..j * 64 + 64], &input, scale);
        }
    }
}

/// Interpolate the decimated LFE channel.
///
/// `lfe[base + n]` is decimated sample `n` of the current frame and `lfe[base - 1 ..= base - 7]`
/// hold history. `dec_select` is 0 for 64x decimation and 1 for 128x.
pub fn lfe_fir(pcm: &mut [f32], lfe: &[i32], base: usize, npcmblocks: usize, dec_select: usize) {
    let factor = 64usize << dec_select;
    let ncoeffs = 8usize >> dec_select;
    let nlfesamples = npcmblocks >> (dec_select + 1);
    let coeff: &[f32; 256] = if dec_select == 1 { &LFE_FIR_128 } else { &LFE_FIR_64 };

    for i in 0..nlfesamples {
        let out = &mut pcm[i * factor..(i + 1) * factor];
        let pos = base + i;
        for j in 0..factor / 2 {
            let mut a = 0f32;
            let mut b = 0f32;
            for k in 0..ncoeffs {
                let s = lfe[pos - k] as f32;
                a += coeff[j * ncoeffs + k] * s;
                b += coeff[255 - j * ncoeffs - k] * s;
            }
            out[j] = a;
            out[factor / 2 + j] = b;
        }
    }
}

/// Filter the 96 kHz oversampled LFE PCM to attenuate the high frequency components of the
/// interpolation image.
pub fn lfe_x96(dst: &mut [f32], src: &[f32], hist: &mut f32) {
    let mut prev = *hist;
    for (d, &s) in dst.chunks_exact_mut(2).zip(src) {
        d[0] = 0.25 * s + 0.75 * prev;
        d[1] = 0.75 * s + 0.25 * prev;
        prev = s;
    }
    *hist = prev;
}

// -------------------------------------------------------------------------------------------
// Bit-exact fixed-point synthesis (dcadct.c, synth_filter.c, dcadsp.c)
// -------------------------------------------------------------------------------------------

fn sum_a(input: &[i32], output: &mut [i32], len: usize) {
    for i in 0..len {
        output[i] = input[2 * i].wrapping_add(input[2 * i + 1]);
    }
}

fn sum_b(input: &[i32], output: &mut [i32], len: usize) {
    output[0] = input[0];
    for i in 1..len {
        output[i] = input[2 * i].wrapping_add(input[2 * i - 1]);
    }
}

fn sum_c(input: &[i32], output: &mut [i32], len: usize) {
    for i in 0..len {
        output[i] = input[2 * i];
    }
}

fn sum_d(input: &[i32], output: &mut [i32], len: usize) {
    output[0] = input[1];
    for i in 1..len {
        output[i] = input[2 * i - 1].wrapping_add(input[2 * i + 1]);
    }
}

fn dct_a(input: &[i32], output: &mut [i32]) {
    for i in 0..8 {
        let mut res = 0i64;
        for j in 0..8 {
            res = res.wrapping_add(i64::from(DCT_A[i][j]) * i64::from(input[j]));
        }
        output[i] = norm(res, 23);
    }
}

fn dct_b(input: &[i32], output: &mut [i32]) {
    for i in 0..8 {
        let mut res = i64::from(input[0]) << 23;
        for j in 0..7 {
            res = res.wrapping_add(i64::from(DCT_B[i][j]) * i64::from(input[1 + j]));
        }
        output[i] = norm(res, 23);
    }
}

fn mod_a(input: &[i32], output: &mut [i32]) {
    for i in 0..8 {
        output[i] = mul23(MOD_A[i], input[i].wrapping_add(input[8 + i]));
    }
    for (i, k) in (8..16).zip((0..8).rev()) {
        output[i] = mul23(MOD_A[i], input[k].wrapping_sub(input[8 + k]));
    }
}

fn mod_b(input: &mut [i32], output: &mut [i32]) {
    for i in 0..8 {
        input[8 + i] = mul23(MOD_B[i], input[8 + i]);
    }
    for i in 0..8 {
        output[i] = input[i].wrapping_add(input[8 + i]);
    }
    for (i, k) in (8..16).zip((0..8).rev()) {
        output[i] = input[k].wrapping_sub(input[8 + k]);
    }
}

fn mod_c(input: &[i32], output: &mut [i32]) {
    for i in 0..16 {
        output[i] = mul23(MOD_C[i], input[i].wrapping_add(input[16 + i]));
    }
    for (i, k) in (16..32).zip((0..16).rev()) {
        output[i] = mul23(MOD_C[i], input[k].wrapping_sub(input[16 + k]));
    }
}

fn mod64_a(input: &[i32], output: &mut [i32]) {
    for i in 0..16 {
        output[i] = mul23(MOD64_A[i], input[i].wrapping_add(input[16 + i]));
    }
    for (i, k) in (16..32).zip((0..16).rev()) {
        output[i] = mul23(MOD64_A[i], input[k].wrapping_sub(input[16 + k]));
    }
}

fn mod64_b(input: &mut [i32], output: &mut [i32]) {
    for i in 0..16 {
        input[16 + i] = mul23(MOD64_B[i], input[16 + i]);
    }
    for i in 0..16 {
        output[i] = input[i].wrapping_add(input[16 + i]);
    }
    for (i, k) in (16..32).zip((0..16).rev()) {
        output[i] = input[k].wrapping_sub(input[16 + k]);
    }
}

fn mod64_c(input: &[i32], output: &mut [i32]) {
    for i in 0..32 {
        output[i] = mul23(MOD64_C[i], input[i].wrapping_add(input[32 + i]));
    }
    for (i, k) in (32..64).zip((0..32).rev()) {
        output[i] = mul23(MOD64_C[i], input[k].wrapping_sub(input[32 + k]));
    }
}

fn clp_v(input: &mut [i32]) {
    for v in input {
        *v = clip23(*v);
    }
}

/// Pre-scaling shift of the fixed-point IMDCT: inputs with a large magnitude are scaled down to
/// avoid overflow.
fn imdct_shift(input: &[i32]) -> u32 {
    let mut mag = 0i32;
    for &v in input {
        mag = mag.wrapping_add(v.wrapping_abs());
    }
    if mag > 0x40_0000 { 2 } else { 0 }
}

fn imdct_half_32(output: &mut [i32], input: &[i32]) {
    let mut buf_a = [0i32; 32];
    let mut buf_b = [0i32; 32];

    let shift = imdct_shift(&input[..32]);
    let round = if shift > 0 { 1i32 << (shift - 1) } else { 0 };

    for i in 0..32 {
        buf_a[i] = input[i].wrapping_add(round) >> shift;
    }

    sum_a(&buf_a, &mut buf_b[0..], 16);
    sum_b(&buf_a, &mut buf_b[16..], 16);
    clp_v(&mut buf_b);

    sum_a(&buf_b[0..], &mut buf_a[0..], 8);
    sum_b(&buf_b[0..], &mut buf_a[8..], 8);
    sum_c(&buf_b[16..], &mut buf_a[16..], 8);
    sum_d(&buf_b[16..], &mut buf_a[24..], 8);
    clp_v(&mut buf_a);

    dct_a(&buf_a[0..], &mut buf_b[0..]);
    dct_b(&buf_a[8..], &mut buf_b[8..]);
    dct_b(&buf_a[16..], &mut buf_b[16..]);
    dct_b(&buf_a[24..], &mut buf_b[24..]);
    clp_v(&mut buf_b);

    mod_a(&buf_b[0..], &mut buf_a[0..]);
    mod_b(&mut buf_b[16..], &mut buf_a[16..]);
    clp_v(&mut buf_a);

    mod_c(&buf_a, &mut buf_b);

    for v in &mut buf_b {
        *v = clip23(v.wrapping_shl(shift));
    }

    for (i, k) in (0..16).zip((0..32).rev()) {
        output[i] = clip23(buf_b[i].wrapping_sub(buf_b[k]));
        output[16 + i] = clip23(buf_b[i].wrapping_add(buf_b[k]));
    }
}

fn imdct_half_64(output: &mut [i32], input: &[i32]) {
    let mut buf_a = [0i32; 64];
    let mut buf_b = [0i32; 64];

    let shift = imdct_shift(&input[..64]);
    let round = if shift > 0 { 1i32 << (shift - 1) } else { 0 };

    for i in 0..64 {
        buf_a[i] = input[i].wrapping_add(round) >> shift;
    }

    sum_a(&buf_a, &mut buf_b[0..], 32);
    sum_b(&buf_a, &mut buf_b[32..], 32);
    clp_v(&mut buf_b);

    sum_a(&buf_b[0..], &mut buf_a[0..], 16);
    sum_b(&buf_b[0..], &mut buf_a[16..], 16);
    sum_c(&buf_b[32..], &mut buf_a[32..], 16);
    sum_d(&buf_b[32..], &mut buf_a[48..], 16);
    clp_v(&mut buf_a);

    sum_a(&buf_a[0..], &mut buf_b[0..], 8);
    sum_b(&buf_a[0..], &mut buf_b[8..], 8);
    sum_c(&buf_a[16..], &mut buf_b[16..], 8);
    sum_d(&buf_a[16..], &mut buf_b[24..], 8);
    sum_c(&buf_a[32..], &mut buf_b[32..], 8);
    sum_d(&buf_a[32..], &mut buf_b[40..], 8);
    sum_c(&buf_a[48..], &mut buf_b[48..], 8);
    sum_d(&buf_a[48..], &mut buf_b[56..], 8);
    clp_v(&mut buf_b);

    dct_a(&buf_b[0..], &mut buf_a[0..]);
    for o in (8..64).step_by(8) {
        dct_b(&buf_b[o..], &mut buf_a[o..]);
    }
    clp_v(&mut buf_a);

    mod_a(&buf_a[0..], &mut buf_b[0..]);
    mod_b(&mut buf_a[16..], &mut buf_b[16..]);
    mod_b(&mut buf_a[32..], &mut buf_b[32..]);
    mod_b(&mut buf_a[48..], &mut buf_b[48..]);
    clp_v(&mut buf_b);

    mod64_a(&buf_b[0..], &mut buf_a[0..]);
    mod64_b(&mut buf_b[32..], &mut buf_a[32..]);
    clp_v(&mut buf_a);

    mod64_c(&buf_a, &mut buf_b);

    for v in &mut buf_b {
        *v = clip23(v.wrapping_shl(shift));
    }

    for (i, k) in (0..32).zip((0..64).rev()) {
        output[i] = clip23(buf_b[i].wrapping_sub(buf_b[k]));
        output[32 + i] = clip23(buf_b[i].wrapping_add(buf_b[k]));
    }
}

/// Per-channel state of the fixed-point QMF synthesis filter bank.
pub struct SynthStateFixed {
    hist1: Vec<i32>,
    hist2: [i32; 64],
    offset: usize,
}

impl Default for SynthStateFixed {
    fn default() -> Self {
        SynthStateFixed { hist1: vec![0; 1024], hist2: [0; 64], offset: 0 }
    }
}

impl SynthStateFixed {
    pub fn reset(&mut self) {
        self.hist1.fill(0);
        self.hist2 = [0; 64];
        self.offset = 0;
    }

    /// One step of the synthesis filter: `bands` input subband samples produce `bands` PCM
    /// samples. `bands` is 32 or 64, `window` has `bands * 16` coefficients and `norm_bits` is 21
    /// for 32 bands and 20 for 64 bands.
    fn step(&mut self, bands: usize, window: &[i32], out: &mut [i32], input: &[i32]) {
        let half = bands / 2;
        let mask = bands * 16 - 1;
        let off = self.offset;
        let norm_bits = if bands == 32 { 21 } else { 20 };

        if bands == 32 {
            imdct_half_32(&mut self.hist1[off..off + bands], input);
        }
        else {
            imdct_half_64(&mut self.hist1[off..off + bands], input);
        }

        let h = &self.hist1;
        let step = bands * 2;
        for i in 0..half {
            let mut a = i64::from(self.hist2[i]) << norm_bits;
            let mut b = i64::from(self.hist2[i + half]) << norm_bits;
            let mut c = 0i64;
            let mut d = 0i64;

            let mut j = 0;
            while j < bands * 16 {
                let w = |n: usize| i64::from(window[n]);
                a = a.wrapping_add(w(i + j) * i64::from(h[(off + i + j) & mask]));
                b = b.wrapping_add(w(i + j + half) * i64::from(h[(off + half - 1 - i + j) & mask]));
                c = c.wrapping_add(w(i + j + bands) * i64::from(h[(off + half + i + j) & mask]));
                d = d.wrapping_add(
                    w(i + j + bands + half) * i64::from(h[(off + bands - 1 - i + j) & mask]),
                );
                j += step;
            }

            out[i] = clip23(norm(a, norm_bits));
            out[i + half] = clip23(norm(b, norm_bits));
            self.hist2[i] = norm(c, norm_bits);
            self.hist2[i + half] = norm(d, norm_bits);
        }

        self.offset = (off + bands * 16 - bands) & mask;
    }
}

/// 32-band fixed-point synthesis of `npcmblocks` blocks of subband samples into
/// `npcmblocks * 32` PCM samples.
pub fn sub_qmf32_fixed(
    state: &mut SynthStateFixed,
    perfect: bool,
    pcm: &mut [i32],
    lo: &Subbands<'_>,
    npcmblocks: usize,
) {
    let window: &[i32] =
        if perfect { &FIR_32BANDS_PERFECT_FIXED } else { &FIR_32BANDS_NONPERFECT_FIXED };
    let mut input = [0i32; 32];

    for j in 0..npcmblocks {
        for (i, x) in input.iter_mut().enumerate() {
            *x = lo.get(i, j);
        }
        state.step(32, window, &mut pcm[j * 32..j * 32 + 32], &input);
    }
}

/// 64-band fixed-point synthesis (X96). `hi` supplies the X96 subband samples, if present for the
/// channel.
pub fn sub_qmf64_fixed(
    state: &mut SynthStateFixed,
    pcm: &mut [i32],
    lo: &Subbands<'_>,
    hi: Option<&Subbands<'_>>,
    npcmblocks: usize,
) {
    let mut input = [0i32; 64];

    for j in 0..npcmblocks {
        if let Some(hi) = hi {
            for i in 0..32 {
                input[i] = lo.get(i, j).wrapping_add(hi.get(i, j));
            }
            for i in 32..64 {
                input[i] = hi.get(i, j);
            }
        }
        else {
            for i in 0..32 {
                input[i] = lo.get(i, j);
            }
        }
        state.step(64, &FIR_64BANDS_FIXED, &mut pcm[j * 64..j * 64 + 64], &input);
    }
}

/// Fixed-point LFE interpolation (64x decimation only). `lfe[base + n]` is decimated sample `n`
/// of the frame, preceded by history.
pub fn lfe_fir_fixed(pcm: &mut [i32], lfe: &[i32], base: usize, npcmblocks: usize) {
    let nlfesamples = npcmblocks >> 1;

    for i in 0..nlfesamples {
        let out = &mut pcm[i * 64..(i + 1) * 64];
        let pos = base + i;
        for j in 0..32 {
            let mut a = 0i64;
            let mut b = 0i64;
            for k in 0..8 {
                let s = i64::from(lfe[pos - k]);
                a = a.wrapping_add(i64::from(LFE_FIR_64_FIXED[j * 8 + k]) * s);
                b = b.wrapping_add(i64::from(LFE_FIR_64_FIXED[255 - j * 8 - k]) * s);
            }
            out[j] = clip23(norm(a, 23));
            out[32 + j] = clip23(norm(b, 23));
        }
    }
}

/// Fixed-point version of [`lfe_x96`].
pub fn lfe_x96_fixed(dst: &mut [i32], src: &[i32], hist: &mut i32) {
    let mut prev = *hist;
    for (d, &s) in dst.chunks_exact_mut(2).zip(src) {
        let a = 2097471i64 * i64::from(s) + 6291137i64 * i64::from(prev);
        let b = 6291137i64 * i64::from(s) + 2097471i64 * i64::from(prev);
        prev = s;
        d[0] = clip23(norm(a, 23));
        d[1] = clip23(norm(b, 23));
    }
    *hist = prev;
}

/// `dst1[i] -= dst2[i]`-style helper of the embedded XCH downmix: subtract the centre surround
/// contribution `src * sqrt(1/2)` from both surround channels.
pub fn dmix_sub_xch(dst1: &mut [i32], dst2: &mut [i32], src: &[i32]) {
    for ((d1, d2), &s) in dst1.iter_mut().zip(dst2.iter_mut()).zip(src) {
        let cs = mul23(s, 5931520 /* M_SQRT1_2 * (1 << 23) */);
        *d1 = d1.wrapping_sub(cs);
        *d2 = d2.wrapping_sub(cs);
    }
}

/// `dst[i] -= mul15(src[i], coeff)`.
pub fn dmix_sub(dst: &mut [i32], src: &[i32], coeff: i32) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = d.wrapping_sub(mul15(s, coeff));
    }
}

/// `dst[i] = mul16(dst[i], scale_inv)`.
pub fn dmix_scale_inv(dst: &mut [i32], scale_inv: i32) {
    for d in dst {
        *d = mul16(*d, scale_inv);
    }
}

/// `(a, b) = (a + b, a - b)`.
pub fn butterflies_fixed(a: &mut [i32], b: &mut [i32]) {
    for (x, y) in a.iter_mut().zip(b.iter_mut()) {
        let t = x.wrapping_sub(*y);
        *x = x.wrapping_add(*y);
        *y = t;
    }
}
