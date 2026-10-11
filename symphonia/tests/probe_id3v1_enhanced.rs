// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests that an ID3v1 tag, with or without an enhanced "TAG+" tag before it, is read through the
//! default probe.

#![cfg(all(feature = "mp3", feature = "id3v1"))]

use std::io::Cursor;

use symphonia::core::formats::probe::{Hint, Probe};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};

fn probe(data: Vec<u8>) -> Box<dyn FormatReader> {
    let mut probe = Probe::new();
    symphonia::default::register_enabled_formats(&mut probe);

    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());

    probe
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .expect("the stream is probed")
}

/// All of the standard tags selected by `f` in all of the metadata revisions of a format reader.
fn collect(
    format: &mut dyn FormatReader,
    f: impl Fn(&StandardTag) -> Option<String>,
) -> Vec<String> {
    let mut metadata = format.metadata();
    let mut values = Vec::new();

    loop {
        if let Some(revision) = metadata.current() {
            values
                .extend(revision.media.tags.iter().filter_map(|tag| tag.std.as_ref().and_then(&f)));
        }

        if metadata.pop().is_none() {
            break;
        }
    }

    values
}

fn titles(format: &mut dyn FormatReader) -> Vec<String> {
    collect(format, |s| match s {
        StandardTag::TrackTitle(v) => Some(v.to_string()),
        _ => None,
    })
}

fn genres(format: &mut dyn FormatReader) -> Vec<String> {
    collect(format, |s| match s {
        StandardTag::Genre(v) => Some(v.to_string()),
        _ => None,
    })
}

/// MPEG-1 layer 3, 128 kbps, 44.1 kHz frames (417 bytes, no padding).
fn mp3_frames() -> Vec<u8> {
    (0..32)
        .flat_map(|_| {
            let mut f = vec![0u8; 417];
            f[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
            f
        })
        .collect()
}

fn id3v1(title: &[u8]) -> Vec<u8> {
    let mut v1 = vec![0u8; 128];
    v1[..3].copy_from_slice(b"TAG");
    v1[3..3 + title.len()].copy_from_slice(title);
    // Genre: none.
    v1[127] = 255;
    v1
}

fn enhanced(title: &[u8], genre: &[u8]) -> Vec<u8> {
    let mut tag = vec![0u8; 227];
    tag[..4].copy_from_slice(b"TAG+");
    tag[4..4 + title.len()].copy_from_slice(title);
    tag[185..185 + genre.len()].copy_from_slice(genre);
    tag
}

#[test]
fn enhanced_tag_provides_the_full_title_and_genre() {
    let title = "A".repeat(45);
    let stream =
        [mp3_frames(), enhanced(title.as_bytes(), b"Shoegaze"), id3v1(&title.as_bytes()[..30])]
            .concat();

    let mut format = probe(stream);
    assert_eq!(format.format_info().short_name, "mp3");
    assert_eq!(titles(format.as_mut()), [title]);
    assert_eq!(genres(format.as_mut()), ["Shoegaze"]);
}

#[test]
fn id3v1_without_an_enhanced_tag_is_trimmed_and_decoded_as_windows_1252() {
    let mut title = b"It\x92s a \x93test\x94".to_vec();
    title.extend_from_slice(b"   ");
    let stream = [mp3_frames(), id3v1(&title)].concat();

    let mut format = probe(stream);
    assert_eq!(format.format_info().short_name, "mp3");
    assert_eq!(titles(format.as_mut()), ["It\u{2019}s a \u{201c}test\u{201d}"]);
}
