// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CELT mode tables. Ported from libopus `celt/modes.c` / `celt/static_modes_float.h`
//! (`struct OpusCustomMode`, `celt_mode_static`). Ported from libopus (BSD-3-Clause), see
//! NOTICE. Owner (wave 1): "CeltBitstream".
//!
//! Only the single 48 kHz / 960-sample (20 ms max frame) mode is needed — Opus never uses
//! `opus_custom_mode_create` with other parameters.

/// C: `struct PulseCache` (`celt/modes.h`).
pub struct PulseCache {
    pub size: i32,
    /// Indexed by `[band]`: offset into `bits`/`caps`.
    pub index: &'static [i16],
    pub bits: &'static [u8],
    pub caps: &'static [u8],
}

pub struct CeltMode {
    pub sample_rate: i32,
    pub overlap: i32,
    pub nb_ebands: i32,
    pub effective_ebands: i32,
    /// Band edges, in units of 400 Hz (25 entries for 21 bands + sentinels), C: `eBands`.
    pub e_bands: &'static [i16],
    pub max_lm: i32,
    pub nb_short_mdcts: i32,
    pub short_mdct_size: i32,
    /// C: `allocVectors`: the static bit-allocation table used by `rate.rs`'s
    /// `clt_compute_allocation`.
    pub alloc_vectors: &'static [u8],
    pub log_n: &'static [i16],
    /// Analysis/synthesis window, length `overlap`, C: `window`.
    pub window: &'static [f32],
    pub cache: PulseCache,
    // Deviation from the WIP snapshot: dropped the `mdct: &'static
    // crate::celt::mdct::MdctLookup` field the previous session had added here (not part of
    // wave 0's `CeltMode`; referenced a `MDCT_LOOKUP_960` symbol that doesn't exist yet, which
    // blocked the whole crate from compiling). "CeltSynthesis" owns `celt/mdct.rs` and its
    // `MdctLookup`; per `celt_decoder.c`, the MDCT lookup only needs `short_mdct_size`/
    // `max_lm`/`overlap` (all present above) to construct, so `CeltDecoder` (owned by
    // "CeltSynthesis") can hold/construct its own `MdctLookup` independently instead of
    // threading it through the shared static `CeltMode`. See coordination log.
}

impl CeltMode {
    /// C: `m->nbAllocVectors` (not stored directly in wave 0's `CeltMode`, derived here).
    pub fn nb_alloc_vectors(&self) -> i32 {
        self.alloc_vectors.len() as i32 / self.nb_ebands
    }
}

/// C: `eband5ms` (`celt/modes.c`). Critical band boundaries in units of 400 Hz, for a
/// 5 ms (M=1/4 of a 20ms frame at LM=3... actually the base 400Hz-per-bin unit) reference
/// scale; `e_bands` for the 48 kHz/960 mode is this table verbatim.
static EBAND5MS: [i16; 22] =
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100];

/// C: `band_allocation` (`celt/modes.c`). Bit allocation table in units of 1/32 bit/sample
/// (0.1875 dB SNR), `BITALLOC_SIZE` (11) rows by `nbEBands` (21) columns.
static BAND_ALLOCATION: [u8; 231] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    90, 80, 75, 69, 63, 56, 49, 40, 34, 29, 20, 18, 10, 0, 0, 0, 0, 0, 0, 0, 0, //
    110, 100, 90, 84, 78, 71, 65, 58, 51, 45, 39, 32, 26, 20, 12, 0, 0, 0, 0, 0, 0, //
    118, 110, 103, 93, 86, 80, 75, 70, 65, 59, 53, 47, 40, 31, 23, 15, 4, 0, 0, 0, 0, //
    126, 119, 112, 104, 95, 89, 83, 78, 72, 66, 60, 54, 47, 39, 32, 25, 17, 12, 1, 0, 0, //
    134, 127, 120, 114, 103, 97, 91, 85, 78, 72, 66, 60, 54, 47, 41, 35, 29, 23, 16, 10, 1, //
    144, 137, 130, 124, 113, 107, 101, 95, 88, 82, 76, 70, 64, 57, 51, 45, 39, 33, 26, 15, 1, //
    152, 145, 138, 132, 123, 117, 111, 105, 98, 92, 86, 80, 74, 67, 61, 55, 49, 43, 36, 20, 1, //
    162, 155, 148, 142, 133, 127, 121, 115, 108, 102, 96, 90, 84, 77, 71, 65, 59, 53, 46, 30, 1, //
    172, 165, 158, 152, 143, 137, 131, 125, 118, 112, 106, 100, 94, 87, 81, 75, 69, 63, 56, 45, 20, //
    200, 200, 200, 200, 200, 200, 200, 200, 198, 193, 188, 183, 178, 173, 168, 163, 158, 153, 148, 129, 104,
];

/// C: `logN400` (`celt/static_modes_float.h`).
static LOG_N400: [i16; 21] =
    [0, 0, 0, 0, 0, 0, 0, 0, 8, 8, 8, 8, 16, 16, 16, 21, 21, 24, 29, 34, 36];

/// C: `window120` (`celt/static_modes_float.h`). Analysis/synthesis window.
static WINDOW120: [f32; 120] = [
    6.7286966e-05, 0.00060551348, 0.0016815970, 0.0032947962, 0.0054439943, 0.0081276923,
    0.011344001, 0.015090633, 0.019364886, 0.024163635, 0.029483315, 0.035319905,
    0.041668911, 0.048525347, 0.055883718, 0.063737999, 0.072081616, 0.080907428,
    0.090207705, 0.099974111, 0.11019769, 0.12086883, 0.13197729, 0.14351214,
    0.15546177, 0.16781389, 0.18055550, 0.19367290, 0.20715171, 0.22097682,
    0.23513243, 0.24960208, 0.26436860, 0.27941419, 0.29472040, 0.31026818,
    0.32603788, 0.34200931, 0.35816177, 0.37447407, 0.39092462, 0.40749142,
    0.42415215, 0.44088423, 0.45766484, 0.47447104, 0.49127978, 0.50806798,
    0.52481261, 0.54149077, 0.55807973, 0.57455701, 0.59090049, 0.60708841,
    0.62309951, 0.63891306, 0.65450896, 0.66986776, 0.68497077, 0.69980010,
    0.71433873, 0.72857055, 0.74248043, 0.75605424, 0.76927895, 0.78214257,
    0.79463430, 0.80674445, 0.81846456, 0.82978733, 0.84070669, 0.85121779,
    0.86131698, 0.87100183, 0.88027111, 0.88912479, 0.89756398, 0.90559094,
    0.91320904, 0.92042270, 0.92723738, 0.93365955, 0.93969656, 0.94535671,
    0.95064907, 0.95558353, 0.96017067, 0.96442171, 0.96834849, 0.97196334,
    0.97527906, 0.97830883, 0.98106616, 0.98356480, 0.98581869, 0.98784191,
    0.98964856, 0.99125274, 0.99266849, 0.99390969, 0.99499004, 0.99592297,
    0.99672162, 0.99739874, 0.99796667, 0.99843728, 0.99882195, 0.99913147,
    0.99937606, 0.99956527, 0.99970802, 0.99981248, 0.99988613, 0.99993565,
    0.99996697, 0.99998518, 0.99999457, 0.99999859, 0.99999982, 1.0000000,
];

/// C: `cache_index50` (`celt/static_modes_float.h`).
static CACHE_INDEX50: [i16; 105] = [
    -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, 0, 0, 41, 41, 41, 82,
    82, 123, 164, 200, 222, 0, 0, 0, 0, 0, 0, 0, 0, 41, 41, 41,
    41, 123, 123, 123, 164, 164, 240, 266, 283, 295, 41, 41, 41, 41, 41, 41,
    41, 41, 123, 123, 123, 123, 240, 240, 240, 266, 266, 305, 318, 328, 336, 123,
    123, 123, 123, 123, 123, 123, 123, 240, 240, 240, 240, 305, 305, 305, 318, 318,
    343, 351, 358, 364, 240, 240, 240, 240, 240, 240, 240, 240, 305, 305, 305, 305,
    343, 343, 343, 351, 351, 370, 376, 382, 387,
];

/// C: `cache_bits50` (`celt/static_modes_float.h`).
static CACHE_BITS50: [u8; 392] = [
    40, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 40, 15, 23, 28, 31, 34, 36, 38, 39, 41, 42, 43, 44, 45, 46, 47, 47, 49, 50,
    51, 52, 53, 54, 55, 55, 57, 58, 59, 60, 61, 62, 63, 63, 65, 66, 67, 68, 69, 70,
    71, 71, 40, 20, 33, 41, 48, 53, 57, 61, 64, 66, 69, 71, 73, 75, 76, 78, 80, 82,
    85, 87, 89, 91, 92, 94, 96, 98, 101, 103, 105, 107, 108, 110, 112, 114, 117, 119, 121, 123,
    124, 126, 128, 40, 23, 39, 51, 60, 67, 73, 79, 83, 87, 91, 94, 97, 100, 102, 105, 107,
    111, 115, 118, 121, 124, 126, 129, 131, 135, 139, 142, 145, 148, 150, 153, 155, 159, 163, 166, 169,
    172, 174, 177, 179, 35, 28, 49, 65, 78, 89, 99, 107, 114, 120, 126, 132, 136, 141, 145, 149,
    153, 159, 165, 171, 176, 180, 185, 189, 192, 199, 205, 211, 216, 220, 225, 229, 232, 239, 245, 251,
    21, 33, 58, 79, 97, 112, 125, 137, 148, 157, 166, 174, 182, 189, 195, 201, 207, 217, 227, 235,
    243, 251, 17, 35, 63, 86, 106, 123, 139, 152, 165, 177, 187, 197, 206, 214, 222, 230, 237, 250,
    25, 31, 55, 75, 91, 105, 117, 128, 138, 146, 154, 161, 168, 174, 180, 185, 190, 200, 208, 215,
    222, 229, 235, 240, 245, 255, 16, 36, 65, 89, 110, 128, 144, 159, 173, 185, 196, 207, 217, 226,
    234, 242, 250, 11, 41, 74, 103, 128, 151, 172, 191, 209, 225, 241, 255, 9, 43, 79, 110, 138,
    163, 186, 207, 227, 246, 12, 39, 71, 99, 123, 144, 164, 182, 198, 214, 228, 241, 253, 9, 44,
    81, 113, 142, 168, 192, 214, 235, 255, 7, 49, 90, 127, 160, 191, 220, 247, 6, 51, 95, 134,
    170, 203, 234, 7, 47, 87, 123, 155, 184, 212, 237, 6, 52, 97, 137, 174, 208, 240, 5, 57,
    106, 151, 192, 231, 5, 59, 111, 158, 202, 243, 5, 55, 103, 147, 187, 224, 5, 60, 113, 161,
    206, 248, 4, 65, 122, 175, 224, 4, 67, 127, 182, 234,
];

/// C: `cache_caps50` (`celt/static_modes_float.h`).
static CACHE_CAPS50: [u8; 168] = [
    224, 224, 224, 224, 224, 224, 224, 224, 160, 160, 160, 160, 185, 185, 185, 178, 178, 168, 134, 61,
    37, 224, 224, 224, 224, 224, 224, 224, 224, 240, 240, 240, 240, 207, 207, 207, 198, 198, 183, 144,
    66, 40, 160, 160, 160, 160, 160, 160, 160, 160, 185, 185, 185, 185, 193, 193, 193, 183, 183, 172,
    138, 64, 38, 240, 240, 240, 240, 240, 240, 240, 240, 207, 207, 207, 207, 204, 204, 204, 193, 193,
    180, 143, 66, 40, 185, 185, 185, 185, 185, 185, 185, 185, 193, 193, 193, 193, 193, 193, 193, 183,
    183, 172, 138, 65, 39, 207, 207, 207, 207, 207, 207, 207, 207, 204, 204, 204, 204, 201, 201, 201,
    188, 188, 176, 141, 66, 40, 193, 193, 193, 193, 193, 193, 193, 193, 193, 193, 193, 193, 194, 194,
    194, 184, 184, 173, 139, 65, 39, 204, 204, 204, 204, 204, 204, 204, 204, 201, 201, 201, 201, 198,
    198, 198, 187, 187, 175, 140, 66, 40,
];

/// C: `static_mode_48000_960_120` (the sole mode Opus uses).
pub static MODE_48000_960: CeltMode = CeltMode {
    sample_rate: 48000,
    overlap: 120,
    nb_ebands: 21,
    effective_ebands: 21,
    e_bands: &EBAND5MS,
    max_lm: 3,
    nb_short_mdcts: 8,
    short_mdct_size: 120,
    alloc_vectors: &BAND_ALLOCATION,
    log_n: &LOG_N400,
    window: &WINDOW120,
    cache: PulseCache { size: 392, index: &CACHE_INDEX50, bits: &CACHE_BITS50, caps: &CACHE_CAPS50 },
};

// ---------------------------------------------------------------------------------------------
// Shared float math helpers. Ported from libopus `celt/mathops.h` (float / non-FIXED_POINT,
// non-FLOAT_APPROX build: transcendentals go through libm `log`/`exp`/`sqrt`/`cos`, matching
// what a standard (non-embedded) libopus float build uses). Home chosen per wave-1 contract:
// `crate::celt::modes` (a `pub(crate)` section of the file owned by "CeltBitstream"), shared by
// `laplace.rs`/`quant_bands.rs`/`rate.rs`/`cwrs.rs`/`vq.rs`/`bands.rs` (all mine) and by
// "CeltSynthesis"'s `pitch.rs`/`lpc.rs`/`celt.rs`/`decoder.rs` (e.g. `celt_sqrt`).
// ---------------------------------------------------------------------------------------------

/// C: `EC_ILOG` (`celt/ecintrin.h`), i.e. `1 + floor(log2(v))`, `0` for `v == 0`.
pub(crate) fn ec_ilog(v: u32) -> i32 {
    32 - v.leading_zeros() as i32
}

/// C: `isqrt32` (`celt/mathops.c`). `floor(sqrt(val))` with exact integer arithmetic.
pub(crate) fn isqrt32(mut val: u32) -> u32 {
    let mut g: u32 = 0;
    let mut bshift = (ec_ilog(val) - 1) >> 1;
    let mut b: u32 = 1u32 << bshift;
    loop {
        let t = ((g << 1).wrapping_add(b)) << bshift;
        if t <= val {
            g += b;
            val -= t;
        }
        b >>= 1;
        bshift -= 1;
        if bshift < 0 {
            break;
        }
    }
    g
}

/// C: `celt_sqrt` (float build: `(float)sqrt(x)`).
pub(crate) fn celt_sqrt(x: f32) -> f32 {
    (x as f64).sqrt() as f32
}

/// C: `celt_rsqrt` (float build: `1.f/celt_sqrt(x)`).
pub(crate) fn celt_rsqrt(x: f32) -> f32 {
    1.0 / celt_sqrt(x)
}

/// C: `celt_rsqrt_norm` (float build: alias of [`celt_rsqrt`]).
pub(crate) fn celt_rsqrt_norm(x: f32) -> f32 {
    celt_rsqrt(x)
}

/// C: `celt_cos_norm` (float build: `(float)cos((.5f*PI)*(x))`).
pub(crate) fn celt_cos_norm(x: f32) -> f32 {
    const PI: f64 = 3.141592653_f64;
    ((0.5_f64 * PI) * x as f64).cos() as f32
}

/// C: `celt_log2` (float build, non-`FLOAT_APPROX`: `(float)(1.442695040888963387*log(x))`).
pub(crate) fn celt_log2(x: f32) -> f32 {
    (1.442695040888963387_f64 * (x as f64).ln()) as f32
}

/// C: `celt_exp2` (float build, non-`FLOAT_APPROX`: `(float)exp(0.6931471805599453094*(x))`).
pub(crate) fn celt_exp2(x: f32) -> f32 {
    (0.6931471805599453094_f64 * x as f64).exp() as f32
}

/// C: `celt_udiv` (`celt/entcode.h`; the small-division-table fast path is behind
/// `USE_SMALL_DIV_TABLE`, unused by the reference float build we target).
pub(crate) fn celt_udiv(n: u32, d: u32) -> u32 {
    n / d
}

/// C: `celt_sudiv` (`celt/entcode.h`).
pub(crate) fn celt_sudiv(n: i32, d: i32) -> i32 {
    n / d
}

/// C: `celt_div` (`celt/mathops.h`, float build: `(a)/(b)`).
pub(crate) fn celt_div(a: f32, b: f32) -> f32 {
    a / b
}

/// C: `EPSILON` (`celt/arch.h`, float build).
pub(crate) const EPSILON: f32 = 1e-15;

/// C: `fast_atan2f` (`celt/mathops.h`, used when `!FIXED_POINT` — i.e. always, for our float
/// build).
pub(crate) fn fast_atan2f(y: f32, x: f32) -> f32 {
    const CA: f32 = 0.43157974;
    const CB: f32 = 0.67848403;
    const CC: f32 = 0.08595542;
    const CE: f32 = std::f32::consts::PI / 2.0;
    let x2 = x * x;
    let y2 = y * y;
    if x2 + y2 < 1e-18 {
        return 0.0;
    }
    if x2 < y2 {
        let den = (y2 + CB * x2) * (y2 + CC * x2);
        -x * y * (y2 + CA * x2) / den + if y < 0.0 { -CE } else { CE }
    }
    else {
        let den = (x2 + CB * y2) * (x2 + CC * y2);
        x * y * (x2 + CA * y2) / den + if y < 0.0 { -CE } else { CE }
            - if x * y < 0.0 { -CE } else { CE }
    }
}
