// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_common::mpeg::surround::Ac3Header;
use symphonia_core::audio::Channels;
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

/// The sample rates by `fscod`.
pub(crate) const AC3_SAMPLE_RATES: [u32; 3] = [48000, 44100, 32000];

/// Gets the channels of an audio coding mode and LFE flag.
pub(crate) fn ac3_channels(acmod: u8, lfe: bool) -> Channels {
    Ac3Header {
        eac3: false,
        bsid: 8,
        frame_size: 0,
        sample_rate: 0,
        acmod,
        lfe,
        samples: 0,
        dependent: false,
        substream_id: 0,
        swapped: false,
    }
    .channels()
}

impl Dac3Atom {
    /// Parses the sample rate (`fscod`) and the channels (`acmod`, `lfeon`) of the AC3SpecificBox
    /// (ETSI TS 102 366 F.4).
    fn parse(&self) -> Option<(u32, Channels)> {
        let &[b0, b1, ..] = &*self.extra_data
        else {
            return None;
        };

        let sample_rate = *AC3_SAMPLE_RATES.get(usize::from(b0 >> 6))?;
        let acmod = (b1 >> 3) & 0x7;
        let lfe = (b1 >> 2) & 0x1 != 0;

        Some((sample_rate, ac3_channels(acmod, lfe)))
    }

    pub fn fill_audio_sample_entry(self, entry: &mut AudioSampleEntry) {
        entry.codec_id = CODEC_ID_AC3;

        if let Some((sample_rate, channels)) = self.parse() {
            entry.sample_rate = f64::from(sample_rate);
            entry.channels = Some(channels);
        }

        entry.extra_data = Some(self.extra_data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_core::audio::Position;

    #[test]
    fn parses_5_1_48k() {
        // fscod 0, bsid 8, bsmod 0, acmod 7, lfeon 1, bit_rate_code 14.
        let atom = Dac3Atom { extra_data: Box::new([0x10, 0x3d, 0x80]) };
        let (rate, channels) = atom.parse().unwrap();
        assert_eq!(rate, 48000);
        assert_eq!(channels.count(), 6);
        let Channels::Positioned(pos) = channels
        else {
            panic!("expected positioned channels");
        };
        assert!(pos.contains(Position::LFE1));
        assert!(pos.contains(Position::SIDE_LEFT));
    }

    #[test]
    fn rejects_reserved_fscod() {
        let atom = Dac3Atom { extra_data: Box::new([0xd0, 0x08, 0x00]) };
        assert!(atom.parse().is_none());
    }
}
