// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unpack per-index entropy-table selectors and backward predictor coefficients for one NLSF
//! codebook vector. Ported from libopus `silk/NLSF_unpack.c` (BSD-3-Clause), see NOTICE.

#![allow(dead_code)]

use crate::silk::structs::{NlsfCbStruct, MAX_LPC_ORDER, NLSF_QUANT_MAX_AMPLITUDE};

/// C: `silk_NLSF_unpack`. Fills `ec_ix` (indices into `psNLSF_CB->ec_iCDF`) and `pred_q8`
/// (backward predictor coefficients) for `CB1_index`'s entropy path, `order` entries each.
pub(crate) fn silk_nlsf_unpack(
    ec_ix: &mut [i16; MAX_LPC_ORDER],
    pred_q8: &mut [u8; MAX_LPC_ORDER],
    ps_nlsf_cb: &NlsfCbStruct,
    cb1_index: usize,
) {
    let order = ps_nlsf_cb.order as usize;
    let ec_sel = &ps_nlsf_cb.ec_sel[cb1_index * order / 2..];

    let mut i = 0usize;
    while i < order {
        let entry = ec_sel[i / 2];
        ec_ix[i] = (((entry >> 1) & 7) as i32 * (2 * NLSF_QUANT_MAX_AMPLITUDE + 1)) as i16;
        pred_q8[i] = ps_nlsf_cb.pred_q8[i + ((entry & 1) as usize) * (order - 1)];
        ec_ix[i + 1] = ((((entry >> 5) & 7) as i32) * (2 * NLSF_QUANT_MAX_AMPLITUDE + 1)) as i16;
        pred_q8[i + 1] = ps_nlsf_cb.pred_q8[i + (((entry >> 4) & 1) as usize) * (order - 1) + 1];
        i += 2;
    }
}
