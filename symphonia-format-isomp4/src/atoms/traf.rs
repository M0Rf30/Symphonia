// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::atoms::{
    Atom, AtomHeader, AtomIterator, AtomType, ReadAtom, Result, TfhdAtom, TrunAtom, decode_error,
};

/// Track fragment atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct TrafAtom {
    /// Track fragment header.
    pub tfhd: TfhdAtom,
    /// Track fragment sample runs.
    pub truns: Vec<TrunAtom>,
    /// The total number of samples in this track fragment.
    pub total_sample_count: u32,
}

impl Atom for TrafAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let mut tfhd = None;
        let mut truns = Vec::new();

        let mut total_sample_count = 0u32;

        while let Some(header) = it.next_header()? {
            match header.atom_type {
                AtomType::TrackFragmentHeader => {
                    tfhd = Some(it.read_atom::<TfhdAtom>()?);
                }
                AtomType::TrackFragmentRun => {
                    let trun = it.read_atom::<TrunAtom>()?;

                    // Increment the total sample count.
                    let Some(count) = total_sample_count.checked_add(trun.sample_count)
                    else {
                        return decode_error("isomp4 (traf): total sample count overflow");
                    };
                    total_sample_count = count;

                    truns.push(trun);
                }
                _ => (),
            }
        }

        // Tfhd is mandatory.
        let Some(tfhd) = tfhd
        else {
            return decode_error("isomp4 (traf): missing tfhd atom");
        };

        Ok(TrafAtom { tfhd, truns, total_sample_count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};

    fn atom(tag: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(8 + body.len());
        data.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
        data.extend_from_slice(tag);
        data.extend_from_slice(body);
        data
    }

    fn read_traf(run_sample_counts: &[u32]) -> Result<TrafAtom> {
        let mut tfhd = vec![0; 4]; // Version and flags.
        tfhd.extend_from_slice(&1u32.to_be_bytes()); // Track ID.

        let mut body = atom(b"tfhd", &tfhd);

        for count in run_sample_counts {
            let mut trun = vec![0; 4]; // Version and flags.
            trun.extend_from_slice(&count.to_be_bytes());
            body.extend_from_slice(&atom(b"trun", &trun));
        }

        let data = atom(b"traf", &body);

        let len = data.len() as u64;
        let mss = MediaSourceStream::new(
            Box::new(Cursor::new(data)),
            MediaSourceStreamOptions::default(),
        );
        let mut it = AtomIterator::new(mss, Some(len));
        assert!(it.next_header().map(|h| h.is_some()).unwrap_or(false));
        it.read_atom::<TrafAtom>()
    }

    #[test]
    fn traf_total_sample_count_overflow_is_an_error() {
        assert!(read_traf(&[u32::MAX, 1]).is_err());
    }

    #[test]
    fn traf_total_sample_count_sums_runs() {
        let Ok(traf) = read_traf(&[3, 4])
        else {
            panic!("valid traf");
        };
        assert_eq!(traf.total_sample_count, 7);
    }
}
