// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::{Result, decode_error};

/// A bounds-checked, MSB-first bit reader over a byte slice that exposes its bit position.
///
/// MLP/TrueHD verify the exact number of bits consumed against sizes signalled in the bitstream,
/// therefore the position must be observable. Every read past the end of the buffer is an error.
#[derive(Clone)]
pub(crate) struct BitReader<'a> {
    buf: &'a [u8],
    /// Position of the next bit to be read, in bits.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        BitReader { buf, pos: 0 }
    }

    /// The number of bits consumed so far.
    #[inline(always)]
    pub(crate) fn bit_pos(&self) -> usize {
        self.pos
    }

    /// The total number of bits in the underlying buffer.
    #[inline(always)]
    pub(crate) fn bit_len(&self) -> usize {
        self.buf.len() << 3
    }

    /// Peek at the next `n` (0..=32) bits without consuming them. Bits beyond the end of the
    /// buffer read as 0.
    #[inline(always)]
    pub(crate) fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);

        if n == 0 {
            return 0;
        }

        let byte = self.pos >> 3;
        let shift = (self.pos & 7) as u32;

        let word = match self.buf.get(byte..byte + 8) {
            Some(b) => u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            None => {
                // Slow path near the end of the buffer.
                let mut word = 0u64;
                for i in 0..8 {
                    word = (word << 8) | u64::from(self.buf.get(byte + i).copied().unwrap_or(0));
                }
                word
            }
        };

        ((word << shift) >> (64 - n)) as u32
    }

    /// Consume `n` bits.
    #[inline(always)]
    pub(crate) fn skip(&mut self, n: u32) -> Result<()> {
        if self.pos + n as usize > self.bit_len() {
            return decode_error("truehd: unexpected end of data");
        }
        self.pos += n as usize;
        Ok(())
    }

    /// Read `n` (0..=32) bits as an unsigned integer.
    #[inline(always)]
    pub(crate) fn read(&mut self, n: u32) -> Result<u32> {
        if self.pos + n as usize > self.bit_len() {
            return decode_error("truehd: unexpected end of data");
        }
        let val = self.peek(n);
        self.pos += n as usize;
        Ok(val)
    }

    /// Read a single bit.
    #[inline(always)]
    pub(crate) fn read_bit(&mut self) -> Result<u32> {
        self.read(1)
    }

    /// Read `n` (1..=32) bits as a two's complement signed integer.
    #[inline(always)]
    pub(crate) fn read_signed(&mut self, n: u32) -> Result<i32> {
        debug_assert!(n >= 1);
        let val = self.read(n)?;
        Ok(((val << (32 - n)) as i32) >> (32 - n))
    }
}
