// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The synthesis of the linear-prediction-domain frames of USAC: ACELP sub-frames, TCX frames
//! and the bass post-filter.
//!
//! Derived from libxaac (`ixheaacd_lpc.c`, `ixheaacd_acelp_decode.c`,
//! `ixheaacd_tcx_fwd_alcnx.c`, Apache-2.0).

use core::f64::consts::PI;

use symphonia_core::errors::Result;
use symphonia_core::io::BitReaderLtr;

use super::acelp::{self, INTER_LP_FIL_ORDER};
use super::dct4::Dct4;
use super::lpc::{self, ORDER};
use super::tables::FIR_LP_FILT;
use super::{EXC_HIST, LEN_SUBFR, LEN_SUPERFRAME, LpdFrame, MAX_PITCH, PREEMPH};
use crate::aac::usac::arith::ArithState;

const FILTER_DELAY: usize = 12;
const TMIN: i32 = 34;
const TFR2: i32 = 162 - TMIN;
const TFR1: i32 = 160;
const TMAX: i32 = 27 + 6 * TMIN;
const FSCALE_DENOM: i32 = 12800;
const TILT_CODE: f32 = 0.3;
const GAMMA: f32 = 0.92;

/// The longest FAC window: `2 * (ccfl / 8)`.
const FAC_LENGTH: usize = 128;

/// `sin(pi (i + 1/2) / (2 n))`, the first quarter of a sine window of `2 n` samples; `n` lines.
fn sine_window(n: usize) -> Vec<f32> {
    (0..n).map(|i| (PI * (i as f64 + 0.5) / (2 * n) as f64).sin() as f32).collect()
}

fn preemph(sig: &mut [f32], mu: f32, mem: f32) {
    for i in (1..sig.len()).rev() {
        sig[i] -= mu * sig[i - 1];
    }
    sig[0] -= mu * mem;
}

fn deemph(sig: &mut [f32], mem: f32) {
    sig[0] += PREEMPH * mem;
    for i in 1..sig.len() {
        sig[i] += PREEMPH * sig[i - 1];
    }
}

/// Residual of `x` (read at `x0` with `ORDER` samples of history) into `y[y0..]`, `loops` blocks
/// of `l` samples. The coefficient set advances per block if `advance`.
#[allow(clippy::too_many_arguments)]
fn residual(
    a: &[f32],
    advance: bool,
    x: &[f32],
    x0: usize,
    y: &mut [f32],
    y0: usize,
    l: usize,
    loops: usize,
) {
    let mut a0 = 0;
    let mut xo = x0;
    let mut yo = y0;

    for _ in 0..loops {
        for i in 0..l {
            let mut s = x[xo + i];

            for j in 1..=ORDER {
                s += a[a0 + j] * x[xo + i - j];
            }

            y[yo + i] = s;
        }

        if advance {
            a0 += ORDER + 1;
        }
        xo += l;
        yo += l;
    }
}

/// All-pole synthesis of `x` into `y` from the memory `mem`.
fn synthesis(a: &[f32], x: &[f32], y: &mut [f32], mem: &[f32]) {
    let l = x.len();
    let mut buf = [0.0f32; ORDER + LEN_SUPERFRAME];
    buf[..ORDER].copy_from_slice(&mem[..ORDER]);

    for i in 0..l {
        let mut s = x[i];
        for j in 1..=ORDER {
            s -= a[j] * buf[ORDER + i - j];
        }
        buf[ORDER + i] = s;
        y[i] = s;
    }
}

/// All-pole synthesis in place; `x[..ORDER]` is the memory and the samples follow.
fn synthesis_in_place(a: &[f32], x: &mut [f32], l: usize) {
    for i in ORDER..ORDER + l {
        let mut s = x[i];
        for j in 1..=ORDER {
            s -= a[j] * x[i - j];
        }
        x[i] = s;
    }
}

fn lpc_weight(a: &[f32]) -> [f32; ORDER + 1] {
    let mut ap = [0.0f32; ORDER + 1];
    ap[0] = a[0];
    let mut f = GAMMA;

    for i in 1..=ORDER {
        ap[i] = f * a[i];
        f *= GAMMA;
    }

    ap
}

fn dct(dcts: &mut Vec<Dct4>, input: &[f32], out: &mut [f32]) {
    let len = input.len();

    let idx = match dcts.iter().position(|d| d.len() == len) {
        Some(i) => i,
        None => {
            dcts.push(Dct4::new(len));
            dcts.len() - 1
        }
    };

    dcts[idx].transform(input, out);
}

/// The persistent state and scratch buffers of one channel's LPD decoder.
pub struct LpdDecoder {
    ccfl: usize,
    len_subfrm: usize,
    num_subfrm: usize,
    num_subfr_in_superfr: usize,
    num_subfr_by2: usize,
    synth_delay: usize,
    fscale: i32,

    pub frame: LpdFrame,

    /// `-1`: the previous frame was frequency-domain coded, `-2`: after short windows.
    mode_prev: i32,
    pitch_prev: [i32; 8],
    gain_prev: [f32; 8],
    bpf_active_prev: bool,
    lp_a_prev: [f32; 2 * (ORDER + 1)],
    xcitation_prev: Vec<f32>,
    synth_prev: Vec<f32>,
    bpf_prev: [f32; FILTER_DELAY + LEN_SUBFR],
    exc_prev: [f32; 1 + 2 * FAC_LENGTH],
    gain_threshold: f32,
    lsf_prev: [f32; ORDER],
    lspold: [f32; ORDER],
    fac_gain: f32,
    fac_fd_data: [f32; FAC_LENGTH / 4],

    /// The overlap-add buffer shared with the frequency-domain coder, `2 * ccfl` samples.
    pub overlap: Vec<f32>,
    /// Whether the previous frame of this channel was LPD coded (`td_frame_prev`).
    pub prev_lpd: bool,
    /// Whether the frequency-domain frame before this one used short windows.
    last_short: bool,
    /// The LP filter and excitation of the last ACELP frame, for the FAC of a following FD frame.
    lpc_prev: [f32; ORDER + 1],
    acelp_in: [f32; 1 + 2 * FAC_LENGTH],

    synth_buf: Vec<f32>,
    exc_buf: Vec<f32>,
    pitch: [i32; 32],
    pitch_gain: [f32; 32],
    fd_synth: Vec<f32>,
    dcts: Vec<Dct4>,
    sine_long: Vec<f32>,
    sine_short: Vec<f32>,
}

impl LpdDecoder {
    /// `core_rate` is the sampling frequency of the core coder.
    pub fn new(ccfl: usize, core_rate: u32) -> Self {
        let len_subfrm = ccfl / 4;
        let num_subfrm = 4 * ccfl / 1024;
        let num_subfr_in_superfr = 4 * num_subfrm;
        let num_subfr_by2 = num_subfr_in_superfr / 2 - 1;
        // libxaac uses the core rate itself for a 768 sample frame.
        let fscale = if ccfl == 768 {
            core_rate as i32
        }
        else {
            ((core_rate as u64 * ccfl as u64 / 1024) as i32).min(24000)
        };

        let mut dcts = Vec::new();
        for len in [len_subfrm / 2, len_subfrm, 2 * len_subfrm, 4 * len_subfrm, ccfl / 8, ccfl / 16]
        {
            if len >= 2 && !dcts.iter().any(|d: &Dct4| d.len() == len) {
                dcts.push(Dct4::new(len));
            }
        }

        let mut d = LpdDecoder {
            ccfl,
            len_subfrm,
            num_subfrm,
            num_subfr_in_superfr,
            num_subfr_by2,
            synth_delay: num_subfr_by2 * LEN_SUBFR,
            fscale,
            frame: LpdFrame::new(),
            mode_prev: -1,
            pitch_prev: [64; 8],
            gain_prev: [0.0; 8],
            bpf_active_prev: false,
            lp_a_prev: [0.0; 2 * (ORDER + 1)],
            xcitation_prev: vec![0.0; EXC_HIST],
            synth_prev: vec![0.0; MAX_PITCH + num_subfr_by2 * LEN_SUBFR],
            bpf_prev: [0.0; FILTER_DELAY + LEN_SUBFR],
            exc_prev: [0.0; 1 + 2 * FAC_LENGTH],
            gain_threshold: 0.0,
            lsf_prev: [0.0; ORDER],
            lspold: [0.0; ORDER],
            fac_gain: 0.0,
            fac_fd_data: [0.0; FAC_LENGTH / 4],
            overlap: vec![0.0; 2 * ccfl],
            prev_lpd: false,
            last_short: false,
            lpc_prev: [0.0; ORDER + 1],
            acelp_in: [0.0; 1 + 2 * FAC_LENGTH],
            synth_buf: vec![0.0; MAX_PITCH + num_subfr_by2 * LEN_SUBFR + LEN_SUPERFRAME + 256],
            exc_buf: vec![0.0; EXC_HIST + LEN_SUPERFRAME + 256],
            pitch: [64; 32],
            pitch_gain: [0.0; 32],
            fd_synth: vec![0.0; ORDER + ccfl],
            dcts,
            sine_long: sine_window(len_subfrm / 2 * 2),
            sine_short: sine_window(ccfl / 16 * 2),
        };

        d.reset();
        d
    }

    pub fn reset(&mut self) {
        self.mode_prev = -1;
        self.pitch_prev = [64; 8];
        self.gain_prev = [0.0; 8];
        self.bpf_active_prev = false;
        self.lp_a_prev = [0.0; 2 * (ORDER + 1)];
        self.xcitation_prev.fill(0.0);
        self.synth_prev.fill(0.0);
        self.bpf_prev = [0.0; FILTER_DELAY + LEN_SUBFR];
        self.exc_prev = [0.0; 1 + 2 * FAC_LENGTH];
        self.gain_threshold = 0.0;
        self.overlap.fill(0.0);
        self.prev_lpd = false;
        self.last_short = false;
        self.lpc_prev = [0.0; ORDER + 1];
        self.acelp_in = [0.0; 1 + 2 * FAC_LENGTH];
    }

    /// Decode one LPD frame into `out` (`ccfl` samples, int16 scale) including the overlap-add
    /// with the previous frame (`ixheaacd_td_frm_dec`).
    pub fn decode(
        &mut self,
        bs: &mut BitReaderLtr<'_>,
        arith: &mut ArithState,
        seed: &mut u32,
        indep_flag: bool,
        out: &mut [f32],
    ) -> Result<()> {
        // `ixheaacd_tw_buff_update` and `ixheaacd_fix2flt_data` of a frame after FD coding.
        if !self.prev_lpd {
            self.begin_after_fd();
        }

        let mut frame = core::mem::replace(&mut self.frame, LpdFrame::new());
        let res = frame.parse(bs, arith, self.ccfl, indep_flag, self.mode_prev);

        if let Err(e) = res {
            self.frame = frame;
            return Err(e);
        }

        let mut fsynth = vec![0.0f32; self.ccfl];
        self.lpd_dec(&frame, seed, &mut fsynth);

        // td_frm_dec
        let nlong = self.ccfl;
        let lfac = if self.last_short { nlong / 16 } else { nlong / 8 };
        let mod0 = frame.mod_[0];

        if !self.prev_lpd {
            if mod0 == 0 {
                fsynth[..nlong / 2 - lfac - LEN_SUBFR].fill(0.0);
                self.overlap[nlong / 2 - lfac - LEN_SUBFR..nlong / 2].fill(0.0);
            }
            else {
                fsynth[..nlong / 2 - lfac].fill(0.0);
                self.overlap[nlong / 2 - lfac..nlong].fill(0.0);
            }
        }

        for i in 0..nlong {
            out[i] = self.overlap[i] + fsynth[i];
            self.overlap[i] = 0.0;
        }

        self.prev_lpd = true;
        self.frame = frame;

        Ok(())
    }

    /// `ixheaacd_tw_buff_update` and `ixheaacd_fix2flt_data`: prepare for the first LPD frame
    /// after a frequency-domain one, whose overlap is in `self.overlap`.
    fn begin_after_fd(&mut self) {
        let ccfl = self.ccfl;
        let len_subfrm = self.len_subfrm;

        self.mode_prev = if self.last_short { -2 } else { -1 };
        self.pitch_prev = [64; 8];
        self.gain_prev = [0.0; 8];
        self.bpf_active_prev = false;

        let fac_length = if self.last_short { ccfl / 16 } else { len_subfrm / 2 };
        let win = sine_window(2 * fac_length);

        // The overlap is windowed and cut where the LPD frame takes over.
        for i in 0..2 * fac_length {
            self.overlap[ccfl / 2 - fac_length + i] *= win[2 * fac_length - 1 - i];
        }
        self.overlap[ccfl / 2 + fac_length..ccfl].fill(0.0);

        self.lp_a_prev = [0.0; 2 * (ORDER + 1)];
        self.xcitation_prev.fill(0.0);
        self.synth_prev.fill(0.0);
        self.bpf_prev = [0.0; FILTER_DELAY + LEN_SUBFR];
        self.gain_threshold = 0.0;

        self.exc_prev = [0.0; 1 + 2 * FAC_LENGTH];
        let base = len_subfrm / 2 - fac_length;
        for i in 0..2 * fac_length + 1 {
            self.exc_prev[base + i] = self.overlap[i + ccfl / 2 - fac_length - 1];
        }
    }

    /// Hand over the overlap of the frequency-domain frame before the next LPD frame.
    pub fn set_overlap(&mut self, fd_overlap: &[f32], last_short: bool) {
        let ccfl = self.ccfl;
        self.overlap[..ccfl].copy_from_slice(&fd_overlap[..ccfl]);
        self.overlap[ccfl..].fill(0.0);
        self.last_short = last_short;
    }

    /// `ixheaacd_lpd_dec_update`: the state an FD frame after an LPD frame continues from, written
    /// to the overlap buffer of the FD coder (`ccfl` samples).
    pub fn dec_update(&mut self, overlap: &mut [f32]) {
        let ccfl = self.ccfl;
        let synth_delay = self.synth_delay;
        let fac_length = self.len_subfrm / 2;
        let lpd_delay = (4 * self.num_subfrm / 2) * LEN_SUBFR;

        for i in 0..LEN_SUBFR + synth_delay {
            overlap[i] = self.synth_prev[MAX_PITCH - LEN_SUBFR + i];
        }

        let base = LEN_SUBFR + synth_delay - fac_length;
        overlap[base..base + 2 * fac_length].copy_from_slice(&self.exc_prev[1..1 + 2 * fac_length]);

        overlap[lpd_delay + fac_length..ccfl].fill(0.0);

        if self.mode_prev == 0 {
            self.lpc_prev.copy_from_slice(&self.lp_a_prev[ORDER + 1..]);
            self.acelp_in = self.exc_prev;
        }

        self.prev_lpd = false;
    }

    /// `ixheaacd_lpd_bpf_fix`: finish the bass post-filter of the last LPD frame over the first
    /// samples of the FD frame that follows it.
    pub fn bpf_fix(&mut self, is_short: bool, out: &mut [f32]) {
        let ccfl = self.ccfl;
        let lpd_sbf_len = 4 * self.num_subfrm / 2;
        let nsb2 = lpd_sbf_len - 1;
        let synth_delay = nsb2 * LEN_SUBFR;

        let mut synth_buf = vec![0.0f32; MAX_PITCH + synth_delay + ccfl + LEN_SUBFR];
        synth_buf[..MAX_PITCH + synth_delay].copy_from_slice(&self.synth_prev);
        let n = (synth_delay + ccfl + LEN_SUBFR).min(out.len());
        synth_buf[MAX_PITCH - LEN_SUBFR..MAX_PITCH - LEN_SUBFR + n].copy_from_slice(&out[..n]);

        let mut pitch = [64i32; 16];
        let mut gain = [0.0f32; 16];

        pitch[..nsb2].copy_from_slice(&self.pitch_prev[..nsb2]);
        gain[..nsb2].copy_from_slice(&self.gain_prev[..nsb2]);

        if self.mode_prev == 0 {
            pitch[nsb2] = pitch[nsb2 - 1];
            gain[nsb2] = gain[nsb2 - 1];

            if !is_short {
                pitch[nsb2 + 1] = pitch[nsb2];
                gain[nsb2 + 1] = gain[nsb2];
            }
        }

        let sy = MAX_PITCH;

        for i in 0..nsb2 + 2 {
            let tp = pitch[i].clamp(0, MAX_PITCH as i32) as usize;

            if gain[i] > 0.0 {
                let p = sy + i * LEN_SUBFR;
                let mut corr = 0.0f32;
                let mut energy = 1e-6f32;

                for k in 0..LEN_SUBFR {
                    corr += synth_buf[p + k] * synth_buf[p - tp + k];
                    energy += synth_buf[p - tp + k] * synth_buf[p - tp + k];
                }

                gain[i] = corr / energy;
            }
        }

        let len = (lpd_sbf_len + 2) * LEN_SUBFR + LEN_SUBFR;
        let mut signal = vec![0.0f32; len + LEN_SUBFR];

        Self::bass_post_filter(
            &synth_buf,
            &pitch,
            &gain,
            &mut self.bpf_prev,
            &mut signal,
            len,
            ccfl - (lpd_sbf_len + 4) * LEN_SUBFR,
        );

        out[..len].copy_from_slice(&signal[..len]);
    }

    /// `ixheaacd_cal_fac_data`: the time-domain FAC signal (`2 * lfac` samples) of an FD frame
    /// after an LPD frame, from the FAC values `fac` and their gain index.
    pub fn fac_for_fd(&mut self, gain_idx: i32, fac: &[i32], lfac: usize, out: &mut [f32]) {
        let n_long = self.ccfl;
        let gain = 10f64.powf(f64::from(gain_idx) / 28.0) as f32;

        let mut x = [0.0f32; FAC_LENGTH];
        for i in 0..lfac {
            x[i] = fac[i] as f32;
        }

        let mut sig = [0.0f32; ORDER + 2 * FAC_LENGTH];
        dct(&mut self.dcts, &x[..lfac], &mut sig[ORDER..ORDER + lfac]);

        let a = lpc_weight(&self.lpc_prev);
        synthesis_in_place(&a, &mut sig, 2 * lfac);

        let win = sine_window(2 * lfac);
        let len = n_long / 4;

        for i in 0..lfac {
            let w1 = win[lfac + i] * win[lfac + i];
            let fw_hi = 1.0 - w1;
            let fw_lo = win[lfac - 1 - i] * win[lfac + i];

            out[i] = gain * sig[ORDER + i]
                + self.acelp_in[1 + len / 2 + i] * fw_hi
                + self.acelp_in[1 + len / 2 - 1 - i] * fw_lo;
            out[lfac + i] = gain * sig[ORDER + lfac + i];
        }
    }

    fn lpd_dec(&mut self, frame: &LpdFrame, seed: &mut u32, fsynth: &mut [f32]) {
        let len_fr = self.ccfl;
        let len_subfrm = self.len_subfrm;
        let num_subfr = self.num_subfrm;
        let nsb2 = self.num_subfr_by2;
        let nsub = self.num_subfr_in_superfr;
        let synth_delay = self.synth_delay;
        let mod_ = frame.mod_;
        let first_lpd = frame.first_lpd_flag;

        let sbase = MAX_PITCH + synth_delay;
        self.synth_buf[..MAX_PITCH + synth_delay].copy_from_slice(&self.synth_prev);
        self.synth_buf[sbase..].fill(0.0);

        self.exc_buf[..EXC_HIST].copy_from_slice(&self.xcitation_prev);
        self.exc_buf[EXC_HIST..].fill(0.0);

        for i in 0..nsb2 {
            self.pitch[i] = self.pitch_prev[i];
            self.pitch_gain[i] = self.gain_prev[i];
        }
        for i in 0..nsub {
            self.pitch[i + nsb2] = 64;
            self.pitch_gain[i + nsb2] = 0.0;
        }

        let mut lsf = [[0.0f32; ORDER]; 5];

        if !first_lpd {
            lsf[0] = lpc::lsp_to_lsf(&self.lspold);
        }

        lpc::alg_vec_dequant(&frame.lpc_idx[..frame.lpc_len], first_lpd, &mut lsf, &mod_);

        if first_lpd {
            self.lsf_prev = lsf[0];
            self.lspold = lpc::lsf_to_lsp(&self.lsf_prev);
        }

        if first_lpd && (mod_[0] == 0 || mod_[1] == 0 || (mod_[2] == 0 && len_subfrm != 256)) {
            self.first_frame_init(frame, sbase);
        }

        let mut lp_a = [[0.0f32; ORDER + 1]; 9];
        let mut k = 0;

        while k < 4 {
            let mode = mod_[k];

            if self.mode_prev == 0 && mode > 0 && (k != 0 || self.bpf_active_prev) {
                let i = k * num_subfr + nsb2;
                self.pitch[i + 1] = self.pitch[i - 1];
                self.pitch[i] = self.pitch[i - 1];
                self.pitch_gain[i + 1] = self.pitch_gain[i - 1];
                self.pitch_gain[i] = self.pitch_gain[i - 1];
            }

            let lsf_curr = match mode {
                0 | 1 => lsf[k + 1],
                2 => lsf[k + 2],
                _ => lsf[k + 4],
            };

            let lsp_curr = lpc::lsf_to_lsp(&lsf_curr);

            let mut tmp = 0.0f32;
            for i in 0..ORDER {
                tmp += (lsf_curr[i] - self.lsf_prev[i]) * (lsf_curr[i] - self.lsf_prev[i]);
            }
            let stability = (1.25 - tmp / 400000.0).clamp(0.0, 1.0);

            if mode == 0 {
                lpc::interpolation_lsp_params(&self.lspold, &lsp_curr, num_subfr, &mut lp_a);
                self.acelp_alias_cnx(frame, k, &lp_a, stability);

                if self.mode_prev != 0 && frame.bpf_control_info {
                    let i = k * num_subfr + nsb2;
                    self.pitch[i - 1] = self.pitch[i];
                    self.pitch_gain[i - 1] = self.pitch_gain[i];

                    if self.mode_prev != -2 {
                        self.pitch[i - 2] = self.pitch[i];
                        self.pitch_gain[i - 2] = self.pitch_gain[i];
                    }
                }

                k += 1;
            }
            else {
                let (subfr_len, n_subfr) = match mode {
                    1 => (len_subfrm, num_subfr),
                    2 => (len_subfrm << 1, nsub / 2),
                    _ => (len_subfrm << 2, nsub),
                };

                let sets = lpc::lpc_coef_gen(&self.lspold, &lsp_curr, n_subfr);
                self.tcx_mdct(frame, k, &sets, subfr_len, seed);

                k += 1 << (mode - 1);
            }

            self.mode_prev = mode;
            self.lspold = lsp_curr;
            self.lsf_prev = lsf_curr;
        }

        self.xcitation_prev.copy_from_slice(&self.exc_buf[len_fr..len_fr + EXC_HIST]);
        self.synth_prev.copy_from_slice(&self.synth_buf[len_fr..len_fr + MAX_PITCH + synth_delay]);

        if !frame.bpf_control_info {
            if mod_[0] != 0 && self.bpf_active_prev {
                for i in 2..nsub {
                    self.pitch_gain[nsb2 + i] = 0.0;
                }
            }
            else {
                for i in 0..nsub {
                    self.pitch_gain[nsb2 + i] = 0.0;
                }
            }
        }

        self.bpf_active_prev = frame.bpf_control_info;

        for i in 0..nsb2 {
            self.pitch_prev[i] = self.pitch[nsub + i];
            self.gain_prev[i] = self.pitch_gain[nsub + i];
        }

        let sy = MAX_PITCH;

        for i in 0..nsub {
            let tp = self.pitch[i].clamp(0, MAX_PITCH as i32) as usize;
            let gain = self.pitch_gain[i];

            if gain > 0.0 {
                let mut corr = 0.0f32;
                let mut energy = 1e-6f32;
                let p = sy + i * LEN_SUBFR;

                if i * LEN_SUBFR + LEN_SUBFR > LEN_SUPERFRAME
                    || (i * LEN_SUBFR + LEN_SUBFR).saturating_sub(tp) > LEN_SUPERFRAME
                {
                    self.pitch_gain[i] = 0.0;
                    continue;
                }

                for j in 0..LEN_SUBFR {
                    corr += self.synth_buf[p + j] * self.synth_buf[p - tp + j];
                    energy += self.synth_buf[p - tp + j] * self.synth_buf[p - tp + j];
                }

                self.pitch_gain[i] = corr / energy;
            }
        }

        let len2 = if mod_[3] == 0 { synth_delay } else { synth_delay - len_subfrm / 2 };
        Self::bass_post_filter(
            &self.synth_buf,
            &self.pitch,
            &self.pitch_gain,
            &mut self.bpf_prev,
            fsynth,
            len_fr,
            len2,
        );
    }

    /// The initialisation of the synthesis memories from the overlap of the previous frame
    /// (the part of `ixheaacd_lpd_dec` handling `first_lpd_flag`).
    fn first_frame_init(&mut self, frame: &LpdFrame, sbase: usize) {
        let len_subfrm = self.len_subfrm;
        let mod_ = frame.mod_;

        let mut lp_a = [[0.0f32; ORDER + 1]; 9];
        lpc::interpolation_lsp_params(&self.lspold, &self.lspold, 8, &mut lp_a);

        self.lp_a_prev[..ORDER + 1].copy_from_slice(&lp_a[0]);
        self.lp_a_prev[ORDER + 1..].copy_from_slice(&lp_a[0]);

        if mod_[0] == 0 {
            let fac_length =
                if frame.short_fac_flag { len_subfrm * 4 / 16 } else { len_subfrm / 2 };

            let gain = 10f64.powf(f64::from(frame.fac_data[0].clamp(0, 127)) / 28.0) as f32;

            // fwd_alias_cancel_tool: the FAC signal, weighted-synthesis filtered and added to
            // the overlap at the end of the first half of the previous frame.
            let mut x = [0.0f32; FAC_LENGTH];
            for i in 0..fac_length {
                x[i] = frame.fac_data[1 + i] as f32;
            }

            let mut sig = [0.0f32; ORDER + FAC_LENGTH];
            dct(&mut self.dcts, &x[..fac_length], &mut sig[ORDER..ORDER + fac_length]);

            let a = lpc_weight(&lp_a[0]);
            synthesis_in_place(&a, &mut sig, fac_length);

            let base = self.ccfl / 2 - fac_length;
            for i in 0..fac_length {
                self.overlap[base + i] += sig[ORDER + i] * gain;
            }

            let half = self.ccfl / 2;
            self.overlap[half..half + fac_length].fill(0.0);
        }

        for i in 0..2 * len_subfrm {
            self.fd_synth[ORDER + i] = self.overlap[i];
        }

        self.synth_buf[sbase - 2 * len_subfrm..sbase]
            .copy_from_slice(&self.fd_synth[ORDER..ORDER + 2 * len_subfrm]);

        preemph(&mut self.fd_synth[ORDER..ORDER + 2 * len_subfrm], PREEMPH, 0.0);

        self.fd_synth[..ORDER].fill(0.0);

        let mut res = [0.0f32; 2 * 256];
        let length = 2 * len_subfrm / LEN_SUBFR;
        let flat: Vec<f32> = lp_a.iter().flatten().copied().collect();
        residual(&flat, false, &self.fd_synth, ORDER, &mut res, 0, LEN_SUBFR, length);

        let num = if mod_[0] != 0 && (len_subfrm == 256 || mod_[1] != 0) {
            len_subfrm.min(EXC_HIST)
        }
        else {
            (2 * len_subfrm).min(EXC_HIST)
        };

        self.exc_buf[EXC_HIST - num..EXC_HIST]
            .copy_from_slice(&res[2 * len_subfrm - num..2 * len_subfrm]);
    }

    fn acelp_alias_cnx(
        &mut self,
        frame: &LpdFrame,
        k: usize,
        lp_filt: &[[f32; ORDER + 1]],
        stability: f32,
    ) {
        let len_subfr = self.len_subfrm;
        let fac_length = len_subfr / 2;
        let core_mode = frame.acelp_core_mode;
        let sb = len_subfr * k + MAX_PITCH + self.synth_delay;
        let xb = len_subfr * k + MAX_PITCH + INTER_LP_FIL_ORDER + 1;
        let pitch_base = k * self.num_subfrm + self.num_subfr_by2;

        if self.mode_prev > 0 {
            let mut x = [0.0f32; FAC_LENGTH];
            let mut xd = [0.0f32; FAC_LENGTH];

            for i in 0..fac_length {
                x[i] = self.fac_gain * frame.fac[k][i] as f32;
            }
            for i in 0..fac_length / 4 {
                x[i] *= self.fac_fd_data[i];
            }

            dct(&mut self.dcts, &x[..fac_length], &mut xd[..fac_length]);

            let mut xn2 = [0.0f32; 2 * FAC_LENGTH + 16];
            for i in 0..fac_length {
                xn2[fac_length + i] = xd[i] * (2.0 / fac_length as f32);
            }

            // ixheaacd_lpc_wt_synthesis_tool: in place weighted synthesis.
            let a = &self.lp_a_prev[..ORDER + 1];
            for i in 0..fac_length {
                let n = fac_length + i;
                let mut s = xn2[n];
                let mut g = 1.0f32;
                for j in 1..=ORDER {
                    g *= GAMMA;
                    s -= a[j] * g * xn2[n - j];
                }
                xn2[n] = s;
            }

            for i in 0..2 * fac_length {
                xn2[i] += self.synth_buf[sb + i - 2 * fac_length];
            }

            self.synth_buf[sb - fac_length..sb].copy_from_slice(&xn2[fac_length..2 * fac_length]);

            preemph(&mut xn2[..2 * fac_length], PREEMPH, 0.0);

            let ttt = fac_length % LEN_SUBFR;
            let mut a0 = 0;

            // A 768 sample frame has a FAC length of 96, which spans three blocks: the last
            // filter set is used for the third (libxaac reads past its two sets here).
            let mut sets = [0.0f32; 3 * (ORDER + 1)];
            sets[..2 * (ORDER + 1)].copy_from_slice(&self.lp_a_prev);
            sets[2 * (ORDER + 1)..].copy_from_slice(&self.lp_a_prev[ORDER + 1..]);

            if ttt != 0 {
                residual(
                    &sets,
                    false,
                    &xn2,
                    fac_length,
                    &mut self.exc_buf,
                    xb - fac_length,
                    ttt,
                    1,
                );
                a0 = ORDER + 1;
            }

            let loops = (fac_length + ttt) / LEN_SUBFR;
            residual(
                &sets[a0..],
                true,
                &xn2,
                fac_length + ttt,
                &mut self.exc_buf,
                xb + ttt - fac_length,
                LEN_SUBFR,
                loops,
            );
        }

        let mut synth_temp = [0.0f32; 128 + 16];
        for i in 0..ORDER {
            synth_temp[i] =
                self.synth_buf[sb + i - ORDER] - PREEMPH * self.synth_buf[sb + i - ORDER - 1];
        }

        let i = ((self.fscale * TMIN + FSCALE_DENOM / 2) / FSCALE_DENOM) - TMIN;
        let pitch_min = TMIN + i;
        let pitch_fr2 = TFR2 - i;
        let pitch_fr1 = TFR1;
        let pitch_max = TMAX + 6 * i;

        let (mut pitch_lag, mut pitch_lag_frac);
        let mut pitch_lag_min = 0i32;
        let mut subfr_nb = 0;

        let mean_ener_code = frame.mean_energy[k] as f32 * 12.0 + 18.0;

        for subfr_idx in (0..len_subfr).step_by(LEN_SUBFR) {
            let mut pitch_flag = subfr_idx;
            if len_subfr == 256 && subfr_idx == 2 * LEN_SUBFR {
                pitch_flag = 0;
            }

            let mut index = frame.acb_index[k * 4 + subfr_nb];

            if pitch_flag == 0 {
                if index < (pitch_fr2 - pitch_min) * 4 {
                    pitch_lag = pitch_min + index / 4;
                    pitch_lag_frac = index - (pitch_lag - pitch_min) * 4;
                }
                else if index < (pitch_fr2 - pitch_min) * 4 + (pitch_fr1 - pitch_fr2) * 2 {
                    index -= (pitch_fr2 - pitch_min) * 4;
                    pitch_lag = pitch_fr2 + index / 2;
                    pitch_lag_frac = (index - (pitch_lag - pitch_fr2) * 2) * 2;
                }
                else {
                    pitch_lag = index + pitch_fr1
                        - (pitch_fr2 - pitch_min) * 4
                        - (pitch_fr1 - pitch_fr2) * 2;
                    pitch_lag_frac = 0;
                }

                pitch_lag_min = (pitch_lag - 8).max(pitch_min);
                let mut pitch_lag_max = pitch_lag_min + 15;

                if pitch_lag_max > pitch_max {
                    pitch_lag_max = pitch_max;
                    pitch_lag_min = pitch_lag_max - 15;
                }
            }
            else {
                pitch_lag = pitch_lag_min + index / 4;
                pitch_lag_frac = index - (pitch_lag - pitch_lag_min) * 4;
            }

            pitch_lag = pitch_lag.clamp(1, MAX_PITCH as i32);

            let xs = xb + subfr_idx;
            acelp::cb_exc_calc(&mut self.exc_buf, xs, pitch_lag, pitch_lag_frac);

            if !frame.ltp_filtering_flag[k * 4 + subfr_nb] {
                let mut filtered = [0.0f32; LEN_SUBFR];
                for i in 0..LEN_SUBFR {
                    filtered[i] = (0.18 * f64::from(self.exc_buf[xs + i - 1])
                        + 0.64 * f64::from(self.exc_buf[xs + i])
                        + 0.18 * f64::from(self.exc_buf[xs + i + 1]))
                        as f32;
                }
                self.exc_buf[xs..xs + LEN_SUBFR].copy_from_slice(&filtered);
            }

            let mut code = acelp::decode_pulses_per_track(
                &frame.icb_index[k * 4 + subfr_nb],
                acelp::CODE_BITS[core_mode],
            );

            preemph(&mut code, TILT_CODE, 0.0);

            let mut sharp = pitch_lag;
            if pitch_lag_frac > 2 {
                sharp += 1;
            }
            if sharp >= 0 {
                acelp::pitch_sharpening(&mut code, sharp as usize);
            }

            let (pitch_gain, gain_code, mut innov_energy) =
                acelp::decode_gains(frame.gains[k * 4 + subfr_nb] as usize, &code, mean_ener_code);

            let mut pitch_energy = 0.0f32;
            for i in 0..LEN_SUBFR {
                pitch_energy += self.exc_buf[xs + i] * self.exc_buf[xs + i];
            }

            pitch_energy *= pitch_gain * pitch_gain;
            innov_energy *= gain_code * gain_code;

            let r_v = (pitch_energy - innov_energy) / (pitch_energy + innov_energy);

            let mut post = [0.0f32; LEN_SUBFR];
            for i in 0..LEN_SUBFR {
                post[i] = pitch_gain * self.exc_buf[xs + i];
            }
            for i in 0..LEN_SUBFR {
                self.exc_buf[xs + i] = pitch_gain * self.exc_buf[xs + i] + gain_code * code[i];
            }

            let mut sharp = pitch_lag;
            if pitch_lag_frac > 2 {
                sharp += 1;
            }
            let sharp = sharp.min(pitch_max);

            self.pitch[pitch_base + subfr_nb] = sharp;
            self.pitch_gain[pitch_base + subfr_nb] = pitch_gain;

            let voicing_factor = (0.5 * (1.0 - f64::from(r_v))) as f32;
            let gain_smooth_factor = stability * voicing_factor;
            let mut gain_code0 = gain_code;

            if gain_code0 < self.gain_threshold {
                gain_code0 = (f64::from(gain_code0) * 1.19) as f32;
                if gain_code0 > self.gain_threshold {
                    gain_code0 = self.gain_threshold;
                }
            }
            else {
                gain_code0 = (f64::from(gain_code0) / 1.19) as f32;
                if gain_code0 < self.gain_threshold {
                    gain_code0 = self.gain_threshold;
                }
            }

            self.gain_threshold = gain_code0;

            let gain_smooth = (f64::from(gain_smooth_factor) * f64::from(gain_code0)
                + (1.0 - f64::from(gain_smooth_factor)) * f64::from(gain_code))
                as f32;

            for c in &mut code {
                *c *= gain_smooth;
            }

            let cpe = (0.125 * (1.0 + f64::from(r_v))) as f32;

            post[0] += code[0] - cpe * code[1];
            for i in 1..LEN_SUBFR - 1 {
                post[i] += code[i] - cpe * (code[i - 1] + code[i + 1]);
            }
            post[LEN_SUBFR - 1] += code[LEN_SUBFR - 1] - cpe * code[LEN_SUBFR - 2];

            let ss = sb + subfr_idx;
            let mut y = [0.0f32; LEN_SUBFR];
            synthesis(&lp_filt[subfr_nb], &post, &mut y, &synth_temp);
            self.synth_buf[ss..ss + LEN_SUBFR].copy_from_slice(&y);

            synth_temp[..ORDER].copy_from_slice(&y[LEN_SUBFR - ORDER..]);

            subfr_nb += 1;
        }

        let mem = self.synth_buf[sb - 1];
        deemph(&mut self.synth_buf[sb..sb + len_subfr], mem);

        synth_temp[16..].fill(0.0);
        synthesis_in_place(&lp_filt[subfr_nb], &mut synth_temp, 128);

        self.lp_a_prev[..ORDER + 1].copy_from_slice(&lp_filt[subfr_nb - 2]);
        self.lp_a_prev[ORDER + 1..].copy_from_slice(&lp_filt[subfr_nb - 1]);

        self.exc_prev[..1 + fac_length]
            .copy_from_slice(&self.synth_buf[sb + len_subfr - (1 + fac_length)..sb + len_subfr]);
        self.exc_prev[1 + fac_length..1 + 2 * fac_length]
            .copy_from_slice(&synth_temp[16..16 + fac_length]);

        let mem = self.synth_buf[sb + len_subfr - 1];
        deemph(&mut self.exc_prev[1 + fac_length..1 + 2 * fac_length], mem);
    }

    /// The weighting gains of the LPC spectrum `1 / |A(exp(j w))|` at `m` frequencies
    /// (`ixheaacd_lpc_to_td`).
    fn lpc_to_td(coeff: &[f32; ORDER + 1], m: usize) -> [f32; 64] {
        let size_n = 2 * m;
        let mut gains = [0.0f32; 64];

        for (i, g) in gains.iter_mut().enumerate().take(m) {
            let (mut re, mut im) = (0.0f64, 0.0f64);

            for (j, &c) in coeff.iter().enumerate() {
                let ang = -PI * (j * (2 * i + 1)) as f64 / size_n as f64;
                re += f64::from(c) * ang.cos();
                im += f64::from(c) * ang.sin();
            }

            *g = (1.0 / ((re * re + im * im).sqrt() + f64::from(f32::EPSILON))) as f32;
        }

        gains
    }

    fn tcx_mdct(
        &mut self,
        frame: &LpdFrame,
        frame_index: usize,
        sets: &[[f32; ORDER + 1]; 3],
        lg: usize,
        seed: &mut u32,
    ) {
        let len_subfrm = self.len_subfrm;
        let fl = len_subfrm / 2;
        let flp = if self.mode_prev == -2 { self.ccfl / 16 } else { fl };

        let sb = len_subfrm * frame_index + MAX_PITCH + self.synth_delay;
        let xb = len_subfrm * frame_index + EXC_HIST;

        let sine = |long: bool, this: &Self| -> Vec<f32> {
            if long { this.sine_long.clone() } else { this.sine_short.clone() }
        };
        let swin = sine(true, self);
        let swin_prev = sine(flp == fl, self);

        if self.mode_prev != 0 {
            if self.mode_prev > 0 {
                for i in 0..2 * flp {
                    self.exc_prev[i + fl - flp + 1] *= swin_prev[2 * flp - 1 - i];
                }
            }
            for i in 0..fl - flp {
                self.exc_prev[i + fl + flp + 1] = 0.0;
            }
        }

        let noise_level = 0.0625 * (8.0 - frame.noise_factor[frame_index] as f32);

        let tcx_off: usize = frame.tcx_lg[..frame_index].iter().sum();
        let quant = &frame.tcx[tcx_off..tcx_off + lg];

        let mut x = [0.0f32; LEN_SUPERFRAME];
        for i in 0..lg {
            x[i] = quant[i] as f32;
        }

        let mut i = lg / 6;
        while i < lg {
            let max_k = lg.min(i + 8);
            let mut tmp = 0.0f32;
            for q in &quant[i..max_k] {
                tmp += (*q as f32) * (*q as f32);
            }

            if tmp == 0.0 {
                for v in &mut x[i..max_k] {
                    *seed = seed.wrapping_mul(69069).wrapping_add(5);
                    *v = noise_level * if *seed & 0x10000 != 0 { -1.0 } else { 1.0 };
                }
            }
            i += 8;
        }

        // low_fq_deemphasis
        let mut alfd = [0.0f32; LEN_SUPERFRAME / 32];
        {
            let k = 8;
            let i_max = lg / 4;
            let mut max = 0.01f32;

            for i in (0..i_max).step_by(k) {
                let mut rm = 0.01f32;
                for v in &x[i..i + k] {
                    rm += v * v;
                }
                if rm > max {
                    max = rm;
                }
            }

            let mut factor = 0.1f32;

            for i in (0..i_max).step_by(k) {
                let mut rm = 0.01f32;
                for v in &x[i..i + k] {
                    rm += v * v;
                }
                rm = (rm / max).sqrt();
                if rm > factor {
                    factor = rm;
                }
                for v in &mut x[i..i + k] {
                    *v *= factor;
                }
                alfd[i / k] = factor;
            }
        }

        let gm = len_subfrm / 4;
        let gain1 = Self::lpc_to_td(&lpc_weight(&sets[1]), gm);
        let gain2 = Self::lpc_to_td(&lpc_weight(&sets[2]), gm);

        let mut energy = 0.01f32;
        for v in &x[..lg] {
            energy += v * v;
        }

        let temp = energy.sqrt() / lg as f32;
        let gain_tcx = (10f64.powf(f64::from(frame.global_gain[frame_index]) / 28.0)
            / (f64::from(temp) * 2.0)) as f32;

        // noise_shaping
        {
            let k = lg / gm;
            let (mut a, mut b) = (0.0f32, 0.0f32);
            let mut rr_prev = 0.0f32;

            for i in 0..lg {
                if i % k == 0 {
                    let (g1, g2) = (gain1[i / k], gain2[i / k]);
                    a = 2.0 * g1 * g2 / (g1 + g2);
                    b = (g2 - g1) / (g1 + g2);
                }

                x[i] = a * x[i] + b * rr_prev;
                rr_prev = x[i];
            }
        }

        // acelp_mdct_main
        let mut d = [0.0f32; LEN_SUPERFRAME];
        dct(&mut self.dcts, &x[..lg], &mut d[..lg]);

        let l = 2 * fl;
        let m = lg - l;
        let mut xn_buf = [0.0f32; LEN_SUPERFRAME + 2 * FAC_LENGTH];

        for i in 0..m / 2 {
            xn_buf[l + m / 2 - 1 - i] = -d[m / 2 + l / 2 + i];
        }
        for i in 0..l / 2 {
            xn_buf[i] = d[m + l / 2 + i];
            xn_buf[l - 1 - i] = -d[m + l / 2 + i];
        }
        for i in 0..m / 2 {
            xn_buf[l + m / 2 + i] = -d[m / 2 + l / 2 - 1 - i];
        }
        for i in 0..l / 2 {
            xn_buf[l + m + i] = -d[l / 2 - 1 - i];
            xn_buf[2 * l + m - 1 - i] = -d[l / 2 - 1 - i];
        }

        let scale = 2.0 / lg as f32;
        for v in &mut xn_buf[..lg + 2 * fl] {
            *v *= scale;
        }

        self.fac_gain = gain_tcx * 0.5 * (fl as f32 / lg as f32).sqrt();

        for i in 0..fl / 4 {
            self.fac_fd_data[i] = alfd[i * lg / (8 * fl)];
        }

        let mut xn1 = [0.0f32; 2 * FAC_LENGTH];

        if self.mode_prev == 0 {
            let mut facwindow = [0.0f32; 2 * FAC_LENGTH];

            for i in 0..flp {
                facwindow[i] = swin_prev[i] * swin_prev[2 * flp - 1 - i];
                facwindow[flp + i] = 1.0 - swin_prev[flp + i] * swin_prev[flp + i];
            }

            let mut xf = [0.0f32; FAC_LENGTH];
            for i in 0..fl {
                xf[i] = self.fac_gain * frame.fac[frame_index][i] as f32;
            }
            for i in 0..fl / 4 {
                xf[i] *= self.fac_fd_data[i];
            }

            let mut buf = [0.0f32; ORDER + 2 * FAC_LENGTH];
            let mut xd = [0.0f32; FAC_LENGTH];
            dct(&mut self.dcts, &xf[..fl], &mut xd[..fl]);

            for i in 0..fl {
                buf[ORDER + i] = xd[i] * (2.0 / fl as f32);
            }

            let ap = lpc_weight(&sets[1]);
            synthesis_in_place(&ap, &mut buf, 2 * fl);

            xn1[..2 * fl].copy_from_slice(&buf[ORDER..ORDER + 2 * fl]);

            for i in 0..fl {
                xn1[i] += self.exc_prev[1 + fl + i] * facwindow[fl + i]
                    + self.exc_prev[fl - i] * facwindow[fl - 1 - i];
            }
        }

        for v in &mut xn_buf[..lg + 2 * fl] {
            *v *= gain_tcx;
        }

        for i in 0..2 * flp {
            xn_buf[i + fl - flp] *= swin_prev[i];
        }
        for v in &mut xn_buf[..fl - flp] {
            *v = 0.0;
        }

        if self.mode_prev != 0 {
            for i in fl - flp..fl + flp {
                xn_buf[i] += self.exc_prev[1 + i];
            }
        }
        else {
            for i in fl - flp..fl + flp {
                xn_buf[i + fl] += xn1[i];
            }
        }

        self.exc_prev[..1 + 2 * fl].copy_from_slice(&xn_buf[lg - 1..lg + 2 * fl]);

        for i in 0..2 * fl {
            xn_buf[i + lg] *= swin[2 * fl - 1 - i];
        }

        if self.mode_prev != 0 {
            self.synth_buf[sb - flp..sb].copy_from_slice(&xn_buf[fl - flp..fl]);

            let mut buf = [0.0f32; ORDER + FAC_LENGTH];
            for i in 0..ORDER + fl {
                buf[i] = self.synth_buf[sb + i - ORDER - fl]
                    - PREEMPH * self.synth_buf[sb + i - ORDER - fl - 1];
            }

            let ttt = fl % LEN_SUBFR;
            if ttt != 0 {
                residual(&self.lp_a_prev, false, &buf, ORDER, &mut self.exc_buf, xb - fl, ttt, 1);
            }

            let loops = (fl - ttt) / LEN_SUBFR;
            residual(
                &self.lp_a_prev,
                true,
                &buf,
                ORDER + ttt,
                &mut self.exc_buf,
                xb + ttt - fl,
                LEN_SUBFR,
                loops,
            );
        }

        // `xn` is `xn_buf` from `fl`.
        self.synth_buf[sb..sb + lg].copy_from_slice(&xn_buf[fl..fl + lg]);

        let history: Vec<f32> = self.synth_buf[sb - ORDER - 1..sb].to_vec();
        xn_buf[fl - ORDER - 1..fl].copy_from_slice(&history);

        let tmp = xn_buf[fl - ORDER - 1];
        preemph(&mut xn_buf[fl - ORDER..fl + lg], PREEMPH, tmp);

        residual(&sets[2], false, &xn_buf, fl, &mut self.exc_buf, xb, lg, 1);

        self.lp_a_prev[..ORDER + 1].copy_from_slice(&sets[2]);
        self.lp_a_prev[ORDER + 1..].copy_from_slice(&sets[2]);
    }

    /// `ixheaacd_bass_post_filter`: `sig` holds the synthesis with `MAX_PITCH` samples of history
    /// before the first output sample.
    fn bass_post_filter(
        sig: &[f32],
        pitch: &[i32],
        pitch_gain: &[f32],
        bpf_prev: &mut [f32; FILTER_DELAY + LEN_SUBFR],
        out: &mut [f32],
        len_fr: usize,
        len2: usize,
    ) {
        let sy = MAX_PITCH;

        out[..len_fr].copy_from_slice(&sig[sy - LEN_SUBFR..sy - LEN_SUBFR + len_fr]);

        let mut noise_buf = [0.0f32; FILTER_DELAY + 2 * LEN_SUBFR];

        for (sf, num_subfr) in (0..len_fr).step_by(LEN_SUBFR).enumerate() {
            let mut pitch_lag = pitch[sf].clamp(0, 2 * MAX_PITCH as i32) as usize;
            let mut gain = pitch_gain[sf].clamp(0.0, 1.0);

            if (pitch_lag >> 1) + 96 > MAX_PITCH + num_subfr {
                pitch_lag = (MAX_PITCH + num_subfr - 96) << 1;
            }

            let xo = sy + num_subfr - 96;
            let yo = sy + num_subfr - pitch_lag / 2 - 96;

            let (mut x_energy, mut xy_corr, mut y_energy) = (0.01f32, 0.01f32, 0.01f32);

            for i in 0..LEN_SUBFR + 96 {
                x_energy += sig[xo + i] * sig[xo + i];
                xy_corr += sig[xo + i] * sig[yo + i];
                y_energy += sig[yo + i] * sig[yo + i];
            }

            let norm_corr = xy_corr / (x_energy * y_energy).sqrt();

            if norm_corr > 0.95 {
                pitch_lag >>= 1;
            }

            let lg = (len_fr + len2).saturating_sub(pitch_lag + num_subfr).min(LEN_SUBFR);

            let pitch_lag = pitch_lag.min(MAX_PITCH);
            let p = sy + num_subfr;

            let mut noise2 = [0.0f32; LEN_SUBFR];

            if gain > 0.0 {
                if lg > 0 {
                    let mut tmp = 0.01f32;
                    for i in 0..lg {
                        tmp += sig[p + i] * sig[p + i];
                    }
                    let mut energy = 0.01f32;
                    for i in 0..lg {
                        energy += sig[p + i + pitch_lag] * sig[p + i + pitch_lag];
                    }
                    tmp = (tmp / energy).sqrt();
                    if tmp < gain {
                        gain = tmp;
                    }
                }

                let alpha = 0.5 * gain;

                for i in 0..lg {
                    noise2[i] = alpha
                        * (sig[p + i]
                            - 0.5 * sig[p + i - pitch_lag]
                            - 0.5 * sig[p + i + pitch_lag]);
                }
                for i in lg..LEN_SUBFR {
                    noise2[i] = alpha * (sig[p + i] - sig[p + i - pitch_lag]);
                }
            }

            noise_buf[..FILTER_DELAY + LEN_SUBFR].copy_from_slice(&*bpf_prev);
            noise_buf[FILTER_DELAY + LEN_SUBFR..].copy_from_slice(&noise2);
            bpf_prev.copy_from_slice(&noise_buf[LEN_SUBFR..LEN_SUBFR + FILTER_DELAY + LEN_SUBFR]);

            for i in 0..LEN_SUBFR {
                let c = FILTER_DELAY + i;
                let mut tmp = FIR_LP_FILT[0] * noise_buf[c];

                for j in 1..=FILTER_DELAY {
                    tmp += FIR_LP_FILT[j] * (noise_buf[c - j] + noise_buf[c + j]);
                }

                out[i + num_subfr] -= tmp;
            }
        }
    }
}
