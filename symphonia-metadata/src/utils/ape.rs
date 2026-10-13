// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utilities for the layout of APEv2 tags that is shared by the APE and ID3v2 metadata readers.

/// The size of an APE tag header or footer in bytes.
pub const HEADER_LEN: u64 = 32;

/// Flag: the tag has a header (set in both the header and the footer).
pub const FLAG_HAS_HEADER: u32 = 1 << 31;
/// Flag: the tag has a footer (set in both the header and the footer).
pub const FLAG_HAS_FOOTER: u32 = 1 << 30;
/// Flag: this block is the header, not the footer.
pub const FLAG_IS_HEADER: u32 = 1 << 29;

/// The tag layout flags of an APEv2 tag header or footer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ApeTagFlags {
    /// The tag has a header.
    pub has_header: bool,
    /// The tag has a footer.
    pub has_footer: bool,
    /// The block the flags were read from is the header.
    pub is_header: bool,
}

impl ApeTagFlags {
    /// Decode the tag layout flags from the flags field of a header or footer.
    pub fn from_bits(flags: u32) -> Self {
        Self {
            has_header: flags & FLAG_HAS_HEADER != 0,
            has_footer: flags & FLAG_HAS_FOOTER != 0,
            is_header: flags & FLAG_IS_HEADER != 0,
        }
    }

    /// Get the length of the tag, in bytes, from the size field of its header or footer. The size
    /// field includes the items and the footer, but not the header.
    pub fn tag_len(&self, size: u64) -> u64 {
        size + if self.has_header { HEADER_LEN } else { 0 }
    }
}
