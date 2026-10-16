#![allow(clippy::unwrap_used)]
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the VOC reader. The fixtures in `tests/data` were made with FFmpeg's `voc` muxer, and
//! their samples are compared with FFmpeg's decode when `ffmpeg` is on the `PATH`.

mod common;

use std::path::PathBuf;

use symphonia_core::codecs::audio::AudioCodecId;
use symphonia_core::codecs::audio::well_known::*;
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::MetadataOptions;
use symphonia_format_sound::VocReader;

use common::{audio_params, check_seeks, decode_any_s16, drain, ffmpeg_with, mss};

fn path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

fn open(name: &str) -> Box<dyn FormatReader + 'static> {
    let data = std::fs::read(path(name)).unwrap();
    Box::new(VocReader::try_new(mss(data), FormatOptions::default()).unwrap())
}

/// (file, codec, rate, channels, frames)
const FILES: &[(&str, AudioCodecId, u32, usize, u64)] = &[
    ("u8_mono_8k.voc", CODEC_ID_PCM_U8, 8000, 1, 800),
    ("s16_stereo_22k.voc", CODEC_ID_PCM_S16LE, 22050, 2, 1103),
    ("alaw_mono_8k.voc", CODEC_ID_PCM_ALAW, 8000, 1, 800),
    ("mulaw_mono_8k.voc", CODEC_ID_PCM_MULAW, 8000, 1, 800),
];

#[test]
fn parameters() {
    for &(name, codec, rate, channels, frames) in FILES {
        let reader = open(name);
        let track = &reader.tracks()[0];
        let params = audio_params(track);

        assert_eq!(params.codec, codec, "{name}");
        assert_eq!(params.sample_rate, Some(rate), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels), "{name}");
        assert_eq!(track.num_frames, Some(frames), "{name}");
    }
}

#[test]
fn matches_ffmpeg() {
    for &(name, _, _, _, _) in FILES {
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
    for &(name, _, _, channels, frames) in FILES {
        let total = check_seeks(|| open(name), channels, &[0, 1, 100, frames / 2, frames - 1]);
        assert_eq!(total as u64, frames, "{name}");
    }
}

#[test]
fn is_probed() {
    let mut probe = Probe::new();
    probe.register_format::<VocReader<'_>>();

    let data = std::fs::read(path("u8_mono_8k.voc")).unwrap();
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());

    let reader = probe
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .unwrap();

    assert_eq!(reader.format_info().short_name, "voc");
}

#[test]
fn truncated_file_is_read() {
    let mut data = std::fs::read(path("u8_mono_8k.voc")).unwrap();
    data.truncate(data.len() - 100);

    let mut reader = VocReader::try_new(mss(data), FormatOptions::default()).unwrap();
    let bytes: usize = drain(&mut reader).iter().map(|p| p.data.len()).sum();

    assert!(bytes > 0 && bytes < 800);
}

#[test]
fn rejects_bad_signature() {
    let mut data = std::fs::read(path("u8_mono_8k.voc")).unwrap();
    data[0] = b'X';

    assert!(VocReader::try_new(mss(data), FormatOptions::default()).is_err());
}
