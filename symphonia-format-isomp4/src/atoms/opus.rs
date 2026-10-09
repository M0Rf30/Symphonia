// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_common::xiph::audio::opus;
use symphonia_core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia_core::errors::Error;
use symphonia_core::io::BufReader;

use crate::atoms::stsd::AudioSampleEntry;
use crate::atoms::{
    Atom, AtomHeader, AtomIterator, ReadAtom, Result, decode_error, unsupported_error,
};

const OPUS_MAGIC: &[u8] = b"OpusHead";
const OPUS_MAGIC_LEN: usize = OPUS_MAGIC.len();

/// Opus atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct OpusAtom {
    /// Opus extra data (identification header).
    extra_data: Box<[u8]>,
}

impl Atom for OpusAtom {
    fn read<R: ReadAtom>(reader: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        const MIN_OPUS_EXTRA_DATA_SIZE: usize = OPUS_MAGIC_LEN + 11;
        const MAX_OPUS_EXTRA_DATA_SIZE: usize = MIN_OPUS_EXTRA_DATA_SIZE + 257;

        // The dops atom contains an Opus identification header excluding the OpusHead magic
        // signature. Therefore, the atom data length should be atleast as long as the shortest
        // Opus identification header.
        let data_len = header
            .data_size()
            .ok_or(Error::DecodeError("isomp4 (opus): expected atom size to be known"))?
            as usize;

        if data_len < MIN_OPUS_EXTRA_DATA_SIZE - OPUS_MAGIC_LEN {
            return decode_error("isomp4 (opus): opus identification header too short");
        }

        if data_len > MAX_OPUS_EXTRA_DATA_SIZE - OPUS_MAGIC_LEN {
            return decode_error("isomp4 (opus): opus identification header too large");
        }

        // Read the `OpusSpecificBox` payload (`dOps`). Unlike an Ogg `OpusHead`, its version is 0
        // and its multi-byte fields (pre-skip, input sample rate, output gain) are big-endian.
        let mut dops = vec![0; data_len];
        reader.read_buf_exact(&mut dops)?;

        // Verify the version number is 0.
        if dops[0] != 0 {
            return unsupported_error("isomp4 (opus): unsupported opus version");
        }

        let channels = usize::from(dops[1]);
        let mapping_family = dops[10];

        // A non-zero mapping family is followed by the stream count, coupled stream count, and a
        // channel mapping table with one entry per channel.
        let expected_len = if mapping_family == 0 { 11 } else { 11 + 2 + channels };

        if channels == 0 || data_len < expected_len {
            return decode_error("isomp4 (opus): invalid opus identification header");
        }

        // Rebuild an Ogg-style (version 1, little-endian) `OpusHead`, the format the Opus decoder
        // expects as extra data.
        let mut extra_data = Vec::with_capacity(OPUS_MAGIC_LEN + expected_len);
        extra_data.extend_from_slice(OPUS_MAGIC);
        extra_data.push(1);
        extra_data.push(dops[1]);
        extra_data.extend(dops[2..4].iter().rev());
        extra_data.extend(dops[4..8].iter().rev());
        extra_data.extend(dops[8..10].iter().rev());
        extra_data.extend_from_slice(&dops[10..expected_len]);
        let extra_data = extra_data.into_boxed_slice();

        Ok(OpusAtom { extra_data })
    }
}

impl OpusAtom {
    pub fn fill_audio_sample_entry(self, entry: &mut AudioSampleEntry) {
        let mut reader = BufReader::new(&self.extra_data);

        if let Ok(opus_head) = opus::OpusHead::read(&mut reader, 1) {
            entry.channels = Some(opus_head.channels);
            entry.sample_rate = 48_000.0;
        }

        entry.codec_id = CODEC_ID_OPUS;
        entry.extra_data = Some(self.extra_data);
    }
}
