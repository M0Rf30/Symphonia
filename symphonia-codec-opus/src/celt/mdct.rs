// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Modified Discrete Cosine Transform. Ported from libopus `celt/mdct.c`
//! (`clt_mdct_backward`, and `clt_mdct_forward` for round-trip tests).
//! Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltSynthesis".

use crate::celt::kiss_fft::FftState;

/// C: `mdct_lookup` (the precomputed FFT plan + trig tables for one MDCT size).
pub struct MdctLookup {
    pub n: usize,
    pub max_shift: i32,
    pub kfft: [Option<FftState>; 4],
    // Trig tables (`trig`) populated by wave 1.
    _private: (),
}

impl MdctLookup {
    /// C: `clt_mdct_init`.
    pub fn new(n: usize, max_shift: i32) -> Self {
        let _ = (n, max_shift);
        todo!("wave 1 (celt/CeltSynthesis): clt_mdct_init")
    }

    /// C: `clt_mdct_backward`. Inverse MDCT of `input` (length `n/2`) into `out` (length `n`,
    /// overlap-added by the caller), windowed by `window` and interleaved across `channels`.
    #[allow(clippy::too_many_arguments)]
    pub fn backward(
        &self,
        input: &[f32],
        out: &mut [f32],
        window: &[f32],
        overlap: i32,
        shift: i32,
        stride: i32,
    ) {
        let _ = (input, out, window, overlap, shift, stride);
        todo!("wave 1 (celt/CeltSynthesis): clt_mdct_backward")
    }

    /// C: `clt_mdct_forward`, needed only by `#[cfg(test)]` round-trip tests (CELT decode never
    /// calls the forward transform itself).
    pub fn forward(&self, input: &[f32], out: &mut [f32], window: &[f32], overlap: i32, shift: i32) {
        let _ = (input, out, window, overlap, shift);
        todo!("wave 1 (celt/CeltSynthesis): clt_mdct_forward (test-only)")
    }
}
