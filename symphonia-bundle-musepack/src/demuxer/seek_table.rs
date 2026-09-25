// Symphonia Musepack demuxer (SV8 seek table)
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SV8 `ST` (seek table) chunk parser.
//!
//! Ported from libmpcdec `mpc_demux.c` (`mpc_demux_ST`, `mpc_st_read`/`mpc_st_read_size`/
//! `mpc_st_read_golomb`, `mpc_seek_table_entries`) (BSD-3-Clause), see `NOTICE`.
//!
//! This crate could not be validated against a real file containing an `ST` block (neither the
//! self-generated SV8 fixtures nor the real-world SV7 sample used during development contain
//! one -- `mpcenc` did not emit one for the small test files produced here). The parser below is
//! a faithful, defensively-bounded port, but [`SeekTable::parse`] is used with a fallback: any
//! parse failure, or a table that fails [`SeekTable::sanity_check`], causes the demuxer to fall
//! back to the (verified) linear block-size scan instead of trusting unverified table data.

use crate::decoder_core::FRAME_LENGTH;

const MAX_SEEK_TABLE_SIZE: u64 = 65536;

/// A parsed SV8 seek table: absolute bit positions of Musepack blocks, spaced every
/// `2^seek_pwr` frames.
pub(crate) struct SeekTable {
    pub entries: Vec<u64>,
    pub seek_pwr: u32,
}

/// Simple MSB-first bit reader over a byte slice, mirroring `mpc_st_reader`.
struct StReader<'a> {
    data: &'a [u8],
    bit_pos: u64,
    bit_size: u64,
}

impl<'a> StReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        StReader { data, bit_pos: 0, bit_size: (data.len() as u64) * 8 }
    }

    /// Ported from `mpc_st_read`.
    fn read(&mut self, bits: u32) -> Option<u32> {
        if bits > 32 || self.bit_pos > self.bit_size || u64::from(bits) > self.bit_size - self.bit_pos
        {
            return None;
        }
        let mut result: u32 = 0;
        for _ in 0..bits {
            let byte_idx = (self.bit_pos >> 3) as usize;
            let bit_idx = 7 - (self.bit_pos & 7) as u32;
            let bit = u32::from((self.data.get(byte_idx).copied().unwrap_or(0) >> bit_idx) & 1);
            result = (result << 1) | bit;
            self.bit_pos += 1;
        }
        Some(result)
    }

    /// Ported from `mpc_st_read_size`.
    fn read_size(&mut self) -> Option<u64> {
        let mut result: u64 = 0;
        for _ in 0..10 {
            let byte = self.read(8)? as u8;
            if result > (u64::MAX >> 7) {
                return None;
            }
            result = (result << 7) | u64::from(byte & 0x7F);
            if byte & 0x80 == 0 {
                return Some(result);
            }
        }
        None
    }

    /// Ported from `mpc_st_read_golomb`.
    fn read_golomb(&mut self, k: u32) -> Option<i64> {
        let mut prefix: u64 = 0;
        loop {
            let bit = self.read(1)?;
            if bit != 0 {
                break;
            }
            prefix += 1;
            if prefix > (i64::MAX as u64 >> k) {
                return None;
            }
        }
        let suffix = self.read(k)?;
        Some(((prefix << k) | u64::from(suffix)) as i64)
    }
}

/// Ported from `mpc_seek_table_entries`.
fn seek_table_entries(seek_pwr: u32, samples: u64) -> Option<u64> {
    let spacing = (FRAME_LENGTH as u64).checked_shl(seek_pwr)?;
    if spacing == 0 {
        return None;
    }
    Some(2 + samples / spacing)
}

impl SeekTable {
    /// Ported from `mpc_demux_ST`. `payload` is the `ST` block's raw bytes (already extracted,
    /// byte-aligned -- unlike the reference, which reads it from the middle of a continuous
    /// bit-reader with a `+1` byte offset that only matters for that in-place reading style).
    /// `header_position` is `si.header_position` (added to each decoded entry). `block_pwr` is
    /// `si.block_pwr`. `samples` is `si.samples` (raw, pre-`beg_silence`).
    pub fn parse(payload: &[u8], header_position: u64, block_pwr: u8, samples: u64) -> Option<Self> {
        let mut r = StReader::new(payload);

        let file_table_size_full = r.read_size()?;
        if file_table_size_full == 0 || file_table_size_full > u64::from(u32::MAX) {
            return None;
        }
        let mut file_table_size = file_table_size_full;

        let value = r.read(4)?;
        let mut seek_pwr = u32::from(block_pwr) + value;

        let mut diff_pwr: u32 = 0;
        let mut entries = seek_table_entries(seek_pwr, samples)?;
        while entries > MAX_SEEK_TABLE_SIZE {
            if seek_pwr >= 63 || diff_pwr >= 31 {
                return None;
            }
            seek_pwr += 1;
            diff_pwr += 1;
            entries = seek_table_entries(seek_pwr, samples)?;
        }

        if (file_table_size >> diff_pwr) > entries {
            file_table_size = entries.checked_shl(diff_pwr)?;
        }
        let capacity = entries;
        let seek_table_size = (file_table_size + ((1u64 << diff_pwr) - 1)) >> diff_pwr;
        if seek_table_size == 0 || seek_table_size > capacity || seek_table_size > MAX_SEEK_TABLE_SIZE
        {
            return None;
        }

        let mut table = vec![0u64; seek_table_size as usize];

        let tmp = r.read_size()?;
        let mut last0 = tmp.checked_add(header_position)?.checked_mul(8)?;
        table[0] = last0;

        if seek_table_size == 1 {
            return Some(SeekTable { entries: table, seek_pwr });
        }

        let tmp = r.read_size()?;
        let mut last1 = tmp.checked_add(header_position)?.checked_mul(8)?;
        if diff_pwr == 0 {
            table[1] = last1;
        }

        // `last[i & 1]` alternation, matching the reference's 2-slot ring buffer.
        let mask: u64 = (1u64 << diff_pwr) - 1;
        let mut last = [last0, last1];
        for i in 2..file_table_size {
            let code = r.read_golomb(12)?;
            let delta: i64 = if code & 1 != 0 { -(code & !1) } else { code };
            let delta = delta.checked_mul(4)?;

            let prev = last[((i - 1) & 1) as usize];
            let prev2 = last[(i & 1) as usize];
            // value = 2*prev - prev2 + delta, with the same overflow guards as the reference
            // (expressed directly in i128 to avoid replicating the u64-wraparound-detection
            // dance verbatim while still rejecting any actually-overflowing result).
            let value = 2i128 * i128::from(prev) - i128::from(prev2) + i128::from(delta);
            if value < 0 || value > i128::from(u64::MAX) {
                return None;
            }
            let value = value as u64;
            last[(i & 1) as usize] = value;

            if i & mask == 0 {
                let idx = (i >> diff_pwr) as usize;
                if idx >= table.len() {
                    return None;
                }
                table[idx] = value;
            }
        }
        last0 = last[0];
        last1 = last[1];
        let _ = (last0, last1);

        Some(SeekTable { entries: table, seek_pwr })
    }

    /// Defensive validation before trusting a parsed table: entries must be non-empty,
    /// non-decreasing, and end within a plausible bit-position range (checked by the caller
    /// against the actual file size).
    pub fn sanity_check(&self, max_bit_pos: u64) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let mut prev = 0u64;
        for &e in &self.entries {
            if e < prev || e > max_bit_pos {
                return false;
            }
            prev = e;
        }
        true
    }

    /// Frame index (in units of `2^seek_pwr` Musepack frames) that entry `i` corresponds to.
    pub fn spacing_frames(&self) -> u64 {
        1u64 << self.seek_pwr.min(62)
    }
}
