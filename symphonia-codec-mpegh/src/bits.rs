//! MSB-first bit reader with `FDKpushBack`-style rewind semantics.
//!
//! The FDK bitstream reads big-endian bit order (the first bit of the buffer is
//! the MSB of the first byte). Reading past the end of the buffer yields zero
//! bits (matching the reference `FDK_get32` behaviour). `push_back` rewinds the
//! read cursor; this is used by the arithmetic coders to re-align the bitstream.

#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Current read position in bits from the start of `data`.
    pos: usize,
    /// Total number of bits in `data`.
    len: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            len: data.len() * 8,
        }
    }

    #[inline]
    pub fn pos(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn len_bits(&self) -> usize {
        self.len
    }

    /// Number of bits left to read (never negative; clamped to 0).
    #[inline]
    pub fn remaining(&self) -> usize {
        self.len.saturating_sub(self.pos)
    }

    /// Read a single bit (MSB-first); returns 0 past the end of the buffer.
    #[inline]
    pub fn read_bit(&mut self) -> u32 {
        let b = if self.pos < self.len {
            let byte = self.data[self.pos >> 3];
            let bit = 7 - (self.pos & 7);
            ((byte >> bit) & 1) as u32
        } else {
            0
        };
        self.pos += 1;
        b
    }

    /// Read up to 32 bits (MSB-first), right-aligned. Bits past the end are zero.
    pub fn read_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let mut v: u32 = 0;
        for _ in 0..n {
            v = (v << 1) | self.read_bit();
        }
        v
    }

    /// Read two bits.
    #[inline]
    pub fn read_2bits(&mut self) -> u32 {
        self.read_bits(2)
    }

    /// Rewind the cursor by `n` bits (mirrors `FDKpushBack`).
    #[inline]
    pub fn push_back(&mut self, n: usize) {
        self.pos = self.pos.saturating_sub(n);
    }

    /// Skip forward `n` bits (mirrors `FDKpushFor`).
    #[inline]
    pub fn push_forward(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.len);
    }

    /// Byte-align the cursor forward to the next byte boundary.
    pub fn byte_align(&mut self) {
        let rem = self.pos & 7;
        if rem != 0 {
            self.pos += 8 - rem;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msb_first() {
        let mut br = BitReader::new(&[0b1011_0011, 0b0101_0000]);
        assert_eq!(br.read_bits(4), 0b1011);
        assert_eq!(br.read_bits(4), 0b0011);
        assert_eq!(br.read_bits(4), 0b0101);
        assert_eq!(br.read_bits(4), 0b0000);
    }

    #[test]
    fn push_back() {
        let mut br = BitReader::new(&[0xff, 0xff]);
        let _ = br.read_bits(8);
        br.push_back(8);
        assert_eq!(br.read_bits(8), 0xff);
        // Past-end reads are zero.
        let mut br = BitReader::new(&[0xff]);
        assert_eq!(br.read_bits(16), 0xff00);
    }
}
