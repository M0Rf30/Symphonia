// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Ported from FFmpeg libavcodec dcahuff.c (LGPL-2.1+), see NOTICE.

//! Huffman code books of the DTS core.
//!
//! The source tables ([`VLC_SRC`]) list `(symbol, length)` pairs. Codes are assigned sequentially
//! in table order: each code is the previous code plus one unit at its own length (MSB aligned).
//! Every code book is a complete prefix code, so any bit pattern decodes to a symbol.

use std::sync::LazyLock;

use crate::bits::BitReader;
use crate::tables::{QUANT_INDEX_GROUP_SIZE, VLC_SRC};

/// Number of code books (quantizer classes) with Huffman codes.
pub const CODE_BOOKS: usize = 10;

const BITALLOC_SIZES: [usize; CODE_BOOKS] = [3, 5, 7, 9, 13, 17, 25, 33, 65, 129];
const BITALLOC_OFFSETS: [i32; CODE_BOOKS] = [-1, -2, -3, -4, -6, -8, -12, -16, -32, -64];

/// Number of bits resolved by the first-level lookup table.
const LUT_BITS: u32 = 10;

pub struct HuffTable {
    /// First level lookup: `(symbol << 8) | length` or 0 if the code is longer than `lut_bits`.
    lut: Vec<i32>,
    lut_bits: u32,
    /// Codes longer than `lut_bits`: `(code, length, symbol)`.
    long: Vec<(u32, u8, i16)>,
}

impl HuffTable {
    fn new(src: &[(u8, u8)], offset: i32) -> Self {
        let max_len = src.iter().map(|&(_, l)| u32::from(l)).max().unwrap_or(1);
        let lut_bits = max_len.min(LUT_BITS);

        let mut lut = vec![0i32; 1 << lut_bits];
        let mut long = Vec::new();

        let mut code = 0u64;
        for &(sym, len) in src {
            let len = u32::from(len);
            let value = i32::from(sym) + offset;
            let c = (code >> (32 - len)) as u32;

            if len <= lut_bits {
                let shift = lut_bits - len;
                let start = (c << shift) as usize;
                for slot in &mut lut[start..start + (1usize << shift)] {
                    *slot = (value << 8) | len as i32;
                }
            }
            else {
                long.push((c, len as u8, value as i16));
            }

            code += 1u64 << (32 - len);
        }

        // The tables are complete prefix codes.
        debug_assert_eq!(code, 1u64 << 32);

        HuffTable { lut, lut_bits, long }
    }

    /// Decode one symbol.
    #[inline]
    pub fn decode(&self, bs: &mut BitReader<'_>) -> i32 {
        let idx = bs.peek(self.lut_bits) as usize;
        let e = self.lut[idx];
        if e != 0 {
            bs.skip((e & 0xff) as usize);
            return e >> 8;
        }

        // Entry with value 0 and length 0 can't occur, so a zero entry means a long code.
        let bits = bs.peek(16);
        for &(code, len, sym) in &self.long {
            if bits >> (16 - u32::from(len)) == code {
                bs.skip(len as usize);
                return i32::from(sym);
            }
        }

        // Unreachable for complete codes; skip a bit to guarantee progress.
        bs.skip(1);
        0
    }
}

pub struct Huffman {
    pub quant_index: Vec<Vec<HuffTable>>,
    pub bit_allocation: Vec<HuffTable>,
    pub scale_factor: Vec<HuffTable>,
    pub transition_mode: Vec<HuffTable>,
}

impl Huffman {
    pub fn get() -> &'static Huffman {
        static TABLES: LazyLock<Huffman> = LazyLock::new(Huffman::build);
        &TABLES
    }

    fn build() -> Huffman {
        let mut pos = 0;
        let mut take = |count: usize, offset: i32| {
            let t = HuffTable::new(&VLC_SRC[pos..pos + count], offset);
            pos += count;
            t
        };

        let mut quant_index = Vec::with_capacity(CODE_BOOKS);
        for i in 0..CODE_BOOKS {
            let group = usize::from(QUANT_INDEX_GROUP_SIZE[i]);
            quant_index
                .push((0..group).map(|_| take(BITALLOC_SIZES[i], BITALLOC_OFFSETS[i])).collect());
        }

        let bit_allocation = (0..5).map(|_| take(12, 1)).collect();
        let scale_factor = (0..5).map(|_| take(129, -64)).collect();
        let transition_mode = (0..4).map(|_| take(4, 0)).collect();

        Huffman { quant_index, bit_allocation, scale_factor, transition_mode }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_tables_build() {
        // Building asserts that every code book is a complete prefix code.
        let huff = Huffman::get();
        assert_eq!(huff.quant_index.len(), CODE_BOOKS);
        assert_eq!(huff.bit_allocation.len(), 5);
        assert_eq!(huff.scale_factor.len(), 5);
        assert_eq!(huff.transition_mode.len(), 4);
    }

    #[test]
    fn verify_first_code_book() {
        // The first code book is `0` -> 0, `10` -> 1, `11` -> -1 (symbols 1, 2, 0 with offset -1).
        let table = &Huffman::get().quant_index[0][0];
        let mut bs = BitReader::new(&[0b0101_1000]);
        assert_eq!(table.decode(&mut bs), 0);
        assert_eq!(table.decode(&mut bs), 1);
        assert_eq!(table.decode(&mut bs), -1);
        assert_eq!(table.decode(&mut bs), 0);
        assert_eq!(bs.pos(), 6);
    }
}
