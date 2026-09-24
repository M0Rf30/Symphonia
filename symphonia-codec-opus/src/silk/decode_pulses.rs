// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decode quantization indices of the excitation signal: rate level, per-block pulse sums
//! (with LSB extension for large counts), shell decoding, LSB refinement, and sign
//! application. Ported from libopus `silk/decode_pulses.c` (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::range::RangeDecoder;
use crate::silk::code_signs::silk_decode_signs;
use crate::silk::shell_coder::silk_shell_decoder;
use crate::silk::structs::{MAX_NB_SHELL_BLOCKS, N_RATE_LEVELS, SHELL_CODEC_FRAME_LENGTH, SILK_MAX_PULSES};
use crate::silk::tables::{LSB_ICDF, PULSES_PER_BLOCK_ICDF, RATE_LEVELS_ICDF};

/// C: `silk_decode_pulses`. Decodes `pulses[0..frame_length]` (signed excitation pulses).
pub(crate) fn silk_decode_pulses(
    rd: &mut RangeDecoder<'_>,
    pulses: &mut [i16],
    signal_type: i32,
    quant_offset_type: i32,
    frame_length: usize,
) {
    // Decode rate level.
    let rate_level_index = rd.dec_icdf(&RATE_LEVELS_ICDF[(signal_type >> 1) as usize], 8) as usize;

    // Calculate number of shell blocks.
    let mut iter = frame_length >> 4; // LOG2_SHELL_CODEC_FRAME_LENGTH == 4
    if iter * SHELL_CODEC_FRAME_LENGTH < frame_length {
        debug_assert_eq!(frame_length, 12 * 10);
        iter += 1;
    }

    let mut sum_pulses = [0i32; MAX_NB_SHELL_BLOCKS];
    let mut n_lshifts = [0i32; MAX_NB_SHELL_BLOCKS];

    // Sum-Weighted-Pulses Decoding.
    let cdf = &PULSES_PER_BLOCK_ICDF[rate_level_index];
    for i in 0..iter {
        n_lshifts[i] = 0;
        sum_pulses[i] = rd.dec_icdf(cdf, 8);

        // LSB indication.
        while sum_pulses[i] == SILK_MAX_PULSES + 1 {
            n_lshifts[i] += 1;
            // When we've already got 10 LSBs, shift the table to not allow (SILK_MAX_PULSES+1).
            let last_row = &PULSES_PER_BLOCK_ICDF[N_RATE_LEVELS - 1];
            let table: &[u8] = if n_lshifts[i] == 10 { &last_row[1..] } else { last_row };
            sum_pulses[i] = rd.dec_icdf(table, 8);
        }
    }

    // Shell decoding.
    for i in 0..iter {
        let base = i * SHELL_CODEC_FRAME_LENGTH;
        if sum_pulses[i] > 0 {
            silk_shell_decoder(&mut pulses[base..base + SHELL_CODEC_FRAME_LENGTH], rd, sum_pulses[i]);
        } else {
            for p in &mut pulses[base..base + SHELL_CODEC_FRAME_LENGTH] {
                *p = 0;
            }
        }
    }

    // LSB Decoding.
    for i in 0..iter {
        if n_lshifts[i] > 0 {
            let n_ls = n_lshifts[i];
            let base = i * SHELL_CODEC_FRAME_LENGTH;
            for k in 0..SHELL_CODEC_FRAME_LENGTH {
                let mut abs_q = pulses[base + k] as i32;
                for _ in 0..n_ls {
                    abs_q <<= 1;
                    abs_q += rd.dec_icdf(&LSB_ICDF, 8);
                }
                pulses[base + k] = abs_q as i16;
            }
            // Mark the number of pulses non-zero for sign decoding.
            sum_pulses[i] |= n_ls << 5;
        }
    }

    // Decode and add signs to pulse signal.
    silk_decode_signs(rd, pulses, frame_length, signal_type, quant_offset_type, &sum_pulses);
}
