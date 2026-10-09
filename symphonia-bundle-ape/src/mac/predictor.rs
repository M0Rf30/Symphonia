// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

//! Predictor stage for the Monkey's Audio decoder.
//!
//! Implements:
//! - `ScaledFirstOrderFilter` -- simple IIR filter (multiply=31, shift=5)
//! - `Predictor3950` -- for version >= 3950, bitsPerSample < 32
//! - `Predictor3950_32` -- for version >= 3950, bitsPerSample >= 32
//!
//! Reference: `NewPredictor.h`, `NewPredictor.cpp`, `ScaledFirstOrderFilter.h`.
//!
//! A predictor is made of the neural network filter cascade, which only depends on the channel's
//! own residuals, and the second stage, which also depends on the other channel.

use crate::mac::nn_filter::{NnFilter16, NnFilter32, create_filters_16, create_filters_32};

/// Initial coefficients for the "a" taps.
const INITIAL_MA: [i32; 4] = [360, 317, -109, 98];
const INITIAL_MA_64: [i64; 4] = [360, 317, -109, 98];

/// The adaptation direction of a value: `+1` for negative values, `-1` for positive values, `0`
/// for zero. This is the inverse of the usual signum.
#[inline(always)]
fn adapt_sign(value: i32) -> i32 {
    i32::from(value < 0) - i32::from(value > 0)
}

/// The adaptation direction of a value of the 32-bit path. Note that this only looks at bit 31 of
/// the value (not its sign), matching the reference.
#[inline(always)]
fn adapt_sign_64(value: i64) -> i64 {
    if value != 0 { ((value >> 30) & 2) - 1 } else { 0 }
}

// ---------------------------------------------------------------------------
// ScaledFirstOrderFilter
// ---------------------------------------------------------------------------

/// Simple first-order IIR filter: `y[n] = x[n] + (last * 31) >> 5`.
///
/// Template parameters in C++: `<INTTYPE, MULTIPLY=31, SHIFT=5>`. We hardcode multiply=31,
/// shift=5. The INTTYPE is always i32 for last_value; the input/output width varies but the
/// filter itself stores i32.
#[derive(Clone)]
pub struct ScaledFirstOrderFilter {
    last_value: i32,
}

impl ScaledFirstOrderFilter {
    pub fn new() -> Self {
        Self { last_value: 0 }
    }

    /// Decompress (inverse filter): used for channel A output.
    /// `last_value = input + (last_value * 31) >> 5; return last_value`
    #[inline(always)]
    pub fn decompress(&mut self, input: i32) -> i32 {
        self.last_value = input.wrapping_add(((self.last_value as i64 * 31) >> 5) as i32);
        self.last_value
    }

    /// Compress (forward filter): used for channel B input DURING decompression.
    /// `result = input - (last_value * 31) >> 5; last_value = input; return result`
    #[inline(always)]
    pub fn compress(&mut self, input: i32) -> i32 {
        let result = input.wrapping_sub(((self.last_value as i64 * 31) >> 5) as i32);
        self.last_value = input;
        result
    }
}

// ===================================================================
// Predictor3950 -- version >= 3950, bitsPerSample < 32
// (INTTYPE=i32, DATATYPE=i16)
// ===================================================================

/// The second stage of the predictor for 8/16/24-bit audio.
///
/// The reference keeps the history of the two prediction signals in roll buffers whose slot `-1`
/// is overwritten with the first difference of the signal (and whose adaptation signs are
/// recorded in parallel buffers). The taps are therefore, for the "a" signal: the last value, its
/// difference to the value before, and the two differences before that; and for the "b" signal:
/// the compressed input value, its difference to the one before, and the three differences before
/// that. They are kept in small shift registers here.
#[derive(Clone)]
struct Stage2 {
    ary_ma: [i32; 4],
    ary_mb: [i32; 5],
    stage1_filter_a: ScaledFirstOrderFilter,
    stage1_filter_b: ScaledFirstOrderFilter,
    last_value_a: i32,
    prev_raw_a: i32,
    diff_a: [i32; 2],
    sign_a: [i32; 2],
    prev_raw_b: i32,
    diff_b: [i32; 3],
    sign_b: [i32; 3],
}

impl Stage2 {
    fn new() -> Self {
        Self {
            ary_ma: INITIAL_MA,
            ary_mb: [0; 5],
            stage1_filter_a: ScaledFirstOrderFilter::new(),
            stage1_filter_b: ScaledFirstOrderFilter::new(),
            last_value_a: 0,
            prev_raw_a: 0,
            diff_a: [0; 2],
            sign_a: [0; 2],
            prev_raw_b: 0,
            diff_b: [0; 3],
            sign_b: [0; 3],
        }
    }

    /// Decompress a single sample.
    ///
    /// * `n_a` -- the residual of this channel after the neural network filters.
    /// * `n_b` -- cross-channel value (previous X output, or 0).
    ///
    /// Returns the final PCM sample value.
    #[inline(always)]
    fn step<const WIDE_INTERIM: bool>(&mut self, n_a: i32, n_b: i32) -> i32 {
        // Prediction buffers: store last_value_a, and the first difference to the value before.
        let raw_a = self.last_value_a;
        let delta_a = raw_a.wrapping_sub(self.prev_raw_a);

        // B buffer: compress nB through stage1_filter_b, then the first difference.
        let raw_b = self.stage1_filter_b.compress(n_b);
        let delta_b = raw_b.wrapping_sub(self.prev_raw_b);

        let [ma0, ma1, ma2, ma3] = self.ary_ma;
        let [mb0, mb1, mb2, mb3, mb4] = self.ary_mb;
        let [da1, da2] = self.diff_a;
        let [db1, db2, db3] = self.diff_b;

        // Compute prediction and add to residual
        let n_current_a: i32 = if WIDE_INTERIM {
            // Interim mode of high bit-depth audio: keep full precision.
            let pred_a: i64 = i64::from(raw_a)
                .wrapping_mul(i64::from(ma0))
                .wrapping_add(i64::from(delta_a).wrapping_mul(i64::from(ma1)))
                .wrapping_add(i64::from(da1).wrapping_mul(i64::from(ma2)))
                .wrapping_add(i64::from(da2).wrapping_mul(i64::from(ma3)));
            let pred_b: i64 = i64::from(raw_b)
                .wrapping_mul(i64::from(mb0))
                .wrapping_add(i64::from(delta_b).wrapping_mul(i64::from(mb1)))
                .wrapping_add(i64::from(db1).wrapping_mul(i64::from(mb2)))
                .wrapping_add(i64::from(db2).wrapping_mul(i64::from(mb3)))
                .wrapping_add(i64::from(db3).wrapping_mul(i64::from(mb4)));
            n_a.wrapping_add((pred_a.wrapping_add(pred_b >> 1) >> 10) as i32)
        }
        else {
            // Normal path: all arithmetic in i32 (wrapping to match C++ signed overflow). For
            // >16-bit audio outside of interim mode the reference computes the sums in i64 and
            // truncates them to i32 before combining them, which is the same thing.
            let pred_a: i32 = raw_a
                .wrapping_mul(ma0)
                .wrapping_add(delta_a.wrapping_mul(ma1))
                .wrapping_add(da1.wrapping_mul(ma2))
                .wrapping_add(da2.wrapping_mul(ma3));
            let pred_b: i32 = raw_b
                .wrapping_mul(mb0)
                .wrapping_add(delta_b.wrapping_mul(mb1))
                .wrapping_add(db1.wrapping_mul(mb2))
                .wrapping_add(db2.wrapping_mul(mb3))
                .wrapping_add(db3.wrapping_mul(mb4));
            n_a.wrapping_add(pred_a.wrapping_add(pred_b >> 1) >> 10)
        };

        // Adaptation signs of the prediction taps.
        let sign_a = [adapt_sign(raw_a), adapt_sign(delta_a), self.sign_a[0], self.sign_a[1]];
        let sign_b = [
            adapt_sign(raw_b),
            adapt_sign(delta_b),
            self.sign_b[0],
            self.sign_b[1],
            self.sign_b[2],
        ];

        // Adapt coefficients. The direction uses the post-NNFilter nA (NOT nCurrentA, NOT the
        // original residual).
        let adapt_dir = adapt_sign(n_a);
        for (m, s) in self.ary_ma.iter_mut().zip(sign_a) {
            *m = m.wrapping_add(s.wrapping_mul(adapt_dir));
        }
        for (m, s) in self.ary_mb.iter_mut().zip(sign_b) {
            *m = m.wrapping_add(s.wrapping_mul(adapt_dir));
        }

        // Stage 1 filter and output.
        let result = self.stage1_filter_a.decompress(n_current_a);

        // CRITICAL: last_value_a is set to nCurrentA (pre-filter value), NOT the result of
        // stage1_filter_a.
        self.last_value_a = n_current_a;

        // Advance the histories.
        self.prev_raw_a = raw_a;
        self.diff_a = [delta_a, da1];
        self.sign_a = [sign_a[1], self.sign_a[0]];
        self.prev_raw_b = raw_b;
        self.diff_b = [delta_b, db1, db2];
        self.sign_b = [sign_b[1], self.sign_b[0], self.sign_b[1]];

        result
    }
}

/// Predictor for version >= 3950, bitsPerSample < 32.
#[derive(Clone)]
pub struct Predictor3950 {
    nn_filters: Vec<NnFilter16>,
    stage2: Stage2,
    wide: bool,
    interim_mode: bool,
}

impl Predictor3950 {
    /// Create a new predictor for version >= 3950, bitsPerSample < 32.
    ///
    /// * `compression_level` -- APE compression level (1000..5000).
    /// * `version` -- file version number (>= 3950).
    /// * `bits_per_sample` -- 8, 16, or 24.
    pub fn new(compression_level: u32, version: i32, bits_per_sample: u16) -> Self {
        Self {
            nn_filters: create_filters_16(compression_level, version),
            stage2: Stage2::new(),
            wide: bits_per_sample > 16,
            interim_mode: false,
        }
    }

    pub fn set_interim_mode(&mut self, mode: bool) {
        self.interim_mode = mode;
        for f in &mut self.nn_filters {
            f.set_interim_mode(mode);
        }
    }

    /// Reset all state. Called at the start of each frame.
    pub fn flush(&mut self) {
        self.stage2 = Stage2::new();
        for f in &mut self.nn_filters {
            f.flush();
        }
    }

    /// Decompress a single sample.
    ///
    /// * `n_a` -- the entropy-decoded residual of this channel.
    /// * `n_b` -- cross-channel value (previous X output, or 0).
    ///
    /// Returns the final PCM sample value.
    #[inline(always)]
    pub fn decompress_value(&mut self, n_a: i64, n_b: i32) -> i32 {
        // The neural network filter cascade runs in decompression order (reverse of creation: the
        // smallest filter first, then toward the largest). Only nA passes through it.
        let mut n_a = n_a as i32;
        for f in self.nn_filters.iter_mut().rev() {
            n_a = f.step(n_a);
        }

        if self.wide && self.interim_mode {
            self.stage2.step::<true>(n_a, n_b)
        }
        else {
            self.stage2.step::<false>(n_a, n_b)
        }
    }
}

// ===================================================================
// Predictor3950_32 -- version >= 3950, bitsPerSample >= 32
// (INTTYPE=i64, DATATYPE=i32)
// ===================================================================

/// The second stage of the predictor for 32-bit audio; see [`Stage2`].
#[derive(Clone)]
struct Stage2_32 {
    ary_ma: [i64; 4],
    ary_mb: [i64; 5],
    stage1_filter_a: ScaledFirstOrderFilter,
    stage1_filter_b: ScaledFirstOrderFilter,
    last_value_a: i64,
    prev_raw_a: i64,
    diff_a: [i64; 2],
    sign_a: [i64; 2],
    prev_raw_b: i64,
    diff_b: [i64; 3],
    sign_b: [i64; 3],
}

impl Stage2_32 {
    fn new() -> Self {
        Self {
            ary_ma: INITIAL_MA_64,
            ary_mb: [0; 5],
            stage1_filter_a: ScaledFirstOrderFilter::new(),
            stage1_filter_b: ScaledFirstOrderFilter::new(),
            last_value_a: 0,
            prev_raw_a: 0,
            diff_a: [0; 2],
            sign_a: [0; 2],
            prev_raw_b: 0,
            diff_b: [0; 3],
            sign_b: [0; 3],
        }
    }

    #[inline(always)]
    fn step(&mut self, n_a: i64, n_b: i32) -> i32 {
        let raw_a = self.last_value_a;
        let delta_a = raw_a.wrapping_sub(self.prev_raw_a);

        let raw_b = i64::from(self.stage1_filter_b.compress(n_b));
        let delta_b = raw_b.wrapping_sub(self.prev_raw_b);

        let [ma0, ma1, ma2, ma3] = self.ary_ma;
        let [mb0, mb1, mb2, mb3, mb4] = self.ary_mb;
        let [da1, da2] = self.diff_a;
        let [db1, db2, db3] = self.diff_b;

        // All i64 arithmetic for the 32-bit path.
        let pred_a: i64 = raw_a
            .wrapping_mul(ma0)
            .wrapping_add(delta_a.wrapping_mul(ma1))
            .wrapping_add(da1.wrapping_mul(ma2))
            .wrapping_add(da2.wrapping_mul(ma3));
        let pred_b: i64 = raw_b
            .wrapping_mul(mb0)
            .wrapping_add(delta_b.wrapping_mul(mb1))
            .wrapping_add(db1.wrapping_mul(mb2))
            .wrapping_add(db2.wrapping_mul(mb3))
            .wrapping_add(db3.wrapping_mul(mb4));
        let n_current_a: i64 = n_a.wrapping_add(pred_a.wrapping_add(pred_b >> 1) >> 10);

        let sign_a = [adapt_sign_64(raw_a), adapt_sign_64(delta_a), self.sign_a[0], self.sign_a[1]];
        let sign_b = [
            adapt_sign_64(raw_b),
            adapt_sign_64(delta_b),
            self.sign_b[0],
            self.sign_b[1],
            self.sign_b[2],
        ];

        let adapt_dir: i64 = i64::from(n_a < 0) - i64::from(n_a > 0);
        for (m, s) in self.ary_ma.iter_mut().zip(sign_a) {
            *m = m.wrapping_add(s.wrapping_mul(adapt_dir));
        }
        for (m, s) in self.ary_mb.iter_mut().zip(sign_b) {
            *m = m.wrapping_add(s.wrapping_mul(adapt_dir));
        }

        let result = self.stage1_filter_a.decompress(n_current_a as i32);
        self.last_value_a = n_current_a;

        self.prev_raw_a = raw_a;
        self.diff_a = [delta_a, da1];
        self.sign_a = [sign_a[1], self.sign_a[0]];
        self.prev_raw_b = raw_b;
        self.diff_b = [delta_b, db1, db2];
        self.sign_b = [sign_b[1], self.sign_b[0], self.sign_b[1]];

        result
    }
}

/// Predictor for version >= 3950, bitsPerSample >= 32.
#[derive(Clone)]
pub struct Predictor3950_32 {
    nn_filters: Vec<NnFilter32>,
    stage2: Stage2_32,
}

impl Predictor3950_32 {
    pub fn new(compression_level: u32, version: i32) -> Self {
        Self { nn_filters: create_filters_32(compression_level, version), stage2: Stage2_32::new() }
    }

    pub fn flush(&mut self) {
        self.stage2 = Stage2_32::new();
        for f in &mut self.nn_filters {
            f.flush();
        }
    }

    /// Decompress a single sample; `n_a` is the entropy-decoded residual, `n_b` the cross-channel
    /// value.
    #[inline(always)]
    pub fn decompress_value(&mut self, n_a: i64, n_b: i32) -> i32 {
        let mut n_a = n_a;
        for f in self.nn_filters.iter_mut().rev() {
            n_a = f.step(n_a);
        }
        self.stage2.step(n_a, n_b)
    }
}
