// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A streaming sample-rate converter.
//!
//! The converter is a band-limited interpolator, as described by Julius O. Smith III and Phil
//! Gossett, "A Flexible Sampling-Rate Conversion Method", ICASSP 1984, and in J. O. Smith,
//! *Digital Audio Resampling Home Page*, CCRMA, Stanford University,
//! <https://ccrma.stanford.edu/~jos/resample/>. Each output sample is the sum of the input samples
//! around its (fractional) position weighted by a Kaiser-windowed sinc, whose cutoff is half the
//! lower of the two sample rates so that downsampling does not alias.
//!
//! The windowed sinc is evaluated from a *polyphase* table. The table has one row of filter
//! coefficients for each of [`ResamplerOptions::phases`] equally spaced fractional positions
//! between two input samples. The output position is rounded down to a row, and the two
//! neighbouring rows are linearly interpolated (Smith, "Linear interpolation of the
//! filter table"). The position of each output sample is tracked as an exact rational number, so
//! arbitrary ratios do not accumulate error.
//!
//! The Kaiser window is from J. F. Kaiser, "Nonrecursive digital filter design using the I0-sinh
//! window function", Proc. IEEE ISCAS 1974. The window parameter of the default options gives a
//! stopband attenuation of about 90 dB (Kaiser's formula, `beta = 0.1102 * (A - 8.7)`).
//!
//! The filter is symmetric, so it adds no phase shift: output sample `k` is the signal at exactly
//! `k * input_rate / output_rate` input samples. The converter needs [`half_width`] input samples
//! beyond a position to produce it, so output trails the input by that many samples until
//! [`Resampler::flush`] is called at the end of the stream.
//!
//! [`half_width`]: Resampler::half_width

use crate::dsp::window::bessel_i0;
use crate::errors::{Result, limit_error, unsupported_error};

/// The largest filter table, in coefficients, that will be built.
const MAX_TABLE_LEN: usize = 1 << 23;

/// Options for a [`Resampler`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ResamplerOptions {
    /// The number of zero crossings of the sinc kernel on each side of the output position, at
    /// the lower of the two sample rates. More zero crossings give a narrower transition band
    /// (the filter is `2 * zero_crossings` input samples long when upsampling) at proportionally
    /// higher cost.
    pub zero_crossings: usize,
    /// The number of fractional positions between two input samples that have their own row in
    /// the filter table. The error of the linear interpolation between rows decreases with the
    /// square of this number.
    pub phases: usize,
    /// The beta parameter of the Kaiser window. Larger values give a higher stopband attenuation
    /// and a wider transition band.
    pub beta: f64,
}

impl Default for ResamplerOptions {
    fn default() -> Self {
        // A passband that is flat (within 0.01 dB) to 0.45 of the lower sample rate and a stopband
        // of -90 dB or better from 0.55.
        ResamplerOptions { zero_crossings: 32, phases: 512, beta: 9.0 }
    }
}

/// A streaming, multi-channel sample-rate converter operating on `f32` samples.
///
/// Feed input with [`process`](Resampler::process) (planar) or
/// [`process_interleaved`](Resampler::process_interleaved), and call
/// [`flush`](Resampler::flush) once at the end of the stream to obtain the remaining output.
#[derive(Clone, Debug)]
pub struct Resampler {
    channels: usize,
    /// Input rate and output rate divided by their greatest common divisor.
    in_rate: u64,
    out_rate: u64,
    /// The advance, in input samples, per output sample: `step_int + step_rem / out_rate`.
    step_int: usize,
    step_rem: u64,
    /// The number of input samples on each side of the output position used by the filter.
    half: usize,
    /// The number of filter taps, `2 * half`.
    taps: usize,
    phases: usize,
    /// `phases + 1` rows of `taps` coefficients. Row `p` is the filter for the fractional
    /// position `p / phases`; the last row duplicates the first, shifted by one input sample.
    table: Vec<f32>,
    /// The retained input of each channel. Index 0 is at `drained` in padded coordinates, where
    /// the stream is preceded by `half - 1` zeros, so that padded coordinate `i` is the first
    /// filter tap of the output position that has the integer part `i`.
    hist: Vec<Vec<f32>>,
    /// The padded coordinate of the first tap of the next output sample, relative to the start of
    /// `hist`.
    start: usize,
    /// The number of padded input samples dropped from the front of `hist`.
    drained: u64,
    /// The fractional part of the next output position, in units of `1 / out_rate`.
    frac: u64,
    /// The number of input frames received since the start (or last flush).
    total_in: u64,
    /// Planar scratch output for the interleaved functions.
    scratch: Vec<Vec<f32>>,
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl Resampler {
    /// Creates a converter from `in_rate` Hz to `out_rate` Hz for `channels` channels.
    ///
    /// Building the filter table takes `O(phases * taps)` time.
    ///
    /// # Errors
    ///
    /// Returns an unsupported error if a rate, the channel count, `zero_crossings` or `phases` is
    /// zero, or if `beta` is negative or not finite. Returns a limit error if the sample rate
    /// ratio is so large that the filter table would exceed 8M coefficients.
    pub fn new(
        in_rate: u32,
        out_rate: u32,
        channels: usize,
        opts: &ResamplerOptions,
    ) -> Result<Self> {
        if in_rate == 0 || out_rate == 0 || channels == 0 {
            return unsupported_error("core (resample): rates and channels must be non-zero");
        }
        if opts.zero_crossings == 0 || opts.phases == 0 {
            return unsupported_error(
                "core (resample): zero_crossings and phases must be non-zero",
            );
        }
        if !opts.beta.is_finite() || opts.beta < 0.0 {
            return unsupported_error("core (resample): invalid window parameter");
        }

        let g = gcd(u64::from(in_rate), u64::from(out_rate));
        let (in_rate, out_rate) = (u64::from(in_rate) / g, u64::from(out_rate) / g);

        // The cutoff, relative to the input rate. Below the input Nyquist frequency when
        // downsampling, so the part of the spectrum that the output rate cannot represent is
        // removed. When downsampling the kernel is stretched by `1 / cutoff`, so the number of
        // zero crossings stays constant.
        let cutoff = (out_rate as f64 / in_rate as f64).min(1.0);

        // The kernel spans `zero_crossings` zero crossings on either side, which is
        // `zero_crossings / cutoff` input samples.
        let half = (opts.zero_crossings as f64 / cutoff).ceil();

        let taps = half * 2.0;
        if taps * (opts.phases as f64 + 1.0) > MAX_TABLE_LEN as f64 {
            return limit_error("core (resample): sample rate ratio too large");
        }
        let half = half as usize;
        let taps = half * 2;

        let table = build_table(half, opts.phases, cutoff, opts.beta);

        let step_int = (in_rate / out_rate) as usize;
        let step_rem = in_rate % out_rate;

        let mut resampler = Resampler {
            channels,
            in_rate,
            out_rate,
            step_int,
            step_rem,
            half,
            taps,
            phases: opts.phases,
            table,
            hist: vec![Vec::new(); channels],
            start: 0,
            drained: 0,
            frac: 0,
            total_in: 0,
            scratch: vec![Vec::new(); channels],
        };

        resampler.reset();

        Ok(resampler)
    }

    /// The number of input samples after an output sample's position that must have been received
    /// before the output sample can be produced.
    pub fn half_width(&self) -> usize {
        self.half
    }

    /// The number of channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The number of frames that [`flush`](Self::flush) following `input_frames` input frames in
    /// total produces: `ceil(input_frames * output_rate / input_rate)`.
    pub fn output_frames_for(&self, input_frames: u64) -> u64 {
        (input_frames as u128 * self.out_rate as u128).div_ceil(self.in_rate as u128) as u64
    }

    /// Discards all buffered input and starts a new stream.
    pub fn reset(&mut self) {
        for hist in &mut self.hist {
            hist.clear();
            // The stream is preceded by silence.
            hist.resize(self.half - 1, 0.0);
        }
        self.start = 0;
        self.drained = 0;
        self.frac = 0;
        self.total_in = 0;
    }

    /// Converts planar input. `input` has one slice per channel, all of the same length. The
    /// converted frames are appended to the corresponding vector of `output`.
    ///
    /// Not all of the input is converted immediately: the end of the stream is converted by
    /// [`flush`](Self::flush).
    ///
    /// # Panics
    ///
    /// Panics if `input` or `output` do not have one entry for each channel, or the input slices
    /// differ in length.
    pub fn process(&mut self, input: &[&[f32]], output: &mut [Vec<f32>]) {
        assert_eq!(input.len(), self.channels, "input channel count mismatch");
        assert_eq!(output.len(), self.channels, "output channel count mismatch");

        let frames = input.first().map_or(0, |ch| ch.len());
        assert!(input.iter().all(|ch| ch.len() == frames), "input channel length mismatch");

        for (hist, ch) in self.hist.iter_mut().zip(input) {
            hist.extend_from_slice(ch);
        }
        self.total_in += frames as u64;

        self.render(false, output);
    }

    /// Converts the buffered end of the stream and resets the converter for a new stream. The
    /// converted frames are appended to `output`, one vector per channel.
    ///
    /// # Panics
    ///
    /// Panics if `output` does not have one entry for each channel.
    pub fn flush(&mut self, output: &mut [Vec<f32>]) {
        assert_eq!(output.len(), self.channels, "output channel count mismatch");

        // Supply the silence that follows the stream.
        for hist in &mut self.hist {
            hist.resize(hist.len() + self.half, 0.0);
        }

        self.render(true, output);
        self.reset();
    }

    /// Converts interleaved input, appending interleaved output to `output`. See
    /// [`process`](Self::process).
    ///
    /// # Panics
    ///
    /// Panics if the length of `input` is not a multiple of the channel count.
    pub fn process_interleaved(&mut self, input: &[f32], output: &mut Vec<f32>) {
        assert!(input.len() % self.channels == 0, "input is not a whole number of frames");

        for (c, hist) in self.hist.iter_mut().enumerate() {
            hist.extend(input.iter().skip(c).step_by(self.channels));
        }
        self.total_in += (input.len() / self.channels) as u64;

        self.render_interleaved(false, output);
    }

    /// Converts the buffered end of the stream, appending interleaved output to `output`, and
    /// resets the converter. See [`flush`](Self::flush).
    pub fn flush_interleaved(&mut self, output: &mut Vec<f32>) {
        for hist in &mut self.hist {
            hist.resize(hist.len() + self.half, 0.0);
        }

        self.render_interleaved(true, output);
        self.reset();
    }

    fn render_interleaved(&mut self, flush: bool, output: &mut Vec<f32>) {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.iter_mut().for_each(Vec::clear);

        self.render(flush, &mut scratch);

        let frames = scratch.first().map_or(0, Vec::len);
        output.reserve(frames * self.channels);
        for f in 0..frames {
            output.extend(scratch.iter().map(|ch| ch[f]));
        }

        self.scratch = scratch;
    }

    /// Produces all output that the buffered input allows. If `flush`, the buffered input ends
    /// with the zero padding past the end of the stream, and output stops at the end of the
    /// stream.
    fn render(&mut self, flush: bool, output: &mut [Vec<f32>]) {
        let taps = self.taps;
        let avail = self.hist[0].len();

        loop {
            if self.start + taps > avail {
                break;
            }
            // The integer part of the output position is `drained + start`.
            if flush && self.drained + self.start as u64 >= self.total_in {
                break;
            }

            // The table row below the fractional position, and the weight of the row above it.
            let pos = self.frac * self.phases as u64;
            let row = (pos / self.out_rate) as usize;
            let mu = (pos % self.out_rate) as f32 / self.out_rate as f32;

            let c0 = &self.table[row * taps..(row + 1) * taps];
            let c1 = &self.table[(row + 1) * taps..(row + 2) * taps];

            for (hist, out) in self.hist.iter().zip(output.iter_mut()) {
                let x = &hist[self.start..self.start + taps];

                let (mut a, mut b) = (0.0f32, 0.0f32);
                for ((&x, &c0), &c1) in x.iter().zip(c0).zip(c1) {
                    a += c0 * x;
                    b += c1 * x;
                }

                out.push(a + mu * (b - a));
            }

            self.start += self.step_int;
            self.frac += self.step_rem;
            if self.frac >= self.out_rate {
                self.frac -= self.out_rate;
                self.start += 1;
            }
        }

        // Drop the input that no future output needs.
        let drop = self.start.min(avail);
        for hist in &mut self.hist {
            hist.drain(..drop);
        }
        self.drained += drop as u64;
        self.start -= drop;
    }
}

/// Builds the polyphase table: `phases + 1` rows of `2 * half` coefficients.
///
/// Tap `m` of row `p` multiplies the input sample at distance `d = m - half + 1 - p / phases` from
/// the output position, with the coefficient `cutoff * sinc(cutoff * d) * kaiser(d / half)`. Each
/// row is scaled to sum to 1, so that a constant input is reproduced exactly.
fn build_table(half: usize, phases: usize, cutoff: f64, beta: f64) -> Vec<f32> {
    use std::f64::consts::PI;

    let taps = 2 * half;
    let i0_beta = bessel_i0(beta * beta / 4.0);

    let mut table = Vec::with_capacity((phases + 1) * taps);
    let mut row = vec![0.0f64; taps];

    for p in 0..=phases {
        let frac = p as f64 / phases as f64;

        for (m, c) in row.iter_mut().enumerate() {
            let d = m as f64 - (half as f64 - 1.0) - frac;

            let x = cutoff * d;
            let sinc = if x == 0.0 { 1.0 } else { (PI * x).sin() / (PI * x) };

            // `bessel_i0` takes (z / 2)^2 for the argument z = beta * sqrt(1 - r^2).
            let r = d / half as f64;
            let window = if r.abs() >= 1.0 {
                0.0
            }
            else {
                bessel_i0(beta * beta * (1.0 - r * r) / 4.0) / i0_beta
            };

            *c = cutoff * sinc * window;
        }

        let sum: f64 = row.iter().sum();
        table.extend(row.iter().map(|&c| (c / sum) as f32));
    }

    table
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    use std::f64::consts::PI;

    fn tone(freq: f64, rate: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * PI * freq * i as f64 / rate).sin() as f32).collect()
    }

    fn convert(rate_in: u32, rate_out: u32, input: &[f32]) -> Vec<f32> {
        let mut r = Resampler::new(rate_in, rate_out, 1, &Default::default()).unwrap();
        let mut out = vec![Vec::new()];
        r.process(&[input], &mut out);
        r.flush(&mut out);
        out.pop().unwrap()
    }

    /// The amplitude of the component at `freq` in `x[range]`, measured with a Hann window.
    fn amplitude(x: &[f32], freq: f64, rate: f64) -> f64 {
        let n = x.len();
        let (mut re, mut im, mut wsum) = (0.0, 0.0, 0.0);
        for (i, &v) in x.iter().enumerate() {
            let w = 0.5 - 0.5 * (2.0 * PI * (i as f64 + 0.5) / n as f64).cos();
            let ph = 2.0 * PI * freq * i as f64 / rate;
            re += w * f64::from(v) * ph.cos();
            im += w * f64::from(v) * ph.sin();
            wsum += w;
        }
        2.0 * (re * re + im * im).sqrt() / wsum
    }

    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }

    /// The edges where the filter sees the zero padding are excluded from measurements.
    fn middle(x: &[f32], margin: usize) -> &[f32] {
        &x[margin..x.len() - margin]
    }

    #[test]
    fn passband_is_flat() {
        // 44.1 kHz to 48 kHz and 48 kHz to 44.1 kHz, tones up to 0.45 of the lower rate.
        for &(rin, rout) in &[(44100u32, 48000u32), (48000, 44100), (22050, 44100)] {
            let lower = f64::from(rin.min(rout));

            for &frac in &[0.01, 0.05, 0.1, 0.2, 0.3, 0.4, 0.45] {
                let freq = lower * frac;
                let input = tone(freq, f64::from(rin), 40000);
                let out = convert(rin, rout, &input);

                let amp = amplitude(middle(&out, 4000), freq, f64::from(rout));
                assert!(
                    db(amp).abs() < 0.01,
                    "{rin}->{rout} @ {freq:.0} Hz: gain {:.5} dB",
                    db(amp)
                );
            }
        }
    }

    #[test]
    fn stopband_is_attenuated_when_downsampling() {
        // 48 kHz to 32 kHz: a tone above the output Nyquist frequency (16 kHz) must not alias.
        let (rin, rout) = (48000u32, 32000u32);

        for &freq in &[17500.0, 19000.0, 21000.0, 23000.0] {
            let alias = f64::from(rout) - freq;
            let input = tone(freq, f64::from(rin), 40000);
            let out = convert(rin, rout, &input);

            let amp = amplitude(middle(&out, 4000), alias, f64::from(rout));
            assert!(db(amp) < -85.0, "{freq} Hz aliases to {alias} Hz at {:.1} dB", db(amp));
        }
    }

    #[test]
    fn images_are_attenuated_when_upsampling() {
        // 22.05 kHz to 44.1 kHz: a tone at 8 kHz has its image at 22.05 - 8 = 14.05 kHz.
        let input = tone(8000.0, 22050.0, 20000);
        let out = convert(22050, 44100, &input);
        let out = middle(&out, 4000);

        let image = amplitude(out, 14050.0, 44100.0);
        assert!(db(image) < -85.0, "image at {:.1} dB", db(image));
    }

    #[test]
    fn output_is_time_aligned() {
        // The filter has no delay: output k is the signal at time k / output_rate.
        let (rin, rout) = (44100u32, 48000u32);
        let freq = 3000.0;

        let input = tone(freq, f64::from(rin), 20000);
        let out = convert(rin, rout, &input);

        let margin = 2000;
        let worst = out[margin..out.len() - margin]
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let k = i + margin;
                let expect = (2.0 * PI * freq * k as f64 / f64::from(rout)).sin();
                (f64::from(v) - expect).abs()
            })
            .fold(0.0, f64::max);

        assert!(worst < 1e-3, "worst error {worst}");
    }

    #[test]
    fn dc_is_preserved() {
        for &(rin, rout) in &[(44100u32, 48000u32), (48000, 8000), (8000, 48000)] {
            let out = convert(rin, rout, &vec![0.5; 20000]);
            for &v in middle(&out, 1500) {
                assert!((v - 0.5).abs() < 1e-5, "{rin}->{rout}: {v}");
            }
        }
    }

    #[test]
    fn same_rate_is_the_identity() {
        let input: Vec<f32> = (0..1000).map(|i| ((i * 37) % 101) as f32 / 50.0 - 1.0).collect();
        let out = convert(48000, 48000, &input);

        assert_eq!(out.len(), input.len());
        for (i, (&o, &x)) in out.iter().zip(&input).enumerate() {
            assert!((o - x).abs() < 1e-5, "frame {i}: {o} != {x}");
        }
    }

    #[test]
    fn output_length_is_exact() {
        for &(rin, rout) in &[(44100u32, 48000u32), (48000, 44100), (8000, 48000), (96000, 8000)] {
            for &n in &[0usize, 1, 100, 9999] {
                let out = convert(rin, rout, &vec![0.0; n]);
                let r = Resampler::new(rin, rout, 1, &Default::default()).unwrap();
                assert_eq!(out.len() as u64, r.output_frames_for(n as u64), "{rin}->{rout} n={n}");
            }
        }
    }

    #[test]
    fn chunked_processing_matches_one_shot() {
        let input = tone(1234.0, 44100.0, 30000);
        let whole = convert(44100, 48000, &input);

        let mut r = Resampler::new(44100, 48000, 1, &Default::default()).unwrap();
        let mut out = vec![Vec::new()];
        // Chunk sizes that are smaller than, and larger than, the filter.
        for chunk in input.chunks(1).take(70).chain(input[70..].chunks(777)) {
            r.process(&[chunk], &mut out);
        }
        r.flush(&mut out);

        assert_eq!(out[0], whole);
    }

    #[test]
    fn channels_are_independent_and_interleaved_matches_planar() {
        let a = tone(440.0, 44100.0, 5000);
        let b = tone(5000.0, 44100.0, 5000);

        let mut r = Resampler::new(44100, 48000, 2, &Default::default()).unwrap();
        let mut planar = vec![Vec::new(), Vec::new()];
        r.process(&[&a, &b], &mut planar);
        r.flush(&mut planar);

        assert_eq!(planar[0], convert(44100, 48000, &a));
        assert_eq!(planar[1], convert(44100, 48000, &b));

        let interleaved_in: Vec<f32> = a.iter().zip(&b).flat_map(|(&a, &b)| [a, b]).collect();
        let mut out = Vec::new();
        r.process_interleaved(&interleaved_in, &mut out);
        r.flush_interleaved(&mut out);

        let expect: Vec<f32> =
            planar[0].iter().zip(&planar[1]).flat_map(|(&a, &b)| [a, b]).collect();
        assert_eq!(out, expect);
    }

    #[test]
    fn flush_starts_a_new_stream() {
        let input = tone(1000.0, 44100.0, 5000);
        let mut r = Resampler::new(44100, 48000, 1, &Default::default()).unwrap();

        let mut first = vec![Vec::new()];
        r.process(&[&input], &mut first);
        r.flush(&mut first);

        let mut second = vec![Vec::new()];
        r.process(&[&input], &mut second);
        r.flush(&mut second);

        assert_eq!(first, second);
    }

    #[test]
    fn rejects_invalid_parameters() {
        let opts = ResamplerOptions::default();
        assert!(Resampler::new(0, 44100, 1, &opts).is_err());
        assert!(Resampler::new(44100, 0, 1, &opts).is_err());
        assert!(Resampler::new(44100, 48000, 0, &opts).is_err());
        assert!(Resampler::new(44100, 48000, 1, &ResamplerOptions { phases: 0, ..opts }).is_err());
        assert!(
            Resampler::new(44100, 48000, 1, &ResamplerOptions { zero_crossings: 0, ..opts })
                .is_err()
        );
        assert!(
            Resampler::new(44100, 48000, 1, &ResamplerOptions { beta: f64::NAN, ..opts }).is_err()
        );
        // A ratio of 1:100000 would need a gigantic filter.
        assert!(Resampler::new(u32::MAX, 1, 1, &opts).is_err());
    }
}
