// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// Reads a big-endian base-128 variable-length integer: 7 payload bits per byte, most-significant
/// group first, with the most-significant bit of each byte set if another byte follows.
///
/// Bytes are fetched one at a time by calling `next`. At most `max_bytes` bytes are consumed.
///
/// Returns `Ok(Some((value, bytes_consumed)))` on success. Returns `Ok(None)` if the integer is
/// not terminated within `max_bytes` bytes, or if its value would not fit in a `u64`. An error
/// from `next` is propagated.
pub fn read_base128_be_u64<E>(
    max_bytes: u32,
    mut next: impl FnMut() -> Result<u8, E>,
) -> Result<Option<(u64, u32)>, E> {
    let mut result: u64 = 0;

    for n in 1..=max_bytes {
        let byte = next()?;

        if result > (u64::MAX >> 7) {
            return Ok(None);
        }

        result = (result << 7) | u64::from(byte & 0x7f);

        if byte & 0x80 == 0 {
            return Ok(Some((result, n)));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::read_base128_be_u64;
    use std::convert::Infallible;

    fn decode(bytes: &[u8], max: u32) -> Option<(u64, u32)> {
        let mut it = bytes.iter().copied();
        read_base128_be_u64::<Infallible>(max, || Ok(it.next().unwrap_or(0))).unwrap()
    }

    #[test]
    fn verify_read_base128_be_u64() {
        assert_eq!(decode(&[0x01], 9), Some((1, 1)));
        assert_eq!(decode(&[0x81, 0x00], 9), Some((128, 2)));
        assert_eq!(decode(&[0xff, 0x7f], 9), Some((0x3fff, 2)));
        // Unterminated within the limit.
        assert_eq!(decode(&[0x80; 9], 9), None);
        // Overflow of u64.
        let mut b = [0xff; 10];
        b[9] = 0x7f;
        assert_eq!(decode(&b, 10), None);
    }
}
