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

#[cfg(test)]
mod tests {
    //! Port of libopus `celt/tests/test_unit_laplace.c`: encodes known values with
    //! `ec_laplace_encode` (a test-only port, mirroring the C unit test's own local
    //! `ec_laplace_get_start_freq` helper) and decodes them back with [`ec_laplace_decode`].
    use super::*;
    use crate::range::test_encoder::RangeEncoder;
    use crate::range::RangeDecoder;

    /// C: `ec_laplace_get_start_freq` (`celt/tests/test_unit_laplace.c`), the "fs" a real
    /// encoder would pick for `p0=0.5` at the given decay (the value `ec_laplace_decode`'s
    /// callers pass in practice, e.g. `quant_bands.c`'s `prob_model[pi]<<7`).
    fn ec_laplace_get_start_freq(decay: i32) -> u32 {
        let ft = 32768 - LAPLACE_MINP * (2 * LAPLACE_NMIN + 1);
        let fs = (ft as i64 * (16384 - decay) as i64) / (16384 + decay) as i64;
        fs as u32 + LAPLACE_MINP
    }

    /// C: `ec_laplace_encode`. Mutates `*value` in place if it had to be clamped to the
    /// largest representable magnitude (matching the C `int *value` out-parameter).
    fn ec_laplace_encode(enc: &mut RangeEncoder, value: &mut i32, fs: u32, decay: u32) {
        let mut fl = 0u32;
        let mut fs = fs;
        let decay_i = decay as i32;
        let val = *value;
        if val != 0 {
            let s: i32 = if val < 0 { -1 } else { 0 };
            let mut val = (val + s) ^ s;
            fl = fs;
            fs = ec_laplace_get_freq1(fs, decay_i);
            let mut i = 1;
            while fs > 0 && i < val {
                fs *= 2;
                fl += fs + 2 * LAPLACE_MINP;
                fs = (((fs as i64) * decay as i64) >> 15) as u32;
                i += 1;
            }
            if fs == 0 {
                let ndi_max = (32768 - fl + LAPLACE_MINP - 1) >> LAPLACE_LOG_MINP;
                let ndi_max = ((ndi_max as i32) - s) >> 1;
                let di = (val - i).min(ndi_max - 1);
                fl += (2 * di + 1 + s) as u32 * LAPLACE_MINP;
                fs = LAPLACE_MINP.min(32768 - fl);
                val = (i + di + s) ^ s;
                *value = val;
            }
            else {
                fs += LAPLACE_MINP;
                fl += if s == 0 { fs } else { 0 };
            }
            debug_assert!(fl + fs <= 32768);
            debug_assert!(fs > 0);
        }
        enc.encode_bin(fl, fl + fs, 15);
    }

    #[test]
    fn laplace_round_trip() {
        let mut val = [0i32; 200];
        let mut decay = [0i32; 200];
        val[0] = 3;
        decay[0] = 6000;
        val[1] = 0;
        decay[1] = 5800;
        val[2] = -1;
        decay[2] = 5600;
        // A small deterministic LCG stands in for `rand()` (we don't need cryptographic
        // randomness, just coverage across the value/decay space, and want a reproducible
        // test).
        let mut state: u32 = 0xC0FFEE;
        let mut next = || {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            (state >> 16) & 0x7fff
        };
        for i in 3..200 {
            val[i] = (next() % 15) as i32 - 7;
            decay[i] = (next() % 11000) as i32 + 5000;
        }

        let mut enc = RangeEncoder::new();
        for i in 0..200 {
            let fs = ec_laplace_get_start_freq(decay[i]);
            ec_laplace_encode(&mut enc, &mut val[i], fs, decay[i] as u32);
        }
        let bytes = enc.done();

        let mut dec = RangeDecoder::new(&bytes);
        for i in 0..200 {
            let fs = ec_laplace_get_start_freq(decay[i]);
            let d = ec_laplace_decode(&mut dec, fs, decay[i] as u32);
            assert_eq!(d, val[i], "mismatch at {i}: decay={}", decay[i]);
        }
    }

    #[test]
    fn laplace_get_freq1_matches_c_formula() {
        // `ec_laplace_get_freq1` is a direct arithmetic transcription; spot-check it against
        // an independent re-implementation of the same C formula (`ft*(16384-decay)>>15`).
        for decay in [1, 100, 5000, 11456] {
            for fs0 in [1u32, 100, 10000, 32000] {
                let ft = 32768u32.saturating_sub(LAPLACE_MINP * (2 * LAPLACE_NMIN)).saturating_sub(fs0);
                let expected = ((ft as i64 * (16384 - decay) as i64) >> 15) as u32;
                assert_eq!(ec_laplace_get_freq1(fs0, decay), expected);
            }
        }
    }
}
