// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A little-endian bit reader and the prefix codes of the QDesign codecs.

use symphonia_core::errors::{Result, decode_error};

/// A reader of bits packed least-significant bit first (FFmpeg's `BITSTREAM_READER_LE`).
///
/// Like FFmpeg's reader over a zero-padded buffer, reads past the end return zero bits. The
/// number of bits left then becomes negative, which the decoders check for explicitly.
pub(crate) struct Bits<'a> {
    buf: &'a [u8],
    /// The declared size in bits. Reads are not limited by it, as in FFmpeg.
    size: usize,
    /// The position in bits.
    pos: usize,
}

impl<'a> Bits<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Bits { buf, size: buf.len() * 8, pos: 0 }
    }

    /// A reader of `buf` that declares a size of `size` bytes, which may differ from the size of
    /// the buffer. Only [`Bits::bits_left`] depends on the declared size.
    pub(crate) fn with_size(buf: &'a [u8], size: usize) -> Self {
        Bits { buf, size: size.saturating_mul(8), pos: 0 }
    }

    /// The number of unread bits. Negative if the end was passed.
    pub(crate) fn bits_left(&self) -> i64 {
        self.size as i64 - self.pos as i64
    }

    /// The number of bits read.
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    /// Reads `n <= 32` bits.
    pub(crate) fn get(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);

        let byte = self.pos >> 3;
        let mut word = 0u64;

        if let Some(rest) = self.buf.get(byte..) {
            for (i, &b) in rest.iter().take(8).enumerate() {
                word |= u64::from(b) << (8 * i);
            }
        }

        let v = (word >> (self.pos & 7)) & ((1u64 << n) - 1);
        self.pos += n as usize;
        v as u32
    }

    /// Reads a single bit.
    pub(crate) fn get1(&mut self) -> u32 {
        self.get(1)
    }

    /// Skips `n` bits.
    pub(crate) fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// The sum of the bytes of the buffer, as in FFmpeg's `skip_label` checksum.
    pub(crate) fn sum_bytes(&self, from: usize, to: usize) -> u32 {
        self.buf
            .get(from.min(self.buf.len())..to.min(self.buf.len()))
            .map(|bytes| bytes.iter().map(|&b| u32::from(b)).sum())
            .unwrap_or(0)
    }
}

/// A prefix code, as built by FFmpeg's `ff_vlc_init_from_lengths` with `VLC_INIT_LE`.
///
/// The codes are assigned in table order, each one being the previous code plus one at its own
/// length, and the first bit read from the stream is the most significant bit of a code. The
/// escape code has the symbol `-1`.
pub(crate) struct Vlc {
    /// Binary tree. A child is 0 if absent, positive if the index of an inner node, and `-(s + 2)`
    /// if a leaf with the symbol `s`.
    nodes: Vec<[i32; 2]>,
}

impl Vlc {
    /// Builds a code from `[symbol + 1, length]` pairs.
    pub(crate) fn new(table: &[[u8; 2]]) -> Result<Self> {
        let mut nodes = vec![[0i32; 2]];
        let mut code = 0u64;

        for &[sym, len] in table {
            let len = u32::from(len);

            if len == 0 || len > 32 || code & ((1u64 << (32 - len)) - 1) != 0 {
                return decode_error("qdesign: invalid code table");
            }

            let value = (code >> (32 - len)) as u32;
            let leaf = -(i32::from(sym) - 1 + 2);

            let mut node = 0usize;
            for i in (0..len).rev() {
                let bit = ((value >> i) & 1) as usize;

                if i == 0 {
                    nodes[node][bit] = leaf;
                }
                else {
                    let next = nodes[node][bit];
                    node = if next > 0 {
                        next as usize
                    }
                    else if next == 0 {
                        nodes.push([0; 2]);
                        let idx = nodes.len() - 1;
                        nodes[node][bit] = idx as i32;
                        idx
                    }
                    else {
                        return decode_error("qdesign: invalid code table");
                    };
                }
            }

            code += 1u64 << (32 - len);
        }

        Ok(Vlc { nodes })
    }

    /// Reads a symbol. Returns -1 for the escape code, and also for a bit pattern that is not a
    /// code (not possible for the complete codes of this crate).
    pub(crate) fn get(&self, bits: &mut Bits<'_>) -> i32 {
        let mut node = 0usize;

        loop {
            let bit = bits.get1() as usize;
            let next = self.nodes.get(node).map_or(0, |n| n[bit]);

            if next < 0 {
                return -(next + 2);
            }
            if next == 0 {
                return -1;
            }
            node = next as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_bit_order() {
        let mut b = Bits::new(&[0b1010_0110, 0xff]);
        assert_eq!(b.get(3), 0b110);
        assert_eq!(b.get1(), 0);
        assert_eq!(b.get(6), 0b11_1010);
        assert_eq!(b.bits_left(), 6);
        assert_eq!(b.get(8), 0x3f);
        assert_eq!(b.bits_left(), -2);
        assert_eq!(b.get(4), 0);
    }

    #[test]
    fn verify_code_assignment() {
        // Lengths 1, 2, 3, 3: codes 0, 10, 110, 111 with the first stream bit as the msb.
        let vlc = Vlc::new(&[[1, 1], [2, 2], [3, 3], [0, 3]]).unwrap();
        let mut b = Bits::new(&[0b1101_1010, 0b1]);
        assert_eq!(vlc.get(&mut b), 0); // 0
        assert_eq!(vlc.get(&mut b), 1); // 1 0
        assert_eq!(vlc.get(&mut b), 2); // 1 1 0
        assert_eq!(vlc.get(&mut b), -1); // 1 1 1: escape
    }
}
