// Symphonia Shorten bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MSB-first bit reader with the Rice/Golomb codes Shorten is built from.
//!
//! The code layout follows `get_ur_golomb_shorten` / `get_sr_golomb_shorten` of FFmpeg's
//! `libavcodec/golomb.h` (a unary-coded quotient terminated by a `1` bit, followed by `k` raw
//! remainder bits; the signed variant folds the sign into the low bit and uses `k + 1`).

use std::io;

use symphonia_core::errors::Error;

/// The error every read past the end of the data returns. Distinguished from a malformed
/// stream by [`is_eof`], which lets the demuxer tell "need more bytes" from "corrupt".
pub(crate) fn eof() -> Error {
    Error::IoError(io::Error::from(io::ErrorKind::UnexpectedEof))
}

/// Returns `true` if `err` was produced by [`eof`] (or any other unexpected end of input).
pub(crate) fn is_eof(err: &Error) -> bool {
    matches!(err, Error::IoError(e) if e.kind() == io::ErrorKind::UnexpectedEof)
}

/// A bit reader over a byte slice.
#[derive(Clone)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Position of the next bit to read, in bits from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// Creates a reader positioned at bit `bit_pos` of `data`.
    pub fn new(data: &'a [u8], bit_pos: usize) -> Self {
        BitReader { data, pos: bit_pos }
    }

    /// The position of the next bit, in bits from the start of the data.
    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    /// The number of bits that can still be read.
    pub fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    /// Reads `n` (at most 32) bits.
    pub fn read_bits(&mut self, n: u32) -> Result<u32, Error> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Ok(0);
        }
        if self.bits_left() < n as usize {
            return Err(eof());
        }
        let first = self.pos >> 3;
        let skip = (self.pos & 7) as u32;
        // 5 bytes cover the 7 bits of misalignment plus 32 bits; the bounds check above
        // guarantees every bit that is used lies inside the slice.
        let mut acc = 0u64;
        for i in 0..5 {
            acc = acc << 8 | u64::from(self.data.get(first + i).copied().unwrap_or(0));
        }
        acc = (acc << skip) & 0xff_ffff_ffff;
        self.pos += n as usize;
        Ok((acc >> (40 - n)) as u32)
    }

    /// Reads a unary code: the number of `0` bits before the next `1` bit, which is consumed.
    fn read_unary(&mut self) -> Result<u32, Error> {
        let mut zeros = 0u32;
        loop {
            let Some(&byte) = self.data.get(self.pos >> 3) else {
                return Err(eof());
            };
            let skip = (self.pos & 7) as u32;
            // Left-align the unread bits of the byte; the vacated low bits are zero.
            let bits = byte << skip;
            if bits == 0 {
                zeros = zeros.wrapping_add(8 - skip);
                self.pos += (8 - skip) as usize;
            } else {
                let lz = bits.leading_zeros();
                zeros = zeros.wrapping_add(lz);
                self.pos += lz as usize + 1;
                return Ok(zeros);
            }
        }
    }

    /// Reads an unsigned Rice code with `k` (at most 31) remainder bits.
    ///
    /// The result wraps modulo 2^32 like the 32-bit arithmetic of the reference decoder.
    pub fn golomb_u(&mut self, k: u32) -> Result<u32, Error> {
        let quotient = self.read_unary()?;
        let remainder = self.read_bits(k)?;
        Ok(quotient.wrapping_shl(k).wrapping_add(remainder))
    }

    /// Reads a signed Rice code whose unsigned form uses `k + 1` remainder bits.
    pub fn golomb_s(&mut self, k: u32) -> Result<i32, Error> {
        // FFmpeg keeps the unsigned code in an `int`, so the folding shift is arithmetic.
        let uvar = self.golomb_u(k + 1)? as i32;
        Ok(if uvar & 1 != 0 { !(uvar >> 1) } else { uvar >> 1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bits_across_bytes() {
        let data = [0b1010_1100, 0b0101_0011, 0xff, 0x00, 0x80];
        let mut br = BitReader::new(&data, 3);
        assert_eq!(br.read_bits(5).unwrap(), 0b01100);
        assert_eq!(br.read_bits(8).unwrap(), 0b0101_0011);
        assert_eq!(br.read_bits(16).unwrap(), 0xff00);
        assert_eq!(br.read_bits(1).unwrap(), 1);
        assert_eq!(br.bits_left(), 7);
        assert!(is_eof(&br.read_bits(8).unwrap_err()));
    }

    #[test]
    fn reads_32_bits_unaligned() {
        let data = [0xff, 0x12, 0x34, 0x56, 0x78, 0x00];
        let mut br = BitReader::new(&data, 4);
        assert_eq!(br.read_bits(32).unwrap(), 0xf123_4567);
    }

    #[test]
    fn golomb_codes() {
        // k = 2: "001" + "10" = 2 * 4 + 2 = 10; then "1" + "01" = 1.
        let data = [0b0011_0101, 0b0000_0000];
        let mut br = BitReader::new(&data, 0);
        assert_eq!(br.golomb_u(2).unwrap(), 10);
        assert_eq!(br.golomb_u(2).unwrap(), 1);
        // Signed folding: unsigned 0, 1, 2, 3 -> 0, -1, 1, -2 with k = 0 + 1 remainder bit.
        // Codes "10", "11", "010", "011".
        let data = [0b1011_0100, 0b1100_0000];
        let mut br = BitReader::new(&data, 0);
        let got: Vec<i32> = (0..4).map(|_| br.golomb_s(0).unwrap()).collect();
        assert_eq!(got, [0, -1, 1, -2]);
    }

    #[test]
    fn unary_run_spans_bytes() {
        let data = [0x00, 0x00, 0b0001_0000];
        let mut br = BitReader::new(&data, 0);
        assert_eq!(br.golomb_u(0).unwrap(), 19);
        assert!(is_eof(&BitReader::new(&[0, 0], 0).golomb_u(0).unwrap_err()));
    }
}
