// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_common::mpeg::surround::MlpHeader;
use symphonia_core::audio::Channels;
use symphonia_core::codecs::audio::well_known::CODEC_ID_TRUEHD;

use crate::atoms::stsd::AudioSampleEntry;
use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result, read_config_extra_data};

/// The minimum size of the MLPSpecificBox payload: format_info (4 bytes), peak_data_rate (15 bits)
/// and a reserved field (17 bits).
const DMLP_SIZE: u64 = 10;

/// MLP specific box (`dmlp`).
#[derive(Debug)]
pub struct DmlpAtom {
    sample_rate: u32,
    channels: Channels,
}

impl Atom for DmlpAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        const MAX_DMLP_ATOM_SIZE: u64 = 4 * 1024;

        let data = read_config_extra_data(it, header, DMLP_SIZE, MAX_DMLP_ATOM_SIZE)?;

        // The format_info field is the same as the format_info of the major sync of a TrueHD
        // access unit. Reuse that parser by building a minimal access unit with a major sync.
        let mut major_sync = [0u8; 21];
        major_sync[4..8].copy_from_slice(&[0xf8, 0x72, 0x6f, 0xba]);
        major_sync[8..12].copy_from_slice(&data[..4]);

        match MlpHeader::parse(&major_sync) {
            Some(hdr) => Ok(DmlpAtom { sample_rate: hdr.sample_rate, channels: hdr.channels }),
            None => crate::atoms::decode_error("isomp4 (dmlp): invalid format info"),
        }
    }
}

impl DmlpAtom {
    pub fn fill_audio_sample_entry(self, entry: &mut AudioSampleEntry) {
        entry.codec_id = CODEC_ID_TRUEHD;
        entry.sample_rate = f64::from(self.sample_rate);
        entry.channels = Some(self.channels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_core::audio::Position;

    #[test]
    fn parses_format_info() {
        // 48 kHz, channel assignment 0b01111 for the first stream: L/R, C, LFE, Ls/Rs.
        let format_info: u32 = (0 << 28) | (0b01111 << 15);
        let mut major_sync = [0u8; 21];
        major_sync[4..8].copy_from_slice(&[0xf8, 0x72, 0x6f, 0xba]);
        major_sync[8..12].copy_from_slice(&format_info.to_be_bytes());

        let hdr = MlpHeader::parse(&major_sync).unwrap();
        assert_eq!(hdr.sample_rate, 48000);
        let Channels::Positioned(pos) = hdr.channels
        else {
            panic!("expected positioned channels");
        };
        assert!(pos.contains(Position::LFE1));
        assert_eq!(pos.bits().count_ones(), 6);
    }
}
