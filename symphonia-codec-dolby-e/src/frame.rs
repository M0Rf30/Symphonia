// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The parsing of the audio of a frame: the exponents, bit allocation, and mantissas of the
//! groups of each channel.
//!
//! Ported from `parse_*` of FFmpeg's `libavcodec/dolby_e.c` (LGPL-2.1-or-later, Copyright (c)
//! 2017 foo86).

use symphonia_common::mpeg::dolby_e::{DolbyEHeader, MAX_CHANNELS, WordStream};
use symphonia_core::errors::{Result, decode_error, unsupported_error};

use crate::bitalloc::{MAX_BIAS_EXP, bit_allocate};
use crate::dsp::{MAX_MANTISSAS, TABLES};
use crate::tables::*;

/// The maximum number of groups of a channel.
pub(crate) const MAX_GROUPS: usize = 8;

/// The maximum number of exponents of a channel.
const MAX_EXPONENTS: usize = 304;

/// The number of mantissas below which a truncated channel is not recoverable.
const MIN_MANTISSAS: usize = 800;

/// The groups and spectrum of a channel of a segment of a frame. A channel that failed to parse,
/// or without audio, has no groups.
pub(crate) struct Channel {
    pub bw_code: usize,
    pub nb_groups: usize,
    nb_mstr_exp: usize,
    pub groups: [Group; MAX_GROUPS],
    exp_strategy: [bool; MAX_GROUPS],
    exponents: [i32; MAX_EXPONENTS],
    bap: [u8; MAX_EXPONENTS],
    idx: [u8; MAX_EXPONENTS],
    pub mantissas: [f32; MAX_MANTISSAS],
}

impl Channel {
    pub fn new() -> Self {
        Channel {
            bw_code: 0,
            nb_groups: 0,
            nb_mstr_exp: 0,
            groups: [FRM_OFS[0][0][0]; MAX_GROUPS],
            exp_strategy: [false; MAX_GROUPS],
            exponents: [0; MAX_EXPONENTS],
            bap: [0; MAX_EXPONENTS],
            idx: [0; MAX_EXPONENTS],
            mantissas: [0.0; MAX_MANTISSAS],
        }
    }

    /// Read the master exponents and the biased exponents of the group `g`.
    fn unbias_exponents(&mut self, words: &mut WordStream, g: usize) {
        let g = self.groups[g];

        let mut mstr_exp = [0i32; 2];
        for e in &mut mstr_exp[..self.nb_mstr_exp] {
            *e = words.get_bits(2) as i32 * 6;
        }

        let mut bias_exp = [0i32; MAX_BIAS_EXP];
        for e in &mut bias_exp[..usize::from(g.nb_exponent)] {
            *e = words.get_bits(5) as i32;
        }

        let mut k = 0;
        for (i, &mstr) in mstr_exp[..self.nb_mstr_exp].iter().enumerate() {
            for _ in 0..g.nb_bias_exp[i] {
                self.exponents[usize::from(g.exp_ofs) + k] = mstr + bias_exp[k];
                k += 1;
            }
        }
    }

    fn parse_exponents(&mut self, words: &mut WordStream) {
        for i in 0..self.nb_groups {
            let g = self.groups[i];

            self.exp_strategy[i] =
                i == 0 || g.nb_exponent != self.groups[i - 1].nb_exponent || words.get_bit();

            if self.exp_strategy[i] {
                self.unbias_exponents(words, i);
            }
            else {
                let p = self.groups[i - 1];
                let n = usize::from(g.nb_exponent);
                self.exponents.copy_within(
                    usize::from(p.exp_ofs)..usize::from(p.exp_ofs) + n,
                    usize::from(g.exp_ofs),
                );
            }
        }
    }

    fn parse_bit_alloc(&mut self, words: &mut WordStream, fr_code: usize) -> Result<()> {
        let mut bap_strategy = [false; MAX_GROUPS];
        let mut fg_spc = [0usize; MAX_GROUPS];
        let mut fg_ofs = [0usize; MAX_GROUPS];
        let mut msk_mod = [0usize; MAX_GROUPS];

        for i in 0..self.nb_groups {
            bap_strategy[i] = i == 0 || words.get_bit();

            if bap_strategy[i] {
                fg_spc[i] = words.get_bits(2) as usize;
                fg_ofs[i] = words.get_bits(3) as usize;
                msk_mod[i] = words.get_bits(1) as usize;
            }
            else {
                fg_spc[i] = fg_spc[i - 1];
                fg_ofs[i] = fg_ofs[i - 1];
                msk_mod[i] = msk_mod[i - 1];
            }
        }

        if words.get_bit() {
            return unsupported_error("dolby_e: delta bit allocation");
        }

        let snr_ofs = words.get_bits(8) as i32;

        if snr_ofs == 0 {
            self.bap.fill(0);
            return Ok(());
        }

        for i in 0..self.nb_groups {
            let g = self.groups[i];
            let (ofs, n) = (usize::from(g.exp_ofs), usize::from(g.nb_exponent));

            if self.exp_strategy[i] || bap_strategy[i] {
                bit_allocate(
                    n,
                    usize::from(g.imdct_idx),
                    fr_code,
                    &self.exponents[ofs..ofs + n],
                    &mut self.bap[ofs..ofs + n],
                    fg_spc[i],
                    fg_ofs[i],
                    msk_mod[i],
                    snr_ofs,
                );
            }
            else {
                let pofs = usize::from(self.groups[i - 1].exp_ofs);
                self.bap.copy_within(pofs..pofs + n, ofs);
            }
        }

        Ok(())
    }

    fn parse_indices(&mut self, words: &mut WordStream) -> Result<()> {
        for i in 0..self.nb_groups {
            let g = self.groups[i];
            let (ofs, n) = (usize::from(g.exp_ofs), usize::from(g.nb_exponent));

            if words.get_bit() {
                let start = words.get_bits(6) as usize;

                if start > n {
                    return decode_error("dolby_e: invalid start index");
                }

                self.idx[ofs..ofs + start].fill(0);

                for j in start..n {
                    self.idx[ofs + j] = words.get_bits(2) as u8;
                }
            }
            else if i > 0 && n == usize::from(self.groups[i - 1].nb_exponent) {
                let pofs = usize::from(self.groups[i - 1].exp_ofs);
                self.idx.copy_within(pofs..pofs + n, ofs);
            }
            else {
                self.idx[ofs..ofs + n].fill(0);
            }
        }

        Ok(())
    }

    fn parse_mantissas(&mut self, words: &mut WordStream) {
        let t = &**TABLES;

        for i in 0..self.nb_groups {
            let g = self.groups[i];
            let mut mnt = usize::from(g.mnt_ofs);
            let mut j = 0;

            while j < usize::from(g.nb_exponent) {
                let e = usize::from(g.exp_ofs) + j;
                let bap = usize::from(self.bap[e]);
                let idx = usize::from(self.idx[e]);
                let size1 = u32::from(MANTISSA_SIZE1[bap][idx]);
                let count = usize::from(g.nb_mantissa[j]);
                let exp = t.exponent[self.exponents[e] as usize];
                let scale = t.mantissa1[size1 as usize][idx] * exp;

                if size1 == 0 {
                    self.mantissas[mnt..mnt + count].fill(0.0);
                }
                else if idx != 0 {
                    let mut values = [0i32; 100];
                    let escape = -(1i32 << (size1 - 1));

                    for v in &mut values[..count] {
                        *v = words.get_sbits(size1);
                    }

                    for (k, &v) in values[..count].iter().enumerate() {
                        self.mantissas[mnt + k] = if v != escape {
                            v as f32 * scale
                        }
                        else {
                            let size2 = u32::from(MANTISSA_SIZE2[bap][idx]);
                            let value = words.get_sbits(size2);
                            let a = t.mantissa2[size2 as usize][idx];
                            let b = t.mantissa3[size2 as usize][idx];

                            if value < 0 {
                                ((value + 1) as f32 * a - b) * exp
                            }
                            else {
                                (value as f32 * a + b) * exp
                            }
                        };
                    }
                }
                else {
                    let left = words.bits_left();

                    if i == self.nb_groups - 1
                        && (count as isize * size1 as isize) > left
                        && left >= 0
                        && mnt >= MIN_MANTISSAS
                    {
                        // The highest frequencies are not recoverable.
                        log::warn!("dolby_e: truncated mantissas @{mnt}");
                        break;
                    }

                    for k in 0..count {
                        self.mantissas[mnt + k] = words.get_sbits(size1) as f32 * scale;
                    }
                }

                mnt += count;
                j += 1;
            }

            while j < usize::from(g.nb_exponent) + self.bw_code {
                let count = usize::from(g.nb_mantissa[j]);
                self.mantissas[mnt..mnt + count].fill(0.0);
                mnt += count;
                j += 1;
            }
        }
    }
}

/// The frame being parsed: its header, and the channels of the two segments.
pub(crate) struct Frame {
    /// `channels[segment][channel]`.
    pub channels: Box<[[Channel; MAX_CHANNELS]; 2]>,
}

impl Frame {
    pub fn new() -> Self {
        Frame {
            channels: Box::new(std::array::from_fn(|_| std::array::from_fn(|_| Channel::new()))),
        }
    }

    fn parse_channel(
        &mut self,
        words: &mut WordStream,
        header: &DolbyEHeader,
        ch: usize,
        seg_id: usize,
    ) -> Result<()> {
        let c = &mut self.channels[seg_id][ch];

        if header.rev_id[ch] > 1 {
            return unsupported_error("dolby_e: unsupported encoder revision");
        }

        let gr_code;

        if LFE_CHANNEL[header.prog_conf] == ch as i8 {
            gr_code = 3;
            c.bw_code = 29;
        }
        else {
            gr_code = words.get_bits(2) as usize;
            c.bw_code = words.get_bits(3) as usize;

            if gr_code == 3 {
                return decode_error("dolby_e: invalid group type code");
            }
        }

        c.nb_groups = usize::from(NB_GROUPS[gr_code]);
        c.nb_mstr_exp = usize::from(NB_MSTR_EXP[gr_code]);

        for i in 0..c.nb_groups {
            c.groups[i] = FRM_OFS[seg_id][gr_code][i];

            if c.nb_mstr_exp == 2 {
                c.groups[i].nb_exponent -= c.bw_code as u8;
                c.groups[i].nb_bias_exp[1] -= c.bw_code as u8;
            }
        }

        c.parse_exponents(words);
        c.parse_bit_alloc(words, header.fr_code)?;
        c.parse_indices(words)?;
        c.parse_mantissas(words);

        if words.bits_left() < 0 {
            return decode_error("dolby_e: read past the end of a channel");
        }

        Ok(())
    }

    /// Parse the audio of the channels `start..end` of the segment `seg_id`. A channel that
    /// cannot be parsed is silent.
    pub fn parse_audio(
        &mut self,
        words: &mut WordStream,
        data: &[u8],
        header: &DolbyEHeader,
        start: usize,
        end: usize,
        seg_id: usize,
    ) -> Result<()> {
        let key = words.parse_key(data)?;

        for ch in start..end {
            let size = usize::from(header.ch_size[ch]);

            if size == 0 {
                self.channels[seg_id][ch].nb_groups = 0;
                continue;
            }

            words.convert(data, size, key)?;

            if let Err(err) = self.parse_channel(words, header, ch, seg_id) {
                log::debug!("{err}");
                self.channels[seg_id][ch].nb_groups = 0;
            }

            words.skip_words(size)?;
        }

        words.skip_words(1)
    }
}
