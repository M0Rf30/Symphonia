// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MSB-first bit reader with the semantics of FFmpeg's `GetBitContext`.

use symphonia_core::errors::{Result, decode_error};

/// An MSB-first bit reader.
///
/// Like `GetBitContext`, reading never fails: bits past the end of the data read as zero, and the
/// caller tests [`BitReader::bits_left`] at the points where the reference decoder does. The
/// nominal length (`limit`) may be shorter than the data.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// The position of the next bit to read, counted from the start of `data`.
    pos: usize,
    /// The nominal length in bits.
    limit: usize,
}

impl<'a> BitReader<'a> {
    /// Instantiates a reader over `data` that is `limit` bits long.
    pub fn new(data: &'a [u8], limit: usize) -> Self {
        BitReader { data, pos: 0, limit }
    }

    /// The number of unread bits of the nominal length. Negative once the reader has read past
    /// it.
    #[inline]
    pub fn bits_left(&self) -> i64 {
        self.limit as i64 - self.pos as i64
    }

    /// The next 32 bits, without consuming them.
    #[inline]
    pub fn peek32(&self) -> u32 {
        let byte = self.pos >> 3;

        // The 40 bits that start at the byte holding the next bit.
        let mut window = 0u64;

        match self.data.get(byte..byte + 5) {
            Some(bytes) => {
                for &b in bytes {
                    window = window << 8 | u64::from(b);
                }
            }
            None => {
                for i in 0..5 {
                    window = window << 8 | u64::from(self.data.get(byte + i).copied().unwrap_or(0));
                }
            }
        }

        (window >> (8 - (self.pos & 7))) as u32
    }

    /// Skips `n` bits.
    #[inline]
    pub fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// Reads `n` bits (at most 32) as an unsigned integer.
    #[inline]
    pub fn get_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let v = (u64::from(self.peek32()) >> (32 - n)) as u32;
        self.pos += n as usize;
        v
    }

    /// Reads a single bit.
    #[inline]
    pub fn get_bit(&mut self) -> bool {
        self.get_bits(1) != 0
    }

    /// Counts the leading one bits of the stream, up to `max`, and consumes them together with
    /// the terminating zero bit, if the count is less than `max` (FFmpeg `get_unary(gb, 0, max)`).
    pub fn get_unary(&mut self, max: u32) -> u32 {
        let mut n = 0;

        while n < max && self.get_bit() {
            n += 1;
        }

        n
    }

    /// Reads an unsigned Exp-Golomb code in the range 0 to 8190 (FFmpeg `get_ue_golomb`).
    ///
    /// Codes with more than 12 leading zero bits are rejected. FFmpeg returns `AVERROR_INVALIDDATA`
    /// from the same condition but its callers use it as a value without checking.
    #[inline]
    pub fn get_ue_golomb(&mut self) -> Result<u32> {
        let bits = self.peek32();
        let zeros = bits.leading_zeros();

        if zeros > 12 {
            return decode_error("ralf: invalid exp-golomb code");
        }

        let len = 2 * zeros + 1;
        self.pos += len as usize;

        Ok((bits >> (32 - len)) - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_get_bits_across_bytes_and_past_the_end() {
        let data = [0b1010_0101, 0b1100_0011, 0xff];
        let mut br = BitReader::new(&data, 24);

        assert_eq!(br.get_bits(3), 0b101);
        assert_eq!(br.get_bits(7), 0b001_0111);
        assert_eq!(br.bits_left(), 14);
        // 14 bits of data, then 3 bits past the end that read as zero.
        assert_eq!(br.get_bits(17), 0b0_0001_1111_1111_1000);
        assert_eq!(br.bits_left(), -3);
        assert_eq!(br.get_bits(8), 0);
        assert_eq!(br.peek32(), 0);
    }

    #[test]
    fn verify_unary() {
        let data = [0b1101_1111, 0b1000_0000];
        let mut br = BitReader::new(&data, 16);
        assert_eq!(br.get_unary(6), 2);
        // Six ones: capped, the terminating zero is not consumed.
        assert_eq!(br.get_unary(6), 6);
        // The sixth one was inside the first byte's successor; the next bit is a zero.
        assert_eq!(br.get_unary(6), 0);
    }

    /// Appends the Exp-Golomb code of `v` to the bit string `bits`.
    fn push_ue(bits: &mut String, v: u32) {
        let code = format!("{:b}", v + 1);
        bits.push_str(&"0".repeat(code.len() - 1));
        bits.push_str(&code);
    }

    #[test]
    fn verify_ue_golomb() {
        let values = [0, 1, 2, 3, 4, 7, 8, 100, 4095, 4096, 8190];
        let mut bits = String::new();
        for &v in &values {
            push_ue(&mut bits, v);
        }
        // 13 leading zeros are invalid.
        bits.push_str("0000000000000100");

        let mut data = vec![0u8; bits.len().div_ceil(8)];
        for (i, c) in bits.bytes().enumerate() {
            if c == b'1' {
                data[i / 8] |= 0x80 >> (i % 8);
            }
        }

        let mut br = BitReader::new(&data, bits.len());
        for &v in &values {
            assert_eq!(br.get_ue_golomb().unwrap(), v);
        }
        assert!(br.get_ue_golomb().is_err());
    }
}
