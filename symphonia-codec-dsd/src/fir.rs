// Linear phase FIR decimation filters
//
// The second part of the DSD to PCM conversion: after the CIC filter has brought the rate down
// to a small multiple of the PCM rate, a chain of linear phase FIR filters, each decimating by 2,
// brings it to the PCM rate. All except the last are half-band filters. The last also
// compensates the passband droop of the CIC filter and has a sharp transition band between the
// edge of the audio band and the first frequency that aliases back into it.

use std::f64::consts::PI;

/// The stopband attenuation, in dB, that the FIR filters are designed for. Together with the
/// CIC filter it keeps everything that aliases into the passband more than 100 dB down.
pub const STOPBAND_ATTENUATION_DB: f64 = 110.0;

/// Number of independent accumulators in the dot product. The accumulation order is fixed, so the
/// output is the same on every target.
const LANES: usize = 4;

/// Modified Bessel function of the first kind, order 0.
fn bessel_i0(x: f64) -> f64 {
    let half = x / 2.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..100 {
        term *= (half / f64::from(k)).powi(2);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// The Kaiser window parameter for a stopband attenuation of `db` (> 50) dB.
fn kaiser_beta(db: f64) -> f64 {
    0.1102 * (db - 8.7)
}

/// A symmetric Kaiser window of `len` points.
fn kaiser_window(len: usize, beta: f64) -> Vec<f64> {
    let centre = (len - 1) as f64 / 2.0;
    let denom = bessel_i0(beta);
    (0..len)
        .map(|i| {
            let r = if centre > 0.0 { (i as f64 - centre) / centre } else { 0.0 };
            bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / denom
        })
        .collect()
}

/// The (odd) length of a Kaiser windowed filter with `STOPBAND_ATTENUATION_DB` of attenuation and
/// a transition band of `width`, as a fraction of the sample rate (Kaiser's formula).
fn kaiser_length(width: f64) -> usize {
    assert!(width > 0.0, "the transition band must have a positive width");
    let len = ((STOPBAND_ATTENUATION_DB - 7.95) / (14.36 * width)).ceil() as usize;
    len | 1
}

/// The length of the half-band filter from [`design_halfband`].
pub fn halfband_length(rate: f64, passband: f64) -> usize {
    let mut len = kaiser_length((rate / 2.0 - 2.0 * passband) / rate);
    // The outermost taps of a half-band filter of length 4k+3 are not zero.
    while len % 4 != 3 {
        len += 2;
    }
    len
}

/// Design a half-band low-pass filter for decimation by 2 of a signal sampled at `rate` Hz, which
/// passes `0..passband` Hz and has aliasing of less than `-STOPBAND_ATTENUATION_DB` dB into
/// that band. Since the response of a half-band filter is symmetric about `rate / 4`, the stopband
/// starts at `rate / 2 - passband`. The unity DC gain filter is returned in full, odd length.
pub fn design_halfband(rate: f64, passband: f64) -> Vec<f64> {
    let len = halfband_length(rate, passband);
    let window = kaiser_window(len, kaiser_beta(STOPBAND_ATTENUATION_DB));
    let centre = (len - 1) as f64 / 2.0;

    let mut taps: Vec<f64> = (0..len)
        .map(|i| {
            let x = (i as f64 - centre) / 2.0;
            let sinc = if x == 0.0 { 1.0 } else { (PI * x).sin() / (PI * x) };
            // Make the zero taps exact.
            let sinc = if x != 0.0 && x == x.trunc() { 0.0 } else { sinc };
            0.5 * sinc * window[i]
        })
        .collect();

    let sum: f64 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps
}

/// The length of the filter from [`design_lowpass`].
pub fn lowpass_length(rate: f64, passband: f64, stopband: f64) -> usize {
    kaiser_length((stopband - passband) / rate)
}

/// Design a linear phase low-pass filter for a signal sampled at `rate` Hz, whose response is
/// `gain(f)` for `f` in `0..passband` Hz and which has less than `-STOPBAND_ATTENUATION_DB` dB of
/// gain from `stopband` Hz. `gain` is the desired passband gain, by which a droop of previous
/// filters is compensated, and must be positive and smooth up to `(passband + stopband) / 2`.
///
/// The filter is the Kaiser windowed impulse response of an ideal filter with the gain `gain(f)`
/// up to the frequency in the middle of the transition band, and none above it.
pub fn design_lowpass(
    rate: f64,
    passband: f64,
    stopband: f64,
    gain: impl Fn(f64) -> f64,
) -> Vec<f64> {
    const POINTS: usize = 4096;

    let len = lowpass_length(rate, passband, stopband);
    let window = kaiser_window(len, kaiser_beta(STOPBAND_ATTENUATION_DB));
    let centre = (len - 1) as f64 / 2.0;
    let cutoff = (passband + stopband) / 2.0;

    // The desired gain at the midpoints of `POINTS` equal intervals of 0..cutoff.
    let step = cutoff / POINTS as f64;
    let desired: Vec<f64> = (0..POINTS).map(|k| gain((k as f64 + 0.5) * step)).collect();

    let mut taps: Vec<f64> = (0..len)
        .map(|i| {
            let n = i as f64 - centre;
            // The inverse Fourier transform of the desired gain (midpoint rule).
            let integral: f64 = desired
                .iter()
                .enumerate()
                .map(|(k, g)| g * (2.0 * PI * (k as f64 + 0.5) * step * n / rate).cos())
                .sum();
            2.0 * integral * step / rate * window[i]
        })
        .collect();

    // Unity gain at DC, or the desired one.
    let sum: f64 = taps.iter().sum();
    let dc = gain(0.0);
    taps.iter_mut().for_each(|t| *t *= dc / sum);
    taps
}

/// A symmetric (linear phase) FIR filter of odd length that decimates by 2.
///
/// The filter output at index `n` is `y[n] = sum_j h[j] * x[2n - j]`: the output is aligned with
/// the even samples of the input, and delayed by `delay()` input samples. Initially the input is
/// preceded by zeros.
#[derive(Clone)]
pub struct Decimate2 {
    /// The first half of the impulse response, `h[0..M]`, where `M` is the delay.
    half: Vec<f32>,
    /// The centre tap, `h[M]`.
    centre: f32,
    /// The window of the next output, which starts at the first sample, and anything after it.
    buf: Vec<f32>,
}

impl Decimate2 {
    /// Create a decimator from the full impulse response `taps`, which must be symmetric and of
    /// odd length.
    pub fn new(taps: &[f64]) -> Self {
        assert!(taps.len() % 2 == 1, "the length of a symmetric filter must be odd");
        let m = taps.len() / 2;
        let mut filter = Decimate2 {
            half: taps[..m].iter().map(|&t| t as f32).collect(),
            centre: taps[m] as f32,
            buf: Vec::new(),
        };
        filter.reset();
        filter
    }

    /// The delay of the filter, in input samples.
    pub fn delay(&self) -> usize {
        self.half.len()
    }

    /// The number of taps.
    pub fn num_taps(&self) -> usize {
        2 * self.half.len() + 1
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.buf.resize(2 * self.half.len(), 0.0);
    }

    /// The number of output samples that processing `input_len` more input samples produces.
    pub fn output_len(&self, input_len: usize) -> usize {
        // Windows start at 0, 2, ..., and are 2M + 1 samples long.
        let available = self.buf.len() + input_len;
        available.saturating_sub(2 * self.half.len()).div_ceil(2)
    }

    /// Filter `input`, and write the output to `output`, which must be exactly
    /// [`output_len`](Self::output_len) samples long.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        assert_eq!(output.len(), self.output_len(input.len()), "output has the wrong length");

        self.buf.extend_from_slice(input);
        let m = self.half.len();

        for (i, out) in output.iter_mut().enumerate() {
            let start = 2 * i;
            let window = &self.buf[start..start + 2 * m + 1];
            let (left, rest) = window.split_at(m);
            *out = self.centre * rest[0] + dot_symmetric(&self.half, left, &rest[1..]);
        }

        // Drop the samples that no later window includes.
        self.buf.drain(..2 * output.len());
    }
}

/// Sum of `half[j] * (left[j] + right[M - 1 - j])` for `j` in `0..M`.
#[inline]
fn dot_symmetric(half: &[f32], left: &[f32], right: &[f32]) -> f32 {
    debug_assert!(half.len() == left.len() && half.len() == right.len());
    let mut acc = [0.0f32; LANES];

    let mut halves = half.chunks_exact(LANES);
    let mut lefts = left.chunks_exact(LANES);
    let mut rights = right.rchunks_exact(LANES);

    for ((h, l), r) in (&mut halves).zip(&mut lefts).zip(&mut rights) {
        for lane in 0..LANES {
            acc[lane] += h[lane] * (l[lane] + r[LANES - 1 - lane]);
        }
    }

    // The remaining `M % LANES` taps, which are the innermost taps of the filter.
    let mut tail = 0.0f32;
    let remainder = halves.remainder();
    let rights_rem = rights.remainder();
    for ((h, l), r) in remainder.iter().zip(lefts.remainder()).zip(rights_rem.iter().rev()) {
        tail += h * (l + r);
    }

    ((acc[0] + acc[1]) + (acc[2] + acc[3])) + tail
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The response of a filter at `freq` Hz.
    fn response(taps: &[f64], rate: f64, freq: f64) -> f64 {
        let w = 2.0 * PI * freq / rate;
        let re: f64 = taps.iter().enumerate().map(|(n, h)| h * (w * n as f64).cos()).sum();
        let im: f64 = taps.iter().enumerate().map(|(n, h)| h * (w * n as f64).sin()).sum();
        re.hypot(im)
    }

    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }

    #[test]
    fn verify_halfband_design() {
        let rate = 176_400.0;
        let taps = design_halfband(rate, 20_000.0);
        assert_eq!(taps.len(), halfband_length(rate, 20_000.0));
        assert_eq!(taps.len() % 4, 3);

        // Symmetric, with exact zeros at the even distances from the centre.
        let m = taps.len() / 2;
        for k in 1..=m {
            assert_eq!(taps[m - k], taps[m + k]);
            if k % 2 == 0 {
                assert_eq!(taps[m + k], 0.0);
            }
            else {
                assert_ne!(taps[m + k], 0.0);
            }
        }
        assert!((taps.iter().sum::<f64>() - 1.0).abs() < 1e-12);

        // Flat in the passband, and rejecting what aliases into it.
        for f in (0..=20_000).step_by(500) {
            assert!(db(response(&taps, rate, f64::from(f))).abs() < 1e-3, "{f} Hz");
        }
        for f in ((rate / 2.0 - 20_000.0) as i32..=(rate / 2.0) as i32).step_by(500) {
            assert!(db(response(&taps, rate, f64::from(f))) < -105.0, "{f} Hz");
        }
    }

    #[test]
    fn verify_lowpass_design_compensates_gain() {
        let rate = 88_200.0;
        // A gain that rises towards the band edge, like the inverse of the droop of a CIC filter.
        let gain = |f: f64| {
            let x = PI * f / 352_800.0;
            if x < 1e-9 { 1.0 } else { x / x.sin() }
        };
        let taps = design_lowpass(rate, 20_000.0, 24_100.0, gain);
        assert_eq!(taps.len(), lowpass_length(rate, 20_000.0, 24_100.0));
        let m = taps.len() / 2;
        for k in 1..=m {
            assert!((taps[m - k] - taps[m + k]).abs() < 1e-15);
        }
        for f in (100..=20_000).step_by(300) {
            let f = f64::from(f);
            let err = db(response(&taps, rate, f)) - db(gain(f));
            assert!(err.abs() < 5e-3, "{f} Hz: {err} dB");
        }
        for f in (24_100..=44_100).step_by(500) {
            assert!(db(response(&taps, rate, f64::from(f))) < -100.0, "{f} Hz");
        }
    }

    #[test]
    fn verify_decimator_matches_direct_convolution() {
        let taps = design_halfband(176_400.0, 20_000.0);
        let mut filter = Decimate2::new(&taps);
        assert_eq!(filter.num_taps(), taps.len());
        assert_eq!(filter.delay(), taps.len() / 2);

        let mut seed = 12345u32;
        let input: Vec<f32> = (0..1000)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 8) as f32 / (1 << 23) as f32 - 1.0
            })
            .collect();

        // Feed the input in uneven chunks.
        let mut got = Vec::new();
        let mut rest = &input[..];
        for chunk in [1usize, 2, 3, 57, 100, 1, 300, 536] {
            let (head, tail) = rest.split_at(chunk.min(rest.len()));
            rest = tail;
            let mut out = vec![0.0; filter.output_len(head.len())];
            filter.process(head, &mut out);
            got.extend(out);
        }
        assert!(rest.is_empty());
        assert_eq!(got.len(), input.len().div_ceil(2));

        for (n, &g) in got.iter().enumerate() {
            let want: f64 = taps
                .iter()
                .enumerate()
                .filter(|&(j, _)| 2 * n >= j)
                .map(|(j, &h)| h * f64::from(input[2 * n - j]))
                .sum();
            assert!((f64::from(g) - want).abs() < 1e-5, "output {n}: {g} != {want}");
        }

        // Starting again gives the same output.
        filter.reset();
        let mut out = vec![0.0; filter.output_len(input.len())];
        filter.process(&input, &mut out);
        assert_eq!(out, got);
    }
}
