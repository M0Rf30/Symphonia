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
    AudioObjectType, AudioSpecificConfig, Mpeg4AudioSampleRate,
    get_mpeg4_audio_sample_rate_by_index,
};

use crate::AacDecoder;

/// The highest core sample rate of a stream that is assumed to use SBR if it is not signalled.
pub(crate) const MAX_IMPLICIT_SBR_CORE_RATE: u32 = 32_000;

/// The maximum number of `raw_data_block()`s examined to find SBR (and parametric stereo).
pub(crate) const MAX_PROBE_BLOCKS: usize = 8;

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

/// Returns true if the audio specific config describes a stream that may carry implicit SBR.
pub(crate) fn may_have_implicit_sbr(asc: &AudioSpecificConfig) -> bool {
    !asc.sbr_present
        && asc.object_type == AudioObjectType::Lc
        && asc.samples == SAMPLES_PER_FRAME as usize
        && asc.sample_rate <= MAX_IMPLICIT_SBR_CORE_RATE
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
    let mut bw = BitWriter::default();

    bw.put_audio_object_type(2);
    bw.put_sampling_frequency(sample_rate);
    bw.put(channel_config, 4);
    // GASpecificConfig: frameLengthFlag, dependsOnCoreCoder, extensionFlag.
    bw.put(0, 3);

    bw.bytes.into_boxed_slice()
}

/// Rewrites the audio specific config `asc` of an AAC-LC stream so that it explicitly signals
/// (hierarchically, ISO/IEC 14496-3 §1.6.2.1) SBR with the output sample rate `out_rate`, and
/// parametric stereo if `ps` is true.
///
/// Returns `None` if `asc` is not that of an AAC-LC stream.
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
}
