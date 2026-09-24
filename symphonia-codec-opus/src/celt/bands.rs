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

/// C: `quant_all_bands` (decoder direction, `encode == 0`, so the encoder-only `bandE`/
/// `complexity`/`theta_rdo` machinery is dropped). Drives [`crate::celt::cwrs`] /
/// [`crate::celt::vq`] / [`crate::celt::laplace`] per band and fills `x` (and `y` for stereo)
/// with normalized MDCT-domain band shapes. `seed` is the running anti-collapse LCG seed
/// (C: `*seed`/`st->rng`), read on entry and written back on return.
///
/// Deviates from the wave-0 stub: gained `short_blocks`/`spread`/`dual_stereo`/`intensity`/
/// `tf_res`/`total_bits`/`balance`/`lm`/`coded_bands`/`seed`/`disable_inv` (the original stub
/// was missing most of `quant_all_bands`'s real parameters — verified against
/// `celt_decoder.c`'s call site and `bands.c`; see coordination log). `arch`/`complexity` are
/// dropped (encode-only); `bandE` is dropped (only read by the encode-only theta-RDO path).
#[allow(clippy::too_many_arguments)]
pub fn quant_all_bands(
    mode: &CeltMode,
    start: i32,
    end: i32,
    x: &mut [f32],
    y: Option<&mut [f32]>,
    collapse_masks: &mut [u8],
    pulses: &[i32],
    short_blocks: bool,
    spread: i32,
    dual_stereo: bool,
    intensity: i32,
    tf_res: &[i32],
    total_bits: i32,
    balance: i32,
    rd: &mut RangeDecoder<'_>,
    lm: i32,
    coded_bands: i32,
    seed: &mut u32,
    disable_inv: bool,
) {
    let _ = (
        mode,
        start,
        end,
        x,
        y,
        collapse_masks,
        pulses,
        short_blocks,
        spread,
        dual_stereo,
        intensity,
        tf_res,
        total_bits,
        balance,
        rd,
        lm,
        coded_bands,
        seed,
        disable_inv,
    );
    todo!("wave 1 (celt/CeltBitstream): quant_all_bands (decode direction)")
}

/// C: `anti_collapse`.
///
/// Deviates from the wave-0 stub: gained `pulses` (used by the per-band collapse-depth
/// estimate, `bands.c` line ~286 `celt_udiv(1+pulses[i], ...)`; the original stub omitted it).
/// `channels` (C: `C`) matches the stub name; `old_band_e` here is C's `logE` (the just-decoded
/// current-frame log-energy, i.e. the caller's `old_e_bands` after `unquant_energy_finalise`,
/// not a stale value despite the name).
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
    pulses: &[i32],
    seed: u32,
) {
    let _ = (
        mode,
        x,
        collapse_masks,
        lm,
        channels,
        size,
        start,
        end,
        old_band_e,
        old_log_e,
        old_log_e2,
        pulses,
        seed,
    );
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
