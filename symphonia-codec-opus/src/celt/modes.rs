// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CELT mode tables. Ported from libopus `celt/modes.c` / `celt/static_modes_float.h`
//! (`struct OpusCustomMode`, `celt_mode_static`). Ported from libopus (BSD-3-Clause), see
//! NOTICE. Owner (wave 1): "CeltBitstream".
//!
//! Only the single 48 kHz / 960-sample (20 ms max frame) mode is needed — Opus never uses
//! `opus_custom_mode_create` with other parameters.

/// C: `struct PulseCache` (`celt/modes.h`).
pub struct PulseCache {
    pub size: i32,
    /// Indexed by `[band]`: offset into `bits`/`caps`.
    pub index: &'static [i16],
    pub bits: &'static [u8],
    pub caps: &'static [u8],
}

/// C: `struct OpusCustomMode` (`celt/modes.h`), restricted to the fields the decoder reads
/// (encoder-only fields such as `encoder` are omitted).
pub struct CeltMode {
    pub sample_rate: i32,
    pub overlap: i32,
    pub nb_ebands: i32,
    pub effective_ebands: i32,
    /// Band edges, in units of 400 Hz (25 entries for 21 bands + sentinels), C: `eBands`.
    pub e_bands: &'static [i16],
    pub max_lm: i32,
    pub nb_short_mdcts: i32,
    pub short_mdct_size: i32,
    /// C: `allocVectors`: the static bit-allocation table used by `rate.rs`'s
    /// `clt_compute_allocation`.
    pub alloc_vectors: &'static [u8],
    pub log_n: &'static [i16],
    /// Analysis/synthesis window, length `overlap`, C: `window`.
    pub window: &'static [f32],
    pub cache: PulseCache,
}

/// C: `static_mode_48000_960_120` (the sole mode Opus uses). Table contents are populated by
/// wave 1 ("CeltBitstream") by transcribing `celt/static_modes_float.h`; left empty here so the
/// crate compiles — nothing in wave 0 reads these values.
pub static MODE_48000_960: CeltMode = CeltMode {
    sample_rate: 48000,
    overlap: 120,
    nb_ebands: 21,
    effective_ebands: 21,
    e_bands: &[],
    max_lm: 3,
    nb_short_mdcts: 8,
    short_mdct_size: 120,
    alloc_vectors: &[],
    log_n: &[],
    window: &[],
    cache: PulseCache { size: 0, index: &[], bits: &[], caps: &[] },
};
