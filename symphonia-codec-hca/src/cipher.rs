// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The 56-bit key cipher (`cipher_init56` of FFmpeg's `libavcodec/hcadec.c`). It is a byte
//! substitution applied to every byte of a frame.

/// The key of an encrypted stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HcaKey {
    /// The 56-bit key code; the upper 8 bits are not used.
    pub key: u64,
    /// The 16-bit subkey; 0 if the stream has none (AWB subkey).
    pub subkey: u16,
}

impl HcaKey {
    pub const fn new(key: u64, subkey: u16) -> Self {
        HcaKey { key, subkey }
    }

    /// The byte substitution table that decrypts the frames of a stream encrypted with this key.
    /// Returns `None` if the key is 0, which does not encrypt anything.
    pub fn cipher_table(&self) -> Option<[u8; 256]> {
        if self.key == 0 {
            return None;
        }

        let mut key = self.key;

        if self.subkey != 0 {
            let subkey = u64::from(self.subkey);
            let mul = (subkey << 16) | (u64::from(!self.subkey) + 2);
            key = key.wrapping_mul(mul);
        }

        Some(init56(key))
    }
}

/// Generates a row of 16 nibbles from a seed byte (`cipher_init56_create_table`).
fn create_table(key: u8) -> [u8; 16] {
    let mul = u32::from(((key & 1) << 3) | 5);
    let add = u32::from((key & 0xe) | 1);

    let mut state = u32::from(key >> 4);
    let mut row = [0; 16];

    for r in &mut row {
        state = (state * mul + add) & 0xf;
        *r = state as u8;
    }

    row
}

fn init56(mut keycode: u64) -> [u8; 256] {
    // The key code is stored minus one.
    keycode = keycode.saturating_sub(1);

    let mut kc = [0u8; 8];
    for k in &mut kc[..7] {
        *k = keycode as u8;
        keycode >>= 8;
    }

    let seed = [
        kc[1],
        kc[1] ^ kc[6],
        kc[2] ^ kc[3],
        kc[2],
        kc[2] ^ kc[1],
        kc[3] ^ kc[4],
        kc[3],
        kc[3] ^ kc[2],
        kc[4] ^ kc[5],
        kc[4],
        kc[4] ^ kc[3],
        kc[5] ^ kc[6],
        kc[5],
        kc[5] ^ kc[4],
        kc[6] ^ kc[1],
        kc[6],
    ];

    let mut base = [0u8; 256];
    let base_r = create_table(kc[0]);

    for (r, &s) in seed.iter().enumerate() {
        let base_c = create_table(s);
        let nb = base_r[r] << 4;
        for (c, &nibble) in base_c.iter().enumerate() {
            base[r * 16 + c] = nb | nibble;
        }
    }

    // The final shuffle: 0 and 0xff map to themselves.
    let mut cipher = [0u8; 256];
    let mut x = 0usize;
    let mut pos = 1usize;

    for _ in 0..256 {
        x = (x + 17) & 0xff;
        if base[x] != 0 && base[x] != 0xff && pos < 256 {
            cipher[pos] = base[x];
            pos += 1;
        }
    }

    cipher[0] = 0;
    cipher[0xff] = 0xff;
    cipher
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn table_is_a_permutation() {
        // A key that does not produce a permutation would not be invertible by the encoder.
        for (key, subkey) in
            [(0x01cc_b9ed_d6a4_009fu64, 0u16), (0xcc_5546_3ae9, 0), (0xcc_5546_3ae9, 0x1234)]
        {
            let table = HcaKey::new(key, subkey).cipher_table().unwrap();
            let mut seen = [false; 256];
            for &b in &table {
                assert!(!seen[b as usize]);
                seen[b as usize] = true;
            }
            assert_eq!((table[0], table[255]), (0, 255));
        }
    }

    #[test]
    fn zero_key_is_no_cipher() {
        assert!(HcaKey::new(0, 0).cipher_table().is_none());
        assert!(HcaKey::new(0, 5).cipher_table().is_none());
    }
}
