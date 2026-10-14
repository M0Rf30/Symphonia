// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-band intensity stereo.
//!
//! Ported from libspeex `stereo.c` (BSD-3-Clause), see NOTICE. A stereo Speex stream is a mono
//! stream whose in-band messages carry the left/right balance and energy ratio.

use crate::bits::Bits;
use crate::tables::E_RATIO_QUANT;

/// State of the intensity stereo expander (`SpeexStereoState`).
#[derive(Clone)]
pub struct StereoState {
    balance: f32,
    e_ratio: f32,
    smooth_left: f32,
    smooth_right: f32,
}

impl Default for StereoState {
    fn default() -> Self {
        StereoState { balance: 1.0, e_ratio: 0.5, smooth_left: 1.0, smooth_right: 1.0 }
    }
}

impl StereoState {
    /// Handles the in-band stereo request (`speex_std_stereo_request_handler`). The id of the
    /// request has already been read.
    pub fn read_request(&mut self, bits: &mut Bits<'_>) {
        let sign: f32 = if bits.unpack(1) != 0 { -1.0 } else { 1.0 };
        let dexp = bits.unpack(5) as f32;
        self.balance = (f64::from(sign) * 0.25 * f64::from(dexp)).exp() as f32;
        self.e_ratio = E_RATIO_QUANT[bits.unpack(2) as usize];
    }

    /// Expands the mono signal `mono` to the left and right channels (`speex_decode_stereo`).
    ///
    /// The reference walks the frame from its last sample to its first, smoothing the channel
    /// gains along the way, and this is reproduced.
    pub fn expand(&mut self, mono: &[f32], left: &mut [f32], right: &mut [f32]) {
        let balance = self.balance;
        let e_ratio = self.e_ratio;

        let e_right: f32 =
            1.0f32 / ((f64::from(e_ratio) * (1.0 + f64::from(balance))).sqrt() as f32);
        let e_left: f32 = f64::from(balance).sqrt() as f32 * e_right;

        for i in (0..mono.len()).rev() {
            self.smooth_left = self.smooth_left * 0.98f32 + e_left * 0.02f32;
            self.smooth_right = self.smooth_right * 0.98f32 + e_right * 0.02f32;
            left[i] = self.smooth_left * mono[i];
            right[i] = self.smooth_right * mono[i];
        }
    }
}
