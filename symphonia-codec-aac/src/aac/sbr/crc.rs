// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Ported from `oxideav-aac` 0.1.7's `adts_crc.rs` (the `bs_sbr_crc_bits`
// subset only — the ADTS `crc_check()` code lives in this crate's own
// `adts.rs`/`adts_crc.rs` and is unrelated). See
// `symphonia-codec-aac/NOTICE` for the full MIT license text. Clean-room
// implementation of ISO/IEC 14496-3:2009 §4.4.2.8.1.

/// Low-order terms of the SBR CRC generator `x¹⁰ + x⁹ + x⁵ + x⁴ + x + 1`
/// (`G10`, ISO/IEC 14496-3:2009 §4.4.2.8.1).
const SBR_CRC_POLY: u32 = 0x0233;

/// MSB-first CRC-10 shift register for `bs_sbr_crc_bits`.
struct CrcRegister {
    reg: u32,
}

impl CrcRegister {
    /// A register configured for the SBR `bs_sbr_crc_bits` code: 10
    /// bits, generator `G10` (`0x233`), initial value zero.
    fn sbr() -> Self {
        CrcRegister { reg: 0 }
    }

    #[inline]
    fn feed_bit(&mut self, bit: bool) {
        const MASK: u32 = 0x03FF;
        const TOP: u32 = 0x0200;
        let feedback = ((self.reg & TOP) != 0) ^ bit;
        self.reg = (self.reg << 1) & MASK;
        if feedback {
            self.reg ^= SBR_CRC_POLY;
        }
    }

    /// Feed the bit range `[start_bit, end_bit)` of `data`, MSB-first
    /// within each byte. Bits past the end of `data` are fed as zero.
    fn feed_bit_range(&mut self, data: &[u8], start_bit: u64, end_bit: u64) {
        for pos in start_bit..end_bit {
            let byte = (pos / 8) as usize;
            let bit = data.get(byte).is_some_and(|b| b & (0x80 >> (pos % 8)) != 0);
            self.feed_bit(bit);
        }
    }

    fn value(&self) -> u16 {
        self.reg as u16
    }
}

/// Compute `bs_sbr_crc_bits` (`G10`, zero-init) over `data[start_bit..
/// end_bit)`, MSB-first. Mirrors `oxideav_aac::adts_crc::sbr_crc`.
pub(crate) fn sbr_crc(data: &[u8], start_bit: u64, end_bit: u64) -> u16 {
    let mut reg = CrcRegister::sbr();
    reg.feed_bit_range(data, start_bit, end_bit);
    reg.value()
}
