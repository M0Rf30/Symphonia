// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MSB-first bit reader and the Huffman tables built from FFmpeg's `(symbol, length)` lists.

use std::sync::LazyLock;

use symphonia_core::errors::{Result, decode_error};

use crate::tables::{
    COEFFS_TAB, HUFFMAN_COEFFS, HUFFMAN_SF_SIGNED, HUFFMAN_SF_UNSIGNED, SFB_A_TAB, SFB_B_TAB,
};

/// An MSB-first bit reader over a byte slice.
///
/// Reading past the end of the data yields zero bits; [`BitReader::overread`] reports whether
/// that happened.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// Peek up to 32 bits without consuming them.
    #[inline]
    pub(crate) fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let mut word = [0u8; 8];
        if let Some(src) = self.data.get(byte..) {
            let len = src.len().min(8);
            word[..len].copy_from_slice(&src[..len]);
        }
        let v = u64::from_be_bytes(word) << (self.pos & 7);
        (v >> (64 - n)) as u32
    }

    /// Read up to 32 bits. Reading 0 bits yields 0.
    #[inline]
    pub(crate) fn get(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    #[inline]
    pub(crate) fn get1(&mut self) -> bool {
        self.get(1) != 0
    }

    #[inline]
    pub(crate) fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    /// Advance to the next byte boundary.
    #[inline]
    pub(crate) fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// True if more bits were read than the data holds.
    #[inline]
    pub(crate) fn overread(&self) -> bool {
        self.pos > self.data.len() * 8
    }
}

/// A single-level lookup table for a Huffman code.
pub(crate) struct Vlc {
    bits: u32,
    /// `(symbol << 8) | length`, with a length of 0 marking an unassigned code.
    table: Box<[i32]>,
}

impl Vlc {
    /// Builds a table from a list of `[symbol, length]` pairs. Codes are assigned in list order
    /// as left-aligned running sums of `2^-length` (FFmpeg `ff_vlc_init_from_lengths`).
    fn from_lengths(list: &[[u8; 2]], offset: i32) -> Result<Vlc> {
        let bits = list.iter().map(|e| u32::from(e[1])).max().unwrap_or(0);
        if bits == 0 || bits > 16 {
            return decode_error("atrac9: invalid codebook");
        }
        let mut table = vec![0i32; 1 << bits];
        let mut code: u64 = 0;
        for &[sym, len] in list {
            let len = u32::from(len);
            if len == 0 {
                continue;
            }
            if code >= 1 << 32 {
                return decode_error("atrac9: overdetermined codebook");
            }
            let first = (code >> (32 - bits)) as usize;
            let count = 1usize << (bits - len);
            let entry = ((i32::from(sym) + offset) << 8) | len as i32;
            for slot in table.iter_mut().skip(first).take(count) {
                *slot = entry;
            }
            code += 1 << (32 - len);
        }
        Ok(Vlc { bits, table: table.into_boxed_slice() })
    }

    /// Decodes one symbol.
    #[inline]
    pub(crate) fn read(&self, br: &mut BitReader<'_>) -> Result<i32> {
        let entry = self.table[br.peek(self.bits) as usize];
        let len = (entry & 0xff) as u32;
        if len == 0 {
            return decode_error("atrac9: invalid huffman code");
        }
        br.skip(len);
        Ok(entry >> 8)
    }
}

/// All the Huffman tables of the codec.
pub(crate) struct Codebooks {
    /// Scalefactor tables: `[signed][code length]`.
    pub(crate) sf: [[Option<Vlc>; 8]; 2],
    /// Coefficient tables: `[codebook set][precision][codebook index]`.
    pub(crate) coeffs: [[[Option<Vlc>; 4]; 8]; 2],
}

static CODEBOOKS: LazyLock<Option<Codebooks>> = LazyLock::new(|| Codebooks::build().ok());

impl Codebooks {
    pub(crate) fn get() -> Result<&'static Codebooks> {
        match CODEBOOKS.as_ref() {
            Some(cb) => Ok(cb),
            None => decode_error("atrac9: invalid codebook"),
        }
    }

    fn build() -> Result<Codebooks> {
        let mut cb = Codebooks { sf: Default::default(), coeffs: Default::default() };

        // Unsigned scalefactor tables.
        let mut pos = 0;
        for i in 1..7 {
            let n = HUFFMAN_SF_UNSIGNED[i].size();
            cb.sf[0][i] = Some(Vlc::from_lengths(slice(&SFB_A_TAB, pos, n)?, 0)?);
            pos += n;
        }

        // Signed scalefactor tables. The symbols are stored with a +16 offset.
        let mut pos = 0;
        for i in 2..6 {
            let n = HUFFMAN_SF_SIGNED[i].size();
            cb.sf[1][i] = Some(Vlc::from_lengths(slice(&SFB_B_TAB, pos, n)?, -16)?);
            pos += n;
        }

        // Coefficient tables.
        let mut pos = 0;
        for i in 0..2 {
            for j in 2..8 {
                for k in i..4 {
                    let n = HUFFMAN_COEFFS[i][j][k].size();
                    cb.coeffs[i][j][k] = Some(Vlc::from_lengths(slice(&COEFFS_TAB, pos, n)?, 0)?);
                    pos += n;
                }
            }
        }

        Ok(cb)
    }
}

fn slice(tab: &[[u8; 2]], pos: usize, n: usize) -> Result<&[[u8; 2]]> {
    match tab.get(pos..pos + n) {
        Some(s) => Ok(s),
        None => decode_error("atrac9: invalid codebook"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_reader_reads_msb_first() {
        let mut br = BitReader::new(&[0b1010_0101, 0xff]);
        assert_eq!(br.get(3), 0b101);
        assert_eq!(br.get(5), 0b00101);
        assert_eq!(br.get(4), 0xf);
        assert!(!br.overread());
        br.align();
        assert_eq!(br.get(1), 0);
        assert!(br.overread());
    }

    #[test]
    fn sign_extension() {
        assert_eq!(symphonia_core::util::bits::sign_extend_leq32_to_i32(0b111, 3), -1);
        assert_eq!(symphonia_core::util::bits::sign_extend_leq32_to_i32(0b011, 3), 3);
        assert_eq!(symphonia_core::util::bits::sign_extend_leq32_to_i32(0b100, 3), -4);
    }

    #[test]
    fn codebooks_build_and_are_complete() {
        let cb = Codebooks::get().expect("codebooks");
        for tab in cb.sf.iter().flatten().flatten() {
            assert!(tab.table.iter().all(|&e| e & 0xff != 0));
        }
        for tab in cb.coeffs.iter().flatten().flatten().flatten() {
            assert!(tab.table.iter().all(|&e| e & 0xff != 0));
        }
    }
}
