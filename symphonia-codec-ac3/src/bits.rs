// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A minimal MSB-first bit reader that never fails and never panics.
//!
//! Reads past the end of the buffer return zero bits. The caller checks [`BitReader::overrun`]
//! at convenient points (e.g., once per audio block) to detect truncated or corrupt data.

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// The current position in bits from the start of the buffer.
    #[inline]
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Returns true if more bits were consumed than the buffer contains.
    #[inline]
    pub fn overrun(&self) -> bool {
        self.pos > self.data.len() * 8
    }

    /// Reads `n` bits (`n <= 32`) as an unsigned integer.
    #[inline]
    pub fn get(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let mut v = 0u64;
        for i in 0..5 {
            v = (v << 8) | u64::from(self.data.get(byte + i).copied().unwrap_or(0));
        }
        // 40 bits are loaded, up to 7 are consumed by the sub-byte offset, so at most 33 remain.
        let shift = 40 - (self.pos & 7) as u32 - n;
        self.pos += n as usize;
        ((v >> shift) & ((1u64 << n) - 1)) as u32
    }

    /// Reads `n` bits (`1 <= n <= 32`) as a sign-extended two's complement integer.
    #[inline]
    pub fn get_signed(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.get(n);
        symphonia_core::util::bits::sign_extend_leq32_to_i32(v, n)
    }

    #[inline]
    pub fn bit(&mut self) -> bool {
        self.get(1) != 0
    }

    #[inline]
    pub fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }
}

#[cfg(test)]
mod tests {
    use super::BitReader;

    #[test]
    fn verify_reads() {
        let data = [0b1010_1100, 0xFF, 0x00, 0x80];
        let mut br = BitReader::new(&data);
        assert_eq!(br.get(3), 0b101);
        assert_eq!(br.get(5), 0b01100);
        assert_eq!(br.get(12), 0xFF0);
        assert_eq!(br.get_signed(4), 0);
        assert!(br.bit());
        assert!(!br.overrun());
        assert_eq!(br.pos(), 25);
    }

    #[test]
    fn verify_signed_and_overrun() {
        let data = [0xF0];
        let mut br = BitReader::new(&data);
        assert_eq!(br.get_signed(4), -1);
        assert_eq!(br.get(32), 0);
        assert!(br.overrun());
    }

    #[test]
    fn verify_32_bits_unaligned() {
        let data = [0xFF, 0x12, 0x34, 0x56, 0x78, 0x80];
        let mut br = BitReader::new(&data);
        br.skip(7);
        let expect = (1u32 << 31) | (0x12 << 23) | (0x34 << 15) | (0x56 << 7) | 0b011_1100;
        assert_eq!(br.get(32), expect);
    }
}
