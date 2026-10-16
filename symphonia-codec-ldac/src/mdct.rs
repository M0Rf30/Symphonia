// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The LDAC inverse modified discrete cosine transform (IMDCT).
//!
//! LDAC uses a 128-point (44.1/48 kHz) or 256-point (88.2/96 kHz) transform whose core is an
//! unscaled DCT-IV. The analysis window is a half-period sine taper
//! `w[n] = (sin((n + 0.5) / N - 0.5) * PI + 1.0) * 0.5`, and the synthesis window is the dual
//! `v[n] = w[n] / (w[N - 1 - n]^2 + w[n]^2)` so that the overlap-add of consecutive frames is
//! unity (time-domain aliasing cancellation).

use std::sync::LazyLock;

/// The maximum transform level used by LDAC (256 samples, so `1 << 8`).
const MAX_LEVEL: usize = 8;

/// Sine/cosine/shuffle tables for the DCT-IV core, indexed by transform level (log2 size).
struct DctTables {
    sin: Vec<Vec<f32>>,
    cos: Vec<Vec<f32>>,
    shuffle: Vec<Vec<usize>>,
}

fn bit_reverse(mut value: usize, bits: usize) -> usize {
    let mut reversed = 0;

    for _ in 0..bits {
        reversed = (reversed << 1) | (value & 1);
        value >>= 1;
    }

    reversed
}

static DCT_TABLES: LazyLock<DctTables> = LazyLock::new(|| {
    let mut sin = Vec::with_capacity(MAX_LEVEL + 1);
    let mut cos = Vec::with_capacity(MAX_LEVEL + 1);
    let mut shuffle = Vec::with_capacity(MAX_LEVEL + 1);

    for level in 0..=MAX_LEVEL {
        let size = 1usize << level;
        let mut s = vec![0.0f32; size];
        let mut c = vec![0.0f32; size];

        for (i, (s, c)) in s.iter_mut().zip(c.iter_mut()).enumerate() {
            let angle = std::f64::consts::PI * (4 * i + 1) as f64 / (4 * size) as f64;
            *s = angle.sin() as f32;
            *c = angle.cos() as f32;
        }

        let sh = (0..size).map(|i| bit_reverse(i ^ (i / 2), level)).collect();

        sin.push(s);
        cos.push(c);
        shuffle.push(sh);
    }

    DctTables { sin, cos, shuffle }
});

/// Computes the unscaled DCT-IV: `out[j] = sum_n in[n] * cos(PI / N * (n + 0.5) * (j + 0.5))`.
fn dct4(input: &[f32], output: &mut [f32], scratch: &mut [f32], tables: &DctTables) {
    let size = input.len();
    let bits = size.trailing_zeros() as usize;
    let last = size - 1;

    // First stage.
    for i in 0..size / 2 {
        let i2 = i * 2;
        let a = input[i2];
        let b = input[last - i2];
        let s = tables.sin[bits][i];
        let c = tables.cos[bits][i];

        scratch[i2] = a * c + b * s;
        scratch[i2 + 1] = a * s - b * c;
    }

    // Butterfly stages.
    for stage in 0..bits - 1 {
        let block_count = 1usize << stage;
        let block_size = 1usize << (bits - 1 - stage);
        let half_size = block_size / 2;
        let level = half_size.trailing_zeros() as usize;
        let s = &tables.sin[level];
        let c = &tables.cos[level];

        for block in 0..block_count {
            for i in 0..half_size {
                let front = (block * block_size + i) * 2;
                let back = front + block_size;
                let a = scratch[front] - scratch[back];
                let b = scratch[front + 1] - scratch[back + 1];

                scratch[front] += scratch[back];
                scratch[front + 1] += scratch[back + 1];
                scratch[back] = a * c[i] + b * s[i];
                scratch[back + 1] = a * s[i] - b * c[i];
            }
        }
    }

    // Shuffle to natural order.
    let shuffle = &tables.shuffle[bits];

    for (i, &index) in shuffle.iter().enumerate() {
        output[i] = scratch[index];
    }
}

/// The LDAC synthesis windows (`[level][index]`, `level` is `log2(size) - 6`).
static WINDOWS: LazyLock<[Box<[f32]>; 3]> = LazyLock::new(|| {
    let mut windows: [Box<[f32]>; 3] = [Box::new([]), Box::new([]), Box::new([])];

    for (level, slot) in windows.iter_mut().enumerate() {
        let size = 1 << (level + 6);
        let mut mdct = vec![0.0f32; size];
        let mut imdct = vec![0.0f32; size];

        for (i, w) in mdct.iter_mut().enumerate() {
            let angle = ((i as f64 + 0.5) / size as f64 - 0.5) * std::f64::consts::PI;
            *w = ((angle.sin() + 1.0) * 0.5) as f32;
        }

        for i in 0..size {
            let w = mdct[i];
            let w2 = mdct[size - 1 - i];
            imdct[i] = w / (w2 * w2 + w * w);
        }

        *slot = imdct.into_boxed_slice();
    }

    windows
});

/// The state of one channel's inverse transform.
pub(crate) struct LdacImdct {
    /// Transform level (log2 of the frame size), `7` or `8`.
    bits: usize,
    /// Overlap from the previous frame.
    previous: Vec<f32>,
    /// DCT-IV result buffer.
    dct_out: Vec<f32>,
    /// DCT-IV butterfly scratch buffer.
    scratch: Vec<f32>,
}

impl LdacImdct {
    pub(crate) fn new(bits: usize) -> Self {
        let size = 1 << bits;

        LdacImdct {
            bits,
            previous: vec![0.0; size],
            dct_out: vec![0.0; size],
            scratch: vec![0.0; size],
        }
    }

    pub(crate) fn reset(&mut self) {
        self.previous.fill(0.0);
    }

    /// The transform level (log2 of the frame size).
    pub(crate) fn bits(&self) -> usize {
        self.bits
    }

    /// Transforms `input` (the `size` dequantized spectral coefficients) into `output` (`size`
    /// time-domain samples), maintaining the overlap with the previous frame.
    pub(crate) fn imdct(&mut self, input: &[f32], output: &mut [f32]) {
        let size = 1 << self.bits;
        let half = size / 2;
        let window = &WINDOWS[self.bits - 6];

        dct4(input, &mut self.dct_out, &mut self.scratch, &DCT_TABLES);

        for i in 0..half {
            output[i] = window[i] * self.dct_out[i + half] + self.previous[i];
            output[i + half] =
                window[i + half] * -self.dct_out[size - 1 - i] - self.previous[i + half];
            self.previous[i] = window[size - 1 - i] * -self.dct_out[half - 1 - i];
            self.previous[i + half] = window[half - 1 - i] * self.dct_out[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The naive `O(N^2)` DCT-IV, the ground truth for the fast transform.
    fn dct4_naive(input: &[f32]) -> Vec<f32> {
        let size = input.len();

        (0..size)
            .map(|j| {
                input
                    .iter()
                    .enumerate()
                    .map(|(n, &x)| {
                        let angle = std::f64::consts::PI / size as f64
                            * (n as f64 + 0.5)
                            * (j as f64 + 0.5);
                        x as f64 * angle.cos()
                    })
                    .sum::<f64>() as f32
            })
            .collect()
    }

    #[test]
    fn dct4_matches_naive() {
        for bits in [7, 8] {
            let size = 1 << bits;
            let input: Vec<f32> =
                (0..size).map(|i| ((i as f64 * 0.618).sin() * 100.0) as f32).collect();
            let want = dct4_naive(&input);

            let mut output = vec![0.0; size];
            let mut scratch = vec![0.0; size];
            dct4(&input, &mut output, &mut scratch, &DCT_TABLES);

            let max = output
                .iter()
                .zip(&want)
                .map(|(&got, &want)| (got - want).abs())
                .fold(0.0f32, f32::max);

            assert!(max < 1.0e-3, "size {size}: max deviation {max}");
        }
    }
}
