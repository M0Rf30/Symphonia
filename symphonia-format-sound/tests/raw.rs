#![allow(clippy::unwrap_used)]
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the raw PCM reader. The fixtures in `tests/data` were made with FFmpeg's raw PCM
//! muxers, and their samples are compared with FFmpeg's raw demuxers when `ffmpeg` is on the
//! `PATH`.

mod common;

use std::path::PathBuf;

use symphonia_core::codecs::audio::well_known::*;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_format_sound::{RawPcmParams, RawPcmReader, RawPcmSampleFormat};

use common::{audio_params, check_seeks, decode_any_f32, decode_any_s16, ffmpeg_with, mss};

fn path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

fn open(
    name: &str,
    sample_format: RawPcmSampleFormat,
    rate: u32,
    channels: u16,
) -> Box<dyn FormatReader + 'static> {
    let data = std::fs::read(path(name)).unwrap();
    let params = RawPcmParams { sample_format, sample_rate: rate, channels };
    Box::new(RawPcmReader::try_new(mss(data), params, FormatOptions::default()).unwrap())
}

#[test]
fn s16le_matches_ffmpeg() {
    let name = "raw_s16le_mono_22k.raw";
    let reader = open(name, RawPcmSampleFormat::S16Le, 22050, 1);
    let track = &reader.tracks()[0];

    assert_eq!(audio_params(track).codec, CODEC_ID_PCM_S16LE);
    assert_eq!(track.num_frames, Some(1103));

    let Some(expected) =
        ffmpeg_with(&["-f", "s16le", "-ar", "22050", "-ac", "1"], &path(name), "s16le")
    else {
        return;
    };

    let ours = decode_any_s16(open(name, RawPcmSampleFormat::S16Le, 22050, 1).as_mut());
    assert!(ours.iter().flat_map(|s| s.to_le_bytes()).eq(expected.iter().copied()));
}

#[test]
fn f32be_matches_ffmpeg() {
    let name = "raw_f32be_stereo_22k.raw";
    let reader = open(name, RawPcmSampleFormat::F32Be, 22050, 2);

    assert_eq!(audio_params(&reader.tracks()[0]).codec, CODEC_ID_PCM_F32BE);
    assert_eq!(reader.tracks()[0].num_frames, Some(1103));

    let Some(expected) =
        ffmpeg_with(&["-f", "f32be", "-ar", "22050", "-ac", "2"], &path(name), "f32le")
    else {
        return;
    };

    let ours = decode_any_f32(open(name, RawPcmSampleFormat::F32Be, 22050, 2).as_mut());
    assert!(ours.iter().flat_map(|s| s.to_le_bytes()).eq(expected.iter().copied()));
}

#[test]
fn alaw_matches_ffmpeg() {
    let name = "raw_alaw_8k.raw";
    let Some(expected) =
        ffmpeg_with(&["-f", "alaw", "-ar", "8000", "-ac", "1"], &path(name), "s16le")
    else {
        return;
    };

    let ours = decode_any_s16(open(name, RawPcmSampleFormat::ALaw, 8000, 1).as_mut());
    assert!(ours.iter().flat_map(|s| s.to_le_bytes()).eq(expected.iter().copied()));
}

#[test]
fn seeking() {
    let frames = check_seeks(
        || open("raw_f32be_stereo_22k.raw", RawPcmSampleFormat::F32Be, 22050, 2),
        2,
        &[0, 1, 500, 1102],
    );
    assert_eq!(frames, 1103);

    check_seeks(
        || open("raw_s16le_mono_22k.raw", RawPcmSampleFormat::S16Le, 22050, 1),
        1,
        &[0, 7, 1000],
    );
}

#[test]
fn rejects_invalid_parameters() {
    let data = std::fs::read(path("raw_alaw_8k.raw")).unwrap();

    for (rate, channels) in [(0, 1), (8000, 0)] {
        let params =
            RawPcmParams { sample_format: RawPcmSampleFormat::ALaw, sample_rate: rate, channels };
        assert!(
            RawPcmReader::try_new(mss(data.clone()), params, FormatOptions::default()).is_err()
        );
    }
}
