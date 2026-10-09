// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Top-level per-frame decode: normal frames run the full indices/pulses/parameters/core
//! pipeline; lost frames run PLC extrapolation instead; both then run CNG and PLC-glue before
//! updating the rolling output buffer. Ported from libopus `silk/decode_frame.c`
//! (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::range::RangeDecoder;
use crate::silk::cng::silk_cng;
use crate::silk::decode_core::silk_decode_core;
use crate::silk::decode_indices::silk_decode_indices;
use crate::silk::decode_parameters::silk_decode_parameters;
use crate::silk::decode_pulses::silk_decode_pulses;
use crate::silk::plc::{silk_plc, silk_plc_glue_frames};
use crate::silk::structs::{MAX_FRAME_LENGTH, SilkDecoderControl, SilkDecoderState};

/// C: `FLAG_DECODE_NORMAL` / `FLAG_PACKET_LOST` / `FLAG_DECODE_LBRR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LostFlag {
    DecodeNormal,
    PacketLost,
    DecodeLbrr,
}

/// Whether the output of the SILK enhancer stage is reproduced. libopus >= 1.5 built with
/// `ENABLE_OSCE` (the default of the 1.6 distribution builds, e.g. the Arch/ffmpeg libopus 1.6.1)
/// always runs `osce_enhance_frame()` after `silk_decode_core()`, even when no OSCE model is
/// loaded or the OSCE method is `OSCE_METHOD_NONE` (the decoder default, enhancement is only
/// selected through complexity >= 6). With `OSCE_METHOD_NONE` the stage degenerates to an
/// int16 -> float -> int16 round trip whose only visible effect is the clamp of the output to
/// `[-32767, 32767]`. A libopus built without `ENABLE_OSCE` keeps `-32768`; set this to `false`
/// to match such a build.
const OSCE_OUTPUT_CLAMP: bool = true;

/// C: the output stage of `osce_enhance_frame` (`dnn/osce.c`) with `OSCE_METHOD_NONE`:
///
/// ```c
/// /* enhancement only implemented for 20 ms frame at 16kHz */
/// if (psDec->fs_kHz != 16 || psDec->nb_subfr != 4) { osce_reset(...); return; }
/// ...
/// float tmp = 32768.f * out_buffer[i];
/// if (tmp > 32767.f) tmp = 32767.f;
/// if (tmp < -32767.f) tmp = -32767.f;
/// xq[i] = float2int(tmp);
/// ```
///
/// The stage runs on the frames produced by `silk_decode_core` (normal and LBRR frames) only,
/// *after* the LTP `outBuf` has been updated (which therefore keeps `-32768`) and *before* the
/// PLC state update, CNG and PLC glue; concealed frames and frames of any other internal rate
/// or frame duration (10 ms frames of 2 subframes, 8/12 kHz) are not touched.
/// `32768.f * (x / 32768.f)` is exact for `i16`, so only `-32768` is changed.
fn osce_output_clamp(dec: &SilkDecoderState, xq: &mut [i16]) {
    if !OSCE_OUTPUT_CLAMP || dec.fs_khz != 16 || dec.nb_subfr != 4 {
        return;
    }
    for v in xq.iter_mut() {
        *v = (*v).max(-i16::MAX);
    }
}

/// C: `silk_decode_frame`. Decodes one 5/10/20 ms SILK frame set (`psDec->nb_subfr` subframes)
/// into `p_out[0..L]`, returning `L` (`psDec->frame_length`).
pub(crate) fn silk_decode_frame(
    dec: &mut SilkDecoderState,
    rd: &mut RangeDecoder<'_>,
    p_out: &mut [i16],
    lost_flag: LostFlag,
    cond_coding: i32,
) -> usize {
    let l = dec.frame_length as usize;
    debug_assert!(l > 0 && l <= MAX_FRAME_LENGTH);

    let mut ctrl = SilkDecoderControl::default();

    if lost_flag == LostFlag::DecodeNormal
        || (lost_flag == LostFlag::DecodeLbrr && dec.lbrr_flags[dec.n_frames_decoded as usize])
    {
        let mut pulses = [0i16; MAX_FRAME_LENGTH];

        // Decode quantization indices of side info.
        silk_decode_indices(
            dec,
            rd,
            dec.n_frames_decoded as usize,
            lost_flag == LostFlag::DecodeLbrr,
            cond_coding,
        );

        // Decode quantization indices of excitation. C: `pulses` is allocated rounded up to a
        // multiple of `SHELL_CODEC_FRAME_LENGTH` (16) -- e.g. 120 (12 kHz * 10 ms) rounds up to
        // 128 -- since the shell decoder always writes whole 16-sample blocks.
        let padded_l = (l + 15) & !15;
        silk_decode_pulses(
            rd,
            &mut pulses[..padded_l],
            dec.indices.signal_type as i32,
            dec.indices.quant_offset_type as i32,
            l,
        );

        // Decode parameters and pulse signal.
        silk_decode_parameters(dec, &mut ctrl, cond_coding);

        // Run inverse NSQ.
        silk_decode_core(dec, &ctrl, &mut p_out[..l], &pulses[..l]);

        // Update output buffer.
        debug_assert!(dec.ltp_mem_length >= dec.frame_length);
        let mv_len = (dec.ltp_mem_length - dec.frame_length) as usize;
        dec.out_buf.copy_within(l..l + mv_len, 0);
        dec.out_buf[mv_len..mv_len + l].copy_from_slice(&p_out[..l]);

        // Run the SILK enhancer stage (a pass-through, see `osce_output_clamp`).
        osce_output_clamp(dec, &mut p_out[..l]);

        // Update PLC state.
        silk_plc(dec, &mut ctrl, &mut p_out[..l], false);

        dec.loss_cnt = 0;
        dec.prev_signal_type = dec.indices.signal_type as i32;
        debug_assert!(dec.prev_signal_type >= 0 && dec.prev_signal_type <= 2);

        // A frame has been decoded without errors.
        dec.first_frame_after_reset = false;
    }
    else {
        // Handle packet loss by extrapolation.
        silk_plc(dec, &mut ctrl, &mut p_out[..l], true);

        // Update output buffer.
        debug_assert!(dec.ltp_mem_length >= dec.frame_length);
        let mv_len = (dec.ltp_mem_length - dec.frame_length) as usize;
        dec.out_buf.copy_within(l..l + mv_len, 0);
        dec.out_buf[mv_len..mv_len + l].copy_from_slice(&p_out[..l]);
    }

    // Comfort noise generation / estimation.
    silk_cng(dec, &ctrl, &mut p_out[..l], l);

    // Ensure smooth connection of extrapolated and good frames.
    silk_plc_glue_frames(dec, &mut p_out[..l], l);

    // Update some decoder state variables.
    dec.lag_prev = ctrl.pitch_l[dec.nb_subfr as usize - 1];

    l
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(fs_khz: i32, nb_subfr: i32) -> [i16; 4] {
        let mut dec = SilkDecoderState::new();
        dec.fs_khz = fs_khz;
        dec.nb_subfr = nb_subfr;
        let mut xq = [i16::MIN, -1, i16::MIN + 1, i16::MAX];
        osce_output_clamp(&dec, &mut xq);
        xq
    }

    /// libopus' `osce_enhance_frame` only runs for 20 ms frames (4 subframes) of 16 kHz SILK and
    /// clamps `-32768` to `-32767`; every other sample, frame size and rate is untouched.
    #[test]
    fn osce_clamp_only_applies_to_20ms_16khz_frames() {
        assert_eq!(run(16, 4), [-32767, -1, -32767, 32767]);
        for (fs_khz, nb_subfr) in [(16, 2), (12, 4), (8, 4), (12, 2), (8, 2)] {
            assert_eq!(
                run(fs_khz, nb_subfr),
                [i16::MIN, -1, i16::MIN + 1, i16::MAX],
                "{fs_khz} kHz {nb_subfr}"
            );
        }
    }
}
