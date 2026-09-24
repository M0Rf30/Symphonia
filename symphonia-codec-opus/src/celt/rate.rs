// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bit allocation. Ported from libopus `celt/rate.c` (`clt_compute_allocation` and its
//! `interp_bits2pulses`/`compute_pulse_cache` helpers). Ported from libopus (BSD-3-Clause), see
//! NOTICE. Owner (wave 1): "CeltBitstream".

use crate::celt::modes::CeltMode;
use crate::range::RangeDecoder;

/// Output of [`clt_compute_allocation`]: per-band bit/pulse allocation. C: out-parameters
/// `pulses`, `ebits`, `fine_priority` of `clt_compute_allocation`, plus its `intensity`/`dual`
/// return-adjacent out-params, and the `balance`/total-bits return value.
pub struct Allocation {
    pub pulses: Vec<i32>,
    pub fine_energy_bits: Vec<i32>,
    pub fine_priority: Vec<i32>,
    pub intensity: i32,
    pub dual_stereo: bool,
    pub balance: i32,
    pub coded_bands: i32,
}

/// C: `clt_compute_allocation` (decode direction, `encode == 0`).
///
/// Deviates from the wave-0 stub: gained `rd: &mut RangeDecoder<'_>` because the decode
/// direction reads range-coded band-skip/intensity/dual-stereo bits inline
/// (`interp_bits2pulses`'s `ec_dec_bit_logp`/`ec_dec_uint` calls) — matches "CeltBitstream"'s own
/// `opus-celt-bits` branch (see coordination log).
#[allow(clippy::too_many_arguments)]
pub fn clt_compute_allocation(
    mode: &CeltMode,
    start: i32,
    end: i32,
    offsets: &[i32],
    caps: &[i32],
    alloc_trim: i32,
    total_bits: i32,
    lm: i32,
    channels: i32,
    rd: &mut RangeDecoder<'_>,
) -> Allocation {
    let _ = (mode, start, end, offsets, caps, alloc_trim, total_bits, lm, channels, rd);
    todo!("wave 1 (celt/CeltBitstream): clt_compute_allocation")
}
