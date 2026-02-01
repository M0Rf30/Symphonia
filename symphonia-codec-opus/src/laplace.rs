// Laplace Distribution Decoder - Rewritten from xiph/opus celt/laplace.c
// Copyright (c) 2007 CSIRO
// Copyright (c) 2007-2009 Xiph.Org Foundation
// SPDX-License-Identifier: BSD-3-Clause

use crate::entdec::RangeDecoder;

/// The minimum probability of an energy delta (out of 32768)
const LAPLACE_LOG_MINP: u32 = 0;
const LAPLACE_MINP: u32 = 1 << LAPLACE_LOG_MINP;

/// The minimum number of guaranteed representable energy deltas (in one direction)
const LAPLACE_NMIN: u32 = 16;

/// Calculate frequency for Laplace distribution
/// When called, decay is positive and at most 11456
#[inline]
fn ec_laplace_get_freq1(fs0: u32, decay: i32) -> u32 {
    let ft = 32768 - LAPLACE_MINP * (2 * LAPLACE_NMIN) - fs0;
    ((ft as i64) * (16384 - decay as i64) >> 15) as u32
}

/// Decode a value from a Laplace distribution
///
/// This is used for decoding energy deltas in CELT.
///
/// Arguments:
/// - dec: Range decoder
/// - fs: Initial frequency (related to probability of zero)
/// - decay: Decay rate for the exponential tail
pub fn ec_laplace_decode(dec: &mut RangeDecoder, fs: u32, decay: i32) -> i32 {
    let mut val = 0i32;
    let mut fl = 0u32;
    let mut fs = fs;

    let fm = dec.decode_bin(15);

    if fm >= fs {
        val += 1;
        fl = fs;
        fs = ec_laplace_get_freq1(fs, decay) + LAPLACE_MINP;

        // Search the decaying part of the PDF
        while fs > LAPLACE_MINP && fm >= fl + 2 * fs {
            fs *= 2;
            fl += fs;
            fs = ((fs - 2 * LAPLACE_MINP) as i64 * decay as i64 >> 15) as u32;
            fs += LAPLACE_MINP;
            val += 1;
        }

        // Everything beyond that has probability LAPLACE_MINP
        if fs <= LAPLACE_MINP {
            let di = (fm - fl) >> (LAPLACE_LOG_MINP + 1);
            val += di as i32;
            fl += 2 * di * LAPLACE_MINP;
        }

        if fm < fl + fs {
            val = -val;
        } else {
            fl += fs;
        }
    }

    debug_assert!(fl < 32768);
    debug_assert!(fs > 0);
    debug_assert!(fl <= fm);
    debug_assert!(fm < fl + fs.min(32768 - fl));

    dec.update(fl, (fl + fs).min(32768), 32768);
    val
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_laplace_get_freq1() {
        let fs0 = 16384;
        let decay = 8192;
        let freq = ec_laplace_get_freq1(fs0, decay);
        assert!(freq > 0);
        assert!(freq < 32768);
    }
}
