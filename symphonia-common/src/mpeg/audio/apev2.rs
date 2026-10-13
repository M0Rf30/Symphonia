// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Detection of an APEv2 tag footer appended to the end of a stream.

/// The length of an APEv2 header or footer.
pub const APEV2_FOOTER_LEN: usize = 32;

/// A validated APEv2 footer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Apev2Footer {
    /// The size of the tag items plus the footer (excluding the header).
    pub size: u64,
    /// True if a header precedes the tag items.
    pub has_header: bool,
}

impl Apev2Footer {
    /// Parse and validate an APEv2 footer from the first 32 bytes of `footer`. Returns `None` if
    /// the preamble is absent, the footer is flagged as a header, or the size is smaller than the
    /// footer itself.
    pub fn parse(footer: &[u8]) -> Option<Self> {
        let footer = footer.get(..APEV2_FOOTER_LEN)?;

        if &footer[..8] != b"APETAGEX" {
            return None;
        }

        let size = u32::from_le_bytes([footer[12], footer[13], footer[14], footer[15]]);
        let flags = u32::from_le_bytes([footer[20], footer[21], footer[22], footer[23]]);

        // Bit 31 indicates the tag contains a header. Bit 29 would indicate this is the header
        // itself, which is not valid for a footer.
        let has_header = flags & (1 << 31) != 0;
        let is_header = flags & (1 << 29) != 0;

        if is_header || (size as usize) < APEV2_FOOTER_LEN {
            return None;
        }

        Some(Apev2Footer { size: u64::from(size), has_header })
    }

    /// The total length of the tag in bytes, including the header if present.
    pub fn total_len(&self) -> u64 {
        self.size + if self.has_header { APEV2_FOOTER_LEN as u64 } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn footer(size: u32, flags: u32) -> [u8; 32] {
        let mut f = [0u8; 32];
        f[..8].copy_from_slice(b"APETAGEX");
        f[12..16].copy_from_slice(&size.to_le_bytes());
        f[20..24].copy_from_slice(&flags.to_le_bytes());
        f
    }

    #[test]
    fn verify_footer_validation() {
        let f = Apev2Footer::parse(&footer(40, 1 << 31)).expect("valid footer");
        assert_eq!(f.total_len(), 72);
        assert_eq!(Apev2Footer::parse(&footer(40, 0)).expect("valid footer").total_len(), 40);
        // Too small, or flagged as a header.
        assert_eq!(Apev2Footer::parse(&footer(8, 0)), None);
        assert_eq!(Apev2Footer::parse(&footer(40, 1 << 29)), None);
        assert_eq!(Apev2Footer::parse(&[0; 32]), None);
    }
}
