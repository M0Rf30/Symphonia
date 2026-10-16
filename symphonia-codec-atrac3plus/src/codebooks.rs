// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Huffman codebooks of the ATRAC3+ bitstream.

use std::sync::LazyLock;

use symphonia_core::errors::{Result, decode_error};

use crate::bits::Vlc;
use crate::tables::*;

/// All Huffman tables, built once per process.
pub(crate) struct Codebooks {
    /// Word length.
    pub wl: Vec<Vlc>,
    /// Code table indexes.
    pub ct: Vec<Vlc>,
    /// Scale factors.
    pub sf: Vec<Vlc>,
    /// Spectrum, indexed by `(table_type * 8 + code_table) * 7 + word_length - 1`.
    spec: Vec<Vlc>,
    /// Maps a spectrum table index to the index in `spec` holding its codebook.
    spec_map: Vec<usize>,
    /// Gain control data.
    pub gain: Vec<Vlc>,
    /// GHA tone data.
    pub tone: Vec<Vlc>,
}

impl Codebooks {
    /// Get the spectrum codebook for a spectrum table index.
    pub(crate) fn spectrum(&self, tab_index: usize) -> Option<&Vlc> {
        self.spec_map.get(tab_index).and_then(|&i| self.spec.get(i))
    }

    /// Get the shared codebooks, building them on first use.
    pub(crate) fn get() -> Result<&'static Codebooks> {
        static CODEBOOKS: LazyLock<Option<Codebooks>> = LazyLock::new(|| Codebooks::build().ok());

        match &*CODEBOOKS {
            Some(c) => Ok(c),
            None => decode_error("atrac3plus: invalid codebook tables"),
        }
    }

    fn build() -> Result<Codebooks> {
        // Word length and code table codebooks share one symbol table, interleaved.
        let mut xlat = &WL_CT_XLATS[..];
        let mut wl = Vec::new();
        let mut ct = Vec::new();
        for i in 0..4 {
            let (v, n) = Vlc::from_codebook(&WL_CBS[i], xlat)?;
            wl.push(v);
            xlat = &xlat[n..];
            let (v, n) = Vlc::from_codebook(&CT_CBS[i], xlat)?;
            ct.push(v);
            xlat = &xlat[n..];
        }

        let sf = build_set(&SF_CBS, &SF_XLATS)?;
        let gain = build_set(&GAIN_CBS, &GAIN_XLATS)?;
        let tone = build_set(&TONE_CBS, &TONE_XLATS)?;

        // A spectrum codebook whose first entry is negative `-n` reuses codebook `n`.
        let mut spec = Vec::new();
        let mut spec_map = Vec::with_capacity(SPECTRA_CBS.len());
        let mut xlat = &SPECTRA_XLATS[..];
        for cb in SPECTRA_CBS.iter() {
            if cb[0] >= 0 {
                let mut counts = [0u8; 12];
                for (c, &v) in counts.iter_mut().zip(cb) {
                    *c = v as u8;
                }
                let (v, n) = Vlc::from_codebook(&counts, xlat)?;
                xlat = &xlat[n..];
                spec_map.push(spec.len());
                spec.push(v);
            }
            else {
                match spec_map.get(usize::from(cb[0].unsigned_abs())) {
                    Some(&i) => spec_map.push(i),
                    None => return decode_error("atrac3plus: invalid codebook reuse"),
                }
            }
        }

        Ok(Codebooks { wl, ct, sf, spec, spec_map, gain, tone })
    }
}

/// Build a series of codebooks that consume consecutive runs of one symbol table.
fn build_set(cbs: &[[u8; 12]], mut xlat: &[u8]) -> Result<Vec<Vlc>> {
    let mut out = Vec::with_capacity(cbs.len());
    for cb in cbs {
        let (v, n) = Vlc::from_codebook(cb, xlat)?;
        xlat = &xlat[n..];
        out.push(v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_codebooks_build() {
        let cb = Codebooks::get().unwrap();
        assert_eq!(cb.wl.len(), 4);
        assert_eq!(cb.ct.len(), 4);
        assert_eq!(cb.sf.len(), 8);
        assert_eq!(cb.gain.len(), 11);
        assert_eq!(cb.tone.len(), 7);
        for i in 0..112 {
            assert!(cb.spectrum(i).is_some());
        }
        assert!(cb.spectrum(112).is_none());
    }
}
