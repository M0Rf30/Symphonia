// Symphonia TAK Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tables of the TAK decoder, from FFmpeg's `libavcodec/takdec.c` (LGPL-2.1+, Paul B Mahol).

/// The parameters of the entropy code of a residue segment (`struct CParam`).
pub struct CParam {
    /// The number of the bits of the first part of a value.
    pub init: u32,
    /// The least value of the first part that may be followed by an escape bit.
    pub escape: u32,
    pub scale: u32,
    /// The least extended value that is followed by an unary coded scale.
    pub aescape: u32,
    pub bias: u32,
}

/// The sizes of the predictors of a subframe, indexed by a 4-bit field (`predictor_sizes`).
pub const PREDICTOR_SIZES: [usize; 16] =
    [4, 8, 12, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 224, 256, 0];

/// The channel decorrelation modes of the multichannel codec (`mc_dmodes`).
pub const MC_DMODES: [u32; 4] = [1, 3, 4, 6];

/// The entropy codes of the residue segments, indexed by the coding mode minus one (`xcodes`).
pub static XCODES: [CParam; 50] = [
    CParam { init: 0x01, escape: 0x0000001, scale: 0x0000001, aescape: 0x0000003, bias: 0x0000008 },
    CParam { init: 0x02, escape: 0x0000003, scale: 0x0000001, aescape: 0x0000007, bias: 0x0000006 },
    CParam { init: 0x03, escape: 0x0000005, scale: 0x0000002, aescape: 0x000000E, bias: 0x000000D },
    CParam { init: 0x03, escape: 0x0000003, scale: 0x0000003, aescape: 0x000000D, bias: 0x0000018 },
    CParam { init: 0x04, escape: 0x000000B, scale: 0x0000004, aescape: 0x000001C, bias: 0x0000019 },
    CParam { init: 0x04, escape: 0x0000006, scale: 0x0000006, aescape: 0x000001A, bias: 0x0000030 },
    CParam { init: 0x05, escape: 0x0000016, scale: 0x0000008, aescape: 0x0000038, bias: 0x0000032 },
    CParam { init: 0x05, escape: 0x000000C, scale: 0x000000C, aescape: 0x0000034, bias: 0x0000060 },
    CParam { init: 0x06, escape: 0x000002C, scale: 0x0000010, aescape: 0x0000070, bias: 0x0000064 },
    CParam { init: 0x06, escape: 0x0000018, scale: 0x0000018, aescape: 0x0000068, bias: 0x00000C0 },
    CParam { init: 0x07, escape: 0x0000058, scale: 0x0000020, aescape: 0x00000E0, bias: 0x00000C8 },
    CParam { init: 0x07, escape: 0x0000030, scale: 0x0000030, aescape: 0x00000D0, bias: 0x0000180 },
    CParam { init: 0x08, escape: 0x00000B0, scale: 0x0000040, aescape: 0x00001C0, bias: 0x0000190 },
    CParam { init: 0x08, escape: 0x0000060, scale: 0x0000060, aescape: 0x00001A0, bias: 0x0000300 },
    CParam { init: 0x09, escape: 0x0000160, scale: 0x0000080, aescape: 0x0000380, bias: 0x0000320 },
    CParam { init: 0x09, escape: 0x00000C0, scale: 0x00000C0, aescape: 0x0000340, bias: 0x0000600 },
    CParam { init: 0x0A, escape: 0x00002C0, scale: 0x0000100, aescape: 0x0000700, bias: 0x0000640 },
    CParam { init: 0x0A, escape: 0x0000180, scale: 0x0000180, aescape: 0x0000680, bias: 0x0000C00 },
    CParam { init: 0x0B, escape: 0x0000580, scale: 0x0000200, aescape: 0x0000E00, bias: 0x0000C80 },
    CParam { init: 0x0B, escape: 0x0000300, scale: 0x0000300, aescape: 0x0000D00, bias: 0x0001800 },
    CParam { init: 0x0C, escape: 0x0000B00, scale: 0x0000400, aescape: 0x0001C00, bias: 0x0001900 },
    CParam { init: 0x0C, escape: 0x0000600, scale: 0x0000600, aescape: 0x0001A00, bias: 0x0003000 },
    CParam { init: 0x0D, escape: 0x0001600, scale: 0x0000800, aescape: 0x0003800, bias: 0x0003200 },
    CParam { init: 0x0D, escape: 0x0000C00, scale: 0x0000C00, aescape: 0x0003400, bias: 0x0006000 },
    CParam { init: 0x0E, escape: 0x0002C00, scale: 0x0001000, aescape: 0x0007000, bias: 0x0006400 },
    CParam { init: 0x0E, escape: 0x0001800, scale: 0x0001800, aescape: 0x0006800, bias: 0x000C000 },
    CParam { init: 0x0F, escape: 0x0005800, scale: 0x0002000, aescape: 0x000E000, bias: 0x000C800 },
    CParam { init: 0x0F, escape: 0x0003000, scale: 0x0003000, aescape: 0x000D000, bias: 0x0018000 },
    CParam { init: 0x10, escape: 0x000B000, scale: 0x0004000, aescape: 0x001C000, bias: 0x0019000 },
    CParam { init: 0x10, escape: 0x0006000, scale: 0x0006000, aescape: 0x001A000, bias: 0x0030000 },
    CParam { init: 0x11, escape: 0x0016000, scale: 0x0008000, aescape: 0x0038000, bias: 0x0032000 },
    CParam { init: 0x11, escape: 0x000C000, scale: 0x000C000, aescape: 0x0034000, bias: 0x0060000 },
    CParam { init: 0x12, escape: 0x002C000, scale: 0x0010000, aescape: 0x0070000, bias: 0x0064000 },
    CParam { init: 0x12, escape: 0x0018000, scale: 0x0018000, aescape: 0x0068000, bias: 0x00C0000 },
    CParam { init: 0x13, escape: 0x0058000, scale: 0x0020000, aescape: 0x00E0000, bias: 0x00C8000 },
    CParam { init: 0x13, escape: 0x0030000, scale: 0x0030000, aescape: 0x00D0000, bias: 0x0180000 },
    CParam { init: 0x14, escape: 0x00B0000, scale: 0x0040000, aescape: 0x01C0000, bias: 0x0190000 },
    CParam { init: 0x14, escape: 0x0060000, scale: 0x0060000, aescape: 0x01A0000, bias: 0x0300000 },
    CParam { init: 0x15, escape: 0x0160000, scale: 0x0080000, aescape: 0x0380000, bias: 0x0320000 },
    CParam { init: 0x15, escape: 0x00C0000, scale: 0x00C0000, aescape: 0x0340000, bias: 0x0600000 },
    CParam { init: 0x16, escape: 0x02C0000, scale: 0x0100000, aescape: 0x0700000, bias: 0x0640000 },
    CParam { init: 0x16, escape: 0x0180000, scale: 0x0180000, aescape: 0x0680000, bias: 0x0C00000 },
    CParam { init: 0x17, escape: 0x0580000, scale: 0x0200000, aescape: 0x0E00000, bias: 0x0C80000 },
    CParam { init: 0x17, escape: 0x0300000, scale: 0x0300000, aescape: 0x0D00000, bias: 0x1800000 },
    CParam { init: 0x18, escape: 0x0B00000, scale: 0x0400000, aescape: 0x1C00000, bias: 0x1900000 },
    CParam { init: 0x18, escape: 0x0600000, scale: 0x0600000, aescape: 0x1A00000, bias: 0x3000000 },
    CParam { init: 0x19, escape: 0x1600000, scale: 0x0800000, aescape: 0x3800000, bias: 0x3200000 },
    CParam { init: 0x19, escape: 0x0C00000, scale: 0x0C00000, aescape: 0x3400000, bias: 0x6000000 },
    CParam { init: 0x1A, escape: 0x2C00000, scale: 0x1000000, aescape: 0x7000000, bias: 0x6400000 },
    CParam { init: 0x1A, escape: 0x1800000, scale: 0x1800000, aescape: 0x6800000, bias: 0xC000000 },
];
