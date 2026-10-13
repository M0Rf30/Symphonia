// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io;

use super::ReadBytes;

/// Reads bytes one at a time, shifting each into the low byte of a 32-bit window, until
/// `is_sync(window)` returns true, then returns the window.
///
/// The window starts as `window` (use `0` to begin a fresh search), and `is_sync` is evaluated on
/// it before any byte is read. `is_sync` is expected to mask the window as required by the sync
/// pattern.
pub fn read_until_sync<B: ReadBytes>(
    reader: &mut B,
    mut window: u32,
    is_sync: impl Fn(u32) -> bool,
) -> io::Result<u32> {
    while !is_sync(window) {
        window = (window << 8) | u32::from(reader.read_u8()?);
    }

    Ok(window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::BufReader;

    #[test]
    fn verify_read_until_sync() {
        let mut reader = BufReader::new(&[0x00, 0x12, 0xff, 0xfb, 0x90]);
        let w = read_until_sync(&mut reader, 0, |w| w & 0xffe0 == 0xffe0).expect("sync");
        assert_eq!(w & 0xffff, 0xfffb);
        assert_eq!(reader.read_u8().expect("next"), 0x90);

        let mut reader = BufReader::new(&[0x00, 0x01]);
        assert!(read_until_sync(&mut reader, 0, |w| w == 0xffff).is_err());
    }
}
