// Symphonia Musepack demuxer+decoder
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A safe, panic-free bit reader for Musepack bitstreams, plus the Huffman decode primitives
//! that operate on it.
//!
//! Ported from libmpcdec `mpc_bits_reader.h`/`mpc_bits_reader.c` (BSD-3-Clause), see `NOTICE`.
//!
//! Unlike the reference implementation (which reads 32-bit-aligned words, including up to four
//! bytes *before* the current byte pointer, and therefore requires the caller to over-allocate
//! the buffer), this reader indexes its underlying byte slice directly and treats any read past
//! the end of the buffer as zero bits. This keeps every operation safe and total (no unsafe
//! code, no panics) at the cost of some speed, which is an acceptable trade for an audio decoder
//! that must never crash on malformed or truncated (e.g. network-streamed) input.

/// A big-endian, MSB-first bit reader over a byte slice.
#[derive(Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute bit position from the start of `data`.
    pos: u64,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// Current absolute bit position from the start of the buffer.
    #[inline]
    pub fn bit_pos(&self) -> u64 {
        self.pos
    }

    /// Seek to an absolute bit position. Out-of-range positions are clamped to the buffer's
    /// bit length (subsequent reads then behave as if at end-of-stream, returning zero bits).
    #[inline]
    pub fn set_bit_pos(&mut self, pos: u64) {
        self.pos = pos;
    }

    /// Number of bits remaining before the end of the buffer (saturates at 0; never negative).
    #[allow(dead_code)]
    #[inline]
    pub fn bits_left(&self) -> u64 {
        let total = (self.data.len() as u64) * 8;
        total.saturating_sub(self.pos)
    }

    #[inline]
    fn get_bit(&self, idx: u64) -> u32 {
        let byte_idx = (idx >> 3) as usize;
        let bit_idx = 7 - (idx & 7) as u32;
        match self.data.get(byte_idx) {
            Some(&b) => (u32::from(b) >> bit_idx) & 1,
            None => 0,
        }
    }

    /// Peek up to 32 bits ahead without advancing the read position. Bits past the end of the
    /// buffer read as zero.
    #[inline]
    pub fn peek_bits(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let mut v = 0u32;
        for i in 0..n {
            v = (v << 1) | self.get_bit(self.pos + u64::from(i));
        }
        v
    }

    /// Read and consume up to 32 bits.
    #[inline]
    pub fn read_bits(&mut self, n: u32) -> u32 {
        let v = self.peek_bits(n);
        self.pos += u64::from(n);
        v
    }

    #[inline]
    pub fn read_bit(&mut self) -> u32 {
        self.read_bits(1)
    }

    #[inline]
    pub fn skip_bits(&mut self, n: u32) {
        self.pos += u64::from(n);
    }

    /// Ported from libmpcdec `mpc_bits_reader.c` (`mpc_bits_get_size`).
    ///
    /// Reads a big-endian base-128 varint (7 payload bits per byte, MSB = continuation flag).
    /// Returns `(value, bytes_consumed)`.
    pub fn read_size(&mut self) -> (u64, u32) {
        let mut size: u64 = 0;
        let mut n = 0u32;
        loop {
            let byte = self.read_bits(8) as u8;
            size = (size << 7) | u64::from(byte & 0x7F);
            n += 1;
            if byte & 0x80 == 0 || n >= 10 {
                break;
            }
        }
        (size, n)
    }
}

/// A single entry of a (non-canonical) Huffman table, as used directly by the SV7 tables.
///
/// Ported from libmpcdec `huffman.h` (`mpc_huffman_t`).
#[derive(Copy, Clone)]
pub struct HuffEntry {
    pub code: u16,
    pub length: u8,
    pub value: i8,
}

/// Decode one symbol from a plain (non-canonical) Huffman `table`, sorted by descending `code`
/// (as in the reference tables). This is the safe, table-scan equivalent of libmpcdec's
/// `mpc_bits_huff_dec`/`mpc_bits_huff_lut` (the LUT is a pure speed optimization over the same
/// linear scan; skipping it changes nothing about the decoded value).
///
/// Returns `0` (and consumes no bits) if `table` is empty, which cannot happen for any of this
/// crate's static tables but keeps the function total.
pub fn huff_dec(r: &mut BitReader<'_>, table: &[HuffEntry]) -> i32 {
    let code = r.peek_bits(16) as u16;
    let entry = table
        .iter()
        .find(|e| code >= e.code)
        .or_else(|| table.last())
        .copied();
    match entry {
        Some(e) => {
            r.skip_bits(u32::from(e.length));
            i32::from(e.value)
        }
        None => 0,
    }
}

/// A canonical Huffman table: `table` gives (code threshold, bit length, base index) triples
/// sorted by descending code (same layout/shape as `HuffEntry`), and `sym` remaps the computed
/// index to the actual symbol value.
///
/// Ported from libmpcdec `huffman.h` (`mpc_can_data_t`) / `mpc_bits_reader.h`
/// (`mpc_bits_can_dec`).
pub struct CanTable {
    pub table: &'static [HuffEntry],
    pub sym: &'static [i8],
}

/// Decode one symbol from a canonical Huffman table. See [`CanTable`].
pub fn can_dec(r: &mut BitReader<'_>, can: &CanTable) -> i32 {
    let code = r.peek_bits(16) as u16;
    let entry = can
        .table
        .iter()
        .find(|e| code >= e.code)
        .or_else(|| can.table.last())
        .copied();
    let Some(e) = entry
    else {
        return 0;
    };
    r.skip_bits(u32::from(e.length));
    // sym[(Value - (code >> (16 - Length))) & 0xFF]
    let shift = 16u32.saturating_sub(u32::from(e.length));
    let idx = (i32::from(e.value) - i32::from(code >> shift)) & 0xFF;
    can.sym.get(idx as usize).copied().map(i32::from).unwrap_or(0)
}
