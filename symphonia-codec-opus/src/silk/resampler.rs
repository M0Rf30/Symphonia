// SILK Resampler
//
// Resamples SILK output from its native sample rate (8/12/16/24 kHz) to 48 kHz
// as required by the Opus specification.
//
// Uses a polyphase FIR filter with windowed-sinc design for high-quality resampling.

use crate::toc::Bandwidth;

/// Polyphase resampler for SILK output
pub struct Resampler {
    ratio: usize,
    coefficients: Vec<f32>,
    taps_per_phase: usize,
}

impl Resampler {
    pub fn new(bandwidth: Bandwidth) -> Self {
        let ratio = match bandwidth {
            Bandwidth::NarrowBand => 6,    // 8kHz -> 48kHz
            Bandwidth::MediumBand => 4,    // 12kHz -> 48kHz
            Bandwidth::WideBand => 3,      // 16kHz -> 48kHz
            Bandwidth::SuperWideBand => 2, // 24kHz -> 48kHz
            Bandwidth::FullBand => 1,      // 48kHz -> 48kHz (no resampling)
        };

        // Design polyphase filter coefficients
        let (coefficients, taps_per_phase) = if ratio > 1 {
            Self::design_filter(ratio)
        } else {
            (Vec::new(), 0)
        };

        Self { ratio, coefficients, taps_per_phase }
    }

    /// Design windowed-sinc FIR filter for polyphase resampling
    fn design_filter(ratio: usize) -> (Vec<f32>, usize) {
        // Filter parameters
        let taps_per_phase = 16; // 16 taps per polyphase branch
        let total_taps = taps_per_phase * ratio;
        let cutoff = 0.475 / ratio as f32; // Anti-aliasing cutoff
        let mut coeffs = vec![0.0f32; total_taps];

        // Generate windowed-sinc filter
        for i in 0..total_taps {
            let x = i as f32 - (total_taps as f32 - 1.0) / 2.0;

            // Sinc function
            let sinc = if x.abs() < 1e-6 {
                1.0
            } else {
                let arg = std::f32::consts::PI * x / ratio as f32;
                arg.sin() / arg
            };

            // Kaiser window (beta = 8.0 for good stopband attenuation)
            let window = Self::kaiser_window(i, total_taps, 8.0);

            // Low-pass filter response
            let h = 2.0 * cutoff * sinc * window;
            coeffs[i] = h;
        }

        // Normalize so DC gain = ratio
        let sum: f32 = coeffs.iter().sum();
        let scale = ratio as f32 / sum;
        for coeff in &mut coeffs {
            *coeff *= scale;
        }

        (coeffs, taps_per_phase)
    }

    /// Kaiser window function
    fn kaiser_window(n: usize, length: usize, beta: f32) -> f32 {
        let alpha = (length - 1) as f32 / 2.0;
        let x = (n as f32 - alpha) / alpha;
        Self::bessel_i0(beta * (1.0 - x * x).sqrt()) / Self::bessel_i0(beta)
    }

    /// Modified Bessel function of the first kind, order 0
    fn bessel_i0(x: f32) -> f32 {
        let mut sum = 1.0;
        let mut term = 1.0;
        let mut k = 1.0;

        // Series expansion (sufficient accuracy with ~20 terms)
        for _ in 0..20 {
            term *= (x / (2.0 * k)).powi(2);
            sum += term;
            k += 1.0;
            if term < 1e-8 {
                break;
            }
        }

        sum
    }

    /// Resample input from native SILK sample rate to 48kHz using polyphase filtering
    pub fn resample(&self, input: &[f32]) -> Vec<f32> {
        if self.ratio == 1 {
            return input.to_vec();
        }

        let output_len = input.len() * self.ratio;
        let mut output = vec![0.0f32; output_len];

        // Polyphase filtering
        for out_idx in 0..output_len {
            let phase = out_idx % self.ratio;
            let in_idx_base = out_idx / self.ratio;

            let mut acc = 0.0f32;

            // Convolve with the appropriate polyphase filter
            for tap in 0..self.taps_per_phase {
                let coeff_idx = phase + tap * self.ratio;
                let in_idx = in_idx_base as isize - tap as isize + self.taps_per_phase as isize / 2;

                if in_idx >= 0 && (in_idx as usize) < input.len() {
                    acc += input[in_idx as usize] * self.coefficients[coeff_idx];
                }
            }

            output[out_idx] = acc;
        }

        output
    }

    /// Calculate output sample count after resampling
    pub fn output_sample_count(&self, input_samples: usize) -> usize {
        input_samples * self.ratio
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resampler_ratios() {
        let nb_resampler = Resampler::new(Bandwidth::NarrowBand);
        assert_eq!(nb_resampler.ratio, 6);

        let mb_resampler = Resampler::new(Bandwidth::MediumBand);
        assert_eq!(mb_resampler.ratio, 4);

        let wb_resampler = Resampler::new(Bandwidth::WideBand);
        assert_eq!(wb_resampler.ratio, 3);

        let swb_resampler = Resampler::new(Bandwidth::SuperWideBand);
        assert_eq!(swb_resampler.ratio, 2);

        let fb_resampler = Resampler::new(Bandwidth::FullBand);
        assert_eq!(fb_resampler.ratio, 1);
    }

    #[test]
    fn test_resampler_output_count() {
        let resampler = Resampler::new(Bandwidth::NarrowBand);
        assert_eq!(resampler.output_sample_count(160), 960);

        let resampler = Resampler::new(Bandwidth::MediumBand);
        assert_eq!(resampler.output_sample_count(240), 960);
    }

    #[test]
    fn test_polyphase_filter_design() {
        let resampler = Resampler::new(Bandwidth::SuperWideBand);

        // Check filter was created
        assert!(!resampler.coefficients.is_empty());
        assert_eq!(resampler.taps_per_phase, 16);
        assert_eq!(resampler.coefficients.len(), 16 * 2); // 16 taps * 2 phases
    }

    #[test]
    fn test_dc_gain() {
        let resampler = Resampler::new(Bandwidth::SuperWideBand);

        // Test DC response (constant input should give constant output scaled by ratio)
        let input = vec![1.0f32; 100];
        let output = resampler.resample(&input);

        // Check output length
        assert_eq!(output.len(), 200);

        // DC gain should be approximately 1.0 (after settling)
        let avg = output[20..180].iter().sum::<f32>() / 160.0;
        assert!((avg - 1.0).abs() < 0.1, "DC gain test failed: avg = {}", avg);
    }

    #[test]
    fn test_impulse_response() {
        let resampler = Resampler::new(Bandwidth::WideBand);

        // Test impulse response
        let mut input = vec![0.0f32; 50];
        input[25] = 1.0;

        let output = resampler.resample(&input);

        // Output should have a peak around the impulse location
        let peak_idx = output.iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .map(|(idx, _)| idx)
            .unwrap();

        // Peak should be near the expected location (25 * 3 = 75)
        assert!((peak_idx as isize - 75).abs() < 10,
                "Peak at {} instead of near 75", peak_idx);
    }
}
