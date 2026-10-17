// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The decorrelator of MPEG Surround 2-1-2 (ISO/IEC 23003-1 6.6.4): a delay and an all-pass
//! filter per hybrid band, followed by an energy adjustment.
//!
//! The filters and the adjustment follow the float decoder of libxaac (`ixheaacd_mps_decorr.c`,
//! Apache-2.0).

use symphonia_core::dsp::complex::Complex64;

use super::hybrid::HYBRID_BANDS;
use super::tables::HYB_TO_PARAM_28;

type C = Complex64;

/// The all-pass denominators of the four reverberation bands; the numerator is the reversal.
const DEN: [&[f64]; 4] = [
    &[
        1.000000, -0.314818, -0.256828, -0.173641, -0.115077, 0.000599, 0.033343, 0.122672,
        -0.356362, 0.128058, 0.089800,
    ],
    &[
        1.000000, -0.287137, -0.088940, 0.123204, -0.126111, 0.064218, 0.045768, -0.016264,
        -0.122100,
    ],
    &[1.000000, 0.129403, -0.032633, 0.035700],
    &[1.000000, 0.034742, -0.013000],
];

/// The delay of each reverberation band in time slots.
const DELAY: [usize; 4] = [11, 10, 5, 2];

/// The upper QMF band limits (plus one) of the reverberation bands, by `bsDecorrConfig`.
const SPLIT: [[usize; 4]; 3] = [[3, 15, 24, 65], [3, 50, 65, 65], [0, 15, 65, 65]];

/// The QMF band of each hybrid band.
fn qmf_band(hybrid: usize) -> usize {
    match hybrid {
        0..=5 => 0,
        6 | 7 => 1,
        8 | 9 => 2,
        _ => hybrid - 7,
    }
}

const MAX_DELAY: usize = 11;
const MAX_ORDER: usize = 10;

/// The smoothing of the energies in the adjustment.
const ALPHA: f64 = 0.8;
const GAMMA: f64 = 1.5;

struct Band {
    reverb: usize,
    /// The input of the last `delay` slots of the previous frame.
    history: [C; MAX_DELAY],
    state: [C; MAX_ORDER + 1],
}

pub(super) struct Decorrelator {
    bins: usize,
    bands: Vec<Band>,
    smooth_in: [f64; 28],
    smooth_out: [f64; 28],
    abs_thr: f64,
}

impl Decorrelator {
    pub fn new(bins: usize, config: u8, abs_thr: f64) -> Option<Self> {
        let split = SPLIT.get(usize::from(config))?;

        let mut bands = Vec::with_capacity(bins);

        for i in 0..bins {
            let mut reverb = 0;

            while reverb < 3 && qmf_band(i) + 1 >= split[reverb] {
                reverb += 1;
            }

            bands.push(Band {
                reverb,
                history: [C::default(); MAX_DELAY],
                state: [C::default(); MAX_ORDER + 1],
            });
        }

        Some(Decorrelator { bins, bands, smooth_in: [0.0; 28], smooth_out: [0.0; 28], abs_thr })
    }

    /// Restrict the processing to the first `bins` hybrid bands.
    pub fn set_bins(&mut self, bins: usize) {
        self.bins = bins.min(self.bands.len());
    }

    /// Decorrelate `input` into `output`.
    pub fn apply(&mut self, input: &[[C; HYBRID_BANDS]], output: &mut [[C; HYBRID_BANDS]]) {
        let slots = input.len();

        for idx in 0..self.bins {
            let band = &mut self.bands[idx];
            let delay = DELAY[band.reverb];
            let den = DEN[band.reverb];
            let order = den.len();

            for n in 0..slots {
                // The input delayed by `delay` slots.
                let x = if n < delay {
                    band.history[MAX_DELAY - delay + n]
                }
                else {
                    input[n - delay][idx]
                };

                let y = band.state[0] + x * den[order - 1];
                output[n][idx] = y;

                for j in 1..order {
                    // The numerator is the reversed denominator.
                    band.state[j - 1] = band.state[j] + x * den[order - 1 - j] - y * den[j];
                }
            }

            // Keep the last `MAX_DELAY` input slots of the history followed by the input.
            let mut hist = [C::default(); MAX_DELAY];

            for (k, h) in hist.iter_mut().enumerate() {
                let src = slots + k;
                *h = if src < MAX_DELAY { band.history[src] } else { input[src - MAX_DELAY][idx] };
            }

            band.history = hist;
        }

        self.adjust_energy(input, output);
    }

    fn adjust_energy(&mut self, input: &[[C; HYBRID_BANDS]], output: &mut [[C; HYBRID_BANDS]]) {
        for (inp, out) in input.iter().zip(output.iter_mut()) {
            let mut in_energy = [0.0f64; 28];
            let mut out_energy = [0.0f64; 28];

            for j in 0..self.bins {
                let k = usize::from(HYB_TO_PARAM_28[j]);
                in_energy[k] += inp[j].norm_sqr();
                out_energy[k] += out[j].norm_sqr();
            }

            let mut gain = [1.0f64; 28];

            for k in 0..28 {
                self.smooth_in[k] = self.smooth_in[k] * ALPHA + in_energy[k] * (1.0 - ALPHA);
                self.smooth_out[k] = self.smooth_out[k] * ALPHA + out_energy[k] * (1.0 - ALPHA);

                let (si, so) = (self.smooth_in[k], self.smooth_out[k]);

                if so > si * GAMMA {
                    gain[k] = (si * GAMMA / (so + self.abs_thr)).sqrt();
                }

                if si > so * GAMMA {
                    gain[k] = (si / (GAMMA * so + self.abs_thr)).sqrt().min(2.0);
                }
            }

            for j in 0..self.bins {
                out[j] *= gain[usize::from(HYB_TO_PARAM_28[j])];
            }
        }
    }
}
