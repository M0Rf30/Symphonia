// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

/// CRC-32 (IEEE 802.3, reflected, as used by zlib) lookup tables for slice-by-8 processing.
const fn make_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut c = t[0][i];
        let mut k = 1;
        while k < 8 {
            c = t[0][(c & 0xff) as usize] ^ (c >> 8);
            t[k][i] = c;
            k += 1;
        }
        i += 1;
    }
    t
}

static TABLES: [[u32; 256]; 8] = make_tables();

/// Standard CRC-32 (IEEE) of `data`.
fn crc32(data: &[u8]) -> u32 {
    let t = &TABLES;
    let mut crc = !0u32;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let lo = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
        let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        crc = t[7][(lo & 0xff) as usize]
            ^ t[6][((lo >> 8) & 0xff) as usize]
            ^ t[5][((lo >> 16) & 0xff) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xff) as usize]
            ^ t[2][((hi >> 8) & 0xff) as usize]
            ^ t[1][((hi >> 16) & 0xff) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        crc = t[0][((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// The APE frame checksum: the standard CRC-32 of the PCM bytes, shifted right by one bit.
pub fn ape_crc(data: &[u8]) -> u32 {
    crc32(data) >> 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(ape_crc(b"123456789"), 0xCBF4_3926 >> 1);
    }

    #[test]
    fn test_crc32_long_input_matches_bytewise() {
        let data: Vec<u8> =
            (0..1000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let mut crc = !0u32;
        for &b in &data {
            crc = TABLES[0][((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
        }
        assert_eq!(crc32(&data), !crc);
    }
}
