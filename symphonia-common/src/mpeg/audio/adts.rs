// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Raw field extraction for the fixed header of ADTS frames, shared by the ADTS demuxer and the
//! container sniffers. These helpers perform no validation beyond the sync word.

/// The minimum length of an ADTS header (without CRC).
pub const ADTS_MIN_HEADER_LEN: usize = 7;

/// Returns true if the 16-bit word is an ADTS sync word: 12 sync bits, MPEG version (any), and
/// layer bits of zero. The protection bit is ignored.
#[inline(always)]
pub fn is_sync_word(sync: u16) -> bool {
    (sync & 0xfff6) == 0xfff0
}

/// Returns true if `buf` starts with an ADTS sync word.
#[inline]
pub fn starts_with_sync_word(buf: &[u8]) -> bool {
    matches!(buf, [0xff, b1, ..] if b1 & 0xf6 == 0xf0)
}

/// Get the total ADTS frame length in bytes (header, optional CRC, and payload) from the 13-bit
/// frame length field of the header at the start of `buf`. Returns `None` if `buf` is too short to
/// contain the field. The sync word is not checked.
#[inline]
pub fn frame_len(buf: &[u8]) -> Option<usize> {
    match buf {
        [_, _, _, b3, b4, b5, ..] => {
            Some((usize::from(b3 & 0x3) << 11) | (usize::from(*b4) << 3) | usize::from(b5 >> 5))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_adts_fields() {
        assert!(is_sync_word(0xfff1));
        assert!(!is_sync_word(0xfff2));
        assert!(starts_with_sync_word(&[0xff, 0xf1, 0]));
        assert!(!starts_with_sync_word(&[0xff]));
        assert_eq!(frame_len(&[0xff, 0xf1, 0x50, 0x80, 0x02, 0x60, 0xfc]), Some(19));
        assert_eq!(frame_len(&[0xff, 0xf1]), None);
    }
}
