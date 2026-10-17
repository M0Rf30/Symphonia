// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ASF/SSF spectral parsing, section and scale factor data, stereo parameters and the
//! A-SPX/A-CPL configuration (FFmpeg `ac4dec.c`, see `NOTICE`).

use symphonia_core::errors::{Result, decode_error};

use crate::bit::BitReader;
use crate::codebooks::codebooks;
use crate::ctx::*;
use crate::tables::*;

/// ac4dec.c: sb_to_pb
fn sb_to_pb(acpl_num_param_bands_id: i32, acpl_qmf_band: i32) -> i32 {
    let row = match acpl_qmf_band {
        0..=8 => acpl_qmf_band as usize,
        9..=10 => 9,
        11..=13 => 10,
        14..=17 => 11,
        18..=22 => 12,
        23..=34 => 13,
        35..=63 => 14,
        _ => return 0,
    };
    i32::from(QMF_SUBBANDS[row][(acpl_num_param_bands_id & 3) as usize])
}

/// ac4dec.c: get_msfb_bits
fn get_msfb_bits(transf_length: i32) -> u32 {
    if (384..=2048).contains(&transf_length) {
        6
    }
    else if (192..=256).contains(&transf_length) {
        5
    }
    else {
        4
    }
}

/// ac4dec.c: get_side_bits
fn get_side_bits(transf_length: i32) -> u32 {
    if (480..=2048).contains(&transf_length) {
        5
    }
    else if (240..=384).contains(&transf_length) {
        4
    }
    else {
        3
    }
}

/// ac4dec.c: get_sfb_offset
pub(crate) fn get_sfb_offset(transf_length: i32) -> &'static [u16] {
    match transf_length {
        2048 => &SFB_OFFSET_48KHZ_2048,
        1920 => &SFB_OFFSET_48KHZ_1920,
        1536 => &SFB_OFFSET_48KHZ_1536,
        1024 => &SFB_OFFSET_48KHZ_1024,
        960 => &SFB_OFFSET_48KHZ_960,
        768 => &SFB_OFFSET_48KHZ_768,
        512 => &SFB_OFFSET_48KHZ_512,
        480 => &SFB_OFFSET_48KHZ_480,
        384 => &SFB_OFFSET_48KHZ_384,
        256 => &SFB_OFFSET_48KHZ_256,
        240 => &SFB_OFFSET_48KHZ_240,
        192 => &SFB_OFFSET_48KHZ_192,
        128 => &SFB_OFFSET_48KHZ_128,
        120 => &SFB_OFFSET_48KHZ_120,
        _ => &SFB_OFFSET_48KHZ_96,
    }
}

/// ac4dec.c: num_sfb_48
fn num_sfb_48(transf_length: i32) -> i32 {
    match transf_length {
        2048 => 63,
        1920 => 61,
        1536 => 55,
        1024 | 960 => 49,
        768 => 43,
        512 | 480 => 36,
        384 => 33,
        256 | 240 => 20,
        192 => 18,
        128 | 120 => 14,
        _ => 12,
    }
}

/// ac4dec.c: get_max_sfb
fn max_sfb_of(frame_len_base: i32, ssch: &Ssc, g: i32) -> i32 {
    let scp = &ssch.scp;
    let mut idx = 0;

    if frame_len_base >= 1536
        && scp.long_frame == 0
        && scp.transf_length_idx[0] != scp.transf_length_idx[1]
    {
        let num_windows_0 = 1usize << (3 - u32::from(scp.transf_length_idx[0] & 3));
        if g >= i32::from(scp.window_to_group[num_windows_0.min(15)]) {
            idx = 1;
        }
    }

    if scp.side_limited == 1 || (scp.dual_maxsfb == 1 && scp.side_channel == 1) {
        i32::from(scp.max_sfb_side[idx])
    }
    else {
        i32::from(scp.max_sfb[idx])
    }
}

/// ac4dec.c: get_transf_length
fn transf_length_of(frame_len_base: i32, ssch: &Ssc, g: i32, idx: Option<&mut i32>) -> i32 {
    let tab = transf_length_48khz(match frame_len_base {
        2048 => 0,
        1920 => 1,
        1536 => 2,
        1024 => 3,
        960 => 4,
        768 => 5,
        512 => 6,
        _ => 7,
    });
    let scp = &ssch.scp;
    let (i, len) = if frame_len_base >= 1536 {
        if scp.long_frame == 0 {
            let num_windows_0 = 1usize << (3 - u32::from(scp.transf_length_idx[0] & 3));
            let k = if g < i32::from(scp.window_to_group[num_windows_0.min(15)]) {
                scp.transf_length_idx[0]
            }
            else {
                scp.transf_length_idx[1]
            };
            (i32::from(k), i32::from(tab[usize::from(k).min(4)]))
        }
        else {
            (4, frame_len_base)
        }
    }
    else {
        let k = scp.transf_length_idx[0];
        (i32::from(k), i32::from(tab[usize::from(k).min(4)]))
    };
    if let Some(r) = idx {
        *r = i;
    }
    len
}

/// ac4dec.c: ext_decode
fn ext_decode(br: &mut BitReader<'_>) -> Result<i32> {
    let mut n_ext = 0u32;
    while br.get1() != 0 {
        n_ext += 1;
        if n_ext > 11 {
            return decode_error("ac4: ext_decode overflow");
        }
    }
    let ext_val = br.get(n_ext + 4) as i32;
    Ok((1 << (n_ext + 4)) + ext_val)
}

impl Ac4Core {
    /// ac4dec.c: get_max_sfb
    pub(crate) fn get_max_sfb(&self, ch: usize, g: i32) -> i32 {
        max_sfb_of(self.frame_len_base, &self.substream.ssch[ch], g)
    }

    /// ac4dec.c: get_transf_length
    pub(crate) fn get_transf_length(&self, ch: usize, g: i32, idx: Option<&mut i32>) -> i32 {
        transf_length_of(self.frame_len_base, &self.substream.ssch[ch], g, idx)
    }

    /// ac4dec.c: acpl_config_1ch
    pub(crate) fn acpl_config_1ch(&mut self, br: &mut BitReader<'_>, mode: i32) -> Result<()> {
        let ss = &mut self.substream;
        ss.acpl_qmf_band = 0;
        ss.acpl_param_band = 0;
        ss.acpl_num_param_bands_id = br.get(2) as i32;
        ss.acpl_quant_mode[0] = br.get1() as i32;
        if mode == ACPL_PARTIAL {
            ss.acpl_qmf_band = br.get(3) as i32 + 1;
            ss.acpl_param_band = sb_to_pb(ss.acpl_num_param_bands_id, ss.acpl_qmf_band);
        }
        Ok(())
    }

    /// ac4dec.c: acpl_config_2ch
    pub(crate) fn acpl_config_2ch(&mut self, br: &mut BitReader<'_>) -> Result<()> {
        let ss = &mut self.substream;
        ss.acpl_qmf_band = 0;
        ss.acpl_param_band = 0;
        ss.acpl_num_param_bands_id = br.get(2) as i32;
        ss.acpl_quant_mode[0] = br.get1() as i32;
        ss.acpl_quant_mode[1] = br.get1() as i32;
        Ok(())
    }

    /// ac4dec.c: aspx_config
    pub(crate) fn aspx_config(&mut self, br: &mut BitReader<'_>) {
        let ss = &mut self.substream;
        ss.aspx_quant_mode_env = br.get1() as i32;
        ss.prev_aspx_start_freq = ss.aspx_start_freq;
        ss.aspx_start_freq = br.get(3) as i32;
        ss.prev_aspx_stop_freq = ss.aspx_stop_freq;
        ss.aspx_stop_freq = br.get(2) as i32;
        ss.prev_aspx_master_freq_scale = ss.aspx_master_freq_scale;
        ss.aspx_master_freq_scale = br.get1() as i32;
        ss.aspx_interpolation = br.get1() as i32;
        ss.aspx_preflat = br.get1() as i32;
        ss.aspx_limiter = br.get1() as i32;
        ss.aspx_noise_sbg = br.get(2) as i32;
        ss.aspx_num_env_bits_fixfix = br.get1() as i32;
        ss.aspx_freq_res_mode = br.get(2) as i32;
    }

    /// ac4dec.c: asf_transform_info
    fn asf_transform_info(&mut self, br: &mut BitReader<'_>, ch: usize) {
        let flb = self.frame_len_base;
        let tab = transf_length_48khz(self.frame_len_base_idx.clamp(0, 7) as usize);
        let scp = &mut self.substream.ssch[ch].scp;
        if flb >= 1536 {
            scp.long_frame = br.get1() as u8;
            if scp.long_frame == 0 {
                scp.transf_length_idx[0] = br.get(2) as u8;
                scp.transf_length_idx[1] = br.get(2) as u8;
                scp.transf_length[0] = i32::from(tab[usize::from(scp.transf_length_idx[0])]);
                scp.transf_length[1] = i32::from(tab[usize::from(scp.transf_length_idx[1])]);
            }
            else {
                scp.transf_length[0] = flb;
                scp.transf_length[1] = 0;
            }
        }
        else {
            scp.long_frame = 0;
            scp.transf_length_idx[0] = br.get(2) as u8;
            scp.transf_length[0] = i32::from(tab[usize::from(scp.transf_length_idx[0])]);
        }
    }

    /// ac4dec.c: get_grp_bits
    fn get_grp_bits(&self, ch: usize) -> u32 {
        let scp = &self.substream.ssch[ch].scp;
        let flb = self.frame_len_base;
        let (i0, i1) =
            (usize::from(scp.transf_length_idx[0]), usize::from(scp.transf_length_idx[1]));
        // Only 0..3 index the tables (4 marks an LFE channel).
        if i0 > 3 || i1 > 3 {
            return 0;
        }
        if flb >= 1536 && scp.long_frame == 1 {
            return 0;
        }
        if flb >= 1536 && scp.long_frame == 0 {
            return u32::from(N_GRP_BITS_A[i0][i1]);
        }
        if flb < 1536 && flb > 512 {
            return u32::from(N_GRP_BITS_B[i0]);
        }
        if flb <= 512 {
            return u32::from(N_GRP_BITS_C[i0]);
        }
        0
    }

    /// ac4dec.c: asf_psy_elements
    pub(crate) fn asf_psy_elements(
        &mut self,
        _br: &mut BitReader<'_>,
        ch: usize,
        n_grp_bits: u32,
    ) -> Result<()> {
        let flb = self.frame_len_base;
        let ssch = &mut self.substream.ssch[ch];

        ssch.scp.num_windows = 1;
        ssch.scp.num_window_groups = 1;
        ssch.scp.window_to_group[0] = 0;

        if ssch.scp.long_frame == 0 {
            ssch.scp.num_windows = n_grp_bits as u8 + 1;
            if ssch.scp.different_framing != 0 {
                let num_windows_0 = 1usize << (3 - u32::from(ssch.scp.transf_length_idx[0] & 3));
                let mut i = n_grp_bits as usize;
                while i >= num_windows_0 && i > 0 {
                    ssch.scp.scale_factor_grouping[i] = ssch.scp.scale_factor_grouping[i - 1];
                    i -= 1;
                }
                ssch.scp.scale_factor_grouping[num_windows_0 - 1] = 0;
                ssch.scp.num_windows += 1;
            }
            if ssch.scp.num_windows > 16 {
                return decode_error("ac4: too many windows");
            }
            for i in 0..usize::from(ssch.scp.num_windows) - 1 {
                if ssch.scp.scale_factor_grouping[i] == 0 {
                    ssch.scp.num_window_groups += 1;
                }
                ssch.scp.window_to_group[i + 1] = ssch.scp.num_window_groups - 1;
            }
        }

        let mut group_offset = 0i32;
        let mut win_offset = 0i32;
        let mut win = 0usize;
        ssch.offset2sfb.fill(0);
        ssch.offset2g.fill(0);
        for g in 0..usize::from(ssch.scp.num_window_groups) {
            let transf_length_g = transf_length_of(flb, ssch, g as i32, None);
            let sfb_offset = get_sfb_offset(transf_length_g);
            let sfb_max_size = sfb_offset.len() as i32;

            ssch.scp.num_win_in_group[g] = 0;
            for w in 0..usize::from(ssch.scp.num_windows) {
                if usize::from(ssch.scp.window_to_group[w]) == g {
                    ssch.scp.num_win_in_group[g] += 1;
                }
            }
            let nwg = i32::from(ssch.scp.num_win_in_group[g]);

            let max_sfb = max_sfb_of(flb, ssch, g as i32);
            if max_sfb > sfb_max_size - 1 || max_sfb > 120 {
                return decode_error("ac4: max_sfb exceeds the sfb table");
            }
            let max_sfb = max_sfb as usize;
            for sfb in 0..max_sfb {
                ssch.sect_sfb_offset[g][sfb] = group_offset + i32::from(sfb_offset[sfb]) * nwg;
            }
            group_offset += i32::from(sfb_offset[max_sfb]) * nwg;
            if group_offset > 2048 {
                return decode_error("ac4: spectrum too long");
            }
            ssch.sect_sfb_offset[g][max_sfb] = group_offset;
            for sfb in 0..max_sfb {
                let (a, b) = (ssch.sect_sfb_offset[g][sfb], ssch.sect_sfb_offset[g][sfb + 1]);
                for j in a..b {
                    ssch.offset2sfb[j as usize] = sfb as i16;
                    ssch.offset2g[j as usize] = g as u8;
                }
            }

            for w in 0..usize::from(ssch.scp.num_win_in_group[g]) {
                if win + w >= 16 {
                    return decode_error("ac4: too many windows");
                }
                ssch.win_offset[win + w] = win_offset;
                win_offset += transf_length_g;
            }
            win += usize::from(ssch.scp.num_win_in_group[g]);
        }

        Ok(())
    }

    /// ac4dec.c: asf_psy_info
    fn asf_psy_info(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        dual_maxsfb: i32,
        side_limited: i32,
    ) -> Result<()> {
        let flb = self.frame_len_base;
        let n_grp_bits = self.get_grp_bits(ch);
        {
            let scp = &mut self.substream.ssch[ch].scp;
            let mut n_side_bits = get_side_bits(scp.transf_length[0]);
            let mut n_msfb_bits = get_msfb_bits(scp.transf_length[0]);

            scp.different_framing = 0;
            if flb >= 1536
                && scp.long_frame == 0
                && scp.transf_length_idx[0] != scp.transf_length_idx[1]
            {
                scp.different_framing = 1;
            }

            if side_limited != 0 {
                scp.max_sfb_side[0] = br.get(n_side_bits) as u8;
            }
            else {
                scp.max_sfb[0] = br.get(n_msfb_bits) as u8;
                if dual_maxsfb != 0 {
                    scp.max_sfb_side[0] = br.get(n_msfb_bits) as u8;
                }
            }

            if scp.different_framing != 0 {
                n_side_bits = get_side_bits(scp.transf_length[1]);
                n_msfb_bits = get_msfb_bits(scp.transf_length[1]);

                if side_limited != 0 {
                    scp.max_sfb_side[1] = br.get(n_side_bits) as u8;
                }
                else {
                    scp.max_sfb[1] = br.get(n_msfb_bits) as u8;
                    if dual_maxsfb != 0 {
                        scp.max_sfb_side[1] = br.get(n_msfb_bits) as u8;
                    }
                }
            }

            scp.scale_factor_grouping = [0; 16];
            for i in 0..n_grp_bits.min(15) as usize {
                scp.scale_factor_grouping[i] = br.get1() as u8;
            }
        }
        self.asf_psy_elements(br, ch, n_grp_bits)
    }

    /// ac4dec.c: sf_info
    pub(crate) fn sf_info(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        spec_frontend: i32,
        dual_maxsfb: i32,
        side_limited: i32,
    ) -> Result<()> {
        {
            let scp = &mut self.substream.ssch[ch].scp;
            scp.dual_maxsfb = dual_maxsfb as u8;
            scp.side_limited = side_limited as u8;
        }
        if spec_frontend == SF_ASF {
            self.asf_transform_info(br, ch);
            return self.asf_psy_info(br, ch, dual_maxsfb, side_limited);
        }
        Ok(())
    }

    /// ac4dec.c: sap_data
    fn sap_data(&mut self, br: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let flb = self.frame_len_base;
        let ssch = &mut self.substream.ssch[ch];
        let groups = usize::from(ssch.scp.num_window_groups);

        if br.get1() == 0 {
            for g in 0..groups {
                let max_sfb_g = max_sfb_of(flb, ssch, g as i32) as usize;
                let mut sfb = 0;
                while sfb < max_sfb_g {
                    ssch.sap_coeff_used[g][sfb] = br.get1() as u8;
                    if sfb + 1 < max_sfb_g {
                        ssch.sap_coeff_used[g][sfb + 1] = ssch.sap_coeff_used[g][sfb];
                    }
                    sfb += 2;
                }
            }
        }
        else {
            for g in 0..groups {
                let max_sfb_g = max_sfb_of(flb, ssch, g as i32) as usize;
                for sfb in 0..max_sfb_g {
                    ssch.sap_coeff_used[g][sfb] = 1;
                }
            }
        }

        ssch.delta_code_time = 0;
        if groups != 1 {
            ssch.delta_code_time = br.get1() as i32;
        }

        let cb = codebooks();
        for g in 0..groups {
            let max_sfb_g = max_sfb_of(flb, ssch, g as i32) as usize;
            let mut sfb = 0;
            while sfb < max_sfb_g {
                if ssch.sap_coeff_used[g][sfb] != 0 {
                    ssch.dpcm_alpha_q[g][sfb] = cb.scale_factors.decode(br)?;
                }
                sfb += 2;
            }
        }
        Ok(())
    }

    /// ac4dec.c: ssf_st_data
    fn ssf_st_data(&mut self, br: &mut BitReader<'_>, ch: usize, iframe: i32) {
        let ssch = &mut self.substream.ssch[ch];

        ssch.env_idx[0] = br.get(5) as i32;
        if iframe == 1 && ssch.stride_flag == SHORT_STRIDE {
            br.get(5);
        }
        if ssch.stride_flag == SHORT_STRIDE {
            for block in 0..4 {
                ssch.gain_bits[block] = br.get(4) as i32;
            }
        }
        let num_blocks = if ssch.stride_flag == SHORT_STRIDE { 4 } else { 1 };
        for block in 0..num_blocks {
            if block as i32 >= ssch.start_block
                && (block as i32) < ssch.end_block
                && ssch.predictor_presence[block] != 0
            {
                if ssch.delta[block] != 0 {
                    ssch.predictor_lag_delta[block] = br.get(4) as i32;
                }
                else {
                    ssch.predictor_lag[block] = br.get(9) as i32;
                }
            }
            ssch.variance_preserving[block] = br.get1() as i32;
            ssch.alloc_offset[block] = br.get(5) as i32;
        }
    }

    /// ac4dec.c: ac_init (the arithmetic decoder only consumes its 30 start bits; the
    /// reference decoder does not synthesise the speech spectral frontend either).
    fn ssf_ac_data(&mut self, br: &mut BitReader<'_>, ch: usize) {
        let acs = &mut self.substream.ssch[ch].acs;
        acs.ui_model_bits = SSF_MODEL_BITS;
        acs.ui_model_unit = SSF_MODEL_UNIT;
        acs.ui_range_bits = SSF_RANGE_BITS;
        acs.ui_threshold_large = SSF_THRESHOLD_LARGE;
        acs.ui_threshold_small = SSF_THRESHOLD_SMALL;
        acs.ui_low = 0;
        acs.ui_range = SSF_THRESHOLD_LARGE;
        let mut offset = br.get1();
        for _ in 1..acs.ui_range_bits {
            offset = (offset << 1) + br.get1();
        }
        acs.ui_offset = offset;
        acs.ui_offset2 = offset;
    }

    /// ac4dec.c: ssf_granule
    fn ssf_granule(&mut self, br: &mut BitReader<'_>, ch: usize, iframe: i32) {
        {
            let ssch = &mut self.substream.ssch[ch];
            ssch.stride_flag = br.get1() as i32;
            if iframe != 0 {
                ssch.num_bands = br.get(3) as i32 + 12;
            }
            ssch.start_block = 0;
            ssch.end_block = 0;
            if ssch.stride_flag == LONG_STRIDE && iframe == 0 {
                ssch.end_block = 1;
            }
            if ssch.stride_flag == SHORT_STRIDE {
                ssch.end_block = 4;
                if iframe != 0 {
                    ssch.start_block = 1;
                }
            }
            for block in ssch.start_block..ssch.end_block {
                let b = block as usize;
                ssch.predictor_presence[b] = br.get1() as i32;
                if ssch.predictor_presence[b] != 0 {
                    if ssch.start_block == 1 && block == 1 {
                        ssch.delta[b] = 0;
                    }
                    else {
                        ssch.delta[b] = br.get1() as i32;
                    }
                }
            }
        }
        self.ssf_st_data(br, ch, iframe);
        self.ssf_ac_data(br, ch);
    }

    /// ac4dec.c: ssf_data
    fn ssf_data(&mut self, br: &mut BitReader<'_>, ch: usize, iframe: i32) {
        let ssf_iframe = if iframe != 0 { 1 } else { br.get1() as i32 };
        self.ssf_granule(br, ch, ssf_iframe);
        if self.frame_len_base >= 1536 {
            self.ssf_granule(br, ch, 0);
        }
    }

    /// ac4dec.c: asf_section_data
    fn asf_section_data(&mut self, br: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let flb = self.frame_len_base;
        let ssch = &mut self.substream.ssch[ch];

        ssch.sect_cb = [[0; 128]; 16];
        ssch.sfb_cb = [[0; 128]; 16];

        for g in 0..usize::from(ssch.scp.num_window_groups) {
            let mut gidx = 0;
            let transf_length_g = transf_length_of(flb, ssch, g as i32, Some(&mut gidx));
            let (sect_esc_val, n_sect_bits) = if gidx <= 2 { (7, 3) } else { (31, 5) };
            let nsfb48 = num_sfb_48(transf_length_g);

            let mut k = 0i32;
            let mut i = 0usize;
            ssch.num_sec_lsf[g] = 0;
            let max_sfb = max_sfb_of(flb, ssch, g as i32);
            while k < max_sfb {
                if i + 1 >= 128 {
                    return decode_error("ac4: too many sections");
                }
                ssch.sect_cb[g][i] = br.get(4) as u8;
                if ssch.sect_cb[g][i] > 11 {
                    return decode_error("ac4: sect_cb > 11");
                }
                let mut sect_len = 1;
                let mut sect_len_incr = br.get(n_sect_bits) as i32;
                while sect_len_incr == sect_esc_val {
                    sect_len += sect_esc_val;
                    sect_len_incr = br.get(n_sect_bits) as i32;
                }
                sect_len += sect_len_incr;
                if k + sect_len > max_sfb {
                    return decode_error("ac4: section exceeds max_sfb");
                }
                ssch.sect_start[g][i] = k;
                ssch.sect_end[g][i] = k + sect_len;

                if ssch.sect_start[g][i] < nsfb48 && ssch.sect_end[g][i] >= nsfb48 {
                    ssch.num_sec_lsf[g] = i as i32 + 1;
                    if ssch.sect_end[g][i] > nsfb48 {
                        ssch.sect_end[g][i] = nsfb48;
                        i += 1;
                        ssch.sect_start[g][i] = nsfb48;
                        ssch.sect_end[g][i] = k + sect_len;
                        ssch.sect_cb[g][i] = ssch.sect_cb[g][i - 1];
                    }
                }

                for sfb in k..k + sect_len {
                    ssch.sfb_cb[g][sfb as usize] = ssch.sect_cb[g][i];
                }
                k += sect_len;
                i += 1;
            }

            ssch.num_sec[g] = i as i32;
            if ssch.num_sec_lsf[g] == 0 {
                ssch.num_sec_lsf[g] = ssch.num_sec[g];
            }
        }
        Ok(())
    }

    /// ac4dec.c: asf_spectral_data
    fn asf_spectral_data(&mut self, br: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let cbs = codebooks();
        let ssch = &mut self.substream.ssch[ch];

        ssch.max_quant_idx = [[0; 128]; 16];
        ssch.quant_spec.fill(0);

        for g in 0..usize::from(ssch.scp.num_window_groups) {
            for i in 0..ssch.num_sec_lsf[g] as usize {
                if ssch.sect_cb[g][i] == 0 || ssch.sect_cb[g][i] > 11 {
                    continue;
                }
                let start = ssch.sect_sfb_offset[g][ssch.sect_start[g][i] as usize];
                let end = ssch.sect_sfb_offset[g][ssch.sect_end[g][i] as usize];
                let cb = usize::from(ssch.sect_cb[g][i]) - 1;
                let cb_off = i32::from(ASF_CODEBOOK_OFF[cb]);
                let cb_mod = i32::from(ASF_CODEBOOK_MOD[cb]);
                let dim = usize::from(ASF_CODEBOOK_DIM[cb]);
                let unsigned = ASF_CODEBOOK_UNSIGNED[cb] != 0;

                let mut k = start.max(0) as usize;
                while (k as i32) < end {
                    if k + dim > 2048 {
                        return decode_error("ac4: spectral data overflow");
                    }
                    let mut cb_idx = cbs.asf[cb].decode(br)?;
                    if dim == 4 {
                        let mut v = [0i32; 4];
                        v[0] = cb_idx / 27 - cb_off;
                        cb_idx -= (v[0] + cb_off) * 27;
                        v[1] = cb_idx / 9 - cb_off;
                        cb_idx -= (v[1] + cb_off) * 9;
                        v[2] = cb_idx / cb_mod - cb_off;
                        cb_idx -= (v[2] + cb_off) * cb_mod;
                        v[3] = cb_idx - cb_off;
                        if unsigned {
                            for x in v.iter_mut() {
                                if *x != 0 && br.get1() != 0 {
                                    *x = -*x;
                                }
                            }
                        }
                        for (n, x) in v.iter().enumerate() {
                            ssch.quant_spec[k + n] = *x as i16;
                            let sfb = ssch.offset2sfb[k + n] as usize;
                            ssch.max_quant_idx[g][sfb] = ssch.max_quant_idx[g][sfb].max(x.abs());
                        }
                        k += 4;
                    }
                    else {
                        let mut v0 = cb_idx / cb_mod - cb_off;
                        cb_idx -= (v0 + cb_off) * cb_mod;
                        let mut v1 = cb_idx - cb_off;
                        let (mut sign0, mut sign1) = (false, false);
                        if unsigned {
                            if v0 != 0 && br.get1() != 0 {
                                sign0 = true;
                            }
                            if v1 != 0 && br.get1() != 0 {
                                sign1 = true;
                            }
                        }
                        if ssch.sect_cb[g][i] == 11 {
                            if v0 == 16 {
                                v0 = ext_decode(br)?;
                            }
                            if v1 == 16 {
                                v1 = ext_decode(br)?;
                            }
                        }
                        if v0 > 32767 || v1 > 32767 {
                            return decode_error("ac4: spectral value out of range");
                        }
                        if sign0 {
                            v0 = -v0;
                        }
                        if sign1 {
                            v1 = -v1;
                        }
                        for (n, x) in [v0, v1].iter().enumerate() {
                            ssch.quant_spec[k + n] = *x as i16;
                            let sfb = ssch.offset2sfb[k + n] as usize;
                            ssch.max_quant_idx[g][sfb] = ssch.max_quant_idx[g][sfb].max(x.abs());
                        }
                        k += 2;
                    }
                }
            }
        }
        Ok(())
    }

    /// ac4dec.c: asf_scalefac_data
    fn asf_scalefac_data(&mut self, br: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let flb = self.frame_len_base;
        let cbs = codebooks();
        let ssch = &mut self.substream.ssch[ch];
        let mut first_scf_found = false;

        let mut scale_factor = br.get(8) as i32;
        ssch.sf_gain = [[0.0; 128]; 16];

        for g in 0..usize::from(ssch.scp.num_window_groups) {
            let max_sfb = max_sfb_of(flb, ssch, g as i32)
                .min(num_sfb_48(transf_length_of(flb, ssch, g as i32, None)))
                as usize;

            for sfb in 0..max_sfb {
                if ssch.sfb_cb[g][sfb] != 0 && ssch.max_quant_idx[g][sfb] > 0 {
                    if first_scf_found {
                        ssch.dpcm_sf[g][sfb] = cbs.scale_factors.decode(br)?;
                        scale_factor += ssch.dpcm_sf[g][sfb] - 60;
                    }
                    else {
                        first_scf_found = true;
                    }
                    ssch.sf_gain[g][sfb] = 2f32.powf(0.25 * (scale_factor - 100) as f32);
                }
            }
        }
        Ok(())
    }

    /// ac4dec.c: asf_snf_data
    fn asf_snf_data(&mut self, br: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let flb = self.frame_len_base;
        let cbs = codebooks();
        let ssch = &mut self.substream.ssch[ch];

        ssch.snf_data_exists = br.get1() as i32;
        if ssch.snf_data_exists != 0 {
            for g in 0..usize::from(ssch.scp.num_window_groups) {
                let transf_length_g = transf_length_of(flb, ssch, g as i32, None);
                let max_sfb =
                    max_sfb_of(flb, ssch, g as i32).min(num_sfb_48(transf_length_g)) as usize;
                for sfb in 0..max_sfb {
                    if ssch.sfb_cb[g][sfb] == 0 || ssch.max_quant_idx[g][sfb] == 0 {
                        ssch.dpcm_snf[g][sfb] = cbs.snf.decode(br)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// ac4dec.c: sf_data
    pub(crate) fn sf_data(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        iframe: i32,
        spec_frontend: i32,
    ) -> Result<()> {
        if spec_frontend == SF_ASF {
            self.asf_section_data(br, ch)?;
            self.asf_spectral_data(br, ch)?;
            self.asf_scalefac_data(br, ch)?;
            self.asf_snf_data(br, ch)?;
        }
        else {
            self.ssf_data(br, ch, iframe);
        }
        Ok(())
    }

    /// ac4dec.c: chparam_info
    pub(crate) fn chparam_info(&mut self, br: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let flb = self.frame_len_base;
        let sap_mode = br.get(2) as i32;
        self.substream.ssch[ch].sap_mode = sap_mode;

        if sap_mode == 1 {
            let ssch = &mut self.substream.ssch[ch];
            for g in 0..usize::from(ssch.scp.num_window_groups) {
                let max_sfb_g = max_sfb_of(flb, ssch, g as i32) as usize;
                for sfb in 0..max_sfb_g {
                    ssch.ms_used[g][sfb] = br.get1() as u8;
                }
            }
        }
        if sap_mode == 3 {
            self.sap_data(br, ch)?;
        }
        Ok(())
    }

    /// ac4dec.c: stereo_data
    pub(crate) fn stereo_data(&mut self, br: &mut BitReader<'_>, iframe: i32) -> Result<()> {
        self.substream.mdct_stereo_proc[0] = br.get1() as u8;
        if self.substream.mdct_stereo_proc[0] != 0 {
            self.substream.spec_frontend_l = SF_ASF;
            self.substream.spec_frontend_r = SF_ASF;
            self.sf_info(br, 0, SF_ASF, 0, 0)?;
            self.copy_sf_info(0, 1);
            self.chparam_info(br, 0)?;
        }
        else {
            self.substream.spec_frontend_l = br.get1() as i32;
            let l = self.substream.spec_frontend_l;
            self.sf_info(br, 0, l, 0, 0)?;
            self.substream.spec_frontend_r = br.get1() as i32;
            let r = self.substream.spec_frontend_r;
            self.sf_info(br, 1, r, 0, 0)?;
        }
        let (l, r) = (self.substream.spec_frontend_l, self.substream.spec_frontend_r);
        self.sf_data(br, 0, iframe, l)?;
        self.sf_data(br, 1, iframe, r)
    }

    /// ac4dec.c: companding_control
    pub(crate) fn companding_control(
        &mut self,
        br: &mut BitReader<'_>,
        num_chan: usize,
    ) -> Result<()> {
        let ss = &mut self.substream;
        let mut sync_flag = false;
        let mut need_avg = false;

        if num_chan > 1 {
            sync_flag = br.get1() != 0;
        }
        let nc = if sync_flag { 1 } else { num_chan.min(5) };
        for i in 0..nc {
            ss.compand_on[i] = br.get1() as u8;
            if ss.compand_on[i] == 0 {
                need_avg = true;
            }
        }
        if need_avg {
            ss.compand_avg = br.get1() as i32;
        }
        Ok(())
    }
}
