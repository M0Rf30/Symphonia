// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The 8.5, 6.5 and 5.0 kbit/s modes (`sipr.c`): 8 kHz, 48 sample subframes.

use std::f64::consts::{LN_2, LN_10, PI};

use crate::dsp::{
    self, Fixed, adaptive_gain_control, amrwb_lsp_to_lpc, decode_pitch_lag, interpolate,
    lp_synthesis_filter, lp_zero_synthesis_filter, scalar_product, set_fixed_gain, set_min_dist,
    sort_nearly_sorted, tilt_compensation, weighted_vector_sum,
};
use crate::tables::{
    B60_SINC, GAIN_CB, LSF_CB1, LSF_CB2, LSF_CB3, LSF_CB4, LSF_CB5, MEAN_LSF, POW_0_7, POW_0_55,
    POW_0_75, PRED,
};
use crate::{Params, Reader};

const ORDER: usize = 10;
const SUBFR_SIZE: usize = 48;
const MAX_SUBFRAMES: usize = 5;

/// Past samples needed by the excitation interpolation.
const L_INTERPOL: usize = ORDER + 1;
const PITCH_DELAY_MAX: usize = 143;

/// The size of the excitation buffer: its history and the 5 subframes of a frame.
const EXCITATION_LEN: usize = L_INTERPOL + 281 + 2 * 80;

/// The minimum distance of the LSFs, 0.0125 pi.
const LSFQ_DIFF_MIN: f64 = 0.0125 * PI;

/// `2^-(i + 1)`: the numerator weights of the 5.0 kbit/s post-filter.
const POW_0_5: [f32; 10] = [
    1.0 / 2.0,
    1.0 / 4.0,
    1.0 / 8.0,
    1.0 / 16.0,
    1.0 / 32.0,
    1.0 / 64.0,
    1.0 / 128.0,
    1.0 / 256.0,
    1.0 / 512.0,
    1.0 / 1024.0,
];

/// The modes that share this decoder.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    M8k5,
    M6k5,
    M5k0,
}

/// The parameters of a mode (`SiprModeParam`).
pub struct ModeParam {
    pub bits_per_frame: usize,
    pub subframe_count: usize,
    pub frames_per_packet: usize,
    pitch_sharp_factor: f32,
    fc_count: usize,
    vq_bits: [u32; 5],
    pitch_delay_bits: [u32; 5],
    fc_bits: [u32; 3],
    gc_bits: u32,
}

impl Mode {
    pub fn param(self) -> ModeParam {
        match self {
            Mode::M8k5 => ModeParam {
                bits_per_frame: 152,
                subframe_count: 3,
                frames_per_packet: 1,
                pitch_sharp_factor: 0.8,
                fc_count: 3,
                vq_bits: [6, 7, 7, 7, 5],
                pitch_delay_bits: [8, 5, 5, 0, 0],
                fc_bits: [9, 9, 9],
                gc_bits: 7,
            },
            Mode::M6k5 => ModeParam {
                bits_per_frame: 232,
                subframe_count: 3,
                frames_per_packet: 2,
                pitch_sharp_factor: 0.8,
                fc_count: 3,
                vq_bits: [6, 7, 7, 7, 5],
                pitch_delay_bits: [8, 5, 5, 0, 0],
                fc_bits: [5, 5, 5],
                gc_bits: 7,
            },
            Mode::M5k0 => ModeParam {
                bits_per_frame: 296,
                subframe_count: 5,
                frames_per_packet: 2,
                pitch_sharp_factor: 0.85,
                fc_count: 1,
                vq_bits: [6, 7, 7, 7, 5],
                pitch_delay_bits: [8, 5, 8, 5, 5],
                fc_bits: [10, 0, 0],
                gc_bits: 7,
            },
        }
    }
}

/// Reads the parameters of one frame (`decode_parameters`).
pub fn read_params(r: &mut Reader<'_>, p: &ModeParam, out: &mut Params) {
    for i in 0..5 {
        out.vq_indexes[i] = r.bits(p.vq_bits[i]) as usize;
    }

    for i in 0..p.subframe_count {
        out.pitch_delay[i] = r.bits(p.pitch_delay_bits[i]) as i32;

        for j in 0..p.fc_count {
            out.fc_indexes[i][j] = r.bits(p.fc_bits[j]) as i32;
        }

        out.gc_index[i] = r.bits(p.gc_bits) as usize;
    }
}

/// The decoder state of the 8.5, 6.5 and 5.0 kbit/s modes (the relevant part of `SiprContext`).
pub struct Low {
    mode: Mode,
    param: ModeParam,
    past_pitch_gain: f32,
    lsf_history: [f32; ORDER],
    excitation: [f32; EXCITATION_LEN],
    synth_buf: [f32; ORDER + 5 * SUBFR_SIZE + 6],
    lsp_history: [f32; ORDER],
    gain_mem: f32,
    energy_history: [f32; 4],
    highpass_mem: [f32; 2],
    postfilter_mem: [f32; ORDER],
    tilt_mem: f32,
    postfilter_agc: f32,
    postfilter_mem5k0: [f32; ORDER],
    postfilter_syn5k0: [f32; ORDER + SUBFR_SIZE * 5],
}

impl Low {
    pub fn new(mode: Mode) -> Self {
        let mut lsp_history = [0.0; ORDER];

        for (i, l) in lsp_history.iter_mut().enumerate() {
            *l = (((i + 1) as f64) * PI / (ORDER + 1) as f64).cos() as f32;
        }

        Low {
            mode,
            param: mode.param(),
            past_pitch_gain: 0.0,
            lsf_history: [0.0; ORDER],
            excitation: [0.0; EXCITATION_LEN],
            synth_buf: [0.0; ORDER + 5 * SUBFR_SIZE + 6],
            lsp_history,
            gain_mem: 0.0,
            energy_history: [-14.0; 4],
            highpass_mem: [0.0; 2],
            postfilter_mem: [0.0; ORDER],
            tilt_mem: 0.0,
            postfilter_agc: 0.0,
            postfilter_mem5k0: [0.0; ORDER],
            postfilter_syn5k0: [0.0; ORDER + SUBFR_SIZE * 5],
        }
    }

    pub fn param(&self) -> &ModeParam {
        &self.param
    }

    /// Decodes the LSFs into the cosine domain (`lsf_decode_fp`).
    fn lsf_decode(&mut self, vq: &[usize; 5]) -> [f32; ORDER] {
        let mut tmp = [0.0f32; ORDER];

        tmp[0..2].copy_from_slice(&LSF_CB1[vq[0]]);
        tmp[2..4].copy_from_slice(&LSF_CB2[vq[1]]);
        tmp[4..6].copy_from_slice(&LSF_CB3[vq[2]]);
        tmp[6..8].copy_from_slice(&LSF_CB4[vq[3]]);
        tmp[8..10].copy_from_slice(&LSF_CB5[vq[4]]);

        let mut lsf = [0.0f32; ORDER];

        for i in 0..ORDER {
            lsf[i] = (f64::from(self.lsf_history[i]) * 0.33
                + f64::from(tmp[i])
                + f64::from(MEAN_LSF[i])) as f32;
        }

        sort_nearly_sorted(&mut lsf[..ORDER - 1]);

        // A minimum distance is not enforced between the last value and the previous one.
        set_min_dist(&mut lsf[..ORDER - 1], LSFQ_DIFF_MIN);
        lsf[9] = f64::from(lsf[9]).min(1.3 * PI) as f32;

        self.lsf_history = tmp;

        for l in &mut lsf[..ORDER - 1] {
            *l = f64::from(*l).cos() as f32;
        }

        lsf[ORDER - 1] = (f64::from(lsf[ORDER - 1]) * (6.153848 / PI)) as f32;
        lsf
    }

    /// Decodes the sparse fixed vector (`decode_fixed_sparse`).
    fn decode_fixed_sparse(&self, pulses: &[i32; 10], low_gain: bool) -> Fixed {
        let mut f = Fixed::default();

        match self.mode {
            Mode::M6k5 => {
                for i in 0..3 {
                    f.x[i] = (3 * (pulses[i] & 0xf) + i as i32) as usize;
                    f.y[i] = if pulses[i] & 0x10 != 0 { -1.0 } else { 1.0 };
                }

                f.n = 3;
            }
            Mode::M8k5 => {
                for i in 0..3 {
                    f.x[2 * i] = (3 * ((pulses[i] >> 4) & 0xf) + i as i32) as usize;
                    f.x[2 * i + 1] = (3 * (pulses[i] & 0xf) + i as i32) as usize;

                    f.y[2 * i] = if pulses[i] & 0x100 != 0 { -1.0 } else { 1.0 };
                    f.y[2 * i + 1] =
                        if f.x[2 * i + 1] < f.x[2 * i] { -f.y[2 * i] } else { f.y[2 * i] };
                }

                f.n = 6;
            }
            Mode::M5k0 => {
                if low_gain {
                    let offset = if pulses[0] & 0x200 != 0 { 2 } else { 0 };
                    let mut val = pulses[0];

                    for i in 0..3 {
                        let index = (val & 0x7) * 6 + 4 - i * 2;

                        f.y[i as usize] = if (offset + index) & 0x3 != 0 { -1.0 } else { 1.0 };
                        f.x[i as usize] = index as usize;

                        val >>= 3;
                    }

                    f.n = 3;
                }
                else {
                    let subset = (pulses[0] >> 8) & 1;

                    f.x[0] = (((pulses[0] >> 4) & 15) * 3 + subset) as usize;
                    f.x[1] = ((pulses[0] & 15) * 3 + subset + 1) as usize;

                    f.y[0] = if pulses[0] & 0x200 != 0 { -1.0 } else { 1.0 };
                    f.y[1] = -f.y[0];
                    f.n = 2;
                }
            }
        }

        f
    }

    /// Evaluates the adaptive impulse response of a subframe (`eval_ir`), into `ir[ORDER..]`.
    fn eval_ir(az: &[f32], pitch_lag: usize, ir: &mut [f32; ORDER + SUBFR_SIZE], sharp: f32) {
        let mut tmp1 = [0.0f32; SUBFR_SIZE + 1];
        let mut tmp2 = [0.0f32; ORDER + 1];

        tmp1[0] = 1.0;

        for i in 0..ORDER {
            tmp1[i + 1] = az[i] * POW_0_55[i];
            tmp2[i] = az[i] * POW_0_7[i];
        }

        lp_synthesis_filter(ir, &tmp2[..ORDER], &tmp1, SUBFR_SIZE);

        // Pitch sharpening (AMR section 6.1.2).
        let freq = &mut ir[ORDER..];

        for i in pitch_lag..SUBFR_SIZE {
            freq[i] += sharp * freq[i - pitch_lag];
        }
    }

    /// The post-filter of the 5.0 kbit/s mode (`postfilter_5k0`), in place.
    fn postfilter_5k0(&mut self, lpc: &[f32], samples: &mut [f32; SUBFR_SIZE]) {
        let mut buf = [0.0f32; SUBFR_SIZE + ORDER];
        let mut lpc_n = [0.0f32; ORDER];
        let mut lpc_d = [0.0f32; ORDER];

        for i in 0..ORDER {
            lpc_d[i] = lpc[i] * POW_0_75[i];
            lpc_n[i] = lpc[i] * POW_0_5[i];
        }

        buf[..ORDER].copy_from_slice(&self.postfilter_mem);
        lp_synthesis_filter(&mut buf, &lpc_d, samples, SUBFR_SIZE);
        self.postfilter_mem.copy_from_slice(&buf[SUBFR_SIZE..]);

        tilt_compensation(&mut self.tilt_mem, 0.4, &mut buf[ORDER..]);

        buf[..ORDER].copy_from_slice(&self.postfilter_mem5k0);
        self.postfilter_mem5k0.copy_from_slice(&buf[SUBFR_SIZE..]);

        lp_zero_synthesis_filter(samples, &lpc_n, &buf, SUBFR_SIZE);
    }

    /// Decodes one frame of `subframe_count * 48` samples (`decode_frame`).
    pub fn decode_frame(&mut self, params: &Params, out: &mut [f32]) {
        let subframe_count = self.param.subframe_count;
        let frame_size = subframe_count * SUBFR_SIZE;
        let sharp = self.param.pitch_sharp_factor;
        let is_5k0 = self.mode == Mode::M5k0;

        let mut ir = [0.0f32; ORDER + SUBFR_SIZE];

        let lsf_new = self.lsf_decode(&params.vq_indexes);

        // Interpolate the LSPs of each subframe and convert to LPCs (`sipr_decode_lp`).
        let mut az = [0.0f32; ORDER * MAX_SUBFRAMES];
        let t0 = (1.0 / subframe_count as f64) as f32;
        let mut t = t0 * 0.5;

        for az in az.chunks_exact_mut(ORDER).take(subframe_count) {
            let mut lsfint = [0.0f64; ORDER];

            for j in 0..ORDER {
                lsfint[j] = f64::from(self.lsp_history[j] * (1.0 - t) + t * lsf_new[j]);
            }

            amrwb_lsp_to_lpc(&lsfint, az, ORDER);
            t += t0;
        }

        self.lsp_history = lsf_new;

        // Position of the current subframe's excitation.
        let mut exc = PITCH_DELAY_MAX + L_INTERPOL;

        let mut t0_first = 0;

        for i in 0..subframe_count {
            let paz = &az[i * ORDER..(i + 1) * ORDER];
            let mut fixed_vector = [0.0f32; SUBFR_SIZE];

            let (lag, lag_frac) = decode_pitch_lag(params.pitch_delay[i], t0_first, i, is_5k0);

            if i == 0 || (i == 2 && is_5k0) {
                t0_first = lag;
            }

            let lag_u = lag as usize;
            let src = exc - lag_u + usize::from(lag_frac <= 0);
            let frac_pos = 2 * ((2 + lag_frac).rem_euclid(3) + 1) as usize;

            interpolate(&mut self.excitation, exc, src, &B60_SINC, 6, frac_pos, ORDER, SUBFR_SIZE);

            let low_gain = self.past_pitch_gain < 0.8;
            let fixed_cb = self.decode_fixed_sparse(&params.fc_indexes[i], low_gain);

            Self::eval_ir(paz, lag_u, &mut ir, sharp);

            // The convolution of the impulse response with the pulses.
            for j in 0..fixed_cb.n {
                for k in fixed_cb.x[j]..SUBFR_SIZE {
                    fixed_vector[k] += fixed_cb.y[j] * ir[ORDER + k - fixed_cb.x[j]];
                }
            }

            let avg_energy = ((0.01 + f64::from(scalar_product(&fixed_vector, &fixed_vector)))
                / SUBFR_SIZE as f64) as f32;

            let gc = &GAIN_CB[params.gc_index[i]];
            let mut pitch_gain = gc[0];
            self.past_pitch_gain = pitch_gain;

            let mut gain_code = set_fixed_gain(
                gc[1],
                avg_energy,
                &mut self.energy_history,
                (34.0 - 15.0 / (0.05 * LN_10 / LN_2)) as f32,
                &PRED,
            );

            weighted_vector_sum(&mut self.excitation, exc, &fixed_vector, pitch_gain, gain_code);

            pitch_gain = (f64::from(pitch_gain) * (0.5 * f64::from(pitch_gain))) as f32;
            pitch_gain = f64::from(pitch_gain).min(0.4) as f32;

            self.gain_mem = (0.7 * f64::from(self.gain_mem) + 0.3 * f64::from(pitch_gain)) as f32;
            self.gain_mem = self.gain_mem.min(pitch_gain);
            gain_code *= self.gain_mem;

            for j in 0..SUBFR_SIZE {
                fixed_vector[j] = self.excitation[exc + j] - gain_code * fixed_vector[j];
            }

            if is_5k0 {
                self.postfilter_5k0(paz, &mut fixed_vector);

                let at = i * SUBFR_SIZE;
                let mut e = [0.0f32; SUBFR_SIZE];
                e.copy_from_slice(&self.excitation[exc..exc + SUBFR_SIZE]);

                lp_synthesis_filter(
                    &mut self.postfilter_syn5k0[at..at + ORDER + SUBFR_SIZE],
                    paz,
                    &e,
                    SUBFR_SIZE,
                );
            }

            // The synthesis memory is `ORDER` samples before the subframe; the buffer holds 6
            // spare samples before it for alignment in the reference.
            let at = 16 + i * SUBFR_SIZE - ORDER;
            lp_synthesis_filter(
                &mut self.synth_buf[at..at + ORDER + SUBFR_SIZE],
                paz,
                &fixed_vector,
                SUBFR_SIZE,
            );

            exc += SUBFR_SIZE;
        }

        self.synth_buf.copy_within(16 + frame_size - ORDER..16 + frame_size, 16 - ORDER);

        if is_5k0 {
            for i in 0..subframe_count {
                let syn = &self.postfilter_syn5k0[ORDER + i * SUBFR_SIZE..][..SUBFR_SIZE];
                let energy = scalar_product(syn, syn);

                adaptive_gain_control(
                    &mut self.synth_buf[16 + i * SUBFR_SIZE..][..SUBFR_SIZE],
                    energy,
                    0.9,
                    &mut self.postfilter_agc,
                );
            }

            self.postfilter_syn5k0.copy_within(frame_size..frame_size + ORDER, 0);
        }

        self.excitation.copy_within(exc - PITCH_DELAY_MAX - L_INTERPOL..exc, 0);

        dsp::order_2_transfer_function(
            &mut out[..frame_size],
            &self.synth_buf[16..16 + frame_size],
            [-1.99997, 1.000000000],
            [-1.93307352, 0.935891986],
            0.939805806,
            &mut self.highpass_mem,
        );
    }
}
