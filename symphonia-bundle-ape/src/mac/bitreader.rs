// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

//! Byte reader for Monkey's Audio compressed frame data.
//!
//! The compressed stream is a sequence of little-endian 32-bit words that are consumed most
//! significant byte first, so within every group of four bytes the stream order is the reverse of
//! the file order. Every read after the start of a frame is byte aligned, so the stream is
//! addressed in bytes: stream byte `i` is file byte `i ^ 3`. Reads past the end of the data
//! return zero, as if the data was followed by zero padding (and the last, partial, group of four
//! bytes was padded with zero bytes at its end).

/// A reader over the stream bytes of one compressed frame.
pub struct ByteReader<'a> {
    raw: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    /// Create a reader over `raw`, starting `skip_bytes` bytes into the stream.
    pub fn new(raw: &'a [u8], skip_bytes: u32) -> Self {
        // The stream position used to be a 32-bit bit index, so the byte offset wraps at 2^29.
        ByteReader { raw, pos: (skip_bytes & 0x1fff_ffff) as usize }
    }

    /// Read the next stream byte.
    #[inline(always)]
    pub fn next_byte(&mut self) -> u32 {
        let byte = self.raw.get(self.pos ^ 3).copied().unwrap_or(0);
        self.pos = self.pos.wrapping_add(1);
        u32::from(byte)
    }

    /// Read the next four stream bytes as a big-endian 32-bit value.
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        let b0 = self.next_byte();
        let b1 = self.next_byte();
        let b2 = self.next_byte();
        let b3 = self.next_byte();
        (b0 << 24) | (b1 << 16) | (b2 << 8) | b3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_order_within_u32() {
        // File bytes [0xAB, 0xCD, 0xEF, 0x12] are the little-endian word 0x12EFCDAB, which is
        // read most significant byte first: 0x12, 0xEF, 0xCD, 0xAB.
        let mut br = ByteReader::new(&[0xAB, 0xCD, 0xEF, 0x12], 0);
        assert_eq!(br.next_byte(), 0x12);
        assert_eq!(br.next_byte(), 0xEF);
        assert_eq!(br.next_byte(), 0xCD);
        assert_eq!(br.next_byte(), 0xAB);
    }

    #[test]
    fn u32_is_the_little_endian_word() {
        let mut br = ByteReader::new(&[0x78, 0x56, 0x34, 0x12, 0xAA, 0xBB, 0xCC, 0xDD], 0);
        assert_eq!(br.next_u32(), 0x1234_5678);
        assert_eq!(br.next_u32(), 0xDDCC_BBAA);
    }

    #[test]
    fn u32_across_a_word_boundary() {
        // Skipping one byte reads the stream bytes 0x34, 0x56, 0x78 of the first word and the
        // first byte (0xDD) of the second.
        let mut br = ByteReader::new(&[0x78, 0x56, 0x34, 0x12, 0xAA, 0xBB, 0xCC, 0xDD], 1);
        assert_eq!(br.next_u32(), 0x3456_78DD);
    }

    #[test]
    fn partial_last_word_is_padded_at_its_end() {
        // Two file bytes [0xAB, 0xCD] form the word 0x0000CDAB: the stream reads 0, 0, 0xCD, 0xAB.
        let mut br = ByteReader::new(&[0xAB, 0xCD], 0);
        assert_eq!(br.next_byte(), 0);
        assert_eq!(br.next_byte(), 0);
        assert_eq!(br.next_byte(), 0xCD);
        assert_eq!(br.next_byte(), 0xAB);
    }

    #[test]
    fn reads_past_the_end_are_zero() {
        let mut br = ByteReader::new(&[1, 2, 3, 4], 0);
        for _ in 0..4 {
            br.next_byte();
        }
        assert_eq!(br.next_byte(), 0);
        assert_eq!(br.next_u32(), 0);
    }
}
