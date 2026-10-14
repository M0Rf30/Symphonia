// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ITU-T G.726 ADPCM decoder (16, 24, 32 and 40 kbit/s at 8 kHz).
//!
//! The algorithm follows ITU-T G.726 section 4 ("Computational details") using the 11-bit
//! floating point representation of the standard. It is arranged to be bit-exact with FFmpeg's
//! `libavcodec/g726.c` (including its 16-bit truncation of intermediate values), from which the
//! tables below are taken.

use symphonia_core::errors::{Result, decode_error};

// Quantizer reconstruction (inverse quantizer, "DQLN") tables, indexed by the codeword.
// Source: FFmpeg libavcodec/g726.c `iquant_tbl16/24/32/40` (ITU-T G.726 tables 6, 7, 8 and 9).
const IQUANT_16: [i16; 4] = [116, 365, 365, 116];
const IQUANT_24: [i16; 8] = [i16::MIN, 135, 273, 373, 373, 273, 135, i16::MIN];
const IQUANT_32: [i16; 16] =
    [i16::MIN, 4, 135, 213, 273, 323, 373, 425, 425, 373, 323, 273, 213, 135, 4, i16::MIN];
const IQUANT_40: [i16; 32] = [
    i16::MIN,
    -66,
    28,
    104,
    169,
    224,
    274,
    318,
    358,
    395,
    429,
    459,
    488,
    514,
    539,
    566,
    566,
    539,
    514,
    488,
    459,
    429,
    395,
    358,
    318,
    274,
    224,
    169,
    104,
    28,
    -66,
    i16::MIN,
];

// Scale factor adaptation speed ("W") tables, indexed by the codeword.
// Source: FFmpeg libavcodec/g726.c `W_tbl16/24/32/40` (ITU-T G.726 tables 6-9, "W(I)").
const W_16: [i16; 4] = [-22, 439, 439, -22];
const W_24: [i16; 8] = [-4, 30, 137, 582, 582, 137, 30, -4];
const W_32: [i16; 16] =
    [-12, 18, 41, 64, 112, 198, 355, 1122, 1122, 355, 198, 112, 64, 41, 18, -12];
const W_40: [i16; 32] = [
    14, 14, 24, 39, 40, 41, 58, 100, 141, 179, 219, 280, 358, 440, 529, 696, 696, 529, 440, 358,
    280, 219, 179, 141, 100, 58, 41, 40, 39, 24, 14, 14,
];

// Speed control ("F") tables, indexed by the codeword.
// Source: FFmpeg libavcodec/g726.c `F_tbl16/24/32/40` (ITU-T G.726 tables 6-9, "F(I)").
const F_16: [u8; 4] = [0, 7, 7, 0];
const F_24: [u8; 8] = [0, 1, 2, 7, 7, 2, 1, 0];
const F_32: [u8; 16] = [0, 0, 0, 1, 1, 1, 3, 7, 7, 3, 1, 1, 1, 0, 0, 0];
const F_40: [u8; 32] = [
    0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 2, 3, 4, 5, 6, 6, 6, 6, 5, 4, 3, 2, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0,
];

/// G.726 11-bit float (sign, 4-bit exponent, 6-bit mantissa).
#[derive(Clone, Copy)]
struct Float11 {
    sign: u8,
    exp: u8,
    mant: u8,
}

impl Float11 {
    const INIT: Float11 = Float11 { sign: 0, exp: 0, mant: 1 << 5 };

    /// Integer to 11-bit float (`i2f`). `|i|` must be below 65536.
    #[inline]
    fn from_i32(i: i32) -> Self {
        let sign = (i < 0) as u8;
        let i = i.unsigned_abs();
        // av_log2_16bit(i) + !!i
        let exp = if i == 0 { 0 } else { (31 - i.leading_zeros()) + 1 };
        let mant = if i != 0 { (i << 6) >> exp } else { 1 << 5 };
        Float11 { sign, exp: exp as u8, mant: mant as u8 }
    }

    /// Floating point multiply (`mult`). The result is truncated to 16 bits as in FFmpeg.
    #[inline]
    fn mul(&self, other: &Float11) -> i32 {
        let exp = i32::from(self.exp) + i32::from(other.exp);
        let res = (i32::from(self.mant) * i32::from(other.mant) + 0x30) >> 4;
        let res = if exp > 19 { res << (exp - 19) } else { res >> (19 - exp) };
        let res = if (self.sign ^ other.sign) != 0 { -res } else { res };
        i32::from(res as i16)
    }
}

#[inline]
fn sgn(v: i32) -> i32 {
    if v < 0 { -1 } else { 1 }
}

/// G.726 ADPCM decoder.
pub(crate) struct G726Decoder {
    code_size: u32,
    msb_first: bool,
    iquant: &'static [i16],
    w: &'static [i16],
    f: &'static [u8],

    sr: [Float11; 2],
    dq: [Float11; 6],
    a: [i32; 2],
    b: [i32; 6],
    pk: [i32; 2],
    ap: i32,
    yu: i32,
    yl: i32,
    dms: i32,
    dml: i32,
    td: bool,
    se: i32,
    sez: i32,
    y: i32,
}

impl G726Decoder {
    /// `bits_per_sample` in 2..=5 (16/24/32/40 kbit/s). `msb_first` selects the "g726"
    /// big-endian packing; otherwise the "g726le" right-justified little-endian packing.
    pub(crate) fn new(bits_per_sample: u32, msb_first: bool) -> Self {
        assert!((2..=5).contains(&bits_per_sample), "g726: invalid bits per sample");

        let (iquant, w, f): (&'static [i16], &'static [i16], &'static [u8]) = match bits_per_sample
        {
            2 => (&IQUANT_16, &W_16, &F_16),
            3 => (&IQUANT_24, &W_24, &F_24),
            4 => (&IQUANT_32, &W_32, &F_32),
            _ => (&IQUANT_40, &W_40, &F_40),
        };

        let mut dec = G726Decoder {
            code_size: bits_per_sample,
            msb_first,
            iquant,
            w,
            f,
            sr: [Float11::INIT; 2],
            dq: [Float11::INIT; 6],
            a: [0; 2],
            b: [0; 6],
            pk: [1; 2],
            ap: 0,
            yu: 544,
            yl: 34816,
            dms: 0,
            dml: 0,
            td: false,
            se: 0,
            sez: 0,
            y: 544,
        };
        dec.reset();
        dec
    }

    /// Resets the decoder to its initial state.
    pub(crate) fn reset(&mut self) {
        self.sr = [Float11::INIT; 2];
        self.dq = [Float11::INIT; 6];
        self.a = [0; 2];
        self.b = [0; 6];
        self.pk = [1; 2];
        self.ap = 0;
        self.yu = 544;
        self.yl = 34816;
        self.dms = 0;
        self.dml = 0;
        self.td = false;
        self.se = 0;
        self.sez = 0;
        self.y = 544;
    }

    /// The number of whole samples held by `len` bytes of data.
    pub(crate) fn frames_in(&self, len: usize) -> usize {
        len * 8 / self.code_size as usize
    }

    /// Decodes `out.len()` samples from `data`. State persists across calls; the bit position
    /// restarts at the byte boundary of each call.
    pub(crate) fn decode(&mut self, data: &[u8], out: &mut [i32]) -> Result<()> {
        let bits = self.code_size as usize;
        if data.len() * 8 < out.len() * bits {
            return decode_error("adpcm (g726): not enough data for requested samples");
        }

        let mask = (1u32 << bits) - 1;
        let mut bitpos = 0usize;

        for o in out.iter_mut() {
            let byte = bitpos >> 3;
            let shift = bitpos & 7;
            // At most 5 bits starting at bit offset <= 7: spans at most 2 bytes.
            let lo = u32::from(data[byte]);
            let hi = data.get(byte + 1).map_or(0, |&b| u32::from(b));
            let code = if self.msb_first {
                let v = (lo << 8) | hi;
                (v >> (16 - shift - bits)) & mask
            }
            else {
                let v = lo | (hi << 8);
                (v >> shift) & mask
            };
            bitpos += bits;

            *o = i32::from(self.decode_code(code as usize)) << 16;
        }

        Ok(())
    }

    /// Paragraph 4.2.3: inverse adaptive quantizer. The result is truncated to 16 bits as in
    /// FFmpeg.
    #[inline]
    fn inverse_quant(&self, i: usize) -> i32 {
        let dql = i32::from(self.iquant[i]) + (self.y >> 2);
        let dex = (dql >> 7) & 0xf;
        let dqt = (1 << 7) + (dql & 0x7f);
        if dql < 0 { 0 } else { i32::from(((dqt << dex) >> 7) as i16) }
    }

    fn decode_code(&mut self, code: usize) -> i16 {
        let i_sig = (code >> (self.code_size - 1)) as u8;

        let mut dq = self.inverse_quant(code);

        // Transition detect.
        let ylint = self.yl >> 15;
        let ylfrac = (self.yl >> 10) & 0x1f;
        let thr2 = if ylint > 9 { 0x1f << 10 } else { (0x20 + ylfrac) << ylint };
        let tr = self.td && dq > ((3 * thr2) >> 2);

        if i_sig != 0 {
            dq = -dq;
        }
        let re_signal = i32::from((self.se + dq) as i16);

        // Update second order predictor coefficients A2 and A1.
        let pk0 = if self.sez + dq != 0 { sgn(self.sez + dq) } else { 0 };
        let dq0 = if dq != 0 { sgn(dq) } else { 0 };
        if tr {
            self.a = [0; 2];
            self.b = [0; 6];
        }
        else {
            let fa1 = ((-self.a[0] * self.pk[0] * pk0) >> 5).clamp(-256, 255);

            self.a[1] += 128 * pk0 * self.pk[1] + fa1 - (self.a[1] >> 7);
            self.a[1] = self.a[1].clamp(-12288, 12288);
            self.a[0] += 64 * 3 * pk0 * self.pk[0] - (self.a[0] >> 8);
            let lim = 15360 - self.a[1];
            self.a[0] = self.a[0].clamp(-lim, lim);

            for i in 0..6 {
                self.b[i] += 128 * dq0 * sgn(-i32::from(self.dq[i].sign)) - (self.b[i] >> 8);
            }
        }

        // Update Dq, Sr and Pk.
        self.pk[1] = self.pk[0];
        self.pk[0] = if pk0 != 0 { pk0 } else { 1 };
        self.sr[1] = self.sr[0];
        self.sr[0] = Float11::from_i32(re_signal);
        for i in (1..6).rev() {
            self.dq[i] = self.dq[i - 1];
        }
        self.dq[0] = Float11::from_i32(dq);
        self.dq[0].sign = i_sig;

        self.td = self.a[1] < -11776;

        // Update Ap.
        let fi = i32::from(self.f[code]);
        self.dms += (fi << 4) + ((-self.dms) >> 5);
        self.dml += (fi << 4) + ((-self.dml) >> 7);
        if tr {
            self.ap = 256;
        }
        else {
            self.ap += (-self.ap) >> 4;
            if self.y <= 1535 || self.td || ((self.dms << 2) - self.dml).abs() >= (self.dml >> 3) {
                self.ap += 0x20;
            }
        }

        // Update Yu and Yl.
        self.yu = (self.y + i32::from(self.w[code]) + ((-self.y) >> 5)).clamp(544, 5120);
        self.yl += self.yu + ((-self.yl) >> 6);

        // Next iteration for Y.
        let al = if self.ap >= 256 { 1 << 6 } else { self.ap >> 2 };
        self.y = (self.yl + (self.yu - (self.yl >> 6)) * al) >> 6;

        // Next iteration for SE and SEZ.
        let mut se = 0;
        for i in 0..6 {
            se += Float11::from_i32(self.b[i] >> 2).mul(&self.dq[i]);
        }
        self.sez = se >> 1;
        for i in 0..2 {
            se += Float11::from_i32(self.a[i] >> 2).mul(&self.sr[i]);
        }
        self.se = se >> 1;

        (re_signal * 4).clamp(-0xffff, 0xffff) as i16
    }
}

#[cfg(test)]
mod tests {
    use super::G726Decoder;

    // 16 bytes of pseudo-random input and the s16 output of FFmpeg (`-f g726[le] -code_size 4`).
    const INPUT: [u8; 16] = [
        0xe0, 0x56, 0xd9, 0xd8, 0x31, 0xdf, 0xa5, 0x5b, 0x0e, 0xf5, 0xae, 0x4f, 0xda, 0x74, 0x9c,
        0xdb,
    ];
    const BE: [i32; 32] = [
        -8, 0, 48, 68, -20, -76, -24, -116, 104, 28, -56, -4, -144, 176, 236, -204, -36, -40, 8,
        244, -260, -96, 256, 16, -124, -304, 612, 1224, -1992, -1528, -548, -1200,
    ];
    const LE: [i32; 32] = [
        0, -8, 60, 56, -76, -24, -108, -60, 24, 60, 0, -36, 108, -132, -124, 192, -40, -12, 208,
        -12, -56, -204, -4, 188, -248, -108, 252, 540, -800, -1752, -1320, -528,
    ];

    fn run(msb_first: bool, expected: &[i32; 32]) {
        let mut dec = G726Decoder::new(4, msb_first);
        let mut out = [0i32; 32];
        dec.decode(&INPUT, &mut out).unwrap();
        let exp: Vec<i32> = expected.iter().map(|v| v << 16).collect();
        assert_eq!(&out[..], &exp[..]);

        // Reset restores the initial state.
        dec.reset();
        let mut out2 = [0i32; 32];
        dec.decode(&INPUT, &mut out2).unwrap();
        assert_eq!(out, out2);
    }

    #[test]
    fn verify_g726_matches_ffmpeg() {
        run(true, &BE);
        run(false, &LE);
    }

    #[test]
    fn verify_g726_short_data_is_error() {
        let mut dec = G726Decoder::new(3, true);
        let mut out = [0i32; 6];
        assert!(dec.decode(&[0, 0], &mut out).is_err());
    }
}
