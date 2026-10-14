// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Checksums used by MLP and TrueHD.
//!
//! MLP's checksums are CRCs whose final element is XORed into the result instead of being fed
//! through the CRC (see `ff_mlp_checksum8`, `ff_mlp_checksum16` and `ff_mlp_restart_checksum` in
//! FFmpeg's `mlp.c`).

/// MSB-first (non-reflected) 8-bit CRC table with polynomial `poly`.
const fn crc8_table(poly: u8) -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut j = 0;
        while j < 8 {
            c = if c & 0x80 != 0 { (c << 1) ^ poly } else { c << 1 };
            j += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// MSB-first (non-reflected) 16-bit CRC table with polynomial `poly`.
const fn crc16_table(poly: u16) -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut j = 0;
        while j < 8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ poly } else { c << 1 };
            j += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_63: [u8; 256] = crc8_table(0x63);
static CRC_1D: [u8; 256] = crc8_table(0x1d);
static CRC_2D: [u16; 256] = crc16_table(0x002d);

fn crc8(table: &[u8; 256], init: u8, buf: &[u8]) -> u8 {
    buf.iter().fold(init, |crc, &b| table[usize::from(crc ^ b)])
}

/// Major sync checksum. `buf` is the major sync with its trailing 16-bit checksum word removed
/// from the CRC input: the CRC covers `buf[..len - 2]` and is XORed with the (little-endian)
/// last two bytes of `buf`.
///
/// The result is byte-swapped relative to a conventional MSB-first CRC-16 to match the
/// on-disk convention.
pub(crate) fn checksum16(buf: &[u8]) -> u16 {
    let (data, last) = buf.split_at(buf.len() - 2);

    let crc =
        data.iter().fold(0u16, |crc, &b| CRC_2D[usize::from((crc >> 8) as u8 ^ b)] ^ (crc << 8));

    crc.swap_bytes() ^ u16::from_le_bytes([last[0], last[1]])
}

/// Substream checksum: a CRC over `buf[..len - 1]` XORed with the last byte of `buf`.
pub(crate) fn checksum8(buf: &[u8]) -> u8 {
    let (data, last) = buf.split_at(buf.len() - 1);
    // crc_63[0xa2] == 0x3c
    crc8(&CRC_63, 0x3c, data) ^ last[0]
}

/// Restart header checksum. The header is `bit_size` bits long and starts 2 bits into `buf`.
/// Returns `None` if `buf` is too short to contain the header.
pub(crate) fn restart_checksum(buf: &[u8], bit_size: usize) -> Option<u8> {
    let num_bytes = (bit_size + 2) / 8;

    if num_bytes < 2 {
        return None;
    }

    let mut crc = CRC_1D[usize::from(buf.first()? & 0x3f)];
    crc = crc8(&CRC_1D, crc, buf.get(1..num_bytes - 1)?);
    crc ^= buf.get(num_bytes - 1)?;

    let rem = (bit_size + 2) & 7;

    if rem > 0 {
        let mut crc16 = u16::from(crc);
        let last = *buf.get(num_bytes)?;

        for i in 0..rem {
            crc16 <<= 1;
            if crc16 & 0x100 != 0 {
                crc16 ^= 0x11d;
            }
            crc16 ^= u16::from((last >> (7 - i)) & 1);
        }

        crc = crc16 as u8;
    }

    Some(crc)
}

/// XOR of all bytes.
pub(crate) fn parity(buf: &[u8]) -> u8 {
    buf.iter().fold(0, |acc, &b| acc ^ b)
}
