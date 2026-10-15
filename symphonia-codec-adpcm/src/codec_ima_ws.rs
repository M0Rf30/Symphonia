// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Westwood IMA ADPCM (AIFF-C `ADP4`), headerless 4-bit IMA.
//!
//! Matches FFmpeg's `adpcm_ima_ws` decoder for a Westwood VQA version other than 3 (the AIFF-C
//! case): each byte holds two codes of one channel, the low nibble first. The bytes of the
//! channels alternate, so a stereo pair of bytes holds two frames. The predictor and step index
//! carry over from packet to packet and start at zero.

use crate::common_ima::AdpcmImaBlockStatus;

pub(crate) struct ImaWsDecoder {
    state: [AdpcmImaBlockStatus; 2],
}

impl ImaWsDecoder {
    pub(crate) fn new() -> Self {
        Self { state: std::array::from_fn(|_| AdpcmImaBlockStatus { predictor: 0, step_index: 0 }) }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    /// Decodes mono `data` (2 frames per byte) into `out`.
    pub(crate) fn decode_mono(&mut self, data: &[u8], out: &mut [i32]) {
        for (&byte, out) in data.iter().zip(out.chunks_exact_mut(2)) {
            out[0] = self.state[0].expand_direct(byte & 0x0f);
            out[1] = self.state[0].expand_direct(byte >> 4);
        }
    }

    /// Decodes stereo `data` (2 frames per 2 bytes) into `left` and `right`.
    pub(crate) fn decode_stereo(&mut self, data: &[u8], left: &mut [i32], right: &mut [i32]) {
        for ((pair, l), r) in
            data.chunks_exact(2).zip(left.chunks_exact_mut(2)).zip(right.chunks_exact_mut(2))
        {
            l[0] = self.state[0].expand_direct(pair[0] & 0x0f);
            l[1] = self.state[0].expand_direct(pair[0] >> 4);
            r[0] = self.state[1].expand_direct(pair[1] & 0x0f);
            r[1] = self.state[1].expand_direct(pair[1] >> 4);
        }
    }
}
