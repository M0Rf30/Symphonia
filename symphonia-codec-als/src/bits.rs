// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A big-endian (MSB first) bit reader that, like FFmpeg's `GetBitContext`, never fails: reads
//! past the end of the buffer return zero bits. The caller checks [`Bits::left`] when it matters.
//! The position can be moved backwards, which the BGMC decoder needs.

pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }

    /// The number of bits read so far.
    #[allow(dead_code)]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// The number of bits left. Negative if the reader ran past the end of the buffer.
    pub fn left(&self) -> i64 {
        (self.data.len() as i64) * 8 - self.pos as i64
    }

    pub fn get1(&mut self) -> u32 {
        let byte = self.data.get(self.pos >> 3).copied().unwrap_or(0);
        let bit = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        u32::from(bit)
    }

    /// Read `n <= 32` bits. Reading 0 bits returns 0.
    pub fn get(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let first = self.pos >> 3;
        let mut window = 0u64;
        for i in 0..5 {
            window = (window << 8) | u64::from(self.data.get(first + i).copied().unwrap_or(0));
        }
        let shift = 40 - (self.pos & 7) as u32 - n;
        self.pos += n as usize;
        ((window >> shift) & ((1u64 << n) - 1)) as u32
    }

    /// Read `n <= 32` bits as a two's complement signed value.
    pub fn get_signed(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.get(n);
        let shift = 32 - n;
        ((v << shift) as i32) >> shift
    }

    /// Read a unary code of at most `len` bits: the number of 1 bits before the terminating 0 bit
    /// (which is consumed unless `len` ones were read). `len <= 0` reads nothing.
    pub fn get_unary(&mut self, len: i64) -> u32 {
        let mut i = 0i64;
        while i < len && self.get1() != 0 {
            i += 1;
        }
        i as u32
    }

    pub fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// Move the position back by `n` bits.
    pub fn rewind(&mut self, n: usize) {
        self.pos = self.pos.saturating_sub(n);
    }

    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// Read a Rice codeword with parameter `k` (FFmpeg `decode_rice`).
    pub fn rice(&mut self, k: u32) -> i32 {
        let max = self.left() - i64::from(k);
        let mut q = self.get_unary(max);
        let r = if k != 0 { self.get1() != 0 } else { q & 1 == 0 };

        if k > 1 {
            q <<= k - 1;
            q = q.wrapping_add(self.get(k - 1));
        }
        else if k == 0 {
            q >>= 1;
        }

        if r { q as i32 } else { !q as i32 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_get() {
        let mut b = Bits::new(&[0b1010_0101, 0xff, 0x00, 0x80]);
        assert_eq!(b.get(3), 0b101);
        assert_eq!(b.get(5), 0b00101);
        assert_eq!(b.get(12), 0xff0);
        assert_eq!(b.get(32), 0x0800_0000);
        assert_eq!(b.left(), -20);
    }

    #[test]
    fn verify_signed_and_unary() {
        let mut b = Bits::new(&[0b1110_1111, 0b1000_0000]);
        assert_eq!(b.get_unary(8), 3);
        assert_eq!(b.get_signed(4), -1);
        assert_eq!(b.get_unary(2), 1);
    }

    #[test]
    fn verify_rice() {
        // k = 0: unary 110 -> q = 2, r = !(q & 1) = 1, value q >> 1 = 1.
        let mut b = Bits::new(&[0b1100_0000]);
        assert_eq!(b.rice(0), 1);
        // k = 1: unary 0 -> q = 0, sign bit 1 -> value 0, then q = 0 with sign 0 -> !0 = -1.
        let mut b = Bits::new(&[0b0100_0000]);
        assert_eq!(b.rice(1), 0);
        let mut b = Bits::new(&[0b0000_0000]);
        assert_eq!(b.rice(1), -1);
    }
}
