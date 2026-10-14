// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::atoms::{Atom, AtomHeader, AtomIterator, AtomType, ReadAtom, Result};

/// Track reference atom (`tref`).
#[derive(Debug, Default)]
pub struct TrefAtom {
    /// The IDs of the tracks referenced as chapter tracks (`chap` reference type).
    pub chapter_tracks: Vec<u32>,
}

/// A track reference type atom: an array of track IDs.
struct TrackIdsAtom(Vec<u32>);

impl Atom for TrackIdsAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        let count = header.data_size().unwrap_or(0) / 4;

        let mut ids = Vec::new();

        for _ in 0..count.min(256) {
            ids.push(it.read_u32()?);
        }

        Ok(TrackIdsAtom(ids))
    }
}

impl Atom for TrefAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let mut chapter_tracks = Vec::new();

        while let Some(header) = it.next_header()? {
            if header.atom_type == AtomType::ChapterReference {
                chapter_tracks = it.read_atom::<TrackIdsAtom>()?.0;
            }
        }

        Ok(TrefAtom { chapter_tracks })
    }
}
