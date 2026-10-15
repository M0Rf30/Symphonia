// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::util::clamp::clamp_i16;

use crate::common::{Nibble, from_i16_shift};

/// Step index adjustments indexed by the magnitude (the code without its sign bit) for 2, 3, 4
/// and 5 bits per sample. IMA Digital Audio Focus and Technical Working Groups, "Recommended
/// Practices for Enhancing Digital Audio Compatibility in Multimedia Systems", rev 3.00;
/// verified against FFmpeg's `adpcm_ima_wav` decoder.
#[rustfmt::skip]
const IMA_INDEX_TABLES: [&[i32]; 4] = [
    &[-1, 2],
    &[-1, -1, 1, 2],
    &[-1, -1, -1, -1, 2, 4, 6, 8],
    &[-1, -1, -1, -1, -1, -1, -1, -1, 1, 2, 4, 6, 8, 10, 13, 16],
];

#[rustfmt::skip]
pub(crate) static IMA_STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17,
    19, 21, 23, 25, 28, 31, 34, 37, 41, 45,
    50, 55, 60, 66, 73, 80, 88, 97, 107, 118,
    130, 143, 157, 173, 190, 209, 230, 253, 279, 307,
    337, 371, 408, 449, 494, 544, 598, 658, 724, 796,
    876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066,
    2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358,
    5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899,
    15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

/// `AdpcmImaBlockStatus` contains values to decode a block
pub(crate) struct AdpcmImaBlockStatus {
    pub(crate) predictor: i32,
    pub(crate) step_index: i32,
}

impl AdpcmImaBlockStatus {
    pub(crate) fn expand_nibble(&mut self, byte: u8, nibble: Nibble) -> i32 {
        self.expand_code(nibble.get_nibble(byte), 4)
    }

    /// Expands one 4-bit code with the multiply form of the IMA step: `((2 * magnitude + 1) *
    /// step) >> 3`. This is FFmpeg's `adpcm_ima_expand_nibble(.., shift = 3)`, which the Duck
    /// DK3/DK4, Westwood and Xbox decoders use; it rounds differently from [`Self::expand_code`].
    pub(crate) fn expand_direct(&mut self, code: u8) -> i32 {
        let step = IMA_STEP_TABLE[self.step_index as usize];
        let magnitude = i32::from(code & 7);
        let diff = ((2 * magnitude + 1) * step) >> 3;
        let predictor = if code & 8 != 0 { self.predictor - diff } else { self.predictor + diff };
        self.predictor = clamp_i16(predictor) as i32;
        self.step_index =
            (self.step_index + IMA_INDEX_TABLES[2][usize::from(code & 7)]).clamp(0, 88);
        from_i16_shift!(self.predictor)
    }

    /// Expands one `bits_per_sample` (2 to 5) bit code and returns the new sample as an i32
    /// holding a 16-bit sample in its upper half.
    pub(crate) fn expand_code(&mut self, code: u8, bits_per_sample: u32) -> i32 {
        let step = IMA_STEP_TABLE[self.step_index as usize];
        let shift = bits_per_sample - 1;
        let sign = (code >> shift) & 1 != 0;
        let magnitude = code & ((1 << shift) - 1);

        // Reference IMA/DVI algorithm (IMA "Recommended Practices", rev 3.00): the successive
        // approximation sum with each term truncated separately. This is NOT equal to
        // `((2 * magnitude + 1) * step) >> shift`, which rounds differently.
        let mut diff = step >> shift;
        for bit in 0..shift {
            if magnitude & (1 << (shift - 1 - bit)) != 0 {
                diff += step >> bit;
            }
        }

        let predictor = if sign { self.predictor - diff } else { self.predictor + diff };
        self.predictor = clamp_i16(predictor) as i32;
        self.step_index = (self.step_index
            + IMA_INDEX_TABLES[bits_per_sample as usize - 2][magnitude as usize])
            .clamp(0, 88);
        from_i16_shift!(self.predictor)
    }
}
