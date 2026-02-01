// SILK Resampler
//
// Resamples SILK output from its native sample rate (8/12/16/24 kHz) to 48 kHz
// as required by the Opus specification.
//
// Uses linear interpolation for simplicity. For production quality,
// a polyphase filter would be better.

use crate::toc::Bandwidth;

pub struct Resampler {
    ratio: usize,
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

        Self { ratio }
    }

    /// Resample input from native SILK sample rate to 48kHz
    pub fn resample(&self, input: &[f32]) -> Vec<f32> {
        if self.ratio == 1 {
            // No resampling needed for FullBand
            return input.to_vec();
        }

        let output_len = input.len() * self.ratio;
        let mut output = vec![0.0f32; output_len];

        // Linear interpolation
        for i in 0..input.len() - 1 {
            let sample_start = input[i];
            let sample_end = input[i + 1];

            for r in 0..self.ratio {
                let t = r as f32 / self.ratio as f32;
                let interpolated = sample_start + t * (sample_end - sample_start);
                output[i * self.ratio + r] = interpolated;
            }
        }

        // Handle last sample (hold)
        if !input.is_empty() {
            let last_sample = input[input.len() - 1];
            for r in 0..self.ratio {
                output[(input.len() - 1) * self.ratio + r] = last_sample;
            }
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
        assert_eq!(resampler.output_sample_count(160), 960); // 20ms at 8kHz -> 48kHz

        let resampler = Resampler::new(Bandwidth::MediumBand);
        assert_eq!(resampler.output_sample_count(240), 960); // 20ms at 12kHz -> 48kHz
    }

    #[test]
    fn test_linear_interpolation() {
        let resampler = Resampler::new(Bandwidth::SuperWideBand); // 2x upsampling
        let input = vec![0.0, 1.0, 0.0];
        let output = resampler.resample(&input);

        assert_eq!(output.len(), 6);
        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.5); // Interpolated
        assert_eq!(output[2], 1.0);
        assert_eq!(output[3], 0.5); // Interpolated
    }
}
