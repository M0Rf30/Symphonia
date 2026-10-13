// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers shared by the integration tests of the facade crate. Each test binary uses a subset.

// Every test binary compiles this module but uses only some of the helpers.
#![allow(dead_code)]

use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;

use symphonia::core::formats::probe::{Hint, Probe};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia_testutil::syncsafe;

/// Probes `data` with every enabled format.
pub fn probe(data: Vec<u8>) -> Box<dyn FormatReader> {
    let mut probe = Probe::new();
    symphonia::default::register_enabled_formats(&mut probe);

    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());

    probe
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .expect("the stream is probed")
}

/// All of the track titles in all of the metadata revisions of a format reader.
pub fn titles(format: &mut dyn FormatReader) -> Vec<String> {
    let mut metadata = format.metadata();
    let mut titles = Vec::new();

    loop {
        if let Some(revision) = metadata.current() {
            titles.extend(revision.media.tags.iter().filter_map(|tag| match &tag.std {
                Some(StandardTag::TrackTitle(title)) => Some(title.to_string()),
                _ => None,
            }));
        }

        if metadata.pop().is_none() {
            break;
        }
    }

    titles
}

/// An ID3v2 tag with a single TIT2 (title) frame. The tag has a footer if it is version 4.
pub fn id3v2_tag(major: u8, title: &str) -> Vec<u8> {
    let mut body = vec![3u8];
    body.extend_from_slice(title.as_bytes());

    let mut frame = b"TIT2".to_vec();
    if major == 4 {
        frame.extend_from_slice(&syncsafe(body.len()));
    }
    else {
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    }
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&body);

    let flags = if major == 4 { 0x10 } else { 0 };

    let mut tag = vec![b'I', b'D', b'3', major, 0, flags];
    tag.extend_from_slice(&syncsafe(frame.len()));
    tag.extend_from_slice(&frame);

    if major == 4 {
        tag.extend_from_slice(&[b'3', b'D', b'I', major, 0, flags]);
        tag.extend_from_slice(&syncsafe(frame.len()));
    }

    tag
}

/// MPEG-1 layer 3, 128 kbps, 44.1 kHz frames (417 bytes, no padding).
pub fn mp3_frames() -> Vec<u8> {
    (0..32)
        .flat_map(|_| {
            let mut f = vec![0u8; 417];
            f[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
            f
        })
        .collect()
}

/// Runs `ffmpeg` with `args`, and asserts that it succeeds.
pub fn run_ffmpeg(args: &[&str]) {
    let status = Command::new("ffmpeg").args(args).status().expect("failed to spawn ffmpeg");
    assert!(status.success(), "ffmpeg {args:?} failed");
}

/// A scratch directory named `name` in the temporary directory, created if needed.
pub fn fixtures_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(name);
    std::fs::create_dir_all(&dir).expect("the fixtures directory is created");
    dir
}
