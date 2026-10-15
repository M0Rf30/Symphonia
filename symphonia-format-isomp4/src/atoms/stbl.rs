// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::atoms::stsz::SampleSize;
use crate::atoms::{
    Atom, AtomHeader, AtomIterator, AtomType, Co64Atom, ReadAtom, Result, StcoAtom, StscAtom,
    StsdAtom, StszAtom, SttsAtom, decode_error,
};

use log::{debug, warn};

/// Sample table atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct StblAtom {
    pub stsd: StsdAtom,
    pub stts: SttsAtom,
    pub stsc: StscAtom,
    pub stsz: StszAtom,
    pub stco: Option<StcoAtom>,
    pub co64: Option<Co64Atom>,
}

impl StblAtom {
    /// QuickTime may describe block-based audio (Microsoft ADPCM, IMA ADPCM WAV) with one sample
    /// per frame, with the samples of a constant size that is not the size of the blocks. Group
    /// the samples such that one sample is one block.
    /// Group the single frame samples of QuickTime and ISO PCM tracks (as many as 1 packet per
    /// frame) into packets of up to `MAX_PCM_PACKET_FRAMES` frames.
    fn regroup_pcm_frames(&mut self) {
        // The number of frames at which the cost of packets is negligible.
        const MAX_PCM_PACKET_FRAMES: u32 = 4096;

        if !self.stsd.is_pcm_frames() {
            return;
        }

        let SampleSize::Constant(size) = self.stsz.sample_sizes
        else {
            return;
        };

        // Chunks and timing must be made of whole groups.
        let g = crate::atoms::stts::gcd(
            self.stts.sample_count_gcd(),
            self.stsc.chunk_sample_count_gcd(self.stsz.sample_count),
        );

        // The largest divisor of the common divisor that is not too large.
        let Some(group) = (1..=g.min(MAX_PCM_PACKET_FRAMES)).rev().find(|d| g % d == 0)
        else {
            return;
        };

        let Some(group_size) = size.checked_mul(group)
        else {
            return;
        };

        if group > 1 && self.stsz.sample_count % group == 0 {
            let mut stts = self.stts.clone();
            if stts.regroup(group) && self.stsc.regroup(group) {
                self.stts = stts;
                self.stsz.sample_count /= group;
                self.stsz.sample_sizes = SampleSize::Constant(group_size);
            }
        }
    }

    /// QuickTime describes MACE audio with one sample per frame (a nominal size of 1), but
    /// the data is coded in blocks of 6 frames. Group the samples such that one sample is one
    /// block.
    fn regroup_mace_blocks(&mut self) {
        let Some((block_align, frames)) = self.stsd.mace_block()
        else {
            return;
        };

        if !matches!(self.stsz.sample_sizes, SampleSize::Constant(_))
            || self.stsz.sample_count % frames != 0
        {
            return;
        }

        // Validate before the first change.
        let mut stts = self.stts.clone();
        if stts.regroup(frames) && self.stsc.regroup(frames) {
            self.stts = stts;
            self.stsz.sample_count /= frames;
            self.stsz.sample_sizes = SampleSize::Constant(block_align);
        }
    }

    fn regroup_adpcm_blocks(&mut self) {
        let Some((block_align, frames)) = self.stsd.adpcm_block()
        else {
            return;
        };

        let SampleSize::Constant(size) = self.stsz.sample_sizes
        else {
            return;
        };

        // Samples that are already blocks.
        if size == block_align || self.stsz.sample_count % frames != 0 {
            return;
        }

        // Validate before the first change.
        let mut stts = self.stts.clone();
        if stts.regroup(frames) && self.stsc.regroup(frames) {
            self.stts = stts;
            self.stsz.sample_count /= frames;
            self.stsz.sample_sizes = SampleSize::Constant(block_align);
        }
    }
}

impl Atom for StblAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let mut stsd = None;
        let mut stts = None;
        let mut stsc = None;
        let mut stsz = None;
        let mut stco = None;
        let mut co64 = None;

        while let Some(header) = it.next_header()? {
            match header.atom_type {
                AtomType::SampleDescription => {
                    stsd = Some(it.read_atom::<StsdAtom>()?);
                }
                AtomType::TimeToSample => {
                    stts = Some(it.read_atom::<SttsAtom>()?);
                }
                AtomType::CompositionTimeToSample => {
                    // Composition time to sample atom is only required for video.
                    debug!("ignoring ctts atom.");
                }
                AtomType::SyncSample => {
                    // Sync sample atom is only required for video.
                    debug!("ignoring stss atom.");
                }
                AtomType::SampleToChunk => {
                    stsc = Some(it.read_atom::<StscAtom>()?);
                }
                AtomType::SampleSize => {
                    stsz = Some(it.read_atom::<StszAtom>()?);
                }
                AtomType::ChunkOffset => {
                    stco = Some(it.read_atom::<StcoAtom>()?);
                }
                AtomType::ChunkOffset64 => {
                    co64 = Some(it.read_atom::<Co64Atom>()?);
                }
                _ => (),
            }
        }

        if stco.is_none() && co64.is_none() {
            // This is a spec. violation, but some m4a files appear to lack these atoms.
            warn!("missing stco or co64 atom");
        }

        let Some(stsd) = stsd
        else {
            return decode_error("isomp4 (stbl): missing stsd atom");
        };
        let Some(stts) = stts
        else {
            return decode_error("isomp4 (stbl): missing stts atom");
        };
        let Some(stsc) = stsc
        else {
            return decode_error("isomp4 (stbl): missing stsc atom");
        };
        let Some(stsz) = stsz
        else {
            return decode_error("isomp4 (stbl): missing stsz atom");
        };

        let mut stbl = StblAtom { stsd, stts, stsc, stsz, stco, co64 };
        stbl.regroup_adpcm_blocks();
        stbl.regroup_mace_blocks();
        stbl.regroup_pcm_frames();
        Ok(stbl)
    }
}
