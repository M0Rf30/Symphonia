// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A decoder of zlib (RFC 1950) streams of DEFLATE (RFC 1951) data, used to undo the zlib
//! `ContentCompression` of Matroska tracks.

use symphonia_core::errors::{Result, decode_error};

/// The maximum size of a decompressed frame. No frame of an audio codec is anywhere close.
const MAX_OUTPUT: usize = 1 << 28;

const MAX_BITS: usize = 15;

/// The base length and the number of extra bits of the length symbols 257 to 285 (RFC 1951
/// section 3.2.5).
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];

/// The base distance and the number of extra bits of the distance symbols 0 to 29.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The order the code lengths of the code length alphabet are stored in.
const CLEN_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit_buf: u32,
    bit_cnt: u32,
}

impl BitReader<'_> {
    fn bits(&mut self, n: u32) -> Result<u32> {
        while self.bit_cnt < n {
            let Some(&byte) = self.data.get(self.pos)
            else {
                return decode_error("mkv: truncated zlib stream");
            };
            self.pos += 1;
            self.bit_buf |= u32::from(byte) << self.bit_cnt;
            self.bit_cnt += 8;
        }
        let value = self.bit_buf & ((1u32 << n) - 1);
        self.bit_buf >>= n;
        self.bit_cnt -= n;
        Ok(value)
    }
}

/// A canonical Huffman code.
struct Huffman {
    /// The number of codes of each length.
    count: [u16; MAX_BITS + 1],
    /// The symbols ordered by code.
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self> {
        let mut count = [0u16; MAX_BITS + 1];
        for &len in lengths {
            count[usize::from(len)] += 1;
        }
        count[0] = 0;

        // Reject over-subscribed codes. Incomplete codes are only valid in the special cases
        // that zlib accepts, which are not distinguished: an unused code simply never matches.
        let mut left = 1i32;
        for &c in &count[1..] {
            left = (left << 1) - i32::from(c);
            if left < 0 {
                return decode_error("mkv: invalid zlib huffman code");
            }
        }

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

        Ok(Huffman { count, symbols })
    }

    fn decode(&self, br: &mut BitReader<'_>) -> Result<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;

        for len in 1..=MAX_BITS {
            code |= br.bits(1)? as i32;
            let count = i32::from(self.count[len]);
            if code - count < first {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }

        decode_error("mkv: invalid zlib huffman symbol")
    }
}

fn inflate_codes(
    br: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<()> {
    loop {
        let symbol = usize::from(lit.decode(br)?);

        match symbol {
            0..=255 => {
                if out.len() >= MAX_OUTPUT {
                    return decode_error("mkv: zlib output too large");
                }
                out.push(symbol as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let sym = symbol - 257;
                let len =
                    usize::from(LENGTH_BASE[sym]) + br.bits(u32::from(LENGTH_EXTRA[sym]))? as usize;

                let dsym = usize::from(dist.decode(br)?);
                if dsym >= DIST_BASE.len() {
                    return decode_error("mkv: invalid zlib distance symbol");
                }
                let d =
                    usize::from(DIST_BASE[dsym]) + br.bits(u32::from(DIST_EXTRA[dsym]))? as usize;

                if d > out.len() {
                    return decode_error("mkv: zlib distance too far back");
                }
                if out.len() + len > MAX_OUTPUT {
                    return decode_error("mkv: zlib output too large");
                }
                // The copy may overlap itself.
                let start = out.len() - d;
                for i in 0..len {
                    out.push(out[start + i]);
                }
            }
            _ => return decode_error("mkv: invalid zlib length symbol"),
        }
    }
}

/// Decompress a zlib stream.
pub(crate) fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>> {
    // The 2 byte header: CMF (method 8, deflate), and FLG (check bits, and a preset dictionary
    // flag which Matroska does not allow).
    if data.len() < 2
        || data[0] & 0x0f != 8
        || ((u16::from(data[0]) << 8) | u16::from(data[1])) % 31 != 0
        || data[1] & 0x20 != 0
    {
        return decode_error("mkv: invalid zlib header");
    }

    let mut br = BitReader { data, pos: 2, bit_buf: 0, bit_cnt: 0 };
    let mut out = Vec::with_capacity(data.len().saturating_mul(2));

    loop {
        let last = br.bits(1)? == 1;

        match br.bits(2)? {
            0 => {
                // Stored block: skip to the byte boundary.
                br.bit_buf = 0;
                br.bit_cnt = 0;
                let Some(hdr) = br.data.get(br.pos..br.pos + 4)
                else {
                    return decode_error("mkv: truncated zlib stream");
                };
                let len = usize::from(u16::from_le_bytes([hdr[0], hdr[1]]));
                let nlen = u16::from_le_bytes([hdr[2], hdr[3]]);
                if nlen != !(len as u16) {
                    return decode_error("mkv: invalid zlib stored block");
                }
                br.pos += 4;
                let Some(bytes) = br.data.get(br.pos..br.pos + len)
                else {
                    return decode_error("mkv: truncated zlib stream");
                };
                if out.len() + len > MAX_OUTPUT {
                    return decode_error("mkv: zlib output too large");
                }
                out.extend_from_slice(bytes);
                br.pos += len;
            }
            1 => {
                // Fixed Huffman codes (RFC 1951 section 3.2.6).
                let mut lengths = [8u8; 288];
                lengths[144..256].fill(9);
                lengths[256..280].fill(7);
                let lit = Huffman::new(&lengths)?;
                let dist = Huffman::new(&[5u8; 30])?;
                inflate_codes(&mut br, &mut out, &lit, &dist)?;
            }
            2 => {
                let nlen = br.bits(5)? as usize + 257;
                let ndist = br.bits(5)? as usize + 1;
                let ncode = br.bits(4)? as usize + 4;
                if nlen > 286 || ndist > 30 {
                    return decode_error("mkv: invalid zlib dynamic block");
                }

                let mut clens = [0u8; 19];
                for &idx in &CLEN_ORDER[..ncode] {
                    clens[idx] = br.bits(3)? as u8;
                }
                let clen_code = Huffman::new(&clens)?;

                let mut lengths = [0u8; 286 + 30];
                let mut i = 0;
                while i < nlen + ndist {
                    let sym = clen_code.decode(&mut br)?;
                    if sym < 16 {
                        lengths[i] = sym as u8;
                        i += 1;
                    }
                    else {
                        let (value, rep) = match sym {
                            16 => {
                                if i == 0 {
                                    return decode_error("mkv: invalid zlib code lengths");
                                }
                                (lengths[i - 1], 3 + br.bits(2)? as usize)
                            }
                            17 => (0, 3 + br.bits(3)? as usize),
                            _ => (0, 11 + br.bits(7)? as usize),
                        };
                        if i + rep > nlen + ndist {
                            return decode_error("mkv: invalid zlib code lengths");
                        }
                        lengths[i..i + rep].fill(value);
                        i += rep;
                    }
                }

                if lengths[256] == 0 {
                    return decode_error("mkv: zlib block has no end-of-block code");
                }

                let lit = Huffman::new(&lengths[..nlen])?;
                let dist = Huffman::new(&lengths[nlen..nlen + ndist])?;
                inflate_codes(&mut br, &mut out, &lit, &dist)?;
            }
            _ => return decode_error("mkv: invalid zlib block type"),
        }

        if last {
            // The Adler-32 trailer is not verified: the container has its own error detection,
            // and a corrupt frame is a decode error for the codec.
            return Ok(out);
        }
    }
}
