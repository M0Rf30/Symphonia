// Symphonia Musepack demuxer+decoder
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SV7 Huffman code tables.
//!
//! Ported verbatim (as data) from libmpcdec `huffman.c` (BSD-3-Clause), see `NOTICE`. The
//! numeric contents of every table below are byte-for-byte identical to the C source; only the
//! syntax (`{code, length, value}` -> `he(code, length, value)`) differs.

use crate::bits::HuffEntry;

const fn he(code: u16, length: u8, value: i8) -> HuffEntry {
    HuffEntry { code, length, value }
}

/// `mpc_table_HuffHdr`.
pub static HUFF_HDR: [HuffEntry; 10] = [
    he(0x8000, 1, 0), he(0x6000, 3, 1), he(0x5e00, 7, -4), he(0x5d80, 9, 3), he(0x5d00, 9, 4),
    he(0x5c00, 8, -5), he(0x5800, 6, 2), he(0x5000, 5, -3), he(0x4000, 4, -2), he(0x0, 2, -1),
];

/// `mpc_table_HuffSCFI`.
pub static HUFF_SCFI: [HuffEntry; 4] =
    [he(0x8000, 1, 1), he(0x6000, 3, 2), he(0x4000, 3, 0), he(0x0, 2, 3)];

/// `mpc_table_HuffDSCF`.
pub static HUFF_DSCF: [HuffEntry; 16] = [
    he(0xf800, 5, 5), he(0xf000, 5, -4), he(0xe000, 4, 3), he(0xd000, 4, -3), he(0xc000, 4, 8),
    he(0xa000, 3, 1), he(0x9000, 4, 0), he(0x8800, 5, -5), he(0x8400, 6, 7), he(0x8000, 6, -7),
    he(0x6000, 3, -1), he(0x4000, 3, 2), he(0x3000, 4, 4), he(0x2800, 5, 6), he(0x2000, 5, -6),
    he(0x0, 3, -2),
];

/// `mpc_table_HuffQ1`.
pub static HUFF_Q1: [[HuffEntry; 27]; 2] = [
    [
        he(0xe000, 3, 13), he(0xdc00, 6, 26), he(0xd800, 6, 0), he(0xd400, 6, 20), he(0xd000, 6, 6),
        he(0xc000, 4, 14), he(0xb000, 4, 12), he(0xa000, 4, 4), he(0x9000, 4, 22), he(0x8c00, 6, 8),
        he(0x8800, 6, 18), he(0x8400, 6, 24), he(0x8000, 6, 2), he(0x7000, 4, 16), he(0x6000, 4, 10),
        he(0x5800, 5, 17), he(0x5000, 5, 9), he(0x4800, 5, 1), he(0x4000, 5, 25), he(0x3800, 5, 5),
        he(0x3000, 5, 21), he(0x2800, 5, 3), he(0x2000, 5, 11), he(0x1800, 5, 15), he(0x1000, 5, 23),
        he(0x800, 5, 19), he(0x0, 5, 7),
    ],
    [
        he(0x8000, 1, 13), he(0x7e00, 7, 15), he(0x7c00, 7, 1), he(0x7a00, 7, 11), he(0x7800, 7, 7),
        he(0x7600, 7, 17), he(0x7400, 7, 25), he(0x7200, 7, 19), he(0x7180, 9, 8), he(0x7100, 9, 18),
        he(0x7080, 9, 2), he(0x7000, 9, 24), he(0x6e00, 7, 3), he(0x6c00, 7, 23), he(0x6a00, 7, 21),
        he(0x6800, 7, 5), he(0x6700, 8, 0), he(0x6600, 8, 26), he(0x6500, 8, 6), he(0x6400, 8, 20),
        he(0x6000, 6, 9), he(0x5000, 4, 14), he(0x4000, 4, 12), he(0x3000, 4, 4), he(0x2000, 4, 22),
        he(0x1000, 4, 16), he(0x0, 4, 10),
    ],
];

/// `mpc_table_HuffQ2`.
pub static HUFF_Q2: [[HuffEntry; 25]; 2] = [
    [
        he(0xf000, 4, 13), he(0xe000, 4, 17), he(0xd000, 4, 7), he(0xc000, 4, 11), he(0xbc00, 6, 1),
        he(0xb800, 6, 23), he(0xb600, 7, 4), he(0xb400, 7, 20), he(0xb200, 7, 0), he(0xb000, 7, 24),
        he(0xa800, 5, 22), he(0xa000, 5, 10), he(0x8000, 3, 12), he(0x7800, 5, 2), he(0x7000, 5, 14),
        he(0x6000, 4, 6), he(0x5000, 4, 18), he(0x4000, 4, 8), he(0x3000, 4, 16), he(0x2800, 5, 9),
        he(0x2000, 5, 5), he(0x1800, 5, 15), he(0x1000, 5, 21), he(0x800, 5, 19), he(0x0, 5, 3),
    ],
    [
        he(0xf800, 5, 18), he(0xf000, 5, 6), he(0xe800, 5, 8), he(0xe700, 8, 3), he(0xe6c0, 10, 24),
        he(0xe680, 10, 4), he(0xe640, 10, 0), he(0xe600, 10, 20), he(0xe400, 7, 23), he(0xe200, 7, 1),
        he(0xe000, 7, 19), he(0xd800, 5, 16), he(0xd600, 7, 15), he(0xd400, 7, 21), he(0xd200, 7, 9),
        he(0xd000, 7, 5), he(0xcc00, 6, 2), he(0xc800, 6, 10), he(0xc400, 6, 14), he(0xc000, 6, 22),
        he(0x8000, 2, 12), he(0x6000, 3, 13), he(0x4000, 3, 17), he(0x2000, 3, 11), he(0x0, 3, 7),
    ],
];

/// `mpc_table_HuffQ3`.
pub static HUFF_Q3: [[HuffEntry; 7]; 2] = [
    [
        he(0xe000, 3, 1), he(0xd000, 4, 3), he(0xc000, 4, -3), he(0xa000, 3, 2), he(0x8000, 3, -2),
        he(0x4000, 2, 0), he(0x0, 2, -1),
    ],
    [
        he(0xc000, 2, 0), he(0x8000, 2, -1), he(0x4000, 2, 1), he(0x3000, 4, -2), he(0x2800, 5, 3),
        he(0x2000, 5, -3), he(0x0, 3, 2),
    ],
];

/// `mpc_table_HuffQ4`.
pub static HUFF_Q4: [[HuffEntry; 9]; 2] = [
    [
        he(0xe000, 3, 0), he(0xc000, 3, -1), he(0xa000, 3, 1), he(0x8000, 3, -2), he(0x6000, 3, 2),
        he(0x5000, 4, -4), he(0x4000, 4, 4), he(0x2000, 3, 3), he(0x0, 3, -3),
    ],
    [
        he(0xe000, 3, 1), he(0xd000, 4, 2), he(0xc000, 4, -3), he(0x8000, 2, 0), he(0x6000, 3, -2),
        he(0x5000, 4, 3), he(0x4800, 5, -4), he(0x4000, 5, 4), he(0x0, 2, -1),
    ],
];

/// `mpc_table_HuffQ5`.
pub static HUFF_Q5: [[HuffEntry; 15]; 2] = [
    [
        he(0xf000, 4, 2), he(0xe800, 5, 5), he(0xe400, 6, -7), he(0xe000, 6, 7), he(0xd000, 4, -3),
        he(0xc000, 4, 3), he(0xb800, 5, -6), he(0xb000, 5, 6), he(0xa000, 4, -4), he(0x9000, 4, 4),
        he(0x8000, 4, -5), he(0x6000, 3, 0), he(0x4000, 3, -1), he(0x2000, 3, 1), he(0x0, 3, -2),
    ],
    [
        he(0xf000, 4, 3), he(0xe800, 5, 4), he(0xe600, 7, 6), he(0xe500, 8, -7), he(0xe400, 8, 7),
        he(0xe000, 6, -6), he(0xc000, 3, 0), he(0xa000, 3, -1), he(0x8000, 3, 1), he(0x6000, 3, -2),
        he(0x4000, 3, 2), he(0x3800, 5, -5), he(0x3000, 5, 5), he(0x2000, 4, -4), he(0x0, 3, -3),
    ],
];

/// `mpc_table_HuffQ6`.
pub static HUFF_Q6: [[HuffEntry; 31]; 2] = [
    [
        he(0xf800, 5, 3), he(0xf000, 5, -4), he(0xec00, 6, -11), he(0xe800, 6, 12), he(0xe000, 5, 4),
        he(0xd800, 5, 6), he(0xd000, 5, -5), he(0xc800, 5, 5), he(0xc000, 5, 7), he(0xb800, 5, -7),
        he(0xb400, 6, -12), he(0xb000, 6, -13), he(0xa800, 5, -6), he(0xa000, 5, 8), he(0x9800, 5, -8),
        he(0x9000, 5, 9), he(0x8800, 5, -9), he(0x8400, 6, 13), he(0x8200, 7, -15), he(0x8000, 7, 15),
        he(0x7000, 4, 0), he(0x6800, 5, -10), he(0x6000, 5, 10), he(0x5000, 4, -1), he(0x4000, 4, 2),
        he(0x3000, 4, 1), he(0x2000, 4, -2), he(0x1c00, 6, 14), he(0x1800, 6, -14), he(0x1000, 5, 11),
        he(0x0, 4, -3),
    ],
    [
        he(0xf800, 5, -6), he(0xf000, 5, 6), he(0xe000, 4, 1), he(0xd000, 4, -1), he(0xce00, 7, 10),
        he(0xcc00, 7, -10), he(0xcb00, 8, -11), he(0xca80, 9, -12), he(0xca60, 11, 13), he(0xca58, 13, 15),
        he(0xca50, 13, -14), he(0xca48, 13, 14), he(0xca40, 13, -15), he(0xca00, 10, -13), he(0xc900, 8, 11),
        he(0xc800, 8, 12), he(0xc400, 6, -9), he(0xc000, 6, 9), he(0xb000, 4, -2), he(0xa000, 4, 2),
        he(0x9000, 4, 3), he(0x8000, 4, -3), he(0x7800, 5, -7), he(0x7000, 5, 7), he(0x6000, 4, -4),
        he(0x5000, 4, 4), he(0x4800, 5, -8), he(0x4000, 5, 8), he(0x3000, 4, 5), he(0x2000, 4, -5),
        he(0x0, 3, 0),
    ],
];

/// `mpc_table_HuffQ7`.
pub static HUFF_Q7: [[HuffEntry; 63]; 2] = [
    [
        he(0xfc00, 6, 7), he(0xf800, 6, 8), he(0xf400, 6, 9), he(0xf000, 6, -8), he(0xec00, 6, 11),
        he(0xea00, 7, 21), he(0xe900, 8, -28), he(0xe800, 8, 28), he(0xe400, 6, -9), he(0xe200, 7, -22),
        he(0xe000, 7, -21), he(0xdc00, 6, -10), he(0xd800, 6, -11), he(0xd400, 6, 10), he(0xd000, 6, 12),
        he(0xcc00, 6, -13), he(0xca00, 7, 22), he(0xc800, 7, 23), he(0xc400, 6, -12), he(0xc000, 6, 13),
        he(0xbc00, 6, 14), he(0xb800, 6, -14), he(0xb600, 7, -23), he(0xb500, 8, -29), he(0xb400, 8, 29),
        he(0xb000, 6, -15), he(0xac00, 6, 15), he(0xa800, 6, 16), he(0xa400, 6, -16), he(0xa200, 7, -24),
        he(0xa000, 7, 24), he(0x9c00, 6, 17), he(0x9a00, 7, -25), he(0x9900, 8, -30), he(0x9800, 8, 30),
        he(0x9400, 6, -17), he(0x9000, 6, 18), he(0x8c00, 6, -18), he(0x8a00, 7, 25), he(0x8800, 7, 26),
        he(0x8400, 6, 19), he(0x8200, 7, -26), he(0x8000, 7, -27), he(0x7800, 5, 2), he(0x7400, 6, -19),
        he(0x7000, 6, 20), he(0x6800, 5, -1), he(0x6700, 8, -31), he(0x6600, 8, 31), he(0x6400, 7, 27),
        he(0x6000, 6, -20), he(0x5800, 5, 1), he(0x5000, 5, -5), he(0x4800, 5, -3), he(0x4000, 5, 3),
        he(0x3800, 5, 0), he(0x3000, 5, -2), he(0x2800, 5, -4), he(0x2000, 5, 4), he(0x1800, 5, 5),
        he(0x1000, 5, -6), he(0x800, 5, 6), he(0x0, 5, -7),
    ],
    [
        he(0xf800, 5, -1), he(0xf000, 5, 2), he(0xe800, 5, -2), he(0xe000, 5, 3), he(0xdf00, 8, -20),
        he(0xdec0, 10, 24), he(0xdebc, 14, 28), he(0xdeb8, 14, -28), he(0xdeb4, 14, -30), he(0xdeb0, 14, 30),
        he(0xdea0, 12, -27), he(0xde9c, 14, 29), he(0xde98, 14, -29), he(0xde94, 14, 31), he(0xde90, 14, -31),
        he(0xde80, 12, 27), he(0xde00, 9, -22), he(0xdc00, 7, -17), he(0xd800, 6, -11), he(0xd000, 5, -3),
        he(0xc800, 5, 4), he(0xc000, 5, -4), he(0xbe00, 7, 17), he(0xbd00, 8, 20), he(0xbc80, 9, 22),
        he(0xbc40, 10, -25), he(0xbc00, 10, -26), he(0xb800, 6, 12), he(0xb000, 5, 5), he(0xa800, 5, -5),
        he(0xa000, 5, 6), he(0x9800, 5, -6), he(0x9400, 6, -12), he(0x9200, 7, -18), he(0x9000, 7, 18),
        he(0x8c00, 6, 13), he(0x8800, 6, -13), he(0x8000, 5, -7), he(0x7c00, 6, 14), he(0x7b00, 8, 21),
        he(0x7a00, 8, -21), he(0x7800, 7, -19), he(0x7000, 5, 7), he(0x6800, 5, 8), he(0x6400, 6, -14),
        he(0x6000, 6, -15), he(0x5800, 5, -8), he(0x5400, 6, 15), he(0x5200, 7, 19), he(0x51c0, 10, 25),
        he(0x5180, 10, 26), he(0x5100, 9, -23), he(0x5080, 9, 23), he(0x5000, 9, -24), he(0x4800, 5, -9),
        he(0x4000, 5, 9), he(0x3c00, 6, 16), he(0x3800, 6, -16), he(0x3000, 5, 10), he(0x2000, 4, 0),
        he(0x1800, 5, -10), he(0x1000, 5, 11), he(0x0, 4, 1),
    ],
];

/// Bundled per-quantizer-order table pair lookup, matching the C `mpc_HuffQ[7][2]` array of
/// `mpc_lut_data`. Index `0` (order 0, `Res==-1` noise) is unused by the decoder; indices `1..=6`
/// correspond to SV7 `Res` values `1..=6` (the `Res==7` "raw bits" case as `Res_bit`/no Huffman
/// table).
pub fn huff_q(order: usize, sub: usize) -> &'static [HuffEntry] {
    match order {
        1 => &HUFF_Q1[sub],
        2 => &HUFF_Q2[sub],
        3 => &HUFF_Q3[sub],
        4 => &HUFF_Q4[sub],
        5 => &HUFF_Q5[sub],
        6 => &HUFF_Q6[sub],
        7 => &HUFF_Q7[sub],
        _ => &HUFF_HDR[..0],
    }
}
