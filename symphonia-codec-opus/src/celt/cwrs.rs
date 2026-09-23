// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Combinatorial (codeword-range) pulse-vector decoding. Ported from libopus `celt/cwrs.c`
//! (`decode_pulses`, `cwrsi`, `icwrs`, `get_required_bits`). Ported from libopus
//! (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltBitstream".

use crate::range::RangeDecoder;

/// C: `decode_pulses`. Decodes a pulse-vector index for an `n`-dimensional band with `k` pulses
/// into `y` (length `n`).
pub fn decode_pulses(y: &mut [i32], n: i32, k: i32, rd: &mut RangeDecoder<'_>) {
    let _ = (y, n, k, rd);
    todo!("wave 1 (celt/CeltBitstream): decode_pulses/cwrsi")
}
