// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::num::NonZero;

use symphonia_core::errors::Error;

use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result, decode_error};

#[derive(Debug)]
pub enum ReferenceType {
    Segment,
    Media,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct SidxReference {
    pub reference_type: ReferenceType,
    pub reference_size: u32,
    pub subsegment_duration: u32,
    // pub starts_with_sap: bool,
    // pub sap_type: u8,
    // pub sap_delta_time: u32,
}

/// Segment index atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct SidxAtom {
    pub reference_id: u32,
    pub timescale: NonZero<u32>,
    pub earliest_pts: u64,
    pub first_offset: u64,
    pub references: Vec<SidxReference>,
    /// The duration of the media subsegments this index references directly. References to other
    /// segment indexes are excluded: their duration is that of the referenced index's
    /// subsegments (ISO/IEC 14496-12, 8.16.3), so summing every index counts them only once.
    pub media_duration: u64,
}

impl Atom for SidxAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        let (version, _) = it.read_extended_header()?;

        let reference_id = it.read_u32()?;
        let timescale = NonZero::new(it.read_u32()?)
            .ok_or(Error::DecodeError("isomp4 (sidx): timescale is zero"))?;

        // The anchor point for segment offsets is the first byte after this atom.
        let anchor = header
            .size()
            .and_then(|atom_len| header.pos().checked_add(atom_len.get()))
            .ok_or(Error::DecodeError("isomp4 (sidx): expected valid atom position and size"))?;

        let (earliest_pts, first_offset) = match version {
            0 => (u64::from(it.read_u32()?), anchor.checked_add(u64::from(it.read_u32()?))),
            1 => (it.read_u64()?, anchor.checked_add(it.read_u64()?)),
            _ => return decode_error("isomp4 (sidx): invalid version"),
        };

        let first_offset =
            first_offset.ok_or(Error::DecodeError("isomp4 (sidx): first offset overflow"))?;

        let _reserved = it.read_u16()?;
        let reference_count = it.read_u16()?;

        let mut references = Vec::new();
        let mut media_duration = 0u64;

        for _ in 0..reference_count {
            let reference = it.read_u32()?;
            let subsegment_duration = it.read_u32()?;

            let reference_type = match (reference & 0x8000_0000) != 0 {
                false => ReferenceType::Media,
                true => ReferenceType::Segment,
            };

            let reference_size = reference & !0x8000_0000;

            // Ignore SAP
            let _ = it.read_u32()?;
            // At most 65535 32-bit durations: the sum cannot overflow.
            if matches!(reference_type, ReferenceType::Media) {
                media_duration += u64::from(subsegment_duration);
            }

            references.push(SidxReference { reference_type, reference_size, subsegment_duration });
        }

        Ok(SidxAtom {
            reference_id,
            timescale,
            earliest_pts,
            first_offset,
            references,
            media_duration,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};

    /// Reads a version 1 `sidx` with `(is_segment_index, duration)` references.
    fn read_sidx_v1(first_offset: u64, refs: &[(bool, u32)]) -> Result<SidxAtom> {
        let mut body = vec![1, 0, 0, 0]; // Version and flags.
        body.extend_from_slice(&1u32.to_be_bytes()); // Reference ID.
        body.extend_from_slice(&1000u32.to_be_bytes()); // Timescale.
        body.extend_from_slice(&0u64.to_be_bytes()); // Earliest presentation time.
        body.extend_from_slice(&first_offset.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes()); // Reserved.
        body.extend_from_slice(&(refs.len() as u16).to_be_bytes()); // Reference count.
        for &(is_index, duration) in refs {
            body.extend_from_slice(&(u32::from(is_index) << 31 | 100).to_be_bytes());
            body.extend_from_slice(&duration.to_be_bytes());
            body.extend_from_slice(&0u32.to_be_bytes()); // SAP.
        }

        let mut data = Vec::with_capacity(8 + body.len());
        data.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
        data.extend_from_slice(b"sidx");
        data.extend_from_slice(&body);

        let len = data.len() as u64;
        let mss = MediaSourceStream::new(
            Box::new(Cursor::new(data)),
            MediaSourceStreamOptions::default(),
        );
        let mut it = AtomIterator::new(mss, Some(len));
        assert!(it.next_header().map(|h| h.is_some()).unwrap_or(false));
        it.read_atom::<SidxAtom>()
    }

    #[test]
    fn sidx_first_offset_overflow_is_an_error() {
        assert!(read_sidx_v1(u64::MAX, &[]).is_err());
    }

    #[test]
    fn sidx_first_offset_is_relative_to_the_end_of_the_atom() {
        let Ok(sidx) = read_sidx_v1(16, &[])
        else {
            panic!("valid sidx");
        };
        assert_eq!(sidx.first_offset, 40 + 16);
    }

    /// A daisy-chained index: two media subsegments, then a reference to the next index whose
    /// duration covers the rest of the stream. Only the media subsegments count.
    #[test]
    fn sidx_media_duration_excludes_index_references() {
        let Ok(sidx) = read_sidx_v1(0, &[(false, 1000), (false, 2000), (true, 9000)])
        else {
            panic!("valid sidx");
        };
        assert_eq!(sidx.references.len(), 3);
        assert_eq!(sidx.media_duration, 3000);
    }
}
