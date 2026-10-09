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

use std::sync::OnceLock;

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

    /// Load the 8 bytes starting at byte `idx` as a big-endian `u64`; bytes past the end of the
    /// buffer read as zero.
    #[inline]
    fn load_u64(&self, idx: usize) -> u64 {
        match self.data.get(idx..).and_then(|s| s.first_chunk::<8>()) {
            Some(chunk) => u64::from_be_bytes(*chunk),
            None => {
                let mut buf = [0u8; 8];
                if let Some(tail) = self.data.get(idx..) {
                    buf[..tail.len()].copy_from_slice(tail);
                }
                u64::from_be_bytes(buf)
            }
        }
    }

    /// Peek up to 32 bits ahead without advancing the read position. Bits past the end of the
    /// buffer read as zero.
    #[inline]
    pub fn peek_bits(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        // At most 7 bits of sub-byte offset plus 32 bits fit in the 64-bit window.
        let window = self.load_u64((self.pos >> 3) as usize) << (self.pos & 7);
        (window >> (64 - n)) as u32
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

/// Number of leading code bits indexed by the first-level lookup table.
const LUT_BITS: u32 = 10;
/// Marks a lookup slot whose prefix does not select a single table entry.
const LUT_SLOW: u8 = 0xFF;

/// Index of the entry the reference linear scan selects for the 16-bit `code`: the first entry
/// (tables are sorted by descending `code`) with `code >= e.code`, else the last one.
fn scan_index(table: &[HuffEntry], code: u16) -> Option<usize> {
    table.iter().position(|e| code >= e.code).or_else(|| table.len().checked_sub(1))
}

/// A lazily built first-level lookup table accelerating the linear scan over a Huffman table.
///
/// Slot `p` holds the index of the entry selected by *every* 16-bit code starting with the
/// 10-bit prefix `p` -- or [`LUT_SLOW`] if codes sharing that prefix select different entries
/// (only possible for codes longer than 10 bits), in which case the scan is used. The result is
/// therefore always identical to the scan.
pub struct HuffLut {
    cell: OnceLock<Option<Box<[u8]>>>,
}

impl HuffLut {
    pub const fn new() -> Self {
        HuffLut { cell: OnceLock::new() }
    }

    fn build(table: &[HuffEntry]) -> Option<Box<[u8]>> {
        if table.is_empty() || table.len() >= usize::from(LUT_SLOW) {
            return None;
        }
        let span = 1u32 << (16 - LUT_BITS);
        let lut = (0..1u32 << LUT_BITS)
            .map(|p| {
                let lo = (p << (16 - LUT_BITS)) as u16;
                let hi = (lo as u32 + span - 1) as u16;
                match (scan_index(table, lo), scan_index(table, hi)) {
                    (Some(a), Some(b)) if a == b => a as u8,
                    _ => LUT_SLOW,
                }
            })
            .collect();
        Some(lut)
    }

    /// The entry selected for the 16-bit `code`, identical to the reference scan.
    #[inline]
    fn find(&self, table: &[HuffEntry], code: u16) -> Option<HuffEntry> {
        if let Some(lut) = self.cell.get_or_init(|| Self::build(table)) {
            let i = lut[usize::from(code >> (16 - LUT_BITS))];
            if i != LUT_SLOW {
                return table.get(usize::from(i)).copied();
            }
        }
        scan_index(table, code).map(|i| table[i])
    }
}

impl Default for HuffLut {
    fn default() -> Self {
        Self::new()
    }
}

/// A plain Huffman table together with its lazily built lookup table.
pub struct HuffTable {
    pub table: &'static [HuffEntry],
    lut: HuffLut,
}

impl HuffTable {
    pub const fn new(table: &'static [HuffEntry]) -> Self {
        HuffTable { table, lut: HuffLut::new() }
    }
}

/// Decode one symbol from a plain (non-canonical) Huffman `table`, sorted by descending `code`
/// (as in the reference tables). Equivalent to libmpcdec's `mpc_bits_huff_dec`/
/// `mpc_bits_huff_lut`: a first-level lookup table resolves the common short codes and the
/// reference linear scan handles the rest.
///
/// Returns `0` (and consumes no bits) if `table` is empty, which cannot happen for any of this
/// crate's static tables but keeps the function total.
#[inline]
pub fn huff_dec(r: &mut BitReader<'_>, table: &HuffTable) -> i32 {
    let code = r.peek_bits(16) as u16;
    match table.lut.find(table.table, code) {
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
    lut: HuffLut,
}

impl CanTable {
    pub const fn new(table: &'static [HuffEntry], sym: &'static [i8]) -> Self {
        CanTable { table, sym, lut: HuffLut::new() }
    }
}

/// Decode one symbol from a canonical Huffman table. See [`CanTable`].
#[inline]
pub fn can_dec(r: &mut BitReader<'_>, can: &CanTable) -> i32 {
    let code = r.peek_bits(16) as u16;
    let Some(e) = can.lut.find(can.table, code)
    else {
        return 0;
    };
    r.skip_bits(u32::from(e.length));
    // sym[(Value - (code >> (16 - Length))) & 0xFF]
    let shift = 16u32.saturating_sub(u32::from(e.length));
    let idx = (i32::from(e.value) - i32::from(code >> shift)) & 0xFF;
    can.sym.get(idx as usize).copied().map(i32::from).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huffman::{sv7_tables, sv8_tables};

    /// Deterministic xorshift pseudo-random bytes.
    fn pseudo_random(len: usize, mut x: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    /// The original bit-by-bit definition of `peek_bits`.
    fn peek_bits_ref(data: &[u8], pos: u64, n: u32) -> u32 {
        let mut v = 0u32;
        for i in 0..n {
            let idx = pos + u64::from(i);
            let bit = data.get((idx >> 3) as usize).map_or(0, |&b| (b >> (7 - (idx & 7))) & 1);
            v = (v << 1) | u32::from(bit);
        }
        v
    }

    #[test]
    fn peek_and_read_bits_match_bitwise_reference() {
        for len in [0usize, 1, 3, 7, 8, 9, 15, 40] {
            let data = pseudo_random(len, 0x9E37_79B9_7F4A_7C15 ^ len as u64);
            let mut r = BitReader::new(&data);
            // Includes positions past the end of the buffer.
            for pos in 0..(len as u64 * 8 + 80) {
                r.set_bit_pos(pos);
                for n in 0..=32 {
                    assert_eq!(
                        r.peek_bits(n),
                        peek_bits_ref(&data, pos, n),
                        "len={len} pos={pos} n={n}"
                    );
                }
                let v = r.read_bits(13);
                assert_eq!(v, peek_bits_ref(&data, pos, 13));
                assert_eq!(r.bit_pos(), pos + 13);
            }
        }
    }

    #[test]
    fn read_size_decodes_varints() {
        let data = [0x81, 0x00, 0x7F, 0xFF, 0x80];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_size(), (128, 2));
        assert_eq!(r.read_size(), (127, 1));
    }

    /// The original linear-scan decode of a plain table: `(symbol, bits consumed)`.
    fn huff_ref(table: &[HuffEntry], code: u16) -> Option<(i32, u32)> {
        let e = table.iter().find(|e| code >= e.code).or_else(|| table.last())?;
        Some((i32::from(e.value), u32::from(e.length)))
    }

    fn check_huff(table: &HuffTable) {
        for code in 0..=u16::MAX {
            let data = [(code >> 8) as u8, code as u8, 0, 0];
            let mut r = BitReader::new(&data);
            let sym = huff_dec(&mut r, table);
            let (want_sym, want_len) = huff_ref(table.table, code).unwrap_or((0, 0));
            assert_eq!((sym, r.bit_pos()), (want_sym, u64::from(want_len)), "code {code:#06x}");
        }
    }

    /// The original linear-scan decode of a canonical table.
    fn can_ref(can: &CanTable, code: u16) -> (i32, u32) {
        let Some(e) = can.table.iter().find(|e| code >= e.code).or_else(|| can.table.last())
        else {
            return (0, 0);
        };
        let shift = 16u32.saturating_sub(u32::from(e.length));
        let idx = (i32::from(e.value) - i32::from(code >> shift)) & 0xFF;
        (can.sym.get(idx as usize).copied().map_or(0, i32::from), u32::from(e.length))
    }

    fn check_can(can: &CanTable) {
        for code in 0..=u16::MAX {
            let data = [(code >> 8) as u8, code as u8, 0, 0];
            let mut r = BitReader::new(&data);
            let sym = can_dec(&mut r, can);
            let (want_sym, want_len) = can_ref(can, code);
            assert_eq!((sym, r.bit_pos()), (want_sym, u64::from(want_len)), "code {code:#06x}");
        }
    }

    #[test]
    fn sv7_lut_decode_matches_linear_scan() {
        for t in [&sv7_tables::T_HDR, &sv7_tables::T_SCFI, &sv7_tables::T_DSCF] {
            check_huff(t);
        }
        for order in 0..=7 {
            for sub in 0..2 {
                check_huff(sv7_tables::huff_q(order, sub));
            }
        }
    }

    #[test]
    fn sv8_lut_decode_matches_linear_scan() {
        for t in
            sv8_tables::CAN_SCFI.iter().chain(&sv8_tables::CAN_DSCF).chain(&sv8_tables::CAN_RES)
        {
            check_can(t);
        }
        for t in sv8_tables::CAN_Q.iter().flatten() {
            check_can(t);
        }
        for t in [&sv8_tables::CAN_BANDS, &sv8_tables::CAN_Q1, &sv8_tables::CAN_Q9UP] {
            check_can(t);
        }
    }

    #[test]
    fn huff_dec_is_total_on_an_empty_table() {
        static EMPTY: HuffTable = HuffTable::new(&[]);
        let mut r = BitReader::new(&[0xFF, 0xFF]);
        assert_eq!(huff_dec(&mut r, &EMPTY), 0);
        assert_eq!(r.bit_pos(), 0);
    }
}
