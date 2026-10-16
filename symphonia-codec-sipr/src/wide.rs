// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The 16 kbit/s mode (`sipr16k.c`): 16 kHz, two 80 sample subframes.

use std::f64::consts::{LN_2, LN_10, PI};

use crate::dsp::{
    Fixed, interpolate, lp_synthesis_filter, lp_synthesis_filter_in_place, lspd_to_lpc,
    scalar_product, set_min_dist, weighted_vector_sum,
};
use crate::tables::{
    FC_4PULSES_8BITS_TRACKS_13, GAIN_CB_16K, GAIN_PITCH_CB_16K, LSF_CB1_16K, LSF_CB2_16K,
    LSF_CB3_16K, LSF_CB4_16K, LSF_CB5_16K, MEAN_LSF_16K, PRED_16K, QU, SINC_WIN,
};
use crate::{Params, Reader};

const ORDER: usize = 16;
const SUBFR: usize = 80;
const PITCH_MIN: i32 = 30;
const PITCH_MAX: i32 = 281;
const L_INTERPOL: usize = 11;

/// The size of the excitation buffer: history, and the two subframes of a frame.
const EXCITATION_LEN: usize = L_INTERPOL + PITCH_MAX as usize + 2 * SUBFR;

/// The minimum distance of the LSFs, 0.0125 pi.
const LSFQ_DIFF_MIN: f64 = 0.0125 * PI;

/// `2^-(i + 1)`: the weights of the post-filter.
const POW_0_5: [f32; 16] = [
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
    1.0 / 2048.0,
    1.0 / 4096.0,
    1.0 / 8192.0,
    1.0 / 16384.0,
    1.0 / 32768.0,
    1.0 / 65536.0,
];

pub const BITS_PER_FRAME: usize = 160;
pub const SAMPLES_PER_FRAME: usize = 2 * SUBFR;

const VQ_BITS: [u32; 5] = [7, 8, 7, 7, 7];
const PITCH_DELAY_BITS: [u32; 2] = [9, 6];
const FC_BITS: [u32; 10] = [4, 5, 4, 5, 4, 5, 4, 5, 4, 5];

/// Reads the parameters of the frame (`decode_parameters`).
pub fn read_params(r: &mut Reader<'_>, out: &mut Params) {
    out.ma_pred_switch = r.bits(1) as usize;

    for i in 0..5 {
        out.vq_indexes[i] = r.bits(VQ_BITS[i]) as usize;
    }

    for i in 0..2 {
        out.pitch_delay[i] = r.bits(PITCH_DELAY_BITS[i]) as i32;
        out.gp_index[i] = r.bits(4) as usize;

        for j in 0..10 {
            out.fc_indexes[i][j] = r.bits(FC_BITS[j]) as i32;
        }

        out.gc_index[i] = r.bits(5) as usize;
    }
}

/// The first subframe's pitch delay in thirds (`dec_delay3_1st`).
fn delay3_first(index: i32) -> i32 {
    if index < 390 { index + 88 } else { 3 * index - 690 }
}

/// The second subframe's pitch delay in thirds (`dec_delay3_2nd`).
fn delay3_second(index: i32, prev_lag: i32) -> i32 {
    if index < 62 {
        let min = (prev_lag - 10).clamp(PITCH_MIN, PITCH_MAX - 19);
        3 * min + index - 2
    }
    else {
        3 * prev_lag
    }
}

/// `x / 3` for small non-negative `x`.
fn div3(x: i32) -> i32 {
    (x * 10923) >> 15
}

/// The decoder state of the 16 kbit/s mode.
pub struct Wide {
    lsf_history: [f32; ORDER],
    excitation: [f32; EXCITATION_LEN],
    synth_buf: [f32; ORDER + SAMPLES_PER_FRAME + 6],
    energy_history: [f32; 4],
    pitch_lag_prev: i32,
    iir_mem: [f32; ORDER + 1],
    filt_buf: [[f32; ORDER + 1]; 2],
    /// Which of the two buffers of `filt_buf` is `filt_mem[0]` of the reference.
    filt_cur: usize,
    mem_preemph: [f32; ORDER],
    synth: [f32; ORDER],
    lsp_history: [f64; ORDER],
}

impl Wide {
    pub fn new() -> Self {
        let mut lsp_history = [0.0; ORDER];

        for (i, l) in lsp_history.iter_mut().enumerate() {
            *l = (((i + 1) as f64) * PI / (ORDER + 1) as f64).cos();
        }

        Wide {
            lsf_history: [0.0; ORDER],
            excitation: [0.0; EXCITATION_LEN],
            synth_buf: [0.0; ORDER + SAMPLES_PER_FRAME + 6],
            energy_history: [-14.0; 4],
            pitch_lag_prev: 180,
            iir_mem: [0.0; ORDER + 1],
            filt_buf: [[0.0; ORDER + 1]; 2],
            filt_cur: 0,
            mem_preemph: [0.0; ORDER],
            synth: [0.0; ORDER],
            lsp_history,
        }
    }

    /// Decodes the LSFs (`lsf_decode_fp_16k`).
    fn lsf_decode(&mut self, vq: &[usize; 5], ma_pred: usize) -> [f32; ORDER] {
        let mut q = [0.0f32; ORDER];

        q[0..3].copy_from_slice(&LSF_CB1_16K[vq[0]]);
        q[3..6].copy_from_slice(&LSF_CB2_16K[vq[1]]);
        q[6..9].copy_from_slice(&LSF_CB3_16K[vq[2]]);
        q[9..12].copy_from_slice(&LSF_CB4_16K[vq[3]]);
        q[12..16].copy_from_slice(&LSF_CB5_16K[vq[4]]);

        let mut lsf = [0.0f32; ORDER];

        for i in 0..ORDER {
            lsf[i] =
                (1.0 - QU[ma_pred]) * q[i] + QU[ma_pred] * self.lsf_history[i] + MEAN_LSF_16K[i];
        }

        self.lsf_history = q;
        lsf
    }

    /// The post-filter (`postfilter`). `synth[ORDER..]` is the synthesis of the frame, preceded by
    /// its memory.
    fn postfilter(&mut self, out: &mut [f32]) {
        let (cur, prev) = (self.filt_cur, 1 - self.filt_cur);

        for i in 0..ORDER {
            self.filt_buf[cur][i] = self.iir_mem[i] * POW_0_5[i];
        }

        // `tmpbuf` is the unfiltered-by-the-new-coefficients path of the cross-fade.
        let mut buf = [0.0f32; 30 + ORDER];
        buf[..ORDER].copy_from_slice(&self.mem_preemph);

        lp_synthesis_filter(
            &mut buf,
            &self.filt_buf[prev][..ORDER],
            &self.synth_buf[ORDER..ORDER + 30],
            30,
        );

        self.synth_buf[..ORDER].copy_from_slice(&self.mem_preemph);
        lp_synthesis_filter_in_place(&mut self.synth_buf, &self.filt_buf[cur][..ORDER], 30);

        let synth = &self.synth_buf[ORDER..];

        out[30 - ORDER..30].copy_from_slice(&synth[30 - ORDER..30]);
        lp_synthesis_filter(
            &mut out[30 - ORDER..],
            &self.filt_buf[cur][..ORDER],
            &synth[30..],
            2 * SUBFR - 30,
        );

        self.mem_preemph.copy_from_slice(&out[2 * SUBFR - ORDER..2 * SUBFR]);

        self.filt_cur = prev;

        let mut s = 0.0f32;

        for i in 0..30 {
            out[i] = buf[ORDER + i] + s * (synth[i] - buf[ORDER + i]);
            s = (f64::from(s) + 1.0 / 30.0) as f32;
        }
    }

    /// Decodes one frame of 160 samples (`ff_sipr_decode_frame_16k`).
    pub fn decode_frame(&mut self, params: &Params, out: &mut [f32]) {
        let mut lsf_new = self.lsf_decode(&params.vq_indexes, params.ma_pred_switch);

        set_min_dist(&mut lsf_new, LSFQ_DIFF_MIN / 2.0);

        let mut lsp_new = [0.0f64; ORDER];

        for i in 0..ORDER {
            lsp_new[i] = f64::from(lsf_new[i].cos());
        }

        // The LPCs of both subframes (`acelp_lp_decodef`).
        let mut az = [[0.0f32; ORDER]; 2];
        let mut lsp_1st = [0.0f64; ORDER];

        for i in 0..ORDER {
            lsp_1st[i] = (lsp_new[i] + self.lsp_history[i]) * 0.5;
        }

        lspd_to_lpc(&lsp_1st, &mut az[0], ORDER >> 1);
        lspd_to_lpc(&lsp_new, &mut az[1], ORDER >> 1);

        self.lsp_history = lsp_new;

        self.synth_buf[..ORDER].copy_from_slice(&self.synth);

        // The excitation of the frame starts after the history of the longest pitch lag.
        let exc = 292;

        for i in 0..2 {
            let start = exc + i * SUBFR;

            let delay_3x = if i == 0 {
                delay3_first(params.pitch_delay[0])
            }
            else {
                delay3_second(params.pitch_delay[i], self.pitch_lag_prev)
            };

            let pitch_fac = GAIN_PITCH_CB_16K[params.gp_index[i]];

            let mut f = Fixed {
                pitch_fac: pitch_fac.min(1.0),
                pitch_lag: div3(delay_3x + 1) as usize,
                ..Fixed::default()
            };

            self.pitch_lag_prev = div3(delay_3x + 1);

            let delay_int = div3(delay_3x + 2);
            let delay_frac = delay_3x + 2 - 3 * delay_int;

            interpolate(
                &mut self.excitation,
                start,
                start + 1 - delay_int as usize,
                &SINC_WIN,
                3,
                (delay_frac + 1) as usize,
                10,
                SUBFR,
            );

            // The ten pulses (`ff_decode_10_pulses_35bits`).
            let fc = &params.fc_indexes[i];

            f.n = 10;

            for p in 0..5 {
                let pos1 = FC_4PULSES_8BITS_TRACKS_13[(fc[2 * p + 1] & 15) as usize] as usize + p;
                let pos2 = FC_4PULSES_8BITS_TRACKS_13[(fc[2 * p] & 15) as usize] as usize + p;
                let sign = if fc[2 * p + 1] & 16 != 0 { -1.0 } else { 1.0 };

                f.x[2 * p + 1] = pos1;
                f.x[2 * p] = pos2;
                f.y[2 * p + 1] = sign;
                f.y[2 * p] = if pos2 < pos1 { -sign } else { sign };
            }

            let mut fixed_vector = [0.0f32; SUBFR];
            f.set_vector(&mut fixed_vector, 1.0);

            // The gain (`acelp_decode_gain_codef`).
            let gain_corr_factor = GAIN_CB_16K[params.gc_index[i]];

            let mut mr_energy = (19.0 - 15.0 / (0.05 * LN_10 / LN_2)) as f32;
            mr_energy += scalar_product(&PRED_16K, &self.energy_history[..2]);

            let gain_code = gain_corr_factor
                * ((f64::from((SUBFR as f64).sqrt() as f32)
                    * (LN_10 / 20.0 * f64::from(mr_energy)).exp()
                    / (0.01 + f64::from(scalar_product(&fixed_vector, &fixed_vector))).sqrt())
                    as f32);

            self.energy_history[1] = self.energy_history[0];
            self.energy_history[0] = (20.0 * f64::from(gain_corr_factor).log10()) as f32;

            weighted_vector_sum(&mut self.excitation, start, &fixed_vector, pitch_fac, gain_code);

            let mut e = [0.0f32; SUBFR];
            e.copy_from_slice(&self.excitation[start..start + SUBFR]);
            let at = ORDER + i * SUBFR - ORDER;

            lp_synthesis_filter(&mut self.synth_buf[at..at + ORDER + SUBFR], &az[i], &e, SUBFR);
        }

        self.synth.copy_from_slice(&self.synth_buf[SAMPLES_PER_FRAME..SAMPLES_PER_FRAME + ORDER]);

        self.excitation.copy_within(2 * SUBFR..2 * SUBFR + L_INTERPOL + PITCH_MAX as usize, 0);

        self.postfilter(out);

        self.iir_mem[..ORDER].copy_from_slice(&az[1]);
    }
}
