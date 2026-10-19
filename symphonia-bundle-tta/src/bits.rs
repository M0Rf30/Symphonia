// Symphonia TTA Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The little-endian (least significant bit first) bit reader of the TTA frames.
//!
//! FFmpeg reads the frames with `BITSTREAM_READER_LE` and `get_unary`; this reader has the same
//! behaviour at the end of the data, which the decoder relies on to fail the same frames as
//! FFmpeg.

/// The number of bits that can be peeked at once: a 64-bit load, of which up to 7 low bits are
/// consumed already.
const PEEK_BITS: u32 = 56;

/// A reader of a byte slice, from the least significant bit of each byte.
pub(crate) struct BitReaderLe<'a> {
    data: &'a [u8],
    /// The position of the next bit.
    pos: usize,
}

impl<'a> BitReaderLe<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReaderLe { data, pos: 0 }
    }

    /// The number of bits that are not read yet.
    #[inline]
    pub fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    /// The next [`PEEK_BITS`] bits (or fewer, followed by zeros, at the end of the data).
    #[inline]
    fn peek(&self) -> u64 {
        let byte = self.pos >> 3;
        let word = match self.data.get(byte..byte + 8) {
            Some(bytes) => {
                let mut w = [0u8; 8];
                w.copy_from_slice(bytes);
                u64::from_le_bytes(w)
            }
            None => {
                let mut w = [0u8; 8];
                let rest = self.data.get(byte..).unwrap_or(&[]);
                w[..rest.len()].copy_from_slice(rest);
                u64::from_le_bytes(w)
            }
        };
        word >> (self.pos & 7)
    }

    /// Reads the number of consecutive 1 bits up to the next 0 bit, which is consumed, like
    /// FFmpeg's `get_unary(gb, 0, get_bits_left(gb))`: if the data ends before a 0 bit, all the
    /// bits are consumed, and their number is returned.
    #[inline]
    pub fn read_unary_ones(&mut self) -> u64 {
        let mut count = 0u64;

        loop {
            let left = self.bits_left();
            let ones = (!self.peek()).trailing_zeros();
            let avail = (left as u64).min(u64::from(PEEK_BITS)) as u32;

            if ones < avail {
                // The 0 bit is within the data.
                self.pos += ones as usize + 1;
                return count + u64::from(ones);
            }
            else if left <= PEEK_BITS as usize {
                // Only 1 bits to the end of the data.
                self.pos += left;
                return count + left as u64;
            }

            self.pos += PEEK_BITS as usize;
            count += u64::from(PEEK_BITS);
        }
    }

    /// Reads `n` bits, 1 <= `n` <= 25. The caller must make sure that `n` bits are left.
    #[inline]
    pub fn read_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 25 && n as usize <= self.bits_left());
        let v = (self.peek() & ((1u64 << n) - 1)) as u32;
        self.pos += n as usize;
        v
    }

    /// Skips to the next byte boundary.
    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// Skips `n` bits, if that many are left.
    pub fn skip(&mut self, n: usize) -> bool {
        if n > self.bits_left() {
            return false;
        }
        self.pos += n;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_unary_and_bits() {
        // Bits from the LSB of each byte: 1 1 0 | 1 0 | 1 0 0 (a 3-bit value of 1) | ...
        let data = [0b0010_1011u8, 0b0000_0100];
        let mut br = BitReaderLe::new(&data);
        assert_eq!(br.read_unary_ones(), 2);
        assert_eq!(br.read_unary_ones(), 1);
        assert_eq!(br.read_bits(3), 0b001);
        assert_eq!(br.bits_left(), 8);
    }

    #[test]
    fn verify_unary_long_runs_and_end_of_data() {
        // 130 ones, then a zero.
        let mut data = vec![0xffu8; 16];
        data.push(0b0000_0011);
        let mut br = BitReaderLe::new(&data);
        assert_eq!(br.read_unary_ones(), 130);
        assert_eq!(br.bits_left(), data.len() * 8 - 131);

        // Only ones to the end of the data: all the bits are consumed.
        for len in [1usize, 7, 8, 9, 20] {
            let data = vec![0xffu8; len];
            let mut br = BitReaderLe::new(&data);
            assert_eq!(br.read_unary_ones(), (len * 8) as u64);
            assert_eq!(br.bits_left(), 0);
            // Nothing is left: a read of the unary code yields 0.
            assert_eq!(br.read_unary_ones(), 0);
        }

        // The terminating zero is the last bit.
        let mut br = BitReaderLe::new(&[0x7f]);
        assert_eq!(br.read_unary_ones(), 7);
        assert_eq!(br.bits_left(), 0);
    }

    #[test]
    fn verify_align_and_skip() {
        let data = [0u8; 4];
        let mut br = BitReaderLe::new(&data);
        assert_eq!(br.read_unary_ones(), 0);
        br.align();
        assert_eq!(br.bits_left(), 24);
        assert!(br.skip(24));
        assert!(!br.skip(1));
    }
}
