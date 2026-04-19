// DSD Decimation Orchestrator
// Coordinates CIC and FIR filters for high-quality DSD-to-PCM conversion

use crate::bitstream::unpack_dsd_bytes_to_f32;
use crate::cic::CicFilter;
use crate::fir::FirDecimator;

use symphonia_core::codecs::audio::BitOrder;
use symphonia_core::errors::{decode_error, Result};

use log::debug;

/// DSD decimation configuration
#[derive(Debug, Clone, Copy)]
pub struct DecimationConfig {
    /// Total decimation ratio
    pub total_decimation: usize,
    /// CIC decimation ratio (first stage)
    pub cic_decimation: usize,
    /// FIR decimation ratio (second stage)
    pub fir_decimation: usize,
    /// CIC filter stages
    pub cic_stages: usize,
    /// FIR filter taps
    pub fir_taps: usize,
}

impl DecimationConfig {
    /// Create decimation config for DSD to PCM conversion
    ///
    /// # Arguments
    /// * `dsd_rate` - DSD input rate (e.g., 2822400 for DSD64)
    /// * `pcm_rate` - Desired PCM output rate (e.g., 44100, 48000, 88200)
    pub fn new(dsd_rate: u32, pcm_rate: u32) -> Result<Self> {
        if pcm_rate == 0 || dsd_rate == 0 {
            return decode_error("dsd: invalid sample rates");
        }

        let total_decimation = (dsd_rate / pcm_rate) as usize;

        if total_decimation == 0 || dsd_rate % pcm_rate != 0 {
            return decode_error("dsd: incompatible sample rates for decimation");
        }

        // Choose CIC and FIR decimation ratios
        // CIC handles the heavy lifting (high decimation ratios efficiently)
        // FIR provides final polishing (lower decimation, better filtering)
        let (cic_decimation, fir_decimation) = Self::choose_decimation_ratios(total_decimation);

        // CIC parameters
        let cic_stages = 4; // 4 stages provides good stopband rejection

        // FIR parameters
        let fir_taps = if fir_decimation <= 4 {
            63 // More taps for less decimation
        }
        else {
            31 // Fewer taps for more decimation
        };

        debug!(
            "DSD decimation config: {} -> {} (total: {}, CIC: {}x{}, FIR: {}x{})",
            dsd_rate, pcm_rate, total_decimation, cic_decimation, cic_stages, fir_decimation, fir_taps
        );

        Ok(DecimationConfig {
            total_decimation,
            cic_decimation,
            fir_decimation,
            cic_stages,
            fir_taps,
        })
    }

    /// Choose optimal CIC and FIR decimation ratios
    fn choose_decimation_ratios(total: usize) -> (usize, usize) {
        // Try to keep CIC decimation high (efficient) and FIR decimation moderate
        // Always use at least 2x FIR to ensure proper filtering
        match total {
            8 => (4, 2),      // DSD64 -> 352.8k
            16 => (8, 2),     // DSD64 -> 176.4k or DSD128 -> 352.8k
            32 => (16, 2),    // DSD128 -> 176.4k or DSD64 -> 88.2k or DSD256 -> 352.8k
            64 => (32, 2),    // DSD64 -> 44.1k or DSD256 -> 176.4k
            128 => (64, 2),   // DSD128 -> 44.1k or DSD512 -> 176.4k
            256 => (128, 2),  // DSD256 -> 44.1k
            512 => (128, 4),  // DSD512 -> 44.1k
            _ => {
                // General case: try to balance, always use FIR >= 2
                if total % 64 == 0 {
                    (64, total / 64)
                }
                else if total % 32 == 0 {
                    (32, total / 32)
                }
                else if total % 16 == 0 {
                    (16, total / 16)
                }
                else if total % 8 == 0 {
                    (8, total / 8)
                }
                else if total % 4 == 0 {
                    (4, total / 4)
                }
                else if total % 2 == 0 {
                    (2, total / 2)
                }
                else {
                    // Find best factorization
                    let sqrt = (total as f64).sqrt() as usize;
                    for i in (2..=sqrt).rev() {
                        if total % i == 0 {
                            return (i, total / i);
                        }
                    }
                    (total, 1)
                }
            }
        }
    }
}

/// Per-channel DSD decimator
pub struct ChannelDecimator {
    cic: CicFilter,
    fir: FirDecimator,
    intermediate_buffer: Vec<f32>,
}

impl ChannelDecimator {
    /// Create a new channel decimator
    pub fn new(config: &DecimationConfig) -> Self {
        log::debug!("Creating ChannelDecimator: CIC={}x, FIR={}x",
                    config.cic_decimation, config.fir_decimation);

        let cic = CicFilter::new(config.cic_decimation, config.cic_stages);

        // FIR cutoff: slightly below Nyquist of output rate to avoid aliasing
        let fir_cutoff = 0.4 / config.fir_decimation as f64;
        let fir = FirDecimator::new(config.fir_decimation, config.fir_taps, fir_cutoff);

        log::debug!("FIR created with decimation={}, taps={}, cutoff={}",
                    config.fir_decimation, config.fir_taps, fir_cutoff);

        // Intermediate buffer between CIC and FIR
        // Size enough for typical block processing
        let intermediate_buffer = Vec::with_capacity(8192);

        ChannelDecimator { cic, fir, intermediate_buffer }
    }

    /// Process DSD samples for this channel, producing PCM output
    ///
    /// # Arguments
    /// * `dsd_input` - DSD sample bytes for this channel
    /// * `bit_order` - Bit order of DSD data
    /// * `unpacked_buffer` - Temporary buffer for unpacked DSD bits (reused for efficiency)
    /// * `pcm_output` - Output buffer for PCM samples
    ///
    /// # Returns
    /// Number of PCM samples produced
    pub fn process(
        &mut self,
        dsd_input: &[u8],
        bit_order: BitOrder,
        unpacked_buffer: &mut Vec<f32>,
        pcm_output: &mut [f32],
    ) -> usize {
        // Unpack DSD bytes to individual bit values (as f32: -1.0 or +1.0)
        let num_bits = dsd_input.len() * 8;
        unpacked_buffer.clear();
        unpacked_buffer.resize(num_bits, 0.0);
        unpack_dsd_bytes_to_f32(dsd_input, bit_order, unpacked_buffer);

        // Debug: log first few unpacked values
        static mut LOG_ONCE: bool = false;
        unsafe {
            if !LOG_ONCE {
                log::debug!("Unpacked DSD (first 16): {:?}", &unpacked_buffer[..16.min(unpacked_buffer.len())]);
                LOG_ONCE = true;
            }
        }

        // Stage 1: CIC filter (high decimation)
        self.intermediate_buffer.clear();
        let cic_output_size = self.cic.output_size_for_input(num_bits);

        unsafe {
            static mut CIC_LOG_ONCE: bool = false;
            if !CIC_LOG_ONCE {
                log::debug!("CIC: input_bits={}, expected_output_size={}", num_bits, cic_output_size);
                CIC_LOG_ONCE = true;
            }
        }

        if cic_output_size > 0 {
            self.intermediate_buffer.resize(cic_output_size, 0.0);
            let cic_produced = self.cic.process_buffer(unpacked_buffer, &mut self.intermediate_buffer);
            self.intermediate_buffer.truncate(cic_produced);

            // Debug: log CIC output
            unsafe {
                static mut CIC_LOG_ONCE2: bool = false;
                if !CIC_LOG_ONCE2 {
                    log::debug!("CIC: actually_produced={} (expected {})", cic_produced, cic_output_size);
                    log::debug!("CIC output (first 16): {:?}", &self.intermediate_buffer[..16.min(self.intermediate_buffer.len())]);
                    let min = self.intermediate_buffer.iter().fold(f32::INFINITY, |a, &b| a.min(b));
                    let max = self.intermediate_buffer.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
                    log::debug!("CIC output range: [{}, {}]", min, max);
                    CIC_LOG_ONCE2 = true;
                }
            }
        }

        // Stage 2: FIR filter (moderate decimation + droop compensation)
        let fir_produced = if !self.intermediate_buffer.is_empty() {
            unsafe {
                static mut FIR_LOG_ONCE: bool = false;
                if !FIR_LOG_ONCE {
                    log::debug!("FIR: input_samples={}", self.intermediate_buffer.len());
                    FIR_LOG_ONCE = true;
                }
            }
            self.fir.process_buffer(&self.intermediate_buffer, pcm_output)
        }
        else {
            0
        };

        // Debug: log FIR output
        unsafe {
            static mut FIR_LOG_ONCE2: bool = false;
            if !FIR_LOG_ONCE2 && fir_produced > 0 {
                log::debug!("FIR: actually_produced={}", fir_produced);
                log::debug!("FIR output (first 16): {:?}", &pcm_output[..16.min(fir_produced)]);
                let min = pcm_output[..fir_produced].iter().fold(f32::INFINITY, |a, &b| a.min(b));
                let max = pcm_output[..fir_produced].iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
                log::debug!("FIR output range: [{}, {}]", min, max);
                FIR_LOG_ONCE2 = true;
            }
        }

        fir_produced
    }

    /// Reset filter state
    pub fn reset(&mut self) {
        self.cic.reset();
        self.fir.reset();
        self.intermediate_buffer.clear();
    }
}

/// DSD to PCM decimator for multi-channel audio
pub struct DsdDecimator {
    channels: Vec<ChannelDecimator>,
    bit_order: BitOrder,
    unpacked_buffer: Vec<f32>,
}

impl DsdDecimator {
    /// Create a new DSD decimator
    ///
    /// # Arguments
    /// * `config` - Decimation configuration
    /// * `num_channels` - Number of audio channels
    /// * `bit_order` - Bit order of DSD data
    pub fn new(config: DecimationConfig, num_channels: usize, bit_order: BitOrder) -> Self {
        let channels = (0..num_channels).map(|_| ChannelDecimator::new(&config)).collect();

        DsdDecimator {
            channels,
            bit_order,
            unpacked_buffer: Vec::with_capacity(8192 * 8),
        }
    }

    /// Process DSD data to PCM
    ///
    /// # Arguments
    /// * `dsd_planes` - DSD input data per channel (planar format)
    /// * `pcm_planes` - PCM output buffers per channel (planar format)
    ///
    /// # Returns
    /// Number of PCM samples produced per channel
    pub fn process_planar(&mut self, dsd_planes: &[&[u8]], pcm_planes: &mut [&mut [f32]]) -> Result<usize> {
        if dsd_planes.len() != self.channels.len() {
            return decode_error("dsd: channel count mismatch");
        }

        if pcm_planes.len() != self.channels.len() {
            return decode_error("dsd: output channel count mismatch");
        }

        let mut samples_produced = 0;

        for (ch_idx, decimator) in self.channels.iter_mut().enumerate() {
            let produced =
                decimator.process(dsd_planes[ch_idx], self.bit_order, &mut self.unpacked_buffer, pcm_planes[ch_idx]);

            if ch_idx == 0 {
                samples_produced = produced;
            }
            else if produced != samples_produced {
                return decode_error("dsd: channel sample count mismatch");
            }
        }

        Ok(samples_produced)
    }

    /// Reset all channel decimators
    pub fn reset(&mut self) {
        for decimator in &mut self.channels {
            decimator.reset();
        }
        self.unpacked_buffer.clear();
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decimation_config_dsd64_to_44k() {
        let config = DecimationConfig::new(2822400, 44100).unwrap();
        assert_eq!(config.total_decimation, 64);
        assert_eq!(config.cic_decimation * config.fir_decimation, 64);
    }

    #[test]
    fn test_decimation_config_dsd128_to_88k() {
        let config = DecimationConfig::new(5644800, 88200).unwrap();
        assert_eq!(config.total_decimation, 64);
    }

    #[test]
    fn test_decimation_config_invalid_rates() {
        assert!(DecimationConfig::new(0, 44100).is_err());
        assert!(DecimationConfig::new(2822400, 0).is_err());
        assert!(DecimationConfig::new(2822400, 45000).is_err()); // Not evenly divisible
    }

    #[test]
    fn test_channel_decimator_creation() {
        let config = DecimationConfig::new(2822400, 44100).unwrap();
        let _decimator = ChannelDecimator::new(&config);
    }

    #[test]
    fn test_dsd_decimator_creation() {
        let config = DecimationConfig::new(2822400, 44100).unwrap();
        let decimator = DsdDecimator::new(config, 2, BitOrder::LsbFirst);
        assert_eq!(decimator.channels.len(), 2);
    }

    #[test]
    fn test_choose_decimation_ratios() {
        assert_eq!(DecimationConfig::choose_decimation_ratios(8), (4, 2));
        assert_eq!(DecimationConfig::choose_decimation_ratios(16), (8, 2));
        assert_eq!(DecimationConfig::choose_decimation_ratios(32), (16, 2));
        assert_eq!(DecimationConfig::choose_decimation_ratios(64), (32, 2));
        assert_eq!(DecimationConfig::choose_decimation_ratios(128), (64, 2));
    }
}
