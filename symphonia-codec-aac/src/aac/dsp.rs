// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// Previous Author: Kostya Shishkov <kostya.shiskov@gmail.com>
//
// This source file includes code originally written for the NihAV
// project. With the author's permission, it has been relicensed for,
// and ported to the Symphonia project.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::dsp::mdct::Imdct;

use symphonia_common::mpeg::audio::AudioObjectType;

use crate::aac::common::*;
use crate::aac::eld_window::{ELD_WINDOW_480, ELD_WINDOW_512};
use crate::aac::window::*;

const SHORT_WIN_POINT0: usize = 512 - 64;
const SHORT_WIN_POINT1: usize = 512 + 64;

/// The length of the overlap-add state of a channel for an audio object type.
pub fn delay_len(aot: AudioObjectType) -> usize {
    match aot {
        // The state of the low delay filterbank spans the transforms of the three previous
        // frames (of at most 512 samples).
        AudioObjectType::ErAacEld => 3 * 512,
        _ => 1024,
    }
}

/// An IMDCT of any (even) number of spectral samples, computed directly from its definition. It
/// has the same definition and scaling as [`Imdct`].
struct ImdctDirect {
    n: usize,
    /// `scale * cos(pi / n * (i + (n + 1) / 2) * (k + 1 / 2))` for the output `i` and input `k`.
    table: Vec<f32>,
}

impl ImdctDirect {
    fn new(n: usize, scale: f64) -> Self {
        let mut table = Vec::with_capacity(2 * n * n);

        for i in 0..2 * n {
            for k in 0..n {
                let arg = std::f64::consts::PI / n as f64
                    * (i as f64 + (n as f64 + 1.0) / 2.0)
                    * (k as f64 + 0.5);
                table.push((scale * arg.cos()) as f32);
            }
        }

        ImdctDirect { n, table }
    }

    fn imdct(&self, spec: &[f32], out: &mut [f32]) {
        for (row, out) in self.table.chunks_exact(self.n).zip(out.iter_mut()) {
            *out = row.iter().zip(spec).map(|(c, x)| c * x).sum();
        }
    }
}

/// The IMDCT of the low delay filterbanks: a power-of-2 number of spectral samples (512) uses the
/// FFT based one.
enum LdImdct {
    Fft(Imdct),
    Direct(ImdctDirect),
}

impl LdImdct {
    fn new(n: usize, scale: f64) -> Self {
        if n.is_power_of_two() {
            LdImdct::Fft(Imdct::new_scaled(n, scale))
        }
        else {
            LdImdct::Direct(ImdctDirect::new(n, scale))
        }
    }

    fn imdct(&mut self, spec: &[f32], out: &mut [f32]) {
        match self {
            LdImdct::Fft(imdct) => imdct.imdct(spec, out),
            LdImdct::Direct(imdct) => imdct.imdct(spec, out),
        }
    }
}

/// The filterbank state of AAC LD and ELD for a frame length.
struct LdDsp {
    /// The frame length.
    n: usize,
    /// The low overlap window, which `window_shape` 1 selects (instead of the KBD window).
    low_overlap_win: Vec<f32>,
    sine_win: Vec<f32>,
    /// The window coefficients of AAC ELD.
    eld_win: &'static [f32],
    imdct: LdImdct,
    pcm: Vec<f32>,
    /// The (permuted) spectrum and the transform of AAC ELD.
    eld_spec: Vec<f32>,
    eld_buf: Vec<f32>,
}

pub struct Dsp {
    ld: Option<LdDsp>,
    kbd_long_win: [f32; 1024],
    kbd_short_win: [f32; 128],
    sine_long_win: [f32; 1024],
    sine_short_win: [f32; 128],
    imdct_long: Imdct,
    imdct_short: Imdct,
    pcm_long: [f32; 2048],
    pcm_short: [f32; 1152],
}

impl Dsp {
    pub fn new() -> Self {
        Self::new_for(AudioObjectType::Lc, 1024)
    }

    /// Create the filterbanks for an audio object type with the frame length `frame_len`.
    pub fn new_for(aot: AudioObjectType, frame_len: usize) -> Self {
        let ld = match aot {
            AudioObjectType::ErAacLd | AudioObjectType::ErAacEld => {
                let n = frame_len;

                let mut sine_win = vec![0.0; n];

                generate_window(WindowType::Sine, 1.0, n, true, &mut sine_win);

                // The low overlap window: zero for the first 3/8 of the half window, a sine
                // slope of 1/4 of it, and then one.
                let mut low_overlap_win = vec![0.0; n];
                let slope = n / 4;
                let zeros = (n - slope) / 2;

                for (i, w) in low_overlap_win.iter_mut().enumerate() {
                    *w = if i < zeros {
                        0.0
                    }
                    else if i < zeros + slope {
                        (((i - zeros) as f32 + 0.5) * std::f32::consts::FRAC_PI_2 / slope as f32)
                            .sin()
                    }
                    else {
                        1.0
                    };
                }

                Some(LdDsp {
                    n,
                    low_overlap_win,
                    sine_win,
                    eld_win: if n == 480 { &ELD_WINDOW_480 } else { &ELD_WINDOW_512 },
                    imdct: LdImdct::new(n, 1.0 / (2 * n) as f64),
                    pcm: vec![0.0; 2 * n],
                    eld_spec: vec![0.0; n],
                    eld_buf: vec![0.0; n],
                })
            }
            _ => None,
        };

        let mut kbd_long_win: [f32; 1024] = [0.0; 1024];
        let mut kbd_short_win: [f32; 128] = [0.0; 128];
        generate_window(WindowType::KaiserBessel(4.0), 1.0, 1024, true, &mut kbd_long_win);
        generate_window(WindowType::KaiserBessel(6.0), 1.0, 128, true, &mut kbd_short_win);
        let mut sine_long_win: [f32; 1024] = [0.0; 1024];
        let mut sine_short_win: [f32; 128] = [0.0; 128];
        generate_window(WindowType::Sine, 1.0, 1024, true, &mut sine_long_win);
        generate_window(WindowType::Sine, 1.0, 128, true, &mut sine_short_win);

        Self {
            ld,
            kbd_long_win,
            kbd_short_win,
            sine_long_win,
            sine_short_win,
            imdct_long: Imdct::new_scaled(1024, 1.0 / 2048.0),
            imdct_short: Imdct::new_scaled(128, 1.0 / 256.0),
            pcm_long: [0.0; 2048],
            pcm_short: [0.0; 1152],
        }
    }

    #[allow(clippy::cognitive_complexity)]
    pub fn synth(
        &mut self,
        coeffs: &[f32; 1024],
        delay: &mut [f32],
        seq: u8,
        window_shape: bool,
        prev_window_shape: bool,
        dst: &mut [f32],
    ) {
        let (long_win, short_win) = match window_shape {
            true => (&self.kbd_long_win, &self.kbd_short_win),
            false => (&self.sine_long_win, &self.sine_short_win),
        };

        let (prev_long_win, prev_short_win) = match prev_window_shape {
            true => (&self.kbd_long_win, &self.kbd_short_win),
            false => (&self.sine_long_win, &self.sine_short_win),
        };

        // Inverse MDCT
        if seq != EIGHT_SHORT_SEQUENCE {
            self.imdct_long.imdct(coeffs, &mut self.pcm_long);
        }
        else {
            for (ain, aout) in coeffs.chunks_exact(128).zip(self.pcm_long.chunks_exact_mut(256)) {
                self.imdct_short.imdct(ain, aout);
            }

            // Zero the eight short sequence buffer.
            self.pcm_short.fill(0.0);

            for (w, src) in self.pcm_long.chunks_exact(256).enumerate() {
                if w > 0 {
                    for i in 0..128 {
                        self.pcm_short[w * 128 + i] += src[i] * short_win[i];
                        self.pcm_short[w * 128 + i + 128] += src[i + 128] * short_win[127 - i];
                    }
                }
                else {
                    for i in 0..128 {
                        self.pcm_short[i] = src[i] * prev_short_win[i];
                        self.pcm_short[i + 128] = src[i + 128] * short_win[127 - i];
                    }
                }
            }
        }

        // Output new audio samples.
        match seq {
            ONLY_LONG_SEQUENCE | LONG_START_SEQUENCE => {
                for i in 0..1024 {
                    dst[i] = delay[i] + (self.pcm_long[i] * prev_long_win[i]);
                }
            }
            EIGHT_SHORT_SEQUENCE => {
                dst[..SHORT_WIN_POINT0].copy_from_slice(&delay[..SHORT_WIN_POINT0]);

                for i in SHORT_WIN_POINT0..1024 {
                    dst[i] = delay[i] + self.pcm_short[i - SHORT_WIN_POINT0];
                }
            }
            LONG_STOP_SEQUENCE => {
                dst[..SHORT_WIN_POINT0].copy_from_slice(&delay[..SHORT_WIN_POINT0]);

                for i in SHORT_WIN_POINT0..SHORT_WIN_POINT1 {
                    dst[i] = delay[i] + self.pcm_long[i] * prev_short_win[i - SHORT_WIN_POINT0];
                }
                for i in SHORT_WIN_POINT1..1024 {
                    dst[i] = delay[i] + self.pcm_long[i];
                }
            }
            _ => unreachable!(),
        };

        // Save delay for overlap.
        match seq {
            ONLY_LONG_SEQUENCE | LONG_STOP_SEQUENCE => {
                for i in 0..1024 {
                    delay[i] = self.pcm_long[i + 1024] * long_win[1023 - i];
                }
            }
            EIGHT_SHORT_SEQUENCE => {
                for i in 0..SHORT_WIN_POINT1 {
                    // Last part is already windowed.
                    delay[i] = self.pcm_short[i + 512 + 64];
                }

                delay[SHORT_WIN_POINT1..].fill(0.0);
            }
            LONG_START_SEQUENCE => {
                delay[..SHORT_WIN_POINT0]
                    .copy_from_slice(&self.pcm_long[1024..(SHORT_WIN_POINT0 + 1024)]);

                for i in SHORT_WIN_POINT0..SHORT_WIN_POINT1 {
                    delay[i] = self.pcm_long[i + 1024] * short_win[127 - (i - SHORT_WIN_POINT0)];
                }

                delay[SHORT_WIN_POINT1..].fill(0.0);
            }
            _ => unreachable!(),
        };
    }

    /// Synthesise a frame of AAC LD: an IMDCT of `n` spectral samples, windowed and overlapped
    /// like a long window of AAC LC, with the window length (`2 n`) of the frame length. The
    /// window shape bit selects the sine window or, instead of the KBD window, a low overlap
    /// window.
    pub fn synth_ld(
        &mut self,
        coeffs: &[f32; 1024],
        delay: &mut [f32],
        window_shape: bool,
        prev_window_shape: bool,
        dst: &mut [f32],
    ) {
        let ld = self.ld.as_mut().expect("the filterbank of aac ld exists");
        let n = ld.n;

        let win = if window_shape { &ld.low_overlap_win } else { &ld.sine_win };
        let prev_win = if prev_window_shape { &ld.low_overlap_win } else { &ld.sine_win };

        ld.imdct.imdct(&coeffs[..n], &mut ld.pcm);

        for i in 0..n {
            dst[i] = delay[i] + ld.pcm[i] * prev_win[i];
        }

        for i in 0..n {
            delay[i] = ld.pcm[n + i] * win[n - 1 - i];
        }
    }

    /// Synthesise a frame of AAC ELD: the low delay filterbank (ISO/IEC 14496-3 §4.6.17.2). The
    /// transform is the IMDCT of a rearrangement of the spectrum, and the output is the sum of the
    /// windowed transforms of this and the previous three frames. `delay` holds the transforms of
    /// the previous three frames, the most recent first.
    pub fn synth_eld(&mut self, coeffs: &[f32; 1024], delay: &mut [f32], dst: &mut [f32]) {
        let ld = self.ld.as_mut().expect("the filterbank of aac eld exists");
        let n = ld.n;
        let (n2, n4) = (n / 2, n / 4);

        // Rearrange the spectrum so that the transform is that of a conventional IMDCT (Chivukula,
        // Reznik, Devarajan: "Efficient algorithms for MPEG-4 AAC-ELD, AAC-LD and AAC-LC
        // filterbanks", 2008).
        let spec = &mut ld.eld_spec;
        spec.copy_from_slice(&coeffs[..n]);

        for i in (0..n2).step_by(2) {
            let tmp = spec[i];
            spec[i] = -spec[n - 1 - i];
            spec[n - 1 - i] = tmp;

            let tmp = -spec[i + 1];
            spec[i + 1] = spec[n - 2 - i];
            spec[n - 2 - i] = tmp;
        }

        // The middle half of the transform, which has even symmetry on the left and odd symmetry
        // on the right.
        ld.imdct.imdct(&ld.eld_spec, &mut ld.pcm);

        let buf = &mut ld.eld_buf;

        for (i, b) in buf.iter_mut().enumerate() {
            *b = if i % 2 == 0 { -ld.pcm[n2 + i] } else { ld.pcm[n2 + i] };
        }

        let window = ld.eld_win;
        let saved = &delay[..3 * n];

        for i in n4..n2 {
            dst[i - n4] = buf[n2 - 1 - i] * window[i - n4] + saved[i + n2] * window[i + n - n4]
                - saved[n + n2 - 1 - i] * window[i + 2 * n - n4]
                - saved[2 * n + n2 + i] * window[i + 3 * n - n4];
        }

        for i in 0..n2 {
            dst[n4 + i] = buf[i] * window[i + n2 - n4]
                - saved[n - 1 - i] * window[i + n2 + n - n4]
                - saved[n + i] * window[i + n2 + 2 * n - n4]
                + saved[2 * n + n - 1 - i] * window[i + n2 + 3 * n - n4];
        }

        for i in 0..n4 {
            dst[n2 + n4 + i] = buf[i + n2] * window[i + n - n4]
                - saved[n2 - 1 - i] * window[i + 2 * n - n4]
                - saved[n + n2 + i] * window[i + 3 * n - n4];
        }

        // Update the state.
        delay.copy_within(0..2 * n, n);
        delay[..n].copy_from_slice(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic spectrum.
    fn spectrum(n: usize, seed: u32) -> Vec<f32> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 8) as f32 / (1 << 23) as f32 - 1.0
            })
            .collect()
    }

    #[test]
    fn direct_imdct_is_the_imdct() {
        let n = 512;
        let spec = spectrum(n, 7);

        let mut fast = vec![0.0; 2 * n];
        Imdct::new_scaled(n, 1.0 / (2 * n) as f64).imdct(&spec, &mut fast);

        let mut direct = vec![0.0; 2 * n];
        ImdctDirect::new(n, 1.0 / (2 * n) as f64).imdct(&spec, &mut direct);

        for (a, b) in fast.iter().zip(&direct) {
            assert!((a - b).abs() < 1e-5, "{a} vs {b}");
        }
    }

    #[test]
    fn direct_imdct_of_480_follows_the_definition() {
        let n = 480;
        let spec = spectrum(n, 11);

        let mut out = vec![0.0; 2 * n];
        ImdctDirect::new(n, 1.0).imdct(&spec, &mut out);

        for i in [0, 1, 239, 240, 479, 480, 700, 959] {
            let expected: f64 = spec
                .iter()
                .enumerate()
                .map(|(k, &x)| {
                    f64::from(x)
                        * (std::f64::consts::PI / n as f64
                            * (i as f64 + (n as f64 + 1.0) / 2.0)
                            * (k as f64 + 0.5))
                            .cos()
                })
                .sum();

            assert!((f64::from(out[i]) - expected).abs() < 1e-3, "i={i}");
        }
    }

    #[test]
    fn low_overlap_window_is_power_complementary() {
        for n in [512, 480] {
            let dsp = Dsp::new_for(AudioObjectType::ErAacLd, n);
            let ld = dsp.ld.as_ref().unwrap();

            // The window is zero for 3/8 of the half window, a sine slope for 1/4 of it, and
            // one for the rest.
            assert_eq!(ld.low_overlap_win[..n * 3 / 8].iter().sum::<f32>(), 0.0);
            assert!(ld.low_overlap_win[n * 5 / 8..].iter().all(|&w| w == 1.0));

            for i in 0..n {
                let (a, b) = (ld.low_overlap_win[i], ld.low_overlap_win[n - 1 - i]);
                assert!((a * a + b * b - 1.0).abs() < 1e-5, "n={n} i={i}");

                let (a, b) = (ld.sine_win[i], ld.sine_win[n - 1 - i]);
                assert!((a * a + b * b - 1.0).abs() < 1e-5, "n={n} i={i}");
            }
        }
    }

    #[test]
    fn eld_windows_have_the_length_of_the_filterbank() {
        // The window of an ELD filterbank has 4 N coefficients, the last N / 4 of which are
        // zero and not stored.
        assert_eq!(ELD_WINDOW_512.len(), 512 * 15 / 4);
        assert_eq!(ELD_WINDOW_480.len(), 480 * 15 / 4);

        // The main lobe has its peak within the first 2 N coefficients, at about 1.
        for window in [&ELD_WINDOW_512[..], &ELD_WINDOW_480[..]] {
            let peak = window.iter().cloned().fold(f32::MIN, f32::max);
            assert!(peak > 1.0 && peak < 1.1);
        }
    }

    #[test]
    fn eld_filterbank_reconstructs_a_stationary_signal() {
        // A spectrum of a single line produces a stationary tone. After the state of the
        // filterbank has been filled, the output has a constant amplitude.
        for n in [512, 480] {
            let mut dsp = Dsp::new_for(AudioObjectType::ErAacEld, n);
            let mut delay = vec![0.0; delay_len(AudioObjectType::ErAacEld)];
            let mut coeffs = [0.0f32; 1024];
            coeffs[40] = 1000.0;

            let mut dst = vec![0.0; n];
            let mut energies = vec![];

            for _ in 0..8 {
                dsp.synth_eld(&coeffs, &mut delay, &mut dst);
                energies.push(dst.iter().map(|s| s * s).sum::<f32>());
            }

            // The first frames fill the state of the filterbank.
            let steady = energies[7];
            assert!(steady > 0.0);
            assert!((energies[6] - steady).abs() < steady * 0.02, "n={n}");
        }
    }
}
