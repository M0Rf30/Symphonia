// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Frame durations, sample rates, and the sizes derived from them.
//!
//! Port of the definitions of liblc3 `include/lc3_private.h` (`enum lc3_dt`, `enum lc3_srate`,
//! `LC3_NS`), `src/common.h`, and of the `lc3_ns_2m5`, `lc3_ne_2m5`, `lc3_ns_4m` and
//! `lc3_frame_bytes_hr_lim` tables of `src/tables.c`.

/// Frame duration identifier (`enum lc3_dt`).
pub const DT_2M5: usize = 0;
pub const DT_5M: usize = 1;
pub const DT_7M5: usize = 2;
pub const DT_10M: usize = 3;

/// Sample rate identifier (`enum lc3_srate`).
pub const SRATE_8K: usize = 0;
pub const SRATE_16K: usize = 1;
pub const SRATE_24K: usize = 2;
pub const SRATE_32K: usize = 3;
pub const SRATE_48K: usize = 4;
pub const SRATE_48K_HR: usize = 5;
pub const SRATE_96K_HR: usize = 6;

/// Bandwidth identifiers (`enum lc3_bandwidth`) share the values of the sample rates.
pub const BANDWIDTH_SWB: usize = SRATE_32K;
pub const BANDWIDTH_FB: usize = SRATE_48K;

/// Minimum size of a frame, in bytes (`LC3_MIN_FRAME_BYTES`).
pub const MIN_FRAME_BYTES: usize = 20;

/// Maximum size of a frame, in bytes, of the non-high-resolution modes (`LC3_MAX_FRAME_BYTES`).
pub const MAX_FRAME_BYTES: usize = 400;

/// Maximum number of samples of a frame (`LC3_MAX_NS`, 10 ms at 96 kHz).
pub const MAX_NS: usize = 960;

/// Number of samples of a 2.5 ms frame, by sample rate (`lc3_ns_2m5`).
const NS_2M5: [usize; 7] = [20, 40, 60, 80, 120, 120, 240];

/// Number of coded coefficients of a 2.5 ms frame, by sample rate (`lc3_ne_2m5`).
const NE_2M5: [usize; 7] = [20, 40, 60, 80, 100, 120, 240];

/// Number of samples of a 4 ms frame, by sample rate (`lc3_ns_4m`).
const NS_4M: [usize; 7] = [32, 64, 96, 128, 192, 192, 384];

/// Limits of the size of a frame of the high-resolution modes, by `[dt][sr - 48K_HR]`
/// (`lc3_frame_bytes_hr_lim`): minimum and maximum. The 7.5 ms duration has no such mode.
const FRAME_BYTES_HR_LIM: [[(usize, usize); 2]; 4] = [
    [(54 / 2, 210), (62 / 2, 210)],
    [(93 / 2, 375), (109 / 2, 375)],
    [(0, 0), (0, 0)],
    [(156 / 2, 625), (187 / 2, 625)],
];

/// Returns `true` for the high-resolution sample rates (`lc3_hr`).
#[inline]
pub fn is_hr(sr: usize) -> bool {
    sr >= SRATE_48K_HR
}

/// Returns the number of samples of a frame (`lc3_ns`).
#[inline]
pub fn ns(dt: usize, sr: usize) -> usize {
    NS_2M5[sr] * (1 + dt)
}

/// Returns the number of coded coefficients of a frame (`lc3_ne`).
#[inline]
pub fn ne(dt: usize, sr: usize) -> usize {
    NE_2M5[sr] * (1 + dt)
}

/// Returns the number of samples of a 4 ms frame (`lc3_ns_4m`).
#[inline]
pub fn ns_4m(sr: usize) -> usize {
    NS_4M[sr]
}

/// Returns the number of delayed samples of the MDCT (`lc3_nd`).
#[inline]
pub fn nd(dt: usize, sr: usize) -> usize {
    (ns(dt, sr) + if dt == DT_7M5 { NS_4M[sr] } else { NS_2M5[sr] }) >> 1
}

/// Returns the number of samples of history kept for the long term postfilter (`lc3_nh`).
#[inline]
pub fn nh(dt: usize, sr: usize) -> usize {
    if sr > SRATE_48K_HR { 0 } else { (8 + usize::from(dt == DT_7M5)) * NS_2M5[sr] }
}

/// Returns the maximum size of a frame, in bytes (`lc3_max_frame_bytes`).
#[inline]
pub fn max_frame_bytes(dt: usize, sr: usize) -> usize {
    if is_hr(sr) { FRAME_BYTES_HR_LIM[dt][sr - SRATE_48K_HR].1 } else { MAX_FRAME_BYTES }
}

/// Returns the frame duration identifier of a duration in microseconds (`resolve_dt`).
pub fn resolve_dt(us: u32, hrmode: bool) -> Option<usize> {
    match us {
        2500 => Some(DT_2M5),
        5000 => Some(DT_5M),
        7500 if !hrmode => Some(DT_7M5),
        10000 => Some(DT_10M),
        _ => None,
    }
}

/// Returns the sample rate identifier of a sample rate in Hz (`resolve_srate`).
pub fn resolve_srate(hz: u32, hrmode: bool) -> Option<usize> {
    match (hrmode, hz) {
        (false, 8000) => Some(SRATE_8K),
        (false, 16000) => Some(SRATE_16K),
        (false, 24000) => Some(SRATE_24K),
        (false, 32000) => Some(SRATE_32K),
        (false, 48000) => Some(SRATE_48K),
        (true, 48000) => Some(SRATE_48K_HR),
        (true, 96000) => Some(SRATE_96K_HR),
        _ => None,
    }
}
