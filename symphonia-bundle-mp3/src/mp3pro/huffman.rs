// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The SBR bit reader and Huffman codebooks (spec section 8).
//!
//! The ten codebooks are the Huffman tables of ISO/IEC 14496-3 (SBR envelope and noise-floor
//! data). Each is stored as a binary tree in a flat array: the children of node `n` are
//! `book[2 * n]` (bit 0) and `book[2 * n + 1]` (bit 1). A non-negative entry is the index of the
//! next node, a negative entry is a leaf whose decoded value (the signed delta) is `entry + 64`.

/// An MSB-first bit reader over one SBR block.
///
/// Reads past the end of the block return zero bits (spec section 9): the encoder omits trailing
/// zero flags.
pub(super) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// The number of bits read so far (including bits read past the end of the block).
    pub(super) fn pos(&self) -> usize {
        self.pos
    }

    /// Reads a single bit.
    pub(super) fn bit(&mut self) -> u32 {
        let byte = self.data.get(self.pos >> 3).copied().unwrap_or(0);
        let bit = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        u32::from(bit)
    }

    /// Reads `n <= 32` bits as an unsigned integer.
    pub(super) fn get(&mut self, n: u32) -> u32 {
        (0..n).fold(0, |v, _| (v << 1) | self.bit())
    }

    /// Skips `n` bits.
    pub(super) fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}

/// Decodes one Huffman symbol with `book` and returns its signed delta.
pub(super) fn decode(r: &mut BitReader<'_>, book: &[i8]) -> i32 {
    let mut node = 0usize;
    loop {
        let next = book[2 * node + r.bit() as usize];
        if next < 0 {
            return i32::from(next) + 64;
        }
        node = next as usize;
    }
}

/// The envelope codebook for the coding direction (`time`), the coupling channel (`balance`) and
/// the amplitude resolution (`amp_res`: 0 = 1.5 dB, 1 = 3.0 dB).
pub(super) fn env_book(balance: bool, amp_res: i32, time: bool) -> &'static [i8] {
    match (balance, amp_res != 0, time) {
        (false, false, true) => &ENV_LEVEL_10_T,
        (false, false, false) => &ENV_LEVEL_10_F,
        (true, false, true) => &ENV_BALANCE_10_T,
        (true, false, false) => &ENV_BALANCE_10_F,
        (false, true, true) => &ENV_LEVEL_11_T,
        (false, true, false) => &ENV_LEVEL_11_F,
        (true, true, true) => &ENV_BALANCE_11_T,
        (true, true, false) => &ENV_BALANCE_11_F,
    }
}

/// The noise-floor codebook. Frequency-direction noise data uses the 3.0 dB envelope codebooks.
pub(super) fn noise_book(balance: bool, time: bool) -> &'static [i8] {
    match (balance, time) {
        (false, true) => &NOISE_LEVEL_11_T,
        (true, true) => &NOISE_BALANCE_11_T,
        (balance, false) => env_book(balance, 1, false),
    }
}

/// Envelope, level, 1.5 dB, time direction.
static ENV_LEVEL_10_T: [i8; 240] = [
    1, 2, -64, -65, 3, 4, -63, -66, 5, 6, -62, -67, 7, 8, -61, -68, 9, 10, -60, -69, 11, 12, -59,
    -70, 13, 14, -58, -71, 15, 16, -57, -72, 17, 18, -73, -56, 19, 21, -74, 20, -55, -75, 22, 26,
    23, 24, -54, -76, -77, 25, -53, -78, 27, 34, 28, 29, -52, -79, 30, 31, -80, -51, 32, 33, -83,
    -82, -81, -50, 35, 57, 36, 40, 37, 38, -88, -84, -48, 39, -90, -85, 41, 46, 42, 43, -49, -87,
    44, 45, -89, -86, -124, -123, 47, 50, 48, 49, -122, -121, -120, -119, 51, 54, 52, 53, -118,
    -117, -116, -115, 55, 56, -114, -113, -112, -111, 58, 89, 59, 74, 60, 67, 61, 64, 62, 63, -110,
    -109, -108, -107, 65, 66, -106, -105, -104, -103, 68, 71, 69, 70, -102, -101, -100, -99, 72,
    73, -98, -97, -96, -95, 75, 82, 76, 79, 77, 78, -94, -93, -92, -91, 80, 81, -47, -46, -45, -44,
    83, 86, 84, 85, -43, -42, -41, -40, 87, 88, -39, -38, -37, -36, 90, 105, 91, 98, 92, 95, 93,
    94, -35, -34, -33, -32, 96, 97, -31, -30, -29, -28, 99, 102, 100, 101, -27, -26, -25, -24, 103,
    104, -23, -22, -21, -20, 106, 113, 107, 110, 108, 109, -19, -18, -17, -16, 111, 112, -15, -14,
    -13, -12, 114, 117, 115, 116, -11, -10, -9, -8, 118, 119, -7, -6, -5, -4,
];

/// Envelope, level, 1.5 dB, frequency direction.
static ENV_LEVEL_10_F: [i8; 240] = [
    1, 2, -64, -65, 3, 4, -63, -66, 5, 6, -67, -62, 7, 8, -68, -61, 9, 10, -69, -60, 11, 13, -70,
    12, -59, -71, 14, 16, -58, 15, -72, -57, 17, 19, -73, 18, -56, -74, 20, 23, 21, 22, -55, -75,
    -54, -53, 24, 27, 25, 26, -76, -52, -77, -51, 28, 31, 29, 30, -50, -78, -79, -49, 32, 36, 33,
    34, -48, -47, -80, 35, -81, -82, 37, 47, 38, 41, 39, 40, -83, -46, -45, -84, 42, 44, -85, 43,
    -44, -43, 45, 46, -88, -87, -86, -90, 48, 66, 49, 56, 50, 53, 51, 52, -92, -42, -41, -39, 54,
    55, -105, -89, -38, -37, 57, 60, 58, 59, -94, -91, -40, -36, 61, 63, -20, 62, -115, -110, 64,
    65, -108, -107, -101, -97, 67, 89, 68, 75, 69, 72, 70, 71, -95, -93, -34, -27, 73, 74, -22,
    -17, -16, -124, 76, 82, 77, 79, -123, 78, -122, -121, 80, 81, -120, -119, -118, -117, 83, 86,
    84, 85, -116, -114, -113, -112, 87, 88, -111, -109, -106, -104, 90, 105, 91, 98, 92, 95, 93,
    94, -103, -102, -100, -99, 96, 97, -98, -96, -35, -33, 99, 102, 100, 101, -32, -31, -30, -29,
    103, 104, -28, -26, -25, -24, 106, 113, 107, 110, 108, 109, -23, -21, -19, -18, 111, 112, -15,
    -14, -13, -12, 114, 117, 115, 116, -11, -10, -9, -8, 118, 119, -7, -6, -5, -4,
];

/// Envelope, balance, 1.5 dB, time direction.
static ENV_BALANCE_10_T: [i8; 96] = [
    -64, 1, -63, 2, -65, 3, -62, 4, -66, 5, -61, 6, -67, 7, -60, 8, -68, 9, 10, 11, -69, -59, 12,
    13, -70, -58, 14, 28, 15, 21, 16, 18, -57, 17, -71, -56, 19, 20, -88, -87, -86, -85, 22, 25,
    23, 24, -84, -83, -82, -81, 26, 27, -80, -79, -78, -77, 29, 36, 30, 33, 31, 32, -76, -75, -74,
    -73, 34, 35, -72, -55, -54, -53, 37, 41, 38, 39, -52, -51, -50, 40, -49, -48, 42, 45, 43, 44,
    -47, -46, -45, -44, 46, 47, -43, -42, -41, -40,
];

/// Envelope, balance, 1.5 dB, frequency direction.
static ENV_BALANCE_10_F: [i8; 96] = [
    -64, 1, -65, 2, -63, 3, -66, 4, -62, 5, -61, 6, -67, 7, -68, 8, -60, 9, 10, 11, -69, -59, -70,
    12, -58, 13, 14, 17, -71, 15, -57, 16, -56, -73, 18, 32, 19, 25, 20, 22, -72, 21, -88, -87, 23,
    24, -86, -85, -84, -83, 26, 29, 27, 28, -82, -81, -80, -79, 30, 31, -78, -77, -76, -75, 33, 40,
    34, 37, 35, 36, -74, -55, -54, -53, 38, 39, -52, -51, -50, -49, 41, 44, 42, 43, -48, -47, -46,
    -45, 45, 46, -44, -43, -42, 47, -41, -40,
];

/// Envelope, level, 3.0 dB, time direction.
static ENV_LEVEL_11_T: [i8; 124] = [
    -64, 1, -65, 2, -63, 3, -66, 4, -62, 5, -67, 6, -61, 7, -68, 8, -60, 9, 10, 11, -69, -59, 12,
    14, -70, 13, -71, -58, 15, 18, 16, 17, -72, -57, -73, -74, 19, 22, -56, 20, -55, 21, -54, -77,
    23, 31, 24, 25, -75, -76, 26, 27, -78, -53, 28, 29, -52, -95, -94, 30, -93, -92, 32, 47, 33,
    40, 34, 37, 35, 36, -91, -90, -89, -88, 38, 39, -87, -86, -85, -84, 41, 44, 42, 43, -83, -82,
    -81, -80, 45, 46, -79, -51, -50, -49, 48, 55, 49, 52, 50, 51, -48, -47, -46, -45, 53, 54, -44,
    -43, -42, -41, 56, 59, 57, 58, -40, -39, -38, -37, 60, 61, -36, -35, -34, -33,
];

/// Envelope (and noise floor), level, 3.0 dB, frequency direction.
static ENV_LEVEL_11_F: [i8; 124] = [
    -64, 1, -65, 2, -63, 3, -66, 4, -62, 5, -67, 6, 7, 8, -61, -68, 9, 10, -60, -69, 11, 12, -59,
    -70, 13, 14, -58, -71, 15, 16, -57, -72, 17, 19, -56, 18, -55, -73, 20, 24, 21, 22, -74, -54,
    -53, 23, -75, -76, 25, 30, 26, 27, -52, -51, 28, 29, -77, -79, -50, -49, 31, 39, 32, 35, 33,
    34, -78, -46, -82, -88, 36, 37, -83, -48, -47, 38, -86, -85, 40, 47, 41, 44, 42, 43, -80, -44,
    -43, -42, 45, 46, -39, -87, -84, -40, 48, 55, 49, 52, 50, 51, -95, -94, -93, -92, 53, 54, -91,
    -90, -89, -81, 56, 59, 57, 58, -45, -41, -38, -37, 60, 61, -36, -35, -34, -33,
];

/// Envelope, balance, 3.0 dB, time direction.
static ENV_BALANCE_11_T: [i8; 48] = [
    -64, 1, -63, 2, -65, 3, -66, 4, -62, 5, -61, 6, -67, 7, -68, 8, -60, 9, 10, 16, 11, 13, -69,
    12, -76, -75, 14, 15, -74, -73, -72, -71, 17, 20, 18, 19, -70, -59, -58, -57, 21, 22, -56, -55,
    -54, 23, -53, -52,
];

/// Envelope (and noise floor), balance, 3.0 dB, frequency direction.
static ENV_BALANCE_11_F: [i8; 48] = [
    -64, 1, -65, 2, -63, 3, -66, 4, -62, 5, -61, 6, -67, 7, -68, 8, -60, 9, 10, 13, -69, 11, -59,
    12, -58, -76, 14, 17, 15, 16, -75, -74, -73, -72, 18, 21, 19, 20, -71, -70, -57, -56, 22, 23,
    -55, -54, -53, -52,
];

/// Noise floor, level, time direction.
static NOISE_LEVEL_11_T: [i8; 124] = [
    -64, 1, -63, 2, -65, 3, -66, 4, -62, 5, -67, 6, 7, 8, -61, -68, 9, 30, 10, 15, -60, 11, -69,
    12, 13, 14, -59, -53, -95, -94, 16, 23, 17, 20, 18, 19, -93, -92, -91, -90, 21, 22, -89, -88,
    -87, -86, 24, 27, 25, 26, -85, -84, -83, -82, 28, 29, -81, -80, -79, -78, 31, 46, 32, 39, 33,
    36, 34, 35, -77, -76, -75, -74, 37, 38, -73, -72, -71, -70, 40, 43, 41, 42, -58, -57, -56, -55,
    44, 45, -54, -52, -51, -50, 47, 54, 48, 51, 49, 50, -49, -48, -47, -46, 52, 53, -45, -44, -43,
    -42, 55, 58, 56, 57, -41, -40, -39, -38, 59, 60, -37, -36, -35, 61, -34, -33,
];

/// Noise floor, balance, time direction.
static NOISE_BALANCE_11_T: [i8; 48] = [
    -64, 1, -65, 2, -63, 3, 4, 9, -66, 5, -62, 6, 7, 8, -76, -75, -74, -73, 10, 17, 11, 14, 12, 13,
    -72, -71, -70, -69, 15, 16, -68, -67, -61, -60, 18, 21, 19, 20, -59, -58, -57, -56, 22, 23,
    -55, -54, -53, -52,
];
