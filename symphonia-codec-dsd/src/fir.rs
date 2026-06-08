// FIR Compensation Filter for CIC Droop Correction
// Provides anti-aliasing and compensates for CIC passband droop

use std::f64::consts::PI;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

/// FIR decimation filter
///
/// This filter compensates for the passband droop of the CIC filter
/// and provides additional anti-aliasing filtering before final decimation.
pub struct FirDecimator {
    /// Filter coefficients
    coefficients: Vec<f32>,
    /// Decimation ratio
    decimation: usize,
    /// Sample history as a *doubled* linear buffer (length `2 * taps`): each
    /// incoming sample is written at `pos` and `pos + taps`, so the most recent
    /// `taps` samples are always available as one contiguous (non-wrapping)
    /// slice — letting the convolution use straight SIMD loads instead of a
    /// per-tap gather.
    buffer: Vec<f32>,
    /// Current position in buffer
    buffer_pos: usize,
    /// Sample counter for decimation
    sample_count: usize,
}

impl FirDecimator {
    /// Create a new FIR decimator
    ///
    /// # Arguments
    /// * `decimation` - Decimation ratio
    /// * `taps` - Number of filter taps (should be odd)
    /// * `cutoff` - Cutoff frequency as fraction of input sample rate (0.0 to 0.5)
    pub fn new(decimation: usize, taps: usize, cutoff: f64) -> Self {
        assert!(decimation > 0, "Decimation must be > 0");
        assert!(taps > 0, "Taps must be > 0");
        assert!(taps % 2 == 1, "Taps should be odd for symmetric filter");
        assert!(cutoff > 0.0 && cutoff < 0.5, "Cutoff must be between 0 and 0.5");

        log::debug!("FirDecimator::new called with decimation={}, taps={}, cutoff={}",
                    decimation, taps, cutoff);

        let coefficients = Self::design_kaiser_lpf(taps, cutoff);

        let fir = FirDecimator {
            coefficients,
            decimation,
            buffer: vec![0.0; taps * 2],
            buffer_pos: 0,
            sample_count: 0,
        };

        log::debug!("FirDecimator created: decimation field = {}", fir.decimation);

        fir
    }

    /// Design a Kaiser window low-pass filter
    ///
    /// Kaiser window provides good control over passband ripple and
    /// stopband attenuation.
    fn design_kaiser_lpf(taps: usize, cutoff: f64) -> Vec<f32> {
        let mut coeffs = Vec::with_capacity(taps);
        let center = (taps - 1) as f64 / 2.0;

        // Kaiser window parameter (beta = 5.0 gives reasonable sidelobes)
        let beta = 5.0;
        let i0_beta = Self::bessel_i0(beta);

        for n in 0..taps {
            let x = n as f64 - center;

            // Sinc function for ideal low-pass filter
            let sinc = if x.abs() < 1e-10 {
                2.0 * PI * cutoff
            }
            else {
                (2.0 * PI * cutoff * x).sin() / x
            };

            // Kaiser window
            let window_arg = 2.0 * n as f64 / (taps - 1) as f64 - 1.0;
            let window = Self::bessel_i0(beta * (1.0 - window_arg * window_arg).sqrt()) / i0_beta;

            coeffs.push((sinc * window) as f32);
        }

        // Normalize coefficients to maintain unity gain at DC
        let sum: f32 = coeffs.iter().sum();
        for coeff in &mut coeffs {
            *coeff /= sum;
        }

        coeffs
    }

    /// Bessel function I0 (zeroth order, first kind)
    /// Used for Kaiser window calculation
    fn bessel_i0(x: f64) -> f64 {
        let mut sum = 1.0;
        let mut term = 1.0;
        let x_half_sq = (x / 2.0).powi(2);

        for k in 1..30 {
            term *= x_half_sq / (k as f64 * k as f64);
            sum += term;
            if term < 1e-10 * sum {
                break;
            }
        }

        sum
    }

    /// Process a single sample
    ///
    /// Returns Some(output) when decimation produces an output,
    /// None otherwise.
    pub fn process(&mut self, input: f32) -> Option<f32> {
        let taps = self.coefficients.len();

        // Doubled-buffer push: write the sample twice so the recent-sample
        // window stays contiguous regardless of position.
        self.buffer[self.buffer_pos] = input;
        self.buffer[self.buffer_pos + taps] = input;
        self.buffer_pos += 1;
        if self.buffer_pos == taps {
            self.buffer_pos = 0;
        }

        // Decimation: only compute output every Rth sample
        self.sample_count += 1;
        if self.sample_count < self.decimation {
            return None;
        }
        self.sample_count = 0;

        // Contiguous window of the last `taps` samples. The Kaiser LPF is
        // symmetric, so dotting in oldest→newest order matches the original
        // reverse-circular convolution.
        let window = &self.buffer[self.buffer_pos..self.buffer_pos + taps];
        let mut output = 0.0f32;
        for (coeff, sample) in self.coefficients.iter().zip(window) {
            output += coeff * sample;
        }

        Some(output)
    }

    /// Process a buffer of samples
    pub fn process_buffer(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        // Use SIMD-optimized version if available
        #[cfg(target_arch = "x86_64")]
        {
            if is_x86_feature_detected!("avx") && is_x86_feature_detected!("fma") {
                // SAFETY: We've verified CPU support for AVX and FMA instructions
                return unsafe { self.process_buffer_simd_avx(input, output) };
            }
        }

        // Fallback to scalar processing
        self.process_buffer_scalar(input, output)
    }

    /// Scalar (non-SIMD) buffer processing
    fn process_buffer_scalar(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let mut out_idx = 0;

        for &sample in input {
            if let Some(out_sample) = self.process(sample) {
                if out_idx < output.len() {
                    output[out_idx] = out_sample;
                    out_idx += 1;
                }
                else {
                    break;
                }
            }
        }

        out_idx
    }

    /// SIMD-optimized buffer processing using AVX
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx,fma")]
    unsafe fn process_buffer_simd_avx(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let mut out_idx = 0;
        let taps = self.coefficients.len();

        for &sample in input {
            // Doubled-buffer push keeps the window contiguous for SIMD.
            self.buffer[self.buffer_pos] = sample;
            self.buffer[self.buffer_pos + taps] = sample;
            self.buffer_pos += 1;
            if self.buffer_pos == taps {
                self.buffer_pos = 0;
            }

            self.sample_count += 1;
            if self.sample_count < self.decimation {
                continue;
            }
            self.sample_count = 0;

            if out_idx >= output.len() {
                break;
            }

            let window = &self.buffer[self.buffer_pos..self.buffer_pos + taps];
            output[out_idx] = Self::dot_avx(&self.coefficients, window);
            out_idx += 1;
        }

        out_idx
    }

    /// Dot product of `coeffs` with a contiguous `window` of equal length,
    /// using AVX + FMA. Both inputs are contiguous, so no gather is needed.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx,fma")]
    unsafe fn dot_avx(coeffs: &[f32], window: &[f32]) -> f32 {
        let n = coeffs.len().min(window.len());
        let mut sum = _mm256_setzero_ps();

        let mut i = 0;
        while i + 8 <= n {
            let c = _mm256_loadu_ps(coeffs.as_ptr().add(i));
            let w = _mm256_loadu_ps(window.as_ptr().add(i));
            sum = _mm256_fmadd_ps(c, w, sum);
            i += 8;
        }

        // Horizontal sum of the 8 accumulated lanes
        let sum_high = _mm256_extractf128_ps(sum, 1);
        let sum_low = _mm256_castps256_ps128(sum);
        let sum128 = _mm_add_ps(sum_low, sum_high);
        let sum64 = _mm_add_ps(sum128, _mm_movehl_ps(sum128, sum128));
        let sum32 = _mm_add_ss(sum64, _mm_shuffle_ps(sum64, sum64, 0x1));
        let mut result = _mm_cvtss_f32(sum32);

        // Scalar tail for the remaining (< 8) coefficients
        while i < n {
            result += coeffs[i] * window[i];
            i += 1;
        }

        result
    }

    /// Reset filter state
    pub fn reset(&mut self) {
        for sample in &mut self.buffer {
            *sample = 0.0;
        }
        self.buffer_pos = 0;
        self.sample_count = 0;
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fir_decimator_creation() {
        let fir = FirDecimator::new(8, 31, 0.4);
        assert_eq!(fir.decimation, 8);
        assert_eq!(fir.coefficients.len(), 31);
    }

    #[test]
    #[should_panic(expected = "Taps should be odd")]
    fn test_fir_even_taps() {
        FirDecimator::new(8, 32, 0.4);
    }

    #[test]
    #[should_panic(expected = "Cutoff must be between 0 and 0.5")]
    fn test_fir_invalid_cutoff() {
        FirDecimator::new(8, 31, 0.6);
    }

    #[test]
    fn test_fir_coefficients_normalized() {
        let fir = FirDecimator::new(8, 31, 0.4);

        // Sum of coefficients should be close to 1.0 (unity gain at DC)
        let sum: f32 = fir.coefficients.iter().sum();
        assert!((sum - 1.0).abs() < 0.001, "Coefficients not normalized: sum = {}", sum);
    }

    #[test]
    fn test_fir_decimation() {
        let mut fir = FirDecimator::new(8, 31, 0.4);

        // First 7 samples should produce no output
        for _ in 0..7 {
            assert_eq!(fir.process(1.0), None);
        }

        // 8th sample should produce output
        assert!(fir.process(1.0).is_some());
    }

    #[test]
    fn test_fir_dc_response() {
        let mut fir = FirDecimator::new(8, 31, 0.4);

        // Feed DC signal (constant 1.0)
        let input = vec![1.0f32; 320];
        let mut output = vec![0.0f32; 40];

        let out_count = fir.process_buffer(&input, &mut output);
        assert_eq!(out_count, 40);

        // After settling (filter delay), output should be close to 1.0
        for &sample in &output[20..] {
            assert!((sample - 1.0).abs() < 0.05, "DC response not unity: {}", sample);
        }
    }

    #[test]
    fn test_fir_reset() {
        let mut fir = FirDecimator::new(8, 31, 0.4);

        // Process some samples
        for _ in 0..50 {
            fir.process(1.0);
        }

        // Reset
        fir.reset();

        // State should be cleared
        assert_eq!(fir.sample_count, 0);
        assert_eq!(fir.buffer_pos, 0);
        for &sample in &fir.buffer {
            assert_eq!(sample, 0.0);
        }
    }

    #[test]
    fn test_bessel_i0() {
        // Test known values of I0(x)
        let i0_0 = FirDecimator::bessel_i0(0.0);
        assert!((i0_0 - 1.0).abs() < 1e-6, "I0(0) should be 1.0");

        let i0_5 = FirDecimator::bessel_i0(5.0);
        // I0(5) ≈ 27.24
        assert!((i0_5 - 27.24).abs() < 0.1, "I0(5) ≈ 27.24, got {}", i0_5);
    }

    #[test]
    fn test_fir_filter_symmetry() {
        let fir = FirDecimator::new(8, 31, 0.4);

        // Coefficients should be symmetric (linear phase filter)
        let len = fir.coefficients.len();
        for i in 0..len / 2 {
            let diff = (fir.coefficients[i] - fir.coefficients[len - 1 - i]).abs();
            assert!(diff < 1e-6, "Coefficients not symmetric at index {}", i);
        }
    }
}
