// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Canonical prefix codes described by nibble-packed code lengths.

use symphonia_core::errors::{Result, decode_error};

use crate::bits::BitReader;

/// The longest code, in bits.
const MAX_CODE_LEN: usize = 16;
/// The widest direct lookup, in bits (FFmpeg: `FFMIN(max_bits, 9)`).
const MAX_LUT_BITS: u32 = 9;
/// The largest number of symbols of any RALF code ("no RALF table uses more than that").
const MAX_ELEMS: usize = 644;

/// A prefix code whose symbols are the indices `0..elems` of its code length table.
///
/// The codes are assigned canonically as in FFmpeg's `init_ralf_vlc`: shorter codes first, and
/// within one length by increasing symbol.
pub(crate) struct Vlc {
    /// Direct lookup on the next `lut_bits` bits: `symbol << 4 | length` for codes of at most
    /// `lut_bits` bits, 0 for longer codes.
    lut: Box<[u16]>,
    lut_bits: u32,
    /// The first code of each length.
    first_code: [u32; MAX_CODE_LEN + 1],
    /// The number of codes of each length.
    count: [u16; MAX_CODE_LEN + 1],
    /// The position in `sorted` of the first symbol of each length.
    first_index: [u16; MAX_CODE_LEN + 1],
    /// The symbols ordered by (length, symbol).
    sorted: Box<[u16]>,
}

impl Vlc {
    /// Builds the code of `elems` symbols whose lengths, minus one, are stored in `data` four
    /// bits each, high nibble first.
    pub fn new(data: &[u8], elems: usize) -> Result<Self> {
        if elems == 0 || elems > MAX_ELEMS || data.len() < elems.div_ceil(2) {
            return decode_error("ralf: invalid code table");
        }

        let mut lens = [0u8; MAX_ELEMS];
        let mut count = [0u16; MAX_CODE_LEN + 1];
        let mut max_bits = 0;

        for (i, len) in lens.iter_mut().enumerate().take(elems) {
            let nibble = if i & 1 == 0 { data[i >> 1] >> 4 } else { data[i >> 1] & 0xf };
            *len = nibble + 1;
            count[usize::from(*len)] += 1;
            max_bits = max_bits.max(*len);
        }

        // The first code and symbol position of each length.
        let mut first_code = [0u32; MAX_CODE_LEN + 1];
        let mut first_index = [0u16; MAX_CODE_LEN + 1];

        let mut code = 0u32;
        let mut index = 0u16;

        for len in 1..=MAX_CODE_LEN {
            first_code[len] = code;
            first_index[len] = index;

            // The code must not run out of room.
            if code + u32::from(count[len]) > 1 << len {
                return decode_error("ralf: oversubscribed code");
            }

            code = (code + u32::from(count[len])) << 1;
            index += count[len];
        }

        let lut_bits = u32::from(max_bits).min(MAX_LUT_BITS);
        let mut lut = vec![0u16; 1 << lut_bits];
        let mut sorted = vec![0u16; elems];

        let mut next_code = first_code;
        let mut next_index = first_index;

        for (symbol, &len) in lens.iter().enumerate().take(elems) {
            let len = usize::from(len);

            sorted[usize::from(next_index[len])] = symbol as u16;
            next_index[len] += 1;

            let code = next_code[len];
            next_code[len] += 1;

            if len as u32 <= lut_bits {
                let shift = lut_bits - len as u32;
                let entry = ((symbol as u16) << 4) | len as u16;

                for slot in &mut lut[(code << shift) as usize..((code + 1) << shift) as usize] {
                    *slot = entry;
                }
            }
        }

        Ok(Vlc {
            lut: lut.into_boxed_slice(),
            lut_bits,
            first_code,
            count,
            first_index,
            sorted: sorted.into_boxed_slice(),
        })
    }

    /// Reads one symbol.
    #[inline]
    pub fn read(&self, br: &mut BitReader<'_>) -> Result<usize> {
        let bits = br.peek32();
        let entry = self.lut[(bits >> (32 - self.lut_bits)) as usize];

        if entry != 0 {
            br.skip(usize::from(entry & 0xf));
            return Ok(usize::from(entry >> 4));
        }

        self.read_long(br, bits)
    }

    /// Reads a symbol whose code is longer than the lookup table is wide.
    #[cold]
    fn read_long(&self, br: &mut BitReader<'_>, bits: u32) -> Result<usize> {
        for len in self.lut_bits as usize + 1..=MAX_CODE_LEN {
            // Canonical codes of one length are consecutive.
            let rank = (bits >> (32 - len)).wrapping_sub(self.first_code[len]);

            if rank < u32::from(self.count[len]) {
                br.skip(len);
                return Ok(usize::from(
                    self.sorted[usize::from(self.first_index[len]) + rank as usize],
                ));
            }
        }

        decode_error("ralf: invalid code")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::*;

    /// Assigns the codes of the table as FFmpeg's `init_ralf_vlc` does (a direct port, written
    /// independently of [`Vlc::new`]). Returns the code and length of each symbol.
    fn reference_codes(data: &[u8], elems: usize) -> Vec<(u32, u32)> {
        let lens: Vec<u32> = (0..elems)
            .map(|i| u32::from(if i & 1 == 0 { data[i >> 1] >> 4 } else { data[i >> 1] & 0xf }) + 1)
            .collect();

        let mut counts = [0u32; 18];
        for &l in &lens {
            counts[l as usize] += 1;
        }

        let mut prefixes = [0u32; 18];
        for l in 1..=16 {
            prefixes[l + 1] = (prefixes[l] + counts[l]) << 1;
        }

        lens.iter()
            .map(|&l| {
                let code = prefixes[l as usize];
                prefixes[l as usize] += 1;
                (code, l)
            })
            .collect()
    }

    /// Every symbol of a table decodes from its own code, and the code is complete.
    fn check_table(data: &[u8], elems: usize) {
        let vlc = Vlc::new(data, elems).unwrap();

        let mut kraft = 0u64;

        for (symbol, (code, len)) in reference_codes(data, elems).into_iter().enumerate() {
            kraft += 1 << (16 - len);

            // The code, left aligned, followed by an all-zeros and an all-ones tail.
            for tail in [0u32, 0xffff_ffff] {
                let word = (code << (32 - len)) | (tail >> len);
                let bytes = word.to_be_bytes();
                let mut br = BitReader::new(&bytes, 32);
                assert_eq!(vlc.read(&mut br).unwrap(), symbol);
                assert_eq!(br.bits_left(), 32 - i64::from(len));
            }
        }

        assert_eq!(kraft, 1 << 16, "code is not complete");
    }

    #[test]
    fn verify_all_tables() {
        for set in 0..3 {
            check_table(&FILTER_PARAM_DEF[set], FILTERPARAM_ELEMENTS);
            check_table(&BIAS_DEF[set], BIAS_ELEMENTS);
            check_table(&CODING_MODE_DEF[set], CODING_MODE_ELEMENTS);

            for table in FILTER_COEFFS_DEF[set][..10].iter().flat_map(|t| &t[..11]) {
                check_table(table, FILTER_COEFFS_ELEMENTS);
            }
            for table in &SHORT_CODES_DEF[set][..15] {
                check_table(table, SHORT_CODES_ELEMENTS);
            }
            for table in &LONG_CODES_DEF[set][..125] {
                check_table(table, LONG_CODES_ELEMENTS);
            }
        }
    }

    #[test]
    fn verify_oversubscribed_code_is_rejected() {
        // Three one-bit codes.
        assert!(Vlc::new(&[0x00, 0x00], 3).is_err());
    }

    #[test]
    fn verify_incomplete_code_reports_invalid_patterns() {
        // Two symbols of length 2: patterns 10 and 11 are unassigned.
        let vlc = Vlc::new(&[0x11], 2).unwrap();
        let mut br = BitReader::new(&[0b1000_0000], 8);
        assert!(vlc.read(&mut br).is_err());
        let mut br = BitReader::new(&[0b0100_0000], 8);
        assert_eq!(vlc.read(&mut br).unwrap(), 1);
    }
}
