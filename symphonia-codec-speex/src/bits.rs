// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reader of the Speex bit-stream.
//!
//! Ported from libspeex `bits.c` (BSD-3-Clause), see NOTICE. Bits are packed most-significant bit
//! first. Like `SpeexBits`, reading past the end of the packet does not fail: it latches an
//! overflow flag and returns zero from then on, and the decoder checks the flag at the points
//! libspeex does.

pub struct Bits<'a> {
    data: &'a [u8],
    /// Position of the next bit to read.
    pos: usize,
    overflow: bool,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0, overflow: false }
    }

    /// Number of unread bits, or -1 once the stream has overflowed (`speex_bits_remaining`).
    pub fn remaining(&self) -> i32 {
        if self.overflow { -1 } else { (self.data.len() * 8 - self.pos) as i32 }
    }

    /// Reads `n` (at most 32) bits as an unsigned value (`speex_bits_unpack_unsigned`).
    pub fn unpack(&mut self, n: u32) -> u32 {
        if self.pos + n as usize > self.data.len() * 8 {
            self.overflow = true;
        }
        if self.overflow {
            return 0;
        }
        let mut d = 0u32;
        for _ in 0..n {
            let bit = (self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
            d = (d << 1) | u32::from(bit);
            self.pos += 1;
        }
        d
    }

    /// Returns the next bit without consuming it (`speex_bits_peek`).
    pub fn peek(&mut self) -> u32 {
        if self.pos + 1 > self.data.len() * 8 {
            self.overflow = true;
        }
        if self.overflow {
            return 0;
        }
        u32::from((self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1)
    }

    /// Skips `n` bits (`speex_bits_advance`). Skipping before the start of the packet is only
    /// reachable with a corrupt stream and counts as overflow.
    pub fn advance(&mut self, n: i32) {
        let new_pos = self.pos as i64 + i64::from(n);
        if new_pos > (self.data.len() * 8) as i64 || new_pos < 0 || self.overflow {
            self.overflow = true;
            return;
        }
        self.pos = new_pos as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_msb_first_unpack_and_overflow() {
        let mut bits = Bits::new(&[0b1010_0101, 0xff]);
        assert_eq!(bits.remaining(), 16);
        assert_eq!(bits.peek(), 1);
        assert_eq!(bits.unpack(3), 0b101);
        assert_eq!(bits.unpack(7), 0b001_0111);
        assert_eq!(bits.remaining(), 6);
        // Reading past the end latches the overflow flag and yields zero.
        assert_eq!(bits.unpack(7), 0);
        assert_eq!(bits.remaining(), -1);
        assert_eq!(bits.unpack(1), 0);
    }

    #[test]
    fn verify_advance_bounds() {
        let mut bits = Bits::new(&[0; 2]);
        bits.advance(16);
        assert_eq!(bits.remaining(), 0);
        bits.advance(1);
        assert_eq!(bits.remaining(), -1);

        let mut bits = Bits::new(&[0; 2]);
        bits.advance(-4);
        assert_eq!(bits.remaining(), -1);
    }
}
