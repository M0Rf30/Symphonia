// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::bits::BitReader;
use crate::tables::HUFFMAN_CODES;
use symphonia_core::errors::{Result, decode_error};

/// The longest code in any of the three codebooks.
const MAX_CODE_LEN: u32 = 9;

/// A direct lookup table indexed by the next `MAX_CODE_LEN` bits of the stream. Each entry is
/// `(code_length << 8) | symbol`. An entry of 0 is not a valid code.
type Lut = [u16; 1 << MAX_CODE_LEN];

const fn build_lut(codes: &[(u16, u8); 18]) -> Lut {
    let mut lut = [0u16; 1 << MAX_CODE_LEN];

    let mut sym = 0;
    while sym < 18 {
        let (code, len) = codes[sym];

        if len != 0 {
            let first = (code as usize) << (MAX_CODE_LEN as usize - len as usize);
            let count = 1usize << (MAX_CODE_LEN as usize - len as usize);

            let mut i = 0;
            while i < count {
                lut[first + i] = ((len as u16) << 8) | sym as u16;
                i += 1;
            }
        }

        sym += 1;
    }

    lut
}

static LUTS: [Lut; 3] =
    [build_lut(&HUFFMAN_CODES[0]), build_lut(&HUFFMAN_CODES[1]), build_lut(&HUFFMAN_CODES[2])];

/// Decode one symbol (0..=17) using the codebook `codebook` (1..=3).
#[inline(always)]
pub(crate) fn read_symbol(br: &mut BitReader<'_>, codebook: usize) -> Result<i32> {
    let entry = LUTS[codebook - 1][br.peek(MAX_CODE_LEN) as usize];

    if entry == 0 {
        return decode_error("truehd: invalid huffman code");
    }

    br.skip(u32::from(entry >> 8))?;

    Ok(i32::from(entry & 0xff))
}
