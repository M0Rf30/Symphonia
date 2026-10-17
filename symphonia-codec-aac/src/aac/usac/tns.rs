// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Temporal Noise Shaping of the USAC frequency-domain coder (ISO/IEC 23003-3 7.3.3.3).
//!
//! USAC reuses the AAC TNS tool (`tns_data()`), but the filter order of a long window is coded
//! with 4 bits (not 5) and there is no upper bound on it, the number of bands the tool may
//! span is the USAC `tns_max_bands`, and the short-window stride is `ccfl / 8` rather than 128.

use symphonia_core::errors::Result;
use symphonia_core::io::ReadBitsLtr;

use std::f32::consts;

const TNS_MAX_ORDER: usize = 15;
const MAX_WINDOWS: usize = 8;

/// A single TNS filter: the coded band length, order, direction and the LPC coefficients.
#[derive(Copy, Clone)]
struct TnsFilter {
    length: usize,
    order: usize,
    direction: bool,
    lpc: [f32; TNS_MAX_ORDER + 1],
}

impl TnsFilter {
    fn new() -> Self {
        TnsFilter { length: 0, order: 0, direction: false, lpc: [0.0; TNS_MAX_ORDER + 1] }
    }

    fn read<B: ReadBitsLtr>(&mut self, bs: &mut B, long_win: bool, coef_res: bool) -> Result<()> {
        // ISO/IEC 23003-3 7.3.3.3: the filter length is 6 bits in a long window, 4 in a short
        // one, and the order is 4 bits (long) or 3 bits (short).
        self.length = bs.read_bits_leq32(if long_win { 6 } else { 4 })? as usize;
        self.order = bs.read_bits_leq32(if long_win { 4 } else { 3 })? as usize;

        if self.order > 0 {
            self.direction = bs.read_bool()?;

            let coef_compress = bs.read_bool()?;

            // ISO/IEC 14496-3 4.6.9.2: `coef_res` selects a 4-bit (true) or 3-bit (false)
            // coefficient; `coef_compress` drops the most significant bit.
            let mut coef_res_bits = if coef_res { 4 } else { 3 };
            if coef_compress {
                coef_res_bits -= 1;
            }

            let sign_mask = 1 << (coef_res_bits - 1);
            let neg_mask = !((1 << coef_res_bits) - 1);

            // `1 << (coef_res_bits - 1)` before compression (ISO/IEC 14496-3 4.6.9.2).
            let fac_base = if coef_res { 8.0 } else { 4.0 };

            let iqfac = (fac_base - 0.5) / consts::FRAC_PI_2;
            let iqfac_m = (fac_base + 0.5) / consts::FRAC_PI_2;

            let mut tmp: [f32; TNS_MAX_ORDER] = [0.0; TNS_MAX_ORDER];

            for el in tmp[..self.order].iter_mut() {
                let val = bs.read_bits_leq32(coef_res_bits)? as u8;

                // Convert to a signed integer.
                let c = if (val & sign_mask) != 0 {
                    f32::from((val | neg_mask) as i8)
                }
                else {
                    f32::from(val as i8)
                };

                *el = (if c >= 0.0 { c / iqfac } else { c / iqfac_m }).sin();
            }

            // The step-up recursion: convert the reflection coefficients into the LPC
            // coefficients of the all-pole synthesis filter.
            let mut b: [f32; TNS_MAX_ORDER + 1] = [0.0; TNS_MAX_ORDER + 1];

            for m in 1..=self.order {
                for i in 1..m {
                    b[i] = self.lpc[i - 1] + tmp[m - 1] * self.lpc[m - i - 1];
                }

                self.lpc[..(m - 1)].copy_from_slice(&b[1..m]);
                self.lpc[m - 1] = tmp[m - 1];
            }
        }

        Ok(())
    }
}

/// The per-channel TNS data of one frame.
#[derive(Copy, Clone)]
pub struct Tns {
    n_filt: [usize; MAX_WINDOWS],
    filter: [[TnsFilter; 4]; MAX_WINDOWS],
    present: bool,
}

impl Tns {
    pub fn new() -> Self {
        Tns {
            n_filt: [0; MAX_WINDOWS],
            filter: [[TnsFilter::new(); 4]; MAX_WINDOWS],
            present: false,
        }
    }

    pub fn reset(&mut self) {
        *self = Tns::new();
    }

    pub fn set_present(&mut self) {
        self.present = true;
    }

    /// Read the `tns_data()` of a channel (ISO/IEC 23003-3 7.3.3.3).
    pub fn read_data<B: ReadBitsLtr>(
        &mut self,
        bs: &mut B,
        num_windows: usize,
        is8: bool,
    ) -> Result<()> {
        for w in 0..num_windows {
            self.n_filt[w] = bs.read_bits_leq32(if is8 { 1 } else { 2 })? as usize;

            let coef_res = if self.n_filt[w] != 0 { bs.read_bool()? } else { false };

            for filt in 0..self.n_filt[w] {
                self.filter[w][filt].read(bs, !is8, coef_res)?;
            }
        }

        Ok(())
    }

    /// Apply the all-pole synthesis filters to the dequantized spectrum of `num_windows`
    /// windows of `stride` samples each (`stride` is `ccfl / 8` for a short window, `ccfl` for
    /// a long window, which has a single window).
    pub fn synth(
        &self,
        swb_offset: &[u16],
        max_sfb: usize,
        tns_max_bands: usize,
        num_windows: usize,
        stride: usize,
        coeffs: &mut [f32],
    ) {
        if !self.present {
            return;
        }

        let mmm = tns_max_bands.min(max_sfb);

        for w in 0..num_windows {
            let mut bottom = swb_offset.len() - 1;

            for f in 0..self.n_filt[w] {
                let top = bottom;

                bottom = top.saturating_sub(self.filter[w][f].length);

                let order = self.filter[w][f].order;

                if order == 0 {
                    continue;
                }

                let start = w * stride + swb_offset[bottom.min(mmm)] as usize;
                let end = w * stride + swb_offset[top.min(mmm)] as usize;

                let lpc = &self.filter[w][f].lpc;

                if !self.filter[w][f].direction {
                    for (m, i) in (start..end).enumerate() {
                        for j in 0..order.min(m) {
                            coeffs[i] -= coeffs[i - j - 1] * lpc[j];
                        }
                    }
                }
                else {
                    for (m, i) in (start..end).rev().enumerate() {
                        for j in 0..order.min(m) {
                            coeffs[i] -= coeffs[i + j + 1] * lpc[j];
                        }
                    }
                }
            }
        }
    }
}
