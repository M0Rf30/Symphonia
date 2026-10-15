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
// Disable to better express the specification.
#![allow(clippy::collapsible_else_if)]
// The decoder follows the structure of the specification.
#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]

//! A DTS Coherent Acoustics (DCA) audio decoder.
//!
//! Decodes the backward compatible core substream, including the XCH, XXCH, X96 and XBR core
//! extensions, from packets in any of the four DTS bitstream forms (16-bit and 14-bit, big- and
//! little-endian) and from DTS-HD packets where a core substream is followed by an extension
//! substream (EXSS).
//!
//! If the extension substream carries a lossless (XLL, DTS-HD Master Audio) component, the output
//! is the bit-exact lossless reconstruction. The low bit rate (LBR, DTS Express) component is
//! decoded when the packet has neither a core nor an XLL component.

mod bits;
mod decoder;
mod dsp;
mod exss;
mod huff;
mod lbr;
mod tables;
mod tables_dct;
mod tables_lbr;
mod xll;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, Channels,
    GenericAudioBufferRef, Position,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_DCA;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, Downmix, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::decoder::{
    CoreDecoder, SYNC_CORE_14B_BE, SYNC_CORE_14B_LE, SYNC_CORE_BE, SYNC_CORE_LE, SYNC_SUBSTREAM,
    spkr,
};
use crate::lbr::LbrDecoder;
use crate::xll::{XllDecoder, XllError};

/// The minimum size of a packet.
const MIN_PACKET_SIZE: usize = 16;
/// The maximum size of a packet.
const MAX_PACKET_SIZE: usize = 0x104000;
/// The maximum number of output samples per channel in a frame (128 PCM blocks of 32 samples at
/// twice the core sample rate).
const MAX_FRAME_SAMPLES: usize = 128 * 32 * 2;

/// Get the position of a DTS speaker. `mask` is the speaker mask of the stream.
fn speaker_position(speaker: usize, mask: u32) -> Position {
    // In arrangements that carry any surround speaker other than Ls/Rs, Ls/Rs are the side
    // channels (consistent with the 6.1 and 7.1 layouts), otherwise they are the rear channels
    // (consistent with 5.1).
    let more_surrounds = mask
        & (1 << spkr::CS | 1 << spkr::LSR | 1 << spkr::RSR | 1 << spkr::LSS | 1 << spkr::RSS)
        != 0;

    match speaker {
        0 => Position::FRONT_CENTER,
        1 => Position::FRONT_LEFT,
        2 => Position::FRONT_RIGHT,
        3 if more_surrounds => Position::SIDE_LEFT,
        3 => Position::REAR_LEFT,
        4 if more_surrounds => Position::SIDE_RIGHT,
        4 => Position::REAR_RIGHT,
        5 => Position::LFE1,
        6 => Position::REAR_CENTER,
        7 => Position::REAR_LEFT,
        8 => Position::REAR_RIGHT,
        9 => Position::SIDE_LEFT,
        10 => Position::SIDE_RIGHT,
        11 => Position::FRONT_LEFT_CENTER,
        12 => Position::FRONT_RIGHT_CENTER,
        13 => Position::TOP_FRONT_LEFT,
        14 => Position::TOP_FRONT_CENTER,
        15 => Position::TOP_FRONT_RIGHT,
        16 => Position::LFE2,
        17 => Position::FRONT_LEFT_WIDE,
        18 => Position::FRONT_RIGHT_WIDE,
        19 => Position::TOP_CENTER,
        20 => Position::TOP_SIDE_LEFT,
        21 => Position::TOP_SIDE_RIGHT,
        22 => Position::TOP_REAR_CENTER,
        23 => Position::TOP_REAR_LEFT,
        24 => Position::TOP_REAR_RIGHT,
        25 => Position::BOTTOM_FRONT_CENTER,
        26 => Position::BOTTOM_FRONT_LEFT,
        _ => Position::BOTTOM_FRONT_RIGHT,
    }
}

/// Convert a packet in a DTS bitstream form other than 16-bit big-endian into 16-bit big-endian.
///
/// Returns `false` if `src` does not start with a recognized sync word.
fn convert_bitstream(src: &[u8], dst: &mut Vec<u8>) -> bool {
    if src.len() < 4 {
        return false;
    }

    dst.clear();

    let mrk = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);

    // Read the little-endian or big-endian 16-bit word at the given even byte index. A missing
    // trailing byte is zero.
    let word = |i: usize, le: bool| -> u16 {
        let a = src[i];
        let b = src.get(i + 1).copied().unwrap_or(0);
        if le { u16::from_le_bytes([a, b]) } else { u16::from_be_bytes([a, b]) }
    };

    match mrk {
        SYNC_CORE_BE | SYNC_SUBSTREAM => {
            dst.extend_from_slice(src);
            true
        }
        SYNC_CORE_LE => {
            for i in (0..src.len()).step_by(2) {
                dst.extend_from_slice(&word(i, true).to_be_bytes());
            }
            dst.truncate(src.len());
            true
        }
        SYNC_CORE_14B_BE | SYNC_CORE_14B_LE => {
            let le = mrk == SYNC_CORE_14B_LE;

            // Pack 14 bits of each word, MSB first.
            let mut acc = 0u32;
            let mut nbits = 0;

            for i in (0..src.len()).step_by(2) {
                acc = acc << 14 | u32::from(word(i, le) & 0x3fff);
                nbits += 14;
                while nbits >= 8 {
                    nbits -= 8;
                    dst.push((acc >> nbits) as u8);
                }
                acc &= (1 << nbits) - 1;
            }
            if nbits > 0 {
                dst.push((acc << (8 - nbits)) as u8);
            }
            true
        }
        _ => false,
    }
}

/// DTS Coherent Acoustics (DCA) decoder.
pub struct DcaDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    core: CoreDecoder,
    xll: XllDecoder,
    lbr: LbrDecoder,
    /// The `PACKET_*` flags of the last packet.
    packet: u32,
    /// Scratch buffer for packets that are not in 16-bit big-endian form.
    conv: Vec<u8>,
    buf: AudioBuffer<f32>,
    /// The DTS speaker of each plane of `buf`.
    plane_spkr: Vec<usize>,
    /// The (sample rate, speaker mask, capacity) `buf` was created for.
    buf_key: (u32, u32, usize),
}

/// The packet has a core substream.
const PACKET_CORE: u32 = 0x01;
/// The packet has a valid XLL component.
const PACKET_XLL: u32 = 0x04;
/// Mask of the flags that describe the contents of the packet.
const PACKET_MASK: u32 = 0x0f;
/// Lossy output of the core is forced while XLL recovers.
const PACKET_RECOVERY: u32 = 0x100;
/// The core output of the packet is suitable for combining with a residual.
const PACKET_RESIDUAL: u32 = 0x200;

/// The decoder output a frame was rendered from.
#[derive(Copy, Clone)]
enum Source {
    CoreFloat,
    CoreFixed,
    /// XLL output with the given number of bits of resolution.
    Xll(u32),
}

impl DcaDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_DCA {
            return unsupported_error("dca: invalid codec");
        }

        // The sample rate and channels are determined from the stream. Use the parameters from the
        // container until the first packet is decoded.
        let rate = params.sample_rate.unwrap_or(48000);
        let channels =
            params.channels.clone().unwrap_or(Channels::Positioned(Position::FRONT_CENTER));

        Ok(DcaDecoder {
            params: params.clone(),
            opts: *opts,
            core: CoreDecoder::new(),
            xll: XllDecoder::new(),
            lbr: LbrDecoder::new(),
            packet: 0,
            conv: Vec::new(),
            buf: AudioBuffer::new(AudioSpec::new(rate, channels), 0),
            plane_spkr: Vec::new(),
            buf_key: (0, 0, 0),
        })
    }

    /// Ensure the output buffer matches the stream layout.
    fn prepare_buffer(&mut self, sample_rate: u32, ch_mask: u32, nsamples: usize) {
        let capacity = nsamples.max(MAX_FRAME_SAMPLES);

        if self.buf_key == (sample_rate, ch_mask, capacity) {
            return;
        }

        // Map each DTS speaker to a position and order the planes by position. If two speakers
        // map to the same position, the lowest speaker index wins.
        let mut layout: Vec<(Position, usize)> = Vec::new();

        for speaker in 0..spkr::COUNT {
            if ch_mask & (1 << speaker) != 0 {
                let pos = speaker_position(speaker, ch_mask);
                if !layout.iter().any(|&(p, _)| p == pos) {
                    layout.push((pos, speaker));
                }
            }
        }

        layout.sort_by_key(|&(pos, _)| pos.bits());

        let positions = layout.iter().fold(Position::empty(), |acc, &(pos, _)| acc | pos);

        self.plane_spkr = layout.iter().map(|&(_, speaker)| speaker).collect();

        let channels = Channels::Positioned(positions);

        self.buf = AudioBuffer::new(AudioSpec::new(sample_rate, channels.clone()), capacity);
        self.buf_key = (sample_rate, ch_mask, capacity);

        self.params.with_sample_rate(sample_rate).with_channels(channels);
    }

    fn decode_inner(&mut self, pkt: &PacketRef<'_>) -> Result<()> {
        let data = pkt.data;

        if data.len() < MIN_PACKET_SIZE || data.len() > MAX_PACKET_SIZE {
            return decode_error("dca: invalid packet size");
        }

        // Convert the input to 16-bit big-endian.
        let mrk = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);

        let input: &[u8] = if mrk == SYNC_CORE_BE || mrk == SYNC_SUBSTREAM {
            data
        }
        else {
            let mut found = false;

            for i in 0..=data.len() - MIN_PACKET_SIZE {
                if convert_bitstream(&data[i..], &mut self.conv) {
                    found = true;
                    break;
                }
            }

            if !found {
                return decode_error("dca: not a valid dca frame");
            }

            &self.conv
        };

        let mut rest = input;
        let mut have_core = false;

        // Parse the backward compatible core substream.
        if be32(input) == SYNC_CORE_BE {
            self.core.parse(input)?;
            have_core = true;

            // EXSS data must be aligned on a 4-byte boundary.
            let frame_size = (self.core.frame_size + 3) & !3;
            if input.len() - 4 > frame_size {
                rest = &input[frame_size..];
            }
        }

        // Parse the extension substream.
        let mut asset = None;

        if rest.len() >= 4 && be32(rest) == SYNC_SUBSTREAM {
            asset = exss::parse(rest).ok();
        }

        let prev_packet = self.packet;
        let mut packet = 0;

        if have_core {
            packet |= PACKET_CORE;
        }

        // Parse the lossless (XLL) component of the extension substream.
        if let Some(a) = asset.as_ref().filter(|a| a.has_xll()) {
            match self.xll.parse(rest, a) {
                Ok(()) => packet |= PACKET_XLL,
                Err(XllError::Again) => {
                    // Conceal a synchronization error.
                    if prev_packet & PACKET_XLL != 0 && have_core {
                        packet |= PACKET_XLL | PACKET_RECOVERY;
                    }
                }
                // Without XLL, the core is used if there is one.
                Err(_) => (),
            }
        }

        // The LBR component is decoded when there is nothing else.
        if packet & (PACKET_CORE | PACKET_XLL) == 0 {
            return match asset {
                Some(a) if a.has_lbr() => {
                    self.lbr.parse(rest, &a)?;
                    let f = self.lbr.filter_frame();
                    self.packet = 0;
                    self.prepare_buffer(f.sample_rate, f.ch_mask, f.nsamples);
                    self.buf.clear();
                    self.buf.render_uninit(Some(f.nsamples));
                    for (idx, &speaker) in self.plane_spkr.iter().enumerate() {
                        let plane = self.buf.plane_mut(idx).expect("plane index in range");
                        plane[..f.nsamples].copy_from_slice(&self.lbr.out[speaker][..f.nsamples]);
                    }
                    if self.opts.downmix != Downmix::None {
                        self.downmix(f.sample_rate, f.ch_mask, f.nsamples);
                    }
                    if self.opts.gapless && (pkt.trim_start.get() != 0 || pkt.trim_end.get() != 0) {
                        self.buf.trim(pkt.trim_start.get() as usize, pkt.trim_end.get() as usize);
                    }
                    Ok(())
                }
                _ => decode_error("dca: no valid substream found"),
            };
        }

        // Parse the core extensions, X96 is not decoded when it is replaced by XLL.
        // Like the reference decoder, a stereo downmix with the coefficients embedded in the core
        // is made from the primary channel set: the extension channels are not decoded.
        let stereo = matches!(self.opts.downmix, Downmix::StereoLoRo | Downmix::StereoLtRt);
        let embedded_dmix =
            stereo && have_core && packet & PACKET_XLL == 0 && { self.core.has_stereo_downmix() };

        if have_core {
            self.core.parse_extensions(
                input,
                asset.as_ref().map(|a| (rest, a)),
                packet & PACKET_XLL != 0,
                embedded_dmix,
            )?;
        }

        let xll_asset = asset.as_ref().is_some_and(|a| a.has_xll());

        let (nsamples, sample_rate, ch_mask, source) = if packet & PACKET_XLL != 0 {
            if have_core {
                // Enable X96 synthesis if needed.
                let x96_synth = if self.xll.chset0_freq() == 96000 && self.core.sample_rate == 48000
                {
                    1
                }
                else {
                    -1
                };

                self.core.filter_fixed(x96_synth)?;

                // Force lossy downmixed output on the first core frame filtered. This prevents
                // audible clicks when seeking and is consistent with what the reference decoder
                // does when there are multiple channel sets.
                if prev_packet & PACKET_RESIDUAL == 0
                    && self.xll.nreschsets > 0
                    && self.xll.nchsets > 1
                {
                    packet |= PACKET_RECOVERY;
                }

                // Set the 'residual ok' flag for the next frame.
                packet |= PACKET_RESIDUAL;
            }

            let core = if have_core { Some(&self.core) } else { None };

            match self.xll.filter_frame(core, packet & PACKET_RECOVERY != 0) {
                Ok(f) => (f.nsamples, f.sample_rate, f.ch_mask, Source::Xll(f.bits)),
                // Fall back to the core, unless it is a hard error.
                Err(XllError::Invalid) if have_core => {
                    let info = self.core.filter_frame_fixed(true)?;
                    (info.nsamples, info.sample_rate, info.ch_mask, Source::CoreFixed)
                }
                Err(_) => return decode_error("dca: xll decoding failed"),
            }
        }
        else if xll_asset {
            // The core is the lossy fallback of XLL, use the fixed-point decoder like the XLL
            // decoder would.
            let info = self.core.filter_frame_fixed(false)?;
            packet |= PACKET_RESIDUAL;
            (info.nsamples, info.sample_rate, info.ch_mask, Source::CoreFixed)
        }
        else {
            let info = self.core.filter_frame()?;
            let ch_mask = if embedded_dmix {
                self.core.downmix_to_stereo(info.nsamples)
            }
            else {
                info.ch_mask
            };
            (info.nsamples, info.sample_rate, ch_mask, Source::CoreFloat)
        };

        self.packet = packet;

        self.prepare_buffer(sample_rate, ch_mask, nsamples);

        self.buf.clear();
        self.buf.render_uninit(Some(nsamples));

        for (idx, &speaker) in self.plane_spkr.iter().enumerate() {
            let plane = self.buf.plane_mut(idx).expect("plane index in range");
            let plane = &mut plane[..nsamples];

            match source {
                Source::CoreFloat => plane.copy_from_slice(&self.core.out[speaker][..nsamples]),
                Source::CoreFixed => {
                    let src = &self.core.out_fixed[speaker][..nsamples];
                    for (d, &s) in plane.iter_mut().zip(src) {
                        *d = s as f32 * (1.0 / 8388608.0);
                    }
                }
                Source::Xll(bits) => {
                    let scale = 1.0 / (1u32 << (bits - 1)) as f32;
                    let src = &self.xll.out[speaker][..nsamples];
                    for (d, &s) in plane.iter_mut().zip(src) {
                        *d = s as f32 * scale;
                    }
                }
            }
        }

        // Downmix with the standard equations if the stream did not provide the coefficients.
        if self.opts.downmix != Downmix::None && !embedded_dmix {
            self.downmix(sample_rate, ch_mask, nsamples);
        }

        // Trim gaps.
        if self.opts.gapless && (pkt.trim_start.get() != 0 || pkt.trim_end.get() != 0) {
            self.buf.trim(pkt.trim_start.get() as usize, pkt.trim_end.get() as usize);
        }

        Ok(())
    }
}

/// The weights of the channels of a downmix input: the front channels, the center channel and
/// the left and right surround channels.
#[derive(Copy, Clone, Default)]
struct Roles {
    l: f32,
    r: f32,
    c: f32,
    ls: f32,
    rs: f32,
}

/// The downmix equations of ITU-R BS.775-4 Annex 4 Table 2 (mono 1/0 and stereo 2/0 formats),
/// with the surround coefficient of 1/sqrt(2). The Lt/Rt downmix mixes the surround channels in
/// anti-phase as is done for Dolby Surround matrix encoding. The low frequency effects channel and
/// the height and wide channels are not mixed.
fn downmix_rows(mode: Downmix) -> Vec<Roles> {
    const K: f32 = std::f32::consts::FRAC_1_SQRT_2;

    match mode {
        Downmix::StereoLoRo => vec![
            Roles { l: 1.0, c: K, ls: K, ..Default::default() },
            Roles { r: 1.0, c: K, rs: K, ..Default::default() },
        ],
        Downmix::StereoLtRt => vec![
            Roles { l: 1.0, c: K, ls: -K, rs: -K, ..Default::default() },
            Roles { r: 1.0, c: K, ls: K, rs: K, ..Default::default() },
        ],
        _ => vec![Roles { l: K, r: K, c: 1.0, ls: 0.5, rs: 0.5 }],
    }
}

impl DcaDecoder {
    /// Mix the planes of `self.buf` with the standard downmix equations and make the result the
    /// content of the buffer.
    fn downmix(&mut self, sample_rate: u32, ch_mask: u32, nsamples: usize) {
        let rows = downmix_rows(self.opts.downmix);

        // The number of speakers feeding each surround role: they are averaged.
        let left_surr = [spkr::LS, spkr::LSR, spkr::LSS];
        let right_surr = [spkr::RS, spkr::RSR, spkr::RSS];
        let count = |set: &[usize]| set.iter().filter(|&&s| ch_mask & (1 << s) != 0).count().max(1);
        let nl = count(&left_surr) as f32;
        let nr = count(&right_surr) as f32;

        // The gain of each source plane in each output channel.
        let gains: Vec<Vec<f32>> = rows
            .iter()
            .map(|row| {
                self.plane_spkr
                    .iter()
                    .map(|&s| match s {
                        spkr::C => row.c,
                        spkr::L => row.l,
                        spkr::R => row.r,
                        spkr::LS | spkr::LSR | spkr::LSS => row.ls / nl,
                        spkr::RS | spkr::RSR | spkr::RSS => row.rs / nr,
                        // A mono surround channel is fed to both surround channels.
                        spkr::CS => (row.ls + row.rs) * 0.5,
                        _ => 0.0,
                    })
                    .collect()
            })
            .collect();

        let src: Vec<Vec<f32>> = (0..self.plane_spkr.len())
            .map(|i| self.buf.plane(i).map_or_else(Vec::new, |p| p[..nsamples].to_vec()))
            .collect();

        let out_mask = if rows.len() == 2 { 1 << spkr::L | 1 << spkr::R } else { 1 << spkr::C };

        self.prepare_buffer(sample_rate, out_mask, nsamples);
        self.buf.clear();
        self.buf.render_uninit(Some(nsamples));

        // The output planes are ordered by position: left, right for stereo.
        for (idx, row) in gains.iter().enumerate() {
            let plane = self.buf.plane_mut(idx).expect("plane index in range");
            let plane = &mut plane[..nsamples];
            plane.fill(0.0);
            for (gain, input) in row.iter().zip(&src) {
                if *gain != 0.0 {
                    for (o, &i) in plane.iter_mut().zip(input) {
                        *o += gain * i;
                    }
                }
            }
        }
    }
}

#[inline]
fn be32(data: &[u8]) -> u32 {
    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
}

impl AudioDecoder for DcaDecoder {
    fn reset(&mut self) {
        self.core.flush();
        self.xll.flush();
        self.lbr.flush();
        self.packet &= PACKET_MASK;
    }

    fn codec_info(&self) -> &CodecInfo {
        // Only one codec is supported.
        &Self::supported_codecs().first().expect("at least one codec registered").info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_inner(packet) {
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

impl RegisterableAudioDecoder for DcaDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(DcaDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_DCA, "dca", "DTS Coherent Acoustics")]
    }
}
