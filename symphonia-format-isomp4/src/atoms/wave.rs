// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::atoms::qt_meta::PayloadAtom;
use crate::atoms::stsd::AudioSampleEntry;
use crate::atoms::{
    AlacAtom, Atom, AtomHeader, AtomIterator, AtomType, EsdsAtom, ReadAtom, Result,
};

#[allow(dead_code)]
#[derive(Debug)]
pub struct WaveAtom {
    pub esds: Option<EsdsAtom>,
    pub alac: Option<AlacAtom>,
    /// The `enda` flag: `Some(true)` if PCM samples are little-endian.
    pub little_endian: Option<bool>,
    /// The WAVEFORMATEX of a Microsoft ADPCM or IMA ADPCM (WAV) track, with the codec tag.
    pub wave_format: Option<(AtomType, Box<[u8]>)>,
}

impl Atom for WaveAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let mut esds = None;
        let mut alac = None;
        let mut little_endian = None;
        let mut wave_format = None;

        while let Some(header) = it.next_header()? {
            match header.atom_type {
                AtomType::Esds => esds = Some(it.read_atom::<EsdsAtom>()?),
                // QuickTime stores the ALAC magic cookie in an `alac` atom nested within `wave`
                // (alongside the `frma` format atom) rather than directly within the sample entry.
                AtomType::AudioSampleEntryAlac => alac = Some(it.read_atom::<AlacAtom>()?),
                AtomType::Enda => {
                    little_endian =
                        Some(it.read_atom::<PayloadAtom>()?.data.iter().any(|&b| b != 0));
                }
                atom_type @ (AtomType::AudioSampleEntryMsAdpcm
                | AtomType::AudioSampleEntryImaWav) => {
                    wave_format = Some((atom_type, it.read_atom::<PayloadAtom>()?.data));
                }
                _ => (),
            }
        }

        Ok(WaveAtom { esds, alac, little_endian, wave_format })
    }
}

impl WaveAtom {
    pub fn fill_audio_sample_entry(self, entry: &mut AudioSampleEntry) -> Result<()> {
        if let Some(esds) = self.esds {
            esds.fill_audio_sample_entry(entry)?;
        }

        if let Some(alac) = self.alac {
            alac.fill_audio_sample_entry(entry);
        }

        if self.little_endian.is_some() {
            entry.little_endian = self.little_endian;
        }

        if let Some((atom_type, format)) = self.wave_format {
            entry.fill_from_waveformatex(atom_type, &format);
        }

        Ok(())
    }
}
