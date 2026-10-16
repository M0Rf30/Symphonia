// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A tiny most-significant-bit-first bit reader for LDAC frames.
//!
//! The LDAC bitstream packs every field from the most-significant bit, exactly like
//! `BitReaderLtr` in `symphonia-core`, but the scale-factor Huffman codes need a bounded
//! look-ahead (peek) before the variable-length code is consumed, which that reader does not
//! expose. This reader mirrors the packing order of the reference `pack_store_ldac`.

pub(crate) struct BitReader<'a> {
    buf: &'a [u8],
    /// Bit position, counting from the most-significant bit of `buf[0]`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        BitReader { buf, pos: 0 }
    }

    /// The number of bits left to read.
    pub(crate) fn bits_left(&self) -> usize {
        self.buf.len().saturating_mul(8).saturating_sub(self.pos)
    }

    /// Reads `n` (at most 16) bits as an unsigned value, MSB first.
    pub(crate) fn read(&mut self, n: u32) -> Option<u32> {
        if n > 16 || self.bits_left() < n as usize {
            return None;
        }

        let mut value = 0u32;
        let mut remaining = n;

        while remaining > 0 {
            let byte = self.buf[self.pos / 8];
            let bit_in_byte = (self.pos % 8) as u32;
            let available = (8 - bit_in_byte).min(remaining);
            let shift = 8 - bit_in_byte - available;
            let chunk = (u32::from(byte) >> shift) & ((1u32 << available) - 1);

            value = (value << available) | chunk;
            self.pos += available as usize;
            remaining -= available;
        }

        Some(value)
    }

    /// Reads `n` (at most 16) bits as a two's-complement signed value.
    pub(crate) fn read_signed(&mut self, n: u32) -> Option<i32> {
        let value = self.read(n)?;
        let shift = 32 - n;
        Some(((value << shift) as i32) >> shift)
    }

    /// Peeks `n` (at most 16) bits without advancing the reader.
    pub(crate) fn peek(&self, n: u32) -> Option<u32> {
        if n > 16 || self.bits_left() < n as usize {
            return None;
        }

        let mut value = 0u32;
        let mut remaining = n;
        let mut pos = self.pos;

        while remaining > 0 {
            let byte = self.buf[pos / 8];
            let bit_in_byte = (pos % 8) as u32;
            let available = (8 - bit_in_byte).min(remaining);
            let shift = 8 - bit_in_byte - available;
            let chunk = (u32::from(byte) >> shift) & ((1u32 << available) - 1);

            value = (value << available) | chunk;
            pos += available as usize;
            remaining -= available;
        }

        Some(value)
    }

    /// Advances the reader to the next multiple of `n` bits.
    pub(crate) fn align(&mut self, n: u32) {
        let n = n as usize;
        let remainder = self.pos % n;

        if remainder != 0 {
            self.pos += n - remainder;
        }
    }
}
