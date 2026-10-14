// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
// Casts and indexing patterns inherent to the transform-codec arithmetic.
#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]

//! Dolby Digital (AC-3) and Dolby Digital Plus (E-AC-3) audio decoder.
//!
//! # Features
//!
//! * AC-3 (ETSI TS 102 366, main body): all audio coding modes, LFE, coupling, rematrixing,
//!   dynamic range control (as transmitted), and block switching. Bit stream identifications 0 to
//!   10 are decoded, including the alternate bit stream syntax (6) and the half and quarter
//!   sample rate variants (9 and 10).
//! * E-AC-3 (Annex E): independent substreams and the dependent substreams of substream 0
//!   (e.g., 7.1), adaptive hybrid transform, spectral extension, enhanced coupling, all frame
//!   sizes of 1, 2, 3 and 6 audio blocks, and the custom channel map.
//!
//! # Output
//!
//! Planar 32-bit floating-point samples in the range -1.0 to +1.0, one plane per channel
//! ordered by ascending [`Position`] bit (the Symphonia canonical channel order). Down-mixing is
//! not performed.
//!
//! # Packets
//!
//! A packet may contain one or more syncframes (e.g. an E-AC-3 independent syncframe followed by
//! its dependent syncframes). Packets in the byte-swapped (little-endian, `0x770B`) form are
//! accepted.
//!
//! # Notes
//!
//! * Programme selection: independent substreams with a `substreamid` other than 0 (other
//!   programmes) and the dependent substreams attached to them are skipped; programme 0 is
//!   decoded. `blkstrtinfo` is parsed and skipped (it only aids splicing).
//! * Transient pre-noise processing (E.2.7) is applied per frame. Corrections reaching into a
//!   frame that was already emitted are clipped. The spec gives no window shape or bit
//!   allocation details for it; a raised-cosine cross-fade is used. Unverified: no reference
//!   decoder or stream is available.
//! * E-AC-3 reduced sample rates (`fscod` 3 with `fscod2`, ATSC A/52:2018 Annex E) are decoded
//!   with the half-rate bit allocation indexing used for AC-3 `bsid` 9 and 10. Unverified.
//! * AC-3 syncframes with `bsid` 9 or 10 are muted (decoded as silence of the signalled
//!   duration), as ETSI TS 102 366 clauses 4.4.1.3 and E.1.3.1.6 require.

mod bitalloc;
mod bits;
mod core;
mod ext;
mod header;
mod imdct;
mod tables;
mod transient;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, Channels,
    GenericAudioBufferRef, Position,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_AC3, CODEC_ID_EAC3};
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::core::{Core, Lfg, MAX_BLOCKS};
use crate::header::{FrameHeader, MIN_HEADER_BYTES, STRMTYP_DEPENDENT};
use crate::imdct::BLOCK_SIZE;

/// Channel locations (ETSI TS 102 366 Table E.1.4), in ascending id order.
const LOC_L: u8 = 0;
const LOC_C: u8 = 1;
const LOC_R: u8 = 2;
const LOC_LS: u8 = 3;
const LOC_RS: u8 = 4;
const LOC_LC: u8 = 5;
const LOC_RC: u8 = 6;
const LOC_LRS: u8 = 7;
const LOC_RRS: u8 = 8;
const LOC_CS: u8 = 9;
const LOC_TS: u8 = 10;
const LOC_LSD: u8 = 11;
const LOC_RSD: u8 = 12;
const LOC_LW: u8 = 13;
const LOC_RW: u8 = 14;
const LOC_VHL: u8 = 15;
const LOC_VHR: u8 = 16;
const LOC_VHC: u8 = 17;
const LOC_LTS: u8 = 18;
const LOC_RTS: u8 = 19;
const LOC_LFE2: u8 = 20;
const LOC_LFE: u8 = 21;

/// The maximum number of channels of an access unit (independent + dependent substream).
const MAX_AU_CHANNELS: usize = 16;

/// The channel locations of the full bandwidth channels of each audio coding mode, in coded
/// order.
fn acmod_locations(acmod: u8) -> &'static [u8] {
    match acmod {
        0 | 2 => &[LOC_L, LOC_R],
        1 => &[LOC_C],
        3 => &[LOC_L, LOC_C, LOC_R],
        4 => &[LOC_L, LOC_R, LOC_CS],
        5 => &[LOC_L, LOC_C, LOC_R, LOC_CS],
        6 => &[LOC_L, LOC_R, LOC_LS, LOC_RS],
        _ => &[LOC_L, LOC_C, LOC_R, LOC_LS, LOC_RS],
    }
}

/// Maps a channel location to a Symphonia channel position.
fn location_position(loc: u8, quad: bool) -> Option<Position> {
    Some(match loc {
        LOC_L => Position::FRONT_LEFT,
        LOC_C => Position::FRONT_CENTER,
        LOC_R => Position::FRONT_RIGHT,
        // The surround channels of the 2/2 mode are rear channels, otherwise they are side
        // channels.
        LOC_LS if quad => Position::REAR_LEFT,
        LOC_RS if quad => Position::REAR_RIGHT,
        LOC_LS => Position::SIDE_LEFT,
        LOC_RS => Position::SIDE_RIGHT,
        LOC_LC => Position::FRONT_LEFT_CENTER,
        LOC_RC => Position::FRONT_RIGHT_CENTER,
        LOC_LRS => Position::REAR_LEFT,
        LOC_RRS => Position::REAR_RIGHT,
        LOC_CS => Position::REAR_CENTER,
        LOC_TS => Position::TOP_CENTER,
        LOC_LW => Position::FRONT_LEFT_WIDE,
        LOC_RW => Position::FRONT_RIGHT_WIDE,
        LOC_VHL => Position::TOP_FRONT_LEFT,
        LOC_VHR => Position::TOP_FRONT_RIGHT,
        LOC_VHC => Position::TOP_FRONT_CENTER,
        LOC_LTS => Position::TOP_SIDE_LEFT,
        LOC_RTS => Position::TOP_SIDE_RIGHT,
        LOC_LFE2 => Position::LFE2,
        LOC_LFE => Position::LFE1,
        // LOC_LSD and LOC_RSD have no position.
        _ => return None,
    })
}

/// Expands a custom channel map into a list of locations in coded channel order. Returns the
/// number of locations.
fn expand_chanmap(chanmap: u16, out: &mut [u8; MAX_AU_CHANNELS]) -> usize {
    const MAP: [&[u8]; 16] = [
        &[LOC_L],
        &[LOC_C],
        &[LOC_R],
        &[LOC_LS],
        &[LOC_RS],
        &[LOC_LC, LOC_RC],
        &[LOC_LRS, LOC_RRS],
        &[LOC_CS],
        &[LOC_TS],
        &[LOC_LSD, LOC_RSD],
        &[LOC_LW, LOC_RW],
        &[LOC_VHL, LOC_VHR],
        &[LOC_VHC],
        &[LOC_LTS, LOC_RTS],
        &[LOC_LFE2],
        &[LOC_LFE],
    ];

    let mut n = 0;
    for (bit, locs) in MAP.iter().enumerate() {
        if chanmap & (0x8000 >> bit) != 0 {
            for &l in *locs {
                out[n] = l;
                n += 1;
            }
        }
    }
    n
}

/// An access unit: an independent frame and (optionally) its dependent frame.
struct AccessUnit {
    indep: FrameHeader,
    dep: Option<FrameHeader>,
}

/// Dolby Digital (AC-3) and Dolby Digital Plus (E-AC-3) decoder.
pub struct Ac3Decoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    codec_idx: usize,
    buf: AudioBuffer<f32>,
    /// Index 0: the independent substream. Index 1: the dependent substream.
    cores: [Box<Core>; 2],
    lfg: Lfg,
    /// Scratch space for byte-swapped packets.
    swapped: Vec<u8>,
}

impl Ac3Decoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        let codec_idx = match params.codec {
            CODEC_ID_AC3 => 0,
            CODEC_ID_EAC3 => 1,
            _ => return unsupported_error("ac3: invalid codec"),
        };

        let rate = params.sample_rate.unwrap_or(48_000);
        let channels = params
            .channels
            .clone()
            .unwrap_or(Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT));

        let buf = AudioBuffer::new(AudioSpec::new(rate, channels), BLOCK_SIZE * MAX_BLOCKS);

        Ok(Ac3Decoder {
            params: params.clone(),
            opts: *opts,
            codec_idx,
            buf,
            cores: [Box::new(Core::new()), Box::new(Core::new())],
            lfg: Lfg::new(),
            swapped: Vec::new(),
        })
    }

    /// Appends the decoded access unit to the output buffer.
    fn emit(&mut self, au: &AccessUnit) {
        let n = au.indep.num_blocks * BLOCK_SIZE;

        // Gather the channel locations: (location, core, pcm row).
        let mut ent = [(0u8, 0u8, 0u8); MAX_AU_CHANNELS];
        let mut cnt = 0;

        let fbw = au.indep.fbw_channels();
        for (i, &loc) in acmod_locations(au.indep.acmod).iter().enumerate() {
            ent[cnt] = (loc, 0, i as u8);
            cnt += 1;
        }
        if au.indep.lfeon {
            ent[cnt] = (LOC_LFE, 0, fbw as u8);
            cnt += 1;
        }

        if let Some(dep) = &au.dep {
            let mut locs = [0u8; MAX_AU_CHANNELS];
            let mut nlocs = 0;

            if let Some(map) = dep.chanmap {
                nlocs = expand_chanmap(map, &mut locs);
            }

            if nlocs != dep.channels() {
                // No (or an inconsistent) custom channel map: use the audio coding mode.
                nlocs = 0;
                for &loc in acmod_locations(dep.acmod) {
                    locs[nlocs] = loc;
                    nlocs += 1;
                }
                if dep.lfeon {
                    locs[nlocs] = LOC_LFE;
                    nlocs += 1;
                }
            }

            for (row, &loc) in locs[..nlocs].iter().enumerate() {
                match ent[..cnt].iter_mut().find(|e| e.0 == loc) {
                    Some(e) => *e = (loc, 1, row as u8),
                    None if cnt < MAX_AU_CHANNELS => {
                        ent[cnt] = (loc, 1, row as u8);
                        cnt += 1;
                    }
                    None => (),
                }
            }
        }

        // Determine the channel order of the output planes.
        let quad = au.indep.acmod == 6;
        let mut positions = Position::empty();
        let mut positioned = true;
        for e in &ent[..cnt] {
            match location_position(e.0, quad) {
                Some(p) if !positions.intersects(p) => positions |= p,
                _ => positioned = false,
            }
        }

        let ent = &mut ent[..cnt];
        let channels = if positioned {
            ent.sort_unstable_by_key(|e| {
                location_position(e.0, quad).map(|p| p.bits()).unwrap_or(u64::MAX)
            });
            Channels::Positioned(positions)
        }
        else {
            ent.sort_unstable_by_key(|e| e.0);
            Channels::Discrete(cnt as u16)
        };

        // Re-create the buffer if the stream changed.
        let spec = AudioSpec::new(au.indep.sample_rate, channels);
        if self.buf.spec() != &spec {
            self.buf = AudioBuffer::new(spec, n);
        }

        let start = self.buf.frames();
        self.buf.grow_capacity(start + n);
        self.buf.render_uninit(Some(n));

        for (i, e) in ent.iter().enumerate() {
            let src = &self.cores[usize::from(e.1)].pcm[usize::from(e.2)][..n];
            if let Some(plane) = self.buf.plane_mut(i) {
                plane[start..start + n].copy_from_slice(src);
            }
        }
    }

    fn decode_inner(&mut self, data: &[u8]) -> Result<()> {
        let mut pending: Option<AccessUnit> = None;
        let mut decoded_any = false;

        let mut pos = 0;
        while pos + MIN_HEADER_BYTES <= data.len() {
            let hdr = match header::parse(&data[pos..]) {
                Ok(hdr) => hdr,
                Err(_) => {
                    // Re-synchronize.
                    pos += 1;
                    continue;
                }
            };

            let end = (pos + hdr.frame_size).min(data.len());
            let frame = &data[pos..end];
            pos = end;

            if !hdr.eac3 || hdr.strmtyp != STRMTYP_DEPENDENT {
                // A new independent frame (of substream 0) completes the previous access unit.
                if hdr.substreamid != 0 {
                    continue;
                }
                if let Some(au) = pending.take() {
                    self.emit(&au);
                }
                self.cores[0].decode_frame(&hdr, frame, &mut self.lfg);
                pending = Some(AccessUnit { indep: hdr, dep: None });
                decoded_any = true;
            }
            else if let Some(au) = pending.as_mut() {
                // Dependent substreams extend the preceding independent frame.
                if hdr.substreamid == 0
                    && au.dep.is_none()
                    && hdr.num_blocks == au.indep.num_blocks
                    && hdr.sample_rate == au.indep.sample_rate
                {
                    self.cores[1].decode_frame(&hdr, frame, &mut self.lfg);
                    au.dep = Some(hdr);
                }
            }
        }

        if let Some(au) = pending.take() {
            self.emit(&au);
        }

        if !decoded_any {
            return decode_error("ac3: no syncframe found");
        }

        Ok(())
    }

    fn decode_packet(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();

        // Packets may be in the byte-swapped (little-endian) form.
        let swapped = packet.data.len() >= 2 && packet.data[0] == 0x77 && packet.data[1] == 0x0b;

        let result = if swapped {
            let mut scratch = std::mem::take(&mut self.swapped);
            scratch.clear();
            scratch.extend_from_slice(packet.data);
            for pair in scratch.chunks_exact_mut(2) {
                pair.swap(0, 1);
            }
            let r = self.decode_inner(&scratch);
            self.swapped = scratch;
            r
        }
        else {
            self.decode_inner(packet.data)
        };

        result?;

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for Ac3Decoder {
    fn reset(&mut self) {
        for core in &mut self.cores {
            core.reset();
        }
        self.lfg.reset();
        self.buf.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()[self.codec_idx].info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_packet(packet) {
            self.buf.clear();
            Err(e)
        }
        else {
            Ok(self.buf.as_generic_audio_buffer_ref())
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for Ac3Decoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(Ac3Decoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[
            support_audio_codec!(CODEC_ID_AC3, "ac3", "Dolby Digital (AC-3)"),
            support_audio_codec!(CODEC_ID_EAC3, "eac3", "Dolby Digital Plus (E-AC-3)"),
        ]
    }
}
