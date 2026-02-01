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
    #[inline]
    pub fn resample(&self, input: &[f32]) -> Vec<f32> {
        if self.ratio == 1 {
            return input.to_vec();
        }

        let output_len = input.len() * self.ratio;
        let mut output = vec![0.0f32; output_len];

        // Use specialized implementations for common ratios
        match self.ratio {
            2 => self.resample_2x(input, &mut output),
            3 => self.resample_3x(input, &mut output),
            4 => self.resample_4x(input, &mut output),
            6 => self.resample_6x(input, &mut output),
            _ => self.resample_generic(input, &mut output),
        }

        output
    }

    #[inline(always)]
    fn resample_generic(&self, input: &[f32], output: &mut [f32]) {
        let half_taps = self.taps_per_phase / 2;

        for out_idx in 0..output.len() {
            let phase = out_idx % self.ratio;
            let in_idx_base = out_idx / self.ratio;

            let mut acc = 0.0f32;

            // Manual loop unrolling for better performance
            let mut tap = 0;
            while tap + 4 <= self.taps_per_phase {
                let in_idx0 = in_idx_base as isize - tap as isize + half_taps as isize;
                let in_idx1 = in_idx0 - 1;
                let in_idx2 = in_idx0 - 2;
                let in_idx3 = in_idx0 - 3;

                if in_idx0 >= 0 && in_idx0 < input.len() as isize {
                    acc += unsafe { *input.get_unchecked(in_idx0 as usize) }
                         * unsafe { *self.coefficients.get_unchecked(phase + tap * self.ratio) };
                }
                if in_idx1 >= 0 && in_idx1 < input.len() as isize {
                    acc += unsafe { *input.get_unchecked(in_idx1 as usize) }
                         * unsafe { *self.coefficients.get_unchecked(phase + (tap + 1) * self.ratio) };
                }
                if in_idx2 >= 0 && in_idx2 < input.len() as isize {
                    acc += unsafe { *input.get_unchecked(in_idx2 as usize) }
                         * unsafe { *self.coefficients.get_unchecked(phase + (tap + 2) * self.ratio) };
                }
                if in_idx3 >= 0 && in_idx3 < input.len() as isize {
                    acc += unsafe { *input.get_unchecked(in_idx3 as usize) }
                         * unsafe { *self.coefficients.get_unchecked(phase + (tap + 3) * self.ratio) };
                }

                tap += 4;
            }

            // Handle remaining taps
            while tap < self.taps_per_phase {
                let in_idx = in_idx_base as isize - tap as isize + half_taps as isize;
                if in_idx >= 0 && in_idx < input.len() as isize {
                    acc += unsafe { *input.get_unchecked(in_idx as usize) }
                         * unsafe { *self.coefficients.get_unchecked(phase + tap * self.ratio) };
                }
                tap += 1;
            }

            unsafe { *output.get_unchecked_mut(out_idx) = acc; }
        }
    }

    // Optimized 2x upsampling
    #[inline]
    fn resample_2x(&self, input: &[f32], output: &mut [f32]) {
        let half_taps = self.taps_per_phase / 2;

        for i in 0..input.len() {
            // Even samples (phase 0)
            let mut acc0 = 0.0f32;
            // Odd samples (phase 1)
            let mut acc1 = 0.0f32;

            for tap in 0..self.taps_per_phase {
                let in_idx = i as isize - tap as isize + half_taps as isize;
                if in_idx >= 0 && in_idx < input.len() as isize {
                    let sample = unsafe { *input.get_unchecked(in_idx as usize) };
                    acc0 += sample * unsafe { *self.coefficients.get_unchecked(tap * 2) };
                    acc1 += sample * unsafe { *self.coefficients.get_unchecked(1 + tap * 2) };
                }
            }

            unsafe {
                *output.get_unchecked_mut(i * 2) = acc0;
                *output.get_unchecked_mut(i * 2 + 1) = acc1;
            }
        }
    }

    // Optimized 3x upsampling
    #[inline]
    fn resample_3x(&self, input: &[f32], output: &mut [f32]) {
        let half_taps = self.taps_per_phase / 2;

        for i in 0..input.len() {
            let mut acc0 = 0.0f32;
            let mut acc1 = 0.0f32;
            let mut acc2 = 0.0f32;

            for tap in 0..self.taps_per_phase {
                let in_idx = i as isize - tap as isize + half_taps as isize;
                if in_idx >= 0 && in_idx < input.len() as isize {
                    let sample = unsafe { *input.get_unchecked(in_idx as usize) };
                    acc0 += sample * unsafe { *self.coefficients.get_unchecked(tap * 3) };
                    acc1 += sample * unsafe { *self.coefficients.get_unchecked(1 + tap * 3) };
                    acc2 += sample * unsafe { *self.coefficients.get_unchecked(2 + tap * 3) };
                }
            }

            unsafe {
                *output.get_unchecked_mut(i * 3) = acc0;
                *output.get_unchecked_mut(i * 3 + 1) = acc1;
                *output.get_unchecked_mut(i * 3 + 2) = acc2;
            }
        }
    }

    // Optimized 4x upsampling
    #[inline]
    fn resample_4x(&self, input: &[f32], output: &mut [f32]) {
        let half_taps = self.taps_per_phase / 2;

        for i in 0..input.len() {
            let mut acc0 = 0.0f32;
            let mut acc1 = 0.0f32;
            let mut acc2 = 0.0f32;
            let mut acc3 = 0.0f32;

            for tap in 0..self.taps_per_phase {
                let in_idx = i as isize - tap as isize + half_taps as isize;
                if in_idx >= 0 && in_idx < input.len() as isize {
                    let sample = unsafe { *input.get_unchecked(in_idx as usize) };
                    acc0 += sample * unsafe { *self.coefficients.get_unchecked(tap * 4) };
                    acc1 += sample * unsafe { *self.coefficients.get_unchecked(1 + tap * 4) };
                    acc2 += sample * unsafe { *self.coefficients.get_unchecked(2 + tap * 4) };
                    acc3 += sample * unsafe { *self.coefficients.get_unchecked(3 + tap * 4) };
                }
            }

            unsafe {
                *output.get_unchecked_mut(i * 4) = acc0;
                *output.get_unchecked_mut(i * 4 + 1) = acc1;
                *output.get_unchecked_mut(i * 4 + 2) = acc2;
                *output.get_unchecked_mut(i * 4 + 3) = acc3;
            }
        }
    }

    // Optimized 6x upsampling
    #[inline]
    fn resample_6x(&self, input: &[f32], output: &mut [f32]) {
        let half_taps = self.taps_per_phase / 2;

        for i in 0..input.len() {
            let mut acc = [0.0f32; 6];

            for tap in 0..self.taps_per_phase {
                let in_idx = i as isize - tap as isize + half_taps as isize;
                if in_idx >= 0 && in_idx < input.len() as isize {
                    let sample = unsafe { *input.get_unchecked(in_idx as usize) };
                    for phase in 0..6 {
                        acc[phase] += sample * unsafe { *self.coefficients.get_unchecked(phase + tap * 6) };
                    }
                }
            }

            unsafe {
                let out_base = i * 6;
                *output.get_unchecked_mut(out_base) = acc[0];
                *output.get_unchecked_mut(out_base + 1) = acc[1];
                *output.get_unchecked_mut(out_base + 2) = acc[2];
                *output.get_unchecked_mut(out_base + 3) = acc[3];
                *output.get_unchecked_mut(out_base + 4) = acc[4];
                *output.get_unchecked_mut(out_base + 5) = acc[5];
            }
        }
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
