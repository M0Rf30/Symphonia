// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Yamaha ADPCM (AICA / YMZ280B style, `WAVE_FORMAT_YAMAHA_ADPCM` 0x0020).
//!
//! Matches FFmpeg's `adpcm_yamaha` decoder (`adpcm_yamaha_expand_nibble` in `libavcodec/adpcm.c`
//! with `ff_adpcm_yamaha_difflookup` and `ff_adpcm_yamaha_indexscale`). Mono data holds two
//! samples per byte (low nibble first). Stereo data holds one frame per byte: the low nibble is
//! the left and the high nibble the right channel. The predictor and step carry over from packet
//! to packet.

use symphonia_core::util::clamp::clamp_i16;

use crate::common::from_i16_shift;

const DIFF_LOOKUP: [i32; 16] = [1, 3, 5, 7, 9, 11, 13, 15, -1, -3, -5, -7, -9, -11, -13, -15];
const INDEX_SCALE: [i32; 16] =
    [230, 230, 230, 230, 307, 409, 512, 614, 230, 230, 230, 230, 307, 409, 512, 614];

const STEP_MIN: i32 = 127;
const STEP_MAX: i32 = 24576;

#[derive(Clone, Copy)]
struct ChannelState {
    predictor: i32,
    step: i32,
}

impl ChannelState {
    const INIT: Self = Self { predictor: 0, step: STEP_MIN };

    fn expand(&mut self, nibble: u8) -> i32 {
        let nibble = usize::from(nibble);
        self.predictor = clamp_i16(self.predictor + (self.step * DIFF_LOOKUP[nibble]) / 8) as i32;
        self.step = ((self.step * INDEX_SCALE[nibble]) >> 8).clamp(STEP_MIN, STEP_MAX);
        from_i16_shift!(self.predictor)
    }
}

pub(crate) struct YamahaDecoder {
    state: [ChannelState; 2],
}

impl YamahaDecoder {
    pub(crate) fn new() -> Self {
        Self { state: [ChannelState::INIT; 2] }
    }

    pub(crate) fn reset(&mut self) {
        self.state = [ChannelState::INIT; 2];
    }

    /// Decodes mono `data` (2 frames per byte) into `out`.
    pub(crate) fn decode_mono(&mut self, data: &[u8], out: &mut [i32]) {
        for (byte, out) in data.iter().zip(out.chunks_exact_mut(2)) {
            out[0] = self.state[0].expand(byte & 0x0f);
            out[1] = self.state[0].expand(byte >> 4);
        }
    }

    /// Decodes stereo `data` (1 frame per byte) into `left` and `right`.
    pub(crate) fn decode_stereo(&mut self, data: &[u8], left: &mut [i32], right: &mut [i32]) {
        for ((byte, l), r) in data.iter().zip(left.iter_mut()).zip(right.iter_mut()) {
            *l = self.state[0].expand(byte & 0x0f);
            *r = self.state[1].expand(byte >> 4);
        }
    }
}
