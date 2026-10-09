// FIR Compensation Filter for CIC Droop Correction
// Provides anti-aliasing and compensates for CIC passband droop

use std::f64::consts::PI;

/// Number of independent accumulators (lanes) in the dot product.
const LANES: usize = 16;

/// FIR decimation filter
///
/// This filter compensates for the passband droop of the CIC filter
/// and provides additional anti-aliasing filtering before final decimation.
pub struct FirDecimator {
    /// Filter coefficients
    coefficients: Vec<f32>,
    /// Filter coefficients zero-padded to a multiple of `LANES`.
    padded: Vec<f32>,
    /// Decimation ratio
    decimation: usize,
    /// The `taps - 1` most recent input samples (oldest first) preceding the next input.
    history: Vec<f32>,
    /// Scratch buffer holding `history`, the current input and zero padding, so that every
    /// output window is one contiguous slice.
    scratch: Vec<f32>,
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

        log::debug!(
            "FirDecimator::new called with decimation={}, taps={}, cutoff={}",
            decimation,
            taps,
            cutoff
        );

        let coefficients = Self::design_kaiser_lpf(taps, cutoff);

        let mut padded = coefficients.clone();
        padded.resize(taps.next_multiple_of(LANES), 0.0);

        let fir = FirDecimator {
            coefficients,
            decimation,
            history: vec![0.0; taps - 1],
            scratch: Vec::new(),
            padded,
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
            let sinc =
                if x.abs() < 1e-10 { 2.0 * PI * cutoff } else { (2.0 * PI * cutoff * x).sin() / x };

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
        let mut output = [0.0];
        (self.process_buffer(&[input], &mut output) == 1).then_some(output[0])
    }

    /// Process a buffer of samples
    ///
    /// Returns the number of output samples written. If `output` is too small for all the outputs
    /// of `input`, only the input up to the last output that fits is consumed.
    pub fn process_buffer(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let taps = self.coefficients.len();
        let decimation = self.decimation;

        // Input samples until the next output is due, and the number of outputs due.
        let first = decimation - self.sample_count;
        let due = if input.len() < first { 0 } else { (input.len() - first) / decimation + 1 };
        let count = due.min(output.len());

        let consumed = match count {
            0 if due > 0 => 0,
            n if n == due => input.len(),
            n => first + (n - 1) * decimation,
        };
        let input = &input[..consumed];

        // Lay out the history followed by the input contiguously: the window of each output is
        // then a plain slice, with no wrap-around handling and no per-sample bookkeeping. The
        // zero padding extends the last window to a multiple of the lane count.
        self.scratch.clear();
        self.scratch.extend_from_slice(&self.history);
        self.scratch.extend_from_slice(input);
        let end = self.scratch.len();
        self.scratch.resize(end + self.padded.len() - taps, 0.0);

        // The newest sample of output `j` is input sample `first - 1 + j * decimation`, which is
        // the last sample of the window starting at that same index of `scratch`. The Kaiser LPF
        // is symmetric, so dotting in oldest→newest order matches the original reverse-circular
        // convolution.
        let padded_len = self.padded.len();
        for (j, out) in output[..count].iter_mut().enumerate() {
            let start = (first - 1) + j * decimation;
            *out = dot(&self.padded, &self.scratch[start..start + padded_len]);
        }

        self.history.copy_from_slice(&self.scratch[end - (taps - 1)..end]);
        self.sample_count = (self.sample_count + consumed) % decimation;

        count
    }

    /// Reset filter state
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.sample_count = 0;
    }
}

/// Dot product of `coeffs` with a window of the same length, which must be a multiple of
/// `LANES`.
///
/// The sum is formed in `LANES` independent partial sums in a fixed order, which breaks the serial
/// dependency of a plain `acc += c * w` loop and lets the compiler emit SIMD (SSE, AVX or NEON,
/// depending on the target) without any `unsafe` code or `-ffast-math` reassociation. Products
/// and sums are not fused, so the result is identical on every target.
#[inline(always)]
fn dot(coeffs: &[f32], window: &[f32]) -> f32 {
    debug_assert_eq!(coeffs.len() % LANES, 0);
    debug_assert_eq!(coeffs.len(), window.len());

    let mut acc = [0.0f32; LANES];

    for (c, w) in coeffs.chunks_exact(LANES).zip(window.chunks_exact(LANES)) {
        for lane in 0..LANES {
            acc[lane] += c[lane] * w[lane];
        }
    }

    // Reduce the lanes in a fixed tree: halves, quarters, eighths, then the final pair.
    let mut half = [0.0f32; LANES / 2];
    for i in 0..LANES / 2 {
        half[i] = acc[i] + acc[i + LANES / 2];
    }
    let quarter = [half[0] + half[4], half[1] + half[5], half[2] + half[6], half[3] + half[7]];
    (quarter[0] + quarter[2]) + (quarter[1] + quarter[3])
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
        assert!(fir.history.iter().all(|&sample| sample == 0.0));
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

    /// Deterministic pseudo-random samples in [-1, 1).
    fn test_signal(len: usize, seed: u32) -> Vec<f32> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 8) as f32 / (1u32 << 23) as f32 - 1.0
            })
            .collect()
    }

    #[test]
    fn test_fir_matches_f64_convolution() {
        for &(decimation, taps) in &[(2usize, 63usize), (4, 63), (8, 31), (2, 31), (3, 7)] {
            let mut fir = FirDecimator::new(decimation, taps, 0.4 / decimation as f64);
            let coefficients = fir.coefficients.clone();
            let input = test_signal(1000, 7 + taps as u32);

            let mut output = vec![0.0; 1000 / decimation];
            let n = fir.process_buffer(&input, &mut output);
            assert_eq!(n, 1000 / decimation);

            for (m, &got) in output.iter().enumerate() {
                // The newest sample of output m is input[(m + 1) * decimation - 1].
                let newest = (m + 1) * decimation - 1;
                let expected: f64 = coefficients
                    .iter()
                    .enumerate()
                    .map(|(k, &c)| {
                        // Coefficient k applies to the sample taps - 1 - k steps before the newest.
                        let age = taps - 1 - k;
                        newest.checked_sub(age).map_or(0.0, |i| f64::from(c) * f64::from(input[i]))
                    })
                    .sum();
                assert!(
                    (f64::from(got) - expected).abs() < 1e-5,
                    "output {m}: {got} vs {expected}"
                );
            }
        }
    }

    #[test]
    fn test_fir_buffer_and_sample_paths_agree() {
        let input = test_signal(777, 99);

        let mut a = FirDecimator::new(2, 63, 0.2);
        let expected: Vec<f32> = input.iter().filter_map(|&s| a.process(s)).collect();

        // Odd chunk sizes exercise partial decimation blocks across calls.
        let mut b = FirDecimator::new(2, 63, 0.2);
        let mut got = Vec::new();
        for chunk in input.chunks(13) {
            let mut out = vec![0.0; chunk.len()];
            let n = b.process_buffer(chunk, &mut out);
            got.extend_from_slice(&out[..n]);
        }

        assert_eq!(got.len(), expected.len());
        assert!(got.iter().zip(&expected).all(|(x, y)| x.to_bits() == y.to_bits()));
    }

    #[test]
    fn test_fir_small_output_buffer() {
        let input = test_signal(100, 5);

        let mut full = FirDecimator::new(4, 31, 0.1);
        let mut expected = vec![0.0; 25];
        assert_eq!(full.process_buffer(&input, &mut expected), 25);

        // With room for only 10 outputs, the input is consumed up to the 10th output; feeding the
        // rest afterwards continues seamlessly.
        let mut split = FirDecimator::new(4, 31, 0.1);
        let mut got = vec![0.0; 10];
        assert_eq!(split.process_buffer(&input, &mut got), 10);
        let mut rest = vec![0.0; 15];
        assert_eq!(split.process_buffer(&input[40..], &mut rest), 15);
        got.extend_from_slice(&rest);

        assert!(got.iter().zip(&expected).all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    #[test]
    fn test_dot_lane_order() {
        // Pin the (target independent) summation order with values that expose reassociation:
        // lane 0 sees 1e8 then -1e8, the other lanes see small values.
        let mut coeffs = vec![0.0f32; 2 * LANES];
        let mut window = vec![0.0f32; 2 * LANES];
        coeffs[0] = 1.0;
        window[0] = 1.0e8;
        coeffs[LANES] = 1.0;
        window[LANES] = -1.0e8;
        coeffs[1] = 1.0;
        window[1] = 3.0;
        // Lane 0: (1e8 + -1e8) = 0 exactly, then 3.0 from lane 1 survives.
        assert_eq!(dot(&coeffs, &window), 3.0);

        // A serial sum would give 0.0 as 1e8 + 3.0 rounds to 1e8 + 4.0 first.
        let serial: f32 = coeffs.iter().zip(&window).fold(0.0, |a, (c, w)| a + c * w);
        assert_ne!(serial, 3.0);
    }
}
