// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// LSB-first bit reader, porting the getbit/getbits macros from unpack3.c.

/// A stream that reads past its end yields zero bits, like the C reference. The window `sr`
/// holds the next `bc` valid bits of the stream (LSB first), and is refilled a whole `u64` at a
/// time; bits above `bc` are either zero or the genuine following bits of the stream.
pub(super) struct Bits<'a> {
    data: &'a [u8],
    ptr: usize,
    bc: u32,
    sr: u64,
}

impl<'a> Bits<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Bits { data, ptr: 0, bc: 0, sr: 0 }
    }

    /// Top the window up to at least 56 valid bits.
    #[inline(always)]
    fn refill(&mut self) {
        match self.data.get(self.ptr..).and_then(|rest| rest.first_chunk::<8>()) {
            Some(chunk) => {
                self.sr |= u64::from_le_bytes(*chunk) << self.bc;
                self.ptr += ((63 - self.bc) >> 3) as usize;
                self.bc |= 56;
            }
            None => self.refill_slow(),
        }
    }

    /// Refill near the end of the data: byte by byte, padding with zeros.
    #[cold]
    #[inline(never)]
    fn refill_slow(&mut self) {
        while self.bc <= 56 {
            let byte = match self.data.get(self.ptr) {
                Some(&b) => {
                    self.ptr += 1;
                    b
                }
                None => 0,
            };
            self.sr |= u64::from(byte) << self.bc;
            self.bc += 8;
        }
    }

    #[inline(always)]
    pub(super) fn getbit(&mut self) -> u32 {
        if self.bc == 0 {
            self.refill();
        }
        let bit = (self.sr & 1) as u32;
        self.sr >>= 1;
        self.bc -= 1;
        bit
    }

    /// Read `nbits` (at most 32) bits, LSB first.
    #[inline(always)]
    pub(super) fn getbits(&mut self, nbits: u32) -> u32 {
        debug_assert!(nbits <= 32);
        if nbits == 0 {
            return 0;
        }
        if self.bc < nbits {
            self.refill();
        }
        let val = (self.sr & ((1u64 << nbits) - 1)) as u32;
        self.sr >>= nbits;
        self.bc -= nbits;
        val
    }

    /// Count (and consume) the 1 bits up to `max` (at most 33), and, if fewer than `max` were
    /// found, the 0 bit that ends them. Equivalent to `while n < max && getbit() == 1 { n += 1 }`.
    #[inline(always)]
    pub(super) fn count_ones(&mut self, max: u32) -> u32 {
        debug_assert!(max <= 33);
        if self.bc < 40 {
            self.refill();
        }
        let ones = (!self.sr).trailing_zeros();
        let n = if ones >= max { max } else { ones + 1 };
        self.sr >>= n;
        self.bc -= n;
        ones.min(max)
    }
}

/// The number of bits needed to represent `n` (0 for 0).
#[inline(always)]
pub(super) fn count_bits(n: u32) -> u32 {
    32 - n.leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lsb_first_and_pads_with_zeros() {
        let data = [0b1010_0101, 0xff];
        let mut bs = Bits::new(&data);
        assert_eq!(bs.getbit(), 1);
        assert_eq!(bs.getbits(3), 0b010);
        assert_eq!(bs.getbits(8), 0b1111_1010);
        assert_eq!(bs.getbits(4), 0b1111);
        assert_eq!(bs.getbits(32), 0);
    }

    #[test]
    fn counts_bits() {
        assert_eq!(count_bits(0), 0);
        assert_eq!(count_bits(1), 1);
        assert_eq!(count_bits(u32::MAX), 32);
    }
}
