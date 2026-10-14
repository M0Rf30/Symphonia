// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt::Debug;

use symphonia_core::meta::MetadataRevision;

use crate::atoms::{
    Atom, AtomHeader, AtomIterator, AtomType, GaplessInfo, Id32Atom, IlstAtom, KeysAtom, ReadAtom,
    Result,
};

/// User data atom.
#[allow(dead_code)]
pub struct MetaAtom {
    /// Metadata revision.
    pub metadata: Option<MetadataRevision>,
    /// ID3v2 metadata revision from an `ID32` box, if present.
    pub id3v2: Option<MetadataRevision>,
    /// Gapless playback information, if present.
    pub gapless: Option<GaplessInfo>,
}

impl Debug for MetaAtom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "(redacted)")
    }
}

impl MetaAtom {
    /// If ID3v2 metadata (`ID32` box) was read, consumes the metadata and returns it.
    pub fn take_id3v2(&mut self) -> Option<MetadataRevision> {
        self.id3v2.take()
    }

    /// If metadata was read, consumes the metadata and returns it.
    pub fn take_metadata(&mut self) -> Option<MetadataRevision> {
        self.metadata.take()
    }
}

impl Atom for MetaAtom {
    #[allow(clippy::single_match)]
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        it.read_meta_header()?;

        let mut metadata = None;
        let mut gapless = None;
        let mut id3v2 = None;
        let mut keys = None;
        let mut ilst = None;

        while let Some(header) = it.next_header()? {
            match header.atom_type {
                AtomType::MetaList => {
                    ilst = Some(it.read_atom::<IlstAtom>()?);
                }
                AtomType::MetadataKeys => {
                    keys = Some(it.read_atom::<KeysAtom>()?);
                }
                AtomType::Id3v2Metadata => {
                    id3v2 = it.read_atom::<Id32Atom>()?.metadata;
                }
                // TODO: Support country and language lists.
                _ => (),
            }
        }

        // The keys list may follow the items, so the items are resolved once all atoms are read.
        if let Some(mut ilst) = ilst {
            ilst.add_unmapped_tags(keys.as_ref());
            gapless = ilst.gapless;
            metadata = Some(ilst.metadata);
        }

        Ok(MetaAtom { metadata, id3v2, gapless })
    }
}
