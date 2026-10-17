// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::codecs::audio::well_known::CODEC_ID_AC4;

use crate::atoms::stsd::AudioSampleEntry;
use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result, read_config_extra_data};

/// AC4SpecificBox: the `ac4_dsi_v1` of ETSI TS 103 190-2 Annex E.
#[derive(Debug)]
pub struct Dac4Atom {
    /// The `ac4_dsi_v1` bytes.
    extra_data: Box<[u8]>,
}

impl Atom for Dac4Atom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        // The fields up to and including `n_presentations` are 3 bytes long.
        const MIN_DAC4_ATOM_SIZE: u64 = 3;
        const MAX_DAC4_ATOM_SIZE: u64 = 64 * 1024;

        let extra_data =
            read_config_extra_data(it, header, MIN_DAC4_ATOM_SIZE, MAX_DAC4_ATOM_SIZE)?;

        Ok(Dac4Atom { extra_data })
    }
}

impl Dac4Atom {
    /// Parses the sample rate (`fs_index`) of the AC4SpecificBox.
    ///
    /// The bit layout of `ac4_dsi_v1` starts with `ac4_dsi_version` (3 bits), `bitstream_version`
    /// (7 bits), `fs_index` (1 bit), `frame_rate_index` (4 bits), and `n_presentations` (9 bits).
    /// `fs_index` 0 is 44.1 kHz and 1 is 48 kHz.
    fn parse_sample_rate(&self) -> Option<u32> {
        let &[b0, b1, ..] = &*self.extra_data
        else {
            return None;
        };

        // Only versions 0 and 1 of the DSI are defined.
        if b0 >> 5 > 1 {
            return None;
        }

        Some(if (b1 >> 5) & 0x1 != 0 { 48000 } else { 44100 })
    }

    pub fn fill_audio_sample_entry(self, entry: &mut AudioSampleEntry) {
        entry.codec_id = CODEC_ID_AC4;

        if let Some(sample_rate) = self.parse_sample_rate() {
            entry.sample_rate = f64::from(sample_rate);
        }

        entry.extra_data = Some(self.extra_data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_48k_dsi() {
        // The start of the `dac4` of a stereo, 48 kHz, 2048 sample frame stream: DSI version 1,
        // bitstream version 2, fs_index 1, frame_rate_index 13, 2 presentations.
        let atom = Dac4Atom { extra_data: Box::new([0x20, 0xba, 0x02, 0x40, 0x00]) };
        assert_eq!(atom.parse_sample_rate(), Some(48000));
    }

    #[test]
    fn parses_44k1_dsi() {
        // DSI version 1, bitstream version 2, fs_index 0, frame_rate_index 13.
        let atom = Dac4Atom { extra_data: Box::new([0x20, 0x9a, 0x02, 0x40]) };
        assert_eq!(atom.parse_sample_rate(), Some(44100));
    }

    #[test]
    fn rejects_unknown_dsi_version() {
        let atom = Dac4Atom { extra_data: Box::new([0x40, 0xba, 0x02]) };
        assert!(atom.parse_sample_rate().is_none());
    }
}
