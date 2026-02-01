// CELT Decoder - Integration of all CELT components
// Rewritten from xiph/opus celt/celt_decoder.c
// Copyright (c) 2007-2010 Xiph.Org Foundation
// Copyright (c) 2008 Gregory Maxwell
// SPDX-License-Identifier: BSD-3-Clause

use crate::entdec::RangeDecoder;
use crate::quant_bands::{unquant_coarse_energy, unquant_fine_energy, unquant_energy_finalise};
use crate::cwrs::decode_pulses;
use crate::bands::{denormalise_bands, anti_collapse};
use crate::mdct::{MdctContext, vorbis_window};
use crate::celt_constants::{EBANDS_48K, NB_BANDS};
use crate::rate::compute_allocation;

/// CELT decoder state
pub struct CeltDecoder {
    /// Sample rate (typically 48000 Hz)
    sample_rate: u32,
    /// Number of channels (1 or 2)
    channels: usize,
    /// Frame size in samples (120, 240, 480, or 960)
    pub frame_size: usize,
    /// log2(frame_size / 120)
    lm: usize,

    // MDCT state
    mdct: MdctContext,
    window: Vec<f32>,

    // Overlap buffers (per channel)
    overlap: Vec<Vec<f32>>,

    // Previous frame energies for temporal prediction
    old_band_e: Vec<f32>,
    prev1_log_e: Vec<f32>,
    prev2_log_e: Vec<f32>,

    // Random seed for anti-collapse
    rng: u32,
}

impl CeltDecoder {
    /// Create a new CELT decoder
    ///
    /// Arguments:
    /// - sample_rate: Audio sample rate (typically 48000)
    /// - channels: Number of channels (1 or 2)
    /// - frame_size: Frame size in samples (120, 240, 480, or 960)
    pub fn new(sample_rate: u32, channels: usize, frame_size: usize) -> Self {
        // Compute LM (log2 of frame size / 120)
        let lm = match frame_size {
            120 => 0,
            240 => 1,
            480 => 2,
            960 => 3,
            _ => panic!("Invalid frame size: {}", frame_size),
        };

        // Create MDCT context
        let mdct = MdctContext::new(frame_size);
        let window = vorbis_window(frame_size);

        // Initialize overlap buffers (one per channel)
        let mut overlap = Vec::new();
        for _ in 0..channels {
            overlap.push(vec![0.0; frame_size / 2]);
        }

        // Initialize energy buffers (NB_BANDS per channel)
        let old_band_e = vec![0.0; NB_BANDS * channels];
        let prev1_log_e = vec![0.0; NB_BANDS * channels];
        let prev2_log_e = vec![0.0; NB_BANDS * channels];

        Self {
            sample_rate,
            channels,
            frame_size,
            lm,
            mdct,
            window,
            overlap,
            old_band_e,
            prev1_log_e,
            prev2_log_e,
            rng: 0,
        }
    }

    /// Decode a CELT frame
    ///
    /// Arguments:
    /// - data: Encoded CELT frame data
    /// - output: Output PCM buffer (must be frame_size * channels)
    ///
    /// Returns: Number of samples decoded, or error
    pub fn decode(&mut self, data: &[u8], output: &mut [f32]) -> Result<usize, &'static str> {
        if output.len() < self.frame_size * self.channels {
            return Err("Output buffer too small");
        }

        // Check minimum frame size
        if data.len() < 2 {
            return Err("Frame too short");
        }

        // Create range decoder
        let mut dec = RangeDecoder::new(data)
            .map_err(|_| "Failed to initialize range decoder")?;

        // Decode frame header
        let intra = dec.decode_bit_logp(3);
        let _post_filter = dec.decode_bit_logp(1);

        // Decode coarse energy
        unquant_coarse_energy(
            0,
            NB_BANDS,
            &mut self.old_band_e,
            intra,
            &mut dec,
            self.channels,
            self.lm,
            NB_BANDS,
        );

        // Compute bit allocation
        let bits_available = dec.bits_left() as i32;
        let (pulses, fine_quant, fine_priority) =
            compute_allocation(bits_available, self.lm, self.channels, 0, NB_BANDS, &mut dec);

        // Decode fine energy
        unquant_fine_energy(
            0,
            NB_BANDS,
            &mut self.old_band_e,
            &fine_quant,
            &mut dec,
            self.channels,
            NB_BANDS,
        );

        // Finalize energy
        let bits_left = dec.bits_left() as i32;
        unquant_energy_finalise(
            0,
            NB_BANDS,
            &mut self.old_band_e,
            &fine_quant,
            &fine_priority,
            bits_left,
            &mut dec,
            self.channels,
            NB_BANDS,
        );

        // Decode pulses for each band
        let mut x = vec![0.0; self.frame_size];
        let collapse_masks = vec![0xff; NB_BANDS * self.channels];

        for i in 0..NB_BANDS {
            if pulses[i] > 0 {
                let n = (EBANDS_48K[i + 1] - EBANDS_48K[i]) as usize;

                // Skip bands with only 1 bin (PVQ requires n > 1)
                if n > 1 {
                    let mut y = vec![0; n];

                    decode_pulses(&mut y, n, pulses[i] as usize, &mut dec);

                    // Convert to float and store
                    let start = EBANDS_48K[i] as usize;
                    for (j, &val) in y.iter().enumerate() {
                        if start + j < x.len() {
                            x[start + j] = val as f32;
                        }
                    }
                }
            }
        }

        // Apply anti-collapse
        anti_collapse(
            &mut x,
            &collapse_masks,
            self.lm,
            self.channels,
            0,
            NB_BANDS,
            &self.old_band_e,
            &self.prev1_log_e,
            &self.prev2_log_e,
            &pulses,
            self.rng,
            NB_BANDS,
        );

        // Denormalize bands
        let mut freq = vec![0.0; self.frame_size];
        denormalise_bands(
            &x,
            &mut freq,
            &self.old_band_e,
            &EBANDS_48K,
            0,
            NB_BANDS,
            8, // M = 8 for 960-sample frames
        );

        // Perform IMDCT for each channel
        for c in 0..self.channels {
            let input_start = c * (self.frame_size / 2);
            let output_start = c * self.frame_size;

            let input_slice = if input_start + self.frame_size / 2 <= freq.len() {
                &freq[input_start..input_start + self.frame_size / 2]
            } else {
                &freq[0..self.frame_size / 2]
            };

            self.mdct.imdct(
                input_slice,
                &mut output[output_start..output_start + self.frame_size],
                &self.window,
                &mut self.overlap[c],
            );
        }

        // Update energy history for next frame
        self.prev2_log_e.copy_from_slice(&self.prev1_log_e);
        self.prev1_log_e.copy_from_slice(&self.old_band_e);

        // Update random seed
        self.rng = self.rng.wrapping_add(1);

        Ok(self.frame_size * self.channels)
    }

    /// Reset decoder state
    pub fn reset(&mut self) {
        // Clear overlap buffers
        for overlap in &mut self.overlap {
            overlap.fill(0.0);
        }

        // Clear energy history
        self.old_band_e.fill(0.0);
        self.prev1_log_e.fill(0.0);
        self.prev2_log_e.fill(0.0);

        // Reset RNG
        self.rng = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decoder_creation() {
        let decoder = CeltDecoder::new(48000, 2, 960);
        assert_eq!(decoder.sample_rate, 48000);
        assert_eq!(decoder.channels, 2);
        assert_eq!(decoder.frame_size, 960);
        assert_eq!(decoder.lm, 3);
    }

    #[test]
    fn test_decoder_reset() {
        let mut decoder = CeltDecoder::new(48000, 1, 480);

        // Modify some state
        decoder.old_band_e[0] = 1.0;
        decoder.prev1_log_e[0] = 2.0;
        decoder.overlap[0][0] = 3.0;

        // Reset
        decoder.reset();

        // Check state is cleared
        assert_eq!(decoder.old_band_e[0], 0.0);
        assert_eq!(decoder.prev1_log_e[0], 0.0);
        assert_eq!(decoder.overlap[0][0], 0.0);
    }

    #[test]
    #[should_panic(expected = "Invalid frame size")]
    fn test_invalid_frame_size() {
        CeltDecoder::new(48000, 1, 123); // Invalid frame size
    }
}
