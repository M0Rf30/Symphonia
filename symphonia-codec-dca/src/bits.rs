// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A big-endian (MSB first) bit reader with "reads past the end return zero" semantics.
//!
//! The DTS bitstream syntax is validated by checking the remaining bit count at well-defined
//! points rather than on every read, so the reader never fails: reads beyond the end of the
//! buffer yield zero bits and the position keeps advancing. [`BitReader::left`] then reports a
//! negative number of remaining bits.

#[derive(Clone)]
pub struct BitReader<'a> {
    buf: &'a [u8],
    /// Current position in bits.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        BitReader { buf, pos: 0 }
    }

    /// The underlying buffer.
    pub fn buf(&self) -> &'a [u8] {
        self.buf
    }

    /// The current position in bits.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// The total size of the buffer in bits.
    pub fn size_in_bits(&self) -> usize {
        self.buf.len() * 8
    }

    /// The number of bits left, negative if the reader has read past the end.
    pub fn left(&self) -> i64 {
        self.size_in_bits() as i64 - self.pos as i64
    }

    /// Peek up to 32 bits without consuming them.
    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }

        let byte = self.pos >> 3;
        let shift = (self.pos & 7) as u32;

        // Gather 5 bytes (40 bits), zero padded past the end of the buffer.
        let mut v = 0u64;
        if byte + 5 <= self.buf.len() {
            for &b in &self.buf[byte..byte + 5] {
                v = (v << 8) | u64::from(b);
            }
        }
        else {
            for i in 0..5 {
                v = (v << 8) | u64::from(self.buf.get(byte + i).copied().unwrap_or(0));
            }
        }

        ((v >> (40 - shift - n)) & ((1u64 << n) - 1)) as u32
    }

    /// Read up to 32 bits.
    #[inline]
    pub fn get(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    /// Read a single bit as a bool.
    #[inline]
    pub fn get1(&mut self) -> bool {
        self.get(1) != 0
    }

    /// Read `n` bits (`n` <= 32) as a two's complement signed integer.
    #[inline]
    pub fn get_signed(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.get(n);
        symphonia_core::util::bits::sign_extend_leq32_to_i32(v, n)
    }

    /// Skip `n` bits.
    #[inline]
    pub fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// Seek forward to the absolute bit position `p`. Fails if `p` is behind the current position
    /// or beyond the end of the buffer.
    pub fn seek(&mut self, p: usize) -> bool {
        if p < self.pos || p > self.size_in_bits() {
            return false;
        }
        self.pos = p;
        true
    }

    /// Set the position to an absolute bit position without any checks.
    pub fn set_pos(&mut self, p: usize) {
        self.pos = p;
    }
}

/// CRC-16 CCITT (polynomial 0x1021, MSB first), as used by the DTS bitstream.
pub fn crc16(init: u16, data: &[u8]) -> u16 {
    let mut crc = init;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_reads() {
        let mut bs = BitReader::new(&[0xa5, 0x5a, 0xff, 0x00, 0x12]);
        assert_eq!(bs.get(4), 0xa);
        assert!(!bs.get1());
        assert_eq!(bs.get(3), 0b101);
        assert_eq!(bs.get(16), 0x5aff);
        assert_eq!(bs.pos(), 24);
        assert_eq!(bs.left(), 16);
        assert_eq!(bs.peek(8), 0x00);
    }

    #[test]
    fn verify_signed_and_overrun() {
        let mut bs = BitReader::new(&[0xf0]);
        assert_eq!(bs.get_signed(4), -1);
        assert_eq!(bs.get_signed(4), 0);

        // Reads past the end yield zeros and the position continues to advance.
        assert_eq!(bs.get(32), 0);
        assert_eq!(bs.left(), -32);
    }

    #[test]
    fn verify_seek() {
        let mut bs = BitReader::new(&[0; 4]);
        bs.skip(8);
        assert!(!bs.seek(4));
        assert!(bs.seek(16));
        assert_eq!(bs.pos(), 16);
        assert!(!bs.seek(33));
    }

    #[test]
    fn verify_crc16() {
        // CRC-16/CCITT-FALSE check value.
        assert_eq!(crc16(0xffff, b"123456789"), 0x29b1);
    }
}
