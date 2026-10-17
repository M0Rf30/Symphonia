// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Stereo tools of the USAC frequency-domain coder: M/S and the complex stereo prediction of
//! ISO/IEC 23003-3 7.7.

use symphonia_core::errors::Result;
use symphonia_core::io::ReadBitsLtr;

use super::fd::{EIGHT_SHORT, IcsInfo, LONG_START, LONG_STOP};
use super::tables::{MDST_FILT_CUR, MDST_FILT_PREV};
use crate::aac::codebooks;

const MAX_SFB: usize = 64;
const MAX_GROUPS: usize = 8;
const SFB_PER_PRED_BAND: usize = 2;

/// The persistent stereo-prediction state of a channel pair.
pub struct StereoState {
    pub ms_mask_mode: u8,
    pub common_window: bool,
    pub max_sfb_ste: usize,
    pred_dir: bool,
    complex_coef: bool,
    use_prev_frame: bool,
    pred_used: [bool; MAX_GROUPS * MAX_SFB],
    alpha_q_re: [f32; MAX_GROUPS * MAX_SFB],
    alpha_q_im: [f32; MAX_GROUPS * MAX_SFB],
    prev_alpha_q_re: [f32; MAX_GROUPS * MAX_SFB],
    prev_alpha_q_im: [f32; MAX_GROUPS * MAX_SFB],
    pub ms_mask: [bool; MAX_GROUPS * MAX_SFB],
    dmix_re: Vec<f32>,
    prev_dmix_re: Vec<f32>,
    dmix_im: Vec<f32>,
    prev_num_window_groups: usize,
}

impl StereoState {
    pub fn new(core_frame_len: usize) -> Self {
        StereoState {
            ms_mask_mode: 0,
            common_window: false,
            max_sfb_ste: 0,
            pred_dir: false,
            complex_coef: false,
            use_prev_frame: false,
            pred_used: [false; MAX_GROUPS * MAX_SFB],
            alpha_q_re: [0.0; MAX_GROUPS * MAX_SFB],
            alpha_q_im: [0.0; MAX_GROUPS * MAX_SFB],
            prev_alpha_q_re: [0.0; MAX_GROUPS * MAX_SFB],
            prev_alpha_q_im: [0.0; MAX_GROUPS * MAX_SFB],
            ms_mask: [false; MAX_GROUPS * MAX_SFB],
            dmix_re: vec![0.0; core_frame_len],
            prev_dmix_re: vec![0.0; core_frame_len],
            dmix_im: vec![0.0; core_frame_len],
            prev_num_window_groups: 1,
        }
    }

    pub fn reset(&mut self) {
        *self = StereoState::new(self.dmix_re.len());
    }

    /// Parse the complex stereo prediction data (ISO/IEC 23003-3 7.7.3).
    pub fn decode_complex_pred<B: ReadBitsLtr>(
        &mut self,
        bs: &mut B,
        ics: &IcsInfo,
        indep_flag: bool,
    ) -> Result<()> {
        let max_sfb_ste = self.max_sfb_ste;
        let num_window_groups = ics.num_window_groups;

        if !bs.read_bool()? {
            // cplx_pred_all == 0: per prediction band.
            for g in 0..num_window_groups {
                for sfb in (0..max_sfb_ste).step_by(SFB_PER_PRED_BAND) {
                    let val = bs.read_bool()?;
                    self.pred_used[g * max_sfb_ste + sfb] = val;
                    if sfb + 1 < max_sfb_ste {
                        self.pred_used[g * max_sfb_ste + sfb + 1] = val;
                    }
                }
            }
        }
        else {
            for g in 0..num_window_groups {
                for sfb in 0..max_sfb_ste {
                    self.pred_used[g * max_sfb_ste + sfb] = true;
                }
            }
        }

        self.pred_dir = bs.read_bool()?;
        self.complex_coef = bs.read_bool()?;

        self.use_prev_frame = false;
        if self.complex_coef && !indep_flag {
            self.use_prev_frame = bs.read_bool()?;
        }

        let delta_code_time = if !indep_flag { bs.read_bool()? } else { false };

        for g in 0..num_window_groups {
            for sfb in (0..max_sfb_ste).step_by(SFB_PER_PRED_BAND) {
                let mut last_alpha_q_re = 0.0;
                let mut last_alpha_q_im = 0.0;

                if delta_code_time {
                    if g > 0 {
                        // After the first group: the previous group of this frame.
                        last_alpha_q_re = self.alpha_q_re[(g - 1) * max_sfb_ste + sfb];
                        last_alpha_q_im = self.alpha_q_im[(g - 1) * max_sfb_ste + sfb];
                    }
                    else if ics.window_sequence == EIGHT_SHORT
                        && ics.window_sequence_prev == EIGHT_SHORT
                    {
                        // Short-to-short: the last group of the previous frame.
                        let wg = self.prev_num_window_groups - 1;
                        last_alpha_q_re = self.prev_alpha_q_re[wg * max_sfb_ste + sfb];
                        last_alpha_q_im = self.prev_alpha_q_im[wg * max_sfb_ste + sfb];
                    }
                    else {
                        last_alpha_q_re = self.prev_alpha_q_re[g * max_sfb_ste + sfb];
                        last_alpha_q_im = self.prev_alpha_q_im[g * max_sfb_ste + sfb];
                    }
                }
                else if sfb > 0 {
                    last_alpha_q_re = self.alpha_q_re[g * max_sfb_ste + sfb - 1];
                    last_alpha_q_im = self.alpha_q_im[g * max_sfb_ste + sfb - 1];
                }

                if self.pred_used[g * max_sfb_ste + sfb] {
                    let mut val = 60 - bs.read_codebook(&codebooks::SCALEFACTORS)?.0 as i32;
                    last_alpha_q_re += val as f32 * 0.1;
                    if self.complex_coef {
                        val = 60 - bs.read_codebook(&codebooks::SCALEFACTORS)?.0 as i32;
                        last_alpha_q_im += val as f32 * 0.1;
                    }
                    self.alpha_q_re[g * max_sfb_ste + sfb] = last_alpha_q_re;
                    self.alpha_q_im[g * max_sfb_ste + sfb] = last_alpha_q_im;
                }
                else {
                    self.alpha_q_re[g * max_sfb_ste + sfb] = 0.0;
                    self.alpha_q_im[g * max_sfb_ste + sfb] = 0.0;
                }

                if sfb + 1 < max_sfb_ste {
                    self.alpha_q_re[g * max_sfb_ste + sfb + 1] =
                        self.alpha_q_re[g * max_sfb_ste + sfb];
                    self.alpha_q_im[g * max_sfb_ste + sfb + 1] =
                        self.alpha_q_im[g * max_sfb_ste + sfb];
                }
            }
        }

        Ok(())
    }

    /// Zero the previous-frame alpha values (independence flag, or an unsupported transform).
    pub fn reset_prev_alphas(&mut self) {
        self.prev_alpha_q_re = [0.0; MAX_GROUPS * MAX_SFB];
        self.prev_alpha_q_im = [0.0; MAX_GROUPS * MAX_SFB];
    }

    /// Zero the current-frame alpha values, which are always zeroed before a frame is decoded.
    pub fn alpha_zero_current(&mut self) {
        self.alpha_q_re = [0.0; MAX_GROUPS * MAX_SFB];
        self.alpha_q_im = [0.0; MAX_GROUPS * MAX_SFB];
    }

    pub fn store_prev_window_groups(&mut self, num_window_groups: usize) {
        self.prev_num_window_groups = num_window_groups;
    }

    /// Save the previous-frame prediction coefficients after decoding.
    pub fn store_prev(&mut self) {
        self.prev_alpha_q_re = self.alpha_q_re;
        self.prev_alpha_q_im = self.alpha_q_im;
    }

    /// Apply M/S to the two dequantized spectra (ISO/IEC 23003-3 7.7.1).
    pub fn apply_ms(&self, ics: &IcsInfo, stride: usize, coeffs1: &mut [f32], coeffs2: &mut [f32]) {
        let mut win = 0;

        for g in 0..ics.num_window_groups {
            for _ in 0..ics.group_len[g] {
                for sfb in 0..self.max_sfb_ste {
                    if !self.ms_mask[g * self.max_sfb_ste + sfb] {
                        continue;
                    }

                    let start = win * stride + ics.swb_offset[sfb] as usize;
                    let end = win * stride + ics.swb_offset[sfb + 1] as usize;

                    for (c1, c2) in coeffs1[start..end].iter_mut().zip(&mut coeffs2[start..end]) {
                        let (m, s) = (*c1, *c2);
                        *c1 = m + s;
                        *c2 = m - s;
                    }
                }
                win += 1;
            }
        }
    }

    /// Apply the complex stereo prediction (ISO/IEC 23003-3 7.7.2) to the dequantized spectra.
    ///
    /// `prev1`/`prev2` are the final spectra of the previous frame, which form the previous
    /// downmix for the MDST estimate. Structure and filter selection follow FDK's
    /// `CJointStereo_ApplyMS()` (`stereo.cpp`).
    #[allow(clippy::too_many_arguments)]
    pub fn apply_complex_stereo(
        &mut self,
        ics: &IcsInfo,
        stride: usize,
        coeffs1: &mut [f32],
        coeffs2: &mut [f32],
        prev1: &[f32],
        prev2: &[f32],
    ) {
        let max_sfb_ste = self.max_sfb_ste;
        let sign = if self.pred_dir { -1.0 } else { 1.0 };
        let short = ics.window_sequence == EIGHT_SHORT;
        let band_end = ics.swb_offset[max_sfb_ste] as usize;
        let extended = (band_end + 7).min(stride);

        // The downmix MDCT of every window of the current frame: the (already predicted) mid
        // channel in the prediction bands, `(L +/- R) / 2` elsewhere.
        let mut win = 0;
        for g in 0..ics.num_window_groups {
            for _ in 0..ics.group_len[g] {
                let base = win * stride;
                for sfb in 0..max_sfb_ste {
                    let range = base + ics.swb_offset[sfb] as usize
                        ..base + ics.swb_offset[sfb + 1] as usize;

                    if self.pred_used[g * max_sfb_ste + sfb] {
                        self.dmix_re[range.clone()].copy_from_slice(&coeffs1[range]);
                    }
                    else {
                        for i in range {
                            self.dmix_re[i] = 0.5 * (coeffs1[i] + sign * coeffs2[i]);
                        }
                    }
                }
                self.dmix_re[base + band_end..base + stride].fill(0.0);
                win += 1;
            }
        }

        // The MDST estimate of every window.
        self.dmix_im[..ics.num_windows * stride].fill(0.0);

        if self.complex_coef {
            let shape = usize::from(ics.window_shape);

            // The downmix of the previous frame: its last window when short.
            if self.use_prev_frame {
                let off = if short { 7 * stride } else { 0 };
                for i in 0..stride {
                    self.prev_dmix_re[i] = 0.5 * (prev1[off + i] + sign * prev2[off + i]);
                }
            }

            for w in 0..ics.num_windows {
                let base = w * stride;
                let (re_cur, im) = (&self.dmix_re[base..base + stride], &mut self.dmix_im[base..]);

                if short && w != 0 {
                    complex_stereo_interpolate_imag(
                        im,
                        re_cur,
                        MDST_FILT_CUR[0][shape],
                        extended,
                        1.0,
                        1.0,
                    );
                    complex_stereo_interpolate_imag(
                        im,
                        &self.dmix_re[base - stride..base],
                        mdst_filt_prev(shape),
                        extended,
                        -1.0,
                        1.0,
                    );
                }
                else {
                    complex_stereo_interpolate_imag(
                        im,
                        re_cur,
                        complex_stereo_get_filter(ics),
                        extended,
                        1.0,
                        1.0,
                    );

                    if self.use_prev_frame {
                        complex_stereo_interpolate_imag(
                            im,
                            &self.prev_dmix_re[..stride],
                            complex_stereo_get_filter_prev(ics),
                            extended,
                            -1.0,
                            1.0,
                        );
                    }
                }
            }
        }

        // The upmix: `side = R - alpha_re * dmx_re - alpha_im * dmx_im`.
        let mut win = 0;
        for g in 0..ics.num_window_groups {
            for _ in 0..ics.group_len[g] {
                let base = win * stride;

                for sfb in 0..max_sfb_ste {
                    if !self.pred_used[g * max_sfb_ste + sfb] {
                        continue;
                    }

                    let alpha_re = self.alpha_q_re[g * max_sfb_ste + sfb];
                    let alpha_im = self.alpha_q_im[g * max_sfb_ste + sfb];

                    for i in
                        base + ics.swb_offset[sfb] as usize..base + ics.swb_offset[sfb + 1] as usize
                    {
                        let side =
                            coeffs2[i] - alpha_re * self.dmix_re[i] - alpha_im * self.dmix_im[i];
                        let left = coeffs1[i] + side;
                        let right = coeffs1[i] - side;
                        coeffs1[i] = left;
                        coeffs2[i] = sign * right;
                    }
                }
                win += 1;
            }
        }
    }
}

/// The 7-tap MDST estimation filter (ISO/IEC 23003-3 7.7.2), with the boundary handling of the
/// reference implementation.
fn complex_stereo_interpolate_imag(
    im: &mut [f32],
    re: &[f32],
    f: [f32; 7],
    len: usize,
    factor_even: f32,
    factor_odd: f32,
) {
    let mut s;

    let i = 0;
    s = f[6] * re[2]
        + f[5] * re[1]
        + f[4] * re[0]
        + f[3] * re[0]
        + f[2] * re[1]
        + f[1] * re[2]
        + f[0] * re[3];
    im[i] += s * factor_even;

    let i = 1;
    s = f[6] * re[1]
        + f[5] * re[0]
        + f[4] * re[0]
        + f[3] * re[1]
        + f[2] * re[2]
        + f[1] * re[3]
        + f[0] * re[4];
    im[i] += s * factor_odd;

    let i = 2;
    s = f[6] * re[0]
        + f[5] * re[0]
        + f[4] * re[1]
        + f[3] * re[2]
        + f[2] * re[3]
        + f[1] * re[4]
        + f[0] * re[5];
    im[i] += s * factor_even;

    let mut i = 3;
    while i < len - 4 {
        s = f[6] * re[i - 3]
            + f[5] * re[i - 2]
            + f[4] * re[i - 1]
            + f[3] * re[i]
            + f[2] * re[i + 1]
            + f[1] * re[i + 2]
            + f[0] * re[i + 3];
        im[i] += s * factor_odd;

        s = f[6] * re[i - 2]
            + f[5] * re[i - 1]
            + f[4] * re[i]
            + f[3] * re[i + 1]
            + f[2] * re[i + 2]
            + f[1] * re[i + 3]
            + f[0] * re[i + 4];
        im[i + 1] += s * factor_even;
        i += 2;
    }

    i = len - 3;
    s = f[6] * re[i - 3]
        + f[5] * re[i - 2]
        + f[4] * re[i - 1]
        + f[3] * re[i]
        + f[2] * re[i + 1]
        + f[1] * re[i + 2]
        + f[0] * re[i + 2];
    im[i] += s * factor_odd;

    i = len - 2;
    s = f[6] * re[i - 3]
        + f[5] * re[i - 2]
        + f[4] * re[i - 1]
        + f[3] * re[i]
        + f[2] * re[i + 1]
        + f[1] * re[i + 1]
        + f[0] * re[i];
    im[i] += s * factor_even;

    i = len - 1;
    s = f[6] * re[i - 3]
        + f[5] * re[i - 2]
        + f[4] * re[i - 1]
        + f[3] * re[i]
        + f[2] * re[i]
        + f[1] * re[i - 1]
        + f[0] * re[i - 2];
    im[i] += s * factor_odd;
}

/// The MDST estimation filter of the current frame (FDK `mdst_filt_coef_curr`, ISO/IEC 23003-3
/// Table 124), selected by the window sequence and the previous and current window shapes.
fn complex_stereo_get_filter(ics: &IcsInfo) -> [f32; 7] {
    let shape = match (ics.window_shape_prev, ics.window_shape) {
        (false, false) => 0,
        (true, true) => 1,
        (false, true) => 2,
        (true, false) => 3,
    };

    let win = match ics.window_sequence {
        LONG_START => {
            // A stop-start sequence can only follow a short or a start sequence.
            if ics.window_sequence_prev == EIGHT_SHORT || ics.window_sequence_prev == LONG_START {
                3
            }
            else {
                1
            }
        }
        LONG_STOP => 2,
        _ => 0,
    };

    MDST_FILT_CUR[win][shape]
}

/// The MDST estimation filter of the previous frame (FDK `mdst_filt_coef_prev`).
fn complex_stereo_get_filter_prev(ics: &IcsInfo) -> [f32; 7] {
    let shape = usize::from(ics.window_shape_prev);

    let row = match ics.window_sequence {
        LONG_START
            if ics.window_sequence_prev == EIGHT_SHORT
                || ics.window_sequence_prev == LONG_START =>
        {
            2 + shape
        }
        LONG_STOP => 2 + shape,
        _ => shape,
    };

    mdst_filt_prev_row(row)
}

/// The previous-window filter of the windows after the first of a short sequence.
fn mdst_filt_prev(shape: usize) -> [f32; 7] {
    mdst_filt_prev_row(shape)
}

fn mdst_filt_prev_row(row: usize) -> [f32; 7] {
    let c = MDST_FILT_PREV[row];
    [c[0], c[1], c[2], c[3], c[2], c[1], c[0]]
}
