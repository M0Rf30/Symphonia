// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The synthesis tables of the SBC decoder.
//!
//! The values are the 32-bit constants of the A2DP specification (Appendix B, proto tables and
//! the synthesis matrix) as they are listed in FFmpeg's `libavcodec/sbcdec_data.h` (LGPL-2.1+,
//! originally from the BlueZ SBC library by Marcel Holtmann et al.). The reference scales them
//! down when they are used (`SS4`, `SS8` and `SN4`, `SN8`); [`shift_table`] does the same, with an
//! arithmetic shift of the value reinterpreted as a signed integer.

/// Reinterpret each raw 32-bit constant as a signed integer, and shift it right.
const fn shift_table<const N: usize>(raw: &[u32; N], shift: u32) -> [i32; N] {
    let mut out = [0i32; N];
    let mut i = 0;
    while i < N {
        out[i] = (raw[i] as i32) >> shift;
        i += 1;
    }
    out
}

const PROTO_4_40M0_RAW: [u32; 20] = [
    0x00000000, 0xffa6982f, 0xfba93848, 0x0456c7b8, 0x005967d1, 0xfffb9ac7, 0xff589157, 0xf9c2a8d8,
    0x027c1434, 0x0019118b, 0xfff3c74c, 0xff137330, 0xf81b8d70, 0x00ec1b8b, 0xfff0b71a, 0xffe99b00,
    0xfef84470, 0xf6fb4370, 0xffcdc351, 0xffe01dc7,
];
/// The prototype filter of the 4 sub-band synthesis, even half.
pub static PROTO_4_40M0: [i32; 20] = shift_table(&PROTO_4_40M0_RAW, 12);

const PROTO_4_40M1_RAW: [u32; 20] = [
    0xffe090ce, 0xff2c0475, 0xf694f800, 0xff2c0475, 0xffe090ce, 0xffe01dc7, 0xffcdc351, 0xf6fb4370,
    0xfef84470, 0xffe99b00, 0xfff0b71a, 0x00ec1b8b, 0xf81b8d70, 0xff137330, 0xfff3c74c, 0x0019118b,
    0x027c1434, 0xf9c2a8d8, 0xff589157, 0xfffb9ac7,
];
/// The prototype filter of the 4 sub-band synthesis, odd half.
pub static PROTO_4_40M1: [i32; 20] = shift_table(&PROTO_4_40M1_RAW, 12);

const PROTO_8_80M0_RAW: [u32; 40] = [
    0x00000000, 0xfe8d1970, 0xee979f00, 0x11686100, 0x0172e690, 0xfff5bd1a, 0xfdf1c8d4, 0xeac182c0,
    0x0d9daee0, 0x00e530da, 0xffe9811d, 0xfd52986c, 0xe7054ca0, 0x0a00d410, 0x006c1de4, 0xffdba705,
    0xfcbc98e8, 0xe3889d20, 0x06af2308, 0x000bb7db, 0xffca00ed, 0xfc3fbb68, 0xe071bc00, 0x03bf7948,
    0xffc4e05c, 0xffb54b3b, 0xfbedadc0, 0xdde26200, 0x0142291c, 0xff960e94, 0xff9f3e17, 0xfbd8f358,
    0xdbf79400, 0xff405e01, 0xff7d4914, 0xff8b1a31, 0xfc1417b8, 0xdac7bb40, 0xfdbb828c, 0xff762170,
];
/// The prototype filter of the 8 sub-band synthesis, even half.
pub static PROTO_8_80M0: [i32; 40] = shift_table(&PROTO_8_80M0_RAW, 14);

const PROTO_8_80M1_RAW: [u32; 40] = [
    0xff7c272c, 0xfcb02620, 0xda612700, 0xfcb02620, 0xff7c272c, 0xff762170, 0xfdbb828c, 0xdac7bb40,
    0xfc1417b8, 0xff8b1a31, 0xff7d4914, 0xff405e01, 0xdbf79400, 0xfbd8f358, 0xff9f3e17, 0xff960e94,
    0x0142291c, 0xdde26200, 0xfbedadc0, 0xffb54b3b, 0xffc4e05c, 0x03bf7948, 0xe071bc00, 0xfc3fbb68,
    0xffca00ed, 0x000bb7db, 0x06af2308, 0xe3889d20, 0xfcbc98e8, 0xffdba705, 0x006c1de4, 0x0a00d410,
    0xe7054ca0, 0xfd52986c, 0xffe9811d, 0x00e530da, 0x0d9daee0, 0xeac182c0, 0xfdf1c8d4, 0xfff5bd1a,
];
/// The prototype filter of the 8 sub-band synthesis, odd half.
pub static PROTO_8_80M1: [i32; 40] = shift_table(&PROTO_8_80M1_RAW, 14);

const SYN_MATRIX_4_RAW: [u32; 32] = [
    0x05a82798, 0xfa57d868, 0xfa57d868, 0x05a82798, 0x030fbc54, 0xf89be510, 0x07641af0, 0xfcf043ac,
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0xfcf043ac, 0x07641af0, 0xf89be510, 0x030fbc54,
    0xfa57d868, 0x05a82798, 0x05a82798, 0xfa57d868, 0xf89be510, 0xfcf043ac, 0x030fbc54, 0x07641af0,
    0xf8000000, 0xf8000000, 0xf8000000, 0xf8000000, 0xf89be510, 0xfcf043ac, 0x030fbc54, 0x07641af0,
];
/// The synthesis matrix of 4 sub-bands, `8` rows of `4` coefficients.
pub static SYN_MATRIX_4: [i32; 32] = shift_table(&SYN_MATRIX_4_RAW, 14);

static SYN_MATRIX_8_RAW: [u32; 128] = [
    0x05a82798, 0xfa57d868, 0xfa57d868, 0x05a82798, 0x05a82798, 0xfa57d868, 0xfa57d868, 0x05a82798,
    0x0471ced0, 0xf8275a10, 0x018f8b84, 0x06a6d988, 0xf9592678, 0xfe70747c, 0x07d8a5f0, 0xfb8e3130,
    0x030fbc54, 0xf89be510, 0x07641af0, 0xfcf043ac, 0xfcf043ac, 0x07641af0, 0xf89be510, 0x030fbc54,
    0x018f8b84, 0xfb8e3130, 0x06a6d988, 0xf8275a10, 0x07d8a5f0, 0xf9592678, 0x0471ced0, 0xfe70747c,
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0xfe70747c, 0x0471ced0, 0xf9592678, 0x07d8a5f0, 0xf8275a10, 0x06a6d988, 0xfb8e3130, 0x018f8b84,
    0xfcf043ac, 0x07641af0, 0xf89be510, 0x030fbc54, 0x030fbc54, 0xf89be510, 0x07641af0, 0xfcf043ac,
    0xfb8e3130, 0x07d8a5f0, 0xfe70747c, 0xf9592678, 0x06a6d988, 0x018f8b84, 0xf8275a10, 0x0471ced0,
    0xfa57d868, 0x05a82798, 0x05a82798, 0xfa57d868, 0xfa57d868, 0x05a82798, 0x05a82798, 0xfa57d868,
    0xf9592678, 0x018f8b84, 0x07d8a5f0, 0x0471ced0, 0xfb8e3130, 0xf8275a10, 0xfe70747c, 0x06a6d988,
    0xf89be510, 0xfcf043ac, 0x030fbc54, 0x07641af0, 0x07641af0, 0x030fbc54, 0xfcf043ac, 0xf89be510,
    0xf8275a10, 0xf9592678, 0xfb8e3130, 0xfe70747c, 0x018f8b84, 0x0471ced0, 0x06a6d988, 0x07d8a5f0,
    0xf8000000, 0xf8000000, 0xf8000000, 0xf8000000, 0xf8000000, 0xf8000000, 0xf8000000, 0xf8000000,
    0xf8275a10, 0xf9592678, 0xfb8e3130, 0xfe70747c, 0x018f8b84, 0x0471ced0, 0x06a6d988, 0x07d8a5f0,
    0xf89be510, 0xfcf043ac, 0x030fbc54, 0x07641af0, 0x07641af0, 0x030fbc54, 0xfcf043ac, 0xf89be510,
    0xf9592678, 0x018f8b84, 0x07d8a5f0, 0x0471ced0, 0xfb8e3130, 0xf8275a10, 0xfe70747c, 0x06a6d988,
];
/// The synthesis matrix of 8 sub-bands, `16` rows of `8` coefficients.
pub static SYN_MATRIX_8: [i32; 128] = shift_table(&SYN_MATRIX_8_RAW, 14);
