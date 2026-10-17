// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `Mps212Data()` of a USAC frame (ISO/IEC 23003-3 Table 40, ISO/IEC 23003-1 5.2) and its
//! mapping to one parameter value per band and parameter set.
//!
//! The control flow follows the float MPEG Surround decoder of libxaac
//! (`ixheaacd_mps_frame_parsing`, `ixheaacd_mps_ecdata_decoding`,
//! `ixheaacd_mps_mapindexdata`, Apache-2.0).

use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};

use symphonia_common::mpeg::audio::usac::UsacMps212Config;

use super::huffman::{PairRequest, ParamType, ec_data_pair_dec};
use super::{MAX_BANDS, MAX_SETS};

/// The frequency strides of the coded parameter bands.
const STRIDE: [usize; 4] = [1, 2, 5, 28];

/// Smoothing time constants, in time slots.
const SMOOTHING_TIME: [f64; 4] = [64.0, 128.0, 256.0, 512.0];

type Idx = [[i32; MAX_BANDS]; MAX_SETS];

/// The coding state of one parameter type.
#[derive(Clone, Copy, Default)]
struct ParamData {
    /// `bsXxxDataMode` of each parameter set.
    mode: [u8; MAX_SETS],
    /// `bsQuantCoarseXxx` of each coded set.
    coarse: [bool; MAX_SETS],
    /// `bsFreqResStrideXxx` of each coded set.
    stride: [u8; MAX_SETS],
    /// The quantizer of the last coded set of the previous frame.
    coarse_prev: bool,
}

/// The parsed parameters of a frame and the history they are coded against.
pub(super) struct Params {
    pub time_slots: usize,
    pub param_bands: usize,
    pub num_bands_ipd: usize,
    high_rate: bool,
    phase_coding: bool,

    pub num_sets: usize,
    pub num_sets_prev: usize,
    pub param_slots: [i32; MAX_SETS],
    /// The number of time slots covered by each parameter set, and its inverse.
    pub slot_diff: [i32; MAX_SETS],
    pub inv_slot_diff: [f64; MAX_SETS],
    ext_frame: bool,
    independency: bool,

    cld: ParamData,
    icc: ParamData,
    ipd: ParamData,
    cmp_cld: Idx,
    cmp_icc: Idx,
    cmp_ipd: Idx,
    cmp_cld_prev: [i32; MAX_BANDS],
    cmp_icc_prev: [i32; MAX_BANDS],
    cmp_ipd_prev: [i32; MAX_BANDS],
    cld_pre: [i32; MAX_BANDS],
    icc_pre: [i32; MAX_BANDS],
    ipd_pre: [i32; MAX_BANDS],

    /// The quantizer indices per parameter set and band.
    pub cld_idx: Idx,
    pub icc_idx: Idx,
    pub ipd_idx: Idx,

    /// `bsPhaseMode` and `bsOpdSmoothingMode` of the frame.
    pub phase_mode: bool,
    pub opd_smoothing: bool,
    /// Whether the IPD of a parameter set uses the coarse quantizer.
    pub ipd_coarse: [bool; MAX_SETS],

    smooth_mode: [u8; MAX_SETS],
    smooth_time: [u8; MAX_SETS],
    smg_stride: [u8; MAX_SETS],
    smg_data: [[bool; MAX_BANDS]; MAX_SETS],

    /// The smoothing time constant (slots) and per-band flags of each parameter set.
    pub smoothing_time: [f64; MAX_SETS],
    pub smoothing_data: [[bool; MAX_BANDS]; MAX_SETS],
    prev_smoothing_time: f64,
    prev_smoothing_data: [bool; MAX_BANDS],
}

impl Params {
    pub fn new(cfg: &UsacMps212Config, time_slots: usize, param_bands: usize) -> Self {
        Params {
            time_slots,
            param_bands,
            num_bands_ipd: usize::from(cfg.ott_bands_phase),
            high_rate: cfg.high_rate_mode,
            phase_coding: cfg.phase_coding,
            num_sets: 1,
            num_sets_prev: 1,
            param_slots: [0; MAX_SETS],
            slot_diff: [1; MAX_SETS],
            inv_slot_diff: [1.0; MAX_SETS],
            ext_frame: false,
            independency: false,
            cld: ParamData::default(),
            icc: ParamData::default(),
            ipd: ParamData::default(),
            cmp_cld: [[0; MAX_BANDS]; MAX_SETS],
            cmp_icc: [[0; MAX_BANDS]; MAX_SETS],
            cmp_ipd: [[0; MAX_BANDS]; MAX_SETS],
            cmp_cld_prev: [0; MAX_BANDS],
            cmp_icc_prev: [0; MAX_BANDS],
            cmp_ipd_prev: [0; MAX_BANDS],
            cld_pre: [0; MAX_BANDS],
            icc_pre: [0; MAX_BANDS],
            ipd_pre: [0; MAX_BANDS],
            cld_idx: [[0; MAX_BANDS]; MAX_SETS],
            icc_idx: [[0; MAX_BANDS]; MAX_SETS],
            ipd_idx: [[0; MAX_BANDS]; MAX_SETS],
            phase_mode: false,
            opd_smoothing: false,
            ipd_coarse: [false; MAX_SETS],
            smooth_mode: [0; MAX_SETS],
            smooth_time: [0; MAX_SETS],
            smg_stride: [0; MAX_SETS],
            smg_data: [[false; MAX_BANDS]; MAX_SETS],
            smoothing_time: [256.0; MAX_SETS],
            smoothing_data: [[false; MAX_BANDS]; MAX_SETS],
            prev_smoothing_time: 0.0,
            prev_smoothing_data: [false; MAX_BANDS],
        }
    }

    /// Parse the `Mps212Data()` of a frame.
    pub fn parse(&mut self, bs: &mut BitReaderLtr<'_>, usac_independency: bool) -> Result<()> {
        self.num_sets_prev = self.num_sets;

        let frame_type;

        if self.high_rate {
            frame_type = bs.read_bool()?;
            self.num_sets = bs.read_bits_leq32(3)? as usize + 1;
        }
        else {
            frame_type = false;
            self.num_sets = 1;
        }

        let ts = self.time_slots as i32;
        let nps = self.num_sets as i32;

        if frame_type {
            let bits = 32 - ((self.time_slots as u32) - 1).leading_zeros();
            let mut prev = -1;

            for i in 0..self.num_sets {
                let slot = bs.read_bits_leq32(bits)? as i32;

                if prev >= slot || slot >= ts {
                    return decode_error("aac: usac mps212 invalid parameter slot");
                }

                self.param_slots[i] = slot;
                prev = slot;
            }
        }
        else {
            for i in 0..self.num_sets {
                self.param_slots[i] = (ts * (i as i32 + 1) + nps - 1) / nps - 1;
            }
        }

        self.independency = if usac_independency { true } else { bs.read_bool()? };

        self.ec_data(bs, ParamType::Cld)?;
        self.ec_data(bs, ParamType::Icc)?;

        if self.phase_coding {
            self.phase_mode = bs.read_bool()?;

            if self.phase_mode {
                self.opd_smoothing = bs.read_bool()?;
                self.ec_data(bs, ParamType::Ipd)?;
            }
            else {
                self.clear_ipd();
            }
        }
        else {
            self.phase_mode = false;
            self.clear_ipd();
        }

        if self.high_rate {
            for ps in 0..self.num_sets {
                self.smooth_mode[ps] = bs.read_bits_leq32(2)? as u8;

                if self.smooth_mode[ps] >= 2 {
                    self.smooth_time[ps] = bs.read_bits_leq32(2)? as u8;
                }

                if self.smooth_mode[ps] == 3 {
                    self.smg_stride[ps] = bs.read_bits_leq32(2)? as u8;
                    let groups =
                        (self.param_bands - 1) / STRIDE[usize::from(self.smg_stride[ps])] + 1;

                    for pg in 0..groups {
                        self.smg_data[ps][pg] = bs.read_bool()?;
                    }
                }
            }
        }
        else {
            self.smooth_mode[..self.num_sets].fill(0);
        }

        Ok(())
    }

    fn clear_ipd(&mut self) {
        self.cmp_ipd_prev[..self.num_bands_ipd].fill(0);
        self.ipd_pre[..self.num_bands_ipd].fill(0);

        for set in &mut self.cmp_ipd {
            set[..self.num_bands_ipd].fill(0);
        }

        for set in &mut self.ipd_idx {
            set[..self.num_bands_ipd].fill(0);
        }

        self.opd_smoothing = false;
    }

    /// `ixheaacd_mps_ecdata_decoding`.
    fn ec_data(&mut self, bs: &mut BitReaderLtr<'_>, ty: ParamType) -> Result<()> {
        let band_stop = if ty == ParamType::Ipd { self.num_bands_ipd } else { self.param_bands };
        let num_sets = self.num_sets;
        let independency = self.independency;

        let (data, cmp, last) = match ty {
            ParamType::Cld => (&mut self.cld, &mut self.cmp_cld, &mut self.cmp_cld_prev),
            ParamType::Icc => (&mut self.icc, &mut self.cmp_icc, &mut self.cmp_icc_prev),
            ParamType::Ipd => (&mut self.ipd, &mut self.cmp_ipd, &mut self.cmp_ipd_prev),
        };

        for i in 0..num_sets {
            data.mode[i] = bs.read_bits_leq32(2)? as u8;
        }

        let mut set_index = 0;
        let mut pair = false;
        let mut old_coarse = data.coarse_prev;

        for i in 0..num_sets {
            if data.mode[i] == 0 {
                last[..band_stop].fill(0);
                old_coarse = false;
            }

            if data.mode[i] != 3 {
                continue;
            }

            if pair {
                pair = false;
                continue;
            }

            pair = bs.read_bool()?;
            data.coarse[set_index] = bs.read_bool()?;
            data.stride[set_index] = bs.read_bits_leq32(2)? as u8;

            let coarse = data.coarse[set_index];

            if coarse != old_coarse {
                if old_coarse {
                    coarse_to_fine(&mut last[..band_stop], ty);
                }
                else {
                    for v in &mut last[..band_stop] {
                        *v /= 2;
                    }
                }
            }

            let mut strides = [0usize; MAX_BANDS + 1];
            let stride = STRIDE[usize::from(data.stride[set_index])];
            let data_bands = stride_map(stride, band_stop, &mut strides);

            for pb in 0..data_bands {
                last[pb] = last[strides[pb]];
            }

            let history = *last;
            let req = PairRequest {
                ty,
                history: &history,
                data_bands,
                pair,
                coarse,
                diff_time_back: !(independency && i == 0) || set_index > 0,
            };

            let mut out = [[0i32; MAX_BANDS]; 2];
            ec_data_pair_dec(bs, &req, &mut out)?;

            cmp[set_index][..data_bands].copy_from_slice(&out[0][..data_bands]);

            if pair {
                cmp[set_index + 1][..data_bands].copy_from_slice(&out[1][..data_bands]);
            }

            let last_set = usize::from(pair);

            for pb in 0..data_bands {
                for j in strides[pb]..strides[pb + 1] {
                    let v = out[last_set][pb];

                    last[j] =
                        if ty == ParamType::Ipd { if coarse { v & 7 } else { v & 15 } } else { v };
                }
            }

            old_coarse = coarse;

            if pair {
                data.coarse[set_index + 1] = coarse;
                data.stride[set_index + 1] = data.stride[set_index];
            }

            set_index += usize::from(pair) + 1;
        }

        Ok(())
    }

    /// `ixheaacd_mps_frame_decode`: map the coded data to indices and derive the smoothing
    /// parameters and slot spans of the parameter sets.
    pub fn decode(&mut self) -> Result<()> {
        self.ext_frame = self.param_slots[self.num_sets - 1] != self.time_slots as i32 - 1;

        let nps = self.num_sets;
        let ext = self.ext_frame;
        let bands = self.param_bands;
        let slots = self.param_slots;
        let ts = self.time_slots as i32;

        map_index_data(
            ParamType::Cld,
            &mut self.cld,
            &self.cmp_cld,
            &mut self.cld_idx,
            &mut self.cld_pre,
            nps,
            ext,
            bands,
            &slots,
        )?;
        map_index_data(
            ParamType::Icc,
            &mut self.icc,
            &self.cmp_icc,
            &mut self.icc_idx,
            &mut self.icc_pre,
            nps,
            ext,
            bands,
            &slots,
        )?;

        if self.phase_coding {
            map_index_data(
                ParamType::Ipd,
                &mut self.ipd,
                &self.cmp_ipd,
                &mut self.ipd_idx,
                &mut self.ipd_pre,
                nps,
                ext,
                bands,
                &slots,
            )?;
        }

        for i in 0..nps {
            self.ipd_coarse[i] = self.ipd.coarse[i];
        }

        self.smoothing_params();

        if ext {
            self.num_sets += 1;
            self.param_slots[self.num_sets - 1] = ts - 1;
        }

        self.slot_diff[0] = self.param_slots[0] + 1;

        if self.slot_diff[0] > 72 {
            return decode_error("aac: usac mps212 invalid parameter slot");
        }

        self.inv_slot_diff[0] = 1.0 / f64::from(self.slot_diff[0]);

        for i in 1..self.num_sets {
            self.slot_diff[i] = self.param_slots[i] - self.param_slots[i - 1];

            if self.slot_diff[i] <= 0 || self.slot_diff[0] + self.slot_diff[i] > 72 {
                return decode_error("aac: usac mps212 invalid parameter slot");
            }

            self.inv_slot_diff[i] = 1.0 / f64::from(self.slot_diff[i]);
        }

        Ok(())
    }

    /// `ixheaacd_mps_dec_and_mapframesmg`.
    fn smoothing_params(&mut self) {
        let nps = self.num_sets;
        let bands = self.param_bands;

        for ps in 0..nps {
            match self.smooth_mode[ps] {
                0 => {
                    self.smoothing_time[ps] = 256.0;
                    self.smoothing_data[ps][..bands].fill(false);
                }
                1 => {
                    if ps > 0 {
                        self.smoothing_time[ps] = self.smoothing_time[ps - 1];
                        self.smoothing_data[ps] = self.smoothing_data[ps - 1];
                    }
                    else {
                        self.smoothing_time[ps] = self.prev_smoothing_time;
                        self.smoothing_data[ps] = self.prev_smoothing_data;
                    }
                }
                2 => {
                    self.smoothing_time[ps] = SMOOTHING_TIME[usize::from(self.smooth_time[ps])];
                    self.smoothing_data[ps][..bands].fill(true);
                }
                _ => {
                    self.smoothing_time[ps] = SMOOTHING_TIME[usize::from(self.smooth_time[ps])];

                    let stride = STRIDE[usize::from(self.smg_stride[ps])];
                    let groups = (bands - 1) / stride + 1;
                    let mut map = [0usize; MAX_BANDS + 1];
                    create_mapping(&mut map, 0, bands, stride);

                    for pg in 0..groups {
                        for pb in map[pg]..map[pg + 1] {
                            self.smoothing_data[ps][pb] = self.smg_data[ps][pg];
                        }
                    }
                }
            }
        }

        self.prev_smoothing_time = self.smoothing_time[nps - 1];
        self.prev_smoothing_data = self.smoothing_data[nps - 1];

        if self.ext_frame {
            self.smoothing_time[nps] = self.smoothing_time[nps - 1];
            self.smoothing_data[nps] = self.smoothing_data[nps - 1];
        }
    }
}

/// `ixheaacd_mps_coarse2fine`.
fn coarse_to_fine(data: &mut [i32], ty: ParamType) {
    for v in data.iter_mut() {
        *v <<= 1;

        if ty == ParamType::Cld {
            if *v == -14 {
                *v = -15;
            }
            else if *v == 14 {
                *v = 15;
            }
        }
    }
}

/// `ixheaacd_mps_getstridemap`: the band edges of the groups of `stride` bands; returns the
/// number of groups.
fn stride_map(stride: usize, band_stop: usize, strides: &mut [usize; MAX_BANDS + 1]) -> usize {
    let data_bands = (band_stop - 1) / stride + 1;

    strides[0] = 0;

    for pb in 1..=data_bands {
        strides[pb] = strides[pb - 1] + stride;
    }

    let mut start_offset = 0;

    while strides[data_bands] > band_stop {
        if start_offset < data_bands {
            start_offset += 1;
        }
        else {
            start_offset = 1;
        }

        for s in &mut strides[start_offset..=data_bands] {
            *s -= 1;
        }
    }

    data_bands
}

/// `ixheaacd_mps_createmapping`: the band edges of the groups the bands `[start, stop)` are
/// divided into when the coded resolution is `ch_fac` bands per value.
fn create_mapping(map: &mut [usize; MAX_BANDS + 1], start: usize, stop: usize, ch_fac: usize) {
    let input_bands = (stop - start) as i32;
    let ch_fac = ch_fac as i32;
    let out_bands = ((input_bands - 1) / ch_fac + 1).max(1);

    let mut vdk = [ch_fac; MAX_BANDS + 1];
    let mut bands_diff = input_bands - out_bands * ch_fac;

    let (incr, mut k) = if bands_diff > 0 { (-1, out_bands - 1) } else { (1, 0) };

    while bands_diff != 0 {
        vdk[k as usize] -= incr;
        k += incr;
        bands_diff += incr;

        if k >= out_bands {
            if bands_diff > 0 {
                k = out_bands - 1;
            }
            else if bands_diff < 0 {
                k = 0;
            }
        }
    }

    map[0] = start;

    for i in 0..out_bands as usize {
        map[i + 1] = map[i] + vdk[i] as usize;
    }
}

/// `ixheaacd_mps_mapindexdata` without the dequantization: expand the coded values to every
/// band and parameter set, interpolate the sets that are interpolated, and clamp.
#[allow(clippy::too_many_arguments)]
fn map_index_data(
    ty: ParamType,
    data: &mut ParamData,
    cmp: &Idx,
    out: &mut Idx,
    prev: &mut [i32; MAX_BANDS],
    nps: usize,
    ext: bool,
    band_stop: usize,
    param_slots: &[i32; MAX_SETS],
) -> Result<()> {
    let mut interpolate = [false; MAX_SETS + 1];
    let mut coarse_flag = [false; MAX_SETS];
    let mut set_index = 0;

    for i in 0..nps {
        match data.mode[i] {
            0 => {
                out[i][..band_stop].fill(0);
                prev[..band_stop].fill(0);
                data.coarse_prev = false;
                coarse_flag[i] = false;
            }
            1 | 2 => {
                out[i][..band_stop].copy_from_slice(&prev[..band_stop]);
                coarse_flag[i] = data.coarse_prev;
                interpolate[i] = data.mode[i] == 2;
            }
            _ => {
                let ch_fac = STRIDE[usize::from(data.stride[set_index])];
                let data_bands = (band_stop - 1) / ch_fac + 1;
                let mut map = [0usize; MAX_BANDS + 1];
                create_mapping(&mut map, 0, band_stop, ch_fac);

                for (g, v) in cmp[set_index][..data_bands].iter().enumerate() {
                    for b in map[g]..map[g + 1] {
                        out[i][b] = *v;
                    }
                }

                prev[..band_stop].copy_from_slice(&out[i][..band_stop]);

                data.coarse_prev = data.coarse[set_index];
                coarse_flag[i] = data.coarse[set_index];
                set_index += 1;
            }
        }
    }

    for i in 0..nps {
        if coarse_flag[i] {
            coarse_to_fine(&mut out[i][..band_stop], ty);
        }
    }

    // Interpolate the sets between their neighbours.
    let mut i1: i32 = -1;

    for i in 0..nps {
        if !interpolate[i] {
            i1 = i as i32;
        }

        let mut i2 = i;

        while interpolate[i2] {
            i2 += 1;
        }

        let (i1u, x1) = if i1 < 0 { (0, 0) } else { (i1 as usize, param_slots[i1 as usize]) };
        let xi = param_slots[i];

        if !interpolate[i] {
            continue;
        }

        if i2 >= nps {
            return decode_error("aac: usac mps212 interpolation without a target");
        }

        let x2 = param_slots[i2];

        for band in 0..band_stop {
            let mut y1 = out[i1u][band];
            let mut y2 = out[i2][band];

            let yi = if ty == ParamType::Ipd {
                if y2 - y1 > 8 {
                    y1 += 16;
                }

                if y1 - y2 > 8 {
                    y2 += 16;
                }

                if x2 != x1 { (y1 + (xi - x1) * (y2 - y1) / (x2 - x1)) % 16 } else { 0 }
            }
            else if x2 != x1 {
                y1 + (xi - x1) * (y2 - y1) / (x2 - x1)
            }
            else {
                0
            };

            out[i][band] = yi;
        }
    }

    for set in out.iter_mut().take(nps) {
        for v in &mut set[..band_stop] {
            match ty {
                ParamType::Cld => *v = (*v).clamp(-15, 15),
                ParamType::Icc => *v = (*v).clamp(0, 7),
                ParamType::Ipd => {}
            }
        }
    }

    if ext {
        out[nps] = out[nps - 1];
    }

    Ok(())
}
