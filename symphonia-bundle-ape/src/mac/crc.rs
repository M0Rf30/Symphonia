// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

use symphonia_core::checksum::Crc32Ieee;
use symphonia_core::io::Monitor;

/// The APE frame checksum: the standard CRC-32 (IEEE) of the PCM bytes, shifted right by one bit.
pub fn ape_crc(data: &[u8]) -> u32 {
    let mut crc = Crc32Ieee::new(!0);
    crc.process_buf_bytes(data);
    !crc.crc() >> 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ape_crc_check_value() {
        assert_eq!(ape_crc(b"123456789"), 0xCBF4_3926 >> 1);
        assert_eq!(ape_crc(b""), 0);
    }
}
