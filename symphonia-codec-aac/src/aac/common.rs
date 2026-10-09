// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// Previous Author: Kostya Shishkov <kostya.shiskov@gmail.com>
//
// This source file includes code originally written for the NihAV
// project. With the author's permission, it has been relicensed for,
// and ported to the Symphonia project.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub const MAX_WINDOWS: usize = 8;
pub const MAX_SFBS: usize = 64;

pub const ONLY_LONG_SEQUENCE: u8 = 0;
pub const LONG_START_SEQUENCE: u8 = 1;
pub const EIGHT_SHORT_SEQUENCE: u8 = 2;
pub const LONG_STOP_SEQUENCE: u8 = 3;

pub static SWB_OFFSET_48K_LONG: [usize; 49 + 1] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 1024,
];

pub const SWB_OFFSET_48K_SHORT: [usize; 14 + 1] =
    [0, 4, 8, 12, 16, 20, 28, 36, 44, 56, 68, 80, 96, 112, 128];

pub static SWB_OFFSET_32K_LONG: [usize; 51 + 1] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 960, 992, 1024,
];

pub static SWB_OFFSET_8K_LONG: [usize; 40 + 1] = [
    0, 12, 24, 36, 48, 60, 72, 84, 96, 108, 120, 132, 144, 156, 172, 188, 204, 220, 236, 252, 268,
    288, 308, 328, 348, 372, 396, 420, 448, 476, 508, 544, 580, 620, 664, 712, 764, 820, 880, 944,
    1024,
];

pub const SWB_OFFSET_8K_SHORT: [usize; 15 + 1] =
    [0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 60, 72, 88, 108, 128];

pub static SWB_OFFSET_16K_LONG: [usize; 43 + 1] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 100, 112, 124, 136, 148, 160, 172, 184, 196, 212,
    228, 244, 260, 280, 300, 320, 344, 368, 396, 424, 456, 492, 532, 572, 616, 664, 716, 772, 832,
    896, 960, 1024,
];

pub const SWB_OFFSET_16K_SHORT: [usize; 15 + 1] =
    [0, 4, 8, 12, 16, 20, 24, 28, 32, 40, 48, 60, 72, 88, 108, 128];

pub static SWB_OFFSET_24K_LONG: [usize; 47 + 1] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 52, 60, 68, 76, 84, 92, 100, 108, 116, 124, 136,
    148, 160, 172, 188, 204, 220, 240, 260, 284, 308, 336, 364, 396, 432, 468, 508, 552, 600, 652,
    704, 768, 832, 896, 960, 1024,
];

pub const SWB_OFFSET_24K_SHORT: [usize; 15 + 1] =
    [0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 64, 76, 92, 108, 128];

pub static SWB_OFFSET_64K_LONG: [usize; 47 + 1] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 100, 112, 124, 140,
    156, 172, 192, 216, 240, 268, 304, 344, 384, 424, 464, 504, 544, 584, 624, 664, 704, 744, 784,
    824, 864, 904, 944, 984, 1024,
];

pub const SWB_OFFSET_64K_SHORT: [usize; 12 + 1] =
    [0, 4, 8, 12, 16, 20, 24, 32, 40, 48, 64, 92, 128];

pub static SWB_OFFSET_96K_LONG: [usize; 41 + 1] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 96, 108, 120, 132,
    144, 156, 172, 188, 212, 240, 276, 320, 384, 448, 512, 576, 640, 704, 768, 832, 896, 960, 1024,
];

/// The scale factor band offsets of AAC LD and ELD (ISO/IEC 14496-3 §4.5.? tables of the
/// scalefactor bands for the frame lengths of 512 and 480 samples).
pub static SWB_OFFSET_512_48K: [usize; 36 + 1] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60, 68, 76, 84, 92, 100, 112, 124,
    136, 148, 164, 184, 208, 236, 268, 300, 332, 364, 396, 428, 460, 512,
];

pub static SWB_OFFSET_512_32K: [usize; 37 + 1] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 96, 108, 120, 132,
    144, 160, 176, 192, 212, 236, 260, 288, 320, 352, 384, 416, 448, 480, 512,
];

pub static SWB_OFFSET_512_24K: [usize; 31 + 1] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 52, 60, 68, 80, 92, 104, 120, 140, 164, 192, 224,
    256, 288, 320, 352, 384, 416, 448, 480, 512,
];

pub static SWB_OFFSET_480_48K: [usize; 35 + 1] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 96, 108, 120, 132,
    144, 156, 172, 188, 212, 240, 272, 304, 336, 368, 400, 432, 480,
];

pub static SWB_OFFSET_480_32K: [usize; 37 + 1] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60, 64, 72, 80, 88, 96, 104, 112, 124,
    136, 148, 164, 180, 200, 224, 256, 288, 320, 352, 384, 416, 448, 480,
];

pub static SWB_OFFSET_480_24K: [usize; 30 + 1] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 52, 60, 68, 80, 92, 104, 120, 140, 164, 192, 224,
    256, 288, 320, 352, 384, 416, 448, 480,
];

/// A Linear Congruential Generator (LCG) pseudo-random number generator from Numerical Recipes.
#[derive(Clone)]
pub struct Lcg {
    state: u32,
}

impl Lcg {
    pub fn new(state: u32) -> Self {
        Lcg { state }
    }

    #[inline(always)]
    pub fn next(&mut self) -> i32 {
        // Numerical Recipes LCG parameters.
        self.state = self.state.wrapping_mul(1664525).wrapping_add(1013904223);
        self.state as i32
    }
}

#[derive(Clone, Copy)]
pub struct GASubbandInfo {
    pub min_srate: u32,
    pub long_bands: &'static [usize],
    pub short_bands: &'static [usize],
    /// The maximum number of bands TNS applies to in a long window, for the frame lengths of AAC
    /// LD and ELD (480 and 512). Zero for 1024 sample frames, where it depends on the sampling
    /// frequency index only.
    pub tns_max_long_bands: usize,
}

impl GASubbandInfo {
    pub fn find(srate: u32) -> GASubbandInfo {
        for sbi in AAC_SUBBAND_INFO.iter() {
            if srate >= sbi.min_srate {
                return *sbi;
            }
        }
        unreachable!()
    }

    /// Get the scale factor bands for AAC LD and ELD, which have frame lengths of 512 or 480
    /// samples, and a window of one block (there are no short windows).
    ///
    /// The tables are defined for sampling frequencies of 22.05 kHz and above. Lower ones use the
    /// tables of 22.05 kHz and 24 kHz.
    pub fn find_ld(srate: u32, frame_len: usize) -> Option<GASubbandInfo> {
        let (long_bands, tns_max_long_bands): (&'static [usize], usize) = match (frame_len, srate) {
            (512, 46009..) => (&SWB_OFFSET_512_48K, 31),
            (512, 37566..) => (&SWB_OFFSET_512_48K, 32),
            (512, 27713..) => (&SWB_OFFSET_512_32K, 37),
            (512, _) => (&SWB_OFFSET_512_24K, 31),
            (480, 46009..) => (&SWB_OFFSET_480_48K, 31),
            (480, 37566..) => (&SWB_OFFSET_480_48K, 32),
            (480, 27713..) => (&SWB_OFFSET_480_32K, 37),
            (480, _) => (&SWB_OFFSET_480_24K, 30),
            _ => return None,
        };

        Some(GASubbandInfo { min_srate: 0, long_bands, short_bands: &[], tns_max_long_bands })
    }

    pub fn find_idx(srate: u32) -> usize {
        for (i, sbi) in AAC_SUBBAND_INFO.iter().enumerate() {
            if srate >= sbi.min_srate {
                return i;
            }
        }
        unreachable!()
    }
}

static AAC_SUBBAND_INFO: [GASubbandInfo; 12] = [
    GASubbandInfo {
        min_srate: 92017,
        long_bands: &SWB_OFFSET_96K_LONG,
        short_bands: &SWB_OFFSET_64K_SHORT,
        tns_max_long_bands: 0,
    }, //96K
    GASubbandInfo {
        min_srate: 75132,
        long_bands: &SWB_OFFSET_96K_LONG,
        short_bands: &SWB_OFFSET_64K_SHORT,
        tns_max_long_bands: 0,
    }, //88.2K
    GASubbandInfo {
        min_srate: 55426,
        long_bands: &SWB_OFFSET_64K_LONG,
        short_bands: &SWB_OFFSET_64K_SHORT,
        tns_max_long_bands: 0,
    }, //64K
    GASubbandInfo {
        min_srate: 46009,
        long_bands: &SWB_OFFSET_48K_LONG,
        short_bands: &SWB_OFFSET_48K_SHORT,
        tns_max_long_bands: 0,
    }, //48K
    GASubbandInfo {
        min_srate: 37566,
        long_bands: &SWB_OFFSET_48K_LONG,
        short_bands: &SWB_OFFSET_48K_SHORT,
        tns_max_long_bands: 0,
    }, //44.1K
    GASubbandInfo {
        min_srate: 27713,
        long_bands: &SWB_OFFSET_32K_LONG,
        short_bands: &SWB_OFFSET_48K_SHORT,
        tns_max_long_bands: 0,
    }, //32K
    GASubbandInfo {
        min_srate: 23004,
        long_bands: &SWB_OFFSET_24K_LONG,
        short_bands: &SWB_OFFSET_24K_SHORT,
        tns_max_long_bands: 0,
    }, //24K
    GASubbandInfo {
        min_srate: 18783,
        long_bands: &SWB_OFFSET_24K_LONG,
        short_bands: &SWB_OFFSET_24K_SHORT,
        tns_max_long_bands: 0,
    }, //22.05K
    GASubbandInfo {
        min_srate: 13856,
        long_bands: &SWB_OFFSET_16K_LONG,
        short_bands: &SWB_OFFSET_16K_SHORT,
        tns_max_long_bands: 0,
    }, //16K
    GASubbandInfo {
        min_srate: 11502,
        long_bands: &SWB_OFFSET_16K_LONG,
        short_bands: &SWB_OFFSET_16K_SHORT,
        tns_max_long_bands: 0,
    }, //12K
    GASubbandInfo {
        min_srate: 9391,
        long_bands: &SWB_OFFSET_16K_LONG,
        short_bands: &SWB_OFFSET_16K_SHORT,
        tns_max_long_bands: 0,
    }, //11.025K
    GASubbandInfo {
        min_srate: 0,
        long_bands: &SWB_OFFSET_8K_LONG,
        short_bands: &SWB_OFFSET_8K_SHORT,
        tns_max_long_bands: 0,
    }, //8K
];

macro_rules! validate {
    ($a:expr) => {
        if !$a {
            log::error!("check failed at {}:{}", file!(), line!());
            return symphonia_core::errors::decode_error("aac: invalid data");
        }
    };
}

pub(crate) use validate;
