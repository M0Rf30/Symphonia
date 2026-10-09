// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests against externally generated sample files.
//!
//! The samples are located using the `SYMPHONIA_TAG_SAMPLES` environment variable, which should
//! point to a directory containing the sample files. Each test is skipped if the sample is
//! absent.

#![cfg(feature = "id3v2")]

use std::fs::File;
use std::path::PathBuf;

use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::{MetadataOptions, MetadataReader, StandardTag, Tag};
use symphonia_metadata::id3v2::{Id3v2Reader, read_trailing_id3v2};

fn open(name: &str) -> Option<(MediaSourceStream<'static>, u64)> {
    let dir = PathBuf::from(std::env::var_os("SYMPHONIA_TAG_SAMPLES")?);
    let file = File::open(dir.join(name)).ok()?;
    let len = file.metadata().ok()?.len();
    Some((MediaSourceStream::new(Box::new(file), Default::default()), len))
}

fn read_leading(name: &str) -> Option<Vec<Tag>> {
    let (mss, _) = open(name)?;
    let mut reader = Id3v2Reader::try_new(mss, MetadataOptions::default()).unwrap();
    Some(reader.read_all().unwrap().revision.media.tags)
}

fn values(tags: &[Tag], f: impl Fn(&StandardTag) -> Option<&str>) -> Vec<String> {
    tags.iter().filter_map(|t| t.std.as_ref().and_then(&f)).map(str::to_owned).collect()
}

#[test]
fn compressed_frames_are_decoded() {
    for name in ["id3v2_3_compressed_frame.mp3", "id3v2_4_compressed_frame.mp3"] {
        let Some(tags) = read_leading(name)
        else {
            continue;
        };

        let titles = values(&tags, |s| match s {
            StandardTag::TrackTitle(v) => Some(v.as_str()),
            _ => None,
        });

        assert_eq!(titles, ["Tést Títle ✓ 日本語"], "{name}");
    }
}

#[test]
fn multi_valued_and_legacy_genre_frames_are_mapped() {
    if let Some(tags) = read_leading("id3v2_4_multivalue.mp3") {
        let genres = values(&tags, |s| match s {
            StandardTag::Genre(v) => Some(v.as_str()),
            _ => None,
        });

        assert_eq!(genres, ["Electronic", "Ambient"]);
    }

    if let Some(tags) = read_leading("id3v2_4_tcon_legacy.mp3") {
        let genres = values(&tags, |s| match s {
            StandardTag::Genre(v) => Some(v.as_str()),
            _ => None,
        });

        assert!(!genres.is_empty());
        assert!(genres.iter().all(|g| g == "Pop" || g == "Rock"), "{genres:?}");
    }
}

#[test]
fn musicbrainz_recording_id_is_mapped() {
    let Some(tags) = read_leading("id3v2_4_txxx_rg_mb.mp3")
    else {
        return;
    };

    let ids = values(&tags, |s| match s {
        StandardTag::MusicBrainzRecordingId(v) => Some(v.as_str()),
        _ => None,
    });

    assert_eq!(ids, ["8622e4d1-bc90-4532-b8df-35f6bbb6731c"]);
}

#[test]
fn trailing_tag_is_found() {
    let Some((mut mss, len)) = open("id3v2_4_at_end_footer.mp3")
    else {
        return;
    };

    let tag = read_trailing_id3v2(&mut mss, len).unwrap().expect("expected a trailing tag");

    assert_eq!(tag.end, len);
    assert!(tag.start > 0);

    let titles = values(&tag.metadata.revision.media.tags, |s| match s {
        StandardTag::TrackTitle(v) => Some(v.as_str()),
        _ => None,
    });

    assert_eq!(titles, ["Tést Títle ✓ 日本語"]);
}
