// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Constant tables of MPEG-4 ALS (ISO/IEC 14496-3 subpart 11), as laid out in FFmpeg
//! `libavcodec/alsdec.c`.

/// Rice parameters and index offsets, `(offset, rice parameter)`, for decoding the indices of the
/// scaled PARCOR values. The table is selected by `coef_table` of the `ALSSpecificConfig`.
pub static PARCOR_RICE_TABLE: [[(i8, u8); 20]; 3] = [
    [
        (-52, 4),
        (-29, 5),
        (-31, 4),
        (19, 4),
        (-16, 4),
        (12, 3),
        (-7, 3),
        (9, 3),
        (-5, 3),
        (6, 3),
        (-4, 3),
        (3, 3),
        (-3, 2),
        (3, 2),
        (-2, 2),
        (3, 2),
        (-1, 2),
        (2, 2),
        (-1, 2),
        (2, 2),
    ],
    [
        (-58, 3),
        (-42, 4),
        (-46, 4),
        (37, 5),
        (-36, 4),
        (29, 4),
        (-29, 4),
        (25, 4),
        (-23, 4),
        (20, 4),
        (-17, 4),
        (16, 4),
        (-12, 4),
        (12, 3),
        (-10, 4),
        (7, 3),
        (-4, 4),
        (3, 3),
        (-1, 3),
        (1, 3),
    ],
    [
        (-59, 3),
        (-45, 5),
        (-50, 4),
        (38, 4),
        (-39, 4),
        (32, 4),
        (-30, 4),
        (25, 3),
        (-23, 3),
        (20, 3),
        (-20, 3),
        (16, 3),
        (-13, 3),
        (10, 3),
        (-7, 3),
        (3, 3),
        (0, 3),
        (-1, 3),
        (2, 3),
        (-1, 2),
    ],
];

/// Scaled PARCOR values used for the first two PARCOR coefficients, indexed by the Rice coded
/// indices: `(32 + ((i * (i + 1)) << 7) - (1 << 20)) / 32` (the division is exact).
pub static PARCOR_SCALED_VALUES: [i16; 128] = {
    let mut table = [0i16; 128];
    let mut i = 0;
    while i < 128 {
        table[i] = ((32 + (((i * (i + 1)) as i32) << 7) - (1 << 20)) / 32) as i16;
        i += 1;
    }
    table
};

/// Gain values of p(0) for long-term prediction, indexed by the Rice coded indices.
pub static LTP_GAIN_VALUES: [[u8; 4]; 4] =
    [[0, 8, 16, 24], [32, 40, 48, 56], [64, 70, 76, 82], [88, 92, 96, 100]];

/// Inter-channel weighting factors for multi-channel correlation.
pub static MCC_WEIGHTINGS: [i16; 32] = [
    204, 192, 179, 166, 153, 140, 128, 115, 102, 89, 76, 64, 51, 38, 25, 12, 0, -12, -25, -38, -51,
    -64, -76, -89, -102, -115, -128, -140, -153, -166, -179, -192,
];

/// Tail codes used in arithmetic coding using block Gilbert-Moore codes.
pub static TAIL_CODE: [[u8; 6]; 16] = [
    [74, 44, 25, 13, 7, 3],
    [68, 42, 24, 13, 7, 3],
    [58, 39, 23, 13, 7, 3],
    [126, 70, 37, 19, 10, 5],
    [132, 70, 37, 20, 10, 5],
    [124, 70, 38, 20, 10, 5],
    [120, 69, 37, 20, 11, 5],
    [116, 67, 37, 20, 11, 5],
    [108, 66, 36, 20, 10, 5],
    [102, 62, 36, 20, 10, 5],
    [88, 58, 34, 19, 10, 5],
    [162, 89, 49, 25, 13, 7],
    [156, 87, 49, 26, 14, 7],
    [150, 86, 47, 26, 14, 7],
    [142, 84, 47, 26, 14, 7],
    [131, 79, 46, 26, 14, 7],
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_parcor_scaled_values() {
        // Spot checks against the table printed in FFmpeg alsdec.c.
        assert_eq!(i32::from(PARCOR_SCALED_VALUES[0]), -1048544 / 32);
        assert_eq!(i32::from(PARCOR_SCALED_VALUES[1]), -1048288 / 32);
        assert_eq!(i32::from(PARCOR_SCALED_VALUES[89]), -23264 / 32);
        assert_eq!(i32::from(PARCOR_SCALED_VALUES[90]), -224 / 32);
        assert_eq!(i32::from(PARCOR_SCALED_VALUES[91]), 23072 / 32);
        assert_eq!(i32::from(PARCOR_SCALED_VALUES[127]), 1032224 / 32);
    }
}
