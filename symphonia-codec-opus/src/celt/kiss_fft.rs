// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mixed-radix FFT, the core of the MDCT. Ported from libopus `celt/kiss_fft.c`
//! (`opus_fft_alloc`, `opus_fft`, `opus_ifft`) restricted to the float build.
//! Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltSynthesis".

/// A complex sample, C: `kiss_fft_cpx`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Complex {
    pub r: f32,
    pub i: f32,
}

/// C: `kiss_fft_state` (the precomputed twiddle-factor/factorization plan for one FFT size).
pub struct FftState {
    pub nfft: usize,
    // Twiddles, factors, bit-reversal table, etc. populated by wave 1.
    _private: (),
}

impl FftState {
    /// C: `opus_fft_alloc_twiddles` / `opus_fft_alloc`, restricted to the sizes CELT actually
    /// needs (`960/1, 960/2, 960/4, 960/8` for the 4 possible short-MDCT counts).
    pub fn new(nfft: usize) -> Self {
        let _ = nfft;
        todo!("wave 1 (celt/CeltSynthesis): opus_fft_alloc / twiddle precomputation")
    }

    /// C: `opus_fft` (forward transform; CELT decode only needs the inverse, but both are
    /// needed for MDCT round-trip unit tests).
    pub fn forward(&self, input: &[Complex], output: &mut [Complex]) {
        let _ = (input, output);
        todo!("wave 1 (celt/CeltSynthesis): opus_fft")
    }

    /// C: `opus_ifft`.
    pub fn inverse(&self, input: &[Complex], output: &mut [Complex]) {
        let _ = (input, output);
        todo!("wave 1 (celt/CeltSynthesis): opus_ifft")
    }
}
