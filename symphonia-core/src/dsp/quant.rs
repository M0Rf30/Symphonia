// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared inverse-quantization tables.

use lazy_static::lazy_static;

/// The length of the [`pow43_table`].
pub const POW43_LEN: usize = 8207;

lazy_static! {
    static ref POW43: Box<[f32; POW43_LEN]> = {
        let pow43: Vec<f32> = (0..POW43_LEN).map(|i| f32::powf(i as f32, 4.0 / 3.0)).collect();
        pow43.into_boxed_slice().try_into().expect("vec initialized to POW43_LEN")
    };
}

/// Lookup table for `x(i) = i^(4/3)` for integral `i` in `0..POW43_LEN`.
///
/// Used to inverse-quantize Huffman-decoded magnitudes in MPEG audio codecs.
pub fn pow43_table() -> &'static [f32; POW43_LEN] {
    &POW43
}
