// DSD Decimation Orchestrator
// Coordinates the CIC and FIR filters for DSD-to-PCM conversion

//! DSD to PCM decimation.
//!
//! A 1-bit DSD stream is converted to PCM by a chain of linear phase low-pass filters that
//! decimate:
//!
//! 1. A [CIC filter](crate::cic) of 5 to 8 stages, evaluated exactly with integer arithmetic
//!    directly on the packed bits, decimates by `R / 2^h` to a rate of `2^h` times the PCM rate.
//! 2. `h` FIR filters ([`Decimate2`]) of 1 to 3 stages decimate by 2 each. All but the last are
//!    half-band filters. The last has a transition band from 0.4535 to 0.5465 times the PCM
//!    rate (20.0 to 24.1 kHz at 44.1 kHz) and compensates the passband droop of the CIC filter.
//!
//! The overall response is flat to within 0.001 dB up to 0.4535 times the PCM rate, and every
//! frequency that aliases into that band is attenuated by at least 100 dB, except at PCM rates
//! of a quarter or an eighth of the DSD rate, where the CIC filter limits it to 85 and 50 dB.
//!
//! The filters have a combined delay of about 1 ms (43 frames at 44.1 kHz), which is
//! compensated by dropping the output of the start of the stream, so that PCM frame `k` is
//! aligned with DSD sample `k * R`. At the end of the stream, [`DsdDecimator::flush_channel`] outputs the
//! frames that the delay holds back.

use crate::cic::CicFilter;
use crate::fir::{self, Decimate2};

use log::debug;
use symphonia_core::codecs::audio::BitOrder;
use symphonia_core::errors::{Result, decode_error};

/// The edge of the passband, as a fraction of the PCM rate: 20 kHz at 44.1 kHz.
const PASSBAND_FRACTION: f64 = 20_000.0 / 44_100.0;

/// The smallest decimation ratio of the CIC filter, where it is evaluated on whole nibbles.
const MIN_CIC_DECIMATION: usize = 4;

/// The largest number of FIR stages.
const MAX_FIR_STAGES: usize = 3;

/// The DSD idle pattern (silence) with the earliest sample in the most significant bit.
const DSD_IDLE: u8 = 0x69;

/// DSD decimation configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecimationConfig {
    /// The DSD sample rate (Hz)
    pub dsd_rate: u32,
    /// The PCM sample rate (Hz)
    pub pcm_rate: u32,
    /// Total decimation ratio
    pub total_decimation: usize,
    /// CIC decimation ratio (first stage)
    pub cic_decimation: usize,
    /// CIC filter stages
    pub cic_stages: usize,
    /// The number of FIR stages that decimate by 2
    pub fir_stages: usize,
}

impl DecimationConfig {
    /// Create decimation config for DSD to PCM conversion
    ///
    /// The ratio of the rates must be an integer `R` that is even and at least 8, so
    /// that 44.1 kHz DSD converts to PCM at 352.8 kHz and below, but not 48 kHz.
    ///
    /// # Arguments
    /// * `dsd_rate` - DSD input rate (e.g., 2822400 for DSD64)
    /// * `pcm_rate` - Desired PCM output rate (e.g., 44100, 48000, 88200)
    pub fn new(dsd_rate: u32, pcm_rate: u32) -> Result<Self> {
        if pcm_rate == 0 || dsd_rate == 0 {
            return decode_error("dsd: invalid sample rates");
        }

        if dsd_rate % pcm_rate != 0 {
            return decode_error("dsd: incompatible sample rates for decimation");
        }

        let total_decimation = (dsd_rate / pcm_rate) as usize;

        if total_decimation < 2 * MIN_CIC_DECIMATION || total_decimation % 2 != 0 {
            return decode_error(
                "dsd: PCM rate is not an even divisor of at most 1/8 of the DSD rate",
            );
        }

        // As many FIR stages as possible: the CIC filter must decimate by at least 4, and a
        // higher ratio aliases less into the passband when it is followed by FIR filters.
        let mut fir_stages = (total_decimation.trailing_zeros() as usize).min(MAX_FIR_STAGES);
        while total_decimation >> fir_stages < MIN_CIC_DECIMATION {
            fir_stages -= 1;
        }
        let cic_decimation = total_decimation >> fir_stages;

        // The less the CIC filter decimates the more stages it needs to reject what aliases into
        // the passband when it decimates.
        let cic_stages = match fir_stages {
            3 => 5,
            2 => 7,
            _ => 8,
        };

        debug!(
            "DSD decimation config: {} -> {} (total: {}, CIC: {}x{}, FIR: {} stages)",
            dsd_rate, pcm_rate, total_decimation, cic_decimation, cic_stages, fir_stages
        );

        Ok(DecimationConfig {
            dsd_rate,
            pcm_rate,
            total_decimation,
            cic_decimation,
            cic_stages,
            fir_stages,
        })
    }

    /// The highest frequency (Hz) that is passed with a flat response.
    pub fn passband(&self) -> f64 {
        PASSBAND_FRACTION * f64::from(self.pcm_rate)
    }

    /// The sample rate (Hz) at the input of FIR stage `stage`.
    fn stage_rate(&self, stage: usize) -> f64 {
        f64::from(self.dsd_rate) / (self.cic_decimation << stage) as f64
    }

    /// The number of taps of each FIR stage.
    pub fn fir_taps(&self) -> Vec<usize> {
        let passband = self.passband();
        (0..self.fir_stages)
            .map(|stage| {
                let rate = self.stage_rate(stage);
                if stage + 1 < self.fir_stages {
                    fir::halfband_length(rate, passband)
                }
                else {
                    fir::lowpass_length(rate, passband, rate / 2.0 - passband)
                }
            })
            .collect()
    }

    /// The number of PCM frames that the delay of the filters shifts the output by.
    pub fn delay_frames(&self) -> usize {
        let r = self.cic_decimation as f64;
        let stages = self.cic_stages as f64;

        // The delay in DSD samples of the centre of the impulse response of the CIC filter at the
        // end of its window, and of the FIR stages, which work at a lower rate each.
        let mut delay = stages * (r - 1.0) / 2.0 - (r - 1.0);
        for (stage, taps) in self.fir_taps().into_iter().enumerate() {
            delay += (taps / 2) as f64 * (self.cic_decimation << stage) as f64;
        }
        (delay / self.total_decimation as f64).round() as usize
    }

    /// Design the FIR stages.
    fn design(&self) -> Vec<Decimate2> {
        let passband = self.passband();
        let dsd_rate = f64::from(self.dsd_rate);
        let r = self.cic_decimation as f64;
        let n = self.cic_stages as i32;

        (0..self.fir_stages)
            .map(|stage| {
                let rate = self.stage_rate(stage);
                let taps = if stage + 1 < self.fir_stages {
                    fir::design_halfband(rate, passband)
                }
                else {
                    // Compensate the droop of the CIC filter, whose response is
                    // `(sin(pi f R / fs) / (R sin(pi f / fs)))^N`.
                    let compensation = |f: f64| {
                        let x = std::f64::consts::PI * f / dsd_rate;
                        if x < 1e-12 { 1.0 } else { ((r * x.sin()) / (r * x).sin()).powi(n) }
                    };
                    fir::design_lowpass(rate, passband, rate / 2.0 - passband, compensation)
                };
                Decimate2::new(&taps)
            })
            .collect()
    }
}

/// The filter chain of a channel.
struct ChannelDecimator {
    cic: CicFilter,
    stages: Vec<Decimate2>,
    /// The intermediate results of the chain.
    work: [Vec<f32>; 2],
    /// The number of output frames that remain to be dropped to compensate the delay.
    skip: usize,
    /// The number of DSD samples that have been input.
    input_samples: u64,
    /// The number of frames that have been output.
    output_frames: u64,
}

impl ChannelDecimator {
    fn new(config: &DecimationConfig, stages: Vec<Decimate2>, skip: usize) -> Self {
        ChannelDecimator {
            cic: CicFilter::new(config.cic_decimation, config.cic_stages),
            stages,
            work: [Vec::new(), Vec::new()],
            skip,
            input_samples: 0,
            output_frames: 0,
        }
    }

    /// The number of frames that processing `bytes` bytes produces.
    fn output_len(&self, bytes: usize) -> usize {
        let mut n = self.cic.output_size_for_input(bytes);
        for stage in &self.stages {
            n = stage.output_len(n);
        }
        n.saturating_sub(self.skip)
    }

    /// Run `input` through the filters, and return the output, which includes the frames that
    /// are to be dropped at the start of the stream.
    fn run(&mut self, input: &[u8], bit_order: BitOrder) -> &[f32] {
        let (mut src, mut dst) = {
            let [a, b] = &mut self.work;
            (a, b)
        };

        src.resize(self.cic.output_size_for_input(input.len()), 0.0);
        self.cic.process_bytes(input, bit_order, src);

        for stage in &mut self.stages {
            dst.resize(stage.output_len(src.len()), 0.0);
            stage.process(src, dst);
            std::mem::swap(&mut src, &mut dst);
        }

        self.input_samples += 8 * input.len() as u64;
        src
    }

    /// Filter `input` to `output`, which must be at least `output_len(input.len())` long, and
    /// return the number of frames.
    fn process(&mut self, input: &[u8], bit_order: BitOrder, output: &mut [f32]) -> usize {
        let skip = self.skip;
        let result = self.run(input, bit_order);

        let dropped = skip.min(result.len());
        let frames = result.len() - dropped;
        output[..frames].copy_from_slice(&result[dropped..]);

        self.skip -= dropped;
        self.output_frames += frames as u64;
        frames
    }

    fn reset(&mut self, skip: usize) {
        self.cic.reset();
        self.stages.iter_mut().for_each(Decimate2::reset);
        self.skip = skip;
        self.input_samples = 0;
        self.output_frames = 0;
    }
}

/// DSD to PCM decimator for multi-channel audio
pub struct DsdDecimator {
    config: DecimationConfig,
    channels: Vec<ChannelDecimator>,
    bit_order: BitOrder,
    /// The number of output frames dropped at the start of the stream.
    delay: usize,
    /// Padding for the end of the stream.
    padding: Vec<u8>,
}

impl DsdDecimator {
    /// Create a new DSD decimator, that drops the frames that the delay of its filters holds
    /// back at the start of the stream.
    ///
    /// # Arguments
    /// * `config` - Decimation configuration
    /// * `num_channels` - Number of audio channels
    /// * `bit_order` - Bit order of DSD data
    pub fn new(config: DecimationConfig, num_channels: usize, bit_order: BitOrder) -> Self {
        Self::with_delay_compensation(config, num_channels, bit_order, true)
    }

    /// Like [`new`](Self::new), but the frames that the delay holds back are kept if
    /// `compensate_delay` is false. The output is then delayed by [`DecimationConfig::delay_frames`]
    /// frames and the first of them are the start-up of the filters.
    pub fn with_delay_compensation(
        config: DecimationConfig,
        num_channels: usize,
        bit_order: BitOrder,
        compensate_delay: bool,
    ) -> Self {
        let delay = if compensate_delay { config.delay_frames() } else { 0 };

        // The filters are the same for all channels.
        let design = config.design();
        let channels = (0..num_channels)
            .map(|_| ChannelDecimator::new(&config, design.clone(), delay))
            .collect();

        DsdDecimator { config, channels, bit_order, delay, padding: Vec::new() }
    }

    /// The configuration of the decimator.
    pub fn config(&self) -> &DecimationConfig {
        &self.config
    }

    /// The number of channels.
    pub fn num_channels(&self) -> usize {
        self.channels.len()
    }

    /// The number of frames that processing `bytes` more bytes of each channel produces.
    pub fn output_len(&self, bytes: usize) -> usize {
        self.channels.first().map_or(0, |channel| channel.output_len(bytes))
    }

    /// Process the packed DSD bytes of one channel, `input`, to PCM, in `output`, which must be
    /// at least [`output_len`](Self::output_len) frames long.
    ///
    /// All channels must be given the same number of bytes between calls to
    /// [`reset`](Self::reset), and in the same order.
    ///
    /// # Returns
    /// Number of PCM samples produced
    pub fn process_channel(
        &mut self,
        channel: usize,
        input: &[u8],
        output: &mut [f32],
    ) -> Result<usize> {
        let Some(decimator) = self.channels.get_mut(channel)
        else {
            return decode_error("dsd: channel index out of range");
        };
        if output.len() < decimator.output_len(input.len()) {
            return decode_error("dsd: PCM output buffer too small");
        }
        Ok(decimator.process(input, self.bit_order, output))
    }

    /// Process DSD data to PCM
    ///
    /// # Arguments
    /// * `dsd_planes` - DSD input data per channel (planar format)
    /// * `pcm_planes` - PCM output buffers per channel (planar format)
    ///
    /// # Returns
    /// Number of PCM samples produced per channel
    pub fn process_planar(
        &mut self,
        dsd_planes: &[&[u8]],
        pcm_planes: &mut [&mut [f32]],
    ) -> Result<usize> {
        if dsd_planes.len() != self.channels.len() {
            return decode_error("dsd: channel count mismatch");
        }

        if pcm_planes.len() != self.channels.len() {
            return decode_error("dsd: output channel count mismatch");
        }

        let mut samples_produced = 0;

        for (ch, (input, output)) in dsd_planes.iter().zip(pcm_planes.iter_mut()).enumerate() {
            let produced = self.process_channel(ch, input, output)?;

            if ch == 0 {
                samples_produced = produced;
            }
            else if produced != samples_produced {
                return decode_error("dsd: channel sample count mismatch");
            }
        }

        Ok(samples_produced)
    }

    /// The number of frames [`flush_channel`](Self::flush_channel) produces: the frames that the delay of the
    /// filters has held back of the stream so far, a stream of `n` DSD samples being `n / R`
    /// frames long.
    pub fn pending_frames(&self) -> usize {
        self.channels.first().map_or(0, |channel| self.pending(channel))
    }

    fn pending(&self, channel: &ChannelDecimator) -> usize {
        let total = channel.input_samples / self.config.total_decimation as u64;
        total.saturating_sub(channel.output_frames) as usize
    }

    /// Output the frames that [`pending_frames`](Self::pending_frames) counts of `channel`, by
    /// running DSD silence through the filters, to `output`, which must be at least that long.
    /// The channel then has no more pending frames until more input is processed.
    ///
    /// # Returns
    /// Number of PCM samples produced
    pub fn flush_channel(&mut self, channel: usize, output: &mut [f32]) -> Result<usize> {
        let Some(pending) = self.channels.get(channel).map(|c| self.pending(c))
        else {
            return decode_error("dsd: channel index out of range");
        };
        let r = self.config.total_decimation;
        let bit_order = self.bit_order;

        let decimator = &mut self.channels[channel];
        if output.len() < pending {
            return decode_error("dsd: PCM output buffer too small");
        }

        // The last frame needs the input up to a delay after its position.
        let samples = (self.delay + 2) * r;
        let idle = match bit_order {
            BitOrder::MsbFirst => DSD_IDLE,
            BitOrder::LsbFirst => DSD_IDLE.reverse_bits(),
        };
        self.padding.clear();
        self.padding.resize(samples.div_ceil(8), idle);

        let skip = decimator.skip;
        let input_samples = decimator.input_samples;
        let result = decimator.run(&self.padding, bit_order);
        let dropped = skip.min(result.len());
        let frames = (result.len() - dropped).min(pending);
        output[..frames].copy_from_slice(&result[dropped..dropped + frames]);

        // The padding is not part of the stream.
        decimator.input_samples = input_samples;
        decimator.output_frames += frames as u64;
        decimator.skip = 0;
        Ok(frames)
    }

    /// Reset filter state
    pub fn reset(&mut self) {
        let delay = self.delay;
        self.channels.iter_mut().for_each(|channel| channel.reset(delay));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::f64::consts::PI;

    const DSD64: u32 = 2_822_400;

    /// Modulate a sine of `freq` Hz and `amplitude` with a second order 1-bit delta-sigma
    /// modulator, to `seconds` of DSD64 packed with the first sample in the most significant bit.
    fn modulate_sine(freq: f64, amplitude: f64, seconds: f64) -> Vec<u8> {
        let samples = (f64::from(DSD64) * seconds) as usize / 8 * 8;
        let (mut i1, mut i2) = (0.0f64, 0.0f64);
        let mut bytes = vec![0u8; samples / 8];
        for n in 0..samples {
            let x = amplitude * (2.0 * PI * freq * n as f64 / f64::from(DSD64)).sin();
            let v = if i2 >= 0.0 { 1.0 } else { -1.0 };
            i1 += x - v;
            i2 += i1 - v;
            if v > 0.0 {
                bytes[n / 8] |= 0x80 >> (n % 8);
            }
        }
        bytes
    }

    fn decimate(config: DecimationConfig, dsd: &[u8], bit_order: BitOrder) -> Vec<f32> {
        let mut decimator = DsdDecimator::new(config, 1, bit_order);
        let mut out = vec![0.0; decimator.output_len(dsd.len())];
        let n = decimator.process_channel(0, dsd, &mut out).unwrap();
        out.truncate(n);
        out
    }

    /// The amplitude and phase (of a sine) of the component of `freq` Hz of `x` sampled at
    /// `rate` Hz, from `skip` samples on, by least squares.
    fn fit(x: &[f32], rate: f64, freq: f64, skip: usize) -> (f64, f64) {
        let (mut ss, mut cc, mut sc, mut sx, mut cx) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (n, &v) in x.iter().enumerate().skip(skip) {
            let w = 2.0 * PI * freq * n as f64 / rate;
            let (s, c) = w.sin_cos();
            ss += s * s;
            cc += c * c;
            sc += s * c;
            sx += s * f64::from(v);
            cx += c * f64::from(v);
        }
        let det = ss * cc - sc * sc;
        let a = (sx * cc - cx * sc) / det;
        let b = (cx * ss - sx * sc) / det;
        (a.hypot(b), b.atan2(a))
    }

    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }

    #[test]
    fn verify_config() {
        // (DSD rate, PCM rate, CIC decimation, CIC stages, FIR stages)
        let cases = [
            (DSD64, 44_100, 8, 5, 3),
            (DSD64, 88_200, 4, 5, 3),
            (DSD64, 176_400, 4, 7, 2),
            (DSD64, 352_800, 4, 8, 1),
            (DSD64, 22_050, 16, 5, 3),
            (DSD64 * 2, 44_100, 16, 5, 3),
            (DSD64 * 4, 44_100, 32, 5, 3),
            (DSD64 * 8, 44_100, 64, 5, 3),
            (3_072_000, 48_000, 8, 5, 3),
            (DSD64, 14_700, 24, 5, 3),
        ];
        for (dsd, pcm, cic, stages, fir) in cases {
            let c = DecimationConfig::new(dsd, pcm).unwrap();
            assert_eq!(c.total_decimation, (dsd / pcm) as usize);
            assert_eq!((c.cic_decimation, c.cic_stages, c.fir_stages), (cic, stages, fir));
            assert_eq!(c.cic_decimation << c.fir_stages, c.total_decimation);
            assert_eq!(c.fir_taps().len(), fir);
        }

        // Rates that do not divide, ratios that are odd or less than 8, and zero.
        for (dsd, pcm) in [
            (DSD64, 48_000),
            (DSD64, 705_600),
            (DSD64, 1_411_200),
            (DSD64, 313_600),
            (DSD64, 0),
            (0, 44_100),
        ] {
            assert!(DecimationConfig::new(dsd, pcm).is_err(), "{dsd} -> {pcm}");
        }

        // About a millisecond of delay.
        let c = DecimationConfig::new(DSD64, 44_100).unwrap();
        assert_eq!(c.fir_taps(), vec![19, 27, 153]);
        assert_eq!(c.delay_frames(), 43);
    }

    #[test]
    fn verify_passband_gain_and_phase() {
        let config = DecimationConfig::new(DSD64, 44_100).unwrap();
        let rate = 44_100.0;

        for freq in [1_000.0, 5_000.0, 10_000.0, 15_000.0, 18_000.0, 20_000.0] {
            let dsd = modulate_sine(freq, 0.25, 0.2);
            let pcm = decimate(config, &dsd, BitOrder::MsbFirst);
            assert_eq!(pcm.len(), dsd.len() * 8 / 64 - config.delay_frames());

            let (amplitude, phase) = fit(&pcm, rate, freq, 200);
            assert!(db(amplitude / 0.25).abs() < 0.02, "{freq} Hz: {} dB", db(amplitude / 0.25));

            // The output is aligned with the input to within a frame.
            let delay_frames = -phase / (2.0 * PI * freq) * rate;
            assert!(delay_frames.abs() < 0.75, "{freq} Hz: delay {delay_frames} frames");
        }
    }

    #[test]
    fn verify_delay_without_compensation() {
        let config = DecimationConfig::new(DSD64, 44_100).unwrap();
        // A frequency whose period is more than twice the delay.
        let dsd = modulate_sine(400.0, 0.25, 0.1);
        let mut decimator =
            DsdDecimator::with_delay_compensation(config, 1, BitOrder::MsbFirst, false);
        let mut pcm = vec![0.0; decimator.output_len(dsd.len())];
        let n = decimator.process_channel(0, &dsd, &mut pcm).unwrap();
        pcm.truncate(n);
        assert_eq!(pcm.len(), dsd.len() * 8 / 64);

        let (_, phase) = fit(&pcm, 44_100.0, 400.0, 200);
        let delay_frames = -phase / (2.0 * PI * 400.0) * 44_100.0;
        assert!((delay_frames - config.delay_frames() as f64).abs() < 0.75, "{delay_frames}");
    }

    #[test]
    fn verify_aliasing_is_rejected() {
        let rate = 44_100.0;
        let config = DecimationConfig::new(DSD64, 44_100).unwrap();

        // Tones that the decimation aliases into the passband.
        for freq in [26_000.0, 30_000.0, 40_000.0, 60_000.0, 100_000.0, 150_000.0, 350_000.0] {
            let dsd = modulate_sine(freq, 0.25, 0.2);
            let pcm = decimate(config, &dsd, BitOrder::MsbFirst);
            let alias = (freq - rate * (freq / rate).round()).abs();
            assert!(alias < 20_000.0);
            let (amplitude, _) = fit(&pcm, rate, alias, 200);
            assert!(
                db(amplitude / 0.25) < -95.0,
                "{freq} Hz -> {alias} Hz: {} dB",
                db(amplitude / 0.25)
            );
        }
    }

    #[test]
    fn verify_dc_gain_and_silence() {
        let config = DecimationConfig::new(DSD64, 44_100).unwrap();

        // Full scale: all ones. The output is 1.0 once the filters are full.
        let pcm = decimate(config, &[0xFF; 5000], BitOrder::LsbFirst);
        for &s in &pcm[200..] {
            assert!((s - 1.0).abs() < 1e-5, "{s}");
        }
        let pcm = decimate(config, &[0x00; 5000], BitOrder::LsbFirst);
        for &s in &pcm[200..] {
            assert!((s + 1.0).abs() < 1e-5, "{s}");
        }

        // The idle pattern is silence.
        let pcm = decimate(config, &[DSD_IDLE; 5000], BitOrder::MsbFirst);
        for &s in &pcm[200..] {
            assert!(s.abs() < 1e-6, "{s}");
        }
    }

    #[test]
    fn verify_chunking_and_bit_order() {
        let config = DecimationConfig::new(DSD64, 88_200).unwrap();
        let dsd = modulate_sine(3_000.0, 0.3, 0.05);
        let whole = decimate(config, &dsd, BitOrder::MsbFirst);

        // The same data with the bits of each byte reversed, and the other bit order.
        let reversed: Vec<u8> = dsd.iter().map(|b| b.reverse_bits()).collect();
        assert_eq!(decimate(config, &reversed, BitOrder::LsbFirst), whole);

        // In chunks of every size.
        for chunk in [1usize, 2, 3, 7, 64, 333, 4096] {
            let mut decimator = DsdDecimator::new(config, 1, BitOrder::MsbFirst);
            let mut got = Vec::new();
            for part in dsd.chunks(chunk) {
                let mut out = vec![0.0; decimator.output_len(part.len())];
                let n = decimator.process_channel(0, part, &mut out).unwrap();
                assert_eq!(n, out.len());
                got.extend_from_slice(&out);
            }
            assert_eq!(got, whole, "chunks of {chunk}");
        }

        // After a reset the output starts again.
        let mut decimator = DsdDecimator::new(config, 1, BitOrder::MsbFirst);
        let mut out = vec![0.0; decimator.output_len(dsd.len())];
        decimator.process_channel(0, &dsd, &mut out).unwrap();
        decimator.reset();
        let mut again = vec![0.0; decimator.output_len(dsd.len())];
        decimator.process_channel(0, &dsd, &mut again).unwrap();
        assert_eq!(out, again);
    }

    #[test]
    fn verify_channels_are_independent() {
        let config = DecimationConfig::new(DSD64, 44_100).unwrap();
        let a = modulate_sine(1_000.0, 0.25, 0.02);
        let b = modulate_sine(7_000.0, 0.4, 0.02);

        let mut decimator = DsdDecimator::new(config, 2, BitOrder::MsbFirst);
        let frames = decimator.output_len(a.len());
        let (mut pa, mut pb) = (vec![0.0; frames], vec![0.0; frames]);
        let n = decimator.process_planar(&[&a, &b], &mut [&mut pa, &mut pb]).unwrap();
        assert_eq!(n, frames);
        assert_eq!(pa, decimate(config, &a, BitOrder::MsbFirst));
        assert_eq!(pb, decimate(config, &b, BitOrder::MsbFirst));

        // The channels must have the same length and the buffers be large enough.
        assert!(decimator.process_planar(&[&a], &mut [&mut pa]).is_err());
        assert!(decimator.process_channel(0, &a, &mut [0.0; 1]).is_err());
    }

    #[test]
    fn verify_flush_completes_the_stream() {
        let config = DecimationConfig::new(DSD64, 44_100).unwrap();
        let dsd = modulate_sine(2_000.0, 0.25, 0.1);
        let samples = dsd.len() * 8;

        let mut decimator = DsdDecimator::new(config, 1, BitOrder::MsbFirst);
        let mut pcm = Vec::new();
        for part in dsd.chunks(1000) {
            let mut out = vec![0.0; decimator.output_len(part.len())];
            decimator.process_channel(0, part, &mut out).unwrap();
            pcm.extend_from_slice(&out);
        }
        assert_eq!(decimator.pending_frames(), config.delay_frames());

        let mut tail = vec![0.0; decimator.pending_frames()];
        assert_eq!(decimator.flush_channel(0, &mut tail).unwrap(), tail.len());
        assert_eq!(decimator.pending_frames(), 0);
        pcm.extend_from_slice(&tail);
        assert_eq!(pcm.len(), samples / 64);

        // The result is that of the stream followed by DSD silence.
        let mut padded = dsd.clone();
        padded.resize(dsd.len() + 400, DSD_IDLE);
        let whole = decimate(config, &padded, BitOrder::MsbFirst);
        assert_eq!(pcm, whole[..pcm.len()]);

        // The tone continues to its end.
        let (amplitude, _) = fit(&pcm[..pcm.len() - 50], 44_100.0, 2_000.0, 200);
        assert!(db(amplitude / 0.25).abs() < 0.02);
    }
}
