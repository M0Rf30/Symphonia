// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A canonical Huffman decoder mirroring FFmpeg's `VLC`/`get_vlc2` (see `NOTICE`).

use symphonia_core::errors::decode_error;

use crate::bit::BitReader;

/// A decoded Huffman table. Codes are stored MSB-first; `decode` walks the tree one bit at a time,
/// returning `None` when a truncated code (past the end of the input) is encountered.
pub struct Vlc {
    /// `next0`/`next1`/`symbol`: a binary trie indexed by node.
    next0: Vec<i32>,
    next1: Vec<i32>,
    symbol: Vec<i32>,
    /// Index of the root node.
    root: i32,
}

impl Vlc {
    /// Builds a table from MSB-first `codes` with the corresponding `bits` lengths. `symbol` is the
    /// symbol index (`0..codes.len()`).
    pub fn new(bits: &[u8], codes: &[u32]) -> Self {
        let mut next0 = vec![-1i32; 1];
        let mut next1 = vec![-1i32; 1];
        let mut symbol = vec![-1i32; 1];
        let mut node = 0usize;

        for (sym, (&len, &code)) in bits.iter().zip(codes).enumerate() {
            let len = len as usize;
            if len == 0 {
                continue;
            }
            let mut cur = 0usize;
            for k in (0..len).rev() {
                let bit = (code >> k) & 1;
                let n = if bit == 0 { next0[cur] } else { next1[cur] };
                if n < 0 {
                    node += 1;
                    if bit == 0 {
                        next0[cur] = node as i32;
                    } else {
                        next1[cur] = node as i32;
                    }
                    next0.push(-1);
                    next1.push(-1);
                    symbol.push(-1);
                }
                cur = if bit == 0 { next0[cur] } else { next1[cur] } as usize;
            }
            symbol[cur] = sym as i32;
        }

        Vlc { next0, next1, symbol, root: 0 }
    }

    /// Decodes one symbol. Returns an error on an invalid/truncated code.
    pub fn decode(&self, br: &mut BitReader<'_>) -> Result<i32, symphonia_core::errors::Error> {
        let mut cur = self.root as usize;
        loop {
            let sym = self.symbol[cur];
            if sym >= 0 {
                return Ok(sym);
            }
            // Detect truncation: the stream no longer has bits to walk the tree.
            if br.bits_left() <= 0 {
                return decode_error("ac4: truncated vlc code");
            }
            let bit = br.get1();
            let n = if bit == 0 { self.next0[cur] } else { self.next1[cur] };
            if n < 0 {
                return decode_error("ac4: invalid vlc code");
            }
            cur = n as usize;
        }
    }
}
