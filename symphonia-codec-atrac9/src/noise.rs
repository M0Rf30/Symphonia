// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The noise source of the band extension: FFmpeg's additive lagged Fibonacci generator
//! (`libavutil/lfg.c`) seeded the same way, with its Box-Muller Gaussian output (`av_bmg_get`).
//!
//! The decoder fills extended bands with this noise, so reproducing FFmpeg's output requires the
//! same generator state.

/// The seed used by the ATRAC9 decoder of FFmpeg.
const SEED: u32 = 0xfbad_f00d;

/// MD5 of a single 16-byte message (as used by `av_lfg_init`).
fn md5_16(msg: &[u8; 16]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    // K[i] = floor(2^32 * abs(sin(i + 1))).
    let k = |i: usize| -> u32 { (((i + 1) as f64).sin().abs() * 4294967296.0) as u32 };

    // A single padded block: message, 0x80, zeros, and the bit length (128).
    let mut block = [0u8; 64];
    block[..16].copy_from_slice(msg);
    block[16] = 0x80;
    block[56] = 128;

    let mut m = [0u32; 16];
    for (w, c) in m.iter_mut().zip(block.chunks_exact(4)) {
        *w = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    }

    let (mut a, mut b, mut c, mut d) =
        (0x6745_2301u32, 0xefcd_ab89u32, 0x98ba_dcfeu32, 0x1032_5476u32);

    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let f = f.wrapping_add(a).wrapping_add(k(i)).wrapping_add(m[g]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(f.rotate_left(S[i]));
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&0x6745_2301u32.wrapping_add(a).to_le_bytes());
    out[4..8].copy_from_slice(&0xefcd_ab89u32.wrapping_add(b).to_le_bytes());
    out[8..12].copy_from_slice(&0x98ba_dcfeu32.wrapping_add(c).to_le_bytes());
    out[12..16].copy_from_slice(&0x1032_5476u32.wrapping_add(d).to_le_bytes());
    out
}

/// Additive lagged Fibonacci generator (`AVLFG`).
pub(crate) struct Lfg {
    state: [u32; 64],
    index: u32,
}

impl Lfg {
    pub(crate) fn new() -> Self {
        // The first 8 words of the state are never seeded by FFmpeg and stay zero.
        let mut state = [0u32; 64];
        for i in (8..64).step_by(4) {
            let mut tmp = [0u8; 16];
            tmp[0..4].copy_from_slice(&SEED.to_le_bytes());
            tmp[4] = i as u8;
            let h = md5_16(&tmp);
            for (j, w) in h.chunks_exact(4).enumerate() {
                state[i + j] = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
            }
        }
        Lfg { state, index: 0 }
    }

    fn get(&mut self) -> u32 {
        let i = self.index as usize;
        let a =
            self.state[i.wrapping_sub(24) & 63].wrapping_add(self.state[i.wrapping_sub(55) & 63]);
        self.state[i & 63] = a;
        self.index = self.index.wrapping_add(1);
        a
    }

    /// Two normally distributed numbers (`av_bmg_get`).
    pub(crate) fn gaussian_pair(&mut self) -> [f64; 2] {
        const K: f64 = 2.0 / 4294967295.0;
        loop {
            let x1 = K * f64::from(self.get()) - 1.0;
            let x2 = K * f64::from(self.get()) - 1.0;
            let w = x1 * x1 + x2 * x2;
            if w >= 1.0 {
                continue;
            }
            let w = ((-2.0 * w.ln()) / w).sqrt();
            return [x1 * w, x2 * w];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_rfc1321_style_vector() {
        // MD5 of 16 zero bytes.
        let h = md5_16(&[0u8; 16]);
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "4ae71336e44bf9bf79d2752e234818a5");
    }

    #[test]
    fn gaussian_pair_is_finite() {
        let mut lfg = Lfg::new();
        for _ in 0..1000 {
            let [a, b] = lfg.gaussian_pair();
            assert!(a.is_finite() && b.is_finite());
        }
    }
}
