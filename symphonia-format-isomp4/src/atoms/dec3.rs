// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::well_known::CODEC_ID_EAC3;

use crate::atoms::dac3::{AC3_SAMPLE_RATES, ac3_channels};
use crate::atoms::stsd::AudioSampleEntry;
use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result, read_config_extra_data};

#[derive(Debug)]
pub struct Dec3Atom {
    /// EAC3SpecificBox
    extra_data: Box<[u8]>,
}

impl Atom for Dec3Atom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        // EC3SpecificBox is at least 5 bytes long: a 2 byte header and one 3 byte independent
        // substream entry.
        const MIN_DEC3_ATOM_SIZE: u64 = 5;
        const MAX_DEC3_ATOM_SIZE: u64 = 4 * 1024;

        let extra_data =
            read_config_extra_data(it, header, MIN_DEC3_ATOM_SIZE, MAX_DEC3_ATOM_SIZE)?;

        Ok(Dec3Atom { extra_data })
    }
}

/// The positions of the bits of `chan_loc` (the dependent substream channel locations), from the
/// most significant bit (bit 8) to the least significant bit (bit 0). `None` is for the surround
/// direct channels, which have no position.
const CHAN_LOC: [Option<Position>; 9] = [
    Some(Position::FRONT_LEFT_CENTER.union(Position::FRONT_RIGHT_CENTER)),
    Some(Position::REAR_LEFT.union(Position::REAR_RIGHT)),
    Some(Position::REAR_CENTER),
    Some(Position::TOP_CENTER),
    None,
    Some(Position::FRONT_LEFT_WIDE.union(Position::FRONT_RIGHT_WIDE)),
    Some(Position::TOP_FRONT_LEFT.union(Position::TOP_FRONT_RIGHT)),
    Some(Position::TOP_FRONT_CENTER),
    Some(Position::LFE2),
];

impl Dec3Atom {
    /// Parses the sample rate (`fscod`) and the channels of the first independent substream and
    /// its dependent substreams of the EC3SpecificBox (ETSI TS 102 366 F.6).
    fn parse(&self) -> Option<(u32, Channels)> {
        let &[_, _, b0, b1, b2, ref rest @ ..] = &*self.extra_data
        else {
            return None;
        };

        let sample_rate = *AC3_SAMPLE_RATES.get(usize::from(b0 >> 6))?;
        let acmod = (b1 >> 1) & 0x7;
        let lfe = b1 & 0x1 != 0;
        let num_dep_sub = (b2 >> 1) & 0xf;

        let mut channels = ac3_channels(acmod, lfe);

        if num_dep_sub > 0 {
            let chan_loc = u16::from(b2 & 1) << 8 | u16::from(*rest.first()?);

            // Add the channel locations of the dependent substreams.
            let mut count = channels.count();
            let mut positions = match channels {
                Channels::Positioned(pos) => pos,
                _ => Position::empty(),
            };
            let mut positioned = true;

            for (i, loc) in CHAN_LOC.iter().enumerate() {
                if chan_loc & (0x100 >> i) == 0 {
                    continue;
                }
                match loc {
                    Some(pos) if !positions.intersects(*pos) => {
                        positions |= *pos;
                        count += pos.bits().count_ones() as usize;
                    }
                    Some(pos) => {
                        positioned = false;
                        count += pos.bits().count_ones() as usize;
                    }
                    None => {
                        positioned = false;
                        count += 2;
                    }
                }
            }

            channels = if positioned {
                Channels::Positioned(positions)
            }
            else {
                Channels::Discrete(count as u16)
            };
        }

        Some((sample_rate, channels))
    }

    pub fn fill_audio_sample_entry(self, entry: &mut AudioSampleEntry) {
        entry.codec_id = CODEC_ID_EAC3;

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

    #[test]
    fn parses_5_1_48k() {
        // data_rate 640, 1 independent substream: fscod 0, bsid 16, acmod 7, lfeon 1, no
        // dependent substreams.
        let atom = Dec3Atom { extra_data: Box::new([0x14, 0x00, 0x20, 0x0f, 0x00]) };
        let (rate, channels) = atom.parse().unwrap();
        assert_eq!(rate, 48000);
        assert_eq!(channels.count(), 6);
    }

    #[test]
    fn adds_dependent_substream_channels() {
        // As above, with one dependent substream carrying Lrs/Rrs (chan_loc bit 7) for 7.1.
        let atom = Dec3Atom { extra_data: Box::new([0x14, 0x20, 0x20, 0x0f, 0x02, 0x80]) };
        let (rate, channels) = atom.parse().unwrap();
        assert_eq!(rate, 48000);
        assert_eq!(channels.count(), 8);
    }
}
