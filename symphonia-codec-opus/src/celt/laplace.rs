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

/// The minimum probability of an energy delta (out of 32768). C: `LAPLACE_MINP`
/// (`LAPLACE_LOG_MINP` is `0`, so `LAPLACE_MINP = 1<<0`).
const LAPLACE_MINP: u32 = 1;
const LAPLACE_LOG_MINP: u32 = 0;
/// The minimum number of guaranteed representable energy deltas (in one direction).
/// C: `LAPLACE_NMIN`.
const LAPLACE_NMIN: u32 = 16;

/// C: `ec_laplace_get_freq1`. Called with `decay` positive and at most `11456`.
fn ec_laplace_get_freq1(fs0: u32, decay: i32) -> u32 {
    let ft = 32768 - LAPLACE_MINP * (2 * LAPLACE_NMIN) - fs0;
    ((ft as i64 * (16384 - decay) as i64) >> 15) as u32
}

/// C: `ec_laplace_decode`. Decodes one coarse-energy prediction residual.
pub fn ec_laplace_decode(rd: &mut RangeDecoder<'_>, fs: u32, decay: u32) -> i32 {
    let mut val: i32 = 0;
    let mut fl: u32;
    let mut fs = fs;
    let decay = decay as i32;
    let fm = rd.decode_bin(15);
    fl = 0;
    if fm >= fs {
        val += 1;
        fl = fs;
        fs = ec_laplace_get_freq1(fs, decay) + LAPLACE_MINP;
        // Search the decaying part of the PDF.
        while fs > LAPLACE_MINP && fm >= fl + 2 * fs {
            fs *= 2;
            fl += fs;
            fs = (((fs - 2 * LAPLACE_MINP) as i64 * decay as i64) >> 15) as u32;
            fs += LAPLACE_MINP;
            val += 1;
        }
        // Everything beyond that has probability LAPLACE_MINP.
        if fs <= LAPLACE_MINP {
            let di = (fm - fl) >> (LAPLACE_LOG_MINP + 1);
            val += di as i32;
            fl += 2 * di * LAPLACE_MINP;
        }
        if fm < fl + fs {
            val = -val;
        }
        else {
            fl += fs;
        }
    }
    rd.update(fl, (fl + fs).min(32768), 32768);
    val
}
