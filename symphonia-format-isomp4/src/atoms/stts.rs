// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result, decode_error};

#[derive(Debug)]
pub struct SampleDurationEntry {
    pub sample_count: u32,
    pub sample_delta: u32,
    /// The number of the first sample of the entry, relative to the `SttsAtom`.
    first_sample: u32,
    /// The timestamp of the first sample of the entry.
    first_ts: u64,
}

impl SampleDurationEntry {
    /// The exclusive end sample number of the entry. Cannot overflow because the sample counts of
    /// the table are validated when the atom is read.
    fn end_sample(&self) -> u32 {
        self.first_sample + self.sample_count
    }

    /// The exclusive end timestamp of the entry. Cannot overflow because the total duration of the
    /// table is validated when the atom is read.
    fn end_ts(&self) -> u64 {
        self.first_ts + u64::from(self.sample_count) * u64::from(self.sample_delta)
    }
}

/// Time-to-sample atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct SttsAtom {
    pub entries: Vec<SampleDurationEntry>,
    pub total_duration: u64,
}

impl SttsAtom {
    /// Get the timestamp and duration for the sample indicated by `sample_num`. Note, `sample_num`
    /// is indexed relative to the `SttsAtom`. Complexity of this function in O(log2 N).
    pub fn find_timing_for_sample(&self, sample_num: u32) -> Option<(u64, u32)> {
        // The Stts atom compactly encodes a mapping between number of samples and sample duration.
        // Find the entry containing the sample. The timestamp is then the first timestamp of the
        // entry plus the product of the number of preceding samples in the entry and the sample
        // duration.
        let idx = self.entries.partition_point(|entry| entry.end_sample() <= sample_num);
        let entry = self.entries.get(idx)?;

        let entry_sample_offset = sample_num - entry.first_sample;
        let ts = entry.first_ts + u64::from(entry.sample_delta) * u64::from(entry_sample_offset);

        Some((ts, entry.sample_delta))
    }

    /// Get the sample that contains the timestamp indicated by `ts`. Note, the returned `sample_num`
    /// is indexed relative to the `SttsAtom`. Complexity of this function in O(log2 N).
    pub fn find_sample_for_timestamp(&self, ts: u64) -> Option<u32> {
        let idx = self.entries.partition_point(|entry| entry.end_ts() <= ts);
        let entry = self.entries.get(idx)?;

        // The entry ends after `ts`, and starts at or before `ts`, thus the delta is non-zero.
        let entry_sample_offset = (ts - entry.first_ts) / u64::from(entry.sample_delta);

        Some(entry.first_sample + entry_sample_offset as u32)
    }
}

impl Atom for SttsAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let (_, _) = it.read_extended_header()?;

        let mut total_sample_count: u32 = 0;
        let mut total_duration: u64 = 0;

        let entries = it.read_table(|it| {
            let sample_count = it.read_u32()?;
            let sample_delta = it.read_u32()?;

            let first_sample = total_sample_count;
            let first_ts = total_duration;

            let Some(next_sample_count) = total_sample_count.checked_add(sample_count)
            else {
                return decode_error("isomp4 (stts): total sample count overflow");
            };
            total_sample_count = next_sample_count;

            let Some(next_duration) =
                total_duration.checked_add(u64::from(sample_count) * u64::from(sample_delta))
            else {
                return decode_error("isomp4 (stts): total duration overflow");
            };
            total_duration = next_duration;

            Ok(SampleDurationEntry { sample_count, sample_delta, first_sample, first_ts })
        })?;

        Ok(SttsAtom { entries, total_duration })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};

    fn read_stts(entries: &[(u32, u32)]) -> Result<SttsAtom> {
        let mut body = vec![0, 0, 0, 0]; // Version and flags.
        body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (count, delta) in entries {
            body.extend_from_slice(&count.to_be_bytes());
            body.extend_from_slice(&delta.to_be_bytes());
        }

        let mut data = Vec::with_capacity(8 + body.len());
        data.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
        data.extend_from_slice(b"stts");
        data.extend_from_slice(&body);

        let len = data.len() as u64;
        let mss = MediaSourceStream::new(
            Box::new(Cursor::new(data)),
            MediaSourceStreamOptions::default(),
        );
        let mut it = AtomIterator::new(mss, Some(len));
        assert!(it.next_header().map(|h| h.is_some()).unwrap_or(false));
        it.read_atom::<SttsAtom>()
    }

    #[test]
    fn stts_total_sample_count_overflow_is_an_error() {
        assert!(read_stts(&[(u32::MAX, 0), (2, 0)]).is_err());
    }

    #[test]
    fn stts_lookup_with_sample_counts_near_u32_max() {
        // A single entry covering every sample number, and a pair of entries whose counts sum to
        // exactly u32::MAX, must be searchable without overflowing.
        let Ok(stts) = read_stts(&[(u32::MAX, 1024)])
        else {
            panic!("valid stts");
        };
        assert_eq!(stts.find_timing_for_sample(1), Some((1024, 1024)));
        assert_eq!(
            stts.find_timing_for_sample(u32::MAX - 1),
            Some((u64::from(u32::MAX - 1) * 1024, 1024))
        );
        assert_eq!(stts.find_timing_for_sample(u32::MAX), None);

        let Ok(stts) = read_stts(&[(1, 1024), (u32::MAX - 1, 1024)])
        else {
            panic!("valid stts");
        };
        assert_eq!(stts.find_timing_for_sample(1), Some((1024, 1024)));
        assert_eq!(stts.find_timing_for_sample(u32::MAX), None);

        // Counts that sum to more than u32::MAX are rejected when the atom is read.
        assert!(read_stts(&[(1, 1024), (u32::MAX, 1024)]).is_err());
    }

    #[test]
    fn stts_lookups_across_entries() {
        let Ok(stts) = read_stts(&[(2, 10), (0, 99), (3, 5)])
        else {
            panic!("valid stts");
        };

        assert_eq!(stts.find_timing_for_sample(0), Some((0, 10)));
        assert_eq!(stts.find_timing_for_sample(1), Some((10, 10)));
        assert_eq!(stts.find_timing_for_sample(2), Some((20, 5)));
        assert_eq!(stts.find_timing_for_sample(4), Some((30, 5)));
        assert_eq!(stts.find_timing_for_sample(5), None);

        assert_eq!(stts.find_sample_for_timestamp(0), Some(0));
        assert_eq!(stts.find_sample_for_timestamp(19), Some(1));
        assert_eq!(stts.find_sample_for_timestamp(20), Some(2));
        assert_eq!(stts.find_sample_for_timestamp(34), Some(4));
        assert_eq!(stts.find_sample_for_timestamp(35), None);
    }
}
