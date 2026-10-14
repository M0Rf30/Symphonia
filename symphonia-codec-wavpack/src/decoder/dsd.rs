// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// WavPack DSD (1-bit) block decoder.
// Ported from `unpack_dsd.c` (`init_dsd_block()`, `unpack_dsd_samples()`, `init_dsd_block_fast()`,
// `decode_fast()`, `init_ptable()`, `init_dsd_block_high()`, `decode_high()`) of the WavPack
// reference implementation by David Bryant (https://github.com/dbry/WavPack, BSD licence).

use super::crc_accumulate;
use super::v4v5::UnpackError;
use crate::block::MONO_DATA;

/// The maximum number of history bits in DSD "fast" mode (`MAX_HISTORY_BITS`).
const MAX_HISTORY_BITS: u8 = 5;
/// The maximum number of bytes of the value lookup table per bin (`MAX_BYTES_PER_BIN`).
const MAX_BYTES_PER_BIN: usize = 1280;

/// The leading byte of an `ID_DSD_BLOCK` sub-block: the power of two by which the sample rate of
/// the block header (the "byte rate") is multiplied to get the rate of the DSD bytes' bit
/// stream divided by 8. `None` if the sub-block is malformed.
pub fn dsd_rate_shift(dsd_block: &[u8]) -> Option<u32> {
    match dsd_block {
        [shift, _, ..] if *shift <= 31 => Some(u32::from(*shift)),
        _ => None,
    }
}

#[inline(always)]
fn byte_ready(low: u32, high: u32) -> bool {
    (low ^ high) & 0xff00_0000 == 0
}

/// The largest block, in samples, that is accepted.
const MAX_BLOCK_SAMPLES: u32 = 1 << 20;

/// Decode the contents of an `ID_DSD_BLOCK` sub-block (`dsd_block`) of a block with
/// `block_samples` samples (DSD bytes per channel) and the given header `flags` and `crc`.
///
/// The returned vector has `block_samples` bytes per decoded channel, interleaved if there are
/// two channels. A `FALSE_STEREO` block decodes one channel. The CRC is verified.
pub fn unpack_dsd(
    flags: u32,
    block_samples: u32,
    dsd_block: &[u8],
    header_crc: u32,
) -> Result<Vec<i32>, UnpackError> {
    // A mutated `block_samples` must not result in a huge allocation (see `unpack_samples_v4v5`).
    if block_samples > MAX_BLOCK_SAMPLES {
        return Err(UnpackError::Malformed);
    }

    let channels = if flags & MONO_DATA != 0 { 1usize } else { 2 };
    let total = (block_samples as usize).checked_mul(channels).ok_or(UnpackError::Malformed)?;

    // `init_dsd_block()`.
    let [shift, mode, data @ ..] = dsd_block
    else {
        return Err(UnpackError::Malformed);
    };
    if *shift > 31 {
        return Err(UnpackError::Malformed);
    }

    let mut out: Vec<i32> = Vec::new();
    out.try_reserve_exact(total).map_err(|_| UnpackError::Malformed)?;

    match *mode {
        0 => {
            if data.len() != total {
                return Err(UnpackError::Malformed);
            }
            out.extend(data.iter().map(|&b| i32::from(b)));
        }
        1 => decode_fast(data, channels, total, &mut out)?,
        3 => decode_high(data, channels, total, &mut out)?,
        _ => return Err(UnpackError::Malformed),
    }

    let crc = out.iter().fold(u32::MAX, |crc, &b| crc_accumulate(crc, b as u32));
    if crc != header_crc {
        return Err(UnpackError::Crc);
    }

    Ok(out)
}

/// A byte-wise reader of the range coder's input.
struct Input<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Input<'_> {
    #[inline(always)]
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    #[inline(always)]
    fn next(&mut self) -> Option<u32> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(u32::from(b))
    }

    fn value32(&mut self) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..4 {
            value = (value << 8) | self.next()?;
        }
        Some(value)
    }
}

/// `init_dsd_block_fast()` and `decode_fast()`: the range coder with the probability tables
/// of the (history_bins x 256) previous byte contexts.
fn decode_fast(
    data: &[u8],
    channels: usize,
    total: usize,
    out: &mut Vec<i32>,
) -> Result<(), UnpackError> {
    let mut input = Input { data, pos: 0 };

    let history_bits = input.next().ok_or(UnpackError::Malformed)? as u8;
    if input.remaining() == 0 || history_bits > MAX_HISTORY_BITS {
        return Err(UnpackError::Malformed);
    }

    let bins = 1usize << history_bits;
    let max_probability = input.next().ok_or(UnpackError::Malformed)? as u8;

    let mut probabilities = vec![[0u8; 256]; bins];

    if max_probability < 0xff {
        let flat_len = bins * 256;
        let mut outpos = 0usize;

        while outpos < flat_len && input.remaining() > 0 {
            let code = input.next().ok_or(UnpackError::Malformed)? as u8;

            if code > max_probability {
                let zcount = usize::from(code - max_probability);
                // The table is zero-initialised, so skipping the zeros only advances.
                outpos = (outpos + zcount).min(flat_len);
            }
            else if code != 0 {
                probabilities[outpos >> 8][outpos & 0xff] = code;
                outpos += 1;
            }
            else {
                break;
            }
        }

        // The table must be complete and followed by a zero terminator (if any data is left).
        if outpos < flat_len || (input.remaining() > 0 && input.next() != Some(0)) {
            return Err(UnpackError::Malformed);
        }
    }
    else if input.remaining() > bins * 256 {
        for row in probabilities.iter_mut() {
            row.copy_from_slice(&input.data[input.pos..input.pos + 256]);
            input.pos += 256;
        }
    }
    else {
        return Err(UnpackError::Malformed);
    }

    let mut summed = vec![[0u16; 256]; bins];
    // The value lookup tables of all bins, `lookup_start[bi]` being where bin `bi`'s starts
    // (`usize::MAX` if the bin has no probabilities).
    let mut lookup: Vec<u8> = Vec::new();
    lookup.try_reserve_exact(bins * MAX_BYTES_PER_BIN).map_err(|_| UnpackError::Malformed)?;
    let mut lookup_start = [usize::MAX; 32];
    let mut total_summed = 0usize;

    for bi in 0..bins {
        let mut sum_values = 0usize;
        for i in 0..256 {
            sum_values += usize::from(probabilities[bi][i]);
            summed[bi][i] = sum_values as u16;
        }

        if sum_values != 0 {
            total_summed += sum_values;
            if total_summed > bins * MAX_BYTES_PER_BIN {
                return Err(UnpackError::Malformed);
            }

            lookup_start[bi] = lookup.len();
            for (i, &count) in probabilities[bi].iter().enumerate() {
                lookup.extend(std::iter::repeat_n(i as u8, usize::from(count)));
            }
        }
    }

    if input.remaining() < 4 {
        return Err(UnpackError::Malformed);
    }

    let mut value = input.value32().ok_or(UnpackError::Malformed)?;
    let (mut p0, mut p1) = (0usize, 0usize);
    let (mut low, mut high) = (0u32, u32::MAX);
    let mask = bins - 1;

    for _ in 0..total {
        let sum = u32::from(summed[p0][255]);
        if sum == 0 {
            return Err(UnpackError::Malformed);
        }

        let mut mult = high.wrapping_sub(low) / sum;

        if mult == 0 {
            if input.remaining() < 4 {
                return Err(UnpackError::Malformed);
            }
            value = input.value32().ok_or(UnpackError::Malformed)?;
            low = 0;
            high = u32::MAX;
            mult = high / sum;
            if mult == 0 {
                return Err(UnpackError::Malformed);
            }
        }

        let index = value.wrapping_sub(low) / mult;
        if index >= sum {
            return Err(UnpackError::Malformed);
        }

        let code = lookup[lookup_start[p0] + index as usize] as usize;
        out.push(code as i32);

        if code != 0 {
            low = low.wrapping_add(u32::from(summed[p0][code - 1]).wrapping_mul(mult));
        }
        high =
            low.wrapping_add(u32::from(probabilities[p0][code]).wrapping_mul(mult)).wrapping_sub(1);

        if channels == 1 {
            p0 = code & mask;
        }
        else {
            p0 = p1;
            p1 = code & mask;
        }

        while byte_ready(high, low) {
            value = (value << 8) | input.next().ok_or(UnpackError::Malformed)?;
            high = (high << 8) | 0xff;
            low <<= 8;
        }
    }

    Ok(())
}

const PTABLE_BITS: u32 = 8;
const PTABLE_BINS: usize = 1 << PTABLE_BITS;
const PTABLE_MASK: i32 = PTABLE_BINS as i32 - 1;

const UP: i32 = 0x0100_00fe;
const DOWN: i32 = 0x0001_0000;
const DECAY: u32 = 8;

const PRECISION: u32 = 20;
const VALUE_ONE: i32 = 1 << PRECISION;
const PRECISION_USE: u32 = 12;

const RATE_S: u8 = 20;

/// Move `value` `count` steps towards `DOWN`. Stops early once it has converged (it is a fixed
/// point), which bounds the work for corrupt parameters.
fn decay(value: &mut i32, count: i32) {
    for _ in 0..count {
        let delta = (DOWN - *value) >> DECAY;
        if delta == 0 {
            break;
        }
        *value += delta;
    }
}

/// `init_ptable()`.
fn init_ptable(rate_i: u8, rate_s: u8) -> [i32; PTABLE_BINS] {
    let mut table = [0i32; PTABLE_BINS];
    let rate_s = i32::from(rate_s);
    let mut value: i32 = 0x0080_8000;
    let mut rate: i32 = i32::from(rate_i) << 8;

    decay(&mut value, (rate + 128) >> 8);

    for i in 0..PTABLE_BINS / 2 {
        table[i] = value;
        table[PTABLE_BINS - 1 - i] = 0x0100_ffff - value;

        if value > 0x01_0000 {
            // The reference relies on wrapping signed arithmetic here for implausible rates.
            rate = rate.wrapping_add(rate.wrapping_mul(rate_s).wrapping_add(128) >> 8);
            decay(&mut value, rate.wrapping_add(64) >> 7);
        }
    }

    table
}

/// The adaptive predictor state of one channel in "high" mode (`DSDfilters`).
#[derive(Default, Clone, Copy)]
struct Filters {
    value: i32,
    filter0: i32,
    filter1: i32,
    filter2: i32,
    filter3: i32,
    filter4: i32,
    filter5: i32,
    filter6: i32,
    factor: i32,
    byte: u32,
}

impl Filters {
    /// Decode one bit of the channel with the shared range coder and predictor tables, then
    /// update the predictor. This is the body of the per-bit loop of `decode_high()`.
    #[inline(always)]
    fn step(
        &mut self,
        ptable: &mut [i32; PTABLE_BINS],
        rc: &mut RangeCoder<'_>,
    ) -> Result<(), UnpackError> {
        let pp = &mut ptable[((self.value >> (PRECISION - PRECISION_USE)) & PTABLE_MASK) as usize];
        let split = rc
            .low
            .wrapping_add((rc.high.wrapping_sub(rc.low) >> 8).wrapping_mul((*pp >> 16) as u32));

        if rc.value <= split {
            rc.high = split;
            *pp += (UP - *pp) >> DECAY;
            self.filter0 = -1;
        }
        else {
            rc.low = split.wrapping_add(1);
            *pp += (DOWN - *pp) >> DECAY;
            self.filter0 = 0;
        }

        while byte_ready(rc.high, rc.low) {
            rc.value = (rc.value << 8) | rc.input.next().ok_or(UnpackError::Malformed)?;
            rc.high = (rc.high << 8) | 0xff;
            rc.low <<= 8;
        }

        self.value = self.value.wrapping_add(self.filter6.wrapping_mul(8));
        self.byte = (self.byte << 1) | (self.filter0 & 1) as u32;
        self.factor = self.factor.wrapping_add(
            (((self.value ^ self.filter0) >> 31) | 1)
                & ((self.value ^ self.value.wrapping_sub(self.filter6.wrapping_mul(16))) >> 31),
        );
        self.filter1 =
            self.filter1.wrapping_add(((self.filter0 & VALUE_ONE).wrapping_sub(self.filter1)) >> 6);
        self.filter2 =
            self.filter2.wrapping_add(((self.filter0 & VALUE_ONE).wrapping_sub(self.filter2)) >> 4);
        self.filter3 = self.filter3.wrapping_add(self.filter2.wrapping_sub(self.filter3) >> 4);
        self.filter4 = self.filter4.wrapping_add(self.filter3.wrapping_sub(self.filter4) >> 4);
        self.value = self.filter4.wrapping_sub(self.filter5) >> 4;
        self.filter5 = self.filter5.wrapping_add(self.value);
        self.filter6 = self.filter6.wrapping_add(self.value.wrapping_sub(self.filter6) >> 3);
        self.value = self.prediction();

        Ok(())
    }

    /// `filter1 - filter5 + ((filter6 * factor) >> 2)`.
    #[inline(always)]
    fn prediction(&self) -> i32 {
        self.filter1
            .wrapping_sub(self.filter5)
            .wrapping_add(self.filter6.wrapping_mul(self.factor) >> 2)
    }
}

struct RangeCoder<'a> {
    input: Input<'a>,
    low: u32,
    high: u32,
    value: u32,
}

/// `init_dsd_block_high()` and `decode_high()`: a bitwise range coder driven by a predictor
/// on the history of each channel's bits.
fn decode_high(
    data: &[u8],
    channels: usize,
    total: usize,
    out: &mut Vec<i32>,
) -> Result<(), UnpackError> {
    let mut input = Input { data, pos: 0 };

    if input.remaining() < if channels == 1 { 13 } else { 20 } {
        return Err(UnpackError::Malformed);
    }

    let rate_i = input.next().ok_or(UnpackError::Malformed)? as u8;
    let rate_s = input.next().ok_or(UnpackError::Malformed)? as u8;
    if rate_s != RATE_S {
        return Err(UnpackError::Malformed);
    }

    let mut ptable = init_ptable(rate_i, rate_s);
    let mut filters = [Filters::default(); 2];

    for f in filters.iter_mut().take(channels) {
        let mut read_filter = || -> Result<i32, UnpackError> {
            Ok((input.next().ok_or(UnpackError::Malformed)? as i32) << (PRECISION - 8))
        };
        f.filter1 = read_filter()?;
        f.filter2 = read_filter()?;
        f.filter3 = read_filter()?;
        f.filter4 = read_filter()?;
        f.filter5 = read_filter()?;
        f.filter6 = 0;
        let lo = input.next().ok_or(UnpackError::Malformed)?;
        let hi = input.next().ok_or(UnpackError::Malformed)?;
        f.factor = i32::from((lo | (hi << 8)) as u16 as i16);
    }

    let value = input.value32().ok_or(UnpackError::Malformed)?;
    let mut rc = RangeCoder { input, low: 0, high: u32::MAX, value };

    for _ in 0..total / channels {
        filters[0].value = filters[0].prediction();
        if channels == 2 {
            filters[1].value = filters[1].prediction();
        }

        for _ in 0..8 {
            filters[0].step(&mut ptable, &mut rc)?;
            if channels == 2 {
                filters[1].step(&mut ptable, &mut rc)?;
            }
        }

        for f in filters.iter_mut().take(channels) {
            out.push((f.byte & 0xff) as i32);
            f.factor -= (f.factor + 512) >> 10;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_raw_mode_roundtrip_and_crc() {
        let bytes = [0x69u8, 0x96, 0x00, 0xff];
        let mut block = vec![2u8, 0];
        block.extend(bytes);

        let crc = bytes.iter().fold(u32::MAX, |c, &b| crc_accumulate(c, u32::from(b)));

        let out = unpack_dsd(0, 2, &block, crc).unwrap();
        assert_eq!(out, [0x69, 0x96, 0x00, 0xff]);

        // A mismatching CRC, a wrong length and an unknown mode are errors.
        assert_eq!(unpack_dsd(0, 2, &block, crc ^ 1), Err(UnpackError::Crc));
        assert_eq!(unpack_dsd(0, 3, &block, crc), Err(UnpackError::Malformed));
        assert_eq!(unpack_dsd(0, 2, &[2, 2, 0, 0, 0, 0], crc), Err(UnpackError::Malformed));
        assert_eq!(unpack_dsd(0, 2, &[40, 0, 1, 2, 3, 4], crc), Err(UnpackError::Malformed));
    }

    #[test]
    fn verify_corrupt_coded_modes_do_not_panic() {
        for mode in [1u8, 3] {
            for len in [0usize, 1, 2, 5, 13, 19, 20, 40, 300] {
                for fill in [0u8, 1, 20, 0xff] {
                    let mut block = vec![3u8, mode];
                    block.extend(std::iter::repeat_n(fill, len));
                    for flags in [0, MONO_DATA] {
                        let _ = unpack_dsd(flags, 64, &block, 0);
                    }
                }
            }
        }
    }
}
