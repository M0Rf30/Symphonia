// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The linear-prediction-domain coder of USAC (ISO/IEC 23003-3 clause 7.14 to 7.16): ACELP and
//! TCX frames.
//!
//! Derived from libxaac (`ixheaacd_lpd*.c`, `ixheaacd_acelp_*.c`, `ixheaacd_tcx_*.c`,
//! Apache-2.0); see the `NOTICE` of the crate.

mod acelp;
mod avq;
mod dct4;
mod decoder;
mod lpc;
mod tables;

use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};

use super::arith::ArithState;

pub use decoder::LpdDecoder;

/// The number of samples of an ACELP sub-frame.
const LEN_SUBFR: usize = acelp::LEN_SUBFR;

/// The longest pitch lag.
const MAX_PITCH: usize = 411;

/// The samples of the history of the adaptive codebook.
const EXC_HIST: usize = MAX_PITCH + acelp::INTER_LP_FIL_ORDER + 1;

/// Pre-emphasis factor.
const PREEMPH: f32 = 0.68;

/// The length of the frame of the long window set: the superframe is always this long.
const LEN_SUPERFRAME: usize = 1024;

/// The parsed `lpd_channel_stream()` of a frame.
pub struct LpdFrame {
    pub acelp_core_mode: usize,
    pub mod_: [i32; 4],
    pub bpf_control_info: bool,
    pub core_mode_last: bool,
    pub fac_data_present: bool,
    pub first_lpd_flag: bool,
    pub short_fac_flag: bool,
    /// The FAC data of each sub-frame set (`fac_length` values each).
    pub fac: [[i32; 128]; 4],
    pub mean_energy: [i32; 4],
    pub acb_index: [i32; 16],
    pub ltp_filtering_flag: [bool; 16],
    pub icb_index: [[i32; 8]; 16],
    pub gains: [i32; 16],
    pub noise_factor: [i32; 4],
    pub global_gain: [i32; 4],
    pub tcx_lg: [usize; 4],
    /// The quantised TCX spectra, one after the other.
    pub tcx: Vec<i32>,
    lpc_idx: [i32; 192],
    lpc_len: usize,
    /// The gain index followed by the FAC data of the first frame of an LPD run.
    pub fac_data: [i32; 129],
}

impl LpdFrame {
    pub fn new() -> Self {
        LpdFrame {
            acelp_core_mode: 0,
            mod_: [0; 4],
            bpf_control_info: false,
            core_mode_last: false,
            fac_data_present: false,
            first_lpd_flag: false,
            short_fac_flag: false,
            fac: [[0; 128]; 4],
            mean_energy: [0; 4],
            acb_index: [0; 16],
            ltp_filtering_flag: [false; 16],
            icb_index: [[0; 8]; 16],
            gains: [0; 16],
            noise_factor: [0; 4],
            global_gain: [0; 4],
            tcx_lg: [0; 4],
            tcx: vec![0; LEN_SUPERFRAME],
            lpc_idx: [0; 192],
            lpc_len: 0,
            fac_data: [0; 129],
        }
    }

    fn push_lpc(&mut self, v: i32) -> Result<()> {
        if self.lpc_len >= self.lpc_idx.len() {
            return decode_error("aac: usac lpc data overflow");
        }
        self.lpc_idx[self.lpc_len] = v;
        self.lpc_len += 1;
        Ok(())
    }
}

/// Read the `fac_data()` of `out.len()` values (`fac_decoding`).
pub fn read_fac(bs: &mut BitReaderLtr<'_>, out: &mut [i32]) -> Result<()> {
    for chunk in out.chunks_mut(8) {
        let mut qn = 0;

        while qn <= 36 && bs.read_bool()? {
            qn += 1;
        }

        if qn > 0 {
            qn += 1;
        }

        let (mut n, mut nk) = (qn, 0);

        if qn > 4 {
            nk = (qn - 3) >> 1;
            n = qn - nk * 2;
        }

        let idx = if n > 0 { bs.read_bits_leq32(4 * n as u32)? } else { 0 };
        let mut kv = [0i32; 8];

        if nk > 0 {
            for v in &mut kv {
                *v = bs.read_bits_leq32(nk as u32)? as i32;
            }
        }

        let b = avq::rotated_gosset_mtx_dec(qn, idx, &kv);
        chunk.copy_from_slice(&b[..chunk.len()]);
    }

    Ok(())
}

/// The first bit of a number of bits of the `lpc_data()` quantiser indexes.
fn read_unary(bs: &mut BitReaderLtr<'_>, max: u32) -> Result<u32> {
    let mut v = 0;
    while v < max && bs.read_bool()? {
        v += 1;
    }
    Ok(v)
}

impl LpdFrame {
    /// Parse `lpd_channel_stream()` (ISO/IEC 23003-3 Table 25 and libxaac
    /// `ixheaacd_lpd_channel_stream`).
    ///
    /// `last_lpd_mode` is the mode of the last sub-frame of the previous frame (-1 or -2 when
    /// the previous frame was frequency-domain coded).
    pub fn parse(
        &mut self,
        bs: &mut BitReaderLtr<'_>,
        arith: &mut ArithState,
        ccfl: usize,
        indep_flag: bool,
        last_lpd_mode: i32,
    ) -> Result<()> {
        let len_subfrm = ccfl / 4;
        let num_subfrm = 4 * ccfl / 1024;

        self.acelp_core_mode = bs.read_bits_leq32(3)? as usize;
        let lpd_mode = bs.read_bits_leq32(5)?;
        self.bpf_control_info = bs.read_bool()?;
        self.core_mode_last = bs.read_bool()?;
        self.fac_data_present = bs.read_bool()?;

        self.mod_ = match lpd_mode {
            25 => [3; 4],
            24 => [2; 4],
            20..=23 => [(lpd_mode & 1) as i32, ((lpd_mode >> 1) & 1) as i32, 2, 2],
            16..=19 => [2, 2, (lpd_mode & 1) as i32, ((lpd_mode >> 1) & 1) as i32],
            0..=15 => [
                (lpd_mode & 1) as i32,
                ((lpd_mode >> 1) & 1) as i32,
                ((lpd_mode >> 2) & 1) as i32,
                ((lpd_mode >> 3) & 1) as i32,
            ],
            _ => return decode_error("aac: usac invalid lpd_mode"),
        };

        self.first_lpd_flag = !self.core_mode_last;
        self.lpc_len = 0;
        self.tcx_lg = [0; 4];

        let mut last_mode = last_lpd_mode;
        let mut first_tcx = true;
        let mut tcx_pos = 0;
        let fac_len = len_subfrm / 2;

        let mut k = 0;

        while k < 4 {
            let m = self.mod_[k];

            let read_fac_now = if k == 0 {
                self.core_mode_last && self.fac_data_present
            }
            else {
                (last_mode == 0 && m > 0) || (last_mode > 0 && m == 0)
            };

            if read_fac_now {
                let mut tmp = [0i32; 128];
                read_fac(bs, &mut tmp[..fac_len])?;
                self.fac[k] = tmp;
            }

            if m == 0 {
                self.mean_energy[k] = bs.read_bits_leq32(2)? as i32;

                for sfr in 0..num_subfrm {
                    let i = k * 4 + sfr;
                    let bits = if sfr == 0 || (num_subfrm == 4 && sfr == 2) { 9 } else { 6 };
                    self.acb_index[i] = bs.read_bits_leq32(bits)? as i32;
                    self.ltp_filtering_flag[i] = bs.read_bool()?;

                    if self.acelp_core_mode == 5 {
                        for j in 0..4 {
                            self.icb_index[i][j] = bs.read_bits_leq32(2)? as i32;
                        }
                        for j in 4..8 {
                            self.icb_index[i][j] = bs.read_bits_leq32(14)? as i32;
                        }
                    }
                    else {
                        let bits = &acelp::NUM_BITS_CELP_CODING[self.acelp_core_mode];
                        for j in 0..4 {
                            self.icb_index[i][j] = bs.read_bits_leq32(bits[j])? as i32;
                        }
                    }

                    self.gains[i] = bs.read_bits_leq32(7)? as i32;
                }

                last_mode = 0;
                k += 1;
            }
            else {
                let lg = len_subfrm << (m - 1);
                self.tcx_lg[k] = lg;

                self.noise_factor[k] = bs.read_bits_leq32(3)? as i32;
                self.global_gain[k] = bs.read_bits_leq32(7)? as i32;

                let mut reset = false;

                if first_tcx {
                    reset = if indep_flag { true } else { bs.read_bool()? };
                }

                if tcx_pos + lg > self.tcx.len() {
                    return decode_error("aac: usac tcx overflow");
                }

                arith.decode_spectrum(
                    bs,
                    first_tcx && reset,
                    lg,
                    lg,
                    &mut self.tcx[tcx_pos..tcx_pos + lg],
                )?;

                tcx_pos += lg;
                first_tcx = false;
                last_mode = m;
                k += 1 << (m - 1);
            }
        }

        self.parse_lpc_data(bs)?;

        // The FAC data of the first LPD frame after a frequency-domain frame.
        self.short_fac_flag = false;

        if !self.core_mode_last && self.fac_data_present {
            self.short_fac_flag = bs.read_bool()?;
            let fac_length = if self.short_fac_flag { ccfl / 16 } else { ccfl / 8 };

            let gain = bs.read_bits_leq32(7)? as i32;

            if gain > 128 {
                return decode_error("aac: usac invalid fac gain");
            }

            self.fac_data[0] = gain;

            let mut tmp = [0i32; 128];
            read_fac(bs, &mut tmp[..fac_length])?;
            self.fac_data[1..1 + fac_length].copy_from_slice(&tmp[..fac_length]);
        }
        else if !self.fac_data_present {
            self.fac_data = [0; 129];
        }

        Ok(())
    }

    /// `lpc_data()`: the quantiser indexes of the LPC sets (libxaac `ixheaacd_lpc_data`).
    fn parse_lpc_data(&mut self, bs: &mut BitReaderLtr<'_>) -> Result<()> {
        let mut nk_mode = 0;

        self.push_lpc(bs.read_bits_leq32(8)? as i32)?;
        self.code_book_indices(bs, nk_mode)?;

        if self.first_lpd_flag {
            let mode = Self::mode_lpc(bs, 0, &mut nk_mode)?;
            self.push_lpc(mode)?;

            if mode == 0 {
                self.push_lpc(bs.read_bits_leq32(8)? as i32)?;
            }

            self.code_book_indices(bs, nk_mode)?;
        }

        if self.mod_[0] < 3 {
            let mode = Self::mode_lpc(bs, 2, &mut nk_mode)?;
            self.push_lpc(mode)?;

            if mode == 0 {
                self.push_lpc(bs.read_bits_leq32(8)? as i32)?;
            }

            self.code_book_indices(bs, nk_mode)?;
        }

        if self.mod_[0] < 2 {
            let mode = Self::mode_lpc(bs, 1, &mut nk_mode)?;
            self.push_lpc(mode)?;

            if mode == 0 {
                self.push_lpc(bs.read_bits_leq32(8)? as i32)?;
            }

            if mode != 1 {
                self.code_book_indices(bs, nk_mode)?;
            }
        }

        if self.mod_[2] < 2 {
            let mode = Self::mode_lpc(bs, 3, &mut nk_mode)?;
            self.push_lpc(mode)?;

            if mode == 0 {
                self.push_lpc(bs.read_bits_leq32(8)? as i32)?;
            }

            self.code_book_indices(bs, nk_mode)?;
        }

        Ok(())
    }

    fn mode_lpc(bs: &mut BitReaderLtr<'_>, set: u32, nk_mode: &mut i32) -> Result<i32> {
        let mut mode = 0;

        match set {
            0 | 2 => {
                mode = i32::from(bs.read_bool()?);
                if mode == 1 {
                    *nk_mode = 3;
                }
            }
            1 => {
                if !bs.read_bool()? {
                    mode = 2;
                    *nk_mode = 2;
                }
                else if !bs.read_bool()? {
                    mode = 0;
                    *nk_mode = 0;
                }
                else {
                    mode = 1;
                    *nk_mode = 1;
                }
            }
            3 => {
                if !bs.read_bool()? {
                    mode = 1;
                    *nk_mode = 1;
                }
                else if !bs.read_bool()? {
                    mode = 0;
                    *nk_mode = 0;
                }
                else {
                    mode = if !bs.read_bool()? { 2 } else { 3 };
                    *nk_mode = 2;
                }
            }
            _ => {}
        }

        Ok(mode)
    }

    fn code_book_indices(&mut self, bs: &mut BitReaderLtr<'_>, nk_mode: i32) -> Result<()> {
        let mut qn = [0i32; 2];

        match nk_mode {
            1 => {
                for q in &mut qn {
                    *q = read_unary(bs, 64)? as i32;
                    if *q > 0 {
                        *q += 1;
                    }
                }
            }
            _ => {
                for q in &mut qn {
                    *q = 2 + bs.read_bits_leq32(2)? as i32;
                }

                if nk_mode == 2 {
                    for q in &mut qn {
                        if *q > 4 {
                            *q = read_unary(bs, 64)? as i32;
                            if *q > 0 {
                                *q += 4;
                            }
                        }
                    }
                }
                else {
                    for q in &mut qn {
                        if *q > 4 {
                            let ext = read_unary(bs, 64)? as i32;
                            *q = match ext {
                                0 => 5,
                                1 => 6,
                                2 => 0,
                                _ => ext + 4,
                            };
                        }
                    }
                }
            }
        }

        self.push_lpc(qn[0])?;
        self.push_lpc(qn[1])?;

        for q in qn {
            if q > 0 {
                let (nk, n) = if q > 4 { ((q - 3) / 2, q - ((q - 3) / 2) * 2) } else { (0, q) };

                self.push_lpc(bs.read_bits_leq32(4 * n as u32)? as i32)?;

                for _ in 0..8 {
                    let v = if nk > 0 { bs.read_bits_leq32(nk as u32)? as i32 } else { 0 };
                    self.push_lpc(v)?;
                }
            }
        }

        Ok(())
    }
}
