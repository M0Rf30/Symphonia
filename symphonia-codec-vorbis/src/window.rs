// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::dsp::window::vorbis_power_sine_slope;

pub struct Windows {
    /// Short block window left-half curve.
    pub short: Vec<f32>,
    /// Long block window left-half curve.
    pub long: Vec<f32>,
}

impl Windows {
    pub fn new(blocksize0: usize, blocksize1: usize) -> Self {
        let short = vorbis_power_sine_slope(blocksize0 / 2);
        let long = vorbis_power_sine_slope(blocksize1 / 2);
        Windows { short, long }
    }
}
