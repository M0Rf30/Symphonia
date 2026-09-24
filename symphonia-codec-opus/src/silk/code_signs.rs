// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decode signs of pulse-coded excitation samples. Ported from libopus `silk/code_signs.c`
//! (decoder side only -- `silk_encode_signs` is encoder-only and excluded) (BSD-3-Clause), see
//! NOTICE.

#![allow(dead_code)]

use crate::range::RangeDecoder;
use crate::silk::macros::{silk_add_lshift32, silk_min, silk_smulbb};
use crate::silk::structs::{MAX_NB_SHELL_BLOCKS, SHELL_CODEC_FRAME_LENGTH};
use crate::silk::tables::SIGN_ICDF;

/// C: `silk_dec_map`: maps a decoded bit (0/1) to a sign multiplier (-1/+1).
#[inline]
fn silk_dec_map(a: i32) -> i32 {
    (a << 1) - 1
}

/// C: `silk_decode_signs`. Applies signs to the (so far nonnegative) pulse amplitudes in
/// `pulses[0..length]`, in place. `sum_pulses` holds, per `SHELL_CODEC_FRAME_LENGTH`-sample
/// block, the pre-LSB-decode pulse-sum value (bits `0..5`; higher bits used as an LSB-shift
/// marker by `silk_decode_pulses`, matching the C `p & 0x1F` masking below).
pub(crate) fn silk_decode_signs(
    rd: &mut RangeDecoder<'_>,
    pulses: &mut [i16],
    length: usize,
    signal_type: i32,
    quant_offset_type: i32,
    sum_pulses: &[i32; MAX_NB_SHELL_BLOCKS],
) {
    let mut icdf = [0u8, 0u8];
    let i = silk_smulbb(7, silk_add_lshift32(quant_offset_type, signal_type, 1));
    let icdf_ptr = &SIGN_ICDF[i as usize..];
    let n_blocks = (length + SHELL_CODEC_FRAME_LENGTH / 2) >> 4; // LOG2_SHELL_CODEC_FRAME_LENGTH == 4

    let mut q_off = 0usize;
    for blk in 0..n_blocks {
        let p = sum_pulses[blk];
        if p > 0 {
            icdf[0] = icdf_ptr[silk_min(p & 0x1F, 6) as usize];
            for j in 0..SHELL_CODEC_FRAME_LENGTH {
                if pulses[q_off + j] > 0 {
                    let bit = rd.dec_icdf(&icdf, 8);
                    pulses[q_off + j] = (pulses[q_off + j] as i32 * silk_dec_map(bit)) as i16;
                }
            }
        }
        q_off += SHELL_CODEC_FRAME_LENGTH;
    }
}
