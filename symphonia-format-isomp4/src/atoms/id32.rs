// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use log::warn;
use symphonia_core::io::BufReader;
use symphonia_core::meta::MetadataRevision;
use symphonia_metadata::embedded::riff::read_riff_id3_chunk;

use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result};

/// ID3v2 metadata box (ISO/IEC 14496-12, `ID32`).
///
/// A full box, followed by a packed ISO-639-2 language code (with a leading pad bit), followed by
/// a complete ID3v2 tag.
#[derive(Debug)]
pub struct Id32Atom {
    /// The metadata read from the ID3v2 tag, if it was valid.
    pub metadata: Option<MetadataRevision>,
}

impl Atom for Id32Atom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        let (_, _) = it.read_extended_header()?;

        // Version and flags (4 bytes) were consumed above. Subtract them and the language code.
        let data_len = header.data_size().unwrap_or(0).saturating_sub(4);
        let tag_len = data_len.saturating_sub(2);

        // Skip the pad bit and language code.
        let _ = it.read_u16()?;

        let buf = it.read_boxed_slice_exact(tag_len as usize)?;

        let mut side_data = Vec::new();

        let metadata = match read_riff_id3_chunk(&mut BufReader::new(&buf), &mut side_data) {
            Ok(rev) => Some(rev),
            Err(err) => {
                warn!("isomp4: failed to read ID32 metadata: {err}");
                None
            }
        };

        Ok(Id32Atom { metadata })
    }
}
