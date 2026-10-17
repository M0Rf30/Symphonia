// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A-SPX and A-CPL parameter data parsing (FFmpeg `ac4dec.c`, see `NOTICE`).

use symphonia_core::errors::{Result, decode_error};

use crate::bit::BitReader;
use crate::codebooks::codebooks;
use crate::ctx::*;
use crate::tables::*;
use crate::vlc::Vlc;

/// Upper bound on the iterations of the patch construction loop (the C loop is unbounded).
const MAX_PATCH_ITERATIONS: u32 = 64;

/// ac4dec.c: noise_mid_border
fn noise_mid_border(aspx_tsg_ptr: i32, aspx_int_class: i32, num_atsg_sig: i32) -> i32 {
    if aspx_tsg_ptr == -1 {
        if aspx_int_class == VARFIX { 1 } else { num_atsg_sig - 1 }
    }
    else if aspx_tsg_ptr >= 0 {
        if aspx_int_class == VARFIX {
            num_atsg_sig - 1
        }
        else {
            1i32.max((num_atsg_sig - 1).min(aspx_tsg_ptr))
        }
    }
    else {
        0
    }
}

/// ac4dec.c: freq_res
fn freq_res(
    atsg_sig: &[i32],
    atsg: usize,
    aspx_tsg_ptr: i32,
    num_aspx_timeslots: i32,
    aspx_freq_res_mode: i32,
    aspx_freq_res: &[i32],
) -> Result<i32> {
    match aspx_freq_res_mode {
        0 => match aspx_freq_res.get(atsg) {
            Some(&v) => Ok(v),
            None => decode_error("ac4: aspx freq res index out of range"),
        },
        1 => Ok(0),
        2 => {
            let (Some(&a), Some(&b)) = (atsg_sig.get(atsg), atsg_sig.get(atsg + 1))
            else {
                return decode_error("ac4: aspx atsg index out of range");
            };
            let width = f64::from(b.wrapping_sub(a));
            if ((atsg as i32) < aspx_tsg_ptr && num_aspx_timeslots > 8)
                || width > (f64::from(num_aspx_timeslots) / 6.0 + 3.25)
            {
                Ok(1)
            }
            else {
                Ok(0)
            }
        }
        3 => Ok(1),
        _ => decode_error("ac4: invalid aspx freq res mode"),
    }
}

/// ac4dec.c: get_tab_border
fn get_tab_border(atsg_sig: &mut [i32], num_aspx_timeslots: i32, num_atsg: i32) -> Result<()> {
    let ts_idx = match num_aspx_timeslots {
        6 => 0,
        8 => 1,
        12 => 2,
        15 => 3,
        16 => 4,
        _ => return decode_error("ac4: invalid number of aspx timeslots"),
    };

    if num_atsg < 0 {
        return decode_error("ac4: invalid aspx border count");
    }

    let col = (num_atsg >> 1) as usize;
    let size = num_atsg as usize + 1;

    let src = TAB_BORDER[ts_idx].get(col).and_then(|row| row.get(..size));
    let dst = atsg_sig.get_mut(..size);

    match (src, dst) {
        (Some(src), Some(dst)) => {
            dst.copy_from_slice(src);
            Ok(())
        }
        _ => decode_error("ac4: aspx border table out of range"),
    }
}

/// ac4dec.c: get_aspx_hcb
fn get_aspx_hcb(
    data_type: i32,
    quant_mode: i32,
    stereo_mode: i32,
    hcb_type: i32,
) -> Result<&'static Vlc> {
    let cb = codebooks();
    let (q, sm, h) = (quant_mode as usize, stereo_mode as usize, hcb_type as usize);

    let vlc = if data_type == DT_SIGNAL {
        cb.aspx_signal.get(sm).and_then(|a| a.get(q)).and_then(|a| a.get(h))
    }
    else {
        cb.aspx_noise.get(sm).and_then(|a| a.get(h))
    };

    match vlc {
        Some(vlc) => Ok(vlc),
        None => decode_error("ac4: invalid aspx codebook selection"),
    }
}

/// ac4dec.c: get_aspx_off
fn get_aspx_off(data_type: i32, quant_mode: i32, stereo_mode: i32, hcb_type: i32) -> Result<i32> {
    let (q, sm, h) = (quant_mode as usize, stereo_mode as usize, hcb_type as usize);

    let off = if data_type == DT_SIGNAL {
        ASPX_CODEBOOK_SIGNAL_OFF.get(sm).and_then(|a| a.get(q)).and_then(|a| a.get(h))
    }
    else {
        ASPX_CODEBOOK_NOISE_OFF.get(sm).and_then(|a| a.get(h))
    };

    match off {
        Some(&off) => Ok(i32::from(off)),
        None => decode_error("ac4: invalid aspx codebook selection"),
    }
}

/// ac4dec.c: aspx_huff_data
pub(crate) fn aspx_huff_data(
    br: &mut BitReader<'_>,
    data_type: i32,
    num_sbg: i32,
    quant_mode: i32,
    stereo_mode: i32,
    direction: i32,
    data: &mut [i32],
) -> Result<()> {
    let num_sbg = num_sbg.max(0) as usize;

    if direction == 0 {
        // FREQ
        let cb = get_aspx_hcb(data_type, quant_mode, stereo_mode, F0)?;
        let off = get_aspx_off(data_type, quant_mode, stereo_mode, F0)?;
        let Some(d0) = data.get_mut(0)
        else {
            return decode_error("ac4: aspx data out of range");
        };
        *d0 = cb.decode(br)? - off;

        let cb = get_aspx_hcb(data_type, quant_mode, stereo_mode, DF)?;
        let off = get_aspx_off(data_type, quant_mode, stereo_mode, DF)?;
        for i in 1..num_sbg {
            let Some(d) = data.get_mut(i)
            else {
                return decode_error("ac4: aspx data out of range");
            };
            *d = cb.decode(br)? - off;
        }
    }
    else {
        // TIME
        let cb = get_aspx_hcb(data_type, quant_mode, stereo_mode, DT)?;
        let off = get_aspx_off(data_type, quant_mode, stereo_mode, DT)?;
        for i in 0..num_sbg {
            let Some(d) = data.get_mut(i)
            else {
                return decode_error("ac4: aspx data out of range");
            };
            *d = cb.decode(br)? - off;
        }
    }

    Ok(())
}

/// ac4dec.c: is_element_of_sbg_patches
fn is_element_of_sbg_patches(sbg_lim_sbg: i32, sbg_patches: &[i32], num_sbg_patches: i32) -> bool {
    let n = (num_sbg_patches.max(-1) + 1) as usize;
    sbg_patches.iter().take(n).any(|&p| p == sbg_lim_sbg)
}

/// ac4dec.c: remove_element
fn remove_element(sbg_lim: &mut [i32], num_sbg_lim: i32, sbg: i32) {
    let (start, end) = (sbg.max(0) as usize, num_sbg_lim.max(0) as usize);
    for i in start..end {
        if let Some(&next) = sbg_lim.get(i + 1) {
            sbg_lim[i] = next;
        }
    }
}

impl Ac4Core {
    /// ac4dec.c: aspx_atsg
    pub(crate) fn aspx_atsg(&mut self, ch: usize, iframe: i32) -> Result<()> {
        let nts = self.num_aspx_timeslots;
        let freq_res_mode = self.substream.aspx_freq_res_mode;
        let c = &mut self.substream.ssch[ch];

        let num_atsg_sig = c.aspx_num_env;
        let num_atsg_noise = c.aspx_num_noise;

        if !(1..=5).contains(&num_atsg_sig) || !(1..=2).contains(&num_atsg_noise) {
            return decode_error("ac4: invalid aspx envelope count");
        }
        if c.aspx_int_class != FIXFIX
            && (!(0..=3).contains(&c.aspx_num_rel_left)
                || !(0..=3).contains(&c.aspx_num_rel_right)
                || c.aspx_num_rel_left + c.aspx_num_rel_right + 1 != num_atsg_sig)
        {
            return decode_error("ac4: invalid aspx relative border count");
        }

        let n_sig = num_atsg_sig as usize;
        let n_noise = num_atsg_noise as usize;

        if c.aspx_int_class == FIXFIX {
            get_tab_border(&mut c.atsg_sig, nts, num_atsg_sig)?;
            get_tab_border(&mut c.atsg_noise, nts, num_atsg_noise)?;
            c.atsg_freqres[0] =
                freq_res(&c.atsg_sig, 0, 0, nts, freq_res_mode, &c.aspx_freq_res)? as u8;
            for atsg in 1..n_sig {
                c.atsg_freqres[atsg] = c.atsg_freqres[0];
            }
        }
        else {
            let rel_left = c.aspx_num_rel_left as usize;
            let rel_right = c.aspx_num_rel_right as usize;

            match c.aspx_int_class {
                FIXVAR => {
                    c.atsg_sig[0] = 0;
                    c.atsg_sig[n_sig] = c.aspx_var_bord_right + nts;
                    for tsg in 0..rel_right {
                        c.atsg_sig[n_sig - tsg - 1] =
                            c.atsg_sig[n_sig - tsg] - c.aspx_rel_bord_right[tsg];
                    }
                }
                VARFIX => {
                    c.atsg_sig[0] =
                        if iframe != 0 { c.aspx_var_bord_left } else { c.previous_stop_pos - nts };
                    c.atsg_sig[n_sig] = nts;
                    for tsg in 0..rel_left {
                        c.atsg_sig[tsg + 1] = c.atsg_sig[tsg] + c.aspx_rel_bord_left[tsg];
                    }
                }
                VARVAR => {
                    c.atsg_sig[0] =
                        if iframe != 0 { c.aspx_var_bord_left } else { c.previous_stop_pos - nts };
                    c.atsg_sig[n_sig] = c.aspx_var_bord_right + nts;
                    for tsg in 0..rel_left {
                        c.atsg_sig[tsg + 1] = c.atsg_sig[tsg] + c.aspx_rel_bord_left[tsg];
                    }
                    for tsg in 0..rel_right {
                        c.atsg_sig[n_sig - tsg - 1] =
                            c.atsg_sig[n_sig - tsg] - c.aspx_rel_bord_right[tsg];
                    }
                }
                _ => return decode_error("ac4: invalid aspx interval class"),
            }

            c.atsg_noise[0] = c.atsg_sig[0];
            c.atsg_noise[n_noise] = c.atsg_sig[n_sig];
            if num_atsg_noise > 1 {
                let mid = noise_mid_border(c.aspx_tsg_ptr, c.aspx_int_class, num_atsg_sig);
                c.atsg_noise[1] = match c.atsg_sig.get(mid.max(0) as usize) {
                    Some(&v) => v,
                    None => return decode_error("ac4: invalid aspx noise border"),
                };
            }
            for atsg in 0..n_sig {
                c.atsg_freqres[atsg] = freq_res(
                    &c.atsg_sig,
                    atsg,
                    c.aspx_tsg_ptr,
                    nts,
                    freq_res_mode,
                    &c.aspx_freq_res,
                )? as u8;
            }
        }

        c.previous_stop_pos = c.atsg_sig[n_sig];

        for atsg in 0..n_sig {
            if c.atsg_freqres[atsg] != 0 {
                c.num_sbg_sig[atsg] = c.num_sbg_sig_highres;
                c.sbg_sig[atsg] = c.sbg_sig_highres;
            }
            else {
                c.num_sbg_sig[atsg] = c.num_sbg_sig_lowres;
                c.sbg_sig[atsg] = c.sbg_sig_lowres;
            }
        }

        Ok(())
    }

    /// ac4dec.c: aspx_framing
    pub(crate) fn aspx_framing(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        iframe: i32,
    ) -> Result<()> {
        let nts = self.num_aspx_timeslots;
        let env_bits_fixfix = self.substream.aspx_num_env_bits_fixfix;
        let freq_res_mode = self.substream.aspx_freq_res_mode;
        let c = &mut self.substream.ssch[ch];

        // Number of bits of the relative-border fields.
        let rel_bits = 1 + u32::from(nts > 8);

        c.aspx_num_rel_left = 0;
        c.aspx_num_rel_right = 0;

        c.aspx_int_class = cb_int_class(br)?;

        c.aspx_num_env_prev = c.aspx_num_env;

        match c.aspx_int_class {
            FIXFIX => {
                let bits = (env_bits_fixfix.clamp(0, 31) + 1) as u32;
                let num_env = 1 + i64::from(br.get(bits));
                if num_env > 4 {
                    return decode_error("ac4: invalid aspx num env in FIXFIX");
                }
                c.aspx_num_env = num_env as i32;

                if freq_res_mode == 0 {
                    c.aspx_freq_res[0] = br.get1() as i32;
                }
            }
            FIXVAR => {
                c.aspx_var_bord_right = br.get(2) as i32;
                c.aspx_num_rel_right = br.get(rel_bits) as i32;
                for i in 0..c.aspx_num_rel_right as usize {
                    c.aspx_rel_bord_right[i] = 2 * br.get(rel_bits) as i32 + 2;
                }
            }
            VARFIX => {
                if iframe != 0 {
                    c.aspx_var_bord_left = br.get(2) as i32;
                }
                c.aspx_num_rel_left = br.get(rel_bits) as i32;
                for i in 0..c.aspx_num_rel_left as usize {
                    c.aspx_rel_bord_left[i] = 2 * br.get(rel_bits) as i32 + 2;
                }
            }
            _ => {
                // VARVAR
                if iframe != 0 {
                    c.aspx_var_bord_left = br.get(2) as i32;
                }
                c.aspx_num_rel_left = br.get(rel_bits) as i32;
                for i in 0..c.aspx_num_rel_left as usize {
                    c.aspx_rel_bord_left[i] = 2 * br.get(rel_bits) as i32 + 2;
                }
                c.aspx_var_bord_right = br.get(2) as i32;
                c.aspx_num_rel_right = br.get(rel_bits) as i32;
                for i in 0..c.aspx_num_rel_right as usize {
                    c.aspx_rel_bord_right[i] = 2 * br.get(rel_bits) as i32 + 2;
                }
            }
        }

        if c.aspx_int_class != FIXFIX {
            c.aspx_num_env = c.aspx_num_rel_left + c.aspx_num_rel_right + 1;
            if c.aspx_num_env > 5 {
                return decode_error("ac4: invalid aspx num env");
            }

            let ptr_bits = (((c.aspx_num_env + 2) as f32).ln() / 2.0f32.ln()).ceil() as u32;
            c.aspx_tsg_ptr_prev = c.aspx_tsg_ptr;
            c.aspx_tsg_ptr = br.get(ptr_bits) as i32 - 1;
            if freq_res_mode == 0 {
                for env in 0..c.aspx_num_env as usize {
                    c.aspx_freq_res[env] = br.get1() as i32;
                }
            }
        }

        c.aspx_num_noise_prev = c.aspx_num_noise;

        c.aspx_num_noise = if c.aspx_num_env > 1 { 2 } else { 1 };

        if c.aspx_num_env_prev == 0 {
            c.aspx_num_env_prev = c.aspx_num_env;
        }
        if c.aspx_num_noise_prev == 0 {
            c.aspx_num_noise_prev = c.aspx_num_noise;
        }

        self.aspx_atsg(ch, iframe)
    }

    /// ac4dec.c: aspx_delta_dir
    pub(crate) fn aspx_delta_dir(&mut self, br: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let c = &mut self.substream.ssch[ch];

        if !(0..=5).contains(&c.aspx_num_env) || !(0..=2).contains(&c.aspx_num_noise) {
            return decode_error("ac4: invalid aspx envelope count");
        }

        for env in 0..c.aspx_num_env as usize {
            c.aspx_sig_delta_dir[env] = br.get1() as i32;
        }

        for env in 0..c.aspx_num_noise as usize {
            c.aspx_noise_delta_dir[env] = br.get1() as i32;
        }

        Ok(())
    }

    /// ac4dec.c: aspx_hfgen_iwc_2ch
    pub(crate) fn aspx_hfgen_iwc_2ch(
        &mut self,
        br: &mut BitReader<'_>,
        ch0: usize,
        ch1: usize,
        aspx_balance: i32,
    ) -> Result<()> {
        let nts = self.num_aspx_timeslots;
        if !(0..=16).contains(&nts) {
            return decode_error("ac4: invalid number of aspx timeslots");
        }
        let nts = nts as usize;

        let num_noise = self.substream.ssch[ch0].num_sbg_noise;
        let num_highres = self.substream.ssch[ch0].num_sbg_sig_highres;
        if !(0..=5).contains(&num_noise) || !(0..=22).contains(&num_highres) {
            return decode_error("ac4: invalid aspx subband group count");
        }
        let (num_noise, num_highres) = (num_noise as usize, num_highres as usize);

        let mut aspx_tic_left = 0;
        let mut aspx_tic_right = 0;

        {
            let c0 = &mut self.substream.ssch[ch0];
            c0.aspx_tna_mode_prev = c0.aspx_tna_mode;
        }
        {
            let c1 = &mut self.substream.ssch[ch1];
            c1.aspx_tna_mode_prev = c1.aspx_tna_mode;
        }

        for n in 0..num_noise {
            self.substream.ssch[ch0].aspx_tna_mode[n] = br.get(2) as i32;
        }
        if aspx_balance == 0 {
            for n in 0..num_noise {
                self.substream.ssch[ch1].aspx_tna_mode[n] = br.get(2) as i32;
            }
        }
        else {
            for n in 0..num_noise {
                let v = self.substream.ssch[ch0].aspx_tna_mode[n];
                self.substream.ssch[ch1].aspx_tna_mode[n] = v;
            }
        }
        if br.get1() != 0 {
            for n in 0..num_highres {
                self.substream.ssch[ch0].aspx_add_harmonic[n] = br.get1() as i32;
            }
        }
        if br.get1() != 0 {
            for n in 0..num_highres {
                self.substream.ssch[ch1].aspx_add_harmonic[n] = br.get1() as i32;
            }
        }

        for n in 0..num_highres {
            self.substream.ssch[ch0].aspx_fic_used_in_sfb[n] = 0;
            self.substream.ssch[ch1].aspx_fic_used_in_sfb[n] = 0;
        }

        if br.get1() != 0 {
            if br.get1() != 0 {
                for n in 0..num_highres {
                    self.substream.ssch[ch0].aspx_fic_used_in_sfb[n] = br.get1() as i32;
                }
            }

            if br.get1() != 0 {
                for n in 0..num_highres {
                    self.substream.ssch[ch1].aspx_fic_used_in_sfb[n] = br.get1() as i32;
                }
            }
        }

        for n in 0..nts {
            self.substream.ssch[ch0].aspx_tic_used_in_slot[n] = 0;
            self.substream.ssch[ch1].aspx_tic_used_in_slot[n] = 0;
        }

        if br.get1() != 0 {
            let aspx_tic_copy = br.get1();

            if aspx_tic_copy == 0 {
                aspx_tic_left = br.get1();
                aspx_tic_right = br.get1();
            }

            if aspx_tic_copy != 0 || aspx_tic_left != 0 {
                for n in 0..nts {
                    self.substream.ssch[ch0].aspx_tic_used_in_slot[n] = br.get1() as i32;
                }
            }

            if aspx_tic_right != 0 {
                for n in 0..nts {
                    self.substream.ssch[ch1].aspx_tic_used_in_slot[n] = br.get1() as i32;
                }
            }

            if aspx_tic_copy != 0 {
                for n in 0..nts {
                    let v = self.substream.ssch[ch0].aspx_tic_used_in_slot[n];
                    self.substream.ssch[ch1].aspx_tic_used_in_slot[n] = v;
                }
            }
        }

        Ok(())
    }

    /// ac4dec.c: aspx_ec_data
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn aspx_ec_data(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        data_type: i32,
        num_env: i32,
        freq_res: [u8; 5],
        quant_mode: i32,
        stereo_mode: i32,
        direction: [i32; 8],
    ) -> Result<()> {
        let c = &mut self.substream.ssch[ch];

        let dt = if data_type == DT_SIGNAL { 0 } else { 1 };
        if !(0..=5).contains(&num_env) {
            return decode_error("ac4: invalid aspx envelope count");
        }

        for env in 0..num_env as usize {
            let num_sbg = if data_type == DT_SIGNAL {
                if freq_res[env] != 0 { c.num_sbg_sig_highres } else { c.num_sbg_sig_lowres }
            }
            else {
                c.num_sbg_noise
            };
            let dir = direction.get(env).copied().unwrap_or(0);
            aspx_huff_data(
                br,
                data_type,
                num_sbg,
                quant_mode,
                stereo_mode,
                dir,
                &mut c.aspx_data[dt][env],
            )?;
        }

        Ok(())
    }

    /// ac4dec.c: aspx_elements
    pub(crate) fn aspx_elements(&mut self, ch: usize, iframe: i32) -> Result<()> {
        let fs_index = self.fs_index;
        let ss = &*self.substream;
        let start_freq = ss.aspx_start_freq;
        let stop_freq = ss.aspx_stop_freq;
        let master_freq_scale = ss.aspx_master_freq_scale;
        let noise_sbg = ss.aspx_noise_sbg;
        let master_reset = (i32::from(ss.prev_aspx_start_freq != start_freq)
            + i32::from(ss.prev_aspx_stop_freq != stop_freq)
            + i32::from(ss.prev_aspx_master_freq_scale != master_freq_scale))
            * iframe;
        let c = &mut self.substream.ssch[ch];

        c.master_reset = master_reset;
        if c.master_reset != 0 {
            let (num_master, template): (i32, &[u8]) = if master_freq_scale == 1 {
                (22 - 2 * start_freq - 2 * stop_freq, &SBG_TEMPLATE_HIGHRES)
            }
            else {
                (20 - 2 * start_freq - 2 * stop_freq, &SBG_TEMPLATE_LOWRES)
            };

            if !(0..=22).contains(&num_master) || start_freq < 0 {
                return decode_error("ac4: invalid aspx master band count");
            }

            for sbg in 0..=num_master {
                match template.get((2 * start_freq + sbg) as usize) {
                    Some(&v) => c.sbg_master[sbg as usize] = i32::from(v),
                    None => return decode_error("ac4: aspx master band table out of range"),
                }
            }
            c.num_sbg_master = num_master;
        }

        if !(0..=22).contains(&c.num_sbg_master) {
            return decode_error("ac4: invalid aspx master band count");
        }
        let num_master = c.num_sbg_master;

        c.sba = c.sbg_master[0];
        c.sbz = c.sbg_master[num_master as usize];

        let offset = c.aspx_xover_subband_offset;
        let num_highres = num_master - offset;
        if offset < 0 || num_highres < 0 {
            return decode_error("ac4: invalid aspx crossover subband offset");
        }
        c.num_sbg_sig_highres = num_highres;
        for sbg in 0..=num_highres {
            c.sbg_sig_highres[sbg as usize] = c.sbg_master[(sbg + offset) as usize];
        }

        c.sbx = c.sbg_sig_highres[0];
        if c.sbx <= 0 {
            return decode_error("ac4: invalid aspx start subband");
        }
        c.num_sb_aspx = c.sbg_sig_highres[num_highres as usize] - c.sbx;

        let num_lowres = num_highres - num_highres / 2;
        c.num_sbg_sig_lowres = num_lowres;
        c.sbg_sig_lowres[0] = c.sbg_sig_highres[0];
        if (num_highres & 1) == 0 {
            for sbg in 1..=num_lowres as usize {
                c.sbg_sig_lowres[sbg] = c.sbg_sig_highres[2 * sbg];
            }
        }
        else {
            for sbg in 1..=num_lowres as usize {
                c.sbg_sig_lowres[sbg] = c.sbg_sig_highres[2 * sbg - 1];
            }
        }

        c.num_sbg_sig[0] = num_lowres;
        c.num_sbg_sig[1] = num_highres;

        // The C expression mixes float and double: float product, double `+ 0.5`, float floor.
        let noise_f = (noise_sbg as f32 * (c.sbz as f32 / c.sbx as f32).log2()) as f64 + 0.5;
        let noise_f = (noise_f as f32).floor();
        let noise_f = if 1.0 > noise_f { 1.0 } else { noise_f };
        if noise_f.is_nan() {
            return decode_error("ac4: invalid num sbg noise");
        }
        c.num_sbg_noise = noise_f as i32;
        if c.num_sbg_noise > 5 {
            return decode_error("ac4: invalid num sbg noise");
        }
        let num_noise = c.num_sbg_noise as usize;

        let mut idx = [0i32; 6];
        c.sbg_noise[0] = c.sbg_sig_lowres[0];
        for sbg in 1..=num_noise {
            idx[sbg] = idx[sbg - 1];
            idx[sbg] +=
                ((num_lowres - idx[sbg - 1]) as f32 / (num_noise + 1 - sbg) as f32).floor() as i32;
            match c.sbg_sig_lowres.get(idx[sbg].max(0) as usize) {
                Some(&v) if idx[sbg] >= 0 => c.sbg_noise[sbg] = v,
                _ => return decode_error("ac4: invalid aspx noise band index"),
            }
        }

        let mut msb = c.sba;
        let mut usb = c.sbx;
        c.num_sbg_patches = 0;
        let goal_sb = if fs_index != 0 { 43 } else { 46 };
        let source_band_low = if master_freq_scale == 1 { 4 } else { 2 };

        let mut sbg: i32;
        if goal_sb < c.sbx + c.num_sb_aspx {
            sbg = 0;
            for i in 0..=num_master {
                if c.sbg_master[i as usize] >= goal_sb {
                    break;
                }
                sbg = i + 1;
            }
        }
        else {
            sbg = num_master;
        }

        let mut iterations = 0;
        loop {
            iterations += 1;
            if iterations > MAX_PATCH_ITERATIONS || sbg < 0 || sbg > num_master {
                return decode_error("ac4: invalid aspx patch construction");
            }

            let mut j = sbg;
            let mut sb = c.sbg_master[j as usize];
            let mut odd = (sb - 2 + c.sba) % 2;

            while sb > (c.sba - source_band_low + msb - odd) && j >= 1 {
                j -= 1;
                sb = c.sbg_master[j as usize];
                odd = (sb - 2 + c.sba) % 2;
            }

            let patch_num_sb = (sb - usb).max(0);
            let np = c.num_sbg_patches as usize;
            if np < 6 {
                c.sbg_patch_num_sb[np] = patch_num_sb;
                c.sbg_patch_start_sb[np] = c.sba - odd - patch_num_sb;
            }
            if patch_num_sb > 0 {
                if np >= 6 {
                    return decode_error("ac4: too many aspx patches");
                }
                usb = sb;
                msb = sb;
                c.num_sbg_patches += 1;
            }
            else {
                msb = c.sbx;
            }

            if c.sbg_master[sbg as usize] - sb < 3 {
                sbg = num_master;
            }

            if !(sb != (c.sbx + c.num_sb_aspx) && j > 0) {
                break;
            }
        }

        if c.num_sbg_patches > 1 && c.sbg_patch_num_sb[c.num_sbg_patches as usize - 1] < 3 {
            c.num_sbg_patches -= 1;
        }

        // The patch border table holds `num_sbg_patches + 1` entries.
        if c.num_sbg_patches >= 6 {
            return decode_error("ac4: too many aspx patches");
        }
        let num_patches = c.num_sbg_patches as usize;

        c.sbg_patches[0] = c.sbx;
        for i in 1..=num_patches {
            c.sbg_patches[i] = c.sbg_patches[i - 1] + c.sbg_patch_num_sb[i - 1];
        }

        // Copy sbg_sig_lowres into lower part of limiter table.
        for sbg in 0..=num_lowres as usize {
            c.sbg_lim[sbg] = c.sbg_sig_lowres[sbg];
        }

        // Copy patch borders into higher part of limiter table.
        for sbg in 1..num_patches {
            c.sbg_lim[sbg + num_lowres as usize] = c.sbg_patches[sbg];
        }

        // Sort patch borders + low res sbg into temporary limiter table.
        c.num_sbg_lim = num_lowres + c.num_sbg_patches - 1;
        if c.num_sbg_lim > 1 {
            c.sbg_lim[..c.num_sbg_lim as usize].sort_unstable();
        }
        let mut sbg = 1usize;

        while sbg as i32 <= c.num_sbg_lim {
            let num_octaves =
                (f64::from(c.sbg_lim[sbg] as f32 / c.sbg_lim[sbg - 1] as f32)).log2() as f32;

            if f64::from(num_octaves) < 0.245 {
                if c.sbg_lim[sbg] == c.sbg_lim[sbg - 1] {
                    remove_element(&mut c.sbg_lim, c.num_sbg_lim, sbg as i32);
                    c.num_sbg_lim -= 1;
                }
                else if is_element_of_sbg_patches(
                    c.sbg_lim[sbg],
                    &c.sbg_patches,
                    c.num_sbg_patches,
                ) {
                    if is_element_of_sbg_patches(
                        c.sbg_lim[sbg - 1],
                        &c.sbg_patches,
                        c.num_sbg_patches,
                    ) {
                        sbg += 1;
                    }
                    else {
                        remove_element(&mut c.sbg_lim, c.num_sbg_lim, sbg as i32 - 1);
                        c.num_sbg_lim -= 1;
                    }
                }
                else {
                    remove_element(&mut c.sbg_lim, c.num_sbg_lim, sbg as i32);
                    c.num_sbg_lim -= 1;
                }
            }
            else {
                sbg += 1;
            }
        }

        Ok(())
    }

    /// ac4dec.c: aspx_data_2ch
    pub(crate) fn aspx_data_2ch(
        &mut self,
        br: &mut BitReader<'_>,
        ch0: usize,
        ch1: usize,
        iframe: i32,
    ) -> Result<()> {
        let quant_mode_env = self.substream.aspx_quant_mode_env;

        if iframe != 0 {
            let v = br.get(3) as i32;
            self.substream.ssch[ch0].aspx_xover_subband_offset = v;
            self.substream.ssch[ch1].aspx_xover_subband_offset = v;
        }

        self.aspx_elements(ch0, iframe)?;
        self.aspx_elements(ch1, iframe)?;

        self.aspx_framing(br, ch0, iframe)?;

        let qmode = {
            let c0 = &self.substream.ssch[ch0];
            if c0.aspx_int_class == FIXFIX && c0.aspx_num_env == 1 { 0 } else { quant_mode_env }
        };
        self.substream.ssch[ch0].aspx_qmode_env = qmode;
        self.substream.ssch[ch1].aspx_qmode_env = qmode;

        let balance = br.get1() as i32;
        self.substream.ssch[ch0].aspx_balance = balance;
        self.substream.ssch[ch1].aspx_balance = balance;

        if balance == 0 {
            self.aspx_framing(br, ch1, iframe)?;
            let c1 = &mut self.substream.ssch[ch1];
            c1.aspx_qmode_env = quant_mode_env;
            if c1.aspx_int_class == FIXFIX && c1.aspx_num_env == 1 {
                c1.aspx_qmode_env = 0;
            }
        }
        else {
            let (num_env, num_noise, freqres) = {
                let c0 = &self.substream.ssch[ch0];
                (c0.aspx_num_env, c0.aspx_num_noise, c0.atsg_freqres)
            };
            let c1 = &mut self.substream.ssch[ch1];
            c1.aspx_num_env = num_env;
            c1.aspx_num_noise = num_noise;
            c1.atsg_freqres = freqres;
        }

        self.aspx_delta_dir(br, ch0)?;
        self.aspx_delta_dir(br, ch1)?;
        self.aspx_hfgen_iwc_2ch(br, ch0, ch1, balance)?;

        let stereo_mode1 = if balance != 0 { SM_BALANCE } else { SM_LEVEL };

        let (num_env, freqres, qmode, dir) = {
            let c = &self.substream.ssch[ch0];
            (c.aspx_num_env, c.atsg_freqres, c.aspx_qmode_env, sig_dir(c))
        };
        self.aspx_ec_data(br, ch0, DT_SIGNAL, num_env, freqres, qmode, SM_LEVEL, dir)?;

        let (num_env, freqres, qmode, dir) = {
            let c = &self.substream.ssch[ch1];
            (c.aspx_num_env, c.atsg_freqres, c.aspx_qmode_env, sig_dir(c))
        };
        self.aspx_ec_data(br, ch1, DT_SIGNAL, num_env, freqres, qmode, stereo_mode1, dir)?;

        let (num_noise, dir) = {
            let c = &self.substream.ssch[ch0];
            (c.aspx_num_noise, noise_dir(c))
        };
        self.aspx_ec_data(br, ch0, DT_NOISE, num_noise, [0; 5], 0, SM_LEVEL, dir)?;

        let (num_noise, dir) = {
            let c = &self.substream.ssch[ch1];
            (c.aspx_num_noise, noise_dir(c))
        };
        self.aspx_ec_data(br, ch1, DT_NOISE, num_noise, [0; 5], 0, stereo_mode1, dir)
    }

    /// ac4dec.c: aspx_hfgen_iwc_1ch
    pub(crate) fn aspx_hfgen_iwc_1ch(&mut self, br: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let nts = self.num_aspx_timeslots;
        if !(0..=16).contains(&nts) {
            return decode_error("ac4: invalid number of aspx timeslots");
        }
        let nts = nts as usize;

        let c = &mut self.substream.ssch[ch];
        if !(0..=5).contains(&c.num_sbg_noise) || !(0..=22).contains(&c.num_sbg_sig_highres) {
            return decode_error("ac4: invalid aspx subband group count");
        }
        let (num_noise, num_highres) = (c.num_sbg_noise as usize, c.num_sbg_sig_highres as usize);

        c.aspx_tna_mode_prev = c.aspx_tna_mode;

        for n in 0..num_noise {
            c.aspx_tna_mode[n] = br.get(2) as i32;
        }
        if br.get1() != 0 {
            for n in 0..num_highres {
                c.aspx_add_harmonic[n] = br.get1() as i32;
            }
        }

        for n in 0..num_highres {
            c.aspx_fic_used_in_sfb[n] = 0;
        }

        if br.get1() != 0 {
            for n in 0..num_highres {
                c.aspx_fic_used_in_sfb[n] = br.get1() as i32;
            }
        }

        for n in 0..nts {
            c.aspx_tic_used_in_slot[n] = 0;
        }

        if br.get1() != 0 {
            for n in 0..nts {
                c.aspx_tic_used_in_slot[n] = br.get1() as i32;
            }
        }

        Ok(())
    }

    /// ac4dec.c: aspx_data_1ch
    pub(crate) fn aspx_data_1ch(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        iframe: i32,
    ) -> Result<()> {
        let quant_mode_env = self.substream.aspx_quant_mode_env;

        if iframe != 0 {
            self.substream.ssch[ch].aspx_xover_subband_offset = br.get(3) as i32;
        }

        self.substream.ssch[ch].aspx_balance = 0;

        self.aspx_elements(ch, iframe)?;
        self.aspx_framing(br, ch, iframe)?;

        {
            let c = &mut self.substream.ssch[ch];
            c.aspx_qmode_env = quant_mode_env;
            if c.aspx_int_class == FIXFIX && c.aspx_num_env == 1 {
                c.aspx_qmode_env = 0;
            }
        }

        self.aspx_delta_dir(br, ch)?;
        self.aspx_hfgen_iwc_1ch(br, ch)?;

        let (num_env, freqres, qmode, dir) = {
            let c = &self.substream.ssch[ch];
            (c.aspx_num_env, c.atsg_freqres, c.aspx_qmode_env, sig_dir(c))
        };
        self.aspx_ec_data(br, ch, DT_SIGNAL, num_env, freqres, qmode, 0, dir)?;

        let (num_noise, dir) = {
            let c = &self.substream.ssch[ch];
            (c.aspx_num_noise, noise_dir(c))
        };
        self.aspx_ec_data(br, ch, DT_NOISE, num_noise, [0; 5], 0, 0, dir)
    }

    /// ac4dec.c: acpl_framing_data
    pub(crate) fn acpl_framing_data(&mut self, br: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let c = &mut self.substream.ssch[ch];

        c.acpl_interpolation_type = br.get1() as i32;
        c.acpl_num_param_sets_cod = br.get1() as i32;
        if c.acpl_interpolation_type != 0 {
            for ps in 0..(c.acpl_num_param_sets_cod + 1) as usize {
                c.acpl_param_timeslot[ps] = br.get(5) as i32;
            }
        }

        Ok(())
    }

    /// ac4dec.c: acpl_ec_data
    pub(crate) fn acpl_ec_data(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        data_type: usize,
        data_bands: i32,
        start_band: i32,
        quant_mode: i32,
    ) -> Result<()> {
        let c = &mut self.substream.ssch[ch];

        for _ps in 0..c.acpl_num_param_sets_cod + 1 {
            let Some(data) = c.acpl_data.get_mut(data_type)
            else {
                return decode_error("ac4: invalid acpl data type");
            };
            acpl_huff_data(br, data_type, data_bands, start_band, quant_mode, data)?;
        }

        Ok(())
    }

    /// ac4dec.c: acpl_data_2ch
    pub(crate) fn acpl_data_2ch(
        &mut self,
        br: &mut BitReader<'_>,
        ch0: usize,
        ch1: usize,
    ) -> Result<()> {
        self.acpl_framing_data(br, ch0)?;

        let (num_bands, st, q0, q1) = self.acpl_params()?;

        for t in [ALPHA1, ALPHA2, BETA1, BETA2, BETA3] {
            self.acpl_ec_data(br, ch0, t, num_bands, st, q0)?;
        }
        for t in [GAMMA1, GAMMA2, GAMMA3, GAMMA4, GAMMA5, GAMMA6] {
            self.acpl_ec_data(br, ch1, t, num_bands, st, q1)?;
        }

        Ok(())
    }

    /// ac4dec.c: acpl_data_1ch
    pub(crate) fn acpl_data_1ch(&mut self, br: &mut BitReader<'_>, ch: usize) -> Result<()> {
        self.acpl_framing_data(br, ch)?;

        let (num_bands, start, q0, _) = self.acpl_params()?;

        self.acpl_ec_data(br, ch, ALPHA1, num_bands, start, q0)?;
        self.acpl_ec_data(br, ch, BETA1, num_bands, start, q0)
    }

    /// Returns `(num_bands, acpl_param_band, acpl_quant_mode[0], acpl_quant_mode[1])`.
    fn acpl_params(&self) -> Result<(i32, i32, i32, i32)> {
        let ss = &*self.substream;

        match usize::try_from(ss.acpl_num_param_bands_id)
            .ok()
            .and_then(|id| ACPL_NUM_PARAM_BANDS.get(id))
        {
            Some(&n) => {
                Ok((i32::from(n), ss.acpl_param_band, ss.acpl_quant_mode[0], ss.acpl_quant_mode[1]))
            }
            None => decode_error("ac4: invalid acpl num param bands id"),
        }
    }
}

/// ac4dec.c: aspx_framing (`get_vlc2` of `aspx_int_class_vlc`)
fn cb_int_class(br: &mut BitReader<'_>) -> Result<i32> {
    let v = codebooks().aspx_int_class.decode(br)?;
    if !(FIXFIX..=VARVAR).contains(&v) {
        return decode_error("ac4: invalid aspx int class");
    }
    Ok(v)
}

/// Signal envelope delta directions (`aspx_sig_delta_dir`).
fn sig_dir(c: &Ssc) -> [i32; 8] {
    c.aspx_sig_delta_dir
}

/// Noise envelope delta directions (`aspx_noise_delta_dir`), padded to the signal length.
fn noise_dir(c: &Ssc) -> [i32; 8] {
    let mut dir = [0; 8];
    dir[..2].copy_from_slice(&c.aspx_noise_delta_dir);
    dir
}

/// ac4dec.c: get_acpl_hcb
fn get_acpl_hcb(data_type: usize, quant_mode: i32, hcb_type: i32) -> Result<&'static Vlc> {
    let vlc = usize::try_from(quant_mode).ok().and_then(|q| {
        let h = usize::try_from(hcb_type).ok()?;
        codebooks().acpl.get(data_type)?.get(q)?.get(h)
    });

    match vlc {
        Some(vlc) => Ok(vlc),
        None => decode_error("ac4: invalid acpl codebook selection"),
    }
}

/// ac4dec.c: acpl_huff_data
fn acpl_huff_data(
    br: &mut BitReader<'_>,
    data_type: usize,
    data_bands: i32,
    start_band: i32,
    quant_mode: i32,
    data: &mut [i32],
) -> Result<()> {
    let class = match data_type {
        ALPHA1 | ALPHA2 => 0,
        BETA1 | BETA2 => 1,
        BETA3 => 2,
        _ => 3,
    };

    let (Ok(start), Ok(bands)) = (usize::try_from(start_band), usize::try_from(data_bands))
    else {
        return decode_error("ac4: invalid acpl band range");
    };

    let diff_type = br.get1();
    if diff_type == 0 {
        // DIFF_FREQ
        let cb = get_acpl_hcb(class, quant_mode, F0)?;
        let Some(d) = data.get_mut(start)
        else {
            return decode_error("ac4: acpl data out of range");
        };
        *d = cb.decode(br)?;

        let cb = get_acpl_hcb(class, quant_mode, DF)?;
        for i in start + 1..bands {
            let Some(d) = data.get_mut(i)
            else {
                return decode_error("ac4: acpl data out of range");
            };
            *d = cb.decode(br)?;
        }
    }
    else {
        // DIFF_TIME
        let cb = get_acpl_hcb(class, quant_mode, DT)?;
        for i in start..bands {
            let Some(d) = data.get_mut(i)
            else {
                return decode_error("ac4: acpl data out of range");
            };
            *d = cb.decode(br)?;
        }
    }

    Ok(())
}
