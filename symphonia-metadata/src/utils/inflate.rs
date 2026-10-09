// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A small, safe, and size-bounded zlib/DEFLATE (RFC 1950/1951) decompressor.
//!
//! This exists only to support compressed metadata (e.g., ID3v2 compressed frames) without taking
//! on an external dependency. It is optimized for simplicity and robustness against malicious
//! input, not for speed.

/// The maximum number of bits in a Huffman code.
const MAX_BITS: usize = 15;

/// Base lengths for length symbols 257..=285.
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];

/// Extra bits for length symbols 257..=285.
const LEN_EXTRA: [u8; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];

/// Base distances for distance symbols 0..=29.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

/// Extra bits for distance symbols 0..=29.
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The order in which code length code lengths are stored in a dynamic block header.
const CODE_LEN_ORDER: [usize; 19] =
    [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// An LSB-first bit reader.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u32,
    cnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0, buf: 0, cnt: 0 }
    }

    /// Read `n` (<= 16) bits.
    fn bits(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 16);

        while self.cnt < n {
            let byte = *self.data.get(self.pos)?;
            self.pos += 1;
            self.buf |= u32::from(byte) << self.cnt;
            self.cnt += 8;
        }

        let value = self.buf & ((1u32 << n) - 1);
        self.buf >>= n;
        self.cnt -= n;
        Some(value)
    }

    /// Discard any buffered bits up to the next byte boundary.
    fn align(&mut self) {
        self.buf = 0;
        self.cnt = 0;
    }

    /// Read `len` whole bytes. The reader must be byte aligned.
    fn bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        debug_assert_eq!(self.cnt, 0);
        let end = self.pos.checked_add(len)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }
}

/// A canonical Huffman decoding table.
struct Huffman {
    /// The number of codes of each length.
    count: [u16; MAX_BITS + 1],
    /// Symbols ordered by code.
    symbols: Vec<u16>,
}

impl Huffman {
    /// Build a table from a list of code lengths, one per symbol. Over-subscribed sets of lengths
    /// are rejected. Incomplete sets are permitted, but decoding an unassigned code will fail.
    fn new(lengths: &[u8]) -> Option<Huffman> {
        let mut count = [0u16; MAX_BITS + 1];

        for &len in lengths {
            count[usize::from(len)] += 1;
        }

        // Check for an over-subscribed set of lengths.
        let mut left = 1i32;

        for len in 1..=MAX_BITS {
            left <<= 1;
            left -= i32::from(count[len]);

            if left < 0 {
                return None;
            }
        }

        // Offsets into the symbol table for each length.
        let mut offsets = [0u16; MAX_BITS + 2];

        for len in 1..=MAX_BITS {
            offsets[len + 1] = offsets[len] + count[len];
        }

        let mut symbols = vec![0u16; lengths.len()];

        for (symbol, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[usize::from(offsets[usize::from(len)])] = symbol as u16;
                offsets[usize::from(len)] += 1;
            }
        }

        Some(Huffman { count, symbols })
    }

    /// Decode one symbol.
    fn decode(&self, reader: &mut BitReader<'_>) -> Option<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;

        for len in 1..=MAX_BITS {
            code |= reader.bits(1)? as i32;

            let count = i32::from(self.count[len]);

            if code - count < first {
                return self.symbols.get((index + (code - first)) as usize).copied();
            }

            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }

        None
    }
}

/// Decompress the codes of a Huffman-compressed block.
fn inflate_codes(
    reader: &mut BitReader<'_>,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
    max_out: usize,
) -> Option<()> {
    loop {
        let symbol = usize::from(lit.decode(reader)?);

        match symbol {
            0..=255 => {
                if out.len() >= max_out {
                    return None;
                }
                out.push(symbol as u8);
            }
            256 => return Some(()),
            257..=285 => {
                let idx = symbol - 257;
                let len =
                    usize::from(LEN_BASE[idx]) + reader.bits(u32::from(LEN_EXTRA[idx]))? as usize;

                let dsym = usize::from(dist.decode(reader)?);

                if dsym >= DIST_BASE.len() {
                    return None;
                }

                let distance = usize::from(DIST_BASE[dsym])
                    + reader.bits(u32::from(DIST_EXTRA[dsym]))? as usize;

                if distance > out.len() || len > max_out - out.len() {
                    return None;
                }

                // The source and destination may overlap, so copy one byte at a time.
                let start = out.len() - distance;

                for i in 0..len {
                    out.push(out[start + i]);
                }
            }
            _ => return None,
        }
    }
}

/// Read the table definitions of a dynamic Huffman block.
fn read_dynamic_tables(reader: &mut BitReader<'_>) -> Option<(Huffman, Huffman)> {
    let nlen = reader.bits(5)? as usize + 257;
    let ndist = reader.bits(5)? as usize + 1;
    let ncode = reader.bits(4)? as usize + 4;

    if nlen > 286 || ndist > 30 {
        return None;
    }

    let mut lengths = [0u8; 320];

    for &idx in CODE_LEN_ORDER.iter().take(ncode) {
        lengths[idx] = reader.bits(3)? as u8;
    }

    let code_len_table = Huffman::new(&lengths[..19])?;

    let mut lengths = [0u8; 320];
    let mut idx = 0;

    while idx < nlen + ndist {
        let symbol = code_len_table.decode(reader)?;

        if symbol < 16 {
            lengths[idx] = symbol as u8;
            idx += 1;
        }
        else {
            let (prev, repeat) = match symbol {
                16 => {
                    if idx == 0 {
                        return None;
                    }
                    (lengths[idx - 1], 3 + reader.bits(2)? as usize)
                }
                17 => (0, 3 + reader.bits(3)? as usize),
                18 => (0, 11 + reader.bits(7)? as usize),
                _ => return None,
            };

            if idx + repeat > nlen + ndist {
                return None;
            }

            lengths[idx..idx + repeat].fill(prev);
            idx += repeat;
        }
    }

    // The end-of-block code must be present.
    if lengths[256] == 0 {
        return None;
    }

    let lit = Huffman::new(&lengths[..nlen])?;
    let dist = Huffman::new(&lengths[nlen..nlen + ndist])?;

    Some((lit, dist))
}

/// Decompress a raw DEFLATE stream (RFC 1951).
///
/// Returns `None` if the stream is malformed, truncated, or would decompress to more than
/// `max_out` bytes.
pub fn inflate(data: &[u8], max_out: usize) -> Option<Vec<u8>> {
    let mut reader = BitReader::new(data);
    let mut out = Vec::new();

    loop {
        let is_final = reader.bits(1)? == 1;

        match reader.bits(2)? {
            // Stored block.
            0 => {
                reader.align();

                let len = reader.bytes(4)?;
                let len_val = u16::from_le_bytes([len[0], len[1]]);
                let nlen_val = u16::from_le_bytes([len[2], len[3]]);

                if len_val != !nlen_val {
                    return None;
                }

                let block = reader.bytes(usize::from(len_val))?;

                if block.len() > max_out - out.len() {
                    return None;
                }

                out.extend_from_slice(block);
            }
            // Fixed Huffman codes.
            1 => {
                let mut lengths = [0u8; 288];
                lengths[..144].fill(8);
                lengths[144..256].fill(9);
                lengths[256..280].fill(7);
                lengths[280..].fill(8);

                let lit = Huffman::new(&lengths)?;
                let dist = Huffman::new(&[5u8; 30])?;

                inflate_codes(&mut reader, &lit, &dist, &mut out, max_out)?;
            }
            // Dynamic Huffman codes.
            2 => {
                let (lit, dist) = read_dynamic_tables(&mut reader)?;

                inflate_codes(&mut reader, &lit, &dist, &mut out, max_out)?;
            }
            _ => return None,
        }

        if is_final {
            return Some(out);
        }
    }
}

/// Decompress a zlib stream (RFC 1950).
///
/// Returns `None` if the stream is malformed, truncated, requires a preset dictionary, or would
/// decompress to more than `max_out` bytes. The Adler-32 trailer is not verified.
pub fn zlib_decompress(data: &[u8], max_out: usize) -> Option<Vec<u8>> {
    let (cmf, flg) = match data {
        [cmf, flg, ..] => (*cmf, *flg),
        _ => return None,
    };

    // The compression method must be DEFLATE with a window size no larger than 32K, the header
    // checksum must be valid, and preset dictionaries are not supported.
    if cmf & 0x0f != 8 || cmf >> 4 > 7 || (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
        return None;
    }

    if flg & 0x20 != 0 {
        return None;
    }

    inflate(&data[2..], max_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // zlib.compress(b"hello hello hello hello", 9): a fixed Huffman block with a back-reference.
    const FIXED: [u8; 16] = [
        0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27, 0x01, 0x68, 0x03, 0x08,
        0xb1,
    ];

    // A zlib stream with a single stored block holding b"abc".
    const STORED: [u8; 14] =
        [0x78, 0x01, 0x01, 0x03, 0x00, 0xfc, 0xff, 0x61, 0x62, 0x63, 0x02, 0x4d, 0x01, 0x27];

    // zlib.compress(TEXT, 9): a dynamic Huffman block.
    const DYNAMIC: [u8; 98] = [
        0x78, 0xda, 0xb5, 0xcb, 0xc7, 0x15, 0x80, 0x20, 0x10, 0x45, 0xd1, 0x56, 0x7e, 0x05, 0x1c,
        0x73, 0xe8, 0xc2, 0x85, 0x0d, 0xa0, 0x82, 0x62, 0x1a, 0x41, 0x31, 0x55, 0xef, 0x34, 0xe1,
        0xfa, 0xdd, 0x57, 0x0f, 0x0a, 0xd6, 0x9b, 0x76, 0x42, 0xe3, 0xe8, 0x5a, 0xa1, 0xe9, 0xc6,
        0xe8, 0x97, 0x6d, 0x07, 0x9d, 0xca, 0xe1, 0xe0, 0x3c, 0xcb, 0xf7, 0x41, 0x47, 0xbd, 0x40,
        0xfd, 0x1b, 0xae, 0x24, 0xbb, 0xe5, 0x41, 0xc3, 0xe8, 0x32, 0xc7, 0x00, 0x6d, 0x4e, 0xc5,
        0xe9, 0x55, 0x2b, 0x66, 0x63, 0x3d, 0x39, 0x7e, 0xfb, 0x5d, 0x20, 0x08, 0xa3, 0x38, 0x49,
        0xb3, 0xbc, 0x28, 0x3f, 0x75, 0x0c, 0x41, 0x3a,
    ];

    const TEXT: &[u8] = b"The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs. 0123456789";

    #[test]
    fn verify_stored_block() {
        assert_eq!(zlib_decompress(&STORED, 16).as_deref(), Some(&b"abc"[..]));
    }

    #[test]
    fn verify_dynamic_block() {
        assert_eq!(zlib_decompress(&DYNAMIC, 1024).as_deref(), Some(TEXT));
        assert!(zlib_decompress(&DYNAMIC, TEXT.len() - 1).is_none());
        assert!(zlib_decompress(&DYNAMIC[..40], 1024).is_none());
    }

    #[test]
    fn verify_fixed_block() {
        assert_eq!(zlib_decompress(&FIXED, 64).as_deref(), Some(&b"hello hello hello hello"[..]));
    }

    #[test]
    fn verify_output_limit() {
        assert!(zlib_decompress(&FIXED, 22).is_none());
        assert!(zlib_decompress(&FIXED, 23).is_some());
        assert!(zlib_decompress(&STORED, 2).is_none());
    }

    #[test]
    fn verify_malformed() {
        // Empty, bad header, truncated, and a back-reference before the start of the output.
        assert!(zlib_decompress(&[], 64).is_none());
        assert!(zlib_decompress(&[0x78], 64).is_none());
        assert!(zlib_decompress(&[0x78, 0x00, 0x00], 64).is_none());
        assert!(zlib_decompress(&FIXED[..8], 64).is_none());
        assert!(inflate(&[0x03], 64).is_none());
        // Reserved block type 3.
        assert!(inflate(&[0x07], 64).is_none());
        // Fixed block starting with a length/distance pair.
        assert!(inflate(&[0x03, 0x02, 0x00, 0x00], 64).is_none());
    }
}
