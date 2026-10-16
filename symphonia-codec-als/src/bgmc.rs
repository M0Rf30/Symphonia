// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Block Gilbert-Moore code (BGMC) arithmetic decoder, a port of FFmpeg `libavcodec/bgmc.c`
//! (LGPL-2.1+, Copyright (c) 2010 Thilo Borgmann), see the `NOTICE` file.

use crate::bgmc_tables::{CF_TABLES_1, CF_TABLES_2, CF_TABLES_3};
use crate::bits::Bits;

/// Bits used by the frequency counters.
const FREQ_BITS: u32 = 14;
/// Bits used to represent the values.
const VALUE_BITS: u32 = 18;
const TOP_VALUE: u32 = (1 << VALUE_BITS) - 1;
const FIRST_QTR: u32 = TOP_VALUE / 4 + 1;
const HALF: u32 = 2 * FIRST_QTR;
const THIRD_QTR: u32 = 3 * FIRST_QTR;

/// Number of bits used to index the lookup tables.
const LUT_BITS: u32 = FREQ_BITS - 8;
const LUT_SIZE: usize = 1 << LUT_BITS;
/// Number of buffered lookup tables.
const LUT_BUFF: usize = 4;

/// The cumulative frequency table for the table index `sx` (0..16).
fn cf_table(sx: usize) -> &'static [u16] {
    match sx {
        0..=2 => &CF_TABLES_1[sx],
        3..=10 => &CF_TABLES_2[sx - 3],
        _ => &CF_TABLES_3[(sx - 11).min(4)],
    }
}

/// The state of the arithmetic decoder.
pub struct BgmcState {
    high: u32,
    low: u32,
    value: u32,
}

/// Lookup tables to speed up the symbol search, for each of the `LUT_BUFF` deltas.
pub struct Bgmc {
    lut: Vec<u8>,
    status: [i32; LUT_BUFF],
}

impl Bgmc {
    pub fn new() -> Self {
        Bgmc { lut: vec![0; LUT_BUFF * 16 * LUT_SIZE], status: [-1; LUT_BUFF] }
    }

    /// Retune the lookup table for `delta`, refilling it if it is for another delta. Returns the
    /// offset of the table in `lut`.
    fn lut_offset(&mut self, delta: i32) -> usize {
        let i = delta.clamp(0, LUT_BUFF as i32 - 1) as usize;
        let base = (i * LUT_SIZE) << 4;

        if self.status[i] != delta {
            let delta_bits = delta.max(0) as u32;
            let mut out = base;

            for sx in 0..16 {
                let cf = cf_table(sx);

                for k in 0..LUT_SIZE {
                    let target = ((k as u32) + 1) << (FREQ_BITS - LUT_BITS);
                    let mut symbol = 1usize << delta_bits;

                    while cf.get(symbol).is_some_and(|&c| u32::from(c) > target) {
                        symbol += 1 << delta_bits;
                    }

                    self.lut[out] = (symbol >> delta_bits) as u8;
                    out += 1;
                }
            }

            self.status[i] = delta;
        }

        base
    }

    /// Initialise the decoder and read the first value.
    pub fn init(bits: &mut Bits<'_>) -> Option<BgmcState> {
        if bits.left() < i64::from(VALUE_BITS) {
            return None;
        }
        Some(BgmcState { high: TOP_VALUE, low: 0, value: bits.get(VALUE_BITS) })
    }

    /// Finish decoding: the decoder read `VALUE_BITS - 2` bits too many.
    pub fn finish(bits: &mut Bits<'_>) {
        bits.rewind((VALUE_BITS - 2) as usize);
    }

    /// Decode `dst.len()` symbols. Returns `false` for a corrupt stream.
    pub fn decode(
        &mut self,
        bits: &mut Bits<'_>,
        dst: &mut [i32],
        delta: i32,
        sx: usize,
        state: &mut BgmcState,
    ) -> bool {
        let lut_base = self.lut_offset(delta) + sx * LUT_SIZE;
        let delta = delta.max(0) as u32;
        let cf = cf_table(sx);

        let mut high = state.high;
        let mut low = state.low;
        let mut value = state.value;

        for out in dst.iter_mut() {
            let range = high.wrapping_sub(low).wrapping_add(1);
            let target = (value.wrapping_sub(low).wrapping_add(1) << FREQ_BITS).wrapping_sub(1)
                / range.max(1);

            let Some(&lut_val) =
                self.lut.get(lut_base + (target >> (FREQ_BITS - LUT_BITS)) as usize)
            else {
                return false;
            };
            let mut symbol = (usize::from(lut_val)) << delta;

            loop {
                match cf.get(symbol) {
                    Some(&c) if u32::from(c) > target => symbol += 1 << delta,
                    Some(_) => break,
                    None => return false,
                }
            }

            let symbol = (symbol >> delta) - 1;

            let (Some(&c0), Some(&c1)) = (cf.get(symbol << delta), cf.get((symbol + 1) << delta))
            else {
                return false;
            };

            high = low.wrapping_add(
                (range.wrapping_mul(u32::from(c0)).wrapping_sub(1 << FREQ_BITS)) >> FREQ_BITS,
            );
            low = low.wrapping_add(range.wrapping_mul(u32::from(c1)) >> FREQ_BITS);

            loop {
                if high >= HALF {
                    if low >= HALF {
                        value = value.wrapping_sub(HALF);
                        low = low.wrapping_sub(HALF);
                        high = high.wrapping_sub(HALF);
                    }
                    else if low >= FIRST_QTR && high < THIRD_QTR {
                        value = value.wrapping_sub(FIRST_QTR);
                        low = low.wrapping_sub(FIRST_QTR);
                        high = high.wrapping_sub(FIRST_QTR);
                    }
                    else {
                        break;
                    }
                }

                low = low.wrapping_mul(2);
                high = high.wrapping_mul(2).wrapping_add(1);
                value = value.wrapping_mul(2).wrapping_add(bits.get1());

                // A corrupt stream can make the renormalisation spin on zero bits forever.
                if bits.left() < -64 {
                    return false;
                }
            }

            *out = symbol as i32;
        }

        state.high = high;
        state.low = low;
        state.value = value;
        true
    }
}
