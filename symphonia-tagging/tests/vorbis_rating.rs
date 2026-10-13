#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Round-trip tests of ratings in Vorbis comments (FLAC and Ogg Vorbis): the `rating_email` and
//! `vorbis_rating_scale` options of the writer, and the formats the reader accepts.

use std::fs;
use std::path::{Path, PathBuf};

use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, RawTag, StandardTag, Tag};
use symphonia_tagging::{VorbisRatingScale, WriteOptions, write_file};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

const FILES: [&str; 2] = ["vorbis-plain.flac", "vorbis-plain.ogg"];

/// Copies a fixture into a fresh directory and returns its path.
fn fixture(test: &str, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("symphonia-tagging-{}-{test}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let dst = dir.join(name);
    fs::copy(Path::new(FIXTURES).join(name), &dst).unwrap();
    dst
}

fn rating(ppm: u32) -> Tag {
    Tag::new_std(RawTag::new("RATING", (ppm / 10_000).to_string()), StandardTag::Rating(ppm))
}

/// The ratings (raw key, PPM) read through the default probe.
fn read_ratings(path: &Path) -> Vec<(String, u32)> {
    let file = fs::File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut format = symphonia::default::get_probe()
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .unwrap_or_else(|e| panic!("{} does not probe: {e}", path.display()));

    let mut metadata = format.metadata();
    let mut ratings = Vec::new();

    loop {
        if let Some(revision) = metadata.current() {
            for tag in &revision.media.tags {
                if let Some(StandardTag::Rating(ppm)) = tag.std {
                    ratings.push((tag.raw.key.clone(), ppm));
                }
            }
        }

        if metadata.pop().is_none() {
            break;
        }
    }

    ratings
}

/// Whether the file holds the comment `comment` (length prefixed, as in a comment header).
fn has_comment(path: &Path, comment: &str) -> bool {
    let bytes = fs::read(path).unwrap();

    let mut needle = (comment.len() as u32).to_le_bytes().to_vec();
    needle.extend_from_slice(comment.as_bytes());

    bytes.windows(needle.len()).any(|w| w == needle.as_slice())
}

/// Writes `tags` with `opts` to a copy of every fixture, and calls `check` with its path.
fn for_each_file(test: &str, tags: &[Tag], opts: &WriteOptions, check: impl Fn(&Path)) {
    for name in FILES {
        let path = fixture(test, name);
        write_file(&path, tags, &[], opts).unwrap();
        check(&path);
    }
}

fn opts(email: Option<&str>, scale: VorbisRatingScale) -> WriteOptions {
    WriteOptions {
        rating_email: email.map(str::to_string),
        vorbis_rating_scale: scale,
        ..Default::default()
    }
}

#[test]
fn percent_without_email_is_the_default() {
    for_each_file("rating_default", &[rating(800_000)], &WriteOptions::default(), |path| {
        assert!(has_comment(path, "RATING=80"));
        assert_eq!(read_ratings(path), vec![("RATING".to_string(), 800_000)]);
    });
}

#[test]
fn fraction_without_email() {
    let opts = opts(None, VorbisRatingScale::Fraction);

    for (ppm, text) in [(800_000, "0.8"), (1_000_000, "1.0"), (0, "0.0"), (833_300, "0.8333")] {
        for_each_file("rating_fraction", &[rating(ppm)], &opts, |path| {
            assert!(has_comment(path, &format!("RATING={text}")), "{text}");
            assert_eq!(read_ratings(path), vec![("RATING".to_string(), ppm)], "{text}");
        });
    }
}

#[test]
fn percent_with_email() {
    let opts = opts(Some("me@example.com"), VorbisRatingScale::Percent);

    for_each_file("rating_email_percent", &[rating(800_000)], &opts, |path| {
        assert!(has_comment(path, "RATING:me@example.com=80"));
        assert!(!has_comment(path, "RATING=80"));
        assert_eq!(read_ratings(path), vec![("RATING:me@example.com".to_string(), 800_000)]);
    });
}

#[test]
fn fraction_with_email() {
    let opts = opts(Some("me@example.com"), VorbisRatingScale::Fraction);

    for_each_file("rating_email_fraction", &[rating(833_300)], &opts, |path| {
        assert!(has_comment(path, "RATING:me@example.com=0.8333"));
        assert_eq!(read_ratings(path), vec![("RATING:me@example.com".to_string(), 833_300)]);
    });
}

#[test]
fn ratings_of_other_users_keep_their_key() {
    let other =
        Tag::new_std(RawTag::new("RATING:other@example.org", "50"), StandardTag::Rating(500_000));

    for (scale, text) in [(VorbisRatingScale::Percent, "50"), (VorbisRatingScale::Fraction, "0.5")]
    {
        let opts = opts(Some("me@example.com"), scale);

        for_each_file("rating_other", &[other.clone(), rating(900_000)], &opts, |path| {
            assert!(has_comment(path, &format!("RATING:other@example.org={text}")));
            assert!(!has_comment(path, &format!("RATING:me@example.com={text}")));

            let mut ratings = read_ratings(path);
            ratings.sort();

            assert_eq!(
                ratings,
                vec![
                    ("RATING:me@example.com".to_string(), 900_000),
                    ("RATING:other@example.org".to_string(), 500_000),
                ]
            );
        });
    }
}

#[test]
fn the_reader_accepts_fractions_percentages_and_keyed_ratings() {
    // Free-form tags are written as they are, so this tests the reader.
    let tags = [
        Tag::new(RawTag::new("RATING", "0.8333")),
        Tag::new(RawTag::new("rating:quodlibet@example.org", "0.25")),
        Tag::new(RawTag::new("RATING:other", "80")),
    ];

    for_each_file("rating_reader", &tags, &WriteOptions::default(), |path| {
        let mut ratings: Vec<(String, u32)> =
            read_ratings(path).into_iter().map(|(key, ppm)| (key.to_uppercase(), ppm)).collect();
        ratings.sort();

        assert_eq!(
            ratings,
            vec![
                ("RATING".to_string(), 833_300),
                ("RATING:OTHER".to_string(), 800_000),
                ("RATING:QUODLIBET@EXAMPLE.ORG".to_string(), 250_000),
            ]
        );
    });
}

#[test]
fn the_reader_rejects_out_of_range_ratings() {
    let tags = [Tag::new(RawTag::new("RATING", "1.5")), Tag::new(RawTag::new("RATING:x", "101"))];

    for_each_file("rating_range", &tags, &WriteOptions::default(), |path| {
        assert!(read_ratings(path).is_empty());
    });
}
