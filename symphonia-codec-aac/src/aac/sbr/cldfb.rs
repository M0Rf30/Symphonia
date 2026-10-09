// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The low delay complex-modulated filterbanks (CLDFB) of AAC-ELD with low delay SBR.
//!
//! ELD does not use the QMF banks of ordinary SBR (§4.6.18.4). Its analysis bank splits the
//! core output into 32 complex subbands, one slot per 32 samples, with a non-symmetric 320 tap
//! prototype filter (ten blocks of 32 samples, so that the delay is far shorter than that of the
//! 640 tap QMF). Its synthesis bank has 64 bands (dual-rate SBR: the output has twice the sample
//! rate of the core) or 32 bands (downsampled SBR: the output has the sample rate of the core).
//! The 64 band prototype has 640 taps.
//!
//! Both banks are a prototype filter followed by a modulation by `cos` and `sin` kernels of a
//! type IV discrete trigonometric transform, with a phase rotation that makes the subband
//! samples complex-valued. The analysis filter works on the signal in the time order, with the
//! newest sample last; the synthesis filter is the transposed (polyphase) form of the analysis
//! one.
//!
//! ## Provenance
//!
//! The prototype filters are the normative data of the standard. The structure of the banks (the
//! polyphase filtering, the folding into the type IV transforms, and the phase rotations) was
//! established from the FDK AAC source code, and checked against the output of its decoder. The
//! gains are those that give a unit gain in the analysis to synthesis chain, and the subband
//! amplitude of the QMF analysis of ordinary SBR.

use super::cldfb_tables::{CLDFB_320, CLDFB_640};
use super::error::{SbrError as Error, SbrResult as Result};
use super::qmf::Complex;

use core::f64::consts::{FRAC_1_SQRT_2, PI};
use std::sync::LazyLock;

/// The number of the prototype filter taps per band.
const TAPS_PER_BAND: usize = 5;

/// The signs of `cos(3π/4 - iπ/2)` and `sin(3π/4 - iπ/2)` for the analysis, by `i mod 4`.
const ANA_COS: [f64; 4] = [-1.0, 1.0, 1.0, -1.0];
const ANA_SIN: [f64; 4] = [1.0, 1.0, -1.0, -1.0];
/// The signs of `cos(π/4 + iπ/2)` and `sin(π/4 + iπ/2)` for the synthesis, by `i mod 4`.
const SYN_COS: [f64; 4] = [1.0, -1.0, -1.0, 1.0];
const SYN_SIN: [f64; 4] = [1.0, 1.0, -1.0, -1.0];

/// The gain of the analysis bank, so that a sinusoid at the centre of a band has the same
/// subband amplitude as in the QMF analysis of ordinary SBR.
const ANALYSIS_GAIN: f64 = 4.0;
/// The gain of the synthesis bank, so that a unit-amplitude subband signal gives the same
/// output amplitude as in the QMF synthesis of ordinary SBR.
const SYNTHESIS_GAIN: f64 = 1.0 / 32.0;

/// The kernels of the type IV discrete cosine and sine transforms of length `l`:
/// `cos(π/l·(n + 1/2)·(k + 1/2))` (and with `sin`), in `[n][k]` order so that one input value
/// accumulates into all the outputs at once (every output still sums over `n` in order).
#[derive(Debug)]
struct TransformTables {
    l: usize,
    cos: Vec<f64>,
    sin: Vec<f64>,
}

impl TransformTables {
    fn new(l: usize) -> Self {
        let mut cos = vec![0.0; l * l];
        let mut sin = vec![0.0; l * l];
        for k in 0..l {
            for n in 0..l {
                let arg = PI / l as f64 * (n as f64 + 0.5) * (k as f64 + 0.5);
                cos[n * l + k] = arg.cos();
                sin[n * l + k] = arg.sin();
            }
        }
        TransformTables { l, cos, sin }
    }
}

/// A pair of the type IV discrete cosine and sine transforms of length `l`:
/// `X[k] = Σ x[n]·cos(π/l·(n + 1/2)·(k + 1/2))` (and with `sin`). The kernels are shared.
#[derive(Debug, Clone)]
struct TransformIv {
    tables: &'static TransformTables,
}

impl TransformIv {
    fn new(l: usize) -> Self {
        static T32: LazyLock<TransformTables> = LazyLock::new(|| TransformTables::new(32));
        static T64: LazyLock<TransformTables> = LazyLock::new(|| TransformTables::new(64));
        match l {
            32 => TransformIv { tables: &T32 },
            64 => TransformIv { tables: &T64 },
            _ => unreachable!("the CLDFB has 32 or 64 bands"),
        }
    }

    /// Both transforms: the cosine transform of `xc` and the sine transform of `xs`.
    fn dct_dst(&self, xc: &[f64], xs: &[f64], oc: &mut [f64], os: &mut [f64]) {
        let l = self.tables.l;
        let (oc, os) = (&mut oc[..l], &mut os[..l]);
        // The sum of an iterator of floats starts at -0.0.
        oc.fill(-0.0);
        os.fill(-0.0);
        for n in 0..l {
            let (c, s) = (xc[n], xs[n]);
            let rc = &self.tables.cos[n * l..(n + 1) * l];
            let rs = &self.tables.sin[n * l..(n + 1) * l];
            for k in 0..l {
                oc[k] += rc[k] * c;
                os[k] += rs[k] * s;
            }
        }
    }
}

/// The prototype filter of the analysis bank by tap: `[p][k] = CLDFB_320[5k + p]`.
fn analysis_taps() -> &'static [f64] {
    static TAPS: LazyLock<Vec<f64>> = LazyLock::new(|| {
        let mut t = vec![0.0; 320];
        for k in 0..64 {
            for p in 0..TAPS_PER_BAND {
                t[64 * p + k] = CLDFB_320[TAPS_PER_BAND * k + p];
            }
        }
        t
    });
    &TAPS
}

/// The polyphase prototype filter of a synthesis bank with `l` bands by tap and band:
/// `p1[t·l + m] = table[5m + t]` and `p2[t·l + m] = table[half + 5m + t]`.
#[derive(Debug)]
struct SynthesisTaps {
    p1: Vec<f64>,
    p2: Vec<f64>,
}

fn synthesis_taps(l: usize) -> &'static SynthesisTaps {
    fn build(l: usize) -> SynthesisTaps {
        let table: &[f64] = if l == 64 { &CLDFB_640 } else { &CLDFB_320 };
        let half = table.len() / 2;
        let mut p1 = vec![0.0; TAPS_PER_BAND * l];
        let mut p2 = vec![0.0; TAPS_PER_BAND * l];
        for m in 0..l {
            for t in 0..TAPS_PER_BAND {
                p1[t * l + m] = table[TAPS_PER_BAND * m + t];
                p2[t * l + m] = table[half + TAPS_PER_BAND * m + t];
            }
        }
        SynthesisTaps { p1, p2 }
    }
    static T32: LazyLock<SynthesisTaps> = LazyLock::new(|| build(32));
    static T64: LazyLock<SynthesisTaps> = LazyLock::new(|| build(64));
    if l == 64 { &T64 } else { &T32 }
}

/// The 32 band CLDFB analysis bank of one channel.
#[derive(Debug, Clone)]
pub struct CldfbAnalysis {
    /// The last 320 input samples, in time order.
    state: [f64; 320],
    transform: TransformIv,
}

impl Default for CldfbAnalysis {
    fn default() -> Self {
        Self::new()
    }
}

impl CldfbAnalysis {
    /// The number of bands, and of the input samples per slot.
    pub const BANDS: usize = 32;

    #[must_use]
    pub fn new() -> Self {
        CldfbAnalysis { state: [0.0; 320], transform: TransformIv::new(Self::BANDS) }
    }

    /// Feed the 32 new samples of one slot, and return its 32 complex subband samples.
    pub fn push_slot(&mut self, samples: &[f64]) -> Result<[Complex; 32]> {
        const L: usize = CldfbAnalysis::BANDS;

        if samples.len() != L {
            return Err(Error::SbrQmfInvalid);
        }

        self.state.copy_within(L.., 0);
        self.state[320 - L..].copy_from_slice(samples);

        // The prototype filter, in the order of the transform input: `u[2L - 1 - k]`.
        let taps = analysis_taps();
        let mut acc = [-0.0f64; 2 * L];
        for p in 0..TAPS_PER_BAND {
            let c = &taps[2 * L * p..2 * L * (p + 1)];
            let x = &self.state[2 * L * p..2 * L * (p + 1)];
            for k in 0..2 * L {
                acc[k] += c[k] * x[k];
            }
        }
        let mut u = [0.0f64; 2 * L];
        for k in 0..2 * L {
            u[2 * L - 1 - k] = acc[k];
        }

        // The folding into the inputs of the cosine and sine transforms.
        let mut re = [0.0f64; L];
        let mut im = [0.0f64; L];
        for i in 0..L {
            let x = u[i] / 2.0;
            let y = u[2 * L - 1 - i];
            re[i] = x - y / 2.0;
            im[i] = x + y / 2.0;
        }

        let mut tre = [0.0f64; L];
        let mut tim = [0.0f64; L];
        self.transform.dct_dst(&re, &im, &mut tre, &mut tim);

        // The phase rotation.
        let mut out = [Complex::default(); L];
        for (i, o) in out.iter_mut().enumerate() {
            let cos = ANA_COS[i % 4] * FRAC_1_SQRT_2;
            let sin = ANA_SIN[i % 4] * FRAC_1_SQRT_2;
            o.im = tim[i] * cos - tre[i] * sin;
            o.re = tim[i] * sin + tre[i] * cos;
            *o = *o * ANALYSIS_GAIN;
        }

        Ok(out)
    }
}

/// The CLDFB synthesis bank of one channel, with 32 or 64 bands.
#[derive(Debug, Clone)]
pub struct CldfbSynthesis {
    bands: usize,
    /// The polyphase state: 9 values per band.
    state: Vec<f64>,
    transform: TransformIv,
}

impl CldfbSynthesis {
    /// A synthesis bank with `bands` (32 or 64) bands.
    pub fn new(bands: usize) -> Result<Self> {
        if bands != 32 && bands != 64 {
            return Err(Error::SbrQmfInvalid);
        }
        Ok(CldfbSynthesis {
            bands,
            state: vec![0.0; (2 * TAPS_PER_BAND - 1) * bands],
            transform: TransformIv::new(bands),
        })
    }

    /// The number of bands, and of the output samples per slot.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.bands
    }

    /// Synthesize one slot of subband samples (the first `bands` of `x`), appending the
    /// `bands` output samples to `out`.
    pub fn push_slot(&mut self, x: &[Complex], out: &mut Vec<f64>) -> Result<()> {
        let l = self.bands;

        if x.len() < l {
            return Err(Error::SbrQmfInvalid);
        }

        // The phase rotation.
        let mut re = [0.0f64; 64];
        let mut im = [0.0f64; 64];
        for i in 0..l {
            let cos = SYN_COS[i % 4] * FRAC_1_SQRT_2;
            let sin = SYN_SIN[i % 4] * FRAC_1_SQRT_2;
            im[i] = x[i].im * cos - x[i].re * sin;
            re[i] = x[i].im * sin + x[i].re * cos;
        }

        let mut tre = [0.0f64; 64];
        let mut tim = [0.0f64; 64];
        self.transform.dct_dst(&re[..l], &im[..l], &mut tre, &mut tim);

        // The unfolding.
        for i in 0..l / 2 {
            let (r1, i2) = (tre[i], tim[l - 1 - i]);
            let (r2, i1) = (tre[l - 1 - i], tim[i]);
            tre[i] = (r1 - i1) / 2.0;
            tim[l - 1 - i] = -(r1 + i1) / 2.0;
            tre[l - 1 - i] = (r2 - i2) / 2.0;
            tim[i] = -(r2 + i2) / 2.0;
        }

        // The prototype filter, in the polyphase form. The state is by tap and band, so that the
        // bands (`m = l - 1 - j`) are filtered at once.
        let taps = synthesis_taps(l);

        let mut rre = [0.0f64; 64];
        let mut rim = [0.0f64; 64];
        for m in 0..l {
            rre[m] = tre[l - 1 - m];
            rim[m] = tim[l - 1 - m];
        }
        let (rre, rim) = (&rre[..l], &rim[..l]);

        let start = out.len();
        out.resize(start + l, 0.0);
        let out = &mut out[start..];

        let s = &mut self.state[..9 * l];

        for m in 0..l {
            out[l - 1 - m] = (s[m] + taps.p2[4 * l + m] * rre[m]) * SYNTHESIS_GAIN;
        }

        // `sta[t] = sta[t + 1] + coefficient·x`, with `p1·im` for even `t` and `p2·re` for odd.
        for t in 0..8 {
            let (head, tail) = s.split_at_mut((t + 1) * l);
            let dst = &mut head[t * l..];
            let src = &tail[..l];
            let (coef, x) = if t % 2 == 0 {
                (&taps.p1[(4 - t / 2) * l..(5 - t / 2) * l], rim)
            }
            else {
                (&taps.p2[(4 - t / 2 - 1) * l..(5 - t / 2 - 1) * l], rre)
            };
            for m in 0..l {
                dst[m] = src[m] + coef[m] * x[m];
            }
        }
        for m in 0..l {
            s[8 * l + m] = taps.p1[m] * rim[m];
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic, broadband test signal.
    fn signal(n: usize) -> Vec<f64> {
        let mut seed = 0x1234_5678u32;
        (0..n)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = f64::from((seed >> 16) as i16) / 32768.0;
                0.5 * noise + 0.3 * (i as f64 * 0.05).sin()
            })
            .collect()
    }

    /// Run `x` through the analysis bank and the synthesis bank with `bands` bands (the upper
    /// bands of a 64 band bank are zero).
    fn round_trip(x: &[f64], bands: usize) -> Vec<f64> {
        let mut ana = CldfbAnalysis::new();
        let mut syn = CldfbSynthesis::new(bands).unwrap();
        let mut out = Vec::new();
        for slot in x.chunks_exact(32) {
            let sub = ana.push_slot(slot).unwrap();
            let mut full = [Complex::default(); 64];
            full[..32].copy_from_slice(&sub);
            syn.push_slot(&full, &mut out).unwrap();
        }
        out
    }

    /// The (delay, gain, relative error) of the best alignment `y[ratio·n + delay] ≈ gain·x[n]`.
    fn align(
        x: &[f64],
        y: &[f64],
        ratio: usize,
        delays: core::ops::Range<usize>,
    ) -> (usize, f64, f64) {
        let mut best = (0, 0.0, f64::MAX);
        // Skip the start-up transient, and the tail.
        let (lo, hi) = (2048, x.len() - 1300);
        for delay in delays {
            let (mut xy, mut xx) = (0.0, 0.0);
            for n in lo..hi {
                xy += x[n] * y[ratio * n + delay];
                xx += x[n] * x[n];
            }
            let gain = xy / xx;
            let mut err = 0.0;
            for n in lo..hi {
                let d = y[ratio * n + delay] - gain * x[n];
                err += d * d;
            }
            err = (err / (gain * gain * xx)).sqrt();
            if err < best.2 {
                best = (delay, gain, err);
            }
        }
        best
    }

    #[test]
    fn analysis_synthesis_32_is_near_perfect_reconstruction() {
        let x = signal(32 * 200);
        let y = round_trip(&x, 32);
        let (delay, gain, err) = align(&x, &y, 1, 0..128);
        // The low delay: 32 samples, a single slot.
        assert_eq!(delay, 32);
        assert!((gain - 1.0).abs() < 1e-3, "gain {gain}");
        assert!(err < 1e-3, "error {err}");
    }

    #[test]
    fn analysis_synthesis_64_is_near_perfect_reconstruction() {
        // A signal below a quarter of the core rate, so that the upsampled output is free of
        // images, made of incommensurate sinusoids so that the delay is unambiguous.
        let x: Vec<f64> = (0..32 * 200)
            .map(|i| {
                (0..6)
                    .map(|j| {
                        let w = 0.043 + 0.1137 * f64::from(j);
                        (i as f64 * w + f64::from(j)).sin() / (1.0 + f64::from(j))
                    })
                    .sum()
            })
            .collect();
        let y = round_trip(&x, 64);
        let (delay, gain, err) = align(&x, &y, 2, 0..256);
        // The delay is a fraction of a sample at the output rate, which limits the error.
        assert!((64..=65).contains(&delay), "delay {delay}");
        assert!((gain - 1.0).abs() < 1e-2, "gain {gain}");
        assert!(err < 0.08, "error {err}");
    }

    #[test]
    fn analysis_amplitude_matches_the_qmf_of_ordinary_sbr() {
        use super::super::qmf::AnalysisQmf;

        // A sinusoid at the centre of band 5.
        let f = PI * 5.5 / 32.0;
        let s: Vec<f64> = (0..32 * 64).map(|i| (i as f64 * f).sin()).collect();
        let mut std_bank = AnalysisQmf::new();
        let mut bank = CldfbAnalysis::new();
        let (mut m_std, mut m_cldfb) = (0.0f64, 0.0f64);
        for (n, slot) in s.chunks_exact(32).enumerate() {
            let a = std_bank.push_slot(slot).unwrap();
            let c = bank.push_slot(slot).unwrap();
            if n > 20 {
                m_std = m_std.max(a[5].norm_sqr().sqrt());
                m_cldfb = m_cldfb.max(c[5].norm_sqr().sqrt());
            }
        }
        assert!((m_cldfb / m_std - 1.0).abs() < 0.01, "std {m_std}, cldfb {m_cldfb}");
    }

    #[test]
    fn synthesis_rejects_a_wrong_band_count() {
        assert!(CldfbSynthesis::new(40).is_err());
        assert!(CldfbAnalysis::new().push_slot(&[0.0; 31]).is_err());
    }
}
