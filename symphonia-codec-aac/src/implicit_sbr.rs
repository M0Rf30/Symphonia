// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Detection of implicitly signalled SBR and parametric stereo.
//!
//! The transports that have no audio specific config of their own (ADTS, ADIF), or whose config
//! does not signal the extension (LATM), cannot tell a decoder that a stream is HE-AAC. The only
//! evidence is an `EXT_SBR_DATA` (and, for parametric stereo, a PS) payload in the `fil_element()`
//! of the first `raw_data_block()`s. The format readers look for it, so that the codec parameters
//! and the timeline of the track describe the decoded output (the SBR output rate, and stereo for
//! parametric stereo), as they do for streams that signal SBR explicitly (MP4).

use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

use symphonia_common::mpeg::audio::{
    AudioSpecificConfig, Mpeg4AudioSampleRate, ProgramConfig,
    get_mpeg4_audio_sample_rate_by_index,
};

use crate::AacDecoder;

pub(crate) use symphonia_common::mpeg::audio::may_have_implicit_sbr;
pub(crate) use symphonia_common::mpeg::audio::{
    MAX_IMPLICIT_SBR_CORE_RATE, MAX_IMPLICIT_SBR_PROBE_BLOCKS as MAX_PROBE_BLOCKS,
};

/// The number of samples per frame of AAC-LC.
const SAMPLES_PER_FRAME: u64 = 1024;

/// What the first `raw_data_block()`s of a stream tell about its extensions.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImplicitExtensions {
    /// The stream carries SBR data, so its output has twice the sample rate of the core codec.
    pub sbr: bool,
    /// The stream carries parametric stereo data, so a mono core has a stereo output.
    pub ps: bool,
}

/// Looks for SBR and parametric stereo in the `raw_data_block()`s `blocks` of the stream whose
/// plain (AAC-LC) audio specific config is `asc`.
///
/// The blocks are decoded with a throw-away decoder, which discovers the extensions the same way
/// the decoder of the stream will (the first `fil_element()` with an SBR payload, and the first
/// frame whose SBR payload renders a stereo pair from a mono core).
pub(crate) fn detect<'a>(
    asc: &[u8],
    asc_rate: u32,
    blocks: impl IntoIterator<Item = &'a [u8]>,
) -> ImplicitExtensions {
    let mut params = AudioCodecParameters::new();

    params.for_codec(CODEC_ID_AAC).with_sample_rate(asc_rate).with_extra_data(asc.into());

    let Ok(mut decoder) = AacDecoder::try_new(&params, &AudioDecoderOptions::default())
    else {
        return ImplicitExtensions::default();
    };

    let core_channels = decoder.codec_params().channels.as_ref().map(|c| c.count());

    let mut found = ImplicitExtensions::default();
    let mut ts = Timestamp::ZERO;

    for block in blocks.into_iter().take(MAX_PROBE_BLOCKS) {
        let dur = Duration::new(SAMPLES_PER_FRAME);
        let packet = Packet::new(0, ts, dur, Box::<[u8]>::from(block));
        ts = ts.saturating_add(dur);

        // A block that cannot be decoded says nothing about the stream.
        if decoder.decode(&packet).is_err() {
            continue;
        }

        let params = decoder.codec_params();

        found.sbr = params.sample_rate.is_some_and(|rate| rate > asc_rate);
        found.ps = found.sbr
            && core_channels == Some(1)
            && params.channels.as_ref().map(|c| c.count()) == Some(2);

        // Parametric stereo can only be told once the SBR payload of the frame has been decoded.
        if found.ps {
            break;
        }
    }

    found
}

/// Looks for implicit SBR (and parametric stereo) in the `raw_data_block()`s `blocks` of a stream
/// whose audio specific config is `asc`.
///
/// This is the detector the elementary stream parsers of the transport stream and Flash Video
/// readers are given (`symphonia_common::mpeg::es::ImplicitSbrDetector`). Returns the audio
/// specific config that signals the extension explicitly, or `None` if the stream may not have
/// implicit SBR, or none was found.
pub fn detect_implicit_sbr(asc: &[u8], blocks: &[&[u8]]) -> Option<Box<[u8]>> {
    let config = AudioSpecificConfig::read(asc).ok()?;

    if !may_have_implicit_sbr(&config) {
        return None;
    }

    let ext = detect(asc, config.sample_rate, blocks.iter().copied());

    if !ext.sbr {
        return None;
    }

    with_explicit_sbr(asc, config.sample_rate.saturating_mul(2), ext.ps)
}

/// A MSB-first bit writer.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    n_bits: usize,
}

impl BitWriter {
    fn put(&mut self, value: u32, n: usize) {
        for i in (0..n).rev() {
            if self.n_bits % 8 == 0 {
                self.bytes.push(0);
            }

            *self.bytes.last_mut().expect("a byte was pushed") |=
                (((value >> i) & 1) as u8) << (7 - self.n_bits % 8);

            self.n_bits += 1;
        }
    }

    /// Write a `program_config_element()` with the elements of `pce`. The element instance tags
    /// are numbered in the order of the elements of each kind, there are no mixdown coefficients
    /// and no comment, and the byte alignment is relative to the first bit written.
    fn put_program_config_element(&mut self, pce: &ProgramConfig) {
        self.put(0, 4); // element_instance_tag
        self.put(pce.object_type, 2);
        self.put(pce.sampling_frequency_index, 4);
        self.put(pce.front_is_cpe.len() as u32, 4);
        self.put(pce.side_is_cpe.len() as u32, 4);
        self.put(pce.back_is_cpe.len() as u32, 4);
        self.put(pce.num_lfe, 2);
        self.put(pce.num_assoc_data, 3);
        self.put(pce.num_valid_cc, 4);
        self.put(0, 3); // No mono, stereo, or matrix mixdown.

        for is_cpe in [&pce.front_is_cpe, &pce.side_is_cpe, &pce.back_is_cpe] {
            for (tag, &is_cpe) in is_cpe.iter().enumerate() {
                self.put(u32::from(is_cpe), 1);
                self.put(tag as u32 & 0xf, 4);
            }
        }

        for tag in 0..pce.num_lfe {
            self.put(tag, 4);
        }

        for tag in 0..pce.num_assoc_data {
            self.put(tag, 4);
        }

        for tag in 0..pce.num_valid_cc {
            self.put(0, 1); // cc_element_is_ind_sw
            self.put(tag, 4);
        }

        // byte_alignment()
        self.n_bits = self.n_bits.next_multiple_of(8);

        self.put(0, 8); // comment_field_bytes
    }

    fn put_audio_object_type(&mut self, aot: u32) {
        if aot < 31 {
            self.put(aot, 5);
        }
        else {
            self.put(31, 5);
            self.put(aot - 32, 6);
        }
    }

    fn put_sampling_frequency(&mut self, rate: u32) {
        match (0..13u32).find(|&idx| {
            matches!(get_mpeg4_audio_sample_rate_by_index(idx), Mpeg4AudioSampleRate::SampleRate(r) if r == rate)
        }) {
            Some(idx) => self.put(idx, 4),
            None => {
                self.put(15, 4);
                self.put(rate, 24);
            }
        }
    }
}

/// Builds the plain audio specific config of an AAC-LC stream with a predefined channel
/// configuration from the fields of an ADTS header: the sample rate and `channel_configuration`.
pub(crate) fn plain_asc(sample_rate: u32, channel_config: u32) -> Box<[u8]> {
    build_asc(2, sample_rate, channel_config, None, None)
}

/// Builds an audio specific config for a stream of an AAC object type with a 1024 sample frame
/// (`audio_object_type`: 1 is AAC Main, 2 is AAC LC, ...).
///
/// If `channel_config` is 0, the channel layout is that of the program config `pce`. If `sbr` is
/// `Some((output_rate, ps))`, the config signals SBR (and parametric stereo if `ps` is true)
/// explicitly and hierarchically, ISO/IEC 14496-3 §1.6.2.1.
pub(crate) fn build_asc(
    audio_object_type: u32,
    sample_rate: u32,
    channel_config: u32,
    pce: Option<&ProgramConfig>,
    sbr: Option<(u32, bool)>,
) -> Box<[u8]> {
    let mut bw = BitWriter::default();

    match sbr {
        Some((output_rate, ps)) => {
            bw.put_audio_object_type(if ps { 29 } else { 5 });
            bw.put_sampling_frequency(sample_rate);
            bw.put(channel_config, 4);
            bw.put_sampling_frequency(output_rate);
            bw.put_audio_object_type(audio_object_type);
        }
        None => {
            bw.put_audio_object_type(audio_object_type);
            bw.put_sampling_frequency(sample_rate);
            bw.put(channel_config, 4);
        }
    }

    // GASpecificConfig: frameLengthFlag, dependsOnCoreCoder, extensionFlag.
    bw.put(0, 3);

    if channel_config == 0 {
        if let Some(pce) = pce {
            bw.put_program_config_element(pce);
        }
    }

    bw.bytes.into_boxed_slice()
}

/// Rewrites the audio specific config `asc` of an AAC-LC stream so that it explicitly signals
/// (hierarchically, ISO/IEC 14496-3 §1.6.2.1) SBR with the output sample rate `out_rate`, and
/// parametric stereo if `ps` is true.
///
/// Returns `None` if `asc` is not that of an AAC-LC stream, or has a channel layout that is a
/// program config element with fields that this does not preserve (the core coder delay).
pub(crate) fn with_explicit_sbr(asc: &[u8], out_rate: u32, ps: bool) -> Option<Box<[u8]>> {
    let mut bs = BitReaderLtr::new(asc);

    let aot = match bs.read_bits_leq32(5).ok()? {
        31 => bs.read_bits_leq32(6).ok()? + 32,
        aot => aot,
    };

    if aot != 2 {
        return None;
    }

    let sf_index = bs.read_bits_leq32(4).ok()?;
    let escaped_rate = if sf_index == 15 { Some(bs.read_bits_leq32(24).ok()?) } else { None };
    let channel_config = bs.read_bits_leq32(4).ok()?;

    // The byte alignment of a program config element depends on its position in the config, which
    // the hierarchical signalling moves: write the config again.
    if channel_config == 0 {
        // frameLengthFlag, dependsOnCoreCoder, and extensionFlag are all clear.
        if bs.read_bits_leq32(3).ok()? != 0 {
            return None;
        }

        let pce = ProgramConfig::read(&mut bs).ok()?;

        let core_rate = match escaped_rate {
            Some(rate) => rate,
            None => match get_mpeg4_audio_sample_rate_by_index(sf_index) {
                Mpeg4AudioSampleRate::SampleRate(rate) => rate,
                _ => return None,
            },
        };

        return Some(build_asc(2, core_rate, 0, Some(&pce), Some((out_rate, ps))));
    }

    let mut bw = BitWriter::default();

    // The hierarchical signalling: SBR (or PS) wrapping the object type of the core codec.
    bw.put_audio_object_type(if ps { 29 } else { 5 });
    bw.put(sf_index, 4);

    if let Some(rate) = escaped_rate {
        bw.put(rate, 24);
    }

    bw.put(channel_config, 4);
    bw.put_sampling_frequency(out_rate);
    bw.put_audio_object_type(2);

    // The rest of the audio specific config (GASpecificConfig and anything after it) is that of
    // the core codec.
    let total_bits = asc.len() * 8;
    let mut consumed = 5 + 4 + 4 + if escaped_rate.is_some() { 24 } else { 0 };

    while consumed < total_bits {
        let n = (total_bits - consumed).min(16);
        bw.put(bs.read_bits_leq32(n as u32).ok()?, n);
        consumed += n;
    }

    Some(bw.bytes.into_boxed_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_common::mpeg::audio::AudioObjectType;

    #[test]
    fn plain_asc_is_aac_lc() {
        // AAC-LC, 22.05 kHz, stereo.
        let asc = AudioSpecificConfig::read(&plain_asc(22_050, 2)).unwrap();
        assert_eq!(asc.object_type, AudioObjectType::Lc);
        assert_eq!(asc.sample_rate, 22_050);
        assert_eq!(asc.samples, 1024);
        assert_eq!(asc.channels.map(|c| c.count()), Some(2));
        assert!(!asc.sbr_present);
    }

    #[test]
    fn explicit_sbr_asc_signals_output_rate() {
        let plain = plain_asc(22_050, 2);
        let asc =
            AudioSpecificConfig::read(&with_explicit_sbr(&plain, 44_100, false).unwrap()).unwrap();

        assert_eq!(asc.object_type, AudioObjectType::Lc);
        assert_eq!(asc.sample_rate, 22_050);
        assert!(asc.sbr_present);
        assert!(!asc.ps_present);
        assert_eq!(asc.output_sample_rate(), 44_100);
        assert_eq!(asc.output_channels().map(|c| c.count()), Some(2));
    }

    #[test]
    fn explicit_sbr_asc_keeps_the_program_config() {
        // The byte alignment of the program config element moves with the hierarchical signalling,
        // which a 5.1 layout with a long program config makes sure to be observed.
        let pce = ProgramConfig {
            front_is_cpe: vec![false, true],
            back_is_cpe: vec![true],
            num_lfe: 1,
            ..stereo_pce()
        };

        for (rate, ps) in [(44_100, false), (48_000, false)] {
            let plain = build_asc(2, rate / 2, 0, Some(&pce), None);
            let asc =
                AudioSpecificConfig::read(&with_explicit_sbr(&plain, rate, ps).unwrap()).unwrap();

            assert!(asc.sbr_present);
            assert_eq!(asc.sample_rate, rate / 2);
            assert_eq!(asc.output_sample_rate(), rate);
            assert_eq!(asc.channels, Some(layouts_5p1()));
            assert_eq!(
                asc.channel_elements,
                AudioSpecificConfig::read(&plain).unwrap().channel_elements
            );
        }

        // A stereo layout with a program config.
        let plain = build_asc(2, 22_050, 0, Some(&stereo_pce()), None);
        let asc =
            AudioSpecificConfig::read(&with_explicit_sbr(&plain, 44_100, false).unwrap()).unwrap();
        assert_eq!(asc.channels, Some(symphonia_core::audio::layouts::CHANNEL_LAYOUT_STEREO));
        assert_eq!(asc.output_sample_rate(), 44_100);
    }

    #[test]
    fn explicit_ps_asc_signals_stereo_output() {
        let plain = plain_asc(24_000, 1);
        let asc =
            AudioSpecificConfig::read(&with_explicit_sbr(&plain, 48_000, true).unwrap()).unwrap();

        assert!(asc.sbr_present);
        assert!(asc.ps_present);
        assert_eq!(asc.sample_rate, 24_000);
        assert_eq!(asc.output_sample_rate(), 48_000);
        assert_eq!(asc.output_channels().map(|c| c.count()), Some(2));
    }

    #[test]
    fn explicit_sbr_asc_with_escaped_rates() {
        // A core rate without an index is escape coded, as is the doubled rate.
        let mut bw = BitWriter::default();
        bw.put_audio_object_type(2);
        bw.put_sampling_frequency(14_700);
        bw.put(1, 4);
        bw.put(0, 3);

        let asc = AudioSpecificConfig::read(&with_explicit_sbr(&bw.bytes, 29_400, false).unwrap())
            .unwrap();

        assert_eq!(asc.sample_rate, 14_700);
        assert_eq!(asc.output_sample_rate(), 29_400);
    }

    #[test]
    fn explicit_sbr_asc_requires_aac_lc() {
        let mut bw = BitWriter::default();
        bw.put_audio_object_type(1);
        bw.put_sampling_frequency(22_050);
        bw.put(2, 4);
        bw.put(0, 3);

        assert!(with_explicit_sbr(&bw.bytes, 44_100, false).is_none());
    }

    fn stereo_pce() -> ProgramConfig {
        ProgramConfig {
            object_type: 1,
            sampling_frequency_index: 4,
            front_is_cpe: vec![true],
            side_is_cpe: vec![],
            back_is_cpe: vec![],
            num_lfe: 0,
            num_assoc_data: 0,
            num_valid_cc: 0,
        }
    }

    #[test]
    fn asc_with_program_config_has_its_layout() {
        let pce = ProgramConfig {
            front_is_cpe: vec![false, true],
            back_is_cpe: vec![true],
            num_lfe: 1,
            ..stereo_pce()
        };

        let asc = AudioSpecificConfig::read(&build_asc(2, 44_100, 0, Some(&pce), None)).unwrap();

        assert_eq!(asc.object_type, AudioObjectType::Lc);
        assert_eq!(asc.sample_rate, 44_100);
        assert_eq!(asc.channels, Some(layouts_5p1()));
        assert!(!asc.sbr_present);

        // With SBR, the program config follows the hierarchical signalling and is still aligned.
        let asc =
            AudioSpecificConfig::read(&build_asc(2, 22_050, 0, Some(&pce), Some((44_100, false))))
                .unwrap();

        assert_eq!(asc.channels, Some(layouts_5p1()));
        assert_eq!(asc.output_sample_rate(), 44_100);
        assert_eq!(asc.channel_elements.map(|e| e.len()), Some(4));
    }

    fn layouts_5p1() -> symphonia_core::audio::Channels {
        symphonia_core::audio::layouts::CHANNEL_LAYOUT_AAC_5P1
    }

    #[test]
    fn asc_with_stereo_program_config() {
        let asc =
            AudioSpecificConfig::read(&build_asc(2, 44_100, 0, Some(&stereo_pce()), None)).unwrap();

        assert_eq!(asc.channels, Some(symphonia_core::audio::layouts::CHANNEL_LAYOUT_STEREO));
    }
}
