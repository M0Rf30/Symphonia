// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

//! Neural network filters of the Monkey's Audio decoder.
//!
//! Two concrete types instead of generics, matching the C++ template instantiations:
//!
//! - `NnFilter16` -- for 8/16/24-bit audio (INTTYPE=i32, DATATYPE=i16)
//! - `NnFilter32` -- for 32-bit audio (INTTYPE=i64, DATATYPE=i32)
//!
//! Reference: `NNFilter.h`, `NNFilter.cpp`, `NNFilterGeneric.cpp`, `NNFilterCommon.h`.
//!
//! A filter is stepped one sample at a time: a residual goes in, the filtered sample comes out.
//! The filters of a channel form a cascade that only depends on that channel's own residuals.

/// Number of samples a filter's history buffers hold in addition to the `order` samples of
/// history, before they are rolled back to the start.
const NN_WINDOW: usize = 512;

/// Compression level constants.
pub const COMPRESSION_FAST: u32 = 1000;
pub const COMPRESSION_NORMAL: u32 = 2000;
pub const COMPRESSION_HIGH: u32 = 3000;
pub const COMPRESSION_EXTRA_HIGH: u32 = 4000;
pub const COMPRESSION_INSANE: u32 = 5000;

/// Filter configuration: (order, shift).
type FilterConfig = (usize, u32);

/// Return the filter configurations for a given compression level.
///
/// The returned list is in creation order (largest filter first for multi-filter levels). The
/// filters are applied in the reverse order.
fn filter_configs(compression_level: u32) -> &'static [FilterConfig] {
    match compression_level {
        COMPRESSION_FAST => &[],
        COMPRESSION_NORMAL => &[(16, 11)],
        COMPRESSION_HIGH => &[(64, 11)],
        COMPRESSION_EXTRA_HIGH => &[(256, 13), (32, 10)],
        COMPRESSION_INSANE => &[(1280, 15), (256, 13), (16, 11)],
        _ => &[],
    }
}

/// Whether a file version uses the current delta update rule (>= 3980) or the old one.
#[inline]
fn uses_new_delta(version: i32) -> bool {
    version == -1 || version >= 3980
}

// ===================================================================
// NnFilter16 -- for 8/16/24-bit audio (INTTYPE=i32, DATATYPE=i16)
// ===================================================================

/// Fused dot product and weight adaptation for the <i32, i16> path.
///
/// Computes the dot product of `hist` and the weights `w` (i16 * i16 accumulated in i32), and
/// adapts the weights (`w += delta` if `SUB` is false, `w -= delta` otherwise) in the same pass.
/// The dot product uses the weights as they were before the adaptation.
///
/// The accumulator lanes and the chunks of 8 elements are what lets the compiler turn this into
/// multiply-add (`pmaddwd`) instructions. The kernels are not inlined, as that makes the compiler
/// lose track of the weights not aliasing the history and deltas.
#[inline(never)]
fn dot_adapt_16<const N: usize, const SUB: bool>(
    hist: &[i16; N],
    w: &mut [i16; N],
    delta: &[i16; N],
) -> i32 {
    let mut acc = [0i32; 8];
    for ((h, w), d) in hist.chunks_exact(8).zip(w.chunks_exact_mut(8)).zip(delta.chunks_exact(8)) {
        for j in 0..8 {
            acc[j] = acc[j].wrapping_add(i32::from(h[j]) * i32::from(w[j]));
        }
        for j in 0..8 {
            w[j] = if SUB { w[j].wrapping_sub(d[j]) } else { w[j].wrapping_add(d[j]) };
        }
    }
    acc.iter().fold(0i32, |sum, &v| sum.wrapping_add(v))
}

/// Fused dot product and weight adaptation, with the deltas multiplied by the adaptation
/// direction `s` (-1, 0 or 1) instead of being added or subtracted.
#[inline(never)]
fn dot_adapt_mul_16<const N: usize>(
    hist: &[i16; N],
    w: &mut [i16; N],
    delta: &[i16; N],
    s: i16,
) -> i32 {
    let mut acc = [0i32; 8];
    for ((h, w), d) in hist.chunks_exact(8).zip(w.chunks_exact_mut(8)).zip(delta.chunks_exact(8)) {
        for j in 0..8 {
            acc[j] = acc[j].wrapping_add(i32::from(h[j]) * i32::from(w[j]));
        }
        for j in 0..8 {
            w[j] = w[j].wrapping_add(d[j].wrapping_mul(s));
        }
    }
    acc.iter().fold(0i32, |sum, &v| sum.wrapping_add(v))
}

/// Dot product for the <i32, i16> path, for when the weights are not adapted.
#[inline(never)]
fn dot_16<const N: usize>(hist: &[i16; N], w: &[i16; N]) -> i32 {
    let mut acc = [0i32; 8];
    for (h, w) in hist.chunks_exact(8).zip(w.chunks_exact(8)) {
        for j in 0..8 {
            acc[j] = acc[j].wrapping_add(i32::from(h[j]) * i32::from(w[j]));
        }
    }
    acc.iter().fold(0i32, |sum, &v| sum.wrapping_add(v))
}

/// A neural network filter for 8/16/24-bit audio.
#[derive(Clone)]
pub struct NnFilter16 {
    order: usize,
    shift: u32,
    one_shifted: i32,
    new_delta: bool,
    weights: Vec<i16>,
    /// Saturated output history; `order + NN_WINDOW` samples.
    hist: Vec<i16>,
    /// Weight adaptation deltas; `order + NN_WINDOW` samples.
    delta: Vec<i16>,
    /// Index of the next sample in `hist` and `delta`.
    pos: usize,
    running_average: i32,
    interim_mode: bool,
}

impl NnFilter16 {
    /// Create a new 16-bit NN filter.
    ///
    /// * `order` -- 16, 32, 64, 256 or 1280.
    /// * `shift` -- right-shift applied after dot product.
    /// * `version` -- file version; -1 means "current" (>= 3980 behaviour).
    pub fn new(order: usize, shift: u32, version: i32) -> Self {
        assert!(
            matches!(order, 16 | 32 | 64 | 256 | 1280),
            "NnFilter16: unsupported order {order}"
        );
        Self {
            order,
            shift,
            one_shifted: 1i32 << (shift - 1),
            new_delta: uses_new_delta(version),
            weights: vec![0i16; order],
            hist: vec![0i16; order + NN_WINDOW],
            delta: vec![0i16; order + NN_WINDOW],
            pos: order,
            running_average: 0,
            interim_mode: false,
        }
    }

    pub fn set_interim_mode(&mut self, mode: bool) {
        self.interim_mode = mode;
    }

    /// Reset all state. Called at the start of each frame.
    pub fn flush(&mut self) {
        self.weights.fill(0);
        // Only the history that will be read before it is written needs to be cleared.
        self.hist[..=self.order].fill(0);
        self.delta[..=self.order].fill(0);
        self.pos = self.order;
        self.running_average = 0;
    }

    /// Core decompression: takes an encoded residual, returns the reconstructed sample value.
    #[inline(always)]
    pub fn step(&mut self, input: i32) -> i32 {
        match self.order {
            16 => self.step_inline::<16>(input),
            32 => self.step_inline::<32>(input),
            64 => self.step_inline::<64>(input),
            256 => self.step_outline::<256>(input),
            1280 => self.step_outline::<1280>(input),
            _ => unreachable!("order is checked by the constructor"),
        }
    }

    #[inline(always)]
    fn step_inline<const N: usize>(&mut self, input: i32) -> i32 {
        self.step_order::<N, false>(input)
    }

    // The large filters are expensive enough for the call not to matter, and are not worth
    // inlining everywhere.
    #[inline(never)]
    fn step_outline<const N: usize>(&mut self, input: i32) -> i32 {
        self.step_order::<N, true>(input)
    }

    /// `BRANCH` selects how the weights are adapted for the direction of the input: with branches
    /// (best for the large filters, where a misprediction is cheap in comparison) or by
    /// multiplying the deltas with the direction (best for the small filters).
    #[inline(always)]
    fn step_order<const N: usize, const BRANCH: bool>(&mut self, input: i32) -> i32 {
        let pos = self.pos;

        // 1. Dot product over history, and 3. adapt the weights -- CRITICAL: the adaptation
        // direction is the sign of the INPUT (the residual), NOT the output. Both are done in
        // one pass over the weights.
        let weights: &mut [i16; N] = (&mut self.weights[..N]).try_into().unwrap();
        let input_hist: &[i16; N] = (&self.hist[pos - N..pos]).try_into().unwrap();
        let delta_hist: &[i16; N] = (&self.delta[pos - N..pos]).try_into().unwrap();
        let dot_product = if BRANCH {
            if input < 0 {
                dot_adapt_16::<N, false>(input_hist, weights, delta_hist)
            }
            else if input > 0 {
                dot_adapt_16::<N, true>(input_hist, weights, delta_hist)
            }
            else {
                dot_16(input_hist, weights)
            }
        }
        else {
            let direction = i16::from(input < 0) - i16::from(input > 0);
            dot_adapt_mul_16(input_hist, weights, delta_hist, direction)
        };

        // 2. Compute output (prediction + residual)
        let output: i32 = if self.interim_mode {
            // Widen to i64 before adding rounding bias and shifting
            input.wrapping_add(
                ((i64::from(dot_product) + i64::from(self.one_shifted)) >> self.shift) as i32,
            )
        }
        else {
            input.wrapping_add(dot_product.wrapping_add(self.one_shifted) >> self.shift)
        };

        // 4. Update delta buffer -- CRITICAL: uses OUTPUT (reconstructed sample)
        let delta = &mut self.delta;
        if self.new_delta {
            // UPDATE_DELTA_NEW (version >= 3980 or version == -1).
            let abs_value = output.wrapping_abs();
            let running_average = self.running_average;
            let magnitude: i32 = if abs_value > running_average.wrapping_mul(3) {
                32
            }
            else if abs_value > running_average.wrapping_mul(4) / 3 {
                16
            }
            else if abs_value > 0 {
                8
            }
            else {
                0
            };
            delta[pos] = (if output < 0 { magnitude } else { -magnitude }) as i16;

            // Exponential moving average (integer division truncates toward zero)
            self.running_average =
                running_average.wrapping_add(abs_value.wrapping_sub(running_average) / 16);

            // Decay historical deltas at positions [-1], [-2], [-8]
            delta[pos - 1] >>= 1;
            delta[pos - 2] >>= 1;
            delta[pos - 8] >>= 1;
        }
        else {
            // UPDATE_DELTA_OLD (version < 3980).
            delta[pos] = if output == 0 {
                0
            }
            else if output < 0 {
                4
            }
            else {
                -4
            };

            // Decay historical deltas at positions [-4], [-8]
            delta[pos - 4] >>= 1;
            delta[pos - 8] >>= 1;
        }

        // 5. Store saturated value in input history
        self.hist[pos] = output.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;

        // 6. Advance, rolling the history back to the start of the buffers if they are full.
        self.pos += 1;
        if self.pos == N + NN_WINDOW {
            self.hist.copy_within(NN_WINDOW.., 0);
            self.delta.copy_within(NN_WINDOW.., 0);
            self.pos = N;
        }

        output
    }
}

// ===================================================================
// NnFilter32 -- for 32-bit audio (INTTYPE=i64, DATATYPE=i32)
// ===================================================================

/// A neural network filter for 32-bit audio.
#[derive(Clone)]
pub struct NnFilter32 {
    order: usize,
    shift: u32,
    one_shifted: i64,
    new_delta: bool,
    weights: Vec<i32>,
    hist: Vec<i32>,
    delta: Vec<i32>,
    pos: usize,
    running_average: i64,
}

impl NnFilter32 {
    /// Create a new 32-bit NN filter.
    pub fn new(order: usize, shift: u32, version: i32) -> Self {
        assert!(
            matches!(order, 16 | 32 | 64 | 256 | 1280),
            "NnFilter32: unsupported order {order}"
        );
        Self {
            order,
            shift,
            one_shifted: i64::from(1i32 << (shift - 1)),
            new_delta: uses_new_delta(version),
            weights: vec![0i32; order],
            hist: vec![0i32; order + NN_WINDOW],
            delta: vec![0i32; order + NN_WINDOW],
            pos: order,
            running_average: 0,
        }
    }

    pub fn flush(&mut self) {
        self.weights.fill(0);
        self.hist[..=self.order].fill(0);
        self.delta[..=self.order].fill(0);
        self.pos = self.order;
        self.running_average = 0;
    }

    /// Core decompression: takes an encoded residual, returns the reconstructed sample value.
    #[inline(always)]
    pub fn step(&mut self, input: i64) -> i64 {
        let order = self.order;
        let pos = self.pos;
        let weights = &mut self.weights[..order];

        // 1. Dot product over history: each i32*i32 TRUNCATES to i32 via wrapping_mul BEFORE
        // widening to i64 for accumulation.
        let input_hist = &self.hist[pos - order..pos];
        let mut dot_product: i64 = 0;
        for (h, w) in input_hist.iter().zip(weights.iter()) {
            dot_product = dot_product.wrapping_add(i64::from(h.wrapping_mul(*w)));
        }

        // 2. Compute output
        let output: i64 =
            input.wrapping_add(dot_product.wrapping_add(self.one_shifted) >> self.shift);

        // 3. Adapt weights -- CRITICAL: uses INPUT (the residual)
        let delta_hist = &self.delta[pos - order..pos];
        if input < 0 {
            for (w, d) in weights.iter_mut().zip(delta_hist.iter()) {
                *w = w.wrapping_add(*d);
            }
        }
        else if input > 0 {
            for (w, d) in weights.iter_mut().zip(delta_hist.iter()) {
                *w = w.wrapping_sub(*d);
            }
        }

        // 4. Update delta buffer -- uses OUTPUT
        let delta = &mut self.delta;
        if self.new_delta {
            let abs_value = output.wrapping_abs();
            let running_average = self.running_average;

            // The shifts (25/26/27) operate on the full i64 width. Sign extraction only works
            // correctly when the value fits in 32 bits (which it generally does after
            // saturation clamping).
            delta[pos] = if abs_value > running_average.wrapping_mul(3) {
                (((output >> 25) & 64) - 32) as i32
            }
            else if abs_value > running_average.wrapping_mul(4) / 3 {
                (((output >> 26) & 32) - 16) as i32
            }
            else if abs_value > 0 {
                (((output >> 27) & 16) - 8) as i32
            }
            else {
                0
            };

            self.running_average =
                running_average.wrapping_add(abs_value.wrapping_sub(running_average) / 16);

            delta[pos - 1] >>= 1;
            delta[pos - 2] >>= 1;
            delta[pos - 8] >>= 1;
        }
        else {
            delta[pos] = if output == 0 { 0 } else { (((output >> 28) & 8) - 4) as i32 };

            delta[pos - 4] >>= 1;
            delta[pos - 8] >>= 1;
        }

        // 5. Store saturated value in input history. For <i64, i32>: DATATYPE is i32, but the
        // saturation is still to the i16 range.
        self.hist[pos] = output.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i32;

        // 6. Advance
        self.pos += 1;
        if self.pos == order + NN_WINDOW {
            self.hist.copy_within(NN_WINDOW.., 0);
            self.delta.copy_within(NN_WINDOW.., 0);
            self.pos = order;
        }

        output
    }
}

// ===================================================================
// Filter cascade helpers
// ===================================================================

/// Create NnFilter16 instances for a compression level, in creation order. The filters are
/// applied in the reverse order.
pub fn create_filters_16(compression_level: u32, version: i32) -> Vec<NnFilter16> {
    filter_configs(compression_level)
        .iter()
        .map(|&(order, shift)| NnFilter16::new(order, shift, version))
        .collect()
}

/// Create NnFilter32 instances for a compression level, in creation order.
pub fn create_filters_32(compression_level: u32, version: i32) -> Vec<NnFilter32> {
    filter_configs(compression_level)
        .iter()
        .map(|&(order, shift)| NnFilter32::new(order, shift, version))
        .collect()
}
