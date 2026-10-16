// Symphonia TAK Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The least-significant-bit-first bit reader of TAK streams.
//!
//! TAK packs its fields from the least significant bit of each byte (FFmpeg reads the streams
//! with `BITSTREAM_READER_LE`, see `libavcodec/takdec.c`). Reads beyond the end of the data return
//! zero bits, like FFmpeg's zero-padded input buffers; callers check [`BitReader::bits_left`].

/// A reader of bit fields, least significant bit first.
pub struct BitReader<'a> {
    buf: &'a [u8],
    /// The position in bits.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        BitReader { buf, pos: 0 }
    }

    /// The number of bits read so far.
    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    /// The number of bits that are left; negative if more bits were read than the data holds.
    pub fn bits_left(&self) -> i64 {
        (self.buf.len() as i64 * 8) - self.pos as i64
    }

    /// Reads `n <= 32` bits.
    #[inline]
    pub fn get(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let byte = self.pos >> 3;
        let shift = self.pos & 7;
        self.pos += n as usize;

        let word = match self.buf.get(byte..byte + 8) {
            Some(b) => u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            None => {
                let mut w = [0u8; 8];
                if let Some(rest) = self.buf.get(byte..) {
                    w[..rest.len()].copy_from_slice(rest);
                }
                u64::from_le_bytes(w)
            }
        };

        ((word >> shift) & ((1u64 << n) - 1)) as u32
    }

    /// Reads a single bit.
    #[inline]
    pub fn get1(&mut self) -> bool {
        self.get(1) != 0
    }

    /// Reads `n <= 64` bits.
    pub fn get64(&mut self, n: u32) -> u64 {
        if n <= 32 {
            u64::from(self.get(n))
        }
        else {
            let lo = u64::from(self.get(32));
            lo | (u64::from(self.get(n - 32)) << 32)
        }
    }

    /// Reads a two's complement signed field of `n <= 32` bits.
    #[inline]
    pub fn get_signed(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.get(n);
        ((v << (32 - n)) as i32) >> (32 - n)
    }

    /// Skips `n` bits.
    pub fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// Skips to the next byte boundary.
    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// Counts the zero bits up to a one bit, reading at most `max` bits. Returns `max` without
    /// consuming a terminating bit if `max` zero bits were read (FFmpeg's `get_unary(gb, 1, max)`).
    #[inline]
    pub fn get_unary(&mut self, max: u32) -> u32 {
        let mut n = 0;
        while n < max && !self.get1() {
            n += 1;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::BitReader;

    #[test]
    fn verify_lsb_first() {
        let mut br = BitReader::new(&[0b1010_0110, 0xff]);
        assert_eq!(br.get(3), 0b110);
        assert_eq!(br.get(5), 0b10100);
        assert_eq!(br.get(4), 0xf);
        assert_eq!(br.bits_left(), 4);
        assert_eq!(br.get(8), 0xf);
        assert_eq!(br.bits_left(), -4);
    }

    #[test]
    fn verify_signed_and_unary() {
        let mut br = BitReader::new(&[0b0001_1111, 0b0000_0001]);
        assert_eq!(br.get_signed(5), -1);
        assert_eq!(br.get_unary(9), 3);
        assert_eq!(br.get_unary(2), 2);
    }
}
