// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decode side-information (quantization index) parameters from the payload: signal
//! type/quantization offset, gains, NLSF indices, NLSF interpolation factor, pitch lags, LTP
//! gains/scaling, and the excitation seed. Ported from libopus `silk/decode_indices.c`
//! (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::range::RangeDecoder;
use crate::silk::macros::silk_rshift;
use crate::silk::nlsf_unpack::silk_nlsf_unpack;
use crate::silk::structs::{
    SilkDecoderState, CODE_CONDITIONALLY, CODE_INDEPENDENTLY, MAX_LPC_ORDER, MAX_NB_SUBFR, NLSF_QUANT_MAX_AMPLITUDE,
    TYPE_VOICED,
};
use crate::silk::tables::{
    LTP_GAIN_ICDF_PTRS, LTP_PER_INDEX_ICDF, LTPSCALE_ICDF, NLSF_EXT_ICDF, NLSF_INTERPOLATION_FACTOR_ICDF,
    PITCH_DELTA_ICDF, PITCH_LAG_ICDF, TYPE_OFFSET_NO_VAD_ICDF, TYPE_OFFSET_VAD_ICDF, UNIFORM4_ICDF, UNIFORM8_ICDF,
    GAIN_ICDF, DELTA_GAIN_ICDF,
};

/// C: `silk_decode_indices`.
pub(crate) fn silk_decode_indices(
    dec: &mut SilkDecoderState,
    rd: &mut RangeDecoder<'_>,
    frame_index: usize,
    decode_lbrr: bool,
    cond_coding: i32,
) {
    let mut ec_ix = [0i16; MAX_LPC_ORDER];
    let mut pred_q8 = [0u8; MAX_LPC_ORDER];

    // Decode signal type and quantizer offset.
    let ix = if decode_lbrr || dec.vad_flags[frame_index] {
        rd.dec_icdf(&TYPE_OFFSET_VAD_ICDF, 8) + 2
    } else {
        rd.dec_icdf(&TYPE_OFFSET_NO_VAD_ICDF, 8)
    };
    dec.indices.signal_type = silk_rshift(ix, 1) as i8;
    dec.indices.quant_offset_type = (ix & 1) as i8;

    // Decode gains.
    // First subframe.
    if cond_coding == CODE_CONDITIONALLY {
        // Conditional coding.
        dec.indices.gains_indices[0] = rd.dec_icdf(&DELTA_GAIN_ICDF, 8) as i8;
    } else {
        // Independent coding, in two stages: MSB bits followed by 3 LSBs.
        let signal_type = dec.indices.signal_type as usize;
        let mut g0 = rd.dec_icdf(&GAIN_ICDF[signal_type], 8) << 3;
        g0 += rd.dec_icdf(&UNIFORM8_ICDF, 8);
        dec.indices.gains_indices[0] = g0 as i8;
    }

    // Remaining subframes.
    for i in 1..dec.nb_subfr as usize {
        dec.indices.gains_indices[i] = rd.dec_icdf(&DELTA_GAIN_ICDF, 8) as i8;
    }

    // Decode LSF indices.
    let signal_type_half = (dec.indices.signal_type as usize) >> 1;
    let n_vectors = dec.ps_nlsf_cb.n_vectors as usize;
    dec.indices.nlsf_indices[0] =
        rd.dec_icdf(&dec.ps_nlsf_cb.cb1_icdf[signal_type_half * n_vectors..], 8) as i8;
    silk_nlsf_unpack(&mut ec_ix, &mut pred_q8, dec.ps_nlsf_cb, dec.indices.nlsf_indices[0] as usize);
    debug_assert_eq!(dec.ps_nlsf_cb.order as i32, dec.lpc_order);
    let order = dec.ps_nlsf_cb.order as usize;
    for i in 0..order {
        let mut val = rd.dec_icdf(&dec.ps_nlsf_cb.ec_icdf[ec_ix[i] as usize..], 8);
        if val == 0 {
            val -= rd.dec_icdf(&NLSF_EXT_ICDF, 8);
        } else if val == 2 * NLSF_QUANT_MAX_AMPLITUDE {
            val += rd.dec_icdf(&NLSF_EXT_ICDF, 8);
        }
        dec.indices.nlsf_indices[i + 1] = (val - NLSF_QUANT_MAX_AMPLITUDE) as i8;
    }

    // Decode LSF interpolation factor.
    dec.indices.nlsf_interp_coef_q2 = if dec.nb_subfr as usize == MAX_NB_SUBFR {
        rd.dec_icdf(&NLSF_INTERPOLATION_FACTOR_ICDF, 8) as i8
    } else {
        4
    };

    if dec.indices.signal_type == TYPE_VOICED as i8 {
        // Decode pitch lags.
        // Get lag index.
        let mut decode_absolute_lag_index = true;
        if cond_coding == CODE_CONDITIONALLY && dec.ec_prev_signal_type == TYPE_VOICED {
            // Decode delta index.
            let delta_lag_index = rd.dec_icdf(&PITCH_DELTA_ICDF, 8);
            if delta_lag_index > 0 {
                let delta_lag_index = delta_lag_index - 9;
                dec.indices.lag_index = dec.ec_prev_lag_index + delta_lag_index as i16;
                decode_absolute_lag_index = false;
            }
        }
        if decode_absolute_lag_index {
            // Absolute decoding.
            dec.indices.lag_index = (rd.dec_icdf(&PITCH_LAG_ICDF, 8) * silk_rshift(dec.fs_khz, 1)) as i16;
            dec.indices.lag_index += rd.dec_icdf(dec.pitch_lag_low_bits_icdf, 8) as i16;
        }
        dec.ec_prev_lag_index = dec.indices.lag_index;

        // Get contour index.
        dec.indices.contour_index = rd.dec_icdf(dec.pitch_contour_icdf, 8) as i8;

        // Decode LTP gains.
        // Decode PERIndex value.
        dec.indices.per_index = rd.dec_icdf(&LTP_PER_INDEX_ICDF, 8) as i8;

        for k in 0..dec.nb_subfr as usize {
            dec.indices.ltp_index[k] = rd.dec_icdf(LTP_GAIN_ICDF_PTRS[dec.indices.per_index as usize], 8) as i8;
        }

        // Decode LTP scaling.
    dec.indices.ltp_scale_index =
        if cond_coding == CODE_INDEPENDENTLY { rd.dec_icdf(&LTPSCALE_ICDF, 8) as i8 } else { 0 };
    }
    dec.ec_prev_signal_type = dec.indices.signal_type as i32;

    // Decode seed.
    dec.indices.seed = rd.dec_icdf(&UNIFORM4_ICDF, 8) as i8;
}
