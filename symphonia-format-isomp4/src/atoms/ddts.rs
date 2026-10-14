// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::well_known::CODEC_ID_DCA;

use crate::atoms::stsd::AudioSampleEntry;
use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result, read_config_extra_data};

/// The size of the DTSSpecificBox payload (ETSI TS 102 114 E.2.2.3).
const DDTS_SIZE: u64 = 20;

/// DTS specific box (`ddts`).
#[derive(Debug)]
pub struct DdtsAtom {
    /// The sampling frequency in Hz.
    sample_rate: u32,
    /// The number of PCM samples per channel in a frame of the core, if known.
    frame_size: Option<u64>,
    /// The bits per sample of the PCM data the stream was encoded from.
    pcm_sample_depth: u8,
    /// The channel layout bit mask.
    channel_layout: u16,
    /// The payload of the box.
    extra_data: Box<[u8]>,
}

impl Atom for DdtsAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        const MAX_DDTS_ATOM_SIZE: u64 = 4 * 1024;

        let extra_data = read_config_extra_data(it, header, DDTS_SIZE, MAX_DDTS_ATOM_SIZE)?;

        let sample_rate =
            u32::from_be_bytes([extra_data[0], extra_data[1], extra_data[2], extra_data[3]]);
        let pcm_sample_depth = extra_data[12];

        // FrameDuration is the 2 most significant bits of the byte after the sample depth.
        let frame_size = Some(512u64 << (extra_data[13] >> 6));

        // Skip StreamConstruction, CoreLFEPresent, CoreLayout, CoreSize, StereoDownmix, and
        // RepresentationType.
        let channel_layout = u16::from_be_bytes([extra_data[17], extra_data[18]]);

        Ok(DdtsAtom { sample_rate, frame_size, pcm_sample_depth, channel_layout, extra_data })
    }
}

/// Gets the channels of a DTS channel layout bit mask (ETSI TS 102 114 E.2.2.3), with the
/// same speaker positions as the decoder.
fn dts_channels(layout: u16) -> Channels {
    // Ls/Rs are the side channels if any other surround channel is present, else rear channels.
    let more_surrounds = layout & (1 << 4 | 1 << 6 | 1 << 11) != 0;

    let mut pos = Position::empty();

    for bit in 0..16 {
        if layout & (1 << bit) == 0 {
            continue;
        }

        pos |= match bit {
            0 => Position::FRONT_CENTER,
            1 => Position::FRONT_LEFT | Position::FRONT_RIGHT,
            2 if more_surrounds => Position::SIDE_LEFT | Position::SIDE_RIGHT,
            2 => Position::REAR_LEFT | Position::REAR_RIGHT,
            3 => Position::LFE1,
            4 => Position::REAR_CENTER,
            5 => Position::TOP_FRONT_LEFT | Position::TOP_FRONT_RIGHT,
            6 => Position::REAR_LEFT | Position::REAR_RIGHT,
            7 => Position::TOP_FRONT_CENTER,
            8 => Position::TOP_CENTER,
            9 => Position::FRONT_LEFT_CENTER | Position::FRONT_RIGHT_CENTER,
            10 => Position::FRONT_LEFT_WIDE | Position::FRONT_RIGHT_WIDE,
            11 => Position::SIDE_LEFT | Position::SIDE_RIGHT,
            12 => Position::LFE2,
            13 => Position::TOP_SIDE_LEFT | Position::TOP_SIDE_RIGHT,
            14 => Position::TOP_REAR_CENTER,
            _ => Position::TOP_REAR_LEFT | Position::TOP_REAR_RIGHT,
        };
    }

    Channels::Positioned(pos)
}

impl DdtsAtom {
    pub fn fill_audio_sample_entry(self, entry: &mut AudioSampleEntry) {
        entry.codec_id = CODEC_ID_DCA;

        // The sample rate of the sample entry is limited to 16 bits.
        if self.sample_rate > 0 {
            entry.sample_rate = f64::from(self.sample_rate);
        }

        if self.channel_layout != 0 {
            entry.channels = Some(dts_channels(self.channel_layout));
        }

        if self.pcm_sample_depth != 0 {
            entry.bits_per_sample = Some(u32::from(self.pcm_sample_depth));
        }

        entry.frames_per_packet = self.frame_size;
        entry.extra_data = Some(self.extra_data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_5_1_layout() {
        // C, L/R, Ls/Rs, LFE1.
        let Channels::Positioned(pos) = dts_channels(0x000f)
        else {
            panic!("expected positioned channels");
        };
        assert_eq!(pos.bits().count_ones(), 6);
        assert!(pos.contains(Position::REAR_LEFT));
        assert!(pos.contains(Position::LFE1));
    }

    #[test]
    fn surrounds_are_sides_with_other_surrounds() {
        // C, L/R, Ls/Rs, LFE1, Cs.
        let Channels::Positioned(pos) = dts_channels(0x001f)
        else {
            panic!("expected positioned channels");
        };
        assert_eq!(pos.bits().count_ones(), 7);
        assert!(pos.contains(Position::SIDE_LEFT));
        assert!(pos.contains(Position::REAR_CENTER));
    }
}
