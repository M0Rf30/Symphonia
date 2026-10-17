// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A-SPX high frequency regeneration (FFmpeg `ac4dec.c`, see `NOTICE`).
//!
//! The functions are translated one to one from the reference; array bounds that the C code
//! leaves to the bitstream are validated once in [`validate`] so that malformed streams yield
//! a decode error instead of a panic.

use symphonia_core::errors::{Result, decode_error};

use crate::ctx::*;
use crate::spectral::get_sfb_offset;
use crate::tables::*;

/// Number of QMF time slots by which the HF adjuster delays the signal (`ts_offset_hfadj`).
const TS_OFFSET_HFADJ: i32 = 4;
/// ac4dec.c: NOISE_FLOOR_OFFSET
const NOISE_FLOOR_OFFSET: f32 = 6.0;
/// ac4dec.c: PAN_OFFSET
const PAN_OFFSET: f32 = 12.0;

/// Frame level parameters read by the A-SPX stages.
#[derive(Clone, Copy)]
struct Frame {
    num_qmf_timeslots: i32,
    ts_offset_hfgen: i32,
    num_ts_in_ats: i32,
}

/// Converts a possibly negative C index range into a `usize` range.
fn urange(a: i32, b: i32) -> std::ops::Range<usize> {
    a.max(0) as usize..b.max(0) as usize
}

fn at(v: &[i32], i: usize) -> i32 {
    v.get(i).copied().unwrap_or(-1)
}

/// ac4dec.c: polyfit (least squares fit by Gauss-Jordan elimination).
fn polyfit(order: usize, x_val: &[f32], y_val: &[f32], coefficients: &mut [f32]) -> bool {
    const MAX_ORDER: usize = 5;
    let count = x_val.len().min(y_val.len());
    if count <= order || order > MAX_ORDER {
        return false;
    }
    let n = order + 1;
    let w = 2 * n;
    let mut b = [0.0f32; MAX_ORDER + 1];
    let mut p = [0.0f32; (MAX_ORDER + 1) * 2 + 1];
    let mut a = [0.0f32; (MAX_ORDER + 1) * 2 * (MAX_ORDER + 1)];

    for ii in 0..count {
        let x = x_val[ii];
        let y = y_val[ii];
        let mut powx = 1.0f32;
        for bj in b.iter_mut().take(n) {
            *bj += y * powx;
            powx *= x;
        }
    }

    p[0] = count as f32;
    for ii in 0..count {
        let x = x_val[ii];
        let mut powx = x;
        for pj in p.iter_mut().take(w + 1).skip(1) {
            *pj += powx;
            powx *= x;
        }
    }

    for ii in 0..n {
        for jj in 0..n {
            a[ii * w + jj] = p[ii + jj];
        }
        a[ii * w + ii + n] = 1.0;
    }

    for ii in 0..n {
        let x = a[ii * w + ii];
        if x == 0.0 {
            return false;
        }
        for kk in 0..w {
            a[ii * w + kk] /= x;
        }
        for jj in 0..n {
            if jj != ii {
                let y = a[jj * w + ii];
                for kk in 0..w {
                    a[jj * w + kk] -= y * a[ii * w + kk];
                }
            }
        }
    }

    for ii in 0..n {
        let mut x = 0.0f32;
        for kk in 0..n {
            x += a[ii * w + kk + n] * b[kk];
        }
        coefficients[ii] = x;
    }
    true
}

fn fcomplex_mul(x: f32, yi: f32, u: f32, vi: f32) -> (f32, f32) {
    (x * u - yi * vi, x * vi + u * yi)
}

fn sqr(a: f32, b: f32) -> f32 {
    a * a + b * b
}

fn fcomplex_div(x: f32, yi: f32, u: f32, vi: f32) -> (f32, f32) {
    ((x * u + yi * vi) / sqr(u, vi), (x * vi - u * yi) / sqr(u, vi))
}

/// Checks every bitstream derived bound used by the A-SPX stages of one channel.
fn validate(f: Frame, ssch: &Ssc) -> Result<()> {
    let bad = || decode_error("ac4: invalid aspx parameters");
    let in_range = |v: i32, lo: i32, hi: i32| v >= lo && v <= hi;

    if !in_range(f.num_qmf_timeslots, 4, 32)
        || !in_range(f.ts_offset_hfgen, 0, 10)
        || f.num_qmf_timeslots + f.ts_offset_hfgen + TS_OFFSET_HFADJ > 42
        || !in_range(f.num_ts_in_ats, 1, 4)
    {
        return bad();
    }
    if !in_range(ssch.sbx, 0, 64)
        || !in_range(ssch.sba, 0, 64)
        || !in_range(ssch.num_sb_aspx, 0, 64 - ssch.sbx)
        || !in_range(ssch.aspx_num_env, 0, 5)
        || !in_range(ssch.aspx_num_noise, 0, 2)
        || !in_range(ssch.num_sbg_noise, 0, 5)
        || !in_range(ssch.num_sbg_sig_highres, 0, 23)
        || !in_range(ssch.num_sbg_sig_lowres, 0, 23)
        || !in_range(ssch.num_sbg_patches, 0, 5)
        || !in_range(ssch.num_sbg_lim, 0, 30)
    {
        return bad();
    }
    // Time borders: the HF signal must fit the QMF buffers.
    let env = ssch.aspx_num_env as usize;
    let mut prev = 0;
    for &b in &ssch.atsg_sig[..=env] {
        if b < prev || b * f.num_ts_in_ats > 38 {
            return bad();
        }
        prev = b;
    }
    for &b in &ssch.atsg_noise[..=ssch.aspx_num_noise as usize] {
        if b < 0 {
            return bad();
        }
    }
    let sb_ok = |v: i32| (ssch.sbx..=64).contains(&v);
    for atsg in 0..env {
        let n = ssch.num_sbg_sig[atsg];
        if !in_range(n, 0, 23) {
            return bad();
        }
        if !ssch.sbg_sig[atsg][..=n as usize].iter().all(|&v| sb_ok(v)) {
            return bad();
        }
    }
    if !ssch.sbg_sig_highres[..=ssch.num_sbg_sig_highres as usize].iter().all(|&v| sb_ok(v))
        || !ssch.sbg_noise[..=ssch.num_sbg_noise as usize].iter().all(|&v| sb_ok(v))
        || !ssch.sbg_lim[..=ssch.num_sbg_lim as usize].iter().all(|&v| sb_ok(v))
    {
        return bad();
    }
    let mut sum = 0;
    for i in 0..ssch.num_sbg_patches as usize {
        let (n, s) = (ssch.sbg_patch_num_sb[i], ssch.sbg_patch_start_sb[i]);
        if n < 0 || s < 0 || s + n > 64 {
            return bad();
        }
        sum += n;
    }
    if ssch.sbx + sum > 64 {
        return bad();
    }
    Ok(())
}

impl Ac4Core {
    fn frame(&self) -> Frame {
        Frame {
            num_qmf_timeslots: self.num_qmf_timeslots,
            ts_offset_hfgen: self.ts_offset_hfgen,
            num_ts_in_ats: self.num_ts_in_ats,
        }
    }

    /// ac4dec.c: channel_windows_valid
    fn channel_windows_valid(&self, ch: usize) -> bool {
        let Some(ssch) = self.substream.ssch.get(ch)
        else {
            return false;
        };
        let groups = usize::from(ssch.scp.num_window_groups);
        if groups > ssch.scp.num_win_in_group.len() {
            return false;
        }
        let mut win = 0usize;
        let mut k = 0usize;
        for g in 0..groups {
            let n = self.get_transf_length(ch, g as i32, None);
            let max_sfb = self.get_max_sfb(ch, g as i32);
            let sfb_offset = get_sfb_offset(n);
            if n <= 0
                || n as usize > ssch.pcm.len()
                || max_sfb < 0
                || max_sfb as usize >= sfb_offset.len()
            {
                return false;
            }
            for _ in 0..ssch.scp.num_win_in_group[g] {
                if win >= ssch.win_offset.len()
                    || ssch.win_offset[win] < 0
                    || ssch.win_offset[win] as usize + n as usize > ssch.pcm.len()
                    || i32::from(sfb_offset[max_sfb as usize]) > n
                {
                    return false;
                }
                k += usize::from(sfb_offset[max_sfb as usize]);
                win += 1;
            }
        }
        k <= ssch.scaled_spec.len()
    }

    /// ac4dec.c: prepare_channel
    pub(crate) fn prepare_channel(&mut self, ch: usize) -> Result<()> {
        if !self.channel_windows_valid(ch) {
            return decode_error("ac4: invalid window layout");
        }
        self.spectral_reordering(ch);
        self.spectral_synthesis(ch);
        self.qmf_analysis(ch);
        Ok(())
    }

    /// ac4dec.c: mono_aspx_processing
    pub(crate) fn mono_aspx_processing(&mut self) -> Result<()> {
        if self.substream.codec_mode != CM_ASPX {
            return Ok(());
        }
        let f = self.frame();
        let (preflat, interp) = (self.substream.aspx_preflat, self.substream.aspx_interpolation);
        let mut first_frame = self.first_frame;
        let ssch = &mut self.substream.ssch[0];
        validate(f, ssch)?;
        aspx_processing(f, ssch);
        get_qsignal_scale_factors(ssch, 0);
        get_qnoise_scale_factors(ssch, 0);
        mono_deq_signal_factors(ssch);
        mono_deq_noise_factors(ssch);
        regenerate(f, preflat, interp, &mut first_frame, ssch);
        self.first_frame = first_frame;
        Ok(())
    }

    /// ac4dec.c: stereo_aspx_processing
    pub(crate) fn stereo_aspx_processing(&mut self) -> Result<()> {
        if self.substream.codec_mode != CM_ASPX {
            return Ok(());
        }
        let f = self.frame();
        let (preflat, interp) = (self.substream.aspx_preflat, self.substream.aspx_interpolation);
        let mut first_frame = self.first_frame;
        let (a, b) = self.substream.ssch.split_at_mut(1);
        let (s0, s1) = (&mut a[0], &mut b[0]);
        validate(f, s0)?;
        validate(f, s1)?;
        aspx_processing(f, s0);
        aspx_processing(f, s1);
        get_qsignal_scale_factors(s0, 0);
        get_qsignal_scale_factors(s1, 1);
        get_qnoise_scale_factors(s0, 0);
        get_qnoise_scale_factors(s1, 1);
        if s0.aspx_balance == 0 {
            mono_deq_signal_factors(s0);
            mono_deq_signal_factors(s1);
            mono_deq_noise_factors(s0);
            mono_deq_noise_factors(s1);
        }
        else {
            stereo_deq_signoise_factors(s0, s1);
        }
        regenerate(f, preflat, interp, &mut first_frame, s0);
        regenerate(f, preflat, interp, &mut first_frame, s1);
        self.first_frame = first_frame;
        Ok(())
    }
}

/// The stages that follow the scale factor dequantisation, in reference order.
fn regenerate(f: Frame, preflat: i32, interp: i32, first_frame: &mut bool, ssch: &mut Ssc) {
    preflattening(f, ssch);
    get_covariance(f, ssch);
    get_alphas(ssch);
    get_chirps(ssch);
    create_high_signal(f, preflat, ssch);
    estimate_spectral_envelopes(f, interp, ssch);
    map_signoise(ssch);
    add_sinusoids(ssch);
    generate_tones(f, first_frame, ssch);
    generate_noise(f, ssch);
    assemble_hf_signal(f, ssch);
}

/// ac4dec.c: aspx_processing
fn aspx_processing(f: Frame, ssch: &mut Ssc) {
    *ssch.q_low_prev = *ssch.q_low;
    let (n, hf) = (f.num_qmf_timeslots as usize, f.ts_offset_hfgen as usize);
    let sbx = ssch.sbx as usize;

    for ts in 0..hf {
        for sb in 0..sbx {
            ssch.q_low[0][ts][sb] = ssch.q_prev[0][ts + n - hf][sb];
            ssch.q_low[1][ts][sb] = ssch.q_prev[1][ts + n - hf][sb];
        }
    }
    for ts in hf..n + hf {
        for sb in 0..sbx {
            ssch.q_low[0][ts][sb] = ssch.q[0][ts - hf][sb];
            ssch.q_low[1][ts][sb] = ssch.q[1][ts - hf][sb];
        }
    }
}

/// ac4dec.c: get_qsignal_scale_factors
fn get_qsignal_scale_factors(ssch: &mut Ssc, ch: usize) {
    let mut sbg_idx_high2low = [0usize; 24];
    let mut sbg_idx_low2high = [0usize; 24];
    let mut sbg_low = 0usize;

    for sbg in 0..ssch.num_sbg_sig_highres as usize {
        if at(&ssch.sbg_sig_lowres, sbg_low + 1) == ssch.sbg_sig_highres[sbg] {
            sbg_low += 1;
            if sbg_low < 24 {
                sbg_idx_low2high[sbg_low] = sbg;
            }
        }
        sbg_idx_high2low[sbg] = sbg_low.min(23);
    }

    let delta = i32::from(ch == 1 && ssch.aspx_balance == 1) + 1;
    let prev = (ssch.aspx_num_env_prev - 1).clamp(0, 4) as usize;

    ssch.qscf_sig_sbg_prev = ssch.qscf_sig_sbg;
    ssch.qscf_sig_sbg = [[0; 64]; 5];

    for atsg in 0..ssch.aspx_num_env as usize {
        for sbg in 0..ssch.num_sbg_sig[atsg] as usize {
            if atsg == 0 {
                ssch.atsg_freqres_prev[atsg] = ssch.atsg_freqres[prev];
                ssch.qscf_prev[atsg][sbg] = ssch.qscf_sig_sbg_prev[prev][sbg];
            }
            else {
                ssch.atsg_freqres_prev[atsg] = ssch.atsg_freqres[atsg - 1];
                ssch.qscf_prev[atsg][sbg] = ssch.qscf_sig_sbg[atsg - 1][sbg];
            }
            if ssch.aspx_sig_delta_dir[atsg] == 0 {
                // FREQ
                ssch.qscf_sig_sbg[atsg][sbg] = 0;
                for i in 0..=sbg {
                    ssch.qscf_sig_sbg[atsg][sbg] += delta * ssch.aspx_data[0][atsg][i];
                }
            }
            else {
                // TIME
                let (cur, old) = (ssch.atsg_freqres[atsg], ssch.atsg_freqres_prev[atsg]);
                if cur == old {
                    ssch.qscf_sig_sbg[atsg][sbg] = ssch.qscf_prev[atsg][sbg];
                    ssch.qscf_sig_sbg[atsg][sbg] += delta * ssch.aspx_data[0][atsg][sbg];
                }
                else if cur == 0 && old == 1 {
                    ssch.qscf_sig_sbg[atsg][sbg] = ssch.qscf_prev[atsg][sbg_idx_low2high[sbg]];
                    ssch.qscf_sig_sbg[atsg][sbg] += delta * ssch.aspx_data[0][atsg][sbg];
                }
                else if cur == 1 && old == 0 {
                    ssch.qscf_sig_sbg[atsg][sbg] = ssch.qscf_prev[atsg][sbg_idx_high2low[sbg]];
                    ssch.qscf_sig_sbg[atsg][sbg] += delta * ssch.aspx_data[0][atsg][sbg];
                }
            }
        }
    }
}

/// ac4dec.c: get_qnoise_scale_factors
///
/// The frequency-direction sum reads `aspx_data[1][atsg][sbg]` for every term, as FFmpeg does.
fn get_qnoise_scale_factors(ssch: &mut Ssc, ch: usize) {
    let delta = i32::from(ch == 1 && ssch.aspx_balance == 1) + 1;
    let prev = (ssch.aspx_num_noise_prev - 1).clamp(0, 1) as usize;

    ssch.qscf_noise_prev = ssch.qscf_noise_sbg;
    ssch.qscf_noise_sbg = [[0; 64]; 2];

    for atsg in 0..ssch.aspx_num_noise as usize {
        for sbg in 0..ssch.num_sbg_noise as usize {
            if ssch.aspx_noise_delta_dir[atsg] == 0 {
                for _ in 0..=sbg {
                    ssch.qscf_noise_sbg[atsg][sbg] += delta * ssch.aspx_data[1][atsg][sbg];
                }
            }
            else if atsg == 0 {
                ssch.qscf_noise_sbg[atsg][sbg] = ssch.qscf_noise_prev[prev][sbg];
                ssch.qscf_noise_sbg[atsg][sbg] += delta * ssch.aspx_data[1][atsg][sbg];
            }
            else {
                ssch.qscf_noise_sbg[atsg][sbg] = ssch.qscf_noise_sbg[atsg - 1][sbg];
                ssch.qscf_noise_sbg[atsg][sbg] += delta * ssch.aspx_data[1][atsg][sbg];
            }
        }
    }
}

/// ac4dec.c: mono_deq_signal_factors
fn mono_deq_signal_factors(ssch: &mut Ssc) {
    let a = if ssch.aspx_qmode_env == 0 { 2.0f32 } else { 1.0 };

    ssch.scf_sig_sbg = [[0.0; 64]; 5];
    for atsg in 0..ssch.aspx_num_env as usize {
        for sbg in 0..ssch.num_sbg_sig[atsg] as usize {
            ssch.scf_sig_sbg[atsg][sbg] = 64.0 * 2f32.powf(ssch.qscf_sig_sbg[atsg][sbg] as f32 / a);
        }
        if ssch.aspx_sig_delta_dir[atsg] == 0
            && ssch.qscf_sig_sbg[atsg][0] == 0
            && ssch.scf_sig_sbg[atsg][1] < 0.0
        {
            ssch.scf_sig_sbg[atsg][0] = ssch.scf_sig_sbg[atsg][1];
        }
    }
}

/// ac4dec.c: mono_deq_noise_factors
fn mono_deq_noise_factors(ssch: &mut Ssc) {
    ssch.scf_noise_sbg = [[0.0; 64]; 2];
    for atsg in 0..ssch.aspx_num_noise as usize {
        for sbg in 0..ssch.num_sbg_noise as usize {
            ssch.scf_noise_sbg[atsg][sbg] =
                2f32.powf(NOISE_FLOOR_OFFSET - ssch.qscf_noise_sbg[atsg][sbg] as f32);
        }
    }
}

/// ac4dec.c: stereo_deq_signoise_factors
fn stereo_deq_signoise_factors(s0: &mut Ssc, s1: &mut Ssc) {
    let a = if s0.aspx_qmode_env == 0 { 2.0f32 } else { 1.0 };

    for atsg in 0..s0.aspx_num_env as usize {
        for sbg in 0..s0.num_sbg_sig[atsg] as usize {
            let q0 = s0.qscf_sig_sbg[atsg][sbg] as f32;
            let q1 = s1.qscf_sig_sbg[atsg][sbg] as f32;
            let nom = 64.0 * 2f32.powf(q0 / a + 1.0);
            let denom_a = 1.0 + 2f32.powf(PAN_OFFSET - q1 / a);
            let denom_b = 1.0 + 2f32.powf(q1 / a - PAN_OFFSET);
            s0.scf_sig_sbg[atsg][sbg] = nom / denom_a;
            s1.scf_sig_sbg[atsg][sbg] = nom / denom_b;
        }
    }

    for atsg in 0..s0.aspx_num_noise as usize {
        for sbg in 0..s0.num_sbg_noise as usize {
            let q0 = s0.qscf_noise_sbg[atsg][sbg] as f32;
            let q1 = s1.qscf_noise_sbg[atsg][sbg] as f32;
            let nom = 2f32.powf(NOISE_FLOOR_OFFSET - q0 + 1.0);
            let denom_a = 1.0 + 2f32.powf(PAN_OFFSET - q1);
            let denom_b = 1.0 + 2f32.powf(q1 - PAN_OFFSET);
            s0.scf_noise_sbg[atsg][sbg] = nom / denom_a;
            s1.scf_noise_sbg[atsg][sbg] = nom / denom_b;
        }
    }
}

/// ac4dec.c: preflattening
fn preflattening(f: Frame, ssch: &mut Ssc) {
    const ORDER: usize = 3;
    let nsb = ssch.sbx as usize;
    let mut poly_array = [0.0f32; 64];
    let mut pow_env = [0.0f32; 64];
    let mut slope = [0.0f32; 64];
    let mut x = [0.0f32; 64];
    let mut mean_energy = 0.0f32;

    for (i, xi) in x.iter_mut().enumerate().take(nsb) {
        *xi = i as f32;
    }

    let env = ssch.aspx_num_env as usize;
    let t0 = (ssch.atsg_sig[0] * f.num_ts_in_ats) as usize;
    let t1 = (ssch.atsg_sig[env] * f.num_ts_in_ats) as usize;
    for sb in 0..nsb {
        pow_env[sb] = 0.0;
        for ts in t0..t1 {
            pow_env[sb] += ssch.q_low[0][ts][sb].powi(2);
            pow_env[sb] += ssch.q_low[1][ts][sb].powi(2);
        }
        pow_env[sb] /= ((ssch.atsg_sig[env] - ssch.atsg_sig[0]) * f.num_ts_in_ats) as f32;
        pow_env[sb] = 10.0 * (pow_env[sb] + 1.0).log10();
        mean_energy += pow_env[sb];
    }

    mean_energy /= nsb as f32;
    polyfit(ORDER, &x[..nsb], &pow_env[..nsb], &mut poly_array);

    for k in (0..=ORDER).rev() {
        for sb in 0..nsb {
            slope[sb] += x[sb].powi(k as i32) * poly_array[k];
        }
    }

    for sb in 0..nsb {
        ssch.gain_vec[sb] = 10f32.powf((mean_energy - slope[sb]) / 20.0);
    }
}

/// ac4dec.c: get_chirps
fn get_chirps(ssch: &mut Ssc) {
    ssch.chirp_arr_prev = ssch.chirp_arr;

    for sbg in 0..ssch.num_sbg_noise as usize {
        let cur = (ssch.aspx_tna_mode[sbg] & 3) as usize;
        let old = (ssch.aspx_tna_mode_prev[sbg] & 3) as usize;
        let mut new_chirp = NEW_CHIRP_TAB[cur][old];

        if new_chirp < ssch.chirp_arr_prev[sbg] {
            new_chirp = 0.75 * new_chirp + 0.25 * ssch.chirp_arr_prev[sbg];
        }
        else {
            new_chirp = 0.90625 * new_chirp + 0.09375 * ssch.chirp_arr_prev[sbg];
        }

        ssch.chirp_arr[sbg] = if new_chirp < 0.015625 { 0.0 } else { new_chirp };
    }
}

/// ac4dec.c: get_covariance
fn get_covariance(f: Frame, ssch: &mut Ssc) {
    let n = f.num_qmf_timeslots as usize;
    let hfgen = f.ts_offset_hfgen as usize;
    let hfadj = TS_OFFSET_HFADJ as usize;
    let sba = ssch.sba as usize;

    for sb in 0..sba {
        let ts_offset_prev = n - hfadj;
        for ts in 0..hfadj {
            ssch.q_low_ext[0][ts][sb] = ssch.q_low_prev[0][ts + ts_offset_prev][sb];
            ssch.q_low_ext[1][ts][sb] = ssch.q_low_prev[1][ts + ts_offset_prev][sb];
        }
        for ts in 0..n + hfgen {
            ssch.q_low_ext[0][ts + hfadj][sb] = ssch.q_low[0][ts][sb];
            ssch.q_low_ext[1][ts + hfadj][sb] = ssch.q_low[1][ts][sb];
        }
    }

    let num_ts_ext = n + hfgen + hfadj;
    for sb in 0..sba {
        for i in 0..3 {
            for j in 1..3 {
                ssch.cov[sb][i][j] = [0.0; 2];
                let mut ts = hfadj;
                while ts < num_ts_ext {
                    let (re, im) = fcomplex_mul(
                        ssch.q_low_ext[0][ts - 2 * i][sb],
                        ssch.q_low_ext[1][ts - 2 * i][sb],
                        ssch.q_low_ext[0][ts - 2 * j][sb],
                        -ssch.q_low_ext[1][ts - 2 * j][sb],
                    );
                    ssch.cov[sb][i][j][0] += re;
                    ssch.cov[sb][i][j][1] += im;
                    ts += 2;
                }
            }
        }
    }
}

/// ac4dec.c: get_alphas
fn get_alphas(ssch: &mut Ssc) {
    let epsilon_inv = 2f32.powf(-20.0);

    for sb in 0..ssch.sba as usize {
        let c = ssch.cov[sb];
        let (d0, d1) = fcomplex_mul(c[2][2][0], c[2][2][1], c[1][1][0], c[1][1][1]);
        let d0 = d0 - sqr(c[1][2][0], c[1][2][1]) / (1.0 + epsilon_inv);
        if sqr(d0, d1) <= 1e-6 {
            ssch.alpha1[sb] = [0.0, 0.0];
        }
        else {
            let r = (c[0][1][0] * c[1][2][0] - c[0][1][1] * c[1][2][1])
                - (c[0][2][0] * c[1][1][0] - c[0][2][1] * c[1][1][1]);
            let i = (c[0][1][0] * c[1][2][1] + c[0][1][1] * c[1][2][0])
                - (c[0][2][0] * c[1][1][1] + c[0][2][1] * c[1][1][0]);
            let (r, i) = fcomplex_div(r, i, d0, d1);
            ssch.alpha1[sb] = [r, i];
        }

        if sqr(c[1][1][0], c[1][1][1]) <= 1e-6 {
            ssch.alpha0[sb] = [0.0, 0.0];
        }
        else {
            let a1 = ssch.alpha1[sb];
            let r = -c[0][1][0] + a1[0] * c[1][2][0] + a1[1] * c[1][2][1];
            let i = -c[0][1][1] + a1[1] * c[1][2][0] - a1[0] * c[1][2][1];
            let (r, i) = fcomplex_div(r, i, c[1][1][0], c[1][1][1]);
            ssch.alpha0[sb] = [r, i];
        }
    }
}

/// ac4dec.c: create_high_signal
fn create_high_signal(f: Frame, preflat: i32, ssch: &mut Ssc) {
    let hfadj = TS_OFFSET_HFADJ as usize;
    let env = ssch.aspx_num_env as usize;
    let t0 = (ssch.atsg_sig[0] * f.num_ts_in_ats) as usize;
    let t1 = (ssch.atsg_sig[env] * f.num_ts_in_ats) as usize;

    for ts in t0..t1 {
        let mut sum_sb_patches = 0i32;
        let mut g = 0usize;
        for i in 0..ssch.num_sbg_patches as usize {
            for sb in 0..ssch.sbg_patch_num_sb[i] {
                let sb_high = (ssch.sbx + sum_sb_patches + sb) as usize;

                if at(&ssch.sbg_noise, g + 1) == sb_high as i32 {
                    g += 1;
                }
                let gc = g.min(5);

                let n = ts + hfadj;
                let p = (ssch.sbg_patch_start_sb[i] + sb) as usize;
                ssch.q_high[0][ts][sb_high] = ssch.q_low_ext[0][n][p];
                ssch.q_high[1][ts][sb_high] = ssch.q_low_ext[1][n][p];

                let (r, im) = fcomplex_mul(
                    ssch.alpha0[p][0],
                    ssch.alpha0[p][1],
                    ssch.q_low_ext[0][n - 2][p],
                    ssch.q_low_ext[1][n - 2][p],
                );
                let (r, im) = fcomplex_mul(r, im, ssch.chirp_arr[gc], 0.0);
                ssch.q_high[0][ts][sb_high] += r;
                ssch.q_high[1][ts][sb_high] += im;
                let (r, im) = fcomplex_mul(
                    ssch.alpha1[p][0],
                    ssch.alpha1[p][1],
                    ssch.q_low_ext[0][n - 4][p],
                    ssch.q_low_ext[1][n - 4][p],
                );
                let (r, im) = fcomplex_mul(r, im, ssch.chirp_arr[gc].powi(2), 0.0);
                ssch.q_high[0][ts][sb_high] += r;
                ssch.q_high[1][ts][sb_high] += im;
                if preflat != 0 {
                    let (r, im) = fcomplex_mul(
                        ssch.q_high[0][ts][sb_high],
                        ssch.q_high[1][ts][sb_high],
                        1.0 / ssch.gain_vec[p],
                        0.0,
                    );
                    ssch.q_high[0][ts][sb_high] = r;
                    ssch.q_high[1][ts][sb_high] = im;
                }
            }
            sum_sb_patches += ssch.sbg_patch_num_sb[i];
        }
    }
}

/// ac4dec.c: estimate_spectral_envelopes
fn estimate_spectral_envelopes(f: Frame, interpolation: i32, ssch: &mut Ssc) {
    let hfadj = TS_OFFSET_HFADJ;

    for atsg in 0..ssch.aspx_num_env as usize {
        let mut sbg = 0usize;
        for sb in 0..ssch.num_sb_aspx as usize {
            let mut est_sig = 0.0f32;

            if sb as i32 == at(&ssch.sbg_sig[atsg], sbg + 1) {
                sbg += 1;
            }
            let (lo, hi) = (at(&ssch.sbg_sig[atsg], sbg), at(&ssch.sbg_sig[atsg], sbg + 1));

            let tsa = ssch.atsg_sig[atsg] * f.num_ts_in_ats + hfadj;
            let tsz = ssch.atsg_sig[atsg + 1] * f.num_ts_in_ats + hfadj;
            for ts in urange(tsa, tsz) {
                if interpolation == 0 {
                    for j in urange(lo, hi) {
                        est_sig += ssch.q_high[0][ts][j].hypot(ssch.q_high[1][ts][j]);
                    }
                }
                else {
                    let j = sb + ssch.sbx as usize;
                    est_sig += ssch.q_high[0][ts][j].hypot(ssch.q_high[1][ts][j]);
                }
            }

            let dt = (ssch.atsg_sig[atsg + 1] - ssch.atsg_sig[atsg]) as f32;
            if interpolation == 0 {
                est_sig /= (hi - lo) as f32;
                est_sig /= dt;
            }
            else {
                est_sig /= dt;
            }
            ssch.est_sig_sb[atsg][sb] = est_sig;
        }
    }
}

/// ac4dec.c: map_signoise
fn map_signoise(ssch: &mut Ssc) {
    let mut atsg_noise = 0usize;
    let sbx = ssch.sbx;

    ssch.scf_noise_sb = [[0.0; 64]; 5];
    ssch.scf_sig_sb = [[0.0; 64]; 5];

    for atsg in 0..ssch.aspx_num_env as usize {
        for sbg in 0..ssch.num_sbg_sig[atsg] as usize {
            for sb in urange(ssch.sbg_sig[atsg][sbg] - sbx, ssch.sbg_sig[atsg][sbg + 1] - sbx) {
                ssch.scf_sig_sb[atsg][sb] = ssch.scf_sig_sbg[atsg][sbg];
            }
        }

        if ssch.atsg_sig[atsg] == at(&ssch.atsg_noise, atsg_noise + 1) {
            atsg_noise += 1;
        }

        for sbg in 0..ssch.num_sbg_noise as usize {
            for sb in urange(ssch.sbg_noise[sbg] - sbx, ssch.sbg_noise[sbg + 1] - sbx) {
                ssch.scf_noise_sb[atsg][sb] = ssch.scf_noise_sbg[atsg_noise.min(1)][sbg];
            }
        }
    }
}

/// ac4dec.c: add_sinusoids
fn add_sinusoids(ssch: &mut Ssc) {
    const EPSILON: f32 = 1.0;
    const LIM_GAIN: f32 = 1.41254;
    const MAX_BOOST_FACT: f32 = 1.584893192;
    let epsilon0 = 10f32.powf(-12.0);
    let max_sig_gain = 10f32.powf(5.0);

    let sbx = ssch.sbx;
    let num_env = ssch.aspx_num_env as usize;
    let num_sb = ssch.num_sb_aspx as usize;
    let prev = (ssch.aspx_num_env_prev - 1).clamp(0, 4) as usize;
    let p_sine_at_end: i32 = if ssch.aspx_tsg_ptr_prev == ssch.aspx_num_env_prev { 0 } else { -1 };

    for atsg in 0..num_env {
        for sbg in 0..ssch.num_sbg_sig_highres as usize {
            let sba = ssch.sbg_sig_highres[sbg] - sbx;
            let sbz = ssch.sbg_sig_highres[sbg + 1] - sbx;
            let sb_mid = (0.5 * (sbz + sba) as f32 + 0.5) as i32;
            for sb in urange(sba, sbz) {
                if sb as i32 == sb_mid
                    && (atsg as i32 >= ssch.aspx_tsg_ptr
                        || p_sine_at_end == 0
                        || ssch.sine_idx_sb_prev[prev][sb] != 0.0)
                {
                    ssch.sine_idx_sb[atsg][sb] = ssch.aspx_add_harmonic[sbg] as f32;
                }
                else {
                    ssch.sine_idx_sb[atsg][sb] = 0.0;
                }
            }
        }
    }

    ssch.sine_idx_sb_prev = ssch.sine_idx_sb;

    for atsg in 0..num_env {
        for sbg in 0..ssch.num_sbg_sig[atsg] as usize {
            let range = urange(ssch.sbg_sig[atsg][sbg] - sbx, ssch.sbg_sig[atsg][sbg + 1] - sbx);
            let sine_present = range.clone().any(|sb| ssch.sine_idx_sb[atsg][sb] == 1.0);
            for sb in range {
                ssch.sine_area_sb[atsg][sb] = if sine_present { 1.0 } else { 0.0 };
            }
        }
    }

    ssch.noise_lev_sb = [[0.0; 64]; 5];

    for atsg in 0..num_env {
        for sb in 0..num_sb {
            let sig_noise_fact = ssch.scf_sig_sb[atsg][sb] / (1.0 + ssch.scf_noise_sb[atsg][sb]);
            ssch.sine_lev_sb[atsg][sb] = (sig_noise_fact * ssch.sine_idx_sb[atsg][sb]).sqrt();
            ssch.noise_lev_sb[atsg][sb] = (sig_noise_fact * ssch.scf_noise_sb[atsg][sb]).sqrt();
        }
    }

    for atsg in 0..num_env {
        for sb in 0..num_sb {
            if ssch.sine_area_sb[atsg][sb] == 0.0 {
                let mut denom = EPSILON + ssch.est_sig_sb[atsg][sb];
                if !(atsg as i32 == ssch.aspx_tsg_ptr || atsg as i32 == p_sine_at_end) {
                    denom *= 1.0 + ssch.scf_noise_sb[atsg][sb];
                }
                ssch.sig_gain_sb[atsg][sb] = (ssch.scf_sig_sb[atsg][sb] / denom).sqrt();
            }
            else {
                let mut denom = EPSILON + ssch.est_sig_sb[atsg][sb];
                denom *= 1.0 + ssch.scf_noise_sb[atsg][sb];
                ssch.sig_gain_sb[atsg][sb] =
                    (ssch.scf_sig_sb[atsg][sb] * ssch.scf_noise_sb[atsg][sb] / denom).sqrt();
            }
        }
    }

    let num_lim = ssch.num_sbg_lim as usize;
    for atsg in 0..num_env {
        for sbg in 0..num_lim {
            let mut nom = 0.0f32;
            let mut denom = epsilon0;
            for sb in urange(ssch.sbg_lim[sbg] - sbx, ssch.sbg_lim[sbg + 1] - 1 - sbx) {
                nom += ssch.scf_sig_sb[atsg][sb];
                denom += ssch.est_sig_sb[atsg][sb];
            }
            ssch.max_sig_gain_sbg[atsg][sbg] = (nom / denom).sqrt() * LIM_GAIN;
        }

        let mut sbg = 0usize;
        for sb in 0..num_sb {
            if sb as i32 == at(&ssch.sbg_lim, sbg + 1) - sbx {
                sbg += 1;
            }
            ssch.max_sig_gain_sb[atsg][sb] =
                ssch.max_sig_gain_sbg[atsg][sbg.min(63)].min(max_sig_gain);
        }
    }

    ssch.noise_lev_sb_lim = [[0.0; 64]; 5];

    for atsg in 0..num_env {
        for sb in 0..num_sb {
            let mut tmp = ssch.noise_lev_sb[atsg][sb];
            tmp *= ssch.max_sig_gain_sb[atsg][sb] / ssch.sig_gain_sb[atsg][sb];
            ssch.noise_lev_sb_lim[atsg][sb] = ssch.noise_lev_sb[atsg][sb].min(tmp);
        }
    }

    for atsg in 0..num_env {
        for sb in 0..num_sb {
            ssch.sig_gain_sb_lim[atsg][sb] =
                ssch.sig_gain_sb[atsg][sb].min(ssch.max_sig_gain_sb[atsg][sb]);
        }
    }

    for atsg in 0..num_env {
        for sbg in 0..num_lim {
            let mut nom = epsilon0;
            let mut denom = epsilon0;
            for sb in urange(ssch.sbg_lim[sbg] - sbx, ssch.sbg_lim[sbg + 1] - 1 - sbx) {
                nom += ssch.scf_sig_sb[atsg][sb];
                denom += ssch.est_sig_sb[atsg][sb] * ssch.sig_gain_sb_lim[atsg][sb].powi(2);
                denom += ssch.sine_lev_sb[atsg][sb].powi(2);
                if !(ssch.sine_lev_sb[atsg][sb] != 0.0
                    || atsg as i32 == ssch.aspx_tsg_ptr
                    || atsg as i32 == p_sine_at_end)
                {
                    denom += ssch.noise_lev_sb_lim[atsg][sb].powi(2);
                }
            }
            ssch.boost_fact_sbg[atsg][sbg] = (nom / denom).sqrt();
        }
    }

    for atsg in 0..num_env {
        let mut sbg = 0usize;
        for sb in 0..num_sb {
            if sb as i32 == at(&ssch.sbg_lim, sbg + 1) - sbx {
                sbg += 1;
            }
            ssch.boost_fact_sb[atsg][sb] =
                ssch.boost_fact_sbg[atsg][sbg.min(63)].min(MAX_BOOST_FACT);
        }
    }

    ssch.noise_lev_sb_adj = [[0.0; 64]; 5];

    for atsg in 0..num_env {
        for sb in 0..num_sb {
            let boost_fact = ssch.boost_fact_sb[atsg][sb];
            ssch.sig_gain_sb_adj[atsg][sb] = ssch.sig_gain_sb_lim[atsg][sb] * boost_fact;
            ssch.noise_lev_sb_adj[atsg][sb] = ssch.noise_lev_sb_lim[atsg][sb] * boost_fact;
            ssch.sine_lev_sb_adj[atsg][sb] = ssch.sine_lev_sb[atsg][sb] * boost_fact;
        }
    }
}

/// ac4dec.c: sine_idx
fn sine_idx(sb: usize, ts: usize, first_frame: &mut bool, ssch: &Ssc) -> i32 {
    let mut index = if *first_frame {
        *first_frame = false;
        1
    }
    else {
        (i32::from(ssch.sine_idx_prev[ts][sb]) + 1) % 4
    };
    index += ts as i32 - ssch.atsg_sig[0];
    index.rem_euclid(4)
}

/// ac4dec.c: noise_idx
fn noise_idx(sb: usize, ts: usize, ssch: &Ssc) -> i32 {
    let mut index = if ssch.master_reset != 0 { 0 } else { i32::from(ssch.noise_idx_prev[ts][sb]) };
    index += ssch.num_sb_aspx * (ts as i32 - ssch.atsg_sig[0]);
    index += sb as i32 + 1;
    index.rem_euclid(512)
}

/// ac4dec.c: generate_noise
fn generate_noise(f: Frame, ssch: &mut Ssc) {
    let mut atsg = 0usize;
    let env = ssch.aspx_num_env as usize;
    let t0 = (ssch.atsg_sig[0] * f.num_ts_in_ats) as usize;
    let t1 = (ssch.atsg_sig[env] * f.num_ts_in_ats) as usize;

    *ssch.qmf_noise = [[[0.0; 64]; 42]; 2];

    for ts in t0..t1 {
        if ts as i32 == at(&ssch.atsg_sig, atsg + 1) * f.num_ts_in_ats {
            atsg += 1;
        }
        for sb in 0..ssch.num_sb_aspx as usize {
            let idx = noise_idx(sb, ts, ssch);
            ssch.noise_idx_prev[ts][sb] = idx as i16;
            let lev = ssch.noise_lev_sb_adj[atsg.min(4)][sb];
            ssch.qmf_noise[0][ts][sb] = lev * ASPX_NOISE[idx as usize][0];
            ssch.qmf_noise[1][ts][sb] = lev * ASPX_NOISE[idx as usize][1];
        }
    }
}

/// ac4dec.c: generate_tones
fn generate_tones(f: Frame, first_frame: &mut bool, ssch: &mut Ssc) {
    let mut atsg = 0usize;
    let env = ssch.aspx_num_env as usize;
    let t0 = (ssch.atsg_sig[0] * f.num_ts_in_ats) as usize;
    let t1 = (ssch.atsg_sig[env] * f.num_ts_in_ats) as usize;

    for ts in t0..t1 {
        if ts as i32 == at(&ssch.atsg_sig, atsg + 1) * f.num_ts_in_ats {
            atsg += 1;
        }
        for sb in 0..ssch.num_sb_aspx as usize {
            let idx = sine_idx(sb, ts, first_frame, ssch);
            ssch.sine_idx_prev[ts][sb] = idx as i8;
            let lev = ssch.sine_lev_sb_adj[atsg.min(4)][sb];
            ssch.qmf_sine[0][ts][sb] = lev * f32::from(ASPX_SINE[0][idx as usize]);
            let sign = if (sb as i32 + ssch.sbx) % 2 == 0 { 1.0 } else { -1.0 };
            ssch.qmf_sine[1][ts][sb] = lev * sign * f32::from(ASPX_SINE[1][idx as usize]);
        }
    }
}

/// ac4dec.c: assemble_hf_signal
fn assemble_hf_signal(f: Frame, ssch: &mut Ssc) {
    let hfadj = TS_OFFSET_HFADJ as usize;
    let n = f.num_qmf_timeslots as usize;
    let env = ssch.aspx_num_env as usize;
    let t0 = (ssch.atsg_sig[0] * f.num_ts_in_ats) as usize;
    let t1 = (ssch.atsg_sig[env] * f.num_ts_in_ats) as usize;
    let sbx = ssch.sbx as usize;
    let num_sb = ssch.num_sb_aspx as usize;
    let mut atsg = 0usize;

    *ssch.y_prev = *ssch.y;
    *ssch.y = [[[0.0; 64]; 42]; 2];

    for ts in 0..t0 {
        for sb in 0..num_sb {
            ssch.y[0][ts][sb] = ssch.y_prev[0][n + ts][sb];
            ssch.y[1][ts][sb] = ssch.y_prev[1][n + ts][sb];
        }
    }

    for ts in t0..t1 {
        if ts as i32 == at(&ssch.atsg_sig, atsg + 1) * f.num_ts_in_ats {
            atsg += 1;
        }
        for sb in 0..num_sb {
            let (r, i) = fcomplex_mul(
                ssch.sig_gain_sb_adj[atsg.min(4)][sb],
                0.0,
                ssch.q_high[0][ts + hfadj][sb + sbx],
                ssch.q_high[1][ts + hfadj][sb + sbx],
            );
            ssch.y[0][ts][sb] = r;
            ssch.y[1][ts][sb] = i;
        }
    }

    for ts in t0..t1 {
        for sb in 0..num_sb {
            ssch.y[0][ts][sb] += ssch.qmf_sine[0][ts][sb];
            ssch.y[1][ts][sb] += ssch.qmf_sine[1][ts][sb];
            ssch.y[0][ts][sb] += ssch.qmf_noise[0][ts][sb];
            ssch.y[1][ts][sb] += ssch.qmf_noise[1][ts][sb];
        }
    }

    for ts in t0..t1 {
        for sb in sbx..64 {
            ssch.q[0][ts][sb] += ssch.y[0][ts][sb - sbx] / 32768.0;
            ssch.q[1][ts][sb] += ssch.y[1][ts][sb - sbx] / 32768.0;
        }
    }

    *ssch.q_prev = *ssch.q;
}
