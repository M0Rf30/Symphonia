// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::meta::{MetadataContainer, PerTrackMetadata};

use crate::atoms::{
    Atom, AtomHeader, AtomIterator, AtomType, EdtsAtom, MdiaAtom, MetaAtom, ReadAtom, Result,
    TkhdAtom, TrefAtom, UdtaAtom, decode_error,
};

/// Track atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct TrakAtom {
    /// Track header atom.
    pub tkhd: TkhdAtom,
    /// Optional, edit list atom.
    pub edts: Option<EdtsAtom>,
    /// Optional, track reference atom.
    pub tref: Option<TrefAtom>,
    /// Media atom.
    pub mdia: MdiaAtom,
    /// Optional, track-level user data atom.
    pub udta: Option<UdtaAtom>,
    /// Optional, track-level metadata atom.
    pub meta: Option<MetaAtom>,
}

impl TrakAtom {
    /// If track-level metadata was read, consumes the metadata and returns it.
    pub fn take_metadata(&mut self) -> Option<PerTrackMetadata> {
        let mut container = MetadataContainer::default();

        let revs = [
            self.meta.as_mut().and_then(|meta| meta.take_metadata()),
            self.udta.as_mut().and_then(|udta| udta.take_metadata()),
        ];

        for rev in revs.into_iter().flatten() {
            container.tags.extend(rev.media.tags);
            container.visuals.extend(rev.media.visuals);
        }

        if container.tags.is_empty() && container.visuals.is_empty() {
            return None;
        }

        Some(PerTrackMetadata { track_id: u64::from(self.tkhd.id), metadata: container })
    }
}

impl Atom for TrakAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let mut tkhd = None;
        let mut edts = None;
        let mut tref = None;
        let mut udta = None;
        let mut meta = None;
        let mut mdia = None;

        while let Some(header) = it.next_header()? {
            match header.atom_type {
                AtomType::TrackHeader => {
                    tkhd = Some(it.read_atom::<TkhdAtom>()?);
                }
                AtomType::Edit => {
                    edts = Some(it.read_atom::<EdtsAtom>()?);
                }
                AtomType::TrackReference => {
                    tref = Some(it.read_atom::<TrefAtom>()?);
                }
                AtomType::UserData => {
                    udta = Some(it.read_atom::<UdtaAtom>()?);
                }
                AtomType::Meta => {
                    meta = Some(it.read_atom::<MetaAtom>()?);
                }
                AtomType::Media => {
                    mdia = Some(it.read_atom::<MdiaAtom>()?);
                }
                _ => (),
            }
        }

        let Some(tkhd) = tkhd
        else {
            return decode_error("isomp4 (trak): missing tkhd atom");
        };

        let Some(mdia) = mdia
        else {
            return decode_error("isomp4 (trak): missing mdia atom");
        };

        Ok(TrakAtom { tkhd, edts, tref, mdia, udta, meta })
    }
}
