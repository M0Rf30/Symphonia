// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Laplace-distributed integer decoding. Ported from libopus `celt/laplace.c`
//! (`ec_laplace_decode`). Ported from libopus (BSD-3-Clause), see NOTICE.
//! Owner (wave 1): "CeltBitstream".

use crate::range::RangeDecoder;

/// C: `ec_laplace_decode`. Decodes one coarse-energy prediction residual.
pub fn ec_laplace_decode(rd: &mut RangeDecoder<'_>, fs: u32, decay: u32) -> i32 {
    let _ = (rd, fs, decay);
    todo!("wave 1 (celt/CeltBitstream): ec_laplace_decode")
}
