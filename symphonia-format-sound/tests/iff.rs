#![allow(clippy::unwrap_used)]
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the IFF reader. FFmpeg has no IFF muxer, so the fixtures in `tests/data` were
//! written by hand (random sound data), and their samples are compared with FFmpeg's `iff`
//! demuxer and decoders when `ffmpeg` is on the `PATH`.

mod common;

use std::path::PathBuf;

use symphonia_core::codecs::audio::AudioCodecId;
use symphonia_core::codecs::audio::well_known::*;
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::MetadataOptions;
use symphonia_format_sound::IffReader;

use common::{audio_params, check_seeks, decode_any_s16, ffmpeg_with, mss};

fn path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

fn open(name: &str) -> Box<dyn FormatReader + 'static> {
    let data = std::fs::read(path(name)).unwrap();
    Box::new(IffReader::try_new(mss(data), FormatOptions::default()).unwrap())
}

/// (file, codec, rate, channels)
const FILES: &[(&str, AudioCodecId, u32, usize)] = &[
    ("s8_mono_8k.8svx", CODEC_ID_PCM_S8_PLANAR, 8000, 1),
    ("s8_stereo_11k.8svx", CODEC_ID_PCM_S8_PLANAR, 11025, 2),
    ("fib_mono_8k.8svx", CODEC_ID_DPCM_8SVX_FIB, 8000, 1),
    ("fib_stereo_8k.8svx", CODEC_ID_DPCM_8SVX_FIB, 8000, 2),
    ("exp_mono_8k.8svx", CODEC_ID_DPCM_8SVX_EXP, 8000, 1),
    ("s16_mono_16k.16sv", CODEC_ID_PCM_S16BE_PLANAR, 16000, 1),
    ("s16_stereo_16k.16sv", CODEC_ID_PCM_S16BE_PLANAR, 16000, 2),
];

#[test]
fn parameters() {
    for &(name, codec, rate, channels) in FILES {
        let reader = open(name);
        let params = audio_params(&reader.tracks()[0]);

        assert_eq!(params.codec, codec, "{name}");
        assert_eq!(params.sample_rate, Some(rate), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels), "{name}");
    }
}

#[test]
fn matches_ffmpeg() {
    for &(name, ..) in FILES {
        let Some(expected) = ffmpeg_with(&[], &path(name), "s16le")
        else {
            return;
        };

        let ours = decode_any_s16(open(name).as_mut());

        assert_eq!(ours.len() * 2, expected.len(), "{name}");
        assert!(ours.iter().flat_map(|s| s.to_le_bytes()).eq(expected.iter().copied()), "{name}");
    }
}

#[test]
fn seeking() {
    for &(name, _, _, channels) in FILES {
        let frames = open(name).tracks()[0].num_frames.unwrap();
        check_seeks(|| open(name), channels, &[0, 1, 37, frames / 2, frames - 1]);
    }
}

#[test]
fn is_probed() {
    let mut probe = Probe::new();
    probe.register_format::<IffReader<'_>>();

    let data = std::fs::read(path("s8_stereo_11k.8svx")).unwrap();
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());

    let reader = probe
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .unwrap();

    assert_eq!(reader.format_info().short_name, "iff");
}

#[test]
fn rejects_bad_form() {
    let mut data = std::fs::read(path("s8_mono_8k.8svx")).unwrap();
    data[8..12].copy_from_slice(b"XXXX");

    assert!(IffReader::try_new(mss(data), FormatOptions::default()).is_err());
}
