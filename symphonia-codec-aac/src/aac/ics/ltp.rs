// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// Previous Author: Kostya Shishkov <kostya.shiskov@gmail.com>
//
// This source file includes code originally written for the NihAV
// project. With the author's permission, it has been relicensed for,
// and ported to the Symphonia project.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::io::ReadBitsLtr;

use symphonia_core::errors::Result;

/// The maximum number of scale factor bands that LTP can be applied to (ISO/IEC 14496-3
/// `MAX_LTP_LONG_SFB`).
pub const MAX_LTP_SFB: usize = 40;

/// The maximum of `PRED_SFB_MAX` over the sampling frequencies (ISO/IEC 14496-3 table 4.?).
pub const MAX_PRED_SFB: usize = 41;

/// The number of bands that the main profile prediction applies to by sampling frequency index
/// (`PRED_SFB_MAX`, as in FFmpeg's `ff_aac_pred_sfb_max`).
const PRED_SFB_MAX: [usize; 12] = [33, 33, 38, 40, 40, 40, 41, 41, 37, 37, 37, 34];

/// The LTP gains by `ltp_coef` (ISO/IEC 14496-3 table 4.?, `codebook_ltp`).
const LTP_COEF: [f32; 8] =
    [0.570829, 0.696616, 0.813004, 0.911304, 0.984900, 1.067894, 1.194601, 1.369533];

/// The `ltp_data()` of a channel: the lag of the prediction in the output of the filterbank and
/// the bands it is used in.
#[derive(Clone, Copy)]
pub struct LtpData {
    pub lag: usize,
    pub coef: f32,
    pub used: [bool; MAX_LTP_SFB],
}

impl LtpData {
    pub fn read<B: ReadBitsLtr>(bs: &mut B, max_sfb: usize) -> Result<Self> {
        let lag = bs.read_bits_leq32(11)? as usize;
        let coef = LTP_COEF[bs.read_bits_leq32(3)? as usize];

        let mut used = [false; MAX_LTP_SFB];

        for u in used.iter_mut().take(max_sfb.min(MAX_LTP_SFB)) {
            *u = bs.read_bool()?;
        }

        Ok(LtpData { lag, coef, used })
    }
}

/// The prediction data of a channel of the main profile (`prediction_used` and the reset group).
#[derive(Clone, Copy)]
pub struct MainPredData {
    /// The group of predictors to reset after the frame (1..=30), or 0 for none.
    pub reset_group: u8,
    pub used: [bool; MAX_PRED_SFB],
}

impl MainPredData {
    pub fn read<B: ReadBitsLtr>(bs: &mut B, max_sfb: usize, rate_idx: usize) -> Result<Self> {
        let mut reset_group = 0;

        if bs.read_bool()? {
            reset_group = bs.read_bits_leq32(5)? as u8;

            if reset_group == 0 || reset_group > 30 {
                return symphonia_core::errors::decode_error("aac: invalid predictor reset group");
            }
        }

        let mut used = [false; MAX_PRED_SFB];

        for u in used.iter_mut().take(max_sfb.min(PRED_SFB_MAX[rate_idx.min(11)])) {
            *u = bs.read_bool()?;
        }

        Ok(MainPredData { reset_group, used })
    }

    /// The number of bands the predictors are updated in.
    pub fn num_bands(rate_idx: usize) -> usize {
        PRED_SFB_MAX[rate_idx.min(11)]
    }
}

/// The state of the backward adaptive predictor of a spectral coefficient: a second order
/// lattice LMS filter (ISO/IEC 14496-3 4.6.6.3, with FFmpeg's `predict()` for the truncation of
/// the state to 16 bit floating point).
#[derive(Clone, Copy)]
pub struct Predictor {
    r0: f32,
    r1: f32,
    cor0: f32,
    cor1: f32,
    var0: f32,
    var1: f32,
}

impl Default for Predictor {
    fn default() -> Self {
        Predictor { r0: 0.0, r1: 0.0, cor0: 0.0, cor1: 0.0, var0: 1.0, var1: 1.0 }
    }
}

/// Round to the nearest 16-bit mantissa-less float (the upper 16 bits of an f32).
fn flt16_round(x: f32) -> f32 {
    f32::from_bits(x.to_bits().wrapping_add(0x8000) & 0xffff_0000)
}

/// Round to nearest even to the upper 16 bits of an f32.
fn flt16_even(x: f32) -> f32 {
    let b = x.to_bits();
    f32::from_bits(b.wrapping_add(0x7fff + ((b >> 16) & 1)) & 0xffff_0000)
}

fn flt16_trunc(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & 0xffff_0000)
}

impl Predictor {
    /// Predict a coefficient, adding the prediction to it if `output_enable`, and update the
    /// state with the (corrected) coefficient.
    pub fn predict(&mut self, coef: &mut f32, output_enable: bool) {
        const A: f32 = 0.953125; // 61 / 64
        const ALPHA: f32 = 0.90625; // 29 / 32

        let (r0, r1) = (self.r0, self.r1);
        let (cor0, cor1) = (self.cor0, self.cor1);
        let (var0, var1) = (self.var0, self.var1);

        let k1 = if var0 > 1.0 { cor0 * flt16_even(A / var0) } else { 0.0 };
        let k2 = if var1 > 1.0 { cor1 * flt16_even(A / var1) } else { 0.0 };

        let pv = flt16_round(k1 * r0 + k2 * r1);

        if output_enable {
            *coef += pv;
        }

        let e0 = *coef;
        let e1 = e0 - k1 * r0;

        self.cor1 = flt16_trunc(ALPHA * cor1 + r1 * e1);
        self.var1 = flt16_trunc(ALPHA * var1 + 0.5 * (r1 * r1 + e1 * e1));
        self.cor0 = flt16_trunc(ALPHA * cor0 + r0 * e0);
        self.var0 = flt16_trunc(ALPHA * var0 + 0.5 * (r0 * r0 + e0 * e0));

        self.r1 = flt16_trunc(A * (r0 - k1 * e0));
        self.r0 = flt16_trunc(A * e0);
    }

    pub fn reset(&mut self) {
        *self = Predictor::default();
    }
}
