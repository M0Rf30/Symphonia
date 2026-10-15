// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bitstream reader: a port of the read side of liblc3 `src/bits.c` and `src/bits.h`.
//!
//! An LC3 frame has two parts: the arithmetic coded data (read forward from the start of the
//! frame, through a 24-bit range decoder), and the plain bits (read backward from the end of the
//! frame, through a 32-bit accumulator). Reading beyond the frame returns zeros; the overflow is
//! detected by [`Bits::check`] once the frame is decoded.

use crate::tables::AcModel;

/// Size of the bits accumulator (`LC3_ACCU_BITS`).
const ACCU_BITS: i32 = 32;

/// Size of the arithmetic coder window, in bits (`LC3_AC_BITS`).
const AC_BITS: i32 = 24;

/// The bitstream of a frame (`struct lc3_bits`, in read mode).
pub struct Bits<'a> {
    buf: &'a [u8],
    /// Forward position of the arithmetic coder (`buffer.p_fw`).
    p_fw: usize,
    /// Backward position of the plain bits reader (`buffer.p_bw`).
    p_bw: usize,
    accu_v: u32,
    accu_n: i32,
    accu_nover: i32,
    low: u32,
    range: u32,
    error: bool,
}

impl<'a> Bits<'a> {
    /// Sets up the reading of a frame (`lc3_setup_bits` with `LC3_BITS_MODE_READ`).
    pub fn new(buf: &'a [u8]) -> Self {
        let mut bits = Bits {
            buf,
            p_fw: 0,
            p_bw: buf.len(),
            accu_v: 0,
            accu_n: ACCU_BITS,
            accu_nover: 0,
            low: 0,
            range: 0xffffff,
            error: false,
        };

        let b0 = bits.ac_get();
        let b1 = bits.ac_get();
        let b2 = bits.ac_get();
        bits.low = (b0 << 16) | (b1 << 8) | b2;

        bits.accu_load();

        bits
    }

    /// Arithmetic coder get byte, 0 on overflow (`ac_get`).
    #[inline]
    fn ac_get(&mut self) -> u32 {
        match self.buf.get(self.p_fw) {
            Some(&b) => {
                self.p_fw += 1;
                u32::from(b)
            }
            None => 0,
        }
    }

    /// Loads the accumulator (`accu_load`).
    #[inline]
    fn accu_load(&mut self) {
        let nbytes = (self.accu_n >> 3).min(self.p_bw as i32);

        self.accu_n -= 8 * nbytes;

        for _ in 0..nbytes {
            self.p_bw -= 1;
            self.accu_v >>= 8;
            self.accu_v |= u32::from(self.buf[self.p_bw]) << (ACCU_BITS - 8);
        }

        if self.accu_n >= 8 {
            self.accu_nover = (self.accu_nover + self.accu_n).min(ACCU_BITS);
            // A shift of the full width is undefined in the reference. The bits are then beyond
            // the end of the frame, which is detected as an overflow by `check`.
            self.accu_v = self.accu_v.checked_shr(self.accu_n as u32).unwrap_or(0);
            self.accu_n = 0;
        }
    }

    /// Returns the number of bits of the range of the arithmetic coder, `1 + log2(range)`
    /// (`ac_get_range_bits`).
    #[inline]
    fn range_bits(&self) -> i32 {
        (32 - self.range.leading_zeros()) as i32
    }

    /// Returns the number of bits left in the bitstream, negative on overflow (`get_bits_left`).
    fn bits_left(&self) -> i32 {
        let end = self.p_bw as i32 + ACCU_BITS / 8;
        let start = self.p_fw as i32 - AC_BITS / 8;
        // The pending bits of the arithmetic coder, with no cache nor carry (in read mode).
        let pending = 26 - self.range_bits();

        8 * (end - start) - (self.accu_n + self.accu_nover + pending)
    }

    /// Returns the number of bits left in the bitstream (`lc3_get_bits_left`).
    pub fn bits_left_clamped(&self) -> i32 {
        self.bits_left().max(0)
    }

    /// Returns `true` if the bitstream is valid: no overflow, no arithmetic coder error
    /// (`lc3_check_bits`).
    pub fn check(&self) -> bool {
        self.bits_left() >= 0 && !self.error
    }

    /// Reads `n` plain bits, 0 to 32 (`lc3_get_bits`).
    #[inline]
    pub fn get_bits(&mut self, n: i32) -> u32 {
        if self.accu_n + n <= ACCU_BITS {
            let v = self.accu_v.checked_shr(self.accu_n as u32).unwrap_or(0) & mask(n);
            self.accu_n += n;
            v
        }
        else {
            self.get_bits_generic(n)
        }
    }

    /// Reads one plain bit (`lc3_get_bit`).
    #[inline]
    pub fn get_bit(&mut self) -> u32 {
        self.get_bits(1)
    }

    /// Reads from 1 to 32 bits, exceeding the capacity of the accumulator
    /// (`lc3_get_bits_generic`).
    fn get_bits_generic(&mut self, n: i32) -> u32 {
        self.accu_load();

        let n1 = (ACCU_BITS - self.accu_n).min(n);
        let mut v = self.accu_v.checked_shr(self.accu_n as u32).unwrap_or(0) & mask(n1);
        self.accu_n += n1;

        let n2 = n - n1;

        if n2 != 0 {
            self.accu_load();

            v |= (self.accu_v.checked_shr(self.accu_n as u32).unwrap_or(0) & mask(n2))
                .checked_shl(n1 as u32)
                .unwrap_or(0);
            self.accu_n += n2;
        }

        v
    }

    /// Reads a symbol of an arithmetic coding model (`lc3_get_symbol`).
    #[inline]
    pub fn get_symbol(&mut self, model: &AcModel) -> usize {
        let range = (self.range >> 10) & 0xffff;

        self.error |= self.low >= (range << 10);
        if self.error {
            self.low = 0;
        }

        let lt = |s: usize, low: u32| low < range * u32::from(model[s].0);

        let mut s: usize = 16;
        let low = self.low;

        if lt(s, low) {
            s >>= 1;
            s = if lt(s, low) { s - 4 } else { s + 4 };
            s = if lt(s, low) { s - 2 } else { s + 2 };
            s = if lt(s, low) { s - 1 } else { s + 1 };
            s -= usize::from(lt(s, low));
        }

        self.low = self.low.wrapping_sub(range * u32::from(model[s].0));
        self.range = range * u32::from(model[s].1);

        if self.range < 0x10000 {
            self.ac_read_renorm();
        }

        s
    }

    /// Arithmetic coder renormalization (`lc3_ac_read_renorm`).
    fn ac_read_renorm(&mut self) {
        while self.range < 0x10000 {
            self.low = ((self.low << 8) | self.ac_get()) & 0xffffff;
            self.range <<= 8;
        }
    }
}

/// Returns a mask of the `n` low bits, `(1u << n) - 1` without the undefined shift of 32.
#[inline]
fn mask(n: i32) -> u32 {
    if n >= 32 { u32::MAX } else { (1u32 << n) - 1 }
}
