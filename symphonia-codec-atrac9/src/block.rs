// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Block (channel element) bitstream parsing and spectrum reconstruction, following FFmpeg's
//! `atrac9dec.c` (`atrac9_decode_block` and the functions it calls).

use symphonia_core::errors::{Result, decode_error};

use crate::bits::{BitReader, Codebooks, Vlc, sign_extend};
use crate::noise::Lfg;
use crate::tables::{
    B_DIST, BAND_EXT_CNT, BAND_EXT_GROUP, BAND_EXT_LENGTHS, BAND_EXT_SCALES_M0, BAND_EXT_SCALES_M2,
    BAND_EXT_SCALES_M3, BAND_EXT_SCALES_M4, BAND_Q_UNIT_MAP, HUFFMAN_COEFFS, Q_UNIT_TO_CODEBOOKIDX,
    Q_UNIT_TO_COEFF_CNT, Q_UNIT_TO_COEFF_IDX, QUANT_STEP_COARSE, QUANT_STEP_FINE, SCALEFACTOR_C,
    SF_WEIGHTS, SRI_MAX_BANDS,
};

/// The type of a block (FFmpeg's `ATRAC9BlockType`).
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum BlockType {
    /// A single channel.
    Sce,
    /// Two coupled channels.
    Cpe,
    /// A single low-frequency effects channel.
    Lfe,
}

/// State of one channel of a block.
pub(crate) struct ChannelData {
    band_ext: usize,
    q_unit_cnt: usize,
    band_ext_data: [i32; 4],
    scalefactors: [i32; 31],
    scalefactors_prev: [i32; 31],

    precision_coarse: [i32; 30],
    precision_fine: [i32; 30],
    precision_mask: [i32; 30],

    codebookset: [usize; 30],

    q_coeffs_coarse: [i32; 256],
    q_coeffs_fine: [i32; 256],

    /// The reconstructed spectrum.
    pub(crate) coeffs: [f32; 256],
    /// The second half of the previous IMDCT output.
    pub(crate) prev_win: [f32; 128],
}

impl ChannelData {
    fn new() -> Self {
        ChannelData {
            band_ext: 0,
            q_unit_cnt: 0,
            band_ext_data: [0; 4],
            scalefactors: [0; 31],
            scalefactors_prev: [0; 31],
            precision_coarse: [0; 30],
            precision_fine: [0; 30],
            precision_mask: [0; 30],
            codebookset: [0; 30],
            q_coeffs_coarse: [0; 256],
            q_coeffs_fine: [0; 256],
            coeffs: [0.0; 256],
            prev_win: [0.0; 128],
        }
    }
}

/// State of one block.
pub(crate) struct BlockData {
    pub(crate) channel: [ChannelData; 2],

    band_count: usize,
    q_unit_cnt: usize,
    q_unit_cnt_prev: usize,

    /// Stereo blocks only.
    stereo_q_unit: usize,

    /// Band extension.
    has_band_ext: bool,
    has_band_ext_data: bool,
    band_ext_q_unit: usize,

    /// Bit allocation gradient.
    grad_mode: u32,
    grad_boundary: usize,
    gradient: [i32; 31],

    /// Stereo.
    cpe_base_channel: usize,
    is_signs: [i32; 30],

    reusable: bool,
}

impl BlockData {
    pub(crate) fn new() -> Box<Self> {
        Box::new(BlockData {
            channel: [ChannelData::new(), ChannelData::new()],
            band_count: 0,
            q_unit_cnt: 0,
            q_unit_cnt_prev: 0,
            stereo_q_unit: 0,
            has_band_ext: false,
            has_band_ext_data: false,
            band_ext_q_unit: 0,
            grad_mode: 0,
            grad_boundary: 0,
            gradient: [0; 31],
            cpe_base_channel: 0,
            is_signs: [0; 30],
            reusable: false,
        })
    }

    /// Clears the IMDCT overlap of both channels.
    pub(crate) fn reset_overlap(&mut self) {
        for c in self.channel.iter_mut() {
            c.prev_win = [0.0; 128];
        }
    }
}

/// Decoder state that is shared by all blocks.
pub(crate) struct Shared {
    pub(crate) samplerate_idx: usize,
    /// The bit allocation curves (FFmpeg's `alloc_curve`).
    pub(crate) alloc_curve: Box<[[u8; 48]; 48]>,
    pub(crate) lfg: Lfg,
    pub(crate) codebooks: &'static Codebooks,
}

impl Shared {
    pub(crate) fn new(samplerate_idx: usize, codebooks: &'static Codebooks) -> Self {
        let mut alloc_curve = Box::new([[0u8; 48]; 48]);
        let len = B_DIST.len();
        for i in 1..=len {
            for j in 0..i {
                alloc_curve[i - 1][j] = B_DIST[(j * len) / i];
            }
        }
        Shared { samplerate_idx, alloc_curve, lfg: Lfg::new(), codebooks }
    }
}

fn sf_vlc(cb: &Codebooks, signed: usize, len: usize) -> Result<&Vlc> {
    match cb.sf[signed].get(len).and_then(Option::as_ref) {
        Some(v) => Ok(v),
        None => decode_error("atrac9: invalid scalefactor codebook"),
    }
}

impl BlockData {
    fn parse_gradient(&mut self, sh: &Shared, br: &mut BitReader<'_>) -> Result<()> {
        self.grad_mode = br.get(2);

        let (range, value) = if self.grad_mode != 0 {
            let r0 = br.get(5) as usize;
            let v0 = br.get(5) as i32;
            ([r0, 31], [v0, 31])
        }
        else {
            let r0 = br.get(6) as usize;
            let r1 = br.get(6) as usize + 1;
            let v0 = br.get(5) as i32;
            let v1 = br.get(5) as i32;
            ([r0, r1], [v0, v1])
        };
        self.grad_boundary = br.get(4) as usize;

        if range[0] >= range[1] || range[1] > 31 {
            return decode_error("atrac9: invalid gradient range");
        }

        if self.grad_boundary > self.q_unit_cnt {
            return decode_error("atrac9: invalid gradient boundary");
        }

        let values = value[1] - value[0];
        let sign = if values < 0 { -1 } else { 1 };
        let base = value[0] + sign;
        let scale = (values.abs() - 1) as f32 / 31.0f32;
        let curve = &sh.alloc_curve[range[1] - range[0] - 1];

        for i in 0..=self.q_unit_cnt {
            self.gradient[i] = value[usize::from(i >= range[0])];
        }

        for i in range[0]..range[1] {
            self.gradient[i] = base + sign * ((scale * f32::from(curve[i - range[0]])) as i32);
        }

        Ok(())
    }

    fn calc_precision(&mut self, ch: usize) {
        let q_unit_cnt = self.q_unit_cnt;
        let grad_mode = self.grad_mode;
        let grad_boundary = self.grad_boundary;
        let gradient = &self.gradient;
        let c = &mut self.channel[ch];

        c.precision_mask = [0; 30];
        for i in 1..q_unit_cnt {
            let delta = (c.scalefactors[i] - c.scalefactors[i - 1]).abs() - 1;
            if delta > 0 {
                let neg = usize::from(c.scalefactors[i - 1] > c.scalefactors[i]);
                c.precision_mask[i - neg] += delta.min(5);
            }
        }

        if grad_mode != 0 {
            for i in 0..q_unit_cnt {
                c.precision_coarse[i] = c.scalefactors[i];
                c.precision_coarse[i] += c.precision_mask[i] - gradient[i];
                if c.precision_coarse[i] < 0 {
                    continue;
                }
                match grad_mode {
                    1 => c.precision_coarse[i] >>= 1,
                    2 => c.precision_coarse[i] = (3 * c.precision_coarse[i]) >> 3,
                    3 => c.precision_coarse[i] >>= 2,
                    _ => (),
                }
            }
        }
        else {
            for i in 0..q_unit_cnt {
                c.precision_coarse[i] = c.scalefactors[i] - gradient[i];
            }
        }

        for i in 0..q_unit_cnt {
            c.precision_coarse[i] = c.precision_coarse[i].max(1);
        }

        for i in 0..grad_boundary {
            c.precision_coarse[i] += 1;
        }

        for i in 0..q_unit_cnt {
            c.precision_fine[i] = 0;
            if c.precision_coarse[i] > 15 {
                c.precision_fine[i] = c.precision_coarse[i].min(30) - 15;
                c.precision_coarse[i] = 15;
            }
        }
    }

    fn parse_band_ext(&mut self, br: &mut BitReader<'_>, stereo: bool) -> Result<()> {
        let mut ext_band = 0;

        if self.has_band_ext {
            if self.q_unit_cnt < 13 || self.q_unit_cnt > 20 {
                return decode_error("atrac9: invalid band extension unit count");
            }
            ext_band = usize::from(BAND_EXT_GROUP[self.q_unit_cnt - 13][2]);
            if stereo {
                let v = br.get(2) as usize;
                self.channel[1].band_ext = if ext_band > 2 { v } else { 4 };
            }
            else {
                br.skip(1);
            }
        }

        self.has_band_ext_data = br.get1();
        if !self.has_band_ext_data {
            return Ok(());
        }

        if !self.has_band_ext {
            br.skip(2);
            let n = br.get(5);
            br.skip(n);
            return Ok(());
        }

        let v = br.get(2) as usize;
        self.channel[0].band_ext = if ext_band > 2 { v } else { 4 };

        let clip_only = br.get(5) == 0;
        for c in self.channel.iter_mut().take(usize::from(stereo) + 1) {
            let count = usize::from(BAND_EXT_CNT[c.band_ext][ext_band]);
            for j in 0..count {
                let len = u32::from(BAND_EXT_LENGTHS[c.band_ext][ext_band][j]);
                if clip_only {
                    // av_clip_uintp2.
                    c.band_ext_data[j] = c.band_ext_data[j].clamp(0, (1 << len) - 1);
                }
                else {
                    c.band_ext_data[j] = br.get(len) as i32;
                }
            }
        }

        Ok(())
    }

    fn read_scalefactors(
        &mut self,
        cb: &Codebooks,
        br: &mut BitReader<'_>,
        ch: usize,
        first_in_pkt: bool,
    ) -> Result<()> {
        const MODE_MAP: [[u32; 4]; 2] = [[0, 1, 2, 3], [0, 2, 3, 4]];
        let mode = MODE_MAP[ch][br.get(2) as usize];

        let band_ext_q_unit = self.band_ext_q_unit;
        let q_unit_cnt_prev = self.q_unit_cnt_prev;
        let ch0_sf = self.channel[0].scalefactors;
        let c = &mut self.channel[ch];

        c.scalefactors = [0; 31];

        if first_in_pkt && (mode == 4 || (mode == 3 && ch == 0)) {
            return decode_error("atrac9: invalid scalefactor coding mode");
        }

        match mode {
            0 => {
                // VLC delta offset.
                let sf_weights = &SF_WEIGHTS[br.get(3) as usize];
                let base = br.get(5) as i32;
                let len = br.get(2) as usize + 3;
                let tab = sf_vlc(cb, 0, len)?;

                c.scalefactors[0] = br.get(len as u32) as i32;

                for i in 1..band_ext_q_unit {
                    let val = c.scalefactors[i - 1] + tab.read(br)?;
                    c.scalefactors[i] = val & ((1 << len) - 1);
                }

                for i in 0..band_ext_q_unit {
                    c.scalefactors[i] += base - i32::from(sf_weights[i]);
                }
            }
            1 => {
                // CLC offset.
                let len = br.get(2) + 2;
                let base = if len < 5 { br.get(5) as i32 } else { 0 };
                for i in 0..band_ext_q_unit {
                    c.scalefactors[i] = base + br.get(len) as i32;
                }
            }
            2 | 4 => {
                // VLC distance to baseline.
                let (baseline, baseline_len) = if mode == 4 {
                    (c.scalefactors_prev, q_unit_cnt_prev)
                }
                else if ch != 0 {
                    (ch0_sf, band_ext_q_unit)
                }
                else {
                    (c.scalefactors_prev, q_unit_cnt_prev)
                };

                let len = br.get(2) as usize + 2;
                let unit_cnt = band_ext_q_unit.min(baseline_len);
                let tab = sf_vlc(cb, 1, len)?;

                for i in 0..unit_cnt {
                    let dist = tab.read(br)?;
                    c.scalefactors[i] = baseline[i] + dist;
                }

                for i in unit_cnt..band_ext_q_unit {
                    c.scalefactors[i] = br.get(5) as i32;
                }
            }
            _ => {
                // VLC offset with baseline.
                let (baseline, baseline_len) = if ch != 0 {
                    (ch0_sf, band_ext_q_unit)
                }
                else {
                    (c.scalefactors_prev, q_unit_cnt_prev)
                };

                let base = br.get(5) as i32 - (1 << (5 - 1));
                let len = br.get(2) as usize + 1;
                let unit_cnt = band_ext_q_unit.min(baseline_len);
                let tab = sf_vlc(cb, 0, len)?;

                c.scalefactors[0] = br.get(len as u32) as i32;

                for i in 1..unit_cnt {
                    let val = c.scalefactors[i - 1] + tab.read(br)?;
                    c.scalefactors[i] = val & ((1 << len) - 1);
                }

                for i in 0..unit_cnt {
                    c.scalefactors[i] += base + baseline[i];
                }

                for i in unit_cnt..band_ext_q_unit {
                    c.scalefactors[i] = br.get(5) as i32;
                }
            }
        }

        for i in 0..band_ext_q_unit {
            if c.scalefactors[i] < 0 || c.scalefactors[i] > 31 {
                return decode_error("atrac9: invalid scalefactor");
            }
        }

        c.scalefactors_prev = c.scalefactors;

        Ok(())
    }

    fn calc_codebook_idx(&mut self, ch: usize, samplerate_idx: usize) {
        let c = &mut self.channel[ch];
        let mut avg = 0;
        let last_sf = c.scalefactors[c.q_unit_cnt];

        c.codebookset = [0; 30];

        if c.q_unit_cnt <= 1 {
            return;
        }
        if samplerate_idx > 7 {
            return;
        }

        c.scalefactors[c.q_unit_cnt] = c.scalefactors[c.q_unit_cnt - 1];

        if c.q_unit_cnt > 12 {
            for i in 0..12 {
                avg += c.scalefactors[i];
            }
            avg = (avg + 6) / 12;
        }

        for i in 8..c.q_unit_cnt {
            let prev = c.scalefactors[i - 1];
            let cur = c.scalefactors[i];
            let next = c.scalefactors[i + 1];
            let min = prev.min(next);
            if cur - min >= 3 || 2 * cur - prev - next >= 3 {
                c.codebookset[i] = 1;
            }
        }

        for i in 12..c.q_unit_cnt {
            let cur = c.scalefactors[i];
            let cnd = i32::from(Q_UNIT_TO_COEFF_CNT[i] == 16);
            let min = c.scalefactors[i + 1].min(c.scalefactors[i - 1]);
            if c.codebookset[i] != 0 {
                continue;
            }

            c.codebookset[i] = usize::from((cur - min) >= 2 && cur >= (avg - cnd));
        }

        c.scalefactors[c.q_unit_cnt] = last_sf;
    }

    fn read_coeffs_coarse(
        &mut self,
        cb: &Codebooks,
        br: &mut BitReader<'_>,
        ch: usize,
        samplerate_idx: usize,
    ) -> Result<()> {
        let max_prec = if samplerate_idx > 7 { 1 } else { 7 };
        let c = &mut self.channel[ch];

        c.q_coeffs_coarse = [0; 256];

        for i in 0..c.q_unit_cnt {
            let start = Q_UNIT_TO_COEFF_IDX[i];
            let bands = Q_UNIT_TO_COEFF_CNT[i];
            let prec = (c.precision_coarse[i] + 1) as usize;
            let coeffs = &mut c.q_coeffs_coarse[start..start + bands];

            if prec <= max_prec {
                let set = c.codebookset[i];
                let cbi = Q_UNIT_TO_CODEBOOKIDX[i];
                let huff = &HUFFMAN_COEFFS[set][prec][cbi];
                let Some(tab) = cb.coeffs[set][prec][cbi].as_ref()
                else {
                    return decode_error("atrac9: invalid coefficient codebook");
                };
                let value_cnt = usize::from(huff.1);
                let value_bits = u32::from(huff.3);
                let groups = bands >> huff.2;

                for j in 0..groups {
                    let mut val = tab.read(br)? as u32;
                    for k in 0..value_cnt {
                        coeffs[j * value_cnt + k] = sign_extend(val, value_bits);
                        val >>= value_bits;
                    }
                }
            }
            else {
                for coeff in coeffs.iter_mut() {
                    *coeff = sign_extend(br.get(prec as u32), prec as u32);
                }
            }
        }

        Ok(())
    }

    fn read_coeffs_fine(&mut self, br: &mut BitReader<'_>, ch: usize) {
        let c = &mut self.channel[ch];

        c.q_coeffs_fine = [0; 256];

        for i in 0..c.q_unit_cnt {
            let start = Q_UNIT_TO_COEFF_IDX[i];
            let end = Q_UNIT_TO_COEFF_IDX[i + 1];

            if c.precision_fine[i] <= 0 {
                continue;
            }
            let len = (c.precision_fine[i] + 1) as u32;

            for j in start..end {
                c.q_coeffs_fine[j] = sign_extend(br.get(len), len);
            }
        }
    }

    fn dequantize(&mut self, ch: usize) {
        let c = &mut self.channel[ch];

        c.coeffs = [0.0; 256];

        for i in 0..c.q_unit_cnt {
            let start = Q_UNIT_TO_COEFF_IDX[i];
            let end = Q_UNIT_TO_COEFF_IDX[i + 1];

            let coarse_c = QUANT_STEP_COARSE[c.precision_coarse[i] as usize];
            let fine_c = QUANT_STEP_FINE[c.precision_fine[i] as usize];

            for j in start..end {
                let vc = c.q_coeffs_coarse[j] as f32 * coarse_c;
                let vf = c.q_coeffs_fine[j] as f32 * fine_c;
                c.coeffs[j] = vc + vf;
            }
        }
    }

    fn apply_intensity_stereo(&mut self, stereo: bool) {
        if !stereo || self.q_unit_cnt <= self.stereo_q_unit {
            return;
        }

        let src_ch = self.cpe_base_channel;
        let src = self.channel[src_ch].coeffs;
        let dst = &mut self.channel[1 - src_ch].coeffs;

        for i in self.stereo_q_unit..self.q_unit_cnt {
            let sign = self.is_signs[i] as f32;
            let start = Q_UNIT_TO_COEFF_IDX[i];
            let end = Q_UNIT_TO_COEFF_IDX[i + 1];
            for j in start..end {
                dst[j] = sign * src[j];
            }
        }
    }

    fn apply_scalefactors(&mut self, stereo: bool) {
        for i in 0..=usize::from(stereo) {
            let q_unit_cnt = self.q_unit_cnt;
            let c = &mut self.channel[i];
            for j in 0..q_unit_cnt {
                let start = Q_UNIT_TO_COEFF_IDX[j];
                let end = Q_UNIT_TO_COEFF_IDX[j + 1];
                let scale = SCALEFACTOR_C[c.scalefactors[j] as usize];
                for k in start..end {
                    c.coeffs[k] *= scale;
                }
            }
        }
    }

    fn apply_band_extension(&mut self, lfg: &mut Lfg, stereo: bool) {
        let group = &BAND_EXT_GROUP[self.q_unit_cnt - 13];
        let g_units = [
            self.q_unit_cnt,
            usize::from(group[0]),
            usize::from(group[1]),
            usize::from(group[1]).max(22),
        ];

        let g_bins = [
            Q_UNIT_TO_COEFF_IDX[g_units[0]],
            Q_UNIT_TO_COEFF_IDX[g_units[1]],
            Q_UNIT_TO_COEFF_IDX[g_units[2]],
            Q_UNIT_TO_COEFF_IDX[g_units[3]],
        ];

        for ch in 0..=usize::from(stereo) {
            let c = &mut self.channel[ch];

            // Mirror the spectrum.
            for i in 0..3 {
                for j in 0..(g_bins[i + 1] - g_bins[i]) {
                    if let Some(src) = (g_bins[i] - j).checked_sub(1) {
                        c.coeffs[g_bins[i] + j] = c.coeffs[src];
                    }
                }
            }

            let d = c.band_ext_data;
            let m0 = |m: usize, row: usize, idx: i32| -> f32 {
                BAND_EXT_SCALES_M0[m][row].get(idx as usize).copied().unwrap_or(0.0)
            };

            match c.band_ext {
                0 => {
                    let mut sf = [0.0f32; 6];
                    let l = g_units[3] - g_units[0] - 1;
                    let n_start = Q_UNIT_TO_COEFF_IDX[g_units[3] - 1];
                    let n_cnt = Q_UNIT_TO_COEFF_CNT[g_units[3] - 1];
                    match group[2] {
                        3 => {
                            sf[0] = m0(0, 0, d[0]);
                            sf[1] = m0(0, 1, d[0]);
                            sf[2] = m0(0, 2, d[1]);
                            sf[3] = m0(0, 3, d[2]);
                            sf[4] = m0(0, 4, d[3]);
                        }
                        4 => {
                            sf[0] = m0(1, 0, d[0]);
                            sf[1] = m0(1, 1, d[0]);
                            sf[2] = m0(1, 2, d[1]);
                            sf[3] = m0(1, 3, d[2]);
                            sf[4] = m0(1, 4, d[3]);
                        }
                        5 => {
                            sf[0] = m0(2, 0, d[0]);
                            sf[1] = m0(2, 1, d[1]);
                            sf[2] = m0(2, 2, d[1]);
                        }
                        _ => (),
                    }

                    if let Some(slot) = sf.get_mut(l) {
                        *slot = SCALEFACTOR_C[c.scalefactors[g_units[0]] as usize];
                    }

                    fill_with_noise(lfg, &mut c.coeffs, n_start, n_cnt);
                    scale_band_ext_coeffs(&mut c.coeffs, &sf, g_units[0], g_units[3]);
                }
                1 => {
                    let mut sf = [0.0f32; 6];
                    for (i, slot) in sf.iter_mut().enumerate().take(g_units[3] - g_units[0]) {
                        *slot = SCALEFACTOR_C[c.scalefactors[g_units[0] + i] as usize];
                    }

                    fill_with_noise(lfg, &mut c.coeffs, g_bins[0], g_bins[3] - g_bins[0]);
                    scale_band_ext_coeffs(&mut c.coeffs, &sf, g_units[0], g_units[3]);
                }
                2 => {
                    let m2 =
                        |idx: i32| BAND_EXT_SCALES_M2.get(idx as usize).copied().unwrap_or(0.0);
                    let g_sf = [m2(d[0]), m2(d[1])];

                    for i in 0..2 {
                        for j in g_bins[i]..g_bins[i + 1] {
                            c.coeffs[j] *= g_sf[i];
                        }
                    }
                }
                3 => {
                    let m3 = |idx: i32, col: usize| {
                        BAND_EXT_SCALES_M3.get(idx as usize).map_or(0.0, |e| e[col])
                    };
                    let mut scale = m3(d[0], 0);
                    let rate = 2.0f64.powf(f64::from(m3(d[1], 1))) as f32;
                    for i in g_bins[0]..g_bins[3] {
                        scale *= rate;
                        c.coeffs[i] *= scale;
                    }
                }
                _ => {
                    let m = BAND_EXT_SCALES_M4.get(d[0] as usize).copied().unwrap_or(0.0);
                    let g_sf = [0.7079468f32 * m, 0.5011902f32 * m, 0.3548279f32 * m];

                    for i in 0..3 {
                        for j in g_bins[i]..g_bins[i + 1] {
                            c.coeffs[j] *= g_sf[i];
                        }
                    }
                }
            }
        }
    }

    /// Decodes one block from the bitstream into the spectra of its channels.
    pub(crate) fn decode(
        &mut self,
        sh: &mut Shared,
        br: &mut BitReader<'_>,
        kind: BlockType,
    ) -> Result<()> {
        let first_in_pkt = !br.get1();
        let reuse_params = br.get1();
        let stereo = kind == BlockType::Cpe;

        if kind == BlockType::Lfe {
            let precision = if reuse_params { 8 } else { 4 };
            self.q_unit_cnt = 2;
            let c = &mut self.channel[0];
            c.q_unit_cnt = 2;

            c.scalefactors = [0; 31];
            c.q_coeffs_fine = [0; 256];
            c.q_coeffs_coarse = [0; 256];

            for i in 0..2 {
                c.scalefactors[i] = br.get(5) as i32;
                c.precision_coarse[i] = precision;
                c.precision_fine[i] = 0;
            }

            for i in 0..2 {
                let start = Q_UNIT_TO_COEFF_IDX[i];
                let end = Q_UNIT_TO_COEFF_IDX[i + 1];
                for j in start..end {
                    c.q_coeffs_coarse[j] = br.get(c.precision_coarse[i] as u32 + 1) as i32;
                }
            }

            self.dequantize(0);
            self.apply_scalefactors(false);
            return Ok(());
        }

        if first_in_pkt && reuse_params {
            return decode_error("atrac9: invalid block flags");
        }

        // Band parameters.
        if !reuse_params {
            let min_band_count = if sh.samplerate_idx > 7 { 1 } else { 3 };
            self.reusable = false;
            self.band_count = br.get(4) as usize + min_band_count;
            // The band count can be at most 15 + 3, the map has 19 entries.
            self.q_unit_cnt = usize::from(BAND_Q_UNIT_MAP[self.band_count]);

            self.band_ext_q_unit = self.q_unit_cnt;
            self.stereo_q_unit = self.q_unit_cnt;

            if self.band_count > usize::from(SRI_MAX_BANDS[sh.samplerate_idx]) {
                return decode_error("atrac9: invalid band count");
            }

            if stereo {
                let stereo_band = br.get(4) as usize + min_band_count;
                if stereo_band > self.band_count {
                    return decode_error("atrac9: invalid stereo band");
                }
                self.stereo_q_unit = usize::from(BAND_Q_UNIT_MAP[stereo_band]);
            }

            self.has_band_ext = br.get1();
            if self.has_band_ext {
                let ext_band = br.get(4) as usize + min_band_count;
                if ext_band < self.band_count || ext_band >= BAND_Q_UNIT_MAP.len() {
                    return decode_error("atrac9: invalid extension band");
                }
                self.band_ext_q_unit = usize::from(BAND_Q_UNIT_MAP[ext_band]);
            }
            self.reusable = true;
        }
        if !self.reusable {
            return decode_error("atrac9: invalid block reused");
        }

        // Bit allocation gradient.
        self.parse_gradient(sh, br)?;

        // Intensity stereo data.
        self.cpe_base_channel = 0;
        if stereo {
            self.cpe_base_channel = usize::from(br.get1());
            if br.get1() {
                for i in self.stereo_q_unit..self.q_unit_cnt {
                    self.is_signs[i] = 1 - 2 * br.get(1) as i32;
                }
            }
            else {
                self.is_signs = [1; 30];
            }
        }

        // Band extension.
        self.parse_band_ext(br, stereo)?;

        // Scalefactors and coefficients.
        for i in 0..=usize::from(stereo) {
            self.channel[i].q_unit_cnt =
                if i == self.cpe_base_channel { self.q_unit_cnt } else { self.stereo_q_unit };
            self.read_scalefactors(sh.codebooks, br, i, first_in_pkt)?;

            self.calc_precision(i);
            self.calc_codebook_idx(i, sh.samplerate_idx);
            self.read_coeffs_coarse(sh.codebooks, br, i, sh.samplerate_idx)?;
            self.read_coeffs_fine(br, i);
            self.dequantize(i);
        }

        self.q_unit_cnt_prev =
            if self.has_band_ext { self.band_ext_q_unit } else { self.q_unit_cnt };

        self.apply_intensity_stereo(stereo);
        self.apply_scalefactors(stereo);

        if self.has_band_ext && self.has_band_ext_data {
            self.apply_band_extension(&mut sh.lfg, stereo);
        }

        Ok(())
    }
}

fn fill_with_noise(lfg: &mut Lfg, coeffs: &mut [f32; 256], start: usize, count: usize) {
    let mut maxval = 0.0f32;
    for i in (0..count).step_by(2) {
        let tmp = lfg.gaussian_pair();
        coeffs[start + i] = tmp[0] as f32;
        coeffs[start + i + 1] = tmp[1] as f32;
        maxval = tmp[0].abs().max(tmp[1].abs()).max(f64::from(maxval)) as f32;
    }
    // Normalize.
    for coeff in &mut coeffs[start..start + count] {
        *coeff /= maxval;
    }
}

fn scale_band_ext_coeffs(coeffs: &mut [f32; 256], sf: &[f32; 6], s_unit: usize, e_unit: usize) {
    for i in s_unit..e_unit {
        let start = Q_UNIT_TO_COEFF_IDX[i];
        let end = Q_UNIT_TO_COEFF_IDX[i + 1];
        let scale = sf.get(i - s_unit).copied().unwrap_or(0.0);
        for coeff in &mut coeffs[start..end] {
            *coeff *= scale;
        }
    }
}
