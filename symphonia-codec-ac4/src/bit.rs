// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A MSB-first bit reader mirroring FFmpeg's `GetBitContext` (see `NOTICE`).

use symphonia_core::errors::Result;

/// Tracks the position within a bitstream.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// Number of bits remaining (may be negative for a truncated stream).
    pub fn bits_left(&self) -> isize {
        (self.data.len() * 8) as isize - self.pos as isize
    }

    /// Number of bits consumed so far.
    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    /// Reads a single bit, returning `0` past the end of the buffer.
    pub fn get1(&mut self) -> u32 {
        let p = self.pos;
        self.pos = p + 1;
        let byte = self.data.get(p >> 3).copied().unwrap_or(0);
        u32::from((byte >> (7 - (p & 7))) & 1)
    }

    /// Reads `bits` bits MSB-first. Bits past the end read as zero.
    pub fn get(&mut self, bits: u32) -> u32 {
        let bits = bits as usize;
        if bits == 0 {
            return 0;
        }
        let mut v: u32 = 0;
        for _ in 0..bits {
            v = (v << 1) | self.get1();
        }
        v
    }

    /// Skips `bits` bits.
    pub fn skip(&mut self, bits: u32) {
        self.pos += bits as usize;
    }

    /// Skips an arbitrary number of bits (FFmpeg `skip_bits_long`).
    pub fn skip_long(&mut self, bits: usize) {
        self.pos = self.pos.saturating_add(bits);
    }

    /// Aligns the read position to the next byte boundary.
    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }
}

/// A variable-bit-length integer: `value` repeated until a zero continuation bit.
pub fn variable_bits(br: &mut BitReader<'_>, bits: u32) -> Result<i32> {
    let mut value = 0i32;
    loop {
        value = value.wrapping_add(br.get(bits) as i32);
        if br.get1() == 0 {
            break;
        }
        value = (value << bits).wrapping_add(1 << bits);
    }
    Ok(value)
}
