// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::atoms::stsd::AudioSampleEntry;
use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result, read_config_extra_data};

/// The size of the payload of the AMRSpecificBox.
const DAMR_ATOM_SIZE: u64 = 9;

/// The `damr` (AMRSpecificBox) atom of the `samr` (AMR-NB) and `sawb` (AMR-WB) sample entries,
/// 3GPP TS 26.244 (AMRSpecificBox):
///
/// ```text
/// vendor:               4 bytes
/// decoder_version:      1 byte
/// mode_set:             2 bytes, big-endian, bit n set if frame type n may be used
/// mode_change_period:   1 byte
/// frames_per_sample:    1 byte, number of frames in one sample
/// ```
#[derive(Debug)]
pub struct DamrAtom {
    /// The payload of the atom.
    extra_data: Box<[u8]>,
}

impl Atom for DamrAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        // Some encoders append fields after the box that the specification defines.
        let extra_data =
            read_config_extra_data(it, header, DAMR_ATOM_SIZE, DAMR_ATOM_SIZE + 4 * 1024)?;

        Ok(DamrAtom { extra_data })
    }
}

impl DamrAtom {
    /// The number of frames of a sample, 1 if it is not given.
    pub fn frames_per_sample(&self) -> u64 {
        // The field is known to be present by the minimum size of the atom.
        self.extra_data.get(8).map_or(1, |&n| u64::from(n.max(1)))
    }

    /// Stores the payload of the atom, which is the decoder specific information (vendor,
    /// version, mode set, mode change period, and frames per sample), as the extra data.
    pub fn fill_audio_sample_entry(self, entry: &mut AudioSampleEntry) {
        entry.extra_data = Some(self.extra_data);
    }
}
