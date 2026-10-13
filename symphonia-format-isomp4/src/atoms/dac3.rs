// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::codecs::audio::well_known::CODEC_ID_AC3;

use crate::atoms::stsd::AudioSampleEntry;
use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result, read_config_extra_data};

#[derive(Debug)]
pub struct Dac3Atom {
    /// AC3SpecificBox
    extra_data: Box<[u8]>,
}

impl Atom for Dac3Atom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        // AC3SpecificBox is 3 bytes long.
        const MIN_DAC3_ATOM_SIZE: u64 = 3;
        const MAX_DAC3_ATOM_SIZE: u64 = 4 * 1024;

        let extra_data =
            read_config_extra_data(it, header, MIN_DAC3_ATOM_SIZE, MAX_DAC3_ATOM_SIZE)?;

        Ok(Dac3Atom { extra_data })
    }
}

impl Dac3Atom {
    pub fn fill_audio_sample_entry(self, entry: &mut AudioSampleEntry) {
        entry.codec_id = CODEC_ID_AC3;
        entry.extra_data = Some(self.extra_data);
    }
}
