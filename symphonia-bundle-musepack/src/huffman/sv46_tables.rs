// Symphonia Musepack demuxer+decoder
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SV4-SV6 Huffman code tables.
//!
//! Ported (as data) from libmpcdec `huffsv46.c` (BSD-3-Clause), see `NOTICE`. The reference
//! stores 32-bit code thresholds and scans for the first entry whose threshold is not above the
//! next 32 bits of the stream. Every threshold is a multiple of `2^(32 - max_length)` with
//! `max_length <= 14`, so the top 16 bits of each threshold, as stored here, order every
//! possible 16-bit stream prefix exactly like the original 32-bit values (the generated tables
//! were checked against `huffsv46.c` entry by entry). `HuffEntry` can therefore be used as is,
//! and the tables are decoded by `bits::huff_dec` like the SV7 ones.

use crate::bits::{HuffEntry, HuffTable};

const fn he(code: u16, length: u8, value: i8) -> HuffEntry {
    HuffEntry { code, length, value }
}

/// `mpc_table_SCFI_Bundle`.
pub static HUFF_SCFI_BUNDLE: [HuffEntry; 8] = [
    he(0x8000, 1, 7),
    he(0x4000, 2, 3),
    he(0x3800, 5, 1),
    he(0x3000, 5, 2),
    he(0x2c00, 6, 0),
    he(0x2800, 6, 6),
    he(0x2000, 5, 4),
    he(0x0, 3, 5),
];

/// `mpc_table_DSCF_Entropie`.
pub static HUFF_DSCF_ENTROPIE: [HuffEntry; 13] = [
    he(0xe000, 3, 1),
    he(0xd000, 4, 3),
    he(0xc800, 5, 5),
    he(0xc000, 5, -3),
    he(0xb000, 4, -2),
    he(0xa000, 4, 4),
    he(0x8000, 3, -1),
    he(0x6000, 3, 2),
    he(0x5800, 5, -5),
    he(0x5400, 6, 6),
    he(0x5000, 6, -6),
    he(0x4000, 4, -4),
    he(0x0, 2, 0),
];

/// `mpc_table_Region_A`.
pub static HUFF_REGION_A: [HuffEntry; 16] = [
    he(0x8000, 1, 1),
    he(0x7800, 5, 3),
    he(0x7400, 6, 4),
    he(0x7200, 7, 7),
    he(0x7100, 8, 8),
    he(0x7080, 9, 9),
    he(0x7040, 10, 10),
    he(0x7020, 11, 11),
    he(0x7010, 12, 12),
    he(0x7008, 13, 13),
    he(0x7004, 14, 14),
    he(0x7000, 14, 15),
    he(0x6800, 5, 5),
    he(0x6000, 5, 6),
    he(0x4000, 3, 0),
    he(0x0, 2, 2),
];

/// `mpc_table_Region_B`.
pub static HUFF_REGION_B: [HuffEntry; 8] = [
    he(0x8000, 1, 1),
    he(0x4000, 2, 0),
    he(0x2000, 3, 2),
    he(0x1000, 4, 3),
    he(0x800, 5, 4),
    he(0x400, 6, 5),
    he(0x200, 7, 6),
    he(0x0, 7, 7),
];

/// `mpc_table_Region_C`.
pub static HUFF_REGION_C: [HuffEntry; 4] =
    [he(0x8000, 1, 0), he(0x4000, 2, 1), he(0x2000, 3, 2), he(0x0, 3, 3)];

/// `mpc_table_Entropie_1`.
pub static HUFF_ENTROPIE_1: [HuffEntry; 3] = [he(0x8000, 1, 0), he(0x4000, 2, -1), he(0x0, 2, 1)];

/// `mpc_table_Entropie_2`.
pub static HUFF_ENTROPIE_2: [HuffEntry; 5] =
    [he(0xc000, 2, 0), he(0xa000, 3, 2), he(0x8000, 3, -2), he(0x4000, 2, 1), he(0x0, 2, -1)];

/// `mpc_table_Entropie_3`.
pub static HUFF_ENTROPIE_3: [HuffEntry; 7] = [
    he(0xc000, 2, 0),
    he(0xa000, 3, -2),
    he(0x9000, 4, 2),
    he(0x8800, 5, -3),
    he(0x8000, 5, 3),
    he(0x4000, 2, -1),
    he(0x0, 2, 1),
];

/// `mpc_table_Entropie_4`.
pub static HUFF_ENTROPIE_4: [HuffEntry; 9] = [
    he(0xf000, 4, 3),
    he(0xe000, 4, -3),
    he(0xc000, 3, 1),
    he(0xa000, 3, -1),
    he(0x8000, 3, 2),
    he(0x6000, 3, -2),
    he(0x5000, 4, -4),
    he(0x4000, 4, 4),
    he(0x0, 2, 0),
];

/// `mpc_table_Entropie_5`.
pub static HUFF_ENTROPIE_5: [HuffEntry; 15] = [
    he(0xf000, 4, -2),
    he(0xe800, 5, -5),
    he(0xe400, 6, -7),
    he(0xe000, 6, 7),
    he(0xd000, 4, -3),
    he(0xc000, 4, 3),
    he(0xb800, 5, -6),
    he(0xb000, 5, 6),
    he(0xa000, 4, 4),
    he(0x8000, 3, 0),
    he(0x6000, 3, 1),
    he(0x4000, 3, -1),
    he(0x3000, 4, -4),
    he(0x2000, 4, 5),
    he(0x0, 3, 2),
];

/// `mpc_table_Entropie_6`.
pub static HUFF_ENTROPIE_6: [HuffEntry; 31] = [
    he(0xf800, 5, -4),
    he(0xf000, 5, 5),
    he(0xe800, 5, -5),
    he(0xe400, 6, 10),
    he(0xe000, 6, -10),
    he(0xd800, 5, -6),
    he(0xd000, 5, 6),
    he(0xc800, 5, 7),
    he(0xc000, 5, -7),
    he(0xb800, 5, -8),
    he(0xb400, 6, -11),
    he(0xb200, 7, 14),
    he(0xb000, 7, -14),
    he(0xa800, 5, 8),
    he(0xa400, 6, 11),
    he(0xa000, 6, -13),
    he(0x9000, 4, 0),
    he(0x8000, 4, 1),
    he(0x7000, 4, -1),
    he(0x6000, 4, 3),
    he(0x5000, 4, 2),
    he(0x4800, 5, -9),
    he(0x4400, 6, 12),
    he(0x4000, 6, 13),
    he(0x3000, 4, -3),
    he(0x2000, 4, -2),
    he(0x1800, 5, 9),
    he(0x1400, 6, -12),
    he(0x1200, 7, 15),
    he(0x1000, 7, -15),
    he(0x0, 4, 4),
];

/// `mpc_table_Entropie_7`.
pub static HUFF_ENTROPIE_7: [HuffEntry; 63] = [
    he(0xff00, 8, 28),
    he(0xfe00, 8, 26),
    he(0xfc00, 7, -20),
    he(0xf800, 6, 8),
    he(0xf400, 6, -8),
    he(0xf000, 6, -9),
    he(0xec00, 6, 9),
    he(0xea00, 7, 20),
    he(0xe800, 7, 21),
    he(0xe400, 6, -10),
    he(0xe000, 6, -11),
    he(0xdc00, 6, 10),
    he(0xd800, 6, 11),
    he(0xd600, 7, -21),
    he(0xd500, 8, 29),
    he(0xd400, 8, -29),
    he(0xd000, 6, 13),
    he(0xcc00, 6, -13),
    he(0xc800, 6, -12),
    he(0xc400, 6, 12),
    he(0xc200, 7, -22),
    he(0xc000, 7, 22),
    he(0xbc00, 6, 14),
    he(0xb800, 6, 15),
    he(0xb400, 6, -14),
    he(0xb200, 7, -23),
    he(0xb000, 7, 23),
    he(0xac00, 6, -15),
    he(0xa800, 6, -16),
    he(0xa400, 6, 16),
    he(0xa200, 7, 27),
    he(0xa000, 7, -27),
    he(0x9c00, 6, 17),
    he(0x9800, 6, -17),
    he(0x9700, 8, -30),
    he(0x9600, 8, 30),
    he(0x9400, 7, 24),
    he(0x9000, 6, -18),
    he(0x8800, 5, -1),
    he(0x8000, 5, 1),
    he(0x7e00, 7, -24),
    he(0x7c00, 7, 25),
    he(0x7800, 6, 18),
    he(0x7000, 5, -3),
    he(0x6800, 5, 3),
    he(0x6000, 5, 5),
    he(0x5800, 5, 0),
    he(0x5000, 5, -2),
    he(0x4c00, 6, 19),
    he(0x4800, 6, -19),
    he(0x4000, 5, -5),
    he(0x3800, 5, -4),
    he(0x3000, 5, -7),
    he(0x2800, 5, 2),
    he(0x2000, 5, 4),
    he(0x1800, 5, 7),
    he(0x1600, 7, -25),
    he(0x1400, 7, -26),
    he(0x1200, 7, -28),
    he(0x1100, 8, -31),
    he(0x1000, 8, 31),
    he(0x800, 5, 6),
    he(0x0, 5, -6),
];

/// Lookup-accelerated wrappers over the tables above (see [`HuffTable`]).
pub static T_SCFI_BUNDLE: HuffTable = HuffTable::new(&HUFF_SCFI_BUNDLE);
pub static T_DSCF_ENTROPIE: HuffTable = HuffTable::new(&HUFF_DSCF_ENTROPIE);
pub static T_REGION_A: HuffTable = HuffTable::new(&HUFF_REGION_A);
pub static T_REGION_B: HuffTable = HuffTable::new(&HUFF_REGION_B);
pub static T_REGION_C: HuffTable = HuffTable::new(&HUFF_REGION_C);
pub static T_ENTROPIE_1: HuffTable = HuffTable::new(&HUFF_ENTROPIE_1);
pub static T_ENTROPIE_2: HuffTable = HuffTable::new(&HUFF_ENTROPIE_2);
pub static T_ENTROPIE_3: HuffTable = HuffTable::new(&HUFF_ENTROPIE_3);
pub static T_ENTROPIE_4: HuffTable = HuffTable::new(&HUFF_ENTROPIE_4);
pub static T_ENTROPIE_5: HuffTable = HuffTable::new(&HUFF_ENTROPIE_5);
pub static T_ENTROPIE_6: HuffTable = HuffTable::new(&HUFF_ENTROPIE_6);
pub static T_ENTROPIE_7: HuffTable = HuffTable::new(&HUFF_ENTROPIE_7);

/// `mpc_table_SampleHuff[res]`: the sample code table of quantizer `res`, or `None` for the
/// quantizers (`0` and `8..=17`) whose samples are not Huffman coded.
pub fn sample_huff(res: i32) -> Option<&'static HuffTable> {
    match res {
        1 => Some(&T_ENTROPIE_1),
        2 => Some(&T_ENTROPIE_2),
        3 => Some(&T_ENTROPIE_3),
        4 => Some(&T_ENTROPIE_4),
        5 => Some(&T_ENTROPIE_5),
        6 => Some(&T_ENTROPIE_6),
        7 => Some(&T_ENTROPIE_7),
        _ => None,
    }
}
