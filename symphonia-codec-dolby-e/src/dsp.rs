// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The derived tables (windows, mantissa scales, exponent scales, gains) and the inverse
//! transforms of the decoder.
//!
//! Ported from `init_tables`, `imdct_calc` and `transform` of FFmpeg's `libavcodec/dolby_e.c`
//! (LGPL-2.1-or-later, Copyright (c) 2017 foo86).

use std::f64::consts::FRAC_1_SQRT_2;
use std::sync::LazyLock;

use symphonia_core::dsp::mdct::Imdct;
use symphonia_core::dsp::window::{WindowType, generate_window};

use crate::tables::*;

/// The length of the window table.
const WINDOW_LEN: usize = 3712;

/// The maximum number of mantissas of a channel.
pub(crate) const MAX_MANTISSAS: usize = 1024;

/// The number of samples of a transform block of a channel (before the windowed overlap).
pub(crate) const BLOCK_LEN: usize = 1152;

/// The number of samples that overlap between blocks.
pub(crate) const OVERLAP: usize = 256;

/// The number of samples of a segment (half a frame) of a channel.
pub(crate) const SEGMENT_SAMPLES: usize = 896;

/// The tables that are derived from constants.
pub(crate) struct Tables {
    /// The scale of the mantissas by size and index: `mantissa1[size][idx]`.
    pub mantissa1: [[f32; 4]; 17],
    /// The scale and offset of the escaped mantissas by size and index.
    pub mantissa2: [[f32; 4]; 17],
    pub mantissa3: [[f32; 4]; 17],
    /// The scale of the exponents.
    pub exponent: [f32; 50],
    /// The gain by (10-bit) gain code.
    pub gain: [f32; 1024],
    /// The windows of all the shapes of the blocks, selected by `Group::win_ofs`.
    pub window: [f32; WINDOW_LEN],
}

impl Tables {
    fn new() -> Box<Self> {
        let mut t = Box::new(Tables {
            mantissa1: [[0.0; 4]; 17],
            mantissa2: [[0.0; 4]; 17],
            mantissa3: [[0.0; 4]; 17],
            exponent: [0.0; 50],
            gain: [0.0; 1024],
            window: [0.0; WINDOW_LEN],
        });

        for i in 1..17 {
            t.mantissa1[i][0] = 1.0 / (1u32 << (i - 1)) as f32;
        }

        for i in 2..16 {
            let d = ((1u32 << i) - 1) as f32;
            t.mantissa1[i][1] = 1.0 / d;
            t.mantissa1[i][2] = 0.5 / d;
            t.mantissa1[i][3] = 0.25 / d;
        }

        t.mantissa1[16][1] = 0.5 / (1u32 << 15) as f32;
        t.mantissa1[16][2] = 0.75 / (1u32 << 15) as f32;
        t.mantissa1[16][3] = 0.875 / (1u32 << 15) as f32;

        for i in 1..17 {
            t.mantissa2[i][1] = t.mantissa1[i][0] * 0.5;
            t.mantissa2[i][2] = t.mantissa1[i][0] * 0.75;
            t.mantissa2[i][3] = t.mantissa1[i][0] * 0.875;

            for j in 1..4 {
                t.mantissa3[i][j] = 1.0 / (1u32 << i) as f32 + 1.0 / (1u32 << j) as f32
                    - 1.0 / (1u32 << (i + j)) as f32;
            }
        }

        t.mantissa3[1][3] = 0.6875;

        for i in 0..25 {
            t.exponent[i * 2] = 1.0 / (1u32 << i) as f32;
            t.exponent[i * 2 + 1] = (FRAC_1_SQRT_2 / f64::from(1u32 << i)) as f32;
        }

        for i in 1..1024 {
            t.gain[i] = ((i as f32 - 960.0) / 64.0).exp2();
        }

        let w = &mut t.window;

        // Short 1.
        generate_window(WindowType::KaiserBessel(3.0), 1.0, 128, true, &mut w[..128]);
        for i in 0..128 {
            w[128 + i] = w[127 - i];
        }

        // Start.
        w[256..448].copy_from_slice(&START_WINDOW);

        // Short 2.
        w[448..640].copy_from_slice(&SHORT_WINDOW2);
        for i in 0..64 {
            w[640 + i] = w[63 - i];
        }

        // Short 3.
        w[704..768].copy_from_slice(&SHORT_WINDOW3);
        for i in 0..192 {
            w[768 + i] = w[64 + i];
        }

        // Bridge.
        for i in 0..128 {
            w[960 + i] = w[i];
        }
        w[1088..1152].fill(1.0);

        // Long.
        generate_window(WindowType::KaiserBessel(3.0), 1.0, 256, true, &mut w[1408..1664]);
        w[1664..2304].fill(1.0);
        for i in 0..256 {
            let v = w[1663 - i];
            w[2304 + i] = v;
            w[1152 + i] = v;
        }

        // Reverse start.
        for i in 0..192 {
            w[2560 + i] = w[447 - i];
        }

        // Reverse short 2.
        for i in 0..256 {
            w[2752 + i] = w[703 - i];
        }

        // Reverse short 3.
        for i in 0..256 {
            w[3008 + i] = w[959 - i];
        }

        // Reverse bridge.
        for i in 0..448 {
            w[3264 + i] = w[1407 - i];
        }

        t
    }
}

/// The derived tables.
pub(crate) static TABLES: LazyLock<Box<Tables>> = LazyLock::new(Tables::new);

/// The inverse transforms of the three sizes, and the scratch of the transform of a channel.
pub(crate) struct Transform {
    /// The inverse MDCTs of 128, 256, and 1024 coefficients.
    imdct: [Imdct; 3],
    /// The full output of the transform.
    full: Box<[f32; 2048]>,
    /// The block of the transform, with the phase applied.
    block: Box<[f32; 2048]>,
}

impl Transform {
    pub fn new() -> Self {
        // The transform is scaled by 2 (as FFmpeg's), and negated: with the positive scale the
        // output is the exact negation of FFmpeg's (`ff_mdct_init(.., inverse = 1, 2.0)`), with the
        // negative one it is identical (max error 5e-8 on the `16-11` sample).
        Transform {
            imdct: [
                Imdct::new_scaled(128, -2.0),
                Imdct::new_scaled(256, -2.0),
                Imdct::new_scaled(1024, -2.0),
            ],
            full: Box::new([0.0; 2048]),
            block: Box::new([0.0; 2048]),
        }
    }

    /// Transform the `values` (of the group `g`) into `self.block`.
    ///
    /// The phase selects how the block is derived from the middle half of the transform output
    /// (which is all that FFmpeg's half transform produces): 0 mirrors it to the second half, 1
    /// is the full transform, and 2 mirrors it, negated, to the first half.
    fn imdct_calc(&mut self, g: &Group, mantissas: &[f32; MAX_MANTISSAS]) {
        let idx = usize::from(g.imdct_idx);
        let coefs = 1usize << (IMDCT_BITS[idx] - 1);
        let n = coefs * 2;

        let values = &mantissas[usize::from(g.mnt_ofs)..][..coefs];
        self.imdct[idx].imdct(values, &mut self.full[..n]);

        let (full, block) = (&self.full[..n], &mut self.block[..n]);

        match g.imdct_phs {
            0 => {
                block[..coefs].copy_from_slice(&full[n / 4..n / 4 + coefs]);
                for i in 0..coefs {
                    block[coefs + i] = block[coefs - i - 1];
                }
            }
            1 => block.copy_from_slice(full),
            _ => {
                block[coefs..].copy_from_slice(&full[n / 4..n / 4 + coefs]);
                for i in 0..coefs {
                    block[i] = -block[n - i - 1];
                }
            }
        }
    }

    /// Transform the groups of a channel, window and overlap-add them into `result`.
    pub fn transform_groups(
        &mut self,
        groups: &[Group],
        mantissas: &[f32; MAX_MANTISSAS],
        result: &mut [f32; BLOCK_LEN],
    ) {
        let window = &TABLES.window;

        result.fill(0.0);

        for g in groups {
            self.imdct_calc(g, mantissas);

            let len = usize::from(g.win_len);
            let src = &self.block[usize::from(g.src_ofs)..][..len];
            let win = &window[usize::from(g.win_ofs)..][..len];
            let dst = &mut result[usize::from(g.dst_ofs)..][..len];

            for ((d, &s), &w) in dst.iter_mut().zip(src).zip(win) {
                *d += s * w;
            }
        }
    }
}
