// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-band decode driver. Ported from libopus `celt/bands.c` (decoder path only:
//! `quant_all_bands`, `anti_collapse`, `denormalise_bands`, and the spreading-decision
//! helpers). Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltBitstream".

use crate::celt::modes::CeltMode;
use crate::range::RangeDecoder;

/// C: `quant_all_bands` (decoder direction, `encode == 0`). Drives [`crate::celt::cwrs`] /
/// [`crate::celt::vq`] / [`crate::celt::laplace`] per band and fills `x` (and `y` for stereo)
/// with normalized MDCT-domain band shapes.
#[allow(clippy::too_many_arguments)]
pub fn quant_all_bands(
    mode: &CeltMode,
    start: i32,
    end: i32,
    x: &mut [f32],
    y: Option<&mut [f32]>,
    collapse_masks: &mut [u8],
    band_e: &[f32],
    pulses: &[i32],
    lm: i32,
    codec_channels: i32,
    rd: &mut RangeDecoder<'_>,
) {
    let _ = (mode, start, end, x, y, collapse_masks, band_e, pulses, lm, codec_channels, rd);
    todo!("wave 1 (celt/CeltBitstream): quant_all_bands (decode direction)")
}

/// C: `anti_collapse`.
#[allow(clippy::too_many_arguments)]
pub fn anti_collapse(
    mode: &CeltMode,
    x: &mut [f32],
    collapse_masks: &[u8],
    lm: i32,
    channels: i32,
    size: i32,
    start: i32,
    end: i32,
    old_band_e: &[f32],
    old_log_e: &[f32],
    old_log_e2: &[f32],
    seed: &mut u32,
) {
    let _ =
        (mode, x, collapse_masks, lm, channels, size, start, end, old_band_e, old_log_e, old_log_e2, seed);
    todo!("wave 1 (celt/CeltBitstream): anti_collapse")
}

/// C: `denormalise_bands`. Converts normalized band shapes `x` back into MDCT-domain
/// coefficients `freq`, scaled by decoded band energy `band_log_e`.
#[allow(clippy::too_many_arguments)]
pub fn denormalise_bands(
    mode: &CeltMode,
    x: &[f32],
    freq: &mut [f32],
    band_log_e: &[f32],
    start: i32,
    end: i32,
    m: i32,
    downsample: i32,
    silence: bool,
) {
    let _ = (mode, x, freq, band_log_e, start, end, m, downsample, silence);
    todo!("wave 1 (celt/CeltBitstream): denormalise_bands")
}
