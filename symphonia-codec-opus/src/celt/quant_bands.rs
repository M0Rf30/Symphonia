// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Band energy quantization (decode side). Ported from libopus `celt/quant_bands.c`
//! (`unquant_coarse_energy`, `unquant_fine_energy`, `unquant_energy_finalise`, `amp2Log2`).
//! Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltBitstream".

use crate::celt::modes::CeltMode;
use crate::range::RangeDecoder;

/// C: `unquant_coarse_energy`. Decodes coarse per-band log-energy into `old_e_bands`
/// (length `channels * mode.nb_ebands`, dB-scaled `Q8` fixed-point-equivalent as `f32`).
pub fn unquant_coarse_energy(
    mode: &CeltMode,
    start: i32,
    end: i32,
    old_e_bands: &mut [f32],
    intra: bool,
    rd: &mut RangeDecoder<'_>,
    channels: i32,
    lm: i32,
) {
    let _ = (mode, start, end, old_e_bands, intra, rd, channels, lm);
    todo!("wave 1 (celt/CeltBitstream): unquant_coarse_energy")
}

/// C: `unquant_fine_energy`.
pub fn unquant_fine_energy(
    mode: &CeltMode,
    start: i32,
    end: i32,
    old_e_bands: &mut [f32],
    fine_quant: &[i32],
    rd: &mut RangeDecoder<'_>,
    channels: i32,
) {
    let _ = (mode, start, end, old_e_bands, fine_quant, rd, channels);
    todo!("wave 1 (celt/CeltBitstream): unquant_fine_energy")
}

/// C: `unquant_energy_finalise`.
pub fn unquant_energy_finalise(
    mode: &CeltMode,
    start: i32,
    end: i32,
    old_e_bands: &mut [f32],
    fine_quant: &[i32],
    fine_priority: &[i32],
    bits_left: i32,
    rd: &mut RangeDecoder<'_>,
    channels: i32,
) {
    let _ = (mode, start, end, old_e_bands, fine_quant, fine_priority, bits_left, rd, channels);
    todo!("wave 1 (celt/CeltBitstream): unquant_energy_finalise")
}

/// C: `amp2Log2` (used indirectly by anti-collapse and denormalisation on the decode side).
pub fn amp2_log2(mode: &CeltMode, effective_end: i32, end: i32, band_e: &[f32], band_log_e: &mut [f32], channels: i32) {
    let _ = (mode, effective_end, end, band_e, band_log_e, channels);
    todo!("wave 1 (celt/CeltBitstream): amp2Log2")
}
