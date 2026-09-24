// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Binary-tree "shell" decoder for one 16-pulse sub-block: recursively splits a total pulse
//! count into two halves via entropy-coded binomial-like tables. Ported from libopus
//! `silk/shell_coder.c` (decoder side only -- `silk_shell_encoder`/`encode_split` are
//! encoder-only and excluded) (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::range::RangeDecoder;
use crate::silk::tables::{SHELL_CODE_TABLE0, SHELL_CODE_TABLE1, SHELL_CODE_TABLE2, SHELL_CODE_TABLE3, SHELL_CODE_TABLE_OFFSETS};

/// C: `decode_split`. Splits `p` into `(p_child1, p_child2)` with `p_child1 + p_child2 == p`.
#[inline]
fn decode_split(rd: &mut RangeDecoder<'_>, p: i32, shell_table: &[u8]) -> (i16, i16) {
    if p > 0 {
        let offset = SHELL_CODE_TABLE_OFFSETS[p as usize] as usize;
        let p_child1 = rd.dec_icdf(&shell_table[offset..], 8) as i16;
        (p_child1, p as i16 - p_child1)
    } else {
        (0, 0)
    }
}

/// C: `silk_shell_decoder`. Decodes one shell-code frame of `SHELL_CODEC_FRAME_LENGTH` (16)
/// pulses given the total pulse count `pulses4` for the frame; writes the per-sample
/// nonnegative pulse amplitudes into `pulses0[0..16]`.
pub(crate) fn silk_shell_decoder(pulses0: &mut [i16], rd: &mut RangeDecoder<'_>, pulses4: i32) {
    debug_assert!(pulses0.len() >= 16);

    let mut pulses3 = [0i16; 2];
    let mut pulses2 = [0i16; 4];
    let mut pulses1 = [0i16; 8];

    (pulses3[0], pulses3[1]) = decode_split(rd, pulses4, &SHELL_CODE_TABLE3);

    (pulses2[0], pulses2[1]) = decode_split(rd, pulses3[0] as i32, &SHELL_CODE_TABLE2);

    (pulses1[0], pulses1[1]) = decode_split(rd, pulses2[0] as i32, &SHELL_CODE_TABLE1);
    (pulses0[0], pulses0[1]) = decode_split(rd, pulses1[0] as i32, &SHELL_CODE_TABLE0);
    (pulses0[2], pulses0[3]) = decode_split(rd, pulses1[1] as i32, &SHELL_CODE_TABLE0);

    (pulses1[2], pulses1[3]) = decode_split(rd, pulses2[1] as i32, &SHELL_CODE_TABLE1);
    (pulses0[4], pulses0[5]) = decode_split(rd, pulses1[2] as i32, &SHELL_CODE_TABLE0);
    (pulses0[6], pulses0[7]) = decode_split(rd, pulses1[3] as i32, &SHELL_CODE_TABLE0);

    (pulses2[2], pulses2[3]) = decode_split(rd, pulses3[1] as i32, &SHELL_CODE_TABLE2);

    (pulses1[4], pulses1[5]) = decode_split(rd, pulses2[2] as i32, &SHELL_CODE_TABLE1);
    (pulses0[8], pulses0[9]) = decode_split(rd, pulses1[4] as i32, &SHELL_CODE_TABLE0);
    (pulses0[10], pulses0[11]) = decode_split(rd, pulses1[5] as i32, &SHELL_CODE_TABLE0);

    (pulses1[6], pulses1[7]) = decode_split(rd, pulses2[3] as i32, &SHELL_CODE_TABLE1);
    (pulses0[12], pulses0[13]) = decode_split(rd, pulses1[6] as i32, &SHELL_CODE_TABLE0);
    (pulses0[14], pulses0[15]) = decode_split(rd, pulses1[7] as i32, &SHELL_CODE_TABLE0);
}
