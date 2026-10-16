// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MSB-first bit reader and canonical Huffman tables.

use symphonia_core::errors::{Result, decode_error};

/// An MSB-first bit reader over a byte slice.
///
/// Like the reference decoder (which reads from zero-padded buffers), reading past the end of the
/// data yields zero bits. [`BitReader::overread`] reports whether that happened.
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

    /// The number of bits left, negative if the reader has read past the end.
    #[inline]
    pub(crate) fn bits_left(&self) -> isize {
        (self.data.len() * 8) as isize - self.pos as isize
    }

    /// True if more bits were read than the data holds.
    #[inline]
    pub(crate) fn overread(&self) -> bool {
        self.bits_left() < 0
    }
}

/// A single-level lookup table for a canonical Huffman code with at most 12-bit codes.
pub(crate) struct Vlc {
    bits: u32,
    /// `symbol << 8 | code_length`, with a code length of 0 marking an unassigned code.
    table: Box<[u32]>,
}

impl Vlc {
    /// Build a table from a codebook descriptor and symbols.
    ///
    /// `counts[b - 1]` is the number of codes of length `b`; codes are assigned in increasing
    /// order, shortest first, to the symbols of `xlat` in sequence (FFmpeg's
    /// `build_canonical_huff` + `ff_vlc_init_from_lengths`). Returns the table and the number of
    /// symbols consumed from `xlat`.
    pub(crate) fn from_codebook(counts: &[u8; 12], xlat: &[u8]) -> Result<(Vlc, usize)> {
        let mut lens = [0u8; 256];
        let mut n = 0;
        for (b, &count) in counts.iter().enumerate() {
            for _ in 0..count {
                if n >= lens.len() {
                    return decode_error("atrac3plus: invalid codebook");
                }
                lens[n] = (b + 1) as u8;
                n += 1;
            }
        }
        if n == 0 || xlat.len() < n {
            return decode_error("atrac3plus: invalid codebook");
        }

        let bits = u32::from(lens[n - 1]);
        let mut table = vec![0u32; 1 << bits];
        // Code values are accumulated left-aligned in 32 bits.
        let mut code: u64 = 0;
        for (&len, &sym) in lens[..n].iter().zip(xlat) {
            let len = u32::from(len);
            if code >= 1 << 32 {
                return decode_error("atrac3plus: overdetermined codebook");
            }
            let first = (code >> (32 - bits)) as usize;
            let span = 1usize << (bits - len);
            for e in table.get_mut(first..first + span).unwrap_or_default() {
                *e = u32::from(sym) << 8 | len;
            }
            code += 1 << (32 - len);
        }

        Ok((Vlc { bits, table: table.into_boxed_slice() }, n))
    }

    /// Decode one symbol.
    #[inline]
    pub(crate) fn decode(&self, br: &mut BitReader<'_>) -> Result<u32> {
        let entry = self.table[br.peek(self.bits) as usize];
        let len = entry & 0xff;
        if len == 0 {
            return decode_error("atrac3plus: invalid Huffman code");
        }
        br.skip(len);
        Ok(entry >> 8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_bit_reader() {
        let data = [0b1010_0000, 0xff, 0x01];
        let mut br = BitReader::new(&data);
        assert_eq!(br.get(1), 1);
        assert_eq!(br.get(2), 0b01);
        assert_eq!(br.get(0), 0);
        assert_eq!(br.get(5), 0);
        assert_eq!(br.get(8), 0xff);
        assert_eq!(br.get(7), 0);
        assert!(!br.overread());
        assert_eq!(br.get(1), 1);
        assert_eq!(br.bits_left(), 0);
        assert_eq!(br.get(16), 0);
        assert!(br.overread());
    }

    #[test]
    fn verify_canonical_codes() {
        // Lengths 1, 2, 3, 3: codes 0, 10, 110, 111.
        let counts = [1, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let (vlc, used) = Vlc::from_codebook(&counts, &[7, 6, 5, 4]).unwrap();
        assert_eq!(used, 4);
        let data = [0b0101_1011, 0b1000_0000];
        let mut br = BitReader::new(&data);
        assert_eq!(vlc.decode(&mut br).unwrap(), 7);
        assert_eq!(vlc.decode(&mut br).unwrap(), 6);
        assert_eq!(vlc.decode(&mut br).unwrap(), 5);
        assert_eq!(vlc.decode(&mut br).unwrap(), 4);
    }
}
