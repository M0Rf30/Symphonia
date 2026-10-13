// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared `(length, codeword)` prefix-code decoding for the SBR and PS
//! Huffman codebooks.
//!
//! A [`Codebook`] pairs a table of MSB-first `(length, codeword)` entries
//! with its largest-absolute-value (`lav`) offset and a lazily built binary
//! decoding tree, so decoding takes one step per bit instead of a scan of
//! the whole table per bit.

use std::sync::OnceLock;

use super::bits::BitReader;

/// The longest codeword across every SBR and PS Huffman table is 20 bits.
/// Decoding refuses to read past this many bits without a match (a
/// malformed bitstream would otherwise loop until the reader runs dry).
pub const HUFF_MAX_CODE_LEN: u32 = 20;

/// A child is the index of an inner node, or `LEAF | index` of a codebook entry, or `NONE` for a
/// prefix that is no codeword.
const LEAF: u16 = 0x8000;
const NONE: u16 = 0;

/// The binary decoding tree of a codebook. Node 0 is the root.
struct DecodeTree {
    nodes: Vec<[u16; 2]>,
}

impl DecodeTree {
    fn new(table: &[(u8, u32)]) -> Self {
        let mut nodes = vec![[NONE; 2]];

        for (idx, &(len, code)) in table.iter().enumerate() {
            let mut node = 0usize;
            for i in (0..len).rev() {
                let bit = ((code >> i) & 1) as usize;
                if i == 0 {
                    nodes[node][bit] = LEAF | idx as u16;
                }
                else {
                    if nodes[node][bit] == NONE {
                        nodes.push([NONE; 2]);
                        nodes[node][bit] = (nodes.len() - 1) as u16;
                    }
                    node = usize::from(nodes[node][bit]);
                }
            }
        }

        DecodeTree { nodes }
    }
}

/// A Huffman codebook: its table, `lav`, and lazily built decoding tree.
pub struct Codebook {
    table: &'static [(u8, u32)],
    lav: i32,
    tree: OnceLock<DecodeTree>,
}

impl Codebook {
    pub const fn new(table: &'static [(u8, u32)], lav: i32) -> Self {
        Codebook { table, lav, tree: OnceLock::new() }
    }

    #[cfg(test)]
    pub fn lav(&self) -> i32 {
        self.lav
    }

    #[cfg(test)]
    pub fn table(&self) -> &'static [(u8, u32)] {
        self.table
    }

    /// Reads bits MSB-first from `reader` until they match an entry of the codebook, returning
    /// the matching table index minus `lav`, i.e. the signed DPCM delta.
    ///
    /// Returns `err` if no codeword of length up to [`HUFF_MAX_CODE_LEN`] matches or the reader
    /// runs dry. After a prefix that is no codeword, the bits up to the maximum codeword length
    /// are consumed before failing.
    pub fn decode<E: Copy>(&self, reader: &mut BitReader<'_>, err: E) -> Result<i32, E> {
        let tree = self.tree.get_or_init(|| DecodeTree::new(self.table));

        let mut node = 0usize;
        let mut len: u32 = 0;
        loop {
            let bit = reader.read_u32(1).map_err(|_| err)? as usize;
            len += 1;
            let next = tree.nodes[node][bit];
            if next & LEAF != 0 {
                return Ok(i32::from(next & !LEAF) - self.lav);
            }
            if next == NONE {
                while len < HUFF_MAX_CODE_LEN {
                    reader.read_u32(1).map_err(|_| err)?;
                    len += 1;
                }
                return Err(err);
            }
            if len >= HUFF_MAX_CODE_LEN {
                return Err(err);
            }
            node = usize::from(next);
        }
    }
}
