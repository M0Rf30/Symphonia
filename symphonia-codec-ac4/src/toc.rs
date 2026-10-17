// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Table of contents, substream and metadata parsing, and the per-frame entry point
//! (`ac4dec.c` `ac4_toc` .. `ac4_substream` and `ac4_decode_frame`, see `NOTICE`).

use symphonia_core::errors::{Result, decode_error, unsupported_error};

use crate::bit::{BitReader, variable_bits};
use crate::codebooks::codebooks;
use crate::ctx::*;
use crate::tables::{FRAME_LEN_BASE_48KHZ, FRAME_LEN_BASE_IDX_48KHZ};

/// Output order of the 5.1 channels (the LFE follows the main channels in the decoder).
const ORDER_5_1: [u8; 6] = [0, 1, 2, 5, 3, 4];
/// Output order of the 7.1 channels (the LFE follows the main channels in the decoder).
const ORDER_7_1: [u8; 8] = [0, 1, 2, 7, 3, 4, 5, 6];

/// Locates a `SubstreamInfo`, which lives either in a presentation or in a substream group.
#[derive(Clone, Copy)]
pub(crate) enum SsiRef {
    Pres(usize),
    Group(usize),
}

/// ac4dec.c: `get_unary(gb, 0, len)`; counts `1` bits up to `len` without failing.
fn get_unary_max(br: &mut BitReader<'_>, len: i32) -> i32 {
    let mut v = 0;
    while v < len && br.get1() != 0 {
        v += 1;
    }
    v
}

/// Reads a 2-bit index that is extended by `variable_bits(2)` when it equals 3.
fn get_index2(br: &mut BitReader<'_>) -> Result<i32> {
    let mut index = br.get(2) as i32;
    if index == 3 {
        index = index.wrapping_add(variable_bits(br, 2)?);
    }
    Ok(index)
}

/// Skips `n` bytes, rejecting negative counts.
fn skip_bytes(br: &mut BitReader<'_>, n: i32) -> Result<()> {
    match usize::try_from(n) {
        Ok(n) => {
            br.skip_long(n.saturating_mul(8));
            Ok(())
        }
        Err(_) => decode_error("ac4: invalid skip size"),
    }
}

/// Skips `n` bits, rejecting negative counts.
fn skip_bits_checked(br: &mut BitReader<'_>, n: i32) -> Result<()> {
    match usize::try_from(n) {
        Ok(n) => {
            br.skip_long(n);
            Ok(())
        }
        Err(_) => decode_error("ac4: invalid skip size"),
    }
}

/// ac4dec.c: `emdf_payloads_substream_info`
fn emdf_payloads_substream_info(br: &mut BitReader<'_>, e: &mut EmdfInfo) -> Result<()> {
    e.substream_index = get_index2(br)?;
    Ok(())
}

/// ac4dec.c: `emdf_protection`
fn emdf_protection(br: &mut BitReader<'_>) {
    let first = br.get(2);
    let second = br.get(2);

    for v in [first, second] {
        match v {
            0 => (),
            1 => br.skip(8),
            2 => br.skip_long(32),
            _ => br.skip_long(128),
        }
    }
}

/// ac4dec.c: `emdf_info`
fn emdf_info(br: &mut BitReader<'_>, e: &mut EmdfInfo) -> Result<()> {
    e.version = br.get(2) as i32;
    if e.version == 3 {
        e.version = e.version.wrapping_add(variable_bits(br, 2)?);
    }
    e.key_id = br.get(3) as i32;
    if e.key_id == 7 {
        e.key_id = e.key_id.wrapping_add(variable_bits(br, 3)?);
    }

    if br.get1() != 0 {
        emdf_payloads_substream_info(br, e)?;
    }

    emdf_protection(br);

    Ok(())
}

/// ac4dec.c: `content_type`
fn content_type(br: &mut BitReader<'_>) {
    br.skip(3);
    if br.get1() != 0 {
        if br.get1() != 0 {
            br.skip(1);
            br.skip(16);
        }
        else {
            let language_tag_bytes = br.get(6);

            br.skip_long(8 * language_tag_bytes as usize);
        }
    }
}

/// ac4dec.c: `presentation_config_ext_info`
fn presentation_config_ext_info(br: &mut BitReader<'_>) -> Result<()> {
    let mut n_skip_bytes = br.get(5) as i32;
    if br.get1() != 0 {
        n_skip_bytes = n_skip_bytes.wrapping_add(variable_bits(br, 2)? << 5);
    }

    skip_bytes(br, n_skip_bytes)
}

/// ac4dec.c: `oamd_substream_info`
fn oamd_substream_info(br: &mut BitReader<'_>, substreams_present: bool) -> Result<()> {
    br.skip(1);
    if substreams_present {
        // The index is parsed but unused.
        get_index2(br)?;
    }

    Ok(())
}

/// ac4dec.c: `get_num_ts_in_ats`
fn get_num_ts_in_ats(frame_length: i32) -> i32 {
    if frame_length <= 2048 && frame_length >= 1536 {
        return 2;
    }

    1
}

/// ac4dec.c: `channel_mode_contains_lfe`
fn channel_mode_contains_lfe(channel_mode: i32) -> bool {
    matches!(channel_mode, 4 | 6 | 8 | 10)
}

impl Ac4Core {
    /// Returns the `SubstreamInfo` designated by `r`.
    pub(crate) fn ssi(&self, r: SsiRef) -> &SubstreamInfo {
        match r {
            SsiRef::Pres(p) => &self.pinfo[p].ssinfo,
            SsiRef::Group(g) => &self.ssgroup[g].ssinfo,
        }
    }

    /// Returns the `SubstreamInfo` designated by `r` for modification.
    pub(crate) fn ssi_mut(&mut self, r: SsiRef) -> &mut SubstreamInfo {
        match r {
            SsiRef::Pres(p) => &mut self.pinfo[p].ssinfo,
            SsiRef::Group(g) => &mut self.ssgroup[g].ssinfo,
        }
    }

    /// Records the kind of the substream `index` (`s->substream_type[index] = ty`).
    fn set_substream_type(&mut self, index: i32, ty: i32) -> Result<()> {
        let slot = usize::try_from(index).ok().and_then(|i| self.substream_type.get_mut(i));
        match slot {
            Some(slot) => {
                *slot = ty;
                Ok(())
            }
            None => decode_error("ac4: invalid substream index"),
        }
    }

    /// ac4dec.c: `check_sequence`
    pub(crate) fn check_sequence(&self) -> Result<()> {
        if self.sequence_counter > 1020 {
            return decode_error("ac4: invalid sequence counter");
        }

        if self.sequence_counter == self.sequence_counter_prev + 1 {
            return Ok(());
        }

        if self.sequence_counter != 0 && self.sequence_counter_prev == 0 {
            return Ok(());
        }

        if self.sequence_counter == 1 && self.sequence_counter_prev == 1020 {
            return Ok(());
        }

        if self.sequence_counter == 0 && self.sequence_counter_prev == 0 {
            return Ok(());
        }

        decode_error("ac4: unexpected sequence counter")
    }

    /// ac4dec.c: `frame_rate_multiply_info`
    pub(crate) fn frame_rate_multiply_info(&mut self, br: &mut BitReader<'_>, p: usize) {
        let frame_rate_index = self.frame_rate_index;
        let pi = &mut self.pinfo[p];

        pi.multiplier_bit = 0;

        match frame_rate_index {
            2..=4 => {
                pi.multiplier = br.get1() as i32;
                if pi.multiplier != 0 {
                    pi.multiplier_bit = br.get1() as i32;
                }
                pi.frame_rate_factor =
                    if pi.multiplier != 0 { if pi.multiplier_bit != 0 { 4 } else { 2 } } else { 1 };
            }
            0 | 1 | 7..=9 => {
                pi.multiplier = br.get1() as i32;
                pi.frame_rate_factor = if pi.multiplier != 0 { 2 } else { 1 };
            }
            _ => pi.frame_rate_factor = 1,
        }
    }

    /// ac4dec.c: `ac4_hsf_ext_substream_info`
    pub(crate) fn ac4_hsf_ext_substream_info(
        &mut self,
        br: &mut BitReader<'_>,
        ssi: SsiRef,
        substream_present: bool,
    ) -> Result<()> {
        if substream_present {
            let index = get_index2(br)?;
            self.ssi_mut(ssi).hsf_ext_substream_index = index;
        }

        Ok(())
    }

    /// ac4dec.c: `ac4_substream_info`
    pub(crate) fn ac4_substream_info(
        &mut self,
        br: &mut BitReader<'_>,
        p: usize,
        ssi: SsiRef,
    ) -> Result<()> {
        let fs_index = self.fs_index;
        let frame_rate_factor = self.pinfo[p].frame_rate_factor;

        let s = self.ssi_mut(ssi);

        s.sus_ver = 0;
        s.channel_mode = codebooks().channel_mode.decode(br)?;

        if s.channel_mode == 16 {
            s.channel_mode = s.channel_mode.wrapping_add(variable_bits(br, 2)?);
        }

        if fs_index == 1 && br.get1() != 0 {
            s.sf_multiplier = 1 + br.get1() as i32;
        }

        if br.get1() != 0 {
            s.bitrate_indicator = codebooks().bitrate_indicator.decode(br)?;
        }

        if s.channel_mode == 7 || s.channel_mode == 8 || s.channel_mode == 9 || s.channel_mode == 10
        {
            s.add_ch_base = br.get1() as i32;
        }

        if br.get1() != 0 {
            content_type(br);
        }

        for i in 0..frame_rate_factor.clamp(0, 4) as usize {
            s.iframe[i] = br.get1() as i32;
        }

        s.substream_index = get_index2(br)?;
        let index = s.substream_index;

        self.set_substream_type(index, ST_SUBSTREAM)
    }

    /// ac4dec.c: `ac4_presentation_info`
    pub(crate) fn ac4_presentation_info(&mut self, br: &mut BitReader<'_>, p: usize) -> Result<()> {
        self.pinfo[p].single_substream = br.get1() as i32;
        if self.pinfo[p].single_substream != 1 {
            let mut config = br.get(3) as i32;
            if config == 0x7 {
                config = config.wrapping_add(variable_bits(br, 2)?);
            }
            self.pinfo[p].presentation_config = config;
        }

        self.pinfo[p].presentation_version = get_unary_max(br, 31);

        self.pinfo[p].add_emdf_substreams = 0;
        if self.pinfo[p].single_substream != 1 && self.pinfo[p].presentation_config == 6 {
            self.pinfo[p].add_emdf_substreams = 1;
        }
        else {
            self.pinfo[p].mdcompat = br.get(3) as i32;

            if br.get1() != 0 {
                self.pinfo[p].presentation_id = variable_bits(br, 2)?;
            }

            self.frame_rate_multiply_info(br, p);
            emdf_info(br, &mut self.pinfo[p].emdf[0])?;

            if self.pinfo[p].single_substream == 1 {
                self.ac4_substream_info(br, p, SsiRef::Pres(p))?;
            }
            else {
                self.pinfo[p].hsf_ext = br.get1() as i32;
                match self.pinfo[p].presentation_config {
                    0 => {
                        self.ac4_substream_info(br, p, SsiRef::Pres(p))?;
                        self.ac4_hsf_ext_substream_info(br, SsiRef::Pres(p), true)?;
                        self.ac4_substream_info(br, p, SsiRef::Pres(p))?;
                    }
                    _ => presentation_config_ext_info(br)?,
                }
            }

            self.pinfo[p].pre_virtualized = br.get1() as i32;
            self.pinfo[p].add_emdf_substreams = br.get1() as i32;
        }

        self.add_emdf_substreams_info(br, p)
    }

    /// Parses the additional EMDF substreams of a presentation (tail of `ac4_presentation_info`).
    fn add_emdf_substreams_info(&mut self, br: &mut BitReader<'_>, p: usize) -> Result<()> {
        let pi = &mut self.pinfo[p];

        if pi.add_emdf_substreams != 0 {
            let mut n = br.get(2) as i32;
            if n == 0 {
                n = variable_bits(br, 2)?.wrapping_add(4);
            }
            if n < 0 || n as usize > pi.emdf.len() {
                return decode_error("ac4: invalid number of additional EMDF substreams");
            }
            pi.n_add_emdf_substreams = n;

            for i in 0..n as usize {
                emdf_info(br, &mut pi.emdf[i])?;
            }
        }

        Ok(())
    }

    /// ac4dec.c: `substream_index_table`
    pub(crate) fn substream_index_table(&mut self, br: &mut BitReader<'_>) -> Result<()> {
        let mut nb_substreams = br.get(2) as i32;
        if nb_substreams == 0 {
            nb_substreams = variable_bits(br, 2)?.wrapping_add(4);
        }
        if nb_substreams < 0 || nb_substreams as usize > self.substream_size.len() {
            return decode_error("ac4: invalid number of substreams");
        }
        self.nb_substreams = nb_substreams;

        let size_present = if nb_substreams == 1 { br.get1() != 0 } else { true };

        if size_present {
            for i in 0..nb_substreams as usize {
                let more_bits = br.get1() != 0;

                let mut size = br.get(10) as i32;
                if more_bits {
                    size = size.wrapping_add(variable_bits(br, 2)? << 10);
                }
                self.substream_size[i] = size;
            }
        }

        Ok(())
    }

    /// ac4dec.c: `presentation_substream_info`
    pub(crate) fn presentation_substream_info(
        &mut self,
        br: &mut BitReader<'_>,
        p: usize,
    ) -> Result<()> {
        let psi = &mut self.pinfo[p].psinfo;

        psi.alternative = br.get1() as i32;
        psi.pres_ndot = br.get1() as i32;
        psi.substream_index = get_index2(br)?;
        let index = psi.substream_index;

        self.set_substream_type(index, ST_PRESENTATION)
    }

    /// ac4dec.c: `frame_rate_fractions_info`
    pub(crate) fn frame_rate_fractions_info(&mut self, br: &mut BitReader<'_>, p: usize) {
        let frame_rate_index = self.frame_rate_index;
        let pi = &mut self.pinfo[p];

        pi.frame_rate_fraction = 1;
        if frame_rate_index >= 5
            && frame_rate_index <= 9
            && pi.frame_rate_factor == 1
            && br.get1() != 0
        {
            pi.frame_rate_fraction = 2;
        }

        if frame_rate_index >= 10 && frame_rate_index <= 12 && br.get1() != 0 {
            if br.get1() != 0 {
                pi.frame_rate_fraction = 4;
            }
            else {
                pi.frame_rate_fraction = 2;
            }
        }
    }

    /// ac4dec.c: `ac4_substream_info_chan`
    pub(crate) fn ac4_substream_info_chan(
        &mut self,
        br: &mut BitReader<'_>,
        g: usize,
        substreams_present: bool,
        sus_ver: i32,
    ) -> Result<()> {
        let fs_index = self.fs_index;
        let frame_rate_factor = self.pinfo[0].frame_rate_factor;
        let ssi = &mut self.ssgroup[g].ssinfo;

        ssi.sus_ver = sus_ver;
        ssi.channel_mode = codebooks().channel_mode.decode(br)?;

        if ssi.channel_mode == 16 {
            ssi.channel_mode = ssi.channel_mode.wrapping_add(variable_bits(br, 2)?);
        }

        if ssi.channel_mode == 11
            || ssi.channel_mode == 12
            || ssi.channel_mode == 13
            || ssi.channel_mode == 14
        {
            ssi.back_channels_present = br.get1() as i32;
            ssi.centre_present = br.get1() as i32;
            ssi.top_channels_present = br.get(2) as i32;
        }

        if fs_index != 0 && br.get1() != 0 {
            ssi.sf_multiplier = 1 + br.get1() as i32;
        }

        if br.get1() != 0 {
            ssi.bitrate_indicator = codebooks().bitrate_indicator.decode(br)?;
        }

        if ssi.channel_mode == 7
            || ssi.channel_mode == 8
            || ssi.channel_mode == 9
            || ssi.channel_mode == 10
        {
            ssi.add_ch_base = br.get1() as i32;
        }

        for i in 0..frame_rate_factor.clamp(0, 4) as usize {
            ssi.iframe[i] = br.get1() as i32;
        }

        if substreams_present {
            ssi.substream_index = get_index2(br)?;
        }

        Ok(())
    }

    /// ac4dec.c: `ac4_substream_group_info`
    pub(crate) fn ac4_substream_group_info(
        &mut self,
        br: &mut BitReader<'_>,
        g: usize,
    ) -> Result<()> {
        let substreams_present = br.get1() != 0;
        let hsf_ext = br.get1() != 0;
        let n_lf_substreams = if br.get1() != 0 {
            1
        }
        else {
            let mut n = br.get(2) as i32 + 2;
            if n == 5 {
                n = n.wrapping_add(variable_bits(br, 2)?);
            }
            n
        };
        self.ssgroup[g].channel_coded = br.get1() as i32;
        if self.ssgroup[g].channel_coded != 0 {
            for _ in 0..n_lf_substreams {
                if br.bits_left() < 0 {
                    return decode_error("ac4: truncated substream group info");
                }

                let sus_ver = if self.version == 1 { br.get1() as i32 } else { 1 };

                self.ac4_substream_info_chan(br, g, substreams_present, sus_ver)?;
                if hsf_ext {
                    self.ac4_hsf_ext_substream_info(br, SsiRef::Group(g), substreams_present)?;
                }
            }
        }
        else {
            if br.get1() != 0 {
                oamd_substream_info(br, substreams_present)?;
            }
            // ac4dec.c: av_assert0(0); the AJOC/object substream infos are not implemented.
            return unsupported_error("ac4: object-coded substream groups");
        }

        if br.get1() != 0 {
            content_type(br);
        }

        Ok(())
    }

    /// ac4dec.c: `ac4_sgi_specifier`
    pub(crate) fn ac4_sgi_specifier(&mut self, br: &mut BitReader<'_>, g: usize) -> Result<()> {
        if self.version == 1 {
            // ac4dec.c: av_assert0(0)
            return unsupported_error("ac4: version 1 substream group specifier");
        }

        let mut group_index = br.get(3) as i32;
        if group_index == 7 {
            group_index = group_index.wrapping_add(variable_bits(br, 2)?);
        }

        // The group indices select entries of `ssgroup`.
        if group_index < 0 || group_index as usize >= self.ssgroup.len() {
            return decode_error("ac4: invalid substream group index");
        }
        match self.ssgroup.get_mut(g) {
            Some(group) => group.group_index = group_index,
            None => return decode_error("ac4: invalid substream group"),
        }

        self.total_groups = self.total_groups.max(group_index);

        Ok(())
    }

    /// ac4dec.c: `ac4_presentation_v1_info`
    pub(crate) fn ac4_presentation_v1_info(
        &mut self,
        br: &mut BitReader<'_>,
        p: usize,
    ) -> Result<()> {
        let single_substream_group = br.get1() as i32;
        if single_substream_group != 1 {
            let mut config = br.get(3) as i32;
            if config == 7 {
                config = config.wrapping_add(variable_bits(br, 2)?);
            }
            self.pinfo[p].presentation_config = config;
        }
        if self.version != 1 {
            self.pinfo[p].presentation_version = get_unary_max(br, 31);
        }

        if single_substream_group != 1 && self.pinfo[p].presentation_config == 6 {
            self.pinfo[p].add_emdf_substreams = 1;
        }
        else {
            if self.version != 1 {
                self.pinfo[p].mdcompat = br.get(3) as i32;
            }

            if br.get1() != 0 {
                self.pinfo[p].presentation_id = variable_bits(br, 2)?;
            }

            self.frame_rate_multiply_info(br, p);
            self.frame_rate_fractions_info(br, p);
            emdf_info(br, &mut self.pinfo[p].emdf[0])?;

            if br.get1() != 0 {
                self.pinfo[p].enable_presentation = br.get1() as i32;
            }

            if single_substream_group == 1 {
                self.ac4_sgi_specifier(br, 0)?;
                self.pinfo[p].n_substream_groups = 1;
            }
            else {
                self.pinfo[p].multi_pid = br.get1() as i32;
                match self.pinfo[p].presentation_config {
                    0 => {
                        // Music and Effects + Dialogue
                        self.ac4_sgi_specifier(br, 0)?;
                        self.ac4_sgi_specifier(br, 1)?;
                        self.pinfo[p].n_substream_groups = 2;
                    }
                    1 => {
                        // Main + DE
                        self.ac4_sgi_specifier(br, 0)?;
                        self.ac4_sgi_specifier(br, 1)?;
                        self.pinfo[p].n_substream_groups = 1;
                    }
                    2 => {
                        // Main + Associated Audio
                        self.ac4_sgi_specifier(br, 0)?;
                        self.ac4_sgi_specifier(br, 1)?;
                        self.pinfo[p].n_substream_groups = 2;
                    }
                    3 => {
                        // Music and Effects + Dialogue + Associated Audio
                        self.ac4_sgi_specifier(br, 0)?;
                        self.ac4_sgi_specifier(br, 1)?;
                        self.ac4_sgi_specifier(br, 2)?;
                        self.pinfo[p].n_substream_groups = 3;
                    }
                    4 => {
                        // Main + DE + Associated Audio
                        self.ac4_sgi_specifier(br, 0)?;
                        self.ac4_sgi_specifier(br, 1)?;
                        self.ac4_sgi_specifier(br, 2)?;
                        self.pinfo[p].n_substream_groups = 2;
                    }
                    5 => {
                        // Arbitrary number of roles and substream groups
                        let mut n = br.get(2) as i32 + 2;
                        if n == 5 {
                            n = n.wrapping_add(variable_bits(br, 2)?);
                        }
                        if n < 0 || n as usize > self.ssgroup.len() {
                            return decode_error("ac4: invalid number of substream groups");
                        }
                        self.pinfo[p].n_substream_groups = n;

                        for sg in 0..n as usize {
                            self.ac4_sgi_specifier(br, sg)?;
                        }
                    }
                    _ => {
                        // EMDF and other data
                        presentation_config_ext_info(br)?;
                    }
                }
            }
            self.pinfo[p].pre_virtualized = br.get1() as i32;
            self.pinfo[p].add_emdf_substreams = br.get1() as i32;
            self.presentation_substream_info(br, p)?;
        }

        self.add_emdf_substreams_info(br, p)
    }

    /// ac4dec.c: `ac4_toc`
    pub(crate) fn ac4_toc(&mut self, br: &mut BitReader<'_>) -> Result<()> {
        self.version = br.get(2) as i32;
        if self.version == 3 {
            self.version = self.version.wrapping_add(variable_bits(br, 2)?);
        }
        self.sequence_counter_prev = self.sequence_counter;
        self.sequence_counter = br.get(10) as i32;

        self.wait_frames = br.get1() as i32;
        if self.wait_frames != 0 {
            self.nb_wait_frames = br.get(3) as i32;
            if self.nb_wait_frames > 0 {
                br.skip(2);
            }
        }

        self.fs_index = br.get1() as i32;
        self.frame_rate_index = br.get(4) as i32;
        let frame_len_base = FRAME_LEN_BASE_48KHZ[self.frame_rate_index as usize & 15];
        if frame_len_base == 0 {
            return decode_error("ac4: reserved frame rate index");
        }
        self.frame_len_base = i32::from(frame_len_base);
        self.num_ts_in_ats = get_num_ts_in_ats(self.frame_len_base);
        self.frame_len_base_idx =
            i32::from(FRAME_LEN_BASE_IDX_48KHZ[self.frame_rate_index as usize & 15]);
        // The resampling ratio of the C decoder only feeds a value that is overwritten.
        self.num_qmf_timeslots = self.frame_len_base / 64;
        self.num_aspx_timeslots = self.num_qmf_timeslots / self.num_ts_in_ats;
        self.ts_offset_hfgen = 3 * self.num_ts_in_ats;
        self.iframe_global = br.get1() as i32;
        if self.iframe_global != 0 {
            self.have_iframe = true;
        }
        else {
            self.check_sequence()?;
        }
        if br.get1() != 0 {
            self.nb_presentations = 1;
        }
        else {
            if br.get1() != 0 {
                self.nb_presentations = variable_bits(br, 2)?.wrapping_add(2);
            }
            else {
                self.nb_presentations = 0;
            }
        }
        if self.nb_presentations < 0 || self.nb_presentations as usize > self.pinfo.len() {
            return decode_error("ac4: invalid number of presentations");
        }

        self.payload_base = 0;
        if br.get1() != 0 {
            self.payload_base = br.get(5) as i32 + 1;
            if self.payload_base == 0x20 {
                self.payload_base = self.payload_base.wrapping_add(variable_bits(br, 3)?);
            }
        }

        if self.version <= 1 {
            for i in 0..self.nb_presentations as usize {
                self.ac4_presentation_info(br, i)?;
            }
        }
        else {
            if br.get1() != 0 {
                self.short_program_id = br.get(16) as i32;
                if br.get1() != 0 {
                    br.skip_long(16 * 8);
                }
            }

            for i in 0..self.nb_presentations as usize {
                self.ac4_presentation_v1_info(br, i)?;
            }

            for i in 0..=self.total_groups as usize {
                if i >= self.ssgroup.len() {
                    return decode_error("ac4: invalid number of substream groups");
                }
                self.ac4_substream_group_info(br, i)?;
            }
        }

        self.substream_index_table(br)?;

        br.align();

        Ok(())
    }

    /// ac4dec.c: `further_loudness_info`
    pub(crate) fn further_loudness_info(
        &mut self,
        br: &mut BitReader<'_>,
        ssi: SsiRef,
    ) -> Result<()> {
        let m = &mut self.ssi_mut(ssi).meta;

        m.loudness_version = br.get(2) as i32;
        if m.loudness_version == 3 {
            m.loudness_version += br.get(4) as i32;
        }

        m.loud_prac_type = br.get(4) as i32;
        if m.loud_prac_type != 0 {
            if br.get1() != 0 {
                m.dialgate_prac_type = br.get(3) as i32;
            }
            m.loudcorr_type = br.get1() as i32;
        }

        if br.get1() != 0 {
            m.loudrelgat = br.get(11) as i32;
        }

        if br.get1() != 0 {
            m.loudspchgat = br.get(11) as i32;
            m.dialgate_prac_type = br.get(3) as i32;
        }

        if br.get1() != 0 {
            m.loudstrm3s = br.get(11) as i32;
        }

        if br.get1() != 0 {
            m.max_loudstrm3s = br.get(11) as i32;
        }

        if br.get1() != 0 {
            m.truepk = br.get(11) as i32;
        }

        if br.get1() != 0 {
            m.max_truepk = br.get(11) as i32;
        }

        if br.get1() != 0 {
            let mut prgmbndy_bit = 0;

            m.prgmbndy = 1;
            while prgmbndy_bit == 0 {
                if br.bits_left() <= 0 {
                    return decode_error("ac4: truncated loudness info");
                }
                m.prgmbndy = m.prgmbndy.wrapping_shl(1);
                prgmbndy_bit = br.get1();
            }

            m.end_or_start = br.get1() as i32;
            if br.get1() != 0 {
                m.prgmbndy_offset = br.get(11) as i32;
            }
        }

        if br.get1() != 0 {
            m.lra = br.get(10) as i32;
            m.lra_prac_type = br.get(3) as i32;
        }

        if br.get1() != 0 {
            m.loudmntry = br.get(11) as i32;
        }

        if br.get1() != 0 {
            m.max_loudmntry = br.get(11) as i32;
        }

        if br.get1() != 0 {
            let mut e_bits_size = br.get(5) as i32;
            if e_bits_size == 31 {
                e_bits_size = e_bits_size.wrapping_add(variable_bits(br, 4)?);
            }
            skip_bits_checked(br, e_bits_size)?;
        }

        Ok(())
    }

    /// ac4dec.c: `basic_metadata`
    pub(crate) fn basic_metadata(&mut self, br: &mut BitReader<'_>, ssi: SsiRef) -> Result<()> {
        let sus_ver = self.ssi(ssi).sus_ver;
        let channel_mode = self.ssi(ssi).channel_mode;

        if sus_ver == 0 {
            self.ssi_mut(ssi).meta.dialnorm_bits = br.get(7) as i32;
        }

        if br.get1() != 0 {
            if br.get1() != 0 {
                self.further_loudness_info(br, ssi)?;
            }

            let m = &mut self.ssi_mut(ssi).meta;

            if channel_mode == 1 && br.get1() != 0 {
                m.pre_dmixtyp_2ch = br.get(3) as i32;
                m.phase90_info_2ch = br.get(2) as i32;
            }

            if channel_mode > 1 {
                if br.get1() != 0 {
                    m.loro_center_mixgain = br.get(3) as i32;
                    m.loro_surround_mixgain = br.get(3) as i32;
                    if br.get1() != 0 {
                        m.loro_dmx_loud_corr = br.get(5) as i32;
                    }
                    if br.get1() != 0 {
                        m.ltrt_center_mixgain = br.get(3) as i32;
                        m.ltrt_surround_mixgain = br.get(3) as i32;
                    }
                    if br.get1() != 0 {
                        m.ltrt_dmx_loud_corr = br.get(5) as i32;
                    }
                    if channel_mode_contains_lfe(channel_mode) && br.get1() != 0 {
                        m.lfe_mixgain = br.get(5) as i32;
                    }
                    m.preferred_dmx_method = br.get(2) as i32;
                }
                if channel_mode == 3 || channel_mode == 4 {
                    if br.get1() != 0 {
                        m.pre_dmixtyp_5ch = br.get(3) as i32;
                    }
                    if br.get1() != 0 {
                        m.pre_upmixtyp_5ch = br.get(4) as i32;
                    }
                }

                if channel_mode >= 5 && channel_mode <= 10 && br.get1() != 0 {
                    if channel_mode >= 5 && channel_mode <= 6 {
                        m.pre_upmixtyp_3_4 = br.get(2) as i32;
                    }
                    else if channel_mode >= 9 && channel_mode <= 10 {
                        m.pre_upmixtyp_3_2_2 = br.get(1) as i32;
                    }
                }
                m.phase90_info_mc = br.get(2) as i32;
                m.surround_attenuation_known = br.get1() as i32;
                m.lfe_attenuation_known = br.get1() as i32;
            }

            if br.get1() != 0 {
                m.dc_block_on = br.get1() as i32;
            }
        }

        Ok(())
    }

    /// ac4dec.c: `drc_config`
    pub(crate) fn drc_config(&mut self, br: &mut BitReader<'_>, ssi: SsiRef) {
        let m = &mut self.ssi_mut(ssi).meta;

        m.drc_decoder_nr_modes = br.get(3) as i32;
        // ac4dec.c: drc_decoder_mode_config() is empty and consumes no bits.
        m.drc_eac3_profile = br.get(3) as i32;
    }

    /// ac4dec.c: `drc_frame`
    pub(crate) fn drc_frame(&mut self, br: &mut BitReader<'_>, ssi: SsiRef, iframe: i32) {
        if br.get1() != 0 && iframe != 0 {
            self.drc_config(br, ssi);
        }

        // ac4dec.c: drc_data() is empty and consumes no bits.
    }

    /// ac4dec.c: `metadata`
    pub(crate) fn metadata(
        &mut self,
        br: &mut BitReader<'_>,
        ssi: SsiRef,
        iframe: i32,
    ) -> Result<()> {
        self.basic_metadata(br, ssi)?;
        // ac4dec.c: extended_metadata() is empty and consumes no bits.
        let mut tools_metadata_size = br.get(7) as i32;
        if br.get1() != 0 {
            tools_metadata_size =
                tools_metadata_size.wrapping_add(variable_bits(br, 3)?.wrapping_shl(7));
        }
        // Parsed for its bit position only, the C decoder does not use it either.
        let _ = tools_metadata_size;

        self.drc_frame(br, ssi, iframe);

        // ac4dec.c: dialog_enhancement() is empty and consumes no bits.

        if br.get1() != 0 {
            // ac4dec.c: emdf_payloads_substream() is empty and consumes no bits.
        }

        Ok(())
    }

    /// ac4dec.c: `ac4_substream`
    pub(crate) fn ac4_substream(&mut self, br: &mut BitReader<'_>, ssinfo: SsiRef) -> Result<()> {
        let mut audio_size = br.get(15) as i32;
        if br.get1() != 0 {
            audio_size = audio_size.wrapping_add(variable_bits(br, 7)?.wrapping_shl(15));
        }
        if audio_size < 0 || audio_size > 131072 {
            return decode_error("ac4: invalid audio_size");
        }

        br.align();

        let offset = br.bit_pos() >> 3;
        let channel_mode = self.ssi(ssinfo).channel_mode;
        let iframe = self.ssi(ssinfo).iframe[0];
        self.audio_data(br, channel_mode, iframe)?;

        br.align();
        let consumed = (br.bit_pos() >> 3) - offset;
        if consumed > audio_size as usize {
            return decode_error("ac4: substream audio data overread");
        }
        if consumed < audio_size as usize {
            let mut non_zero = 0;

            for _ in consumed..audio_size as usize {
                non_zero += i32::from(br.get(8) != 0);
            }
            if non_zero != 0 {
                log::warn!("ac4: substream audio data underread: {}", non_zero);
            }
        }

        let iframe_global = self.iframe_global;
        self.metadata(br, ssinfo, iframe_global)?;

        br.align();

        Ok(())
    }

    /// ac4dec.c: `ac4_decode_frame` up to and including the post-processing of the spectra.
    ///
    /// Returns `None` while no I-frame has been seen. Otherwise the per-channel PCM can be
    /// fetched with [`Ac4Core::copy_channel`].
    pub(crate) fn decode_frame(&mut self, data: &[u8]) -> Result<Option<FrameInfo>> {
        if data.len() < 8 {
            return decode_error("ac4: invalid packet size");
        }

        let header = u16::from_be_bytes([data[0], data[1]]);
        let mut start_offset = 0;
        if header == 0xAC40 || header == 0xAC41 {
            let size = u16::from_be_bytes([data[2], data[3]]);

            start_offset = 4;
            if size == 0xFFFF {
                start_offset += 3;
            }
        }

        let mut br = BitReader::new(data);
        let br = &mut br;
        br.skip_long(start_offset * 8);

        self.ac4_toc(br)?;

        if !self.have_iframe {
            return Ok(None);
        }

        let presentation = self
            .target_presentation
            .min((self.nb_presentations - 1).max(0))
            .max(0)
            .min(self.pinfo.len() as i32 - 1) as usize;
        let ssinfo = if self.version == 2 { SsiRef::Group(0) } else { SsiRef::Pres(presentation) };
        let channel_mode = self.ssi(ssinfo).channel_mode;

        // ac4dec.c: audio_data() aborts on the modes 7.. and the C decoder has no layouts for them.
        if channel_mode < 0 || channel_mode > 6 {
            return unsupported_error("ac4: unsupported channel mode");
        }
        let channels = usize::from(CHANNEL_MODE_NB_CHANNELS[channel_mode as usize]);
        if channels == 0 || channels > self.substream.ssch.len() {
            return unsupported_error("ac4: unsupported channel mode");
        }

        let payload_bytes = usize::try_from(self.payload_base);
        match payload_bytes {
            Ok(n) => br.skip_long(n.saturating_mul(8)),
            Err(_) => return decode_error("ac4: invalid payload base"),
        }

        for i in 0..self.nb_substreams as usize {
            let substream_type = self.substream_type[i];

            match substream_type {
                ST_SUBSTREAM => self.ac4_substream(br, ssinfo)?,
                ST_PRESENTATION => skip_bytes(br, self.substream_size[i])?,
                _ => return decode_error("ac4: invalid substream type"),
            }

            if substream_type == ST_SUBSTREAM {
                break;
            }
        }

        if br.bits_left() < 0 {
            log::warn!("ac4: overread");
        }

        for ch in 0..channels {
            self.scale_spec(ch);
        }

        match channel_mode {
            0 => (), // nothing to do
            1 => self.stereo_processing(),
            3 | 4 => self.m5channel_processing(),
            _ => (),
        }

        for ch in 0..channels {
            self.prepare_channel(ch)?;
        }

        match channel_mode {
            0 => self.mono_aspx_processing()?,
            1 => self.stereo_aspx_processing()?,
            _ => (),
        }

        Ok(Some(FrameInfo {
            sample_rate: if self.fs_index != 0 { 48000 } else { 44100 },
            channels,
            channel_mode,
            nb_samples: self.frame_len_base as usize,
        }))
    }

    /// Writes output channel `out_ch` of the frame decoded by [`Ac4Core::decode_frame`].
    ///
    /// The decoder keeps the LFE after the main channels (5.1: `ssch[5]`, 7.1: `ssch[7]`)
    /// while the output order wants it fourth, hence the remapping.
    pub(crate) fn copy_channel(&mut self, out_ch: usize, channel_mode: i32, out: &mut [f32]) {
        let order: Option<&[u8]> = match channel_mode {
            4 => Some(&ORDER_5_1),
            6 => Some(&ORDER_7_1),
            _ => None,
        };

        let ch = match order {
            Some(order) => order.get(out_ch).map(|&ch| usize::from(ch)),
            None => Some(out_ch),
        };

        match ch {
            Some(ch) if ch < self.substream.ssch.len() => self.decode_channel(ch, out),
            _ => out.fill(0.0),
        }
    }

    /// ac4dec.c: `ac4_flush`
    pub(crate) fn flush(&mut self) {
        self.have_iframe = false;
        self.sequence_counter_prev = 0;
    }
}
