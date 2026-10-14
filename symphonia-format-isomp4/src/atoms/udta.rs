// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::meta::{MetadataBuilder, MetadataRevision, Tag};

use crate::atoms::ilst::ISOMP4_METADATA_INFO;
use crate::atoms::qt_meta::{
    PayloadAtom, atom_fourcc, is_3gpp_text_box, parse_3gpp_box, parse_qt_udta_text,
};

use crate::atoms::{
    Atom, AtomHeader, AtomIterator, AtomType, ChplAtom, GaplessInfo, Id32Atom, MetaAtom, ReadAtom,
    Result,
};

/// User data atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct UdtaAtom {
    /// Metadata atom.
    pub meta: Option<MetaAtom>,
    /// Nero chapter list atom.
    pub chpl: Option<ChplAtom>,
    /// ID3v2 metadata revision from an `ID32` box directly in the user data atom, if present.
    pub id3v2: Option<MetadataRevision>,
    /// Tags from QuickTime user data text atoms (`\u{a9}nam`, ...) and 3GPP asset metadata boxes
    /// (`titl`, ...).
    pub tags: Vec<Tag>,
}

impl UdtaAtom {
    /// If metadata was read, consumes the metadata and returns it.
    pub fn take_metadata(&mut self) -> Option<MetadataRevision> {
        let mut rev = self.meta.as_mut().and_then(|meta| meta.take_metadata());

        // Append the tags of the user data atoms to the metadata of the `ilst` atom, or create
        // new metadata if there is none.
        if !self.tags.is_empty() {
            let tags = std::mem::take(&mut self.tags);

            match rev.as_mut() {
                Some(rev) => rev.media.tags.extend(tags),
                None => {
                    let mut mb = MetadataBuilder::new(ISOMP4_METADATA_INFO);
                    tags.into_iter().for_each(|tag| {
                        mb.add_tag(tag);
                    });
                    rev = Some(mb.build());
                }
            }
        }

        rev
    }

    /// If ID3v2 metadata (`ID32` box) was read, consumes the metadata and returns it.
    pub fn take_id3v2(&mut self) -> Option<MetadataRevision> {
        self.id3v2.take().or_else(|| self.meta.as_mut().and_then(|meta| meta.take_id3v2()))
    }

    /// Get the gapless playback information, if present.
    pub fn gapless(&self) -> Option<GaplessInfo> {
        self.meta.as_ref().and_then(|meta| meta.gapless)
    }
}

impl Atom for UdtaAtom {
    #[allow(clippy::single_match)]
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let mut meta = None;
        let mut chpl = None;
        let mut id3v2 = None;
        let mut tags = Vec::new();

        while let Some(header) = it.next_header()? {
            match header.atom_type {
                AtomType::Meta => {
                    meta = Some(it.read_atom::<MetaAtom>()?);
                }
                AtomType::ChapterList => {
                    chpl = Some(it.read_atom::<ChplAtom>()?);
                }
                AtomType::MetaTagName => {
                    // The QuickTime track name (in a track's user data): plain text.
                    let payload = it.read_atom::<PayloadAtom>()?;
                    let text =
                        std::sync::Arc::new(crate::atoms::qt_meta::decode_text(&payload.data));

                    if !text.is_empty() {
                        let raw = symphonia_core::meta::RawTag::new("name", text.clone());
                        tags.push(symphonia_core::meta::Tag::new_std(
                            raw,
                            symphonia_core::meta::StandardTag::TrackTitle(text),
                        ));
                    }
                }
                AtomType::Id3v2Metadata => {
                    id3v2 = it.read_atom::<Id32Atom>()?.metadata;
                }
                atom_type => {
                    // QuickTime user data text atoms, and 3GPP asset metadata boxes.
                    if let Some(fourcc) = atom_fourcc(atom_type) {
                        if fourcc[0] == 0xa9 {
                            let payload = it.read_atom::<PayloadAtom>()?;
                            let key = crate::atoms::qt_meta::fourcc_to_string(fourcc);
                            tags.extend(parse_qt_udta_text(&key, &payload.data));
                        }
                        else if is_3gpp_text_box(fourcc) {
                            let payload = it.read_atom::<PayloadAtom>()?;
                            parse_3gpp_box(fourcc, &payload.data, &mut tags);
                        }
                    }
                }
            }
        }

        Ok(UdtaAtom { meta, chpl, id3v2, tags })
    }
}
