// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ATRAC1/ATRAC3 constant tables, extracted programmatically from FFmpeg's `atrac1data.h`,
//! `atrac3data.h` and `atrac.c` (LGPL-2.1+, see `NOTICE`).

/// `bfu_amount_tab1` (atrac1data.h).
pub(crate) const BFU_AMOUNT_TAB1: [u8; 8] = [20, 28, 32, 36, 40, 44, 48, 52];

/// `bfu_amount_tab2` (atrac1data.h).
pub(crate) const BFU_AMOUNT_TAB2: [u8; 4] = [0, 112, 176, 208];

/// `bfu_amount_tab3` (atrac1data.h).
pub(crate) const BFU_AMOUNT_TAB3: [u8; 8] = [0, 24, 36, 48, 72, 108, 132, 156];

/// Number of BFUs in each QMF band, as cumulative start indices (`bfu_bands_t`, atrac1data.h).
pub(crate) const BFU_BANDS_T: [u8; 4] = [0, 20, 36, 52];

/// Number of spectral lines in each BFU (`specs_per_bfu`, atrac1data.h).
pub(crate) const SPECS_PER_BFU: [u8; 52] = [
    8, 8, 8, 8, 4, 4, 4, 4, 8, 8, 8, 8, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 7, 7, 7, 7, 9, 9, 9, 9,
    10, 10, 10, 10, 12, 12, 12, 12, 12, 12, 12, 12, 20, 20, 20, 20, 20, 20, 20, 20,
];

/// Start position of each BFU in the MDCT spectrum, long mode (`bfu_start_long`, atrac1data.h).
pub(crate) const BFU_START_LONG: [u16; 52] = [
    0, 8, 16, 24, 32, 36, 40, 44, 48, 56, 64, 72, 80, 86, 92, 98, 104, 110, 116, 122, 128, 134,
    140, 146, 152, 159, 166, 173, 180, 189, 198, 207, 216, 226, 236, 246, 256, 268, 280, 292, 304,
    316, 328, 340, 352, 372, 392, 412, 432, 452, 472, 492,
];

/// Start position of each BFU in the MDCT spectrum, short mode (`bfu_start_short`, atrac1data.h).
pub(crate) const BFU_START_SHORT: [u16; 52] = [
    0, 32, 64, 96, 8, 40, 72, 104, 12, 44, 76, 108, 20, 52, 84, 116, 26, 58, 90, 122, 128, 160,
    192, 224, 134, 166, 198, 230, 141, 173, 205, 237, 150, 182, 214, 246, 256, 288, 320, 352, 384,
    416, 448, 480, 268, 300, 332, 364, 396, 428, 460, 492,
];

/// First half of the 48-tap QMF prototype (`qmf_48tap_half`, atrac.c).
pub(crate) const QMF_48TAP_HALF: [f32; 24] = [
    -0.00001461907,
    -0.00009205479,
    -0.000056157569,
    0.00030117269,
    0.0002422519,
    -0.00085293897,
    -0.0005205574,
    0.0020340169,
    0.00078333891,
    -0.0042153862,
    -0.00075614988,
    0.0078402944,
    -0.000061169922,
    -0.01344162,
    0.0024626821,
    0.021736089,
    -0.007801671,
    -0.034090221,
    0.01880949,
    0.054326009,
    -0.043596379,
    -0.099384367,
    0.13207909,
    0.46424159,
];

/// Spectral coefficient Huffman tables as `(symbol, code length)` (`atrac3_hufftabs`,
/// atrac3data.h). Codes are assigned in table order (see `ff_vlc_init_from_lengths`).
pub(crate) static HUFFTABS: [(u8, u8); 139] = [
    (31, 1),
    (32, 3),
    (33, 3),
    (34, 4),
    (35, 4),
    (36, 5),
    (37, 5),
    (38, 5),
    (39, 5),
    (31, 1),
    (32, 3),
    (30, 3),
    (33, 3),
    (29, 3),
    (31, 1),
    (32, 3),
    (30, 3),
    (33, 4),
    (29, 4),
    (34, 4),
    (28, 4),
    (31, 1),
    (32, 3),
    (30, 3),
    (33, 4),
    (29, 4),
    (34, 5),
    (28, 5),
    (35, 5),
    (27, 5),
    (31, 2),
    (32, 3),
    (30, 3),
    (33, 4),
    (29, 4),
    (34, 4),
    (28, 4),
    (38, 4),
    (24, 4),
    (35, 5),
    (27, 5),
    (36, 6),
    (26, 6),
    (37, 6),
    (25, 6),
    (31, 3),
    (32, 4),
    (30, 4),
    (33, 4),
    (29, 4),
    (34, 4),
    (28, 4),
    (46, 4),
    (16, 4),
    (35, 5),
    (27, 5),
    (36, 5),
    (26, 5),
    (37, 5),
    (25, 5),
    (38, 6),
    (24, 6),
    (39, 6),
    (23, 6),
    (40, 6),
    (22, 6),
    (41, 6),
    (21, 6),
    (42, 7),
    (20, 7),
    (43, 7),
    (19, 7),
    (44, 7),
    (18, 7),
    (45, 7),
    (17, 7),
    (31, 3),
    (62, 4),
    (0, 4),
    (32, 5),
    (30, 5),
    (33, 5),
    (29, 5),
    (34, 5),
    (28, 5),
    (35, 5),
    (27, 5),
    (36, 5),
    (26, 5),
    (37, 6),
    (25, 6),
    (38, 6),
    (24, 6),
    (39, 6),
    (23, 6),
    (40, 6),
    (22, 6),
    (41, 6),
    (21, 6),
    (42, 6),
    (20, 6),
    (43, 6),
    (19, 6),
    (44, 6),
    (18, 6),
    (45, 7),
    (17, 7),
    (46, 7),
    (16, 7),
    (47, 7),
    (15, 7),
    (48, 7),
    (14, 7),
    (49, 7),
    (13, 7),
    (50, 7),
    (12, 7),
    (51, 7),
    (11, 7),
    (52, 8),
    (10, 8),
    (53, 8),
    (9, 8),
    (54, 8),
    (8, 8),
    (55, 8),
    (7, 8),
    (56, 8),
    (6, 8),
    (57, 8),
    (5, 8),
    (58, 8),
    (4, 8),
    (59, 8),
    (3, 8),
    (60, 8),
    (2, 8),
    (61, 8),
    (1, 8),
];

/// Number of entries of each Huffman table (`huff_tab_sizes`, atrac3data.h).
pub(crate) const HUFF_TAB_SIZES: [u8; 7] = [9, 5, 7, 9, 15, 31, 63];

/// Bits per value for constant length coding (`clc_length_tab`, atrac3data.h).
pub(crate) const CLC_LENGTH_TAB: [u8; 8] = [0, 4, 3, 3, 4, 4, 5, 6];

/// `mantissa_clc_tab` (atrac3data.h).
pub(crate) const MANTISSA_CLC_TAB: [i8; 4] = [0, 1, -2, -1];

/// `mantissa_vlc_tab` (atrac3data.h).
pub(crate) const MANTISSA_VLC_TAB: [i8; 18] =
    [0, 0, 0, 1, 0, -1, 1, 0, -1, 0, 1, 1, 1, -1, -1, 1, -1, -1];

/// `inv_max_quant` (atrac3data.h).
pub(crate) const INV_MAX_QUANT: [f32; 8] = [
    0.0,
    (1.0_f64 / 1.5) as f32,
    (1.0_f64 / 2.5) as f32,
    (1.0_f64 / 3.5) as f32,
    (1.0_f64 / 4.5) as f32,
    (1.0_f64 / 7.5) as f32,
    (1.0_f64 / 15.5) as f32,
    (1.0_f64 / 31.5) as f32,
];

/// `subband_tab` (atrac3data.h).
pub(crate) const SUBBAND_TAB: [u16; 33] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 288, 320,
    352, 384, 416, 448, 480, 512, 576, 640, 704, 768, 896, 1024,
];

/// `matrix_coeffs` (atrac3data.h).
pub(crate) const MATRIX_COEFFS: [f32; 8] = [0.0, 2.0, 2.0, 2.0, 0.0, 0.0, 1.0, 1.0];
