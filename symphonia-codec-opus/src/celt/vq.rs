// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Spherical vector quantization. Ported from libopus `celt/vq.c` (`alg_unquant`,
//! `renormalise_vector`, `stereo_itheta`). Ported from libopus (BSD-3-Clause), see NOTICE.
//! Owner (wave 1): "CeltBitstream".

use crate::range::RangeDecoder;

/// C: `alg_unquant`. Decodes and denormalizes `n`-dimensional shape `x` with `k` pulses and
/// gain `gain`.
pub fn alg_unquant(x: &mut [f32], n: i32, k: i32, spread: i32, blocks: i32, gain: f32, rd: &mut RangeDecoder<'_>) {
    let _ = (x, n, k, spread, blocks, gain, rd);
    todo!("wave 1 (celt/CeltBitstream): alg_unquant")
}

/// C: `renormalise_vector`.
pub fn renormalise_vector(x: &mut [f32], n: i32, gain: f32) {
    let _ = (x, n, gain);
    todo!("wave 1 (celt/CeltBitstream): renormalise_vector")
}

/// C: `stereo_itheta`.
pub fn stereo_itheta(x: &[f32], y: &[f32], stereo: bool, n: i32) -> i32 {
    let _ = (x, y, stereo, n);
    todo!("wave 1 (celt/CeltBitstream): stereo_itheta")
}
