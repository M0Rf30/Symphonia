// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MPEG Surround 2-1-2 of USAC (ISO/IEC 23003-3 7.11, ISO/IEC 23003-1).
//!
//! The tool upmixes the QMF-domain downmix of a channel pair element with one core channel into
//! two channels with the help of CLD, ICC and IPD parameters and a decorrelated signal. The
//! structure follows the float MPEG Surround decoder of libxaac (Apache-2.0, see the `NOTICE`
//! file). Residual coding, the temporal envelope tools and SBR after the upmix
//! (`stereoConfigIndex` 3) are not implemented.

mod decorr;
mod huffman;
mod hybrid;
mod params;
mod tables;

use symphonia_core::dsp::complex::Complex64;
use symphonia_core::errors::{Result, unsupported_error};
use symphonia_core::io::BitReaderLtr;

use symphonia_common::mpeg::audio::usac::UsacMps212Config;

use crate::aac::sbr::decoder::{QmfUpmix, UpmixedSlots};

use decorr::Decorrelator;
use hybrid::{HYBRID_BANDS, HybridAnalysis};
use params::Params;
use tables::*;

type C = Complex64;

/// The maximum number of parameter bands.
pub(super) const MAX_BANDS: usize = 28;

/// The maximum number of parameter sets of a frame, including the one that extends it.
pub(super) const MAX_SETS: usize = 9;

/// The floor of the decorrelator energies (`1e-9` at the full scale of 16 bit PCM).
const ABS_THR: f64 = 1e-9 * 32768.0 * 32768.0;

/// The clipping gains of `bsFixedGainDMX`.
const CLIP_GAIN: [f64; 8] = [
    1.0,
    1.189_207_115_002_721,
    core::f64::consts::SQRT_2,
    1.681_792_830_507_429,
    2.0,
    2.378_414_230_005_442,
    2.0 * core::f64::consts::SQRT_2,
    4.0,
];

/// The upmix matrices of one parameter set, per parameter band: the gains from the direct
/// signal and from the decorrelated signal to the two output channels.
#[derive(Clone, Copy, Default)]
struct Matrices {
    direct: [[f64; 2]; MAX_BANDS],
    diffuse: [[f64; 2]; MAX_BANDS],
}

/// The MPS212 upmixer of a channel pair.
pub struct Mps212Decoder {
    cfg: UsacMps212Config,
    params: Params,
    hyb_map: &'static [u8; HYBRID_BANDS],
    input_gain: f64,

    c_l: [f64; 31],
    cos_t: [[f64; 31]; 8],
    sin_t: [[f64; 31]; 8],

    analysis: HybridAnalysis,
    decor: Decorrelator,

    /// The matrices of the parameter sets of the frame and the last set of the previous frame.
    sets: [Matrices; MAX_SETS],
    prev: Matrices,
    /// The pre-matrix gain of each parameter set and band, and the last one of the previous frame.
    m1: [[f64; MAX_BANDS]; MAX_SETS],
    m1_prev: [f64; MAX_BANDS],
    /// The phases of the left and right channel.
    phase: [[[f64; 2]; MAX_BANDS]; MAX_SETS],
    phase_prev: [[f64; 2]; MAX_BANDS],
    /// The smoothed phases of the OPD smoothing.
    opd_smooth: [[f64; 2]; MAX_BANDS],
    /// The first frame is rendered through the pre-matrix.
    first_frame: bool,

    // Per frame work buffers.
    hyb_in: Vec<[C; HYBRID_BANDS]>,
    hyb_diff: Vec<[C; HYBRID_BANDS]>,
    hyb_dir: [Vec<[C; HYBRID_BANDS]>; 2],
    out: [Vec<[C; 64]>; 2],
}

impl Mps212Decoder {
    /// `time_slots` is the number of QMF slots of a frame.
    pub fn new(cfg: UsacMps212Config, time_slots: usize) -> Result<Self> {
        if cfg.residual_coding {
            return unsupported_error("aac: usac mps212 residual coding is not implemented");
        }

        if cfg.temp_shape_config != 0 {
            return unsupported_error("aac: usac mps212 temporal shaping is not implemented");
        }

        if time_slots == 0 || time_slots > 64 {
            return unsupported_error("aac: usac mps212 unsupported frame length");
        }

        let param_bands = usize::from(UsacMps212Config::NUM_BANDS[usize::from(cfg.freq_res)]);

        let hyb_map: &'static [u8; HYBRID_BANDS] = match param_bands {
            4 => &HYB_TO_PARAM_4,
            5 => &HYB_TO_PARAM_5,
            7 => &HYB_TO_PARAM_7,
            10 => &HYB_TO_PARAM_10,
            14 => &HYB_TO_PARAM_14,
            20 => &HYB_TO_PARAM_20,
            28 => &HYB_TO_PARAM_28,
            _ => return unsupported_error("aac: usac mps212 invalid frequency resolution"),
        };

        let Some(decor) = Decorrelator::new(HYBRID_BANDS, cfg.decorr_config, ABS_THR)
        else {
            return unsupported_error("aac: usac mps212 invalid decorrelator configuration");
        };

        // The OTT tables: the channel gains from the CLD and the rotation from the ICC
        // (ISO/IEC 23003-1 6.5.3.2).
        const CLD: [f64; 31] = [
            -150.0, -45.0, -40.0, -35.0, -30.0, -25.0, -22.0, -19.0, -16.0, -13.0, -10.0, -8.0,
            -6.0, -4.0, -2.0, 0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 13.0, 16.0, 19.0, 22.0, 25.0, 30.0,
            35.0, 40.0, 45.0, 150.0,
        ];
        const ICC: [f64; 8] = [1.0, 0.9370, 0.84118, 0.60092, 0.36764, 0.0, -0.5890, -0.9900];

        let mut c_l = [0.0; 31];
        let mut cos_t = [[0.0; 31]; 8];
        let mut sin_t = [[0.0; 31]; 8];

        for (i, cld) in CLD.iter().enumerate() {
            let p = 10f64.powf(cld / 10.0);
            let c1 = (p / (1.0 + p)).sqrt();
            let c2 = (1.0 / (1.0 + p)).sqrt();
            c_l[i] = c1;

            for (j, icc) in ICC.iter().enumerate() {
                let alpha = 0.5 * icc.acos();
                let beta = (alpha.tan() * (c2 - c1) / (c2 + c1)).atan();
                cos_t[j][i] = (alpha + beta).cos();
                sin_t[j][i] = (alpha + beta).sin();
            }
        }

        Ok(Mps212Decoder {
            cfg,
            params: Params::new(&cfg, time_slots, param_bands),
            hyb_map,
            input_gain: CLIP_GAIN[usize::from(cfg.fixed_gain_dmx & 7)],
            c_l,
            cos_t,
            sin_t,
            analysis: HybridAnalysis::new(),
            decor,
            sets: [Matrices::default(); MAX_SETS],
            prev: Matrices::default(),
            m1: [[1.0; MAX_BANDS]; MAX_SETS],
            m1_prev: [0.0; MAX_BANDS],
            phase: [[[0.0; 2]; MAX_BANDS]; MAX_SETS],
            phase_prev: [[0.0; 2]; MAX_BANDS],
            opd_smooth: [[0.0; 2]; MAX_BANDS],
            first_frame: true,
            hyb_in: vec![[C::default(); HYBRID_BANDS]; time_slots],
            hyb_diff: vec![[C::default(); HYBRID_BANDS]; time_slots],
            hyb_dir: [
                vec![[C::default(); HYBRID_BANDS]; time_slots],
                vec![[C::default(); HYBRID_BANDS]; time_slots],
            ],
            out: [vec![[C::default(); 64]; time_slots], vec![[C::default(); 64]; time_slots]],
        })
    }

    /// Parse the `Mps212Data()` of a frame.
    pub fn parse(&mut self, bs: &mut BitReaderLtr<'_>, independent: bool) -> Result<()> {
        self.params.parse(bs, independent)
    }

    /// Upmix the `band_count` lowest QMF bands of the downmix `qmf`. Returns the QMF slots of the
    /// two output channels.
    pub fn upmix_frame(&mut self, qmf: &[[C; 64]], band_count: usize) -> Result<UpmixedSlots<'_>> {
        let slots = self.params.time_slots;

        if qmf.len() != slots || !(4..=64).contains(&band_count) {
            return unsupported_error("aac: usac mps212 frame shape mismatch");
        }

        self.params.decode()?;
        self.calc_matrices();

        let nps = self.params.num_sets;
        let bands = self.params.param_bands;
        let n_hyb = band_count + 7;

        // Smooth the matrices.
        let mut smoothed = false;

        for ps in 0..nps {
            let delta = f64::from(self.params.slot_diff[ps]) / self.params.smoothing_time[ps];
            let one_minus = 1.0 - delta;

            for pb in 0..bands {
                if !self.params.smoothing_data[ps][pb] {
                    continue;
                }

                smoothed = true;

                let (m1_ref, direct_ref, diffuse_ref) = if ps == 0 {
                    (self.m1_prev[pb], self.prev.direct[pb], self.prev.diffuse[pb])
                }
                else {
                    (
                        self.m1[ps - 1][pb],
                        self.sets[ps - 1].direct[pb],
                        self.sets[ps - 1].diffuse[pb],
                    )
                };

                self.m1[ps][pb] = delta * self.m1[ps][pb] + one_minus * m1_ref;

                for row in 0..2 {
                    let s = &mut self.sets[ps];
                    s.direct[pb][row] = delta * s.direct[pb][row] + one_minus * direct_ref[row];
                    s.diffuse[pb][row] = delta * s.diffuse[pb][row] + one_minus * diffuse_ref[row];
                }
            }
        }

        let pre_mix = self.first_frame || smoothed;

        // Hybrid analysis of the gain-scaled downmix.
        let gain = self.input_gain;
        let mut scaled = [C::default(); 64];

        for (ts, slot) in qmf.iter().enumerate() {
            for (d, s) in scaled.iter_mut().zip(slot.iter()).take(band_count) {
                *d = *s * gain;
            }

            self.analysis.push_slot(&scaled[..band_count], &mut self.hyb_in[ts]);
        }

        // The pre-matrix, interpolated from the previous frame.
        if pre_mix {
            let mut ts = 0;
            let mut prev = self.m1_prev;

            for ps in 0..nps {
                let steps = self.params.slot_diff[ps] as usize;
                let inv = self.params.inv_slot_diff[ps];

                if ps > 0 && !self.cfg.high_rate_mode {
                    break;
                }

                let mut base = prev;
                let step: [f64; MAX_BANDS] =
                    core::array::from_fn(|pb| inv * (self.m1[ps][pb] - prev[pb]));

                for _ in 0..steps {
                    for pb in 0..bands {
                        base[pb] += step[pb];
                    }

                    for qs in 0..n_hyb {
                        self.hyb_in[ts][qs] *= base[usize::from(self.hyb_map[qs])];
                    }

                    ts += 1;
                }

                prev = self.m1[ps];
            }
        }

        // The decorrelated signal.
        self.decor.set_bins(n_hyb);
        self.decor.apply(&self.hyb_in[..slots], &mut self.hyb_diff[..slots]);

        // Interpolate the matrices and the phases per slot and mix.
        self.mix(n_hyb);

        // Synthesize the two output channels.
        for ch in 0..2 {
            for ts in 0..slots {
                let out = &mut self.out[ch][ts];
                *out = [C::default(); 64];
                hybrid::synthesis(&self.hyb_dir[ch][ts], &mut out[..band_count]);
            }
        }

        // Carry the state of the last parameter set over.
        self.m1_prev = self.m1[nps - 1];
        self.prev = self.sets[nps - 1];
        self.phase_prev = self.phase[nps - 1];
        self.first_frame = false;

        Ok((&self.out[0], &self.out[1]))
    }
}

/// Wrap an angle into `[0, 2 pi)`.
fn wrap_phase(mut phase: f64) -> f64 {
    let two_pi = 2.0 * core::f64::consts::PI;

    while phase < 0.0 {
        phase += two_pi;
    }

    while phase >= two_pi {
        phase -= two_pi;
    }

    phase
}

/// Interpolate between two angles along the shorter way: returns the angle at the fraction
/// `alpha` and the step from the first to the second.
fn angle_interpolation(mut a1: f64, mut a2: f64, alpha: f64) -> (f64, f64) {
    let pi = core::f64::consts::PI;

    while a2 - a1 > pi {
        a1 += 2.0 * pi;
    }

    while a1 - a2 > pi {
        a2 += 2.0 * pi;
    }

    ((1.0 - alpha) * a1 + alpha * a2, a2 - a1)
}

impl Mps212Decoder {
    /// `ixheaacd_pre_and_mix_matrix_calculation`: the upmix matrices and phases of every
    /// parameter set from the CLD, ICC and IPD indices.
    fn calc_matrices(&mut self) {
        let nps = self.params.num_sets;
        let bands = self.params.param_bands;
        let ipd_bands = self.params.num_bands_ipd.min(bands);
        let pi = core::f64::consts::PI;

        for ps in 0..nps {
            for pb in 0..bands {
                let cld = (self.params.cld_idx[ps][pb] + 15) as usize;
                let icc = (self.params.icc_idx[ps][pb] & 7) as usize;

                let cl = self.c_l[cld];
                let cr = self.c_l[30 - cld];

                self.sets[ps].direct[pb] =
                    [self.cos_t[icc][cld] * cl, self.cos_t[icc][30 - cld] * cr];
                self.sets[ps].diffuse[pb] =
                    [self.sin_t[icc][cld] * cl, -self.sin_t[icc][30 - cld] * cr];
                self.m1[ps][pb] = 1.0;
            }

            self.phase[ps] = [[0.0; 2]; MAX_BANDS];

            if self.cfg.phase_coding && self.params.phase_mode {
                for pb in 0..ipd_bands {
                    let cld = (self.params.cld_idx[ps][pb] + 15) as usize;
                    let ipd = (self.params.ipd_idx[ps][pb] & 15) as usize;
                    let icc = (self.params.icc_idx[ps][pb] & 7) as usize;

                    let opd = if cld == 15 && ipd == 8 {
                        0.0
                    }
                    else {
                        f64::from(ATAN_Q28[ipd][icc][cld]) / f64::from(1u32 << 28)
                    };

                    let ipd_angle = ipd as f64 * pi / 8.0;

                    self.phase[ps][pb] = [wrap_phase(opd), wrap_phase(opd - ipd_angle)];
                }
            }
        }

        self.smooth_opd();
    }

    /// `ixheaacd_mps_smoothing_opd`.
    fn smooth_opd(&mut self) {
        let nps = self.params.num_sets;
        let bands = self.params.param_bands;
        let pi = core::f64::consts::PI;

        if !self.params.opd_smoothing {
            for pb in 0..bands {
                self.opd_smooth[pb] = self.phase[nps - 1][pb];
            }

            return;
        }

        for ps in 0..nps {
            let thr = if self.params.ipd_coarse[ps] { 50.0 } else { 25.0 } * pi / 180.0;
            let delta = f64::from(self.params.slot_diff[ps]) / 128.0;

            for pb in 0..bands {
                let mut t = self.phase[ps][pb];

                for ch in 0..2 {
                    let s = self.opd_smooth[pb][ch];

                    while t[ch] > s + pi {
                        t[ch] -= 2.0 * pi;
                    }

                    while t[ch] < s - pi {
                        t[ch] += 2.0 * pi;
                    }

                    self.opd_smooth[pb][ch] = delta * t[ch] + (1.0 - delta) * s;
                }

                let mut tmp = (t[0] - t[1]) - (self.opd_smooth[pb][0] - self.opd_smooth[pb][1]);

                while tmp > pi {
                    tmp -= 2.0 * pi;
                }

                while tmp < -pi {
                    tmp += 2.0 * pi;
                }

                if tmp.abs() > thr {
                    self.opd_smooth[pb] = t;
                }

                for ch in 0..2 {
                    while self.opd_smooth[pb][ch] > 2.0 * pi {
                        self.opd_smooth[pb][ch] -= 2.0 * pi;
                    }

                    while self.opd_smooth[pb][ch] < 0.0 {
                        self.opd_smooth[pb][ch] += 2.0 * pi;
                    }
                }

                self.phase[ps][pb] = self.opd_smooth[pb];
            }
        }
    }

    /// Interpolate the matrices and phases to every slot and mix the direct and the decorrelated
    /// signal into the two output channels.
    fn mix(&mut self, n_hyb: usize) {
        let nps = self.params.num_sets;
        let bands = self.params.param_bands;
        let phased = self.cfg.phase_coding;

        let mut ts = 0;

        for ps in 0..nps {
            let inv = self.params.inv_slot_diff[ps];

            let (prev_matrices, prev_phase) = if ps == 0 {
                (self.prev, self.phase_prev)
            }
            else {
                (self.sets[ps - 1], self.phase[ps - 1])
            };

            let cur = self.sets[ps];

            let mut direct = prev_matrices.direct;
            let mut diffuse = prev_matrices.diffuse;
            let mut step_dir = [[0.0; 2]; MAX_BANDS];
            let mut step_dif = [[0.0; 2]; MAX_BANDS];
            let mut angle = [[0.0; 2]; MAX_BANDS];
            let mut angle_step = [[0.0; 2]; MAX_BANDS];

            for pb in 0..bands {
                for row in 0..2 {
                    step_dir[pb][row] = inv * (cur.direct[pb][row] - prev_matrices.direct[pb][row]);
                    step_dif[pb][row] =
                        inv * (cur.diffuse[pb][row] - prev_matrices.diffuse[pb][row]);

                    if phased {
                        let (a, step) =
                            angle_interpolation(prev_phase[pb][row], self.phase[ps][pb][row], inv);
                        angle[pb][row] = a;
                        angle_step[pb][row] = step * inv;
                    }
                }
            }

            for _ in 0..self.params.slot_diff[ps] {
                if ts >= self.params.time_slots {
                    return;
                }

                let mut rot = [[C::new(1.0, 0.0); 2]; MAX_BANDS];

                for pb in 0..bands {
                    for row in 0..2 {
                        direct[pb][row] += step_dir[pb][row];
                        diffuse[pb][row] += step_dif[pb][row];

                        if phased {
                            rot[pb][row] = C::new(angle[pb][row].cos(), angle[pb][row].sin());
                            angle[pb][row] += angle_step[pb][row];
                        }
                    }
                }

                for qs in 0..n_hyb {
                    let pb = usize::from(self.hyb_map[qs]);
                    let w_dir = self.hyb_in[ts][qs];
                    let w_dif = self.hyb_diff[ts][qs];

                    // The lowest two hybrid bands rotate the other way (ISO/IEC 23003-1 6.5.3.3).
                    let sign = if qs < 2 { 1.0 } else { -1.0 };

                    for row in 0..2 {
                        let (g_dir, g_dif) = (direct[pb][row], diffuse[pb][row]);

                        let mut out =
                            w_dir * (g_dir * rot[pb][row].re) + w_dif * (g_dif * rot[pb][row].re);

                        if phased {
                            let r_dir = g_dir * rot[pb][row].im;
                            let r_dif = g_dif * rot[pb][row].im;

                            out.re += sign * (w_dir.im * r_dir + w_dif.im * r_dif);
                            out.im -= sign * (w_dir.re * r_dir + w_dif.re * r_dif);
                        }

                        self.hyb_dir[row][ts][qs] = out;
                    }
                }

                ts += 1;
            }
        }
    }
}

impl QmfUpmix for Mps212Decoder {
    fn upmix(
        &mut self,
        qmf: &[[C; 64]],
        band_count: usize,
    ) -> core::result::Result<UpmixedSlots<'_>, ()> {
        self.upmix_frame(qmf, band_count).map_err(|_| ())
    }
}
