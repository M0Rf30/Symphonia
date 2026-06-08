// CIC (Cascaded Integrator-Comb) Decimation Filter
// Efficient multi-stage decimation for DSD-to-PCM conversion

/// CIC filter for decimation
///
/// A CIC filter consists of integrator stages followed by decimation
/// and then comb stages. It's very efficient for high decimation ratios
/// as it requires no multiplications.
///
/// # Theory
/// CIC filters accumulate (integrate) input samples, then decimate by R,
/// then differentiate (comb) the result. Multiple stages increase the
/// filtering order, improving stopband attenuation.
///
/// # Gain Compensation
/// This implementation assumes a differential delay (M) of 1. The CIC filter
/// gain is calculated as `R^N` where:
/// - R = decimation ratio
/// - N = number of stages
///
/// For the general case with M ≠ 1, the gain would be `(RM)^N`.
/// See: [Intel AN455: Understanding CIC Compensation Filters](https://cdrdv2-public.intel.com/653906/an455.pdf)
///
/// The frequency response is: H(f) = [sin(πRMf)/sin(πf)]^N
#[cfg_attr(test, allow(dead_code))]
pub struct CicFilter {
    /// Decimation ratio
    decimation: usize,
    /// Number of stages (typically 3-5)
    stages: usize,
    /// Integrator state (one per stage)
    integrators: Vec<i64>,
    /// Comb state (one per stage, each holds previous value)
    combs: Vec<i64>,
    /// Sample counter for decimation
    sample_count: usize,
}

impl CicFilter {
    /// Create a new CIC filter
    ///
    /// # Arguments
    /// * `decimation` - Decimation ratio (must be > 0)
    /// * `stages` - Number of stages (typically 3-5)
    pub fn new(decimation: usize, stages: usize) -> Self {
        assert!(decimation > 0, "Decimation must be > 0");
        assert!(stages > 0, "Stages must be > 0");
        assert!(stages <= 8, "Too many stages");

        CicFilter {
            decimation,
            stages,
            integrators: vec![0; stages],
            combs: vec![0; stages],
            sample_count: 0,
        }
    }

    /// Process a single sample
    ///
    /// Returns Some(output) when decimation produces an output sample,
    /// None otherwise (most of the time).
    ///
    /// # Arguments
    /// * `input` - Input sample value
    pub fn process(&mut self, input: f32) -> Option<f32> {
        // Convert to i64 for accumulation without overflow
        let mut value = (input * 32768.0) as i64;

        // Integrator stages
        for integrator in &mut self.integrators {
            value = value.wrapping_add(*integrator);
            *integrator = value;
        }

        // Decimation: only output every Rth sample
        self.sample_count += 1;
        if self.sample_count < self.decimation {
            return None;
        }
        self.sample_count = 0;

        // Comb stages (differentiate)
        for comb_state in &mut self.combs {
            let prev = *comb_state;
            *comb_state = value;
            value = value.wrapping_sub(prev);
        }

        // Scale back to f32 with appropriate gain compensation
        // CIC gain = (decimation)^stages, so we divide by that
        let gain = (self.decimation as f64).powi(self.stages as i32);
        let output = (value as f64 / gain / 32768.0) as f32;

        Some(output)
    }

    /// Process a buffer of samples, collecting output samples
    ///
    /// # Arguments
    /// * `input` - Input buffer
    /// * `output` - Output buffer (will be filled with decimated samples)
    ///
    /// # Returns
    /// Number of output samples produced
    pub fn process_buffer(&mut self, input: &[f32], output: &mut [f32]) -> usize {
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

    /// Reset filter state
    pub fn reset(&mut self) {
        for integrator in &mut self.integrators {
            *integrator = 0;
        }
        for comb in &mut self.combs {
            *comb = 0;
        }
        self.sample_count = 0;
    }

    /// Get the expected output size for a given input size
    pub fn output_size_for_input(&self, input_len: usize) -> usize {
        // Account for partial decimation from previous calls
        let total_samples = self.sample_count + input_len;
        total_samples / self.decimation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cic_filter_creation() {
        let filter = CicFilter::new(8, 3);
        assert_eq!(filter.decimation, 8);
        assert_eq!(filter.stages, 3);
    }

    #[test]
    #[should_panic(expected = "Decimation must be > 0")]
    fn test_cic_filter_zero_decimation() {
        CicFilter::new(0, 3);
    }

    #[test]
    #[should_panic(expected = "Stages must be > 0")]
    fn test_cic_filter_zero_stages() {
        CicFilter::new(8, 0);
    }

    #[test]
    fn test_cic_filter_decimation() {
        let mut filter = CicFilter::new(8, 3);

        // First 7 samples should produce no output
        for _ in 0..7 {
            assert_eq!(filter.process(1.0), None);
        }

        // 8th sample should produce output
        let result = filter.process(1.0);
        assert!(result.is_some());
    }

    #[test]
    fn test_cic_filter_dc_response() {
        let mut filter = CicFilter::new(8, 3);

        // Feed DC signal (constant 1.0)
        let input = vec![1.0f32; 80];
        let mut output = vec![0.0f32; 10];

        let out_count = filter.process_buffer(&input, &mut output);
        assert_eq!(out_count, 10);

        // After settling, output should be close to 1.0 for DC input
        // (allowing for some transient at the start)
        for &sample in &output[5..] {
            assert!((sample - 1.0).abs() < 0.1, "Sample {} not close to 1.0", sample);
        }
    }

    #[test]
    fn test_cic_filter_reset() {
        let mut filter = CicFilter::new(8, 3);

        // Process some samples
        for _ in 0..10 {
            filter.process(1.0);
        }

        // Reset
        filter.reset();

        // State should be cleared
        assert_eq!(filter.sample_count, 0);
        for &integrator in &filter.integrators {
            assert_eq!(integrator, 0);
        }
        for &comb in &filter.combs {
            assert_eq!(comb, 0);
        }
    }

    #[test]
    fn test_output_size_calculation() {
        let filter = CicFilter::new(8, 3);
        assert_eq!(filter.output_size_for_input(80), 10);
        assert_eq!(filter.output_size_for_input(16), 2);
        assert_eq!(filter.output_size_for_input(7), 0);
    }

    #[test]
    fn test_cic_filter_alternating_signal() {
        let mut filter = CicFilter::new(8, 3);

        // Feed alternating signal (DSD silence pattern)
        let mut input = Vec::new();
        for i in 0..160 {
            input.push(if i % 2 == 0 { 1.0 } else { -1.0 });
        }

        let mut output = vec![0.0f32; 20];
        let out_count = filter.process_buffer(&input, &mut output);
        assert_eq!(out_count, 20);

        // Alternating signal at Nyquist should be heavily attenuated by CIC
        for &sample in &output[5..] {
            assert!(sample.abs() < 0.5, "High frequency not attenuated: {}", sample);
        }
    }
}
