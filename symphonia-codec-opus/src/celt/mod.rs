// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CELT (Constrained Energy Lapped Transform) decoder module.
//!
//! CELT is the frequency-domain component of the Opus codec, used for
//! general audio and music.
//!
//! This implementation is ported from lu-zero's opus decoder.

mod bitexact;
mod imdct15;

use imdct15::IMDCT15;

use std::mem;
use std::ops::Range;

use symphonia_core::audio::{AudioBuffer, AudioBufferRef, AsAudioBufferRef, Signal, SignalSpec};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::errors::Result;
use symphonia_core::formats::Packet;

use crate::entropy::{CeltRangeDecoder as RangeDecoder, ICDFContext};
use crate::maths::*;
use crate::packet::*;
use crate::toc::FrameDuration;

const SHORT_BLOCKSIZE: usize = 120;
const OVERLAP: usize = SHORT_BLOCKSIZE;
const MAX_LOG_BLOCKS: usize = 3;
const MAX_FRAME_SIZE: usize = SHORT_BLOCKSIZE * (1 << MAX_LOG_BLOCKS);

const MAX_BANDS: usize = 21;
const MIN_PERIOD: usize = 15;

const SPREAD_NONE: usize = 0;
const SPREAD_LIGHT: usize = 1;
const SPREAD_NORMAL: usize = 2;
const SPREAD_AGGRESSIVE: usize = 3;

#[derive(Debug, Default)]
struct PostFilter {
    period: usize,
    period_new: usize,
    period_old: usize,

    gains: [f32; 3],
    gains_new: [f32; 3],
    gains_old: [f32; 3],
}

#[derive(Debug)]
struct CeltFrame {
    pf: PostFilter,
    energy: [f32; MAX_BANDS],
    prev_energy: [f32; MAX_BANDS],
    collapse_masks: [u8; MAX_BANDS],

    buf: Vec<f32>, // TODO: replace with an array once const-generics

    deemph_coeff: f32,
}

impl Default for CeltFrame {
    fn default() -> Self {
        CeltFrame {
            pf: Default::default(),
            energy: Default::default(),
            prev_energy: Default::default(),
            collapse_masks: Default::default(),

            buf: vec![0f32; 2048],

            deemph_coeff: 0f32,
        }
    }
}

// #[derive(Debug)]
pub struct Celt {
    stereo: bool,
    stereo_pkt: bool,
    bits: usize,
    lm: usize, // aka duration in mdct blocks
    band: Range<usize>,
    frames: [CeltFrame; 2],
    spread: usize,

    fine_bits: [i32; MAX_BANDS],
    fine_priority: [bool; MAX_BANDS],
    pulses: [i32; MAX_BANDS],
    tf_change: [i8; MAX_BANDS],

    anticollapse_bit: usize,
    blocks: usize,
    blocksize: usize,

    intensity_stereo: usize,
    dual_stereo: bool,

    remaining: i32,
    remaining2: i32,
    codedband: usize,

    scratch: [f32; 22 * 8],
    seed: u32,

    // IMDCT synthesis
    imdct: [IMDCT15; 4], // One for each lm value (0-3)
    overlap: [[f32; 960]; 2], // Overlap buffers for each channel (max frame size)
}

const POSTFILTER_TAPS: &[&[f32]] = &[
    &[0.3066406250, 0.2170410156, 0.1296386719],
    &[0.4638671875, 0.2680664062, 0.0],
    &[0.7998046875, 0.1000976562, 0.0],
];

const TAPSET: &ICDFContext = &ICDFContext {
    total: 4,
    dist: &[2, 3, 4],
};

const ALPHA_COEF: &[f32] = &[
    29440.0 / 32768.0,
    26112.0 / 32768.0,
    21248.0 / 32768.0,
    16384.0 / 32768.0,
];

const BETA_COEF: &[f32] = &[
    1.0 - 30147.0 / 32768.0,
    1.0 - 22282.0 / 32768.0,
    1.0 - 12124.0 / 32768.0,
    1.0 - 6554.0 / 32768.0,
];

// TODO: make it a &[&[(u8, u8)]] if it makes no speed difference
const COARSE_ENERGY_INTRA: &[&[u8]] = &[
    // 120-samples
    &[
        24, 179, 48, 138, 54, 135, 54, 132, 53, 134, 56, 133, 55, 132, 55, 132, 61, 114, 70, 96,
        74, 88, 75, 88, 87, 74, 89, 66, 91, 67, 100, 59, 108, 50, 120, 40, 122, 37, 97, 43, 78, 50,
    ],
    // 240-samples
    &[
        23, 178, 54, 115, 63, 102, 66, 98, 69, 99, 74, 89, 71, 91, 73, 91, 78, 89, 86, 80, 92, 66,
        93, 64, 102, 59, 103, 60, 104, 60, 117, 52, 123, 44, 138, 35, 133, 31, 97, 38, 77, 45,
    ],
    // 480-samples
    &[
        21, 178, 59, 110, 71, 86, 75, 85, 84, 83, 91, 66, 88, 73, 87, 72, 92, 75, 98, 72, 105, 58,
        107, 54, 115, 52, 114, 55, 112, 56, 129, 51, 132, 40, 150, 33, 140, 29, 98, 35, 77, 42,
    ],
    // 960-samples
    &[
        22, 178, 63, 114, 74, 82, 84, 83, 92, 82, 103, 62, 96, 72, 96, 67, 101, 73, 107, 72, 113,
        55, 118, 52, 125, 52, 118, 52, 117, 55, 135, 49, 137, 39, 157, 32, 145, 29, 97, 33, 77, 40,
    ],
];

const COARSE_ENERGY_INTER: &[&[u8]] = &[
    // 120-samples
    &[
        72, 127, 65, 129, 66, 128, 65, 128, 64, 128, 62, 128, 64, 128, 64, 128, 92, 78, 92, 79, 92,
        78, 90, 79, 116, 41, 115, 40, 114, 40, 132, 26, 132, 26, 145, 17, 161, 12, 176, 10, 177,
        11,
    ],
    // 240-samples
    &[
        83, 78, 84, 81, 88, 75, 86, 74, 87, 71, 90, 73, 93, 74, 93, 74, 109, 40, 114, 36, 117, 34,
        117, 34, 143, 17, 145, 18, 146, 19, 162, 12, 165, 10, 178, 7, 189, 6, 190, 8, 177, 9,
    ],
    // 480-samples
    &[
        61, 90, 93, 60, 105, 42, 107, 41, 110, 45, 116, 38, 113, 38, 112, 38, 124, 26, 132, 27,
        136, 19, 140, 20, 155, 14, 159, 16, 158, 18, 170, 13, 177, 10, 187, 8, 192, 6, 175, 9, 159,
        10,
    ],
    // 960-samples
    &[
        42, 121, 96, 66, 108, 43, 111, 40, 117, 44, 123, 32, 120, 36, 119, 33, 127, 33, 134, 34,
        139, 21, 147, 23, 152, 20, 158, 25, 154, 26, 166, 21, 173, 16, 184, 13, 184, 10, 150, 13,
        139, 15,
    ],
];

const STATIC_CAPS: &[&[&[u8]]] = &[
    // 120-sample
    &[
        &[
            224, 224, 224, 224, 224, 224, 224, 224, 160, 160, 160, 160, 185, 185, 185, 178, 178,
            168, 134, 61, 37,
        ],
        &[
            224, 224, 224, 224, 224, 224, 224, 224, 240, 240, 240, 240, 207, 207, 207, 198, 198,
            183, 144, 66, 40,
        ],
    ],
    // 240-sample
    &[
        &[
            160, 160, 160, 160, 160, 160, 160, 160, 185, 185, 185, 185, 193, 193, 193, 183, 183,
            172, 138, 64, 38,
        ],
        &[
            240, 240, 240, 240, 240, 240, 240, 240, 207, 207, 207, 207, 204, 204, 204, 193, 193,
            180, 143, 66, 40,
        ],
    ],
    // 480-sample
    &[
        &[
            185, 185, 185, 185, 185, 185, 185, 185, 193, 193, 193, 193, 193, 193, 193, 183, 183,
            172, 138, 65, 39,
        ],
        &[
            207, 207, 207, 207, 207, 207, 207, 207, 204, 204, 204, 204, 201, 201, 201, 188, 188,
            176, 141, 66, 40,
        ],
    ],
    // 960-sample
    &[
        &[
            193, 193, 193, 193, 193, 193, 193, 193, 193, 193, 193, 193, 194, 194, 194, 184, 184,
            173, 139, 65, 39,
        ],
        &[
            204, 204, 204, 204, 204, 204, 204, 204, 201, 201, 201, 201, 198, 198, 198, 187, 187,
            175, 140, 66, 40,
        ],
    ],
];

const FREQ_RANGE: &[u8] = &[
    1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 4, 4, 4, 6, 6, 8, 12, 18, 22,
];

const MODEL_ENERGY_SMALL: &ICDFContext = &ICDFContext {
    total: 4,
    dist: &[2, 3, 4],
};

const TF_SELECT: &[[[[i8; 2]; 2]; 2]] = &[
    [[[0, -1], [0, -1]], [[0, -1], [0, -1]]],
    [[[0, -1], [0, -2]], [[1, 0], [1, -1]]],
    [[[0, -2], [0, -3]], [[2, 0], [1, -1]]],
    [[[0, -2], [0, -3]], [[3, 0], [1, -1]]],
];

const MODEL_SPREAD: &ICDFContext = &ICDFContext {
    total: 32,
    dist: &[7, 9, 30, 32],
};

const ALLOC_TRIM: &ICDFContext = &ICDFContext {
    total: 128,
    dist: &[2, 4, 9, 19, 41, 87, 109, 119, 124, 126, 128],
};

const LOG2_FRAC: &[u8] = &[
    0, 8, 13, 16, 19, 21, 23, 24, 26, 27, 28, 29, 30, 31, 32, 32, 33, 34, 34, 35, 36, 36, 37, 37,
];

const STATIC_ALLOC: &[[u8; 21]; 11] = &[
    /* 1/32 bit/sample */
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ],
    [
        90, 80, 75, 69, 63, 56, 49, 40, 34, 29, 20, 18, 10, 0, 0, 0, 0, 0, 0, 0, 0,
    ],
    [
        110, 100, 90, 84, 78, 71, 65, 58, 51, 45, 39, 32, 26, 20, 12, 0, 0, 0, 0, 0, 0,
    ],
    [
        118, 110, 103, 93, 86, 80, 75, 70, 65, 59, 53, 47, 40, 31, 23, 15, 4, 0, 0, 0, 0,
    ],
    [
        126, 119, 112, 104, 95, 89, 83, 78, 72, 66, 60, 54, 47, 39, 32, 25, 17, 12, 1, 0, 0,
    ],
    [
        134, 127, 120, 114, 103, 97, 91, 85, 78, 72, 66, 60, 54, 47, 41, 35, 29, 23, 16, 10, 1,
    ],
    [
        144, 137, 130, 124, 113, 107, 101, 95, 88, 82, 76, 70, 64, 57, 51, 45, 39, 33, 26, 15, 1,
    ],
    [
        152, 145, 138, 132, 123, 117, 111, 105, 98, 92, 86, 80, 74, 67, 61, 55, 49, 43, 36, 20, 1,
    ],
    [
        162, 155, 148, 142, 133, 127, 121, 115, 108, 102, 96, 90, 84, 77, 71, 65, 59, 53, 46, 30, 1,
    ],
    [
        172, 165, 158, 152, 143, 137, 131, 125, 118, 112, 106, 100, 94, 87, 81, 75, 69, 63, 56, 45,
        20,
    ],
    [
        200, 200, 200, 200, 200, 200, 200, 200, 198, 193, 188, 183, 178, 173, 168, 163, 158, 153,
        148, 129, 104,
    ],
];

const FREQ_BANDS: &[u8] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100,
];

const LOG_FREQ_RANGE: &[u8] = &[
    0, 0, 0, 0, 0, 0, 0, 0, 8, 8, 8, 8, 16, 16, 16, 21, 21, 24, 29, 34, 36,
];

const MAX_FINE_BITS: i32 = 8;

const BIT_INTERLEAVE: &[u8] = &[0, 1, 1, 1, 2, 3, 3, 3, 2, 3, 3, 3, 2, 3, 3, 3];

const PVQ_U: &[u32] = &[
    /* N = 0, K = 0...176 */
    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, /* N = 1, K = 1...176 */
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, /* N = 2, K = 2...176 */
    3, 5, 7, 9, 11, 13, 15, 17, 19, 21, 23, 25, 27, 29, 31, 33, 35, 37, 39, 41, 43, 45, 47, 49, 51,
    53, 55, 57, 59, 61, 63, 65, 67, 69, 71, 73, 75, 77, 79, 81, 83, 85, 87, 89, 91, 93, 95, 97, 99,
    101, 103, 105, 107, 109, 111, 113, 115, 117, 119, 121, 123, 125, 127, 129, 131, 133, 135, 137,
    139, 141, 143, 145, 147, 149, 151, 153, 155, 157, 159, 161, 163, 165, 167, 169, 171, 173, 175,
    177, 179, 181, 183, 185, 187, 189, 191, 193, 195, 197, 199, 201, 203, 205, 207, 209, 211, 213,
    215, 217, 219, 221, 223, 225, 227, 229, 231, 233, 235, 237, 239, 241, 243, 245, 247, 249, 251,
    253, 255, 257, 259, 261, 263, 265, 267, 269, 271, 273, 275, 277, 279, 281, 283, 285, 287, 289,
    291, 293, 295, 297, 299, 301, 303, 305, 307, 309, 311, 313, 315, 317, 319, 321, 323, 325, 327,
    329, 331, 333, 335, 337, 339, 341, 343, 345, 347, 349, 351, /* N = 3, K = 3...176 */
    13, 25, 41, 61, 85, 113, 145, 181, 221, 265, 313, 365, 421, 481, 545, 613, 685, 761, 841, 925,
    1013, 1105, 1201, 1301, 1405, 1513, 1625, 1741, 1861, 1985, 2113, 2245, 2381, 2521, 2665, 2813,
    2965, 3121, 3281, 3445, 3613, 3785, 3961, 4141, 4325, 4513, 4705, 4901, 5101, 5305, 5513, 5725,
    5941, 6161, 6385, 6613, 6845, 7081, 7321, 7565, 7813, 8065, 8321, 8581, 8845, 9113, 9385, 9661,
    9941, 10225, 10513, 10805, 11101, 11401, 11705, 12013, 12325, 12641, 12961, 13285, 13613,
    13945, 14281, 14621, 14965, 15313, 15665, 16021, 16381, 16745, 17113, 17485, 17861, 18241,
    18625, 19013, 19405, 19801, 20201, 20605, 21013, 21425, 21841, 22261, 22685, 23113, 23545,
    23981, 24421, 24865, 25313, 25765, 26221, 26681, 27145, 27613, 28085, 28561, 29041, 29525,
    30013, 30505, 31001, 31501, 32005, 32513, 33025, 33541, 34061, 34585, 35113, 35645, 36181,
    36721, 37265, 37813, 38365, 38921, 39481, 40045, 40613, 41185, 41761, 42341, 42925, 43513,
    44105, 44701, 45301, 45905, 46513, 47125, 47741, 48361, 48985, 49613, 50245, 50881, 51521,
    52165, 52813, 53465, 54121, 54781, 55445, 56113, 56785, 57461, 58141, 58825, 59513, 60205,
    60901, 61601, /* N = 4, K = 4...176 */
    63, 129, 231, 377, 575, 833, 1159, 1561, 2047, 2625, 3303, 4089, 4991, 6017, 7175, 8473, 9919,
    11521, 13287, 15225, 17343, 19649, 22151, 24857, 27775, 30913, 34279, 37881, 41727, 45825,
    50183, 54809, 59711, 64897, 70375, 76153, 82239, 88641, 95367, 102425, 109823, 117569, 125671,
    134137, 142975, 152193, 161799, 171801, 182207, 193025, 204263, 215929, 228031, 240577, 253575,
    267033, 280959, 295361, 310247, 325625, 341503, 357889, 374791, 392217, 410175, 428673, 447719,
    467321, 487487, 508225, 529543, 551449, 573951, 597057, 620775, 645113, 670079, 695681, 721927,
    748825, 776383, 804609, 833511, 863097, 893375, 924353, 956039, 988441, 1021567, 1055425,
    1090023, 1125369, 1161471, 1198337, 1235975, 1274393, 1313599, 1353601, 1394407, 1436025,
    1478463, 1521729, 1565831, 1610777, 1656575, 1703233, 1750759, 1799161, 1848447, 1898625,
    1949703, 2001689, 2054591, 2108417, 2163175, 2218873, 2275519, 2333121, 2391687, 2451225,
    2511743, 2573249, 2635751, 2699257, 2763775, 2829313, 2895879, 2963481, 3032127, 3101825,
    3172583, 3244409, 3317311, 3391297, 3466375, 3542553, 3619839, 3698241, 3777767, 3858425,
    3940223, 4023169, 4107271, 4192537, 4278975, 4366593, 4455399, 4545401, 4636607, 4729025,
    4822663, 4917529, 5013631, 5110977, 5209575, 5309433, 5410559, 5512961, 5616647, 5721625,
    5827903, 5935489, 6044391, 6154617, 6266175, 6379073, 6493319, 6608921, 6725887, 6844225,
    6963943, 7085049, 7207551, /* N = 5, K = 5...176 */
    321, 681, 1289, 2241, 3649, 5641, 8361, 11969, 16641, 22569, 29961, 39041, 50049, 63241, 78889,
    97281, 118721, 143529, 172041, 204609, 241601, 283401, 330409, 383041, 441729, 506921, 579081,
    658689, 746241, 842249, 947241, 1061761, 1186369, 1321641, 1468169, 1626561, 1797441, 1981449,
    2179241, 2391489, 2618881, 2862121, 3121929, 3399041, 3694209, 4008201, 4341801, 4695809,
    5071041, 5468329, 5888521, 6332481, 6801089, 7295241, 7815849, 8363841, 8940161, 9545769,
    10181641, 10848769, 11548161, 12280841, 13047849, 13850241, 14689089, 15565481, 16480521,
    17435329, 18431041, 19468809, 20549801, 21675201, 22846209, 24064041, 25329929, 26645121,
    28010881, 29428489, 30899241, 32424449, 34005441, 35643561, 37340169, 39096641, 40914369,
    42794761, 44739241, 46749249, 48826241, 50971689, 53187081, 55473921, 57833729, 60268041,
    62778409, 65366401, 68033601, 70781609, 73612041, 76526529, 79526721, 82614281, 85790889,
    89058241, 92418049, 95872041, 99421961, 103069569, 106816641, 110664969, 114616361, 118672641,
    122835649, 127107241, 131489289, 135983681, 140592321, 145317129, 150160041, 155123009,
    160208001, 165417001, 170752009, 176215041, 181808129, 187533321, 193392681, 199388289,
    205522241, 211796649, 218213641, 224775361, 231483969, 238341641, 245350569, 252512961,
    259831041, 267307049, 274943241, 282741889, 290705281, 298835721, 307135529, 315607041,
    324252609, 333074601, 342075401, 351257409, 360623041, 370174729, 379914921, 389846081,
    399970689, 410291241, 420810249, 431530241, 442453761, 453583369, 464921641, 476471169,
    488234561, 500214441, 512413449, 524834241, 537479489, 550351881, 563454121, 576788929,
    590359041, 604167209, 618216201, 632508801,
    /* N = 6, K = 6...96 (technically V(109,5) fits in 32 bits, but that can't be
    achieved by splitting an Opus band) */
    1683, 3653, 7183, 13073, 22363, 36365, 56695, 85305, 124515, 177045, 246047, 335137, 448427,
    590557, 766727, 982729, 1244979, 1560549, 1937199, 2383409, 2908411, 3522221, 4235671, 5060441,
    6009091, 7095093, 8332863, 9737793, 11326283, 13115773, 15124775, 17372905, 19880915, 22670725,
    25765455, 29189457, 32968347, 37129037, 41699767, 46710137, 52191139, 58175189, 64696159,
    71789409, 79491819, 87841821, 96879431, 106646281, 117185651, 128542501, 140763503, 153897073,
    167993403, 183104493, 199284183, 216588185, 235074115, 254801525, 275831935, 298228865,
    322057867, 347386557, 374284647, 402823977, 433078547, 465124549, 499040399, 534906769,
    572806619, 612825229, 655050231, 699571641, 746481891, 795875861, 847850911, 902506913,
    959946283, 1020274013, 1083597703, 1150027593, 1219676595, 1292660325, 1369097135, 1449108145,
    1532817275, 1620351277, 1711839767, 1807415257, 1907213187, 2011371957, 2120032959,
    /* N = 7, K = 7...54 (technically V(60,6) fits in 32 bits, but that can't be
    achieved by splitting an Opus band) */
    8989, 19825, 40081, 75517, 134245, 227305, 369305, 579125, 880685, 1303777, 1884961, 2668525,
    3707509, 5064793, 6814249, 9041957, 11847485, 15345233, 19665841, 24957661, 31388293, 39146185,
    48442297, 59511829, 72616013, 88043969, 106114625, 127178701, 151620757, 179861305, 212358985,
    249612805, 292164445, 340600625, 395555537, 457713341, 527810725, 606639529, 695049433,
    793950709, 904317037, 1027188385, 1163673953, 1314955181, 1482288821, 1667010073, 1870535785,
    2094367717,
    /* N = 8, K = 8...37 (technically V(40,7) fits in 32 bits, but that can't be
    achieved by splitting an Opus band) */
    48639, 108545, 224143, 433905, 795455, 1392065, 2340495, 3800305, 5984767, 9173505, 13726991,
    20103025, 28875327, 40754369, 56610575, 77500017, 104692735, 139703809, 184327311, 240673265,
    311207743, 398796225, 506750351, 638878193, 799538175, 993696769, 1226990095, 1505789553,
    1837271615, 2229491905,
    /* N = 9, K = 9...28 (technically V(29,8) fits in 32 bits, but that can't be
    achieved by splitting an Opus band) */
    265729, 598417, 1256465, 2485825, 4673345, 8405905, 14546705, 24331777, 39490049, 62390545,
    96220561, 145198913, 214828609, 312193553, 446304145, 628496897, 872893441, 1196924561,
    1621925137, 2173806145, /* N = 10, K = 10...24 */
    1462563, 3317445, 7059735, 14218905, 27298155, 50250765, 89129247, 152951073, 254831667,
    413442773, 654862247, 1014889769, 1541911931, 2300409629, 3375210671,
    /* N = 11, K = 11...19 (technically V(20,10) fits in 32 bits, but that can't be
    achieved by splitting an Opus band) */
    8097453, 18474633, 39753273, 81270333, 158819253, 298199265, 540279585, 948062325, 1616336765,
    /* N = 12, K = 12...18 */
    45046719, 103274625, 224298231, 464387817, 921406335, 1759885185, 3248227095,
    /* N = 13, K = 13...16 */
    251595969, 579168825, 1267854873, 2653649025, /* N = 14, K = 14 */
    1409933619,
];

const PVQ_U_ROW: &[usize] = &[
    0, 176, 351, 525, 698, 870, 1041, 1131, 1178, 1207, 1226, 1240, 1248, 1254, 1257,
];

#[inline(always)]
fn pvq_u_row(row_index: usize) -> &'static [u32] {
    &PVQ_U[PVQ_U_ROW[row_index]..]
}

const CACHE_BITS: &[u8] = &[
    40, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 40, 15, 23, 28, 31, 34, 36, 38, 39, 41, 42, 43, 44, 45, 46, 47,
    47, 49, 50, 51, 52, 53, 54, 55, 55, 57, 58, 59, 60, 61, 62, 63, 63, 65, 66, 67, 68, 69, 70, 71,
    71, 40, 20, 33, 41, 48, 53, 57, 61, 64, 66, 69, 71, 73, 75, 76, 78, 80, 82, 85, 87, 89, 91, 92,
    94, 96, 98, 101, 103, 105, 107, 108, 110, 112, 114, 117, 119, 121, 123, 124, 126, 128, 40, 23,
    39, 51, 60, 67, 73, 79, 83, 87, 91, 94, 97, 100, 102, 105, 107, 111, 115, 118, 121, 124, 126,
    129, 131, 135, 139, 142, 145, 148, 150, 153, 155, 159, 163, 166, 169, 172, 174, 177, 179, 35,
    28, 49, 65, 78, 89, 99, 107, 114, 120, 126, 132, 136, 141, 145, 149, 153, 159, 165, 171, 176,
    180, 185, 189, 192, 199, 205, 211, 216, 220, 225, 229, 232, 239, 245, 251, 21, 33, 58, 79, 97,
    112, 125, 137, 148, 157, 166, 174, 182, 189, 195, 201, 207, 217, 227, 235, 243, 251, 17, 35,
    63, 86, 106, 123, 139, 152, 165, 177, 187, 197, 206, 214, 222, 230, 237, 250, 25, 31, 55, 75,
    91, 105, 117, 128, 138, 146, 154, 161, 168, 174, 180, 185, 190, 200, 208, 215, 222, 229, 235,
    240, 245, 255, 16, 36, 65, 89, 110, 128, 144, 159, 173, 185, 196, 207, 217, 226, 234, 242, 250,
    11, 41, 74, 103, 128, 151, 172, 191, 209, 225, 241, 255, 9, 43, 79, 110, 138, 163, 186, 207,
    227, 246, 12, 39, 71, 99, 123, 144, 164, 182, 198, 214, 228, 241, 253, 9, 44, 81, 113, 142,
    168, 192, 214, 235, 255, 7, 49, 90, 127, 160, 191, 220, 247, 6, 51, 95, 134, 170, 203, 234, 7,
    47, 87, 123, 155, 184, 212, 237, 6, 52, 97, 137, 174, 208, 240, 5, 57, 106, 151, 192, 231, 5,
    59, 111, 158, 202, 243, 5, 55, 103, 147, 187, 224, 5, 60, 113, 161, 206, 248, 4, 65, 122, 175,
    224, 4, 67, 127, 182, 234,
];

const CACHE_INDEX: &[i16] = &[
    -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, 0, 0, 41, 41, 41, 82, 82, 123, 164, 200, 222, 0, 0, 0, 0,
    0, 0, 0, 0, 41, 41, 41, 41, 123, 123, 123, 164, 164, 240, 266, 283, 295, 41, 41, 41, 41, 41,
    41, 41, 41, 123, 123, 123, 123, 240, 240, 240, 266, 266, 305, 318, 328, 336, 123, 123, 123,
    123, 123, 123, 123, 123, 240, 240, 240, 240, 305, 305, 305, 318, 318, 343, 351, 358, 364, 240,
    240, 240, 240, 240, 240, 240, 240, 305, 305, 305, 305, 343, 343, 343, 351, 351, 370, 376, 382,
    387,
];

const QN_EXP2: &[u16] = &[16384, 17866, 19483, 21247, 23170, 25267, 27554, 30048];

const BIT_DEINTERLEAVE: &[u8] = &[
    0x00, 0x03, 0x0C, 0x0F, 0x30, 0x33, 0x3C, 0x3F, 0xC0, 0xC3, 0xCC, 0xCF, 0xF0, 0xF3, 0xFC, 0xFF,
];

fn haar1(buf: &mut [f32], n0: usize, stride: usize) {
    use std::f32::consts::FRAC_1_SQRT_2;

    buf.chunks_exact_mut(2 * stride).take(n0 / 2).for_each(|l| {
        let (l0, l1) = l.split_at_mut(stride);

        l0.iter_mut().zip(l1.iter_mut()).for_each(|(e0, e1)| {
            let v0 = (*e0 + *e1) * FRAC_1_SQRT_2;
            let v1 = (*e0 - *e1) * FRAC_1_SQRT_2;
            *e0 = v0;
            *e1 = v1;
        });
    });
}

const HADAMARD_ORDERY: &[usize] = &[
    1, 0, 3, 0, 2, 1, 7, 0, 4, 3, 6, 1, 5, 2, 15, 0, 8, 7, 12, 3, 11, 4, 14, 1, 9, 6, 13, 2, 10, 5,
];

fn interleave_hadamard(
    scratch: &mut [f32],
    buf: &mut [f32],
    n0: usize,
    stride: usize,
    hadamard: bool,
) {
    let size = n0 * stride;

    if hadamard {
        let shuffle = &HADAMARD_ORDERY[stride - 2..];
        for i in 0..stride {
            for j in 0..n0 {
                scratch[j * stride + i] = buf[shuffle[i] * n0 + j];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                scratch[j * stride + i] = buf[i * n0 + j];
            }
        }
    }
    println!("interleave");
    for v in &buf[..size] {
        println!("  {:#.10}", v);
    }

    buf[..size].copy_from_slice(&scratch[..size]);
}

fn deinterleave_hadamard(
    scratch: &mut [f32],
    buf: &mut [f32],
    n0: usize,
    stride: usize,
    hadamard: bool,
) {
    let size = n0 * stride;

    println!("before deinterleave");
    for v in &buf[..size] {
        println!("  {:#.10}", v);
    }

    if hadamard {
        let shuffle = &HADAMARD_ORDERY[stride - 2..];
        for i in 0..stride {
            for j in 0..n0 {
                scratch[shuffle[i] * n0 + j] = buf[j * stride + i];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                scratch[i * n0 + j] = buf[j * stride + i];
            }
        }
    }

    println!("deinterleave");
    for v in &scratch[..size] {
        println!("  {:#.10}", v);
    }

    buf[..size].copy_from_slice(&scratch[..size]);
}

// k is clamped to be at most 128
fn cwrsi(mut n: u32, mut k: u32, mut i: u32, y: &mut [i32]) -> u32 {
    let mut norm = 0u32;

    let mut y = y.iter_mut();

    fn update(k0: u32, k: u32, s: i32, norm: &mut u32) -> i32 {
        println!("{} - {}", k0, k);
        let d = k0 - k;

        let val = (d as i32 + s) ^ s;
        *norm += (val * val) as u32;
        val as i32
    }

    while n > 2 {
        let yy = y.next().unwrap();
        println!("k {} n {} i {}", k, n, i);
        if k >= n {
            let row = pvq_u_row(n as usize);
            let p = row[k as usize + 1] as u32;
            println!("pulse {}", p);
            let s = if i >= p {
                i -= p;
                -1
            } else {
                0
            };

            let k0 = k;
            let q = row[n as usize];
            let mut p;
            if q > i {
                k = n;
                loop {
                    k -= 1;
                    p = pvq_u_row(k as usize)[n as usize];
                    println!("pulse {}", p);
                    if i >= p {
                        break;
                    }
                }
            } else {
                loop {
                    p = row[k as usize];
                    if i >= p {
                        break;
                    }
                    k -= 1;
                }
            }

            i -= p;
            println!("-- i {} p {}", i, p);
            *yy = update(k0, k, s, &mut norm);
        } else {
            let mut p = pvq_u_row(k as usize)[n as usize] as u32;
            let q = pvq_u_row(k as usize + 1)[n as usize] as u32;
            println!("i {i} p {} q {}", p, q);
            if i >= p && i < q {
                i -= p;
                println!("zeroing {i} {p}");
                *yy = 0;
            } else {
                let s = if i >= q {
                    i -= q;
                    -1
                } else {
                    0
                };

                let k0 = k;
                loop {
                    k -= 1;
                    p = pvq_u_row(k as usize)[n as usize];
                    if i >= p {
                        break;
                    }
                }

                i -= p;
                println!("i {} p {}", i, p);
                *yy = update(k0, k, s, &mut norm);
            }
        }
        n -= 1;
    }
    {
        // n == 2
        let p = 2 * k + 1;
        println!("p {} i {}", p, i);
        let s = if i >= p {
            i -= p;
            -1
        } else {
            0
        };

        let k0 = k;
        k = (i + 1) / 2;
        if k != 0 {
            i -= 2 * k - 1;
        }

        let yy = y.next().unwrap();
        println!("n == 2");
        *yy = update(k0, k, s, &mut norm);
    }

    {
        // n == 1
        let s = -(i as i32);

        let yy = y.next().unwrap();
        println!("n == 1");
        *yy = update(k, 0, s, &mut norm);
    }

    println!("norm {}", norm);
    norm
}

fn decode_pulses(rd: &mut RangeDecoder, y: &mut [i32], n: usize, k: usize) -> f32 {
    fn pvq_u(n: usize, k: usize) -> usize {
        pvq_u_row(n.min(k))[n.max(k)] as usize
    }
    fn pvq_v(n: usize, k: usize) -> usize {
        pvq_u(n, k) + pvq_u(n, k + 1)
    }

    let idx = rd.decode_uniform(pvq_v(n, k));
    println!("idx {}", idx);
    cwrsi(n as u32, k as u32, idx as u32, y) as f32
}

// TODO use windows_mut once it exists
fn exp_rotation1(x: &mut [f32], len: usize, stride: usize, c: f32, s: f32) {
    let end = len - stride;
    for i in 0..end {
        let x1 = x[i];
        let x2 = x[i + stride];

        x[i + stride] = c * x2 + s * x1;
        x[i] = c * x1 - s * x2;
    }

    for i in (0..end - stride - 1).rev() {
        let x1 = x[i];
        let x2 = x[i + stride];
        x[i + stride] = c * x2 + s * x1;
        x[0] = c * x1 - s * x2;
    }
}

fn exp_rotation(x: &mut [f32], len: usize, stride: usize, k: usize, spread: usize) {
    if 2 * k >= len || spread == SPREAD_NONE {
        return;
    }

    let gain = len as f32 / ((len + (20 - 5 * spread) * k) as f32);
    let theta = std::f32::consts::PI * gain * gain / 4.0;

    let c = theta.cos();
    let s = theta.sin();

    let mut stride2 = 0;
    if len >= stride << 3 {
        stride2 = 1;
        // equivalent to rounded sqrt(len / stride)
        while (stride2 * stride2 + stride2) * stride + (stride >> 2) < len {
            stride2 += 1;
        }
    }

    let _l = len / stride;
    for i in 0..stride {
        if stride2 != 0 {
            exp_rotation1(&mut x[i * len..], len, stride2, s, c);
        }
        exp_rotation1(&mut x[i * len..], len, 1, c, s);
    }
}

fn extract_collapse_mask(y: &[i32], b: usize) -> usize {
    if b <= 1 {
        return 1;
    }

    let mut collapse_mask = 0;
    for block in y.chunks_exact(b) {
        block.iter().enumerate().for_each(|(i, &v)| {
            collapse_mask |= ((v != 0) as usize) << i;
        });
    }

    return collapse_mask;
}

fn bits2pulses(cache: &[u8], bits: i32) -> i32 {
    let mut low = 0;
    let mut high = cache[0] as usize;

    for _i in 0..6 {
        let center = (low + high + 1) >> 1;
        if cache[center] as i32 >= bits {
            high = center;
        } else {
            low = center;
        }
    }

    let low_bits = bits - if low == 0 { -1 } else { cache[low] as i32 };
    let high_bits = cache[high] as i32 - bits;

    let pulses = if low_bits <= high_bits { low } else { high };
    pulses as i32
}

fn pulses2bits(cache: &[u8], pulses: i32) -> i32 {
    if pulses == 0 {
        0
    } else {
        cache[pulses as usize] as i32
    }
}

fn unquantize(
    rd: &mut RangeDecoder,
    x: &mut [f32],
    n: usize,
    k: usize,
    spread: usize,
    blocks: usize,
    gain: f32,
) -> usize {
    let mut y: [i32; 176] = unsafe { mem::uninitialized() };

    let gain = gain / decode_pulses(rd, &mut y, n, k).sqrt();

    x[..n].iter_mut().zip(y[..n].iter()).for_each(|(o, &i)| {
        *o = gain * i as f32;
    });

    exp_rotation(x, n, blocks, k, spread);

    return extract_collapse_mask(&y[..n], blocks);
}

fn renormalize_vector(x: &mut [f32], gain: f32) {
    let g: f32 = x.iter().map(|&v| v * v).sum();

    let gain = gain / g.sqrt();

    x.iter_mut().for_each(|v| *v *= gain);
}

fn stereo_merge(x: &mut [f32], y: &mut [f32], mid: f32, n: usize) {
    let (xp, side) = x[..n]
        .iter()
        .zip(y[..n].iter())
        .fold((0f32, 0f32), |(xp, side), (&xv, &yv)| {
            (xp + xv * yv, side + yv * yv)
        });

    println!("xp {} side {}", xp, side);

    let xp = xp * mid;

    let e = mid * mid + side;

    let e0 = e - 2f32 * xp;
    let e1 = e + 2f32 * xp;

    if e0 < 6e-4f32 || e1 < 6e-4f32 {
        &mut y[..n].copy_from_slice(&x[..n]);
    }

    let gain0 = 1f32 / e0.sqrt();
    let gain1 = 1f32 / e1.sqrt();

    for (xv, yv) in x[..n].iter_mut().zip(y[..n].iter_mut()) {
        let v0 = mid * *xv;
        let v1 = *yv;

        *xv = gain0 * (v0 - v1);
        *yv = gain1 * (v0 + v1);
    }
}

struct BandInfo {
    itheta: usize,
    inv: bool,
    mid: f32,
    side: f32,
    delta: i16,
    qalloc: usize,
    fill: usize,
}

impl Celt {
    pub fn new(stereo: bool) -> Self {
        let frames = Default::default();

        // Create IMDCT instances for each lm value (0-3)
        // lm=0: 120 samples (2.5ms), lm=1: 240 samples (5ms)
        // lm=2: 480 samples (10ms), lm=3: 960 samples (20ms)
        let imdct = [
            IMDCT15::new(3), // lm=0: 15 * 2^3 = 120
            IMDCT15::new(4), // lm=1: 15 * 2^4 = 240
            IMDCT15::new(5), // lm=2: 15 * 2^5 = 480
            IMDCT15::new(6), // lm=3: 15 * 2^6 = 960
        ];

        Celt {
            stereo,
            stereo_pkt: false,
            bits: 0,
            lm: 0,
            frames,
            band: 0..MAX_BANDS,
            spread: SPREAD_NORMAL,
            fine_bits: Default::default(),
            fine_priority: Default::default(),
            pulses: Default::default(),
            tf_change: Default::default(),
            anticollapse_bit: 0,
            blocks: 0,
            blocksize: 0,
            intensity_stereo: 0,
            dual_stereo: false,
            codedband: 0,
            remaining: 0,
            remaining2: 0,
            scratch: unsafe { mem::zeroed() },
            seed: 0,
            imdct,
            overlap: [[0.0; 960]; 2],
        }
    }

    pub fn setup(&mut self, _pkt: &Packet) {
        // Stereo configuration is already set during initialization
        // In Symphonia's architecture, this comes from codec parameters
    }

    fn reset_gains(&mut self) {
        self.frames[0].pf.gains_new = [0.0; 3];
        self.frames[1].pf.gains_new = [0.0; 3];
    }

    fn parse_postfilter(&mut self, rd: &mut RangeDecoder) {
        if rd.decode_logp(1) {
            let octave = rd.decode_uniform(6);
            let period = (16 << octave) + rd.rawbits(4 + octave) - 1;
            let gain_bits = rd.rawbits(3) + 1;
            let gain = gain_bits as f32 * 0.09375;

            let tapset = if rd.available() >= 2 {
                rd.decode_icdf(TAPSET)
            } else {
                0
            };

            println!(
                "postfilter: octave {}, period {}, gain {}, tapset {}",
                octave, period, gain, tapset
            );
            let taps = POSTFILTER_TAPS[tapset];
            for frame in self.frames.iter_mut() {
                frame.pf.period_new = period.max(MIN_PERIOD);
                frame.pf.gains_new = [taps[0] * gain, taps[1] * gain, taps[2] * gain];
            }
        } else {
            println!("postfilter: no");
        }
    }

    fn decode_coarse_energy(&mut self, rd: &mut RangeDecoder, band: Range<usize>) {
        let (alpha, beta, model) = if rd.available() > 3 && rd.decode_logp(3) {
            (
                0f32,
                1f32 - 4915f32 / 32768f32,
                COARSE_ENERGY_INTRA[self.lm],
            )
        } else {
            (
                ALPHA_COEF[self.lm],
                BETA_COEF[self.lm],
                COARSE_ENERGY_INTER[self.lm],
            )
        };

        println!("model {:.6} {:.6}", alpha, beta);

        let mut prev = [0f32; 2];
        let frames = &mut self.frames;
        for i in 0..MAX_BANDS {
            let mut coarse_energy_band = |j| {
                let f: &mut CeltFrame = &mut frames[j];
                let en = &mut f.energy[i];
                if i < band.start || i >= band.end {
                    *en = 0.0
                } else {
                    let available = rd.available();
                    println!("available {}", available);
                    let value = if available >= 15 {
                        let k = i.min(20) << 1;
                        let v = rd
                            .decode_laplace((model[k] as usize) << 7, (model[k + 1] as isize) << 6);
                        println!("decode_laplace {:.6} <- {} {}", v, i, k);
                        v
                    } else if available >= 1 {
                        let v = rd.decode_icdf(MODEL_ENERGY_SMALL) as isize;
                        (v >> 1) ^ -(v & 1)
                    } else {
                        -1
                    } as f32;

                    println!(
                        "energy {}/{} {:.6} * {:.6} + {:.6} + {:.6}",
                        i, j, *en, alpha, prev[j], value
                    );
                    *en = en.max(-9f32) * alpha + prev[j] + value;
                    prev[j] += beta * value;
                }
            };

            coarse_energy_band(0);
            if self.stereo_pkt {
                coarse_energy_band(1);
            }
        }
        println!("{:#.6?}", &frames[0].energy[..]);
        println!("{:#.6?}", &frames[1].energy[..]);
    }

    fn decode_tf_changes(&mut self, rd: &mut RangeDecoder, band: Range<usize>, transient: bool) {
        let mut tf_changed = [false; MAX_BANDS];
        let bits = if transient { (2, 4) } else { (4, 5) };
        let mut available = rd.available();

        let tf_select = TF_SELECT[self.lm][transient as usize];

        let select_bit = self.lm != 0 && available > bits.0;
        println!("select_bit {} {}", select_bit, available);

        let mut field_bits = bits.0;
        let mut diff = false;
        let mut changed = false;
        for (i, tf_change) in tf_changed[band.clone()].iter_mut().enumerate() {
            if available > field_bits + select_bit as usize {
                diff ^= rd.decode_logp(field_bits);
                println!("band {} bits {} {}", i, field_bits, diff);
                available = rd.available();
                changed |= diff;
            }

            *tf_change = diff;
            field_bits = bits.1;
        }

        let select =
            if select_bit && tf_select[0][changed as usize] != tf_select[1][changed as usize] {
                rd.decode_logp(1)
            } else {
                false
            };
        {
            let tf_change = self.tf_change[band.clone()].iter_mut();

            for (tf, &changed) in tf_change.zip(tf_changed[band.clone()].iter()) {
                *tf = tf_select[select as usize][changed as usize];
            }
        }
        println!("tf_change {:#?}", &self.tf_change[band]);
    }

    fn decode_allocation(&mut self, rd: &mut RangeDecoder, band: Range<usize>) {
        let mut caps: [i32; MAX_BANDS] = unsafe { mem::uninitialized() };
        let mut threshold = [0; MAX_BANDS];
        let mut trim_offset = [0; MAX_BANDS];
        let mut boost = [0; MAX_BANDS];
        let scale = self.lm + self.stereo_pkt as usize;
        let mut skip_startband = band.start;

        let _spread = if rd.available() > 4 {
            rd.decode_icdf(MODEL_SPREAD)
        } else {
            SPREAD_NORMAL
        };

        let static_caps = &STATIC_CAPS[self.lm][self.stereo_pkt as usize];

        caps.iter_mut()
            .zip(static_caps.iter().zip(FREQ_RANGE.iter()))
            .for_each(|(cap, (&static_cap, &freq_range))| {
                *cap = (static_cap as i32 + 64) * (freq_range as i32) << scale >> 2;
            });

        println!("caps {:#?}", &caps[..]);

        let mut dynalloc = 6;
        let mut boost_size = 0;

        println!("consumed {}", rd.tell_frac());

        for i in band.clone() {
            let quanta = FREQ_RANGE[i] << scale;
            let quanta = (quanta << 3).min(quanta.max(6 << 3)) as i32;
            let mut band_dynalloc = dynalloc;
            while (band_dynalloc << 3) + boost_size < rd.available_frac() && boost[i] < caps[i] {
                let add = rd.decode_logp(band_dynalloc);
                if !add {
                    break;
                }
                boost[i] += quanta;
                boost_size += quanta as usize;
                band_dynalloc = 1;
            }

            if boost[i] != 0 && dynalloc > 2 {
                dynalloc -= 1;
            }
        }

        let alloc_trim = if rd.available_frac() > boost_size + (6 << 3) {
            rd.decode_icdf(ALLOC_TRIM)
        } else {
            5
        } as i32;

        println!("alloc_trim {}", alloc_trim);

        let mut available = rd.available_frac() - 1;
        self.anticollapse_bit =
            if self.blocks > 1 && self.lm >= 2 && available >= (self.lm + 2) << 3 {
                available -= 1 << 3;
                1 << 3
            } else {
                0
            };

        println!("anticollapse_bit {}", self.anticollapse_bit);

        let skip_bit = if available >= 1 << 3 {
            available -= 1 << 3;
            1 << 3
        } else {
            0
        };

        println!("skip_bit {}", skip_bit);

        let (mut intensity_stereo_bit, dual_stereo_bit) = if self.stereo_pkt {
            let intensity_stereo = LOG2_FRAC[band.end - band.start] as usize;
            if intensity_stereo <= available {
                available -= intensity_stereo;
                let dual_stereo = if available >= 1 << 3 {
                    available -= 1 << 3;
                    1 << 3
                } else {
                    0
                };
                (intensity_stereo, dual_stereo)
            } else {
                (0, 0)
            }
        } else {
            (0, 0)
        };

        println!("intensity_stereo_bit {}", intensity_stereo_bit);

        for i in band.clone() {
            let trim = alloc_trim - (5 + self.lm) as i32;
            let range = FREQ_RANGE[i] as i32 * (band.end - i - 1) as i32;
            let lm = self.lm + 3;
            let scale = lm as i32 + self.stereo_pkt as i32;
            let stereo_threshold = (self.stereo_pkt as i32) << 8;

            threshold[i] = ((3 * FREQ_RANGE[i] as i32) << lm >> 4).max(stereo_threshold);

            trim_offset[i] = trim * (range << scale) >> 6;

            if FREQ_RANGE[i] << self.lm == 1 {
                trim_offset[i] -= stereo_threshold;
            }

            println!("trim_offset {} {}", i, trim_offset[i]);
        }

        const CELT_VECTOR: usize = 11;
        let coded_channel_bits = (self.stereo_pkt as i32 + 1) << 3;

        let mut low = 1;
        let mut high = CELT_VECTOR - 1;
        while low <= high {
            let center = (low + high) / 2;
            let mut done = false;
            let mut total = 0;

            for i in band.clone().rev() {
                let bandbits = (FREQ_RANGE[i] as i32 * STATIC_ALLOC[center][i] as i32)
                    << (self.stereo_pkt as i32)
                    << self.lm
                    >> 2;

                println!("bandbits {}", bandbits);

                let bandbits = if bandbits != 0 {
                    (bandbits + trim_offset[i]).max(0)
                } else {
                    bandbits
                } + boost[i];

                if bandbits >= threshold[i] || done {
                    done = true;
                    total += bandbits.min(caps[i]);
                } else {
                    if bandbits >= coded_channel_bits {
                        total += coded_channel_bits;
                    }
                }

                println!("total {} {}", total, available);
            }

            if total as usize > available {
                high = center - 1;
            } else {
                low = center + 1;
            }
            println!("{} {} {}", high, low, center);
        }

        println!("high {} low {}", high, low);

        high = low;
        low -= 1;

        // Clamp indices to valid range for STATIC_ALLOC (0..11)
        high = high.min(10);
        low = low.min(10);

        let mut bits1 = [0; MAX_BANDS];
        let mut bits2 = [0; MAX_BANDS];

        println!("high {} low {}", high, low);

        for i in band.clone() {
            let bits_estimation = |idx: usize| -> i32 {
                let bits = (FREQ_RANGE[i] as i32 * STATIC_ALLOC[idx][i] as i32)
                    << (self.stereo_pkt as i32)
                    << self.lm
                    >> 2;
                if bits != 0 {
                    (bits + trim_offset[i]).max(0)
                } else {
                    bits
                }
            };
            bits1[i] = bits_estimation(low);
            bits2[i] = bits_estimation(high);

            if boost[i] != 0 {
                if low != 0 {
                    bits1[i] += boost[i];
                }

                bits2[i] += boost[i];

                skip_startband = i;
            }

            bits2[i] = (bits2[i] - bits1[i]).max(0);
            println!("bits2 {}", bits2[i]);
        }

        const ALLOC_STEPS: usize = 6;

        low = 0;
        high = 1 << ALLOC_STEPS;

        for _i in 0..ALLOC_STEPS {
            let center = (low + high) / 2;
            let mut done = false;
            let mut total = 0;

            for j in band.clone().rev() {
                let bits = bits1[j] + (center as i32 * bits2[j] >> ALLOC_STEPS);

                if bits >= threshold[j] || done {
                    done = true;
                    total += bits.min(caps[j]);
                } else if bits >= coded_channel_bits {
                    total += coded_channel_bits;
                }
            }

            if total as usize > available {
                high = center;
            } else {
                low = center;
            }
        }

        let mut done = false;
        let mut total = 0;

        for i in band.clone().rev() {
            let mut bits = bits1[i] + (low as i32 * bits2[i] >> ALLOC_STEPS);

            if bits >= threshold[i] || done {
                done = true;
            } else {
                bits = if bits >= coded_channel_bits {
                    coded_channel_bits
                } else {
                    0
                }
            }

            let bits = bits.min(caps[i]);
            self.pulses[i] = bits;
            total += bits;

            println!("total {}", total);
        }

        let mut bands = band.clone().rev();

        let codedband = loop {
            let j = bands.next().unwrap();
            let codedband = j + 1;

            println!("codedband {} {}", codedband, j);
            if j == skip_startband {
                available += skip_bit;
                break codedband;
            }

            let band_delta = (FREQ_BANDS[codedband] - FREQ_BANDS[band.start]) as i32;
            let (bits, remaining) = {
                let remaining = available as i32 - total;
                let bits = remaining / band_delta;
                (bits, remaining - bits * band_delta)
            };
            let mut allocation =
                self.pulses[j] + bits * FREQ_BANDS[j] as i32 + 0.max(remaining - band_delta);

            if allocation >= threshold[j].max(coded_channel_bits) {
                if rd.decode_logp(1) {
                    break codedband;
                }

                total += 1 << 3;
                allocation -= 1 << 3;
            }

            total -= self.pulses[j];
            if intensity_stereo_bit != 0 {
                total -= intensity_stereo_bit as i32;
                intensity_stereo_bit = LOG2_FRAC[j - band.start] as usize;
                total += intensity_stereo_bit as i32;
            }

            self.pulses[j] = if allocation >= coded_channel_bits {
                coded_channel_bits
            } else {
                0
            };

            total += self.pulses[j];

            println!("band skip total {}", total);
        };

        self.intensity_stereo = if intensity_stereo_bit != 0 {
            band.start + rd.decode_uniform(codedband + 1 - band.start)
        } else {
            0
        };

        self.dual_stereo = if self.intensity_stereo <= band.start {
            available += dual_stereo_bit;
            false
        } else if dual_stereo_bit != 0 {
            rd.decode_logp(1)
        } else {
            false
        };

        println!(
            "intensity {}, dual {}",
            self.intensity_stereo, self.dual_stereo as usize
        );

        let band_delta = (FREQ_BANDS[codedband] - FREQ_BANDS[band.start]) as i32;
        let (bandbits, mut remaining) = {
            let remaining = available as i32 - total;
            let bits = remaining / band_delta;
            (bits, remaining - bits * band_delta)
        };

        for i in band.clone() {
            let freq_range = FREQ_RANGE[i] as i32;
            let bits = remaining.min(freq_range);

            self.pulses[i] += bits + bandbits * freq_range;
            remaining -= bits;
        }

        println!("remaining {}", remaining);

        let mut extrabits = 0;

        const FINE_OFFSET: i32 = 21;

        for i in band.clone() {
            let n = (FREQ_RANGE[i] as i32) << self.lm;
            let prev_extra = extrabits;
            self.pulses[i] += extrabits;

            if n > 1 {
                extrabits = 0.max(self.pulses[i] - caps[i]);
                self.pulses[i] -= extrabits;

                let dof = n * (self.stereo_pkt as i32 + 1)
                    + (self.stereo_pkt && n > 2 && !self.dual_stereo && i < self.intensity_stereo)
                        as i32;
                let duration = (self.lm << 3) as i32;
                let dof_channels = dof * (LOG_FREQ_RANGE[i] as i32 + duration);
                let mut offset = (dof_channels >> 1) - dof * FINE_OFFSET;

                println!("dof {} {} {}", dof, dof_channels, offset);

                if n == 2 {
                    offset += dof << 1;
                }

                let pulse = self.pulses[i] + offset;
                if pulse < 2 * (dof << 3) {
                    offset += dof_channels >> 2;
                } else if pulse < 3 * (dof << 3) {
                    offset += dof_channels >> 3;
                }

                let pulse = self.pulses[i] + offset;

                let fine_bits = (pulse + (dof << 2)) / (dof << 3);
                println!("pulses {}, offset {}", self.pulses[i], offset);
                let max_bits = (self.pulses[i] >> 3) >> (self.stereo_pkt as usize);
                let max_bits = max_bits.min(MAX_FINE_BITS).max(0);

                self.fine_bits[i] = fine_bits.max(0).min(max_bits);
                println!("fine_bits {} {}", fine_bits, self.fine_bits[i]);
                self.fine_priority[i] = self.fine_bits[i] * (dof << 3) >= pulse;

                self.pulses[i] -= self.fine_bits[i] << (self.stereo_pkt as usize) << 3;
            } else {
                extrabits = (self.pulses[i] - ((self.stereo_pkt as i32 + 1) << 3)).max(0);
                self.pulses[i] -= extrabits;
                self.fine_bits[i] = 0;
                self.fine_priority[i] = true;
            }

            if extrabits > 0 {
                let scale = self.stereo_pkt as usize + 1 + 2;
                let extra_fine = (MAX_FINE_BITS - self.fine_bits[i]).min(extrabits >> scale);

                self.fine_bits[i] += extra_fine;

                let extra_fine = extra_fine << scale;
                self.fine_priority[i] = extra_fine >= extrabits - prev_extra;

                extrabits -= extra_fine;
            }

            println!("extrabits {}", extrabits);
            println!("fine_bits {}", self.fine_bits[i]);
        }

        self.remaining = extrabits;

        for i in codedband..band.end {
            self.fine_bits[i] = self.pulses[i] >> (self.stereo_pkt as usize) >> 3;
            self.pulses[i] = 0;
            self.fine_priority[i] = self.fine_bits[i] < 1;

            println!("fine_bits end {}", self.fine_bits[i]);
        }

        self.codedband = codedband;
    }

    fn decode_fine_energy(&mut self, rd: &mut RangeDecoder, band: Range<usize>) {
        for i in band {
            if self.fine_bits[i] == 0 {
                continue;
            }

            for f in 0..self.stereo_pkt as usize + 1 {
                let frame = &mut self.frames[f];
                let q2 = rd.rawbits(self.fine_bits[i] as usize) as f32;
                println!("-- fine_bits {}", self.fine_bits[i]);
                let offset = (q2 + 0.5) * (1 << (14 - self.fine_bits[i])) as f32 / 16384.0 - 0.5;
                println!("q2 {}", q2);
                frame.energy[i] += offset;
            }
        }
    }

    fn decode_band_1(
        &mut self,
        rd: &mut RangeDecoder,
        mid_buf: &mut [f32],
        side_buf: Option<&mut [f32]>,
        lowband_out: Option<&mut [f32]>,
    ) {
        let mut one_sample = move || {
            let sign = if self.remaining2 >= 1 << 3 {
                self.remaining2 -= 1 << 3;
                rd.rawbits(1)
            } else {
                0
            };

            if sign != 0 {
                -1f32
            } else {
                1f32
            }
        };

        mid_buf[0] = one_sample();
        if let Some(side) = side_buf {
            side[0] = one_sample();
        }
        if let Some(out) = lowband_out {
            out[0] = mid_buf[0];
        }
    }

    fn compute_theta(
        &self,
        rd: &mut RangeDecoder,
        band: usize,
        lm: usize,
        n: usize,
        b: usize,
        b0: usize,
        blocks: usize,
        dualstereo: bool,
        fill: usize,
    ) -> BandInfo {
        const QTHETA_OFFSET: usize = 4;
        const QTHETA_OFFSET_TWOPHASE: usize = 16;
        const BITRES: i32 = 2 << 3;
        println!("band {}", band);
        let pulse_cap = LOG_FREQ_RANGE[band] as usize + lm * 8;
        let offset = (pulse_cap >> 1)
            - if dualstereo && n == 2 {
                QTHETA_OFFSET_TWOPHASE
            } else {
                QTHETA_OFFSET
            };

        let qn = if dualstereo && band >= self.intensity_stereo {
            1
        } else {
            let n2 = if dualstereo && n == 2 {
                2 * n - 2
            } else {
                2 * n - 1
            };
            println!("n2 {} pulse_cap {} b {}", n2, pulse_cap, b);
            let qb = (b - pulse_cap - (4 << 3))
                .min((b + n2 * offset) / n2)
                .min(8 << 3);

            if qb < (1 << 3 >> 1) {
                1
            } else {
                ((QN_EXP2[qb & 0x7] as usize >> (14 - (qb >> 3))) + 1) >> 1 << 1
            }
        };

        println!("qn {}", qn);

        let tell_frac = rd.tell_frac();
        let (itheta, inv) = if qn != 1 {
            let itheta = if dualstereo && n > 2 {
                rd.decode_step(qn / 2)
            } else if dualstereo || b0 > 1 {
                rd.decode_uniform(qn + 1)
            } else {
                rd.decode_triangular(qn)
            };
            (itheta * 16384 / qn, false)
        } else {
            let inv = if dualstereo && b as i32 > BITRES && self.remaining2 > BITRES {
                rd.decode_logp(2)
            } else {
                false
            };
            (0, inv)
        };

        let qalloc = rd.tell_frac() - tell_frac;

        let (imid, iside, fill, delta) = if itheta == 0 {
            let imid = 32767;
            let iside = 0;
            let fill = fill & ((1 << blocks) - 1);
            let delta = -16384;
            (imid, iside, fill, delta)
        } else if itheta == 16384 {
            let imid = 0;
            let iside = 32767;
            let fill = fill & (((1 << blocks) - 1) << blocks);
            let delta = 16384;
            (imid, iside, fill, delta)
        } else {
            let imid = bitexact::cos(itheta as i16) as i32;
            let iside = bitexact::cos(16384 - itheta as i16) as i32;
            let delta =
                bitexact::frac_mul16(((n - 1) << 7) as i16, bitexact::log2tan(iside, imid) as i16);
            (imid, iside, fill, delta)
        };

        BandInfo {
            itheta,
            inv,
            fill,
            delta,
            qalloc,
            mid: imid as f32 / 32768.0,
            side: iside as f32 / 32768.0,
        }
    }

    fn rng(&mut self) -> u32 {
        self.seed = 1664525 * self.seed + 1013904223;

        self.seed
    }

    fn decode_band_no_split(
        &mut self,
        rd: &mut RangeDecoder,
        mid_buf: &mut [f32],
        lowband: Option<&[f32]>,
        n: usize,
        blocks: usize,
        gain: f32,
        cache: &[u8],
        b: i32,
        fill: usize,
    ) -> usize {
        let mut q = bits2pulses(cache, b);
        let mut curr_bits = pulses2bits(cache, q);

        self.remaining2 -= curr_bits;

        // TODO: simplify
        while self.remaining2 < 0 && q > 0 {
            self.remaining2 += curr_bits;
            q -= 1;
            curr_bits = pulses2bits(cache, q);
            self.remaining2 -= curr_bits;
        }

        let cm = if q != 0 {
            let k = if q < 8 {
                q
            } else {
                (8 + (q & 7)) << ((q >> 3) - 1)
            };
            unquantize(rd, mid_buf, n, k as usize, self.spread, blocks, gain)
        } else {
            let cm_mask = (1usize << blocks) - 1;
            let fill = fill & cm_mask;
            if fill == 0 {
                for v in mid_buf[..n].iter_mut() {
                    *v = 0f32;
                }
                0
            } else {
                let cm = if let Some(lowband) = lowband {
                    for (v, &l) in mid_buf[..n].iter_mut().zip(lowband[..n].iter()) {
                        let rng = if (self.rng() & 0x8000) != 0 {
                            1f32 / 256f32
                        } else {
                            -1f32 / 256f32
                        };
                        *v = l + rng;
                    }
                    fill
                } else {
                    for v in mid_buf[..n].iter_mut() {
                        *v = (self.rng() as i32 >> 20) as f32;
                    }
                    cm_mask
                };
                renormalize_vector(&mut mid_buf[..n], gain);
                cm
            }
        };
        cm
    }

    fn decode_band<'a>(
        &mut self,
        rd: &mut RangeDecoder,
        band: usize,
        mut mid_buf: &'a mut [f32],
        mut side_buf: Option<&'a mut [f32]>,
        mut n: usize,
        mut b: i32,
        mut blocks: usize,
        mut lowband: Option<&'a [f32]>,
        mut lowband_out: Option<&'a mut [f32]>,
        mut lm: isize,
        level: usize,
        gain: f32,
        mut fill: usize,
    ) -> usize {
        let mut n_b = n / blocks;
        let mut n_b0 = n_b;
        let dualstereo = side_buf.is_some();
        let n0 = n;
        let mut b0 = blocks;

        let mut time_divide = 0;
        let longblocks = b0 == 1;
        println!(
            "decode_band N={} lowband_out {}",
            n,
            lowband_out.is_some() as usize
        );

        println!("mid_buf");
        for v in mid_buf[..n].iter() {
            println!("{:.08}", v);
        }

        if n == 1 {
            self.decode_band_1(rd, mid_buf, side_buf, lowband_out);

            return 1;
        }

        let mut lowband = lowband.map(|l| std::borrow::Cow::from(&l[..n]));

        let recombine = if !dualstereo && level == 0 {
            let mut tf_change = self.tf_change[band];
            let recombine = if tf_change > 0 { tf_change } else { 0 };

            println!("recombine {}", recombine);

            if let Some(ref lowband) = lowband {
                println!("lowband");
                for v in lowband.iter() {
                    println!("{:.08}", v);
                }
            }

            if b0 > 1 || (recombine != 0 || ((n_b & 1) == 0 && tf_change < 0)) {}

            for k in 0..recombine {
                if let Some(ref mut lowband_in) = lowband {
                    haar1(lowband_in.to_mut(), n >> k, 1 << k);
                };

                fill =
                    BIT_INTERLEAVE[fill & 0xf] as usize | (BIT_INTERLEAVE[fill >> 4] as usize) << 2;
            }

            blocks >>= recombine;
            n_b <<= recombine;
            println!("blocks {} N_B {}", blocks, n_b);
            while (n_b & 1) == 0 && tf_change < 0 {
                if let Some(ref mut lowband_in) = lowband {
                    println!("EDIT");
                    haar1(lowband_in.to_mut(), n_b, blocks);
                };

                fill |= fill << blocks;
                blocks <<= 1;
                n_b >>= 1;

                time_divide += 1;
                tf_change += 1;
            }

            b0 = blocks;
            n_b0 = n_b;

            println!("B0 {}", b0);
            if b0 > 1 {
                if let Some(ref mut lowband_in) = lowband {
                    deinterleave_hadamard(
                        &mut self.scratch,
                        lowband_in.to_mut(),
                        n_b >> recombine,
                        b0 << recombine,
                        longblocks,
                    );
                }
            }

            recombine
        } else {
            0
        };

        let mut lowband = lowband.as_deref();

        let cache = &CACHE_BITS[CACHE_INDEX[(lm + 1) as usize * MAX_BANDS + band] as usize..];

        let split = if !dualstereo && lm >= 0 && b > (cache[cache[0] as usize] as i32) + 12 && n > 2
        {
            n >>= 1;
            let (split_mid, split_side) = mid_buf.split_at_mut(n);

            mid_buf = split_mid;
            side_buf = Some(split_side);

            lm -= 1;
            if blocks == 1 {
                fill = (fill & 1) | (fill << 1);
            }
            blocks = (blocks + 1) >> 1;
            true
        } else {
            dualstereo
        };

        println!("split {} blocks {} lm {}", split as u8, blocks, lm);

        // TODO: move this code out
        let mut cm = if let Some(side_buf) = side_buf {
            let info = self.compute_theta(
                rd,
                band,
                lm as usize,
                n,
                b as usize,
                b0,
                blocks,
                dualstereo,
                fill,
            );
            let BandInfo {
                itheta,
                inv: _,
                mid,
                side,
                mut delta,
                qalloc,
                fill: _,
            } = info;

            b -= qalloc as i32;

            println!(
                "itheta {} delta {} n {} dualstereo {}",
                itheta, delta, n, dualstereo as usize
            );
            /* This is a special case for N=2 that only works for stereo and takes
            advantage of the fact that mid and side are orthogonal to encode
            the side with just one bit. */
            let cm = if n == 2 && dualstereo {
                let sbits = if itheta != 0 || itheta != 16384 {
                    1 << 3
                } else {
                    0
                };
                let mbits = b - sbits;
                self.remaining2 -= qalloc as i32 + sbits;

                let (mid_buf2, side_buf2) = if itheta > 8192 {
                    (&mut side_buf[..], &mut mid_buf[..])
                } else {
                    (&mut mid_buf[..], &mut side_buf[..])
                };

                let sign = if sbits != 0 { 1 - 2 * rd.rawbits(1) } else { 1 } as f32;

                let cm = self.decode_band(
                    rd,
                    band,
                    mid_buf2,
                    None,
                    n,
                    mbits,
                    blocks,
                    lowband.as_ref().map(|c| c.as_ref()),
                    lowband_out.as_deref_mut(),
                    lm,
                    level,
                    gain,
                    fill,
                );
                side_buf2[0] = -sign * mid_buf2[1];
                // TODO complete the mapping
                //
                cm
            } else {
                let mut next_lowband2 = None;
                let mut next_lowband_out1 = None;
                // normal split

                if b0 > 1 && !dualstereo && (itheta & 0x3fff) != 0 {
                    if itheta > 8192 {
                        delta -= delta >> (4 - lm);
                    } else {
                        delta = (delta + (n << 3 >> (5 - lm)) as i16).min(0)
                    }
                }

                println!("delta {delta}");
                let mut mbits = ((b - delta as i32) / 2).clamp(0, b);
                let mut sbits = b - mbits;

                self.remaining2 -= qalloc as i32;

                if !dualstereo {
                    if let Some(bands) = lowband.take() {
                        let bands = bands.split_at(n);
                        lowband = Some(bands.0);
                        next_lowband2 = Some(bands.1);
                    }
                }

                let mut next_level = 0;

                if dualstereo {
                    next_lowband_out1 = lowband_out.as_deref_mut();
                } else {
                    next_level += 1;
                }

                let rebalance = self.remaining2;
                let side_shift = if dualstereo { b0 >> 1 } else { 0 };
                if mbits >= sbits {
                    let mut cm = self.decode_band(
                        rd,
                        band,
                        mid_buf,
                        None,
                        n,
                        mbits,
                        blocks,
                        lowband,
                        next_lowband_out1,
                        lm,
                        next_level,
                        if dualstereo { 1.0 } else { gain * mid },
                        fill,
                    );
                    let rebalance = mbits - (rebalance - self.remaining2);
                    if rebalance > 3 << 3 && itheta != 0 {
                        sbits += rebalance - (3 << 3);
                    }

                    cm |= self.decode_band(
                        rd,
                        band,
                        side_buf,
                        None,
                        n,
                        sbits,
                        blocks,
                        next_lowband2,
                        None,
                        lm,
                        next_level,
                        gain * side,
                        fill >> blocks,
                    ) << side_shift;
                    cm
                } else {
                    let mut cm = self.decode_band(
                        rd,
                        band,
                        side_buf,
                        None,
                        n,
                        sbits,
                        blocks,
                        next_lowband2,
                        None,
                        lm,
                        next_level,
                        gain * side,
                        fill >> blocks,
                    ) << side_shift;

                    let rebalance = sbits - (rebalance - self.remaining2);
                    if rebalance > 3 << 3 && itheta != 16384 {
                        mbits += rebalance - (3 << 3);
                    }

                    cm |= self.decode_band(
                        rd,
                        band,
                        mid_buf,
                        None,
                        n,
                        mbits,
                        blocks,
                        lowband,
                        next_lowband_out1,
                        lm,
                        next_level,
                        gain * mid,
                        fill,
                    );

                    cm
                }
            };

            // After stereo split: mid and side have been decoded into separate buffers
            // The stereo merging will happen at a higher level
            cm
        } else {
            self.decode_band_no_split(
                rd,
                mid_buf,
                lowband.as_ref().map(|c| c.as_ref()),
                n,
                blocks,
                gain,
                cache,
                b,
                fill,
            )
        };

        if level == 0 {
            if b0 > 1 {
                interleave_hadamard(
                    &mut self.scratch,
                    mid_buf,
                    n_b >> recombine,
                    b0 << recombine,
                    longblocks,
                );
            }

            n_b = n_b0;
            blocks = b0;
            for _ in 0..time_divide {
                blocks >>= 1;
                n_b <<= 1;
                cm |= cm >> blocks;
                haar1(mid_buf, n_b, blocks);
            }

            for k in 0..recombine {
                cm = BIT_DEINTERLEAVE[cm as usize] as usize;
                haar1(mid_buf, n0 >> k, 1 << k);
            }

            blocks <<= recombine;

            if let Some(lowband_out) = lowband_out {
                let n = (n0 as f32).sqrt();
                println!("Lowband_out");
                for (v, &m) in lowband_out[..n0].iter_mut().zip(mid_buf[..n0].iter()) {
                    *v = n * m;
                    println!("{:.08}", *v);
                }
            }

            cm &= (1 << blocks) - 1;
            println!("cm {}", cm);
        }

        cm
    }

    fn decode_bands(
        &mut self,
        rd: &mut RangeDecoder,
        band: Range<usize>,
        coeff0: &mut [f32],
        coeff1: &mut [f32],
    ) {
        let lm = self.lm;

        let mut update_lowband = true;
        let mut lowband_offset = 0;

        const NORM_SIZE: usize = 8 * 100;
        let mut norm_mid = [0f32; NORM_SIZE];
        let mut norm_side = [0f32; NORM_SIZE];

        for i in band.clone() {
            let band_offset = (FREQ_BANDS[i] as usize) << lm;
            let band_size = (FREQ_RANGE[i] as usize) << lm;

            let x = &mut coeff0[band_offset..];
            let y = &mut coeff1[band_offset..];

            let consumed = rd.tell_frac() as i32;

            if i != band.start {
                self.remaining -= consumed;
            }

            self.remaining2 = (rd.available_frac() - 1 - self.anticollapse_bit) as i32;

            let b = if i <= self.codedband - 1 {
                println!("rem {} rem2 {}", self.remaining, self.remaining2);
                let divisor = ((self.codedband - 1).min(3) as i32).max(1);
                let remaining = self.remaining / divisor;
                (self.remaining2 + 1)
                    .min(self.pulses[i] + remaining)
                    .max(0)
                    .min(16383)
            } else {
                0
            };

            println!("b {}", b);

            if FREQ_BANDS[i] as i32 - FREQ_RANGE[i] as i32 >= FREQ_BANDS[band.start] as i32
                && (update_lowband || lowband_offset == 0)
            {
                lowband_offset = i;
            }

            let mut cm = [0, 0];
            let effective_lowband = if lowband_offset != 0
                && (self.spread != SPREAD_AGGRESSIVE || self.blocks > 1 || self.tf_change[i] < 0)
            {
                let effective_lowband =
                    FREQ_BANDS[band.start].max(FREQ_BANDS[lowband_offset] - FREQ_RANGE[i]);
                println!(
                    "effective_lowband {} off {} range {}",
                    effective_lowband, lowband_offset, FREQ_RANGE[i]
                );
                let mut foldstart = lowband_offset;

                for (e, &v) in FREQ_BANDS[..lowband_offset].iter().enumerate().rev() {
                    if v <= effective_lowband {
                        // println!("start [{}] {}", e, v);
                        foldstart = e;
                        break;
                    }
                }

                let mut foldend = lowband_offset;

                for (e, &v) in FREQ_BANDS[lowband_offset..].iter().enumerate() {
                    if v >= effective_lowband + FREQ_RANGE[i] {
                        // println!("end [{}] {}", e, v);
                        foldend += e;
                        break;
                    }
                }
                println!("fold {} {}", foldstart, foldend);

                for j in foldstart..foldend {
                    cm[0] |= self.frames[0].collapse_masks[j] as usize;
                    cm[1] |= self.frames[self.stereo_pkt as usize].collapse_masks[j] as usize;
                }

                Some(effective_lowband)
            } else {
                cm[0] = (1usize << self.blocks) - 1;
                cm[1] = cm[0];

                None
            };

            println!("cm {} {}", cm[0], cm[1]);

            if self.dual_stereo && i == self.intensity_stereo {
                self.dual_stereo = false;
                for j in (FREQ_BANDS[band.start] << lm) as usize..band_offset as usize {
                    norm_mid[j] = (norm_mid[j] + norm_side[j]) / 2.0;
                }
            }

            //            let mut lowband_scratch: [f32; 8 * 22] = unsafe { mem::uninitialized() };

            let lowband_off = effective_lowband.map(|e| (e << lm) as usize);
            let lowband_out_off = if i != band.end {
                Some(band_offset)
            } else {
                None
            };

            let (lowband_mid, lowband_mid_out) = norm_mid.split_at_mut(band_offset);
            let n = band_size;

            let lowband_mid = lowband_off.map(|off| &lowband_mid[off..off + n]);
            let lowband_mid_out = lowband_out_off.map(|_| lowband_mid_out);

            if self.dual_stereo {
                let (lowband_side, lowband_side_out) = norm_side.split_at_mut(band_offset);

                let lowband_side = lowband_off.map(|off| &lowband_side[off..off + n]);
                let lowband_side_out = lowband_out_off.map(|_| lowband_side_out);

                cm[0] = self.decode_band(
                    rd,
                    i,
                    x,
                    None,
                    band_size,
                    b / 2,
                    self.blocks,
                    lowband_mid,
                    lowband_mid_out,
                    lm as isize,
                    0,
                    1f32,
                    // &mut lowband_scratch,
                    cm[0],
                );

                cm[1] = self.decode_band(
                    rd,
                    i,
                    y,
                    None,
                    band_size,
                    b / 2,
                    self.blocks,
                    lowband_side,
                    lowband_side_out,
                    lm as isize,
                    0,
                    1f32,
                    // &mut lowband_scratch,
                    cm[1],
                );
            } else {
                cm[0] = self.decode_band(
                    rd,
                    i,
                    x,
                    Some(y),
                    band_size,
                    b / 2,
                    self.blocks,
                    lowband_mid,
                    lowband_mid_out,
                    lm as isize,
                    0,
                    1f32,
                    // &mut lowband_scratch,
                    cm[0] | cm[1],
                );
                cm[1] = cm[0];
            }

            self.frames[0].collapse_masks[i] = cm[0] as u8;
            self.frames[self.stereo_pkt as usize].collapse_masks[i] = cm[1] as u8;
            self.remaining += self.pulses[i] + consumed;

            update_lowband = b > ((band_size << 3) as i32);
        }
    }

    pub fn decode(
        &mut self,
        rd: &mut RangeDecoder,
        _out_buf: &mut [f32],
        frame_duration: FrameDuration,
        band: Range<usize>,
    ) {
        assert!(band.end <= MAX_BANDS);

        let frame_size = frame_duration.sample_count();

        self.lm = (frame_size / SHORT_BLOCKSIZE).celt_ilog2() - 1;

        println!("framebits {} tell {}", rd.len(), rd.tell());

        let silence = if rd.available() > 0 {
            rd.decode_logp(15)
        } else {
            true
        };

        println!("silence {}", silence);

        if silence {
            // Pretend we are at the end of the buffer
            rd.to_end();
        }

        self.reset_gains();
        if band.start == 0 && rd.available() >= 16 {
            self.parse_postfilter(rd);
        }

        let transient = if self.lm != 0 && rd.available() >= 3 {
            rd.decode_logp(3)
        } else {
            false
        };

        println!("duration {}, transient {}", self.lm, transient);

        self.blocks = if transient { 1 << self.lm } else { 1 };
        self.blocksize = frame_size / self.blocks;

        if !self.stereo_pkt {
            let (f0, f1) = self.frames.split_at_mut(1);

            f0[0]
                .energy
                .iter_mut()
                .zip(f1[0].energy.iter())
                .for_each(|(e0, &e1)| *e0 = e0.max(e1));
        }

        self.frames
            .iter_mut()
            .for_each(|f| f.collapse_masks.iter_mut().for_each(|c| *c = 0));

        self.decode_coarse_energy(rd, band.clone());

        println!(
            "available {} tell {} frac {}",
            rd.available(),
            rd.tell(),
            rd.tell_frac()
        );

        self.decode_tf_changes(rd, band.clone(), transient);

        println!(
            "available {} tell {} frac {}",
            rd.available(),
            rd.tell(),
            rd.tell_frac()
        );

        self.decode_allocation(rd, band.clone());

        println!(
            "available {} tell {} frac {}",
            rd.available(),
            rd.tell(),
            rd.tell_frac()
        );

        self.decode_fine_energy(rd, band.clone());

        // TODO: Consider spinning out a Band struct
        let mut coeff0 = [0f32; MAX_FRAME_SIZE];
        let mut coeff1 = [0f32; MAX_FRAME_SIZE];

        println!(
            "available {} tell {} frac {}",
            rd.available(),
            rd.tell(),
            rd.tell_frac()
        );

        self.decode_bands(rd, band.clone(), &mut coeff0, &mut coeff1);

        self.seed = rd.range as u32;

        // Apply simplified synthesis to produce audio output
        // This is a basic implementation that writes decoded coefficients
        // TODO: Implement full IMDCT synthesis with windowing and overlap-add
        self.synthesize_output(_out_buf, &coeff0, &coeff1, frame_size);
    }

    /// IMDCT synthesis with windowing and overlap-add
    fn synthesize_output(&mut self, out_buf: &mut [f32], coeff0: &[f32], coeff1: &[f32], frame_size: usize) {
        let num_channels = if self.stereo { 2 } else { 1 };
        let block_size = self.blocksize;
        let num_blocks = self.blocks;

        // Scale factor for IMDCT output
        let scale = 1.0 / (block_size as f32).sqrt();

        // Temporary buffers for IMDCT output
        let mut time_data = vec![0.0f32; block_size * 2];

        // Process each channel
        for ch in 0..num_channels {
            let coeffs = if ch == 0 { coeff0 } else { coeff1 };
            let overlap = &mut self.overlap[ch];

            // Process each block in this frame
            for block_idx in 0..num_blocks {
                let block_start = block_idx * block_size;
                let coeff_start = block_start;

                // Get coefficients for this block (half of blocksize due to MDCT symmetry)
                let block_coeffs = if coeff_start + block_size <= coeffs.len() {
                    &coeffs[coeff_start..coeff_start + block_size]
                } else {
                    // If we don't have enough coefficients, use what we have
                    let available = coeffs.len().saturating_sub(coeff_start);
                    if available > 0 {
                        &coeffs[coeff_start..coeff_start + available]
                    } else {
                        &[]
                    }
                };

                if block_coeffs.is_empty() {
                    // No data for this block, fill with zeros
                    for i in 0..block_size {
                        let out_idx = (block_start + i) * num_channels + ch;
                        if out_idx < out_buf.len() {
                            out_buf[out_idx] = overlap[i];
                            overlap[i] = 0.0;
                        }
                    }
                    continue;
                }

                // Apply IMDCT using the appropriate instance for current lm
                time_data.fill(0.0);
                self.imdct[self.lm].imdct15_half(&mut time_data[..block_size], block_coeffs, 1, scale);

                // Apply window and overlap-add (TDAC)
                // For CELT, we use a sine window
                for i in 0..block_size {
                    let window = ((i as f32 + 0.5) * std::f32::consts::PI / block_size as f32).sin();

                    // Overlap-add with previous frame's second half
                    let sample = time_data[i] * window + overlap[i];

                    // Store second half for next frame's overlap
                    if i < block_size / 2 {
                        overlap[i] = time_data[block_size - 1 - i] * window;
                    } else {
                        overlap[i] = -time_data[block_size - 1 - i] * window;
                    }

                    // Write to output buffer (interleaved)
                    let out_idx = (block_start + i) * num_channels + ch;
                    if out_idx < out_buf.len() {
                        out_buf[out_idx] = sample;
                    }
                }
            }
        }
    }
}

/// CELT decoder - wraps the internal Celt implementation for Symphonia compatibility.
pub struct Decoder {
    params: CodecParameters,
    buf: AudioBuffer<f32>,
    celt: Celt,
}

impl Decoder {
    pub fn try_new(params: CodecParameters) -> Result<Self> {
        let channels = params.channels.unwrap_or_else(|| {
            use symphonia_core::audio::Channels;
            Channels::FRONT_LEFT | Channels::FRONT_RIGHT
        });

        let spec = SignalSpec::new(48000, channels);
        let buf = AudioBuffer::new(960, spec);

        let stereo = channels.count() == 2;
        let celt = Celt::new(stereo);

        Ok(Decoder { params, buf, celt })
    }

    pub fn reset(&mut self) {
        self.buf.clear();
    }

    pub fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    pub fn decode(&mut self, packet: &Packet) -> Result<AudioBufferRef> {
        use crate::toc::Toc;

        let packet_data = packet.data.as_ref();
        if packet_data.is_empty() {
            return Err(symphonia_core::errors::Error::DecodeError("celt: empty packet"));
        }

        // Parse TOC byte to get frame duration and band range
        let toc_byte = packet_data[0];
        let toc = Toc::try_new(toc_byte)
            .map_err(|_| symphonia_core::errors::Error::DecodeError("celt: failed to parse TOC"))?;

        // Create range decoder from packet data (skip TOC byte)
        let mut rd = RangeDecoder::new(&packet_data[1..])
            .map_err(|_| symphonia_core::errors::Error::DecodeError("celt: failed to create range decoder"))?;

        // Determine band range based on bandwidth
        // For now, use full range (0..21)
        let band_range = 0..MAX_BANDS;

        // Create output buffer for decoded samples
        let frame_size = toc.frame_size as usize;
        let num_channels = self.buf.spec().channels.count();
        let mut out_buf = vec![0f32; frame_size * num_channels];

        // Call internal CELT decoder
        self.celt.decode(&mut rd, &mut out_buf, toc.frame_size, band_range);

        // Render the decoded samples to Symphonia's AudioBuffer
        self.buf.render_reserved(Some(frame_size));

        // Deinterleave samples from out_buf to Symphonia's planar format
        if num_channels == 2 {
            // Stereo - deinterleave L/R channels
            for i in 0..frame_size {
                if i * 2 + 1 < out_buf.len() {
                    self.buf.chan_mut(0)[i] = out_buf[i * 2];
                    self.buf.chan_mut(1)[i] = out_buf[i * 2 + 1];
                } else {
                    self.buf.chan_mut(0)[i] = 0.0;
                    self.buf.chan_mut(1)[i] = 0.0;
                }
            }
        } else {
            // Mono
            for i in 0..frame_size {
                self.buf.chan_mut(0)[i] = if i < out_buf.len() { out_buf[i] } else { 0.0 };
            }
        }

        Ok(self.buf.as_audio_buffer_ref())
    }
}
