// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::Result;
use symphonia_core::io::ReadBytes;

/// Decode a syncsafe integer from the first 4 bytes of `buf`. The most significant bit of every
/// byte is ignored.
pub fn decode_synchsafe(buf: &[u8]) -> u32 {
    buf.iter().take(4).fold(0, |acc, &x| (acc << 7) | u32::from(x & 0x7f))
}

/// Encode the 28 least significant bits of `value` as a 4 byte syncsafe integer.
#[cfg(any(test, feature = "write"))]
pub fn encode_synchsafe(value: u32) -> [u8; 4] {
    [
        (value >> 21) as u8 & 0x7f,
        (value >> 14) as u8 & 0x7f,
        (value >> 7) as u8 & 0x7f,
        value as u8 & 0x7f,
    ]
}

pub fn read_syncsafe_leq32<B: ReadBytes>(reader: &mut B, bit_width: u8) -> Result<u32> {
    debug_assert!(bit_width <= 32);

    let mut result = 0u32;
    let mut bits_read = 0;

    while bits_read < bit_width {
        // Ensure bits_read never exceeds the bit width which will cause an overflow
        let next_read = (bit_width - bits_read).min(7);
        bits_read += next_read;
        // The mask should have of the bits below 2 ^ nex_read set to 1
        let mask = (1 << next_read) - 1;
        result |= u32::from(reader.read_u8()? & mask) << (bit_width - bits_read);
    }

    Ok(result)
}

pub fn decode_unsynchronisation(buf: &mut [u8]) -> &mut [u8] {
    let len = buf.len();
    let mut src = 0;
    let mut dst = 0;

    // Decode the unsynchronisation scheme in-place.
    while src + 1 < len {
        buf[dst] = buf[src];
        dst += 1;
        src += 1;

        if buf[src - 1] == 0xff && buf[src] == 0x00 {
            src += 1;
        }
    }

    if src < len {
        buf[dst] = buf[src];
        dst += 1;
    }

    &mut buf[..dst]
}

#[cfg(test)]
mod tests {
    use super::{decode_unsynchronisation, read_syncsafe_leq32};
    use symphonia_core::io::BufReader;

    #[test]
    fn verify_read_syncsafe_leq32() {
        let mut stream = BufReader::new(&[3, 4, 80, 1, 15]);
        assert_eq!(101875743, read_syncsafe_leq32(&mut stream, 32).unwrap());

        // Special case: for a bit depth that is not a multiple of 7 such as 32
        // we need to ensure the mask is correct.
        // In this case, the final iteration should read 4 bits and have a mask of 0b0000_1111.
        // 0b0000_1111 has a 0 in 16's place so testing mask & 16 will ensure this is working.
        let mut stream = BufReader::new(&[16, 16, 16, 16, 16]);
        assert_eq!(541098240, read_syncsafe_leq32(&mut stream, 32).unwrap());

        let mut stream = BufReader::new(&[3, 4, 80, 1]);
        assert_eq!(6367233, read_syncsafe_leq32(&mut stream, 28).unwrap());

        let mut stream = BufReader::new(&[3, 4, 80, 1]);
        assert_eq!(0, read_syncsafe_leq32(&mut stream, 0).unwrap());
    }

    #[test]
    fn verify_decode_unsynchronisation() {
        assert_eq!(decode_unsynchronisation(&mut []), &[] as &[u8]);
        assert_eq!(decode_unsynchronisation(&mut [0xff, 0x00]), &[0xff]);
        assert_eq!(
            decode_unsynchronisation(&mut [0x01, 0xff, 0x00, 0x00, 0xff]),
            &[0x01, 0xff, 0x00, 0xff]
        );
    }
}
