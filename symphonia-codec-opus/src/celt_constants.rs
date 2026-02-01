// CELT Constants and Tables - From xiph/opus celt/celt.h and celt/bands.c
// SPDX-License-Identifier: BSD-3-Clause

/// Maximum number of frequency bands
pub const MAX_BANDS: usize = 21;

/// ICDF table for allocation trim (11 symbols)
/// Values represent cumulative frequencies in descending order
/// From celt/celt.h line 194
pub const TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];

/// ICDF table for spread decision (4 symbols)
/// Probs: NONE: 21.875%, LIGHT: 6.25%, NORMAL: 65.625%, AGGRESSIVE: 6.25%
/// From celt/celt.h line 196
pub const SPREAD_ICDF: [u8; 4] = [25, 23, 2, 0];

/// ICDF table for tapset selection (3 symbols)
/// From celt/celt.h line 198
pub const TAPSET_ICDF: [u8; 3] = [2, 1, 0];

/// Spread modes
pub const SPREAD_NONE: u8 = 0;
pub const SPREAD_LIGHT: u8 = 1;
pub const SPREAD_NORMAL: u8 = 2;
pub const SPREAD_AGGRESSIVE: u8 = 3;

/// Frequency band boundaries for 48kHz, 960-sample frames
/// These define the pseudo-critical bands used by CELT
/// From static_modes_float.h
pub const EBANDS_48K: [i16; 22] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100
];

/// Number of frequency bands
pub const NB_BANDS: usize = 21;

/// Log2 of band sizes
pub const LOG_N: [i16; 21] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 5
];

/// Allocation vectors - bits per band at different rates
/// From static_modes_float.h - allocation matrix
pub const ALLOC_VECTORS: [[u8; 21]; 11] = [
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 2, 3, 3, 4, 4, 4, 4, 5, 5, 6, 6, 7, 8, 8, 9, 10, 10, 11, 12, 13],
    [2, 3, 4, 5, 6, 6, 7, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 21],
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 21, 23, 25, 27],
    [4, 5, 6, 7, 8, 9, 10, 11, 13, 14, 15, 17, 18, 20, 22, 24, 26, 28, 30, 33, 36],
    [5, 6, 7, 8, 9, 11, 12, 14, 15, 17, 19, 21, 23, 25, 27, 30, 32, 35, 38, 41, 45],
    [6, 7, 8, 10, 11, 13, 14, 16, 18, 20, 22, 24, 27, 29, 32, 35, 38, 42, 45, 49, 54],
    [7, 8, 10, 11, 13, 15, 17, 19, 21, 23, 26, 28, 31, 34, 37, 41, 44, 48, 53, 57, 63],
    [8, 9, 11, 13, 15, 17, 19, 21, 24, 26, 29, 32, 35, 39, 42, 46, 51, 55, 60, 66, 72],
    [9, 10, 12, 14, 17, 19, 21, 24, 27, 30, 33, 36, 40, 43, 47, 52, 57, 62, 67, 74, 81],
    [10, 11, 14, 16, 19, 21, 24, 27, 30, 33, 36, 40, 44, 48, 52, 57, 63, 69, 75, 82, 90],
];

/// Coarse energy ICDF for intra-frame coding
/// Different tables for different frame sizes (shortMdct index)
pub const COARSE_ENERGY_INTRA: [[u8; 4]; 3] = [
    // 120 samples
    [72, 127, 65, 0],
    // 240 samples
    [83, 78, 62, 0],
    // other
    [0, 0, 0, 0],
];

/// Coarse energy ICDF for inter-frame coding
pub const COARSE_ENERGY_INTER: [[u8; 4]; 3] = [
    // 120 samples
    [0, 0, 0, 0],
    // 240 samples
    [0, 0, 0, 0],
    // other
    [0, 0, 0, 0],
];

/// Fine energy quantization bits per band
pub const FINE_ENERGY_BITS: [u8; 21] = [
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2
];

/// Cache for pulse vector quantization
/// These tables are used for efficient pulse allocation
pub const CACHE_BITS50: [[u8; 392]; 1] = [[
    40, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 8, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7
]];

/// Maximum frame size (960 samples at 48kHz = 20ms)
pub const MAX_FRAME_SIZE: usize = 960;

/// Maximum period for pitch analysis
pub const MAX_PERIOD: usize = 1024;
