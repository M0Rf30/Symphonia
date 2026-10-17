// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::{Result, decode_error};

use crate::bit::BitReader;
use crate::ctx::*;

/// ac4dec.c: get_msfbl_bits
fn msfbl_bits(transf_length: i32) -> u32 {
    if (1536..=2048).contains(&transf_length) { 3 } else { 2 }
}

impl Ac4Core {
    /// Copies the shared sf_info() state of channel `src` into channel `dst` (the C memcpy
    /// sequence of `scp`, `sect_sfb_offset`, `offset2sfb`, `offset2g` and `win_offset`).
    pub(crate) fn copy_sf_info(&mut self, src: usize, dst: usize) {
        if src == dst {
            return;
        }
        let ssch = &mut self.substream.ssch;
        let (scp, sect_sfb_offset, offset2sfb, offset2g, win_offset) = {
            let s = &ssch[src];
            (s.scp.clone(), s.sect_sfb_offset, s.offset2sfb, s.offset2g, s.win_offset)
        };
        let d = &mut ssch[dst];
        d.scp = scp;
        d.sect_sfb_offset = sect_sfb_offset;
        d.offset2sfb = offset2sfb;
        d.offset2g = offset2g;
        d.win_offset = win_offset;
    }

    /// ac4dec.c: channel_pair_element
    pub(crate) fn channel_pair_element(
        &mut self,
        br: &mut BitReader<'_>,
        iframe: i32,
    ) -> Result<()> {
        self.substream.codec_mode = br.get(2) as i32;
        if iframe != 0 {
            if self.substream.codec_mode != CM_SIMPLE {
                self.aspx_config(br);
            }
            if self.substream.codec_mode == CM_ASPX_ACPL_1 {
                self.acpl_config_1ch(br, ACPL_PARTIAL)?;
            }
            if self.substream.codec_mode == CM_ASPX_ACPL_2 {
                self.acpl_config_1ch(br, ACPL_FULL)?;
            }
        }

        match self.substream.codec_mode {
            CM_SIMPLE => {
                self.stereo_data(br, iframe)?;
            }
            CM_ASPX => {
                self.companding_control(br, 2)?;
                self.stereo_data(br, iframe)?;
                self.aspx_data_2ch(br, 0, 1, iframe)?;
            }
            CM_ASPX_ACPL_1 => {
                self.companding_control(br, 1)?;
                self.substream.mdct_stereo_proc[0] = br.get1() as u8;
                if self.substream.mdct_stereo_proc[0] != 0 {
                    self.substream.spec_frontend_m = SF_ASF;
                    self.substream.spec_frontend_s = SF_ASF;
                    self.sf_info(br, 0, SF_ASF, 1, 0)?;
                    self.copy_sf_info(0, 1);
                    self.chparam_info(br, 0)?;
                }
                else {
                    self.substream.spec_frontend_m = br.get1() as i32;
                    let m = self.substream.spec_frontend_m;
                    self.sf_info(br, 0, m, 0, 0)?;
                    self.substream.spec_frontend_s = br.get1() as i32;
                    let s = self.substream.spec_frontend_s;
                    self.sf_info(br, 1, s, 0, 1)?;
                }
                let m = self.substream.spec_frontend_m;
                self.sf_data(br, 0, iframe, m)?;
                self.sf_data(br, 1, iframe, m)?;
                self.aspx_data_1ch(br, 0, iframe)?;
                self.acpl_data_1ch(br, 0)?;
            }
            CM_ASPX_ACPL_2 => {
                self.companding_control(br, 1)?;
                let spec_frontend = br.get1() as i32;
                self.sf_info(br, 0, spec_frontend, 0, 0)?;
                self.sf_data(br, 0, iframe, spec_frontend)?;
                self.aspx_data_1ch(br, 0, iframe)?;
                self.acpl_data_1ch(br, 0)?;
            }
            _ => {}
        }

        Ok(())
    }

    /// ac4dec.c: four_channel_data
    pub(crate) fn four_channel_data(&mut self, br: &mut BitReader<'_>, iframe: i32) -> Result<()> {
        self.sf_info(br, 0, SF_ASF, 0, 0)?;

        // Full channel copies (C: memcpy of the whole SubstreamChannel).
        for i in 1..4 {
            let (head, tail) = self.substream.ssch.split_at_mut(i);
            tail[0].clone_from(&head[0]);
        }

        for i in 0..4 {
            self.chparam_info(br, i)?;
        }

        for i in 0..4 {
            self.sf_data(br, i, iframe, SF_ASF)?;
        }

        Ok(())
    }

    /// ac4dec.c: five_channel_info
    pub(crate) fn five_channel_info(&mut self, br: &mut BitReader<'_>) -> Result<()> {
        self.substream.chel_matsel = br.get(4) as u8;

        for i in 0..5 {
            self.chparam_info(br, i)?;
        }

        Ok(())
    }

    /// ac4dec.c: five_channel_data
    pub(crate) fn five_channel_data(&mut self, br: &mut BitReader<'_>, iframe: i32) -> Result<()> {
        self.sf_info(br, 0, SF_ASF, 0, 0)?;

        for i in 1..5 {
            self.copy_sf_info(0, i);
        }

        self.five_channel_info(br)?;

        for i in 0..5 {
            self.sf_data(br, i, iframe, SF_ASF)?;
        }

        Ok(())
    }

    /// ac4dec.c: sf_info_lfe
    pub(crate) fn sf_info_lfe(&mut self, br: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let n_msfbl_bits = msfbl_bits(self.frame_len_base);

        // The LFE is one long window: no window grouping.
        let scp = &mut self.substream.ssch[ch].scp;
        scp.long_frame = 1;
        scp.max_sfb[0] = br.get(n_msfbl_bits) as u8;
        scp.num_window_groups = 1;
        scp.transf_length_idx[0] = 4;

        self.asf_psy_elements(br, ch, 0)
    }

    /// ac4dec.c: mono_data
    pub(crate) fn mono_data(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        lfe: i32,
        iframe: i32,
    ) -> Result<()> {
        let spec_frontend;
        if lfe != 0 {
            spec_frontend = SF_ASF;
            self.sf_info_lfe(br, ch)?;
        }
        else {
            spec_frontend = br.get1() as i32;
            self.sf_info(br, ch, spec_frontend, 0, 0)?;
        }
        self.sf_data(br, ch, iframe, spec_frontend)
    }

    /// ac4dec.c: channel_element_7x
    pub(crate) fn channel_element_7x(
        &mut self,
        br: &mut BitReader<'_>,
        channel_mode: i32,
        iframe: i32,
    ) -> Result<()> {
        self.substream.codec_mode = br.get(2) as i32;
        if iframe != 0 {
            if self.substream.codec_mode != CM_SIMPLE {
                self.aspx_config(br);
            }
            if self.substream.codec_mode == CM_ASPX_ACPL_1 {
                self.acpl_config_1ch(br, ACPL_PARTIAL)?;
            }
            if self.substream.codec_mode == CM_ASPX_ACPL_2 {
                self.acpl_config_1ch(br, ACPL_FULL)?;
            }
        }

        if channel_mode == 6 {
            self.mono_data(br, 7, 1, iframe)?;
        }

        if self.substream.codec_mode == CM_ASPX_ACPL_1
            || self.substream.codec_mode == CM_ASPX_ACPL_2
        {
            self.companding_control(br, 5)?;
        }

        self.substream.coding_config = br.get(2) as u8;
        match self.substream.coding_config {
            2 => self.four_channel_data(br, iframe),
            3 => self.five_channel_data(br, iframe),
            _ => Ok(()),
        }
    }

    /// ac4dec.c: three_channel_info
    pub(crate) fn three_channel_info(
        &mut self,
        br: &mut BitReader<'_>,
        ch0: usize,
        ch1: usize,
        _ch2: usize,
    ) -> Result<()> {
        self.substream.chel_matsel = br.get(4) as u8;
        self.chparam_info(br, ch0)?;
        self.chparam_info(br, ch1)
    }

    /// ac4dec.c: three_channel_data
    pub(crate) fn three_channel_data(
        &mut self,
        br: &mut BitReader<'_>,
        ch0: usize,
        ch1: usize,
        ch2: usize,
    ) -> Result<()> {
        self.sf_info(br, ch0, SF_ASF, 0, 0)?;

        self.copy_sf_info(ch0, ch1);
        self.copy_sf_info(ch0, ch2);

        self.three_channel_info(br, ch0, ch1, ch2)?;
        self.sf_data(br, ch0, 0, SF_ASF)?;
        self.sf_data(br, ch1, 0, SF_ASF)?;
        self.sf_data(br, ch2, 0, SF_ASF)?;

        Ok(())
    }

    /// ac4dec.c: two_channel_data
    pub(crate) fn two_channel_data(
        &mut self,
        br: &mut BitReader<'_>,
        ch0: usize,
        ch1: usize,
        x: usize,
    ) -> Result<()> {
        if br.bits_left() <= 0 {
            return decode_error("ac4: two_channel_data underflow");
        }

        let proc = br.get1() as u8;
        match self.substream.mdct_stereo_proc.get_mut(x) {
            Some(p) => *p = proc,
            None => return decode_error("ac4: invalid mdct_stereo_proc index"),
        }
        if proc != 0 {
            self.sf_info(br, ch0, SF_ASF, 0, 0)?;
            self.copy_sf_info(ch0, ch1);
            self.chparam_info(br, ch0)?;
        }
        else {
            self.sf_info(br, ch0, SF_ASF, 0, 0)?;
            self.sf_info(br, ch1, SF_ASF, 0, 0)?;
        }
        self.sf_data(br, ch0, 0, SF_ASF)?;
        self.sf_data(br, ch1, 0, SF_ASF)?;

        Ok(())
    }

    /// ac4dec.c: channel_element_3x
    pub(crate) fn channel_element_3x(&mut self, br: &mut BitReader<'_>, iframe: i32) -> Result<()> {
        self.substream.codec_mode = br.get1() as i32;
        if self.substream.codec_mode == CM_ASPX {
            if iframe != 0 {
                self.aspx_config(br);
            }
            self.companding_control(br, 3)?;
        }

        self.substream.coding_config = br.get1() as u8;
        match self.substream.coding_config {
            0 => {
                self.stereo_data(br, iframe)?;
                self.mono_data(br, 2, 0, iframe)?;
            }
            _ => {
                self.three_channel_data(br, 0, 1, 2)?;
            }
        }

        if self.substream.codec_mode == CM_ASPX {
            self.aspx_data_2ch(br, 0, 1, iframe)?;
            self.aspx_data_1ch(br, 2, iframe)?;
        }

        Ok(())
    }

    /// ac4dec.c: channel_element_5x
    pub(crate) fn channel_element_5x(
        &mut self,
        br: &mut BitReader<'_>,
        lfe: i32,
        iframe: i32,
    ) -> Result<()> {
        self.substream.codec_mode = br.get(3) as i32;
        if iframe != 0 {
            if self.substream.codec_mode != CM_SIMPLE {
                self.aspx_config(br);
            }
            if self.substream.codec_mode == CM_ASPX_ACPL_1 {
                self.acpl_config_1ch(br, ACPL_PARTIAL)?;
            }
            if self.substream.codec_mode == CM_ASPX_ACPL_2 {
                self.acpl_config_1ch(br, ACPL_FULL)?;
            }
            if self.substream.codec_mode == CM_ASPX_ACPL_3 {
                self.acpl_config_2ch(br)?;
            }
        }

        if lfe != 0 {
            self.mono_data(br, 5, 1, iframe)?;
        }

        match self.substream.codec_mode {
            CM_SIMPLE | CM_ASPX => {
                if self.substream.codec_mode == CM_ASPX {
                    self.companding_control(br, 5)?;
                }

                self.substream.coding_config = br.get(2) as u8;
                match self.substream.coding_config {
                    0 => {
                        self.substream.mode_2ch = br.get1() as u8;
                        self.two_channel_data(br, 0, 1, 0)?;
                        self.two_channel_data(br, 2, 3, 1)?;
                        self.mono_data(br, 4, 0, iframe)?;
                    }
                    1 => {
                        self.three_channel_data(br, 0, 1, 2)?;
                        self.two_channel_data(br, 3, 4, 0)?;
                    }
                    2 => {
                        self.four_channel_data(br, iframe)?;
                        self.mono_data(br, 4, 0, iframe)?;
                    }
                    _ => {
                        self.five_channel_data(br, iframe)?;
                    }
                }

                if self.substream.codec_mode == CM_ASPX {
                    self.aspx_data_2ch(br, 0, 1, iframe)?;
                    self.aspx_data_2ch(br, 2, 3, iframe)?;
                    self.aspx_data_1ch(br, 4, iframe)?;
                }
            }
            CM_ASPX_ACPL_1 | CM_ASPX_ACPL_2 => {
                self.companding_control(br, 3)?;
                self.substream.coding_config = br.get1() as u8;
                if self.substream.coding_config != 0 {
                    self.three_channel_data(br, 0, 1, 2)?;
                }
                else {
                    self.two_channel_data(br, 0, 1, 0)?;
                }

                if self.substream.codec_mode == CM_ASPX_ACPL_1 {
                    self.substream.max_sfb_master = br.get(5) as i32; // XXX
                    self.chparam_info(br, 3)?;
                    self.chparam_info(br, 4)?;
                    self.sf_data(br, 3, iframe, SF_ASF)?;
                    self.sf_data(br, 4, iframe, SF_ASF)?;
                }
                if self.substream.coding_config == 0 {
                    self.mono_data(br, 2, 0, iframe)?;
                }

                self.aspx_data_2ch(br, 0, 1, iframe)?;
                self.aspx_data_1ch(br, 2, iframe)?;
                self.acpl_data_1ch(br, 0)?;
                self.acpl_data_1ch(br, 1)?;
            }
            CM_ASPX_ACPL_3 => {
                self.companding_control(br, 2)?;
                self.stereo_data(br, iframe)?;
                self.aspx_data_2ch(br, 0, 1, iframe)?;
                self.acpl_data_2ch(br, 0, 1)?;
            }
            _ => return decode_error("ac4: invalid codec mode"),
        }

        Ok(())
    }

    /// ac4dec.c: single_channel_element
    pub(crate) fn single_channel_element(
        &mut self,
        br: &mut BitReader<'_>,
        iframe: i32,
    ) -> Result<()> {
        self.substream.codec_mode = br.get1() as i32;
        if iframe != 0 && self.substream.codec_mode == CM_ASPX {
            self.aspx_config(br);
        }
        if self.substream.codec_mode == CM_SIMPLE {
            self.mono_data(br, 0, 0, iframe)
        }
        else {
            self.companding_control(br, 1)?;
            self.mono_data(br, 0, 0, iframe)?;
            self.aspx_data_1ch(br, 0, iframe)
        }
    }

    /// ac4dec.c: audio_data
    pub(crate) fn audio_data(
        &mut self,
        br: &mut BitReader<'_>,
        channel_mode: i32,
        iframe: i32,
    ) -> Result<()> {
        match channel_mode {
            0 => self.single_channel_element(br, iframe),
            1 => self.channel_pair_element(br, iframe),
            2 => self.channel_element_3x(br, iframe),
            3 => self.channel_element_5x(br, 0, iframe),
            4 => self.channel_element_5x(br, 1, iframe),
            5 | 6 => self.channel_element_7x(br, channel_mode, iframe),
            _ => decode_error("ac4: invalid channel mode"),
        }
    }
}
