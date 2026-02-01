// MDCT Synthesis - Rewritten from xiph/opus celt/mdct.c
// Copyright (c) 2007-2008 CSIRO
// Copyright (c) 2007-2008 Xiph.Org Foundation
// SPDX-License-Identifier: BSD-3-Clause

use rustfft::{FftPlanner, num_complex::Complex};
use std::f32::consts::PI;

/// MDCT state for inverse transform
pub struct MdctContext {
    n: usize,
    n2: usize,
    n4: usize,
    fft: std::sync::Arc<dyn rustfft::Fft<f32>>,
    twiddle: Vec<f32>,
}

impl MdctContext {
    /// Create new MDCT context for given size
    ///
    /// Arguments:
    /// - n: MDCT size (must be power of 2, typically 120, 240, 480, or 960)
    pub fn new(n: usize) -> Self {
        let n2 = n / 2;
        let n4 = n / 4;

        // Create FFT planner and plan for N/4 size
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_inverse(n4);

        // Pre-compute twiddle factors
        let mut twiddle = Vec::with_capacity(n2);
        for i in 0..n2 {
            let angle = 2.0 * PI * (i as f32 + 0.125) / (n as f32);
            twiddle.push(angle.cos());
        }

        Self {
            n,
            n2,
            n4,
            fft,
            twiddle,
        }
    }

    /// Perform inverse MDCT with overlap-add
    ///
    /// Converts frequency-domain coefficients to time-domain samples.
    /// Applies windowing and overlap-adds with previous frame.
    ///
    /// Arguments:
    /// - input: Frequency domain coefficients (length N/2)
    /// - output: Time domain output (length N)
    /// - window: Window function (length N)
    /// - overlap: Overlap buffer from previous frame (length N/2)
    pub fn imdct(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        window: &[f32],
        overlap: &mut [f32],
    ) {
        debug_assert_eq!(input.len(), self.n2);
        debug_assert_eq!(output.len(), self.n);
        debug_assert_eq!(window.len(), self.n);
        debug_assert_eq!(overlap.len(), self.n2);

        // Allocate working buffers
        let mut f = vec![Complex::new(0.0, 0.0); self.n4];
        let mut z = vec![0.0f32; self.n];

        // Pre-rotation and FFT input preparation
        for i in 0..self.n4 {
            let re_idx = 2 * i;
            let im_idx = self.n2 - 2 * i - 1;

            let re = input[re_idx];
            let im = -input[im_idx];

            let c = self.twiddle[2 * i];
            let s = self.twiddle[2 * i + 1];

            f[i] = Complex::new(
                re * c + im * s,
                -re * s + im * c,
            );
        }

        // Perform inverse FFT
        self.fft.process(&mut f);

        // Post-rotation and output generation
        for i in 0..self.n4 {
            let c = self.twiddle[2 * i];
            let s = self.twiddle[2 * i + 1];

            let re = f[i].re;
            let im = f[i].im;

            z[2 * i] = re * c + im * s;
            z[self.n2 - 2 * i - 1] = re * s - im * c;
        }

        // Mirror for second half
        for i in 0..self.n2 {
            z[self.n2 + i] = -z[self.n2 - i - 1];
        }

        // Apply window and overlap-add
        // First half: overlap-add with previous frame
        for i in 0..self.n2 {
            output[i] = z[i] * window[i] + overlap[i];
        }

        // Second half: save for next frame's overlap
        for i in 0..self.n2 {
            overlap[i] = z[self.n2 + i] * window[self.n2 + i];
        }
    }
}

/// Generate sine window for MDCT
///
/// Creates a sine window: w[i] = sin(π * (i + 0.5) / N)
pub fn sine_window(n: usize) -> Vec<f32> {
    let mut window = Vec::with_capacity(n);
    for i in 0..n {
        let angle = PI * (i as f32 + 0.5) / (n as f32);
        window.push(angle.sin());
    }
    window
}

/// Generate Vorbis window for MDCT
///
/// Creates a Vorbis/Opus window: w[i] = sin(π/2 * sin²(π * (i + 0.5) / N))
pub fn vorbis_window(n: usize) -> Vec<f32> {
    let mut window = Vec::with_capacity(n);
    for i in 0..n {
        let angle = PI * (i as f32 + 0.5) / (n as f32);
        let sin_val = angle.sin();
        window.push((PI / 2.0 * sin_val * sin_val).sin());
    }
    window
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mdct_context_creation() {
        let mdct = MdctContext::new(480);
        assert_eq!(mdct.n, 480);
        assert_eq!(mdct.n2, 240);
        assert_eq!(mdct.n4, 120);
        assert_eq!(mdct.twiddle.len(), 240);
    }

    #[test]
    fn test_sine_window() {
        let window = sine_window(480);
        assert_eq!(window.len(), 480);

        // Check symmetry: w[i] = w[N-1-i]
        for i in 0..240 {
            assert!((window[i] - window[479 - i]).abs() < 1e-6);
        }

        // Check bounds
        for &w in &window {
            assert!(w >= 0.0 && w <= 1.0);
        }
    }

    #[test]
    fn test_vorbis_window() {
        let window = vorbis_window(480);
        assert_eq!(window.len(), 480);

        // Check symmetry
        for i in 0..240 {
            assert!((window[i] - window[479 - i]).abs() < 1e-6);
        }

        // Check bounds
        for &w in &window {
            assert!(w >= 0.0 && w <= 1.0);
        }
    }

    #[test]
    fn test_imdct_dimensions() {
        let mut mdct = MdctContext::new(480);
        let input = vec![0.0; 240];
        let mut output = vec![0.0; 480];
        let window = sine_window(480);
        let mut overlap = vec![0.0; 240];

        mdct.imdct(&input, &mut output, &window, &mut overlap);

        // Should not panic and produce finite values
        for &val in &output {
            assert!(val.is_finite());
        }
    }
}
